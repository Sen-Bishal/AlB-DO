use crate::actions::{ActionHandler, SessionSlots};
use crate::api::ApiHandler;
use crate::config::AppConfig;
use crate::contract::{
    AllowAllAuthProvider, AuthDecision, AuthProvider, LayoutHandler, PropsLoader, RouteHandler,
    RuntimeMiddleware,
};
use crate::error::RuntimeError;
use crate::handlers::action::{run_action_request, ActionRegistry};
use crate::handlers::api::dispatch_api_route;
use crate::handlers::public_assets::PublicAssets;
use crate::handlers::{streaming_handler, StreamingAppState, StreamingTransportConfig};
use crate::inspector::{
    self as inspector_routes, GraphSnapshot as InspectorGraphSnapshot, InspectorState,
};
use crate::lifecycle::{RequestContext, ResponseBody, ResponsePayload};
use crate::render::csrf::CsrfRegistry;
use crate::render::tier_b::{SharedRenderServices, TierBOpcodeRegistry};
use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
use dom_render_compiler::runtime::{BroadcastRegistry, SessionId, SlotStore};
use crate::renderer_runtime::RendererRuntime;
use crate::routing::{CompiledRouter, HttpMethod, RouteMatch, RouteTarget};
use crate::webtransport::{WebTransportRuntime, WebTransportSessionRegistry};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info};

const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Bridge from a Phase-K `CompiledProject`'s handler registry to the
/// server's `ActionHandler` trait. One adapter is registered per
/// `(proxy_id, handler)` pair by `register_compiled_project`.
///
/// `handle` constructs a `SessionSlotView` from the dispatcher's
/// `SessionSlots` (same `Arc<SlotStore>`, same `SessionId`) and calls
/// the project's `invoke_action`. The drain happens inside
/// `invoke_action` so the explicit return already carries the
/// `SlotSet` opcodes; the dispatcher's follow-up drain is then a
/// no-op, which is idempotent and safe.
//
// Phase P · Stream C.2 — adapter carries the per-server
// `Arc<BroadcastRegistry>` so `handle()` can install it into the
// interpreter's `PHASE_K_BROADCAST` thread-local for the duration of
// the action dispatch. Without that install, a TS handler calling
// `broadcast(topic, updater)` would surface a clean error from the
// interpreter ("broadcast() unavailable") because the builtin only
// resolves when the thread-local is set.
struct CompiledProjectActionAdapter {
    project: Arc<dom_render_compiler::runtime::CompiledProject>,
    action_id: u32,
    broadcast: Arc<BroadcastRegistry>,
}

#[async_trait::async_trait]
impl ActionHandler for CompiledProjectActionAdapter {
    async fn handle(
        &self,
        _ctx: &RequestContext,
        envelope: &dom_render_compiler::ir::action::ActionEnvelope,
        slots: SessionSlots,
    ) -> Result<Vec<dom_render_compiler::ir::opcode::Instruction>, RuntimeError> {
        debug_assert_eq!(
            envelope.action_id, self.action_id,
            "compiled adapter mis-dispatched: registered for {}, got envelope for {}",
            self.action_id, envelope.action_id,
        );
        let view = dom_render_compiler::runtime::SessionSlotView::new(
            slots.session_id(),
            slots.store().clone(),
        );
        // Phase P · C.2 — `invoke_action_with_broadcast` installs the
        // broadcast registry on the per-thread Phase K stack for the
        // duration of `eval_handler_body`, so a TS action body's
        // `broadcast(topic, updater)` call routes through this same
        // `Arc<BroadcastRegistry>`. Fan-out lands on every subscribed
        // session over the WT patches lane without further plumbing.
        self.project
            .invoke_action_with_broadcast(envelope, &view, self.broadcast.as_ref())
            .map_err(|err| {
                RuntimeError::RequestHandling(format!(
                    "compiled action handler {} failed: {err:#}",
                    self.action_id
                ))
            })
    }
}

type SharedHandler = Arc<dyn RouteHandler>;
type SharedApiHandler = Arc<dyn ApiHandler>;
type SharedLayoutHandler = Arc<dyn LayoutHandler>;
type SharedMiddleware = Arc<dyn RuntimeMiddleware>;
type SharedAuthProvider = Arc<dyn AuthProvider>;
type SharedPropsLoader = Arc<dyn PropsLoader>;

#[derive(Clone)]
struct RuntimeState {
    router: Arc<CompiledRouter>,
    handlers: Arc<HashMap<String, SharedHandler>>,
    /// Phase-F — API handlers keyed by the same `handler_id` namespace
    /// as page handlers. Dispatch picks the right registry by looking
    /// up `target.handler_id` here before falling through to `handlers`.
    api_handlers: Arc<HashMap<String, SharedApiHandler>>,
    /// Phase-G — action handlers keyed by `action_id` (the same u32
    /// `BindEvent.proxy_id` carries on the wire). Served via
    /// `POST /_albedo/action`.
    action_handlers: Arc<ActionRegistry>,
    /// Phase-H — shared reactive slot store. Action handlers read and
    /// write through a `SessionSlots` view built per-request; the
    /// pipeline (when bound) holds the same `Arc<SlotStore>` so writes
    /// are visible to both sides without copying.
    slot_store: Arc<SlotStore>,
    /// Phase L — per-session CSRF token registry. The action
    /// dispatcher validates `_csrf` fields from JSON form payloads
    /// against this map; the renderer side will eventually read it
    /// to substitute the per-session token into hidden form inputs
    /// stamped with `data-albedo-csrf`.
    csrf: Arc<CsrfRegistry>,
    layouts: Arc<HashMap<String, SharedLayoutHandler>>,
    middleware: Arc<HashMap<String, SharedMiddleware>>,
    auth_provider: SharedAuthProvider,
    request_timeout: Duration,
    streaming_runtime: Option<Arc<StreamingAppState>>,
    inspector: Option<Arc<InspectorState>>,
    /// Phase M.1 — error registry the floating overlay subscribes
    /// to. `None` in production builds; `Some` when dev mode is on.
    dev_error_registry: Option<crate::dev::SharedErrorRegistry>,
    /// Phase M.2 — HMR registry the in-place DOM-swap client
    /// subscribes to. Same on/off semantics as the error registry.
    dev_hmr_registry: Option<crate::dev::SharedHmrRegistry>,
    /// Phase N — public/ static asset mount(s). When present,
    /// `dispatch` checks for a matching file before falling through
    /// to the dynamic route matcher.
    public_assets: Option<Arc<PublicAssets>>,
    /// Phase O.2 — broadcast slot registry. Topic-keyed shared
    /// state; writes fan out as `SlotSet` opcodes over the WT
    /// patches lane to every subscribed session. Always allocated
    /// (cheap when unused); userland reaches it via
    /// `AlbedoServer::broadcast()`.
    broadcast: Arc<BroadcastRegistry>,
}

