---
name: project_error_loading_boundaries_gap
description: CLOSED 2026-06-26 — error.tsx/loading.tsx boundaries now render on albedo serve (was discovered+in-manifest but dropped at serve)
metadata: 
  node_type: memory
  type: project
  originSessionId: 7c866b42-1b2c-42c4-aaf2-deebedccb171
---

✅ **CLOSED 2026-06-26** (uncommitted — user owns commits). `error.tsx`/`loading.tsx` boundaries now render on `albedo serve`. Was the same "discovered→built→dropped at serve" disease as RSC ([[project_async_server_components_gap]]).

**The fix (3 files, all in `crates/albedo-server`):**
1. **Boot plan** — `RendererRuntime::build_tier_b_render_plan` (`renderer_runtime.rs`) now also registers each route's `error_component`/`loading_component` into the `TierBRenderPlan`, keyed by the **bare component name** (Tier-B nodes stay keyed by `render::*` — no collision). Refactored the per-component resolve (entry module + dep-ordered source graph) into a reusable `add_component_to_plan(&self, plan, by_name, key, component_name)`; best-effort + idempotent per key.
2. **Wire** — new `InjectionChunk::error_boundary(node, html)` → `__albedo_inject(id, html, 'error')` and `InjectionChunk::fallback_with_html(node, html)` → `…'fallback'` variants (`render/tier_b.rs`). They ship **real HTML** so the client replaces `outerHTML` (the `html===null` attribute-only path in `albedo-runtime.js:629` is now only the last-resort stub).
3. **Serve** — `build_stream` (`handlers/streaming.rs`) captures `route.error_component`/`loading_component`, and on `Ok(Err(err))` renders the error boundary via the warmed `PooledTierBRenderRegistry` (props `{error:{message: err.to_string()}}`), on timeout renders the loading boundary; via new async helper `render_route_boundary(app, component, props) -> Option<String>` (returns None → generic stub). Calls `app.services.registry.call(name, props, &HashMap::new())`.

**Verified end-to-end** on portfolio `/boom` (`albedo serve --port 3119`, debug binary): response carries `__albedo_inject("__b_boom_4", "<error.tsx html>")` with "Something went wrong" + "Recovered by error.tsx: …probe: intentional server-side failure" — the thrown message **propagated into `error.message`** (proving props wiring). Zero `null,'error'` stubs. 3 new unit tests in `tier_b.rs`; full `cargo test -p albedo-server` green (118 lib + all integration).

**Known cosmetic follow-up (not blocking):** the injected message is double-wrapped — `render registry failed for 'render::Boom': render registry failed for 'render::Boom': RenderError: …: probe: intentional server-side failure` (render_tier_b wraps the registry's RegistryFailure in another RegistryFailure). Real cause is present; could unwrap one layer for a cleaner `error.message`.

**Loading boundary caveat:** wired as the **timeout fallback** only. True Suspense semantics (loading UI in the shell up front, replaced when the Tier-B render resolves) are NOT done — today `build_stream` awaits all `tier_b_futures` before emitting injects, so there's no real deferral to show "loading" during. Symmetric follow-up if/when Tier-B rendering becomes incrementally streamed.

**Probe fixtures (KEEP):** `A:\albedo-portfolio\src\routes\{boom,error}.tsx`. ✅ **`/boom` NEUTERED 2026-06-26** — `loadData()` now returns instead of throwing; re-arm via the one commented `throw` line. Demo-safe.

**✅ Audit DONE 2026-06-26 — NO third instance.** Swept every `RouteManifest` field for the discovered→built→dropped pattern. Result: the bug class is closed at two (RSC + these boundaries). Findings: `layout_chain` is populated AND consumed (baked into the shell at build via `wrap_in_layouts`); `metadata` is populated AND consumed (baked into `<head>` at build, static slices 1+2 — dynamic `generateMetadata()` slice 3 is a *tracked TODO*, not a silent drop); `action_ids` is **never populated** — `collect_route_action_ids()` (builder.rs:256) is a stub returning `Vec::new()`, a documented schema-stability placeholder, and actions dispatch via the boot `ActionRegistry` (action.rs:83, keyed by FNV action_id) not this field. `shared_slot_topics`/`tier_c`/`error_component`/`loading_component` all consumed at serve. No fix needed.
