# FORGE — the backend-less backend

*The engine already decides where your UI runs. Teach it to decide where your data lives, and
the backend stops being a system you integrate — it becomes an artifact the compiler emits.*

> Companion to `ENDGAME.md` (the engineering) and `STRATEGY.md` (the business). This doc is
> both: **Part A is the thesis** — the paradigm and why it changes web development; **Part C
> is the roadmap** — the phased build that gets us there without betting the house. Held to
> the ENDGAME standard: nothing claimed that we can't measure, and the hard parts are
> engineered, not imported.

> **Naming — the ALKMY cosmology.** The engine is **ALKMY** — the alchemical transmutation
> core (ordinary TSX → tier-sliced gold) and the umbrella over the ecosystem. Its client-side
> runtime — the layer that hydrates islands and *paints the image onto the screen* — is
> **Phosphor**. ALKMY powers a family of products, named on the magnum-opus thread:
> **ALB'DO** (*albedo*, the white reflective surface) = the **frontend** · **FORGE** (where
> durable state is forged) = the **backend-less backend**, this doc · **CTRNI'TAS**
> (*citrinitas*, the yellow *awakening*) = the **AI / self-optimizing intelligence** ·
> **RUB'DO** (*rubedo*, red → gold → money) = **fintech / financial management** for ALB'DO
> apps (future). *ENDGAME.md and STRATEGY.md predate this rename and still write "ALBEDO" for
> what is now ALKMY (the engine) and ALB'DO (the frontend).*

---

# PART A — THE THESIS

## 0. The one line

> Every framework asks *"how do I connect my frontend to my backend?"*
> ALKMY's answer: **there is no seam, because the compiler never split them into two
> systems.** You write your app. It emits the database.

## 1. The gap we are closing

ALKMY today has **no data story of any kind**. Not a thin one — none. A dependency scan and
graph query confirm: no database adapter, no ORM, no connection pool, no migration tool, no
external-API client, no query cache. Everything server-side runs inside **QuickJS**, which is
synchronous, single-threaded, and has no I/O. A server component that needs data has, right
now, no first-class way to get it.

For a framework whose target user is the mainstream React/Next developer, this is *the*
gap. The first question that developer asks after "how fast is it" is "how do I talk to my
data." We have been unable to answer. This document is the answer — and the answer is not "we
added a data layer." It is a fundamental expansion of what the engine *is*, shipped as its own
product: **FORGE**.

## 2. The reframe: the database is a tier

ALKMY's whole thesis (`ENDGAME.md`) is that a **compiler** takes one ordinary React program
and classifies every component into a hydration tier — Tier A (static, zero JS), Tier B
(server-driven patches), Tier C (client island) — and *that single decision is the game*. The
compiler does the placement a human never would by hand.

The FORGE paradigm is one sentence long:

> **Data is just another tier the compiler emits.**

The same tier-slicer that decides whether a *component* renders at build, on the server, or on
the client, is extended to decide whether a piece of *data* is baked at build (Tier-A static
data), fetched per-request (Tier-B), or synced reactively to the client (Tier-C). "Backend"
was never a separate thing. It was just the set of tiers the compiler hadn't been taught to
target yet.

