//! An in-memory [`DataSubstrate`] test double.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;

use crate::forge::substrate::{DataSubstrate, Transaction};
use crate::forge::value::{Result, Rows, SqlValue};

/// In-memory test double for [`DataSubstrate`].
///
/// It records every migration, query, and write it receives and replays a
/// programmed queue of query responses. It deliberately does **not**
/// interpret SQL — parsing and executing SQL is exactly the job Phase 0
/// delegates to libSQL rather than hand-rolling a query engine — so this
/// exists only to exercise the compiler/serve wiring end-to-end before the
/// real backend is attached, and as a fixture in tests.
#[derive(Default)]
pub struct RecordingSubstrate {
    inner: Arc<Mutex<Recording>>,
}

#[derive(Default)]
struct Recording {
    migrations: Vec<String>,
    queries: Vec<(String, Vec<SqlValue>)>,
    writes: Vec<(String, Vec<SqlValue>)>,
    responses: VecDeque<Rows>,
}

impl RecordingSubstrate {
    /// A fresh recorder with no programmed responses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one [`Rows`] to be returned by the next
    /// [`DataSubstrate::query`] call. Responses are dequeued FIFO; an
    /// empty queue yields empty rows.
    pub fn push_response(&self, rows: Rows) {
        self.lock().responses.push_back(rows);
    }

    /// Every DDL string passed to [`DataSubstrate::migrate`], in order.
    pub fn migrations(&self) -> Vec<String> {
        self.lock().migrations.clone()
    }

    /// Every `(sql, params)` passed to [`DataSubstrate::query`], in order.
    pub fn queries(&self) -> Vec<(String, Vec<SqlValue>)> {
        self.lock().queries.clone()
    }

    /// Every `(sql, params)` passed to [`DataSubstrate::execute`], in
    /// order.
    pub fn writes(&self) -> Vec<(String, Vec<SqlValue>)> {
        self.lock().writes.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Recording> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl DataSubstrate for RecordingSubstrate {
    async fn migrate(&self, ddl: &str) -> Result<()> {
        self.lock().migrations.push(ddl.to_owned());
        Ok(())
    }

    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows> {
        let mut guard = self.lock();
        guard.queries.push((sql.to_owned(), params.to_vec()));
        Ok(guard.responses.pop_front().unwrap_or_default())
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        self.lock().writes.push((sql.to_owned(), params.to_vec()));
        Ok(1)
    }

    async fn begin(&self) -> Result<Box<dyn Transaction>> {
        // Share the substrate's recording log so writes made through the
        // transaction still show up in `writes()` — the least surprising
        // behaviour for a wiring double.
        Ok(Box::new(RecordingTransaction {
            inner: Arc::clone(&self.inner),
        }))
    }
}

/// A transaction over [`RecordingSubstrate`]. It records like its parent but
/// **cannot actually roll back** — the double doesn't interpret SQL, so it
/// has no state to undo, and `commit`/`rollback` are both no-ops. Atomicity
/// is proven against the real libSQL backend, never here; this exists only
/// to keep the wiring exercisable through `&dyn DataSubstrate`.
struct RecordingTransaction {
    inner: Arc<Mutex<Recording>>,
}

impl RecordingTransaction {
    fn lock(&self) -> MutexGuard<'_, Recording> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Transaction for RecordingTransaction {
    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows> {
        let mut guard = self.lock();
        guard.queries.push((sql.to_owned(), params.to_vec()));
        Ok(guard.responses.pop_front().unwrap_or_default())
    }

    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64> {
        self.lock().writes.push((sql.to_owned(), params.to_vec()));
        Ok(1)
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        Ok(())
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::value::Row;

    #[tokio::test]
    async fn records_and_replays_through_trait_object() {
        let sub = RecordingSubstrate::new();
        sub.push_response(Rows {
            columns: vec!["id".to_owned(), "title".to_owned()],
            rows: vec![Row::new(vec![1i64.into(), "Halation".into()])],
        });

        // Exercise it purely through the object-safe trait — proving the
        // seam is usable as `&dyn DataSubstrate`, the way the serve path
        // will hold it.
        let db: &dyn DataSubstrate = &sub;
        db.migrate("CREATE TABLE post (id INTEGER, title TEXT)")
            .await
            .unwrap();
        db.execute("INSERT INTO post (title) VALUES (?)", &["Halation".into()])
            .await
            .unwrap();
        let rows = db.query("SELECT id, title FROM post", &[]).await.unwrap();

        // The programmed response comes back verbatim.
        assert_eq!(rows.columns, vec!["id".to_owned(), "title".to_owned()]);
        let first = rows.rows.first().unwrap();
        assert_eq!(first.get(1), Some(&SqlValue::Text("Halation".to_owned())));

        // And every call was recorded, params intact.
        assert_eq!(sub.migrations().len(), 1);
        assert_eq!(sub.queries().len(), 1);
        let writes = sub.writes();
        let first_write = writes.first().unwrap();
        assert_eq!(first_write.1, vec![SqlValue::Text("Halation".to_owned())]);
    }
}
