//! T-0033 · Q1a slice 2 — named predicates imported one hop (Design 1, R3-1),
//! checked app-wide by [`classify_project`].

use std::collections::BTreeMap;
use std::fs;

use super::{classify_project, Classification};
use crate::forge::declare::CollectionDecl;

fn decls() -> BTreeMap<String, CollectionDecl> {
    let mut out = BTreeMap::new();
    out.insert(
        "invoices".to_string(),
        serde_json::from_str(r#"{ "fields": { "owner": "text", "status": "text", "due": "timestamp" } }"#)
            .expect("decl"),
    );
    out
}

/// A project under `src/` with `files`, classified.
fn project(files: &[(&str, &str)]) -> Classification {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    for (path, body) in files {
        let file = src.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, body).unwrap();
    }
    classify_project(&src, dir.path(), &decls()).expect("classify")
}

fn messages(c: &Classification) -> Vec<String> {
    c.diagnostics
        .iter()
        .map(|d| format!("{} — {}", d.at, d.message))
        .collect()
}

const PREDS: &str = "export const open = { status: \"open\", due: { lt: 100 } };\n";
const PAGE: &str = "import { invoices } from \"albedo/forge\";\nimport { open } from \"../lib/preds\";\nexport default async function P() {\n  return await invoices.where(open).count();\n}\n";

#[test]
fn an_imported_const_predicate_lowers_like_its_literal() {
    let imported = project(&[("lib/preds.ts", PREDS), ("routes/page.tsx", PAGE)]);
    assert!(imported.diagnostics.is_empty(), "{:?}", messages(&imported));
    assert_eq!(imported.queries.len(), 1);
    assert_eq!(imported.queries[0].imported.len(), 1);
    assert_eq!(imported.queries[0].imported[0].1, "open");
    let inline = project(&[(
        "routes/page.tsx",
        "import { invoices } from \"albedo/forge\";\nexport default async function P(x) {\n  return await invoices.where({ status: \"open\", due: { lt: x } }).count();\n}\n",
    )]);
    assert_eq!(imported.queries[0].canonical.id, inline.queries[0].canonical.id);
}

#[test]
fn export_as_and_index_modules_resolve() {
    let c = project(&[
        ("lib/preds/index.ts", "const base = { status: \"open\" };\nexport { base as open };\n"),
        (
            "routes/page.tsx",
            "import { invoices } from \"albedo/forge\";\nimport { open } from \"../lib/preds\";\nexport default async function P() {\n  return await invoices.where(open).count();\n}\n",
        ),
    ]);
    assert!(c.diagnostics.is_empty(), "{:?}", messages(&c));
    assert_eq!(c.queries.len(), 1);
}

#[test]
fn a_rule_broken_inside_an_imported_predicate_is_named_at_the_import_with_its_home_line() {
    let c = project(&[
        ("lib/preds.ts", "const base = {};\nexport const open = { ...base };\n"),
        ("routes/page.tsx", PAGE),
    ]);
    assert!(c.queries.is_empty());
    let m = messages(&c);
    assert!(
        m.iter().any(|m| m.starts_with("src/routes/page.tsx:4:31")
            && m.contains("from src/lib/preds.ts:2:")
            && m.contains("spread")),
        "{m:?}"
    );
}

#[test]
fn an_imported_predicate_used_any_other_way_anywhere_is_refused_and_its_queries_dropped() {
    let cases: &[(&[(&str, &str)], &str, &str)] = &[
        (
            &[("lib/preds.ts", "export const open = { status: \"open\" };\nopen.status = \"closed\";\n")],
            "src/lib/preds.ts:2:1",
            "written to outside a query",
        ),
        (
            &[
                ("lib/preds.ts", "export const open = { status: \"open\" };\n"),
                ("lib/tamper.ts", "import { open } from \"./preds\";\nObject.assign(open, { status: \"x\" });\n"),
            ],
            "src/lib/tamper.ts:2:15",
            "passed to a function",
        ),
        (
            &[
                ("lib/preds.ts", "export const open = { status: \"open\" };\n"),
                ("lib/again.ts", "export { open } from \"./preds\";\n"),
            ],
            "src/lib/again.ts:1:10",
            "re-exporting `open` is refused",
        ),
        (
            &[
                ("lib/preds.ts", "export const open = { status: \"open\" };\n"),
                ("lib/star.ts", "export * from \"./preds\";\n"),
            ],
            "src/lib/star.ts:1:1",
            "`export *` re-exports `open`",
        ),
        (
            &[
                ("lib/preds.ts", "export const open = { status: \"open\" };\n"),
                ("lib/alias.ts", "import { open } from \"./preds\";\nexport const copy = open;\n"),
            ],
            "src/lib/alias.ts:2:21",
            "used outside a query",
        ),
    ];
    for (extra, site, expected) in cases {
        let mut files: Vec<(&str, &str)> = extra.to_vec();
        files.push(("routes/page.tsx", PAGE));
        let c = project(&files);
        assert!(c.queries.is_empty(), "{site}: the query reading it stands: {:?}", c.queries);
        let m = messages(&c);
        assert!(
            m.iter().any(|m| m.starts_with(site) && m.contains(expected)),
            "expected `{expected}` at {site}, got {m:?}"
        );
    }
}

#[test]
fn an_import_that_is_not_a_const_object_is_refused_by_name() {
    let c = project(&[
        ("lib/preds.ts", "export let open = { status: \"open\" };\n"),
        ("routes/page.tsx", PAGE),
    ]);
    assert!(c.queries.is_empty());
    assert!(
        messages(&c).iter().any(|m| m.contains("by name")),
        "{:?}",
        messages(&c)
    );
}
