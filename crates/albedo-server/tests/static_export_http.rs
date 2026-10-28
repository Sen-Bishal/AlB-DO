//! `albedo ship --target static` — the exported folder, served by a plain static
//! host, against what `albedo serve` sends for the same URLs.
//!
//! ## Why this exists
//!
//! The static target used to print `static export ready` and point at
//! `.albedo/dist`, a folder of build manifests with **no HTML in it** — the
//! server assembles every page per request. Nothing checked the claim, because
//! nothing ever served the folder it named. So these tests do exactly that:
//! `init` → build → `ship --target static` with the real binary, then put the
//! output behind a server that knows nothing about ALB'DO and compare it, byte
//! for byte, with `albedo serve` on the same project.
//!
//! ## Every refusal is paired with a control
//!
//! An exporter that refused everything would pass every "is it refused?"
//! assertion, and one that exported nothing would pass a comparison of two 404s.
//! So the parity test asserts each page's own text is present on both sides,
//! the refusal of a form is shown on a route the same export had just written,
//! and that route's neighbours are shown not to be named.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Kills `albedo serve` when the test ends, pass or fail — a leaked server
/// holds the port and the binary open, and the next `cargo build` fails on it.
struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd
}

fn run(cmd: &mut Command, what: &str) -> Output {
    cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"))
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn run_ok(cmd: &mut Command, what: &str) -> Output {
    let out = run(cmd, what);
    assert!(
        out.status.success(),
        "{what} failed ({:?})\n{}",
        out.status.code(),
        text(&out)
    );
    out
}

/// `albedo init app` in a fresh temp dir; returns (home, work, app).
fn scaffold() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("temp home");
    let work = tempfile::tempdir().expect("temp workdir");
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");
    (home, work, app)
}

const CONFIG: &str = r#"export default {
  contract_version: 1,
  root: "src",
  entry: "routes/index.tsx",
  server: { host: "127.0.0.1", port: 3000 },
};
"#;

const INDEX_TSX: &str = r#"import Hero from "../components/Hero";

export default function Home() {
  return (
    <main>
      <Hero />
      <a href="/about">About this site</a>
      <a href="/docs/intro">Read the intro</a>
    </main>
  );
}
"#;

const ABOUT_TSX: &str = r#"export const metadata = { title: "About — static fixture" };

export default function About() {
  return (
    <main>
      <h1>About this site</h1>
      <p>Every page here is a file.</p>
      <img src="/img/dot.svg" alt="" />
    </main>
  );
}
"#;

const INTRO_TSX: &str = r#"const steps = ["build", "export", "upload"];

export default function Intro() {
  return (
    <article>
      <h1>Intro, nested one level down</h1>
      <ol>
        {steps.map((step) => (
          <li>{step}</li>
        ))}
      </ol>
    </article>
  );
}
"#;

/// Turn the scaffold into an app a static host can serve: no FORGE, no AUTH,
/// no island; three routes, one of them nested, and two `public/` files.
fn make_static(app: &Path) {
    for gone in [
        "src/routes/guestbook.tsx",
        "src/routes/sign-in.tsx",
        "src/components/Counter.tsx",
    ] {
        fs::remove_file(app.join(gone)).unwrap_or_else(|err| panic!("remove {gone}: {err}"));
    }
    fs::remove_dir_all(app.join("src/routes/room")).expect("remove src/routes/room");
    fs::write(app.join("albedo.config.ts"), CONFIG).unwrap();
    fs::write(app.join("src/routes/index.tsx"), INDEX_TSX).unwrap();
    fs::write(app.join("src/routes/about.tsx"), ABOUT_TSX).unwrap();
    fs::create_dir_all(app.join("src/routes/docs")).unwrap();
    fs::write(app.join("src/routes/docs/intro.tsx"), INTRO_TSX).unwrap();
    fs::create_dir_all(app.join("public/img")).unwrap();
    fs::write(app.join("public/hello.txt"), "hello asset\n").unwrap();
    fs::write(
        app.join("public/img/dot.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"/>\n",
    )
    .unwrap();
}

fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    probe.local_addr().expect("addr").port()
}

