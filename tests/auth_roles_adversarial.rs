//! Adversarial R1a storage coverage: a large role set survives a database
//! restart, resolves identically for requests and jobs, and cannot duplicate.
#![cfg(feature = "forge")]

use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::store::{
    create_session, grant_role, load_principal, resolve_session, upsert_principal, GrantOutcome,
    ProviderProfile,
};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

#[tokio::test]
async fn hundreds_of_roles_survive_restart_without_truncation_or_duplicates() {
    const NOW: i64 = 1_754_400_000_000;
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("roles.db");
    let expected: Vec<_> = (0..256).map(|i| format!("role_{i:04}")).collect();
    let (id, token) = {
        let db = LibSqlSubstrate::open_local(&path).await.expect("open");
        let tables =
            schema::augment(&ForgeSchema::build(Vec::new()).expect("schema")).expect("auth tables");
        bootstrap_schema(&db, &tables).await.expect("bootstrap");
        let profile = ProviderProfile {
            email: Some("many@example.com".into()),
            name: None,
            image: None,
            claims: serde_json::Map::new(),
        };
        let principal = upsert_principal(&db, "password", "many", &profile, NOW)
            .await
            .expect("principal");
        let token = create_session(&db, &principal.id, "password", NOW, 60_000)
            .await
            .expect("session");
        for role in expected.iter().rev() {
            assert_eq!(
                grant_role(&db, &principal.id, role, Some("cli"), NOW)
                    .await
                    .expect("grant"),
                GrantOutcome::Granted
            );
        }
        // Exercise the actual database index, rather than inspecting its DDL.
        assert!(
            db.execute(
                "INSERT INTO albedo_roles (principal, role, granted_at) VALUES (?1, ?2, ?3)",
                &[
                    SqlValue::Text(principal.id.as_str().into()),
                    SqlValue::Text(expected[0].clone()),
                    SqlValue::Integer(NOW)
                ],
            )
            .await
            .is_err(),
            "a duplicate role row must fail at storage"
        );
        (principal.id, token)
    };
    let db = LibSqlSubstrate::open_local(&path).await.expect("reopen");
    let session = resolve_session(&db, &token, NOW + 1)
        .await
        .expect("resolve");
    assert_eq!(session.principal().expect("live").roles, expected);
    let job = load_principal(&db, &id)
        .await
        .expect("job load")
        .expect("exists");
    assert_eq!(job.roles, expected);
    assert_eq!(job.user_value()["roles"], serde_json::json!(expected));
}

async fn bootstrap_person(path: &std::path::Path) -> dom_render_compiler::auth::PrincipalId {
    let db = LibSqlSubstrate::open_local(path).await.expect("open");
    let tables =
        schema::augment(&ForgeSchema::build(Vec::new()).expect("schema")).expect("auth tables");
    bootstrap_schema(&db, &tables).await.expect("bootstrap");
    let profile = ProviderProfile {
        email: Some("failure@example.com".into()),
        name: None,
        image: None,
        claims: serde_json::Map::new(),
    };
    upsert_principal(&db, "password", "failure", &profile, 1000)
        .await
        .expect("principal")
        .id
}

#[tokio::test]
async fn storage_failure_rolls_back_the_grant_and_allows_a_retry() {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("failure.db");
    let id = bootstrap_person(&path).await;
    let db = LibSqlSubstrate::open_local(&path).await.expect("open");
    db.execute("CREATE TRIGGER reject_role BEFORE INSERT ON albedo_roles BEGIN SELECT RAISE(ABORT, 'role storage refused'); END", &[])
        .await.expect("inject deterministic storage error");
    let error = grant_role(&db, &id, "payroll", Some("cli"), 1001)
        .await
        .expect_err("storage failure must reach caller");
    assert!(
        error.to_string().contains("role storage refused"),
        "{error}"
    );
    assert!(load_principal(&db, &id)
        .await
        .expect("load")
        .expect("exists")
        .roles
        .is_empty());
    db.execute("DROP TRIGGER reject_role", &[])
        .await
        .expect("remove failure");
    assert_eq!(
        grant_role(&db, &id, "payroll", Some("cli"), 1002)
            .await
            .expect("retry"),
        GrantOutcome::Granted
    );
    assert_eq!(
        load_principal(&db, &id)
            .await
            .expect("load")
            .expect("exists")
            .roles,
        ["payroll"]
    );
}

struct RoleChild(std::process::Child);
impl Drop for RoleChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn role_transaction_process_child() {
    let Some(path) = std::env::var_os("ALBEDO_ROLE_CRASH_DB") else {
        return;
    };
    let ready = std::env::var_os("ALBEDO_ROLE_CRASH_READY").expect("ready path");
    let id = std::env::var("ALBEDO_ROLE_CRASH_ID").expect("principal");
    let db = LibSqlSubstrate::open_local(std::path::Path::new(&path))
        .await
        .expect("open");
    let tx = db.begin().await.expect("transaction");
    tx.execute(
        "INSERT INTO albedo_roles (principal, role, granted_at) VALUES (?1, 'payroll', 1001)",
        &[SqlValue::Text(id)],
    )
    .await
    .expect("uncommitted grant");
    std::fs::write(ready, b"inserted").expect("signal insert");
    std::future::pending::<()>().await;
    tx.commit().await.expect("commit");
}

#[tokio::test]
async fn killed_role_writer_leaves_no_uncommitted_grant_after_restart() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("crash.db");
    let ready = dir.path().join("ready");
    let id = bootstrap_person(&path).await;
    let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "role_transaction_process_child", "--nocapture"])
        .env("ALBEDO_ROLE_CRASH_DB", &path)
        .env("ALBEDO_ROLE_CRASH_READY", &ready)
        .env("ALBEDO_ROLE_CRASH_ID", id.as_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("child writer");
    let mut child = RoleChild(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() {
        assert!(
            child.0.try_wait().expect("child status").is_none(),
            "writer exited before insert"
        );
        assert!(Instant::now() < deadline, "writer did not reach insert");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child.0.kill().expect("kill writer");
    assert!(
        !child.0.wait().expect("wait for writer").success(),
        "writer must actually die"
    );
    let db = LibSqlSubstrate::open_local(&path)
        .await
        .expect("reopen after crash");
    assert!(load_principal(&db, &id)
        .await
        .expect("load")
        .expect("principal survives")
        .roles
        .is_empty());
    assert_eq!(
        grant_role(&db, &id, "payroll", Some("cli"), 1002)
            .await
            .expect("retry after crash"),
        GrantOutcome::Granted
    );
}
