---
name: project-phase-l
description: "Phase L (DETONATION) — forms, Navigate, Link, CSRF. What shipped, the architectural decisions, the load-bearing lessons."
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

Phase L is complete. The "load-bearing piece of the framework — forms and navigation" gate from the sprint plan is met programmatically; manual browser verification is ad-hoc.

**Why this memory exists:** Phase L spread across multiple sessions and several non-obvious decisions (cookie session round-trip, action-name vs action-id, the resolver race in the multi-thread runtime). Future sessions starting cold should not re-derive these.

**Commits**: `34ede8c` (NCL01 inspector fix), `12f0f31` (Phase L cleanup and wiring), `299893d` (also carries the action-name fix in albedo-server-demo).

## What landed (load-bearing surfaces)

**Compile-time JSX transforms** (`src/transforms/`):
- `link.rs` — extracts `<Link href>` elements + metadata
- `form.rs` — extracts `<form action="action:NAME">` + the action name + field names; exports `FORM_ACTION_PREFIX`, `allocate_form_action_id`, `allocate_field_error_id`
- Both follow the same source-traversal-order pattern as the Phase K hook/event extractors

**Renderer stamps** (`src/runtime/eval/core.rs::eval_jsx_element`):
- `<Link>` rewrites to `<a href="..." data-albedo-link>` (host-element path)
- `<form action="action:NAME">` rewrites: `action=...` attribute stripped, replaced with `data-albedo-action="NAME"`; CSRF placeholder input injected as first body child; thread-local FORM_ACTION_STACK pushes the action name so descendant `<input>/<select>/<textarea>` with `name` attributes emit sibling `<span data-albedo-error="FIELD" data-albedo-id="HASH"></span>` error spans
- All stamps preserve the existing `data-albedo-id` Phase-J shell-stamp contract

**Server-side typed form actions** (`crates/albedo-server/src/render/`):
- `form_action.rs` — `TypedFormActionHandler<T>`, `FromFormPayload` trait, `form_action_handler::<T>(...)` + `form_action_handler_json::<T>(...)`, `form_action_id(name)`
- `form_validation.rs` — `validation_error_text_opcodes(action_name, errors)` builds `SetText` opcodes that target the renderer-stamped error spans
- `csrf.rs` — `CsrfRegistry` (per-session token store), `substitute_csrf_token_in_html`, `read_session_cookie`, `build_session_set_cookie`, `ALBEDO_SESSION_COOKIE = "albedo-session"`, `CSRF_FIELD_NAME = "_csrf"`

**Builder ergonomic** (`crates/albedo-server/src/server.rs`):
- `AlbedoServerBuilder::register_form_action::<T>(action_name: &str, handler)` — takes the action *name*, derives the wire `action_id` via FNV-1a-32 internally. Same hash family as the compile-time `transforms::form::allocate_form_action_id`.
- `AlbedoServer::csrf_registry()` exposed for tests and userland mint paths.

**CSRF gate** (`crates/albedo-server/src/handlers/action.rs`):
- `run_action_request` reads `_csrf` field from JSON payloads via `extract_csrf_field`. If present, validates against `CsrfRegistry`. Mismatch → 403. Non-form payloads (no JSON, or JSON without `_csrf`) skip the check entirely. Button-click actions remain unaffected.

**CSRF post-render substitution** (`crates/albedo-server/src/handlers/streaming.rs`):
- `build_shell_chunk` mints the per-session token and runs `substitute_csrf_token_in_html(&shell, &token)` after the slot replace. Cost: one `str::contains` + one `str::replace` per response.
- `streaming_handler` reads `albedo-session` cookie, mints `SessionId::random()` if absent, sets `Set-Cookie: albedo-session=...; Path=/; HttpOnly; SameSite=Lax` on the response when fresh.
- WT path (`stream_route_over_webtransport`) uses the WT-handshake session as the CSRF session; documented in code as the design choice.

**Action route session resolution** (`crates/albedo-server/src/server.rs::run_action_route`):
- Reads `albedo-session` cookie first, then falls back to the existing `x-albedo-session` header, then fresh random. Cookie-then-header order means browser-driven flows round-trip automatically.

**Client-side interception** (`assets/albedo-link-forms.js`):
- IIFE that reads `globalThis.__ALBEDO_RUNTIME` and installs three behaviours:
  1. `<a data-albedo-link>` click → `requestRouteRefresh(path)` + `history.pushState(url)`
  2. `<form data-albedo-action>` submit → serialize FormData to JSON → bincode-encode `ActionEnvelope` → POST `/_albedo/action` → apply returned `OpcodeFrame`
  3. `Navigate { url }` opcode handler → same path as Link click

**Build-time delivery** (`src/bin/albedo.rs`, `src/manifest/builder.rs`):
- `albedo_link_forms_template()` includes the JS asset via `include_str!`
- Build emits it to `_albedo/link-forms.js`
- Default shim script always includes `<script type="module" src="/_albedo/link-forms.js"></script>` (loaded after runtime.js, before WT bootstrap)

