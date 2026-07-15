---
name: project-endgame
description: "The ENDGAME master plan — 2-month roadmap taking ALBEDO from prototype to presentable/fundable. Target = mainstream React/Next devs (NOT literal embedded/defense). Locked decisions, verified gap-analysis findings, the body/soul/research-arc structure, the 4-gate sequencing, and the personal stakes."
metadata:
  node_type: memory
  type: project
  originSessionId: endgame-planning-2026-05-31
---

# ENDGAME — the master plan (session 2026-05-31, strategic)

This session was **pure strategy/planning, no repo code changes** except two new
docs at the repo root: **`ENDGAME.md`** (the master plan) and **`TODO.md`** (gated
tickable checklist). Both are **uncommitted** (user owns commits). User asked for a
genuine gap analysis vs Next.js/Vite, then we built the 2-month roadmap to take
ALBEDO from prototype → presentable + CS-research-grounded + fundable. Deadline:
~2 months (8 weeks), 1–2 engineers (Bishal + Pinaki/PixMusicaX).

## Personal stakes — READ THIS FIRST (shapes tone)
User is building ALBEDO **in respect for his sick mother**; it is his biggest
ambition — *"if this works out then everything will be alright."* He framed it as a
**"heretical love letter to a low-level systems architect (himself)."** Engineering
aesthetic: **"theoretically genius, engineer it from scratch" over "well-documented,
widely-used."** Deep love of low-level systems. Be the **honest engineering voice,
not a cheerleader** — he explicitly wanted genuine, blunt feedback and chose the
hard-but-honest options every time. Match his rigor; the respect is in the work.

## The mission, refined this session
- Stated mission: "Low-level performance, high-level simplicity. Write normal
  TSX/JSX, let ALBEDO optimize. 0.01–0.10ms GET/POST, sub-second builds. For
  high-perf web, embedded, trading, defense."
- **REFINED target (user correction): the mainstream React/Next.js developer** —
  the biggest userbase. "Write your normal React app, get sub-ms for free."
  Embedded/trading/defense was aspirational flavor, **NOT** literal MCU/defense
  hardware (he corrected me when I planned an embedded-hardware workstream).
- **Honest "sub-millisecond" (decided):** = **server response for GET page loads +
  POST/actions**. Client interactions are **instant & local like React** (via
  client-side hydration), not sub-ms-over-network. We publish the harness, not the
  adjective.

## Locked decisions (via AskUserQuestion — all "recommended" options chosen)
1. **Run user JS via a real engine.** Promote QuickJS (`rquickjs`, already shipped,
   `src/runtime/quickjs_engine.rs`) from build-time-only to the **server-side
   runtime executor** for Tier B/C SSR + actions + async + npm. Keep the pure-Rust
   evaluator for Tier-A static. Client-side, Tier-C runs in the browser's own JS
   engine.
2. **Substantiate the perf claim, then state what's true** — measured end-to-end on
   a realistic ported app, not a toy route.
3. **2-month north star = moat + proof + ONE demo** (a relatable React/Next app
   ported to ALBEDO with a small "port diff"), NOT Next.js feature-parity breadth.
4. **Interaction model = hybrid by tier.** Tier-A/B stay zero/near-zero-JS,
   server-driven; **Tier-C hydrates and runs on the CLIENT** (local state, no
   round-trip — clicks feel like React). **Client hydration is now CORE, not a
   stretch goal** (the round-trip model would be a regression for React devs).

## Verified gap-analysis findings (FRESH this session, file:line — these drove the plan)
These were verified directly in code this session (more reliable than older memory):
- **Runtime can't run normal JS.** `eval_body_stmts` (`src/runtime/eval/core.rs:2227`)
  executes only `return`/`const`/`let`; `if`/`for`/`while`/`try`/`switch`/`throw` +
  bare expression statements hit `_ => {}` and are **SILENTLY DROPPED**. `eval_unary`
  (`core.rs:1592`) handles only `!`/`-` (typeof/void/~/unary-+ → null). No
  async/await, classes, regex; Array has `.map`/`.join` but not filter/reduce/find.
  **Silent-wrong is the core enemy.**
