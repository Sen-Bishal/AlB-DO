---
name: project-phase-glossary
description: "Phase letter codes (Phase A–M) — what each one means, what files belong to it, the corresponding wire/runtime artifact."
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

The codebase has its own development-phase letter codes. Comments reference them constantly ("Phase 2 (B-emitter)", "Phase-G", "Phase K Stage 2", "Phase L · CSRF substitution"). Without this glossary, code review tracking is impossible.

**Why:** The phases are NOT chronological in the obvious way — they're functional layers added incrementally to the wire format and runtime, each with its own contract. Pinaki Pritam Singha appears in PHASE comments as a reviewer/collaborator (e.g. "PHASE 2 (B-emitter) — Pinaki: ...").

**How to apply:** When code refers to "Phase X", look up the row below. Don't conflate Phase letters with "Cycle N" (cycles are SoA-refactor stages within Phase 2; Phase A is the original opcode/wire freeze).

| Phase | What it locked / shipped | Key files |
|-------|--------------------------|-----------|
| **A** | Opcode set + binary frame format (bincode wire). `Instruction` variants 0..=13 (14 variants), `OpcodeFrame`, `InternTable`, `InternPatchOp`, `InstructionRange::try_new` private-field invariant. `LOCKED_WIRE_VERSION = 1`. | `src/ir/opcode.rs`, `src/ir/wire.rs`, `src/ir/conformance.rs` |
| **B** | Opcode emitter — bridges `IrColumns` to `OpcodeFrame` and routes via WT muxer. `InternTableSnapshot` diffs intern tables → `PatchInternTable` ops. | `src/runtime/emitter.rs` |
| **C** | Client decoder (bakabox JS — outside this repo's Rust source, lives in `assets/`). | `assets/albedo-runtime.js`, `assets/bincode.js` |
| **D** | Async islands — `Placeholder`/`Patch` opcodes, suspense allocator, spawned resolver futures, mpsc back-channel. Needs tokio runtime handle. | `src/runtime/pipeline.rs` (suspense_allocator, pending_placeholders, async_tx/rx) |
| **E** | useState / hooks wire surface — `SetTextRef`, `SetAttrRef`, `SlotSet` opcodes (folded into Phase A wire so deferred clients don't break). | `src/ir/opcode.rs` (Alt-D opcode triple), `src/runtime/slot_store.rs` |
| **F** | API handlers (HTTP only, no opcode side) — distinct from page handlers in the server's `ApiHandler` registry. | `crates/albedo-server/src/api.rs`, `crates/albedo-server/src/handlers/api.rs` |
| **G** | Client → server actions — `ActionEnvelope` POSTed to `/_albedo/action`; `ActionRegistry`/`ActionHandler`. `proxy_id` reused as `action_id`; no Instruction enum change. | `src/ir/action.rs`, `crates/albedo-server/src/actions.rs`, `crates/albedo-server/src/handlers/action.rs` |
| **H** | Server-side reactive `SlotStore` (Phase G's storage). Shared via `Arc` with the runtime pipeline so writes from action handlers are visible to the next drain without copy. | `src/runtime/slot_store.rs`, `src/runtime/session.rs`, `crates/albedo-server/src/actions.rs` (SessionSlots) |
| **I** | `Navigate` opcode — variant 14, added at the END of the enum. Bumps `LOCKED_WIRE_VERSION` to 2. Shipped pre-Phase-L (out of the sprint plan's order). | `src/ir/opcode.rs` (Instruction::Navigate), `src/ir/conformance.rs` |
| **J** | AST interpreter (the Phase-J evaluator). Walks SWC AST against props + a slot view to produce HTML. Predecessor of Phase K compile-time extraction. | `src/runtime/eval/core.rs` |
| **K** | Compile-time JSX transforms — `useState` extractor, JSX `on*` handler extractor, free-variable collector, `CompiledProject` facade. Bridges user-authored JSX → wire opcodes by re-interpreting metadata at render time. Three stages: (1) inline arrow handlers + literal initials, (2) closures over component props (`capture_slots`), (3) closures over module-level constants (`module_constants`). | `src/transforms/{hooks,events}.rs`, `src/runtime/compiled.rs`, `src/runtime/eval/core.rs` (RENDER_K thread-local) |
| **L** | **DETONATION** — forms, navigation, CSRF. `<Link>` rewrite to `<a data-albedo-link>`, `<form action="action:NAME">` rewrite to `<form data-albedo-action>` + CSRF placeholder + error spans. `register_form_action::<T>(action_name, handler)` builder. `CsrfRegistry` with cookie session round-trip and post-render substitution. Action route validates `_csrf` field of JSON payloads. Detailed in [[project-phase-l]]. | `src/transforms/{link,form}.rs`, `src/runtime/eval/core.rs` (FORM_ACTION_STACK + stamps), `crates/albedo-server/src/render/{form_action,form_validation,csrf}.rs`, `assets/albedo-link-forms.js`, `crates/albedo-server/tests/form_submit_end_to_end.rs` |
| **M** | **FALLOUT (DX)** — error overlay (server registry + SSE + client overlay), slot-preserving HMR (in-place DOM swap on file change), TypeScript intrinsic types, source-map sidecars (v3 stubs). New `crate::dev::*` module is the dev-only surface. `AlbedoServerBuilder::with_dev_mode(bool)`. Detailed in [[project-phase-m]]. | `crates/albedo-server/src/dev/{error_overlay,hmr,mod}.rs`, `crates/albedo-server/src/handlers/dev.rs`, `assets/albedo-{error-overlay,hmr-apply}.js`, `scaffold/src/albedo-env.d.ts`, `src/bundler/{rewrite,emit}.rs` |
| **N** | **WARHEAD** — framework primitives. File-based routing (`src/routing/file_based.rs::discover_routes`), layouts via `layout.tsx`, dynamic params reuse existing `CompiledRouter::normalize_route_paths`, CSS-modules scoping primitive (no JSX rewrite yet), `public/` static-asset dispatch arm + `AlbedoServerBuilder::with_public_dir(..)`, multi-stage Docker + fly.toml templates, Vercel ship target downgraded to an explicit "use --target docker" error. Detailed in [[project-phase-n]]. | `src/routing/{mod,file_based}.rs`, `src/transforms/css_modules.rs`, `crates/albedo-server/src/handlers/public_assets.rs`, `crates/albedo-server/tests/public_assets_end_to_end.rs`, `src/bin/albedo.rs` (docker/fly/vercel rewrites + `print_ship_help` + completion scripts) |
| **O.1** | **CHAIN REACTION · tier budget** — `tier-budget.toml` declares per-tier ceilings; `evaluate_budget(manifest, budget) -> BudgetReport` checks routes. New `albedo budget [--strict] [--format pretty\|json]` subcommand; `albedo build`/`ship` auto-gate when `tier-budget.toml` exists, `--no-budget` opts out. Defaults: Tier-A ≤ 50 cmp/route, Tier-B ≤ 8 KB/route + 4 KB/cmp, Tier-C ≤ 10 fetch/route. Detailed in [[project-phase-o]]. | `src/budget/{mod,config,format,report}.rs`, `tests/budget_integration.rs`, `src/bin/albedo.rs` (`run_budget_command`, `run_prod_build_with_budget`, completions), workspace `Cargo.toml` (`toml = "0.8"`) |
| **O.2** | **CHAIN REACTION · broadcast slots** — server-pushed reactivity over the WT patches lane. Three weeks landed: (W1) `BroadcastRegistry` + topic-keyed shared state + fan-out via existing `SlotSet` opcode (no wire-format bump); (W2) `useSharedSlot<T>("topic")` SWC extractor + `CompiledProject` wiring + `auto_subscribe` helper + runnable `examples/chat_broadcast.rs`; (W3) renderer integration via new `render_entry_with_broadcast` (ComponentScope.shared_slots, PHASE_K_BROADCAST thread-local, eval branch for `const x = useSharedSlot("t")`) + 7 failure-mode tests (1000×10 storm, concurrent subscribe/write, slow-consumer prune, 256 KB payloads). Detailed in [[project-phase-o]]. | `src/runtime/broadcast.rs`, `src/transforms/shared_slots.rs`, `src/runtime/eval/core.rs` (renderer touch), `src/runtime/compiled.rs` (`render_entry_with_broadcast`), `crates/albedo-server/{src/server.rs, src/webtransport.rs, examples/chat_broadcast.rs, tests/broadcast_end_to_end.rs}`, `tests/{shared_slot_golden.rs, broadcast_failure_modes.rs}`, `tests/fixtures/shared_slot/` |
| **O.3** | **CHAIN REACTION · tier-aware bundle splitting** — post-emit bundle-byte gate measures actual wrapper JS bytes per Tier-B component. `BundleByteReport` attributes artifacts to `(component_id, tier)`; `evaluate_bundle_budget` fails when a wrapper exceeds 1 KB default. Pretty formatter emits the actionable hint: *"Move heavy imports in X to Tier-C, or raise `tier_b_bundle_max_kb_per_component = N` in tier-budget.toml."* Build/ship run both O.1 (source-weight) and O.3 (bundle-byte) gates; `--no-budget` opts out of both. Detailed in [[project-phase-o]]. | `src/budget/bundle.rs`, `src/budget/{config,format,report}.rs` (extensions), `src/bin/albedo.rs` (`enforce_budget_after_build` extended with emit-report param), `tests/bundle_budget_integration.rs` |
| **O.4–O.5** | **Deferred** until external users exist (post-Phase-P). O.4 (View Wire DevTools Chrome extension) — pure JS, optimises for tweets over adoption pre-users. O.5 (Go reference server) — wire is already conformance-frozen at `LOCKED_WIRE_VERSION = 2`, can happen any time once a real deployment needs portability. | — |
| **P** | **PLATFORM SPRINT** — close all 18 cohesion gaps surfaced in the post-O.3 audit. The substrate works (Phases A–O.3 tests prove it) but `albedo serve` / `albedo dev` / the manifest builder don't actually use most of it. P wires everything end-to-end so app devs write zero Rust. Seven streams (A–G); Stream B done + pushed (`f625be6`). Stream B specifically: manifest builder now uses `CompiledProject` + `render_entry_with_broadcast` to pre-render every Tier-B node with full HTML + bincode-encoded `OpcodeFrame` payload. Detailed in [[project-phase-p]]. | `src/manifest/{schema,builder,mod}.rs` (Stream B); plan at `C:\Users\bisha\.claude\plans\1-ts-form-action-handler-shimmering-hellman.md` |

## Cycles (within Phase 2 — the SoA IR refactor)
- **Cycle 1** — `IrColumns` skeleton.
- **Cycle 2** — `DirtyBitmap` (flat `Box<[AtomicU64]>` replacing the linked-list ring), SIMD `wide::u64x4` hash diff kernel. Source hash flip from FNV-1a → xxh3_64. CANONICAL_IR_SCHEMA_VERSION bumped 1.0 → 1.1.
- **Cycle 3** — `ColumnPass`/`parallel_column_pass` split-borrow rayon scopes.
- **Cycle 4** — Lane-sorted columns: `lane_ids`, `lane_offsets`, `LaneColumnPass`, `parallel_lane_column_pass`. Edges stored as column indices (not raw ids) so split-borrow needs no id indirection.
- **Cycle 5** — Sub-ms RAF reconciliation tick (`frame.rs::frame_tick` + `FrameArena`).
- **Cycle 6** — (planned) — fold `effects`/`priority` into patch field_mask without a wire-format break.

## Sprint dependency graph (per `C:\Users\bisha\.claude\plans\lets-make-nukes-sprint.md`)

```
J ──► K ──► L ──┐
                ├──► O (moat)
M, N ───────────┘
```

- J, K, L, M: **done** as of 2026-05-24 (pushed).
- N (WARHEAD): **done + pushed** as of 2026-05-25 (commit `7811290`).
- O.1 (CHAIN REACTION · tier budget): **done + pushed** as of 2026-05-25 (commit `7811290`, same commit as N).
- O.2 (broadcast slots, all 3 weeks): **done + pushed** as of 2026-05-25 (commit `6860f69`). Substrate works at the public Rust API level (chat example proves it); production wire-through is Phase P work.
- O.3 (tier-aware bundle splitting): **done + pushed** as of 2026-05-25 (commit `6860f69`, same commit as O.2).
- **P (PLATFORM SPRINT)**: in flight as of 2026-05-26. The full audit found 18 wire-through gaps (the substrate works in isolation but the production server / dev server / build manifest don't actually call most of it). Phase P closes all of them across seven streams (A–G). Stream B (manifest carries real pre-rendered HTML + opcodes + per-route metadata) **done + pushed** as commit `f625be6` "Albedo server scaffold". Stream A (production server boot), Stream C (TS authoring, absorbs O.6), and Stream E.1 (layout composition) unblocked next. Plan: [[project-phase-p]] + `C:\Users\bisha\.claude\plans\1-ts-form-action-handler-shimmering-hellman.md`.
- O.4 (DevTools), O.5 (Go reference server): **deferred** until external users exist (after Phase P).
