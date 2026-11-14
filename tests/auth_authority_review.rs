//! Transaction-boundary falsifiers for T-0026.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::store::{self, ProviderProfile};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::substrate::Transaction;
use dom_render_compiler::forge::value::{Result, Rows, SqlValue, SubstrateError};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

const NOW: i64 = 1_754_400_000_000;

async fn booted() -> Arc<LibSqlSubstrate> {
    let db = Arc::new(LibSqlSubstrate::open_ephemeral().await.unwrap());
    let schema = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
    bootstrap_schema(db.as_ref(), &schema).await.unwrap();
    db
}

fn injected() -> SubstrateError {
    SubstrateError::Backend("injected authority transaction failure".into())
}

/// Fail one selected statement, or reject the commit before it becomes durable.
struct FailingStore {
    db: Arc<LibSqlSubstrate>,
    statement: Option<&'static str>,
    reached: Arc<AtomicBool>,
}

struct FailingTransaction {
    inner: Box<dyn Transaction>,
    statement: Option<&'static str>,
    reached: Arc<AtomicBool>,
}

#[async_trait]
impl DataSubstrate for FailingStore {
    async fn migrate(&self, ddl: &str) -> Result<()> {
        self.db.migrate(ddl).await
    }
    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows> {
        self.db.query(sql, params).await
    }
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        self.db.execute(sql, params).await
    }
    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        Ok(Box::new(FailingTransaction {
            inner: self.db.begin().await?,
            statement: self.statement,
            reached: self.reached.clone(),
        }))
    }
}

#[async_trait]
impl Transaction for FailingTransaction {
    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows> {
        self.inner.query(sql, params).await
    }
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        if self.statement.is_some_and(|prefix| sql.starts_with(prefix)) {
            self.reached.store(true, Ordering::Relaxed);
            return Err(injected());
        }
        self.inner.execute(sql, params).await
    }
    async fn commit(self: Box<Self>) -> Result<()> {
        if self.statement.is_none() {
            self.reached.store(true, Ordering::Relaxed);
            self.inner.rollback().await?;
            return Err(injected());
        }
        self.inner.commit().await
    }
    async fn rollback(self: Box<Self>) -> Result<()> {
        self.inner.rollback().await
    }
}

async fn snapshot(db: &dyn DataSubstrate) -> Vec<Rows> {
    let mut rows = Vec::new();
    for table in [
        schema::USERS,
        schema::ACCOUNTS,
        schema::SESSIONS,
        schema::ROLES,
        schema::AUTHORITY,
        schema::AUTHORITY_CLOCK,
    ] {
        rows.push(
            db.query(&format!("SELECT * FROM {table} ORDER BY 1"), &[])
                .await
                .unwrap(),
        );
    }
    rows
}

#[tokio::test]
async fn grant_and_revoke_failures_preserve_roles_epochs_clock_and_provenance() {
    for revoke in [false, true] {
        for statement in [
            Some(if revoke {
                "DELETE FROM albedo_roles"
            } else {
                "INSERT OR IGNORE INTO albedo_roles"
            }),
            Some("UPDATE albedo_authority_clock"),
            Some("INSERT INTO albedo_authority "),
            None,
        ] {
            let db = booted().await;
            let principal = store::upsert_principal(
                db.as_ref(),
                "password",
                "ada",
                &ProviderProfile::default(),
                NOW,
            )
            .await
            .unwrap();
            // Preserve an existing epoch and provenance, rather than only
            // checking rollback against an entirely empty authority table.
            store::grant_role(
                db.as_ref(),
                &principal.id,
                "existing",
                Some("original"),
                NOW,
            )
            .await
            .unwrap();
            if revoke {
                store::grant_role(db.as_ref(), &principal.id, "payroll", Some("original"), NOW)
                    .await
                    .unwrap();
            }
            let before = snapshot(db.as_ref()).await;
            let reached = Arc::new(AtomicBool::new(false));
            let failing = FailingStore {
                db: db.clone(),
                statement,
                reached: reached.clone(),
            };
            let result = if revoke {
                store::revoke_role_changed(&failing, &principal.id, "payroll")
                    .await
                    .map(|_| ())
            } else {
                store::grant_role_changed(&failing, &principal.id, "payroll", Some("new"), NOW + 99)
                    .await
                    .map(|_| ())
            };
            assert!(
                result.is_err(),
                "operation unexpectedly succeeded: {statement:?}"
            );
            assert!(
                reached.load(Ordering::Relaxed),
                "fault not exercised: {statement:?}"
            );
            assert_eq!(
                snapshot(db.as_ref()).await,
                before,
                "partial write: revoke={revoke}, {statement:?}"
            );
        }
    }
}

