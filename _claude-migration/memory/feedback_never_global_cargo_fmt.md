---
name: feedback-never-global-cargo-fmt
description: "Never run a bare `cargo fmt` in this repo — HEAD is NOT fmt-clean under its own rustfmt.toml, so a global fmt rewraps comments across the whole tree and pollutes the user's uncommitted prior work."
metadata: 
  node_type: memory
  type: feedback
  originSessionId: f8e4cbdf-8d46-4361-9bc0-59b02fb98d2e
---

# Never run a global `cargo fmt` in this repo

The repo's `rustfmt.toml` sets `wrap_comments = true, comment_width = 100, normalize_comments = true`, but the **committed code at HEAD wraps comments at ~70 chars** — i.e. HEAD has never been fmt-clean under its own config. So a bare `cargo fmt` is NOT a no-op: it rewraps comments to width 100 across ~100 files, including files you never touched and the user's large uncommitted prior work.

**Why this hurts:** the user owns commits and is exacting about clean, reviewable diffs ([[user_profile]], [[feedback_rewrite_weak_design]]). A global fmt buries a 3-file change under ~100 files of cosmetic churn, and rewraps prior-session uncommitted work that can't be cleanly un-fmt'd (no session-start snapshot in git → unrecoverable cosmetic damage; semantics are fine, tests still pass).

**How to apply:**
- Do NOT run `cargo fmt` / `cargo fmt --all`. CI runs fmt but apparently tolerates HEAD's state — don't "fix" it.
- Format only files you intentionally edited: `rustfmt --config-path rustfmt.toml <file.rs>` on those specific files, OR just hand-match the surrounding style (comments wrapped ~70 chars, 4-space indent).
- If you already ran a global fmt: `git checkout HEAD -- <files>` restores any file that was committed-clean at session start (removes the churn). Files with uncommitted prior work CANNOT be cleanly restored — preserve them, disclose the churn, never `git checkout` them (that destroys the user's work).
- Verify your footprint with `git diff HEAD --name-only` (note: the "LF will be replaced by CRLF" lines are stderr autocrlf noise, not real changes).

Incident: 2026-06-20 during A4 ([[project_a4_userland_boundary]]).
