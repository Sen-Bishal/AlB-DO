---
name: project-a4-userland-boundary
description: "A4 'userland boundary' (Gate 2) fully closed 2026-06-20 — global CSS in prod, dev/prod layout parity, JSX whitespace fidelity. All three verified dev+serve on a fresh albedo init app."
metadata:
  node_type: memory
  type: project
  originSessionId: a4-finish-2026-06-20
---

# A4 "userland boundary" — CLOSED (2026-06-20)

Finished all three A4 items from [[project_dogfood_portfolio]] in one session. All verified end-to-end on a fresh `albedo init` scaffold app (build + dev server), and the full `cargo test` suite is green (30 test binaries). **Uncommitted** (user owns commits). Three intentional source files touched: `src/runtime/eval/component.rs`, `src/bin/albedo.rs`, `src/dev/contract.rs`.

## 1. CSS → production (the dogfood blocker)
**The Gate-1-D "root cause fix" was a red herring.** `build_assets_manifest`'s CSS-check reorder operates on components, but `ProjectScanner::is_component_file` (`src/scanner.rs`, asserted at the `style.css` test) **rejects `.css`** — CSS files are *never* scanned as components, so `assets.css` is structurally always `[]`. That whole path cannot ship global CSS.
**Real fix:** `inject_global_css_into_shells` in `src/bin/albedo.rs` — at build time, walk the source tree (reusing the dev `collect_css_bundle` logic via a new `collect_css_bundle_filtered`, excluding `.module.css`) and inline a `<style data-albedo-global-css>` block into every route shell's `doctype_and_head` **before** `render-manifest.v2.json` is serialized. This mirrors exactly what `albedo dev` already inlines → dev/prod ship identical CSS. `.module.css` still flows through the existing scoped per-route injection (`collect_scoped_module_css_for_route`). Build now prints `global css · inlined into N routes`.

## 2. Dev/prod layout parity
`albedo dev` never composed `routes/layout.tsx` (composition was build-only via `wrap_in_layouts`). Fix: `ResolvedDevContract` gained `route_layouts: HashMap<url, Vec<entry-rel>>` (outermost→leaf), populated from `discover_routes().routes[].layout_chain` in `src/dev/contract.rs`. New `compose_dev_layouts` (`src/bin/albedo.rs`) renders each layout via `render_entry_with_broadcast` and substitutes the `LAYOUT_CHILDREN_SENTINEL` innermost-first (same contract as `wrap_in_layouts`), wired into BOTH `render_single_dev_route` (on-demand) and `render_all_routes` (cached; closure now takes `url`). Dev now serves `<div class="app-shell">` + nav + footer, sentinel substituted not leaked.

## 3. JSX whitespace fidelity
`normalize_jsx_text` (`src/runtime/eval/component.rs`) dropped BOTH boundary whitespace runs whenever the text contained ANY newline → `\n  to see <code>` became `to see<code`. Rewrote to React's real rule: inspect only the leading/trailing whitespace *runs* — drop a run only if it itself contains a newline (indentation adjacent to a tag), else preserve as one space. Also pure-whitespace nodes now keep a single space unless they span a newline (`{a} {b}`). Covers the Tier-A pure-Rust render path (what static prose uses). The QuickJS Tier-B/C JSX→`h()` transpile path has separate whitespace handling, not exercised by this papercut — left as-is.

## Verification
Fresh `albedo init a4app`; `albedo build` → manifest shells carry `data-albedo-global-css` + CSS vars + `app-shell` layout + `to see <code` (space preserved); `albedo dev` → same layout + CSS + whitespace. Parity confirmed. The TODO.md A4 bullets + the "A4 parity" verification line are all `[x]`.

## ⚠️ Cost incurred this session — see [[feedback_never_global_cargo_fmt]]
I ran a global `cargo fmt` which rewrapped comments to width 100 (per the repo's own `rustfmt.toml`) across ~100 files. Restored the ~79 previously-committed-clean files to HEAD and re-applied my 3 files cleanly, BUT the ~19 prior-work `.rs` files from earlier sessions (the binding-mode ladder etc., still uncommitted) retain comment-rewrapping churn I could not separate from their real edits (no session-start snapshot). **No code lost — semantics identical, tests green — but those files' uncommitted diffs now carry cosmetic comment churn.** Flagged to the user.
