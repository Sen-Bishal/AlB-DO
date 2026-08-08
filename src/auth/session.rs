//! AUTH · P0 — the session token, and the row it resolves through.
//!
//! ## Two things called "session", and they are not the same thing
//!
//! This module is the one place where that distinction has to be stated,
//! because both already exist in the tree and confusing them is a security bug
//! rather than a naming annoyance:
//!
//! | | [`SessionId`](crate::runtime::session::SessionId) | [`SessionToken`] (here) |
//! |---|---|---|
//! | identifies | **a tab** | **a human** |
//! | shape | `Uuid`, sent as-is | 256 random bits, stored only as a hash |
//! | cookie | `__Host-albedo-session` | `__Host-albedo_session` |
//! | used for | CSRF pairing, the per-session slot store, the PHOSPHOR lane | resolving a [`Principal`] |
//! | lifetime | a browser session | the declared `auth.session.ttl` |
//!
//! One human has one auth session and as many tab sessions as they have tabs
//! open. The existing `SessionId` is **not** an authentication credential and
//! must never be promoted into one — it is minted on first visit to anyone who
//! asks, with no login involved. `RouteAuthority::authorize_route` taking an
//! `Option<SessionId>` is therefore *the tab*, not the principal, which is
//! exactly the gap `AUTH.md` R2 names.
//!
//! ## Why the stored value is a hash
//!
//! Invariant 2.6. The bearer token exists in the cookie and nowhere else on our
//! side; the database holds `SHA-256(token)`. A backup, a replica, a log line,
//! or a `SELECT *` in a support session yields nothing that can be replayed as a
//! login. This is the same reason a password is not stored — the difference is
//! only in which hash is appropriate, and that difference is real:
//!
//! 🔑 **A session token gets a fast hash, and a password gets a slow one.** A
//! password is low-entropy, so the defence against a stolen database is making
//! each guess expensive. A session token is 256 bits of CSPRNG output, so there
//! is no dictionary to slow down — and because it is verified on *every
//! authenticated request*, a deliberately slow hash there is a DoS vector we
//! would be building ourselves. Reaching for Argon2 here would look more careful
//! and be strictly worse.

use crate::auth::principal::PrincipalId;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fmt;

/// Bytes of entropy in a session token.
///
/// 32 bytes = 256 bits. Well past the ~128 bits that would already make
/// guessing infeasible, and it costs nothing: the token is generated once per
/// login and compared by an index lookup.
pub const TOKEN_BYTES: usize = 32;

/// A session bearer token — the value that lives in the cookie.
///
/// Deliberately **not** `Clone`, `Serialize`, or `Debug`-transparent. Every one
/// of those would make it one careless line away from a log file, a JSON
/// response, or a journal entry, and the whole point of the type is that the
/// value has exactly two legitimate destinations: a `Set-Cookie` header, and
/// [`Self::hash`].
pub struct SessionToken(String);

impl SessionToken {
    /// Mint a fresh token from the OS CSPRNG.
    #[must_use]
    pub fn mint() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        // `rand::thread_rng` is a CSPRNG seeded from the OS entropy source.
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Adopt a token presented by a client.
    ///
    /// No validation beyond non-emptiness, on purpose: a presented token is
    /// untrusted input whose only use is to be hashed and looked up. Rejecting
    /// "malformed" tokens early would add a way to distinguish *wrong shape*
    /// from *wrong value* without adding any safety, since a wrong value fails
    /// the lookup anyway.
    #[must_use]
    pub fn from_presented(raw: &str) -> Option<Self> {
        if raw.is_empty() || raw.len() > 512 {
            return None;
        }
        Some(Self(raw.to_string()))
    }

    /// The value to put in a `Set-Cookie` header. **The only accessor that
    /// yields the secret**, named so it is obvious in review.
    #[must_use]
    pub fn expose_for_cookie(&self) -> &str {
        &self.0
    }

    /// The value to store and look up by.
    #[must_use]
    pub fn hash(&self) -> TokenHash {
        TokenHash::of(&self.0)
    }
}

/// Redacted. A token must not be able to reach a log through a derived `Debug`
/// on some struct that happens to contain one.
impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

/// `SHA-256(token)`, lowercase hex — the value in `albedo_sessions.token_hash`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenHash(String);

