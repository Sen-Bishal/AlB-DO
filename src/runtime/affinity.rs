//! Optional core-pinned rayon pool for the 4-lane workers.
//!
//! Each worker thread is bound to a physical core via `core_affinity` so the
//! lane partition stays put across kernel scheduling decisions, reducing
//! cross-NUMA migrations and warming the per-core L1/L2.
//!
//! The constructor returns `None` when pinning isn't available (CI sandbox,
//! restricted containers, etc.), letting the caller fall back to the global
//! rayon pool with no behavior change.

/// Builds a rayon pool whose workers are pinned to physical cores.
///
/// Workers are mapped to cores round-robin so `num_threads > core count`
/// gracefully degrades to multiple workers per core. Returns `None` if the
/// platform refuses to enumerate core ids or the pool fails to build.
pub fn build_pinned_rayon_pool(num_threads: usize) -> Option<rayon::ThreadPool> {
    let core_ids = core_affinity::get_core_ids()?;
    if core_ids.is_empty() {
        return None;
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .start_handler(move |worker_idx| {
            // Round-robin so worker N pins to core (N % core_count).
            if let Some(core_id) = core_ids.get(worker_idx % core_ids.len()) {
                core_affinity::set_for_current(*core_id);
            }
        })
        .build()
        .ok()
}
