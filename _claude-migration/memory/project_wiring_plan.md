---
name: project-wiring-plan
description: HISTORICAL — the 8-gap wiring plan that bridged the pre-Phase-L runtime. Most gaps closed during Phase L/M; preserved for context on why certain primitives exist where they do.
metadata: 
  node_type: memory
  type: project
  status: historical
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

> **HISTORICAL NOTE (2026-05-24):** This document describes the 8-gap plan that was active when Phase L started. Phase L (DETONATION) and Phase M (FALLOUT — DX) both landed afterward and rendered most of these gaps moot. See [[project-phase-l]] and [[project-phase-m]] for the current state. The plan below is preserved verbatim for context on why some primitives (SIMD hash diff, `GranularityController`, `core_affinity` pinning) exist where they do in the runtime.

`Implemenatation-plan.md` at the project root is the active plan. The user's framing: "ALBEDO's core infrastructure (4-lane highway, WebTransport, frame tick, dirty bitmap, incremental cache, offset globals) is fully wired and production-ready. However, 7 distinct gaps exist where features were built and tested but never connected to the live execution path." (Header says 7, document lists 8 — Gap 8 is `core_affinity` pinning.)

**Why:** The user is moving into active development on these gaps next. Each gap is a connector between an already-implemented primitive and a call site that should be using it.

**How to apply:** When the user says "let's start on Gap N" or names any of the file pairs below, this is the spec. The implementation has already been validated by the user. Don't re-discuss design; implement the named change. Verify primitives still exist before code-gen (file rename / cycle 6 may have moved them).

## What is *already* wired (don't touch)
- 4-lane highway + cross-lane routing (`highway.rs`, `pipeline.rs`)
- WebTransport end-to-end: muxer → stream router → QUIC server (`webtransport.rs`, `frame.rs`, `crates/albedo-server/src/webtransport.rs`)
- Frame tick Cycle 2–5: drain → partition → emit → sequence
- Dirty bitmap mark/drain path (`dirty_bitmap.rs`)
- Incremental cache: hash, invalidate, persist, hit-rate tracking (`incremental.rs`)
- Lane offsets binary-search in `frame_tick` hot path

## Gap 1 — SIMD Hash Diff Kernel never called from pipeline
**Files:** `src/runtime/dirty_bitmap.rs:183–264` (primitive — unchanged), `src/runtime/pipeline.rs:38–82` (call site).

`hash_diff_into_bitmap` + `_at` are documented SIMD primitives. `pipeline.rs::mark_component_dirty` marks one bit at a time per component.

**Fix:** Add `prev_source_hashes: Vec<u64>` field. New methods `reconcile_source_hashes()` and `reconcile_lane_source_hashes(lane)`. Imports at top of pipeline.rs.

**State (already partial):** `prev_source_hashes` field exists in `pipeline.rs` (cycle 2 baseline). `hash_diff_into_bitmap`/`_at` imports exist at the top of pipeline.rs. Verify whether `reconcile_source_hashes()` is implemented in current source before writing it.

## Gap 2 — Cache Priming never called at startup
**Files:** `src/runtime/renderer/manifest.rs:126–131` (primitive `prime_runtime_cache`), `crates/albedo-server/src/renderer_runtime.rs` (~line 56 — call site).

**Fix:** After `register_manifest_modules_with_precompiled` in `renderer_runtime.rs::from_artifacts_dir`, build a `Vec<RouteRenderRequest>` from `manifest.routes.keys()` and call `renderer.prime_runtime_cache(&warm_requests)` with `tracing::warn!` soft-fail.

**State:** Already landed — `renderer_runtime.rs` already contains the exact warm_requests block (verified). This gap may be already-closed; check `git log` before re-implementing.

## Gap 3 — `GranularityController` + `ComponentAnalyzer` never used in production
**Files:** `src/analysis/adaptive.rs:59` (controller), `src/analysis/analyzer.rs` (serial), `src/lib.rs:163` (call site).

**Fix:** In `optimize()`, `optimize_canonical_ir_columns()`, `optimize_incremental()`: pick between `ParallelAnalyzer` and `ComponentAnalyzer` via `GranularityController::should_parallelize`. Create the controller ONCE per call (`new()` does sysinfo I/O).

**State:** Partially landed in `lib.rs::run_analysis_with(controller)` — used by `optimize` and `optimize_canonical_ir_columns`. Verify that `optimize_incremental` also routes through `run_analysis_with` before changing anything.

## Gap 4 — `parallel_column_pass` / `parallel_lane_column_pass` never called from pipeline
**Files:** `src/ir/columns.rs:677,733` (primitives), `src/runtime/pipeline.rs` (call site).

