//! AUTH · account linking and the `user` a request sees — against a real
//! substrate.
//!
//! Before this, GitHub, Google and a password with the same email were three
//! principals: `upsert_principal` keys on `(provider, subject)` and nothing ever
//! joined two identities. A customer who paid signed in one way and later signed
//! in another saw an empty account. These pin the join that fixes it — and the
//! refusals that keep it from being an account takeover.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use dom_render_compiler::auth::principal::PrincipalId;
use dom_render_compiler::auth::session::SessionToken;
use dom_render_compiler::auth::store::{
    create_session, link_account, load_principal, resolve_session, unlink_account,
    upsert_principal, LinkOutcome, ProviderProfile, UnlinkOutcome,
};
use dom_render_compiler::auth::{schema, Principal};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

const NOW: i64 = 1_754_400_000_000;
const DAY_MS: i64 = 86_400_000;

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
        name: Some("Ada".to_string()),
        image: None,
        claims: serde_json::Map::new(),
    }
}

async fn sign_in(db: &LibSqlSubstrate, provider: &str, subject: &str) -> (Principal, SessionToken) {
    let principal = upsert_principal(db, provider, subject, &profile("ada@example.com"), NOW)
        .await
        .expect("upsert");
    let token = create_session(db, &principal.id, provider, NOW, 30 * DAY_MS)
        .await
        .expect("session");
    (principal, token)
}

async fn resolved(db: &LibSqlSubstrate, token: &SessionToken) -> Principal {
    resolve_session(db, token, NOW + 1)
        .await
        .expect("resolve")
        .principal()
        .cloned()
        .expect("a live session")
}

/// The session a request resolves carries every connected provider, and
/// `user_value` shows them.
#[tokio::test]
async fn a_resolved_session_lists_every_connected_provider() {
    let db = booted().await;
    let (principal, token) = sign_in(&db, "github", "gh-1").await;
    assert_eq!(resolved(&db, &token).await.providers, ["github"]);

    link_account(&db, &principal.id, "google", "g-1", NOW).await.expect("link");
    let seen = resolved(&db, &token).await;
    assert_eq!(seen.providers, ["github", "google"]);
    assert_eq!(seen.user_value()["providers"], serde_json::json!(["github", "google"]));
    assert_eq!(seen.user_value()["email"], "ada@example.com");
    assert_eq!(seen.user_value()["provider"], "github");
}

/// The headline. Once Google is connected, signing in with Google is the SAME
/// principal — the customer's data is where they left it.
#[tokio::test]
async fn a_linked_identity_signs_in_as_the_principal_it_was_linked_to() {
    let db = booted().await;
    let (principal, _) = sign_in(&db, "github", "gh-1").await;

    assert_eq!(
        link_account(&db, &principal.id, "google", "g-1", NOW).await.unwrap(),
        LinkOutcome::Linked
    );
    let (via_google, _) = sign_in(&db, "google", "g-1").await;
    assert_eq!(via_google.id, principal.id);

    // …and a second click is not an error.
    assert_eq!(
        link_account(&db, &principal.id, "google", "g-1", NOW).await.unwrap(),
        LinkOutcome::AlreadyLinked
    );
}

/// 🔒 The takeover refusal. An identity that already signs in as somebody else
/// is never moved — not even when the emails match.
#[tokio::test]
async fn an_identity_owned_by_another_principal_is_never_taken() {
    let db = booted().await;
    let (victim, _) = sign_in(&db, "google", "g-victim").await;
    let (attacker, _) = sign_in(&db, "github", "gh-attacker").await;

    assert_eq!(
        link_account(&db, &attacker.id, "google", "g-victim", NOW).await.unwrap(),
        LinkOutcome::TakenByAnother
    );
    let (still, _) = sign_in(&db, "google", "g-victim").await;
    assert_eq!(still.id, victim.id, "the identity must still sign in as its owner");
}

