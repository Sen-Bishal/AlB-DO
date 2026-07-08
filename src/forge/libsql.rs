//! A libSQL-backed [`DataSubstrate`] — the Phase 0 storage backend.
//!
//! Local file (or `:memory:`) only for now; libSQL's remote /
//! embedded-replica modes are a later (deploy-manifold / edge) concern.
//! This is the first backend that actually *executes* the SQL the
//! compiler synthesizes, rather than recording it like
//! [`RecordingSubstrate`](crate::forge::mem::RecordingSubstrate).

use async_trait::async_trait;
use libsql::{params_from_iter, Builder, Connection, Value as LibsqlValue};

use crate::forge::substrate::DataSubstrate;
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
        let bound: Vec<LibsqlValue> = params.iter().map(to_libsql).collect();
        let mut result = self
            .conn
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

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        let bound: Vec<LibsqlValue> = params.iter().map(to_libsql).collect();
        self.conn
            .execute(sql, params_from_iter(bound))
            .await
            .map_err(|e| SubstrateError::Query(e.to_string()))
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
}
