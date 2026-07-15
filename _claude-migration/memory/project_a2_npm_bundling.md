---
name: project_a2_npm_bundling
description: "A2 npm dep-bundling — DONE for SSR+actions (2026-06-11); architecture, key decisions, proven against real zod/date-fns, remaining A3-side items"
metadata: 
  node_type: memory
  type: project
  originSessionId: 61864a17-702a-4158-8bb6-41cbd9627cc9
---

# A2 — npm dependency bundling (Gate 2), shipped 2026-06-11 (uncommitted)

**Status: COMMITTED** — `d88af36` "NPM bridge layer with lateral schema" (2026-06-12). `import { z } from "zod"` works end-to-end:
`CompiledProject::wrap` discovers it, bundles from `node_modules`, the QuickJS
render/action paths preload it, components render with it, action handlers
validate through it. Proven against **real `zod@4.4.3` + `date-fns@4.4.0`**.

## Architecture (the decision that shaped everything)

**No scope-hoisting bundler** (`swc_bundler` 0.234–0.238 matches the pinned SWC
0.37 generation and remains a fallback, but was rejected): the engine already
links modules through `globalThis.__ALBEDO_MODULES`, so npm packages lower to
**one lazy memoized factory per file** + an **alias per bare specifier**:

- `src/bundler/npm.rs` — Node-style resolver (`exports` maps w/ nested
  conditions, fixed priority import→module→default→require, single-`*`
  wildcards w/ longest-prefix; `module`/`main`/index fallbacks; extension/index
  probing; nearest-package.json `"type"` ESM/CJS classification; upward
  node_modules walk from the importing file → nested/transitive packages work)
  + graph walker (swc parse; `RequireCollector` visitor for CJS) + per-file
  artifact emission. `MAX_GRAPH_FILES=4096` cap. Loud `NpmBundleError` variants.
  Also `scan_bare_imports` (TSX-aware discovery) and `is_bare_npm_specifier`.
- `quickjs_engine.rs` — `build_npm_runtime_helpers_script()` installs
  `__ALBEDO_NPM_FACTORIES` / `__ALBEDO_NPM_ALIASES` / `__albedo_require_record`
  (memoized via `__ALBEDO_MODULES`, **record published BEFORE the factory runs**
  → CJS-grade cycle tolerance, **no topo-sort needed**; factory throw → record
  deleted + rethrow) + import-binding helpers `__albedo_import_default/
  _namespace/_named` (npm → real ESM semantics; project modules → legacy
  unwrap, byte-compatible) + a `process.env.NODE_ENV` shim.
  `compile_npm_module_script(key, source, NpmModuleFormat::{Esm,Cjs,Json},
  resolve_map)` lowers files: ESM gets re-export support (`export {x} from`,
  `export * from` w/ default-excluded guarded copy, `export * as ns from`),
  CJS gets `module/exports/require/__filename/__dirname/global` shims +
  default+copied-named interop (`__albedo_cjs` marker), JSON parses+re-serializes.
- Record keys: `npm:<pkg>@<version>/<relpath>` (collision-free across nested
  versions); alias artifact maps `"zod"` → entry key.
- `compiled.rs` — `CompiledProject.npm_bundles` built at wrap
  (`bundle_project_npm_dependencies`: scan retained sources, skip project-module
  specifiers, warn+skip on resolve failure → loud MODULE_MISSING at use);
  `preload_npm_bundles(engine)` called in `render_entry_quickjs_inner` AND
  `invoke_action_quickjs_inner` (hash-memoized, lazy → cheap steady-state);
  handler scope seeds npm import bindings (sorted, try/null-wrapped,
  owned/seeded-name guards) **before** module consts so
  `const User = z.object(...)` resolves.

## Bonus fixes landed with it

1. **`__albedo_require` was render-function-local but module records call it at
   LOAD time** → promoted to a global. This closed the old A1 gap: project
   child-component imports now link under QuickJS
   (`tests/npm_bundle.rs::project_child_component_import_now_links`).
2. `export class` support in the project-module record compiler (was loud-unsupported).
3. `ComponentProject::load_from_dir`/`patch` now skip `node_modules`
   (`path_is_in_node_modules`) — previously a project-root node_modules would
   be ingested wholesale as components.

## Honest semantics / limitations

- Cycle tolerance is **CJS-grade**: destructured cycle back-references snapshot
  `undefined`; call-time access via namespace import works (documented + tested).
- Condition resolution uses fixed priority, not Node's object-key-order
  (serde_json Map is sorted; avoided preserve_order). Deviation negligible for
  import-context conditions.
- Star-vs-star export collisions: first wins (ESM says ambiguous-drop); locals
  always beat stars (correct).
- `h()` does NOT escape text children (only attributes) — pre-existing engine
  behavior surfaced while testing, NOT introduced here. Potential SSR XSS
  surface worth a future look.
- Not yet wired: `ServerRenderer` manifest-path preload (older Tier-B/C SSR
  loop); client-side vendor chunks via `vendor.rs` (belongs to A3); dev-mode
  (`albedo dev` in albedo.rs) npm path untested.

## Tests

- `tests/npm_bundle.rs` (8, always-on synthetic): exports maps, re-export
  chains, class exports, CJS interop, JSON, cycles, subpaths, transitive
  packages, full CompiledProject render+action loop, child-component link fix.
- `tests/npm_real_packages.rs` (5): **skips loudly** when
  `target/npm-fixture/node_modules` absent (create:
  `cd target/npm-fixture && npm install --no-save zod date-fns`). zod schema in
  render, loud ZodError, date-fns root (~250-file graph) + subpath, zod action
  handler through CompiledProject.
- + unit tests in npm.rs (9). Full workspace green: lib 370 + all suites, zero
  failures. New code clippy-clean against crate denies (unwrap_used,
  indexing_slicing); repo-wide sweep remains Gate 1 D.

Related: [[project_a1_bridge]] (the QuickJS executor this builds on),
[[project_endgame]] (Gate 2 sequencing).
