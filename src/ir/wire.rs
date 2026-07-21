use super::opcode::{InternTable, OpcodeFrame};

/// Errors produced by the wire encoding layer.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// Failed to encode an [`OpcodeFrame`] or [`InternTable`] into bytes.
    #[error("wire encode failed: {0}")]
    Encode(#[from] bincode::error::EncodeError),
    /// Failed to decode bytes into an [`OpcodeFrame`] or [`InternTable`].
    #[error("wire decode failed: {0}")]
    Decode(#[from] bincode::error::DecodeError),
}

/// The locked wire configuration shared by the Rust runtime and the
/// bakabox client decoder.
///
/// Every knob is spelled out, even when it matches `bincode::config::standard()`
/// today, because `standard()` is documented as "the current sane defaults"
/// — a future bincode release is free to flip a default and silently change
/// every byte we put on the wire. The bakabox decoder is hand-rolled JS; it
/// has no way to track bincode version drift. Pinning each knob makes any
/// regression a compile break here, not a 3am production wire-format break.
///
/// Knobs and why they are pinned:
///
/// - **Little-endian**: bakabox runs on the browser, which is overwhelmingly
///   little-endian. Choosing it here means the JS decoder reads bytes in the
///   same order they were written without per-field swaps.
/// - **Variable-length integers**: most opcode fields are small `u16`/`u32`
///   IDs (TagId, AttrId, StableId). Varint shrinks the average frame by ~30%
///   versus fixed-width encoding without costing the decoder anything that
///   matters at frame sizes we ship (sub-kilobyte typical).
/// - **No limit**: the encode side trusts its own output — frames we build are
///   bounded by construction and the WebTransport muxer budgets them at the
///   transport layer ([`crate::runtime::webtransport`]). The *decode* side is a
///   different story: it consumes attacker-controlled bytes, so it does NOT
///   inherit this no-limit config (see [`decode_config`]). Splitting the two
///   keeps the on-wire bytes of every valid frame byte-identical — the limit is
///   a decode-time acceptance bound, not a serialization knob — so the
///   conformance contract below is unaffected.
///
/// Any change to **this** (encode) configuration is a wire-format break and
/// MUST be accompanied by a bump of
/// [`crate::ir::conformance::LOCKED_WIRE_VERSION`] and a regenerated
/// `tests/fixtures/wire/v4_canonical_frame.bin`. Changing [`decode_config`]'s
/// limit is not a format break (valid frames decode identically).
#[inline]
pub(crate) fn config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_little_endian()
        .with_variable_int_encoding()
        .with_no_limit()
}

/// Upper bound on the bytes any single wire decode may read or allocate.
///
/// A bincode decoder with no limit trusts the length prefix of a `Vec`/`String`
/// and pre-allocates that capacity — so a handful of bytes claiming a petabyte
/// length triggers an unbounded allocation that aborts the process (an
/// uncatchable OOM, not a `panic!`). This is the classic *decode bomb*. The
/// most exposed caller is [`crate::ir::action::decode_action_envelope`], fed
/// straight from a `POST /_albedo/action` body; the HTTP layer caps that body
/// at 2 MiB, so this ceiling sits comfortably above it while staying orders of
/// magnitude below "exhaust the host". No legitimate opcode frame (sub-kilobyte
/// typical) or action envelope approaches it.
pub(crate) const MAX_WIRE_DECODE_BYTES: usize = 8 * 1024 * 1024;

/// Decode-side configuration: identical format knobs to [`config`] (so valid
/// bytes decode the same), plus a hard byte limit so a forged length prefix is
/// rejected as a typed [`WireError::Decode`] instead of triggering a runaway
/// allocation. Used by every [`WireDecode`] impl; never by encode.
#[inline]
fn decode_config() -> impl bincode::config::Config {
    bincode::config::standard()
        .with_little_endian()
        .with_variable_int_encoding()
        .with_limit::<MAX_WIRE_DECODE_BYTES>()
}

// ── Codec traits ─────────────────────────────────────────────────────
//
// PHASE 2 (B-emitter) — Pinaki: emit through these traits
// (`frame.wire_encode()`), not through the free functions. Reason: when we
// swap bincode for FlatBuffers later, only this file changes. The free
// functions below stay as ergonomic wrappers but they are not the contract
// boundary. — Bishal-albdo@may-2026

