//! Phase-G — HTTP handler that decodes an [`ActionEnvelope`], dispatches
//! to the registered [`ActionHandler`], and ships the resulting opcode
//! patches back as a binary [`OpcodeFrame`].
//!
//! Wire: `POST /_albedo/action`. Body = bincode `ActionEnvelope`.
//! Response = bincode `OpcodeFrame` (the same shape bakabox already
//! decodes for the WT patches stream).
//!
//! # CSRF coverage, stated plainly
//!
//! **Every** action must present a valid per-session token — form,
//! click, and input alike. The token reaches the server by one of two
//! channels:
//!
//! * the [`CSRF_HEADER`] request header, which the client runtime
//!   attaches to every action POST. This is the only channel click and
//!   input actions have: their bincode payload carries no field for a
//!   token, but a header rides alongside any body.
//! * the `_csrf` field of a form submit's JSON payload — the hidden
//!   input the renderers stamp and the streaming handler fills.
//!
//! The header is consulted first; a form without it falls back to its
//! payload field. Whichever channel supplies the token, it is validated
//! against the session, and **no token on either channel is a 403** —
//! fail closed for click/input exactly as for forms. Click/input used
//! to sail through ungated because they could not carry a token; now
//! that the runtime attaches the header, they can and must.
//!
//! The token is safe to hand to same-origin JavaScript (the runtime
//! reads it from `globalThis.__ALBEDO_CSRF__`): it is not the session
//! secret — that stays in the `HttpOnly` `albedo-session` cookie — and
//! it is already in the DOM as every form's hidden `_csrf` input. The
//! same-origin policy is what keeps a cross-site page from reading it,
//! which is the exact threat a CSRF token guards against.

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

/// The set of `action_id`s reachable from a `<form action="action:NAME">`,
/// and therefore required to present a valid CSRF token.
///
/// Assembled at registration time from the server's own knowledge — the
/// compiled project's form extracts plus every explicit
/// `register_form_action` — so membership is decided before any request
/// is served and cannot be influenced by one.
pub type FormActionIds = std::collections::HashSet<u32>;

/// Request header carrying the per-session CSRF token on every action
/// POST. The client runtime reads the token from
/// `globalThis.__ALBEDO_CSRF__` (published by the streaming shell) and
/// attaches it here — the only token channel click/input actions have,
/// since their bincode payload has no field to carry one.
///
/// Mirrored client-side as the literal `x-albedo-csrf` in
/// `assets/albedo-runtime.js` and `assets/albedo-link-forms.js`. Header
/// names are case-insensitive and [`RequestContext`] lowercases them, so
/// this is spelled lowercase to match the lookup key directly.
pub const CSRF_HEADER: &str = "x-albedo-csrf";

