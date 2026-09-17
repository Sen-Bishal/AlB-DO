//! SHUTTER · the outbound budget over real HTTP, from `albedo init`.
//!
//! ## What this proves that the unit tests cannot
//!
//! `shutter`'s own tests pin `outbound_call` against a manual clock, and the
//! workflow driver's test pins that a refused call is never sent. Neither can see
//! whether a real action's `fetch()` reaches the admission hook at all — the
//! caller is only known to the dispatcher, and the hook is reached through a
//! task-local — or whether a declared source's `limit` survives config → boot →
//! limiter. A path that silently skips the scope admits everything, and would
//! look exactly like a generous limit.
//!
//! ## The shape of the proof
//!
//! One host, `limit: "4/m"`, reached through two doors by two different callers:
//!
//! 1. An **anonymous** visitor submits a form action whose body calls the host.
//!    The first four submits go out; the fifth is `429` and never arrives.
//! 2. A **signed-in** user — a different SHUTTER caller, who has made no calls of
//!    their own — hits a page whose **middleware** calls the same host, and is
//!    refused at once.
//!
//! (2) is the point of a per-host budget: no per-caller bucket would refuse a
//! caller with a clean record. Its control is (1) itself — the same host admitted
//! calls until the shared budget was spent.

use reqwest::{redirect::Policy, StatusCode};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
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

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd
}

fn run_ok(cmd: &mut Command, what: &str) {
    let out = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    assert!(
        out.status.success(),
        "{what} failed ({:?})\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

type Hits = Arc<Mutex<HashMap<String, usize>>>;

async fn upstream() -> (String, Hits) {
    let hits: Hits = Arc::new(Mutex::new(HashMap::new()));
    let counted = Arc::clone(&hits);
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let counted = Arc::clone(&counted);
        async move {
            *counted.lock().unwrap().entry(uri.path().to_string()).or_default() += 1;
            ([("content-type", "application/json")], "{}")
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), hits)
}

fn hits(hits: &Hits, path: &str) -> usize {
    hits.lock().unwrap().get(path).copied().unwrap_or(0)
}

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn cookie(response: &reqwest::Response, prefix: &str) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find(|v| v.starts_with(prefix))
        .map(str::to_string)
}

/// A page's form: the tab cookie, CSRF token and return path a browser would send.
async fn served_form(client: &reqwest::Client, url: &str) -> (String, String, String) {
    let page = client.get(url).send().await.expect("page");
    assert_eq!(page.status(), StatusCode::OK, "{url}");
    let tab = cookie(&page, "__Host-albedo-session=").expect("tab session");
    let html = page.text().await.unwrap();
    let csrf = hidden_field(&html, "_csrf").unwrap_or_else(|| panic!("no CSRF token:\n{html}"));
    let back = hidden_field(&html, "_albedo_return").unwrap_or_default();
    (tab, csrf, back)
}

