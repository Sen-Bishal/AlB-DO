use super::classify::BundleClass;
use super::plan::BundlePlan;
use super::precompiled::build_precompiled_runtime_modules_artifact;
use super::rewrite::{build_wrapper_module_source, RewriteAction};
use super::static_slice::build_bundle_static_slice_manifest;
use crate::manifest::schema::{DomPosition, PrecompiledRuntimeModulesArtifact, RenderManifestV2};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const BUNDLE_PLAN_FILENAME: &str = "bundle-plan.json";
pub const BUNDLE_RUNTIME_MAP_FILENAME: &str = "bundle-runtime-map.json";
pub const BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME: &str = "route-prefetch-manifest.json";
pub const BUNDLE_STATIC_SLICES_FILENAME: &str = "static-slices.json";
pub const BUNDLE_PRECOMPILED_MODULES_FILENAME: &str = "precompiled-runtime-modules.json";
pub const BUNDLE_WT_BOOTSTRAP_FILENAME: &str = "_albedo/wt-bootstrap.js";
pub const BUNDLE_RUNTIME_MAP_VERSION: &str = "1.0";
pub const BUNDLE_ROUTE_PREFETCH_MANIFEST_VERSION: &str = "1.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedArtifact {
    pub relative_path: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleEmitReport {
    pub output_dir: PathBuf,
    pub artifacts: Vec<EmittedArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleRuntimeMap {
    pub version: String,
    pub plan_version: String,
    pub entry_component_id: Option<u64>,
    pub modules: Vec<BundleRuntimeModule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleRuntimeModule {
    pub component_id: u64,
    pub module_path: String,
    pub class: BundleClass,
    pub dependency_ids: Vec<u64>,
    pub wrapper_module: String,
    pub vendor_chunks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dom_position: Option<DomPosition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutePrefetchManifest {
    pub version: String,
    pub plan_version: String,
    pub routes: Vec<RoutePrefetchRoute>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutePrefetchRoute {
    pub entry_component_id: u64,
    pub entry_module: String,
    pub prefetch_modules: Vec<String>,
    pub vendor_chunks: Vec<String>,
}

pub fn emit_bundle_plan_json(plan: &BundlePlan) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(plan)
}

pub fn emit_bundle_runtime_map_json(plan: &BundlePlan) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&build_bundle_runtime_map(plan))
}

pub fn emit_route_prefetch_manifest_json(plan: &BundlePlan) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&build_route_prefetch_manifest(plan))
}

pub fn emit_static_slices_json(
    manifest: &RenderManifestV2,
    module_sources: &HashMap<String, String>,
) -> Result<String, serde_json::Error> {
    let static_manifest = build_bundle_static_slice_manifest(manifest, module_sources);
    serde_json::to_string_pretty(&static_manifest)
}

pub fn emit_precompiled_runtime_modules_json(
    manifest: &RenderManifestV2,
    module_sources: &HashMap<String, String>,
) -> Result<String, serde_json::Error> {
    let precompiled: PrecompiledRuntimeModulesArtifact =
        build_precompiled_runtime_modules_artifact(manifest, module_sources);
    serde_json::to_string_pretty(&precompiled)
}

pub fn emit_wrapper_modules(plan: &BundlePlan) -> BTreeMap<String, String> {
    let mut emitted = BTreeMap::new();

    for action in &plan.rewrite_actions {
        if let RewriteAction::WrapModule {
            source_module,
            wrapper_module,
            ..
        } = action
        {
            emitted
                .entry(wrapper_module.clone())
                .or_insert_with(|| build_wrapper_module_source(source_module));
        }
    }

    emitted
}

pub fn emit_vendor_chunk_modules(plan: &BundlePlan) -> BTreeMap<String, String> {
    let mut emitted = BTreeMap::new();
    let mut chunks = plan.vendor_chunks.clone();
    chunks.sort_by(|left, right| {
        left.chunk_name
            .cmp(&right.chunk_name)
            .then_with(|| left.packages.cmp(&right.packages))
            .then_with(|| left.component_ids.cmp(&right.component_ids))
    });

    for chunk in chunks {
        let path = stable_vendor_chunk_module_path(&chunk.chunk_name);
        emitted
            .entry(path)
            .or_insert_with(|| build_vendor_chunk_module_source(&chunk.packages));
    }

    emitted
}

