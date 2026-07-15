# FORGE — Capability Budget

*An exhaustive accounting of every backend and data-driven capability a software engineer
might reach for — mapped onto the FORGE paradigm and roadmap, so scope is chosen with the whole
territory in view rather than discovered mid-build.*

> Companion to `backend.md` (the thesis + Part C roadmap). Where `backend.md` argues *why* and
> sequences the *first* phases, this document is the **map of the entire country**: what a
> "complete" backend surface even *is*, category by category, down to the niche cases — so that
> when we harden scope we are cutting from a known whole, not hoping we remembered everything.
>
> This is a **reference and brainstorming artifact**, not a commitment. Listing a capability
> here is not a promise to build it; it is a promise to have *considered* it and decided,
> consciously, where it sits: inferred, escape-hatched, substrate-delegated, hosted, or
> deliberately out of scope.

---

## 0. How to read this document

### 0.1 The governing tension, and its resolution

FORGE's thesis is **pure escape-analysis inference, zero markers** (`backend.md` §3). The
user's mandate for *this* document is the opposite pole: **total freedom of usage — every
capability, even the niche ones, must be expressible, or the framework is a cage.**

These are not in conflict once we stop treating "inference" as the only layer. The resolution
is **progressive disclosure — a freedom ladder.** The magic is the *default*, not the *ceiling*:

| Layer | Name | What the developer does | Example |
|---|---|---|---|
| **L0** | **Inferred** | Nothing. Writes ordinary React/TSX; the compiler forges the data. | `const posts = ...; posts.push(x)` → a table, a query, a durable write. |
| **L1** | **Hint / override** | Writes normal code, adds an optional declarative annotation *only* when they want to steer a decision. | `useCollection(posts, { index: "createdAt", tier: "request" })`. |
| **L2** | **Explicit primitive** | Calls a first-class FORGE API for intent the compiler cannot read from data flow. | `forge.transaction()`, `forge.job()`, `forge.query(...)`. |
| **L3** | **Substrate escape** | Drops to the raw substrate for the truly bespoke. | Raw SQL, BYO-Postgres extension, a hand-written migration. |

**Rule:** every capability in this catalog must be reachable at *some* layer. A capability that
is L0-inferable is a win; one that needs L2/L3 is still *covered* — freedom preserved — it just
doesn't get the magic. The failure mode we refuse is a capability reachable at *no* layer.
This directly resolves `backend.md` Open Question #2 (predictability vs. magic): **we hold the
pure-inference line at L0 and guarantee an escape hatch exists at L1–L3.**

### 0.2 The four data tiers (the spine everything hangs on)

`backend.md` §2 / Pillar 2 says *data is tier-sliced like UI*. Concretely, escape analysis
places every datum into one of four tiers. This taxonomy is the organizing spine of the whole
catalog — most "which capability" questions reduce to "which tier does this datum live in":

| Tier | Lives | Read | Reactive? | Engine seam it reuses | Good for |
|---|---|---|---|---|---|
| **Build** | Baked into output at compile | inlined static props | no | existing static-props path | constants, content, config, seed |
| **Slot** | Process-global in-memory, substrate-backed | `useSharedSlot` at render (sync, in-mem) | **yes, free** | `BroadcastRegistry` + Phosphor | small, shared, param-free, hot collections (feeds, guestbook, presence, dashboards) |
| **Request** | Fetched per request through `DataContext` | `TierBDataFetcher` → substrate query | on demand | `DataDep`/`DataSource::DbQuery` seam | large, per-user, private, paginated, cold reads |
| **Client** | Local to the browser session | client island state | client-only | Tier-C island | ephemeral UI state, optimistic drafts, offline cache |

The **placement rule is compiler-decidable** (a key result of our slot-tier pressure test): a
query that closes over request/session context (e.g. `currentUser`) → **request tier**; a
param-free globally-shared read → **slot tier**; a compile-time-constant read → **build tier**.
This decidability is what keeps L0 honest.

### 0.3 Paradigm-fit legend (used throughout)

- 🟢 **Inferred (L0)** — falls out of escape analysis; zero config.
- 🟡 **Inferred + hint (L1)** — default is inferred; optional override exists.
- 🔵 **Explicit primitive (L2)** — needs developer intent; a first-class FORGE API.
- 🟠 **Substrate (L3-ish)** — delegated to / normalized over the pluggable `DataSubstrate`.
- ☁️ **Hosted / CTRNI'TAS** — premium, self-optimizing, runs on ALKMY compute (per `STRATEGY.md`).
- ⛔ **Out of scope (candidate)** — plausibly never FORGE's job; note the boundary.

Phase tags map to `backend.md` Part C: **P0**–**P4**, and **P5+** for "beyond the current
roadmap horizon."

---

# PART I — THE CORE DATA PLANE

## 1. Data modeling & schema

