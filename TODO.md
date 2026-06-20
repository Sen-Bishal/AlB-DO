# ALBEDO — ENDGAME TODO

Actionable companion to [`ENDGAME.md`](./ENDGAME.md). Ordered by **dependency
gates**, not calendar — each gate is independently demoable. Pace it yourself.
🟢 body · 🔵 soul (in-plan craft) · 🟠 robustness.

**Never cut:** A1, A3, loud errors, C's honest harness.
**Cut order if behind:** F → Tailwind → rANS/WASM-codec → Salsa.

> **🐕 Dogfood findings (2026-06-18)** — building a real portfolio (`A:\albedo-portfolio`)
> on the mid-sprint debug binary surfaced three gaps the plan didn't name, now tracked as
> **A4 "userland boundary"** (Gate 2) + an extension to **Gate 1 D**. These are the first
> things a real user hits (`albedo init` → style it → `albedo serve`), so they gate the
> Workstream E demo. None are hard; all were unowned.

---

## 🚩 Gate 1 — "normal TSX runs, or errors loudly" — ✅ CLOSED 2026-06-20

- [x] 🟢 **A1** Promote `QuickJsEngine` to the runtime executor for Tier B/C SSR + actions + async (`src/runtime/quickjs_engine.rs`)
- [x] 🟢 **A1** Bridge host objects — props, slot store (`src/runtime/slot_store.rs`), broadcast (`src/runtime/broadcast.rs`); lower results to opcodes via the existing emitter. *In progress:* the **handler-execution bridge** landed (`src/runtime/bridge.rs` + `QuickJsEngine::eval_handler`). A TSX handler body now runs under QuickJS with seeded value bindings, `setX` setters bound to `SlotId`s, a `broadcast(topic, value)` builtin, and an `event` payload; effects come back as `Vec<HandlerEffect>` (slot-write + broadcast, in source order) that each lower to the same `Instruction::SlotSet` the action dispatcher already drains. Loops/`try`/array methods — everything the pure-Rust evaluator rejects — now run; a throw surfaces loudly. Pure (no `SlotStore`/`BroadcastRegistry` dep), under the request-arena discipline. Second slice landed: **`CompiledProject::invoke_action_quickjs` (+ `_with_broadcast`)** — the compiled action path now has a QuickJS-backed counterpart to `invoke_action`. AST→JS codegen (`handler_body_to_js`/`expr_to_js` in `compiled.rs`) turns the stored `HandlerBody` AST back into source; the JS scope is seeded from the slot store (value + capture slots as JSON; unwritten `useState` values fall back to their codegen'd initial as engine-trusted `raw_bindings`); setters + `event` payload bridged; effects persisted to the store + broadcast fan-out via `write_topic`; dirty set left clean. **Parity proven** against the pure-Rust path on the canonical counter (`tests/ts_action_quickjs.rs`: same slot increment, same single `SlotSet`, clean drain; two clicks read persisted state). *Remaining:* (1) swap the server adapter (`CompiledProjectActionAdapter` in `crates/albedo-server/src/server.rs`) to call the QuickJS path — needs a per-worker engine pool since `QuickJsEngine` is `!Send`/`&mut` while the action path is `&self`+async; (2) module-level constant seeding (currently a loud `ReferenceError`); (3) updater-function form of `broadcast(topic, fn)` (this slice is value-form); (4) SSR props→host-object exposure.
- [x] 🟢 **A1** Keep Tier-A on the pure-Rust evaluator (zero JS, the sub-ms server path)
- [x] 🟢 **A1** Loud errors — pure-Rust evaluator rejects unsupported syntax loudly; QuickJS action handler failures now surface through the dev overlay (`DevErrorRegistry::report_action` called from `run_action_request` on every handler `Err`, wired via `run_action_route` in `server.rs`).
- [x] 🔵 **III** Request-scoped bump arena under QuickJS — **DONE** (`src/runtime/arena.rs`; `Runtime::new_with_alloc`). Two-region bump allocator (persistent + request); per-render `begin_request`/`run_gc`/`end_request` → O(1) reset, no per-request GC churn. Reset on a *shared* runtime is unsafe until QuickJS's retained, data-dependent global tables (shapes/atoms) are warmed into the persistent region, so the first `ARENA_WARMUP_RENDERS` (8) renders run in persistent mode, then reset is enabled. `realloc`/`dealloc` dispatch by pointer region so a persistent table growing mid-render stays persistent. *Residual hazard:* first use of a lazily-initialised runtime feature **after** warmup → harden by warming all routes at boot (renderer already primes routes) + a soak/fuzz pass [Gate 1 D].
- [x] 🔵 **V** Allocation-counter test asserting **zero heap traffic per frame tick** — **DONE** (`quickjs_engine::tests::request_arena_resets_each_render_*`): 200 request-scoped renders → byte-identical output, request region resets to 0 each tick, persistent watermark flat (zero per-tick growth), zero fallback spills. Arena counters surfaced via `QuickJsEngine::arena_stats()`.
- [x] 🟠 **D** `catch_unwind` around request handling → 500 instead of a crashed worker (`dispatch` spawns `dispatch_inner` as a tokio task; `JoinError` → 500 + error log)
- [x] 🟠 **D** Test CI (`.github/workflows/ci.yml`): `cargo test` + clippy + fmt on PRs (added `.github/workflows/ci.yml`; matrix: ubuntu/windows/macos; nightly toolchain)
- [x] 🟠 **D** **Silent-failure sweep on the BUILD/manifest path** — **DONE 2026-06-20.** `infer_routes_dir` fix (prior session) plus a full sweep of `src/manifest/builder.rs`, `crates/albedo-server/src/routing.rs`, `src/bundler/npm.rs`, and `src/bin/albedo.rs` for the same disease. Found and fixed 4 instances, all in `builder.rs` (routing.rs/npm.rs/albedo.rs were clean — their `?`/`unwrap_or_default()` usages fail loudly at the right scope or are genuinely benign): (1) `build_assets_manifest` checked `.css` files for inclusion *after* an early `continue` on missing tier metadata — CSS files never have tier metadata, so **every CSS file was unconditionally skipped**, making `assets.css` permanently `[]`. This is the literal root cause of the A4 "CSS ships zero bytes in prod" bug — moved the CSS check above the metadata gate. (2) `wrap_in_layouts` silently `continue`d past a layout that failed to resolve/render — added `tracing::warn!` so a dropped layout shows in the build log instead of vanishing. (3) `render_static_component_html` fallback to the tag-stripped placeholder was silent — added `tracing::warn!` naming the component. (4) `build_compiled_render_project`/`build_static_render_project` discarded the real `Result` error via `.ok()?` — both now log the error via `tracing::warn!` before falling back. All 30 `manifest` tests still green; no happy-path behavior changed, only failure paths now log or (in the CSS case) stopped failing.

## 🚩 Gate 2 — "feels like React, faster"

- [x] 🟢 **A2** npm dep-bundling — **DONE for SSR + actions** (`src/bundler/npm.rs`). Engineered in-tree instead of `swc_bundler`/esbuild: a Node-style resolver (exports maps incl. nested conditions + `*` wildcards, `module`/`main` fallbacks, nearest-`package.json` `"type"` classification) + graph walker lower each reachable file to a **lazy memoized factory** (`__ALBEDO_NPM_FACTORIES`, record published before the factory runs → CJS-grade cycle tolerance, no topo-sort) + an alias per bare specifier. ESM/CJS/JSON all lower; CJS gets `module.exports` interop (default + copied named). `CompiledProject::wrap` discovers bare imports by scanning retained sources and bundles once; both QuickJS paths preload (hash-memoized); handler scopes seed npm import bindings before module consts. **Proven against real `zod@4.4.3` + `date-fns@4.4.0`** (`tests/npm_real_packages.rs`, skips w/o `target/npm-fixture`; synthetic always-on gates in `tests/npm_bundle.rs`). Bonus fixes: `__albedo_require` promoted to a global (project child-component imports now LINK under QuickJS — old A1 gap), `export class` support, `node_modules` excluded from the component walk. *Remaining for A3:* client-side vendor chunks via `src/bundler/vendor.rs` classification; `ServerRenderer` manifest-path preload.
- [ ] 🟢 **A3** Tier-C client hydration via Preact-compatible runtime (~3KB); rehydrate server markup; `useState`/`useEffect` run in the browser — **no round-trip**
- [ ] 🟢 **B** `useEffect` / `useRef` / `useMemo` / `useContext` — extend `src/transforms/hooks.rs` + the client runtime
- [ ] 🟢 **B** Head/metadata API — `<title>`/meta/OG → `RouteManifest` (`src/manifest/schema.rs`) → shell HTML (`src/manifest/builder.rs`)
- [ ] 🔵 **I** Columnar wire: stream-split opcode frames + Stream VByte + delta/FOR bit-packing *(now real patch traffic exists to tune against)*
- [ ] 🟠 **D** Triage + remove hot-path `.unwrap()` / `panic!` (serve, parse, decode first)
- [x] 🟢 **A4** **CSS → production pipeline** — **DONE 2026-06-20** (end-to-end). The Gate-1-D `build_assets_manifest` reorder was a red herring: `ProjectScanner::is_component_file` (`src/scanner.rs:203` test) **rejects `.css`**, so CSS files are *never* scanned as components — `assets.css` is structurally always `[]` and that code path can't ship global CSS. Real fix: `inject_global_css_into_shells` (`src/bin/albedo.rs`) walks the source tree at build time (reusing the dev `collect_css_bundle` logic, minus `.module.css`) and inlines a `<style data-albedo-global-css>` block into every route shell's `doctype_and_head` **before** the manifest is serialized — mirroring exactly what `albedo dev` already inlines, so dev and prod ship identical CSS. `.module.css` continues through the existing scoped-injection path untouched. **Verified** on a fresh `albedo init` app: build prints "global css inlined into N routes"; both route shells carry the CSS vars; no `public/styles.css` + `<link>` workaround needed.
- [x] 🟢 **A4** **Dev/prod render parity — layouts** — **DONE 2026-06-20.** `ResolvedDevContract` now carries `route_layouts` (per-URL outermost→leaf layout chain, populated from `discover_routes` in `src/dev/contract.rs`). `compose_dev_layouts` (`src/bin/albedo.rs`) renders each layout and substitutes the `<children />` sentinel innermost-first — the same contract as the build path's `wrap_in_layouts`. Wired into both dev render paths (`render_single_dev_route` on-demand + `render_all_routes` cached). **Verified**: `albedo dev` now serves `<div class="app-shell">` with nav + footer (sentinel substituted, not leaked), structurally matching `albedo serve`.
- [x] 🟢 **A4** **JSX whitespace fidelity** — **DONE 2026-06-20.** Root cause in `normalize_jsx_text` (`src/runtime/eval/component.rs`): *any* newline in the text node dropped **both** leading and trailing boundary whitespace, so `\n  to see <code>` collapsed to `to see<code`. Rewrote to React's actual rule — inspect only the boundary whitespace *runs*: a run is dropped only when it itself contains a newline (source indentation adjacent to a tag), else preserved as one space. Also fixed pure-whitespace nodes (`{a} {b}` keeps its space). **Verified** in both dev + prod: `to see <code>broadcast()</code> fan out` renders with all spaces intact, no `{" "}` needed. *(Covers the Tier-A pure-Rust render path; the QuickJS Tier-B/C transpile path has its own JSX→`h()` whitespace handling — not exercised by this papercut, left as-is.)*

## 🚩 Gate 3 — honest numbers published

- [~] 🟢 **C** End-to-end harness: GET TTFB + POST/action latency vs Next.js/Remix, same hardware; p50/p99 cold + warm — **GET side DONE 2026-06-20.** Built a zero-dep serve-time latency harness (`src/dev/serve_bench.rs`, raw HTTP/1.1 over `TcpStream` — no oha/bombardier/reqwest needed) driven by `albedo-bench --serve <url> --path … --warmup … --samples … --concurrency … --markdown`. Reports per-endpoint cold (first uncontended hit) + warm TTFB/total p50/p90/p99, emits JSON + a README table, and fails loudly on any non-2xx endpoint (broken-route guard). **First release numbers** (scaffold app, `albedo serve --release`, 2000 samples / concurrency 32): `GET /` TTFB p50 **1.97ms** / p99 **2.53ms**; `GET /chat` p50 **2.20ms** / p99 **2.95ms** — all 100% 2xx. Methodology + numbers in `benchmarks/parity/README.md`. **Remaining:** (1) POST `/_albedo/action` over-the-wire latency — harness supports POST bodies but the driver doesn't yet build a valid bincode `ActionEnvelope`; (2) true cold-process-start TTFB (boot-then-hit spawn mode); (3) the side-by-side Next/Remix run is the operator's (ALBEDO side is reproducible).
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
- [x] **A4 parity:** a fresh `albedo init` project, styled only via the conventional `src/styles.css`, renders **identically** in `albedo dev` and `albedo serve` (layout applied, CSS present) with **no manual `public/` + `<link>` workaround** — verified 2026-06-20 on a scaffold app: both surfaces ship the `app-shell` layout (nav+footer), the global CSS vars, and correct inline-element whitespace.
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
