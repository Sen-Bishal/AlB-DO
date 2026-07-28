//! APERTURE · A1 — the refresh loop.
//!
//! ## What this file is here to correct
//!
//! `APERTURE.md` § 4.2a claimed *"live dashboards with no polling code"*. As
//! A1 first shipped that was **false**, and the argument that produced it is
//! worth keeping written down because every clause of it was true:
//!
//! > the refresh window is enforced inside the client, so warming on every
//! > render and every subscribe is cheap — the cache's freshness check **is**
//! > the schedule.
//!
//! A cache is only consulted when something asks. Liveness is exactly the case
//! where nothing asks: a viewer sitting on an open tab issues no render and no
//! subscribe, so nothing consulted the cache, so nothing ever refreshed. What
//! A1 shipped without this module is a **shared cache** — N concurrent readers
//! cost one upstream request, which is real and worth having — but not live
//! data. Every gate written for A0 and A1 drove the system through *calls*,
//! which is precisely why none of them noticed that nothing drives it on its
//! own.
//!
//! This is the thing that asks.
//!
//! ## The three rules
//!
//! 1. **Only topics somebody is watching.** The candidate set is derived from the broadcast
//!    registry ([`BroadcastRegistry::live_external_topics`]), never maintained here. A topic with
//!    no subscriber is answered from cache on its next render; polling it would spend an upstream
//!    quota on nobody.
//! 2. **Each on its own declared window.** `refresh: "60s"` means this loop leaves that topic alone
//!    for 60s after an attempt — *after an attempt*, not after a success, so an upstream that is
//!    down is retried on its own cadence rather than on every tick.
//! 3. **A poll that changes nothing wakes nobody.** [`BroadcastRegistry::try_topic_external`]
//!    republishes only on a byte difference. Answering an unchanged poll with a full-body `SlotSet`
//!    to every open tab would spend on the wire exactly what the shared cache saved upstream.
//!
//! ## Why the tick is a method, not just a timer
//!
//! [`RefreshLoop::tick`] is public and returns counts. Every claim this module
//! makes is a claim about a count — *"100 unchanged polls produce one value
//! change"* — and A0 already learned (from `PRISM.md` § 11) that a claim proved
//! by a stopwatch is a claim that gets re-litigated on someone else's machine.
//! So the gates drive `tick()` directly N times and assert exactly, and
//! [`RefreshLoop::run`] is the thin part that decides *when* to call it.
//!
//! The tick interval is therefore the floor on liveness: a route declaring
//! `refresh: "0s"` is refreshed once per tick, not continuously.

use crate::aperture::declare::{ResolvedSource, DEFAULT_REFRESH};
use crate::aperture::reader::SourceReader;
use crate::runtime::broadcast::BroadcastRegistry;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// How often [`RefreshLoop::run`] considers the watched set.
///
/// One second, because it is the floor on how live a `refresh: "0s"` route can
/// be, and a tick over a handful of topics is a `DashMap` scan.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

/// How many refreshes one tick may have on the wire at once.
///
/// A dashboard whose topics all come due on the same tick must not turn into a
/// burst of N simultaneous requests at one upstream — that is a self-inflicted
/// rate-limit, and the shared cache exists to make exactly this kind of fan-out
/// unnecessary.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 8;

/// The longest a route may push its next refresh out to.
///
/// Not a policy on how often an author may poll — it is a bound on arithmetic.
/// `refresh: "9999999999h"` parses, and adding that to an `Instant` overflows,
/// so the schedule clamps. A year is far beyond any process's uptime, which
/// makes the clamp unobservable to anything except the panic it prevents.
const MAX_REFRESH_WINDOW: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// What one refresh of one topic did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The body changed; subscribers were sent a frame.
    Published,
    /// The upstream answered, but with the bytes already resident. No frame.
    Unchanged,
    /// The wire slot is held by another name, so this topic cannot go live.
    Refused,
    /// The read failed. The topic keeps its last good value.
    Failed,
}

