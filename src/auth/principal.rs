//! AUTH · P0 — the principal, and the one id every provider lands on.
//!
//! `development-plan/AUTH.md` invariant 2.1: a first-party passkey login and a
//! delegated Clerk JWT produce the same value, so app code cannot detect which
//! and a provider swap is a config edit.
//!
//! ## Why the id is always ours
//!
//! A principal's id is not decoration — it becomes a **partition key**, and
//! therefore a topic namespace, the moment anyone writes
//! `todos.where({ owner: user.id })`. That puts it under
//! [`is_valid_partition_key`](crate::runtime::broadcast::is_valid_partition_key)'s
//! alphabet, `[A-Za-z0-9_-]{1,64}`, which is deliberately narrow because
//! excluding `:` is what makes two partitions aliasing onto one channel
//! *unexpressible* rather than merely checked (PRISM invariant 5).
//!
//! Provider subjects do not respect that alphabet, and cannot be made to:
//!
//! | provider | subject | in the alphabet? |
//! |---|---|---|
//! | Google / Azure AD | `104829…`, a GUID | yes, by luck |
//! | Clerk | `user_2abc…` | yes, by luck |
//! | Auth0 | `google-oauth2\|10482…` | **no** — `\|` |
//! | anything email-keyed | `ada@example.com` | **no** — `@`, `.` |
//!
//! So a delegated subject can never *be* the id. It is stored beside one, in the
//! `accounts` table, and [`PrincipalId`] is minted by us. Three things fall out
//! that we would otherwise have had to argue for separately:
//!
//! 1. **The alphabet holds by construction**, for providers that do not exist yet.
//! 2. **R3 resolves to *mirror*, not *reference*** — the question `AUTH.md` § 8 left
//!    open. A row we own is what makes joins and § 5's instant revocation possible.
//! 3. **Account linking becomes expressible**: one human, two providers, two
//!    `accounts` rows, one `PrincipalId`. A derived id could not say that.
//!
//! The cost is one indirection at login — a lookup keyed `(provider, subject)`.
//! That is the trade, and it is the right way round.

use crate::runtime::broadcast::is_valid_partition_key;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// The prefix every minted principal id carries.
///
/// Not cosmetic: it makes a principal id greppable in a log, a topic string and
/// a database column, and it keeps a minted id textually distinguishable from a
/// provider subject that happens to be alphanumeric. A bare `104829…` in a topic
/// name tells a reader nothing; `u_…` tells them what they are looking at.
pub const PRINCIPAL_ID_PREFIX: &str = "u_";

/// A validated principal identifier — the value `user.id` resolves to.
///
/// The newtype exists so the partition-key alphabet is checked **once, here**,
/// rather than at each of the render, subscribe and action paths that will
/// interpolate it into a topic. Construction is the only gate, so a
/// `PrincipalId` in hand is a proof that
/// [`partition_topic_name`](crate::runtime::broadcast::partition_topic_name)
/// cannot reject it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PrincipalId(String);

impl PrincipalId {
    /// Mint a fresh identifier for a newly seen human.
    ///
    /// Simple-hex UUID rather than a shorter encoding: 34 characters is well
    /// inside the 64-byte partition-key bound, and every character is already in
    /// the alphabet, so the mint cannot produce a value its own validator would
    /// reject.
    #[must_use]
    pub fn mint() -> Self {
        Self(format!(
            "{PRINCIPAL_ID_PREFIX}{}",
            Uuid::new_v4().as_simple()
        ))
    }

    /// Adopt an id that already exists — read back from the `users` table, or
    /// carried on a session row.
    ///
    /// # Errors
    /// [`PrincipalIdError`] when the value is outside the partition-key
    /// alphabet. A stored id failing this is a corrupted row rather than user
    /// input, but it is checked anyway: the alternative is discovering it at the
    /// point where it has already been interpolated into a topic.
    pub fn parse(raw: impl Into<String>) -> Result<Self, PrincipalIdError> {
        let raw = raw.into();
        if !is_valid_partition_key(&raw) {
            return Err(PrincipalIdError { found: raw });
        }
        Ok(Self(raw))
    }

    /// The id as it appears in a topic name, a SQL parameter and a log line.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<PrincipalId> for String {
    fn from(id: PrincipalId) -> Self {
        id.0
    }
}

impl TryFrom<String> for PrincipalId {
    type Error = PrincipalIdError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(raw)
    }
}

/// A principal id outside the partition-key alphabet.
///
/// Carries the offending value because the realistic cause is a provider
/// subject that reached the id slot — and naming the subject is what makes that
/// mistake obvious rather than mysterious.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalIdError {
    /// The value that was refused.
    pub found: String,
}

impl fmt::Display for PrincipalIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a usable principal id — an id becomes a topic namespace the moment a \
             component reads `user.id`, so it must match [A-Za-z0-9_-]{{1,64}}. A provider's own \
             subject (Auth0's `google-oauth2|…`, an email address) does not qualify and is never \
             the id: store it on the `accounts` row and mint a principal id beside it",
            self.found
        )
    }
}

