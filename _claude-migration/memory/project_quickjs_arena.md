---
name: project-quickjs-arena
description: "The QuickJS memory model under the engine (src/runtime/arena.rs). REDESIGNED 2026-06-29: the request-scoped O(1)-reset bump region was fundamentally unsafe for a general JS runtime and was REPLACED — request-time memory is now QuickJS-managed (system allocator); the persistent bump is kept for warmup only."
metadata:
  node_type: memory
  type: project
  originSessionId: gate1-arena-2026-06-01
---

# QuickJS arena — Movement III, REDESIGNED 2026-06-29 (Option A)

⚠️ **The original request-scoped O(1)-reset design was removed — it was unsafe by construction for a general framework.** See [[project_p3_dynamic_routes]] for the investigation that forced this.

## Current design (Option A — `src/runtime/arena.rs`, all uncommitted)
Two lifetimes, but request-time lifetime is **deferred to QuickJS itself**:
- **persistent region** — a bump arena for warmup/bootstrap state (builtins, loaded modules, the runtime-global tables grown once during warmup). Never freed while the engine lives; reclaimed on engine drop. Allocations land here when `in_request == false` (i.e. during `begin_warmup`/`end_warmup` or the first `ARENA_WARMUP_RENDERS`=8 renders).
- **request-time memory** — when `in_request == true`, fresh allocations go to the **system allocator** (`alloc_system`) and are freed **per-block by QuickJS's own refcount + cycle collector** via `dealloc`. `end_request` no longer resets anything (it just clears the flag); `run_gc()` at the boundary still collects cycles.
- `ArenaStats` now reports `system_live_bytes` / `system_peak_bytes` (request-time gauge) instead of `request_used`/`request_peak`. `RegionKind`, the second `Region`, and `DEFAULT_REQUEST_CAP` are gone. `ArenaControl::new` takes one cap. `realloc`/`dealloc` dispatch on `is_persistent(ptr)`.
- **Engine side UNCHANGED**: `render_component_inner` / `eval_handler` / `eval_route_metadata` still do the `scoped` calc + `begin_request`/`run_gc`/`end_request`. The semantics of those calls changed inside the arena, not the call sites. Warmup (`warm_render_targets` in `engine_pool.rs`) is now a **perf** optimization (interns stable shapes/atoms persistently to avoid per-request system-alloc churn), **not** a correctness requirement.

## Why the old design was unsafe (the root cause — don't reintroduce O(1) reset)
The old model bump-allocated request memory into a second region and reset the cursor in O(1) at the request boundary. Its stated invariant — "by render end, QuickJS has removed the request's shapes/atoms from the runtime-global tables, so only persistent-region data is still referenced" — **is false**. QuickJS interns long-lived **shapes** (hidden classes) and **atoms** (property-name strings) keyed off the *per-request* object shapes / property names it sees, and they stay reachable from `rt->shape_hash` / `atom_array` across requests. The reset freed that still-live memory → next request reusing a dangling shape aborted (`js_free_shape0`: `assert(sh->header.ref_count == 0)` at quickjs.c:4577, or an access violation at 5843). Empirically bisected to the absolute minimum: a component that merely **receives** a non-empty props object (`{params:{slug}}`) crashes on the 2nd request — no element building, no retention needed. Static routes survived only because their real request props are also `{}` (matching warmup); the dynamic `[slug]` route was the first whose request props had a shape `{}`-warmup never created. **No warmup scheme can fix this in general** — you can never pre-intern every shape/atom arbitrary app data will produce (data-dependent keys, conditional render paths). That's why warmup-based fixes were rejected as patchwork and the memory *model* was changed instead.

## Verified (2026-06-29)
- In-process repro `quickjs_engine::tests::dynamic_route_render_survives_reset_after_throwing_warmup` (warmup throws on `{}`, then 16 scoped renders with real params) — **was the deterministic crash, now passes**. Kept as the regression guard.
- Rewritten arena unit tests (8) + the rewritten V guardrail `request_arena_resets_each_render_without_persistent_growth_or_corruption` (steady-state: persistent watermark flat, `system_live_bytes` returns to baseline each render = no leak). 417 lib + 120 server-lib + 5 serve-boot green.
- Live serve: 9 requests across 6 distinct slugs + repeats + a 404 → all 200, per-essay `generateMetadata` titles correct, 404 → error boundary, server stays up; preview DOM `readyState=complete` with correct title + prose. (Old binary aborted on request #2.)

## Notes
- The "warmup-then-reset discipline" and "residual hazard" from the original note are now **moot** (the hazard class is closed by the redesign, not deferred). Auto-GC threshold tuning (`set_gc_threshold`) and the perf re-introduction of a *safe* custom allocator are deferred-until-measured ([[project_endgame]]).
- Honest tradeoff: we gave up the (unproven, GC-undercut) "zero steady-state heap traffic" claim for correctness/generality.
- **📌 STANDING DECISION (user, 2026-06-29):** *if a real measurement later shows allocator pressure matters, a **safe** custom allocator can be reintroduced then* — never the old unsafe O(1)-reset model. Correctness first; this is a deliberate deferred-until-measured perf option, not an abandoned idea. Any reintroduction must keep request-time shapes/atoms from dangling (e.g. a generational/free-list scheme, or segregating QuickJS shape/atom allocations), and must pass the `dynamic_route_render_survives_reset_after_throwing_warmup` repro.

See [[project-runtime-kernel]], [[project-renderer-and-eval]], [[project_p3_dynamic_routes]].
