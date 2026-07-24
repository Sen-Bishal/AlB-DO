//! PHOSPHOR · the lane — one connection per browser profile.
//!
//! Design: `development-plan/PHOSPHOR.md`. The sentence it implements:
//! **a browser profile holds one physical connection (the *trunk*); a tab
//! holds zero** — tabs hold route-scoped, server-authorized, refcounted
//! *subscriptions* (*circuits*) that ride the shared trunk.
//!
//! The per-tab lane (`handlers::patches`) billed every tab up to two of the
//! browser's ~6 per-origin HTTP/1.1 connections, so three tabs of `albedo dev`
//! starved the pool and the next request — an action POST, the reload that
//! would have fixed it — queued forever (`TODO.md` § 2d, measured at 27,361 ms
//! for a plain GET that completed the instant another tab closed). The trunk
//! makes connections O(browser profiles) and tabs free.
//!
//! # Contract (inherited from the per-tab lane, then narrowed)
//!
//! - **The server decides the topics, not the client.** The subscribe unit is
//!   a route path, resolved through the same router + manifest the render
//!   used. All resolution goes through ONE choke point ([`RouteAuthority`]) —
//!   the seam item 4's parameterized, identity-checked topics land into.
//! - **Seed precedes live frames, per route.** Guaranteed by the *forwarder
//!   start-gate*: a circuit's registry sink can buffer frames from the moment
//!   `auto_subscribe` registers it (under each topic's linearization lock),
//!   but nothing enters the trunk until the seed has been pushed and the
//!   forwarder is then started. The trunk is FIFO, so the order is proved by
//!   construction — the multiplexed restatement of `serve_patch_stream`'s
//!   yield-order guarantee.
//! - **Bounded queues; a lane that can't drain dies loudly.** A full trunk
//!   means the *browser* can't keep up with its one shared stream. Dropping a
//!   single circuit would leave some routes silently stale on a live
//!   connection — the lying-page failure the registry's whole-session-drop
//!   posture exists to prevent — so the forwarder kills the whole lane and the
//!   client reconnects with resync. Same posture, one level up.
//! - **Caps exist here** because the lane is the first place they are
//!   expressible: lanes per process, routes per lane, and a per-lane
//!   token-bucket on subscribe operations. This closes the uncapped-subscribe
//!   fd-exhaustion hole § 2d called out. Per-*identity* caps attach to
//!   [`Lane::identity`] the day auth lands, with no protocol change.
//!
//! # Wire
//!
//! `GET /_albedo/phosphor[?dev=1]` — SSE. First event `hello`
//! (`{"lane":"…","proto":1}`), then `patch` events whose data is a JSON
//! envelope `{"r":<route>,"n":<join nonce, absent for broadcasts>,"f":<base64
//! OpcodeFrame>}`. In dev mode the trunk also carries the `overlay`/`hmr`
//! events (same JSON payloads as `/_albedo/dev/stream`), so a dev browser
//! holds exactly one connection total.
//!
//! `POST /_albedo/phosphor/routes` — subscribe delta:
//! `{"lane":…,"add":[{"p":…,"n":…,"resync":bool}],"remove":[…]}` →
//! `{"ok":[…],"denied":[…]}`. The lane id is a capability handle: random,
//! unguessable, required on every subscribe, never stored in a cookie.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use dashmap::DashMap;
use dom_render_compiler::forge::RowProjector;
use dom_render_compiler::ir::opcode::{Instruction, OpcodeFrame};
use dom_render_compiler::ir::wire::encode_frame;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::BroadcastRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

/// Lanes a single process will hold. Beyond it the trunk request is refused
/// with 503 + `Retry-After` — an fd ceiling, not a correctness limit.
const MAX_LANES: usize = 1024;

/// Routes one lane may hold circuits for. A browser profile with more than
/// this many *distinct live routes* open at once is not a browsing session,
/// it is a subscribe loop.
const MAX_ROUTES_PER_LANE: usize = 32;

/// Trunk channel depth. Sized above the per-circuit channel so a burst
/// across several routes doesn't kill a healthy lane.
const TRUNK_CHANNEL_CAPACITY: usize = 256;

/// Per-circuit channel depth — matches the per-tab lane's
/// `PATCH_CHANNEL_CAPACITY` posture exactly.
const CIRCUIT_CHANNEL_CAPACITY: usize = 64;

/// Subscribe-op token bucket: a lane may burst this many operations…
const SUBSCRIBE_BURST: f64 = 16.0;
/// …and refills at this rate. Real navigation is well under it; a subscribe
/// loop hits 429 within seconds.
const SUBSCRIBE_REFILL_PER_SEC: f64 = 4.0;

