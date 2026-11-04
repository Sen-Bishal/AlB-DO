//! Durable admission and completion through the compiled action HTTP adapter.

#![cfg(feature = "forge")]

use albedo_server::render::csrf::CsrfRegistry;
use albedo_server::{AlbedoServerBuilder, AppConfig};
use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use dom_render_compiler::aperture::{
    ApertureClient, ApertureError, EgressMode, EgressPolicy, ResponseCache, Transport, WireRequest,
    WireResponse, DEFAULT_RESPONSE_BUDGET,
};
use dom_render_compiler::forge::{DataSubstrate, ForgeCollection, ForgeSchema, LibSqlSubstrate};
use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::{CompiledProject, SessionId};
use dom_render_compiler::transforms::allocate_form_action_id;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Notify};
use tower::ServiceExt;

const SIMPLE_ACTION: &str = r#"
import { action } from "albedo";
export const save = action(({ form }) => {
  append("guestbook", { author: "action", message: form.message });
  return { ok: "saved" };
});
export default function Page() { return <p>actions</p>; }
"#;

const QUEUED_ACTIONS: &str = r#"
import { action } from "albedo";
export const save = action(async ({ form }) => {
  const response = await fetch("https://api.test/admit", { method: "POST", body: "intent" });
  const result = await response.json();
  append("guestbook", { author: "action", message: form.message });
  enqueue("later", { message: result.message }, { delay: "1d" });
  return { ok: "saved" };
});
export const invalid_queue = action(() => {
  append("guestbook", { author: "action", message: "must roll back" });
  enqueue("later", { message: "also rolls back" }, { delay: "1d" });
  enqueue("undeclared", {});
  return { ok: "must not complete" };
});
export default function Page() { return <p>actions</p>; }
"#;

#[derive(Debug, Default)]
struct PausedTransport {
    calls: AtomicUsize,
    entered: Notify,
    release: Notify,
    exited: Notify,
}

struct SendGuard<'a>(&'a Notify);

impl Drop for SendGuard<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[async_trait]
impl Transport for PausedTransport {
    async fn send(&self, _request: &WireRequest) -> Result<WireResponse, ApertureError> {
        let _guard = SendGuard(&self.exited);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(WireResponse {
            status: 200,
            body: br#"{"message":"accepted"}"#.to_vec(),
            headers: vec![("content-type".into(), "application/json".into())],
            etag: None,
            last_modified: None,
            content_type: Some("application/json".into()),
        })
    }
}

struct Running {
    router: Router,
    csrf: Arc<CsrfRegistry>,
    db: LibSqlSubstrate,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}

async fn start(pooled: bool, transport: Option<Arc<PausedTransport>>) -> Running {
    let directory = tempfile::tempdir().expect("fixture directory");
    let source = directory.path().join("src");
    std::fs::create_dir(&source).expect("source directory");
    std::fs::write(
        source.join("Page.tsx"),
        if pooled {
            QUEUED_ACTIONS
        } else {
            SIMPLE_ACTION
        },
    )
    .expect("action source");
    if pooled {
        std::fs::write(
            source.join("jobs.ts"),
            r#"import { job } from "albedo/jobs";
               export const later = job({}, (args) => "done");"#,
        )
        .expect("job declaration");
    }
    let project = Arc::new(CompiledProject::load_from_dir(&source).expect("project compiles"));
    let schema = ForgeSchema::build(vec![ForgeCollection::new(
        "guestbook",
        "guestbook",
        "SELECT id, author, message FROM guestbook ORDER BY id",
        "id",
        vec!["CREATE TABLE IF NOT EXISTS guestbook (id INTEGER PRIMARY KEY AUTOINCREMENT, author TEXT NOT NULL, message TEXT NOT NULL)".into()].into_boxed_slice(),
        Vec::new().into_boxed_slice(),
    )])
    .expect("schema");
    let db_path = directory.path().join("forge.db");
    let mut config = AppConfig::default();
    config.server.host = "127.0.0.1".into();
    config.server.port = 0;
    config.forge.db_path = Some(db_path.to_string_lossy().into_owned());
    let mut builder = AlbedoServerBuilder::new(config)
        .with_forge_schema(schema)
        .with_build_id("action-atomicity");
    if pooled {
        builder = builder.with_quickjs_action_engine_pool(2);
    }
    if let Some(transport) = transport {
        let client = ApertureClient::new(
            transport,
            Arc::new(ResponseCache::new(DEFAULT_RESPONSE_BUDGET)),
            Arc::new(EgressPolicy::new(EgressMode::Serve)),
        );
        builder = builder.with_aperture_client(Arc::new(client));
    }
    let server = builder
        .register_compiled_project(project)
        .build()
        .expect("server builds");
    let router = server.router();
    let csrf = server.csrf_registry();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        server
            .run_until(
                move |_| {
                    let _ = ready_tx.send(());
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
            .expect("server runs");
    });
    tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("boot deadline")
        .expect("server ready");
    let db = LibSqlSubstrate::open_local(&db_path)
        .await
        .expect("database");
    Running {
        router,
        csrf,
        db,
        shutdown: Some(shutdown_tx),
        task,
        _directory: directory,
    }
}

