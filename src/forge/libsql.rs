//! A libSQL-backed [`DataSubstrate`] — the Phase 0 storage backend.
//!
//! Local file (or `:memory:`) only for now; libSQL's remote /
//! embedded-replica modes are a later (deploy-manifold / edge) concern.
//! This is the first backend that actually *executes* the SQL the
//! compiler synthesizes, rather than recording it like
//! [`RecordingSubstrate`](crate::forge::mem::RecordingSubstrate).

use async_trait::async_trait;
use libsql::{
    params_from_iter, Builder, Connection, Transaction as LibsqlTx, TransactionBehavior,
    Value as LibsqlValue,
};

use crate::forge::substrate::{DataSubstrate, Transaction};
use crate::forge::value::{Result, Row, Rows, SqlValue, SubstrateError};

/// A libSQL connection presented through the neutral [`DataSubstrate`]
/// surface. Holds a single connection; the serve path will share it
/// behind an `Arc`.
pub struct LibSqlSubstrate {
    conn: Connection,
}

impl LibSqlSubstrate {
    /// Open (creating if absent) a local libSQL database at `path`.
    ///
    /// # Errors
    /// Returns [`SubstrateError::Backend`] if the database cannot be
    /// opened or a connection cannot be established.
    pub async fn open_local(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let db = Builder::new_local(path.as_ref())
            .build()
            .await
            .map_err(|e| SubstrateError::Backend(e.to_string()))?;
        let conn = db
            .connect()
            .map_err(|e| SubstrateError::Backend(e.to_string()))?;
        Ok(Self { conn })
    }

    /// Open an ephemeral in-memory libSQL database. Useful for tests and
    /// throwaway dev runs.
    ///
    /// # Errors
    /// Returns [`SubstrateError::Backend`] if the in-memory database
    /// cannot be initialized.
    pub async fn open_in_memory() -> Result<Self> {
        Self::open_local(":memory:").await
    }
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
        self.conn
            .execute_batch(ddl)
            .await
            .map(|_| ())
            .map_err(|e| SubstrateError::Migration(e.to_string()))
    }

    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows> {
        run_query(&self.conn, sql, params).await
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        run_execute(&self.conn, sql, params).await
    }

    /// Begin with `BEGIN IMMEDIATE` semantics: the write lock is taken at
    /// `begin`, not lazily on the first write, so two concurrent
    /// reserve-commit transactions serialise cleanly rather than racing to
    /// upgrade a deferred read lock — the classic SQLite
    /// `SQLITE_BUSY`/deadlock window. This is the seam the oversell
    /// invariant (conditional decrement + purchase insert, committed as one
    /// unit) is built on.
    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(|e| SubstrateError::Backend(e.to_string()))?;
        Ok(Box::new(LibSqlTransaction { tx }))
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
        let db = LibSqlSubstrate::open_in_memory().await.unwrap();

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
        let db = LibSqlSubstrate::open_in_memory().await.unwrap();
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
        let db = LibSqlSubstrate::open_in_memory().await.unwrap();
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
