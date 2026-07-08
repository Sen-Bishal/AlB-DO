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
/// ## Not here yet
///
/// Durable actions (Phase 0's "a write that survives a process kill") are
/// an orchestration layer *on top of* [`execute`](DataSubstrate::execute),
/// not a change to this surface — they'll arrive alongside the libSQL
/// backend. Kept out of the first cut so the seam stays minimal.
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

    /// Run a write (`INSERT`/`UPDATE`/`DELETE`), returning rows affected.
    /// Phase 0 wraps this in a durable action so a crash mid-write rolls
    /// back and resumes; that orchestration layers over this method rather
    /// than altering it.
    async fn execute(&self, sql: &str, params: &[SqlValue]) -> Result<u64>;
}