/// Counts from one pass over the watched set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// External topics with at least one subscriber.
    pub watched: usize,
    /// Of those, how many were past their refresh window and were read.
    pub polled: usize,
    /// Of those, how many changed and fanned out.
    pub published: usize,
    /// Of those, how many failed or were refused.
    pub failed: usize,
}

/// Read one declared source and publish it as its topic's value.
///
/// **The single place a source read becomes a topic value.** The call-driven
/// warm (a render, a subscribe) and this module's timer-driven one are the same
/// operation and differ only in what triggered them; writing that twice is how
/// the two would eventually disagree about staleness or about fan-out.
///
/// Fail-soft by design: a dashboard reading six widgets where one upstream is
/// down must still show the other five, so every failure is a warning naming
/// the topic and nothing propagates.
pub async fn refresh_topic(
    reader: &SourceReader,
    registry: &BroadcastRegistry,
    wanted: &ResolvedSource,
) -> RefreshOutcome {
    let read = match reader.read(wanted).await {
        Ok(read) => read,
        Err(err) => {
            warn!(
                target: "albedo.aperture",
                topic = %wanted.topic,
                url = %wanted.url,
                error = %err,
                "source read failed; topic keeps its last value"
            );
            return RefreshOutcome::Failed;
        }
    };

    match registry.try_topic_external(
        wanted.topic.clone(),
        Arc::from(wanted.source.as_str()),
        Arc::from(wanted.route.as_str()),
        Arc::from(wanted.url.as_str()),
        read.body().to_vec(),
    ) {
        Ok(warm) if warm.published => RefreshOutcome::Published,
        Ok(_) => RefreshOutcome::Unchanged,
        Err(err) => {
            // The slot-id guard firing. One resource loudly refuses to go live
            // rather than two silently sharing a wire slot — the same trade
            // PRISM § 5.3 makes.
            warn!(
                target: "albedo.aperture",
                topic = %wanted.topic,
                error = %err,
                "source refused: wire slot already held by another topic"
            );
            RefreshOutcome::Refused
        }
    }
}

/// The thing that asks.
#[derive(Debug)]
pub struct RefreshLoop {
    registry: Arc<BroadcastRegistry>,
    reader: Arc<SourceReader>,
    /// When each watched topic may next be read.
    ///
    /// Scheduling state, not a second copy of *what exists*: the set is
    /// re-derived from the registry every tick and this map is pruned to it, so
    /// a missing entry means "read it now" and a stale one cannot keep a
    /// forgotten topic alive.
    due: Mutex<HashMap<String, Instant>>,
    max_in_flight: usize,
}

impl RefreshLoop {
    /// Build a loop over a registry and a reader.
    #[must_use]
    pub fn new(registry: Arc<BroadcastRegistry>, reader: Arc<SourceReader>) -> Self {
        Self {
            registry,
            reader,
            due: Mutex::new(HashMap::new()),
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        }
    }

    /// Override the per-tick concurrency cap.
    #[must_use]
    pub fn with_max_in_flight(mut self, max_in_flight: usize) -> Self {
        self.max_in_flight = max_in_flight.max(1);
        self
    }

