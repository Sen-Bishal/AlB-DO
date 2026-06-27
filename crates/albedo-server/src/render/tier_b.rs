use async_trait::async_trait;
use dom_render_compiler::ir::opcode::{Instruction, StableId};
use dom_render_compiler::manifest::schema::{DataDep, DataSource, TierBNode};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("dynamic prop '{key}' is missing from request context")]
    MissingDynamicProp { key: String },
    #[error("failed to merge dynamic prop '{key}': static props must be a JSON object")]
    StaticPropsNotObject { key: String },
    #[error("render registry failed for '{render_fn}': {message}")]
    RegistryFailure { render_fn: String, message: String },
    #[error("data fetch failed for '{key}': {message}")]
    DataFetchFailure { key: String, message: String },
}

#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub path: String,
    pub params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub cookies: HashMap<String, String>,
}

impl RequestContext {
    pub fn resolve(&self, key: &str) -> Result<Value, RenderError> {
        if let Some(value) = self.params.get(key) {
            return Ok(Value::String(value.clone()));
        }

        if key == "path" {
            return Ok(Value::String(self.path.clone()));
        }

        if let Some(header) = key.strip_prefix("header:") {
            if let Some(value) = self.headers.get(header) {
                return Ok(Value::String(value.clone()));
            }
        }

        if let Some(cookie) = key.strip_prefix("cookie:") {
            if let Some(value) = self.cookies.get(cookie) {
                return Ok(Value::String(value.clone()));
            }
        }

        Err(RenderError::MissingDynamicProp {
            key: key.to_string(),
        })
    }
}

#[async_trait]
pub trait TierBRenderRegistry: Send + Sync {
    async fn call(
        &self,
        render_fn: &str,
        props: &Value,
        data: &HashMap<String, Value>,
    ) -> Result<String, RenderError>;

    /// Gate 2 · B slice 3 — evaluate a route's `generateMetadata(props)` export
    /// to its raw metadata object (the Next.js `Metadata` shape). `key` is the
    /// boot-plan key the route's metadata module was registered under. Returns
    /// `Ok(None)` when the route declares no `generateMetadata` (the default for
    /// registries without a real engine pool, so non-pooled paths are unchanged).
    async fn call_metadata(
        &self,
        _key: &str,
        _props: &Value,
    ) -> Result<Option<Value>, RenderError> {
        Ok(None)
    }
}

/// Phase-E opcode-shaped Tier-B render registry.
///
/// Replaces [`TierBRenderRegistry`]'s `String` output with an opcode
/// instruction vector destined for the bakabox VM via the patches stream.
/// Userland renderers implement this when they want to ship Tier-B
/// islands through the binary WT path instead of HTML chunks.
///
/// `placeholder_stable_id` is the bakabox-side anchor the
/// `Placeholder` opcode created. Resolved opcodes that want to render
/// inside the placeholder typically emit `Append { parent_id:
/// placeholder_stable_id, child_id: <fresh> }`; resolvers that want
/// to replace the placeholder altogether emit a `Remove` followed by
/// fresh creates against a different parent.
#[async_trait]
pub trait TierBOpcodeRegistry: Send + Sync {
    async fn call(
        &self,
        render_fn: &str,
        placeholder_stable_id: StableId,
        props: &Value,
        data: &HashMap<String, Value>,
    ) -> Result<Vec<Instruction>, RenderError>;
}

/// Deterministic FNV-1a 32-bit hash of a placeholder id string. Used
/// to derive a stable bakabox `StableId` from the manifest's string
/// `placeholder_id` so the server-side `Placeholder` opcode and any
/// client-side anchor (shell-rendered `data-albedo-id` attributes once
/// the renderer stamps them) align without a per-route id table.
///
/// FNV-1a-32 collides with negligible probability across realistic
/// placeholder-id corpuses and is reproducible across rebuilds; we do
/// not need the cryptographic guarantees of a wider hash here.
#[must_use]
pub fn stable_id_for_placeholder(placeholder_id: &str) -> StableId {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET;
    for byte in placeholder_id.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    StableId(hash)
}

#[async_trait]
pub trait TierBDataFetcher: Send + Sync {
    async fn fetch(
        &self,
        dep: &DataDep,
        ctx: &RequestContext,
    ) -> Result<(String, Value), RenderError>;
}

pub struct DefaultTierBDataFetcher;

