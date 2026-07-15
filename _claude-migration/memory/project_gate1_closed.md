---
name: project-gate1-closed
description: "Gate 1 ('normal TSX runs, or errors loudly') fully closed 2026-06-20 via the build-path silent-failure sweep in src/manifest/builder.rs — 4 fixes, one of which is the A4 CSS-prod root cause."
metadata:
  node_type: memory
  type: project
  originSessionId: gate1-sweep-2026-06-20
---

# Gate 1 closed (2026-06-20)

Finished the last open Gate 1 item from [[project_dogfood_portfolio]] — the build-path silent-failure sweep. Audited `src/manifest/builder.rs`, `crates/albedo-server/src/routing.rs`, `src/bundler/npm.rs`, `src/bin/albedo.rs` for the `?`/`unwrap_or_default()`/`continue`-swallows-a-real-failure disease (the class the `infer_routes_dir` bug exemplified). **All findings were in `builder.rs`** — routing.rs/npm.rs/albedo.rs were clean (their `?` usages fail loudly at function scope, which is correct; the rest were benign string-parsing or fire-and-forget IO).

## 4 fixes landed (uncommitted — user owns commits)
1. **CSS-asset discovery bug — the actual A4 root cause.** `build_assets_manifest` checked `component.file_path.ends_with(".css")` for inclusion in `assets.css` *after* an early `continue` that fires whenever `self.metadata.get(&component.id)` is `None`. CSS files never get tier metadata (they're not JSX components), so every CSS file was unconditionally skipped before the check ever ran. `assets.css` was permanently `[]` regardless of what CSS existed. Fix: moved the CSS-extension check above the metadata gate. **Root cause of the portfolio app's "zero CSS in prod" finding is now fixed**, though unverified end-to-end (see caveat below).
2. `wrap_in_layouts` silently `continue`d past a layout that failed to resolve/render, dropping it with no log. Added `tracing::warn!` (target `albedo.manifest.layout`).
3. `render_static_component_html`'s fallback to the tag-stripped text placeholder was silent. Added `tracing::warn!` naming the component (target `albedo.manifest.render`).
4. `build_compiled_render_project` / `build_static_render_project` discarded the real `Result` error from `CompiledProject::wrap` / `ComponentProject::load_from_dir` via `.ok()?`. Both now `match` and log the error via `tracing::warn!` (target `albedo.manifest.build`) before falling back.

All 30 `manifest::*` tests green after the change; no happy-path behavior changed, only failure paths now log (or, in fix #1's case, stopped failing).

## Caveat — A4 CSS item is NOT fully closed
Fix #1 removes the dead-code path but two things are still unverified per [[project_dogfood_portfolio]]'s acceptance criteria:
- the emitted CSS entry is the raw relative `file_path`, not a hashed `/_albedo/…css` asset — may still need that pass.
- haven't re-run `A:\albedo-portfolio`'s build to confirm a `<link rel="stylesheet">` actually lands in `doctype_and_head` and the manual `public/styles.css` workaround can be dropped.

TODO.md A4's CSS bullet is marked `[~]` (partial) reflecting this — verify against the portfolio app before checking it off for real.

## State / next session
- **Gate 1 is now fully closed** in `TODO.md` (all items `[x]`).
- Still uncommitted on top of `7b424b4 gate`: everything from [[project_dogfood_portfolio]] (A1 bridge, A3 hydration, binding-mode ladder, client hooks, Head slices 1+2, the portfolio app) **plus** this session's 4 builder.rs fixes and the TODO.md edits. User owns commits — do not stage/commit.
- **Recommended opener next session:** A4 — verify the CSS fix against the portfolio app (the user said they don't want to work on the portfolio app *this* session, so this was deferred), then dev/prod layout parity, then JSX whitespace. After A4, Workstream C harness in `--release`.
- Reminder: `taskkill /F /IM albedo.exe` before `cargo build/test` (binary held open blocks relink).
