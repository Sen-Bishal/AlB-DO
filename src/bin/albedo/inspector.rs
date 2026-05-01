//! Binary-local dev inspector — sync-friendly variant served from the
//! `albedo dev` HTTP loop.
//!
//! The full-featured inspector lives in `albedo-server`, but `albedo dev`
//! cannot pull that crate in (cycle through `dom-render-compiler`). So this
//! module reimplements the small surface the CLI needs:
//!
//! * Wire-compatible `RenderEvent` / `GraphSnapshot` / `MetricsSnapshot` JSON
//!   so the same `inspector.html` asset (vendored from `albedo-server`)
//!   renders against either backend.
//! * A `RenderObserver` + `LaneObserver` pair that the dev process installs
//!   into `dom_render_compiler::runtime::render_observer`.
//! * Sync handlers for `/__albedo`, `/__albedo/api/{graph,events,metrics}`
//!   that plug straight into `handle_dev_connection`'s `if/else` ladder.
//!
//! SSE clients are kept in an `Arc<Mutex<Vec<TcpStream>>>`, mirroring the
//! HMR client list — events are pushed to all open streams when the
//! `RenderObserver` fires, with dead sockets pruned on the next push.

use dom_render_compiler::manifest::schema::{ComponentManifestEntry, RenderManifestV2, Tier};
use dom_render_compiler::runtime::render_observer::{
    LaneFrameReport, LaneObserver, RenderInfo, RenderObserver,
};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_64;

/// HTML asset served at `/__albedo`. Vendored from the `albedo-server` crate
/// so the dev CLI and the production server present the same UI.
pub const INSPECTOR_HTML: &str =
    include_str!("../../../crates/albedo-server/src/inspector/assets/inspector.html");

const DEFAULT_ASSUMED_ALPHA: f32 = 0.7;
const ALPHA_DELTA_WARNING: f32 = 0.20;
const LATENCY_RING_CAPACITY: usize = 256;
const TREND_RING_CAPACITY: usize = 60;

// ───────────────────────────── Wire types ─────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EventTier {
    A,
    B,
    C,
}

