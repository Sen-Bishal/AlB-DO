use crate::config::WebTransportConfig;
use crate::error::RuntimeError;
use dom_render_compiler::runtime::webtransport::{
    WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES, WT_STREAM_SLOT_PREFETCH, WT_STREAM_SLOT_SHELL,
};
use dom_render_compiler::runtime::{BroadcastRegistry, SessionId};
use quinn::{Connection, Endpoint, SendStream};
use serde::Serialize;
use std::collections::HashMap;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Duration};
use tracing::{debug, info, warn};
use uuid::Uuid;

const WEBTRANSPORT_STREAM_COUNT: usize = 4;

/// R1b · the QUIC application close code for a session whose authority ended.
pub const WT_CLOSE_REVOKED: u32 = 0x4152; // "AR"

/// R1b · who a WT session belongs to. A session is accepted before any HTTP
/// bridge names an identity, so it starts unbound; the first bridge binds it.
#[derive(Default)]
pub enum WtBinding {
    /// No bridge yet. Only control traffic (`stream_open`, `session_init`,
    /// keep-alives) may be written.
    #[default]
    Unbound,
    /// Bound by the first bridge. `token` is `None` for an anonymous bridge.
    Bound {
        /// The binding identity's session token hash.
        token: Option<dom_render_compiler::auth::session::TokenHash>,
        /// The session's live-registry entry. Owned here, so it lives exactly
        /// as long as the QUIC session, not the bridge request.
        live: crate::live_authority::LiveHandle,
    },
}

// The QUIC session owns the handle before the first bridge awaits its read.
// If that bridge is cancelled, close the pending binding rather than leaving
// an Opening entry that no later bridge can finish.
struct PendingBinding(Option<Arc<crate::live_authority::LiveEntry>>);

impl Drop for PendingBinding {
    fn drop(&mut self) {
        if let Some(entry) = self.0.take() {
            entry.close();
        }
    }
}

#[derive(Clone)]
pub struct WebTransportSessionHandle {
    pub session_id: Uuid,
    pub remote_addr: SocketAddr,
    pub stream_senders: [mpsc::Sender<bytes::Bytes>; WEBTRANSPORT_STREAM_COUNT],
    /// R1b · the identity binding (ownership checks on bind).
    pub binding: Arc<Mutex<WtBinding>>,
    /// R1b · the bound session's live entry, set once on the first bind. The
    /// protected writers read it lock-free on every frame.
    pub gate: Arc<std::sync::OnceLock<Arc<crate::live_authority::LiveEntry>>>,
    /// R1b · the connection, so a revocation can close it from the server side.
    pub connection: Option<Connection>,
}

#[derive(Clone, Default)]
pub struct WebTransportSessionRegistry {
    sessions: Arc<Mutex<HashMap<Uuid, WebTransportSessionHandle>>>,
}

