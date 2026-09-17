//! `albedo serve` reads its bind address from the environment — under the flags.
//!
//! 🔴 Found 2026-09-06, fixed 2026-09-13: every `ALBEDO_SERVER_*` and
//! `ALBEDO_*_TIMEOUT_MS` variable was read only by a JSON config-file loader
//! that `serve` never uses, so `ALBEDO_SERVER_PORT=5477 albedo serve .` bound
//! 3000. The container template only appeared to honour them because its `CMD`
//! shell-expands them into `--host`/`--port`.
//!
//! Runs the real binary from `albedo init` and asks over a socket which port
//! answered. The flag-beats-environment half is the control: a fix that applied
//! the environment *over* everything would pass the first half and surprise
//! everyone who types `--port`.
//!
//! The timeout half is proven by `middleware_http.rs`, whose runaway test lowers
//! the request timeout through `ALBEDO_REQUEST_TIMEOUT_MS` and times it.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd).env("USERPROFILE", home).env("HOME", home);
    // The variables under test come only from where each test puts them.
    cmd.env_remove("ALBEDO_SERVER_PORT").env_remove("ALBEDO_SERVER_HOST");
    cmd
}

fn run_ok(cmd: &mut Command, what: &str) {
    let out = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    assert!(
        out.status.success(),
        "{what} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("addr")
        .port()
}

/// Wait until `port` answers HTTP — false if the server exits first or a minute
/// passes.
async fn answers(server: &mut Server, port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if reqwest::get(format!("http://127.0.0.1:{port}/_albedo/runtime.js"))
            .await
            .is_ok()
        {
            return true;
        }
        if let Ok(Some(_)) = server.0.try_wait() {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn refuses(port: u16) -> bool {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .is_err()
}

fn spawn(home: &Path, app: &Path, args: &[&str], port_env: u16, log: &str) -> Server {
    Server(
        albedo(home, app)
            .args(args)
            .env("ALBEDO_SERVER_PORT", port_env.to_string())
            .stdout(Stdio::null())
            .stderr(fs::File::create(app.join(log)).expect("log"))
            .spawn()
            .expect("serve starts"),
    )
}

#[tokio::test]
async fn serve_binds_the_environment_port_unless_a_flag_names_one() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    run_ok(albedo(home.path(), work.path()).args(["init", "envapp"]), "albedo init");
    let app = work.path().join("envapp");
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");

    // ── the environment, with no flag ──
    let env_port = free_port();
    let mut server = spawn(home.path(), &app, &["serve", "."], env_port, "env.log");
    assert!(
        answers(&mut server, env_port).await,
        "🔴 `ALBEDO_SERVER_PORT={env_port} albedo serve .` did not bind {env_port}:\n{}",
        fs::read_to_string(app.join("env.log")).unwrap_or_default()
    );
    drop(server);

    // ── CONTROL: an explicit flag beats the environment ──
    let (flag_port, ignored_port) = (free_port(), free_port());
    let flag = flag_port.to_string();
    let mut server = spawn(
        home.path(),
        &app,
        &["serve", ".", "--port", &flag],
        ignored_port,
        "flag.log",
    );
    assert!(
        answers(&mut server, flag_port).await,
        "`--port {flag_port}` must bind {flag_port} whatever the environment says:\n{}",
        fs::read_to_string(app.join("flag.log")).unwrap_or_default()
    );
    assert!(
        refuses(ignored_port).await,
        "the environment's port must not be bound when a flag names another"
    );
}
