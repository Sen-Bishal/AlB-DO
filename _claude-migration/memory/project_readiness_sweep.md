---
name: project_readiness_sweep
description: Codebase readiness sweep (STRATEGY.md Decision 3) — Pass 0 + Pass 1 execution log and findings
metadata: 
  node_type: memory
  type: project
  originSessionId: 6a057456-b23a-407b-b8be-f1913e8b83f5
---

The **codebase readiness sweep** = Decision 3 in repo `STRATEGY.md` (4 passes: 0 ground-truth,
1 wiring/dead-code, 2 correctness landmines, 3 presentability). It was DEFERRED — captured, not run —
see [[project_strategy_gtm]]. On **2026-07-03** the user greenlit **Pass 0 + Pass 1**, then **Pass 2** on resume.
Working tree was clean at HEAD `f7e7b10` when this started. **COMMITTED + PUSHED as `8b35153`**
("repo cleanup : CSRF bug found. wire-ups done. legacy dead code removed.") — Pass 0/1/2 dead-code +
golden fix + `legend.md` + the `development-plan/` move (8 planning docs untracked via `git rm --cached`,
folder git-ignored) all landed in that single commit.

## Pass 0 — ground truth (DONE)
- Build clean (`cargo build -p albedo-server --bin albedo`, ~10s).
- Full workspace tests (`-j2`) had **exactly one failure**: golden fixture
  `tests/fixtures/golden/manifest_v2_test_app_components.json` was **one behavioral change stale** —
  the manifest now emits an early inline bootstrap shim in `<head>` (`__albedo_inject`/`__albedo_hydrate`
  queue-pushers, so hydration calls are captured before the deferred `/_albedo/runtime.js` module loads;
  landed with the "bootstrap" work in `3c29520`) but the fixture predated it. **Finding: committed HEAD
  `f7e7b10` shipped with a failing test.** Fixed by regenerating via `ALBEDO_UPDATE_GOLDENS=1`. All green after.

## Pass 1 — wiring & dead code (DONE, the big one)
Removed the legacy dev renderer. Removing its single `#[allow(dead_code)]` on `legacy_live_dev_runtime`
(in `src/bin/albedo.rs`) exposed a **75-item dead island** — the legacy fn was just the visible tip:
- Legacy hand-rolled HTTP/SSE dev server (`handle_dev_connection`, `read_http_request_body`,
  `write_sse_handshake`, `broadcast_*`, `inject_hmr_client_script`).
- Legacy watch/rebuild loop (`watch_and_rebuild_loop`, `PendingRebuild`, `rebuild_with_pending`, hot-set
  resolution helpers, `SharedDevState`, `DevAllRoutesArtifact`).
- Legacy renderer (`compose_dev_layouts`, `render_all_routes`, `dev_bakabox_script_tags`,
  `dev_static_asset`, `dev_shell_base_css`, `collect_css_bundle`, `base64_encode`, `escape_html`, …).
- Transport negotiation (`DevTransportDecision`, `determine_dev_transport`, …).
- **An entire never-shipped inspector/metrics subsystem** = the whole `src/bin/albedo/inspector.rs`
  module (662 lines: `InspectorState`, `InspectorPublisher`, `MetricsSnapshot`, `GraphSnapshot`,
  `ComponentCounter`, `INSPECTOR_HTML`, …). Reachable ONLY from the legacy dev server.

**Method (the rigorous bit):** dead functions were *interleaved* with live serve-path helpers
(`serve_connection_guarded`, `bind_dev_listener`, `read_http_request_head`/`parse_http_request_head`,
`inject_head_partial_into_shells`, `try_open_browser`, `write_http_response`, `run_prod_build` all live
and embedded among the dead). So NOT a contiguous block. Classified each via caller analysis, deleted the
7 dead line-ranges by awk filter (preserving the interleaved live fns), rebuilt so the compiler confirmed
the remaining dead set (imports, tests), then cleaned those.

