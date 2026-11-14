//! T-0033 · Q1a — new-style FORGE reads: the descriptor, its lowering to SQL,
//! derived indexes, and argument coercion.
//!
//! ```ts
//! import { invoices } from "albedo/forge";
//! await invoices.where({ status: "open" }).orderBy("due", "desc").limit(50);
//! ```
//!
//! This module is pure: it takes a descriptor and a collection schema and
//! produces SQL, a parameter plan and index DDL. Recognising chains in source
//! (the classification pass), running them (Q1b) and live queries (Q3) live
//! elsewhere.

pub mod coerce;
pub mod descriptor;
pub mod index;
pub mod lower;

pub use descriptor::{
    Aggregate, Canonical, CollectionSchema, Direction, Mode, OrderTerm, QueryDescriptor,
    QueryError, RangeBound, RowBound, Shape, Term, TermOp,
};
pub use lower::{lower, Lowered, MaskStatement, ParamRole, ParamSpec};

#[cfg(test)]
mod tests;
