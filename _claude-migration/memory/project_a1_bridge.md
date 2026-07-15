---
name: project_a1_bridge
description: A1 host-object bridge — handlers run under QuickJS; eval_handler + HandlerEffect; what server wiring remains
metadata: 
  node_type: memory
  type: project
  originSessionId: 7fbc3b41-1ba9-45c8-9644-10ff588a0210
---

**A1 host-object bridge, first slice — shipped 2026-06-08 (uncommitted; user owns commits).**

The genuinely-missing A1 work was *not* SSR (already runs through QuickJS via
`ServerRenderer<QuickJsEngine>` → `RendererRuntime`, the live `albedo serve`
path) and *not* Tier-A (stays on the pure-Rust `eval` module). It was running
**event handlers / server `action()` bodies** under QuickJS instead of the
pure-Rust `eval_handler_body` ([core.rs:977]), which models only a JS subset.

### What landed
- **`src/runtime/bridge.rs`** (new): `HandlerEffect` (SlotSet | Broadcast, each
  `.into_instruction()` → `Instruction::SlotSet`), `HandlerInvocation` (JS body
  source + seeded env map + setter→`SlotId` list + optional `event_json`), a
  script builder, and an envelope decoder. **Pure** — no `SlotStore`/
  `BroadcastRegistry` dependency.
- **`QuickJsEngine::eval_handler(entry, &HandlerInvocation) -> Vec<HandlerEffect>`**
  in `src/runtime/quickjs_engine.rs`. Runs under the same request-arena
  begin/run_gc/end discipline as `render_component`.
- Exports in `src/runtime/mod.rs`: `HandlerEffect`, `HandlerInvocation`.

### Design choice — effects collected in JS, not host FFI
The generated IIFE seeds `let`s for value bindings, defines `const setX =
v => __albedo_effects.push({kind:'slot',slot_id:N,value:v})` per setter, a
`broadcast(topic,value)` builtin, and `event`; runs the body; returns
`{ok,value,error}` where `value` is `JSON.stringify(__albedo_effects)`. No
`Function::new` closures / `Rc<RefCell>` across FFI. Source order preserved.
Loops/`try`/array methods all work now; a throw → loud `RenderError`. Binding/
setter names validated as JS identifiers (loud reject otherwise).

### Verified
8 new tests green (5 in `bridge`, 3 in `quickjs_engine::tests::eval_handler_*`),
incl. a `for`+`try` handler computing 116 + a broadcast. Full `cargo test --lib`
= **353 passed, 0 failed**. (Clippy still errors workspace-wide — the deferred
**Gate 1 D** sweep; my `.unwrap()` on `context` matches `render_component`'s
existing idiom, not a new violation.)

### Slice 2 — shipped 2026-06-08 (same session, uncommitted)
**`CompiledProject::invoke_action_quickjs` + `_with_broadcast`** in
`src/runtime/compiled.rs` — QuickJS-backed counterpart to `invoke_action`.
- AST→JS codegen: `handler_body_to_js` / `expr_to_js` / `emit_module_js`
  (swc `Emitter`) turn the stored `HandlerBody` AST back to source.