#[tokio::test]
async fn failed_tombstone_or_commit_keeps_principal_sessions_and_roles() {
    for statement in [
        Some("UPDATE albedo_authority_clock"),
        Some("INSERT INTO albedo_authority "),
        Some("DELETE FROM albedo_users"),
        None,
    ] {
        let db = booted().await;
        let principal = store::upsert_principal(
            db.as_ref(),
            "password",
            "ada",
            &ProviderProfile::default(),
            NOW,
        )
        .await
        .unwrap();
        let token = store::create_session(db.as_ref(), &principal.id, "password", NOW, 60_000)
            .await
            .unwrap();
        store::grant_role(db.as_ref(), &principal.id, "payroll", Some("original"), NOW)
            .await
            .unwrap();
        let before = snapshot(db.as_ref()).await;
        let reached = Arc::new(AtomicBool::new(false));
        let failing = FailingStore {
            db: db.clone(),
            statement,
            reached: reached.clone(),
        };
        assert!(store::delete_principal(&failing, &principal.id)
            .await
            .is_err());
        assert!(
            reached.load(Ordering::Relaxed),
            "fault not exercised: {statement:?}"
        );
        assert_eq!(
            snapshot(db.as_ref()).await,
            before,
            "partial deletion: {statement:?}"
        );
        assert!(store::resolve_session(db.as_ref(), &token, NOW + 1)
            .await
            .unwrap()
            .principal()
            .is_some());
    }
}

#[tokio::test]
async fn sweep_change_read_uses_seq_index_without_a_sort_or_table_scan() {
    let db = booted().await;
    let plan = db.query(
        "EXPLAIN QUERY PLAN SELECT principal, epoch, seq FROM albedo_authority WHERE seq > ?1 ORDER BY seq",
        &[SqlValue::Integer(17)],
    ).await.unwrap();
    let details: Vec<_> = plan
        .rows
        .iter()
        .filter_map(|row| row.get(3).and_then(SqlValue::as_str))
        .collect();
    assert!(
        details.iter().any(
            |detail| detail.contains("SEARCH albedo_authority USING INDEX")
                && detail.contains("seq>?")
        ),
        "{details:?}"
    );
    assert!(
        !details.iter().any(
            |detail| detail.contains("SCAN albedo_authority") || detail.contains("TEMP B-TREE")
        ),
        "{details:?}"
    );
}

#[tokio::test]
async fn a_backend_commit_constraint_failure_rolls_the_authority_change_back() {
    for revoke in [false, true] {
        let db = booted().await;
        let principal = store::upsert_principal(
            db.as_ref(),
            "password",
            "ada",
            &ProviderProfile::default(),
            NOW,
        )
        .await
        .unwrap();
        if revoke {
            store::grant_role(db.as_ref(), &principal.id, "payroll", Some("original"), NOW)
                .await
                .unwrap();
        }
        db.execute("PRAGMA foreign_keys = ON", &[]).await.unwrap();
        db.migrate("CREATE TABLE commit_parent (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
        db.migrate("CREATE TABLE commit_child (parent INTEGER REFERENCES commit_parent(id) DEFERRABLE INITIALLY DEFERRED)")
            .await
            .unwrap();
        db.migrate(&format!(
            "CREATE TRIGGER reject_role_commit AFTER {} ON albedo_roles BEGIN INSERT INTO commit_child VALUES (17); END",
            if revoke { "DELETE" } else { "INSERT" },
        ))
        .await
        .unwrap();
        let before = snapshot(db.as_ref()).await;
        let result = if revoke {
            store::revoke_role_changed(db.as_ref(), &principal.id, "payroll")
                .await
                .map(|_| ())
        } else {
            store::grant_role_changed(db.as_ref(), &principal.id, "payroll", Some("new"), NOW)
                .await
                .map(|_| ())
        };
        let error = result.expect_err("deferred foreign key must reject the outer commit");
        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
        assert_eq!(snapshot(db.as_ref()).await, before, "revoke={revoke}");
        let children = db.query("SELECT * FROM commit_child", &[]).await.unwrap();
        assert!(
            children.rows.is_empty(),
            "failed transaction retained its trigger write"
        );
    }
}

#[tokio::test]
async fn dropping_an_uncommitted_grant_and_reopening_preserves_clock_and_schema() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("authority with spaces 日本語.db");
    let schema = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
    let db = LibSqlSubstrate::open_local(&path).await.unwrap();
    bootstrap_schema(&db, &schema).await.unwrap();
    let principal =
        store::upsert_principal(&db, "password", "ada", &ProviderProfile::default(), NOW)
            .await
            .unwrap();
    store::grant_role(&db, &principal.id, "existing", Some("original"), NOW)
        .await
        .unwrap();
    let before = snapshot(&db).await;
    let transaction = db.begin().await.unwrap();
    let changed = store::grant_role_in_transaction(
        transaction.as_ref(),
        &principal.id,
        "cancelled",
        Some("new"),
        NOW + 1,
    )
    .await
    .unwrap();
    assert_eq!(changed, (store::GrantOutcome::Granted, Some(2)));
    // This is cancellation/drop, not an explicit rollback by the auth store.
    drop(transaction);
    drop(db);

    let reopened = LibSqlSubstrate::open_local(&path).await.unwrap();
    bootstrap_schema(&reopened, &schema).await.unwrap();
    bootstrap_schema(&reopened, &schema).await.unwrap();
    assert_eq!(snapshot(&reopened).await, before);
    store::grant_role(&reopened, &principal.id, "later", Some("cli"), NOW + 2)
        .await
        .unwrap();
    assert_eq!(store::authority_clock(&reopened).await.unwrap(), 2);
    let changes = store::authority_changes_after(&reopened, 1).await.unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].epoch, changes[0].seq), (2, 2));
}