pub struct AlbedoServerBuilder {
    config: AppConfig,
    handlers: HashMap<String, SharedHandler>,
    /// Phase-F — API handler registry. Distinct from `handlers` so
    /// dispatch can pick the right call path; same handler_id namespace
    /// so a route's `handler` field resolves to whichever registry the
    /// user populated.
    api_handlers: HashMap<String, SharedApiHandler>,
    /// Phase-G — action handler registry keyed by u32 `action_id`.
    /// Populated via [`Self::register_action`]; served by the
    /// `POST /_albedo/action` axum route.
    action_handlers: ActionRegistry,
    props_loaders: HashMap<String, SharedPropsLoader>,
    layouts: HashMap<String, SharedLayoutHandler>,
    middleware: HashMap<String, SharedMiddleware>,
    auth_provider: SharedAuthProvider,
    renderer: Option<RendererRuntime>,
    /// Dev inspector toggle. `Some(true)` / `Some(false)` overrides the
    /// default. `None` defaults to `cfg!(debug_assertions)` — on in
    /// debug builds, off in release.
    inspector_enabled: Option<bool>,
    /// Phase-E opcode registry. When set, the WT streaming path runs
    /// Tier-B render functions through this and ships opcodes; when
    /// unset, the WT path falls back to SSE.
    opcode_registry: Option<Arc<dyn TierBOpcodeRegistry>>,
    /// Phase-D opcode pipeline + tokio runtime handle. The handle is
    /// stashed alongside so the pipeline can spawn resolver Futures.
    /// Userland binds both via `with_pipeline`.
    pipeline: Option<(FourLaneRuntimePipeline, tokio::runtime::Handle)>,
    /// Phase M — dev-mode toggle. `Some(true)` / `Some(false)`
    /// overrides; `None` defaults to `cfg!(debug_assertions)` so
    /// debug builds get the overlay + HMR endpoints automatically.
    dev_mode_enabled: Option<bool>,
    /// Phase N — directories served verbatim at the URL root. Each
    /// `with_public_dir` call appends; the first matching root wins.
    public_dirs: Vec<std::path::PathBuf>,
    /// Phase N — `Cache-Control` value applied to every public asset
    /// response. `None` means auto: `public, max-age=3600` when dev
    /// mode is off, `no-store` when dev mode is on.
    public_cache_control: Option<String>,
    /// Phase P · Stream C.2 — the per-server broadcast registry, minted
    /// at builder construction so [`Self::register_compiled_project`]
    /// can clone the same `Arc` into every `CompiledProjectActionAdapter`.
    /// `build()` reuses this exact `Arc` for `RuntimeState.broadcast`,
    /// so action handlers, the WT runtime, and any userland write all
    /// resolve topics against one registry.
    broadcast: Arc<BroadcastRegistry>,
}