impl From<Tier> for EventTier {
    fn from(value: Tier) -> Self {
        match value {
            Tier::A => Self::A,
            Tier::B => Self::B,
            Tier::C => Self::C,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RenderEvent {
    pub component_id: u64,
    pub component_name: String,
    pub tier: EventTier,
    pub duration_us: u64,
    pub timestamp_ms: u64,
    pub cascade_children: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentNode {
    pub id: u64,
    pub label: String,
    pub tier: EventTier,
    pub albedo: f32,
    pub weight_bytes: u64,
    pub module_path: String,
    pub can_defer: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentEdge {
    pub from: u64,
    pub to: u64,
    pub multiplicity: u32,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphSource {
    Empty,
    Manifest,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphSnapshot {
    pub nodes: Vec<ComponentNode>,
    pub edges: Vec<ComponentEdge>,
    pub generated_at_ms: u64,
    pub source: GraphSource,
}

impl GraphSnapshot {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            generated_at_ms: now_ms(),
            source: GraphSource::Empty,
        }
    }

    pub fn from_manifest(manifest: &RenderManifestV2) -> Self {
        let entries: &[ComponentManifestEntry] = manifest.components.as_slice();
        let mut nodes = Vec::with_capacity(entries.len());
        let mut edges = Vec::new();
        for entry in entries {
            nodes.push(ComponentNode {
                id: entry.id,
                label: entry.name.clone(),
                tier: EventTier::from(entry.tier),
                albedo: priority_to_albedo(entry.priority),
                weight_bytes: entry.weight_bytes,
                module_path: entry.module_path.clone(),
                can_defer: entry.can_defer,
            });
            for dep in &entry.dependencies {
                edges.push(ComponentEdge {
                    from: entry.id,
                    to: *dep,
                    multiplicity: 1,
                });
            }
        }
        Self {
            nodes,
            edges,
            generated_at_ms: now_ms(),
            source: GraphSource::Manifest,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TierBreakdown {
    pub a: u64,
    pub b: u64,
    pub c: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HotComponent {
    pub id: u64,
    pub name: String,
    pub tier: EventTier,
    pub render_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlowComponent {
    pub id: u64,
    pub name: String,
    pub tier: EventTier,
    pub p95_us: u64,
    pub render_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub collective_albedo: f32,
    pub cascade_ratio: f32,
    pub assumed_alpha: f32,
    pub measured_alpha: f32,
    pub alpha_delta: f32,
    pub alpha_warning: bool,
    pub total_renders: u64,
    pub tracked_components: usize,
    pub tier_breakdown: TierBreakdown,
    pub hottest: Vec<HotComponent>,
    pub slowest: Vec<SlowComponent>,
    pub trend: Vec<f32>,
    pub lane_utilization: [f32; 4],
}

// ───────────────────── Metrics aggregator (sync) ─────────────────────

#[derive(Debug)]
struct ComponentCounter {
    name: String,
    tier: EventTier,
    render_count: u64,
    cascade_total: u64,
    last_seen_ms: u64,
    durations_us: VecDeque<u64>,
}

impl ComponentCounter {
    fn new(name: String, tier: EventTier) -> Self {
        Self {
            name,
            tier,
            render_count: 0,
            cascade_total: 0,
            last_seen_ms: 0,
            durations_us: VecDeque::with_capacity(LATENCY_RING_CAPACITY),
        }
    }

    fn record(&mut self, event: &RenderEvent) {
        self.render_count = self.render_count.saturating_add(1);
        let cascade_count = u64::try_from(event.cascade_children.len()).unwrap_or(u64::MAX);
        self.cascade_total = self.cascade_total.saturating_add(cascade_count);
        self.last_seen_ms = event.timestamp_ms;
        if self.durations_us.len() == LATENCY_RING_CAPACITY {
            self.durations_us.pop_front();
        }
        self.durations_us.push_back(event.duration_us);
    }

    fn p95_us(&self) -> u64 {
        if self.durations_us.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u64> = self.durations_us.iter().copied().collect();
        sorted.sort_unstable();
        let last = sorted.len().saturating_sub(1);
        let idx = last.saturating_mul(95).saturating_add(50) / 100;
        let idx = idx.min(last);
        sorted.get(idx).copied().unwrap_or(0)
    }
}

#[derive(Debug)]
struct AggregatorInner {
    components: HashMap<u64, ComponentCounter>,
    total_renders: u64,
    total_cascade: u64,
    trend: VecDeque<f32>,
    last_trend_ms: u64,
}

#[derive(Debug)]
pub struct InspectorState {
    aggregator: Mutex<AggregatorInner>,
    graph: RwLock<GraphSnapshot>,
    lane_utilization: Mutex<[f32; 4]>,
    sse_clients: Arc<Mutex<Vec<TcpStream>>>,
    assumed_alpha: f32,
    tier_map: RwLock<HashMap<String, Tier>>,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self::new(DEFAULT_ASSUMED_ALPHA)
    }
}

impl InspectorState {
    pub fn new(assumed_alpha: f32) -> Self {
        Self {
            aggregator: Mutex::new(AggregatorInner {
                components: HashMap::new(),
                total_renders: 0,
                total_cascade: 0,
                trend: VecDeque::with_capacity(TREND_RING_CAPACITY),
                last_trend_ms: 0,
            }),
            graph: RwLock::new(GraphSnapshot::empty()),
            lane_utilization: Mutex::new([0.0; 4]),
            sse_clients: Arc::new(Mutex::new(Vec::new())),
            assumed_alpha,
            tier_map: RwLock::new(HashMap::new()),
        }
    }

    pub fn set_graph(&self, snapshot: GraphSnapshot) {
        if let Ok(mut guard) = self.graph.write() {
            *guard = snapshot;
        }
    }

    pub fn set_tier_map(&self, map: HashMap<String, Tier>) {
        if let Ok(mut guard) = self.tier_map.write() {
            *guard = map;
        }
    }

    pub fn graph_snapshot(&self) -> GraphSnapshot {
        self.graph
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| GraphSnapshot::empty())
    }

    pub fn set_lane_utilization(&self, util: [f32; 4]) {
        if let Ok(mut guard) = self.lane_utilization.lock() {
            *guard = util;
        }
    }

    fn lane_utilization(&self) -> [f32; 4] {
        self.lane_utilization
            .lock()
            .map(|g| *g)
            .unwrap_or([0.0; 4])
    }

    fn tier_for(&self, name: &str) -> EventTier {
        self.tier_map
            .read()
            .ok()
            .and_then(|map| map.get(name).copied())
            .map_or(EventTier::B, EventTier::from)
    }

    fn record_event(&self, event: &RenderEvent) {
        if let Ok(mut guard) = self.aggregator.lock() {
            let entry = guard
                .components
                .entry(event.component_id)
                .or_insert_with(|| ComponentCounter::new(event.component_name.clone(), event.tier));
            entry.record(event);
            guard.total_renders = guard.total_renders.saturating_add(1);
            let cascade_count = u64::try_from(event.cascade_children.len()).unwrap_or(u64::MAX);
            guard.total_cascade = guard.total_cascade.saturating_add(cascade_count);

            // Sample collective albedo into the trend ring at most once per
            // ~250ms, regardless of event burst rate.
            if event.timestamp_ms.saturating_sub(guard.last_trend_ms) >= 250 {
                guard.last_trend_ms = event.timestamp_ms;
                let albedo = collective_albedo(guard.total_renders, guard.total_cascade);
                if guard.trend.len() == TREND_RING_CAPACITY {
                    guard.trend.pop_front();
                }
                guard.trend.push_back(albedo);
            }
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let Ok(guard) = self.aggregator.lock() else {
            return empty_metrics(self.assumed_alpha, self.lane_utilization());
        };

        let collective = collective_albedo(guard.total_renders, guard.total_cascade);
        let measured_alpha = if guard.total_renders == 0 {
            0.0
        } else {
            ratio_f32(guard.total_cascade, guard.total_renders)
        };
        let alpha_delta = (measured_alpha - self.assumed_alpha).abs();
        let alpha_warning =
            self.assumed_alpha > 0.0 && (alpha_delta / self.assumed_alpha) > ALPHA_DELTA_WARNING;

        let mut hottest: Vec<HotComponent> = guard
            .components
            .iter()
            .map(|(id, c)| HotComponent {
                id: *id,
                name: c.name.clone(),
                tier: c.tier,
                render_count: c.render_count,
            })
            .collect();
        hottest.sort_by_key(|c| std::cmp::Reverse(c.render_count));
        hottest.truncate(5);

        let mut slowest: Vec<SlowComponent> = guard
            .components
            .iter()
            .map(|(id, c)| SlowComponent {
                id: *id,
                name: c.name.clone(),
                tier: c.tier,
                p95_us: c.p95_us(),
                render_count: c.render_count,
            })
            .collect();
        slowest.sort_by_key(|c| std::cmp::Reverse(c.p95_us));
        slowest.truncate(5);

        let mut tier = TierBreakdown::default();
        for c in guard.components.values() {
            match c.tier {
                EventTier::A => tier.a = tier.a.saturating_add(c.render_count),
                EventTier::B => tier.b = tier.b.saturating_add(c.render_count),
                EventTier::C => tier.c = tier.c.saturating_add(c.render_count),
            }
        }

        MetricsSnapshot {
            collective_albedo: collective,
            cascade_ratio: measured_alpha,
            assumed_alpha: self.assumed_alpha,
            measured_alpha,
            alpha_delta,
            alpha_warning,
            total_renders: guard.total_renders,
            tracked_components: guard.components.len(),
            tier_breakdown: tier,
            hottest,
            slowest,
            trend: guard.trend.iter().copied().collect(),
            lane_utilization: self.lane_utilization(),
        }
    }
}

fn empty_metrics(assumed_alpha: f32, lane_utilization: [f32; 4]) -> MetricsSnapshot {
    MetricsSnapshot {
        collective_albedo: 1.0,
        cascade_ratio: 0.0,
        assumed_alpha,
        measured_alpha: 0.0,
        alpha_delta: 0.0,
        alpha_warning: false,
        total_renders: 0,
        tracked_components: 0,
        tier_breakdown: TierBreakdown::default(),
        hottest: Vec::new(),
        slowest: Vec::new(),
        trend: Vec::new(),
        lane_utilization,
    }
}

// ──────────────────── Observer / publisher bridge ────────────────────

#[derive(Clone)]
pub struct InspectorPublisher {
    state: Arc<InspectorState>,
}

impl InspectorPublisher {
    pub fn new(state: Arc<InspectorState>) -> Self {
        Self { state }
    }
}

impl RenderObserver for InspectorPublisher {
    fn on_render(&self, info: RenderInfo) {
        let event = RenderEvent {
            component_id: component_id(&info.component_name, &info.module_spec),
            component_name: info.component_name.clone(),
            tier: self.state.tier_for(&info.component_name),
            duration_us: info.duration_us,
            timestamp_ms: now_ms(),
            cascade_children: info
                .cascade_children
                .iter()
                .map(|name| component_id(name, ""))
                .collect(),
        };
        self.state.record_event(&event);
        broadcast_event(&self.state.sse_clients, &event);
    }
}

impl LaneObserver for InspectorPublisher {
    fn on_frame(&self, report: LaneFrameReport) {
        let total: u64 = report.lane_patches.iter().map(|c| u64::from(*c)).sum();
        if total == 0 {
            self.state.set_lane_utilization([0.0; 4]);
            return;
        }
        let mut util = [0.0_f32; 4];
        for (slot, count) in util.iter_mut().zip(report.lane_patches.iter()) {
            *slot = ratio_f32(u64::from(*count), total);
        }
        self.state.set_lane_utilization(util);
    }
}

// ────────────────────────── HTTP serving ──────────────────────────

/// True when the request path falls under the inspector's reserved prefix.
pub fn matches_path(path: &str) -> bool {
    path == "/__albedo" || path.starts_with("/__albedo/")
}

/// Result of an inspector dispatch — whether it took ownership of the
/// connection (for the SSE stream that lives forever) or returned a
/// one-shot response that the caller already wrote to the stream.
pub enum Dispatch {
    /// The handler wrote a complete response and the connection can close.
    Handled,
    /// The handler took ownership of the stream (SSE) and pushed it onto
    /// the inspector's event-client list. The caller must NOT touch the
    /// stream again.
    StreamOwned,
}

/// Sync handler invoked from `handle_dev_connection` for any path that
/// `matches_path` accepts. Writes the response to `stream` directly.
pub fn dispatch(
    state: &Arc<InspectorState>,
    path: &str,
    stream: &mut TcpStream,
) -> std::io::Result<Dispatch> {
    match path {
        "/__albedo" | "/__albedo/" => {
            write_simple(
                stream,
                200,
                "OK",
                "text/html; charset=utf-8",
                INSPECTOR_HTML.as_bytes(),
            )?;
            Ok(Dispatch::Handled)
        }
        "/__albedo/api/graph" => {
            let body = serde_json::to_vec(&state.graph_snapshot()).unwrap_or_default();
            write_json(stream, 200, "OK", &body)?;
            Ok(Dispatch::Handled)
        }
        "/__albedo/api/metrics" => {
            let body = serde_json::to_vec(&state.snapshot()).unwrap_or_default();
            write_json(stream, 200, "OK", &body)?;
            Ok(Dispatch::Handled)
        }
        "/__albedo/api/events" => {
            // Mirror the HMR pattern: write SSE handshake + push the stream
            // into the shared client list. The publisher writes events to
            // every retained stream; dead sockets are pruned on next push.
            write_inspector_sse_handshake(stream)?;
            // Need an owned TcpStream for the client list. Clone is fine here —
            // the caller relinquishes its handle after StreamOwned is returned.
            let owned = stream.try_clone()?;
            if let Ok(mut clients) = state.sse_clients.lock() {
                clients.push(owned);
            }
            Ok(Dispatch::StreamOwned)
        }
        _ => {
            write_simple(
                stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                b"inspector route not found\n",
            )?;
            Ok(Dispatch::Handled)
        }
    }
}

fn write_simple(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        ctype = content_type,
        len = body.len(),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_json(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write_simple(stream, status, reason, "application/json", body)
}

fn write_inspector_sse_handshake(stream: &mut TcpStream) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-store, must-revalidate\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\nx-albedo-transport: sse\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.write_all(b"event: ready\ndata: listening\n\n")?;
    stream.flush()
}

fn broadcast_event(clients: &Arc<Mutex<Vec<TcpStream>>>, event: &RenderEvent) {
    let json = match serde_json::to_string(event) {
        Ok(s) => s,
        Err(_) => return,
    };
    let payload = format!("event: render\ndata: {json}\n\n");
    let mut guard = match clients.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut retained = Vec::with_capacity(guard.len());
    for mut stream in guard.drain(..) {
        if stream
            .write_all(payload.as_bytes())
            .and_then(|_| stream.flush())
            .is_ok()
        {
            retained.push(stream);
        }
    }
    *guard = retained;
}

// ───────────────────────── Helpers ─────────────────────────

fn collective_albedo(total_renders: u64, total_cascade: u64) -> f32 {
    if total_renders == 0 {
        return 1.0;
    }
    let ratio = ratio_f32(total_cascade, total_renders);
    (1.0 - ratio.min(1.0)).clamp(0.0, 1.0)
}

fn ratio_f32(numer: u64, denom: u64) -> f32 {
    if denom == 0 {
        return 0.0;
    }
    // f64 divide for precision, narrow to f32 at the end. The compiler
    // crate denies `as_conversions`, so allow it locally for the explicit
    // numeric narrowings that this ratio absolutely needs.
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let n = numer as f64;
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let d = denom as f64;
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let result = (n / d) as f32;
    result
}

fn priority_to_albedo(priority: f64) -> f32 {
    let raw = 1.0 - (priority.clamp(0.0, 4.0) / 4.0) * 0.5;
    let raw = raw.clamp(0.2, 0.95);
    #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
    let v = raw as f32;
    v
}

fn component_id(name: &str, module_spec: &str) -> u64 {
    let mut buf = Vec::with_capacity(module_spec.len().saturating_add(1).saturating_add(name.len()));
    buf.extend_from_slice(module_spec.as_bytes());
    buf.push(b'\0');
    buf.extend_from_slice(name.as_bytes());
    xxh3_64(&buf)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Builds a `name → Tier` map from the manifest. Used by the publisher to
/// colour events with the right tier swatch.
pub fn tier_map_from_manifest(manifest: &RenderManifestV2) -> HashMap<String, Tier> {
    manifest
        .components
        .iter()
        .map(|c| (c.name.clone(), c.tier))
        .collect()
}
