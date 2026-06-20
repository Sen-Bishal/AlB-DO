# AlB'DO

A JSX/TSX render compiler and HTTP server, written in Rust. You write
React-shaped components; AlB'DO compiles and serves them as one binary,
with no Node.js in the request path.

It is a work in progress — a prototype I'm building in the open, not a
finished framework. The pieces below work today; the APIs will still
move.

---

## What works right now

- `.tsx`/`.jsx` components parsed with SWC and rendered to HTML.
- File-based routing with nested `layout.tsx` composition.
- `useState` / `useEffect` / `useRef` / `useMemo` islands that hydrate
  in the browser.
- Server `action()` handlers and SSR run under an embedded JS engine
  (QuickJS), so real JS — loops, `try`, array methods — works, and a
  broken construct fails loudly instead of rendering null.
- npm dependencies bundled in-tree for SSR + actions (tested against
  `zod` and `date-fns`).
- Global CSS and CSS modules, `<title>`/meta, and a dev server with
  hot reload over SSE.
- A "binding mode" path: for many stateful components the compiler ships
  a few hundred bytes of bindings instead of hydrating the whole
  component, and a click updates the bound node locally with no network
  round-trip.

## What isn't done

- No production-ready guarantees. Expect rough edges and breaking
  changes.
- Only the Windows binary is built today; other platforms compile but
  aren't packaged yet.
- Parts of the reactivity coverage (conditionals, keyed lists),
  `useContext`, dynamic metadata, and the columnar wire format are
  still in progress.
- No published head-to-head benchmark against other frameworks. The
  numbers below are AlB'DO measured on one machine; reproduce them and
  compare against your own setup.

---

## Try it

```sh
albedo init my-app
cd my-app
albedo dev        # dev server + hot reload
albedo build      # production build
albedo serve      # build, then serve it
```

---

## How rendering is decided

The compiler reads each component's effects at build time and picks how
much, if any, client JavaScript it needs. There's nothing to configure.

```
Tier A   no hooks, no async, no side effects   →  plain HTML, zero JS
Tier B   event handlers, light interactivity   →  only that island ships JS
Tier C   full hooks / async / side effects     →  full client hydration
```

On top of that, the "binding mode" path can take a stateful component
and ship just the state bindings — the server-rendered DOM stays put,
and the handler runs in the browser against those bindings. No VDOM, no
re-render, no request.

---

## Measured numbers

One 16-core machine, release build. Reproduce with the commands in
[`benchmarks/parity/README.md`](./benchmarks/parity/README.md).

**Request latency** — a `GET /` SSR shell (28.8 KB, the scaffold's
starter page), served over the wire:

| Connection model | Concurrency | TTFB p50 | TTFB p99 |
|---|---|--:|--:|
| keep-alive, uncontended | 1 | 0.07 ms (70 µs) | 0.17 ms |
| keep-alive, steady | 8 | 0.13 ms | 0.30 ms |
| keep-alive, all cores | 16 | 0.23 ms | 0.53 ms |
| new connection per request | 1 | 0.36 ms | 0.54 ms |

Render and serve costs about **70 µs** over loopback when a connection
is reused. A fresh TCP connect per request adds ~0.3 ms (that's OS
cost, the same for anything). Per-request latency stays under a
millisecond up to core saturation.

**In-process cost** (no socket, Criterion):

| What | Time / size |
|---|--:|
| Server action dispatch (decode → run → encode) | ~13.6 µs |
| Static (Tier A) route — framework shell, no client JS | ~315 B |
| One interactive island (handler wrapper + bindings) | ~250–400 B |

(The 28.8 KB shell above is mostly the starter's own CSS; the framework
itself adds the ~315 B.)

These are loopback and micro-benchmarks, not a full load test. They say
what AlB'DO's own overhead is — they don't simulate your network or
your database.

---

## Where it's headed

Roughly in order, while it's still under development:

1. **Honest perf, finished** — over-the-wire action latency, cold
   process start, and clean-vs-incremental build timing, alongside the
   request numbers above.
2. **Wider reactivity** — conditionals and keyed lists in binding mode,
   `useContext`, dynamic metadata.
3. **A real app, ported** — take an existing React/Next app across to
   AlB'DO and write up the friction honestly.
4. **Distribution** — cross-platform binaries so `albedo` installs and
   runs anywhere, not just Windows.

---

## Layout

```
src/
  effects.rs          effect analysis → tier decision
  parser.rs           SWC JSX/TSX parser
  manifest/           build manifest + shell composition
  bundler/            classify → plan → rewrite → emit
  runtime/            render kernel, scheduler, QuickJS engine
  dev/serve_bench.rs  serve-time latency harness
crates/
  albedo-server/      axum + tokio HTTP runtime, the `albedo` binary
  albedo-node/        cross-platform bindings
```

---

Built by [Sen-Bishal](https://github.com/Sen-Bishal) 
