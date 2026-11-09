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
use dom_render_compiler::runtime::webtransport::{WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES};
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
async fn streams(connection: &quinn::Connection) -> (quinn::RecvStream, quinn::RecvStream) {
    let mut control = None;
    let mut patches = None;
    for _ in 0..4 {
        let mut stream = connection.accept_uni().await.unwrap();
        let open: serde_json::Value =
            serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
        match open["stream_slot"].as_u64().unwrap() as u8 {
            WT_STREAM_SLOT_CONTROL => control = Some(stream),
            WT_STREAM_SLOT_PATCHES => patches = Some(stream),
            _ => {}
        }
    }
    (control.unwrap(), patches.unwrap())
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
    let (mut control, mut patches) = streams(&connection).await;
    // Control traffic flows before any binding.
    let id = session_id(&mut control).await;
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
    let sender = listener
        .sessions
        .stream_sender(id, WT_STREAM_SLOT_PATCHES)
        .unwrap();
    sender
        .send(bytes::Bytes::from_static(b"secret-payroll-frame"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), pause.reached.notified())
        .await
        .expect("writer reached its gate");

    // Revoke: logout of ada's session.
    let Identity::Authenticated { token, .. } = &ada else {
        unreachable!()
    };
    assert_eq!(live.close_tokens(std::slice::from_ref(token)), 1);
    pause.release.notify_one();

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
    let leaked = tokio::time::timeout(Duration::from_millis(500), read_frame(&mut patches)).await;
    assert!(
        !matches!(leaked, Ok(Some(ref bytes)) if bytes == b"secret-payroll-frame"),
        "a frame admitted after the close was sent"
    );

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
