---
name: project-albedo-server
description: "HTTP runtime crate — AlbedoServer builder, ActionRegistry, streaming + opcode pipeline, WebTransport (quinn), Inspector, RendererRuntime artifact loading."
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

`crates/albedo-server` is the axum 0.8 HTTP runtime. It's the production-side consumer of the artifacts the compiler emits. Cannot depend on a Node runtime; the `dev` CLI re-implements a tiny slice of the inspector for self-contained dev mode.

**Why:** The user is actively wiring runtime gaps into this server. Several gaps in [[project-wiring-plan]] touch this crate (e.g. Gap 2 — `prime_runtime_cache` in `renderer_runtime.rs`).

## Entry point — `AlbedoServer` (`server.rs`, 74 KB)
Builder pattern: `AlbedoServer::builder().port(...).register_route(...).register_action(...).build().serve().await`.

`RuntimeState` (cloned per request):
- `router: Arc<CompiledRouter>` (matchit-based).
- `handlers`, `api_handlers`, `action_handlers: Arc<ActionRegistry>`, `layouts`, `middleware`, `auth_provider`.
- `slot_store: Arc<SlotStore>` — shared with the runtime pipeline.
- `request_timeout: Duration`.
- `streaming_runtime: Option<Arc<StreamingAppState>>`.
- `inspector: Option<Arc<InspectorState>>`.

`MAX_REQUEST_BODY_BYTES = 2 MB`.

`CompiledProjectActionAdapter` bridges Phase K's `CompiledProject.invoke_action` to the server's `ActionHandler` trait. `register_compiled_project` bulk-registers an adapter per (proxy_id, handler).

## Routing (`routing.rs`)
- `HttpMethod` enum + `TryFrom<&http::Method>`.
- `AuthPolicy::Optional|Required|Role(String)`.
- `RouteTarget { route_name, handler_id, entry_module, props_loader, layout_handlers, middleware, auth }`.
- `CompiledRouter` uses `matchit::Router<usize>` indexed into `Vec<PathRouteEntry>`. Each entry holds method → target.
- `RouteMatch::Matched|MethodNotAllowed{allowed}|NotFound`.

## Phase L · forms, Navigate, CSRF

The forms/Navigate/Link suite added a per-server `Arc<CsrfRegistry>` shared between `StreamingAppState` and `RuntimeState`. The single-Arc invariant is load-bearing — if anything mints two registries, every form POST silently 403s.

- `crate::render::csrf::CsrfRegistry` — per-session token table. `token_for(session)` mints lazily.
- `crate::render::csrf::substitute_csrf_token_in_html` — literal `str::replace` of `value="" data-albedo-csrf` with `value="TOKEN" data-albedo-csrf` post-render. Called from `build_shell_chunk`.
- `crate::render::csrf::read_session_cookie` / `build_session_set_cookie` / `ALBEDO_SESSION_COOKIE = "albedo-session"` — the cookie round-trip helpers.
- `crate::handlers::action::run_action_request` — extracts `_csrf` field from JSON payloads, validates against the registry, returns 403 on mismatch. Non-form payloads skip the check.
- `crate::handlers::streaming::streaming_handler` — reads/mints the session, `Set-Cookie` on response, threads `page_session` through `build_stream` → `build_shell_chunk`.
- `crate::server::run_action_route` — reads cookie first, then `x-albedo-session` header, then fresh random.
- `AlbedoServerBuilder::register_form_action::<T>(action_name: &str, handler)` — derives `action_id` via `form_action_id(name)` (FNV-1a-32) internally; same hash family as the compile-time `transforms::form::allocate_form_action_id`.
- `AlbedoServer::csrf_registry()` — public accessor for tests/userland.
- `crate::render::form_action::TypedFormActionHandler<T>` + `FromFormPayload` trait — the more advanced path with `FormDecodeError::Validation` → per-field `SetText` opcode emission via `validation_error_text_opcodes`.

Detailed lessons in [[project-phase-l]].

## Phase M · dev surface

Opt-in via `AlbedoServerBuilder::with_dev_mode(bool)` (defaults to `cfg!(debug_assertions)`).

