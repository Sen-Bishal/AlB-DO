use crate::runtime::dirty_bitmap::DirtyBitmap;
use crate::types::ComponentId;
use crossbeam::queue::ArrayQueue;
use dashmap::DashMap;
use rustc_hash::FxHashMap;
use std::collections::HashSet;

pub const HOT_SET_MAX: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum RenderPriority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}

impl Default for RenderPriority {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum HotSetError {
    #[error("hot set capacity exceeded: max={max}")]
    CapacityExceeded { max: usize },
}

#[derive(Debug)]
pub struct HotSetRegistry {
    entries: DashMap<ComponentId, RenderPriority>,
    max_size: usize,
}

impl Default for HotSetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HotSetRegistry {
    pub fn new() -> Self {
        Self::with_max_size(HOT_SET_MAX)
    }

    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            entries: DashMap::new(),
            max_size: max_size.max(1),
        }
    }

    pub fn register(
        &self,
        component_id: ComponentId,
        priority: RenderPriority,
    ) -> Result<bool, HotSetError> {
        if let Some(mut current) = self.entries.get_mut(&component_id) {
            *current = priority;
            return Ok(false);
        }

        if self.entries.len() >= self.max_size {
            return Err(HotSetError::CapacityExceeded { max: self.max_size });
        }

        self.entries.insert(component_id, priority);
        Ok(true)
    }

    pub fn deregister(&self, component_id: ComponentId) -> bool {
        self.entries.remove(&component_id).is_some()
    }

    pub fn contains(&self, component_id: ComponentId) -> bool {
        self.entries.contains_key(&component_id)
    }

    pub fn priority(&self, component_id: ComponentId) -> Option<RenderPriority> {
        self.entries.get(&component_id).map(|entry| *entry)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn max_size(&self) -> usize {
        self.max_size
    }

    pub fn snapshot_ids_sorted(&self) -> Vec<ComponentId> {
        let mut ids = self
            .entries
            .iter()
            .map(|entry| *entry.key())
            .collect::<Vec<_>>();
        ids.sort_unstable_by_key(|id| id.as_u64());
        ids
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingDrainStats {
    pub drained: usize,
    pub pushed: usize,
    pub dropped: usize,
}

impl RingDrainStats {
    fn empty() -> Self {
        Self {
            drained: 0,
            pushed: 0,
            dropped: 0,
        }
    }
}

/// Lock-free dirty tracker for the hot-set.
///
/// Cycle 2 of the SoA IR refactor replaced the prior `NonNull`-based
/// `CachePadded<RingNode>` linked list with a flat
/// [`DirtyBitmap`](crate::runtime::dirty_bitmap::DirtyBitmap) addressed by
/// slot index. The struct keeps the historical `SentinelRing` name and
/// public surface so the scheduler and CLI integrations need no changes:
/// internally it is a bitmap, externally `mark_dirty` / `drain` /
/// `drain_to_queue` behave exactly as before but eliminate the per-node
/// pointer chase that dominated the prior drain cost.
#[derive(Debug)]
pub struct SentinelRing {
    slot_for: FxHashMap<ComponentId, usize>,
    id_for: Vec<ComponentId>,
    bitmap: DirtyBitmap,
}

impl SentinelRing {
    pub fn new(component_ids: &[ComponentId]) -> Result<Self, HotSetError> {
        let unique_ids = dedupe_ids(component_ids);
        if unique_ids.len() > HOT_SET_MAX {
            return Err(HotSetError::CapacityExceeded { max: HOT_SET_MAX });
        }

        let bitmap = DirtyBitmap::with_capacity(unique_ids.len());
        let mut slot_for =
            FxHashMap::with_capacity_and_hasher(unique_ids.len(), Default::default());
        for (slot, id) in unique_ids.iter().enumerate() {
            slot_for.insert(*id, slot);
        }

        Ok(Self {
            slot_for,
            id_for: unique_ids,
            bitmap,
        })
    }

    pub fn from_registry(registry: &HotSetRegistry) -> Result<Self, HotSetError> {
        let ids = registry.snapshot_ids_sorted();
        Self::new(ids.as_slice())
    }

    pub fn rebuild(&mut self, component_ids: &[ComponentId]) -> Result<(), HotSetError> {
        *self = Self::new(component_ids)?;
        Ok(())
    }

    pub fn rebuild_from_registry(&mut self, registry: &HotSetRegistry) -> Result<(), HotSetError> {
        let ids = registry.snapshot_ids_sorted();
        self.rebuild(ids.as_slice())
    }

    pub fn len(&self) -> usize {
        self.id_for.len()
    }

    pub fn is_empty(&self) -> bool {
        self.id_for.is_empty()
    }

    pub fn contains(&self, component_id: ComponentId) -> bool {
        self.slot_for.contains_key(&component_id)
    }

    /// Returns the live count of dirty bits.
    ///
    /// `O(word_count)` — the hot set is bounded by [`HOT_SET_MAX`], so this
    /// is one bitmap word worth of work in practice.
    pub fn dirty_count(&self) -> u32 {
        self.bitmap.count_set() as u32
    }

    /// Atomically marks `component_id` dirty. Returns `true` iff the slot
    /// transitioned from clean to dirty (matching the prior `SentinelRing`
    /// contract used by the scheduler).
    pub fn mark_dirty(&self, component_id: ComponentId) -> bool {
        let Some(&slot) = self.slot_for.get(&component_id) else {
            return false;
        };
        self.bitmap.mark(slot)
    }

    /// Drains every dirty slot, calling `on_dirty(component_id)` once per
    /// slot, and returns the total drained count.
    ///
    /// Order: ascending by internal slot index, which matches the order in
    /// which ids were registered (the constructor preserves caller order;
    /// [`Self::from_registry`] sorts ids by `as_u64()` first, so its drain
    /// is sorted-ascending — relied on by existing scheduler tests).
    pub fn drain<F>(&self, mut on_dirty: F) -> usize
    where
        F: FnMut(ComponentId),
    {
        self.bitmap.drain(|slot| {
            if let Some(component_id) = self.id_for.get(slot) {
                on_dirty(*component_id);
            }
        })
    }

    pub fn drain_to_queue(&self, queue: &ArrayQueue<ComponentId>) -> RingDrainStats {
        let mut stats = RingDrainStats::empty();
        stats.drained = self.drain(|component_id| {
            if queue.push(component_id).is_ok() {
                stats.pushed += 1;
            } else {
                stats.dropped += 1;
            }
        });
        stats
    }
}

fn dedupe_ids(component_ids: &[ComponentId]) -> Vec<ComponentId> {
    let mut seen = HashSet::new();
    let mut unique_ids = Vec::with_capacity(component_ids.len());
    for component_id in component_ids {
        if seen.insert(*component_id) {
            unique_ids.push(*component_id);
        }
    }
    unique_ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_hot_set_registry_enforces_max_size() {
        let registry = HotSetRegistry::new();
        for id in 0..HOT_SET_MAX as u64 {
            let inserted = registry
                .register(ComponentId::new(id), RenderPriority::Normal)
                .unwrap();
            assert!(inserted);
        }
        assert_eq!(registry.len(), HOT_SET_MAX);

        let err = registry
            .register(ComponentId::new(HOT_SET_MAX as u64), RenderPriority::Normal)
            .unwrap_err();
        assert_eq!(err, HotSetError::CapacityExceeded { max: HOT_SET_MAX });

        let updated = registry
            .register(ComponentId::new(0), RenderPriority::Critical)
            .unwrap();
        assert!(!updated);
        assert_eq!(
            registry.priority(ComponentId::new(0)),
            Some(RenderPriority::Critical)
        );
    }

    #[test]
    fn test_sentinel_ring_mark_dirty_and_drain_keep_counter_in_sync() {
        let ring = SentinelRing::new(&[ComponentId::new(1), ComponentId::new(2)]).unwrap();

        assert_eq!(ring.dirty_count(), 0);
        assert!(ring.mark_dirty(ComponentId::new(1)));
        assert!(!ring.mark_dirty(ComponentId::new(1)));
        assert_eq!(ring.dirty_count(), 1);

        let mut drained = Vec::new();
        let drained_count = ring.drain(|component_id| drained.push(component_id));
        assert_eq!(drained_count, 1);
        assert_eq!(drained, vec![ComponentId::new(1)]);
        assert_eq!(ring.dirty_count(), 0);

        let drained_again = ring.drain(|_| {});
        assert_eq!(drained_again, 0);
    }

    #[test]
    fn test_sentinel_ring_rejects_unregistered_component_updates() {
        let ring = SentinelRing::new(&[ComponentId::new(10)]).unwrap();
        assert!(!ring.mark_dirty(ComponentId::new(99)));
        assert_eq!(ring.dirty_count(), 0);
    }

    #[test]
    fn test_sentinel_ring_drains_to_bounded_queue() {
        let ring = SentinelRing::new(&[
            ComponentId::new(1),
            ComponentId::new(2),
            ComponentId::new(3),
        ])
        .unwrap();
        ring.mark_dirty(ComponentId::new(1));
        ring.mark_dirty(ComponentId::new(2));
        ring.mark_dirty(ComponentId::new(3));

        let queue = ArrayQueue::new(2);
        let stats = ring.drain_to_queue(&queue);

        assert_eq!(
            stats,
            RingDrainStats {
                drained: 3,
                pushed: 2,
                dropped: 1,
            }
        );
        assert_eq!(queue.pop(), Some(ComponentId::new(1)));
        assert_eq!(queue.pop(), Some(ComponentId::new(2)));
        assert_eq!(queue.pop(), None);
        assert_eq!(ring.dirty_count(), 0);
    }

    #[test]
    fn test_sentinel_ring_concurrent_mark_dirty_does_not_double_count() {
        let ring = SentinelRing::new(&[ComponentId::new(7)]).unwrap();

        thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..1000 {
                        ring.mark_dirty(ComponentId::new(7));
                    }
                });
            }
        });

        assert_eq!(ring.dirty_count(), 1);
        let drained = ring.drain(|_| {});
        assert_eq!(drained, 1);
        assert_eq!(ring.dirty_count(), 0);
    }

    #[test]
    fn test_sentinel_ring_builds_from_registry_sorted_by_component_id() {
        let registry = HotSetRegistry::new();
        registry
            .register(ComponentId::new(5), RenderPriority::Normal)
            .unwrap();
        registry
            .register(ComponentId::new(2), RenderPriority::High)
            .unwrap();
        registry
            .register(ComponentId::new(9), RenderPriority::Low)
            .unwrap();

        let ring = SentinelRing::from_registry(&registry).unwrap();
        ring.mark_dirty(ComponentId::new(9));
        ring.mark_dirty(ComponentId::new(2));
        ring.mark_dirty(ComponentId::new(5));

        let mut drained = Vec::new();
        ring.drain(|component_id| drained.push(component_id));

        assert_eq!(
            drained,
            vec![
                ComponentId::new(2),
                ComponentId::new(5),
                ComponentId::new(9)
            ]
        );
    }
}
