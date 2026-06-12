//! Phase-G — HTTP handler that decodes an [`ActionEnvelope`], dispatches
//! to the registered [`ActionHandler`], and ships the resulting opcode
//! patches back as a binary [`OpcodeFrame`].
//!
//! Wire: `POST /_albedo/action`. Body = bincode `ActionEnvelope`.
//! Response = bincode `OpcodeFrame` (the same shape bakabox already
//! decodes for the WT patches stream).

use crate::actions::{ActionHandler, SessionSlots};
use crate::error::RuntimeError;
use crate::lifecycle::RequestContext;
use crate::render::csrf::CsrfRegistry;
use axum::body::Body;
use axum::http::{header, Response, StatusCode};
use bytes::Bytes;
use dom_render_compiler::ir::action::decode_action_envelope;
use dom_render_compiler::ir::opcode::OpcodeFrame;
use dom_render_compiler::ir::wire::encode_frame;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

/// Lookup table from `action_id` → handler. The dispatcher reads from
/// this; the server builder fills it via
/// [`crate::AlbedoServerBuilder::register_action`].
pub type ActionRegistry = HashMap<u32, Arc<dyn ActionHandler>>;

/// Runs the action HTTP path. Decodes the envelope, validates CSRF
/// for form-shaped payloads, looks up the handler by `action_id`,
/// invokes it, and wire-encodes the returned instructions as an
/// [`OpcodeFrame`] (no `component_id`).
///
/// Error mapping:
/// - Malformed body → 400 with a short text reason
/// - CSRF mismatch → 403 with `csrf` reason
/// - Unknown `action_id` → 404
/// - Handler error → 500 with the underlying message
///
/// The success response is `200` with
/// `content-type: application/octet-stream`; bakabox's client
/// dispatcher feeds the bytes straight into `applyFrameBytes`.
pub async fn run_action_request(
    registry: &ActionRegistry,
    csrf: &CsrfRegistry,
    ctx: RequestContext,
    body: Bytes,
    slots: SessionSlots,
    overlay: Option<&crate::dev::SharedErrorRegistry>,
) -> Response<Body> {
    let (envelope, _consumed) = match decode_action_envelope(body.as_ref()) {
        Ok(value) => value,
        Err(err) => {
            warn!(error = %err, "rejecting malformed action envelope");
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid action envelope: {err}"),
            );
        }
    };

    // Phase L · CSRF gate. Form submissions carry a `_csrf` field in
    // their JSON payload (the renderer injects the input via the
    // `<form action="action:NAME">` rewrite path; the server's
    // post-render middleware fills in the per-session token). Actions
    // whose payload is not a JSON object — or that don't carry a
    // `_csrf` field — skip the check. This keeps button-click
    // actions and other non-form shapes on the wire unchanged.
    if let Some(presented) = extract_csrf_field(&envelope.payload) {
        if let Err(err) = csrf.validate(slots.session_id(), &presented) {
            warn!(
                action_id = envelope.action_id,
                session = %slots.session_id(),
                error = %err,
                "CSRF validation failed",
            );
            return error_response(
                StatusCode::FORBIDDEN,
                format!("CSRF validation failed: {err}"),
            );
        }
    }

    let handler = match registry.get(&envelope.action_id) {
        Some(handler) => handler.clone(),
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("no handler registered for action_id {}", envelope.action_id),
            );
        }
    };

    let mut instructions = match handler.handle(&ctx, &envelope, slots.clone()).await {
        Ok(out) => out,
        Err(err) => {
            warn!(action_id = envelope.action_id, error = %err, "action handler failed");
            if let Some(reg) = overlay {
                reg.report_action(format!("action {} failed: {err}", envelope.action_id));
            }
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                err.to_string(),
            );
        }
    };

    // Phase-H — append any `SlotSet` opcodes the handler triggered
    // via `slots.write`. Drain is best-effort: a poisoned mutex
    // returns an empty vec, so the handler's explicit response still
    // ships even if the slot store is in a bad state.
    instructions.extend(slots.drain_pending());

    // Action responses don't share frame_ids with the WT patches
    // stream — each is a self-contained `OpcodeFrame` per the Phase-D
    // wire amendment. The `frame_id` here is `0`: bakabox's
    // `applyFrameBytes` doesn't currently key on frame_id for HTTP
    // responses (no multi-message reassembly), so the value is
    // bookkeeping only.
    let frame = OpcodeFrame {
        frame_id: 0,
        component_id: None,
        instructions,
    };
    let bytes = match encode_frame(&frame) {
        Ok(bytes) => bytes,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to encode action response: {err}"),
            );
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("cache-control", "no-store")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Centralises the error response shape so every failure mode in
/// `run_action_request` looks the same on the wire. Plain-text body
/// keeps debugging cheap; the client surfaces non-200 by logging the
/// status + body and dropping the result.
fn error_response(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Body::from(message))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Convenience wrapper so the [`crate::error::RuntimeError`] surface
/// stays the canonical failure type for action handlers. Currently
/// unused outside `RuntimeError::Authentication` mapping — the
/// dispatcher converts every error variant uniformly to 500 in this
/// Phase-G MVP.
#[allow(dead_code)]
pub(crate) fn status_for_error(err: &RuntimeError) -> StatusCode {
    match err {
        RuntimeError::Authentication(_) => StatusCode::UNAUTHORIZED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Try to read a `_csrf` string field out of a JSON-encoded payload.
///
/// Returns:
///   * `Some(token)` when the payload parses as a JSON object that
///     carries a string-valued `_csrf` field.
///   * `None` when the payload is not JSON, is JSON but not an
///     object, or is an object that doesn't carry `_csrf`. The
///     dispatcher treats `None` as "not a form action, skip the CSRF
///     check" — button-click actions and other non-form shapes never
///     have to opt in.
///
/// Deliberately lenient on the parse: a bincode payload (or any
/// non-JSON bytes) returns `None` rather than an error, so non-form
/// actions never trip on a JSON parser they were never expected to
/// satisfy.
fn extract_csrf_field(payload: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let object = value.as_object()?;
    let field = object.get(crate::render::csrf::CSRF_FIELD_NAME)?;
    field.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{ActionHandler, SessionSlots};
    use axum::body::to_bytes;
    use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
    use dom_render_compiler::ir::opcode::{Instruction, SlotId, StableId, TagId};
    use dom_render_compiler::ir::wire::decode_frame;
    use dom_render_compiler::runtime::{SessionId, SlotStore};

    fn ctx() -> RequestContext {
        RequestContext {
            request_id: "t".into(),
            method: crate::routing::HttpMethod::Post,
            path: "/_albedo/action".into(),
            query: Default::default(),
            params: Default::default(),
            headers: Default::default(),
            body: Bytes::new(),
            metadata: Default::default(),
        }
    }

    fn slots() -> SessionSlots {
        SessionSlots::new(SessionId::random(), Arc::new(SlotStore::new()))
    }

    /// Fresh empty CSRF registry — every existing dispatcher test
    /// uses payloads that either aren't JSON or don't carry a
    /// `_csrf` field, so the validate path is effectively a no-op
    /// for them. The CSRF-specific tests below mint their own
    /// registry to exercise the validate path explicitly.
    fn csrf() -> CsrfRegistry {
        CsrfRegistry::new()
    }

    async fn body_bytes(resp: Response<Body>) -> Bytes {
        to_bytes(resp.into_body(), 1024 * 1024).await.unwrap()
    }

    #[tokio::test]
    async fn dispatches_to_registered_handler_and_returns_wire_encoded_frame() {
        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, env: ActionEnvelope, _slots: SessionSlots| async move {
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(env.action_id),
                }])
            },
        );
        registry.insert(42, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 42,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap(),
        );

        let response = run_action_request(&registry, &csrf(), ctx(), body, slots(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/octet-stream"),
        );

        let bytes = body_bytes(response).await;
        let (frame, consumed) = decode_frame(&bytes).expect("frame decodes");
        assert_eq!(consumed, bytes.len());
        assert!(matches!(
            frame.instructions[0],
            Instruction::Create { stable_id: StableId(42), .. }
        ));
    }

    #[tokio::test]
    async fn malformed_body_returns_400() {
        let registry: ActionRegistry = HashMap::new();
        let response = run_action_request(
            &registry,
            &csrf(),
            ctx(),
            Bytes::from_static(&[0xff, 0xff, 0xff, 0xff]),
            slots(),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_action_id_returns_404() {
        let registry: ActionRegistry = HashMap::new();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 7,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap(),
        );
        let response = run_action_request(&registry, &csrf(), ctx(), body, slots(), None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn handler_error_returns_500() {
        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                Err(RuntimeError::RequestHandling("boom".into()))
            },
        );
        registry.insert(1, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 1,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap(),
        );
        let response = run_action_request(&registry, &csrf(), ctx(), body, slots(), None).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn slot_writes_inside_handler_surface_as_slot_set_in_response() {
        // Phase-H closing the reactive loop: handler writes a slot →
        // dispatcher drains the dirty set after handle() returns →
        // SlotSet opcode is appended to the response wire frame.
        let store = Arc::new(SlotStore::new());
        let view = SessionSlots::new(SessionId::random(), store);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, slots: SessionSlots| async move {
                slots.write(SlotId(7), b"42".to_vec());
                Ok(Vec::new())
            },
        );
        registry.insert(1, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 1,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap(),
        );

        let response = run_action_request(&registry, &csrf(), ctx(), body, view, None).await;
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = body_bytes(response).await;
        let (frame, _) = decode_frame(&bytes).expect("frame decodes");
        assert_eq!(
            frame.instructions.len(),
            1,
            "handler returned no explicit opcodes; only the SlotSet should ship",
        );
        match &frame.instructions[0] {
            Instruction::SlotSet { slot_id, value } => {
                assert_eq!(*slot_id, SlotId(7));
                assert_eq!(value, b"42");
            }
            other => panic!("expected SlotSet, got {other:?}"),
        }
    }

    /// Build a SessionSlots view bound to a known session id so the
    /// CSRF tests below can mint a token against the same session.
    fn slots_for(session: SessionId) -> SessionSlots {
        SessionSlots::new(session, Arc::new(SlotStore::new()))
    }

    /// Wrap a JSON object containing a `_csrf` field (plus any extra
    /// shape the caller wants) into the wire form bakabox emits for
    /// a `<form>` submit.
    fn json_payload_with_csrf(token: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "_csrf": token,
            "user": "alice",
        }))
        .expect("payload encodes")
    }

    #[tokio::test]
    async fn csrf_field_with_valid_token_dispatches_normally() {
        // Mint a token bound to a known session, then submit a
        // form-shaped payload carrying that exact token. The
        // dispatcher must accept and run the handler.
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let token = csrf_registry.token_for(session);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(1),
                }])
            },
        );
        registry.insert(99, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 99,
                event_kind: 2, // Submit
                payload: json_payload_with_csrf(&token),
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            ctx(),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn csrf_field_with_invalid_token_returns_403() {
        // Mint a token, then submit with a different token. The
        // dispatcher must return 403 and the registered handler
        // must not run.
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let _real_token = csrf_registry.token_for(session);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                panic!("handler must not run on CSRF mismatch");
            },
        );
        registry.insert(99, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 99,
                event_kind: 2,
                payload: json_payload_with_csrf("00000000000000000000000000000000"),
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            ctx(),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn csrf_field_absent_skips_validation_and_dispatches() {
        // JSON object payload WITHOUT a `_csrf` field — non-form
        // action shape. Must dispatch normally, validate path
        // skipped. Registry has a session token but the request
        // doesn't present one, so the check must not run.
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let _token = csrf_registry.token_for(session);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(5),
                }])
            },
        );
        registry.insert(5, handler);

        let payload = serde_json::to_vec(&serde_json::json!({ "user": "alice" })).unwrap();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 5,
                event_kind: 0, // Click — no form payload
                payload,
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            ctx(),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn csrf_extractor_returns_none_for_non_json_payload() {
        // Non-JSON bytes (a bincode envelope is the typical case)
        // must return None — the dispatcher then skips the check
        // and routes the action normally. Direct unit test on the
        // extractor so the parse-leniency contract stays explicit.
        assert!(super::extract_csrf_field(&[0xff, 0x00, 0x12, 0x34]).is_none());
        // JSON that isn't an object — array, string, number — also
        // returns None.
        assert!(super::extract_csrf_field(br#"[1,2,3]"#).is_none());
        assert!(super::extract_csrf_field(br#""bare string""#).is_none());
    }
}
