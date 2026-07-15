---
name: project_dep_detection_gap
description: "ALBEDO dependency detection is name/JSX-based — components can't import data/util modules; the flagship's real blocker (2026-06-28)"
metadata: 
  node_type: memory
  type: project
  originSessionId: d16f51ce-b988-4283-a416-c1a1341e5b1c
---

## ✅ CLOSED 2026-06-28 (uncommitted) — A+B+C all landed, verified end-to-end on Halation

**Fixed.** A component can now import a non-component data/util module. Build of Halation shows `Issue deps=[EssayCard, __module__essays]` (was `[]`); served `/` renders the keyed essay list from `content/essays.ts` **stably across 6+ requests, HTTP 200, no segfault, clean serve log.** What landed:
- **A — `src/parser.rs`:** `ParsedComponent` gained `import_sources: Vec<String>` (the `from "..."` specifiers, captured in `visit_import_decl`) + `is_module_only: bool`. New export tracking (`visit_export_decl`/`visit_named_export`/default-export → `has_export`). `parse_source` synthesizes ONE module-only node (`__module__<stem>`) for a file that declared no component but exports something; zero-export files still produce nothing.
- **B — `src/scanner.rs` `build_compiler`:** new `resolve_import_to_id` resolves each `import_source` to a node by **path** (`import_candidates`/`normalize_specifier` relative to importer, drive-letter dropped consistently on both sides) against a `normalize_specifier(file_path)→id` map; adds the dep edge. Legacy name-match kept as a union fallback (idempotent, no regression).
- **C — node tolerance:** `types::Component` gained `#[serde(default)] is_module_only` (set from parsed). Module-only nodes ARE manifest components (so `register_from_manifest` registers their source by `module_path` on serve + the dep edge wires them) but are **skipped as renderable**: filtered out of `builder.rs sorted_children` (the one child-walk choke point → never static-rendered) and skipped as route roots in `manifest/mod.rs` (`entry_components_for_routes` + `entry_component_for_route`). Build-time `ComponentProject::load_from_dir` already had the data source (loads all files under root), so no build-path change needed.

**Tests:** 3 new parser unit tests + 1 scanner path-dep test; full lib suite **413 green**; npm_bundle/css/hydration/action/bundler/budget integration suites green; albedo-server checks clean. **Remaining:** rebuild+`cargo install --force` the RELEASE `albedo` (verified on a fresh DEBUG binary; PATH binary still the old alpha). Then P3 (`essays/[slug]` + serve-path `[slug]` param extraction). See [[project_halation_flagship]].

---

(original diagnosis, kept for context)

**ALBEDO can't import a non-component module (data/util/lib) from a component.** Found dogfooding Halation P2 (`A:\halation`), 2026-06-28. This is the real Gate-4 "port friction" finding — async server components were a **red herring** in the investigation.

**Root cause (exact lines):**
- `src/parser.rs` `visit_import_decl` (~L179-188) stores only the imported **binding names** in `ParsedComponent.imports: Vec<String>` (L75) — it **discards `import.src`** (the module path) entirely.
- `src/scanner.rs` `build_compiler` (L119-127) wires deps by matching each `parsed.imports` name against `component_map` (keyed by component **name**). So a dep edge is added only when the imported binding name equals a component's name (e.g. `import EssayCard` → component `EssayCard`). `import { getIssue }` / `import { essays }` → no matching component name → **no dependency edge**.
- Compounding: the graph nodes are **components only**. A pure data `.ts` (named exports, no JSX/default component) is parsed → produces no `ParsedComponent` → never a graph node → no source in the module registry, so it can't even be a dep target.

**Symptom:** a route importing a data module builds fine but, on `albedo serve`, the Tier-B render plan omits the data module (`Issue deps=[]` in `render-manifest.v2.json`). At request time `load_module(index)` runs `__albedo_require(dataKey)` → module never loaded → throws → and under the request-scoped QuickJS arena the repeated failure **segfaults** the worker (boot reaches "serving"; crash is at request). `<EssayCard/>` (JSX usage, name==component) works; `getIssue()` (call) does not.

**The proper fix (bounded, multi-part):** (A) parser captures import sources; (B) scanner resolves deps by **path** (`import_candidates` against a path→id map relative to the importer) not name; (C) include imported **non-component** modules as graph nodes + registry entries so data/util modules link. (C) is the architectural depth — it breaks the "every graph node is a renderable component" invariant.

