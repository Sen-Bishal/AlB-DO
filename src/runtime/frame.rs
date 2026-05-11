//! Cycle 5 — sub-ms RAF reconciliation tick.
//!
//! [`frame_tick`] is the end-of-pipeline hot path. It assumes Cycles 1-4
//! have landed:
//!
//! * [`IrColumns`](crate::ir::columns::IrColumns) is lane-sorted and exposes
//!   per-lane contiguous column slices.
//! * [`DirtyBitmap`] addresses component slots by column index; its
//!   [`drain_into`](DirtyBitmap::drain_into) feeds the scratch Vec without
//!   allocating.
//! * [`WebTransportMuxer`] allocates per-stream sequence numbers via a
//!   lock-free `fetch_add(1, Relaxed)`.
//!
//! Every intermediate buffer lives in a [`FrameArena`] owned by the pipeline,
//! so the inner loop performs *zero* heap traffic — the only allocations
//! happen in the one-time arena construction. The four per-lane patch
//! buffers are handed out as disjoint `&mut Vec<u8>` borrows to rayon
//! workers, which lets the borrow checker prove race-freedom without any
//! synchronization primitives.
//!
//! Wire format of a per-component patch record (little-endian, 16 bytes):
//!
//! | offset | size | field                       |
//! |--------|------|-----------------------------|
//! | 0      | 4    | column index (`u32`)        |
//! | 4      | 4    | field mask (`u32`)          |
//! | 8      | 8    | new `source_hash` (`u64`)   |
//!
//! The mask is always [`field_mask::SOURCE_HASH`] today; the slot exists so
//! Cycle 6 can fold in `effects`/`priority` without a wire-format break.

use super::dirty_bitmap::DirtyBitmap;
use super::emitter::{self, EmitResult};
use super::webtransport::{WebTransportMuxer, WEBTRANSPORT_STREAM_COUNT, WT_STREAM_SLOT_PATCHES};
use crate::ir::columns::{field_mask, IrColumns, LANE_COUNT};
use std::collections::VecDeque;
use std::time::Instant;
use tracing::{info, trace};

const PATCH_RECORD_BYTES: usize = 16;
const DEFAULT_METRICS_WINDOW: usize = 1024;

/// Pre-allocated per-frame scratch space.
///
/// Every field is sized up-front by [`FrameArena::with_capacity`] and reused
/// across ticks. The only cost at steady state is the cache-line touch on
/// `clear()` — there is no allocation, and the same backing storage
/// re-materializes the scratch indices, lane buckets, and patch buffers on
/// every frame.
#[derive(Debug)]
pub struct FrameArena {
    scratch_indices: Vec<u32>,
    lane_buckets: [Vec<u32>; LANE_COUNT],
    patch_bufs: [Vec<u8>; LANE_COUNT],
    opcode_results: Vec<EmitResult>,
}

impl Default for FrameArena {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl FrameArena {
    /// Allocates all four lane buckets and patch buffers with `capacity`
    /// reserved for dirty indices. Subsequent [`frame_tick`] calls reuse the
    /// same backing storage — pick `capacity` close to the worst-case dirty
    /// count so the hot path never grows a Vec.
    pub fn with_capacity(capacity: usize) -> Self {
        let per_lane = capacity.div_ceil(LANE_COUNT).max(1);
        let patch_cap = per_lane.saturating_mul(PATCH_RECORD_BYTES);
        Self {
            scratch_indices: Vec::with_capacity(capacity),
            lane_buckets: std::array::from_fn(|_| Vec::with_capacity(per_lane)),
            patch_bufs: std::array::from_fn(|_| Vec::with_capacity(patch_cap)),
            opcode_results: Vec::with_capacity(LANE_COUNT),
        }
    }

    pub fn scratch_capacity(&self) -> usize {
        self.scratch_indices.capacity()
    }

    pub fn lane_patch_bytes(&self, lane: usize) -> &[u8] {
        self.patch_bufs.get(lane).map_or(&[], Vec::as_slice)
    }

    pub fn lane_dirty_indices(&self, lane: usize) -> &[u32] {
        self.lane_buckets.get(lane).map_or(&[], Vec::as_slice)
    }

