---
name: project-phase-n
description: "Phase N (WARHEAD) — file-based routing, layouts, dynamic params, CSS modules, public/ static assets, docker/fly ship targets, vercel downgrade. Foundations + what's deferred."
metadata: 
  node_type: memory
  type: project
  originSessionId: b5632263-e10d-49fc-bce2-7ddf4430ee47
---

Phase N is the framework-primitives layer. Six deliverables shipped in one batch on top of `nuclearshiz`. **Commit**: `7811290` "phase N and O.1" (combined with O.1; 19 files, +2834 / −47).

**Why this memory exists:** Phase N adds a new compiler-side discovery surface (`src/routing/`), a new compile-time transform (`src/transforms/css_modules.rs`), a new server-side dispatch arm (`crates/albedo-server/src/handlers/public_assets.rs`), and rewrites the ship-target CLI surface. Several follow-ups are deliberately deferred — list at end.

## What landed

### N.1 — File-based routing (`src/routing/{mod.rs, file_based.rs}`)

- `discover_routes(routes_dir: &Path) -> Result<RouteDiscovery, RouteDiscoveryError>` walks `<root>/routes/`.
- Output: `Vec<DiscoveredRoute> { url_path, source_rel_path, layout_chain }` + `Vec<DiscoveredLayout>`.
- Path translation table:
  | File                       | URL path             |
  |----------------------------|----------------------|
  | `index.tsx`                | `/`                  |
  | `about.tsx`                | `/about`             |
  | `blog/index.tsx`           | `/blog`              |
  | `blog/[slug].tsx`          | `/blog/[slug]`       |
  | `blog/[...rest].tsx`       | `/blog/[...rest]`    |
  | `catalog/[[...slug]].tsx`  | `/catalog/[[...slug]]` |
- Existing `CompiledRouter::normalize_route_paths` already handles `[slug]` / `[...slug]` / `[[...slug]]`, so file-based discovery just preserves them.
- Skipped: names starting with `_`, hidden segments (any dir starting with `.`), non-{tsx,jsx,ts,js} extensions, `layout.tsx` (captured as layout, not route).
- Layouts compose root→leaf by directory depth; each `layout.tsx` applies to every route at or below its directory.
- Duplicate routes (same URL from two files) → `RouteDiscoveryError::DuplicateRoute`. Missing dir → `RoutesDirMissing`.
- Re-exported via `src/routing/mod.rs` → `dom_render_compiler::routing::*` via the new top-level `pub mod routing` in lib.rs.
- 12 unit tests in `file_based.rs`.

### N.2 — Dynamic params

Effectively free — `CompiledRouter::normalize_route_paths` shipped Next-style segment support in earlier sprints (see `crates/albedo-server/src/routing.rs` test_next_style_* cases). N.1's discovery just passes them through verbatim. No new router code; round-trip tests live in `file_based.rs`.

### N.3 — CSS modules (`src/transforms/css_modules.rs`)

- `scope_module_css(module_id: &str, css: &str) -> ScopedCssModule { scoped_css, class_map, hash_suffix }`.
- Hash via `xxh3_64(module_id)`, first 8 hex chars → suffix.
- Rewrites top-level `.classname` selectors to `.classname_<hash>`. `{...}` rule bodies, `/* ... */` comments, and quoted strings are passed through verbatim — the matcher is a hand-written scanner respecting CSS lexical context (no `csstree`/PostCSS dep).
- Covers: simple classes, pseudo-classes (`.foo:hover`), descendant/child combinators (`.a > .b`), comma-separated selector lists. Each unique class hits the map once.
- `is_css_module_path(path)` recognises `*.module.css`.
- 8 unit tests.
- Re-exported from `transforms::mod`.
- **Not done yet**: JSX-side rewrite of `styles.foo` accesses to the scoped class name. The scoping primitive is in; wiring through the QuickJS render path + collecting per-route inline `<style>` blocks is the next step. Hooks into `runtime/eval/core.rs::eval_jsx_element`.

### N.4 — `public/` static asset serving (`crates/albedo-server/src/handlers/public_assets.rs`)

- `PublicAssets::new(roots, cache_control)`; `resolve(url_path)` → first matching file across mounted roots; `read_response(path)` returns an axum `Response<Body>`.
- `sanitize_public_path(url)` rejects `/`, absolute paths, parent-dir traversal, NUL bytes, Windows drive prefixes. Pure function — tested independently.
- MIME via extension lookup table (svg, png, woff2, wasm, etc.).
- Builder additions on `AlbedoServerBuilder`:
  - `with_public_dir(dir)` — stackable, first matching root wins
  - `with_public_cache_control(value)` — overrides the dev/prod default
