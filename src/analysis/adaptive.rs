//! Whether a fan-out is worth its thread-spawn overhead.
//!
//! This module used to be a `GranularityController` struct holding a
//! `sysinfo::System`, live CPU/memory sampling and a cache-miss counter. Every
//! one of those fields existed for `calculate_chunk_size` and
//! `record_batch_metrics` — which **no caller ever invoked**, in `src/`, in
//! `tests/`, or in the benches. Meanwhile `GranularityController::new()` runs
//! `System::new_all()`, a full enumeration of every process, disk and network
//! interface on the machine, and it ran on **every** `optimize()`,
//! `optimize_incremental()`, `optimize_canonical_ir_columns()` and
//! `RenderPipeline` construction.
//!
//! The only method anything called was `should_parallelize`, and it reads no
//! field of `self` — so the scan was pure cost. It is now the free function
//! below, with the arithmetic preserved operation-for-operation (including the
//! integer divisions, which truncate and are load-bearing at small
//! `total_items`). `threshold_is_unchanged_by_the_collapse` pins that.
//!
//! Deliberately still a heuristic, not a cost model: the constants below are
//! unitless and were never calibrated against a measurement. That is fine for
//! what it decides — serial-vs-rayon on a graph whose size is known — and
//! calling it a heuristic in the open is better than dressing it up with a
//! system scan whose result is discarded.

/// Cost of putting one worker thread to use, in the same unitless currency as
/// [`WORK_PER_BYTE`].
const PARALLELISM_OVERHEAD: usize = 1_000;

/// Work attributed to one item, as a multiple of its size in bytes.
const WORK_PER_BYTE: usize = 10;

/// Parallel has to beat serial by this margin (8/10) before it is worth it —
/// headroom for the fact that the estimate either side of the comparison is a
/// guess.
const MARGIN_NUMERATOR: usize = 8;
const MARGIN_DENOMINATOR: usize = 10;

/// Returns `true` when fanning `total_items` out across the available cores is
/// expected to beat processing them serially.
///
/// `item_size_bytes` stands in for per-item work: bigger items are assumed to
/// cost proportionally more to process, which is crude but monotonic in the
/// right direction. Small graphs amortize thread spawn poorly, so they stay
/// serial.
pub fn should_parallelize(total_items: usize, item_size_bytes: usize) -> bool {
    should_parallelize_on(total_items, item_size_bytes, num_cpus::get())
}

/// [`should_parallelize`] for an explicit worker count, so the decision can be
/// tested for the machines it runs on rather than the one the test runs on.
///
/// 🪤 Fewer than two items, or fewer than two workers, is always serial. The
/// cost model's `total_items / workers` truncates, so on a 2–4 core machine
/// (every CI runner) one item's work rounded to **zero** and a singleton "won"
/// against itself run serially — the tests only passed on a many-core rig.
fn should_parallelize_on(total_items: usize, item_size_bytes: usize, workers: usize) -> bool {
    if total_items < 2 || workers < 2 {
        return false;
    }
    let work_per_item = item_size_bytes * WORK_PER_BYTE;

    let sequential_cost = total_items * work_per_item;
    let parallel_cost = (total_items / workers) * work_per_item + workers * PARALLELISM_OVERHEAD;

    parallel_cost < sequential_cost * MARGIN_NUMERATOR / MARGIN_DENOMINATOR
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two cases the old `GranularityController` test asserted, kept
    /// verbatim so the collapse is provably behavior-preserving at the
    /// boundaries anyone had written down.
    /// The two cases the old `GranularityController` test asserted, on the
    /// 8- and 16-core machines they were written on. On 4 cores ten 100-byte
    /// items *do* clear the margin (2×1000 + 4×1000 < 8000), so the old
    /// unparameterised form was a statement about the test machine.
    #[test]
    fn test_should_parallelize() {
        for workers in [8, 16] {
            assert!(!should_parallelize_on(10, 100, workers), "{workers} workers");
            assert!(should_parallelize_on(1000, 1000, workers), "{workers} workers");
        }
    }

    /// Re-implements the pre-collapse expression *literally* and asserts
    /// agreement across a sweep, for every machine size CI and the dev rig
    /// have. This is the regression guard for the refactor itself: it fails if
    /// anyone "tidies" the integer division into float math. The one deliberate
    /// departure — fewer than two items or workers is serial — is excluded
    /// here and pinned by `empty_and_singleton_inputs_stay_serial`.
    #[test]
    fn threshold_is_unchanged_by_the_collapse() {
        fn original(total_items: usize, item_size_bytes: usize, workers: usize) -> bool {
            let parallelism_overhead = 1000;
            let estimated_work_per_item = item_size_bytes * 10;

            let sequential_cost = total_items * estimated_work_per_item;
            let parallel_cost =
                (total_items / workers) * estimated_work_per_item + (workers * parallelism_overhead);

            parallel_cost < (sequential_cost * 8 / 10)
        }

        for workers in [2, 3, 4, 8, 16, 32] {
            for total_items in [2, 4, 8, 15, 16, 17, 32, 64, 128, 1_000, 10_000] {
                for item_size_bytes in [1, 8, 64, 100, 256, 1_000, 4_096] {
                    assert_eq!(
                        should_parallelize_on(total_items, item_size_bytes, workers),
                        original(total_items, item_size_bytes, workers),
                        "disagreement at total_items={total_items}, \
                         item_size_bytes={item_size_bytes}, workers={workers}"
                    );
                }
            }
        }
    }

    /// An empty graph, a single item, or a single core must never pay for
    /// threads — on any machine. Before the guard, `(1, 1_000)` parallelised on
    /// 2–4 cores because `1 / workers` truncated the item's work to zero.
    #[test]
    fn empty_and_singleton_inputs_stay_serial() {
        for workers in [1, 2, 3, 4, 8, 16, 64] {
            for item_size_bytes in [1, 1_000, 4_096, 1 << 20] {
                assert!(!should_parallelize_on(0, item_size_bytes, workers));
                assert!(!should_parallelize_on(1, item_size_bytes, workers));
            }
        }
        for total_items in [2, 100, 10_000] {
            assert!(!should_parallelize_on(total_items, 4_096, 1), "one core is serial");
        }
    }
}