#[async_trait]
impl TierBDataFetcher for DefaultTierBDataFetcher {
    async fn fetch(
        &self,
        dep: &DataDep,
        ctx: &RequestContext,
    ) -> Result<(String, Value), RenderError> {
        let value = match &dep.source {
            DataSource::RequestContext { key } => ctx.resolve(key)?,
            DataSource::Cache {
                cache_key_template,
                ttl_s,
            } => serde_json::json!({
                "cache_key": cache_key_template,
                "ttl_s": ttl_s,
                "hit": false
            }),
            DataSource::HttpFetch {
                url_template,
                method,
            } => serde_json::json!({
                "url": url_template,
                "method": method,
                "status": "not_fetched_in_default_fetcher"
            }),
            DataSource::DbQuery { query, param_keys } => serde_json::json!({
                "query": query,
                "param_keys": param_keys,
                "rows": []
            }),
        };

        Ok((dep.key.clone(), value))
    }
}

pub async fn render_tier_b(
    node: &TierBNode,
    ctx: &RequestContext,
    render_registry: &(dyn TierBRenderRegistry + Send + Sync),
    data_fetcher: &(dyn TierBDataFetcher + Send + Sync),
) -> Result<String, RenderError> {
    let mut props = node.static_props.clone();
    let props_obj = props
        .as_object_mut()
        .ok_or_else(|| RenderError::StaticPropsNotObject {
            key: "static_props".to_string(),
        })?;

    for key in &node.dynamic_prop_keys {
        let value = ctx.resolve(key)?;
        props_obj.insert(key.clone(), value);
    }

    let mut fetches = node
        .data_deps
        .iter()
        .cloned()
        .map(|dep| {
            let ctx = ctx.clone();
            async move { data_fetcher.fetch(&dep, &ctx).await }
        })
        .collect::<FuturesUnordered<_>>();

    let mut data = HashMap::new();
    while let Some(result) = fetches.next().await {
        let (key, value) = result?;
        data.insert(key, value);
    }

    let component_html = render_registry
        .call(node.render_fn.as_str(), &props, &data)
        .await
        .map_err(|err| RenderError::RegistryFailure {
            render_fn: node.render_fn.clone(),
            message: err.to_string(),
        })?;

    let mut full_html = component_html;
    for child in &node.tier_a_children {
        full_html = full_html.replace(
            &format!("<!--__SLOT_{}-->", child.placeholder_id),
            &child.html,
        );
    }

    Ok(full_html)
}

/// Phase-E: opcode-shaped counterpart to [`render_tier_b`].
///
/// Resolves dynamic props from the request context, fans out `data_deps`
/// fetches in parallel via the existing [`TierBDataFetcher`] surface,
/// and hands the merged `(props, data)` to the opcode registry. The
/// returned `Vec<Instruction>` is the body of a Phase-D async-island
/// `Patch`: the pipeline ships it after the `Patch` opcode in the same
/// `OpcodeFrame`.
///
/// Errors surface as [`RenderError`]; callers that want to keep the
/// async-island slot intact on failure should map the error into an
/// empty Vec or a fallback opcode stream.
pub async fn render_tier_b_opcodes(
    node: &TierBNode,
    ctx: &RequestContext,
    opcode_registry: &(dyn TierBOpcodeRegistry + Send + Sync),
    data_fetcher: &(dyn TierBDataFetcher + Send + Sync),
) -> Result<Vec<Instruction>, RenderError> {
    let mut props = node.static_props.clone();
    let props_obj = props
        .as_object_mut()
        .ok_or_else(|| RenderError::StaticPropsNotObject {
            key: "static_props".to_string(),
        })?;

    for key in &node.dynamic_prop_keys {
        let value = ctx.resolve(key)?;
        props_obj.insert(key.clone(), value);
    }

    let mut fetches = node
        .data_deps
        .iter()
        .cloned()
        .map(|dep| {
            let ctx = ctx.clone();
            async move { data_fetcher.fetch(&dep, &ctx).await }
        })
        .collect::<FuturesUnordered<_>>();

    let mut data = HashMap::new();
    while let Some(result) = fetches.next().await {
        let (key, value) = result?;
        data.insert(key, value);
    }

    let placeholder_stable_id = stable_id_for_placeholder(&node.placeholder_id);

    opcode_registry
        .call(
            node.render_fn.as_str(),
            placeholder_stable_id,
            &props,
            &data,
        )
        .await
        .map_err(|err| RenderError::RegistryFailure {
            render_fn: node.render_fn.clone(),
            message: err.to_string(),
        })
}

