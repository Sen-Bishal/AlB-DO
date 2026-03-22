use super::ast_eval::ComponentProject;
use super::engine::{
    stable_source_hash, BootstrapPayload, LoadErrorKind, RuntimeEngine, RuntimeError, RuntimeResult,
};
use super::static_slice::build_static_slice_manifest;
use crate::hydration;
use crate::manifest::schema::{PrecompiledRuntimeModulesArtifact, RenderManifestV2};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

const PROPS_CACHE_MAX_ENTRIES: usize = 256;

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
                RuntimeError::load(
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
                        RuntimeError::load(
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
                    return Err(RuntimeError::load(
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
            return Err(RuntimeError::load(
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
            return Err(RuntimeError::load(
                LoadErrorKind::DependencyCycle,
                format!(
                    "dependency cycle detected while visiting module '{}'",
                    current
                ),
            ));
        }

        let module = self.modules.get(current).ok_or_else(|| {
            RuntimeError::load(
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

fn unique_stable(input: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(input.len());

    for item in input {
        if seen.insert(item.clone()) {
            output.push(item);
        }
    }

    output
}

fn normalize_invalidation_token(value: &str) -> String {
    value.trim().replace('\\', "/")
}

fn entry_matches_path(entry: &str, path: &str) -> bool {
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

fn route_render_result_to_stream(result: RouteRenderResult) -> RouteRenderStreamResult {
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
struct StaticSliceCacheKey {
    entry: String,
    props_hash: u64,
    source_fingerprint: u64,
    invalidation_version: u64,
}

pub struct ServerRenderer<E: RuntimeEngine> {
    engine: E,
    module_registry: ModuleRegistry,
    loaded_module_hashes: HashMap<String, u64>,
    normalized_props_cache: HashMap<String, String>,
    static_slice_modules: HashMap<String, u64>,
    static_slice_html_cache: HashMap<StaticSliceCacheKey, String>,
    route_invalidation_versions: HashMap<String, u64>,
    tag_invalidation_versions: HashMap<String, u64>,
    route_tags: HashMap<String, Vec<String>>,
    invalidation_version_clock: u64,
}

impl<E: RuntimeEngine> ServerRenderer<E> {
    pub fn new(mut engine: E, bootstrap: &BootstrapPayload) -> RuntimeResult<Self> {
        engine.init(bootstrap)?;
        Ok(Self {
            engine,
            module_registry: ModuleRegistry::default(),
            loaded_module_hashes: HashMap::new(),
            normalized_props_cache: HashMap::new(),
            static_slice_modules: HashMap::new(),
            static_slice_html_cache: HashMap::new(),
            route_invalidation_versions: HashMap::new(),
            tag_invalidation_versions: HashMap::new(),
            route_tags: HashMap::new(),
            invalidation_version_clock: 0,
        })
    }

    pub fn warm_runtime(&mut self) -> RuntimeResult<()> {
        self.engine.warm()
    }

    pub fn register_module(&mut self, specifier: impl Into<String>, code: impl Into<String>) {
        let specifier = specifier.into();
        self.module_registry
            .register_module(specifier.clone(), code);
        self.invalidate_static_slice_for_module(specifier.as_str());
    }

    pub fn register_module_with_dependencies(
        &mut self,
        specifier: impl Into<String>,
        code: impl Into<String>,
        dependencies: Vec<String>,
    ) {
        let specifier = specifier.into();
        self.module_registry.register_module_with_dependencies(
            specifier.clone(),
            code,
            dependencies,
        );
        self.invalidate_static_slice_for_module(specifier.as_str());
    }

    pub fn register_module_with_metadata(
        &mut self,
        specifier: impl Into<String>,
        code: impl Into<String>,
        dependencies: Vec<String>,
        head_tags: Vec<String>,
    ) {
        let specifier = specifier.into();
        self.module_registry.register_module_with_metadata(
            specifier.clone(),
            code,
            dependencies,
            head_tags,
        );
        self.invalidate_static_slice_for_module(specifier.as_str());
    }

    pub fn register_manifest_modules(
        &mut self,
        manifest: &RenderManifestV2,
        module_sources: &HashMap<String, String>,
    ) -> RuntimeResult<()> {
        self.register_manifest_modules_with_precompiled(manifest, module_sources, None)
    }

    pub fn register_manifest_modules_with_precompiled(
        &mut self,
        manifest: &RenderManifestV2,
        module_sources: &HashMap<String, String>,
        precompiled_modules: Option<&PrecompiledRuntimeModulesArtifact>,
    ) -> RuntimeResult<()> {
        self.module_registry
            .register_from_manifest_with_precompiled(
                manifest,
                module_sources,
                precompiled_modules,
            )?;
        for component in &manifest.components {
            self.register_route_tags(
                component.module_path.clone(),
                vec![
                    format!("component:{}", component.id),
                    format!("tier:{:?}", component.tier),
                    format!("hydration:{:?}", component.hydration_mode),
                ],
            );
        }
        let static_slices = build_static_slice_manifest(manifest, module_sources);
        self.load_static_slice_manifest(&static_slices);
        Ok(())
    }

    pub fn prime_runtime_cache(&mut self, requests: &[RouteRenderRequest]) -> RuntimeResult<()> {
        self.warm_runtime()?;
        for request in requests {
            self.render_route(request)?;
        }
        Ok(())
    }

    pub fn module_registry(&self) -> &ModuleRegistry {
        &self.module_registry
    }

    pub fn register_route_tags(&mut self, route_entry: impl Into<String>, tags: Vec<String>) {
        let route_entry = route_entry.into();
        let normalized_tags = tags
            .into_iter()
            .map(|tag| normalize_invalidation_token(tag.as_str()))
            .filter(|tag| !tag.is_empty())
            .collect::<Vec<_>>();
        self.route_tags
            .insert(route_entry, unique_stable(normalized_tags));
    }

    pub fn revalidate_path(&mut self, path: &str) {
        let normalized = normalize_invalidation_token(path);
        if normalized.is_empty() {
            return;
        }
        let version = self.next_invalidation_version();
        self.route_invalidation_versions
            .insert(normalized.clone(), version);
        self.static_slice_html_cache
            .retain(|key, _| !entry_matches_path(key.entry.as_str(), normalized.as_str()));
    }

    pub fn revalidate_tag(&mut self, tag: &str) {
        let normalized = normalize_invalidation_token(tag);
        if normalized.is_empty() {
            return;
        }
        let version = self.next_invalidation_version();
        self.tag_invalidation_versions
            .insert(normalized.clone(), version);

        let mut impacted_entries = HashSet::new();
        for (entry, tags) in &self.route_tags {
            if tags.iter().any(|candidate| candidate == &normalized) {
                impacted_entries.insert(entry.clone());
            }
        }

        self.static_slice_html_cache
            .retain(|key, _| !impacted_entries.contains(&key.entry));
    }

    pub fn render_route(&mut self, route: &RouteRenderRequest) -> RuntimeResult<RouteRenderResult> {
        self.render_route_with_overrides(route, None, Vec::new())
    }

    pub fn render_route_stream(
        &mut self,
        route: &RouteRenderRequest,
    ) -> RuntimeResult<RouteRenderStreamResult> {
        self.render_route_stream_with_overrides(route, None, Vec::new())
    }

    pub fn render_route_with_manifest_hydration(
        &mut self,
        route: &RouteRenderRequest,
        manifest: &RenderManifestV2,
    ) -> RuntimeResult<RouteRenderResult> {
        let artifacts = hydration::build_hydration_artifacts(manifest, route.entry.as_str())
            .map_err(|err| {
                RuntimeError::render(format!(
                    "failed to build hydration artifacts for route '{}': {err}",
                    route.entry
                ))
            })?;

        if let Some(artifacts) = artifacts {
            self.render_route_with_overrides(
                route,
                Some(artifacts.payload_json),
                vec![artifacts.payload_script_tag, artifacts.bootstrap_script_tag],
            )
        } else {
            self.render_route_with_overrides(route, Some("{}".to_string()), Vec::new())
        }
    }

    pub fn render_route_stream_with_manifest_hydration(
        &mut self,
        route: &RouteRenderRequest,
        manifest: &RenderManifestV2,
    ) -> RuntimeResult<RouteRenderStreamResult> {
        let artifacts = hydration::build_hydration_artifacts(manifest, route.entry.as_str())
            .map_err(|err| {
                RuntimeError::render(format!(
                    "failed to build hydration artifacts for route '{}': {err}",
                    route.entry
                ))
            })?;

        if let Some(artifacts) = artifacts {
            self.render_route_stream_with_overrides(
                route,
                Some(artifacts.payload_json),
                vec![artifacts.payload_script_tag, artifacts.bootstrap_script_tag],
            )
        } else {
            self.render_route_stream_with_overrides(route, Some("{}".to_string()), Vec::new())
        }
    }

    fn render_route_stream_with_overrides(
        &mut self,
        route: &RouteRenderRequest,
        hydration_payload_override: Option<String>,
        extra_head_tags: Vec<String>,
    ) -> RuntimeResult<RouteRenderStreamResult> {
        let result =
            self.render_route_with_overrides(route, hydration_payload_override, extra_head_tags)?;
        Ok(route_render_result_to_stream(result))
    }

    fn render_route_with_overrides(
        &mut self,
        route: &RouteRenderRequest,
        hydration_payload_override: Option<String>,
        extra_head_tags: Vec<String>,
    ) -> RuntimeResult<RouteRenderResult> {
        let total_start = Instant::now();
        let normalized_props = self.normalize_props_json(&route.entry, &route.props_json)?;

        let load_start = Instant::now();
        let module_order = self
            .module_registry
            .resolve_module_order(&route.entry, &route.module_order)?;
        let mut head_tags = Vec::new();
        let mut seen_head_tags = HashSet::new();

        for specifier in &module_order {
            let module = self.module_registry.module(specifier).ok_or_else(|| {
                RuntimeError::load(
                    LoadErrorKind::ModuleMissing,
                    format!("module missing during load: '{}'", specifier),
                )
            })?;

            for tag in &module.head_tags {
                if seen_head_tags.insert(tag.clone()) {
                    head_tags.push(tag.clone());
                }
            }
        }
        for tag in extra_head_tags {
            if seen_head_tags.insert(tag.clone()) {
                head_tags.push(tag);
            }
        }

        let static_slice_key =
            self.build_static_slice_cache_key(&route.entry, &normalized_props, &module_order);
        if let Some(cache_key) = static_slice_key.as_ref() {
            if let Some(static_html) = self.static_slice_html_cache.get(cache_key) {
                return Ok(RouteRenderResult {
                    html: static_html.clone(),
                    shell_html: static_html.clone(),
                    deferred_chunks: Vec::new(),
                    head_tags,
                    hydration_payload: hydration_payload_override
                        .or_else(|| route.hydration_payload.clone())
                        .unwrap_or_else(|| "{}".to_string()),
                    timings: RenderTimings {
                        module_load_ms: load_start.elapsed().as_millis(),
                        modules_loaded_this_render: 0,
                        module_cache_hits: module_order.len() as u32,
                        module_cache_misses: 0,
                        render_ms: 0,
                        render_eval_ms: 0,
                        total_ms: total_start.elapsed().as_millis(),
                    },
                });
            }
        }

        let mut modules_loaded_this_render = 0_u32;
        let mut module_cache_hits = 0_u32;
        let mut module_cache_misses = 0_u32;

        for specifier in &module_order {
            let module = self.module_registry.module(specifier).ok_or_else(|| {
                RuntimeError::load(
                    LoadErrorKind::ModuleMissing,
                    format!("module missing during load: '{}'", specifier),
                )
            })?;

            if self.loaded_module_hashes.get(specifier).copied() == Some(module.source_hash) {
                module_cache_hits += 1;
            } else {
                if let Some(precompiled_script) = module.precompiled_script.as_ref() {
                    self.engine.load_precompiled_module(
                        specifier,
                        precompiled_script,
                        module.source_hash,
                    )?;
                } else {
                    self.engine.load_module(specifier, &module.code)?;
                }
                self.loaded_module_hashes
                    .insert(specifier.clone(), module.source_hash);
                modules_loaded_this_render += 1;
                module_cache_misses += 1;
            }
        }
        let module_load_ms = load_start.elapsed().as_millis();

        let render_start = Instant::now();
        let render_output = self
            .engine
            .render_component_stream(&route.entry, &normalized_props)?;
        let render_ms = render_start.elapsed().as_millis();
        let render_eval_ms = render_output.eval_ms;
        let shell_html = render_output.shell_html;
        let deferred_chunks = render_output.deferred_chunks;
        let mut html = shell_html.clone();
        for chunk in &deferred_chunks {
            html.push_str(chunk);
        }
        if let Some(cache_key) = static_slice_key {
            self.static_slice_html_cache.insert(cache_key, html.clone());
        }

        Ok(RouteRenderResult {
            html,
            shell_html,
            deferred_chunks,
            head_tags,
            hydration_payload: hydration_payload_override
                .or_else(|| route.hydration_payload.clone())
                .unwrap_or_else(|| "{}".to_string()),
            timings: RenderTimings {
                module_load_ms,
                modules_loaded_this_render,
                module_cache_hits,
                module_cache_misses,
                render_ms,
                render_eval_ms,
                total_ms: total_start.elapsed().as_millis(),
            },
        })
    }

    pub fn render_route_from_component_dir(
        &mut self,
        route: &FsRouteRenderRequest,
    ) -> RuntimeResult<RouteRenderResult> {
        self.render_route_from_component_dir_with_overrides(route, None, Vec::new())
    }

    pub fn render_route_from_component_dir_with_manifest_hydration(
        &mut self,
        route: &FsRouteRenderRequest,
        manifest: &RenderManifestV2,
    ) -> RuntimeResult<RouteRenderResult> {
        let artifacts = hydration::build_hydration_artifacts(manifest, route.entry_module.as_str())
            .map_err(|err| {
                RuntimeError::render(format!(
                    "failed to build hydration artifacts for filesystem route '{}': {err}",
                    route.entry_module
                ))
            })?;

        if let Some(artifacts) = artifacts {
            self.render_route_from_component_dir_with_overrides(
                route,
                Some(artifacts.payload_json),
                vec![artifacts.payload_script_tag, artifacts.bootstrap_script_tag],
            )
        } else {
            self.render_route_from_component_dir_with_overrides(
                route,
                Some("{}".to_string()),
                Vec::new(),
            )
        }
    }

    fn render_route_from_component_dir_with_overrides(
        &mut self,
        route: &FsRouteRenderRequest,
        hydration_payload_override: Option<String>,
        extra_head_tags: Vec<String>,
    ) -> RuntimeResult<RouteRenderResult> {
        let total_start = Instant::now();

        let load_start = Instant::now();
        let project = ComponentProject::load_from_dir(&route.components_root).map_err(|err| {
            RuntimeError::render(format!(
                "failed to load component project from '{}': {err}",
                route.components_root.display()
            ))
        })?;
        let module_load_ms = load_start.elapsed().as_millis();

        let render_start = Instant::now();
        let props_value: serde_json::Value =
            serde_json::from_str(&route.props_json).map_err(|err| {
                RuntimeError::props(format!(
                    "invalid props JSON for filesystem route render '{}': {err}",
                    route.entry_module
                ))
            })?;
        let html = project
            .render_entry(&route.entry_module, &props_value)
            .map_err(|err| {
                RuntimeError::render(format!(
                    "filesystem fallback render failed for entry '{}': {err}",
                    route.entry_module
                ))
            })?;
        let render_ms = render_start.elapsed().as_millis();

        Ok(RouteRenderResult {
            shell_html: html.clone(),
            deferred_chunks: Vec::new(),
            html,
            head_tags: unique_stable(extra_head_tags),
            hydration_payload: hydration_payload_override
                .or_else(|| route.hydration_payload.clone())
                .unwrap_or_else(|| "{}".to_string()),
            timings: RenderTimings {
                module_load_ms,
                modules_loaded_this_render: 0,
                module_cache_hits: 0,
                module_cache_misses: 0,
                render_ms,
                render_eval_ms: render_ms,
                total_ms: total_start.elapsed().as_millis(),
            },
        })
    }

    fn normalize_props_json(&mut self, entry: &str, props_json: &str) -> RuntimeResult<String> {
        if let Some(normalized) = self.normalized_props_cache.get(props_json) {
            return Ok(normalized.clone());
        }

        let props_value: serde_json::Value = serde_json::from_str(props_json).map_err(|err| {
            RuntimeError::props(format!("invalid props JSON for route '{entry}': {err}"))
        })?;
        let normalized_props = serde_json::to_string(&props_value).map_err(|err| {
            RuntimeError::props(format!(
                "failed to normalize props JSON for route '{entry}': {err}"
            ))
        })?;

        if self.normalized_props_cache.len() >= PROPS_CACHE_MAX_ENTRIES {
            if let Some(evicted_key) = self.normalized_props_cache.keys().next().cloned() {
                self.normalized_props_cache.remove(&evicted_key);
            }
        }

        self.normalized_props_cache
            .insert(props_json.to_string(), normalized_props.clone());
        Ok(normalized_props)
    }

    fn next_invalidation_version(&mut self) -> u64 {
        self.invalidation_version_clock = self.invalidation_version_clock.wrapping_add(1);
        self.invalidation_version_clock
    }

    fn route_cache_invalidation_version(&self, entry: &str) -> u64 {
        let mut version = 0_u64;

        for (path, path_version) in &self.route_invalidation_versions {
            if entry_matches_path(entry, path) {
                version = version.max(*path_version);
            }
        }

        if let Some(tags) = self.route_tags.get(entry) {
            for tag in tags {
                if let Some(tag_version) = self.tag_invalidation_versions.get(tag) {
                    version = version.max(*tag_version);
                }
            }
        }

        version
    }

    fn load_static_slice_manifest(
        &mut self,
        manifest: &crate::manifest::schema::StaticSliceArtifactManifest,
    ) {
        let mut next = HashMap::new();
        for slice in &manifest.slices {
            if slice.eligible {
                next.insert(slice.module_path.clone(), slice.source_hash);
            }
        }

        if self.static_slice_modules != next {
            self.static_slice_html_cache.clear();
        }
        self.static_slice_modules = next;
    }

    fn invalidate_static_slice_for_module(&mut self, specifier: &str) {
        self.static_slice_modules.remove(specifier);
        self.static_slice_html_cache.clear();
    }

    fn build_static_slice_cache_key(
        &self,
        entry: &str,
        normalized_props: &str,
        module_order: &[String],
    ) -> Option<StaticSliceCacheKey> {
        if module_order.is_empty() {
            return None;
        }

        let mut fingerprint_basis = String::new();
        for specifier in module_order {
            let source_hash = self.static_slice_modules.get(specifier)?;
            fingerprint_basis.push_str(specifier);
            fingerprint_basis.push(':');
            fingerprint_basis.push_str(&source_hash.to_string());
            fingerprint_basis.push(';');
        }

        Some(StaticSliceCacheKey {
            entry: entry.to_string(),
            props_hash: stable_source_hash(normalized_props),
            source_fingerprint: stable_source_hash(fingerprint_basis.as_str()),
            invalidation_version: self.route_cache_invalidation_version(entry),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::engine::{
        BootstrapPayload, LoadErrorKind, RenderOutput, RenderStreamOutput, RuntimeError,
    };
    use crate::runtime::quickjs_engine::QuickJsEngine;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::Instant;

    #[derive(Default)]
    struct DeferredChunkEngine;

    impl RuntimeEngine for DeferredChunkEngine {
        fn init(&mut self, _bootstrap: &BootstrapPayload) -> RuntimeResult<()> {
            Ok(())
        }

        fn load_module(&mut self, _specifier: &str, _code: &str) -> RuntimeResult<()> {
            Ok(())
        }

        fn load_precompiled_module(
            &mut self,
            _specifier: &str,
            _compiled_script: &str,
            _source_hash: u64,
        ) -> RuntimeResult<()> {
            Ok(())
        }

        fn render_component(
            &mut self,
            _entry: &str,
            _props_json: &str,
        ) -> RuntimeResult<RenderOutput> {
            Ok(RenderOutput {
                html: "<main>fallback</main>".to_string(),
                eval_ms: 0,
            })
        }

        fn render_component_stream(
            &mut self,
            _entry: &str,
            _props_json: &str,
        ) -> RuntimeResult<RenderStreamOutput> {
            Ok(RenderStreamOutput {
                shell_html: "<main>".to_string(),
                deferred_chunks: vec!["ALBEDO".to_string(), "</main>".to_string()],
                eval_ms: 0,
            })
        }

        fn warm(&mut self) -> RuntimeResult<()> {
            Ok(())
        }
    }

    #[test]
    fn test_quickjs_server_renderer_fixture_component() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module("app", "(props) => '<main>Hello ' + props.name + '</main>'");

        let request = RouteRenderRequest {
            entry: "app".to_string(),
            props_json: r#"{"name":"ALBEDO"}"#.to_string(),
            module_order: vec!["app".to_string()],
            hydration_payload: None,
        };

        let result = renderer.render_route(&request).unwrap();
        assert_eq!(result.html, "<main>Hello ALBEDO</main>");
        assert_eq!(result.hydration_payload, "{}");
        assert_eq!(result.timings.modules_loaded_this_render, 1);
        assert_eq!(result.timings.module_cache_hits, 0);
        assert_eq!(result.timings.module_cache_misses, 1);
        assert!(result.timings.render_ms >= result.timings.render_eval_ms);
    }

    #[test]
    fn test_render_route_module_hash_cache_hits_and_hot_reload_misses() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let request = RouteRenderRequest {
            entry: "routes/home".to_string(),
            props_json: r#"{"name":"ALBEDO"}"#.to_string(),
            module_order: vec!["routes/home".to_string()],
            hydration_payload: None,
        };

        renderer.register_module(
            "routes/home",
            "(props) => '<main>v1 ' + props.name + '</main>'",
        );
        let first = renderer.render_route(&request).unwrap();
        assert_eq!(first.html, "<main>v1 ALBEDO</main>");
        assert_eq!(first.timings.modules_loaded_this_render, 1);
        assert_eq!(first.timings.module_cache_hits, 0);
        assert_eq!(first.timings.module_cache_misses, 1);

        let second = renderer.render_route(&request).unwrap();
        assert_eq!(second.html, "<main>v1 ALBEDO</main>");
        assert_eq!(second.timings.modules_loaded_this_render, 0);
        assert_eq!(second.timings.module_cache_hits, 1);
        assert_eq!(second.timings.module_cache_misses, 0);

        renderer.register_module(
            "routes/home",
            "(props) => '<main>v2 ' + props.name + '</main>'",
        );
        let third = renderer.render_route(&request).unwrap();
        assert_eq!(third.html, "<main>v2 ALBEDO</main>");
        assert_eq!(third.timings.modules_loaded_this_render, 1);
        assert_eq!(third.timings.module_cache_hits, 0);
        assert_eq!(third.timings.module_cache_misses, 1);
    }

    #[test]
    fn test_prime_runtime_cache_warms_modules_before_first_live_request() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module("app", "(props) => '<main>' + props.name + '</main>'");

        let request = RouteRenderRequest {
            entry: "app".to_string(),
            props_json: r#"{"name":"ALBEDO"}"#.to_string(),
            module_order: vec!["app".to_string()],
            hydration_payload: None,
        };

        renderer
            .prime_runtime_cache(std::slice::from_ref(&request))
            .unwrap();

        let live = renderer.render_route(&request).unwrap();
        assert_eq!(live.html, "<main>ALBEDO</main>");
        assert_eq!(live.timings.modules_loaded_this_render, 0);
        assert_eq!(live.timings.module_cache_hits, 1);
        assert_eq!(live.timings.module_cache_misses, 0);
    }

    #[test]
    fn test_tier_a_static_slice_cache_hit_bypasses_engine_render() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-22T00:00:00Z".to_string(),
            components: vec![
                crate::manifest::schema::ComponentManifestEntry {
                    id: 1,
                    name: "Leaf".to_string(),
                    module_path: "components/leaf".to_string(),
                    tier: crate::manifest::schema::Tier::A,
                    weight_bytes: 1024,
                    priority: 1.0,
                    dependencies: vec![],
                    can_defer: false,
                    hydration_mode: crate::manifest::schema::HydrationMode::None,
                },
                crate::manifest::schema::ComponentManifestEntry {
                    id: 2,
                    name: "Entry".to_string(),
                    module_path: "routes/home".to_string(),
                    tier: crate::manifest::schema::Tier::A,
                    weight_bytes: 2048,
                    priority: 2.0,
                    dependencies: vec![1],
                    can_defer: false,
                    hydration_mode: crate::manifest::schema::HydrationMode::None,
                },
            ],
            parallel_batches: vec![vec![1], vec![2]],
            critical_path: vec![1, 2],
            vendor_chunks: Vec::new(),
        };

        let mut sources = HashMap::new();
        sources.insert(
            "components/leaf".to_string(),
            "(props) => '<p>' + props.label + '</p>'".to_string(),
        );
        sources.insert(
            "routes/home".to_string(),
            "(props, require) => '<main>' + require('components/leaf')({label: props.label}) + '</main>'".to_string(),
        );

        renderer
            .register_manifest_modules(&manifest, &sources)
            .unwrap();

        let request = RouteRenderRequest {
            entry: "routes/home".to_string(),
            props_json: r#"{"label":"Static Tier A"}"#.to_string(),
            module_order: Vec::new(),
            hydration_payload: None,
        };

        let cold = renderer.render_route(&request).unwrap();
        assert_eq!(cold.html, "<main><p>Static Tier A</p></main>");

        let warm = renderer.render_route(&request).unwrap();
        assert_eq!(warm.html, cold.html);
        assert_eq!(warm.timings.modules_loaded_this_render, 0);
        assert_eq!(warm.timings.module_cache_misses, 0);
        assert_eq!(warm.timings.render_ms, 0);
        assert_eq!(warm.timings.render_eval_ms, 0);
    }

    #[test]
    fn test_static_slice_cache_invalidates_on_manifest_source_change() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-22T00:00:00Z".to_string(),
            components: vec![crate::manifest::schema::ComponentManifestEntry {
                id: 10,
                name: "Entry".to_string(),
                module_path: "routes/home".to_string(),
                tier: crate::manifest::schema::Tier::A,
                weight_bytes: 1024,
                priority: 1.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: crate::manifest::schema::HydrationMode::None,
            }],
            parallel_batches: vec![vec![10]],
            critical_path: vec![10],
            vendor_chunks: Vec::new(),
        };

        let request = RouteRenderRequest {
            entry: "routes/home".to_string(),
            props_json: r#"{"name":"ALBEDO"}"#.to_string(),
            module_order: Vec::new(),
            hydration_payload: None,
        };

        let mut sources_v1 = HashMap::new();
        sources_v1.insert(
            "routes/home".to_string(),
            "(props) => '<main>v1 ' + props.name + '</main>'".to_string(),
        );
        renderer
            .register_manifest_modules(&manifest, &sources_v1)
            .unwrap();

        let first = renderer.render_route(&request).unwrap();
        let second = renderer.render_route(&request).unwrap();
        assert_eq!(first.html, "<main>v1 ALBEDO</main>");
        assert_eq!(second.timings.render_ms, 0);
        assert_eq!(second.timings.render_eval_ms, 0);

        let mut sources_v2 = HashMap::new();
        sources_v2.insert(
            "routes/home".to_string(),
            "(props) => '<main>v2 ' + props.name + '</main>'".to_string(),
        );
        renderer
            .register_manifest_modules(&manifest, &sources_v2)
            .unwrap();

        let updated = renderer.render_route(&request).unwrap();
        assert_eq!(updated.html, "<main>v2 ALBEDO</main>");
        assert!(updated.timings.modules_loaded_this_render >= 1);
    }

    #[test]
    fn test_revalidate_path_invalidates_route_static_slice_cache() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-27T00:00:00Z".to_string(),
            components: vec![crate::manifest::schema::ComponentManifestEntry {
                id: 11,
                name: "Entry".to_string(),
                module_path: "routes/home".to_string(),
                tier: crate::manifest::schema::Tier::A,
                weight_bytes: 1024,
                priority: 1.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: crate::manifest::schema::HydrationMode::None,
            }],
            parallel_batches: vec![vec![11]],
            critical_path: vec![11],
            vendor_chunks: Vec::new(),
        };

        let mut sources = HashMap::new();
        sources.insert(
            "routes/home".to_string(),
            "let count = 0; export default function App() { count += 1; return '<main>' + count + '</main>'; }".to_string(),
        );
        renderer
            .register_manifest_modules(&manifest, &sources)
            .unwrap();

        let request = RouteRenderRequest {
            entry: "routes/home".to_string(),
            props_json: "{}".to_string(),
            module_order: Vec::new(),
            hydration_payload: None,
        };

        let first = renderer.render_route(&request).unwrap();
        let warm_cache_hit = renderer.render_route(&request).unwrap();
        assert_eq!(first.html, "<main>1</main>");
        assert_eq!(warm_cache_hit.html, "<main>1</main>");

        renderer.revalidate_path("routes/home");
        let after_revalidate = renderer.render_route(&request).unwrap();
        assert_eq!(after_revalidate.html, "<main>2</main>");
    }

    #[test]
    fn test_revalidate_tag_invalidates_tagged_route_static_slice_cache() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-27T00:00:00Z".to_string(),
            components: vec![crate::manifest::schema::ComponentManifestEntry {
                id: 22,
                name: "Entry".to_string(),
                module_path: "routes/home".to_string(),
                tier: crate::manifest::schema::Tier::A,
                weight_bytes: 1024,
                priority: 1.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: crate::manifest::schema::HydrationMode::None,
            }],
            parallel_batches: vec![vec![22]],
            critical_path: vec![22],
            vendor_chunks: Vec::new(),
        };

        let mut sources = HashMap::new();
        sources.insert(
            "routes/home".to_string(),
            "let count = 0; export default function App() { count += 1; return '<main>' + count + '</main>'; }".to_string(),
        );
        renderer
            .register_manifest_modules(&manifest, &sources)
            .unwrap();
        renderer.register_route_tags("routes/home", vec!["homepage".to_string()]);

        let request = RouteRenderRequest {
            entry: "routes/home".to_string(),
            props_json: "{}".to_string(),
            module_order: Vec::new(),
            hydration_payload: None,
        };

        let first = renderer.render_route(&request).unwrap();
        let warm_cache_hit = renderer.render_route(&request).unwrap();
        assert_eq!(first.html, "<main>1</main>");
        assert_eq!(warm_cache_hit.html, "<main>1</main>");

        renderer.revalidate_tag("homepage");
        let after_revalidate = renderer.render_route(&request).unwrap();
        assert_eq!(after_revalidate.html, "<main>2</main>");
    }

    #[test]
    fn test_register_manifest_modules_with_precompiled_prefers_compiled_script() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-22T00:00:00Z".to_string(),
            components: vec![crate::manifest::schema::ComponentManifestEntry {
                id: 5,
                name: "Entry".to_string(),
                module_path: "routes/home".to_string(),
                tier: crate::manifest::schema::Tier::A,
                weight_bytes: 1024,
                priority: 1.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: crate::manifest::schema::HydrationMode::None,
            }],
            parallel_batches: vec![vec![5]],
            critical_path: vec![5],
            vendor_chunks: Vec::new(),
        };

        let source = "(props) => '<main>source ' + props.name + '</main>'".to_string();
        let mut sources = HashMap::new();
        sources.insert("routes/home".to_string(), source.clone());

        let precompiled_script = "(function() {\n  const __albedo_exports = Object.create(null);\n  Object.defineProperty(__albedo_exports, \"__albedo_is_module_record\", { value: true, enumerable: false });\n  const __albedo_default_export__ = ((props) => '<main>compiled ' + props.name + '</main>');\n  __albedo_exports.default = __albedo_default_export__;\n  globalThis.__ALBEDO_MODULES[\"routes/home\"] = __albedo_exports;\n})();".to_string();
        let precompiled = crate::manifest::schema::PrecompiledRuntimeModulesArtifact {
            version: crate::manifest::schema::PrecompiledRuntimeModulesArtifact::VERSION
                .to_string(),
            engine: crate::manifest::schema::PrecompiledRuntimeModulesArtifact::ENGINE_QUICKJS
                .to_string(),
            manifest_schema_version: manifest.schema_version.clone(),
            manifest_generated_at: manifest.generated_at.clone(),
            modules: vec![crate::manifest::schema::PrecompiledRuntimeModuleEntry {
                component_id: 5,
                module_path: "routes/home".to_string(),
                source_hash: stable_source_hash(&source),
                compiled_script: precompiled_script,
            }],
            skipped: Vec::new(),
        };

        renderer
            .register_manifest_modules_with_precompiled(&manifest, &sources, Some(&precompiled))
            .unwrap();
        let result = renderer
            .render_route(&RouteRenderRequest {
                entry: "routes/home".to_string(),
                props_json: r#"{"name":"ALBEDO"}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            })
            .unwrap();

        assert_eq!(result.html, "<main>compiled ALBEDO</main>");
    }

    #[test]
    fn test_renderer_new_reports_init_error_for_invalid_bootstrap() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload {
            dom_shim_js: "(".to_string(),
            runtime_helpers_js: String::new(),
            preloaded_libraries: Vec::new(),
        };

        let err = match ServerRenderer::new(engine, &bootstrap) {
            Ok(_) => panic!("renderer initialization should have failed"),
            Err(err) => err,
        };
        assert!(matches!(err, RuntimeError::InitError(_)));
    }

    #[test]
    fn test_module_registry_resolves_topological_order() {
        let mut registry = ModuleRegistry::default();
        registry.register_module(
            "shared/badge",
            "(props) => '<span>' + props.label + '</span>'",
        );
        registry.register_module_with_dependencies(
            "components/hero",
            "(props, require) => '<section>' + require('shared/badge')({label: props.tag}) + '</section>'",
            vec!["shared/badge".to_string()],
        );
        registry.register_module_with_dependencies(
            "routes/home",
            "(props, require) => '<main>' + require('components/hero')(props) + '</main>'",
            vec!["components/hero".to_string()],
        );

        let order = registry.resolve_module_order("routes/home", &[]).unwrap();
        assert_eq!(
            order,
            vec![
                "shared/badge".to_string(),
                "components/hero".to_string(),
                "routes/home".to_string()
            ]
        );
    }

    #[test]
    fn test_render_route_reports_missing_dependency() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module_with_dependencies(
            "routes/home",
            "(props, require) => require('shared/missing')(props)",
            vec!["shared/missing".to_string()],
        );

        let err = renderer
            .render_route(&RouteRenderRequest {
                entry: "routes/home".to_string(),
                props_json: "{}".to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            })
            .unwrap_err();

        assert!(matches!(
            err,
            RuntimeError::LoadError {
                kind: LoadErrorKind::ModuleMissing,
                ..
            }
        ));
        assert!(err.to_string().contains("shared/missing"));
    }

    #[test]
    fn test_render_route_reports_cyclic_module_graph() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module_with_dependencies(
            "components/a",
            "(props, require) => require('components/b')(props)",
            vec!["components/b".to_string()],
        );
        renderer.register_module_with_dependencies(
            "components/b",
            "(props, require) => require('components/a')(props)",
            vec!["components/a".to_string()],
        );

        let err = renderer
            .render_route(&RouteRenderRequest {
                entry: "components/a".to_string(),
                props_json: "{}".to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            })
            .unwrap_err();

        assert!(matches!(
            err,
            RuntimeError::LoadError {
                kind: LoadErrorKind::DependencyCycle,
                ..
            }
        ));
        assert!(err.to_string().contains("dependency cycle"));
    }

    #[test]
    fn test_render_route_reports_invalid_props_json() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module("app", "(props) => '<main>' + props.name + '</main>'");

        let err = renderer
            .render_route(&RouteRenderRequest {
                entry: "app".to_string(),
                props_json: "{invalid".to_string(),
                module_order: vec!["app".to_string()],
                hydration_payload: None,
            })
            .unwrap_err();

        assert!(matches!(err, RuntimeError::PropsError(_)));
    }

    #[test]
    fn test_render_route_reports_invalid_entry_export() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module(
            "routes/named-only",
            "export function NamedOnly(props) { return '<main>' + props.name + '</main>'; }",
        );

        let err = renderer
            .render_route(&RouteRenderRequest {
                entry: "routes/named-only".to_string(),
                props_json: r#"{"name":"ALBEDO"}"#.to_string(),
                module_order: vec!["routes/named-only".to_string()],
                hydration_payload: None,
            })
            .unwrap_err();

        assert!(matches!(
            err,
            RuntimeError::LoadError {
                kind: LoadErrorKind::InvalidEntryExport,
                ..
            }
        ));
    }

    #[test]
    fn test_quickjs_loader_supports_common_export_shapes() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module(
            "shared/constants",
            "export function label() { return 'ALBEDO'; } export const SUFFIX = '!';",
        );
        renderer.register_module_with_dependencies(
            "routes/export-shapes",
            "export default function App(props, require) { const constants = require('shared/constants'); return '<main>' + props.name + constants.SUFFIX + '</main>'; }",
            vec!["shared/constants".to_string()],
        );

        let result = renderer
            .render_route(&RouteRenderRequest {
                entry: "routes/export-shapes".to_string(),
                props_json: r#"{"name":"ALBEDO"}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            })
            .unwrap();

        assert_eq!(result.html, "<main>ALBEDO!</main>");
    }

    #[test]
    fn test_render_route_with_dependency_graph() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module(
            "shared/badge",
            "(props) => '<span class=\"badge\">' + props.label + '</span>'",
        );
        renderer.register_module_with_dependencies(
            "components/hero",
            "(props, require) => '<section><h1>' + props.title + '</h1>' + require('shared/badge')({label: props.tag}) + '</section>'",
            vec!["shared/badge".to_string()],
        );
        renderer.register_module_with_dependencies(
            "routes/home",
            "(props, require) => '<main>' + require('components/hero')(props) + '</main>'",
            vec!["components/hero".to_string()],
        );

        let request = RouteRenderRequest {
            entry: "routes/home".to_string(),
            props_json: r#"{"title":"ALBEDO","tag":"Renderer"}"#.to_string(),
            module_order: Vec::new(),
            hydration_payload: Some(r#"{"route":"home"}"#.to_string()),
        };

        let result = renderer.render_route(&request).unwrap();
        assert_eq!(
            result.html,
            "<main><section><h1>ALBEDO</h1><span class=\"badge\">Renderer</span></section></main>"
        );
        assert_eq!(result.hydration_payload, r#"{"route":"home"}"#);
    }

    #[test]
    fn test_render_route_stream_emits_deferred_html_chunks_from_engine() {
        let engine = DeferredChunkEngine;
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module("app", "(props) => '<main>' + props.name + '</main>'");

        let streamed = renderer
            .render_route_stream(&RouteRenderRequest {
                entry: "app".to_string(),
                props_json: r#"{"name":"ALBEDO"}"#.to_string(),
                module_order: vec!["app".to_string()],
                hydration_payload: None,
            })
            .unwrap();

        assert_eq!(streamed.chunks.len(), 3);
        assert_eq!(streamed.chunks[0].kind, RouteStreamChunkKind::ShellHtml);
        assert_eq!(streamed.chunks[0].content, "<main>");
        assert_eq!(streamed.chunks[1].kind, RouteStreamChunkKind::DeferredHtml);
        assert_eq!(streamed.chunks[1].content, "ALBEDO");
        assert_eq!(streamed.chunks[2].kind, RouteStreamChunkKind::DeferredHtml);
        assert_eq!(streamed.chunks[2].content, "</main>");
    }

    #[test]
    fn test_render_route_stream_emits_shell_and_hydration_chunks() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module("app", "(props) => '<main>' + props.name + '</main>'");

        let streamed = renderer
            .render_route_stream(&RouteRenderRequest {
                entry: "app".to_string(),
                props_json: r#"{"name":"ALBEDO"}"#.to_string(),
                module_order: vec!["app".to_string()],
                hydration_payload: Some(r#"{"route":"home"}"#.to_string()),
            })
            .unwrap();

        assert_eq!(streamed.chunks[0].kind, RouteStreamChunkKind::ShellHtml);
        assert_eq!(streamed.chunks[0].content, "<main>ALBEDO</main>");
        assert!(streamed
            .chunks
            .iter()
            .any(|chunk| chunk.kind == RouteStreamChunkKind::HydrationPayload
                && chunk.content == r#"{"route":"home"}"#));
    }

    #[test]
    fn test_render_route_stream_with_manifest_hydration_adds_head_tag_chunks() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-26T00:00:00Z".to_string(),
            components: vec![
                crate::manifest::schema::ComponentManifestEntry {
                    id: 1,
                    name: "Leaf".to_string(),
                    module_path: "components/leaf".to_string(),
                    tier: crate::manifest::schema::Tier::B,
                    weight_bytes: 1024,
                    priority: 1.0,
                    dependencies: vec![],
                    can_defer: false,
                    hydration_mode: crate::manifest::schema::HydrationMode::OnIdle,
                },
                crate::manifest::schema::ComponentManifestEntry {
                    id: 2,
                    name: "Entry".to_string(),
                    module_path: "routes/entry".to_string(),
                    tier: crate::manifest::schema::Tier::C,
                    weight_bytes: 2048,
                    priority: 2.0,
                    dependencies: vec![1],
                    can_defer: false,
                    hydration_mode: crate::manifest::schema::HydrationMode::OnInteraction,
                },
            ],
            parallel_batches: vec![vec![1], vec![2]],
            critical_path: vec![1, 2],
            vendor_chunks: Vec::new(),
        };

        let mut sources = HashMap::new();
        sources.insert(
            "components/leaf".to_string(),
            "(props) => '<p>' + props.text + '</p>'".to_string(),
        );
        sources.insert(
            "routes/entry".to_string(),
            "(props, require) => '<article>' + require('components/leaf')({text: props.text}) + '</article>'".to_string(),
        );

        renderer
            .register_manifest_modules(&manifest, &sources)
            .unwrap();

        let streamed = renderer
            .render_route_stream_with_manifest_hydration(
                &RouteRenderRequest {
                    entry: "routes/entry".to_string(),
                    props_json: r#"{"text":"hello"}"#.to_string(),
                    module_order: Vec::new(),
                    hydration_payload: None,
                },
                &manifest,
            )
            .unwrap();

        assert!(streamed
            .chunks
            .iter()
            .any(|chunk| chunk.kind == RouteStreamChunkKind::HeadTag));
    }

    #[test]
    fn test_register_manifest_modules_and_render() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-12T00:00:00Z".to_string(),
            components: vec![
                crate::manifest::schema::ComponentManifestEntry {
                    id: 1,
                    name: "Leaf".to_string(),
                    module_path: "components/leaf".to_string(),
                    tier: crate::manifest::schema::Tier::A,
                    weight_bytes: 1024,
                    priority: 1.0,
                    dependencies: vec![],
                    can_defer: false,
                    hydration_mode: crate::manifest::schema::HydrationMode::None,
                },
                crate::manifest::schema::ComponentManifestEntry {
                    id: 2,
                    name: "Entry".to_string(),
                    module_path: "routes/entry".to_string(),
                    tier: crate::manifest::schema::Tier::B,
                    weight_bytes: 2048,
                    priority: 2.0,
                    dependencies: vec![1],
                    can_defer: false,
                    hydration_mode: crate::manifest::schema::HydrationMode::OnIdle,
                },
            ],
            parallel_batches: vec![vec![1], vec![2]],
            critical_path: vec![1, 2],
            vendor_chunks: Vec::new(),
        };

        let mut sources = HashMap::new();
        sources.insert(
            "components/leaf".to_string(),
            "(props) => '<p>' + props.text + '</p>'".to_string(),
        );
        sources.insert(
            "routes/entry".to_string(),
            "(props, require) => '<article>' + require('components/leaf')({text: props.text}) + '</article>'".to_string(),
        );

        renderer
            .register_manifest_modules(&manifest, &sources)
            .unwrap();

        let result = renderer
            .render_route(&RouteRenderRequest {
                entry: "routes/entry".to_string(),
                props_json: r#"{"text":"hello"}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            })
            .unwrap();

        assert_eq!(result.html, "<article><p>hello</p></article>");
    }

    #[test]
    fn test_render_route_from_test_app_component_directory() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let components_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-app")
            .join("src")
            .join("components");
        let result = renderer
            .render_route_from_component_dir(&FsRouteRenderRequest {
                components_root,
                entry_module: "App.jsx".to_string(),
                props_json: "{}".to_string(),
                hydration_payload: None,
            })
            .unwrap();

        assert!(result.html.contains("<div class=\"App\">"));
        assert!(result.html.contains("<h1>My App</h1>"));
        assert!(result.html.contains("<button>Home</button>"));
        assert!(result.html.contains("<h3>Scalable</h3>"));
        assert!(result.html.contains("<p>© 2026 My App</p>"));
    }

    #[test]
    fn test_ast_fallback_reports_unsupported_syntax_cleanly() {
        let temp_dir = tempfile::tempdir().unwrap();
        let app_file = temp_dir.path().join("App.jsx");
        std::fs::write(
            &app_file,
            "export default function App(props) { return <main>{props.items.map((item) => item).join(',')}</main>; }",
        )
        .unwrap();

        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        let err = renderer
            .render_route_from_component_dir(&FsRouteRenderRequest {
                components_root: temp_dir.path().to_path_buf(),
                entry_module: "App.jsx".to_string(),
                props_json: r#"{"items":["a","b"]}"#.to_string(),
                hydration_payload: None,
            })
            .unwrap_err();

        assert!(matches!(err, RuntimeError::RenderError(_)));
        assert!(err.to_string().contains("unsupported expression"));
    }

    #[test]
    fn test_quickjs_cold_vs_warm_render_smoke() {
        let engine = QuickJsEngine::new();
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).unwrap();

        renderer.register_module("app", "(props) => '<main>' + props.name + '</main>'");

        let request = RouteRenderRequest {
            entry: "app".to_string(),
            props_json: r#"{"name":"ALBEDO"}"#.to_string(),
            module_order: vec!["app".to_string()],
            hydration_payload: None,
        };

        let cold_start = Instant::now();
        let cold = renderer.render_route(&request).unwrap();
        let cold_elapsed_ms = cold_start.elapsed().as_secs_f64() * 1000.0;

        renderer.warm_runtime().unwrap();

        let warm_start = Instant::now();
        for _ in 0..5 {
            let result = renderer.render_route(&request).unwrap();
            assert_eq!(result.html, cold.html);
        }
        let warm_avg_elapsed_ms = (warm_start.elapsed().as_secs_f64() * 1000.0) / 5.0;

        assert!(cold_elapsed_ms >= 0.0);
        assert!(warm_avg_elapsed_ms <= (cold_elapsed_ms * 10.0) + 5.0);
    }
}
