//! AUTH · P0 — the session lifecycle against a real substrate.
//!
//! Login → cookie → principal → rotate → revoke, plus the two races whose
//! losing side corrupts ownership rather than merely failing. These are the
//! properties `AUTH.md` invariants 2.2, 2.5 and 2.6 and risk R4 name, asserted
//! where they actually hold — a mock substrate would prove only that the SQL
//! strings were written down.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use dom_render_compiler::auth::principal::PrincipalId;
use dom_render_compiler::auth::session::{SessionRejection, SessionToken};
use dom_render_compiler::auth::store::{
    create_session, delete_principal, purge_expired_sessions, resolve_session, revoke_all_sessions,
    revoke_session, rotate_session, sessions_for, upsert_principal, ProviderProfile, Resolved,
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

fn profile(email: &str, name: &str) -> ProviderProfile {
    ProviderProfile {
        email: Some(email.to_string()),
        name: Some(name.to_string()),
        image: None,
        claims: serde_json::Map::new(),
    }
}

async fn login(db: &LibSqlSubstrate, provider: &str, subject: &str) -> (Principal, SessionToken) {
    let principal = upsert_principal(db, provider, subject, &profile("ada@example.com", "Ada"), NOW)
        .await
        .expect("upsert");
    let token = create_session(db, &principal.id, provider, NOW, 30 * DAY_MS)
        .await
        .expect("create session");
    (principal, token)
}

/// The P0 gate: a cookie resolves to a `user` on the request path.
#[tokio::test]
async fn a_session_cookie_resolves_to_its_principal() {
    let db = booted().await;
    let (principal, token) = login(&db, "passkey", "ada-authenticator").await;

    let resolved = resolve_session(&db, &token, NOW + 1).await.expect("resolve");
    let seen = resolved.principal().expect("a live session resolves");

    assert_eq!(seen.id, principal.id);
    assert_eq!(seen.email.as_deref(), Some("ada@example.com"));
    assert_eq!(seen.name.as_deref(), Some("Ada"));
    assert_eq!(seen.provider, "passkey");
}

/// **Invariant 2.6.** The stored value must not be replayable — so the raw
/// token must appear nowhere in the table, and a token that merely *looks* right
/// must not resolve.
#[tokio::test]
async fn the_raw_token_is_never_stored_and_a_forged_one_does_not_resolve() {
    let db = booted().await;
    let (_, token) = login(&db, "passkey", "ada-authenticator").await;

    let stored = db
        .query("SELECT token_hash FROM albedo_sessions", &[])
        .await
        .expect("read sessions");
    let hash = stored.rows[0]
        .get(0)
        .and_then(SqlValue::as_str)
        .expect("hash column");
    assert_ne!(
        hash,
        token.expose_for_cookie(),
        "the stored value is the token itself — a database read is a login"
    );

    // Presenting the *hash* must not work either: it is not the token, and a
    // system that accepted it would have made the database replayable after all.
    let forged = SessionToken::from_presented(hash).expect("non-empty");
    assert!(
        resolve_session(&db, &forged, NOW + 1)
            .await
            .expect("resolve")
            .principal()
            .is_none(),
        "the stored hash must not itself be a valid bearer token"
    );
}

#[tokio::test]
async fn an_unknown_token_resolves_to_nobody() {
    let db = booted().await;
    let stranger = SessionToken::mint();

    let resolved = resolve_session(&db, &stranger, NOW).await.expect("resolve");
    assert!(matches!(
        resolved,
        Resolved::Anonymous(SessionRejection::UnknownToken)
    ));
}

#[tokio::test]
async fn an_expired_session_resolves_to_nobody() {
    let db = booted().await;
    let (_, token) = login(&db, "passkey", "ada-authenticator").await;

    let live = resolve_session(&db, &token, NOW + 30 * DAY_MS - 1)
        .await
        .expect("resolve");
    assert!(live.principal().is_some(), "still inside the TTL");

    let dead = resolve_session(&db, &token, NOW + 30 * DAY_MS)
        .await
        .expect("resolve");
    assert!(matches!(
        dead,
        Resolved::Anonymous(SessionRejection::Expired)
    ));
}

/// **R4 — session fixation.** A token held before login must not become the
/// victim's session after it. Rotation is what breaks that, and the old value
/// must stop working the instant the new one starts.
#[tokio::test]
async fn rotation_invalidates_the_old_token_and_issues_a_working_one() {
    let db = booted().await;
    let (principal, planted) = login(&db, "passkey", "ada-authenticator").await;

    let rotated = rotate_session(
        &db,
        &planted.hash(),
        &principal.id,
        "passkey",
        NOW + 1,
        30 * DAY_MS,
    )
    .await
    .expect("rotate");

    assert!(
        resolve_session(&db, &planted, NOW + 2)
            .await
            .expect("resolve")
            .principal()
            .is_none(),
        "the pre-login token still works — this is the fixation bug"
    );
    assert_eq!(
        resolve_session(&db, &rotated, NOW + 2)
            .await
            .expect("resolve")
            .principal()
            .expect("the new token works")
            .id,
        principal.id
    );

    // Exactly one session survives: a rotation that inserted without deleting
    // would leave two live logins for one human.
    let live = sessions_for(&db, &principal.id, NOW + 2)
        .await
        .expect("device list");
    assert_eq!(live.len(), 1);
    assert_eq!(
        live[0].rotated_from.as_deref(),
        Some(planted.hash().as_str()),
        "the replaced session is recorded, so a replay is distinguishable later"
    );
}

/// **The race whose losing side corrupts ownership.** Two first logins for one
/// human must converge on one principal — otherwise the loser owns rows their
/// own session can never name.
#[tokio::test]
async fn concurrent_first_logins_converge_on_one_principal() {
    let db = booted().await;

    let ada = profile("ada@example.com", "Ada");
    let (first, second) = tokio::join!(
        upsert_principal(&db, "google", "104829901776232416982", &ada, NOW),
        upsert_principal(&db, "google", "104829901776232416982", &ada, NOW),
    );

    let first = first.expect("first upsert");
    let second = second.expect("second upsert");
    assert_eq!(
        first.id, second.id,
        "two principals for one human — the losing side's rows are unreachable"
    );

    let users = db
        .query("SELECT COUNT(*) FROM albedo_users", &[])
        .await
        .expect("count");
    assert_eq!(
        users.rows[0].get(0),
        Some(&SqlValue::Integer(1)),
        "the rolled-back loser left a user row behind"
    );
}

/// A returning human keeps their id — that is what makes it worth minting one.
#[tokio::test]
async fn a_returning_login_reuses_the_principal_and_refreshes_the_profile() {
    let db = booted().await;

    let first = upsert_principal(&db, "github", "gh-1", &profile("ada@example.com", "Ada"), NOW)
        .await
        .expect("first login");
    let renamed = upsert_principal(
        &db,
        "github",
        "gh-1",
        &profile("ada@newjob.example", "Ada Lovelace"),
        NOW + DAY_MS,
    )
    .await
    .expect("second login");

    assert_eq!(first.id, renamed.id);

    let token = create_session(&db, &renamed.id, "github", NOW + DAY_MS, 30 * DAY_MS)
        .await
        .expect("session");
    let seen = resolve_session(&db, &token, NOW + DAY_MS + 1)
        .await
        .expect("resolve");
    let seen = seen.principal().expect("resolves");
    assert_eq!(seen.name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(seen.email.as_deref(), Some("ada@newjob.example"));
}

/// One human, two providers, one principal — what a minted id buys that a
/// derived one could not say.
#[tokio::test]
async fn the_same_subject_at_two_providers_is_two_principals_until_linked() {
    let db = booted().await;

    let google = upsert_principal(&db, "google", "shared-subject", &ProviderProfile::default(), NOW)
        .await
        .expect("google");
    let github = upsert_principal(&db, "github", "shared-subject", &ProviderProfile::default(), NOW)
        .await
        .expect("github");

    assert_ne!(
        google.id, github.id,
        "one subject string at two providers is not evidence of one human"
    );
}

/// **§ 5 — instant global logout.** One `DELETE` over a partition, and every
/// device's token stops resolving.
#[tokio::test]
async fn revoking_everywhere_kills_every_device_at_once() {
    let db = booted().await;
    let principal = upsert_principal(&db, "passkey", "ada", &ProviderProfile::default(), NOW)
        .await
        .expect("upsert");

    let mut tokens = Vec::new();
    for _ in 0..3 {
        tokens.push(
            create_session(&db, &principal.id, "passkey", NOW, 30 * DAY_MS)
                .await
                .expect("session"),
        );
    }
    assert_eq!(sessions_for(&db, &principal.id, NOW).await.unwrap().len(), 3);

    let killed = revoke_all_sessions(&db, &principal.id)
        .await
        .expect("revoke all");
    assert_eq!(killed, 3);

    for token in &tokens {
        assert!(
            resolve_session(&db, token, NOW + 1)
                .await
                .expect("resolve")
                .principal()
                .is_none(),
            "a device survived a global logout"
        );
    }
}

/// Logging out one device must not log out the others.
#[tokio::test]
async fn revoking_one_session_leaves_the_others_alone() {
    let db = booted().await;
    let principal = upsert_principal(&db, "passkey", "ada", &ProviderProfile::default(), NOW)
        .await
        .expect("upsert");
    let phone = create_session(&db, &principal.id, "passkey", NOW, 30 * DAY_MS)
        .await
        .expect("phone");
    let laptop = create_session(&db, &principal.id, "passkey", NOW, 30 * DAY_MS)
        .await
        .expect("laptop");

    assert_eq!(revoke_session(&db, &phone.hash()).await.expect("revoke"), 1);

    assert!(resolve_session(&db, &phone, NOW + 1)
        .await
        .expect("resolve")
        .principal()
        .is_none());
    assert!(resolve_session(&db, &laptop, NOW + 1)
        .await
        .expect("resolve")
        .principal()
        .is_some());
}

/// Two principals must never see each other's sessions — the property that
/// makes everything downstream of `user.id` safe.
#[tokio::test]
async fn one_principals_token_never_resolves_to_another() {
    let db = booted().await;
    let (alice, alice_token) = login(&db, "passkey", "alice-key").await;
    let (mallory, _) = login(&db, "passkey", "mallory-key").await;
    assert_ne!(alice.id, mallory.id);

    let resolved = resolve_session(&db, &alice_token, NOW + 1)
        .await
        .expect("resolve");
    assert_eq!(resolved.principal().expect("resolves").id, alice.id);

    // Mallory's device list must be empty of Alice's sessions.
    let mallorys = sessions_for(&db, &mallory.id, NOW).await.expect("list");
    assert!(mallorys.iter().all(|s| s.principal == mallory.id));
}

/// A deleted account must not leave a usable session behind.
#[tokio::test]
async fn deleting_a_principal_takes_their_sessions_with_it() {
    let db = booted().await;
    let (principal, token) = login(&db, "passkey", "ada-authenticator").await;

    delete_principal(&db, &principal.id).await.expect("delete");

    let resolved = resolve_session(&db, &token, NOW + 1).await.expect("resolve");
    assert!(
        resolved.principal().is_none(),
        "a deleted account's session still resolves"
    );

    for table in schema::RESERVED_COLLECTIONS {
        let count = db
            .query(
                &format!("SELECT COUNT(*) FROM {table} WHERE principal = ?1"),
                &[SqlValue::Text(principal.id.as_str().to_string())],
            )
            .await
            .expect("count");
        assert_eq!(
            count.rows[0].get(0),
            Some(&SqlValue::Integer(0)),
            "`{table}` kept a row for a deleted principal"
        );
    }
}

/// A session whose user row vanished is reported distinctly in our logs, and
/// identically — as nobody — to the client.
#[tokio::test]
async fn an_orphaned_session_resolves_to_nobody() {
    let db = booted().await;
    let (principal, token) = login(&db, "passkey", "ada-authenticator").await;

    db.execute(
        "DELETE FROM albedo_users WHERE principal = ?1",
        &[SqlValue::Text(principal.id.as_str().to_string())],
    )
    .await
    .expect("delete user row only");

    let resolved = resolve_session(&db, &token, NOW + 1).await.expect("resolve");
    assert!(matches!(
        resolved,
        Resolved::Anonymous(SessionRejection::PrincipalGone)
    ));
}

/// Housekeeping, not enforcement — an unpurged expired session is already
/// unusable, so this only has to keep the table from growing.
#[tokio::test]
async fn purging_removes_only_expired_sessions() {
    let db = booted().await;
    let principal = upsert_principal(&db, "passkey", "ada", &ProviderProfile::default(), NOW)
        .await
        .expect("upsert");
    let short = create_session(&db, &principal.id, "passkey", NOW, DAY_MS)
        .await
        .expect("short");
    let long = create_session(&db, &principal.id, "passkey", NOW, 30 * DAY_MS)
        .await
        .expect("long");

    let purged = purge_expired_sessions(&db, NOW + 2 * DAY_MS)
        .await
        .expect("purge");
    assert_eq!(purged, 1);

    assert!(resolve_session(&db, &short, NOW + 2 * DAY_MS)
        .await
        .expect("resolve")
        .principal()
        .is_none());
    assert!(resolve_session(&db, &long, NOW + 2 * DAY_MS)
        .await
        .expect("resolve")
        .principal()
        .is_some());
}

/// A principal read back from the database must survive the alphabet check that
/// makes it usable as a topic namespace.
#[tokio::test]
async fn a_stored_principal_round_trips_through_the_partition_key_alphabet() {
    let db = booted().await;
    let (principal, _) = login(&db, "passkey", "ada-authenticator").await;

    let rows = db
        .query("SELECT principal FROM albedo_users", &[])
        .await
        .expect("read");
    let stored = rows.rows[0]
        .get(0)
        .and_then(SqlValue::as_str)
        .expect("principal column");

    assert_eq!(
        PrincipalId::parse(stored).expect("a stored id is a valid partition key"),
        principal.id
    );
}
