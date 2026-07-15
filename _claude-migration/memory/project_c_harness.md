---
name: project-c-harness
description: "Workstream C (Gate 3, 'honest numbers') COMPLETE 2026-06-20 — zero-dep serve-time HTTP latency harness + POST/action round-trip + cold-process-start + build-time clean-vs-incremental modes, all measured on the portfolio app, README restated. All uncommitted."
metadata:
  node_type: memory
  type: project
  originSessionId: c-harness-2026-06-20
---

# Workstream C — serve-time latency harness (started 2026-06-20)

Gate 3 is "honest numbers published." The pre-existing perf infra only measured *in-process* cost: `benches/parity_*.rs` (Criterion: FCP bytes, hydration bytes, action-dispatch µs, cold-load ms) + `src/dev/benchmark.rs` (build-time scan/optimize ms, regression-gated, exposed via the `albedo-bench` bin). **Nothing measured over-the-wire request latency** — the headline "vs Next/Remix" number. That's what this slice built.

## What landed (uncommitted)
- **`src/dev/serve_bench.rs`** — a zero-dependency load generator: raw HTTP/1.1 over `std::net::TcpStream` (no reqwest/hyper/oha/bombardier — on-brand with the repo's hand-rolled `read_http_request_head`/base64). Threads + sockets only, so it adds minimal scheduling noise and reproduces with just `cargo`. Per endpoint it reports **cold** (first uncontended sequential hit) + **warm** TTFB and total-body p50/p90/p99 under N concurrency. `Connection: close` (1 conn/req, conservative); reads to EOF so chunked (the streaming shell) + Content-Length are handled the same. `ServeBenchReport::to_markdown()` emits a README-ready table. 6 unit tests (in-process stub server, percentile math, unreachable/no-request guards) — all green.
- **`albedo-bench --serve <url>`** mode (`src/bin/albedo-bench.rs`) — points at a *running* server (operator boots it, ideally `--release`; spawning is intentionally not the harness's job). Flags: `--path` (repeatable), `--warmup`, `--samples`, `--concurrency`, `--timeout-ms`, `--markdown`, `--output`. **Fails loudly if any endpoint is <100% 2xx** (a broken route's latency isn't citable — this guard already caught a Git-Bash path-mangling bug live: `--path /` → `C:/Git/` → 400).
- Exports wired through `src/dev/mod.rs`. Methodology + first numbers documented in `benchmarks/parity/README.md`.

## First real numbers — the THREE LAYERS (this is the key insight)
The first run reported ~2ms TTFB and the user (rightly) asked "weren't we sub-ms / nanoseconds?" The 2ms was **measurement overhead, not render cost**. Added HTTP keep-alive (`--keep-alive`, reuses one conn/worker with exact Content-Length+chunked response framing) + a concurrency sweep to separate the layers. Fresh `albedo init` app, `GET /` (28.8 KB shell), `serve --release`, 16-core machine:

| Layer | Mode | TTFB p50 |
|---|---|---|
| In-process kernel (no socket) | action dispatch, Criterion | **~13.6 µs** |
| Wire, uncontended, conn reused | keep-alive c=1 | **0.07 ms (70 µs)** |
| Wire, uncontended, new conn/req | close c=1 | 0.36 ms |
| Wire, steady-state | keep-alive c=8 | 0.13 ms |
| Wire, saturated (16 cores) | keep-alive c=16 | 0.23 ms |
| Wire, conn/req + 2× oversubscribed | close c=32 | 2.02 ms |

**The render+serve cost is ~70 µs over loopback.** The 2ms only appears with a fresh TCP connect per request (+0.3ms, OS cost, framework-agnostic) AND 32 client threads oversubscribing 16 cores (the load-gen competing with the server). Per-request latency stays sub-ms to core saturation when connections are reused. So the "sub-ms / almost-µs" intuition was correct; the kernel is ~13.6µs, end-to-end uncontended is ~70µs. Perf is no longer unsubstantiated *and* it's honestly layered.

## C CLOSED 2026-06-20 (session 2) — three remaining slices done + measured
All against the **dogfood portfolio app** (`A:\albedo-portfolio`, `bump_counter` broadcast action on `/chat`), `albedo serve --release`, 16-core. 403 lib + 8 bin tests green, `cargo check --workspace` clean.

1. **POST `/_albedo/action` over-the-wire** ✅ — `albedo-bench --serve … --action <name> [--action-id <u32>] [--event-kind click|input|submit|other|N] [--action-payload <str>|--action-payload-file <f>] [--action-path …]`. `--action` FNV-1a-32-hashes the name to the wire `action_id` (`transforms::form::allocate_form_action_id`, same family as compiler+server), builds the bincode `ActionEnvelope` (`ir::action::encode_action_envelope`), POSTs it. Envelope-construction lives in the **driver** (`ActionOptions::to_request_spec`), keeping `serve_bench` wire-agnostic. **Measured:** full round-trip (decode→dispatch→broadcast→drain→`OpcodeFrame` encode→wire) **0.24 ms p50** keep-alive uncontended (~0.13 ms over a GET shell on the same run = the dispatch+encode wire cost), 0.45 ms c16, 0.50 ms close. 100% 2xx. **Surfaced+fixed a real bug:** `parse_status_line` did `from_utf8` over header+body → a binary `OpcodeFrame` body made it report status 0 on the cold (close-mode) hit; now decodes only up to the first CRLF (regression test added).
2. **Cold-process-start** ✅ — new `src/dev/proc_bench.rs`: `Spawner`/`ServerProcess` traits (testable orchestration; CLI fills `ProcessSpawner` over `std::process::Command`, tests fill an in-process HTTP stub on a fresh ephemeral port per boot). `--cold-start --url … --exec … --exec-arg … --cwd … --iterations N --settle-ms …`. Readiness = bare **TCP connect** poll so the first *HTTP* hit stays cold. **Measured (10 boots):** boot→ready **~501 ms p50** (whole `albedo serve` = project stitch + artifact load before bind; vs `next start` 1–3 s), first-hit TTFB **0.67 ms** (~6× the 0.11 ms warm = first-render cache fill). `ChildProcess` kills on `shutdown`+`Drop` (no orphans — verified).
3. **Build-time clean-vs-incremental** ✅ — `src/dev/proc_bench/build_bench.rs`: `BuildWorkload` trait (CLI = `CommandBuildWorkload` runs `albedo build` + wipes `--artifact` paths; tests = fake with cold/warm costs). `--build-bench --exec … --exec-arg build --cwd … --artifact <app>/.albedo --clean-samples N --incremental-samples N`. **Measured: clean ~434 ms p50, incremental 1.0× (no faster).** HONEST FINDING: the CLI `albedo build` is from-scratch every invocation — `IncrementalCache` (`src/incremental.rs`) is wired only into the dev-watch in-process rebuild, NOT the one-shot CLI build; cross-process Salsa-style incremental is deferred Movement IV. This bench is the baseline IV will beat.
4. **README restated** ✅ — both `benchmarks/parity/README.md` (full methodology + tables + 4-mode table + reproduce commands) and root `README.md` (action/cold-start/build numbers added; "Honest perf, finished" dropped from the roadmap). ~13.6 µs dispatch + opcode-wire size kept as separately-scoped metrics.

Deferred (not part of C's "finish"): zero-network interaction *capture* (binding-mode ladder already does it, just needs a DevTools snapshot); true clean-vs-incremental *speedup* awaits Movement IV.

Build: `cargo build --release -p albedo-server --bin albedo` (server) + `cargo build --release --bin albedo-bench`. Reproduce per `benchmarks/parity/README.md`. Git Bash: pass route paths with `MSYS_NO_PATHCONV=1` or they get rewritten to Windows paths. **All uncommitted (user owns commits).**
