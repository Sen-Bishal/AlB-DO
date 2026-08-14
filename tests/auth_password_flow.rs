//! AUTH · P2 — sign up, sign in, against a real substrate.
//!
//! `AUTH.md` § 9's gate for this phase is *"a stranger signs up and logs in with
//! no third party involved."* These are the store-level half of that, asserted
//! where the properties actually hold: the uniqueness constraint that makes
//! re-registration impossible lives in SQLite, and a mock would only prove the
//! SQL strings were written down.
//!
//! The HTTP half — CSRF, the cookie, the redirect, the limiter — is the server
//! crate's, over `handlers::auth_routes`.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use dom_render_compiler::auth::password::{hash_password, normalize_email, verify_password};
use dom_render_compiler::auth::store::{
    create_credential_account, create_session, lookup_credential, resolve_session, upsert_principal,
    ProviderProfile,
};
use dom_render_compiler::auth::schema;
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

const NOW: i64 = 1_754_400_000_000;
const DAY_MS: i64 = 86_400_000;
const PROVIDER: &str = "password";

async fn booted() -> LibSqlSubstrate {
    let db = LibSqlSubstrate::open_ephemeral().await.expect("open");
    let schema = schema::augment(&ForgeSchema::build(Vec::new()).expect("empty schema"))
        .expect("auth tables");
    bootstrap_schema(&db, &schema).await.expect("bootstrap");
    db
}

fn profile(email: &str) -> ProviderProfile {
    ProviderProfile {
        email: Some(email.to_string()),
        ..ProviderProfile::default()
    }
}

/// Sign up, then sign in, then hold a session that resolves. The phase gate, in
/// one function.
#[tokio::test]
async fn a_stranger_signs_up_and_then_signs_in() {
    let db = booted().await;
    let email = normalize_email("Ada@Example.com").expect("an address");
    let secret = hash_password("correct horse battery").expect("hashes");

    let created = create_credential_account(&db, PROVIDER, &email, &profile(&email), &secret, NOW)
        .await
        .expect("substrate")
        .expect("a fresh address registers");
    assert_eq!(created.provider, PROVIDER);
    assert_eq!(created.email.as_deref(), Some("ada@example.com"));

    // Signing in is a lookup plus a verify — no second source of truth about who
    // this is.
    let found = lookup_credential(&db, PROVIDER, &email)
        .await
        .expect("substrate")
        .expect("the account exists");
    assert_eq!(found.principal, created.id);
    assert!(verify_password(&found.secret_hash, "correct horse battery"));
    assert!(!verify_password(&found.secret_hash, "correct horse batterz"));

    // …and the session that follows resolves to the same human.
    let token = create_session(&db, &found.principal, PROVIDER, NOW, 30 * DAY_MS)
        .await
        .expect("session");
    let resolved = resolve_session(&db, &token, NOW + 1).await.expect("resolve");
    assert_eq!(resolved.principal().expect("live session").id, created.id);
}

/// **The account-takeover test.** Registering an address that already exists must
/// fail — never silently replace the password, and never hand back the existing
/// principal. `upsert_principal` deliberately does adopt an existing account,
/// which is right for OAuth and catastrophic here, so the two paths are separate
/// functions and this is the assertion that keeps them separate.
#[tokio::test]
async fn re_registering_an_address_never_replaces_its_password() {
    let db = booted().await;
    let email = normalize_email("ada@example.com").expect("an address");
    let original = hash_password("the original password").expect("hashes");
    let attacker = hash_password("the attacker password").expect("hashes");

    let first = create_credential_account(&db, PROVIDER, &email, &profile(&email), &original, NOW)
        .await
        .expect("substrate")
        .expect("first registration succeeds");

    let second =
        create_credential_account(&db, PROVIDER, &email, &profile(&email), &attacker, NOW + 1)
            .await
            .expect("substrate");
    assert!(second.is_none(), "a taken address must not register again");

    // The original password still works and the attacker's does not.
    let found = lookup_credential(&db, PROVIDER, &email)
        .await
        .expect("substrate")
        .expect("the account exists");
    assert_eq!(found.principal, first.id, "the principal did not change");
    assert!(verify_password(&found.secret_hash, "the original password"));
    assert!(!verify_password(&found.secret_hash, "the attacker password"));
}

