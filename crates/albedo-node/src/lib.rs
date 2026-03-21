use dom_render_compiler::bundler::BundlePlanOptions;
use dom_render_compiler::estimator::WeightEstimator;
use dom_render_compiler::incremental::CacheStats as CompilerCacheStats;
use dom_render_compiler::manifest::schema::{RenderManifestV2, VendorChunk};
use dom_render_compiler::parser::ParsedComponent;
use dom_render_compiler::scanner::ProjectScanner;
use dom_render_compiler::types::{Component, ComponentId};
use dom_render_compiler::RenderCompiler;
use napi::bindgen_prelude::{AsyncTask, Error, Result, Task};
use napi_derive::napi;
use once_cell::sync::Lazy;
use serde_json::Value;
use std::any::Any;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static LAST_CACHE_METRICS: Lazy<Mutex<CacheMetrics>> =
    Lazy::new(|| Mutex::new(CacheMetrics::default()));

#[napi(object)]
#[derive(Clone, Default)]
pub struct AnalyzeProjectOptions {
    pub cache_dir: Option<String>,
    pub persist_cache: Option<bool>,
}

#[napi(object)]
#[derive(Clone, Default)]
pub struct OptimizeManifestOptions {
    pub infer_shared_vendor_chunks: Option<bool>,
    pub shared_dependency_min_components: Option<u32>,
}

#[napi(object)]
#[derive(Clone, Default)]
pub struct CacheMetrics {
    pub cache_enabled: bool,
    pub cache_dir: Option<String>,
    pub total_cached: u32,
    pub invalidated: u32,
    pub files_tracked: u32,
    pub cache_hit_rate: f64,
    pub last_project_path: Option<String>,
    pub analyzed_components: u32,
}

pub struct AnalyzeProjectTask {
    project_path: String,
    options: AnalyzeProjectOptions,
}

impl Task for AnalyzeProjectTask {
    type Output = Value;
    type JsValue = napi::JsUnknown;

    fn compute(&mut self) -> Result<Self::Output> {
        let project_path = self.project_path.clone();
        let options = self.options.clone();
        panic_safe(move || analyze_project_impl(&project_path, &options))
    }

    fn resolve(&mut self, env: napi::Env, output: Self::Output) -> Result<Self::JsValue> {
        env.to_js_value(&output)
    }
}

pub struct OptimizeManifestTask {
    manifest: Value,
    options: OptimizeManifestOptions,
}

impl Task for OptimizeManifestTask {
    type Output = Value;
    type JsValue = napi::JsUnknown;

    fn compute(&mut self) -> Result<Self::Output> {
        let manifest = self.manifest.clone();
        let options = self.options.clone();
        panic_safe(move || optimize_manifest_impl(manifest, &options))
    }

    fn resolve(&mut self, env: napi::Env, output: Self::Output) -> Result<Self::JsValue> {
        env.to_js_value(&output)
    }
}

#[napi(js_name = "analyzeProject")]
pub fn analyze_project(
    project_path: String,
    options: Option<AnalyzeProjectOptions>,
) -> AsyncTask<AnalyzeProjectTask> {
    AsyncTask::new(AnalyzeProjectTask {
        project_path,
        options: options.unwrap_or_default(),
    })
}

#[napi(js_name = "optimizeManifest")]
pub fn optimize_manifest(
    manifest: Value,
    options: Option<OptimizeManifestOptions>,
) -> AsyncTask<OptimizeManifestTask> {
    AsyncTask::new(OptimizeManifestTask {
        manifest,
        options: options.unwrap_or_default(),
    })
}

#[napi(js_name = "getCacheStats")]
pub fn get_cache_stats() -> CacheMetrics {
    snapshot_cache_metrics()
}

