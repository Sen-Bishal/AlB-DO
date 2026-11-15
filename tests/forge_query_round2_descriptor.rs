//! Additional T-0033 acceptance checks for overlapping predicates and all masks.

use std::collections::{BTreeMap, BTreeSet};

use dom_render_compiler::forge::declare::{FieldSpec, FieldType};
use dom_render_compiler::forge::query::coerce::{coerce, coerce_in, Coerced, Provenance};
use dom_render_compiler::forge::query::index::wanted;
use dom_render_compiler::forge::query::CollectionSchema;
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::transforms::forge_queries::{classify_source, LoweredQuery};
use serde_json::json;

fn schema() -> CollectionSchema {
    CollectionSchema {
        name: "items".into(),
        table: "item_rows".into(),
        key: "item_key".into(),
        fields: [
            ("a", FieldType::Int),
            ("b", FieldType::Int),
            ("c", FieldType::Int),
            ("label", FieldType::Text),
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
        "import {{ items }} from 'albedo/forge';\nimport {{ optional, or }} from 'albedo/query';\nexport async function read(v) {{ return await {chain}; }}"
    );
    let schemas = BTreeMap::from([("items".into(), schema())]);
    let mut result = classify_source("src/round2.ts", &source, false, &schemas);
    assert!(
        result.diagnostics.is_empty(),
        "{chain}: {:?}",
        result.diagnostics
    );
    assert_eq!(result.queries.len(), 1, "{chain}");
    result.queries.remove(0)
}

#[test]
fn equality_and_in_on_the_same_column_need_one_index_prefix_column() {
    for chain in [
        "items.where({ a: v.a }).where({ a: { in: v.values } }).limit(50)",
        "items.where({ a: { in: v.values } }).where({ a: v.a }).count()",
        "items.where({ a: v.a }).where({ a: { in: v.values } }).groupBy('b', {})",
    ] {
        let q = query(chain);
        for index in wanted(&q.canonical.descriptor, &schema()) {
            let names: BTreeSet<_> = index.columns.iter().map(|c| &c.column).collect();
            assert_eq!(
                names.len(),
                index.columns.len(),
                "{chain}: {}",
                index.column_spec()
            );
        }
    }
}

#[test]
fn or_branch_combined_with_base_equality_does_not_repeat_an_in_column() {
    let q =
        query("items.where({ a: v.a }).where(or({ a: { in: v.values } }, { b: v.b })).limit(10)");
    for index in wanted(&q.canonical.descriptor, &schema()) {
        let names: BTreeSet<_> = index.columns.iter().map(|c| &c.column).collect();
        assert_eq!(names.len(), index.columns.len(), "{}", index.column_spec());
    }
}

#[test]
fn coercion_handles_large_malformed_numbers_and_preserves_text_bytes() {
    let huge = "9".repeat(100_000);
    for ty in [FieldType::Int, FieldType::Timestamp, FieldType::Real] {
        assert!(coerce(&json!(huge), ty, Provenance::Code, "a").is_err());
        assert_eq!(
            coerce(&json!(huge), ty, Provenance::RouteParam, "a").unwrap(),
            Coerced::NoMatch
        );
    }
    let text = "nul\0quote'\r\n\\?\u{1f600}";
    assert_eq!(
        coerce(&json!(text), FieldType::Text, Provenance::Code, "label").unwrap(),
        Coerced::Bind(SqlValue::Text(text.into()))
    );
    let encoded = coerce_in(&[json!(text)], FieldType::Text, Provenance::Code, "label")
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&encoded).unwrap(),
        vec![text]
    );
}

#[tokio::test]
async fn every_optional_mask_returns_exact_intersection_and_hostile_text_stays_bound() {
    // A single database per binary avoids the known native-libsql teardown issue.
    let dir = tempfile::tempdir().unwrap();
    let db = libsql::Builder::new_local(dir.path().join("masks.db"))
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE item_rows(item_key INTEGER PRIMARY KEY, a INTEGER, b INTEGER, c INTEGER, label TEXT)", ()).await.unwrap();
    for bits in 0..8_i64 {
        conn.execute(
            "INSERT INTO item_rows VALUES (?1,?2,?3,?4,?5)",
            libsql::params![bits + 1, bits & 1, (bits >> 1) & 1, (bits >> 2) & 1, "safe"],
        )
        .await
        .unwrap();
    }
    let q =
        query("items.where({ a: optional(v.a), b: optional(v.b), c: optional(v.c) }).limit(1000)");
    assert_eq!(q.lowered.statements.len(), 8);
    for statement in &q.lowered.statements {
        let values = statement
            .binds
            .iter()
            .map(|_| libsql::Value::Integer(1))
            .collect::<Vec<_>>();
        let mut rows = conn.query(&statement.sql, values).await.unwrap();
        let mut actual = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            actual.push(row.get::<i64>(0).unwrap());
        }
        let expected = (0..8_i64)
            .filter(|bits| (bits & i64::from(statement.mask)) == i64::from(statement.mask))
            .map(|bits| bits + 1)
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "mask {}", statement.mask);
    }
    let hostile = "safe' OR 1=1; DROP TABLE item_rows;--";
    let q = query("items.where({ label: v.label }).limit(1000)");
    assert!(!q.lowered.statements[0].sql.contains(hostile));
    let mut rows = conn
        .query(
            &q.lowered.statements[0].sql,
            [libsql::Value::Text(hostile.into())],
        )
        .await
        .unwrap();
    assert!(rows.next().await.unwrap().is_none());
    let mut count = conn
        .query("SELECT COUNT(*) FROM item_rows", ())
        .await
        .unwrap();
    assert_eq!(
        count.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        8
    );
}
