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

Run the following from repository root before opening a pull request. These are
**exactly** the commands `.github/workflows/ci.yml` runs, in the same order — if
they pass locally and fail in CI, that is a bug in this list, so please say so.

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

- Binaries are auto-published by the `Release Binaries (Main)` workflow from `main`.
- The release workflow publishes three platform archives: Linux, Windows, and macOS.
- Do not manually edit release assets on GitHub; update source/workflows and let automation publish.

## Documentation Policy

- Keep `README.md` and `LICENSE.md` accurate.
- For contributor-facing process updates, modify this file (`CONTRIBUTING.md`).
- Product docs should describe capabilities and usage, not internal architecture.

## Security Reporting

- Do not open public issues for security vulnerabilities.
- Report them through GitHub's private vulnerability reporting on this
  repository. See [`SECURITY.md`](./SECURITY.md) for what to include and what
  is in scope.

