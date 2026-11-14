//! R1b (T-0026) · AC 11 — WebTransport sessions, server side, with a real QUIC
//! client (quinn) against the server's own listener.
//!
//! Browser establishment is **not** claimed (T-0028): the listener speaks raw
//! QUIC with framed uni streams, which is exactly what this client speaks.
//!
//! Falsifier 6: on the pre-T-0026 tree nothing closes a WT session from the
//! server side — it waits for the peer — so `revocation_closes_the_session`
//! times out waiting for the close.

#![cfg(feature = "forge")]

use albedo_server::auth::Identity;
use albedo_server::config::WebTransportConfig;
use albedo_server::live_authority::{LiveRegistry, Pause};
use albedo_server::webtransport::{WRITER_PAUSE, WT_CLOSE_REVOKED};
use albedo_server::{WebTransportRuntime, WebTransportSessionRegistry};
use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::store::{
    create_session, resolve_session, upsert_principal, ProviderProfile,
};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};
use dom_render_compiler::runtime::webtransport::{
    WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES, WT_STREAM_SLOT_PREFETCH, WT_STREAM_SLOT_SHELL,
};
use quinn::rustls;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

struct Listener {
    addr: std::net::SocketAddr,
    sessions: WebTransportSessionRegistry,
    cert: rustls::pki_types::CertificateDer<'static>,
    _dir: tempfile::TempDir,
    _shutdown: tokio::sync::watch::Sender<bool>,
}

async fn listener() -> Listener {
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let config = WebTransportConfig {
        enabled: true,
        cert_path: Some(cert_path.display().to_string()),
        key_path: Some(key_path.display().to_string()),
        ..WebTransportConfig::default()
    };
    let sessions = WebTransportSessionRegistry::default();
    let runtime = WebTransportRuntime::bind_with_registry(addr, &config, sessions.clone()).unwrap();
    let (shutdown, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = runtime.run(rx).await;
    });
    Listener {
        addr,
        sessions,
        cert: cert.cert.der().clone(),
        _dir: dir,
        _shutdown: shutdown,
    }
}

async fn connect(listener: &Listener) -> quinn::Connection {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(listener.cert.clone()).unwrap();
    let client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_config);
    endpoint
        .connect(listener.addr, "localhost")
        .unwrap()
        .await
        .unwrap()
}