/// Reconnect cadence pushed to the client, same reasoning as the per-tab lane.
const RECONNECT_DELAY: Duration = Duration::from_millis(1_000);

/// SSE event names. `patch` matches the per-tab lane; `hello` is new.
const PATCH_EVENT: &str = "patch";
const HELLO_EVENT: &str = "hello";

// ─── Route authority ─────────────────────────────────────────────────

/// The single choke point where a route path becomes a topic list.
///
/// Today this is `resolve_route_topics` + allow-all — every topic is a global
/// compile-time constant, so nothing is grantable that isn't already public.
/// Item 4 (dynamic topics) changes only an implementation of this trait:
/// parameterized route → parameterized topics, checked against the lane's
/// identity. The subscribe protocol around it does not move. `None` means
/// *denied or unknown* — the caller reports the route in `denied` and
/// subscribes nothing.
pub trait RouteAuthority: Send + Sync {
    fn authorize_route(&self, identity: Option<SessionId>, path: &str) -> Option<Vec<String>>;
    /// The broadcast registry circuits subscribe against. Resolved per
    /// subscribe call (not pinned at trunk-open) so a dev world-swap binds
    /// NEW circuits to the live world; existing circuits keep the registry
    /// they subscribed on and die with their tab's reload.
    fn registry(&self) -> Arc<BroadcastRegistry>;
    /// Row projector for `ReconcileList` resync, when one exists.
    fn projector(&self) -> Option<Arc<dyn RowProjector>>;
}

// ─── Lane state ──────────────────────────────────────────────────────

/// One event on the trunk channel. Dev events don't ride this — they are
/// merged into the SSE stream straight from the dev registries' broadcast
/// channels, which already fan out independently per subscriber.
#[derive(Debug)]
pub(crate) enum TrunkEvent {
    Patch {
        route: Arc<str>,
        /// `Some` targets one joining tab (its seed/resync); `None` is a
        /// live broadcast every tab on the route applies.
        nonce: Option<String>,
        frame: Vec<u8>,
    },
}

/// One (lane, route) circuit: an ordinary `BroadcastRegistry` session whose
/// sink is drained by a tagging forwarder into the trunk.
struct RouteSub {
    session: SessionId,
    /// Pinned at subscribe time — see [`RouteAuthority::registry`].
    registry: Arc<BroadcastRegistry>,
    /// Kept so a later joiner on the same route can be re-seeded through
    /// `auto_subscribe` (which replaces the sink with the same sender —
    /// a no-op replacement — and returns current values under the topic
    /// locks). Dropping the `RouteSub` drops this sender; together with
    /// `cleanup_session` pruning the registry's clones, that closes the
    /// circuit channel and ends the forwarder.
    sender: mpsc::Sender<Vec<u8>>,
    /// The circuit channel's receiver, held until the forwarder is started
    /// (the start-gate). `None` once spawned.
    pending_rx: Option<mpsc::Receiver<Vec<u8>>>,
    refs: usize,
}

/// Simple token bucket; refills continuously, saturates at the burst size.
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new() -> Self {
        Self {
            tokens: SUBSCRIBE_BURST,
            last: Instant::now(),
        }
    }

    fn try_take(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * SUBSCRIBE_REFILL_PER_SEC).min(SUBSCRIBE_BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// One browser profile's lane.
pub struct Lane {
    trunk: mpsc::Sender<TrunkEvent>,
    /// Fires when the lane must die (trunk backpressure). The trunk stream
    /// selects on it and ends, and its drop-guard unsubscribes everything.
    kill: Arc<Notify>,
    routes: Mutex<HashMap<String, RouteSub>>,
    budget: Mutex<TokenBucket>,
    /// The session cookie's id, when the browser presented one. Unused for
    /// authorization today (every topic is public); it is the accounting key
    /// per-identity caps and item-4 authz attach to.
    identity: Option<SessionId>,
}

/// Process-wide lane table. Lives on the persistent side of `RuntimeState`
/// (like the dev registries), so a dev world-swap doesn't orphan open trunks.
#[derive(Default)]
pub struct PhosphorState {
    lanes: DashMap<String, Arc<Lane>>,
}

impl PhosphorState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Diagnostic — number of open lanes.
    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    #[cfg(test)]
    fn get(&self, lane: &str) -> Option<Arc<Lane>> {
        self.lanes.get(lane).map(|entry| entry.clone())
    }
}

// ─── Trunk (GET /_albedo/phosphor) ───────────────────────────────────