fn analyze_project_impl(
    project_path: &str,
    options: &AnalyzeProjectOptions,
) -> BridgeResult<Value> {
    let project_dir = PathBuf::from(project_path);
    if !project_dir.exists() {
        return Err(format!(
            "project path does not exist: '{}'",
            project_dir.display()
        ));
    }
    if !project_dir.is_dir() {
        return Err(format!(
            "project path is not a directory: '{}'",
            project_dir.display()
        ));
    }

    let scanner = ProjectScanner::new();
    let components = scanner.scan_directory(&project_dir).map_err(|err| {
        format!(
            "failed to scan directory '{}': {err}",
            project_dir.display()
        )
    })?;

    if let Some(cache_dir) = options.cache_dir.as_ref() {
        if cache_dir.trim().is_empty() {
            return Err("cache_dir cannot be empty when provided".to_string());
        }

        let cache_dir = PathBuf::from(cache_dir);
        fs::create_dir_all(&cache_dir).map_err(|err| {
            format!(
                "failed to create cache directory '{}': {err}",
                cache_dir.display()
            )
        })?;

        let mut compiler = build_compiler_with_cache(components.clone(), &cache_dir);
        let file_paths = component_file_paths(&components);
        let optimization = compiler.optimize_incremental(&file_paths).map_err(|err| {
            format!(
                "failed to optimize project '{}' with cache '{}': {err}",
                project_dir.display(),
                cache_dir.display()
            )
        })?;

        if options.persist_cache.unwrap_or(true) {
            compiler.save_cache().map_err(|err| {
                format!(
                    "failed to persist cache at '{}': {err}",
                    cache_dir.display()
                )
            })?;
        }

        let manifest = compiler.manifest_v2_from_result(&optimization);
        update_cache_metrics(
            compiler.cache_stats(),
            Some(cache_dir.as_path()),
            Some(project_dir.as_path()),
            manifest.components.len(),
        );

        serde_json::to_value(manifest)
            .map_err(|err| format!("failed to serialize manifest output: {err}"))
    } else {
        let compiler = scanner.build_compiler(components);
        let manifest = compiler.optimize_manifest_v2().map_err(|err| {
            format!(
                "failed to optimize project '{}': {err}",
                project_dir.display()
            )
        })?;

        update_cache_metrics(
            None,
            None,
            Some(project_dir.as_path()),
            manifest.components.len(),
        );

        serde_json::to_value(manifest)
            .map_err(|err| format!("failed to serialize manifest output: {err}"))
    }
}

fn optimize_manifest_impl(
    manifest: Value,
    options: &OptimizeManifestOptions,
) -> BridgeResult<Value> {
    let mut manifest: RenderManifestV2 =
        serde_json::from_value(manifest).map_err(|err| format!("invalid manifest input: {err}"))?;

    if manifest.schema_version.trim().is_empty() {
        manifest.schema_version = RenderManifestV2::SCHEMA_VERSION.to_string();
    }

    let bundle_options = bundle_plan_options_from(options)?;
    let compiler = RenderCompiler::new();
    let plan = compiler.bundle_plan_from_manifest_v2(&manifest, &bundle_options);

    manifest.components.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.module_path.cmp(&right.module_path))
    });
    for component in &mut manifest.components {
        component.dependencies.sort_unstable();
        component.dependencies.dedup();
    }

    for batch in &mut manifest.parallel_batches {
        batch.sort_unstable();
        batch.dedup();
    }
    manifest.parallel_batches.sort();

    let mut vendor_chunks = plan
        .vendor_chunks
        .into_iter()
        .map(|chunk| VendorChunk {
            chunk_name: chunk.chunk_name,
            packages: chunk.packages,
        })
        .collect::<Vec<_>>();
    for chunk in &mut vendor_chunks {
        chunk.packages.sort();
        chunk.packages.dedup();
    }
    vendor_chunks.sort_by(|left, right| {
        left.chunk_name
            .cmp(&right.chunk_name)
            .then_with(|| left.packages.cmp(&right.packages))
    });
    manifest.vendor_chunks = vendor_chunks;

    serde_json::to_value(manifest).map_err(|err| format!("failed to serialize manifest: {err}"))
}

fn bundle_plan_options_from(options: &OptimizeManifestOptions) -> BridgeResult<BundlePlanOptions> {
    let mut bundle_options = BundlePlanOptions::default();

    if let Some(infer_shared_vendor_chunks) = options.infer_shared_vendor_chunks {
        bundle_options.vendor.infer_shared_vendor_chunks = infer_shared_vendor_chunks;
    }

    if let Some(shared_dependency_min_components) = options.shared_dependency_min_components {
        if shared_dependency_min_components == 0 {
            return Err(
                "shared_dependency_min_components must be at least 1 when provided".to_string(),
            );
        }
        bundle_options.vendor.shared_dependency_min_components =
            shared_dependency_min_components as usize;
    }

    Ok(bundle_options)
}

fn build_compiler_with_cache(components: Vec<ParsedComponent>, cache_dir: &Path) -> RenderCompiler {
    let estimator = WeightEstimator::new();
    let mut compiler = RenderCompiler::with_cache(cache_dir.to_path_buf());
    let mut component_map: HashMap<String, ComponentId> = HashMap::new();

    for parsed in &components {
        let mut component = Component::new(ComponentId::new(0), parsed.name.clone());

        component.weight = estimator.estimate(parsed);
        component.bitrate = estimator.estimate_bitrate(parsed);
        component.file_path = parsed.file_path.clone();
        component.line_number = parsed.line_number;

        let hints = estimator.estimate_priority_hints(parsed);
        component.is_above_fold = hints.is_above_fold;
        component.is_lcp_candidate = hints.is_lcp_candidate;
        component.is_interactive = hints.is_interactive;

        let id = compiler.add_component(component);
        component_map.insert(parsed.name.clone(), id);
    }

    for parsed in &components {
        if let Some(&from_id) = component_map.get(&parsed.name) {
            for import in &parsed.imports {
                if let Some(&to_id) = component_map.get(import) {
                    let _ = compiler.add_dependency(from_id, to_id);
                }
            }
        }
    }

    compiler
}

