---
name: project-phase-o
description: "Phase O (CHAIN REACTION) — the moat. O.1 (tier budget), O.2 (broadcast slots), O.3 (tier-aware bundle splitting) all landed. O.4 / O.5 deferred."
metadata: 
  node_type: memory
  type: project
  originSessionId: b5632263-e10d-49fc-bce2-7ddf4430ee47
---

Phase O is the five-piece "moat" — features no other framework can ship because no other framework has the substrate. **Status as of 2026-05-25**: O.1, O.2, O.3 all done + pushed. The recommended sprint stopping point. O.4 (DevTools) + O.5 (Go reference server) deferred until external users exist.

## Commits

- `7811290` "phase N and O.1" — O.1 alongside Phase N.
- `6860f69` "Async completion gate 1" — O.2 (all three weeks) + O.3 in one atomic.

## Sprint-shape outcome

Recommended pacing was **O.1 → O.2 → O.3 → stop and ship**. That's what landed:

- O.1 (1 week of work) — source-weight gate.
- O.2 (3 weeks compressed into focused sessions) — broadcast substrate + JSX surface + renderer integration + production hardening.
- O.3 (1 week) — measured-bytes bundle gate, reuses O.1 machinery.
- O.4 — deferred (Chrome extension, no Rust impact, no pre-user signal on what workflows to support).
- O.5 — deferred (wire is conformance-frozen; portability proof can happen any time).

## O.1 — Tier budget

**Module**: `src/budget/{mod.rs, config.rs, format.rs, report.rs}`.

**Config**: `tier-budget.toml` at project root. `[defaults]` block + optional `[routes."/path"]` overrides. Built-in defaults: Tier-A ≤ 50 components/route, Tier-B ≤ 8 KB/route + 4 KB/component, Tier-C ≤ 10 concurrent fetches/route. `load_budget_from_dir` returns `Ok(None)` when missing — no auto-fallback so typos don't silently degrade.

**Eval**: `evaluate_budget(manifest, budget) -> BudgetReport` is a pure function. Walks `RenderManifestV2.routes`, cross-references `ComponentManifestEntry.weight_bytes` via a `HashMap<String, u64>` index. Tier-A count = top-level `tier_a_root` + `tier_b[].tier_a_children`. Per-component check runs over every Tier-B node in every route. **Source-weight** estimate — fast, uses only the manifest. O.3 added the **measured-bytes** companion gate.

**CLI**: `albedo budget [--strict] [--format pretty|json]` runs the standalone evaluator. `albedo build` and `albedo ship` auto-gate when `tier-budget.toml` exists. `--no-budget` opts out.

## O.2 — Broadcast slots (server-pushed reactivity)

The framework-defining piece. Two browser tabs share state over a binary wire with zero polling. Bakabox stays dumb — server multiplexes, wire stays singular.

### W1 — Substrate (`src/runtime/broadcast.rs`)

- `BroadcastRegistry` — topic-keyed shared state, `DashMap<String, Arc<BroadcastTopic>>` + reverse index `DashMap<SessionId, FxHashSet<String>>` for O(k) cleanup.
- `BroadcastTopic { name, slot_id, value: Mutex<Vec<u8>>, subscribers: DashMap<SessionId, BroadcastSender> }`.
- `broadcast_slot_id(topic)` — `fnv1a_32("broadcast::{topic}")`. The `"broadcast::"` prefix avoids collision with Phase K's `"{module}::{fn}#{idx}"` slot IDs.
- `write_topic(topic, value) -> BroadcastDelivery`: updates the topic's value, builds one `OpcodeFrame { instructions: [SlotSet { slot_id, value }] }`, encodes via `encode_frame`, `try_send`s the bytes to every subscriber. Returns `{ delivered, dropped_full, dropped_closed }`. Slow/dead consumers are pruned immediately.
- `subscribe(session, topic, sender) -> Vec<u8>` returns current value for late-joiner initial paint.
- `cleanup_session(session)` walks the reverse index for O(k) removal.
- **No wire-format change** — reuses existing `Instruction::SlotSet`. `LOCKED_WIRE_VERSION` stayed at 2.
- Server wiring: `AlbedoServer.state.broadcast: Arc<BroadcastRegistry>` always allocated; `AlbedoServer::broadcast()` accessor; `WebTransportRuntime::with_broadcast(...)` calls `cleanup_session` automatically on session drop.
- 12 unit tests + 5 server-side integration tests in `crates/albedo-server/tests/broadcast_end_to_end.rs`.

### W2 — JSX extractor + demo

