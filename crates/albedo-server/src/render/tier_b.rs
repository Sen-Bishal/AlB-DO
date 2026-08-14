use async_trait::async_trait;
use dom_render_compiler::auth::PrincipalId;
use dom_render_compiler::ir::opcode::{Instruction, StableId};
use dom_render_compiler::manifest::schema::{
    DataDep, DataSource, PartitionTopicSpec, SourceTopicSpec, TierBNode,
};
use dom_render_compiler::transforms::shared_slot_lists::RowProjection;
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
    /// A registered render fn failed. `diagnostic` is the full developer
    /// chain (component path, engine wrapping) shown in logs / the dev
    /// overlay via `Display`; `thrown_message` is the raw text the component
    /// threw, which is all a reader-facing `error.tsx` boundary should see
    /// (never the path or the wrapping). Keep them distinct so the boundary
    /// reads structured data instead of parsing the diagnostic string.
    #[error("render registry failed for '{render_fn}': {diagnostic}")]
    RegistryFailure {
        render_fn: String,
        thrown_message: String,
        diagnostic: String,
    },
    #[error("data fetch failed for '{key}': {message}")]
    DataFetchFailure { key: String, message: String },
}

impl RenderError {
    /// The reader-facing message for an `error.tsx` boundary: the original
    /// thrown text only — never the wrapped diagnostic chain or a filesystem
    /// path. Logs and the dev overlay use `Display` (the full chain) instead.
    pub fn user_message(&self) -> String {
        match self {
            RenderError::RegistryFailure { thrown_message, .. } => thrown_message.clone(),
            RenderError::DataFetchFailure { message, .. } => message.clone(),
            RenderError::MissingDynamicProp { .. } | RenderError::StaticPropsNotObject { .. } => {
                self.to_string()
            }
        }
    }
}

/// Plain `Send` carrier for a component render failure crossing the engine's
/// dedicated thread. The closure handed to `with_engine` must be
/// `Send + 'static`, so the engine's `RuntimeError` cannot cross — but a flat
/// `String` would lose the thrown/diagnostic distinction. This struct keeps
/// both as data: `thrown_message` (reader-facing) and `diagnostic` (logs).
#[derive(Debug, Clone)]
struct ComponentRenderFailure {
    thrown_message: String,
    diagnostic: String,
}

#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub path: String,
    pub params: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub cookies: HashMap<String, String>,
    /// AUTH item 5 · the request's authenticated principal, or `None` for
    /// anonymous.
    ///
    /// It sits beside `params` because it answers the same kind of question —
    /// *what does this request know?* — and because both feed the same resolver.
    /// `cookies` already carries the raw session cookie; this is what the
    /// dispatcher turned it into, and components and topic resolution must never
    /// re-derive identity from the cookie themselves.
    pub principal: Option<PrincipalId>,
}

impl RequestContext {
    pub fn resolve(&self, key: &str) -> Result<Value, RenderError> {
        // AUTH § 3 · `user` in component scope. An anonymous request resolves to
        // `null` rather than to an absent prop, so `user?.id` is the honest
        // spelling and a component cannot accidentally read a stale binding.
        //
        // Only `id` is exposed here. Profile fields live on `albedo_users` and
        // are read like any other row; putting them in scope would make every
        // component that mentions `user` a reason to join a table it never asked
        // for, and would tempt a partition key that cannot be one.
        if key == "user" {
            return Ok(match &self.principal {
                Some(id) => serde_json::json!({ "id": id.as_str() }),
                None => Value::Null,
            });
        }

        // Dynamic `[slug]` routes request the whole parsed route-params map as a
        // single `params` object prop. Assemble it here so a component authored
        // as `async function Page({ params })` receives `{ slug: "..." }`.
        if key == "params" {
            let map = self
                .params
                .iter()
                .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                .collect::<serde_json::Map<String, Value>>();
            return Ok(Value::Object(map));
        }

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

    // The registry already returns a typed `RenderError::RegistryFailure`
    // carrying this `render_fn` and the thrown message — propagate it as-is
    // rather than wrapping it in a *second* `RegistryFailure`, which is what
    // produced the doubled "render registry failed for '…'" prefix readers
    // used to see through the error boundary.
    let component_html = render_registry
        .call(node.render_fn.as_str(), &props, &data)
        .await?;

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
        .map_err(|err| {
            let diagnostic = err.to_string();
            RenderError::RegistryFailure {
                render_fn: node.render_fn.clone(),
                thrown_message: diagnostic.clone(),
                diagnostic,
            }
        })
}

#[derive(Clone)]
pub struct InjectionChunk {
    placeholder_id: String,
    kind: ChunkKind,
}

#[derive(Clone)]
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