impl WebTransportSessionRegistry {
    pub fn insert(&self, handle: WebTransportSessionHandle) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(handle.session_id, handle);
        }
    }

    pub fn remove(&self, session_id: &Uuid) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(session_id);
        }
    }

    pub fn count(&self) -> usize {
        self.sessions
            .lock()
            .map(|sessions| sessions.len())
            .unwrap_or(0)
    }

    pub fn has(&self, session_id: &Uuid) -> bool {
        self.sessions
            .lock()
            .map(|sessions| sessions.contains_key(session_id))
            .unwrap_or(false)
    }

    pub(crate) fn bound_entry(
        &self,
        session_id: Uuid,
    ) -> Option<Arc<crate::live_authority::LiveEntry>> {
        let sessions = self.sessions.lock().ok()?;
        sessions.get(&session_id)?.gate.get().cloned()
    }

    /// Phase P · Stream C.4 — clone the raw `mpsc::Sender` for one
    /// stream slot of a session. Used by the streaming handler to
    /// hand the patches-lane sender to [`crate::BroadcastRegistry::auto_subscribe`]
    /// so future `write_topic` fan-outs land on the session directly
    /// (the broadcast registry stores per-subscriber senders and
    /// drains them with `try_send`). `None` when either the session
    /// is unknown or `stream_slot` is out of range.
    pub fn stream_sender(
        &self,
        session_id: Uuid,
        stream_slot: u8,
    ) -> Option<mpsc::Sender<bytes::Bytes>> {
        if (stream_slot as usize) >= WEBTRANSPORT_STREAM_COUNT {
            return None;
        }
        let sessions = self.sessions.lock().ok()?;
        let handle = sessions.get(&session_id)?;
        Some(handle.stream_senders[stream_slot as usize].clone())
    }

    pub async fn send_payload(
        &self,
        session_id: Uuid,
        stream_slot: u8,
        payload: Vec<u8>,
    ) -> Result<(), RuntimeError> {
        if stream_slot as usize >= WEBTRANSPORT_STREAM_COUNT {
            return Err(RuntimeError::ServerRuntime(format!(
                "invalid WT stream slot {stream_slot}"
            )));
        }

        let sender = {
            let sessions = self.sessions.lock().map_err(|_| {
                RuntimeError::ServerRuntime("WT session registry lock poisoned".to_string())
            })?;
            let handle = sessions.get(&session_id).ok_or_else(|| {
                RuntimeError::ServerRuntime(format!("WT session '{}' not found", session_id))
            })?;
            handle.stream_senders[stream_slot as usize].clone()
        };

        sender.send(bytes::Bytes::from(payload)).await.map_err(|_| {
            RuntimeError::ServerRuntime(format!(
                "WT stream slot {} channel closed for session '{}'",
                stream_slot, session_id
            ))
        })
    }

    /// R1b · bind `session_id` to `identity` on its first bridge, or check a
    /// later bridge presents the same identity. Returns the session's live entry,
    /// re-validated, when it admits; an error refuses the bridge.
    ///
    /// Binding once is what makes the UUID safe to trust downstream (the CSRF
    /// table, the broadcast subscription, the output senders): a second bridge
    /// presenting the same UUID under another identity is refused here, before
    /// any of those are touched.
    pub async fn bind(
        &self,
        session_id: Uuid,
        identity: &crate::auth::Identity,
        live: &Arc<crate::live_authority::LiveRegistry>,
        db: Option<&dyn dom_render_compiler::forge::DataSubstrate>,
    ) -> Result<Arc<crate::live_authority::LiveEntry>, String> {
        let (binding, gate, connection) = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| "WT session registry lock poisoned".to_string())?;
            let handle = sessions
                .get(&session_id)
                .ok_or_else(|| format!("WT session '{session_id}' not found"))?;
            (handle.binding.clone(), handle.gate.clone(), handle.connection.clone())
        };
        let presented = identity.token().cloned();
        let (entry, fresh) = {
            let mut slot = binding
                .lock()
                .map_err(|_| "WT binding lock poisoned".to_string())?;
            match &*slot {
                WtBinding::Bound { token, live } => {
                    if *token != presented {
                        return Err("WT session is bound to another identity".to_string());
                    }
                    (live.entry().clone(), false)
                }
                WtBinding::Unbound => {
                    let handle = live.register(crate::live_authority::LiveKind::WebTransport, identity);
                    let entry = handle.entry().clone();
                    let _ = gate.set(entry.clone());
                    *slot = WtBinding::Bound {
                        token: presented,
                        live: handle,
                    };
                    (entry, true)
                }
            }
        };
        if fresh {
            let mut pending = PendingBinding(Some(entry.clone()));
            if let Some(connection) = connection {
                entry.on_close(move || {
                    connection.close(WT_CLOSE_REVOKED.into(), b"authority revoked");
                });
            }
            live.revalidate(&entry, db).await;
            pending.0 = None;
        }
        if entry.admits() {
            Ok(entry)
        } else {
            Err("WT session authority is no longer valid".to_string())
        }
    }

    pub async fn send_json<T: Serialize + ?Sized>(
        &self,
        session_id: Uuid,
        stream_slot: u8,
        payload: &T,
    ) -> Result<(), RuntimeError> {
        let payload = serde_json::to_vec(payload).map_err(|err| {
            RuntimeError::ServerRuntime(format!("failed to serialize WT payload: {err}"))
        })?;
        self.send_payload(session_id, stream_slot, payload).await
    }
}

pub struct WebTransportRuntime {
    endpoint: Endpoint,
    sessions: WebTransportSessionRegistry,
    keepalive_interval: Duration,
    stream_buffer_capacity: usize,
    /// Phase O.2 · broadcast registry to clean up on session drop.
    /// `None` means broadcast wasn't wired into this runtime; sessions
    /// still close cleanly, the registry just keeps stale subscriber
    /// entries that the next `write_topic` would prune anyway.
    broadcast: Option<Arc<BroadcastRegistry>>,
}

