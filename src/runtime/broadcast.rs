//! Phase O.2 · CHAIN REACTION — broadcast slot registry.
//!
//! Phase H's [`crate::runtime::slot_store::SlotStore`] holds reactive
//! state per session: every `(SessionId, SlotId)` pair has its own
//! value. That's the right model for `useState` (each user's counter
//! is private) and the wrong model for shared state (a chat room
//! every connected tab can see). This module is the second store
//! living alongside the first — keyed by *topic* and fanning writes
//! out as [`Instruction::SlotSet`] opcodes to every session that
//! subscribed.
//!
//! ## Architectural contract
//!
//! - **Topics, not sessions.** A topic (`"chat:room-42"`) is global.
//!   Sessions subscribe to topics, not the other way round.
//! - **Wire reuse.** Fan-out emits `Instruction::SlotSet` wrapped in an
//!   `OpcodeFrame`; the client can't tell whether a SlotSet came from a
//!   per-session write or a broadcast — it just applies the value to
//!   the slot. Since S4 a fan-out MAY carry a second instruction, see
//!   the delta lane below.
//! - **Deterministic slot IDs.** A topic string hashes (FNV-1a-32 of
//!   `"broadcast::{topic}"`) to its `SlotId`. The `"broadcast::"`
//!   prefix avoids collision with Phase K's per-session slot IDs
//!   (`"{module}::{fn}#{idx}"`).
//! - **Server-side state, dumb client.** Subscriptions live on the
//!   server. The bakabox runtime just receives opcodes on the patches
//!   lane — same dispatch as any other patch.
//!
//! ## S4 · the delta lane
//!
//! [`BroadcastRegistry::write_topic_delta`] is the second write path:
//! the same topic transition expressed *twice* in one frame —
//! `SlotSet` (the authoritative post-write snapshot) followed by
//! `SlotDelta` (the z-set describing what changed). Both are needed,
//! and the order is load-bearing:
//!
//! - `SlotSet` keeps every **value** consumer of the topic correct —
//!   scalar bind sites, coarse `html`-tier bindings, aggregations —
//!   without each of them having to understand rows.
//! - `SlotDelta` drives the **keyed-list** sink, which `SlotSet` cannot
//!   reach: a broadcast list anchor (the Tier-B `<ul>` the B2 pass
//!   stamped) carries no bind site, so a snapshot alone would leave it
//!   painted with pre-write rows until a reload.
//!
//! The two sinks are disjoint (`bindings` vs `listSlots`), so a
//! consumer never applies the same change twice; delta-last means a
//! list anchor that *was* rebuilt by a coarse `html` site is
//! reconciled against its fresh DOM, not a detached one.
//!
//! ## Ordering (why the value mutex is held across fan-out)
//!
//! A `SlotSet` is last-write-wins: reorder two of them and the topic
//! still converges. A `SlotDelta` is **not** self-healing — it is a
//! function of the state it was computed against, so a delta that
//! reaches a client out of order corrupts that client's list until it
//! reloads. The topic's value mutex is therefore the topic's
//! *linearization lock*: a write holds it across
//! read-prev → compute → store → encode → fan-out, and a subscribe
//! holds it across register-sink → read-snapshot. That buys three
//! invariants worth the (uncontended, non-blocking) cost:
//!
//! 1. A delta is always computed against the state the *previous*
//!    fanned-out delta produced — writers serialize per topic.
//! 2. Delivery order equals lock order, so `apply(Δ₁..Δₙ) == stored
//!    value` holds for every subscriber.
//! 3. A joining session's snapshot is either strictly before or
//!    strictly after a write, never interleaved with it — so it can
//!    never apply a delta already folded into its snapshot, nor miss
//!    one that isn't.
//!
//! Fan-out under the lock is `try_send` only — no I/O, no `.await` —
//! and the lock is per topic, so unrelated topics never contend.
//!
//! ## Backpressure policy
//!
//! Fan-out uses non-blocking [`tokio::sync::mpsc::Sender::try_send`].
//! A full channel means the client is consuming slower than the
//! server writes; the offending session is dropped from the topic
//! and surfaces in [`BroadcastDelivery::dropped_full`]. A closed
//! channel means the session disconnected; it's removed immediately
//! and surfaces in [`BroadcastDelivery::dropped_closed`]. Either way,
//! one slow consumer cannot stall delivery to fast ones — the
//! framework's reactivity invariant is "writes are eventually
//! visible to subscribers that are still alive."
//!
//! ## Cleanup
//!
//! Sessions disconnect. Without explicit cleanup, the subscriber
//! map grows monotonically. [`BroadcastRegistry::cleanup_session`]
//! walks the reverse index (session → set of topics) in O(k) where
//! k is the number of topics this session was subscribed to, then
//! removes that session from each topic's subscriber map. Call it
//! from the WT session-drop hook.

use crate::ir::opcode::{Instruction, OpcodeFrame, ReconcileRow, RowKey, SlotChange, SlotId};
use crate::ir::wire::{encode_frame, WireError};
use crate::runtime::eval::component::fnv1a_32;
use crate::runtime::session::SessionId;
use dashmap::DashMap;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::mpsc;

/// Subscriber sink. Each session registers one of these per topic;
/// the [`BroadcastRegistry`] pushes already-encoded
/// [`OpcodeFrame`] bytes (one per write) which the WT layer
/// forwards verbatim to stream slot 2 (patches).
pub type BroadcastSender = mpsc::Sender<Vec<u8>>;

/// One topic's state. Constructed by [`BroadcastRegistry::topic`]
/// and held behind an `Arc` so handlers can clone cheaply and write
/// without re-resolving the topic by string each time.
#[derive(Debug)]
pub struct BroadcastTopic {
    /// Human-readable key. Kept for diagnostics, logs, and tests;
    /// the wire never sees this string.
    name: String,
    /// Wire-level slot id. Derived once from `name` via
    /// [`broadcast_slot_id`]; stable across processes.
    slot_id: SlotId,
    /// Current value, and — since S4 — the topic's **linearization
    /// lock**. Late subscribers read it so they don't render an empty
    /// topic on first paint; writers hold it across the whole
    /// compute → store → fan-out sequence so delta order on the wire
    /// matches the order the value moved through (see the module
    /// docs). `Mutex<Vec<u8>>` rather than `RwLock` because writes are
    /// common and the read path already clones the bytes out before
    /// doing anything else — and because a shared read lock could not
    /// serialize writers, which is the whole point.
    value: Mutex<Vec<u8>>,
    /// Subscribed sessions. `DashMap` so an in-progress write iterating
    /// subscribers doesn't block a fresh subscribe on the same topic.
    subscribers: DashMap<SessionId, BroadcastSender>,
}

