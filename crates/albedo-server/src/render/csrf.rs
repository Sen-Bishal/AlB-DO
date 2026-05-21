//! Phase L · per-session CSRF tokens.
//!
//! Strategy: a per-session random token is minted on first read, held
//! in a `DashMap<SessionId, String>`, and embedded in every form the
//! server emits as `<input type="hidden" name="_csrf" value="...">`.
//! Action handlers (or the dispatcher, before they run) call
//! [`CsrfRegistry::validate`] against the `_csrf` field of the
//! decoded payload; a mismatch yields a [`CsrfError::Invalid`] which
//! the dispatcher turns into a 403.
//!
//! Token shape: 32 hex characters from a 128-bit random source. The
//! token is stored and compared as a plain string; no time-based
//! rotation in Stage 1 because the session id itself rotates per
//! browser session.
//!
//! Not a replacement for SameSite cookies or origin checks — this is
//! defence in depth, not the only line.

use dashmap::DashMap;
use dom_render_compiler::runtime::SessionId;
use std::sync::Arc;

/// Hidden form field name the server injects and the dispatcher
/// validates. Kept here so the renderer's form-render path and the
/// dispatch-time validator agree without a string-typed contract.
pub const CSRF_FIELD_NAME: &str = "_csrf";

/// Server-wide CSRF registry. Cloneable — internally an
/// `Arc<DashMap>` — so the renderer and the dispatcher can both hold
/// a handle. One instance per `AlbedoServer`.
#[derive(Clone, Default)]
pub struct CsrfRegistry {
    tokens: Arc<DashMap<SessionId, String>>,
}

/// Failure modes surfaced by [`CsrfRegistry::validate`]. Both map to
/// 403 at the HTTP boundary, but they're distinguished so logs can
/// tell "no session token at all" apart from "wrong token presented".
#[derive(Debug, thiserror::Error)]
pub enum CsrfError {
    #[error("CSRF token missing")]
    Missing,
    #[error("CSRF token mismatch")]
    Invalid,
}

impl CsrfRegistry {
    /// Fresh empty registry. Constructed once per server.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current CSRF token for `session`, minting one on
    /// first call. Subsequent reads return the same string for the
    /// lifetime of the session — useful for the renderer's
    /// form-injection pass, which may run multiple times per session
    /// across navigations.
    ///
    /// Race tolerance: if two threads call `token_for` concurrently
    /// for an unseen session, both mint a fresh token, but only the
    /// first `entry().or_insert` write lands in the map. The map only
    /// ever holds one token per session.
    pub fn token_for(&self, session: SessionId) -> String {
        if let Some(existing) = self.tokens.get(&session) {
            return existing.clone();
        }
        let fresh = mint_token();
        self.tokens.entry(session).or_insert(fresh).clone()
    }

    /// Compare a presented token against the stored one for
    /// `session`. Uses [`constant_time_eq`] to avoid leaking
    /// information about the stored token via early-exit timing.
    ///
    /// Returns `Err(CsrfError::Missing)` when no token has been
    /// minted yet for the session (or when the presented string is
    /// empty), `Err(CsrfError::Invalid)` when the strings differ, and
    /// `Ok(())` on match.
    pub fn validate(&self, session: SessionId, presented: &str) -> Result<(), CsrfError> {
        let stored = self.tokens.get(&session).ok_or(CsrfError::Missing)?;
        if presented.is_empty() {
            return Err(CsrfError::Missing);
        }
        if constant_time_eq(stored.as_bytes(), presented.as_bytes()) {
            Ok(())
        } else {
            Err(CsrfError::Invalid)
        }
    }

    /// Drop the token for a closed session so the table doesn't grow
    /// unbounded on long-running servers. Called from the WT
    /// session-close path.
    pub fn clear(&self, session: SessionId) {
        self.tokens.remove(&session);
    }

    /// Diagnostic: how many sessions currently hold a token. Used by
    /// the inspector and tests; not on the hot path.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// `true` when no sessions hold a token. Convenience for tests.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

/// Render-time helper: build the hidden CSRF input HTML to splice
/// into a form's children. The renderer calls this once per
/// `<form action="action:...">` it emits so every form ships with a
/// fresh per-session token.
///
/// Both attribute values are server-controlled (the field name is a
/// constant; the token is 32 hex chars from [`mint_token`]), so HTML
/// escaping is not required, but we still produce the canonical
/// quoted shape for stability across DOM parsers.
pub fn csrf_hidden_input_html(token: &str) -> String {
    format!("<input type=\"hidden\" name=\"{CSRF_FIELD_NAME}\" value=\"{token}\">")
}

/// Mints a 128-bit random token rendered as 32 lowercase hex chars.
///
/// Uses `uuid::Uuid::new_v4()` as the entropy source — `uuid` is
/// already a workspace dep, and its v4 generator pulls from the OS
/// CSPRNG on every supported platform, which is the property we need
/// here. The 128 bits are formatted with leading zeros so the output
/// length is always exactly 32 characters.
fn mint_token() -> String {
    let raw = uuid::Uuid::new_v4();
    format!("{:032x}", raw.as_u128())
}

/// Constant-time byte equality. Returns false fast on length mismatch
/// (the length is not itself a secret — both tokens are the
/// well-known 32-byte hex form).
///
/// Equal-length inputs are compared with a fixed-cost XOR fold so the
/// timing of a wrong-token check does not depend on the position of
/// the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_stable_per_session() {
        let reg = CsrfRegistry::new();
        let s = SessionId::random();
        let a = reg.token_for(s);
        let b = reg.token_for(s);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn distinct_sessions_get_distinct_tokens() {
        let reg = CsrfRegistry::new();
        let a = reg.token_for(SessionId::random());
        let b = reg.token_for(SessionId::random());
        assert_ne!(a, b);
    }

    #[test]
    fn validate_accepts_correct_token() {
        let reg = CsrfRegistry::new();
        let s = SessionId::random();
        let t = reg.token_for(s);
        assert!(reg.validate(s, &t).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_token() {
        let reg = CsrfRegistry::new();
        let s = SessionId::random();
        let _ = reg.token_for(s);
        let err = reg
            .validate(s, "00000000000000000000000000000000")
            .unwrap_err();
        assert!(matches!(err, CsrfError::Invalid));
    }

    #[test]
    fn validate_rejects_missing_session() {
        let reg = CsrfRegistry::new();
        let err = reg.validate(SessionId::random(), "anything").unwrap_err();
        assert!(matches!(err, CsrfError::Missing));
    }

    #[test]
    fn validate_rejects_empty_presented_token() {
        let reg = CsrfRegistry::new();
        let s = SessionId::random();
        let _ = reg.token_for(s);
        let err = reg.validate(s, "").unwrap_err();
        assert!(matches!(err, CsrfError::Missing));
    }

    #[test]
    fn clear_removes_the_session_entry() {
        let reg = CsrfRegistry::new();
        let s = SessionId::random();
        let _ = reg.token_for(s);
        assert_eq!(reg.len(), 1);
        reg.clear(s);
        assert!(reg.is_empty());
    }

    #[test]
    fn hidden_input_uses_canonical_field_name() {
        let html = csrf_hidden_input_html("abcd");
        assert!(html.contains("name=\"_csrf\""));
        assert!(html.contains("value=\"abcd\""));
        assert!(html.starts_with("<input type=\"hidden\""));
    }

    #[test]
    fn constant_time_eq_rejects_different_length() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn constant_time_eq_accepts_equal_inputs() {
        assert!(constant_time_eq(b"deadbeef", b"deadbeef"));
    }
}
