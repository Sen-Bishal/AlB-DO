//! Phase P · Stream G — Action dispatch round-trip latency.
//!
//! Measures the **in-process latency of dispatching a TS-side
//! `action()` declaration**: decode the bincode `ActionEnvelope`,
//! resolve the handler in `CompiledProject`, invoke through
//! `invoke_action_with_broadcast`, encode the resulting `OpcodeFrame`.
//!
//! This is the "framework cost" of an action — the wire-level
//! overhead bakabox pays for every click that hits a server handler.
//! HTTP framing (header parsing, axum routing) sits OUTSIDE this
//! measurement; that's I/O and not Phase P's invariant. What this
//! bench pins is the *interpreter + slot store + broadcast* path
//! that the framework owns.
//!
//! Reference: Next.js Server Actions ship through `app/`'s route
//! handler which includes the framework's Node.js boot cost, JSON
//! serialization, and React's per-render reconciliation. Even on a
//! warm process, the in-process path typically lands in low-ms.
//! ALBEDO's path is bincode + interpreter on the same thread — the
//! numbers below tell you where it ends up.
//!
//! Reproduce with:
//!   cargo bench --bench parity_action_roundtrip

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
use dom_render_compiler::ir::opcode::OpcodeFrame;
use dom_render_compiler::ir::wire::encode_frame;
use dom_render_compiler::runtime::eval::{CompiledProject, SessionSlotView};
use dom_render_compiler::runtime::slot_store::SlotStore;
use dom_render_compiler::runtime::{BroadcastRegistry, SessionId};
use dom_render_compiler::transforms::allocate_form_action_id;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ts_action")
        .join("broadcast_demo")
}

fn build_session(
    project: &CompiledProject,
) -> (Arc<BroadcastRegistry>, SessionSlotView, ActionEnvelope, Vec<u8>) {
    let broadcast = Arc::new(BroadcastRegistry::new());
    // Pre-register the topic so the action's `broadcast()` call
    // finds a live entry on first dispatch (mirrors Stream C.3's
    // auto-registration).
    for topic in project.shared_slot_topics() {
        broadcast.topic(topic, b"null".to_vec());
    }
    broadcast.topic(
        "counter",
        serde_json::to_vec(&0).expect("seed counter as 0"),
    );

    let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    let envelope = ActionEnvelope {
        action_id: allocate_form_action_id("increment_counter"),
        event_kind: 0,
        payload: Vec::new(),
    };
    let encoded_envelope = encode_action_envelope(&envelope).expect("encode envelope");
    (broadcast, slots, envelope, encoded_envelope)
}

fn bench_action_roundtrip(c: &mut Criterion) {
    let project = CompiledProject::load_from_dir(fixture_root())
        .expect("load broadcast_demo fixture");
    let (broadcast, slots, _envelope, encoded_envelope) = build_session(&project);

    // The full round-trip mirrors what bakabox / the HTTP handler
    // do: decode envelope bytes, look up handler, invoke with
    // broadcast scope installed, encode result as an OpcodeFrame.
    c.bench_function("action_dispatch_round_trip", |b| {
        b.iter(|| {
            let (decoded, _) =
                dom_render_compiler::ir::action::decode_action_envelope(&encoded_envelope)
                    .expect("decode envelope");
            let instructions = project
                .invoke_action_with_broadcast(&decoded, &slots, broadcast.as_ref())
                .expect("invoke action");
            let frame = OpcodeFrame {
                frame_id: 0,
                component_id: None,
                instructions,
            };
            let encoded = encode_frame(&frame).expect("encode response frame");
            black_box(encoded);
        });
    });

    // Plain invoke_action_with_broadcast (skipping envelope decode +
    // response encode) — isolates the interpreter cost so you can
    // see what the wire framing adds.
    c.bench_function("action_invoke_interpreter_only", |b| {
        let (decoded, _) =
            dom_render_compiler::ir::action::decode_action_envelope(&encoded_envelope)
                .expect("decode envelope");
        b.iter(|| {
            let instructions = project
                .invoke_action_with_broadcast(&decoded, &slots, broadcast.as_ref())
                .expect("invoke action");
            black_box(instructions);
        });
    });
}

criterion_group!(benches, bench_action_roundtrip);
criterion_main!(benches);
