//! Phase K · end-to-end demo gate.
//!
//! Proves that the bridge from `dom_render_compiler::runtime::CompiledProject`
//! to `albedo-server`'s `ActionRegistry` closes the loop: bakabox's
//! `POST /_albedo/action` body decodes to an `ActionEnvelope`, the
//! dispatcher routes by `action_id` (the `proxy_id` baked into a
//! `BindEvent` opcode at render time), the compiled handler body runs
//! server-side via the shared Phase-J interpreter, slot writes
//! surface as `SlotSet` opcodes in the wire response, and bakabox
//! can re-apply.
//!
//! When this test passes, the Phase-K plan-level demo is structurally
//! satisfied: "Counter increments. Click button, value goes from 0
//! → 1. Refresh, value persists (slot table)." The browser side is
//! Phase L+M's full hydration story; the wire substrate this test
//! exercises is the part Phase K is responsible for.

use albedo_server::{AlbedoServerBuilder, AppConfig};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
use dom_render_compiler::ir::opcode::{Instruction, SlotId};
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::eval::{render_entry_with_bindings, RenderOptions};
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::{SessionSlotView, SlotStore};
use dom_render_compiler::runtime::CompiledProject;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const MAX_BODY: usize = 1024 * 1024;

fn counter_fixture() -> PathBuf {
    // Reach the workspace-root fixture directory from inside the
    // server crate (two parent() hops back to the root).
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join("counter")
}

#[tokio::test]
async fn compiled_counter_dispatch_increments_slot_through_http_action_route() {
    // ── 1. Compile the counter fixture and share via Arc ─────────────
    let project = Arc::new(
        CompiledProject::load_from_dir(counter_fixture())
            .expect("counter fixture compiles"),
    );

    // ── 2. Discover the proxy_id and slot_id by rendering once ───────
    //
    // The render emits BindEvent { proxy_id, ... } and SetTextRef
    // { slot_id, ... } — we read these out so the test doesn't need to
    // re-derive them from the FNV-1a-32 allocator. Using a fresh slot
    // store so this render is independent of the dispatch sequence
    // below.
    let render_store = Arc::new(SlotStore::new());
    let render_session = SessionId::random();
    let render_view = SessionSlotView::new(render_session, render_store.clone());
    let render = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &render_view,
        &RenderOptions { hook_compile: true },
    )
    .expect("counter renders");

    let proxy_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("render emits a BindEvent for the counter button");
    let slot_id: SlotId = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("render emits a SetTextRef for {n}");
    assert!(
        render
            .opcodes
            .iter()
            .any(|op| matches!(op, Instruction::InitInternTable { .. })),
        "render must prepend an InitInternTable for event ids; got: {:?}",
        render.opcodes,
    );

    // ── 3. Build a server that registers the compiled project ────────
    //
    // No routes or layouts — we're only exercising the
    // `/_albedo/action` POST path that Phase G ships and Phase K
    // hooks into via `register_compiled_project`.
    let config = AppConfig {
        server: Default::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    };
    let server = AlbedoServerBuilder::new(config)
        .register_compiled_project(project.clone())
        .build()
        .expect("server builds with compiled project registered");

    // ── 4. POST the action envelope, decode the response frame ──────
    let body_bytes = encode_action_envelope(&ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    })
    .expect("envelope encodes");

    let response = server
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/_albedo/action")
                .body(Body::from(body_bytes))
                .unwrap(),
        )
        .await
        .expect("router handles the POST");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "compiled action dispatch must return 200 OK"
    );
    let response_bytes = to_bytes(response.into_body(), MAX_BODY)
        .await
        .expect("body bytes drain");
    let (frame, _) = decode_frame(&response_bytes).expect("response decodes as OpcodeFrame");

    // ── 5. Assert the response carries the SlotSet for slot_id ──────
    let slot_set: Vec<_> = frame
        .instructions
        .iter()
        .filter_map(|op| match op {
            Instruction::SlotSet { slot_id: s, value } if *s == slot_id => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(
        slot_set.len(),
        1,
        "response must contain exactly one SlotSet for slot_id {slot_id:?}; got: {:?}",
        frame.instructions,
    );

    // ── 6. Decode the SlotSet value and confirm n incremented 0→1 ───
    let written: Value = serde_json::from_slice(slot_set[0]).expect("slot value decodes");
    let n = written.as_f64().expect("slot value is numeric");
    assert_eq!(
        n as i64, 1,
        "POST /_albedo/action must execute the handler body `setN(n + 1)` against the slot store; expected 1, got {n}",
    );
}

#[tokio::test]
async fn compiled_counter_persists_across_two_action_invocations() {
    // The session-state guarantee Phase K ships: the same session
    // sees incremented values across sequential POSTs because the
    // SlotStore inside the server is shared across requests, and
    // the same `x-albedo-session` header scopes both POSTs to the
    // same `(session, slot)` key.
    let project = Arc::new(
        CompiledProject::load_from_dir(counter_fixture())
            .expect("counter fixture compiles"),
    );

    // Render once to discover the proxy_id and slot_id baked into the
    // BindEvent / SetTextRef opcodes. The render itself runs against a
    // throwaway slot store so its first-render initialisation doesn't
    // leak into the server's slot store below.
    let render_view = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    let render = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &render_view,
        &RenderOptions { hook_compile: true },
    )
    .expect("counter renders");
    let proxy_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("BindEvent proxy_id");
    let slot_id: SlotId = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("SetTextRef slot_id");

    let config = AppConfig {
        server: Default::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    };
    let server = AlbedoServerBuilder::new(config)
        .register_compiled_project(project.clone())
        .build()
        .expect("server builds");

    // Bakabox sets `x-albedo-session` on every action POST so the
    // dispatcher scopes `SessionSlots` to the same UUID across the
    // session's lifetime. Pin one here so the three increments share
    // a slot.
    let session_uuid = uuid::Uuid::new_v4().to_string();
    let router = server.router();

    for expected in [1, 2, 3] {
        let body = encode_action_envelope(&ActionEnvelope {
            action_id: proxy_id,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "increment #{expected} must return 200"
        );
        let bytes = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
        let (frame, _) = decode_frame(&bytes).expect("decode frame");

        let value = frame
            .instructions
            .iter()
            .find_map(|op| match op {
                Instruction::SlotSet { slot_id: s, value } if *s == slot_id => Some(value),
                _ => None,
            })
            .unwrap_or_else(|| panic!(
                "increment #{expected} must emit a SlotSet; got: {:?}",
                frame.instructions
            ));
        let n: Value = serde_json::from_slice(value).unwrap();
        assert_eq!(
            n.as_f64().unwrap() as i64,
            expected,
            "increment #{expected} should yield n = {expected}"
        );
    }
}
