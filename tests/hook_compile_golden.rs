//! Phase K — hook-compile corpus (the spec).
//!
//! Each fixture under `tests/fixtures/hook_compile/<case>/` is a single
//! component the compiler must lift into:
//!
//!   1. A render that emits binding opcodes alongside HTML:
//!        - `BindEvent { stable_id, event_id, proxy_id }` per `on*` handler
//!        - `SetTextRef { stable_id, slot_id }` per `{slot}` read in JSX
//!
//!   2. A server-side ActionHandler registration per handler, keyed by
//!      `proxy_id`. Invoking the handler must execute the handler body
//!      against the session slot store and produce `SlotSet` opcodes
//!      for every `setX(...)` call inside.
//!
//! The corpus stages risk per the sprint plan:
//!   - Stage 1: literal `useState(N)` only, no closures over outer scope.
//!     Counter, multi_hook, string_state.
//!   - Stage 2 (deferred): closures over component props.
//!   - Stage 3 (deferred): closures over module-level constants.
//!
//! Stage 1 is the gate for Phase K — when all three Stage-1 cases pass,
//! the framework moment has landed.

use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::{Instruction, SlotId};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::slot_store::SlotStore;
use dom_render_compiler::runtime::session::SessionId;
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

fn compile(name: &str) -> CompiledProject {
    CompiledProject::load_from_dir(fixture(name)).expect("project compiles")
}

fn render_initial(project: &CompiledProject, slots: &SessionSlotView) -> (String, Vec<Instruction>) {
    let opts = RenderOptions { hook_compile: true };
    let out = render_entry_with_bindings(
        project,
        "Component.tsx",
        &Value::Object(Default::default()),
        slots,
        &opts,
    )
    .expect("initial render succeeds");
    (out.html, out.opcodes)
}

// ── Stage 1 · Counter ────────────────────────────────────────────────

#[test]
fn counter_emits_bind_event_and_set_text_ref_on_initial_render() {
    let project = compile("counter");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let (html, opcodes) = render_initial(&project, &slots);

    // HTML carries the initial slot value 0.
    assert!(
        html.contains("<button") && html.contains(">0</button>"),
        "Counter initial HTML must show `0`, got: {html}"
    );

    // Binding opcodes: at minimum one BindEvent (click) and one
    // SetTextRef (the {n} interpolation in the button body).
    let bind_event_count = opcodes
        .iter()
        .filter(|op| matches!(op, Instruction::BindEvent { .. }))
        .count();
    let set_text_ref_count = opcodes
        .iter()
        .filter(|op| matches!(op, Instruction::SetTextRef { .. }))
        .count();
    assert_eq!(bind_event_count, 1, "Counter must emit one BindEvent for onClick; opcodes: {opcodes:?}");
    assert_eq!(set_text_ref_count, 1, "Counter must emit one SetTextRef for {{n}}; opcodes: {opcodes:?}");
}

#[test]
fn counter_action_dispatch_increments_slot_and_emits_slot_set() {
    let project = compile("counter");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    // First render to materialise the BindEvent and discover its proxy_id.
    let (_html, opcodes) = render_initial(&project, &slots);
    let proxy_id = opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("Counter render emits a BindEvent");
    let slot_id = opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("Counter render emits a SetTextRef");

    // Simulate the bakabox POST: invoke the compiled handler.
    let envelope = ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    };
    let response = project
        .invoke_action(&envelope, &slots)
        .expect("compiled handler dispatches without error");

    // The handler does `setN(n + 1)`. Slot 0 read 0, write 1.
    let raw = store.read(session, slot_id).expect("slot must be written by handler");
    let decoded: Value = serde_json::from_slice(&raw).expect("slot value decodes as JSON");
    let n = decoded
        .as_f64()
        .expect("slot value must be numeric after setN(n + 1)");
    assert_eq!(n as i64, 1, "n must be incremented from 0 to 1");

    // The response from invoke_action carries both the handler's
    // explicit return AND the auto-drained slot writes. This mirrors
    // the wire dispatcher in `albedo-server::handlers::action` so
    // tests exercise the same shape that ships over HTTP.
    let slot_sets: Vec<_> = response
        .iter()
        .filter(|op| matches!(op, Instruction::SlotSet { slot_id: s, .. } if *s == slot_id))
        .collect();
    assert_eq!(
        slot_sets.len(),
        1,
        "handler response must include exactly one SlotSet for the written slot; got: {response:?}"
    );

    // Confirm the dispatcher already drained — a subsequent drain
    // call should return empty so the same SlotSet isn't shipped twice.
    let post_dispatch_drain = slots.drain_pending();
    assert!(
        post_dispatch_drain.is_empty(),
        "invoke_action must drain pending writes; got leftover: {post_dispatch_drain:?}"
    );
}

