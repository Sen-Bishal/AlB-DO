# Next.js → ALBEDO: the port diff

*The friction story, told through **Halation** — a real editorial app (file routes, layouts,
dynamic `[slug]` routes, error/loading boundaries, `useState`/`useEffect`/`useContext` islands,
async data, `<title>`/metadata, and zod-validated server `action()` forms) ported to ALBEDO and
run on `albedo serve`. Everything below is taken from that app, not invented.*

The thesis in one line: **you keep React and delete the ceremony.** Same JSX, same hooks, same
file-router instincts — but no `"use client"` / `"use server"` directives, no bundler config, no
Node on the deploy path. A Rust compiler reads your components and decides what ships.

---

## The 60-second version

| You do this in Next (App Router) | You do this in ALBEDO | Verdict |
|---|---|---|
| `app/page.tsx` | `routes/index.tsx` | rename |
| `app/essays/[slug]/page.tsx` | `routes/essays/[slug].tsx` (a **file**, not a folder) | rename |
| `layout.tsx`, `loading.tsx`, `error.tsx` | same names, same semantics | ✅ unchanged |
| `export const metadata` / `generateMetadata()` | **identical** | ✅ unchanged |
| `async function Page()` + `await` data | **identical** | ✅ unchanged |
| `"use client"` on interactive components | **deleted** — the compiler infers it | ✅ better |
| `"use server"` action functions | `action()` imported from `"albedo"` | rewrite |
| `<form action={myAction}>` (fn ref) | `<form action="action:my_action">` (name sentinel) | rewrite |
| `useState` / `useEffect` / `useContext` | **identical** | ✅ unchanged |
| `next build` → Node/serverless output | `albedo build` → one Rust binary + static dist | different runtime |
| deploy to Vercel | deploy to Docker / Fly (Vercel can't run a Rust binary) | different target |

**Net:** most of the tree is copy-paste. The two things you actually rewrite are the
**directive removal** (a deletion, not a rewrite) and the **server-action shape**.

---

## What ports unchanged

### JSX + hooks — verbatim
`useState`, `useEffect`, `useContext`, `createContext`, `useRef`, `useMemo`, `useCallback` all
work. This is a real island from Halation, unedited:

```tsx
// components/ThemeControl.tsx — copied from a React app, runs as-is
import { useState, useEffect, useContext, createContext } from "react";
const ThemeContext = createContext("day");

export default function ThemeControl() {
  const [theme, setTheme] = useState("night");
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
  }, [theme]);
  return (
    <ThemeContext.Provider value={theme}>
      <button onClick={() => setTheme(theme === "night" ? "day" : "night")}>
        toggle mode
      </button>
    </ThemeContext.Provider>
  );
}
```

No `"use client"`. ALBEDO sees the hooks + the `onClick` and ships this as a hydrating island on
its own. On a static page it ships **zero** JavaScript.

### File routing — near-identical instincts
```
Next (app/)                         ALBEDO (routes/)
  app/page.tsx                        routes/index.tsx
  app/layout.tsx                      routes/layout.tsx
  app/essays/[slug]/page.tsx          routes/essays/[slug].tsx
  app/essays/layout.tsx               routes/essays/layout.tsx
  app/essays/loading.tsx              routes/essays/loading.tsx
  app/essays/error.tsx               routes/essays/error.tsx
```
Layouts compose root→leaf, `<children />` marks the slot (Next uses `{children}` as a prop; ALBEDO
uses a `<children />` element). Dynamic segments are `[slug]` and arrive as a `params` prop.

### Metadata — the same API
```tsx
// routes/index.tsx — static metadata, identical to Next
export const metadata = {
  title: "Halation № 001 — a journal of light & making",
  description: "The first issue: six pieces on tools that disappear…",
};

// routes/essays/[slug].tsx — dynamic metadata, identical to Next
export async function generateMetadata({ params }) {
  const essay = getEssay(params.slug);
  return { title: `${essay.title} — Halation`, description: essay.dek };
}
```

### Async server components + data — the same shape
```tsx
// routes/essays/[slug].tsx — an async server component, no client runtime shipped
export default async function Essay({ params }) {
  const essay = getEssay(params.slug);
  if (!essay) throw new Error(`No piece filed under “${params.slug}.”`); // → error.tsx boundary
  return (
    <article>
      <h1>{essay.title}</h1>
      <div className="essay-body">{essay.body.map((p) => <p>{p}</p>)}</div>
    </article>
  );
}
```
`throw` inside the component routes to the nearest `error.tsx` — same as Next.

### npm packages — real, bundled
`import { z } from "zod"`, `import clsx from "clsx"` — bundled at build time (no `node_modules` on
the server). zod runs inside server actions exactly as you'd expect.

---

## What actually changes (with real side-by-sides)

### 1. Directives disappear — the compiler tiers your components
This is the headline difference and it's a **subtraction**.

```tsx
// Next: you annotate the boundary
"use client";
export default function Counter() { const [n, setN] = useState(0); /* … */ }
```
```tsx
// ALBEDO: you don't. The compiler classifies:
export default function Counter() { const [n, setN] = useState(0); /* … */ }
```
`albedo build` prints the classification it inferred:
```
A  About            no hooks, no IO, no side effects        → zero JS, static
B  Issue            async boundary                          → settled HTML
C  ThemeControl     hooks + interaction boundary            → hydrated island
```
- **Tier A** — pure/static → HTML, no JS.
- **Tier B** — async/data → settled HTML, no client runtime.
- **Tier C** — hooks/events/effects → a hydrating island (the only JS that ships).

You stop *deciding* the boundary and start *reading* it. The upside: no mislabeled
`"use client"` accidentally pulling a subtree client-side. The cost: **there is no manual
override yet** — you can't force a boundary the classifier didn't pick (see friction §2).

### 2. Server actions: `action()` + a name sentinel
```tsx
// Next: "use server" fn + pass the fn to the form
async function subscribe(formData: FormData) {
  "use server";
  const email = formData.get("email");
  // …
}
export default function Footer() {
  return <form action={subscribe}><input name="email" /></form>;
}
```
```tsx
// ALBEDO: action() from "albedo", bound by NAME
import { action } from "albedo";
import { z } from "zod";

const SubscribeSchema = z.object({ email: z.string().email("Not an email.") });

export const subscribe = action(({ event }) => {
  const parsed = SubscribeSchema.safeParse(event ?? {});
  if (!parsed.success) return { error: { email: parsed.error.issues[0].message } };
  return { error: { status: "You're on the list." } };
});

export default function Footer() {
  return (
    <form action="action:subscribe">
      <input name="email" type="email" />
      <span data-albedo-error="email" />   {/* zod message lands here, in place */}
    </form>
  );
}
```
Three concrete differences:
- **Binding is by name.** `action="action:subscribe"` matches `export const subscribe`
  (`FNV-1a-32(name)` on both sides). No import of the function into the JSX.
- **Validation errors are declarative.** Return `{ error: { field: msg } }`; ALBEDO projects each
  message onto the `<span data-albedo-error="field">` the form already declared, **reconciled**
  against the form's field set (a re-submit clears stale messages). No `useActionState`, no manual
  error plumbing. The whole round-trip is a single `POST /_albedo/action` with **no page reload**.
- **The payload is `event`, not `FormData`.** The submitted fields arrive as a parsed JSON object
  bound to a free `event` in the body. ⚠️ The `({ event })` destructure reads nicely but is
  **cosmetic** today — see friction §3.

---

## The friction — the honest gaps a porter hits

This is the part that matters. None of these are hidden; all were found by actually building
Halation.

### 1. Form actions must be **server-rendered** (no forms inside client islands) — *hard limitation*
A `<form action="action:…">` and its `data-albedo-error` spans must be rendered by the **server**
so the compiler can stamp the form-action attribute and the field-error slot ids the validation
patch targets. Put the same form inside a Tier-C **client-rendered island** and neither stamp is
emitted client-side → the submit silently doesn't wire, or the error can't find its span.

**Workaround today:** keep form-bearing components server-rendered (Tier A/B). In Halation the
margin-note composer was demoted from a client island (it had a live char-count) to a plain
server form to make the action work. **This is a real capability gap** (client-island codegen for
form actions), not a config knob.

### 2. No manual tier override — *ergonomic gap*
There's no `"use client"` / `"use server"` escape hatch to force a boundary the classifier didn't
choose. 95% of the time inference is what you wanted; when it isn't, you refactor the component to
change what the classifier sees rather than annotate it. A future explicit override is the obvious
fix.

### 3. Action param doesn't truly bind — *rough edge*
`action(({ event }) => …)` works because the body reads a free `event` the runtime seeds; the
destructured parameter itself is dropped by the compiler. So `action(({ form }) => form.x)` would
**not** bind `form` — you must read `event`. It renders like the Next signature but isn't wired
like one. (Same root as an earlier event-arg gap; fixable, just not done.)

### 4. `createContext` Provider wants a single child element — *papercut*
A `<Context.Provider>` in a client island should wrap one element, not a multi-child fragment.
Wrap the children in a single `<div>`. (Halation does exactly this in `ThemeControl`.)

### 5. Deploy target is Docker / Fly, **not Vercel** — *architectural*
`albedo serve` is a long-running Rust HTTP server (QuickJS pool, streaming pipeline, action
dispatcher). Vercel's runtime doesn't execute a native binary, so `albedo ship --target vercel`
**errors on purpose**. Ship to Docker or Fly. A `--target static` export exists for CDN hosting,
but it's static-only — you lose the server surface (so server actions, streaming, and live
hydration don't function). ⚠️ *And note:* the docker/fly templates are correct-by-inspection but
have **not yet had a verified green deploy run** — that's still an open task.

### 6. No `node_modules` on the server — *mostly a feature, occasionally a wall*
Packages are bundled at build time. Pure-JS libs (zod, clsx) work. A package that needs Node
built-ins (`fs`, `net`, native addons) at runtime won't — there's no Node underneath.

---

## Scorecard

| Concern | Status |
|---|---|
| JSX, `useState/useEffect/useContext/useRef/useMemo/useCallback` | ✅ ports clean |
| File routing, layouts, `loading.tsx`/`error.tsx`, `[slug]` params | ✅ ports clean |
| `metadata` / `generateMetadata` | ✅ ports clean |
| Async server components + `throw`→error boundary | ✅ ports clean |
| Static-first output (zero-JS pages) | ✅ better than Next by default |
| `"use client"` / `"use server"` | 🔁 delete them (compiler infers) |
| Server actions + validation | 🔁 rewrite to `action()` + `data-albedo-error` (nicer errors, by-name binding) |
| npm packages | ✅ if pure-JS; ⚠️ no Node built-ins |
| Forms inside client-rendered islands | ⛔ not supported yet (server-render the form) |
| Manual tier override | ⛔ none yet |
| Deploy to Vercel | ⛔ Docker/Fly instead (static export only for CDN) |

**The honest summary:** a typical Next App-Router app's *pages, layouts, metadata, data fetching,
and hooks port with renames and deletions.* The real work is the **server-action rewrite** and
respecting the **server-rendered-form** constraint. In exchange you get automatic tiering,
zero-JS static pages, no bundler config, and a single self-contained binary with no Node on the
path.

---

*Worked example: [`A:\halation`](file:///A:/halation) · plan at `A:\halation\SPEC.md` · run with
`albedo serve`. Feature status tracked in [`TODO.md`](./TODO.md) (Gate 4).*
