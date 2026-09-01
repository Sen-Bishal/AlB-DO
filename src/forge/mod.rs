//! # FORGE — the backend-less backend
//!
//! *The engine already decides where your UI runs. FORGE teaches it to
//! decide where your data lives, so the backend stops being a system you
//! integrate and becomes an artifact the compiler emits.*
//!
//! This module is the **runtime (storage) plane** of that idea. The
//! compile-time half is the `forge` block: declarations lower to
//! [`ForgeCollection`](skeleton::ForgeCollection)s, each carrying the DDL and
//! the query that materialises it, and a component's `useSharedSlot(collection)`
//! mints the topic that read is served from. What this module supplies is the
//! thing that actually *runs* those queries: the pluggable [`DataSubstrate`].
//!
//! 🔑 **Every read reaches a caller through a declared collection**, which is
//! what makes it a topic, which is what lets it be partitioned and keyed by a
//! principal. There is deliberately no raw-query path into a component's props —
//! see [`DataSource`](crate::manifest::schema::DataSource), where the reasoning
//! is recorded next to the variant that was removed to keep it true.
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
//! - [`drift`] — [`evolve_schema`](drift::evolve_schema), the boot gate that
//!   reconciles the declaration with the database: it adds new nullable columns
//!   and refuses to serve any other disagreement. Migrations are `IF NOT EXISTS`
//!   only, so an edited `forge` block would otherwise apply as silence.
//!
//! ## Roadmap (see `development-plan/backend.md`)
//!
//! Phase 0 targets a single libSQL-backed substrate and detects exactly
//! one persistent collection. The libSQL implementation and the
//! escape-analysis pass land next; this scaffold fixes the boundary they
//! meet at, and nothing here is wired into the default serve path yet.

pub mod bindings;
pub mod declare;
pub mod delta;
pub mod drift;
pub mod mem;
pub mod reserve;
pub mod skeleton;
pub mod substrate;
pub mod typegen;
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
pub use bindings::{
    validate_literal_topic_reads, validate_partition_bindings, LiteralTopicRead,
    PartitionBinding, WritableTopics,
};
pub use declare::{CollectionDecl, FieldType};
pub use drift::{evolve_schema, Addition, Change, CollectionDrift, SchemaDrift, VerifyError};
pub use typegen::emit_forge_dts;
pub use skeleton::{ForgeCollection, ForgeSchema, ForgeSchemaError, SeedRow};
pub use substrate::{DataSubstrate, Transaction};
pub use value::{Result, Row, Rows, SqlValue, SubstrateError};
pub use write::{
    apply_writes, install_forge_write_collector, FanOut, ForgeWrite, ForgeWriteCollector,
};

#[cfg(feature = "forge")]
pub use libsql::LibSqlSubstrate;