pub fn build_bundle_runtime_map(plan: &BundlePlan) -> BundleRuntimeMap {
    let mut vendor_chunk_paths = BTreeMap::new();
    for chunk in &plan.vendor_chunks {
        vendor_chunk_paths.insert(
            chunk.chunk_name.clone(),
            stable_vendor_chunk_module_path(&chunk.chunk_name),
        );
    }

    let mut vendor_links: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for action in &plan.rewrite_actions {
        if let RewriteAction::LinkVendorChunk {
            component_id,
            chunk_name,
        } = action
        {
            if let Some(path) = vendor_chunk_paths.get(chunk_name) {
                vendor_links
                    .entry(*component_id)
                    .or_default()
                    .insert(path.clone());
            }
        }
    }

    let modules = plan
        .modules
        .iter()
        .map(|module| BundleRuntimeModule {
            component_id: module.component_id,
            module_path: module.module_path.clone(),
            class: module.class,
            dependency_ids: module.dependency_ids.clone(),
            wrapper_module: normalize_relative_artifact_path(&module.wrapper_module_path),
            vendor_chunks: vendor_links
                .get(&module.component_id)
                .map(|paths| paths.iter().cloned().collect())
                .unwrap_or_default(),
            dom_position: module.dom_position.clone(),
        })
        .collect();

    BundleRuntimeMap {
        version: BUNDLE_RUNTIME_MAP_VERSION.to_string(),
        plan_version: plan.version.clone(),
        entry_component_id: plan.entry_component_id,
        modules,
    }
}

pub fn build_route_prefetch_manifest(plan: &BundlePlan) -> RoutePrefetchManifest {
    let runtime_map = build_bundle_runtime_map(plan);

    let modules_by_id: BTreeMap<u64, &BundleRuntimeModule> = runtime_map
        .modules
        .iter()
        .map(|module| (module.component_id, module))
        .collect();

    let mut entry_ids = runtime_map
        .modules
        .iter()
        .filter(|module| module.class == BundleClass::Entry)
        .map(|module| module.component_id)
        .collect::<Vec<_>>();
    if entry_ids.is_empty() {
        if let Some(entry_id) = runtime_map.entry_component_id {
            entry_ids.push(entry_id);
        }
    }
    entry_ids.sort_unstable();
    entry_ids.dedup();

    let mut routes = Vec::new();
    for entry_id in entry_ids {
        let Some(entry_module) = modules_by_id.get(&entry_id) else {
            continue;
        };

        let mut visited = BTreeSet::new();
        let mut stack = vec![entry_id];
        while let Some(component_id) = stack.pop() {
            if !visited.insert(component_id) {
                continue;
            }
            if let Some(module) = modules_by_id.get(&component_id) {
                for dependency_id in &module.dependency_ids {
                    stack.push(*dependency_id);
                }
            }
        }

        let mut prefetch_candidates = runtime_map
            .modules
            .iter()
            .filter(|module| {
                visited.contains(&module.component_id) && module.class != BundleClass::Deferred
            })
            .collect::<Vec<_>>();
        prefetch_candidates.sort_by(|left, right| {
            left.component_id
                .cmp(&right.component_id)
                .then_with(|| left.module_path.cmp(&right.module_path))
        });
        let prefetch_modules = prefetch_candidates
            .into_iter()
            .map(|module| module.wrapper_module.clone())
            .collect::<Vec<_>>();

        let mut vendor_chunks = runtime_map
            .modules
            .iter()
            .filter(|module| {
                visited.contains(&module.component_id) && module.class != BundleClass::Deferred
            })
            .flat_map(|module| module.vendor_chunks.iter().cloned())
            .collect::<Vec<_>>();
        vendor_chunks.sort();
        vendor_chunks.dedup();

        routes.push(RoutePrefetchRoute {
            entry_component_id: entry_id,
            entry_module: entry_module.module_path.clone(),
            prefetch_modules,
            vendor_chunks,
        });
    }

    RoutePrefetchManifest {
        version: BUNDLE_ROUTE_PREFETCH_MANIFEST_VERSION.to_string(),
        plan_version: plan.version.clone(),
        routes,
    }
}

pub fn emit_bundle_artifacts_to_dir(
    plan: &BundlePlan,
    output_dir: impl AsRef<Path>,
) -> io::Result<BundleEmitReport> {
    emit_bundle_artifacts_to_dir_internal(plan, output_dir, None, None)
}

