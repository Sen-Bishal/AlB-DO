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
use dom_render_compiler::runtime::{broadcast_slot_id, BroadcastRegistry, GrantedSeed, TopicGrant};
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
    grants: &[TopicGrant],
    resync: Option<&dyn RowProjector>,
) -> Response<Body> {
    serve_patch_stream_live(broadcast, grants, resync, None).await
}

/// R1b · [`serve_patch_stream`] for a registered live connection.
///
/// `live` is the connection's registry handle. The stream owns it, so the entry
/// lives exactly as long as the connection; every protected frame is handed off
/// only after a load of the entry's state saw `Live` ([`LiveEntry::admits`]),
/// and the stream ends when the entry closes. Closing also drops the broadcast
/// subscription, whether or not the stream is ever polled again.
///
/// [`LiveEntry::admits`]: crate::live_authority::LiveEntry::admits
pub async fn serve_patch_stream_live(
    broadcast: Arc<BroadcastRegistry>,
    grants: &[TopicGrant],
    resync: Option<&dyn RowProjector>,
    live: Option<crate::live_authority::LiveHandle>,
) -> Response<Body> {
    let session = SessionId::random();
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<bytes::Bytes>(PATCH_CHANNEL_CAPACITY);

    // Subscribe BEFORE anything else: `subscribe_granted` registers the sink
    // and reads each topic's value under that topic's linearization lock, so a
    // write racing this connect either lands in `initial` or arrives as a frame
    // afterwards — never both, never neither. Everything below is computed from
    // the values it returned, so the resync describes exactly the state this
    // subscription started from and live frames stack on top of it in order.
    //
    // Column projection · `initial` is cut to the columns the route reads;
    // `seed_values` stays whole because it only feeds the resync *render*, and
    // that render's output is the route's own row markup.
    let GrantedSeed {
        instructions: initial,
        values: seed_values,
    } = broadcast.subscribe_granted(session, sender, grants);

    // R1b · the cleanup guard exists from the moment the subscription does —
    // before the resync await below — so an open cancelled during the resync
    // drops the subscription instead of leaking it. (It used to be built after.)
    let guard = SubscriptionGuard {
        broadcast: broadcast.clone(),
        session,
    };
    if let Some(live) = &live {
        // Closing ends the subscription now, not at the next poll.
        let broadcast = broadcast.clone();
        live.entry().on_close(move || broadcast.cleanup_session(session));
    }

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

    // The guard (built above, outside the generator body) is moved in. A
    // `stream!` body does not run until the stream is first polled, so a guard
    // constructed *inside* it would never exist for a connection that is
    // dropped before its first poll — a client that opens the lane and vanishes
    // would stay subscribed for the life of the process. Moved in at
    // construction, the generator owns it from the start and dropping the
    // stream always unsubscribes. The live handle rides the same way.
    let stream = async_stream::stream! {
        let _guard = guard;
        let live = live;
        let admits = |live: &Option<crate::live_authority::LiveHandle>| {
            live.as_ref().is_none_or(|handle| handle.entry().admits())
        };
        let mut event_id: u64 = 0;

        // `retry:` first, so a client dropped mid-stream already knows the
        // cadence to come back on. Not protected: it carries no data.
        yield Ok::<_, Infallible>(SseEvent::default().retry(RECONNECT_DELAY).comment("open"));

        if let Some(bytes) = seed {
            if !admits(&live) {
                return;
            }
            event_id += 1;
            yield Ok::<_, Infallible>(patch_event(&bytes, event_id));
        }
        if let Some(bytes) = resync_frame {
            if !admits(&live) {
                return;
            }
            event_id += 1;
            yield Ok::<_, Infallible>(patch_event(&bytes, event_id));
        }
        // No per-frame select on the close: closing runs the hook installed
        // above, which drops this connection's senders from every topic, so
        // `recv()` returns `None` and the loop ends on its own.
        while let Some(bytes) = receiver.recv().await {
            // Handoff admission: one atomic load immediately before the yield.
            if !admits(&live) {
                break;
            }
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
/// `pub(crate)` because the PHOSPHOR lane's takeover/reconnect repair is the
/// same resync — one spelling of "re-assert the full desired row sets".
pub(crate) async fn resync_frame(
    projector: &dyn RowProjector,
    seed_values: &[(String, Vec<u8>)],
) -> Option<Vec<u8>> {
    let mut instructions = Vec::new();
    for (topic, value) in seed_values {
        // The seed names a *channel*; the projector is keyed by *collection*.
        // For everything that predates PRISM those are the same string, so that
        // is tried first — which also settles the ambiguity a static topic
        // carrying a colon (`broadcast("chat:lobby")`) would otherwise create.
        // Only when the whole name names no template is it read as a partition.
        let rows = match projector.project_rows(topic, None, value).await {
            Some(rows) => Some(rows),
            None => match dom_render_compiler::runtime::split_partition_topic(topic) {
                Some((collection, key)) => {
                    projector.project_rows(collection, Some(key), value).await
                }
                None => None,
            },
        };
        let Some(rows) = rows else {
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
        let (tx, _rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
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
        let response = serve_patch_stream(broadcast.clone(), &[TopicGrant::whole("guestbook")], None).await;
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
        let _response = serve_patch_stream(broadcast.clone(), &[TopicGrant::whole("guestbook")], None).await;

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
            _partition: Option<&str>,
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
        let response = serve_patch_stream(broadcast.clone(), &[TopicGrant::whole("guestbook")], None).await;
        assert_eq!(broadcast.get("guestbook").unwrap().subscriber_count(), 1);

        drop(response);
        // The guard lives inside the stream body, which the response owns.
        assert_eq!(
            broadcast.get("guestbook").unwrap().subscriber_count(),
            0,
            "a disconnected client must not stay subscribed"
        );
    }

    struct PausedProjection {
        reached: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl RowProjector for PausedProjection {
        async fn project_rows(
            &self,
            _collection: &str,
            _partition: Option<&str>,
            _value: &[u8],
        ) -> Option<dom_render_compiler::forge::RenderedRows> {
            self.reached.notify_one();
            self.release.notified().await;
            Some(
                [("1".to_string(), "<li>private</li>".to_string())]
                    .into_iter()
                    .collect(),
            )
        }
    }

    #[tokio::test]
    async fn cancelling_a_resync_open_removes_both_registrations() {
        use crate::auth::Identity;
        use crate::live_authority::{LiveKind, LiveRegistry};

        let broadcast = Arc::new(BroadcastRegistry::new());
        broadcast.topic("guestbook", b"[]".to_vec());
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register(LiveKind::Sse, &Identity::Anonymous);
        assert!(registry.revalidate(handle.entry(), None).await);
        let projector = PausedProjection {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        };
        let grants = [TopicGrant::whole("guestbook")];
        let mut opening = Box::pin(serve_patch_stream_live(
            broadcast.clone(),
            &grants,
            Some(&projector),
            Some(handle),
        ));
        tokio::select! {
            _ = &mut opening => panic!("projection did not pause"),
            _ = projector.reached.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("projection was not reached"),
        }
        assert_eq!(registry.len(), 1);
        assert_eq!(broadcast.get("guestbook").unwrap().subscriber_count(), 1);
        drop(opening);
        assert!(
            registry.is_empty(),
            "cancelled open retained its live handle"
        );
        assert_eq!(broadcast.get("guestbook").unwrap().subscriber_count(), 0);
    }

    #[tokio::test]
    async fn closing_during_resync_cleans_up_without_polling_and_gates_the_seed() {
        use crate::auth::Identity;
        use crate::live_authority::{LiveKind, LiveRegistry};

        let broadcast = Arc::new(BroadcastRegistry::new());
        broadcast.topic("guestbook", b"[]".to_vec());
        let registry = Arc::new(LiveRegistry::new());
        let handle = registry.register(LiveKind::Sse, &Identity::Anonymous);
        assert!(registry.revalidate(handle.entry(), None).await);
        let entry = handle.entry().clone();
        let projector = PausedProjection {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        };
        let grants = [TopicGrant::whole("guestbook")];
        let mut opening = Box::pin(serve_patch_stream_live(
            broadcast.clone(),
            &grants,
            Some(&projector),
            Some(handle),
        ));
        tokio::select! {
            _ = &mut opening => panic!("projection did not pause"),
            _ = projector.reached.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("projection was not reached"),
        }
        assert!(entry.close());
        assert_eq!(
            broadcast.get("guestbook").unwrap().subscriber_count(),
            0,
            "close must remove the subscription before the open is polled again"
        );
        projector.release.notify_one();
        let response = opening.await;
        let bytes = tokio::time::timeout(
            Duration::from_secs(5),
            axum::body::to_bytes(response.into_body(), 16_384),
        )
        .await
        .unwrap()
        .unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(
            body.contains("retry:"),
            "unprotected opening event still flows"
        );
        assert!(
            !body.contains("event: patch"),
            "seed or resync escaped after close: {body}"
        );
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn queued_sse_frames_stop_at_close_without_disturbing_other_readers() {
        use crate::auth::Identity;
        use crate::live_authority::{LiveKind, LiveRegistry};
        use dom_render_compiler::auth::principal::{Principal, PrincipalId};
        use dom_render_compiler::auth::session::TokenHash;
        use futures_util::StreamExt;

        let registry = Arc::new(LiveRegistry::new());
        let broadcast = Arc::new(BroadcastRegistry::new());
        broadcast.topic("guestbook", b"initial".to_vec());
        let identity = |name: &str| {
            let mut principal = Principal::new(PrincipalId::parse(name).unwrap(), "password");
            principal.session_expires_at = Some(i64::MAX - 1);
            Identity::Authenticated {
                principal: Box::new(principal),
                token: TokenHash::of(name),
            }
        };
        let stale = registry.register(LiveKind::Sse, &identity("u_stale"));
        let other = registry.register(LiveKind::Sse, &identity("u_other"));
        let anonymous = registry.register(LiveKind::Sse, &Identity::Anonymous);
        for handle in [&stale, &other, &anonymous] {
            assert!(handle.entry().promote());
        }
        let entry = stale.entry().clone();
        let grants = [TopicGrant::whole("guestbook")];
        let mut stale_body = serve_patch_stream_live(broadcast.clone(), &grants, None, Some(stale))
            .await
            .into_body()
            .into_data_stream();
        let mut other_body = serve_patch_stream_live(broadcast.clone(), &grants, None, Some(other))
            .await
            .into_body()
            .into_data_stream();
        let mut anonymous_body =
            serve_patch_stream_live(broadcast.clone(), &grants, None, Some(anonymous))
                .await
                .into_body()
                .into_data_stream();
        // Poll through the opening comment and seed, then leave the next
        // protected frame queued immediately before the body's admission load.
        for body in [&mut stale_body, &mut other_body, &mut anonymous_body] {
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_secs(5), body.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
        }
        let before = broadcast
            .write_topic("guestbook", b"queued".to_vec())
            .unwrap();
        assert_eq!(before.delivered, 3);
        assert_eq!(registry.close_principal_below("u_stale", 1), 1);
        assert!(entry.is_closed());
        assert_eq!(broadcast.get("guestbook").unwrap().subscriber_count(), 2);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), stale_body.next())
                .await
                .unwrap()
                .is_none(),
            "queued frame escaped after close"
        );
        assert_eq!(registry.close_principal_below("u_stale", 1), 0);
        // C and the anonymous public reader must each receive every queued
        // publication once, despite sharing B's topic and invalidation scan.
        let after = broadcast
            .write_topic("guestbook", b"after".to_vec())
            .unwrap();
        assert_eq!(after.delivered, 2);
        for body in [&mut other_body, &mut anonymous_body] {
            for (id, value) in [(2, b"queued".as_slice()), (3, b"after".as_slice())] {
                let bytes = tokio::time::timeout(Duration::from_secs(5), body.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let event = std::str::from_utf8(&bytes).unwrap();
                assert!(event.contains(&format!("id: {id}\n")), "{event}");
                let data = event
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap();
                let wire = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap();
                let (frame, _) = decode_frame(&wire).unwrap();
                assert!(matches!(frame.instructions.as_slice(),
                    [Instruction::SlotSet { value: actual, .. }] if actual == value));
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(25), body.next())
                    .await
                    .is_err(),
                "duplicate protected frame received"
            );
        }
        drop(stale_body);
        drop(other_body);
        drop(anonymous_body);
        assert!(registry.is_empty());
        assert_eq!(broadcast.get("guestbook").unwrap().subscriber_count(), 0);
    }

    #[tokio::test]
    async fn cancelled_wide_resync_removes_every_topic_subscription() {
        use crate::auth::Identity;
        use crate::live_authority::{LiveKind, LiveRegistry};

        let registry = Arc::new(LiveRegistry::new());
        let broadcast = Arc::new(BroadcastRegistry::new());
        let grants: Vec<_> = (0..256)
            .map(|i| {
                let topic = format!("topic_{i}");
                broadcast.topic(&topic, b"[]".to_vec());
                TopicGrant::whole(topic)
            })
            .collect();
        let handle = registry.register(LiveKind::Sse, &Identity::Anonymous);
        assert!(handle.entry().promote());
        let projector = PausedProjection {
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        };
        let mut opening = Box::pin(serve_patch_stream_live(
            broadcast.clone(),
            &grants,
            Some(&projector),
            Some(handle),
        ));
        tokio::select! {
            _ = &mut opening => panic!("projection did not pause"),
            _ = projector.reached.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("projection was not reached"),
        }
        for i in 0..256 {
            assert_eq!(
                broadcast
                    .get(&format!("topic_{i}"))
                    .unwrap()
                    .subscriber_count(),
                1
            );
        }
        drop(opening);
        assert!(registry.is_empty());
        for i in 0..256 {
            assert_eq!(
                broadcast
                    .get(&format!("topic_{i}"))
                    .unwrap()
                    .subscriber_count(),
                0
            );
        }
    }
}
