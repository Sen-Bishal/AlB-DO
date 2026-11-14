//! T-0033 · Q1a — the classification pass: what each reference is, and where.

use std::collections::BTreeMap;

use super::{classify_source, Classification};
use crate::forge::declare::{FieldSpec, FieldType};
use crate::forge::query::{CollectionSchema, Shape};

fn schema(name: &str, fields: &[(&str, FieldType)], partition_by: Option<&str>) -> CollectionSchema {
    CollectionSchema {
        name: name.into(),
        table: name.into(),
        key: "id".into(),
        fields: fields
            .iter()
            .map(|(n, t)| ((*n).to_string(), FieldSpec::new(*t)))
            .collect(),
        default_order: Vec::new(),
        partition_by: partition_by.map(str::to_string),
    }
}

pub(super) fn schemas() -> BTreeMap<String, CollectionSchema> {
    let mut out = BTreeMap::new();
    out.insert(
        "guestbook".into(),
        schema("guestbook", &[("author", FieldType::Text), ("message", FieldType::Text)], None),
    );
    out.insert(
        "invoices".into(),
        schema(
            "invoices",
            &[
                ("amount", FieldType::Real),
                ("due", FieldType::Timestamp),
                ("owner", FieldType::Text),
                ("status", FieldType::Text),
            ],
            None,
        ),
    );
    out.insert(
        "messages".into(),
        schema("messages", &[("room", FieldType::Text), ("body", FieldType::Text)], Some("room")),
    );
    // Collections named like helpers (Falsifier 7).
    for name in ["count", "sum", "optional"] {
        out.insert(name.into(), schema(name, &[("label", FieldType::Text)], None));
    }
    out
}

pub(super) fn classify(source: &str) -> Classification {
    classify_source("src/routes/probe.tsx", source, false, &schemas())
}

fn one_diagnostic(source: &str) -> String {
    let c = classify(source);
    assert_eq!(c.queries.len(), 0, "{:?}", c.queries);
    assert_eq!(c.diagnostics.len(), 1, "{:?}", c.diagnostics);
    format!("{} — {}", c.diagnostics[0].at, c.diagnostics[0].message)
}

