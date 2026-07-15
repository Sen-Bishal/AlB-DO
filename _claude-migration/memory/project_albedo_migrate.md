---
name: project_albedo_migrate
description: "`albedo migrate` — diagnostic-first one-way on-ramp from Next/React/Vite into ALBEDO; the adoption engine that attacks switching cost"
metadata:
  node_type: memory
  type: project
  originSessionId: af6f5f72-59c1-4b08-9509-667f25f28ddf
---

**`albedo migrate`** (brainstormed 2026-07-04, vision only, no code) — the adoption on-ramp,
aimed straight at the #1 business risk: **developer switching cost** (see [[project_strategy_gtm]]).
**Formalized as `development-plan/STRATEGY.md` Decision 4** (Stage 1 adoption lever), 2026-07-04.

**DECIDED (user, 2026-07-04):**
- **One-way on-ramp, NOT permanent Next-compat.** Get their app IN, then convert to *idiomatic*
  ALBEDO. Match familiar **basics only** (file routing, component/hook conventions, layout
  model); **never chase Vercel's full surface** — "semantic similarity to Next" is a treadmill
  Vercel controls (App Router/RSC/middleware/next.config churn on their schedule). Migration =
  finite work, not an infinite compat layer.
- **Diagnostic-first, built in Stage 1** (adoption), but AFTER the soul has one real number +
  FORGE Phase 0 works — need a real "why" to migrate toward, or you burn the one impression.

**Two phases:** (1) cut switching cost — make ALBEDO semantically similar to Next/React on the
basics, functionally superior underneath. (2) the `albedo migrate` tool.

**Tool shape:**
- Reads a Next/React/Vite/etc repo into ALBEDO's `ComponentGraph` → emits a **diagnostic
  report** (tier breakdown, dead-JS-shipped, clean-port / needs-your-decision / unsupported).
  **The report is a SALES WEAPON before it's a migration tool** — a dev runs it on their real
  app and sees the unignorable number ("80% Tier-A, shipping 900KB of hydration JS for nothing")
  WITHOUT porting a line.
- Then guided codemod-style transform on a **manual↔automatic slider** (dev reviews every
  transform; the jscodeshift/codemod trust model) → converts to idiomatic ALBEDO.
- **Data layer: DISSOLVE their backend (Prisma/API routes) INTO FORGE, don't mirror it** —
  mirroring imports their ceremony and kills FORGE's no-schema differentiation.

**Monetization:** free mechanical port + diagnostic; **Pro-tier CTRNI'TAS / graphify-grounded
AI does the hard semantic rewrites** ("this useEffect+SWR fetch → a FORGE reactive query") →
migration funnels INTO Equinox, not just a free giveaway.

**Engineering synergy (de-risks the build):** **graphify** already parses arbitrary codebases
into a knowledge graph → the diagnostic layer is largely graphify pointed at a Next repo, reusing
owned machinery instead of a from-scratch analyzer.

Slots into ENDGAME Stage 1 (adoption) alongside the deploy manifold. Cross:
[[project_endgame]], [[project_strategy_gtm]], [[project_backend_paradigm]] (FORGE/ALKMY/CTRNI'TAS).
