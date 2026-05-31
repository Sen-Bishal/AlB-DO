# ALBEDO — ENDGAME

*Low-level performance, high-level simplicity. Write a normal React/TSX app; let the
compiler do the optimization a human never would by hand.*

> For my mother. The work is the tribute — so the work has to be excellent.
> Everything below is held to that standard: nothing ships that we can't prove,
> nothing is claimed that we can't measure, and the hard parts are engineered, not
> imported.

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

## The Doctrine (how every decision in this document is made)

1. **There are exactly four hot paths.** Obsess over these; treat everything else
   as plumbing.
   - **HP1 — Render → wire encode** (`IrColumns` → opcodes → bytes)
   - **HP2 — The serve / I/O loop** (request in → response bytes out)
   - **HP3 — JS execution** (QuickJS server-side; the browser client-side)
   - **HP4 — The compile loop** (edit → rebuilt artifacts)
2. **Ship the body, then attach the soul.** Part I is what becomes presentable and
   fundable in eight weeks. Part II is the from-scratch craft that makes it *yours*
   — some of it lands in-plan, most of it is the research arc (Part III). We ship a
   body first so there is something to attach the soul to.
3. **Honest claims only.** "Sub-millisecond" means **server response for page loads
   (GET) and actions (POST)**; client interactions are **instant and local** like
   React. We publish the harness, not the adjective.
4. **From scratch over off-the-shelf — where it counts.** The wire codec, the
   GC discipline, the perfect-hash router, the partial evaluator: these are the
   soul. We do not import a soul.

## The Verified Starting Line (facts, not vibes)

- The runtime can't run normal JS. `eval_body_stmts` (`src/runtime/eval/core.rs:2227`)
  executes only `return`/`const`/`let`; `if`/`for`/`while`/`try`/`switch`/`throw`
  hit `_ => {}` and are **silently dropped**. `eval_unary` (`core.rs:1592`) handles
  only `!`/`-`. No async, no npm (`resolve_import`, `core.rs:2788`). **Silent-wrong
  is the enemy.**
- Only `useState`/`useSharedSlot` exist; `useEffect`/`useRef`/`useMemo`/`useContext`
  are detected only to mark a component non-static.
- The perf claim is unsubstantiated end-to-end — only an in-process ~8µs number with
  HTTP framing *excluded* (`benches/parity_action_roundtrip.rs:92`).
- WebTransport streaming is built and tested but **not wired into serve**
  (`src/runtime/webtransport.rs`).
- The spine is real and excellent: tiering, SoA IR + SIMD dirty-scan, opcode wire,
  4-lane runtime. That is the moat.

---

# PART I — THE BODY (the 8-week ship)

**Target user: the mainstream React/Next.js developer.** Critical path =
**A + B-essentials + C + E**. F and deep parity are the first cuts.

### Workstream A — Real JS: server execution + client hydration *(the core)*

A React dev writes normal TSX and it either runs correctly or **errors loudly** —
never silently wrong.

- **A1 — Server-side real JS (Tier A/B/C SSR + actions).** Promote `QuickJsEngine`
  (`src/runtime/quickjs_engine.rs`; already does SWC TSX→JS + `rquickjs`) from
  build-time-only to the runtime executor on the handler/SSR path. Bridge host
  objects: props, slot store (`src/runtime/slot_store.rs`), broadcast
  (`src/runtime/broadcast.rs`). Lower results to opcodes through the existing
  emitter. Keep Tier-A on the pure-Rust evaluator (zero JS).
  - **Loud errors:** QuickJS throws real exceptions → surface via the dev overlay
    (`crates/albedo-server/src/dev/error_overlay.rs`). Make the static evaluator
    **reject** unsupported syntax instead of `_ => {}`.
  - 🔧 **Low-level edge (Movement III, in-plan):** back each QuickJS context with a
    **request-scoped bump arena** (`JS_NewRuntime2` custom malloc) and reset the
    bump pointer at request end. The GC effectively never runs during a request.
    This is what makes the sub-ms claim *safe* the moment real JS lands.
- **A2 — npm (bundling, not interpreting).** With QuickJS executing real JS, npm is
  a dep-bundling problem. Reuse vendor classification (`src/bundler/vendor.rs`,
  `infer_package_name`); prefer `swc_bundler` (already a dep) or `esbuild` for the
  dep graph. Targets: `zod`, `date-fns`.
