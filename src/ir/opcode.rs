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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct InstructionRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
#[repr(u8)]
pub enum InternTableKind {
    Tag = 0,
    Attr = 1,
    Event = 2,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum Instruction {
    InitInternTable {
        table: InternTable,
    },

    Create {
        tag_id: TagId,
        stable_id: StableId,
    },

    SetAttr {
        stable_id: StableId,
        attr_id: AttrId,
        value: Vec<u8>,
    },

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
}

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
            range: InstructionRange {
                start: 64,
                end: 256,
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
                    range: InstructionRange { start: 0, end: 64 },
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

    #[test]
    fn intern_table_incremental_update_overwrites_entries() {
        let initial = InternTable {
            kind: InternTableKind::Event,
            entries: vec![
                InternEntry { id: 0, value: "click".to_string() },
                InternEntry { id: 1, value: "input".to_string() },
            ],
        };
        let update = InternTable {
            kind: InternTableKind::Event,
            entries: vec![
                InternEntry { id: 1, value: "change".to_string() },
            ],
        };

        let mut merged = std::collections::HashMap::new();
        for entry in &initial.entries {
            merged.insert(entry.id, entry.value.clone());
        }
        for entry in &update.entries {
            merged.insert(entry.id, entry.value.clone());
        }

        assert_eq!(merged.get(&0).map(String::as_str), Some("click"));
        assert_eq!(merged.get(&1).map(String::as_str), Some("change"));
    }
}
