//! # FORGE — the backend-less backend
//!
//! *The engine already decides where your UI runs. FORGE teaches it to
//! decide where your data lives, so the backend stops being a system you
//! integrate and becomes an artifact the compiler emits.*
//!
//! This module is the **runtime (storage) plane** of that idea. The
//! compile-time half already has a seam in the manifest: escape analysis
//! populates [`DataDep`](crate::manifest::schema::DataDep)s carrying a
//! [`DataSource::DbQuery`](crate::manifest::schema::DataSource), and the
//! serve path pre-resolves those queries into component props before the
//! synchronous QuickJS render. What was missing is the thing that actually
//! *runs* a query: the pluggable [`DataSubstrate`].
//!
//! ## Shape
//!
//! - [`value`] — substrate-neutral value/row types the engine speaks, so
//!   no storage crate's types leak into the compiler or serve loop.
//! - [`substrate`] — the [`DataSubstrate`] trait: the one seam every
//!   backend (libSQL for Phase 0, BYO-Postgres later, an edge KV further
//!   out) implements.
//! - [`mem`] — [`RecordingSubstrate`](mem::RecordingSubstrate), an
//!   in-memory test double that lets the wiring be exercised before the
//!   real libSQL backend is attached.
//! - [`reserve`] — [`Reservations`](reserve::Reservations), atomic claiming
//!   of a bounded resource (tickets, stock, seats, quotas). The contention
//!   primitive: supply never goes negative, never oversells, and a retried
//!   request never claims twice.
//!
//! ## Roadmap (see `development-plan/backend.md`)
//!
//! Phase 0 targets a single libSQL-backed substrate and detects exactly
//! one persistent collection. The libSQL implementation and the
//! escape-analysis pass land next; this scaffold fixes the boundary they
//! meet at, and nothing here is wired into the default serve path yet.

pub mod delta;
pub mod mem;
pub mod reserve;
pub mod skeleton;
pub mod substrate;
pub mod value;
pub mod write;

#[cfg(feature = "forge")]
pub mod libsql;

pub use delta::{
    appended_rows, classify_positioned_insert, diff_records, project_changes,
    project_inserted_rows, PositionedInsert, RecordChange, RenderedRows, RowProjector,
};
pub use reserve::{
    IdempotencyConflict, ReleaseOutcome, ReserveError, ReserveOutcome, ReserveRequest, Reservations,
};
pub use skeleton::{ForgeCollection, ForgeSchema, ForgeSchemaError, SeedRow};
pub use substrate::{DataSubstrate, Transaction};
pub use value::{Result, Row, Rows, SqlValue, SubstrateError};
pub use write::{apply_writes, install_forge_write_collector, ForgeWrite, ForgeWriteCollector};

#[cfg(feature = "forge")]
pub use libsql::LibSqlSubstrate;