**Real bug found + fixed:** the LIVE new dev path `run_live_dev_runtime` printed
`inspector · http://{addr}/__albedo` — but `/__albedo` was served ONLY by the (dead) legacy
`handle_dev_connection`. The new path boots `boot_production_server`, which never serves `/__albedo`.
So `albedo dev` was **advertising an inspector URL that 404s**. Removed the false advertisement.

**Result:** `src/bin/albedo.rs` −2013 lines; `inspector.rs` deleted (−662); golden +/−1. Net
**42 insertions / 2635 deletions**. Warning-free build, `cargo test -p albedo-server --bin albedo` 17/17,
full workspace suite green (exit 0). `graphify update .` run after.

## Pass 2 — correctness landmines (DONE 2026-07-03; mostly already-clean, one flagged)
Verdict: **the serve/request hot path is already unwrap-disciplined** — the ENDGAME D-list "~646 unwraps
on the hot path" was pessimistic; those live in the *build-time compiler* (root `src/`, runs during
`albedo build` = a CLI that exits loudly — acceptable), not the request path. Confirmed non-test
unwrap/expect/panic counts on the actual per-request path:
- `handlers/streaming.rs`, `render/mod.rs`, `render/tier_b.rs`, `handlers/action.rs` = **0**.
- `server.rs` = **2**, both `.expect("render world lock poisoned")` on the world `RwLock` — idiomatic
  lock-poison propagation, only fires if another thread already panicked; not input-triggerable.
- Runtime per-request path `compiled.rs` (`invoke_action_quickjs_inner`), `renderer/manifest.rs`
  (`render_route`), `form_result.rs` = **0** non-test.
- **eval silent drops: ALREADY FIXED** — `eval_body_stmts` (`src/runtime/eval/core.rs:2996`) catch-all
  `other =>` now returns a loud `Err("unsupported statement … must run on QuickJS Tier B/C")`; `Stmt::Expr`
  handled (comment documents the old silent no-op bug). No `_ => {}` drop remains there.
- **Wire decode: ALREADY graceful** — `decode_action_envelope` via `match` (400 on bad bytes),
  `serde_json::from_slice(...).ok()?` for the `_csrf` field; Gate-1-D bincode decode-bomb fix stands.

**FLAGGED, not fixed — the CSRF cross-invocation bug** (`crates/albedo-server/src/render/csrf.rs`).
`CsrfRegistry` = in-memory `Arc<DashMap<SessionId,String>>` of random tokens. Two real defects: (1) a form
minted on instance A fails `validate` (→ `Missing` → 403) when its POST lands on instance B — breaks any
multi-instance/serverless deploy; (2) `clear()` has **no production caller** (only a test — the WT-close
caller the old note mentioned was in deleted legacy/inspector code), so the map **grows unbounded** on a
long-running `albedo serve`. Correct fix = **stateless keyed-MAC token**: `token = MAC(server_secret,
session_id)`, recomputed on validate, no per-session storage; secret from env (`ALBEDO_CSRF_SECRET`) for
shared multi-instance, random boot fallback for single-server. **Deliberately NOT done now:** needs a crypto
dep (hmac+sha2 or blake3 — can't cleanly add offline) AND a secret-provisioning design that belongs WITH the
deploy adapter (which doesn't exist yet). Current DashMap design is correct for the only shipping mode
(single `albedo serve`). Do this AS PART OF the deploy-manifold work — see [[project_strategy_gtm]] deploy plan.

## NOT done (remaining sweep scope)
- Pass 1 leftover: full re-verification of every P1–P6 "live-verified" claim on serve was not re-run (needs
  live apps; memory records each as verified; the one wiring gap found — `/__albedo` — was fixed). Deploy-
  adapter seam = N/A (doesn't exist yet).
- Pass 3 (presentability): mostly moot — mega-diff already committed `f7e7b10`; remaining = fresh-app
  end-to-end + `graphify-out/` tracking decision.
- Live `albedo dev`/`serve` smoke on a real app after the Pass-1 deletion NOT run (dead-code-only + full
  suite green judged sufficient; live `run_live_dev_runtime` untouched bar the banner line).
