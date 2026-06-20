//! Workstream C · process-lifecycle benches.
//!
//! The wire harness ([`crate::dev::serve_bench`]) measures latency
//! against a server the caller already booted — so its "cold" sample is
//! the first *uncontended* hit, not a fresh process. Two numbers it
//! structurally can't produce live here:
//!
//!   1. **Cold-process-start TTFB** — spawn the server, wait until it's
//!      listening, then time the very first HTTP request. Repeated over
//!      N boots so boot time and first-render latency get a real
//!      distribution, not a single anecdote. (This module.)
//!   2. **Build-time clean-vs-incremental** — see [`build_bench`].
//!
//! Both wrap a subprocess, so the orchestration is written against
//! small traits ([`Spawner`] / [`ServerProcess`]) the CLI fills with
//! `std::process::Command` and tests fill with in-process stubs. The
//! timing/aggregation logic is therefore unit-tested without spawning a
//! real `albedo serve` — the real-process path is the operator's to run
//! (same split as the Next/Remix comparison: we measure, they boot).

use crate::dev::serve_bench::{measure_single, probe_tcp_ready, LatencyStats, RequestSpec};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod build_bench;

/// A spawned server the harness can later terminate. `shutdown` is
/// best-effort and idempotent — called once per iteration after the
/// cold hit, and again on Drop as a safety net so a panicking run never
/// leaks a child process holding the port.
pub trait ServerProcess {
    /// Terminate the process. Must tolerate being called when the
    /// process has already exited.
    fn shutdown(&mut self);
}

/// Boots one server instance. Each call must yield a *fresh* process
/// and the base url it is reachable at (`http://host:port`). Returning
/// the url from `spawn` — rather than fixing it in the config — lets the
/// test spawner bind a new ephemeral port per boot (no port-reuse
/// races) while the real CLI spawner returns its single configured url.
pub trait Spawner {
    fn spawn(&self) -> std::io::Result<(Box<dyn ServerProcess>, String)>;
}

#[derive(Debug, thiserror::Error)]
pub enum ProcBenchError {
    #[error("iterations must be >= 1")]
    NoIterations,
    #[error("failed to spawn server process: {0}")]
    Spawn(std::io::Error),
    #[error("server never became ready within the timeout: {0}")]
    NeverReady(String),
    #[error("cold request failed: {0}")]
    ColdRequest(String),
}

/// Cold-start run configuration.
#[derive(Debug, Clone)]
pub struct ColdStartConfig {
    /// The request whose first-hit latency is measured (typically
    /// `GET /`). Readiness is a separate TCP probe so this stays cold.
    pub probe: RequestSpec,
    /// How many fresh boots to sample. Each is a full spawn → ready →
    /// hit → kill cycle.
    pub iterations: u32,
    /// Max wall time to wait for the listener to accept per boot.
    pub ready_timeout: Duration,
    /// Gap between readiness poll attempts.
    pub poll_interval: Duration,
    /// Per-request timeout for the measured cold hit.
    pub request_timeout: Duration,
    /// Pause after killing a process before the next boot, giving the OS
    /// time to release the port (relevant when the real spawner reuses a
    /// fixed port across iterations).
    pub settle: Duration,
}

impl Default for ColdStartConfig {
    fn default() -> Self {
        Self {
            probe: RequestSpec::get("root", "/"),
            iterations: 10,
            ready_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(25),
            request_timeout: Duration::from_secs(10),
            settle: Duration::from_millis(250),
        }
    }
}

/// One cold boot's measurement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColdStartSample {
    /// Spawn → first successful TCP connect.
    pub boot_ready_ms: f64,
    /// First HTTP request after ready: time to first byte.
    pub first_ttfb_ms: f64,
    /// First HTTP request: time to last byte.
    pub first_total_ms: f64,
    pub status: u16,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColdStartReport {
    pub version: String,
    pub iterations: u32,
    /// Per-boot raw samples, in boot order.
    pub samples: Vec<ColdStartSample>,
    /// Distribution of boot-to-ready times across all boots.
    pub boot_ready: LatencyStats,
    /// Distribution of first-hit TTFB across all boots.
    pub first_ttfb: LatencyStats,
    /// Distribution of first-hit total across all boots.
    pub first_total: LatencyStats,
}

