---
name: project_dev_serve_unification
description: albedo dev now boots the SAME production streaming pipeline as serve (one renderer) with hot-swap reload — closes the dev/serve parity gap. Legacy dev renderer is dead code pending deletion.
metadata: 
  node_type: memory
  type: project
  originSessionId: 058ac778-3b3c-49b5-9cee-22621aa1d03d
---

**The dev/serve parity gap is CLOSED (2026-07-02, uncommitted, live-verified).** Root cause: `albedo dev`
ran a SECOND, legacy renderer (`render_all_routes`/`render_single_dev_route`/`compose_dev_layouts` — the
Phase-K/bakabox opcode path + a hand-rolled HTTP server) while `albedo serve` used the modern Tier-A/B/C
streaming pipeline. Every feature landed since A4 (islands, dynamic `generateMetadata`, error/loading
boundaries, `head.html` pre-paint) went into serve ONLY — so dev silently rotted (it even hardcoded
`<title>ALBEDO Dev</title>`). Fixed by making **dev = the production pipeline + watch + hot-swap**, not a
second renderer.

Key discovery that made this tractable: the server crate ALREADY had all the dev machinery (dev_mode
flag, `/_albedo/dev/*` overlay+HMR SSE endpoints, client scripts, error/HMR registries, action→overlay
reporting). It just wasn't booted by dev, didn't inject the dev client scripts into the streaming shell,
and had no hot-swap. So this was "wire dev into the pipeline that exists + add hot-swap + delete the
legacy path," NOT "build a dev pipeline."

## Three increments (all landed)
- **A · dev-complete the production pipeline.** `StreamingAppState` gained `dev_mode` + `with_dev_mode`
  (`crates/albedo-server/src/handlers/streaming.rs`); `build_stream` injects
  `<script src="/_albedo/dev/overlay.js" defer>` + `hmr-apply.js` before `</body>` when dev. Both
  self-connect via `EventSource` on load. `ProductionServerOptions` gained `dev_mode` (`boot.rs`, default
  false); `boot_production_server` threads it to `with_dev_mode`. Wired in `server.rs build()`
  (`dev_mode_enabled` hoisted above the streaming-state construction).
- **B · hot-swappable render world (the elegant core).** Split `RuntimeState` (`server.rs`) into:
  - **`RenderWorld`** — the self-contained render/dispatch state (router, handlers, api_handlers,
    action_handlers, slot_store, csrf, layouts, middleware, auth_provider, request_timeout,
    streaming_runtime, public_assets, broadcast). Everything render-coupled as ONE unit so a full-reload
    swap is trivially consistent (action handlers + their slot store + streaming state are always built
    together).
  - **`RuntimeState`** = `{ world: Arc<RwLock<Arc<RenderWorld>>>, inspector, dev_error_registry,
    dev_hmr_registry }`. Only the SSE-backing dev registries + inspector persist across a swap, so the
    HMR/overlay connections survive and the reload event reaches the client.
  - `dispatch_inner` loads the world ONCE (`let world = state.world();` — read-lock, clone Arc, drop
    guard; never held across `.await`) so a concurrent swap can't split one request across two worlds.
    Helpers (`execute_route`, `run_action_route`, `run_api_request`, `should_use_manifest_streaming`,
    `apply_layout_handlers`) take `&RenderWorld`.
  - **`DevReloadHandle`** (pub, exported from lib): `reload(&opts)` boots a fresh world via
    `boot_production_server`, grafts it onto the running server's world slot (`RwLock` write), clears the
    overlay, pushes `HmrRegistry::reload(rev)`. Build failure leaves the last good world serving + reports
    to the overlay. `AlbedoServer::dev_reload_handle()` returns it only when dev_mode. **Chose std
    `RwLock<Arc<_>>` over the `arc-swap` crate** (no new dep; read-lock-clone-release is nanoseconds vs
    microsecond renders; prod never swaps → uncontended). Chose **full-reload semantics** (client
    `location.reload()`, slots reset) over slot-preserving in-place swap — user pick; simpler/robust, and
    a full reload resets client islands anyway. Slot-preserving is a documented future upgrade.
- **C · rewrite `albedo dev`.** `run_live_dev_runtime` (`src/bin/albedo.rs`) now: `run_prod_build` →
  `boot_production_server(opts{dev_mode:true})` → spawn `dev_watch_and_reload` (notify watcher on
  `contract.root`=src, NOT `.albedo/dist` so a rebuild can't retrigger itself; debounce-drain the burst →
  `run_prod_build` + `reload.reload(&opts)`) → run the server on a fresh tokio runtime. The legacy
  `run_live_dev_runtime` was renamed `legacy_live_dev_runtime` + `#[allow(dead_code)]` (deletion tracked
  as spawned task `task_aa28936f` — ~15 scattered fns + 3 tests, compiler-guided, keep shared build/serve
  helpers).

## Verification (live on Halation, debug binary)
- **dev** (`albedo dev --port 3300`/`3009`): served + browser. Dynamic `<title>On the Glow Around Bright
  Things — Halation</title>` (generateMetadata — legacy dev couldn't), `data-theme=dark` (head.html
  pre-paint), MarginNote `data-albedo-hydrated=true` + char count 0→11 (event handler runs), ReadingProgress
  present, overlay + hmr client scripts loaded, `/_albedo/dev/errors` + `/_albedo/dev/hmr` SSE both 200
  (connected), console clean. **Hot reload proven bidirectionally**: edited `MarginNote.tsx` → log
  `✓ reloaded in ~950ms` → served HTML updated on the SAME socket (no restart); reverted → reloaded again.
- **serve** (`albedo serve --port 3320`): dynamic title + 2 island markers + **ZERO `/_albedo/dev/`
  scripts** (injection correctly gated by dev_mode). No prod-path regression.
- Tests: albedo-server 124 lib + all integration (incl. `serve_boot_end_to_end` = the dispatch refactor's
  coverage), albedo bin 26, dom-render-compiler 420 lib — all green.

## Gotchas / notes
- `.claude/launch.json` gained a `halation-dev` config (`albedo dev --port 3009`) for preview.
- Preview quirk (again): programmatic `window.scrollTo` doesn't emit a native `scroll` event in a
  backgrounded preview tab (dispatch `new Event('scroll')`); rAF can stall (use `setTimeout`).
- The reload's `boot_production_server` runs from the watcher std-thread and works WITHOUT a tokio context
  (same as the initial serve boot — no opcode pipeline bound → no `Handle::current()` needed). Re-warms
  the QuickJS pools each reload (fine for dev, ~950ms total rebuild+swap on Halation).
- Related: [[project_p5_useeffect_hydration_gap]] (the island features dev now shows), [[project_preferences_panel]]
  (head.html pre-paint — was ⚠️ "dev doesn't inject the partial"; NOW IT DOES via the unified pipeline),
  [[project_albedo_server]], [[feedback_rewrite_weak_design]] (this was the weak-design rewrite).