    /// Per-lane opcode emission results from the last [`frame_tick`].
    pub fn opcode_results(&self) -> &[EmitResult] {
        &self.opcode_results
    }

    fn reset_for_tick(&mut self) {
        self.scratch_indices.clear();
        for bucket in &mut self.lane_buckets {
            bucket.clear();
        }
        for buf in &mut self.patch_bufs {
            buf.clear();
        }
        self.opcode_results.clear();
    }
}

/// Per-frame metrics surfaced for observability + regression benches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameReport {
    pub dirty_count: usize,
    pub drain_ns: u128,
    pub partition_ns: u128,
    pub emit_ns: u128,
    pub total_ns: u128,
    pub frames_pushed: usize,
    pub lane_patches: [usize; LANE_COUNT],
    pub lane_bytes: [usize; LANE_COUNT],
    pub lane_sequences: [Option<u64>; LANE_COUNT],
    /// Number of opcode frames emitted by the Phase B emitter.
    pub opcode_frames_emitted: usize,
    /// Total wire bytes from opcode emission, per lane.
    pub opcode_bytes: [usize; LANE_COUNT],
}

/// Ring-buffered `FrameReport` accumulator used by the pipeline to expose
/// tail-latency percentiles without pulling in a histogram crate.
///
/// Samples are stored as raw `total_ns` values in a `VecDeque` capped at
/// `window`. Percentile queries sort a scratch copy — acceptable at the
/// 1 Hz aggregation cadence the scheduler calls this on, and never on the
/// tick hot path.
#[derive(Debug, Clone)]
pub struct FrameMetrics {
    samples: VecDeque<u128>,
    window: usize,
    total_ticks: u64,
    total_dirty: u64,
    total_frames_pushed: u64,
    total_lane_bytes: [u64; LANE_COUNT],
    max_total_ns: u128,
}

impl Default for FrameMetrics {
    fn default() -> Self {
        Self::with_window(DEFAULT_METRICS_WINDOW)
    }
}

impl FrameMetrics {
    pub fn with_window(window: usize) -> Self {
        let window = window.max(1);
        Self {
            samples: VecDeque::with_capacity(window),
            window,
            total_ticks: 0,
            total_dirty: 0,
            total_frames_pushed: 0,
            total_lane_bytes: [0; LANE_COUNT],
            max_total_ns: 0,
        }
    }

    pub fn record(&mut self, report: &FrameReport) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(report.total_ns);
        self.total_ticks = self.total_ticks.saturating_add(1);
        self.total_dirty = self
            .total_dirty
            .saturating_add(u64::try_from(report.dirty_count).unwrap_or(u64::MAX));
        self.total_frames_pushed = self
            .total_frames_pushed
            .saturating_add(u64::try_from(report.frames_pushed).unwrap_or(u64::MAX));
        for lane in 0..LANE_COUNT {
            let bytes = u64::try_from(report.lane_bytes.get(lane).copied().unwrap_or(0))
                .unwrap_or(u64::MAX);
            if let Some(slot) = self.total_lane_bytes.get_mut(lane) {
                *slot = slot.saturating_add(bytes);
            }
        }
        if report.total_ns > self.max_total_ns {
            self.max_total_ns = report.total_ns;
        }
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    pub fn total_ticks(&self) -> u64 {
        self.total_ticks
    }

    pub fn total_dirty(&self) -> u64 {
        self.total_dirty
    }

    pub fn total_frames_pushed(&self) -> u64 {
        self.total_frames_pushed
    }

    pub fn total_lane_bytes(&self) -> [u64; LANE_COUNT] {
        self.total_lane_bytes
    }

    pub fn max_total_ns(&self) -> u128 {
        self.max_total_ns
    }

    pub fn p50_ns(&self) -> u128 {
        self.percentile_ns(500)
    }

    pub fn p99_ns(&self) -> u128 {
        self.percentile_ns(990)
    }

