---
name: project_async_server_components_gap
description: Gate 4 dogfood finding — async server components are unsupported and fail silently to an empty island
metadata: 
  node_type: memory
  type: project
  originSessionId: 74179692-20b1-4bf2-9a7a-6c58503b4a2c
---

**Finding (2026-06-21, Gate 4 E dogfood of the portfolio):** ALBEDO has **no async server component** concept. An `async function Page()` (the Next App Router idiom: run on server, await data, ship static HTML, zero client JS) is **misclassified as a client island and renders nothing**.

**Root cause:** `decide_tier_and_hydration` in `src/effects.rs:120` treats `effects.asynchronous` as an `AsyncBoundary` hydration signal → routes the component to Tier C or (no handler/hook + small weight) **Tier B**. The SSR render then emits an *empty* `<div data-albedo-tier="b">` placeholder for client hydration; the client runtime (`assets/albedo-client.js`) calls the component, gets a `Promise`, and renders nothing. The QuickJS render entry `__ALBEDO_RENDER_COMPONENT` (`src/runtime/quickjs_engine.rs:417`) does `String(__albedo_component(props))` with **no await / job-queue drain**, so even on the server path a Promise would stringify to `[object Promise]`.

**Two bugs, one of which violates a Never-Cut invariant:**
1. **Wrong semantics** — async-without-hooks-or-handlers is a *server data* component (render+await on server → static Tier-A HTML), the opposite of a client island.
2. **Silent failure** — empty `<main>`, no overlay, no console error. Violates the "loud errors" invariant ([[project_gate1_closed]], TODO "Never cut: loud errors"). The loud-error stopgap should land regardless of whether full support does.

