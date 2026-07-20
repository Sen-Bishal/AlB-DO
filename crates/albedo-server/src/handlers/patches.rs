//! S4 · the patches lane for sessions without WebTransport.
//!
//! A broadcast write fans an `OpcodeFrame` out to every subscribed session's
//! sink. Until now there was exactly one way to *be* a subscribed session:
//! establish a WebTransport session, which
//! [`stream_route_over_webtransport`](super::streaming) auto-subscribes on
//! connect. WT needs HTTP/3, so on the ordinary `albedo serve` path — plain
//! HTTP/1.1, transport negotiated as `sse` — no page was ever subscribed to
//! anything. The server had an emitter and the client had a sink, and nothing
//! between them: a FORGE write reached the database, rematerialised the topic,
//! fanned out to zero subscribers, and the open page kept showing pre-write
//! rows until someone reloaded.
//!
//! This is that missing wire. `GET /_albedo/patches?p=<page path>` opens an SSE
//! stream, subscribes the connection to exactly the topics the page's route
//! declares, and forwards every frame the broadcast registry pushes.
//!
//! # Contract
//!
//! - **The server decides the topics, not the client.** The query carries the *page path*, which is
//!   resolved through the manifest the same way the render did; the client never names a topic. A
//!   client-supplied topic list would let any page subscribe to any collection's stream — a read
//!   capability nobody granted it.
//! - **Frames are base64 over SSE.** `data:` is line-delimited UTF-8 text and a frame is arbitrary
//!   bincode bytes, so they are base64'd here and decoded on the client into the same
//!   `applyFrameBytes` the WT patches slot feeds. One wire vocabulary, two transports.
//! - **The initial state rides the same lane.** `auto_subscribe` returns one `SlotSet` per topic at
//!   its current value; that ships as the first event, before any live frame, so a page that
//!   connects after its own render still converges rather than waiting for the next write.
//! - **Subscription is per connection.** Each stream mints its own `SessionId` and unsubscribes it
//!   on drop, so a navigated-away tab stops accumulating in the registry rather than waiting to be
//!   pruned by a failed send.

use axum::body::Body;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::BroadcastRegistry;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

/// Frames a slow client may fall behind by before the registry drops it.
///
/// Matches the WT patches stream's posture: a bounded queue and a dropped
/// subscriber, never an unbounded buffer that trades a stalled tab for server
/// memory. A dropped session's page is stale, not corrupt — its next navigation
/// re-subscribes and re-seeds from the topic's current value.
const PATCH_CHANNEL_CAPACITY: usize = 64;

/// SSE event name for a wire frame. The client listens for exactly this.
const PATCH_EVENT: &str = "patch";

/// Open the patches stream, subscribing this connection to `topics`.
///
/// `topics` come from the page's own route manifest, resolved by the caller —
/// which already owns the router and the manifest, and is the only place that
/// can answer "what does *this* page read" authoritatively. Keeping the lookup
/// out here is also what keeps this function a pure function of its arguments.
///
/// A route with no topics still gets a stream: it costs one idle keep-alive and
/// means the client has one unconditional code path rather than a conditional
/// one whose "no stream" branch only shows up on some pages.
pub fn serve_patch_stream(broadcast: Arc<BroadcastRegistry>, topics: &[String]) -> Response<Body> {
    let session = SessionId::random();
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(PATCH_CHANNEL_CAPACITY);

    // Subscribe BEFORE the stream is polled: `auto_subscribe` registers the
    // sink and reads each topic's value under that topic's linearization lock,
    // so a write racing this connect either lands in `initial` or arrives as a
    // frame afterwards — never both, never neither.
    let initial = broadcast.auto_subscribe(session, sender, topics);
    let seed = (!initial.is_empty())
        .then(|| dom_render_compiler::ir::opcode::OpcodeFrame {
            frame_id: 0,
            component_id: None,
            instructions: initial,
        })
        .and_then(|frame| dom_render_compiler::ir::wire::encode_frame(&frame).ok());

    // Built HERE, outside the generator body, and moved in. A `stream!` body
    // does not run until the stream is first polled, so a guard constructed
    // *inside* it would never exist for a connection that is dropped before
    // its first poll — a client that opens the lane and vanishes would stay
    // subscribed for the life of the process. Moved in at construction, the
    // generator owns it from the start and dropping the stream always
    // unsubscribes.
    let guard = SubscriptionGuard { broadcast, session };

    let stream = async_stream::stream! {
        let _guard = guard;

        if let Some(bytes) = seed {
            yield Ok::<_, Infallible>(patch_event(&bytes));
        }
        while let Some(bytes) = receiver.recv().await {
            yield Ok::<_, Infallible>(patch_event(&bytes));
        }
    };

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

/// Drops this connection's subscription when its stream ends.
struct SubscriptionGuard {
    broadcast: Arc<BroadcastRegistry>,
    session: SessionId,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.broadcast.cleanup_session(self.session);
    }
}

