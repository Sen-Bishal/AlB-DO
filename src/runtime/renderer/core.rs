use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use crate::manifest::schema::{PrecompiledRuntimeModulesArtifact, RenderManifestV2};
use crate::runtime::engine::{stable_source_hash, LoadErrorKind, RuntimeResult};

#[derive(Debug, Clone)]
pub struct RegisteredModule {
    pub code: String,
    pub source_hash: u64,
    pub precompiled_script: Option<String>,
    pub dependencies: Vec<String>,
    pub head_tags: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleRegistry {
    modules: BTreeMap<String, RegisteredModule>,
}

impl ModuleRegistry {
    pub fn register_module(&mut self, specifier: impl Into<String>, code: impl Into<String>) {
        self.register_module_with_metadata(specifier, code, Vec::new(), Vec::new());
    }

    pub fn register_module_with_dependencies(
        &mut self,
        specifier: impl Into<String>,
        code: impl Into<String>,
        dependencies: Vec<String>,
    ) {
        self.register_module_with_metadata(specifier, code, dependencies, Vec::new());
    }

    pub fn register_module_with_metadata(
        &mut self,
        specifier: impl Into<String>,
        code: impl Into<String>,
        dependencies: Vec<String>,
        head_tags: Vec<String>,
    ) {
        self.register_module_with_metadata_and_precompiled(
            specifier,
            code,
            dependencies,
            head_tags,
            None,
            None,
        );
    }

    pub fn register_module_with_metadata_and_precompiled(
        &mut self,
        specifier: impl Into<String>,
        code: impl Into<String>,
        dependencies: Vec<String>,
        head_tags: Vec<String>,
        source_hash_override: Option<u64>,
        precompiled_script: Option<String>,
    ) {
        let code = code.into();
        let normalized_dependencies = unique_stable(dependencies);
        let normalized_head_tags = unique_stable(head_tags);
        let source_hash = source_hash_override.unwrap_or_else(|| stable_source_hash(&code));

        self.modules.insert(
            specifier.into(),
            RegisteredModule {
                code,
                source_hash,
                precompiled_script,
                dependencies: normalized_dependencies,
                head_tags: normalized_head_tags,
            },
        );
    }

    pub fn register_from_manifest(
        &mut self,
        manifest: &RenderManifestV2,
        module_sources: &HashMap<String, String>,
    ) -> RuntimeResult<()> {
        self.register_from_manifest_with_precompiled(manifest, module_sources, None)
    }

    pub fn register_from_manifest_with_precompiled(
        &mut self,
        manifest: &RenderManifestV2,
        module_sources: &HashMap<String, String>,
        precompiled_modules: Option<&PrecompiledRuntimeModulesArtifact>,
    ) -> RuntimeResult<()> {
        let id_to_module_path: HashMap<u64, String> = manifest
            .components
            .iter()
            .map(|component| (component.id, component.module_path.clone()))
            .collect();
        let precompiled_index: HashMap<&str, _> = precompiled_modules
            .map(|artifact| {
                artifact
                    .modules
                    .iter()
                    .map(|module| (module.module_path.as_str(), module))
                    .collect()
            })
            .unwrap_or_default();

        let mut components = manifest.components.clone();
        components.sort_by_key(|component| component.id);

        for component in components {
            let code = module_sources.get(&component.module_path).ok_or_else(|| {
                crate::runtime::engine::RuntimeError::load(
                    LoadErrorKind::ModuleMissing,
                    format!(
                        "missing source for module '{}' from manifest component '{}'",
                        component.module_path, component.name
                    ),
                )
            })?;

            let mut dependencies = Vec::new();
            for dependency_id in &component.dependencies {
                let dependency_specifier =
                    id_to_module_path.get(dependency_id).ok_or_else(|| {
                        crate::runtime::engine::RuntimeError::load(
                            LoadErrorKind::ModuleMissing,
                            format!(
                                "manifest dependency id '{}' not found for component '{}'",
                                dependency_id, component.name
                            ),
                        )
                    })?;
                dependencies.push(dependency_specifier.clone());
            }

            let source_hash = stable_source_hash(code);
            let precompiled_script = precompiled_index
                .get(component.module_path.as_str())
                .filter(|entry| entry.source_hash == source_hash)
                .map(|entry| entry.compiled_script.clone());

            self.register_module_with_metadata_and_precompiled(
                component.module_path.clone(),
                code.clone(),
                dependencies,
                Vec::new(),
                Some(source_hash),
                precompiled_script,
            );
        }

        Ok(())
    }

    pub fn resolve_module_order(
        &self,
        entry: &str,
        requested: &[String],
    ) -> RuntimeResult<Vec<String>> {
        if !requested.is_empty() {
            for specifier in requested {
                if !self.modules.contains_key(specifier) {
                    return Err(crate::runtime::engine::RuntimeError::load(
                        LoadErrorKind::ModuleMissing,
                        format!("module missing: '{}'", specifier),
                    ));
                }
            }
            return Ok(unique_stable(requested.to_vec()));
        }

        self.resolve_topological_order(entry)
    }

