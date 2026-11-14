//! Additional Q1a checks for canonicalization and bound JSON values.

use std::collections::BTreeMap;

use dom_render_compiler::forge::declare::{FieldSpec, FieldType};
use dom_render_compiler::forge::query::coerce::{coerce_in, Provenance};
use dom_render_compiler::forge::query::CollectionSchema;
use dom_render_compiler::transforms::forge_queries::{classify_source, LoweredQuery};
use serde_json::{json, Value};

fn schemas() -> BTreeMap<String, CollectionSchema> {
    BTreeMap::from([(
        "items".into(),
        CollectionSchema {
            name: "items".into(),
            table: "item_rows".into(),
            key: "item_key".into(),
            fields: BTreeMap::from([
                ("label".into(), FieldSpec::nullable(FieldType::Text)),
                ("n".into(), FieldSpec::nullable(FieldType::Int)),
            ]),
            default_order: Vec::new(),
            partition_by: None,
        },
    )])
}

fn query(chain: &str) -> LoweredQuery {
    let source = format!(
        "import {{ items }} from 'albedo/forge';\nimport {{ or, optional, count }} from 'albedo/query';\nexport async function read(v) {{ return await {chain}; }}"
    );
    let mut result = classify_source("src/read.ts", &source, false, &schemas());
    assert!(
        result.diagnostics.is_empty(),
        "{chain}: {:?}",
        result.diagnostics
    );
    assert_eq!(result.queries.len(), 1);
    result.queries.remove(0)
}

#[test]
fn concurrent_classifier_runs_keep_identity_and_site_slot_maps_isolated() {
    let expected = query("items.where({ label: v.label, n: v.n }).limit(7)");
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let expected = &expected;
                scope.spawn(move || {
                    for iteration in 0..16 {
                        let reversed = (worker + iteration) % 2 == 0;
                        let q = query(if reversed {
                            "items.where({ n: v.n, label: v.label }).limit(7)"
                        } else {
                            "items.where({ label: v.label, n: v.n }).limit(7)"
                        });
                        assert_eq!(q.canonical.id, expected.canonical.id);
                        assert_eq!(q.canonical.json, expected.canonical.json);
                        let mut map = q.canonical.slot_map.clone();
                        if reversed {
                            map.reverse();
                        }
                        assert_eq!(map, expected.canonical.slot_map);
                        assert_eq!(q.lowered, expected.lowered);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    });
}

#[test]
fn text_in_serialization_preserves_nul_unicode_escapes_and_duplicates() {
    let values = [
        json!("\0"),
        json!("雪😀"),
        json!("\"\\\r\n\t"),
        json!("x') OR 1=1 --"),
        json!("雪😀"),
    ];
    let encoded = coerce_in(&values, FieldType::Text, Provenance::Code, "label")
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<Value>>(&encoded).unwrap(),
        values
    );
}

#[tokio::test]
async fn bound_text_and_maximum_in_array_execute_exactly_on_libsql() {
    // A single database in this binary avoids the Windows multiple-substrate hazard.
    let dir = tempfile::tempdir().unwrap();
    let db = libsql::Builder::new_local(dir.path().join("query.db"))
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE item_rows(item_key INTEGER PRIMARY KEY, label TEXT, n INTEGER)",
        (),
    )
    .await
    .unwrap();
    for (key, text) in [(1, "\0"), (2, "雪😀"), (3, "\"\\\r\n\t"), (4, "ordinary")] {
        conn.execute(
            "INSERT INTO item_rows VALUES (?1, ?2, ?3)",
            libsql::params![key, text, key],
        )
        .await
        .unwrap();
    }
    let texts = [
        json!("\0"),
        json!("雪😀"),
        json!("\"\\\r\n\t"),
        json!("x') OR 1=1 --"),
    ];
    let encoded = coerce_in(&texts, FieldType::Text, Provenance::Code, "label")
        .unwrap()
        .unwrap();
    let q = query("items.where({ label: { in: v } }).orderBy('item_key', 'desc').limit(1000)");
    let mut rows = conn
        .query(&q.lowered.statements[0].sql, [encoded])
        .await
        .unwrap();
    let mut actual = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        actual.push(row.get::<i64>(0).unwrap());
    }
    assert_eq!(actual, [3, 2, 1]);

    let numbers: Vec<_> = (0..1000).map(|n| json!(n.to_string())).collect();
    let encoded = coerce_in(&numbers, FieldType::Int, Provenance::Code, "n")
        .unwrap()
        .unwrap();
    let q = query("items.where({ n: { in: v } }).count()");
    let mut rows = conn
        .query(&q.lowered.statements[0].sql, [encoded])
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        4
    );
    assert!(rows.next().await.unwrap().is_none());

    let q = query("items.where(or({}, { n: v })).count()");
    let mut rows = conn
        .query(&q.lowered.statements[0].sql, [libsql::Value::Integer(999)])
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        4
    );
    assert!(rows.next().await.unwrap().is_none());
}