| # | Capability | Fit | Tier(s) | Notes / FORGE expression |
|---|---|---|---|---|
| 1.1 | Entities / tables / collections | 🟢 | any | inferred from a persisted collection's shape |
| 1.2 | Scalar types (int, float, text, bool) | 🟢 | any | inferred from usage; TS types are the schema |
| 1.3 | Decimal / money (exact precision) | 🟡 | request/slot | inference can't tell `number`-as-money from float → **L1 hint** or a `Money` branded type |
| 1.4 | Dates / times / timezones / intervals | 🟡 | any | `Date` inferred; tz policy is a hint |
| 1.5 | JSON / document / nested objects | 🟢 | any | a nested object that never escapes to its own collection → JSON column |
| 1.6 | Arrays / lists | 🟢 | any | inferred; array-of-entity → relation, array-of-scalar → array column |
| 1.7 | Enums / unions | 🟢 | any | TS string-literal union → enum/check constraint |
| 1.8 | Binary / blob (inline) | 🟡 | request | small blobs inline; large → **files subsystem (§9)** |
| 1.9 | Nullable vs required | 🟢 | any | `T \| undefined` / optional prop → nullable |
| 1.10 | Default values | 🟢 | any | initializer expression → column default |
| 1.11 | Computed / generated columns | 🟡 | any | a pure derived field → generated column (L1 to force stored vs virtual) |
| 1.12 | Relations 1:1, 1:N, N:M | 🟢 | any | inferred from reference shape; join table synthesized for N:M |
| 1.13 | Self-referential / recursive (trees, graphs) | 🟡 | request | inferred; recursive *queries* need L1/L2 (see §3.14) |
| 1.14 | Polymorphic / heterogeneous relations | 🔵 | request | ambiguous to infer → **explicit union type + L1** |
| 1.15 | Primary keys (auto, UUID, composite, natural) | 🟡 | any | auto-id default; UUID/composite/natural via L1 branded-id or hint |
| 1.16 | Foreign keys + cascade policy | 🟡 | any | FK inferred from relation; cascade (delete/nullify/restrict) is an L1 policy |
| 1.17 | Unique / check / not-null constraints | 🟡 | any | not-null inferred; unique/check from L1 or a validated (`zod`) field |
| 1.18 | Domain / branded types | 🔵 | any | `Email`, `Slug`, `Money` — explicit branded types drive precise columns |
| 1.19 | Enums with associated data (tagged unions) | 🟡 | any | discriminated union → tag column + JSON/side-table |
| 1.20 | Vectors / embeddings | 🔵🟠 | request | AI columns; substrate-dependent (pgvector). See §23. |
| 1.21 | Geospatial types | 🔵🟠 | request | point/polygon; substrate (PostGIS). See §22.1. |
| 1.22 | Ranges / intervals | 🟠 | request | substrate-native (Postgres range types) |
| 1.23 | Full-text / tsvector columns | 🟡🟠 | request | inferred from a search read (§13) or L1 |
| 1.24 | Schemaless / dynamic fields | 🔵 | any | escape hatch: an explicit `Json`/open-record field |
| 1.25 | Encrypted fields (at rest, app-level) | 🔵 | any | a `Encrypted<T>` branded type → transparent field encryption |

**Pillar link:** §1 is **Pillar 1** (escape analysis → schema) and **Pillar 3** (the schema is a
build artifact). The niche types (1.20–1.23) are where **Pillar-1 inference must defer to the
substrate** — a decidability boundary worth naming now.

## 2. Migrations & schema evolution

| # | Capability | Fit | Notes |
|---|---|---|---|
| 2.1 | Auto-generated migrations (schema diff) | 🟢 | **Pillar 3 core** — diff inferred schema between builds |
| 2.2 | Forward (up) migration artifact | 🟢 | emitted, inspectable |
| 2.3 | Reversible (down) migration | 🟡 | auto for reversible ops; irreversible ops need L1 confirmation |
| 2.4 | Data backfills / data migrations | 🔵 | code-level intent, not schema-inferable → **explicit migration hook** |
| 2.5 | Zero-downtime (expand/contract) | 🟡☁️ | inferable pattern for additive changes; multi-step orchestration is hosted-grade |
| 2.6 | Destructive-change guardrails | 🟢 | column/table drop → **build refuses without explicit ack** (predictability guardrail) |
| 2.7 | Seed data | 🔵 | explicit seed source (build-tier data is the natural home) |
| 2.8 | Branch / per-environment schemas | 🟡☁️ | branch DBs; hosted convenience, local via substrate |
| 2.9 | Migration in CI/CD | 🔵 | `albedo build` emits the artifact; running it is a deploy step |
| 2.10 | Schema versioning / history | 🟢 | content-addressed schema hashes form the version chain |
| 2.11 | Online index build | 🟠 | substrate concern; FORGE emits `CONCURRENTLY` where supported |
| 2.12 | Multi-tenant schema propagation | 🔵 | see §17 |

**Open risk flagged in `backend.md` §10:** auto-migration is where "the compiler decided your
schema" is scariest. **2.6 (destructive guardrail) is the mitigation** and should be treated as
never-cut.

## 3. Queries & reads

| # | Capability | Fit | Tier | Notes |
|---|---|---|---|---|
| 3.1 | CRUD read by id | 🟢 | any | inferred |
| 3.2 | Filtering (where / comparison / boolean) | 🟢 | request | inferred from `.filter()` / access predicates |
| 3.3 | Sorting | 🟢 | request | inferred from `.sort()` |
| 3.4 | Offset pagination | 🟢 | request | inferred from slice patterns |
| 3.5 | Cursor / keyset pagination | 🟡 | request | preferred at scale; L1 to force keyset |
| 3.6 | Aggregations (count/sum/avg/min/max) | 🟢 | request/slot | inferred; aggregate over a slot collection stays reactive |
| 3.7 | Group by / having | 🟡 | request | inferred from `reduce`/grouping patterns; complex → L1 |
| 3.8 | Joins (inner/outer/lateral) | 🟢 | request | **Pillar 4** synthesizes joins from cross-collection reads |
| 3.9 | N+1 elimination | 🟢 | request | **Pillar 4 headline** — whole-graph batching/hoisting |
| 3.10 | Projection / field selection | 🟢 | request | only-read fields are selected |
| 3.11 | Distinct / dedup | 🟢 | request | inferred |
| 3.12 | Set ops (union/intersect/except) | 🟡 | request | inferred from combined reads; niche → L2 |
| 3.13 | Subqueries / CTEs | 🟡 | request | synthesized; hand-tuned → L2 raw |
| 3.14 | Recursive CTEs (trees/graphs) | 🔵 | request | explicit — recursion intent isn't in flat data flow |
| 3.15 | Window functions | 🔵 | request | explicit primitive; niche |
| 3.16 | Full-text search | 🟡🟠 | request | §13 |
| 3.17 | Fuzzy / similarity | 🟡🟠 | request | §13 |
| 3.18 | Vector / semantic kNN | 🔵🟠 | request | §23 |
| 3.19 | Geospatial proximity | 🔵🟠 | request | §22.1 |
| 3.20 | Raw SQL escape hatch | 🟠 | request | **L3 — always available**; the freedom guarantee |
| 3.21 | Query result caching | 🟢 | request | content-addressed hash = cache key (**Pillar 4**) |
| 3.22 | Streaming large result sets | 🔵 | request | cursor/stream primitive; ties to render streaming |
| 3.23 | Read replicas / read routing | 🟠☁️ | request | substrate + hosted topology |
| 3.24 | Query timeout / cancellation | 🟡 | request | default budget; L1 override |
| 3.25 | Query cost / depth limits | 🟡 | request | safety rail, esp. for any exposed API (§12) |
| 3.26 | `EXPLAIN` / plan inspection | 🔵 | dev | DX tooling (§20) |
| 3.27 | Prepared statements | 🟢 | request | synthesized queries are parameterized by construction (also = SQLi safety) |

