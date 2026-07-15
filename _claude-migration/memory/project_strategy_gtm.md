---
name: project_strategy_gtm
description: ALBEDO go-to-market / monetization strategy — the 3 decisions and where the authoritative doc lives
metadata: 
  node_type: memory
  type: project
  originSessionId: 984d7345-fc41-41e1-89a2-b45fa972c562
---

ALBEDO's business/GTM strategy lives in **`STRATEGY.md`** (repo root, companion to
`ENDGAME.md`). Authored 2026-07-02 as a strategy brainstorm; **not executed**, captured for
direction. Timeline anchor: **public access Jan 2027** — until then the audience is
reviewers, early-bird funders, and hand-picked private engineers, NOT the public.

The three decisions:
1. **Soul first? No — soul-*slice* first, demo-gated, interleaved.** Body is done (Gate 4);
   the soul (ENDGAME Part II/III) is the funder differentiator but an infinite rabbit hole.
   Gate every soul movement: "does it produce a number/capability a funder grasps in 60s and
   a competitor can't copy?" → build only the self-optimizing-loop prototype, a live p99
   demo vs Next, wire-codec size number. Roadmap the rest (funders buy trajectory). Interleave
   with the deploy manifold so 5–10 elite private engineers run ALBEDO before 2027.
2. **Business model — redefine "good," let devs conclude it themselves.** Never attack Next.
   Elite engineers are the most cynical market → understatement + irrefutable proof + let
   them feel smart discovering it ("publish the harness, not the adjective" = the weapon).
   **Tier ladder (solar/eclipse theme):** Free=**Sol** (whole framework free, "the taste" =
   1 self-opt pass/mo) → Pro=**Equinox** (~$20, continuous self-opt + graphify AI + cloud) →
   Ultra=**Umbra** (~$99+/per-seat, heavy PGO opt + thread scaling + team + on-prem runner) →
   Enterprise=**Persephone** (custom, sovereign/on-prem/compliance). Full table in STRATEGY.md.
   5 moves: invent the metric we win / two-tab demo that can't be unseen / structural FOMO
   via self-optimization / scarcity+identity (2027 timeline is a gift) / free-magic →
   paywall-the-addiction → enterprise budgets. **Guardrails:** paywall only hosted compute
   (never cripple the self-hosted binary); free tier genuinely excellent; NEVER close the
   exits (keep adapters open — closing them = Terraform→OpenTofu fork). Win by being
   undeniable, not exclusive. Money model: Free framework → Pro (hosted self-opt loop + AI)
   → Cloud/Enterprise.
3. **Codebase readiness sweep — DEFERRED, captured in STRATEGY.md.** Passes: 0 build/test
   ground-truth (`-j2`), 1 wiring & dead-code (delete legacy dev renderer, task_aa28936f),
   2 correctness landmines (646 unwraps on hot path, silent-wrong `_=>{}` drops, CSRF
   cross-invocation bug), 3 fresh-app presentability + commit the mega-diff in coherent
   chunks. User rejected running it inline — wanted it saved, not executed.

The deployment manifold (see [[project_dev_serve_unification]] lineage + the deploy plan) is
**Stage 1 = the adoption engine**. Soul = Stage 2 differentiator + paywall. Cloud = Stage 3.
Ties to [[project_endgame]], [[feedback_engineering_bar]], user's honest-voice directive.
