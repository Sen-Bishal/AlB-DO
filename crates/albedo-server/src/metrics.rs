//! 15.7 · Prometheus text exposition on an opt-in, separate listener.
//!
//! The inspector's `/__albedo/api/metrics` is a dev view — mounted only in debug
//! builds, shaped for a sparkline, and 404 on the `albedo serve` anyone deploys.
//! Infrastructure needs a scrape target, so this is one: `ALBEDO_METRICS_ADDR`
//! binds a second plain-HTTP listener serving `GET /metrics` in the Prometheus
//! text format (0.0.4). OpenTelemetry is covered by the Collector's `prometheus`
//! receiver scraping the same endpoint; there is no OTLP push.
//!
//! Deliberately table stakes, hand-written rather than a client crate: every
//! number below is either an atomic this module owns or a counter a subsystem
//! already kept, read at scrape time.
//!
//! 🔑 **A separate listener, not a route.** A route on the public port would put
//! pool sizes and error rates on the internet unless every deployment remembered
//! to block it; a second address is closed until someone opens it, and it is the
//! shape Fly's `[metrics]` block, a Kubernetes scrape annotation and a
//! `127.0.0.1` bind all expect.
//!
//! 🪤 **Latency is measured to the response head, not the end of the body.** An
//! event stream lives for as long as the tab does, so a "full response" histogram
//! would be dominated by PHOSPHOR trunks and say nothing about the server. Open
//! streams are counted by their own gauge instead.

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use dashmap::DashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Upper bounds, in seconds — the Prometheus client libraries' defaults, so a
/// dashboard written against any other service reads this one unchanged.
const DURATION_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The HTTP counters, owned by the server and fed by one router layer.
#[derive(Debug)]
pub struct HttpMetrics {
    started_at: SystemTime,
    /// Keyed on `(method, status)`. Both are bounded — methods are folded to a
    /// fixed set below and status is a `u16` the server itself chose — so the
    /// label set cannot grow with traffic. **No path label**, for the same
    /// reason: a raw path is attacker-chosen cardinality.
    requests: DashMap<(&'static str, u16), AtomicU64>,
    /// One more slot than `DURATION_BUCKETS` for `+Inf`. Non-cumulative here;
    /// cumulated on render.
    buckets: [AtomicU64; DURATION_BUCKETS.len() + 1],
    duration_sum_us: AtomicU64,
    duration_count: AtomicU64,
    in_flight: AtomicI64,
    event_streams_open: AtomicI64,
}

impl Default for HttpMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpMetrics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started_at: SystemTime::now(),
            requests: DashMap::new(),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            duration_sum_us: AtomicU64::new(0),
            duration_count: AtomicU64::new(0),
            in_flight: AtomicI64::new(0),
            event_streams_open: AtomicI64::new(0),
        }
    }

    fn record(&self, method: &Method, status: StatusCode, elapsed: Duration) {
        let key = (method_label(method), status.as_u16());
        // `get` first: the steady state is an existing key, and a read lock on
        // one shard is cheaper than the write `entry` takes.
        if let Some(counter) = self.requests.get(&key) {
            counter.fetch_add(1, Ordering::Relaxed);
        } else {
            self.requests
                .entry(key)
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }

        let seconds = elapsed.as_secs_f64();
        let slot = DURATION_BUCKETS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(DURATION_BUCKETS.len());
        self.buckets[slot].fetch_add(1, Ordering::Relaxed);
        self.duration_sum_us.fetch_add(
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.duration_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// Decrements a gauge when dropped — so a request whose future is cancelled
/// (client gone, timeout) still leaves `in_flight`, and a stream whose body is
/// dropped for any reason still leaves `event_streams_open`.
struct GaugeGuard<'a>(&'a AtomicI64);

impl<'a> GaugeGuard<'a> {
    fn enter(gauge: &'a AtomicI64) -> Self {
        gauge.fetch_add(1, Ordering::Relaxed);
        Self(gauge)
    }
}

impl Drop for GaugeGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Owned twin of [`GaugeGuard`] for a body that outlives the layer's borrow.
struct OwnedGaugeGuard(Arc<HttpMetrics>);

impl Drop for OwnedGaugeGuard {
    fn drop(&mut self) {
        self.0.event_streams_open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The router layer: count, time to the response head, and track event streams
/// for as long as their bodies live.
pub async fn record_http(
    metrics: Arc<HttpMetrics>,
    request: Request<Body>,
    next: axum::middleware::Next,
) -> Response<Body> {
    let method = request.method().clone();
    let started = std::time::Instant::now();
    let response = {
        let _in_flight = GaugeGuard::enter(&metrics.in_flight);
        next.run(request).await
    };
    metrics.record(&method, response.status(), started.elapsed());

    let is_event_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !is_event_stream {
        return response;
    }
    metrics.event_streams_open.fetch_add(1, Ordering::Relaxed);
    let guard = OwnedGaugeGuard(metrics);
    let (parts, body) = response.into_parts();
    let body = Body::from_stream(futures_util::StreamExt::map(
        body.into_data_stream(),
        move |chunk| {
            let _alive = &guard;
            chunk
        },
    ));
    Response::from_parts(parts, body)
}

fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

/// Everything a scrape reads that this module does not own — gathered by the
/// server, which is the only thing that can reach the pool, the lanes and the
/// clients.
#[derive(Debug, Default, Clone)]
pub struct RuntimeGauges {
    pub engine_pool: Option<EnginePoolGauges>,
    pub phosphor_lanes: usize,
    pub broadcast_topics: usize,
    pub aperture: Option<dom_render_compiler::aperture::MetricsSnapshot>,
    pub shutter_degraded_decisions: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct EnginePoolGauges {
    pub size: usize,
    pub live: usize,
    pub busy: usize,
    pub interruptions: u64,
    pub replacements: u64,
    pub confinements: u64,
    pub confinement_failures: u64,
}

/// Render one scrape.
#[must_use]
pub fn render(http: &HttpMetrics, runtime: &RuntimeGauges) -> String {
    let mut out = String::with_capacity(4096);

    family(&mut out, "albedo_build_info", "gauge", "Always 1; the labels carry the build.");
    let _ = writeln!(
        out,
        "albedo_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );

    let start = http
        .started_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    gauge(&mut out, "process_start_time_seconds", "Start time of the process since the Unix epoch.", start);
    process_gauges(&mut out);

    family(
        &mut out,
        "albedo_http_requests_total",
        "counter",
        "HTTP responses by method and status code.",
    );
    let mut rows: Vec<((&'static str, u16), u64)> = http
        .requests
        .iter()
        .map(|entry| (*entry.key(), entry.value().load(Ordering::Relaxed)))
        .collect();
    rows.sort_unstable();
    for ((method, code), count) in rows {
        let _ = writeln!(
            out,
            "albedo_http_requests_total{{method=\"{method}\",code=\"{code}\"}} {count}"
        );
    }

    family(
        &mut out,
        "albedo_http_request_duration_seconds",
        "histogram",
        "Time from request to response head. Excludes the life of streamed bodies.",
    );
    let mut cumulative = 0u64;
    for (bound, bucket) in DURATION_BUCKETS.iter().zip(http.buckets.iter()) {
        cumulative += bucket.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "albedo_http_request_duration_seconds_bucket{{le=\"{bound}\"}} {cumulative}"
        );
    }
    cumulative += http.buckets[DURATION_BUCKETS.len()].load(Ordering::Relaxed);
    let _ = writeln!(
        out,
        "albedo_http_request_duration_seconds_bucket{{le=\"+Inf\"}} {cumulative}"
    );
    let _ = writeln!(
        out,
        "albedo_http_request_duration_seconds_sum {}",
        http.duration_sum_us.load(Ordering::Relaxed) as f64 / 1_000_000.0
    );
    // `_count` must equal the `+Inf` bucket; both are read from one pass so a
    // request landing mid-scrape cannot make them disagree.
    let _ = writeln!(out, "albedo_http_request_duration_seconds_count {cumulative}");

    gauge(
        &mut out,
        "albedo_http_requests_in_flight",
        "Requests whose response head has not been produced yet.",
        http.in_flight.load(Ordering::Relaxed).max(0) as f64,
    );
    gauge(
        &mut out,
        "albedo_http_event_streams_open",
        "text/event-stream responses whose body is still open (PHOSPHOR trunks, patch streams).",
        http.event_streams_open.load(Ordering::Relaxed).max(0) as f64,
    );

    if let Some(pool) = runtime.engine_pool {
        gauge(&mut out, "albedo_engine_pool_size", "QuickJS engines the pool was built with.", pool.size as f64);
        gauge(&mut out, "albedo_engine_pool_live", "Engines still serving; below size means slots were retired.", pool.live as f64);
        gauge(&mut out, "albedo_engine_pool_busy", "Engines checked out right now.", pool.busy as f64);
        counter(&mut out, "albedo_engine_interruptions_total", "Checkouts stopped for running past the job budget.", pool.interruptions);
        counter(&mut out, "albedo_engine_replacements_total", "Engines replaced after a job panicked on them.", pool.replacements);
        counter(&mut out, "albedo_engine_confinements_total", "SANDGATE-A realm rebuilds after a checkout.", pool.confinements);
        counter(&mut out, "albedo_engine_confinement_failures_total", "Realm rebuilds that failed.", pool.confinement_failures);
    }

    gauge(&mut out, "albedo_phosphor_lanes_open", "Open PHOSPHOR lanes.", runtime.phosphor_lanes as f64);
    gauge(&mut out, "albedo_broadcast_topics", "Registered broadcast topics.", runtime.broadcast_topics as f64);
    counter(
        &mut out,
        "albedo_shutter_degraded_decisions_total",
        "Rate-limit decisions made while SHUTTER's table was at capacity.",
        runtime.shutter_degraded_decisions,
    );

    if let Some(aperture) = runtime.aperture {
        counter(&mut out, "albedo_aperture_upstream_requests_total", "Outbound requests put on the wire.", aperture.upstream_requests);
        counter(&mut out, "albedo_aperture_not_modified_total", "Outbound requests answered 304.", aperture.not_modified);
        counter(&mut out, "albedo_aperture_fresh_hits_total", "Reads served from cache without contacting upstream.", aperture.fresh_hits);
        counter(&mut out, "albedo_aperture_coalesced_total", "Callers that joined an in-flight outbound request.", aperture.coalesced);
        counter(&mut out, "albedo_aperture_stale_on_error_total", "Stale bodies served because upstream failed.", aperture.stale_on_error);
        counter(&mut out, "albedo_aperture_throttled_total", "Outbound requests refused because the host budget was spent.", aperture.throttled);
    }

    out
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    family(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    family(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

/// The two process gauges every Prometheus dashboard assumes. Linux only — it
/// is where this is deployed, and `/proc` makes them one read each; elsewhere
/// they are omitted rather than reported as zero.
#[cfg(target_os = "linux")]
fn process_gauges(out: &mut String) {
    // statm: size resident shared … in pages.
    if let Some(pages) = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok())
    {
        // 4 KiB pages: every architecture this is shipped for (x86_64, aarch64
        // with the default kernel) uses them, and reading the real value needs
        // `libc::sysconf`, which is not worth a dependency for a gauge.
        gauge(out, "process_resident_memory_bytes", "Resident memory size in bytes.", (pages * 4096) as f64);
    }
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        gauge(out, "process_open_fds", "Number of open file descriptors.", entries.count() as f64);
    }
}

#[cfg(not(target_os = "linux"))]
fn process_gauges(_out: &mut String) {}

/// Serve `GET /metrics` on `listener` until `shutdown` flips.
///
/// Its own tiny router: nothing from the app's dispatch — no middleware, no
/// SHUTTER, no auth — is on this path, so a scrape can neither be rate-limited
/// into a false outage nor reach an app route.
pub async fn serve(
    listener: tokio::net::TcpListener,
    scrape: Arc<dyn Fn() -> String + Send + Sync>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let router = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let scrape = scrape.clone();
            async move {
                let mut response = Response::new(Body::from(scrape()));
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
                response
            }
        }),
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.wait_for(|fired| *fired).await;
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
        text.lines()
            .find(|line| line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no line starting `{prefix}` in:\n{text}"))
    }

    #[test]
    fn histogram_is_cumulative_and_count_matches_inf() {
        let http = HttpMetrics::new();
        http.record(&Method::GET, StatusCode::OK, Duration::from_millis(3));
        http.record(&Method::GET, StatusCode::OK, Duration::from_millis(30));
        http.record(&Method::POST, StatusCode::NOT_FOUND, Duration::from_secs(20));
        let text = render(&http, &RuntimeGauges::default());

        assert_eq!(line(&text, "albedo_http_request_duration_seconds_bucket{le=\"0.005\"}"), "albedo_http_request_duration_seconds_bucket{le=\"0.005\"} 1");
        assert_eq!(line(&text, "albedo_http_request_duration_seconds_bucket{le=\"0.05\"}"), "albedo_http_request_duration_seconds_bucket{le=\"0.05\"} 2");
        assert_eq!(line(&text, "albedo_http_request_duration_seconds_bucket{le=\"10\"}"), "albedo_http_request_duration_seconds_bucket{le=\"10\"} 2");
        assert_eq!(line(&text, "albedo_http_request_duration_seconds_bucket{le=\"+Inf\"}"), "albedo_http_request_duration_seconds_bucket{le=\"+Inf\"} 3");
        assert_eq!(line(&text, "albedo_http_request_duration_seconds_count"), "albedo_http_request_duration_seconds_count 3");
        assert_eq!(line(&text, "albedo_http_requests_total{method=\"GET\",code=\"200\"}"), "albedo_http_requests_total{method=\"GET\",code=\"200\"} 2");
        assert_eq!(line(&text, "albedo_http_requests_total{method=\"POST\",code=\"404\"}"), "albedo_http_requests_total{method=\"POST\",code=\"404\"} 1");
    }

    #[test]
    fn unknown_methods_fold_into_one_label() {
        let http = HttpMetrics::new();
        for verb in ["PROPFIND", "BREW", "X-ANYTHING"] {
            let method = Method::from_bytes(verb.as_bytes()).expect("method");
            http.record(&method, StatusCode::METHOD_NOT_ALLOWED, Duration::ZERO);
        }
        let text = render(&http, &RuntimeGauges::default());
        assert_eq!(
            text.lines().filter(|l| l.starts_with("albedo_http_requests_total{")).count(),
            1,
            "attacker-chosen methods must not mint label values:\n{text}"
        );
    }

    #[test]
    fn every_sample_has_a_type_line_before_it() {
        let http = HttpMetrics::new();
        http.record(&Method::GET, StatusCode::OK, Duration::from_millis(1));
        let runtime = RuntimeGauges {
            engine_pool: Some(EnginePoolGauges { size: 4, live: 4, ..Default::default() }),
            aperture: Some(dom_render_compiler::aperture::MetricsSnapshot {
                upstream_requests: 0,
                conditional_requests: 0,
                not_modified: 0,
                value_changes: 0,
                fresh_hits: 0,
                coalesced: 0,
                stale_on_error: 0,
                throttled: 0,
            }),
            ..Default::default()
        };
        let text = render(&http, &runtime);
        let mut typed = std::collections::HashSet::new();
        for l in text.lines() {
            if let Some(rest) = l.strip_prefix("# TYPE ") {
                let name = rest.split_whitespace().next().expect("name");
                assert!(typed.insert(name.to_string()), "`{name}` typed twice");
                continue;
            }
            if l.starts_with('#') {
                continue;
            }
            let sample = l.split(['{', ' ']).next().expect("sample name");
            let family = ["_bucket", "_sum", "_count"]
                .iter()
                .find_map(|suffix| sample.strip_suffix(suffix).filter(|base| typed.contains(*base)))
                .unwrap_or(sample);
            assert!(typed.contains(family), "`{sample}` has no # TYPE line before it");
        }
    }
}