- `crate::dev::DevErrorRegistry` — broadcast bus + monotonic id allocator + `ErrorKind` taxonomy (Compile / Render / Action / Runtime). `report_render` / `report_action` / generic `report`. Wire shape `OverlayEvent::Error|Dismiss|Clear`.
- `crate::dev::HmrRegistry` — broadcast bus for `HmrEvent::Apply { route, html, revision }` + `Reload`.
- `crate::handlers::dev::serve_error_stream` / `serve_hmr_stream` — SSE endpoints under `/_albedo/dev/{errors,hmr}`.
- `crate::handlers::dev::serve_overlay_script` / `serve_hmr_apply_script` — static asset endpoints serving the IIFE clients (`assets/albedo-{error-overlay,hmr-apply}.js`).
- `AlbedoServer::dev_error_registry()` / `dev_hmr_registry()` — `Option<SharedRegistry>` accessors for userland integrations to push events.
- Dispatch arm in `crate::server::dispatch` recognises `/_albedo/dev/{overlay.js,hmr-apply.js,errors,hmr}` and routes only when the matching registry exists on `RuntimeState`. Production builds with `with_dev_mode(false)` see clean 404s on those paths.

Detailed work in [[project-phase-m]].

## Phase N · public/ static assets

Opt-in via `AlbedoServerBuilder::with_public_dir(path)` (stackable; first matching root wins) and `with_public_cache_control(value)` (defaults: `no-store` when dev mode is on, `public, max-age=3600` otherwise).

- `crate::handlers::public_assets::PublicAssets` — `{ roots: Vec<PathBuf>, cache_header: HeaderValue }`. `resolve(url_path) -> Option<PathBuf>` runs through `sanitize_public_path` (blocks `/`, absolute paths, parent-dir traversal, NUL bytes, Windows drive prefixes) then probes each root in registration order. `read_response(path)` returns an axum `Response<Body>` with extension-derived `Content-Type` + the registry's `Cache-Control`.
- `crate::handlers::public_assets::sanitize_public_path` and `content_type_for_path` are public so userland tests can reuse them without going through a `PublicAssets`.
- Dispatch arm sits in `crate::server::dispatch` between the action route and the route matcher. GET/HEAD only — other methods fall through and surface 405 from the router. HEAD returns headers with an empty body.
- `RuntimeState.public_assets: Option<Arc<PublicAssets>>`. `None` when no `with_public_dir(..)` calls were made.
- `AlbedoServer::public_assets()` accessor for tests/userland.
- Integration test: `crates/albedo-server/tests/public_assets_end_to_end.rs` (6 cases: hit, nested hit, miss, traversal block, HEAD empty body, dev cache header).

Detailed in [[project-phase-n]].

## ActionHandler (`actions.rs`) — Phase G/H
Trait:
```rust
async fn handle(&self, ctx: &RequestContext, envelope: &ActionEnvelope, slots: SessionSlots) -> Result<Vec<Instruction>, RuntimeError>;
```
Blanket impl for `Fn(RequestContext, ActionEnvelope, SessionSlots) -> Future<Output = ...>` — closures register directly.

`SessionSlots`: `{ session_id, store: Arc<SlotStore> }`. Read/write API + `drain_pending()`. Identical wire to `runtime::SessionSlotView`.

HTTP endpoint: `POST /_albedo/action`. Body = bincode `ActionEnvelope`. Response = bincode `OpcodeFrame` with `content-type: application/octet-stream`. Errors: 400 (malformed envelope), 404 (unknown action_id), 500 (handler error). After handler returns, dispatcher appends `slots.drain_pending()` (best-effort) to the response opcodes.

## Streaming (`handlers/streaming.rs`, 41 KB)
`StreamingAppState`:
- `manifest: Arc<RenderManifestV2>`.
- `services: SharedRenderServices`.
- `transport: StreamingTransportConfig`.
- `webtransport_sessions: Option<WebTransportSessionRegistry>`.
- `pipeline: Option<SharedPipeline>` = `Option<Arc<Mutex<FourLaneRuntimePipeline>>>`. Mutex over RwLock because the tick path is the only writer and uncontended `Mutex::lock` is one instruction.

WT headers: `x-albedo-wt-session`, `x-albedo-wt-prefer` (negotiation). Falls back to legacy JSON tier-B render when no pipeline is attached.

