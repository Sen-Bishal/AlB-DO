//! Wire-format conformance fixture for the bakabox client.
//!
//! [`canonical_v1_frame`] returns a deterministic [`OpcodeFrame`] that
//! exercises every [`Instruction`] discriminant in the locked Phase-A wire
//! format. The Rust side encodes this frame and writes the bytes to a
//! checked-in fixture file under `tests/fixtures/wire/`. The bakabox JS
//! decoder (`assets/albedo-bincode.js`) reads the same fixture and must
//! produce a structurally identical frame, byte-for-byte.
//!
//! This module is the **single source of truth** for the wire contract
//! between the Rust runtime and bakabox. Any change to [`Instruction`]
//! variant order, field order, or codec configuration that does not also
//! bump [`LOCKED_WIRE_VERSION`] and update the fixture is a silent break.

use super::opcode::{
    AttrId, EventId, Instruction, InstructionRange, InternEntry, InternPatchOp, InternTable,
    InternTableKind, OpcodeFrame, ProxyId, SlotId, StableId, SuspenseId, TagId,
};

/// Wire format version. Bump whenever any of the following change:
///
/// - The `bincode::Configuration` returned by [`super::wire::config`].
/// - The variant order of [`Instruction`].
/// - The field order of any opcode payload struct.
/// - The shape of [`OpcodeFrame`], [`InternTable`], or [`InternPatchOp`].
///
/// The bakabox client decoder MUST refuse to decode any frame whose wire
/// version it does not recognise. A version bump on the server requires a
/// coordinated client upgrade.
pub const LOCKED_WIRE_VERSION: u32 = 1;

/// Returns the deterministic conformance frame for [`LOCKED_WIRE_VERSION`].
///
/// The frame is intentionally exhaustive: it contains exactly one of every
/// [`Instruction`] variant, with field values chosen to round-trip every
/// integer width, every Option arm, and at least one non-empty `Vec<u8>`
/// payload. The fixture's stability under encoding is the contract.
///
/// Do not edit this function casually. Any change that alters the encoded
/// bytes is a wire-format break and requires:
///   1. A [`LOCKED_WIRE_VERSION`] bump.
///   2. Regenerating `tests/fixtures/wire/v1_canonical_frame.bin`.
///   3. A matching update to the bakabox decoder test suite.
///
/// # Panics
///
/// Never. The single internal `InstructionRange::try_new(0, 64)` call has
/// `start <= end` by construction and cannot fail; the `expect` is purely
/// to keep the surface infallible — wrapping the literal `0..64` constant
/// in a `Result` propagation would be noise without an information gain.
#[must_use]
pub fn canonical_v1_frame() -> OpcodeFrame {
    OpcodeFrame {
        frame_id: 1,
        component_id: Some(42),
        instructions: vec![
            Instruction::InitInternTable {
                table: InternTable {
                    kind: InternTableKind::Tag,
                    entries: vec![
                        InternEntry { id: 0, value: "div".to_string() },
                        InternEntry { id: 1, value: "span".to_string() },
                    ],
                },
            },
            Instruction::PatchInternTable {
                kind: InternTableKind::Attr,
                ops: vec![
                    InternPatchOp::Set { id: 0, value: "class".to_string() },
                    InternPatchOp::Remove { id: 9 },
                ],
            },
            Instruction::Create {
                tag_id: TagId(0),
                stable_id: StableId(1),
            },
            Instruction::SetAttr {
                stable_id: StableId(1),
                attr_id: AttrId(0),
                value: b"root".to_vec(),
            },
            Instruction::SetText {
                stable_id: StableId(1),
                text: b"hello bakabox".to_vec(),
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
                proxy_id: ProxyId(7),
            },
            Instruction::BindSlot {
                stable_id: StableId(1),
                slot_id: SlotId(3),
            },
            Instruction::Placeholder {
                stable_id: StableId(2),
                suspense_id: SuspenseId(10),
            },
            Instruction::Patch {
                suspense_id: SuspenseId(10),
                range: InstructionRange::try_new(0, 64)
                    .expect("0..64 is a valid InstructionRange"),
            },
            Instruction::SetTextRef {
                stable_id: StableId(1),
                slot_id: SlotId(11),
            },
            Instruction::SetAttrRef {
                stable_id: StableId(1),
                attr_id: AttrId(0),
                slot_id: SlotId(12),
            },
            Instruction::SlotSet {
                slot_id: SlotId(11),
                value: b"reactive-value".to_vec(),
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::wire::{decode_frame, encode_frame};

    #[test]
    fn canonical_v1_frame_round_trips() {
        let frame = canonical_v1_frame();
        let bytes = encode_frame(&frame).expect("canonical frame must encode");
        let (decoded, consumed) =
            decode_frame(&bytes).expect("canonical frame must decode");
        assert_eq!(consumed, bytes.len(), "decoder must consume every byte");
        assert_eq!(frame, decoded);
    }

    #[test]
    fn canonical_v1_frame_covers_every_instruction_variant() {
        // If a new variant is added to Instruction, this test fails fast
        // and reminds the author to extend the fixture. Count is hard-coded
        // so adding a variant without updating the fixture is a CI break.
        const EXPECTED_VARIANT_COUNT: usize = 14;
        let frame = canonical_v1_frame();
        assert_eq!(
            frame.instructions.len(),
            EXPECTED_VARIANT_COUNT,
            "canonical_v1_frame must contain exactly one of every Instruction variant; \
             update both the fixture and EXPECTED_VARIANT_COUNT when a variant is added \
             or removed, and bump LOCKED_WIRE_VERSION"
        );
    }
}
