//! Phase O.2 · `useSharedSlot` golden — the end-to-end correctness
//! gate for shared-state rendering.
//!
//! Each test compiles the `lobby` fixture (a single component reading
//! `useSharedSlot("chat:lobby")`), seeds the broadcast registry with a
//! value, renders the component, and asserts the wire-shape contract:
//!
//!   1. The rendered HTML inlines the topic's current value.
//!   2. The opcode stream carries one `SetTextRef` whose slot id
//!      equals `broadcast_slot_id("chat:lobby")` — i.e. the same id
//!      future `write_topic` calls fan out as `SlotSet` opcodes.
//!   3. The session is auto-subscribed; a follow-up `write_topic`
//!      delivers a SlotSet to the session's mpsc receiver.
//!   4. The initial-state opcodes prepended by
//!      `render_entry_with_broadcast` carry the topic's current value
//!      so the bakabox dispatch order (`SlotSet` first, then
//!      `SetTextRef`) leaves the DOM coherent.
//!
//! Bakabox is a dumb client — it can't tell a per-session SlotSet
//! from a broadcast one. This test verifies that fact at the wire
//! level: the same opcodes flow through the same machinery, just
//! authored server-side via a different store.

use dom_render_compiler::ir::opcode::{Instruction, SlotId};
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::eval::{CompiledProject, RenderOptions, SessionSlotView};
use dom_render_compiler::runtime::slot_store::SlotStore;
use dom_render_compiler::runtime::{
    broadcast_slot_id, render_entry_with_broadcast, BroadcastRegistry, SessionId,
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

const TOPIC: &str = "chat:lobby";
const CHANNEL_CAPACITY: usize = 16;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("shared_slot")
        .join("lobby")
}

fn compile() -> CompiledProject {
    CompiledProject::load_from_dir(fixture()).expect("project compiles")
}

fn fresh_session() -> (SessionSlotView, SessionId, Arc<SlotStore>) {
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    (SessionSlotView::new(session, store.clone()), session, store)
}

fn extract_slot_set_value(payload: &[u8]) -> (SlotId, Vec<u8>) {
    let (frame, _) = decode_frame(payload).expect("decode frame");
    assert_eq!(frame.instructions.len(), 1, "broadcast frame must carry one SlotSet");
    match frame.instructions.into_iter().next().expect("non-empty") {
        Instruction::SlotSet { slot_id, value } => (slot_id, value),
        other => panic!("expected SlotSet, got {other:?}"),
    }
}