#[test]
fn counter_second_render_reflects_persisted_slot_value() {
    let project = compile("counter");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    // First render to register bindings.
    let (_html, opcodes) = render_initial(&project, &slots);
    let proxy_id = opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("counter render emits a BindEvent");

    // Click 3 times.
    for _ in 0..3 {
        let envelope = ActionEnvelope {
            action_id: proxy_id,
            event_kind: 0,
            payload: Vec::new(),
        };
        project.invoke_action(&envelope, &slots).expect("dispatch succeeds");
        slots.drain_pending();
    }

    // A re-render in the same session must now show `3`, not `0`.
    let (html, _opcodes) = render_initial(&project, &slots);
    assert!(
        html.contains(">3</button>"),
        "after 3 increments, re-render must show `3` in the button; got: {html}"
    );
}

// ── Stage 1 · multi-hook (slot id allocation) ────────────────────────

#[test]
fn multi_hook_component_allocates_one_slot_per_useState_in_source_order() {
    let project = compile("multi_hook");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let (html, opcodes) = render_initial(&project, &slots);

    // Initial HTML carries both initial values.
    assert!(html.contains(">anon</span>"), "name slot initial 'anon' missing: {html}");
    assert!(html.contains(">0</span>"), "count slot initial 0 missing: {html}");

    // Two SetTextRef opcodes (one per slot-read site).
    let set_text_refs: Vec<SlotId> = opcodes
        .iter()
        .filter_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        set_text_refs.len(),
        2,
        "expected two SetTextRef opcodes (one per useState read); got: {opcodes:?}"
    );

    // Two distinct SlotIds — multi-hook must NOT collide.
    assert_ne!(
        set_text_refs[0], set_text_refs[1],
        "two hooks must allocate distinct slot ids; got both as {:?}",
        set_text_refs[0]
    );

    // Two BindEvent opcodes (one per button).
    let bind_events = opcodes
        .iter()
        .filter(|op| matches!(op, Instruction::BindEvent { .. }))
        .count();
    assert_eq!(bind_events, 2, "expected two BindEvent opcodes; got: {opcodes:?}");
}

#[test]
fn multi_hook_writes_to_correct_slot_per_setter() {
    let project = compile("multi_hook");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let (_html, opcodes) = render_initial(&project, &slots);

    // Collect the two BindEvents. Per fixture: first button = setCount, second = setName.
    let bind_events: Vec<u32> = opcodes
        .iter()
        .filter_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .collect();
    assert_eq!(bind_events.len(), 2);

    // Slot ids: [name_slot, count_slot] in source order.
    let set_text_refs: Vec<SlotId> = opcodes
        .iter()
        .filter_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .collect();
    let name_slot = set_text_refs[0];
    let count_slot = set_text_refs[1];

    // Invoke the bump handler — must write to count_slot, not name_slot.
    project
        .invoke_action(
            &ActionEnvelope { action_id: bind_events[0], event_kind: 0, payload: Vec::new() },
            &slots,
        )
        .expect("bump dispatch ok");
    let count_raw = store.read(session, count_slot).expect("count must be written");
    let count_v: Value = serde_json::from_slice(&count_raw).unwrap();
    assert_eq!(
        count_v.as_f64().map(|f| f as i64),
        Some(1),
        "bump must write 1 to count_slot"
    );
    let name_after_bump = store.read(session, name_slot).map(|raw| {
        serde_json::from_slice::<Value>(&raw).ok()
    });
    if let Some(Some(name_v)) = name_after_bump {
        assert_eq!(
            name_v.as_str(),
            Some("anon"),
            "bump must not change name_slot away from 'anon'"
        );
    }
    slots.drain_pending();

    // Invoke the rename handler — must write to name_slot.
    project
        .invoke_action(
            &ActionEnvelope { action_id: bind_events[1], event_kind: 0, payload: Vec::new() },
            &slots,
        )
        .expect("rename dispatch ok");
    let name_raw = store.read(session, name_slot).expect("name must be written");
    let name: String = serde_json::from_slice(&name_raw).unwrap();
    assert_eq!(name, "alice", "rename must write 'alice' to name_slot");
}

