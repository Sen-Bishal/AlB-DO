//! Store-boundary adversarial checks for T-0026.

use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::store::{self, GrantOutcome, ProviderProfile};
use dom_render_compiler::auth::PrincipalId;
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

const NOW: i64 = 1_754_400_000_000;

async fn bootstrap(db: &LibSqlSubstrate) {
    let schema = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
    bootstrap_schema(db, &schema).await.unwrap();
}

#[tokio::test]
async fn concurrent_duplicate_grants_and_revokes_only_allocate_one_change_each() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("contended authority.db");
    let db = LibSqlSubstrate::open_local(&path).await.unwrap();
    bootstrap(&db).await;
    let principal = store::upsert_principal(
        &db,
        "password",
        "same-person",
        &ProviderProfile::default(),
        NOW,
    )
    .await
    .unwrap();
    for revoke in [false, true] {
        let mut tasks = Vec::new();
        for worker in 0..8 {
            let path = path.clone();
            let principal = principal.id.clone();
            tasks.push(tokio::spawn(async move {
                let connection = LibSqlSubstrate::open_local(path).await.unwrap();
                for attempt in 0..100 {
                    let outcome = if revoke {
                        store::revoke_role_changed(&connection, &principal, "payroll")
                            .await
                            .map(|epoch| epoch.is_some())
                    } else {
                        store::grant_role_changed(
                            &connection,
                            &principal,
                            "payroll",
                            Some(&format!("worker-{worker}")),
                            NOW + worker,
                        )
                        .await
                        .map(|(outcome, epoch)| {
                            assert_eq!(epoch.is_some(), outcome == GrantOutcome::Granted);
                            outcome == GrantOutcome::Granted
                        })
                    };
                    match outcome {
                        Ok(changed) => return changed,
                        Err(error) if attempt == 99 => panic!("contention never cleared: {error}"),
                        Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
                    }
                }
                unreachable!()
            }));
        }
        let mut changes = 0;
        for task in tasks {
            changes += usize::from(task.await.unwrap());
        }
        assert_eq!(changes, 1, "duplicate operation changed authority twice");
        let expected = if revoke { 2 } else { 1 };
        assert_eq!(store::authority_clock(&db).await.unwrap(), expected);
        assert_eq!(
            store::load_principal(&db, &principal.id)
                .await
                .unwrap()
                .unwrap()
                .authority_epoch,
            expected
        );
        if !revoke {
            let grants = store::role_grants(&db, Some("payroll")).await.unwrap();
            assert_eq!(grants.len(), 1);
            let winner: i64 = grants[0]
                .granted_by
                .as_deref()
                .unwrap()
                .strip_prefix("worker-")
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(grants[0].granted_at, NOW + winner);
        }
    }
}

#[tokio::test]
async fn live_check_rejects_another_principals_token_and_the_exact_expiry_boundary() {
    let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
    bootstrap(&db).await;
    let ada = store::upsert_principal(&db, "password", "ada", &ProviderProfile::default(), NOW)
        .await
        .unwrap();
    let bob = store::upsert_principal(&db, "password", "bob", &ProviderProfile::default(), NOW)
        .await
        .unwrap();
    let token = store::create_session(&db, &ada.id, "password", NOW, 1_000)
        .await
        .unwrap();
    store::grant_role(&db, &bob.id, "payroll", Some("cli"), NOW)
        .await
        .unwrap();
    let wrong_owner = store::live_check(&db, &token.hash(), &bob.id, NOW)
        .await
        .unwrap();
    assert!(!wrong_owner.session_live);
    assert_eq!(wrong_owner.epoch, 1);
    assert!(
        store::live_check(&db, &token.hash(), &ada.id, NOW + 999)
            .await
            .unwrap()
            .session_live
    );
    assert!(
        !store::live_check(&db, &token.hash(), &ada.id, NOW + 1_000)
            .await
            .unwrap()
            .session_live
    );
    assert!(store::resolve_session(&db, &token, NOW + 1_000)
        .await
        .unwrap()
        .principal()
        .is_none());
}

