---
name: project-runtime-kernel
description: "4-lane runtime kernel — FourLaneRuntimePipeline, frame_tick, dirty bitmap, highway, scheduler, pi-arch, emitter, WebTransport muxer."
metadata: 
  node_type: memory
  type: project
  originSessionId: 1567cc15-f58b-4900-b9ba-40c458d1c555
---

The runtime kernel lives in `src/runtime/`. The hot-path entry point is `FourLaneRuntimePipeline::tick_frame()` in `src/runtime/pipeline.rs` (57 KB — the biggest file in the kernel).

**Why:** This is the "cycle 2–5" refactor — SoA IR columns, SIMD diff into a flat bitmap, lane-partitioned rayon passes, frame ticks emitting `OpcodeFrame`s to WT streams. The kernel itself is wired end-to-end; the user's wiring plan ([[project-wiring-plan]]) is closing the last 8 gaps that prevent the kernel from being driven from the live code paths.

**How to apply:** Anything performance-critical lives here. Edits MUST preserve the contract that the hot path performs zero heap allocation (everything lives in `FrameArena`).

## FourLaneRuntimePipeline (`pipeline.rs`)
Owns:
- `highway: HighwayPlan` (4-lane component partition + cross-lane deps)
- `analyses` (the per-component analysis HashMap)
- `scheduler: OvertakeZoneScheduler`
- `inter_lane: PiArchLayer` (4 ArrayQueues with Lagrange-scored routing)
- `stream_router: WTStreamRouter` (wraps WebTransportMuxer)
- `frame_arena: FrameArena` (preallocated scratch indices, per-lane buckets, per-lane patch bufs, opcode results)
- `columns: IrColumns` (lane-sorted)
- `dirty_bitmap: DirtyBitmap`
- `prev_source_hashes: Vec<u64>` — baseline for SIMD diff. Seeded from initial columns to avoid spurious bootstrap deltas on first tick.
- `granularity: GranularityController` — ONE per pipeline (its new() does sysinfo I/O).
- `pinned_pool: Option<rayon::ThreadPool>` — built by `affinity::build_pinned_rayon_pool(LANE_COUNT)`. None falls back to global pool.
- `frame_metrics: FrameMetrics` — ring buffer of `total_ns` for p50/p99.
- `prev_intern_snapshot: InternTableSnapshot` — diffs against current `StringInterner` to emit `PatchInternTable` ops.
- `runtime_handle: Option<tokio::runtime::Handle>` — Phase D async spawner needs it; `require_runtime_handle` is the typed-error path.
- Phase D: `suspense_allocator: AtomicU32`, `pending_placeholders: DashMap<SuspenseId, AsyncIslandRecord>`, `async_tx/rx` mpsc channel, `pending_placeholder_emissions`.
- Phase H: `slot_store: Arc<SlotStore>` — shared with the server's action dispatcher.

Builder chain: `FourLaneRuntimePipeline::new(...).with_runtime_handle(handle).with_slot_store(arc)`.

## Highway (`highway.rs`)
`LANE_COUNT = 4`. Compile-time assert: `runtime::highway::LANE_COUNT == ir::columns::LANE_COUNT` (so column partitions stay in lockstep).

`HighwayPlan::from_levels(graph, analyses, levels)` partitions each topological level by `phase_to_lane(phase) = floor(phase.rem_euclid(2π) / (2π / LANE_COUNT))`. Within a lane × level, sorts by priority desc then ComponentId asc. Computes `cross_lane_dependencies`: every dep edge whose endpoints land in different lanes.

`component_lane: Vec<(ComponentId, u8)>` sorted by id — binary searched in the hot path instead of HashMap lookups. `cross_lane_deps_for_dependent(id)` binary-searches by `dependent` (O(log N) + linear expansion across equal-key run).

`lane_offsets: [u32; 5]` — cumulative prefix-sum, paired with `IrColumns::lane_offsets`.

## DirtyBitmap (`dirty_bitmap.rs`)
Flat `Box<[AtomicU64]>` indexed by column position. `mark(idx)` → `fetch_or(1 << bit, AcqRel)`. `drain(F)` → per-word `swap(0, AcqRel)` then `trailing_zeros` pop loop.

`hash_diff_into_bitmap(old, new, bitmap)` and `hash_diff_into_bitmap_at(old, new, bitmap, start_idx)` are SIMD (`wide::u64x4`) equality diff kernels — fold inequality mask into bitmap words. Handles sub-word offsets for per-lane reconcile feeding the same global bitmap.

`drain_into(scratch: &mut Vec<u32>)` clears+appends — the zero-alloc drain used by `frame_tick`.