/// Encode any wire-shaped type to bytes.
///
/// Implemented for every `T: bincode::Encode` via a blanket impl, so any
/// type derived with `#[derive(Encode, Decode)]` is automatically wirable.
pub trait WireEncode {
    fn wire_encode(&self) -> Result<Vec<u8>, WireError>;
}

/// Decode bytes into a wire-shaped type, returning the decoded value and
/// the number of bytes consumed.
pub trait WireDecode: Sized {
    fn wire_decode(bytes: &[u8]) -> Result<(Self, usize), WireError>;
}

impl<T: bincode::Encode> WireEncode for T {
    fn wire_encode(&self) -> Result<Vec<u8>, WireError> {
        Ok(bincode::encode_to_vec(self, config())?)
    }
}

impl<T: bincode::Decode<()>> WireDecode for T {
    fn wire_decode(bytes: &[u8]) -> Result<(Self, usize), WireError> {
        // Bounded decode config — see `decode_config`. A malformed length
        // prefix errors out here rather than allocating from it.
        let (value, len) = bincode::decode_from_slice(bytes, decode_config())?;
        Ok((value, len))
    }
}

// ── Free-function wrappers ───────────────────────────────────────────
//
// These remain the ergonomic call sites for callers that have a concrete
// type at hand. They delegate straight to the trait — no parallel codec
// path.

/// Encodes an [`OpcodeFrame`] into a compact binary representation.
///
/// The returned `Vec<u8>` is the payload that goes onto a WebTransport
/// stream as [`FramePayload::Binary`](crate::runtime::webtransport::FramePayload::Binary).
pub fn encode_frame(frame: &OpcodeFrame) -> Result<Vec<u8>, WireError> {
    frame.wire_encode()
}

/// Decodes an [`OpcodeFrame`] from a binary slice. Returns the frame and
/// the number of bytes consumed.
///
/// Caller MUST have already reassembled all WebTransport STREAM messages
/// sharing the same `frame_id` via
/// [`crate::runtime::webtransport::WebTransportMuxer::reassemble_binary_stream`]
/// before calling this. See [`OpcodeFrame`] for the concatenation contract.
pub fn decode_frame(bytes: &[u8]) -> Result<(OpcodeFrame, usize), WireError> {
    OpcodeFrame::wire_decode(bytes)
}

// ── InternTable ──────────────────────────────────────────────────────

/// Encodes an [`InternTable`] into a compact binary representation.
///
/// Typically sent once on the control stream at session init, with
/// incremental updates via [`crate::ir::opcode::Instruction::PatchInternTable`].
pub fn encode_intern_table(table: &InternTable) -> Result<Vec<u8>, WireError> {
    table.wire_encode()
}

