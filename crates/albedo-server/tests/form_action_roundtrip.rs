//! Phase L · form action roundtrip test.
//!
//! Verifies that a typed `form_action_handler::<T>(name, handler)`:
//!
//!   1. Receives the JSON-encoded payload bytes decoded into `T` and
//!      runs the user closure on success.
//!   2. Emits `SetText` opcodes targeting the per-field
//!      `data-albedo-error` ids when `FromFormPayload::from_form_payload`
//!      returns `FormDecodeError::Validation`. No user closure runs.
//!   3. Returns a `RuntimeError` for genuinely malformed payloads so
//!      the dispatcher can render a 400.
//!
//! The dispatcher is exercised directly (no HTTP layer) so the test
//! stays fast and deterministic.

use albedo_server::actions::{ActionHandler, SessionSlots};
use albedo_server::lifecycle::RequestContext;
use albedo_server::render::form_action::{
    form_action_handler, form_action_id, FormDecodeError, FromFormPayload,
};
use albedo_server::routing::HttpMethod;
use bytes::Bytes;
use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::{Instruction, SlotId, StableId};
use dom_render_compiler::runtime::{SessionId, SlotStore};
use dom_render_compiler::transforms::form::allocate_field_error_id;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Raw JSON-decoded form payload. Used as the inner shape for both
/// the "happy path" and "validation error" cases.
#[derive(Debug, Deserialize)]
struct LoginForm {
    user: String,
    pass: String,
}

/// Validated wrapper. Demonstrates the per-field error path: any
/// failure is collected into `FormDecodeError::Validation`, which the
/// dispatcher will surface as `SetText` opcodes per field.
struct ValidatedLogin {
    user: String,
    pass: String,
}

impl FromFormPayload for ValidatedLogin {
    fn from_form_payload(payload: &[u8]) -> Result<Self, FormDecodeError> {
        let raw: LoginForm = serde_json::from_slice(payload)
            .map_err(|e| FormDecodeError::Malformed(e.to_string()))?;
        let mut errors = HashMap::new();
        if raw.user.is_empty() {
            errors.insert("user".to_string(), "required".to_string());
        }
        if raw.pass.len() < 8 {
            errors.insert("pass".to_string(), "at least 8 chars".to_string());
        }
        if !errors.is_empty() {
            return Err(FormDecodeError::Validation { errors });
        }
        Ok(ValidatedLogin {
            user: raw.user,
            pass: raw.pass,
        })
    }
}

/// Minimal RequestContext stub matching the shape the actions
/// module's own unit tests use. Action handlers under test don't
/// read the request body here, so an empty `Bytes` is fine.
fn ctx() -> RequestContext {
    RequestContext {
        request_id: "test".into(),
        method: HttpMethod::Post,
        path: "/_albedo/action".into(),
        query: BTreeMap::new(),
        params: BTreeMap::new(),
        headers: BTreeMap::new(),
        body: Bytes::new(),
        metadata: BTreeMap::new(),
    }
}

/// Fresh per-test slot view bound to a random session. The action
/// handlers under test do not actually write to slots, but the
/// `ActionHandler::handle` signature requires a view.
fn slots() -> SessionSlots {
    SessionSlots::new(SessionId::random(), Arc::new(SlotStore::new()))
}

#[tokio::test]
async fn typed_handler_decodes_and_runs_on_valid_payload() {
    let handler = form_action_handler::<ValidatedLogin, _, _>(
        "submit_login",
        |_ctx, form: ValidatedLogin, _slots| async move {
            assert_eq!(form.user, "alice");
            assert!(form.pass.len() >= 8);
            // The user closure returns whatever opcodes the
            // application wants; the dispatcher passes them through
            // unchanged.
            Ok(vec![Instruction::SlotSet {
                slot_id: SlotId(1),
                value: b"ok".to_vec(),
            }])
        },
    );

    let envelope = ActionEnvelope {
        action_id: form_action_id("submit_login"),
        event_kind: 2, // Submit
        payload: serde_json::to_vec(&serde_json::json!({
            "user": "alice",
            "pass": "longenough",
        }))
        .unwrap(),
    };

    let out = handler.handle(&ctx(), &envelope, slots()).await.unwrap();
    assert_eq!(out.len(), 1);
    assert!(matches!(
        &out[0],
        Instruction::SlotSet { slot_id: SlotId(1), value } if value == b"ok"
    ));
}

#[tokio::test]
async fn typed_handler_emits_settext_on_validation_errors() {
    let handler = form_action_handler::<ValidatedLogin, _, _>(
        "submit_login",
        |_ctx, _form: ValidatedLogin, _slots| async move {
            panic!("application handler must not run when validation fails");
        },
    );

    let envelope = ActionEnvelope {
        action_id: form_action_id("submit_login"),
        event_kind: 2,
        payload: serde_json::to_vec(&serde_json::json!({
            "user": "",
            "pass": "short",
        }))
        .unwrap(),
    };

    let out = handler.handle(&ctx(), &envelope, slots()).await.unwrap();
    // One SetText per failing field — `user` and `pass`.
    assert_eq!(out.len(), 2);

    // Build the expected stable id set so we can confirm each emitted
    // opcode targets one of the declared error spans.
    let expected_user = StableId(allocate_field_error_id("submit_login", "user"));
    let expected_pass = StableId(allocate_field_error_id("submit_login", "pass"));

    let mut saw_user = false;
    let mut saw_pass = false;
    for op in &out {
        match op {
            Instruction::SetText { stable_id, text } => {
                assert!(!text.is_empty(), "validation error message must not be empty");
                if *stable_id == expected_user {
                    saw_user = true;
                } else if *stable_id == expected_pass {
                    saw_pass = true;
                } else {
                    panic!("unexpected stable_id in validation patch: {stable_id:?}");
                }
            }
            other => panic!("expected SetText, got {other:?}"),
        }
    }
    assert!(saw_user, "user field error opcode missing");
    assert!(saw_pass, "pass field error opcode missing");
}

#[tokio::test]
async fn malformed_payload_surfaces_runtime_error() {
    let handler = form_action_handler::<ValidatedLogin, _, _>(
        "submit_login",
        |_ctx, _form: ValidatedLogin, _slots| async move {
            panic!("application handler must not run on malformed input");
        },
    );

    let envelope = ActionEnvelope {
        action_id: form_action_id("submit_login"),
        event_kind: 2,
        payload: b"<<<not json>>>".to_vec(),
    };

    let result = handler.handle(&ctx(), &envelope, slots()).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("malformed form payload"),
        "error should mention the malformed-payload variant, got: {err}"
    );
}

#[tokio::test]
async fn action_id_round_trips_with_compiler_side_allocator() {
    // The server-side `form_action_id` and the compiler-side
    // `allocate_form_action_id` must produce identical u32s so the
    // wire is consistent. Cover both directions here so a future
    // hash-family change can't drift one side without breaking this.
    let name = "submit_login";
    let server_side = form_action_id(name);
    let compiler_side =
        dom_render_compiler::transforms::form::allocate_form_action_id(name);
    assert_eq!(server_side, compiler_side);
}
