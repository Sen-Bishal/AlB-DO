use super::hot_set::{HotSetError, HotSetRegistry, RenderPriority, RingDrainStats, SentinelRing};
use crate::types::ComponentId;
use crossbeam::queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_OVERTAKE_INTERVAL: usize = 100;
pub const DEFAULT_OVERTAKE_BUDGET_MS: u64 = 2;
pub const DEFAULT_ANALYZER_QUEUE_CAPACITY: usize = 4096;
pub const DEFAULT_RENDER_QUEUE_CAPACITY: usize = 4096;
pub const DEFAULT_ANALYZER_CHUNK_LIMIT: usize = 400;

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub overtake_budget: Duration,
    pub overtake_interval: usize,
    pub analyzer_queue_capacity: usize,
    pub render_queue_capacity: usize,
    pub initial_analyzer_chunk_limit: usize,
    pub max_analyzer_chunk_limit: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            overtake_budget: Duration::from_millis(DEFAULT_OVERTAKE_BUDGET_MS),
            overtake_interval: DEFAULT_OVERTAKE_INTERVAL,
            analyzer_queue_capacity: DEFAULT_ANALYZER_QUEUE_CAPACITY,
            render_queue_capacity: DEFAULT_RENDER_QUEUE_CAPACITY,
            initial_analyzer_chunk_limit: DEFAULT_ANALYZER_CHUNK_LIMIT,
            max_analyzer_chunk_limit: 4096,
        }
    }
}