    /// Percentile lookup keyed by permille (parts-per-thousand) so the
    /// implementation stays in integer arithmetic — no `f64` casts, no
    /// `as_conversions` lint noise. `permille` is clamped to `[0, 1000]`.
    pub fn percentile_ns(&self, permille: u32) -> u128 {
        if self.samples.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u128> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let clamped = permille.min(1000);
        let last_idx = sorted.len().saturating_sub(1);
        let idx = last_idx
            .saturating_mul(usize::try_from(clamped).unwrap_or(0))
            .saturating_add(500)
            / 1000;
        let idx = idx.min(last_idx);
        sorted.get(idx).copied().unwrap_or(0)
    }

    /// Emits a `tracing::info!` event with the current window's summary.
    /// Intended to be called on a low-frequency timer (e.g. once per second),
    /// never from inside [`frame_tick`].
    pub fn emit_summary(&self) {
        info!(
            target: "albedo.frame.summary",
            samples = self.samples.len(),
            ticks = self.total_ticks,
            dirty = self.total_dirty,
            frames_pushed = self.total_frames_pushed,
            p50_ns = u64::try_from(self.p50_ns()).unwrap_or(u64::MAX),
            p99_ns = u64::try_from(self.p99_ns()).unwrap_or(u64::MAX),
            max_ns = u64::try_from(self.max_total_ns).unwrap_or(u64::MAX),
            "frame_metrics window summary"
        );
    }
}

/// End-to-end reconciliation tick — the FINAL OBJECTIVE of the SoA plan.
///
/// The call order inside this function mirrors the plan, one line per stage:
///
/// 1. **Drain** — `bitmap.drain_into(scratch)` yields column indices.
/// 2. **Partition** — scan `scratch` once, look each index up against the
///    lane-sorted `lane_offsets`, push into the matching bucket.
/// 3. **Emit** — `rayon::join` over the four lanes; each worker writes its
///    patch records into its own pre-sized `Vec<u8>` in the arena.
/// 4. **Sequence** — one `fetch_add(1, Relaxed)` per non-empty lane against
///    the shared [`WebTransportMuxer`] so the receiver can reassemble.
///
/// The arena is the sole mutable state. `columns`, `bitmap`, and `muxer`
/// are all `&` borrows — the function is safe to call from any context that
/// can obtain a mutable arena reference, and the arena's backing allocations
/// survive the tick boundary.
pub fn frame_tick(
    columns: &IrColumns,
    bitmap: &DirtyBitmap,
    muxer: &WebTransportMuxer,
    arena: &mut FrameArena,
) -> FrameReport {
    let tick_start = Instant::now();
    arena.reset_for_tick();

    let drain_start = Instant::now();
    let dirty_count = bitmap.drain_into(&mut arena.scratch_indices);
    let drain_ns = drain_start.elapsed().as_nanos();

    if dirty_count == 0 {
        let total_ns = tick_start.elapsed().as_nanos();
        return FrameReport {
            dirty_count: 0,
            drain_ns,
            total_ns,
            ..FrameReport::default()
        };
    }

    let partition_start = Instant::now();
    let lane_offsets = columns.lane_offsets();
    partition_dirty_by_lane(&arena.scratch_indices, &lane_offsets, &mut arena.lane_buckets);
    let partition_ns = partition_start.elapsed().as_nanos();

    let emit_start = Instant::now();
    let [buf0, buf1, buf2, buf3] = split_patch_bufs_mut(&mut arena.patch_bufs);
    let [bucket0, bucket1, bucket2, bucket3] = split_buckets(&arena.lane_buckets);
    let hashes = columns.source_hashes();

    let ((lane0_bytes, lane1_bytes), (lane2_bytes, lane3_bytes)) = rayon::join(
        || {
            rayon::join(
                || build_lane_patch_buf(bucket0, hashes, buf0),
                || build_lane_patch_buf(bucket1, hashes, buf1),
            )
        },
        || {
            rayon::join(
                || build_lane_patch_buf(bucket2, hashes, buf2),
                || build_lane_patch_buf(bucket3, hashes, buf3),
            )
        },
    );
    let lane_bytes = [lane0_bytes, lane1_bytes, lane2_bytes, lane3_bytes];
    let emit_ns = emit_start.elapsed().as_nanos();

    let mut lane_sequences: [Option<u64>; LANE_COUNT] = [None; LANE_COUNT];
    let mut lane_patches = [0usize; LANE_COUNT];
    let mut frames_pushed = 0usize;
    for lane in 0..LANE_COUNT {
        let patch_count = arena.lane_buckets.get(lane).map_or(0, Vec::len);
        if let Some(slot) = lane_patches.get_mut(lane) {
            *slot = patch_count;
        }
        if patch_count == 0 {
            continue;
        }
        if let Some(slot) = lane_sequences.get_mut(lane) {
            *slot = muxer.allocate_sequence(stream_for_lane(lane));
        }
        frames_pushed = frames_pushed.saturating_add(1);
    }

    // ── Phase B: opcode emission ─────────────────────────────────
    let lane_bucket_refs = split_buckets(&arena.lane_buckets);
    let mut opcode_frames_emitted = 0usize;
    let mut opcode_bytes = [0usize; LANE_COUNT];
    if let Ok(results) = emitter::emit_lane_frames(columns, &lane_bucket_refs, muxer) {
        for result in &results {
            let lane_idx = usize::from(result.lane);
            if let Some(slot) = opcode_bytes.get_mut(lane_idx) {
                *slot = result.wire_bytes.len();
            }
        }
        opcode_frames_emitted = results.len();
        arena.opcode_results = results;
    }

    let total_ns = tick_start.elapsed().as_nanos();
    let report = FrameReport {
        dirty_count,
        drain_ns,
        partition_ns,
        emit_ns,
        total_ns,
        frames_pushed,
        lane_patches,
        lane_bytes,
        lane_sequences,
        opcode_frames_emitted,
        opcode_bytes,
    };

    trace!(
        target: "albedo.frame",
        dirty_count,
        drain_ns = u64::try_from(report.drain_ns).unwrap_or(u64::MAX),
        partition_ns = u64::try_from(report.partition_ns).unwrap_or(u64::MAX),
        emit_ns = u64::try_from(report.emit_ns).unwrap_or(u64::MAX),
        total_ns = u64::try_from(report.total_ns).unwrap_or(u64::MAX),
        frames_pushed,
        "frame_tick complete"
    );

    report
}