impl ColdStartReport {
    pub const VERSION: &'static str = "cold-start/1";

    /// README-ready Markdown summary.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "### Cold process start — {} boots\n\n",
            self.iterations
        ));
        out.push_str("| Metric | p50 | p90 | p99 | min | max |\n");
        out.push_str("|---|--:|--:|--:|--:|--:|\n");
        let row = |name: &str, s: &LatencyStats| {
            format!(
                "| {name} | {:.2} ms | {:.2} ms | {:.2} ms | {:.2} ms | {:.2} ms |\n",
                s.p50_ms, s.p90_ms, s.p99_ms, s.min_ms, s.max_ms
            )
        };
        out.push_str(&row("boot → ready", &self.boot_ready));
        out.push_str(&row("first-hit TTFB", &self.first_ttfb));
        out.push_str(&row("first-hit total", &self.first_total));
        out
    }
}

/// Run the cold-start bench: `iterations` full spawn → ready → hit →
/// kill cycles, aggregated into a distribution. The first failed boot
/// or cold request aborts the run with a clear error rather than
/// reporting partial numbers (a half-measured cold-start is not
/// citable).
pub fn run_cold_starts(
    spawner: &dyn Spawner,
    config: &ColdStartConfig,
) -> std::result::Result<ColdStartReport, ProcBenchError> {
    if config.iterations == 0 {
        return Err(ProcBenchError::NoIterations);
    }

    let mut samples = Vec::with_capacity(config.iterations as usize);
    for _ in 0..config.iterations {
        let (mut process, base_url) = spawner.spawn().map_err(ProcBenchError::Spawn)?;

        // Readiness — a bare TCP connect so the HTTP hit below is cold.
        let boot_ready =
            match probe_tcp_ready(&base_url, config.ready_timeout, config.poll_interval) {
                Ok(d) => d,
                Err(err) => {
                    process.shutdown();
                    return Err(ProcBenchError::NeverReady(err.to_string()));
                }
            };

        // The measured cold hit.
        let cold = measure_single(&base_url, &config.probe, config.request_timeout);
        process.shutdown();

        match cold {
            Ok(sample) => samples.push(ColdStartSample {
                boot_ready_ms: boot_ready.as_secs_f64() * 1000.0,
                first_ttfb_ms: sample.ttfb_ms,
                first_total_ms: sample.total_ms,
                status: sample.status,
                bytes: sample.bytes,
            }),
            Err(err) => return Err(ProcBenchError::ColdRequest(err.to_string())),
        }

        if !config.settle.is_zero() {
            std::thread::sleep(config.settle);
        }
    }

    let boot_ready = LatencyStats::from_millis(samples.iter().map(|s| s.boot_ready_ms).collect());
    let first_ttfb = LatencyStats::from_millis(samples.iter().map(|s| s.first_ttfb_ms).collect());
    let first_total = LatencyStats::from_millis(samples.iter().map(|s| s.first_total_ms).collect());

    Ok(ColdStartReport {
        version: ColdStartReport::VERSION.to_string(),
        iterations: config.iterations,
        samples,
        boot_ready,
        first_ttfb,
        first_total,
    })
}

/// CLI-facing spawner: runs a command (e.g. `albedo serve --port 3000`)
/// as a child process and reports the operator-fixed base url. The same
/// port is reused every boot, so `ColdStartConfig::settle` should be
/// non-zero to let the OS release it between iterations.
pub struct ProcessSpawner {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub base_url: String,
    /// Inherit stdout/stderr when true; silence them (the default) so
    /// the server's logging doesn't drown the bench output.
    pub inherit_io: bool,
}

/// A `std::process::Child` wrapped to satisfy [`ServerProcess`]. Killing
/// is best-effort and the child is also killed on Drop so a panic mid-
/// run never orphans a server holding the port.
struct ChildProcess {
    child: Option<std::process::Child>,
}

