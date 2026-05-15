use dom_render_compiler::scanner::ProjectScanner;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_test_app_components() -> (
    ProjectScanner,
    Vec<dom_render_compiler::parser::ParsedComponent>,
) {
    let scanner = ProjectScanner::new();
    let components_root = project_root()
        .join("tests")
        .join("fixtures")
        .join("components");
    let components = scanner
        .scan_directory(&components_root)
        .expect("test-app components should scan");
    (scanner, components)
}

fn normalize_generated_at(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("generated_at") {
            object.insert(
                "generated_at".to_string(),
                Value::String("<normalized>".to_string()),
            );
        }
    }
}

fn normalize_module_paths(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if key == "module_path" {
                    if let Some(path) = child.as_str() {
                        *child = Value::String(normalize_path_string(path));
                    }
                } else {
                    normalize_module_paths(child);
                }
            }
        }
        Value::Array(array) => {
            for child in array {
                normalize_module_paths(child);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn normalize_source_hashes(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                if key == "source_hash" {
                    *child = Value::String("<normalized>".to_string());
                } else {
                    normalize_source_hashes(child);
                }
            }
        }
        Value::Array(array) => {
            for child in array {
                normalize_source_hashes(child);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn normalize_path_string(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let marker = "tests/fixtures/components/";
    if let Some(index) = normalized.find(marker) {
        return normalized[index..].to_string();
    }
    normalized
}

fn assert_json_fixture(path: &Path, mut actual: Value) {
    normalize_generated_at(&mut actual);
    normalize_module_paths(&mut actual);
    normalize_source_hashes(&mut actual);

    let update = std::env::var("ALBEDO_UPDATE_GOLDENS").ok().as_deref() == Some("1");
    if update {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("golden fixture directory should be creatable");
        }
        let payload = serde_json::to_vec_pretty(&actual).expect("normalized JSON should serialize");
        fs::write(path, payload).expect("golden fixture should be writable");
        return;
    }

    let expected_raw = fs::read(path).expect("golden fixture should exist");
    let expected: Value = serde_json::from_slice(&expected_raw).expect("fixture should be JSON");
    assert_eq!(actual, expected, "fixture mismatch at {}", path.display());
}

#[test]
fn test_golden_manifest_v2_for_test_app_components() {
    let (scanner, components) = load_test_app_components();
    let compiler = scanner.build_compiler(components);
    let manifest = compiler
        .optimize_manifest_v2()
        .expect("manifest should optimize");
    let actual = serde_json::to_value(manifest).expect("manifest should serialize to JSON value");

    let fixture = project_root()
        .join("tests")
        .join("fixtures")
        .join("golden")
        .join("manifest_v2_test_app_components.json");
    assert_json_fixture(&fixture, actual);
}

/// Phase J static-slicer dedup contract: when a Tier-A root inlines its
/// Tier-A descendants into one HTML string, those descendants must NOT
/// also appear as separate `tier_a_root` entries (and the shell's
/// `body_open` must not contain their `__SLOT_` placeholders). Tier-B
/// islands nested inside an inlined Tier-A subtree keep their own body
/// anchor — they are not absorbed by the parent's render.
///
/// This test is the regression guard for the manifest builder's
/// `traverse` logic. If a refactor reintroduces double-counting, this
/// fires before the golden fixture catches it.
#[test]
fn test_static_slicer_dedup_does_not_double_count_inlined_tier_a_children() {
    let (scanner, components) = load_test_app_components();
    let compiler = scanner.build_compiler(components);
    let manifest = compiler
        .optimize_manifest_v2()
        .expect("manifest should optimize");

    let route = manifest
        .routes
        .get("/")
        .expect("test-app fixture exposes route '/'");

    let tier_a_root_ids: Vec<&str> = route
        .tier_a_root
        .iter()
        .map(|n| n.component_id.as_str())
        .collect();
    assert_eq!(
        tier_a_root_ids,
        vec!["App"],
        "App is the only true Tier-A root; Header/Hero/Features/etc. \
         are inlined into App.html and must not appear as separate roots"
    );

    let tier_b_ids: Vec<&str> = route
        .tier_b
        .iter()
        .map(|n| n.component_id.as_str())
        .collect();
    assert_eq!(
        tier_b_ids,
        vec!["Button"],
        "Button is the only Tier-B in the fixture and must remain in tier_b \
         even though every Tier-A ancestor was deduped"
    );

    let body_open = route.shell.body_open.as_str();
    assert!(
        body_open.contains("__SLOT___a_app_0"),
        "App's tier-A slot must remain in body_open"
    );
    assert!(
        body_open.contains("__b_button_1"),
        "Button's tier-B anchor must remain in body_open even when its \
         Tier-A ancestors are inlined into App's html"
    );
    for inlined in [
        "__a_header_",
        "__a_navigation_",
        "__a_features_",
        "__a_featurecard_",
        "__a_footer_",
        "__a_heroimage_",
    ] {
        assert!(
            !body_open.contains(inlined),
            "body_open must not contain placeholder for inlined child {inlined:?}, got: {body_open}"
        );
    }

    // Sanity: App's inlined HTML must actually contain the rendered
    // descendants — the dedup is only safe because the renderer already
    // emitted them as one string.
    let app_html = &route.tier_a_root[0].html;
    for fragment in [
        "class=\"App\"",
        "class=\"header\"",
        "class=\"hero\"",
        "class=\"features\"",
    ] {
        assert!(
            app_html.contains(fragment),
            "App's tier-A html must inline descendant fragment {fragment:?}, got: {app_html}"
        );
    }
}

#[test]
fn test_golden_canonical_ir_for_test_app_components() {
    let (scanner, components) = load_test_app_components();
    let canonical_ir = scanner.build_canonical_ir(&components);
    let actual =
        serde_json::to_value(canonical_ir).expect("canonical IR should serialize to JSON value");

    let fixture = project_root()
        .join("tests")
        .join("fixtures")
        .join("golden")
        .join("canonical_ir_test_app_components.json");
    assert_json_fixture(&fixture, actual);
}
