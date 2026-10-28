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
//! [`InspectorState`] and, from the build manifest, each component's graph
//! node id and tier — so a render lights up the node the graph drew for it,
//! in the right swatch. A component the manifest does not name gets a
//! content-hashed id (`module_spec + name`, xxh3-64) and `Tier::B`, the safe
//! middle of the inspector's palette.
//!
//! The evaluator's observer slots are process-wide and set once; [`attach`]
//! installs a forwarder into them the first time an inspector is mounted, and
//! points it at the newest publisher every time after.

use super::events::{EventTier, RenderEvent};
use super::state::InspectorState;
use dom_render_compiler::manifest::schema::RenderManifestV2;
use dom_render_compiler::runtime::render_observer::{
    install_lane_observer, install_render_observer, LaneFrameReport, LaneObserver, RenderInfo,
    RenderObserver,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Once, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use xxhash_rust::xxh3::xxh3_64;

#[derive(Debug)]
pub struct InspectorPublisher {
    inspector: Arc<InspectorState>,
    /// `component_name → (graph node id, tier)` from the build manifest. A name
    /// two components share is left out: it cannot say which node rendered.
    known: HashMap<String, (u64, EventTier)>,
}

impl InspectorPublisher {
    pub fn new(inspector: Arc<InspectorState>) -> Self {
        Self {
            inspector,
            known: HashMap::new(),
        }
    }

    /// Resolve component names against `manifest`, the same components
    /// [`super::GraphSnapshot::from_manifest`] draws.
    #[must_use]
    pub fn with_manifest(mut self, manifest: &RenderManifestV2) -> Self {
        let mut known = HashMap::with_capacity(manifest.components.len());
        let mut shared = HashSet::new();
        for entry in &manifest.components {
            let identity = (entry.id, EventTier::from(entry.tier));
            if known.insert(entry.name.clone(), identity).is_some() {
                shared.insert(entry.name.as_str());
            }
        }
        for name in shared {
            known.remove(name);
        }
        self.known = known;
        self
    }

    fn identify(&self, name: &str, module_spec: &str) -> (u64, EventTier) {
        self.known
            .get(name)
            .copied()
            .unwrap_or_else(|| (component_id(name, module_spec), EventTier::B))
    }

    fn cascade_ids(&self, names: &[String]) -> Vec<u64> {
        names.iter().map(|name| self.identify(name, "").0).collect()
    }
}

/// The publisher feeding the inspector mounted now.
///
/// `albedo dev` keeps one inspector for the life of the process but rebuilds
/// the manifest on every save, so the publisher is replaced each time and the
/// forwarder always reads the current one.
static CURRENT: RwLock<Option<Arc<InspectorPublisher>>> = RwLock::new(None);

struct Forwarder;

fn current() -> Option<Arc<InspectorPublisher>> {
    CURRENT.read().ok().and_then(|slot| slot.clone())
}

impl RenderObserver for Forwarder {
    fn on_render(&self, info: RenderInfo) {
        if let Some(publisher) = current() {
            publisher.on_render(info);
        }
    }
}

impl LaneObserver for Forwarder {
    fn on_frame(&self, report: LaneFrameReport) {
        if let Some(publisher) = current() {
            publisher.on_frame(report);
        }
    }
}

/// Route the evaluator's render and lane events to `publisher`'s inspector,
/// replacing whichever publisher had them. A process that never mounts an
/// inspector never calls this, and its render path finds no observer at all.
pub fn attach(publisher: InspectorPublisher) {
    if let Ok(mut slot) = CURRENT.write() {
        *slot = Some(Arc::new(publisher));
    }
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let forwarder = Arc::new(Forwarder);
        // Refused only if something else took the slot first; the inspector
        // then shows no live events rather than someone else's.
        let _ = install_render_observer(forwarder.clone());
        let _ = install_lane_observer(forwarder);
    });
}

impl RenderObserver for InspectorPublisher {
    fn on_render(&self, info: RenderInfo) {
        let (component_id, tier) = self.identify(&info.component_name, &info.module_spec);
        let event = RenderEvent {
            component_id,
            component_name: info.component_name.clone(),
            tier,
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

    use dom_render_compiler::manifest::schema::{ComponentManifestEntry, HydrationMode, Tier};

    fn manifest(components: &[(u64, &str, Tier)]) -> RenderManifestV2 {
        let mut manifest = RenderManifestV2::legacy_defaults();
        manifest.components = components
            .iter()
            .map(|&(id, name, tier)| ComponentManifestEntry {
                id,
                name: name.to_string(),
                module_path: format!("src/{name}.tsx"),
                tier,
                weight_bytes: 0,
                priority: 0.0,
                dependencies: Vec::new(),
                can_defer: false,
                hydration_mode: HydrationMode::None,
            })
            .collect();
        manifest
    }

    /// A render, and a cascade naming it, both land on the node the graph drew
    /// from the manifest — otherwise the graph never lights up.
    #[test]
    fn events_carry_the_graph_node_id_from_the_manifest() {
        let inspector = Arc::new(InspectorState::new());
        let publisher = InspectorPublisher::new(inspector.clone()).with_manifest(&manifest(&[
            (11, "List", Tier::B),
            (12, "Item", Tier::C),
        ]));
        let mut events = inspector.subscribe();

        publisher.on_render(RenderInfo {
            component_name: "List".to_string(),
            module_spec: "src/List.tsx".to_string(),
            duration_us: 320,
            cascade_children: vec!["Item".to_string(), "Unlisted".to_string()],
        });
        let event = events.try_recv().expect("the render was published");
        assert_eq!((event.component_id, event.tier), (11, EventTier::B));
        assert_eq!(event.cascade_children[0], 12, "a cascade names the child's node");
        assert_eq!(
            event.cascade_children[1],
            component_id("Unlisted", ""),
            "a component the manifest does not name still gets a stable id"
        );
        assert_eq!(inspector.metrics().snapshot().total_renders, 1);
    }

    /// Two components sharing a name cannot say which of them rendered, so
    /// neither borrows a node id.
    #[test]
    fn a_shared_name_resolves_to_no_node() {
        let inspector = Arc::new(InspectorState::new());
        let publisher = InspectorPublisher::new(inspector).with_manifest(&manifest(&[
            (1, "Button", Tier::A),
            (2, "Button", Tier::C),
            (3, "Card", Tier::A),
        ]));
        assert_eq!(publisher.identify("Card", "x"), (3, EventTier::A));
        let (id, tier) = publisher.identify("Button", "src/a.tsx");
        assert!(id != 1 && id != 2, "an ambiguous name must not claim a node");
        assert_eq!(tier, EventTier::B);
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
