use dom_render_compiler::bundler::emit::{
    emit_bundle_artifacts_to_dir, emit_bundle_artifacts_to_dir_with_sources, emit_bundle_plan_json,
    emit_bundle_runtime_map_json, emit_precompiled_runtime_modules_json,
    emit_route_prefetch_manifest_json, emit_static_slices_json, emit_vendor_chunk_modules,
    emit_wrapper_modules, BUNDLE_PLAN_FILENAME, BUNDLE_PRECOMPILED_MODULES_FILENAME,
    BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME, BUNDLE_RUNTIME_MAP_FILENAME,
    BUNDLE_STATIC_SLICES_FILENAME,
};
use dom_render_compiler::bundler::plan::{build_bundle_plan, BundlePlanOptions};
use dom_render_compiler::manifest::schema::{
    ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier, VendorChunk,
};
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;
use walkdir::WalkDir;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("bundler")
        .join(name)
}

fn read_fixture(name: &str) -> String {
    fs::read_to_string(fixture_path(name))
        .expect("fixture should exist")
        .replace("\r\n", "\n")
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}

fn normalize_hashes_for_snapshot(value: &str) -> String {
    let normalized = normalize_newlines(value);
    let hash_re = Regex::new(r"(__albedo__/(?:wrappers|vendor)/)[0-9a-f]{16}_").unwrap();
    let normalized = hash_re.replace_all(&normalized, "${1}<HASH>_").to_string();
    let source_hash_re = Regex::new(r#""source_hash":\s*\d+"#).unwrap();
    source_hash_re
        .replace_all(&normalized, "\"source_hash\": <HASH64>")
        .to_string()
}

fn normalize_snapshot(value: &str) -> String {
    normalize_hashes_for_snapshot(value).trim_end().to_string()
}

fn component(id: u64, module_path: &str, dependencies: Vec<u64>) -> ComponentManifestEntry {
    ComponentManifestEntry {
        id,
        name: format!("C{id}"),
        module_path: module_path.to_string(),
        tier: Tier::C,
        weight_bytes: 2048,
        priority: 1.0,
        dependencies,
        can_defer: true,
        hydration_mode: HydrationMode::OnVisible,
    }
}

fn fixture_manifest() -> RenderManifestV2 {
    RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-20T00:00:00Z".to_string(),
        components: vec![
            component(3, "src/routes/home.tsx", vec![1, 2]),
            component(1, "src/components/header.tsx", vec![]),
            component(2, "src/components/hero.tsx", vec![1]),
            component(9, "/repo/node_modules/react/index.js", vec![]),
        ],
        parallel_batches: vec![vec![1, 9], vec![2], vec![3]],
        critical_path: vec![1, 2, 3],
        vendor_chunks: vec![VendorChunk {
            chunk_name: "vendor.core".to_string(),
            packages: vec!["react".to_string()],
        }],
        ..RenderManifestV2::legacy_defaults()
    }
}

fn fixture_static_slice_manifest() -> RenderManifestV2 {
    RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-22T00:00:00Z".to_string(),
        components: vec![
            ComponentManifestEntry {
                id: 1,
                name: "TierAEligible".to_string(),
                module_path: "src/components/tier-a-eligible.tsx".to_string(),
                tier: Tier::A,
                weight_bytes: 1024,
                priority: 1.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: HydrationMode::None,
            },
            ComponentManifestEntry {
                id: 2,
                name: "TierAHook".to_string(),
                module_path: "src/components/tier-a-hook.tsx".to_string(),
                tier: Tier::A,
                weight_bytes: 1024,
                priority: 1.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: HydrationMode::None,
            },
            ComponentManifestEntry {
                id: 3,
                name: "TierB".to_string(),
                module_path: "src/components/tier-b.tsx".to_string(),
                tier: Tier::B,
                weight_bytes: 4096,
                priority: 1.0,
                dependencies: vec![1, 2],
                can_defer: true,
                hydration_mode: HydrationMode::OnIdle,
            },
        ],
        parallel_batches: vec![vec![1, 2], vec![3]],
        critical_path: vec![3],
        vendor_chunks: Vec::new(),
        ..RenderManifestV2::legacy_defaults()
    }
}

