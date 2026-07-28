//! APERTURE · A2 — an action body that calls out, end to end.
//!
//! The unit tests in `runtime::quickjs_engine` drive the suspend protocol with a
//! hand-written body. This gate starts from **TSX a user could have written**
//! and proves the whole chain: the extractor keeps the `async` arrow, the
//! workflow lowering strips the `await`s that would otherwise be a SyntaxError,
//! the engine suspends with the request staged, and the same action completes on
//! the next pass with its effects intact.
//!
//! The pass loop lives here rather than inside the dispatch call, because that
//! is where it lives in production too: resolving a request is async, so the
//! engine goes back to the pool across the round trip (APERTURE.md invariant
//! 2.6). A driver that looped inside the sync dispatch would be holding an
//! engine while it waited — the blocking host function this design exists to
//! avoid, wearing a different shape.

use dom_render_compiler::aperture::{Journal, StepKind, StepOutcome};
use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::runtime::compiled::ActionPass;
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

/// Render once to discover the handler's `proxy_id`, exactly as the client would
/// from the bind opcodes.
fn proxy_id(project: &CompiledProject, slots: &SessionSlotView) -> u32 {
    let out = render_entry_with_bindings(
        project,
        "Component.tsx",
        &Value::Object(Default::default()),
        slots,
        &RenderOptions { hook_compile: true },
    )
    .expect("initial render succeeds");
    out.opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("render emits a BindEvent")
}

#[test]
fn an_awaiting_handler_suspends_once_then_completes_with_its_effects() {
    let project =
        CompiledProject::load_from_dir(fixture("fetching_handler")).expect("fixture compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = engine();

    let envelope = ActionEnvelope {
        action_id: proxy_id(&project, &slots),
        event_kind: 0,
        payload: Vec::new(),
    };

    let mut journal = Journal::new("w_gate", "build-gate");
    let mut issued: Vec<String> = Vec::new();
    let mut passes = 0usize;

    let instructions = loop {
        passes += 1;
        assert!(passes < 6, "runaway replay");
        match project
            .invoke_action_quickjs_pass(&mut engine, &envelope, &slots, None, Some(&journal))
            .expect("pass dispatches")
        {
            ActionPass::Completed(instructions) => break instructions,
            ActionPass::Suspended { pending, .. } => {
                // The engine is free here. In the server this is where it goes
                // back to the pool and the HTTP layer takes over.
                assert_eq!(pending.len(), 1, "one call staged");
                let request = &pending[0];
                assert_eq!(request.method, "GET");
                issued.push(request.url.clone());

                // § 5.3 — the key is derived from the journal position. The
                // author never wrote one and could not have got it wrong.
                assert_eq!(journal.idempotency_key(request.step), "w_gate:0");

                journal
                    .append(
                        request.step,
                        StepKind::Fetch,
                        &request.digest,
                        StepOutcome::Completed(json!({
                            "status": 200,
                            "body": r#"{"state":"green"}"#,
                        })),
                    )
                    .expect("append");
            }
        }
    };

    assert_eq!(passes, 2, "one pass to ask, one to finish");
    assert_eq!(issued, vec!["https://api.test/status"]);
    assert_eq!(journal.len(), 1);

    // The effect the body produced on its *completing* pass — the suspended
    // pass produced none, which is what keeps replay from double-applying.
    let written: Vec<&Vec<u8>> = instructions
        .iter()
        .filter_map(|op| match op {
            Instruction::SlotSet { value, .. } => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(written.len(), 1, "one slot write, got {instructions:?}");
    assert_eq!(
        String::from_utf8(written[0].clone()).unwrap(),
        "\"green\"",
        "the state the upstream returned reached the slot"
    );
}

/// A body that never calls out is untouched by any of this: same passes, same
/// effects, no journal involvement. The lowering runs on every handler, so this
/// is the guard that it costs nothing to bodies that do not fetch.
#[test]
fn a_handler_that_never_calls_out_completes_in_one_pass() {
    let project = CompiledProject::load_from_dir(fixture("counter")).expect("counter compiles");
    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = engine();

    let envelope = ActionEnvelope {
        action_id: proxy_id(&project, &slots),
        event_kind: 0,
        payload: Vec::new(),
    };

    let journal = Journal::new("w", "b");
    match project
        .invoke_action_quickjs_pass(&mut engine, &envelope, &slots, None, Some(&journal))
        .expect("dispatches")
    {
        ActionPass::Completed(instructions) => {
            assert!(
                instructions
                    .iter()
                    .any(|op| matches!(op, Instruction::SlotSet { .. })),
                "the counter still increments"
            );
            assert_eq!(journal.len(), 0, "and records nothing");
        }
        ActionPass::Suspended { .. } => panic!("a body with no fetch must not suspend"),
    }
}
