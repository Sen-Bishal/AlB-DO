# AlB'DO

A JSX/TSX render compiler, database, and HTTP server, written in Rust. You
write React-shaped components; AlB'DO compiles and serves them as one binary,
with no Node.js in the request path.

The compiler decides *per component* how much client JavaScript it needs —
often none — and it does the same for your data: a collection you declare in
config becomes a persisted table, a live subscription, and a typed `.d.ts`,
without a server directory, an ORM, an API route, or any WebSocket code.

Everything below is implemented and exercised by the test suite. What is not
supported is listed under [Limitations](#limitations), and the build refuses a
configuration that would depend on it rather than shipping something that
silently does nothing.

---

## Install

Download the build for your platform from the
[releases page](https://github.com/Sen-Bishal/AlB-DO/releases):

| Platform            | File |
|---------------------|------|
| Windows x64         | `AlBDO-<version>-x64.msi` (per-user installer, adds `albedo` to `PATH`) or the `.zip` |
| macOS Intel         | `albedo-<version>-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `albedo-<version>-aarch64-apple-darwin.tar.gz` |
| Linux x64           | `albedo-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64         | `albedo-<version>-aarch64-unknown-linux-gnu.tar.gz` |

Every file has a `.sha256` beside it. The archives contain the `albedo` binary
and shell completions; put the binary on your `PATH`, then check it:

```sh
albedo --version
```

Shell completions: `albedo completions <bash|zsh|fish|powershell>`.

## Quick start

```sh
albedo init my-app
```

```sh
cd my-app && albedo dev
```

| Command | What it does |
|---|---|
| `albedo init <dir>` | Create a new app |
| `albedo dev` | Development server with hot reload and the error overlay |
| `albedo build` | Production build; fails on anything `serve` would refuse |
| `albedo serve` | Build and serve in production mode |
| `albedo ship --target <binary\|docker\|fly>` | Package the app for deployment |
| `albedo doctor` | Report what the compiler decided about each route, and why |
| `albedo budget` | Check the build against the tier byte budget |
| `albedo email test <address>` | Send one real email through the configured provider |
| `albedo files <dir>` | Serve a directory as static files |
| `albedo --version` | Print the version |

---

## What's included

### Compiler and rendering

- `.tsx`/`.jsx` parsed with SWC and rendered to HTML.
- **Three-tier classification, decided at build time, with nothing to
  configure** (see [How rendering is decided](#how-rendering-is-decided)).
- File-based routing, nested `layout.tsx` composition, and dynamic routes
  (`/post/[id]`).
- `error.tsx` / `loading.tsx` boundaries, and `async` server components.
- Global CSS and CSS modules, `<title>` and meta tags, `generateMetadata`.
- `useState` / `useEffect` / `useRef` / `useMemo` / `useCallback` /
  `useContext` / `useId`, `forwardRef`, `createPortal` and `cloneElement`.
  `useContext` resolves a Provider's value during the server render.
- Server `action()` handlers and SSR run under an embedded JS engine (QuickJS),
  so real JavaScript — loops, `try`, array methods — works, and a broken
  construct fails loudly instead of rendering null. Each engine's realm is
  rebuilt after any checkout on which third-party code ran.
- npm dependencies bundled at build time for SSR and actions; `node_modules` is
  not needed at runtime.

### FORGE — the database, on by default

- Declare a collection in `albedo.config.ts` and it exists: table and live
  topic. No migration folder, no ORM, no API route. Optional `seed` rows for
  reference data.
- Field types `text`, `int`, `real`, `bool`, `timestamp`, with a trailing `?`
  for nullable. A `bool` round-trips as a `bool`.
- `append` / `update` / `remove` from server actions and jobs.
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
  per-user data, and multi-tenancy work.
- Route-scoped subscription: a client subscribes to the route it is on, not to
  every topic in the app.

### Auth

- Session cookies (`__Host-`, `HttpOnly`, `SameSite=Lax`), rotated on sign-in,
  stored hashed. Sign out ends one session (`POST /_albedo/auth/logout`); sign
  out everywhere ends every session of the account (`POST
  /_albedo/auth/logout-all`).
- Providers: **password** (argon2id), **OAuth 2.0** (presets for GitHub, GitLab
  and Discord, or any server by URL), **OpenID Connect** (presets for Google,
  Microsoft, Apple and Twitch, or any issuer by discovery), and **magic link**.
- **Password reset** (`reset: "email"`) and **email confirmation**
  (`verify: "required"`). With confirmation on, sign-up does not reveal whether
  an address already has an account.
- Emailed links land on a page with a button, so a mail scanner that prefetches
  the link cannot spend it; tokens are single-use, stored hashed, and a newer
  link voids an older one.
- Route gating with `export const auth = "required"`; strangers are redirected
  to the declared sign-in route.
- `user` reaches components as a prop, and identity is a legal **partition
  key** — so `messages.where({ owner: user.id })` is row-level security that
  cannot be authored apart from the read, because the read *is* the policy.
- **The sign-in flow works with JavaScript disabled.** CSRF protection on every
  same-origin POST form.
- GCRA rate limiting per caller, per targeted account, and per mailbox.

### Email

- An `email` block with Resend, Postmark, SendGrid or Mailgun, or `log`, which
  writes each message to `.albedo/outbox/` as an `.eml` file for development.
- `sendEmail({ to, subject, text, html, cc, bcc, replyTo, attachments })` from
  any action or job. Messages are validated when queued — a bad address or a
  line break in a subject throws in the action — and delivered by the job
  queue with retries; a provider's permanent refusal is not retried.
- Attachments inline (base64) or by upload id; an upload can be attached only
  by the user who uploaded it.
- The framework's own emails (reset, confirm, sign-in link) can be replaced per
  field with `{{app}}`, `{{link}}` and `{{expires}}` placeholders.
- Links in mail are built from `ALBEDO_PUBLIC_ORIGIN` (or the ACME domain),
  never from a request's `Host` header.

### Background jobs

- `src/jobs.ts`: `export const nightly = job({ schedule: "@daily" }, fn)`.
  Five-field cron, `@aliases` and `every N` intervals; `enqueue()` from actions
  and jobs, with an optional id that makes it idempotent.
- A durable queue with leases, exponential backoff with jitter, and
  dead-lettering. A scheduled fire runs exactly once, even across a restart.
- A job runs **for** a principal, never as a system identity; `over: "users"`
  fans a schedule out into one run per user.

### Middleware and uploads

- `src/middleware.ts` (Next-shaped `matcher` + default export) can refuse,
  redirect or rewrite a request. It cannot grant one: the route's own gate runs
  after it.
- Declared `uploads` buckets with type and size limits. A handler receives a
  content-addressed reference, never the bytes.

### Calling the outside world

- **`await fetch(...)` works inside a handler, action or job body** — written as
  plain JavaScript. The server suspends the body, resolves the request, and
  replays it against a durable journal, so a completed call is not repeated.
- A declared `sources` block makes a remote endpoint a topic: it refreshes,
  paints like any other slot, and generates types into `.albedo/sources.d.ts`.
- An **egress policy enforced inside the DNS resolver**, so a declared allowlist
  cannot be walked around by DNS rebinding, by an IP-literal URL, or by a
  redirect (redirects are refused outright).
- Response caching, single-flight coalescing, conditional requests, derived
  idempotency keys, a per-host upstream budget, and a request deadline.

### Operations

- One binary. `albedo ship --target binary` produces a self-contained
  executable; `--target docker` and `--target fly` write the deployment files.
- HTTPS in-process from a certificate you provide, or from Let's Encrypt with
  `albedo serve --domain app.example.com` (TLS-ALPN-01, port 443).
- Graceful shutdown on `SIGTERM`/Ctrl-C: in-flight requests and live streams
  drain within a deadline.
- Prometheus metrics on a separate listener (`ALBEDO_METRICS_ADDR`).
- Correct client addresses behind a load balancer (`ALBEDO_TRUSTED_PROXIES`).

---

## Configuration

An app is configured by `albedo.config.ts` at its root; `albedo init` writes a
commented one. The top-level blocks:

| Block | Declares |
|---|---|
| `server` | Bind host and port |
| `forge` | Collections: fields, ordering, `partition_by`, optional `seed` |
| `auth` | Session lifetime and cookie, providers, and the sign-in route (`login`) |
| `email` | Sender, provider, API key (`{ env: "NAME" }`), templates |
| `uploads` | Buckets: accepted types and size limits |
| `sources` | External HTTP endpoints read as topics |

Secrets are declared as `{ env: "VARIABLE" }` and read at boot, never written
into the config file.

### Environment variables

| Variable | Purpose |
|---|---|
| `ALBEDO_SERVER_HOST`, `ALBEDO_SERVER_PORT` | Bind address; `--host` / `--port` override |
| `ALBEDO_PUBLIC_ORIGIN` | The origin browsers use (`https://app.example.com`). Required behind a proxy or in a container, and the base of every link in email |
| `ALBEDO_TLS_CERT`, `ALBEDO_TLS_KEY` | PEM certificate and key for in-process HTTPS |
| `ALBEDO_ACME_DOMAIN` | Comma-separated domains to obtain a Let's Encrypt certificate for (`--domain`) |
| `ALBEDO_ACME_CONTACT` | Let's Encrypt account contact (`--acme-contact`) |
| `ALBEDO_ACME_CACHE` | Where the ACME account and certificates are cached (`--acme-cache`) |
| `ALBEDO_ACME_STAGING` | Use the Let's Encrypt staging CA — rehearse with this first (`--acme-staging`) |
| `ALBEDO_FORGE_DB` | Path of the database file, e.g. on a mounted volume |
| `ALBEDO_TRUSTED_PROXIES` | Comma-separated addresses or CIDR ranges whose `X-Forwarded-For` is believed |
| `ALBEDO_INSTANCES` | Number of processes sharing app-wide rate budgets (default 1) |
| `ALBEDO_METRICS_ADDR` | Serve Prometheus metrics on this address (off when unset) |
| `ALBEDO_REQUEST_TIMEOUT_MS` | Per-request deadline (default 15000) |
| `ALBEDO_SHUTDOWN_TIMEOUT_MS` | Drain deadline on shutdown (default 5000) |
| `ALBEDO_REQUEST_TIMINGS` | `1` to print a timing line per request when stdout is not a terminal |
| `ALBEDO_SANDGATE_BYTECODE` | `0` disables the cached-bytecode fast path for npm packages |
| `ALBEDO_NO_TUI` | Disable the interactive terminal dashboard |
| `NO_COLOR` | Disable coloured output |
| `RUST_LOG` | Log filter for structured logs, e.g. `albedo=info` |

---

## Limitations

- **One server process per database.** Two processes serving the same
  `forge.db` would each hold their own live view of it, so a write through one
  is not pushed to clients of the other. Scale vertically; horizontal scaling is
  not supported.
- **Auth providers:** passkeys (WebAuthn), delegated JWT providers (the Clerk
  and Auth0 presets) and custom resolver modules are not supported. Declaring
  one fails the build.
- **Outbound calls are not batched.** Three independent `fetch()` calls in one
  handler cost three round trips.
- **Email** is sent through an HTTP provider; SMTP is not supported.
- No published head-to-head benchmark against other frameworks. The numbers
  below are AlB'DO measured on one machine.

---

## How rendering is decided

The compiler reads each component at build time and decides who owns its
state — and therefore how much, if any, component JavaScript ships.

```
Tier A   no state at all                           →  markup only, zero JS
Tier B   state owned by the server (shared,        →  markup only, zero component JS;
         persisted or server-computed)                updates arrive as opcodes
Tier C   state owned by the client alone           →  a compiled island
```

The decision is a property of the code, not an annotation. Adding `useState`
to a Tier A component moves it at the next build; removing it moves it back.

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

1. **Horizontal scaling** — more than one server process per database.
2. **Passkeys**, and delegated JWT providers (Clerk, Auth0).
3. **Batched outbound calls** — independent `fetch()` requests issued together
   instead of one after another.
4. **SMTP** as an email transport, beside the HTTP providers.

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
  auth/             principal, session, password, OAuth, mailed tokens
  email/            the `email` block, message validation, provider formats
  jobs/             job declarations, schedules, the durable queue
  middleware/       middleware declaration and matching
  upload/           upload buckets and storage
  aperture/         outbound HTTP client, egress policy, cache, journal
  routing/          route matching, dynamic segments
  doctor/           the reach matrix behind `albedo doctor`
  shutter/          GCRA rate limiting
crates/
  albedo-server/    axum + tokio HTTP runtime, the `albedo` binary
  albedo-node/      Node-API bridge (napi)
scaffold/           what `albedo init` writes
installer/          the Windows MSI source
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
