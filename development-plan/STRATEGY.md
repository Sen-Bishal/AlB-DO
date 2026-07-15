# ALBEDO — STRATEGY

*Companion to `ENDGAME.md`. ENDGAME is the engineering plan (the body + the soul).
This is the go-to-market, monetization, and readiness plan. For my mother — the work is
the tribute, so the strategy has to be as honest as the engineering.*

Timeline anchor: **public access Jan 2027.** Until then the audience is **reviewers,
early-bird funders, and hand-picked private engineers** — not the public. The whole
strategy is shaped by that: we are not selling to users yet, we are earning the right to
exist in front of people who decide whether ALBEDO gets funded and evangelized.

---

## Decision 1 — Soul first? No: **soul-slice first, demo-gated, interleaved.**

**Context.** The body (ENDGAME Part I) is essentially done (Gate 4 feature surface
complete). No users are waiting on features until 2027. The only pressure is impressing
funders/reviewers. The soul (ENDGAME Part II/III) is what differentiates — but it is an
infinite rabbit hole (Cranelift JIT, io_uring, e-graphs = research-lab timeline).

**The funder's only real question:** *"Why won't Vercel just crush this?"* The body cannot
answer it (feature-parity is commoditizable). Only the soul can (compiler-deep
optimization Vercel structurally cannot copy on Node).

**The gate for every soul movement:** *"Does this produce a number or capability a funder
grasps in 60 seconds and a competitor cannot copy?"* Build only what passes:
- **Self-optimizing loop prototype** (ENDGAME Part III #1) — "ALBEDO learns your app and
  ships a provably-faster build." This is the differentiated funding story, the AI/suite
  wedge, and the premium paywall, all in one artifact.
- **Live p99 tail-latency demo** that beats Next on identical hardware (enough of Movements
  I/II to be real, not the full research arc).
- **Wire-codec size number** as a supporting proof.

Everything else in Part II/III: **written down + prototyped just enough to show
trajectory.** Funders buy trajectory, not completeness.

**Non-negotiable:** interleave soul work with the deployment manifold so **5–10 elite
private engineers are actually running ALBEDO before 2027.** Their word-of-mouth outweighs
any deck. Never let the pleasure of from-scratch work substitute for a funder-visible proof.

---

## Decision 2 — The business model: redefine "good," let them conclude it themselves

**Core principle.** You do not beat Next by claiming to be better than Next. The
elite-engineer market is the most cynical audience alive — hype, astroturf, and
overclaiming are instant death. The psychological capture that works on smart people is
**understatement + irrefutable proof + letting them feel smart for discovering it.**
ENDGAME's doctrine — *"publish the harness, not the adjective"* — is the weapon. We
manufacture the conclusion; we never state it. Goal: make developers lose interest in
Next/Vercel **without ever attacking them.**

**The five moves.**

1. **Invent the metric we win by construction.** A public, honest number devs brag about
   (e.g. a "zero-JS ratio," a "self-optimization delta," a live leaderboard). When devs
   post *their app's* number, the definition of "good" has moved onto a field we own.
2. **The two-tab demo that can't be unseen.** Same app, ALBEDO vs Next, side by side, ours
   settles before theirs finishes. We never say "Next is slow." We show two tabs and stop.
   A conclusion they reach themselves is permanent; one they're told is resented.
3. **Structural FOMO via self-optimization.** "It gets faster while you sleep." A framework
   that improves your app on its own makes staying on Next feel like leaving free
   performance on the table every day. Standing still becomes falling behind — zero attack
   surface.
4. **Scarcity + identity (the 2027 timeline is a gift).** Invite-only early access. ALBEDO
   becomes a signal of taste and systems-sophistication. Flatter the smart. Next quietly
   becomes "the framework for people who don't care how it works" — implied by who's in the
   room, never said aloud.
5. **Free magic → paywall the addiction → developer love into enterprise budgets.** Free
   tier delivers a "how is this possible" moment. The paywall is the second hit — and it
   gates **our hosted compute, never the user's self-hosted binary** (see monetization
   guardrails). Captured devs bring their company's spend; bottom-up love becomes top-down
   contracts.