pub struct InjectionChunk {
    placeholder_id: String,
    kind: ChunkKind,
}

enum ChunkKind {
    Success { html: String },
    Fallback { html: String },
    /// A route `error.tsx` boundary rendered to real HTML — the placeholder
    /// is replaced by the fallback UI and marked `'error'` so the client can
    /// style it, instead of being left a blank `data-albedo-error` stub.
    ErrorBoundary { html: String },
    Error,
}

impl InjectionChunk {
    pub fn success(node: &TierBNode, html: String) -> Self {
        Self {
            placeholder_id: node.placeholder_id.clone(),
            kind: ChunkKind::Success { html },
        }
    }

    pub fn fallback(node: &TierBNode) -> Self {
        let fallback = node
            .fallback_html
            .clone()
            .unwrap_or_else(|| "<div data-albedo-fallback=\"timeout\"></div>".to_string());
        Self {
            placeholder_id: node.placeholder_id.clone(),
            kind: ChunkKind::Fallback { html: fallback },
        }
    }

    /// Timeout fallback backed by a route `loading.tsx` boundary's rendered
    /// HTML instead of the generic timeout placeholder div.
    pub fn fallback_with_html(node: &TierBNode, html: String) -> Self {
        Self {
            placeholder_id: node.placeholder_id.clone(),
            kind: ChunkKind::Fallback { html },
        }
    }

    /// A throwing Tier-B/async component whose route declares an `error.tsx`:
    /// inject the rendered boundary HTML rather than a blank error stub.
    pub fn error_boundary(node: &TierBNode, html: String) -> Self {
        Self {
            placeholder_id: node.placeholder_id.clone(),
            kind: ChunkKind::ErrorBoundary { html },
        }
    }

    pub fn error(node: &TierBNode, _error: RenderError) -> Self {
        Self {
            placeholder_id: node.placeholder_id.clone(),
            kind: ChunkKind::Error,
        }
    }

    pub fn into_script_tag(self) -> String {
        let id = serde_json::to_string(&self.placeholder_id).unwrap_or_else(|_| "\"\"".to_string());
        match self.kind {
            ChunkKind::Success { html } => {
                let html = serde_json::to_string(&html).unwrap_or_else(|_| "\"\"".to_string());
                format!("<script>__albedo_inject({id},{html})</script>")
            }
            ChunkKind::Fallback { html } => {
                let html = serde_json::to_string(&html).unwrap_or_else(|_| "\"\"".to_string());
                format!("<script>__albedo_inject({id},{html},'fallback')</script>")
            }
            ChunkKind::ErrorBoundary { html } => {
                let html = serde_json::to_string(&html).unwrap_or_else(|_| "\"\"".to_string());
                format!("<script>__albedo_inject({id},{html},'error')</script>")
            }
            ChunkKind::Error => format!("<script>__albedo_inject({id},null,'error')</script>"),
        }
    }
}

/// Self-contained load+render plan for one Tier-B component, precomputed at
/// boot while the (`!Send`) renderer is still single-threaded. Owns everything
/// a pool engine needs to render the component on the request path: the entry
/// module spec and the full topologically-ordered list of `(specifier, code)`
/// to register first (component module bodies link their imports *eagerly* at
/// load via `__albedo_require`, so dependencies must be loaded before the
/// module that imports them).
#[derive(Debug, Clone)]
pub struct TierBEntryPlan {
    /// Module spec passed to `__ALBEDO_RENDER_COMPONENT` (the component's
    /// `module_path`; its default export is the render function).
    pub entry: String,
    /// `(specifier, code)` pairs in dependency-first load order. `load_module`
    /// is idempotent by source hash, so re-loading on every checkout is a cheap
    /// hash-compare after an engine has seen the module once.
    pub modules: Vec<(String, String)>,
}

/// Map from a `TierBNode.render_fn` (e.g. `"render::Stats"`) to its boot-built
/// [`TierBEntryPlan`]. Built once by the renderer and handed to
/// [`PooledTierBRenderRegistry`].
pub type TierBRenderPlan = HashMap<String, TierBEntryPlan>;