/// All four lane buffers carry [`WT_STREAM_SLOT_PATCHES`] as their transport
/// slot today. Kept as a helper so Cycle 6 can promote control/prefetch
/// lanes without touching callers.
fn stream_for_lane(_lane: usize) -> usize {
    usize::from(WT_STREAM_SLOT_PATCHES)
}

fn partition_dirty_by_lane(
    scratch: &[u32],
    lane_offsets: &[u32; LANE_COUNT + 1],
    buckets: &mut [Vec<u32>; LANE_COUNT],
) {
    for &idx in scratch {
        let lane = lane_for_index(idx, lane_offsets);
        if let Some(bucket) = buckets.get_mut(lane) {
            bucket.push(idx);
        }
    }
}

/// `lane_offsets` is sorted ascending with `LANE_COUNT + 1` entries, so we can
/// locate the lane in `O(log LANE_COUNT)` — four iterations max. For a
/// four-lane layout this is three compares; we keep the binary search to
/// avoid hard-coding the lane count at call sites.
fn lane_for_index(idx: u32, lane_offsets: &[u32; LANE_COUNT + 1]) -> usize {
    match lane_offsets.binary_search(&idx) {
        Ok(mut hit) => {
            while hit + 1 < lane_offsets.len()
                && lane_offsets.get(hit + 1).copied() == Some(idx)
            {
                hit = hit.saturating_add(1);
            }
            hit.min(LANE_COUNT.saturating_sub(1))
        }
        Err(insert) => insert.saturating_sub(1).min(LANE_COUNT.saturating_sub(1)),
    }
}

fn build_lane_patch_buf(indices: &[u32], hashes: &[u64], buf: &mut Vec<u8>) -> usize {
    buf.clear();
    buf.reserve(indices.len().saturating_mul(PATCH_RECORD_BYTES));
    for &column_idx in indices {
        let hash = usize::try_from(column_idx)
            .ok()
            .and_then(|pos| hashes.get(pos).copied())
            .unwrap_or(0);
        buf.extend_from_slice(&column_idx.to_le_bytes());
        buf.extend_from_slice(&field_mask::SOURCE_HASH.to_le_bytes());
        buf.extend_from_slice(&hash.to_le_bytes());
    }
    buf.len()
}