## RendererRuntime (`renderer_runtime.rs`)
`from_artifacts_dir(dir)`:
1. Reads `render-manifest.v2.json` → `RenderManifestV2`.
2. Loads sources from `runtime-module-sources.json` or, if missing, falls back to reading each `component.module_path` from disk.
3. Optionally loads `precompiled-runtime-modules.json` (QuickJS bytecode artifact).
4. Constructs `ServerRenderer<QuickJsEngine>` with empty `BootstrapPayload`.
5. `register_manifest_modules_with_precompiled` — binds precompiled bytecode by source_hash equality.
6. Soft-fail prime: pre-renders every manifest route with empty props; warns on error but continues.

Artifact filenames (constants in `dom-render-compiler::bundler::emit`):
- `RENDER_MANIFEST_FILENAME = "render-manifest.v2.json"`.
- `RUNTIME_MODULE_SOURCES_FILENAME = "runtime-module-sources.json"`.
- `BUNDLE_PRECOMPILED_MODULES_FILENAME = "precompiled-runtime-modules.json"`.
- `BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME = "route-prefetch-manifest.json"`.
- `BUNDLE_STATIC_SLICES_FILENAME = "static-slices.json"`.
- `BUNDLE_WT_BOOTSTRAP_FILENAME = "_albedo/wt-bootstrap.js"`.

## WebTransportRuntime (`webtransport.rs`)
- Built on `quinn` (QUIC). Per-session `WebTransportSessionHandle { session_id: Uuid, remote_addr, stream_senders: [mpsc::Sender<Vec<u8>>; 4] }`.
- `WebTransportSessionRegistry { sessions: Arc<Mutex<HashMap<Uuid, Handle>>> }`.
- `send_payload(session_id, stream_slot, payload)` validates slot < 4 and dispatches to the per-stream mpsc sender.

## Inspector (`inspector/`)
- `dispatch(path)` matches `/__albedo`, `/__albedo/api/graph`, `/__albedo/api/events`, `/__albedo/api/metrics`.
- `RenderEvent { component_id, component_name, tier, duration_us, timestamp_ms, cascade_children }` — wire shape mirrored by `bin/albedo/inspector.rs` so the same HTML works in dev.
- `MetricsAggregator` tracks latency ring + hot components + slow components + tier breakdown.
- `InspectorPublisher` — broadcast channel for SSE.
- `heartbeat.rs` — optional task pushing periodic metrics.

## Tier-B render (`render/tier_b.rs`)
- `TierBRenderRegistry` (async trait, server-supplied).
- `TierBDataFetcher` resolves `DataDep`s (`DbQuery`, `HttpFetch`, `Cache`, `RequestContext`).
- `RequestContext { path, params, headers, cookies }` and its `.resolve(key)` method (supports `header:foo`, `cookie:bar`, `path`).
- `stable_id_for_placeholder` MUST match `eval::component::fnv1a_32` — anchor IDs cross WT boundary.

## Config (`config.rs`, 17 KB)
- `AppConfig { server, renderer: Option<RendererConfig>, layouts, routes }`.
- `ServerConfig { host, port, request_timeout_ms, shutdown_timeout_ms, ... }`.
- `RendererConfig { artifacts_dir }`.
- Env overrides via `apply_env_overrides("ALBEDO_")` — `ALBEDO_SERVER_HOST`, `ALBEDO_SERVER_PORT`, `ALBEDO_REQUEST_TIMEOUT_MS`, etc.

## albedo-node (`crates/albedo-node/`)
NAPI bindings consumed by the npm shell package. JS surface:
- `analyzeProject(path, opts)` → returns RenderManifestV2 (via JSON `Value`).
- `optimizeManifest(manifest, opts)` → post-processes an existing manifest.
- `getCacheStats()` → metrics from the last `analyzeProject`.

`panic_safe(...)` wraps every closure (lint: unwrap/expect denied except inside that wrapper). Heavy work runs via `napi::Task` so Node's event loop isn't blocked.

`static LAST_CACHE_METRICS: Lazy<Mutex<CacheMetrics>>` — process-wide singleton.

Prebuilt artifact: `index.win32-x64-msvc.node` (4.6 MB) checked in; Darwin/Linux still planned per README.