impl WebTransportRuntime {
    pub fn bind(addr: SocketAddr, config: &WebTransportConfig) -> Result<Self, RuntimeError> {
        Self::bind_with_registry(addr, config, WebTransportSessionRegistry::default())
    }

    pub fn bind_with_registry(
        addr: SocketAddr,
        config: &WebTransportConfig,
        sessions: WebTransportSessionRegistry,
    ) -> Result<Self, RuntimeError> {
        let cert_path = config.cert_path.as_deref().ok_or_else(|| {
            RuntimeError::InvalidConfig("missing webtransport.cert_path".to_string())
        })?;
        let key_path = config.key_path.as_deref().ok_or_else(|| {
            RuntimeError::InvalidConfig("missing webtransport.key_path".to_string())
        })?;

        let cert_chain = load_cert_chain(cert_path)?;
        let private_key = load_private_key(key_path)?;

        let mut server_config = quinn::ServerConfig::with_single_cert(cert_chain, private_key)
            .map_err(|err| {
                RuntimeError::ServerStartup(format!("invalid WT certificate/key: {err}"))
            })?;
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.keep_alive_interval(Some(Duration::from_millis(
            config.keepalive_interval_ms.max(1),
        )));
        transport_config.max_concurrent_uni_streams(quinn::VarInt::from_u32(64));
        server_config.transport_config(Arc::new(transport_config));

        let endpoint = Endpoint::server(server_config, addr).map_err(|err| {
            RuntimeError::ServerStartup(format!("failed to bind WT endpoint: {err}"))
        })?;

        Ok(Self {
            endpoint,
            sessions,
            keepalive_interval: Duration::from_millis(config.keepalive_interval_ms.max(1)),
            stream_buffer_capacity: config.stream_buffer_capacity.max(1),
            broadcast: None,
        })
    }

    /// Phase O.2 · attach a broadcast registry so the session-drop
    /// path runs `cleanup_session(session_id)` automatically when a
    /// QUIC connection closes. Without this wiring the registry's
    /// reverse-index entry for a disconnected session lingers until
    /// the next write to a topic it was subscribed to (which then
    /// observes the closed channel and prunes lazily). Wiring it
    /// makes cleanup eager and predictable.
    #[must_use]
    pub fn with_broadcast(mut self, broadcast: Arc<BroadcastRegistry>) -> Self {
        self.broadcast = Some(broadcast);
        self
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), RuntimeError> {
        info!("WebTransport QUIC listener ready");

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else {
                        break;
                    };
                    let remote_addr = incoming.remote_address();
                    let connecting = match incoming.accept() {
                        Ok(connecting) => connecting,
                        Err(err) => {
                            warn!(remote_addr = %remote_addr, error = %err, "failed to begin WT handshake");
                            continue;
                        }
                    };
                    let sessions = self.sessions.clone();
                    let keepalive_interval = self.keepalive_interval;
                    let stream_buffer_capacity = self.stream_buffer_capacity;
                    let broadcast = self.broadcast.clone();
                    tokio::spawn(async move {
                        if let Err(err) = handle_connecting(
                            connecting,
                            sessions,
                            keepalive_interval,
                            stream_buffer_capacity,
                            broadcast,
                        ).await {
                            warn!(error = %err, "webtransport session task failed");
                        }
                    });
                }
            }
        }

        self.endpoint.close(0u32.into(), b"server shutdown");
        Ok(())
    }

    pub fn session_count(&self) -> usize {
        self.sessions.count()
    }

    pub fn session_registry(&self) -> WebTransportSessionRegistry {
        self.sessions.clone()
    }
}

