//! T-0045 · Layouts, route error boundaries and islands that share a function
//! name across modules each keep their own behaviour. `export default function
//! Layout()` in every layout file is the ordinary habit; before the fix the
//! manifest builder re-resolved the layout chain by name and `/admin` silently
//! lost its own layout.
//!
//! The binary under test is `CARGO_BIN_EXE_albedo`, or `ALBEDO_TEST_BINARY`
//! when set (used to show the same scenario failing on an older build).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn binary() -> PathBuf {
    std::env::var_os("ALBEDO_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_albedo")))
}

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(binary());
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn review_app() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = albedo(home.path(), work.path())
        .args(["init", "app"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let app = work.path().join("app");
    (home, work, app)
}

async fn review_serve(home: &Path, app: &Path) -> (Server, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let server = Server {
        child: albedo(home, app)
            .args(["serve", ".", "--port", &port.to_string()])
            .stdout(fs::File::create(app.join("review.stdout.log")).unwrap())
            .stderr(fs::File::create(app.join("review.stderr.log")).unwrap())
            .spawn()
            .unwrap(),
    };
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(response) = client.get(format!("{base}/_albedo/ready")).send().await {
            if response.status().is_success() {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{}\n{}",
            fs::read_to_string(app.join("review.stderr.log")).unwrap_or_default(),
            fs::read_to_string(app.join("review.stdout.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (server, base)
}

async fn review_get(client: &reqwest::Client, base: &str, path: &str) -> String {
    let response = client.get(format!("{base}{path}")).send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status.as_u16(), 200, "GET {path}: {body}");
    body
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_metadata_and_deep_crlf_layouts_survive_concurrency_and_restart() {
    let (home, _work, app) = review_app();
    let routes = app.join("src/routes");
    write(&routes.join("layout.tsx"),
        "export default function Layout({ children }) {\r\n return <div data-review-root=\"yes\">{children}</div>;\r\n}\r\n");
    let mut nested = PathBuf::new();
    for level in 0..12 {
        nested.push(format!("level{level}"));
        write(&routes.join(&nested).join("layout.tsx"), &format!(
            "export default function Layout({{ children }}) {{\r\n return <section data-depth=\"{level}\">{{children}}</section>;\r\n}}\r\n"));
    }
    write(
        &routes.join(&nested).join("index.tsx"),
        "export default function Page() { return <p>deep-leaf</p>; }\r\n",
    );
    for label in ["a", "b"] {
        write(
            &routes.join(format!("meta{label}/[id].tsx")),
            &format!(
                r#"
export async function generateMetadata({{ params }}) {{ return {{ title: "meta-{label}-" + params.id }}; }}
export default function Page({{ params }}) {{
  if (params.id === "bad") throw new Error("failure-{label}");
  return <p>body-{label}-{{params.id}}</p>;
}}
"#
            ),
        );
        write(
            &routes.join(format!("meta{label}/error.tsx")),
            &format!("export default function Error() {{ return <p>boundary-{label}</p>; }}\r\n"),
        );
    }
    let deep_path = format!("/{}", nested.to_string_lossy().replace('\\', "/"));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    for _ in 0..2 {
        let (server, base) = review_serve(home.path(), &app).await;
        let deep = review_get(&client, &base, &deep_path).await;
        assert!(
            deep.contains("deep-leaf") && deep.contains("data-review-root=\"yes\""),
            "{deep}"
        );
        let mut previous = 0;
        for level in 0..12 {
            let marker = format!("data-depth=\"{level}\"");
            assert_eq!(deep.matches(&marker).count(), 1, "{deep}");
            let position = deep.find(&marker).unwrap();
            assert!(position > previous, "layout order at {level}: {deep}");
            previous = position;
        }
        let root = review_get(&client, &base, "/").await;
        assert!(
            root.contains("data-review-root=\"yes\"") && !root.contains("data-depth="),
            "{root}"
        );
        let mut requests = tokio::task::JoinSet::new();
        for index in 0..24 {
            let client = client.clone();
            let base = base.clone();
            requests.spawn(async move {
                let label = if index % 2 == 0 { "a" } else { "b" };
                let other = if label == "a" { "b" } else { "a" };
                let id = if index % 3 == 0 {
                    "bad".to_string()
                } else {
                    index.to_string()
                };
                let body = review_get(&client, &base, &format!("/meta{label}/{id}")).await;
                assert!(
                    body.contains(&format!("<title>meta-{label}-{id}</title>")),
                    "{body}"
                );
                if id == "bad" {
                    assert!(body.contains(&format!("boundary-{label}")), "{body}");
                } else {
                    assert!(body.contains(&format!("body-{label}-{id}")), "{body}");
                }
                assert!(
                    !body.contains(&format!("meta-{other}-"))
                        && !body.contains(&format!("boundary-{other}")),
                    "{body}"
                );
            });
        }
        while let Some(result) = requests.join_next().await {
            result.unwrap();
        }
        drop(server);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_hydrated_islands_deliver_their_own_client_code() {
    let (home, _work, app) = review_app();
    for label in ["a", "b"] {
        write(
            &app.join(format!("src/components/{label}/Counter.tsx")),
            &format!(
                r#"
import {{ useState, useEffect }} from "albedo";
export default function Counter() {{
  const [n, setN] = useState(0);
  useEffect(() => {{ document.title = "effect-{label}"; }}, []);
  return <button onClick={{() => setN(n + 1)}}>hydrated-{label} {{n}}</button>;
}}
"#
            ),
        );
        write(&app.join(format!("src/routes/hydrated{label}.tsx")), &format!(
            "import Counter from \"../components/{label}/Counter\";\nexport default function Page() {{ return <main><Counter /></main>; }}\n"));
    }
    let output = albedo(home.path(), &app)
        .args(["build", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(app.join(".albedo/dist/render-manifest.v2.json")).unwrap(),
    )
    .unwrap();
    let (_server, base) = review_serve(home.path(), &app).await;
    let client = reqwest::Client::new();
    let mut chunks = Vec::new();
    for label in ["a", "b"] {
        let route = format!("/hydrated{label}");
        let html = review_get(&client, &base, &route).await;
        assert!(html.contains(&format!("hydrated-{label}")), "{html}");
        let nodes = manifest["routes"][&route]["tier_c"].as_array().unwrap();
        let node = nodes
            .iter()
            .find(|n| n["component_id"] == "Counter")
            .unwrap();
        let chunk_path = node["bundle_path"].as_str().unwrap();
        // A3 sends client component code inline; bundle_path names a legacy
        // artifact that the production server no longer loads.
        let chunk = html.split("<script").skip(1).collect::<String>();
        assert!(
            chunk.contains(&format!("hydrated-{label}"))
                && chunk.contains(&format!("effect-{label}")),
            "{chunk_path}: {chunk}"
        );
        let other = if label == "a" { "b" } else { "a" };
        assert!(
            !chunk.contains(&format!("hydrated-{other}")),
            "{chunk_path}: {chunk}"
        );
        for script in html.split("<script").skip(1) {
            let opening = script.split('>').next().unwrap();
            if let Some(rest) = opening.split("src=\"").nth(1) {
                let src = rest.split('"').next().unwrap();
                review_get(&client, &base, src).await;
            }
        }
        chunks.push(chunk_path.to_string());
    }
    assert_ne!(chunks[0], chunks[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn same_named_islands_on_one_route_keep_separate_anchors() {
    let (home, _work, app) = review_app();
    for label in ["a", "b"] {
        write(
            &app.join(format!("src/components/{label}/Counter.tsx")),
            &format!(
                r#"
import {{ useState }} from "albedo";
export default function Counter() {{
  const [n, setN] = useState(0);
  return <button onClick={{() => setN(n + 1)}}>side-{label} {{n}}</button>;
}}
"#
            ),
        );
        let wrapper = if label == "a" { "PanelA" } else { "PanelB" };
        write(
            &app.join(format!("src/components/{label}/{wrapper}.tsx")),
            &format!("import Counter from './Counter';\nexport default function {wrapper}() {{ return <section><Counter /></section>; }}\n"),
        );
    }
    write(
        &app.join("src/routes/side.tsx"),
        r#"
import PanelA from "../components/a/PanelA";
import PanelB from "../components/b/PanelB";
export default function Page() { return <main><PanelA /><PanelB /></main>; }
"#,
    );
    let (_server, base) = review_serve(home.path(), &app).await;
    let body = review_get(&reqwest::Client::new(), &base, "/side").await;
    assert!(body.contains("side-a") && body.contains("side-b"), "{body}");
    assert!(!body.contains("Page did not render"), "{body}");
    let markup = body
        .split("<body>")
        .nth(1)
        .unwrap()
        .split("<script")
        .next()
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(app.join(".albedo/dist/render-manifest.v2.json")).unwrap(),
    )
    .unwrap();
    for node in manifest["routes"]["/side"]["tier_c"].as_array().unwrap() {
        let id = node["component_key"].as_u64().unwrap();
        assert_eq!(
            markup
                .matches(&format!("data-albedo-island=\"{id}\""))
                .count(),
            1,
            "{id} must anchor exactly one island: {markup}"
        );
    }
    assert!(
        markup.contains("side-a") && markup.contains("side-b"),
        "{markup}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_module_islands_keep_their_own_markup_and_seed() {
    let (home, _work, app) = review_app();
    for (module, label) in [
        ("src/components/Counter.tsx", "short-path"),
        ("src/nested/components/Counter.tsx", "long-path"),
    ] {
        write(&app.join(module), &format!(
            "import {{ useState }} from 'albedo';\r\nexport default function Counter({{ seed }}) {{ const [n, setN] = useState(seed); return <button onClick={{() => setN(n + 1)}}>{label} {{n}}</button>; }}\r\n"
        ));
    }
    write(&app.join("src/routes/overlap.tsx"),
        "import First from '../components/Counter';\r\nimport Second from '../nested/components/Counter';\r\nexport default function Page() { return <main><First seed={19} /><Second seed={43} /></main>; }\r\n");
    let (_server, base) = review_serve(home.path(), &app).await;
    let body = review_get(&reqwest::Client::new(), &base, "/overlap").await;
    let markup = body
        .split("<body>")
        .nth(1)
        .unwrap()
        .split("<script")
        .next()
        .unwrap();
    assert_eq!(markup.matches("short-path 19").count(), 1, "{markup}");
    assert_eq!(markup.matches("long-path 43").count(), 1, "{markup}");
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_layout_islands_receive_their_own_seed_props_over_http() {
    let (home, _work, app) = review_app();
    for label in ["a", "b"] {
        write(&app.join(format!("src/components/{label}/Counter.tsx")), &format!(
            "import {{ useState }} from 'albedo';\nexport default function Counter({{ seed }}) {{ const [n, setN] = useState(seed || -1); return <button onClick={{() => setN(n + 1)}}>layout-{label} {{n}}</button>; }}\n"
        ));
    }
    write(&app.join("src/routes/layout.tsx"),
        "import Counter from '../components/a/Counter';\nexport default function Layout({ children }) { return <div><Counter seed={11} />{children}</div>; }\n");
    write(&app.join("src/routes/admin/layout.tsx"),
        "import Counter from '../../components/b/Counter';\nexport default function Layout({ children }) { return <section><Counter seed={29} />{children}</section>; }\n");
    write(
        &app.join("src/routes/admin/index.tsx"),
        "export default function Page() { return <p>admin-leaf</p>; }\n",
    );
    let (_server, base) = review_serve(home.path(), &app).await;
    let body = review_get(&reqwest::Client::new(), &base, "/admin").await;
    let markup = body
        .split("<body>")
        .nth(1)
        .unwrap()
        .split("<script")
        .next()
        .unwrap();
    assert!(
        markup.contains("layout-a 11") && markup.contains("layout-b 29"),
        "{markup}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn same_named_layouts_boundaries_and_islands_keep_their_own_behaviour() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = albedo(home.path(), work.path())
        .args(["init", "app"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let app = work.path().join("app");
    let routes = app.join("src/routes");

    // Layouts: the scaffold's root layout renamed to `Layout`, plus a nested
    // `admin/layout.tsx` with the same name.
    let root_layout = routes.join("layout.tsx");
    let source = fs::read_to_string(&root_layout).unwrap();
    let renamed = source.replacen(
        "export default function RootLayout(",
        "export default function Layout(",
        1,
    );
    assert_ne!(
        renamed, source,
        "scaffold root layout is no longer `RootLayout`; update the test"
    );
    fs::write(&root_layout, renamed).unwrap();
    write(
        &routes.join("admin/layout.tsx"),
        "export default function Layout({ children }) {\n  return <div><p>admin-layout</p>{children}</div>;\n}\n",
    );
    write(
        &routes.join("admin/index.tsx"),
        "export default function Page() {\n  return <p>admin-page</p>;\n}\n",
    );

    // Error boundaries: two route folders, each with a throwing dynamic page
    // and its own `error.tsx`, both `export default function Error()`.
    for which in ["a", "b"] {
        write(
            &routes.join(format!("boom{which}/[id].tsx")),
            &format!(
                "export default function Page({{ params }}) {{\n  if (params.id !== \"ok\") throw new Error(\"boom-{which}\");\n  return <p>fine-{which}</p>;\n}}\n"
            ),
        );
        write(
            &routes.join(format!("boom{which}/error.tsx")),
            &format!("export default function Error() {{\n  return <p>error-{which}</p>;\n}}\n"),
        );
    }

    // Islands: two stateful components named `Counter` in different modules.
    for which in ["a", "b"] {
        write(
            &app.join(format!("src/components/{which}/Counter.tsx")),
            &format!(
                "import {{ useState }} from \"albedo\";\n\nexport default function Counter() {{\n  const [n, setN] = useState(0);\n  return <button onClick={{() => setN(n + 1)}}>counter-{which} {{n}}</button>;\n}}\n"
            ),
        );
        write(
            &routes.join(format!("island{which}.tsx")),
            &format!(
                "import Counter from \"../components/{which}/Counter\";\n\nexport default function Page() {{\n  return <section><p>island-page-{which}</p><Counter /></section>;\n}}\n"
            ),
        );
    }

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let stderr = app.join("serve.stderr.log");
    let stdout = app.join("serve.stdout.log");
    let _server = Server {
        child: albedo(home.path(), &app)
            .args(["serve", ".", "--port", &port.to_string()])
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    };
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while client
        .get(format!("{base}/_albedo/ready"))
        .send()
        .await
        .is_err()
    {
        assert!(
            Instant::now() < deadline,
            "serve never answered: {}\n{}",
            fs::read_to_string(&stderr).unwrap_or_default(),
            fs::read_to_string(&stdout).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let get = |path: &'static str| {
        let client = client.clone();
        let base = base.clone();
        async move {
            let response = client.get(format!("{base}{path}")).send().await.unwrap();
            (response.status().as_u16(), response.text().await.unwrap())
        }
    };

    let mut failures = Vec::new();
    let (_, admin) = get("/admin").await;
    if !admin.contains("admin-layout") {
        failures.push("/admin lost its own layout (admin/layout.tsx)");
    }
    if !admin.contains("admin-page") {
        failures.push("/admin lost its page");
    }
    let (_, home_page) = get("/").await;
    if home_page.contains("admin-layout") {
        failures.push("/ rendered the admin layout");
    }

    let (_, boom_a) = get("/booma/x").await;
    let (_, boom_b) = get("/boomb/x").await;
    if !boom_a.contains("error-a") || boom_a.contains("error-b") {
        failures.push("/booma/x did not render its own error boundary");
    }
    if !boom_b.contains("error-b") || boom_b.contains("error-a") {
        failures.push("/boomb/x did not render its own error boundary");
    }

    let (_, island_a) = get("/islanda").await;
    let (_, island_b) = get("/islandb").await;
    if !island_a.contains("counter-a") || island_a.contains("counter-b") {
        failures.push("/islanda did not render its own Counter");
    }
    if !island_b.contains("counter-b") || island_b.contains("counter-a") {
        failures.push("/islandb did not render its own Counter");
    }

    assert!(
        failures.is_empty(),
        "{failures:#?}\n--- server stdout ---\n{}",
        fs::read_to_string(&stdout).unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn static_counter_with_overlapping_path_keeps_its_own_markup_over_http() {
    let (home, _work, app) = review_app();
    write(&app.join("src/components/Counter.tsx"),
        "import { useState } from 'albedo';\r\nexport default function Counter({ seed }) { const [n, setN] = useState(seed); return <button onClick={() => setN(n + 1)}>interactive-counter {n}</button>; }\r\n");
    write(
        &app.join("src/nested/components/Counter.tsx"),
        "export default function Counter({ seed }) { return <p>static-counter {seed}</p>; }\r\n",
    );
    write(&app.join("src/routes/overlap-static.tsx"),
        "import Interactive from '../components/Counter';\r\nimport Static from '../nested/components/Counter';\r\nexport default function Page() { return <main><Interactive seed={11} /><Static seed={99} /></main>; }\r\n");
    let (_server, base) = review_serve(home.path(), &app).await;
    let body = review_get(&reqwest::Client::new(), &base, "/overlap-static").await;
    let markup = body
        .split("<body>")
        .nth(1)
        .unwrap()
        .split("<script")
        .next()
        .unwrap();
    assert_eq!(markup.matches("static-counter 99").count(), 1, "{markup}");
    assert_eq!(
        markup.matches("interactive-counter 11").count(),
        1,
        "{markup}"
    );
    assert_eq!(markup.matches("data-albedo-island=").count(), 1, "{markup}");
}
#[tokio::test(flavor = "multi_thread")]
async fn same_named_layouts_boundaries_and_css_keep_route_scope_after_restart() {
    let (home, _work, app) = review_app();
    let routes = app.join("src/routes");
    write(&routes.join("layout.tsx"),
        "export default function Layout({ children }) { return <div data-review-layer='root'>{children}</div>; }\r\n");
    write(&routes.join("branch/layout.tsx"),
        "export default function Layout({ children }) { return <section data-review-layer='branch'>{children}</section>; }\r\n");
    for (folder, label, css) in [
        ("root", "root", ".label { color: red; }"),
        (
            "branch/deep",
            "branch",
            ".label { color: blue; } body { display: none; }",
        ),
    ] {
        write(&routes.join(format!("{folder}/page.module.css")), css);
        write(&routes.join(format!("{folder}/index.tsx")), &format!(
            "import styles from './page.module.css';\r\nexport default function Page() {{ return <p className={{styles.label}}>{label}-page</p>; }}\r\n"
        ));
        write(&routes.join(format!("{folder}/[id].tsx")), &format!(
            "export default function Page({{ params }}) {{ if (params.id === 'bad') throw new Error('boom'); return <p>{label}-dynamic</p>; }}\r\n"
        ));
    }
    for (folder, label) in [("", "root"), ("branch/", "branch")] {
        write(
            &routes.join(format!("{folder}error.tsx")),
            &format!("export default function Error() {{ return <p>{label}-boundary</p>; }}\r\n"),
        );
    }
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let (server, base) = review_serve(home.path(), &app).await;
        for (path, label, other) in [
            ("/root", "root", "branch"),
            ("/branch/deep", "branch", "root"),
        ] {
            let body = review_get(&client, &base, path).await;
            assert!(body.contains(&format!("{label}-page")), "{body}");
            assert!(body.contains("data-review-layer=\"root\""), "{body}");
            assert_eq!(
                body.contains("data-review-layer=\"branch\""),
                label == "branch",
                "{body}"
            );
            if label == "root" {
                assert!(
                    body.contains("color: red") && !body.contains("display: none"),
                    "{body}"
                );
            } else {
                assert!(
                    body.contains("color: blue") && !body.contains("color: red"),
                    "{body}"
                );
            }
            let failure = review_get(&client, &base, &format!("{path}/bad")).await;
            assert!(failure.contains(&format!("{label}-boundary")), "{failure}");
            assert!(!failure.contains(&format!("{other}-boundary")), "{failure}");
        }
        drop(server);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sibling_layout_client_code_and_boundaries_stay_isolated_after_process_kill() {
    let (home, _work, app) = review_app();
    write(&app.join("src/routes/layout.tsx"),
        "export default function Layout({ children }) { return <div data-r4-root='yes'>{children}</div>; }\r\n");
    for (label, seed) in [("a", 17), ("b", 31)] {
        write(&app.join(format!("src/components/{label}/Counter.tsx")), &format!(
            "import {{ useState, useEffect }} from 'albedo';\r\nexport default function Counter({{ seed }}) {{ const [n, setN] = useState(seed); useEffect(() => {{ document.title = 'r4-effect-{label}'; }}, []); return <button onClick={{() => setN(n + 1)}}>r4-island-{label} {{n}}</button>; }}\r\n"));
        write(&app.join(format!("src/routes/{label}/layout.tsx")), &format!(
            "import Counter from '../../components/{label}/Counter';\r\nexport default function Layout({{ children }}) {{ return <section data-r4-layout='{label}'><Counter seed={{{seed}}} />{{children}}</section>; }}\r\n"));
        write(&app.join(format!("src/routes/{label}/[id].tsx")), &format!(
            "export default function Page({{ params }}) {{ if (params.id === 'bad') throw new Error('r4-boom-{label}'); return <p>r4-page-{label}-{{params.id}}</p>; }}\r\n"));
        write(
            &app.join(format!("src/routes/{label}/error.tsx")),
            &format!(
                "export default function Error() {{ return <p>r4-boundary-{label}</p>; }}\r\n"
            ),
        );
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    for _ in 0..2 {
        let (server, base) = review_serve(home.path(), &app).await;
        let mut requests = tokio::task::JoinSet::new();
        for index in 0..16 {
            let client = client.clone();
            let base = base.clone();
            requests.spawn(async move {
                let (label, other, seed) = if index % 2 == 0 {
                    ("a", "b", 17)
                } else {
                    ("b", "a", 31)
                };
                let id = if index % 3 == 0 {
                    "bad".to_string()
                } else {
                    index.to_string()
                };
                let body = review_get(&client, &base, &format!("/{label}/{id}")).await;
                assert!(body.contains("data-r4-root=\"yes\""), "{body}");
                assert!(
                    body.contains(&format!("data-r4-layout=\"{label}\"")),
                    "{body}"
                );
                assert!(
                    !body.contains(&format!("data-r4-layout=\"{other}\"")),
                    "{body}"
                );
                assert!(
                    body.contains(&format!("r4-island-{label} {seed}")),
                    "{body}"
                );
                if id == "bad" {
                    assert!(body.contains(&format!("r4-boundary-{label}")), "{body}");
                } else {
                    assert!(body.contains(&format!("r4-page-{label}-{id}")), "{body}");
                }
                assert!(!body.contains(&format!("r4-boundary-{other}")), "{body}");
                let scripts = body.split("<script").skip(1).collect::<String>();
                assert!(scripts.contains(&format!("r4-effect-{label}")), "{scripts}");
                assert!(
                    !scripts.contains(&format!("r4-effect-{other}")),
                    "{scripts}"
                );
                for script in body.split("<script").skip(1) {
                    let opening = script.split('>').next().unwrap();
                    if let Some(rest) = opening.split("src=\"").nth(1) {
                        review_get(&client, &base, rest.split('"').next().unwrap()).await;
                    }
                }
            });
        }
        while let Some(result) = requests.join_next().await {
            result.unwrap();
        }
        drop(server);
    }
}
