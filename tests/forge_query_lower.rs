//! T-0033 · Q1a — Falsifier 3 / Acceptance 5: Rust decides what reaches SQL.
//!
//! An int column `n` with rows 42, NULL and 9007199254740993, and a text column
//! `t` holding "42". The real lowered statement (`n IS ?1`) runs on libsql
//! behind a counting harness that does what Q1b's executor must: coerce first
//! (`forge::query::coerce`), then bind and run, or short-circuit.
//!
//! - accepted inputs: the rows SQL returns are exactly the rows Rust's
//!   null-safe comparison on the coerced value picks;
//! - rejected inputs: **zero statements** reach the database — an empty result
//!   from a route param, an error from code;
//! - and the danger is real: the same rejected values bound raw *would* match
//!   through SQLite's type affinity.
//!
//! One database, one test function (the access-violation trap,
//! `src/forge/libsql.rs`).

use std::collections::BTreeMap;

use dom_render_compiler::forge::declare::{FieldSpec, FieldType};
use dom_render_compiler::forge::query::coerce::{coerce, Coerced, Provenance};
use dom_render_compiler::forge::query::CollectionSchema;
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::transforms::forge_queries::classify_source;
use serde_json::{json, Value};

fn schema() -> CollectionSchema {
    CollectionSchema {
        name: "items".into(),
        table: "items".into(),
        key: "id".into(),
        fields: [
            ("n".to_string(), FieldSpec::nullable(FieldType::Int)),
            ("t".to_string(), FieldSpec::nullable(FieldType::Text)),
        ]
        .into_iter()
        .collect(),
        default_order: Vec::new(),
        partition_by: None,
    }
}

/// The lowered SQL for `items.where({ <column>: v })`, and its one column type.
fn lowered(column: &str) -> (String, FieldType) {
    let mut schemas = BTreeMap::new();
    schemas.insert("items".to_string(), schema());
    let source = format!(
        "import {{ items }} from \"albedo/forge\";\nexport async function q(v) {{ return await items.where({{ {column}: v }}); }}\n"
    );
    let c = classify_source("src/q.ts", &source, false, &schemas);
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    let q = &c.queries[0];
    assert_eq!(q.lowered.statements.len(), 1);
    (q.lowered.statements[0].sql.clone(), q.lowered.params[0].ty)
}

/// What Q1b's executor does with one equality argument, counting statements.
struct Harness {
    conn: libsql::Connection,
    statements: usize,
}

enum Outcome {
    Rows(Vec<i64>),
    Error(String),
}

impl Harness {
    async fn run(&mut self, sql: &str, ty: FieldType, column: &str, value: &Value, provenance: Provenance) -> Outcome {
        match coerce(value, ty, provenance, column) {
            Err(err) => Outcome::Error(err.to_string()),
            Ok(Coerced::NoMatch) => Outcome::Rows(Vec::new()),
            Ok(Coerced::Bind(bound)) => Outcome::Rows(self.raw(sql, to_libsql(&bound)).await),
        }
    }

    async fn raw(&mut self, sql: &str, value: libsql::Value) -> Vec<i64> {
        self.statements += 1;
        let mut rows = self.conn.query(sql, vec![value]).await.expect("query");
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await.expect("row") {
            ids.push(row.get::<i64>(0).expect("id"));
        }
        ids
    }
}

fn to_libsql(value: &SqlValue) -> libsql::Value {
    match value {
        SqlValue::Null => libsql::Value::Null,
        SqlValue::Integer(i) => libsql::Value::Integer(*i),
        SqlValue::Real(r) => libsql::Value::Real(*r),
        SqlValue::Text(t) => libsql::Value::Text(t.clone()),
        other => panic!("unexpected bind {other:?}"),
    }
}

/// (id, n, t)
const ROWS: &[(i64, Option<i64>, Option<&str>)] = &[
    (1, Some(42), Some("x")),
    (2, None, Some("42")),
    (3, Some(9_007_199_254_740_993), None),
];