**Stopgaps if not fixing the engine:** inline the data into each route (self-contained async works — the `stats.tsx`/probe-D shape), or keep data in a module that is *also* a component **and** referenced by a JSX-matching name (fragile).

Separately fixed this session: the QuickJS **import-linking specifier mismatch** (relative imports looked up by raw string vs absolute `module_path` key) — `__albedo_resolve_project` resolver + `rewrite_import_declaration` change in `src/runtime/quickjs_engine.rs`. Necessary regardless; verified (child-component imports render in async Tier-B; 437 tests green). See [[project_async_server_components_gap]], [[project_quickjs_arena]], [[project_halation_flagship]].

## Agreed fix (user chose "fix it properly" 2026-06-28) — A + B + C

- **A. Parser captures import paths** (`src/parser.rs`). `ComponentVisitor::visit_import_decl` (~L179-188) pushes only binding names into `current_imports`; it ignores `import.src.value`. Capture the source path too — add e.g. `pub import_sources: Vec<String>` to `ParsedComponent` (L71-76), populated from each `import.src.value` (relative ones at least). Keep `imports` (names) for existing consumers.
- **B. Scanner resolves deps by PATH** (`src/scanner.rs` `build_compiler`, L119-127). Build a `normalized-module-path → ComponentId` map; for each component, resolve each import source relative to the importer's `file_path` (parent.join(source) + `import_candidates` extension probing from `runtime::eval::component`, `normalize_specifier` for the key) → look up → `add_dependency`. Replaces the name-keyed `component_map.get(import)` match. (Keep name-based as a fallback only if a regression appears.)
- **C. Non-component modules become graph + registry nodes** (the architectural part — breaks "every graph node is a renderable component"). A pure data `.ts` (named exports, no JSX/default) currently yields **zero** `ParsedComponent` from `parser.parse_file` (`scanner.rs scan_directory_with_mode` L64-65 just appends comps), so it never reaches the graph or the module registry. Need: (1) parser/scanner to surface a **module-only node** (carry name + `file_path`/module_path + source; flag it non-renderable); (2) `src/graph.rs`/`types` `Component` to tolerate a node with no render; (3) `src/manifest/builder.rs` to include module-only nodes in the manifest `components` (so `module_path` + source serialize) **without** routing/tiering/rendering them; (4) `src/runtime/renderer/core.rs build_module_registry_from_manifest` then registers their source by `module_path` automatically. Tiering + static render passes must **skip** module-only nodes (never an entry/route).

**Verify (next session):** parser unit (import sources captured) + scanner unit (path-based dep edge for a named import) → build Halation, assert `Issue deps=[<essays id>]` in `.albedo/dist/render-manifest.v2.json` → `albedo serve`, confirm `/` renders the full keyed list + `[slug]` hrefs and is **stable across requests** (no segfault) → regression: lib tests + npm_bundle/css/hydration/action suites (`-j2`).

## Tree state at break (2026-06-28, all uncommitted)
- **Engine:** `src/runtime/quickjs_engine.rs` carries ONLY the Bug-1 linking fix (`__albedo_resolve_project` in the prelude IIFE, `is_relative_specifier` + `resolve_project_specifier_base` helpers, and `rewrite_import_declaration` wrapping relative specifiers). Debug instrumentation + a wrong "job-drain" experiment were **reverted**; `crates/albedo-server/src/render/tier_b.rs` is back to original.
- **App** (`A:\halation`): intended P2 files in place but **blocked on bug 3** — won't render on serve yet. `content/essays.ts` (sync selectors `getIssue`/`getArchive`/`getEssay` + `Essay[]` incl. `body` for P3), `routes/index.tsx` (async, composes `getIssue` + keyed `EssayCard` map), `routes/archive.tsx` (async), `routes/about.tsx` (static), `routes/error.tsx` (P4 boundary, bonus), `components/EssayCard.tsx`. Probe files removed.
- **Binaries STALE:** `target/release/albedo.exe` was built WITH the reverted job-drain and BEFORE cleanup → **rebuild** (`cargo build --release -p albedo-server --bin albedo -j2`, ~12 min; taskkill albedo.exe first) before trusting a serve. PATH `albedo` older still.
- **Tests:** 437 green on the Bug-1 fix (one flaky `proc_bench::cold_start_aggregates_over_multiple_boots` — passes in isolation, timing/process-spawn under parallel load, unrelated).
