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

/// Wire configuration — pinned so both ends agree.
///
/// `standard()` = little-endian, variable-length integers, no limit.
#[inline]
fn config() -> impl bincode::config::Config {
    bincode::config::standard()
}

/// Encodes an [`OpcodeFrame`] into a compact binary representation.
///
/// The returned `Vec<u8>` is the payload that goes onto a WebTransport
/// stream as [`FramePayload::Binary`](crate::runtime::webtransport::FramePayload::Binary).
pub fn encode_frame(frame: &OpcodeFrame) -> Result<Vec<u8>, WireError> {
    Ok(bincode::encode_to_vec(frame, config())?)
}

/// Decodes an [`OpcodeFrame`] from a binary slice.
///
/// Returns the decoded frame. The caller owns the input slice and can
/// discard it after this call returns.
pub fn decode_frame(bytes: &[u8]) -> Result<OpcodeFrame, WireError> {
    let (frame, _bytes_read) = bincode::decode_from_slice(bytes, config())?;
    Ok(frame)
}

// ── InternTable ──────────────────────────────────────────────────────

/// Encodes an [`InternTable`] into a compact binary representation.
///
/// Typically sent once on the control stream at session init, with
/// incremental updates via subsequent calls.
pub fn encode_intern_table(table: &InternTable) -> Result<Vec<u8>, WireError> {
    Ok(bincode::encode_to_vec(table, config())?)
}

/// Decodes an [`InternTable`] from a binary slice.
pub fn decode_intern_table(bytes: &[u8]) -> Result<InternTable, WireError> {
    let (table, _bytes_read) = bincode::decode_from_slice(bytes, config())?;
    Ok(table)
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
        let decoded = decode_frame(&bytes).expect("decode must succeed");
        assert_eq!(frame, decoded);
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
        let decoded = decode_intern_table(&bytes).expect("decode must succeed");
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
        let decoded = decode_frame(&bytes).expect("decode");
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
        let decoded = decode_frame(&bytes).expect("decode");
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
                    range: InstructionRange { start: 0, end: 128 },
                },
            ],
        };
        let bytes = encode_frame(&frame).expect("encode");
        let decoded = decode_frame(&bytes).expect("decode");
        assert_eq!(frame, decoded);
    }
}
