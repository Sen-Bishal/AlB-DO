//! A libSQL-backed [`DataSubstrate`] — the Phase 0 storage backend.
//!
//! Local file only for now (including
//! [`open_ephemeral`](LibSqlSubstrate::open_ephemeral)'s throwaway temp file);
//! libSQL's remote / embedded-replica modes are a later (deploy-manifold /
//! edge) concern. This is the first backend that actually *executes* the SQL
//! the compiler synthesizes, rather than recording it like
//! [`RecordingSubstrate`](crate::forge::mem::RecordingSubstrate).
//!
//! ## Concurrency shape
//!
//! Two properties make a contended write behave correctly here, and both are
//! load-bearing for the oversell invariant:
//!
//! - **A connection per transaction.** SQLite allows one open transaction per
//!   connection, so a shared connection would fail concurrent claimants at
//!   `begin` rather than at the supply check.
//! - **`BEGIN IMMEDIATE` + a busy timeout + WAL.** Writers serialise and *wait*
//!   for the lock instead of erroring, then read the truth; readers never block
//!   the writer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use libsql::{
    params_from_iter, Builder, Connection, Database, Transaction as LibsqlTx, TransactionBehavior,
    Value as LibsqlValue,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::forge::substrate::{DataSubstrate, Transaction};
use crate::forge::value::{Result, Row, Rows, SqlValue, SubstrateError};

/// A safety net, not a strategy.
///
/// Writes are serialised in-process (see [`LibSqlSubstrate::writer`]), so SQLite
/// should never see two writers and never need to back off. This timeout only
/// covers the case where something *outside* this process holds the database's
/// write lock — another `albedo` instance, a `sqlite3` shell. If it ever fires,
/// the in-process queue is not the only writer and that is worth knowing loudly.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// A libSQL database presented through the neutral [`DataSubstrate`] surface.
///
/// ## The connection strategy, and why it is this one
///
/// SQLite allows **one writer at a time, per database** — that is not a tuning
/// knob, it is the storage engine. The only question is *where* writers queue.
///
/// - **In SQLite** (many connections racing for the write lock): a blocked
///   writer *sleeps* on the busy handler in growing increments (1, 2, 5, 10,
///   25, 50 ms…). Measured on the claim path, that cost **~15 ms per queued
///   buyer** — buyer #100 in a drop waits over a second, for a lock they may
///   not even need.
/// - **In this process** (one writer connection behind a fair async mutex):
///   writers queue in memory in microseconds, in FIFO order, and SQLite never
///   sees contention at all because there is only ever one writer.
///
/// The second is strictly better, so: **`writer` is the single connection every
/// write and every transaction goes through**, serialised by an async mutex;
/// **`reader` serves [`query`](DataSubstrate::query)**, which WAL lets run in
/// parallel with the writer and with itself.
///
/// This also removes connection *churn*. An earlier design opened a fresh
/// connection per transaction, which was correct but meant thousands of
/// open/close cycles under load; two long-lived connections have none.
pub struct LibSqlSubstrate {
    /// Reads only. WAL readers never block the writer, never block each other,
    /// and never take the write lock.
    reader: Connection,
    /// The one writer. Every write and transaction holds this mutex for its
    /// duration, which is what makes "one writer at a time" true *before*
    /// SQLite has to enforce it.
    writer: Arc<Mutex<Connection>>,
    /// Directory to delete on drop — set only by
    /// [`open_ephemeral`](Self::open_ephemeral).
    ephemeral_dir: Option<PathBuf>,
}

impl LibSqlSubstrate {
    /// Open (creating if absent) a local libSQL database at `path`.
    ///
    /// The database is put into **WAL** journal mode: readers do not block the
    /// writer, which is what a live drop — thousands of watchers reading the
    /// counter while buyers write it — actually needs.
    ///
    /// # Errors
    /// Returns [`SubstrateError::Backend`] if the database cannot be
    /// opened or a connection cannot be established.
    pub async fn open_local(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = Builder::new_local(path.as_ref())
            .build()
            .await
            .map_err(|e| SubstrateError::Backend(e.to_string()))?;

        let writer = connect(&db).await?;
        // Journal mode is a property of the database, not the connection, so
        // this is set once here rather than on every `connect`. Like
        // `busy_timeout` it reports its result, hence `query`. It must be set
        // before the reader connects, so the reader joins a WAL database.
        writer
            .query("PRAGMA journal_mode = WAL", ())
            .await
            .map_err(|e| SubstrateError::Backend(e.to_string()))?;

        let reader = connect(&db).await?;

        Ok(Self {
            reader,
            writer: Arc::new(Mutex::new(writer)),
            ephemeral_dir: None,
        })
    }