async fn handle_connecting(
    connecting: quinn::Connecting,
    sessions: WebTransportSessionRegistry,
    keepalive_interval: Duration,
    stream_buffer_capacity: usize,
    broadcast: Option<Arc<BroadcastRegistry>>,
) -> Result<(), RuntimeError> {
    let connection = connecting.await.map_err(|err| {
        RuntimeError::ServerRuntime(format!("failed to accept WT session: {err}"))
    })?;
    let session_id = Uuid::new_v4();
    let remote_addr = connection.remote_address();
    let binding = Arc::new(Mutex::new(WtBinding::Unbound));
    let gate = Arc::new(std::sync::OnceLock::new());
    let stream_senders =
        spawn_stream_writers(connection.clone(), stream_buffer_capacity, gate.clone());

    sessions.insert(WebTransportSessionHandle {
        session_id,
        remote_addr,
        stream_senders: stream_senders.clone(),
        binding: binding.clone(),
        gate,
        connection: Some(connection.clone()),
    });

    info!(
        session_id = %session_id,
        remote_addr = %remote_addr,
        transport = "webtransport",
        "webtransport session accepted"
    );

    enqueue_control_message(
        &stream_senders[WT_STREAM_SLOT_CONTROL as usize],
        session_init_payload(session_id)?,
    )
    .await?;

    let keepalive_sender = stream_senders[WT_STREAM_SLOT_CONTROL as usize].clone();
    let keepalive_task = tokio::spawn(async move {
        let mut ticker = time::interval(keepalive_interval);
        loop {
            ticker.tick().await;
            if enqueue_control_message(
                &keepalive_sender,
                control_payload("keep_alive", session_id)?,
            )
            .await
            .is_err()
            {
                break;
            }
        }
        Ok::<(), RuntimeError>(())
    });

    connection.closed().await;
    keepalive_task.abort();
    sessions.remove(&session_id);
    // R1b · the live entry lives as long as the QUIC session: drop it now.
    let ended = binding.lock().map(|mut slot| std::mem::take(&mut *slot));
    drop(ended);
    if let Some(broadcast) = broadcast.as_ref() {
        // Eager broadcast cleanup keeps the subscriber map tight
        // even when a session never wrote (so lazy pruning during
        // `write_topic` would never have observed the closed
        // channel). Cheap when the session was subscribed to
        // nothing — a single DashMap remove.
        broadcast.cleanup_session(SessionId::from(session_id));
    }

    info!(
        session_id = %session_id,
        remote_addr = %remote_addr,
        transport = "webtransport",
        "webtransport session closed"
    );

    Ok(())
}

fn spawn_stream_writers(
    connection: Connection,
    stream_buffer_capacity: usize,
    gate: Arc<std::sync::OnceLock<Arc<crate::live_authority::LiveEntry>>>,
) -> [mpsc::Sender<bytes::Bytes>; WEBTRANSPORT_STREAM_COUNT] {
    let (control_tx, control_rx) = mpsc::channel(stream_buffer_capacity);
    let (shell_tx, shell_rx) = mpsc::channel(stream_buffer_capacity);
    let (patch_tx, patch_rx) = mpsc::channel(stream_buffer_capacity);
    let (prefetch_tx, prefetch_rx) = mpsc::channel(stream_buffer_capacity);

    // R1b · control traffic is unbound protocol traffic and is never gated;
    // shell, patches and prefetch carry route data and are.
    spawn_stream_writer(connection.clone(), WT_STREAM_SLOT_CONTROL, control_rx, None);
    spawn_stream_writer(connection.clone(), WT_STREAM_SLOT_SHELL, shell_rx, Some(gate.clone()));
    spawn_stream_writer(connection.clone(), WT_STREAM_SLOT_PATCHES, patch_rx, Some(gate.clone()));
    spawn_stream_writer(connection, WT_STREAM_SLOT_PREFETCH, prefetch_rx, Some(gate));

    [control_tx, shell_tx, patch_tx, prefetch_tx]
}

/// Test seam (R1b AC 11): pause a protected writer after it dequeues a frame and
/// before its admission load. `reached` fires, then it waits on `release`.
pub static WRITER_PAUSE: std::sync::OnceLock<Arc<crate::live_authority::Pause>> =
    std::sync::OnceLock::new();

