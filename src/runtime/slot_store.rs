//! Phase-H — per-session reactive slot store.
//!
//! Holds the server-side state that Phase G's `ActionHandler`s mutate
//! and bakabox's `SetTextRef` / `SetAttrRef` / `SlotSet` opcodes
//! reference. One [`SlotStore`] instance is shared (via `Arc`) between
//! the runtime pipeline and the server's action dispatcher so writes
//! the handler performs during its run are visible to subsequent drains
//! without an intermediate copy.
//!
//! Concurrency: the value table is a [`DashMap`] keyed by
//! `(SessionId, SlotId)`, so independent slot operations don't lock
//! each other. The dirty-set is wrapped in a `Mutex` since drains are
//! always single-consumer (the action dispatcher after a handler runs,
//! or the tick loop on the runtime side) and the lock is held only
//! while building the `SlotSet` vector.
//!
//! ## Lifetime
//!
//! A session is a browser tab's cookie, and the server is never told when
//! one ends: the cookie has no expiry and a closed tab sends nothing. So
//! nothing here waits for an end. Every read and write stamps the session,
//! and [`SlotStore::evict_idle`] drops sessions that have gone quiet — the
//! server sweeps with [`SESSION_IDLE_TTL`].

use crate::ir::opcode::{Instruction, SlotId};
use crate::runtime::session::SessionId;
use dashmap::DashMap;
use rustc_hash::FxHashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a session's slots outlive its last read or write.
///
/// A tab idle for longer than this that acts again starts from its hooks'
/// initial values — what a reload would give it anyway.
pub const SESSION_IDLE_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Concurrent server-side slot store. Constructed once per server and
/// shared via `Arc<SlotStore>`.
#[derive(Debug)]
pub struct SlotStore {
    /// `(session, slot) -> value`. The value is owned `Vec<u8>` so
    /// readers can return a clone without holding a `DashMap` ref
    /// across an `.await`.
    values: DashMap<(SessionId, SlotId), Vec<u8>>,
    /// Pending dirty keys. Writes push here; drains move every entry
    /// into a SlotSet vector and clear the set. `FxHashSet` because
    /// hashing here is on the hot path.
    dirty: Mutex<FxHashSet<(SessionId, SlotId)>>,
    /// Each session's last read or write, in milliseconds since `born`.
    last_seen: DashMap<SessionId, u64>,
    born: Instant,
}

impl Default for SlotStore {
    fn default() -> Self {
        Self {
            values: DashMap::new(),
            dirty: Mutex::default(),
            last_seen: DashMap::new(),
            born: Instant::now(),
        }
    }
}

