//! Adversarial acceptance checks for T-0033's pure descriptor/SQL boundary.

use std::collections::BTreeMap;

use dom_render_compiler::forge::declare::{FieldSpec, FieldType};
use dom_render_compiler::forge::query::coerce::{coerce, coerce_in, Coerced, Provenance};
use dom_render_compiler::forge::query::index::{dedup, wanted};
use dom_render_compiler::forge::query::CollectionSchema;
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::transforms::forge_queries::{classify_source, LoweredQuery};
use serde_json::{json, Value};

fn schema() -> CollectionSchema {
    CollectionSchema {
        name: "invoices".into(),
        table: "invoice_rows".into(),
        key: "invoice_key".into(),
        fields: [
            ("amount", FieldType::Real),
            ("due", FieldType::Timestamp),
            ("n", FieldType::Int),
            ("owner", FieldType::Text),
            ("paid", FieldType::Bool),
            ("status", FieldType::Text),
        ]
        .into_iter()
        .map(|(name, ty)| (name.into(), FieldSpec::nullable(ty)))
        .collect(),
        default_order: Vec::new(),
        partition_by: None,
    }
}

fn query(chain: &str) -> LoweredQuery {
    let source = format!(
        "import {{ invoices }} from 'albedo/forge';\nimport {{ or, optional, count, sum, avg, min, max }} from 'albedo/query';\nexport async function read(v) {{ return await {chain}; }}"
    );
    let schemas = BTreeMap::from([("invoices".into(), schema())]);
    let mut result = classify_source("src/review.ts", &source, false, &schemas);
    assert!(
        result.diagnostics.is_empty(),
        "{chain}: {:?}",
        result.diagnostics
    );
    assert_eq!(result.queries.len(), 1, "{chain}");
    result.queries.remove(0)
}

fn binds(q: &LoweredQuery, mask: usize, authored: &[libsql::Value]) -> Vec<libsql::Value> {
    let mut canonical = vec![libsql::Value::Null; q.canonical.slot_map.len()];
    assert_eq!(authored.len(), canonical.len());
    for (source, target) in q.canonical.slot_map.iter().enumerate() {
        canonical[usize::from(*target)] = authored[source].clone();
    }
    q.lowered.statements[mask]
        .binds
        .iter()
        .map(|slot| canonical[usize::from(*slot)].clone())
        .collect()
}

#[test]
fn coercion_checks_i64_edges_every_provenance_and_malformed_real_strings() {
    for ty in [FieldType::Int, FieldType::Timestamp] {
        for provenance in [
            Provenance::Code,
            Provenance::Principal,
            Provenance::RouteParam,
        ] {
            for number in [i64::MIN, i64::MAX, 9_007_199_254_740_993] {
                assert_eq!(
                    coerce(&json!(number.to_string()), ty, provenance, "n").unwrap(),
                    Coerced::Bind(SqlValue::Integer(number))
                );
            }
            for value in [
                json!("9223372036854775808"),
                json!("-9223372036854775809"),
                json!("+1"),
                json!("\u{0661}"),
                json!([]),
                json!({}),
            ] {
                let result = coerce(&value, ty, provenance, "n");
                if provenance == Provenance::RouteParam {
                    assert_eq!(result.unwrap(), Coerced::NoMatch, "{value}");
                } else {
                    assert!(result.is_err(), "{value}: {provenance:?}");
                }
            }
        }
    }
    for bad in [
        "+1", "01", "1.", ".1", "1e", "1e+", "1e309", "NaN", "Infinity", " 1", "1\n", "--1", "0x10",
    ] {
        assert!(
            coerce(&json!(bad), FieldType::Real, Provenance::Code, "amount").is_err(),
            "{bad}"
        );
        assert_eq!(
            coerce(
                &json!(bad),
                FieldType::Real,
                Provenance::RouteParam,
                "amount"
            )
            .unwrap(),
            Coerced::NoMatch
        );
    }
    for good in ["-0", "1e-3", "1E+3", "0.5", "-2.5"] {
        assert!(
            matches!(
                coerce(&json!(good), FieldType::Real, Provenance::Code, "amount"),
                Ok(Coerced::Bind(SqlValue::Real(_)))
            ),
            "{good}"
        );
    }
    for bad in [
        json!(0),
        json!(1),
        json!("TRUE"),
        json!("False"),
        json!("1"),
    ] {
        assert!(coerce(&bad, FieldType::Bool, Provenance::Code, "paid").is_err());
        assert_eq!(
            coerce(&bad, FieldType::Bool, Provenance::RouteParam, "paid").unwrap(),
            Coerced::NoMatch
        );
    }
    for text in ["true", "false"] {
        assert!(coerce(&json!(text), FieldType::Bool, Provenance::Principal, "paid").is_err());
    }
}