**Monetization guardrails (hard-won from the earlier session — do not violate).**
- **Paywall only what *we* run.** Gating capabilities of a binary on the user's own metal
  is crippleware: it destroys trust at the exact moment we need adoption, and it is forkable
  and circumventable. Gate hosted services (the optimization loop, hosted AI inference,
  cloud thread-scaling), not their install.
- **Free tier must be genuinely excellent, never deliberately hobbled.** The difference
  between "premium adds superpowers" (good) and "free is artificially crippled" (fatal).
- **Never close the exits.** The endgame is to *win* the hosting layer, not to "cut off
  Vercel." Removing deploy-anywhere after building adoption on openness is exactly what
  produced Terraform→OpenTofu, Redis→Valkey, the HashiCorp revolt. Keep the adapters open;
  make our cloud the obvious best home (co-designed runtime → cheaper + auto-optimal
  placement + the only place the self-optimizing loop runs). Win by being undeniable, not
  exclusive. The carrot, never the locked door.

**The money model, concretely — the tier ladder.**

Named as one continuous solar story: the sun (Sol) → the balance (Equinox) → the total
shadow (Umbra) → the sovereign of the shadow who crosses worlds (Persephone). Moving up-tier
is framed as *ascending into the eclipse* — initiation, not just a bigger invoice.

**Governing line (never violate):** Sol gives away the *entire* framework; every paid tier
sells only **compute that runs on our machines**. The paywall's job is not to gate the
framework — it is to monetize the addiction the free framework creates. Positioning weapon:
**flat, predictable, "no surprise bills"** pricing (the direct counter to Vercel's most-hated
trait). Cost edge: ALBEDO's Rust+QuickJS runtime is cheaper per request than Node-on-Lambda,
so the cloud tiers can undercut Vercel *and* keep margin.

| Tier (price) | Name | Meaning | Features | Who |
|---|---|---|---|---|
| **Free** ($0, OSS) | **Sol** | the sun — source of all light, given freely; *albedo* is the light Sol reflects | full framework (compiler, runtime, every tier/hook/action, honest harness); **deploy anywhere** (all adapters, unlimited, self-host no limits); **the taste** — 1 self-optimization pass/month on one project; community support | every developer — the adoption engine, never crippled |
| **Pro** (~$20/dev/mo) | **Equinox** | the balance point where light meets shadow — free framework meets hosted intelligence | Sol +: **continuous self-optimization** ("faster while you sleep"); **graphify-grounded AI**; **ALBEDO cloud** (cheapest-runtime, auto tier-placement, generous usage); semantic observability, preview deploys, analytics; no surprise bills | individual devs + indie/small teams |
| **Ultra** (~$99+/mo or per-seat) | **Umbra** | the total-shadow core of an eclipse — max depth, intelligence, scale | Equinox +: **priority heavy optimization** (full e-graph + rANS PGO, dedicated compute); **cloud thread-per-core scaling** (our cores, never their binary); **team** (seats, RBAC, shared insights); SLA + priority support; isolated/dedicated deploy; on-prem optimization runner | serious teams + companies — where the real revenue lives |
| **Enterprise** (custom) | **Persephone** | sovereign of the shadow who crosses worlds — rules the deepest tier *and* reaches into the customer's own | Umbra +: custom procurement/security/compliance; **sovereign/on-prem deployments**; data-sovereignty; dedicated isolation; volume pricing; white-glove support & custom contracts | large orgs needing custom terms, on-prem, or compliance |

**Still open (decide with real numbers, not now):** exact Equinox price ($20 vs $29, depends
how much cloud usage is bundled); Umbra per-seat vs flat-base (lean: Equinox $20 to match the
dev-tool anchor, Umbra per-seat to capture team expansion).

