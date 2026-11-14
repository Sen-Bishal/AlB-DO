//! T-0033 · unit tests for the pure query core.

use std::collections::BTreeMap;

use serde_json::json;

use super::coerce::{coerce, coerce_in, Coerced, Provenance};
use super::descriptor::*;
use super::index::{dedup, wanted, IndexColumn, IndexSpec};
use super::lower::lower;
use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
use crate::forge::value::SqlValue;

fn invoices() -> CollectionSchema {
    let mut fields = BTreeMap::new();
    for (name, ty) in [
        ("amount", FieldType::Real),
        ("due", FieldType::Timestamp),
        ("owner", FieldType::Text),
        ("paid", FieldType::Bool),
        ("status", FieldType::Text),
    ] {
        fields.insert(name.to_string(), FieldSpec::new(ty));
    }
    CollectionSchema {
        name: "invoices".into(),
        table: "inv_t".into(),
        key: "inv_id".into(),
        fields,
        default_order: Vec::new(),
        partition_by: None,
    }
}

fn eq(column: &str, param: u16) -> Term {
    Term {
        column: column.into(),
        op: TermOp::Eq { param },
        optional_slot: None,
    }
}

fn rows(terms: Vec<Term>, order: Vec<(&str, Direction)>, bound: RowBound) -> QueryDescriptor {
    QueryDescriptor {
        root: "invoices".into(),
        terms,
        or_groups: Vec::new(),
        order: order
            .into_iter()
            .map(|(c, d)| OrderTerm {
                column: c.into(),
                dir: d,
            })
            .collect(),
        bound,
        shape: Shape::Rows,
        includes: Vec::new(),
        relation_terms: Vec::new(),
        mode: Mode::Request,
    }
}

// ── Identity (Acceptance 2) ─────────────────────────────────────────────

#[test]
fn two_sites_writing_keys_in_different_orders_share_one_id_with_their_own_slot_maps() {
    let schema = invoices();
    // Site A: where({ status: a0, owner: a1 }); site B: where({ owner: b0, status: b1 }).
    let a = rows(vec![eq("status", 0), eq("owner", 1)], vec![], RowBound::Limit { n: 50 })
        .finish(&schema)
        .unwrap();
    let b = rows(vec![eq("owner", 0), eq("status", 1)], vec![], RowBound::Limit { n: 50 })
        .finish(&schema)
        .unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(a.json, b.json);
    assert!(a.id.starts_with("q_") && a.id.len() == 18, "{}", a.id);
    // Canonical order puts `owner` before `status`.
    assert_eq!(a.slot_map, vec![1, 0], "site A: status→1, owner→0");
    assert_eq!(b.slot_map, vec![0, 1], "site B: owner→0, status→1");
    // Byte-identical across runs.
    let again = rows(vec![eq("status", 0), eq("owner", 1)], vec![], RowBound::Limit { n: 50 })
        .finish(&schema)
        .unwrap();
    assert_eq!(a.json, again.json);
}

#[test]
fn different_shapes_get_different_ids() {
    let schema = invoices();
    let a = rows(vec![eq("status", 0)], vec![], RowBound::Limit { n: 50 }).finish(&schema).unwrap();
    let b = rows(vec![eq("status", 0)], vec![], RowBound::Limit { n: 51 }).finish(&schema).unwrap();
    assert_ne!(a.id, b.id);
}

// ── Ordering rules (Design 5/6, R4-1) ───────────────────────────────────

#[test]
fn the_key_is_appended_as_tie_break_in_the_last_terms_direction() {
    let schema = invoices();
    let c = rows(vec![], vec![("due", Direction::Desc)], RowBound::None).finish(&schema).unwrap();
    let sql = &lower(&c.descriptor, &schema).statements[0].sql;
    assert!(sql.ends_with("ORDER BY due DESC, inv_id DESC"), "{sql}");
}