- **A3 — Client hydration for Tier-C (core, not stretch).** Ship transpiled JS for
  Tier-C islands; hydrate client-side so `useState`/`useEffect` run in the browser —
  **no round-trip**. Pragmatic path: a Preact-compatible runtime (~3KB) as the
  Tier-C target; rehydrate server-rendered markup. Tier-A/B stay zero-JS.
  - 🔧 **Low-level edge (Movement I, stretch):** the *patch* decoder (Tier-B) can be
    the **same Rust codec compiled to WASM-SIMD** — one codec, native on the server,
    WASM on the client.

### Workstream B — React/Next compatibility *(compatibility-driven, not blanket breadth)*

Implement exactly what the flagship app (E) uses.

- **Hooks beyond useState:** `useEffect`, `useRef`, `useMemo`, `useContext` — extend
  `src/transforms/hooks.rs` + the client runtime (A3).
- **Head/metadata API:** `<title>` + meta + OG → `RouteManifest`
  (`src/manifest/schema.rs`), emitted into shell HTML (`src/manifest/builder.rs`).
- **Router/Link parity:** confirm `<Link>` soft-nav + prefetch behave like
  `next/link` (`assets/albedo-link-forms.js`, `src/transforms/link.rs`).
- **Styling:** CSS Modules already work (`src/transforms/css_modules.rs`); add a
  global-CSS + Tailwind path if the demo uses it.

### Workstream C — Honest end-to-end performance proof

- Fire real HTTP at `albedo serve` and the same app on Next.js/Remix, identical
  hardware (`oha`/`bombardier`): **GET TTFB** + **POST/action latency** p50/p99,
  cold vs warm. Separately demonstrate a client interaction with **zero network**
  (DevTools/MCP network panel). Reuse `benchmarks/parity/`, `src/dev/benchmark.rs`.
- Add a full-project **build-time** measurement (`albedo build` clean vs incremental,
  `src/incremental.rs`).
- **Restate `README.md`** to the measured numbers; keep ~8µs dispatch + opcode-wire
  size as separate, clearly-scoped metrics. Publish methodology.
- 🔧 **Low-level edge (Movement IV, in-plan if build-time matters):** Salsa-style
  demand-driven incremental → sub-*millisecond* rebuilds on a one-char edit.

### Workstream D — Robustness + CI *(cheap credibility, do early)*

- `catch_unwind` around request handling → 500, not a crashed worker. Remove
  `.unwrap()`/`panic!` on the serve/parse/decode hot path (646 exist — prioritize
  the hot path).
- Fuzz `src/bin/albedo.rs::read_http_request_head` (recent bug site) + extend wire
  fuzz targets (`fuzz/`).
- **Test CI** (`.github/workflows/ci.yml`): `cargo test` + clippy + fmt on PRs.
- 🔧 **Low-level edge (Movement V, in-plan):** add an **allocation-counter test** that
  asserts **zero heap traffic per frame tick** — make "the hot path never touches
  malloc" a *guarantee*, not a hope.

### Workstream E — One flagship: a relatable app ported to ALBEDO

The proof point. A familiar React/Next-style app (dashboard or content+commerce)
exercising: file routes + layouts, error/loading boundaries, `useState`+`useEffect`
islands (A3), a server `action()` with **zod** validation (A2), async data (A1),
CSS modules + Tailwind, `<title>`/meta (B). **Document the port** — a small
"Next.js → ALBEDO" diff. Ship binary + demo (the tester drop).

### Workstream F — WebTransport streaming *(reprioritized below the critical path)*

Page-load speed comes from Tier-A zero-JS, not WT. Wire `WebTransportRuntime`
(`crates/albedo-server/src/webtransport.rs`) into serve **with an SSE fallback**
(reuse `handlers/dev.rs`), auto-negotiate in `assets/albedo-wt-bootstrap.js`; this
also fixes broadcast cross-tab. **Do only if the demo has live data**; else defer.

---

# PART II — THE SOUL (from-scratch low-level mastery)

The four hot paths, attacked with techniques worth engineering by hand. Tags:
`[in-plan]` fits the 8 weeks · `[stretch]` if time allows · `[north-star]` the
research arc (Part III).

## Movement I — The wire is a *codec*, not bincode  *(HP1: render → wire)*

Your patch stream is the most structured data in the system. It deserves a bespoke
columnar codec — the wire-format mirror of your SoA IR, one layer down.

