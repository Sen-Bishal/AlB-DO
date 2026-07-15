---
name: project_preferences_panel
description: "Halation app-wide reader preferences (theme/size/density) built on layout-island reachability, plus the new ALBEDO `src/head.html` pre-paint head-partial convention that kills theme FOUC"
metadata: 
  node_type: memory
  type: project
  originSessionId: e8157bff-9e37-4cc8-9c0e-d9e8ae45bf89
---

# PreferencesPanel — app-wide theme / type-size / density (Halation), 2026-07-01

Built the full reader-preferences feature on Halation and DONE + LIVE-VERIFIED (uncommitted). It's the
payoff of [[project_p5_useeffect_hydration_gap]] (layout-island reachability) and forced one genuinely
new ALBEDO framework capability (a pre-paint head partial).

## What shipped (Halation app)
- `src/components/PreferencesPanel.tsx` — Tier-C island (useState+useEffect+onClick → `side_effects` →
  fix #2 keeps it A3-hydrated). Three cycle buttons: theme `dark↔light`, size `small/medium/large`,
  density `comfortable/compact/spacious`. Writes `data-theme`/`data-size`/`data-density` onto `<html>` +
  persists `localStorage['halation:prefs']` **in the click handler** (NOT an effect — avoids re-applying
  defaults on mount). Mount effect only READS the attributes (set by the bootstrap) to sync its labels.
- Mounted in `src/routes/layout.tsx` masthead → a LAYOUT island → appears app-wide (replaced the
  `EffectToggle` reachability witness).
- `src/styles.css` — already ~65 `var()` tokens so this was mostly token work: promoted `font-size`,
  line-heights, para-gap to tokens; added `[data-theme="light"]` cream-on-ink palette (+ light-mode
  `.shimmer` wordmark and `::selection` overrides so the near-white sweep/selection still read on cream),
  `[data-size=small/large]` (`--font-size` on `<html>` → rescales the whole rem layout), `[data-density=
  compact/spacious]` (leading/measure/para-gap), `--btn-ink` (constant dark text on the gold accent),
  `.prefs`/`.prefs-btn` masthead styling, and a 320ms `body` background/color cross-fade.
- `src/head.html` — the pre-paint bootstrap (see below).

## NEW FRAMEWORK CAPABILITY — `src/head.html` pre-paint head partial
The flash-of-default-theme problem: islands hydrate on idle, well AFTER first paint, so a hydration-time
effect would paint the default theme then snap. Fixed with a blocking head script that runs before paint.
ALBEDO had no API for an app-authored head script, so added one:
- **`inject_head_partial_into_shells(manifest, root)` in `src/bin/albedo.rs`** — mirrors the existing
  `inject_global_css_into_shells`. Reads `<root>/src/head.html` (fallback `<root>/head.html`); if present,
  injects its RAW contents verbatim into every route shell's `<head>`, right after `<meta charset>`,
  idempotent via a `<!--albedo:head-partial-->` marker. Called in the prod build flow right after the CSS
  injection (before manifest serialization). Logs "head partial inlined into N routes".
- Chose `.html` not `.js` because the scanner treats `.js`/`.ts`/`.jsx`/`.tsx` as components
  (`scanner.rs is_component_file`) → a `src/head.js` would be mis-parsed. `.html` is ignored by both the
  component scanner and the CSS bundler, and is more general (app can inline script/meta/link).
- Halation's `src/head.html`: a blocking IIFE that reads `localStorage['halation:prefs']`, falls back to
  `prefers-color-scheme` then dark, and stamps the three `data-*` attributes on `<html>`. The panel and
  the bootstrap share the same storage key + attribute contract.
- ⚠️ **Prod-only** — the injection is in the `albedo build` CLI path (like the CSS inject), so `albedo
  serve` (reads built dist) gets it. `albedo dev` does NOT inject the partial yet → dev would still FOUC.
  Dev parity is a follow-up (same gap class as the A4 dev/prod layout parity work).

## Live proof (Halation, debug bin :3001)
Build logged "head partial inlined into 5 routes". Served `/archive` (imports no islands): bootstrap in
`<head>` after charset, before `<body>`; panel ships in masthead; `client.js` loads. Behavior (rAF-flushed,
then 500ms settle for the theme cross-fade): theme `dark→light` flips body bg `rgb(11,10,9)→rgb(243,235,221)`
(cream) + text to warm-black, and back; size `medium→large` rescales `<html>` 19px→21px instantly; density
→ compact/spacious. All persisted to `localStorage`, **zero `/_albedo/action` POST**, console clean.
Reloaded `/` with stored light/large/spacious prefs → all applied, bg already settled cream with NO
transition crawl = painted correct from frame one (the panel's mount effect only READS, so the on-load
`data-theme=light` can only be the bootstrap) → **no FOUC**. Screenshot = a genuinely handsome cream
light mode, deep-amber wordmark, "◐ LIGHT / TYPE · LARGE / DENSITY · SPACIOUS" controls in the masthead.

## Gotchas re-confirmed
- A3 re-render + effects commit async (rAF) AND the 320ms theme transition means a synchronous post-click
  snapshot reads stale/mid-transition values — settle >transition before asserting computed colors.
- The transition only animates on in-page toggle (same document, attr change); hard nav + the pre-paint
  bootstrap means a fresh document paints the right theme with nothing to fade from → no nav flash.

## Remaining for a "ship it" version
1. **Dev parity** for the head partial (`albedo dev` should inject `src/head.html` too).
2. The PreferencesPanel SSR labels show defaults until hydrate (cosmetic, same as the useContext label
   correction) — fine.
3. Still-open upstream P5 gap: **async-composite** (Tier-C island inside an `async function Page`); not
   needed for this feature (the panel is a layout island, fully working).
