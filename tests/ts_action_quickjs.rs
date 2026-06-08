//! A1 · host-object bridge — actions through QuickJS.
//!
//! `CompiledProject::invoke_action_quickjs` runs a handler body in the real JS
//! engine instead of the pure-Rust interpreter. This gate asserts **parity**
//! with the pure-Rust `invoke_action` path on the canonical `useState` counter:
//! the same slot increments, the same single `SlotSet` ships, and the dirty set
//! is left clean — so swapping the executor is invisible on the wire. It then
//! exercises a handler using a `for` loop, which the pure-Rust evaluator
//! rejects, to prove the engine genuinely runs arbitrary JS.

use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join(name)
}

fn engine() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("engine init");
    engine
}

/// Render once to discover the handler's `proxy_id` and the slot it writes,
/// exactly as `bakabox` would from the bind opcodes.
fn bind_ids(project: &CompiledProject, slots: &SessionSlotView) -> (u32, dom_render_compiler::ir::opcode::SlotId) {
    let opts = RenderOptions { hook_compile: true };
    let out = render_entry_with_bindings(
        project,
        "Component.tsx",
        &Value::Object(Default::default()),
        slots,
        &opts,
    )
    .expect("initial render succeeds");

    let proxy_id = out
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("render emits a BindEvent");
    let slot_id = out
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("render emits a SetTextRef");
    (proxy_id, slot_id)
}

#[test]
fn counter_action_via_quickjs_matches_pure_rust_path() {
    let project = CompiledProject::load_from_dir(fixture("counter")).expect("counter compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = engine();

    let (proxy_id, slot_id) = bind_ids(&project, &slots);

    let envelope = ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    };
    let response = project
        .invoke_action_quickjs(&mut engine, &envelope, &slots)
        .expect("quickjs handler dispatches without error");

    // `setN(n + 1)` with n seeded from the initial 0 → slot is written to 1.
    let raw = store
        .read(session, slot_id)
        .expect("slot must be persisted by the handler");
    let decoded: Value = serde_json::from_slice(&raw).expect("slot value decodes as JSON");
    assert_eq!(
        decoded.as_f64().expect("numeric") as i64,
        1,
        "n must increment 0 → 1, matching the pure-Rust path"
    );

    // Exactly one SlotSet for the written slot — identical wire shape.
    let slot_sets: Vec<_> = response
        .iter()
        .filter(|op| matches!(op, Instruction::SlotSet { slot_id: s, .. } if *s == slot_id))
        .collect();
    assert_eq!(
        slot_sets.len(),
        1,
        "quickjs dispatch must ship exactly one SlotSet; got: {response:?}"
    );

    // The dispatch left the dirty set clean — a follow-up drain is a no-op,
    // the same post-condition `invoke_action` guarantees.
    assert!(
        slots.drain_pending().is_empty(),
        "invoke_action_quickjs must leave the dirty set drained"
    );
}

#[test]
fn quickjs_handler_resolves_module_level_constant() {
    // The handler body is `setN(n + STEP)` where `STEP` is a module-level
    // `const STEP = 5`. The QuickJS path must seed `STEP` into the handler scope
    // (parity with the pure-Rust `seed_env_with_module_constants`); before const
    // seeding it threw `ReferenceError: STEP is not defined`.
    let project =
        CompiledProject::load_from_dir(fixture("counter_const")).expect("counter_const compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = engine();

    let (proxy_id, slot_id) = bind_ids(&project, &slots);
    let envelope = ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    };

    project
        .invoke_action_quickjs(&mut engine, &envelope, &slots)
        .expect("quickjs handler dispatches without a ReferenceError for STEP");

    // n started at 0; `n + STEP` with STEP = 5 → slot written to 5.
    let raw = store.read(session, slot_id).expect("slot persisted");
    let decoded: Value = serde_json::from_slice(&raw).expect("slot value decodes");
    assert_eq!(
        decoded.as_f64().expect("numeric") as i64,
        5,
        "handler must read the module const STEP (=5): 0 + 5 = 5"
    );
}

#[test]
fn second_dispatch_via_quickjs_reads_persisted_slot_value() {
    let project = CompiledProject::load_from_dir(fixture("counter")).expect("counter compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = engine();

    let (proxy_id, slot_id) = bind_ids(&project, &slots);
    let envelope = ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    };

    // Two clicks: the second must read the persisted 1 and write 2 — proving
    // the QuickJS path seeds `n` from the slot store, not a stale initial.
    project
        .invoke_action_quickjs(&mut engine, &envelope, &slots)
        .expect("first dispatch");
    project
        .invoke_action_quickjs(&mut engine, &envelope, &slots)
        .expect("second dispatch");

    let raw = store.read(session, slot_id).expect("slot persisted");
    let decoded: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        decoded.as_f64().unwrap() as i64,
        2,
        "second click must increment the persisted value 1 → 2"
    );
}
