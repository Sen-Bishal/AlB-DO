---
name: project_halation_flagship
description: "Halation — the purpose-built ALBEDO Gate-4 flagship app (literary journal, editorial/serif/shimmer aesthetic)"
metadata: 
  node_type: memory
  type: project
  originSessionId: d13d3386-fd05-4377-a577-3d73a21408b0
---

**Halation** (`A:\halation`, no git, sibling to `albedo-portfolio`) — the Gate-4 / Workstream-E
flagship, started 2026-06-28. A literary journal ("a journal of light & making") chosen *because*
editorial content authentically needs every ALBEDO server surface AND fits the user's aesthetic ask:
**non-corporate, serif, shimmer, italics, stylized everywhere.** Scaffolded via `albedo init halation`
(after rejecting `A:\quant-analysis\quant` — see [[project_quant_flagship_rejected]]). Living plan +
surface matrix + port-diff seed at `A:\halation\SPEC.md`.

**Aesthetic system (P1 — DONE + browser-verified 2026-06-28):** Fraunces (display, wonky italics,
`font-variation-settings` SOFT/WONK/opsz) + Newsreader (body italics), loaded via `@import` at top of
`src/styles.css` (ALBEDO inlines the concatenated CSS). Champagne-gold `.shimmer` text primitive
(animated gradient + background-clip), masthead light-sweep, drop-cap with halation glow. Warm
near-black `#0b0a09` / albedo-cream `#f4ece2`. Hand-written CSS, NOT Tailwind (ALBEDO's proven
global-CSS→prod path; Tailwind is still the conditional rung). Masthead wordmark + colophon live in
`src/routes/layout.tsx`; static cover in `src/routes/index.tsx`. Starter cruft removed (chat.tsx /
Hero / Counter). Verified on `albedo dev` :3000 — both comps Tier-A, 0 console errors, fonts load.
Preview registered in `A:\AlBDO-v-0.1.0\.claude\launch.json` as `halation` (port 3000).

**▶ CURRENT STATUS (2026-06-29):** P1 ✅ · P2 ✅ · P3 ✅ · **P4 ✅ DONE & verified** (uncommitted). **NEXT SESSION = P5** (islands: ReadingProgress useState+useEffect, ThemeControl via useContext, MarginNote). Then P6 actions (zod), P7 port-diff.

**P4 (boundaries) — ✅ DONE 2026-06-29 (uncommitted):** Added `src/routes/essays/loading.tsx` (champagne shimmer skeleton tracing the essay shape — kicker/title/dek/body lines; new `.skeleton*` CSS in `styles.css`, `prefers-reduced-motion` honored) + `src/routes/essays/error.tsx` ("This piece has dissolved into light", № 404 kicker, shimmer h1, back-to-issue escape link). Reworded root `routes/error.tsx` into a distinct journal-wide fallback ("The press has jammed"). **No engine work needed for discovery** — `src/routing/file_based.rs` already binds the NEAREST boundary (closest-to-leaf, longest-prefix; tested `nested_error_boundary_wins_over_root_boundary_closer_to_leaf`); manifest confirms `/essays/[slug]` → `EssayError`+`EssayLoading` while `/`,`/about`,`/archive` keep root `RouteError`. Verified: bad slug → clean boundary, valid essay 200 + correct `generateMetadata` title, no regression.

