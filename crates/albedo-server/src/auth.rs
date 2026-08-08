//! AUTH · P0 — the request-time half: a cookie becomes a principal.
//!
//! The compiler crate owns the vocabulary, the schema and the store
//! ([`dom_render_compiler::auth`]); this owns the one thing that needs an HTTP
//! request — reading the cookie, and handing the result to the render, action
//! and subscribe paths.
//!
//! ## Invariant 2.2 is a *shape*, not a check
//!
//! > A principal is request-scoped and never cached across requests.
//!
//! `AUTH.md` calls this the one failure here that is a CVE rather than a bug:
//! the response cache, the topic value cache and the render path must not be
//! able to serve one principal's bytes to another. There is no assertion that
//! enforces it, because an assertion would be checking a property the design
//! should make unreachable. So:
//!
//! - [`AuthRuntime`] holds **no map from token to principal**. Resolution is a database read every
//!   time. That is one indexed lookup, and it is what makes revocation instant — a cache with any
//!   TTL at all would be a window in which a revoked session still works.
//! - [`Identity`] is not `Clone`-into-anything-long-lived by accident: it is produced per request
//!   and passed down, never stored.
//!
//! The audit that matters is therefore about the *other* caches — anything that
//! keys rendered bytes by URL alone becomes a cross-principal leak the moment a
//! route reads `user`. P1 is what makes such a route exist, so the guard lands
//! with it rather than before it: see [`Identity::varies_response`].

use axum::http::HeaderMap;
use dom_render_compiler::auth::principal::Principal;
use dom_render_compiler::auth::session::{SessionToken, TokenHash};
use dom_render_compiler::auth::store::{self, Resolved};
use dom_render_compiler::auth::AuthRegistry;
use dom_render_compiler::forge::DataSubstrate;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Who is making this request, resolved once and passed down.
///
/// `Anonymous` is an ordinary outcome, not a failure: most requests to most apps
/// have no session, and an app that declared no providers has none at all.
#[derive(Debug, Clone, Default)]
pub enum Identity {
    /// No session, or one that did not resolve. Why it did not is deliberately
    /// not carried here — it belongs in our logs, not in anything a handler
    /// might be tempted to branch on and leak.
    #[default]
    Anonymous,
    /// A live session.
    Authenticated {
        /// The principal.
        principal: Box<Principal>,
        /// The presented token's hash, kept so rotation and single-device logout
        /// can name *this* session without re-reading the cookie.
        token: TokenHash,
    },
}

impl Identity {
    /// The principal, if any.
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            Self::Anonymous => None,
            Self::Authenticated { principal, .. } => Some(principal),
        }
    }

    /// This session's token hash, for rotation and logout.
    #[must_use]
    pub fn token(&self) -> Option<&TokenHash> {
        match self {
            Self::Anonymous => None,
            Self::Authenticated { token, .. } => Some(token),
        }
    }

    /// Whether a response produced under this identity may be shared with
    /// another one.
    ///
    /// **The cache seam for invariant 2.2.** Today every route is public, so
    /// this is `false` for `Anonymous` and `true` for anyone authenticated —
    /// conservative, and correct in the only direction that matters: it can
    /// cost a cache hit, never a leak. P1 replaces the second arm with the real
    /// question (*did this route read `user`?*), which the compiler already
    /// knows from `shared_slot_partitions_for_entry`.
    #[must_use]
    pub fn varies_response(&self) -> bool {
        matches!(self, Self::Authenticated { .. })
    }
}

/// The live auth path: a declaration, plus the substrate it resolves against.
///
/// Built at boot and held on `LiveRuntime` beside the FORGE substrate and the
/// APERTURE reader, because all three answer the same question — *what does
/// this request get to see* — and holding them together is what keeps one
/// request from resolving its identity through a different world than its data.
pub struct AuthRuntime {
    registry: AuthRegistry,
    substrate: Arc<dyn DataSubstrate>,
}

impl AuthRuntime {
    /// Assemble from a lowered declaration and the app's substrate.
    #[must_use]
    pub fn new(registry: AuthRegistry, substrate: Arc<dyn DataSubstrate>) -> Self {
        Self {
            registry,
            substrate,
        }
    }