#[test]
fn an_explicit_key_sort_keeps_authored_order_and_nothing_is_appended() {
    let schema = invoices();
    let c = rows(vec![], vec![("inv_id", Direction::Asc)], RowBound::First).finish(&schema).unwrap();
    let sql = &lower(&c.descriptor, &schema).statements[0].sql;
    assert!(sql.ends_with("ORDER BY inv_id LIMIT 1"), "{sql}");
}

#[test]
fn a_sort_after_an_explicit_key_sort_is_refused() {
    let err = rows(
        vec![],
        vec![("inv_id", Direction::Asc), ("due", Direction::Asc)],
        RowBound::Limit { n: 1 },
    )
    .finish(&invoices())
    .unwrap_err();
    assert!(matches!(err, QueryError::SortAfterKey { .. }), "{err}");
    assert!(err.to_string().contains("can never change the order"), "{err}");
}

#[test]
fn sorting_one_column_twice_is_refused() {
    let err = rows(vec![], vec![("due", Direction::Asc), ("due", Direction::Desc)], RowBound::None)
        .finish(&invoices())
        .unwrap_err();
    assert!(matches!(err, QueryError::DuplicateSort { .. }), "{err}");
}

#[test]
fn no_authored_order_uses_the_collections_declared_order_then_the_key() {
    let mut schema = invoices();
    schema.default_order = vec![OrderTerm {
        column: "due".into(),
        dir: Direction::Asc,
    }];
    let c = rows(vec![], vec![], RowBound::None).finish(&schema).unwrap();
    let sql = &lower(&c.descriptor, &schema).statements[0].sql;
    assert!(sql.ends_with("ORDER BY due, inv_id"), "{sql}");
}

// ── Lowering ────────────────────────────────────────────────────────────

#[test]
fn lowering_honours_table_and_key_overrides_and_binds_every_value() {
    let schema = invoices();
    let mut d = rows(vec![eq("status", 0)], vec![("due", Direction::Desc)], RowBound::Limit { n: 50 });
    d.terms.push(Term {
        column: "due".into(),
        op: TermOp::Range {
            lo: None,
            hi: Some(RangeBound {
                param: 1,
                inclusive: false,
            }),
        },
        optional_slot: None,
    });
    let c = d.finish(&schema).unwrap();
    let low = lower(&c.descriptor, &schema);
    assert_eq!(low.statements.len(), 1);
    let s = &low.statements[0];
    assert_eq!(
        s.sql,
        "SELECT inv_id, amount, due, owner, paid, status FROM inv_t WHERE due < ?1 AND status IS ?2 ORDER BY due DESC, inv_id DESC LIMIT 50"
    );
    assert_eq!(s.binds.len(), 2);
}

#[test]
fn optional_slots_produce_one_statement_per_presence_mask() {
    let schema = invoices();
    let mut d = rows(vec![eq("status", 0)], vec![], RowBound::Limit { n: 10 });
    d.terms.push(Term {
        column: "owner".into(),
        op: TermOp::Eq { param: 1 },
        optional_slot: Some(0),
    });
    let c = d.finish(&schema).unwrap();
    let low = lower(&c.descriptor, &schema);
    assert_eq!(low.statements.len(), 2);
    assert!(!low.statements[0].sql.contains("owner IS"), "{}", low.statements[0].sql);
    assert!(low.statements[1].sql.contains("owner IS"), "{}", low.statements[1].sql);
    assert_eq!(low.statements[0].binds.len(), 1);
    assert_eq!(low.statements[1].binds.len(), 2);
}

#[test]
fn four_optional_slots_are_refused() {
    let mut d = rows(vec![], vec![], RowBound::None);
    for (i, column) in ["status", "owner", "amount", "due"].iter().enumerate() {
        d.terms.push(Term {
            column: (*column).into(),
            op: TermOp::Eq { param: i as u16 },
            optional_slot: Some(i as u8),
        });
    }
    assert!(matches!(
        d.finish(&invoices()).unwrap_err(),
        QueryError::TooManyOptionalSlots { count: 4 }
    ));
}

