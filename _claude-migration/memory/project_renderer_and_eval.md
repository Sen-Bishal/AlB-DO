---
name: project-renderer-and-eval
description: "The SSR side — ServerRenderer, ComponentProject (AST evaluator), CompiledProject (Phase K), QuickJsEngine, SlotStore, opcode-emitting render path."
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

There are TWO render paths in the codebase, layered:

1. **AST-eval path** — `runtime::eval::ComponentProject` walks the SWC AST directly and emits HTML strings. Used by `manifest::builder::ManifestBuilder::build_route_manifest` for static tier-A render at build time, and by `albedo dev` for fast HMR rendering. Also drives Phase K's hook-compile + opcode emission via thread-local state.
2. **QuickJS path** — `runtime::quickjs_engine::QuickJsEngine` (impl `RuntimeEngine`) compiles SWC→JS via `swc_ecma_transforms_react::jsx` + `strip_type` then evaluates inside `rquickjs`. Used by `albedo-server::renderer_runtime::RendererRuntime`'s `ServerRenderer<QuickJsEngine>` for production SSR with precompiled bytecode artifacts.

The two paths share: `stable_source_hash` (xxh3_64), the bootstrap shim, and the manifest contract.

## ComponentProject (`runtime/eval/core.rs` — 88 KB)
The AST evaluator. `ComponentProject::load_from_dir(root)` scans the dir, parses each `.tsx|.jsx|.ts|.js` via `runtime/eval/expr.rs::parse_module` → `ParsedModule { imports, functions, default_export, module_constants }`.

`render_entry(entry, props_json)` evaluates the entry component's body, walking JSX and expressions through a giant match in `core.rs`. Supports:
- JSX elements/fragments with attributes, children, conditional rendering, `Array.map()`.
- Expression matrix: literals, template literals, ternary/binary/unary, `const` bindings, `new Date()`, computed values.
- `classnames`/`clsx` native implementations (no npm).
- Object/array literals, string prototype methods.
- HTML-escapes text by default; trusts attribute paths via `escape_attr`.

`render_local` is wrapped in `render_observer::enter_frame_guard(name, module_spec)` so the RAII guard fires `RenderObserver::on_render(RenderInfo)` on drop.

`PatchReport` returned by `patch(changed_paths, deleted_paths)`: incremental re-parse on HMR; tracks `reparsed_specifiers`.

Element IDs: thread-local `RENDER_ELEMENT_COUNTER` reset per render; combined with `module_spec` to FNV-1a-32 `data-albedo-id` stamps. Must match server-side `stable_id_for_placeholder` in `render::tier_b`.

## Phase K — CompiledProject (`runtime/compiled.rs`)
Wraps `ComponentProject` to add compile-time metadata extraction:
- `transforms::hooks::extract_use_state_hooks(function, imports)` — surfaces `useState` calls in source order. Rejects useState inside conditionals (Rules of Hooks + slot-allocation correctness). Returns `Vec<HookBinding>`.
- `transforms::events::extract_handlers_in_function(stmts)` — surfaces JSX `on*` attributes whose value is an inline arrow/fn. Returns `Vec<HandlerExtract>`.
- `transforms::events::collect_free_idents_in_handler_body(body)` — Stage 2 free-variable collector with a scope stack (binds params + var decls; `obj.prop` only contributes `obj`).

For each component: allocates `value_slots`, `setter_slots`, `proxy_ids`, `capture_slots` (Stage 2 — captured props that handlers actually reference). All ids are FNV-1a-32 of stable string keys (see [[project-compiler-pipeline]] for the exact templates).

`render_entry_with_bindings(compiled, entry, props, slots, opts)`:
- `opts.hook_compile = false` → falls back to Phase J HTML-only render.
- `opts.hook_compile = true` → calls `project.render_entry_compiled(entry, props, compiled, slots)` which sets up the thread-local `RENDER_K` state in `core.rs`. The renderer then:
  - Pushes a `ComponentScope` per render with slot/proxy maps.
  - Emits `BindEvent { stable_id, event_id, proxy_id }` per JSX on* handler.
  - Emits `SetTextRef { stable_id, slot_id }` when a slot-read appears in text position.
  - Maintains a render-scoped event intern table (lazy alloc starting at id 1); `drain_phase_k_opcodes` prepends `InitInternTable { kind: Event, entries }` so bakabox can resolve `event_id`.
  - Returns `(html, opcodes: Vec<Instruction>)`.

