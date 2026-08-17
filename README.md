# AlB'DO

A JSX/TSX render compiler, database, and HTTP server, written in Rust. You
write React-shaped components; AlB'DO compiles and serves them as one binary,
with no Node.js in the request path.

The compiler decides *per component* how much client JavaScript it needs —
often none — and it does the same for your data: a collection you declare in
config becomes a persisted table, a live subscription, and a typed `.d.ts`,
without a server directory, an ORM, an API route, or any WebSocket code.

It is a work in progress — a prototype built in the open, not a finished
framework. Everything under "What works" below is implemented and exercised by
the test suite; everything under "What isn't done" is honest about the gaps.
The APIs will still move.

---

## What works right now

### Compiler and rendering

- `.tsx`/`.jsx` parsed with SWC and rendered to HTML.
- **Three-tier classification, decided at build time, with nothing to
  configure.** Every component is analysed for what it actually does and
  compiled accordingly (see [How rendering is decided](#how-rendering-is-decided)).
- **Binding mode** — for many stateful components the compiler ships a few
  hundred bytes of bindings instead of hydrating the whole component. A click
  updates the bound node locally: no VDOM, no re-render, no network round-trip.
- File-based routing, nested `layout.tsx` composition, and dynamic routes
  (`/post/[id]`).
- `error.tsx` / `loading.tsx` boundaries, and `async` server components.
- Global CSS and CSS modules, `<title>` and meta tags.
- `useState` / `useEffect` / `useRef` / `useMemo` / `useCallback` islands that
  hydrate in the browser.
- Server `action()` handlers and SSR run under an embedded JS engine (QuickJS),
  so real JavaScript — loops, `try`, array methods — works, and a broken
  construct fails loudly instead of rendering null.
- npm dependencies bundled in-tree for SSR and actions (tested against `zod`
  and `date-fns`).
- A dev server with hot reload over SSE.

### FORGE — the backend, on by default

- Declare a collection in `albedo.config.ts` and it exists: table, seed rows,
  and a live topic. No migration folder, no ORM, no API route.
- Field types `text`, `int`, `real`, `bool`, `timestamp`, with a trailing `?`
  for nullable. A `bool` round-trips as a `bool` — writing `true` and reading
  it back does not hand you a `1`.
- `append` / `update` / `remove` as server actions.
- **Additive schema evolution.** Add a nullable field, restart, and the column
  is added with existing rows intact. Anything else is refused by name rather
  than applied halfway.
- Typed `.albedo/forge.d.ts` generated from the declared schema.

### Live data

- `useSharedSlot` for scalars and lists — updates land **cross-tab with no
  polling**, and the author writes no subscription code.
- Keyed rows: a write re-renders the changed row and leaves the untouched DOM
  nodes alone.
- **Dynamic topics** — a route parameter can name a topic, so `/room/[id]`,
  per-user data, and multi-tenancy work rather than being a compile error.
- Route-scoped subscription: a client subscribes to the route it is on, not to
  every topic in the app.

### Auth

- Session cookies, first-party password credentials (argon2), and CSRF
  protection on same-origin POST forms.
- Route gating by declared policy.
- `user` reaches components as a prop, and identity is a legal **partition
  key** — so `messages.where({ owner: user.id })` is row-level security that
  cannot be authored apart from the read, because the read *is* the policy.
- **The sign-in flow works with JavaScript disabled.**
- GCRA rate limiting on credential attempts.

### Calling the outside world

- **`await fetch(...)` works inside a handler or action body** — written as
  plain JavaScript, exactly as any vendor's sample code spells it. The compiler
  lowers the `await`; the server suspends the body, resolves the request, and
  replays it against a journal that answers.
- A declared `sources` block makes a remote endpoint a topic: it refreshes,
  paints like any other slot, and generates types into `.albedo/sources.d.ts`.
- An **egress policy enforced inside the DNS resolver**, so a declared allowlist
  cannot be walked around by DNS rebinding, by an IP-literal URL, or by a
  redirect (redirects are refused outright).
- Response caching, single-flight coalescing, conditional requests, derived
  idempotency keys, and a request deadline.

---

## What isn't done

- **Outbound `fetch()` works, but does not batch or survive a crash.** Three
  independent GETs in one handler cost three round trips — request hoisting
  isn't built. The journal is in-memory, so there is no crash recovery and no
  retry policy for a call that fails halfway.
- **No OAuth.** Password is the only first-party credential; the outbound half
  it needs now exists, but the provider flows are not written.
- **No file uploads.** Zero code in tree.
- **No passkeys.** Password is the only first-party credential today.
- **`forwardRef`, `createPortal`, and `useId` do not exist** — verified zero
  occurrences in the tree. Most React component libraries (shadcn/ui included)
  depend on at least one, so they will not work.
- **`useContext` server-renders the context *default*, not the Provider
  value.** A single SSR pass invokes `h` eagerly, so a nested Provider cannot
  thread its value down; the client applies it on hydration. It does not
  crash, but SSR output and post-hydration output differ.
- No production-ready guarantees. Pre-1.0, expect rough edges and breaking
  changes.
- The columnar wire format is designed and encoded but has no emitter yet.
- No published head-to-head benchmark against other frameworks. The numbers
  below are AlB'DO measured on one machine.

---

## Try it

```sh
albedo init my-app
```

```sh
cd my-app && albedo dev
```

`albedo build` produces a production build; `albedo serve` builds and serves
it; `albedo doctor` reports what the compiler decided and why.

---

## How rendering is decided

The compiler reads each component's effects at build time and picks how much,
if any, client JavaScript it needs.

```
Tier A   no hooks, no async, no side effects   →  plain HTML, zero JS
Tier B   event handlers, light interactivity   →  bindings only (~250–400 B)
Tier C   full hooks / async / side effects     →  full client hydration
```

The decision is a property of the code, not an annotation. Adding `useState`
to a Tier A component moves it to B or C at the next build; removing it moves
it back.

## The pipeline

```
  COMPILE  ·  albedo build / dev
  ────────────────────────────────────────────────────────────────────
   src/**.tsx
       │
       ▼
   scan ──► parse ──► analyse ──► transform ──► IR + wire encode
  scanner.rs  parser.rs  analysis/   transforms/      ir/
                            │
                            └── the tier decision (A / B / C)
                                and the escape analysis under it
       │
       ▼
   RenderManifestV2  +  islands  +  forge schema  +  .d.ts codegen
      manifest/          bundler/      forge/

  SERVE  ·  albedo serve / dev
  ────────────────────────────────────────────────────────────────────
   request
       │
       ▼
   route match ──► auth: session → Principal ──► policy gate
    routing/            auth/                    auth/declare.rs
       │
       ▼
   render ──┬── Tier A ──► pure-Rust evaluator      ──► HTML, no JS
            └── Tier B/C ─► QuickJS engine          ──► HTML + island
    runtime/                runtime/quickjs_engine.rs
       │
       │  a read mints a TOPIC; the topic is partitioned by the
       │  principal, and that IS the authorization
       ▼
   FORGE (libsql) ──► topic value ──► delta ──► live clients
     forge/                            broadcast/    PHOSPHOR lane
```

The load-bearing idea is in the last two boxes: **authorization is derived
from the read, never authored beside it.** An author cannot spell a topic name
directly, so there is no way to write a query whose policy disagrees with it.

---

## Measured numbers

One 16-core machine, release build. Reproduce with the commands in
[`benchmarks/parity/README.md`](./benchmarks/parity/README.md).

**Request latency** — a `GET /` SSR shell (28.8 KB, the scaffold's starter
page), served over the wire:

| Connection model | Concurrency | TTFB p50 | TTFB p99 |
|---|---|--:|--:|
| keep-alive, uncontended | 1 | 0.07 ms (70 µs) | 0.17 ms |
| keep-alive, steady | 8 | 0.13 ms | 0.30 ms |
| keep-alive, all cores | 16 | 0.23 ms | 0.53 ms |
| new connection per request | 1 | 0.36 ms | 0.54 ms |

Render and serve costs about **70 µs** over loopback when a connection is
reused. A fresh TCP connect per request adds ~0.3 ms (OS cost, the same for
anything). Per-request latency stays under a millisecond up to core saturation.

**Action round-trip** — a `POST /_albedo/action` (decode the bincode envelope →
run the handler → encode the opcode-frame response), measured over the wire
against a real `broadcast()` action:

| Connection model | Concurrency | TTFB p50 | TTFB p99 |
|---|---|--:|--:|
| keep-alive, uncontended | 1 | 0.24 ms | 0.43 ms |
| keep-alive, all cores | 16 | 0.45 ms | 1.34 ms |
| new connection per request | 1 | 0.50 ms | 1.21 ms |

**Cold process start** — spawn `albedo serve`, wait for the port, time the
first-ever hit: **~0.5 s** to ready (project stitch + artifact load, one Rust
process — no Node boot), then a **0.67 ms** first render that warms to 0.11 ms
within a few requests.

**Build time** — `albedo build` is ~**434 ms** clean for the 5-component
starter. The CLI build is from-scratch every run today (the incremental cache
is dev-watch only), so clean and re-run measure 1.0×.

**In-process cost** (no socket, Criterion):

| What | Time / size |
|---|--:|
| Server action dispatch (decode → run → encode) | ~13.6 µs |
| Static (Tier A) route — framework shell, no client JS | ~315 B |
| One interactive island (handler wrapper + bindings) | ~250–400 B |

(The 28.8 KB shell above is mostly the starter's own CSS; the framework itself
adds the ~315 B.)

These are loopback and micro-benchmarks, not a load test. They say what
AlB'DO's own overhead is — they don't simulate your network or your database.

---

## Where it's headed

Roughly in order:

1. **Compatibility** — `forwardRef`, `createPortal`, `useId`, and a Provider
   value that survives SSR, so existing component libraries work. This is the
   largest gap between "it runs my code" and "it runs the code I already have."
2. **Durability for outbound calls** — persisting the journal so a partially
   completed call survives a restart, and hoisting independent requests so they
   batch instead of serializing.
3. **OAuth**, on top of the outbound path that now exists.
4. **File uploads.**
5. **A real app, ported** — take an existing React/Next app across and write up
   the friction honestly.

---

## Repository layout

```
src/
  scanner.rs        project scan
  parser.rs         SWC JSX/TSX parser
  effects.rs        effect analysis → tier decision
  analysis/         classification, escape analysis
  transforms/       JSX rewrites, shared slots, actions
  ir/               canonical IR, opcode + wire format
  manifest/         build manifest + shell composition
  bundler/          classify → plan → rewrite → emit
  runtime/          render kernel, QuickJS engine, broadcast
  forge/            the database: declare, write, projection, drift
  auth/             principal, session, password, policy
  aperture/         outbound HTTP client, egress policy, cache
  routing/          route matching, dynamic segments
  doctor/           the reach matrix behind `albedo doctor`
  shutter/          GCRA rate limiting
crates/
  albedo-server/    axum + tokio HTTP runtime, the `albedo` binary
  albedo-node/      Node-API bridge (napi)
scaffold/           what `albedo init` writes
tests/              integration, conformance, and wire fixtures
fuzz/               cargo-fuzz targets for the wire decoders
```

## Documentation

- [`legend.md`](./legend.md) — a reviewer's map: the core idea, the end-to-end
  dataflow, and a file-by-file guide to what controls what. **Start here if you
  are reading the source.**
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — branching, the checks CI runs, and
  the pre-commit hook.
- [`SECURITY.md`](./SECURITY.md) — what is in scope and how to report privately.

Source files sometimes cite design documents under `development-plan/`. Those
are internal working notes and are not published; the code and `legend.md` are
the normative description.

---

## Credits

Built by [Bishal Sen](https://github.com/Sen-Bishal) — compiler, runtime, and
everything else in `src/`.
**Paushali Banerjee** — COO and co-founder; operations, and the reason any of
this reaches anyone.

Copyright © 2026 Albedo Technologies Private Limited.
Released under the [MIT License](./LICENSE.md).
