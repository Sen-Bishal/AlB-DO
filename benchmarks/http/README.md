# HTTP throughput harness

This standalone Cargo package measures completed HTTP/1.1 GET responses over
keep-alive connections, with one request in flight per connection. One command
creates a temporary app, starts its own `albedo serve`, waits for
`/_albedo/ready`, runs interleaved rounds, and stops and reaps the child. The
server is also stopped on errors, Ctrl-C, and panic unwinding. An occupied port
is refused before any Albedo command runs; the harness never kills by image name.

Build the server from the repository root, with no other `albedo` process running:

```powershell
cargo build -p albedo-server --release --bin albedo --offline
cd benchmarks/http
cargo build --release --offline
cargo run --release --offline -- scaffold --albedo ../../target/release/albedo.exe
cargo run --release --offline -- live-vs-control --rows 0 --albedo ../../target/release/albedo.exe
cargo run --release --offline -- live-vs-control --rows 100 --albedo ../../target/release/albedo.exe
tasklist /FI "IMAGENAME eq albedo.exe"
```

On other platforms, use the executable without `.exe`. This package has its own
workspace, lockfile, and build directory; it does not join the root workspace.
Release-path binaries are required by default. `--allow-debug` permits a debug or
otherwise unverified path and labels the output accordingly. The path check is
a build-profile convention, not proof of a copied executable's compilation flags.

## Scenarios and controls

`scaffold` uses the routes created by `albedo init`: `/`, `/guestbook`, and
`/_albedo/health`. `live-vs-control` also creates `/gbnodata` by copying the
guestbook page and replacing only its `useSharedSlot<Entry[]>("guestbook")`
statement with a constant array. The copied `sign_guestbook` action and its form
target are renamed to `sign_gbnodata` to avoid duplicate wire IDs at server boot.
It imports the same deterministic `id`,
`author`, and `message` rows into the live guestbook using JSONL and
`albedo import`. Zero rows skip the empty import. Both pages therefore render
the same row shape and page structure; the control omits the live slot read.
Before timing this scenario, the harness checks both decoded pages for a ledger,
the requested row count and row labels, and absence of render-error/fallback
markers. HTTP 200 alone does not prove a successful render. These probes are
outside the measured load path; their body capture is limited to 32 MiB.

Defaults are port `3911`, connection counts `1,50`, three rounds, five seconds
per measured run, and a two-second warm-up with eight connections per route.
Routes run in the same order within every round, rather than finishing all
rounds for one route before starting the next. For a shorter diagnostic run:

```powershell
cargo run --release --offline -- live-vs-control --rows 100 --albedo ../../target/release/albedo.exe --port 3912 --conns 1,50 --seconds 2 --rounds 3 --warmup-seconds 1 --routes /guestbook,/gbnodata,/_albedo/health --pair /guestbook,/gbnodata
```

Use `--routes` for comma-separated paths and `--pair` for the two paths whose
median throughput ratio should be printed. The live/control pair is the default
for `live-vs-control`. Keep the health endpoint in the measured set: its result
shows the client/server headroom available on this rig.

## SHUTTER and failures

The child receives `ALBEDO_TRUSTED_PROXIES=127.0.0.1`. Requests carry rotating
`X-Forwarded-For` values over 65,536 addresses, using one counter across warm-up
and measured runs. This spreads the synthetic traffic across limiter subjects
without changing the server or its limits. Both routes still pay SHUTTER's
bookkeeping cost. Real-world traffic includes rate limiting; these measurements
represent successful rendering throughput under this synthetic client model.

Every status is counted, including warm-up responses. Any non-200 response or
transport/protocol error invalidates the run and produces a nonzero exit status;
status counts are printed so a fast stream of 429s cannot pass as useful
throughput. No valid median summary is produced for an invalid run. The fixture
preflight also fails if the server returns an HTTP-200 render-error stub. On the
12 November checkout, the 100-row scenario exposed this server defect (T-0041);
its error-page rates must not be used as 100-row rendering measurements.

The deliberate negative check removes proxy trust even if inherited from the
parent environment. It should fail and report `429` counts:

```powershell
cargo run --release --offline -- scaffold --albedo ../../target/release/albedo.exe --no-proxy-trust --conns 8 --seconds 1 --rounds 1 --warmup-seconds 0
```

To check port refusal, bind port 3911 in another terminal, then run the harness:

```powershell
python -m http.server 3911 --bind 127.0.0.1
# In a second terminal, from benchmarks/http:
cargo run --release --offline -- scaffold --albedo ../../target/release/albedo.exe --port 3911
```

The second command must exit nonzero naming port 3911. Stop the Python server
afterwards. Automated driver tests cover both checks; the real-server test needs
an explicit binary:

```powershell
$env:ALBEDO_TEST_BINARY = (Resolve-Path ../../target/release/albedo.exe).Path
cargo test --offline --test driver -- --include-ignored
```

## Reading results

Standard output contains metadata, per-run TSV, and medians per route and
connection count, plus the selected pair's ratio. The TSV columns are:

```text
build route conns round req_s ok non200 mean_us body_bytes
```

`req_s` counts successful responses per elapsed second. `mean_us` is the mean
request latency; `body_bytes` is the mean decoded response-body size, excluding
HTTP framing. The reader consumes both content-length and chunked bodies before
issuing the next request. Metadata records the Albedo binary path and version,
checkout commit, OS, core count, scenario, and run settings. The commit describes
the checkout, not an embedded provenance claim about the supplied executable.

Record the complete output and exit status. Compare medians at the same
concurrency and row count, on the same build and rig. The expected live/control
comparison is within 3%; report any divergence as measured rather than tuning
the harness to match. Absolute req/s numbers are machine-specific; the
live/control ratio is the claim. The earlier curl-based “+3%” number included
rate-limit refusals and is not a baseline for this method.

Client and server share one host, CPU, and memory bandwidth. Results include
local HTTP, application rendering, database/topic reads, and limiter bookkeeping.
They exclude browser JavaScript, hydration, SSE/live-update delivery, public
network latency, and a realistic user population. Health throughput helps expose
generator headroom; it does not prove that the client cannot limit any workload.