/// Dev registries the trunk merges in when the client asked (`?dev=1`) and
/// the server actually runs them. In production both are `None` and the flag
/// is inert — nothing to stream, nothing leaked.
pub struct DevTap {
    pub errors: Option<crate::dev::SharedErrorRegistry>,
    pub hmr: Option<Arc<crate::dev::HmrRegistry>>,
}

impl DevTap {
    pub fn none() -> Self {
        Self {
            errors: None,
            hmr: None,
        }
    }
}

/// Open a trunk: mint the lane, register it, and serve the SSE stream.
///
/// The lane is registered *before* the stream is constructed and the cleanup
/// guard is moved into the generator — the same never-leak-on-early-drop
/// discipline as `serve_patch_stream`. The subscribe POST can only arrive
/// after the client has read `hello` off a polled stream, so registration
/// order is never observable as a race.
pub async fn serve_trunk(
    phosphor: Arc<PhosphorState>,
    dev: DevTap,
    identity: Option<SessionId>,
) -> Response<Body> {
    if phosphor.lanes.len() >= MAX_LANES {
        let mut response = Response::new(Body::from("phosphor lane limit reached"));
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
        return response;
    }

    let lane_id = uuid::Uuid::new_v4().simple().to_string();
    let (trunk_tx, mut trunk_rx) = mpsc::channel::<TrunkEvent>(TRUNK_CHANNEL_CAPACITY);
    let kill = Arc::new(Notify::new());
    let lane = Arc::new(Lane {
        trunk: trunk_tx,
        kill: kill.clone(),
        routes: Mutex::new(HashMap::new()),
        budget: Mutex::new(TokenBucket::new()),
        identity,
    });
    phosphor.lanes.insert(lane_id.clone(), lane);

    // Dev events merge straight from the registries' broadcast channels —
    // they fan out per-subscriber already, so they never touch the trunk
    // channel or compete with patch ordering.
    let mut dev_stream = build_dev_stream(dev);

    let guard = LaneGuard {
        phosphor,
        lane_id: lane_id.clone(),
    };

    let stream = async_stream::stream! {
        let _guard = guard;
        let mut event_id: u64 = 0;

        yield Ok::<_, Infallible>(
            SseEvent::default()
                .retry(RECONNECT_DELAY)
                .event(HELLO_EVENT)
                .data(format!("{{\"lane\":\"{lane_id}\",\"proto\":1}}")),
        );

        loop {
            tokio::select! {
                event = trunk_rx.recv() => {
                    let Some(TrunkEvent::Patch { route, nonce, frame }) = event else {
                        break; // every sender dropped — lane torn down
                    };
                    event_id += 1;
                    yield Ok::<_, Infallible>(patch_event(&route, nonce.as_deref(), &frame, event_id));
                }
                Some(dev_event) = dev_stream.next() => {
                    yield Ok::<_, Infallible>(dev_event);
                }
                _ = kill.notified() => {
                    // Backpressure kill: the browser cannot drain the shared
                    // stream. End loudly; the owner reconnects with resync.
                    break;
                }
            }
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

use futures_util::stream::{self, BoxStream, StreamExt};

/// Merge the dev registries into one endless event stream (`overlay` / `hmr`
/// SSE names, same payloads as `/_albedo/dev/stream`). Absent registries
/// contribute `pending()` so the merged stream never terminates the trunk.
fn build_dev_stream(dev: DevTap) -> BoxStream<'static, SseEvent> {
    use tokio_stream::wrappers::BroadcastStream;

    let overlay: BoxStream<'static, SseEvent> = match dev.errors {
        Some(registry) => BroadcastStream::new(registry.subscribe())
            .filter_map(|item| async move {
                item.ok().map(|event| crate::handlers::dev::render_overlay_event(&event))
            })
            .boxed(),
        None => stream::pending().boxed(),
    };
    let hmr: BoxStream<'static, SseEvent> = match dev.hmr {
        Some(registry) => BroadcastStream::new(registry.subscribe())
            .filter_map(|item| async move {
                item.ok().map(|event| crate::handlers::dev::render_hmr_event(&event))
            })
            .boxed(),
        None => stream::pending().boxed(),
    };
    stream::select(overlay, hmr).boxed()
}

/// Unregisters the lane and every circuit when the trunk stream drops —
/// client gone, network cut, backpressure kill, server shutdown: one path.
struct LaneGuard {
    phosphor: Arc<PhosphorState>,
    lane_id: String,
}

impl Drop for LaneGuard {
    fn drop(&mut self) {
        if let Some((_, lane)) = self.phosphor.lanes.remove(&self.lane_id) {
            let routes = std::mem::take(
                &mut *lane.routes.lock().expect("phosphor route table poisoned"),
            );
            for (_, sub) in routes {
                sub.registry.cleanup_session(sub.session);
            }
        }
    }
}

