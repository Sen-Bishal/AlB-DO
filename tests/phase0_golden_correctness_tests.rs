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
    load_components_from(
        &project_root()
            .join("tests")
            .join("fixtures")
            .join("components"),
    )
}

fn load_components_from(
    components_root: &Path,
) -> (
    ProjectScanner,
    Vec<dom_render_compiler::parser::ParsedComponent>,
) {
    let scanner = ProjectScanner::new();
    let components = scanner
        .scan_directory(components_root)
        .expect("test-app components should scan");
    (scanner, components)
}

fn build_id_for_components_at(components_root: &Path) -> String {
    let (scanner, components) = load_components_from(components_root);
    let compiler = scanner.build_compiler(components);
    compiler
        .optimize_manifest_v2()
        .expect("manifest should optimize")
        .build_id
}

/// Copy the fixture components into `dest`, so the same sources can be built
/// from a different absolute location.
fn copy_fixture_components_to(dest: &Path) {
    let src = project_root()
        .join("tests")
        .join("fixtures")
        .join("components");
    fs::create_dir_all(dest).expect("destination should be creatable");
    for entry in fs::read_dir(&src).expect("fixture components should be readable") {
        let entry = entry.expect("directory entry should read");
        if entry.file_type().expect("file type should read").is_file() {
            fs::copy(entry.path(), dest.join(entry.file_name())).expect("fixture should copy");
        }
    }
}

/// `build_id` must identify the PROJECT, not the directory it happens to sit in.
///
/// It is a hash over every component's path + source hash, and those paths are
/// absolute (`ComponentManifestEntry.module_path` is `Component.file_path`
/// verbatim). Hashing them raw made the id follow the checkout around: moving
/// this repo from `A:\` to `C:\Development\ALKMY\AlB-DO` silently invalidated
/// the manifest golden below, because the same source produced a different id.
///
/// That is not a stale fixture — it is a build id that cannot be compared across
/// two machines, which is the only thing a build id is for. Nothing consumes
/// `build_id` yet, so this pins the property before something does.
#[test]
fn build_id_is_independent_of_where_the_project_lives_on_disk() {
    let first = tempfile::tempdir().expect("temp dir");
    let second = tempfile::tempdir().expect("temp dir");

    // Different roots, and different nesting depths, so a prefix that merely
    // happened to be the same length wouldn't hide a location dependency.
    let a = first.path().join("here");
    let b = second.path().join("somewhere").join("much").join("deeper");
    copy_fixture_components_to(&a);
    copy_fixture_components_to(&b);

    assert_eq!(
        build_id_for_components_at(&a),
        build_id_for_components_at(&b),
        "build_id must not change when the same project is built from a different path"
    );
}

/// The id still has to be a real fingerprint: same location, changed source =>
/// different id. Otherwise the test above could pass with a constant.
#[test]
fn build_id_still_changes_when_a_component_source_changes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().join("app");
    copy_fixture_components_to(&root);

    let before = build_id_for_components_at(&root);

    let target = root.join("Footer.jsx");
    let source = fs::read_to_string(&target).expect("fixture should read");
    fs::write(&target, format!("{source}\n// touched\n")).expect("fixture should write");

    assert_ne!(
        before,
        build_id_for_components_at(&root),
        "build_id must still track component source content"
    );
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

/// `build_id` is a hash *of* every component's `source_hash` — the very field
/// `normalize_source_hashes` already declares too unstable to assert. Pinning
/// the derived hash while normalizing its input is incoherent, and it made this
/// fixture a tripwire that has now gone stale twice for reasons unrelated to the
/// manifest's actual shape.
///
/// Concretely, `source_hash` is `xxh3_64` over raw source bytes and this repo has
/// no `.gitattributes`: git stores LF but checks out CRLF on Windows
/// (`git ls-files --eol` → `i/lf w/crlf`), so the same commit hashes differently
/// on a Windows dev box than on Linux CI. A golden cannot pin that.
///
/// This is NOT sweeping the real defect under the rug: `build_id` genuinely was
/// path-dependent (it hashed absolute paths, so moving the checkout changed it),
/// and that is fixed in `ManifestBuilder::build_build_id`. The property is now
/// asserted head-on by `build_id_is_independent_of_where_the_project_lives_on_disk`
/// and `build_id_still_changes_when_a_component_source_changes`, which fail with a
/// direct message instead of a 5 kB JSON diff.
fn normalize_build_id(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        if object.contains_key("build_id") {
            object.insert("build_id".to_string(), Value::String("<normalized>".to_string()));
        }
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
    normalize_build_id(&mut actual);

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