#[test]
fn or_groups_get_explicit_parentheses_at_both_levels() {
    // R4-3: ((a AND b) OR (c AND d)) AND (e OR f)
    let schema = invoices();
    let mut d = rows(vec![], vec![], RowBound::None);
    d.or_groups = vec![
        vec![vec![eq("status", 0), eq("owner", 1)], vec![eq("paid", 2), eq("amount", 3)]],
        vec![vec![eq("due", 4)], vec![eq("inv_id", 5)]],
    ];
    let c = d.finish(&schema).unwrap();
    let sql = &lower(&c.descriptor, &schema).statements[0].sql;
    assert!(sql.contains(") OR ("), "{sql}");
    let where_part = sql.split(" WHERE ").nth(1).unwrap().split(" ORDER BY ").next().unwrap();
    assert!(where_part.starts_with("(("), "{where_part}");
    assert_eq!(where_part.matches(") AND (").count(), 1, "{where_part}");
}

#[test]
fn or_arity_is_two_to_four() {
    let mut d = rows(vec![], vec![], RowBound::None);
    d.or_groups = vec![vec![vec![eq("status", 0)]]];
    assert!(matches!(d.finish(&invoices()).unwrap_err(), QueryError::OrArity { count: 1 }));
}

#[test]
fn a_two_bound_range_maps_each_bound_to_its_own_slot() {
    // R4-4: `{ due: { lt: g(), gt: f() } }` written lt-first: g → slot 0 (hi), f → slot 1 (lo).
    let schema = invoices();
    let mut d = rows(vec![], vec![], RowBound::None);
    d.terms.push(Term {
        column: "due".into(),
        op: TermOp::Range {
            lo: Some(RangeBound {
                param: 1,
                inclusive: false,
            }),
            hi: Some(RangeBound {
                param: 0,
                inclusive: false,
            }),
        },
        optional_slot: None,
    });
    let c = d.finish(&schema).unwrap();
    let low = lower(&c.descriptor, &schema);
    let s = &low.statements[0];
    assert!(s.sql.contains("due > ?1 AND due < ?2"), "{}", s.sql);
    // ?1 binds the lower bound, which the author wrote second (authored slot 1).
    let lo_canonical = c.slot_map[1];
    let hi_canonical = c.slot_map[0];
    assert_eq!(s.binds, vec![lo_canonical, hi_canonical]);
}

#[test]
fn count_group_and_get_shapes() {
    let schema = invoices();
    let mut count = rows(vec![eq("status", 0)], vec![], RowBound::None);
    count.shape = Shape::Count;
    let c = count.finish(&schema).unwrap();
    assert_eq!(
        lower(&c.descriptor, &schema).statements[0].sql,
        "SELECT COUNT(*) AS a0 FROM inv_t WHERE status IS ?1"
    );

    let mut group = rows(vec![], vec![], RowBound::None);
    let mut aggs = BTreeMap::new();
    aggs.insert("n".to_string(), Aggregate::Count);
    aggs.insert(
        "total".to_string(),
        Aggregate::Sum {
            column: "amount".into(),
        },
    );
    group.shape = Shape::Group {
        column: "status".into(),
        aggs,
    };
    let c = group.finish(&schema).unwrap();
    assert_eq!(
        lower(&c.descriptor, &schema).statements[0].sql,
        "SELECT status, COUNT(*) AS a0, SUM(amount) AS a1 FROM inv_t GROUP BY status ORDER BY status"
    );

    let mut get = rows(vec![], vec![], RowBound::None);
    get.shape = Shape::Get { param: 0 };
    let c = get.finish(&schema).unwrap();
    assert_eq!(
        lower(&c.descriptor, &schema).statements[0].sql,
        "SELECT inv_id, amount, due, owner, paid, status FROM inv_t WHERE inv_id = ?1"
    );
}

