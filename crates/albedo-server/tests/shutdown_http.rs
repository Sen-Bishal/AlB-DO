//! Graceful shutdown with a live viewer attached — `docker-optimizations.md` § 3b.
//!
//! 🔴 The 13.1 verification used only short requests, and missed that **every
//! live page holds an event stream that never finishes**: a graceful shutdown
//! waited out the whole `shutdown_timeout_ms` for it, every time, and
//! `docker stop` killed the process mid-drain. So this test does what that one
//! did not — **holds a long-lived connection open through the shutdown**.
//!
//! Booted in-process from an app `albedo init` made and `albedo build` built,
//! because the shutdown has to be *started* by the test: this rig is Windows,
//! which delivers no SIGTERM. [`AlbedoServer::run_until`] is the same serve loop
//! `run` uses, with the signal swapped for a future the test resolves. The real
//! signal is exercised in a Linux container (TODO § 15.6 Round 6).

use albedo_server::{boot_production_server, ProductionServerOptions};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd).env("USERPROFILE", home).env("HOME", home);
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

/// A booted server: its base URL, the trigger that starts its shutdown, and the
/// task that finishes when its serve loop returns.
struct Running {
    base: String,
    stop: tokio::sync::oneshot::Sender<()>,
    served: tokio::task::JoinHandle<Result<(), albedo_server::RuntimeError>>,
}

async fn boot(app: &Path) -> Running {
    let contract = dom_render_compiler::dev_contract::resolve_dev_contract(&[], app)
        .expect("the built app's contract resolves");
    let mut opts = ProductionServerOptions::from_contract(&contract);
    opts.port = free_port();
    let base = format!("http://127.0.0.1:{}", opts.port);
    let server = boot_production_server(&opts).expect("the built app boots");

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(server.run_until(
        move |_| {
            let _ = ready_tx.send(());
        },
        async move {
            let _ = stop_rx.await;
        },
    ));
    ready_rx.await.expect("the server signals ready");
    Running { base, stop, served }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_viewer_does_not_hold_a_graceful_shutdown_open() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    run_ok(albedo(home.path(), work.path()).args(["init", "shutdownapp"]), "albedo init");
    let app = work.path().join("shutdownapp");
    std::fs::write(
        app.join("src/middleware.ts"),
        r#"
import { next } from "albedo/middleware";
export default function middleware(request) {
  if (request.path === "/slow") { const end = Date.now() + 8000; while (Date.now() < end) {} }
  return next();
}
"#,
    )
    .unwrap();
    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");

    // ── 1 · a PHOSPHOR trunk, held open, under the default 5 s drain ──
    // SAFETY-OF-ENV: this test is the only one in its binary, and the variable
    // is set only for the second boot below.
    std::env::remove_var("ALBEDO_SHUTDOWN_TIMEOUT_MS");
    let server = boot(&app).await;
    let mut trunk = reqwest::get(format!("{}/_albedo/phosphor", server.base))
        .await
        .expect("the trunk opens");
    assert_eq!(trunk.status(), 200);
    let hello = trunk.chunk().await.expect("readable").expect("a first event");
    assert!(
        String::from_utf8_lossy(&hello).contains("hello"),
        "CONTROL — the trunk is live before the shutdown: {hello:?}"
    );

    let started = Instant::now();
    server.stop.send(()).expect("the serve loop is listening");
    let served = tokio::time::timeout(Duration::from_secs(20), server.served)
        .await
        .expect("the serve loop returned")
        .expect("joined");
    let drained_in = started.elapsed();
    assert!(served.is_ok(), "{served:?}");
    assert!(
        drained_in < Duration::from_millis(1500),
        "🔴 shutdown took {drained_in:?} with one live viewer — the event stream held the drain \
         open until shutdown_timeout_ms (5 s)"
    );
    // The stream was ENDED, not severed by the deadline: it drains to a clean end.
    let rest = tokio::time::timeout(Duration::from_secs(5), async {
        while trunk.chunk().await?.is_some() {}
        Ok::<_, reqwest::Error>(())
    })
    .await
    .expect("the stream ends");
    assert!(rest.is_ok(), "the trunk was cut rather than ended: {rest:?}");

    // ── 2 · a request that genuinely runs long, with the drain lowered ──
    std::env::set_var("ALBEDO_SHUTDOWN_TIMEOUT_MS", "600");
    let server = boot(&app).await;
    let slow = tokio::spawn(reqwest::get(format!("{}/slow", server.base)));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let started = Instant::now();
    server.stop.send(()).expect("the serve loop is listening");
    tokio::time::timeout(Duration::from_secs(20), server.served)
        .await
        .expect("the serve loop returned")
        .expect("joined")
        .expect("serve ended cleanly");
    let drained_in = started.elapsed();
    std::env::remove_var("ALBEDO_SHUTDOWN_TIMEOUT_MS");
    assert!(
        drained_in >= Duration::from_millis(550),
        "CONTROL — the drain must actually wait for the in-flight request: {drained_in:?}"
    );
    assert!(
        drained_in < Duration::from_millis(3000),
        "🔴 the drain took {drained_in:?} — `ALBEDO_SHUTDOWN_TIMEOUT_MS=600` was not the bound"
    );
    slow.abort();
}