impl std::error::Error for PrincipalIdError {}

/// Who is making this request.
///
/// **One shape, whoever minted it** — `AUTH.md` invariant 2.1. Only [`Self::id`]
/// is guaranteed, because it is the only field every provider has: a passkey
/// login carries no email, a GitHub token may carry no name, an enterprise IdP
/// may carry neither.
///
/// [`Self::claims`] is the provider's raw payload, passed through rather than
/// normalised. Normalising it would mean deciding, for every provider that will
/// ever exist, which of its claims matter — so instead the shape is preserved
/// and typed per provider by the generated `.d.ts`, the same way
/// [`aperture::typegen`](crate::aperture::typegen) types a source response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Principal {
    /// The id we minted. Stable across provider swaps for the same human, and
    /// always inside the partition-key alphabet — see the module docs.
    pub id: PrincipalId,
    /// Best-known email, when the provider supplied one. **Not an identifier**:
    /// two providers can disagree about it, and it can change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Display name, when the provider supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Avatar URL, when the provider supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Which declared provider authenticated this request — the key from the
    /// `auth.providers` block, not the provider *kind*. Two Okta tenants are two
    /// names and one kind, and telling them apart matters.
    pub provider: String,
    /// The provider's raw claim payload.
    ///
    /// Never merged into the fields above, so a provider cannot overwrite an id
    /// by naming a claim `id`.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub claims: serde_json::Map<String, serde_json::Value>,
}

impl Principal {
    /// The minimum viable principal: an id and the provider that vouched for it.
    #[must_use]
    pub fn new(id: PrincipalId, provider: impl Into<String>) -> Self {
        Self {
            id,
            email: None,
            name: None,
            image: None,
            provider: provider.into(),
            claims: serde_json::Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_id_is_always_a_valid_partition_key() {
        // The mint and the validator must agree, or `user.id` fails at the
        // topic-naming site rather than here. 256 samples is enough to catch a
        // formatting mistake; it is not a randomness test.
        for _ in 0..256 {
            let id = PrincipalId::mint();
            assert!(
                is_valid_partition_key(id.as_str()),
                "minted `{id}` is outside the partition-key alphabet"
            );
            assert!(id.as_str().starts_with(PRINCIPAL_ID_PREFIX));
        }
    }

    #[test]
    fn minted_ids_are_distinct() {
        assert_ne!(PrincipalId::mint(), PrincipalId::mint());
    }

    /// The whole reason the id is ours. Each of these is a real subject format
    /// from a provider we intend to support.
    #[test]
    fn real_provider_subjects_are_refused_as_ids() {
        for subject in [
            "google-oauth2|104829901776232416982", // Auth0
            "ada@example.com",                     // any email-keyed IdP
            "urn:example:user:7",                  // SAML-ish
            "",                                    // absent claim
        ] {
            assert!(
                PrincipalId::parse(subject).is_err(),
                "`{subject}` must not be usable as a principal id"
            );
        }
    }

    /// Subjects that *do* fit the alphabet are still not adopted as ids by any
    /// production path — but parsing them must work, because a stored id read
    /// back from the `users` table goes through the same door.
    #[test]
    fn a_conforming_id_round_trips() {
        let id = PrincipalId::parse("u_7f3a").expect("inside the alphabet");
        assert_eq!(id.as_str(), "u_7f3a");
        assert_eq!(String::from(id.clone()), "u_7f3a");
        assert_eq!(id.to_string(), "u_7f3a");
    }

    #[test]
    fn an_id_longer_than_the_bound_is_refused() {
        assert!(PrincipalId::parse(format!("u_{}", "a".repeat(63))).is_err());
        assert!(PrincipalId::parse(format!("u_{}", "a".repeat(62))).is_ok());
    }

    /// A provider must not be able to overwrite the id by naming a claim `id` —
    /// claims are a sibling of the fields, never a source for them.
    #[test]
    fn claims_are_carried_beside_the_fields_not_merged_into_them() {
        let mut principal = Principal::new(PrincipalId::mint(), "clerk");
        let minted = principal.id.clone();
        principal
            .claims
            .insert("id".to_string(), serde_json::json!("attacker"));
        assert_eq!(principal.id, minted);
    }

    #[test]
    fn a_principal_round_trips_through_json() {
        let mut principal = Principal::new(PrincipalId::mint(), "passkey");
        principal.email = Some("ada@example.com".to_string());
        let encoded = serde_json::to_string(&principal).expect("serialize");
        let decoded: Principal = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(principal, decoded);
    }

    /// Deserialization goes through `PrincipalId::parse`, so a hand-edited or
    /// corrupted row cannot smuggle a topic separator into an id.
    #[test]
    fn json_deserialization_enforces_the_alphabet() {
        let hostile = r#"{"id":"u_7f3a:admin","provider":"passkey"}"#;
        assert!(serde_json::from_str::<Principal>(hostile).is_err());
    }
}
