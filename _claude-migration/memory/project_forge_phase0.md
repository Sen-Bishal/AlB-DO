---
name: project_forge_phase0
description: FORGE Phase 0 implementation started — DataSubstrate seam + local libSQL backend live on the FORGE branch
metadata: 
  node_type: memory
  type: project
  originSessionId: d910705a-bdb8-4107-8ca1-102cda03a044
---

FORGE moved from vision ([[project_backend_paradigm]]) to code. Work lives on the **`FORGE` git branch** (user cut it 2026-07-04; user owns commits, nothing staged). Sequencing decision from that session: **FORGE Phase 0 before the soul-slice/wire-codec** — de-risk the inference thesis (the Ur/Web autopsy) first; category-defining "where's the backend?" demo travels further than a bytes-on-the-wire number, esp. through the non-technical cofounder. Flip only if a funder meeting makes "why not Vercel" the live question.

**Substrate decisions:**
- **libSQL, not redb** — the plan verifies *emitted SQL is N+1-free* (`backend.md:443`) and Pillar 4 is relational; redb (KV) would mean hand-building a query layer. `unsafe_code = "forbid"` (workspace) was the only thing favoring redb; user granted "unsafe permitted" → libSQL wins. **Local file only** for Phase 0; remote/embedded-replica = later deploy-manifold/edge concern.
- Everything feature-gated behind cargo feature **`forge`** (off by default) so `main`'s build/deps are untouched. Verified: `libsql` absent from default `cargo tree`.

**What's built (`src/forge/`, all additive, NOT wired into serve path yet):**
- `substrate.rs` — `DataSubstrate` trait: object-safe + async (`async-trait`), SQL-shaped: `migrate` / `query(sql,&[SqlValue])->Rows` / `execute->u64`. Durable actions (crash-survival) deliberately deferred — an orchestration layer *on top of* `execute`, not a trait change.
- `value.rs` — substrate-neutral `SqlValue`/`Row`/`Rows`/`SubstrateError` so no storage crate leaks into the engine (the pluggability point).
- `mem.rs` — `RecordingSubstrate` test double (records calls, replays canned Rows; does NOT parse SQL — that's exactly libSQL's job).
- `libsql.rs` — `LibSqlSubstrate` (`#[cfg(feature="forge")]`), `open_local`/`open_in_memory`. Real test persists+reads a row from in-memory libSQL. Gotcha hit: `execute_batch` returns `BatchRows` not `()`.
- `async-trait` added to root crate deps; `libsql = "0.6"` optional.

**The seam that already existed (where escape analysis will plug in):** `DataSource::DbQuery { query, param_keys }` on `TierBNode.data_deps` (`schema.rs:290`), emitted by `data_deps_for_component` (`builder.rs:1197`). Nothing executes those today — that's the hole FORGE fills.

**Next steps (dependency order):** ✅ #1 libSQL local substrate DONE. → #2 escape pass detects ONE persistent collection in `ComponentGraph`, emits a `DbQuery` dep. → #3 serve-path pre-resolves the `DbQuery` through the substrate into props *before* the sync QuickJS render (first browser-visible moment). → #4 durable write + crash-survival (the Phase 0 exit-criterion headline).

Theory to study: **Ur/Web** (Chlipala, POPL 2015) is the theoretical spine + the risk autopsy; **DBSP** (arXiv 2203.16684) for Phase 2 IVM. No single "FORGE paper" — it's the *combination* (`backend.md:228`).

---

**UPDATE 2026-07-06 — strategy pivot + wiring started.** After deep design (4 sessions),
the build order changed from the note above: **slot-tier-first, #3-before-#2, a
"walking skeleton."** Rationale in [[project_forge_capability_budget]] (the 4 data tiers) and
the slot-tier pressure test: a *persistent collection = a `BroadcastRegistry` topic whose value
is materialized from the substrate* (SSR-readable via `useSharedSlot` at render, live-reactive
across tabs for free, verified against `shared_slot_golden`). So #3 targets the **slot seam**,
not the `DataDep`/`TierBDataFetcher` request-fetch seam. Guestbook-as-**display-list** (static
items = free reactivity lane). Gates: (1) boot-hydrate topic→SSR shows rows · (2) live-across-tabs
· (3) survives restart · (4) crash-mid-write. Write path = **mutate-DB→rematerialize→`write_topic`**,
a NEW durable primitive (reuses only the fan-out), NOT the ephemeral `broadcast(topic,updater)`.