**FORGE across the ladder (the backend-less backend, priced separately, same rungs).** FORGE
is a separately-branded product (`development-plan/backend.md`) but rides the *identical*
Sol→Persephone rungs and obeys the same governing line — **local is always free; only hosted
intelligence is paid.** It carries its own SKU/pricing, so the "full ecosystem" (ALB'DO + FORGE
hosted) is the higher-priced path, but the split is brand/price only, never a padlock on the
binary.

| Tier | What FORGE adds (on top of the frontend) |
|---|---|
| **Sol** (free) | The **entire "no backend" magic on your own metal** — escape-analysis persistence, inferred schema, auto-migrations, content-addressed query synthesis, durable actions, embedded substrate **+ BYO-Postgres**. Deploy anywhere, never gated. (A *bigger* free-wow than frontend-only.) |
| **Equinox** (Pro) | **Hosted FORGE** — managed substrate + the **self-optimizing data loop** (CTRNI'TAS: "your database tunes itself while you sleep") + graphify-grounded AI over your data. |
| **Umbra** (Ultra) | Priority heavy data optimization (full IVM materialization + rANS-trained data-delta wire on dedicated compute), cross-region sync, team RBAC over shared data insights. |
| **Persephone** (Enterprise) | Sovereign / on-prem managed data, data-sovereignty, compliance, dedicated isolation. |

FORGE's premium sits on the **same paywall axis** as the framework's self-optimizing soul
(Equinox) — not a new monetization surface, an extension of the one already sanctioned above.

**The staged arc.**
1. **Stage 1 — Ubiquity (now):** ship the deployment manifold (see the deployment plan).
   Deploy-anywhere, excellent free framework. No paywall. Pure adoption.
2. **Stage 2 — Soul as a drip + first paywall:** ship demo-gated soul continuously.
   Monetize the *hosted* ones (optimization loop, AI, cloud scaling). Free stays excellent.
3. **Stage 3 — The home, not the cage:** the ALBEDO cloud is the best/cheapest place to run
   ALBEDO. Adapters stay open. We capture the Vercel layer by being undeniable.

---

## Decision 3 — Codebase readiness sweep (DEFERRED — run as its own focused session)

Goal: make the codebase showable to funders and early-adopting private engineers. A sharp
reviewer who clones the repo must find something coherent, wired, and honest — not a
half-refactored mega-diff. **Do NOT run inline; execute deliberately when greenlit.**

**Known starting facts (from memory + prior exploration):**
- One giant uncommitted diff spanning compiler/runtime/server/CLI. A reviewer cloning HEAD
  sees the *old* state — the risk is not "is it done," it's "is it committed coherently."
- A legacy dev renderer is marked `#[allow(dead_code)]` pending deletion (task_aa28936f).
- ENDGAME's own D-list: ~646 `unwrap()`/`panic!` (prioritize the serve/parse/decode hot
  path); silent-wrong evaluator paths (`_ => {}` drops in `eval_body_stmts`).