/// Runs the action HTTP path. Decodes the envelope, enforces the CSRF
/// gate, looks up the handler by `action_id`, invokes it, and
/// wire-encodes the returned instructions as an [`OpcodeFrame`] (no
/// `component_id`).
///
/// Error mapping:
/// - Malformed body → 400 with a short text reason
/// - Missing or mismatched CSRF on a form action → 403 with `csrf` reason
/// - Unknown `action_id` → 404
/// - Handler error → 500 with the underlying message
///
/// The success response is `200` with
/// `content-type: application/octet-stream`; bakabox's client
/// dispatcher feeds the bytes straight into `applyFrameBytes`.
pub async fn run_action_request(
    registry: &ActionRegistry,
    csrf: &CsrfRegistry,
    form_actions: &FormActionIds,
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

    // Phase L · CSRF gate.
    //
    // Every action must present a valid per-session token. It arrives on
    // one of two channels — the `x-albedo-csrf` header the client runtime
    // attaches to every POST, or the `_csrf` field a form submit carries
    // in its JSON payload. The header is checked first so click/input
    // actions (whose payloads have no field for a token) are covered by
    // the same rule as forms; a form without the header falls back to its
    // payload field.
    //
    // The requirement is emphatically NOT inferred from the payload
    // shape. The gate once ran only `if the payload carried a _csrf
    // field`, which asked the caller to volunteer the evidence used to
    // judge it: omitting the field skipped the check entirely, and a
    // renderer that forgot to emit the input (as the QuickJS/Tier-B path
    // did) produced submissions that sailed through and looked normal.
    // `form_actions` is consulted only to phrase the rejection precisely
    // — it no longer decides whether the check runs, because the check
    // always runs.
    let presented = ctx
        .headers
        .get(CSRF_HEADER)
        .cloned()
        .or_else(|| extract_csrf_field(&envelope.payload));
    match presented {
        Some(token) => {
            if let Err(err) = csrf.validate(slots.session_id(), &token) {
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
        // Fail closed. No token on either channel is a forged cross-site
        // submit, a client that hasn't attached the header, or a renderer
        // bug — all 403s, and the last two are ones we want loud rather
        // than silently accepted.
        None => {
            let kind = if form_actions.contains(&envelope.action_id) {
                "form action"
            } else {
                "action"
            };
            warn!(
                action_id = envelope.action_id,
                session = %slots.session_id(),
                "{kind} submitted without a CSRF token; rejecting",
            );
            return error_response(
                StatusCode::FORBIDDEN,
                format!(
                    "CSRF validation failed: {kind} {} carried no token \
                     (`{}` header or `{}` payload field)",
                    envelope.action_id,
                    CSRF_HEADER,
                    crate::render::csrf::CSRF_FIELD_NAME,
                ),
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
///     object, or is an object that doesn't carry `_csrf`.
///
/// `None` means "no token was presented" and nothing more. It used to
/// double as "so this isn't a form action, skip the check", which made
/// the extractor's leniency load-bearing for security — a caller could
/// silence the gate by sending bytes this can't parse. Whether a token
/// is *required* is now decided by the caller-independent
/// `FormActionIds` set, leaving this function to answer only the
/// narrow question it can actually answer.
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

    /// A request context carrying the `x-albedo-csrf` header — the
    /// channel the client runtime uses for click/input actions. Keyed
    /// lowercase because [`RequestContext`] normalises header names that
    /// way (see `normalize_headers`).
    fn ctx_with_csrf_header(token: &str) -> RequestContext {
        let mut ctx = ctx();
        ctx.headers.insert(CSRF_HEADER.to_string(), token.to_string());
        ctx
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

    /// A server with no form actions registered. `form_actions` no
    /// longer decides whether the gate runs — every action needs a token
    /// now — so this only affects how a rejection is *phrased*, not
    /// whether one happens.
    fn no_form_actions() -> FormActionIds {
        FormActionIds::new()
    }

    /// The dispatcher tests below don't exercise CSRF, but every action
    /// must now clear the gate, so they need a valid token. Mint one for
    /// a fresh session in `reg` and return a context presenting it in the
    /// header plus a slots view on the same session, so `validate`
    /// matches. This is the server-side mirror of what the browser does
    /// with the shell's `__ALBEDO_CSRF__` global.
    fn authed(reg: &CsrfRegistry) -> (RequestContext, SessionSlots) {
        let session = SessionId::random();
        let ctx = ctx_with_csrf_header(&reg.token_for(session));
        (ctx, slots_for(session))
    }

    /// A server that knows `action_id` is submitted to by a form, and
    /// therefore demands a valid token from it.
    fn form_actions(ids: &[u32]) -> FormActionIds {
        ids.iter().copied().collect()
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

        let reg = csrf();
        let (ctx, slots) = authed(&reg);
        let response = run_action_request(
            &registry,
            &reg,
            &no_form_actions(),
            ctx,
            body,
            slots,
            None,
        )
        .await;
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
            &no_form_actions(),
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
        let reg = csrf();
        let (ctx, slots) = authed(&reg);
        let response = run_action_request(
            &registry,
            &reg,
            &no_form_actions(),
            ctx,
            body,
            slots,
            None,
        )
        .await;
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
        let reg = csrf();
        let (ctx, slots) = authed(&reg);
        let response = run_action_request(
            &registry,
            &reg,
            &no_form_actions(),
            ctx,
            body,
            slots,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn slot_writes_inside_handler_surface_as_slot_set_in_response() {
        // Phase-H closing the reactive loop: handler writes a slot →
        // dispatcher drains the dirty set after handle() returns →
        // SlotSet opcode is appended to the response wire frame.
        let store = Arc::new(SlotStore::new());
        let session = SessionId::random();
        let view = SessionSlots::new(session, store);

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

        let reg = csrf();
        let ctx = ctx_with_csrf_header(&reg.token_for(session));
        let response = run_action_request(
            &registry,
            &reg,
            &no_form_actions(),
            ctx,
            body,
            view,
            None,
        )
        .await;
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
            &form_actions(&[99]),
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
            &form_actions(&[99]),
            ctx(),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// THE regression test for closing the click/input gap. A click
    /// action (no form submits to it) that presents NO token on either
    /// channel used to dispatch — its payload had no field to carry one,
    /// so the gate let it through. Now the client attaches the token as a
    /// header, so a tokenless click is a forged or misbehaving caller and
    /// must 403. The handler panics if it runs, so this cannot pass
    /// vacuously.
    #[tokio::test]
    async fn click_action_without_any_token_is_rejected() {
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let _token = csrf_registry.token_for(session);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                panic!("handler must not run for a click action with no CSRF token");
            },
        );
        registry.insert(5, handler);

        let payload = serde_json::to_vec(&serde_json::json!({ "user": "alice" })).unwrap();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 5,
                event_kind: 0, // Click — no form payload, no header either
                payload,
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            &no_form_actions(),
            ctx(),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn click_action_with_valid_header_token_dispatches() {
        // The header channel: a click action carrying a valid token in
        // `x-albedo-csrf` (and no `_csrf` payload field) must dispatch.
        // This is exactly what the client runtime emits for a
        // BindEvent-wired click after the shell published the token.
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let token = csrf_registry.token_for(session);

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

        // A click payload with no `_csrf` field — the token rides the header.
        let payload = serde_json::to_vec(&serde_json::json!({ "user": "alice" })).unwrap();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 5,
                event_kind: 0,
                payload,
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            &no_form_actions(),
            ctx_with_csrf_header(&token),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn click_action_with_invalid_header_token_returns_403() {
        // A wrong token in the header must 403, whatever the payload.
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let _real = csrf_registry.token_for(session);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                panic!("handler must not run on a bad header token");
            },
        );
        registry.insert(5, handler);

        let payload = serde_json::to_vec(&serde_json::json!({ "user": "alice" })).unwrap();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 5,
                event_kind: 0,
                payload,
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            &no_form_actions(),
            ctx_with_csrf_header("00000000000000000000000000000000"),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn header_token_is_honoured_when_the_payload_field_is_absent_on_a_form() {
        // A form action whose payload lost its `_csrf` field but whose
        // POST still carried the header must dispatch — the header is the
        // fallback that keeps a form working even if its hidden input
        // never rendered. Proves the two channels are OR'd, header-first.
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let token = csrf_registry.token_for(session);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(99),
                }])
            },
        );
        registry.insert(99, handler);

        let payload = serde_json::to_vec(&serde_json::json!({ "user": "alice" })).unwrap();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 99,
                event_kind: 2,
                payload,
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            &form_actions(&[99]),
            ctx_with_csrf_header(&token),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// THE regression test for the bug this gate was rebuilt around.
    ///
    /// A form action arriving with no `_csrf` field at all is exactly
    /// what a Tier-B/QuickJS-rendered form used to send, because that
    /// renderer emitted no CSRF input — and the old gate, which ran only
    /// when a token was present, dispatched it. The handler panics if it
    /// runs, so this cannot pass vacuously.
    #[tokio::test]
    async fn form_action_without_a_csrf_field_is_rejected() {
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();
        let _token = csrf_registry.token_for(session);

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                panic!("handler must not run for a form action with no CSRF token");
            },
        );
        registry.insert(99, handler);

        // A well-formed form payload — just missing the token.
        let payload = serde_json::to_vec(&serde_json::json!({ "user": "alice" })).unwrap();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 99,
                event_kind: 2, // Submit
                payload,
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            &form_actions(&[99]),
            ctx(),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// The gate must not be dodgeable by changing the payload's *shape*.
    /// `extract_csrf_field` returns `None` for anything that isn't a JSON
    /// object, so a caller could previously skip the check by sending
    /// bytes the extractor can't read. Membership in `form_actions` is
    /// what decides, so this is still a 403.
    #[tokio::test]
    async fn form_action_cannot_dodge_the_gate_with_a_non_json_payload() {
        let csrf_registry = CsrfRegistry::new();
        let session = SessionId::random();

        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                panic!("handler must not run for an ungated form action");
            },
        );
        registry.insert(99, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 99,
                event_kind: 0, // claiming to be a click changes nothing
                payload: vec![0xff, 0x00, 0x12, 0x34],
            })
            .unwrap(),
        );

        let response = run_action_request(
            &registry,
            &csrf_registry,
            &form_actions(&[99]),
            ctx(),
            body,
            slots_for(session),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