    /// The lowered `auth` block.
    #[must_use]
    pub fn registry(&self) -> &AuthRegistry {
        &self.registry
    }

    /// The substrate, for the login flows that write to it.
    #[must_use]
    pub fn substrate(&self) -> &Arc<dyn DataSubstrate> {
        &self.substrate
    }

    /// Resolve a request's identity.
    ///
    /// Never returns an error. A substrate failure resolves to
    /// [`Identity::Anonymous`] — **fail closed**: a database that cannot answer
    /// "is this session live?" has not said yes, and treating an outage as
    /// "everyone is who they claim" is the inversion that turns a bad afternoon
    /// into an incident. The failure is logged; the request proceeds as a
    /// stranger.
    pub async fn resolve(&self, headers: &HeaderMap) -> Identity {
        if self.registry.is_empty() {
            // No providers declared: nobody can be authenticated, so there is
            // nothing to look up and no query to spend.
            return Identity::Anonymous;
        }

        let Some(token) = self.presented_token(headers) else {
            return Identity::Anonymous;
        };
        let hash = token.hash();

        match store::resolve_session(self.substrate.as_ref(), &token, now_ms()).await {
            Ok(Resolved::Principal(principal)) => Identity::Authenticated {
                principal,
                token: hash,
            },
            Ok(Resolved::Anonymous(reason)) => {
                tracing::debug!(target: "albedo.auth", %reason, "request is anonymous");
                Identity::Anonymous
            }
            Err(err) => {
                tracing::error!(
                    target: "albedo.auth",
                    %err,
                    "session lookup failed; treating the request as anonymous"
                );
                Identity::Anonymous
            }
        }
    }

    /// Pull the session token out of a request's `Cookie` header.
    ///
    /// Cookie only — no header fallback, unlike the tab session's
    /// `x-albedo-session`. A bearer credential readable from a header the caller
    /// controls is a credential that rides cross-origin requests without
    /// `SameSite` having any say, which is the protection the cookie form is
    /// chosen for. API clients get a token through a login endpoint, not by
    /// borrowing this one.
    fn presented_token(&self, headers: &HeaderMap) -> Option<SessionToken> {
        let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
        let value = dom_render_compiler::auth::read_cookie(raw, &self.registry.session_cookie)?;
        SessionToken::from_presented(value)
    }

    /// The `Set-Cookie` value that opens a session.
    #[must_use]
    pub fn set_cookie(&self, token: &SessionToken) -> String {
        dom_render_compiler::auth::set_cookie_value(
            &self.registry.session_cookie,
            token,
            self.registry.session_ttl.as_secs(),
        )
    }

    /// The `Set-Cookie` value that ends one.
    #[must_use]
    pub fn clear_cookie(&self) -> String {
        dom_render_compiler::auth::clear_cookie_value(&self.registry.session_cookie)
    }

    /// Session lifetime in milliseconds, for the store's `expires_at`.
    #[must_use]
    pub fn ttl_ms(&self) -> i64 {
        i64::try_from(self.registry.session_ttl.as_millis()).unwrap_or(i64::MAX)
    }
}

