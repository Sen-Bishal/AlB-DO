# AlBDO starter

A JSX/TSX app compiled and served by [AlBDO](https://github.com/anthropic-ai/albedo) — a
tiered DOM compiler written in Rust, with no Node.js on the runtime path.

## Getting started

```bash
albedo dev      # live dev server with HMR
albedo build    # compile for production
albedo ship     # package for deployment
```

## What&#39;s inside

```
.
├── albedo.config.ts    — dev server + compile contract
├── public/
│   └── index.html      — static shell used for prod builds
├── src/
│   ├── App.tsx         — entry component
│   ├── styles.css      — hand-crafted stylesheet (auto-bundled)
│   └── components/
│       ├── Hero.tsx    — Tier A · pure, static HTML
│       ├── Counter.tsx — Tier B · hydrated island
│       └── LiveFeed.tsx— Tier C · streamed async
└── tsconfig.json
```

## The three tiers

AlBDO sorts every component into exactly one tier based on its effect
profile — hooks, async, IO, side effects, and weight. The compiler picks
for you, but you can see each one at work in the starter:

| Tier | Example       | What ships                               |
|------|---------------|------------------------------------------|
| A    | `Hero.tsx`    | HTML only · zero JS, zero hydration      |
| B    | `Counter.tsx` | small hydration island, boots on idle    |
| C    | `LiveFeed.tsx`| streamed from the server as data lands   |

## Styling

This project uses a single hand-crafted stylesheet at `src/styles.css`.
AlBDO&#39;s dev pipeline concatenates every `.css` file under your project
root into one inline `<style>` block — no PostCSS, no Tailwind, no
bundler config. Drop in more `.css` files anywhere and they&#39;re picked
up automatically.

## Next steps

- Edit `src/App.tsx` and watch HMR re-render in milliseconds.
- Add a new component under `src/components/` and import it.
- Run `albedo build` and inspect `.albedo/dist/` to see the tiered output.
