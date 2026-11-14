//! T-0033 · Q1a — the query descriptor: what a new-style read *is*, separate
//! from where it was written.
//!
//! A descriptor is built from an authored chain
//! (`invoices.where({ status: "open" }).orderBy("due", "desc").limit(50)`)
//! against the collection's declared schema, validated, and put in canonical
//! form. **Identity is the canonical JSON only** (`q_` + 16 hex of its sha256),
//! so two identical queries in two files share one descriptor; where each was
//! written, and which source-order argument feeds which parameter slot, is
//! call-site metadata ([`Canonical::slot_map`]), never part of the identity.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
use crate::forge::skeleton::{parse_order_by, SortDir};

/// Most optional slots one descriptor may carry (2^3 = 8 presence masks).
pub const MAX_OPTIONAL_SLOTS: usize = 3;
/// `limit(n)` accepts `1..=MAX_LIMIT`.
pub const MAX_LIMIT: u32 = 1000;
/// `or(…)` takes this many branches at most (and at least two).
pub const MAX_OR_BRANCHES: usize = 4;

/// Sort direction of one `ORDER BY` term.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Asc,
    Desc,
}

/// One bound of a range term. `param` is the parameter slot that supplies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeBound {
    pub param: u16,
    pub inclusive: bool,
}

/// What a term tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TermOp {
    /// `col IS ?` (null-safe equality).
    Eq { param: u16 },
    /// `col IS NULL` (an authored `null`).
    IsNull,
    /// `col IS NOT NULL` (`{ not: null }`).
    NotNull,
    /// `col IN (SELECT value FROM json_each(?))`.
    In { param: u16 },
    /// One or two bounds on one column.
    Range {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        lo: Option<RangeBound>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        hi: Option<RangeBound>,
    },
}

impl TermOp {
    fn params(&self) -> Vec<u16> {
        match self {
            Self::Eq { param } | Self::In { param } => vec![*param],
            Self::Range { lo, hi } => lo.iter().chain(hi.iter()).map(|b| b.param).collect(),
            Self::IsNull | Self::NotNull => Vec::new(),
        }
    }

    fn params_mut(&mut self) -> Vec<&mut u16> {
        match self {
            Self::Eq { param } | Self::In { param } => vec![param],
            Self::Range { lo, hi } => lo
                .iter_mut()
                .chain(hi.iter_mut())
                .map(|b| &mut b.param)
                .collect(),
            Self::IsNull | Self::NotNull => Vec::new(),
        }
    }
}

/// One predicate term on one column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Term {
    pub column: String,
    #[serde(flatten)]
    pub op: TermOp,
    /// Set when the term is present only if its slot was set at run time
    /// (`optional(x)`, or an `if (…) q = q.where(…)` statement).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub optional_slot: Option<u8>,
}

/// One `ORDER BY` term.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderTerm {
    pub column: String,
    pub dir: Direction,
}

/// How many rows come back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowBound {
    None,
    Limit { n: u32 },
    First,
}

/// An aggregate in `groupBy` (or the whole-query `count()`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fn", rename_all = "snake_case")]
pub enum Aggregate {
    Count,
    Sum { column: String },
    Avg { column: String },
    Min { column: String },
    Max { column: String },
}

/// What a run returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shape {
    /// Rows (`Row[]`, or `Row | null` with [`RowBound::First`]).
    Rows,
    /// `count()` → a number.
    Count,
    /// `groupBy(column, { alias: aggregate })` → `{ [column], ...aliases }[]`.
    Group {
        column: String,
        aggs: BTreeMap<String, Aggregate>,
    },
    /// `get(key)` → `Row | null`, by the collection's key.
    Get { param: u16 },
}

/// Request-time read, or a live (`useSharedSlot`) query (Q3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Request,
    Live,
}