/// Production Tier-B render registry: resolves async/server Tier-B components to
/// real HTML by rendering them through the warmed QuickJS [`engine pool`], the
/// same warmed/concurrent/arena engines that execute `action()` calls.
///
/// Replaces [`StubTierBRenderRegistry`] (which returned an empty `<section>`,
/// so every Tier-B node — async server components AND legit interactive Tier-B —
/// rendered nothing on `albedo serve`). Each `call` checks out an engine, loads
/// the component's module graph (idempotent after the first checkout), and runs
/// `render_component_with_host`, whose `MaybePromise::finish` drives the QuickJS
/// job queue so an `async function Page()` is awaited on the server before its
/// HTML is lowered.
///
/// [`engine pool`]: crate::engine_pool::QuickJsEnginePool
pub struct PooledTierBRenderRegistry {
    pool: Arc<crate::engine_pool::QuickJsEnginePool>,
    plan: TierBRenderPlan,
}

impl PooledTierBRenderRegistry {
    #[must_use]
    pub fn new(pool: Arc<crate::engine_pool::QuickJsEnginePool>, plan: TierBRenderPlan) -> Self {
        Self { pool, plan }
    }
}

#[async_trait]
impl TierBRenderRegistry for PooledTierBRenderRegistry {
    async fn call(
        &self,
        render_fn: &str,
        props: &Value,
        _data: &HashMap<String, Value>,
    ) -> Result<String, RenderError> {
        // A component with no boot-built plan can't be rendered on the request
        // path — surface it loudly instead of silently injecting nothing (the
        // exact silent-empty failure this registry exists to kill).
        let plan =
            self.plan
                .get(render_fn)
                .cloned()
                .ok_or_else(|| {
                    RenderError::RegistryFailure {
                render_fn: render_fn.to_string(),
                message:
                    "no Tier-B render plan registered at boot (component not in manifest routes?)"
                        .to_string(),
            }
                })?;

        let props_json = serde_json::to_string(props).unwrap_or_else(|_| "{}".to_string());
        let render_fn_owned = render_fn.to_string();

        // The closure crosses to the engine's dedicated thread, so every capture
        // and the return type must be `Send + 'static`. Return a plain
        // `Result<String, String>` rather than the engine's `RuntimeError` to
        // keep the boundary free of engine-internal types.
        let rendered = self
            .pool
            .with_engine(move |engine| -> Result<String, String> {
                use dom_render_compiler::runtime::engine::RuntimeEngine;
                for (specifier, code) in &plan.modules {
                    engine
                        .load_module(specifier, code)
                        .map_err(|err| err.to_string())?;
                }
                engine
                    .render_component_with_host(&plan.entry, &props_json, "{}")
                    .map(|output| output.html)
                    .map_err(|err| err.to_string())
            })
            .await
            .map_err(|err| RenderError::RegistryFailure {
                render_fn: render_fn_owned.clone(),
                message: err.to_string(),
            })?;

        rendered.map_err(|message| RenderError::RegistryFailure {
            render_fn: render_fn_owned,
            message,
        })
    }

    async fn call_metadata(
        &self,
        key: &str,
        props: &Value,
    ) -> Result<Option<Value>, RenderError> {
        // Same boot-plan + pooled-engine path as `call`, but the engine
        // evaluates `generateMetadata` to a DATA object rather than rendering
        // HTML. A route without a registered plan can't be dynamic — treat it as
        // "no dynamic metadata" (the static `<head>` stands) rather than failing
        // the whole request over a head detail.
        let Some(plan) = self.plan.get(key).cloned() else {
            return Ok(None);
        };

        let props_json = serde_json::to_string(props).unwrap_or_else(|_| "{}".to_string());
        let key_owned = key.to_string();

        let resolved = self
            .pool
            .with_engine(move |engine| -> Result<Option<Value>, String> {
                use dom_render_compiler::runtime::engine::RuntimeEngine;
                for (specifier, code) in &plan.modules {
                    engine
                        .load_module(specifier, code)
                        .map_err(|err| err.to_string())?;
                }
                engine
                    .eval_route_metadata(&plan.entry, &props_json)
                    .map_err(|err| err.to_string())
            })
            .await
            .map_err(|err| RenderError::RegistryFailure {
                render_fn: key_owned.clone(),
                message: err.to_string(),
            })?;

        resolved.map_err(|message| RenderError::RegistryFailure {
            render_fn: key_owned,
            message,
        })
    }
}

pub struct StubTierBRenderRegistry;

#[async_trait]
impl TierBRenderRegistry for StubTierBRenderRegistry {
    async fn call(
        &self,
        render_fn: &str,
        props: &Value,
        data: &HashMap<String, Value>,
    ) -> Result<String, RenderError> {
        let props_json = serde_json::to_string(props).unwrap_or_else(|_| "{}".to_string());
        let data_json = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
        Ok(format!(
            "<section data-render-fn=\"{}\" data-props='{}' data-data='{}'></section>",
            render_fn, props_json, data_json
        ))
    }
}

