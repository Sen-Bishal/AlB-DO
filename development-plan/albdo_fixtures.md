# ALBEDO Full-Product Wiring Plan

## Context

ALBEDO's core infrastructure (4-lane highway, WebTransport, frame tick, dirty bitmap, incremental cache, offset globals) is fully wired and production-ready. However, 7 distinct gaps exist where features were built and tested but never connected to the live execution path. This plan wires each one.

---

## What's Already Working (don't touch)

- 4-lane highway + cross-lane routing (`highway.rs`, `pipeline.rs`)
- WebTransport end-to-end: muxer → stream router → QUIC server (`webtransport.rs`, `frame.rs`, `crates/albedo-server/src/webtransport.rs`)
- Frame tick Cycle 2–5: drain → partition → emit → sequence
- Dirty bitmap mark/drain path (`dirty_bitmap.rs`)
- Incremental cache: hash, invalidate, persist, hit-rate tracking (`incremental.rs`)
- Lane offsets binary-search in `frame_tick` hot path

---

## Gaps to Wire (priority order)

---

### GAP 1 — SIMD Hash Diff Kernel never called from pipeline

**Files:** `src/runtime/dirty_bitmap.rs:183–264`, `src/runtime/pipeline.rs:38–82`

**Problem:** `hash_diff_into_bitmap()` and `hash_diff_into_bitmap_at()` exist as documented, benchmarked SIMD primitives. The pipeline's `mark_component_dirty()` (`pipeline.rs:164`) marks bits one at a time per component instead of using the bulk SIMD kernel. The whole-store reconcile path is missing.

**Fix:**
1. Add `prev_source_hashes: Vec<u64>` to `FourLaneRuntimePipeline` (after `dirty_bitmap`, line ~38). Initialize in `new()` by cloning `columns.source_hashes().to_vec()`.
2. Add `pub fn reconcile_source_hashes(&mut self) -> usize` to `FourLaneRuntimePipeline`:
   - Borrows `self.columns.source_hashes()` as `new_hashes`
   - Calls `hash_diff_into_bitmap(&self.prev_source_hashes, new_hashes, &self.dirty_bitmap)`
   - Updates `self.prev_source_hashes` via `copy_from_slice` (no allocation)
   - Returns mismatch count
3. Add `pub fn reconcile_lane_source_hashes(&mut self, lane: usize) -> usize` for per-lane reconcile:
   - Reads `self.columns.lane_offsets()` to get lane start
   - Calls `hash_diff_into_bitmap_at(old_lane_slice, new_lane_slice, &self.dirty_bitmap, start_idx)`
4. Add import at top of `pipeline.rs`:
   ```rust
   use super::dirty_bitmap::{hash_diff_into_bitmap, hash_diff_into_bitmap_at};
   ```

**Call site:** Call `reconcile_source_hashes()` before `tick_frame()` each render loop iteration.

---

### GAP 2 — Cache Priming never called at startup

**Files:** `src/runtime/renderer/manifest.rs:126–131`, `crates/albedo-server/src/renderer_runtime.rs` (~line 56)

**Problem:** `prime_runtime_cache()` pre-warms `static_slice_html_cache` and `normalized_props_cache` by pre-rendering all manifest routes. It's never called — every deploy starts cold.

**Fix:** In `renderer_runtime.rs::from_artifacts_dir()`, after `register_manifest_modules_with_precompiled()` succeeds, add:
```rust
let warm_requests: Vec<RouteRenderRequest> = manifest
    .routes
    .keys()
    .map(|entry| RouteRenderRequest {
        entry: entry.clone(),
        props_json: "{}".to_string(),
        module_order: Vec::new(),
        hydration_payload: None,
    })
    .collect();

if !warm_requests.is_empty() {
    if let Err(err) = renderer.prime_runtime_cache(&warm_requests) {
        tracing::warn!(target: "albedo.renderer", error = %err, "cache priming failed");
    }
}
```
Soft-fail (warn + continue) because a priming failure should not abort server startup — it degrades to cold-cache, same as today.

---

### GAP 3 — `GranularityController` + `ComponentAnalyzer` never used in production

**Files:** `src/analysis/adaptive.rs:59`, `src/analysis/analyzer.rs`, `src/lib.rs:163`

**Problem:** `GranularityController::should_parallelize()` decides whether to use rayon based on graph size and CPU count. `ComponentAnalyzer` is the single-threaded analysis path. Both are test-only; `lib.rs::optimize()` hardcodes `ParallelAnalyzer`.