**P4 surfaced + FIXED a real engine defect — error-boundary message was leaking internals ([[project_error_loading_boundaries_gap]] cosmetic follow-up, now properly closed).** The reader-facing `error.tsx` dek showed the *full wrapped error chain* — `render registry failed for 'render::Essay': render registry failed… : RenderError: failed to render component 'A:\halation\src\routes\essays\[slug].tsx': <thrown>` — i.e. a **double "render registry failed" wrap + an absolute filesystem path** in end-user copy. Per user directive ("senior fix, not a bandaid — no string-peeling"), fixed **structurally**, threading the thrown message as typed data end-to-end (uncommitted): (1) `src/runtime/engine.rs` — new `RuntimeError::RenderComponentError { component, message }` (Display byte-identical to old, logs unchanged) + `thrown_message()` accessor; (2) `quickjs_engine.rs map_render_error` populates it (path → `component` field, no longer `format!`'d into the message); (3) `crates/albedo-server/src/render/tier_b.rs` — `RegistryFailure` gains `{thrown_message, diagnostic}`, a `Send` `ComponentRenderFailure` carrier threads both across the engine thread hop, `RenderError::user_message()` returns reader text, and **`render_tier_b` stops re-wrapping** the registry's already-typed error (kills the double-wrap); (4) `streaming.rs` boundary props use `err.user_message()` (logs/overlay still get full `Display`). Result (verified via direct curl on the rebuilt binary): dek = just `No piece is filed under "<slug>."`, 0 "render registry failed", no `[slug].tsx` path. 120 server-lib + 19 quickjs-engine + 8 tier_b tests green. ⚠️ Binary rebuilt+installed to `~/.cargo/bin/albedo.exe` ~20:25.

⚠️ **Preview-MCP staleness gotcha (this session):** the Claude preview server on port 3001 kept serving the PRE-rebuild binary even after `preview_stop` + `taskkill /F /IM albedo.exe` + fresh `preview_start` (it caches the binary/process resolved at its FIRST launch of the session, before any rebuild). Verify engine changes via **direct `albedo serve` + curl**, not the preview, after a mid-session rebuild. Bare `albedo` (PATH = `~/.cargo/bin/albedo.exe`) and explicit path both gave the correct clean output; only the preview was stale.

⚠️ **Separate pre-existing leak found (spawned as a background task, NOT P4):** the prod global-CSS inliner (`inject_global_css_into_shells` in `src/bin/albedo.rs`) prepends `/* A:/halation/src/styles.css */` — the **absolute project path** — into every served page's inline `<style>`. Minor info-disclosure, unrelated to the error fix.

**Build phases (SPEC.md):** P1 look ✅ · P2 content+async index (keyed `{essays.map}`) ✅ · P3 dynamic
`essays/[slug].tsx` async + `generateMetadata()` + nested layout ✅ · P4 loading/error boundaries ⏳ NEXT ·
P5 islands (ReadingProgress, ThemeControl via useContext, MarginNote) · P6 actions (margin-note +
subscribe, zod-validated, loud errors) · P7 polish + Next→ALBEDO port-diff writeup.

**P2 status (2026-06-28 — ✅ UNBLOCKED + RENDERING ON SERVE):** the dep-detection fix (A+B+C) landed
and is verified — `albedo build` reports 7 components incl. the `__module__essays` data node, the manifest
carries `Issue deps=[EssayCard, __module__essays]`, and served `/` renders the keyed essay list from
`content/essays.ts` stably (6+ requests, 200, no segfault). Full detail in [[project_dep_detection_gap]]
(now CLOSED). **NEXT = rebuild/install the RELEASE binary (verified on debug), then P3.** ⤵ historical:

**P2 status (2026-06-28 — written, NOT yet rendering on serve):** All P2 app code authored to senior
standard — `content/essays.ts` (typed `Essay[]` with `body` for P3, sync selectors), async
`routes/index.tsx` (feature + keyed `EssayCard` map) + `archive.tsx`, `about.tsx`, `error.tsx` (P4
bonus), `components/EssayCard.tsx`. Real essay bodies written (ALBEDO origin in metaphor). **Verifying
on a live serve surfaced TWO engine bugs** — see [[project_dep_detection_gap]]: (1) import-linking
specifier mismatch — **FIXED** (`__albedo_resolve_project`, verified, 437 tests green); (2) **THE
blocker:** ALBEDO wires deps by name/JSX-usage not by import path, so a route importing `content/essays.ts`
gets no dep edge → the data module never loads on serve → **segfault** under the scoped arena. (Async
server data was a complete red herring in the chase.) **User chose to fix it properly (A+B+C plan in
[[project_dep_detection_gap]]).** Resume there: implement the dep-resolution fix, rebuild release binary
(stale), then re-verify `/` renders the full keyed list stably. THEN P3.

**Content direction (user, 2026-06-28):** the essays are **about ALBEDO itself — how it reached this
state — told in metaphorical/literary terms**, NOT a changelog or feature list. Each piece is a veiled
telling of a real chapter of the build: tiering, the QuickJS request arena, binding-mode over
hydration, sub-ms TTFB / latency as a value, the loud-error doctrine, the honest harness, "rendered
then released." The existing P1 placeholder titles already lean this way ("The Compiler as a Reading
Glass", "Latency Is a Moral Quality", "Rendered, Then Released") — keep that voice; the journal *is*
ALBEDO's own origin story refracted through light/making metaphor. (Mind the personal stakes — keep it
dignified, not a pitch.) Write real essay bodies in P2/P3.

**The real engine work this flagship forces (the dogfood win):**
1. **`[slug]` params are empty on serve** — prod streaming handler matches routes by *exact path* →
   `params:{}` for dynamic routes (known limit from the Head/RSC work). The essay route needs the
   slug to load the right piece, so **closing dynamic-route param extraction on the serve path is P3's
   first task.**
2. **Keyed lists on real serve** — the issue index `{essays.map(...)}` is the first time that
   binding-mode rung runs outside the test harness.

Everything else (async RSC, generateMetadata, error/loading, action+zod, useContext) is assembling
already-proven surfaces. ⚠️ The PATH `albedo` binary is `0.1.0-alpha.1` (installed); for P3 engine
changes, rebuild `albedo-server` + `cargo install --force` (or point launch.json at a fresh target binary).
