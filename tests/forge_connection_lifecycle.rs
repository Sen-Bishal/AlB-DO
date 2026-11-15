//! Adversarial coverage for the T-0040 connection lifetime workaround.

use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate, SqlValue};
use std::sync::Arc;

#[test]
fn sqlite_allocation_limits_during_open_do_not_abort_the_process() {
    const LIMIT: &str = "ALBEDO_CONNECTION_TEST_HEAP_LIMIT";
    const ROOT: &str = "ALBEDO_CONNECTION_TEST_ROOT";
    if let Ok(limit) = std::env::var(LIMIT) {
        let root = std::path::PathBuf::from(std::env::var_os(ROOT).unwrap());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let control = LibSqlSubstrate::open_local(root.join("control.db"))
                .await
                .unwrap();
            let _ = control
                .query(&format!("PRAGMA hard_heap_limit = {limit}"), &[])
                .await;
            for n in 0..16 {
                match LibSqlSubstrate::open_local(root.join(format!("limited-{n}.db"))).await {
                    Ok(db) => drop(db),
                    Err(error) => println!("limit={limit} open={n}: {error}"),
                }
            }
        });
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut exercised_error = false;
    for limit in (64 * 1024..=256 * 1024).step_by(8 * 1024) {
        let root = dir.path().join(limit.to_string());
        std::fs::create_dir(&root).unwrap();
        // SQLite's hard heap limit is global and can only be lowered. Keep
        // each limit in its own process, away from other tests and runtimes.
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "sqlite_allocation_limits_during_open_do_not_abort_the_process",
                "--exact",
                "--nocapture",
            ])
            .env(LIMIT, limit.to_string())
            .env(ROOT, root)
            .output()
            .unwrap();
        exercised_error |= String::from_utf8_lossy(&result.stdout).contains("open=");
        assert!(
            result.status.success(),
            "limit={limit}, status={}; stdout={}; stderr={}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert!(
        exercised_error,
        "at least one open must hit SQLite's heap limit"
    );
    dir.close().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_connection_churn_does_not_close_a_live_writer_or_hold_files() {
    let dir = tempfile::tempdir().unwrap();
    // canonicalize gives an extended-length path on Windows; include spaces,
    // Unicode and a quote to exercise bound VACUUM/ATTACH filenames as well.
    let mut root = std::fs::canonicalize(dir.path()).unwrap();
    for _ in 0..5 {
        root.push("long-directory-segment-012345678901234567890123456789");
    }
    std::fs::create_dir_all(&root).unwrap();
    assert!(root.as_os_str().len() > 260);
    let db = Arc::new(
        LibSqlSubstrate::open_local(root.join("live 'λ.db"))
            .await
            .unwrap(),
    );
    db.migrate("CREATE TABLE t (id INTEGER PRIMARY KEY, payload BLOB)")
        .await
        .unwrap();
    assert!(db
        .snapshot_into(&root.join("missing/backup.db"))
        .await
        .is_err());
    let writer = Arc::clone(&db);
    let task = tokio::spawn(async move {
        for id in 0..64 {
            writer
                .execute(
                    "INSERT INTO t VALUES (?1, ?2)",
                    &[
                        SqlValue::Integer(id),
                        SqlValue::Blob(vec![0x5a; 128 * 1024]),
                    ],
                )
                .await
                .unwrap();
            tokio::task::yield_now().await;
        }
    });
    for n in 0..12 {
        let path = root.join(format!("snapshot 'λ {n}.db"));
        let facts = db.snapshot_into(&path).await.unwrap();
        assert!(facts.tables["t"] <= 64);
        // Already-existing destination fails after opening a connection.
        assert!(db.snapshot_into(&path).await.is_err());
        LibSqlSubstrate::seal(&path).await.unwrap();
        let snapshot = LibSqlSubstrate::open_local(&path).await.unwrap();
        let count = snapshot.query("SELECT COUNT(*) FROM t", &[]).await.unwrap();
        assert_eq!(
            count.rows[0].get(0),
            Some(&SqlValue::Integer(
                i64::try_from(facts.tables["t"]).unwrap()
            ))
        );
        drop(snapshot);
        std::fs::remove_file(&path).expect("snapshot and seal released their handles");
    }
    task.await.unwrap();
    let count = db.query("SELECT COUNT(*) FROM t", &[]).await.unwrap();
    assert_eq!(count.rows[0].get(0), Some(&SqlValue::Integer(64)));
    drop(db);
    dir.close()
        .expect("all live, snapshot and failed-copy handles released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transactions_can_outlive_the_substrate_on_another_thread() {
    let dir = tempfile::tempdir().unwrap();
    for resolution in 0..3 {
        let path = dir.path().join(format!("transaction-{resolution}.db"));
        let db = LibSqlSubstrate::open_local(&path).await.unwrap();
        db.migrate("CREATE TABLE t (id INTEGER)").await.unwrap();
        let tx = db.begin().await.unwrap();
        tx.execute("INSERT INTO t VALUES (7)", &[]).await.unwrap();
        drop(db);
        tokio::spawn(async move {
            let rows = tx.query("SELECT id FROM t", &[]).await.unwrap();
            assert_eq!(rows.rows[0].get(0), Some(&SqlValue::Integer(7)));
            match resolution {
                0 => tx.commit().await.unwrap(),
                1 => tx.rollback().await.unwrap(),
                _ => drop(tx),
            }
        })
        .await
        .unwrap();
        let reopened = LibSqlSubstrate::open_local(&path).await.unwrap();
        let count = reopened.query("SELECT COUNT(*) FROM t", &[]).await.unwrap();
        assert_eq!(
            count.rows[0].get(0),
            Some(&SqlValue::Integer(i64::from(resolution == 0)))
        );
        drop(reopened);
        std::fs::remove_file(path).expect("last transaction owner released the file");
    }
    dir.close().unwrap();
}

#[tokio::test]
async fn failed_open_and_seal_release_handles_and_preserve_invalid_files() {
    let dir = tempfile::tempdir().unwrap();
    let corrupt = dir.path().join("not-a-database.db");
    let bytes = vec![0x91; 8192];
    std::fs::write(&corrupt, &bytes).unwrap();
    for _ in 0..30 {
        assert!(LibSqlSubstrate::open_local(&corrupt).await.is_err());
        assert!(LibSqlSubstrate::seal(&corrupt).await.is_err());
        assert_eq!(std::fs::read(&corrupt).unwrap(), bytes);
        // Opening an invalid path fails before any usable connection exists.
        assert!(LibSqlSubstrate::open_local(dir.path()).await.is_err());
    }
    std::fs::remove_file(corrupt).expect("failed initialization released handles");
    dir.close().unwrap();
}

#[tokio::test]
async fn failed_seal_of_a_live_wal_database_keeps_its_writer_usable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.db");
    let db = LibSqlSubstrate::open_local(&path).await.unwrap();
    db.migrate("CREATE TABLE t (id INTEGER)").await.unwrap();
    let tx = db.begin().await.unwrap();
    tx.execute("INSERT INTO t VALUES (9)", &[]).await.unwrap();
    assert!(LibSqlSubstrate::seal(&path).await.is_err());
    tx.commit().await.unwrap();
    assert_eq!(
        db.execute("INSERT INTO t VALUES (10)", &[]).await.unwrap(),
        1
    );
    drop(db);
    LibSqlSubstrate::seal(&path).await.unwrap();
    assert!(!path.with_extension("db-wal").exists());
    std::fs::remove_file(path).unwrap();
    dir.close().unwrap();
}
