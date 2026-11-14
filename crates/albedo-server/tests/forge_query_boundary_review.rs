//! T-0033 refusal through direct boot and both dev lifecycle paths.

use albedo_server::{boot_production_server, ProductionServerOptions};
use std::fs;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const PROBE: &str = "import { guestbook } from 'albedo/forge';\r\n\r\nexport default async function Probe() {\r\n  const n = await guestbook.where({ author: 'ada' }).count();\r\n  throw new Error('render-must-not-run');\r\n  return <p>{n}</p>;\r\n}\r\n";

fn command(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env("ALBEDO_NO_TUI", "1")
        .env("NO_COLOR", "1");
    cmd
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn scaffold(home: &Path, work: &Path) -> std::path::PathBuf {
    let output = command(home, work).args(["init", "app"]).output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    work.join("app")
}

fn available_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn assert_refusal(log: &str) {
    assert!(log.contains("src/routes/probe.tsx:4:19"), "{log}");
    assert!(
        log.contains("compiled and lowered") && log.contains("Q1b"),
        "{log}"
    );
    assert!(
        !log.contains("render-must-not-run"),
        "render ran before refusal: {log}"
    );
}

#[test]
fn direct_boot_refuses_changed_source_before_loading_stale_or_invalid_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dist = dir.path().join(".albedo/dist");
    fs::create_dir_all(src.join("routes")).unwrap();
    fs::create_dir_all(&dist).unwrap();
    fs::write(src.join("routes/probe.tsx"), PROBE).unwrap();
    // Deliberately invalid: the source refusal must win over artifact loading.
    fs::write(dist.join("render-manifest.v2.json"), b"not JSON").unwrap();
    let options = ProductionServerOptions {
        project_dir: dir.path().into(),
        source_root: src,
        dist_dir: dist,
        host: "127.0.0.1".into(),
        port: 0,
        dev_mode: false,
        hmr: false,
        forge: serde_json::from_str(r#"{"guestbook":{"fields":{"author":"text"}}}"#).unwrap(),
        sources: Default::default(),
        auth: Default::default(),
        uploads: Default::default(),
        email: None,
        redirects: Vec::new(),
        webhooks: Default::default(),
        tls: Default::default(),
        forge_db: None,
    };
    let error = match boot_production_server(&options) {
        Ok(_) => panic!("boot accepted the new query"),
        Err(error) => error.to_string(),
    };
    assert_refusal(&error);
}

#[test]
fn dev_cold_start_refuses_crlf_probe_before_listening() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    fs::write(app.join("src/routes/probe.tsx"), PROBE).unwrap();
    let log = app.join("cold-dev.log");
    let output = fs::File::create(&log).unwrap();
    let port = available_port();
    let mut server = Running(
        command(home.path(), &app)
            .args(["dev", ".", "--port", &port.to_string()])
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "dev did not refuse: {}",
            fs::read_to_string(&log).unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    assert_refusal(&fs::read_to_string(log).unwrap());
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
}

#[tokio::test]
async fn dev_reload_refuses_new_query_and_keeps_the_last_good_application() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    let log = app.join("reload-dev.log");
    let output = fs::File::create(&log).unwrap();
    let port = available_port();
    let mut server = Running(
        command(home.path(), &app)
            .args(["dev", ".", "--port", &port.to_string()])
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}/");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if client
            .get(&url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "dev exited: {}",
            fs::read_to_string(&log).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "dev not ready: {}",
            fs::read_to_string(&log).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    fs::write(app.join("src/routes/probe.tsx"), PROBE).unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let report = fs::read_to_string(&log).unwrap();
        if report.contains("rebuild failed") && report.contains("Q1b") {
            assert_refusal(&report);
            break;
        }
        assert!(Instant::now() < deadline, "no refusal on reload: {report}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(client.get(&url).send().await.unwrap().status().is_success());
    let probe = client
        .get(format!("http://127.0.0.1:{port}/probe"))
        .send()
        .await
        .unwrap();
    assert_eq!(probe.status(), reqwest::StatusCode::NOT_FOUND);
    let report = fs::read_to_string(app.join(".albedo/query-report.txt")).unwrap();
    assert!(report.contains("src/routes/probe.tsx:4:19"));
}
