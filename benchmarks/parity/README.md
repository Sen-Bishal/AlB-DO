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