/// Wall-clock milliseconds since the epoch — the unit
/// [`FieldType::Timestamp`](dom_render_compiler::forge::declare::FieldType::Timestamp)
/// stores and JavaScript's `Date` speaks.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::auth::principal::PrincipalId;
    use dom_render_compiler::auth::AuthDeclaration;
    use dom_render_compiler::forge::mem::RecordingSubstrate;

    fn runtime(declaration: serde_json::Value) -> AuthRuntime {
        let declaration: AuthDeclaration =
            serde_json::from_value(declaration).expect("declaration parses");
        AuthRuntime::new(
            declaration.lower().expect("lowers"),
            Arc::new(RecordingSubstrate::new()),
        )
    }

    fn with_cookie(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            value.parse().expect("cookie header"),
        );
        headers
    }

    #[tokio::test]
    async fn an_app_with_no_providers_resolves_everyone_as_anonymous() {
        let runtime = runtime(serde_json::json!({}));
        let headers = with_cookie("__Host-albedo_session=anything");
        assert!(runtime.resolve(&headers).await.principal().is_none());
    }

    #[tokio::test]
    async fn a_request_with_no_cookie_is_anonymous() {
        let runtime = runtime(serde_json::json!({ "providers": { "passkey": {} } }));
        assert!(runtime
            .resolve(&HeaderMap::new())
            .await
            .principal()
            .is_none());
    }

    /// **Fail closed.** `RecordingSubstrate` answers every query with an empty
    /// result, which stands in for "the database cannot confirm this session".
    /// The request must proceed as a stranger, never as the claimed principal.
    #[tokio::test]
    async fn an_unanswerable_lookup_resolves_to_anonymous() {
        let runtime = runtime(serde_json::json!({ "providers": { "passkey": {} } }));
        let headers = with_cookie("__Host-albedo_session=some-token");
        assert!(runtime.resolve(&headers).await.principal().is_none());
    }

    /// The tab cookie is not a credential and must never be read as one — it is
    /// minted for anyone who visits, with no login involved.
    ///
    /// Both cookies now carry the `__Host-` prefix and differ by a single
    /// character (`albedo-session` against `albedo_session`), so this reads the
    /// name from the constant rather than a literal: a rename that collided
    /// would otherwise leave the test passing against a name nobody sets.
    #[tokio::test]
    async fn the_tab_session_cookie_is_not_accepted_as_an_auth_cookie() {
        let runtime = runtime(serde_json::json!({ "providers": { "passkey": {} } }));
        let headers = with_cookie(&format!(
            "{}=9f1c8a70-0000-4000-8000-000000000000",
            crate::render::ALBEDO_SESSION_COOKIE
        ));
        assert!(runtime.presented_token(&headers).is_none());
    }

    /// A bearer credential readable from a caller-controlled header rides
    /// cross-origin requests with `SameSite` having no say.
    #[tokio::test]
    async fn a_session_token_is_not_accepted_from_a_header() {
        let runtime = runtime(serde_json::json!({ "providers": { "passkey": {} } }));
        let mut headers = HeaderMap::new();
        headers.insert("x-albedo-session", "some-token".parse().unwrap());
        headers.insert("authorization", "Bearer some-token".parse().unwrap());
        assert!(runtime.presented_token(&headers).is_none());
    }

    #[test]
    fn a_declared_cookie_name_is_the_one_read_and_written() {
        let runtime = runtime(serde_json::json!({
            "session": { "cookie": "my_app_session" },
            "providers": { "passkey": {} }
        }));
        let headers = with_cookie("my_app_session=tok");
        assert!(runtime.presented_token(&headers).is_some());
        assert!(runtime.clear_cookie().starts_with("my_app_session="));
    }

    #[test]
    fn the_cookie_max_age_matches_the_declared_ttl() {
        let runtime = runtime(serde_json::json!({
            "session": { "ttl": "12h" },
            "providers": { "passkey": {} }
        }));
        let cookie = runtime.set_cookie(&SessionToken::mint());
        assert!(cookie.contains("Max-Age=43200"), "{cookie}");
        assert_eq!(runtime.ttl_ms(), 12 * 60 * 60 * 1_000);
    }

    /// Conservative in the only direction that matters: costing a cache hit is
    /// survivable, serving one principal's bytes to another is not.
    #[test]
    fn an_authenticated_response_is_never_marked_shareable() {
        assert!(!Identity::Anonymous.varies_response());
        assert!(Identity::Authenticated {
            principal: Box::new(Principal::new(PrincipalId::mint(), "passkey")),
            token: TokenHash::of("t"),
        }
        .varies_response());
    }

    #[test]
    fn now_is_in_milliseconds_not_seconds() {
        // Sanity: milliseconds since 1970 passed 1.7e12 in 2023 and will not
        // reach 1e13 until 2286. Seconds would be ~1.7e9.
        let now = now_ms();
        assert!(now > 1_700_000_000_000, "{now} looks like seconds");
        assert!(now < 10_000_000_000_000, "{now} is implausible");
    }
}
