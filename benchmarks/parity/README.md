# Phase P · Stream G — Performance parity benches

Four Criterion benches that pin ALBEDO's speed-claim numbers as a
citable artifact, replacing the bare "exponentially faster than
Next.js / Vite" marketing line with measured bytes and microseconds
anyone can reproduce.

| Bench source | What it measures |
|---|---|
| `benches/parity_fcp_bytes.rs` | Bytes the browser parses on first paint, averaged across 10 routes |
| `benches/parity_hydration_bytes.rs` | Wrapper JS + opcode-frame bytes per Tier-B island (counter, form, list) |
| `benches/parity_action_roundtrip.rs` | In-process latency of dispatching a TS-side `action()` declaration |
| `benches/parity_cold_start.rs` | Elapsed time for `CompiledProject::load_from_dir` over a multi-file project |

## Reproduce the numbers

From the workspace root:

```bash
cargo bench --bench parity_fcp_bytes
cargo bench --bench parity_hydration_bytes
cargo bench --bench parity_action_roundtrip
cargo bench --bench parity_cold_start
```

Each bench prints a structured summary to **stderr** before the
Criterion timing output. The summary lines are what feed into the
main README's perf table; the Criterion timings are reference
information about how fast the framework can produce the numbers
themselves.

## Methodology

### FCP bytes (parity_fcp_bytes)

A 10-route manifest is synthesised with route-shaped file paths
(`src/routes/index.tsx`, `/about.tsx`, etc.). For each route, the
manifest's `HtmlShell` carries the full first-byte payload:

- `doctype_and_head` — DOCTYPE, `<html><head>`, `<meta>`, `<title>`, any CSS link tags
- `body_open` — `<body>` open tag + every Tier-A node's HTML inlined verbatim (Stream B)
- `body_close` — closing tags
- `shim_script` — the bakabox runtime + Phase L `link-forms.js` script references

We sum those four fields per route and report mean / min / max.
There's no client-side JS in the shell — Tier-A ships static HTML
only — so this number IS what the browser receives on byte-zero
of first paint.

**What's deliberately NOT counted**: Tier-B island wrappers (those
load lazily via `<script type="module">` references and arrive
after the shell), Tier-C streamed nodes (those arrive on the WT
patches lane after first paint).

### Hydration bytes (parity_hydration_bytes)

Three representative Tier-B component shapes pinned via synthetic
opcode frames:

- **counter** — one `BindEvent` (click → setter) + one `SetTextRef`
  (count display) + one initial `SlotSet`. Phase K's shape for a
  `useState` counter.
- **form** — one `BindEvent` (submit) + a `Create` + a `SetText`
  for an error span. Phase L's shape for a form-action component.
- **list** — one `BindEvent` + 10 `Create`/`SetText`/`Append`
  triples for list items. Heavier representative of a feed.

For each, we report:
- Wrapper JS bytes — `build_wrapper_module_source(path)` (the
  trampoline that bakabox imports per island).
- Opcode bytes — bincode-encoded `OpcodeFrame` length.
- Total — sum of the two.

**Reference comparison**: A React 18 minimal counter shipped via
Next.js `app/` lands at 42–48 KB gzipped per route (framework
runtime + React reconciler + the counter module). Compare ALBEDO's
hundreds-of-bytes per-island cost against that baseline.

### Action round-trip (parity_action_roundtrip)

Loads the `tests/fixtures/ts_action/broadcast_demo` fixture (a
`useSharedSlot` + `action()` + `broadcast()` triple). Measures two
timings:

- `action_dispatch_round_trip` — full wire path: decode bincode
  `ActionEnvelope` → resolve handler in `CompiledProject` → invoke
  with broadcast scope installed → encode `OpcodeFrame` response.
- `action_invoke_interpreter_only` — same minus the envelope
  decode + response encode. Isolates the interpreter cost so you
  can see what the wire framing adds.

**What's deliberately NOT counted**: HTTP framing (axum routing,
header parsing, request body buffering). That's I/O and lives
outside the framework's reactive loop. What this bench pins is
the *framework cost* of dispatching an action.

