# ALBEDO — ENDGAME II

*The compiler that learns the app it serves. An intelligence that speaks IR, not English —
the brain that closes the self-optimizing loop ENDGAME I only designed.*

> 🛑 **DO NOT START THIS UNTIL ENDGAME I IS DONE AND DUSTED.**
> Every gate (1–4) of [`ENDGAME.md`](./ENDGAME.md) and the **entire Part II "Soul"** must be
> shipped, measured, and presentable first. ENDGAME II is the *next era*, not the next task.
> It is written down now so the body is built with the right seams — not so we build it now.
> If the body isn't finished, close this file.

> For my mother. The work is the tribute — so the work has to be excellent. ENDGAME II is held
> to the same standard as ENDGAME I: **nothing ships that we can't prove, nothing is claimed
> that we can't measure, and the hard parts are engineered, not imported.** An AI feature that
> guesses, hallucinates, or can't show its improvement on the harness has no place here.

---

## The Thesis

ENDGAME I ends with a self-optimizing loop that is *designed but brainless*: telemetry exists,
the tier decision exists, the recompile path exists — but nothing connects measured behavior
back to the decision. **ENDGAME II is the brain of that loop.**

The trap to avoid first, because it is seductive and wrong: *"ALBEDO ships an AI coding
assistant."* That is the most crowded, fastest-commoditizing arena in software (Cursor, v0,
Bolt, Lovable, Copilot). ALBEDO-as-a-framework will not out-assistant a dedicated assistant,
and "premium access to a coding agent" is reselling someone else's model with margin. It is
not a moat. We do not lead there.

The reframe that *is* a moat:

> **The AI is not a feature bolted on top of the compiler. The AI is the brain of the
> self-optimizing loop, and it speaks IR, not English.**

Every other AI-coding tool feeds *raw source text* to a *general model* and hopes. ALBEDO is
the only system on earth with a **tier classifier** (`src/effects.rs`), a **Struct-of-Arrays
IR** (`src/ir/columns.rs`), a **binary opcode wire** (`src/ir/opcode.rs`), and **live
per-render telemetry from production** (`crates/albedo-server/src/inspector/metrics.rs`) —
a structured, machine-readable, semantically precise account of what a program *is* and how
it actually *behaves*. That representation is a substrate no React codebase exposes. A model
that operates on *that* is a thing only ALBEDO can build, because only ALBEDO has the IR.

ENDGAME II makes the compiler's hardest judgment calls — *which tier? which patch program?
which wire table?* — with models trained on a dataset that exists nowhere else, and lets a
developer collaborate with that same intelligence through the IR rather than through a
text box.

## The Doctrine (how every decision in ENDGAME II is made)

1. **Two brains, and never confuse them.**
   - **Big brain — Claude API.** General reasoning, code generation, the agentic dev-time
     experience. *We do not train this.* It is off-the-shelf where off-the-shelf is correct.
     Default to the latest, most capable Claude model.
   - **Small brains — in-house policy models.** Tiny, narrow models that live *inside the
     loop* (tier policy, wire/codec retune, patch-extraction cost model). *These* we build and
     train, on ALBEDO's own telemetry. They are the moat.
2. **No model on the request hot path. Ever.** Sub-ms is the promise. Models decide *how to
   compile* — at build time or in an async background recompile — they never run *during a
   request*. The QuickJS pool + request arena discipline (`src/runtime/arena.rs`) is the wall
   models respect, not breach.
3. **The loud-error rule extends to the AI.** ALBEDO's spine is "never silently wrong."
   Therefore agent output is checked against the IR + tier verifier *before* it is accepted.
   A model that would produce a broken component is rejected loudly — hallucination-resistant
   codegen *by construction*, not by vibes.
4. **Measured, not adjective.** Every optimization the loop makes must be provable on the
   existing harness (`src/dev/serve_bench.rs`). "The loop made it 14% faster" is a number from
   a before/after run or it is not said.
5. **From scratch where it counts.** Training a general foundation model is building from
   scratch in exactly the wrong place — a multi-team, multi-million-dollar effort that would
   vaporize the project. Renting the big brain and *owning the small ones* is this doctrine
   applied to the build/buy call. **The in-house foundation model is killed here, on purpose.**