// ── Stage 1 · string state ───────────────────────────────────────────

#[test]
fn string_state_initial_render_emits_set_text_ref_for_string_slot() {
    let project = compile("string_state");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let (html, opcodes) = render_initial(&project, &slots);

    assert!(html.contains(">idle</button>"), "initial label must be 'idle'; got: {html}");
    assert!(
        opcodes
            .iter()
            .any(|op| matches!(op, Instruction::SetTextRef { .. })),
        "string-state must emit SetTextRef for {{label}}; got: {opcodes:?}"
    );
}

#[test]
fn string_state_setter_with_literal_writes_string_to_slot() {
    let project = compile("string_state");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let (_html, opcodes) = render_initial(&project, &slots);
    let proxy_id = opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("string-state emits BindEvent");
    let slot_id = opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("string-state emits SetTextRef");

    project
        .invoke_action(
            &ActionEnvelope { action_id: proxy_id, event_kind: 0, payload: Vec::new() },
            &slots,
        )
        .expect("dispatch succeeds");

    let raw = store.read(session, slot_id).expect("label slot must be written");
    let decoded: String = serde_json::from_slice(&raw).expect("slot value decodes as string");
    assert_eq!(decoded, "ready", "setLabel('ready') must store 'ready'");
}

// ── Stage 2 · closures over component props ──────────────────────────

#[test]
fn stage2_stepper_initial_render_emits_bindings_and_renders_initial_state() {
    let project = compile("stepper");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let opts = RenderOptions { hook_compile: true };
    let out = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &serde_json::json!({ "step": 5 }),
        &slots,
        &opts,
    )
    .expect("stepper renders with props");

    assert!(
        out.html.contains(">0</button>"),
        "stepper initial HTML must show 0; got: {}",
        out.html
    );
    assert_eq!(
        out.opcodes
            .iter()
            .filter(|op| matches!(op, Instruction::BindEvent { .. }))
            .count(),
        1,
        "stepper must emit one BindEvent"
    );
    assert_eq!(
        out.opcodes
            .iter()
            .filter(|op| matches!(op, Instruction::SetTextRef { .. }))
            .count(),
        1,
        "stepper must emit one SetTextRef"
    );
}

#[test]
fn stage2_handler_reads_captured_prop_and_writes_correct_increment() {
    let project = compile("stepper");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let opts = RenderOptions { hook_compile: true };
    let render = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &serde_json::json!({ "step": 5 }),
        &slots,
        &opts,
    )
    .expect("render");

    let proxy_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("BindEvent");
    let slot_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("SetTextRef");

    let response = project
        .invoke_action(
            &ActionEnvelope { action_id: proxy_id, event_kind: 0, payload: Vec::new() },
            &slots,
        )
        .expect("dispatch");

    let raw = store.read(session, slot_id).expect("slot written");
    let v: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        v.as_f64().map(|f| f as i64),
        Some(5),
        "handler with captured prop must write `n + step = 0 + 5 = 5`"
    );
    assert!(
        response
            .iter()
            .any(|op| matches!(op, Instruction::SlotSet { slot_id: s, .. } if *s == slot_id)),
        "response must include the SlotSet for the increment"
    );
}

#[test]
fn stage2_re_render_with_new_props_updates_captured_snapshot() {
    let project = compile("stepper");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let opts = RenderOptions { hook_compile: true };

    let render = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &serde_json::json!({ "step": 5 }),
        &slots,
        &opts,
    )
    .expect("first render");
    let proxy_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("BindEvent");
    let slot_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("SetTextRef");

    // Re-render with new prop value — capture slot is rewritten.
    let _ = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &serde_json::json!({ "step": 10 }),
        &slots,
        &opts,
    )
    .expect("second render");

    project
        .invoke_action(
            &ActionEnvelope { action_id: proxy_id, event_kind: 0, payload: Vec::new() },
            &slots,
        )
        .expect("dispatch");

    let raw = store.read(session, slot_id).expect("slot written");
    let v: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        v.as_f64().map(|f| f as i64),
        Some(10),
        "post-re-render dispatch must use the new prop value (step=10), not stale step=5"
    );
}