**Reference comparison**: Next.js Server Actions on a warm process
land in the low-millisecond range (1–5 ms). The numbers below
include Node.js's JSON parse + React's per-call reconcile. ALBEDO's
microsecond-scale path is bincode + interpreter on the same thread.

### Cold start (parity_cold_start)

Measures `CompiledProject::load_from_dir` over the
`tests/fixtures/layouts/` fixture: parse every `.tsx`, run Phase K
metadata extraction across every function, build the CSS-module
registry. This is the heavier of the two passes
`boot_production_server` performs (the other being
`RendererRuntime::from_artifacts_dir` which is bounded by disk I/O
on the manifest JSON, not by computation).

Criterion's `sample_size(10)` and `measurement_time(10s)` keep the
bench tractable while still producing meaningful confidence
intervals on a genuinely one-shot workload.

**Reference comparison**: `next start` for a 10-route Next.js app
typically takes 1–3 seconds to ready depending on bundle size.
ALBEDO's single-binary architecture avoids a Node.js boot + JIT
warm-up; the entire serve path is one Rust process loading bincode
+ JSON manifests.

## Serve-time latency, over the wire (Workstream C)

The four Criterion benches above pin *in-process* costs. They cannot
show the number an operator feels: end-to-end request latency against a
running `albedo serve`. That's what the **serve-time harness** measures
(`src/dev/serve_bench.rs`, driven by the `albedo-bench --serve` mode).

It's a deliberately zero-dependency load generator — a raw HTTP/1.1
client over `std::net::TcpStream`, no `reqwest`/`hyper`/oha/bombardier
to install — so it reproduces with nothing but `cargo` and adds minimal
scheduling noise of its own. It points at a server you already booted;
spawning is the operator's job (so the binary stays `--release`).

The harness has four modes:

| Mode | Flag | What it measures |
|---|---|---|
| Serve latency | `--serve <url>` | GET shell **and** POST `action()` TTFB/total p50/p90/p99 over the wire |
| Cold process start | `--cold-start` | Spawn the server → boot-to-ready → **first** (truly cold) HTTP hit, over N boots |
| Build time | `--build-bench` | `albedo build` clean (artifacts wiped) vs warm re-run wall clock |
| Build-time workloads | *(default)* | The in-process scan/optimize benches (`benchmarks/workloads.json`) |

### Reproduce

```bash
# 1. Build + boot the target app in RELEASE (numbers off a debug
#    binary are meaningless — it's 10-50x slow).
cargo build --release -p albedo-server --bin albedo
cd my-app && ../target/release/albedo serve --port 3000 &

# 2. Build + run the harness (also release).
cargo build --release -p dom-render-compiler --bin albedo-bench

# GET shells + a POST action() round-trip, in one run.
./target/release/albedo-bench \
    --serve http://127.0.0.1:3000 \
    --path / --path /chat \
    --action bump_counter --event-kind submit \
    --warmup 200 --samples 3000 --concurrency 1 --keep-alive \
    --markdown --output target/benchmarks/serve.json
```

`--action <NAME>` FNV-1a-32-hashes the name to the wire `action_id`
(the same hash the compiler + server use), builds a valid bincode
`ActionEnvelope`, and POSTs it to `/_albedo/action`. `--event-kind`
(`click`/`input`/`submit`/`other`), `--action-payload`, and
`--action-id` (raw id override) tune the envelope; the same per-endpoint
percentile machinery and the <100%-2xx guard apply, so an unregistered
action (404) fails the run loudly instead of reporting a bogus latency.

**Cold process start** (true boot-to-first-hit, not just the first
uncontended hit) and **build-time clean-vs-incremental**:

```bash
# Cold start: 10 full spawn → ready → first-hit → kill cycles.
./target/release/albedo-bench --cold-start \
    --url http://127.0.0.1:3000 \
    --exec ./target/release/albedo --exec-arg serve --exec-arg --port --exec-arg 3000 \
    --cwd my-app --path /chat --iterations 10 --settle-ms 600 --markdown

# Build time: clean (wipe .albedo) vs warm re-run.
./target/release/albedo-bench --build-bench \
    --exec ./target/release/albedo --exec-arg build \
    --cwd my-app --artifact my-app/.albedo \
    --clean-samples 5 --incremental-samples 8 --markdown
```