impl BroadcastTopic {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn slot_id(&self) -> SlotId {
        self.slot_id
    }

    /// Clone of the current value. Used by a freshly-subscribed
    /// session to render the initial state without waiting for the
    /// next write.
    pub fn current_value(&self) -> Vec<u8> {
        self.lock_value().clone()
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }

    /// Take the topic's linearization lock, **recovering from poison**.
    ///
    /// A poisoned mutex here means some other thread panicked while
    /// holding it. The protected datum is a plain `Vec<u8>` that is
    /// always replaced wholesale, so it cannot be left half-updated —
    /// there is no torn state to protect anyone from. Propagating the
    /// poison instead (or, as this path used to, silently skipping the
    /// write on `Err`) would turn one unrelated panic into a topic
    /// that accepts writes and never delivers them: a permanently
    /// stale page with no error anywhere.
    fn lock_value(&self) -> MutexGuard<'_, Vec<u8>> {
        self.value.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The post-write state of a topic, as produced by the closure passed to
/// [`BroadcastRegistry::write_topic_delta`]: the authoritative snapshot and
/// the z-set delta that carries the same transition incrementally.
///
/// Both describe *one* transition and must agree — the standing oracle is
/// `apply(previous, changes) == value`. The registry cannot check that
/// (it has no row model; rendering rows is a view concern), so the caller
/// that computes both is responsible for deriving them from the same
/// pre-state. That is exactly why this arrives as a closure over the
/// previous bytes rather than as two loose arguments: the read of the
/// pre-state and the write of the post-state happen inside one critical
/// section, so a concurrent writer cannot slip between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicTransition {
    /// Authoritative post-write bytes. Becomes the topic's stored value and
    /// ships as `SlotSet` — the truth a reload, a late joiner, or a coarse
    /// value binding sees.
    pub value: Vec<u8>,
    /// How the keyed-list rows changed, if this topic drives one. Ships after
    /// the `SlotSet` as the matching list opcode (or nothing).
    pub update: ListUpdate,
}

/// The keyed-list half of a topic write: how to bring subscribers' rows to
/// match the new value, chosen by the caller from how the collection actually
/// changed.
///
/// Two shapes, because two are all the wire needs. A `SlotDelta` is `O(|Δ|)`
/// and preserves node identity, but its inserts land at the tail — so it is
/// only correct for an order-preserving tail append. Anything else (a reorder,
/// a mid-list insert, a first write off a `null` placeholder) ships the full
/// ordered set as a `ReconcileList`, which is `O(|view|)` on the wire but the
/// only shape that can express position. The caller picks; the registry just
/// encodes what it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListUpdate {
    /// Nothing row-shaped changed — the frame degenerates to a plain `SlotSet`.
    None,
    /// Order-preserving tail append, as signed row changes. Ships as
    /// [`Instruction::SlotDelta`] after [`coalesce_changes`].
    Delta(Vec<SlotChange>),
    /// The full desired row set, in order. Ships as
    /// [`Instruction::ReconcileList`]; used for reorder, mid-insert, resync, or
    /// any transition a tail-append cannot reproduce.
    Reconcile(Vec<ReconcileRow>),
}

/// Outcome of a [`BroadcastRegistry::write_topic`] call. The numbers
/// add up: `delivered + dropped_full + dropped_closed == initial
/// subscriber count`, modulo the race where a session unsubscribes
/// mid-write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BroadcastDelivery {
    /// Sessions whose patches-lane channel accepted the bytes.
    pub delivered: usize,
    /// Sessions whose channel was full at write time. They were
    /// removed from the topic; reconnecting re-subscribes.
    pub dropped_full: Vec<SessionId>,
    /// Sessions whose channel was closed (typically because the
    /// receiver task ended — disconnect, panic, shutdown). They were
    /// removed from the topic.
    pub dropped_closed: Vec<SessionId>,
}

impl BroadcastDelivery {
    pub fn drop_count(&self) -> usize {
        self.dropped_full.len() + self.dropped_closed.len()
    }
}

/// Errors that can surface from the broadcast write path.
/// Wire-encode failure is the only error here that isn't recoverable
/// at the caller level — every other condition (no subscribers, full
/// channel, closed channel) is reported through
/// [`BroadcastDelivery`] so a write can be a partial success.
#[derive(Debug, thiserror::Error)]
pub enum BroadcastError {
    #[error("topic '{0}' is not registered")]
    UnknownTopic(String),
    #[error("failed to encode broadcast frame: {0}")]
    Encode(#[from] WireError),
}

/// Process-global broadcast registry. Hold one per server (clone the
/// `Arc` into the WT runtime, into route handlers, into action
/// handlers) so every write resolves against the same topic table.
#[derive(Debug, Default)]
pub struct BroadcastRegistry {
    /// Topic name → topic state. `Arc<BroadcastTopic>` so handlers
    /// can hand-off a topic reference without holding a DashMap
    /// entry guard across `.await` points.
    topics: DashMap<String, Arc<BroadcastTopic>>,
    /// Reverse index: session → set of topics that session is
    /// subscribed to. The slot-store-style write path doesn't need
    /// this, but [`Self::cleanup_session`] does — without it,
    /// cleanup is O(topics × subscribers) instead of O(topics-for-session).
    by_session: DashMap<SessionId, FxHashSet<String>>,
    /// Monotonic frame id allocator. Each broadcast `OpcodeFrame`
    /// gets a fresh id so reassembly (per the wire contract in
    /// [`crate::ir::opcode::OpcodeFrame`]) treats it as a standalone
    /// frame even when the underlying WT transport splits it.
    next_frame_id: AtomicU64,
}

impl BroadcastRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `topic`, creating it on first reference. Returns the
    /// `Arc<BroadcastTopic>` so callers can keep a stable handle.
    /// `initial` seeds the topic value when it is created; on a
    /// second call with the same `topic` string the existing value
    /// is preserved (the `initial` argument is ignored).
    pub fn topic(&self, topic: impl Into<String>, initial: Vec<u8>) -> Arc<BroadcastTopic> {
        let topic = topic.into();
        if let Some(existing) = self.topics.get(&topic) {
            return existing.clone();
        }
        let slot_id = broadcast_slot_id(&topic);
        let entry = self
            .topics
            .entry(topic.clone())
            .or_insert_with(|| {
                Arc::new(BroadcastTopic {
                    name: topic,
                    slot_id,
                    value: Mutex::new(initial),
                    subscribers: DashMap::new(),
                })
            });
        entry.clone()
    }

