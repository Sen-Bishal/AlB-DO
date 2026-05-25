//! Phase O.2 · runnable broadcast demo.
//!
//! Single-file, HTTP-only chat that exercises the broadcast substrate
//! end-to-end without QUIC certs or a real JSX renderer. The whole
//! flow is:
//!
//! 1. A client opens `GET /sse` — server mints a session id, hands
//!    back the SSE event stream, and subscribes the session to topic
//!    `"chat:lobby"` via [`BroadcastRegistry::auto_subscribe`].
//! 2. A client `POST /post` with `{"from": "...", "text": "..."}`
//!    appends to the message list and calls `write_topic` — every
//!    subscribed SSE stream gets the new state as a base64-encoded
//!    `SlotSet` opcode frame.
//! 3. Open two terminals (`curl -N http://127.0.0.1:3000/sse`),
//!    `POST /post` from a third, and watch both SSE streams receive
//!    the same patch. Two-tab chat over a binary wire, the dumb
//!    client just needs to decode and apply.
//!
//! Run:
//!
//! ```bash
//! cargo run -p albedo-server --example chat_broadcast
//! curl -N http://127.0.0.1:3000/sse                                 # terminal 1
//! curl -N http://127.0.0.1:3000/sse                                 # terminal 2
//! curl -X POST http://127.0.0.1:3000/post \
//!     -H 'content-type: application/json' \
//!     -d '{"from":"alice","text":"hi"}'                             # terminal 3
//! ```
//!
//! The renderer integration that auto-wires `useSharedSlot("chat:lobby")`
//! to this same `BroadcastRegistry` is the next session's work.
//! This example sticks to the userland Rust API so the substrate is
//! testable now.

use albedo_server::BroadcastRegistry;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Response, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use dom_render_compiler::runtime::SessionId;
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

const TOPIC: &str = "chat:lobby";
const CHANNEL_CAPACITY: usize = 32;

#[derive(Clone)]
struct AppState {
    broadcast: Arc<BroadcastRegistry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    from: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct PostRequest {
    from: String,
    text: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState {
        broadcast: Arc::new(BroadcastRegistry::new()),
    };
    state
        .broadcast
        .topic(TOPIC, serde_json::to_vec(&Vec::<Message>::new())?);

    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/post", post(post_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("chat-broadcast demo listening on http://127.0.0.1:3000");
    println!("  GET  /sse   — subscribe to {TOPIC}");
    println!("  POST /post  — body {{\"from\":\"...\",\"text\":\"...\"}}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Per-session subscription. Mints a `SessionId`, opens a bounded
/// channel, hands the sender to [`BroadcastRegistry::auto_subscribe`],
/// then converts the receiver into an SSE event stream.
///
/// The bakabox runtime would consume the same `Vec<u8>` payloads off
/// the WT patches lane verbatim — here we wrap each frame in a
/// base64-encoded SSE event so a `curl -N` client can read them
/// without a binary protocol.
async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session = SessionId::random();
    let (tx, rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let initial = state
        .broadcast
        .auto_subscribe(session, tx, &[TOPIC.to_string()]);

    // Initial SlotSet payload — convert each opcode to an SSE event
    // so the client renders the current chat state immediately.
    let initial_events: Vec<Event> = initial
        .into_iter()
        .filter_map(|instr| {
            if let dom_render_compiler::ir::opcode::Instruction::SlotSet { value, .. } = instr {
                Some(Event::default().event("slot_set").data(b64(&value)))
            } else {
                None
            }
        })
        .collect();

    let initial_stream = tokio_stream::iter(initial_events.into_iter().map(Ok));

    let live_stream = ReceiverStream::new(rx).map(|payload: Vec<u8>| {
        // Each payload is one bincode-encoded `OpcodeFrame`. The
        // demo client unwraps and decodes; the production bakabox
        // runtime treats it identically.
        Ok::<_, Infallible>(Event::default().event("slot_set").data(b64(&payload)))
    });

    Sse::new(initial_stream.chain(live_stream))
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Append the incoming message to the topic's state and broadcast.
/// The state is the JSON array of every message seen so far —
/// real-world apps would store this externally and pass only the
/// delta; the demo keeps it self-contained.
async fn post_handler(
    State(state): State<AppState>,
    _headers: HeaderMap,
    Json(req): Json<PostRequest>,
) -> Response<Body> {
    let topic = match state.broadcast.get(TOPIC) {
        Some(t) => t,
        None => return error_response(StatusCode::INTERNAL_SERVER_ERROR, "topic missing"),
    };

    let current = topic.current_value();
    let mut messages: Vec<Message> = if current.is_empty() {
        Vec::new()
    } else {
        match serde_json::from_slice(&current) {
            Ok(v) => v,
            Err(err) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("malformed topic state: {err}"),
                );
            }
        }
    };
    messages.push(Message { from: req.from, text: req.text });

    let bytes = match serde_json::to_vec(&messages) {
        Ok(b) => b,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("serialize failed: {err}"),
            );
        }
    };

    let report = match state.broadcast.write_topic(TOPIC, bytes) {
        Ok(r) => r,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("broadcast write failed: {err}"),
            );
        }
    };

    let body = serde_json::json!({
        "delivered": report.delivered,
        "dropped_full": report.dropped_full.len(),
        "dropped_closed": report.dropped_closed.len(),
        "total_messages": messages.len(),
    });
    let body = serde_json::to_vec(&body).unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| error_response(StatusCode::INTERNAL_SERVER_ERROR, "response build"))
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    (status, message.to_string()).into_response()
}

/// Tiny base64 encoder — pulled inline so the example doesn't drag
/// in a `base64` crate dependency. Plain RFC 4648 alphabet, no
/// padding shortcut (we always pad).
fn b64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(((bytes.len() + 2) / 3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}
