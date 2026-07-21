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
//!
//! # Reconnect, and why a reconnect needs more than a reconnection
//!
//! A client whose sink fills up is dropped by the registry, its channel closes,
//! this stream ends, and `EventSource` reconnects on its own. That restores the
//! *connection*. It does not restore the *page*: the seed a fresh subscription
//! ships is a `SlotSet` per topic, and a `SlotSet` cannot reach a keyed list —
//! a broadcast list anchor has no bind site, it is driven only by `SlotDelta`.
//! So a client that reconnected after missing a delta would be connected,
//! healthy-looking, and still showing rows that no longer exist. That is the
//! precise failure this whole lane was built to end, reintroduced one layer up.
//!
//! So a reconnect is answered with a **resync**: every topic's full desired row
//! set, as a `ReconcileList`. The sink drops rows whose key is no longer in the
//! set, upserts the rest, and skips rows whose markup it already holds — so the
//! cost of recovery is proportional to what actually changed, and a row that was
//! *deleted* while the client was gone is retracted rather than left behind. A
//! positive-only `SlotDelta` could not do that last part: an upsert cannot
//! retract, so a delete missed during a disconnect used to linger as a ghost
//! until navigation. `ReconcileList` (put on the byte wire alongside this) is
//! exactly the shape that closes it.
//!
//! Reconnects are told apart from first connections by `Last-Event-ID`, which
//! the browser echoes automatically because every event here carries an `id`.
//! A first connection therefore pays nothing: its rows are already in the HTML
//! that just rendered.

use axum::body::Body;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use dom_render_compiler::forge::RowProjector;
use dom_render_compiler::ir::opcode::{Instruction, OpcodeFrame, ReconcileRow, RowKey};
use dom_render_compiler::ir::wire::encode_frame;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::{broadcast_slot_id, BroadcastRegistry};
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

