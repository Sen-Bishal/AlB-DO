---
name: project-phase-m
description: "Phase M (FALLOUT — DX) — error overlay, slot-preserving HMR, TypeScript intrinsic types, source-map sidecars. Foundations + what's deferred."
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

Phase M is the DX layer. Four sub-deliverables shipped in one batch; M.5 (VS Code extension) is deferred indefinitely as pure tooling.

**Why this memory exists:** Phase M adds a server-side dev surface (`crate::dev::*`) and a client-side overlay/HMR pair that's reusable infrastructure for everything that comes after. Future work in N/O hangs off these.

**Commit**: `299893d` "ennumerating phase M with DX experience and fallback for N".

## What landed

### M.1 — Error overlay

**Server-side** (`crates/albedo-server/src/dev/error_overlay.rs`):
- `DevErrorRegistry` — broadcast-channel-backed, monotonic id allocator, kind-tagged events (Compile / Render / Action / Runtime). Cheap to clone (`Arc<DevErrorRegistry>`).
- `OverlayEvent` enum — `Error(DevError)`, `Dismiss { id }`, `Clear`. Serializable to JSON for the SSE stream.
- `DevError` carries optional file/line/column when the originating diagnostic surfaces them.

**HTTP surface** (`crates/albedo-server/src/handlers/dev.rs`):
- `GET /_albedo/dev/errors` — SSE stream of `OverlayEvent`s under the `overlay` SSE event name. Keep-alive every 15s.
- `GET /_albedo/dev/overlay.js` — the IIFE client.
- `dev_not_found()` for unmatched dev paths so misroutes don't fall through to 500.

**Client** (`assets/albedo-error-overlay.js`):
- Self-contained IIFE. Mounts a fixed-position `<div data-albedo-error-overlay>` host with `attachShadow({mode: 'closed'})` so the overlay's CSS never bleeds into the page or vice versa.
- Renders kind-coloured chips (Compile=orange, Render=red, Action=purple, Runtime=yellow). Each entry has a head line, optional meta (file:line:col), an optional pre-block of remaining message lines, and a dismiss button.
- ESC dismisses the topmost error. Conn indicator in bottom-right shows live / reconnecting / waiting.
- Exponential backoff on reconnect (500ms → 8s cap).

**Server wiring** (`crates/albedo-server/src/server.rs`):
- `AlbedoServerBuilder::with_dev_mode(bool)` toggle; defaults to `cfg!(debug_assertions)`.
- `RuntimeState.dev_error_registry: Option<SharedErrorRegistry>` and `RuntimeState.dev_hmr_registry: Option<SharedHmrRegistry>` — `Some` when dev mode is on.
- Dispatch arm in `dispatch()` recognizes `/_albedo/dev/{overlay.js,hmr-apply.js,errors,hmr}` paths.
- `AlbedoServer::dev_error_registry()` accessor lets userland push reports.

### M.2 — Slot-preserving HMR

**Server-side** (`crates/albedo-server/src/dev/hmr.rs`):
- `HmrRegistry` — broadcast bus with `HmrEvent::Apply(HmrPayload)` carrying `{ route, html, revision }` and `HmrEvent::Reload { revision }` as the escape hatch.
- Monotonic revision number ordering: client drops out-of-order events.

**HTTP surface** (`crates/albedo-server/src/handlers/dev.rs`):
- `GET /_albedo/dev/hmr` — SSE stream of HmrEvents under the `hmr` SSE event name.
- `GET /_albedo/dev/hmr-apply.js` — the IIFE client.

**Client** (`assets/albedo-hmr-apply.js`):
- IIFE that subscribes to `/_albedo/dev/hmr`. On `Apply`: parses the new HTML off-document via `DOMParser`, replaces `document.body.innerHTML`, restores draft input values (except `type="password"`) + scroll position + checkbox state. Dispatches a `CustomEvent("albedo:hmr-applied")` for userland integrations.
- Fall back to full reload when the fetch or parse fails.