- `HandlerInvocation` gained `raw_bindings: &[(String,String)]` — engine-trusted
  JS expression seeds (used for unwritten `useState` initials, codegen'd).
- Env seeded from slot store (value + capture slots as JSON); setters from
  `component.setter_slots`; `event` from `envelope.payload` if valid JSON.
- Effects: `SlotSet` → `slots.write` (persist) + instruction; `Broadcast` →
  `registry.write_topic` (fan-out) + local instruction. After loop,
  `drain_pending()` discarded to keep dirty clean (matches `invoke_action`).
- **Parity proven**: `tests/ts_action_quickjs.rs` — counter increments 0→1, one
  `SlotSet`, clean drain; two clicks read persisted state (0→1→2).
  `cargo test --lib` = 354 green; related action/hook integration tests green.

### Slice 3 — server engine pool + adapter swap (opt-in, parity-proven) — 2026-06-09 (uncommitted)
The design-fork slice. **Decision (user-picked): explicit engine pool, warm-on-construction.**
- **`crates/albedo-server/src/engine_pool.rs`** (new): `QuickJsEnginePool` —
  bounded, each `QuickJsEngine` **pinned to its own dedicated OS thread**.
  Reconciliation for `!Send`+`&mut` under the multi-thread tokio runtime: a
  literal "move the engine to the caller" checkout is *unsound* (engine would
  migrate threads across an `.await`), so checkout **ships a closure**
  `with_engine(|&mut QuickJsEngine| -> R)` to the engine's thread over a
  `std::mpsc` channel and `.await`s the result on a `oneshot`. Only the closure
  + `R` cross threads (both `Send`); the engine never leaves its thread.
  Concurrency gated by `Semaphore(size)`; idle engines on a `std::Mutex<Vec>`
  stack (popped/pushed without holding across an await). `Drop` closes the
  semaphore, drops senders, joins threads. `EnginePoolError` (`ShuttingDown` /
  `WorkerLost`). Warm-on-construction: each worker `prewarm()`s **before**
  signalling ready, so no checkout hits a cold engine. 3 unit tests.
  *Open TODO in `warm_engine`:* the 8-render arena warmup (promote out of
  persistent mode) still needs a representative `HandlerInvocation`/render —
  `prewarm()` only inits builtins. Marked `TODO(a1-adapter-swap)`.
- **Adapter wired** (`server.rs`): `CompiledProjectActionAdapter` gained
  `engine_pool: Option<Arc<QuickJsEnginePool>>`; when `Some`, `handle()` ships
  `invoke_action_quickjs_with_broadcast` to a pooled engine. **Opt-in via builder
  `AlbedoServerBuilder::with_quickjs_action_engine_pool(size)`** — must be called
  *before* `register_compiled_project` (adapter captures the pool at
  registration). **Default stays `None` (pure-Rust) — zero behavior change.**
- **Parity proven end-to-end** through the real HTTP `/_albedo/action` route:
  `tests/compiled_project_dispatch.rs::compiled_counter_dispatch_via_quickjs_pool_matches_pure_rust_wire`
  — identical single `SlotSet`, increments 0→1→2 with session persistence.
  Full `cargo test -p albedo-server` green (114 lib + all integration).
- **Still to do before flipping on by default:** (a) the arena-warmup TODO
  above; (b) enable the pool in production boot (`boot.rs`) once gaps below are
  closed; (c) module-const seeding + updater-form broadcast (below) — these are
  the real reasons it's opt-in, not default (existing actions using either would
  break under QuickJS).

