---
name: project_tierb_serve_path_fixes
description: "Two serve-path bugs found dogfooding Halation after the dep fix unblocked the Tier-B index — inject-ordering + asset cache staleness, both fixed 2026-06-28"
metadata: 
  node_type: memory
  type: project
  originSessionId: 20c04970-a527-4e49-83f5-a22ad6b99b69
---

Booting the Halation flagship on a real `albedo serve` (after [[project_dep_detection_gap]] unblocked the
async Tier-B issue index) surfaced **two more serve-path bugs**, both fixed + browser-verified 2026-06-28 (uncommitted).

## Bug 1 — Tier-B injection ordering (blank Tier-B body)
**Symptom:** Tier-A pages (about/colophon) rendered fully, but Tier-B routes (the issue index, archive) had a
**blank `<main>`** in the browser even though the server response was complete (full keyed essay HTML present, 14.9 KB).
**Root cause:** Tier-B HTML is delivered as `<script>__albedo_inject("__b_<id>", "<main>…</main>")</script>` — a
**classic inline script** (runs during parse) — but `__albedo_inject` is defined in `/_albedo/runtime.js`, a
`type="module"` script (**deferred**, runs after parse). No early stub existed, so the body's inject call fired
before the function existed and was **lost** (not even queued; the runtime's internal `queue` only helps calls made
*after* install when the placeholder isn't in the DOM yet). Placeholder stayed empty.
**Fix (2 edits):**
- `src/manifest/builder.rs` — new `TIER_B_INJECT_BOOTSTRAP` const: a **classic** `<script>` pushed into the shell
  `<head>` (first thing, before `</head>`) that defines buffering `__albedo_inject`/`__albedo_hydrate` stubs which
  push args into `window.__ALBEDO_INJECT_QUEUE` / `__ALBEDO_HYDRATE_QUEUE`.
- `assets/albedo-runtime.js` `installLegacyHtmlInjector` — after installing the real handlers, **drains** both
  queues (replays buffered calls in order, nulls the queue).
**Verified:** clean load on a fresh origin → `<main>` present (1072px), `queueDrained:true`, all 6 essays + keyed
`{essays.map}` contents list render with zero intervention, clean console. Tests: `tier_b_inject_bootstrap_is_a_classic_stub…`
+ `shell_head_carries_tier_b_inject_bootstrap_before_module_runtime` (lib) + `embedded_runtime_drains_inject_queue` (server).

## Bug 2 — framework asset cache staleness (deploy drift)
**Symptom:** after rebuilding the runtime, the browser kept running the OLD `runtime.js` (blocked verification).
**Root cause:** `crates/albedo-server/src/handlers/albedo_assets.rs dispatch_albedo_asset` served the in-binary
framework JS (`/_albedo/runtime.js`, `client.js`, `link-forms.js`, `hydration.js`, `bincode.js`, `wt-bootstrap.js`)
with `Cache-Control: public, max-age=3600` and **no ETag**. These URLs are **fixed / non-content-hashed**, so a binary
rev that changes the bytes keeps the same URL → browsers serve a **stale client runtime for up to an hour** after a
deploy (drifting from the server). The code comment even *claimed* the bytes were "content-hashed via the build id" —
**false** (only `/_albedo/chunks/<name>.<hash>.js` are hashed; those may stay immutable).
**Fix:** serve these framework assets with `Cache-Control: no-cache` (revalidate). Test:
`dispatch_marks_framework_assets_no_cache`. **Caveat:** a browser that already cached the old response under its
`max-age=3600` won't revalidate until it expires — verify changes on a **fresh origin/port** (used port 3001).

## State
All uncommitted. Debug `albedo` binary rebuilt with both fixes; Halation rebuilt; preview launch config (`halation`)
points at the debug binary `serve` on **port 3001**. 415 lib + 120 albedo-server-lib + manifest/asset suites green.
RELEASE binary still needs `cargo build --release … && cargo install --force` for the PATH `albedo`. See
[[project_halation_flagship]], [[project_a3_client_hydration]] (the Tier-C runtime that owns `albedo-runtime.js`).