**Placement note:** 3.x is the **request tier's** heartland. The slot tier only serves the
*param-free* subset (3.1/3.3/3.6 over small shared collections). Anything with a `where` bound to
request context is request-tier by the decidability rule (§0.2).

## 4. Mutations & writes

| # | Capability | Fit | Notes |
|---|---|---|---|
| 4.1 | Insert / update / delete | 🟢 | inferred from mutation of a persisted collection |
| 4.2 | Upsert (insert-or-update) | 🟢 | inferred from "set by key" patterns |
| 4.3 | Partial update | 🟢 | field-level diff |
| 4.4 | Bulk / batch writes | 🟡 | inferred from loop-writes → batched; L1 to force |
| 4.5 | Atomic increment / counters | 🟢 | `x.count++` on shared state → atomic `UPDATE ... = +1` (avoids read-modify-write races) |
| 4.6 | Returning values | 🟢 | write result flows back to the action |
| 4.7 | Optimistic concurrency (version col) | 🟡 | inferred version column; conflict surfaces as a typed error |
| 4.8 | Pessimistic locking (`FOR UPDATE`) | 🔵 | explicit — lock intent isn't inferable |
| 4.9 | Cascading writes | 🟡 | from relation graph + cascade policy (§1.16) |
| 4.10 | **Durable / crash-resumable writes** | 🟢 | **Pillar 6 headline** — checkpoint-in-transaction |
| 4.11 | Idempotency keys | 🟡 | inferred for durable actions; L1 for external-facing endpoints |
| 4.12 | Soft delete | 🟡 | a `deletedAt` pattern → soft-delete + auto-filtered reads |
| 4.13 | Audit trail / change tracking | 🟡☁️ | opt-in per collection; temporal history is hosted-grade |
| 4.14 | Temporal / as-of / bitemporal queries | 🔵☁️ | explicit; heavy (Datomic-style) — P5+ |
| 4.15 | Outbox / transactional messaging | 🟡 | durable action + external effect = outbox by construction (§10.7) |
| 4.16 | Write-through to cache | 🟢 | no cache to invalidate — slot rematerialize handles it (our slot-tier finding) |

**Slot-tier write path (from our pressure test):** for slot-tier collections the write is
**mutate-DB → rematerialize topic → `write_topic` fan-out**, *not* the ephemeral
`broadcast(topic, updater)`. This is a new durable-mutation primitive that reuses only the
fan-out. Naming and building it is the substance of **P0 gate 4 → P3**.

## 5. Transactions & consistency

| # | Capability | Fit | Notes |
|---|---|---|---|
| 5.1 | Request = transaction (Ur/Web model) | 🟢 | the default envelope: one request, one atomic unit, auto commit/rollback |
| 5.2 | ACID within an action | 🟢 | inherited from the request transaction |
| 5.3 | Explicit multi-step transaction | 🔵 | `forge.transaction(fn)` when the boundary ≠ the request |
| 5.4 | Savepoints / nested tx | 🟡 | nested `forge.transaction` → savepoints |
| 5.5 | Isolation-level selection | 🟡🟠 | default serializable-ish; L1 override; substrate-bounded |
| 5.6 | Retry on serialization failure | 🟢 | automatic bounded retry (durable-action machinery) |
| 5.7 | Deadlock detection / handling | 🟠 | substrate; FORGE surfaces a typed error |
| 5.8 | Distributed tx / sagas | 🔵☁️ | explicit saga primitive over durable actions (§10.10); P3+ |
| 5.9 | Two-phase commit | 🟠 | substrate-dependent; rarely FORGE's job |
| 5.10 | Consistency model selection | 🟡☁️ | strong by default (single substrate); causal/eventual under sync = P2+ |
| 5.11 | Read-your-writes | 🟢 | within request tx, free; across sessions via slot rematerialize |
| 5.12 | Invariants / constraints as guarantees | 🟢 | constraints (§1.17) enforced by substrate |

**Concurrency caveat (from pressure test):** a single shared substrate connection serializes the
process; true per-request isolation / pooling is a **P3 (deploy/scale)** concern. The
`DataContext` seam keeps that swap non-breaking.

## 6. Reactivity & realtime

