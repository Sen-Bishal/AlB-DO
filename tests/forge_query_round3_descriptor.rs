//! Resource-boundary checks for T-0033's descriptor construction.

use std::collections::BTreeMap;

use dom_render_compiler::forge::declare::{FieldSpec, FieldType};
use dom_render_compiler::forge::query::CollectionSchema;
use dom_render_compiler::transforms::forge_queries::classify_source;

#[test]
fn a_query_exceeding_parameter_slot_capacity_is_refused_without_panicking() {
    // Four permitted OR branches exceed u16 parameter capacity without a
    // deeply nested call chain or an invalid/duplicate predicate key.
    let fields: BTreeMap<_, _> = (0..16_384)
        .map(|i| (format!("c{i}"), FieldSpec::new(FieldType::Int)))
        .collect();
    let predicate = format!(
        "{{{}}}",
        fields
            .keys()
            .map(|column| format!("{column}: 0"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let source = format!(
        "import {{ items }} from 'albedo/forge';\nimport {{ or }} from 'albedo/query';\nexport async function read() {{ return await items.where(or({predicate},{predicate},{predicate},{predicate})).limit(1); }}"
    );
    let schemas = BTreeMap::from([(
        "items".into(),
        CollectionSchema {
            name: "items".into(),
            table: "item_rows".into(),
            key: "item_key".into(),
            fields,
            default_order: Vec::new(),
            partition_by: None,
        },
    )]);
    let result = std::panic::catch_unwind(|| {
        classify_source("src/parameter_capacity.ts", &source, false, &schemas)
    });
    let result = result.expect("a large query must produce a diagnostic, not panic");
    assert!(result.queries.is_empty(), "an overflowing slot map is unusable");
    assert!(
        result.diagnostics.iter().any(|diagnostic| {
            let message = diagnostic.message.to_ascii_lowercase();
            message.contains("param") && (message.contains("limit") || message.contains("many"))
        }),
        "{:?}",
        result.diagnostics
    );
}