/// The email is the account key, so the folding [`normalize_email`] does has to
/// be what actually decides identity — not a convenience applied in one place.
#[tokio::test]
async fn case_and_surrounding_space_do_not_create_a_second_account() {
    let db = booted().await;
    let secret = hash_password("correct horse battery").expect("hashes");

    let canonical = normalize_email("ada@example.com").expect("an address");
    create_credential_account(&db, PROVIDER, &canonical, &profile(&canonical), &secret, NOW)
        .await
        .expect("substrate")
        .expect("registers");

    for spelling in ["  Ada@Example.COM  ", "ADA@EXAMPLE.COM", "ada@example.com"] {
        let folded = normalize_email(spelling).expect("an address");
        let again =
            create_credential_account(&db, PROVIDER, &folded, &profile(&folded), &secret, NOW + 1)
                .await
                .expect("substrate");
        assert!(again.is_none(), "`{spelling}` must be the same account");
    }
}

/// A principal who has an account but no password row — a passkey-only human —
/// must be indistinguishable from an address nobody has registered. Both are
/// `None`, and the login path pays the same KDF cost for each.
#[tokio::test]
async fn an_account_with_no_password_looks_exactly_like_no_account() {
    let db = booted().await;

    // A passkey registration goes through `upsert_principal`, which writes the
    // user and account rows and no credential.
    let subject = "ada-authenticator";
    upsert_principal(&db, "passkey", subject, &profile("ada@example.com"), NOW)
        .await
        .expect("passkey account");

    let by_passkey_subject = lookup_credential(&db, "passkey", subject)
        .await
        .expect("substrate");
    assert!(
        by_passkey_subject.is_none(),
        "an account with no secret_hash must not yield a credential"
    );

    let never_registered = lookup_credential(&db, PROVIDER, "nobody@example.com")
        .await
        .expect("substrate");
    assert!(never_registered.is_none());
}

/// A password row is scoped to its provider. Declaring both `password` and a
/// custom credential provider must not let a secret written for one authenticate
/// against the other.
#[tokio::test]
async fn a_credential_does_not_cross_providers() {
    let db = booted().await;
    let email = normalize_email("ada@example.com").expect("an address");
    let secret = hash_password("correct horse battery").expect("hashes");

    create_credential_account(&db, PROVIDER, &email, &profile(&email), &secret, NOW)
        .await
        .expect("substrate")
        .expect("registers");

    let elsewhere = lookup_credential(&db, "legacy", &email)
        .await
        .expect("substrate");
    assert!(
        elsewhere.is_none(),
        "a password for `{PROVIDER}` must not authenticate against `legacy`"
    );
}

/// **Invariant 2.6, applied to the other credential we store.** The plaintext
/// must appear nowhere in the database — not in the credential row, not in the
/// user row, nowhere a backup or a support `SELECT *` would reach.
#[tokio::test]
async fn the_plaintext_password_is_nowhere_in_the_database() {
    let db = booted().await;
    let email = normalize_email("ada@example.com").expect("an address");
    let plaintext = "an unmistakable passphrase 8842";
    let secret = hash_password(plaintext).expect("hashes");

    create_credential_account(&db, PROVIDER, &email, &profile(&email), &secret, NOW)
        .await
        .expect("substrate")
        .expect("registers");

    for table in ["albedo_credentials", "albedo_users", "albedo_accounts"] {
        let rows = db
            .query(&format!("SELECT * FROM {table}"), &[])
            .await
            .expect("query");
        let dumped = format!("{rows:?}");
        assert!(
            !dumped.contains(plaintext),
            "`{table}` contains the plaintext password"
        );
    }
}
