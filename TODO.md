# ALBEDO — ENDGAME TODO

Actionable companion to [`ENDGAME.md`](./ENDGAME.md). Ordered by **dependency
gates**, not calendar — each gate is independently demoable. Pace it yourself.
🟢 body · 🔵 soul (in-plan craft) · 🟠 robustness.

**Never cut:** A1, A3, loud errors, C's honest harness.
**Cut order if behind:** F → Tailwind → rANS/WASM-codec → Salsa.

---

## 🚩 Gate 1 — "normal TSX runs, or errors loudly"

- [ ] 🟢 **A1** Promote `QuickJsEngine` to the runtime executor for Tier B/C SSR + actions + async (`src/runtime/quickjs_engine.rs`)
- [ ] 🟢 **A1** Bridge host objects — props, slot store (`src/runtime/slot_store.rs`), broadcast (`src/runtime/broadcast.rs`); lower results to opcodes via the existing emitter
- [ ] 🟢 **A1** Keep Tier-A on the pure-Rust evaluator (zero JS, the sub-ms server path)
- [ ] 🟢 **A1** Loud errors — *half done:* the pure-Rust evaluator now **rejects** unsupported syntax loudly instead of `_ => {}` (`eval_body_stmts`, `src/runtime/eval/core.rs`). It also gained `Stmt::Expr` (fixes a silent-drop bug: block-bodied handlers' setter/`broadcast()` calls did nothing), `if`/`else`, and nested blocks; `for`/`while`/`try`/`switch`/`throw`/etc. return a descriptive `Err` naming the construct + pointing at QuickJS (tests in `tests/ts_action_broadcast.rs`). *Remaining:* surface QuickJS runtime exceptions through the dev overlay (`crates/albedo-server/src/dev/error_overlay.rs`).
- [x] 🔵 **III** Request-scoped bump arena under QuickJS — **DONE** (`src/runtime/arena.rs`; `Runtime::new_with_alloc`). Two-region bump allocator (persistent + request); per-render `begin_request`/`run_gc`/`end_request` → O(1) reset, no per-request GC churn. Reset on a *shared* runtime is unsafe until QuickJS's retained, data-dependent global tables (shapes/atoms) are warmed into the persistent region, so the first `ARENA_WARMUP_RENDERS` (8) renders run in persistent mode, then reset is enabled. `realloc`/`dealloc` dispatch by pointer region so a persistent table growing mid-render stays persistent. *Residual hazard:* first use of a lazily-initialised runtime feature **after** warmup → harden by warming all routes at boot (renderer already primes routes) + a soak/fuzz pass [Gate 1 D].
- [x] 🔵 **V** Allocation-counter test asserting **zero heap traffic per frame tick** — **DONE** (`quickjs_engine::tests::request_arena_resets_each_render_*`): 200 request-scoped renders → byte-identical output, request region resets to 0 each tick, persistent watermark flat (zero per-tick growth), zero fallback spills. Arena counters surfaced via `QuickJsEngine::arena_stats()`.
- [ ] 🟠 **D** `catch_unwind` around request handling → 500 instead of a crashed worker
- [ ] 🟠 **D** Test CI (`.github/workflows/ci.yml`): `cargo test` + clippy + fmt on PRs

## 🚩 Gate 2 — "feels like React, faster"

- [ ] 🟢 **A2** npm dep-bundling via `swc_bundler`/esbuild; reuse vendor classification (`src/bundler/vendor.rs`); targets: `zod`, `date-fns`
- [ ] 🟢 **A3** Tier-C client hydration via Preact-compatible runtime (~3KB); rehydrate server markup; `useState`/`useEffect` run in the browser — **no round-trip**
- [ ] 🟢 **B** `useEffect` / `useRef` / `useMemo` / `useContext` — extend `src/transforms/hooks.rs` + the client runtime
- [ ] 🟢 **B** Head/metadata API — `<title>`/meta/OG → `RouteManifest` (`src/manifest/schema.rs`) → shell HTML (`src/manifest/builder.rs`)
- [ ] 🔵 **I** Columnar wire: stream-split opcode frames + Stream VByte + delta/FOR bit-packing *(now real patch traffic exists to tune against)*
- [ ] 🟠 **D** Triage + remove hot-path `.unwrap()` / `panic!` (serve, parse, decode first)

## 🚩 Gate 3 — honest numbers published

- [ ] 🟢 **C** End-to-end harness: `oha`/`bombardier` GET TTFB + POST/action latency vs Next.js/Remix, same hardware; p50/p99 cold + warm (`benchmarks/parity/`, `src/dev/benchmark.rs`)
- [ ] 🟢 **C** Demonstrate a client interaction with **zero network** (DevTools/MCP network panel)
- [ ] 🟢 **C** Build-time bench: `albedo build` clean vs incremental (`src/incremental.rs`)
- [ ] 🟢 **C** Restate `README.md` to the measured numbers; keep ~8µs dispatch + opcode-wire size as separate, clearly-scoped metrics; publish methodology
- [ ] 🔵 **V** PTHash perfect-hash router + branchless emit + software prefetch *(do after the harness so the delta is measured with `perf`/`coz`)*
- [ ] 🔵 **IV** Salsa-style demand-driven incremental → sub-ms rebuilds *(only if the build-time claim matters for the demo)*
- [ ] 🟢 **B** Link/router parity (`next/link`-style soft-nav + prefetch); Tailwind/global-CSS path *if the demo needs it*
- [ ] 🟢 **F** *(conditional)* WebTransport into serve + SSE fallback + cross-tab fix — **only if the demo has live data**

## 🚩 Gate 4 — presentable + fundable

- [ ] 🟢 **E** Flagship app ported to ALBEDO: file routes + layouts, error/loading boundaries, `useState`+`useEffect` islands, server `action()` + zod, async data, CSS modules/Tailwind, `<title>`/meta
- [ ] 🟢 **E** Document the "Next.js → ALBEDO" port diff (the friction story)
- [ ] 🟢 **E** Ship binary + demo (tester drop)
- [ ] 🟠 **D** Fuzz `read_http_request_head` (`src/bin/albedo.rs`); extend wire-decoder fuzz targets (`fuzz/`)

---

## ✅ Verification (the work proves itself)

- [ ] TSX with `if`/`for`/`try`, an `async` handler, `import { z } from "zod"`, and a `useState`+`useEffect` island → correct SSR, broken construct shows a **loud overlay error** (not null), click updates state with **no network request**
- [ ] `cargo test` green (660+)
- [ ] Ported app renders `<title>`/meta in source; `useEffect` runs client-side; `<Link>` soft-navigates
- [ ] p50/p99 table vs Next/Remix; build clean vs incremental
- [ ] Fuzzer finds no panics in `read_http_request_head`; malformed request → 500; CI green on a PR
- [ ] Allocation-counter test asserts zero heap/tick

---

## 🔭 Deferred — the research arc (Part III, post-deadline)

- [ ] **II** io_uring / thread-per-core, share-nothing, RIO on Windows
- [ ] **I** rANS entropy coder trained at build time ("PGO for the wire")
- [ ] **IV** Hash-consed IR + e-graph equality saturation (minimal patch program)
- [ ] **IV** Partial evaluation / staging (tiering as a special case)
- [ ] **III** Cranelift micro-JIT for hot handler shapes
- [ ] QuickJS heap snapshot / CoW restore
- [ ] Bounded-WCET render kernel (lean `alloc`-only / `no_std`-able crate)
- [ ] The self-optimizing loop (runtime telemetry → recompile)
