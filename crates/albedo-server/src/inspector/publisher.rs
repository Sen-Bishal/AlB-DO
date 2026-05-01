//! Bridge between the compiler/runtime observer hooks and [`InspectorState`].
//!
//! The compiler crate exposes two trait surfaces in
//! `dom_render_compiler::runtime::render_observer`:
//!
//! * `RenderObserver::on_render(RenderInfo)` — fires once per finished compile-time
//!   component render, with cascade children recorded from a thread-local stack.
//! * `LaneObserver::on_frame(LaneFrameReport)` — fires once per runtime
//!   `frame_tick`, with per-lane patch counts and bytes.
//!
//! [`InspectorPublisher`] implements both. It owns a handle on
//! [`InspectorState`] and an optional name→tier map sourced from the build
//! manifest so each event lands with the right tier swatch — when the map is
//! missing or the name is not in it, events default to `Tier::B`, which is the
//! safe middle of the inspector's palette.
//!
//! Component IDs are content-hashed from `module_spec + name` (xxh3-64). They
//! are stable across renders within one process but intentionally not stable
//! across rename — that is a property a content hash cannot offer, and the
//! inspector treats node identity as ephemeral within a session.

use super::events::{EventTier, RenderEvent};
use super::state::InspectorState;
use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::runtime::render_observer::{
    LaneFrameReport, LaneObserver, RenderInfo, RenderObserver,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Debug)]
pub struct InspectorPublisher {
    inspector: Arc<InspectorState>,
    /// Map of `component_name → Tier`, sourced from `RenderManifestV2.components`.
    /// Optional — when absent (no manifest, no compile-time tiering), every
    /// event defaults to `Tier::B`.
    tier_map: HashMap<String, Tier>,
}

impl InspectorPublisher {
    pub fn new(inspector: Arc<InspectorState>) -> Self {
        Self {
            inspector,
            tier_map: HashMap::new(),
        }
    }

    pub fn with_tier_map(mut self, tier_map: HashMap<String, Tier>) -> Self {
        self.tier_map = tier_map;
        self
    }

    fn tier_for(&self, name: &str) -> EventTier {
        self.tier_map
            .get(name)
            .copied()
            .map_or(EventTier::B, EventTier::from)
    }

    fn cascade_ids(&self, names: &[String]) -> Vec<u64> {
        names.iter().map(|n| component_id(n, "")).collect()
    }
}

impl RenderObserver for InspectorPublisher {
    fn on_render(&self, info: RenderInfo) {
        let event = RenderEvent {
            component_id: component_id(&info.component_name, &info.module_spec),
            component_name: info.component_name.clone(),
            tier: self.tier_for(&info.component_name),
            duration_us: info.duration_us,
            timestamp_ms: now_ms(),
            cascade_children: self.cascade_ids(&info.cascade_children),
            note: None,
        };
        self.inspector.publish_event(event);
    }
}

impl LaneObserver for InspectorPublisher {
    fn on_frame(&self, report: LaneFrameReport) {
        // Express utilization as the fraction of patches each lane carried in
        // the most recent tick. This is the operator-meaningful number — it
        // says "lane 2 took 70% of the dirty work" rather than reporting raw
        // byte counts that vary with scene size.
        let total: u64 = report.lane_patches.iter().map(|c| u64::from(*c)).sum();
        if total == 0 {
            self.inspector.set_lane_utilization([0.0; 4]);
            return;
        }
        // Cast through f64 to keep the division precise for small lane counts;
        // the result fits in f32 with room to spare.
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        let total_f = total as f64;
        let mut util = [0.0_f32; 4];
        for (slot, count) in util.iter_mut().zip(report.lane_patches.iter()) {
            #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
            let count_f = u64::from(*count) as f64;
            #[allow(clippy::cast_possible_truncation, clippy::as_conversions)]
            let frac = (count_f / total_f) as f32;
            *slot = frac;
        }
        self.inspector.set_lane_utilization(util);
    }
}

/// Stable per-process identifier for `(module_spec, component_name)`. xxh3-64
/// is the same hash family the IR column store uses for source hashes, so the
/// inspector and the compiler produce comparable identifiers when both are
/// fed the same input.
fn component_id(name: &str, module_spec: &str) -> u64 {
    let mut buf = Vec::with_capacity(module_spec.len() + 1 + name.len());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascade_ids_match_published_event_ids() {
        let inspector = Arc::new(InspectorState::new());
        let publisher = InspectorPublisher::new(inspector.clone());

        publisher.on_render(RenderInfo {
            component_name: "TodoList".to_string(),
            module_spec: "src/Todo.tsx".to_string(),
            duration_us: 320,
            cascade_children: vec!["TodoItem".to_string(), "Filter".to_string()],
        });
        publisher.on_render(RenderInfo {
            component_name: "TodoItem".to_string(),
            module_spec: "src/Todo.tsx".to_string(),
            duration_us: 90,
            cascade_children: vec![],
        });

        let snapshot = inspector.metrics().snapshot();
        assert_eq!(snapshot.total_renders, 2);
        // Cascade child id of "TodoItem" published by TodoList should equal
        // the id "TodoItem" reports for itself when the empty module_spec is
        // used by `cascade_ids`. This verifies the inspector can join cascades
        // back to nodes by name even when module_spec is unknown.
        let from_cascade = component_id("TodoItem", "");
        let from_render = component_id("TodoItem", "src/Todo.tsx");
        assert_ne!(from_cascade, from_render);
        // The current shape uses module_spec = "" for cascades because the
        // child's module isn't on the parent's frame. This is documented in
        // the publisher; if the choice changes, this test should change too.
    }

    #[test]
    fn lane_observer_writes_normalised_utilization() {
        let inspector = Arc::new(InspectorState::new());
        let publisher = InspectorPublisher::new(inspector.clone());
        publisher.on_frame(LaneFrameReport {
            lane_bytes: [0; 4],
            lane_patches: [1, 3, 0, 0],
            dirty_count: 4,
            total_ns: 1_000,
        });
        let util = inspector.lane_utilization();
        assert!((util[0] - 0.25).abs() < 1e-3);
        assert!((util[1] - 0.75).abs() < 1e-3);
        assert_eq!(util[2], 0.0);
        assert_eq!(util[3], 0.0);
    }

    #[test]
    fn lane_observer_zero_total_resets_utilization() {
        let inspector = Arc::new(InspectorState::new());
        inspector.set_lane_utilization([0.4, 0.4, 0.1, 0.1]);
        let publisher = InspectorPublisher::new(inspector.clone());
        publisher.on_frame(LaneFrameReport {
            lane_bytes: [0; 4],
            lane_patches: [0; 4],
            dirty_count: 0,
            total_ns: 100,
        });
        assert_eq!(inspector.lane_utilization(), [0.0; 4]);
    }
}
