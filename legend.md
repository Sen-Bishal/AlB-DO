# LEGEND — a reviewer's map of the ALBEDO codebase

> Orientation for someone cloning this repo cold. It explains **the one core idea**, the
> **end-to-end dataflow**, and then a **folder-by-folder / file-by-file legend** of what
> controls what and how the pieces correlate. Paths are clickable.

ALBEDO is a Rust-native compiler + HTTP runtime for JSX/TSX apps. You author React-style
components; ALBEDO compiles them ahead of time, decides *per component* how much client
JavaScript (if any) each one needs, and serves them from a streaming Rust server. The CLI is
`albedo` (`init` / `dev` / `serve` / `build` / `ship`).

---

## 1. The one idea you must hold: the three tiers

Everything in this codebase orbits a single compile-time decision. Every component is
classified into a **tier** based on what it actually does (`src/lib.rs` "Effect Lattice"):

| Tier | What it is | Client JS shipped | Rendered by |
|------|-----------|-------------------|-------------|
| **A** | No hooks, no async, no side effects — pure markup | **zero bytes** | pure-Rust evaluator |
| **B** | Light interactivity, event handlers → "binding mode" | island wiring only | server, wired to the client runtime |
| **C** | Full hook surface (`useState`/`useEffect`/…), async I/O | full hydration ("islands") | QuickJS on the server + hydrated on the client |

If you understand *why a given component is A, B, or C*, you understand ALBEDO. The moat is in
`src/analysis/` (classification) and `src/runtime/` (two different render engines for A vs B/C).
The classifier itself is [src/analysis/analyzer.rs](src/analysis/analyzer.rs), and the
soundness argument under it is the escape analysis in the same directory — a component whose
state escapes cannot be Tier A no matter what its hooks look like.

---

## 2. The 30-second dataflow

```
                   COMPILE (albedo build / dev)                         SERVE (albedo serve / dev)
   source .tsx ──► scan ──► parse ──► analyze(tier) ──► transform ──► ┐
   src/scanner   src/parser  src/analysis  src/transforms             │
                                                                      ▼
                                              IR + wire encode ──► RenderManifestV2 ──► albedo-server
                                              src/ir             src/manifest         crates/albedo-server
                                                                      │                    │ boots, holds a RenderWorld
                                              bundle client JS ───────┤                    │ dispatch per request:
                                              src/bundler + assets/   │                    │  · Tier-A → static HTML
                                                                      ▼                    │  · Tier-B → HTML + island wiring
                                              .albedo/dist/ (manifest + /_albedo/*.js) ────┘  · Tier-C → SSR + hydration
                                                                                             │
                                                                              browser ◄── assets/albedo-*.js
                                                                              (runtime, reactive, hydration, forms)
```

**The contract that ties the two halves together is `RenderManifestV2`**
([src/manifest/schema.rs](src/manifest/schema.rs)): the compiler's *only* output the server
consumes. If you want to see the seam between "compiler" and "runtime," read that struct.

---

## 3. Workspace map (3 crates + client assets)

| Unit | Path | Role |
|------|------|------|
| **`dom-render-compiler`** (root crate) | [src/](src/) | The compiler **and** the render runtime/engines **and** the `albedo` CLI. This is ~80% of the code. |
| **`albedo-server`** | [crates/albedo-server/](crates/albedo-server/) | The production HTTP server: boot, per-request dispatch, streaming SSR, actions, CSRF, dev overlay/HMR, WebTransport, inspector. |
| **`albedo-node`** | [crates/albedo-node/](crates/albedo-node/) | Thin N-API binding (one file) exposing the compiler to Node tooling. |
| **client runtime** | [assets/](assets/) | The `include_str!`'d browser JS that ships to hydrate Tier-B/C pages. Not Rust. |

Supporting dirs: [scaffold/](scaffold/) (what `albedo init` copies), [tests/](tests/) +
[fuzz/](fuzz/) + [benches/](benches/) (correctness/robustness/perf), [examples/](examples/),
[installer/](installer/).