#[tokio::test]
async fn one_host_budget_is_shared_across_callers_and_across_actions_and_middleware() {
    let (base, upstream_hits) = upstream().await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    run_ok(albedo(home.path(), work.path()).args(["init", "budget"]), "albedo init");
    let app = work.path().join("budget");

    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        config.replacen(
            "export default {",
            &format!(
                "export default {{\n  sources: {{ api: {{ base: \"{base}\", limit: \"4/m\", routes: {{ status: {{ path: \"/status\" }} }} }} }},"
            ),
            1,
        ),
    )
    .unwrap();
    fs::write(
        app.join("src/routes/ping.tsx"),
        format!(
            r#"import {{ action }} from "albedo";

export const ping = action(async ({{ form }}) => {{
  await fetch("{base}/from-action");
}});

export default function Ping() {{
  return (
    <form action="action:ping" method="POST">
      <input name="note" />
    </form>
  );
}}
"#
        ),
    )
    .unwrap();
    fs::write(
        app.join("src/middleware.ts"),
        format!(
            r#"import {{ next }} from "albedo/middleware";
export const config = {{ matcher: ["/guestbook"] }};
export default async function middleware() {{
  const res = await fetch("{base}/from-middleware");
  return next({{ headers: {{ "x-upstream": String(res.status) }} }});
}}
"#
        ),
    )
    .unwrap();
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let child = albedo(home.path(), &app)
        .args(["serve", ".", "--port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(fs::File::create(app.join("serve.stderr.log")).unwrap())
        .spawn()
        .expect("serve starts");
    let _server = Server { child };
    let origin = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while reqwest::get(format!("{origin}/_albedo/runtime.js")).await.is_err() {
        assert!(Instant::now() < deadline, "serve never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let client = reqwest::Client::builder().redirect(Policy::none()).build().unwrap();

    // ── 1 · an anonymous visitor's form action spends the host's budget ──
    let mut statuses = Vec::new();
    for attempt in 0..6 {
        let (tab, csrf, back) = served_form(&client, &format!("{origin}/ping")).await;
        let before = hits(&upstream_hits, "/from-action");
        let response = client
            .post(format!("{origin}/_albedo/action/ping"))
            .header("cookie", &tab)
            .form(&[("_csrf", csrf.as_str()), ("_albedo_return", back.as_str()), ("note", "x")])
            .send()
            .await
            .unwrap();
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(
                hits(&upstream_hits, "/from-action"),
                before,
                "attempt {attempt}: a refused call never reached the upstream"
            );
            assert!(response.headers().contains_key("retry-after"), "a 429 says when to retry");
        }
        statuses.push(status);
    }
    let stderr = fs::read_to_string(app.join("serve.stderr.log")).unwrap_or_default();
    assert_eq!(
        hits(&upstream_hits, "/from-action"),
        4,
        "exactly the declared limit went out; statuses {statuses:?}\nserver stderr:\n{stderr}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::TOO_MANY_REQUESTS).count(),
        2,
        "the fifth and sixth submits were refused: {statuses:?}"
    );

    // ── 2 · a different caller, through a different door, finds it spent ──
    let (tab, csrf, back) = served_form(&client, &format!("{origin}/sign-in")).await;
    let registered = client
        .post(format!("{origin}/_albedo/auth/password/register"))
        .header("cookie", &tab)
        .form(&[
            ("_csrf", csrf.as_str()),
            ("_albedo_return", back.as_str()),
            ("email", "grace@example.com"),
            ("password", "a-long-passphrase"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(registered.status(), StatusCode::SEE_OTHER);
    let session = cookie(&registered, "__Host-albedo_session=").expect("signed in");

    let refused = client
        .get(format!("{origin}/guestbook"))
        .header("cookie", format!("{tab}; {session}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "a caller with no calls of their own is refused by the host's shared budget"
    );
    assert_eq!(hits(&upstream_hits, "/from-middleware"), 0);
}

/// An app with one declared source (`limit`) and a public form action that calls
/// it once per submit. Returns the project directory; `home` keeps first-run out.
fn scaffold_pinging_app(home: &Path, work: &Path, base: &str, limit: &str) -> std::path::PathBuf {
    run_ok(albedo(home, work).args(["init", "split"]), "albedo init");
    let app = work.join("split");
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        config.replacen(
            "export default {",
            &format!(
                "export default {{\n  sources: {{ api: {{ base: \"{base}\", limit: \"{limit}\", routes: {{ status: {{ path: \"/status\" }} }} }} }},"
            ),
            1,
        ),
    )
    .unwrap();
    fs::write(
        app.join("src/routes/ping.tsx"),
        format!(
            r#"import {{ action }} from "albedo";
export const ping = action(async ({{ form }}) => {{ await fetch("{base}/from-action"); }});
export default function Ping() {{
  return (<form action="action:ping" method="POST"><input name="note" /></form>);
}}
"#
        ),
    )
    .unwrap();
    run_ok(albedo(home, &app).arg("build"), "albedo build");
    app
}

/// 🔑 `ALBEDO_INSTANCES` splits a declared host budget: an instance told it is
/// one of two admits half of `limit`, so two of them together admit the whole.
/// And a split that cannot be honoured, or a count that is not a count, stops
/// the boot instead of being rounded into something that over-admits.
#[tokio::test]
async fn albedo_instances_splits_the_declared_host_budget_and_refuses_what_it_cannot_split() {
    let (base, upstream_hits) = upstream().await;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold_pinging_app(home.path(), work.path(), &base, "4/m");

    // ── refusals first: a boot that must not start ──
    for (value, needle) in [("8", "cannot be split across 8"), ("two", "not a positive whole number")] {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut child = albedo(home.path(), &app)
            .args(["serve", ".", "--port", &port.to_string()])
            .env("ALBEDO_INSTANCES", value)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("ALBEDO_INSTANCES={value}: the server started instead of refusing");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let output = child.wait_with_output().unwrap();
        let said = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        assert!(!status.success(), "ALBEDO_INSTANCES={value} must not boot");
        assert!(said.contains(needle), "ALBEDO_INSTANCES={value}: the refusal says why:\n{said}");
    }

    // ── one of two instances admits half ──
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let child = albedo(home.path(), &app)
        .args(["serve", ".", "--port", &port.to_string()])
        .env("ALBEDO_INSTANCES", "2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _server = Server { child };
    let origin = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while reqwest::get(format!("{origin}/_albedo/runtime.js")).await.is_err() {
        assert!(Instant::now() < deadline, "serve never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let client = reqwest::Client::builder().redirect(Policy::none()).build().unwrap();

    let mut statuses = Vec::new();
    for _ in 0..4 {
        let (tab, csrf, back) = served_form(&client, &format!("{origin}/ping")).await;
        let response = client
            .post(format!("{origin}/_albedo/action/ping"))
            .header("cookie", &tab)
            .form(&[("_csrf", csrf.as_str()), ("_albedo_return", back.as_str()), ("note", "x")])
            .send()
            .await
            .unwrap();
        statuses.push(response.status());
    }
    assert_eq!(hits(&upstream_hits, "/from-action"), 2, "half of 4/m: {statuses:?}");
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::TOO_MANY_REQUESTS).count(),
        2,
        "{statuses:?}"
    );
}