#[test]
fn in_array_cap_is_inclusive_and_invalid_route_elements_cannot_widen_it() {
    let values: Vec<Value> = (0..1000).map(|n| json!(n.to_string())).collect();
    let encoded = coerce_in(&values, FieldType::Int, Provenance::Code, "n")
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<i64>>(&encoded).unwrap(),
        (0..1000).collect::<Vec<_>>()
    );
    let mut oversized = values;
    oversized.push(json!("invalid"));
    assert!(coerce_in(&oversized, FieldType::Int, Provenance::RouteParam, "n").is_err());
    assert_eq!(
        coerce_in(&[], FieldType::Int, Provenance::Code, "n").unwrap(),
        None
    );
    assert_eq!(
        coerce_in(
            &[json!("x"), json!("00")],
            FieldType::Int,
            Provenance::RouteParam,
            "n"
        )
        .unwrap(),
        None
    );
    assert_eq!(
        coerce_in(
            &[
                json!("x"),
                json!("-9223372036854775808"),
                json!("9223372036854775807")
            ],
            FieldType::Int,
            Provenance::RouteParam,
            "n"
        )
        .unwrap()
        .unwrap(),
        "[-9223372036854775808,9223372036854775807]"
    );
    assert!(coerce_in(
        &[json!(true), json!(null)],
        FieldType::Bool,
        Provenance::RouteParam,
        "paid"
    )
    .is_err());
}

#[test]
fn nested_or_permutations_share_identity_but_preserve_authored_slot_maps() {
    let a = query("invoices.where(or({ status: v.a, owner: v.b }, { n: v.c, due: { lt: v.d, gt: v.e } })).where({ paid: optional(v.f) })");
    let b = query("invoices.where({ paid: optional(v.f) }).where(or({ due: { gt: v.e, lt: v.d }, n: v.c }, { owner: v.b, status: v.a }))");
    assert_eq!(a.canonical.json, b.canonical.json);
    assert_eq!(a.canonical.id, b.canonical.id);
    let av = [1, 2, 3, 4, 5, 6].map(libsql::Value::Integer);
    let bv = [6, 5, 4, 3, 2, 1].map(libsql::Value::Integer);
    for mask in 0..2 {
        assert_eq!(
            a.lowered.statements[mask].sql,
            b.lowered.statements[mask].sql
        );
        assert_eq!(binds(&a, mask, &av), binds(&b, mask, &bv));
    }
    let mut all = wanted(&a.canonical.descriptor, &schema());
    all.extend(wanted(&b.canonical.descriptor, &schema()));
    let forward = dedup(all.clone(), &[]);
    all.reverse();
    assert_eq!(forward, dedup(all, &[]));
    let names: Vec<_> = forward.iter().map(|index| index.name()).collect();
    assert_eq!(
        names.len(),
        names
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
}

#[test]
fn optional_masks_keep_bind_numbers_dense_and_each_parameter_typed() {
    let q = query("invoices.where({ status: optional(v.a), due: optional(v.b), paid: optional(v.c) }).orderBy('due', 'desc').limit(1000)");
    assert_eq!(q.lowered.statements.len(), 8);
    assert_eq!(q.lowered.params.len(), 3);
    for (mask, statement) in q.lowered.statements.iter().enumerate() {
        assert_eq!(usize::from(statement.mask), mask);
        assert_eq!(statement.binds.len(), mask.count_ones() as usize);
        for number in 1..=statement.binds.len() {
            assert!(
                statement.sql.contains(&format!("?{number}")),
                "{}",
                statement.sql
            );
        }
        assert!(
            !statement
                .sql
                .contains(&format!("?{}", statement.binds.len() + 1)),
            "{}",
            statement.sql
        );
        for slot in &statement.binds {
            let spec = q
                .lowered
                .params
                .iter()
                .find(|param| param.slot == *slot)
                .unwrap();
            assert_eq!(schema().column_type(&spec.column), Some(spec.ty));
        }
    }
}

#[test]
fn not_null_is_not_a_constant_equality_for_index_sort_derivation() {
    let q = query("invoices.where({ status: { not: null } }).orderBy('status', 'desc').limit(50)");
    let indexes = wanted(&q.canonical.descriptor, &schema());
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].column_spec(), "status DESC, invoice_key DESC");
}

#[test]
fn count_equality_and_range_on_the_same_column_do_not_duplicate_the_index_column() {
    let q = query("invoices.where({ n: v.eq }).where({ n: { gt: v.lo } }).count()");
    let indexes = wanted(&q.canonical.descriptor, &schema());
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].column_spec(), "n");
}

async fn ids(conn: &libsql::Connection, q: &LoweredQuery, authored: &[libsql::Value]) -> Vec<i64> {
    let mut rows = conn
        .query(&q.lowered.statements[0].sql, binds(q, 0, authored))
        .await
        .unwrap();
    let mut result = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        result.push(row.get::<i64>(0).unwrap());
    }
    result
}

