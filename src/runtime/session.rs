//! Phase-H — session identifier for the per-session slot store.
//!
//! Bakabox sessions are already identified by a UUID at the WT layer
//! ([`crate::runtime::webtransport`] uses `uuid::Uuid` directly). This
//! newtype wraps that so the slot store's `(SessionId, SlotId)` key
//! cannot accidentally collide with any other `Uuid`-keyed map in the
//! codebase, and so the type tells reviewers what the value represents.

use std::fmt;
use uuid::Uuid;

/// Identifier for a single bakabox session. Equal to the `uuid::Uuid`
/// the WT layer assigns when a session opens; the newtype enforces
/// session-specific intent at the type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Wraps a raw `Uuid` as a `SessionId`.
    #[must_use]
    pub const fn new(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Returns a freshly generated session id. Used by tests and any
    /// future server-side path that needs to mint a session without a
    /// transport handshake.
    #[must_use]
    pub fn random() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the inner `Uuid` for interop with code that pre-dates
    /// the newtype (the WT registry, log lines, etc.).
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl From<Uuid> for SessionId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl From<SessionId> for Uuid {
    fn from(session: SessionId) -> Self {
        session.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newtype_round_trips_through_uuid() {
        let raw = Uuid::new_v4();
        let session = SessionId::from(raw);
        assert_eq!(session.as_uuid(), raw);
        assert_eq!(Uuid::from(session), raw);
    }

    #[test]
    fn equal_uuids_compare_equal_as_session_ids() {
        let raw = Uuid::new_v4();
        assert_eq!(SessionId::new(raw), SessionId::new(raw));
    }
}