fn request(csrf: &CsrfRegistry, action: &str, intent: &str, message: &str) -> Request<Body> {
    let session = SessionId::random();
    let token = csrf.token_for(session);
    let envelope = ActionEnvelope {
        action_id: allocate_form_action_id(action),
        event_kind: 3,
        payload: serde_json::to_vec(&json!({ "_albedo_intent": intent, "message": message }))
            .expect("payload"),
    };
    Request::builder()
        .method("POST")
        .uri("/_albedo/action")
        .header("x-albedo-session", session.to_string())
        .header("x-albedo-csrf", token)
        .body(Body::from(
            encode_action_envelope(&envelope).expect("envelope"),
        ))
        .expect("request")
}

async fn dispatch(router: Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = tokio::time::timeout(Duration::from_secs(10), router.oneshot(request))
        .await
        .expect("dispatch deadline")
        .expect("HTTP response");
    let status = response.status();
    if status == StatusCode::CONFLICT {
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
            Some("60"),
            "an active intent should tell the caller when to retry"
        );
    }
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response body");
    (status, bytes.to_vec())
}

async fn count(db: &dyn DataSubstrate, table: &str) -> i64 {
    let rows = db
        .query(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .expect("count query");
    match rows.rows[0].get(0) {
        Some(dom_render_compiler::forge::SqlValue::Integer(value)) => *value,
        other => panic!("unexpected count {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_retries_admit_one_owner_and_replay_the_atomic_result() {
    let transport = Arc::new(PausedTransport::default());
    let server = start(true, Some(transport.clone())).await;
    let first = tokio::spawn(dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "one-intent", "once"),
    ));
    tokio::time::timeout(Duration::from_secs(5), transport.entered.notified())
        .await
        .expect("the owner reached its outbound step");

    let (status, body) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "one-intent", "once"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "active retry must not execute"
    );
    assert!(
        String::from_utf8_lossy(&body).contains("progress"),
        "expected an in-progress refusal: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&server.db, "guestbook").await, 0);
    assert_eq!(count(&server.db, "albedo_jobs").await, 0);

    transport.release.notify_one();
    let (status, original) = first.await.expect("owner task");
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&original)
    );
    let (status, replayed) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "one-intent", "once"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&replayed)
    );
    let (original, _) = decode_frame(&original).expect("original frame");
    let (replayed, _) = decode_frame(&replayed).expect("replayed frame");
    assert_eq!(original.instructions, replayed.instructions);
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&server.db, "guestbook").await, 1);
    assert_eq!(count(&server.db, "albedo_jobs").await, 1);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 1);

    let (status, _) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "one-intent", "changed payload"),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "an intent cannot change its request"
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&server.db, "guestbook").await, 1);

    // Identical authored fields with a fresh intent are a separate operation.
    transport.release.notify_one();
    let (status, body) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "another-intent", "once"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_eq!(count(&server.db, "guestbook").await, 2);
    assert_eq!(count(&server.db, "albedo_jobs").await, 2);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 2);
}

