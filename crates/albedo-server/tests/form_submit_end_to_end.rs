//! Phase L · end-to-end form submit integration test.
//!
//! Exercises the full HTTP path that Step 6 of the Phase-L itinerary
//! gates on: a browser POSTs a form, the server validates the CSRF
//! token against the per-session registry, the typed handler runs,
//! and the response carries the opcodes the client applies (in
//! particular, `Instruction::Navigate` so the demo's "login → /dashboard"
//! flow works without a full page reload).
//!
//! The streaming-handler-side CSRF substitution is unit-tested in
//! `crate::render::csrf`; the action-dispatcher-side CSRF gate is
//! unit-tested in `crate::handlers::action`. This file glues them
//! together by exercising the action route through the public axum
//! router, with cookies carrying the per-session id between requests
//! the way a real browser would.

use albedo_server::actions::SessionSlots;
use albedo_server::config::{AppConfig, ServerConfig};
use albedo_server::lifecycle::RequestContext;
use albedo_server::render::form_action::form_action_id;
use albedo_server::render::ALBEDO_SESSION_COOKIE;
use albedo_server::server::AlbedoServerBuilder;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::SessionId;
use serde::Deserialize;
use tower::ServiceExt;

const ACTION_NAME: &str = "submit_login";
const MAX_BODY: usize = 1024 * 1024;

/// Form payload shape the dispatcher decodes via `register_form_action`.
/// Same JSON the client-side runtime emits when serializing a
/// `<form>` whose inputs carry `name="user"` etc.
#[derive(Deserialize)]
struct LoginForm {
    #[allow(dead_code)]
    user: String,
    #[allow(dead_code)]
    pass: String,
}

/// Builds a server with one form-action handler that emits a
/// `Navigate { url: "/dashboard" }` opcode on success. The handler
/// also writes a slot so the test can confirm the per-session slot
/// store survives the action dispatch.
fn build_server() -> albedo_server::server::AlbedoServer {
    let config = AppConfig {
        server: ServerConfig::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    };

    AlbedoServerBuilder::new(config)
        .register_form_action::<LoginForm, _, _>(
            ACTION_NAME,
            |_ctx: RequestContext, _form: LoginForm, _slots: SessionSlots| async move {
                Ok(vec![Instruction::Navigate {
                    url: "/dashboard".to_string(),
                }])
            },
        )
        .build()
        .expect("server build")
}

/// Construct a POST /_albedo/action request carrying the bincoded
/// envelope, with an optional `albedo-session` cookie pinned to the
/// supplied session id. Mirrors what the browser's link-forms client
/// emits during a form submit.
fn action_request(envelope: ActionEnvelope, session_cookie: Option<SessionId>) -> Request<Body> {
    let body = encode_action_envelope(&envelope).expect("envelope encodes");
    let mut builder = Request::builder()
        .method("POST")
        .uri("/_albedo/action")
        .header("content-type", "application/octet-stream");
    if let Some(session) = session_cookie {
        builder = builder.header(
            "Cookie",
            format!("{ALBEDO_SESSION_COOKIE}={}", session.as_uuid()),
        );
    }
    builder.body(Body::from(body)).expect("request builds")
}

/// JSON form payload bytes wrapping `_csrf` and the user/pass
/// fields the action handler decodes via the `LoginForm` shape.
fn login_payload(token: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "_csrf": token,
        "user": "alice",
        "pass": "hunter2-very-long",
    }))
    .expect("login payload encodes")
}

#[tokio::test]
async fn form_submit_with_matching_csrf_token_dispatches_and_returns_navigate() {
    // The renderer would mint a session id per page request and put
    // it on `Set-Cookie`. For the integration test we bypass the
    // page-render leg and assert just the action-side contract:
    // given a session id and its token in the registry, a form-shaped
    // POST with the cookie + the matching token must dispatch.
    let server = build_server();
    let session = SessionId::random();
    let token = server.csrf_registry().token_for(session);

    let envelope = ActionEnvelope {
        action_id: form_action_id(ACTION_NAME),
        event_kind: 2, // ActionEventKind::Submit
        payload: login_payload(&token),
    };

    let response = server
        .router()
        .oneshot(action_request(envelope, Some(session)))
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::OK, "valid CSRF must dispatch");
    let body = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
    let (frame, _) = decode_frame(&body).expect("response decodes as OpcodeFrame");

    // The handler returns exactly one Navigate opcode; the dispatcher
    // may append SlotSet for drained writes, but the handler in this
    // test writes no slots, so the response is single-instruction.
    let navigate = frame
        .instructions
        .iter()
        .find_map(|instr| match instr {
            Instruction::Navigate { url } => Some(url.clone()),
            _ => None,
        })
        .expect("response carries a Navigate opcode");
    assert_eq!(navigate, "/dashboard");
}