    /// The markup this chunk becomes when it is painted into the served
    /// document instead of injected by a script.
    ///
    /// 🔑 **This is the `outerHTML` semantics of `__albedo_inject`, in Rust.**
    /// The client's injector does `el.outerHTML = html`, which *replaces* the
    /// placeholder rather than filling it, so the painted form must replace it
    /// too — otherwise a page rendered without JavaScript and the same page
    /// after injection would differ by a wrapper element, and every selector,
    /// stylesheet and delta anchor that resolved against one would be reasoning
    /// about the other. The two paths converge on identical DOM by construction.
    ///
    /// The error arm mirrors the injector's other branch: it keeps the
    /// placeholder and marks it, because there is no markup to put there.
    #[must_use]
    pub fn into_painted_markup(self) -> String {
        // Written raw, exactly as `seed_tier_b_placeholders` writes the same
        // element — the id is a compiler-generated placeholder name, and two
        // spellings of one element is how the CSRF input came to disagree with
        // itself. Keeping them byte-identical is what lets the seeder's matcher
        // and this painter never drift.
        let id = &self.placeholder_id;
        match self.kind {
            ChunkKind::Success { html }
            | ChunkKind::Fallback { html }
            | ChunkKind::ErrorBoundary { html } => html,
            ChunkKind::Error => {
                format!("<div id=\"{id}\" data-albedo-tier=\"b\" data-albedo-error=\"error\"></div>")
            }
        }
    }