#[tokio::test]
async fn saturated_clock_or_epoch_fails_without_half_applying_a_role() {
    for saturate_clock in [false, true] {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap(&db).await;
        let principal =
            store::upsert_principal(&db, "password", "ada", &ProviderProfile::default(), NOW)
                .await
                .unwrap();
        store::grant_role(&db, &principal.id, "existing", Some("original"), NOW)
            .await
            .unwrap();
        if saturate_clock {
            db.execute(
                "UPDATE albedo_authority_clock SET seq = ?1 WHERE id = 1",
                &[SqlValue::Integer(i64::MAX)],
            )
            .await
            .unwrap();
        } else {
            db.execute(
                "UPDATE albedo_authority SET epoch = ?1 WHERE principal = ?2",
                &[
                    SqlValue::Integer(i64::MAX),
                    SqlValue::Text(principal.id.as_str().to_string()),
                ],
            )
            .await
            .unwrap();
        }
        let before_clock = db
            .query("SELECT * FROM albedo_authority_clock", &[])
            .await
            .unwrap();
        let before_authority = db
            .query("SELECT * FROM albedo_authority", &[])
            .await
            .unwrap();
        assert!(
            store::grant_role(&db, &principal.id, "new", Some("new"), NOW + 1)
                .await
                .is_err()
        );
        assert_eq!(
            db.query("SELECT * FROM albedo_authority_clock", &[])
                .await
                .unwrap(),
            before_clock
        );
        assert_eq!(
            db.query("SELECT * FROM albedo_authority", &[])
                .await
                .unwrap(),
            before_authority
        );
        assert!(store::role_grants(&db, Some("new"))
            .await
            .unwrap()
            .is_empty());
        let existing = store::role_grants(&db, Some("existing")).await.unwrap();
        assert_eq!(existing[0].granted_by.as_deref(), Some("original"));
        assert_eq!(existing[0].granted_at, NOW);
    }
}

#[tokio::test]
async fn process_exit_before_commit_leaves_no_role_or_authority_change_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("crash authority.db");
    let db = LibSqlSubstrate::open_local(&path).await.unwrap();
    bootstrap(&db).await;
    let principal =
        store::upsert_principal(&db, "password", "ada", &ProviderProfile::default(), NOW)
            .await
            .unwrap();
    store::grant_role(&db, &principal.id, "existing", Some("original"), NOW)
        .await
        .unwrap();
    drop(db);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "authority_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("ALBEDO_AUTHORITY_REVIEW_CRASH_DB", &path)
        .env(
            "ALBEDO_AUTHORITY_REVIEW_CRASH_PRINCIPAL",
            principal.id.as_str(),
        )
        .status()
        .unwrap();
    assert_eq!(
        status.code(),
        Some(73),
        "child did not reach the transaction exit"
    );
    let reopened = LibSqlSubstrate::open_local(&path).await.unwrap();
    assert_eq!(store::authority_clock(&reopened).await.unwrap(), 1);
    assert_eq!(
        store::load_principal(&reopened, &principal.id)
            .await
            .unwrap()
            .unwrap()
            .authority_epoch,
        1
    );
    assert!(store::role_grants(&reopened, Some("uncommitted"))
        .await
        .unwrap()
        .is_empty());
    let original = store::role_grants(&reopened, Some("existing"))
        .await
        .unwrap();
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].granted_by.as_deref(), Some("original"));
    store::grant_role(
        &reopened,
        &principal.id,
        "after-restart",
        Some("cli"),
        NOW + 2,
    )
    .await
    .unwrap();
    assert_eq!(store::authority_clock(&reopened).await.unwrap(), 2);
}

#[tokio::test]
#[ignore = "subprocess helper invoked by the crash-recovery test"]
async fn authority_crash_child() {
    let path = std::env::var_os("ALBEDO_AUTHORITY_REVIEW_CRASH_DB").expect("child database");
    let principal = PrincipalId::parse(
        std::env::var("ALBEDO_AUTHORITY_REVIEW_CRASH_PRINCIPAL").expect("child principal"),
    )
    .unwrap();
    let db = LibSqlSubstrate::open_local(std::path::PathBuf::from(path))
        .await
        .unwrap();
    let transaction = db.begin().await.unwrap();
    let result = store::grant_role_in_transaction(
        transaction.as_ref(),
        &principal,
        "uncommitted",
        Some("child"),
        NOW + 1,
    )
    .await
    .unwrap();
    assert_eq!(result, (GrantOutcome::Granted, Some(2)));
    // Exit while the transaction is alive: neither rollback nor Drop runs.
    std::process::exit(73);
}