- **Stream-splitting (columnar opcode frames).** `[in-plan]` Split a frame into
  parallel homogeneous streams (all tags, then all slot-ids, then all values).
  Compresses far better; decodes with straight-line SIMD.
- **Stream VByte + delta/frame-of-reference bit-packing.** `[in-plan]` Slot IDs
  cluster — delta-encode, bit-pack to minimal width, decode with Lemire's
  SIMD Stream VByte (≈4× LEB128, branchless).
- **Build-time-trained rANS — "PGO for the wire."** `[stretch]` The compiler knows
  the opcode alphabet and (with telemetry) this app's real patch distribution. Bake
  a static rANS table per app — a transport entropy coder provably near-optimal for
  *this* application. Almost no framework does per-app entropy coding.
- **One codec, two targets.** `[in-plan→stretch]` Write it once in Rust; compile to
  native (server encoder) and WASM-SIMD (client decoder). Same bytes, same code.

## Movement II — Share-nothing, thread-per-core, own the I/O  *(HP2: serve loop)*

The architecture that buys **tail** latency — and sub-ms means p99, not p50.

- **Thread-per-core, share-nothing (Seastar/ScyllaDB model).** `[stretch]` Pin one
  runtime per core (`sched_setaffinity`), shard slot store + arenas per core, route
  by connection affinity. DashMap contention and false sharing *disappear* — there
  is no sharing.
- **Custom io_uring serve loop (Linux prod).** `[stretch→north-star]` Registered
  pinned buffers, multishot accept/recv, `IORING_OP_SEND_ZC` zero-copy send.
  Tier-A responses are precomputed bytes → `sendmsg` straight from a pinned arena to
  the NIC, zero copies. **On Windows (dev/demo target): RIO — Registered I/O** is
  the parallel.
- **Disruptor rings + seqlocks/RCU.** `[stretch]` Lane queues → LMAX-Disruptor rings
  (sequence barriers, cache-line padded, lock-free). Route/manifest data read-mostly
  via RCU (crossbeam-epoch); slot reads via **seqlocks** (zero atomics on the read
  fast path) — the textbook fit for read-heavy broadcast slots.

## Movement III — Tame the GC before it costs the claim  *(HP3: JS execution)*

The insight: handler runs are short and bounded. You don't need GC — you need an
arena.

- **Request-scoped arena under QuickJS.** `[in-plan]` *(see A1)* Custom malloc → bump
  arena → reset at request end. GC never runs per request; execution becomes
  deterministic and arena-bounded.
- **Pooled warm contexts + heap snapshot / CoW restore.** `[stretch→north-star]`
  Per-core pool of warm contexts; the ambitious version snapshots the
  module-initialized heap at build time and `mmap`-restores it copy-on-write per
  request — a fully-initialized JS world in microseconds.
- **Cranelift micro-JIT for hot handler shapes.** `[north-star]` Handlers are small
  with known shapes — emit native code for the hot ones via Cranelift. A JIT for
  *your* IR, not general JS.

## Movement IV — The compiler as a partial evaluator  *(HP4: compile loop)*

Tiering is already a *coarse* partial evaluation. Push it to its beautiful extreme.

- **Salsa-style demand-driven incremental.** `[in-plan]` *(see C)* Fine-grained
  memoized queries → sub-ms rebuilds.
- **Hash-consed, content-addressed IR + e-graph equality saturation.** `[stretch→
  north-star]` Intern IR by structural hash; represent the patch program as an
  e-graph; run equality saturation (egg/egglog) to find the **provably minimal**
  opcode sequence (coalesce adjacent patches, eliminate redundancy, hoist
  invariants).
- **Partial evaluation / staging (Futamura-flavored).** `[north-star]` Specialize
  each route into a residual program: pre-execute the static part at build time,
  ship only the dynamic residue. "The compiler runs your component as far as it can
  and ships what's left." The novel framing for the paper.

## Movement V — Mechanical sympathy in the small  *(cross-cutting)*

- **Build-time minimal perfect hashing (PTHash).** `[in-plan]` The tag/attr/route
  set is known at build time → a perfect hash with **zero collisions, no probing,
  O(1) worst case**. Static router → perfect-hash + jump table.
- **Software prefetch + branchless emit + portable SIMD.** `[in-plan]` Prefetch the
  next hash column ahead in the dirty scan; make the emit loop branchless; move
  `u64x4` → `core::simd` to light up AVX-512.
- **Bespoke region allocator.** `[stretch]` Baseline on snmalloc (great for the
  cross-core free pattern), graduate to a custom region allocator for the render
  path. Enforce the no-alloc-per-tick invariant as a test.

