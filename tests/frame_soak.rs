//! 1M-frame soak on the cycle-5 reconciliation pipeline.
//!
//! Drives the pipeline's owned [`DirtyBitmap`] under a randomized dirty
//! ratio and asserts the [`FrameArena`] never grows its backing storage —
//! the authoritative leak-detection on the SoA reconcile substrate.
//!
//! `#[ignore]`'d by default because a million ticks takes several seconds
//! in release and is not appropriate for the default CI gate. Run with:
//!
//! ```text
//! cargo test --release --test frame_soak -- --ignored --nocapture
//! ```

use dom_render_compiler::graph::ComponentGraph;
use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
use dom_render_compiler::runtime::scheduler::SchedulerConfig;
use dom_render_compiler::types::{Component, ComponentAnalysis, ComponentId};
use std::collections::HashMap;

const COMPONENT_COUNT: usize = 256;
const FRAME_COUNT: usize = 1_000_000;
const DIRTY_BASIS: u64 = 100;
const TAU: f64 = std::f64::consts::TAU;

/// Deterministic 64-bit Lehmer PRNG. Chosen for the soak because it has
/// zero dependencies, no setup cost, and a well-understood 2^63 period —
/// more than enough for 1e6 ticks × 256 slots.
struct Lehmer64(u64);
impl Lehmer64 {
    fn next(&mut self) -> u64 {
        let product = u128::from(self.0).wrapping_mul(0xDA94_2042_E4DD_58B5);
        let high = (product >> 64) as u64;
        let low = product as u64;
        self.0 = high ^ low;
        self.0
    }
}

fn analysis(id: ComponentId, phase: f64) -> ComponentAnalysis {
    ComponentAnalysis {
        id,
        priority: 1.0,
        estimated_time_ms: 1.0,
        phase,
        topological_level: 0,
    }
}

fn build_pipeline() -> (FourLaneRuntimePipeline, usize) {
    let graph = ComponentGraph::new();
    let ids: Vec<ComponentId> = (0..COMPONENT_COUNT)
        .map(|i| {
            graph.add_component(Component::new(ComponentId::new(0), format!("C{i}")))
        })
        .collect();

    let mut analyses = HashMap::new();
    for (slot, id) in ids.iter().enumerate() {
        let phase = (slot as f64 * 0.31) % TAU;
        analyses.insert(*id, analysis(*id, phase));
    }
    let tiers: HashMap<_, _> = ids.iter().map(|id| (*id, Tier::B)).collect();

    let pipeline = FourLaneRuntimePipeline::new(
        &graph,
        analyses,
        tiers,
        &[],
        SchedulerConfig::default(),
        64,
    )
    .expect("pipeline construction");

    let len = pipeline.columns().len();
    (pipeline, len)
}

#[test]
#[ignore = "1M-frame soak — run with --ignored --release"]
fn pipeline_tick_frame_soak_1m_frames_no_growth() {
    let (mut pipeline, len) = build_pipeline();
    assert_eq!(len, COMPONENT_COUNT);

    // Prime the arena at worst-case dirty-set so subsequent growth can only
    // come from a leak, not amortized doubling during warmup.
    for slot in 0..len {
        pipeline.mark_column_dirty(slot);
    }
    let _ = pipeline.tick_frame();

    let baseline_scratch = pipeline.frame_arena().scratch_capacity();

    let mut rng = Lehmer64(0xDEAD_BEEF_1234_5678);
    for _ in 0..FRAME_COUNT {
        for slot in 0..len {
            if rng.next() % DIRTY_BASIS == 0 {
                pipeline.mark_column_dirty(slot);
            }
        }
        let _ = pipeline.tick_frame();
    }

    assert_eq!(
        pipeline.frame_arena().scratch_capacity(),
        baseline_scratch,
        "scratch capacity must not grow across 1M ticks"
    );
    let metrics = pipeline.frame_metrics();
    assert_eq!(metrics.total_ticks(), (FRAME_COUNT + 1) as u64);
    assert!(metrics.p99_ns() > 0, "percentile should observe at least one sample");
    assert!(metrics.p99_ns() >= metrics.p50_ns());
}

#[test]
fn pipeline_tick_frame_soak_smoke_10k_frames() {
    let (mut pipeline, len) = build_pipeline();
    for slot in 0..len {
        pipeline.mark_column_dirty(slot);
    }
    let _ = pipeline.tick_frame();
    let baseline = pipeline.frame_arena().scratch_capacity();

    let mut rng = Lehmer64(0xC0FFEE_u64);
    for _ in 0..10_000 {
        for slot in 0..len {
            if rng.next() % DIRTY_BASIS == 0 {
                pipeline.mark_column_dirty(slot);
            }
        }
        let _ = pipeline.tick_frame();
    }

    assert_eq!(pipeline.frame_arena().scratch_capacity(), baseline);
    assert_eq!(pipeline.frame_metrics().total_ticks(), 10_001);
}
