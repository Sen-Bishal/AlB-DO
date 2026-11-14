//! T-0033 · Q1a — derived indexes (Design 11).
//!
//! Equality–Sort–Range: equality columns (by name) ++ `in` columns (by name,
//! R3-3) ++ sort columns with directions (minus any that are equality-
//! constrained) ++ one range column unless it is already a sort column.
//! `count`: equality ++ `in` ++ range. `groupBy`: equality ++ `in` ++ group
//! column. Each OR branch gets its own equality ++ range index. Then
//! **deduplicated by left-prefix subsumption** across every query on the table
//! and against legacy partition indexes, and named
//! `idx_<table>_q<8 hex of sha256 over the canonical column spec>` — the legacy
//! scheme drops directions, so `(a, b)` and `(a, b DESC)` would share a name.

use super::descriptor::{hex, CollectionSchema, Direction, QueryDescriptor, Shape, Term, TermOp};
use sha2::{Digest, Sha256};

/// One indexed column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IndexColumn {
    pub column: String,
    pub dir: Direction,
}

/// A derived index on one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    pub table: String,
    pub columns: Vec<IndexColumn>,
}

impl IndexSpec {
    /// `a, b DESC` — the canonical column spec (also what the name hashes).
    #[must_use]
    pub fn column_spec(&self) -> String {
        self.columns
            .iter()
            .map(|c| match c.dir {
                Direction::Asc => c.column.clone(),
                Direction::Desc => format!("{} DESC", c.column),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `idx_<table>_q<8 hex>`; distinct for `(a, b)` and `(a, b DESC)`.
    #[must_use]
    pub fn name(&self) -> String {
        let digest = Sha256::digest(self.column_spec().as_bytes());
        format!("idx_{}_q{}", self.table, &hex(&digest)[..8])
    }

    /// `CREATE INDEX IF NOT EXISTS …`.
    #[must_use]
    pub fn ddl(&self) -> String {
        format!(
            "CREATE INDEX IF NOT EXISTS {} ON {}({})",
            self.name(),
            self.table,
            self.column_spec()
        )
    }

    /// True when `self`'s columns are a left prefix of `other`'s (same columns,
    /// same directions), so `other` serves every query `self` would.
    #[must_use]
    pub fn is_prefix_of(&self, other: &IndexSpec) -> bool {
        self.table == other.table
            && self.columns.len() <= other.columns.len()
            && self.columns.iter().zip(&other.columns).all(|(a, b)| a == b)
    }
}

fn asc(column: &str) -> IndexColumn {
    IndexColumn {
        column: column.to_string(),
        dir: Direction::Asc,
    }
}

/// Every index one descriptor wants, per presence mask, before dedup.
#[must_use]
pub fn wanted(descriptor: &QueryDescriptor, schema: &CollectionSchema) -> Vec<IndexSpec> {
    let optional = descriptor
        .terms
        .iter()
        .chain(descriptor.or_groups.iter().flatten().flatten())
        .filter_map(|t| t.optional_slot)
        .map(|s| u32::from(s) + 1)
        .max()
        .unwrap_or(0);
    let mut out = Vec::new();
    for mask in 0..(1u16 << optional) {
        let present = |t: &&Term| t.optional_slot.map_or(true, |s| mask & (1 << s) != 0);
        let terms: Vec<&Term> = descriptor.terms.iter().filter(present).collect();
        if let Some(spec) = for_terms(&terms, descriptor, schema) {
            out.push(spec);
        }
        for group in &descriptor.or_groups {
            for branch in group {
                let mut branch_terms: Vec<&Term> = terms.clone();
                branch_terms.extend(branch.iter().filter(present));
                if let Some(spec) = for_branch(&branch_terms, schema) {
                    out.push(spec);
                }
            }
        }
    }
    out
}

fn split(terms: &[&Term]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut eq: Vec<String> = Vec::new();
    let mut inn: Vec<String> = Vec::new();
    let mut range: Vec<String> = Vec::new();
    for term in terms {
        let bucket = match term.op {
            TermOp::Eq { .. } | TermOp::IsNull => &mut eq,
            TermOp::In { .. } => &mut inn,
            TermOp::Range { .. } | TermOp::NotNull => &mut range,
        };
        if !bucket.contains(&term.column) {
            bucket.push(term.column.clone());
        }
    }
    eq.sort();
    inn.retain(|column| !eq.contains(column));
    inn.sort();
    range.sort();
    (eq, inn, range)
}

fn for_terms(
    terms: &[&Term],
    descriptor: &QueryDescriptor,
    schema: &CollectionSchema,
) -> Option<IndexSpec> {
    if matches!(descriptor.shape, Shape::Get { .. }) {
        return None; // the primary key serves it
    }
    let (eq, inn, range) = split(terms);
    let mut columns: Vec<IndexColumn> = eq.iter().chain(&inn).map(|c| asc(c)).collect();
    match &descriptor.shape {
        Shape::Rows => {
            for term in &descriptor.order {
                if eq.contains(&term.column) {
                    continue; // constant under the equality: dropped from the index
                }
                if !columns.iter().any(|c| c.column == term.column) {
                    columns.push(IndexColumn {
                        column: term.column.clone(),
                        dir: term.dir,
                    });
                }
            }
            if let Some(first_range) = range.first() {
                if !columns.iter().any(|c| &c.column == first_range) {
                    columns.push(asc(first_range));
                }
            }
        }
        Shape::Count => {
            if let Some(first_range) = range.first() {
                if !columns.iter().any(|c| &c.column == first_range) {
                    columns.push(asc(first_range));
                }
            }
        }
        Shape::Group { column, .. } => {
            if !columns.iter().any(|c| &c.column == column) {
                columns.push(asc(column));
            }
        }
        Shape::Get { .. } => {}
    }
    // An index whose only column is the key adds nothing.
    if columns.is_empty() || (columns.len() == 1 && columns[0].column == schema.key) {
        return None;
    }
    Some(IndexSpec {
        table: schema.table.clone(),
        columns,
    })
}

fn for_branch(terms: &[&Term], schema: &CollectionSchema) -> Option<IndexSpec> {
    let (eq, inn, range) = split(terms);
    let mut columns: Vec<IndexColumn> = eq.iter().chain(&inn).map(|c| asc(c)).collect();
    if let Some(first_range) = range.first() {
        if !columns.iter().any(|c| &c.column == first_range) {
            columns.push(asc(first_range));
        }
    }
    (!columns.is_empty()).then(|| IndexSpec {
        table: schema.table.clone(),
        columns,
    })
}

/// Dedup `wanted` by left-prefix subsumption, against each other and against
/// `existing` (legacy partition indexes). Returns the indexes to create, in a
/// stable order.
#[must_use]
pub fn dedup(mut wanted: Vec<IndexSpec>, existing: &[IndexSpec]) -> Vec<IndexSpec> {
    // Longest first, so a shorter one is checked against everything that could
    // subsume it.
    wanted.sort_by(|a, b| {
        b.columns
            .len()
            .cmp(&a.columns.len())
            .then_with(|| a.column_spec().cmp(&b.column_spec()))
    });
    let mut kept: Vec<IndexSpec> = Vec::new();
    for spec in wanted {
        let subsumed = existing.iter().chain(kept.iter()).any(|other| spec.is_prefix_of(other));
        if !subsumed {
            kept.push(spec);
        }
    }
    kept.sort_by(|a, b| a.table.cmp(&b.table).then_with(|| a.column_spec().cmp(&b.column_spec())));
    kept
}
