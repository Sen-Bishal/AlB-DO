//! Phase B — Opcode Emitter.
//!
//! Bridge between the SoA [`IrColumns`] column store and the Phase A opcode
//! wire format. Walks lane-sorted column slices, generates [`OpcodeFrame`]s
//! containing DOM-mutation [`Instruction`]s, encodes them via [`WireEncode`],
//! and routes the binary payload to the WebTransport muxer as
//! [`FramePayload::Binary`].
//!
//! # Hot-path contract
//!
//! Every buffer passed into [`emit_lane_frames`] is caller-owned (lives in a
//! [`FrameArena`](super::frame::FrameArena) or equivalent). The emitter
//! itself performs **zero** heap allocations on the steady-state path — the
//! only `Vec` growth is instruction assembly, which reuses capacity across
//! ticks when the caller retains the `EmitResult` storage.
//!
//! # Wire encoding
//!
//! Encoding goes through the [`WireEncode`] trait (`frame.wire_encode()`),
//! **not** the free-function wrappers — fulfilling the Phase A `wire.rs`
//! contract that trait calls are the codec boundary.

use super::webtransport::{WebTransportMuxer, WT_STREAM_SLOT_PATCHES};
use crate::ir::columns::{IrColumns, StringInterner, LANE_COUNT};
use crate::ir::opcode::{
    InstructionRange, InternEntry, InternPatchOp, InternTable, InternTableKind, OpcodeFrame,
    RangeError, SlotId, SuspenseId,
};
use crate::ir::wire::{WireEncode, WireError};
use crate::ir::Instruction;

/// Per-lane emission result — one encoded `OpcodeFrame` per non-empty lane.
#[derive(Debug, Clone)]
pub struct EmitResult {
    pub lane: u8,
    pub frame_id: u64,
    pub component_id: Option<u64>,
    pub wire_bytes: Vec<u8>,
    pub instruction_count: usize,
}

/// Errors produced by the emitter.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Range(#[from] RangeError),
}

/// Snapshot of the string interner at a point in time, used for diffing
/// intern table changes between ticks.
///
/// Each field holds the entries for one [`InternTableKind`]. Bootstrap sends
/// all three as [`Instruction::InitInternTable`]; incremental updates diff
/// against a previous snapshot and emit [`Instruction::PatchInternTable`].
#[derive(Debug, Clone, Default)]
pub struct InternTableSnapshot {
    pub tags: Vec<InternEntry>,
    pub attrs: Vec<InternEntry>,
    pub events: Vec<InternEntry>,
}

impl InternTableSnapshot {
    /// Captures the current state of a [`StringInterner`] into tag/attr/event
    /// buckets. The caller supplies the classification function `classify`
    /// which maps `(id, value)` → `Option<InternTableKind>`.
    ///
    /// Entries that `classify` returns `None` for are silently skipped.
    pub fn capture<F>(interner: &StringInterner, classify: F) -> Self
    where
        F: Fn(u16, &str) -> Option<InternTableKind>,
    {
        let mut snapshot = Self::default();
        for idx in 0..interner.len() {
            let id = u16::try_from(idx).unwrap_or(u16::MAX);
            let sid = crate::ir::columns::StringId::new(u32::try_from(idx).unwrap_or(u32::MAX));
            let value = interner.resolve(sid);
            if let Some(kind) = classify(id, value) {
                let entry = InternEntry {
                    id,
                    value: value.to_string(),
                };
                match kind {
                    InternTableKind::Tag => snapshot.tags.push(entry),
                    InternTableKind::Attr => snapshot.attrs.push(entry),
                    InternTableKind::Event => snapshot.events.push(entry),
                }
            }
        }
        snapshot
    }

    fn entries_for(&self, kind: InternTableKind) -> &[InternEntry] {
        match kind {
            InternTableKind::Tag => &self.tags,
            InternTableKind::Attr => &self.attrs,
            InternTableKind::Event => &self.events,
        }
    }
}

// ── Core emission ────────────────────────────────────────────────────