#[tokio::test]
async fn use_shared_slot_renders_current_topic_value_and_emits_set_text_ref() {
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    let seed = serde_json::to_vec(&serde_json::json!(["alice: hi"])).unwrap();
    broadcast.topic(TOPIC, seed.clone());

    let (slots, _session, _store) = fresh_session();
    let (tx, _rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

    let opts = RenderOptions { hook_compile: true };
    let out = render_entry_with_broadcast(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots,
        &broadcast,
        tx,
        &opts,
    )
    .expect("render succeeds");

    // (1) HTML inlines the topic value — JSX renders the JSON array
    //     as `alice: hi` (single-element string array stringifies).
    assert!(
        out.html.contains("alice: hi"),
        "render should inline broadcast value into the HTML; got: {}",
        out.html
    );

    // (2) Opcode stream contains a SetTextRef whose slot id matches
    //     broadcast_slot_id(TOPIC). Position-independent search since
    //     the initial SlotSet appears first.
    let target_slot = broadcast_slot_id(TOPIC);
    let has_set_text_ref = out.opcodes.iter().any(|op| match op {
        Instruction::SetTextRef { slot_id, .. } => *slot_id == target_slot,
        _ => false,
    });
    assert!(
        has_set_text_ref,
        "expected SetTextRef targeting broadcast slot id; opcodes: {:?}",
        out.opcodes
    );

    // (3) Initial SlotSet for the topic is in the opcode stream so the
    //     client paints with the current broadcast value even before
    //     the first fan-out arrives.
    let initial_slot_set = out.opcodes.iter().find_map(|op| match op {
        Instruction::SlotSet { slot_id, value } if *slot_id == target_slot => Some(value.clone()),
        _ => None,
    });
    assert_eq!(
        initial_slot_set,
        Some(seed),
        "initial SlotSet must carry the seeded topic value"
    );
}

#[tokio::test]
async fn render_auto_subscribes_session_so_follow_up_write_topic_delivers() {
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    broadcast.topic(TOPIC, b"[]".to_vec());

    let (slots, _session, _store) = fresh_session();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

    let _ = render_entry_with_broadcast(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots,
        &broadcast,
        tx,
        &RenderOptions { hook_compile: true },
    )
    .expect("render succeeds");

    // Write after the render — the session was auto-subscribed so the
    // mpsc receiver must observe the SlotSet.
    let report = broadcast
        .write_topic(TOPIC, br#"["bob: yo"]"#.to_vec())
        .expect("write succeeds");
    assert_eq!(report.delivered, 1);

    let payload = rx.recv().await.expect("subscriber receives fan-out");
    let (slot_id, value) = extract_slot_set_value(&payload);
    assert_eq!(slot_id, broadcast_slot_id(TOPIC));
    assert_eq!(value, br#"["bob: yo"]"#.to_vec());
}

#[tokio::test]
async fn rendering_a_component_with_unknown_topic_auto_creates_it() {
    // `useSharedSlot("brand-new-topic")` in JSX, never registered
    // explicitly. The auto_subscribe path inside
    // `render_entry_with_broadcast` ensures the topic exists with an
    // empty initial value so the render binds something sensible.
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    assert!(broadcast.get(TOPIC).is_none(), "topic must start absent");

    let (slots, _session, _store) = fresh_session();
    let (tx, _rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

    let _ = render_entry_with_broadcast(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots,
        &broadcast,
        tx,
        &RenderOptions { hook_compile: true },
    )
    .expect("render succeeds");

    assert!(
        broadcast.get(TOPIC).is_some(),
        "render with useSharedSlot must auto-create the topic"
    );
}

#[tokio::test]
async fn two_sessions_rendering_same_component_both_receive_broadcast_writes() {
    // The framework moment: two tabs subscribe via the same JSX,
    // server-side broadcast write fans out to both.
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    broadcast.topic(TOPIC, b"[]".to_vec());

    let (slots_a, _, _) = fresh_session();
    let (slots_b, _, _) = fresh_session();
    let (tx_a, mut rx_a) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let (tx_b, mut rx_b) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);

    let _ = render_entry_with_broadcast(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots_a,
        &broadcast,
        tx_a,
        &RenderOptions { hook_compile: true },
    )
    .unwrap();
    let _ = render_entry_with_broadcast(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots_b,
        &broadcast,
        tx_b,
        &RenderOptions { hook_compile: true },
    )
    .unwrap();

    let report = broadcast
        .write_topic(TOPIC, b"[\"hello\"]".to_vec())
        .expect("write");
    assert_eq!(report.delivered, 2);

    for rx in [&mut rx_a, &mut rx_b] {
        let payload = rx.recv().await.expect("each session receives");
        let (_slot, value) = extract_slot_set_value(&payload);
        assert_eq!(value, b"[\"hello\"]".to_vec());
    }
}

#[tokio::test]
async fn rendering_without_broadcast_falls_back_to_null_binding() {
    // When the renderer goes through `render_entry_with_bindings`
    // (no broadcast handle), `useSharedSlot` binds to null — the
    // component renders without breaking. This is the safety net
    // for code paths that pre-date Phase O.2.
    use dom_render_compiler::runtime::eval::render_entry_with_bindings;
    let project = compile();
    let (slots, _, _) = fresh_session();

    let out = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots,
        &RenderOptions { hook_compile: true },
    )
    .expect("render without broadcast still succeeds");

    // The HTML renders without a topic value — it should at least
    // produce the wrapping element without panicking.
    assert!(out.html.contains("<ul"), "render should not break; got: {}", out.html);
}