impl TokenHash {
    /// Hash a raw token value.
    #[must_use]
    pub fn of(raw: &str) -> Self {
        let digest = Sha256::digest(raw.as_bytes());
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    /// Adopt a hash read back from the database.
    #[must_use]
    pub fn from_stored(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// As it appears in a SQL parameter.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One row of `albedo_sessions`, as the resolver reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// The row's own key.
    pub id: i64,
    /// Who this session belongs to.
    pub principal: PrincipalId,
    /// Which declared provider authenticated it.
    pub provider: String,
    /// Epoch milliseconds — matches
    /// [`FieldType::Timestamp`](crate::forge::declare::FieldType::Timestamp).
    pub created_at: i64,
    /// Epoch milliseconds. Past this, the session resolves to nobody.
    pub expires_at: i64,
    /// The hash of the session this one replaced, when it came from a rotation.
    ///
    /// Kept rather than discarded because it is the only way to *notice* a
    /// stolen cookie: a request presenting a token that was already rotated away
    /// is either a race with the legitimate client or a replay of a captured
    /// value, and a system that overwrites the row cannot tell those apart
    /// afterwards.
    pub rotated_from: Option<String>,
}

impl SessionRecord {
    /// Whether this session is still live at `now` (epoch milliseconds).
    ///
    /// Expiry is checked **here and in the query**, deliberately. The query's
    /// `expires_at > ?` is what keeps a stale row from being fetched at all; this
    /// is what keeps a caller that fetched a row by some other path — a test, a
    /// device list, a future admin screen — from treating it as live.
    #[must_use]
    pub fn is_live_at(&self, now: i64) -> bool {
        self.expires_at > now
    }
}

/// Why a presented cookie did not become a principal.
///
/// The variants exist for *our* logs, not for the client: every one of them is
/// answered with the same anonymous request, because telling a caller whether a
/// token was unknown, expired, or belonged to a deleted user is telling them
/// something about a token they were not supposed to have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRejection {
    /// No cookie, or an empty one.
    NoCookie,
    /// The token hashed to nothing in the table.
    UnknownToken,
    /// The row exists but `expires_at` has passed.
    Expired,
    /// The session's principal has no `albedo_users` row — the account was
    /// deleted while a session was open.
    PrincipalGone,
}

impl fmt::Display for SessionRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCookie => f.write_str("no session cookie presented"),
            Self::UnknownToken => f.write_str("session token does not match any live session"),
            Self::Expired => f.write_str("session has expired"),
            Self::PrincipalGone => f.write_str("session's principal no longer exists"),
        }
    }
}

/// Build the `Set-Cookie` value that carries a session token.
///
/// Every attribute here is load-bearing:
///
/// - **`HttpOnly`** — script cannot read it, so an XSS does not directly yield the credential.
/// - **`Secure`** — required by the `__Host-` prefix, and correct regardless.
/// - **`SameSite=Lax`** — the cookie rides top-level navigations (so arriving back from an OAuth
///   redirect works) but not cross-site subrequests.
/// - **`Path=/`** and **no `Domain`** — required by the `__Host-` prefix, which is what stops a
///   sibling subdomain from setting a cookie the parent will honour. That is the session-fixation
///   vector a plain cookie name leaves open, and the prefix is enforced by the browser rather than
///   by us.
/// - **`Max-Age`** — matched to the row's TTL so the browser stops presenting a token the server
///   would refuse anyway.
#[must_use]
pub fn set_cookie_value(name: &str, token: &SessionToken, max_age_secs: u64) -> String {
    format!(
        "{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_secs}",
        value = token.expose_for_cookie(),
    )
}

