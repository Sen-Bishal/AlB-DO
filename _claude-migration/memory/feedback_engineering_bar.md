---
name: feedback_engineering_bar
description: "The bar for ALBEDO engine work — tech-lead judgment, tactical-not-bloated code, concept-reflecting novel design"
metadata: 
  node_type: memory
  type: feedback
  originSessionId: 98a94617-24ae-45da-b474-6bd93239835a
---

Standing directive for all ALBEDO engine work (adjustments, bug fixes, new capabilities), given 2026-07-02:

1. **Think like a technical lead / experienced senior dev** — weigh system-design consequences, not just "make it pass."
2. **Write tactical + strategic code, not bloated** — minimal surface, load-bearing lines only; no ceremony.
3. **The implementation must reflect the concept itself** — the method/logic should be genuinely novel, pragmatic, and give the software a *system-design edge*. Don't reach for the obvious/generic mechanism when a sharper one expresses the idea better.

**Why:** ALBEDO is the user's flagship ambition (see [[user_profile]]); its moat is design cleverness (tiering, binding-mode, QuickJS arena). Merely-correct code that doesn't carry an edge dilutes that. This is a higher bar than "works + tested."

**How to apply:** Before writing an engine change, ask "what's the sharpest expression of this concept that also earns its place in the system?" Prefer a mechanism that composes with existing moat pieces (opcodes, tiering, wire ids) over a bolt-on. Pairs with [[feedback_rewrite_weak_design]] (free hand to rewrite weak choices). Verify-gate rigor still applies ([[user_profile]]).
