---
name: project_forge_capability_budget
description: "budget_forge.md — exhaustive FORGE backend capability catalog (freedom ladder, 4 data tiers, coverage matrix to roadmap)"
metadata: 
  node_type: memory
  type: project
  originSessionId: 7b460d5d-b2cc-4cff-a318-c638168a4f6b
---

**`development-plan/budget_forge.md`** (git-ignored, private) authored 2026-07-06 — an
exhaustive executive brief cataloguing EVERY backend/data capability an engineer might need
(even niche), categorized, and mapped onto the FORGE roadmap/pillars in [[project_backend_paradigm]].
Purpose: choose scope against the whole map instead of discovering gaps mid-build. Reference +
brainstorming artifact, NOT commitments.

Key framing contributions (these are the reusable ideas, beyond the catalog):
- **The freedom ladder (progressive disclosure)** — resolves `backend.md` Open Q#2
  (predictability vs magic): **L0 inferred (pure escape analysis) → L1 hint/override → L2
  explicit primitive → L3 substrate/raw-SQL escape.** Every capability must be reachable at
  *some* layer; the refused failure mode is "reachable at no layer." Magic is the default, not
  the ceiling.
- **The four data tiers** (the organizing spine, from the slot-tier brainstorm in
  [[project_forge_phase0]]): **build / slot / request / client.** Placement is
  compiler-decidable: query closing over request/session ctx (e.g. `currentUser`) → request
  tier; param-free globally-shared → slot tier. Slot tier = `BroadcastRegistry`+Phosphor (free
  live reactivity, static lists only); request tier = `DataDep`/`DbQuery` fetch.
- Parts V (§25–28) connect catalog→roadmap: P0=single collection, P1=inference core+auto-migrate,
  P2=IVM/reactive, P3=substrates+durable+jobs+blobs, P4=CTRNI'TAS self-opt, P5+=CRDT/bitemporal/geo.

8 insights it surfaced (roadmap candidates, §27): (1) freedom ladder resolves Open Q#2;
(2) **inferred authorization** — same `currentUser`-read analysis that picks the request tier
can *emit RLS predicates* ("compiler wrote your auth"), P1-explorable; (3) job/workflow plane =
Pillar 6 durable-writes *generalized*, one primitive not a new subsystem; (4) substrates are a
*family* — name `BlobSubstrate`(files)/`SearchSubstrate` before P3; (5) free wins from
knowing-graph-whole: SQLi-safe-by-construction, GDPR-delete-cascade, OpenAPI emission;
(6) IVM is secretly an analytics engine (live reactive-aggregate dashboards); (7) emitted-query
inspection is the predictability keystone (never-cut); (8) CQRS/event-sourcing fall out of the
tier split free. §28 adds 3 never-cuts: freedom ladder, artifact inspection, 4-tier decidability.
§29 draws the delegate-don't-build border (video transcode, msg brokers, search engines,
compliance posture) — test: *does it benefit from knowing the data graph whole?*
