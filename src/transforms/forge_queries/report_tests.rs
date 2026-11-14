//! T-0033 · Q1a slice 3 — the build's query report (Design 1, 11; Acceptance 7).

use std::collections::BTreeMap;

use super::{classify_source, query_report, Classification, DERIVED_INDEX_WARNING};
use crate::forge::declare::CollectionDecl;
use crate::forge::query::CollectionSchema;

fn decls() -> BTreeMap<String, CollectionDecl> {
    let mut out = BTreeMap::new();
    for (name, json) in [
        ("invoices", r#"{ "fields": { "amount": "real", "due": "timestamp", "owner": "text", "status": "text" } }"#),
        ("messages", r#"{ "fields": { "room": "text", "body": "text" }, "partition_by": "room" }"#),
    ] {
        out.insert(name.to_string(), serde_json::from_str(json).expect("decl"));
    }
    out
}

fn classify(body: &str) -> Classification {
    let schemas: BTreeMap<String, CollectionSchema> = decls()
        .iter()
        .map(|(n, d)| (n.clone(), CollectionSchema::from_decl(n, d)))
        .collect();
    let source = format!(
        "import {{ useSharedSlot }} from \"albedo\";\nimport {{ invoices, messages }} from \"albedo/forge\";\nexport async function load(v, params) {{\n{body}\n}}\n"
    );
    let c = classify_source("src/routes/r.tsx", &source, false, &schemas);
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    c
}

#[test]
fn no_new_style_query_means_no_report() {
    // A real component: `params` is its first (props) argument.
    let c = super::classify_source(
        "src/routes/r.tsx",
        "import { useSharedSlot } from \"albedo\";\nimport { messages } from \"albedo/forge\";\n\
         export default function R({ params }) {\n  return useSharedSlot(messages.where({ room: params.id }));\n}\n",
        false,
        &super::tests::schemas(),
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert_eq!(c.legacy, 1);
    assert!(query_report(&c, &decls()).is_none());
}

#[test]
fn the_report_lists_sql_unbounded_and_whole_table_reads_and_indexes() {
    let c = classify(
        "  await invoices.where({ status: v }).orderBy(\"due\", \"desc\").limit(50);\n\
         \x20 await invoices.where({ owner: v });\n\
         \x20 await invoices.count();\n",
    );
    let report = query_report(&c, &decls()).expect("report");
    assert!(report.contains("3 new-style queries"), "{report}");
    assert!(report.contains("ORDER BY due DESC, id DESC LIMIT 50"), "{report}");
    assert!(report.contains("CREATE INDEX IF NOT EXISTS idx_invoices_q"), "{report}");
    assert!(report.contains("(status, due DESC, id DESC)"), "{report}");
    let unbounded = report.split("## Unbounded reads").nth(1).unwrap();
    assert!(unbounded.starts_with(" (no limit/first): 1"), "{report}");
    assert!(unbounded.contains("src/routes/r.tsx:5:"), "the owner read: {report}");
    let whole = report.split("## May examine a whole table").nth(1).unwrap();
    assert!(whole.contains("src/routes/r.tsx:6:"), "count() over everything: {report}");
}

#[test]
fn a_query_the_legacy_partition_index_serves_derives_no_index() {
    let c = classify("  return await messages.where({ room: v }).limit(20);");
    let report = query_report(&c, &decls()).expect("report");
    assert!(
        !report.contains("idx_messages_q"),
        "(room, id) is the legacy partition index already: {report}"
    );
}

#[test]
fn more_than_six_derived_indexes_on_a_table_is_a_warning() {
    let body: String = ["amount", "due", "owner", "status"]
        .iter()
        .flat_map(|col| {
            ["asc", "desc"].iter().map(move |dir| {
                format!("  await invoices.where({{ id: v }}).orderBy(\"{col}\", \"{dir}\").limit(5);\n")
            })
        })
        .collect();
    let c = classify(&body);
    let report = query_report(&c, &decls()).expect("report");
    assert!(
        report.contains(&format!("more than {DERIVED_INDEX_WARNING}")),
        "{report}"
    );
}