---

## 4. The compiler — root `src/` (crate `dom-render-compiler`)

The pipeline runs roughly top-to-bottom in this list. Entry point: `RenderCompiler` in
[src/lib.rs](src/lib.rs); `optimize_manifest_v2()` is the "compile everything" call.

### Front end — get source into a typed graph
| File / dir | Controls |
|------------|----------|
| [src/scanner.rs](src/scanner.rs) | Walks the project, discovers component/route/module files, applies scan policy. |
| [src/parser.rs](src/parser.rs) | SWC-powered JSX/TSX parse → `ParsedComponent`; also seeds effect inference. |
| [src/graph.rs](src/graph.rs) | Builds the `ComponentGraph` (who imports/renders whom). The dependency backbone. |
| [src/types.rs](src/types.rs) | Core shared types (`ComponentId`, etc.). |
| [src/effects.rs](src/effects.rs) · [src/estimator.rs](src/estimator.rs) | Effect profiles and render-weight estimates that feed tiering. |

### Analysis — the tier brain
| File | Controls |
|------|----------|
| [src/analysis/analyzer.rs](src/analysis/analyzer.rs) | **The tier classifier.** Assigns A/B/C from effect profile. Start here for the core idea. |
| [src/analysis/topological.rs](src/analysis/topological.rs) · [parallel_topo.rs](src/analysis/parallel_topo.rs) · [parallel.rs](src/analysis/parallel.rs) | Topological sort + critical-path scoring → parallel render batches. |
| [src/analysis/adaptive.rs](src/analysis/adaptive.rs) | Adaptive/heuristic tuning of the analysis. |

### Transforms — per-tier rewrites of the AST
| File | Controls |
|------|----------|
| [src/transforms/hooks.rs](src/transforms/hooks.rs) | `useState`/`useEffect`/`useMemo`/… compile handling. |
| [src/transforms/events.rs](src/transforms/events.rs) | Event handlers → binding-mode wiring (Tier-B) vs client thunks (Tier-C). |
| [src/transforms/actions.rs](src/transforms/actions.rs) · [form.rs](src/transforms/form.rs) | Server `action()` + `<form>` compilation (the P6 feature). |
| [src/transforms/shared_slots.rs](src/transforms/shared_slots.rs) | `useSharedSlot` broadcast topics. |
| [src/transforms/link.rs](src/transforms/link.rs) | `<Link>` / client navigation interception. |
| [src/transforms/css_modules.rs](src/transforms/css_modules.rs) | `.module.css` scoping. |

### IR + wire — the opcode/binary layer
| File | Controls |
|------|----------|
| [src/ir/opcode.rs](src/ir/opcode.rs) | The instruction set the client runtime applies (DOM patches). |
| [src/ir/wire.rs](src/ir/wire.rs) · [columns.rs](src/ir/columns.rs) | Binary wire encoding of opcodes (the columnar codec). |
| [src/ir/action.rs](src/ir/action.rs) | The action-envelope encode/decode (client POST ⇆ server). **Hardened decode** (Gate-1-D). |
| [src/ir/conformance.rs](src/ir/conformance.rs) | Wire-format conformance checks. |

### Manifest — the compiler↔server contract
| File | Controls |
|------|----------|
| [src/manifest/schema.rs](src/manifest/schema.rs) | **`RenderManifestV2`** — the single artifact the server reads. The most important struct in the repo. |
| [src/manifest/builder.rs](src/manifest/builder.rs) | Assembles the manifest from analyzed components (shells, tier roots, hydration blocks). |
| [src/manifest/metadata.rs](src/manifest/metadata.rs) | `<head>`/metadata + `generateMetadata()` compilation. |