impl AlbedoServerBuilder {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            handlers: HashMap::new(),
            api_handlers: HashMap::new(),
            action_handlers: ActionRegistry::new(),
            props_loaders: HashMap::new(),
            layouts: HashMap::new(),
            middleware: HashMap::new(),
            auth_provider: Arc::new(AllowAllAuthProvider),
            renderer: None,
            inspector_enabled: None,
            opcode_registry: None,
            pipeline: None,
            dev_mode_enabled: None,
            public_dirs: Vec::new(),
            public_cache_control: None,
            // Phase P · C.2 — mint here so `register_compiled_project`
            // (which may run before `build()`) sees the same `Arc` the
            // runtime state will hold. Idle cost is one empty
            // DashMap; non-broadcast workloads don't pay anything.
            broadcast: Arc::new(BroadcastRegistry::new()),
        }
    }

    /// Phase P · C.2 — access the broadcast registry this builder
    /// will install on the eventual [`AlbedoServer`]. Useful when
    /// userland code needs to seed a topic (with
    /// [`BroadcastRegistry::topic`]) before any client connects.
    /// Cloning the returned `Arc` is cheap; both halves resolve to
    /// the same registry.
    pub fn broadcast(&self) -> Arc<BroadcastRegistry> {
        self.broadcast.clone()
    }

    /// Phase N — mount a directory whose files are served verbatim
    /// at the URL root (`<dir>/logo.svg` → `GET /logo.svg`). Multiple
    /// calls stack; the first matching root wins. Lookups go through
    /// [`crate::handlers::public_assets::sanitize_public_path`] so
    /// traversal attempts cannot escape the mount.
    #[must_use]
    pub fn with_public_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.public_dirs.push(dir.into());
        self
    }

    /// Phase N — override the `Cache-Control` header used for public
    /// asset responses. When unset the value tracks dev mode:
    /// `no-store` in dev, `public, max-age=3600` in production.
    #[must_use]
    pub fn with_public_cache_control(mut self, value: impl Into<String>) -> Self {
        self.public_cache_control = Some(value.into());
        self
    }

    /// Phase M — explicit toggle for the error overlay + HMR
    /// surface mounted at `/_albedo/dev/*`. `None` (default) means
    /// auto: enabled on `cfg!(debug_assertions)`, off otherwise.
    #[must_use]
    pub fn with_dev_mode(mut self, enabled: bool) -> Self {
        self.dev_mode_enabled = Some(enabled);
        self
    }

    /// Forces the dev inspector on or off. By default the inspector is mounted
    /// when the binary is built with debug assertions and skipped otherwise —
    /// call this to override that policy (for example, to expose the inspector
    /// in a release-mode preview build).
    pub fn with_inspector(mut self, enabled: bool) -> Self {
        self.inspector_enabled = Some(enabled);
        self
    }

    pub fn register_handler(
        mut self,
        handler_id: impl Into<String>,
        handler: impl RouteHandler + 'static,
    ) -> Self {
        self.handlers.insert(handler_id.into(), Arc::new(handler));
        self
    }

    /// Registers an [`ApiHandler`] under `handler_id`. Routes whose
    /// `handler` field resolves to this id are dispatched through the
    /// API path ([`dispatch_api_route`]) instead of the page-route
    /// pipeline. Auth still flows through the registered
    /// `AuthProvider` against `RouteTarget.auth`.
    pub fn register_api_handler(
        mut self,
        handler_id: impl Into<String>,
        handler: impl ApiHandler + 'static,
    ) -> Self {
        self.api_handlers
            .insert(handler_id.into(), Arc::new(handler));
        self
    }

    /// Phase-G — registers an [`ActionHandler`] under the u32
    /// `action_id`. Bakabox's `BindEvent` opcode carries `action_id`
    /// as its `proxy_id`; when the corresponding DOM event fires, the
    /// client POSTs an `ActionEnvelope` to `/_albedo/action`. The
    /// handler returns opcode patches which the dispatcher wire-encodes
    /// and returns to bakabox for in-place DOM mutation.
    pub fn register_action(
        mut self,
        action_id: u32,
        handler: impl ActionHandler + 'static,
    ) -> Self {
        self.action_handlers.insert(action_id, Arc::new(handler));
        self
    }

    /// Phase K — register every handler in a [`CompiledProject`] into
    /// the action registry. This is the bridge that turns a successful
    /// compile + render into a live action dispatcher: bakabox POSTs
    /// `/_albedo/action` with the `proxy_id` it learned from a
    /// `BindEvent` opcode, the dispatcher routes by `action_id` (same
    /// `u32`), and the compiled handler body executes server-side via
    /// the shared Phase-J interpreter with setter calls translating to
    /// slot writes.
    ///
    /// The same `CompiledProject` instance should drive both rendering
    /// (`render_entry_with_bindings`) and dispatch (this builder
    /// method) so the slot ids, proxy ids, and handler bodies all line
    /// up. Multiple `CompiledProject`s can coexist by calling this
    /// method repeatedly — proxy_id collisions are vanishingly
    /// unlikely (FNV-1a-32 over `{module}::{fn}::{event}#{idx}`) but
    /// later registrations win.
    pub fn register_compiled_project(
        mut self,
        project: Arc<dom_render_compiler::runtime::CompiledProject>,
    ) -> Self {
        for proxy_id in project.handler_proxy_ids() {
            let adapter = CompiledProjectActionAdapter {
                project: project.clone(),
                action_id: proxy_id,
                // Phase P · C.2 — share the builder's broadcast `Arc`
                // with the adapter so its `handle()` invocation routes
                // `broadcast(topic, updater)` calls through the same
                // registry the WT runtime + route handlers see.
                broadcast: self.broadcast.clone(),
            };
            self.action_handlers.insert(proxy_id, Arc::new(adapter));
        }

        // Phase P · Stream C.3 — auto-register every `useSharedSlot`
        // topic this project references so the streaming handler's
        // C.4 auto-subscribe pass (and any userland `broadcast()`
        // write that happens before the first subscriber) finds a
        // live `BroadcastTopic` to attach to. `BroadcastRegistry::topic`
        // is idempotent — a second call with the same name returns
        // the existing entry rather than clobbering its value, so
        // calling this on multiple `CompiledProject`s that share
        // topics is safe. Seed value is `b"null"` rather than `b"[]"`
        // because we don't know the topic's element type at this
        // layer; the `broadcast()` interpreter builtin already
        // tolerates a `Null` current value by passing it to the
        // updater closure.
        for topic in project.shared_slot_topics() {
            self.broadcast.topic(topic, b"null".to_vec());
        }

        self
    }

    /// Phase L — registers a typed form-submit handler under an
    /// action **name** (the suffix the JSX form's
    /// `action="action:NAME"` carries). The builder derives the
    /// stable `action_id` via FNV-1a-32 (the same hash family the
    /// renderer stamps into `data-albedo-action`), so userland never
    /// has to compute the id by hand. The dispatcher decodes the
    /// incoming `ActionEnvelope.payload` as JSON into `T` before
    /// invoking `handler`; on parse failure the action surfaces a
    /// [`RuntimeError::RequestHandling`] which the action HTTP path
    /// renders as a 500 with the underlying serde message.
    ///
    /// The form payload shape is the JSON object the client-side
    /// runtime emits from a browser `FormData`: keys are input
    /// `name` attributes, values are the last submitted string value
    /// for each name. Repeated `name`s collapse to the last value
    /// (matches `<form>` POST semantics). For per-field validation
    /// patches (`SetText` opcodes targeting `data-albedo-error`
    /// spans), implement [`crate::render::FromFormPayload`] on a
    /// wrapping type and register through
    /// [`Self::register_action`] with [`crate::render::form_action_handler`].
    pub fn register_form_action<T, F, Fut>(
        mut self,
        action_name: impl Into<String>,
        handler: F,
    ) -> Self
    where
        T: serde::de::DeserializeOwned + Send + 'static,
        F: Fn(RequestContext, T, SessionSlots) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<
                Output = Result<
                    Vec<dom_render_compiler::ir::opcode::Instruction>,
                    RuntimeError,
                >,
            > + Send
            + 'static,
    {
        // Derive the wire-level `action_id` from the user-supplied
        // action name. Same FNV-1a-32 family the compile-time form
        // extractor uses, so the JSX `action="action:NAME"` and the
        // server-side `register_form_action("NAME", ...)` resolve to
        // the same `action_id` on the wire without any per-route
        // configuration.
        let action_name = action_name.into();
        let action_id = crate::render::form_action::form_action_id(&action_name);

        let handler = Arc::new(handler);
        let wrapped = move |ctx: RequestContext,
                            envelope: dom_render_compiler::ir::action::ActionEnvelope,
                            slots: SessionSlots| {
            let handler = handler.clone();
            async move {
                let parsed: T = serde_json::from_slice(&envelope.payload).map_err(|err| {
                    RuntimeError::RequestHandling(format!(
                        "form payload did not deserialize as {}: {err}",
                        std::any::type_name::<T>()
                    ))
                })?;
                (handler)(ctx, parsed, slots).await
            }
        };
        self.action_handlers
            .insert(action_id, Arc::new(wrapped));
        self
    }

    pub fn register_props_loader(
        mut self,
        loader_id: impl Into<String>,
        loader: impl PropsLoader + 'static,
    ) -> Self {
        self.props_loaders
            .insert(loader_id.into(), Arc::new(loader));
        self
    }

    pub fn register_layout(
        mut self,
        layout_id: impl Into<String>,
        layout_handler: impl LayoutHandler + 'static,
    ) -> Self {
        self.layouts
            .insert(layout_id.into(), Arc::new(layout_handler));
        self
    }

    pub fn register_middleware(
        mut self,
        middleware_id: impl Into<String>,
        middleware: impl RuntimeMiddleware + 'static,
    ) -> Self {
        self.middleware
            .insert(middleware_id.into(), Arc::new(middleware));
        self
    }

    pub fn with_auth_provider(mut self, auth_provider: impl AuthProvider + 'static) -> Self {
        self.auth_provider = Arc::new(auth_provider);
        self
    }

    pub fn with_renderer_runtime(mut self, renderer: RendererRuntime) -> Self {
        self.renderer = Some(renderer);
        self
    }

    /// Registers the Phase-E opcode registry that resolves Tier-B
    /// nodes for the WT streaming path. Without it the WT streaming
    /// path errors out and the request falls back to SSE.
    pub fn with_opcode_registry(
        mut self,
        registry: impl TierBOpcodeRegistry + 'static,
    ) -> Self {
        self.opcode_registry = Some(Arc::new(registry));
        self
    }

    /// Binds an opcode pipeline + tokio runtime handle. The pair is
    /// installed on `StreamingAppState` so Phase-D's async-island
    /// machinery can spawn resolver Futures and Phase-E's WT path can
    /// drain opcode chunks. Pair this with [`Self::with_opcode_registry`]
    /// to enable the binary WT path end-to-end.
    pub fn with_pipeline(
        mut self,
        pipeline: FourLaneRuntimePipeline,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        self.pipeline = Some((pipeline, runtime_handle));
        self
    }

    pub fn build(self) -> Result<AlbedoServer, RuntimeError> {
        self.config.validate()?;

        let router = CompiledRouter::from_route_and_layout_specs(
            self.config.routes.as_slice(),
            self.config.layouts.as_slice(),
        )?;

        let mut renderer = self.renderer;
        if renderer.is_none() {
            if let Some(renderer_config) = &self.config.renderer {
                renderer = Some(RendererRuntime::from_config(renderer_config)?);
            }
        }

        let shared_wt_sessions = self
            .config
            .server
            .webtransport
            .enabled
            .then(WebTransportSessionRegistry::default);

        let services = SharedRenderServices {
            opcode_registry: self.opcode_registry.clone(),
            ..SharedRenderServices::default()
        };

        // Phase-H — one shared slot store for the lifetime of the
        // server. Action handlers read/write through it via the
        // dispatcher-built `SessionSlots`; the pipeline, when bound,
        // holds the same `Arc` so future tick-side emissions see the
        // same state. Without this sharing each side would run
        // against an empty store and the reactive loop never closes.
        let slot_store = Arc::new(SlotStore::new());

        // Phase L · mint the CSRF registry once and share the same
        // `Arc` between the streaming state (which mints tokens
        // during page render) and `RuntimeState` (which validates
        // them during action dispatch). The two paths MUST see the
        // same token table or every form POST 403s.
        let csrf_registry = Arc::new(CsrfRegistry::new());

        // Phase O.2 · single broadcast registry per server (minted in
        // the builder so `register_compiled_project` adapters share
        // the same `Arc`). Every route/action handler that publishes
        // a topic
        // ──────────────────────────────────────────────────────────
        // Phase P · C.2 trailing note: the same `Arc` is now reused
        // from `self.broadcast` rather than re-minted here, so
        // adapters registered before `build()` see the same registry
        // the runtime state ends up with. `subscribe()` / `write_topic()`
        // are themselves concurrent so no further sharing layer is
        // needed.
        let broadcast = self.broadcast;

        // Construct StreamingAppState, binding the optional pipeline +
        // runtime handle when both are present. `with_pipeline` consumes
        // the pair, so `take()` to move it out of the builder. The Arc
        // wrap happens after pipeline binding so the bound pipeline is
        // visible through `state.pipeline()`.
        let mut pipeline_binding = self.pipeline;
        let streaming_runtime = renderer.as_ref().map(|runtime| {
            let state = StreamingAppState::new(
                Arc::new(runtime.manifest().clone()),
                services.clone(),
                StreamingTransportConfig::new(
                    self.config.server.webtransport.enabled,
                    self.config.server.port,
                ),
                shared_wt_sessions.clone(),
            )
            .with_csrf(csrf_registry.clone())
            // Phase P · C.4 — same broadcast `Arc` the action adapter
            // and runtime state hold, so a WT session's auto-subscribe
            // attaches the patches-lane sender to topics that
            // subsequent action-handler `broadcast()` calls fan out to.
            .with_broadcast(broadcast.clone());
            let state = match pipeline_binding.take() {
                Some((pipeline, handle)) => {
                    let pipeline = pipeline.with_slot_store(slot_store.clone());
                    state.with_pipeline(pipeline, handle)
                }
                None => state,
            };
            Arc::new(state)
        });

        let has_entry_routes = self
            .config
            .routes
            .iter()
            .any(|route| route.entry_module.is_some());

        for route in &self.config.routes {
            let has_layout_handlers = match router.match_route(route.method, route.path.as_str()) {
                RouteMatch::Matched(matched) => !matched.target.layout_handlers.is_empty(),
                RouteMatch::MethodNotAllowed { .. } | RouteMatch::NotFound => true,
            };

            let route_uses_manifest_streaming =
                matches!(route.method, HttpMethod::Get | HttpMethod::Head)
                    && route.entry_module.is_some()
                    && route.props_loader.is_none()
                    && route.auth.is_none()
                    && route.middleware.is_empty()
                    && !has_layout_handlers
                    && streaming_runtime
                        .as_ref()
                        .map(|runtime| runtime.manifest.routes.contains_key(route.path.as_str()))
                        .unwrap_or(false);

            // Phase-F: a route's `handler` may resolve to either a
            // page `RouteHandler` or an API `ApiHandler`. Build fails
            // only when neither registry knows the id.
            if !route_uses_manifest_streaming
                && !self.handlers.contains_key(route.handler.as_str())
                && !self.api_handlers.contains_key(route.handler.as_str())
            {
                return Err(RuntimeError::HandlerNotFound {
                    handler_id: route.handler.clone(),
                });
            }
            if let Some(props_loader_id) = &route.props_loader {
                if !self.props_loaders.contains_key(props_loader_id) {
                    return Err(RuntimeError::PropsLoaderNotFound {
                        loader_id: props_loader_id.clone(),
                    });
                }
            }
            for middleware in &route.middleware {
                if !self.middleware.contains_key(middleware.as_str()) {
                    return Err(RuntimeError::MiddlewareNotFound {
                        middleware_id: middleware.clone(),
                    });
                }
            }
        }
        if has_entry_routes && renderer.is_none() {
            return Err(RuntimeError::RendererNotConfigured);
        }
        for layout in &self.config.layouts {
            if !self.layouts.contains_key(layout.handler.as_str()) {
                return Err(RuntimeError::LayoutNotFound {
                    layout_id: layout.handler.clone(),
                });
            }
        }

        let inspector_enabled = self
            .inspector_enabled
            .unwrap_or(cfg!(debug_assertions));
        let inspector = if inspector_enabled {
            let inspector_state = Arc::new(InspectorState::new());
            if let Some(streaming) = streaming_runtime.as_ref() {
                inspector_state.set_graph(InspectorGraphSnapshot::from_manifest(
                    streaming.manifest.as_ref(),
                ));
            }
            Some(inspector_state)
        } else {
            None
        };

        // Phase M · mint dev-mode registries when enabled. Defaults
        // follow the inspector convention (on in debug builds, off
        // in release) so a `cargo run --release` server doesn't leak
        // dev routes.
        let dev_mode_enabled = self
            .dev_mode_enabled
            .unwrap_or(cfg!(debug_assertions));
        let (dev_error_registry, dev_hmr_registry) = if dev_mode_enabled {
            (
                Some(Arc::new(crate::dev::DevErrorRegistry::new())),
                Some(Arc::new(crate::dev::HmrRegistry::new())),
            )
        } else {
            (None, None)
        };

        let public_assets = if self.public_dirs.is_empty() {
            None
        } else {
            let cache_control = self.public_cache_control.unwrap_or_else(|| {
                if dev_mode_enabled {
                    "no-store".to_string()
                } else {
                    "public, max-age=3600".to_string()
                }
            });
            Some(Arc::new(PublicAssets::new(
                self.public_dirs,
                cache_control.as_str(),
            )))
        };

        let state = RuntimeState {
            router: Arc::new(router),
            handlers: Arc::new(self.handlers),
            api_handlers: Arc::new(self.api_handlers),
            action_handlers: Arc::new(self.action_handlers),
            slot_store,
            // Phase L · same Arc the streaming state holds, so
            // tokens minted during page render are the ones the
            // action dispatcher validates against.
            csrf: csrf_registry.clone(),
            layouts: Arc::new(self.layouts),
            middleware: Arc::new(self.middleware),
            auth_provider: self.auth_provider,
            request_timeout: Duration::from_millis(self.config.server.request_timeout_ms),
            streaming_runtime,
            inspector,
            dev_error_registry,
            dev_hmr_registry,
            public_assets,
            broadcast,
        };

        Ok(AlbedoServer {
            config: self.config,
            state,
        })
    }
}