/// One patch envelope as an SSE event. The `id` is a per-trunk sequence
/// number for observability; the owner's resync protocol doesn't read it
/// (takeover always resubscribes with `resync`), so nothing depends on it.
fn patch_event(route: &str, nonce: Option<&str>, frame: &[u8], id: u64) -> SseEvent {
    #[derive(Serialize)]
    struct Envelope<'a> {
        r: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        n: Option<&'a str>,
        f: String,
    }
    let data = serde_json::to_string(&Envelope {
        r: route,
        n: nonce,
        f: base64::engine::general_purpose::STANDARD.encode(frame),
    })
    .expect("patch envelope serializes");
    SseEvent::default().event(PATCH_EVENT).id(id.to_string()).data(data)
}

// ─── Subscribe (POST /_albedo/phosphor/routes) ───────────────────────

#[derive(Debug, Deserialize)]
pub struct SubscribeRequest {
    pub lane: String,
    #[serde(default)]
    pub add: Vec<AddRoute>,
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddRoute {
    /// Route path, e.g. `/guestbook`.
    pub p: String,
    /// Join nonce: present for a fresh tab join (seed targets exactly that
    /// tab), absent for an owner takeover/reconnect (seed + resync broadcast
    /// to every tab on the route).
    #[serde(default)]
    pub n: Option<String>,
    /// Re-assert full row sets (`ReconcileList`) after the seed — the
    /// takeover/reconnect repair. A fresh page load doesn't ask: its rows
    /// are the HTML that just rendered.
    #[serde(default)]
    pub resync: bool,
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct SubscribeOutcome {
    pub ok: Vec<String>,
    pub denied: Vec<String>,
}

/// HTTP wrapper: parse, rate-limit, delegate, encode.
pub async fn handle_subscribe(
    phosphor: Arc<PhosphorState>,
    authority: &dyn RouteAuthority,
    body: &[u8],
) -> Response<Body> {
    let request: SubscribeRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(err) => return plain(StatusCode::BAD_REQUEST, format!("invalid subscribe: {err}")),
    };
    let Some(lane) = phosphor.lanes.get(&request.lane).map(|entry| entry.clone()) else {
        // Unknown lane: the trunk died (or never existed). 404 tells the
        // owner to reopen the trunk rather than retry the POST.
        return plain(StatusCode::NOT_FOUND, "unknown lane".to_string());
    };
    if !lane.budget.lock().expect("phosphor budget poisoned").try_take() {
        return plain(StatusCode::TOO_MANY_REQUESTS, "subscribe budget exhausted".to_string());
    }

    match subscribe_routes(&lane, authority, request).await {
        Ok(outcome) => {
            let body = serde_json::to_string(&outcome).expect("outcome serializes");
            let mut response = Response::new(Body::from(body));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json; charset=utf-8"),
            );
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(status) => plain(status, "subscribe refused".to_string()),
    }
}

/// The subscribe engine. Public-in-module so tests drive it without HTTP.
///
/// Per added route, in this exact order (the forwarder start-gate):
///
/// 1. authorize → topics (denied routes subscribe nothing);
/// 2. under the route-table lock: create the circuit (or bump its refcount)
///    and `auto_subscribe` — the sink is registered and each topic's value
///    snapshotted under that topic's linearization lock, so frames may begin
///    queueing in the circuit channel but the seed is exact;
/// 3. push the seed frame **directly into the trunk**, nonce-tagged;
/// 4. when `resync`: project the full row sets from the step-2 snapshot
///    (rendering, awaited OUTSIDE every lock) and push the `ReconcileList`
///    frame;
/// 5. only now start the forwarder. The trunk is FIFO: nothing buffered in
///    step 2 can precede the seed of steps 3–4.
async fn subscribe_routes(
    lane: &Arc<Lane>,
    authority: &dyn RouteAuthority,
    request: SubscribeRequest,
) -> Result<SubscribeOutcome, StatusCode> {
    let mut outcome = SubscribeOutcome::default();

    for add in request.add {
        let Some(topics) = authority.authorize_route(lane.identity, &add.p) else {
            outcome.denied.push(add.p);
            continue;
        };

        // ── step 2: circuit + atomic seed snapshot, under the table lock ──
        let (seed_instructions, seed_values, start_gate) = {
            let mut routes = lane.routes.lock().expect("phosphor route table poisoned");
            let sub = match routes.get_mut(&add.p) {
                Some(existing) => {
                    existing.refs += 1;
                    existing
                }
                None => {
                    if routes.len() >= MAX_ROUTES_PER_LANE {
                        return Err(StatusCode::TOO_MANY_REQUESTS);
                    }
                    let (tx, rx) = mpsc::channel::<Vec<u8>>(CIRCUIT_CHANNEL_CAPACITY);
                    routes.insert(
                        add.p.clone(),
                        RouteSub {
                            session: SessionId::random(),
                            registry: authority.registry(),
                            sender: tx,
                            pending_rx: Some(rx),
                            refs: 1,
                        },
                    );
                    routes.get_mut(&add.p).expect("just inserted")
                }
            };
            // Re-subscribing an existing session with the same sender is a
            // sink replacement with itself — a no-op registration that still
            // returns each topic's current value under its lock. That makes
            // "second tab joins an already-live route" and "first tab opens
            // the route" the same code path.
            let instructions = sub
                .registry
                .auto_subscribe(sub.session, sub.sender.clone(), &topics);
            let values: Vec<(String, Vec<u8>)> = topics
                .iter()
                .zip(instructions.iter())
                .filter_map(|(topic, instruction)| match instruction {
                    Instruction::SlotSet { value, .. } => Some((topic.clone(), value.clone())),
                    _ => None,
                })
                .collect();
            (instructions, values, sub.pending_rx.take())
        };

        // ── step 3: seed rides the trunk before anything from the circuit ──
        if !seed_instructions.is_empty() {
            if let Ok(frame) = encode_frame(&OpcodeFrame {
                frame_id: 0,
                component_id: None,
                instructions: seed_instructions,
            }) {
                push_or_kill(lane, &add.p, add.n.clone(), frame);
            }
        }

        // ── step 4: resync from the step-2 snapshot, rendered outside locks ──
        if add.resync {
            if let Some(projector) = authority.projector() {
                if let Some(frame) =
                    crate::handlers::patches::resync_frame(projector.as_ref(), &seed_values).await
                {
                    push_or_kill(lane, &add.p, add.n.clone(), frame);
                }
            }
        }

        // ── step 5: open the gate ──
        if let Some(rx) = start_gate {
            spawn_forwarder(Arc::from(add.p.as_str()), rx, lane.clone());
        }

        outcome.ok.push(add.p);
    }

    for path in request.remove {
        let released = {
            let mut routes = lane.routes.lock().expect("phosphor route table poisoned");
            match routes.get_mut(&path) {
                Some(sub) => {
                    sub.refs = sub.refs.saturating_sub(1);
                    if sub.refs == 0 {
                        routes.remove(&path)
                    } else {
                        None
                    }
                }
                None => None,
            }
        };
        if let Some(sub) = released {
            // Dropping `sub` drops its kept sender; cleanup prunes the
            // registry's clones; the circuit channel closes; the forwarder
            // ends. No task leak, no dangling subscription.
            sub.registry.cleanup_session(sub.session);
        }
    }

    Ok(outcome)
}

/// Push a nonce-tagged frame into the trunk, or kill the lane when the trunk
/// is full — the browser is not draining, and a silently-dropped seed would
/// strand a tab stale on a healthy-looking connection.
fn push_or_kill(lane: &Arc<Lane>, route: &str, nonce: Option<String>, frame: Vec<u8>) {
    let event = TrunkEvent::Patch {
        route: Arc::from(route),
        nonce,
        frame,
    };
    if lane.trunk.try_send(event).is_err() {
        lane.kill.notify_waiters();
    }
}

/// The circuit's forwarder: drain the registry sink into the trunk, tagging
/// each frame with the route. Started only after the seed is on the trunk
/// (the start-gate). A full trunk kills the lane — see the module docs.
fn spawn_forwarder(route: Arc<str>, mut rx: mpsc::Receiver<Vec<u8>>, lane: Arc<Lane>) {
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let event = TrunkEvent::Patch {
                route: route.clone(),
                nonce: None,
                frame,
            };
            if lane.trunk.try_send(event).is_err() {
                lane.kill.notify_waiters();
                return;
            }
        }
    });
}