#[tokio::test]
async fn an_invalid_later_enqueue_rolls_back_writes_jobs_and_completion() {
    let server = start(true, None).await;
    let (status, body) = dispatch(
        server.router.clone(),
        request(&server.csrf, "invalid_queue", "invalid-job", "unused"),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        String::from_utf8_lossy(&body).contains("undeclared"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(count(&server.db, "guestbook").await, 0);
    assert_eq!(count(&server.db, "albedo_jobs").await, 0);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 0);
}

#[tokio::test]
async fn cancellation_leaves_no_effects_and_an_expired_attempt_can_resume() {
    let transport = Arc::new(PausedTransport::default());
    let server = start(true, Some(transport.clone())).await;
    let attempt = tokio::spawn(dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "cancelled-intent", "after retry"),
    ));
    tokio::time::timeout(Duration::from_secs(5), transport.entered.notified())
        .await
        .expect("outbound request started");
    attempt.abort();
    assert!(attempt
        .await
        .expect_err("attempt was cancelled")
        .is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), transport.exited.notified())
        .await
        .expect("cancellation drops the in-flight transport future");
    assert_eq!(count(&server.db, "guestbook").await, 0);
    assert_eq!(count(&server.db, "albedo_jobs").await, 0);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 0);

    let (status, body) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "cancelled-intent", "after retry"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&body).contains("progress"));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

    // Advance only this fixture's durable lease; no wall-clock sleep is needed.
    assert_eq!(
        server
            .db
            .execute(
                "UPDATE albedo_action_admissions SET lease_until = 0 WHERE completed_at IS NULL",
                &[],
            )
            .await
            .expect("expire abandoned attempt"),
        1
    );
    transport.release.notify_one();
    let (status, body) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "cancelled-intent", "after retry"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_eq!(count(&server.db, "guestbook").await, 1);
    assert_eq!(count(&server.db, "albedo_jobs").await, 1);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 1);
}

#[tokio::test]
async fn completion_storage_failure_rolls_back_staged_writes_and_jobs() {
    let transport = Arc::new(PausedTransport::default());
    let server = start(true, Some(transport.clone())).await;
    server
        .db
        .migrate(
            "CREATE TRIGGER reject_completion BEFORE INSERT ON albedo_workflow_results \
             BEGIN SELECT RAISE(ABORT, 'completion rejected'); END",
        )
        .await
        .expect("install completion fault");
    transport.release.notify_one();
    let (status, body) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "completion-failure", "atomic"),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        String::from_utf8_lossy(&body).contains("completion rejected"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(count(&server.db, "guestbook").await, 0);
    assert_eq!(count(&server.db, "albedo_jobs").await, 0);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 0);

    server
        .db
        .migrate("DROP TRIGGER reject_completion")
        .await
        .expect("remove completion fault");
    server
        .db
        .execute(
            "UPDATE albedo_action_admissions SET lease_until = 0 WHERE completed_at IS NULL",
            &[],
        )
        .await
        .expect("expire failed attempt");
    let (status, body) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "completion-failure", "atomic"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        transport.calls.load(Ordering::SeqCst),
        1,
        "settled HTTP step resumes"
    );
    assert_eq!(count(&server.db, "guestbook").await, 1);
    assert_eq!(count(&server.db, "albedo_jobs").await, 1);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 1);
}

#[tokio::test]
async fn pure_rust_actions_also_replay_without_repeating_the_write() {
    let server = start(false, None).await;
    for _ in 0..2 {
        let (status, body) = dispatch(
            server.router.clone(),
            request(&server.csrf, "save", "fallback-intent", "once"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    }
    assert_eq!(count(&server.db, "guestbook").await, 1);
    assert_eq!(count(&server.db, "albedo_workflow_results").await, 1);

    server
        .db
        .execute(
            "UPDATE albedo_workflow_results SET instructions = X'FF'",
            &[],
        )
        .await
        .expect("corrupt saved result");
    let (status, _) = dispatch(
        server.router.clone(),
        request(&server.csrf, "save", "fallback-intent", "once"),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "corrupt replay must fail closed");
    assert_eq!(count(&server.db, "guestbook").await, 1);
}