### Bundler — client JS emission
| File | Controls |
|------|----------|
| [src/bundler/classify.rs](src/bundler/classify.rs) · [plan.rs](src/bundler/plan.rs) | Decide which modules become client bundles and how. |
| [src/bundler/rewrite.rs](src/bundler/rewrite.rs) · [emit.rs](src/bundler/emit.rs) | Rewrite + emit the `/_albedo/*.js` chunks. |
| [src/bundler/npm.rs](src/bundler/npm.rs) · [vendor.rs](src/bundler/vendor.rs) · [precompiled.rs](src/bundler/precompiled.rs) | npm dependency bundling / vendoring. |
| [src/bundler/static_slice.rs](src/bundler/static_slice.rs) | Tier-A static-HTML slicing/dedup. |

### Hydration & routing
| File | Controls |
|------|----------|
| [src/hydration/plan.rs](src/hydration/plan.rs) · [payload.rs](src/hydration/payload.rs) · [script.rs](src/hydration/script.rs) | What islands hydrate and the payload/script that drives them client-side. |
| [src/routing/file_based.rs](src/routing/file_based.rs) | `src/routes/` file-based routing, incl. `[slug]` dynamic params. |
| [src/budget/](src/budget/) | `tier-budget.toml` gate — enforces JS-size budgets at build (`albedo build`). |

### Runtime — the two render engines (this is the kernel)
Lives in [src/runtime/](src/runtime/). ALBEDO renders Tier-A with a **pure-Rust evaluator** and
Tier-B/C with **QuickJS**, sharing a request-scoped memory arena.
| File | Controls |
|------|----------|
| [src/runtime/eval/core.rs](src/runtime/eval/core.rs) | The **pure-Rust JS evaluator** for Tier-A (`eval_body_stmts` etc.). Unsupported constructs fail *loudly* → punt to QuickJS. |
| [src/runtime/quickjs_engine.rs](src/runtime/quickjs_engine.rs) · [engine.rs](src/runtime/engine.rs) | The **QuickJS engine** for Tier-B/C. |
| [src/runtime/arena.rs](src/runtime/arena.rs) | Request-scoped bump arena (warmup only; per-request allocs go to the system allocator). ⚠️ The O(1) bump-reset was **removed deliberately** — it allowed a cross-request use-after-free. Do not reintroduce it. |
| [src/runtime/compiled.rs](src/runtime/compiled.rs) | `CompiledProject` + `invoke_action_*` — the per-request action invocation path. |
| [src/runtime/renderer/manifest.rs](src/runtime/renderer/manifest.rs) | `render_route*` — turns a manifest route into HTML (the streaming SSR core). |
| [src/runtime/slot_store.rs](src/runtime/slot_store.rs) · [broadcast.rs](src/runtime/broadcast.rs) · [session.rs](src/runtime/session.rs) | State: per-session slot values, broadcast topics, session ids. |
| [src/runtime/form_result.rs](src/runtime/form_result.rs) | Projects an action's `{error:{field}}` return onto the form's error slots (P6). |
| [src/runtime/bridge.rs](src/runtime/bridge.rs) | Host-object bridge between Rust and the JS engines (A1). |
| [src/runtime/webtransport.rs](src/runtime/webtransport.rs) · [highway.rs](src/runtime/highway.rs) · [pipeline.rs](src/runtime/pipeline.rs) | Transport + the multi-lane render pipeline. |

### CLI
| File | Controls |
|------|----------|
| [src/bin/albedo.rs](src/bin/albedo.rs) | **The `albedo` command.** `init`/`dev`/`serve`/`build`/`ship` dispatch, contract resolution, dev watch→rebuild→hot-swap. `dev` and `serve` both boot the *same* production pipeline, deliberately — divergence between them was a recurring source of "works in dev" bugs. |
| [src/bin/albedo/printer.rs](src/bin/albedo/printer.rs) | The "Halation" CLI styling + tier report. |
| [src/bin/albedo/first_run.rs](src/bin/albedo/first_run.rs) | First-run experience. |
| [src/bin/albedo-bench.rs](src/bin/albedo-bench.rs) · [dom-compiler.rs](src/bin/dom-compiler.rs) | Bench + raw-compiler bins. |

