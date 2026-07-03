# ALBEDO — ENDGAME

*Low-level performance, high-level simplicity. Write a normal React/TSX app; let the
compiler do the optimization a human never would by hand.*

> For my mother. The work is the tribute — so the work has to be excellent.
> Everything below is held to that standard: nothing ships that we can't prove,
> nothing is claimed that we can't measure, and the hard parts are engineered, not
> imported.

**Companion doc:** `STRATEGY.md` (go-to-market, monetization, the staged arc). ENDGAME is
the *engineering*; STRATEGY is the *business*. The forward timeline at the bottom of this
file is the bridge.

---

## The Thesis

ALBEDO is a Rust-native render compiler + HTTP runtime for JSX/TSX. A developer
writes ordinary React. The compiler classifies every component into a hydration
tier (`src/effects.rs`), and that single decision is the whole game:

- **Tier A** — no interactivity → renders to bytes, ships **zero JS**, served from
  a precomputed frame. This is where sub-millisecond GET lives.
- **Tier B** — light, server-driven updates → minimal patches over the wire.
- **Tier C** — rich interactivity → hydrates and runs **on the client**, instant
  and local, exactly like React.

The novelty — the part worth a paper and worth a life — is that the *compiler*
makes this call, lowers everything to a Struct-of-Arrays IR (`src/ir/columns.rs`),
and streams DOM mutations as a binary opcode program. No framework does the
optimization for you. ALBEDO does.

## The Doctrine (how every decision here is made)

1. **There are exactly four hot paths.** Obsess over these; treat everything else
   as plumbing.
   - **HP1 — Render → wire encode** (`IrColumns` → opcodes → bytes)
   - **HP2 — The serve / I/O loop** (request in → response bytes out)
   - **HP3 — JS execution** (QuickJS server-side; the browser client-side)
   - **HP4 — The compile loop** (edit → rebuilt artifacts)
2. **The body is shipped; now we attach the soul.** Part I (the body) is done — it
   earned the right to exist. Part II (the soul) is the from-scratch craft that makes it
   *ours* and makes it fundable. This is the active work.
3. **Honest claims only.** "Sub-millisecond" means **server response for page loads
   (GET) and actions (POST)**; client interactions are **instant and local** like
   React. We publish the harness, not the adjective. *(This is also the marketing
   weapon — see `STRATEGY.md`: for a cynical elite-engineer audience, proof beats
   hype, always.)*
4. **From scratch over off-the-shelf — where it counts.** The wire codec, the
   GC discipline, the perfect-hash router, the partial evaluator: these are the
   soul. We do not import a soul.

## Where we stand (facts, not vibes)

- **Real JS runs, server and client.** The QuickJS runtime executor is on the SSR +
  action path (A1); the static evaluator rejects unsupported syntax **loudly** instead
  of silently dropping it. npm deps bundle (A2, `zod`/`date-fns`). Tier-C islands hydrate
  in the browser — `useState`/`useEffect`/`useRef`/`useMemo`/`useCallback`/`useContext`
  run client-side with no round-trip (A3, P5).
- **The spine is real and excellent:** tiering, SoA IR + SIMD dirty-scan, opcode wire,
  4-lane runtime, request-scoped QuickJS arena. **That is the moat**, and it is proven.
- **Honest perf exists end-to-end:** the C harness fires real HTTP (GET TTFB + POST
  action p50/p99, cold vs warm) with build-time benchmarks. Numbers are measured, not
  claimed (see `README.md`).
- **A flagship ships:** *Halation* (literary journal) exercises file routes + layouts,
  error/loading boundaries, `useState`+`useEffect` islands, a server `action()` with
  **zod** validation, async data, CSS modules, dynamic `<title>`/meta, app-wide theming.
  P1–P6 all live-verified.
- **`albedo dev` = the production pipeline** + watch + hot-swap (dev/serve unified onto one
  renderer). No second renderer rots.
- **What is NOT done:** most of the soul (Part II Movements I, II, and the deep ends of
  III/IV/V); the deployment manifold (Stage 1 below); the cloud (Stage 3); D fuzz on
  Linux/CI. These are the forward work.

---