    /// The placeholder this chunk targets.
    #[must_use]
    pub fn placeholder_id(&self) -> &str {
        &self.placeholder_id
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
    /// Broadcast topics this component reads via `useSharedSlot`, resolved at
    /// boot from the compiled project (the manifest doesn't carry them).
    ///
    /// The request path seeds each topic's *current* value into the render's
    /// host object; the topic list is fixed at compile time, but the values are
    /// not, so only the list can be precomputed here. Empty for the vast
    /// majority of components, which read no shared slots at all.
    pub shared_topics: Vec<String>,
    /// PRISM · the *partitioned* shared-slot bindings this component reads,
    /// still unresolved.
    ///
    /// The counterpart to `shared_topics` for a topic whose identity does not
    /// exist until a request supplies its key. Boot can precompute the spec and
    /// nothing else — not the topic string, not the value, not even whether the
    /// binding will resolve at all, since that depends on a URL nobody has
    /// requested yet.
    pub shared_partitions: Vec<PartitionTopicSpec>,
    /// APERTURE · the declared-source bindings this component reads, still
    /// unresolved. The counterpart to `shared_partitions` for a topic whose
    /// derivation is an HTTP GET.
    pub shared_sources: Vec<SourceTopicSpec>,
    /// Per-topic row-template incrementalisation class ([`RowProjection`]),
    /// derived at boot from the `.map()` callbacks in this entry's modules.
    /// Lets FORGE's row projector answer a single-record write on a `PerRecord`
    /// collection by rendering one row instead of the whole view. A topic absent
    /// from this map is treated as [`RowProjection::WholeView`] — the
    /// always-correct whole-view render.
    pub shared_topic_classes: HashMap<String, RowProjection>,
    /// Absolute module specifier → the **project-relative** spec its rendered
    /// `data-albedo-id` anchors are keyed to.
    ///
    /// Two strings for one module because they answer different questions. The
    /// specifier is what imports resolve against, so it is the absolute path the
    /// manifest carries. An anchor id is `fnv1a_32("{spec}#{n}")` and has to hash
    /// the string the *pure-Rust* renderer hashed when it built this component's
    /// opcode frame — project-relative, and deliberately not machine-specific.
    /// Feed the engine the wrong one and every `BindEvent` in the frame names an
    /// element that does not exist.
    ///
    /// A module with no entry here is stamped with nothing, which is the correct
    /// degradation: no anchors is what shipped before, and a *wrong* anchor would
    /// be worse than none.
    pub stamp_specs: HashMap<String, String>,
}

/// Map from a `TierBNode.render_fn` (e.g. `"render::Stats"`) to its boot-built
/// [`TierBEntryPlan`]. Built once by the renderer and handed to
/// [`PooledTierBRenderRegistry`].
pub type TierBRenderPlan = HashMap<String, TierBEntryPlan>;

/// Server-context module body for a Tier-C island reached as a dependency of a
/// server-rendered (Tier-B/async) component — a *client reference* in the
/// React-Server-Components sense.
///
/// The island's real code never runs in the pool engines (the server graph);
/// its module is swapped for this stub, whose default export renders only the
/// framework's canonical empty island placeholder (`__albedo_island_placeholder`
/// in the QuickJS prelude). The serve-time island fill pass then replaces that
/// placeholder with the island's standalone SSR markup + `data-albedo-island`
/// marker — the identical treatment a Tier-A parent's island child receives, so
/// both renderers converge on one island representation.
///
/// `placeholder_id` is the island node's manifest `placeholder_id`
/// (`__c_<slug>_<id>`), which contains no markup-significant characters, so the
/// `{:?}` JS-string encoding is exact.
#[must_use]
pub fn island_client_reference_stub(placeholder_id: &str) -> String {
    format!(
        "export default (function __albedoIslandRef(props) {{ \
return globalThis.__albedo_island_placeholder({placeholder_id:?}); }});"
    )
}

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
    /// The server's live broadcast registry — the same `Arc` the action
    /// handlers, the WT/SSE runtime and FORGE's boot hydration write through.
    ///
    /// Without it this path had no way to resolve `useSharedSlot`, and passed
    /// an empty host seed instead: the component saw `null`, threw, and the
    /// island never entered the DOM. It must be the live registry, not a
    /// snapshot — a topic's value changes under the server, and each request
    /// must render whatever is current *at that request*.
    broadcast: Arc<dom_render_compiler::runtime::BroadcastRegistry>,
    /// P6 · per-action `data-albedo-error` span markup, keyed by action name.
    /// Computed once at boot by [`dom_render_compiler::runtime::eval::CompiledProject::form_error_span_seed`]
    /// — the SAME generator the non-pooled render path uses — so a Tier-B
    /// form emits the exact error sinks the submit projection targets and
    /// bakabox stops dropping the frame. Empty when the project has no forms.
    form_error_spans: serde_json::Map<String, Value>,
    /// PRISM · read-through materialisation for partitioned topics.
    ///
    /// A partition has no value in the registry until something asks for it, and
    /// a render is usually the first thing to ask. Without this the first paint
    /// of `/room/42` would show an empty room and only fill in once somebody
    /// wrote to it — the page would look *wrong*, not merely slow.
    warmer: Arc<dyn crate::topics::TopicWarmer>,
    /// APERTURE · the declared `sources` block, or `None` when the app declared
    /// none. Needed here because resolving a source spec means walking the
    /// route's path template, which only the registry knows.
    source_registry: Option<Arc<dom_render_compiler::aperture::SourceRegistry>>,
}

impl PooledTierBRenderRegistry {
    #[must_use]
    pub fn new(
        pool: Arc<crate::engine_pool::QuickJsEnginePool>,
        plan: TierBRenderPlan,
        broadcast: Arc<dom_render_compiler::runtime::BroadcastRegistry>,
        form_error_spans: serde_json::Map<String, Value>,
        warmer: Arc<dyn crate::topics::TopicWarmer>,
    ) -> Self {
        Self {
            pool,
            plan,
            broadcast,
            form_error_spans,
            warmer,
            source_registry: None,
        }
    }

