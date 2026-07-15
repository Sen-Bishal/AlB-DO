---
name: user-profile
description: "Bishal — AlBDO project lead. Collaboration patterns, code style preferences, the verification rhythm we've settled into across sessions."
metadata: 
  node_type: memory
  type: user
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

The user is **Bishal Sen** (git user "Bishal", email bishalsenpersonal@gmail.com per `Cargo.toml`). One of the two project authors of AlBDO (the other being PixMusicaX). Pinaki Pritam Singha is a frequent reviewer/collaborator named in code comments. Frequently signs off internal review notes as "Bishal-albdo@may-2026" in comments.

## Engineering depth

- Deep Rust — comfortable with rayon split-borrow, SIMD via `wide`, atomic bitmaps, lock-free queues, core pinning, NAPI bindings, manual bincode versioning, SWC AST visitors.
- Designs in phases (Phase A through M now) with explicit wire-format freezing and per-phase invariants. Comments in committed code are essay-quality, not throwaway.
- Tracks performance to sub-ms granularity: 4-lane kernel, p99 budgets, FrameArena zero-alloc contract.
- Prefers single-binary, no-Node-in-hot-path deployment model.

## Collaboration patterns (observed across sessions)

**Planning style.** Plans come in via files (`Implemenatation-plan.md`, `C:\Users\bisha\.claude\plans\lets-make-nukes-sprint.md`). Per-session work uses the plan as the contract; the model proposes order/scope; user picks. Don't propose alternative sprint structures unless asked.

**Verification rhythm.** User pushes commits at end of session, asks the model to verify by reading `git log` + recent commit stat. Standard greeting after a push: "pushed code, verify" or similar. Expected response: confirm `0 0` ahead/behind, list the file delta with stats, flag anything that doesn't match the work-just-done.

**Commit ownership.** Model does NOT stage or commit. User stages and commits themselves. Standard instruction at task start: "do not commit or even stage any changes to git. just write the code locally." Honour this strictly.

**Comment style.** "Well-commented but without unnecessarily long comments." Module-level doc comments are welcome. Function-level intent comments are welcome. Multi-paragraph essays inside function bodies are NOT welcome. Match the existing code's tone — focused, pithy, intent-over-mechanics.

**Error-handling instruction (Phase M onward).** "If you find any error, silently continue without writing big descriptions in the chat." Translates to: fix the error inline if obvious (e.g. a stale fixture or unrelated drift caught by cargo check); skip the explanatory paragraph; only surface the fix in the final summary if it's structurally interesting.

**Verification gates.** Strong preference for objective programmatic gates (cargo test) over manual browser verification. When proposing demo work, prefer "write a Rust integration test that hits this path through the public router" over "scaffold a JSX page and ask you to open the browser". Both have value but the test is what locks the gate.

**Decision style.** Wants opinionated single-pick recommendations, not menus. "What's next" should yield "X first, because Y" with at most one alternative noted. Multi-bullet decision trees feel like waffling.

**Phase boundaries are big deals.** Crossing from one Phase to the next is treated as a meaningful gate. Pause at the boundary, summarise what landed, ask before proceeding. Don't roll Phase boundaries together.

## Session rhythm (5-hour cycles)

- Session opens with state-check ("what's next" or a specific plan reference)
- Mid-session: chunk work into a few coherent edits, run `cargo test --workspace` between batches if uncertainty is high
- Session closes with "pushed, verify" + a forward question or "see you tomorrow"
- Across sessions: 5-hour cooldowns typical (the user mentions them explicitly)

## Things to NOT do

- Don't propose `git stash`, `git reset --hard`, force pushes, or anything destructive unless explicitly asked.
- Don't add CLAUDE-Co-Authored-By footers to commit messages (user does their own commits).
- Don't auto-create scaffolds or example apps unless asked — the user knows when they want browser-verification.
- Don't write meta-narrative "I will now..." chatter. Get to the work or get to the recommendation.

## How to apply this profile

When implementing: follow the plan literally; don't redesign mid-sprint.
When discussing architecture: reference Phase letters and Cycle numbers explicitly. The memory files use them as anchors.
When verifying: re-grep against current source before claiming a primitive exists at a named line number. Several historical plan docs are stale; the source is the source of truth.
When recommending: pick one, justify in 2–3 lines, mention 1 alternative if relevant. No trees.

## Open ergonomic preferences (inferred, not explicit)

- Comments addressing future readers ("the renderer's output is deterministic so a literal `str::replace` is sufficient") are preferred over comments about implementation history ("we used to use a regex").
- Test names that read like assertions of behaviour (`form_submit_with_matching_csrf_token_dispatches_and_returns_navigate`) are preferred over numbered or terse names. The existing codebase follows this style.
- Where pragmatic, fail loud over silent degradation — see `WebTransportError::PayloadKindMismatch` (typed error rather than silent drop) and the action dispatcher returning 403 (not 200) on CSRF mismatch.
