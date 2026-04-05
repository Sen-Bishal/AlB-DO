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