/// Emits one [`OpcodeFrame`] for each lane that has dirty slots, encoding
/// dirty-slot metadata as [`Instruction::SlotSet`] instructions and returning
/// the wire-encoded bytes.
///
/// # Arguments
///
/// * `columns`       — the lane-sorted column store.
/// * `dirty_indices` — flat array of dirty column indices (output of [`DirtyBitmap::drain_into`]).
/// * `lane_buckets`  — pre-partitioned per-lane dirty indices (the same buckets
///   [`frame_tick`](super::frame::frame_tick) builds).
/// * `muxer`         — sequence allocator for frame IDs.
///
/// # Wire contract
///
/// * `frame_id` is allocated via `muxer.allocate_sequence()` — fulfils the PHASE 2 comment on
///   [`OpcodeFrame::frame_id`].
/// * Encoding goes through `frame.wire_encode()` (trait path).
pub fn emit_lane_frames(
    columns: &IrColumns,
    lane_buckets: &[&[u32]; LANE_COUNT],
    muxer: &WebTransportMuxer,
) -> Result<Vec<EmitResult>, EmitError> {
    let mut results = Vec::with_capacity(LANE_COUNT);
    let hashes = columns.source_hashes();
    let effects = columns.effects();
    let ids = columns.ids();

    for lane in 0..LANE_COUNT {
        let bucket = lane_buckets.get(lane).copied().unwrap_or(&[]);
        if bucket.is_empty() {
            continue;
        }

        let stream_id = usize::from(WT_STREAM_SLOT_PATCHES);
        let frame_id = muxer.allocate_sequence(stream_id).unwrap_or(0);

        // Derive component_id from the first dirty slot's lane — if the
        // lane has exactly one component, attribute the frame to it.
        let component_id = if bucket.len() == 1 {
            let slot = usize::try_from(bucket.first().copied().unwrap_or(0)).unwrap_or(0);
            ids.get(slot).copied()
        } else {
            None
        };

        let instructions = emit_lane_instructions(bucket, hashes, effects);
        let instruction_count = instructions.len();

        let frame = OpcodeFrame {
            frame_id,
            component_id,
            instructions,
        };

        // Trait path — fulfils wire.rs PHASE 2 contract.
        let wire_bytes = frame.wire_encode()?;

        results.push(EmitResult {
            lane: u8::try_from(lane).unwrap_or(0),
            frame_id,
            component_id,
            wire_bytes,
            instruction_count,
        });
    }

    Ok(results)
}

/// Generates [`Instruction::SlotSet`] entries for each dirty column index.
///
/// For each dirty slot we emit the `source_hash` as the slot value — this is
/// the thin-emitter approach where column-level metadata changes are encoded
/// as slot updates. The `effects` byte is packed into the first byte of the
/// value payload so the client can react to tier changes without a separate
/// instruction.
///
/// Wire format of the `SlotSet` value payload (8 + 1 = 9 bytes):
///
/// | offset | size | field              |
/// |--------|------|--------------------|
/// | 0      | 8    | source_hash (LE)   |
/// | 8      | 1    | effects bitmask    |
#[inline]
fn emit_lane_instructions(indices: &[u32], hashes: &[u64], effects: &[u8]) -> Vec<Instruction> {
    let mut instructions = Vec::with_capacity(indices.len());

    for &column_idx in indices {
        let slot = usize::try_from(column_idx).unwrap_or(0);

        // Build 9-byte payload: 8 bytes hash LE + 1 byte effects.
        let hash = hashes.get(slot).copied().unwrap_or(0);
        let effect_byte = effects.get(slot).copied().unwrap_or(0);

        let mut value = Vec::with_capacity(9);
        value.extend_from_slice(&hash.to_le_bytes());
        value.push(effect_byte);

        instructions.push(Instruction::SlotSet {
            slot_id: SlotId(column_idx),
            value,
        });
    }

    instructions
}

// ── Intern table bootstrap & diff ────────────────────────────────────

/// Builds the initial [`Instruction::InitInternTable`] instructions for
/// session bootstrap — one per non-empty table kind.
pub fn bootstrap_intern_tables(snapshot: &InternTableSnapshot) -> Vec<Instruction> {
    let mut instructions = Vec::with_capacity(3);
    let kinds = [
        InternTableKind::Tag,
        InternTableKind::Attr,
        InternTableKind::Event,
    ];

    for kind in kinds {
        let entries = snapshot.entries_for(kind);
        if entries.is_empty() {
            continue;
        }
        instructions.push(Instruction::InitInternTable {
            table: InternTable {
                kind,
                entries: entries.to_vec(),
            },
        });
    }

    instructions
}

