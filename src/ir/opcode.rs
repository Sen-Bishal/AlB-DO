use bincode::{Decode, Encode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct TagId(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct AttrId(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct EventId(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct StableId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ProxyId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SlotId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SuspenseId(pub u32);

/// Reconciliation identity for a z-set row — *which* DOM node a change
/// targets, deliberately distinct from the row's payload (which weight it
/// carries). FORGE-backed rows key on their primary key, client `useState`
/// arrays on the `key` prop, keyless lists on position — all stringified into
/// this one wire type so the client sink can use it directly as a map key.
///
/// A `String` (UTF-8), like [`InternEntry::value`]: it holds an identifier,
/// never arbitrary payload. Row payload bytes stay `Vec<u8>` (see
/// [`SlotChange::payload`]) so they can carry non-UTF-8 rendered markup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RowKey(pub String);

/// Half-open `[start, end)` byte range.
///
/// **Phase D semantics (v1):** offsets resolve into the bytes of the
/// **`OpcodeFrame` carrying this `Patch`** — *not* the placeholder's
/// original frame. Async resolutions ship as standalone frames with a
/// fresh `frame_id`; the range, when non-empty, identifies the slice of
/// the same frame's reassembled bytes that holds the resolved opcode
/// instructions. An empty range (`start == end`) means "the resolved
/// opcodes are the remaining instructions in this frame after the
/// `Patch` opcode" — which is what the v1 emitter currently produces.
///
/// Reassembly via
/// [`crate::runtime::webtransport::WebTransportMuxer::reassemble_binary_stream`]
/// still happens BEFORE [`crate::ir::wire::decode_frame`]; the only
/// change from Phase A is that a `Patch` is now a self-contained
/// frame, decoupled from the `Placeholder`'s original frame.
///
/// Fields are private; construct via [`InstructionRange::try_new`] so
/// the invariant `start <= end` holds at every wire boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct InstructionRange {
    start: u32,
    end: u32,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RangeError {
    #[error("instruction range invalid: start ({start}) > end ({end})")]
    StartAfterEnd { start: u32, end: u32 },
}

impl InstructionRange {
    /// Constructs a range, rejecting `start > end`.
    pub fn try_new(start: u32, end: u32) -> Result<Self, RangeError> {
        if start > end {
            return Err(RangeError::StartAfterEnd { start, end });
        }
        Ok(Self { start, end })
    }

    #[inline]
    pub fn start(&self) -> u32 {
        self.start
    }

    #[inline]
    pub fn end(&self) -> u32 {
        self.end
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
#[repr(u8)]
pub enum InternTableKind {
    Tag = 0,
    Attr = 1,
    Event = 2,
}

/// One entry in an [`InternTable`].
///
/// `value` is `String` (UTF-8) by deliberate choice: the intern table holds
/// **identifiers** (tag/attr/event names), never arbitrary user payload.
/// Inline value bytes for `SetAttr` / `SetText` are still `Vec<u8>` so they
/// can carry non-UTF-8 binary attribute values.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct InternEntry {
    pub id: u16,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct InternTable {
    pub kind: InternTableKind,
    pub entries: Vec<InternEntry>,
}

/// One mutation against an existing intern table.
///
/// Carried inside [`Instruction::PatchInternTable`]. `Set` upserts; `Remove`
/// drops the entry by id. Bootstrap remains
/// [`Instruction::InitInternTable`] (single-shot, ships the full table) so
/// the warm path stays a single byte of discriminant.
//
// PHASE 2 (B-emitter) — Pinaki: the compiler's intern-table-diff pass
// emits these ops on the control stream when the JSX corpus grows new
// tags/attrs/events between hot reloads. `Remove` is for evicting an id
// the client must drop from its mirror; do not reuse it for "rename".
// — Bishal-albdo@may-2026
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum InternPatchOp {
    Set { id: u16, value: String },
    Remove { id: u16 },
}

/// One signed change in a z-set delta ([`Instruction::SlotDelta`]).
///
/// `weight` is the multiplicity delta in the signed-multiset algebra: `+1`
/// inserts a row, `−1` retracts one, and aggregations (`count`/`sum`) produce
/// other integers. `key` is the reconciliation identity ([`RowKey`]); `payload`
/// is the row's rendered bytes (server-rendered HTML first slice — a compiled
/// template op is a later rung).
///
/// The client pairs a `−`/`+` on the *same* `key` within one delta into a
/// single in-place patch; a lone `+` inserts, a lone `−` removes. Coalescing on
/// the emit side keys on the whole record (`key` **and** `payload`), never on
/// `key` alone — else an update (retract old + insert new) would cancel to
/// weight-0 and drop the edit.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct SlotChange {
    pub weight: i32,
    pub key: RowKey,
    pub payload: Vec<u8>,
}

/// One row of a full-set reconcile ([`Instruction::ReconcileList`]).
///
/// Where a [`SlotChange`] is a *signed* edit whose position is implied by
/// arrival order (inserts land at the tail), a `ReconcileRow` is a positional
/// assertion: the desired rows in desired order, `payload` being the row's
/// rendered bytes and `key` its reconciliation identity. The client walks the
/// list in order, so `ReconcileList` expresses reorder and mid-insert — the
/// transitions a tail-appending `SlotDelta` cannot.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ReconcileRow {
    pub key: RowKey,
    pub payload: Vec<u8>,
}

/// The opcode set that drives the client's slot-table.
///
/// Wire-format note: variant order is the bincode discriminant. Adding new
/// variants is allowed; **reordering existing ones is a wire break**. Any
/// reorder requires a coordinated client/server upgrade.
//
// PHASE 2 (B-emitter) — Pinaki: `src/runtime/emitter.rs` is the only
// producer. Walk `IrColumns` lane slices via the existing `LaneColumnPass`
// (`src/ir/columns.rs`), emit one `OpcodeFrame` per lane, hand the encoded
// bytes to `WTStreamRouter` as `FramePayload::Binary`. Do NOT mix `SetText`
// with `SetTextRef` for the same `stable_id` in one frame — pick one
// according to whether the value is reactive or static.
// — Bishal-albdo@may-2026
//
// PHASE 3 (C-client) — every variant here needs a `case` in the client
// `switch(op)` in `assets/albedo-runtime.js` (currently a 46-line
// outerHTML patcher; will be rewritten to ~300 lines).
// — Bishal-albdo@may-2026
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum Instruction {
    /// Bootstrap an intern table on the control stream. Full table.
    InitInternTable {
        table: InternTable,
    },

    /// Incremental updates to an existing intern table.
    ///
    /// Sent on the control stream after the bootstrap `InitInternTable`.
    /// The client applies `ops` in order against its mirror of `kind`.
    PatchInternTable {
        kind: InternTableKind,
        ops: Vec<InternPatchOp>,
    },

    Create {
        tag_id: TagId,
        stable_id: StableId,
    },

    /// Static attribute write — value is inlined.
    SetAttr {
        stable_id: StableId,
        attr_id: AttrId,
        value: Vec<u8>,
    },

    /// Static text write — value is inlined.
    SetText {
        stable_id: StableId,
        text: Vec<u8>,
    },

    Append {
        parent_id: StableId,
        child_id: StableId,
    },

    Remove {
        stable_id: StableId,
    },

    BindEvent {
        stable_id: StableId,
        event_id: EventId,
        proxy_id: ProxyId,
    },

    BindSlot {
        stable_id: StableId,
        slot_id: SlotId,
    },

    Placeholder {
        stable_id: StableId,
        suspense_id: SuspenseId,
    },

    Patch {
        suspense_id: SuspenseId,
        range: InstructionRange,
    },

    // ── Alt D — reactive slot refs ─────────────────────────────────
    //
    // PHASE 5 (E-hooks) — Pinaki: these are the wire-level binding for
    // `useState` / `useEffect` server-side mirrors. Emitter produces a
    // `SetTextRef` / `SetAttrRef` once at create-time; subsequent updates
    // arrive as `SlotSet` carrying only the new value bytes against
    // `slot_id`. The client keeps a `Map<SlotId, {kind, target}>` and
    // re-applies on each `SlotSet`. Folded into the wire NOW (not in v2)
    // because deferring would force every deployed client to upgrade
    // when reactive bindings land. — Bishal-albdo@may-2026
    /// Bind a text node's content to a server-side reactive slot.
    SetTextRef {
        stable_id: StableId,
        slot_id: SlotId,
    },

    /// Bind an attribute to a server-side reactive slot.
    SetAttrRef {
        stable_id: StableId,
        attr_id: AttrId,
        slot_id: SlotId,
    },

    /// Push a new value into a slot. Client re-applies to every bound site.
    SlotSet {
        slot_id: SlotId,
        value: Vec<u8>,
    },

    /// Phase-I — instruct the bakabox client to navigate to `url`.
    ///
    /// Added as variant index 14 at the end of the enum so existing
    /// wire decoders that haven't been recompiled still parse every
    /// pre-existing variant correctly. A bakabox decoder that does
    /// not know variant 14 surfaces a typed error (see the JS
    /// decoder's `INSTRUCTION_NAMES` length check) rather than
    /// silently mis-aligning subsequent reads.
    ///
    /// `LOCKED_WIRE_VERSION` bumps to 2 alongside this variant.
    Navigate {
        url: String,
    },

    /// Push a **z-set delta** into a keyed slot — the incremental-view
    /// primitive for lists and derived collections.
    ///
    /// Each [`SlotChange`] applies by its [`RowKey`]: `+weight` inserts (or, when
    /// paired with a same-key `−`, patches in place) the row; `−weight` retracts
    /// it. This is keyed reconciliation with identity coming from the algebra,
    /// not a hand-written differ. Scalar [`Instruction::SlotSet`] is the
    /// degenerate singleton case (`{−old, +new}` on one row).
    ///
    /// Added as variant index 15 at the end of the enum, so a decoder that
    /// doesn't know it surfaces a typed error rather than mis-aligning
    /// subsequent reads. `LOCKED_WIRE_VERSION` bumps to 3 alongside this variant.
    SlotDelta {
        slot_id: SlotId,
        changes: Vec<SlotChange>,
    },

    /// Reconcile a keyed slot against the **full desired row set**, in order.
    ///
    /// The positional counterpart to [`Instruction::SlotDelta`]: the sink drops
    /// rows whose key is absent from `rows`, upserts the rest by key, and moves
    /// each into `rows` order (an unchanged row keeps its DOM node, so identity —
    /// focus, selection, scroll — survives). This is what a `SlotDelta` cannot
    /// express — a reorder, or an insert that is not at the tail — and what a
    /// reconnecting client is resynced with, since a full set can retract a row
    /// that a positive-only upsert would leave behind as a ghost.
    ///
    /// Added as variant index 16 at the end of the enum, so a decoder that does
    /// not know it surfaces a typed error rather than mis-aligning subsequent
    /// reads. `LOCKED_WIRE_VERSION` bumps to 4 alongside this variant.
    ReconcileList {
        slot_id: SlotId,
        rows: Vec<ReconcileRow>,
    },

    /// Insert rows into a keyed slot at a **named position**.
    ///
    /// The missing rung between [`Instruction::SlotDelta`] and
    /// [`Instruction::ReconcileList`]. A `SlotDelta` insert is `O(|Δ|)` but
    /// always lands at the tail, so any non-tail insert — the head row of a
    /// `created_at DESC` feed being the common one — had to fall back to a
    /// `ReconcileList` that re-asserts the whole view: `O(|view|)` on the wire
    /// *and* a whole-view server render. `SlotInsert` keeps both sides
    /// `O(|Δ|)` by naming the anchor instead of re-sending the set.
    ///
    /// `before` is the [`RowKey`] the new rows are inserted ahead of; `None`
    /// means "at the tail" (the degenerate case a `SlotDelta` already covers,
    /// kept so one opcode expresses every single-position insert). `rows` are
    /// inserted in order, immediately before the anchor.
    ///
    /// The anchor is a *position assertion*, not a full-set assertion: unlike
    /// `ReconcileList` this op retracts nothing, so it must only be emitted
    /// when the producer knows the rest of the view is unchanged. A client
    /// that does not hold `before` cannot honour the position; it appends and
    /// waits for the next resync `ReconcileList` to correct the order, which
    /// is the same correctness-via-fallback the tail-append classifier uses.
    ///
    /// Added as variant index 17 at the end of the enum, so a decoder that
    /// does not know it surfaces a typed error rather than mis-aligning
    /// subsequent reads. `LOCKED_WIRE_VERSION` bumps to 5 alongside this
    /// variant.
    SlotInsert {
        slot_id: SlotId,
        before: Option<RowKey>,
        rows: Vec<ReconcileRow>,
    },
}

/// One wire frame.
///
/// **Concatenation contract:** an `OpcodeFrame` MAY span multiple
/// WebTransport STREAM messages
/// ([`crate::runtime::webtransport::WebTransportFrame`]s sharing the same
/// `frame_id`). The decoder reassembles the concatenated byte buffer via
/// [`crate::runtime::webtransport::WebTransportMuxer::reassemble_binary_stream`]
/// BEFORE invoking [`crate::ir::wire::decode_frame`]. Per-message decoding
/// is unsupported; do not assume frame == STREAM message.
//
// PHASE 2 (B-emitter) — Pinaki: `frame_id` MUST be allocated via the
// per-stream sequence in `WebTransportMuxer::allocate_sequence` so a frame
// split across STREAM messages stays attributable to the same logical
// frame on reassembly. `component_id` is `None` for cross-component
// patches (rare; only emitted on the control stream).
// — Bishal-albdo@may-2026
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct OpcodeFrame {
    pub frame_id: u64,
    pub component_id: Option<u64>,
    pub instructions: Vec<Instruction>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_create_round_trips_through_encode_decode() {
        let instruction = Instruction::Create {
            tag_id: TagId(1),
            stable_id: StableId(42),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_set_attr_round_trips() {
        let instruction = Instruction::SetAttr {
            stable_id: StableId(7),
            attr_id: AttrId(3),
            value: b"my-class".to_vec(),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_set_text_round_trips() {
        let instruction = Instruction::SetText {
            stable_id: StableId(10),
            text: "Hello, world! 🌍".as_bytes().to_vec(),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_append_round_trips() {
        let instruction = Instruction::Append {
            parent_id: StableId(1),
            child_id: StableId(2),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_remove_round_trips() {
        let instruction = Instruction::Remove {
            stable_id: StableId(99),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_bind_event_round_trips() {
        let instruction = Instruction::BindEvent {
            stable_id: StableId(5),
            event_id: EventId(0),
            proxy_id: ProxyId(1001),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_bind_slot_round_trips() {
        let instruction = Instruction::BindSlot {
            stable_id: StableId(8),
            slot_id: SlotId(200),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_placeholder_round_trips() {
        let instruction = Instruction::Placeholder {
            stable_id: StableId(30),
            suspense_id: SuspenseId(500),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_patch_round_trips() {
        let instruction = Instruction::Patch {
            suspense_id: SuspenseId(500),
            range: InstructionRange::try_new(64, 256).expect("valid range"),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn instruction_init_intern_table_round_trips() {
        let instruction = Instruction::InitInternTable {
            table: InternTable {
                kind: InternTableKind::Tag,
                entries: vec![
                    InternEntry { id: 0, value: "div".to_string() },
                    InternEntry { id: 1, value: "span".to_string() },
                    InternEntry { id: 2, value: "section".to_string() },
                ],
            },
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn patch_intern_table_round_trips() {
        let instruction = Instruction::PatchInternTable {
            kind: InternTableKind::Event,
            ops: vec![
                InternPatchOp::Set { id: 1, value: "change".to_string() },
                InternPatchOp::Remove { id: 0 },
                InternPatchOp::Set { id: 2, value: "submit".to_string() },
            ],
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn set_text_ref_round_trips() {
        let instruction = Instruction::SetTextRef {
            stable_id: StableId(7),
            slot_id: SlotId(42),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn set_attr_ref_round_trips() {
        let instruction = Instruction::SetAttrRef {
            stable_id: StableId(7),
            attr_id: AttrId(3),
            slot_id: SlotId(99),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn slot_set_round_trips() {
        let instruction = Instruction::SlotSet {
            slot_id: SlotId(42),
            value: b"new-text".to_vec(),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn slot_delta_round_trips() {
        let instruction = Instruction::SlotDelta {
            slot_id: SlotId(42),
            changes: vec![
                // insert
                SlotChange {
                    weight: 1,
                    key: RowKey("row-7".to_string()),
                    payload: b"<li>alice</li>".to_vec(),
                },
                // retract
                SlotChange {
                    weight: -1,
                    key: RowKey("row-3".to_string()),
                    payload: Vec::new(),
                },
                // update = retract old + insert new on the same key
                SlotChange {
                    weight: -1,
                    key: RowKey("row-9".to_string()),
                    payload: b"<li>old</li>".to_vec(),
                },
                SlotChange {
                    weight: 1,
                    key: RowKey("row-9".to_string()),
                    payload: b"<li>new</li>".to_vec(),
                },
            ],
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    /// The `Some` arm: rows land ahead of a named anchor. This is the shape a
    /// reverse-chron feed emits — the new row goes before the current head.
    #[test]
    fn slot_insert_before_anchor_round_trips() {
        let instruction = Instruction::SlotInsert {
            slot_id: SlotId(42),
            before: Some(RowKey("row-3".to_string())),
            rows: vec![ReconcileRow {
                key: RowKey("row-9".to_string()),
                payload: b"<li>newest</li>".to_vec(),
            }],
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    /// The `None` arm — tail append. The conformance fixture only carries the
    /// `Some` arm (one instruction per variant), so the option's absent tag is
    /// covered here or nowhere.
    #[test]
    fn slot_insert_at_tail_round_trips() {
        let instruction = Instruction::SlotInsert {
            slot_id: SlotId(42),
            before: None,
            rows: vec![
                ReconcileRow {
                    key: RowKey("row-9".to_string()),
                    payload: b"<li>alice</li>".to_vec(),
                },
                ReconcileRow {
                    key: RowKey("row-10".to_string()),
                    payload: b"<li>bob</li>".to_vec(),
                },
            ],
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (Instruction, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(instruction, decoded);
    }

    #[test]
    fn frame_with_alt_d_instructions_round_trips() {
        let frame = OpcodeFrame {
            frame_id: 17,
            component_id: Some(3),
            instructions: vec![
                Instruction::SetTextRef {
                    stable_id: StableId(1),
                    slot_id: SlotId(10),
                },
                Instruction::SetAttrRef {
                    stable_id: StableId(1),
                    attr_id: AttrId(0),
                    slot_id: SlotId(11),
                },
                Instruction::SlotSet {
                    slot_id: SlotId(10),
                    value: b"hello".to_vec(),
                },
            ],
        };
        let bytes =
            bincode::encode_to_vec(&frame, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (OpcodeFrame, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn instruction_range_try_new_rejects_start_after_end() {
        let err = InstructionRange::try_new(100, 50).unwrap_err();
        assert_eq!(
            err,
            RangeError::StartAfterEnd { start: 100, end: 50 }
        );
    }

    #[test]
    fn instruction_range_try_new_accepts_equal_start_end() {
        let range = InstructionRange::try_new(64, 64).expect("equal endpoints are valid");
        assert!(range.is_empty());
        assert_eq!(range.len(), 0);
    }

    #[test]
    fn instruction_range_accessors_match_constructor_inputs() {
        let range = InstructionRange::try_new(10, 42).expect("valid");
        assert_eq!(range.start(), 10);
        assert_eq!(range.end(), 42);
        assert_eq!(range.len(), 32);
        assert!(!range.is_empty());
    }

    #[test]
    fn opcode_frame_with_mixed_instructions_round_trips() {
        let frame = OpcodeFrame {
            frame_id: 42,
            component_id: Some(7),
            instructions: vec![
                Instruction::InitInternTable {
                    table: InternTable {
                        kind: InternTableKind::Tag,
                        entries: vec![
                            InternEntry { id: 0, value: "div".to_string() },
                        ],
                    },
                },
                Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(1),
                },
                Instruction::SetAttr {
                    stable_id: StableId(1),
                    attr_id: AttrId(0),
                    value: b"container".to_vec(),
                },
                Instruction::SetText {
                    stable_id: StableId(1),
                    text: b"hello".to_vec(),
                },
                Instruction::Append {
                    parent_id: StableId(0),
                    child_id: StableId(1),
                },
                Instruction::BindEvent {
                    stable_id: StableId(1),
                    event_id: EventId(0),
                    proxy_id: ProxyId(100),
                },
                Instruction::BindSlot {
                    stable_id: StableId(1),
                    slot_id: SlotId(50),
                },
                Instruction::Placeholder {
                    stable_id: StableId(2),
                    suspense_id: SuspenseId(10),
                },
                Instruction::Remove {
                    stable_id: StableId(99),
                },
                Instruction::Patch {
                    suspense_id: SuspenseId(10),
                    range: InstructionRange::try_new(0, 64).expect("valid range"),
                },
            ],
        };
        let bytes =
            bincode::encode_to_vec(&frame, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (OpcodeFrame, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn opcode_frame_with_no_component_id_round_trips() {
        let frame = OpcodeFrame {
            frame_id: 0,
            component_id: None,
            instructions: vec![
                Instruction::Create {
                    tag_id: TagId(5),
                    stable_id: StableId(100),
                },
            ],
        };
        let bytes =
            bincode::encode_to_vec(&frame, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (OpcodeFrame, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn intern_table_round_trips() {
        let table = InternTable {
            kind: InternTableKind::Attr,
            entries: vec![
                InternEntry { id: 0, value: "class".to_string() },
                InternEntry { id: 1, value: "id".to_string() },
                InternEntry { id: 2, value: "href".to_string() },
            ],
        };
        let bytes =
            bincode::encode_to_vec(&table, bincode::config::standard())
                .expect("encode");
        let (decoded, _): (InternTable, _) =
            bincode::decode_from_slice(&bytes, bincode::config::standard())
                .expect("decode");
        assert_eq!(table, decoded);
    }

    #[test]
    fn create_instruction_encodes_compactly() {
        let instruction = Instruction::Create {
            tag_id: TagId(0),
            stable_id: StableId(1),
        };
        let bytes =
            bincode::encode_to_vec(&instruction, bincode::config::standard())
                .expect("encode");
        assert!(
            bytes.len() <= 8,
            "Create instruction should encode to ≤ 8 bytes, got {}",
            bytes.len()
        );
    }
}
