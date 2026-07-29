//! APERTURE · A2 — the server seam.
//!
//! `tests/aperture_workflow.rs` (workspace root) proves the protocol with the
//! pass loop written *in the test*. This proves the loop that ships: a real
//! `POST /_albedo/action`, a real engine pool, a real `ApertureClient`, and a
//! handler body whose `await fetch(…)` is answered by the server rather than by
//! a driver someone wrote by hand.
//!
//! Until this file existed, every APERTURE write-path test drove the system
//! through a loop that only tests contained — which is precisely how "the
//! protocol works" and "`fetch()` works in an app" managed to be different
//! statements for a week.
//!
//! ## The two things being asserted
//!
//! 1. **The round trip closes.** The upstream's body reaches a slot, through the
//!    same wire opcode a `setState` would have used.
//! 2. **The engine is released across it** (invariant 2.6). Gate 5 measured this
//!    against a synthetic loop; here it is measured against the shipped one,
//!    which is the only version that can regress.

use albedo_server::{AlbedoServerBuilder, AppConfig};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use dom_render_compiler::aperture::{
    ApertureClient, ApertureError, EgressMode, EgressPolicy, ResponseCache, Transport, WireRequest,
    WireResponse, DEFAULT_RESPONSE_BUDGET,
};
use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::eval::{render_entry_with_bindings, RenderOptions};
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::{SessionSlotView, SlotStore};
use dom_render_compiler::runtime::CompiledProject;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

const MAX_BODY: usize = 1024 * 1024;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join(name)
}

/// A transport that answers from a script and records **how many requests were
/// in flight at the same moment**.
///
/// The peak is the whole point of the concurrency assertion below, and it is a
/// count rather than a stopwatch for the reason A0 already established: a claim
/// proved by timing is a claim that gets re-litigated on someone else's machine.
/// Under a design that holds an engine for the round trip, this number cannot
/// exceed the pool size — the engine is the thing waiting — and no amount of
/// tuning changes that.
#[derive(Debug)]
struct PeakTransport {
    body: Vec<u8>,
    delay: Duration,
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    requests: Mutex<Vec<WireRequest>>,
}

