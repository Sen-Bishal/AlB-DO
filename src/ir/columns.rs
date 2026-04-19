//! Struct-of-Arrays column store for the canonical IR.
//!
//! `IrColumns` is the runtime truth for the IR graph. Hot-path consumers —
//! reconcile, dirty scan, lane routing — read one column at a time, so each
//! cache line load is 100% useful work. The classic array-of-structs
//! [`CanonicalIrDocument`](super::CanonicalIrDocument) is retained only as a
//! serialization shell: it is materialized on demand via
//! [`IrColumns::to_canonical`] when JSON export is requested and is
//! reconstructed from JSON via [`IrColumns::from_canonical`].
//!
//! # Layout
//!
//! ```text
//! hot numeric columns  : ids, source_hashes, estimated_sizes,
//!                        line_numbers, effects, export_kinds,
//!                        priorities, phases, presence
//! cold interned columns: symbols, module_paths (via StringInterner)
//! edges                : edge_from, edge_to (column indices, not ids)
//! lookup               : id_to_index (FxHashMap<u64, u32>)
//! modules              : IrModuleColumn (cold)
//! ```
//!
//! Edges reference **column indices** rather than raw ids so that a rayon
//! split-borrow on a lane slice (cycle 4) requires no id indirection at
//! emit time.

use super::{
    build_canonical_components_and_edges_from_graph,
    build_canonical_components_and_edges_from_parsed, new_canonical_document, CanonicalIrComponent,
    CanonicalIrDocument, CanonicalIrEdge, CanonicalIrModule, IrExportKind,
};
use crate::graph::ComponentGraph;
use crate::parser::ParsedComponent;
use crate::types::{ComponentAnalysis, ComponentId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Range;

/// Number of highway lanes the column store can partition into.
///
/// Mirrors [`crate::runtime::highway::LANE_COUNT`]; the runtime side carries
/// a compile-time assertion that the two constants stay in lockstep.
pub const LANE_COUNT: usize = 4;

/// Interned string handle.
///
/// Handles are stable for the lifetime of their owning [`StringInterner`].
/// Equivalent interners built from the same inputs in the same order produce
/// identical `StringId` sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct StringId(u32);

impl StringId {
    #[inline]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    #[inline]
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Flat string interner backed by a `Vec<String>` and an `FxHashMap` lookup.
///
/// `FxHashMap` is preferred over the std `HashMap` because its hash is
/// ~2× faster for the short identifiers we hold (component names, file
/// paths), and the map is not exposed to adversarial input.
#[derive(Debug, Default, Clone)]
pub struct StringInterner {
    storage: Vec<String>,
    index: FxHashMap<String, u32>,
}

impl StringInterner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            storage: Vec::with_capacity(capacity),
            index: FxHashMap::with_capacity_and_hasher(capacity, Default::default()),
        }
    }

    pub fn intern(&mut self, value: &str) -> StringId {
        if let Some(&existing) = self.index.get(value) {
            return StringId(existing);
        }
        let next = u32::try_from(self.storage.len()).unwrap_or(u32::MAX);
        self.storage.push(value.to_string());
        self.index.insert(value.to_string(), next);
        StringId(next)
    }

    pub fn resolve(&self, id: StringId) -> &str {
        self.storage
            .get(id.0 as usize)
            .map(String::as_str)
            .unwrap_or("")
    }

    pub fn len(&self) -> usize {
        self.storage.len()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }
}

/// Bit-packed effect flags.
pub mod effect_bits {
    use crate::effects::EffectProfile;

    pub const HOOKS: u8 = 1 << 0;
    pub const ASYNC: u8 = 1 << 1;
    pub const IO: u8 = 1 << 2;
    pub const SIDE_EFFECTS: u8 = 1 << 3;

    #[inline]
    pub fn pack(profile: EffectProfile) -> u8 {
        let mut bits: u8 = 0;
        if profile.hooks {
            bits |= HOOKS;
        }
        if profile.asynchronous {
            bits |= ASYNC;
        }
        if profile.io {
            bits |= IO;
        }
        if profile.side_effects {
            bits |= SIDE_EFFECTS;
        }
        bits
    }

    #[inline]
    pub fn unpack(bits: u8) -> EffectProfile {
        EffectProfile {
            hooks: (bits & HOOKS) != 0,
            asynchronous: (bits & ASYNC) != 0,
            io: (bits & IO) != 0,
            side_effects: (bits & SIDE_EFFECTS) != 0,
        }
    }
}

/// Bit-packed export kind column values.
pub mod export_kind_bits {
    use super::IrExportKind;

    pub const NAMED: u8 = 0;
    pub const DEFAULT: u8 = 1;

    #[inline]
    pub const fn pack(kind: IrExportKind) -> u8 {
        match kind {
            IrExportKind::Named => NAMED,
            IrExportKind::Default => DEFAULT,
        }
    }

    #[inline]
    pub const fn unpack(bits: u8) -> IrExportKind {
        if bits == DEFAULT {
            IrExportKind::Default
        } else {
            IrExportKind::Named
        }
    }
}

mod presence_bits {
    pub const PRIORITY: u8 = 1 << 0;
    pub const PHASE: u8 = 1 << 1;
}

/// Field-selection bits carried by a [`LaneColumnPatch`].
///
/// A patch's `field_mask` marks which of the payload slots are authoritative;
/// unset bits mean the corresponding payload field is ignored. The bit layout
/// is stable and forms part of the wire contract for lane-scoped patches.
pub mod field_mask {
    pub const EFFECTS: u32 = 1 << 0;
    pub const SOURCE_HASH: u32 = 1 << 1;
    pub const PRIORITY: u32 = 1 << 2;
    pub const PHASE: u32 = 1 << 3;
    pub const ESTIMATED_SIZE: u32 = 1 << 4;
    pub const EXPORT_KIND: u32 = 1 << 5;
    pub const LINE_NUMBER: u32 = 1 << 6;
    pub const SYMBOL: u32 = 1 << 7;
    pub const MODULE_PATH: u32 = 1 << 8;
}