fn component_file_paths(components: &[ParsedComponent]) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for component in components {
        if !component.file_path.is_empty() {
            paths.insert(PathBuf::from(&component.file_path));
        }
    }
    paths.into_iter().collect()
}

fn update_cache_metrics(
    stats: Option<CompilerCacheStats>,
    cache_dir: Option<&Path>,
    project_path: Option<&Path>,
    analyzed_components: usize,
) {
    let mut metrics = CacheMetrics {
        cache_enabled: cache_dir.is_some(),
        cache_dir: cache_dir.map(path_to_string),
        total_cached: 0,
        invalidated: 0,
        files_tracked: 0,
        cache_hit_rate: 0.0,
        last_project_path: project_path.map(path_to_string),
        analyzed_components: saturating_u32(analyzed_components),
    };

    if let Some(stats) = stats {
        metrics.total_cached = saturating_u32(stats.total_cached);
        metrics.invalidated = saturating_u32(stats.invalidated);
        metrics.files_tracked = saturating_u32(stats.files_tracked);
        metrics.cache_hit_rate = stats.cache_hit_rate;
    }

    store_cache_metrics(metrics);
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn saturating_u32(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}

fn store_cache_metrics(metrics: CacheMetrics) {
    match LAST_CACHE_METRICS.lock() {
        Ok(mut guard) => {
            *guard = metrics;
        }
        Err(poisoned) => {
            *poisoned.into_inner() = metrics;
        }
    }
}

fn snapshot_cache_metrics() -> CacheMetrics {
    match LAST_CACHE_METRICS.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn panic_safe<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> BridgeResult<T>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) => Err(Error::from_reason(message)),
        Err(payload) => {
            let message = panic_payload_to_string(payload);
            Err(Error::from_reason(format!(
                "albedo-node bridge panicked: {message}"
            )))
        }
    }
}

fn panic_payload_to_string(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic payload".to_string()
}

type BridgeResult<T> = std::result::Result<T, String>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_bundle_plan_options_rejects_zero_threshold() {
        let err = bundle_plan_options_from(&OptimizeManifestOptions {
            infer_shared_vendor_chunks: None,
            shared_dependency_min_components: Some(0),
        })
        .unwrap_err();

        assert!(err.contains("at least 1"));
    }

    #[test]
    fn test_optimize_manifest_normalizes_ordering() {
        let manifest = json!({
            "schema_version": "2.0",
            "generated_at": "2026-02-20T00:00:00Z",
            "components": [
                {
                    "id": 2,
                    "name": "C2",
                    "module_path": "src/components/hero.tsx",
                    "tier": "C",
                    "weight_bytes": 2048,
                    "priority": 1.0,
                    "dependencies": [1],
                    "can_defer": true,
                    "hydration_mode": "OnVisible"
                },
                {
                    "id": 1,
                    "name": "C1",
                    "module_path": "src/components/header.tsx",
                    "tier": "C",
                    "weight_bytes": 2048,
                    "priority": 1.0,
                    "dependencies": [],
                    "can_defer": true,
                    "hydration_mode": "OnVisible"
                },
                {
                    "id": 3,
                    "name": "C3",
                    "module_path": "/repo/node_modules/react/index.js",
                    "tier": "C",
                    "weight_bytes": 2048,
                    "priority": 1.0,
                    "dependencies": [],
                    "can_defer": true,
                    "hydration_mode": "OnVisible"
                },
                {
                    "id": 4,
                    "name": "C4",
                    "module_path": "/repo/node_modules/react/index.js",
                    "tier": "C",
                    "weight_bytes": 2048,
                    "priority": 1.0,
                    "dependencies": [],
                    "can_defer": true,
                    "hydration_mode": "OnVisible"
                }
            ],
            "parallel_batches": [[2, 1]],
            "critical_path": [1, 2],
            "vendor_chunks": []
        });

        let optimized = optimize_manifest_impl(manifest, &OptimizeManifestOptions::default())
            .expect("manifest optimization should succeed");

        let component_ids = optimized["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(component_ids, vec![1, 2, 3, 4]);

        let vendor_chunks = optimized["vendor_chunks"].as_array().unwrap();
        assert_eq!(vendor_chunks.len(), 1);
        assert_eq!(vendor_chunks[0]["chunk_name"], "vendor.react");
    }
}
