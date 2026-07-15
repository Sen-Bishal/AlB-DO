---
name: project_gtm_content_calendar
description: Pre-reveal hype content calendar (25 Jul→15 Aug 2026) as an on-brand PDF; LinkedIn+Twitter only.
metadata: 
  node_type: memory
  type: project
  originSessionId: 5a9080b1-7079-48bf-bec2-1eba6971bac8
---

Built a pre-reveal **content calendar** to generate hype/traction for consumers, early adopters, and VCs. Deliverable: `A:\AlBDO-v-0.1.0\gtm\ALKMY_Pre-Reveal_Content_Calendar.pdf` (**v0.2, 12 plates, product-first**) + editable source `gtm\calendar.html`. Rendered from on-brand HTML via **headless Chrome print-to-pdf** (`chrome.exe --headless=new --no-pdf-header-footer --print-to-pdf`, A4, `print-color-adjust:exact` for the black bg). To re-render after edits: same command; page-count must stay 9 (more = a plate overflowed).

**Timeline (locked by user):** 25 Jul = hype begins → **15 Aug = the reveal** (drop real numbers + names + first frame) → **Sep end = beta release**. This supersedes the old "public Jan 2027" framing for the pre-reveal push.

**Decisions the user made (via structured Q):**
- **Channels = LinkedIn + Twitter ONLY.** He explicitly rejected HN/Reddit/Lobsters ("nope to reddit and shit"). I flagged the cost (those are the only merit-ranked, zero-audience reach unlock) — he accepted; reframed as fine for a *beta* (want ~hundreds of serious signups, not 40k tourists).
- **Founder-led, two voices:** he → X/Twitter (systems voice: heresy, proof, process); cofounder → LinkedIn (view-from-above: story, category, business).
- **Open-core:** engine + renderer public; everything else (incl. the FORGE self-tuning moat) private. He confirmed intent; must verify moat isn't in the public repo before it opens.
- **Reveal format:** he picked Supabase-style "Launch Week" earlier; this calendar culminates in a single big 15 Aug reveal day + road-to-beta (compatible — can expand the 15th into a week).
- Starting audience ≈ **zero** (300 LinkedIn each, 0 Twitter). A **content maker** exists (production+scheduling); founders supply substance/voice.

**The creative spine (all drawn from [[project_alkmy_brand_identity]]):** the governing paradox = **generate hype WITHOUT hype-cycle language** — the brand voice forbids disrupt/seamless/next-gen; be the quietest, most crafted voice in the feed. Everything passes the **one-frame test**. **Law: no numbers before 15 Aug.** Weeks themed to alchemy stages: Wk1 Nigredo/Blackening, Wk2 Albedo/Whitening, Wk3 Phosphor/Countdown → 15 Aug reveal.

**v0.2 revision (user redirect):** CUT the founder "View From Above" story entirely — *"don't let 'our story' cheapen the product."* Now **product-first**: 5 pillars = Heresy · Old World (enemy stack) · **FORGE** (backend proof / THE DROP) · **ALB'DO** (frontend/DX) · Void (aesthetic). Single objective = **placement in the web-framework arena** (Next/Astro/Remix). **Only ALB'DO + FORGE in play; CTRNI'TAS + RUB'DO HELD until after 15 Aug — no gold(#E8B33D)/oxblood(#C1272D) pre-reveal**, they appear at the reveal only as two locked stages. Every post now carries **COPY + DESIGN-brief blocks** (doc is designer-facing); added a **visual-system spec plate** (exact hexes, materials, aspect ratios). LinkedIn stays product/category (not personal). Pre-reveal palette = black+argent+glass+phosphor-glow+forge-ember+aberration only.

Ties to [[project_strategy_gtm]] and [[project_war_doctrine]] (THE DROP is the lighthouse demo). **Pending:** user wants a *separate strategy discussion*; I also offered to draft the full Week-1 posts as real copy.