**Tasks 1–2 DONE (compile-verified, forge-off build untouched):**
- `crates/albedo-server/Cargo.toml`: added `[features] forge = ["dom-render-compiler/forge"]`
  passthrough (root crate's `forge` = `dep:libsql`).
- `server.rs`: `RuntimeState` gained `#[cfg(feature="forge")] forge_substrate:
  Option<Arc<dyn ...forge::DataSubstrate>>` (persistent tier, survives dev world-swap);
  `AlbedoServer::run(mut self)` opens `LibSqlSubstrate::open_local("forge.db").await` once
  before the listener binds (sole async boot seam — `build()` is sync). `None` seeded in `build()`.
- Boot-hydration insertion point identified: `server.rs:506` `for topic in
  project.shared_slot_topics() { self.broadcast.topic(topic, b"null") }` — Gate-1 (next task)
  replaces the `b"null"` seed with the materialized `SELECT` for FORGE-backed topics, via an
  async hydration pass in `run()` after the substrate opens.
- NOT yet verified at runtime (no observed "FORGE substrate opened" log) — compile bar only;
  the forge `albedo` bin is a SEPARATE build (`--features forge`); default `albedo` has no FORGE.

**⚠ BUILD-ENV GOTCHA (durable — bites every `--features forge` build/test).** `libsql-ffi
v0.5.0` needs a C toolchain AND its build.rs shells out to Unix `cp` (build.rs:42). This shell
has neither on PATH by default. `vswhere` is empty (BuildTools v18 SKU not in the instance
store) so `cc` can't auto-find MSVC. Recipe that works, in ONE PowerShell call (env doesn't
persist across tool calls):
```
$vc="C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cmd /c "`"$vc`" && set" 2>$null | %{ if($_ -match '^([^=]+)=(.*)$'){ Set-Item "env:$($matches[1])" $matches[2] } }
$env:PATH = $env:PATH + ";C:\Git\usr\bin"   # APPEND for `cp`
cargo check -p albedo-server --features forge
```
**⚠ APPEND, don't prepend** Git usr/bin: it also contains a GNU `link.exe` that shadows
MSVC's `link.exe`, breaking the *link* step (`/usr/bin/link: missing operand`). `check` doesn't
link so prepend "worked"; `test`/`build` link and fail. Append → MSVC `cl.exe`/`link.exe` win,
`cp` still found.

**GATE 1 (read loop) — ENGINE-SIDE DONE & GREEN (2026-07-06).** `src/forge/skeleton.rs`
(unconditional — speaks only the `DataSubstrate` trait): `FORGE_SLOTS` topic→query map
(hand-authored stand-in for #2 inference; one entry: `guestbook`), `bootstrap_schema`
(CREATE+seed-if-empty; idempotent = Gate-3 property), `hydrate_topics` (query→`rows_to_json`
array-of-objects→`broadcast.topic()`+`write_topic()` to override the register-time `b"null"`
seed). Wired into `AlbedoServer::run()`: open substrate → bootstrap → hydrate → serve. 3 tests
green: 2 unit (`--features forge`) + **e2e** (`tests/forge_gate1_e2e.rs` + fixture
`tests/fixtures/forge_guestbook/Component.tsx`) proving the FULL loop through the real
`render_entry_with_broadcast` path — SSR HTML =
`<ul data-forge="guestbook">…<li>ada: first light</li><li>alan: the machine stirs</li>…</ul>`.
The `.map()` over `useSharedSlot` renders server-side; the `<span style="display:contents"
data-albedo-id>` list-binding wrapper is already emitted → Gate 2 reactivity hooks present.

**⚠ LIVE-SERVE GAP FOUND (2026-07-06) — the e2e-render proof ≠ the production serve.** Built a
presentable demo app `A:\forge-guestbook` (`albedo init`; Anthropic-esque serif/bone/clay theme
in `src/styles.css`, guestbook at `index.tsx`, minimal `layout.tsx`; launch cfg `forge-guestbook`
:3120). Forge bin built (`cargo build -p albedo-server --bin albedo --features forge`; overwrites
`target/debug/albedo.exe`). Served: **page renders gorgeously but the entries list is EMPTY.**
Root cause chain: (1) `useSharedSlot`→Tier-B, and **Stream-B pre-renders/BAKES Tier-B HTML at
`albedo build` time** (streaming.rs:861 "Tier-B opcode frame baked into the manifest by Stream
B") when the topic is still the `b"null"` register seed → served HTML ships baked null →
`null.map()` threw → `__albedo_inject(...,'error')`. (2) FORGE hydrates the topic at *serve-boot*
(run()), AFTER the bake — so runtime rows never reach the static HTML. (3) Rows are meant to reach
the CLIENT via `auto_subscribe` initial SlotSet, but that path is only in the **WT** handler
(streaming.rs:863); the demo negotiated **SSE** (no QUIC certs) → list stays empty. (4) My
`(entries||[])` null-guard stopped the error but likely broke the reactive-list detection (array
expr no longer a bare slot ident). **dev mode same gap** (also bakes). So neither serve nor dev
shows rows in static HTML. **The slot-tier "SSR shows the rows" claim holds at the RENDER fn
(`render_entry_with_broadcast`, e2e-proven) but NOT through the build-bake serve pipeline.**
Fix options: **(A) build-time hydration** — `albedo build` opens substrate+hydrates FORGE topics
BEFORE Stream-B bake → baked HTML carries real rows → curl+browser+no-JS visible; = "build-tier"
data (SSG-ish, fine for read-only Gate-1; revert component to clean `entries.map()`). **(B)
request-time re-render** of shared-slot Tier-B against the hydrated registry (truer live SSR,
bigger serve change). **(C)** wire SSE auto_subscribe + confirm client list-render from SlotSet
(reactive path). Recommended for the demo: **A**. Surfaced to user for decision; NOT yet
implemented. Nothing committed.

**BUILD-TIME HYDRATION (A) IMPLEMENTED (2026-07-06) + DEEPER SERVE-PATH GAP FOUND.** Code:
`skeleton::materialize_seeds(substrate)->Vec<(topic,bytes)>` (hydrate_topics now calls it);
`ManifestBuilder` gained `forge_topic_seeds` field + free fn `materialize_forge_seeds(working_dir)`
(opens `<root>/forge.db`, bootstraps, materializes; runs on a `std::thread::scope` thread w/ its
own current-thread runtime so it's safe whether or not an ambient tokio runtime exists — dev
hot-reload rebuilds *inside* the serve runtime); `render_tier_b_inline` pre-seeds its fresh
registry from `forge_topic_seeds` before `render_entry_with_broadcast`. **PROVEN working at the
bake layer:** `albedo build` creates `forge.db`, and `render-manifest.v2.json` contains the baked
`<li><span class="entry-message">first light</span>…` rows AND the served page's `data-base64`
frame decodes to the exact rows JSON. So the data reaches the browser.
**BUT the browser still shows an empty list.** Root cause (fully traced): the guestbook is
**Tier-B**, and its serve-time `__albedo_inject("__b_guestbook_3", …)` is generated at REQUEST
time (NOT in the manifest — grep=0), and that request-time Tier-B render does **not install the
hydrated `BroadcastRegistry`** (PHASE_K_BROADCAST) → `useSharedSlot("guestbook")` resolves
**null** → clean `entries.map()` throws (`,'error'` → island never enters DOM); the `(entries||[])`
guard avoids the throw but drops the reactive list-binding so the frame's `SlotSet` can't populate
the `<ul>`. **The real fix is ALBEDO-serve-path, not FORGE:** make the request-time Tier-B page
render install the run()-hydrated `world.broadcast` so `useSharedSlot` resolves (→ rows render in
SSR directly, even no-JS). Render sites needing the hydrated registry: (1) build manifest bake
✅fixed via `forge_topic_seeds`; (2) **request-time Tier-B render — the gap**; the Tier-C reactive
block builder (`renderer_runtime::build_reactive_blocks`) is a different path (islands, not this).
libSQL `Connection` is `Clone`; a fresh forge.db is created by build now. App at `A:\forge-guestbook`
(launch cfg `forge-guestbook`:3120). Styling DONE (Anthropic bone/clay/Newsreader — screenshotted,
cofounder-ready). Nothing committed.
**NOT yet done:** live `albedo serve` in a browser (needs forge-enabled bin
`cargo build -p albedo-server --bin albedo --features forge` + a built app) — the test proves
the mechanism through the identical render fn the server uses. Next: Gate 2 (write→live), Gate 3
(restart persistence), Gate 4 (crash-mid-write durable). User pushes to git only after seeing it
run; nothing committed.

---

**UPDATE 2026-07-09 — read loop COMMITTED; WRITE loop started (transaction seam = commit #1).**
The Gate-1 read loop landed on-branch as commit **`7a7e8e0` "firing the forge"** (substrate opens
at boot on persistent `RuntimeState`, `bootstrap_schema`+`hydrate_topics` feed the guestbook topic
before the listener binds; build-time `materialize_seeds` too). FORGE branch is 1 commit ahead of
`main`.

**Strategic scoping decision (this session, reconciled against [[project_war_doctrine]]):** THE
DROP's **Beat 1 gate = crash-ATOMIC, NOT crash-resumable.** Verified the tree has NO durable-action
seam, NO intent log, NO transaction primitive; WAR.md §6/§5.4 claims "resumes on restart" (Reading
B = DBOS-style durable execution, research-adjacent). Split it: **crash-atomic** (zero oversell,
exact count, clean rejection, committed state survives a kill — buildable now, provable, already
beats Astro per §5.2) is the gate; **crash-resume** (the survivor's purchase continues on reboot)
is a SEPARATE later milestone. Rationale: §5.5's own "demo inverts if flaky" logic says don't stake
the gate on the least-built clause. Also §4 guardrail: cap time in Beat 1 (don't gold-plate into a
Temporal clone) so the MOAT (Beat 2 = CTRNI'TAS self-tuning, the thing that survives Cloudflare)
actually gets built. Build ladder: **①tx primitive → ②`reserve()` invariant fn → ③in-proc
concurrency harness (N buyers, 1 ticket, ×1000) → ④subprocess kill harness.**

**① DONE — the `Transaction` seam (NOT committed; user owns commits).** Files: `src/forge/`
`substrate.rs`, `libsql.rs`, `mem.rs`, `mod.rs` only.
- `substrate.rs`: added `DataSubstrate::begin(&self)->Result<Box<dyn Transaction>>` + new
  object-safe `Transaction` trait (`query`/`execute`/`commit`/`rollback`; commit/rollback take
  `self: Box<Self>` so a resolved tx can't be reused). Docs draw the crash-atomic-now vs
  crash-resume-later line.
- `libsql.rs`: factored shared `run_query`/`run_execute(&Connection,…)` (a `libsql::Transaction`
  derefs to `Connection`, so substrate + tx read/write via ONE path). `begin()` uses **`BEGIN
  IMMEDIATE`** (`transaction_with_behavior(TransactionBehavior::Immediate)`) — takes the write lock
  up front so concurrent reserve-commits serialize instead of racing a deferred-upgrade into
  `SQLITE_BUSY`. libSQL 0.6 `Transaction` is owned (no lifetime), `commit(self)`/`rollback(self)`.
  2 tests green: `rolled_back_transaction_leaves_no_trace`, `committed_transaction_persists`.
- `mem.rs`: `RecordingSubstrate` now `Arc<Mutex<Recording>>` so `RecordingTransaction` shares the
  log; commit/rollback are NO-OPS (documented — the double can't roll back; atomicity only proven vs
  real libSQL).
- Verified: **6 forge tests pass** (`cargo test -p dom-render-compiler --features forge --lib
  forge:: -j2`), default `cargo check` (no forge) compiles (seam is un-gated). Reverted format-on-
  save `wrap_comments` churn from `skeleton.rs`/`mod.rs` — HEAD is fmt-dirty, matched neighbors not
  the config. **Next = ② `reserve()`** (new `src/forge/drop.rs`): `begin`→`UPDATE drop SET
  remaining=remaining-1 WHERE id=?1 AND remaining>0`→if affected==1 INSERT purchase+commit (Won)
  else rollback (SoldOut). Caveat: `LibSqlSubstrate` holds ONE connection → N buyers serialize
  there; true multi-connection contention is a stronger follow-up, not required for the gate.