impl SlotStore {
    /// Returns a fresh, empty slot store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn touch(&self, session: SessionId) {
        self.last_seen.insert(session, self.now_ms());
    }

    /// Reads the current value of a slot. Returns `None` when the slot
    /// has not been written for this session yet. Allocates — for hot
    /// paths that compare against the previous value, consider
    /// computing the hash before the read.
    #[must_use]
    pub fn read(&self, session: SessionId, slot_id: SlotId) -> Option<Vec<u8>> {
        let value = self.values.get(&(session, slot_id)).map(|entry| entry.clone());
        if value.is_some() {
            self.touch(session);
        }
        value
    }

    /// Writes a new value into the slot and records it as dirty so the
    /// next [`Self::drain_set_instructions`] call ships a `SlotSet`
    /// opcode. Idempotent: writing the same key twice between drains
    /// produces one `SlotSet` carrying the final value (HashSet dedup).
    pub fn write(&self, session: SessionId, slot_id: SlotId, value: Vec<u8>) {
        self.values.insert((session, slot_id), value);
        self.touch(session);
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.insert((session, slot_id));
        }
    }

    /// Drop every session that has neither read nor written a slot for
    /// `idle`. Returns how many sessions went.
    ///
    /// One pass over the value table however many sessions leave. A session
    /// that wakes between being selected and being dropped loses the value it
    /// just wrote; for a session idle this long that is the same outcome as
    /// arriving a moment later.
    pub fn evict_idle(&self, idle: Duration) -> usize {
        let idle_ms = u64::try_from(idle.as_millis()).unwrap_or(u64::MAX);
        self.evict_seen_before(self.now_ms().saturating_sub(idle_ms))
    }

    fn evict_seen_before(&self, cutoff_ms: u64) -> usize {
        let mut gone = FxHashSet::default();
        self.last_seen.retain(|session, seen| {
            let keep = *seen >= cutoff_ms;
            if !keep {
                gone.insert(*session);
            }
            keep
        });
        if gone.is_empty() {
            return 0;
        }
        self.values.retain(|(session, _), _| !gone.contains(session));
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.retain(|(session, _)| !gone.contains(session));
        }
        gone.len()
    }

    /// Drains every dirty slot for the supplied session into a vector
    /// of [`Instruction::SlotSet`] opcodes ready to ship. Dirty entries
    /// for other sessions are left untouched.
    ///
    /// Returns an empty vec when no slot changed for this session
    /// since the last drain.
    pub fn drain_set_instructions(&self, session: SessionId) -> Vec<Instruction> {
        let keys: Vec<(SessionId, SlotId)> = match self.dirty.lock() {
            Ok(mut dirty) => {
                let session_keys: Vec<(SessionId, SlotId)> =
                    dirty.iter().filter(|(s, _)| *s == session).copied().collect();
                for key in &session_keys {
                    dirty.remove(key);
                }
                session_keys
            }
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::with_capacity(keys.len());
        for (sess, slot_id) in keys {
            if let Some(value) = self.read(sess, slot_id) {
                out.push(Instruction::SlotSet { slot_id, value });
            }
        }
        out
    }

    /// Removes every value belonging to `session`.
    pub fn clear_session(&self, session: SessionId) {
        self.last_seen.remove(&session);
        self.values.retain(|(s, _), _| *s != session);
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.retain(|(s, _)| *s != session);
        }
    }

    /// Total number of slot entries currently held. Exposed for tests
    /// and metrics; not on the hot path.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// `true` when no slots have been written. Convenience for tests.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// Session-scoped slot view used by compile-time bindings (Phase K) and
/// mirrored by `albedo_server::actions::SessionSlots` for the wire-side
/// handler dispatcher. Identical structure on both sides so a server
/// dispatcher and an in-process compile test share one substrate.
///
/// `Clone` is cheap — two `Arc` bumps — so threading the view through
/// closures or spawned tasks is free.
#[derive(Clone, Debug)]
pub struct SessionSlotView {
    session_id: SessionId,
    store: Arc<SlotStore>,
}

impl SessionSlotView {
    #[must_use]
    pub fn new(session_id: SessionId, store: Arc<SlotStore>) -> Self {
        Self { session_id, store }
    }

    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[must_use]
    pub fn read(&self, slot_id: SlotId) -> Option<Vec<u8>> {
        self.store.read(self.session_id, slot_id)
    }

    pub fn write(&self, slot_id: SlotId, value: Vec<u8>) {
        self.store.write(self.session_id, slot_id, value);
    }

    pub fn drain_pending(&self) -> Vec<Instruction> {
        self.store.drain_set_instructions(self.session_id)
    }

