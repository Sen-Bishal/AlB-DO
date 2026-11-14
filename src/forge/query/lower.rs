//! T-0033 · Q1a — lowering a canonical descriptor to SQL.
//!
//! One statement per **presence mask** (which optional slots were set), all
//! parameterised. Every identifier comes from the collection schema, never from
//! source text. Values are only ever bound (`?n`), never spliced.

use super::descriptor::{
    Aggregate, CollectionSchema, Direction, QueryDescriptor, RangeBound, RowBound, Shape, Term,
    TermOp,
};
use crate::forge::declare::FieldType;

/// What one parameter slot feeds, so the runtime (Q1b) can coerce the value
/// before it reaches SQL (`coerce.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    /// Canonical slot number.
    pub slot: u16,
    pub column: String,
    pub ty: FieldType,
    pub role: ParamRole,
}

/// How a parameter is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamRole {
    Eq,
    /// A JSON array of scalars, read through `json_each`.
    In,
    RangeLo,
    RangeHi,
    /// `get(key)`.
    Key,
}

/// One statement: the SQL text and, for each `?n` in order, the canonical
/// slot whose value binds there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskStatement {
    /// Bit `i` set ⇔ optional slot `i` was set at run time.
    pub mask: u8,
    pub sql: String,
    pub binds: Vec<u16>,
}

/// Everything Q1a produces for one descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lowered {
    pub statements: Vec<MaskStatement>,
    pub params: Vec<ParamSpec>,
}

/// Lower `descriptor` (already [`QueryDescriptor::finish`]ed against `schema`).
#[must_use]
pub fn lower(descriptor: &QueryDescriptor, schema: &CollectionSchema) -> Lowered {
    let optional = optional_slots(descriptor);
    let masks = 1u16 << optional;
    let statements = (0..masks)
        .map(|mask| statement(descriptor, schema, mask as u8))
        .collect();
    Lowered {
        statements,
        params: param_specs(descriptor, schema),
    }
}

fn optional_slots(descriptor: &QueryDescriptor) -> u32 {
    descriptor
        .terms
        .iter()
        .chain(descriptor.or_groups.iter().flatten().flatten())
        .filter_map(|t| t.optional_slot)
        .map(|s| u32::from(s) + 1)
        .max()
        .unwrap_or(0)
}

fn present(term: &Term, mask: u8) -> bool {
    term.optional_slot
        .map_or(true, |slot| mask & (1 << slot) != 0)
}

struct Binder {
    binds: Vec<u16>,
}

impl Binder {
    fn next(&mut self, slot: u16) -> String {
        self.binds.push(slot);
        format!("?{}", self.binds.len())
    }
}

fn term_sql(term: &Term, binder: &mut Binder) -> String {
    let column = &term.column;
    match &term.op {
        TermOp::Eq { param } => format!("{column} IS {}", binder.next(*param)),
        TermOp::IsNull => format!("{column} IS NULL"),
        TermOp::NotNull => format!("{column} IS NOT NULL"),
        TermOp::In { param } => format!(
            "{column} IN (SELECT value FROM json_each({}))",
            binder.next(*param)
        ),
        TermOp::Range { lo, hi } => {
            let mut parts = Vec::new();
            let bound = |b: &RangeBound, low: bool, binder: &mut Binder| {
                let op = match (low, b.inclusive) {
                    (true, true) => ">=",
                    (true, false) => ">",
                    (false, true) => "<=",
                    (false, false) => "<",
                };
                format!("{column} {op} {}", binder.next(b.param))
            };
            if let Some(b) = lo {
                parts.push(bound(b, true, binder));
            }
            if let Some(b) = hi {
                parts.push(bound(b, false, binder));
            }
            parts.join(" AND ")
        }
    }
}

fn where_clause(descriptor: &QueryDescriptor, mask: u8, binder: &mut Binder) -> Option<String> {
    let mut conjuncts: Vec<String> = descriptor
        .terms
        .iter()
        .filter(|t| present(t, mask))
        .map(|t| term_sql(t, binder))
        .collect();
    for group in &descriptor.or_groups {
        let branches: Vec<String> = group
            .iter()
            .map(|branch| {
                let terms: Vec<String> = branch
                    .iter()
                    .filter(|t| present(t, mask))
                    .map(|t| term_sql(t, binder))
                    .collect();
                if terms.is_empty() {
                    // A branch whose every term is absent matches everything.
                    "1".to_string()
                } else {
                    format!("({})", terms.join(" AND "))
                }
            })
            .collect();
        // R4-3: explicit parentheses at both levels.
        conjuncts.push(format!("({})", branches.join(" OR ")));
    }
    (!conjuncts.is_empty()).then(|| conjuncts.join(" AND "))
}