    /// Returns the topic if it has been registered. No side effects.
    pub fn get(&self, topic: &str) -> Option<Arc<BroadcastTopic>> {
        self.topics.get(topic).map(|entry| entry.clone())
    }

    /// Snapshot of every registered topic's current value, as
    /// `(topic, value_bytes)`. Used by the QuickJS action path to seed an
    /// updater-form `broadcast(topic, fn)` evaluator with the pre-write state so
    /// the updater's first read matches what the pure-Rust path reads via
    /// `current_value()`. A topic absent from the snapshot is treated as `null`
    /// by the caller (first-call default), matching pure-Rust semantics for an
    /// as-yet-unregistered topic.
    pub fn snapshot_values(&self) -> Vec<(String, Vec<u8>)> {
        self.topics
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().current_value()))
            .collect()
    }

    /// Subscribe `session` to `topic` with a sink (typically the
    /// session's WT patches-lane sender). Returns the topic's
    /// current value so the caller can ship an initial SlotSet to
    /// the freshly-joined client.
    ///
    /// A second subscribe for the same `(session, topic)` replaces
    /// the previous sender — useful when a session reconnects with
    /// a fresh transport.
    pub fn subscribe(
        &self,
        session: SessionId,
        topic: &str,
        sender: BroadcastSender,
    ) -> Result<Vec<u8>, BroadcastError> {
        let topic_entry = self
            .topics
            .get(topic)
            .ok_or_else(|| BroadcastError::UnknownTopic(topic.to_string()))?;
        // Register the sink and read the snapshot under the topic's
        // linearization lock, so this session lands strictly before or
        // strictly after any concurrent write: either it is in the
        // subscriber set for that write's delta and holds the pre-state, or
        // it is not and holds the post-state. Doing these two steps outside
        // the lock admits both bad interleavings — a delta already folded
        // into the snapshot (applied twice) and a delta missed entirely.
        let current = {
            let guard = topic_entry.lock_value();
            topic_entry.subscribers.insert(session, sender);
            guard.clone()
        };
        drop(topic_entry);

        self.by_session
            .entry(session)
            .or_default()
            .insert(topic.to_string());

        Ok(current)
    }

    /// Remove `(session, topic)` from the subscriber table. Safe to
    /// call when not subscribed — silent no-op.
    pub fn unsubscribe(&self, session: SessionId, topic: &str) {
        if let Some(topic_entry) = self.topics.get(topic) {
            topic_entry.subscribers.remove(&session);
        }
        if let Some(mut topics) = self.by_session.get_mut(&session) {
            topics.remove(topic);
        }
    }

    /// Remove `session` from every topic it subscribed to. Called by
    /// the WT runtime when a session disconnects.
    pub fn cleanup_session(&self, session: SessionId) {
        let removed = self.by_session.remove(&session).map(|(_, set)| set);
        let Some(topics) = removed else {
            return;
        };
        for topic in topics {
            if let Some(topic_entry) = self.topics.get(&topic) {
                topic_entry.subscribers.remove(&session);
            }
        }
    }

    /// Update `topic`'s value and fan out a `SlotSet` opcode frame
    /// to every subscriber. Returns a [`BroadcastDelivery`]
    /// describing which sessions received the patch and which were
    /// dropped (full channel / closed channel).
    ///
    /// Dropped sessions are removed from the topic immediately so a
    /// follow-up write doesn't retry them. The reverse index is
    /// also pruned so [`Self::cleanup_session`] later has nothing
    /// to do.
    pub fn write_topic(
        &self,
        topic: &str,
        value: Vec<u8>,
    ) -> Result<BroadcastDelivery, BroadcastError> {
        self.write_topic_delta(topic, |_previous| TopicTransition {
            value,
            update: ListUpdate::None,
        })
    }

    /// S4 · the delta write. `compute` is handed the topic's **previous**
    /// bytes and returns the post-write snapshot together with the z-set
    /// delta that produced it; the registry stores the snapshot and fans out
    /// one frame carrying `SlotSet` then `SlotDelta`.
    ///
    /// The closure runs inside the topic's critical section, which is what
    /// makes `previous` trustworthy: two concurrent appends cannot both diff
    /// against the same pre-state and fan out deltas that, applied in
    /// sequence, describe a state neither of them produced. That also makes
    /// the closure a bad place for I/O — it must be a pure, cheap function
    /// of the bytes it is given. FORGE, for instance, does its substrate read
    /// *before* the call and only diffs in here.
    ///
    /// Changes are [`coalesce_changes`]d before encoding, so a caller may
    /// emit the naive per-mutation delta and let the registry compact it.
    /// An empty (or fully-cancelling) delta degenerates to a lone `SlotSet`,
    /// which is exactly the pre-S4 behaviour and the reason
    /// [`Self::write_topic`] is now a thin call into this one — there is a
    /// single fan-out path to reason about, not two.
    ///
    /// # Errors
    /// [`BroadcastError::UnknownTopic`] when `topic` was never registered
    /// (the closure is not run), or [`BroadcastError::Encode`] if the frame
    /// cannot be encoded — in which case the value **has** already been
    /// stored, so a reload still shows the truth.
    pub fn write_topic_delta<F>(
        &self,
        topic: &str,
        compute: F,
    ) -> Result<BroadcastDelivery, BroadcastError>
    where
        F: FnOnce(&[u8]) -> TopicTransition,
    {
        let topic_entry = self
            .topics
            .get(topic)
            .ok_or_else(|| BroadcastError::UnknownTopic(topic.to_string()))?
            .clone();

        // ── critical section: prev → compute → store → encode → fan out ──
        let mut guard = topic_entry.lock_value();
        let transition = compute(guard.as_slice());
        *guard = transition.value.clone();

        let mut instructions = Vec::with_capacity(2);
        instructions.push(Instruction::SlotSet {
            slot_id: topic_entry.slot_id,
            value: transition.value,
        });
        // List op LAST: a list anchor that a coarse `html` binding just rebuilt
        // from the SlotSet must be reconciled in its fresh DOM.
        match transition.update {
            ListUpdate::None => {}
            ListUpdate::Delta(changes) => {
                let changes = coalesce_changes(changes);
                if !changes.is_empty() {
                    instructions.push(Instruction::SlotDelta {
                        slot_id: topic_entry.slot_id,
                        changes,
                    });
                }
            }
            ListUpdate::Reconcile(rows) => {
                // A full reconcile is already the desired set — no coalescing,
                // and an empty set legitimately means "the list is now empty",
                // which must ship so subscribers drop their last rows.
                instructions.push(Instruction::ReconcileList {
                    slot_id: topic_entry.slot_id,
                    rows,
                });
            }
        }

        let encoded = encode_frame(&OpcodeFrame {
            frame_id: self.next_frame_id.fetch_add(1, Ordering::Relaxed),
            component_id: None,
            instructions,
        })?;

        let report = self.fan_out(topic, &topic_entry, &encoded);
        drop(guard);
        Ok(report)
    }

    /// Push one encoded frame to every subscriber of `topic_entry`, dropping
    /// the sessions whose sink refused it.
    ///
    /// Called with the topic's value lock held (see the module docs on
    /// ordering). Nothing in here blocks: `try_send` is non-blocking by
    /// construction, and the pruning touches only `DashMap`s that no write
    /// path locks the value under.
    ///
    /// # A refused send drops the whole session, not one subscription
    ///
    /// A full or closed channel is a statement about the session's *sink*, not
    /// about this topic — the sink is shared by every topic that session reads.
    /// Removing it from only the topic that noticed leaves the other topics
    /// holding live clones of the same sender, which is worse than it sounds:
    /// the channel never closes, so the transport never ends, so the client is
    /// never told anything went wrong. It keeps receiving updates for its
    /// quieter topics while silently missing every one on the busy topic —
    /// stale in a way no reload-free interaction can correct, and invisible in
    /// logs because from the server's side delivery keeps succeeding.
    ///
    /// Dropping the session everywhere makes the failure loud instead: the last
    /// sender clone goes, the channel closes, the transport's stream ends, and
    /// the client reconnects and re-subscribes from current state. The cost of
    /// being wrong in this direction is one reconnect; the cost of being wrong
    /// in the other is a page that lies.
    fn fan_out(
        &self,
        topic: &str,
        topic_entry: &BroadcastTopic,
        encoded: &[u8],
    ) -> BroadcastDelivery {
        let mut report = BroadcastDelivery::default();
        let mut to_remove: Vec<SessionId> = Vec::new();

        for entry in topic_entry.subscribers.iter() {
            let session = *entry.key();
            match entry.value().try_send(encoded.to_vec()) {
                Ok(()) => report.delivered += 1,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    report.dropped_full.push(session);
                    to_remove.push(session);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    report.dropped_closed.push(session);
                    to_remove.push(session);
                }
            }
        }

        for session in to_remove {
            // Remove from this topic first: `cleanup_session` walks the reverse
            // index, and this topic's entry there is what points back here.
            topic_entry.subscribers.remove(&session);
            if let Some(mut topics) = self.by_session.get_mut(&session) {
                topics.remove(topic);
            }
            // …then everywhere else, so the session's sink is fully released
            // and its channel actually closes. Takes no value locks, so it
            // cannot deadlock against the one held across this call.
            self.cleanup_session(session);
        }

        report
    }

    /// Phase O.2 · subscribe one session to many topics in a single
    /// call and return one `Instruction::SlotSet` per topic carrying
    /// its current value. Topics that aren't yet registered are
    /// **silently created with an empty value** — this matches the
    /// render-side ergonomic where `useSharedSlot("new-topic")`
    /// implies "ensure this topic exists" rather than "fail when it
    /// hasn't been pre-registered".
    ///
    /// The returned `Vec<Instruction>` is the initial-state payload
    /// the renderer wants to ship to the freshly-subscribed client
    /// alongside the shell HTML — without it, the client would
    /// render an empty slot until the first write.
    pub fn auto_subscribe(
        &self,
        session: SessionId,
        sender: BroadcastSender,
        topics: &[String],
    ) -> Vec<Instruction> {
        let mut out = Vec::with_capacity(topics.len());
        for topic in topics {
            let topic_arc = self.topic(topic.clone(), Vec::new());
            // Same atomic register-then-snapshot as `subscribe`.
            let value = {
                let guard = topic_arc.lock_value();
                topic_arc.subscribers.insert(session, sender.clone());
                guard.clone()
            };
            self.by_session
                .entry(session)
                .or_default()
                .insert(topic.clone());
            out.push(Instruction::SlotSet {
                slot_id: topic_arc.slot_id,
                value,
            });
        }
        out
    }

    /// Diagnostic — number of registered topics.
    pub fn topic_count(&self) -> usize {
        self.topics.len()
    }

    /// Diagnostic — number of sessions tracked in the reverse index.
    /// May briefly include sessions whose only subscription was just
    /// removed; the entry is dropped on next [`Self::cleanup_session`]
    /// or once the empty set is observed.
    pub fn session_count(&self) -> usize {
        self.by_session.len()
    }
}