/// Decodes an [`InternTable`] from a binary slice.
pub fn decode_intern_table(bytes: &[u8]) -> Result<(InternTable, usize), WireError> {
    InternTable::wire_decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::opcode::*;

    #[test]
    fn encode_decode_frame_round_trips() {
        let frame = OpcodeFrame {
            frame_id: 1,
            component_id: Some(42),
            instructions: vec![
                Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(1),
                },
                Instruction::SetAttr {
                    stable_id: StableId(1),
                    attr_id: AttrId(0),
                    value: b"root".to_vec(),
                },
                Instruction::Append {
                    parent_id: StableId(0),
                    child_id: StableId(1),
                },
            ],
        };

        let bytes = encode_frame(&frame).expect("encode must succeed");
        let (decoded, _) = decode_frame(&bytes).expect("decode must succeed");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn frame_round_trips_via_trait_calls() {
        let frame = OpcodeFrame {
            frame_id: 2,
            component_id: None,
            instructions: vec![Instruction::Create {
                tag_id: TagId(1),
                stable_id: StableId(7),
            }],
        };
        let bytes = frame.wire_encode().expect("trait encode");
        let (decoded, _) = OpcodeFrame::wire_decode(&bytes).expect("trait decode");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn intern_table_round_trips_via_trait_calls() {
        let table = InternTable {
            kind: InternTableKind::Tag,
            entries: vec![InternEntry {
                id: 0,
                value: "div".to_string(),
            }],
        };
        let bytes = table.wire_encode().expect("trait encode");
        let (decoded, _) = InternTable::wire_decode(&bytes).expect("trait decode");
        assert_eq!(table, decoded);
    }

    #[test]
    fn encode_decode_intern_table_round_trips() {
        let table = InternTable {
            kind: InternTableKind::Tag,
            entries: vec![
                InternEntry {
                    id: 0,
                    value: "div".to_string(),
                },
                InternEntry {
                    id: 1,
                    value: "span".to_string(),
                },
                InternEntry {
                    id: 2,
                    value: "article".to_string(),
                },
            ],
        };

        let bytes = encode_intern_table(&table).expect("encode must succeed");
        let (decoded, _) = decode_intern_table(&bytes).expect("decode must succeed");
        assert_eq!(table, decoded);
    }

    #[test]
    fn corrupt_bytes_produce_wire_error() {
        let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0xFB];
        let result = decode_frame(&garbage);
        assert!(result.is_err(), "corrupt input must produce WireError");

        let result = decode_intern_table(&garbage);
        assert!(result.is_err(), "corrupt input must produce WireError");
    }

    #[test]
    fn empty_frame_round_trips() {
        let frame = OpcodeFrame {
            frame_id: 0,
            component_id: None,
            instructions: Vec::new(),
        };
        let bytes = encode_frame(&frame).expect("encode");
        let (decoded, _) = decode_frame(&bytes).expect("decode");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn create_instruction_encodes_compactly() {
        let frame = OpcodeFrame {
            frame_id: 0,
            component_id: None,
            instructions: vec![Instruction::Create {
                tag_id: TagId(0),
                stable_id: StableId(1),
            }],
        };
        let bytes = encode_frame(&frame).expect("encode");
        // Frame overhead (frame_id varint + component_id Option + instructions len)
        // + Create payload. Should be well under 20 bytes total.
        assert!(
            bytes.len() <= 20,
            "single-Create frame should be compact, got {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn large_frame_round_trips() {
        let instructions: Vec<Instruction> = (0..1000)
            .map(|i| Instruction::Create {
                tag_id: TagId(u16::try_from(i % 100).unwrap_or(0)),
                stable_id: StableId(i),
            })
            .collect();

        let frame = OpcodeFrame {
            frame_id: 999,
            component_id: Some(1),
            instructions,
        };
        let bytes = encode_frame(&frame).expect("encode");
        let (decoded, _) = decode_frame(&bytes).expect("decode");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn frame_with_all_instruction_types_round_trips() {
        let frame = OpcodeFrame {
            frame_id: 100,
            component_id: Some(5),
            instructions: vec![
                Instruction::InitInternTable {
                    table: InternTable {
                        kind: InternTableKind::Tag,
                        entries: vec![InternEntry {
                            id: 0,
                            value: "div".to_string(),
                        }],
                    },
                },
                Instruction::PatchInternTable {
                    kind: InternTableKind::Attr,
                    ops: vec![
                        InternPatchOp::Set { id: 0, value: "class".to_string() },
                        InternPatchOp::Remove { id: 1 },
                    ],
                },
                Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(1),
                },
                Instruction::SetAttr {
                    stable_id: StableId(1),
                    attr_id: AttrId(0),
                    value: b"class-name".to_vec(),
                },
                Instruction::SetText {
                    stable_id: StableId(1),
                    text: b"content".to_vec(),
                },
                Instruction::Append {
                    parent_id: StableId(0),
                    child_id: StableId(1),
                },
                Instruction::Remove {
                    stable_id: StableId(99),
                },
                Instruction::BindEvent {
                    stable_id: StableId(1),
                    event_id: EventId(0),
                    proxy_id: ProxyId(42),
                },
                Instruction::BindSlot {
                    stable_id: StableId(1),
                    slot_id: SlotId(7),
                },
                Instruction::Placeholder {
                    stable_id: StableId(2),
                    suspense_id: SuspenseId(10),
                },
                Instruction::Patch {
                    suspense_id: SuspenseId(10),
                    range: InstructionRange::try_new(0, 128).expect("valid range"),
                },
                Instruction::SetTextRef {
                    stable_id: StableId(1),
                    slot_id: SlotId(9),
                },
                Instruction::SetAttrRef {
                    stable_id: StableId(1),
                    attr_id: AttrId(0),
                    slot_id: SlotId(11),
                },
                Instruction::SlotSet {
                    slot_id: SlotId(9),
                    value: b"reactive".to_vec(),
                },
            ],
        };
        let bytes = encode_frame(&frame).expect("encode");
        let (decoded, _) = decode_frame(&bytes).expect("decode");
        assert_eq!(frame, decoded);
    }
}