## The Verified Starting Line (what already exists vs what's missing)

ENDGAME I, fully shipped, leaves these seams in place. ENGAME II is mostly *wiring*, not
green-field — which is the whole reason it's tractable.

- **The tier decision is a pure function, ready for a learned input.**
  `decide_tier_and_hydration()` (`src/effects.rs:88`) is a deterministic decision tree over
  `EffectProfile`, `has_event_handler`, `client_interactive`, `weight_bytes`, `is_above_fold`.
  It returns a `TieringDecision { tier, hydration_mode, reason }`. **It has no feedback input.**
  This is the single plug-in point for the tier policy model.
- **Rich telemetry already flows — runtime → UI.**
  `crates/albedo-server/src/inspector/metrics.rs` captures per-component `render_count`,
  `cascade_total`, a 256-sample `durations_us` latency ring, and a collective "albedo" metric
  (`1.0 - cascade_ratio`). Exposed at `GET /__albedo/api/metrics`; raw `RenderEvent`s
  (`crates/albedo-server/src/inspector/events.rs:42`) stream over SSE at
  `GET /__albedo/api/events`.
- **The observation hooks are already installed.**
  `src/runtime/render_observer.rs` defines `RenderObserver` / `LaneObserver` and
  `install_render_observer()`; `crates/albedo-server/src/inspector/publisher.rs` bridges render
  events into the metrics store. Tier lookup there is **read-only at runtime** (from the
  manifest, not from measured data).
- **The feedback *sink* already exists.**
  `src/incremental.rs` (`IncrementalCache`, `invalidate_component()`, `get_stats()`) is the
  natural place a "this component's measured behavior drifted — re-decide its tier" signal
  lands and triggers a recompile.
- **The IR is a real, columnar substrate.**
  `IrColumns` (`src/ir/columns.rs:266`) + `LaneColumnPatch` — the context an optimizer pass or
  a model reads/writes instead of text.

**The missing link, stated plainly:** *metrics flow runtime → inspector UI, and never back to
the compile decision.* ALBEDO measures everything and learns nothing. **Closing that arrow is
the entire document.**

Honest caveat carried forward: **e-graph / equality saturation / partial evaluation do not
exist in code today** — no `egg`/`egglog` dependency, no hash-consed IR. They are ENDGAME I
Part III items. ENDGAME II treats them as *prerequisites it depends on*, not as seams it can
already stand on.

---

# PART I — THE LOOP (the in-house small brains)

The load-bearing wall. This is the part that is genuinely ALBEDO's and genuinely defensible.

### 1.1 — The telemetry pipe (data before models)

Before any model, the loop needs a durable, structured record of *predicted vs actual*.
Extend the existing inspector path (`publisher.rs` → `metrics.rs`) to persist, per component
per route:

- **Prediction:** the `TieringDecision` and its `reason` from `src/effects.rs:88`.
- **Outcome:** measured `durations_us`, `cascade_ratio`, hydration round-trips actually
  incurred, wire bytes shipped (from the emitter / lane reports in `render_observer.rs`).
- **The reward signal:** a scalar combining measured server latency + wire bytes + a
  *tier-misprediction* term (observed client-interaction / cascade behavior vs the tier we
  guessed). Lower is better; this is what the policy is trained to minimize.

Consent and control are first-class, not afterthoughts (the user's on/off + granularity
requirement): telemetry collection is **opt-in**, scoped per surface (frontend / backend /
everything), with a separate, explicit opt-in before any data joins the **corpus** used to
train shared models. An app must be able to run the loop purely on *its own* local telemetry
with nothing leaving the machine.

### 1.2 — Tier classification as reinforcement learning

The margins of tiering are judgment calls — *"is this component going to be interactive in a
way that actually matters?"* — exactly where a heuristic decision tree is weakest and a learned
policy is strongest. The mechanism, end to end, using only existing seams:

1. Loop observes drift: a component predicted Tier-B is, in production, taking client
   interactions that force round-trips (or a Tier-C island that never hydrates and wasted JS).