| # | Capability | Fit | Notes |
|---|---|---|---|
| 6.1 | Live queries / subscriptions | 🟢 | **Pillar 5** — a slot-tier read is live for free; request-tier read becomes live via IVM |
| 6.2 | Incremental view maintenance (DBSP) | 🟢🔬 | **P2 core/research** — deltas not re-fetches |
| 6.3 | Pub/sub | 🟢 | `BroadcastRegistry` topics already are this |
| 6.4 | Presence (who's online / typing) | 🟢 | slot-tier ephemeral collection |
| 6.5 | Broadcast / fan-out | 🟢 | exists (`write_topic`) |
| 6.6 | Optimistic updates + reconcile | 🟡 | client-tier optimistic state + server delta reconcile (P2+) |
| 6.7 | Conflict resolution (CRDT / LWW / OT) | 🔵🔬 | explicit strategy; local-first is P5+ |
| 6.8 | Offline-first / local-first sync | 🔵🔬 | big surface (Zero/Electric-class); P5+, explicit opt-in |
| 6.9 | Change feeds / CDC | 🟡🟠 | slot rematerialize is app-level CDC; substrate CDC for external consumers |
| 6.10 | Delta sync over the wire | 🟢 | rides Movement-I wire codec + Phosphor (**Pillar 5**) |
| 6.11 | Backpressure / slow-consumer policy | 🟢 | already implemented in `BroadcastRegistry` (drop-full/closed) |
| 6.12 | Coarse vs keyed list reconciliation | 🟡 | coarse innerHTML swap exists; keyed reconcile is a documented engine follow-up |
| 6.13 | Reactive aggregations | 🟢🔬 | count/sum over a live collection via IVM (P2) |

**Reactivity boundary (pressure-test result):** free live reactivity holds for **static display
lists**; interactive-per-row lists drop to the A3 island path. And reactivity is coupled to the
WT/Phosphor transport — where WT is unavailable, it degrades to "refresh sees new data" (SSR
still current). Both are honest limits to hold in the demo script.

## 7. Caching

| # | Capability | Fit | Notes |
|---|---|---|---|
| 7.1 | Query cache (content-addressed) | 🟢 | **Pillar 4** — hash = cache key |
| 7.2 | Materialized views | 🟢☁️ | IVM-materialized hot queries (**Pillar 5**, auto via **CTRNI'TAS**) |
| 7.3 | Object / row cache | 🟢 | slot tier *is* an in-memory materialized cache |
| 7.4 | Cache invalidation | 🟢 | **dissolved** — there's no cache to invalidate, only a slot that rematerializes |
| 7.5 | HTTP caching (ETag / Cache-Control / SWR) | 🟡 | inferable for build/static-tier responses; L1 for request-tier |
| 7.6 | CDN / edge cache | 🟠☁️ | deploy-manifold + edge substrate |
| 7.7 | Memoization of pure derivations | 🟢 | derived-value memo already in the render path |
| 7.8 | Distributed cache (Redis-class) | 🟠 | a substrate target, not a bespoke build |
| 7.9 | Tiered / stale-while-revalidate | 🟡☁️ | policy layer; hosted optimization |

**Framing win:** §7 is where FORGE's model is *categorically* different — "there is no cache
layer, there is a data tier." Worth its own line in the pitch.

---

# PART II — THE APPLICATION PLANE

## 8. Identity, authN & authZ

| # | Capability | Fit | Notes |
|---|---|---|---|
| 8.1 | Sessions | 🟡 | engine has session + CSRF; FORGE ties identity to durable state |
| 8.2 | Password auth + hashing + MFA/TOTP | 🔵 | explicit auth primitive (security-sensitive; never "inferred") |
| 8.3 | OAuth / OIDC / social | 🔵 | explicit provider config |
| 8.4 | SAML / enterprise SSO | 🔵☁️ | explicit; enterprise (Persephone) |
| 8.5 | Passkeys / WebAuthn | 🔵 | explicit |
| 8.6 | Magic links / OTP | 🔵 | explicit + email/SMS (§11) |
| 8.7 | API keys / tokens | 🔵 | explicit; for exposed APIs (§12) |
| 8.8 | JWT / refresh tokens | 🔵 | explicit; stateless-CSRF work is adjacent |
| 8.9 | RBAC | 🟡 | role checks in code → policy; L1 for the role model |
| 8.10 | ABAC (attribute-based) | 🔵 | explicit policy |
| 8.11 | ReBAC (Zanzibar / relationship) | 🔵🔬 | explicit; graph-authz, niche/heavy |
| 8.12 | Row-level security | 🟢🟡 | **inferable** — a read that closes over `currentUser` *is* an RLS predicate (the request-tier decidability rule doubles as an ownership signal) |
| 8.13 | Field-level security | 🟡 | per-field visibility policy |
| 8.14 | Rate limiting per identity | 🟡 | §16.9 |
| 8.15 | Impersonation / admin override | 🔵 | explicit, audited |
| 8.16 | Consent / preference storage | 🟢 | ordinary persisted state |

**Genuine inference opportunity (8.12):** the same escape analysis that routes `currentUser`
reads to the request tier can *emit the RLS predicate* — "the compiler wrote your authorization
rules" is a latent second headline. Flag for P1 exploration; security-sensitive, so guardrailed.

## 9. Files, blobs & media

| # | Capability | Fit | Notes |
|---|---|---|---|
| 9.1 | File upload (multipart) | 🔵 | explicit upload endpoint/primitive |
| 9.2 | Resumable / chunked upload (tus) | 🔵 | explicit; large-file |
| 9.3 | Object storage (S3-compatible) | 🟠 | a `BlobSubstrate` sibling to `DataSubstrate` |
| 9.4 | Signed / expiring URLs | 🔵🟠 | primitive over the blob substrate |
| 9.5 | Image transforms / resize | 🔵☁️ | primitive; hosted transform pipeline |
| 9.6 | Video transcoding | ⛔☁️ | almost certainly delegated, not built |
| 9.7 | CDN delivery | 🟠☁️ | deploy manifold |
| 9.8 | Content-addressed blobs / dedup | 🟡 | hash-addressed storage (fits the content-addressed theme) |
| 9.9 | Metadata extraction | 🔵 | primitive |
| 9.10 | Virus / content scanning | ⛔☁️ | integration, not core |
| 9.11 | Streaming media ranges | 🔵🟠 | primitive over blob substrate |

**Architectural note:** files want a **`BlobSubstrate`** parallel to `DataSubstrate` — blobs are
not rows. Naming it now prevents shoehorning binary into the relational plane. P3-adjacent.

## 10. Background work, async & orchestration

| # | Capability | Fit | Notes |
|---|---|---|---|
| 10.1 | Background jobs / task queue | 🔵 | explicit `forge.job()` (intent, not data flow) |
| 10.2 | Cron / scheduled tasks | 🔵 | explicit schedule primitive |
| 10.3 | Delayed / deferred jobs | 🔵 | explicit |
| 10.4 | Retries w/ backoff | 🟢 | durable-action machinery gives this free |
| 10.5 | Dead-letter queue | 🟡 | failed durable workflows land here |
| 10.6 | **Durable workflow orchestration** | 🟢🔬 | **Pillar 6 / DBOS** — the crown jewel; crash-resumable multi-step |
| 10.7 | Outbox / exactly-once external effects | 🟡 | durable action + effect = outbox |
| 10.8 | Fan-out / fan-in | 🔵 | workflow primitive |
| 10.9 | Priority / rate-limited queues | 🟡 | policy on the job primitive |
| 10.10 | Sagas / compensating transactions | 🔵🔬 | explicit; distributed-write correctness |
| 10.11 | Event-driven triggers | 🟡 | a write triggering a workflow (DB-trigger-like, but in the durable layer) |
| 10.12 | Idempotent job processing | 🟢 | idempotency keys (§4.11) |
| 10.13 | Long-running / streaming tasks | 🔵 | primitive; progress via slot tier |

**This is the second-biggest surface after the data plane.** Durable execution (10.6) is
already **Pillar 6**; the rest of §10 is the natural expansion of that one primitive into a full
job/workflow system. Worth explicitly deciding: *is FORGE's job system just "durable actions
with a schedule," or a separate subsystem?* Recommend the former — one primitive, many
affordances.

## 11. External integrations & I/O

| # | Capability | Fit | Notes |
|---|---|---|---|
| 11.1 | Outbound HTTP (REST/GraphQL client) | 🔵 | explicit; `HttpFetch` DataSource already exists as a seam |
| 11.2 | Inbound webhooks (receiver) | 🔵 | explicit endpoint + signature verify |
| 11.3 | Outbound webhooks (delivery) | 🟡 | durable + retry (§10) = reliable delivery |
| 11.4 | Payments (Stripe etc.) | 🔵 | integration primitive; RUB'DO-adjacent |
| 11.5 | Email / SMS (transactional) | 🔵 | integration primitive |
| 11.6 | Push notifications | 🔵 | integration primitive |
| 11.7 | Message brokers (Kafka/SQS/Rabbit) | 🟠 | a substrate/adapter, for event-driven apps |
| 11.8 | gRPC / protobuf clients | 🔵 | explicit |
| 11.9 | Circuit breakers / timeouts / retries | 🟢 | wrap external calls in durable-action reliability |
| 11.10 | API composition / BFF aggregation | 🟢 | whole-graph read synthesis extends to external sources (Pillar 4 generalization) |
| 11.11 | Secrets for integrations | 🔵 | §16.5 |

## 12. API surface (exposing the backend)

| # | Capability | Fit | Notes |
|---|---|---|---|
| 12.1 | Server actions (RPC to own frontend) | 🟢 | **exists** (P6 `action()` + zod) |
| 12.2 | REST endpoints | 🟡 | inferable from a route that returns data; L1 to shape |
| 12.3 | Typed RPC (tRPC-like) | 🟢 | actions are already typed end-to-end |
| 12.4 | GraphQL server | 🔵 | explicit; whole-graph synthesis makes FORGE a natural GraphQL *backend* |
| 12.5 | Public / third-party API | 🔵 | explicit; needs keys, versioning, docs |
| 12.6 | API versioning | 🔵 | explicit policy |
| 12.7 | OpenAPI / schema generation | 🟢 | the inferred schema *is* the API schema — emit OpenAPI free |
| 12.8 | Request validation | 🟢 | zod exists |
| 12.9 | Response serialization / content negotiation | 🟡 | default JSON; L1 for other formats |
| 12.10 | CORS | 🔵 | config |
| 12.11 | Rate limiting / quotas / throttling | 🟡 | §16.9 |
| 12.12 | Pagination standards (for public APIs) | 🟡 | cursor default |
| 12.13 | Webhook subscriptions (as a provider) | 🔵 | explicit |

**Latent win (12.7):** since the schema and queries are compiler artifacts, FORGE can **emit an
OpenAPI/GraphQL schema for free** — the app *is* documented backend. Cheap, high-credibility.

## 13. Search

| # | Capability | Fit | Notes |
|---|---|---|---|
| 13.1 | Full-text search | 🟡🟠 | inferred from a text-search read; substrate FTS (SQLite FTS5 / PG tsvector) |
| 13.2 | Faceted search | 🔵 | explicit |
| 13.3 | Autocomplete / typeahead | 🟡 | prefix-search pattern |
| 13.4 | Fuzzy / trigram | 🟠 | substrate extension |
| 13.5 | Relevance ranking | 🟡🟠 | substrate scoring; L1 tuning |
| 13.6 | Vector / semantic search | 🔵🟠 | §23 |
| 13.7 | Hybrid (keyword + vector) | 🔵☁️ | explicit; advanced |
| 13.8 | External engine (Elastic/Meili/Typesense) | 🟠 | a `SearchSubstrate` adapter |

## 14. Analytics / OLAP

| # | Capability | Fit | Notes |
|---|---|---|---|
| 14.1 | Aggregation pipelines / reporting | 🟢 | request-tier aggregate queries (§3.6–3.7) |
| 14.2 | Time-series data | 🔵🟠 | explicit; substrate (timescale-class) |
| 14.3 | Event tracking / metrics | 🟢 | append-only collection |
| 14.4 | Dashboards (live) | 🟢 | slot-tier reactive aggregates (§6.13) — a strong demo |
| 14.5 | Data-warehouse export / ETL | 🔵☁️ | explicit; enterprise |
| 14.6 | Columnar / OLAP store | 🟠 | substrate target (ties to ENDGAME columnar-wire work) |
| 14.7 | Streaming analytics | 🟢🔬 | IVM *is* streaming analytics (Pillar 5 generalization) |

**Insight:** IVM (Pillar 5) is secretly an **analytics engine**. A live dashboard of reactive
aggregates is the same machinery as reactive UI. Worth a demo and a positioning line.

---

# PART III — CROSS-CUTTING & OPERATIONAL PLANE

## 15. Observability

| # | Capability | Fit | Notes |
|---|---|---|---|
| 15.1 | Structured logging | 🟢 | request timings already exist; extend to data ops |
| 15.2 | Metrics / counters | 🟢 | slot-tier counters |
| 15.3 | Distributed tracing | 🔵 | explicit; spans across actions/queries |
| 15.4 | Query performance / slow-query log | 🟢 | **feeds CTRNI'TAS** (Pillar-5/§5 closer telemetry) |
| 15.5 | Error tracking | 🟡 | ties to the dev error registry |
| 15.6 | Health / readiness checks | 🔵 | primitive |
| 15.7 | Audit logging | 🟡 | §4.13 |
| 15.8 | Alerting | ☁️ | hosted |
| 15.9 | Telemetry capture (the loop) | 🟢☁️ | **P4 / CTRNI'TAS** — access patterns, read/write ratios, mispredictions |

**§15.4 + §15.9 are not just ops — they are the fuel for `backend.md` §5's closer.** Observability
in FORGE is dual-purpose: it serves the developer *and* the self-optimizing compiler.

## 16. Security, compliance & governance

| # | Capability | Fit | Notes |
|---|---|---|---|
| 16.1 | SQL-injection safety | 🟢 | synthesized queries are parameterized by construction — *free* |
| 16.2 | Encryption in transit (TLS) | 🟠 | substrate/deploy |
| 16.3 | Encryption at rest | 🟠☁️ | substrate/hosted |
| 16.4 | Field-level / app encryption | 🔵 | `Encrypted<T>` (§1.25) |
| 16.5 | Secrets management | 🔵 | explicit; integration creds |
| 16.6 | PII detection / masking | 🟡☁️ | inferable from branded types; hosted scanning |
| 16.7 | Data retention / TTL | 🟡 | policy per collection |
| 16.8 | Right-to-be-forgotten / GDPR delete | 🟡 | cascade from the relation graph (the compiler knows every reference) |
| 16.9 | Rate limiting / DDoS | 🟡 | per-identity/endpoint policy |
| 16.10 | CSRF / XSS | 🟢 | **CSRF exists**; XSS handled by the render escaping |
| 16.11 | Data residency / sovereignty | 🟠☁️ | Persephone (enterprise) |
| 16.12 | Compliance (SOC2/HIPAA/PCI) | ☁️ | hosted posture, not code |
| 16.13 | Input sanitization | 🟢 | zod + render escaping |

**Two free wins:** 16.1 (SQLi-safe by construction) and 16.8 (GDPR-cascade from the reference
graph) are capabilities most stacks bolt on painfully; FORGE gets them from *knowing the graph
whole*. These belong in the pitch.

## 17. Multi-tenancy

| # | Capability | Fit | Notes |
|---|---|---|---|
| 17.1 | Row-level tenant isolation | 🟢🟡 | same mechanism as RLS (§8.12) — inferred from a `tenant` scope |
| 17.2 | Schema-per-tenant | 🔵🟠 | explicit + substrate |
| 17.3 | DB-per-tenant | 🟠☁️ | substrate topology |
| 17.4 | Tenant routing / context | 🟡 | request-context scope |
| 17.5 | Per-tenant config / flags | 🟢 | ordinary scoped state |
| 17.6 | Tenant provisioning | 🔵☁️ | durable workflow (§10) |
| 17.7 | Cross-tenant admin / analytics | 🔵 | explicit, privileged |
| 17.8 | Per-tenant data export | 🟡 | GDPR-export machinery (§16.8) |

## 18. Scale, performance & topology

| # | Capability | Fit | Notes |
|---|---|---|---|
| 18.1 | Connection pooling | 🟠 | **P3** — the `DataContext` seam makes this a non-breaking swap |
| 18.2 | Read replicas | 🟠☁️ | topology |
| 18.3 | Sharding / partitioning | 🟠☁️ | substrate; heavy |
| 18.4 | Horizontal scaling of compute | 🟠☁️ | deploy manifold |
| 18.5 | Auto-indexing | 🟢☁️ | **P4 / CTRNI'TAS** — index the reads that actually fire |
| 18.6 | Auto-materialization | 🟢☁️ | **P4 / CTRNI'TAS** — hot queries → IVM views |
| 18.7 | Auto tier re-placement | 🟢☁️ | **P4** — churny "static" table demoted; constant read promoted |
| 18.8 | Geo-distribution / edge data | 🟠☁️ | edge-KV substrate + manifold |
| 18.9 | Denormalization | 🟢☁️ | a CTRNI'TAS optimization, not a developer chore |
| 18.10 | Cold-start optimization | 🟢 | slot hydration + arena warmup already engineered |

**§18.5–18.7 are literally `backend.md` §5's bullet list.** This section = **Phase 4** made
concrete. The developer-facing promise: *you never index, never denormalize, never re-tier —
the app does it while you sleep.*

## 19. Deployment, backup & lifecycle

| # | Capability | Fit | Notes |
|---|---|---|---|
| 19.1 | Local dev substrate | 🟢 | embedded libSQL (`forge.db`) — **P0** |
| 19.2 | Env promotion (dev/staging/prod) | 🔵☁️ | deploy manifold |
| 19.3 | Provisioning | 🟠☁️ | hosted |
| 19.4 | Backups / restore | 🟠☁️ | substrate + hosted |
| 19.5 | Point-in-time recovery | 🟠☁️ | substrate/hosted |
| 19.6 | Replication / failover | 🟠☁️ | topology |
| 19.7 | Branch / preview databases | 🟡☁️ | ties to schema branching (§2.8) |
| 19.8 | Data anonymization for non-prod | 🔵 | from PII/branded types (§16.6) |
| 19.9 | Blue-green / canary for data + migrations | 🟡☁️ | expand/contract (§2.5) |
| 19.10 | Disaster recovery drills | ☁️ | hosted |

## 20. Developer experience & tooling

| # | Capability | Fit | Notes |
|---|---|---|---|
| 20.1 | End-to-end type safety | 🟢 | inferred schema → generated TS types |
| 20.2 | Schema introspection | 🟢 | the emitted schema artifact |
| 20.3 | **Emitted-query inspection** | 🟢 | *see the SQL the compiler wrote* — trust-building, predictability guardrail |
| 20.4 | Admin panel / data browser | 🔵☁️ | generated from the schema (like Django admin, free from inference) |
| 20.5 | Seeding / test factories | 🔵 | §7 / §21 |
| 20.6 | `EXPLAIN` / query analysis | 🔵 | dev tool |
| 20.7 | Local substrate emulation | 🟢 | embedded store *is* the emulator |
| 20.8 | Query / data logging in dev | 🟢 | extends request timings |
| 20.9 | Docs generation | 🟢 | OpenAPI (§12.7) + schema docs |
| 20.10 | REPL / data console | 🔵 | dev tool |
| 20.11 | graphify over the data graph | 🟢 | reuse the existing knowledge-graph tooling on the data-dependency graph |

**§20.3 is a predictability keystone.** The antidote to "the compiler decided your schema and I
don't trust it" is *showing* the developer exactly what it decided, inspectably. This is
never-cut for adoption.

## 21. Testing

| # | Capability | Fit | Notes |
|---|---|---|---|
| 21.1 | Transactional test isolation (rollback per test) | 🟢 | request-tx model gives this naturally |
| 21.2 | Fixtures / factories | 🔵 | from the schema |
| 21.3 | Snapshot testing of data | 🟡 | golden-style (engine already uses goldens) |
| 21.4 | Property-based invariant testing | 🔵🔬 | constraints as properties |
| 21.5 | Migration testing | 🟢 | emitted migrations are diffable artifacts |
| 21.6 | Deterministic seed reproducibility | 🟢 | content-addressed seeds |
| 21.7 | External-service mocking | 🔵 | integration test doubles (the `RecordingSubstrate` pattern already exists) |

---

# PART IV — SPECIALIZED & NICHE DATA DOMAINS

*The long tail. Freedom-of-usage lives or dies here — these are the cases that make an engineer
say "…but can it do X?"*

## 22. Domain-specific data models

- **22.1 Geospatial** 🔵🟠 — points/regions, proximity, routing; PostGIS substrate. P5+.
- **22.2 Time-series** 🔵🟠 — high-ingest, downsampling, retention windows; substrate. P5+.
- **22.3 Graph data / traversal** 🔵 — recursive relationships, shortest-path; recursive CTE (§3.14) or graph substrate.
- **22.4 Ledger / append-only / immutable** 🟡 — event log, audit, financial (RUB'DO); soft-delete-off + insert-only inferred.
- **22.5 Event sourcing** 🔵🔬 — events as source of truth, projections as views; IVM projections are a natural fit but explicit.
- **22.6 CQRS** 🟢 — read/write model split *is* the tier split (write path vs materialized read slot). Latent free.
- **22.7 Full-text / document** 🟡🟠 — §13.
- **22.8 Key-value** 🟠 — a substrate target; the simplest slot.
- **22.9 Hierarchical / nested set / closure table** 🔵 — trees; explicit pattern.
- **22.10 Bitemporal** 🔵☁️ — valid-time + transaction-time; heavy, P5+.

## 23. AI / ML data (CTRNI'TAS-adjacent, user-facing)

- **23.1 Vector storage** 🔵🟠 — embeddings columns; pgvector/substrate.
- **23.2 kNN / ANN search** 🔵🟠 — semantic retrieval.
- **23.3 RAG pipelines** 🔵☁️ — chunk/embed/retrieve; graphify already does knowledge-graph retrieval — reuse.
- **23.4 Embedding generation** 🔵☁️ — model integration.
- **23.5 Feature store** 🔵🔬 — ML features; niche.
- **23.6 Semantic caching** 🟢☁️ — content-addressed + embeddings.
- **23.7 LLM tool-calling over app data** 🟢☁️ — the schema *is* the tool schema; CTRNI'TAS/Equinox surface ("AI over your data").
- **23.8 Model-inference-as-data-dependency** 🔵 — treat an inference call like an external read (§11.1).

## 24. Application-pattern data (the "every app eventually needs" list)

- **24.1 Feature flags** 🟢 — scoped config state.
- **24.2 A/B test assignment + metrics** 🟡☁️ — assignment state + analytics + (CTRNI'TAS decides winners).
- **24.3 Leaderboards / ranked sets** 🟡 — sorted aggregate; reactive via slot tier.
- **24.4 Activity feeds / timelines** 🔵🔬 — fan-out-on-write vs fan-out-on-read; classic scale problem, explicit strategy.
- **24.5 Notifications inbox** 🟢 — per-user collection + reactivity (slot/request hybrid).
- **24.6 Comments / threading** 🟢 — self-referential relation (§1.13).
- **24.7 Tagging / taxonomy / folksonomy** 🟢 — N:M relation.
- **24.8 Versioning / history / undo-redo** 🟡 — temporal (§4.13–4.14).
- **24.9 Draft / publish / editorial workflow** 🟡 — state-machine field + durable workflow.
- **24.10 Approval workflows / state machines** 🔵 — explicit state machine over durable actions.
- **24.11 i18n / localization data** 🟢 — keyed content, build or request tier.
- **24.12 Currency / FX / unit conversion** 🔵 — RUB'DO-adjacent; explicit.
- **24.13 Scheduling / calendar / availability** 🔵 — explicit; recurrence rules are niche-hard.
- **24.14 Distributed locks / semaphores / quotas** 🔵 — coordination primitive over the substrate.
- **24.15 Sequences / ID generation (snowflake etc.)** 🟡 — inferred id, L1 for distributed schemes.
- **24.16 Idempotency store** 🟢 — durable-action machinery (§4.11).
- **24.17 Rate-limit counters** 🟢 — atomic counters (§4.5).
- **24.18 Recommendation data** 🔵☁️ — 23.x + analytics.
- **24.19 Shopping cart / session-scoped commerce** 🟢 — client+slot hybrid; RUB'DO-adjacent.
- **24.20 Soft config / settings** 🟢 — scoped state.

---

# PART V — COVERAGE MATRIX → THE FORGE ROADMAP

## 25. Where each category rides the existing roadmap

Mapping the catalog onto `backend.md` Part C's five phases. "Rides" = the phase where the
capability first becomes real (often at L0 for the common case; niche cases trail as escape
hatches or later phases).

| Roadmap phase (`backend.md`) | Catalog sections it delivers | The one-line promise |
|---|---|---|
| **P0 — the spike** `[demo]` | §1 (single collection), §4.1/4.10 (one durable write), §6.1 (slot-tier live read) | "where's the backend?" — one persisted, reactive, crash-survivable collection, zero config |
| **P1 — inference core** `[core]` | §1 (general schema), §2 (auto-migration), §3 + §4 (general reads/writes, N+1-free), §5.1 (request-tx), §8.12/§17.1 (inferred RLS/tenancy), §20.1–20.3 (types + inspection) | "you wrote no schema, no query, no migration — here they are, inspectable and correct" |
| **P2 — reactive IVM** `[core]/[research]` | §6 (subscriptions, IVM, optimistic), §7.2 (materialized views), §14 (live analytics), §22.6 (CQRS) | "a write in one tab is a *delta* in every other — no re-fetch, one dataflow graph" |
| **P3 — pluggable substrates + durable at scale** `[core]` | §5 (full tx/isolation), §9 (BlobSubstrate), §10 (jobs/workflows/sagas), §11 (integrations), §16.2–16.3 (encryption), §18.1–18.4 (pooling/replicas/topology), §19 (deploy/backup) | "it's just Postgres underneath, deploys anywhere, and writes survive a crash — provably" |
| **P4 — CTRNI'TAS over data** `[research]/[demo]` | §7.9, §15.4/15.9 (telemetry), §18.5–18.9 (auto-index/materialize/re-tier), §23.7 (AI over data), §24.2/24.18 (learned decisions) | "the database tunes itself to the app while you sleep — uncopyable, hosted" |
| **P5+ — beyond horizon** | §6.7–6.8 (CRDT/local-first), §4.14/§22.10 (bitemporal), §22.1–22.2 (geo/time-series), §23.5 (feature store), §24.4 (feed fan-out), most 🔵🔬 items | the deep specializations — sequenced only when a real app demands them |

## 26. How the catalog maps to the six pillars

| Pillar (`backend.md` §4) | Catalog center of gravity |
|---|---|
| **P1 escape analysis → schema** | §1, §8.12, §17.1 — *the schema, RLS, and tenancy all fall out of one analysis* |
| **P2 data is tier-sliced** | §0.2 (the four tiers), §3 vs §6 (request vs slot placement), §18.7 (re-tiering) |
| **P3 auto schema + migrations** | §2 entirely |
| **P4 content-addressed query synthesis** | §3.8–3.9 (N+1-free joins), §3.21/§7.1 (hash = cache key), §11.10 (BFF), §16.1 (SQLi-safe) |
| **P5 incremental reactivity (IVM)** | §6, §7.2, §14 (analytics = IVM), §22.6 (CQRS) |
| **P6 durable writes** | §4.10, §5, §10 (the whole job/workflow plane is durable-writes generalized) |
| **§5 closer (CTRNI'TAS)** | §15.4/15.9, §18.5–18.9, §23.7 — *the telemetry loop pointed at data* |

## 27. What this exercise surfaced (the reason we did it)

Brainstorming the whole territory before hardening scope turned up things the phase-plan alone
didn't make obvious. These are **candidates to fold into the roadmap**, not commitments:

1. **The freedom ladder (§0.1) resolves Open Question #2.** "Predictability vs. magic" isn't a
   binary to decide later — it's an architecture: L0 magic + guaranteed L1–L3 escape hatches.
   Adopt this as a stated principle, not an open question.
2. **Inferred authorization (§8.12) is a latent second headline.** The same `currentUser`-read
   analysis that assigns the request tier can *emit RLS predicates*. "The compiler wrote your
   auth rules" is a P1-explorable wow. Security-sensitive → guardrailed, but real.
3. **The job/workflow plane (§10) is Pillar 6 generalized, not a new subsystem.** Decide now:
   FORGE's background/cron/saga story = "durable actions with schedules and fan-out." One
   primitive, many affordances. Prevents a sprawling second system in P3.
4. **Files need a `BlobSubstrate` (§9), search may want a `SearchSubstrate` (§13.8).** The
   pluggable-substrate pattern is not one interface — it's a *family*. Name them before P3 so
   binary/search don't get shoehorned into the relational plane.
5. **Two "free" security/compliance wins (§16.1 SQLi-by-construction, §16.8 GDPR-cascade) and
   one free API win (§12.7 OpenAPI emission)** come from *knowing the graph whole*. Cheap,
   high-credibility, underexploited in the current pitch.
6. **IVM is secretly an analytics engine (§14).** A live dashboard of reactive aggregates is the
   same machinery as reactive UI — a P2 demo we hadn't named.
7. **Emitted-artifact inspection (§20.3) is the predictability keystone** and should be treated
   as never-cut alongside P0 and the honest-inference guardrail.
8. **CQRS/event-sourcing (§22.5–22.6) fall out of the tier split for free** — the write path and
   the materialized read slot *are* CQRS. Worth stating; costs nothing.

## 28. The never-cut line (extending `backend.md` §"never cut")

`backend.md` names two never-cuts: **Phase 0** and the **honest-inference guardrail**. This
catalog adds three that are cheap and load-bearing for adoption:

- **The freedom ladder** — every capability reachable at *some* layer; no dead ends.
- **Emitted-artifact inspection** (§20.3) — the trust mechanism for all the inference.
- **The four-tier decidability rule** (§0.2) — the thing that keeps L0 honest and placement
  predictable.

---

## 29. Explicitly-out-of-scope candidates (draw the border on purpose)

Freedom of usage does **not** mean *build everything*. These are capabilities FORGE should
likely **delegate, not build** — noted so the boundary is a decision, not an oversight:

- Video transcoding, virus scanning (§9.6, §9.10) — integrations.
- Being a general-purpose message broker (§11.7) — adapt to Kafka/SQS, don't reimplement.
- Being a general-purpose search engine (§13.8) — adapt to Meili/Elastic for heavy cases.
- Compliance *posture* (§16.12) — a hosted/organizational property, not code.
- A full BI/warehouse product (§14.5–14.6) — export to it; don't become Snowflake.

The test for the border: *does the capability benefit from FORGE knowing the app's data graph
whole?* If yes → build it (that's the moat). If no → adapt to the best-in-class tool. SQLi-safety
benefits (build); video transcoding doesn't (delegate).

---

*This budget is the territory. `backend.md` is the route through it. The route is deliberately
narrow — P0 first, trajectory after — but now the narrowness is a **choice made against the
whole map**, not a horizon we couldn't see past. When we harden scope, we cut from here.*
