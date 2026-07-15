---
name: project-phase-p
description: Phase P (PLATFORM SPRINT) — close all 18 audit gaps. Zero-Rust framework. Stream B done; A/C/D/E.1-3/F/G queued.
metadata: 
  node_type: memory
  type: project
  originSessionId: b5632263-e10d-49fc-bce2-7ddf4430ee47
---

Phase P is the integration sprint that converts ALBEDO from "powerful substrate with userland-Rust escape hatches" into a complete framework where app developers write only TS/JSX. It subsumes the previously-approved Phase O.6 (TS-only path) as Stream C within a larger seven-stream sprint that closes every cohesion gap surfaced in the post-O.3 audit.

**Plan location**: `C:\Users\bisha\.claude\plans\1-ts-form-action-handler-shimmering-hellman.md` (filename predates the broader scope; document was expanded with user approval).

## Bar at sprint end

- App developers write **zero Rust**. The custom `server/main.rs` pattern from `albedo-async-showcase` is deleted.
- `albedo dev` and `albedo serve` are both fully-featured framework servers — render, dispatch, broadcast, fan-out, hydrate, all wired.
- Build-time manifests carry real rendered HTML (not `<div data-albedo-fallback>` placeholders).
- Next/Remix convention parity (layouts, error.tsx, loading.tsx, file-based routing).
- Benchmarks publish citable numbers vs Next/Remix proving the speed claim.

## The 18 audit gaps (from the planning exploration)

| # | Gap | Severity | Stream |
|---|---|---|---|
| 1 | `albedo serve` is `build + files`, no `AlbedoServer` boot, no `/_albedo/action` | Critical | A |
| 2 | Manifest builder emits `<div data-albedo-fallback>` for every Tier-B node | Critical | **B (done)** |
| 3 | Dev shell injects only HMR client — no `runtime.js` | Critical | D |
| 4 | Dev HTTP handler accepts GET only; no `POST /_albedo/action` | Critical | D |
| 5 | `register_compiled_project` has no production caller | Critical | A |
| 6 | `CompiledProject::load_from_dir` has no production caller | Critical | A |
| 7 | `discover_routes` returns `layout_chain` but manifest builder never reads it | High | **B (done)** |
| 8 | `render_entry_with_broadcast` never invoked by production | High | A |
| 9 | `discover_routes` runs only in `resolve_dev_contract`; build never sees file-based routes | High | **B (done)** |
| 10 | TS-side `action(...)` extraction missing | High | C.1 |
| 11 | `broadcast()` interpreter builtin missing | High | C.2 |
| 12 | No auto-topic-registration at server startup | Medium | C.3 |
| 13 | Streaming handler doesn't auto-subscribe sessions to route topics | Medium | C.4 |
| 14 | CSS modules JSX rewrite not wired | Medium | E.3 |
| 15 | `error.tsx` / `loading.tsx` per-route conventions absent | Medium | E.2 |
| 16 | `RendererRuntime::prime_runtime_cache` uses Phase J not CompiledProject | Medium | A (incidental) |
| 17 | Source maps emit `"mappings":""` stubs | Low | F.1 |
| 18 | Scaffold produces `src/App.tsx` (pre-Phase-N) | Low | F.2 |

## Seven streams + status *(updated 2026-05-28)*