pub fn emit_bundle_artifacts_to_dir_with_sources(
    plan: &BundlePlan,
    manifest: &RenderManifestV2,
    module_sources: &HashMap<String, String>,
    output_dir: impl AsRef<Path>,
) -> io::Result<BundleEmitReport> {
    emit_bundle_artifacts_to_dir_internal(plan, output_dir, Some(manifest), Some(module_sources))
}

fn emit_bundle_artifacts_to_dir_internal(
    plan: &BundlePlan,
    output_dir: impl AsRef<Path>,
    manifest: Option<&RenderManifestV2>,
    module_sources: Option<&HashMap<String, String>>,
) -> io::Result<BundleEmitReport> {
    let output_dir = output_dir.as_ref();
    fs::create_dir_all(output_dir)?;

    let mut artifacts = Vec::new();

    let wt_bootstrap_source = emit_wt_bootstrap_source();
    let wt_bootstrap_bytes = wt_bootstrap_source.into_bytes();
    let wt_bootstrap_path = output_dir.join(relative_path_to_fs_path(BUNDLE_WT_BOOTSTRAP_FILENAME));
    write_artifact(&wt_bootstrap_path, &wt_bootstrap_bytes)?;
    artifacts.push(EmittedArtifact {
        relative_path: BUNDLE_WT_BOOTSTRAP_FILENAME.to_string(),
        bytes: wt_bootstrap_bytes.len(),
    });

    let plan_json = emit_bundle_plan_json(plan).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize bundle plan JSON: {err}"),
        )
    })?;
    let plan_bytes = plan_json.into_bytes();
    let plan_path = output_dir.join(BUNDLE_PLAN_FILENAME);
    write_artifact(&plan_path, &plan_bytes)?;
    artifacts.push(EmittedArtifact {
        relative_path: BUNDLE_PLAN_FILENAME.to_string(),
        bytes: plan_bytes.len(),
    });

    for (relative_wrapper_path, source) in emit_wrapper_modules(plan) {
        let normalized = normalize_relative_artifact_path(&relative_wrapper_path);
        let wrapper_path = output_dir.join(relative_path_to_fs_path(&normalized));
        let source_bytes = source.into_bytes();
        write_artifact(&wrapper_path, &source_bytes)?;
        artifacts.push(EmittedArtifact {
            relative_path: normalized,
            bytes: source_bytes.len(),
        });
    }

    for (relative_vendor_path, source) in emit_vendor_chunk_modules(plan) {
        let normalized = normalize_relative_artifact_path(&relative_vendor_path);
        let vendor_path = output_dir.join(relative_path_to_fs_path(&normalized));
        let source_bytes = source.into_bytes();
        write_artifact(&vendor_path, &source_bytes)?;
        artifacts.push(EmittedArtifact {
            relative_path: normalized,
            bytes: source_bytes.len(),
        });
    }

    let runtime_map_json = emit_bundle_runtime_map_json(plan).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize bundle runtime map JSON: {err}"),
        )
    })?;
    let runtime_map_bytes = runtime_map_json.into_bytes();
    let runtime_map_path = output_dir.join(BUNDLE_RUNTIME_MAP_FILENAME);
    write_artifact(&runtime_map_path, &runtime_map_bytes)?;
    artifacts.push(EmittedArtifact {
        relative_path: BUNDLE_RUNTIME_MAP_FILENAME.to_string(),
        bytes: runtime_map_bytes.len(),
    });

    let prefetch_manifest_json = emit_route_prefetch_manifest_json(plan).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize route prefetch manifest JSON: {err}"),
        )
    })?;
    let prefetch_manifest_bytes = prefetch_manifest_json.into_bytes();
    let prefetch_manifest_path = output_dir.join(BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME);
    write_artifact(&prefetch_manifest_path, &prefetch_manifest_bytes)?;
    artifacts.push(EmittedArtifact {
        relative_path: BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME.to_string(),
        bytes: prefetch_manifest_bytes.len(),
    });

    if let (Some(manifest), Some(module_sources)) = (manifest, module_sources) {
        let static_slices_json =
            emit_static_slices_json(manifest, module_sources).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to serialize static slices JSON: {err}"),
                )
            })?;
        let static_slices_bytes = static_slices_json.into_bytes();
        let static_slices_path = output_dir.join(BUNDLE_STATIC_SLICES_FILENAME);
        write_artifact(&static_slices_path, &static_slices_bytes)?;
        artifacts.push(EmittedArtifact {
            relative_path: BUNDLE_STATIC_SLICES_FILENAME.to_string(),
            bytes: static_slices_bytes.len(),
        });

        let precompiled_modules_json =
            emit_precompiled_runtime_modules_json(manifest, module_sources).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to serialize precompiled modules JSON: {err}"),
                )
            })?;
        let precompiled_modules_bytes = precompiled_modules_json.into_bytes();
        let precompiled_modules_path = output_dir.join(BUNDLE_PRECOMPILED_MODULES_FILENAME);
        write_artifact(&precompiled_modules_path, &precompiled_modules_bytes)?;
        artifacts.push(EmittedArtifact {
            relative_path: BUNDLE_PRECOMPILED_MODULES_FILENAME.to_string(),
            bytes: precompiled_modules_bytes.len(),
        });
    }

    artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    Ok(BundleEmitReport {
        output_dir: output_dir.to_path_buf(),
        artifacts,
    })
}

