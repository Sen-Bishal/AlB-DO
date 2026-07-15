//! Substrate-neutral value and row types.
//!
//! The compiler and serve path speak these types; each [`DataSubstrate`]
//! implementation converts them to and from its backend's native
//! representation (libSQL `Value`, Postgres, …). Keeping the boundary in
//! neutral types is what makes the substrate genuinely *pluggable* — the
//! engine never names a storage crate.
//!
//! [`DataSubstrate`]: crate::forge::substrate::DataSubstrate

use thiserror::Error;

/// A single cell value crossing the substrate boundary.
///
/// The variants mirror SQLite's storage classes — the Phase 0 target is
/// libSQL — but nothing here depends on any storage crate, so a different
/// backend can map these onto its own type system.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    /// SQL `NULL`.
    Null,
    /// 64-bit signed integer (SQLite `INTEGER`).
    Integer(i64),
    /// 64-bit float (SQLite `REAL`).
    Real(f64),
    /// UTF-8 text (SQLite `TEXT`).
    Text(String),
    /// Raw bytes (SQLite `BLOB`).
    Blob(Vec<u8>),
}

impl From<i64> for SqlValue {
    fn from(v: i64) -> Self {
        Self::Integer(v)
    }
}

impl From<f64> for SqlValue {
    fn from(v: f64) -> Self {
        Self::Real(v)
    }
}

impl From<bool> for SqlValue {
    fn from(v: bool) -> Self {
        Self::Integer(i64::from(v))
    }
}

impl From<String> for SqlValue {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl From<&str> for SqlValue {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}

impl SqlValue {
    /// Borrow as `i64` when this is an [`SqlValue::Integer`].
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(v) => Some(*v),
            _ => None,
        }
    }

    /// Borrow as `&str` when this is an [`SqlValue::Text`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(v) => Some(v),
            _ => None,
        }
    }
}

/// One result row. Values are positional and align with the owning
/// [`Rows::columns`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    values: Vec<SqlValue>,
}

impl Row {
    /// Build a row from its ordered column values.
    pub fn new(values: Vec<SqlValue>) -> Self {
        Self { values }
    }

    /// Borrow the value at column `idx`, if present.
    pub fn get(&self, idx: usize) -> Option<&SqlValue> {
        self.values.get(idx)
    }

    /// All values, in column order.
    pub fn values(&self) -> &[SqlValue] {
        &self.values
    }
}

/// A result set: a stable column schema plus the rows, whose value order
/// matches [`Rows::columns`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Rows {
    /// Column names, in selection order.
    pub columns: Vec<String>,
    /// Result rows, each aligned to `columns`.
    pub rows: Vec<Row>,
}

/// Errors surfaced across the substrate boundary.
#[derive(Debug, Error)]
pub enum SubstrateError {
    /// The backend itself failed (connection, I/O, driver).
    #[error("substrate backend error: {0}")]
    Backend(String),
    /// A DDL migration failed to apply.
    #[error("migration failed: {0}")]
    Migration(String),
    /// A read/write statement failed.
    #[error("query failed: {0}")]
    Query(String),
}

/// Convenience alias for substrate results.
pub type Result<T> = std::result::Result<T, SubstrateError>;
