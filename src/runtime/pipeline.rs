use super::affinity::build_pinned_rayon_pool;
use super::dirty_bitmap::{hash_diff_into_bitmap, hash_diff_into_bitmap_at, DirtyBitmap};
use super::emitter::{self, EmitResult, InternTableSnapshot};
use super::frame::{frame_tick, FrameArena, FrameMetrics, FrameReport};
use super::highway::{phase_to_lane, HighwayPlan, LANE_COUNT};
use super::hot_set::{HotSetError, RenderPriority};
use super::pi_arch::{
    DispatchOutcome, LaneMessage, LaneTarget, PhaseResult, PiArchKernel, PiArchLayer,
};
use super::scheduler::{OvertakeZoneScheduler, SchedulerConfig, SchedulerFrameStats};
use super::webtransport::{
    FramePayload, LaneRenderedChunk, WTRenderMode, WTStreamRouter, WebTransportError,
    WebTransportFrame, WebTransportMuxer, WT_STREAM_SLOT_PATCHES,
};
use crate::analysis::adaptive::GranularityController;
use crate::ir::opcode::{Instruction, InstructionRange, OpcodeFrame, StableId, SuspenseId};
use crate::ir::wire::{self, WireError};
use crate::graph::ComponentGraph;
use crate::ir::columns::{IrColumns, LaneColumnPass};
use crate::manifest::schema::Tier;
use crate::runtime::slot_store::SlotStore;
use crate::types::{CompilerError, ComponentAnalysis, ComponentId};
use dashmap::DashMap;
use rustc_hash::{FxHashMap, FxHasher};
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum RuntimePipelineError {
    #[error(transparent)]
    Compiler(#[from] CompilerError),
    #[error(transparent)]
    HotSet(#[from] HotSetError),
    #[error(transparent)]
    WebTransport(#[from] WebTransportError),
    #[error(transparent)]
    Wire(#[from] WireError),
    /// Phase-D operations (async island spawn, Future resolution) require a
    /// tokio runtime handle. Surface this as a typed error rather than the
    /// `Handle::current()` panic so the server can refuse the request with a
    /// clean 5xx instead of crashing the tick loop.
    #[error("pipeline missing tokio runtime handle; call `with_runtime_handle` before spawning async work")]
    MissingRuntimeHandle,
    /// Phase-D: an invariant on `InstructionRange` was violated. Surfaced
    /// as a typed error so cancellation / wire-shape bugs don't panic
    /// the tick loop.
    #[error(transparent)]
    Range(#[from] crate::ir::opcode::RangeError),
}

/// Phase-D — pending async island registry entry.
///
/// Records the placeholder `stable_id` so a later `Remove` can mark the
/// island cancelled before its resolver Future lands. `cancelled` is
/// atomic so a tick-thread mark and a resolver-thread read don't need a
/// lock.
#[derive(Debug)]
struct AsyncIslandRecord {
    stable_id: StableId,
    cancelled: AtomicBool,
}

/// Phase-D — message sent from a resolved async-island Future back to
/// the pipeline. Drained inside `tick_frame` and wrapped into a `Patch`
/// frame on `WT_STREAM_SLOT_PATCHES`.
#[derive(Debug)]
struct AsyncResolution {
    suspense_id: SuspenseId,
    instructions: Vec<Instruction>,
}

pub struct FourLaneRuntimePipeline {
    highway: HighwayPlan,
    analyses: HashMap<ComponentId, ComponentAnalysis>,
    scheduler: OvertakeZoneScheduler,
    inter_lane: PiArchLayer,
    stream_router: WTStreamRouter,
    frame_arena: FrameArena,
    columns: IrColumns,
    dirty_bitmap: DirtyBitmap,
    // Snapshot of the previous tick's source hashes; diffed against the live
    // column on each reconcile to drive the SIMD bulk dirty-mark kernel.
    // Without it, the kernel runs against an all-zero baseline and the
    // first tick floods the bitmap with bootstrap deltas. Co-designed
    // with the kernel; ship together.
    prev_source_hashes: Vec<u64>,
    // Constructed once at pipeline build time — `new()` does sysinfo I/O.
    granularity: GranularityController,
    // Optional core-pinned rayon pool; when present, `tick_frame` runs
    // inside `pool.install` so every lane worker stays on a fixed core.
    // `None` falls back to the global rayon pool with no behavior change.
    pinned_pool: Option<rayon::ThreadPool>,
    frame_metrics: FrameMetrics,
    // Phase B — intern table baseline for incremental diffing.
    prev_intern_snapshot: InternTableSnapshot,
    // Phase B-finish — optional tokio runtime handle, threaded in by the
    // server crate via `with_runtime_handle`. Stays `None` in unit tests
    // that exercise sync paths only. Phase D's `Placeholder` emitter will
    // require it via `require_runtime_handle` and fail with a typed error
    // rather than panicking on `Handle::current()` outside a runtime ctx.
    runtime_handle: Option<tokio::runtime::Handle>,

    // Phase D — async island substrate.
    //
    // `suspense_allocator` mints monotonic SuspenseIds. `pending_placeholders`
    // tracks every in-flight island so a `Remove` against the placeholder's
    // stable_id can mark the eventual Patch as cancelled. The mpsc channel
    // carries resolutions from spawned resolver tasks back to `tick_frame`,
    // which wraps each into a `Patch` frame.
    //
    // `pending_placeholder_emissions` buffers `LaneRenderedChunk`s built by
    // `enqueue_async_island` so the next `drain_opcode_chunks` ships the
    // `Placeholder` opcode on the same tick the island was registered.
    suspense_allocator: AtomicU32,
    pending_placeholders: DashMap<SuspenseId, AsyncIslandRecord>,
    async_tx: mpsc::UnboundedSender<AsyncResolution>,
    async_rx: Mutex<mpsc::UnboundedReceiver<AsyncResolution>>,
    pending_placeholder_emissions: Mutex<Vec<LaneRenderedChunk>>,

    // Phase H — server-side reactive slot store. Shared via `Arc` with
    // the server's action dispatcher so handlers can read/write slots
    // and the changes are visible to subsequent drains without an
    // intermediate copy. Defaults to a fresh empty store; userland
    // binds the shared instance via `with_slot_store` when it wants
    // the server-side state visible from the runtime side too.
    slot_store: Arc<SlotStore>,
}

impl FourLaneRuntimePipeline {
    pub fn new(
        graph: &ComponentGraph,
        analyses: HashMap<ComponentId, ComponentAnalysis>,
        component_tiers: HashMap<ComponentId, Tier>,
        hot_set_entries: &[(ComponentId, RenderPriority)],
        scheduler_config: SchedulerConfig,
        lane_queue_capacity: usize,
    ) -> Result<Self, RuntimePipelineError> {
        let highway = HighwayPlan::build(graph, &analyses)?;
        let scheduler = OvertakeZoneScheduler::with_hot_set(scheduler_config, hot_set_entries)?;
        let inter_lane = PiArchLayer::new(lane_queue_capacity.max(1), PiArchKernel::default());
        let stream_router =
            WTStreamRouter::with_component_tiers(WebTransportMuxer::new(), component_tiers);
        let frame_arena = FrameArena::with_capacity(analyses.len());

        let mut columns = IrColumns::from_graph(graph, &analyses);
        let phase_by_id: FxHashMap<u64, f64> = analyses
            .iter()
            .map(|(id, analysis)| (id.as_u64(), analysis.phase))
            .collect();
        columns.sort_by_lane(|id| phase_by_id.get(&id).copied().map_or(0, phase_to_lane));
        let dirty_bitmap = DirtyBitmap::with_capacity(columns.len());
        // Seed the reconcile baseline with the columns' initial hashes so
        // the first `reconcile_source_hashes()` only flags real mutations
        // and not the bootstrap delta against an all-zero `Vec`.
        let prev_source_hashes = columns.source_hashes().to_vec();
        let frame_metrics = FrameMetrics::default();
        let prev_intern_snapshot = InternTableSnapshot::default();

        let (async_tx, async_rx) = mpsc::unbounded_channel();

        // One pool per pipeline. `LANE_COUNT` workers, one per lane; the
        // helper returns `None` if the platform refuses pinning, in which
        // case `tick_frame` falls back to rayon's global pool.
        let pinned_pool = build_pinned_rayon_pool(LANE_COUNT);

        Ok(Self {
            highway,
            analyses,
            scheduler,
            inter_lane,
            stream_router,
            frame_arena,
            columns,
            dirty_bitmap,
            prev_source_hashes,
            granularity: GranularityController::new(),
            pinned_pool,
            frame_metrics,
            prev_intern_snapshot,
            runtime_handle: None,
            suspense_allocator: AtomicU32::new(0),
            pending_placeholders: DashMap::new(),
            async_tx,
            async_rx: Mutex::new(async_rx),
            pending_placeholder_emissions: Mutex::new(Vec::new()),
            slot_store: Arc::new(SlotStore::new()),
        })
    }

    /// Binds a tokio runtime handle to the pipeline.
    ///
    /// The handle is stored, not consumed during construction. Phase D's
    /// async-island spawner reads it via [`Self::require_runtime_handle`]
    /// when emitting `Placeholder` opcodes; sync code paths (the
    /// reconcile/tick fast path, fixture tests) never touch it.
    ///
    /// Returns `self` so server bootstrap can chain the call:
    ///
    /// ```ignore
    /// let pipeline = FourLaneRuntimePipeline::new(...)?
    ///     .with_runtime_handle(tokio::runtime::Handle::current());
    /// ```
    ///
    /// Calling twice replaces the handle — there is no historical reason
    /// to keep the prior one, and the server only ever binds the handle
    /// once at startup.
    #[must_use]
    pub fn with_runtime_handle(mut self, handle: tokio::runtime::Handle) -> Self {
        self.runtime_handle = Some(handle);
        self
    }

    /// Returns the bound tokio runtime handle, if any.
    ///
    /// Prefer [`Self::require_runtime_handle`] at call sites that genuinely
    /// need to spawn — the typed error path is part of the contract.
    pub fn runtime_handle(&self) -> Option<&tokio::runtime::Handle> {
        self.runtime_handle.as_ref()
    }

    /// Returns the bound tokio runtime handle or a typed error.
    ///
    /// This is the entry point Phase D's spawner uses. It exists so the
    /// failure mode for "pipeline constructed without a runtime handle"
    /// is a well-typed [`RuntimePipelineError::MissingRuntimeHandle`]
    /// instead of a `Handle::current()` panic mid-tick.
    pub fn require_runtime_handle(
        &self,
    ) -> Result<&tokio::runtime::Handle, RuntimePipelineError> {
        self.runtime_handle
            .as_ref()
            .ok_or(RuntimePipelineError::MissingRuntimeHandle)
    }

    /// Phase-H — binds a shared [`SlotStore`] to this pipeline.
    ///
    /// Pass the same `Arc<SlotStore>` the server's action dispatcher
    /// holds so writes the dispatcher performs are immediately visible
    /// to the pipeline's reads (and vice versa). Without this call
    /// each side runs against its own empty store and the reactive
    /// loop never closes.
    ///
    /// Returns `self` so it chains with `with_runtime_handle` in
    /// server bootstrap.
    #[must_use]
    pub fn with_slot_store(mut self, slot_store: Arc<SlotStore>) -> Self {
        self.slot_store = slot_store;
        self
    }

    /// Returns the shared slot store. Callers clone the `Arc` to keep
    /// a stable view across requests.
    #[must_use]
    pub fn slot_store(&self) -> Arc<SlotStore> {
        self.slot_store.clone()
    }

    pub fn submit_analyzer_result(&self, component_id: ComponentId) -> bool {
        self.scheduler.submit_analyzer_result(component_id)
    }

    /// Replaces the scheduler's hot-set registry with `entries` and rebuilds
    /// the sentinel ring. Use after pipeline construction to flush in a
    /// freshly-derived hot set (e.g. from `component_tiers`).
    pub fn reconfigure_hot_set(
        &mut self,
        entries: &[(ComponentId, RenderPriority)],
    ) -> Result<(), RuntimePipelineError> {
        Ok(self.scheduler.configure_hot_set(entries)?)
    }

    /// Adds a single component to the hot set without disturbing the rest of
    /// the registry. Sentinel ring is rebuilt to keep the dirty-mark path
    /// O(1).
    pub fn register_hot_component(
        &mut self,
        component_id: ComponentId,
        priority: RenderPriority,
    ) -> Result<(), RuntimePipelineError> {
        Ok(self
            .scheduler
            .register_hot_component(component_id, priority)?)
    }

    /// Removes a component from the hot set. No-op if it wasn't registered.
    pub fn deregister_hot_component(&mut self, component_id: ComponentId) {
        self.scheduler.deregister_hot_component(component_id);
    }

    pub fn mark_hot_dirty(&self, component_id: ComponentId) -> bool {
        self.scheduler.mark_hot_dirty(component_id)
    }

    pub fn run_scheduler_frame(&self) -> SchedulerFrameStats {
        self.scheduler.run_frame()
    }

    pub fn dispatch_cross_lane_dependency_signals(&self) -> usize {
        let mut routed = 0usize;

        for edge in &self.highway.cross_lane_dependencies {
            let Some(source_analysis) = self.analyses.get(&edge.dependency) else {
                continue;
            };
            let Some(target_analysis) = self.analyses.get(&edge.dependent) else {
                continue;
            };

            let message = LaneMessage {
                from_lane: edge.from_lane,
                component_id: edge.dependency,
                phase_result: PhaseResult {
                    phase: source_analysis.phase,
                    priority: source_analysis.priority,
                },
            };

            let target = LaneTarget {
                lane: edge.to_lane,
                phase: target_analysis.phase,
                priority: target_analysis.priority,
            };

            if let DispatchOutcome::Routed { .. } = self.inter_lane.dispatch(message, &[target]) {
                routed += 1;
            }
        }

        routed
    }

    /// Targeted variant of [`Self::dispatch_cross_lane_dependency_signals`].
    ///
    /// Uses the `O(log N + k)` binary-search lookup on
    /// `cross_lane_dependencies` (sorted by `dependent`) instead of scanning
    /// the full edge list. Suitable for the per-component update path where
    /// only one dependent is known to have changed.
    pub fn dispatch_cross_lane_signals_for(&self, dependent: ComponentId) -> usize {
        let mut routed = 0usize;
        for edge in self.highway.cross_lane_deps_for_dependent(dependent) {
            let Some(source_analysis) = self.analyses.get(&edge.dependency) else {
                continue;
            };
            let Some(target_analysis) = self.analyses.get(&edge.dependent) else {
                continue;
            };

            let message = LaneMessage {
                from_lane: edge.from_lane,
                component_id: edge.dependency,
                phase_result: PhaseResult {
                    phase: source_analysis.phase,
                    priority: source_analysis.priority,
                },
            };
            let target = LaneTarget {
                lane: edge.to_lane,
                phase: target_analysis.phase,
                priority: target_analysis.priority,
            };

            if let DispatchOutcome::Routed { .. } = self.inter_lane.dispatch(message, &[target]) {
                routed += 1;
            }
        }
        routed
    }

    /// Diagnostic-only — returns the flattened component order owned by
    /// `lane`. Not for the hot path; intended for introspection and tests.
    pub fn lane_component_ids(&self, lane: usize) -> Vec<ComponentId> {
        self.highway
            .lanes
            .get(lane)
            .map(|plan| plan.flattened_components())
            .unwrap_or_default()
    }

    pub fn drain_inter_lane_messages(&self, lane: usize) -> Vec<LaneMessage> {
        let mut drained = Vec::new();
        if lane >= LANE_COUNT {
            return drained;
        }
        self.inter_lane
            .drain_lane(lane, |message| drained.push(message));
        drained
    }

    pub fn drain_render_queue_to_lane_chunks(&self) -> Vec<LaneRenderedChunk> {
        let mut chunks = Vec::new();
        while let Some(component_id) = self.scheduler.pop_render_ready() {
            let chunk = self.stream_router.route_component_chunk(
                component_id,
                WTRenderMode::Patch,
                format!("component:{}", component_id.as_u64()),
            );
            chunks.push(chunk);
        }
        chunks
    }

    pub fn mux_lane_chunks(
        &self,
        chunks: &[LaneRenderedChunk],
    ) -> Result<Vec<WebTransportFrame>, RuntimePipelineError> {
        Ok(self.stream_router.mux_lane_chunks(chunks)?)
    }

    /// Marks a component dirty by its id, looking up its column index via
    /// [`IrColumns::index_of`]. Returns `true` iff the id is known and the
    /// bit was not already set.
    pub fn mark_component_dirty(&self, component_id: ComponentId) -> bool {
        let Some(column_idx) = self.columns.index_of(component_id.as_u64()) else {
            return false;
        };
        let Ok(idx) = usize::try_from(column_idx) else {
            return false;
        };
        self.dirty_bitmap.mark(idx)
    }

    /// Marks a column slot directly. Exposed for fuzz harnesses and soak
    /// tests that drive the bitmap without going through `ComponentId`.
    pub fn mark_column_dirty(&self, column_idx: usize) -> bool {
        self.dirty_bitmap.mark(column_idx)
    }

    /// Whole-store reconcile. Diffs the live `source_hashes` column against
    /// the snapshot taken on the previous tick using the SIMD hash-diff
    /// kernel, OR-folds mismatches into the dirty bitmap, and refreshes the
    /// snapshot in place. Returns the number of mismatched slots.
    ///
    /// Call this once per render loop iteration before `tick_frame()` so
    /// the bitmap reflects mutations the analyzer wrote into the column
    /// store since the last tick. Pair with `reconcile_lane_source_hashes`
    /// for the cycle-4 per-lane form when only one lane mutated.
    pub fn reconcile_source_hashes(&mut self) -> usize {
        let new_hashes = self.columns.source_hashes();
        // Length drift means the column set changed — resize the snapshot
        // so `copy_from_slice` below stays valid; the diff is bounded by
        // `len.min(len)` so the extra slots simply re-baseline.
        if self.prev_source_hashes.len() != new_hashes.len() {
            self.prev_source_hashes.resize(new_hashes.len(), 0);
        }
        let mismatches =
            hash_diff_into_bitmap(&self.prev_source_hashes, new_hashes, &self.dirty_bitmap);
        self.prev_source_hashes.copy_from_slice(new_hashes);
        mismatches
    }

    /// Per-lane reconcile. Diffs only the slice owned by `lane` and feeds
    /// mismatches into the global bitmap at the lane's start offset, so the
    /// cycle-2 and cycle-4 reconcile paths share one bitmap.
    pub fn reconcile_lane_source_hashes(&mut self, lane: usize) -> usize {
        if lane >= LANE_COUNT {
            return 0;
        }
        let offsets = self.columns.lane_offsets();
        let start = offsets.get(lane).copied().unwrap_or(0) as usize;
        let end = offsets.get(lane.saturating_add(1)).copied().unwrap_or(0) as usize;
        let new_hashes = self.columns.source_hashes();
        if start > end || end > new_hashes.len() {
            return 0;
        }
        // Keep the snapshot length in lockstep with the column store so the
        // lane slice is always in-bounds on both sides of the diff.
        if self.prev_source_hashes.len() != new_hashes.len() {
            self.prev_source_hashes.resize(new_hashes.len(), 0);
        }
        let old_lane_slice = &self.prev_source_hashes[start..end];
        let new_lane_slice = &new_hashes[start..end];
        let mismatches =
            hash_diff_into_bitmap_at(old_lane_slice, new_lane_slice, &self.dirty_bitmap, start);
        self.prev_source_hashes[start..end].copy_from_slice(new_lane_slice);
        mismatches
    }

    /// Recomputes per-slot `source_hashes` for every lane by folding the
    /// hot scheduling columns (`effects`, `priorities`, `phases`) into a
    /// stable FxHash digest. Picks the rayon fan-out path when the
    /// granularity controller expects parallelism to amortize, otherwise
    /// drives each lane serially via `lane_column_pass_mut`.
    ///
    /// Pair with [`Self::reconcile_source_hashes`] to surface mutations:
    /// this pass writes the new digests, the reconcile diffs them.
    pub fn run_column_analysis_pass(&mut self) {
        if self
            .granularity
            .should_parallelize(self.columns.len(), std::mem::size_of::<u64>())
        {
            self.columns.parallel_lane_column_pass(
                |pass| recompute_lane_source_hashes(pass),
                |pass| recompute_lane_source_hashes(pass),
                |pass| recompute_lane_source_hashes(pass),
                |pass| recompute_lane_source_hashes(pass),
            );
        } else {
            for lane in 0..LANE_COUNT {
                if let Some(pass) = self.columns.lane_column_pass_mut(lane) {
                    recompute_lane_source_hashes(pass);
                }
            }
        }
    }


    /// Cycle-5 RAF entry point.
    ///
    /// Drives a single reconciliation tick: drain the owned bitmap, partition
    /// the dirty set by lane via `columns.lane_offsets()`, emit per-lane patch
    /// buffers through a rayon join fan-out, and allocate one monotone
    /// sequence per non-empty lane against the shared WebTransport muxer.
    ///
    /// Zero allocations on the hot path — the pipeline's [`FrameArena`]
    /// owns every buffer and is cleared, not freed, between ticks. The
    /// returned [`FrameReport`] is also folded into [`FrameMetrics`] for
    /// downstream percentile observability.
    pub fn tick_frame(&mut self) -> FrameReport {
        // Run on the core-pinned pool when one is available so each lane
        // worker stays warm on a single core; otherwise fall through to
        // rayon's global pool.
        let report = if let Some(pool) = self.pinned_pool.as_ref() {
            pool.install(|| {
                frame_tick(
                    &self.columns,
                    &self.dirty_bitmap,
                    &self.stream_router.muxer,
                    &mut self.frame_arena,
                )
            })
        } else {
            frame_tick(
                &self.columns,
                &self.dirty_bitmap,
                &self.stream_router.muxer,
                &mut self.frame_arena,
            )
        };
        self.frame_metrics.record(&report);
        report
    }

    /// Read-only access to the frame arena's lane patch buffers, used by
    /// transports that want to forward the raw bytes after a `tick_frame`.
    pub fn frame_arena(&self) -> &FrameArena {
        &self.frame_arena
    }

    pub fn columns(&self) -> &IrColumns {
        &self.columns
    }

    pub fn dirty_bitmap(&self) -> &DirtyBitmap {
        &self.dirty_bitmap
    }

    pub fn frame_metrics(&self) -> &FrameMetrics {
        &self.frame_metrics
    }

    /// Emits the current metrics window as a tracing event. Kept separate
    /// from [`Self::tick_frame`] so the emit cadence is the caller's choice.
    pub fn emit_frame_metrics_summary(&self) {
        self.frame_metrics.emit_summary();
    }

    /// Returns the opcode emission results from the most recent
    /// [`Self::tick_frame`] call. These are the wire-encoded `OpcodeFrame`s
    /// that can be forwarded to the WebTransport patches stream.
    pub fn last_opcode_results(&self) -> &[EmitResult] {
        self.frame_arena.opcode_results()
    }

    /// Diffs the intern table state against the previous snapshot and returns
    /// `PatchInternTable` instructions for any changes. Updates the internal
    /// baseline snapshot for the next call.
    pub fn reconcile_intern_tables<F>(&mut self, classify: F) -> Vec<crate::ir::Instruction>
    where
        F: Fn(u16, &str) -> Option<crate::ir::opcode::InternTableKind>,
    {
        let current = InternTableSnapshot::capture(self.columns.strings(), classify);
        let instructions = emitter::diff_intern_tables(&self.prev_intern_snapshot, &current);
        self.prev_intern_snapshot = current;
        instructions
    }

    /// Builds bootstrap `InitInternTable` instructions for session init.
    /// Call once when a new WebTransport session connects.
    pub fn get_bootstrap_intern_payload<F>(&self, classify: F) -> Vec<crate::ir::Instruction>
    where
        F: Fn(u16, &str) -> Option<crate::ir::opcode::InternTableKind>,
    {
        let snapshot = InternTableSnapshot::capture(self.columns.strings(), classify);
        emitter::bootstrap_intern_tables(&snapshot)
    }

    // ── Phase D — async island surface ─────────────────────────────────

    /// Registers an async island and ships the `Placeholder` on the next
    /// `drain_opcode_chunks` call.
    ///
    /// Allocates a fresh `SuspenseId`, builds a `Placeholder { stable_id,
    /// suspense_id }` chunk for the patches stream, and spawns `resolver`
    /// on the bound tokio runtime. When the future completes, its
    /// `Vec<Instruction>` is delivered to the pipeline via an internal
    /// mpsc channel; the next `tick_frame` wraps that vector into a
    /// `Patch` frame.
    ///
    /// Returns the allocated `SuspenseId` so callers that want to track
    /// the island (e.g. for cancellation via [`Self::cancel_pending_async_for`])
    /// can correlate.
    ///
    /// Requires a runtime handle bound via [`Self::with_runtime_handle`];
    /// returns [`RuntimePipelineError::MissingRuntimeHandle`] otherwise.
    pub fn enqueue_async_island<F>(
        &self,
        stable_id: StableId,
        resolver: F,
    ) -> Result<SuspenseId, RuntimePipelineError>
    where
        F: Future<Output = Vec<Instruction>> + Send + 'static,
    {
        let handle = self.require_runtime_handle()?.clone();
        let suspense_id = SuspenseId(self.suspense_allocator.fetch_add(1, Ordering::Relaxed));

        self.pending_placeholders.insert(
            suspense_id,
            AsyncIslandRecord {
                stable_id,
                cancelled: AtomicBool::new(false),
            },
        );

        let placeholder_chunk = self.build_patches_chunk(vec![Instruction::Placeholder {
            stable_id,
            suspense_id,
        }])?;

        match self.pending_placeholder_emissions.lock() {
            Ok(mut queue) => queue.push(placeholder_chunk),
            Err(_) => {
                // Lock poisoning would mean an earlier tick panicked while
                // holding it. Drop the placeholder rather than panic again;
                // the island's Future will still resolve and surface a
                // (now-orphan) Patch which `drain_async_patch_chunks`
                // tolerates by skipping unknown suspense ids.
                self.pending_placeholders.remove(&suspense_id);
                return Err(RuntimePipelineError::MissingRuntimeHandle);
            }
        }

        let tx = self.async_tx.clone();
        handle.spawn(async move {
            let instructions = resolver.await;
            let _ = tx.send(AsyncResolution {
                suspense_id,
                instructions,
            });
        });

        Ok(suspense_id)
    }

    /// Marks every pending async island whose placeholder targets
    /// `stable_id` as cancelled. When the island's Future eventually
    /// lands, `drain_async_patch_chunks` drops it instead of shipping a
    /// `Patch` that would target a removed DOM node.
    ///
    /// Returns the number of islands marked. Callers ship `Remove`
    /// opcodes for the same `stable_id` independently; this call only
    /// affects the *pending* async side of the bookkeeping.
    pub fn cancel_pending_async_for(&self, stable_id: StableId) -> usize {
        let mut marked = 0;
        for entry in self.pending_placeholders.iter() {
            if entry.value().stable_id == stable_id
                && !entry.value().cancelled.swap(true, Ordering::Relaxed)
            {
                marked += 1;
            }
        }
        marked
    }

    /// Number of async islands currently in-flight (placeholder shipped,
    /// resolver Future not yet drained into a `Patch`). Exposed for
    /// observability and tests; not on the hot path.
    pub fn pending_async_count(&self) -> usize {
        self.pending_placeholders.len()
    }

    /// Drains every resolved-but-not-yet-shipped async island and builds
    /// one `Patch` frame per non-cancelled resolution. Cancelled islands
    /// are removed silently.
    ///
    /// Returns chunks ready to enqueue on the patches stream. Caller is
    /// `drain_opcode_chunks`; outside callers should not normally need
    /// this directly.
    fn drain_async_patch_chunks(&self) -> Result<Vec<LaneRenderedChunk>, RuntimePipelineError> {
        let mut out = Vec::new();

        let mut rx = match self.async_rx.lock() {
            Ok(guard) => guard,
            Err(_) => return Ok(out),
        };

        while let Ok(resolution) = rx.try_recv() {
            let cancelled = self
                .pending_placeholders
                .remove(&resolution.suspense_id)
                .map(|(_, record)| record.cancelled.load(Ordering::Relaxed))
                .unwrap_or(true);

            if cancelled {
                continue;
            }

            // Per the Phase-D wire amendment in `src/ir/opcode.rs`, the
            // `Patch` ships in its own frame and the resolved opcodes are
            // the remaining instructions in the same frame. An empty
            // `InstructionRange` signals that contract to the client.
            let mut instructions = Vec::with_capacity(resolution.instructions.len() + 1);
            instructions.push(Instruction::Patch {
                suspense_id: resolution.suspense_id,
                range: InstructionRange::try_new(0, 0)?,
            });
            instructions.extend(resolution.instructions);

            out.push(self.build_patches_chunk(instructions)?);
        }

        Ok(out)
    }

    /// Builds an `OpcodeFrame` from `instructions`, allocates a fresh
    /// `frame_id` against the patches stream, encodes, and routes
    /// through the stream router. Shared by `enqueue_async_island` and
    /// `drain_async_patch_chunks` so the wire shape stays in one place.
    fn build_patches_chunk(
        &self,
        instructions: Vec<Instruction>,
    ) -> Result<LaneRenderedChunk, RuntimePipelineError> {
        let frame_id = self
            .stream_router
            .muxer
            .allocate_sequence(WT_STREAM_SLOT_PATCHES as usize)
            .unwrap_or(0);
        let frame = OpcodeFrame {
            frame_id,
            component_id: None,
            instructions,
        };
        let wire_bytes = wire::encode_frame(&frame)?;
        Ok(self
            .stream_router
            .route_global_chunk(WTRenderMode::Patch, wire_bytes))
    }

    /// Converts the opcode emission results from the most recent
    /// [`Self::tick_frame`] call into [`LaneRenderedChunk`]s ready for
    /// the WebTransport patches stream.
    ///
    /// The returned chunk's `lane` field is the **WT stream slot**
    /// (always [`crate::runtime::webtransport::WT_STREAM_SLOT_PATCHES`]
    /// for opcode emission), not the highway lane that produced it.
    /// Routing through [`WTStreamRouter::route_component_chunk`] keeps
    /// the patch_sequence counters in lockstep with the muxer so a later
    /// `mux_lane_chunks` builds well-attributed `WebTransportFrame`s.
    ///
    /// The highway lane (0..3) is an internal scheduling detail. Once a
    /// chunk crosses the runtime → transport boundary the only relevant
    /// dimension is which WT stream it lands on.
    pub fn drain_opcode_chunks(&self) -> Vec<LaneRenderedChunk> {
        let mut chunks = Vec::new();

        // Phase D: resolved async islands first. Cancelled or unknown
        // resolutions are dropped silently; errors collapsing to an
        // empty Vec keep the tick loop alive even if the patches stream
        // hits a transient routing problem.
        if let Ok(async_chunks) = self.drain_async_patch_chunks() {
            chunks.extend(async_chunks);
        }

        // Per-tick patch frames from the synchronous emitter.
        for result in self.frame_arena.opcode_results() {
            let payload = FramePayload::Binary(result.wire_bytes.clone());
            let chunk = match result.component_id {
                Some(id) => self.stream_router.route_component_chunk(
                    ComponentId::new(id),
                    WTRenderMode::Patch,
                    payload,
                ),
                None => self
                    .stream_router
                    .route_global_chunk(WTRenderMode::Patch, payload),
            };
            chunks.push(chunk);
        }

        // Phase D: Placeholders queued by `enqueue_async_island` this
        // tick. Drained last so the client sees them after any
        // resolutions from prior ticks have already replaced earlier
        // placeholders.
        if let Ok(mut queued) = self.pending_placeholder_emissions.lock() {
            chunks.append(&mut queued);
        }

        chunks
    }

    /// Reconciles intern tables and returns a binary [`LaneRenderedChunk`]
    /// for the control stream if any changes were detected.
    pub fn drain_intern_table_patches<F>(
        &mut self,
        classify: F,
    ) -> Result<Option<LaneRenderedChunk>, RuntimePipelineError>
    where
        F: Fn(u16, &str) -> Option<crate::ir::opcode::InternTableKind>,
    {
        let instructions = self.reconcile_intern_tables(classify);
        self.frame_opcode_instructions_for_patches(instructions)
    }

    /// Builds the one-shot bootstrap intern-table chunk for a new bakabox
    /// session.
    ///
    /// The returned [`LaneRenderedChunk`] is destined for
    /// [`crate::runtime::webtransport::WT_STREAM_SLOT_CONTROL`] and carries
    /// `Instruction::InitInternTable` entries for every kind that
    /// `classify` maps a string into. `None` is returned when the column
    /// store holds no classifiable strings — sending an empty bootstrap
    /// would still be valid wire, but skipping the message keeps the
    /// control stream quiet on cold-start sessions with no intern state.
    ///
    /// This is the symmetrical counterpart to [`Self::drain_intern_table_patches`]
    /// for session init. Servers should call this exactly once per WT session
    /// (right after `session_init` on the control stream) and rely on
    /// `drain_intern_table_patches` for every subsequent reconcile tick.
    ///
    /// The internal baseline snapshot is also refreshed here, so a
    /// subsequent `drain_intern_table_patches` against the same state will
    /// see no drift and return `None`. Without this refresh, the first
    /// patch-diff after bootstrap would resurface the bootstrap entries as
    /// "new" — silently doubling the wire bytes on session warmup.
    pub fn drain_bootstrap_intern_chunk<F>(
        &mut self,
        classify: F,
    ) -> Result<Option<LaneRenderedChunk>, RuntimePipelineError>
    where
        F: Fn(u16, &str) -> Option<crate::ir::opcode::InternTableKind>,
    {
        let snapshot = InternTableSnapshot::capture(self.columns.strings(), &classify);
        let instructions = emitter::bootstrap_intern_tables(&snapshot);
        self.prev_intern_snapshot = snapshot;
        self.frame_opcode_instructions_for_patches(instructions)
    }

    /// Shared helper: wraps an opcode instruction batch into an
    /// [`OpcodeFrame`], encodes it, and routes the binary chunk onto
    /// the WebTransport patches stream. Returns `None` when there is
    /// nothing to ship.
    ///
    /// All binary opcode traffic — bootstrap intern tables, incremental
    /// intern patches, the per-tick patch frames — rides
    /// [`crate::runtime::webtransport::WT_STREAM_SLOT_PATCHES`]. The
    /// control slot (slot 0) is reserved for JSON session envelopes
    /// (`session_init`, `keep_alive`, `stream_open`) so the bakabox
    /// client can treat slot 0 as pure-text and slot 2 as pure-binary,
    /// avoiding a per-message kind discriminator.
    ///
    /// Centralising the frame_id allocation and wire encoding in one
    /// place keeps the contract `(instructions) -> chunk` provable; the
    /// bootstrap and patch paths are byte-identical past the
    /// instruction-vector boundary.
    fn frame_opcode_instructions_for_patches(
        &self,
        instructions: Vec<crate::ir::opcode::Instruction>,
    ) -> Result<Option<LaneRenderedChunk>, RuntimePipelineError> {
        if instructions.is_empty() {
            return Ok(None);
        }

        let stream_id = crate::runtime::webtransport::WT_STREAM_SLOT_PATCHES as usize;
        let frame_id = self
            .stream_router
            .muxer
            .allocate_sequence(stream_id)
            .unwrap_or(0);

        let frame = crate::ir::opcode::OpcodeFrame {
            frame_id,
            component_id: None,
            instructions,
        };

        let wire_bytes = crate::ir::wire::encode_frame(&frame)?;
        Ok(Some(self.stream_router.route_global_chunk(
            WTRenderMode::Patch,
            wire_bytes,
        )))
    }
}

/// Per-lane source-hash recompute. Folds `effects`, `priorities`, and
/// `phases` (bit-cast to u32) into FxHash so any mutation in those columns
/// flips the slot's `source_hash` and the next reconcile pass picks it up.
fn recompute_lane_source_hashes(pass: LaneColumnPass<'_>) {
    let LaneColumnPass {
        effects,
        source_hashes,
        priorities,
        phases,
        ..
    } = pass;
    let len = source_hashes.len();
    for i in 0..len {
        let mut hasher = FxHasher::default();
        effects.get(i).copied().unwrap_or(0).hash(&mut hasher);
        priorities
            .get(i)
            .copied()
            .unwrap_or(0.0)
            .to_bits()
            .hash(&mut hasher);
        phases
            .get(i)
            .copied()
            .unwrap_or(0.0)
            .to_bits()
            .hash(&mut hasher);
        source_hashes[i] = hasher.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Component;
    use std::f64::consts::PI;
    use std::time::Duration;

    fn analysis(id: ComponentId, phase: f64, priority: f64) -> ComponentAnalysis {
        ComponentAnalysis {
            id,
            priority,
            estimated_time_ms: 1.0,
            phase,
            topological_level: 0,
        }
    }

    #[test]
    fn test_pipeline_runs_scheduler_and_muxes_lane_frames() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));
        let id_b = graph.add_component(Component::new(ComponentId::new(0), "B".to_string()));
        graph.add_dependency(id_a, id_b).unwrap();

        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, PI + 0.2, 1.0));
        analyses.insert(id_b, analysis(id_b, 0.1, 2.0));
        let component_tiers = HashMap::from([(id_a, Tier::C), (id_b, Tier::B)]);

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            component_tiers,
            &[(id_b, RenderPriority::Critical)],
            SchedulerConfig {
                overtake_budget: Duration::from_secs(1),
                overtake_interval: 2,
                ..SchedulerConfig::default()
            },
            32,
        )
        .unwrap();

        pipeline.mark_hot_dirty(id_b);
        pipeline.submit_analyzer_result(id_a);
        let frame = pipeline.run_scheduler_frame();
        assert_eq!(frame.hot_set_rendered, 1);
        assert_eq!(frame.analyzer_forwarded, 1);

        let chunks = pipeline.drain_render_queue_to_lane_chunks();
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.lane
                    == super::super::webtransport::WT_STREAM_SLOT_PATCHES as usize)
        );
        let frames = pipeline.mux_lane_chunks(&chunks).unwrap();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn test_pipeline_tick_frame_drives_owned_bitmap_to_lane_patches() {
        use crate::runtime::highway::LANE_COUNT;

        let graph = ComponentGraph::new();
        let ids: Vec<ComponentId> = (0..4)
            .map(|i| graph.add_component(Component::new(ComponentId::new(0), format!("C{i}"))))
            .collect();

        // Phase values spread across the [0, 2π) circle so phase_to_lane
        // drops one component per lane.
        let phases = [0.1_f64, 1.7, 3.4, 5.0];
        let mut analyses = HashMap::new();
        for (id, phase) in ids.iter().zip(phases.iter()) {
            analyses.insert(*id, analysis(*id, *phase, 1.0));
        }
        let component_tiers = ids
            .iter()
            .map(|id| (*id, Tier::B))
            .collect::<HashMap<_, _>>();

        let mut pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            component_tiers,
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        assert_eq!(pipeline.columns().len(), 4);
        assert_eq!(pipeline.dirty_bitmap().capacity(), 4);

        let offsets = pipeline.columns().lane_offsets();
        for lane in 0..LANE_COUNT {
            let start = offsets.get(lane).copied().unwrap_or(0);
            let end = offsets.get(lane.saturating_add(1)).copied().unwrap_or(0);
            assert_eq!(
                end.saturating_sub(start),
                1,
                "each lane should receive exactly one component (lane={lane})"
            );
        }

        for id in &ids {
            assert!(
                pipeline.mark_component_dirty(*id),
                "marking a known id must succeed"
            );
        }

        let report = pipeline.tick_frame();
        assert_eq!(report.dirty_count, 4);
        assert_eq!(report.frames_pushed, LANE_COUNT);
        assert!(report.lane_sequences.iter().all(Option::is_some));
        assert_eq!(pipeline.frame_metrics().total_ticks(), 1);
        assert!(pipeline.frame_metrics().sample_count() >= 1);

        let baseline_capacity = pipeline.frame_arena().scratch_capacity();
        for id in &ids {
            pipeline.mark_component_dirty(*id);
        }
        let _ = pipeline.tick_frame();
        assert_eq!(
            pipeline.frame_arena().scratch_capacity(),
            baseline_capacity,
            "subsequent ticks must not reallocate the arena"
        );
        assert_eq!(pipeline.frame_metrics().total_ticks(), 2);
    }

    #[test]
    fn test_pipeline_mark_component_dirty_rejects_unknown_id() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));

        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, 0.1, 1.0));

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id_a, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        assert!(!pipeline.mark_component_dirty(ComponentId::new(u64::MAX)));
        assert!(pipeline.mark_component_dirty(id_a));
    }

    #[test]
    fn test_reconcile_source_hashes_marks_only_mutated_slots() {
        let graph = ComponentGraph::new();
        let ids: Vec<ComponentId> = (0..4)
            .map(|i| graph.add_component(Component::new(ComponentId::new(0), format!("C{i}"))))
            .collect();
        let phases = [0.1_f64, 1.7, 3.4, 5.0];
        let mut analyses = HashMap::new();
        for (id, phase) in ids.iter().zip(phases.iter()) {
            analyses.insert(*id, analysis(*id, *phase, 1.0));
        }
        let component_tiers = ids
            .iter()
            .map(|id| (*id, Tier::B))
            .collect::<HashMap<_, _>>();

        let mut pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            component_tiers,
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        // Baseline: snapshot already mirrors columns, so reconcile is a no-op.
        assert_eq!(pipeline.reconcile_source_hashes(), 0);
        assert_eq!(pipeline.dirty_bitmap().count_set(), 0);

        // Mutate one column's source hash and confirm exactly one bit is marked.
        pipeline.columns.column_pass_mut().source_hashes[0] = 0xDEAD_BEEF;
        assert_eq!(pipeline.reconcile_source_hashes(), 1);
        let mut drained = Vec::new();
        pipeline.dirty_bitmap().drain(|idx| drained.push(idx));
        assert_eq!(drained, vec![0]);

        // Snapshot must have been refreshed — second reconcile sees no drift.
        assert_eq!(pipeline.reconcile_source_hashes(), 0);
        assert_eq!(pipeline.dirty_bitmap().count_set(), 0);
    }

    #[test]
    fn test_pipeline_dispatches_cross_lane_signals() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));
        let id_b = graph.add_component(Component::new(ComponentId::new(0), "B".to_string()));
        graph.add_dependency(id_a, id_b).unwrap();

        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, PI + 0.2, 1.0));
        analyses.insert(id_b, analysis(id_b, 0.1, 2.0));
        let component_tiers = HashMap::from([(id_a, Tier::B), (id_b, Tier::C)]);

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            component_tiers,
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        let routed = pipeline.dispatch_cross_lane_dependency_signals();
        assert!(routed >= 1);
        let drained = pipeline.drain_inter_lane_messages(2);
        assert!(!drained.is_empty());
    }

    #[test]
    fn pipeline_runtime_handle_is_none_by_default() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));
        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, 0.1, 1.0));

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id_a, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        assert!(
            pipeline.runtime_handle().is_none(),
            "fresh pipeline must not carry a runtime handle"
        );
        assert!(
            matches!(
                pipeline.require_runtime_handle(),
                Err(RuntimePipelineError::MissingRuntimeHandle)
            ),
            "require_runtime_handle must surface MissingRuntimeHandle when unbound"
        );
    }

    #[test]
    fn pipeline_with_runtime_handle_binds_and_exposes_it() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("must build tokio runtime for test");
        let handle = runtime.handle().clone();

        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));
        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, 0.1, 1.0));

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id_a, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap()
        .with_runtime_handle(handle);

        assert!(
            pipeline.runtime_handle().is_some(),
            "with_runtime_handle must bind the handle"
        );
        assert!(
            pipeline.require_runtime_handle().is_ok(),
            "require_runtime_handle must return the bound handle"
        );
    }

    // ── Phase D — async island tests ───────────────────────────────────

    use crate::ir::opcode::{Instruction, StableId, SuspenseId, TagId};
    use crate::ir::wire::decode_frame;
    use crate::runtime::webtransport::{FramePayload, WT_STREAM_SLOT_PATCHES};

    /// Builds a minimal pipeline with a bound runtime handle suitable for
    /// driving the async-island surface in tests. Returns both the
    /// pipeline and the tokio runtime so the runtime can drive resolver
    /// Futures via `block_on`.
    fn build_async_pipeline() -> (FourLaneRuntimePipeline, tokio::runtime::Runtime) {
        // Multi-thread runtime so spawned resolver Futures run on a worker;
        // the test thread can then `block_on(sleep)` to give them time to
        // land their resolutions on the pipeline's mpsc.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("test runtime must build");
        let handle = runtime.handle().clone();

        let graph = ComponentGraph::new();
        let id = graph.add_component(Component::new(ComponentId::new(0), "Async".to_string()));
        let mut analyses = HashMap::new();
        analyses.insert(id, analysis(id, 0.1, 1.0));

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .expect("pipeline must build")
        .with_runtime_handle(handle);

        (pipeline, runtime)
    }

    fn decoded_instructions(chunk: &LaneRenderedChunk) -> Vec<Instruction> {
        let bytes = match &chunk.payload {
            FramePayload::Binary(b) => b.clone(),
            FramePayload::Text(_) => panic!("opcode chunks must be Binary"),
        };
        let (frame, _) = decode_frame(&bytes).expect("chunk must decode");
        frame.instructions
    }

    #[test]
    fn enqueue_async_island_emits_placeholder_chunk() {
        let (pipeline, _runtime) = build_async_pipeline();

        let suspense_id = pipeline
            .enqueue_async_island(StableId(7), async { Vec::new() })
            .expect("enqueue must succeed when runtime handle is bound");
        assert_eq!(suspense_id, SuspenseId(0));
        assert_eq!(pipeline.pending_async_count(), 1);

        let chunks = pipeline.drain_opcode_chunks();
        assert_eq!(chunks.len(), 1, "exactly one Placeholder chunk this tick");
        assert_eq!(chunks[0].lane as u8, WT_STREAM_SLOT_PATCHES);

        let instructions = decoded_instructions(&chunks[0]);
        assert_eq!(
            instructions,
            vec![Instruction::Placeholder {
                stable_id: StableId(7),
                suspense_id: SuspenseId(0),
            }]
        );
    }

    #[test]
    fn resolved_future_ships_patch_frame_with_resolution() {
        let (pipeline, runtime) = build_async_pipeline();

        let suspense_id = pipeline
            .enqueue_async_island(StableId(11), async {
                vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(11),
                }]
            })
            .expect("enqueue must succeed");

        // Drain the placeholder so it doesn't show up in the next drain.
        let _ = pipeline.drain_opcode_chunks();

        // Drive the spawned resolver to completion. `block_on` returning
        // a stable value of `Vec::new()` lets the runtime tick the
        // spawned task; after the await point the task has already sent
        // its resolution into the pipeline's mpsc.
        // Give the worker thread a tick to await the resolver and send
        // its resolution through the mpsc channel.
        runtime.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        });

        let chunks = pipeline.drain_opcode_chunks();
        assert_eq!(chunks.len(), 1, "patch frame must ship on the next drain");
        let instructions = decoded_instructions(&chunks[0]);
        assert_eq!(instructions.len(), 2, "Patch + one resolved Create");
        match &instructions[0] {
            Instruction::Patch { suspense_id: sid, .. } => assert_eq!(*sid, suspense_id),
            other => panic!("first instruction must be Patch, got {:?}", other),
        }
        assert!(matches!(
            instructions[1],
            Instruction::Create { stable_id: StableId(11), .. }
        ));
        assert_eq!(
            pipeline.pending_async_count(),
            0,
            "draining the patch must clear the pending entry"
        );
    }

    #[test]
    fn cancelled_async_island_drops_resolution_silently() {
        let (pipeline, runtime) = build_async_pipeline();

        pipeline
            .enqueue_async_island(StableId(42), async {
                vec![Instruction::SetText {
                    stable_id: StableId(42),
                    text: b"never shipped".to_vec(),
                }]
            })
            .expect("enqueue must succeed");

        // Caller decides the placeholder is no longer needed (e.g. the
        // component was removed) BEFORE the resolver lands.
        let cancelled = pipeline.cancel_pending_async_for(StableId(42));
        assert_eq!(cancelled, 1, "exactly one pending island must be marked");

        // Drain placeholder; this is the Placeholder chunk from enqueue.
        let _ = pipeline.drain_opcode_chunks();

        // Drive the resolver to completion.
        // Give the worker thread a tick to await the resolver and send
        // its resolution through the mpsc channel.
        runtime.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        });

        let chunks = pipeline.drain_opcode_chunks();
        assert!(
            chunks.is_empty(),
            "cancelled resolution must not ship a Patch; got {} chunks",
            chunks.len()
        );
        assert_eq!(pipeline.pending_async_count(), 0);
    }

    #[test]
    fn enqueue_without_runtime_handle_surfaces_typed_error() {
        let graph = ComponentGraph::new();
        let id = graph.add_component(Component::new(ComponentId::new(0), "X".to_string()));
        let mut analyses = HashMap::new();
        analyses.insert(id, analysis(id, 0.1, 1.0));

        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .unwrap();

        let result = pipeline.enqueue_async_island(StableId(1), async { Vec::new() });
        assert!(matches!(
            result,
            Err(RuntimePipelineError::MissingRuntimeHandle)
        ));
        assert_eq!(
            pipeline.pending_async_count(),
            0,
            "no pending entry must be left behind when spawn fails"
        );
    }

    #[test]
    fn suspense_ids_are_monotonic_across_enqueues() {
        let (pipeline, _runtime) = build_async_pipeline();
        let a = pipeline.enqueue_async_island(StableId(1), async { Vec::new() }).unwrap();
        let b = pipeline.enqueue_async_island(StableId(2), async { Vec::new() }).unwrap();
        let c = pipeline.enqueue_async_island(StableId(3), async { Vec::new() }).unwrap();
        assert_eq!(a, SuspenseId(0));
        assert_eq!(b, SuspenseId(1));
        assert_eq!(c, SuspenseId(2));
    }
}
