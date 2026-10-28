# ERNAX — the commit ledger

Every commit tagged `ERNAX-nnn` is written down here, in full, at the time it is made.

## Why this file exists

A commit subject line has room for a claim and no room for its evidence. This repository has
been bitten repeatedly by the gap between the two — a subject saying a thing shipped, and the
tree saying it shipped *and does not run*. So an ERNAX entry carries what a subject cannot:
what was built, what was **proven** and how, what was left out **on purpose**, and what is
still open. The last two matter most. A ledger that only records wins is a ledger that reads
as finished work, and the next person to open it has no way to tell a verified claim from an
optimistic one.

## The rules

1. **Newest first.** New entries go directly under this section, above the previous one.
2. **One entry per `ERNAX-nnn` commit**, with its short SHA and date.
3. Every entry carries all five headings: *What shipped* · *The design calls* · *Proven* ·
   *Deliberately not built* · *Open*.
4. **"Proven" means a test or a measurement, and says which.** If a claim rests on a contract
   rather than an observation — a platform guarantee, a crate's documented behaviour — it goes
   under *Open* with the reason it could not be observed here.
5. Numbers carry their corpus. `2 296 passing` alone is not a fact; *"2 296 across 98 binaries,
   both crates, warm run"* is.

---

## ERNAX-001 — `73def40` · 2026-09-17

**57 files, +13 494 / −220.** The whole uncommitted backlog: the infrastructure-surface items
15.5, 15.6 and 15.7, four rounds of engine-pool work, the outbound budget, and the build-profile
fix.

### What shipped

**JOBS · item 15.5** — `src/jobs/{declare,schedule,queue,builtin,collect}.rs`,
`crates/albedo-server/src/jobs.rs`, `eval_job` in the QuickJS engine.
One `src/jobs.ts`, one export per job: `export const nightly = job({ schedule: "@daily" }, fn)`.
Cron (five-field), `@aliases`, `every N` intervals; a durable queue with leases, exponential
backoff with jitter, and dead-lettering; a runner that sleeps until the next due instant rather
than polling. `append`/`update`/`remove`/`enqueue` are importable from `"albedo/jobs"` inside a
job body, and `enqueue()` is available to action bodies. `albedo:expire_sessions` runs hourly in
Rust.

**MIDDLEWARE · item 15.6** — `src/middleware/{declare,matcher,outcome}.rs` +
`crates/albedo-server/src/middleware.rs`. One `src/middleware.ts`, Next-shaped
(`export const config = { matcher }` + a default export). It can refuse, redirect or rewrite a
request; it can never grant one — identity is resolved before it runs and the route's own gate
runs after, against whatever route the request finally lands on.

**METRICS · item 15.7** — `crates/albedo-server/src/metrics.rs`. Opt-in Prometheus endpoint on a
second listener via `ALBEDO_METRICS_ADDR`, bound before readiness so a taken port is a boot
failure rather than a scrape that silently never answers.

**ENGINE POOL** — `engine_pool.rs` +726 lines across rounds 4–7: an interrupt handler with realm
rebuild, a panicked job replacing its engine, shutdown ending live streams, dead pool slots
retired, and `with_engine_budget` so a job can declare a longer deadline than a request without
raising it for every render on the same pool.

**OUTBOUND / SHUTTER** — per-host shared budgets, `ALBEDO_INSTANCES` splitting app-wide budgets,
actions charged for their outbound calls, and the host budget moved to the APERTURE wire so it
covers every outbound surface including the refresh loop and OAuth.

**BUILD PROFILE** — `[profile.dev] debug = "line-tables-only"`. See *Proven* for why.

**DEPENDENCIES** — `cron` 0.12 and `chrono` 0.4. `cron` is pinned to 0.12 deliberately: 0.15+
parse with `winnow` 0.6 while this tree already resolves `winnow` 0.7, and two majors of one
parser to read a five-field string is not worth it. 0.12 adds exactly one crate.

### The design calls

**A job runs *for* a principal, never *as* the system.** There is no god-mode identity and the
number of partition bypasses in the tree is **zero**. Identity arrives three ways: enqueued work
inherits the enqueuer's principal; a plain scheduled run is anonymous and is refused an
identity-partitioned read exactly as an unauthenticated request is; and a scheduled job
declaring `over: "users"` has its fire expand into one run per principal, each carrying exactly
one. That fan-out is what makes a per-user digest possible *without* any identity that can see
every user's rows.