/// The descriptor. Field order is the canonical JSON's field order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryDescriptor {
    pub root: String,
    pub terms: Vec<Term>,
    /// AND of OR groups; each group is 2..=4 branches; each branch is an AND
    /// of terms.
    pub or_groups: Vec<Vec<Vec<Term>>>,
    pub order: Vec<OrderTerm>,
    pub bound: RowBound,
    pub shape: Shape,
    /// Reserved for Q1r (references). Always empty in Q1a.
    pub includes: Vec<String>,
    /// Reserved for Q1r (relation filters). Always empty in Q1a.
    pub relation_terms: Vec<String>,
    pub mode: Mode,
}

/// What a collection looks like to the query layer: the declared schema,
/// lowered the same way FORGE lowers it (table and key overrides honoured).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSchema {
    pub name: String,
    pub table: String,
    pub key: String,
    pub fields: BTreeMap<String, FieldSpec>,
    /// The collection's declared `order_by`, parsed (key ascending when none).
    pub default_order: Vec<OrderTerm>,
    pub partition_by: Option<String>,
}

impl CollectionSchema {
    /// From a `forge` block entry. `name` is the collection's key in the block.
    #[must_use]
    pub fn from_decl(name: &str, decl: &CollectionDecl) -> Self {
        let table = decl.table.clone().unwrap_or_else(|| name.to_string());
        let key = decl.key.clone().unwrap_or_else(|| "id".to_string());
        let default_order = match decl.order_by.as_deref() {
            Some(raw) => parse_order_by(&format!("SELECT 1 ORDER BY {raw}"))
                .iter()
                .map(|term| OrderTerm {
                    column: term.column.clone(),
                    dir: match term.dir {
                        SortDir::Asc => Direction::Asc,
                        SortDir::Desc => Direction::Desc,
                    },
                })
                .collect(),
            None => Vec::new(),
        };
        Self {
            name: name.to_string(),
            table,
            key,
            fields: decl.fields.clone(),
            default_order,
            partition_by: decl.partition_by.clone(),
        }
    }

    /// The type of `column`: a declared field, or the key (always an int).
    #[must_use]
    pub fn column_type(&self, column: &str) -> Option<FieldType> {
        if column == self.key {
            return Some(FieldType::Int);
        }
        self.fields.get(column).map(|spec| spec.ty)
    }
}

/// Why a descriptor is refused. Positions are added by the caller (the
/// classification pass knows file:line:column; this module doesn't).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("`{collection}` has no column `{column}` (declared: {declared})")]
    UnknownColumn {
        collection: String,
        column: String,
        declared: String,
    },
    #[error("`{column}` is sorted twice")]
    DuplicateSort { column: String },
    #[error(
        "`orderBy(\"{after}\")` after `orderBy(\"{key}\")` can never change the order: `{key}` is unique"
    )]
    SortAfterKey { key: String, after: String },
    #[error("limit({n}) is out of range: use 1..={MAX_LIMIT}")]
    LimitOutOfRange { n: u64 },
    #[error("or(…) takes 2 to {MAX_OR_BRANCHES} predicate objects; this one has {count}")]
    OrArity { count: usize },
    #[error(
        "this query has {count} optional parts; at most {MAX_OPTIONAL_SLOTS} are allowed (raw SQL arrives with Q1b)"
    )]
    TooManyOptionalSlots { count: usize },
    #[error("`{aggregate}` needs a number or timestamp column; `{column}` is {ty}")]
    AggregateType {
        aggregate: &'static str,
        column: String,
        ty: &'static str,
    },
    #[error("`{column}` is a bool; ranges compare numbers, timestamps or text")]
    RangeOnBool { column: String },
    #[error("the alias `{alias}` is used twice, or equals the group column")]
    AliasClash { alias: String },
    #[error(
        "the alias `{alias}` must match ^[A-Za-z_][A-Za-z0-9_]{{0,63}}$ (it never reaches SQL, but it names a field)"
    )]
    BadAlias { alias: String },
}

/// A descriptor in canonical form, its id, and the map from the authored
/// (source-order) parameter numbers to the canonical ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical {
    pub descriptor: QueryDescriptor,
    pub json: String,
    pub id: String,
    /// `slot_map[authored] = canonical`.
    pub slot_map: Vec<u16>,
    /// `optional_map[authored] = canonical` for optional slots.
    pub optional_map: Vec<u8>,
}