    pub fn module(&self, specifier: &str) -> Option<&RegisteredModule> {
        self.modules.get(specifier)
    }

    fn resolve_topological_order(&self, entry: &str) -> RuntimeResult<Vec<String>> {
        if !self.modules.contains_key(entry) {
            return Err(crate::runtime::engine::RuntimeError::load(
                LoadErrorKind::ModuleMissing,
                format!("entry module missing: '{}'", entry),
            ));
        }

        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        let mut ordered = Vec::new();

        self.visit_module(entry, &mut visiting, &mut visited, &mut ordered)?;
        Ok(ordered)
    }

    fn visit_module(
        &self,
        current: &str,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        ordered: &mut Vec<String>,
    ) -> RuntimeResult<()> {
        if visited.contains(current) {
            return Ok(());
        }
        if visiting.contains(current) {
            return Err(crate::runtime::engine::RuntimeError::load(
                LoadErrorKind::DependencyCycle,
                format!(
                    "dependency cycle detected while visiting module '{}'",
                    current
                ),
            ));
        }

        let module = self.modules.get(current).ok_or_else(|| {
            crate::runtime::engine::RuntimeError::load(
                LoadErrorKind::ModuleMissing,
                format!("module missing: '{}'", current),
            )
        })?;
        visiting.insert(current.to_string());

        for dependency in &module.dependencies {
            self.visit_module(dependency, visiting, visited, ordered)?;
        }

        visiting.remove(current);
        visited.insert(current.to_string());
        ordered.push(current.to_string());
        Ok(())
    }
}

pub fn unique_stable(input: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(input.len());

    for item in input {
        if seen.insert(item.clone()) {
            output.push(item);
        }
    }

    output
}

pub fn normalize_invalidation_token(value: &str) -> String {
    value.trim().replace('\\', "/")
}

pub fn entry_matches_path(entry: &str, path: &str) -> bool {
    let normalized_path = normalize_invalidation_token(path);
    if normalized_path.is_empty() {
        return false;
    }
    if entry == normalized_path {
        return true;
    }

    let stripped = normalized_path.trim_start_matches('/');
    if stripped.is_empty() {
        return false;
    }

    entry.contains(stripped)
}

#[derive(Debug, Clone)]
pub struct RouteRenderRequest {
    pub entry: String,
    pub props_json: String,
    pub module_order: Vec<String>,
    pub hydration_payload: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FsRouteRenderRequest {
    pub components_root: PathBuf,
    pub entry_module: String,
    pub props_json: String,
    pub hydration_payload: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RenderTimings {
    pub module_load_ms: u128,
    pub modules_loaded_this_render: u32,
    pub module_cache_hits: u32,
    pub module_cache_misses: u32,
    pub render_ms: u128,
    pub render_eval_ms: u128,
    pub total_ms: u128,
}

#[derive(Debug, Clone)]
pub struct RouteRenderResult {
    pub html: String,
    pub shell_html: String,
    pub deferred_chunks: Vec<String>,
    pub head_tags: Vec<String>,
    pub hydration_payload: String,
    pub timings: RenderTimings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteStreamChunkKind {
    ShellHtml,
    DeferredHtml,
    HeadTag,
    HydrationPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStreamChunk {
    pub kind: RouteStreamChunkKind,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct RouteRenderStreamResult {
    pub chunks: Vec<RouteStreamChunk>,
    pub head_tags: Vec<String>,
    pub hydration_payload: String,
    pub timings: RenderTimings,
}

pub fn route_render_result_to_stream(result: RouteRenderResult) -> RouteRenderStreamResult {
    let mut chunks = Vec::new();
    chunks.push(RouteStreamChunk {
        kind: RouteStreamChunkKind::ShellHtml,
        content: result.shell_html.clone(),
    });

    for deferred in &result.deferred_chunks {
        chunks.push(RouteStreamChunk {
            kind: RouteStreamChunkKind::DeferredHtml,
            content: deferred.clone(),
        });
    }

    for head_tag in &result.head_tags {
        chunks.push(RouteStreamChunk {
            kind: RouteStreamChunkKind::HeadTag,
            content: head_tag.clone(),
        });
    }

    if result.hydration_payload != "{}" {
        chunks.push(RouteStreamChunk {
            kind: RouteStreamChunkKind::HydrationPayload,
            content: result.hydration_payload.clone(),
        });
    }

    RouteRenderStreamResult {
        chunks,
        head_tags: result.head_tags,
        hydration_payload: result.hydration_payload,
        timings: result.timings,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StaticSliceCacheKey {
    pub entry: String,
    pub props_hash: u64,
    pub source_fingerprint: u64,
    pub invalidation_version: u64,
}