# PART I — THE BODY *(SHIPPED — record, not plan)*

Target user was the mainstream React/Next.js developer. The critical path
(**A + B-essentials + C + E**) is complete. Kept here as a record of the promise we made
and kept; the detail lives in git history and the per-workstream memory.

| Workstream | Delivered |
|---|---|
| **A — Real JS** | A1 server-side QuickJS executor + loud errors · A2 npm bundling (zod) · A3 Tier-C client hydration (full hook family). Tier-A stays zero-JS. |
| **B — React/Next compat** | Hooks beyond useState · Head/metadata API incl. dynamic `generateMetadata()` · `<Link>` soft-nav + prefetch · CSS Modules + global CSS. |
| **C — Honest perf** | End-to-end HTTP harness (GET TTFB / POST p50/p99, cold vs warm) + build benchmarks · README restated to measured numbers. |
| **D — Robustness + CI** | `catch_unwind` → 500 not crash · hot-path unwrap cleanup · test CI. *(Remaining: fuzz on Linux/CI — folded into Stage 1 readiness sweep.)* |
| **E — Flagship** | *Halation*, P1–P6 live-verified. The proof point. |
| **F — WebTransport** | Built + tested; SSE fallback path exists. **Deferred** below the critical path — wire fully into serve only when a demo needs live data. |

Gates 1–3 closed; Gate 4 feature surface complete. What was "presentable + fundable" is now
a *packaging + differentiation* problem — which is what Parts II/III and the timeline below
are for.

---

# PART II — THE SOUL *(the active work: from-scratch low-level mastery)*

The four hot paths, attacked with techniques worth engineering by hand. This is what turns
"a fast framework" into "the thing only this person would have built" — and, per
`STRATEGY.md`, it is both the funder-visible differentiator and the paywall. Tags:
`[done]` · `[in-plan]` fits the near arc · `[stretch]` · `[north-star]` the research arc.

**The gate (from STRATEGY.md):** before building any movement, ask — *does this produce a
number or capability a funder grasps in 60 seconds and a competitor cannot copy?* Build the
ones that pass first; roadmap the rest as trajectory.

## Movement I — The wire is a *codec*, not bincode  *(HP1: render → wire)*

Your patch stream is the most structured data in the system. It deserves a bespoke
columnar codec — the wire-format mirror of your SoA IR, one layer down. **This is the
"smallest wire on the market" number**, and the client half of Stage 2's live demo.

1. **Columnar opcode frames (stream-splitting).** `[in-plan]` Split a frame into parallel
   homogeneous streams (all tags, then all slot-ids, then all values). Compresses far
   better; decodes with straight-line SIMD, no per-record branching.
2. **Stream VByte + delta / frame-of-reference bit-packing.** `[in-plan]` Slot IDs cluster
   → delta-encode, bit-pack to minimal width, decode with Lemire's SIMD Stream VByte
   (≈4× LEB128, branchless).