pub struct AlbedoServer {
    config: AppConfig,
    state: RuntimeState,
}

impl AlbedoServer {
    pub fn router(&self) -> Router {
        Router::new()
            .route("/", any(dispatch))
            .route("/{*path}", any(dispatch))
            .with_state(self.state.clone())
    }

    /// Handle on the dev inspector's shared state, when one is mounted.
    /// Subsystems that want to publish render events into the inspector hold
    /// onto this `Arc` and call `publish_event` directly — there is no
    /// additional indirection from this method.
    pub fn inspector(&self) -> Option<Arc<InspectorState>> {
        self.state.inspector.clone()
    }

    /// Phase L · handle on the shared CSRF token registry. Used by
    /// integration tests that need to mint or inspect tokens
    /// outside the page-render path (for example, to construct a
    /// known-valid form-submit payload without first hitting the
    /// streaming handler). Production code does not need this — the
    /// page-render path mints tokens on its own.
    pub fn csrf_registry(&self) -> Arc<CsrfRegistry> {
        self.state.csrf.clone()
    }

    /// Phase M.1 · access the dev error overlay registry. `None`
    /// when the server was built without dev mode enabled. Userland
    /// integration code (a file watcher, an external linter, etc.)
    /// uses this to push errors into the in-browser overlay.
    pub fn dev_error_registry(&self) -> Option<crate::dev::SharedErrorRegistry> {
        self.state.dev_error_registry.clone()
    }

    /// Phase M.2 · access the slot-preserving HMR registry. Same
    /// availability rules as the error registry above.
    pub fn dev_hmr_registry(&self) -> Option<crate::dev::SharedHmrRegistry> {
        self.state.dev_hmr_registry.clone()
    }

    /// Phase N · expose the public asset registry for tests and
    /// userland code that wants to introspect the mounted roots.
    /// `None` when no `with_public_dir(..)` calls were made.
    pub fn public_assets(&self) -> Option<Arc<PublicAssets>> {
        self.state.public_assets.clone()
    }

    /// Phase O.2 · handle on the per-server broadcast registry.
    /// Route handlers, action handlers, and userland watchers all
    /// resolve topics against this `Arc`. Always available — there
    /// is no "broadcast disabled" mode; an unused registry is just
    /// an empty `DashMap` and costs nothing at idle.
    pub fn broadcast(&self) -> Arc<BroadcastRegistry> {
        self.state.broadcast.clone()
    }

    pub async fn run(self) -> Result<(), RuntimeError> {
        let addr = self.config.server.socket_addr()?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| RuntimeError::ServerStartup(err.to_string()))?;
        info!("ALBEDO server listening on {}", addr);
        let router = self.router();

