---
name: project-a3-client-hydration
description: "A3 — Tier-C client hydration. The browser VDOM runtime (assets/albedo-client.js), its contract with the SSR h pragma, and the slice plan."
metadata: 
  node_type: memory
  type: project
  originSessionId: 21fe6951-812a-47d6-9d17-f6648dab1424
---

A3 (Gate 2 critical path, "make-or-break" per [[project-endgame]]): a Tier-C
component, server-rendered to HTML, rehydrates in the browser and runs
`useState`/`useEffect` locally with **zero network round-trip**.

## The hole A3 fills (verified 2026-06-13)
The hydration *scaffolding* already existed and is wired: `src/hydration/`
(plan→payload→a ≤2KB bootstrap in `script.rs`) injects island descriptors +
a bootstrap into the shell (`ServerRenderer` at `runtime/renderer/manifest.rs`
~218/242/429), and the bootstrap fires `globalThis.__ALBEDO_HYDRATE_ISLAND(island)`
on each island's trigger (idle/visible/interaction). But the ONLY definition of
that fn was the 14-line stub `assets/albedo-hydration.js` (sets an attribute) —
**no component JS ever reached the browser, no client runtime existed.**
`bakabox` (`assets/albedo-runtime.js`) is the Tier-B opcode VM ("no virtual DOM,
no reconciliation, only apply", server-authoritative) — NOT the Tier-C runtime.

## The contract (why one transpiled module runs on both sides)
The JSX transpile uses pragma `h` / `h.Fragment` (`quickjs_engine.rs:1326`).
Server `h` (`build_builtin_runtime_helpers_script`, quickjs_engine.rs:539) builds
HTML strings AND eagerly invokes function components. Client `h` is the mirror:
same `h(type, props, ...children)` signature, but builds a vnode and **defers**
component invocation until the reconciler installs a hook-state cell — that
deferral is what lets hooks run in the browser. Both install `globalThis.h` so
transpiled bare-`h` references resolve (client also installs `useState`/
`useEffect`/`Fragment`/`__ALBEDO_HYDRATE_ISLAND`).

## Slice 1 — DONE (2026-06-13, uncommitted)
`assets/albedo-client.js` — the Preact-compatible client runtime (~classic IIFE,
installs globals; ship target ~3KB min+gz). Didact-style instance tree:
`instantiate` (mount/create), `hydrateInstance` (adopt server DOM, no re-paint,
attach listeners, tag-mismatch → clean `mountReplace`), `reconcile` (diff
instance vs vnode: text patch / type-change replace / component re-render / host
prop+children diff). Hooks: positional `useState` (updater form, skips no-op
sets), `useEffect` (deps-gated, cleanup-then-effect, run after commit).
Microtask-batched scheduler (`enqueue`/`flush`, falls back to sync when no
queueMicrotask). Entry points: `hydrate(vnode, container)`, `hydrateIsland`
(root IS the component output node — the `data-albedo-island` marker),
`registerComponent`, and `__ALBEDO_HYDRATE_ISLAND(descriptor)` (registry lookup
by component_id|module_path, `data-albedo-island` querySelector, `data-albedo-
hydrated` idempotency guard). Known scoped gaps: index-based child keying (no
keyed reconcile yet); multi-child `Fragment` at a reconcilable boundary throws
loudly; `useRef`/`useMemo`/`useContext` deferred to Workstream B.

**Proof:** `tests/client_hydration.rs` (2 tests, green) — driven under rquickjs
with a compact DOM shim (the repo's JS-test discipline, cf.
`hydration_integration_tests.rs`). Counter: hydrate adopts the server `<button>`
(same node identity), click drives `useState`→in-place text patch (same node),
`useEffect` runs on mount + re-runs on dep change, `fetch` spy stays 0. Second
test drives the real bootstrap entry `__ALBEDO_HYDRATE_ISLAND` end-to-end.

## Slice 2 (A3.2) — DONE (2026-06-13, uncommitted)
Ship the component JS + initial props to the browser, both ends proven.
- **`compile_client_island_module(specifier, source, component_id)`** in
  `src/runtime/quickjs_engine.rs` — transpiles a Tier-C island with the same JSX
  pragma, lowers it to classic-JS statements, wraps in an IIFE that
  `globalThis.__albedoClient.registerComponent(id, default)`. Framework imports
  (react/react-dom/albedo) bind to globals; **non-framework imports (npm /
  child-module) are rejected LOUDLY** (the A2-leftover client vendor-chunk story,
  deferred — such islands degrade to static SSR).
- **Refactor (server output byte-identical):** extracted the module-item walk
  from `compile_exporting_module` into shared `lower_module_to_statements(spec,
  src, import_rewriter: fn)` returning `LoweredModule { statements,
  export_assignments, default_export_local }`. Server passes
  `rewrite_import_declaration`; client passes `rewrite_import_for_client`. (370
  lib tests still green, incl. ts_render/ts_action/npm_bundle.)
- **Props:** `HydrationIslandPayload.props: serde_json::Value` (`#[serde(default)]`;
  dropped `Eq` from the two payload structs). `build_hydration_payload(.., props_json)`
  seeds the **entry island** with the route props (nested islands → `{}`, same
  bound as A1). Threaded `props_json` through `build_hydration_artifacts` and all
  three `*_with_manifest_hydration` renderer callers.
- **Serving:** `albedo-client.js` served at `/_albedo/client.js`
  (`crates/albedo-server/src/handlers/albedo_assets.rs`). `ServerRenderer`'s new
  `build_client_island_head_tags(plan)` injects `<script src="/_albedo/client.js">`
  + one inline self-registering `<script>` per island, BEFORE the payload+bootstrap
  tags (document order ⇒ globals+registry ready before the bootstrap fires).
  Inline `</` neutralised via `escape_inline_script`. Non-compilable islands are
  skipped (graceful degradation, no page break).
- **Proof:** `tests/client_hydration.rs` +2 (real COUNTER_TSX → browser module →
  descriptor hydrate w/ seeded `{start:5}` → click 5→6, zero network; loud reject
  of a `zod` import). `tests/hydration_integration_tests.rs` +1 (Tier-C route head
  tags carry `/_albedo/client.js` + `registerComponent("50"`, payload seeds props).

## Slice 3 (A3.3) — DONE (2026-06-14, uncommitted)
SSR stamps `data-albedo-island="{component_id}"` on the entry island's root
element. **Gate A is now closed.**
- **`inject_island_marker(html, component_id)`** in `src/runtime/renderer/manifest.rs`
  — scans the HTML string past the tag name, walks attributes respecting quoted
  values, and inserts `data-albedo-island="{id}"` immediately before the first
  unquoted `>`. Attribute does not need to appear in the component's JSX — the
  reconciler's `updateDomProps` only touches keys in the component's vnode props,
  so the SSR-stamped attribute survives hydration and subsequent reconciles.
- **`entry_island_id(plan)`** — resolves the entry's component_id from the plan
  (`plan.islands` where `module_path == plan.entry`).
- All three `*_with_manifest_hydration` callers updated: `render_route_with_manifest_hydration`,
  `render_route_stream_with_manifest_hydration`, and
  `render_route_from_component_dir_with_manifest_hydration` — each stamps
  `result.html` + `result.shell_html` after rendering, before returning.
- **Proof:** `tests/hydration_integration_tests.rs::test_a3_3_ssr_stamps_island_marker_and_hydrates_end_to_end`
  (the ENDGAME A3 verification line, green). Full cycle under QuickJS+DOM shim:
  SSR renders `<button>count: 5</button>` → marker injected → `compile_client_island_module`
  builds island IIFE → bootstrap wires click listener (interaction trigger) →
  first click fires `__ALBEDO_HYDRATE_ISLAND` → `hydrateIsland` adopts server node,
  wires `onClick` → second click drives `useState(5) → 6` → text patches in-place →
  `data-albedo-hydrated="true"`, text="count: 6", same DOM node, zero network.
  Full workspace green (all suites, 0 failures).

**A3 is complete. Gate A is closed.**

After A3: **B** (useRef/useMemo/useContext + Head API), then **C** (honest
benchmarks), then **E** (flagship app). See [[project-endgame]],
[[project-a1-bridge]].
