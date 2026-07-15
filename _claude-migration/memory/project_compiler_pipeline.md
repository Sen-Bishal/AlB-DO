---
name: project-compiler-pipeline
description: How JSX/TSX sources become RenderManifestV2 + bundle artifacts. Scanner → parser → graph → analysis → IR (SoA) → manifest/bundler/hydration.
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

The compiler pipeline lives entirely in the `dom-render-compiler` crate (root). Single facade: `RenderCompiler` in `src/lib.rs`.

**Why:** Understanding the order, *and which fields are deliberately denormalized*, is critical for proposing edits. Many fields cross multiple layers (e.g. `source_hash` is derived in `parser.rs`, stored in `Component`, copied into `CanonicalIrComponent`, hashed again in `IrColumns.source_hashes`, and SIMD-diffed in `dirty_bitmap.rs`).

**How to apply:** When the user asks "what does this pipeline phase do", trace from `lib.rs::optimize()` outward. When the user says "add field X", check whether the field is needed in the IR column store (hot path) or only in the AoS shell (JSON export).

## Stages

1. **Scan** — `scanner::ProjectScanner::scan_directory_with_mode(path, ScanMode::Lenient|Strict)` walks files via `walkdir`. Accepts `jsx|tsx|js|ts`. Lenient collects failures into `ScanReport.failures`; strict returns `CompilerError::AnalysisFailed` listing every failure.

2. **Parse** — `parser::ComponentParser::parse_source` runs SWC. Visits AST with `ComponentVisitor` (collects components — fn decl / var decl with arrow/fn / default export) and `EffectCollector` (call expressions). `is_hook_call` ⇒ identifier starts with `use` + uppercase third char. `is_async_call` / `is_io_call` / `is_side_effect_call` ⇒ allowlist of specific names (`fetch`, `axios.*`, `fs.*`, `console.*`, `localStorage.setItem`, etc.). `source_hash` = `xxh3_64(source bytes)` — must match `incremental::hash_file_bytes` and `runtime::engine::stable_source_hash`.

3. **Graph** — `scanner::ProjectScanner::build_compiler` constructs `ComponentGraph` (DashMap of components, name index, dependencies, dependents). `IdGenerator` mints sequential `ComponentId`s. Imports → dependency edges by name lookup. `WeightEstimator::estimate_priority_hints` sets `is_above_fold`/`is_lcp_candidate`/`is_interactive` by substring matching the component name (header/hero/banner/nav/image/featured/button/form/input/link).

4. **Analyze** — `RenderCompiler::optimize` is the entry. Calls `validate()` (cycle detection via DFS), constructs `GranularityController` ONCE per call (its `new()` does sysinfo I/O — `System::new_all()`), then `should_parallelize(graph.len(), sizeof::<ComponentAnalysis>())` → `ParallelAnalyzer` or `ComponentAnalyzer`. Analysis computes:
   - `priority = adjusted_bitrate / weight` (with `calculate_adjusted_bitrate` multipliers: above_fold ×5, interactive ×3, lcp ×10, weight>1000 ×0.5).
   - `estimated_time_ms = weight / adjusted_bitrate * 1000`.
   - `phase = sum over deps of 2π * (dep.weight / total_weight)` — angle in radians; used later for lane assignment via `phase_to_lane`.

5. **Topological sort** — `ParallelTopologicalSorter::sort_with_priority(analyses)` produces `Vec<Vec<ComponentId>>` levels. `create_batches` wraps into `RenderBatch`s with `can_defer = idx > 0`. `find_critical_path_parallel` walks longest path by estimated_time_ms.

6. **Tier decision** — `effects::decide_tier_and_hydration(profile, is_interactive, is_above_fold, weight_bytes, inputs)`. Order: side_effects → IO → async → hooks → weight-based fallthrough. Defaults: `tier_a_inline_max_bytes = 8 KB`, `tier_c_split_min_bytes = 40 KB`, `tier_b_mode = OnIdle`, `tier_c_mode = OnVisible`.