/// Build the `Set-Cookie` value that clears a session cookie.
///
/// Attributes must match the ones it was set with or the browser keeps the
/// original — a logout that appears to work and does not is worse than one that
/// fails loudly.
#[must_use]
pub fn clear_cookie_value(name: &str) -> String {
    format!("{name}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// Read a named cookie out of a `Cookie` header value.
///
/// The first match wins, which is what browsers do, and a value that later
/// turns out to be unusable does **not** cause a fall-through to the next entry
/// of the same name. That distinction matters here and nowhere else: this
/// function reads a credential, and a credential reader that keeps looking
/// after a rejection is one that can be walked through a list of guesses. The
/// tab session's reader takes the opposite side deliberately — see
/// `read_session_cookie` in the server crate, which documents why continuity
/// wins there.
///
/// Returning `None` means *this* cookie is absent, and that is the only claim
/// this function is entitled to make.
#[must_use]
pub fn read_cookie<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    cookie_entries(cookie_header)
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

/// Split a `Cookie` header value into its `(name, value)` pairs, in the order
/// the client sent them.
///
/// **This is the only cookie tokenizer in the tree.** It exists because there
/// were three, and they had drifted into three different answers for the same
/// header: one skipped malformed entries, one aborted at them, and one resolved
/// duplicate names backwards from the other two. None of that was a decision
/// anyone made — it was three loops written on three days. Splitting a header
/// is not where a codebase should be expressing opinions, so the opinions now
/// live at the call sites, one line each, and the scanning lives here and is
/// tested once.
///
/// Standalone and header-type-agnostic so the compiler crate can own — and
/// test — the parsing, while the server crate supplies the header.
///
/// An entry carrying no `=` is skipped rather than ending the scan. Such an
/// entry is a *neighbour's* malformation, and aborting on it bills the wrong
/// party: a header like `consent; __Host-albedo_session=…` — which a real
/// browser will send, because a valueless cookie is legal — would otherwise
/// yield no token at all, and the user would be silently signed out with
/// nothing anywhere saying why.
///
/// The `?` below is load-bearing in the *opposite* direction to the one that
/// caused that bug: inside a `filter_map` closure it abandons one entry, where
/// in a `for` loop it abandoned the whole header. Same operator, same
/// expression, and the difference between them was a silent sign-out. Keeping
/// this an iterator adaptor is what stops that failure mode returning.
pub fn cookie_entries(cookie_header: &str) -> impl Iterator<Item = (&str, &str)> + '_ {
    cookie_header.split(';').filter_map(|entry| {
        let (key, value) = entry.trim().split_once('=')?;
        Some((key.trim(), value.trim()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_carries_the_declared_entropy() {
        let token = SessionToken::mint();
        let decoded = URL_SAFE_NO_PAD
            .decode(token.expose_for_cookie())
            .expect("base64url round trips");
        assert_eq!(decoded.len(), TOKEN_BYTES);
    }

    #[test]
    fn minted_tokens_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1024 {
            assert!(
                seen.insert(SessionToken::mint().expose_for_cookie().to_string()),
                "the CSPRNG repeated itself"
            );
        }
    }

    /// A token must not be able to reach a log through a derived `Debug` on
    /// something that contains one.
    #[test]
    fn debug_does_not_print_the_token() {
        let token = SessionToken::mint();
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(token.expose_for_cookie()));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn hashing_is_deterministic_and_not_the_token() {
        let token = SessionToken::mint();
        assert_eq!(token.hash(), token.hash());
        assert_ne!(token.hash().as_str(), token.expose_for_cookie());
        // SHA-256 as lowercase hex.
        assert_eq!(token.hash().as_str().len(), 64);
        assert!(token
            .hash()
            .as_str()
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn hashing_matches_a_known_sha256_vector() {
        // SHA-256("abc") — pins the algorithm, so swapping it is a test failure
        // rather than a silent invalidation of every stored session.
        assert_eq!(
            TokenHash::of("abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn distinct_tokens_hash_distinctly() {
        assert_ne!(SessionToken::mint().hash(), SessionToken::mint().hash());
    }

    #[test]
    fn an_empty_or_oversized_presented_token_is_refused() {
        assert!(SessionToken::from_presented("").is_none());
        assert!(SessionToken::from_presented(&"a".repeat(513)).is_none());
        assert!(SessionToken::from_presented("plausible-token").is_some());
    }

    #[test]
    fn a_session_is_live_until_its_expiry() {
        let record = SessionRecord {
            id: 1,
            principal: PrincipalId::mint(),
            provider: "passkey".to_string(),
            created_at: 1_000,
            expires_at: 2_000,
            rotated_from: None,
        };
        assert!(record.is_live_at(1_999));
        assert!(!record.is_live_at(2_000), "expiry is exclusive");
        assert!(!record.is_live_at(2_001));
    }

    #[test]
    fn the_set_cookie_carries_every_attribute_that_matters() {
        let token = SessionToken::mint();
        let header = set_cookie_value("__Host-albedo_session", &token, 2_592_000);
        assert!(header.contains(token.expose_for_cookie()));
        for attribute in [
            "Path=/",
            "HttpOnly",
            "Secure",
            "SameSite=Lax",
            "Max-Age=2592000",
        ] {
            assert!(header.contains(attribute), "missing {attribute}: {header}");
        }
        // A `Domain` would break the `__Host-` prefix, which is what stops a
        // sibling subdomain from fixing a session on the parent.
        assert!(!header.contains("Domain"), "{header}");
    }

    /// A logout that appears to work and does not is worse than one that fails
    /// loudly, and the browser only replaces a cookie whose attributes match.
    #[test]
    fn the_clearing_cookie_matches_the_setting_cookie_attributes() {
        let set = set_cookie_value("__Host-albedo_session", &SessionToken::mint(), 60);
        let clear = clear_cookie_value("__Host-albedo_session");
        for attribute in ["Path=/", "HttpOnly", "Secure", "SameSite=Lax"] {
            assert!(set.contains(attribute) && clear.contains(attribute), "{attribute}");
        }
        assert!(clear.contains("Max-Age=0"));
    }

    #[test]
    fn a_cookie_is_found_among_its_neighbours() {
        let header = "theme=dark; __Host-albedo_session=tok123; __Host-albedo-session=abc";
        assert_eq!(
            read_cookie(header, "__Host-albedo_session"),
            Some("tok123")
        );
        assert_eq!(read_cookie(header, "theme"), Some("dark"));
        assert_eq!(read_cookie(header, "absent"), None);
    }

    /// The tab cookie and the auth cookie coexist in one header and must not be
    /// confused for one another — the distinction this module exists to keep.
    ///
    /// Since both took the `__Host-` prefix the two names differ by one
    /// character, `-` against `_`, which is exactly the margin a lookup that
    /// matched loosely would erase. The tab name is spelled out rather than
    /// imported because it belongs to the server crate downstream of this one;
    /// if it moves, this literal has to move with it.
    #[test]
    fn the_tab_cookie_is_not_mistaken_for_the_auth_cookie() {
        let header = "__Host-albedo-session=9f1c8a70-0000-4000-8000-000000000000";
        assert_eq!(read_cookie(header, "__Host-albedo_session"), None);
        assert!(read_cookie(header, "__Host-albedo-session").is_some());
    }

    #[test]
    fn a_cookie_header_without_an_equals_sign_does_not_panic() {
        assert_eq!(read_cookie("malformed", "anything"), None);
    }

    /// A valueless entry ahead of the session cookie is the dangerous order:
    /// stopping there returns no token, and a signed-in user is answered as
    /// anonymous with no error anywhere to explain it.
    #[test]
    fn a_valueless_entry_before_the_target_does_not_hide_it() {
        let header = "consent; __Host-albedo_session=tok123";
        assert_eq!(read_cookie(header, "__Host-albedo_session"), Some("tok123"));
    }

    /// The same header the other way round — so the test above cannot pass by
    /// accident on a scan that simply stops early.
    #[test]
    fn a_valueless_entry_after_the_target_does_not_hide_it() {
        let header = "__Host-albedo_session=tok123; consent";
        assert_eq!(read_cookie(header, "__Host-albedo_session"), Some("tok123"));
    }

    #[test]
    fn a_header_of_only_valueless_entries_finds_nothing() {
        assert_eq!(read_cookie("consent; dnt; whatever", "__Host-albedo_session"), None);
    }

    /// The tokenizer is now shared by all three readers, so its behaviour is
    /// tested directly rather than only through whichever caller happens to
    /// exercise it.
    #[test]
    fn the_tokenizer_yields_trimmed_pairs_in_order_and_drops_valueless_entries() {
        let entries: Vec<_> = cookie_entries("  a=1 ; consent;  b = 2 ; ; c=3").collect();
        assert_eq!(entries, vec![("a", "1"), ("b", "2"), ("c", "3")]);
    }

    /// A value may legally contain `=` — base64 padding is the common case, and
    /// a session token is base64url. Splitting on the *first* `=` is what keeps
    /// that intact; splitting on the last, or refusing the entry, would corrupt
    /// the token into something that simply fails to resolve.
    #[test]
    fn a_value_containing_equals_signs_survives_intact() {
        assert_eq!(read_cookie("t=YWJjZA==", "t"), Some("YWJjZA=="));
        assert_eq!(read_cookie("a=1; t=x=y=z", "t"), Some("x=y=z"));
    }

    /// An empty value is a real cookie, distinct from an absent one — it is
    /// exactly what `clear_cookie_value` sets to expire a session, so a reader
    /// that treated it as absent would disagree with the logout path.
    #[test]
    fn an_empty_value_is_present_not_absent() {
        assert_eq!(read_cookie("a=; b=2", "a"), Some(""));
        assert_eq!(read_cookie("a=; b=2", "c"), None);
    }
}