/// Rust's verdict: `column IS value`, null-safe, on the coerced value.
fn rust_ids(column: &str, bound: &SqlValue) -> Vec<i64> {
    ROWS.iter()
        .filter(|(_, n, t)| match (column, bound) {
            ("n", SqlValue::Null) => n.is_none(),
            ("n", SqlValue::Integer(k)) => *n == Some(*k),
            ("t", SqlValue::Null) => t.is_none(),
            ("t", SqlValue::Text(s)) => *t == Some(s.as_str()),
            _ => false,
        })
        .map(|(id, _, _)| *id)
        .collect()
}

#[tokio::test]
async fn falsifier_3_rust_coercion_decides_what_reaches_sql() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = libsql::Builder::new_local(dir.path().join("f3.db")).build().await.expect("open");
    let conn = db.connect().expect("connect");
    conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER, t TEXT)", ())
        .await
        .expect("table");
    for (id, n, t) in ROWS {
        conn.execute("INSERT INTO items(id, n, t) VALUES (?1, ?2, ?3)", libsql::params![*id, *n, *t])
            .await
            .expect("row");
    }
    let mut harness = Harness { conn, statements: 0 };
    let (n_sql, n_ty) = lowered("n");
    let (t_sql, t_ty) = lowered("t");
    assert!(n_sql.contains("WHERE n IS ?1"), "{n_sql}");

    // Accepted: SQL agrees with Rust on the coerced value.
    for (value, provenance) in [
        (json!(42), Provenance::Code),
        (json!("42"), Provenance::RouteParam),
        (json!("9007199254740993"), Provenance::RouteParam),
        (json!(null), Provenance::Code),
    ] {
        let Ok(Coerced::Bind(bound)) = coerce(&value, n_ty, provenance, "n") else {
            panic!("{value} must be accepted");
        };
        let before = harness.statements;
        let Outcome::Rows(sql) = harness.run(&n_sql, n_ty, "n", &value, provenance).await else {
            panic!("{value} errored");
        };
        assert_eq!(harness.statements, before + 1);
        assert_eq!(sql, rust_ids("n", &bound), "{value}: SQL vs Rust");
    }
    let Outcome::Rows(sql) = harness.run(&t_sql, t_ty, "t", &json!("42"), Provenance::Code).await else {
        panic!("text accepted");
    };
    assert_eq!(sql, vec![2]);

    // Rejected: zero statements, from a route param and from code alike.
    let rejected_n = [
        json!("042"),
        json!(" 42"),
        json!("4a"),
        json!("42.0"),
        json!("1e3"),
        Value::from(9_007_199_254_740_993_f64),
    ];
    let before = harness.statements;
    for value in &rejected_n {
        match harness.run(&n_sql, n_ty, "n", value, Provenance::RouteParam).await {
            Outcome::Rows(rows) => assert!(rows.is_empty(), "{value} from a route param matches nothing"),
            Outcome::Error(e) => panic!("{value} from a route param is a no-match, not an error: {e}"),
        }
        assert!(
            matches!(harness.run(&n_sql, n_ty, "n", value, Provenance::Code).await, Outcome::Error(_)),
            "{value} from code is an error"
        );
    }
    assert!(
        matches!(harness.run(&t_sql, t_ty, "t", &json!(42), Provenance::Code).await, Outcome::Error(_)),
        "a number for a text column is refused by Rust"
    );
    assert_eq!(harness.statements, before, "no rejected value reached the database");

    // The danger the rejection prevents: bound raw, SQLite's affinity matches.
    let affinity_042 = harness.raw(&n_sql, libsql::Value::Text("042".into())).await;
    let affinity_t = harness.raw(&t_sql, libsql::Value::Integer(42)).await;
    eprintln!("raw `n IS '042'` → {affinity_042:?}; raw `t IS 42` → {affinity_t:?}");
    assert_eq!(affinity_042, vec![1], "SQLite would have matched \"042\" to 42");
    assert_eq!(affinity_t, vec![2], "SQLite would have matched 42 to \"42\"");
    drop(harness);
    drop(db);
}
