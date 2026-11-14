//! T-0033 · Q1a slice 3 — provenance per call site, per slot, by binding
//! (Design 4, R3-6, Acceptance 10, Falsifier 8's spelling mutant).

use std::collections::BTreeMap;

use serde_json::json;

use super::{classify_source, Classification};
use crate::forge::declare::{FieldSpec, FieldType};
use crate::forge::query::coerce::{coerce, Coerced, Provenance};
use crate::forge::query::CollectionSchema;

fn classify(source: &str) -> Classification {
    let mut schemas = BTreeMap::new();
    schemas.insert(
        "items".to_string(),
        CollectionSchema {
            name: "items".into(),
            table: "items".into(),
            key: "id".into(),
            fields: [
                ("done".to_string(), FieldSpec::new(FieldType::Bool)),
                ("owner".to_string(), FieldSpec::new(FieldType::Text)),
            ]
            .into_iter()
            .collect(),
            default_order: Vec::new(),
            partition_by: None,
        },
    );
    let c = classify_source(
        "src/routes/r.tsx",
        &format!("import {{ items }} from \"albedo/forge\";\n{source}"),
        false,
        &schemas,
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    c
}

#[test]
fn two_sites_sharing_one_boolean_descriptor_coerce_route_text_and_code_text_differently() {
    let c = classify(
        "export default async function A({ params }) {\n  return await items.where({ done: params.flag }).count();\n}\n\
         export async function b() {\n  return await items.where({ done: \"true\" }).count();\n}\n",
    );
    assert_eq!(c.queries.len(), 2);
    let (a, b) = (&c.queries[0], &c.queries[1]);
    assert_eq!(a.canonical.id, b.canonical.id, "one descriptor");
    assert_eq!(a.provenance, vec![Provenance::RouteParam]);
    assert_eq!(b.provenance, vec![Provenance::Code]);
    let text = json!("true");
    assert!(matches!(
        coerce(&text, FieldType::Bool, a.provenance[0], "done"),
        Ok(Coerced::Bind(_))
    ));
    assert!(coerce(&text, FieldType::Bool, b.provenance[0], "done").is_err());
}

#[test]
fn provenance_follows_the_binding_not_the_spelling() {
    let c = classify(
        "export default async function C({ params, user }) {\n\
         \x20 function inner(params) {\n    return items.where({ owner: params.flag }).count();\n  }\n\
         \x20 const local = () => { const params = { flag: \"x\" }; return items.where({ owner: params.flag }).count(); };\n\
         \x20 await items.where({ owner: params.id }).count();\n\
         \x20 await items.where({ owner: user.id }).count();\n\
         \x20 return [inner, local];\n}\n\
         export async function D(props) {\n  return await items.where({ owner: props.params.id }).count();\n}\n\
         export async function E({ params: p }) {\n  return await items.where({ owner: p.id }).count();\n}\n",
    );
    let by_line: Vec<(usize, Provenance)> = c
        .queries
        .iter()
        .map(|q| (q.at.line, q.provenance[0]))
        .collect();
    assert_eq!(
        by_line,
        vec![
            (4, Provenance::Code),       // inner's own `params` parameter shadows the prop
            (6, Provenance::Code),       // a local `const params`
            (7, Provenance::RouteParam), // the component's destructured `params`
            (8, Provenance::Principal),  // its `user`
            (12, Provenance::RouteParam), // `props.params.id`
            (15, Provenance::RouteParam), // `{ params: p }` → `p.id`
        ],
    );
}
