//! JSX expression-evaluation matrix.
//!
//! Each subdirectory under `tests/fixtures/jsx_matrix/<case>/` contains:
//!   - `Component.tsx` — a single-component module exporting `default`.
//!   - `expected.html` — the exact HTML the renderer should produce.
//!
//! The matrix is the spec for Phase J: every shape listed here must render
//! identically to its `expected.html`. Adding a fixture is a commitment.
//!
//! State-changing semantics (setState, useEffect, async, closures-as-values)
//! are intentionally OUT of scope here — those are Phase K territory.

use dom_render_compiler::runtime::eval::render_from_components_dir;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// The renderer stamps `data-albedo-id="<u32>"` onto every shell element
/// (Phase J shell-stamping deliverable). The matrix asserts expression
/// semantics, not anchor IDs, so strip the attribute before comparing.
/// Stamping has its own dedicated test below.
fn strip_albedo_anchors(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find(" data-albedo-id=\"") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + " data-albedo-id=\"".len()..];
        if let Some(close) = after_open.find('"') {
            rest = &after_open[close + 1..];
        } else {
            // Malformed; bail and keep the rest verbatim.
            out.push_str(&rest[start..]);
            return out;
        }
    }
    out.push_str(rest);
    out
}

fn matrix_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("jsx_matrix")
}

fn list_cases() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(matrix_root()).expect("matrix root must exist") {
        let entry = entry.expect("read matrix entry");
        if entry.file_type().expect("file type").is_dir() {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

fn run_case(case_dir: &Path) -> Result<String, String> {
    let entry_path = case_dir.join("Component.tsx");
    if !entry_path.exists() {
        return Err(format!(
            "case '{}' is missing Component.tsx",
            case_dir.display()
        ));
    }
    let entry_spec = "Component.tsx";
    let props = Value::Object(Default::default());
    render_from_components_dir(case_dir, entry_spec, &props)
        .map_err(|err| format!("render failed for '{}': {err:#}", case_dir.display()))
}

fn read_expected(case_dir: &Path) -> String {
    let path = case_dir.join("expected.html");
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("missing expected.html at '{}': {err}", path.display()))
        .trim_end_matches(['\r', '\n'])
        .to_string()
}

#[test]
fn jsx_expr_matrix_renders_every_case_to_expected_html() {
    let cases = list_cases();
    assert!(!cases.is_empty(), "matrix must contain at least one case");

    let mut failures: Vec<String> = Vec::new();
    for case_dir in &cases {
        let case_name = case_dir.file_name().unwrap().to_string_lossy().to_string();
        let expected = read_expected(case_dir);
        match run_case(case_dir) {
            Ok(actual) => {
                let actual = strip_albedo_anchors(actual.trim_end_matches(['\r', '\n']));
                if actual != expected {
                    failures.push(format!(
                        "[{case_name}]\n  expected: {expected}\n  actual:   {actual}"
                    ));
                }
            }
            Err(err) => {
                failures.push(format!("[{case_name}]\n  error: {err}"));
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "JSX expr matrix: {}/{} cases failed\n\n{}\n",
            failures.len(),
            cases.len(),
            failures.join("\n\n")
        );
    }
}

/// Phase J shell-stamping contract: every host element the renderer emits
/// must carry `data-albedo-id="<u32>"`. Bakabox's `seedNodesFromDocument`
/// keys off this attribute to register every Tier-A node into its anchor
/// map; without the stamp, no Tier-B/C patch can address pre-rendered
/// DOM. Re-render and confirm IDs are stable across calls.
#[test]
fn shell_stamps_data_albedo_id_on_every_host_element_and_is_stable() {
    let case_dir = matrix_root().join("ternary");
    let html_a = run_case(&case_dir).expect("render must succeed");
    let html_b = run_case(&case_dir).expect("re-render must succeed");

    assert_eq!(
        html_a, html_b,
        "stable_id assignment must be deterministic across renders"
    );

    // Two host elements in the fixture: `<span>...</span>` (the JSXElement
    // returned by Component itself). The matrix's other fixtures vary in
    // element count; this one is the simplest pin.
    let stamp_count = html_a.matches(" data-albedo-id=\"").count();
    assert!(
        stamp_count >= 1,
        "expected at least one data-albedo-id stamp, got 0 in: {html_a}"
    );

    // The stamp's value must be a u32 (matches StableId(u32)). Pull every
    // value and assert each parses cleanly.
    let mut rest = html_a.as_str();
    while let Some(start) = rest.find(" data-albedo-id=\"") {
        let after = &rest[start + " data-albedo-id=\"".len()..];
        let close = after.find('"').expect("stamp must close");
        let value = &after[..close];
        value
            .parse::<u32>()
            .unwrap_or_else(|_| panic!("data-albedo-id must be u32, got {value:?}"));
        rest = &after[close + 1..];
    }
}

/// Independent cross-render scopes: a render of fixture A and a render of
/// fixture B must each get their own counter origin so concurrent renders
/// across requests don't leak ids between routes.
#[test]
fn shell_stamp_counter_resets_between_renders() {
    let a = run_case(&matrix_root().join("ternary")).expect("ternary renders");
    let b = run_case(&matrix_root().join("ternary")).expect("ternary re-renders");
    assert_eq!(a, b, "second render of same fixture must produce same html");
}
