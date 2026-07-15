---
name: project_p6_actions_zod
description: P6 server action() + zod forms — DONE + LIVE-VERIFIED; the last Gate-4 feature; found+fixed 2 real client-path bugs
metadata: 
  node_type: memory
  type: project
  originSessionId: 98a94617-24ae-45da-b474-6bd93239835a
---

**P6 — server `action()` + zod-validated forms — DONE + LIVE-VERIFIED 2026-07-02 (⚠️ UNCOMMITTED).**
The last unbuilt Gate-4 feature (TODO.md:73 checklist item) and Halation's final flagship
feature. Most infra pre-existed (action extraction, QuickJS action pool on at boot via
`with_quickjs_action_engine_pool`, per-field `data-albedo-error` opcode machinery, zod
bundles via A2). The gap was the **action-result channel**, plus two real client-path bugs
dogfooding surfaced.

## The edge-giving design (honors [[feedback_engineering_bar]])
zod stays **pure userland JS**; ALBEDO's contribution is a **compile-time-anchored projection**
of the action's return `{ error: { field: msg } }` onto the form's already-allocated
`data-albedo-error` slots, **reconciled against the form's declared field set** (from
`FormExtract.fields`) so a re-submit never leaves a stale message. The form's field manifest
IS the schema — the runtime never guesses which spans exist. Same "boundary computed at build
time, runtime fills pre-allocated stable-id slots" discipline as the rest of the moat, applied
to the action return.

## Engine changes (compiler crate)
- **`src/runtime/bridge.rs`** — action body wrapped in an inner `(function(){…})()` so a
  userland `return` is *captured*, not leaked. **This also fixed a latent bug**: the old wrapper
  spliced the body straight into the effect-collection `try`, so an early `return {error}`
  escaped the epilogue → effects lost. Envelope grew a `result` lane; `decode_handler_envelope`
  now returns `HandlerOutcome { effects, result }` (result best-effort → `None`). `eval_handler`
  returns the outcome.
- **`src/runtime/form_result.rs`** (NEW) — `project_form_result(action_name, &Value, &[String])`:
  one `SetText` per declared field, filled if named in `error` (string msg) else cleared. Undeclared
  keys / non-string msgs ignored (declared set authoritative). 6 unit tests.
- **`src/runtime/compiled.rs`** — `CompiledProject.action_form_fields: HashMap<u32,(name,Vec<field>)>`
  built in `wrap()` from every component's `forms`; `invoke_action_quickjs_inner` appends the
  projected opcodes after lowering effects. Projection is QuickJS-path only (serve/dev use it; the
  pure-Rust `invoke_action` path has no zod). Zero signature ripple (chose this over surfacing
  result to the server adapter, which would touch every test caller).

## Two REAL client-path bugs found (form actions had NEVER worked end-to-end in-browser)
1. **Envelope key mismatch** — `assets/albedo-link-forms.js` passed snake_case `action_id`/
   `event_kind` but `bincode.js encodeActionEnvelope` reads camelCase `actionId`/`eventKind` →
   threw synchronously inside the submit listener → **no form action ever reached the wire**.
   (runtime.js's own event path used camelCase correctly; link-forms diverged.) One-object fix.
2. **Client-rendered island forms** — a Tier-C island renders client-side and does NOT run the
   SSR form-action transform, so its `<form>` kept the raw `action="action:NAME"` sentinel (no
   `data-albedo-action`) AND its error spans lacked the compile-time `allocate_field_error_id`
   stamps → interceptor didn't fire / opcodes couldn't find spans. **Partial fix:** taught the
   interceptor to also read the raw `action:` sentinel (`resolveFormActionName`) — but span-id
   stamping is still SSR-only. **Deferred gap: "form actions inside client-rendered islands"**
   (needs client-side codegen for the attr transform + span-id stamps). For now **form actions
   must be server-rendered.** `link-forms.js` is `include_str!`'d → rebuild binary after editing.

## Halation forms (both SSR / Tier-A, live-verified)
- **Subscribe** (`routes/layout.tsx` colophon) — `z.string().email()`; action in the layout module.
- **MarginNote** (`components/MarginNote.tsx`, action in `essays/layout.tsx`) — `z.string().min(1).max(280)`.
  Authored as `action(({ event }) => …)` — the closure param is stripped (like the P5 event-arg
  bug), body reads the seeded free `event` (= JSON form payload; CSRF/extra keys stripped by
  z.object). **Gotcha found:** putting the `action` export IN the island's own module (MarginNote.tsx
  as Tier-C) broke layout-island collection (island silently didn't render — synthetic `__action__`
  component in the island module interferes). Fix: action lives in the layout module; **and MarginNote
  was made Tier-A (dropped the P5 char-count island)** so the form + spans are SSR-stamped. A hidden
  `status` input gives a success-message span (reuses the projector: invalid fills `note`, valid
  fills `status` — the reconcile swaps them).
- Verified live on `/essays/what-the-margin-knows` + `/`: invalid → zod msg in-place, valid →
  success + error cleared, **one `POST /_albedo/action` 200**, zero navigation/reload, clean console.
  CSS: warm-rust error (on-brand, not red) + `:empty` collapse in `styles.css`.

## Tests: 427 compiler-lib + 124 server-lib + ts_action(5/3) + form_action_roundtrip(4) + topic(3) all green (`-j2`).
(`cold_start_aggregates_over_multiple_boots` flakes under parallel `-j2` port contention; passes solo.)

## Remaining Gate-4 (all packaging, not features): port-diff writeup, `albedo ship` docker/fly green run,
tester drop, D fuzz (Linux/CI). See [[project_halation_flagship]] / [[project_endgame]].