#[test]
fn stage2_greeter_captures_multiple_props_of_different_shapes() {
    let project = compile("greeter");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());

    let opts = RenderOptions { hook_compile: true };
    let render = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &serde_json::json!({ "initial": "alice", "exclaim": "!!!" }),
        &slots,
        &opts,
    )
    .expect("render");

    assert!(
        render.html.contains(">alice</button>"),
        "initial render must use prop value for useState initial; got: {}",
        render.html
    );

    let proxy_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("BindEvent");
    let slot_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("SetTextRef");

    project
        .invoke_action(
            &ActionEnvelope { action_id: proxy_id, event_kind: 0, payload: Vec::new() },
            &slots,
        )
        .expect("dispatch");

    let raw = store.read(session, slot_id).expect("slot written");
    let v: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        v.as_str(),
        Some("alice!!!"),
        "handler should concatenate `name + exclaim` using captured prop"
    );
}

// ── Intern table contract ────────────────────────────────────────────

/// Every render that emits any `BindEvent` must also prepend an
/// `InitInternTable { kind: Event, .. }` opcode whose entries cover
/// every `event_id` referenced by a `BindEvent`. Bakabox resolves
/// `event_id → event_name` through this table; without it, the
/// `event_id` is unresolvable and the binding silently misfires.
#[test]
fn render_prepends_event_intern_table_covering_every_bind_event_id() {
    use dom_render_compiler::ir::opcode::{InternTableKind, Instruction as I};

    let project = compile("counter");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let (_html, opcodes) = render_initial(&project, &slots);

    // First opcode must be the intern table.
    let first = &opcodes[0];
    let (table_kind, table_entries) = match first {
        I::InitInternTable { table } => (table.kind, &table.entries),
        other => panic!("first opcode must be InitInternTable, got: {other:?}"),
    };
    assert_eq!(table_kind, InternTableKind::Event);
    assert!(!table_entries.is_empty(), "intern table must carry at least one entry");

    // Every BindEvent's event_id must appear in the table.
    let referenced: Vec<u16> = opcodes
        .iter()
        .filter_map(|op| match op {
            I::BindEvent { event_id, .. } => Some(event_id.0),
            _ => None,
        })
        .collect();
    let known: std::collections::HashSet<u16> = table_entries.iter().map(|e| e.id).collect();
    for id in &referenced {
        assert!(
            known.contains(id),
            "BindEvent references event_id {id} not in intern table {table_entries:?}"
        );
    }

    // The counter only has `click`, so the table should contain it.
    assert!(
        table_entries.iter().any(|e| e.value == "click"),
        "intern table for Counter must include 'click'; got {table_entries:?}"
    );
}

// ── Determinism guard ────────────────────────────────────────────────

#[test]
fn proxy_ids_and_slot_ids_are_deterministic_across_compilations() {
    let a = compile("counter");
    let b = compile("counter");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots_a = SessionSlotView::new(session, store.clone());
    let slots_b = SessionSlotView::new(session, store.clone());

    let (_h_a, op_a) = render_initial(&a, &slots_a);
    let (_h_b, op_b) = render_initial(&b, &slots_b);

    let proxy_a: Vec<u32> = op_a.iter().filter_map(|op| match op {
        Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
        _ => None,
    }).collect();
    let proxy_b: Vec<u32> = op_b.iter().filter_map(|op| match op {
        Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
        _ => None,
    }).collect();
    assert_eq!(proxy_a, proxy_b, "proxy_ids must be deterministic across compilations");

    let slot_a: Vec<SlotId> = op_a.iter().filter_map(|op| match op {
        Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
        _ => None,
    }).collect();
    let slot_b: Vec<SlotId> = op_b.iter().filter_map(|op| match op {
        Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
        _ => None,
    }).collect();
    assert_eq!(slot_a, slot_b, "slot_ids must be deterministic across compilations");
}
