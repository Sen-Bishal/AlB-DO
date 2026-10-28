//! Component-graph snapshot served at `GET /__albedo/api/graph`.
//!
//! Built from a `RenderManifestV2` so the inspector's view of the world tracks
//! whatever the compiler emitted on the last build. The snapshot is a pure
//! serializable struct — no live references — so the inspector can hand a
//! cheap clone to every API request.

use super::events::EventTier;
use dom_render_compiler::manifest::schema::{ComponentManifestEntry, RenderManifestV2};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentNode {
    pub id: u64,
    pub label: String,
    pub tier: EventTier,
    /// 0..1 — higher means the component reflects most of its render budget
    /// back without cascading. Initialized from manifest priority and updated
    /// from live metrics by the metrics aggregator.
    pub albedo: f32,
    pub weight_bytes: u64,
    pub module_path: String,
    pub can_defer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentEdge {
    pub from: u64,
    pub to: u64,
    pub multiplicity: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphSnapshot {
    pub nodes: Vec<ComponentNode>,
    pub edges: Vec<ComponentEdge>,
    pub generated_at_ms: u64,
    pub source: GraphSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GraphSource {
    #[default]
    Empty,
    Manifest,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn priority_to_albedo(priority: f64) -> f32 {
    // Higher priority components get more attention in the runtime, which we
    // map to a slightly lower albedo (more of the render budget is spent here
    // rather than reflected back). Clamp into [0.2, 0.95] so every node
    // visualizes as a reasonable disc.
    let raw = 1.0 - (priority.clamp(0.0, 4.0) / 4.0) * 0.5;
    raw.clamp(0.2, 0.95) as f32
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

    /// Builds a snapshot from the compiler manifest's component entries.
    /// Edges are derived from each component's `dependencies` list with
    /// multiplicity 1 — the compiler doesn't track render fan-out yet, so the
    /// inspector starts from the static dependency edges and lets the live
    /// event stream colour them as cascades fire.
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
