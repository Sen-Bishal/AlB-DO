# Albedo

**A Rust-native DOM render compiler and HTTP runtime for the next generation of web applications.**

Albedo is a full-stack compilation and execution engine built from the ground up in Rust. It statically analyses your component graph, produces deterministic bundle artifacts, and serves them through a high-throughput axum-based HTTP runtime — all without a Node.js process in the hot path.

> **Status: Pre-release.** The compiler pipeline and runtime kernel are stable. The developer surface and cross-platform Node bridge are under active development. See the [Roadmap](#roadmap) below.

---

## Architecture

```
┌─────────────────────────────────────────────────────┐
│              Developer surface (coming soon)         │
│   albedo dev · albedo.config.ts · HMR · overlays    │
├──────────────────────┬──────────────────────────────┤
│   CLI (dom-compiler) │   Node bridge (NAPI)          │
│  analyze · bundle    │   win32-x64 · macOS · Linux   │
│  showcase · serve    │                               │
├──────────────────────┴──────────────────────────────┤
│          HTTP server runtime (albedo-server)         │
│    axum · radix router · middleware · auth ·         │
│    layout · streaming                                │
├─────────────────────────────────────────────────────┤
│                   Runtime kernel                     │
│  Sentinel ring │ Scheduler │ π-arch │ WebTransport  │
├─────────────────────────────────────────────────────┤
│              Compiler pipeline                       │
│  Graph · Analyzer · Bundler · QuickJS · Incr. cache │
└─────────────────────────────────────────────────────┘
```

---

## What's Shipped

### Compiler Pipeline ✅

The full compile path is implemented and tested end-to-end.

| Module | Implementation | Notes |
|---|---|---|
| **Graph** | DashMap-backed component graph | Parallel read access, cycle detection |
| **Analyzer** | Parallel static analysis | Per-component weight, LCP/fold hints, interactivity scoring |
| **Bundler** | Deterministic artifact emitter | Classify → Plan → Rewrite → Emit, vendor splitting, static slices |
| **QuickJS engine** | SWC + IIFE transform + rquickjs | Isolated JS evaluation without Node |
| **Incremental cache** | File-hash based invalidation | Skips unchanged components across builds |

The pipeline produces a `RenderManifestV2`, a `BundlePlan`, and optionally a `CanonicalIR` document from a single `RenderCompiler` entry point.

### Runtime Kernel ✅ (core complete, stubs present)

| Component | Status | Notes |
|---|---|---|
| **Sentinel ring** | ✅ Done, tested | Priority-aware hot-set eviction for render slots |
| **Scheduler** | ✅ Done, tested | Overtake budget, lock-free analyzer/render queues |
| **π-arch lanes** | 🔧 Stub | Structured concurrency lanes — implementation pending |
| **WebTransport** | 🔧 Stub | Bidirectional streaming layer — implementation pending |

### HTTP Server Runtime ✅ (`albedo-server`)

Built on `axum 0.8` + `tokio`. Provides:
- Radix-tree router with typed route params
- Middleware stack (auth, layout injection, request tracing)
- Streaming HTML response support
- UUID-tagged request lifecycle

### CLI (`dom-compiler`) ✅

```
dom-compiler analyze   # Emit render manifest + optimization report
dom-compiler bundle    # Produce deterministic bundle artifacts
dom-compiler showcase  # Interactive component viewer
dom-compiler serve     # Launch albedo-server
```

### Node Bridge (`albedo-node`) ⚠️ Partial

NAPI bindings are working and packaged for **win32-x64** only. macOS and Linux builds are not yet included.

---

## Roadmap

These are the remaining gaps before a stable public release. Ordered by dependency and impact.

---

### 1. Cross-platform Node Bridge
**Priority: High**

The NAPI bindings currently ship only for `win32-x64-msvc`. Before release the bridge must be compiled and tested on:
- `darwin-x64` / `darwin-arm64` (macOS Intel + Apple Silicon)
- `linux-x64-gnu` / `linux-arm64-gnu`

This unblocks CI matrix testing and any user outside Windows.

---

### 2. π-arch Lanes (Full Implementation)
**Priority: High**

The `pi_arch.rs` module is a stub. π-arch lanes provide structured concurrency scheduling for the runtime kernel — parallel execution channels with deterministic ordering guarantees. Completing this enables true multi-lane rendering and is a prerequisite for the effect lattice.

---

### 3. WebTransport Layer
**Priority: Medium**

`webtransport.rs` is stubbed. This is the bidirectional streaming primitive that underpins live data push and incremental hydration at the transport level. Implementation depends on QUIC/HTTP3 integration within the axum server runtime.

---

### 4. Canonical IR + Effect Lattice
**Priority: High — blocks user-layer composability**

The IR document format (`ir.rs`) exists but the **effect lattice** is not built. The lattice provides a typed effect algebra over component boundaries:

- `Pure` — no side effects, fully cacheable
- `Hooks` — local reactive state
- `Async` — suspense-capable data loading
- `IO` — file system / network
- `SideEffects` — DOM mutations, external subscriptions

Without the lattice, the compiler cannot make precise caching and parallelism decisions. This replaces the current heuristic tiering system.

---

### 5. Developer Surface
**Priority: High — required for DX**

The developer experience layer is entirely absent. This includes:

| Feature | Description |
|---|---|
| `albedo dev` | File-watch rebuild loop with fast incremental recompile |
| `albedo.config.ts` | TypeScript-first project configuration (parser exists in `dev_contract.rs`, surface layer missing) |
| HMR | Hot module replacement — push updated bundles to the browser without full reload |
| Error overlays | Compiler and runtime errors surfaced as in-browser overlays |
| Tier feedback in terminal | Display which effect tier each component was assigned and why |

This is the last major gate before Albedo is usable as a day-to-day development tool.

---

## Getting Started

> Full installation docs are coming with the developer surface milestone. In the meantime:

```bash
# Build the CLI
cargo build --release --bin dom-compiler

# Analyse a project
./target/release/dom-compiler analyze ./my-app

# Bundle
./target/release/dom-compiler bundle ./my-app --out ./dist
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
  *.rs              # Compiler core: graph, analyzer, IR, incremental cache, types
tests/              # Integration test suite
benchmarks/         # Performance baseline + workload definitions
```

---

## License

TBD — will be specified at release.
