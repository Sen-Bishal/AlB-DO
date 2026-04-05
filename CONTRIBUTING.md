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

Run the following from repository root before opening a pull request:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test
cargo check --release --bins
```

## Pre-commit Hook Setup (required for all contributors)

Install the repo's pre-commit hook once after cloning:

```bash
git config core.hooksPath .hooks
# On Unix/macOS:
chmod +x .hooks/pre-commit
```

The hook runs `cargo fmt --check`, `cargo clippy -D warnings`, and a fast `cargo nextest` pass
before every commit. No commit will go through if any of these fail.

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
- Report security issues privately to maintainers through your internal/private channel.

