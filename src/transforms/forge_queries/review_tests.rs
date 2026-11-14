//! Independent T-0033 acceptance probes for value positions and evaluation edges.

use super::tests::{classify, schemas};
use super::{classify_source, rewrite_source, Classification};

fn diagnostics(c: &Classification) -> Vec<String> {
    c.diagnostics
        .iter()
        .map(|d| format!("{}: {}", d.at, d.message))
        .collect()
}

#[test]
fn jsx_collection_names_are_value_uses_and_must_be_refused() {
    for jsx in ["<Invoices />", "<Invoices.Row />", "<Invoices></Invoices>"] {
        let c = classify(&format!(
            "import {{ invoices as Invoices }} from 'albedo/forge';\nexport default function Page() {{ return {jsx}; }}\n"
        ));
        assert!(
            c.refusal().is_some(),
            "unclassified collection value: {jsx}"
        );
        assert!(
            c.diagnostics
                .iter()
                .any(|d| d.at.line == 2 && d.message.contains("collection")),
            "{jsx}: {:?}",
            diagnostics(&c)
        );
    }
}

#[test]
fn jsx_helper_names_are_value_uses_and_must_be_refused() {
    let c = classify("import { optional as Optional } from 'albedo/query';\nexport default function Page() { return <Optional />; }\n");
    assert!(
        c.refusal().is_some(),
        "an imported helper used as a JSX value escaped classification"
    );
}

#[test]
fn lowercase_intrinsic_jsx_tags_do_not_read_a_same_named_import() {
    let c = classify("import { invoices } from 'albedo/forge';\nexport default function Page() { return <invoices />; }\n");
    assert!(c.refusal().is_none(), "{:?}", diagnostics(&c));
    let c = classify("import { invoices } from 'albedo/forge';\nexport default function Page() { return <invoices.Row.Cell />; }\n");
    assert!(c
        .diagnostics
        .iter()
        .any(|d| d.message.contains("collection")));
}

#[test]
fn spread_arguments_are_refused_in_every_query_method_and_helper() {
    for chain in [
        "invoices.limit(...10)",
        "invoices.orderBy(...'due').limit(1)",
        "invoices.orderBy('due', ...'desc').limit(1)",
        "invoices.groupBy(...'status', { n: count() })",
        "invoices.groupBy('status', ...{ n: count() })",
        "invoices.groupBy('status', { n: sum(...'amount') })",
        "invoices.where({ owner: optional(...['ada']) }).count()",
    ] {
        let source = format!("import {{ invoices }} from 'albedo/forge';\nimport {{ optional, count, sum }} from 'albedo/query';\nasync function page() {{ return await {chain}; }}\n");
        let c = classify(&source);
        assert!(
            c.queries.is_empty() && !c.diagnostics.is_empty(),
            "spread silently lowered: {chain}; {:?}",
            diagnostics(&c)
        );
    }
}

#[test]
fn spread_collection_arguments_are_not_legacy_live_reads() {
    let c = classify("import { useSharedSlot } from 'albedo';\nimport { guestbook } from 'albedo/forge';\nconst rows = useSharedSlot(...guestbook);\n");
    assert_eq!(
        c.legacy, 0,
        "spreading a collection is a value use, not useSharedSlot(collection)"
    );
    assert!(
        !c.diagnostics.is_empty(),
        "spread collection escaped classification"
    );
}

#[test]
fn a_string_topic_tuple_spread_is_ordinary_js_beside_an_unused_collection_import() {
    let c = classify("import { useSharedSlot } from 'albedo';\nimport { invoices } from 'albedo/forge';\nconst args = ['topic'];\nconst rows = useSharedSlot(...args);\n");
    assert!(c.refusal().is_none(), "{:?}", diagnostics(&c));
    assert_eq!(c.legacy, 0);
}

