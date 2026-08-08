# AlBDO starter

A JSX/TSX app compiled and served by [AlBDO](https://albdo.dev) — a
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
├── albedo.config.ts        — dev server + compile contract + THE BACKEND
├── tier-budget.toml        — per-tier ceilings; `albedo build` gates on this
├── public/
│   └── index.html          — static shell used by the file-server fallback
├── src/
│   ├── routes/             — file-based routes (Phase N)
│   │   ├── layout.tsx      — root layout wrapping every route below
│   │   ├── index.tsx       — `/`          landing
│   │   └── guestbook.tsx   — `/guestbook` live FORGE collection
│   ├── components/
│   │   ├── Hero.tsx        — Tier A · pure, static HTML
│   │   └── Counter.tsx     — Tier C · a client island
│   ├── albedo-env.d.ts     — ambient JSX + framework module types
│   └── styles.css          — hand-crafted stylesheet (auto-bundled)
└── tsconfig.json
```

## The three tiers

AlBDO sorts every component into exactly one tier. **The compiler decides —
you never mark a component with a directive.** All three are in the starter:

| Tier | Example          | What ships                                                  |
|------|------------------|-------------------------------------------------------------|
| A    | `Hero.tsx`       | markup only · zero JS                                        |
| B    | `guestbook.tsx`  | markup only · **zero component JS** — updates ride the wire  |
| C    | `Counter.tsx`    | a compiled client island — **the only tier that ships code** |

**The question the compiler is answering is who owns the state.**

`guestbook.tsx` holds its rows in a FORGE collection, so the *server* owns
them — every connected client has to see the same list. That state can never
live in one browser, so its updates travel as instructions over an open
connection and **no component code is sent at all**. That is Tier B, and it is
the tier most frameworks do not have: not static, not shipped.

`Counter.tsx` holds a number that nobody else will ever see. Nothing outside
that one tab cares, so pushing each click through the server would buy a
network round trip and nothing else. It gets a small island instead. That is
Tier C, and **it is the right answer, not a failure** — the island is there
because the state genuinely is local.

`Hero.tsx` holds no state at all. Tier A.

> Run `albedo build` to see the letter and the byte cost for every component in
> your project. **Trust that output over any comment, including this one** —
> the compiler is the only thing that actually decides.

## The backend is a config block

Open `albedo.config.ts` and look at `forge`. That block **is** the
backend — declare the shape of a collection and AlBDO emits the table,
the query that materializes it, and the seed rows, then keeps every
connected client in sync with it. There is no server directory, no ORM,
no API layer and no migration folder in this project because there is
nothing for them to do.

```ts
forge: {
  guestbook: {
    fields: { author: "text", message: "text" },
    seed: [{ author: "ada", message: "first light" }],
  },
},
```

A field is one of `text`, `int`, `real`, `bool` or `timestamp`, and a trailing
`?` makes it nullable — `nickname: "text?"`. The types are not decoration: they
decide what your rows look like in JavaScript. A `bool` reads back as `true`,
not `1`; a `timestamp` is epoch **milliseconds**, the same number `Date.now()`
gives you, so `new Date(row.created_at)` just works; a `text?` is `string | null`
rather than an absent key. `.albedo/forge.d.ts` is generated from this block, so
your editor knows all of it without you writing a type.

Editing the block after rows exist is safe. On startup ALBEDO compares what you
declared against the database, and **adds a new nullable field to the live
table** — one line on the console saying it did, and the rows you already had
read it as `null`. Every other edit refuses to start and names the field:
dropping one, renaming it, changing its type, or adding a *required* one, which
would mean inventing a value for every row written before it existed. A refusal
never touches your data; revert that field, or delete `forge.db` to rebuild from
the declaration.

Inside an `action()` body you get three free functions against it:
`append(collection, record)`, `update(collection, key, fields)` and
`remove(collection, key)`. A write is applied after the handler returns,
the collection is rematerialized, and the change fans out to every open
tab — see `src/routes/guestbook.tsx`.

Add a collection to that block and it exists. That is the whole workflow.

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

## Actions + live collections

Write a server-side handler inline with your route:

```tsx
import { action, useSharedSlot } from "albedo";

export const sign_guestbook = action(({ form }) =>
  append("guestbook", { author: form.author, message: form.message }),
);

export default function Guestbook() {
  const entries = useSharedSlot("guestbook");
  return (
    <>
      <ul>
        {entries.map((entry) => (
          <li key={entry.id}>
            {entry.author} — {entry.message}
          </li>
        ))}
      </ul>
      <form action="action:sign_guestbook" method="POST">
        <input name="author" />
        <input name="message" />
        <button type="submit">sign</button>
      </form>
    </>
  );
}
```

`useSharedSlot(topic)` reads the live collection and subscribes the
session's patches lane; a write inside an `action()` fans out to every
subscriber. Try the included `/guestbook` route in two browser tabs —
`albedo dev` and `albedo serve` both handle as many as you like.

> However many tabs you open, the browser holds **one** connection to the
> server. Tabs elect an owner (Web Locks), share its stream, and subscribe by
> route — the PHOSPHOR lane. Hot reload and the error overlay ride the same
> connection in dev, so a stack of tabs costs what one costs.

**Two rules worth knowing up front:**

- **Don't guard the `.map()`.** Writing `(entries || []).map(...)` stops
  the compiler seeing a bare slot identifier, which silently drops the
  reactive binding — you get a list that renders once and never updates.
- **Give every row a stable `key`.** That is what live reconciliation
  keys on, and it's why untouched rows keep their DOM nodes across a
  write (so focus, selection and scroll survive).

> **Note.** `useSharedSlot` topics are currently resolved at build time,
> so the topic argument must be a string literal — `useSharedSlot(\`room:${id}\`)`
> is not supported yet. Per-user and per-room collections are in progress.

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
