//! R1b (T-0026) · AC 1–5: the authority epoch and clock, at the store.
//!
//! Each test names the old behaviour it would have caught.

use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::session::{SessionToken, TokenHash};
use dom_render_compiler::auth::store::{
    authority_changes_after, authority_clock, create_session, delete_principal, grant_role,
    grant_role_changed, live_check, load_principal, resolve_session, revoke_all_sessions,
    revoke_role_changed, role_grants, unlink_account, upsert_principal, GrantOutcome,
    ProviderProfile, UnlinkOutcome,
};
use dom_render_compiler::auth::{email_token, Principal};
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

async fn person(db: &LibSqlSubstrate, who: &str) -> (Principal, SessionToken) {
    let profile = ProviderProfile {
        email: Some(format!("{who}@x.dev")),
        ..ProviderProfile::default()
    };
    let principal = upsert_principal(db, "password", who, &profile, NOW)
        .await
        .expect("upsert");
    let token = create_session(db, &principal.id, "password", NOW, 30 * DAY_MS)
        .await
        .expect("session");
    (principal, token)
}

async fn epoch_of(db: &LibSqlSubstrate, principal: &Principal) -> i64 {
    load_principal(db, &principal.id)
        .await
        .expect("load")
        .expect("exists")
        .authority_epoch
}

/// AC 1 · both tables are reserved and created by boot; the clock starts at 0.
#[tokio::test]
async fn boot_creates_both_tables_and_a_zero_clock() {
    let db = booted().await;
    assert!(schema::is_reserved(schema::AUTHORITY) && schema::is_reserved(schema::AUTHORITY_CLOCK));
    assert_eq!(authority_clock(&db).await.expect("clock"), 0);
}

/// AC 2 · a real grant / revoke bumps the epoch and allocates a fresh seq; an
/// idempotent one bumps nothing and keeps provenance. Old code: no epoch at all.
#[tokio::test]
async fn real_changes_bump_and_idempotent_ones_do_not() {
    let db = booted().await;
    let (ada, _) = person(&db, "ada").await;
    assert_eq!(epoch_of(&db, &ada).await, 0);

    let (outcome, epoch) = grant_role_changed(&db, &ada.id, "payroll", Some("cli"), NOW)
        .await
        .expect("grant");
    assert_eq!((outcome, epoch), (GrantOutcome::Granted, Some(1)));
    assert_eq!(authority_clock(&db).await.unwrap(), 1);

    let (outcome, epoch) =
        grant_role_changed(&db, &ada.id, "payroll", Some("someone-else"), NOW + 5)
            .await
            .expect("grant again");
    assert_eq!((outcome, epoch), (GrantOutcome::AlreadyHeld, None));
    assert_eq!(
        authority_clock(&db).await.unwrap(),
        1,
        "an idempotent grant allocated a seq"
    );
    let grants = role_grants(&db, Some("payroll")).await.unwrap();
    assert_eq!(
        grants[0].granted_by.as_deref(),
        Some("cli"),
        "provenance was overwritten"
    );
    assert_eq!(grants[0].granted_at, NOW);

    assert_eq!(
        revoke_role_changed(&db, &ada.id, "payroll").await.unwrap(),
        Some(2)
    );
    assert_eq!(
        revoke_role_changed(&db, &ada.id, "payroll").await.unwrap(),
        None
    );
    assert_eq!(authority_clock(&db).await.unwrap(), 2);
    assert_eq!(epoch_of(&db, &ada).await, 2);
}

/// AC 2 · a failure inside the transaction (the clock row gone) leaves roles,
/// epoch, seq and provenance as they were.
#[tokio::test]
async fn a_failed_bump_rolls_the_role_change_back() {
    let db = booted().await;
    let (ada, _) = person(&db, "ada").await;
    db.execute(&format!("DELETE FROM {}", schema::AUTHORITY_CLOCK), &[])
        .await
        .unwrap();
    assert!(grant_role(&db, &ada.id, "payroll", Some("cli"), NOW)
        .await
        .is_err());
    assert!(
        role_grants(&db, Some("payroll")).await.unwrap().is_empty(),
        "grant half-applied"
    );
    assert_eq!(epoch_of(&db, &ada).await, 0);
}

