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

use axum::http::HeaderMap;
use dashmap::DashMap;
use dom_render_compiler::runtime::SessionId;
use std::sync::Arc;

/// Hidden form field name the renderers inject and the dispatcher
/// validates.
///
/// Re-exported from `transforms::form`, which owns the whole
/// form-action markup contract — the field name here and the `name=`
/// the renderers actually emit are now the same constant rather than
/// two literals that happen to match today.
pub use dom_render_compiler::transforms::form::CSRF_FIELD_NAME;

/// Cookie name that carries the per-session id between requests.
/// The streaming handler mints one on first visit (Set-Cookie); the
/// browser auto-attaches it to every subsequent request including
/// form action POSTs. Both the page-render path and the action
/// dispatch path read from this cookie so the CSRF token table
/// stays addressable by the same `SessionId` on both sides.
pub const ALBEDO_SESSION_COOKIE: &str = "albedo-session";

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

/// Phase L · post-render CSRF substitution.
///
/// Fills the per-session `token` into every hidden CSRF placeholder the
/// renderers emitted, so the input is ready when the client serializes
/// the form on submit.
///
/// Thin delegate to `transforms::form::fill_csrf_tokens`, which owns
/// both halves of this contract: the placeholder the renderers emit and
/// the anchor this fill matches on. They previously lived on opposite
/// sides of a crate boundary as two independently-maintained literals
/// — the arrangement that let a renderer emit markup the fill couldn't
/// see. This name is kept because it reads better at the call sites.
///
/// Returns the input unchanged when no placeholder is present (any page
/// without a form).
pub fn substitute_csrf_token_in_html(html: &str, token: &str) -> String {
    dom_render_compiler::transforms::form::fill_csrf_tokens(html, token)
}

/// Reads the `albedo-session` cookie value from a request's header
/// map and parses it into a [`SessionId`].
///
/// Returns `None` when the cookie is absent, malformed, or doesn't
/// parse as a UUID. Callers that need a session id even on first
/// visit should pair this with [`SessionId::random`] for a
/// fresh-mint fallback (and remember to set the matching
/// `Set-Cookie` on the response — see [`build_session_set_cookie`]).
///
/// Multiple cookies in a single header are supported (the
/// `Cookie` header semicolon-separates them). The first matching
/// `albedo-session=...` value wins; later duplicates are ignored.
pub fn read_session_cookie(headers: &HeaderMap) -> Option<SessionId> {
    let cookie_header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for entry in cookie_header.split(';') {
        let trimmed = entry.trim();
        if let Some(value) = trimmed.strip_prefix(&format!("{ALBEDO_SESSION_COOKIE}=")) {
            if let Ok(uuid) = uuid::Uuid::parse_str(value) {
                return Some(SessionId::new(uuid));
            }
        }
    }
    None
}

/// Build a `Set-Cookie` header value that pins `albedo-session` to
/// the supplied [`SessionId`]. Attributes:
///   * `Path=/` — every route on the origin sees the cookie.
///   * `HttpOnly` — JavaScript can't read it, so token leakage via
///     XSS doesn't directly compromise the CSRF surface.
///   * `SameSite=Lax` — sent on top-level same-site navigations but
///     not on cross-site POSTs, which is exactly the threat CSRF
///     tokens guard against.
/// The cookie is session-scoped (no `Max-Age` / `Expires`) so it
/// rolls when the browser closes — matches the lifetime semantics
/// of the in-memory `CsrfRegistry`.
pub fn build_session_set_cookie(session: SessionId) -> String {
    format!(
        "{ALBEDO_SESSION_COOKIE}={uuid}; Path=/; HttpOnly; SameSite=Lax",
        uuid = session.as_uuid()
    )
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

    #[test]
    fn substitute_csrf_token_fills_empty_value_placeholder() {
        let rendered = r#"<form><input type="hidden" name="_csrf" value="" data-albedo-csrf /></form>"#;
        let out = substitute_csrf_token_in_html(rendered, "abc123");
        assert!(out.contains("value=\"abc123\" data-albedo-csrf"));
        assert!(!out.contains("value=\"\" data-albedo-csrf"));
    }

    #[test]
    fn substitute_csrf_token_is_a_noop_when_no_marker_present() {
        let plain = "<div>nothing to do here</div>";
        assert_eq!(substitute_csrf_token_in_html(plain, "abc123"), plain);
    }

    #[test]
    fn substitute_csrf_token_handles_multiple_forms_in_one_page() {
        let rendered = concat!(
            "<form><input value=\"\" data-albedo-csrf /></form>",
            "<form><input value=\"\" data-albedo-csrf /></form>",
        );
        let out = substitute_csrf_token_in_html(rendered, "deadbeef");
        assert_eq!(out.matches("value=\"deadbeef\"").count(), 2);
        assert!(!out.contains("value=\"\" data-albedo-csrf"));
    }

    #[test]
    fn read_session_cookie_finds_value_in_single_cookie_header() {
        let mut headers = HeaderMap::new();
        let session = SessionId::random();
        let cookie = format!("{ALBEDO_SESSION_COOKIE}={}", session.as_uuid());
        headers.insert(
            axum::http::header::COOKIE,
            cookie.parse().expect("cookie header value"),
        );
        let parsed = read_session_cookie(&headers).expect("cookie parses");
        assert_eq!(parsed, session);
    }

    #[test]
    fn read_session_cookie_finds_value_among_multiple_cookies() {
        let mut headers = HeaderMap::new();
        let session = SessionId::random();
        let cookie = format!(
            "theme=dark; {ALBEDO_SESSION_COOKIE}={}; preferences=compact",
            session.as_uuid()
        );
        headers.insert(
            axum::http::header::COOKIE,
            cookie.parse().expect("cookie header value"),
        );
        let parsed = read_session_cookie(&headers).expect("cookie parses");
        assert_eq!(parsed, session);
    }

    #[test]
    fn read_session_cookie_returns_none_when_absent() {
        let headers = HeaderMap::new();
        assert!(read_session_cookie(&headers).is_none());
    }

    #[test]
    fn read_session_cookie_returns_none_for_malformed_uuid() {
        let mut headers = HeaderMap::new();
        let cookie = format!("{ALBEDO_SESSION_COOKIE}=not-a-uuid");
        headers.insert(
            axum::http::header::COOKIE,
            cookie.parse().expect("cookie header value"),
        );
        assert!(read_session_cookie(&headers).is_none());
    }

    #[test]
    fn build_session_set_cookie_carries_expected_attributes() {
        let session = SessionId::random();
        let header = build_session_set_cookie(session);
        assert!(header.contains(&session.as_uuid().to_string()));
        assert!(header.contains("Path=/"));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Lax"));
        // No Max-Age / Expires — session cookie rolls when the
        // browser closes, matching the in-memory registry's lifetime.
        assert!(!header.contains("Max-Age"));
        assert!(!header.contains("Expires"));
    }
}