/// Phase-E stub opcode registry. Used by `SharedRenderServices::default()`
/// and by tests; returns an empty instruction vector. Real renderers
/// implement [`TierBOpcodeRegistry`] to emit opcodes that target the
/// placeholder element via its server-assigned `StableId`.
pub struct StubTierBOpcodeRegistry;

#[async_trait]
impl TierBOpcodeRegistry for StubTierBOpcodeRegistry {
    async fn call(
        &self,
        _render_fn: &str,
        _placeholder_stable_id: StableId,
        _props: &Value,
        _data: &HashMap<String, Value>,
    ) -> Result<Vec<Instruction>, RenderError> {
        Ok(Vec::new())
    }
}

#[derive(Clone)]
pub struct SharedRenderServices {
    pub registry: Arc<dyn TierBRenderRegistry>,
    pub data_fetcher: Arc<dyn TierBDataFetcher>,
    /// Phase-E opcode registry. When `Some`, the WT streaming path
    /// resolves Tier-B nodes through this and the pipeline's
    /// async-island machinery. When `None`, the WT path falls back to
    /// the legacy JSON+HTML envelope shipped through `__albedo_inject`.
    pub opcode_registry: Option<Arc<dyn TierBOpcodeRegistry>>,
}

impl Default for SharedRenderServices {
    fn default() -> Self {
        Self {
            registry: Arc::new(StubTierBRenderRegistry),
            data_fetcher: Arc::new(DefaultTierBDataFetcher),
            opcode_registry: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::manifest::schema::{DomPosition, RenderedNode, TierBNode};
    use serde_json::json;

    struct TestRegistry;

    #[async_trait]
    impl TierBRenderRegistry for TestRegistry {
        async fn call(
            &self,
            _render_fn: &str,
            _props: &Value,
            _data: &HashMap<String, Value>,
        ) -> Result<String, RenderError> {
            Ok("<article><!--__SLOT___a_leaf--></article>".to_string())
        }
    }

    struct TestFetcher;

    #[async_trait]
    impl TierBDataFetcher for TestFetcher {
        async fn fetch(
            &self,
            dep: &DataDep,
            _ctx: &RequestContext,
        ) -> Result<(String, Value), RenderError> {
            Ok((dep.key.clone(), json!("ok")))
        }
    }

    fn node() -> TierBNode {
        TierBNode {
            component_id: "Feature".to_string(),
            placeholder_id: "__b_feature".to_string(),
            render_fn: "render::Feature".to_string(),
            static_props: json!({"title":"x"}),
            dynamic_prop_keys: vec!["path".to_string()],
            data_deps: vec![DataDep {
                key: "payload".to_string(),
                source: DataSource::RequestContext {
                    key: "path".to_string(),
                },
            }],
            tier_a_children: vec![RenderedNode {
                component_id: "Leaf".to_string(),
                placeholder_id: "__a_leaf".to_string(),
                html: "<p>leaf</p>".to_string(),
                position: DomPosition {
                    parent_placeholder: Some("__b_feature".to_string()),
                    slot: "default".to_string(),
                    order: 0,
                },
            }],
            position: DomPosition {
                parent_placeholder: None,
                slot: "default".to_string(),
                order: 0,
            },
            timeout_ms: 100,
            fallback_html: Some("<p>fallback</p>".to_string()),
            initial_html: None,
            initial_opcode_frame: Vec::new(),
        }
    }

    #[tokio::test]
    async fn test_render_tier_b_inlines_tier_a_children() {
        let node = node();
        let ctx = RequestContext {
            path: "/home".to_string(),
            ..RequestContext::default()
        };
        let html = render_tier_b(&node, &ctx, &TestRegistry, &TestFetcher)
            .await
            .expect("tier b should render");
        assert_eq!(html, "<article><p>leaf</p></article>");
    }

    #[test]
    fn test_injection_chunk_formats_script() {
        let script = InjectionChunk::fallback(&node()).into_script_tag();
        assert!(script.contains("__albedo_inject"));
        assert!(script.contains("fallback"));
    }

    #[test]
    fn error_boundary_chunk_injects_real_html_not_null() {
        // The bug this closes: a throwing Tier-B node used to ship
        // `__albedo_inject(id, null, 'error')` → a blank placeholder. With a
        // route `error.tsx`, it must ship the rendered boundary HTML so the
        // client replaces the placeholder with fallback UI.
        let script = InjectionChunk::error_boundary(&node(), "<p>boom</p>".to_string())
            .into_script_tag();
        assert!(script.contains("__albedo_inject"));
        assert!(script.contains("<p>boom</p>"), "must carry the boundary HTML");
        assert!(script.contains("'error'"), "must keep the error status marker");
        assert!(
            !script.contains("null"),
            "the regression: error boundary must not inject null"
        );
    }

    #[test]
    fn fallback_with_html_uses_loading_boundary_markup() {
        let script = InjectionChunk::fallback_with_html(&node(), "<p>loading…</p>".to_string())
            .into_script_tag();
        assert!(script.contains("<p>loading…</p>"));
        assert!(script.contains("'fallback'"));
    }

    // ── Phase E — opcode renderer tests ───────────────────────────────

    use dom_render_compiler::ir::opcode::{Instruction, StableId, TagId};

    /// Opcode-shaped registry stub. Captures the placeholder StableId
    /// passed by `render_tier_b_opcodes` so the test can assert the
    /// renderer wiring forwards it correctly. Returns a fixed two-op
    /// instruction sequence anchored to the placeholder.
    struct TestOpcodeRegistry {
        seen_placeholder: std::sync::Mutex<Option<StableId>>,
    }

    impl TestOpcodeRegistry {
        fn new() -> Self {
            Self {
                seen_placeholder: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl TierBOpcodeRegistry for TestOpcodeRegistry {
        async fn call(
            &self,
            _render_fn: &str,
            placeholder_stable_id: StableId,
            _props: &Value,
            _data: &HashMap<String, Value>,
        ) -> Result<Vec<Instruction>, RenderError> {
            *self.seen_placeholder.lock().unwrap() = Some(placeholder_stable_id);
            Ok(vec![
                Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(9_999),
                },
                Instruction::Append {
                    parent_id: placeholder_stable_id,
                    child_id: StableId(9_999),
                },
            ])
        }
    }

    #[tokio::test]
    async fn render_tier_b_opcodes_forwards_placeholder_stable_id() {
        let node = node();
        let ctx = RequestContext {
            path: "/home".to_string(),
            ..RequestContext::default()
        };
        let registry = TestOpcodeRegistry::new();

        let opcodes = render_tier_b_opcodes(&node, &ctx, &registry, &TestFetcher)
            .await
            .expect("opcode render must succeed");

        let expected_id = stable_id_for_placeholder(&node.placeholder_id);
        assert_eq!(
            *registry.seen_placeholder.lock().unwrap(),
            Some(expected_id),
            "registry must receive the FNV-hashed placeholder id"
        );
        assert_eq!(opcodes.len(), 2);
        assert!(matches!(
            opcodes[1],
            Instruction::Append { parent_id, .. } if parent_id == expected_id
        ));
    }

    #[test]
    fn stable_id_for_placeholder_is_deterministic_and_collision_resistant() {
        let a = stable_id_for_placeholder("__b_feature");
        let b = stable_id_for_placeholder("__b_feature");
        let c = stable_id_for_placeholder("__b_other");
        assert_eq!(a, b, "same input must produce same id across calls");
        assert_ne!(a, c, "different inputs should not collide on this corpus");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pooled_registry_surfaces_unregistered_component_loudly() {
        // The whole point of this registry is to kill silent-empty Tier-B
        // renders: a component with no boot-built plan must produce a loud
        // `RegistryFailure`, never an empty success.
        let pool = Arc::new(crate::engine_pool::QuickJsEnginePool::with_size(1));
        let registry = PooledTierBRenderRegistry::new(pool, TierBRenderPlan::new());

        let err = registry
            .call("render::Missing", &json!({}), &HashMap::new())
            .await
            .expect_err("an unregistered component must fail loudly, not render empty");

        match err {
            RenderError::RegistryFailure { render_fn, message } => {
                assert_eq!(render_fn, "render::Missing");
                assert!(
                    message.contains("no Tier-B render plan"),
                    "unexpected message: {message}"
                );
            }
            other => panic!("expected RegistryFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stub_opcode_registry_returns_empty_instruction_vector() {
        let registry = StubTierBOpcodeRegistry;
        let out = registry
            .call(
                "render::Whatever",
                StableId(42),
                &json!({}),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert!(
            out.is_empty(),
            "stub registry must produce no opcodes; real renderers replace it"
        );
    }
}