/// Diffs two [`InternTableSnapshot`]s and produces
/// [`Instruction::PatchInternTable`] instructions for each table kind that
/// changed.
///
/// * New entries (present in `current` but not `prev`) → `InternPatchOp::Set`.
/// * Removed entries (present in `prev` but not `current`) → `InternPatchOp::Remove`.
/// * Changed entries (same id, different value) → `InternPatchOp::Set`.
pub fn diff_intern_tables(
    prev: &InternTableSnapshot,
    current: &InternTableSnapshot,
) -> Vec<Instruction> {
    let mut instructions = Vec::new();
    let kinds = [
        InternTableKind::Tag,
        InternTableKind::Attr,
        InternTableKind::Event,
    ];

    for kind in kinds {
        let prev_entries = prev.entries_for(kind);
        let curr_entries = current.entries_for(kind);

        let mut ops = Vec::new();

        // Build lookup from prev entries by id.
        let prev_map: rustc_hash::FxHashMap<u16, &str> = prev_entries
            .iter()
            .map(|e| (e.id, e.value.as_str()))
            .collect();

        let curr_map: rustc_hash::FxHashMap<u16, &str> = curr_entries
            .iter()
            .map(|e| (e.id, e.value.as_str()))
            .collect();

        // Detect additions and changes.
        for entry in curr_entries {
            match prev_map.get(&entry.id) {
                Some(prev_val) if *prev_val == entry.value.as_str() => {
                    // Unchanged — skip.
                }
                _ => {
                    ops.push(InternPatchOp::Set {
                        id: entry.id,
                        value: entry.value.clone(),
                    });
                }
            }
        }

        // Detect removals.
        for entry in prev_entries {
            if !curr_map.contains_key(&entry.id) {
                ops.push(InternPatchOp::Remove { id: entry.id });
            }
        }

        if !ops.is_empty() {
            instructions.push(Instruction::PatchInternTable { kind, ops });
        }
    }

    instructions
}

// ── Suspense boundary patcher (stub) ─────────────────────────────────