It reports, per endpoint: **cold** (first sequential hit after the
caller booted), and **warm** TTFB + total-body p50/p90/p99 under the
configured concurrency. `--markdown` prints a README-ready table; the
JSON report carries the full distribution.

### Methodology

- **One TCP connection per request** (`Connection: close`). Folds
  connect cost into every sample — consistent across any framework you
  point a comparable tool at, and the conservative choice (keep-alive
  would only make ALBEDO look faster).
- **TTFB** = just-before-write → first response byte. **Total** = →
  EOF. We read to close, so `Content-Length` and chunked
  (`Transfer-Encoding: chunked`, which the streaming shell uses) bodies
  are handled identically.
- **Guard:** any endpoint returning <100% 2xx fails the run loudly —
  a broken route's latency is not citable.
- **Two senses of "cold", both now measured.** In `--serve` mode "cold"
  is the first *uncontended sequential* hit against an already-running
  server — single-shot latency, not a fresh boot. For true fresh-process
  cold start (spawn → bind → first-ever HTTP hit), use `--cold-start`:
  it spawns the server, polls a bare **TCP connect** for readiness (so
  the first *HTTP* request is genuinely the first render, not warmed by
  the readiness probe), times that first hit, kills the process, and
  repeats over N boots for a real distribution.

### First measured numbers (scaffold app, release, 16-core machine)

Fresh `albedo init` app (5-component starter), `albedo serve
--release`, `GET /` (a 28.8 KB SSR shell). The number depends entirely
on **what you include around the render** — connection model and
concurrency — so report the layer, not a single figure:

| Layer | Mode | TTFB p50 | TTFB p99 |
|---|---|--:|--:|
| In-process kernel (no socket) | action dispatch, Criterion | **~13.6 µs** | — |
| Wire, uncontended, conn reused | keep-alive, concurrency 1 | **0.07 ms** (70 µs) | 0.17 ms |
| Wire, uncontended, new conn/req | close, concurrency 1 | 0.36 ms | 0.54 ms |
| Wire, steady-state, conn reused | keep-alive, concurrency 8 | 0.13 ms | 0.30 ms |
| Wire, saturated (16 cores), reused | keep-alive, concurrency 16 | 0.23 ms | 0.53 ms |
| Wire, new conn/req + 2× oversubscribed | close, concurrency 32 | 2.02 ms | 2.64 ms |

Reading this table:
- The **render+serve cost** is ~**70 µs** over loopback (keep-alive,
  uncontended) — the truest "what does ALBEDO add" number.
- A fresh **TCP connect per request** adds ~0.3 ms (the 0.07 → 0.36
  jump). That's OS/loopback cost, identical for any framework.
- **Oversubscription** (32 client threads + server on 16 cores) is what
  produces the headline 2 ms — it's the load generator competing with
  the server for cores, not render time. Per-request latency stays
  sub-millisecond right up to core saturation when connections are
  reused.

Compare against `next start` on the same hardware with your own run of
an equivalent tool, **at the same connection model and concurrency**.

### Action round-trip, over the wire (measured)

The same harness now POSTs a real bincode `ActionEnvelope` to
`/_albedo/action` and measures the full server round-trip: envelope
decode → handler dispatch → slot drain → `OpcodeFrame` encode → wire.
Measured against the dogfood portfolio app's `bump_counter` action
(a `broadcast()` over a shared slot), `albedo serve --release`,
16-core machine, 3000 samples:

| Layer | Mode | TTFB p50 | TTFB p99 |
|---|---|--:|--:|
| GET `/` (shell, same run, reference) | keep-alive, c1 | 0.11 ms | 0.21 ms |
| **Action POST, uncontended, conn reused** | keep-alive, c1 | **0.24 ms** | 0.43 ms |
| Action POST, new conn/req | close, c1 | 0.50 ms | 1.21 ms |
| Action POST, saturated (16 cores) | keep-alive, c16 | 0.45 ms | 1.34 ms |