### FORGE — the database
Declared collections become tables, live topics, and generated types. Feature-gated as `forge`,
but **on by default**: a build without it silently yields `null` topics at serve time.
| File | Controls |
|------|----------|
| [src/forge/declare.rs](src/forge/declare.rs) | The declaration language — `FieldType` (`text`/`int`/`real`/`bool`/`timestamp`) and `FieldSpec { ty, nullable }`, the trailing `?`. What an app is allowed to say about its own schema. |
| [src/forge/write.rs](src/forge/write.rs) | `append`/`update`/`remove`. ⚠️ `returned_record` splices a `RETURNING` row without re-querying — it and `skeleton::rows_to_json` are **two conversion sites that must agree**, or a value's type changes between the write and the next full read. |
| [src/forge/drift.rs](src/forge/drift.rs) | `evolve_schema` — adds a new nullable column via `ALTER`, refuses every other change **by name**. The declared SQL type names (`BOOLEAN`/`TIMESTAMP`) survive in `PRAGMA table_info`, which is what makes a retype detectable. |
| [src/forge/mod.rs](src/forge/mod.rs) | Collection handles, partition keys, and the projection into topic values. |

### AUTH — identity as a key source
| File | Controls |
|------|----------|
| [src/auth/principal.rs](src/auth/principal.rs) | `Principal` — one identity type regardless of which credential minted it. |
| [src/auth/session.rs](src/auth/session.rs) · [store.rs](src/auth/store.rs) | Session cookies and the session store. |
| [src/auth/password.rs](src/auth/password.rs) | First-party password credentials (argon2). |
| [src/auth/declare.rs](src/auth/declare.rs) | Declared route policy and the gate that enforces it. |
| [src/shutter/gcra.rs](src/shutter/gcra.rs) · [cost.rs](src/shutter/cost.rs) | GCRA rate limiting; the weight of a request is **derived**, not configured. Credential attempts are metered here. |

🔑 **The invariant worth understanding before changing any of this:** identity reaches the read
path as **data** (a partition key on a topic), not as `if` statements in handlers. That is what
makes `messages.where({ owner: user.id })` row-level security which cannot be written wrong — the
read and the policy are the same expression, and an author cannot spell the topic name directly.
Every "policy authored apart from the read" footgun is structurally absent, and it stays absent
only as long as **every** read path mints a topic.

### APERTURE — the outside world
| File | Controls |
|------|----------|
| [src/aperture/egress.rs](src/aperture/egress.rs) | The egress policy, enforced at the **resolver** level so it cannot be bypassed by DNS rebinding. ⚠️ An IP-literal URL once bypassed this entirely — that class of hole is what this file exists to close. |
| [src/aperture/cache.rs](src/aperture/cache.rs) · [client.rs](src/aperture/client.rs) | Byte-budgeted LRU response store; cache lookup, single-flight coalescing, conditional revalidation. |
| [src/aperture/transport.rs](src/aperture/transport.rs) | The `reqwest` implementation of the network seam. |

⚠️ **Phase A0.** There is no JS surface: no global `fetch` is installed and nothing in a handler
body can reach this module. Do not document outbound HTTP as a user-facing feature until A1/A2 land.

### `albedo doctor`
| File | Controls |
|------|----------|
| [src/doctor/matrix.rs](src/doctor/matrix.rs) | The reach matrix — what can reach what, and under which principal. **Derived from the tree, never a maintained list**; a hand-maintained capability table goes stale the first time someone forgets it. |

---

## 5. The server — `crates/albedo-server/`

Consumes `RenderManifestV2` + the dist and answers HTTP. Entry: [lib.rs](crates/albedo-server/src/lib.rs)
→ [boot.rs](crates/albedo-server/src/boot.rs).

