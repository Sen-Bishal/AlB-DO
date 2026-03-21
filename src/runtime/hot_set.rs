use crate::types::ComponentId;
use crossbeam::queue::ArrayQueue;
use crossbeam::utils::CachePadded;
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

pub const HOT_SET_MAX: usize = 32;
const DIRTY_FALSE: u8 = 0;
const DIRTY_TRUE: u8 = 1;

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

#[derive(Debug)]
struct RingNode {
    component_id: Option<ComponentId>,
    dirty: AtomicU8,
    next: NonNull<CachePadded<RingNode>>,
}

impl RingNode {
    fn sentinel() -> Self {
        Self {
            component_id: None,
            dirty: AtomicU8::new(DIRTY_FALSE),
            next: NonNull::dangling(),
        }
    }

    fn component(component_id: ComponentId) -> Self {
        Self {
            component_id: Some(component_id),
            dirty: AtomicU8::new(DIRTY_FALSE),
            next: NonNull::dangling(),
        }
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

#[derive(Debug)]
pub struct SentinelRing {
    sentinel: NonNull<CachePadded<RingNode>>,
    nodes: Vec<NonNull<CachePadded<RingNode>>>,
    node_index: HashMap<ComponentId, usize>,
    dirty_count: AtomicU32,
}

// Safety: ring topology is immutable after construction and shared access only mutates atomics.
unsafe impl Send for SentinelRing {}

// Safety: all concurrent state transitions are guarded by atomics; pointers are read-only links.
unsafe impl Sync for SentinelRing {}

impl SentinelRing {
    pub fn new(component_ids: &[ComponentId]) -> Result<Self, HotSetError> {
        let unique_ids = dedupe_ids(component_ids);
        if unique_ids.len() > HOT_SET_MAX {
            return Err(HotSetError::CapacityExceeded { max: HOT_SET_MAX });
        }

        let sentinel_ptr = leak_node(RingNode::sentinel());
        let mut nodes = Vec::with_capacity(unique_ids.len() + 1);
        nodes.push(sentinel_ptr);

        let mut node_index = HashMap::with_capacity(unique_ids.len());
        for component_id in unique_ids {
            let node_ptr = leak_node(RingNode::component(component_id));
            node_index.insert(component_id, nodes.len());
            nodes.push(node_ptr);
        }

        // Build a circular singly linked list: sentinel -> n1 -> n2 -> ... -> sentinel.
        unsafe {
            if nodes.len() == 1 {
                let sentinel = &mut *sentinel_ptr.as_ptr();
                sentinel.next = sentinel_ptr;
            } else {
                let sentinel = &mut *sentinel_ptr.as_ptr();
                sentinel.next = nodes[1];
                for idx in 1..nodes.len() {
                    let next = if idx + 1 < nodes.len() {
                        nodes[idx + 1]
                    } else {
                        sentinel_ptr
                    };
                    let node = &mut *nodes[idx].as_ptr();
                    node.next = next;
                }
            }
        }

        Ok(Self {
            sentinel: sentinel_ptr,
            nodes,
            node_index,
            dirty_count: AtomicU32::new(0),
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
        self.nodes.len().saturating_sub(1)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, component_id: ComponentId) -> bool {
        self.node_index.contains_key(&component_id)
    }

    pub fn dirty_count(&self) -> u32 {
        self.dirty_count.load(Ordering::Acquire)
    }

    pub fn mark_dirty(&self, component_id: ComponentId) -> bool {
        let Some(index) = self.node_index.get(&component_id).copied() else {
            return false;
        };

        let node_ptr = self.nodes[index];
        let node = unsafe { node_ptr.as_ref() };

        let flipped = node
            .dirty
            .compare_exchange(DIRTY_FALSE, DIRTY_TRUE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();

        if flipped {
            self.dirty_count.fetch_add(1, Ordering::AcqRel);
        }

        flipped
    }

    pub fn drain<F>(&self, mut on_dirty: F) -> usize
    where
        F: FnMut(ComponentId),
    {
        if self.dirty_count.load(Ordering::Acquire) == 0 {
            return 0;
        }

        let mut drained = 0;
        unsafe {
            let sentinel = self.sentinel;
            let mut cursor = sentinel.as_ref().next;

            while cursor != sentinel {
                let node = cursor.as_ref();
                let was_dirty = node.dirty.swap(DIRTY_FALSE, Ordering::Relaxed) == DIRTY_TRUE;
                if was_dirty {
                    self.dirty_count.fetch_sub(1, Ordering::AcqRel);
                    if let Some(component_id) = node.component_id {
                        on_dirty(component_id);
                        drained += 1;
                    }
                }
                cursor = node.next;
            }
        }

        drained
    }

    pub fn drain_to_queue(&self, queue: &ArrayQueue<ComponentId>) -> RingDrainStats {
        if self.dirty_count.load(Ordering::Acquire) == 0 {
            return RingDrainStats::empty();
        }

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

impl Drop for SentinelRing {
    fn drop(&mut self) {
        for ptr in self.nodes.drain(..) {
            unsafe {
                drop(Box::from_raw(ptr.as_ptr()));
            }
        }
    }
}

fn leak_node(node: RingNode) -> NonNull<CachePadded<RingNode>> {
    let leaked = Box::leak(Box::new(CachePadded::new(node)));
    NonNull::from(leaked)
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