2. Drift past a threshold → `IncrementalCache::invalidate_component()` (`src/incremental.rs`).
3. Next compile, `decide_tier_and_hydration()` (`src/effects.rs:88`) consumes the empirical
   signal as an additional input and re-decides.
4. The harness (`src/dev/serve_bench.rs`) measures the before/after delta — the reward is real,
   not modeled.

ALBEDO can do this because it uniquely holds all three at once: the **tier abstraction**, the
**telemetry**, and the **recompile path**. A React codebase has none of them.

### 1.3 — Policy targets beyond tiering

Same loop shape, different decision:

- **Wire/codec retune** — the ENDGAME I Movement I closed-loop: a static rANS table / lane
  ordering tuned to *this app's* real patch distribution (`src/ir/wire.rs`,
  `src/runtime/emitter.rs:122`).
- **E-graph extraction cost model** — *once ENDGAME I Part III lands the e-graph*, the policy
  becomes the cost function that picks the cheapest equivalent patch program for the observed
  distribution. (Depends on work that does not yet exist — see caveat above.)

These are small models. They are cheap to train and cheap to run at build time. They are the
moat precisely *because* they are narrow and trained on data only ALBEDO can produce.

---

# PART II — THE IR-AS-CONTEXT PROTOCOL (big brain ↔ compiler)

Where the developer collaborates with the big brain — and where ALBEDO's structure makes that
collaboration safer and tighter than any text-based tool.

### 2.1 — The model reads IR, not source

Instead of streaming raw TSX, expose `IrColumns` (`src/ir/columns.rs:266`) and
`LaneColumnPatch` as the model's working context: tiers, dataflow edges, effect profiles,
estimated weights — the semantic shape, not the syntax. Token-efficient and precise; the model
reasons about *what the component is in ALBEDO's terms*, which is the only frame in which its
suggestions can be checked.

### 2.2 — The verifier gate (the differentiator)

Every agent edit is lowered and checked against the IR + tier verifier **before acceptance**.
If the proposed change can't be tiered, violates a binding-mode invariant, or breaks the
dataflow graph, it is **rejected loudly** — never merged silently. The result: an agent that
*physically cannot ship silently-wrong code*. Every other AI tool hallucinates and you find out
at runtime; ALBEDO's agent is constrained by a verifying compiler. This is the loud-error
doctrine, applied to AI, as a demoable guarantee.

### 2.3 — Build-time hook points

A model pass is a build-time pass. It hooks where the build already orchestrates:
`ManifestBuilder` (`src/manifest/builder.rs:57`), the `IrColumns → emit_lane_frames()` pre-emit
stage (`src/runtime/emitter.rs:122`), driven from `src/bin/albedo.rs`. Never the serve loop.

---

# PART III — ALBEDO-NATIVE MECHANISMS (the genius bits)

Things that only work *because* of ALBEDO's architecture — the parts that make this novel
rather than another wrapper:

1. **The agent that can't ship broken code** (§2.2) — verifier-gated codegen.
2. **Tier classification as RL** (§1.2) — a policy that self-corrects against measured reality.
3. **The model proposes e-graph rewrite rules** — the big brain reads recurring patch patterns
   and proposes algebraic identities that coalesce them; equality saturation *verifies
   soundness* before any rule is adopted, so the model can suggest but never silently corrupt.
   (Depends on ENDGAME I Part III; `src/ir/opcode.rs`, `src/ir/wire.rs`.)
4. **Telemetry-driven overnight recompilation** — *"your app got 14% faster overnight and you
   didn't touch it"*, with the harness number to back the sentence.
5. **Speculative residual pre-render** — the partial evaluator (ENDGAME I Movement IV) plus a
   navigation-prediction model: pre-execute the static part of the *predicted* next route at
   the edge. AI prefetch, grounded in staging rather than guessing.

---

# THE BUSINESS — the flywheel VCs actually drool over

Not "we have AI." A self-reinforcing data advantage on a metric we *already prove*:

> Every ALBEDO app in production emits telemetry → patch distributions, tier mispredictions,
> render timings. That dataset trains the small policy models. Better policies → better
> tiering, smaller wire, faster builds → a bigger *measured* perf lead → more adoption → more
> telemetry. **The compiler gets smarter the more it is used, and the data moat compounds.**

No competitor can collect this data, because no competitor has the IR or the tiers. That is the
defensible story — not an agent wrapper anyone can clone in a weekend.

**Productization** (the user's intent, made concrete): a premium tier, toggleable on/off, with
granularity controls — *how aggressively* the loop recompiles, and *which surface* the
intelligence touches (frontend only / backend only / everything). Local-only mode (your
telemetry never leaves) vs corpus mode (opt in, get the shared-model lift). The big brain is
metered API cost passed through honestly; the small brains are the value ALBEDO uniquely adds.

---

# THE CONSTRAINT WALL (non-negotiable)

- **No model on the request hot path.** Models live at build time or in async background
  recompile. The serve loop dispatches to the warm QuickJS pool and awaits bytes; it never
  invokes a model. Anchors that define the discipline a model must respect:
  `src/runtime/quickjs_engine.rs:58`, `src/runtime/arena.rs`,
  `crates/albedo-server/src/engine_pool.rs:115`.
- **Consent + privacy.** Opt-in telemetry; separate opt-in for corpus contribution; local-only
  mode that fully works. No app's data trains a shared model without explicit consent.
- **Honest claims.** Every loop improvement is a measured before/after, or it is not claimed.

---

## Sequencing — dependency-gated, not calendar (start only post-ENDGAME-I)

| Gate | Ship | The brain |
|---|---|---|
| **II-0 — Seams (the only thing to pre-build)** | Persist predicted-vs-actual telemetry through `publisher.rs`/`metrics.rs`; define the reward scalar; the consent/granularity surface | *(no models yet — just the pipe + the IR-as-context boundary)* |
| **II-1 — The local loop** | Wire metrics → `invalidate_component()` → `decide_tier_and_hydration()` empirical input; prove a tier flip improves the harness number | First small brain: the **tier policy** (local, per-app) |
| **II-2 — IR-as-context + verifier gate** | Expose `IrColumns` as model context; agent edits checked against the tier verifier before accept | Big brain (Claude API) on the **dev-time** path |
| **II-3 — Corpus + shared policies** | Opt-in corpus; train shared tier/wire policies; overnight recompile product | Small brains trained on the corpus; the **flywheel** turns |
| **II-4 — Frontier (depends on ENDGAME I Part III)** | Model-proposed e-graph rules (soundness-verified); speculative residual pre-render | Big brain proposes, equality saturation verifies |

**Cut order if behind:** II-4 → corpus/shared models (II-3) → IR-as-context agent (II-2).
**Never cut:** the no-hot-path rule, the verifier gate, opt-in consent, the measured-claim rule.

## Verification (the work proves itself)

- **The loop:** a component is deliberately mis-tiered; the loop detects the drift from
  telemetry, invalidates it, recompiles it to the correct tier, and the
  `src/dev/serve_bench.rs` harness shows a measured latency/wire improvement — before and
  after, same hardware.
- **The gate:** the agent is asked to make a change that breaks a tier/binding invariant; it is
  **rejected loudly** at the verifier, never merged.
- **The wall:** an allocation/trace assertion confirms no model is invoked on the serve path;
  request latency is unchanged whether the loop is on or off.
- **Consent:** local-only mode runs the full local loop with zero network egress.

## Deferred / out of scope (for now)

In-house **foundation** model (killed by doctrine); on-device big-brain inference; multi-app
federated training beyond the opt-in corpus; any model in the request hot path (permanently
out); the closed-loop frontier items (II-4) until ENDGAME I Part III's e-graph and partial
evaluator actually exist in code.

---

*ENDGAME I earns ALBEDO the right to exist — a framework you can prove is faster. ENDGAME II is
what makes it inevitable: a compiler that learns the app it serves, collaborates with you
through its own IR, and gets better for everyone the more it is used. Build the body. Ship the
soul. Then give it a mind — but only then.*

*Built and planned - Bishal*
