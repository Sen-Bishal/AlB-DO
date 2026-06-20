//! Workstream C · serve-time HTTP latency harness.
//!
//! The Phase-G `benches/parity_*` benches pin the framework's
//! *in-process* costs (FCP bytes, opcode size, action-dispatch µs,
//! cold-load ms). What they can't show is the number an operator
//! actually feels: end-to-end **request latency over the wire** against
//! a running `albedo serve` — TTFB for the SSR shell, full-body time,
//! p50/p90/p99 under concurrency, cold (first hit after boot) vs warm
//! (steady state). That's the headline "honest numbers vs Next/Remix"
//! deliverable, and it has to be measured against a `--release` binary
//! to mean anything.
//!
//! This is a deliberately zero-dependency load generator: a raw
//! HTTP/1.1 client over `std::net::TcpStream`, matching the repo's
//! hand-rolled `read_http_request_head` / base64 style. No reqwest, no
//! hyper-client, no async runtime — just threads and a socket, so the
//! measurement adds the least possible scheduling noise of its own and
//! reproduces with nothing but `cargo`. It points at a URL the caller
//! already booted; spawning the server is the driver's job.
//!
//! Methodology notes (kept honest, mirrored into the README table):
//!   * One TCP connection per request (`Connection: close`). This
//!     folds connect cost into every sample — consistent across any
//!     framework you point a comparable tool at, and the conservative
//!     choice (keep-alive would only make ALBEDO look faster).
//!   * TTFB = time from just-before-write to the first response byte.
//!     Total = time to EOF (last byte). We read to close, so both
//!     `Content-Length` and `Transfer-Encoding: chunked` responses
//!     (the streaming shell uses chunked) are handled identically.
//!   * Cold = the first request after the caller booted the server,
//!     measured once, sequentially, before warmup. Warm = the measured
//!     batch after `warmup` discarded requests.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// HTTP method the harness can issue. Kept minimal — GET for the SSR
/// shell, POST for `action()` round-trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One endpoint to exercise. `body`/`content_type` apply to POST; for
/// the `/_albedo/action` path the body must be a valid bincode
/// `ActionEnvelope` (the driver constructs it) — the harness ships it
/// verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSpec {
    pub name: String,
    pub method: Method,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl RequestSpec {
    pub fn get(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            method: Method::Get,
            path: path.into(),
            body: None,
            content_type: None,
        }
    }

    pub fn post(
        name: impl Into<String>,
        path: impl Into<String>,
        body: Vec<u8>,
        content_type: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            method: Method::Post,
            path: path.into(),
            body: Some(body),
            content_type: Some(content_type.into()),
        }
    }
}

/// Harness configuration. `base_url` is `http://host:port` (https is
/// out of scope — raw TCP only).
#[derive(Debug, Clone)]
pub struct ServeBenchConfig {
    pub base_url: String,
    pub warmup: u32,
    pub samples: u32,
    pub concurrency: usize,
    pub timeout: Duration,
    /// When true, each worker reuses ONE TCP connection across all its
    /// requests (HTTP/1.1 keep-alive), so the per-request TCP-connect
    /// cost is paid once instead of every sample. This isolates the
    /// server's render+serve cost from connection churn — the truer
    /// steady-state number. When false, every request opens a fresh
    /// connection with `Connection: close` (the conservative default).
    pub keep_alive: bool,
    pub requests: Vec<RequestSpec>,
}

impl Default for ServeBenchConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:3000".to_string(),
            warmup: 50,
            samples: 500,
            concurrency: 16,
            timeout: Duration::from_secs(10),
            keep_alive: false,
            requests: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServeBenchError {
    #[error("invalid base url '{0}' (expected http://host:port)")]
    InvalidUrl(String),
    #[error("could not resolve '{0}'")]
    UnresolvedHost(String),
    #[error("server at {url} is unreachable: {source}")]
    Unreachable {
        url: String,
        source: std::io::Error,
    },
    #[error("no request specs configured")]
    NoRequests,
}