| File / dir | Controls |
|------------|----------|
| [boot.rs](crates/albedo-server/src/boot.rs) · [lifecycle.rs](crates/albedo-server/src/lifecycle.rs) | Boot the server from a dist dir; build the `RenderWorld`; wire dev mode. |
| [server.rs](crates/albedo-server/src/server.rs) | **Per-request dispatch** (`dispatch_inner`). Holds the swappable `RenderWorld` behind an `RwLock` (the hot-swap seam for dev reload). |
| [renderer_runtime.rs](crates/albedo-server/src/renderer_runtime.rs) | Bridges the manifest render into the request lifecycle. |
| [handlers/streaming.rs](crates/albedo-server/src/handlers/streaming.rs) | **Streaming SSR** of a page (Tier-A/B/C shell + islands). |
| [handlers/action.rs](crates/albedo-server/src/handlers/action.rs) | **Action POST** path: decode envelope → CSRF gate → invoke handler → wire response. |
| [handlers/public_assets.rs](crates/albedo-server/src/handlers/public_assets.rs) · [albedo_assets.rs](crates/albedo-server/src/handlers/albedo_assets.rs) | Static + framework-JS (`/_albedo/*.js`) serving. |
| [render/csrf.rs](crates/albedo-server/src/render/csrf.rs) | Per-session CSRF tokens. ⚠️ **Known landmine:** in-memory `DashMap` → 403s across multi-instance deploys + unbounded growth. Fix belongs with the deploy adapter (see §7). |
| [render/form_action.rs](crates/albedo-server/src/render/form_action.rs) · [form_validation.rs](crates/albedo-server/src/render/form_validation.rs) · [tier_b.rs](crates/albedo-server/src/render/tier_b.rs) | Form action rendering, validation reconciliation, Tier-B injection. |
| [dev/hmr.rs](crates/albedo-server/src/dev/hmr.rs) · [dev/error_overlay.rs](crates/albedo-server/src/dev/error_overlay.rs) | Dev-only HMR channel + in-browser error overlay (injected only when `dev_mode`). |
| [webtransport.rs](crates/albedo-server/src/webtransport.rs) | WebTransport/QUIC transport. |
| [timing.rs](crates/albedo-server/src/timing.rs) | Live per-request server-compute timing printed to the terminal. |
| [inspector/](crates/albedo-server/src/inspector/) | **Live** server-side inspector/metrics (graph, events, publisher, heartbeat). NB: distinct from the old *bin-side* inspector, which was dead code and was deleted in the readiness sweep. |
| [config.rs](crates/albedo-server/src/config.rs) · [routing.rs](crates/albedo-server/src/routing.rs) · [contract.rs](crates/albedo-server/src/contract.rs) | Server config, route table, dev contract. |

---

## 6. The client runtime — `assets/*.js`

Shipped to the browser via `include_str!` (so **editing these requires rebuilding the binary**).
| File | Controls |
|------|----------|
| [assets/albedo-runtime.js](assets/albedo-runtime.js) | Entry client module; applies wire opcodes / patches. |
| [assets/albedo-reactive.js](assets/albedo-reactive.js) | The reactive core for hydrated islands. |
| [assets/albedo-hydration.js](assets/albedo-hydration.js) | Boots Tier-C islands from the hydration payload. |
| [assets/albedo-link-forms.js](assets/albedo-link-forms.js) | Intercepts `<Link>` navigation + `<form action>` submits → action POSTs. |
| [assets/albedo-wt-bootstrap.js](assets/albedo-wt-bootstrap.js) | WebTransport bootstrap. |
| [assets/albedo-hmr-apply.js](assets/albedo-hmr-apply.js) · [albedo-error-overlay.js](assets/albedo-error-overlay.js) | Dev HMR apply + error overlay (dev only). |
| [assets/bincode.js](assets/bincode.js) | Wire (bincode-compatible) decode in the browser — mirror of `src/ir/wire.rs`. |
| [assets/albedo-client.js](assets/albedo-client.js) | Shared client glue. |

