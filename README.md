<div align="center">

<br />

```
 █████╗ ██╗██████╗ ██████╗  ██████╗
██╔══██╗██║██╔══██╗██╔══██╗██╔═══██╗
███████║██║██████╔╝██║  ██║██║   ██║
██╔══██║██║██╔══██╗██║  ██║██║   ██║
██║  ██║██║██████╔╝██████╔╝╚██████╔╝
╚═╝  ╚═╝╚═╝╚═════╝ ╚═════╝  ╚═════╝
```

**A Rust-native DOM render compiler and HTTP runtime for JSX/TSX.**  
Zero Node.js in the hot path. Zero compromise on speed.

<br />

![Version](https://img.shields.io/badge/version-0.1.0--pre-e8a020?style=flat-square&labelColor=1a1a1a)
![Built with Rust](https://img.shields.io/badge/built_with-Rust-ce422b?style=flat-square&labelColor=1a1a1a&logo=rust&logoColor=white)
![License](https://img.shields.io/badge/license-MIT-f5c842?style=flat-square&labelColor=1a1a1a)
![Crate](https://img.shields.io/badge/crate-dom--render--compiler-3da35d?style=flat-square&labelColor=1a1a1a)
![Runtime](https://img.shields.io/badge/runtime-axum_0.8_+_tokio-2677cc?style=flat-square&labelColor=1a1a1a)
![Parser](https://img.shields.io/badge/JSX%2FTSX-SWC--powered-0d9488?style=flat-square&labelColor=1a1a1a)
![Status](https://img.shields.io/badge/status-pre--release-6b7280?style=flat-square&labelColor=1a1a1a)

<br />

</div>

---

## Why AlBDO

AlBDO is not a meta-framework bolted on top of an existing runtime. It is a **compiler and HTTP runtime built ground-up in Rust** — the bundler, the scheduler, the server, and the CLI are a single unified binary. No Node.js ever touches a live request.

| | AlBDO | Next.js | Remix |
|---|---|---|---|
| **Language** | Rust | JavaScript | JavaScript |
| **Node.js in hot path** | None | Always | Always |
| **Hydration strategy** | Compiler-inferred (A/B/C) | Manual hints | Manual hints |
| **Cached response time** | ~0.07ms | ~2–8ms | ~3–10ms |
| **Deploy artifact** | Single binary | Node process + assets | Node process + assets |
| **HMR** | SSE + AST patch cache | Webpack / Turbopack | Vite |

---

## Effect Lattice — Hydration Tiers

AlBDO's compiler analyses every component's effect profile at build time and classifies it into one of three hydration tiers. No runtime detection. No configuration needed.

```
EffectProfile { hooks, async, io, side_effects }
        │
        ▼
┌─────────────────────────────────────────────────────────┐
│  Tier A  │  No hooks · no async · no side effects       │
│          │  → Ships pure HTML. Zero bytes of client JS. │
├─────────────────────────────────────────────────────────┤
│  Tier B  │  Light interactivity, event handlers         │
│          │  → Only the island hydrates on the client.   │
├─────────────────────────────────────────────────────────┤
│  Tier C  │  Full hook surface, async I/O, side effects  │
│          │  → Full client hydration, compiler-decided.  │
└─────────────────────────────────────────────────────────┘
```

> **v0.1.1** will print tier decisions in the terminal during `albedo dev` and `albedo build`.

```
  App          → Tier A  (zero JS)
  Header       → Tier A  (zero JS)
  HeroImage    → Tier A  (zero JS)
  Button       → Tier B  (selective hydration)
  Navigation   → Tier B  (selective hydration)
  FeatureCard  → Tier C  (full hydration)
```

---

## Quick Start

```sh
# Install — npm shell package, platform binary auto-selected
npm install -g albedo

# Scaffold a new project (generates _albedo_guide.tsx with Tier A/B/C examples)
albedo init my-app
cd my-app

# Start dev server with HMR over SSE
albedo dev

# Production build → single deployable binary
albedo build
```

---

### Runtime kernel

| Component | Role |
|---|---|
| `SentinelRing` | Request watchdog and backpressure gate |
| `OvertakeZoneScheduler` | Preemptive task scheduler |
| `PiArchKernel` | Lagrange-scored 4-lane render kernel |
| `WebTransportMuxer` | 4-stream HTTP/3 mux (bidirectional) |

### Key source files

```
dom-render-compiler/
├── src/
│   ├── lib.rs               # RenderCompiler facade
│   ├── types.rs             # Tier, HydrationMode, shared types
│   ├── effects.rs           # EffectProfile + lattice inference
│   ├── ir.rs                # CanonicalIrDocument
│   ├── graph.rs             # ComponentGraph (DashMap)
│   ├── parser.rs            # SWC JSX/TSX parser + effect pass
│   ├── manifest/schema.rs   # RenderManifestV2
│   ├── bundler/             # Classify → Plan → Rewrite → Emit
│   └── runtime/             # engine, scheduler, pi_arch, webtransport
├── crates/
│   ├── albedo-node/         # NAPI bindings (cross-platform)
│   └── albedo-server/       # axum 0.8 + tokio HTTP runtime
└── bin/
    ├── albedo.rs            # CLI: init / dev / build + HMR over SSE
    ├── dom-compiler.rs
    └── albedo-bench.rs
```

---

## Performance

Phase P · Stream G ships four reproducible Criterion benches under
[`benches/parity_*.rs`](./benches/) measuring the framework cost of
the framework's hot paths. Methodology + how to reproduce:
[`benchmarks/parity/README.md`](./benchmarks/parity/README.md).

| Metric | ALBEDO | Reference (Next.js / React 18) | Ratio |
|---|---|---|---|
| **FCP shell bytes** (per route, mean across 10) | **~315 B** | 80–150 KB (first HTML + framework runtime) | ~250–475× smaller |
| **Hydration bytes** (counter island: wrapper + opcodes) | **~283 B** | 42–48 KB gzipped (React island) | ~150× smaller |
| **Action round-trip** (in-process, full wire) | **~8 µs** | 1–5 ms (Server Actions warm path) | ~125–625× faster |
| **Cold start** (`CompiledProject::load_from_dir`, small project) | **~1.12 ms** | 1–3 s (`next start`, equivalent route set) | ~1000–3000× faster |

> Numbers reproduced locally via `cargo bench --bench parity_*`.
> Reference figures come from published Next.js / React baselines; we
> measure ALBEDO and frame the comparison honestly, never asking the
> reader to take a marketing claim on trust. Re-run the benches when
> the relevant code paths change — see the methodology doc for the
> refresh cadence.

```
Cached response time   ~0.07ms   (categorically faster than JS-based frameworks)
Node.js processes      0         (none — ever)
Deploy artifact        1 binary  (scp it anywhere)
```

---

## Features

- **SWC-powered JSX/TSX parser** with full effect inference — template literals, ternary/binary/unary, `const` bindings, `Array.map()`, `classnames`/`clsx` (native, no npm), object/array literals, and string prototype methods
- **AST patch cache** — `source_hashes` + `patch()` + `PatchReport` for incremental re-parse on HMR
- **Deterministic cache invalidation** via FNV-1a hashing (not `DefaultHasher`)
- **3-phase mutex pattern** to unblock concurrent HTTP requests during render
- **Multi-route support** — `albedo.config.json` `routes` map, single `load_from_dir` scan, per-route `SharedDevState`
- **Radix router** with middleware, layout, and streaming in `crates/albedo-server`
- **`albedo init`** generates `_albedo_guide.tsx` — a self-documenting starter with inline Tier A/B/C examples

---

## Roadmap — v0.1.1

> Edge-native release. Focus: HTTP/3 streaming, single-binary distribution, and zero-config asset pipeline.

### WebTransport-native streaming

Bidirectional component streaming over HTTP/3 via the `WebTransportMuxer` 4-stream kernel. True full-duplex server push — no polling, no WebSocket fallback.

**Status:** `pre-release hardening complete`

Ship checklist:
- `WTStreamRouter` stream-slot assignment + per-component patch sequencing
- HTTP transport negotiation with silent SSE fallback
- WT bootstrap emission only for Tier B/C routes
- WT capability endpoint at `/_albedo/wt`
- Session bridge: route renders can be pushed onto WT stream slots 0/1/2/3
- Dev observability: per-client transport logs + stream assignment traces

---

### Single-binary edge compilation

Deploy your entire application as one `scp`-able binary. Full cross-platform NAPI build matrix via GitHub Actions:

| Target | Status |
|---|---|
| `win32-x64-msvc` | Available |
| `darwin-x64` | In progress |
| `darwin-arm64` | In progress |
| `linux-x64-gnu` | Planned |
| `linux-arm64-gnu` | Planned |

**Status:** `in progress`

---

### Zero-config image & font pipeline

Automatic asset optimization baked into the compiler pass — no config files, no plugins, no Webpack. Images emit optimal formats (AVIF/WebP) and fonts are subset at build time. Zero runtime overhead.

**Status:** `planned`

---

### Compile-time i18n

Translated pages resolved entirely at compile time. The compiler emits a separate static bundle per locale — zero runtime i18n library, zero locale-detection overhead in the hot path.

**Status:** `planned`

---

### Tier feedback in terminal

Effect lattice decisions (Tier A / B / C per component) are already computed at compile time. v0.1.1 surfaces them as structured output during `albedo dev` and `albedo build` so developers can see exactly what the compiler decided and why.

**Status:** `planned`

---

## Distribution

AlBDO follows the **esbuild/Turbo npm distribution model**:

```
albedo               ← shell package (detects platform, delegates)
├── albedo-win32-x64-msvc
├── albedo-darwin-x64
├── albedo-darwin-arm64
├── albedo-linux-x64-gnu
└── albedo-linux-arm64-gnu
```

Homebrew tap and a `curl | sh` installer backed by GitHub Releases are also planned for v0.1.1.

---

## Contributing

AlBDO is pre-release and developed in the open. The codebase is structured for independent contribution:

- **`albedo-core`** — compiler IR, effect lattice, graph, parser
- **`albedo-analyzer`** — bundle planning, manifest generation, rewrite passes

GSoC submissions are planned for both crates as independent projects.

| Resource | Link |
|----------|------|
| GSoC proposal — `albedo-analyzer` | *(link TBD)* |
| GSoC proposal — `albedo-core` | *(link TBD)* |
| Landing page | [albedo.dev](https://albedo.dev) |
| Architecture deep-dive | [`docs/architecture.md`](docs/architecture.md) |

---

## Compatibility

| OS | Node version | NAPI target | Status |
|----|-------------|-------------|--------|
| Windows x64 | 18 / 20 / 22 | `win32-x64-msvc` | Available |
| macOS x64 | 18 / 20 / 22 | `darwin-x64` | In progress |
| macOS ARM64 | 18 / 20 / 22 | `darwin-arm64` | In progress |
| Linux x64 | 18 / 20 / 22 | `linux-x64-gnu` | Planned |
| Linux ARM64 | 18 / 20 / 22 | `linux-arm64-gnu` | Planned |

---

<div align="center">

Built by [Sen-Bishal](https://github.com/Sen-Bishal) and [PixMusicaX](https://github.com/PixMusicaX)

[github.com/AlBDO](https://github.com/AlBDO) · MIT License

</div>
