//! R1a role lifecycle failures and isolation, over one substrate per test.
#![cfg(feature = "forge")]

use dom_render_compiler::auth::session::SessionToken;
use dom_render_compiler::auth::store::{
    create_session, delete_principal, grant_role, load_principal, resolve_session, revoke_role,
    role_grants, upsert_principal, GrantOutcome, ProviderProfile,
};
use dom_render_compiler::auth::{schema, Principal};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

const NOW: i64 = 1_754_400_000_000;

async fn booted() -> LibSqlSubstrate {
    let db = LibSqlSubstrate::open_ephemeral().await.expect("open");
    let tables =
        schema::augment(&ForgeSchema::build(Vec::new()).expect("schema")).expect("auth schema");
    bootstrap_schema(&db, &tables).await.expect("bootstrap");
    db
}

async fn person(db: &LibSqlSubstrate, subject: &str) -> (Principal, SessionToken) {
    let profile = ProviderProfile {
        email: Some(format!("{subject}@example.com")),
        name: None,
        image: None,
        claims: Default::default(),
    };
    let principal = upsert_principal(db, "password", subject, &profile, NOW)
        .await
        .expect("person");
    let token = create_session(db, &principal.id, "password", NOW, 60_000)
        .await
        .expect("session");
    (principal, token)
}

async fn resolved(db: &LibSqlSubstrate, token: &SessionToken) -> Principal {
    resolve_session(db, token, NOW + 1)
        .await
        .expect("resolve")
        .principal()
        .expect("live session")
        .clone()
}

#[tokio::test]
async fn role_delete_failure_rolls_back_account_and_session_erasure() {
    let db = booted().await;
    let (ada, token) = person(&db, "ada").await;
    grant_role(&db, &ada.id, "payroll", Some("cli"), NOW)
        .await
        .expect("grant");
    // Roles are deleted after sessions and accounts. Failure here must also
    // restore those earlier deletes, rather than leave a half-erased person.
    db.execute(
        "CREATE TRIGGER refuse_role_delete BEFORE DELETE ON albedo_roles \
         BEGIN SELECT RAISE(ABORT, 'role erasure refused'); END",
        &[],
    )
    .await
    .expect("inject deletion failure");
    let error = delete_principal(&db, &ada.id)
        .await
        .expect_err("erasure failure must reach caller");
    assert!(
        error.to_string().contains("role erasure refused"),
        "{error}"
    );
    let restored = resolved(&db, &token).await;
    assert_eq!(restored.id, ada.id);
    assert_eq!(restored.providers, ["password"]);
    assert_eq!(restored.roles, ["payroll"]);
    assert_eq!(role_grants(&db, None).await.expect("grants").len(), 1);

    db.execute("DROP TRIGGER refuse_role_delete", &[])
        .await
        .expect("remove failure");
    delete_principal(&db, &ada.id).await.expect("retry erase");
    assert!(load_principal(&db, &ada.id).await.expect("load").is_none());
    assert!(resolve_session(&db, &token, NOW + 1)
        .await
        .expect("old session")
        .principal()
        .is_none());
    assert!(role_grants(&db, None).await.expect("grants").is_empty());
}

#[tokio::test]
async fn revocation_and_erasure_do_not_touch_another_holders_same_role() {
    let db = booted().await;
    let (ada, ada_token) = person(&db, "ada").await;
    let (bob, bob_token) = person(&db, "bob").await;
    for id in [&ada.id, &bob.id] {
        grant_role(&db, id, "payroll", Some("cli"), NOW)
            .await
            .expect("grant");
    }
    grant_role(&db, &ada.id, "hr", Some("cli"), NOW)
        .await
        .expect("second role");
    assert!(revoke_role(&db, &ada.id, "payroll").await.expect("revoke"));
    assert_eq!(resolved(&db, &ada_token).await.roles, ["hr"]);
    assert_eq!(resolved(&db, &bob_token).await.roles, ["payroll"]);
    delete_principal(&db, &ada.id).await.expect("erase ada");
    assert_eq!(resolved(&db, &bob_token).await.roles, ["payroll"]);
    let grants = role_grants(&db, None).await.expect("grants");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].principal, bob.id);
    assert_eq!(grants[0].role, "payroll");
}

#[tokio::test]
async fn a_repeated_grant_preserves_the_original_provenance_and_timestamp() {
    let db = booted().await;
    let (ada, _) = person(&db, "ada").await;
    assert_eq!(
        grant_role(&db, &ada.id, "payroll", Some("first-operator"), NOW)
            .await
            .expect("initial grant"),
        GrantOutcome::Granted
    );
    assert_eq!(
        grant_role(&db, &ada.id, "payroll", Some("second-operator"), NOW + 999)
            .await
            .expect("repeat grant"),
        GrantOutcome::AlreadyHeld
    );
    let grants = role_grants(&db, Some("payroll")).await.expect("grants");
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].granted_by.as_deref(), Some("first-operator"));
    assert_eq!(grants[0].granted_at, NOW);
}

#[tokio::test]
async fn removing_and_redeclaring_a_role_filters_values_without_destroying_grants() {
    let db = booted().await;
    let (ada, token) = person(&db, "ada").await;
    for role in ["payroll", "hr"] {
        grant_role(&db, &ada.id, role, Some("cli"), NOW)
            .await
            .expect("grant");
    }
    let original = role_grants(&db, None).await.expect("stored grants");
    for declared in [
        vec!["hr".to_string()],
        vec![],
        vec!["hr".into(), "payroll".into()],
    ] {
        let mut request_user = resolved(&db, &token).await;
        let mut job_user = load_principal(&db, &ada.id)
            .await
            .expect("load")
            .expect("exists");
        request_user.retain_declared_roles(&declared);
        job_user.retain_declared_roles(&declared);
        assert_eq!(request_user.roles, declared);
        assert_eq!(job_user.user_value()["roles"], serde_json::json!(declared));
        assert_eq!(
            role_grants(&db, None).await.expect("retained grants"),
            original
        );
    }
}

#[tokio::test]
async fn provider_role_claims_never_become_first_party_grants() {
    let db = booted().await;
    let profile = ProviderProfile {
        email: Some("spoof@example.com".into()),
        name: None,
        image: None,
        claims: serde_json::from_value(serde_json::json!({
            "roles": ["admin", "payroll"], "role": "admin", "user": { "roles": ["admin"] }
        }))
        .expect("claims object"),
    };
    let created = upsert_principal(&db, "password", "spoof", &profile, NOW)
        .await
        .expect("person");
    assert_eq!(
        created.claims["roles"],
        serde_json::json!(["admin", "payroll"])
    );
    assert_eq!(created.user_value()["roles"], serde_json::json!([]));
    let token = create_session(&db, &created.id, "password", NOW, 60_000)
        .await
        .expect("session");
    assert!(resolved(&db, &token).await.roles.is_empty());
    assert!(load_principal(&db, &created.id)
        .await
        .expect("load")
        .expect("exists")
        .roles
        .is_empty());
    grant_role(&db, &created.id, "hr", Some("cli"), NOW)
        .await
        .expect("real grant");
    assert_eq!(resolved(&db, &token).await.roles, ["hr"]);
    assert_eq!(role_grants(&db, None).await.expect("grants").len(), 1);
}