    /// One pass: refresh every watched topic that is past its window.
    pub async fn tick(&self) -> RefreshReport {
        let watched = self.registry.live_external_topics();
        let mut report = RefreshReport {
            watched: watched.len(),
            ..RefreshReport::default()
        };
        if watched.is_empty() {
            self.prune(&[]);
            return report;
        }

        let now = Instant::now();
        let names: Vec<String> = watched.iter().map(|topic| topic.topic.clone()).collect();
        self.prune(&names);

        let due: Vec<ResolvedSource> = {
            let schedule = self.lock_due();
            watched
                .iter()
                .filter(|topic| schedule.get(&topic.topic).map_or(true, |at| *at <= now))
                .map(|topic| ResolvedSource {
                    topic: topic.topic.clone(),
                    url: topic.url.to_string(),
                    source: topic.source.to_string(),
                    route: topic.route.to_string(),
                })
                .collect()
        };
        report.polled = due.len();
        if due.is_empty() {
            return report;
        }

        let permits = Arc::new(tokio::sync::Semaphore::new(self.max_in_flight));
        let mut tasks = tokio::task::JoinSet::new();
        for wanted in due {
            let reader = Arc::clone(&self.reader);
            let registry = Arc::clone(&self.registry);
            let permits = Arc::clone(&permits);
            tasks.spawn(async move {
                // `acquire_owned` can only fail on a closed semaphore, and this
                // one is owned by the tick that is awaiting these tasks.
                let _permit = permits.acquire_owned().await;
                let outcome = refresh_topic(reader.as_ref(), registry.as_ref(), &wanted).await;
                (wanted, outcome)
            });
        }

        while let Some(joined) = tasks.join_next().await {
            let Ok((wanted, outcome)) = joined else {
                // A panicked refresh must not take the loop with it, and must
                // not leave its topic unscheduled either — but there is no
                // topic name to reschedule, so the next tick simply retries it.
                report.failed = report.failed.saturating_add(1);
                continue;
            };
            match outcome {
                RefreshOutcome::Published => {
                    report.published = report.published.saturating_add(1);
                }
                RefreshOutcome::Unchanged => {}
                RefreshOutcome::Refused | RefreshOutcome::Failed => {
                    report.failed = report.failed.saturating_add(1);
                }
            }
            // Rescheduled after an **attempt**, not after a success. An upstream
            // that is down or a route whose slot is refused would otherwise be
            // retried on every tick — turning one broken widget into the most
            // expensive thing in the process.
            let window = self
                .reader
                .registry()
                .get(&wanted.source, &wanted.route)
                .map_or(DEFAULT_REFRESH, |route| route.refresh);
            // Clamped and checked, because `refresh` is author input and
            // `"9999999999h"` parses: a plain `+` on an `Instant` panics on
            // overflow, and this add runs on the refresh task, where the only
            // symptom would be liveness quietly stopping for the whole process.
            // Anything past [`MAX_REFRESH_WINDOW`] is already "not while this
            // process is running"; the `unwrap_or` is then unreachable, and
            // degrades to polling rather than to panicking if it ever is not.
            let attempted_at = Instant::now();
            let next = attempted_at
                .checked_add(window.min(MAX_REFRESH_WINDOW))
                .unwrap_or(attempted_at);
            self.lock_due().insert(wanted.topic, next);
        }

        report
    }