fn fixture_static_slice_sources() -> HashMap<String, String> {
    let mut sources = HashMap::new();
    sources.insert(
        "src/components/tier-a-eligible.tsx".to_string(),
        "export default function TierAEligible(props){return '<main>'+props.title+'</main>';}"
            .to_string(),
    );
    sources.insert(
        "src/components/tier-a-hook.tsx".to_string(),
        "export default function TierAHook(){const [value]=useState(0);return String(value);}"
            .to_string(),
    );
    sources.insert(
        "src/components/tier-b.tsx".to_string(),
        "export default function TierB(props){return '<section>'+props.label+'</section>';}"
            .to_string(),
    );
    sources
}

fn normalize_precompiled_snapshot(value: &str) -> String {
    let normalized = normalize_snapshot(value);
    let compiled_script_re = Regex::new(r#""compiled_script":\s*"(?:\\.|[^"\\])*""#).unwrap();
    compiled_script_re
        .replace_all(&normalized, "\"compiled_script\": \"<SCRIPT>\"")
        .to_string()
}

fn snapshot_artifacts(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
    {
        let rel = entry
            .path()
            .strip_prefix(root)
            .expect("artifact should be under root")
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(entry.path()).expect("artifact should be readable");
        files.insert(rel, bytes);
    }
    files
}

#[test]
fn test_wrapper_sources_match_golden_fixtures() {
    let plan = build_bundle_plan(&fixture_manifest(), &BundlePlanOptions::default());
    let wrappers = emit_wrapper_modules(&plan);

    let home_wrapper_path = plan
        .modules
        .iter()
        .find(|module| module.module_path == "src/routes/home.tsx")
        .expect("home module should exist")
        .wrapper_module_path
        .clone();
    let header_wrapper_path = plan
        .modules
        .iter()
        .find(|module| module.module_path == "src/components/header.tsx")
        .expect("header module should exist")
        .wrapper_module_path
        .clone();

    let home_source = wrappers
        .get(&home_wrapper_path)
        .expect("home wrapper source should exist");
    let header_source = wrappers
        .get(&header_wrapper_path)
        .expect("header wrapper source should exist");

    assert_eq!(
        normalize_newlines(home_source),
        read_fixture("home_wrapper.mjs")
    );
    assert_eq!(
        normalize_newlines(header_source),
        read_fixture("header_wrapper.mjs")
    );
}

#[test]
fn test_vendor_chunk_sources_match_golden_fixtures() {
    let plan = build_bundle_plan(&fixture_manifest(), &BundlePlanOptions::default());
    let vendor_chunks = emit_vendor_chunk_modules(&plan);

    assert_eq!(vendor_chunks.len(), 1);
    let source = vendor_chunks
        .values()
        .next()
        .expect("vendor source should exist");
    assert_eq!(normalize_newlines(source), read_fixture("vendor_core.mjs"));
}

#[test]
fn test_bundle_plan_json_matches_golden_fixture() {
    let plan = build_bundle_plan(&fixture_manifest(), &BundlePlanOptions::default());
    let json = emit_bundle_plan_json(&plan).expect("bundle plan JSON should serialize");
    assert_eq!(
        normalize_snapshot(&json),
        normalize_snapshot(&read_fixture("bundle_plan.json"))
    );
}

#[test]
fn test_bundle_runtime_map_json_matches_golden_fixture() {
    let plan = build_bundle_plan(&fixture_manifest(), &BundlePlanOptions::default());
    let json =
        emit_bundle_runtime_map_json(&plan).expect("bundle runtime map JSON should serialize");
    assert_eq!(
        normalize_snapshot(&json),
        normalize_snapshot(&read_fixture("bundle_runtime_map.json"))
    );
}

#[test]
fn test_route_prefetch_manifest_json_matches_golden_fixture() {
    let plan = build_bundle_plan(&fixture_manifest(), &BundlePlanOptions::default());
    let json = emit_route_prefetch_manifest_json(&plan)
        .expect("route prefetch manifest JSON should serialize");
    assert_eq!(
        normalize_snapshot(&json),
        normalize_snapshot(&read_fixture("route_prefetch_manifest.json"))
    );
}

#[test]
fn test_emit_bundle_artifacts_is_byte_identical_across_runs() {
    let plan = build_bundle_plan(&fixture_manifest(), &BundlePlanOptions::default());

    let out_a = tempdir().unwrap();
    let out_b = tempdir().unwrap();

    let report_a = emit_bundle_artifacts_to_dir(&plan, out_a.path()).unwrap();
    let report_b = emit_bundle_artifacts_to_dir(&plan, out_b.path()).unwrap();

    assert_eq!(report_a.artifacts, report_b.artifacts);

    let snapshot_a = snapshot_artifacts(out_a.path());
    let snapshot_b = snapshot_artifacts(out_b.path());

    assert_eq!(snapshot_a, snapshot_b);
    assert!(snapshot_a.contains_key(BUNDLE_PLAN_FILENAME));
    assert!(snapshot_a.contains_key(BUNDLE_RUNTIME_MAP_FILENAME));
    assert!(snapshot_a.contains_key(BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME));
    assert!(snapshot_a
        .keys()
        .any(|path| path.starts_with("__albedo__/wrappers/")));
    assert!(snapshot_a
        .keys()
        .any(|path| path.starts_with("__albedo__/vendor/")));
    assert!(snapshot_a
        .keys()
        .any(|path| path.ends_with("_src_routes_home_tsx.mjs")));
}

#[test]
fn test_static_slices_json_matches_golden_fixture() {
    let manifest = fixture_static_slice_manifest();
    let sources = fixture_static_slice_sources();
    let json =
        emit_static_slices_json(&manifest, &sources).expect("static slices JSON should serialize");
    assert_eq!(
        normalize_snapshot(&json),
        normalize_snapshot(&read_fixture("static_slices.json"))
    );
}

#[test]
fn test_emit_bundle_artifacts_with_sources_includes_static_slices() {
    let manifest = fixture_static_slice_manifest();
    let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());
    let sources = fixture_static_slice_sources();
    let out = tempdir().unwrap();

    let report =
        emit_bundle_artifacts_to_dir_with_sources(&plan, &manifest, &sources, out.path()).unwrap();
    assert!(report
        .artifacts
        .iter()
        .any(|artifact| artifact.relative_path == BUNDLE_STATIC_SLICES_FILENAME));

    let static_path = out.path().join(BUNDLE_STATIC_SLICES_FILENAME);
    assert!(static_path.is_file());
    let emitted = fs::read_to_string(static_path).unwrap();
    assert_eq!(
        normalize_snapshot(&emitted),
        normalize_snapshot(&read_fixture("static_slices.json"))
    );
}