/// Compact a z-set delta: sum the weights of **identical records** and drop
/// the ones that cancel to zero, preserving first-appearance order.
///
/// The identity is the whole record — `(key, payload)` — never `key` alone.
/// That distinction is the single most load-bearing rule on this path, and it
/// is the S0 spike's finding: an *update* is expressed as a retraction of the
/// old row plus an insertion of the new one, both under the same `key`. Sum
/// by key and those two weights cancel to zero, the change vanishes from the
/// wire, and the client silently keeps showing the stale row — a lost edit
/// with no error anywhere. Sum by `(key, payload)` and they survive as the
/// `−`/`+` pair the client pairs back into one in-place patch.
///
/// Genuine duplicates *do* collapse: appending the same row twice in one
/// action yields one record of weight 2, which the client treats as one
/// insert (multiplicity is a set-algebra property, not a DOM one).
///
/// Ordering is stable because insert position is carried by arrival order —
/// the sink appends `+` rows in the order it receives them, so reordering
/// here would reorder the page.
#[must_use]
pub fn coalesce_changes(changes: Vec<SlotChange>) -> Vec<SlotChange> {
    if changes.len() < 2 {
        // Still drop a lone weight-0 record: it says nothing, and shipping it
        // would make an "empty delta" frame look non-empty.
        return changes
            .into_iter()
            .filter(|change| change.weight != 0)
            .collect();
    }

    let mut order: Vec<SlotChange> = Vec::with_capacity(changes.len());
    let mut seen: FxHashMap<(RowKey, Vec<u8>), usize> = FxHashMap::default();
    for change in changes {
        match seen.get(&(change.key.clone(), change.payload.clone())) {
            Some(&index) => {
                // Saturating: a weight overflow would flip a sign and turn an
                // insert into a retraction. Clamping is wrong too, but it is
                // wrong in the direction that keeps the row on the page.
                order[index].weight = order[index].weight.saturating_add(change.weight);
            }
            None => {
                seen.insert((change.key.clone(), change.payload.clone()), order.len());
                order.push(change);
            }
        }
    }

    order.retain(|change| change.weight != 0);
    order
}