impl ServerProcess for ChildProcess {
    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Spawner for ProcessSpawner {
    fn spawn(&self) -> std::io::Result<(Box<dyn ServerProcess>, String)> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        if self.inherit_io {
            cmd.stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit());
        } else {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        let child = cmd.spawn()?;
        Ok((
            Box::new(ChildProcess { child: Some(child) }),
            self.base_url.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A stub server bound to an ephemeral port, served on a background
    /// thread until its shutdown flag flips. Each boot is a brand-new
    /// listener on a fresh port — exactly what a real cold start is —
    /// so there are no port-reuse races in the test.
    struct StubProcess {
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    }

    impl ServerProcess for StubProcess {
        fn shutdown(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
        }
    }

    /// Spawner that stands up a fresh in-process HTTP stub per boot and
    /// counts how many times it was asked to spawn.
    struct StubSpawner {
        spawns: Arc<AtomicUsize>,
        body: &'static str,
    }

    impl Spawner for StubSpawner {
        fn spawn(&self) -> std::io::Result<(Box<dyn ServerProcess>, String)> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let port = listener.local_addr()?.port();
            listener.set_nonblocking(true)?;
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = Arc::clone(&shutdown);
            let body = self.body;
            std::thread::spawn(move || {
                loop {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buf = [0u8; 1024];
                            let _ = stream.read(&mut buf);
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = stream.write_all(resp.as_bytes());
                            let _ = stream.flush();
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok((Box::new(StubProcess { shutdown }), format!("http://127.0.0.1:{port}")))
        }
    }

    fn fast_config(iterations: u32) -> ColdStartConfig {
        ColdStartConfig {
            probe: RequestSpec::get("root", "/"),
            iterations,
            ready_timeout: Duration::from_secs(2),
            poll_interval: Duration::from_millis(5),
            request_timeout: Duration::from_secs(2),
            settle: Duration::ZERO,
        }
    }

    #[test]
    fn cold_start_aggregates_over_multiple_boots() {
        let spawns = Arc::new(AtomicUsize::new(0));
        let spawner = StubSpawner {
            spawns: Arc::clone(&spawns),
            body: "<!doctype html><body>cold</body>",
        };
        let report = run_cold_starts(&spawner, &fast_config(4)).expect("cold-start runs");

        assert_eq!(spawns.load(Ordering::SeqCst), 4, "one spawn per iteration");
        assert_eq!(report.samples.len(), 4);
        assert_eq!(report.boot_ready.count, 4);
        assert_eq!(report.first_ttfb.count, 4);
        for s in &report.samples {
            assert_eq!(s.status, 200, "stub answers 200");
            assert!(s.bytes > 0, "read a body");
            assert!(s.boot_ready_ms >= 0.0);
            assert!(s.first_ttfb_ms >= 0.0);
        }
        assert!(report.to_markdown().contains("boot → ready"));
    }

    #[test]
    fn zero_iterations_is_an_error() {
        let spawner = StubSpawner {
            spawns: Arc::new(AtomicUsize::new(0)),
            body: "x",
        };
        assert!(matches!(
            run_cold_starts(&spawner, &fast_config(0)),
            Err(ProcBenchError::NoIterations)
        ));
    }

    /// A spawner whose "server" never binds anything reachable — the
    /// readiness probe must time out and the run must fail loudly, not
    /// report a bogus zero.
    struct DeadSpawner;
    impl Spawner for DeadSpawner {
        fn spawn(&self) -> std::io::Result<(Box<dyn ServerProcess>, String)> {
            struct Noop;
            impl ServerProcess for Noop {
                fn shutdown(&mut self) {}
            }
            // Port 1 is privileged + closed → connect always fails.
            Ok((Box::new(Noop), "http://127.0.0.1:1".to_string()))
        }
    }

    #[test]
    fn never_ready_server_fails_loudly() {
        let config = ColdStartConfig {
            ready_timeout: Duration::from_millis(150),
            poll_interval: Duration::from_millis(20),
            ..fast_config(1)
        };
        assert!(matches!(
            run_cold_starts(&DeadSpawner, &config),
            Err(ProcBenchError::NeverReady(_))
        ));
    }
}
