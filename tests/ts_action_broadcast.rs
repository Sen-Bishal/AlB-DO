//! Phase P · Stream C.2 — `broadcast()` interpreter builtin gate.
//!
//! Loads a TSX fixture whose `action(...)` declarations call the
//! `broadcast(topic, updater)` builtin in their bodies, dispatches
//! each via [`CompiledProject::invoke_action_with_broadcast`], and
//! asserts:
//!
//!   1. The broadcast registry's topic value updates.
//!   2. The updater receives the topic's current JSON value (so the
//!      read-modify-write semantics line up with `setState(fn)`).
//!   3. A subscribed session receives a `SlotSet` opcode whose
//!      `slot_id == broadcast_slot_id(topic)` over the WT patches
//!      lane — i.e. the same wire shape Phase O.2 fan-out emits.
//!   4. `broadcast()` without an installed `PHASE_K_BROADCAST` (the
//!      v1 scope guard) surfaces a clean Rust error rather than
//!      silently no-op-ing.
//!   5. Updater body sees full JSON value (here: a `Vec<String>`),
//!      not a string-coerced summary — proves the eval path doesn't
//!      drop into `value_to_string`.

use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::{Instruction, SlotId};
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::eval::{CompiledProject, SessionSlotView};
use dom_render_compiler::runtime::slot_store::SlotStore;
use dom_render_compiler::runtime::{broadcast_slot_id, BroadcastRegistry, SessionId};
use dom_render_compiler::transforms::allocate_form_action_id;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

const CHANNEL_CAPACITY: usize = 32;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ts_action")
        .join("broadcast_demo")
}

fn compile() -> CompiledProject {
    CompiledProject::load_from_dir(fixture()).expect("project compiles")
}

fn fresh_session() -> (SessionSlotView, SessionId, Arc<SlotStore>) {
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    (
        SessionSlotView::new(session, store.clone()),
        session,
        store,
    )
}

fn envelope_for(action_name: &str) -> ActionEnvelope {
    ActionEnvelope {
        action_id: allocate_form_action_id(action_name),
        event_kind: 0,
        payload: Vec::new(),
    }
}

fn extract_slot_set(payload: &[u8]) -> (SlotId, Vec<u8>) {
    let (frame, _) = decode_frame(payload).expect("decode frame");
    assert_eq!(
        frame.instructions.len(),
        1,
        "broadcast frame must carry exactly one SlotSet, got {frame:?}"
    );
    match frame.instructions.into_iter().next().expect("non-empty") {
        Instruction::SlotSet { slot_id, value } => (slot_id, value),
        other => panic!("expected SlotSet, got {other:?}"),
    }
}

#[tokio::test]
async fn broadcast_call_writes_topic_value() {
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    let (slots, _session, _store) = fresh_session();

    let envelope = envelope_for("set_counter_to_seven");
    project
        .invoke_action_with_broadcast(&envelope, &slots, broadcast.as_ref())
        .expect("action dispatches without error");

    let topic = broadcast
        .get("counter")
        .expect("topic auto-registered by broadcast() builtin");
    let value: serde_json::Value =
        serde_json::from_slice(&topic.current_value()).expect("topic value is JSON");
    assert_eq!(value, serde_json::json!(7.0));
}

#[tokio::test]
async fn broadcast_updater_receives_current_value_for_read_modify_write() {
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    // Pre-seed the topic so the updater has a meaningful current value.
    broadcast.topic("counter", serde_json::to_vec(&41).unwrap());

    let (slots, _session, _store) = fresh_session();
    let envelope = envelope_for("increment_counter");
    project
        .invoke_action_with_broadcast(&envelope, &slots, broadcast.as_ref())
        .expect("increment dispatches");

    let topic = broadcast.get("counter").expect("topic exists");
    let value: serde_json::Value = serde_json::from_slice(&topic.current_value()).unwrap();
    assert_eq!(
        value,
        serde_json::json!(42.0),
        "updater n => n + 1 must see current value 41 and write 42 \
         (numbers serialise as f64 through the interpreter)"
    );
}

#[tokio::test]
async fn broadcast_fans_out_slot_set_opcode_to_subscribed_session() {
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    broadcast.topic("counter", serde_json::to_vec(&0).unwrap());

    // Subscribe a session before the action fires. The auto_subscribe
    // helper drains the topic's initial value into `Vec<Instruction>`
    // and arms the channel for future writes.
    let listener_session = SessionId::random();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let _initial = broadcast.auto_subscribe(listener_session, tx, &["counter".to_string()]);

    // Different session invokes the action — broadcasts are
    // cross-session by design.
    let (slots, _writer_session, _store) = fresh_session();
    let envelope = envelope_for("set_counter_to_seven");
    project
        .invoke_action_with_broadcast(&envelope, &slots, broadcast.as_ref())
        .expect("dispatch ok");

    let payload = rx
        .recv()
        .await
        .expect("listener channel must receive the broadcast frame");
    let (slot_id, value) = extract_slot_set(&payload);
    assert_eq!(slot_id, broadcast_slot_id("counter"));
    let decoded: serde_json::Value = serde_json::from_slice(&value).unwrap();
    assert_eq!(decoded, serde_json::json!(7.0));
}

#[tokio::test]
async fn broadcast_returns_clean_error_when_called_without_phase_k_broadcast_installed() {
    let project = compile();
    let (slots, _session, _store) = fresh_session();
    // Plain `invoke_action` — no broadcast installed. The interpreter's
    // free-ident dispatch sees `current_phase_k_broadcast() == None`
    // and falls through to the next branch, which doesn't know what
    // `broadcast` is and surfaces an error from the body evaluator.
    let envelope = envelope_for("set_counter_to_seven");
    let result = project.invoke_action(&envelope, &slots);
    assert!(
        result.is_err(),
        "broadcast() outside an action-with-broadcast context must error, \
         got success: {result:?}"
    );
}

#[tokio::test]
async fn broadcast_updater_with_array_value_round_trips_structured_json() {
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    broadcast.topic(
        "chat:lobby",
        serde_json::to_vec(&serde_json::json!(["seed"])).unwrap(),
    );

    let (slots, _session, _store) = fresh_session();
    let envelope = envelope_for("replace_log_with_two_items");
    project
        .invoke_action_with_broadcast(&envelope, &slots, broadcast.as_ref())
        .expect("dispatch ok");

    let topic = broadcast.get("chat:lobby").unwrap();
    let value: serde_json::Value = serde_json::from_slice(&topic.current_value()).unwrap();
    assert_eq!(
        value,
        serde_json::json!(["alpha", "beta"]),
        "updater that returns a literal array must JSON-encode the \
         full array — proves the round-trip isn't value-to-string coerced"
    );
}

#[tokio::test]
async fn second_invoke_of_increment_continues_from_first_write() {
    let project = compile();
    let broadcast = Arc::new(BroadcastRegistry::new());
    broadcast.topic("counter", serde_json::to_vec(&0).unwrap());

    let (slots, _session, _store) = fresh_session();
    let envelope = envelope_for("increment_counter");
    for expected in 1..=3 {
        project
            .invoke_action_with_broadcast(&envelope, &slots, broadcast.as_ref())
            .expect("increment dispatches");
        let topic = broadcast.get("counter").unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&topic.current_value()).unwrap();
        assert_eq!(
            value.as_f64(),
            Some(expected as f64),
            "after {expected} invocation(s), counter should be {expected}"
        );
    }
}
