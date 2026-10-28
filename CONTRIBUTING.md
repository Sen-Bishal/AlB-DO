# Contributing to Albedo

This repository is maintained with a release-first approach. Contributions are welcome when they are production-safe, tested, and aligned with the product direction.

## Ground Rules

- Keep user-facing behavior stable unless the change explicitly documents a breaking impact.
- Do not commit secrets, local credentials, machine-specific paths, or generated installer artifacts.
- Keep implementation details out of product-facing documentation unless maintainers request them.

## Branching and PR Flow

- Create a feature branch from `main`.
- Open a pull request targeting `main`.
- Keep pull requests focused on one logical change.
- Use clear titles: `area: short summary` (example: `runtime: tighten route cache invalidation`).

## Commit Quality

- Write commit messages in imperative mood (example: `Add cache guard for dev rebuild`).
- Prefer small, reviewable commits.
- Avoid mixing refactors with feature behavior changes in the same commit when possible.

## Local Validation Before PR

Run the following from repository root before opening a pull request. These are the
commands `.github/workflows/ci.yml` runs, in the same order.

> ⚠️ **Known: `fmt` and `clippy` do not currently pass on a clean checkout, even
> though CI reports green.** Verified 2026-08-17 — do not assume your change caused it.
>
> - **`fmt`** — `rustfmt.toml` uses three *unstable* options (`wrap_comments`,
>   `normalize_comments`, `comment_width`). Their output shifts between nightly
>   builds, so whether the tree is fmt-clean depends on which nightly you have.
>   **Do not mass-reformat.** Format only files you actually edited:
>   `rustfmt --config-path rustfmt.toml <file.rs>`. A global `cargo fmt` rewrites
>   ~160 files and buries your diff.
> - **`clippy`** — [`src/lib.rs`](./src/lib.rs) sets `#![warn(clippy::pedantic)]`,
>   and CI passes `-D warnings`, which promotes every pedantic lint to an error
>   (~3,674 of them). CI passes only because its cache makes the step a no-op.
>
> Both are tracked. Until they are resolved, treat `cargo test` as the real gate.

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

> **Windows note.** `cargo test --workspace` links several large test binaries at
> once and has been observed exhausting memory on 32 GB Windows machines. If it
> dies, run the crates separately (`cargo test -p dom-render-compiler`, then
> `cargo test -p albedo-server`) and cap parallelism with `-j8`. This is a local
> resource limit, not a broken tree — CI runs the workspace form on all three
> platforms.

### What ships, and what does not

A release contains the `albedo` binary alone (`cargo build --release -p
albedo-server --bin albedo`). Two things exist only for development:

- **Developer tools** — the benchmark harness `albedo-bench` and the analysis
  CLI `dom-compiler` build only with `--features dev-tools`.
- **Test doubles** — a recording substrate, a scripted HTTP transport, a
  hand-moved clock and a seeded fixture schema live behind the `test-support`
  feature. The crates' dev-dependencies turn it on, so every test and bench sees
  them and no release build contains them. Code outside `#[cfg(test)]` must not
  reach for them.

## Pre-commit Hook Setup (optional, recommended)

The repo ships a hook at [`.hooks/pre-commit`](./.hooks/pre-commit). Install it
once after cloning:

```bash
git config core.hooksPath .hooks
```

On Unix/macOS also make it executable:

```bash
chmod +x .hooks/pre-commit
```

It runs `cargo fmt --check` and `cargo clippy -D warnings` — the two checks that
fail most often — and skips the test suite deliberately, because a pre-commit
hook slow enough to be annoying just gets bypassed. Use `git commit --no-verify`
when you are deliberately committing work in progress.

## CI and Release Expectations

- A release is a tag. Bump `version` in the root `Cargo.toml`, then push
  `vX.Y.Z` with the same number — the workflow refuses a tag that disagrees.
- `.github/workflows/release.yml` builds `albedo` for Windows x64, macOS
  (Intel and Apple Silicon) and Linux (x64 and arm64), makes each binary report
  its own version before packaging it, builds the Windows MSI, and opens a
  **draft** release. Publishing the draft is a maintainer's decision.
- Rehearse a release without tagging: Actions → Release → Run workflow. Every
  target is built and packaged and the artifacts stay on the run; nothing is
  published.
- Nothing is published from `main`. Do not edit release assets by hand on
  GitHub; change the source or the workflow and cut a new tag.

## Documentation Policy

- Keep `README.md` and `LICENSE.md` accurate.
- For contributor-facing process updates, modify this file (`CONTRIBUTING.md`).
- Product docs should describe capabilities and usage, not internal architecture.

## Security Reporting

- Do not open public issues for security vulnerabilities.
- Report them through GitHub's private vulnerability reporting on this
  repository. See [`SECURITY.md`](./SECURITY.md) for what to include and what
  is in scope.