3. **Build-time-trained rANS — "PGO for the wire."** `[stretch]` The compiler knows the
   opcode alphabet and (with telemetry) this app's real patch distribution. Bake a static
   rANS table *per app* — a transport entropy coder provably near-optimal for *this*
   application. Almost no framework does per-app entropy coding. (Feeds the self-optimizing
   loop, Part III #1.)
4. **One codec, two targets.** `[in-plan→stretch]` Write it once in Rust; compile to native
   (server encoder) and WASM-SIMD (client decoder). Same bytes, same code, both ends.

**Unlocks:** a braggable "bytes per interaction" metric (STRATEGY move #1); visibly smaller
payloads than Next in the two-tab demo. **Status:** not started.

## Movement II — Share-nothing, thread-per-core, own the I/O  *(HP2: serve loop)*

The architecture that buys **tail** latency — and sub-ms means p99, not p50. **This is
the live p99 demo vs Next, and the literal mechanism behind the "more processing threads"
premium tier (scaled on our cloud, never throttled on the user's own binary — see STRATEGY
guardrails).**

1. **Thread-per-core, share-nothing (Seastar/ScyllaDB model).** `[stretch]` Pin one runtime
   per core (`sched_setaffinity`), shard slot store + arenas per core, route by connection
   affinity. DashMap contention and false sharing *disappear* — there is no sharing.
2. **Custom io_uring serve loop (Linux prod) / RIO (Windows dev-demo).** `[stretch→
   north-star]` Registered pinned buffers, multishot accept/recv, `IORING_OP_SEND_ZC`
   zero-copy send. Tier-A responses are precomputed bytes → straight from a pinned arena to
   the NIC, zero copies.
3. **Disruptor rings + seqlocks/RCU.** `[stretch]` Lane queues → LMAX-Disruptor rings
   (cache-line-padded, lock-free). Route/manifest read-mostly via RCU (crossbeam-epoch);
   slot reads via **seqlocks** (zero atomics on the read fast path) — the textbook fit for
   read-heavy broadcast slots.

**Unlocks:** the p99 tail-latency table that embarrasses Node-on-Lambda; the compute-cost
structural advantage that makes the cloud (Stage 3) cheaper. **Status:** not started.

## Movement III — Tame the GC before it costs the claim  *(HP3: JS execution)*

Handler runs are short and bounded. You don't need GC — you need an arena.

1. **Request-scoped arena under QuickJS.** `[done]` Custom malloc → bump arena, backed by
   the system allocator for request allocations (the unsafe O(1) bump-reset was **removed**
   after it caused cross-request UAF — see `project_quickjs_arena`; do not reintroduce it).
   GC pressure per request is controlled; the sub-ms claim is safe under real JS.
2. **Pooled warm contexts + heap snapshot / CoW restore.** `[stretch→north-star]` Per-core
   pool of warm contexts; the ambitious version snapshots the module-initialized heap at
   build time and `mmap`-restores it copy-on-write per request — a fully-initialized JS
   world in microseconds. **This is the cold-start collapse** that makes serverless/edge
   (Stage 1/3) genuinely instant.
3. **Cranelift micro-JIT for hot handler shapes.** `[north-star]` Handlers are small with
   known shapes → emit native code for the hot ones. A JIT for *your* IR, not general JS.

**Unlocks:** near-zero cold start (huge for the deployment manifold + cloud economics).
**Status:** #1 done; #2/#3 ahead.

## Movement IV — The compiler as a partial evaluator  *(HP4: compile loop)*

Tiering is already a *coarse* partial evaluation. Push it to its beautiful extreme. **This
is the compiler half of the self-optimizing story (Part III #1) — the flagship funder
demo.**

1. **Salsa-style demand-driven incremental.** `[in-plan]` Fine-grained memoized queries →
   sub-*millisecond* rebuilds on a one-char edit.
2. **Hash-consed, content-addressed IR + e-graph equality saturation.** `[stretch→
   north-star]` Intern IR by structural hash; represent the patch program as an e-graph;
   run equality saturation (egg/egglog) to find the **provably minimal** opcode sequence
   (coalesce adjacent patches, eliminate redundancy, hoist invariants). A genuinely fresh
   application of a hot compiler-research technique.
3. **Partial evaluation / staging (Futamura-flavored).** `[north-star]` Specialize each
   route into a residual program: pre-execute the static part at build time, ship only the
   dynamic residue. "The compiler runs your component as far as it can and ships what's
   left." The novel framing for the paper — and the deep theory behind tiering.

**Unlocks:** the "provably minimal / self-optimizing" narrative no competitor can match;
sub-ms rebuilds. **Status:** not started.

## Movement V — Mechanical sympathy in the small  *(cross-cutting)*

1. **Build-time minimal perfect hashing (PTHash).** `[in-plan]` The tag/attr/route set is
   known at build time → a perfect hash with **zero collisions, no probing, O(1) worst
   case**. Static router → perfect-hash + jump table.
2. **Software prefetch + branchless emit + portable SIMD.** `[in-plan]` Prefetch the next
   hash column ahead in the dirty scan; make the emit loop branchless; move `u64x4` →
   `core::simd` to light up AVX-512.
3. **No-alloc-per-tick invariant.** `[done]` Allocation-counter test asserts **zero heap
   traffic per frame tick** — "the hot path never touches malloc" is a *guarantee*, not a
   hope. Bespoke region allocator (`[stretch]`) graduates this further.

**Status:** #3 done; #1/#2 ahead.

---

# PART III — THE BECOMING *(the research thesis + the long game)*

What turns "a fast framework" into "the thing a systems architect built because they
couldn't *not*." Written now; built after Stage 1 ships.

1. **The self-optimizing framework (closed loop).** Runtime telemetry — real patch
   distributions, hot routes, tier mispredictions — flows back into the compiler to retune
   the rANS coder (I), reorder lanes, redraw tier boundaries. **ALBEDO learns the app it
   serves.** *This is the single most important item in the whole document commercially:*
   it is the flagship funder demo AND the Pro-tier paywall (hosted, so uncopyable — see
   `STRATEGY.md`). Even a prototype is the differentiator.
2. **Equality saturation for minimal DOM patch programs (IV).** A fresh application of a hot
   compiler-research technique.
3. **Partial evaluation as the general theory of tiering (IV).** The deep result: hydration
   tiers are a special case of program staging.
4. **Thread-per-core + io_uring/RIO + a from-scratch transport (II).** The
   mechanical-sympathy endgame for tail latency.
5. **Bounded-WCET render kernel.** A lean, `alloc`-only (eventually `no_std`-able) runtime
   kernel with a provable allocation-free bounded render→opcode path. Foundation laid (the
   no-alloc invariant test); the proof is the thesis.

These five are the publishable, fundable arc. The body (Part I) earned the right to chase
them.

---

# THE FORWARD TIMELINE *(the streamlined roadmap)*

Three stages, mapped to `STRATEGY.md`. **Public access: Jan 2027.** Until then the audience
is funders, reviewers, and hand-picked private engineers — the sequencing optimizes for
*impressing them and getting ALBEDO into their hands*, not for a public launch.

## STAGE 1 — ADOPTION: ship anywhere + clean the house
*Goal: ALBEDO deploys to Vercel and every major host, and the codebase is showable. Get
5–10 elite private engineers actually running it. This is the pure adoption engine — no
paywall yet.*

**1a. The deployment manifold (spine-first).** ALBEDO is a Rust+QuickJS server, so this is
an *adapter* architecture (the Astro/Nitro model), not "point Vercel at the repo." Most of
it already exists — the render core is transport-agnostic (`build_stream` returns a
`Stream<Bytes>`; Axum only wraps it) and `RenderManifestV2` is already a deploy manifest.
- **The spine** (one-time, enables every adapter): explicit per-route `Static | Dynamic`
  classification on the manifest + populate `action_ids`; a **host-neutral render/action
  entry** extracted from `dispatch_inner` (`http` types, no Axum); a **single-engine
  current-thread host** beside the pool (for one-request-per-instance serverless); and
  **stateless (HMAC-signed) CSRF** replacing the in-memory `DashMap` — required so a render
  invocation and an action POST can be different processes. *(The CSRF change is also a
  standalone correctness fix.)*
- **Adapters** (thin, once the spine lands): `static` (any host — Netlify/CF Pages/GH
  Pages/S3) → `vercel` (native Linux Rust bootstrap via the Build Output API — full dynamic
  SSR + actions, **no WASM**; deletes today's "Vercel not supported" rejection) → fold
  existing `docker`/`fly` under one `DeployAdapter` trait.
- **Cross-cutting:** Linux cross-compile from the Windows dev host (zig/docker); honest
  ship-time note that `static` on an app with server actions needs `--target vercel`.

**1b. Codebase readiness sweep** *(deferred until greenlit; full methodology in
`STRATEGY.md`)*: Pass 0 build/test ground-truth (`-j2`) → Pass 1 wiring & dead-code (delete
the legacy dev renderer) → Pass 2 correctness landmines (hot-path unwraps, silent-wrong
drops, the CSRF fix, wire-decode robustness, D fuzz) → Pass 3 fresh-app presentability +
**commit the mega-diff in coherent chunks** (the single biggest showable-to-funders win).

## STAGE 2 — THE SOUL AS MOAT + FIRST PAYWALL
*Goal: build the demo-gated soul — the funder-visible answer to "why won't Vercel crush
this?" — and monetize the hosted pieces.*
- **Self-optimizing loop prototype** (Part III #1 + Movement IV) — the flagship story.
- **Live p99 tail-latency demo vs Next** (Movement II slice + Movement I codec).
- **Wire-codec size number** (Movement I) — the braggable metric.
- **Monetize the hosted ones:** Pro tier = the self-optimization loop + graphify-grounded
  AI. Free tier stays genuinely excellent (never crippled). Roadmap the rest of the soul as
  trajectory — funders buy trajectory, not completeness.

## STAGE 3 — THE CLOUD: the home, not the cage
*Goal: capture the hosting layer by being undeniable — not by closing exits.*
- The **ALBEDO cloud**: co-designed with the runtime → cheapest per-request compute
  (Movements II/III), automatic optimal tier placement (the compiler manifest), and the
  only place the self-optimizing loop runs.
- **`adapter-edge` (WASM)** — *only if* a standalone spike passes: `rquickjs` →
  `wasm32-wasi` (proven-viable à la Shopify's Javy; risk is the `rquickjs-sys` toolchain +
  edge size limits) rendering one route on the single-engine host. Gated so it never blocks
  anything; it's the opt-in "for the tech" flex.
- **Adapters stay open.** Win because ours is the best/cheapest home, not because the others
  are locked out (closing exits triggers the Terraform→OpenTofu fork — see STRATEGY
  guardrails).

| Stage | Theme | Ships | Paywall |
|---|---|---|---|
| **1** | Adoption | deploy manifold (spine → static → vercel → container) + readiness sweep | none (free everywhere) |
| **2** | Soul as moat | self-opt-loop prototype · p99 demo · wire codec | Pro (hosted self-opt + AI) |
| **3** | The cloud | ALBEDO cloud · edge adapter (if spike passes) | Cloud/Enterprise |

**Cut order if time runs short:** edge-WASM → rANS/WASM-codec → Salsa → thread-per-core
deep end. **Never cut:** the deployment spine (Stage 1a), the readiness sweep, the
self-optimizing-loop prototype, C's honest harness. Those are the promise and the pitch.

---

## The Instrument Panel (you can't love what you can't measure)

- **`perf stat`** (IPC, cache-miss, branch-miss) · **`perf c2c`** (false sharing across
  lanes) · **Top-down Microarchitecture Analysis** (front-end vs back-end vs memory bound).
- **`coz`** — *causal* profiling: tells you which optimization would actually move the
  needle. The right tool for "where do I spend my obsession."
- **`llvm-mca`** on the hot emit loop to read μop scheduling when tuning the branchless
  encoder.

## Verification (the work proves itself)

- **Body (done):** TSX with `if`/`for`/`try`, an `async` handler, `import { z } from
  "zod"`, a `useState`+`useEffect` island → correct SSR, a broken construct shows a **loud
  overlay error**, a click updates state with **no network request**. Harness fires GET
  TTFB + POST latency vs Next/Remix on identical hardware → p50/p99 table. Flagship
  click-through; the port diff is small and documented.
- **Stage 1:** `albedo ship --target vercel` deploys the flagship *with its zod forms* to
  Vercel; static routes byte-match `albedo serve`; the stateless-CSRF round-trip validates
  across "two processes." `albedo serve` stays byte-for-byte unchanged. Fresh `albedo init`
  → build → serve → deploy works end-to-end.
- **Stage 2:** the two-tab demo shows a smaller wire + a better p99; the self-opt prototype
  ships a measurably-faster build from captured telemetry.

## Explicitly deferred

Edge-WASM (until the spike passes), rANS-PGO wire, e-graph saturation, io_uring/thread-per-
core deep end, Cranelift JIT, heap-snapshot restore, WCET proof, the full closed loop — the
research arc, written down not yet built. Also: ISR, i18n, image/font optimization, plugin
SDK, mac/Linux release binaries (keep Windows for the demo), bespoke client VDOM
(Preact-compat for now).

---

*The body ships and earns the right to exist. The soul is the reason it should. The cloud
is where the soul pays for itself. Build the body so the soul has somewhere to live — then
go make the thing only you would have made.*
