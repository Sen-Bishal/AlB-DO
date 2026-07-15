---
name: project_backend_paradigm
description: "FORGE — the backend-less backend (\"the compiler emits the database\"); thesis+roadmap in development-plan/backend.md; + the ALKMY product cosmology"
metadata: 
  node_type: memory
  type: project
  originSessionId: af6f5f72-59c1-4b08-9509-667f25f28ddf
---

**`development-plan/backend.md`** authored 2026-07-03 (brainstormed with the user, plan-mode).
The backend/data direction. Ground truth going in: the engine has **zero** data layer (no DB
adapter/ORM/pool/migrations/API client/cache — confirmed by dep scan + graphify), QuickJS is
sync/no-I/O.

**NAMING — the ALKMY cosmology (DECIDED 2026-07-03).** The repo/engine was called "ALBEDO";
renamed into a product family (alchemy magnum-opus thread + a forge):
- **ALKMY** = the engine/compiler (transmutes TSX → tier-sliced output) + ecosystem umbrella.
- **Phosphor** = the client runtime / paint layer (browser-side hydration + DOM patch apply;
  the shipped `albedo-reactive.js`/`albedo-client.js` + future WASM decoder). "ALKMY forges
  server-side, Phosphor lights it up client-side."
- **ALB'DO** (*albedo*) = the **frontend** product (static/metadata/data-fed-locally).
- **FORGE** = the **backend** product (this doc; the backend-less backend). NOTE: overlaps
  Laravel/Atlassian Forge — downstream trademark item, user chose to keep it (legible/sayable).
- **CTRNI'TAS** (*citrinitas*, the "awakening") = the **AI / self-optimizing intelligence** layer.
- **RUB'DO** (*rubedo* → red → gold → money) = **fintech / financial mgmt** for ALB'DO apps (future).
(ENDGAME.md + STRATEGY.md predate the rename and still write "ALBEDO" = ALKMY/ALB'DO.)

**The paradigm (deliberately NOT "add a data layer"):** *the database is just another tier the
compiler emits* — extend the existing tier-slicer (`decide_tier_and_hydration()`, effects.rs)
to place DATA at build/server/client just like it places UI. The tierless/multitier dream
(Ur/Web, Links, ScalaLoci) but in mainstream React/TSX — that's the wedge (those failed by
inventing bespoke languages).

**Converged design decisions (user-chosen in brainstorm):**
- **Surface = pure escape analysis, ZERO markers** (full Ur/Web inference). Unifying idea:
  *persistent shared state = a slot that outlives the request* → storage is the existing
  `SlotStore` model extended past the request boundary (fits THIS engine, no other).
- **Substrate = pluggable** (`DataSubstrate` trait, mirrors the `DeployAdapter` pattern):
  embedded redb/libSQL default, Postgres ("just Postgres underneath"), edge-KV. The
  patentable core is the **inference + IR**, not the storage engine.
- **Mutations = durable actions (DBOS-style)** — upgrade the P6 `action()` path to
  crash-resumable exactly-once workflows (checkpoint-in-transaction).
- **The closer (= CTRNI'TAS) = point the ENDGAME Part III #1 self-optimizing telemetry loop
  (today aimed at the rANS wire + tiers) at DATA**: auto-index, auto-materialize hot queries as
  DBSP IVM views, re-place data across tiers, train the data-delta rANS table. "The data layer
  learns the app" — uncopyable because only ALKMY owns BOTH compiler and store.
- **QuickJS sync = a FEATURE:** compiler knows the data graph → Rust pre-resolves all data
  BEFORE entering QuickJS → render stays pure-sync, JS never needs async.

**Six pillars** (research anchor × engine seam): escape analysis→schema (graph.rs +
CompiledProject::wrap) · data tier-slicing (effects.rs:88) · auto-migration (schema-diff) ·
content-addressed N+1-free query synthesis (hash = cache key = IVM node id) · DBSP incremental
reactivity (BroadcastRegistry + Tier-C, Phosphor paints the delta) · durable writes (actions.rs
/ invoke_action_quickjs_inner).

**Roadmap:** Phase 0 spike ("where's the backend?" demo on Halation — persistent reactive
guestbook, no schema/query/migration written, survives process-kill mid-write; rides ENDGAME
Stage 1) → P1 inference core → P2 reactive IVM → P3 pluggable substrates + durable writes
(rides Stage 1a, shares stateless-CSRF/deploy work) → P4 closed loop / CTRNI'TAS over data
(rides Stage 2 paywall).

**Two products, two domains (user's core framing, backend.md §11.0):** **ALB'DO** = the
frontend product — static, frontend-heavy, **metadata-oriented, data-fed-in-locally** workloads
(landing pages, docs, content, apps fed from build/config/local/props); renders *given* data;
complete + free/open on its own. **FORGE** = the backend-less backend — owns the
**origin/persistence/life of dynamic, shared, server-authoritative data** (state outliving the
request, mutations, durability, sync); the domain ALB'DO alone doesn't enter. Fused = full
ecosystem. This domain boundary is WHY separate pricing isn't crippleware: ALB'DO is a whole
product for its domain, FORGE is a separate product opening a NEW domain (like a DB product
beside a free framework, not a padlock).

**Product shape (DECIDED 2026-07-03, backend.md Part B):** FORGE ships as a
**separately-branded product**, but the split is **brand/price only, NOT a system split** —
"brand-separate, system-fused, hosted-premium." The moat REQUIRES one fused compiler (ALKMY)
owning both ends (decoupling → back to "framework next to a database," no patent). Free/paid
line = **local-free / hosted-premium**, mapped to the STRATEGY.md solar ladder: Sol(free) = the
WHOLE "no backend" magic on the user's own metal (embedded + BYO-Postgres, inferred
schema/migrations/queries, durable actions) — bigger free-wow than frontend-only; Equinox(Pro)
= hosted FORGE + the self-optimizing data loop (CTRNI'TAS, "DB tunes itself while you sleep");
Umbra/Persephone = heavy opt / sovereign. Obeys STRATEGY's governing rule (*"Sol gives away the
ENTIRE framework; paid tiers sell only compute on our machines"*) — gating the local capability
= crippleware = forbidden. ✅ FORGE tier sub-table added to STRATEGY.md Decision 2 (2026-07-04).

**Still open (per doc):** predictability-vs-magic escape hatch (decide from Phase 0 evidence —
Ur/Web's failure mode). No engine code written yet — this is the vision/design artifact only.
Companion to `development-plan/ENDGAME.md` + `STRATEGY.md` ([[project_endgame]],
[[project_strategy_gtm]]). Builds on [[design_tier_classification]], [[project_a1_bridge]],
[[project_p6_actions_zod]].