fn patch_event(frame: &[u8]) -> SseEvent {
    SseEvent::default()
        .event(PATCH_EVENT)
        .data(base64::engine::general_purpose::STANDARD.encode(frame))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::ir::opcode::Instruction;
    use dom_render_compiler::ir::wire::decode_frame;

    /// The base64 the client decodes must round-trip to the exact frame bytes
    /// the WT lane would have shipped — same vocabulary, different envelope.
    #[test]
    fn a_patch_event_carries_the_frame_verbatim() {
        let registry = BroadcastRegistry::new();
        registry.topic("guestbook", b"[]".to_vec());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let initial = registry.auto_subscribe(SessionId::random(), tx, &["guestbook".to_string()]);
        let frame = dom_render_compiler::ir::opcode::OpcodeFrame {
            frame_id: 0,
            component_id: None,
            instructions: initial,
        };
        let bytes = dom_render_compiler::ir::wire::encode_frame(&frame).unwrap();

        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, bytes);

        let (round_tripped, _) = decode_frame(&decoded).unwrap();
        assert!(matches!(
            round_tripped.instructions.as_slice(),
            [Instruction::SlotSet { .. }]
        ));
    }

    /// Opening the stream must subscribe the connection — the whole point.
    /// A route with topics that produced no subscriber is the exact silent
    /// failure this handler exists to end.
    #[tokio::test]
    async fn opening_the_stream_subscribes_the_connection_to_the_routes_topics() {
        let broadcast = Arc::new(BroadcastRegistry::new());
        let response = serve_patch_stream(broadcast.clone(), &["guestbook".to_string()]);
        assert_eq!(response.status(), 200);

        let topic = broadcast
            .get("guestbook")
            .expect("auto_subscribe registers an unknown topic");
        assert_eq!(
            topic.subscriber_count(),
            1,
            "the connection must be a subscriber"
        );
    }

    /// A page that reads nothing still gets a valid stream rather than an
    /// error: the client's lane is unconditional, and a 404 here would surface
    /// as a console error on pages that are working fine.
    // `#[tokio::test]`, not `#[test]`: building the response arms the SSE
    // keep-alive timer, which needs a reactor.
    #[tokio::test]
    async fn a_route_without_topics_yields_an_empty_but_valid_stream() {
        let broadcast = Arc::new(BroadcastRegistry::new());
        let response = serve_patch_stream(broadcast.clone(), &[]);
        assert_eq!(response.status(), 200);
        assert_eq!(broadcast.topic_count(), 0, "no topics, no subscriptions");
    }

    /// A write after connect must reach this connection's sink. This is the
    /// link that did not exist before: emitter → registry → *this* session.
    #[tokio::test]
    async fn a_write_after_connect_reaches_the_subscribed_connection() {
        let broadcast = Arc::new(BroadcastRegistry::new());
        let _response = serve_patch_stream(broadcast.clone(), &["guestbook".to_string()]);

        let report = broadcast
            .write_topic("guestbook", br#"[{"id":1}]"#.to_vec())
            .expect("topic exists after auto_subscribe");
        assert_eq!(
            report.delivered, 1,
            "the SSE connection is a live subscriber"
        );
    }

    /// Dropping the stream must unsubscribe, or every tab a user ever opened
    /// stays in the registry's reverse index for the life of the process.
    #[tokio::test]
    async fn dropping_the_stream_unsubscribes_the_connection() {
        let broadcast = Arc::new(BroadcastRegistry::new());
        let response = serve_patch_stream(broadcast.clone(), &["guestbook".to_string()]);
        assert_eq!(broadcast.get("guestbook").unwrap().subscriber_count(), 1);

        drop(response);
        // The guard lives inside the stream body, which the response owns.
        assert_eq!(
            broadcast.get("guestbook").unwrap().subscriber_count(),
            0,
            "a disconnected client must not stay subscribed"
        );
    }
}