        let shutdown_timeout = Duration::from_millis(self.config.server.shutdown_timeout_ms);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        if let Some(inspector_state) = self.state.inspector.clone() {
            info!("ALBEDO dev inspector mounted at /__albedo");
            crate::inspector::heartbeat::spawn(inspector_state, shutdown_rx.clone());
        }

        let webtransport_task = if self.config.server.webtransport.enabled {
            let shared_sessions = self
                .state
                .streaming_runtime
                .as_ref()
                .and_then(|streaming| streaming.webtransport_sessions.clone())
                .unwrap_or_default();
            let runtime = WebTransportRuntime::bind_with_registry(
                addr,
                &self.config.server.webtransport,
                shared_sessions,
            )?
            .with_broadcast(self.state.broadcast.clone());
            info!("ALBEDO WebTransport QUIC listener active on {}", addr);
            let wt_shutdown = shutdown_rx.clone();
            Some(tokio::spawn(async move { runtime.run(wt_shutdown).await }))
        } else {
            info!("ALBEDO WebTransport disabled; SSE/HTTP streaming fallback remains active");
            None
        };

        let graceful_shutdown = {
            let shutdown_tx = shutdown_tx.clone();
            async move {
                shutdown_signal(shutdown_timeout).await;
                let _ = shutdown_tx.send(true);
            }
        };

        let http_result = axum::serve(listener, router)
            .with_graceful_shutdown(graceful_shutdown)
            .await
            .map_err(|err| RuntimeError::ServerRuntime(err.to_string()));

        let _ = shutdown_tx.send(true);

        if let Some(task) = webtransport_task {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(err) => {
                    return Err(RuntimeError::ServerRuntime(format!(
                        "webtransport task join failed: {err}"
                    )));
                }
            }
        }

        http_result
    }
}

async fn dispatch(State(state): State<RuntimeState>, request: Request<Body>) -> Response {
    let method = match HttpMethod::try_from(request.method()) {
        Ok(method) => method,
        Err(err) => return err.into_response(),
    };

    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);

    if path == "/_albedo/wt" {
        if let Some(streaming_runtime) = &state.streaming_runtime {
            return streaming_handler(State(streaming_runtime.clone()), request)
                .await
                .into_response();
        }
    }

    if inspector_routes::matches_inspector_path(path.as_str()) {
        if let Some(inspector) = &state.inspector {
            return inspector_routes::dispatch(inspector, path.as_str()).into_response();
        }
    }

    // Phase M · dev-mode error overlay + HMR endpoints. Only mounted
    // when the corresponding registries exist on RuntimeState; in
    // production builds both are None and these routes fall through
    // to the regular router, which surfaces a clean 404.
    if path.starts_with("/_albedo/dev/") {
        match path.as_str() {
            "/_albedo/dev/overlay.js" => {
                if state.dev_error_registry.is_some() {
                    return crate::handlers::dev::serve_overlay_script().into_response();
                }
            }
            "/_albedo/dev/hmr-apply.js" => {
                if state.dev_hmr_registry.is_some() {
                    return crate::handlers::dev::serve_hmr_apply_script().into_response();
                }
            }
            "/_albedo/dev/errors" => {
                if let Some(registry) = &state.dev_error_registry {
                    return crate::handlers::dev::serve_error_stream(registry.clone())
                        .into_response();
                }
            }
            "/_albedo/dev/hmr" => {
                if let Some(registry) = &state.dev_hmr_registry {
                    return crate::handlers::dev::serve_hmr_stream(registry.clone())
                        .into_response();
                }
            }
            _ => {
                if state.dev_error_registry.is_some() || state.dev_hmr_registry.is_some() {
                    return crate::handlers::dev::dev_not_found().into_response();
                }
            }
        }
    }

    // Phase-G — bakabox → server action invocations land here. Only
    // POST is accepted; other methods fall through to the normal
    // router (which will surface 405 or 404 as appropriate).
    if path == "/_albedo/action" && method == HttpMethod::Post {
        return run_action_route(&state, request).await;
    }

    // Phase P · post-P wire-through — embedded bakabox client
    // assets. Serves runtime.js / bincode.js / link-forms.js etc.
    // from the binary directly, so production no longer needs to
    // mount `<dist>` as a public_dir (which used to shadow `/` with
    // the static fallback index.html). Fires BEFORE the
    // public-assets dispatch so a user's `public/runtime.js`
    // doesn't accidentally hijack the framework path.
    if matches!(method, HttpMethod::Get | HttpMethod::Head) {
        if let Some(response) =
            crate::handlers::albedo_assets::dispatch_albedo_asset(path.as_str())
        {
            let mut response = response;
            if method == HttpMethod::Head {
                *response.body_mut() = Body::empty();
            }
            return response;
        }
    }

    // Phase N — `public/` static assets resolve before dynamic
    // routes so `public/logo.svg` reliably serves at `/logo.svg`
    // even when the route map has a catch-all. GET/HEAD only; other
    // methods fall through and surface 405 from the router.
    if matches!(method, HttpMethod::Get | HttpMethod::Head) {
        if let Some(assets) = &state.public_assets {
            if let Some(file) = assets.resolve(path.as_str()) {
                let mut response = assets.read_response(&file);
                if method == HttpMethod::Head {
                    *response.body_mut() = Body::empty();
                }
                return response;
            }
        }
    }

    let route_match = state.router.match_route(method, path.as_str());
    let response = match route_match {
        RouteMatch::NotFound => RuntimeError::RouteNotFound {
            method: method.as_str().to_string(),
            path,
        }
        .into_response(),
        RouteMatch::MethodNotAllowed { allowed } => ResponsePayload::new(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("method '{}' is not allowed for this route", method.as_str()),
        )
        .with_header(
            "allow",
            allowed
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
        .into_response(),
        RouteMatch::Matched(matched) => {
            if should_use_manifest_streaming(&state, &matched.target, method, path.as_str()) {
                if let Some(streaming_runtime) = &state.streaming_runtime {
                    return streaming_handler(State(streaming_runtime.clone()), request)
                        .await
                        .into_response();
                }
            }

            let (parts, body) = request.into_parts();
            let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
                Ok(body) => body,
                Err(err) => {
                    return RuntimeError::RequestBodyRead(err.to_string()).into_response();
                }
            };

            let request_context = RequestContext::new(
                method,
                path.clone(),
                query.as_deref(),
                matched.params,
                &parts.headers,
                body,
            );

            // Phase-F: if `handler_id` resolves to an API handler,
            // dispatch through the API path. Otherwise fall through to
            // the page-route flow (middleware, auth, handler, layout).
            if let Some(api_handler) = state.api_handlers.get(&matched.target.handler_id).cloned()
            {
                return run_api_request(&state, matched.target, request_context, api_handler).await;
            }

            let mut request_context = request_context;
            match execute_route(&state, matched.target, &mut request_context).await {
                Ok(response) => response.into_response(),
                Err(err) => {
                    error!(request_id = request_context.request_id, error = %err, "request failed");
                    err.into_response()
                }
            }
        }
    };

    response
}

/// HTTP header bakabox sets to carry the session id alongside each
/// action POST. Mirrors the WT-layer header used during session
/// handshake. Production deployments should bind a signed cookie at
/// session-open time and prefer that over the plain header.
const ACTION_SESSION_HEADER: &str = "x-albedo-session";

