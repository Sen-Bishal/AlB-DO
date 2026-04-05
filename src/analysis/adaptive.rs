use std::sync::atomic::{AtomicUsize, Ordering};
use sysinfo::System;
// Fix this
pub struct GranularityController {
    system: System,
    recent_cache_misses: AtomicUsize,
    recent_throughput: AtomicUsize,
    min_chunk_size: usize,
    max_chunk_size: usize,
}

impl GranularityController {
    pub fn new() -> Self {
        Self {
            system: System::new_all(),
            recent_cache_misses: AtomicUsize::new(0),
            recent_throughput: AtomicUsize::new(0),
            min_chunk_size: 4,
            max_chunk_size: 1024,
        }
    }

    pub fn calculate_chunk_size(&mut self, total_items: usize) -> usize {
        self.system.refresh_cpu();
        self.system.refresh_memory();

        let cpu_load = self.system.global_cpu_info().cpu_usage();
        let cpu_factor = if cpu_load > 80.0 {
            0.5
        } else if cpu_load < 20.0 {
            2.0
        } else {
            1.0
        };

        let mem_available = self.system.available_memory();
        let mem_total = self.system.total_memory();
        let mem_pressure = 1.0 - (mem_available as f64 / mem_total as f64);
        let mem_factor = if mem_pressure > 0.8 {
            0.3
        } else if mem_pressure < 0.3 {
            1.5
        } else {
            1.0
        };

        let cache_miss_rate = self.recent_cache_misses.load(Ordering::Relaxed) as f64
            / self.recent_throughput.load(Ordering::Relaxed).max(1) as f64;
        let cache_factor = if cache_miss_rate > 0.3 { 0.4 } else { 1.2 };

        let base_chunk_size = (total_items as f64 / num_cpus::get() as f64).sqrt();
        let adjusted = base_chunk_size * cpu_factor * mem_factor * cache_factor;

        adjusted
            .max(self.min_chunk_size as f64)
            .min(self.max_chunk_size as f64) as usize
    }

    pub fn should_parallelize(&self, total_items: usize, item_size_bytes: usize) -> bool {
        let parallelism_overhead = 1000;
        let estimated_work_per_item = item_size_bytes * 10;

        let sequential_cost = total_items * estimated_work_per_item;
        let parallel_cost = (total_items / num_cpus::get()) * estimated_work_per_item
            + (num_cpus::get() * parallelism_overhead);

        parallel_cost < (sequential_cost * 8 / 10)
    }

    pub fn record_batch_metrics(&self, cache_misses: usize, items_processed: usize) {
        self.recent_cache_misses
            .fetch_add(cache_misses, Ordering::Relaxed);
        self.recent_throughput
            .fetch_add(items_processed, Ordering::Relaxed);
    }
}

impl Default for GranularityController {
    fn default() -> Self {
        Self::new()
    }
}

pub fn cache_aligned_chunk_size<T>() -> usize {
    const L3_CACHE_SIZE: usize = 8 * 1024 * 1024;
    const SAFETY_FACTOR: f64 = 0.7;

    let item_size = std::mem::size_of::<T>();
    let max_items = (L3_CACHE_SIZE as f64 * SAFETY_FACTOR) / item_size as f64;

    2_usize.pow(max_items.log2().floor() as u32)
}

#[derive(Debug, Clone, Copy)]
pub enum ProcessingStrategy {
    Sequential,
    Parallel { chunk_size: usize },
}

pub fn determine_strategy<T>(total_size: usize) -> ProcessingStrategy {
    let item_size = std::mem::size_of::<T>();
    let total_bytes = total_size * item_size;

    match total_bytes {
        size if size < 32_000 => ProcessingStrategy::Sequential,
        size if size < 256_000 => ProcessingStrategy::Parallel {
            chunk_size: cache_aligned_chunk_size::<T>() / 4,
        },
        size if size < 8_000_000 => ProcessingStrategy::Parallel {
            chunk_size: cache_aligned_chunk_size::<T>(),
        },
        _ => ProcessingStrategy::Parallel {
            chunk_size: cache_aligned_chunk_size::<T>() * 2,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_granularity_controller() {
        let mut controller = GranularityController::new();

        let chunk_size = controller.calculate_chunk_size(1000);
        assert!(chunk_size >= 4);
        assert!(chunk_size <= 1024);
    }

    #[test]
    fn test_should_parallelize() {
        let controller = GranularityController::new();

        assert!(!controller.should_parallelize(10, 100));
        assert!(controller.should_parallelize(1000, 1000));
    }

    #[test]
    fn test_cache_aligned_chunk_size() {
        let chunk_size = cache_aligned_chunk_size::<u64>();
        assert!(chunk_size > 0);
        assert!(chunk_size.is_power_of_two());
    }

    #[test]
    fn test_determine_strategy() {
        let strategy = determine_strategy::<u64>(100);
        matches!(strategy, ProcessingStrategy::Sequential);

        let strategy = determine_strategy::<u64>(100_000);
        matches!(strategy, ProcessingStrategy::Parallel { .. });
    }
}