fn statement(descriptor: &QueryDescriptor, schema: &CollectionSchema, mask: u8) -> MaskStatement {
    let table = &schema.table;
    let key = &schema.key;
    let mut binder = Binder { binds: Vec::new() };

    let sql = match &descriptor.shape {
        Shape::Get { param } => {
            let columns = select_list(schema);
            let bind = binder.next(*param);
            format!("SELECT {columns} FROM {table} WHERE {key} = {bind}")
        }
        Shape::Rows => {
            let columns = select_list(schema);
            let mut sql = format!("SELECT {columns} FROM {table}");
            if let Some(clause) = where_clause(descriptor, mask, &mut binder) {
                sql.push_str(&format!(" WHERE {clause}"));
            }
            if !descriptor.order.is_empty() {
                sql.push_str(&format!(" ORDER BY {}", order_list(descriptor)));
            }
            match descriptor.bound {
                RowBound::None => {}
                RowBound::Limit { n } => sql.push_str(&format!(" LIMIT {n}")),
                RowBound::First => sql.push_str(" LIMIT 1"),
            }
            sql
        }
        Shape::Count => {
            let mut sql = format!("SELECT COUNT(*) AS a0 FROM {table}");
            if let Some(clause) = where_clause(descriptor, mask, &mut binder) {
                sql.push_str(&format!(" WHERE {clause}"));
            }
            sql
        }
        Shape::Group { column, aggs } => {
            // R3-6: SQL uses positional aliases; Rust maps them back.
            let mut select = vec![column.clone()];
            for (i, aggregate) in aggs.values().enumerate() {
                let expr = match aggregate {
                    Aggregate::Count => "COUNT(*)".to_string(),
                    Aggregate::Sum { column } => format!("SUM({column})"),
                    Aggregate::Avg { column } => format!("AVG({column})"),
                    Aggregate::Min { column } => format!("MIN({column})"),
                    Aggregate::Max { column } => format!("MAX({column})"),
                };
                select.push(format!("{expr} AS a{i}"));
            }
            let mut sql = format!("SELECT {} FROM {table}", select.join(", "));
            if let Some(clause) = where_clause(descriptor, mask, &mut binder) {
                sql.push_str(&format!(" WHERE {clause}"));
            }
            sql.push_str(&format!(" GROUP BY {column} ORDER BY {column}"));
            sql
        }
    };
    MaskStatement {
        mask,
        sql,
        binds: binder.binds,
    }
}

/// Key + declared fields in `BTreeMap` order — the legacy query's SELECT list.
fn select_list(schema: &CollectionSchema) -> String {
    std::iter::once(schema.key.as_str())
        .chain(schema.fields.keys().map(String::as_str))
        .collect::<Vec<_>>()
        .join(", ")
}

fn order_list(descriptor: &QueryDescriptor) -> String {
    descriptor
        .order
        .iter()
        .map(|t| match t.dir {
            Direction::Asc => t.column.clone(),
            Direction::Desc => format!("{} DESC", t.column),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn param_specs(descriptor: &QueryDescriptor, schema: &CollectionSchema) -> Vec<ParamSpec> {
    let mut specs = Vec::new();
    let ty = |column: &str| schema.column_type(column).unwrap_or(FieldType::Text);
    let mut push = |slot: u16, column: &str, role: ParamRole| {
        if !specs.iter().any(|s: &ParamSpec| s.slot == slot) {
            specs.push(ParamSpec {
                slot,
                column: column.to_string(),
                ty: ty(column),
                role,
            });
        }
    };
    for term in descriptor
        .terms
        .iter()
        .chain(descriptor.or_groups.iter().flatten().flatten())
    {
        match &term.op {
            TermOp::Eq { param } => push(*param, &term.column, ParamRole::Eq),
            TermOp::In { param } => push(*param, &term.column, ParamRole::In),
            TermOp::Range { lo, hi } => {
                if let Some(b) = lo {
                    push(b.param, &term.column, ParamRole::RangeLo);
                }
                if let Some(b) = hi {
                    push(b.param, &term.column, ParamRole::RangeHi);
                }
            }
            TermOp::IsNull | TermOp::NotNull => {}
        }
    }
    if let Shape::Get { param } = &descriptor.shape {
        push(*param, &schema.key, ParamRole::Key);
    }
    specs.sort_by_key(|s| s.slot);
    specs
}
