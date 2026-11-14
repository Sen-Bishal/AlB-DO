//! A3 · `ALBEDO_ACCESS_LOG=json` through the real binary: one JSON object per
//! request on stdout, named by the `x-request-id` the response carries, with
//! the path but never the query, and the principal once someone is signed in.
//! A2's probes are logged like anything else.

use reqwest::redirect::Policy;
use std::fs;
use std::path::Path;
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

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd
}

fn cookie_named(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|value| value.starts_with(&format!("{name}=")))
        .map(str::to_string)
}

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn lines(stdout: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(stdout)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("{err}: {line}")))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn every_request_is_one_json_line_named_by_its_request_id() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = albedo(home.path(), work.path()).args(["init", "app"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let app = work.path().join("app");

    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let stdout = app.join("serve.stdout.log");
    let _server = Server {
        child: albedo(home.path(), &app)
            .args(["serve", ".", "--port", &port.to_string()])
            .env("ALBEDO_ACCESS_LOG", "json")
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(app.join("serve.stderr.log")).unwrap())
            .spawn()
            .unwrap(),
    };
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(90);
    while reqwest::get(format!("{base}{}", "/_albedo/health")).await.is_err() {
        assert!(Instant::now() < deadline, "albedo serve never answered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let client = reqwest::Client::builder().redirect(Policy::none()).build().unwrap();

    // A caller's own id threads through; a query never reaches the log.
    let traced = client
        .get(format!("{base}/?token=secret-in-a-query"))
        .header("x-request-id", "trace-1234")
        .send()
        .await
        .unwrap();
    assert_eq!(traced.headers()["x-request-id"], "trace-1234");

    // A forged id is replaced, not echoed.
    let forged = client
        .get(format!("{base}/"))
        .header("x-request-id", "two words")
        .send()
        .await
        .unwrap();
    let minted = forged.headers()["x-request-id"].to_str().unwrap().to_string();
    assert_ne!(minted, "two words");

    // Signed in, the line names who.
    let form = client.get(format!("{base}/sign-in")).send().await.unwrap();
    let tab = cookie_named(&form, "__Host-albedo-session").expect("a tab cookie");
    let csrf = hidden_field(&form.text().await.unwrap(), "_csrf").expect("a token");
    let registered = client
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &tab)
        .form(&[("_csrf", csrf.as_str()), ("email", "ada@example.test"), ("password", "access log passphrase")])
        .send()
        .await
        .unwrap();
    let session = cookie_named(&registered, "__Host-albedo_session").expect("signed in");
    let signed_in = client
        .get(format!("{base}/"))
        .header("cookie", format!("{tab}; {session}"))
        .send()
        .await
        .unwrap();
    let signed_in_id = signed_in.headers()["x-request-id"].to_str().unwrap().to_string();

    let deadline = Instant::now() + Duration::from_secs(10);
    let logged = loop {
        let logged = lines(&stdout);
        if logged.iter().any(|line| line["id"] == signed_in_id.as_str()) {
            break logged;
        }
        assert!(Instant::now() < deadline, "the signed-in request was never logged");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    let by_id = |id: &str| {
        logged
            .iter()
            .find(|line| line["id"] == id)
            .unwrap_or_else(|| panic!("no line for {id}: {logged:#?}"))
    };
    let first = by_id("trace-1234");
    assert_eq!(first["method"], "GET");
    assert_eq!(first["path"], "/");
    assert_eq!(first["client"], "127.0.0.1");
    assert!(first.get("principal").is_none(), "an anonymous visit named someone");
    by_id(&minted);
    assert!(by_id(&signed_in_id)["principal"].as_str().is_some_and(|who| !who.is_empty()));
    assert!(logged.iter().any(|line| line["path"] == "/_albedo/health"), "probes are logged too");
    let raw = fs::read_to_string(&stdout).unwrap();
    assert!(!raw.contains("secret-in-a-query"), "a query string reached the log");
}


#[tokio::test(flavor = "multi_thread")]
async fn tier_b_failures_are_visible_without_tracing_and_logged_per_request() {
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
    for route in ["broken", "other"] {
        let dir = app.join("src/routes").join(route);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("[id].tsx"),
            r#"export default function Failure({ params }) {
  if (params.id && params.id !== "ok") throw new Error("diagnostic-boom-" + params.id);
  return <p>healthy render</p>;
}"#,
        )
        .unwrap();
    }

    // Check CLI output both with and without access logging. Neither boot may
    // depend on an inherited tracing subscriber.
    for enabled in [true, false] {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let stdout = app.join(format!("diagnostic-{enabled}.stdout.log"));
        let stderr = app.join(format!("diagnostic-{enabled}.stderr.log"));
        let mut cmd = albedo(home.path(), &app);
        cmd.args(["serve", ".", "--port", &port.to_string()])
            .env_remove("RUST_LOG")
            .env_remove("ALBEDO_ACCESS_LOG")
            .stdout(fs::File::create(&stdout).unwrap())
            .stderr(fs::File::create(&stderr).unwrap());
        if enabled {
            cmd.env("ALBEDO_ACCESS_LOG", "json");
        }
        let _server = Server {
            child: cmd.spawn().unwrap(),
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
                "serve never answered: {}",
                fs::read_to_string(&stderr).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        for (id, path) in [
            ("fail-1", "/broken/a"),
            ("fail-2", "/broken/a"),
            ("fail-other", "/other/a"),
            ("fail-new", "/broken/b"),
            ("healthy", "/broken/ok"),
        ] {
            let response = client
                .get(format!("{base}{path}?token=do-not-log"))
                .header("x-request-id", id)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let body = response.text().await.unwrap();
            if id == "healthy" {
                assert!(body.contains("healthy render"), "{body}");
            } else {
                assert!(body.contains("data-albedo-error=\"error\""), "{body}");
                assert!(
                    !body.contains("diagnostic-boom"),
                    "private diagnostic leaked to body"
                );
            }
        }
        if enabled {
            let logged = lines(&stdout);
            for id in ["fail-1", "fail-2", "fail-other", "fail-new", "healthy"] {
                let matches: Vec<_> = logged.iter().filter(|line| line["id"] == id).collect();
                assert_eq!(matches.len(), 1, "one access line for {id}: {logged:?}");
                if id == "healthy" {
                    assert!(matches[0].get("render_error").is_none());
                } else {
                    let error = matches[0]["render_error"]
                        .as_str()
                        .expect("render_error text");
                    assert!(
                        error.contains("render registry failed"),
                        "full RenderError: {error}"
                    );
                    assert!(error.contains("diagnostic-boom-"), "{error}");
                }
            }
            assert!(!fs::read_to_string(&stdout).unwrap().contains("do-not-log"));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let raw = fs::read_to_string(&stdout).unwrap();
            let diagnostics: Vec<_> = raw
                .lines()
                .filter(|line| line.contains("render error on"))
                .collect();
            if diagnostics.len() >= 3 || Instant::now() >= deadline {
                assert_eq!(
                    diagnostics.len(),
                    3,
                    "one CLI line per route/message, access={enabled}: {raw}"
                );
                assert!(diagnostics.iter().any(
                    |line| line.contains("/broken/[id]") && line.contains("diagnostic-boom-a")
                ));
                assert!(
                    diagnostics
                        .iter()
                        .any(|line| line.contains("/other/[id]")
                            && line.contains("diagnostic-boom-a"))
                );
                assert!(diagnostics
                    .iter()
                    .any(|line| line.contains("diagnostic-boom-b")));
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