**Exactly-once cron is a property of the schema, not of the scheduler.** A schedule is a pure
function from an instant to the next instant, so every process computes the same fire times; the
row id is derived from one (`digest@2026-09-17T03:00:00Z`); the id is the primary key. Two
processes racing the 03:00 tick produce one row and one no-op `INSERT OR IGNORE`. This holds on
the multi-process topology item 13.2 refuses, and across a restart mid-tick. A uuid would have
bought nothing and cost exactly this.

**A lease, not a lock.** A claim carries an expiry, so a `SIGKILL`ed process's work returns
without anything having to notice it died. Reclaiming a lease **increments the attempt count** —
without that, a job that kills the process it runs in is immortal and presents as a
crash-looping deployment.

**The write builtins are imported, never global.** `bridge.rs::lower_effect` records that
`append`/`update`/`remove` are `const`s inside the per-request handler IIFE and *not* globals,
which is what makes the SANDGATE-B origin stamp a tripwire that never fires rather than the only
thing between a package and a database write. Installing them as globals for the job pass would
have demoted that check to the sole defence, so a job body imports them instead, staging is live
only inside an open pass, and re-entering a pass throws rather than replacing it.

**Nothing commits on a suspended pass.** A job body that writes and then awaits a `fetch()`
re-runs from the top, so effects staged before the suspension are discarded rather than
returned — otherwise the write would be applied once per round trip.

### Proven

- **Suites: 2 296 passing across 98 binaries** — root 70 / 1 813, `albedo-server` 28 / 483,
  both exit 0, on a warm run. Baseline before this work was 89 binaries / 2 133.
- **Exactly-once firing, measured:** 52 distinct consecutive fire ids over 52 s of an
  `every 1s` schedule, read out of the WAL. No duplicates.
- **A job's write reaches a rendered page** — schedule → queue row → engine → FORGE write →
  HTTP render, on the real binary from `albedo init`.
- **An action can queue work that outlives the request** — form action → `enqueue()` → runner →
  `append` → page. This is the shape transactional email needs.
- **An `enqueue` with a fixed id lands once** though its caller fired every second.
- **The built-in reaches Rust**: a due `albedo:expire_sessions` row is claimed and completed.
  Falsified by breaking `Builtin::from_name`, which produced the intended terminal failure.
- **The build profile, measured**: test-binary `.pdb` **430 MB → 61 MB (7×)**, main binary
  438 → 191 MB (2.3×), fully-built tree **440 GB → 14 GB**. `line-tables-only` rather than
  `debug = false` because panic backtraces must keep `file:line` — verified by forcing a panic
  and reading `src\jobs\queue.rs:581:9` out of it.

**Four falsifications run against the job queue**, because a first-try green in this repo is a
smell. Breaking the cron normalisation, the import binding, and `INSERT OR IGNORE` each failed
exactly the tests that should have failed. The fourth failed nothing — see *Open*.

### Deliberately not built

- **`broadcast()` in a job body.** The updater form needs a pre-write topic snapshot the job
  pass does not build, and a durable write already fans out through `apply_writes`.
- **Catch-up of missed fires.** A schedule is a set of instants and the runner only enqueues the
  next one. An outage must not emit a burst of backdated work the moment it ends.
- **Job priorities, chaining, per-job concurrency caps, cancelling a running job.** Each is real;
  none is table stakes; all are cheaper to add later than to remove.
- **Distributed queueing.** Single-process is doctrine.

### Open

- 🔴 **The claim's `AND state = ?` guard is unproven by the concurrency tests, and that is
  recorded rather than hidden.** Removing it fails no test: `LibSqlSubstrate` holds one writer
  mutex for every write and transaction, so the *transaction* is what makes the claim atomic and
  the guard is insurance for if that ever stops being true. It has its own direct test — a
  stale-state `UPDATE` affecting zero rows — and the doc comment says which mechanism is doing
  the work.
- 🔴 **For jobs, the SANDGATE-B origin stamp is load-bearing, not a tripwire.** A job body is an
  ordinary module rather than a handler IIFE. The reachability argument rests on pass state and
  on that check, and is written down in the glue.
- 🔴 **`cargo test` on the root package segfaults on a cold run** — the first invocation after a
  rebuild, 4/4 consistently in one session, truncating identically at 11 binaries / 1 493 passed
  and naming `auth_session_store`, which is 15/15 green alone. The warm re-run gives 70 / 1 813.
  A single cold run is therefore not evidence, and **the binary count is the check that catches
  it** — 1 493 passed reads as healthy until you notice 59 binaries never ran.
- 🟠 **13.1's SIGTERM handling is not verified on this rig** (Windows delivers no POSIX signals);
  it rests on the tokio contract plus a container test.
- 🟠 **Live ACME issuance remains unverified.** Rehearse with `--acme-staging` first — a typo
  burns the week's certificate quota.
