# Albedo

Albedo is a Rust-first renderer/compiler focused on deterministic output and low-overhead runtime rendering.

It combines:
- component graph analysis
- parallel topological planning
- tiered rendering (Tier A/B/C)
- static slices + QuickJS runtime execution
- a standalone server runtime and Node bridge

## Why Albedo

Albedo separates rendering paths by behavior instead of treating every component the same:
- Tier A: static-slice eligible, served without runtime engine work
- Tier B: deferred render path
- Tier C: on-demand/hydration-driven path
- Hot set: fast dirty signaling path for high-frequency components

## Repo Layout

- `src/` - compiler, analyzer, runtime, bundler, hydration
- `crates/albedo-server/` - standalone HTTP runtime server
- `crates/albedo-node/` - Node.js bridge (N-API)
- `tests/` - integration and runtime tests
- `Docs/Architecture.md` - architecture blueprint
- `Docs/Things_we_need_to_tackle#1.md` - current DX and implementation gaps

## Quick Start

```bash
cargo run --bin albedo -- help
cargo run --bin albedo -- init my-app
cargo run --bin albedo -- dev my-app/src/components --entry App.tsx
cargo run --bin albedo -- build my-app/src/components --entry App.tsx
cargo run --bin dom-compiler -- analyze test-app/src/components --verbose
cargo run --bin dom-compiler -- showcase test-app/src/components --entry App.jsx --serve --port 4173
cargo run --bin dom-compiler -- dev test-app/src/components --entry App.jsx --host 127.0.0.1 --port 3000 --print-contract
```

Run the server demo:

```bash
cargo run --manifest-path crates/albedo-server/Cargo.toml --bin albedo-server-demo
```

Run tests:

```bash
cargo test --lib
```

Run Phase 0 benchmark gates:

```bash
cargo run --bin albedo-bench -- --config benchmarks/workloads.json --baseline benchmarks/baseline.json --assert-gates --output target/benchmarks/latest.json
```

## Status

Core runtime architecture modules are in place (hot set, scheduler, inter-lane routing, 4-lane topology, and stream muxing), with additional developer-experience and syntax-compatibility work tracked in the docs.

## Developer Contract

The frozen `albedo run dev` contract (config schema, validation rules, and CLI flags) is documented in:

- `Docs/Developer_Contract.md`