/// Lane-scoped column patch — the primary unit of reconcile output.
///
/// Replaces the JSON struct-diff of [`CanonicalIrComponent`]: a patch points
/// at `(lane, column_idx)` directly, and `field_mask` selects which of the
/// packed payload slots are meaningful. Unselected slots hold zero-initialized
/// defaults and must be ignored by the receiver.
#[derive(Debug, Clone, Default)]
pub struct LaneColumnPatch {
    pub lane: u8,
    pub column_idx: u32,
    pub field_mask: u32,
    pub effects: u8,
    pub export_kind: u8,
    pub estimated_size: u32,
    pub line_number: u32,
    pub source_hash: u64,
    pub priority: f32,
    pub phase: f32,
    pub symbol: StringId,
    pub module_path: StringId,
}

/// Disjoint mutable borrow of a single lane's hot column slices.
///
/// Returned by [`IrColumns::lane_column_pass_mut`] and, in grouped form, by
/// [`IrColumns::parallel_lane_column_pass`]. Because every field is a
/// `&mut [_]` into a non-overlapping region of the owning [`IrColumns`], the
/// borrow checker proves that shipping these passes to rayon workers is
/// race-free without synchronization primitives.
#[derive(Debug)]
pub struct LaneColumnPass<'a> {
    pub lane: u8,
    pub column_start: u32,
    pub effects: &'a mut [u8],
    pub source_hashes: &'a mut [u64],
    pub priorities: &'a mut [f32],
    pub phases: &'a mut [f32],
}

/// Disjoint mutable borrow of the hot IR columns for a parallel pass.
///
/// Returned by [`IrColumns::column_pass_mut`]. Each field is a `&mut [_]`
/// into a different backing `Vec` inside the owning [`IrColumns`], so the
/// borrow checker proves that distributing these slices to different
/// rayon workers is race-free without any runtime synchronization.
#[derive(Debug)]
pub struct ColumnPass<'a> {
    pub effects: &'a mut [u8],
    pub source_hashes: &'a mut [u64],
    pub priorities: &'a mut [f32],
    pub phases: &'a mut [f32],
    pub edge_from: &'a mut [u32],
    pub edge_to: &'a mut [u32],
}

/// Cold module column — one entry per module, indexed by module ordinal.
#[derive(Debug, Clone, Default)]
pub struct IrModuleColumn {
    pub module_path: StringId,
    pub source_hash: u64,
    pub imports: Vec<StringId>,
}

/// Struct-of-Arrays representation of the canonical IR graph.
#[derive(Debug, Clone, Default)]
pub struct IrColumns {
    // ── Hot numeric columns (per component) ──────────
    ids: Vec<u64>,
    // PHASE 2 GATEWAY: cycle 2 walks this column with `wide::u64x4` SIMD
    // and feeds the equality mask into a DirtyBitmap word.
    source_hashes: Vec<u64>,
    estimated_sizes: Vec<u32>,
    line_numbers: Vec<u32>,
    effects: Vec<u8>,
    export_kinds: Vec<u8>,
    priorities: Vec<f32>,
    phases: Vec<f32>,
    presence: Vec<u8>,

    // ── Cold interned columns ────────────────────────
    symbols: Vec<StringId>,
    module_paths: Vec<StringId>,
    strings: StringInterner,

    // ── Edges keyed by column index ──────────────────
    // PHASE 2 GATEWAY: cycle 4 sorts these two Vecs by lane and exposes
    // per-lane contiguous slices to rayon join mutations.
    edge_from: Vec<u32>,
    edge_to: Vec<u32>,

    // ── Cold module column ───────────────────────────
    modules: Vec<IrModuleColumn>,

    // ── Lookup ───────────────────────────────────────
    // PHASE 2 GATEWAY: cycle 2 addresses DirtyBitmap bits by column index;
    // random-access lookups by component id go through this map.
    id_to_index: FxHashMap<u64, u32>,

    // ── Lane partition (cycle 4) ─────────────────────
    // `lane_ids[i]` is the lane assignment of component slot `i`.
    // `lane_offsets[lane]..lane_offsets[lane + 1]` is the half-open range
    // occupied by `lane` after a [`Self::sort_by_lane`] call. Before any
    // sort, every component lives in lane 0.
    lane_ids: Vec<u8>,
    lane_offsets: [u32; LANE_COUNT + 1],
}