#[tokio::test]
async fn lowered_null_range_or_group_and_exact_in_semantics_on_libsql() {
    // Keep one database per binary; libsql on this Windows rig crashes with
    // several independently-owned in-process substrates in the same suite.
    let dir = tempfile::tempdir().unwrap();
    let db = libsql::Builder::new_local(dir.path().join("review.db"))
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE invoice_rows(invoice_key INTEGER PRIMARY KEY, amount REAL, due INTEGER, n INTEGER, owner TEXT, paid INTEGER, status TEXT)", ()).await.unwrap();
    conn.execute("INSERT INTO invoice_rows VALUES (3,NULL,10,9007199254740993,'x',0,'b'),(1,NULL,10,42,'x',1,'a'),(2,8,10,9223372036854775807,'y',1,'b'),(4,NULL,30,NULL,NULL,NULL,NULL),(5,4,20,-9223372036854775808,'x',0,'a'),(6,NULL,30,7,'y',1,'c')", ()).await.unwrap();

    assert_eq!(
        ids(&conn, &query("invoices.where({ status: null })"), &[]).await,
        vec![4]
    );
    assert_eq!(
        ids(
            &conn,
            &query("invoices.where({ status: { not: null } }).orderBy('status', 'desc')"),
            &[]
        )
        .await,
        vec![6, 3, 2, 5, 1]
    );
    assert_eq!(
        ids(
            &conn,
            &query("invoices.where({ due: { lt: v.hi, gte: v.lo } }).orderBy('due', 'desc')"),
            &[libsql::Value::Integer(30), libsql::Value::Integer(10)]
        )
        .await,
        vec![5, 3, 2, 1]
    );
    assert_eq!(ids(&conn, &query("invoices.where(or({ status: v.a, owner: v.b }, { n: v.c, due: v.d })).where(or({ paid: v.e }, { due: v.f }))"), &[libsql::Value::Text("a".into()), libsql::Value::Text("x".into()), libsql::Value::Integer(7), libsql::Value::Integer(30), libsql::Value::Integer(1), libsql::Value::Integer(20)]).await, vec![1, 5, 6]);
    let encoded = coerce_in(
        &[
            json!("9007199254740993"),
            json!("9223372036854775807"),
            json!("-9223372036854775808"),
        ],
        FieldType::Int,
        Provenance::Code,
        "n",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        ids(
            &conn,
            &query("invoices.where({ n: { in: v } })"),
            &[libsql::Value::Text(encoded)]
        )
        .await,
        vec![2, 3, 5]
    );
    assert!(ids(
        &conn,
        &query("invoices.get(v)"),
        &[libsql::Value::Integer(999)]
    )
    .await
    .is_empty());
    assert!(ids(
        &conn,
        &query("invoices.where({ status: v }).first()"),
        &[libsql::Value::Text("absent".into())]
    )
    .await
    .is_empty());

    let count_query = query("invoices.where({ status: v }).count()");
    let mut rows = conn
        .query(
            &count_query.lowered.statements[0].sql,
            binds(&count_query, 0, &[libsql::Value::Text("absent".into())]),
        )
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        0
    );
    assert!(rows.next().await.unwrap().is_none());
    let grouped = query("invoices.where({ status: v }).groupBy('status', { a: avg('amount'), b: count(), c: max('amount'), d: min('amount'), e: sum('amount') })");
    let mut rows = conn
        .query(
            &grouped.lowered.statements[0].sql,
            binds(&grouped, 0, &[libsql::Value::Text("c".into())]),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<String>(0).unwrap(), "c");
    assert_eq!(row.get::<i64>(2).unwrap(), 1);
    for column in [1, 3, 4, 5] {
        assert_eq!(row.get_value(column).unwrap(), libsql::Value::Null);
    }
    assert!(rows.next().await.unwrap().is_none());
    let mut empty = conn
        .query(
            &grouped.lowered.statements[0].sql,
            binds(&grouped, 0, &[libsql::Value::Text("absent".into())]),
        )
        .await
        .unwrap();
    assert!(empty.next().await.unwrap().is_none());

    let sorted =
        query("invoices.where({ status: { not: null } }).orderBy('status', 'desc').limit(50)");
    for index in wanted(&sorted.canonical.descriptor, &schema()) {
        conn.execute(&index.ddl(), ()).await.unwrap();
    }
    let fresh = db.connect().unwrap();
    let mut plan = fresh
        .query(
            &format!("EXPLAIN QUERY PLAN {}", sorted.lowered.statements[0].sql),
            (),
        )
        .await
        .unwrap();
    let mut details = Vec::new();
    while let Some(row) = plan.next().await.unwrap() {
        details.push(row.get::<String>(3).unwrap());
    }
    assert!(
        !details.iter().any(|detail| detail.contains("TEMP B-TREE")),
        "NOT NULL is a range, not constant equality: {details:?}"
    );
}