fn write_artifact(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)
}

fn normalize_relative_artifact_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches('/').to_string()
}

fn stable_vendor_chunk_module_path(chunk_name: &str) -> String {
    let normalized = normalize_relative_artifact_path(chunk_name);
    let hash = fnv1a_64_hex(normalized.as_bytes());
    let slug = normalized
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();

    format!("__albedo__/vendor/{hash}_{slug}.mjs")
}

fn build_vendor_chunk_module_source(packages: &[String]) -> String {
    let mut packages = packages
        .iter()
        .map(|pkg| pkg.trim())
        .filter(|pkg| !pkg.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    packages.sort();
    packages.dedup();

    if packages.is_empty() {
        return "const vendor = {};\nexport default vendor;\nexport { vendor };\n".to_string();
    }

    let mut source = String::new();
    for (index, package) in packages.iter().enumerate() {
        source.push_str(&format!(
            "import * as pkg_{index} from {};\n",
            js_string_literal(package)
        ));
    }

    source.push_str("const vendor = {\n");
    for (index, package) in packages.iter().enumerate() {
        source.push_str(&format!("  {}: pkg_{index},\n", js_string_literal(package)));
    }
    source.push_str("};\nexport default vendor;\nexport { vendor };\n");

    source
}

fn js_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("serializing JS string literal should not fail")
}

fn fnv1a_64_hex(input: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{hash:016x}")
}

fn relative_path_to_fs_path(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for segment in path.split('/') {
        if !segment.is_empty() {
            out.push(segment);
        }
    }
    out
}

