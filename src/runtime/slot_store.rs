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

use crate::ir::opcode::{Instruction, SlotId};
use crate::runtime::session::SessionId;
use dashmap::DashMap;
use rustc_hash::FxHashSet;
use std::sync::{Arc, Mutex};

/// Concurrent server-side slot store. Constructed once per server and
/// shared via `Arc<SlotStore>`.
#[derive(Debug, Default)]
pub struct SlotStore {
    /// `(session, slot) -> value`. The value is owned `Vec<u8>` so
    /// readers can return a clone without holding a `DashMap` ref
    /// across an `.await`.
    values: DashMap<(SessionId, SlotId), Vec<u8>>,
    /// Pending dirty keys. Writes push here; drains move every entry
    /// into a SlotSet vector and clear the set. `FxHashSet` because
    /// hashing here is on the hot path.
    dirty: Mutex<FxHashSet<(SessionId, SlotId)>>,
}

impl SlotStore {
    /// Returns a fresh, empty slot store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the current value of a slot. Returns `None` when the slot
    /// has not been written for this session yet. Allocates — for hot
    /// paths that compare against the previous value, consider
    /// computing the hash before the read.
    #[must_use]
    pub fn read(&self, session: SessionId, slot_id: SlotId) -> Option<Vec<u8>> {
        self.values.get(&(session, slot_id)).map(|entry| entry.clone())
    }

    /// Writes a new value into the slot and records it as dirty so the
    /// next [`Self::drain_set_instructions`] call ships a `SlotSet`
    /// opcode. Idempotent: writing the same key twice between drains
    /// produces one `SlotSet` carrying the final value (HashSet dedup).
    pub fn write(&self, session: SessionId, slot_id: SlotId, value: Vec<u8>) {
        self.values.insert((session, slot_id), value);
        if let Ok(mut dirty) = self.dirty.lock() {
            dirty.insert((session, slot_id));
        }
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

    /// Removes every value belonging to `session`. Call this when the
    /// session's WT connection closes so the table doesn't grow
    /// unbounded across long-lived servers (Risk #13).
    pub fn clear_session(&self, session: SessionId) {
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
}