**Wire** (was already done as "Phase I" before L started):
- `Instruction::Navigate { url: String }` — variant 14, last in the enum
- `LOCKED_WIRE_VERSION = 2`
- `canonical_v1_frame` includes Navigate in the conformance fixture

## The 6-step gate itinerary

The order the work landed in, for future reference:

1. **CompiledProject wiring** (`src/runtime/compiled.rs`): added `forms: Vec<FormExtract>` and `links: Vec<LinkExtract>` to `CompiledComponent`. Called the extractors in `wrap()` alongside hooks/handlers.
2. **Renderer stamps** (`src/runtime/eval/core.rs`): the load-bearing edit. Link tag rewrite + form action stamp + CSRF placeholder + error span emission.
3. **Bootstrap loader** (`src/bin/albedo.rs`, `src/manifest/builder.rs`): ship `albedo-link-forms.js` to the client.
4. **Builder ergonomic** (`crates/albedo-server/src/server.rs`): swap `register_form_action(action_id: u32, ...)` → `register_form_action(action_name: &str, ...)`.
5. **CSRF gate** (`crates/albedo-server/src/handlers/action.rs`): extract `_csrf` field + validate before dispatch.
5.5. **CSRF substitution + cookie round-trip** (`csrf.rs`, `streaming.rs`, `server.rs`): the connective tissue that makes form submits actually work end-to-end.
6. **Integration test** (`crates/albedo-server/tests/form_submit_end_to_end.rs`): 5 cases covering matching CSRF, wrong CSRF, no cookie, no `_csrf` field, cookie session round-trip.

## Lessons worth keeping

### The drain_opcode_chunks ordering vs same-tick resolutions

`FourLaneRuntimePipeline::drain_opcode_chunks()` returns chunks in this order:
1. `drain_async_patch_chunks()` — resolved islands from prior ticks
2. `frame_arena.opcode_results()` — per-tick patches
3. `pending_placeholder_emissions` — placeholders queued THIS tick

In a multi-thread tokio runtime (`flavor = "multi_thread"`, `worker_threads = 1`), a spawned resolver future that's `Poll::Ready` on first poll completes BEFORE `drive_pipeline_tick` runs. The resolution lands on `async_tx` and gets drained as step (1), so the placeholder ends up SECOND in the chunk list.

This caused two test flakes:
- `async_island_enqueue_emits_placeholder_via_streaming_state` (server_wire_integration.rs) — fixed by gating the resolver with a `tokio::sync::oneshot::channel` so the future doesn't complete until the test releases it.
- `stream_route_over_webtransport_ships_shell_binary_patches_and_route_complete` (streaming.rs unit tests) — fixed by draining up to 4 frames and asserting *any* of them carries the Placeholder, instead of requiring it be first.

**Future-test rule**: when writing Phase D async-island tests, either (a) gate the resolver future, or (b) drain multiple frames and find-by-shape. Don't assume same-tick ordering.

### Cookie session round-trip pattern

The CSRF flow ONLY works if the page render and the subsequent action POST resolve to the SAME `SessionId`. The pattern:
- Streaming handler: read cookie (`read_session_cookie`), mint fresh if absent, `Set-Cookie` on response
- Action route: read cookie first, then header, then fresh random
- Both sides hit the SAME `Arc<CsrfRegistry>` minted once in `AlbedoServerBuilder::build()` and shared between `StreamingAppState` and `RuntimeState`

If anyone refactors and accidentally mints two registries, every form POST silently 403s. The shared-Arc invariant is the single point of failure.

### Action name vs action ID

The wire carries `action_id: u32` (a FNV-1a-32 hash). Userland API takes `action_name: &str`. The hash is derived on both sides via `form_action_id(name)` / `allocate_form_action_id(name)` — same bytes, same family. Parity test in `form_submit_end_to_end.rs` guards against future drift.

### Renderer emits + server fills

The renderer outputs `<input type="hidden" name="_csrf" value="" data-albedo-csrf />` with an empty value. The server's `substitute_csrf_token_in_html` does a literal `str::replace` of the marker pattern `value="" data-albedo-csrf` with `value="TOKEN" data-albedo-csrf`. No HTML parser involved — the renderer's output is deterministic. If anyone ever changes the renderer's byte order of those two attributes, the substitution silently no-ops and CSRF fails.

## What's intentionally NOT done

- No auto-error-reporter integration with action / streaming render error paths. `DevErrorRegistry` exists; user code or future work hooks it in.
- Phase D async-island ordering bug (resolution arriving same-tick as placeholder) — fixed in tests by gating/multi-drain, not at the source. The drain ordering may want revisiting if real production resolvers turn out to be fast-but-non-degenerate.
- True per-line source maps (Phase M.4 Stage 2) — current source maps are v3 stubs pointing at original `.tsx` filenames with empty mappings.