`invoke_action(envelope, slots)`:
- Looks up `ResolvedHandler` by `action_id` (= `proxy_id`).
- Calls `project.eval_handler_body(module_spec, body, component, slots)` — re-executes the handler AST against the SessionSlotView, binding setters to slot writes.
- Appends `slots.drain_pending()` (any `SlotSet`s the handler triggered).

Phase K Stages (per `compiled.rs` and `transforms/`):
- Stage 1 — `useState` + inline arrow on* handlers, immediate setter calls.
- Stage 2 — closures over component props (captured via `capture_slots`).
- Stage 3 — closures over module-level constants (`ParsedModule.module_constants` is populated by `parse_module`).

Per `git log`: commits `1898b5a phase-k: Stage 3`, `4ebf3b5 phase-k: Stage 2`, `a26d6e3 phase-k: bridge CompiledProject into albedo-server's ActionRegistry`, `d471a8f phase-k: CompiledProject runtime + hook-compile render path`, `ce3b908 phase-k: AST extractors for useState + JSX on* handlers`. All five commits landed on the current branch `nuclearshiz`. Phase K is the active development focus.

## SlotStore + SessionSlotView (`runtime/slot_store.rs`)
- `SlotStore` is the per-server state container shared via `Arc`. `DashMap<(SessionId, SlotId), Vec<u8>>` for values, `Mutex<FxHashSet>` for dirty keys.
- `write(session, slot_id, value)` flips the slot dirty.
- `drain_set_instructions(session)` consumes dirty entries → `Vec<Instruction::SlotSet>`. Coalesces double-writes to the same slot.
- `clear_session(session)` — used when the WT connection closes.
- `SessionSlotView` is the per-session wrapper used by Phase K's invoke path AND by `albedo-server::actions::SessionSlots` (the two are structurally identical so a single substrate spans wire ↔ compile-time evaluator).

## ServerRenderer (`runtime/renderer/manifest.rs`)
Generic over `E: RuntimeEngine`. Owns:
- the engine,
- `ModuleRegistry` (BTreeMap of specifier → `RegisteredModule { code, source_hash, precompiled_script, dependencies, head_tags }`),
- `loaded_module_hashes` (track what the engine already has loaded, avoid re-load),
- `normalized_props_cache: LruCache<String, String>` (256 entries — was a buggy HashMap before Gap 7 fix landed),
- `static_slice_modules`, `static_slice_html_cache: HashMap<StaticSliceCacheKey, String>` (entry+props_hash+source_fingerprint+invalidation_version),
- `route_invalidation_versions`, `tag_invalidation_versions`, `route_tags`, `invalidation_version_clock`.

Key methods:
- `register_manifest_modules_with_precompiled(manifest, sources, precompiled_modules)` — auto-tags each module with `component:{id}`, `tier:{:?}`, `hydration:{:?}` route tags; builds the static-slice manifest.
- `prime_runtime_cache(requests)` — warm renders every route at startup.
- `render_route(request)` / `render_route_stream(request)` / `*_with_overrides(request, hydration, head_tags)` / `*_with_manifest_hydration`.
- `revalidate_path(path)`, `revalidate_tag(tag)` — cache invalidation by route or tag (bumps a version counter, evicts matching cache keys).

## ModuleRegistry (`runtime/renderer/core.rs`)
- `register_from_manifest_with_precompiled` builds the registry from manifest components + sources; binds precompiled bytecode by source_hash equality.
- `resolve_module_order(entry, requested)` returns either the requested list (validated) or a topological order from `entry`.
- `visit_module` DFS; emits `LoadErrorKind::DependencyCycle` for cycles, `ModuleMissing` for unknowns.

## QuickJsEngine (`runtime/quickjs_engine.rs`)
- Holds an `rquickjs::Runtime` + `Context`, one per server.
- `MAX_MODULE_SIZE = 10 MB`.
- `ensure_initialized` installs:
  - `build_builtin_runtime_helpers_script()` (always),
  - optional DOM shim from `BootstrapPayload.dom_shim_js`,
  - optional runtime helpers from `bootstrap.runtime_helpers_js`,
  - `globalThis.__ALBEDO_MODULES = Object.create(null)`,
  - `build_render_function_script()` — the reusable render entry.
- Module table sentinels: `__albedo_is_module_record`, `__ALBEDO_MODULE_MISSING__:`, `__ALBEDO_INVALID_ENTRY_EXPORT__:`.
- `compile_module_script_for_quickjs(path, source)` — pre-build bytecode for `precompiled-runtime-modules.json`.
- Uses SWC: `swc_ecma_transforms_react::jsx`, `strip_type`, `resolver`, `Mark::new`. Whole transformation chain happens in Rust before handing JS to QuickJS.