/// Phase-G/H — runs the action HTTP route. Reads the body, builds a
/// `RequestContext`, extracts a session id from the
/// `x-albedo-session` header (synthesising a random one when absent so
/// handlers never see `None`), and dispatches to [`run_action_request`]
/// with a [`SessionSlots`] view bound to the server's shared slot
/// store. The body cap matches `MAX_REQUEST_BODY_BYTES` so an oversized
/// envelope is rejected with the same shape as any other large request.
async fn run_action_route(state: &RuntimeState, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => return RuntimeError::RequestBodyRead(err.to_string()).into_response(),
    };

    // Phase L · prefer the `albedo-session` cookie (set by the
    // streaming handler on first page render) over the explicit
    // `x-albedo-session` header. Browser-driven form POSTs auto-send
    // the cookie; programmatic clients can still override via the
    // header. Without either, fall back to a fresh random session —
    // which will trip CSRF validation on a subsequent submit, which
    // is the correct failure mode.
    let session_id = crate::render::csrf::read_session_cookie(&parts.headers)
        .or_else(|| {
            parts
                .headers
                .get(ACTION_SESSION_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|raw| uuid::Uuid::parse_str(raw).ok())
                .map(SessionId::new)
        })
        .unwrap_or_else(SessionId::random);

    let query = parts.uri.query().map(str::to_string);
    let ctx = RequestContext::new(
        HttpMethod::Post,
        parts.uri.path().to_string(),
        query.as_deref(),
        Default::default(),
        &parts.headers,
        body.clone(),
    );

    let slots = SessionSlots::new(session_id, state.slot_store.clone());
    run_action_request(
        state.action_handlers.as_ref(),
        state.csrf.as_ref(),
        ctx,
        body,
        slots,
    )
    .await
}

/// Runs an API request: applies the route-level timeout, calls
/// [`dispatch_api_route`], and converts the result into an axum
/// response. Centralised so the dispatcher stays linear and so future
/// per-request observability (tracing, metrics) attaches in one place.
async fn run_api_request(
    state: &RuntimeState,
    target: RouteTarget,
    ctx: RequestContext,
    handler: SharedApiHandler,
) -> Response {
    let request_id = ctx.request_id.clone();
    let dispatch = dispatch_api_route(&target, ctx, &state.auth_provider, &handler);
    let result = tokio::time::timeout(state.request_timeout, dispatch).await;
    match result {
        Ok(Ok(api_response)) => api_response.into_response(),
        Ok(Err(err)) => {
            error!(request_id, error = %err, "api request failed");
            err.into_response()
        }
        Err(_) => {
            let err = RuntimeError::RequestHandling(format!(
                "api request timed out after {} ms",
                state.request_timeout.as_millis()
            ));
            error!(request_id, error = %err, "api request timed out");
            err.into_response()
        }
    }
}

async fn execute_route(
    state: &RuntimeState,
    target: RouteTarget,
    ctx: &mut RequestContext,
) -> Result<ResponsePayload, RuntimeError> {
    for middleware_id in &target.middleware {
        let middleware = state.middleware.get(middleware_id).ok_or_else(|| {
            RuntimeError::MiddlewareNotFound {
                middleware_id: middleware_id.clone(),
            }
        })?;
        middleware.on_request(ctx).await?;
    }

    if let Some(policy) = &target.auth {
        match state.auth_provider.authorize(ctx, policy).await? {
            AuthDecision::Allow => {}
            AuthDecision::Deny { reason } => {
                return Err(RuntimeError::Authentication(reason));
            }
        }
    }

    let handler = state
        .handlers
        .get(target.handler_id.as_str())
        .ok_or_else(|| RuntimeError::HandlerNotFound {
            handler_id: target.handler_id.clone(),
        })?
        .clone();

    let ctx_for_response_hooks = ctx.clone();
    let response_fut = handler.handle(ctx.clone());
    let mut response = tokio::time::timeout(state.request_timeout, response_fut)
        .await
        .map_err(|_| {
            RuntimeError::RequestHandling(format!(
                "request timed out after {} ms",
                state.request_timeout.as_millis()
            ))
        })??;

    if !target.layout_handlers.is_empty() {
        apply_layout_handlers(state, target.layout_handlers.as_slice(), ctx, &mut response).await?;
    }

    for middleware_id in target.middleware.iter().rev() {
        let middleware = state.middleware.get(middleware_id).ok_or_else(|| {
            RuntimeError::MiddlewareNotFound {
                middleware_id: middleware_id.clone(),
            }
        })?;
        middleware
            .on_response(&ctx_for_response_hooks, &mut response)
            .await?;
    }

    Ok(response)
}
fn should_use_manifest_streaming(
    state: &RuntimeState,
    target: &RouteTarget,
    method: HttpMethod,
    path: &str,
) -> bool {
    if !matches!(method, HttpMethod::Get | HttpMethod::Head) {
        return false;
    }

    if target.entry_module.is_none() {
        return false;
    }

    if target.props_loader.is_some() || target.auth.is_some() {
        return false;
    }

    if !target.middleware.is_empty() || !target.layout_handlers.is_empty() {
        return false;
    }

    state
        .streaming_runtime
        .as_ref()
        .map(|runtime| runtime.manifest.routes.contains_key(path))
        .unwrap_or(false)
}

async fn apply_layout_handlers(
    state: &RuntimeState,
    layout_handlers: &[String],
    ctx: &RequestContext,
    response: &mut ResponsePayload,
) -> Result<(), RuntimeError> {
    if !response_is_html(response) {
        return Ok(());
    }

    let mut wrapped_html = match &response.body {
        ResponseBody::Full(body) => std::str::from_utf8(body.as_ref())
            .map_err(|err| {
                RuntimeError::RequestHandling(format!("failed to decode HTML body: {err}"))
            })?
            .to_string(),
        ResponseBody::Stream(chunks) => {
            let mut combined = Vec::new();
            for chunk in chunks {
                combined.extend_from_slice(chunk.as_ref());
            }
            std::str::from_utf8(combined.as_slice())
                .map_err(|err| {
                    RuntimeError::RequestHandling(format!(
                        "failed to decode streamed HTML body: {err}"
                    ))
                })?
                .to_string()
        }
    };

    for layout_id in layout_handlers.iter().rev() {
        let layout = state
            .layouts
            .get(layout_id)
            .ok_or_else(|| RuntimeError::LayoutNotFound {
                layout_id: layout_id.clone(),
            })?;
        wrapped_html = layout.wrap(ctx.clone(), wrapped_html).await?;
    }

    response.body = ResponseBody::Full(wrapped_html.into_bytes().into());
    response.headers.insert(
        "content-type".to_string(),
        "text/html; charset=utf-8".to_string(),
    );
    Ok(())
}

fn response_is_html(response: &ResponsePayload) -> bool {
    response
        .headers
        .get("content-type")
        .map(|value| value.to_ascii_lowercase().starts_with("text/html"))
        .unwrap_or(false)
}