| Stream | Scope | Status |
|---|---|---|
| **A** | `albedo serve` becomes real `AlbedoServer` boot | ✅ DONE + pushed (`4c1358d`) |
| **B** | Build-time manifest carries real HTML + opcodes + per-route metadata | ✅ DONE + pushed (`f625be6`) |
| **C.1** | `action()` extractor + ParsedModule.action_declarations + handler registry | ✅ DONE (local) |
| **C.2** | `broadcast(topic, updater)` interpreter builtin + adapter PHASE_K_BROADCAST install | ✅ DONE (local) |
| **C.3** | Auto topic registration in `register_compiled_project` | ✅ DONE (local) |
| **C.4** | Auto-subscribe in streaming handler | ✅ DONE (local) |
| **D.1** | Dev render promoted to CompiledProject + Phase K (slot store + broadcast Arcs on SharedDevState) | ✅ DONE (local) |
| **D.2** | Bakabox runtime scripts injected into dev shell | ✅ DONE (local) |
| **D.3** | POST `/_albedo/action` endpoint on the dev HTTP handler | ✅ DONE (local) |
| **D.4** | Dev server serves `/_albedo/runtime.js` + `bincode.js` + `link-forms.js` etc. from `include_str!` templates | ✅ DONE (local) |
| **E.1** | Layout chain wired through render via `<children />` intrinsic + sentinel substitution | ✅ DONE (local) |
| **E.2** | `error.tsx` / `loading.tsx` discovered (`DiscoveredErrorBoundary` + `DiscoveredLoadingFallback`) + `RouteManifest.error_component` / `loading_component` populated | ✅ DONE (local) |
| **E.3** | CSS modules JSX rewrite: `styles.foo` resolves to scoped class via `CssModuleRegistry`; manifest shell carries `<style data-albedo-css-modules>` | ✅ DONE (local) |
| **F.1** | Source maps Stage 2 | **Deferred** — design note in `build_wrapper_source_map`. Wrapper is trampoline JS, not SWC-transpiled output; per-line mappings need a `transpile-and-ship` architecture change beyond F.1's scope. |
| **F.2** | Scaffold refresh to Phase N+ conventions (`src/routes/`, `tier-budget.toml`, `action()` + `useSharedSlot` demo) | ✅ DONE (local) |
| **F.3** | CLI honesty pass on help text | ✅ DONE (local) |
| **F.4** | Showcase migration | **CANCELLED** (2026-05-28). User decided to delete `albedo-async-showcase` entirely. Replaced post-P by building a fresh full-fledged demo app on top of the post-P API. |
| **G** | Benchmarks vs Next/Remix — Criterion + Markdown table in README | Queued (last remaining stream for Phase P close-out) |

**Local commit cluster** (uncommitted, spans C + E.1 + D + E.2 + E.3 + F): **+2307 / −307**, 19 modified + 4 deleted + 9 new files/dirs. Branch `nuclearshiz` clean against `origin`. Ready for commit + push when the user runs git themselves.

## Post-Phase-P plan