#[test]
fn aggregate_and_alias_rules() {
    let schema = invoices();
    let group = |alias: &str, aggregate: Aggregate| {
        let mut d = rows(vec![], vec![], RowBound::None);
        let mut aggs = BTreeMap::new();
        aggs.insert(alias.to_string(), aggregate);
        d.shape = Shape::Group {
            column: "status".into(),
            aggs,
        };
        d.finish(&schema)
    };
    assert!(matches!(
        group("x", Aggregate::Sum { column: "owner".into() }).unwrap_err(),
        QueryError::AggregateType { .. }
    ));
    assert!(group("x", Aggregate::Min { column: "owner".into() }).is_ok());
    assert!(matches!(
        group("status", Aggregate::Count).unwrap_err(),
        QueryError::AliasClash { .. }
    ));
    assert!(matches!(
        group("bad alias", Aggregate::Count).unwrap_err(),
        QueryError::BadAlias { .. }
    ));
}

#[test]
fn limit_range_and_unknown_columns() {
    let schema = invoices();
    assert!(matches!(
        rows(vec![], vec![], RowBound::Limit { n: 0 }).finish(&schema).unwrap_err(),
        QueryError::LimitOutOfRange { .. }
    ));
    assert!(matches!(
        rows(vec![], vec![], RowBound::Limit { n: 1001 }).finish(&schema).unwrap_err(),
        QueryError::LimitOutOfRange { .. }
    ));
    let err = rows(vec![eq("nope", 0)], vec![], RowBound::None).finish(&schema).unwrap_err();
    assert!(err.to_string().contains("has no column `nope`"), "{err}");
}

// ── Indexes (Design 11, Falsifiers 2 and 8) ─────────────────────────────

fn col(c: &str, d: Direction) -> IndexColumn {
    IndexColumn {
        column: c.into(),
        dir: d,
    }
}

#[test]
fn esr_index_puts_the_range_column_after_the_sort_columns() {
    // Falsifier 8's fixture: equality status, range amount, order due.
    let schema = invoices();
    let mut d = rows(vec![eq("status", 0)], vec![("due", Direction::Asc)], RowBound::Limit { n: 50 });
    d.terms.push(Term {
        column: "amount".into(),
        op: TermOp::Range {
            lo: Some(RangeBound {
                param: 1,
                inclusive: true,
            }),
            hi: None,
        },
        optional_slot: None,
    });
    let c = d.finish(&schema).unwrap();
    let specs = wanted(&c.descriptor, &schema);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].column_spec(), "status, due, inv_id, amount");
}

#[test]
fn legacy_index_names_drop_directions_and_the_new_scheme_does_not() {
    // Falsifier 2, legacy half: the partition index name has no direction in it.
    let decl = |order: &str| CollectionDecl {
        fields: [
            ("room".to_string(), FieldSpec::new(FieldType::Text)),
            ("b".to_string(), FieldSpec::new(FieldType::Text)),
        ]
        .into_iter()
        .collect(),
        order_by: Some(order.to_string()),
        partition_by: Some("room".to_string()),
        ..CollectionDecl::default()
    };
    let name = |order: &str| {
        let lowered = decl(order).lower("msgs").unwrap();
        let ddl = lowered
            .migrations
            .iter()
            .find(|m| m.starts_with("CREATE INDEX"))
            .unwrap()
            .clone();
        ddl.split_whitespace().nth(5).unwrap().to_string()
    };
    assert_eq!(name("b"), name("b desc"), "legacy naming is direction-blind");

    // New scheme: distinct.
    let a = IndexSpec {
        table: "t".into(),
        columns: vec![col("a", Direction::Asc), col("b", Direction::Asc)],
    };
    let b = IndexSpec {
        table: "t".into(),
        columns: vec![col("a", Direction::Asc), col("b", Direction::Desc)],
    };
    assert_ne!(a.name(), b.name());
}