**Fix in `src/lib.rs::optimize()` (line 163):**
```rust
use crate::analysis::adaptive::GranularityController;
use crate::analysis::analyzer::ComponentAnalyzer;

let controller = GranularityController::new();
let analyses = if controller.should_parallelize(
    self.graph.len(),
    std::mem::size_of::<ComponentAnalysis>(),
) {
    ParallelAnalyzer::new(&self.graph).analyze()?
} else {
    ComponentAnalyzer::new(&self.graph).analyze()?
};
```
Apply the same pattern wherever `ParallelAnalyzer` is hardcoded in `optimize_canonical_ir_columns()` and `optimize_incremental()`. Create `GranularityController` once per `optimize()` call (not per loop iteration — `System::new_all()` does I/O).

---

### GAP 4 — `parallel_column_pass` / `parallel_lane_column_pass` never called from pipeline

**Files:** `src/ir/columns.rs:677,733`, `src/runtime/pipeline.rs`

**Problem:** These rayon-parallelized column mutation passes exist for rehashing and effect recomputation, but no production code invokes them — column passes happen serially or not at all.

**Fix:** Add `pub fn run_column_analysis_pass(&mut self)` to `FourLaneRuntimePipeline`:
1. Create a `GranularityController` (store as pipeline field to avoid repeated `System::new_all()`)
2. If `controller.should_parallelize(self.columns.len(), std::mem::size_of::<u64>())`:
   - Call `self.columns.parallel_lane_column_pass(...)` with per-lane closures that recompute source hashes
3. Else: iterate column slices serially
4. Call `run_column_analysis_pass()` between "receive updated sources" and `reconcile_source_hashes()` in the render loop.

---

### GAP 5 — Hot Scheduler Registration API not exposed via pipeline

**Files:** `src/runtime/scheduler.rs:131–163`, `src/runtime/pipeline.rs`

**Problem:** `configure_hot_set()`, `register_hot_component()`, `deregister_hot_component()` exist on `OvertakeZoneScheduler` and work in unit tests, but `FourLaneRuntimePipeline` exposes none of them — the scheduler's dynamic hot-set management is inaccessible from outside.

**Fix:** Add pass-through methods to `FourLaneRuntimePipeline`:
```rust
pub fn reconfigure_hot_set(
    &mut self,
    entries: &[(ComponentId, RenderPriority)],
) -> Result<(), RuntimePipelineError> {
    Ok(self.scheduler.configure_hot_set(entries)?)
}

pub fn register_hot_component(
    &mut self,
    component_id: ComponentId,
    priority: RenderPriority,
) -> Result<(), RuntimePipelineError> {
    Ok(self.scheduler.register_hot_component(component_id, priority)?)
}

pub fn deregister_hot_component(&mut self, component_id: ComponentId) {
    self.scheduler.deregister_hot_component(component_id);
}
```
Call `reconfigure_hot_set()` after pipeline construction by mapping `component_tiers` → `RenderPriority` (Tier A → Low, Tier B → High, Tier C → Critical).

---

### GAP 6 — `cross_lane_deps_for_dependent` O(log N) lookup unused; O(N) loop used instead

**Files:** `src/runtime/highway.rs:188–222`, `src/runtime/pipeline.rs:97–129`

**Problem:** `cross_lane_deps_for_dependent()` binary-searches `cross_lane_dependencies` sorted by `dependent` — O(log N + k). The production dispatch loop (`dispatch_cross_lane_dependency_signals()`) iterates ALL edges on every call — O(N). For targeted dispatch when a specific component is known, the O(N) scan is wasteful.

**Fix:** Add `pub fn dispatch_cross_lane_signals_for(&self, dependent: ComponentId) -> usize` to `FourLaneRuntimePipeline`:
```rust
pub fn dispatch_cross_lane_signals_for(&self, dependent: ComponentId) -> usize {
    let edges = self.highway.cross_lane_deps_for_dependent(dependent);
    let mut routed = 0;
    for edge in edges {
        // same dispatch logic as existing loop body
        ...
        routed += 1;
    }
    routed
}
```
Keep the existing `dispatch_cross_lane_dependency_signals()` for full-graph broadcast; add the targeted variant for per-component update paths.

For `flattened_components()` (`highway.rs:35`): expose as a diagnostic/introspection API via `pipeline.lane_component_ids(lane: usize) -> Vec<ComponentId>`. Add a doc comment marking it as diagnostic-only — not for the hot path.

---

### GAP 7 — `lru` crate declared, `normalized_props_cache` uses HashMap with broken eviction

**Files:** `src/runtime/renderer/manifest.rs` (~line 22, 486–489), `Cargo.toml`

**Problem:** `lru = "0.12"` is in `Cargo.toml` with zero imports. `normalized_props_cache: HashMap<String, String>` in `ServerRenderer` has manual eviction that pops an **arbitrary** key (not LRU). The `lru` crate was clearly intended here.