#[test]
fn the_why_2_probe_lowers_to_a_count_and_is_named_by_its_line() {
    let c = classify(
        "import { guestbook } from \"albedo/forge\";\n\nexport default async function Probe() {\n  const n = await guestbook.where({ author: \"ada\" }).count();\n  return <p>{n}</p>;\n}\n",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert_eq!(c.queries.len(), 1);
    let q = &c.queries[0];
    assert_eq!(q.at.to_string(), "src/routes/probe.tsx:4:19");
    assert_eq!(q.collection, "guestbook");
    assert_eq!(q.canonical.descriptor.shape, Shape::Count);
    assert!(q.canonical.id.starts_with("q_"));
    assert_eq!(q.lowered.statements.len(), 1);
    let refusal = c.refusal().unwrap();
    assert!(refusal.contains("src/routes/probe.tsx:4:19"), "{refusal}");
    assert!(refusal.contains("compiled and lowered"), "{refusal}");
    assert!(refusal.contains("Q1b"), "{refusal}");
}

#[test]
fn a_bare_collection_value_is_its_own_diagnostic() {
    let d = one_diagnostic("import { guestbook } from \"albedo/forge\";\n\nconst x = guestbook;\n");
    assert!(d.starts_with("src/routes/probe.tsx:3:11 — `guestbook` is a collection"), "{d}");
}

#[test]
fn member_reads_and_calls_on_a_collection_are_diagnostics() {
    for body in ["guestbook.length", "fn(guestbook)", "guestbook()", "guestbook[\"where\"]({})", "typeof guestbook", "({ guestbook })"] {
        let d = one_diagnostic(&format!("import {{ guestbook }} from \"albedo/forge\";\nconst fn = (x) => x;\nconst y = {body};\n"));
        assert!(d.contains("`guestbook` is a collection"), "{body}: {d}");
    }
}

#[test]
fn an_aliased_import_resolves_to_its_exported_collection() {
    let c = classify("import { guestbook as gb } from \"albedo/forge\";\nconst n = gb.where({ author: \"a\" }).count();\n");
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert_eq!(c.queries[0].collection, "guestbook");
}

#[test]
fn a_parameter_or_local_that_shadows_a_collection_is_ordinary_js() {
    let c = classify(
        "import { guestbook } from \"albedo/forge\";\n\
         function f(guestbook) { return guestbook.where({ x: 1 }).count(); }\n\
         function g() { const guestbook = []; return guestbook.length; }\n",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert!(c.queries.is_empty(), "{:?}", c.queries);
}

#[test]
fn type_positions_and_type_only_imports_are_never_flagged() {
    let c = classify(
        "import type { guestbook } from \"albedo/forge\";\n\
         import { type invoices } from \"albedo/forge\";\n\
         import { messages } from \"albedo/forge\";\n\
         type G = typeof guestbook;\n\
         type M = typeof messages;\n\
         let x: typeof invoices;\n",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert!(c.queries.is_empty());
}

#[test]
fn namespace_imports_and_re_exports_are_diagnostics() {
    let d = one_diagnostic("import * as f from \"albedo/forge\";\n");
    assert!(d.contains("import * as f"), "{d}");
    let d = one_diagnostic("export { guestbook } from \"albedo/forge\";\n");
    assert!(d.contains("re-exporting"), "{d}");
    let d = one_diagnostic("import { guestbook } from \"albedo/forge\";\nexport { guestbook };\n");
    assert!(d.contains("re-exporting `guestbook`"), "{d}");
}

#[test]
fn legacy_use_shared_slot_forms_stay_legacy() {
    let c = classify(
        "import { useSharedSlot } from \"albedo\";\n\
         import { guestbook, messages } from \"albedo/forge\";\n\
         export default function C({ params }) {\n\
           const a = useSharedSlot(guestbook);\n\
           const b = useSharedSlot(messages.where({ room: params.id }));\n\
           const c = useSharedSlot(\"guestbook\");\n\
           return null;\n\
         }\n",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert!(c.queries.is_empty());
    assert_eq!(c.legacy, 2);
    assert!(c.refusal().is_none());
}

#[test]
fn a_new_style_live_query_is_refused_with_the_q3_wording() {
    let d = one_diagnostic(
        "import { useSharedSlot } from \"albedo\";\n\
         import { invoices } from \"albedo/forge\";\n\
         const rows = useSharedSlot(invoices.where({ status: \"open\" }).limit(10));\n",
    );
    assert!(d.contains("arrives with Q3"), "{d}");
}

#[test]
fn collections_named_like_helpers_build_without_albedo_query() {
    let c = classify(
        "import { useSharedSlot } from \"albedo\";\n\
         import { count, sum, optional } from \"albedo/forge\";\n\
         const a = useSharedSlot(count); const b = useSharedSlot(sum); const c = useSharedSlot(optional);\n",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert_eq!(c.legacy, 3);
    assert!(c.refusal().is_none());
}

#[test]
fn the_count_helper_lowers_beside_a_collection_named_count() {
    let c = classify(
        "import { count as countRows } from \"albedo/forge\";\n\
         import { count } from \"albedo/query\";\n\
         const g = await countRows.groupBy(\"label\", { n: count() });\n",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert_eq!(c.queries.len(), 1);
    assert_eq!(c.queries[0].collection, "count");
    assert!(matches!(c.queries[0].canonical.descriptor.shape, Shape::Group { .. }));
}

#[test]
fn every_chain_shape_lowers() {
    let c = classify(
        "import { invoices } from \"albedo/forge\";\n\
         import { or, optional, sum } from \"albedo/query\";\n\
         export async function load(params, filter, before) {\n\
           await invoices.where({ status: \"open\", due: { lt: before } }).orderBy(\"due\", \"desc\").limit(50);\n\
           await invoices.get(params.id);\n\
           await invoices.where({ owner: \"me\" }).first();\n\
           await invoices.groupBy(\"status\", { total: sum(\"amount\") });\n\
           await invoices.where(or({ status: \"open\" }, { owner: \"me\" }));\n\
           await invoices.where({ owner: optional(filter.owner) });\n\
           await invoices.where({ owner: { in: [\"a\", \"b\"] }, status: null, due: { not: null } }).count();\n\
         }\n",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
    assert_eq!(c.queries.len(), 7);
}

#[test]
fn identical_queries_in_two_places_share_one_id() {
    let c = classify(
        "import { invoices } from \"albedo/forge\";\n\
         const a = invoices.where({ status: \"x\", owner: \"y\" }).count();\n\
         const b = invoices.where({ owner: \"q\", status: \"z\" }).count();\n",
    );
    assert_eq!(c.queries.len(), 2);
    assert_eq!(c.queries[0].canonical.id, c.queries[1].canonical.id);
    assert_ne!(c.queries[0].at, c.queries[1].at);
}

#[test]
fn rejected_shapes_each_name_their_rule() {
    let cases: &[(&str, &str)] = &[
        ("invoices.where({ \"owner.name\": 1 })", "Q1r"),
        ("invoices.where({ owner: { name: \"a\" } })", "Q1r"),
        ("invoices.where({ owner: { contains: \"a\" } })", "Q2"),
        ("invoices.where({ owner: 1, owner: 2 })", "appears twice"),
        ("invoices.where({ __proto__: 1 })", "`__proto__`"),
        ("invoices.where({ get owner() { return 1; } })", "no getters"),
        ("invoices.where({ ...p })", "spread"),
        ("invoices.where({ [k]: 1 })", "computed"),
        ("invoices.where(or({ owner: optional(x) }, { status: 1 }))", "optional() inside or"),
        ("invoices.where({ due: { lt: null } })", "can't bound a range"),
        ("invoices.where({ due: { lt: 1, lte: 2 } })", "two upper bounds"),
        ("invoices.where({ due: { not: 1 } })", "`not` takes only `null`"),
        ("invoices.filter({ a: 1 })", "isn't a query method"),
        ("invoices.orderBy(\"due\").where({ owner: 1 })", "out of order"),
        ("invoices.where({ owner: 1 }).limit(10).count()", "out of order"),
        ("invoices.get(1).count()", "stands alone"),
        ("invoices.where({ owner: 1 }).limit(n)", "integer literal"),
        ("invoices.where({ owner: 1 }).limit(0)", "integer 1..=1000"),
        ("invoices.where({ owner: 1 }).limit(5000)", "out of range"),
        ("invoices.where({ nope: 1 })", "has no column `nope`"),
        ("invoices.orderBy(\"due\", \"sideways\")", "\"asc\" or \"desc\""),
        ("invoices.where(pred)", "by name"),
    ];
    for (chain, expected) in cases {
        let source = format!(
            "import {{ invoices }} from \"albedo/forge\";\nimport {{ or, optional }} from \"albedo/query\";\nconst r = {chain};\n"
        );
        let c = classify(&source);
        assert!(c.queries.is_empty(), "{chain}: lowered {:?}", c.queries);
        let messages: Vec<String> = c.diagnostics.iter().map(|d| format!("{} — {}", d.at, d.message)).collect();
        assert!(
            messages.iter().any(|m| m.contains(expected) && m.starts_with("src/routes/probe.tsx:3:")),
            "{chain}: expected `{expected}` at line 3, got {messages:?}"
        );
    }
}

#[test]
fn a_helper_outside_a_chain_is_a_diagnostic() {
    let d = one_diagnostic("import { count } from \"albedo/query\";\nconst c = count();\n");
    assert!(d.contains("only means something inside a query chain"), "{d}");
}

#[test]
fn misuse_inside_a_chain_argument_is_still_caught() {
    let c = classify(
        "import { invoices, guestbook } from \"albedo/forge\";\nconst r = invoices.where({ owner: guestbook }).count();\n",
    );
    assert_eq!(c.queries.len(), 1);
    assert_eq!(c.diagnostics.len(), 1, "{:?}", c.diagnostics);
    assert!(c.diagnostics[0].message.contains("`guestbook` is a collection"));
}

#[test]
fn an_undeclared_collection_is_named() {
    let d = one_diagnostic("import { ghosts } from \"albedo/forge\";\nconst r = ghosts.where({ a: 1 }).count();\n");
    assert!(d.contains("`ghosts` isn't a declared collection"), "{d}");
}

#[test]
fn a_module_without_framework_imports_is_untouched() {
    let c = classify("const guestbook = { where() {} };\nguestbook.where({}).count();\n");
    assert!(c.diagnostics.is_empty() && c.queries.is_empty() && c.legacy == 0);
}
