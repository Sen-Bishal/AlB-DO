//! Criterion benchmark locking the cycle-5 sub-ms reconcile claim into CI.
//!
//! `frame_tick_n1000_dirty100` runs the RAF tick against 1000 components
//! with 100 dirty slots per frame — the target working-set size from the
//! SoA plan. The bench measures the full `FourLaneRuntimePipeline::tick_frame`
//! path, including bitmap drain, lane partitioning, rayon fan-out, patch
//! emission, and WebTransport sequence allocation.
//!
//! Additional shapes surface throughput at 10% and 100% dirty ratios so a
//! regression in the partition or emit stage is caught even if the
//! headline bench stays within budget.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dom_render_compiler::graph::ComponentGraph;
use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
use dom_render_compiler::runtime::scheduler::SchedulerConfig;
use dom_render_compiler::types::{Component, ComponentAnalysis, ComponentId};
use std::collections::HashMap;

const TAU: f64 = std::f64::consts::TAU;

fn analysis(id: ComponentId, phase: f64) -> ComponentAnalysis {
    ComponentAnalysis {
        id,
        priority: 1.0,
        estimated_time_ms: 1.0,
        phase,
        topological_level: 0,
    }
}

fn build_pipeline(n: usize) -> FourLaneRuntimePipeline {
    let graph = ComponentGraph::new();
    let ids: Vec<ComponentId> = (0..n)
        .map(|i| {
            graph.add_component(Component::new(ComponentId::new(0), format!("C{i}")))
        })
        .collect();

    let mut analyses = HashMap::new();
    for (slot, id) in ids.iter().enumerate() {
        let phase = (slot as f64 * 0.37) % TAU;
        analyses.insert(*id, analysis(*id, phase));
    }
    let tiers: HashMap<_, _> = ids.iter().map(|id| (*id, Tier::B)).collect();

    FourLaneRuntimePipeline::new(
        &graph,
        analyses,
        tiers,
        &[],
        SchedulerConfig::default(),
        64,
    )
    .expect("pipeline build")
}

/// Deterministic strided dirty-set so every run marks the same slots and the
/// bench is reproducible across machines.
fn mark_strided(pipeline: &FourLaneRuntimePipeline, total: usize, dirty_count: usize) {
    if dirty_count == 0 || total == 0 {
        return;
    }
    let stride = (total / dirty_count).max(1);
    let mut slot = 0;
    for _ in 0..dirty_count {
        pipeline.mark_column_dirty(slot % total);
        slot += stride;
    }
}

fn bench_frame_tick(c: &mut Criterion) {
    // Headline bench: 1000 components × 100 dirty = 10% ratio, the plan's
    // nominal RAF working set. Sub-ms total_ns is the promise we measure.
    {
        let mut pipeline = build_pipeline(1000);
        c.bench_function("frame_tick_n1000_dirty100", |b| {
            b.iter(|| {
                mark_strided(&pipeline, 1000, 100);
                let report = pipeline.tick_frame();
                black_box(report);
            });
        });
    }

    // Stress shapes: full re-render at two scales so partition/emit cost
    // surfaces independently of dirty ratio.
    {
        let mut pipeline = build_pipeline(1000);
        c.bench_function("frame_tick_n1000_dirty1000", |b| {
            b.iter(|| {
                mark_strided(&pipeline, 1000, 1000);
                let report = pipeline.tick_frame();
                black_box(report);
            });
        });
    }
    {
        let mut pipeline = build_pipeline(256);
        c.bench_function("frame_tick_n256_dirty256", |b| {
            b.iter(|| {
                mark_strided(&pipeline, 256, 256);
                let report = pipeline.tick_frame();
                black_box(report);
            });
        });
    }
}

criterion_group!(benches, bench_frame_tick);
criterion_main!(benches);