/// Latency percentile summary over a sample set, in milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LatencyStats {
    pub count: usize,
    pub min_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
}

impl LatencyStats {
    /// Compute stats from raw millisecond samples. Empty input yields a
    /// zeroed summary (count 0) rather than panicking — a request that
    /// never succeeded shows up as count 0 in the report, not a crash.
    pub fn from_millis(mut samples: Vec<f64>) -> Self {
        if samples.is_empty() {
            return Self {
                count: 0,
                min_ms: 0.0,
                max_ms: 0.0,
                mean_ms: 0.0,
                p50_ms: 0.0,
                p90_ms: 0.0,
                p99_ms: 0.0,
            };
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let count = samples.len();
        let sum: f64 = samples.iter().sum();
        Self {
            count,
            min_ms: samples[0],
            max_ms: samples[count - 1],
            mean_ms: sum / count as f64,
            p50_ms: percentile(&samples, 50.0),
            p90_ms: percentile(&samples, 90.0),
            p99_ms: percentile(&samples, 99.0),
        }
    }
}

/// Nearest-rank percentile over an already-sorted ascending slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// A single cold-hit measurement (the first request after boot).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColdSample {
    pub status: u16,
    pub ttfb_ms: f64,
    pub total_ms: f64,
    pub bytes: usize,
}

/// Per-endpoint result: the one cold hit plus warm TTFB / total stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointResult {
    pub name: String,
    pub method: Method,
    pub path: String,
    /// A representative status code observed in the warm batch (the
    /// modal/last success). Non-2xx here means the endpoint is
    /// erroring — the latency numbers are then meaningless and the
    /// caller should investigate, not cite them.
    pub status: u16,
    pub ok_ratio: f64,
    pub cold: ColdSample,
    pub warm_ttfb: LatencyStats,
    pub warm_total: LatencyStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeBenchReport {
    pub version: String,
    pub generated_at: String,
    pub base_url: String,
    pub warmup: u32,
    pub samples: u32,
    pub concurrency: usize,
    pub keep_alive: bool,
    pub endpoints: Vec<EndpointResult>,
}

impl ServeBenchReport {
    pub const VERSION: &'static str = "serve-bench/1";

    /// Render a Markdown table suitable for pasting into the README
    /// perf section. One row per endpoint, warm percentiles + cold.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "### Serve-time latency — `{}`\n\n",
            self.base_url
        ));
        let conn_mode = if self.keep_alive {
            "keep-alive (conn reused)"
        } else {
            "1 conn/req, `Connection: close`"
        };
        out.push_str(&format!(
            "_{} warm samples/endpoint at concurrency {} ({})._\n\n",
            self.samples, self.concurrency, conn_mode
        ));
        out.push_str(
            "| Endpoint | Status | TTFB p50 | TTFB p99 | Total p50 | Total p99 | Cold TTFB |\n",
        );
        out.push_str(
            "|---|---|--:|--:|--:|--:|--:|\n",
        );
        for ep in &self.endpoints {
            out.push_str(&format!(
                "| {} `{} {}` | {} ({:.0}% ok) | {:.2} ms | {:.2} ms | {:.2} ms | {:.2} ms | {:.2} ms |\n",
                ep.name,
                ep.method.as_str(),
                ep.path,
                ep.status,
                ep.ok_ratio * 100.0,
                ep.warm_ttfb.p50_ms,
                ep.warm_ttfb.p99_ms,
                ep.warm_total.p50_ms,
                ep.warm_total.p99_ms,
                ep.cold.ttfb_ms,
            ));
        }
        out
    }
}