/// How long a dropped client waits before reconnecting.
///
/// Sent as SSE's `retry:` so the cadence is ours rather than each browser's
/// default. Short enough that a client dropped for backpressure is stale for
/// about a second, long enough that a server under load isn't reconnect-stormed
/// by every client it just shed.
const RECONNECT_DELAY: Duration = Duration::from_millis(1_000);

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
pub async fn serve_patch_stream(
    broadcast: Arc<BroadcastRegistry>,
    topics: &[String],
    resync: Option<&dyn RowProjector>,
) -> Response<Body> {
    let session = SessionId::random();
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(PATCH_CHANNEL_CAPACITY);

    // Subscribe BEFORE anything else: `auto_subscribe` registers the sink and
    // reads each topic's value under that topic's linearization lock, so a
    // write racing this connect either lands in `initial` or arrives as a frame
    // afterwards — never both, never neither. Everything below is computed from
    // the values it returned, so the resync describes exactly the state this
    // subscription started from and live frames stack on top of it in order.
    let initial = broadcast.auto_subscribe(session, sender, topics);
    let seed_values: Vec<(String, Vec<u8>)> = topics
        .iter()
        .zip(initial.iter())
        .filter_map(|(topic, instruction)| match instruction {
            Instruction::SlotSet { value, .. } => Some((topic.clone(), value.clone())),
            _ => None,
        })
        .collect();

    let seed = (!initial.is_empty())
        .then(|| OpcodeFrame {
            frame_id: 0,
            component_id: None,
            instructions: initial,
        })
        .and_then(|frame| encode_frame(&frame).ok());

    // Only a reconnecting client needs its rows re-asserted; a first connection
    // is already holding the HTML that produced them. Awaited here, before the
    // stream exists, because projecting rows means rendering — and rendering
    // must never happen inside a topic's critical section.
    let resync_frame = match resync {
        Some(projector) => resync_frame(projector, &seed_values).await,
        None => None,
    };

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
        let mut event_id: u64 = 0;

        // `retry:` first, so a client dropped mid-stream already knows the
        // cadence to come back on.
        yield Ok::<_, Infallible>(SseEvent::default().retry(RECONNECT_DELAY).comment("open"));

        if let Some(bytes) = seed {
            event_id += 1;
            yield Ok::<_, Infallible>(patch_event(&bytes, event_id));
        }
        if let Some(bytes) = resync_frame {
            event_id += 1;
            yield Ok::<_, Infallible>(patch_event(&bytes, event_id));
        }
        while let Some(bytes) = receiver.recv().await {
            event_id += 1;
            yield Ok::<_, Infallible>(patch_event(&bytes, event_id));
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

/// One frame re-asserting the full desired row set of every topic, for a client
/// that just reconnected and may have missed frames while it was gone.
///
/// Each topic ships as a `ReconcileList` of its rows in render order. The sink
/// retracts rows whose key is no longer present (the ghost a positive-only
/// upsert could not remove), upserts the rest, and leaves rows whose markup it
/// already holds untouched (DOM identity intact). An empty set legitimately
/// ships — it tells a client that lost the deletion of its last row to clear it
/// — while a topic the projector cannot speak for is skipped rather than guessed
/// at, the same fail-safe rule the write path uses.
async fn resync_frame(
    projector: &dyn RowProjector,
    seed_values: &[(String, Vec<u8>)],
) -> Option<Vec<u8>> {
    let mut instructions = Vec::new();
    for (topic, value) in seed_values {
        let Some(rows) = projector.project_rows(topic, value).await else {
            continue;
        };
        instructions.push(Instruction::ReconcileList {
            slot_id: broadcast_slot_id(topic),
            rows: rows
                .into_iter()
                .map(|(key, html)| ReconcileRow {
                    key: RowKey(key),
                    payload: html.into_bytes(),
                })
                .collect(),
        });
    }
    if instructions.is_empty() {
        return None;
    }
    encode_frame(&OpcodeFrame {
        frame_id: 0,
        component_id: None,
        instructions,
    })
    .ok()
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

/// One frame as an SSE event.
///
/// The `id` exists so the browser echoes `Last-Event-ID` when it reconnects —
/// that header is the only way this handler can tell a returning client from a
/// fresh one, and so the only way it knows to resync. Its value is a
/// per-connection sequence number; nothing reads it back.
fn patch_event(frame: &[u8], id: u64) -> SseEvent {
    SseEvent::default()
        .event(PATCH_EVENT)
        .id(id.to_string())
        .data(base64::engine::general_purpose::STANDARD.encode(frame))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::ir::opcode::Instruction;
    use dom_render_compiler::ir::wire::decode_frame;

    /// The base64 the client decodes must round-trip to the exact frame bytes
    /// the WT lane would have shipped — same vocabulary, different envelope.
    #[tokio::test]
    async fn a_patch_event_carries_the_frame_verbatim() {
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
        let response = serve_patch_stream(broadcast.clone(), &["guestbook".to_string()], None).await;
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
        let response = serve_patch_stream(broadcast.clone(), &[], None).await;
        assert_eq!(response.status(), 200);
        assert_eq!(broadcast.topic_count(), 0, "no topics, no subscriptions");
    }

    /// A write after connect must reach this connection's sink. This is the
    /// link that did not exist before: emitter → registry → *this* session.
    #[tokio::test]
    async fn a_write_after_connect_reaches_the_subscribed_connection() {
        let broadcast = Arc::new(BroadcastRegistry::new());
        let _response = serve_patch_stream(broadcast.clone(), &["guestbook".to_string()], None).await;

        let report = broadcast
            .write_topic("guestbook", br#"[{"id":1}]"#.to_vec())
            .expect("topic exists after auto_subscribe");
        assert_eq!(
            report.delivered, 1,
            "the SSE connection is a live subscriber"
        );
    }

    /// Stands in for the render path when testing the resync.
    struct TwoRows;

    #[async_trait::async_trait]
    impl RowProjector for TwoRows {
        async fn project_rows(
            &self,
            collection: &str,
            _value: &[u8],
        ) -> Option<dom_render_compiler::forge::RenderedRows> {
            (collection == "guestbook").then(|| {
                [
                    ("1".to_string(), "<li data-albedo-key=\"1\">ada</li>".to_string()),
                    ("2".to_string(), "<li data-albedo-key=\"2\">alan</li>".to_string()),
                ]
                .into_iter()
                .collect()
            })
        }
    }

    /// A reconnecting client may have missed frames while it was gone, and the
    /// `SlotSet` seed cannot repair a keyed list — it has no bind site. So the
    /// resync re-asserts the full ordered set as a `ReconcileList`; without it a
    /// client comes back connected, healthy-looking, and showing stale rows —
    /// including a row that was deleted while it was disconnected, which a
    /// positive-only delta could never retract.
    #[tokio::test]
    async fn a_reconnect_re_asserts_the_full_row_set_as_a_reconcile_list() {
        let broadcast = Arc::new(BroadcastRegistry::new());
        broadcast.topic("guestbook", br#"[{"id":1}]"#.to_vec());

        let frame = resync_frame(&TwoRows, &[("guestbook".to_string(), b"[]".to_vec())])
            .await
            .expect("a projectable topic produces a resync");
        let (frame, _) = decode_frame(&frame).unwrap();

        match frame.instructions.as_slice() {
            [Instruction::ReconcileList { slot_id, rows }] => {
                assert_eq!(*slot_id, broadcast_slot_id("guestbook"));
                assert_eq!(rows.len(), 2);
                let keys: Vec<_> = rows.iter().map(|row| row.key.0.as_str()).collect();
                assert_eq!(keys, vec!["1", "2"], "rows ride in render order");
                assert_eq!(
                    String::from_utf8(rows[0].payload.clone()).unwrap(),
                    "<li data-albedo-key=\"1\">ada</li>"
                );
            }
            other => panic!("expected one ReconcileList, got {other:?}"),
        }
    }

    /// A first connection must NOT pay for a resync: the rows it needs are
    /// already in the HTML that just rendered. Only `Last-Event-ID` (absent
    /// here, so the caller passes `None`) buys the extra frame.
    #[tokio::test]
    async fn a_topic_the_projector_cannot_speak_for_is_skipped_not_guessed() {
        assert!(
            resync_frame(&TwoRows, &[("unknown".to_string(), b"[]".to_vec())])
                .await
                .is_none(),
            "no template, no delta — the same fail-safe rule the write path uses"
        );
    }

    /// Dropping the stream must unsubscribe, or every tab a user ever opened
    /// stays in the registry's reverse index for the life of the process.
    #[tokio::test]
    async fn dropping_the_stream_unsubscribes_the_connection() {
        let broadcast = Arc::new(BroadcastRegistry::new());
        let response = serve_patch_stream(broadcast.clone(), &["guestbook".to_string()], None).await;
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