fn plain(status: StatusCode, message: String) -> Response<Body> {
    let mut response = Response::new(Body::from(message));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::ir::wire::decode_frame;
    use dom_render_compiler::runtime::broadcast_slot_id;

    /// Allow-all authority over a fixed `route → topics` table — the shape
    /// today's `resolve_route_topics` produces.
    struct StaticAuthority {
        registry: Arc<BroadcastRegistry>,
        table: HashMap<String, Vec<String>>,
        projector: Option<Arc<dyn RowProjector>>,
    }

    impl RouteAuthority for StaticAuthority {
        fn authorize_route(&self, _identity: Option<SessionId>, path: &str) -> Option<Vec<String>> {
            self.table.get(path).cloned()
        }
        fn registry(&self) -> Arc<BroadcastRegistry> {
            self.registry.clone()
        }
        fn projector(&self) -> Option<Arc<dyn RowProjector>> {
            self.projector.clone()
        }
    }

    fn authority(routes: &[(&str, &[&str])]) -> StaticAuthority {
        StaticAuthority {
            registry: Arc::new(BroadcastRegistry::new()),
            table: routes
                .iter()
                .map(|(route, topics)| {
                    (
                        route.to_string(),
                        topics.iter().map(|t| t.to_string()).collect(),
                    )
                })
                .collect(),
            projector: None,
        }
    }

    /// A lane whose trunk receiver the test holds, bypassing HTTP/SSE.
    fn test_lane(capacity: usize) -> (Arc<Lane>, mpsc::Receiver<TrunkEvent>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Arc::new(Lane {
                trunk: tx,
                kill: Arc::new(Notify::new()),
                routes: Mutex::new(HashMap::new()),
                budget: Mutex::new(TokenBucket::new()),
                identity: None,
            }),
            rx,
        )
    }

    fn add(path: &str, nonce: Option<&str>, resync: bool) -> SubscribeRequest {
        SubscribeRequest {
            lane: String::new(),
            add: vec![AddRoute {
                p: path.to_string(),
                n: nonce.map(str::to_string),
                resync,
            }],
            remove: Vec::new(),
        }
    }

    fn remove(path: &str) -> SubscribeRequest {
        SubscribeRequest {
            lane: String::new(),
            add: Vec::new(),
            remove: vec![path.to_string()],
        }
    }

    /// The ordering proof, observed: subscribe, then write. The trunk must
    /// carry the nonce-tagged seed FIRST and the live frame after — even
    /// though the write raced in through the circuit channel.
    #[tokio::test]
    async fn seed_rides_the_trunk_nonce_tagged_before_live_frames() {
        let auth = authority(&[("/guestbook", &["guestbook"])]);
        let (lane, mut trunk) = test_lane(16);

        subscribe_routes(&lane, &auth, add("/guestbook", Some("abc"), false))
            .await
            .expect("subscribes");
        auth.registry
            .write_topic("guestbook", br#"[{"id":1}]"#.to_vec())
            .expect("write fans out");

        let TrunkEvent::Patch { route, nonce, frame } =
            trunk.recv().await.expect("seed arrives");
        assert_eq!(&*route, "/guestbook");
        assert_eq!(nonce.as_deref(), Some("abc"), "the seed targets the joiner");
        let (decoded, _) = decode_frame(&frame).unwrap();
        assert!(matches!(
            decoded.instructions.as_slice(),
            [Instruction::SlotSet { .. }]
        ));

        let TrunkEvent::Patch { nonce, frame, .. } =
            trunk.recv().await.expect("live frame follows");
        assert_eq!(nonce, None, "live frames broadcast to the whole route");
        let (decoded, _) = decode_frame(&frame).unwrap();
        assert_eq!(decoded.instructions.len(), 1);
    }

    /// One write, two subscribed routes sharing nothing: each circuit gets
    /// exactly its own route's frames, tagged with its own route.
    #[tokio::test]
    async fn a_write_reaches_each_subscribed_route_exactly_once() {
        let auth = authority(&[("/a", &["alpha"]), ("/b", &["beta"])]);
        let (lane, mut trunk) = test_lane(16);

        subscribe_routes(&lane, &auth, add("/a", None, false)).await.unwrap();
        subscribe_routes(&lane, &auth, add("/b", None, false)).await.unwrap();
        // Drain the two seeds.
        let _ = trunk.recv().await.unwrap();
        let _ = trunk.recv().await.unwrap();

        auth.registry.write_topic("alpha", b"1".to_vec()).unwrap();

        let TrunkEvent::Patch { route, frame, .. } = trunk.recv().await.unwrap();
        assert_eq!(&*route, "/a", "only /a's circuit carries alpha");
        let (decoded, _) = decode_frame(&frame).unwrap();
        match decoded.instructions.as_slice() {
            [Instruction::SlotSet { slot_id, .. }] => {
                assert_eq!(*slot_id, broadcast_slot_id("alpha"));
            }
            other => panic!("expected SlotSet, got {other:?}"),
        }
        assert!(
            trunk.try_recv().is_err(),
            "no frame for /b — beta was not written"
        );
    }

    /// An unknown route is denied, subscribes nothing, and doesn't poison
    /// the rest of the batch.
    #[tokio::test]
    async fn a_denied_route_subscribes_nothing() {
        let auth = authority(&[("/known", &["k"])]);
        let (lane, mut trunk) = test_lane(16);

        let mut request = add("/known", None, false);
        request.add.push(AddRoute {
            p: "/unknown".to_string(),
            n: None,
            resync: false,
        });
        let outcome = subscribe_routes(&lane, &auth, request).await.unwrap();

        assert_eq!(outcome.ok, vec!["/known"]);
        assert_eq!(outcome.denied, vec!["/unknown"]);
        assert_eq!(auth.registry.topic_count(), 1, "only /known's topic exists");
        let _ = trunk.recv().await.unwrap(); // /known's seed
        assert!(trunk.try_recv().is_err());
    }

    /// Refcounts: two joins share one circuit (one registry session); the
    /// second join still receives its own nonce-tagged seed. Removing one
    /// keeps the circuit; removing the last releases it.
    #[tokio::test]
    async fn refcounts_share_the_circuit_and_release_at_zero() {
        let auth = authority(&[("/g", &["g"])]);
        let (lane, mut trunk) = test_lane(16);

        subscribe_routes(&lane, &auth, add("/g", Some("tab1"), false)).await.unwrap();
        subscribe_routes(&lane, &auth, add("/g", Some("tab2"), false)).await.unwrap();
        assert_eq!(
            auth.registry.get("g").unwrap().subscriber_count(),
            1,
            "two tabs, ONE registry session — that is the whole point"
        );
        let TrunkEvent::Patch { nonce, .. } = trunk.recv().await.unwrap();
        assert_eq!(nonce.as_deref(), Some("tab1"));
        let TrunkEvent::Patch { nonce, .. } = trunk.recv().await.unwrap();
        assert_eq!(nonce.as_deref(), Some("tab2"), "the second joiner is re-seeded");

        subscribe_routes(&lane, &auth, remove("/g")).await.unwrap();
        assert_eq!(
            auth.registry.get("g").unwrap().subscriber_count(),
            1,
            "one tab remains — the circuit stays"
        );
        subscribe_routes(&lane, &auth, remove("/g")).await.unwrap();
        assert_eq!(
            auth.registry.get("g").unwrap().subscriber_count(),
            0,
            "last tab gone — the circuit is released"
        );
    }

    /// The route-per-lane cap refuses the 33rd distinct route.
    #[tokio::test]
    async fn the_route_cap_refuses_a_subscribe_loop() {
        let table: Vec<(String, Vec<String>)> = (0..MAX_ROUTES_PER_LANE + 1)
            .map(|i| (format!("/r{i}"), vec![format!("t{i}")]))
            .collect();
        let auth = StaticAuthority {
            registry: Arc::new(BroadcastRegistry::new()),
            table: table.into_iter().collect(),
            projector: None,
        };
        let (lane, _trunk) = test_lane(TRUNK_CHANNEL_CAPACITY);

        for i in 0..MAX_ROUTES_PER_LANE {
            subscribe_routes(&lane, &auth, add(&format!("/r{i}"), None, false))
                .await
                .expect("under the cap");
        }
        let over = subscribe_routes(
            &lane,
            &auth,
            add(&format!("/r{MAX_ROUTES_PER_LANE}"), None, false),
        )
        .await;
        assert_eq!(over, Err(StatusCode::TOO_MANY_REQUESTS));
    }

    /// A full trunk kills the lane instead of silently dropping frames: the
    /// kill notification fires, which ends the SSE stream, whose guard then
    /// unsubscribes everything — the client reconnects and resyncs.
    #[tokio::test]
    async fn trunk_backpressure_kills_the_lane_loudly() {
        let auth = authority(&[("/g", &["g"])]);
        let (lane, _trunk_rx_held_but_never_drained) = test_lane(1);

        let killed = {
            let kill = lane.kill.clone();
            tokio::spawn(async move { kill.notified().await })
        };

        subscribe_routes(&lane, &auth, add("/g", None, false)).await.unwrap();
        // Seed filled the 1-slot trunk; these writes overflow the circuit
        // into a full trunk — the forwarder must pull the kill cord.
        for i in 0..8 {
            auth.registry
                .write_topic("g", format!("{i}").into_bytes())
                .unwrap();
        }

        tokio::time::timeout(Duration::from_secs(1), killed)
            .await
            .expect("kill fires under backpressure")
            .expect("kill task joins");
    }

    /// The token bucket 429s a subscribe storm, then recovers.
    #[test]
    fn the_subscribe_budget_exhausts_and_refills() {
        let mut bucket = TokenBucket::new();
        for _ in 0..SUBSCRIBE_BURST as usize {
            assert!(bucket.try_take(), "burst is allowed");
        }
        assert!(!bucket.try_take(), "the burst edge is refused");
        // Manually rewind the clock instead of sleeping.
        bucket.last = Instant::now() - Duration::from_secs(1);
        assert!(bucket.try_take(), "refill restores tokens");
    }

    /// Same projector double the per-tab lane's resync tests use.
    struct TwoRows;

    #[async_trait::async_trait]
    impl RowProjector for TwoRows {
        async fn project_rows(
            &self,
            collection: &str,
            _value: &[u8],
        ) -> Option<dom_render_compiler::forge::RenderedRows> {
            (collection == "g").then(|| {
                [
                    ("1".to_string(), "<li data-albedo-key=\"1\">ada</li>".to_string()),
                    ("2".to_string(), "<li data-albedo-key=\"2\">alan</li>".to_string()),
                ]
                .into_iter()
                .collect()
            })
        }
    }

    /// A takeover subscribe (`resync`, no nonce) ships seed + ReconcileList,
    /// both broadcast so every surviving tab repairs.
    #[tokio::test]
    async fn a_resync_subscribe_ships_a_broadcast_reconcile_list() {
        let mut auth = authority(&[("/g", &["g"])]);
        auth.projector = Some(Arc::new(TwoRows));
        auth.registry.topic("g", br#"[{"id":1}]"#.to_vec());
        let (lane, mut trunk) = test_lane(16);

        subscribe_routes(&lane, &auth, add("/g", None, true)).await.unwrap();

        let TrunkEvent::Patch { nonce, .. } = trunk.recv().await.expect("seed");
        assert_eq!(nonce, None, "takeover seed is broadcast");
        let TrunkEvent::Patch { nonce, frame, .. } = trunk.recv().await.expect("resync");
        assert_eq!(nonce, None, "takeover resync is broadcast");
        let (decoded, _) = decode_frame(&frame).unwrap();
        assert!(
            matches!(
                decoded.instructions.as_slice(),
                [Instruction::ReconcileList { rows, .. }] if rows.len() == 2
            ),
            "the resync re-asserts the full row set"
        );
    }

    /// Trunk-open registers the lane; dropping the SSE response unregisters
    /// it and releases every circuit — the never-leak guard.
    #[tokio::test]
    async fn dropping_the_trunk_releases_the_lane_and_its_circuits() {
        let phosphor = Arc::new(PhosphorState::new());
        let response = serve_trunk(phosphor.clone(), DevTap::none(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(phosphor.lane_count(), 1);

        // Wire a circuit in by hand, as the subscribe path would.
        let auth = authority(&[("/g", &["g"])]);
        let lane_id = phosphor.lanes.iter().next().unwrap().key().clone();
        let lane = phosphor.get(&lane_id).unwrap();
        subscribe_routes(&lane, &auth, add("/g", None, false)).await.unwrap();
        assert_eq!(auth.registry.get("g").unwrap().subscriber_count(), 1);

        drop(response);
        assert_eq!(phosphor.lane_count(), 0, "the guard removed the lane");
        assert_eq!(
            auth.registry.get("g").unwrap().subscriber_count(),
            0,
            "…and released its circuits"
        );
    }

    /// Over the process lane cap, the trunk is refused with 503 +
    /// Retry-After rather than accepted and starved.
    #[tokio::test]
    async fn the_lane_cap_refuses_new_trunks_loudly() {
        let phosphor = Arc::new(PhosphorState::new());
        for i in 0..MAX_LANES {
            let (tx, _rx) = mpsc::channel(1);
            phosphor.lanes.insert(
                format!("lane{i}"),
                Arc::new(Lane {
                    trunk: tx,
                    kill: Arc::new(Notify::new()),
                    routes: Mutex::new(HashMap::new()),
                    budget: Mutex::new(TokenBucket::new()),
                    identity: None,
                }),
            );
        }
        let response = serve_trunk(phosphor, DevTap::none(), None).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
    }

    /// The HTTP wrapper's contract rows: bad JSON, unknown lane, budget.
    #[tokio::test]
    async fn handle_subscribe_maps_the_failure_modes() {
        let phosphor = Arc::new(PhosphorState::new());
        let auth = authority(&[]);

        let bad = handle_subscribe(phosphor.clone(), &auth, b"not json").await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        let unknown = handle_subscribe(
            phosphor.clone(),
            &auth,
            br#"{"lane":"nope","add":[],"remove":[]}"#,
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }
}