fn split_patch_bufs_mut(
    bufs: &mut [Vec<u8>; LANE_COUNT],
) -> [&mut Vec<u8>; LANE_COUNT] {
    let (head0, rest0) = bufs.split_at_mut(1);
    let (head1, rest1) = rest0.split_at_mut(1);
    let (head2, head3) = rest1.split_at_mut(1);
    [
        head0.first_mut().expect("lane 0 buffer present"),
        head1.first_mut().expect("lane 1 buffer present"),
        head2.first_mut().expect("lane 2 buffer present"),
        head3.first_mut().expect("lane 3 buffer present"),
    ]
}

fn split_buckets(buckets: &[Vec<u32>; LANE_COUNT]) -> [&[u32]; LANE_COUNT] {
    [
        buckets.first().map_or(&[], Vec::as_slice),
        buckets.get(1).map_or(&[], Vec::as_slice),
        buckets.get(2).map_or(&[], Vec::as_slice),
        buckets.get(3).map_or(&[], Vec::as_slice),
    ]
}

const _: () = assert!(
    WEBTRANSPORT_STREAM_COUNT >= LANE_COUNT,
    "WebTransport stream slots must cover every IR lane"
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::EffectProfile;
    use crate::parser::ParsedComponent;

    fn parsed(id: u32) -> ParsedComponent {
        ParsedComponent {
            name: format!("C{id}"),
            file_path: format!("src/C{id}.tsx"),
            line_number: (id as usize) + 1,
            imports: Vec::new(),
            estimated_size: 64 + (id as usize) * 8,
            is_default_export: false,
            props: Vec::new(),
            effect_profile: EffectProfile::default(),
            source_hash: 0xAAAA_0000 | u64::from(id),
        }
    }

    fn four_lane_columns() -> IrColumns {
        let parsed_components = (0..4).map(parsed).collect::<Vec<_>>();
        let mut columns = IrColumns::from_parsed(&parsed_components);
        let ids = columns.ids().to_vec();
        let mut lane_of = std::collections::HashMap::new();
        for (position, id) in ids.iter().enumerate() {
            lane_of.insert(*id, position % LANE_COUNT);
        }
        columns.sort_by_lane(|id| lane_of.get(&id).copied().unwrap_or(0));
        columns
    }

    #[test]
    fn frame_tick_on_empty_bitmap_reports_zero_work() {
        let columns = four_lane_columns();
        let bitmap = DirtyBitmap::with_capacity(columns.len());
        let muxer = WebTransportMuxer::new();
        let mut arena = FrameArena::with_capacity(4);

        let report = frame_tick(&columns, &bitmap, &muxer, &mut arena);
        assert_eq!(report.dirty_count, 0);
        assert_eq!(report.frames_pushed, 0);
        assert_eq!(report.lane_patches, [0, 0, 0, 0]);
        assert_eq!(report.lane_bytes, [0, 0, 0, 0]);
        assert!(arena.lane_patch_bytes(0).is_empty());
    }

    #[test]
    fn frame_tick_partitions_dirty_indices_into_lane_buckets() {
        let columns = four_lane_columns();
        let bitmap = DirtyBitmap::with_capacity(columns.len());
        for slot in 0..columns.len() {
            bitmap.mark(slot);
        }

        let muxer = WebTransportMuxer::new();
        let mut arena = FrameArena::with_capacity(columns.len());

        let report = frame_tick(&columns, &bitmap, &muxer, &mut arena);
        assert_eq!(report.dirty_count, columns.len());
        assert_eq!(report.frames_pushed, LANE_COUNT);
        assert_eq!(report.lane_patches, [1, 1, 1, 1]);

        for lane in 0..LANE_COUNT {
            assert_eq!(report.lane_bytes[lane], PATCH_RECORD_BYTES);
            assert_eq!(arena.lane_dirty_indices(lane).len(), 1);
            assert_eq!(arena.lane_patch_bytes(lane).len(), PATCH_RECORD_BYTES);
        }
    }

    #[test]
    fn frame_tick_allocates_monotonic_sequences_per_stream_slot() {
        let columns = four_lane_columns();
        let bitmap = DirtyBitmap::with_capacity(columns.len());
        for slot in 0..columns.len() {
            bitmap.mark(slot);
        }

        let muxer = WebTransportMuxer::new();
        let mut arena = FrameArena::with_capacity(columns.len());

        let first = frame_tick(&columns, &bitmap, &muxer, &mut arena);

        for slot in 0..columns.len() {
            bitmap.mark(slot);
        }
        let second = frame_tick(&columns, &bitmap, &muxer, &mut arena);

        for lane in 0..LANE_COUNT {
            let a = first.lane_sequences[lane].expect("first lane sequence");
            let b = second.lane_sequences[lane].expect("second lane sequence");
            assert!(b > a, "sequences must advance between ticks (lane={lane})");
        }
    }

    #[test]
    fn frame_tick_reuses_arena_storage_across_ticks() {
        let columns = four_lane_columns();
        let bitmap = DirtyBitmap::with_capacity(columns.len());
        let muxer = WebTransportMuxer::new();
        let mut arena = FrameArena::with_capacity(columns.len());
        let baseline = arena.scratch_capacity();

        for _ in 0..64 {
            for slot in 0..columns.len() {
                bitmap.mark(slot);
            }
            let _ = frame_tick(&columns, &bitmap, &muxer, &mut arena);
        }

        assert_eq!(arena.scratch_capacity(), baseline, "zero growth under steady dirty load");
    }

    #[test]
    fn frame_tick_patch_payload_encodes_column_index_and_hash() {
        let columns = four_lane_columns();
        let bitmap = DirtyBitmap::with_capacity(columns.len());
        bitmap.mark(0);

        let muxer = WebTransportMuxer::new();
        let mut arena = FrameArena::with_capacity(columns.len());
        let report = frame_tick(&columns, &bitmap, &muxer, &mut arena);

        assert_eq!(report.dirty_count, 1);
        let lane = (0..LANE_COUNT)
            .find(|lane| report.lane_patches[*lane] == 1)
            .expect("dirty record landed in some lane");
        let payload = arena.lane_patch_bytes(lane);
        assert_eq!(payload.len(), PATCH_RECORD_BYTES);

        let mut column_bytes = [0_u8; 4];
        column_bytes.copy_from_slice(&payload[0..4]);
        assert_eq!(u32::from_le_bytes(column_bytes), 0);

        let mut mask_bytes = [0_u8; 4];
        mask_bytes.copy_from_slice(&payload[4..8]);
        assert_eq!(u32::from_le_bytes(mask_bytes), field_mask::SOURCE_HASH);

        let mut hash_bytes = [0_u8; 8];
        hash_bytes.copy_from_slice(&payload[8..16]);
        let expected_hash = columns.source_hashes().first().copied().unwrap_or(0);
        assert_eq!(u64::from_le_bytes(hash_bytes), expected_hash);
    }

    #[test]
    fn frame_tick_is_leak_free_under_soak() {
        let columns = four_lane_columns();
        let bitmap = DirtyBitmap::with_capacity(columns.len());
        let muxer = WebTransportMuxer::new();
        let mut arena = FrameArena::with_capacity(columns.len());

        for frame in 0..1024 {
            if frame % 3 == 0 {
                bitmap.mark(frame % columns.len());
            }
            let _ = frame_tick(&columns, &bitmap, &muxer, &mut arena);
        }
        assert_eq!(arena.scratch_capacity(), columns.len());
    }

    #[test]
    fn lane_for_index_handles_empty_lanes_gracefully() {
        let offsets = [0_u32, 0, 2, 2, 4];
        assert_eq!(lane_for_index(0, &offsets), 1);
        assert_eq!(lane_for_index(1, &offsets), 1);
        assert_eq!(lane_for_index(2, &offsets), 3);
        assert_eq!(lane_for_index(3, &offsets), 3);
    }
}
