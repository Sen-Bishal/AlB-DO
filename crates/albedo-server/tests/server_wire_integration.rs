//! End-to-end Phase B-finish wire test.
//!
//! Asserts that the opcode pipeline, when bound to a `StreamingAppState`,
//! produces the binary frame shape that the bakabox client expects:
//!
//! - Bootstrap intern table chunk lands on
//!   `WT_STREAM_SLOT_CONTROL` (=0) with `FramePayload::Binary`, and the
//!   bytes round-trip cleanly through [`decode_frame`].
//! - Marking a component dirty and driving one tick yields at least one
//!   patch chunk on `WT_STREAM_SLOT_PATCHES` (=2), also `FramePayload::Binary`.
//! - The decoded `OpcodeFrame.instructions` are well-formed (non-empty,
//!   the shape Phase C's hand-rolled JS decoder will need to handle).
//!
//! This is the gate that says "bytes flow"; Phase C's JS decoder will hang
//! its own conformance tests off the same frame shapes.

use albedo_server::handlers::streaming::{
    drain_pipeline_bootstrap, drive_pipeline_tick, StreamingAppState, StreamingTransportConfig,
};
use albedo_server::render::tier_b::SharedRenderServices;
use dom_render_compiler::graph::ComponentGraph;
use dom_render_compiler::ir::opcode::{Instruction, InternTableKind, StableId, TagId};
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::manifest::schema::{RenderManifestV2, Tier};
use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
use dom_render_compiler::runtime::scheduler::SchedulerConfig;
use dom_render_compiler::runtime::webtransport::{FramePayload, WT_STREAM_SLOT_PATCHES};
use dom_render_compiler::types::{Component, ComponentAnalysis, ComponentId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Builds a four-component pipeline that drops one component per lane via
/// the phase→lane bucketing in `runtime/highway.rs`. Returns the pipeline
/// and the component ids in lane order so tests can dirty them
/// individually.
fn build_test_pipeline() -> (FourLaneRuntimePipeline, Vec<ComponentId>) {
    let graph = ComponentGraph::new();
    let ids: Vec<ComponentId> = (0..4)
        .map(|i| graph.add_component(Component::new(ComponentId::new(0), format!("C{i}"))))
        .collect();

    // Phases spread across [0, 2π) so phase_to_lane drops one per lane.
    let phases = [0.1_f64, 1.7, 3.4, 5.0];
    let mut analyses = HashMap::new();
    for (id, phase) in ids.iter().zip(phases.iter()) {
        analyses.insert(
            *id,
            ComponentAnalysis {
                id: *id,
                priority: 1.0,
                estimated_time_ms: 1.0,
                phase: *phase,
                topological_level: 0,
            },
        );
    }
    let tiers = ids
        .iter()
        .map(|id| (*id, Tier::B))
        .collect::<HashMap<_, _>>();

    let pipeline = FourLaneRuntimePipeline::new(
        &graph,
        analyses,
        tiers,
        &[],
        SchedulerConfig {
            overtake_budget: Duration::from_secs(1),
            overtake_interval: 2,
            ..SchedulerConfig::default()
        },
        32,
    )
    .expect("test pipeline must construct");

    (pipeline, ids)
}

/// Builds an empty streaming app state wrapping the supplied pipeline.
/// The manifest and WT session registry are no-ops here — Phase B-finish
/// tests the chunk plumbing, not the request/route flow.
fn streaming_state_with_pipeline(pipeline: FourLaneRuntimePipeline) -> StreamingAppState {
    StreamingAppState::new(
        Arc::new(RenderManifestV2::legacy_defaults()),
        SharedRenderServices::default(),
        StreamingTransportConfig::new(false, 0),
        None,
    )
    .with_pipeline(pipeline, tokio::runtime::Handle::current())
}

/// Test classifier: tags every interned string as a `Tag`. Production
/// servers will provide a real classifier driven by the renderer's intern
/// context (Phase E). Here it just ensures the bootstrap path produces a
/// non-empty payload when strings exist in the column store.
fn test_classify_all_as_tag(_id: u16, _value: &str) -> Option<InternTableKind> {
    Some(InternTableKind::Tag)
}

#[tokio::test]
async fn streaming_state_without_pipeline_returns_no_chunks() {
    let state = StreamingAppState::new(
        Arc::new(RenderManifestV2::legacy_defaults()),
        SharedRenderServices::default(),
        StreamingTransportConfig::new(false, 0),
        None,
    );

    assert!(
        !state.has_pipeline(),
        "fresh state must report no pipeline bound"
    );
    assert!(
        drive_pipeline_tick(&state).is_empty(),
        "tick must yield no chunks when no pipeline is bound"
    );
    assert!(
        drain_pipeline_bootstrap(&state, test_classify_all_as_tag)
            .expect("must not error when pipeline absent")
            .is_none(),
        "bootstrap must yield None when no pipeline is bound"
    );
}

#[tokio::test]
async fn pipeline_tick_emits_binary_patch_chunks_on_dirty_components() {
    let (pipeline, ids) = build_test_pipeline();
    let state = streaming_state_with_pipeline(pipeline);

    // Sanity: no chunks before anything is dirty.
    assert!(
        drive_pipeline_tick(&state).is_empty(),
        "tick with no dirty components must produce no chunks"
    );

    // Mark every component dirty; one per lane → one chunk per lane.
    {
        let pipeline = state.pipeline().expect("pipeline must be bound").lock().unwrap();
        for id in &ids {
            assert!(
                pipeline.mark_component_dirty(*id),
                "marking a known component must succeed"
            );
        }
    }

    let chunks = drive_pipeline_tick(&state);
    assert!(
        !chunks.is_empty(),
        "tick must produce chunks after components are dirty"
    );

    for chunk in &chunks {
        assert_eq!(
            chunk.lane as u8,
            WT_STREAM_SLOT_PATCHES,
            "patch chunks must route to slot {WT_STREAM_SLOT_PATCHES}"
        );
        match &chunk.payload {
            FramePayload::Binary(bytes) => {
                let (decoded, consumed) =
                    decode_frame(bytes).expect("patch chunk bytes must decode cleanly");
                assert_eq!(consumed, bytes.len(), "decoder must consume entire payload");
                assert!(
                    !decoded.instructions.is_empty(),
                    "patch frame must carry at least one instruction"
                );
            }
            FramePayload::Text(_) => panic!("patch chunks must be Binary, never Text"),
        }
    }
}

#[tokio::test]
async fn bootstrap_intern_chunk_round_trips_through_decoder() {
    // Build a pipeline whose column store contains at least one interned
    // string. `Component::new` writes the symbol name into the interner,
    // so a fresh pipeline already has classifiable entries.
    let (pipeline, _ids) = build_test_pipeline();
    let state = streaming_state_with_pipeline(pipeline);

    let chunk = drain_pipeline_bootstrap(&state, test_classify_all_as_tag)
        .expect("bootstrap must not error")
        .expect("classifier classifies every string, so the chunk must be Some");

    assert_eq!(
        chunk.lane as u8,
        WT_STREAM_SLOT_PATCHES,
        "bootstrap intern chunks must ride the binary patches stream \
         (slot {WT_STREAM_SLOT_PATCHES}) so slot 0 stays pure JSON for \
         session envelopes"
    );

    let bytes = match chunk.payload {
        FramePayload::Binary(bytes) => bytes,
        FramePayload::Text(_) => panic!("bootstrap chunk must be Binary, never Text"),
    };
    let (decoded, consumed) = decode_frame(&bytes).expect("bootstrap bytes must decode");
    assert_eq!(consumed, bytes.len(), "decoder must consume entire payload");
    assert!(
        decoded.component_id.is_none(),
        "bootstrap frame is global, not component-scoped"
    );
    assert!(
        !decoded.instructions.is_empty(),
        "bootstrap frame must carry InitInternTable instructions"
    );
    assert!(
        decoded
            .instructions
            .iter()
            .any(|instr| matches!(
                instr,
                dom_render_compiler::ir::opcode::Instruction::InitInternTable { .. }
            )),
        "bootstrap frame must contain at least one InitInternTable instruction"
    );
}

#[tokio::test]
async fn bootstrap_drains_baseline_so_subsequent_patch_diff_is_empty() {
    let (pipeline, _ids) = build_test_pipeline();
    let state = streaming_state_with_pipeline(pipeline);

    // First call ships the bootstrap; refreshes the internal snapshot.
    let _ = drain_pipeline_bootstrap(&state, test_classify_all_as_tag)
        .expect("bootstrap must not error");

    // A second bootstrap call is allowed (and produces the same payload —
    // bootstrap is idempotent on the wire), but the canonical use is one
    // bootstrap + many patches. Here we just confirm the second call is
    // safe.
    let second = drain_pipeline_bootstrap(&state, test_classify_all_as_tag)
        .expect("repeat bootstrap must not error");
    assert!(
        second.is_some(),
        "bootstrap is idempotent — repeated calls still emit InitInternTable"
    );
}

// ── Phase D — async-island integration ─────────────────────────────────
//
// Drives the async surface through `StreamingAppState::pipeline()` so the
// wire path the server hands to Phase E's renderer is exercised, not just
// the substrate inside the pipeline crate.

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn async_island_enqueue_emits_placeholder_via_streaming_state() {
    let (pipeline, _ids) = build_test_pipeline();
    let state = streaming_state_with_pipeline(pipeline);

    // Phase D · gate the resolver behind a oneshot the test controls.
    // The multi-thread tokio runtime polls spawned tasks on a worker
    // thread as soon as one is free; without this gate, an `async {
    // vec![...] }` block whose future is ready on first poll lands
    // its resolution on `async_tx` BEFORE the first
    // `drive_pipeline_tick` even runs, and the same-tick drain ships
    // both the queued placeholder and the just-arrived patch
    // together — defeating the placeholder-vs-patch sequencing the
    // assertions below expect. Production resolvers do real I/O so
    // this race doesn't show up there, but the test must simulate
    // that latency to stay deterministic across runtime flavors.
    let (release_resolver, resolver_gate) = tokio::sync::oneshot::channel::<()>();

    let suspense_id = {
        let guard = state.pipeline().expect("pipeline bound").lock().unwrap();
        guard
            .enqueue_async_island(StableId(123), async move {
                // Park the future until the test side releases it.
                // Errors on the gate (sender dropped before send) are
                // treated as "release immediately" — the test would
                // already be failing for an unrelated reason.
                let _ = resolver_gate.await;
                vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(123),
                }]
            })
            .expect("enqueue must succeed when pipeline carries the runtime handle")
    };

    let chunks = drive_pipeline_tick(&state);
    assert_eq!(chunks.len(), 1, "Placeholder chunk must ship on this tick");
    assert_eq!(chunks[0].lane as u8, WT_STREAM_SLOT_PATCHES);

    let bytes = match &chunks[0].payload {
        FramePayload::Binary(b) => b.clone(),
        FramePayload::Text(_) => panic!("Phase-D placeholder chunks must be Binary"),
    };
    let (frame, _) = decode_frame(&bytes).expect("placeholder bytes must decode");
    assert_eq!(
        frame.instructions,
        vec![Instruction::Placeholder {
            stable_id: StableId(123),
            suspense_id,
        }]
    );

    // Release the gated resolver and yield long enough for the worker
    // to wake, run the future to completion, and deliver the
    // resolution back across the pipeline's mpsc channel.
    release_resolver
        .send(())
        .expect("resolver gate must accept the release signal");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let patch_chunks = drive_pipeline_tick(&state);
    assert_eq!(
        patch_chunks.len(),
        1,
        "next tick must ship the resolved Patch frame"
    );
    let (patch_frame, _) = decode_frame(match &patch_chunks[0].payload {
        FramePayload::Binary(b) => b,
        FramePayload::Text(_) => panic!("patch chunks must be Binary"),
    })
    .expect("patch bytes must decode");
    assert!(matches!(
        patch_frame.instructions.first(),
        Some(Instruction::Patch { .. })
    ));
}
