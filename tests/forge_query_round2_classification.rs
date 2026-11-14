//! Adversarial binding and cross-module checks for T-0033 verification round 2.

use std::collections::BTreeMap;
use std::fs;

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::forge::query::CollectionSchema;
use dom_render_compiler::transforms::forge_queries::{
    classify_project, classify_source, Classification,
};

fn declarations() -> BTreeMap<String, CollectionDecl> {
    BTreeMap::from([(
        "invoices".into(),
        serde_json::from_str(
            r#"{"fields":{"owner":"text","amount":"int"},"partition_by":"owner"}"#,
        )
        .unwrap(),
    )])
}

fn source(body: &str) -> Classification {
    let schemas = declarations()
        .iter()
        .map(|(name, decl)| (name.clone(), CollectionSchema::from_decl(name, decl)))
        .collect();
    classify_source(
        "src/routes/review.tsx",
        &format!("import {{ invoices }} from 'albedo/forge';\n{body}"),
        false,
        &schemas,
    )
}

fn project(extra: (&str, &str)) -> Classification {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("lib/preds.ts", "export const open = { owner: 'ada' };"),
        (
            "routes/page.tsx",
            "import { invoices } from 'albedo/forge'; import { open } from '../lib/preds'; invoices.where(open).count();",
        ),
        extra,
    ] {
        let path = dir.path().join("src").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    classify_project(&dir.path().join("src"), dir.path(), &declarations()).unwrap()
}

#[test]
fn assignment_targets_are_framework_value_references_too() {
    let mut missed = Vec::new();
    for body in [
        "invoices = null;",
        "({ x: invoices } = { x: null });",
        "[invoices] = [null];",
        "for (invoices of []) {}",
        "for (invoices in {}) {}",
    ] {
        let result = source(body);
        if result.refusal().is_none() {
            missed.push(body);
        } else {
            assert!(result.diagnostics.iter().any(|d| d.at.line == 2));
        }
    }
    assert!(missed.is_empty(), "unclassified references: {missed:?}");
}

#[test]
fn namespace_access_cannot_hide_mutation_of_an_imported_predicate() {
    for body in [
        "import * as all from './preds'; all.open.owner = 'mallory';",
        "import * as all from './preds'; all['open'].owner = 'mallory';",
        "import * as all from './preds'; all[name].owner = 'mallory';",
        "import * as all from './preds'; Object.assign(all.open, { owner: 'mallory' });",
        "import * as all from './preds'; const escaped = all;",
        "import * as all from './preds'; const box = { all }; box.all.open.owner = 'mallory';",
        "import * as all from './preds'; const component = <all.open />;",
    ] {
        let result = project(("lib/tamper.tsx", body));
        assert!(
            result.queries.is_empty(),
            "mutable predicate was lowered: {body}"
        );
        assert!(result
            .diagnostics
            .iter()
            .any(|d| d.at.module == "src/lib/tamper.tsx"));
    }
}

#[test]
fn unrelated_or_shadowed_namespace_members_are_ordinary_javascript() {
    for body in [
        "import * as all from './preds'; console.log(all.other);",
        "import * as all from './preds'; function f(all) { all.open.owner = 'mallory'; }",
        "import type * as all from './preds'; type P = typeof all.open;",
        "import * as all from './preds'; type P = typeof all.open;",
        "import * as all from './preds'; export type { all };",
        "import * as all from './preds'; export { type all };",
        "import * as all from './preds'; const component = <all.unrelated />;",
    ] {
        let result = project(("lib/ordinary.tsx", body));
        assert!(
            result.diagnostics.is_empty(),
            "{body}: {:?}",
            result.diagnostics
        );
        assert_eq!(result.queries.len(), 1);
    }
}

#[test]
fn reexport_star_whitespace_and_comments_cannot_bypass_the_project_census() {
    let mut missed = Vec::new();
    for body in [
        "export\n* from './preds';",
        "export/* comment */* from './preds';",
        "export\t* from './preds';",
        "export * as namespace from './preds';",
        "export { 'open' as alias } from './preds';",
        "import * as all from './preds'; export { all };",
        "import * as all from './preds'; export { all as box };",
    ] {
        let result = project(("lib/reexport.ts", body));
        if !result.queries.is_empty() {
            missed.push(body);
        } else {
            assert!(result
                .diagnostics
                .iter()
                .any(|d| d.at.module == "src/lib/reexport.ts"));
        }
    }
    assert!(missed.is_empty(), "re-exports bypassed census: {missed:?}");
}

#[test]
fn shadowed_collection_assignment_is_ordinary_javascript() {
    let result = source("function helper(invoices) { invoices = null; return invoices; }");
    assert!(result.refusal().is_none(), "{:?}", result.diagnostics);
}

#[test]
fn framework_module_string_escapes_cannot_bypass_project_classification() {
    for body in [
        "import { invoices } from 'albedo/\\x66orge'; const stolen = invoices;",
        "import { invoices } from '\\x61lbedo/forge'; const stolen = invoices;",
        "import { invoices } from 'albedo/fo\\\nrge'; const stolen = invoices;",
        "import { optional } from 'albedo/\\x71uery'; const stolen = optional;",
    ] {
        let result = project(("lib/escaped.ts", body));
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.at.module == "src/lib/escaped.ts"),
            "unclassified import: {body}"
        );
    }
}

#[test]
fn crlf_and_astral_unicode_keep_original_character_coordinates() {
    let result = source("const marker = '😀';\r\nconst marker2 = '😀'; const bad = invoices;\r\n");
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].at.line, 3);
    assert_eq!(result.diagnostics[0].at.column, 35);
    assert!(!result.diagnostics[0].at.generated);
}