#[test]
fn test_precompiled_runtime_modules_json_matches_golden_fixture() {
    let manifest = fixture_static_slice_manifest();
    let sources = fixture_static_slice_sources();
    let json = emit_precompiled_runtime_modules_json(&manifest, &sources)
        .expect("precompiled runtime modules JSON should serialize");
    assert_eq!(
        normalize_precompiled_snapshot(&json),
        normalize_precompiled_snapshot(&read_fixture("precompiled_runtime_modules.json"))
    );
}

#[test]
fn test_emit_bundle_artifacts_with_sources_includes_precompiled_modules() {
    let manifest = fixture_static_slice_manifest();
    let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());
    let sources = fixture_static_slice_sources();
    let out = tempdir().unwrap();

    let report =
        emit_bundle_artifacts_to_dir_with_sources(&plan, &manifest, &sources, out.path()).unwrap();
    assert!(report
        .artifacts
        .iter()
        .any(|artifact| artifact.relative_path == BUNDLE_PRECOMPILED_MODULES_FILENAME));

    let precompiled_path = out.path().join(BUNDLE_PRECOMPILED_MODULES_FILENAME);
    assert!(precompiled_path.is_file());
    let emitted = fs::read_to_string(precompiled_path).unwrap();
    assert_eq!(
        normalize_precompiled_snapshot(&emitted),
        normalize_precompiled_snapshot(&read_fixture("precompiled_runtime_modules.json"))
    );
}
