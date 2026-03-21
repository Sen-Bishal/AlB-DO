# Changelog

All notable behavior-affecting changes to this project are documented in this file.

## [Unreleased] - 2026-02-17

### Added
- Canonical runtime error mapping with typed categories:
  - `InitError`
  - `LoadError`
  - `RenderError`
  - `PropsError`
- QuickJS module loader support for common export forms:
  - `export default function ...`
  - `export function Named ...`
  - `export const X = ...`
- Runtime negative-path coverage for missing dependency resolution, invalid JSON props, cyclic module graphs, and unsupported syntax fallback behavior.
- Runtime cold-vs-warm performance smoke coverage.
- Hydration manager module set under `src/hydration/`:
  - `plan.rs` for tier-driven island planning from `RenderManifestV2`.
  - `payload.rs` for versioned payload generation and checksum validation.
  - `script.rs` for inline payload/bootstrap script tag generation.
- Manifest-driven renderer hydration APIs:
  - `ServerRenderer::render_route_with_manifest_hydration`
  - `ServerRenderer::render_route_from_component_dir_with_manifest_hydration`
- Hydration integration tests for:
  - Tier A no-JS behavior.
  - Tier C trigger-gated hydration scheduling.
  - island failure isolation (one failed island does not block others).
- Step 6 bundler foundation under `src/bundler/`:
  - deterministic `BundlePlan` generation from manifest v2 with stable module ordering.
  - explicit rewrite actions for wrapper-module strategy and vendor-chunk linking.
  - plan emit helpers for JSON export and wrapper module source generation.
- Browser showcase CLI flow for JSX/TSX component directories:
  - `dom-compiler showcase <DIR> --entry <FILE>` renders a full HTML showcase document.
  - `--serve` hosts the rendered result on `http://127.0.0.1:<port>` for local browser preview.
- Showcase observability surface for engineering demos:
  - timed load pipeline stats (`scan`, `graph-build`, `optimize`, `render`, total).
  - graph summary (`components`, `dependencies`, `roots/leaves`, `critical path`, batches, degree peaks).
  - stable per-component dependency hashes for import graph fingerprints.
- Deterministic bundle artifact emission to disk:
  - `bundler::emit::emit_bundle_artifacts_to_dir` writes `bundle-plan.json` plus wrapper modules.
  - `RenderCompiler::{emit_bundle_artifacts_from_manifest_v2, emit_bundle_artifacts_to_dir}`.
- New CLI command:
  - `dom-compiler bundle <DIR> [--out <DIR>]` for scan -> manifest -> bundle-plan -> wrapper emit.
- Bundler integration coverage:
  - golden wrapper fixtures under `tests/fixtures/bundler`.
  - byte-for-byte reproducibility test across repeated emit runs.
- CI reproducibility gate:
  - `.github/workflows/ci.yml` runs the bundler byte-identity integration test on push/PR.
- New `albedo-server` crate scaffold under `crates/albedo-server`:
  - framework contracts (`config`, `contract`, `error`, `lifecycle`).
  - deterministic radix-tree routing with dynamic path params and method guards.
  - runtime server builder/dispatcher with middleware/auth interfaces and request timeout handling.
  - contract + routing + dispatch coverage tests.
- New `albedo-node` bridge scaffold under `crates/albedo-node`:
  - `napi-rs` exports for `analyzeProject`, `optimizeManifest`, and `getCacheStats`.
  - worker-thread execution via `AsyncTask` for CPU-heavy bridge calls.
  - panic-safe error mapping and TypeScript declaration scaffold (`index.d.ts`).

### Changed
- Runtime module resolution diagnostics now classify load failures as:
  - module missing
  - dependency cycle
  - invalid entry export
- `generated_at` contract is now explicitly documented as RFC3339 UTC for both schema v1 (`export_json`) and schema v2 (`export_manifest_v2_json`).
- README rewritten with a single canonical current-status section and only valid repo links.
- README current-status scope now reflects shipped hydration orchestration behavior and method-level API contracts.
