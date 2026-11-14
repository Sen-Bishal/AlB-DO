//! Adversarial project-boundary checks for T-0033.

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::transforms::forge_queries::{classify_project, Classification};
use std::collections::BTreeMap;
use std::fs;

fn declarations() -> BTreeMap<String, CollectionDecl> {
    BTreeMap::from([(
        "invoices".into(),
        serde_json::from_str(r#"{"fields":{"owner":"text","amount":"int"}}"#).unwrap(),
    )])
}

fn project(files: &[(&str, &str)]) -> Classification {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in files {
        let path = dir.path().join("src").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    classify_project(&dir.path().join("src"), dir.path(), &declarations()).unwrap()
}

#[test]
fn escaped_framework_specifiers_cannot_bypass_project_classification() {
    for source in [
        r#"import { invoices } from "albedo/\u0066orge"; invoices.count();"#,
        r#"import { optional } from "albedo/\u0071uery"; const stolen = optional;"#,
    ] {
        let result = project(&[("routes/page.tsx", source)]);
        assert!(
            result.refusal().is_some(),
            "unclassified framework value: {source}"
        );
    }
}

#[test]
fn escaped_predicate_import_cannot_hide_a_cross_module_mutation() {
    let result = project(&[
        ("lib/preds.ts", r#"export const open = { owner: "ada" };"#),
        (
            "routes/page.tsx",
            r#"import { invoices } from "albedo/forge"; import { open } from "../lib/preds"; invoices.where(open).count();"#,
        ),
        (
            "lib/tamper.ts",
            r#"import { \u006fpen as victim } from "./preds"; victim.owner = "mallory";"#,
        ),
    ]);
    assert!(result.queries.is_empty(), "mutable predicate was lowered");
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.at.module == "src/lib/tamper.ts"));
}

#[test]
fn crlf_unicode_coordinates_and_windows_canonical_root_are_source_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("page.tsx"),
        "import { invoices } from 'albedo/forge';\r\nconst label = 'é'; const x = invoices;\r\n",
    )
    .unwrap();
    // canonicalize uses the extended-length path representation on Windows.
    let base = fs::canonicalize(dir.path()).unwrap();
    let result = classify_project(&base.join("src"), &base, &declarations()).unwrap();
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].at.to_string(), "src/page.tsx:2:30");
}

#[test]
fn excluded_declarations_tests_and_generated_modules_stay_excluded() {
    let bad = "import { invoices } from 'albedo/forge'; const x = invoices;";
    let result = project(&[
        ("types.d.ts", bad),
        ("page.test.tsx", bad),
        (".albedo/dist/generated.js", bad),
        ("node_modules/pkg/index.js", bad),
        (
            "page.tsx",
            "export default function Page() { return <p>ok</p>; }",
        ),
    ]);
    assert!(result.refusal().is_none(), "{:?}", result.diagnostics);
}

#[test]
fn concurrent_project_classification_has_stable_ids_and_no_shared_call_sites() {
    let handles: Vec<_> = (0..8).map(|i| std::thread::spawn(move || {
        let source = format!("import {{ invoices }} from 'albedo/forge'; invoices.where({{ owner: '{i}' }}).count();");
        let result = project(&[("page.tsx", &source)]);
        assert!(result.diagnostics.is_empty());
        assert_eq!(result.queries.len(), 1);
        result.queries[0].canonical.id.clone()
    })).collect();
    let ids: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(ids.iter().all(|id| id == &ids[0]));
}
