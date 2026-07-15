---
name: reference-workspace-layout
description: "Where every subsystem lives on disk. Top-level crates, src modules, binaries, and the dev/benchmarks/scaffold dirs."
metadata: 
  node_type: memory
  type: reference
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

**Project root**: `A:\AlBDO-v-0.1.0\` (moved from prior location — current working dir).

**Cargo workspace** (`Cargo.toml`): three members.
- `.` → `dom-render-compiler` (the core crate)
- `crates/albedo-node` → NAPI bindings for Node.js shell package
- `crates/albedo-server` → axum HTTP runtime

**Top-level dirs**:
- `src/` — `dom-render-compiler` core. See sub-mods below.
- `crates/` — workspace members.
- `assets/` — JS runtime/decoder + client scripts. Inventory: `albedo-runtime.js` (bakabox), `bincode.js` (decoder), `albedo-wt-bootstrap.js` (WT), `albedo-hydration.js`, `albedo-link-forms.js` (Phase L Link + form + Navigate interception), `albedo-error-overlay.js` (Phase M.1 floating overlay client), `albedo-hmr-apply.js` (Phase M.2 in-place DOM swap client). Phase N did not add a client asset (CSS-modules is build-time; public/ is direct file serving; ship is config emission). Phase O.1 added no asset.
- `benches/`, `benchmarks/` — criterion benches; `frame_tick` and `opcode_wire` declared in root `Cargo.toml`.
- `fuzz/` — `cargo fuzz` targets for wire decoders.
- `installer/`, `scaffold/`, `examples/`, `professionalization/` — distribution + starter templates.
- `tests/` — workspace-level integration tests. Phase L+ surface includes: `budget_integration.rs` (O.1 manifest-driven), `bundle_budget_integration.rs` (O.3 measured-bytes end-to-end with 142 KB demo), `shared_slot_golden.rs` (O.2 W3 renderer ↔ broadcast end-to-end), `broadcast_failure_modes.rs` (O.2 W3 — concurrency, slow-consumer prune, 1000×10 storm, 256 KB payloads). Fixtures live under `tests/fixtures/` (`hook_compile/`, `shared_slot/`, etc.).

**`src/` modules** (each is a `pub mod` from `src/lib.rs`):
- `src/lib.rs` — `RenderCompiler` facade. `optimize`, `optimize_manifest_v2`, `emit_bundle_artifacts_to_dir`, `optimize_incremental`.
- `src/types.rs` — `Component`, `ComponentId`, `ComponentAnalysis`, `RenderBatch`, `OptimizationResult`, `TierReport`, `CompilerError`.
- `src/effects.rs` — `EffectProfile`, `decide_tier_and_hydration`, `TieringDecision`.
- `src/parser.rs` — SWC JSX/TSX parsing, effect inference (hook calls, async, IO, side effects). `hash_source` uses `xxh3_64`.
- `src/scanner.rs` — `ProjectScanner`: walks dirs, calls parser, builds compiler+canonical IR. Lenient/strict mode.
- `src/graph.rs` — `ComponentGraph` (DashMap). Cycle detection. `calculate_out_degrees`.
- `src/estimator.rs` — `WeightEstimator`. Priority hints from name substrings (header/hero/nav/button/...).
- `src/incremental.rs` — `IncrementalCache` (DashMap + bincode persistence). `xxh3_64` file hashing.
- `src/analysis/` — `analyzer.rs` (serial), `parallel.rs` (rayon), `topological.rs`, `parallel_topo.rs`, `adaptive.rs` (`GranularityController`).
- `src/ir/` — `mod.rs` (CanonicalIrDocument shell), `columns.rs` (SoA `IrColumns` — runtime truth, 4-lane partitioned), `opcode.rs` (Instruction enum + InternTable), `wire.rs` (bincode locked config, WireEncode/WireDecode traits), `action.rs` (ActionEnvelope), `conformance.rs` (`canonical_v1_frame`, `LOCKED_WIRE_VERSION = 2`).
- `src/transforms/` — `hooks.rs` (Phase K useState extractor), `events.rs` (Phase K JSX on* handler extractor + free-ident collector), `form.rs` (Phase L `<form action="action:NAME">` extractor + field metadata + `FORM_ACTION_PREFIX` + `allocate_form_action_id` + `allocate_field_error_id`), `link.rs` (Phase L `<Link href>` extractor), `css_modules.rs` (Phase N `scope_module_css(module_id, css) -> ScopedCssModule { scoped_css, class_map, hash_suffix }` — hand-written CSS scanner respecting `{...}` bodies, `/* ... */` comments, quoted strings; `xxh3_64`-derived 8-hex suffix; `is_css_module_path(p)` for `*.module.css`), `shared_slots.rs` (Phase O.2 `extract_shared_slot_hooks(function, imports) -> Result<Vec<SharedSlotBinding>>` — recognises `const x = useSharedSlot("topic")` imported from `"albedo"`, rejects conditional placement / non-string-literal topic / non-identifier binding; handles TS wrappers).
- `src/routing/` — Phase N file-based route discovery. `file_based.rs::discover_routes(routes_dir) -> RouteDiscovery { routes, layouts }`. Translates `index.tsx → /`, `blog/[slug].tsx → /blog/[slug]` (router already normalises Next-style segments via `CompiledRouter::normalize_route_paths`), `layout.tsx` becomes `DiscoveredLayout` keyed by its directory prefix and stacked root→leaf into `DiscoveredRoute.layout_chain`. Skips `_*` files, hidden dirs, non-{tsx,jsx,ts,js} extensions; duplicates → `RouteDiscoveryError::DuplicateRoute`.
- `src/budget/` — Phase O.1 + O.3 tier budgets. `config.rs::load_budget_from_dir(project_dir) -> Result<Option<TierBudget>>` (reads `tier-budget.toml`; `Ok(None)` when absent). `report.rs::evaluate_budget(manifest, budget) -> BudgetReport` (Phase O.1 source-weight gate, pure function, deterministic). `report.rs::evaluate_bundle_budget(byte_report, budget) -> BudgetReport` (Phase O.3 measured-bytes gate; only Tier-B subject). `bundle.rs::compute_bundle_byte_report(emit_report, plan, manifest) -> BundleByteReport` (attributes emitted artifacts to `(component_id, tier)`; wrappers count, source-maps tracked but excluded from budget, vendor chunks aggregated separately, infrastructure JSON excluded). `format.rs::format_report_pretty(report)` — `TierBBundleKbPerComponent` violations include an actionable `hint` line. Defaults: Tier-A ≤ 50 cmp/route, Tier-B ≤ 8 KB/route + 4 KB/cmp source-weight, **Tier-B ≤ 1 KB/cmp bundle-byte**, Tier-C ≤ 10 fetch/route. Per-route overrides via `[routes."/path"]` blocks.
- `src/runtime/broadcast.rs` — Phase O.2 broadcast slot registry. `BroadcastRegistry { topics: DashMap<String, Arc<BroadcastTopic>>, by_session: DashMap<SessionId, FxHashSet<String>>, next_frame_id: AtomicU64 }`. `topic(name, initial)`, `subscribe(session, topic, sender) -> Vec<u8>`, `unsubscribe`, `cleanup_session(session)`, `write_topic(topic, value) -> BroadcastDelivery`, `auto_subscribe(session, sender, topics) -> Vec<Instruction>`. Wire reuses `Instruction::SlotSet` — no `LOCKED_WIRE_VERSION` bump. `broadcast_slot_id(topic) = fnv1a_32("broadcast::{topic}")` namespace prefix avoids collision with Phase K hook slot ids. Backpressure via `try_send` — slow/dead consumers pruned immediately, surface in `BroadcastDelivery.dropped_{full,closed}`.
- `src/manifest/` — `mod.rs` (`build_render_manifest_v2`), `schema.rs` (RenderManifestV2, Tier, HydrationMode, WTStreamSlot, etc.), `builder.rs` (route/shell/assets builder; uses `runtime::eval::ComponentProject` for Phase J static render PLUS `runtime::compiled::CompiledProject` + `render_entry_with_broadcast` for Phase P Tier-B pre-render via `render_tier_b_inline`). Phase P schema additions on `TierBNode`: `initial_html: Option<String>` + `initial_opcode_frame: Vec<u8>` (bincode-encoded `OpcodeFrame`). Phase P schema additions on `RouteManifest`: `shared_slot_topics`, `action_ids: Vec<RouteActionEntry>` (placeholder until Stream C), `layout_chain`, `error_component`, `loading_component` (the last two are placeholders until Stream E.2).
- `src/bundler/` — `classify.rs`, `plan.rs`, `rewrite.rs` (wrapper module stable paths), `precompiled.rs` (QuickJS bytecode artifacts), `static_slice.rs`, `vendor.rs`, `emit.rs` (filenames, BundleEmitReport, runtime map, route-prefetch).
- `src/hydration/` — `plan.rs`, `payload.rs` (with FNV-1a checksum), `script.rs` (≤ 2KB bootstrap script template).
- `src/runtime/` — see [[project-runtime-kernel]].
- `src/runtime/renderer/` — `core.rs` (ModuleRegistry, RouteRenderRequest, RouteRenderResult), `manifest.rs` (`ServerRenderer<E: RuntimeEngine>` with LRU normalized_props_cache, static-slice cache, route invalidation, `prime_runtime_cache`).
- `src/runtime/eval/` — `core.rs` (88KB; `ComponentProject` evaluator with thread-local `RENDER_K` for Phase K hook-compile state). Phase O.2 surgical edit ~120 lines: `ComponentScope.shared_slots: HashMap<String, (SlotId, String)>` field, new `PHASE_K_BROADCAST: Cell<Option<*const BroadcastRegistry>>` thread-local + `install_phase_k_broadcast` RAII guard, `phase_k_shared_slot_for_value` helper, `phase_k_detect_slot_text_read` extended to recognise shared-slot bindings (useState wins on collision), `eval_var_decl_into_env` branch for `const x = useSharedSlot("t")` resolving via `current_phase_k_broadcast()`, new `ComponentProject::render_entry_compiled_with_broadcast`. **All 17 hook-compile golden tests unchanged.** `expr.rs` (SWC parse → `ParsedModule`), `component.rs` (utility helpers: `fnv1a_32`, `escape_html`, etc.).
- `src/runtime/quickjs_engine.rs` — `QuickJsEngine` (impl `RuntimeEngine`); compiles SWC TSX → JS with `swc_ecma_transforms_react` + `swc_ecma_transforms_typescript::strip_type`; uses `rquickjs`.
- `src/runtime/compiled.rs` — Phase K `CompiledProject` facade over `ComponentProject`. `allocate_slot_id`/`allocate_proxy_id`/`allocate_capture_slot_id` (FNV-1a-32 hashes). `render_entry_with_bindings`. Phase O.2: `CompiledComponent.shared_slots: Vec<SharedSlotBinding>` populated in `wrap()`; `CompiledProject::shared_slot_topics() -> Vec<String>` + `shared_slots_for_component(module, fn)` lookups; `render_entry_with_broadcast(compiled, entry, props, slots, broadcast, sender, opts) -> RenderOutput` auto-subscribes session + prepends initial-state SlotSets.
- `src/runtime/slot_store.rs` — Phase H `SlotStore` (DashMap by (SessionId, SlotId)) + `SessionSlotView`. `drain_set_instructions` produces `Instruction::SlotSet` opcodes.
- `src/runtime/session.rs` — `SessionId` newtype over `uuid::Uuid`.
- `src/runtime/webtransport.rs` — `WebTransportMuxer`, `WTStreamRouter`, `FramePayload` (Text/Binary), `WebTransportFrame`, `WTRenderMode`, `WebTransportError` (incl. `PayloadKindMismatch`). Slot constants 0/1/2/3 = control/shell/patches/prefetch.
- `src/bin/` — three binaries:
  - `albedo.rs` (~110 KB after Phases N + O.1) — CLI: init/dev/build/ship/serve/files/budget/run/completions/help. Embeds `scaffold/` via `include_str!`. Sub-modules in `bin/albedo/`: `printer.rs`, `first_run.rs`, `inspector.rs` (dev inspector — vendors `inspector.html` from albedo-server). Phase N ship targets: `configure_ship_docker` (multi-stage `rust:1-bookworm` → `debian:bookworm-slim` Dockerfile, HEALTHCHECK, `ALBEDO_SERVER_{HOST,PORT}` env), `configure_ship_fly` (Dockerfile + fly.toml with `[[http_service.checks]]`), `configure_ship_vercel` → explicit `Err("vercel is not a supported ship target...")`. Phase O.1 budget: `run_budget_command` (standalone evaluator with `--strict`/`--format`), `run_prod_build_with_budget(contract, skip)` gated by `tier-budget.toml` presence, `--no-budget` opts out on `build`/`ship`. All four shell completions (bash/zsh/fish/powershell) carry the `budget` subcommand and `--no-budget` flag.
  - `dom-compiler.rs` (39 KB) — sub-commands analyze/showcase/bundle/dev.
  - `albedo-bench.rs` — benchmark runner.
- `src/dev/` — `contract.rs` (DevConfig/albedo.config.json parsing), `benchmark.rs` (workload runner, GateStatus), `showcase.rs` (build_showcase_artifact: HTML + stats).

- `src/bundler/rewrite.rs` — Phase M.4 source-map sidecar generation: `build_wrapper_source_map`, the `//# sourceMappingURL=` appendage in `build_wrapper_module_source`. v3-spec stubs.
- `src/bundler/emit.rs` — Phase M.4 `emit_wrapper_source_maps(plan)` + sibling `.map` write loop.