**Fix direction (correct "feels like Next" answer):** async + no event-handlers/hooks → treat as server-render component; QuickJS render must **await the returned Promise** (drain the rquickjs job queue) before lowering to HTML; manifest builder routes it to the static-render-at-build path (pure-Rust Tier-A evaluator can't async — must go through QuickJS). The async+interactive combined case is the hard one (Next composes a server component wrapping a client component) — defer.

**Probe artifact:** `A:\albedo-portfolio\src\routes\stats.tsx` (`async function Stats()` awaiting a stub `getStats()`). Currently renders empty `__b_stats_7`. Keep as the failing fixture until fixed.

Context: this is the first real gap surfaced by the Gate 4 dogfood-probe pass ([[project_dogfood_portfolio]] methodology, which also surfaced [[project_a4_userland_boundary]]). The zero-network proof (Gate 3 line 45) was captured clean the same session.

---

## UPDATE 2026-06-21 (impl session) — the gap was DEEPER than the original diagnosis; user chose request-time full RSC

Investigating the fix surfaced a **third layer the original note missed**: in the `albedo serve` binary, **Tier-B server rendering is STUBBED**. `SharedRenderServices::default()` (`crates/albedo-server/src/render/tier_b.rs:379`) installs `StubTierBRenderRegistry`, whose `call()` returns an empty `<section data-render-fn=…>` — it renders *nothing*. So `Stats` (mis-tiered B) AND `ChatLobby` (legit Tier-B) BOTH inject empty stubs via `__albedo_inject` (verified on live :3115 debug serve). Serve render reality: **Tier-A** = baked at build via pure-Rust (works); **Tier-B** = stub (renders nothing); **Tier-C** = island SSR + hydration (works). The active route path is `build_stream` (prebuilt `route.shell` + `render_tier_b` registry), NOT the QuickJS `render_route_stream_with_manifest_hydration` entry-render path.

**User decision (AskUserQuestion):** implement **request-time full RSC** (per-request await, honest "fetched at request time"), not build-time freeze or loud-fail-only. **Engine facility: REUSE the warmed action `QuickJsEnginePool`** — rendering a component and running an action are the same QuickJS capability; the pool already runs arbitrary `with_engine(|e| …)` closures against warmed/concurrent/arena engines and loads modules on-demand+idempotently (same as actions). A 2nd pool / mutex'd single engine are strictly worse.

**DONE + verified this session (all uncommitted):**
- ✅ **Bug #2 (QuickJS await) FIXED** — `render_component_inner` now calls the render fn as `MaybePromise` and `.finish::<String>()` (drives the job queue to resolution; loud `WouldBlock` if it can't); the JS `__ALBEDO_RENDER_COMPONENT` returns `value.then(envelope, errEnvelope)` when the component returns a thenable, else the sync string fast path. `use rquickjs::promise::MaybePromise`. **2 new unit tests green** (`async_server_component_is_awaited_on_render`, `…_rejection_surfaces_loudly`); full 403+ lib suite still green.

**REMAINING (the request-time serve wiring — NOT yet done):**
1. **Pool-backed `TierBRenderRegistry`** replacing the stub: `pool.with_engine(|e| { load modules; e.render_component_with_host(entry, props, host) })`. Needs the module artifacts (sources + precompiled, on `RendererRuntime`) + manifest to map `TierBNode.render_fn` → entry module + module order (mirror `ServerRenderer`'s load logic). Wire at `server.rs:599` `build()` where the pool (`self.action_engine_pool`) is in scope. This ALSO fixes ChatLobby/all Tier-B.
2. **Tiering (`effects.rs`/`parser.rs`)** — async-without-interactivity → non-hydrating server node so client hydration can't clobber the injected HTML (likely already inert for no-handler/no-hook Tier-B, verify). Bug #1 from the original note.
3. Rebuild binary + re-verify `/stats` on live serve shows real numbers; keep `stats.tsx` as the gate.

⚠️ Build/verify loop: preview ran the **debug** binary on **:3115** (added launch config `albedo-portfolio-dbg`; the existing `albedo-portfolio` config points at the stale **release** binary). `taskkill /F /IM albedo.exe` before each `cargo build`.

---

## ✅ CLOSED 2026-06-21 — RSC serve-wiring DONE & verified end-to-end (all uncommitted)

All three remaining items shipped + a **fourth bug found en route** (the real crash):
- **(1) Pool-backed `PooledTierBRenderRegistry`** (`crates/albedo-server/src/render/tier_b.rs`) replaces `StubTierBRenderRegistry`. Holds the warmed `action_engine_pool` + a boot-built `TierBRenderPlan` (`render_fn` → `{entry, ordered (specifier,code) modules}`), built by `RendererRuntime::build_tier_b_render_plan` (`renderer_runtime.rs`, via `module_registry().resolve_module_order`). `call()` checks out a pool engine, loads the module graph (idempotent), runs `render_component_with_host` (awaits the RSC Promise via the already-landed `MaybePromise::finish`). Unregistered component → **loud** `RegistryFailure`, never silent-empty. Wired at `server.rs` `build()` (only when renderer + pool both present). This unstubbed **ChatLobby/all Tier-B** too.
- **(2) Tiering** (`effects.rs:120`): async + `!has_event_handler` → Tier-B with `HydrationMode::None` (server data component; no client island to clobber).
- **(4) THE CRASH — QuickJS arena residual hazard.** Pool engines were warmed only on the **handler-eval** path (`warm_engine` did 10 `eval_handler`s → `renders_done=10`), so the *first* component render ran scoped and interned its render machinery/atoms into the **request** region → `end_request` freed it → **use-after-free segfault on the 2nd request** (no Rust panic; debug died after 1 req, a generic synthetic-component warm-up attempt only pushed it to 3). Fix = explicit warm-up bracket: `QuickJsEngine::begin_warmup()/end_warmup()` (new `force_persistent` field forces persistent mode regardless of the `renders_done` counter) + `QuickJsEnginePool::warm_render_path(&[WarmupComponent])` (sync, broadcasts a render warm-up job to **every** engine at boot) + `warm_render_targets` renders each REAL Tier-B component in persistent mode so its interned state lands persistently. Called from `server.rs build()` before serving. Mirrors the boot `ServerRenderer`'s "prime every route" pass, per pool engine. **Lesson: generic warm-up is NOT enough — component-specific atoms must be warmed by rendering the REAL components.**

**Verified:** debug + release both serve `/stats` rendering `1284 commits across 37 repos since 2019.` (was empty `__b_stats_7`); ChatLobby Tier-B renders full real HTML; **90/90 sequential + 40/40 concurrent**, server stays up (was: died after 1). 116 albedo-server lib + 16 quickjs/arena tests green, `cargo check --workspace` clean. Files: `effects.rs`, `runtime/quickjs_engine.rs`, `crates/albedo-server/src/{engine_pool,render/tier_b,renderer_runtime,server}.rs`. **Fresh `cargo install --force` done — PATH `albedo` (`~/.cargo/bin`) is now the RSC-fixed binary (v0.1.0-alpha.1); stale 2026-06-20 install replaced; phantom `albedo-bench` install-metadata entry corrected (actual pkg bins = `albedo` + `albedo-server-demo`).** *Hard case still deferred (per original note): async + interactive (server component wrapping a client island).*
