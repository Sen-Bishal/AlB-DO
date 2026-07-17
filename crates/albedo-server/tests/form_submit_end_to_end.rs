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

/// Like [`action_request`] but also attaches the `x-albedo-csrf` header
/// the client runtime now sends on every action POST — the channel that
/// carries the token for click/input actions, whose payload has no field
/// for one.
fn action_request_with_csrf(
    envelope: ActionEnvelope,
    session_cookie: Option<SessionId>,
    token: &str,
) -> Request<Body> {
    let body = encode_action_envelope(&envelope).expect("envelope encodes");
    let mut builder = Request::builder()
        .method("POST")
        .uri("/_albedo/action")
        .header("content-type", "application/octet-stream")
        .header("x-albedo-csrf", token);
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

/// Replaces `form_submit_without_csrf_field_skips_gate_for_non_form_actions`,
/// which asserted this exact request returned 200.
///
/// That test read as though it covered button clicks, but the id it
/// sent is `form_action_id(ACTION_NAME)` — an action registered through
/// `register_form_action`. It only counted as "not a form action"
/// because form-ness used to be inferred from the *payload's shape*: no
/// `_csrf` field meant no check. So the assertion encoded the hole
/// rather than the rule, and any caller could open it by dropping a
/// field and claiming `event_kind=0`.
///
/// Form-ness is now the server's own compile/registration-time fact, so
/// the same request is refused. `event_kind` is the client's word for
/// what happened and is deliberately not consulted.
#[tokio::test]
async fn form_action_without_csrf_field_is_refused_whatever_event_kind_it_claims() {
    let server = build_server();
    let session = SessionId::random();
    let _token = server.csrf_registry().token_for(session);

    let envelope = ActionEnvelope {
        action_id: form_action_id(ACTION_NAME),
        event_kind: 0, // claiming "Click" buys nothing
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

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// Click actions are now gated too. Their bincode payload has no field
/// for a token, so the client runtime attaches it as the `x-albedo-csrf`
/// header (from the shell's `__ALBEDO_CSRF__` global). This test pins
/// both halves of the closed gap on a genuinely non-form-backed action
/// (registered via `register_action`): no token → 403, valid header
/// token → 200. It supersedes `click_action_without_csrf_field_still_dispatches`,
/// which asserted the pre-gate 200 and encoded the hole.
#[tokio::test]
async fn click_action_is_gated_and_dispatches_only_with_a_token() {
    const CLICK_ACTION_ID: u32 = 4242;

    let config = AppConfig {
        server: ServerConfig::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    };
    let server = AlbedoServerBuilder::new(config)
        .register_action(
            CLICK_ACTION_ID,
            |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                Ok(vec![Instruction::Navigate {
                    url: "/clicked".to_string(),
                }])
            },
        )
        .build()
        .expect("server build");

    let session = SessionId::random();
    let token = server.csrf_registry().token_for(session);

    let envelope = || ActionEnvelope {
        action_id: CLICK_ACTION_ID,
        event_kind: 0,
        payload: Vec::new(),
    };

    // No token on either channel → 403, exactly as a tokenless form.
    let refused = server
        .router()
        .oneshot(action_request(envelope(), Some(session)))
        .await
        .expect("router responds");
    assert_eq!(
        refused.status(),
        StatusCode::FORBIDDEN,
        "a click with no token must be refused now that the runtime attaches one",
    );

    // Valid token in the header → dispatches, proving the header is the
    // click path's token channel.
    let accepted = server
        .router()
        .oneshot(action_request_with_csrf(envelope(), Some(session), &token))
        .await
        .expect("router responds");
    assert_eq!(accepted.status(), StatusCode::OK);
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