7. **IR — column store** — `ir::IrColumns::from_graph(graph, analyses)` builds the SoA store. Layout:
   - Hot numeric columns per component: `ids: Vec<u64>`, `source_hashes: Vec<u64>` (the SIMD-scanned column), `estimated_sizes: Vec<u32>`, `line_numbers: Vec<u32>`, `effects: Vec<u8>` (bit-packed via `effect_bits`), `export_kinds: Vec<u8>`, `priorities: Vec<f32>`, `phases: Vec<f32>`, `presence: Vec<u8>` (which of `legacy_priority`/`legacy_phase` are set).
   - Cold interned: `symbols`, `module_paths` (StringId → `StringInterner` storing `Vec<String>` + FxHashMap).
   - Edges: `edge_from: Vec<u32>`, `edge_to: Vec<u32>` — **column indices**, not raw ids. Sorted by lane in cycle 4 for split-borrow rayon scopes.
   - `id_to_index: FxHashMap<u64, u32>` for random lookup.
   - `lane_ids: Vec<u8>` + `lane_offsets: [u32; 5]` after `sort_by_lane(phase_to_lane)`. `LANE_COUNT == 4` (compile-time asserted equal to `runtime::highway::LANE_COUNT`).
   - `parallel_column_pass` / `parallel_lane_column_pass`: split-borrow `&mut [u8]` / `&mut [u64]` / `&mut [f32]` per group, no synchronization — rayon scope.
   - `to_canonical()` materializes the AoS `CanonicalIrDocument` shell. `from_canonical(&doc)` reconstructs columns. JSON export goes through the shell only.
   - **Schema version: "1.1"** — bumped from 1.0 when source_hash flipped from FNV-1a to xxh3_64 (cycle 2 of SoA refactor).

8. **Manifest** — `manifest::build_render_manifest_v2(graph, optimization_result, options)`. Per-component: derives tier+hydration, sorts deps, picks priority via `compute_priority` (critical-path index dominates). Multi-route: `entry_components_for_routes` finds root components (no dependents), maps file paths under `/routes/` to URL paths. Single-root fallback to `/`. Shell HTML, asset chunks, WT stream slots (only Tier B/C) all come from `ManifestBuilder`. `ManifestBuilder::new` builds a `StaticRenderProject` from `runtime::eval::ComponentProject` for static render — that's the cross-call into the evaluator.

9. **Bundler** — `bundler::build_bundle_plan(manifest, options)`:
   - `classify_component`: Entry (entry id) → Critical (in critical path or Tier A) → Critical (Tier B & !can_defer) → Deferred (rest).
   - `RewriteAction::WrapModule`: emits a stable wrapper at `__albedo__/wrappers/{fnv1a_hex}_{slug}.mjs` that re-exports `default ?? render ?? *`.
   - `RewriteAction::LinkVendorChunk`: maps `node_modules/{pkg}` paths to vendor chunks. `infer_package_name` supports scoped (`@scope/pkg`).
   - `plan_vendor_chunks`: infers shared chunks when ≥ 2 components reference the same package.
   - Emit artifacts via `bundler::emit::emit_bundle_artifacts_to_dir(plan, output_dir)`:
     - `bundle-plan.json`, `bundle-runtime-map.json`, `route-prefetch-manifest.json`, `static-slices.json`, `precompiled-runtime-modules.json` (QuickJS bytecode via `compile_module_script_for_quickjs`), wrapper module sources, vendor chunk module sources, `_albedo/wt-bootstrap.js`.

10. **Hydration** — `hydration::build_hydration_artifacts(manifest, entry)`. Builds `HydrationPlan` (entry-reachable islands, trigger per tier: B→Idle, C→Visible|Interaction), then `HydrationPayload` with FNV-1a-64 hex checksum, then `<script>` tags. Bootstrap script template is ≤ 2 KB inline, supports `requestIdleCallback`/IntersectionObserver/click+key+pointer triggers.

## Incremental compile
`RenderCompiler::with_cache(dir)` + `optimize_incremental(file_paths)`:
- `IncrementalCache::detect_changes` returns `ChangeSet { changed, new, deleted }`.
- `invalidate_changed_files` cascades through `dependency_graph` (DashMap).
- For invalidated + new, re-runs the chosen analyzer; reuses cached `ComponentAnalysis` for the rest.
- Persistence: bincode-encoded `.dom-compiler-cache.bin` (atomic rename).
- Hashes: `xxh3_64` everywhere — must NOT regress to `DefaultHasher` (not stable across Rust versions).

## Cross-module ID conventions (critical)
- `parser::hash_source` = `incremental::hash_file_bytes` = `runtime::engine::stable_source_hash` = xxh3_64 of UTF-8 source bytes.
- `eval::component::fnv1a_32` = albedo-server `render::tier_b::stable_id_for_placeholder` (must produce same bytes — anchor IDs cross WT boundary as u32).
- `runtime::compiled::allocate_slot_id` = `fnv1a_32("{module_spec}::{function_name}#{hook_idx}")`.
- `allocate_proxy_id` = `fnv1a_32("{module}::{fn}::{event}#{handler_idx}")`.
- `allocate_capture_slot_id` = `fnv1a_32("{module}::{fn}#prop:{name}")` (Phase K Stage 2).
- `manifest::builder::build_build_id` = fnv1a_64(`{path}:{hash};...` concatenation).
- `bundler::rewrite::stable_wrapper_module_path` = fnv1a_64 hex.