    /// Attach the declared sources. Separate from [`Self::new`] so a server with
    /// no `sources` block — every server that exists today — is unchanged.
    #[must_use]
    pub fn with_sources(
        mut self,
        registry: Option<Arc<dom_render_compiler::aperture::SourceRegistry>>,
    ) -> Self {
        self.source_registry = registry;
        self
    }

    /// APERTURE · resolve this component's source bindings against the `params`
    /// the route matched.
    ///
    /// The params come out of `props` for the identical reason
    /// [`resolve_partitions_for_props`] reads them there: a component that can
    /// write `github.repo({ owner: params.org })` necessarily receives `params`,
    /// so binding the topic to those values binds it to exactly what the
    /// component itself sees.
    fn resolve_sources_for_props(
        &self,
        plan: &TierBEntryPlan,
        props: &Value,
    ) -> Vec<dom_render_compiler::runtime::ResolvedSourceTopic> {
        if plan.shared_sources.is_empty() {
            return Vec::new();
        }
        let Some(registry) = self.source_registry.as_ref() else {
            return Vec::new();
        };
        let params = props.get("params").and_then(Value::as_object);
        dom_render_compiler::runtime::resolve_source_topics(
            &plan.shared_sources,
            registry,
            |name| params.and_then(|map| map.get(name)).and_then(Value::as_str),
        )
    }
}

/// PRISM · resolve this component's partition bindings against the `params` the
/// route matched.
///
/// The params arrive inside `props` because that is where the render already
/// puts them: `dynamic_prop_keys` carries `"params"` for any `[slug]` route, and
/// `RequestContext::resolve("params")` assembles the matched map into the object
/// a component destructures. So a component that can *write*
/// `messages.where({ room: params.id })` necessarily receives `params` — the
/// expression would not compile otherwise — and reading them back out here binds
/// the topic to exactly the values the component itself sees.
fn resolve_partitions_for_props(
    plan: &TierBEntryPlan,
    props: &Value,
) -> Vec<dom_render_compiler::runtime::ResolvedPartition> {
    if plan.shared_partitions.is_empty() {
        return Vec::new();
    }
    let params = props.get("params").and_then(Value::as_object);
    // AUTH item 5 P1 · the principal is read back out of `props` for the same
    // reason `params` is, and it is the same argument: the component renders
    // from these props, so binding the topic to them makes "what the page shows"
    // and "what the lane listens on" the same fact rather than two facts that
    // agree by convention.
    //
    // A malformed id yields `None` and therefore no topic — `PrincipalId::parse`
    // enforces the partition-key alphabet, so this is also the point where a
    // principal that could not be a topic name stops being one quietly instead
    // of minting something the registry would refuse later.
    let principal = props
        .get("user")
        .and_then(|user| user.get("id"))
        .and_then(Value::as_str)
        .and_then(|id| PrincipalId::parse(id).ok());
    dom_render_compiler::runtime::resolve_partition_topics(
        &plan.shared_partitions,
        principal.as_ref(),
        |name| params.and_then(|map| map.get(name)).and_then(Value::as_str),
    )
}

