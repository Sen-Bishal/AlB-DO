//! 15.7 · the Prometheus scrape, against an app `albedo init` made and `albedo
//! build` built.
//!
//! The inspector's `/__albedo/api/metrics` already existed and proved nothing for
//! this: it is mounted only in debug builds, so on the `albedo serve` anyone
//! deploys it is a 404. These tests ask the questions a scrape target has to
//! answer — does traffic move the counters, is the scrape kept off the app's own
//! port, does a taken metrics port fail the boot, and does `ALBEDO_METRICS_ADDR`
//! reach the real binary.

use albedo_server::{boot_production_server, ProductionServerOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd).env("USERPROFILE", home).env("HOME", home);
    cmd.env_remove("ALBEDO_METRICS_ADDR");
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

fn built_app(home: &Path, work: &Path, name: &str) -> PathBuf {
    run_ok(albedo(home, work).args(["init", name]), "albedo init");
    let app = work.join(name);
    run_ok(albedo(home, &app).arg("build"), "albedo build");
    app
}

/// The value of the one sample whose line starts with `series` (name plus any
/// labels, exactly as rendered).
fn sample(text: &str, series: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' '))
        .and_then(|value| value.trim().parse().ok())
}

async fn scrape(metrics_port: u16) -> String {
    let response = reqwest::get(format!("http://127.0.0.1:{metrics_port}/metrics"))
        .await
        .expect("the scrape answers");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8",
        "Prometheus picks its parser from this header"
    );
    response.text().await.expect("scrape body")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn traffic_moves_the_scrape_and_the_scrape_stays_off_the_app_port() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let app = built_app(home.path(), work.path(), "metricsapp");

    let contract = dom_render_compiler::dev_contract::resolve_dev_contract(&[], &app)
        .expect("the built app's contract resolves");
    let mut opts = ProductionServerOptions::from_contract(&contract);
    opts.port = free_port();
    let metrics_port = free_port();
    let base = format!("http://127.0.0.1:{}", opts.port);

    // ── a taken metrics port fails the boot, and `on_ready` never fires ──
    let squatter = std::net::TcpListener::bind(("127.0.0.1", metrics_port)).expect("squat");
    let server = boot_production_server(&opts)
        .expect("the built app boots")
        .with_metrics_addr(([127, 0, 0, 1], metrics_port).into());
    let (ready_tx, mut ready_rx) = tokio::sync::oneshot::channel::<()>();
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        server.run_until(
            move |_| {
                let _ = ready_tx.send(());
            },
            std::future::pending(),
        ),
    )
    .await
    .expect("a boot that cannot bind returns instead of serving");
    let err = result.expect_err("🔴 the server booted with its metrics port taken");
    assert!(err.to_string().contains("metrics"), "the error names the listener: {err}");
    assert!(ready_rx.try_recv().is_err(), "`on_ready` fired for a failed boot");
    drop(squatter);

    // ── the real boot ──
    let server = boot_production_server(&opts)
        .expect("the built app boots")
        .with_metrics_addr(([127, 0, 0, 1], metrics_port).into());
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

    let before = scrape(metrics_port).await;
    assert!(before.contains("albedo_build_info{version="), "{before}");
    let pool_size = sample(&before, "albedo_engine_pool_size").expect("serve runs a QuickJS pool");
    assert!(pool_size >= 1.0, "{before}");
    assert_eq!(sample(&before, "albedo_engine_pool_live"), Some(pool_size));
    let ok_before = sample(&before, "albedo_http_requests_total{method=\"GET\",code=\"200\"}").unwrap_or(0.0);

    for _ in 0..3 {
        let page = reqwest::get(format!("{base}/")).await.expect("page");
        assert_eq!(page.status(), 200);
        page.text().await.expect("page body");
    }
    let missing = reqwest::get(format!("{base}/definitely-not-a-route")).await.expect("404");
    assert_eq!(missing.status(), 404);

    // 🔑 The scrape is not an app route: the app port must not hand it out.
    let leaked = reqwest::get(format!("{base}/metrics")).await.expect("app /metrics");
    let leaked = leaked.text().await.unwrap_or_default();
    assert!(
        !leaked.contains("albedo_http_requests_total"),
        "🔴 the app's public port served the scrape"
    );
    let stray = reqwest::get(format!("http://127.0.0.1:{metrics_port}/"))
        .await
        .expect("metrics listener root");
    assert_eq!(stray.status(), 404, "the metrics listener serves /metrics and nothing else");

    let after = scrape(metrics_port).await;
    let ok_after = sample(&after, "albedo_http_requests_total{method=\"GET\",code=\"200\"}")
        .expect("GET 200 counted");
    assert!(
        ok_after - ok_before >= 3.0,
        "🔴 three page loads moved GET/200 by {} :\n{after}",
        ok_after - ok_before
    );
    assert!(
        sample(&after, "albedo_http_requests_total{method=\"GET\",code=\"404\"}").unwrap_or(0.0) >= 1.0,
        "the 404 is counted under its own code:\n{after}"
    );
    let count = sample(&after, "albedo_http_request_duration_seconds_count").expect("histogram");
    let inf = sample(&after, "albedo_http_request_duration_seconds_bucket{le=\"+Inf\"}").expect("+Inf");
    assert_eq!(count, inf, "_count must equal the +Inf bucket");
    assert!(count >= 5.0, "{after}");
    assert_eq!(sample(&after, "albedo_http_requests_in_flight"), Some(0.0));

    // ── an open event stream is a gauge, and leaves it when it closes ──
    assert_eq!(sample(&after, "albedo_http_event_streams_open"), Some(0.0), "CONTROL");
    let mut trunk = reqwest::get(format!("{base}/_albedo/phosphor")).await.expect("trunk");
    assert_eq!(trunk.status(), 200);
    trunk.chunk().await.expect("readable").expect("hello frame");
    let open = scrape(metrics_port).await;
    assert_eq!(
        sample(&open, "albedo_http_event_streams_open"),
        Some(1.0),
        "🔴 a live PHOSPHOR trunk is not counted:\n{open}"
    );
    assert_eq!(
        sample(&open, "albedo_http_requests_in_flight"),
        Some(0.0),
        "a stream whose head is sent is not in flight — it would pin the gauge for a tab's life"
    );

    drop(trunk);
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut closed = String::new();
    while Instant::now() < deadline {
        closed = scrape(metrics_port).await;
        if sample(&closed, "albedo_http_event_streams_open") == Some(0.0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        sample(&closed, "albedo_http_event_streams_open"),
        Some(0.0),
        "🔴 a closed trunk stayed on the gauge:\n{closed}"
    );

    stop.send(()).expect("the serve loop is listening");
    tokio::time::timeout(Duration::from_secs(20), served)
        .await
        .expect("the serve loop returned")
        .expect("joined")
        .expect("serve ended cleanly");
    assert!(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{metrics_port}/metrics"))
            .send()
            .await
            .is_err(),
        "the metrics listener must not outlive the server"
    );
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The deploy path: `ALBEDO_METRICS_ADDR` on the real binary. Without this the
/// variable could be parsed by a unit test and never reach `serve` — which is
/// exactly how every `ALBEDO_*_TIMEOUT_MS` sat dead until 14.1.
#[tokio::test]
async fn serve_opens_the_scrape_listener_only_when_the_environment_asks() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let app = built_app(home.path(), work.path(), "metricsenv");

    let (port, metrics_port) = (free_port(), free_port());
    let port_arg = port.to_string();
    let mut server = Server(
        albedo(home.path(), &app)
            .args(["serve", ".", "--port", &port_arg])
            .env("ALBEDO_METRICS_ADDR", format!("127.0.0.1:{metrics_port}"))
            .stdout(Stdio::null())
            .stderr(fs::File::create(app.join("serve.log")).expect("log"))
            .spawn()
            .expect("serve starts"),
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut body = None;
    while Instant::now() < deadline && body.is_none() {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{metrics_port}/metrics"))
            .send()
            .await
        {
            body = response.text().await.ok();
        } else if let Ok(Some(status)) = server.0.try_wait() {
            panic!(
                "serve exited ({status}):\n{}",
                fs::read_to_string(app.join("serve.log")).unwrap_or_default()
            );
        } else {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let body = body.unwrap_or_else(|| {
        panic!(
            "🔴 `ALBEDO_METRICS_ADDR=127.0.0.1:{metrics_port} albedo serve .` never answered a scrape:\n{}",
            fs::read_to_string(app.join("serve.log")).unwrap_or_default()
        )
    });
    assert!(body.contains("albedo_build_info"), "{body}");
    drop(server);

    // ── CONTROL: no variable, no listener ──
    let (port, metrics_port) = (free_port(), free_port());
    let port_arg = port.to_string();
    let mut server = Server(
        albedo(home.path(), &app)
            .args(["serve", ".", "--port", &port_arg])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("serve starts"),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut up = false;
    while Instant::now() < deadline {
        if client
            .get(format!("http://127.0.0.1:{port}/_albedo/runtime.js"))
            .send()
            .await
            .is_ok()
        {
            up = true;
            break;
        }
        if let Ok(Some(_)) = server.0.try_wait() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "CONTROL — the app itself must be up before its absence of metrics means anything");
    assert!(
        client
            .get(format!("http://127.0.0.1:{metrics_port}/metrics"))
            .send()
            .await
            .is_err(),
        "🔴 metrics were opened without anyone asking"
    );
}