**Fix:** Add `run_column_analysis_pass()` to `FourLaneRuntimePipeline`. Store the granularity controller as a field (avoid repeated `System::new_all()`). If parallelize, call `self.columns.parallel_lane_column_pass(...)` with per-lane closures that recompute source hashes; else iterate serially. Call before `reconcile_source_hashes()` each tick.

**State:** `granularity` field already exists in the pipeline; the `run_column_analysis_pass` method may not yet.

## Gap 5 — Hot Scheduler Registration API not exposed via pipeline
**Files:** `src/runtime/scheduler.rs:131–163` (primitives), `src/runtime/pipeline.rs` (call site).

**Fix:** Add `reconfigure_hot_set(entries)`, `register_hot_component(id, priority)`, `deregister_hot_component(id)` pass-through methods. Call `reconfigure_hot_set` after pipeline construction by mapping `component_tiers`: Tier A → Low, Tier B → High, Tier C → Critical.

## Gap 6 — `cross_lane_deps_for_dependent` O(log N) unused; O(N) used instead
**Files:** `src/runtime/highway.rs:188–222` (primitive), `src/runtime/pipeline.rs:97–129` (call site).

**Fix:** Add `dispatch_cross_lane_signals_for(dependent) -> usize` using the O(log N) `cross_lane_deps_for_dependent`. Keep existing `dispatch_cross_lane_dependency_signals()` for full-graph broadcast. Add `lane_component_ids(lane)` diagnostic API.

## Gap 7 — `lru` declared, `normalized_props_cache` uses HashMap with broken eviction
**Files:** `src/runtime/renderer/manifest.rs` (~line 22, 486–489), `Cargo.toml`.

**Fix:** Convert `normalized_props_cache: HashMap<String, String>` → `LruCache<String, String>` with `NonZeroUsize::new(PROPS_CACHE_MAX_ENTRIES).unwrap_or(NonZeroUsize::MIN)`. Use `get`/`put` (auto-evict LRU). Drop the manual eviction block.

**State:** ALREADY LANDED. Current `manifest.rs` has `use lru::LruCache;` and `normalized_props_cache: LruCache<String, String>` initialized with `NonZeroUsize`. Verify before re-implementing.

## Gap 8 — `core_affinity` declared, no thread pinning
**Files:** `Cargo.toml`, `src/runtime/affinity.rs` (new file).

**Fix:** Create `src/runtime/affinity.rs` with `build_pinned_rayon_pool(num_threads) -> Option<rayon::ThreadPool>`. Round-robin `core_affinity::get_core_ids()`. Add `pub mod affinity;` to `runtime/mod.rs`. Store `Option<rayon::ThreadPool>` in `FourLaneRuntimePipeline`; if `Some(pool)`, call `pool.install(|| frame_tick(...))` in `tick_frame()`. Falls back to global pool if `None`.

**State:** ALREADY LANDED. `src/runtime/affinity.rs` exists with the exact implementation; `runtime/mod.rs` exposes `pub mod affinity`. Pipeline's `pinned_pool: Option<rayon::ThreadPool>` field is initialized via `build_pinned_rayon_pool(LANE_COUNT)`. Verify `pool.install` is actually used in the hot path before closing this gap.

## Implementation sequence (per the doc)
1, 2, 3, 5, 6, 7, 8 — standalone. Gap 4 depends on Gap 3 (needs GranularityController in pipeline).

## Verification per gap
1. SIMD hash diff: pipeline-level integration test ensuring `reconcile_source_hashes()` matches manual `mark_component_dirty()` calls.
2. Cache priming: start server with tracing span around `prime_runtime_cache`; verify warm cache hit rates via `frame_metrics().emit_summary()`.
3. GranularityController: force small + large graphs.
4. Parallel column pass: dirty count after `run_column_analysis_pass()` + `reconcile_source_hashes()` matches expected mutations.
5. Hot scheduler: pipeline-level test calling `reconfigure_hot_set()` then `run_scheduler_frame()`.
6. Cross-lane targeted dispatch: routed count matches sum of `dispatch_cross_lane_signals_for()` calls.
7. LRU cache: eviction-order test (LRU vs arbitrary).
8. Thread pinning: log thread CPU affinity at pool startup on target hardware; confirm 4 workers on 4 distinct cores.

## Pre-action verification protocol
Several gaps are partially or fully landed already. Before writing code for any gap:
1. Open the named primitive file → confirm the function/struct still exists with the named API.
2. Open the named call site file → confirm the wiring isn't already there (grep for the function name).
3. Only then implement.

This is non-optional. The plan document is a snapshot that the user has been editing against; some closures already shipped on `nuclearshiz` per the recent commits.
