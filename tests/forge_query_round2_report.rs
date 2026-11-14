//! Error/restart coverage for the pre-execution query-report boundary.

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::transforms::forge_queries::enforce_project;
use std::collections::BTreeMap;

#[test]
fn an_unwritable_report_cannot_allow_a_query_and_a_fresh_run_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let page = src.join("page.tsx");
    std::fs::write(
        &page,
        "import { items } from 'albedo/forge';\r\nitems.count();\r\n",
    )
    .unwrap();
    let decls: BTreeMap<String, CollectionDecl> =
        serde_json::from_str(r#"{"items":{"fields":{"title":"text"}}}"#).unwrap();
    let report = dir.path().join(".albedo/query-report.txt");
    // A directory at the report path fails writes on Windows and Unix without
    // relying on Unix permission bits or administrator privileges.
    std::fs::create_dir_all(&report).unwrap();
    let error = enforce_project(&src, dir.path(), &decls, Some(&report)).unwrap_err();
    assert!(error.contains("src/page.tsx:2:1"), "{error}");
    assert!(error.contains("compiled and lowered") && error.contains("Q1b"));
    assert!(!error.contains("Each query's SQL"));

    std::fs::remove_dir(&report).unwrap();
    let error = enforce_project(&src, dir.path(), &decls, Some(&report)).unwrap_err();
    assert!(error.contains("Each query's SQL"));
    assert!(std::fs::read_to_string(&report)
        .unwrap()
        .contains("SELECT COUNT(*)"));

    std::fs::write(
        &page,
        "export default function Page() { return <p>ok</p>; }",
    )
    .unwrap();
    assert!(enforce_project(&src, dir.path(), &decls, Some(&report)).is_ok());
    assert!(!report.exists());
}
