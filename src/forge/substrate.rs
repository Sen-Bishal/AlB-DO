//! The [`DataSubstrate`] trait — FORGE's lower boundary.

use async_trait::async_trait;

use crate::forge::value::{Result, Rows, SqlValue};

/// The pluggable storage plane FORGE compiles against.
///
/// The compiler's escape-analysis pass (Pillar 1) infers a schema and a
/// set of content-addressed queries; the serve path pre-resolves those
/// queries through a `DataSubstrate` into component props *before* the
/// synchronous QuickJS render, and routes durable-action writes back
/// through it. Any backend — libSQL for Phase 0, BYO-Postgres later, an
/// edge KV further out — implements this one trait. "The database is a
/// tier the compiler emits," and this is where that tier bottoms out.
///
/// The trait is object-safe and async on purpose: the serve loop is async
/// (axum/tokio) and the first-class target (libSQL) is async, so query
/// resolution happens in async Rust and the *already-resolved* props then
/// enter the synchronous JS world.
///
/// ## Atomic writes
///
/// [`begin`](DataSubstrate::begin) opens a [`Transaction`]: multiple
/// statements committed as one unit, or rolled back together. This is the
/// primitive the oversell invariant needs — decrement the last unit and
/// record the purchase *atomically*, so a crash between them can neither
/// oversell nor lose the write. Single-statement
/// [`execute`](DataSubstrate::execute) stays for the common
/// non-transactional write.
///
/// ## Not here yet
///
/// *Durable actions* — a write that not only commits atomically but
/// *resumes* a partially-run workflow after a process kill — are an
/// orchestration layer *on top of* [`begin`](DataSubstrate::begin), not a
/// change to this surface. The transaction seam gives crash-*atomicity*;
/// crash-*resumability* (Pillar 6's intent log) layers over it and arrives
/// separately.
#[async_trait]
pub trait DataSubstrate: Send + Sync {
    /// Apply a DDL migration (create table / index). Emitted by
    /// `albedo build` from the inferred schema and expected to be
    /// idempotent via the backend's own `IF NOT EXISTS` discipline.
    async fn migrate(&self, ddl: &str) -> Result<()>;

    /// Run a read query, returning the full result set. `params` bind
    /// positionally to `?` placeholders in `sql`. This is the call the
    /// serve path makes to pre-resolve a component's `data_deps`.
    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows>;

    /// Run a single-statement write (`INSERT`/`UPDATE`/`DELETE`), returning
    /// rows affected. For a write that must be atomic with a preceding read
    /// or another write, use [`begin`](DataSubstrate::begin) instead.
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64>;

    /// Open an atomic transaction. Statements run through the returned
    /// [`Transaction`] commit as one unit on
    /// [`commit`](Transaction::commit) or vanish on
    /// [`rollback`](Transaction::rollback) (or on drop). The seam the
    /// crash-safe, no-oversell write is built on.
    async fn begin(&self) -> Result<Box<dyn Transaction>>;
}

/// An in-flight atomic transaction over a [`DataSubstrate`].
///
/// Reads see the transaction's own uncommitted writes; other connections do
/// not until [`commit`](Transaction::commit). [`commit`](Transaction::commit)
/// and [`rollback`](Transaction::rollback) consume the handle, so it cannot
/// be reused after resolution; dropping it without committing rolls back.
#[async_trait]
pub trait Transaction: Send {
    /// Read within the transaction — sees its own uncommitted writes.
    async fn query(&self, sql: &str, params: &[SqlValue]) -> Result<Rows>;

    /// Write within the transaction, returning rows affected. Not durable
    /// until [`commit`](Transaction::commit).
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64>;

    /// Commit every buffered statement as one atomic unit.
    async fn commit(self: Box<Self>) -> Result<()>;

    /// Discard every buffered statement; the store is left untouched.
    async fn rollback(self: Box<Self>) -> Result<()>;
}