/// Parse `http://host:port` into a resolved socket address + the host
/// header value. http-only; rejects anything else loudly.
fn resolve(base_url: &str) -> std::result::Result<(std::net::SocketAddr, String), ServeBenchError> {
    let rest = base_url
        .strip_prefix("http://")
        .ok_or_else(|| ServeBenchError::InvalidUrl(base_url.to_string()))?;
    // Drop any trailing path on the base url; we only want authority.
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(ServeBenchError::InvalidUrl(base_url.to_string()));
    }
    let with_port = if authority.contains(':') {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    let addr = with_port
        .to_socket_addrs()
        .map_err(|_| ServeBenchError::UnresolvedHost(authority.to_string()))?
        .next()
        .ok_or_else(|| ServeBenchError::UnresolvedHost(authority.to_string()))?;
    Ok((addr, with_port))
}

/// Serialize a request to wire bytes. `keep_alive` flips the
/// `Connection:` header so close-mode and keep-alive-mode issue the
/// header the server should honour.
fn build_request_bytes(spec: &RequestSpec, host_header: &str, keep_alive: bool) -> Vec<u8> {
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: {}\r\nAccept: */*\r\n",
        spec.method.as_str(),
        spec.path,
        host_header,
        connection,
    );
    if let Some(body) = &spec.body {
        if let Some(ct) = &spec.content_type {
            head.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    if let Some(body) = &spec.body {
        out.extend_from_slice(body);
    }
    out
}

/// CLOSE mode: one request/response over a fresh connection, read to
/// EOF. Works for any framing (`Content-Length` or chunked) since the
/// server closes the socket at the end. Returns
/// (status, ttfb, total, body_bytes).
fn do_request(
    addr: std::net::SocketAddr,
    host_header: &str,
    spec: &RequestSpec,
    timeout: Duration,
) -> std::io::Result<(u16, Duration, Duration, usize)> {
    let started = Instant::now();
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true).ok();

    stream.write_all(&build_request_bytes(spec, host_header, false))?;
    stream.flush()?;

    let mut buf = [0u8; 16 * 1024];
    let mut total_bytes = 0usize;
    let mut header_acc: Vec<u8> = Vec::with_capacity(256);
    let mut status: u16 = 0;
    let mut ttfb: Option<Duration> = None;

    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if ttfb.is_none() {
            ttfb = Some(started.elapsed());
        }
        if status == 0 {
            header_acc.extend_from_slice(&buf[..n.min(256)]);
            status = parse_status_line(&header_acc).unwrap_or(0);
        }
        total_bytes += n;
    }

    let total = started.elapsed();
    Ok((status, ttfb.unwrap_or(total), total, total_bytes))
}

/// KEEP-ALIVE mode: a persistent connection that frames each response
/// precisely (via `Content-Length` or chunked decoding) so the socket
/// can be reused for the next request. Without exact framing the next
/// request's bytes would be read as part of this response's body.
struct KeepAliveConn {
    stream: TcpStream,
    /// Bytes already read past the end of the previous response, carried
    /// into the next `round_trip`.
    leftover: Vec<u8>,
}

impl KeepAliveConn {
    fn connect(addr: std::net::SocketAddr, timeout: Duration) -> std::io::Result<Self> {
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            leftover: Vec::new(),
        })
    }

    fn round_trip(
        &mut self,
        spec: &RequestSpec,
        host_header: &str,
    ) -> std::io::Result<(u16, Duration, Duration, usize)> {
        let started = Instant::now();
        self.stream
            .write_all(&build_request_bytes(spec, host_header, true))?;
        self.stream.flush()?;

        let mut buf = std::mem::take(&mut self.leftover);
        let mut read_buf = [0u8; 16 * 1024];
        // TTFB counts from write to the first response byte. If leftover
        // carried bytes from a pipelined read, first byte already arrived.
        let mut ttfb: Option<Duration> = if buf.is_empty() { None } else { Some(Duration::ZERO) };

        // Phase 1: accumulate until headers are complete.
        let (header_end, status, content_length, chunked) = loop {
            if let Some(he) = find_header_end(&buf) {
                let (status, cl, ch) = parse_head(&buf[..he]);
                break (he, status, cl, ch);
            }
            let n = self.stream.read(&mut read_buf)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before response headers",
                ));
            }
            if ttfb.is_none() {
                ttfb = Some(started.elapsed());
            }
            buf.extend_from_slice(&read_buf[..n]);
        };

        // Phase 2: accumulate until the body is fully framed.
        let body_end = loop {
            let complete = if chunked {
                chunked_body_end(&buf, header_end)
            } else {
                let need = header_end + content_length.unwrap_or(0);
                if buf.len() >= need {
                    Some(need)
                } else {
                    None
                }
            };
            if let Some(end) = complete {
                break end;
            }
            let n = self.stream.read(&mut read_buf)?;
            if n == 0 {
                // Server closed mid-body: take what we have, stop reusing.
                break buf.len();
            }
            buf.extend_from_slice(&read_buf[..n]);
        };

        let total = started.elapsed();
        let body_bytes = body_end.saturating_sub(header_end);
        // Stash anything beyond this response for the next round-trip.
        let end = body_end.min(buf.len());
        self.leftover = buf.split_off(end);
        Ok((status, ttfb.unwrap_or(total), total, body_bytes))
    }
}

/// Index just past the `\r\n\r\n` header terminator, or `None` if the
/// headers aren't complete yet.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Parse the status code + body-framing signals from the header block.
/// Returns (status, content_length, is_chunked).
fn parse_head(head: &[u8]) -> (u16, Option<usize>, bool) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" => {
                if value.to_ascii_lowercase().contains("chunked") {
                    chunked = true;
                }
            }
            _ => {}
        }
    }
    (status, content_length, chunked)
}

/// Walk a chunked body starting at `start`. Returns the index just past
/// the terminating `0\r\n\r\n`, or `None` if more bytes are needed.
fn chunked_body_end(buf: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    loop {
        let line_end = find_crlf(buf, pos)?;
        let size_line = std::str::from_utf8(&buf[pos..line_end]).ok()?;
        // Strip any chunk extensions (`;name=value`).
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).ok()?;
        let data_start = line_end + 2; // skip the size line's CRLF
        if size == 0 {
            // Terminating chunk: expect a final CRLF (no trailers from
            // the server we target). Need 2 more bytes to confirm.
            if buf.len() >= data_start + 2 {
                return Some(data_start + 2);
            }
            return None;
        }
        let next = data_start + size + 2; // chunk data + its trailing CRLF
        if buf.len() < next {
            return None;
        }
        pos = next;
    }
}

/// Index of the `\r` in the next `\r\n` at or after `from`, or `None`
/// if no complete CRLF is present yet.
fn find_crlf(buf: &[u8], from: usize) -> Option<usize> {
    if from >= buf.len() {
        return None;
    }
    buf[from..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|p| from + p)
}

/// Pull the numeric status out of `HTTP/1.1 200 OK\r\n...`. Returns
/// `None` until the full status line has arrived.
///
/// Decodes ONLY the bytes up to the first `\r\n`, not the whole buffer:
/// a binary response body (e.g. the bincode `OpcodeFrame` an `action()`
/// returns) is not valid UTF-8, so decoding the accumulated header+body
/// as a string would fail and report status 0 for a perfectly good 200.
fn parse_status_line(bytes: &[u8]) -> Option<u16> {
    let line_end = bytes.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&bytes[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let _http = parts.next()?;
    let code = parts.next()?;
    code.parse::<u16>().ok()
}

/// Run the full harness against the configured server. Verifies
/// reachability first (a clear error beats a wall of timeouts), then
/// benches each endpoint in turn.
pub fn run(config: &ServeBenchConfig) -> std::result::Result<ServeBenchReport, ServeBenchError> {
    if config.requests.is_empty() {
        return Err(ServeBenchError::NoRequests);
    }
    let (addr, host_header) = resolve(&config.base_url)?;

    // Reachability probe — connect once before committing to the run.
    TcpStream::connect_timeout(&addr, config.timeout).map_err(|source| {
        ServeBenchError::Unreachable {
            url: config.base_url.clone(),
            source,
        }
    })?;

    let mut endpoints = Vec::with_capacity(config.requests.len());
    for spec in &config.requests {
        endpoints.push(bench_endpoint(addr, &host_header, spec, config));
    }

    Ok(ServeBenchReport {
        version: ServeBenchReport::VERSION.to_string(),
        generated_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default(),
        base_url: config.base_url.clone(),
        warmup: config.warmup,
        samples: config.samples,
        concurrency: config.concurrency,
        keep_alive: config.keep_alive,
        endpoints,
    })
}

/// Issue exactly one request over a fresh connection and return its
/// cold-sample measurement. Used by the process-lifecycle harness
/// ([`crate::dev::proc_bench`]) to time the very first hit after a
/// boot — the truly-cold render, before any warmup touches caches.
pub fn measure_single(
    base_url: &str,
    spec: &RequestSpec,
    timeout: Duration,
) -> std::result::Result<ColdSample, ServeBenchError> {
    let (addr, host_header) = resolve(base_url)?;
    let (status, ttfb, total, bytes) =
        do_request(addr, &host_header, spec, timeout).map_err(|source| {
            ServeBenchError::Unreachable {
                url: base_url.to_string(),
                source,
            }
        })?;
    Ok(ColdSample {
        status,
        ttfb_ms: dur_ms(ttfb),
        total_ms: dur_ms(total),
        bytes,
    })
}

/// Poll a TCP connect against `base_url` until it succeeds, returning
/// the elapsed time from first attempt to first successful connect.
///
/// Readiness is deliberately a bare TCP accept, NOT an HTTP request:
/// that way the first *HTTP* request the caller then issues is still
/// genuinely cold (first render, first cache fill). A successful connect
/// means the server's listener is bound and accepting — for `albedo
/// serve` that's the readiness signal, since it binds only once the
/// runtime is loaded.
pub fn probe_tcp_ready(
    base_url: &str,
    ready_timeout: Duration,
    poll_interval: Duration,
) -> std::result::Result<Duration, ServeBenchError> {
    let (addr, _host) = resolve(base_url)?;
    let started = Instant::now();
    // Per-attempt connect timeout is bounded so a hung SYN doesn't blow
    // the whole readiness budget on one attempt.
    let attempt_timeout = poll_interval.min(Duration::from_millis(250)).max(Duration::from_millis(20));
    loop {
        match TcpStream::connect_timeout(&addr, attempt_timeout) {
            Ok(_) => return Ok(started.elapsed()),
            Err(source) => {
                if started.elapsed() >= ready_timeout {
                    return Err(ServeBenchError::Unreachable {
                        url: base_url.to_string(),
                        source,
                    });
                }
                std::thread::sleep(poll_interval);
            }
        }
    }
}

fn bench_endpoint(
    addr: std::net::SocketAddr,
    host_header: &str,
    spec: &RequestSpec,
    config: &ServeBenchConfig,
) -> EndpointResult {
    // Cold: a single sequential hit first, before any warmup touches
    // the server's caches/JIT. Best-effort — if the very first request
    // errors we still report it (status 0) so the failure is visible.
    let cold = match do_request(addr, host_header, spec, config.timeout) {
        Ok((status, ttfb, total, bytes)) => ColdSample {
            status,
            ttfb_ms: dur_ms(ttfb),
            total_ms: dur_ms(total),
            bytes,
        },
        Err(_) => ColdSample {
            status: 0,
            ttfb_ms: 0.0,
            total_ms: 0.0,
            bytes: 0,
        },
    };

    // Warmup — discarded.
    for _ in 0..config.warmup {
        let _ = do_request(addr, host_header, spec, config.timeout);
    }

    // Measured batch, spread across `concurrency` worker threads. Each
    // thread claims work by incrementing a shared counter and stops
    // once the claimed index reaches `samples` — a monotonic count-up
    // so the total is exactly `samples` with no underflow or double-
    // counting, regardless of how the work divides across threads.
    let total = config.samples as usize;
    let claimed = Arc::new(AtomicUsize::new(0));
    let concurrency = config.concurrency.max(1);
    let spec = Arc::new(spec.clone());
    let host_header = Arc::new(host_header.to_string());
    let timeout = config.timeout;
    let keep_alive = config.keep_alive;

    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let claimed = Arc::clone(&claimed);
        let spec = Arc::clone(&spec);
        let host_header = Arc::clone(&host_header);
        handles.push(std::thread::spawn(move || {
            let mut ttfbs = Vec::new();
            let mut totals = Vec::new();
            let mut ok = 0usize;
            let mut done = 0usize;
            // Keep-alive mode reuses one connection across this worker's
            // requests; it's lazily (re)established and dropped on error
            // so a broken socket reconnects rather than failing the rest.
            let mut conn: Option<KeepAliveConn> = None;
            loop {
                if claimed.fetch_add(1, Ordering::Relaxed) >= total {
                    break;
                }
                done += 1;
                let result = if keep_alive {
                    if conn.is_none() {
                        conn = KeepAliveConn::connect(addr, timeout).ok();
                    }
                    match conn.as_mut() {
                        Some(c) => {
                            let r = c.round_trip(&spec, &host_header);
                            if r.is_err() {
                                conn = None; // drop the dead socket; reconnect next iter
                            }
                            r
                        }
                        None => Err(std::io::Error::new(
                            std::io::ErrorKind::NotConnected,
                            "keep-alive connect failed",
                        )),
                    }
                } else {
                    do_request(addr, &host_header, &spec, timeout)
                };
                match result {
                    Ok((status, ttfb, total, _bytes)) => {
                        if (200..400).contains(&status) {
                            ok += 1;
                        }
                        ttfbs.push(dur_ms(ttfb));
                        totals.push(dur_ms(total));
                    }
                    Err(_) => {}
                }
            }
            (ttfbs, totals, ok, done)
        }));
    }

    let mut all_ttfb = Vec::new();
    let mut all_total = Vec::new();
    let mut ok_total = 0usize;
    let mut done_total = 0usize;
    for handle in handles {
        if let Ok((ttfbs, totals, ok, done)) = handle.join() {
            all_ttfb.extend(ttfbs);
            all_total.extend(totals);
            ok_total += ok;
            done_total += done;
        }
    }

    let ok_ratio = if done_total == 0 {
        0.0
    } else {
        ok_total as f64 / done_total as f64
    };

    EndpointResult {
        name: spec.name.clone(),
        method: spec.method,
        path: spec.path.clone(),
        status: if cold.status != 0 { cold.status } else { 0 },
        ok_ratio,
        cold,
        warm_ttfb: LatencyStats::from_millis(all_ttfb),
        warm_total: LatencyStats::from_millis(all_total),
    }
}

fn dur_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Spin a throwaway HTTP/1.1 server on an ephemeral port that
    /// replies to every request with a fixed body. Accepts forever,
    /// each connection on its own thread (so concurrent client requests
    /// are served in parallel), and is detached — the OS reclaims it on
    /// process exit. Returns the bound port.
    fn spawn_stub(body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    // Drain the request head so the client's write completes.
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        port
    }

    #[test]
    fn percentile_nearest_rank() {
        // 0..=100 → value equals index, so nearest-rank percentiles
        // land on exact values with no interpolation ambiguity.
        let v: Vec<f64> = (0..=100).map(|n| n as f64).collect();
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&v, 90.0), 90.0);
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(percentile(&v, 0.0), 0.0);
        assert_eq!(percentile(&v, 100.0), 100.0);
    }

    #[test]
    fn stats_from_empty_is_zeroed_not_panic() {
        let s = LatencyStats::from_millis(Vec::new());
        assert_eq!(s.count, 0);
        assert_eq!(s.p99_ms, 0.0);
    }

    #[test]
    fn resolve_parses_host_and_port() {
        let (_addr, host) = resolve("http://127.0.0.1:3000").expect("resolve");
        assert_eq!(host, "127.0.0.1:3000");
        assert!(resolve("ftp://nope").is_err());
        assert!(resolve("http://").is_err());
    }

    #[test]
    fn end_to_end_against_stub_server() {
        let warmup = 2u32;
        let samples = 5u32;
        let port = spawn_stub("<!doctype html><body>ok</body>");

        let config = ServeBenchConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            warmup,
            samples,
            concurrency: 2,
            timeout: Duration::from_secs(2),
            keep_alive: false,
            requests: vec![RequestSpec::get("root", "/")],
        };

        let report = run(&config).expect("bench runs against stub");
        assert_eq!(report.endpoints.len(), 1);
        let ep = &report.endpoints[0];
        assert_eq!(ep.cold.status, 200, "cold hit should see 200");
        assert_eq!(ep.warm_ttfb.count, samples as usize, "all samples recorded");
        assert_eq!(ep.ok_ratio, 1.0, "every warm request 2xx");
        assert!(ep.warm_ttfb.p50_ms >= 0.0);
        assert!(ep.cold.bytes > 0, "cold hit read a body");

        // The markdown table renders the endpoint row.
        let md = report.to_markdown();
        assert!(md.contains("root"));
        assert!(md.contains("TTFB p99"));
    }

    /// Keep-alive stub: serves MANY requests on a single connection,
    /// framing each with `Content-Length` (no close between responses).
    fn spawn_keepalive_stub(body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ka stub");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut acc = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        // Read until we have a full request head, serve a
                        // response, then loop for the next on the SAME socket.
                        let n = match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        acc.extend_from_slice(&chunk[..n]);
                        while let Some(end) = acc
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|p| p + 4)
                        {
                            acc.drain(..end);
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            if stream.write_all(resp.as_bytes()).is_err() {
                                return;
                            }
                            let _ = stream.flush();
                        }
                    }
                });
            }
        });
        port
    }

    #[test]
    fn keep_alive_reuses_connection_and_frames_responses() {
        let samples = 20u32;
        let port = spawn_keepalive_stub("<!doctype html><body>keepalive</body>");

        let config = ServeBenchConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            warmup: 3,
            samples,
            concurrency: 4,
            timeout: Duration::from_secs(2),
            keep_alive: true,
            requests: vec![RequestSpec::get("root", "/")],
        };

        let report = run(&config).expect("keep-alive bench runs");
        let ep = &report.endpoints[0];
        // Exact framing means every one of the N requests is read back
        // cleanly on the reused socket — no desync, all 2xx.
        assert_eq!(ep.warm_ttfb.count, samples as usize, "all samples framed");
        assert_eq!(ep.ok_ratio, 1.0, "every keep-alive request 2xx");
        assert!(report.keep_alive, "report records keep-alive mode");
        assert!(report.to_markdown().contains("keep-alive"));
    }

    /// Action stub: reads the full request (head + `Content-Length`
    /// body), attempts to decode the body as an `ActionEnvelope`, and
    /// replies 200 only when it decodes cleanly (400 otherwise). This
    /// lets a test assert the harness actually delivered a valid
    /// envelope over the wire, not just that it constructed one.
    fn spawn_action_stub() -> u16 {
        use crate::ir::action::decode_action_envelope;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind action stub");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut acc = Vec::new();
                    let mut chunk = [0u8; 1024];
                    // Read until we have headers + the declared body.
                    let (head_end, content_length) = loop {
                        if let Some(he) = acc
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|p| p + 4)
                        {
                            let (_s, cl, _ch) = parse_head(&acc[..he]);
                            break (he, cl.unwrap_or(0));
                        }
                        let n = match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        acc.extend_from_slice(&chunk[..n]);
                    };
                    while acc.len() < head_end + content_length {
                        let n = match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        acc.extend_from_slice(&chunk[..n]);
                    }
                    let body = &acc[head_end..(head_end + content_length).min(acc.len())];
                    let status_line = match decode_action_envelope(body) {
                        Ok(_) => "HTTP/1.1 200 OK",
                        Err(_) => "HTTP/1.1 400 Bad Request",
                    };
                    let resp = format!(
                        "{status_line}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        port
    }

    #[test]
    fn post_action_body_arrives_as_decodable_envelope() {
        use crate::ir::action::{encode_action_envelope, ActionEnvelope};
        let port = spawn_action_stub();
        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 0xABCD,
            event_kind: 2,
            payload: b"{\"_csrf\":\"x\"}".to_vec(),
        })
        .unwrap();

        let config = ServeBenchConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            warmup: 2,
            samples: 6,
            concurrency: 2,
            timeout: Duration::from_secs(2),
            keep_alive: false,
            requests: vec![RequestSpec::post(
                "submit",
                "/_albedo/action",
                body,
                "application/octet-stream",
            )],
        };

        let report = run(&config).expect("action bench runs");
        let ep = &report.endpoints[0];
        // The stub only answers 200 when the body decoded as an
        // envelope — a 100% ok ratio proves the wire delivery.
        assert_eq!(ep.method, Method::Post);
        assert_eq!(ep.cold.status, 200, "stub decoded the cold envelope");
        assert_eq!(ep.ok_ratio, 1.0, "every warm POST delivered a valid envelope");
    }

    #[test]
    fn chunked_body_end_finds_terminator() {
        // Two data chunks ("Wiki" + "pedia") then the 0-terminator.
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let mut buf = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let header_end = buf.len();
        buf.extend_from_slice(body);
        let end = chunked_body_end(&buf, header_end).expect("complete chunked body");
        assert_eq!(end, buf.len(), "end is just past the 0\\r\\n\\r\\n terminator");

        // Truncated (terminator missing) → needs more bytes.
        let partial = &buf[..buf.len() - 3];
        assert!(chunked_body_end(partial, header_end).is_none());
    }

    #[test]
    fn parse_head_reads_status_and_framing() {
        let (status, cl, chunked) =
            parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\nX-Foo: bar");
        assert_eq!(status, 200);
        assert_eq!(cl, Some(42));
        assert!(!chunked);

        let (status, cl, chunked) =
            parse_head(b"HTTP/1.1 503 Service Unavailable\r\nTransfer-Encoding: chunked");
        assert_eq!(status, 503);
        assert_eq!(cl, None);
        assert!(chunked);
    }

    #[test]
    fn parse_status_line_tolerates_binary_body() {
        // A 200 status line followed by a non-UTF-8 body (an action's
        // bincode OpcodeFrame). Decoding the whole buffer as a string
        // would fail; we must still read the 200 from the status line.
        let mut bytes = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0x00, 0xfe, 0x80, 0x01]); // invalid UTF-8
        assert_eq!(parse_status_line(&bytes), Some(200));

        // No CRLF yet → not enough bytes, returns None (keep reading).
        assert_eq!(parse_status_line(b"HTTP/1.1 200 O"), None);
    }

    #[test]
    fn unreachable_server_errors_clearly() {
        // Port 1 is privileged + almost certainly closed.
        let config = ServeBenchConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            timeout: Duration::from_millis(300),
            requests: vec![RequestSpec::get("root", "/")],
            ..Default::default()
        };
        let err = run(&config).expect_err("should fail to connect");
        assert!(matches!(err, ServeBenchError::Unreachable { .. }));
    }

    #[test]
    fn no_requests_is_an_error() {
        let config = ServeBenchConfig {
            requests: Vec::new(),
            ..Default::default()
        };
        assert!(matches!(run(&config), Err(ServeBenchError::NoRequests)));
    }
}
