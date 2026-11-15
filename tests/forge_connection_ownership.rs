//! T-0040 regression tests: a libSQL connection closes once, whatever order
//! its owners drop in.
//!
//! libsql 0.6.0 closed every local connection twice (`LibsqlConnection::drop`
//! disconnected, then its `local::Connection` field disconnected again), which
//! aborted Windows test processes with `0xc0000005`. 0.10 nulls the handle
//! after closing. Each case opens thirty databases with real pages in them,
//! lets one owner of a connection outlive another, then drops everything.
//!
//! Measured 15 Nov 2026 on Windows, 20 child runs per case: on 0.6.0 the first
//! three cases aborted (`0xc0000005`); on 0.10.0 none did. The statement case
//! passed on 0.6.0 too (there the statement's own clone is the last owner and
//! closes once); it stays as a guard on that ordering.
//!
//! An abort cannot be caught in-process, so each case re-runs this binary as a
//! child per attempt and asserts on the child's exit status. Codex wrote the
//! first three as falsifiers against a wrapper that preceded the upgrade
//! (15 Nov 2026); they are kept here in the form that tests the dependency.

use std::future::Future;
use std::path::Path;

const CHILD: &str = "ALBEDO_CONNECTION_OWNERSHIP_CHILD";
const ROOT: &str = "ALBEDO_CONNECTION_OWNERSHIP_ROOT";
const ATTEMPTS: usize = 20;
const DATABASES: usize = 30;

/// Run `exercise` in a fresh child process `ATTEMPTS` times; every child must
/// exit cleanly and leave no file open.
fn check_in_children<F, Fut>(name: &str, exercise: F)
where
    F: Fn(std::path::PathBuf) -> Fut,
    Fut: Future<Output = ()>,
{
    if std::env::var(CHILD).as_deref() == Ok(name) {
        let root = std::path::PathBuf::from(std::env::var_os(ROOT).expect("child root"));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(exercise(root));
        return;
    }

    let root = tempfile::tempdir().expect("tempdir");
    for attempt in 0..ATTEMPTS {
        let path = root.path().join(attempt.to_string());
        std::fs::create_dir(&path).expect("attempt dir");
        let mut child = std::process::Command::new(std::env::current_exe().expect("exe"))
            .args([name, "--exact", "--nocapture"])
            .env(CHILD, name)
            .env(ROOT, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut timed_out = false;
        while child.try_wait().expect("poll child").is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().expect("kill child");
                timed_out = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let result = child.wait_with_output().expect("child output");
        assert!(
            !timed_out && result.status.success(),
            "attempt {attempt}: timed_out={timed_out}, status={}; stdout={}; stderr={}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
    root.close().expect("every connection released its files");
}

async fn database(root: &Path, name: &str, n: usize) -> libsql::Database {
    libsql::Builder::new_local(root.join(format!("{name}-{n}.db")))
        .build()
        .await
        .expect("open database")
}

/// The original 0.6.0 bug, with nothing else in the way: many connections,
/// each with pages written, all dropped.
#[test]
fn many_connections_each_close_once() {
    check_in_children("many_connections_each_close_once", |root| async move {
        let mut live = Vec::new();
        for n in 0..DATABASES {
            let db = database(&root, "plain", n).await;
            let conn = db.connect().expect("connect");
            conn.execute("CREATE TABLE t (v BLOB)", ()).await.expect("table");
            conn.execute("INSERT INTO t VALUES (zeroblob(131072))", ())
                .await
                .expect("insert");
            live.push((conn, db));
        }
        drop(live);
    });
}

/// A clone of the connection outlives the original.
#[test]
fn a_cloned_connection_can_outlive_the_original() {
    check_in_children("a_cloned_connection_can_outlive_the_original", |root| async move {
        let mut live = Vec::new();
        for n in 0..DATABASES {
            let db = database(&root, "clone", n).await;
            let conn = db.connect().expect("connect");
            conn.execute("CREATE TABLE t (v BLOB)", ()).await.expect("table");
            let escaped = conn.clone();
            drop(conn);
            escaped
                .execute("INSERT INTO t VALUES (zeroblob(131072))", ())
                .await
                .expect("insert through the clone");
            live.push((escaped, db));
        }
        drop(live);
    });
}

/// A transaction (which holds its own clone) outlives its connection.
#[test]
fn a_transaction_can_outlive_its_connection() {
    check_in_children("a_transaction_can_outlive_its_connection", |root| async move {
        let mut live = Vec::new();
        for n in 0..DATABASES {
            let db = database(&root, "transaction", n).await;
            let conn = db.connect().expect("connect");
            conn.execute("CREATE TABLE t (v BLOB)", ()).await.expect("table");
            let transaction = conn.transaction().await.expect("begin");
            drop(conn);
            transaction
                .execute("INSERT INTO t VALUES (zeroblob(131072))", ())
                .await
                .expect("insert in the transaction");
            live.push((transaction, db));
        }
        for (transaction, _db) in live {
            transaction.commit().await.expect("commit");
        }
    });
}

/// A prepared statement (which holds its own clone) outlives its connection.
#[test]
fn a_statement_can_outlive_its_connection() {
    check_in_children("a_statement_can_outlive_its_connection", |root| async move {
        let mut live = Vec::new();
        for n in 0..DATABASES {
            let db = database(&root, "statement", n).await;
            let conn = db.connect().expect("connect");
            conn.execute("CREATE TABLE t (v BLOB)", ()).await.expect("table");
            let mut statement = conn
                .prepare("INSERT INTO t VALUES (zeroblob(131072))")
                .await
                .expect("prepare");
            drop(conn);
            statement.execute(()).await.expect("execute after the connection dropped");
            live.push((statement, db));
        }
        drop(live);
    });
}