    #[must_use]
    pub fn store(&self) -> &Arc<SlotStore> {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(id: u32) -> SlotId {
        SlotId(id)
    }

    #[test]
    fn read_returns_none_for_unwritten_slot() {
        let store = SlotStore::new();
        assert!(store.read(SessionId::random(), slot(1)).is_none());
    }

    #[test]
    fn write_then_read_returns_the_value() {
        let store = SlotStore::new();
        let session = SessionId::random();
        store.write(session, slot(1), b"value".to_vec());
        assert_eq!(store.read(session, slot(1)), Some(b"value".to_vec()));
    }

    #[test]
    fn drain_produces_slot_set_for_dirty_slots() {
        let store = SlotStore::new();
        let session = SessionId::random();
        store.write(session, slot(1), b"one".to_vec());
        store.write(session, slot(2), b"two".to_vec());

        let mut drained = store.drain_set_instructions(session);
        drained.sort_by_key(|instr| match instr {
            Instruction::SlotSet { slot_id, .. } => slot_id.0,
            _ => u32::MAX,
        });
        assert_eq!(drained.len(), 2);
        assert!(matches!(
            &drained[0],
            Instruction::SlotSet { slot_id: SlotId(1), value } if value == b"one"
        ));
        assert!(matches!(
            &drained[1],
            Instruction::SlotSet { slot_id: SlotId(2), value } if value == b"two"
        ));
    }

    #[test]
    fn drain_is_idempotent_after_consumption() {
        let store = SlotStore::new();
        let session = SessionId::random();
        store.write(session, slot(1), b"one".to_vec());
        assert_eq!(store.drain_set_instructions(session).len(), 1);
        assert_eq!(
            store.drain_set_instructions(session).len(),
            0,
            "second drain must produce no further SlotSet — first cleared the dirty set",
        );
    }

    #[test]
    fn drain_only_emits_for_the_requested_session() {
        let store = SlotStore::new();
        let session_a = SessionId::random();
        let session_b = SessionId::random();
        store.write(session_a, slot(1), b"a".to_vec());
        store.write(session_b, slot(1), b"b".to_vec());

        let drained_a = store.drain_set_instructions(session_a);
        assert_eq!(drained_a.len(), 1);
        // session_b's dirty entry must still be pending.
        let drained_b = store.drain_set_instructions(session_b);
        assert_eq!(drained_b.len(), 1);
    }

    #[test]
    fn double_write_to_same_slot_emits_one_slot_set_with_latest_value() {
        let store = SlotStore::new();
        let session = SessionId::random();
        store.write(session, slot(1), b"old".to_vec());
        store.write(session, slot(1), b"new".to_vec());

        let drained = store.drain_set_instructions(session);
        assert_eq!(drained.len(), 1, "HashSet dedup must coalesce repeated writes");
        match &drained[0] {
            Instruction::SlotSet { value, .. } => assert_eq!(value, b"new"),
            other => panic!("expected SlotSet, got {other:?}"),
        }
    }

    #[test]
    fn clear_session_removes_values_and_dirty_entries() {
        let store = SlotStore::new();
        let keep = SessionId::random();
        let drop = SessionId::random();
        store.write(keep, slot(1), b"keep".to_vec());
        store.write(drop, slot(1), b"drop".to_vec());
        store.write(drop, slot(2), b"drop2".to_vec());

        store.clear_session(drop);

        assert_eq!(store.len(), 1);
        assert!(store.read(drop, slot(1)).is_none());
        assert_eq!(store.read(keep, slot(1)), Some(b"keep".to_vec()));
        // Dirty entries for the cleared session must also be gone.
        assert!(store.drain_set_instructions(drop).is_empty());
    }

    /// Sessions nobody has touched since the cutoff go — values, pending
    /// dirty entries and all — and the ones touched after it stay.
    #[test]
    fn idle_sessions_are_evicted_and_active_ones_kept() {
        let store = SlotStore::new();
        let idle = SessionId::random();
        let active = SessionId::random();
        store.write(idle, slot(1), b"idle".to_vec());
        store.write(idle, slot(2), b"idle2".to_vec());
        store.write(active, slot(1), b"active".to_vec());
        store.last_seen.insert(idle, 10);
        store.last_seen.insert(active, 1_000);

        assert_eq!(store.evict_seen_before(500), 1);

        assert!(store.read(idle, slot(1)).is_none());
        assert!(store.drain_set_instructions(idle).is_empty(), "no dirty entry outlives its session");
        assert_eq!(store.read(active, slot(1)), Some(b"active".to_vec()));
        assert_eq!(store.len(), 1);
    }

    /// A read keeps a session alive as much as a write does: a tab that only
    /// renders what it set earlier is still in use.
    #[test]
    fn reading_a_slot_refreshes_its_session() {
        let store = SlotStore::new();
        let session = SessionId::random();
        store.write(session, slot(1), b"v".to_vec());
        store.last_seen.insert(session, 0);
        std::thread::sleep(Duration::from_millis(5));
        let _ = store.read(session, slot(1));
        let seen = *store.last_seen.get(&session).unwrap();
        assert!(seen >= 5, "the read restamped the session: {seen}");
        assert_eq!(store.evict_seen_before(1), 0, "read after the cutoff, so it stays");
    }

    #[test]
    fn a_zero_ttl_sweep_with_nothing_written_is_a_no_op() {
        let store = SlotStore::new();
        assert_eq!(store.evict_idle(Duration::ZERO), 0);
    }
}
