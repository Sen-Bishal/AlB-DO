---
name: project_p5_useeffect_hydration_gap
description: "P5 islands dogfooding surfaced a real engine gap — useEffect components don't hydrate on the serve path; fixes #1/#2/#3 + layout-island reachability + async-composite + MarginNote (which surfaced a binding-mode event-param bug) ALL landed & live-verified. P5 islands COMPLETE."
metadata: 
  node_type: memory
  type: project
  originSessionId: 51628aaa-1d2f-484d-8734-c15bf0c69bd5
---

Halation P5 (islands) dogfooding surfaced a genuine ALBEDO engine gap: **`useEffect`-bearing
components silently never run their effects on the serve path.** Investigated 2026-06-29 (uncommitted).

## Root cause (3 layers, all confirmed by reading the code + direct-serve tests)

1. **Tiering mis-categorizes `useEffect` as a passive hook.** `parser.rs is_hook_call` flags *every*
   `use[A-Z]*` as `profile.hooks=true`; only `console.*`/`document.write` set `side_effects`. In
   `effects.rs decide_tier_and_hydration`, a `hooks` component that isn't `client_interactive` (no
   `on*` handler whose closure is client-safe) → **Tier B (server-only, HydrationMode::None)**. So an
   effect-only island (e.g. a scroll-progress bar with `useEffect` but no onClick) is tiered B and
   never hydrates. (`side_effects` already routes to Tier C at effects.rs:96 — useEffect just wasn't
   counted as one.)
2. **Serve-wire eligibility ignores effects.** The serve path ships ONLY the binding-mode reactive
   runtime (`assets/albedo-reactive.js`, `window.__albedoReactive.boot({texts,attrs,derived,events})`),
   NOT the Tier-C hydration client (`assets/albedo-client.js`). `renderer_runtime.rs build_reactive_blocks`
   serve-wires any Tier-C component with bindings+events, and `server.rs` ~L690 INSERTS the reactive
   block OVER the hydration block **per route** (`blocks.insert(path, reactive_block)`). The reactive
   descriptor has no notion of effects → a component with BOTH an event and `useEffect` (e.g. a theme
   toggle that mutates `document`) gets serve-wired and its effect dropped.
3. **Per-route overwrite is coarse.** Because the merge is keyed by route path, a route that mixes a
   serve-wireable island AND a must-hydrate island can't have both — the reactive block clobbers the
   whole route's hydration block.

The hydration subsystem itself is complete + wired: `src/hydration/{mod,plan,payload,script}.rs` →
`renderer_runtime.rs build_hydration_blocks` (emits `data-albedo-island` + `/_albedo/client.js` +
payload/bootstrap) → `albedo-client.js hydrateIslandDescriptor` → `hydrateIsland` calls `runEffects()`.
The portfolio's useState/useEffect/useContext islands hydrate live through exactly this path. The bug
is only that the serve path keeps mis-tiering/serve-wiring effect components away from it.

## Fix status

