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
//!
//! ## Roadmap (see `development-plan/backend.md`)
//!
//! Phase 0 targets a single libSQL-backed substrate and detects exactly
//! one persistent collection. The libSQL implementation and the
//! escape-analysis pass land next; this scaffold fixes the boundary they
//! meet at, and nothing here is wired into the default serve path yet.

pub mod mem;
pub mod skeleton;
pub mod substrate;
pub mod value;

#[cfg(feature = "forge")]
pub mod libsql;

pub use substrate::DataSubstrate;
pub use value::{Result, Row, Rows, SqlValue, SubstrateError};

#[cfg(feature = "forge")]
pub use libsql::LibSqlSubstrate;
