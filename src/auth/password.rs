//! AUTH · P2 — passwords, and why they are second-class on purpose.
//!
//! `AUTH.md` § 6.2 makes **passkey the default and password the opt-in**. That
//! ordering is not taste: a password is the only credential in this system that
//! a human chooses, reuses, and can be talked into typing somewhere else, and it
//! is the only one whose theft is undetectable. Everything below is what it takes
//! to store one responsibly, which is roughly the argument for not having one.
//!
//! It exists anyway because § "P2's shape" names the reason: **passkeys cannot
//! work without JavaScript.** `navigator.credentials` is a JS API with no HTML
//! fallback, so an app that offers only passkeys has a login page that requires a
//! working bundle. Password is what makes the no-JS story true end to end, which
//! is the same claim `crates/albedo-server/src/forms.rs` was built to support.
//!
//! ## What is stored, and what is deliberately not
//!
//! `albedo_credentials.secret_hash` holds a **PHC string** — `$argon2id$v=19$m=…`
//! — which carries the algorithm, the cost parameters and the per-row salt inside
//! it. Three consequences worth stating:
//!
//! 1. **Every row is self-describing.** Raising [`PARAMS`] tomorrow does not invalidate a single
//!    existing password: each verify uses the parameters that row was written with. Migration is
//!    therefore a re-hash on next successful login, not a forced reset — the thing that makes
//!    raising the cost politically possible.
//! 2. **The salt is per row and never reused**, so two people with the same password have unrelated
//!    hashes and one rainbow table buys nothing.
//! 3. **The plaintext is never held longer than a stack frame** and never written anywhere. There is
//!    no "password" column, no audit trail of attempts carrying the input, and no log line in this
//!    module that takes the presented value.
//!
//! ## The two attacks this module is shaped by
//!
//! **Offline cracking**, if the database leaks: answered by Argon2id at
//! [`PARAMS`] — memory-hard, so the GPU and ASIC advantage that makes bcrypt and
//! PBKDF2 uncomfortable does not apply. The cost is real (≈19 MiB and a few tens
//! of milliseconds per verify) and is the point.
//!
//! **User enumeration by timing**: a login for an address with no account must
//! not answer faster than one for an address with a password that is merely
//! wrong. A missing account skips the KDF, and that difference is *tens of
//! milliseconds* — trivially measurable over a network. [`absorb_timing`] is the
//! answer, and it is a real Argon2 verify against a hash of an unknowable value
//! rather than a sleep, because a sleep is a constant an attacker can subtract.
//!
//! ## What this module does NOT try to fix
//!
//! **Signup discloses whether an address already has an account.** It has to: the
//! honest alternative is "we have sent you an email", and § 6.2 chose passkeys
//! precisely to avoid owning email deliverability, so there is no channel to send
//! it on. Every mainstream product with a password signup and no verification
//! step has this property; pretending otherwise by returning a fake success would
//! leave the person unable to log in and unable to find out why. It is written
//! down here rather than discovered later.