impl SchedulerConfig {
    fn normalized(self) -> Self {
        let interval = self.overtake_interval.max(1);
        let initial = self.initial_analyzer_chunk_limit.max(interval);
        let max_limit = self.max_analyzer_chunk_limit.max(initial);
        let analyzer_capacity = self.analyzer_queue_capacity.max(1);
        let render_capacity = self.render_queue_capacity.max(1);

        Self {
            overtake_budget: self.overtake_budget,
            overtake_interval: interval,
            analyzer_queue_capacity: analyzer_capacity,
            render_queue_capacity: render_capacity,
            initial_analyzer_chunk_limit: initial,
            max_analyzer_chunk_limit: max_limit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerFrameStats {
    pub hot_set_rendered: usize,
    pub hot_set_dropped: usize,
    pub analyzer_processed: usize,
    pub analyzer_forwarded: usize,
    pub analyzer_discarded_hot_set: usize,
    pub analyzer_dropped: usize,
    pub overtakes: usize,
    pub analyzer_chunk_limit: usize,
    pub total_frame_time_ms: u128,
}

impl SchedulerFrameStats {
    fn new(analyzer_chunk_limit: usize) -> Self {
        Self {
            hot_set_rendered: 0,
            hot_set_dropped: 0,
            analyzer_processed: 0,
            analyzer_forwarded: 0,
            analyzer_discarded_hot_set: 0,
            analyzer_dropped: 0,
            overtakes: 0,
            analyzer_chunk_limit,
            total_frame_time_ms: 0,
        }
    }
}

pub struct OvertakeZoneScheduler {
    config: SchedulerConfig,
    hot_set_registry: HotSetRegistry,
    sentinel_ring: SentinelRing,
    analyzer_queue: ArrayQueue<ComponentId>,
    render_queue: ArrayQueue<ComponentId>,
    analyzer_chunk_limit: AtomicUsize,
}

impl OvertakeZoneScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        let config = config.normalized();
        let hot_set_registry = HotSetRegistry::new();
        let sentinel_ring =
            SentinelRing::from_registry(&hot_set_registry).expect("empty hot set is always valid");

        Self {
            config: config.clone(),
            hot_set_registry,
            sentinel_ring,
            analyzer_queue: ArrayQueue::new(config.analyzer_queue_capacity),
            render_queue: ArrayQueue::new(config.render_queue_capacity),
            analyzer_chunk_limit: AtomicUsize::new(config.initial_analyzer_chunk_limit),
        }
    }

    pub fn with_hot_set(
        config: SchedulerConfig,
        entries: &[(ComponentId, RenderPriority)],
    ) -> Result<Self, HotSetError> {
        let config = config.normalized();
        let hot_set_registry = HotSetRegistry::new();
        for (component_id, priority) in entries {
            hot_set_registry.register(*component_id, *priority)?;
        }
        let sentinel_ring = SentinelRing::from_registry(&hot_set_registry)?;

        Ok(Self {
            config: config.clone(),
            hot_set_registry,
            sentinel_ring,
            analyzer_queue: ArrayQueue::new(config.analyzer_queue_capacity),
            render_queue: ArrayQueue::new(config.render_queue_capacity),
            analyzer_chunk_limit: AtomicUsize::new(config.initial_analyzer_chunk_limit),
        })
    }

    pub fn configure_hot_set(
        &mut self,
        entries: &[(ComponentId, RenderPriority)],
    ) -> Result<(), HotSetError> {
        let updated_registry = HotSetRegistry::new();
        for (component_id, priority) in entries {
            updated_registry.register(*component_id, *priority)?;
        }
        let rebuilt_ring = SentinelRing::from_registry(&updated_registry)?;

        self.hot_set_registry = updated_registry;
        self.sentinel_ring = rebuilt_ring;
        Ok(())
    }

    pub fn register_hot_component(
        &mut self,
        component_id: ComponentId,
        priority: RenderPriority,
    ) -> Result<(), HotSetError> {
        self.hot_set_registry.register(component_id, priority)?;
        self.sentinel_ring
            .rebuild_from_registry(&self.hot_set_registry)?;
        Ok(())
    }

    pub fn deregister_hot_component(&mut self, component_id: ComponentId) {
        if self.hot_set_registry.deregister(component_id) {
            let _ = self
                .sentinel_ring
                .rebuild_from_registry(&self.hot_set_registry);
        }
    }

    pub fn submit_analyzer_result(&self, component_id: ComponentId) -> bool {
        self.analyzer_queue.push(component_id).is_ok()
    }

    pub fn mark_hot_dirty(&self, component_id: ComponentId) -> bool {
        if !self.hot_set_registry.contains(component_id) {
            return false;
        }
        self.sentinel_ring.mark_dirty(component_id)
    }

    pub fn pop_render_ready(&self) -> Option<ComponentId> {
        self.render_queue.pop()
    }

    pub fn render_queue_len(&self) -> usize {
        self.render_queue.len()
    }

    pub fn analyzer_queue_len(&self) -> usize {
        self.analyzer_queue.len()
    }

    pub fn analyzer_chunk_limit(&self) -> usize {
        self.analyzer_chunk_limit.load(Ordering::Acquire)
    }

    pub fn run_frame(&self) -> SchedulerFrameStats {
        let frame_start = Instant::now();
        let current_limit = self.analyzer_chunk_limit();
        let mut stats = SchedulerFrameStats::new(current_limit);

        let hot_stats: RingDrainStats = self.sentinel_ring.drain_to_queue(&self.render_queue);
        stats.hot_set_rendered = hot_stats.pushed;
        stats.hot_set_dropped = hot_stats.dropped;

        let analyzer_start = Instant::now();
        let mut processed_since_zone = 0usize;
        while stats.analyzer_processed < current_limit {
            let Some(component_id) = self.analyzer_queue.pop() else {
                break;
            };

            stats.analyzer_processed += 1;
            processed_since_zone += 1;

            if self.hot_set_registry.contains(component_id) {
                stats.analyzer_discarded_hot_set += 1;
            } else if self.render_queue.push(component_id).is_ok() {
                stats.analyzer_forwarded += 1;
            } else {
                stats.analyzer_dropped += 1;
            }

            if processed_since_zone >= self.config.overtake_interval {
                if analyzer_start.elapsed() >= self.config.overtake_budget {
                    stats.overtakes += 1;
                    break;
                }
                processed_since_zone = 0;
            }
        }

        self.adjust_analyzer_chunk_limit(&stats);
        stats.total_frame_time_ms = frame_start.elapsed().as_millis();
        stats
    }

    fn adjust_analyzer_chunk_limit(&self, stats: &SchedulerFrameStats) {
        let current = self.analyzer_chunk_limit();
        let mut next = current;
        let interval = self.config.overtake_interval;
        let min_limit = interval;
        let max_limit = self.config.max_analyzer_chunk_limit;

        if stats.overtakes > 0 {
            next = current.saturating_sub(interval).max(min_limit);
        } else if stats.analyzer_processed >= current && stats.analyzer_dropped == 0 {
            next = current.saturating_add(interval).min(max_limit);
        }

        if next != current {
            self.analyzer_chunk_limit.store(next, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(id: u64) -> ComponentId {
        ComponentId::new(id)
    }

    #[test]
    fn test_scheduler_discards_hot_set_components_from_analyzer_queue() {
        let scheduler = OvertakeZoneScheduler::with_hot_set(
            SchedulerConfig::default(),
            &[(component(1), RenderPriority::Critical)],
        )
        .unwrap();

        scheduler.submit_analyzer_result(component(1));
        scheduler.submit_analyzer_result(component(2));

        let stats = scheduler.run_frame();
        assert_eq!(stats.analyzer_discarded_hot_set, 1);
        assert_eq!(stats.analyzer_forwarded, 1);
        assert_eq!(scheduler.pop_render_ready(), Some(component(2)));
        assert_eq!(scheduler.pop_render_ready(), None);
    }

    #[test]
    fn test_scheduler_drains_hot_set_before_analyzer_path() {
        let scheduler = OvertakeZoneScheduler::with_hot_set(
            SchedulerConfig::default(),
            &[(component(10), RenderPriority::High)],
        )
        .unwrap();

        assert!(scheduler.mark_hot_dirty(component(10)));
        scheduler.submit_analyzer_result(component(11));

        let stats = scheduler.run_frame();
        assert_eq!(stats.hot_set_rendered, 1);
        assert_eq!(stats.analyzer_forwarded, 1);
        assert_eq!(scheduler.pop_render_ready(), Some(component(10)));
        assert_eq!(scheduler.pop_render_ready(), Some(component(11)));
    }

    #[test]
    fn test_scheduler_overtakes_on_budget_boundary() {
        let scheduler = OvertakeZoneScheduler::new(SchedulerConfig {
            overtake_budget: Duration::ZERO,
            overtake_interval: 2,
            initial_analyzer_chunk_limit: 10,
            ..SchedulerConfig::default()
        });

        for id in 0..6 {
            assert!(scheduler.submit_analyzer_result(component(id)));
        }

        let stats = scheduler.run_frame();
        assert_eq!(stats.overtakes, 1);
        assert_eq!(stats.analyzer_processed, 2);
        assert_eq!(scheduler.analyzer_queue_len(), 4);
    }

    #[test]
    fn test_scheduler_adaptive_chunk_limit_reduces_when_overtaking() {
        let scheduler = OvertakeZoneScheduler::new(SchedulerConfig {
            overtake_budget: Duration::ZERO,
            overtake_interval: 2,
            initial_analyzer_chunk_limit: 8,
            ..SchedulerConfig::default()
        });

        for id in 0..20 {
            scheduler.submit_analyzer_result(component(id));
        }

        let before = scheduler.analyzer_chunk_limit();
        let stats = scheduler.run_frame();
        let after = scheduler.analyzer_chunk_limit();

        assert!(stats.overtakes > 0);
        assert!(after < before);
    }

    #[test]
    fn test_scheduler_adaptive_chunk_limit_grows_when_no_overtake() {
        let scheduler = OvertakeZoneScheduler::new(SchedulerConfig {
            overtake_budget: Duration::from_secs(1),
            overtake_interval: 2,
            initial_analyzer_chunk_limit: 4,
            max_analyzer_chunk_limit: 6,
            ..SchedulerConfig::default()
        });

        for id in 0..4 {
            scheduler.submit_analyzer_result(component(id));
        }

        let before = scheduler.analyzer_chunk_limit();
        let stats = scheduler.run_frame();
        let after = scheduler.analyzer_chunk_limit();

        assert_eq!(stats.overtakes, 0);
        assert_eq!(stats.analyzer_processed, before);
        assert!(after > before);
    }
}