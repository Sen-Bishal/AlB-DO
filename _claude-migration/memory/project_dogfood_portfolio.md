---
name: project-dogfood-portfolio
description: "Dogfood run: rebuilt A:\\albedo-portfolio via `albedo init`; surfaced 3 unowned production gaps now tracked as A4 'userland boundary' in TODO.md. The report on limitations/perf/feature-coverage vs ENDGAME."
metadata:
  node_type: memory
  type: project
  originSessionId: dogfood-portfolio-2026-06-18
---

# Dogfood portfolio + A4 "userland boundary" (2026-06-18)

Rebuilt `A:\albedo-portfolio` properly via `albedo init albedo-portfolio --force` (the [[feedback-use-albedo-init]] rule), inspected the scaffold, then ported Bishal Sen content on top (Hero/Endorsements/ProjectTabs — the binding-mode islands from [[design_tier_classification]]). **Works live in `albedo serve` :3113** (launch.json `albedo-portfolio` config now points at `serve`). Building a real app on the mid-sprint **debug** binary surfaced 3 gaps the ENDGAME plan never named — now tracked in `TODO.md` as **A4 "userland boundary"** (Gate 2) + a Gate 1 D extension. **All findings + the fix are uncommitted (user owns commits).**

## The 3 gaps (now in TODO.md)
1. **CSS → production is BROKEN (A4, Gate 2).** `albedo build`/`serve` ships **zero CSS**. `build_assets_manifest` (`src/manifest/builder.rs`) only adds a file to `assets.css` if the CSS file is a **registered graph component** — never happens → `assets.css` always `[]` → no stylesheet link in `doctype_and_head`. The scaffold's "concatenate every `.css` under root into inline `<style>`" is **dev-only** (`collect_css_bundle` in `src/bin/albedo.rs`). **This corrects the earlier [[feedback-use-albedo-init]] root-cause:** the unstyled portfolio was NOT just a hand-rolling artifact — production CSS genuinely doesn't work. Only working path today = drop `styles.css` in `public/` + hand-write `<link rel="stylesheet" href="/styles.css">` in the layout (what the portfolio does now).
2. **Dev/prod parity — layouts (A4, Gate 2).** `albedo dev` does NOT compose `routes/layout.tsx`: `render_single_dev_route`/`render_all_routes` (`src/bin/albedo.rs`) call `render_entry_with_broadcast` directly; `wrap_in_layouts` (`src/manifest/builder.rs`) is **build-time only**. Dev renders bare route (no nav/footer), prod renders wrapped → two different documents. Dev server lies about layout. Hard prerequisite for the Workstream E demo.
3. **JSX whitespace fidelity (A4, minor).** Trailing space before an inline element collapses: `at the <span>speed</span>` → `at thespeed`. Needs manual `{" "}`. React preserves it.

## The build-path silent-failure bug (FIXED, uncommitted) — Gate 1 D extension
`infer_routes_dir` (`src/manifest/builder.rs`) used `?` inside a `for` loop over `components.values()` (a `HashMap` — **nondeterministic order**). When a non-route component (`Hero.tsx`) sorted first, route discovery was **silently skipped for the whole build** → `layout_chain: []`, no nav/footer, **NO error** (`✓ built in 405ms`). This is the exact "silent-wrong is the core enemy" disease the plan names, but in the BUILD path (the loud-errors doctrine was only applied to the JS evaluator). Fixed this session: `?` → `let-else { continue }`. TODO.md Gate 1 D now carries a "silent-failure sweep on the build/manifest path" item to audit the whole class.

## The report (limitations / perf / synthesis vs ENDGAME)
User asked for a blunt report. Verdict:
- **Limitations:** the hard/novel things (compiler tiering, binding mode, QuickJS executor, arena) are DONE and working in a real app — the moat is real. The gaps are the *boundary layer* (CSS-prod, dev/prod parity, JSX whitespace) — easy to build, easy to forget, unowned, and the FIRST things a user/funder hits.
- **Perf: it has NOT degraded — it was never substantiated, and the speed-ups are deliberately deferred.** Observed dev GET ~1.8–1.9ms / build ~400–485ms, but: (a) **debug binary** (`target\debug`, 10–50× slower than `--release`); (b) Movements I/II/V (codec, thread-per-core, PTHash) NOT in path yet (sequenced after the harness, per plan); (c) the 8µs claim excluded HTTP framing; (d) **Workstream C harness doesn't exist**, so we literally cannot measure prod serve end-to-end. The 0.05–0.10ms is still a target, not a regression. Building the demo mid-development on a debug build is "benchmarking a house with the framing up."
- **Synthesis:** plan is sound, execution order is right (body before soul, correctness before perf). But it has a blind spot at the userland boundary, and the perf story is unprovable until C exists. Recommended (and partly actioned): add A4 workstream ✅, pull C earlier, run Gate 1 D's silent sweep on the build path ✅ (tracked).

## Next session = full-fledged development
Recommended openers: **(1) A4 CSS-prod pipeline + dev/prod layout parity** (unblocks every demo, cheap), **(2) Workstream C harness in `--release`** (so perf claims become showable). **Reminder: STOP preview / `taskkill /F /IM albedo.exe` before `cargo build/test`** (binary held open). Binary: `cargo build -p albedo-server --bin albedo`. Portfolio rebuild: `albedo build` in `A:\albedo-portfolio` then preview `albedo-portfolio` :3113. Still uncommitted (user owns commits): `infer_routes_dir` fix in `src/manifest/builder.rs`, `TODO.md` A4 edits, `.claude/launch.json` portfolio→serve, and the portfolio app itself.
