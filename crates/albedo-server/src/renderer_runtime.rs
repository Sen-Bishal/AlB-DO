use crate::config::RendererConfig;
use crate::error::RuntimeError;
use dom_render_compiler::bundler::emit::{
    BUNDLE_PRECOMPILED_MODULES_FILENAME, BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME,
    BUNDLE_STATIC_SLICES_FILENAME,
};
use dom_render_compiler::manifest::schema::{PrecompiledRuntimeModulesArtifact, RenderManifestV2};
use dom_render_compiler::runtime::engine::BootstrapPayload;
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::renderer::{
    RouteRenderRequest, RouteRenderStreamResult, ServerRenderer,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const RENDER_MANIFEST_FILENAME: &str = "render-manifest.v2.json";
pub const RUNTIME_MODULE_SOURCES_FILENAME: &str = "runtime-module-sources.json";

pub struct RendererRuntime {
    manifest: RenderManifestV2,
    renderer: ServerRenderer<QuickJsEngine>,
}

impl RendererRuntime {
    pub fn from_config(config: &RendererConfig) -> Result<Self, RuntimeError> {
        let artifacts_dir = PathBuf::from(config.artifacts_dir.as_str());
        Self::from_artifacts_dir(artifacts_dir)
    }

    pub fn from_artifacts_dir(artifacts_dir: PathBuf) -> Result<Self, RuntimeError> {
        let manifest_path = artifacts_dir.join(RENDER_MANIFEST_FILENAME);
        let manifest: RenderManifestV2 = read_json(&manifest_path)?;

        // The standalone runtime expects these artifacts to exist even if route handlers
        // do not consume them directly yet. This keeps build/runtime contracts explicit.
        assert_optional_artifact_present(&artifacts_dir.join(BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME));
        assert_optional_artifact_present(&artifacts_dir.join(BUNDLE_STATIC_SLICES_FILENAME));

        let module_sources = load_module_sources(&artifacts_dir, &manifest)?;
        let precompiled_modules = load_precompiled_modules(&artifacts_dir)?;

        let engine = QuickJsEngine::new().map_err(|err| {
            RuntimeError::RendererFailure(format!("failed to initialize QuickJS engine: {err}"))
        })?;
        let bootstrap = BootstrapPayload::default();
        let mut renderer = ServerRenderer::new(engine, &bootstrap).map_err(|err| {
            RuntimeError::RendererFailure(format!("failed to initialize server renderer: {err}"))
        })?;
        renderer
            .register_manifest_modules_with_precompiled(
                &manifest,
                &module_sources,
                precompiled_modules.as_ref(),
            )
            .map_err(|err| RuntimeError::RendererFailure(err.to_string()))?;

        Ok(Self { manifest, renderer })
    }

    pub fn render_route_stream(
        &mut self,
        entry_module: &str,
        props_json: String,
    ) -> Result<RouteRenderStreamResult, RuntimeError> {
        let request = RouteRenderRequest {
            entry: entry_module.to_string(),
            props_json,
            module_order: Vec::new(),
            hydration_payload: None,
        };

        self.renderer
            .render_route_stream_with_manifest_hydration(&request, &self.manifest)
            .map_err(|err| RuntimeError::RendererFailure(err.to_string()))
    }

    pub fn revalidate_path(&mut self, path: &str) {
        self.renderer.revalidate_path(path);
    }

    pub fn revalidate_tag(&mut self, tag: &str) {
        self.renderer.revalidate_tag(tag);
    }
}

fn load_precompiled_modules(
    artifacts_dir: &Path,
) -> Result<Option<PrecompiledRuntimeModulesArtifact>, RuntimeError> {
    let path = artifacts_dir.join(BUNDLE_PRECOMPILED_MODULES_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let artifact: PrecompiledRuntimeModulesArtifact = read_json(&path)?;
    Ok(Some(artifact))
}

fn load_module_sources(
    artifacts_dir: &Path,
    manifest: &RenderManifestV2,
) -> Result<HashMap<String, String>, RuntimeError> {
    let module_sources_path = artifacts_dir.join(RUNTIME_MODULE_SOURCES_FILENAME);
    if module_sources_path.exists() {
        let artifact: RuntimeModuleSourcesArtifact = read_json(&module_sources_path)?;
        let modules = artifact
            .modules
            .into_iter()
            .map(|module| (module.module_path, module.code))
            .collect();
        return Ok(modules);
    }

    let mut module_sources = HashMap::new();
    for component in &manifest.components {
        if module_sources.contains_key(&component.module_path) {
            continue;
        }
        let source = std::fs::read_to_string(component.module_path.as_str()).map_err(|err| {
            RuntimeError::RendererArtifactIo {
                path: component.module_path.clone(),
                message: err.to_string(),
            }
        })?;
        module_sources.insert(component.module_path.clone(), source);
    }

    Ok(module_sources)
}

fn assert_optional_artifact_present(_path: &Path) {
    // Presence checks are best-effort for now; full integrity enforcement is handled by
    // standalone pipeline verification.
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, RuntimeError> {
    let raw = std::fs::read_to_string(path).map_err(|err| RuntimeError::RendererArtifactIo {
        path: path.display().to_string(),
        message: err.to_string(),
    })?;
    serde_json::from_str(&raw).map_err(|err| RuntimeError::RendererArtifactParse {
        path: path.display().to_string(),
        message: err.to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct RuntimeModuleSourcesArtifact {
    modules: Vec<RuntimeModuleSourceEntry>,
}

#[derive(Debug, Deserialize)]
struct RuntimeModuleSourceEntry {
    module_path: String,
    code: String,
}
