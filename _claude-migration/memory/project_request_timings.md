---
name: project_request_timings
description: Live per-request server-compute timings (ns/µs) printed in the terminal for albedo dev/serve
metadata: 
  node_type: memory
  type: project
  originSessionId: 7d84eab4-8455-499a-88e6-960127de6fdb
---

Per-request **server-compute timing** printed to the terminal for `albedo dev`
AND `albedo serve` — the ENDGAME "publish the number, not the adjective" doctrine
made live in the CLI. Added 2026-07-02, uncommitted, live-verified.

**What it shows:** one champagne-gold line per handled request, e.g.
`▸ GET  /archive   126.0 µs` / `▸ POST /_albedo/action   248.0 µs`. Warm reloads
on Halation hit ~34 µs. Formatter picks the smallest ALBEDO-scale unit (ns < 1µs,
µs < 1ms, ms above) so the sub-ms band never degrades to `0.00 ms` spam.

**Design (the honest-number gate):**
- New module `crates/albedo-server/src/timing.rs` — palette mirrors
  `src/bin/albedo/printer.rs` (ACCENT 179 / ACCENT_SOFT 223 / MUTED 245), pads
  PLAIN then colorizes (the ANSI-width lesson), NO_COLOR respected, 4 unit tests.
- Gated behind builder flag `AlbedoServerBuilder::with_request_timings(bool)`
  (default false → library/test embedders stay silent); `boot_production_server`
  flips it true, so BOTH dev and serve get it (they share that boot path).
  Stored as a persistent `RuntimeState.request_timings` field — NOT in the
  swappable `RenderWorld`, so a dev hot-swap keeps it. See [[project_dev_serve_unification]].
- Instrumented `dispatch_inner` (server.rs) at exactly 3 sites: streaming page
  GET, `execute_route` page GET, and action POST. `Instant::now()` captured at
  the top (includes routing — the perfect-hash matcher is ours), read back only
  at those 3 returns. Deliberately NOT timed: static/public assets, framework JS
  (`/_albedo/*.js`), dev SSE overlay/HMR, WT transport, inspector, 404/405 — so
  the log is pure ALBEDO numbers, zero browser/network noise (the explicit ask).

**Verified:** `albedo serve --port 3939` on Halation → GET `/` cold 80.7µs / warm
34µs, `/archive` 126µs, POST action 248µs (a synthetic CSRF-rejected 400 still
prints — timing is the compute span regardless of outcome). Ties to
[[project_c_harness]] (that's the offline harness; this is the always-on live view).