The path from "Phase P done" to "testers receive ALBEDO" is now four steps (replacing F.4's showcase migration which is cancelled):

1. **Connectivity audit** — walk every public surface across `src/` and `crates/albedo-server/src/`; verify each has a production caller, is consumed by a test, or is intentionally an internal API. Catch orphaned modules that survived Phase P but have no live wire-through. Same shape as the original Phase P audit, run with everything wired in.
2. **Cleanup of stale ALBEDO installs** — remove any prior `cargo install`'d versions on the user's PC, prune old local checkouts at other paths. Land the post-P binary as the only one in $PATH.
3. **Build a real demo app** — a full-fledged web application using post-P ALBEDO: file-based routes, `useSharedSlot`, `action()` + `broadcast()`, layout chain, error/loading boundaries, CSS modules. This is what testers see — NOT the deleted showcase. The user has not yet specified the app shape; design is open.
4. **Tester drop** — ship the binary + the demo app to first external testers.

## Stream B — what landed (`f625be6` "Albedo server scaffold")

The first stream of Phase P. Pushed 2026-05-26.

**Schema additions** (`src/manifest/schema.rs`):
- `TierBNode.initial_html: Option<String>` — pre-rendered HTML.
- `TierBNode.initial_opcode_frame: Vec<u8>` — bincode-encoded `OpcodeFrame` (`BindEvent` + `SetTextRef` + initial `SlotSet`). Encoding via `encode_frame` matches the runtime wire format.
- `RouteManifest.shared_slot_topics: Vec<String>` — populated from `CompiledProject::shared_slot_topics()`.
- `RouteManifest.action_ids: Vec<RouteActionEntry>` — empty placeholder until Stream C populates.
- `RouteManifest.layout_chain: Vec<String>` — populated from `DiscoveredRoute.layout_chain` via file-path tail-matching.
- `RouteManifest.error_component: Option<String>` + `loading_component: Option<String>` — placeholders for Stream E.2.
- All new fields use `#[serde(default)]` so older manifest JSON still decodes.

**Builder rewiring** (`src/manifest/builder.rs`):
- New `CompiledRenderProject` struct alongside existing `StaticRenderProject`. Built by cloning the Phase J `ComponentProject` and wrapping via `CompiledProject::wrap`.
- New `render_tier_b_inline(component) -> Option<(String, Vec<u8>)>` helper:
  1. Mints fresh `SessionId` + empty `SlotStore`.
  2. Mints fresh `BroadcastRegistry` + dummy mpsc channel (receiver dropped; `try_send` is non-blocking so this doesn't need a Tokio runtime).
  3. Calls `render_entry_with_broadcast` with `hook_compile: true`.
  4. Wraps `Vec<Instruction>` in `OpcodeFrame { frame_id: 0, component_id: Some(id), instructions }` and encodes via `encode_frame`.
- `build_tier_b_node` populates `initial_html` + `initial_opcode_frame` from that helper; graceful fallback to `None` + empty bytes on error.
- New `discover_routes_from_components` + `infer_routes_dir` helpers find `<root>/routes/` by scanning component file paths for `/routes/` segments and call `discover_routes`. Closes audit gap #9.
- New `layout_chain_for_route` + `component_name_for_rel_path` helpers translate `DiscoveredRoute.layout_chain` (file paths) into component names via file-path tail-matching.

**Construction sites patched** (test fixtures across 3 files updated for the new schema fields):
- `crates/albedo-server/src/handlers/streaming.rs`
- `crates/albedo-server/src/render/tier_b.rs`
- `src/budget/report.rs`

**Golden snapshot regenerated**: `tests/fixtures/golden/manifest_v2_test_app_components.json`. Button (a Tier-B fixture) now shows `"initial_html": "<button data-albedo-id=\"4151434149\"></button>"` — visible proof Stream B works end-to-end.

**6 new tests** in `src/manifest/mod.rs::tests`:
- `stream_b_tier_b_node_carries_real_html_and_opcode_frame`
- `stream_b_tier_b_opcode_frame_round_trips_and_carries_phase_k_opcodes`
- `stream_b_tier_a_only_route_has_empty_initial_opcode_metadata`
- `stream_b_falls_back_to_placeholder_when_source_is_missing`
- `stream_b_manifest_build_is_deterministic_across_runs`
- `stream_b_shell_still_anchors_tier_b_placeholders`

**Test counts**: lib 313 → 321 (+8, the extra 2 are from pre-Stream-B cohesion fixes that landed in the same commit). Workspace fully green, no regressions.

**Note on commit message**: `f625be6` is named "Albedo server scaffold" but the commit actually carries Stream B (manifest pre-rendering) PLUS the previously-unpushed cohesion fixes from this session (file-based routing into dev contract, `public/` copy at build). The commit name doesn't reflect this fully — worth remembering when reading log history.

## Lessons worth keeping from Stream B

### Building CompiledProject at manifest-build time is cheap

`CompiledProject::wrap(component_project.clone())` does one parse-time metadata extraction pass over already-parsed `ParsedModule`s. The `Clone` on `ComponentProject` copies the parsed AST, which is the largest allocation, but the metadata extraction (hook / event / form / link / shared-slot) is millisecond-scale. No reason to skip this in production builds — it's the price of admission for real-HTML manifests.

### Build-time render uses fake plumbing safely

`render_entry_with_broadcast` requires a `SessionSlotView` + `BroadcastRegistry` + `BroadcastSender`. At build time, none of these need real backing:
- `SessionId::random()` for a one-shot session.
- Fresh empty `Arc<SlotStore>` — no per-session state at build.
- Fresh `BroadcastRegistry` — populated lazily by `auto_subscribe` as it discovers topics.
- `tokio::sync::mpsc::channel::<Vec<u8>>(16)` with the receiver dropped. The broadcast registry's `try_send` is non-blocking; full / closed channel is observed and the subscription is pruned. No Tokio runtime needed at build time.

This pattern can be reused by Stream A (production server startup) and Stream D (dev mode promotion) — they just substitute real plumbing in place of these fakes.

### Tier classification rules are subtle

A Component with `effect_profile.hooks = true` AND `is_interactive = true` lands in Tier-C, not Tier-B. Confirmed by reading `src/effects.rs::decide_tier_and_hydration`: the hook branch checks `is_interactive` and promotes to Tier-C if true. Test fixtures that want a Tier-B useState component MUST leave `is_interactive = false`. Bit me twice during Stream B test writing — worth surfacing in docs near `decide_tier_and_hydration`.

## What's next

Critical path: **B (done) → A + C → D → E → F → G**.

**Most likely next move**: Stream A (the production server boot). With Stream B's per-route `shared_slot_topics` + `layout_chain` available in the manifest, Stream A can construct a real `AlbedoServer` from the artifacts dir + register the CompiledProject for action dispatch. ~2 sessions per the plan.

Alternative: Stream C (TS action extractor) if you'd rather close the JSX-side gap before the production server boot. Either is unblocked.
