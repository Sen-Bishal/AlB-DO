//! T-0048: CSS module imports must load in the real server and test-runner engines.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn albedo(home: &Path, cwd: &Path) -> Command {
    let binary = std::env::var_os("ALBEDO_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_albedo")));
    let mut command = Command::new(binary);
    command
        .current_dir(cwd)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env("ALBEDO_NO_TUI", "1");
    command
}

fn write(app: &Path, relative: &str, source: &str) {
    let path = app.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn app() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let output = albedo(home.path(), work.path())
        .args(["init", "app"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let app = work.path().join("app");
    write(
        &app,
        "src/routes/withcss/page.module.css",
        ".label { color: red; }\n",
    );
    write(
        &app,
        "src/routes/withcss/[id].tsx",
        r#"
import styles from './page.module.css';
export default function CssUnusedPage({ params }) {
  if (params.id === 'bad') throw new Error('expected-bad');
  return <p>ok-withcss-{params.id}</p>;
}
"#,
    );
    write(
        &app,
        "src/routes/withcss/error.tsx",
        r#"
export default function CssUnusedError({ error }) {
  return <p>css-boundary:{error.message}</p>;
}
"#,
    );
    write(
        &app,
        "src/routes/used/[id].tsx",
        r#"
import styles from '../withcss/page.module.css';
export default function CssUsedPage({ params }) {
  return <p className={styles.label} data-missing={styles.nope === undefined ? 'undefined' : 'present'}>used-css-{params.id}</p>;
}
"#,
    );
    write(
        &app,
        "src/routes/static.tsx",
        r#"
import styles from './withcss/page.module.css';
export default function CssStaticPage() {
  return <p className={styles.label}>static-css-control</p>;
}
"#,
    );
    write(
        &app,
        "src/routes/withoutcss/[id].tsx",
        r#"
export default function CssFreePage({ params }) { return <p>css-free-{params.id}</p>; }
"#,
    );
    write(
        &app,
        "src/routes/namespace/only.module.css",
        ".label { color: purple; }\n",
    );
    write(
        &app,
        "src/routes/namespace/[id].tsx",
        r#"
import * as styles from './only.module.css';
export default function CssNamespacePage({ params }) {
  return <p className={styles.default.label}>namespace-css-{params.id}</p>;
}
"#,
    );
    write(
        &app,
        "src/routes/sideeffect/only.module.css",
        ".sideOnly { color: orange; }\n",
    );
    write(
        &app,
        "src/routes/sideeffect/[id].tsx",
        r#"
import './only.module.css';
export default function CssSideEffectPage({ params }) { return <p>side-effect-css-{params.id}</p>; }
"#,
    );
    (home, work, app)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

async fn serve(home: &Path, app: &Path) -> (Server, String) {
    serve_mode(home, app, "serve").await
}

async fn serve_mode(home: &Path, app: &Path, mode: &str) -> (Server, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut server = Server(
        albedo(home, app)
            .args([mode, ".", "--port", &port.to_string()])
            .stdout(fs::File::create(app.join("css.stdout.log")).unwrap())
            .stderr(fs::File::create(app.join("css.stderr.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let base = format!("http://127.0.0.1:{port}");
    let client = client();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(response) = client.get(format!("{base}/_albedo/ready")).send().await {
            if response.status().is_success() {
                return (server, base);
            }
        }
        assert!(
            server.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "{}\n{}",
            fs::read_to_string(app.join("css.stdout.log")).unwrap_or_default(),
            fs::read_to_string(app.join("css.stderr.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn get(client: &reqwest::Client, base: &str, path: &str) -> String {
    let response = client.get(format!("{base}{path}")).send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status.as_u16(), 200, "{path}: {body}");
    body
}

fn scoped_label(body: &str) -> &str {
    scoped_class(body, "label")
}

fn scoped_class<'a>(body: &'a str, original: &str) -> &'a str {
    let prefix = format!("class=\"{original}_");
    let class = body
        .split(&prefix)
        .nth(1)
        .unwrap_or_else(|| panic!("missing scoped {original} class: {body}"));
    let suffix = class.split('"').next().unwrap();
    assert!(!suffix.is_empty(), "{body}");
    let selector = format!(".{original}_{suffix}");
    let styles = body
        .split("<style")
        .skip(1)
        .filter_map(|style| style.split_once('>'))
        .map(|(_, css)| css.split("</style>").next().unwrap())
        .collect::<String>();
    assert!(
        styles.contains(&selector),
        "markup class has no matching stylesheet selector: {body}"
    );
    suffix
}

#[tokio::test(flavor = "multi_thread")]
async fn unused_css_import_loads_without_error_boundary() {
    let (home, _work, app) = app();
    let (_server, base) = serve(home.path(), &app).await;
    let body = get(&client(), &base, "/withcss/ok").await;
    assert!(body.contains("ok-withcss-ok"), "{body}");
    assert!(
        !body.contains("css-boundary") && !body.contains("MODULE_MISSING"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_css_class_matches_stylesheet_and_static_control() {
    let (home, _work, app) = app();
    let (_server, base) = serve(home.path(), &app).await;
    let client = client();
    let dynamic = get(&client, &base, "/used/ok").await;
    assert!(dynamic.contains("used-css-ok"), "{dynamic}");
    assert!(dynamic.contains("data-missing=\"undefined\""), "{dynamic}");
    let suffix = scoped_label(&dynamic);
    let static_body = get(&client, &base, "/static").await;
    assert!(static_body.contains("static-css-control"), "{static_body}");
    assert_eq!(suffix, scoped_label(&static_body));
    let control = get(&client, &base, "/withoutcss/ok").await;
    assert!(control.contains("css-free-ok"), "{control}");
}

#[tokio::test(flavor = "multi_thread")]
async fn css_page_throws_then_recovers_on_same_server() {
    let (home, _work, app) = app();
    let (_server, base) = serve(home.path(), &app).await;
    let client = client();
    let bad = get(&client, &base, "/withcss/bad").await;
    assert!(
        bad.contains("css-boundary:") && bad.contains("expected-bad"),
        "{bad}"
    );
    assert!(!bad.contains("MODULE_MISSING"), "{bad}");
    let recovered = get(&client, &base, "/withcss/ok").await;
    assert!(
        recovered.contains("ok-withcss-ok") && !recovered.contains("css-boundary"),
        "{recovered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn namespace_and_side_effect_css_imports_load() {
    let (home, _work, app) = app();
    let (_server, base) = serve(home.path(), &app).await;
    let client = client();
    let namespace = get(&client, &base, "/namespace/ok").await;
    assert!(namespace.contains("namespace-css-ok"), "{namespace}");
    scoped_label(&namespace);
    let side_effect = get(&client, &base, "/sideeffect/ok").await;
    assert!(side_effect.contains("side-effect-css-ok"), "{side_effect}");
    let styles = side_effect
        .split("<style")
        .skip(1)
        .filter_map(|style| style.split_once('>'))
        .map(|(_, css)| css.split("</style>").next().unwrap())
        .collect::<String>();
    assert!(
        styles.contains(".sideOnly_") && styles.contains("color: orange"),
        "{side_effect}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_reload_provisions_added_css_classes_without_restart() {
    let (home, _work, app) = app();
    let (mut server, base) = serve_mode(home.path(), &app, "dev").await;
    let original_pid = server.0.id();
    let client = client();
    let first = get(&client, &base, "/used/ok").await;
    assert!(first.contains("used-css-ok"), "{first}");
    scoped_label(&first);

    write(
        &app,
        "src/routes/withcss/page.module.css",
        ".label { color: red; }\n.fresh { color: green; }\n",
    );
    let route = app.join("src/routes/used/[id].tsx");
    let source = fs::read_to_string(&route).unwrap();
    let edited = source.replace("className={styles.label}", "className={styles.fresh}");
    assert_ne!(edited, source);
    fs::write(&route, edited).unwrap();

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = String::new();
    loop {
        if let Ok(response) = client.get(format!("{base}/used/ok")).send().await {
            if response.status().is_success() {
                last = response.text().await.unwrap();
                if last.contains("used-css-ok") && last.contains("class=\"fresh_") {
                    scoped_class(&last, "fresh");
                    assert!(last.contains("color: green"), "{last}");
                    assert_eq!(server.0.id(), original_pid);
                    assert!(server.0.try_wait().unwrap().is_none());
                    return;
                }
            }
        }
        assert!(
            server.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "CSS reload did not serve the added class. Last response:\n{last}\n{}\n{}",
            fs::read_to_string(app.join("css.stdout.log")).unwrap_or_default(),
            fs::read_to_string(app.join("css.stderr.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[test]
fn albedo_test_component_imports_css_module() {
    let (home, _work, app) = app();
    for scaffold_test in [
        "src/routes/guestbook.test.tsx",
        "src/routes/sign-in.test.tsx",
        "src/components/Hero.test.tsx",
    ] {
        fs::remove_file(app.join(scaffold_test)).unwrap();
    }
    write(
        &app,
        "src/components/CssCounter.tsx",
        r#"
import { useState } from 'albedo';
import styles from '../routes/withcss/page.module.css';
export default function CssCounter() {
  const [count, setCount] = useState(7);
  return <button className={styles.label} onClick={() => setCount(count + 1)}>css-counter-{count}</button>;
}
"#,
    );
    write(
        &app,
        "src/components/CssCounter.test.tsx",
        r#"
import { test, expect, render } from 'albedo/test';
import CssCounter from './CssCounter';
test('CSS module component loads in the test engine', () => {
  const counter = render(<CssCounter />);
  expect(counter).toContain('css-counter-7');
  expect(counter.html).toContain('class="label_');
});
"#,
    );
    let output = albedo(home.path(), &app).arg("test").output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
}
