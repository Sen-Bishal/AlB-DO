use crate::config::RendererConfig;
use crate::error::RuntimeError;
use dom_render_compiler::bundler::emit::{
    BUNDLE_PRECOMPILED_MODULES_FILENAME, BUNDLE_ROUTE_PREFETCH_MANIFEST_FILENAME,
    BUNDLE_STATIC_SLICES_FILENAME,
};
use dom_render_compiler::hydration::payload::{
    build_hydration_payload, serialize_hydration_payload,
};
use dom_render_compiler::hydration::plan::{
    HydrationIslandPlan, HydrationPlan, HydrationTrigger, HYDRATION_PLAN_VERSION,
};
use dom_render_compiler::hydration::script::{
    build_bootstrap_script_tag, build_payload_script_tag,
};
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
                let Some(module) = self
                    .renderer
                    .module_registry()
                    .module(&component.module_path)
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

    /// Step 3 (binding mode) — precompute the fine-grained reactive block for
    /// every route whose Tier-C component(s) the analysis proved client-driveable
    /// from text bindings alone. Unlike A3 (which hydrates a whole component), a
    /// binding-mode route ships the Phase K static HTML (carrying `data-albedo-id`
    /// stamps) into the placeholder and a tiny inline driver that runs the handler
    /// locally and patches the bound text nodes — no VDOM, no hydration, no
    /// round-trip.
    ///
    /// Returns a `RouteHydration` per eligible route, keyed by route path, so it
    /// drops straight into the same streaming plumbing A3 uses. Fallback-safe:
    /// any component whose payload can't be built (entry won't resolve, no
    /// text/event bindings, structural reactivity) is skipped, so the route falls
    /// back to the A3 island path with no regression.
    pub fn build_reactive_blocks(
        &self,
        compiled: &dom_render_compiler::runtime::CompiledProject,
    ) -> HashMap<String, RouteHydration> {
        use dom_render_compiler::runtime::eval::SessionSlotView;
        use dom_render_compiler::runtime::slot_store::SlotStore;
        use dom_render_compiler::runtime::SessionId;
        use std::sync::Arc;

        let driver = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/albedo-reactive.js"
        ));

        let empty_props = serde_json::Value::Object(Default::default());
        let mut blocks = HashMap::new();

        for (path, route) in &self.manifest.routes {
            let mut placeholders = Vec::new();
            let mut installs = String::new();

            for node in &route.tier_c {
                if node.hydration_mode == HydrationMode::None {
                    continue;
                }
                // The manifest names the component; resolve it to the render-entry
                // spec the compiled project keys on (its absolute `module_path`
                // won't match the project-relative module specs).
                let Some(entry) = compiled.module_spec_for_component(&node.component_id) else {
                    continue;
                };

                let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
                let payload = match compiled.build_reactive_payload(entry, &empty_props, &slots) {
                    // Binding mode requires at least one text/attr/derived binding
                    // driven by at least one client handler. Anything else (no
                    // handler, no slot read, structural-only) is not eligible —
                    // fall through to the A3 island path.
                    Ok(p)
                        if (!p.texts.is_empty()
                            || !p.attrs.is_empty()
                            || !p.derived.is_empty())
                            && !p.events.is_empty() =>
                    {
                        p
                    }
                    _ => continue,
                };

                // Fill the empty Tier-C placeholder with the Phase K HTML — the
                // SAME render the binding frame was emitted from, so its
                // `data-albedo-id` stamps line up with every BindEvent/SetTextRef.
                placeholders.push((node.placeholder_id.clone(), payload.html.clone()));

                if let Ok(json) = serde_json::to_string(&payload) {
                    installs
                        .push_str("<script>window.__albedoReactive&&window.__albedoReactive.boot(");
                    installs.push_str(&escape_inline_script(&json));
                    installs.push_str(");</script>");
                }
            }

            if !placeholders.is_empty() {
                let mut scripts = String::from("<script>");
                scripts.push_str(&escape_inline_script(driver));
                scripts.push_str("</script>");
                scripts.push_str(&installs);
                blocks.insert(
                    path.clone(),
                    RouteHydration {
                        placeholders,
                        closing_scripts: scripts,
                    },
                );
            }
        }

        blocks
    }

    /// Build the per-component Tier-B render plan consumed by
    /// [`crate::render::tier_b::PooledTierBRenderRegistry`]. For every Tier-B
    /// node across all manifest routes, resolve its component to an entry module
    /// and the topologically-ordered module graph it needs, capturing the source
    /// of each module so a (`Send`) pool engine can load + render it off the boot
    /// thread.
    ///
    /// Built here, on the boot thread, because module-order resolution and source
    /// access go through the `!Send` renderer's module registry. The result owns
    /// all its strings, so it ships freely into the concurrent request path.
    /// Best-effort per node: a component whose order can't resolve or whose
    /// source is missing is skipped (it falls back to the registry's loud
    /// "no plan" error at request time rather than rendering wrong HTML).
    #[must_use]
    pub fn build_tier_b_render_plan(&self) -> crate::render::tier_b::TierBRenderPlan {
        let by_name: HashMap<&str, &ComponentManifestEntry> = self
            .manifest
            .components
            .iter()
            .map(|c| (c.name.as_str(), c))
            .collect();

        let mut plan = crate::render::tier_b::TierBRenderPlan::new();
        for route in self.manifest.routes.values() {
            for node in &route.tier_b {
                self.add_component_to_plan(&mut plan, &by_name, &node.render_fn, &node.component_id);
            }

            // Route boundaries (`error.tsx` / `loading.tsx`) are rendered on the
            // request path through the same pooled registry when a Tier-B node
            // throws or times out, so they need boot-built plans too. Keyed by
            // the bare component name (the registry is called with that name);
            // no collision with the `render::*`-shaped Tier-B keys.
            if let Some(name) = route.error_component.as_deref() {
                self.add_component_to_plan(&mut plan, &by_name, name, name);
            }
            if let Some(name) = route.loading_component.as_deref() {
                self.add_component_to_plan(&mut plan, &by_name, name, name);
            }

            // Slice 3 — a route exporting `generateMetadata` needs its leaf
            // module in the pool so the request path can evaluate the export.
            // Registered under the bare component name, the same key the serve
            // path calls `call_metadata` with.
            if let Some(name) = route.dynamic_metadata.as_deref() {
                self.add_component_to_plan(&mut plan, &by_name, name, name);
            }
        }
        plan
    }

    /// Resolve one component's entry module + dependency-ordered source graph and
    /// insert it into `plan` under `key`. Best-effort: a component that isn't in
    /// the manifest, whose module order can't resolve, or whose sources are
    /// missing is logged and skipped (it then surfaces as the registry's loud
    /// "no plan" error at request time rather than rendering wrong HTML).
    /// Idempotent per `key`.
    fn add_component_to_plan(
        &self,
        plan: &mut crate::render::tier_b::TierBRenderPlan,
        by_name: &HashMap<&str, &ComponentManifestEntry>,
        key: &str,
        component_name: &str,
    ) {
        if plan.contains_key(key) {
            return;
        }
        let Some(component) = by_name.get(component_name) else {
            tracing::warn!(
                target: "albedo.renderer",
                key = %key,
                component = %component_name,
                "tier-b component not found in manifest; render plan skipped"
            );
            return;
        };
        let entry = component.module_path.clone();

        let order = match self
            .renderer
            .module_registry()
            .resolve_module_order(&entry, &[])
        {
            Ok(order) => order,
            Err(err) => {
                tracing::warn!(
                    target: "albedo.renderer",
                    key = %key,
                    entry = %entry,
                    error = %err,
                    "tier-b module order unresolved; render plan skipped"
                );
                return;
            }
        };

        let mut modules = Vec::with_capacity(order.len());
        for specifier in &order {
            let Some(module) = self.renderer.module_registry().module(specifier) else {
                tracing::warn!(
                    target: "albedo.renderer",
                    key = %key,
                    specifier = %specifier,
                    "tier-b dependency module missing; render plan skipped"
                );
                return;
            };
            modules.push((specifier.clone(), module.code.clone()));
        }

        plan.insert(
            key.to_string(),
            crate::render::tier_b::TierBEntryPlan { entry, modules },
        );
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
