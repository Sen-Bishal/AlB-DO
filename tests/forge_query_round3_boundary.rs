//! Filesystem failure and Windows path checks for the Q1a refusal boundary.

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::transforms::forge_queries::{classify_project, enforce_project};
use std::collections::BTreeMap;
use std::fs;

fn declarations() -> BTreeMap<String, CollectionDecl> {
    serde_json::from_str(r#"{"items":{"fields":{"title":"text"}}}"#).unwrap()
}

#[test]
fn unreadable_source_fails_before_any_query_report_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("broken.tsx"), [0xff, 0xfe, 0x00]).unwrap();
    let report = dir.path().join("query-report.txt");
    let error = enforce_project(&src, dir.path(), &declarations(), Some(&report)).unwrap_err();
    assert!(error.contains("broken.tsx") && error.contains("failed to read"), "{error}");
    assert!(!report.exists());
}

#[test]
fn long_unicode_path_and_canonical_windows_path_have_identical_call_sites() {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let src = root.join("src");
    let mut nested = src.clone();
    for _ in 0..6 {
        nested.push("query_path_with_spaces_é_and_more_characters");
    }
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("page.tsx"), "import { items } from 'albedo/forge';\r\nitems.count();\r\n").unwrap();
    let first = classify_project(&src, &root, &declarations()).unwrap();
    let second = classify_project(&fs::canonicalize(&src).unwrap(), &root, &declarations()).unwrap();
    assert!(first.diagnostics.is_empty());
    assert_eq!(first.queries.len(), 1);
    assert_eq!(first.queries[0].at, second.queries[0].at);
    assert_eq!(first.queries[0].canonical.id, second.queries[0].canonical.id);
    assert!(first.queries[0].at.module.starts_with("src/"));
    assert!(!first.queries[0].at.module.contains('\\'));
    assert_eq!(first.queries[0].at.line, 2);
    assert_eq!(first.queries[0].at.column, 1);
}
