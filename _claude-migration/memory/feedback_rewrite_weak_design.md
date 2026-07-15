---
name: feedback-rewrite-weak-design
description: "Standing directive (2026-06-01): when you find a weak design choice in EXISTING code, rewrite it pragmatically — don't preserve it for compatibility's sake. Free hand granted."
metadata: 
  node_type: memory
  type: feedback
  originSessionId: 24a2903a-ee78-4b71-bef1-b0fede58f11c
---

When I encounter a **weak design choice in existing code** while doing the work, I should
**rewrite it pragmatically** rather than working around it or sticking with what's already
there. The user explicitly granted a **free hand** here (2026-06-01).

**Why:** matches the ENDGAME aesthetic — "theoretically genius, engineer it from scratch"
over "preserve what exists." The user would rather I fix a bad foundation than build on it.
This is the honest-engineering-voice mandate applied to existing code, not just new code.

**How to apply:**
- Found-it-while-working → fix-it. If a primitive I'm touching is poorly designed, rewrite it
  cleanly instead of adapting my new code to its weakness. Don't ask permission for the
  rewrite itself; use judgment.
- Still keep the rewrite **pragmatic** (the word he used) — clean and correct, not a gold-
  plated rabbit hole. Prefer the simplest design that's actually right.
- Verify after rewriting (tests stay green); call out the rewrite + reasoning in the summary.
- **Boundary:** this is a free hand over *implementation/design of existing code I touch*. It
  does NOT override the plan/sprint structure or commit ownership — I still don't unilaterally
  re-architect the roadmap, and the user still owns commits (see [[user-profile]]). Big
  structural rewrites that ripple widely: do them, but flag the blast radius in the summary.
- Loud over silent: if the old design was silently-wrong, the rewrite should fail loud.