#[test]
fn only_the_exact_partition_read_is_classified_as_legacy() {
    let mut violations = Vec::new();
    for chain in [
        "invoices.where({ status: 'open' })",
        "messages.where({ body: params.id })",
        "messages.where({ room: { in: ['a', 'b'] } })",
    ] {
        let c = classify(&format!("import {{ useSharedSlot }} from 'albedo';\nimport {{ invoices, messages }} from 'albedo/forge';\nexport default function Page({{ params }}) {{ return useSharedSlot({chain}); }}\n"));
        if c.legacy != 0 || !c.diagnostics.iter().any(|d| d.message.contains("Q3")) {
            violations.push(format!(
                "{chain}: legacy={}, diagnostics={:?}",
                c.legacy,
                diagnostics(&c)
            ));
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn type_only_reexports_do_not_become_runtime_value_uses() {
    for export in ["export type { invoices };", "export { type invoices };"] {
        let c = classify(&format!(
            "import {{ invoices }} from 'albedo/forge';\n{export}\n"
        ));
        assert!(
            c.diagnostics.is_empty() && c.queries.is_empty(),
            "{export}: {:?}",
            diagnostics(&c)
        );
    }
    let c = classify("export { type invoices } from 'albedo/forge';\nexport type { optional } from 'albedo/query';\n");
    assert!(c.refusal().is_none(), "{:?}", diagnostics(&c));
}

#[test]
fn type_only_predicate_exports_are_ignored_by_local_and_project_censuses() {
    let c = classify("import { invoices } from 'albedo/forge';\nconst pred = { owner: 'ada' };\nexport { type pred };\nasync function page() { return await invoices.where(pred).count(); }\n");
    assert!(c.diagnostics.is_empty(), "{:?}", diagnostics(&c));
    assert_eq!(c.queries.len(), 1);

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("preds.ts"),
        "export const open = { owner: 'ada' };\n",
    )
    .unwrap();
    std::fs::write(src.join("page.ts"), "import { invoices } from 'albedo/forge';\nimport { open } from './preds';\nexport async function page() { return await invoices.where(open).count(); }\n").unwrap();
    std::fs::write(src.join("types.ts"), "export { type open } from './preds';\nexport type { open } from './preds';\nimport { type open } from './preds';\ntype Predicate = typeof open;\n").unwrap();
    let declarations = [(
        "invoices".into(),
        serde_json::from_str(r#"{"fields":{"owner":"text"}}"#).unwrap(),
    )]
    .into_iter()
    .collect();
    let c = super::classify_project(&src, dir.path(), &declarations).unwrap();
    assert!(c.diagnostics.is_empty(), "{:?}", diagnostics(&c));
    assert_eq!(c.queries.len(), 1);
}

#[test]
fn helper_shadowing_uses_the_resolved_binding() {
    let c = classify("import { invoices } from 'albedo/forge';\nimport { optional } from 'albedo/query';\nfunction page(optional) { return invoices.where({ owner: optional('ada') }).count(); }\n");
    assert!(c.diagnostics.is_empty(), "{:?}", diagnostics(&c));
    assert_eq!(c.queries.len(), 1);
    assert!(c.queries[0]
        .canonical
        .descriptor
        .terms
        .iter()
        .all(|t| t.optional_slot.is_none()));
    assert_eq!(c.queries[0].lowered.statements.len(), 1);
}

#[test]
fn catch_and_block_shadowing_do_not_capture_the_import() {
    let c = classify("import { invoices } from 'albedo/forge';\ntry { throw {}; } catch (invoices) { invoices.where({ unknown: 1 }).count(); }\n{ const invoices = { limit() {} }; invoices.limit(); }\n");
    assert!(
        c.diagnostics.is_empty() && c.queries.is_empty(),
        "{:?}",
        diagnostics(&c)
    );
}

#[test]
fn a_nested_helpers_destructured_params_are_not_component_route_props() {
    use crate::forge::query::coerce::Provenance;

    let c = classify("import { invoices } from 'albedo/forge';\nexport default async function Page({ params }) {\n async function nested({ params }) { return await invoices.where({ owner: params.id }).count(); }\n async function whole(props) { return await invoices.where({ owner: props.params.id }).count(); }\n await invoices.where({ owner: params.id }).count();\n return [nested, whole];\n}\n");
    assert!(c.diagnostics.is_empty(), "{:?}", diagnostics(&c));
    let provenance: Vec<_> = c.queries.iter().map(|q| q.provenance.clone()).collect();
    assert_eq!(
        provenance,
        vec![
            vec![Provenance::Code],
            vec![Provenance::Code],
            vec![Provenance::RouteParam]
        ]
    );
}

#[test]
fn crlf_and_non_ascii_prefix_keep_original_diagnostic_coordinates() {
    let source =
        "import { invoices } from 'albedo/forge';\r\nconst marker = 'λ'; const bad = invoices;\r\n";
    let c = classify_source("src/routes/日本語.tsx", source, false, &schemas());
    assert_eq!(c.diagnostics.len(), 1, "{:?}", diagnostics(&c));
    assert_eq!(
        c.diagnostics[0].at.to_string(),
        "src/routes/日本語.tsx:2:33"
    );
}

#[test]
fn repeated_queries_have_deterministic_ids_and_site_maps_across_runs() {
    let source = "import { invoices } from 'albedo/forge';\nasync function page() {\n await invoices.where({ owner: read('owner'), status: read('status') }).count();\n await invoices.where({ status: read('status'), owner: read('owner') }).count();\n}\n";
    let a = classify(source);
    let b = classify(source);
    assert!(a.diagnostics.is_empty() && b.diagnostics.is_empty());
    assert_eq!(a.queries.len(), 2);
    assert_eq!(a.queries[0].canonical.id, a.queries[1].canonical.id);
    for (a, b) in a.queries.iter().zip(&b.queries) {
        assert_eq!(a.canonical.json, b.canonical.json);
        assert_eq!(a.canonical.slot_map, b.canonical.slot_map);
    }
    assert_ne!(
        a.queries[0].canonical.slot_map,
        a.queries[1].canonical.slot_map
    );
    assert_eq!(
        rewrite_source("src/routes/probe.js", source, &schemas()).unwrap(),
        rewrite_source("src/routes/probe.js", source, &schemas()).unwrap()
    );
}

#[test]
fn a_thousand_independent_queries_are_all_classified_without_slot_leakage() {
    let mut source =
        String::from("import { invoices } from 'albedo/forge';\nasync function page() {\n");
    for n in 0..1000 {
        source.push_str(&format!(
            "await invoices.where({{ owner: '{n}' }}).count();\n"
        ));
    }
    source.push_str("}\n");
    let c = classify(&source);
    assert!(c.diagnostics.is_empty(), "{:?}", diagnostics(&c));
    assert_eq!(c.queries.len(), 1000);
    let id = &c.queries[0].canonical.id;
    assert!(c
        .queries
        .iter()
        .all(|q| q.canonical.id == *id && q.canonical.slot_map == [0]));
}

#[test]
fn nested_predicate_writes_and_aliases_are_refused_before_lowering() {
    for extra in [
        "pred.due.lt = 4;",
        "delete pred.due;",
        "const { due } = pred; due.lt = 4;",
        "Object.defineProperty(pred, 'status', { get() { return 'closed'; } });",
    ] {
        let c = classify(&format!("import {{ invoices }} from 'albedo/forge';\nconst pred = {{ status: 'open', due: {{ lt: 3 }} }};\n{extra}\nasync function page() {{ return await invoices.where(pred).count(); }}\n"));
        assert!(
            c.queries.is_empty() && !c.diagnostics.is_empty(),
            "{extra}: {:?}",
            diagnostics(&c)
        );
    }
}

#[test]
fn three_inline_optional_slots_plus_a_fold_exceed_the_same_cap() {
    let c = classify("import { invoices } from 'albedo/forge';\nimport { optional } from 'albedo/query';\nasync function page(f) {\n let q = invoices.where({ owner: optional(f.owner), status: optional(f.status), due: optional(f.due) });\n if (f.amount) q = q.where({ amount: f.amount });\n return await q.limit(2);\n}\n");
    assert!(c.queries.is_empty());
    assert!(
        c.diagnostics
            .iter()
            .any(|d| d.message.contains("at most 3")),
        "{:?}",
        diagnostics(&c)
    );
}