impl IrColumns {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ids: Vec::with_capacity(capacity),
            source_hashes: Vec::with_capacity(capacity),
            estimated_sizes: Vec::with_capacity(capacity),
            line_numbers: Vec::with_capacity(capacity),
            effects: Vec::with_capacity(capacity),
            export_kinds: Vec::with_capacity(capacity),
            priorities: Vec::with_capacity(capacity),
            phases: Vec::with_capacity(capacity),
            presence: Vec::with_capacity(capacity),
            symbols: Vec::with_capacity(capacity),
            module_paths: Vec::with_capacity(capacity),
            strings: StringInterner::with_capacity(capacity.saturating_mul(2)),
            edge_from: Vec::new(),
            edge_to: Vec::new(),
            modules: Vec::new(),
            id_to_index: FxHashMap::with_capacity_and_hasher(capacity, Default::default()),
            lane_ids: Vec::with_capacity(capacity),
            lane_offsets: [0; LANE_COUNT + 1],
        }
    }

    // ── Core column accessors ────────────────────────
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn ids(&self) -> &[u64] {
        &self.ids
    }

    pub fn source_hashes(&self) -> &[u64] {
        &self.source_hashes
    }

    pub fn estimated_sizes(&self) -> &[u32] {
        &self.estimated_sizes
    }

    pub fn line_numbers(&self) -> &[u32] {
        &self.line_numbers
    }

    pub fn effects(&self) -> &[u8] {
        &self.effects
    }

    pub fn export_kinds(&self) -> &[u8] {
        &self.export_kinds
    }

    pub fn priorities(&self) -> &[f32] {
        &self.priorities
    }

    pub fn phases(&self) -> &[f32] {
        &self.phases
    }

    pub fn presence(&self) -> &[u8] {
        &self.presence
    }

    pub fn symbols(&self) -> &[StringId] {
        &self.symbols
    }

    pub fn module_paths(&self) -> &[StringId] {
        &self.module_paths
    }

    pub fn edge_from(&self) -> &[u32] {
        &self.edge_from
    }

    pub fn edge_to(&self) -> &[u32] {
        &self.edge_to
    }

    pub fn modules(&self) -> &[IrModuleColumn] {
        &self.modules
    }

    pub fn strings(&self) -> &StringInterner {
        &self.strings
    }

    /// Per-slot lane assignment. Before [`Self::sort_by_lane`] is called
    /// every entry is `0`.
    pub fn lane_ids(&self) -> &[u8] {
        &self.lane_ids
    }

    /// Cumulative lane-start positions. `offsets[lane]..offsets[lane + 1]`
    /// is the contiguous column range owned by `lane`.
    pub fn lane_offsets(&self) -> [u32; LANE_COUNT + 1] {
        self.lane_offsets
    }

    /// Half-open column range occupied by `lane`, or `None` if `lane` is out
    /// of range or the stored offsets are inconsistent with column length.
    pub fn lane_range(&self, lane: usize) -> Option<Range<usize>> {
        if lane >= LANE_COUNT {
            return None;
        }
        let start = usize::try_from(self.lane_offsets.get(lane).copied().unwrap_or(0)).ok()?;
        let end =
            usize::try_from(self.lane_offsets.get(lane.saturating_add(1)).copied().unwrap_or(0))
                .ok()?;
        if start > end || end > self.ids.len() {
            return None;
        }
        Some(start..end)
    }

    pub fn lane_ids_slice(&self, lane: usize) -> &[u8] {
        self.lane_range(lane)
            .and_then(|range| self.lane_ids.get(range))
            .unwrap_or(&[])
    }

    pub fn lane_source_hashes(&self, lane: usize) -> &[u64] {
        self.lane_range(lane)
            .and_then(|range| self.source_hashes.get(range))
            .unwrap_or(&[])
    }

    pub fn lane_effects(&self, lane: usize) -> &[u8] {
        self.lane_range(lane)
            .and_then(|range| self.effects.get(range))
            .unwrap_or(&[])
    }

    pub fn lane_priorities(&self, lane: usize) -> &[f32] {
        self.lane_range(lane)
            .and_then(|range| self.priorities.get(range))
            .unwrap_or(&[])
    }

    pub fn lane_phases(&self, lane: usize) -> &[f32] {
        self.lane_range(lane)
            .and_then(|range| self.phases.get(range))
            .unwrap_or(&[])
    }

    pub fn lane_ids_column(&self, lane: usize) -> &[u64] {
        self.lane_range(lane)
            .and_then(|range| self.ids.get(range))
            .unwrap_or(&[])
    }

    pub fn symbol_at(&self, idx: u32) -> &str {
        self.symbols
            .get(idx as usize)
            .map(|sid| self.strings.resolve(*sid))
            .unwrap_or("")
    }

    pub fn module_path_at(&self, idx: u32) -> &str {
        self.module_paths
            .get(idx as usize)
            .map(|sid| self.strings.resolve(*sid))
            .unwrap_or("")
    }

    /// Returns the column position of a component by its id.
    pub fn index_of(&self, id: u64) -> Option<u32> {
        self.id_to_index.get(&id).copied()
    }

    /// Materializes a single component into the AoS shape.
    ///
    /// Allocates; use column accessors directly in hot paths.
    pub fn component_at(&self, idx: u32) -> Option<CanonicalIrComponent> {
        let slot = idx as usize;
        let id = *self.ids.get(slot)?;
        let presence = *self.presence.get(slot)?;

        let legacy_priority = if presence & presence_bits::PRIORITY != 0 {
            self.priorities.get(slot).map(|value| f64::from(*value))
        } else {
            None
        };
        let legacy_phase = if presence & presence_bits::PHASE != 0 {
            self.phases.get(slot).map(|value| f64::from(*value))
        } else {
            None
        };

        Some(CanonicalIrComponent {
            id,
            symbol: self.strings.resolve(*self.symbols.get(slot)?).to_string(),
            module_path: self
                .strings
                .resolve(*self.module_paths.get(slot)?)
                .to_string(),
            export_kind: export_kind_bits::unpack(*self.export_kinds.get(slot)?),
            line_number: *self.line_numbers.get(slot)? as usize,
            estimated_size_bytes: u64::from(*self.estimated_sizes.get(slot)?),
            effects: effect_bits::unpack(*self.effects.get(slot)?),
            source_hash: *self.source_hashes.get(slot)?,
            legacy_priority,
            legacy_phase,
        })
    }

    // ── Builders ─────────────────────────────────────

    /// Builds columns from a list of parsed components.
    ///
    /// Equivalent to `Self::from_canonical(&build_canonical_ir_from_parsed(..))`,
    /// but goes through the shared AoS builder exactly once to keep the two
    /// paths guaranteed-equivalent.
    pub fn from_parsed(components: &[ParsedComponent]) -> Self {
        let (modules, components, edges) =
            build_canonical_components_and_edges_from_parsed(components);
        let document = new_canonical_document(modules, components, edges);
        Self::from_canonical(&document)
    }

    /// Builds columns from a resolved [`ComponentGraph`] and its analyses.
    pub fn from_graph(
        graph: &ComponentGraph,
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
    ) -> Self {
        let (modules, components, edges) =
            build_canonical_components_and_edges_from_graph(graph, analyses);
        let document = new_canonical_document(modules, components, edges);
        Self::from_canonical(&document)
    }

    /// Reconstructs columns from a normalized canonical document.
    pub fn from_canonical(document: &CanonicalIrDocument) -> Self {
        let mut columns = Self::with_capacity(document.components.len());

        for module in &document.modules {
            let module_path = columns.strings.intern(&module.module_path);
            let imports = module
                .imports
                .iter()
                .map(|value| columns.strings.intern(value))
                .collect();
            columns.modules.push(IrModuleColumn {
                module_path,
                source_hash: module.source_hash,
                imports,
            });
        }

        for component in &document.components {
            let col_idx = u32::try_from(columns.ids.len()).unwrap_or(u32::MAX);
            columns.ids.push(component.id);
            columns.id_to_index.insert(component.id, col_idx);

            columns
                .symbols
                .push(columns.strings.intern(&component.symbol));
            columns
                .module_paths
                .push(columns.strings.intern(&component.module_path));
            columns
                .export_kinds
                .push(export_kind_bits::pack(component.export_kind));
            columns
                .line_numbers
                .push(u32::try_from(component.line_number).unwrap_or(u32::MAX));
            columns
                .estimated_sizes
                .push(u32::try_from(component.estimated_size_bytes).unwrap_or(u32::MAX));
            columns.effects.push(effect_bits::pack(component.effects));
            columns.source_hashes.push(component.source_hash);

            let mut presence: u8 = 0;
            let priority_f32 = match component.legacy_priority {
                Some(value) => {
                    presence |= presence_bits::PRIORITY;
                    value as f32
                }
                None => 0.0,
            };
            let phase_f32 = match component.legacy_phase {
                Some(value) => {
                    presence |= presence_bits::PHASE;
                    value as f32
                }
                None => 0.0,
            };
            columns.priorities.push(priority_f32);
            columns.phases.push(phase_f32);
            columns.presence.push(presence);
        }

        for edge in &document.edges {
            let from_idx = columns
                .id_to_index
                .get(&edge.from)
                .copied()
                .unwrap_or(u32::MAX);
            let to_idx = columns
                .id_to_index
                .get(&edge.to)
                .copied()
                .unwrap_or(u32::MAX);
            columns.edge_from.push(from_idx);
            columns.edge_to.push(to_idx);
        }

        // Unsorted columns live entirely in lane 0 until the caller invokes
        // [`Self::sort_by_lane`]. This keeps the empty and pre-sort states
        // observably consistent with a fully lane-partitioned store.
        columns.lane_ids = vec![0_u8; columns.ids.len()];
        let total = u32::try_from(columns.ids.len()).unwrap_or(u32::MAX);
        let mut offsets = [0_u32; LANE_COUNT + 1];
        for slot in 1..=LANE_COUNT {
            if let Some(entry) = offsets.get_mut(slot) {
                *entry = total;
            }
        }
        columns.lane_offsets = offsets;

        columns
    }

    /// Disjoint mutable split-borrow of the hot columns for a
    /// cycle-3 parallel pass.
    ///
    /// The returned [`ColumnPass`] bundles six non-overlapping `&mut [_]`
    /// borrows, one group per logical reconcile lane. Because each field
    /// is borrowed at most once, the compile-time borrow checker proves the
    /// partition is race-free — a rayon `scope`/`join` can drive each group
    /// on a separate worker with no synchronization primitive required.
    pub fn column_pass_mut(&mut self) -> ColumnPass<'_> {
        ColumnPass {
            effects: &mut self.effects,
            source_hashes: &mut self.source_hashes,
            priorities: &mut self.priorities,
            phases: &mut self.phases,
            edge_from: &mut self.edge_from,
            edge_to: &mut self.edge_to,
        }
    }

    /// Fans four column mutators out across a rayon scope.
    ///
    /// Each closure receives a disjoint mutable slice of the column store
    /// and runs on an independent rayon worker. The method blocks until
    /// every worker has returned, which keeps the public surface
    /// synchronous and single-owner — callers never observe a half-written
    /// column from outside this call.
    ///
    /// Column grouping:
    /// - `effects_pass`      → `&mut [u8]`  (tag recompute)
    /// - `source_hashes_pass`→ `&mut [u64]` (rehash changed)
    /// - `schedule_pass`     → `&mut [f32]` for priorities and phases
    /// - `edges_pass`        → `&mut [u32]` for `edge_from` and `edge_to`
    pub fn parallel_column_pass<EF, SH, SC, ED>(
        &mut self,
        effects_pass: EF,
        source_hashes_pass: SH,
        schedule_pass: SC,
        edges_pass: ED,
    ) where
        EF: FnOnce(&mut [u8]) + Send,
        SH: FnOnce(&mut [u64]) + Send,
        SC: FnOnce(&mut [f32], &mut [f32]) + Send,
        ED: FnOnce(&mut [u32], &mut [u32]) + Send,
    {
        let ColumnPass {
            effects,
            source_hashes,
            priorities,
            phases,
            edge_from,
            edge_to,
        } = self.column_pass_mut();

        rayon::scope(|scope| {
            scope.spawn(move |_| effects_pass(effects));
            scope.spawn(move |_| source_hashes_pass(source_hashes));
            scope.spawn(move |_| schedule_pass(priorities, phases));
            scope.spawn(move |_| edges_pass(edge_from, edge_to));
        });
    }

    /// Borrows one lane's hot column slices for mutation.
    ///
    /// Returns `None` if `lane >= LANE_COUNT` or the internal lane offsets
    /// are inconsistent. The returned [`LaneColumnPass`] exposes
    /// non-overlapping `&mut [_]` views into each hot column — exactly the
    /// contract [`Self::parallel_lane_column_pass`] fans out across rayon
    /// workers.
    pub fn lane_column_pass_mut(&mut self, lane: usize) -> Option<LaneColumnPass<'_>> {
        let range = self.lane_range(lane)?;
        let lane_u8 = u8::try_from(lane).ok()?;
        let column_start = u32::try_from(range.start).unwrap_or(0);
        Some(LaneColumnPass {
            lane: lane_u8,
            column_start,
            effects: self.effects.get_mut(range.clone())?,
            source_hashes: self.source_hashes.get_mut(range.clone())?,
            priorities: self.priorities.get_mut(range.clone())?,
            phases: self.phases.get_mut(range)?,
        })
    }

    /// Fans per-lane mutation closures across a rayon scope.
    ///
    /// Each closure receives the [`LaneColumnPass`] for its lane. Because
    /// the lane partitions are disjoint, all four passes can be dispatched
    /// simultaneously with no locking. Runs synchronously — returns once
    /// every worker has completed.
    pub fn parallel_lane_column_pass<F0, F1, F2, F3>(
        &mut self,
        lane0: F0,
        lane1: F1,
        lane2: F2,
        lane3: F3,
    ) where
        F0: FnOnce(LaneColumnPass<'_>) + Send,
        F1: FnOnce(LaneColumnPass<'_>) + Send,
        F2: FnOnce(LaneColumnPass<'_>) + Send,
        F3: FnOnce(LaneColumnPass<'_>) + Send,
    {
        let offsets = self.lane_offsets;
        let total = self.ids.len();

        let effects = split_lane_slices_mut(&mut self.effects, offsets, total);
        let source_hashes = split_lane_slices_mut(&mut self.source_hashes, offsets, total);
        let priorities = split_lane_slices_mut(&mut self.priorities, offsets, total);
        let phases = split_lane_slices_mut(&mut self.phases, offsets, total);

        let [e0, e1, e2, e3] = effects;
        let [h0, h1, h2, h3] = source_hashes;
        let [p0, p1, p2, p3] = priorities;
        let [ph0, ph1, ph2, ph3] = phases;
        let [s0, s1, s2, s3, _] = offsets;

        let pass0 = LaneColumnPass {
            lane: 0,
            column_start: s0,
            effects: e0,
            source_hashes: h0,
            priorities: p0,
            phases: ph0,
        };
        let pass1 = LaneColumnPass {
            lane: 1,
            column_start: s1,
            effects: e1,
            source_hashes: h1,
            priorities: p1,
            phases: ph1,
        };
        let pass2 = LaneColumnPass {
            lane: 2,
            column_start: s2,
            effects: e2,
            source_hashes: h2,
            priorities: p2,
            phases: ph2,
        };
        let pass3 = LaneColumnPass {
            lane: 3,
            column_start: s3,
            effects: e3,
            source_hashes: h3,
            priorities: p3,
            phases: ph3,
        };

        rayon::scope(|scope| {
            scope.spawn(move |_| lane0(pass0));
            scope.spawn(move |_| lane1(pass1));
            scope.spawn(move |_| lane2(pass2));
            scope.spawn(move |_| lane3(pass3));
        });
    }

    /// Reorders every hot column so that all components assigned to the
    /// same lane live in a contiguous slice.
    ///
    /// `lane_for` is invoked once per component id and must return a value
    /// in `0..LANE_COUNT`; out-of-range values are clamped down to
    /// `LANE_COUNT - 1` to keep the partition total. The lane order within
    /// a slice preserves the caller-observed order of `lane_for` invocations
    /// — i.e. the pre-sort column order — which makes the sort stable and
    /// keeps deterministic behavior for golden-fixture tests that assume a
    /// lane-only reordering.
    ///
    /// Side effects:
    /// * `ids`, `source_hashes`, `estimated_sizes`, `line_numbers`,
    ///   `effects`, `export_kinds`, `priorities`, `phases`, `presence`,
    ///   `symbols`, `module_paths` are permuted in lockstep.
    /// * `edge_from` / `edge_to` endpoints are remapped to the new column
    ///   indices; edge order is preserved.
    /// * `id_to_index` is rebuilt.
    /// * `lane_ids` and `lane_offsets` are populated from the histogram.
    pub fn sort_by_lane<F>(&mut self, lane_for: F)
    where
        F: Fn(u64) -> usize,
    {
        let len = self.ids.len();
        if len == 0 {
            self.lane_ids.clear();
            self.lane_offsets = [0; LANE_COUNT + 1];
            return;
        }

        let mut assignments = vec![0_u8; len];
        for (slot, id) in self.ids.iter().enumerate() {
            let lane = lane_for(*id).min(LANE_COUNT.saturating_sub(1));
            if let Some(entry) = assignments.get_mut(slot) {
                *entry = u8::try_from(lane).unwrap_or(0);
            }
        }

        let mut counts = [0_u32; LANE_COUNT];
        for &lane in &assignments {
            if let Some(slot) = counts.get_mut(lane as usize) {
                *slot = slot.saturating_add(1);
            }
        }

        let mut offsets = [0_u32; LANE_COUNT + 1];
        let mut running: u32 = 0;
        for lane in 0..LANE_COUNT {
            if let Some(entry) = offsets.get_mut(lane) {
                *entry = running;
            }
            running = running.saturating_add(counts.get(lane).copied().unwrap_or(0));
        }
        if let Some(last) = offsets.get_mut(LANE_COUNT) {
            *last = u32::try_from(len).unwrap_or(u32::MAX);
        }

        let mut cursor = [0_u32; LANE_COUNT];
        for lane in 0..LANE_COUNT {
            if let Some(entry) = cursor.get_mut(lane) {
                *entry = offsets.get(lane).copied().unwrap_or(0);
            }
        }

        let mut new_idx_of_old = vec![0_u32; len];
        for (old, &lane_byte) in assignments.iter().enumerate() {
            let lane = lane_byte as usize;
            if let Some(cur) = cursor.get_mut(lane) {
                if let Some(entry) = new_idx_of_old.get_mut(old) {
                    *entry = *cur;
                }
                *cur = cur.saturating_add(1);
            }
        }

        let mut old_idx_at_new = vec![0_u32; len];
        for (old, &new_idx) in new_idx_of_old.iter().enumerate() {
            if let Some(entry) = old_idx_at_new.get_mut(new_idx as usize) {
                *entry = u32::try_from(old).unwrap_or(u32::MAX);
            }
        }

        self.ids = permute(&self.ids, &old_idx_at_new);
        self.source_hashes = permute(&self.source_hashes, &old_idx_at_new);
        self.estimated_sizes = permute(&self.estimated_sizes, &old_idx_at_new);
        self.line_numbers = permute(&self.line_numbers, &old_idx_at_new);
        self.effects = permute(&self.effects, &old_idx_at_new);
        self.export_kinds = permute(&self.export_kinds, &old_idx_at_new);
        self.priorities = permute(&self.priorities, &old_idx_at_new);
        self.phases = permute(&self.phases, &old_idx_at_new);
        self.presence = permute(&self.presence, &old_idx_at_new);
        self.symbols = permute(&self.symbols, &old_idx_at_new);
        self.module_paths = permute(&self.module_paths, &old_idx_at_new);
        self.lane_ids = permute(&assignments, &old_idx_at_new);

        for slot in self.edge_from.iter_mut() {
            let old = *slot as usize;
            if let Some(&new_idx) = new_idx_of_old.get(old) {
                *slot = new_idx;
            }
        }
        for slot in self.edge_to.iter_mut() {
            let old = *slot as usize;
            if let Some(&new_idx) = new_idx_of_old.get(old) {
                *slot = new_idx;
            }
        }

        self.id_to_index.clear();
        self.id_to_index.reserve(len);
        for (slot, &id) in self.ids.iter().enumerate() {
            self.id_to_index
                .insert(id, u32::try_from(slot).unwrap_or(u32::MAX));
        }

        self.lane_offsets = offsets;
    }

    /// Snapshots a component's column values into a [`LaneColumnPatch`].
    ///
    /// `field_mask` selects which slots are filled from the live columns;
    /// bits cleared in the mask are left at default values. Returns `None`
    /// if `column_idx` is out of range.
    pub fn build_lane_patch(&self, column_idx: u32, field_mask: u32) -> Option<LaneColumnPatch> {
        let slot = column_idx as usize;
        if slot >= self.ids.len() {
            return None;
        }

        let lane = self.lane_ids.get(slot).copied().unwrap_or(0);
        let mut patch = LaneColumnPatch {
            lane,
            column_idx,
            field_mask,
            ..LaneColumnPatch::default()
        };

        if field_mask & field_mask::EFFECTS != 0 {
            patch.effects = self.effects.get(slot).copied().unwrap_or(0);
        }
        if field_mask & field_mask::EXPORT_KIND != 0 {
            patch.export_kind = self.export_kinds.get(slot).copied().unwrap_or(0);
        }
        if field_mask & field_mask::ESTIMATED_SIZE != 0 {
            patch.estimated_size = self.estimated_sizes.get(slot).copied().unwrap_or(0);
        }
        if field_mask & field_mask::LINE_NUMBER != 0 {
            patch.line_number = self.line_numbers.get(slot).copied().unwrap_or(0);
        }
        if field_mask & field_mask::SOURCE_HASH != 0 {
            patch.source_hash = self.source_hashes.get(slot).copied().unwrap_or(0);
        }
        if field_mask & field_mask::PRIORITY != 0 {
            patch.priority = self.priorities.get(slot).copied().unwrap_or(0.0);
        }
        if field_mask & field_mask::PHASE != 0 {
            patch.phase = self.phases.get(slot).copied().unwrap_or(0.0);
        }
        if field_mask & field_mask::SYMBOL != 0 {
            patch.symbol = self.symbols.get(slot).copied().unwrap_or_default();
        }
        if field_mask & field_mask::MODULE_PATH != 0 {
            patch.module_path = self.module_paths.get(slot).copied().unwrap_or_default();
        }

        Some(patch)
    }

    /// Materializes the column store back into the serialization-shaped
    /// [`CanonicalIrDocument`].
    ///
    /// This is the only place where the AoS form is produced at runtime,
    /// and it runs exclusively on the JSON export path.
    pub fn to_canonical(&self) -> CanonicalIrDocument {
        let modules = self
            .modules
            .iter()
            .map(|module| CanonicalIrModule {
                module_path: self.strings.resolve(module.module_path).to_string(),
                source_hash: module.source_hash,
                imports: module
                    .imports
                    .iter()
                    .map(|sid| self.strings.resolve(*sid).to_string())
                    .collect(),
            })
            .collect::<Vec<_>>();

        let mut components = Vec::with_capacity(self.ids.len());
        for slot in 0..self.ids.len() {
            let idx = u32::try_from(slot).unwrap_or(u32::MAX);
            if let Some(component) = self.component_at(idx) {
                components.push(component);
            }
        }

        let mut edges = Vec::with_capacity(self.edge_from.len());
        for (from_idx, to_idx) in self.edge_from.iter().zip(self.edge_to.iter()) {
            let from = self
                .ids
                .get(*from_idx as usize)
                .copied()
                .unwrap_or_default();
            let to = self.ids.get(*to_idx as usize).copied().unwrap_or_default();
            edges.push(CanonicalIrEdge { from, to });
        }

        new_canonical_document(modules, components, edges)
    }
}

fn permute<T>(src: &[T], old_idx_at_new: &[u32]) -> Vec<T>
where
    T: Copy + Default,
{
    let mut out = Vec::with_capacity(old_idx_at_new.len());
    for &old_idx in old_idx_at_new {
        out.push(src.get(old_idx as usize).copied().unwrap_or_default());
    }
    out
}

fn split_lane_slices_mut<T>(
    slice: &mut [T],
    offsets: [u32; LANE_COUNT + 1],
    total: usize,
) -> [&mut [T]; LANE_COUNT] {
    let [r0, r1, r2, r3, r4] = offsets;
    let clamp = |raw: u32| usize::try_from(raw).unwrap_or(0).min(total).min(slice.len());
    let (b0, b1, b2, b3, b4) = (clamp(r0), clamp(r1), clamp(r2), clamp(r3), clamp(r4));

    let size0 = b1.saturating_sub(b0);
    let size1 = b2.saturating_sub(b1);
    let size2 = b3.saturating_sub(b2);
    let size3 = b4.saturating_sub(b3);

    let (_prefix, rest) = slice.split_at_mut(b0);
    let (lane0, rest) = rest.split_at_mut(size0);
    let (lane1, rest) = rest.split_at_mut(size1);
    let (lane2, rest) = rest.split_at_mut(size2);
    let (lane3, _tail) = rest.split_at_mut(size3);

    [lane0, lane1, lane2, lane3]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::EffectProfile;

    fn sample_parsed() -> Vec<ParsedComponent> {
        vec![
            ParsedComponent {
                name: "App".to_string(),
                file_path: "src/App.tsx".to_string(),
                line_number: 1,
                imports: vec!["Button".to_string()],
                estimated_size: 1_024,
                is_default_export: true,
                props: Vec::new(),
                effect_profile: EffectProfile {
                    hooks: true,
                    ..EffectProfile::default()
                },
                source_hash: 0xA1B2_C3D4_0000_0001,
            },
            ParsedComponent {
                name: "Button".to_string(),
                file_path: "src/Button.tsx".to_string(),
                line_number: 7,
                imports: Vec::new(),
                estimated_size: 256,
                is_default_export: false,
                props: Vec::new(),
                effect_profile: EffectProfile::default(),
                source_hash: 0xA1B2_C3D4_0000_0002,
            },
        ]
    }

    #[test]
    fn string_interner_returns_stable_ids() {
        let mut interner = StringInterner::new();
        let a1 = interner.intern("App");
        let b1 = interner.intern("Button");
        let a2 = interner.intern("App");
        assert_eq!(a1, a2);
        assert_ne!(a1, b1);
        assert_eq!(interner.resolve(a1), "App");
        assert_eq!(interner.resolve(b1), "Button");
    }

    #[test]
    fn effect_bits_round_trip() {
        let profile = EffectProfile {
            hooks: true,
            asynchronous: false,
            io: true,
            side_effects: false,
        };
        let bits = effect_bits::pack(profile);
        assert_eq!(effect_bits::unpack(bits), profile);
    }

    #[test]
    fn columns_round_trip_preserves_document() {
        let parsed = sample_parsed();
        let columns = IrColumns::from_parsed(&parsed);

        assert_eq!(columns.len(), 2);
        assert_eq!(columns.edge_from().len(), 1);
        assert_eq!(columns.edge_to().len(), 1);

        let regenerated = columns.to_canonical();
        // Compare component-by-component rather than by document because
        // `generated_at` differs between the two builds.
        let original = super::super::build_canonical_ir_from_parsed(&parsed);
        assert_eq!(original.components, regenerated.components);
        assert_eq!(original.edges, regenerated.edges);
        assert_eq!(original.modules, regenerated.modules);
    }

    #[test]
    fn parallel_column_pass_splits_mutable_columns() {
        let mut columns = IrColumns::from_parsed(&sample_parsed());

        let effects_before = columns.effects().to_vec();
        let hashes_before = columns.source_hashes().to_vec();

        columns.parallel_column_pass(
            |effects| {
                for bits in effects.iter_mut() {
                    *bits |= effect_bits::IO;
                }
            },
            |hashes| {
                for hash in hashes.iter_mut() {
                    *hash ^= 0xFFFF_FFFF_FFFF_FFFF;
                }
            },
            |priorities, phases| {
                for value in priorities.iter_mut() {
                    *value += 1.0;
                }
                for value in phases.iter_mut() {
                    *value += 2.0;
                }
            },
            |_edge_from, _edge_to| {},
        );

        for (before, after) in effects_before.iter().zip(columns.effects().iter()) {
            assert_eq!(after & effect_bits::IO, effect_bits::IO);
            assert_eq!(before | effect_bits::IO, *after);
        }
        for (before, after) in hashes_before.iter().zip(columns.source_hashes().iter()) {
            assert_eq!(*after, before ^ 0xFFFF_FFFF_FFFF_FFFF);
        }
    }

    #[test]
    fn component_at_recovers_fields() {
        let columns = IrColumns::from_parsed(&sample_parsed());
        let slot = columns.index_of(1).expect("id 1 should exist");
        let component = columns.component_at(slot).expect("component at slot");
        assert_eq!(component.id, 1);
        assert!(matches!(
            component.export_kind,
            IrExportKind::Default | IrExportKind::Named
        ));
    }

    fn four_component_parsed() -> Vec<ParsedComponent> {
        (0..4)
            .map(|idx| ParsedComponent {
                name: format!("C{idx}"),
                file_path: format!("src/C{idx}.tsx"),
                line_number: idx as usize + 1,
                imports: if idx == 0 {
                    vec!["C3".to_string()]
                } else {
                    Vec::new()
                },
                estimated_size: 100 + idx as usize * 10,
                is_default_export: false,
                props: Vec::new(),
                effect_profile: EffectProfile::default(),
                source_hash: 0xDEAD_0000 | u64::from(idx as u32),
            })
            .collect()
    }

    #[test]
    fn fresh_columns_park_everything_in_lane_zero() {
        let columns = IrColumns::from_parsed(&four_component_parsed());
        let len = u32::try_from(columns.len()).unwrap_or(0);
        assert_eq!(columns.lane_offsets(), [0, len, len, len, len]);
        assert!(columns.lane_ids().iter().all(|lane| *lane == 0));
    }

    #[test]
    fn sort_by_lane_partitions_columns_into_contiguous_ranges() {
        let mut columns = IrColumns::from_parsed(&four_component_parsed());

        let ids_before = columns.ids().to_vec();
        let hashes_before = columns.source_hashes().to_vec();

        columns.sort_by_lane(|id| (id as usize) % LANE_COUNT);

        let offsets = columns.lane_offsets();
        assert_eq!(*offsets.first().expect("start"), 0);
        assert_eq!(
            *offsets.last().expect("total"),
            u32::try_from(columns.len()).expect("len fits in u32")
        );
        for lane in 0..LANE_COUNT {
            let start = *offsets.get(lane).expect("lane start");
            let end = *offsets.get(lane + 1).expect("lane end");
            assert!(start <= end, "lane offsets must be monotone");
            for &assignment in columns.lane_ids_slice(lane) {
                assert_eq!(usize::from(assignment), lane);
            }
        }

        let mut id_hash = ids_before.iter().zip(&hashes_before).collect::<Vec<_>>();
        id_hash.sort_by_key(|(id, _)| *id);
        for (id, expected_hash) in id_hash {
            let slot = columns.index_of(*id).expect("id preserved after sort");
            let actual_hash = *columns
                .source_hashes()
                .get(slot as usize)
                .expect("hash slot lives alongside id slot");
            assert_eq!(actual_hash, *expected_hash);
        }
    }

    #[test]
    fn sort_by_lane_remaps_edge_endpoints() {
        let parsed = four_component_parsed();
        let mut columns = IrColumns::from_parsed(&parsed);

        let edges_before = columns
            .edge_from()
            .iter()
            .zip(columns.edge_to())
            .map(|(from, to)| {
                (
                    columns.ids().get(*from as usize).copied().unwrap_or(0),
                    columns.ids().get(*to as usize).copied().unwrap_or(0),
                )
            })
            .collect::<Vec<_>>();

        columns.sort_by_lane(|id| (id as usize) % LANE_COUNT);

        let edges_after = columns
            .edge_from()
            .iter()
            .zip(columns.edge_to())
            .map(|(from, to)| {
                (
                    columns.ids().get(*from as usize).copied().unwrap_or(0),
                    columns.ids().get(*to as usize).copied().unwrap_or(0),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(edges_before, edges_after);
    }

    #[test]
    fn parallel_lane_column_pass_mutates_every_lane_independently() {
        let mut columns = IrColumns::from_parsed(&four_component_parsed());
        columns.sort_by_lane(|id| (id as usize) % LANE_COUNT);

        columns.parallel_lane_column_pass(
            |pass| {
                assert_eq!(pass.lane, 0);
                for slot in pass.effects.iter_mut() {
                    *slot |= effect_bits::HOOKS;
                }
            },
            |pass| {
                assert_eq!(pass.lane, 1);
                for slot in pass.source_hashes.iter_mut() {
                    *slot ^= 0xA5A5_A5A5_A5A5_A5A5;
                }
            },
            |pass| {
                assert_eq!(pass.lane, 2);
                for slot in pass.priorities.iter_mut() {
                    *slot += 1.0;
                }
            },
            |pass| {
                assert_eq!(pass.lane, 3);
                for slot in pass.phases.iter_mut() {
                    *slot += 1.0;
                }
            },
        );

        for lane in 0..LANE_COUNT {
            for slot in columns.lane_range(lane).expect("lane range") {
                match lane {
                    0 => assert!(
                        columns.effects().get(slot).copied().unwrap_or(0) & effect_bits::HOOKS != 0
                    ),
                    1 => assert_ne!(columns.source_hashes().get(slot).copied().unwrap_or(0), 0),
                    2 => assert!(columns.priorities().get(slot).copied().unwrap_or(0.0) >= 1.0),
                    3 => assert!(columns.phases().get(slot).copied().unwrap_or(0.0) >= 1.0),
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn build_lane_patch_snapshots_only_selected_fields() {
        let mut columns = IrColumns::from_parsed(&four_component_parsed());
        columns.sort_by_lane(|id| (id as usize) % LANE_COUNT);

        let slot = columns.index_of(1).expect("id 1 present");
        let full_mask = field_mask::SOURCE_HASH | field_mask::PRIORITY | field_mask::PHASE;
        let patch = columns
            .build_lane_patch(slot, full_mask)
            .expect("patch built");

        assert_eq!(patch.column_idx, slot);
        assert_eq!(patch.field_mask, full_mask);
        assert_eq!(
            patch.source_hash,
            columns.source_hashes().get(slot as usize).copied().unwrap()
        );
        assert_eq!(patch.effects, 0, "unselected field must stay at default");
    }

    #[test]
    fn sort_by_lane_clamps_out_of_range_lane_values() {
        let mut columns = IrColumns::from_parsed(&four_component_parsed());
        columns.sort_by_lane(|_| LANE_COUNT + 7);
        let offsets = columns.lane_offsets();
        let total = u32::try_from(columns.len()).unwrap_or(0);
        assert_eq!(*offsets.get(LANE_COUNT).expect("terminal"), total);
        assert_eq!(
            *offsets
                .get(LANE_COUNT.saturating_sub(1))
                .expect("last start"),
            0
        );
    }
}