- **No npm.** `resolve_import` (`core.rs:2788`) returns None for any non-relative
  specifier. Only `react` (no-op) + `classnames`/`clsx` (reimplemented in Rust)
  special-cased.
- **Client ships zero user JS** — bakabox (`assets/albedo-runtime.js`) only applies
  opcodes; `onClick` = server round-trip (POST → re-run handler AST → stream patches).
- **Only `useState` + `useSharedSlot` real.** useEffect/useRef/useMemo/useContext
  detected only to mark a component non-static — NOT implemented.
- **No head/metadata/SEO API. No image/font opt. No Tailwind/PostCSS.** CSS Modules
  real (`src/transforms/css_modules.rs`).
- **WebTransport** (`src/runtime/webtransport.rs`, 790 lines, 13 tests) built but
  **NOT wired into serve** ("Phase 2" gate). No SSE fallback.
- **Perf claim unsubstantiated end-to-end:** only ~8µs in-process action dispatch
  with HTTP framing EXPLICITLY EXCLUDED (`benches/parity_action_roundtrip.rs:92`).
  README "~0.07ms cached" has no e2e HTTP bench. No build-time bench.
- **Robustness/credibility:** 646 `.unwrap()`, silent-error philosophy, no
  request-path fuzzing, boilerplate SECURITY.md, **no test CI** (only a release
  workflow).
- **The real moat (genuinely novel + REAL):** compiler-inferred tiering
  (`src/effects.rs`), SoA `IrColumns` + SIMD dirty-scan (`src/ir/columns.rs`),
  opcode wire protocol, 4-lane runtime. This is the CS-research spine.