async fn serve(home: &Path, app: &Path) -> (Server, String) {
    let port = free_port();
    let child = albedo(home, app)
        .args(["serve", ".", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(fs::File::create(app.join("serve.stderr.log")).expect("stderr log"))
        .spawn()
        .expect("albedo serve starts");
    let server = Server { child };
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while reqwest::get(format!("{base}/_albedo/runtime.js")).await.is_err() {
        assert!(Instant::now() < deadline, "albedo serve never answered on {base}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (server, base)
}

/// A static host, and nothing more: the file at the path, else that
/// directory's `index.html`, else 404. It knows nothing about ALB'DO — which
/// is the point; it stands in for the CDN a user uploads the folder to.
async fn static_host(root: PathBuf) -> String {
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let root = root.clone();
        async move {
            let relative: PathBuf = uri.path().split('/').filter(|s| !s.is_empty()).collect();
            let file = root.join(relative);
            let file = if file.is_file() {
                file
            } else {
                file.join("index.html")
            };
            match fs::read(&file) {
                Ok(bytes) => (StatusCode::OK, bytes).into_response(),
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("static host binds");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn get(url: &str) -> (StatusCode, Vec<u8>) {
    let response = reqwest::get(url).await.unwrap_or_else(|err| panic!("GET {url}: {err}"));
    let status = StatusCode::from_u16(response.status().as_u16()).expect("status");
    (status, response.bytes().await.expect("body").to_vec())
}

const CSRF_OPEN: &str = "globalThis.__ALBEDO_CSRF__=\"";

/// The per-session token in a page, if it carries the CSRF bootstrap, and the
/// page with it blanked. A page of static markup carries none: it ships no
/// runtime, and the bootstrap is read by nothing else.
fn split_token(html: &str) -> (Option<String>, String) {
    let Some(at) = html.find(CSRF_OPEN) else {
        return (None, html.to_string());
    };
    let start = at + CSRF_OPEN.len();
    let end = start + html[start..].find('"').expect("token closes");
    let token = html[start..end].to_string();
    (Some(token), format!("{}{}", &html[..start], &html[end..]))
}

/// THE parity test. Every page and file the export wrote, fetched from a plain
/// static host, is byte-identical to what `albedo serve` answers for the same
/// URL — the one exception being the per-visitor CSRF token, which `serve`
/// mints fresh and the export writes empty.
#[tokio::test(flavor = "multi_thread")]
async fn every_exported_page_is_the_page_serve_sends() {
    let (home, _work, app) = scaffold();
    make_static(&app);

    let out = run_ok(
        albedo(home.path(), &app).args(["ship", ".", "--target", "static"]),
        "albedo ship --target static",
    );
    let printed = text(&out);
    assert!(printed.contains("3 page(s)"), "{printed}");
    assert!(!printed.contains("static export ready"), "the old claim is gone: {printed}");

    let exported = app.join(".albedo").join("static");
    for page in ["index.html", "about/index.html", "docs/intro/index.html"] {
        assert!(exported.join(page).is_file(), "{page} was not written");
    }

    let (_server, served) = serve(home.path(), &app).await;
    let hosted = static_host(exported.clone()).await;

    let pages = [
        ("/", "Evolution simplified"),
        ("/about", "Every page here is a file."),
        // The last of the `.map()`ed items: the list evaluated, not just the shell.
        ("/docs/intro", ">upload</li>"),
    ];
    for (route, own_text) in pages {
        let (served_status, served_body) = get(&format!("{served}{route}")).await;
        let (hosted_status, hosted_body) = get(&format!("{hosted}{route}")).await;
        assert_eq!(served_status, StatusCode::OK, "serve {route}");
        assert_eq!(hosted_status, StatusCode::OK, "static host {route}");

        let served_html = String::from_utf8(served_body).expect("utf-8");
        let hosted_html = String::from_utf8(hosted_body).expect("utf-8");
        // Controls: both are the real page, not two identical error bodies…
        assert!(served_html.contains(own_text), "serve {route} lacks {own_text:?}");
        assert!(hosted_html.contains(own_text), "export {route} lacks {own_text:?}");

        // Every page here is static markup, so it ships no client runtime — and
        // with it no session token and no live-lane switch, on either side.
        // 🔴 Until 2026-10-07 each carried all three, and this asserted they did.
        let (served_token, served_blanked) = split_token(&served_html);
        let (hosted_token, hosted_blanked) = split_token(&hosted_html);
        for (side, html, token) in [
            ("serve", &served_html, &served_token),
            ("export", &hosted_html, &hosted_token),
        ] {
            assert_eq!(token, &None, "{side} {route}: a static page carries no session token");
            assert!(
                !html.contains("/_albedo/runtime.js") && !html.contains("__ALBEDO_LIVE__"),
                "{side} {route}: a static page ships no client runtime"
            );
        }

        if served_blanked != hosted_blanked {
            let at = served_blanked
                .bytes()
                .zip(hosted_blanked.bytes())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| served_blanked.len().min(hosted_blanked.len()));
            let window = |s: &str| s[at.saturating_sub(80)..(at + 80).min(s.len())].to_string();
            panic!(
                "{route}: the export differs from serve at byte {at}\n serve: {}\nexport: {}",
                window(&served_blanked),
                window(&hosted_blanked)
            );
        }
    }

    for file in [
        "/hello.txt",
        "/img/dot.svg",
        "/_albedo/runtime.js",
        "/_albedo/bincode.js",
        "/_albedo/link-forms.js",
        "/_albedo/hydration.js",
    ] {
        let (served_status, served_body) = get(&format!("{served}{file}")).await;
        let (hosted_status, hosted_body) = get(&format!("{hosted}{file}")).await;
        assert_eq!(served_status, StatusCode::OK, "serve {file}");
        assert_eq!(hosted_status, StatusCode::OK, "static host {file}");
        assert!(!hosted_body.is_empty(), "{file} is empty");
        assert!(served_body == hosted_body, "{file}: the export differs from serve");
    }
}

/// The untouched scaffold needs a server for nearly everything it does, and
/// the refusal says so by name — every route and declaration, not the first —
/// and writes nothing.
#[test]
fn the_scaffold_is_refused_by_name_and_nothing_is_written() {
    let (home, _work, app) = scaffold();

    let out = run(
        albedo(home.path(), &app).args(["ship", ".", "--target", "static"]),
        "albedo ship --target static",
    );
    let printed = text(&out);
    assert!(!out.status.success(), "a refused export must exit non-zero:\n{printed}");
    for expected in [
        "static export refused",
        "forge: declares collection(s) `guestbook`, `messages`",
        "auth: declares provider(s) `password`",
        "/guestbook: declares action(s) `sign_guestbook`",
        "/room/[id]: a dynamic segment",
        "/: needs the server's live lane (islands: `Counter`",
        "nothing was written",
        "--target binary",
    ] {
        assert!(printed.contains(expected), "missing {expected:?} in:\n{printed}");
    }
    assert!(!printed.contains("static export ready"), "{printed}");
    assert!(
        !app.join(".albedo").join("static").exists(),
        "a refused export must not leave a directory behind"
    );
}

/// A form the manifest cannot see — a Tier-A route posting to an action
/// declared in another module, so the route itself declares none — is found in
/// the page `serve` renders, and refuses that route while the export already on
/// disk is left exactly as it was.
#[test]
fn a_form_only_the_rendered_page_shows_refuses_its_route_and_keeps_the_last_export() {
    let (home, _work, app) = scaffold();
    make_static(&app);
    run_ok(
        albedo(home.path(), &app).args(["ship", ".", "--target", "static"]),
        "the first export",
    );
    let about = app.join(".albedo/static/about/index.html");
    let before = fs::read(&about).expect("the first export wrote /about");

    fs::create_dir_all(app.join("src/lib")).unwrap();
    fs::write(
        app.join("src/lib/actions.ts"),
        "import { action } from \"albedo\";\n\n\
         export const subscribe = action(({ form }) => {\n  console.log(form.email);\n});\n",
    )
    .unwrap();
    fs::write(
        app.join("src/routes/about.tsx"),
        ABOUT_TSX.replace(
            "<img src=\"/img/dot.svg\" alt=\"\" />",
            "<form action=\"action:subscribe\" method=\"POST\"><input name=\"email\" />\
             <button type=\"submit\">Subscribe</button></form>",
        ),
    )
    .unwrap();

    let out = run(
        albedo(home.path(), &app).args(["ship", ".", "--target", "static"]),
        "the second export",
    );
    let printed = text(&out);
    assert!(!out.status.success(), "a page with a form must refuse:\n{printed}");
    assert!(
        printed.contains("/about: carries a form bound to an action"),
        "{printed}"
    );
    assert!(
        printed.contains("`/_albedo/action/subscribe`"),
        "the no-JS form endpoint is named too:\n{printed}"
    );
    // Control: the refusal is about /about, not a blanket one.
    assert!(printed.contains("1 part(s)"), "{printed}");
    assert!(!printed.contains("  - /docs/intro"), "{printed}");

    assert_eq!(
        fs::read(&about).expect("the first export is still there"),
        before,
        "a refused export must leave the previous one untouched"
    );
}
