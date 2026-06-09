//! A1 · host-object bridge — renders through QuickJS.
//!
//! `CompiledProject::render_entry_quickjs` runs a component's *render* in the
//! real JS engine instead of the pure-Rust interpreter, with the host objects
//! exposed: props as the component argument, `useState` values seeded from the
//! session slot store (positional, falling back to each hook's initial), and
//! `useSharedSlot` values seeded from the broadcast registry. This is the
//! render-side symmetric counterpart to `invoke_action_quickjs`.
//!
//! These gates assert: (1) a real `import { useState } from "react"` component
//! renders its initial under QuickJS; (2) a slot value seeds the render so it
//! reflects current state; (3) a render body using `Array.map` — which the
//! pure-Rust evaluator does not model — renders correctly; (4) captured props
//! are snapshotted into the slot store so a follow-up action reads them.

use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

fn hook_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join(name)
}

fn render_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("render_quickjs")
        .join(name)
}

fn engine() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("engine init");
    engine
}

/// Render once through the pure-Rust Phase K path to discover the slot a
/// `useState` value binds to — exactly the id bakabox subscribes via the
/// emitted `SetTextRef`. Lets the seeding test address the slot without
/// hard-coding its FNV hash.
fn value_slot_id(
    project: &CompiledProject,
    slots: &SessionSlotView,
) -> dom_render_compiler::ir::opcode::SlotId {
    let opts = RenderOptions { hook_compile: true };
    let out = render_entry_with_bindings(
        project,
        "Component.tsx",
        &Value::Object(Default::default()),
        slots,
        &opts,
    )
    .expect("initial render succeeds");
    out.opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("render emits a SetTextRef for the useState read")
}

#[test]
fn counter_renders_initial_under_quickjs() {
    let project = CompiledProject::load_from_dir(hook_fixture("counter")).expect("counter compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);
    let mut engine = engine();

    // Fresh slot store: `useState(0)` falls back to its initial.
    let out = project
        .render_entry_quickjs(&mut engine, "Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("quickjs render succeeds");

    assert_eq!(
        out.html, "<button>0</button>",
        "initial render must show the useState initial; got: {}",
        out.html
    );
    // onClick must NOT leak into the markup as a stringified closure.
    assert!(
        !out.html.contains("onClick") && !out.html.contains("function"),
        "event handler must be dropped from server markup; got: {}",
        out.html
    );
}

#[test]
fn seeded_slot_value_drives_the_quickjs_render() {
    let project = CompiledProject::load_from_dir(hook_fixture("counter")).expect("counter compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);
    let mut engine = engine();

    // Discover the slot, write a value into it (as a prior action would have),
    // and drain so the write isn't a pending user-facing SlotSet.
    let slot_id = value_slot_id(&project, &slots);
    slots.write(slot_id, b"5".to_vec());
    let _ = slots.drain_pending();

    let out = project
        .render_entry_quickjs(&mut engine, "Component.tsx", &Value::Object(Default::default()), &slots)
        .expect("quickjs render succeeds");

    assert_eq!(
        out.html, "<button>5</button>",
        "render must reflect the persisted slot value (5), not the initial (0); got: {}",
        out.html
    );
}

#[test]
fn render_body_using_array_map_renders_under_quickjs() {
    // The render body calls `props.items.map(...)` — a construct the pure-Rust
    // evaluator does not model. Under QuickJS it just runs.
    let project = CompiledProject::load_from_dir(render_fixture("list")).expect("list compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store);
    let mut engine = engine();

    let props = json!({ "items": ["alpha", "beta", "gamma"] });
    let out = project
        .render_entry_quickjs(&mut engine, "Component.tsx", &props, &slots)
        .expect("quickjs render succeeds");

    assert_eq!(
        out.html, "<ul><li>alpha</li><li>beta</li><li>gamma</li></ul>",
        "Array.map render body must produce the list; got: {}",
        out.html
    );
}

#[test]
fn captured_prop_is_snapshotted_for_the_action_path() {
    // The stepper captures the `step` prop in its onClick handler. Rendering it
    // via QuickJS must snapshot `step` into its capture slot so a follow-up
    // action reads the value the render observed (parity with the pure-Rust
    // `snapshot_captured_props_into_slots`). Without the snapshot the handler's
    // `n + step` reference is unbound and the dispatch errors — so a clean
    // dispatch that yields 4 proves the render persisted the prop.
    let project = CompiledProject::load_from_dir(hook_fixture("stepper")).expect("stepper compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = engine();

    let props = json!({ "step": 4 });

    // Discover the proxy id + value slot on a THROWAWAY store so its own
    // prop-snapshot can't taint the store under test — only the QuickJS render
    // below is allowed to write the real capture slot. Ids are deterministic
    // (module/fn/hook), so they match across stores.
    let (proxy_id, slot_id) = {
        let throwaway = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
        let opts = RenderOptions { hook_compile: true };
        let bind = render_entry_with_bindings(&project, "Component.tsx", &props, &throwaway, &opts)
            .expect("bind render");
        let proxy_id = bind
            .opcodes
            .iter()
            .find_map(|op| match op {
                Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
                _ => None,
            })
            .expect("BindEvent present");
        let slot_id = bind
            .opcodes
            .iter()
            .find_map(|op| match op {
                Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
                _ => None,
            })
            .expect("SetTextRef present");
        (proxy_id, slot_id)
    };

    // Only this render writes the capture slot in `store`.
    project
        .render_entry_quickjs(&mut engine, "Component.tsx", &props, &slots)
        .expect("quickjs render succeeds");

    let envelope = ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    };
    project
        .invoke_action_quickjs(&mut engine, &envelope, &slots)
        .expect("action dispatches (step was snapshotted by the render)");

    let raw = store.read(session, slot_id).expect("slot persisted");
    let decoded: Value = serde_json::from_slice(&raw).expect("decodes");
    assert_eq!(
        decoded.as_f64().expect("numeric") as i64,
        4,
        "count must increment by the captured step (4): 0 + 4 = 4"
    );
}