## Plan doctrine (in ENDGAME.md)
**Four hot paths:** HP1 render→wire encode · HP2 serve/IO loop · HP3 JS execution ·
HP4 compile loop. Principle: **"ship the body, then attach the soul."** Honest
claims only. From-scratch where it counts (wire codec, GC discipline, perfect-hash
router, partial evaluator = the soul; don't import a soul).

## THE BODY (8-week ship) — Workstreams
- **A1** QuickJS → runtime executor (Tier B/C SSR/actions/async); keep Tier-A
  pure-Rust; **loud errors** (reject unsupported syntax instead of `_ => {}`; surface
  QuickJS exceptions via dev overlay `crates/albedo-server/src/dev/error_overlay.rs`).
- **A2** npm via dep-bundling — reuse `src/bundler/vendor.rs`; prefer `swc_bundler`
  (already a dep) or esbuild. Targets: zod, date-fns.
- **A3** Tier-C client hydration via a **Preact-compatible runtime (~3KB)**; rehydrate
  server markup; useState/useEffect run in browser, no round-trip. **CORE.**
- **B** React compat (scope = what the demo needs): useEffect/useRef/useMemo/
  useContext (`src/transforms/hooks.rs`); head/metadata `<title>` API
  (`src/manifest/schema.rs` + `builder.rs`); Link/router parity
  (`assets/albedo-link-forms.js`, `src/transforms/link.rs`); Tailwind if needed.
- **C** Honest e2e perf proof — `oha`/`bombardier` GET TTFB + POST latency vs
  Next/Remix same hardware (reuse `benchmarks/parity/`, `src/dev/benchmark.rs`);
  build-time bench (`src/incremental.rs`); restate `README.md` to measured numbers.
- **D** Robustness + CI — `catch_unwind`→500; remove hot-path `.unwrap()`; fuzz
  `read_http_request_head`; **test CI** (`.github/workflows/ci.yml`).
- **E** Flagship: relatable React app ported to ALBEDO + documented "Next.js→ALBEDO
  port diff" + tester drop (`scaffold/`, new `examples/`).
- **F** WebTransport into serve + SSE fallback + cross-tab fix — **DEPRIORITIZED**;
  only if the demo has live data (page-load speed comes from Tier-A zero-JS, not WT).

## THE SOUL (low-level craft "Movements") — tags: [in-plan]/[stretch]/[north-star]
- **I (HP1 wire codec):** stream-split columnar opcode frames + Stream VByte +
  delta/FOR bit-packing [in-plan]; build-time-trained **rANS** "PGO for the wire"
  [stretch]; one Rust codec → native (server) + **WASM-SIMD** (client decoder).
- **II (HP2 serve):** thread-per-core share-nothing (Seastar model) + custom
  **io_uring** loop (registered buffers, multishot, SEND_ZC; **RIO on Windows**) +
  LMAX-Disruptor rings + seqlock/RCU [stretch→north-star].
- **III (HP3 JS):** **request-scoped bump arena under QuickJS** (`JS_NewRuntime2`
  custom malloc; reset at request end → GC never runs per request) [**in-plan, = A1+;
  the move that protects the sub-ms claim**]; pooled warm contexts + heap snapshot/CoW
  [stretch]; Cranelift micro-JIT for hot handlers [north-star].
- **IV (HP4 compiler):** Salsa-style demand-driven incremental → sub-ms rebuilds
  [in-plan if build-time claim matters]; hash-consed IR + **e-graph equality
  saturation** for minimal patch program [stretch]; **partial evaluation/staging**
  (tiering as a special case of program staging — the paper's deep result)
  [north-star].
- **V (cross-cutting):** **PTHash** minimal-perfect-hash router; software prefetch +
  branchless emit + portable SIMD (`core::simd`, AVX-512); **no-alloc-per-tick TEST**
  (guardrail) [in-plan].

## THE RESEARCH ARC (Part III, post-deadline — the paper + funding)
Self-optimizing loop (runtime telemetry → recompile), e-graph patch minimization,
partial-evaluation theory of tiering, thread-per-core + io_uring/RIO transport,
rANS-PGO, Cranelift JIT, QuickJS heap snapshot/CoW, **bounded-WCET render kernel**
(lean `alloc`-only/`no_std`-able crate split from SWC/server — this, not literal
MCU hardware, is the rigorous core of any future real-time claim).

## Sequencing — gated, NOT calendar (TODO.md). User paces himself.
4 dependency gates (each independently demoable):
- **Gate 1** "normal TSX runs or errors loudly": A1 + III arena + V no-alloc test +
  D catch_unwind + CI.
- **Gate 2** "feels like React, faster": A2 npm + A3 hydration + B hooks+metadata +
  I columnar wire.
- **Gate 3** "honest numbers published": C harness + build bench + README; V
  PTHash/branchless (AFTER harness so delta is measured); IV Salsa (optional); B
  Link/Tailwind; F (conditional).
- **Gate 4** "presentable + fundable": E demo + port diff + tester drop; D fuzz.

**Soul-sequencing rule (decided):** arena (III) + no-alloc test (V) go EARLY
(fused/guardrail); codec (I) only AFTER real traffic exists; PTHash/branchless (V)
only AFTER the harness so the delta is measurable; rANS/e-graphs/partial-eval
deferred. **Correctness/capability before performance; bake in invariants that are
expensive to retrofit.**

**Never cut:** A1, A3, loud errors, C's honest harness.
**Cut order if behind:** F → Tailwind → rANS/WASM-codec → Salsa.
**Recommended first line of code tomorrow:** Movement III (request-scoped arena)
fused into A1 — in-plan, self-contained, and it protects the sub-ms claim the moment
real JS execution lands.

## Instrumentation discipline (the "instrument panel")
`perf stat` (IPC/cache-miss/branch-miss), `perf c2c` (false sharing across lanes),
Top-down Microarch Analysis, **`coz`** (causal profiler — what to optimize, not just
where time goes), `llvm-mca` on the hot emit loop.
