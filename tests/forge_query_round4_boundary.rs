//! Refusal remains deterministic across failed builds and source repairs.

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::transforms::forge_queries::{classify_project, enforce_project};
use std::collections::BTreeMap;
use std::fs;

fn declarations() -> BTreeMap<String, CollectionDecl> {
    serde_json::from_str(r#"{"items":{"fields":{"label":"text"}}}"#).unwrap()
}

#[test]
fn a_missing_source_tree_is_an_error_instead_of_an_empty_success() {
    let dir = tempfile::tempdir().unwrap();
    let result = classify_project(&dir.path().join("missing"), dir.path(), &declarations());
    assert!(result.is_err(), "unvisited source was accepted: {result:?}");
}

#[test]
fn repaired_source_removes_a_stale_report_and_a_retry_recreates_identical_output() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir(&src).unwrap();
    let page = src.join("page.tsx");
    let report = dir.path().join(".albedo/query-report.txt");
    let query =
        "import { items } from 'albedo/forge';\r\nitems.where({ label: 'x' }).limit(1);\r\n";
    let mut first = None;
    for _ in 0..3 {
        fs::write(&page, query).unwrap();
        let refusal =
            enforce_project(&src, dir.path(), &declarations(), Some(&report)).unwrap_err();
        assert!(refusal.contains("compiled and lowered") && refusal.contains("Q1b"));
        let output = fs::read(&report).unwrap();
        if let Some(expected) = &first {
            assert_eq!(&output, expected);
        } else {
            first = Some(output);
        }
        fs::write(
            &page,
            "export default function Page() { return <p>fixed</p>; }",
        )
        .unwrap();
        enforce_project(&src, dir.path(), &declarations(), Some(&report)).unwrap();
        assert!(!report.exists());
    }
}

#[test]
fn report_storage_failure_does_not_hide_the_source_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(
        src.join("page.tsx"),
        "import { items } from 'albedo/forge';\nitems.count();",
    )
    .unwrap();
    let report = dir.path().join("report-is-a-directory");
    fs::create_dir(&report).unwrap();
    let error = enforce_project(&src, dir.path(), &declarations(), Some(&report)).unwrap_err();
    assert!(error.contains("src/page.tsx:2:1"), "{error}");
    assert!(
        error.contains("compiled and lowered") && error.contains("Q1b"),
        "{error}"
    );
    assert!(
        !error.contains("Each query's SQL"),
        "claimed to have written an absent report: {error}"
    );
}