/// Same email at two providers is NOT a link. Only an explicit link is.
#[tokio::test]
async fn matching_emails_alone_never_join_two_principals() {
    let db = booted().await;
    let (a, _) = sign_in(&db, "github", "gh-1").await;
    let (b, _) = sign_in(&db, "google", "g-1").await; // same email, no link
    assert_ne!(a.id, b.id);
}

#[tokio::test]
async fn a_second_identity_at_the_same_provider_is_refused() {
    let db = booted().await;
    let (principal, _) = sign_in(&db, "github", "gh-1").await;
    assert_eq!(
        link_account(&db, &principal.id, "github", "gh-2", NOW).await.unwrap(),
        LinkOutcome::ProviderAlreadyConnected
    );
}

/// The last way in cannot be removed — the account would be locked with its
/// data inside.
#[tokio::test]
async fn the_last_sign_in_method_cannot_be_disconnected() {
    let db = booted().await;
    let (principal, token) = sign_in(&db, "github", "gh-1").await;
    assert_eq!(
        unlink_account(&db, &principal.id, "github", &token.hash()).await.unwrap(),
        UnlinkOutcome::LastMethod
    );
    assert_eq!(
        unlink_account(&db, &principal.id, "google", &token.hash()).await.unwrap(),
        UnlinkOutcome::NotConnected
    );
}

/// Disconnecting ends the sessions that provider opened on OTHER devices, keeps
/// the one asking, drops a password credential with it, and the identity no
/// longer signs in as this principal.
#[tokio::test]
async fn disconnecting_ends_its_other_sessions_and_keeps_the_one_asking() {
    let db = booted().await;
    let (principal, github_session) = sign_in(&db, "github", "gh-1").await;
    link_account(&db, &principal.id, "google", "g-1", NOW).await.unwrap();
    let google_elsewhere = create_session(&db, &principal.id, "google", NOW, DAY_MS)
        .await
        .unwrap();
    let google_here = create_session(&db, &principal.id, "google", NOW, DAY_MS)
        .await
        .unwrap();
    db.execute(
        "INSERT INTO albedo_credentials (principal, provider, secret_hash, created_at) \
         VALUES (?1, 'google', 'x', ?2)",
        &[
            SqlValue::Text(principal.id.as_str().to_string()),
            SqlValue::Integer(NOW),
        ],
    )
    .await
    .unwrap();

    let outcome = unlink_account(&db, &principal.id, "google", &google_here.hash())
        .await
        .unwrap();
    assert_eq!(outcome, UnlinkOutcome::Unlinked { sessions_ended: 1 });

    assert!(resolve_session(&db, &google_elsewhere, NOW + 1)
        .await
        .unwrap()
        .principal()
        .is_none());
    assert_eq!(resolved(&db, &google_here).await.id, principal.id);
    assert_eq!(resolved(&db, &github_session).await.providers, ["github"]);

    let credentials = db
        .query(
            "SELECT 1 FROM albedo_credentials WHERE principal = ?1 AND provider = 'google'",
            &[SqlValue::Text(principal.id.as_str().to_string())],
        )
        .await
        .unwrap();
    assert!(credentials.rows.is_empty(), "the credential goes with the provider");

    let (fresh, _) = sign_in(&db, "google", "g-1").await;
    assert_ne!(fresh.id, principal.id, "a disconnected identity is a stranger again");
}

/// A path with no session — a job, a webhook — still gets the whole profile.
#[tokio::test]
async fn a_principal_loads_by_id_with_no_session() {
    let db = booted().await;
    let (principal, _) = sign_in(&db, "github", "gh-1").await;
    let loaded = load_principal(&db, &principal.id).await.unwrap().expect("exists");
    assert_eq!(loaded.email.as_deref(), Some("ada@example.com"));
    assert_eq!(loaded.providers, ["github"]);
    assert_eq!(loaded.user_value()["provider"], serde_json::Value::Null);

    let stranger = PrincipalId::parse("u_nobody").unwrap();
    assert!(load_principal(&db, &stranger).await.unwrap().is_none());
}
