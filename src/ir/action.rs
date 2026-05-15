//! Phase-G — client → server action wire envelope.
//!
//! Carries a single event from bakabox to the server in bincode-encoded
//! form, sharing the locked codec configuration from [`crate::ir::wire`].
//! No change to [`crate::ir::opcode::Instruction`] — `BindEvent.proxy_id`
//! is reinterpreted as the `action_id` field below at the runtime level.
//!
//! Wire layout (all varint-encoded per the locked config):
//!
//! ```text
//! [action_id u32] [event_kind u8] [payload_len u64] [payload raw bytes]
//! ```
//!
//! `payload` is opaque to the wire layer. Bakabox populates it based on
//! the originating DOM event (see [`ActionEventKind`]); userland action
//! handlers parse it according to the declared `event_kind`.

use crate::ir::wire::WireError;
use bincode::{Decode, Encode};

/// One client → server action invocation. Serialized as the HTTP POST
/// body for `/_albedo/action` and (in a future revision) over a
/// dedicated WT slot.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct ActionEnvelope {
    /// Server-side action identifier. Matches the `proxy_id` field of
    /// the `BindEvent` opcode that originally wired this listener on
    /// bakabox.
    pub action_id: u32,
    /// Event-kind hint. See [`ActionEventKind`] for the discriminant
    /// the bakabox dispatcher emits.
    pub event_kind: u8,
    /// Event-kind-specific payload bytes. Convention:
    ///
    /// - `Click` / `Other` — empty
    /// - `Input` — UTF-8 bytes of `event.target.value`
    /// - `Submit` — Phase-I form encoding (TBD)
    pub payload: Vec<u8>,
}

/// Symbolic names for the `event_kind` byte. Order is the wire
/// contract; reordering breaks the bakabox dispatcher mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ActionEventKind {
    Click = 0,
    Input = 1,
    Submit = 2,
    Other = 3,
}

impl ActionEventKind {
    /// Maps the wire byte back to the symbolic name. Returns `Other`
    /// for unknown values rather than failing — userland handlers
    /// inspect `event_kind` directly when they need strictness.
    #[must_use]
    pub fn from_wire(byte: u8) -> Self {
        match byte {
            0 => Self::Click,
            1 => Self::Input,
            2 => Self::Submit,
            _ => Self::Other,
        }
    }
}

/// Encodes an envelope into bincode bytes ready for the HTTP body.
///
/// # Errors
///
/// Returns [`WireError::Encode`] if bincode serialization fails. This
/// should not happen for valid `ActionEnvelope` values — the locked
/// config has no size limit and all fields are bincode-derivable.
pub fn encode_action_envelope(envelope: &ActionEnvelope) -> Result<Vec<u8>, WireError> {
    use crate::ir::wire::WireEncode;
    envelope.wire_encode()
}

/// Decodes an envelope from bincode bytes, returning the envelope and
/// the number of bytes consumed.
///
/// # Errors
///
/// Returns [`WireError::Decode`] if the bytes are not a valid
/// `ActionEnvelope` under the locked bincode config.
pub fn decode_action_envelope(bytes: &[u8]) -> Result<(ActionEnvelope, usize), WireError> {
    use crate::ir::wire::WireDecode;
    ActionEnvelope::wire_decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_every_field() {
        let envelope = ActionEnvelope {
            action_id: 12345,
            event_kind: ActionEventKind::Input as u8,
            payload: b"hello action".to_vec(),
        };
        let bytes = encode_action_envelope(&envelope).unwrap();
        let (decoded, consumed) = decode_action_envelope(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn small_envelope_encodes_compactly() {
        let envelope = ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: Vec::new(),
        };
        let bytes = encode_action_envelope(&envelope).unwrap();
        // varint(1) + u8(0) + varint(0 len) = 3 bytes
        assert_eq!(bytes, vec![1, 0, 0]);
    }

    #[test]
    fn event_kind_from_wire_maps_unknowns_to_other() {
        assert_eq!(ActionEventKind::from_wire(0), ActionEventKind::Click);
        assert_eq!(ActionEventKind::from_wire(1), ActionEventKind::Input);
        assert_eq!(ActionEventKind::from_wire(2), ActionEventKind::Submit);
        assert_eq!(ActionEventKind::from_wire(3), ActionEventKind::Other);
        assert_eq!(ActionEventKind::from_wire(99), ActionEventKind::Other);
    }
}
