---
name: project-overview
description: "AlBDO — what it is, the workspace shape, and the macro flow from JSX source to client bytes."
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

AlBDO is a Rust-native DOM render compiler + HTTP runtime for JSX/TSX. It replaces Next.js/Remix-style Node hot paths with a single Rust binary that handles parsing, analysis, bundling, server-side rendering, and (eventually) edge-native HTTP/3 streaming. No Node.js touches a live request.

**Why:** The pitch is "compiler-inferred hydration tiers + WebTransport-native streaming + single deployable binary". Effect lattice classifies every component as Tier A (zero JS), Tier B (island hydration), or Tier C (full hydration) at compile time, with no runtime detection.

**How to apply:** When the user talks about renderer/analyzer/runtime, it always means the AlBDO subsystems — not generic terms. Always trace from the parsed JSX (SWC) through `IrColumns` (SoA) → opcodes → WebTransport patches. Phase-letter names (Phase A through Phase K) are the load-bearing terminology — see [[project-phase-glossary]].

Macro flow (compile-time → runtime):

1. `scanner::ProjectScanner` walks the source dir, hands files to `parser::ComponentParser` (SWC-based).
2. `ParsedComponent`s feed `ComponentGraph` (DashMap-backed). `WeightEstimator` derives size + above-fold/LCP/interactive hints from names.
3. `analysis::ParallelAnalyzer` (rayon) computes priority, phase (angle in radians), estimated time. Picks parallel vs serial via `GranularityController` (sysinfo-based — `new()` does I/O).
4. `ParallelTopologicalSorter` layers components into render batches.
5. `ir::IrColumns` (SoA struct-of-arrays) is the runtime IR truth; `CanonicalIrDocument` is the JSON shell. `effects::decide_tier_and_hydration` assigns tiers.
6. `manifest::build_render_manifest_v2` produces `RenderManifestV2` (consumed by `albedo-server`). Bundler emits `BundlePlan` + wrapper modules + vendor chunks + precompiled QuickJS scripts + static-slice manifest.
7. `albedo-server`'s `RendererRuntime` loads artifacts from disk; `ServerRenderer<QuickJsEngine>` warm-caches them; routes are served via axum.
8. Runtime hot path: `FourLaneRuntimePipeline` runs frame ticks — drain dirty bitmap (SIMD `wide::u64x4` over hash columns), partition by lane, emit opcode frames via `WireEncode`, push onto WebTransport muxer streams (slot 0 control, 1 shell, 2 patches, 3 prefetch).
9. Client side: bakabox JS decoder reads bincode opcode frames and applies them to a slot table.

Authors: Bishal Sen + PixMusicaX. Pinaki Pritam Singha is named in comments — appears to be a frequent reviewer/collaborator on this codebase. References to "bakabox" / "sussybox" in comments mean the client-side JS runtime/decoder.
