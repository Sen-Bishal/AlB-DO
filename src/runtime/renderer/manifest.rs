use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::hydration;
use crate::manifest::schema::{PrecompiledRuntimeModulesArtifact, RenderManifestV2};
use crate::runtime::engine::{stable_source_hash, BootstrapPayload, RuntimeEngine, RuntimeResult};
use crate::runtime::eval::ComponentProject;
use crate::runtime::static_slice::build_static_slice_manifest;

use super::core::{
    entry_matches_path, normalize_invalidation_token, route_render_result_to_stream, unique_stable,
    FsRouteRenderRequest, ModuleRegistry, RenderTimings, RouteRenderRequest, RouteRenderResult,
    RouteRenderStreamResult, StaticSliceCacheKey,
};

const PROPS_CACHE_MAX_ENTRIES: usize = 256;

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
                crate::runtime::engine::RuntimeError::render(format!(
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
                crate::runtime::engine::RuntimeError::render(format!(
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
                crate::runtime::engine::RuntimeError::load(
                    crate::runtime::engine::LoadErrorKind::ModuleMissing,
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
                crate::runtime::engine::RuntimeError::load(
                    crate::runtime::engine::LoadErrorKind::ModuleMissing,
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
                crate::runtime::engine::RuntimeError::render(format!(
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
            crate::runtime::engine::RuntimeError::render(format!(
                "failed to load component project from '{}': {err}",
                route.components_root.display()
            ))
        })?;
        let module_load_ms = load_start.elapsed().as_millis();

        let render_start = Instant::now();
        let props_value: serde_json::Value =
            serde_json::from_str(&route.props_json).map_err(|err| {
                crate::runtime::engine::RuntimeError::props(format!(
                    "invalid props JSON for filesystem route render '{}': {err}",
                    route.entry_module
                ))
            })?;
        let html = project
            .render_entry(&route.entry_module, &props_value)
            .map_err(|err| {
                crate::runtime::engine::RuntimeError::render(format!(
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
            crate::runtime::engine::RuntimeError::props(format!(
                "invalid props JSON for route '{entry}': {err}"
            ))
        })?;
        let normalized_props = serde_json::to_string(&props_value).map_err(|err| {
            crate::runtime::engine::RuntimeError::props(format!(
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