async fn shutdown_signal(_timeout: Duration) {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiResponse;
    use crate::config::{RouteSpec, ServerConfig};
    use crate::routing::{AuthPolicy, HttpMethod};
    use axum::body::to_bytes;
    use bytes::Bytes;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_dynamic_route_dispatches_and_reads_param() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "users.show".to_string(),
                method: HttpMethod::Get,
                path: "/users/{id}".to_string(),
                handler: "users.show".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("users.show", |ctx: RequestContext| async move {
                let id = ctx.params.get("id").cloned().unwrap_or_default();
                Ok(ResponsePayload::ok_text(format!("user={id}")))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/users/42?include=profile")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "user=42");
    }

    #[tokio::test]
    async fn test_method_guard_returns_405_with_allow_header() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "users.show".to_string(),
                method: HttpMethod::Get,
                path: "/users/{id}".to_string(),
                handler: "users.show".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("users.show", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_text("ok"))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/users/42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = response
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok());
        assert_eq!(allow, Some("GET"));
    }

    struct DenyAllAuth;

    #[async_trait::async_trait]
    impl AuthProvider for DenyAllAuth {
        async fn authorize(
            &self,
            _ctx: &RequestContext,
            _policy: &AuthPolicy,
        ) -> Result<AuthDecision, RuntimeError> {
            Ok(AuthDecision::Deny {
                reason: "blocked".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn test_auth_policy_blocks_request() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "private".to_string(),
                method: HttpMethod::Get,
                path: "/private".to_string(),
                handler: "private.handler".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: Some(AuthPolicy::Required),
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("private.handler", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_text("secret"))
            })
            .with_auth_provider(DenyAllAuth)
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/private")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_nested_layout_handlers_wrap_html_in_order() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: vec![
                crate::config::LayoutSpec {
                    name: "root".to_string(),
                    path: "/".to_string(),
                    handler: "layout.root".to_string(),
                },
                crate::config::LayoutSpec {
                    name: "dashboard".to_string(),
                    path: "/dashboard".to_string(),
                    handler: "layout.dashboard".to_string(),
                },
            ],
            routes: vec![RouteSpec {
                name: "dashboard.home".to_string(),
                method: HttpMethod::Get,
                path: "/dashboard".to_string(),
                handler: "dashboard.page".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("dashboard.page", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_html("<main>Dashboard</main>"))
            })
            .register_layout(
                "layout.root",
                |_ctx: RequestContext, inner: String| async move {
                    Ok(format!("<html><body>{inner}</body></html>"))
                },
            )
            .register_layout(
                "layout.dashboard",
                |_ctx: RequestContext, inner: String| async move {
                    Ok(format!("<section class=\"dashboard\">{inner}</section>"))
                },
            )
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(
            body,
            "<html><body><section class=\"dashboard\"><main>Dashboard</main></section></body></html>"
        );
    }

    #[tokio::test]
    async fn test_streaming_html_response_chunks_are_emitted() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "stream.page".to_string(),
                method: HttpMethod::Get,
                path: "/stream".to_string(),
                handler: "stream.page".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("stream.page", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_html_stream([
                    Bytes::from_static(b"<main>"),
                    Bytes::from_static(b"ALBEDO"),
                    Bytes::from_static(b"</main>"),
                ]))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok());
        assert_eq!(content_type, Some("text/html; charset=utf-8"));
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "<main>ALBEDO</main>");
    }

    // ── Phase F — API route tests ─────────────────────────────────────

    fn api_route(method: HttpMethod, path: &str, handler: &str, auth: Option<AuthPolicy>) -> RouteSpec {
        RouteSpec {
            name: handler.to_string(),
            method,
            path: path.to_string(),
            handler: handler.to_string(),
            entry_module: None,
            props_loader: None,
            middleware: Vec::new(),
            auth,
        }
    }

    #[tokio::test]
    async fn api_handler_echoes_request_body() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Post, "/api/echo", "echo", None)],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("echo", |ctx: RequestContext| async move {
                Ok(ApiResponse::ok(ctx.body)
                    .with_header("content-type", "application/octet-stream"))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/echo")
                    .body(Body::from("hello-api"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/octet-stream")
        );
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "hello-api");
    }

    #[tokio::test]
    async fn api_handler_returns_json_with_correct_content_type() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Get, "/api/status", "status", None)],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("status", |_ctx: RequestContext| async move {
                ApiResponse::json(&serde_json::json!({ "ok": true, "version": 1 }))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, r#"{"ok":true,"version":1}"#);
    }

    #[tokio::test]
    async fn api_handler_with_required_auth_returns_401_when_denied() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(
                HttpMethod::Get,
                "/api/private",
                "private",
                Some(AuthPolicy::Required),
            )],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("private", |_ctx: RequestContext| async move {
                Ok(ApiResponse::ok(Bytes::from_static(b"secret")))
            })
            .with_auth_provider(DenyAllAuth)
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/private")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "denied auth must surface as 401 on the API path"
        );
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert!(
            !body.as_ref().eq(b"secret"),
            "handler body must never reach the wire when auth denies"
        );
    }

    #[tokio::test]
    async fn api_handler_with_role_auth_runs_when_provider_allows() {
        // Mirrors the Phase-F risk-#9 mitigation test: an admin-only
        // route must invoke the handler when the auth provider says yes.
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(
                HttpMethod::Get,
                "/api/admin",
                "admin",
                Some(AuthPolicy::Role("admin".to_string())),
            )],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("admin", |_ctx: RequestContext| async move {
                Ok(ApiResponse::ok(Bytes::from_static(b"admin-area")))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "admin-area");
    }

    #[tokio::test]
    async fn api_handler_method_mismatch_returns_405() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Get, "/api/users", "users.list", None)],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_api_handler("users.list", |_ctx: RequestContext| async move {
                Ok(ApiResponse::ok(Bytes::from_static(b"[]")))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = response
            .headers()
            .get("allow")
            .and_then(|v| v.to_str().ok());
        assert_eq!(allow, Some("GET"));
    }

    // ── Phase G — action route tests ──────────────────────────────────

    #[tokio::test]
    async fn action_route_dispatches_and_returns_wire_encoded_opcode_frame() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::{Instruction, StableId, TagId};
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };

        let server = AlbedoServerBuilder::new(config)
            .register_action(
                42,
                |_ctx: RequestContext,
                 envelope: dom_render_compiler::ir::action::ActionEnvelope,
                 _slots: SessionSlots| async move {
                    // Handler returns one Create that targets the action_id
                    // as its stable_id so the test can verify the args
                    // reached the handler unmodified.
                    Ok(vec![Instruction::Create {
                        tag_id: TagId(0),
                        stable_id: StableId(envelope.action_id),
                    }])
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 42,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).expect("response decodes as OpcodeFrame");
        assert!(matches!(
            frame.instructions[0],
            Instruction::Create { stable_id: StableId(42), .. }
        ));
    }

    #[tokio::test]
    async fn action_route_returns_404_for_unregistered_action_id() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config).build().unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 99,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn action_route_carries_request_context_to_handler() {
        // Verifies the handler sees the headers from the originating
        // request — Phase H / I will lean on this for CSRF tokens and
        // session-bearing cookies.
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                7,
                |ctx: RequestContext,
                 _env: dom_render_compiler::ir::action::ActionEnvelope,
                 _slots: SessionSlots| async move {
                    // Echo the token header back via SetText so the test
                    // can read it from the decoded response.
                    let token = ctx
                        .headers
                        .get("x-albedo-session")
                        .cloned()
                        .unwrap_or_default();
                    Ok(vec![dom_render_compiler::ir::opcode::Instruction::SetText {
                        stable_id: dom_render_compiler::ir::opcode::StableId(1),
                        text: token.into_bytes(),
                    }])
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 7,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", "sess-abc")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = dom_render_compiler::ir::wire::decode_frame(&bytes).unwrap();
        match &frame.instructions[0] {
            dom_render_compiler::ir::opcode::Instruction::SetText { text, .. } => {
                assert_eq!(text.as_slice(), b"sess-abc");
            }
            other => panic!("expected SetText, got {other:?}"),
        }
    }

    // ── Phase H — reactive slot store integration ─────────────────────

    #[tokio::test]
    async fn slot_state_persists_across_two_action_invocations_in_the_same_session() {
        // The Phase-H closing loop: action A writes a slot, action B
        // reads the same slot for the same session and gets the value
        // back. Distinct sessions stay isolated.
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::SlotId;
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };

        // action_id 1 — writer: stores the payload bytes into slot 7.
        // action_id 2 — reader: emits a `SetText` carrying whatever's
        // currently in slot 7. Empty body when the slot is unset.
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                1,
                |_ctx: RequestContext, env: ActionEnvelope, slots: SessionSlots| async move {
                    slots.write(SlotId(7), env.payload.clone());
                    Ok(Vec::new())
                },
            )
            .register_action(
                2,
                |_ctx: RequestContext, _env: ActionEnvelope, slots: SessionSlots| async move {
                    let current = slots.read(SlotId(7)).unwrap_or_default();
                    Ok(vec![dom_render_compiler::ir::opcode::Instruction::SetText {
                        stable_id: dom_render_compiler::ir::opcode::StableId(1),
                        text: current,
                    }])
                },
            )
            .build()
            .unwrap();

        let session_uuid = uuid::Uuid::new_v4().to_string();
        let router = server.router();

        // First POST — action 1 writes "hello-world" into slot 7.
        let write_body = encode_action_envelope(&ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: b"hello-world".to_vec(),
        })
        .unwrap();
        let write_response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .body(Body::from(write_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(write_response.status(), StatusCode::OK);
        // The write itself produced a SlotSet via the dirty drain.
        let write_bytes = to_bytes(write_response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (write_frame, _) = decode_frame(&write_bytes).unwrap();
        assert!(write_frame.instructions.iter().any(|instr| matches!(
            instr,
            dom_render_compiler::ir::opcode::Instruction::SlotSet { slot_id: SlotId(7), value }
                if value == b"hello-world"
        )));

        // Second POST — action 2 reads slot 7 for the same session and
        // emits the value back as the SetText payload.
        let read_body = encode_action_envelope(&ActionEnvelope {
            action_id: 2,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();
        let read_response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_uuid.as_str())
                    .body(Body::from(read_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(read_response.status(), StatusCode::OK);
        let read_bytes = to_bytes(read_response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (read_frame, _) = decode_frame(&read_bytes).unwrap();
        match &read_frame.instructions[0] {
            dom_render_compiler::ir::opcode::Instruction::SetText { text, .. } => {
                assert_eq!(
                    text.as_slice(),
                    b"hello-world",
                    "slot state must survive across action invocations within a session"
                );
            }
            other => panic!("expected SetText, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slot_state_is_isolated_across_distinct_sessions() {
        // Same reader action, two different session ids → reads return
        // independent (empty) state.
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::SlotId;
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                1,
                |_ctx: RequestContext, env: ActionEnvelope, slots: SessionSlots| async move {
                    slots.write(SlotId(7), env.payload.clone());
                    Ok(Vec::new())
                },
            )
            .register_action(
                2,
                |_ctx: RequestContext, _env: ActionEnvelope, slots: SessionSlots| async move {
                    let current = slots.read(SlotId(7)).unwrap_or_default();
                    Ok(vec![dom_render_compiler::ir::opcode::Instruction::SetText {
                        stable_id: dom_render_compiler::ir::opcode::StableId(1),
                        text: current,
                    }])
                },
            )
            .build()
            .unwrap();

        let router = server.router();
        let session_a = uuid::Uuid::new_v4().to_string();
        let session_b = uuid::Uuid::new_v4().to_string();

        // Write under session A.
        let write_body = encode_action_envelope(&ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: b"a-only".to_vec(),
        })
        .unwrap();
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_a.as_str())
                    .body(Body::from(write_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Read under session B — must NOT see session A's value.
        let read_body = encode_action_envelope(&ActionEnvelope {
            action_id: 2,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .header("x-albedo-session", session_b.as_str())
                    .body(Body::from(read_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).unwrap();
        match &frame.instructions[0] {
            dom_render_compiler::ir::opcode::Instruction::SetText { text, .. } => {
                assert!(
                    text.is_empty(),
                    "session B must not see session A's slot value; got {:?}",
                    String::from_utf8_lossy(text)
                );
            }
            other => panic!("expected SetText, got {other:?}"),
        }
    }

    // ── Phase I — Navigate opcode + register_form_action ─────────────

    #[tokio::test]
    async fn action_handler_can_emit_navigate_opcode() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::Instruction;
        use dom_render_compiler::ir::wire::decode_frame;

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_action(
                1,
                |_ctx: RequestContext, _env: ActionEnvelope, _slots: SessionSlots| async move {
                    Ok(vec![Instruction::Navigate {
                        url: "/dashboard".to_string(),
                    }])
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: Vec::new(),
        })
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).unwrap();
        assert!(
            matches!(
                &frame.instructions[0],
                Instruction::Navigate { url } if url == "/dashboard"
            ),
            "Phase-I Navigate must round-trip through the action response wire path"
        );
    }

    #[tokio::test]
    async fn register_form_action_deserialises_json_payload_into_typed_struct() {
        use crate::render::form_action::form_action_id;
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use dom_render_compiler::ir::opcode::{Instruction, StableId};
        use dom_render_compiler::ir::wire::decode_frame;
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct LoginForm {
            username: String,
            password: String,
        }

        // Phase L · `register_form_action` now takes the action
        // name; the builder derives the wire-level `action_id` via
        // FNV-1a-32. The envelope below uses the same hash so the
        // dispatcher routes the request to the registered handler.
        const ACTION_NAME: &str = "submit_login";

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_form_action::<LoginForm, _, _>(
                ACTION_NAME,
                |_ctx: RequestContext, form: LoginForm, _slots: SessionSlots| async move {
                    // Echo the username back so the test can verify the
                    // typed payload made it through unchanged.
                    Ok(vec![
                        Instruction::SetText {
                            stable_id: StableId(1),
                            text: form.username.into_bytes(),
                        },
                        Instruction::Navigate {
                            url: format!("/welcome?ack={}", form.password.len()),
                        },
                    ])
                },
            )
            .build()
            .unwrap();

        let form_payload = serde_json::to_vec(&serde_json::json!({
            "username": "alice",
            "password": "hunter2",
        }))
        .unwrap();
        let body = encode_action_envelope(&ActionEnvelope {
            action_id: form_action_id(ACTION_NAME),
            event_kind: 2, // Submit
            payload: form_payload,
        })
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        let (frame, _) = decode_frame(&bytes).unwrap();
        match &frame.instructions[0] {
            Instruction::SetText { text, .. } => {
                assert_eq!(text.as_slice(), b"alice");
            }
            other => panic!("expected SetText, got {other:?}"),
        }
        match &frame.instructions[1] {
            Instruction::Navigate { url } => {
                assert_eq!(url, "/welcome?ack=7");
            }
            other => panic!("expected Navigate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_form_action_rejects_malformed_json_with_500() {
        use crate::render::form_action::form_action_id;
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct Required {
            #[allow(dead_code)]
            field: String,
        }

        // Phase L · action name resolves to a stable `action_id` on
        // both ends; the envelope below uses the same hash so the
        // dispatcher finds the handler even though the payload will
        // fail to parse.
        const ACTION_NAME: &str = "malformed_required";

        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: Vec::new(),
        };
        let server = AlbedoServerBuilder::new(config)
            .register_form_action::<Required, _, _>(
                ACTION_NAME,
                |_ctx: RequestContext, _form: Required, _slots: SessionSlots| async move {
                    panic!("handler must not run when payload fails to deserialize");
                },
            )
            .build()
            .unwrap();

        let body = encode_action_envelope(&ActionEnvelope {
            action_id: form_action_id(ACTION_NAME),
            event_kind: 2,
            payload: b"not json".to_vec(),
        })
        .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/_albedo/action")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn missing_api_handler_id_fails_build() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![api_route(HttpMethod::Get, "/api/missing", "missing", None)],
        };

        // No api_handler registered for "missing" — build must reject.
        // `unwrap_err` would require AlbedoServer: Debug, so match by hand.
        match AlbedoServerBuilder::new(config).build() {
            Err(RuntimeError::HandlerNotFound { handler_id }) => {
                assert_eq!(handler_id, "missing");
            }
            Err(other) => panic!("expected HandlerNotFound, got {other:?}"),
            Ok(_) => panic!("build must reject a route with no registered handler"),
        }
    }
}