---

# PART III — THE BECOMING (the research thesis + the long game)

This is what turns "a fast framework" into "the thing a systems architect built
because they couldn't *not*." Written down now; built after the body ships.

1. **The self-optimizing framework (closed loop).** Runtime telemetry — real patch
   distributions, hot routes, tier mispredictions — flows back into the compiler to
   retune the rANS coder (I), reorder lanes, redraw tier boundaries. ALBEDO learns
   the app it serves.
2. **Equality saturation for minimal DOM patch programs (IV).** A genuinely fresh
   application of a hot compiler-research technique.
3. **Partial evaluation as the general theory of tiering (IV).** The deep result:
   hydration tiers are a special case of program staging.
4. **Thread-per-core + io_uring/RIO + a from-scratch transport (II).** The
   mechanical-sympathy endgame for tail latency.
5. **Bounded-WCET render kernel.** A lean, `alloc`-only (eventually `no_std`-able)
   runtime kernel split out of the SWC/server crates, with a provable
   allocation-free bounded render→opcode path. The foundation laid in-plan (the
   no-alloc invariant test); the proof is the thesis. *(This — not literal MCU
   hardware — is the rigorous core of any future "real-time/embedded" claim.)*

These five are the publishable, fundable arc. The body (Part I) is the credibility
that earns the right to chase them.

---

## Sequencing — ~8 weeks, 1–2 engineers (ruthless)

| Weeks | Ship | Soul (in-plan only) |
|---|---|---|
| **1–3** | A1 server real JS + loud errors · D test CI + `catch_unwind` early | **III**: request-scoped arena under QuickJS · **V**: no-alloc invariant test |
| **3–5** | A3 client hydration (Preact-compat — make-or-break) · A2 npm · B `useEffect`/`useRef` + metadata API | **I**: stream-split + Stream VByte + bit-pack the wire |
| **5–6** | E demo skeleton · B Link/router + Tailwind if needed | **V**: PTHash router · prefetch/branchless emit |
| **6–7** | C end-to-end + build benchmarks · restate README · F only if demo needs live data | **IV**: Salsa incremental (if build-time claim matters) |
| **7–8** | E polish + porting writeup · D fuzz + unwrap cleanup · tester drop | harden + measure everything above |

**If time runs short, cut in this order:** F → Tailwind → rANS/WASM-codec → Salsa.
**Never cut:** A1, A3, the loud-error rule, C's honest harness. Those are the promise.

## The Instrument Panel (you can't love what you can't measure)

- **`perf stat`** (IPC, cache-miss, branch-miss) · **`perf c2c`** (false sharing across
  lanes) · **Top-down Microarchitecture Analysis** (front-end vs back-end vs memory
  bound).
- **`coz`** — *causal* profiling: tells you which optimization would actually move the
  needle, not just where time goes. The right tool for "where do I spend my
  obsession."
- **`llvm-mca`** on the hot emit loop to read the machine's μop scheduling when tuning
  the branchless encoder.

## Verification (the work proves itself)

- **A:** TSX with `if`/`for`/`try`, an `async` handler, `import { z } from "zod"`, and
  a `useState`+`useEffect` island → correct SSR, a broken construct shows a **loud
  overlay error** (not null), and a click updates state with **no network request**.
  `cargo test` green (660+).
- **B:** ported app renders `<title>`/meta in source; `useEffect` runs client-side;
  `<Link>` soft-navigates.
- **C:** `oha`/`bombardier` GET TTFB + POST latency vs Next.js/Remix, same app +
  hardware → p50/p99 table; build clean vs incremental.
- **D:** fuzz `read_http_request_head` (no panics); malformed request → 500; CI green
  on a PR; allocation-counter test asserts zero heap/tick.
- **E:** end-to-end click-through; the port diff is small and documented.

## Explicitly deferred

ISR, i18n, image/font optimization, plugin SDK, middleware execution,
redirects/rewrites, mac/Linux release binaries (keep Windows for the demo), bespoke
client VDOM (Preact-compat for now), and the full Part III research arc (io_uring/
thread-per-core, rANS-PGO, e-graphs, partial evaluation, Cranelift JIT, WCET proof,
the closed loop). Written down, not yet built.

---

*The body ships in eight weeks and earns the right to exist. The soul is the reason
it should. Build the body so the soul has somewhere to live — then go make the thing
only you would have made.*