### Slice 4 — module-const seeding (QuickJS action path) — 2026-06-09 (uncommitted)
Closes correctness gap #2. `invoke_action_quickjs_inner` (`compiled.rs`) now
seeds module-level `const`s into the handler scope via the existing
`raw_bindings` (engine-trusted JS) mechanism, reading them from
`self.project.modules()[spec].module_constants` (`Vec<(String, Expr)>`) and
codegen'ing each with `expr_to_js`. **Parity with pure-Rust
`seed_env_with_module_constants`:** seeded *before* state/prop bindings (so
`useState(CONST)` initials + const→const refs resolve in source order);
component-owned names (value/capture/setter) **skipped** (the pure-Rust map would
overwrite, and two `let X` in the bridge IIFE = JS `SyntaxError` — so the
component's own `let` is the sole declaration); each init wrapped
`(function(){try{return (EXPR);}catch(__e){return null;}})()` to mirror pure-Rust's
`unwrap_or(Null)` leniency (a const that references an unresolved import → `null`
instead of throwing, so it never breaks a handler that doesn't use it).
**Proven:** new fixture `tests/fixtures/hook_compile/counter_const` (`const STEP=5`,
handler `setN(n+STEP)`) + test `ts_action_quickjs::quickjs_handler_resolves_module_level_constant`
(0→5). `cargo test --lib` = 354 green; action/broadcast/server-dispatch suites green.

### Slice 5 — updater-form broadcast (QuickJS action path) — 2026-06-09 (uncommitted)
Closes correctness gap #3 (last one blocking default-on). `broadcast(topic, fn)`
now works under QuickJS. **Design (keeps the bridge pure — no host FFI):** seed a
**pre-write snapshot** of topic values into JS; the `broadcast` builtin resolves
the updater *in JS*.
- `BroadcastRegistry::snapshot_values() -> Vec<(String, Vec<u8>)>` (new,
  `broadcast.rs`) — every registered topic's current bytes.
- `HandlerInvocation.broadcast_current: &Map<String, Value>` (new field,
  `bridge.rs`); `build_handler_script` seeds `const __albedo_topic_current={…}`
  and rewrites the `broadcast` builtin: if 2nd arg is `typeof==='function'`,
  read current (snapshot lookup, default `null`), apply updater, **write result
  back into the snapshot** so a *second* updater for the same topic in the same
  body chains (5→6→7) — matching pure-Rust read-modify-write. Value form
  unchanged.
- `invoke_action_quickjs_inner` (`compiled.rs`): populates `broadcast_current`
  from `registry.snapshot_values()` (decoding bytes→JSON, empty→Null), gated on
  broadcast wired **and** `body_src.contains("broadcast")` (non-broadcasting
  handlers pay nothing). Also now **registers the topic** (`registry.topic(…,
  "null")`, idempotent) before `write_topic` so an ad-hoc topic doesn't fail
  `UnknownTopic` — parity with pure-Rust's `topic()`-seed-before-write.
- **Proven:** `quickjs_engine::tests::eval_handler_resolves_updater_form_broadcast`
  (5→6→7 chain) + `ts_action_broadcast::quickjs_broadcast_updater_reads_current_value`
  (reuses `broadcast_demo` fixture's `increment_counter`; seeded 41 → 42, parity
  with the pure-Rust test). `cargo test --lib` = 356 green; action/server suites green.

### Slice 6 — arena warmup + QuickJS default-on in production — 2026-06-09 (uncommitted)
**The QuickJS action executor is now the production default.**
- **`warm_engine`** (`engine_pool.rs`) — TODO resolved. After `prewarm()`, runs
  `POOL_WARMUP_RENDERS` (10, = engine's 8 + margin) representative `eval_handler`
  calls on the fresh engine so its request-scoped arena promotes out of
  persistent mode before the first checkout. Warmup body is broad on purpose
  (loop + `try`/`catch` + array `.map` + setter call + updater-form `broadcast`)
  so QuickJS's lazily-built shape/atom tables for the common handler machinery
  land in the persistent region during construction, not on a live request.
  Evals are pure (effects discarded); soft-fail (a warmup error → colder engine,
  never aborts construction). Imports `HandlerInvocation`/`SlotId`/`Map`.
- **`boot.rs`** — `boot_production_server` now calls
  `.with_quickjs_action_engine_pool(available_parallelism)` **before**
  `register_compiled_project`, so every production action adapter routes through
  the warmed pool. Dev mode unchanged.
- **Proven:** new pool test `pool_engines_are_warmed_into_request_scoped_mode`
  (post-warmup `arena_stats().request_peak > 0` ⇒ scoped mode active;
  `request_used == 0` ⇒ region resets per render). Full `cargo test -p
  albedo-server` = green incl. `serve_boot_end_to_end` (boots production server +
  dispatches an action through the warmed pool). 115 lib + all integration green.

**A1 action-executor work is complete:** QuickJS runs all action bodies in
production, warmed, at full correctness parity (loops/`try`/array methods +
module consts + value/updater broadcast), with the design-fork engine pool.

### Slice 7 — SSR props → host-object exposure (RENDER side) — 2026-06-09 (uncommitted)
**The last A1 item. User chose "both / full parity."** Empirical finding that
reframed the gap: a real `import { useState } from "react"` TSX component **could
not even LOAD under QuickJS** — the import rewrote to `__albedo_require("react")`
which throws `MODULE_MISSING` at load (verified by a scratch probe). So the
QuickJS render only ever worked for self-contained/legacy `(props,require)=>`
modules; real hook components rendered only via the pure-Rust path (build-time
manifest bake / dev). Two render systems exist: pure-Rust `CompiledProject`
(`render_entry_with_bindings`, exposes props+slots+broadcast, emits hydration
opcodes — build/dev) vs `ServerRenderer<QuickJsEngine>::render_component`
(props-only, stateless — live `albedo serve` runtime via `render_route_with_overrides`).

What landed (all uncommitted):
- **Engine (`quickjs_engine.rs`):** global SSR hook shims in
  `build_builtin_runtime_helpers_script` — `useState` (positional via a
  per-render `__ALBEDO_HOOK_INDEX` counter, reads `__ALBEDO_HOST.state[idx]`,
  falls back to the call's initial; setter is a no-op on the server),
  `useSharedSlot` (reads `__ALBEDO_HOST.shared[topic]`), plus benign
  `useEffect`/`useLayoutEffect`/`useRef`/`useMemo`/`useCallback`/`action`/`broadcast`.
  **`react`/`react-dom`/`albedo` imports special-cased** in
  `rewrite_import_declaration` → `rewrite_framework_runtime_import`: bind names to
  `globalThis.<name>` instead of `__albedo_require` (this is what makes hook TSX
  load). The `h` builtin now **drops function-valued props** (so `onClick={fn}`
  doesn't stringify into the attribute). Render fn script takes a 3rd `hostJson`
  arg → installs `globalThis.__ALBEDO_HOST` + resets hook index in `try`, clears
  in `finally`. `render_component` refactored to `render_component_inner(.., host:
  Option<&str>)`.
- **Trait (`engine.rs`):** `RuntimeEngine::render_component_with_host` +
  `render_component_stream_with_host`, **default impls ignore the seed** (additive
  for every engine); QuickJsEngine overrides them. Lets the generic
  `ServerRenderer<E>` call the host-aware path.
- **`ComponentProject` (`eval/core.rs`):** now **retains raw module sources**
  (`sources: HashMap`, kept in lock-step in `load_from_dir`+`patch`) +
  `module_source(spec)` + `resolve_entry_component(entry) -> (module_spec, fn)`.
- **`CompiledProject::render_entry_quickjs` (+ `_with_broadcast`, `_inner`)** —
  symmetric to `invoke_action_quickjs`. Loads the entry's source into the engine
  (idempotent by hash), builds the host seed (`state` keyed by `hook_idx` from
  value slots; `shared` from broadcast topic current values), renders host-aware,
  and **snapshots captured props into their slots** (mirrors pure-Rust
  `snapshot_captured_props_into_slots`) so a follow-up `invoke_action_quickjs`
  reads the props this render observed. Returns HTML only — hydration binding
  opcodes still come from the pure-Rust emitter (this is the SSR HTML payload,
  not an opcode-emitter replacement).
- **`ServerRenderer` wiring:** `RouteRenderRequest.host_json: Option<String>`
  (+ `Default` derive); `render_route_with_overrides` calls
  `render_component_stream_with_host` when present and **bypasses the
  static-slice cache** for seeded (per-session) renders.
- **Proven:** `quickjs_engine::tests` (+5: react+useState loads/renders, seed
  overrides initial, positional alignment, useSharedSlot reads seed, full hook
  surface doesn't crash); `tests/ts_render_quickjs.rs` (new, 4: initial render,
  seeded slot drives render, `Array.map` render body works, captured-prop
  snapshot persists for the action path — isolated via a throwaway discovery
  store); `tests/visual_runtime_tests.rs` (+1 e2e: host seed flows through
  `ServerRenderer.render_route`). Full workspace green (`cargo test --lib`=361,
  `-p albedo-server`=all, 39 suites, 0 failures).
- **Known bounded scope (honest):** (a) host seed covers the **entry component's**
  hooks; nested/looped child components' `useState` fall back to initials (the
  global hook counter assigns children indices after the entry's, which aren't
  seeded — safe, not seeded). (b) Child-component **ES imports** (`import Foo from
  "./Foo"`) still fail to load under QuickJS (the `__albedo_require`-at-load
  limitation; only framework imports are special-cased) — a separate
  module-loader concern, not host-object exposure. (c) `render_entry_quickjs`
  returns HTML only (no binding opcodes). (d) ServerRenderer host seeding is
  **wired but not yet consumed by a production GET caller** — the server doesn't
  yet gather per-session slot/broadcast state into the route render; that
  consumer is the follow-up.

**A1 host-object bridge is complete** (handlers + renders both run under QuickJS
with host objects exposed). Touched this slice: `src/runtime/{quickjs_engine.rs,
engine.rs,compiled.rs}`, `src/runtime/eval/core.rs`, `src/runtime/renderer/
{core.rs,manifest.rs}`, `crates/albedo-server/src/renderer_runtime.rs`,
`tests/{ts_render_quickjs.rs(new),visual_runtime_tests.rs,hydration_integration_tests.rs}`,
`tests/fixtures/render_quickjs/list/Component.tsx (new)`.

### Remaining (next)
1.–4. ✅ (slices 3–6, action side). 5. ✅ SSR props → host-object exposure (slice 7).
- Render-side follow-ups (not A1-blocking): production GET caller that builds the
  per-session host seed; nested-component hook seeding; child-component ES-import
  loading under QuickJS.
- Cross-cutting still open: loud-errors **dev-overlay** line (surface QuickJS
  runtime exceptions through `crates/albedo-server/src/dev/error_overlay.rs`);
  the arena **residual hazard** (a lazily-initialised feature first used after
  warmup) — hardened partly by `warm_engine`'s broad body, fully by a soak/fuzz
  pass [Gate 1 D]; `catch_unwind` + CI + clippy sweep [Gate 1 D].

See [[project_quickjs_arena]] (the arena `eval_handler` reuses) and
[[project_endgame]] (A1 in the gate plan).