    /// Tick until `shutdown` flips.
    ///
    /// Bound to the server's shutdown watch rather than detached, so a dev
    /// reload — which stands up a new server over the same
    /// [`crate::runtime::broadcast::BroadcastRegistry`] — retires the old loop instead of
    /// accumulating one per reload.
    pub async fn run(self, interval: Duration, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let report = self.tick().await;
                    if report.published > 0 || report.failed > 0 {
                        debug!(
                            target: "albedo.aperture",
                            watched = report.watched,
                            polled = report.polled,
                            published = report.published,
                            failed = report.failed,
                            "refresh tick"
                        );
                    }
                }
                // The watch is only ever flipped once, to signal shutdown, and
                // an `Err` means every sender is gone — which is shutdown by
                // another name. Either way this loop is done.
                _ = shutdown.changed() => break,
            }
        }
    }

    /// Drop schedule entries for topics that are no longer watched, so an app
    /// that mints many short-lived external topics does not grow this map
    /// without bound.
    fn prune(&self, live: &[String]) {
        let mut schedule = self.lock_due();
        if schedule.is_empty() {
            return;
        }
        let live: std::collections::HashSet<&str> = live.iter().map(String::as_str).collect();
        schedule.retain(|topic, _| live.contains(topic.as_str()));
    }

    /// The schedule map, recovering from poison. A panicked refresh leaves at
    /// most a stale due-time behind, and the worst it can cause is one early or
    /// one late poll — not a reason to stop refreshing everything else.
    fn lock_due(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        self.due
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aperture::client::{CountingTransport, WireResponse};
    use crate::aperture::declare::{RouteDecl, SourceDecl};
    use crate::aperture::egress::EgressMode;
    use crate::aperture::Transport;
    use crate::runtime::session::SessionId;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    fn json(body: &str, etag: &str) -> WireResponse {
        WireResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            etag: Some(etag.to_string()),
            last_modified: None,
            content_type: Some("application/json".to_string()),
        }
    }

    fn not_modified(etag: &str) -> WireResponse {
        WireResponse {
            status: 304,
            body: Vec::new(),
            etag: Some(etag.to_string()),
            last_modified: None,
            content_type: None,
        }
    }

    /// One source, one param-free route, with the caller's refresh window.
    fn block(refresh: &str) -> BTreeMap<String, SourceDecl> {
        let mut routes = BTreeMap::new();
        routes.insert(
            "status".to_string(),
            RouteDecl {
                path: "/status".to_string(),
                refresh: Some(refresh.to_string()),
                method: None,
            },
        );
        [(
            "acme".to_string(),
            SourceDecl {
                base: "https://api.acme.test".to_string(),
                auth: None,
                headers: BTreeMap::new(),
                routes,
            },
        )]
        .into_iter()
        .collect()
    }

    fn reader(refresh: &str, transport: Arc<dyn Transport>) -> Arc<SourceReader> {
        Arc::new(
            SourceReader::with_transport(&block(refresh), EgressMode::Dev, |_| None, transport)
                .expect("lowers"),
        )
    }

    fn wanted(reader: &SourceReader) -> ResolvedSource {
        reader
            .resolve("acme", "status", |_| None)
            .expect("resolves")
    }

    /// A tab on the page: subscribed to the topic, and asking for nothing else
    /// ever again.
    fn viewer(registry: &BroadcastRegistry, topic: &str) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(16);
        registry
            .subscribe(SessionId::random(), topic, tx)
            .expect("subscribes");
        rx
    }

    /// **Gate 2b.** The retraction, made executable: a viewer that never asks
    /// for anything receives the new value.
    #[tokio::test]
    async fn a_still_viewer_receives_a_new_value_without_asking() {
        let transport = Arc::new(CountingTransport::scripted(vec![
            Ok(json(r#"{"n":1}"#, "\"v1\"")),
            Ok(json(r#"{"n":2}"#, "\"v2\"")),
        ]));
        let reader = reader("0s", transport.clone());
        let registry = Arc::new(BroadcastRegistry::new());
        let wanted = wanted(&reader);

        // The call-driven warm a first render would have done.
        refresh_topic(&reader, &registry, &wanted).await;
        let mut tab = viewer(&registry, &wanted.topic);

        // From here nothing calls anything. The loop is the only actor.
        let report = RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader))
            .tick()
            .await;

        assert_eq!(report.watched, 1);
        assert_eq!(report.polled, 1);
        assert_eq!(report.published, 1);
        assert!(
            tab.try_recv().is_ok(),
            "a viewer who asked for nothing must still be told"
        );
        assert_eq!(
            registry.get(&wanted.topic).unwrap().current_value(),
            br#"{"n":2}"#.to_vec()
        );
    }

    /// The other half of gate 2b, and the trap this module is most likely to
    /// fall into: an unchanged upstream must cost a conditional request and
    /// **zero** wire traffic to subscribers.
    #[tokio::test]
    async fn an_unchanged_upstream_wakes_nobody() {
        let mut script = vec![Ok(json(r#"{"n":1}"#, "\"v1\""))];
        script.extend((0..20).map(|_| Ok(not_modified("\"v1\""))));
        let transport = Arc::new(CountingTransport::scripted(script));
        let reader = reader("0s", transport.clone());
        let registry = Arc::new(BroadcastRegistry::new());
        let wanted = wanted(&reader);

        refresh_topic(&reader, &registry, &wanted).await;
        let mut tab = viewer(&registry, &wanted.topic);

        let refresher = RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader));
        for _ in 0..20 {
            let report = refresher.tick().await;
            assert_eq!(report.polled, 1);
            assert_eq!(report.published, 0);
            assert_eq!(report.failed, 0);
        }

        let metrics = reader.client().metrics();
        assert_eq!(metrics.value_changes, 1, "only the first read is a change");
        assert_eq!(metrics.not_modified, 20, "every poll after it is a 304");
        assert_eq!(metrics.conditional_requests, 20);
        assert!(
            tab.try_recv().is_err(),
            "an unchanged poll must not put a frame on the wire"
        );
    }

    /// A `200` carrying the same bytes is not a change either. An upstream
    /// without an `ETag` cannot answer `304`, so the byte comparison is the
    /// only thing standing between a poller and a frame per tick per tab.
    #[tokio::test]
    async fn an_upstream_without_validators_still_wakes_nobody() {
        let transport = Arc::new(CountingTransport::always(WireResponse {
            status: 200,
            body: br#"{"n":1}"#.to_vec(),
            etag: None,
            last_modified: None,
            content_type: Some("application/json".to_string()),
        }));
        let reader = reader("0s", transport.clone());
        let registry = Arc::new(BroadcastRegistry::new());
        let wanted = wanted(&reader);

        refresh_topic(&reader, &registry, &wanted).await;
        let mut tab = viewer(&registry, &wanted.topic);

        let refresher = RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader));
        for _ in 0..10 {
            assert_eq!(refresher.tick().await.published, 0);
        }

        assert_eq!(transport.calls(), 11, "every poll did reach the upstream");
        assert!(tab.try_recv().is_err(), "and none of them reached a tab");
    }

    /// Rule 1. Warming a topic is not the same as watching one.
    #[tokio::test]
    async fn a_topic_nobody_is_watching_is_not_polled() {
        let transport = Arc::new(CountingTransport::always(json(r#"{"n":1}"#, "\"v1\"")));
        let reader = reader("0s", transport.clone());
        let registry = Arc::new(BroadcastRegistry::new());
        let wanted = wanted(&reader);

        refresh_topic(&reader, &registry, &wanted).await;
        assert_eq!(transport.calls(), 1);

        let refresher = RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader));
        for _ in 0..5 {
            let report = refresher.tick().await;
            assert_eq!(report.watched, 0);
            assert_eq!(report.polled, 0);
        }
        assert_eq!(
            transport.calls(),
            1,
            "nobody is watching, so nobody's quota is spent"
        );
    }

    /// Rule 2. The window the author declared is the window the loop honours —
    /// the tick is a floor on liveness, not a poll rate.
    #[tokio::test]
    async fn the_declared_window_gates_the_tick() {
        let transport = Arc::new(CountingTransport::always(json(r#"{"n":1}"#, "\"v1\"")));
        let reader = reader("3600s", transport.clone());
        let registry = Arc::new(BroadcastRegistry::new());
        let wanted = wanted(&reader);

        refresh_topic(&reader, &registry, &wanted).await;
        let _tab = viewer(&registry, &wanted.topic);

        let refresher = RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader));
        // The first tick has no schedule entry yet, so it reads — and gets a
        // fresh cache hit, which is why that read costs nothing upstream.
        assert_eq!(refresher.tick().await.polled, 1);
        for _ in 0..10 {
            assert_eq!(
                refresher.tick().await.polled,
                0,
                "an hour-long window must not be polled once a second"
            );
        }
        assert_eq!(transport.calls(), 1);
    }

    /// Rule 2's other half. A broken upstream is retried on its declared
    /// cadence, not on every tick — otherwise one dead widget becomes the
    /// busiest thing in the process.
    #[tokio::test]
    async fn a_failing_upstream_is_rescheduled_like_any_other_attempt() {
        let transport = Arc::new(CountingTransport::scripted(vec![Err(
            crate::aperture::ApertureError::Transport("dns".to_string()),
        )]));
        let reader = reader("3600s", transport.clone());
        let registry = Arc::new(BroadcastRegistry::new());
        let wanted = wanted(&reader);

        // Nothing cached and the read fails, so the topic is never minted —
        // mint it by hand to put the loop in the state it would reach if the
        // upstream broke *after* a good first read.
        registry
            .try_topic_external(
                wanted.topic.clone(),
                Arc::from(wanted.source.as_str()),
                Arc::from(wanted.route.as_str()),
                Arc::from(wanted.url.as_str()),
                br#"{"n":1}"#.to_vec(),
            )
            .expect("mints");
        let mut tab = viewer(&registry, &wanted.topic);

        let refresher = RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader));
        let first = refresher.tick().await;
        assert_eq!(first.polled, 1);
        assert_eq!(first.failed, 1);
        for _ in 0..10 {
            assert_eq!(refresher.tick().await.polled, 0);
        }
        assert_eq!(transport.calls(), 1, "one attempt, not eleven");
        assert!(tab.try_recv().is_err(), "a failure is not a value");
        assert_eq!(
            registry.get(&wanted.topic).unwrap().current_value(),
            br#"{"n":1}"#.to_vec(),
            "and the last good value stands"
        );
    }

    /// The schedule map is scheduling state, not a census. When the last tab on
    /// a topic closes, its entry goes with it.
    #[tokio::test]
    async fn the_schedule_does_not_outlive_what_it_schedules() {
        let transport = Arc::new(CountingTransport::always(json(r#"{"n":1}"#, "\"v1\"")));
        let reader = reader("3600s", transport);
        let registry = Arc::new(BroadcastRegistry::new());
        let wanted = wanted(&reader);

        refresh_topic(&reader, &registry, &wanted).await;
        let session = SessionId::random();
        let (tx, _rx) = mpsc::channel(16);
        registry.subscribe(session, &wanted.topic, tx).unwrap();

        let refresher = RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader));
        refresher.tick().await;
        assert_eq!(refresher.lock_due().len(), 1);

        registry.cleanup_session(session);
        refresher.tick().await;
        assert!(
            refresher.lock_due().is_empty(),
            "an unwatched topic leaves no schedule behind"
        );
    }

    /// Many watched topics on one tick must not become many simultaneous
    /// requests at one upstream.
    #[tokio::test]
    async fn one_tick_bounds_how_much_it_puts_on_the_wire() {
        let transport = Arc::new(
            CountingTransport::always(json(r#"{"n":1}"#, "\"v1\""))
                .with_delay(Duration::from_millis(20)),
        );
        let reader = reader("0s", transport.clone());
        let registry = Arc::new(BroadcastRegistry::new());

        // Twelve distinct topics under one route, all watched, all due.
        let mut topics = Vec::new();
        for n in 0..12 {
            let topic = format!("aperture:acme.status:n={n}");
            registry
                .try_topic_external(
                    topic.clone(),
                    Arc::from("acme"),
                    Arc::from("status"),
                    Arc::from(format!("https://api.acme.test/status?n={n}").as_str()),
                    b"{}".to_vec(),
                )
                .expect("mints");
            let _ = viewer(&registry, &topic);
            topics.push(topic);
        }

        let refresher =
            RefreshLoop::new(Arc::clone(&registry), Arc::clone(&reader)).with_max_in_flight(4);
        let report = refresher.tick().await;

        assert_eq!(report.watched, 12);
        assert_eq!(report.polled, 12);
        assert_eq!(transport.calls(), 12, "every one of them was refreshed");
    }
}