/// AC 3 · deletion-safe watermark: consume the highest seq, delete its
/// principal, change another — found. Old design (max(seq) over deletable
/// rows) would reuse the deleted seq and the sweep would skip the change.
#[tokio::test]
async fn deleting_a_principal_cannot_lower_the_high_water_mark() {
    let db = booted().await;
    let (ada, _) = person(&db, "ada").await;
    let (bob, _) = person(&db, "bob").await;
    grant_role(&db, &ada.id, "payroll", Some("cli"), NOW)
        .await
        .unwrap();
    let watermark = authority_clock(&db).await.unwrap();

    delete_principal(&db, &ada.id).await.expect("delete");
    // The deletion itself is a change the sweep sees (tombstone bump).
    let after_delete = authority_changes_after(&db, watermark).await.unwrap();
    assert_eq!(after_delete.len(), 1);
    assert_eq!(after_delete[0].principal, ada.id.as_str());

    // Even with every authority row removed by hand, the clock keeps counting.
    db.execute(&format!("DELETE FROM {}", schema::AUTHORITY), &[])
        .await
        .unwrap();
    let watermark = authority_clock(&db).await.unwrap();
    grant_role(&db, &bob.id, "admin", Some("cli"), NOW)
        .await
        .unwrap();
    grant_role(&db, &bob.id, "hr", Some("cli"), NOW)
        .await
        .unwrap();
    let changes = authority_changes_after(&db, watermark).await.unwrap();
    assert_eq!(changes.len(), 1, "one row per principal (coalesced)");
    assert_eq!(changes[0].principal, bob.id.as_str());
    assert_eq!(changes[0].seq, watermark + 2);
}

