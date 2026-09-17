//! MIDDLEWARE · 15.6 — `src/middleware.ts` under `albedo dev`, edited while it runs.
//!
//! The world a middleware lives on is swapped wholesale by the dev reloader, so a
//! reload *should* pick up a changed file for free. "Should" is the word this
//! codebase keeps paying for — `albedo dev` once broke FORGE after its first
//! reload, and nothing noticed until a browser did. So this runs the real
//! binary, edits the file four times, and asks over HTTP after each edit:
//!
//! 1. a changed body is served;
//! 2. a **broken** file fails the reload loudly and the last good middleware
//!    keeps serving — a dev server that went dark on a typo would be worse;
//! 3. a fixed file with a matcher takes effect, matcher included;
//! 4. a deleted file stops running.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

fn middleware(version: &str, matcher: Option<&str>) -> String {
    let config = matcher
        .map(|m| format!("export const config = {{ matcher: [\"{m}\"] }};\n"))
        .unwrap_or_default();
    format!(
        "import {{ next }} from \"albedo/middleware\";\n{config}export default function middleware() {{ return next({{ headers: {{ \"x-version\": \"{version}\" }} }}); }}\n"
    )
}

async fn version(base: &str, path: &str) -> Option<String> {
    reqwest::get(format!("{base}{path}"))
        .await
        .ok()?
        .headers()
        .get("x-version")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Poll until `check` holds, or fail with what was last seen.
async fn eventually<F, Fut>(what: &str, log: &Path, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if check().await {
            return;
        }
        if Instant::now() > deadline {
            let log = fs::read_to_string(log).unwrap_or_default();
            panic!("timed out waiting for: {what}\ndev log:\n{log}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[tokio::test]
async fn editing_the_middleware_under_albedo_dev_reloads_it_and_a_broken_edit_keeps_the_last_good_one() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let init = albedo(home.path(), work.path()).args(["init", "app"]).output().unwrap();
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    let app = work.path().join("app");
    let file = app.join("src/middleware.ts");
    fs::write(&file, middleware("one", None)).unwrap();

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let log = app.join("dev.log");
    let child = albedo(home.path(), &app)
        .args(["dev", ".", "--port", &port.to_string()])
        .stdout(Stdio::null())
        // The reload report — success and failure alike — is printed to stderr.
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .expect("albedo dev starts");
    let _server = Server { child };
    let base = format!("http://127.0.0.1:{port}");

    eventually("the first middleware to serve", &log, || async {
        version(&base, "/").await.as_deref() == Some("one")
    })
    .await;

    // ── 1 · a changed body ──
    fs::write(&file, middleware("two", None)).unwrap();
    eventually("the edited middleware to serve", &log, || async {
        version(&base, "/").await.as_deref() == Some("two")
    })
    .await;

    // ── 2 · a broken edit fails loudly and the last good one keeps serving ──
    let reloads_before = fs::read_to_string(&log).unwrap().matches("reload failed").count();
    fs::write(&file, middleware("broken", Some("/admin/(.*)"))).unwrap();
    eventually("the reload to fail", &log, || async {
        fs::read_to_string(&log).unwrap_or_default().matches("reload failed").count() > reloads_before
    })
    .await;
    let report = fs::read_to_string(&log).unwrap();
    assert!(
        report.contains("regex syntax"),
        "the failure names what was wrong with the file:\n{report}"
    );
    assert_eq!(
        version(&base, "/").await.as_deref(),
        Some("two"),
        "a broken edit must not take the dev server's middleware down with it"
    );

    // ── 3 · fixed, with a matcher — the matcher reloads too ──
    fs::write(&file, middleware("three", Some("/guestbook"))).unwrap();
    eventually("the scoped middleware to serve /guestbook", &log, || async {
        version(&base, "/guestbook").await.as_deref() == Some("three")
    })
    .await;
    assert_eq!(version(&base, "/").await, None, "and not paths outside its matcher");

    // ── 4 · deleted ──
    fs::remove_file(&file).unwrap();
    eventually("the middleware to stop running", &log, || async {
        reqwest::get(format!("{base}/guestbook"))
            .await
            .map(|r| r.status().is_success() && r.headers().get("x-version").is_none())
            .unwrap_or(false)
    })
    .await;
}
