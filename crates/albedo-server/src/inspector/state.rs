//! Long-lived state shared by every inspector route.

use super::events::RenderEvent;
use super::graph::GraphSnapshot;
use super::metrics::MetricsAggregator;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// Bounded broadcast capacity. SSE subscribers that lag behind have their
/// oldest events dropped on the floor — the inspector is not a system of
/// record, and a stalled browser tab must never back-pressure the renderer.
const EVENT_CHANNEL_CAPACITY: usize = 512;

#[derive(Debug)]
pub struct InspectorState {
    events_tx: broadcast::Sender<RenderEvent>,
    graph: RwLock<GraphSnapshot>,
    metrics: Arc<MetricsAggregator>,
    /// Per-lane fraction of the most recent runtime `frame_tick`'s patches.
    /// Updated by the runtime `LaneObserver`; surfaced in the metrics snapshot
    /// for the inspector's lane heatmap.
    lane_utilization: Mutex<[f32; 4]>,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self::new()
    }
}

impl InspectorState {
    pub fn new() -> Self {
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            events_tx,
            graph: RwLock::new(GraphSnapshot::empty()),
            metrics: Arc::new(MetricsAggregator::default()),
            lane_utilization: Mutex::new([0.0; 4]),
        }
    }

    /// Publishes an event to every connected subscriber and folds it into the
    /// metrics aggregator. Errors from the broadcast channel mean nobody is
    /// listening — that's the steady state when no inspector is open and is
    /// not worth surfacing.
    pub fn publish_event(&self, event: RenderEvent) {
        self.metrics.record(&event);
        let _ = self.events_tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RenderEvent> {
        self.events_tx.subscribe()
    }

    pub fn metrics(&self) -> &Arc<MetricsAggregator> {
        &self.metrics
    }

    pub fn graph_snapshot(&self) -> GraphSnapshot {
        self.graph
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| GraphSnapshot::empty())
    }

    pub fn set_graph(&self, snapshot: GraphSnapshot) {
        if let Ok(mut guard) = self.graph.write() {
            *guard = snapshot;
        }
    }

    /// Updates the rolling lane-utilization vector. The runtime `LaneObserver`
    /// writes this on every `frame_tick`; the inspector reads it once per
    /// `/api/metrics` poll. Lock contention is uninteresting — this is a 16-byte
    /// array and the polls are 2 s apart.
    pub fn set_lane_utilization(&self, utilization: [f32; 4]) {
        if let Ok(mut guard) = self.lane_utilization.lock() {
            *guard = utilization;
        }
    }

    pub fn lane_utilization(&self) -> [f32; 4] {
        self.lane_utilization
            .lock()
            .map(|guard| *guard)
            .unwrap_or([0.0; 4])
    }
}

pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