**`crates/albedo-server/src/`** (axum runtime):
- `lib.rs` — re-exports `AlbedoServer`, `AlbedoServerBuilder`, `CompiledRouter`, `RendererRuntime`, `TierBRenderRegistry`, `WebTransportRuntime`, `ActionHandler`, `InspectorState`, etc.
- `server.rs` (74 KB) — builder, `RuntimeState`, `CompiledProjectActionAdapter`.
- `routing.rs` — `CompiledRouter` (matchit-based), `RouteTarget`, `AuthPolicy`.
- `handlers/` — `action.rs` (`ActionRegistry: HashMap<u32, Arc<dyn ActionHandler>>`, decodes `ActionEnvelope`, Phase L CSRF gate via `extract_csrf_field`), `api.rs`, `streaming.rs` (`StreamingAppState` with `csrf: Arc<CsrfRegistry>`, `with_csrf` builder, Phase L cookie session round-trip + post-render CSRF substitution in `build_shell_chunk`, Phase M.2 in-place HMR client integration), `dev.rs` (Phase M.1 + M.2 — `serve_error_stream`, `serve_hmr_stream`, `serve_overlay_script`, `serve_hmr_apply_script`), `public_assets.rs` (Phase N — `PublicAssets { roots, cache_header }`, `sanitize_public_path` blocks traversal/absolute/NUL/Windows-drive, `content_type_for_path` MIME table, GET/HEAD dispatch arm sits BEFORE route matcher).
- `render/tier_b.rs` — server-side island registry, data fetcher trait.
- `inspector/` — graph snapshot, metrics, SSE event publisher, heartbeat task. Inspector HTML lives at `crates/albedo-server/src/inspector/assets/inspector.html`.
- `webtransport.rs` — `WebTransportRuntime` over quinn (QUIC); per-session `mpsc::Sender<Vec<u8>>` per stream slot.
- `renderer_runtime.rs` — `RendererRuntime::from_artifacts_dir`. Loads `render-manifest.v2.json`, `runtime-module-sources.json`, optional `precompiled-runtime-modules.json`. Soft-fail prime_runtime_cache on startup.
- `actions.rs` — `ActionHandler` trait + `SessionSlots` view.
- `dev/` — Phase M dev-only surface. `error_overlay.rs` (`DevErrorRegistry`, `OverlayEvent`, `DevError`, `ErrorKind`, broadcast bus), `hmr.rs` (`HmrRegistry`, `HmrEvent`, `HmrPayload`), `mod.rs` (re-exports). Only mounted when `with_dev_mode(true)` or `cfg!(debug_assertions)`.
- `render/` — `tier_b.rs` (Phase E render registry), `csrf.rs` (Phase L `CsrfRegistry` + `substitute_csrf_token_in_html` + `read_session_cookie` + `build_session_set_cookie` + `ALBEDO_SESSION_COOKIE`), `form_action.rs` (Phase L `TypedFormActionHandler<T>` + `FromFormPayload` trait + `form_action_handler::<T>(...)` + `form_action_id(name)`), `form_validation.rs` (Phase L `validation_error_text_opcodes`), `mod.rs` (re-exports).
- `config.rs` — `AppConfig`, `ServerConfig`, `RendererConfig` (artifacts_dir), `RouteSpec`, `LayoutSpec`. Env override loader (`ALBEDO_*`).
- `lifecycle.rs`, `contract.rs`, `error.rs`, `api.rs`.
- `bin/albedo-server-demo.rs` — example server.
- `examples/chat_broadcast.rs` — Phase O.2 W2 runnable demo. Single-file HTTP server with `GET /sse` (subscribes session to `chat:lobby` via `auto_subscribe`) + `POST /post` (broadcasts message to all subscribers). `cargo run -p albedo-server --example chat_broadcast`. Two `curl -N /sse` terminals + one `curl -X POST /post` = the two-tab demo at the API level.
- `tests/` — `compiled_project_dispatch.rs`, `server_wire_integration.rs` (Phase L includes the async-island regression fix with `tokio::sync::oneshot` resolver gate), `form_action_roundtrip.rs` (Phase L typed handler unit-style), `form_submit_end_to_end.rs` (Phase L Step 6 — full HTTP flow with cookie session + CSRF validation), `public_assets_end_to_end.rs` (Phase N — GET/HEAD hit, nested hit, miss, traversal block, dev cache-header), `broadcast_end_to_end.rs` (Phase O.2 W1 — two sessions, fan-out, late-joiner seed, cleanup, Arc-identity).

**`crates/albedo-node/`** — NAPI bindings.
- `src/lib.rs` — `analyzeProject`/`optimizeManifest`/`getCacheStats` via `napi::Task` (off the event loop).
- `package.json`, `index.js`, `typed.d.ts`, `index.win32-x64-msvc.node` (4.6 MB prebuilt).

Reference docs at the root:
- `README.md` — pitch + Tier table + quick start.
- `Implemenatation-plan.md` — the active wiring plan; see [[project-wiring-plan]].
- `albdo_fixtures.md` — same wiring plan body, named for fixture discussion.
- `CONTRIBUTING.md`, `SECURITY.md`, `LICENSE.md`, `CODE_OF_CONDUCT.md`, `deny.toml`, `.clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`.