async fn read_frame(stream: &mut quinn::RecvStream) -> Option<Vec<u8>> {
    let len = stream.read_u32().await.ok()?;
    let mut buf = vec![0; len as usize];
    stream.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

/// Accept the four uni streams; return (control, patches), keyed by the slot
/// each announces in its `stream_open` frame.
async fn all_streams(connection: &quinn::Connection) -> Vec<quinn::RecvStream> {
    let mut streams: Vec<Option<quinn::RecvStream>> = (0..4).map(|_| None).collect();
    for _ in 0..4 {
        let mut stream = connection.accept_uni().await.unwrap();
        let open: serde_json::Value =
            serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
        let slot = open["stream_slot"].as_u64().unwrap() as usize;
        streams[slot] = Some(stream);
    }
    streams.into_iter().map(Option::unwrap).collect()
}

async fn streams(connection: &quinn::Connection) -> (quinn::RecvStream, quinn::RecvStream) {
    let mut streams = all_streams(connection).await;
    let patches = streams.remove(WT_STREAM_SLOT_PATCHES as usize);
    let control = streams.remove(WT_STREAM_SLOT_CONTROL as usize);
    (control, patches)
}

async fn session_id(control: &mut quinn::RecvStream) -> uuid::Uuid {
    let init: serde_json::Value =
        serde_json::from_slice(&read_frame(control).await.unwrap()).unwrap();
    assert_eq!(init["event"], "session_init");
    init["session_id"].as_str().unwrap().parse().unwrap()
}

async fn store() -> Arc<dyn DataSubstrate> {
    let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
    let tables = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
    bootstrap_schema(&db, &tables).await.unwrap();
    Arc::new(db)
}

async fn signed_in(db: &dyn DataSubstrate, who: &str) -> Identity {
    let now = albedo_server::auth::now_ms();
    let principal = upsert_principal(db, "password", who, &ProviderProfile::default(), now)
        .await
        .unwrap();
    let token = create_session(db, &principal.id, "password", now, 86_400_000)
        .await
        .unwrap();
    let resolved = resolve_session(db, &token, now).await.unwrap();
    Identity::Authenticated {
        principal: Box::new(resolved.principal().unwrap().clone()),
        token: token.hash(),
    }
}

/// Establishment (unbound control traffic) works; binding admits; a revocation
/// closes the QUIC connection with the revoked code and removes the session;
/// rebinding the UUID to another identity is refused; a frame queued before the
/// close and paused at the writer's admission load is never sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_closes_the_session() {
    let pause = Arc::new(Pause::default());
    WRITER_PAUSE
        .set(pause.clone())
        .ok()
        .expect("one test owns the writer pause");

    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let ada = signed_in(db.as_ref(), "ada").await;
    let mallory = signed_in(db.as_ref(), "mallory").await;

    let connection = connect(&listener).await;
    let mut streams = all_streams(&connection).await;
    // Control traffic flows before any binding.
    let id = session_id(&mut streams[WT_STREAM_SLOT_CONTROL as usize]).await;
    assert!(listener.sessions.has(&id));

    let entry = listener
        .sessions
        .bind(id, &ada, &live, Some(db.as_ref()))
        .await
        .expect("bind admits");
    assert!(entry.admits());
    // Same identity again: fine. Another identity: refused before anything
    // keyed by the UUID is touched.
    listener
        .sessions
        .bind(id, &ada, &live, Some(db.as_ref()))
        .await
        .expect("same identity rebinds");
    let refused = listener
        .sessions
        .bind(id, &mallory, &live, Some(db.as_ref()))
        .await;
    assert!(
        refused.is_err(),
        "a second identity bound an already-bound UUID"
    );

    // A protected frame, queued while live, paused before its admission load.
    for slot in [
        WT_STREAM_SLOT_SHELL,
        WT_STREAM_SLOT_PATCHES,
        WT_STREAM_SLOT_PREFETCH,
    ] {
        listener
            .sessions
            .stream_sender(id, slot)
            .unwrap()
            .send(bytes::Bytes::from_static(b"secret-payroll-frame"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), pause.reached.notified())
            .await
            .expect("each protected writer reached its gate");
    }

    // Revoke: logout of ada's session.
    let Identity::Authenticated { token, .. } = &ada else {
        unreachable!()
    };
    assert_eq!(live.close_tokens(std::slice::from_ref(token)), 1);
    pause.release.notify_waiters();

    let reason = tokio::time::timeout(Duration::from_secs(5), connection.closed())
        .await
        .expect("the server closed the session");
    match reason {
        quinn::ConnectionError::ApplicationClosed(close) => {
            assert_eq!(close.error_code, quinn::VarInt::from_u32(WT_CLOSE_REVOKED));
        }
        other => panic!("closed for another reason: {other:?}"),
    }
    // The paused frame never reached the wire.
    for slot in [
        WT_STREAM_SLOT_SHELL,
        WT_STREAM_SLOT_PATCHES,
        WT_STREAM_SLOT_PREFETCH,
    ] {
        let leaked = tokio::time::timeout(
            Duration::from_millis(500),
            read_frame(&mut streams[slot as usize]),
        )
        .await;
        assert!(
            !matches!(leaked, Ok(Some(ref bytes)) if bytes == b"secret-payroll-frame"),
            "stream slot {slot} sent a frame admitted after the close"
        );
    }

    // The session is removed and its live entry with it.
    for _ in 0..50 {
        if !listener.sessions.has(&id) && live.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !listener.sessions.has(&id),
        "the closed session stayed registered"
    );
    assert!(live.is_empty(), "the live entry outlived the QUIC session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_while_a_bridge_revalidates_refuses_the_binding() {
    use albedo_server::live_authority::Checkpoint;
    use dom_render_compiler::auth::store::revoke_session;

    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let ada = signed_in(db.as_ref(), "gap").await;
    let connection = connect(&listener).await;
    let (mut control, _patches) = streams(&connection).await;
    let id = session_id(&mut control).await;
    let pause = live.pause_at(Checkpoint::BeforeRevalidate);
    let mut binding = Box::pin(listener.sessions.bind(id, &ada, &live, Some(db.as_ref())));
    tokio::select! {
        _ = &mut binding => panic!("binding did not pause"),
        _ = pause.reached.notified() => {},
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("binding was not reached"),
    }
    let token = ada.token().unwrap();
    assert_eq!(revoke_session(db.as_ref(), token).await.unwrap(), 1);
    assert_eq!(live.close_tokens(std::slice::from_ref(token)), 1);
    pause.release.notify_one();
    assert!(binding.await.is_err(), "closed binding was promoted");
    tokio::time::timeout(Duration::from_secs(5), connection.closed())
        .await
        .unwrap();
    for _ in 0..50 {
        if !listener.sessions.has(&id) && live.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!listener.sessions.has(&id));
    assert!(live.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_revalidation_database_closes_the_quic_session() {
    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let ada = signed_in(db.as_ref(), "missing-db").await;
    let connection = connect(&listener).await;
    let (mut control, _patches) = streams(&connection).await;
    let id = session_id(&mut control).await;
    assert!(listener.sessions.bind(id, &ada, &live, None).await.is_err());
    let reason = tokio::time::timeout(Duration::from_secs(5), connection.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason, quinn::ConnectionError::ApplicationClosed(close)
        if close.error_code == quinn::VarInt::from_u32(WT_CLOSE_REVOKED))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_anonymous_binding_needs_no_database_and_survives_unrelated_invalidation() {
    let listener = listener().await;
    let live = Arc::new(LiveRegistry::new());
    let connection = connect(&listener).await;
    let (mut control, _patches) = streams(&connection).await;
    let id = session_id(&mut control).await;
    let entry = listener
        .sessions
        .bind(id, &Identity::Anonymous, &live, None)
        .await
        .unwrap();
    assert!(entry.admits());
    assert_eq!(live.close_principal_below("unrelated", 99), 0);
    assert_eq!(
        live.close_tokens(&[dom_render_compiler::auth::session::TokenHash::of(
            "unrelated"
        )]),
        0
    );
    assert_eq!(live.close_expired(i64::MAX), 0);
    assert!(entry.admits());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), connection.closed())
            .await
            .is_err()
    );
    connection.close(0u32.into(), b"test complete");
    for _ in 0..50 {
        if !listener.sessions.has(&id) && live.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!listener.sessions.has(&id));
    assert!(live.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_the_first_binding_closes_its_opening_entry() {
    use albedo_server::live_authority::Checkpoint;

    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let ada = signed_in(db.as_ref(), "cancelled-bridge").await;
    let connection = connect(&listener).await;
    let (mut control, _patches) = streams(&connection).await;
    let id = session_id(&mut control).await;
    let pause = live.pause_at(Checkpoint::BeforeRevalidate);
    let mut binding = Box::pin(listener.sessions.bind(id, &ada, &live, Some(db.as_ref())));
    tokio::select! {
        _ = &mut binding => panic!("binding did not pause"),
        _ = pause.reached.notified() => {},
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("binding was not reached"),
    }
    drop(binding);
    let closed = tokio::time::timeout(Duration::from_secs(1), connection.closed()).await;
    connection.close(0u32.into(), b"test complete");
    assert!(
        closed.is_ok(),
        "cancelled bind retained an Opening entry and a live QUIC session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_authenticated_uuid_cannot_switch_tokens_or_become_anonymous() {
    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let first = signed_in(db.as_ref(), "same-principal").await;
    let second = signed_in(db.as_ref(), "same-principal").await;
    assert_eq!(
        first.principal().unwrap().id,
        second.principal().unwrap().id
    );
    assert_ne!(first.token(), second.token());
    let connection = connect(&listener).await;
    let (mut control, _patches) = streams(&connection).await;
    let id = session_id(&mut control).await;
    let entry = listener
        .sessions
        .bind(id, &first, &live, Some(db.as_ref()))
        .await
        .unwrap();
    assert!(listener
        .sessions
        .bind(id, &second, &live, Some(db.as_ref()))
        .await
        .is_err());
    assert!(listener
        .sessions
        .bind(id, &Identity::Anonymous, &live, None)
        .await
        .is_err());
    assert!(
        entry.admits(),
        "a refused rebind closed the legitimate owner"
    );
    assert_eq!(live.len(), 1);
    connection.close(0u32.into(), b"test complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_anonymous_uuid_cannot_be_claimed_by_an_authenticated_bridge() {
    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let identity = signed_in(db.as_ref(), "claim-public").await;
    let connection = connect(&listener).await;
    let (mut control, _patches) = streams(&connection).await;
    let id = session_id(&mut control).await;
    let entry = listener
        .sessions
        .bind(id, &Identity::Anonymous, &live, None)
        .await
        .unwrap();
    assert!(listener
        .sessions
        .bind(id, &identity, &live, Some(db.as_ref()))
        .await
        .is_err());
    assert!(entry.admits());
    assert_eq!(live.len(), 1);
    connection.close(0u32.into(), b"test complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_epoch_change_closes_only_the_old_binding_after_bind_returns() {
    use dom_render_compiler::auth::store::grant_role_changed;

    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let old = signed_in(db.as_ref(), "role-owner").await;
    let old_connection = connect(&listener).await;
    let (mut control, _patches) = streams(&old_connection).await;
    let old_id = session_id(&mut control).await;
    listener
        .sessions
        .bind(old_id, &old, &live, Some(db.as_ref()))
        .await
        .unwrap();
    let (_, epoch) = grant_role_changed(
        db.as_ref(),
        &old.principal().unwrap().id,
        "payroll",
        None,
        albedo_server::auth::now_ms(),
    )
    .await
    .unwrap();
    let current = signed_in(db.as_ref(), "role-owner").await;
    let new_connection = connect(&listener).await;
    let (mut new_control, _new_patches) = streams(&new_connection).await;
    let new_id = session_id(&mut new_control).await;
    let fresh = listener
        .sessions
        .bind(new_id, &current, &live, Some(db.as_ref()))
        .await
        .unwrap();
    assert_eq!(
        live.close_principal_below(old.principal().unwrap().id.as_str(), epoch.unwrap()),
        1
    );
    let reason = tokio::time::timeout(Duration::from_secs(5), old_connection.closed())
        .await
        .unwrap();
    assert!(
        matches!(reason, quinn::ConnectionError::ApplicationClosed(close) if close.error_code == quinn::VarInt::from_u32(WT_CLOSE_REVOKED))
    );
    assert!(fresh.admits());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), new_connection.closed())
            .await
            .is_err()
    );
    new_connection.close(0u32.into(), b"test complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_bridges_cannot_steal_a_binding_awaiting_revalidation() {
    use albedo_server::live_authority::Checkpoint;

    let listener = listener().await;
    let db = store().await;
    let live = Arc::new(LiveRegistry::new());
    let owner = signed_in(db.as_ref(), "binding-owner").await;
    let attacker = signed_in(db.as_ref(), "binding-attacker").await;
    let connection = connect(&listener).await;
    let (mut control, _patches) = streams(&connection).await;
    let id = session_id(&mut control).await;
    let pause = live.pause_at(Checkpoint::BeforeRevalidate);
    let mut binding = Box::pin(listener.sessions.bind(id, &owner, &live, Some(db.as_ref())));
    tokio::select! {
        _ = &mut binding => panic!("binding did not pause"),
        _ = pause.reached.notified() => {},
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("binding was not reached"),
    }
    assert!(listener
        .sessions
        .bind(id, &attacker, &live, Some(db.as_ref()))
        .await
        .is_err());
    assert!(
        listener
            .sessions
            .bind(id, &owner, &live, Some(db.as_ref()))
            .await
            .is_err(),
        "an unfinished owner binding admitted a second bridge"
    );
    assert_eq!(live.len(), 1);
    pause.release.notify_one();
    assert!(binding.await.unwrap().admits());
    connection.close(0u32.into(), b"test complete");
}