## Frame tick (`frame.rs`)
`frame_tick(columns, dirty_bitmap, frame_arena, muxer)`. Phases:
1. **Drain** — `dirty_bitmap.drain_into(&mut arena.scratch_indices)`.
2. **Partition** — bucket scratch indices into `arena.lane_buckets[lane]` using `columns.lane_ids[idx]`.
3. **Emit** — per non-empty lane: build 16-byte patch records `[u32 col_idx, u32 field_mask, u64 source_hash]` (field_mask currently always SOURCE_HASH; cycle 6 will fold more); call `emitter::emit_lane_frames` to produce `OpcodeFrame`s + wire bytes.
4. **Sequence** — push onto WT muxer at `WT_STREAM_SLOT_PATCHES` (slot 2), allocating sequence via `muxer.allocate_sequence(stream_id)` (lock-free `fetch_add(1, Relaxed)`).

Publishes a `LaneFrameReport` to the installed `LaneObserver` (process-wide `OnceLock`) so the inspector can render a heatmap. Cost when no observer is installed: one `Option::is_some()` check.

`FrameMetrics::record(report)` keeps a `VecDeque` ring of `total_ns`; `percentile_ns(permille: u32)` clones+sorts on demand (intentional — called at 1 Hz, not per tick).

## Emitter (`emitter.rs`)
`emit_lane_frames(columns, lane_buckets, muxer)`:
- For each non-empty lane: allocate frame_id from muxer; if `bucket.len() == 1`, attribute frame to that component's id.
- `emit_lane_instructions` builds `Vec<Instruction::SlotSet>` — payload is `[8 bytes source_hash LE, 1 byte effects bitmask]`.
- `frame.wire_encode()` via the `WireEncode` trait (not the free `encode_frame` function — that's wrapper-only).
- Returns `EmitResult { lane, frame_id, component_id, wire_bytes, instruction_count }`.

`InternTableSnapshot::capture(interner, classify_fn)` captures Tag/Attr/Event entries from the StringInterner; differencing it against the prev snapshot produces `Instruction::PatchInternTable`s for the control stream.

## Scheduler (`scheduler.rs`)
`OvertakeZoneScheduler`. Owns:
- `HotSetRegistry` (DashMap of ComponentId → RenderPriority, max 32).
- `SentinelRing` — wraps a `DirtyBitmap` + slot→id lookup; replaces the prior linked-list ring.
- `analyzer_queue: ArrayQueue<ComponentId>` (4096) — produced by background analyzer.
- `render_queue: ArrayQueue<ComponentId>` (4096) — consumed by render workers.
- `analyzer_chunk_limit: AtomicUsize` — adaptive: shrinks on overtake, grows when no overtake.

`run_frame`: drain hot set → ring drain into render queue; then chunked drain of analyzer queue (skipping hot-set components, pushing to render queue), bounded by `overtake_interval` + `overtake_budget` (Duration). Returns `SchedulerFrameStats`.

API for dynamic hot-set: `configure_hot_set(entries)`, `register_hot_component`, `deregister_hot_component`. Currently not yet wired through `FourLaneRuntimePipeline` (Gap 5 in [[project-wiring-plan]]).

## Pi-arch (`pi_arch.rs`)
`PiArchKernel` with `LagrangeWeights { phase: 1.0, priority: 1.0, load: 0.25 }`. Routes `LaneMessage`s to one of LANE_COUNT lanes by minimizing `phase_distance + priority_delta + load`. Deterministic; ties resolved by preferring lower lane index. Each `PiArchLayer` holds 4 `ArrayQueue<LaneMessage>`s.

## Observers (`render_observer.rs`)
Two observer surfaces, both behind `OnceLock`:
- `RenderObserver::on_render(RenderInfo)` — fired from `ComponentProject::render_local` via RAII `FrameGuard::drop()`. Carries component name, module spec, duration_us, cascade_children (names of inner renders during this frame).
- `LaneObserver::on_frame(LaneFrameReport)` — fired by `frame_tick`. Per-lane bytes/patches, dirty count, total_ns.

Thread-local `STACK` of `Frame`s maintains parent → child cascade. `FrameGuard` is RAII so early `?` still publishes.

## WebTransport (`webtransport.rs`)
Stream slots: `WT_STREAM_SLOT_CONTROL = 0`, `_SHELL = 1`, `_PATCHES = 2`, `_PREFETCH = 3`. `WEBTRANSPORT_STREAM_COUNT = 4`.

`WTStreamRouter::stream_slot_for(tier, render_mode)`:
- Control → slot 0
- Shell → slot 1 (Tier B/C) or slot 0 (Tier A — control)
- Patch → slot 2
- Prefetch → slot 3

`WebTransportMuxer::reassemble_binary_stream(frames)` concatenates by `frame_id` after sorting by `sequence`. Validates `PayloadKind` (Text vs Binary) — mismatch returns `WebTransportError::PayloadKindMismatch` rather than silently dropping bytes.

Frame contract: one `OpcodeFrame` MAY span multiple `WebTransportFrame`s sharing a `frame_id`; decoder must reassemble before `wire::decode_frame`. `frame_id` allocated via `muxer.allocate_sequence(stream_id)`.

## Affinity (`affinity.rs`)
`build_pinned_rayon_pool(num_threads)` — round-robin pins worker N to core `N % core_count` via `core_affinity`. Returns `None` in sandboxed/CI environments (graceful degradation).