    /// Open a throwaway database in a temporary directory, deleted when this
    /// substrate drops. For tests and scratch dev runs.
    ///
    /// This is deliberately **file-backed rather than `:memory:`**. SQLite
    /// gives every connection to `:memory:` its own private database, so a
    /// transaction's dedicated connection would open a second, empty one with
    /// none of the caller's tables; the `cache=shared` alternative swaps that
    /// for table-level `SQLITE_LOCKED` errors that `busy_timeout` explicitly
    /// will not retry. Both are artefacts of in-memory mode that production —
    /// which is always file-backed — never sees. Testing on a real file keeps
    /// the tests honest about the thing that actually ships.
    ///
    /// # Errors
    /// Returns [`SubstrateError::Backend`] if the temporary directory or the
    /// database cannot be created.
    pub async fn open_ephemeral() -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "forge-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).map_err(|e| SubstrateError::Backend(e.to_string()))?;

        let mut substrate = Self::open_local(dir.join("forge.db")).await?;
        substrate.ephemeral_dir = Some(dir);
        Ok(substrate)
    }
}

impl Drop for LibSqlSubstrate {
    fn drop(&mut self) {
        if let Some(dir) = self.ephemeral_dir.take() {
            // Best-effort: a leaked temp dir is untidy, not incorrect, and a
            // panic in `drop` would be far worse than a stray file.
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Open a connection with the busy timeout applied.
///
/// Every connection this substrate hands out goes through here, so no code
/// path can accidentally get a zero-timeout connection that fails fast under
/// contention.
async fn connect(db: &Database) -> Result<Connection> {
    let conn = db
        .connect()
        .map_err(|e| SubstrateError::Backend(e.to_string()))?;
    // `PRAGMA busy_timeout` reports the value it set, so it is a *query*:
    // libSQL's `execute` rejects any statement that returns rows.
    conn.query(&format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}"), ())
        .await
        .map_err(|e| SubstrateError::Backend(e.to_string()))?;
    Ok(conn)
}

/// Lower a neutral [`SqlValue`] into libSQL's own value type.
fn to_libsql(value: &SqlValue) -> LibsqlValue {
    match value {
        SqlValue::Null => LibsqlValue::Null,
        SqlValue::Integer(i) => LibsqlValue::Integer(*i),
        SqlValue::Real(r) => LibsqlValue::Real(*r),
        SqlValue::Text(t) => LibsqlValue::Text(t.clone()),
        SqlValue::Blob(b) => LibsqlValue::Blob(b.clone()),
    }
}

/// Lift a libSQL value back into the neutral [`SqlValue`].
fn from_libsql(value: LibsqlValue) -> SqlValue {
    match value {
        LibsqlValue::Null => SqlValue::Null,
        LibsqlValue::Integer(i) => SqlValue::Integer(i),
        LibsqlValue::Real(r) => SqlValue::Real(r),
        LibsqlValue::Text(t) => SqlValue::Text(t),
        LibsqlValue::Blob(b) => SqlValue::Blob(b),
    }
}

/// Run a read query against any libSQL [`Connection`] and materialise the
/// result set. Shared by the substrate and its transactions — a
/// [`LibsqlTx`] derefs to `Connection`, so both read paths produce rows
/// identically instead of drifting apart.
async fn run_query(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<Rows> {
    let bound: Vec<LibsqlValue> = params.iter().map(to_libsql).collect();
    let mut result = conn
        .query(sql, params_from_iter(bound))
        .await
        .map_err(|e| SubstrateError::Query(e.to_string()))?;

    let col_count = result.column_count();
    let width = usize::try_from(col_count).unwrap_or(0);
    let mut columns = Vec::with_capacity(width);
    for i in 0..col_count {
        columns.push(result.column_name(i).unwrap_or_default().to_owned());
    }

    let mut rows = Vec::new();
    while let Some(row) = result
        .next()
        .await
        .map_err(|e| SubstrateError::Query(e.to_string()))?
    {
        let mut values = Vec::with_capacity(width);
        for i in 0..col_count {
            let value = row
                .get_value(i)
                .map_err(|e| SubstrateError::Query(e.to_string()))?;
            values.push(from_libsql(value));
        }
        rows.push(Row::new(values));
    }

    Ok(Rows { columns, rows })
}

/// Run a write against any libSQL [`Connection`], returning rows affected.
/// Shared by the substrate and its transactions for the same reason as
/// [`run_query`].
async fn run_execute(conn: &Connection, sql: &str, params: &[SqlValue]) -> Result<u64> {
    let bound: Vec<LibsqlValue> = params.iter().map(to_libsql).collect();
    conn.execute(sql, params_from_iter(bound))
        .await
        .map_err(|e| SubstrateError::Query(e.to_string()))
}

#[async_trait]
impl DataSubstrate for LibSqlSubstrate {
    async fn migrate(&self, ddl: &str) -> Result<()> {
        let writer = self.writer.lock().await;
        writer
            .execute_batch(ddl)
            .await
            .map(|_| ())
            .map_err(|e| SubstrateError::Migration(e.to_string()))
    }

    /// Reads go to the reader connection: no lock, no queue, fully parallel.
    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows> {
        run_query(&self.reader, sql, params).await
    }

    /// Writes queue on the in-process mutex rather than on SQLite's busy
    /// handler — microseconds and fair, instead of milliseconds of backoff.
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        let writer = self.writer.lock().await;
        run_execute(&writer, sql, params).await
    }

    /// Take the writer, then `BEGIN IMMEDIATE`.
    ///
    /// The mutex is acquired **before** the transaction and released only when
    /// the transaction resolves, which is what makes the whole design work:
    ///
    /// - Only one transaction can exist on `writer` at a time, so the
    ///   `cannot start a transaction within a transaction` failure that a
    ///   shared connection produces is impossible by construction.
    /// - SQLite never sees two writers, so its busy handler never fires and
    ///   nobody sleeps. Claimants queue here, in memory, in FIFO order —
    ///   microseconds instead of milliseconds, and *fair*, which a backoff loop
    ///   is not.
    /// - `IMMEDIATE` still takes the write lock up front rather than lazily, so
    ///   the transaction cannot fail a deferred lock upgrade halfway through.
    ///
    /// This is the seam the oversell invariant is built on: a losing buyer must
    /// lose *cleanly*, and a waiting buyer must not pay 15 ms to do it.
    // `significant_drop_tightening` wants the guard released sooner. Holding it
    // is the whole point: it moves into the returned transaction and is dropped
    // on commit/rollback, which is exactly what serialises writers. Releasing it
    // early would put two transactions on one connection — the bug this fixes.
    #[allow(clippy::significant_drop_tightening)]
    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        let writer = Arc::clone(&self.writer).lock_owned().await;
        let tx = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(|e| SubstrateError::Backend(e.to_string()))?;
        Ok(Box::new(LibSqlTransaction { tx, _writer: writer }))
    }
}

/// A libSQL transaction behind the neutral [`Transaction`] surface. Reads
/// and writes route through the same [`run_query`] / [`run_execute`] as the
/// substrate (the inner [`LibsqlTx`] derefs to [`Connection`]);
/// [`commit`](Transaction::commit) / [`rollback`](Transaction::rollback)
/// consume it so it can't be used after resolution. Dropping it without
/// resolving rolls back, via libSQL's own `Drop`.
struct LibSqlTransaction {
    tx: LibsqlTx,
    /// The writer mutex, held for exactly as long as the transaction lives.
    /// Never read — holding it *is* the point: it is what serialises writers
    /// in-process so SQLite never has two to arbitrate between. Dropped on
    /// commit/rollback, which hands the writer to the next queued claimant.
    _writer: OwnedMutexGuard<Connection>,
}

#[async_trait]
impl Transaction for LibSqlTransaction {
    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows> {
        run_query(&self.tx, sql, params).await
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        run_execute(&self.tx, sql, params).await
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        self.tx
            .commit()
            .await
            .map_err(|e| SubstrateError::Backend(e.to_string()))
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        self.tx
            .rollback()
            .await
            .map_err(|e| SubstrateError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persists_and_reads_back_a_real_row() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();

        db.migrate("CREATE TABLE post (id INTEGER PRIMARY KEY, title TEXT NOT NULL)")
            .await
            .unwrap();

        let affected = db
            .execute("INSERT INTO post (title) VALUES (?1)", &["Halation".into()])
            .await
            .unwrap();
        assert_eq!(affected, 1);

        let rows = db.query("SELECT id, title FROM post", &[]).await.unwrap();
        assert_eq!(rows.columns, vec!["id".to_owned(), "title".to_owned()]);
        assert_eq!(rows.rows.len(), 1);

        let row = rows.rows.first().unwrap();
        assert_eq!(row.get(0), Some(&SqlValue::Integer(1)));
        assert_eq!(row.get(1), Some(&SqlValue::Text("Halation".to_owned())));
    }

    /// The load-bearing property for the oversell invariant: an aborted
    /// transaction leaves the store byte-for-byte untouched, even though its
    /// own reads saw the write while it was open.
    #[tokio::test]
    async fn rolled_back_transaction_leaves_no_trace() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        db.migrate("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
            .await
            .unwrap();

        let tx = db.begin().await.unwrap();
        tx.execute("INSERT INTO t (v) VALUES (?1)", &["ghost".into()])
            .await
            .unwrap();

        // Visible to the transaction's own read while it is open …
        let mid = tx.query("SELECT COUNT(*) FROM t", &[]).await.unwrap();
        assert_eq!(mid.rows[0].get(0).and_then(SqlValue::as_i64), Some(1));

        tx.rollback().await.unwrap();

        // … and gone the instant it aborts.
        let after = db.query("SELECT COUNT(*) FROM t", &[]).await.unwrap();
        assert_eq!(after.rows[0].get(0).and_then(SqlValue::as_i64), Some(0));
    }

    /// The mirror property: a committed transaction's writes are durable and
    /// readable through the substrate afterwards.
    #[tokio::test]
    async fn committed_transaction_persists() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        db.migrate("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
            .await
            .unwrap();

        let tx = db.begin().await.unwrap();
        let affected = tx
            .execute("INSERT INTO t (v) VALUES (?1)", &["kept".into()])
            .await
            .unwrap();
        assert_eq!(affected, 1);
        tx.commit().await.unwrap();

        let after = db.query("SELECT v FROM t", &[]).await.unwrap();
        assert_eq!(after.rows.len(), 1);
        assert_eq!(
            after.rows[0].get(0),
            Some(&SqlValue::Text("kept".to_owned()))
        );
    }
}