/// Deterministic mapping from a topic string to its wire `SlotId`.
///
/// Two processes running the same build produce the same slot id
/// for the same topic — that's what makes the topic name a usable
/// reference across the wire. The `"broadcast::"` prefix prevents
/// collision with Phase K per-session slot ids
/// (`"{module}::{fn}#{idx}"`).
#[must_use]
pub fn broadcast_slot_id(topic: &str) -> SlotId {
    let key = format!("broadcast::{topic}");
    SlotId(fnv1a_32(key.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::wire::decode_frame;

    fn drain_into_vec(rx: &mut mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(payload) = rx.try_recv() {
            out.push(payload);
        }
        out
    }

    /// Decode the first frame in `bytes` and return the SlotSet
    /// payload, panicking if the frame isn't shaped as a single
    /// SlotSet (which is what the broadcast path emits).
    fn extract_slot_set(bytes: &[u8]) -> (SlotId, Vec<u8>) {
        let (frame, _) = decode_frame(bytes).expect("decode frame");
        assert_eq!(frame.instructions.len(), 1, "broadcast frame must carry one SlotSet");
        match frame.instructions.into_iter().next().unwrap() {
            Instruction::SlotSet { slot_id, value } => (slot_id, value),
            other => panic!("expected SlotSet, got {other:?}"),
        }
    }

    #[test]
    fn topic_returns_stable_arc_across_calls() {
        let registry = BroadcastRegistry::new();
        let first = registry.topic("chat:room-42", b"[]".to_vec());
        let second = registry.topic("chat:room-42", b"ignored".to_vec());
        assert!(Arc::ptr_eq(&first, &second));
        // The "ignored" initial must NOT overwrite the existing value.
        assert_eq!(second.current_value(), b"[]".to_vec());
    }

    #[test]
    fn broadcast_slot_id_is_deterministic_and_namespaced() {
        let a = broadcast_slot_id("topic-1");
        let b = broadcast_slot_id("topic-1");
        let c = broadcast_slot_id("topic-2");
        assert_eq!(a, b);
        assert_ne!(a, c);

        // Should not collide with Phase K's per-session id family for
        // the same trailing token (paranoid sanity check).
        let collision_candidate = SlotId(fnv1a_32(b"topic-1"));
        assert_ne!(a, collision_candidate);
    }

    /// Cross-language lock: the client derives the same slot id from a topic
    /// (`assets/albedo-runtime.js::topicSlotId` / `list-anchor-scan.test.mjs`).
    /// If this literal changes, update the JS mirror in lockstep or B3/S4 will
    /// register the guestbook anchor under a slot the broadcast never targets.
    #[test]
    fn broadcast_slot_id_matches_the_js_mirror() {
        assert_eq!(broadcast_slot_id("guestbook"), SlotId(3_800_127_029));
        assert_eq!(broadcast_slot_id("chat"), SlotId(2_183_019_110));
    }

    #[test]
    fn subscribe_to_unknown_topic_surfaces_typed_error() {
        let registry = BroadcastRegistry::new();
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let err = registry
            .subscribe(SessionId::random(), "missing", tx)
            .unwrap_err();
        assert!(matches!(err, BroadcastError::UnknownTopic(_)));
    }

    #[test]
    fn subscribe_returns_current_value_for_late_joiners() {
        let registry = BroadcastRegistry::new();
        registry.topic("seed", b"initial".to_vec());
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let value = registry
            .subscribe(SessionId::random(), "seed", tx)
            .unwrap();
        assert_eq!(value, b"initial".to_vec());
    }

    #[tokio::test]
    async fn write_topic_fans_out_slot_set_to_every_subscriber() {
        let registry = BroadcastRegistry::new();
        let topic = registry.topic("counter", b"0".to_vec());

        let (tx_a, mut rx_a) = mpsc::channel::<Vec<u8>>(8);
        let (tx_b, mut rx_b) = mpsc::channel::<Vec<u8>>(8);
        let session_a = SessionId::random();
        let session_b = SessionId::random();
        registry.subscribe(session_a, "counter", tx_a).unwrap();
        registry.subscribe(session_b, "counter", tx_b).unwrap();

        let report = registry.write_topic("counter", b"42".to_vec()).unwrap();
        assert_eq!(report.delivered, 2);
        assert_eq!(report.drop_count(), 0);

        let a_payloads = drain_into_vec(&mut rx_a);
        let b_payloads = drain_into_vec(&mut rx_b);
        assert_eq!(a_payloads.len(), 1);
        assert_eq!(b_payloads.len(), 1);

        let (slot_a, value_a) = extract_slot_set(&a_payloads[0]);
        let (slot_b, value_b) = extract_slot_set(&b_payloads[0]);
        assert_eq!(slot_a, topic.slot_id());
        assert_eq!(slot_b, topic.slot_id());
        assert_eq!(value_a, b"42".to_vec());
        assert_eq!(value_b, b"42".to_vec());

        // The topic's stored value must reflect the write so a
        // future subscriber gets the latest state, not the seed.
        assert_eq!(topic.current_value(), b"42".to_vec());
    }

    #[tokio::test]
    async fn write_to_unknown_topic_yields_typed_error() {
        let registry = BroadcastRegistry::new();
        let err = registry
            .write_topic("nope", b"x".to_vec())
            .unwrap_err();
        assert!(matches!(err, BroadcastError::UnknownTopic(_)));
    }

    #[tokio::test]
    async fn closed_subscriber_channel_is_dropped_on_write() {
        let registry = BroadcastRegistry::new();
        registry.topic("alerts", b"[]".to_vec());

        let (tx, rx) = mpsc::channel::<Vec<u8>>(4);
        let session = SessionId::random();
        registry.subscribe(session, "alerts", tx).unwrap();
        drop(rx); // Receiver gone → sender closed.

        let report = registry.write_topic("alerts", b"pong".to_vec()).unwrap();
        assert_eq!(report.delivered, 0);
        assert_eq!(report.dropped_closed, vec![session]);
        assert!(report.dropped_full.is_empty());

        // Topic is now empty of subscribers; reverse index pruned.
        let topic = registry.get("alerts").unwrap();
        assert_eq!(topic.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn full_subscriber_channel_is_dropped_and_does_not_block_others() {
        let registry = BroadcastRegistry::new();
        registry.topic("hot", b"0".to_vec());

        // Tiny channel for the slow consumer; large for the fast one.
        let (tx_slow, _rx_slow) = mpsc::channel::<Vec<u8>>(1);
        let (tx_fast, mut rx_fast) = mpsc::channel::<Vec<u8>>(64);
        let slow = SessionId::random();
        let fast = SessionId::random();
        registry.subscribe(slow, "hot", tx_slow.clone()).unwrap();
        registry.subscribe(fast, "hot", tx_fast).unwrap();

        // Fill the slow channel so the next try_send returns Full.
        tx_slow.try_send(b"prefill".to_vec()).unwrap();

        let report = registry.write_topic("hot", b"new".to_vec()).unwrap();
        assert_eq!(report.delivered, 1, "fast consumer must receive");
        assert_eq!(report.dropped_full, vec![slow]);

        let fast_payloads = drain_into_vec(&mut rx_fast);
        assert_eq!(fast_payloads.len(), 1);
        let (_, value) = extract_slot_set(&fast_payloads[0]);
        assert_eq!(value, b"new".to_vec());

        // Slow session is no longer in the subscriber table.
        let topic = registry.get("hot").unwrap();
        assert_eq!(topic.subscriber_count(), 1);
    }

    /// The backpressure-recovery invariant. A session subscribed to two topics
    /// whose sink fills up must lose BOTH subscriptions, so the last sender
    /// clone drops and its channel closes — that closure is the only signal
    /// the transport has that this client needs to reconnect. Leave the quiet
    /// topic's clone alive and the channel stays open forever: the client goes
    /// on receiving that topic's updates while silently missing every one on
    /// the busy topic.
    #[tokio::test]
    async fn a_refused_send_drops_the_sessions_whole_subscription_so_its_channel_closes() {
        let registry = BroadcastRegistry::new();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        let session = SessionId::random();
        let _ = registry.auto_subscribe(session, tx, &["busy".to_string(), "quiet".to_string()]);
        assert_eq!(registry.get("quiet").unwrap().subscriber_count(), 1);

        // Fill the sink, then write to just ONE of the two topics.
        registry.write_topic("busy", b"fills-the-channel".to_vec()).unwrap();
        let report = registry.write_topic("busy", b"refused".to_vec()).unwrap();
        assert_eq!(report.dropped_full, vec![session]);

        assert_eq!(
            registry.get("quiet").unwrap().subscriber_count(),
            0,
            "the untouched topic must release the same broken sink"
        );
        assert!(!registry.by_session.contains_key(&session), "reverse index pruned");

        // Draining what was buffered must then see the channel CLOSED, which is
        // what ends the SSE stream and triggers the client's reconnect.
        while rx.try_recv().is_ok() {}
        assert!(
            matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected)),
            "every sender clone must be gone, or the client is never told to reconnect"
        );
    }

    #[tokio::test]
    async fn cleanup_session_removes_subscriptions_for_that_session_only() {
        let registry = BroadcastRegistry::new();
        registry.topic("a", b"".to_vec());
        registry.topic("b", b"".to_vec());

        let (tx_a1, _rx) = mpsc::channel::<Vec<u8>>(4);
        let (tx_a2, _rx) = mpsc::channel::<Vec<u8>>(4);
        let (tx_b1, _rx) = mpsc::channel::<Vec<u8>>(4);
        let s1 = SessionId::random();
        let s2 = SessionId::random();
        registry.subscribe(s1, "a", tx_a1).unwrap();
        registry.subscribe(s1, "b", tx_b1).unwrap();
        registry.subscribe(s2, "a", tx_a2).unwrap();

        assert_eq!(registry.get("a").unwrap().subscriber_count(), 2);
        assert_eq!(registry.get("b").unwrap().subscriber_count(), 1);

        registry.cleanup_session(s1);

        assert_eq!(registry.get("a").unwrap().subscriber_count(), 1);
        assert_eq!(registry.get("b").unwrap().subscriber_count(), 0);
        // Reverse index entry for s1 is gone; s2's stays.
        assert!(!registry.by_session.contains_key(&s1));
        assert!(registry.by_session.contains_key(&s2));
    }

    #[tokio::test]
    async fn double_subscribe_for_same_session_replaces_the_sender() {
        let registry = BroadcastRegistry::new();
        registry.topic("t", b"".to_vec());

        let (tx_first, mut rx_first) = mpsc::channel::<Vec<u8>>(4);
        let (tx_second, mut rx_second) = mpsc::channel::<Vec<u8>>(4);
        let session = SessionId::random();
        registry.subscribe(session, "t", tx_first).unwrap();
        registry.subscribe(session, "t", tx_second).unwrap();

        let report = registry.write_topic("t", b"hi".to_vec()).unwrap();
        assert_eq!(report.delivered, 1);
        assert!(drain_into_vec(&mut rx_first).is_empty());
        assert_eq!(drain_into_vec(&mut rx_second).len(), 1);
    }

    #[tokio::test]
    async fn frame_id_advances_per_write_for_reassembly_correctness() {
        let registry = BroadcastRegistry::new();
        registry.topic("seq", b"".to_vec());
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        registry.subscribe(SessionId::random(), "seq", tx).unwrap();

        registry.write_topic("seq", b"1".to_vec()).unwrap();
        registry.write_topic("seq", b"2".to_vec()).unwrap();
        let payloads = drain_into_vec(&mut rx);
        assert_eq!(payloads.len(), 2);
        let (first_frame, _) = decode_frame(&payloads[0]).unwrap();
        let (second_frame, _) = decode_frame(&payloads[1]).unwrap();
        assert!(
            second_frame.frame_id > first_frame.frame_id,
            "frame ids must be strictly monotone so the reassembler can sequence them"
        );
    }

    #[tokio::test]
    async fn unsubscribe_silently_no_ops_when_not_subscribed() {
        let registry = BroadcastRegistry::new();
        registry.topic("t", b"".to_vec());
        // No panic, no error.
        registry.unsubscribe(SessionId::random(), "t");
        registry.unsubscribe(SessionId::random(), "missing-topic");
    }

    // ── S4 · delta lane ────────────────────────────────────────────────

    fn change(weight: i32, key: &str, payload: &str) -> SlotChange {
        SlotChange {
            weight,
            key: RowKey(key.to_string()),
            payload: payload.as_bytes().to_vec(),
        }
    }

    /// Decode a broadcast frame into its instruction list.
    fn instructions_of(bytes: &[u8]) -> Vec<Instruction> {
        let (frame, _) = decode_frame(bytes).expect("decode frame");
        frame.instructions
    }

    /// The client's `reconcileSlotDelta` + list sink, in miniature: an ordered
    /// `(key, payload)` list standing in for the anchor's rows. Kept
    /// deliberately dumb and mirrored from `assets/albedo-runtime.js` so the
    /// oracle below tests the *wire contract*, not a Rust re-derivation of it.
    fn apply_delta(rows: &mut Vec<(String, String)>, changes: &[SlotChange]) {
        let mut order: Vec<String> = Vec::new();
        let mut plan: std::collections::HashMap<String, (Option<String>, bool)> =
            std::collections::HashMap::new();
        for change in changes {
            let entry = plan.entry(change.key.0.clone()).or_insert_with(|| {
                order.push(change.key.0.clone());
                (None, false)
            });
            let payload = String::from_utf8(change.payload.clone()).expect("utf8 payload");
            if change.weight > 0 {
                entry.0 = Some(payload);
            } else if change.weight < 0 {
                entry.1 = true;
            }
        }
        for key in order {
            let (insert, retract) = plan.remove(&key).expect("planned key");
            let position = rows.iter().position(|(existing, _)| *existing == key);
            match (insert, position) {
                (Some(payload), Some(index)) => rows[index].1 = payload, // patch
                (Some(payload), None) => rows.push((key, payload)),      // insert
                (None, Some(index)) if retract => {
                    rows.remove(index);
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn a_delta_write_ships_the_snapshot_first_and_the_delta_last() {
        let registry = BroadcastRegistry::new();
        let topic = registry.topic("guestbook", b"[]".to_vec());
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        registry
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        let report = registry
            .write_topic_delta("guestbook", |previous| {
                assert_eq!(previous, b"[]", "the closure sees the pre-write bytes");
                TopicTransition {
                    value: br#"[{"id":1}]"#.to_vec(),
                    update: ListUpdate::Delta(vec![change(
                        1,
                        "1",
                        "<li data-albedo-key=\"1\">ada</li>",
                    )]),
                }
            })
            .unwrap();
        assert_eq!(report.delivered, 1);

        let payloads = drain_into_vec(&mut rx);
        assert_eq!(payloads.len(), 1, "one frame carries both instructions");
        match instructions_of(&payloads[0]).as_slice() {
            [Instruction::SlotSet {
                slot_id: set_slot,
                value,
            }, Instruction::SlotDelta {
                slot_id: delta_slot,
                changes,
            }] => {
                assert_eq!(*set_slot, topic.slot_id());
                assert_eq!(*delta_slot, topic.slot_id());
                assert_eq!(value, br#"[{"id":1}]"#);
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].key, RowKey("1".to_string()));
            }
            other => panic!("expected [SlotSet, SlotDelta], got {other:?}"),
        }
        assert_eq!(topic.current_value(), br#"[{"id":1}]"#.to_vec());
    }

    /// A reorder / mid-insert ships the full ordered set as a `ReconcileList`,
    /// after the `SlotSet`, so a subscriber whose anchor a coarse binding just
    /// rebuilt gets its rows placed in the right order.
    #[tokio::test]
    async fn a_reconcile_update_ships_a_reconcile_list_after_the_snapshot() {
        let registry = BroadcastRegistry::new();
        let topic = registry.topic("guestbook", b"[]".to_vec());
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        registry
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        registry
            .write_topic_delta("guestbook", |_| TopicTransition {
                value: br#"[{"id":2},{"id":1}]"#.to_vec(),
                update: ListUpdate::Reconcile(vec![
                    ReconcileRow {
                        key: RowKey("2".to_string()),
                        payload: b"<li data-albedo-key=\"2\">alan</li>".to_vec(),
                    },
                    ReconcileRow {
                        key: RowKey("1".to_string()),
                        payload: b"<li data-albedo-key=\"1\">ada</li>".to_vec(),
                    },
                ]),
            })
            .unwrap();

        let payloads = drain_into_vec(&mut rx);
        match instructions_of(&payloads[0]).as_slice() {
            [Instruction::SlotSet { .. }, Instruction::ReconcileList { slot_id, rows }] => {
                assert_eq!(*slot_id, topic.slot_id());
                let keys: Vec<_> = rows.iter().map(|r| r.key.0.as_str()).collect();
                assert_eq!(keys, vec!["2", "1"], "rows ship in desired order");
            }
            other => panic!("expected [SlotSet, ReconcileList], got {other:?}"),
        }
    }

    /// A value-only transition must stay byte-identical to the pre-S4 frame:
    /// every existing client and test that expects a lone `SlotSet` keeps
    /// working, and `write_topic` keeps being a special case of one path.
    #[tokio::test]
    async fn an_empty_delta_degenerates_to_a_plain_slot_set() {
        let registry = BroadcastRegistry::new();
        registry.topic("t", b"".to_vec());
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        registry.subscribe(SessionId::random(), "t", tx).unwrap();

        registry
            .write_topic_delta("t", |_| TopicTransition {
                value: b"v".to_vec(),
                // Cancels to nothing: identical record, opposite weights.
                update: ListUpdate::Delta(vec![
                    change(1, "a", "<li>a</li>"),
                    change(-1, "a", "<li>a</li>"),
                ]),
            })
            .unwrap();

        let payloads = drain_into_vec(&mut rx);
        assert_eq!(
            instructions_of(&payloads[0]).len(),
            1,
            "no empty SlotDelta on the wire"
        );
    }

    /// THE standing correctness property: applying the delta to the previous
    /// rows must land exactly where a full re-render of the new value lands.
    /// If this ever fails, the delta lane is lying and the page will drift.
    #[test]
    fn apply_delta_equals_full_render_for_insert_update_and_remove() {
        let render = |rows: &[(&str, &str)]| -> Vec<(String, String)> {
            rows.iter()
                .map(|(key, text)| {
                    (
                        key.to_string(),
                        format!("<li data-albedo-key=\"{key}\">{text}</li>"),
                    )
                })
                .collect()
        };

        let cases: Vec<(Vec<(&str, &str)>, Vec<(&str, &str)>, Vec<SlotChange>)> = vec![
            // Append — the guestbook case.
            (
                vec![("1", "ada"), ("2", "alan")],
                vec![("1", "ada"), ("2", "alan"), ("3", "grace")],
                vec![change(1, "3", "<li data-albedo-key=\"3\">grace</li>")],
            ),
            // Update — retract + insert under ONE key. The case that a
            // sum-by-key coalescer silently deletes.
            (
                vec![("1", "ada"), ("2", "alan")],
                vec![("1", "ada"), ("2", "turing")],
                vec![
                    change(-1, "2", "<li data-albedo-key=\"2\">alan</li>"),
                    change(1, "2", "<li data-albedo-key=\"2\">turing</li>"),
                ],
            ),
            // Retraction.
            (
                vec![("1", "ada"), ("2", "alan")],
                vec![("1", "ada")],
                vec![change(-1, "2", "<li data-albedo-key=\"2\">alan</li>")],
            ),
            // Mixed batch in one action.
            (
                vec![("1", "ada")],
                vec![("2", "grace"), ("3", "hopper")],
                vec![
                    change(-1, "1", "<li data-albedo-key=\"1\">ada</li>"),
                    change(1, "2", "<li data-albedo-key=\"2\">grace</li>"),
                    change(1, "3", "<li data-albedo-key=\"3\">hopper</li>"),
                ],
            ),
        ];

        for (previous, next, changes) in cases {
            let mut incremental = render(&previous);
            apply_delta(&mut incremental, &coalesce_changes(changes.clone()));
            let full = render(&next);
            assert_eq!(
                incremental, full,
                "apply(Δ) != full_render for {previous:?} -> {next:?} via {changes:?}"
            );
        }
    }

    /// The S0 finding, pinned as its own test: an update must survive
    /// coalescing. Sum by `key` and this returns empty; sum by `(key, payload)`
    /// and both halves come through.
    #[test]
    fn coalescing_never_cancels_an_update_to_nothing() {
        let compacted = coalesce_changes(vec![
            change(-1, "2", "<li>alan</li>"),
            change(1, "2", "<li>turing</li>"),
        ]);
        assert_eq!(
            compacted.len(),
            2,
            "the -/+ pair IS the update; it must not cancel"
        );
        assert_eq!(compacted[0].weight, -1);
        assert_eq!(compacted[1].weight, 1);
    }

    #[test]
    fn coalescing_folds_identical_records_and_drops_true_cancellations() {
        let compacted = coalesce_changes(vec![
            change(1, "a", "<li>a</li>"),
            change(1, "a", "<li>a</li>"),
            change(1, "b", "<li>b</li>"),
            change(-1, "b", "<li>b</li>"),
            change(0, "c", "<li>c</li>"),
        ]);
        assert_eq!(compacted.len(), 1, "only the folded 'a' record survives");
        assert_eq!(compacted[0].key, RowKey("a".to_string()));
        assert_eq!(
            compacted[0].weight, 2,
            "multiplicity folds; the row stays one row"
        );
    }

    #[test]
    fn coalescing_preserves_arrival_order_because_it_is_insert_order() {
        let compacted = coalesce_changes(vec![
            change(1, "c", "<li>c</li>"),
            change(1, "a", "<li>a</li>"),
            change(1, "b", "<li>b</li>"),
            change(1, "a", "<li>a</li>"),
        ]);
        let keys: Vec<_> = compacted.iter().map(|c| c.key.0.as_str()).collect();
        assert_eq!(
            keys,
            vec!["c", "a", "b"],
            "a folded record keeps its FIRST position"
        );
    }

    /// Concurrent writers must serialize per topic: each delta is computed
    /// against the state the previous one produced, so replaying the deltas in
    /// delivery order reproduces the stored value. Without the linearization
    /// lock both writers would diff the same pre-state and one row would be
    /// lost on every client (while the database kept both).
    #[tokio::test]
    async fn concurrent_writers_produce_a_replayable_delta_sequence() {
        let registry = Arc::new(BroadcastRegistry::new());
        registry.topic("race", b"".to_vec());
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        registry.subscribe(SessionId::random(), "race", tx).unwrap();

        // Each writer appends "wN" to a comma-joined value, deriving BOTH the
        // snapshot and the delta from whatever it observes — i.e. the FORGE
        // shape, minus the substrate.
        let mut handles = Vec::new();
        for index in 0..8 {
            let registry = Arc::clone(&registry);
            handles.push(tokio::task::spawn_blocking(move || {
                registry
                    .write_topic_delta("race", |previous| {
                        let previous = String::from_utf8(previous.to_vec()).unwrap();
                        let key = format!("w{index}");
                        let value = if previous.is_empty() {
                            key.clone()
                        } else {
                            format!("{previous},{key}")
                        };
                        TopicTransition {
                            value: value.into_bytes(),
                            update: ListUpdate::Delta(vec![change(
                                1,
                                &key,
                                &format!("<li>{key}</li>"),
                            )]),
                        }
                    })
                    .unwrap()
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        // Replay every delivered delta in arrival order.
        let mut rows: Vec<(String, String)> = Vec::new();
        for payload in drain_into_vec(&mut rx) {
            for instruction in instructions_of(&payload) {
                if let Instruction::SlotDelta { changes, .. } = instruction {
                    apply_delta(&mut rows, &changes);
                }
            }
        }

        let stored = String::from_utf8(registry.get("race").unwrap().current_value()).unwrap();
        let replayed: Vec<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            replayed,
            stored.split(',').collect::<Vec<_>>(),
            "replaying the deltas in delivery order must reproduce the stored value"
        );
        assert_eq!(
            rows.len(),
            8,
            "no writer's row was lost to a stale pre-state"
        );
    }

    #[tokio::test]
    async fn auto_subscribe_creates_unknown_topics_and_returns_initial_slot_sets() {
        let registry = BroadcastRegistry::new();
        // Seed one topic with a value; leave the other for auto-create.
        registry.topic("known", b"seed".to_vec());
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let session = SessionId::random();

        let opcodes =
            registry.auto_subscribe(session, tx, &["known".to_string(), "fresh".to_string()]);

        assert_eq!(opcodes.len(), 2);
        // First topic carries the seeded value; second carries empty.
        match (&opcodes[0], &opcodes[1]) {
            (
                Instruction::SlotSet {
                    slot_id: s1,
                    value: v1,
                },
                Instruction::SlotSet {
                    slot_id: s2,
                    value: v2,
                },
            ) => {
                assert_eq!(*s1, broadcast_slot_id("known"));
                assert_eq!(v1, b"seed");
                assert_eq!(*s2, broadcast_slot_id("fresh"));
                assert!(v2.is_empty());
            }
            other => panic!("expected two SlotSet opcodes, got {other:?}"),
        }

        // Both topics now hold the session as a subscriber.
        assert_eq!(registry.get("known").unwrap().subscriber_count(), 1);
        assert_eq!(registry.get("fresh").unwrap().subscriber_count(), 1);
    }

    #[tokio::test]
    async fn auto_subscribe_followup_write_reaches_the_subscriber() {
        let registry = BroadcastRegistry::new();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(8);
        let session = SessionId::random();
        let _ = registry.auto_subscribe(session, tx, &["live".to_string()]);

        let report = registry.write_topic("live", b"hello".to_vec()).unwrap();
        assert_eq!(report.delivered, 1);

        let payload = rx.recv().await.expect("write reaches subscriber");
        let (_, value) = extract_slot_set(&payload);
        assert_eq!(value, b"hello");
    }
}