fn spawn_stream_writer(
    connection: Connection,
    stream_slot: u8,
    mut rx: mpsc::Receiver<bytes::Bytes>,
    gate: Option<Arc<std::sync::OnceLock<Arc<crate::live_authority::LiveEntry>>>>,
) {
    tokio::spawn(async move {
        let mut stream = match connection.open_uni().await {
            Ok(stream) => stream,
            Err(err) => {
                warn!(stream_slot, error = %err, "failed to open WT stream");
                return;
            }
        };

        if let Err(err) =
            write_framed_payload(&mut stream, stream_open_payload(stream_slot).as_bytes()).await
        {
            warn!(stream_slot, error = %err, "failed to write WT stream open frame");
            return;
        }
        // `debug`, not `info`: one line per stream, and a session opens many.
        // Session accept/close above stay at `info` because they are per
        // connection — that is the granularity `RUST_LOG=info` has to remain
        // readable at, or nobody will leave it on long enough to see anything.
        debug!(stream_slot, "webtransport stream opened");

        while let Some(payload) = rx.recv().await {
            if let Some(gate) = &gate {
                if let Some(pause) = WRITER_PAUSE.get() {
                    pause.reached.notify_one();
                    pause.release.notified().await;
                }
                // R1b · handoff admission, at the final writer: the load right
                // before the frame reaches the QUIC send stream.
                // Unbound (`None`) admits nothing; bound, one atomic load.
                let admitted = gate.get().is_some_and(|entry| entry.admits());
                if !admitted {
                    continue;
                }
            }
            if let Err(err) = write_framed_payload(&mut stream, &payload).await {
                warn!(stream_slot, error = %err, "failed to write WT payload");
                break;
            }
        }

        if let Err(err) = stream.finish() {
            warn!(stream_slot, error = %err, "failed to finish WT stream");
        }
    });
}

async fn enqueue_control_message(
    sender: &mpsc::Sender<bytes::Bytes>,
    payload: Vec<u8>,
) -> Result<(), RuntimeError> {
    sender
        .send(bytes::Bytes::from(payload))
        .await
        .map_err(|_| RuntimeError::ServerRuntime("WT control stream channel closed".to_string()))
}

async fn write_framed_payload(stream: &mut SendStream, payload: &[u8]) -> Result<(), RuntimeError> {
    stream
        .write_u32(payload.len() as u32)
        .await
        .map_err(|err| {
            RuntimeError::ServerRuntime(format!("failed to write WT frame length: {err}"))
        })?;
    stream.write_all(payload).await.map_err(|err| {
        RuntimeError::ServerRuntime(format!("failed to write WT frame payload: {err}"))
    })?;
    stream.flush().await.map_err(|err| {
        RuntimeError::ServerRuntime(format!("failed to flush WT frame payload: {err}"))
    })?;
    Ok(())
}

fn stream_open_payload(stream_slot: u8) -> String {
    serde_json::json!({
        "type": "stream_open",
        "stream_slot": stream_slot,
    })
    .to_string()
}

fn session_init_payload(session_id: Uuid) -> Result<Vec<u8>, RuntimeError> {
    serde_json::to_vec(&ControlEnvelope {
        event: "session_init",
        session_id: session_id.to_string(),
    })
    .map_err(|err| {
        RuntimeError::ServerRuntime(format!("failed to serialize WT session init: {err}"))
    })
}

fn control_payload(event: &'static str, session_id: Uuid) -> Result<Vec<u8>, RuntimeError> {
    serde_json::to_vec(&ControlEnvelope {
        event,
        session_id: session_id.to_string(),
    })
    .map_err(|err| {
        RuntimeError::ServerRuntime(format!("failed to serialize WT control payload: {err}"))
    })
}

#[derive(Debug, Clone, Serialize)]
struct ControlEnvelope {
    event: &'static str,
    session_id: String,
}

fn load_cert_chain(
    cert_path: &str,
) -> Result<Vec<rustls_pki_types::CertificateDer<'static>>, RuntimeError> {
    let cert_file = std::fs::File::open(Path::new(cert_path)).map_err(|err| {
        RuntimeError::InvalidConfig(format!(
            "failed to open webtransport.cert_path '{}': {err}",
            cert_path
        ))
    })?;
    let mut cert_reader = BufReader::new(cert_file);
    rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| {
            RuntimeError::InvalidConfig(format!(
                "failed to parse PEM certificates from '{}': {err}",
                cert_path
            ))
        })
}

fn load_private_key(
    key_path: &str,
) -> Result<rustls_pki_types::PrivateKeyDer<'static>, RuntimeError> {
    let key_file = std::fs::File::open(Path::new(key_path)).map_err(|err| {
        RuntimeError::InvalidConfig(format!(
            "failed to open webtransport.key_path '{}': {err}",
            key_path
        ))
    })?;
    let mut key_reader = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader).map_err(|err| {
        RuntimeError::InvalidConfig(format!(
            "failed to parse PEM private key from '{}': {err}",
            key_path
        ))
    })?;

    key.ok_or_else(|| {
        RuntimeError::InvalidConfig(format!(
            "no private key found in webtransport.key_path '{}'",
            key_path
        ))
    })
}