impl QueryDescriptor {
    /// Check every column, type and chain rule against `schema`, apply the key
    /// tie-break, and put the descriptor in canonical form.
    ///
    /// # Errors
    /// [`QueryError`] naming the first rule the query breaks.
    pub fn finish(mut self, schema: &CollectionSchema) -> Result<Canonical, QueryError> {
        self.validate(schema)?;
        self.apply_tie_break(schema);
        Ok(self.canonicalize())
    }

    fn validate(&self, schema: &CollectionSchema) -> Result<(), QueryError> {
        let known = |column: &str| -> Result<FieldType, QueryError> {
            schema.column_type(column).ok_or_else(|| QueryError::UnknownColumn {
                collection: schema.name.clone(),
                column: column.to_string(),
                declared: std::iter::once(schema.key.as_str())
                    .chain(schema.fields.keys().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(", "),
            })
        };
        let check_term = |term: &Term| -> Result<(), QueryError> {
            let ty = known(&term.column)?;
            if matches!(term.op, TermOp::Range { .. }) && ty == FieldType::Bool {
                return Err(QueryError::RangeOnBool {
                    column: term.column.clone(),
                });
            }
            Ok(())
        };
        for term in &self.terms {
            check_term(term)?;
        }
        for group in &self.or_groups {
            if !(2..=MAX_OR_BRANCHES).contains(&group.len()) {
                return Err(QueryError::OrArity { count: group.len() });
            }
            for term in group.iter().flatten() {
                check_term(term)?;
            }
        }

        let mut seen = Vec::new();
        let mut key_sorted = false;
        for term in &self.order {
            known(&term.column)?;
            if key_sorted {
                return Err(QueryError::SortAfterKey {
                    key: schema.key.clone(),
                    after: term.column.clone(),
                });
            }
            if seen.contains(&term.column) {
                return Err(QueryError::DuplicateSort {
                    column: term.column.clone(),
                });
            }
            seen.push(term.column.clone());
            key_sorted = term.column == schema.key;
        }

        if let RowBound::Limit { n } = self.bound {
            if !(1..=MAX_LIMIT).contains(&n) {
                return Err(QueryError::LimitOutOfRange { n: u64::from(n) });
            }
        }

        let optional = self.optional_slot_count();
        if optional > MAX_OPTIONAL_SLOTS {
            return Err(QueryError::TooManyOptionalSlots { count: optional });
        }

        if let Shape::Group { column, aggs } = &self.shape {
            known(column)?;
            for (alias, aggregate) in aggs {
                if !valid_alias(alias) {
                    return Err(QueryError::BadAlias {
                        alias: alias.clone(),
                    });
                }
                if alias == column {
                    return Err(QueryError::AliasClash {
                        alias: alias.clone(),
                    });
                }
                let (name, target, numeric_only) = match aggregate {
                    Aggregate::Count => continue,
                    Aggregate::Sum { column } => ("sum", column, true),
                    Aggregate::Avg { column } => ("avg", column, true),
                    Aggregate::Min { column } => ("min", column, false),
                    Aggregate::Max { column } => ("max", column, false),
                };
                let ty = known(target)?;
                let ok = match ty {
                    FieldType::Int | FieldType::Real | FieldType::Timestamp => true,
                    FieldType::Text => !numeric_only,
                    FieldType::Bool => false,
                };
                if !ok {
                    return Err(QueryError::AggregateType {
                        aggregate: name,
                        column: target.clone(),
                        ty: type_name(ty),
                    });
                }
            }
        }
        Ok(())
    }

    fn optional_slot_count(&self) -> usize {
        let mut slots: Vec<u8> = self
            .terms
            .iter()
            .chain(self.or_groups.iter().flatten().flatten())
            .filter_map(|t| t.optional_slot)
            .collect();
        slots.sort_unstable();
        slots.dedup();
        slots.len()
    }

    /// Design 5 / R4-1: rows with equal sort values come back ordered by the
    /// key. The key is appended (in the direction of the last explicit term)
    /// unless the author sorted by it — then the authored order stands. With
    /// no authored order, the collection's declared order applies first.
    fn apply_tie_break(&mut self, schema: &CollectionSchema) {
        if !matches!(self.shape, Shape::Rows) {
            // count / group / get have no row order of their own.
            self.order.clear();
            return;
        }
        if self.order.is_empty() {
            self.order = schema.default_order.clone();
        }
        if self.order.iter().any(|t| t.column == schema.key) {
            return;
        }
        let dir = self.order.last().map_or(Direction::Asc, |t| t.dir);
        self.order.push(OrderTerm {
            column: schema.key.clone(),
            dir,
        });
    }

    /// R3-7: terms sorted by their canonical JSON with parameter placeholders
    /// removed; each OR branch and group sorted the same way; then parameters
    /// and optional slots renumbered in that canonical order.
    fn canonicalize(mut self) -> Canonical {
        let shape_key = |term: &Term| {
            let mut bare = term.clone();
            for param in bare.op.params_mut() {
                *param = 0;
            }
            bare.optional_slot = bare.optional_slot.map(|_| 0);
            serde_json::to_string(&bare).unwrap_or_default()
        };
        self.terms.sort_by_key(|t| shape_key(t));
        for group in &mut self.or_groups {
            for branch in group.iter_mut() {
                branch.sort_by_key(|t| shape_key(t));
            }
            group.sort_by_key(|branch| branch.iter().map(shape_key).collect::<Vec<_>>());
        }
        self.or_groups
            .sort_by_key(|g| g.iter().map(|b| b.iter().map(shape_key).collect::<Vec<_>>()).collect::<Vec<_>>());

        // Renumber in canonical traversal order: terms, then OR groups, then the
        // `get` key.
        let mut authored_params: Vec<u16> = Vec::new();
        let mut authored_optional: Vec<u8> = Vec::new();
        let mut visit = |term: &Term| {
            for param in term.op.params() {
                if !authored_params.contains(&param) {
                    authored_params.push(param);
                }
            }
            if let Some(slot) = term.optional_slot {
                if !authored_optional.contains(&slot) {
                    authored_optional.push(slot);
                }
            }
        };
        for term in &self.terms {
            visit(term);
        }
        for term in self.or_groups.iter().flatten().flatten() {
            visit(term);
        }
        if let Shape::Get { param } = &self.shape {
            if !authored_params.contains(param) {
                authored_params.push(*param);
            }
        }

        let max_authored = authored_params.iter().copied().max().map_or(0, |m| m as usize + 1);
        let mut slot_map = vec![u16::MAX; max_authored];
        for (canonical, authored) in authored_params.iter().enumerate() {
            slot_map[*authored as usize] = canonical as u16;
        }
        let max_optional = authored_optional.iter().copied().max().map_or(0, |m| m as usize + 1);
        let mut optional_map = vec![u8::MAX; max_optional];
        for (canonical, authored) in authored_optional.iter().enumerate() {
            optional_map[*authored as usize] = canonical as u8;
        }

        let renumber = |term: &mut Term| {
            for param in term.op.params_mut() {
                *param = slot_map[*param as usize];
            }
            if let Some(slot) = term.optional_slot.as_mut() {
                *slot = optional_map[*slot as usize];
            }
        };
        for term in &mut self.terms {
            renumber(term);
        }
        for term in self.or_groups.iter_mut().flatten().flatten() {
            renumber(term);
        }
        if let Shape::Get { param } = &mut self.shape {
            *param = slot_map[*param as usize];
        }

        let json = serde_json::to_string(&self).unwrap_or_default();
        let digest = Sha256::digest(json.as_bytes());
        let id = format!("q_{}", &hex(&digest)[..16]);
        Canonical {
            descriptor: self,
            json,
            id,
            slot_map,
            optional_map,
        }
    }
}

/// `^[A-Za-z_][A-Za-z0-9_]{0,63}$` (R3-6), checked by hand.
#[must_use]
pub fn valid_alias(alias: &str) -> bool {
    let mut chars = alias.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && alias.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn type_name(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Text => "text",
        FieldType::Int => "int",
        FieldType::Real => "real",
        FieldType::Bool => "bool",
        FieldType::Timestamp => "timestamp",
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