**Correlation to watch:** the wire format is defined in Rust ([src/ir/wire.rs](src/ir/wire.rs) /
[opcode.rs](src/ir/opcode.rs)) and re-implemented in JS ([assets/bincode.js](assets/bincode.js) +
runtime). These two must stay in lockstep — a change on one side without the other breaks
hydration silently.

---

## 7. Cross-cutting correlations ("what breaks what")

- **Compiler → Server contract = `RenderManifestV2`.** Change [src/manifest/schema.rs](src/manifest/schema.rs)
  and you change what every server handler can rely on. (The golden test
  `tests/fixtures/golden/manifest_v2_test_app_components.json` pins this — it caught a real drift
  during the readiness sweep.)
- **Wire format spans Rust + JS.** [src/ir/wire.rs](src/ir/wire.rs) ⇄ [assets/bincode.js](assets/bincode.js).
- **Tiering decides everything downstream.** [src/analysis/analyzer.rs](src/analysis/analyzer.rs)'s
  A/B/C verdict drives which transform runs, whether a client bundle is emitted, and which server
  render path answers the request.
- **`dev` and `serve` are one renderer.** Both go through `boot_production_server`; `dev` just flips
  `dev_mode` on (overlay + HMR + watch→hot-swap). Do **not** reintroduce a second dev renderer — the
  legacy one was deleted in the readiness sweep.
- **`assets/*.js` are compiled into the binary.** Editing them without rebuilding the `albedo`
  binary ships stale client code.
- **CSRF state is per-process.** [render/csrf.rs](crates/albedo-server/src/render/csrf.rs) holds tokens
  in-memory; correct for a single `albedo serve`, but the stateless keyed-MAC fix must land with the
  deploy adapter before multi-instance hosting.

---

## 8. Where to start reading (by goal)

- **"How does a component become HTML?"** → [src/lib.rs](src/lib.rs) doc → [src/analysis/analyzer.rs](src/analysis/analyzer.rs) → [src/runtime/renderer/manifest.rs](src/runtime/renderer/manifest.rs).
- **"How does a click reach the server?"** → [assets/albedo-link-forms.js](assets/albedo-link-forms.js) → [handlers/action.rs](crates/albedo-server/src/handlers/action.rs) → [src/runtime/compiled.rs](src/runtime/compiled.rs).
- **"What does the CLI actually do?"** → [src/bin/albedo.rs](src/bin/albedo.rs) (`run_serve_command`, `run_live_dev_runtime`, `run_prod_build`).
- **"What's the compiler↔runtime seam?"** → [src/manifest/schema.rs](src/manifest/schema.rs).
- **"Where does the data live?"** → [src/forge/declare.rs](src/forge/declare.rs) → [src/forge/write.rs](src/forge/write.rs) → [src/runtime/broadcast.rs](src/runtime/broadcast.rs).
- **"How is a read authorized?"** → [src/auth/principal.rs](src/auth/principal.rs) → [src/transforms/shared_slots.rs](src/transforms/shared_slots.rs) (a topic's partition key) → [src/auth/declare.rs](src/auth/declare.rs).
- **Roadmap and current state** → [README.md](README.md) § *What isn't done* and § *Where it's headed*.

---

## 9. A note on the design-document references

Doc comments throughout the source cite files under `development-plan/` — `APERTURE.md`,
`AUTH.md`, `PHOSPHOR.md`, `PRISM.md`, and others. **Those are internal working notes and are
not published in this repository.** They are cited because that is honestly where a given
invariant was argued out, and stripping the citations would make the comments claim less
provenance than they have.

Nothing in the code depends on them. This file and the source are the normative description;
where a comment says *"invariant 2.7"*, the invariant it names is stated in the comment itself.

---

*Known open items a reviewer should not be surprised by are called out inline with ⚠️.*