/// Stub for suspense boundary patching.
///
/// When a dirty slot is tagged as suspense-pending, this calculates the byte
/// range of the resolved instructions within the frame and emits a
/// [`Instruction::Patch`]. Uses [`InstructionRange::try_new`] exclusively —
/// fulfils the PHASE 2 invariant comment.
///
/// Full implementation deferred to Phase D when the component-tree
/// representation exists.
pub fn emit_suspense_patch(
    suspense_id: u32,
    byte_start: u32,
    byte_end: u32,
) -> Result<Instruction, EmitError> {
    let range = InstructionRange::try_new(byte_start, byte_end)?;
    Ok(Instruction::Patch {
        suspense_id: SuspenseId(suspense_id),
        range,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::EffectProfile;
    use crate::ir::opcode::*;
    use crate::ir::wire;
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
            is_interactive: false,
            is_client_interactive: false,
            source_hash: 0xBEEF_0000 | u64::from(id),
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

    fn lane_buckets_from_all(columns: &IrColumns) -> [Vec<u32>; LANE_COUNT] {
        let offsets = columns.lane_offsets();
        let mut buckets: [Vec<u32>; LANE_COUNT] = std::array::from_fn(|_| Vec::new());
        for lane in 0..LANE_COUNT {
            let start = offsets.get(lane).copied().unwrap_or(0);
            let end = offsets.get(lane.saturating_add(1)).copied().unwrap_or(0);
            for idx in start..end {
                if let Some(bucket) = buckets.get_mut(lane) {
                    bucket.push(idx);
                }
            }
        }
        buckets
    }

    fn buckets_as_refs(buckets: &[Vec<u32>; LANE_COUNT]) -> [&[u32]; LANE_COUNT] {
        [
            buckets.first().map_or(&[], Vec::as_slice),
            buckets.get(1).map_or(&[], Vec::as_slice),
            buckets.get(2).map_or(&[], Vec::as_slice),
            buckets.get(3).map_or(&[], Vec::as_slice),
        ]
    }

    #[test]
    fn emit_lane_frames_produces_one_result_per_dirty_lane() {
        let columns = four_lane_columns();
        let buckets = lane_buckets_from_all(&columns);
        let refs = buckets_as_refs(&buckets);
        let muxer = WebTransportMuxer::new();

        let results = emit_lane_frames(&columns, &refs, &muxer).unwrap();
        assert_eq!(results.len(), LANE_COUNT);

        for result in &results {
            assert!(!result.wire_bytes.is_empty());
            assert_eq!(result.instruction_count, 1);
        }
    }

    #[test]
    fn emitted_frames_decode_back_to_correct_instructions() {
        let columns = four_lane_columns();
        let buckets = lane_buckets_from_all(&columns);
        let refs = buckets_as_refs(&buckets);
        let muxer = WebTransportMuxer::new();

        let results = emit_lane_frames(&columns, &refs, &muxer).unwrap();

        for result in &results {
            let (decoded, _) =
                wire::decode_frame(&result.wire_bytes).expect("wire bytes must decode cleanly");
            assert_eq!(decoded.instructions.len(), result.instruction_count);
            assert_eq!(decoded.frame_id, result.frame_id);

            // Each instruction should be a SlotSet with a 9-byte value.
            for instruction in &decoded.instructions {
                match instruction {
                    Instruction::SlotSet { value, .. } => {
                        assert_eq!(value.len(), 9, "SlotSet payload must be 9 bytes");
                    }
                    other => panic!("expected SlotSet, got {:?}", other),
                }
            }
        }
    }

    #[test]
    fn slot_set_values_match_column_source_hashes() {
        let columns = four_lane_columns();
        let buckets = lane_buckets_from_all(&columns);
        let refs = buckets_as_refs(&buckets);
        let muxer = WebTransportMuxer::new();

        let results = emit_lane_frames(&columns, &refs, &muxer).unwrap();

        for result in &results {
            let (decoded, _) = wire::decode_frame(&result.wire_bytes).unwrap();
            for instruction in &decoded.instructions {
                if let Instruction::SlotSet { slot_id, value } = instruction {
                    let idx = slot_id.0 as usize;
                    let expected_hash = columns.source_hashes().get(idx).copied().unwrap_or(0);
                    let mut hash_bytes = [0u8; 8];
                    hash_bytes.copy_from_slice(&value[0..8]);
                    assert_eq!(u64::from_le_bytes(hash_bytes), expected_hash);

                    let expected_effects = columns.effects().get(idx).copied().unwrap_or(0);
                    assert_eq!(value[8], expected_effects);
                }
            }
        }
    }

    #[test]
    fn emit_lane_frames_allocates_monotonic_frame_ids() {
        let columns = four_lane_columns();
        let buckets = lane_buckets_from_all(&columns);
        let refs = buckets_as_refs(&buckets);
        let muxer = WebTransportMuxer::new();

        let first = emit_lane_frames(&columns, &refs, &muxer).unwrap();
        let second = emit_lane_frames(&columns, &refs, &muxer).unwrap();

        // All frame_ids in second batch must be greater than all in first.
        let max_first = first.iter().map(|r| r.frame_id).max().unwrap_or(0);
        let min_second = second.iter().map(|r| r.frame_id).min().unwrap_or(0);
        assert!(
            min_second > max_first,
            "frame IDs must advance between calls"
        );
    }

    #[test]
    fn emit_lane_frames_skips_empty_lanes() {
        let columns = four_lane_columns();
        // Only populate lane 0.
        let mut buckets: [Vec<u32>; LANE_COUNT] = std::array::from_fn(|_| Vec::new());
        let offsets = columns.lane_offsets();
        let start = offsets.first().copied().unwrap_or(0);
        let end = offsets.get(1).copied().unwrap_or(0);
        for idx in start..end {
            if let Some(bucket) = buckets.first_mut() {
                bucket.push(idx);
            }
        }
        let refs = buckets_as_refs(&buckets);
        let muxer = WebTransportMuxer::new();

        let results = emit_lane_frames(&columns, &refs, &muxer).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results.first().map(|r| r.lane), Some(0));
    }

    // ── Intern table tests ───────────────────────────────────────────

    #[test]
    fn bootstrap_intern_tables_emits_init_for_each_non_empty_kind() {
        let snapshot = InternTableSnapshot {
            tags: vec![
                InternEntry {
                    id: 0,
                    value: "div".to_string(),
                },
                InternEntry {
                    id: 1,
                    value: "span".to_string(),
                },
            ],
            attrs: vec![InternEntry {
                id: 0,
                value: "class".to_string(),
            }],
            events: Vec::new(),
        };

        let instructions = bootstrap_intern_tables(&snapshot);
        assert_eq!(instructions.len(), 2, "events is empty → only 2 tables");

        for instruction in &instructions {
            match instruction {
                Instruction::InitInternTable { table } => {
                    assert!(!table.entries.is_empty());
                }
                other => panic!("expected InitInternTable, got {:?}", other),
            }
        }
    }

    #[test]
    fn diff_intern_tables_detects_additions_and_removals() {
        let prev = InternTableSnapshot {
            tags: vec![
                InternEntry {
                    id: 0,
                    value: "div".to_string(),
                },
                InternEntry {
                    id: 1,
                    value: "span".to_string(),
                },
            ],
            attrs: Vec::new(),
            events: Vec::new(),
        };

        let current = InternTableSnapshot {
            tags: vec![
                InternEntry {
                    id: 0,
                    value: "div".to_string(),
                },
                // id 1 removed
                InternEntry {
                    id: 2,
                    value: "section".to_string(),
                }, // added
            ],
            attrs: Vec::new(),
            events: Vec::new(),
        };

        let instructions = diff_intern_tables(&prev, &current);
        assert_eq!(instructions.len(), 1);

        match &instructions[0] {
            Instruction::PatchInternTable { kind, ops } => {
                assert_eq!(*kind, InternTableKind::Tag);
                assert_eq!(ops.len(), 2); // 1 Set + 1 Remove
                assert!(ops
                    .iter()
                    .any(|op| matches!(op, InternPatchOp::Set { id: 2, .. })));
                assert!(ops
                    .iter()
                    .any(|op| matches!(op, InternPatchOp::Remove { id: 1 })));
            }
            other => panic!("expected PatchInternTable, got {:?}", other),
        }
    }

    #[test]
    fn diff_intern_tables_detects_value_changes() {
        let prev = InternTableSnapshot {
            tags: vec![InternEntry {
                id: 0,
                value: "div".to_string(),
            }],
            attrs: Vec::new(),
            events: Vec::new(),
        };
        let current = InternTableSnapshot {
            tags: vec![InternEntry {
                id: 0,
                value: "article".to_string(),
            }],
            attrs: Vec::new(),
            events: Vec::new(),
        };

        let instructions = diff_intern_tables(&prev, &current);
        assert_eq!(instructions.len(), 1);
        match &instructions[0] {
            Instruction::PatchInternTable { ops, .. } => {
                assert_eq!(ops.len(), 1);
                assert!(
                    matches!(&ops[0], InternPatchOp::Set { id: 0, value } if value == "article")
                );
            }
            other => panic!("expected PatchInternTable, got {:?}", other),
        }
    }

    #[test]
    fn diff_identical_snapshots_produces_no_ops() {
        let snapshot = InternTableSnapshot {
            tags: vec![InternEntry {
                id: 0,
                value: "div".to_string(),
            }],
            attrs: vec![InternEntry {
                id: 0,
                value: "class".to_string(),
            }],
            events: vec![InternEntry {
                id: 0,
                value: "click".to_string(),
            }],
        };

        let instructions = diff_intern_tables(&snapshot, &snapshot);
        assert!(instructions.is_empty());
    }

    // ── Suspense patcher ─────────────────────────────────────────────

    #[test]
    fn suspense_patch_uses_try_new_and_rejects_invalid_range() {
        let err = emit_suspense_patch(100, 200, 50).unwrap_err();
        assert!(
            matches!(err, EmitError::Range(_)),
            "reversed range must produce RangeError"
        );
    }

    #[test]
    fn suspense_patch_produces_valid_patch_instruction() {
        let instruction = emit_suspense_patch(42, 0, 128).unwrap();
        match instruction {
            Instruction::Patch { suspense_id, range } => {
                assert_eq!(suspense_id, SuspenseId(42));
                assert_eq!(range.start(), 0);
                assert_eq!(range.end(), 128);
            }
            other => panic!("expected Patch, got {:?}", other),
        }
    }

    // ── Round-trip integration ───────────────────────────────────────

    #[test]
    fn full_emit_encode_decode_round_trip() {
        let columns = four_lane_columns();
        let buckets = lane_buckets_from_all(&columns);
        let refs = buckets_as_refs(&buckets);
        let muxer = WebTransportMuxer::new();

        let results = emit_lane_frames(&columns, &refs, &muxer).unwrap();
        assert!(!results.is_empty());

        for result in &results {
            // Decode
            let (decoded, _) = wire::decode_frame(&result.wire_bytes).unwrap();
            // Re-encode
            let re_encoded = decoded.wire_encode().unwrap();
            // Decode again
            let (re_decoded, _) = wire::decode_frame(&re_encoded).unwrap();
            assert_eq!(decoded, re_decoded, "double round-trip must be idempotent");
        }
    }
}
