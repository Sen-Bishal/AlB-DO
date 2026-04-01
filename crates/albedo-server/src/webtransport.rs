use crate::config::WebTransportConfig;
use crate::error::RuntimeError;
use dom_render_compiler::runtime::webtransport::{
    WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES, WT_STREAM_SLOT_PREFETCH, WT_STREAM_SLOT_SHELL,
};
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
use tracing::{info, warn};
use uuid::Uuid;

const WEBTRANSPORT_STREAM_COUNT: usize = 4;

#[derive(Clone)]
pub struct WebTransportSessionHandle {
    pub session_id: Uuid,
    pub remote_addr: SocketAddr,
    pub stream_senders: [mpsc::Sender<Vec<u8>>; WEBTRANSPORT_STREAM_COUNT],
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

        sender.send(payload).await.map_err(|_| {
            RuntimeError::ServerRuntime(format!(
                "WT stream slot {} channel closed for session '{}'",
                stream_slot, session_id
            ))
        })
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
        })
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
                    tokio::spawn(async move {
                        if let Err(err) = handle_connecting(
                            connecting,
                            sessions,
                            keepalive_interval,
                            stream_buffer_capacity,
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
) -> Result<(), RuntimeError> {
    let connection = connecting.await.map_err(|err| {
        RuntimeError::ServerRuntime(format!("failed to accept WT session: {err}"))
    })?;
    let session_id = Uuid::new_v4();
    let remote_addr = connection.remote_address();
    let stream_senders = spawn_stream_writers(connection.clone(), stream_buffer_capacity);

    sessions.insert(WebTransportSessionHandle {
        session_id,
        remote_addr,
        stream_senders: stream_senders.clone(),
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
) -> [mpsc::Sender<Vec<u8>>; WEBTRANSPORT_STREAM_COUNT] {
    let (control_tx, control_rx) = mpsc::channel(stream_buffer_capacity);
    let (shell_tx, shell_rx) = mpsc::channel(stream_buffer_capacity);
    let (patch_tx, patch_rx) = mpsc::channel(stream_buffer_capacity);
    let (prefetch_tx, prefetch_rx) = mpsc::channel(stream_buffer_capacity);

    spawn_stream_writer(connection.clone(), WT_STREAM_SLOT_CONTROL, control_rx);
    spawn_stream_writer(connection.clone(), WT_STREAM_SLOT_SHELL, shell_rx);
    spawn_stream_writer(connection.clone(), WT_STREAM_SLOT_PATCHES, patch_rx);
    spawn_stream_writer(connection, WT_STREAM_SLOT_PREFETCH, prefetch_rx);

    [control_tx, shell_tx, patch_tx, prefetch_tx]
}

fn spawn_stream_writer(connection: Connection, stream_slot: u8, mut rx: mpsc::Receiver<Vec<u8>>) {
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
        info!(stream_slot, "webtransport stream opened");

        while let Some(payload) = rx.recv().await {
            if let Err(err) = write_framed_payload(&mut stream, payload.as_slice()).await {
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
    sender: &mpsc::Sender<Vec<u8>>,
    payload: Vec<u8>,
) -> Result<(), RuntimeError> {
    sender
        .send(payload)
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