- `src/transforms/shared_slots.rs::extract_shared_slot_hooks(function, imports) -> Result<Vec<SharedSlotBinding>, SharedSlotExtractError>`. Mirrors `transforms::hooks::extract_use_state_hooks` structurally. Rejects: conditional placement, missing topic argument, non-string-literal topic, non-identifier binding pattern. Recognises only `useSharedSlot` imported from `"albedo"`. Handles TS wrappers (`as const`, `satisfies`, parens). 11 tests.
- `CompiledComponent.shared_slots: Vec<SharedSlotBinding>` populated in `CompiledProject::wrap`.
- `CompiledProject::shared_slot_topics() -> Vec<String>` (sorted, deduplicated, deterministic) + `shared_slots_for_component(module, fn) -> &[SharedSlotBinding]`.
- `BroadcastRegistry::auto_subscribe(session, sender, topics) -> Vec<Instruction>` — subscribe one session to many topics, auto-creates unknown topics, returns initial-state `SlotSet` opcodes.
- `crates/albedo-server/examples/chat_broadcast.rs` — single-file HTTP-only runnable demo with SSE endpoint + POST broadcast. No QUIC/TLS dependency. `cargo run -p albedo-server --example chat_broadcast`. Two `curl -N /sse` terminals + one `curl -X POST /post` = the two-tab demo.

### W3 — Renderer integration + hardening

The **load-bearing edit**. Surgical touch on the 88 KB `runtime/eval/core.rs`:
- `ComponentScope.shared_slots: HashMap<String, (SlotId, String)>` — binding name → (broadcast slot id, topic key). Populated in `current_phase_k_component` and `eval_handler_body`.
- New `PHASE_K_BROADCAST: Cell<Option<*const BroadcastRegistry>>` thread-local + `install_phase_k_broadcast` RAII guard. Same lifetime contract as `PHASE_K_PROJECT`.
- `phase_k_shared_slot_for_value(name)` lookup helper.
- `phase_k_detect_slot_text_read` extended to also recognise shared-slot bindings (falls back when no useState match; useState wins on name collision to match JS shadowing).
- `eval_var_decl_into_env` gains a branch for `const x = useSharedSlot("topic")`: resolves the topic via `current_phase_k_broadcast()`, decodes value as JSON, binds into the JSX env. Falls back to `Value::Null` when no broadcast registry installed (safety net for pre-O.2 code paths).
- New `ComponentProject::render_entry_compiled_with_broadcast` mirrors `render_entry_compiled` with the broadcast guard installed.
- User-facing `render_entry_with_broadcast(compiled, entry, props, slots, broadcast, subscriber_sender, opts)` in `compiled.rs`: auto-subscribes the session to every `shared_slot_topics()`, runs the render, **prepends** the initial-value `SlotSet` opcodes so the client paints with current state before any `SetTextRef` references it.
- Net edit ~120 lines in 2300; useState path completely untouched, **all 17 hook-compile goldens passed unchanged**.
- 5 golden tests in `tests/shared_slot_golden.rs` (`tests/fixtures/shared_slot/lobby/Component.tsx` fixture) cover: HTML inlines current value, SetTextRef emitted at correct broadcast slot id, initial SlotSet prepended, auto-subscribe so follow-up write delivers, auto-creation of unknown topics, two-session fan-out, fallback to null without broadcast.
- 7 failure-mode tests in `tests/broadcast_failure_modes.rs`: 64 concurrent subscribers, concurrent cleanup-vs-write (no deadlock), slow-consumer prune (1000 writes), 256 KB payloads, **1000 sessions × 10 writes storm (10,000 deliveries in 40ms)**, mid-subscribe disconnect, reverse-index cleanup on auto-subscribe.

### The framework moment

The wire-side identity: a `SetTextRef` emitted by the renderer for a `useSharedSlot` binding points at `broadcast_slot_id(topic)`. The `SlotSet` the broadcast registry fans out points at **the same id**. Bakabox sees identical opcodes whether the write came from a per-session action handler or a global broadcast write. The dumb-client invariant pays off — server-side multiplexing, singular wire.

## O.3 — Tier-aware bundle splitting

Post-emit gate that flips O.1's source-weight *estimate* into a *measured-bytes* guarantee. A PR that imports lodash into a Tier-B Counter now fails the build with the exact diff text the sprint plan demoed.