- The CSRF cross-invocation bug found during the deployment planning (in-memory `DashMap`;
  breaks across serverless — see the deployment plan's stateless-CSRF item).
- `graphify-out/` cache artifacts are tracked in git (review whether that noise belongs).

**The passes (cheapest/highest-signal first):**
- **Pass 0 — Ground truth.** `taskkill /F /IM albedo.exe` first (binary held open → relink
  fails). `cargo build -p albedo-server --bin albedo`; workspace tests with **`-j2`**
  (Windows OOMs otherwise). Does it build clean? Tests green? Record the real numbers.
- **Pass 1 — Wiring & dead code.** Verify every "done + live-verified" memory claim is
  actually wired on the *serve* path, not just test-covered. Delete the legacy dev renderer.
  Audit half-wired features. Confirm the deploy adapter seam is clean.
- **Pass 2 — Correctness landmines.** The stuff a cynical engineer greps for first:
  `.unwrap()`/`panic!` on the request path → `catch_unwind`/`Result`; silent-wrong drops →
  loud errors (ENDGAME's rule); the CSRF fix; wire decode robustness.
- **Pass 3 — Presentability.** `albedo init` → build → serve → deploy works end-to-end on a
  *fresh* app (first impression for a private engineer). README honest to measured numbers.
  Decide on `graphify-out/` tracking. Then **commit the mega-diff in coherent, reviewable
  chunks** — this is the single biggest presentability win.

**Workflow reminders:** `taskkill /F /IM albedo.exe` before any build; `-j2` on workspace
tests; `assets/*.js` are `include_str!`'d → rebuild binary after editing them; verify apps
via direct `albedo serve` + curl (preview MCP can serve a stale binary); **user owns
commits — never stage/commit without being asked.** After code changes: `graphify update .`.

---

## Decision 4 — Cut the switching cost: semantic familiarity + `albedo migrate` (Stage 1 adoption lever)

**Context.** TAM is not the constraint (the developer cloud is a $30B+ market growing double
digits); **incumbent stickiness is.** React/Next/Vercel is a default, and elite engineers do
not switch stacks on features — they switch on an unignorable number *plus* a painless path in.
Decision 1's two-tab demo supplies the number; this decision supplies the path. Two levers,
sequenced.

**Lever 1 — Semantic familiarity (the compat *basics*, deliberately shallow).** Make ALBEDO
feel like home to a Next/React/Vite dev on the basics only — file-based routing, component/hook
conventions, the layout mental model — while being functionally superior underneath.
**Explicitly NOT full Next parity.** "Semantically similar to Next" taken to its end is a
compatibility treadmill Vercel controls (App Router / RSC / middleware / `next.config` churn on
*their* schedule). We match the stable, shallow surface; we never chase the deep, churny one.
Familiarity is skin-deep by design; the superiority is bone-deep.

**Lever 2 — `albedo migrate`: a diagnostic-first, one-way on-ramp.** Reads an existing
Next/React/Vite project into ALBEDO's `ComponentGraph`, then:
- **Emits a diagnostic report first.** Tier breakdown, dead-JS-shipped, clean-port /
  needs-your-decision / unsupported. **This report is a sales weapon before it is a migration
  tool:** a dev runs it on their *own* real app and sees the number they can't unsee ("80% of
  this app is Tier-A static; you ship 900KB of hydration JS for nothing") *without porting a
  line.* This is Decision 2's doctrine — publish the harness, not the adjective — with the
  harness running on **their** code.
- **Then guided transform** on a manual↔automatic slider: the dev reviews every transform (the
  codemod / jscodeshift trust model), as hands-off or hands-on as they want, converting to
  *idiomatic* ALBEDO. A door **in**, not a permanent Next-shaped shim.
- **The data layer dissolves into FORGE — it does not mirror.** Their Prisma schema + API
  routes become FORGE's inferred persistence; mirroring them 1:1 would import their ceremony and
  throw away the differentiation.

**Monetization tie-in (funnels into the paywall, doesn't just give away).** Free tier does the
mechanical port + the diagnostic. The *hard* semantic rewrites — "this `useEffect` + SWR fetch
should become a FORGE reactive query; here's the rewrite" — are where **CTRNI'TAS /
graphify-grounded AI** earns its keep. Migration becomes an on-ramp *into* Equinox, consistent
with the guardrail (sell the hosted intelligence, never gate the local binary).

**Engineering leverage (de-risks the build).** `graphify` already parses arbitrary codebases
into a knowledge graph → the diagnostic layer is largely graphify pointed at a foreign repo:
owned machinery reused, not a from-scratch analyzer.

**Sequencing.** Stage 1 (adoption), alongside the deployment manifold — but built *after* the
soul has one honest number and FORGE Phase 0 works. Migrating a dev onto an ALBEDO that isn't
yet obviously better spends the one impression you get. Prove the *why*, then pave the road to it.

---

## How this connects to the deployment plan

The deployment manifold (ship to Vercel + every major host via an adapter architecture on
a spine of host-neutral render entry + stateless CSRF + explicit route classification) is
**Stage 1 of this strategy** — the adoption engine. It is not a detour; it is the thing
that removes the #1 adoption blocker and gets ALBEDO into elite private engineers' hands
before 2027. The soul is Stage 2's differentiator and paywall. The cloud is Stage 3.