**Fix:**
- Change `normalized_props_cache: HashMap<String, String>` → `normalized_props_cache: lru::LruCache<String, String>`
- Initialize: `lru::LruCache::new(NonZeroUsize::new(PROPS_CACHE_MAX_ENTRIES).unwrap_or(NonZeroUsize::MIN))`
- Replace `get` + `insert` + manual eviction with `lru_cache.get(key)` and `lru_cache.put(key, value)` — auto-evicts LRU
- Remove the manual eviction block
- Add `use lru::LruCache;` at top of `manifest.rs`

---

### GAP 8 — `core_affinity` declared, no thread pinning for lane workers

**Files:** `Cargo.toml`, `src/runtime/` (new file)

**Problem:** `core_affinity = "0.8"` is in `Cargo.toml` with zero imports. The 4-lane rayon workers operate on disjoint column slices — pinning each to a physical core would reduce cross-NUMA migrations and improve cache locality.

**Fix:** Add `src/runtime/affinity.rs`:
```rust
pub fn build_pinned_rayon_pool(num_threads: usize) -> Option<rayon::ThreadPool> {
    let core_ids = core_affinity::get_core_ids()?;
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .start_handler(move |worker_idx| {
            if let Some(core_id) = core_ids.get(worker_idx % core_ids.len()) {
                core_affinity::set_for_current(*core_id);
            }
        })
        .build()
        .ok()
}
```
- Add `pub mod affinity;` to `src/runtime/mod.rs`
- Store `Option<rayon::ThreadPool>` in `FourLaneRuntimePipeline`; if `Some(pool)`, call `pool.install(|| frame_tick(...))` in `tick_frame()`
- Falls back to global rayon pool if `None` (safe degradation)

> **Caveat on Windows:** `SetThreadAffinityMask` requires the correct core count for the target machine. Do not hardcode — let `core_affinity::get_core_ids()` discover it.

---

## Implementation Sequence

| # | Gap | Files | Dependency |
|---|-----|-------|------------|
| 1 | SIMD hash diff | `pipeline.rs`, `dirty_bitmap.rs` (no changes) | None — standalone |
| 2 | Cache priming | `renderer_runtime.rs` | None — standalone |
| 3 | `GranularityController` + `ComponentAnalyzer` | `lib.rs` | None — standalone |
| 4 | `parallel_lane_column_pass` | `pipeline.rs`, `columns.rs` (no changes) | After #3 (needs GranularityController) |
| 5 | Hot scheduler passthrough | `pipeline.rs`, `scheduler.rs` (no changes) | None — standalone |
| 6 | `cross_lane_deps_for_dependent` targeted dispatch | `pipeline.rs`, `highway.rs` (no changes) | None — standalone |
| 7 | `lru` cache for `normalized_props_cache` | `manifest.rs` | None — standalone |
| 8 | `core_affinity` thread pinning | `affinity.rs` (new), `pipeline.rs`, `runtime/mod.rs` | None — standalone |

---

## Verification

1. **SIMD hash diff:** Run `cargo test -p dom-render-compiler runtime::dirty_bitmap` — existing tests pass. Add a pipeline-level integration test that calls `reconcile_source_hashes()` and verifies `tick_frame()` produces the same dirty set as manual `mark_component_dirty()` calls.

2. **Cache priming:** Start the server, add a tracing span around `prime_runtime_cache`, verify warm cache hit rates on first requests via `frame_metrics().emit_summary()`.

3. **GranularityController:** Run `cargo test -p dom-render-compiler analysis` — confirm both branches exercise correctly (force a small graph to take serial path, large graph to take parallel path).

4. **Parallel column pass:** Add a test that calls `run_column_analysis_pass()` and then `reconcile_source_hashes()`, confirming dirty count matches expected mutations.

5. **Hot scheduler:** Existing `scheduler.rs` tests already verify the methods. Add a pipeline-level test calling `reconfigure_hot_set()` and then `run_scheduler_frame()` to confirm hot components reach the render queue.

6. **Cross-lane targeted dispatch:** Run `cargo test -p dom-render-compiler runtime::pipeline` — confirm routed count matches between `dispatch_cross_lane_dependency_signals()` and summed `dispatch_cross_lane_signals_for()` calls per dependent.

7. **LRU cache:** Run `cargo test -p dom-render-compiler runtime::renderer` — eviction behavior test should now show LRU ordering rather than arbitrary eviction.

8. **Thread pinning:** On the target server hardware, log thread CPU affinity IDs at pool startup and confirm each of the 4 workers is pinned to a distinct physical core.