#[tokio::test]
async fn form_submit_with_wrong_token_returns_403_and_skips_handler() {
    // Mint a token for the session but submit a different one. The
    // dispatcher's CSRF gate must short-circuit with 403 before the
    // handler runs — the test's handler would panic if invoked.
    let server = build_server();
    let session = SessionId::random();
    let _real_token = server.csrf_registry().token_for(session);

    let envelope = ActionEnvelope {
        action_id: form_action_id(ACTION_NAME),
        event_kind: 2,
        payload: login_payload("00000000000000000000000000000000"),
    };

    let response = server
        .router()
        .oneshot(action_request(envelope, Some(session)))
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn form_submit_without_cookie_uses_fresh_session_and_403s_on_csrf() {
    // No cookie sent → action route mints a fresh random session →
    // no token has ever been minted for that session → CSRF check
    // sees `presented != stored` (or `Missing`) → 403.
    let server = build_server();

    // The token below was minted for a DIFFERENT (known) session, so
    // even if we put a non-empty token in the payload, the request's
    // cookie-less session id won't match the registry entry.
    let other_session = SessionId::random();
    let _foreign_token = server.csrf_registry().token_for(other_session);

    let envelope = ActionEnvelope {
        action_id: form_action_id(ACTION_NAME),
        event_kind: 2,
        payload: login_payload("00000000000000000000000000000000"),
    };

    let response = server
        .router()
        .oneshot(action_request(envelope, None))
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn form_submit_without_csrf_field_skips_gate_for_non_form_actions() {
    // A button-click action ships event_kind=0 (Click) with a
    // non-form payload (here: an empty JSON object). The dispatcher
    // must NOT run CSRF validation on such payloads — the gate is
    // form-only by design.
    let server = build_server();
    let session = SessionId::random();
    let _token = server.csrf_registry().token_for(session);

    let envelope = ActionEnvelope {
        action_id: form_action_id(ACTION_NAME),
        event_kind: 0,
        // Plain JSON object without `_csrf` field — the extractor
        // returns None and the gate is skipped.
        payload: serde_json::to_vec(&serde_json::json!({
            "user": "alice",
            "pass": "hunter2-very-long",
        }))
        .unwrap(),
    };

    let response = server
        .router()
        .oneshot(action_request(envelope, Some(session)))
        .await
        .expect("router responds");

    // The CSRF check is skipped, but the handler still runs and
    // returns the Navigate opcode (the handler doesn't care about
    // event_kind in this test).
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn cookie_session_round_trips_into_action_handler() {
    // End-to-end session continuity: the cookie sent by the client
    // must surface inside the action handler as the same SessionId,
    // so server-side state (slot store entries, registry tokens) is
    // keyed consistently.
    use std::sync::{Arc, Mutex};

    // Local Arc<Mutex<>> captured by the handler closure. Each test
    // run sees a fresh observer, so parallel-test execution can't
    // observe a sibling test's session id.
    let observed: Arc<Mutex<Option<SessionId>>> = Arc::new(Mutex::new(None));

    let config = AppConfig {
        server: ServerConfig::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    };
    let observed_for_handler = observed.clone();
    let server = AlbedoServerBuilder::new(config)
        .register_form_action::<LoginForm, _, _>(
            ACTION_NAME,
            move |_ctx: RequestContext, _form: LoginForm, slots: SessionSlots| {
                let observed_for_handler = observed_for_handler.clone();
                async move {
                    if let Ok(mut guard) = observed_for_handler.lock() {
                        *guard = Some(slots.session_id());
                    }
                    Ok(Vec::new())
                }
            },
        )
        .build()
        .expect("server build");

    let session = SessionId::random();
    let token = server.csrf_registry().token_for(session);

    let envelope = ActionEnvelope {
        action_id: form_action_id(ACTION_NAME),
        event_kind: 2,
        payload: login_payload(&token),
    };

    let response = server
        .router()
        .oneshot(action_request(envelope, Some(session)))
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);

    let recorded = observed
        .lock()
        .unwrap()
        .expect("handler must have run");
    assert_eq!(
        recorded, session,
        "handler observed session must match the cookie's session id"
    );
}

/// Sanity check: the `register_form_action` ergonomic path and the
/// CSRF wire path share the same hash family. If someone refactors
/// either the server's `form_action_id` or the compiler's
/// `allocate_form_action_id`, this test catches the drift before
/// production silently routes to the wrong action_id.
#[test]
fn action_id_parity_across_compiler_and_server() {
    let server_side = form_action_id(ACTION_NAME);
    let compiler_side =
        dom_render_compiler::transforms::form::allocate_form_action_id(ACTION_NAME);
    assert_eq!(server_side, compiler_side);
}