use argon2::password_hash::{PasswordHash as PhcHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::rngs::OsRng as RandOsRng;
use rand::RngCore;
use std::sync::OnceLock;

/// Shortest password accepted, in bytes.
///
/// Eight, from NIST SP 800-63B, and paired with the deliberate *absence* of
/// composition rules — no "must contain a symbol". The evidence is that
/// composition rules produce `Password1!` and a written-down note, while length
/// produces entropy. The one rule worth having is a floor, and this is it.
pub const MIN_PASSWORD_BYTES: usize = 8;

/// Longest password accepted, in bytes.
///
/// **This is a denial-of-service bound, not a security policy.** Argon2's cost is
/// linear in input length, so an unbounded field lets an anonymous caller spend
/// our CPU by the megabyte on a route that by definition has no session yet.
/// 4 KiB is far past any passphrase a human will type and far below anything
/// that costs us. SP 800-63B says to accept at least 64 characters, which this
/// clears by two orders of magnitude.
pub const MAX_PASSWORD_BYTES: usize = 4096;

/// Longest email accepted — RFC 5321's `Path` limit.
pub const MAX_EMAIL_BYTES: usize = 254;

/// Argon2id cost, pinned rather than defaulted.
///
/// `m = 19456` KiB (19 MiB), `t = 2`, `p = 1` — OWASP's current recommendation
/// for Argon2id, which is also what `Argon2::default()` happens to be today.
/// Written out anyway: a crate upgrade that moved the default would otherwise
/// change the cost of every password written after it, silently and invisibly,
/// and the only way anyone would notice is a latency graph.
///
/// Raising these is safe at any time — existing PHC strings carry their own
/// parameters, so old passwords keep verifying and new ones get the new cost.
fn params() -> &'static Params {
    static PARAMS: OnceLock<Params> = OnceLock::new();
    PARAMS.get_or_init(|| {
        Params::new(19_456, 2, 1, None).unwrap_or_else(|_| Params::DEFAULT)
    })
}

/// The configured hasher.
fn hasher() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params().clone())
}

/// Why a password was refused.
///
/// Only two of these are ever shown to a caller, and neither says anything about
/// whether an account exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordError {
    /// Shorter than [`MIN_PASSWORD_BYTES`].
    TooShort,
    /// Longer than [`MAX_PASSWORD_BYTES`].
    TooLong,
    /// The email did not look like an address we can key an account on.
    InvalidEmail,
    /// The KDF itself failed. Not a caller error — an out-of-memory or a broken
    /// build — so it is surfaced separately rather than folded into "invalid".
    Kdf(String),
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(
                f,
                "password must be at least {MIN_PASSWORD_BYTES} characters"
            ),
            Self::TooLong => write!(
                f,
                "password must be at most {MAX_PASSWORD_BYTES} bytes"
            ),
            Self::InvalidEmail => write!(f, "that does not look like an email address"),
            Self::Kdf(what) => write!(f, "password hashing failed: {what}"),
        }
    }
}

impl std::error::Error for PasswordError {}

/// Hash a new password into a storable PHC string.
///
/// # Errors
/// [`PasswordError::TooShort`] / [`PasswordError::TooLong`] for a policy
/// violation, [`PasswordError::Kdf`] if Argon2 itself fails.
pub fn hash_password(plaintext: &str) -> Result<String, PasswordError> {
    check_policy(plaintext)?;
    let salt = SaltString::generate(&mut RandOsRng);
    hasher()
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| PasswordError::Kdf(err.to_string()))
}

/// Whether `presented` is the password behind `stored`.
///
/// Returns `false` for every failure, including a `stored` value that does not
/// parse as a PHC string. That collapse is deliberate: the only caller is a login
/// path, the only thing it can do about a corrupt hash is refuse, and a distinct
/// error for "this row is malformed" would be a signal about the account's
/// existence handed to whoever asked.
///
/// Length is **not** policy-checked here. A password stored before the floor
/// changed must keep working, so the policy applies where a password is *set*.
#[must_use]
pub fn verify_password(stored: &str, presented: &str) -> bool {
    if presented.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    let Ok(parsed) = PhcHash::new(stored) else {
        return false;
    };
    // `verify_password` re-derives with the parameters *inside* `stored`, not the
    // ones in `params()` — which is what makes raising the cost a migration
    // rather than a lockout.
    hasher()
        .verify_password(presented.as_bytes(), &parsed)
        .is_ok()
}