- **Fix #1 — ✅ DONE + LIVE-BROWSER VERIFIED 2026-06-30 (uncommitted).** It took THREE fixes, not one,
  to make an effect-only island actually run its effect on serve:
  1. **Mis-tiering (the original fix #1).** `parser.rs`: new `is_effect_hook_call`
     (`useEffect`/`useLayoutEffect`/`useInsertionEffect`) sets `profile.side_effects=true` → promotes to
     **Tier C** via the existing side-effect branch (`parser.rs:483`).
  2. **Wrong hydration trigger.** The side-effect branch (`effects.rs:96`) gave below-fold effect
     islands `HydrationMode::OnInteraction` — fatal for a passive scroll bar that never receives an
     interaction. Changed to hydrate **eagerly**: above-fold `Immediate`, below-fold `OnIdle` (both map
     to the client's Idle trigger via `renderer_runtime::trigger_from_mode`). Regression test
     `effects::tests::side_effect_component_hydrates_eagerly_not_on_interaction`.
  3. **Script-load ordering race (the subtle one).** `client.js` is injected as an async-loaded
     external `<script src>` and defines `__ALBEDO_HYDRATE_ISLAND` only at the END of its run, but the
     ≤2KB bootstrap (`src/hydration/script.rs`) captured the entry once (early, undefined) and
     `requestIdleCallback` fired before client.js finished → island silently dropped (the old
     `OnInteraction` "worked" in earlier manual tests only because forced events fired long after
     client.js had loaded). Fix: bootstrap now reads the global **lazily at trigger time** and pushes to
     `globalThis.__ALBEDO_HYDRATE_QUEUE` when not ready; `client.js` (`assets/albedo-client.js`) drains
     that queue on load + installs a live-shim `push`. Bootstrap still ≤2048 B (size test green).
  **Verified live** (Halation `/about`, real `albedo serve` debug binary :3001, NO interaction
  dispatched): island `data-albedo-hydrated=true` on idle; progress bar tracks scroll **exactly
  proportionally (0%→0, 25→25, 50→50, 100→100)**; **zero `/_albedo/action` POSTs**; clean console +
  screenshot. Tests: 8 effects + 3 hydration::script green.
  - **Effect-only islands (no event handler) are now fully unblocked.** They have no events so
    `build_reactive_blocks` skips them → they fall through to the (now eager + race-free) hydration block.
  - ⚠️ **The async-composite gap is confirmed:** ReadingProgress mounted in the `async function Essay`
    route (`essays/[slug].tsx`) renders **inline into the Tier-B server HTML with no
    `data-albedo-island` wrapper** → never hydrates. So fix #1 was verified on the SYNC `about.tsx`
    route (ReadingProgress mounted there). Async-page islands are a separate gap (same family as
    layout-island reachability / fix #3 below).

## ✅ STALE-PREVIEW MYSTERY SOLVED (the multi-session time-sink)
It was NEVER a preview cache. The `halation` config in `A:\AlBDO-v-0.1.0\.claude\launch.json` runs
`A:\AlBDO-v-0.1.0\target\debug\albedo.exe serve --port 3001` — i.e. the **debug** binary. The preview
MCP reads the ROOT project's launch.json (not `A:\halation\.claude\launch.json`, which says 3211).
Testing used `~/.cargo/bin/albedo.exe`, so the preview served a pre-fix DEBUG binary while curl/install
saw the fixed one. **Durable workflow:** before `preview_start halation`, (1) `taskkill /F /IM
albedo.exe`, (2) `cargo build -p albedo-server --bin albedo` (debug), (3) rebuild the app with that SAME
binary: `cd /a/halation && A:/AlBDO-v-0.1.0/target/debug/albedo.exe build` — the hydration trigger is
baked into the app manifest at build time, so the app MUST be rebuilt with the fixed binary, not just the
server. Preview picks port 3001 (its own), reused:false each restart. Killing albedo orphans the preview
server ("No running servers") → just `preview_start` again.
- **Trigger refinement — ✅ DONE (this is fix #1's part 2 above).** Effect islands now hydrate eagerly
  (OnIdle/Immediate), no longer OnInteraction.
- **Fix #2 — TODO (next session).** `build_reactive_blocks` (`renderer_runtime.rs:289`) must NOT
  serve-wire a component that has effect hooks (needed for an event+effect island like ThemeControl — a
  theme toggle that also mutates `document` via useEffect; the binding-mode reactive descriptor has no
  notion of effects → its effect is dropped). Detect via the manifest effect flag or the component
  source (the renderer exposes it the way `build_hydration_blocks` reads `module_registry().module(path)
  .code`); if effects present → skip serve-wire (fall back to A3 hydration).
- **Fix #3 — TODO (next session, the real refactor).** The merge in `server.rs` ~L690 does
  `blocks.insert(path, reactive_block)`, clobbering the *whole route's* hydration block. Make the
  reactive/hydration merge **per-component** (union of placeholders + scripts), so one route can
  serve-wire some islands and hydrate others without one wiping the other.

## ✅ Layout-island reachability — DONE + LIVE-VERIFIED 2026-07-01 (uncommitted)
A Tier-C island mounted in a **layout** now ships + hydrates + anchors correctly on serve. Root cause
was: `build_route_manifest` fills `route.tier_c` via `traverse(root_component)` from the route *entry*;
layouts are rendered separately (`wrap_in_layouts`/`render_layout_html` with the `<children/>` sentinel)
and their island children were never collected → no hydration block. Fix (`src/manifest/builder.rs` +
`src/runtime/eval/core.rs`):
1. **`collect_layout_islands(layout_chain, …)`** (builder) — for each layout in the chain, DFS its render
   subtree (recurse Tier-A/B, stop at Tier-C boundary; `visited` guards cycles, `emitted` dedups across
   the chain) and lower each Tier-C island to a `TierCNode` with a sentinel `parent_placeholder =
   LAYOUT_ISLAND_PARENT` (so `collect_shell_placeholders` keeps it OUT of the `<children/>` slot). Returns
   the nodes + a `name → placeholder_id` map. Called in `build_route_manifest` after `layout_chain` is
   resolved; `tier_c.extend(layout_islands)`.
2. **Inline anchoring** — the island-skip branch in `core.rs` (`render_element`) used to `return
   Ok(String::new())` for a skipped island. New thread-local `LAYOUT_ISLAND_PLACEHOLDERS` (mirrors
   `install_island_skip_set`/`IslandSkipGuard`): while installed (only during layout render, via
   `wrap_in_layouts`), a skipped island emits its REAL `<div id="{ph}" data-albedo-tier="c"></div>`
   placeholder INLINE at its authored position (masthead/footer) — RAW, no escaping, because serve
   (`streaming.rs:716`) string-`.replace`s that exact div. Route islands still emit nothing (map not
   installed) → unchanged. `build_shell`/`wrap_in_layouts` gained a `&layout_island_map` param.
- **Live proof** (Halation, debug bin :3001): mounted proven `EffectToggle` in `routes/layout.tsx`
  masthead. On `/archive` + `/` (routes that import NO islands of their own — `/archive` is async Tier-B):
  island markup ships, `client.js` ships, the island sits **between `</nav>` and `</header>`** (DOM query
  `header.masthead .effect-toggle` = true), hydrates, `useEffect` runs on mount (`data-density=tight`) +
  re-runs on toggle (`tight↔loose`), **zero `/_albedo/action` POST**, console clean. Screenshot shows
  "density: tight [toggle density]" in the masthead.
- Tests: **419 lib** (new `collect_layout_islands_lifts_a_tier_c_island_from_a_layout`) + 122 server-lib +
  5 hydration + 8 reactive green. Halation fixtures: `EffectToggle` now in the root layout masthead.
- ✅ **Async-composite gap — CLOSED + LIVE-VERIFIED 2026-07-01 (uncommitted). See section below.**

## ✅ Async-composite gap — CLOSED + LIVE-VERIFIED 2026-07-01 (uncommitted)
A Tier-C island inside an `async function Page()` (Tier-B, e.g. ReadingProgress in `essays/[slug].tsx`)
now hydrates. **Root cause reframed (senior-architect pass, NOT patchwork):** there are two renderers
for the same tree and the *island boundary* was first-class in only one. The pure-Rust (Tier-A) renderer
knows the boundary (`install_island_skip_set` → emits the empty `<div id=… data-albedo-tier="c">` hole);
the **QuickJS (Tier-B/async) renderer had no concept of an island at all** — `h()` executes every
function component eagerly, so an island nested in an async page rendered inline as anonymous HTML with
no `data-albedo-island` marker → the client's `querySelector('[data-albedo-island=…]')` found nothing.
(Rejected the tempting patchwork: threading a name→id map into the pool + special-casing `h()` by
`type.name` — re-derives island identity by string-match, leaks across pooled engines, breaks on
arrow/renamed exports.)

**Fix = the RSC client-reference model.** An island reached from a *server* render context is not the
real component — it's a **client reference**: its module body is swapped (at boot, from the manifest —
the single tiering source of truth) for a stub that renders ONLY the framework's canonical empty island
placeholder and never runs island code. Both renderers then converge on ONE island representation, and a
single fill pass produces SSR content + marker for every island. Works cleanly because the pool engines
(Tier-B) are a **separate module graph** from the boot renderer that does standalone island SSR — so
stubbing islands in the pool graph doesn't touch the graph that actually renders them (= RSC's
server-graph/client-graph split, for free). 4 edits:
1. **Prelude primitive** (`src/runtime/quickjs_engine.rs`, inside the `h`-block so it closes over
   `AlbedoHtml`): `globalThis.__albedo_island_placeholder(pid)` → `new AlbedoHtml('<div id="…"
   data-albedo-tier="c"></div>')` — byte-identical to the pure-Rust hole (`eval/core.rs:3264`).
2. **Stub generator** `render::tier_b::island_client_reference_stub(placeholder_id)` → `export default
   (function __albedoIslandRef(props){ return globalThis.__albedo_island_placeholder("__c_…"); });`.
3. **Boot substitution** (`renderer_runtime.rs`): `island_client_reference_map` builds module_path →
   placeholder_id from every `route.tier_c` node; `add_component_to_plan` swaps any island dep's code for
   the stub (entry itself never stubbed — a Tier-B page is never an island). Island code thus never runs
   in the pool engines.
4. **Unified fill** (`handlers/streaming.rs`): extracted `replace_island_placeholders(html,
   placeholders)`; the shell fill (`build_shell_chunk`) AND the Tier-B success branch in `build_stream`
   both run it. So the empty hole inside the async page's `__albedo_inject(...)` payload gets replaced
   with the island's marked standalone-SSR HTML — async-page islands become byte-identical to Tier-A ones.
   (Bonus: reactive-mode async islands now fill too, since the pass applies whatever's in
   `hydration.placeholders`.)

The island was ALREADY in `route.tier_c` (traverse `builder.rs:842` collects a Tier-C child of a Tier-B
page with `parent_placeholder = the page's __b_ id`), so `build_hydration_blocks` already emitted its
IIFE + payload descriptor — the ONLY missing piece was the marker in the QuickJS HTML, which the
client-reference now supplies.

- **Tests (all green):** `dom-render-compiler` **420 lib** (+`quickjs_engine::…island_client_reference_stub_renders_empty_placeholder_in_async_page`
  — loads a stub + async page importing it, asserts the exact hole renders and island code doesn't run);
  `albedo-server` **124 lib** (+`tier_b::…island_client_reference_stub_emits_placeholder_call`,
  +`streaming::…replace_island_placeholders_fills_holes_in_tier_b_html`).
- **LIVE-VERIFIED** (Halation `/essays/on-the-glow-around-bright-things`, real `albedo serve` debug bin
  :3001): curl proof — inside the `__albedo_inject("__b_essay_14", …)` payload ReadingProgress ships as
  `<div class="progress-rail" … data-albedo-island="3"><div class="progress-fill" style="width:…"`, zero
  unfilled `data-albedo-tier="c"` holes, payload carries `"component_id":3`. Browser proof —
  `[data-albedo-island="3"]` present, `data-albedo-hydrated="true"`, `useEffect` `measure()` ran on mount
  (0%), scroll → fill width tracks **exactly proportionally (50→50%, 100→100%, 0→0%)**, **zero
  `/_albedo/action` POST** through all interactions, console clean.
- **Next:** move ReadingProgress from `essays/[slug].tsx` into `essays/layout.tsx` (its intended home —
  layout-island reachability now supports it; the layout.tsx comment there is stale), then the last P5
  island **MarginNote**.
- Deferred (not needed for correctness): the pure-Rust island-skip path is still name-based
  (`island_skip_contains`) — a future consolidation could put BOTH renderers on the client-reference
  representation, but the pure-Rust path works today.

## ✅ MarginNote (last P5 island) + binding-mode event-param bug — DONE + LIVE-VERIFIED 2026-07-01 (uncommitted)
MarginNote authored as the reading-shell composer (`A:\halation\src\components\MarginNote.tsx` — an
uncontrolled `<textarea>` whose `onInput={(e) => setCount(e.target.value.length)}` drives a live
`{count} / 280` readout), mounted in `essays/layout.tsx` (a LAYOUT island; the stale "not mounted here"
comment there is now corrected). CSS `.margin-note-composer` added to `styles.css` (champagne focus ring,
mono counter). So the essay route now carries **two islands via two reachability paths** — MarginNote
(layout island) + ReadingProgress (async-composite/client-reference island) — hydrating independently.

**Dogfooding MarginNote surfaced a real framework bug (fix "#4"): binding mode silently dropped the DOM
event argument.** `CompiledProject::build_client_handler_thunk` (`src/runtime/compiled.rs`) builds the
client thunk as `(function(__state,__emit){…})` — it binds state values, setters, and captured props but
NEVER the event. And `HandlerBody` (`src/transforms/events.rs`) had **thrown away the closure's parameter
entirely**. So `onInput={(e) => setCount(e.target.value.length)}` was serve-wired (it has a `{count}` text
binding + an event → eligible) but the thunk referenced a free `e` → the handler no-op'd: **count stayed
0, no console error.** The reactive runtime (`albedo-reactive.js`) even *has* the event (line ~258 passes
`ev` to `eventDispatcher`) but line ~142 called `thunk(state, emit)` and dropped it.

**Fix (the principled "binding mode declines what it can't represent" boundary — same shape as the
structural/list fallbacks, NOT patchwork):** thread the handler closure's event-param shape and decline
binding mode when the handler reads it, so it falls back to A3 hydration where the real closure runs with
the native event (A3 already handles this — client.js `applyProp` attaches `onInput`→`input` with the
real closure). Edits:
- `src/transforms/events.rs`: new `enum HandlerEventParam { None, Ident(String), Unsupported }` +
  `event_param_from_first_pat`; `HandlerExtract` gains `event_param`; captured for inline arrows/fns AND
  bare-ident locals (`LocalHandler { body, event_param }`, `handler_body_from_expr` now returns it).
- `src/runtime/compiled.rs`: `ResolvedHandler.event_param`; `build_client_handler_thunk` returns `Err`
  (→ A3) when the param is `Unsupported` (destructured) or `Ident(name)` AND the body's free idents
  contain `name`. A declared-but-unused param stays serve-wireable. Server `action()` handlers set
  `None` (they don't use the client thunk).
- **Deferred (documented, not needed for correctness): actually SUPPORTING the event in binding mode** —
  bind `var e = __event` in the thunk + forward the event from `albedo-reactive.js eventDispatcher`. That
  keeps the cheaper binding-mode path for controlled inputs (the #1 React interaction) instead of
  full-hydrating them. A3 handles them correctly today, so this is pure optimization.

- **Tests:** fixture `tests/fixtures/hook_compile/event_reading_handler/` + `reactive_bindings::
  event_reading_handler_falls_back_to_island` (asserts `build_reactive_payload` errors for the
  event-reader, and a parameterless handler on the same thread still serve-wires — the decline is
  specific, not a blanket poison). 420 lib + 124 server-lib + 9 reactive + 6/5/5 integration green.
- **LIVE-VERIFIED** (Halation `/essays/[slug]`, debug bin :3001): MarginNote is now an A3 island
  (`data-albedo-island` on `.margin-note-composer`, NO `__albedoReactive` boot — binding mode declined),
  `data-albedo-hydrated=true`, typing drives the count **11→42→0 proportional to input length**, **zero
  `/_albedo/action` POST**. ReadingProgress coexists on the same route (scroll → 0/25/50/100%). Console
  clean. 3 hydrated islands on the route (PreferencesPanel + ReadingProgress + MarginNote). ⚠️ preview
  gotcha: programmatic `window.scrollTo` doesn't emit a native `scroll` event in the backgrounded preview
  tab (and `clientHeight` can read 0) — dispatch `new Event('scroll')` explicitly to test scroll islands;
  and rAF can stall in a background tab (use `setTimeout`, not `requestAnimationFrame`, to flush).

**P5 islands are COMPLETE** (ReadingProgress, ThemeControl, EffectToggle, TypeScale, MarginNote all live).
Next flagship work is P6 (actions: margin-note + subscribe `action()` + zod) per `A:\halation\SPEC.md`.

## EXACT app state (2026-06-30 — fixes COMMITTED `3c29520` "Testing phase 1 with fixes…")
ALBEDO repo fixes are now committed in `3c29520` (tree clean): `src/parser.rs` (is_effect_hook_call), `src/effects.rs`
(eager trigger + regression test), `src/hydration/script.rs` (bootstrap lazy-read + queue),
`assets/albedo-client.js` (queue drain). Debug binary `target/debug/albedo.exe` rebuilt with all three;
`~/.cargo/bin/albedo.exe` is the OLDER fix-#1-only build (rebuild/`cargo install --force` it before
relying on PATH `albedo`).
Halation app:
- `src/components/ReadingProgress.tsx` — useState+useEffect scroll bar, string style.
- `src/routes/about.tsx` — **ReadingProgress IS mounted** here now (the verified sync witness). Mounting
  it added a Tier-C island to an otherwise Tier-A page. This is a test fixture — decide next session
  whether to keep it on /about or move it to the reading shell once layout-island reachability lands.
- `src/routes/essays/[slug].tsx` — ReadingProgress mounted but it's an ASYNC (Tier-B) route → renders
  inline, NOT islanded (async-composite gap). Doesn't hydrate there.
- `src/routes/essays/layout.tsx` — ReadingProgress NOT mounted (layout-island reachability gap).

## ✅ Fix #2 + #3 LANDED TOGETHER 2026-07-01 (uncommitted, code-verified — not yet live-dogfooded)
Both done in one pass; 418 lib + 120 server-lib + 5 hydration + 8 reactive_bindings + 2 new merge unit
tests green; debug bin relinks clean.
- **Fix #2 (effect signal in the CONTRACT, not re-parsed at serve).** Added `side_effects: bool` to
  `TierCNode` (`src/manifest/schema.rs`, `#[serde(default)]`), populated from
  `metadata.effect_profile.side_effects` at `src/manifest/builder.rs:893` (only real construction site;
  test fixture `src/budget/report.rs` got `false`). `build_reactive_blocks`
  (`crates/albedo-server/src/renderer_runtime.rs`) now `continue`s on `node.side_effects` BEFORE building
  the payload → effect-bearing islands (e.g. ThemeControl) are never serve-wired, fall through to A3
  hydration where `runEffects()` runs. (Chose contract field over source-scan per "fix the framework, no
  patchwork" — the tiering already computed `EffectProfile::side_effects`; don't re-derive at serve.)
- **Fix #3 (per-component merge replaces per-route clobber).** `build_hydration_blocks` gained a
  `claimed: &HashMap<String, HashSet<String>>` param and skips any tier_c node whose `placeholder_id` was
  already serve-wired. server.rs `build()` now: (1) builds reactive blocks FIRST (immutable borrow),
  (2) derives `claimed` from each reactive block's placeholder ids, (3) builds hydration for the rest
  (mutable borrow), (4) folds via new `pub(crate) fn merge_island_blocks(hydration, reactive)` — unions
  placeholders + concatenates closing_scripts per route. Disjoint placeholders (A3 skipped claimed) +
  independent script bundles (`__albedoReactive` driver vs `client.js`+IIFEs) → sound. Extracted the
  union into the named fn (with 2 unit tests `renderer_runtime::tests`) instead of burying it inline in
  the 100-line `build()`. The old `blocks.insert(path, reactive)` per-route overwrite is GONE.
- ✅ **LIVE-BROWSER VERIFIED 2026-07-01** (Halation `/theme`, real `albedo serve` debug binary :3001).
  New fixtures: `src/components/{ThemeControl,EffectToggle,TypeScale}.tsx` + `src/routes/theme.tsx`. One
  route, THREE islands, two strategies:
  - **Served HTML proof of fix #3:** `/theme` ships BOTH `__albedoReactive.boot` (TypeScale serve-wired,
    binding mode) AND `data-albedo-island` + `/_albedo/client.js` (ThemeControl + EffectToggle A3) — the
    old per-route `insert` clobber would have dropped one. Both runtimes load (`__albedoReactive` +
    `__ALBEDO_HYDRATE_ISLAND` both present).
  - **Fix #2 proof:** ThemeControl + EffectToggle (both `side_effects`) are A3 islands, NOT in the
    reactive boot set. Their `useEffect` ran on MOUNT (`data-theme=night`, `data-density=tight` set on
    `document.documentElement`) AND re-ran on every toggle (`night↔day`, `tight↔loose`). If serve-wired,
    the effect would've been dropped. ThemeControl's `useContext` also corrected the label from the
    `createContext` default `day` → Provider value `night` on hydrate, and re-rendered the consumer
    through the Provider on each toggle. TypeScale serve-wired stepped `0→1→2`. **Zero `/_albedo/action`
    POST across all interactions.** Console clean.
  - ⚠️ **Two gotchas hit + understood (NOT fix #2/#3 bugs):** (1) A **multi-child Fragment** client
    limitation — `<Ctx.Provider>` wrapping >1 direct child throws `multi-child Fragment is not yet
    reconcilable on the client` (`albedo-client.js singleFragmentChild`); wrap Provider children in ONE
    container element (the portfolio fixture already did). `data-albedo-hydrated` is set BEFORE the throw,
    so a failed hydrate still shows the flag — check console, not just the attr. (2) A3 re-render +
    effects commit **asynchronously** (microtask/rAF after the click), so a snapshot taken synchronously
    right after `dispatchEvent` reads STALE state — flush with a double-`requestAnimationFrame`+`setTimeout`
    before asserting. Both wasted a cycle here; remember them for the next island dogfood.
3. **Layout-island reachability** + the **async-composite gap** (both: walk layout/async island
   children into the hydration set). Then move ReadingProgress into `essays/layout.tsx` (its intended
   home) and verify on the async essay route.
4. **Finish the 3 P5 islands live**: ReadingProgress (reading shell), ThemeControl (useContext),
   MarginNote — each hydrate + scroll/click/input + zero `/_albedo/action` POST.
Workflow reminder: see "STALE-PREVIEW MYSTERY SOLVED" above — rebuild debug binary AND rebuild the app
with it before `preview_start`.