**Dev CLI inline script** (`src/bin/albedo.rs::inject_hmr_client_script`):
- Now does in-place fetch-and-swap instead of `window.location.reload()` when an HMR event arrives over `/_albedo/hmr` (the dev CLI's own SSE endpoint). Same draft-input + scroll preservation. **This is the default behaviour now** — every `albedo dev` save preserves slot state.

**Slot state preservation rationale**: ALBEDO's slot store (Phase H) is server-side, keyed by `SessionId` from the `albedo-session` cookie (Phase L). The cookie survives the in-place DOM swap, so the next render call reads the same slot values. No client-side reconciliation needed.

### M.3 — TypeScript intrinsic types

`scaffold/src/albedo-env.d.ts` rewritten from `[k: string]: any` placeholder to a concrete intrinsic-element type surface:
- `AlbedoBaseAttributes` with proper event handler types (`MouseEvent`, `KeyboardEvent`, `SubmitEvent`, `InputEvent`, `PointerEvent`, `FocusEvent`, `TouchEvent`).
- Per-tag attribute groups: `InputAttributes`, `FormAttributes`, `ButtonAttributes`, `SelectAttributes`, `OptionAttributes`, `TextareaAttributes`, `LabelAttributes`, `AnchorAttributes`, `ImgAttributes`, `MetaAttributes`, `LinkElementAttributes`, `ScriptAttributes`.
- Built-in `<Link>` component declared with required `href`.
- Catch-all `[tagName: string]: AlbedoBaseAttributes` keeps the surface permissive for unenumerated tags.
- `DataAttributes` + `AriaAttributes` index signatures.
- Ambient `Window.__ALBEDO_RUNTIME` types for advanced integrations.

### M.4 — Source maps

**Bundler emit** (`src/bundler/rewrite.rs`, `src/bundler/emit.rs`):
- `build_wrapper_module_source` now appends `//# sourceMappingURL=<basename>.mjs.map`.
- `build_wrapper_source_map(source_module)` emits a v3-spec JSON stub: `{"version":3, "file":..., "sources":[<tsx>], "sourcesContent":[null], "names":[], "mappings":""}`.
- `emit_wrapper_source_maps(plan)` returns `BTreeMap<String, String>` of map paths to content.
- The bundle-emit loop writes the `.map` file as a sibling artifact for every wrapper.

**Stage 1 vs Stage 2**: Stage 1 stubs let browser DevTools open the original `.tsx` from the call stack (the "go to source" works). Per-line resolution requires wiring SWC's `Vec<(BytePos, LineCol)>` mappings collector through the QuickJS-transpile path. That's the Stage 2 follow-up — a 30-line edit once someone needs it.

## Server-side public API additions

For userland integrations that want to push events:

```rust
let server = AlbedoServerBuilder::new(config)
    .with_dev_mode(true)
    .build()?;

// Push an error to the floating overlay:
if let Some(registry) = server.dev_error_registry() {
    registry.report_render("/dashboard", "render panicked at line 42");
}

// Push an HMR apply (e.g. from a custom file watcher):
if let Some(registry) = server.dev_hmr_registry() {
    registry.apply("/", "<body>fresh html</body>", revision);
}
```

## What's intentionally NOT done

- **No auto-integration with error paths.** `run_action_request` doesn't call `dev_error_registry.report_action(...)` on handler error; `streaming_handler` doesn't call `report_render(...)` on render error. The hooks are 10-line edits each, deferred to keep the M batch reviewable. Userland or a follow-up pass wires them.
- **No SWC source-map collector wiring.** M.4 emits valid v3 stubs (no per-line mappings). Stage 2 work.
- **No VS Code extension (M.5).** Pure tooling, no runtime impact, deferrable indefinitely.
- **No production server's HTML shell auto-loads the overlay/HMR scripts.** The endpoints exist and serve content; if a user wants the floating overlay over a production-style server they include `<script src="/_albedo/dev/overlay.js"></script>` themselves. Default-on auto-include needs to gate on dev_mode_enabled which means threading it through the manifest builder's shim script — also a small follow-up.

## Dev-mode invariant

Dev surface is opt-in: `with_dev_mode(false)` or running a release build with no explicit toggle disables the registries entirely (they're `None` on `RuntimeState`). Dispatch arm only mounts the routes when the corresponding registry is `Some`. Production binaries have zero dev-route exposure by default.

## Pre-existing flakes encountered (none are Phase M's fault)

- `runtime::render_observer::frame_guard_publishes_on_drop_with_duration_recorded` — uses a shared `OnceLock` recorder across tests; flakes when tests run in parallel. Manifests as `assertion left: 3, right: 1`.
- `production_cache_tests::test_production_cache_effectiveness` and `test_production_realistic_workflow` — fail when tests are serialized via `--test-threads=1`. Investigation pending.

Both pre-date Phase L and survived the L/M churn. Neither blocks Phase M's gate.
