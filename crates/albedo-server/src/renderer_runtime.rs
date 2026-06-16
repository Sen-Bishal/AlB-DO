use crate::config::RendererConfig;
use crate::error::RuntimeError;
use dom_render_compiler::bundler::emit::{
    BUNDLE_PRECOMPILED_MODULES_FILENAME, BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME,
    BUNDLE_STATIC_SLICES_FILENAME,
};
use dom_render_compiler::hydration::payload::{build_hydration_payload, serialize_hydration_payload};
use dom_render_compiler::hydration::plan::{
    HydrationIslandPlan, HydrationPlan, HydrationTrigger, HYDRATION_PLAN_VERSION,
};
use dom_render_compiler::hydration::script::{build_bootstrap_script_tag, build_payload_script_tag};
use dom_render_compiler::manifest::schema::{
    ComponentManifestEntry, HydrationMode, PrecompiledRuntimeModulesArtifact, RenderManifestV2,
};
use dom_render_compiler::runtime::engine::BootstrapPayload;
use dom_render_compiler::runtime::quickjs_engine::{compile_client_island_module, QuickJsEngine};
use dom_render_compiler::runtime::renderer::{
    inject_island_marker, RouteRenderRequest, RouteRenderStreamResult, ServerRenderer,
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

/// A3 · per-route client-hydration artifacts, precomputed once at boot. The
/// streaming handler fills each Tier-C placeholder with `marked_html` (the
/// island's SSR output stamped with `data-albedo-island`) and emits
/// `closing_scripts` (the client runtime + per-island IIFEs + payload +
/// bootstrap) before `</body>`. Computing this at boot keeps the `!Send`
/// QuickJS render off the concurrent request path.
#[derive(Debug, Clone, Default)]
pub struct RouteHydration {
    /// `(placeholder_id, marked_island_html)` pairs. The placeholder's empty
    /// `<div data-albedo-tier="c"></div>` is replaced wholesale by the marked
    /// HTML so the island marker lands on the component's own root element.
    pub placeholders: Vec<(String, String)>,
    /// The `<script>` block emitted before `</body>`.
    pub closing_scripts: String,
}

/// Make `js` safe to embed verbatim in an inline `<script>`: only `</` can
/// terminate the element early; the backslash is inert inside JS literals.
fn escape_inline_script(js: &str) -> String {
    js.replace("</", "<\\/")
}

fn trigger_from_mode(mode: HydrationMode) -> HydrationTrigger {
    match mode {
        HydrationMode::LazyInteraction | HydrationMode::OnInteraction => {
            HydrationTrigger::Interaction
        }
        HydrationMode::LazyViewport | HydrationMode::OnVisible => HydrationTrigger::Visible,
        _ => HydrationTrigger::Idle,
    }
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
        assert_optional_artifact_present(
            &artifacts_dir.join(BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME),
        );
        assert_optional_artifact_present(&artifacts_dir.join(BUNDLE_STATIC_SLICES_FILENAME));

        let module_sources = load_module_sources(&artifacts_dir, &manifest)?;
        let precompiled_modules = load_precompiled_modules(&artifacts_dir)?;

        let engine = QuickJsEngine::new();
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

        // Startup priming pass: pre-render every manifest route so the
        // static-slice and normalized-props caches are warm on the very
        // first request after deploy instead of paying engine + encoder
        // warmup on every cold entry. Soft-fail by design — a priming
        // error degrades to a cold cache, no new failure mode.
        let warm_requests: Vec<RouteRenderRequest> = manifest
            .routes
            .keys()
            .map(|entry| RouteRenderRequest {
                entry: entry.clone(),
                props_json: "{}".to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
                host_json: None,
            })
            .collect();

        if !warm_requests.is_empty() {
            if let Err(err) = renderer.prime_runtime_cache(&warm_requests) {
                tracing::warn!(target: "albedo.renderer", error = %err, "cache priming failed");
            }
        }

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
            host_json: None,
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

    pub fn manifest(&self) -> &RenderManifestV2 {
        &self.manifest
    }

    /// A3 · precompute the client-hydration block for every manifest route that
    /// carries a hydratable Tier-C island. Each island is SSR-rendered standalone
    /// (so the placeholder shows real markup the browser can adopt and the user
    /// can interact with) and lowered to a self-registering browser IIFE. The
    /// returned map is keyed by route path and consumed by the streaming handler.
    /// Best-effort throughout: an island whose source can't render or compile is
    /// skipped, degrading to a non-interactive server page rather than failing.
    pub fn build_hydration_blocks(&mut self) -> HashMap<String, RouteHydration> {
        struct IslandMeta {
            placeholder_id: String,
            component_id: u64,
            module_path: String,
            source: String,
            trigger: HydrationTrigger,
        }

        // Phase 1 — gather island metadata from the manifest (immutable borrows
        // only), so phase 2 is free to take `&mut self.renderer` to render.
        let by_name: HashMap<&str, &ComponentManifestEntry> = self
            .manifest
            .components
            .iter()
            .map(|c| (c.name.as_str(), c))
            .collect();

        let mut routes: Vec<(String, Vec<IslandMeta>)> = Vec::new();
        for (path, route) in &self.manifest.routes {
            let mut islands = Vec::new();
            for node in &route.tier_c {
                if node.hydration_mode == HydrationMode::None {
                    continue;
                }
                let Some(component) = by_name.get(node.component_id.as_str()) else {
                    continue;
                };
                let Some(module) = self.renderer.module_registry().module(&component.module_path)
                else {
                    continue;
                };
                islands.push(IslandMeta {
                    placeholder_id: node.placeholder_id.clone(),
                    component_id: component.id,
                    module_path: component.module_path.clone(),
                    source: module.code.clone(),
                    trigger: trigger_from_mode(node.hydration_mode),
                });
            }
            if !islands.is_empty() {
                routes.push((path.clone(), islands));
            }
        }

        // Phase 2 — render + compile each island, assemble the per-route block.
        let mut blocks = HashMap::new();
        for (path, islands) in routes {
            let mut placeholders = Vec::new();
            let mut scripts = String::from("<script src=\"/_albedo/client.js\"></script>");
            let mut plan_islands = Vec::new();

            for island in &islands {
                if let Some(html) = self.render_island_html(&island.module_path) {
                    placeholders.push((
                        island.placeholder_id.clone(),
                        inject_island_marker(&html, island.component_id),
                    ));
                }
                if let Ok(iife) = compile_client_island_module(
                    &island.module_path,
                    &island.source,
                    island.component_id,
                ) {
                    scripts.push_str("<script>");
                    scripts.push_str(&escape_inline_script(&iife));
                    scripts.push_str("</script>");
                }
                plan_islands.push(HydrationIslandPlan {
                    component_id: island.component_id,
                    module_path: island.module_path.clone(),
                    trigger: island.trigger,
                    dependencies: Vec::new(),
                });
            }

            // Payload + bootstrap reuse the hydration crate's pure builders. The
            // plan `entry` is the route path (matches no module), so every island
            // hydrates from `{}` — consistent with the standalone SSR above.
            let plan = HydrationPlan {
                version: HYDRATION_PLAN_VERSION.to_string(),
                entry: path.clone(),
                islands: plan_islands,
            };
            if let Ok(payload) = build_hydration_payload(&self.manifest, &plan, "{}") {
                if let Ok(payload_json) = serialize_hydration_payload(&payload) {
                    scripts.push_str(&build_payload_script_tag(
                        &payload_json,
                        &payload.checksum,
                        &payload.version,
                    ));
                    scripts.push_str(&build_bootstrap_script_tag(
                        &payload.checksum,
                        &payload.version,
                    ));
                }
            }

            blocks.insert(
                path,
                RouteHydration {
                    placeholders,
                    closing_scripts: scripts,
                },
            );
        }
        blocks
    }

    /// Render one island component to its SSR HTML from default props. Soft-fails
    /// to `None` so a single bad island can't sink the whole boot.
    fn render_island_html(&mut self, module_path: &str) -> Option<String> {
        let request = RouteRenderRequest {
            entry: module_path.to_string(),
            props_json: "{}".to_string(),
            module_order: Vec::new(),
            hydration_payload: None,
            host_json: None,
        };
        match self.renderer.render_route(&request) {
            Ok(result) => Some(result.html),
            Err(err) => {
                tracing::warn!(
                    target: "albedo.renderer",
                    module_path,
                    error = %err,
                    "island SSR render failed; placeholder stays empty"
                );
                None
            }
        }
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