/// The `host` object seeding one Tier-B render: the component's shared-slot
/// topics at their **current** values, read on the request.
///
/// `state` is deliberately absent. An omitted hook index tells the JS shim to
/// use that hook's own initial argument, which on a fresh page GET is exactly
/// right — and that is precisely why the old hardcoded empty host hid for so
/// long: it was correct for `useState` and silently wrong for `useSharedSlot`,
/// whose topic value has no initial-argument fallback to degrade to.
fn host_seed_for(
    plan: &TierBEntryPlan,
    partitions: &[dom_render_compiler::runtime::ResolvedPartition],
    sources: &[dom_render_compiler::runtime::ResolvedSourceTopic],
    broadcast: &dom_render_compiler::runtime::BroadcastRegistry,
    form_error_spans: &serde_json::Map<String, Value>,
) -> String {
    // Static topics, resolved partitions and resolved sources are seeded through
    // the same function, from the same registry, in one pass — so a component
    // reading one of each cannot end up with three different notions of
    // "current". This is the payoff for APERTURE reusing the broadcast registry
    // rather than carrying its own store: past this line the render does not
    // know or care which derivation produced a value.
    let shared = dom_render_compiler::runtime::shared_slot_host_seed(
        plan.shared_topics
            .iter()
            .map(String::as_str)
            .chain(partitions.iter().map(|p| p.topic.as_str()))
            .chain(sources.iter().map(|s| s.topic.as_str())),
        broadcast,
    );
    let mut host = serde_json::Map::new();
    if !shared.is_empty() {
        host.insert("shared".to_string(), Value::Object(shared));
    }
    // PRISM · `binding -> topic`, read by the `__albedo_topic("<binding>")` call
    // the transpile folded the `.where(…)` argument into. This is the only place
    // a minted topic string crosses into JS, and it crosses as *data* — nothing
    // in the engine ever builds one, which is what keeps the naming rule in one
    // place (invariant 5) and the client free of a hash path it would otherwise
    // need (§ 3.3).
    //
    // A binding that did not resolve is simply absent, and the shim returns null
    // for it: the slot reads null, the page renders, nothing throws.
    //
    // A source binding lands in the *same* map, under the same rule, because the
    // transpile folded both call shapes into the same `__albedo_topic(binding)`.
    // Binding names are component-local and unique per declarator, so the two
    // derivations cannot collide here.
    if !partitions.is_empty() || !sources.is_empty() {
        let topics = partitions
            .iter()
            .map(|p| (p.binding.clone(), Value::String(p.topic.clone())))
            .chain(
                sources
                    .iter()
                    .map(|s| (s.binding.clone(), Value::String(s.topic.clone()))),
            )
            .collect::<serde_json::Map<String, Value>>();
        host.insert("topics".to_string(), Value::Object(topics));
    }
    // P6 · the error sinks the shim appends to a form-action form. Project-
    // global, so independent of this component's shared topics — a form in a
    // component that reads no slots still needs them, which the old early
    // return on empty `shared` would have dropped.
    if !form_error_spans.is_empty() {
        host.insert(
            "formErrorSpans".to_string(),
            Value::Object(form_error_spans.clone()),
        );
    }
    if host.is_empty() {
        return "{}".to_string();
    }
    serde_json::to_string(&Value::Object(host)).unwrap_or_else(|_| "{}".to_string())
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
                    let reason =
                        "no Tier-B render plan registered at boot (component not in manifest routes?)"
                            .to_string();
                    RenderError::RegistryFailure {
                        render_fn: render_fn.to_string(),
                        thrown_message: reason.clone(),
                        diagnostic: reason,
                    }
                })?;

        let props_json = serde_json::to_string(props).unwrap_or_else(|_| "{}".to_string());
        let render_fn_owned = render_fn.to_string();

        // PRISM · resolve this request's partitions, then materialise them
        // *before* the seed is read. Order matters: `host_seed_for` reads values
        // out of the registry, so a warm that ran after it would fill the cache
        // for the next request and render this one empty.
        let partitions = resolve_partitions_for_props(&plan, props);
        if !partitions.is_empty() {
            self.warmer.warm(&partitions).await;
        }

        // APERTURE · the same order, for the same reason. The refresh window is
        // enforced inside the client, so a fresh resource costs a cache lookup
        // here and a stale one costs a conditional request that most often comes
        // back 304 — which is why warming on every render is affordable rather
        // than something a scheduler has to ration.
        let sources = self.resolve_sources_for_props(&plan, props);
        if !sources.is_empty() {
            self.warmer.warm_sources(&sources).await;
        }

        // Read the topics' values here, on the request, not at boot — the whole
        // point is that they are live.
        let host_json = host_seed_for(
            &plan,
            &partitions,
            &sources,
            self.broadcast.as_ref(),
            &self.form_error_spans,
        );

        // The closure crosses to the engine's dedicated thread, so every capture
        // and the return type must be `Send + 'static`. The engine's
        // `RuntimeError` cannot cross (it would leak engine-internal types onto
        // the boundary), but a flat `String` would discard the thrown/diagnostic
        // split — so we carry both as plain data via `ComponentRenderFailure`.
        let rendered = self
            .pool
            .with_engine(move |engine| -> Result<String, ComponentRenderFailure> {
                use dom_render_compiler::runtime::engine::RuntimeEngine;
                for (specifier, code) in &plan.modules {
                    // With the stamp spec, so this render's markup carries the
                    // same `data-albedo-id`s the component's opcode frame names.
                    // Without it the chunk is unaddressable and every
                    // `BindEvent` in the frame throws.
                    engine
                        .load_module_with_spec(
                            specifier,
                            code,
                            plan.stamp_specs.get(specifier).map(String::as_str),
                        )
                        .map_err(|err| ComponentRenderFailure {
                            thrown_message: err.thrown_message(),
                            diagnostic: err.to_string(),
                        })?;
                }
                engine
                    .render_component_with_host(&plan.entry, &props_json, &host_json)
                    .map(|output| output.html)
                    .map_err(|err| ComponentRenderFailure {
                        thrown_message: err.thrown_message(),
                        diagnostic: err.to_string(),
                    })
            })
            .await
            .map_err(|err| {
                // Engine-pool / join failure — infrastructure, not a component
                // throw; the diagnostic doubles as the (rare) reader text.
                let diagnostic = err.to_string();
                RenderError::RegistryFailure {
                    render_fn: render_fn_owned.clone(),
                    thrown_message: diagnostic.clone(),
                    diagnostic,
                }
            })?;

        rendered.map_err(|failure| RenderError::RegistryFailure {
            render_fn: render_fn_owned,
            thrown_message: failure.thrown_message,
            diagnostic: failure.diagnostic,
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
            .map_err(|err| {
                let diagnostic = err.to_string();
                RenderError::RegistryFailure {
                    render_fn: key_owned.clone(),
                    thrown_message: diagnostic.clone(),
                    diagnostic,
                }
            })?;

        resolved.map_err(|message| RenderError::RegistryFailure {
            render_fn: key_owned,
            thrown_message: message.clone(),
            diagnostic: message,
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

    /// Regression cover for the Tier-B serve gap.
    ///
    /// The bug: this registry passed a hardcoded `"{}"` host to
    /// `render_component_with_host`, so `useSharedSlot(topic)` — which the JS
    /// shim resolves from `host.shared[topic]` — saw nothing, returned null,
    /// and the component threw. The island shipped as
    /// `__albedo_inject(id, null, 'error')` and never entered the DOM, while
    /// the *same* component rendered correctly through the in-process
    /// `render_entry_with_broadcast` path. These pin the host seed this
    /// registry now builds.
    mod host_seed {
        use super::*;
        use dom_render_compiler::runtime::BroadcastRegistry;

        fn plan_reading(topics: &[&str]) -> TierBEntryPlan {
            TierBEntryPlan {
                entry: "mod::Comp".to_string(),
                modules: Vec::new(),
                shared_topics: topics.iter().map(|t| (*t).to_string()).collect(),
                shared_partitions: Vec::new(),
                shared_sources: Vec::new(),
                shared_topic_classes: HashMap::new(),
                stamp_specs: HashMap::new(),
            }
        }

        /// The tests below predate partitions and are about the static half of
        /// the seed, so they pass none — PRISM § 9's "static topics behave
        /// exactly as today" is precisely what they now also pin.
        fn host_seed_for(
            plan: &TierBEntryPlan,
            broadcast: &BroadcastRegistry,
            spans: &serde_json::Map<String, Value>,
        ) -> String {
            super::host_seed_for(plan, &[], &[], broadcast, spans)
        }

        #[test]
        fn a_component_reading_no_shared_slots_gets_an_empty_host() {
            let broadcast = BroadcastRegistry::new();
            assert_eq!(
                host_seed_for(&plan_reading(&[]), &broadcast, &serde_json::Map::new()),
                "{}"
            );
        }

        /// The actual fix: the topic's live value has to reach the render.
        #[test]
        fn a_shared_slot_topic_is_seeded_at_its_current_value() {
            let broadcast = BroadcastRegistry::new();
            broadcast.topic("guestbook", br#"[{"author":"ada"}]"#.to_vec());

            let host = host_seed_for(&plan_reading(&["guestbook"]), &broadcast, &serde_json::Map::new());
            let parsed: Value = serde_json::from_str(&host).expect("host seed is JSON");

            assert_eq!(parsed["shared"]["guestbook"], json!([{"author": "ada"}]));
        }

        /// A write must be visible to the NEXT request — the registry holds the
        /// live broadcast `Arc`, not a boot-time snapshot. Seeding from a
        /// snapshot would pass the test above and still serve stale rows
        /// forever, which is the subtler version of the same bug.
        #[test]
        fn the_seed_reflects_writes_made_after_the_plan_was_built() {
            let broadcast = BroadcastRegistry::new();
            broadcast.topic("guestbook", b"[]".to_vec());
            let plan = plan_reading(&["guestbook"]);

            broadcast
                .write_topic("guestbook", br#"[{"author":"alan"}]"#.to_vec())
                .expect("topic is registered");

            let host = host_seed_for(&plan, &broadcast, &serde_json::Map::new());
            let parsed: Value = serde_json::from_str(&host).expect("host seed is JSON");
            assert_eq!(parsed["shared"]["guestbook"], json!([{"author": "alan"}]));
        }

        /// An unregistered topic is omitted, not seeded null: the shim then
        /// falls back on its own default instead of being handed a value the
        /// server never had.
        #[test]
        fn a_topic_absent_from_the_registry_is_omitted_from_the_seed() {
            let broadcast = BroadcastRegistry::new();
            broadcast.topic("known", b"1".to_vec());

            let host = host_seed_for(
                &plan_reading(&["known", "never-registered"]),
                &broadcast,
                &serde_json::Map::new(),
            );
            let parsed: Value = serde_json::from_str(&host).expect("host seed is JSON");

            assert!(parsed["shared"].get("known").is_some());
            assert!(
                parsed["shared"].get("never-registered").is_none(),
                "an unknown topic must not be invented as null"
            );
        }

        /// P6 · the error sinks must reach the render even when the component
        /// reads no shared slots. The old early return on an empty `shared`
        /// dropped the whole host, so a form in a slotless component got no
        /// sinks and its submit dropped the opcode frame.
        #[test]
        fn form_error_spans_are_seeded_even_without_shared_slots() {
            let broadcast = BroadcastRegistry::new();
            let mut spans = serde_json::Map::new();
            spans.insert(
                "sign_guestbook".to_string(),
                Value::String(
                    r#"<span data-albedo-id="1" data-albedo-error="author"></span>"#.to_string(),
                ),
            );
            let host = host_seed_for(&plan_reading(&[]), &broadcast, &spans);
            let parsed: Value = serde_json::from_str(&host).expect("host seed is JSON");
            assert_eq!(
                parsed["formErrorSpans"]["sign_guestbook"],
                json!(r#"<span data-albedo-id="1" data-albedo-error="author"></span>"#),
            );
        }
    }

    /// PRISM · the render half of dynamic topics: a `.where(…)` binding becomes
    /// a topic identity bound to *this* request's params, and both halves of the
    /// seed the JS shims read (`host.topics`, `host.shared`) are filled from it.
    mod partition_seed {
        use super::*;
        use dom_render_compiler::manifest::schema::PartitionTopicSpec;
        use dom_render_compiler::runtime::BroadcastRegistry;

        fn plan_reading_partition() -> TierBEntryPlan {
            TierBEntryPlan {
                entry: "routes/room/[id].tsx::Room".to_string(),
                modules: Vec::new(),
                shared_topics: Vec::new(),
                shared_sources: Vec::new(),
                shared_partitions: vec![PartitionTopicSpec {
                    binding: "rows".to_string(),
                    collection: "messages".to_string(),
                    column: "room".to_string(),
                    key: dom_render_compiler::manifest::schema::PartitionKeySource::RouteParam("id".to_string()),
                }],
                shared_topic_classes: HashMap::new(),
                stamp_specs: HashMap::new(),
            }
        }

        #[test]
        fn a_partition_resolves_against_the_params_the_component_receives() {
            let plan = plan_reading_partition();
            let resolved =
                resolve_partitions_for_props(&plan, &json!({ "params": { "id": "42" } }));

            assert_eq!(resolved.len(), 1);
            assert_eq!(resolved[0].topic, "messages:42");
            assert_eq!(resolved[0].binding, "rows");
        }

        /// A URL segment the alphabet rejects must produce no topic — and, far
        /// more importantly, must not fail the render. PRISM § 4: a weird id in
        /// a URL renders a static page, not a 500.
        #[test]
        fn a_hostile_param_yields_no_topic_and_no_failure() {
            let plan = plan_reading_partition();
            for hostile in ["a:b", "../secrets", ""] {
                let resolved = resolve_partitions_for_props(
                    &plan,
                    &json!({ "params": { "id": hostile } }),
                );
                assert!(resolved.is_empty(), "{hostile:?} must mint nothing");
            }
        }

        /// Props with no `params` at all — a static route, or a component
        /// rendered outside a matched route. Nothing resolves, nothing panics.
        #[test]
        fn absent_params_resolve_to_nothing() {
            let plan = plan_reading_partition();
            assert!(resolve_partitions_for_props(&plan, &json!({})).is_empty());
        }

        /// The seed's two halves have to agree: `host.topics[binding]` names the
        /// topic, and `host.shared[topic]` carries its value. The transpiled
        /// component reads the first to find the key for the second, so a
        /// mismatch renders an empty room with no error anywhere.
        #[test]
        fn the_seed_carries_the_binding_to_topic_map_and_the_value() {
            let broadcast = BroadcastRegistry::new();
            broadcast
                .try_topic_partition(
                    "messages:42".to_string(),
                    "messages".into(),
                    "42".into(),
                    br#"[{"body":"hello"}]"#.to_vec(),
                )
                .expect("minted");

            let plan = plan_reading_partition();
            let resolved =
                resolve_partitions_for_props(&plan, &json!({ "params": { "id": "42" } }));
            let host = super::super::host_seed_for(
                &plan,
                &resolved,
                &[],
                &broadcast,
                &serde_json::Map::new(),
            );
            let parsed: Value = serde_json::from_str(&host).expect("host seed is JSON");

            assert_eq!(parsed["topics"]["rows"], json!("messages:42"));
            assert_eq!(parsed["shared"]["messages:42"], json!([{"body": "hello"}]));
        }

        /// Two rooms are two topics. If this ever collapses, every room shows
        /// every other room's rows — the failure the whole design exists to make
        /// unrepresentable.
        #[test]
        fn two_keys_are_two_topics() {
            let plan = plan_reading_partition();
            let a = resolve_partitions_for_props(&plan, &json!({ "params": { "id": "a" } }));
            let b = resolve_partitions_for_props(&plan, &json!({ "params": { "id": "b" } }));
            assert_ne!(a[0].topic, b[0].topic);
        }
    }

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
    fn island_client_reference_stub_emits_placeholder_call() {
        // The server-graph stub for a Tier-C island must call the prelude
        // placeholder primitive with the island's exact manifest placeholder id
        // and export it as default — no island code, no other output.
        let stub = island_client_reference_stub("__c_progress_7");
        assert!(
            stub.contains("globalThis.__albedo_island_placeholder(\"__c_progress_7\")"),
            "stub must call the placeholder primitive with the pid: {stub}"
        );
        assert!(
            stub.trim_start().starts_with("export default"),
            "stub must be the module's default export: {stub}"
        );
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
        let registry = PooledTierBRenderRegistry::new(
            pool,
            TierBRenderPlan::new(),
            Arc::new(dom_render_compiler::runtime::BroadcastRegistry::new()),
            serde_json::Map::new(),
            Arc::new(crate::topics::NoTopicWarmer),
        );

        let err = registry
            .call("render::Missing", &json!({}), &HashMap::new())
            .await
            .expect_err("an unregistered component must fail loudly, not render empty");

        match err {
            RenderError::RegistryFailure {
                render_fn,
                thrown_message,
                diagnostic,
            } => {
                assert_eq!(render_fn, "render::Missing");
                assert!(
                    diagnostic.contains("no Tier-B render plan"),
                    "unexpected diagnostic: {diagnostic}"
                );
                // The unregistered-plan reason has no separate thrown text, so
                // the reader-facing message mirrors the diagnostic here.
                assert_eq!(thrown_message, diagnostic);
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