- `RuntimeState.public_assets: Option<Arc<PublicAssets>>`. Dispatch arm sits in `crate::server::dispatch` **before** route matching, only for GET/HEAD. HEAD returns headers with an empty body.
- Default Cache-Control: `no-store` when dev mode is on, `public, max-age=3600` otherwise.
- `AlbedoServer::public_assets()` accessor for tests/userland introspection.
- 6 integration tests in `crates/albedo-server/tests/public_assets_end_to_end.rs` + 4 unit tests.

### N.5 — `albedo ship --target docker`

- Rewrote `configure_ship_docker` to emit a **multi-stage** Dockerfile:
  - Stage 1 (`rust:1-bookworm AS builder`) — `cargo build --release --bin albedo` (when no prebuilt) then `albedo build .`
  - Stage 2 (`debian:bookworm-slim AS runtime`) — copies binary + `.albedo/dist` + `public/`, sets `ALBEDO_SERVER_HOST=0.0.0.0`, `ALBEDO_SERVER_PORT=3000`, `EXPOSE 3000`, `HEALTHCHECK` via wget on `/`
  - `CMD ["sh", "-c", "albedo serve --dir dist --host $ALBEDO_SERVER_HOST --port $ALBEDO_SERVER_PORT"]`
- `build_docker_template()` / `build_dockerignore_template()` / `build_fly_toml_template(app_name)` extracted as testable pure functions.
- Fly.toml gains `[env]` block + `[[http_service.checks]]`.

### N.6 — Vercel ship target downgraded

- `configure_ship_vercel(_)` returns `Err("vercel is not a supported ship target — Vercel's runtime does not execute Rust binaries. Use --target docker (or --target fly) to deploy the binary + dist.")` — no vercel.json emitted.
- `parse_ship_target("vercel")` still returns `Ok(ShipTarget::Vercel)` so the rejection message lands at dispatch (specific) rather than at flag parsing (generic "unknown target").
- All four completion scripts (bash/zsh/fish/powershell) and `print_ship_help` updated to drop `vercel` from the target list.

## Files touched

**New**:
- `src/routing/{mod.rs, file_based.rs}`
- `src/transforms/css_modules.rs`
- `crates/albedo-server/src/handlers/public_assets.rs`
- `crates/albedo-server/tests/public_assets_end_to_end.rs`

**Modified**:
- `src/lib.rs` (added `pub mod routing`)
- `src/transforms/mod.rs` (added css_modules)
- `src/bin/albedo.rs` (docker/fly/vercel rewrites + completion updates)
- `crates/albedo-server/Cargo.toml` (added tempfile dev-dep)
- `crates/albedo-server/src/lib.rs` (re-export `PublicAssets`; fixed broken doctest)
- `crates/albedo-server/src/server.rs` (builder + dispatch wiring)
- `crates/albedo-server/src/handlers/mod.rs` (re-exports)

## Test delta

+35 tests, all green. Workspace baseline pre-N was 261 lib tests; post-N (before O.1) was 261 + 18 of those landed in O.1. Pre-existing `AlbedoServer::builder()` doctest was broken and silently fixed (now uses `AlbedoServerBuilder::new(AppConfig::default()).build()?.run().await?`).

## What's intentionally NOT done in N

- **JSX-side `styles.foo` → scoped-classname rewrite.** Scoping primitive is in; QuickJS-side rewrite is the natural follow-up.
- **`DevConfig.routes` auto-population from `src/routes/`.** `discover_routes` is callable, but I did not replace the existing config-driven `routes` map — that would break existing scaffolds. CLI-ergonomic follow-up.
- **Per-route inline `<style>` injection.** Waits on the JSX rewrite.
- **`public/` copy step inside `albedo build`.** The runtime serves the dir directly; the build emit hasn't been taught to copy it into `.albedo/dist/` yet. Means dev/run works but a pure-static `dist/` doesn't carry `public/` assets without a manual copy.
- **`albedo init` scaffold of `src/routes/`.** Scaffold still writes `src/App.tsx` as the entry; file-based-routing demo project is a future addition.

## Lessons worth keeping

### File-based routing reuses the router, doesn't replace it

The temptation was to write a new path converter. Wrong — `CompiledRouter::normalize_route_paths` already handled Next-style segments. File-based discovery just emits the same `[slug]` / `[...slug]` strings the router expects. Total new routing code in the runtime: zero.

### `public/` dispatch must come BEFORE the route matcher

If you put public-asset lookup after the matcher, a catch-all like `/[...rest]` swallows `/logo.svg` before it can resolve. The arm sits just after the action-route arm so `/_albedo/*` still wins, but before `state.router.match_route`. GET/HEAD only — other methods fall through and get the router's 405.

### Vercel target rejection at the dispatcher, not the parser

`parse_ship_target("vercel")` still succeeds. `run_ship_command` matches `ShipTarget::Vercel` and calls `configure_ship_vercel` which returns the specific "Rust binaries don't run on Vercel; use --target docker" error. This gives the user the actual reason instead of a generic "unknown target — try docker, fly, static" — important for an option the docs / muscle memory still suggest is valid.
