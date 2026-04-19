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
}