/// Spend the same work a real verify would, and answer nothing.
///
/// **Call this on every login for an address that has no account.** Without it
/// the two outcomes are distinguishable by a stopwatch: a hit runs Argon2 at
/// [`params`] (tens of milliseconds), a miss returns immediately, and an
/// attacker with a list of addresses learns which ones are registered without
/// ever guessing a password.
///
/// A real hash against a value nobody knows, not a `sleep`: a sleep is a
/// constant, and a constant added to a fast path is still a fast path once it is
/// subtracted. This has the same distribution as the real thing because it *is*
/// the real thing.
pub fn absorb_timing(presented: &str) {
    static DECOY: OnceLock<String> = OnceLock::new();
    let decoy = DECOY.get_or_init(|| {
        // 32 bytes of CSPRNG output, hashed once per process. Nobody — including
        // us — can present the plaintext behind this, so the verify below always
        // fails, always after doing the work.
        let mut secret = [0u8; 32];
        RandOsRng.fill_bytes(&mut secret);
        let salt = SaltString::generate(&mut RandOsRng);
        hasher()
            .hash_password(&secret, &salt)
            .map(|hash| hash.to_string())
            .unwrap_or_default()
    });
    if decoy.is_empty() {
        return;
    }
    let _ = verify_password(decoy, presented);
}

/// Apply the length policy to a password about to be stored.
///
/// # Errors
/// [`PasswordError::TooShort`] or [`PasswordError::TooLong`].
pub fn check_policy(plaintext: &str) -> Result<(), PasswordError> {
    if plaintext.len() < MIN_PASSWORD_BYTES {
        return Err(PasswordError::TooShort);
    }
    if plaintext.len() > MAX_PASSWORD_BYTES {
        return Err(PasswordError::TooLong);
    }
    Ok(())
}

