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
//! - **Wire reuse, no version bump.** Fan-out emits the existing
//!   `Instruction::SlotSet` opcode wrapped in an `OpcodeFrame`. The
//!   client can't tell whether a SlotSet came from a per-session
//!   write or a broadcast — it just applies the value to the slot.
//!   `LOCKED_WIRE_VERSION` stays at 2.
//! - **Deterministic slot IDs.** A topic string hashes (FNV-1a-32 of
//!   `"broadcast::{topic}"`) to its `SlotId`. The `"broadcast::"`
//!   prefix avoids collision with Phase K's per-session slot IDs
//!   (`"{module}::{fn}#{idx}"`).
//! - **Server-side state, dumb client.** Subscriptions live on the
//!   server. The bakabox runtime just receives opcodes on the
//!   patches lane — same dispatch as any other patch.
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

use crate::ir::opcode::{Instruction, OpcodeFrame, SlotId};
use crate::ir::wire::{encode_frame, WireError};
use crate::runtime::eval::component::fnv1a_32;
use crate::runtime::session::SessionId;
use dashmap::DashMap;
use rustc_hash::FxHashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Current value. Late subscribers read this so they don't
    /// render an empty topic on first paint. `Mutex<Vec<u8>>` rather
    /// than `RwLock` because writes are common and the read path
    /// already clones the bytes out before doing anything else.
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
        self.value
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
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
        topic_entry.subscribers.insert(session, sender);
        let current = topic_entry.current_value();
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
        let topic_entry = self
            .topics
            .get(topic)
            .ok_or_else(|| BroadcastError::UnknownTopic(topic.to_string()))?
            .clone();

        if let Ok(mut guard) = topic_entry.value.lock() {
            *guard = value.clone();
        }

        let frame = OpcodeFrame {
            frame_id: self.next_frame_id.fetch_add(1, Ordering::Relaxed),
            component_id: None,
            instructions: vec![Instruction::SlotSet {
                slot_id: topic_entry.slot_id,
                value,
            }],
        };
        let encoded = encode_frame(&frame)?;

        let mut report = BroadcastDelivery::default();
        let mut to_remove: Vec<SessionId> = Vec::new();

        for entry in topic_entry.subscribers.iter() {
            let session = *entry.key();
            match entry.value().try_send(encoded.clone()) {
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
            topic_entry.subscribers.remove(&session);
            if let Some(mut topics) = self.by_session.get_mut(&session) {
                topics.remove(topic);
            }
        }

        Ok(report)
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
            topic_arc.subscribers.insert(session, sender.clone());
            self.by_session
                .entry(session)
                .or_default()
                .insert(topic.clone());
            out.push(Instruction::SlotSet {
                slot_id: topic_arc.slot_id,
                value: topic_arc.current_value(),
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

    #[tokio::test]
    async fn auto_subscribe_creates_unknown_topics_and_returns_initial_slot_sets() {
        let registry = BroadcastRegistry::new();
        // Seed one topic with a value; leave the other for auto-create.
        registry.topic("known", b"seed".to_vec());
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
        let session = SessionId::random();

        let opcodes = registry.auto_subscribe(
            session,
            tx,
            &["known".to_string(), "fresh".to_string()],
        );

        assert_eq!(opcodes.len(), 2);
        // First topic carries the seeded value; second carries empty.
        match (&opcodes[0], &opcodes[1]) {
            (
                Instruction::SlotSet { slot_id: s1, value: v1 },
                Instruction::SlotSet { slot_id: s2, value: v2 },
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