**New module `src/budget/bundle.rs`**:
- `BundleByteReport` / `BundleAttribution` / `ComponentBundleSummary`.
- `compute_bundle_byte_report(emit_report, plan, manifest) -> BundleByteReport` is the pure attribution function.
- Attribution rules:
  - Wrapper `.mjs` → owning component (the hot-path hydration JS).
  - `.mjs.map` → paired wrapper's component, tracked as `source_map_bytes` but **excluded from the budget** (maps inflate dev artefacts without reflecting browser cost).
  - Vendor chunks → tracked separately as `vendor_total_bytes`, not per-component (avoids double-counting shared vendors).
  - Manifests / plan JSON / static slices / precompiled modules → `BundleArtifactClass::Infrastructure`, excluded.

**Budget extensions** (`src/budget/{config.rs, report.rs}`):
- `BudgetDefaults.tier_b_bundle_max_kb_per_component: u32` (default **1 KB** per sprint plan).
- `RouteBudget` mirrors as `Option<u32>` for per-route overrides.
- `evaluate_bundle_budget(byte_report, budget) -> BudgetReport`. **Only Tier-B components subject** — Tier-A ships zero JS, Tier-C streams server-side.
- New `ViolationKind::TierBBundleKbPerComponent`.

**Formatter hint** (`src/budget/format.rs`):
- The new violation gets an actionable `hint` line — exactly the sprint plan's demo text:
  ```
  * tier-b component bundle exceeded - <global>
      limit   1.0 KB
      actual  142.0 KB  (+141.0 KB over)
      top contributors:
        - Counter                  142.0 KB
      hint    Move heavy imports in Counter to Tier-C, or raise
              `tier_b_bundle_max_kb_per_component = 142` in tier-budget.toml.
  ```
- Suggested ceiling computed from actual bytes (`ceil(actual_kb)`) so the user can paste the exact line into config.

**CLI integration** (`src/bin/albedo.rs`):
- `enforce_budget_after_build(contract, manifest, Some(&emit_report), skip)` runs both O.1 (source-weight) and O.3 (bundle-byte) gates. Violations merged into one printed diff.
- `bundle_budget_report(...)` derives the plan from the manifest internally — bundler is deterministic, no plumbing changes.
- `--no-budget` opts out of both. File-gated (only runs when `tier-budget.toml` exists).

**Tests** — 4 in `tests/bundle_budget_integration.rs`: small Tier-B passes default 1 KB ceiling, oversized wrapper trips with actionable diff (using 142 KB to match sprint-plan demo), per-component override relaxes, Tier-A/C wrappers blown to 250 KB never flagged.

## What's still queued (deferred, not blocked)

### O.4 — View Wire DevTools (deferred)

Chrome extension over a debug WT slot. Decodes opcodes, replays frames against live DOM, intercepts outgoing action POSTs. Pure JS extension; only Rust change would be a new `Instruction::DebugEcho` opcode gated to dev builds. **Defer until external users request specific opcode-debugging workflows** — pre-user, the UI is a guess.

### O.5 — Cross-language reference server (deferred)

`reference/go-albedo-server/` — ~1500 LOC Go implementing enough of the wire to serve a Tier-A shell + Tier-C patches. Bakabox can't tell it's not Rust. The wire format is conformance-frozen at `LOCKED_WIRE_VERSION = 2` via `canonical_v1_frame`, so this can happen any time. **Defer until external production deployment exists** — portability proof matters for adoption, not pre-adoption.

## Lessons worth keeping

### Bakabox can't tell broadcast from per-session

Both use the same `Instruction::SlotSet` opcode, same `WT_STREAM_SLOT_PATCHES` lane, same client-side dispatch. The server multiplexes; the wire stays singular. This is what made O.2 ship without a wire-format bump — and what makes the broadcast registry composable with the existing slot store rather than a replacement.

### Backpressure policy via `try_send`

Non-blocking sends mean a slow consumer cannot stall fan-out to fast ones. Full channel → drop the subscription, surface in `dropped_full`. Closed channel → drop, surface in `dropped_closed`. The 1000×10 storm test runs in 40 ms because nothing waits on anything.

### Measured bytes > source-weight estimate

The O.1 source-weight gate uses `ComponentManifestEntry.weight_bytes` (the parser's estimate). The O.3 bundle-byte gate uses the actual emitted wrapper file size. They can differ by 10x. Shipping both gates means CI catches creep at both layers — source-side bloat AND import-side bloat.

### The renderer integration risk pattern

The 88 KB `runtime/eval/core.rs` is a high-stakes file. The Phase K useState pathway has 17 golden tests that **must** stay green. Strategy: add new code paths alongside existing ones, never inside them. `phase_k_detect_slot_text_read` got a fallthrough check for shared slots — useState still wins on name collision. `eval_var_decl_into_env` got a new branch for `useSharedSlot` that's checked BEFORE the existing useState branch. Zero edits inside the useState code itself.