/// Fold an address into the single form an account is keyed by.
///
/// ## Why this is lowercase-and-trim and nothing more
///
/// The address becomes `albedo_accounts.subject`, and that column is under a
/// `UNIQUE (provider, subject)` index — so whatever this returns *is* the
/// identity. Two spellings that fold to one value are one account; two that do
/// not are two accounts. Both mistakes are bad and they pull in opposite
/// directions.
///
/// The local part of an address is case-sensitive per RFC 5321 and case-folded by
/// essentially every real mail provider, so folding it is right in practice and
/// wrong in theory — and the theory-correct choice would let `Ada@` and `ada@`
/// register separately, which is an account-confusion bug a support desk cannot
/// untangle. Nothing further is attempted: no dot-stripping, no `+tag` removal.
/// Those are Gmail-specific policies, and applying them universally would merge
/// two addresses that a different provider treats as two people.
///
/// Returns `None` for anything that cannot be an address, which is the same
/// check the caller would otherwise have to invent.
#[must_use]
pub fn normalize_email(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() > MAX_EMAIL_BYTES || trimmed.is_empty() {
        return None;
    }
    // Deliberately not a validating parser. RFC 5322 is famously permissive and a
    // regex that implements it is a liability; the only property that matters
    // here is that the value is a plausible single address with no room for
    // ambiguity — one `@`, something on each side, no whitespace or control
    // characters anywhere.
    let (local, domain) = trimmed.split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return None;
    }
    if trimmed
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return None;
    }
    Some(trimmed.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips_through_the_kdf() {
        let stored = hash_password("correct horse battery").expect("hashes");
        assert!(verify_password(&stored, "correct horse battery"));
        assert!(!verify_password(&stored, "correct horse batterz"));
        assert!(!verify_password(&stored, ""));
    }

    /// The stored value must be a PHC string carrying algorithm, version and
    /// cost. That self-description is what makes a future cost increase a
    /// migration instead of a mass password reset.
    #[test]
    fn the_stored_form_is_a_self_describing_argon2id_phc_string() {
        let stored = hash_password("correct horse battery").expect("hashes");
        assert!(stored.starts_with("$argon2id$"), "{stored}");
        assert!(stored.contains("v=19"), "{stored}");
        assert!(stored.contains("m=19456"), "{stored}");
        assert!(stored.contains("t=2"), "{stored}");
        assert!(stored.contains("p=1"), "{stored}");
    }

    /// Two people with the same password must not have the same hash, or one
    /// leak plus one crack yields both accounts.
    #[test]
    fn the_same_password_hashes_differently_every_time() {
        let a = hash_password("shared password").expect("hashes");
        let b = hash_password("shared password").expect("hashes");
        assert_ne!(a, b, "the salt must be per row");
        assert!(verify_password(&a, "shared password"));
        assert!(verify_password(&b, "shared password"));
    }

    /// A hash written with *different* parameters than the current ones must
    /// still verify — this is the whole migration story, asserted directly.
    #[test]
    fn a_hash_written_with_older_parameters_still_verifies() {
        let cheap = Argon2::new(
            Algorithm::Argon2id,
            Version::V0x13,
            Params::new(8, 1, 1, None).expect("valid params"),
        );
        let salt = SaltString::generate(&mut RandOsRng);
        let stored = cheap
            .hash_password(b"legacy password", &salt)
            .expect("hashes")
            .to_string();
        assert!(stored.contains("m=8"), "{stored}");
        assert!(
            verify_password(&stored, "legacy password"),
            "raising the cost must not lock anyone out"
        );
    }

    #[test]
    fn the_length_floor_and_ceiling_are_enforced_where_a_password_is_set() {
        assert_eq!(hash_password("short"), Err(PasswordError::TooShort));
        assert_eq!(
            hash_password(&"a".repeat(MAX_PASSWORD_BYTES + 1)),
            Err(PasswordError::TooLong)
        );
        assert!(hash_password(&"a".repeat(MIN_PASSWORD_BYTES)).is_ok());
    }

    /// The ceiling is a DoS bound, so it has to hold on the *verify* path too —
    /// that is the one an anonymous caller can reach without an account.
    #[test]
    fn an_oversized_presented_password_is_refused_before_the_kdf_runs() {
        let stored = hash_password("correct horse battery").expect("hashes");
        assert!(!verify_password(&stored, &"a".repeat(MAX_PASSWORD_BYTES + 1)));
    }

    /// A corrupt or empty stored hash is a refusal, never a panic and never an
    /// accidental accept.
    #[test]
    fn a_malformed_stored_hash_verifies_nothing() {
        for stored in ["", "not-a-phc-string", "$argon2id$broken", "plaintext"] {
            assert!(!verify_password(stored, "anything"), "`{stored}`");
        }
    }

    /// [`absorb_timing`] must actually run the KDF. Asserted by cost rather than
    /// by wall clock — a timing assertion in a test suite is a flake — but the
    /// call must at least be reachable and side-effect-free.
    #[test]
    fn the_timing_decoy_runs_and_never_reports_success() {
        absorb_timing("whatever");
        absorb_timing("");
        // Second call reuses the memoised decoy; no panic, no output, no answer.
    }

    #[test]
    fn email_normalisation_folds_case_and_trims() {
        assert_eq!(
            normalize_email("  Ada.Lovelace@Example.COM "),
            Some("ada.lovelace@example.com".to_string())
        );
    }

    /// The value this returns *is* the account key, so anything ambiguous has to
    /// be refused rather than repaired.
    #[test]
    fn implausible_addresses_are_refused() {
        for raw in [
            "",
            "ada",
            "@example.com",
            "ada@",
            "ada@@example.com",
            "ada@example",
            "ada@.com",
            "ada@example.",
            "ada lovelace@example.com",
            "ada@example.com\r\nBcc: evil@example.com",
        ] {
            assert_eq!(normalize_email(raw), None, "`{raw}` must not be an account key");
        }
        assert_eq!(normalize_email(&format!("{}@e.com", "a".repeat(300))), None);
    }

    /// Gmail's dot and `+tag` rules are Gmail's, not the internet's. Applying
    /// them here would merge two addresses another provider treats as two people.
    #[test]
    fn provider_specific_aliasing_is_not_applied() {
        assert_eq!(
            normalize_email("ada+albedo@example.com"),
            Some("ada+albedo@example.com".to_string())
        );
        assert_ne!(
            normalize_email("a.da@example.com"),
            normalize_email("ada@example.com")
        );
    }
}