Reading this:
- A full `action()` round-trip costs ~**0.24 ms** (240 µs) over loopback,
  uncontended — roughly **0.13 ms more than a GET shell** on the same
  run. That delta *is* the dispatch + broadcast + opcode-encode cost on
  the wire; the in-process slice of it is the ~13.6 µs
  `parity_action_roundtrip` number (the rest is socket + HTTP framing).
- Fresh TCP connect per request adds ~0.26 ms (0.24 → 0.50), the same
  OS/loopback cost any framework pays.
- Under core saturation latency stays sub-millisecond at p50.

### Cold process start (measured)

`--cold-start`, 10 boots of `albedo serve --release` on the portfolio
app, first hit on `GET /chat`:

| Metric | p50 | p90 | p99 | min | max |
|---|--:|--:|--:|--:|--:|
| boot → ready (spawn → listening) | 500.9 ms | 505.9 ms | 506.5 ms | 488.2 ms | 506.5 ms |
| first-hit TTFB (truly cold render) | 0.67 ms | 0.74 ms | 0.75 ms | 0.55 ms | 0.75 ms |
| first-hit total | 0.67 ms | 0.76 ms | 0.76 ms | 0.57 ms | 0.76 ms |

- **boot → ready ~0.5 s** is the whole `albedo serve` startup: it
  stitches the project and loads the runtime artifacts before binding
  the port. (Readiness granularity is the poll interval, default 25 ms.)
  A single Rust process, no Node boot + JIT warm-up — compare `next
  start` (typically 1–3 s to ready for a comparable app).
- **first-hit TTFB 0.67 ms** is the genuinely-cold first render: ~6× the
  steady-state 0.11 ms, the cost of the first cache fill. It decays to
  the warm number within a handful of requests.

### Build time — clean vs incremental (measured)

`--build-bench`, portfolio app, `albedo build --release`, 5 clean
(`.albedo` wiped) + 8 warm re-runs:

| Build | p50 | p90 | min | max |
|---|--:|--:|--:|--:|
| clean (cold) | 434.2 ms | 512.3 ms | 432.5 ms | 512.3 ms |
| incremental (warm re-run) | 435.0 ms | 437.4 ms | 431.5 ms | 513.6 ms |

**Honest finding: incremental is 1.0× — i.e. no faster.** The CLI
`albedo build` is from-scratch on every invocation: the `IncrementalCache`
in `src/incremental.rs` is wired into the dev-server's in-process watch
rebuild (HMR), **not** the one-shot CLI build, and cross-process
demand-driven incremental (Salsa-style) is explicitly deferred to
Movement IV. So ~**434 ms** is today's clean-build baseline for a
5-component app; this bench is the yardstick the IV work will be
measured against, and the 1.0× is the number to beat — not a result to
hide.

## Refresh cadence for the README perf table

The numbers in the main README's "Performance" section were
measured on the maintainer's machine at the date the table notes.
Re-run all four benches and update the table when:

1. A change touches the manifest builder's shell composition
   (affects FCP bytes).
2. A change touches `build_wrapper_module_source` or the opcode
   encoding (affects hydration bytes).
3. A change touches `CompiledProject::invoke_action` or
   `eval_handler_body` (affects action round-trip).
4. A change touches the parse path or Phase K metadata extraction
   (affects cold start).

The Markdown table is hand-maintained; no auto-emit harness yet.

## What's intentionally out of scope

- **Live Next.js / Remix comparison runner.** Spawning a Node.js
  process for head-to-head numbers would add a heavy external
  dependency to the workspace and require Node installed for
  `cargo bench`. The reference figures in the table come from
  published benchmarks + the maintainer's reproductions on a
  separate Next.js install. Methodology > marketing — anyone can
  reproduce the ALBEDO numbers and compare against their own
  measurement of Next on the same hardware.
- **Network-RTT-simulated round-trip.** The action round-trip
  bench is in-process. Adding simulated 80 ms RTT (per the original
  plan) is straightforward via `tokio::time::sleep`, but the
  interesting framework cost is the in-process path — RTT is an
  invariant the user's network adds on top, identical for any
  framework.
- **WT-bootstrap to first-paint** (browser-side). Requires a
  headless browser; queued for a future skill or CI harness.
