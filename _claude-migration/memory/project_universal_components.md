---
name: project_universal_components
description: "The \"any component works natively\" north star — can AlBDO handle ShadCN / arbitrary custom / downloaded components; honest gap analysis + layered path. Deferred to a future sprint."
metadata: 
  node_type: memory
  type: project
  originSessionId: 12229ddf-da82-49d4-b0a9-0365bb32c249
---

**User's standing question (2026-06-09), deferred to a future sprint** (start after
the current A1/Gate-1 sprint, or when most meaningful): the original AlBDO vision
was that **any** component — native, hand-written-however-the-dev-likes, a ShadCN
component, or one downloaded from the internet — "just works" natively. Does it?

**Why:** this is the actual framework-vs-demo test. The user wants AlBDO to take a
component a developer wrote in *their own* style (no required template) and run it.

## Honest answer (A): NOT today, for arbitrary components.
AlBDO handles a specific, growing **subset** and degrades by *failing*, not
gracefully. Verified against the codebase this session:
- **Parsing is broad** (swc front end) — syntax is not the limit.
- **Arbitrary-JS render bodies** work *only for self-contained components* (my
  A1 slice 7 unlocked `.map`/`for`/`try`/etc. under QuickJS — see [[project_a1_bridge]]).
- **Two render engines, neither universal:** the pure-Rust interpreter
  (`runtime/eval/core.rs`, build-time + dev) models only a JS *subset* (e.g.
  literally rejects spread attrs `{...props}`, conditional hooks, non-trivial
  hook shapes); the QuickJS engine runs full JS but **cannot load a component
  that imports another component** — `import Card from "./Card"` rewrites to
  `__albedo_require(...)` which throws `MODULE_MISSING` at module-record load
  (only `react`/`react-dom`/`albedo` framework imports are special-cased to bind
  to globals; `__albedo_require` is defined only inside the render fn, not at load).
- **No npm dependency resolution (A2 unstarted):** ShadCN = Radix UI + `clsx` +
  `cva` + `tailwind-merge` + `lucide-react` + Tailwind; none of those imports
  resolve. This alone stops ShadCN.
- **Hooks are pattern-matched, not general:** only `useState(literal)` pinned to
  a `react` import + `useSharedSlot`; NO `useContext`/`useReducer`/`forwardRef`/
  `useImperativeHandle`/custom hooks (all of which Radix leans on).
- **No client runtime (A3):** even when SSR'd, a component is only interactive
  for the extracted `useState→slot` / `onClick→action` shapes — not "however the
  author wrote it."

## What to do about it (B): add a graceful-degradation tier under the fast path.
Current model = **compile-time extraction against recognized patterns** (what
buys the sub-ms wire). "Any component" needs a universal fallback layered under it:
- **Fast path** (today): recognized shapes → optimized slot/opcode wire.
- **Universal fallback:** everything else runs as *plain JS* — SSR via QuickJS +
  client hydration via a Preact-compatible runtime. The component "just works" as
  a normal React component; it simply forgoes the bespoke optimization.

The fallback is almost entirely the **sum of work already on the roadmap**, just
not framed as one goal (see [[project_endgame]]):
1. **A2** — npm dep bundling (`swc_bundler`/esbuild) → resolves ShadCN's dep tree.
2. **Render-engine unification** — QuickJS as the single non-Tier-A render path;
   fix **ES-import child-component loading** (make `__albedo_require` resolvable at
   module load, dependency-ordered) so composition works.
3. **A3** — Tier-C Preact-compat hydration → interactive client-side as authored.
4. **B** — `useEffect`/`useRef`/`useMemo`/`useContext` (+ general hooks).
5. **Emit hydration opcodes from a QuickJS render** (today opcodes only come from
   the pure-Rust AST walk; HTML-only QuickJS render loses bindings).

**Verdict:** achievable and aligned with existing gates, but multi-gate, not a
near-term flip. **ShadCN is the acceptance test** — it stresses every layer at
once (deps + composition + context + refs + Tailwind). Not a formal plan yet (per
user); revisit after the current sprint.