fn emit_wt_bootstrap_source() -> String {
    include_str!("../../assets/albedo-wt-bootstrap.js").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundler::classify::BundleClass;
    use crate::bundler::plan::{BundleModulePlan, BUNDLE_PLAN_VERSION};
    use crate::bundler::rewrite::RewriteAction;
    use crate::bundler::vendor::VendorChunkPlan;
    use tempfile::tempdir;

    fn fixture_plan() -> BundlePlan {
        BundlePlan {
            version: BUNDLE_PLAN_VERSION.to_string(),
            manifest_schema_version: "2.0".to_string(),
            manifest_generated_at: "2026-02-18T00:00:00Z".to_string(),
            entry_component_id: Some(1),
            modules: vec![BundleModulePlan {
                component_id: 1,
                module_path: "src/routes/home.tsx".to_string(),
                class: BundleClass::Entry,
                dependency_ids: Vec::new(),
                wrapper_module_path: "__albedo__/wrappers/abc_src_routes_home_tsx.mjs".to_string(),
                dom_position: None,
            }],
            vendor_chunks: vec![VendorChunkPlan {
                chunk_name: "vendor.core".to_string(),
                packages: vec!["react".to_string(), "react-dom".to_string()],
                component_ids: vec![1],
            }],
            rewrite_actions: vec![
                RewriteAction::WrapModule {
                    component_id: 1,
                    source_module: "src/routes/home.tsx".to_string(),
                    wrapper_module: "__albedo__/wrappers/abc_src_routes_home_tsx.mjs".to_string(),
                },
                RewriteAction::LinkVendorChunk {
                    component_id: 1,
                    chunk_name: "vendor.core".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_emit_bundle_plan_json_contains_version() {
        let json = emit_bundle_plan_json(&fixture_plan()).unwrap();
        assert!(json.contains(BUNDLE_PLAN_VERSION));
        assert!(json.contains("rewrite_actions"));
    }

    #[test]
    fn test_emit_wrapper_modules_contains_wrapper_source() {
        let wrappers = emit_wrapper_modules(&fixture_plan());
        assert_eq!(wrappers.len(), 1);
        let source = wrappers
            .get("__albedo__/wrappers/abc_src_routes_home_tsx.mjs")
            .unwrap();
        assert!(source.contains("export default resolved;"));
    }

    #[test]
    fn test_emit_vendor_chunk_modules_contains_package_imports() {
        let chunks = emit_vendor_chunk_modules(&fixture_plan());
        assert_eq!(chunks.len(), 1);
        let (_, source) = chunks.iter().next().unwrap();
        assert!(source.contains(r#"import * as pkg_0 from "react";"#));
        assert!(source.contains(r#"import * as pkg_1 from "react-dom";"#));
        assert!(source.contains("export default vendor;"));
    }

    #[test]
    fn test_build_bundle_runtime_map_includes_vendor_chunk_paths() {
        let map = build_bundle_runtime_map(&fixture_plan());
        assert_eq!(map.version, BUNDLE_RUNTIME_MAP_VERSION);
        assert_eq!(map.modules.len(), 1);
        assert_eq!(
            map.modules[0].wrapper_module,
            "__albedo__/wrappers/abc_src_routes_home_tsx.mjs"
        );
        assert_eq!(map.modules[0].vendor_chunks.len(), 1);
        assert!(map.modules[0].vendor_chunks[0].starts_with("__albedo__/vendor/"));
        assert!(map.modules[0].vendor_chunks[0].ends_with("_vendor_core.mjs"));
    }

    #[test]
    fn test_build_route_prefetch_manifest_includes_entry_and_wrapper_modules() {
        let manifest = build_route_prefetch_manifest(&fixture_plan());
        assert_eq!(manifest.version, BUNDLE_ROUTE_PREFETCH_MANIFEST_VERSION);
        assert_eq!(manifest.plan_version, BUNDLE_PLAN_VERSION);
        assert_eq!(manifest.routes.len(), 1);
        assert_eq!(manifest.routes[0].entry_component_id, 1);
        assert_eq!(manifest.routes[0].entry_module, "src/routes/home.tsx");
        assert!(manifest.routes[0]
            .prefetch_modules
            .contains(&"__albedo__/wrappers/abc_src_routes_home_tsx.mjs".to_string()));
        assert_eq!(manifest.routes[0].vendor_chunks.len(), 1);
    }

    #[test]
    fn test_emit_bundle_artifacts_to_dir_writes_plan_runtime_map_wrapper_and_vendor() {
        let temp_dir = tempdir().unwrap();
        let report = emit_bundle_artifacts_to_dir(&fixture_plan(), temp_dir.path()).unwrap();

        assert_eq!(report.artifacts.len(), 6);

        let wt_bootstrap_path = temp_dir.path().join("_albedo").join("wt-bootstrap.js");
        let wt_bootstrap_source = std::fs::read_to_string(wt_bootstrap_path).unwrap();
        assert!(wt_bootstrap_source.contains("WebTransport"));

        let plan_path = temp_dir.path().join(BUNDLE_PLAN_FILENAME);
        let plan_json = std::fs::read_to_string(plan_path).unwrap();
        assert!(plan_json.contains("\"rewrite_actions\""));

        let runtime_map_path = temp_dir.path().join(BUNDLE_RUNTIME_MAP_FILENAME);
        let runtime_map_json = std::fs::read_to_string(runtime_map_path).unwrap();
        assert!(runtime_map_json.contains("\"version\": \"1.0\""));

        let prefetch_manifest_path = temp_dir
            .path()
            .join(BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME);
        let prefetch_manifest_json = std::fs::read_to_string(prefetch_manifest_path).unwrap();
        assert!(prefetch_manifest_json.contains("\"entry_module\": \"src/routes/home.tsx\""));

        let wrapper_path = temp_dir
            .path()
            .join("__albedo__")
            .join("wrappers")
            .join("abc_src_routes_home_tsx.mjs");
        let wrapper_source = std::fs::read_to_string(wrapper_path).unwrap();
        assert!(wrapper_source.contains("export default resolved;"));

        let vendor_path = report
            .artifacts
            .iter()
            .find(|artifact| artifact.relative_path.starts_with("__albedo__/vendor/"))
            .map(|artifact| {
                temp_dir
                    .path()
                    .join(relative_path_to_fs_path(&artifact.relative_path))
            })
            .expect("vendor module should be emitted");
        let vendor_source = std::fs::read_to_string(vendor_path).unwrap();
        assert!(vendor_source.contains(r#"import * as pkg_0 from "react";"#));
    }
}