This is the **tierless / multitier programming** dream — one program, compiler-sliced across
client, server, and database, with all the boilerplate (connections, queries, serialization,
the wire protocol) generated. It is not new as an idea: **Ur/Web**, **Links**, **Eliom**, and
**ScalaLoci** have pursued it for two decades. Ur/Web even generates the SQL and *proves it
safe at compile time*. ([Multitier survey (ScalaLoci)](https://scala-loci.github.io/publications/2020-csur/paper.pdf) ·
[Ur/Web: A Simple Model for Programming the Web](https://www.researchgate.net/publication/279835223_UrWeb_A_Simple_Model_for_Programming_the_Web))

**Here is why it never went mainstream, and why that is our entire opening:** every one of
those systems invented a *new language* nobody wanted to learn. Ur/Web is its own ML dialect.
They had the right idea and the wrong surface. ALKMY does tierless slicing in the **React/TSX
that ten million developers already write** — and it is *already a tier-slicing compiler*. We
are not building the idea from zero. We are one compiler pass away from applying machinery that
already exists in this repo to a new target.

## 3. The unifying mechanism: persistence is *inferred*, not configured

The engine already has request-scoped reactive state: a `useState` call becomes a slot in the
`SlotStore`, written on the server, hydrated on the client by **Phosphor**, reacted to through
the signal graph. The entire paradigm collapses to a single observation:

> **Persistent, shared state is just a slot that outlives the request.**

A `useState` value lives for one request. A value that the compiler can *prove* escapes
request scope and is shared across sessions is, by definition, a database row. The compiler's
job is to notice that boundary crossing.

We chose the most radical surface deliberately: **pure escape analysis, zero developer
markers.** The developer does not write a schema, a model, a `db` object, or an annotation.
They write ordinary data structures and use them. A compiler pass over the component
dependency graph performs lifetime/escape analysis and determines which values persist. This
is the Ur/Web dream taken to its end: *the persistence is latent in the code, and the compiler
forges it into storage.*

The consequence is that **storage is not a new subsystem bolted onto the engine.** It is the
*existing slot model extended past the request boundary.* That is why this design fits **this**
engine and no other — the reactive substrate that already re-renders an island on a state
change is the same substrate that will re-render it on a *stored data* change.

## 4. The six pillars

Each pillar is a real research result crossed with a real seam in the current engine. Nothing
here is hand-waved; the seam map is in §7.

**Pillar 1 — Persistence by escape analysis (the surface).**
A new compiler pass over `ComponentGraph` performs escape/lifetime analysis: which values
outlive the request and are shared across sessions → the **inferred schema**. Zero markers.
*Anchor:* tierless slicing (Ur/Web, Links, ScalaLoci).

**Pillar 2 — Data is tier-sliced.**
`decide_tier_and_hydration()` gains a data input and places each datum: bake at build (Tier-A
static data), fetch per-request (Tier-B), or sync to client (Tier-C). One decision function,
one new input — the mechanism that already exists, pointed at data.

**Pillar 3 — Automatic schema + migrations.**
The compiler diffs the inferred schema between builds and emits the migration as a build
artifact. **The developer never writes a migration.** *Anchor:* schema-evolution synthesis —
[PRISM / Migrator](https://arxiv.org/pdf/1904.05498), [automatic schema-evolution recommendation](https://arxiv.org/html/2404.08525v1).

**Pillar 4 — Query synthesis, content-addressed.**
Because the compiler sees *every* read across the whole component graph, it synthesizes
provably-safe, **N+1-free** queries — it can batch and hoist reads to the route boundary that
an ORM, seeing one component at a time, never could. Each query is **content-addressed**: its
structural hash is simultaneously its cache key and its incremental-view node id. *Anchor:*
[Ur/Web slicing](https://www.researchgate.net/publication/279835223_UrWeb_A_Simple_Model_for_Programming_the_Web) +
[sqlc](https://sqlc.dev/) (compile-time type-safe SQL); content-addressing from
[Unison](https://www.unison-lang.org/docs/the-big-idea/) and [Datomic](https://docs.datomic.com/transactions/model.html).

**Pillar 5 — Incremental reactivity (differential dataflow).**
Storage deltas propagate through the **same signal graph** that re-renders islands — the
`BroadcastRegistry` fan-out, out over the wire, and **Phosphor paints the delta into the DOM**
on the client. A stored-row change and a UI re-render are *one dataflow graph*, server to
screen. Recompute the delta, never the whole query. *Anchor:*
[DBSP: Automatic Incremental View Maintenance](https://arxiv.org/pdf/2203.16684),
[Differential Dataflow](http://michaelisard.com/pubs/differentialdataflow.pdf),
[Riffle](https://riffle.systems/essays/prelude/).

**Pillar 6 — Durable writes (crash-resumable actions).**
The P6 server `action()` — already the mutation entry point (`HandlerOutcome{effects}`) — is
upgraded into a **durable, exactly-once workflow**: the workflow checkpoint is piggybacked
inside the same transaction as the write, so a crash mid-action rolls back cleanly and
resumes. "Server actions that survive a crash and resume" is a headline a funder grasps in
one sentence. *Anchor:* [DBOS / Stonebraker, CIDR 2026](https://www.vldb.org/cidrdb/papers/2026/p9-stonebraker.pdf),
[dbos-transact-ts](https://github.com/dbos-inc/dbos-transact-ts).

## 5. The closer — the data layer *learns the app* (this is CTRNI'TAS)

`ENDGAME.md` already commits to a doctrine (Part III #1, and Movement I.3 "PGO for the wire"):
**runtime telemetry flows back into the compiler to retune the app.** Today that loop is aimed
at the *wire* (retune the per-app rANS entropy table) and *tiers* (redraw hydration
boundaries). "ALKMY learns the app it serves" is already the flagship funder demo. In the
product cosmology, that self-optimizing intelligence is its own layer — **CTRNI'TAS** — and
FORGE is the highest-value surface to point it at.

The highest-value target for that exact loop is **data**, and pointing it there is what makes
this paradigm uncopyable:

- A **database** can auto-index, but it cannot see your components — so it can never re-place
  data across build/server/client tiers.
- A **framework** can cache, but it does not own storage — so it can never rewrite the schema.
- **ALKMY owns both ends of the compile→serve loop.** It is the only system that can close
  the telemetry loop *over data.*

Escape analysis gives the *initial* placement. The runtime then observes real behaviour — hot
rows, read/write ratios, which synthesized queries actually fire, data-tier mispredictions —
and CTRNI'TAS feeds it back so the compiler:

- **auto-creates indexes** for the reads that actually happen,
- **auto-materializes hot queries** as incremental (DBSP) views,
- **re-places data across tiers** (a "static" table that turns out to churn is demoted to
  Tier-B; a per-request read that is effectively constant is promoted to Tier-A),
- **trains the per-app rANS table** for the data-delta wire.

The database *tunes itself to the application*, using information only the compiler-plus-runtime
pair possesses. This is the single most differentiated — and per `STRATEGY.md`, most
monetizable (hosted, therefore uncopyable) — capability in the whole system.

## 6. Why QuickJS's synchronous constraint is a *feature*

The seam map flagged exactly one blocker: QuickJS handlers cannot `await` — evaluation is
synchronous. Every other framework would fight this. For this design it *inverts into the
argument for the whole architecture.*

Because the compiler knows the full data graph ahead of time, **Rust pre-resolves all data
before entering QuickJS.** The render stays pure and synchronous; data-fetching happens in the
fast native layer; the JS never needs async at all. The constraint that would shatter a
bolt-on data layer (where JS reaches out to a DB mid-render) is precisely what *forces* the
correct compile-time architecture: know the data ahead of time, fetch it natively, hand it to
a pure render. Forced elegance — the kind a technical funder leans forward for.

## 7. Correspondence to the engine (the seams are real)

| Pillar | Engine seam — `file : symbol` |
|---|---|
| Escape analysis / schema | `crates/dom-render-compiler/src/graph.rs` (`ComponentGraph`, `get_dependencies`); new `data_deps` extracted in `runtime/compiled.rs` `CompiledProject::wrap()` |
| Data tier placement | `crates/dom-render-compiler/src/effects.rs:88` `decide_tier_and_hydration()` (+ new data input) |
| Reactive substrate / deltas | `runtime/slot_store.rs` `SlotStore`; `crates/albedo-server/src/runtime/broadcast.rs` `BroadcastRegistry`; Phosphor client runtime applies the patch |
| Pre-resolution before JS | `crates/albedo-server/src/server.rs` `RenderWorld` (+ `DataContext` / `DataSubstrate`); render-path pre-fetch |
| Durable writes | `crates/albedo-server/src/actions.rs` `ActionHandler::handle`; `runtime/compiled.rs` `invoke_action_quickjs_inner` |
| Pluggable substrate | new `DataSubstrate` trait — mirrors the `DeployAdapter` trait pattern from `ENDGAME.md` Stage 1a |

The design deliberately mirrors two structures the engine already has: the **adapter
architecture** (Stage 1a's `DeployAdapter`) becomes `DataSubstrate`, and the **manifest**
(`RenderManifestV2`, already a per-route metadata carrier) is where the inferred schema and
query plan ride.

## 8. Substrate: pluggable, so the novelty is the inference, not the storage

FORGE emits to a `DataSubstrate` *interface*, not a fixed engine. Targets:

- **Embedded store** (redb / libSQL) — in-process, zero external dependency, the default for
  `albedo serve` and the demo.
- **Postgres** — instant credibility and adoption ("it's just Postgres underneath"), and the
  natural transactional home for durable actions (Pillar 6).
- **Edge-KV** — ties to the deployment manifold and the eventual edge adapter.

This is the crucial defensibility move: **the patentable, ALKMY-owned core is the inference
pass and its intermediate representation** — the compile-time data-dependency graph, the schema
inference, the content-addressed query IR, the tier placement of data, and the telemetry
retuning loop. The storage engine underneath is a swappable backend. We are not claiming "we
built a better database." We are claiming "we built the compiler that makes the database
disappear." That is both harder to copy and easier to adopt.

## 9. Prior art, and why this is defensible

The honest position: **none of the individual ingredients is unprecedented.** Tierless
languages exist. IVM engines exist (Materialize, Feldera). Durable execution exists (DBOS,
Temporal). Content-addressed data exists (Datomic, Unison). Sync engines exist (Zero, Convex,
ElectricSQL). Compile-time query safety exists (sqlc, Quill, TyQL).

What does **not** exist anywhere is the **combination**, and the combination is the invention:

1. a **mainstream React/TSX surface** (not a bespoke research language),
2. a compiler that **already tier-slices** and extends that same pass to data,
3. **full-graph query synthesis** possible *because* the compiler sees every component,
4. **IVM reactivity fused with UI reactivity** as one graph,
5. a **synchronous runtime that forces native pre-resolution**, and
6. a **runtime→compiler telemetry loop that retunes storage** (CTRNI'TAS) — possible only
   because one system owns both ends.

No database has (2)(3); no framework has (3)(6); no tierless language has (1). That is the
claim.

## 10. Honest risks (non-negotiable — the ENDGAME voice)

- **Full inference is genuinely hard and can feel like unpredictable magic.** Ur/Web proved
  the idea and remained a research artifact, partly because "the compiler decided your schema"
  unsettles engineers who want control. *Mitigation:* Phase 0 must prove predictability on a
  real app (Halation) before we widen inference; the defensible claim is the *combination*,
  not inference alone; and a later escape-hatch (override any query/index/placement) is on the
  table if predictability demands it.
- **IVM over rich queries is a research-grade engine.** DBSP/Feldera are heavy. Phase 2 scope
  must stay deliberately bounded (start with the query shapes the flagship actually uses).
- **Durable execution needs a real transactional substrate.** It pairs with the Postgres
  target and with ENDGAME's stateless-CSRF / deploy work — do not build it against the
  in-memory store.
- **This is Part II/III-scale work.** It is not a weekend. The roadmap below is sequenced so
  the *first* phase is a genuine, showable demo and everything after is trajectory — and per
  `STRATEGY.md`, funders buy trajectory, not completeness.

---

# PART B — PRODUCT SHAPE & POSITIONING

FORGE ships as a **separately-branded product** in the ALKMY ecosystem. But the separation is a
**product/brand/pricing boundary, not a system boundary.** This distinction is load-bearing and
non-negotiable:

> **Brand-separate. System-fused. Hosted-premium.**

## 11.0 Two products, two domains (the boundary that makes this legitimate)

ALB'DO and FORGE split cleanly along *what kind of data the app has*:

- **ALB'DO — the frontend product.** Excels at **static and frontend-heavy, metadata-oriented
  workloads**: landing pages, marketing sites, docs, content/MDX, dashboards, and web-apps
  whose data is **fed in locally** — build-time, config, local files, or request props. It
  renders *given* data across ALKMY's tiers brilliantly and ships near-zero JS. **Complete and
  excellent on its own** for this domain, and free/open (its own Sol tier), deploy-anywhere.
- **FORGE — the backend-less backend.** Owns the **origin, persistence, and life** of
  *dynamic, shared, server-authoritative* data: state that outlives the request, mutations,
  durability, reactive sync — the domain ALB'DO alone does not enter.

Fused, they are a **complete full-stack ecosystem** — *the power of ALB'DO plus a novel
backend-less backend.* **Priced separately:** FORGE carries its own pricing ladder, and the
higher-priced "full ecosystem" path is ALB'DO + FORGE's hosted tiers.

**This boundary is *why it is not crippleware.*** ALB'DO is not the hobbled half of one
product — it is genuinely complete for static / frontend / metadata apps. FORGE is a
**separate product that opens a new domain** (the dynamic backend), the way a database product
sold beside a free framework is a separate product, not a padlock. STRATEGY's governing line
holds: *ALKMY-the-engine and ALB'DO are given away whole;* FORGE is an additional product, and
even *its own* free tier (local, self-hosted backend-less-backend) stays excellent — only its
**hosted** ecosystem is the paid upsell.

## 11.1 One compiler, two SKUs (why the fusion is non-negotiable)

The entire invention (Part A) exists *only because one compiler owns both ends* — whole-graph
query synthesis, pre-resolution before QuickJS, the single telemetry loop. If FORGE were a
**decoupled system** talking to ALB'DO over a wire, all of that evaporates and we are back in
the crowded, unpatentable "a framework next to a database" lane.

So FORGE is **not** a second system. It is a product name and a price drawn *around part of
what the one fused compiler emits.* ALKMY emits UI *and* data from one graph; "FORGE" is simply
how we brand, position, and (for the hosted pieces) charge for the data half. The moat is the
fusion; the brand boundary must never become a system boundary.

## 11.2 The free/paid line (reconciled with STRATEGY.md's governing rule)

STRATEGY.md's hard-won, never-violate line:

> *"Sol gives away the **entire** framework; every paid tier sells only compute that runs on
> our machines… gating capabilities of a binary on the user's own metal is **crippleware** —
> forkable, circumventable, trust-destroying."*

Therefore the split is **local-free / hosted-premium**, mapped onto the solar pricing ladder:

| Tier | Name | What FORGE gives (on top of the frontend) |
|---|---|---|
| **Free** | **Sol** | The **entire "no backend" magic on your own metal** — escape-analysis persistence, inferred schema, auto-migrations, content-addressed query synthesis, durable actions, the embedded substrate *and* BYO-Postgres. Deploy anywhere. Never gated. |
| **Pro** | **Equinox** | **Hosted FORGE** — managed substrate + the **self-optimizing data loop** (CTRNI'TAS: "your database tunes itself while you sleep") + graphify-grounded AI over your data + generous cloud usage. |
| **Ultra** | **Umbra** | Priority heavy data optimization (full IVM materialization + rANS-trained data-delta wire on dedicated compute), cross-region sync, team seats/RBAC over shared data insights. |
| **Enterprise** | **Persephone** | Sovereign / on-prem managed data, data-sovereignty, compliance, dedicated isolation. |

**Why this is strictly better than "charge for the backend at all":**
- It is **guardrail-clean** — the local capability is never padlocked, so no crippleware, no
  fork incentive, no adoption poison.
- It is a **bigger free-wow**, not a smaller one — "it wrote my schema *and* there's no
  server *and* it's free on my metal" is a stronger "how is this possible" moment than
  frontend-only. Bigger wow → more adoption → more expansion.
- The premium is **hosted, therefore uncopyable** — the self-optimizing data loop (CTRNI'TAS)
  can only run where ALKMY owns the compute, exactly like the wire self-opt loop already does.
- **Same paywall axis as the existing soul.** FORGE's premium sits on the identical line as
  ENDGAME Part III #1 / Equinox — we are not inventing a new monetization surface, we are
  extending the one STRATEGY.md already sanctioned.

## 11.3 The expansion story (land full-stack free, expand to the hosted ecosystem)

ALB'DO + FORGE is one continuous product, not two purchases. A developer lands on the free,
full-stack "no backend" magic (bigger wow than the frontend alone), falls in love self-hosting
it, and *then* the hosted ecosystem — a database that optimizes itself, durable execution at
scale, managed sync — becomes the second hit that captures the company's budget. **Bottom-up
love → top-down contracts**, per STRATEGY Decision 2, now with a full-stack surface area
instead of a frontend one. The higher price of the "full ecosystem" is real — but it buys
*hosted intelligence*, never the right to have a backend. And it is the natural on-ramp to the
rest of the cosmology: CTRNI'TAS (intelligence) and, later, RUB'DO (fintech for ALB'DO apps).

*(Note: STRATEGY.md's tier table should eventually gain a FORGE row per the above — flagged,
not yet edited, since STRATEGY is its own doc.)*

---

# PART C — THE ROADMAP

Five phases, mapped onto the `ENDGAME.md` stages so this slots into the existing plan rather
than competing with it. Tags mirror ENDGAME: `[demo]` a funder-visible milestone ·
`[core]` the load-bearing engineering · `[research]` the deep end.

## Phase 0 — The spike: "wait, where's the backend?" `[demo]`
*Goal: the 60-second moment. Prove the thesis end-to-end on Halation with the thinnest
possible slice.*

- One embedded `DataSubstrate` (redb or libSQL).
- Escape-analysis pass that detects **a single persistent collection** (not the general case).
- Pre-resolve that collection into props before QuickJS render (native fetch, sync render).
- One **durable** `action()` write through the P6 path.
- **Demo:** a persistent, reactive feature on Halation (reader reactions / guestbook) where the
  author wrote **no schema, no migration, no query, no ORM** — and it **survives a process
  kill mid-write** (durable action resumes/rolls back). Diff this against a Next+Prisma
  equivalent — that port-diff is the brag.

*Exit criterion:* a private engineer watches it and says "where's the backend?" Ships against
ENDGAME **Stage 1** (adoption) as a differentiation teaser.

## Phase 1 — The inference core `[core]`
*Goal: turn the one-collection spike into the general compile-time data model.*

- Full escape/lifetime analysis over `ComponentGraph` → **inferred schema** for arbitrary
  persistent state.
- **Auto-migration**: diff inferred schema between builds → emit migration artifact.
- **Content-addressed query synthesis** from whole-graph access patterns; N+1 elimination by
  construction.
- Fold data into `decide_tier_and_hydration()` → build/server/client placement of data.

*Exit criterion:* build a non-trivial data-backed app with **zero** hand-written schema/query/
migration; the emitted artifacts are inspectable and correct.

## Phase 2 — Reactive IVM `[core]/[research]`
*Goal: make stored data reactive through the existing signal graph.*

- DBSP-style incremental view maintenance for the synthesized queries (bounded to real
  flagship query shapes first).
- Deltas ride `BroadcastRegistry` + the Movement I wire codec → **Phosphor** paints reactive
  queries server→screen as **one dataflow graph**.
- Content-addressed query hash = IVM node id = cache key (the identity unification).

*Exit criterion:* a stored write in one session reactively updates every subscribed client with
a **delta**, not a re-fetch, with no full reload.

## Phase 3 — Pluggable substrates + durable writes at scale `[core]`
*Goal: adoptability and the transactional write story.*

- Postgres target behind `DataSubstrate` ("it's just Postgres underneath").
- Edge-KV target; wire into the deployment manifold (ENDGAME **Stage 1a**).
- Durable actions (Pillar 6) fully realized against the transactional substrate;
  exactly-once, crash-resumable, checkpoint-in-transaction.

*Exit criterion:* the flagship deploys with a real Postgres substrate and its writes are
provably durable across an induced crash; `albedo serve` on the embedded store is byte-behavior
identical.

## Phase 4 — The closed loop: CTRNI'TAS over data `[research]/[demo]`
*Goal: the uncopyable capability. The data layer learns the app.*

- Telemetry capture: access patterns, read/write ratios, query firing, data-tier
  mispredictions.
- Compiler retuning (CTRNI'TAS): **auto-index**, **auto-materialize** hot queries as IVM
  views, **re-place** data across tiers, **train** the data-delta rANS table.
- Unify with ENDGAME Part III #1 (same loop, new target) → one self-optimization story
  covering wire, tiers, *and* storage.

*Exit criterion:* ship a measurably faster / cheaper data path from captured telemetry, with no
developer intervention. Candidate for the ENDGAME **Stage 2** paywall (hosted → uncopyable).

## Sequencing, cut order, and the never-cut line

- **Slots onto ENDGAME:** Phase 0 rides Stage 1 (adoption teaser); Phase 3 rides Stage 1a
  (deploy manifold, shares the CSRF/stateless work); Phase 4 rides Stage 2 (soul-as-moat,
  same telemetry loop as the wire).
- **Cut order if time runs short:** Phase 4 depth → Phase 3 edge-KV → Phase 2 rich-query IVM.
- **Never cut:** Phase 0 (the demo that proves the thesis) and the honest-inference guardrail
  (predictability before breadth). Those are the promise and the pitch.

## Verification (the work proves itself)

- **Phase 0:** on Halation, author writes only a component that reads+writes a collection →
  `albedo build` emits schema + migration artifacts (observe them) → `albedo serve` → in
  browser: write persists across reload, reactive update with no full reload → kill the
  process mid-write → the durable action resumes/rolls back cleanly. Port-diff vs Next+Prisma
  documented.
- **Phase 1:** a data-backed app builds with zero hand-written schema/query/migration; emitted
  SQL is inspected and N+1-free.
- **Phase 2:** two-tab demo — a write in tab A delivers a *delta* to tab B via Phosphor
  (measure bytes; no re-fetch).
- **Phase 4:** the two-build demo — a second build trained on captured telemetry serves a
  measurably faster data path than the cold build.

## Open questions (to resolve as we build, not before)

1. **Naming — DECIDED.** Engine = **ALKMY**; client runtime = **Phosphor**; frontend =
   **ALB'DO**; backend (this doc) = **FORGE**; AI/self-optimizing = **CTRNI'TAS**; fintech
   (future) = **RUB'DO**. (Working notes: "FORGE" overlaps Laravel/Atlassian Forge — a
   downstream identity/trademark item, not a blocker.)
2. **Predictability vs. magic.** Do we ever expose an escape hatch (override an inferred
   query/index/placement), or hold the line on pure inference? Decide from Phase 0 evidence.
3. **IVM scope.** Which query shapes make the Phase 2 cut? Drive from the flagship's real usage.
4. **Consistency semantics under sync.** Optimistic client mutation + IVM reconcile is a
   natural Phase 2+ layer on top of durable actions — sequence it once the write path is solid.

---

*ALKMY already refuses to make the developer choose where their UI runs — the compiler decides,
better than a human would. FORGE is the same refusal, one layer deeper: stop making the
developer stand up, wire up, and reconcile a second system. Infer it. Forge it. Tune it to the
app. Then there is no backend — there is only the program, and the compiler that knows it
whole.*
