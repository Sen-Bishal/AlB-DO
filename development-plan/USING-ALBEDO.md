# Using AlBDO

A short guide. The full reference lives in [`README.md`](./README.md); the
in-project starter docs live under [`scaffold/README.md`](./scaffold/README.md).

## 1. Install the binary

After building from source:

```bash
cargo build --release --bin albedo -p albedo-server
# → A:\AlBDO-v-0.1.0\target\release\albedo.exe
```

Add the release directory to your user `PATH` so `albedo` is invokable from
anywhere (see [System setup](#system-setup) below for the PowerShell
one-liner on Windows).

Verify:

```bash
albedo help
```

## 2. Create a project

```bash
albedo init my-app
cd my-app
```

The scaffold produces a Phase N+ project layout:

```
my-app/
├── albedo.config.ts        — dev server + compile contract
├── tier-budget.toml        — per-tier ceilings (the build fails when exceeded)
├── public/                 — static assets served at root
├── src/
│   ├── routes/             — file-based routes
│   │   ├── layout.tsx      — wraps every route (Phase E.1 layout chain)
│   │   ├── index.tsx       — `/` landing
│   │   └── chat.tsx        — `/chat` broadcast + action() demo
│   ├── components/
│   │   ├── Hero.tsx        — Tier A (static)
│   │   └── Counter.tsx     — Tier B (hydrated)
│   ├── albedo-env.d.ts     — JSX + framework module types
│   └── styles.css          — auto-bundled
├── package.json
├── tsconfig.json
├── README.md
└── .gitignore
```

## 3. Three commands

```bash
albedo dev        # iterate — HMR + bakabox + /_albedo/action all live
albedo build      # compile manifest + bundle (gates on tier-budget.toml)
albedo serve      # build then boot a real AlbedoServer
```

The `serve` command is the production path: it boots a real axum-backed
`AlbedoServer` with manifest-streaming, `/_albedo/action` dispatch, broadcast
fan-out over the WT patches lane, and `/_albedo/runtime.js` serving — all from
the binary, no extra setup.

Use `--host <IP>` and `--port <PORT>` to override the defaults. `--no-budget`
opts out of the `tier-budget.toml` gate for one invocation. `--help` on any
command shows the supported flags.

## 4. Authoring conventions

### File-based routing

Drop a file under `src/routes/` and it becomes a URL:

| File                        | URL                       |
|-----------------------------|---------------------------|
| `routes/index.tsx`          | `/`                       |
| `routes/about.tsx`          | `/about`                  |
| `routes/blog/[slug].tsx`    | `/blog/:slug`             |
| `routes/docs/[...rest].tsx` | `/docs/*rest` (catch-all) |
| `routes/layout.tsx`         | wraps every route below   |
| `routes/error.tsx`          | renders on Tier-C failure |
| `routes/loading.tsx`        | renders while Tier-C resolves |

Files starting with `_` are skipped. Co-locate helpers under
`src/components/` and import them from your routes.

### Layouts

`routes/layout.tsx` wraps every route below it. The `<children />` JSX
intrinsic marks where the leaf route's HTML gets substituted at build time:

```tsx
export default function RootLayout() {
  return (
    <div className="app-shell">
      <nav>...</nav>
      <children />
      <footer>...</footer>
    </div>
  );
}
```

Nested `routes/blog/layout.tsx` would wrap every `/blog/*` route INSIDE the
root layout. Composition is root → leaf.

### State + actions

`useState` works the way it does in React for any single-component island:

```tsx
import { useState } from "react";
export default function Counter() {
  const [n, setN] = useState(0);
  return <button onClick={() => setN(n + 1)}>{n}</button>;
}
```

The framework compiles this to a Phase K hydration island. Bytes shipped:
~283 B (see [perf table](./README.md#performance)).

For server-pushed state across multiple tabs / sessions, use `useSharedSlot`
+ `action()`:

```tsx
import { action, useSharedSlot } from "albedo";

export const bump_counter = action(() =>
  broadcast("lobby:counter", (n) => n + 1),
);

export default function Lobby() {
  const counter = useSharedSlot("lobby:counter");
  return (
    <form action="action:bump_counter" method="POST">
      <button type="submit">+ bump</button>
      <span>{counter}</span>
    </form>
  );
}
```

Open `/chat` in two browser tabs; click bump in either; both update. The
action body runs server-side, `broadcast()` fans the write out over the WT
patches lane, every subscribed session paints the new value.

### CSS modules

Drop a `Card.module.css` next to a component and reference scoped classes via
`styles.foo`:

```tsx
import styles from "./Card.module.css";
export default function Card() {
  return <article className={styles.card}>...</article>;
}
```

```css
/* Card.module.css */
.card { padding: 1rem; background: #111; }
```

The build hashes the file path, rewrites `.card` to `.card_<hash>`, injects
the scoped CSS into the route's `<style>` block, and substitutes `styles.card`
references at render time. Two components with the same local `.card` name
get different scoped names — no collisions, no leakage.

### Tier budget

`tier-budget.toml` at the project root declares per-tier ceilings. The build
+ ship paths fail when any route exceeds them:

```toml
[defaults]
tier_a_max_components_per_route       = 50
tier_b_max_kb_per_route               = 8
tier_b_max_kb_per_component           = 4
tier_c_max_concurrent_fetches_per_route = 10
tier_b_bundle_max_kb_per_component    = 1   # post-emit measured-bytes gate
```

Per-route overrides via `[routes."/path"]` blocks. Run the standalone gate
with `albedo budget [--strict] [--format pretty|json]` — CI-friendly.

## 5. Deploy

```bash
albedo ship --target docker     # multi-stage Dockerfile + fly.toml
albedo ship --target fly        # same + a fly.io launch config
albedo ship --target static     # extract pre-rendered HTML for CDN
```

`albedo serve` is also a fully-supported deploy mode — just run the binary
behind your reverse proxy of choice.

## 6. Where to look next

- [`scaffold/README.md`](./scaffold/README.md) — what each starter file does.
- [`benchmarks/parity/README.md`](./benchmarks/parity/README.md) — how the perf
  numbers in the README's table are produced + how to refresh them.
- [`README.md`](./README.md) — pitch, full tier table, runtime architecture
  notes, roadmap.

## System setup

After `cargo build --release --bin albedo -p albedo-server`, add the release
dir to your user `PATH`. On Windows (PowerShell):

```powershell
$path = "A:\AlBDO-v-0.1.0\target\release"
[Environment]::SetEnvironmentVariable(
  "Path",
  ([Environment]::GetEnvironmentVariable("Path", "User") + ";" + $path),
  "User"
)
```

Then start a fresh shell. `albedo init my-app` from any directory works.

To replace an older install, locate it first:

```powershell
where.exe albedo            # all paths
Get-Command albedo          # the one that wins
cargo install --list        # cargo-installed binaries
```

Delete or rename the older binary; the new one in `target/release/` takes
over once it's first in `PATH`.