/// AC 4 · concurrent changes from independent connections (separate processes'
/// worth of substrates on one file) get unique seqs and correct final epochs.
#[tokio::test]
async fn concurrent_changes_on_independent_connections_get_unique_seqs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("forge.db");
    let first = LibSqlSubstrate::open_local(&path).await.unwrap();
    let schema = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
    bootstrap_schema(&first, &schema).await.unwrap();
    let mut people = Vec::new();
    for i in 0..4 {
        people.push(person(&first, &format!("p{i}")).await.0);
    }
    let mut tasks = Vec::new();
    for (i, who) in people.iter().enumerate() {
        let path = path.clone();
        let id = who.id.clone();
        tasks.push(tokio::spawn(async move {
            let db = LibSqlSubstrate::open_local(&path).await.unwrap();
            for role in ["a", "b", "c"] {
                let mut tries = 0;
                loop {
                    match grant_role(&db, &id, role, Some("cli"), NOW).await {
                        Ok(_) => break,
                        // A busy database fails visibly; the caller retries.
                        Err(err) if tries < 50 => {
                            tries += 1;
                            let _ = err;
                            tokio::time::sleep(std::time::Duration::from_millis(5 + i as u64))
                                .await;
                        }
                        Err(err) => panic!("grant never succeeded: {err}"),
                    }
                }
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let changes = authority_changes_after(&first, 0).await.unwrap();
    assert_eq!(changes.len(), 4);
    let mut seqs: Vec<i64> = changes.iter().map(|c| c.seq).collect();
    seqs.dedup();
    assert_eq!(seqs.len(), 4, "a seq was reused");
    assert_eq!(authority_clock(&first).await.unwrap(), 12);
    for who in &people {
        assert_eq!(epoch_of(&first, who).await, 3);
    }
}

/// AC 5 · resolution carries epoch (both paths) and expiry (session path only).
#[tokio::test]
async fn resolution_carries_epoch_and_only_sessions_carry_expiry() {
    let db = booted().await;
    let (ada, token) = person(&db, "ada").await;
    grant_role(&db, &ada.id, "payroll", Some("cli"), NOW)
        .await
        .unwrap();
    let resolved = resolve_session(&db, &token, NOW + 1).await.unwrap();
    let principal = resolved.principal().expect("live");
    assert_eq!(principal.authority_epoch, 1);
    assert_eq!(principal.session_expires_at, Some(NOW + 30 * DAY_MS));
    assert!(
        !principal.user_value().to_string().contains("epoch"),
        "the epoch leaked into `user`"
    );

    let loaded = load_principal(&db, &ada.id).await.unwrap().unwrap();
    assert_eq!(loaded.authority_epoch, 1);
    assert_eq!(
        loaded.session_expires_at, None,
        "a sessionless principal has no expiry"
    );
}

/// The re-validation query: present + unexpired + epoch.
#[tokio::test]
async fn live_check_reads_session_presence_expiry_and_epoch() {
    let db = booted().await;
    let (ada, token) = person(&db, "ada").await;
    let check = live_check(&db, &token.hash(), &ada.id, NOW + 1)
        .await
        .unwrap();
    assert!(check.session_live);
    assert_eq!(check.epoch, 0);
    let expired = live_check(&db, &token.hash(), &ada.id, NOW + 31 * DAY_MS)
        .await
        .unwrap();
    assert!(!expired.session_live);
    let unknown = live_check(&db, &TokenHash::of("nope"), &ada.id, NOW + 1)
        .await
        .unwrap();
    assert!(!unknown.session_live);
}

/// Session ends return exactly what they deleted — never a later session.
#[tokio::test]
async fn session_ending_paths_return_exactly_the_deleted_hashes() {
    let db = booted().await;
    let (ada, laptop) = person(&db, "ada").await;
    let phone = create_session(&db, &ada.id, "password", NOW, DAY_MS)
        .await
        .unwrap();
    let mut deleted = revoke_all_sessions(&db, &ada.id).await.unwrap();
    deleted.sort();
    let mut expected = vec![laptop.hash(), phone.hash()];
    expected.sort();
    assert_eq!(deleted, expected);

    // A session minted after that commit is not in any earlier set.
    let later = create_session(&db, &ada.id, "password", NOW, DAY_MS)
        .await
        .unwrap();
    assert!(!deleted.contains(&later.hash()));

    // Unlink: the other sessions of that provider, not the initiating one.
    let google = upsert_principal(&db, "google", "g-ada", &ProviderProfile::default(), NOW)
        .await
        .unwrap();
    let _ = google;
    let (bob, bob_pw) = person(&db, "bob").await;
    db.execute(
        "INSERT INTO albedo_accounts (principal, provider, subject, created_at) VALUES (?1, 'google', 'g-bob', ?2)",
        &[SqlValue::Text(bob.id.as_str().to_string()), SqlValue::Integer(NOW)],
    )
    .await
    .unwrap();
    let g1 = create_session(&db, &bob.id, "google", NOW, DAY_MS)
        .await
        .unwrap();
    let g2 = create_session(&db, &bob.id, "google", NOW, DAY_MS)
        .await
        .unwrap();
    let outcome = unlink_account(&db, &bob.id, "google", &g1.hash())
        .await
        .unwrap();
    assert_eq!(
        outcome,
        UnlinkOutcome::Unlinked {
            sessions_ended: 1,
            ended: vec![g2.hash()],
        }
    );
    let _ = bob_pw;
}

/// Round 5 · magic-link takeover returns the deleted hashes even when the
/// claimed principal is the squatter's (and `took_over` is set).
#[tokio::test]
async fn magic_link_takeover_returns_the_deleted_hashes() {
    let db = booted().await;
    for statement in email_token::ddl() {
        db.migrate(&statement).await.unwrap();
    }
    let (squatter, squat_session) = person(&db, "victim").await;
    let linked = email_token::resolve_magic_link(&db, "magiclink", Some("password"), "victim", NOW)
        .await
        .expect("resolve");
    assert_eq!(linked.principal, squatter.id);
    assert!(linked.took_over);
    assert_eq!(linked.ended, vec![squat_session.hash()]);
}