#[test]
fn dedup_keeps_two_of_a_ab_ab_desc_and_none_already_served_by_a_legacy_index() {
    let t = |cols: Vec<IndexColumn>| IndexSpec {
        table: "t".into(),
        columns: cols,
    };
    let a = t(vec![col("a", Direction::Asc)]);
    let ab = t(vec![col("a", Direction::Asc), col("b", Direction::Asc)]);
    let ab_desc = t(vec![col("a", Direction::Asc), col("b", Direction::Desc)]);
    let kept = dedup(vec![a.clone(), ab.clone(), ab_desc.clone()], &[]);
    assert_eq!(kept.len(), 2);
    assert!(kept.contains(&ab) && kept.contains(&ab_desc));
    assert_ne!(kept[0].name(), kept[1].name());

    let legacy = t(vec![col("a", Direction::Asc), col("b", Direction::Asc)]);
    assert!(dedup(vec![ab, a], &[legacy]).is_empty());
}

// ── Coercion (Design 7, Falsifier 3's Rust half) ────────────────────────

#[test]
fn int_coercion_accepts_exact_integers_and_rejects_everything_else() {
    let code = Provenance::Code;
    let route = Provenance::RouteParam;
    for accepted in [json!(42), json!("42"), json!("9007199254740993"), json!("0"), json!("-7")] {
        assert!(
            matches!(coerce(&accepted, FieldType::Int, code, "n"), Ok(Coerced::Bind(SqlValue::Integer(_)))),
            "{accepted} should be accepted"
        );
    }
    assert_eq!(
        coerce(&json!("9007199254740993"), FieldType::Int, code, "n").unwrap(),
        Coerced::Bind(SqlValue::Integer(9_007_199_254_740_993))
    );
    for rejected in [json!("042"), json!(" 42"), json!("4a"), json!("42.0"), json!("1e3"), json!("-0"), json!(9_007_199_254_740_993_i64), json!(4.5)] {
        assert_eq!(
            coerce(&rejected, FieldType::Int, route, "n").unwrap(),
            Coerced::NoMatch,
            "{rejected} from a route param can't match"
        );
        assert!(coerce(&rejected, FieldType::Int, code, "n").is_err(), "{rejected} from code is an error");
    }
    assert_eq!(coerce(&json!(null), FieldType::Int, code, "n").unwrap(), Coerced::Bind(SqlValue::Null));
}

#[test]
fn text_compared_with_a_number_is_refused_by_rust() {
    // SQLite affinity would otherwise match `t = 42` against "42".
    assert!(coerce(&json!(42), FieldType::Text, Provenance::Code, "t").is_err());
    assert_eq!(
        coerce(&json!(42), FieldType::Text, Provenance::RouteParam, "t").unwrap(),
        Coerced::NoMatch
    );
}

#[test]
fn real_and_bool_coercion() {
    let code = Provenance::Code;
    assert!(matches!(coerce(&json!("1.5e3"), FieldType::Real, code, "x"), Ok(Coerced::Bind(SqlValue::Real(_)))));
    assert!(coerce(&json!("1.5e"), FieldType::Real, code, "x").is_err());
    assert!(coerce(&json!(".5"), FieldType::Real, code, "x").is_err());
    assert_eq!(
        coerce(&json!("true"), FieldType::Bool, Provenance::RouteParam, "b").unwrap(),
        Coerced::Bind(SqlValue::Integer(1))
    );
    assert!(coerce(&json!("true"), FieldType::Bool, code, "b").is_err(), "the string only counts from a route param");
}

#[test]
fn in_arrays_coerce_each_element_and_keep_i64s_exact() {
    let route = Provenance::RouteParam;
    let json_text = coerce_in(&[json!("9007199254740993"), json!("abc"), json!(3)], FieldType::Int, route, "n")
        .unwrap()
        .unwrap();
    assert_eq!(json_text, "[9007199254740993,3]");
    assert_eq!(coerce_in(&[json!("abc")], FieldType::Int, route, "n").unwrap(), None);
    assert!(coerce_in(&[json!("abc")], FieldType::Int, Provenance::Code, "n").is_err());
    assert!(coerce_in(&[json!(null)], FieldType::Int, route, "n").is_err());
    let big: Vec<_> = (0..1001).map(|i| json!(i)).collect();
    assert!(coerce_in(&big, FieldType::Int, Provenance::Code, "n").is_err());
}
