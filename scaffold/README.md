# AlBDO starter

A JSX/TSX app compiled and served by [AlBDO](https://github.com/anthropic-ai/albedo) — a
tiered DOM compiler written in Rust, with no Node.js on the runtime path.

## Getting started

```bash
albedo dev      # live dev server with HMR + bakabox + /_albedo/action
albedo build    # compile for production (gates on tier-budget.toml)
albedo serve    # boot a real AlbedoServer against the built artefacts
albedo budget   # standalone tier-budget gate (CI-friendly)
albedo ship     # package for deployment (docker / fly / static)
```

## What's inside

```
.
├── albedo.config.ts        — dev server + compile contract
├── tier-budget.toml        — per-tier ceilings; `albedo build` gates on this
├── public/
│   └── index.html          — static shell used by the file-server fallback
├── src/
│   ├── routes/             — file-based routes (Phase N)
│   │   ├── layout.tsx      — root layout wrapping every route below
│   │   ├── index.tsx       — `/`     landing
│   │   └── chat.tsx        — `/chat` broadcast demo (Phase O.2 + C)
│   ├── components/
│   │   ├── Hero.tsx        — Tier A · pure, static HTML
│   │   └── Counter.tsx     — Tier B · hydrated island
│   ├── albedo-env.d.ts     — ambient JSX + framework module types
│   └── styles.css          — hand-crafted stylesheet (auto-bundled)
└── tsconfig.json
```

## The three tiers

AlBDO sorts every component into exactly one tier based on its effect
profile — hooks, async, IO, side effects, and weight. The compiler picks
for you, but you can see each one at work in the starter:

| Tier | Example       | What ships                                          |
|------|---------------|-----------------------------------------------------|
| A    | `Hero.tsx`    | HTML only · zero JS, zero hydration                 |
| B    | `Counter.tsx` | small hydration island, boots on idle               |
| B    | `chat.tsx`    | `useSharedSlot` + `broadcast()` — server-push state |
| C    | (add one)     | streamed from the server as data lands              |

## File-based routing

Drop a `.tsx` file under `src/routes/` and AlBDO turns it into a URL:

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

## Actions + broadcast

Write a server-side handler inline with your route:

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

`useSharedSlot(topic)` reads the live topic value and subscribes the
session's WebTransport patches lane. `broadcast(topic, updater)` inside
an action fans the write out to every subscriber. Try the included
`/chat` route in two browser tabs.

## Styling

This project uses a single hand-crafted stylesheet at `src/styles.css`,
auto-bundled by AlBDO's dev pipeline. CSS modules work too — drop a
`Card.module.css` next to a component and reference scoped classes via
`styles.card`. The build injects scoped CSS into each route's `<style>`
block automatically.

## Next steps

- Edit `src/routes/index.tsx` and watch HMR re-render in milliseconds.
- Add a new route under `src/routes/` and link to it with `<Link href="/...">`.
- Tighten `tier-budget.toml` to lock down your performance budget.
- Run `albedo build && albedo serve` to see the production server boot.
