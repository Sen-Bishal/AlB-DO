# Albedo

**A Rust-native DOM render compiler and HTTP runtime for the next generation of web applications.**

Albedo is a full-stack compilation and execution engine built from the ground up in Rust. It statically analyses your component graph, produces deterministic bundle artifacts, and serves them through a high-throughput axum-based HTTP runtime — all without a Node.js process in the hot path.

> **Status: Pre-release.** The full compiler pipeline, runtime kernel, developer surface, and HTTP server are implemented and tested. Two items remain before public release — see the [Roadmap](#roadmap) below.

---

## Architecture

```
┌─────────────────────────────────────────────────────┐
│      Developer surface - Under Development          │
│   albedo dev · albedo.config.ts · HMR · overlays    │
├──────────────────────┬──────────────────────────────┤
│   CLI (dom-compiler) │   Node bridge (NAPI)         │
│  analyze · bundle    │   win32-x64                  |
│  showcase · serve    │   macOS · Linux (pending)    │
├──────────────────────┴──────────────────────────────┤
│          HTTP server runtime (albedo-server)        │
│    axum · radix router · middleware · auth ·        │
│    layout · streaming                               │
├─────────────────────────────────────────────────────┤
│                   Runtime kernel                    │
│  Sentinel ring │ Scheduler │ π-arch │ WebTransport  │
├─────────────────────────────────────────────────────┤
│              Compiler pipeline                      │
│  Graph · Analyzer · Bundler · QuickJS · Incr. cache │
├─────────────────────────────────────────────────────┤
│         Canonical IR + effect lattice               │
│    Pure · Hooks · Async · IO · SideEffects          │
└─────────────────────────────────────────────────────┘
```

---

## What's Shipped

### Compiler Pipeline ✅

The full compile path is implemented and tested end-to-end.

| Module | Notes |
|---|---|
| **Graph** | DashMap-backed component graph with parallel read access and cycle detection |
| **Analyzer** | Parallel static analysis — per-component weight, LCP/fold hints, interactivity scoring |
| **Bundler** | Deterministic artifact emitter: Classify → Plan → Rewrite → Emit, vendor splitting, static slices |
| **QuickJS engine** | SWC + IIFE transform + rquickjs — isolated JS evaluation without Node |
| **Incremental cache** | File-hash based invalidation, skips unchanged components across builds |

The pipeline produces a `RenderManifestV2`, a `BundlePlan`, and optionally a `CanonicalIR` document from a single `RenderCompiler` entry point.

### Canonical IR + Effect Lattice ✅

`ir.rs` and `effects.rs` are fully implemented. Every component in the graph is assigned an `EffectProfile` across five typed effect kinds:

| Kind | Meaning |
|---|---|
| `Pure` | No side effects — fully cacheable and statically sliceable |
| `Hooks` | Local reactive state, hook-driven hydration |
| `Async` | Suspense-capable data loading, async boundary |
| `IO` | File system / network access |
| `SideEffects` | DOM mutations, external subscriptions |

The `decide_tier_and_hydration` function maps effect profiles to concrete `Tier` + `HydrationMode` decisions. This replaces heuristic tiering entirely.

### Runtime Kernel ✅

| Component | Notes |
|---|---|
| **Sentinel ring** | Priority-aware hot-set eviction for render slots — done, tested |
| **Scheduler** | Overtake budget, lock-free analyzer/render queues — done, tested |
| **π-arch lanes** | `PiArchKernel` with Lagrange scoring, `PiArchLayer` with dispatch/drain — done, tested |
| **WebTransport** | `WebTransportMuxer` with 4-stream muxing, sequence tracking, reassembly — done, tested |

### HTTP Server Runtime ✅ (`albedo-server`)

Built on `axum 0.8` + `tokio`. Provides:
- Radix-tree router with typed route params
- Middleware stack (auth, layout injection, request tracing)
- Streaming HTML response support
- UUID-tagged request lifecycle

### Developer Surface ✅

The full developer workflow is implemented in `albedo` CLI binary.

| Feature | Status |
|---|---|
| `albedo init` | Scaffold a new project with TypeScript/JavaScript starter, config, and theme |
| `albedo dev` | File-watch rebuild loop with incremental recompile |
| `albedo build` | Optimized production build into `.albedo/dist` |
| `albedo.config.ts` | TypeScript config parsing via SWC — `defineConfig` pattern supported |
| HMR | SSE-based hot module replacement — push reloads to browser without page refresh |
| Error overlays | Compiler and runtime errors surfaced as styled in-browser overlays with HMR reconnect |
| Dev headers | `x-albedo-render-ms`, `x-albedo-total-ms`, `x-albedo-dev-state` on every response |

### CLI (`dom-compiler`) ✅

```
dom-compiler analyze   # Emit render manifest + optimization report
dom-compiler bundle    # Produce deterministic bundle artifacts
dom-compiler showcase  # Interactive component viewer
dom-compiler serve     # Launch albedo-server
```

### Node Bridge (`albedo-node`) ⚠️ Partial

NAPI bindings are built and packaged for **win32-x64** only. macOS and Linux builds are not yet produced.

---

## Roadmap

Two items remain before a stable public release.

---

### 1. Cross-platform Node Bridge
**Priority: High**

The NAPI bindings currently ship only for `win32-x64-msvc`. Before release the bridge must be compiled and tested on:
- `darwin-x64` / `darwin-arm64` (macOS Intel + Apple Silicon)
- `linux-x64-gnu` / `linux-arm64-gnu`

This unblocks CI matrix testing and any user outside Windows.

---

### 2. Tier Feedback in Terminal
**Priority: Medium**

The effect lattice runs at compile time and produces tier + hydration mode decisions for every component (`Pure` / `Hooks` / `Async` / `IO` / `SideEffects`). These decisions are serialized into the manifest but are not currently surfaced to the developer's terminal during `albedo dev` or `albedo build`.

The goal is to display per-component tier assignments inline during the build so developers can see exactly why a component was placed in Tier A (static inline), Tier B (hook-driven hydration), or Tier C (split boundary) — and catch unexpected promotions early.

---

## Getting Started

```bash
# Initialize a new project
cargo run --bin albedo -- init my-app
cd my-app

# Start the dev server (file watch + HMR)
cargo run --bin albedo -- dev

# Production build
cargo run --bin albedo -- build
```

Or use the compiled binary directly after `cargo build --release`:

```bash
./target/release/albedo init my-app
./target/release/albedo dev my-app
./target/release/albedo build my-app
```

The Node bridge (for programmatic use from JavaScript) is available as a `.node` binary under `crates/albedo-node/` for Windows x64.

---

## Project Structure

```
crates/
  albedo-node/      # NAPI Node.js bindings
  albedo-server/    # axum HTTP server runtime
src/
  bin/              # CLI entrypoints (dom-compiler, albedo, albedo-bench)
  bundler/          # Classify → Plan → Rewrite → Emit pipeline
  hydration/        # Hydration payload + script generation
  manifest/         # RenderManifestV2 schema + builder
  runtime/          # Kernel: scheduler, sentinel ring, quickjs, π-arch, WebTransport
  effects.rs        # Effect lattice: EffectProfile, EffectKind, TieringDecision
  ir.rs             # Canonical IR document format + builder
  *.rs              # Compiler core: graph, analyzer, incremental cache, types
tests/              # Integration test suite
benchmarks/         # Performance baseline + workload definitions
```

---

## License

TBD — will be specified at release.