impl PeakTransport {
    fn new(body: &str, delay: Duration) -> Self {
        Self {
            body: body.as_bytes().to_vec(),
            delay,
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<WireRequest> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

#[async_trait]
impl Transport for PeakTransport {
    async fn send(&self, request: &WireRequest) -> Result<WireResponse, ApertureError> {
        self.requests
            .lock()
            .expect("requests mutex")
            .push(request.clone());
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);

        Ok(WireResponse {
            status: 200,
            body: self.body.clone(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-upstream".to_string(), "peak-transport".to_string()),
            ],
            etag: None,
            last_modified: None,
            content_type: Some("application/json".to_string()),
        })
    }
}

fn client(transport: Arc<PeakTransport>) -> Arc<ApertureClient> {
    // `Serve` rather than `Dev`: the address-class denies live in the DNS
    // resolver, which only the `reqwest` transport installs, so the strict mode
    // costs this test nothing and proves the seam does not need the loose one.
    let policy = Arc::new(EgressPolicy::new(EgressMode::Serve));
    Arc::new(ApertureClient::new(
        transport,
        Arc::new(ResponseCache::new(DEFAULT_RESPONSE_BUDGET)),
        policy,
    ))
}

fn empty_config() -> AppConfig {
    AppConfig {
        server: Default::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    }
}

/// Render once to read the ids off the opcodes, exactly as the browser would.
fn ids(project: &CompiledProject) -> (u32, dom_render_compiler::ir::opcode::SlotId) {
    let store = Arc::new(SlotStore::new());
    let view = SessionSlotView::new(SessionId::random(), store);
    let render = render_entry_with_bindings(
        project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &view,
        &RenderOptions { hook_compile: true },
    )
    .expect("fixture renders");

    let proxy_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("render emits a BindEvent");
    let slot_id = render
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("render emits a SetTextRef for {label}");
    (proxy_id, slot_id)
}

/// POST one action envelope and return the decoded response frame's opcodes.
async fn dispatch(
    server: &albedo_server::AlbedoServer,
    proxy_id: u32,
) -> (StatusCode, Vec<Instruction>) {
    let session_uuid = uuid::Uuid::new_v4().to_string();
    let csrf = server
        .csrf_registry()
        .token_for(SessionId::new(uuid::Uuid::parse_str(&session_uuid).unwrap()));
    let body = encode_action_envelope(&ActionEnvelope {
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
                .header("x-albedo-session", session_uuid.as_str())
                .header("x-albedo-csrf", csrf.as_str())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("router handles the POST");

    let status = response.status();
    if status != StatusCode::OK {
        let bytes = to_bytes(response.into_body(), MAX_BODY).await.expect("body");
        panic!(
            "action dispatch returned {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
    }
    let bytes = to_bytes(response.into_body(), MAX_BODY).await.expect("body");
    let (frame, _) = decode_frame(&bytes).expect("response decodes as an OpcodeFrame");
    (status, frame.instructions)
}

/// The headline: an `await fetch(…)` in a handler body, dispatched over HTTP,
/// reaching a slot with the upstream's value in it.
///
/// Nothing in the fixture's TSX mentions a journal, a pass, or a suspension —
/// § 5.5's "no ALBEDO dialect" is either true here or it is not true anywhere.
#[tokio::test]
async fn an_action_body_that_awaits_a_fetch_completes_over_http() {
    let project = Arc::new(
        CompiledProject::load_from_dir(fixture("fetching_handler")).expect("fixture compiles"),
    );
    let (proxy_id, slot_id) = ids(&project);

    let transport = Arc::new(PeakTransport::new(
        r#"{"state":"green"}"#,
        Duration::from_millis(1),
    ));
    let server = AlbedoServerBuilder::new(empty_config())
        // Order matters: the pool must exist before the adapters capture it.
        .with_quickjs_action_engine_pool(1)
        .with_aperture_client(client(Arc::clone(&transport)))
        .with_build_id("build-seam")
        .register_compiled_project(Arc::clone(&project))
        .build()
        .expect("server builds");

    let (status, instructions) = dispatch(&server, proxy_id).await;
    assert_eq!(status, StatusCode::OK);

    let written: Vec<&Vec<u8>> = instructions
        .iter()
        .filter_map(|op| match op {
            Instruction::SlotSet { slot_id: s, value } if *s == slot_id => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(
        written.len(),
        1,
        "one slot write for {slot_id:?}; got {instructions:?}"
    );
    assert_eq!(
        String::from_utf8(written[0].clone()).unwrap(),
        "\"green\"",
        "the upstream's value reached the slot"
    );

    let sent = transport.requests();
    assert_eq!(sent.len(), 1, "one upstream request, not one per pass");
    assert_eq!(sent[0].method, "GET");
    assert_eq!(sent[0].url, "https://api.test/status");
    assert!(
        !sent[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("idempotency-key")),
        "a GET has nothing to deduplicate"
    );
}

/// Invariant 2.6, at the seam that ships.
///
/// **One** engine, four concurrent actions, each making one call. If the engine
/// were held across the round trip — a blocking host function, or a pass loop
/// written inside the synchronous dispatch — the peak could not exceed one, at
/// any pool size below four. Gate 5 proved this about a loop written in a test;
/// this proves it about the loop in `server.rs`.
#[tokio::test]
async fn four_concurrent_actions_share_one_engine_and_still_overlap_their_round_trips() {
    const ACTIONS: usize = 4;

    let project = Arc::new(
        CompiledProject::load_from_dir(fixture("fetching_handler")).expect("fixture compiles"),
    );
    let (proxy_id, _) = ids(&project);

    // Long enough that four sub-millisecond body passes cannot serialise their
    // way out of overlapping, short enough that the test costs a blink.
    let transport = Arc::new(PeakTransport::new(
        r#"{"state":"green"}"#,
        Duration::from_millis(150),
    ));
    let server = Arc::new(
        AlbedoServerBuilder::new(empty_config())
            .with_quickjs_action_engine_pool(1)
            .with_aperture_client(client(Arc::clone(&transport)))
            .with_build_id("build-seam")
            .register_compiled_project(Arc::clone(&project))
            .build()
            .expect("server builds"),
    );

    let mut tasks = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let server = Arc::clone(&server);
        tasks.push(tokio::spawn(async move {
            dispatch(&server, proxy_id).await.0
        }));
    }
    for task in tasks {
        assert_eq!(task.await.expect("task joins"), StatusCode::OK);
    }

    assert_eq!(
        transport.peak(),
        ACTIONS,
        "with one engine, {ACTIONS} round trips must still overlap — a peak of 1 means the \
         engine is being held across the wait"
    );
    assert_eq!(transport.requests().len(), ACTIONS);
}

/// A step that failed must reach the author as an error.
///
/// The failure mode this rules out is the quiet one: the call fails, the body
/// throws on the response it never got, and the dispatch answers 200 with an
/// opcode list that happens to be empty — a button that does nothing, with
/// nothing anywhere saying why. The fixture has no `try`/`catch`, so a failed
/// step must take the whole dispatch down.
#[tokio::test]
async fn a_failed_step_takes_the_dispatch_down_rather_than_writing_nothing() {
    #[derive(Debug)]
    struct Broken;
    #[async_trait]
    impl Transport for Broken {
        async fn send(&self, _request: &WireRequest) -> Result<WireResponse, ApertureError> {
            Err(ApertureError::Transport("connection refused".to_string()))
        }
    }

    let project = Arc::new(
        CompiledProject::load_from_dir(fixture("fetching_handler")).expect("fixture compiles"),
    );
    let (proxy_id, _) = ids(&project);

    let server = AlbedoServerBuilder::new(empty_config())
        .with_quickjs_action_engine_pool(1)
        .with_aperture_client(Arc::new(ApertureClient::new(
            Arc::new(Broken),
            Arc::new(ResponseCache::new(DEFAULT_RESPONSE_BUDGET)),
            Arc::new(EgressPolicy::new(EgressMode::Serve)),
        )))
        .with_build_id("build-seam")
        .register_compiled_project(Arc::clone(&project))
        .build()
        .expect("server builds");

    let session_uuid = uuid::Uuid::new_v4().to_string();
    let csrf = server
        .csrf_registry()
        .token_for(SessionId::new(uuid::Uuid::parse_str(&session_uuid).unwrap()));
    let body = encode_action_envelope(&ActionEnvelope {
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
                .header("x-albedo-session", session_uuid.as_str())
                .header("x-albedo-csrf", csrf.as_str())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("router handles the POST");

    assert_ne!(
        response.status(),
        StatusCode::OK,
        "a body whose only call failed must not report success"
    );
    let bytes = to_bytes(response.into_body(), MAX_BODY).await.expect("body");
    let message = String::from_utf8_lossy(&bytes);
    assert!(
        message.contains("connection refused"),
        "the upstream's reason has to survive to the response; got: {message}"
    );
}
