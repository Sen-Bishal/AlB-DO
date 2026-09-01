use crate::error::RuntimeError;
use crate::render::tier_b::{
    render_tier_b, render_tier_b_opcodes, stable_id_for_placeholder, InjectionChunk,
    RequestContext as TierBRequestContext, SharedRenderServices,
};
use crate::webtransport::WebTransportSessionRegistry;
use async_stream::stream;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode, Version};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use dom_render_compiler::ir::opcode::InternTableKind;
use dom_render_compiler::manifest::schema::{
    HydrationMode, RenderManifestV2, RouteManifest, TierBNode,
};
use dom_render_compiler::runtime::pipeline::{FourLaneRuntimePipeline, RuntimePipelineError};
use dom_render_compiler::runtime::webtransport::{
    FramePayload, LaneRenderedChunk, WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES,
    WT_STREAM_SLOT_PREFETCH, WT_STREAM_SLOT_SHELL,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::time::{timeout, Duration};
use tracing::{debug, warn};
use uuid::Uuid;

/// Shared handle to the opcode pipeline that produces binary frames for
/// the bakabox client.
///
/// `Mutex` rather than `RwLock`: the hot tick path mutates the pipeline
/// (dirty bitmap drain, scratch buffers) and is the only writer; the
/// uncontended fast path through `Mutex::lock` is single-instruction on
/// modern targets. `Arc` wraps it because [`StreamingAppState`] is cloned
/// into every request future.
pub type SharedPipeline = Arc<Mutex<FourLaneRuntimePipeline>>;

const WT_SESSION_HEADER: &str = "x-albedo-wt-session";
const WT_PREFER_HEADER: &str = "x-albedo-wt-prefer";

#[derive(Clone)]
pub struct StreamingAppState {
    pub manifest: Arc<RenderManifestV2>,
    pub services: SharedRenderServices,
    pub transport: StreamingTransportConfig,
    pub webtransport_sessions: Option<WebTransportSessionRegistry>,
    /// Optional opcode pipeline. Populated by
    /// [`Self::with_pipeline`] during server bootstrap or test setup.
    /// `None` means the streaming path falls back to the legacy JSON
    /// tier-B render — used by tests that don't exercise the binary wire
    /// and by environments that haven't yet plumbed a renderer.
    pipeline: Option<SharedPipeline>,
    /// Phase L · per-session CSRF registry shared with the action
    /// dispatcher's [`crate::server::RuntimeState`]. The page-render
    /// path consults this to fill the empty `value=""` placeholders
    /// the renderer stamps for form-action elements. Defaults to a
    /// fresh empty registry so tests that don't exercise CSRF compile
    /// and run unchanged; production wires it via [`Self::with_csrf`].
    csrf: Arc<crate::render::csrf::CsrfRegistry>,
    /// Phase P · Stream C.4 — broadcast registry shared with the
    /// action dispatcher's adapter and the per-server `RuntimeState`.
    /// The streaming handler calls `auto_subscribe` on this when a
    /// WT session establishes, so the session immediately receives
    /// `SlotSet` opcodes for every topic the route's JSX referenced
    /// via `useSharedSlot`. `None` for tests / configurations that
    /// don't wire a registry — `auto_subscribe` is skipped in that
    /// case rather than erroring.
    broadcast: Option<Arc<dom_render_compiler::runtime::BroadcastRegistry>>,
    /// A3 · per-route client-hydration blocks precomputed at boot by
    /// [`crate::renderer_runtime::RendererRuntime::build_hydration_blocks`].
    /// `build_stream` fills each Tier-C placeholder with the island's marked
    /// SSR HTML and emits the client runtime + island IIFEs + payload +
    /// bootstrap before `</body>`. Empty for tests / Tier-A-only builds.
    hydration: Arc<HashMap<String, crate::renderer_runtime::RouteHydration>>,
    /// Dev mode — when set, `build_stream` injects the error-overlay + HMR
    /// client `<script>` tags (served at `/_albedo/dev/*`) before `</body>`, so
    /// `albedo dev` runs the SAME production streaming pipeline as `albedo serve`
    /// and gets the overlay + slot-preserving hot reload on top. `false` for
    /// `albedo serve` and tests.
    dev_mode: bool,
}

impl StreamingAppState {
    pub fn new(
        manifest: Arc<RenderManifestV2>,
        services: SharedRenderServices,
        transport: StreamingTransportConfig,
        webtransport_sessions: Option<WebTransportSessionRegistry>,
    ) -> Self {
        Self {
            manifest,
            services,
            transport,
            webtransport_sessions,
            pipeline: None,
            csrf: Arc::new(crate::render::csrf::CsrfRegistry::new()),
            broadcast: None,
            hydration: Arc::new(HashMap::new()),
            dev_mode: false,
        }
    }

    /// Enable dev mode on this streaming state so `build_stream` injects the
    /// error-overlay + HMR client scripts. Wired by the server builder from its
    /// `dev_mode_enabled` flag; `albedo serve` leaves it off.
    #[must_use]
    pub fn with_dev_mode(mut self, dev_mode: bool) -> Self {
        self.dev_mode = dev_mode;
        self
    }

    /// A3 · bind the per-route hydration blocks built at boot. Production wires
    /// the map [`RendererRuntime::build_hydration_blocks`] returns; tests and
    /// Tier-A-only builds leave it empty (default), and `build_stream` simply
    /// emits no client-hydration scripts.
    #[must_use]
    pub fn with_hydration(
        mut self,
        hydration: Arc<HashMap<String, crate::renderer_runtime::RouteHydration>>,
    ) -> Self {
        self.hydration = hydration;
        self
    }

    /// Phase P · Stream C.4 — bind a broadcast registry to this
    /// streaming state. Production wires the **same** `Arc` the
    /// `RuntimeState` action dispatcher holds (and the
    /// `CompiledProjectActionAdapter` clones into each registered
    /// action handler). When set, the streaming handler calls
    /// `auto_subscribe` per WT session connect against this
    /// registry; when unset, the auto-subscribe pass is skipped.
    #[must_use]
    pub fn with_broadcast(
        mut self,
        broadcast: Arc<dom_render_compiler::runtime::BroadcastRegistry>,
    ) -> Self {
        self.broadcast = Some(broadcast);
        self
    }

    /// Phase P · Stream C.4 — accessor for the bound broadcast
    /// registry, used by the streaming handler's WT path and by
    /// tests that want to seed topic values before the session
    /// connects.
    pub fn broadcast(
        &self,
    ) -> Option<&Arc<dom_render_compiler::runtime::BroadcastRegistry>> {
        self.broadcast.as_ref()
    }

    /// Binds a shared CSRF registry to this streaming state. Production
    /// wires the **same** `Arc<CsrfRegistry>` here that the
    /// `RuntimeState` action dispatcher holds, so the per-session
    /// tokens minted during page render are the ones the action route
    /// validates against. Without this call the streaming state runs
    /// with a fresh empty registry — fine for tests, broken for
    /// end-to-end CSRF.
    #[must_use]
    pub fn with_csrf(mut self, csrf: Arc<crate::render::csrf::CsrfRegistry>) -> Self {
        self.csrf = csrf;
        self
    }

    /// Returns the shared CSRF registry handle. Used by the
    /// streaming handler to mint or look up tokens per request, and
    /// exposed for tests that want to pre-populate tokens.
    pub fn csrf(&self) -> &Arc<crate::render::csrf::CsrfRegistry> {
        &self.csrf
    }

    /// Binds an opcode pipeline to this streaming state.
    ///
    /// The pipeline is consumed and bound to `runtime_handle` (so Phase-D
    /// async-island spawn paths can find a runtime context without
    /// panicking on `Handle::current()`), wrapped in `Arc<Mutex<_>>`, and
    /// stashed for the lifetime of the streaming app state.
    ///
    /// Returns `self` so this composes with [`Self::new`] in a single
    /// builder expression.
    #[must_use]
    pub fn with_pipeline(
        mut self,
        pipeline: FourLaneRuntimePipeline,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let pipeline = pipeline.with_runtime_handle(runtime_handle);
        self.pipeline = Some(Arc::new(Mutex::new(pipeline)));
        self
    }

    /// Returns the shared pipeline handle, or `None` when no pipeline is
    /// bound.
    pub fn pipeline(&self) -> Option<&SharedPipeline> {
        self.pipeline.as_ref()
    }

    /// Returns `true` if an opcode pipeline has been bound. Used by the
    /// streaming handler to choose between the binary opcode path and the
    /// legacy JSON tier-B render.
    pub fn has_pipeline(&self) -> bool {
        self.pipeline.is_some()
    }
}

// ── Pipeline tick + chunk helpers ────────────────────────────────────────
//
// Phase B-finish wire surface: the streaming handler talks to the pipeline
// through these free functions, never through raw `Mutex::lock`. Each
// function has one job; failures map to typed `RuntimeError` so the axum
// handler can `into_response()` them uniformly.

/// Drives one reconciliation tick on the bound pipeline and returns the
/// binary opcode chunks that resulted.
///
/// Returns an empty `Vec` when no pipeline is bound. Synchronous — the
/// underlying `Mutex` is held for the duration of the tick, which must
/// not span an `.await`. Callers in an async context should wrap this in
/// [`tokio::task::spawn_blocking`]; the tick itself is sub-millisecond on
/// the hot path so the blocking-pool round-trip is the dominant cost.
pub fn drive_pipeline_tick(state: &StreamingAppState) -> Vec<LaneRenderedChunk> {
    let Some(pipeline) = state.pipeline.as_ref() else {
        return Vec::new();
    };
    let Ok(mut guard) = pipeline.lock() else {
        // Mutex poisoning means an earlier tick panicked. The pipeline
        // is in an indeterminate state; the safest move is to skip this
        // tick and let the supervising layer rebuild. Returning empty
        // is the correct wire-level answer — no frames, no harm.
        warn!("opcode pipeline mutex poisoned; tick skipped");
        return Vec::new();
    };
    guard.tick_frame();
    guard.drain_opcode_chunks()
}

/// Produces the one-shot bootstrap intern table chunk for a fresh bakabox
/// session.
///
/// Call exactly once per new WT session, immediately after
/// `session_init`. Subsequent reconciliation rounds should use
/// [`drain_pipeline_intern_patches`] instead — calling this twice would
/// re-bootstrap, clobbering the client's intern mirror.
///
/// `classify` decides which interned strings ship as part of which kind
/// (Tag / Attr / Event). The renderer owns this mapping; the streaming
/// layer just threads it through.
pub fn drain_pipeline_bootstrap<F>(
    state: &StreamingAppState,
    classify: F,
) -> Result<Option<LaneRenderedChunk>, RuntimePipelineError>
where
    F: Fn(u16, &str) -> Option<InternTableKind>,
{
    let Some(pipeline) = state.pipeline.as_ref() else {
        return Ok(None);
    };
    let mut guard = pipeline
        .lock()
        .map_err(|_| RuntimePipelineError::MissingRuntimeHandle)?;
    guard.drain_bootstrap_intern_chunk(classify)
}

/// Produces the incremental intern table patch chunk, if any, since the
/// previous reconciliation.
///
/// Returns `Ok(None)` when nothing in the intern table has changed —
/// callers should skip the send in that case to keep the control stream
/// quiet during steady-state ticks.
pub fn drain_pipeline_intern_patches<F>(
    state: &StreamingAppState,
    classify: F,
) -> Result<Option<LaneRenderedChunk>, RuntimePipelineError>
where
    F: Fn(u16, &str) -> Option<InternTableKind>,
{
    let Some(pipeline) = state.pipeline.as_ref() else {
        return Ok(None);
    };
    let mut guard = pipeline
        .lock()
        .map_err(|_| RuntimePipelineError::MissingRuntimeHandle)?;
    guard.drain_intern_table_patches(classify)
}

/// Forwards a batch of [`LaneRenderedChunk`]s to the bakabox client over
/// the WebTransport session, one `send_payload` per chunk.
///
/// The chunk's `lane` field selects the WT stream slot. `FramePayload::Text`
/// payloads are sent UTF-8 encoded as-is so existing JSON consumers (the
/// shell, prefetch) keep working alongside binary opcode chunks.
///
/// Returns `Ok(())` when the session has no WT registry (server has
/// WebTransport disabled) — the streaming handler will fall back to SSE.
pub async fn ship_chunks_to_session(
    state: &StreamingAppState,
    session_id: Uuid,
    chunks: Vec<LaneRenderedChunk>,
) -> Result<(), RuntimeError> {
    let Some(sessions) = state.webtransport_sessions.as_ref() else {
        return Ok(());
    };
    for chunk in chunks {
        let payload = match chunk.payload {
            FramePayload::Binary(bytes) => bytes,
            FramePayload::Text(text) => text.into_bytes(),
        };
        sessions
            .send_payload(session_id, chunk.lane as u8, payload)
            .await?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct StreamingTransportConfig {
    pub webtransport_enabled: bool,
    pub webtransport_path: String,
    pub alt_svc: Option<String>,
}

impl StreamingTransportConfig {
    pub fn new(webtransport_enabled: bool, port: u16) -> Self {
        let alt_svc = webtransport_enabled.then(|| format!("h3=\":{port}\"; ma=86400"));
        Self {
            webtransport_enabled,
            webtransport_path: "/_albedo/wt".to_string(),
            alt_svc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NegotiatedTransport {
    WebTransport,
    Sse,
}

impl NegotiatedTransport {
    fn as_header_value(self) -> &'static str {
        match self {
            Self::WebTransport => "webtransport",
            Self::Sse => "sse",
        }
    }
}

pub async fn streaming_handler(
    State(app): State<Arc<StreamingAppState>>,
    req: Request,
) -> impl IntoResponse {
    let path = req.uri().path().to_string();

    // WebTransport capability probe — answered without a route or params.
    if path == app.transport.webtransport_path {
        let negotiated_transport = negotiate_transport(&req, &app.transport);
        return webtransport_capability_response(app.as_ref(), negotiated_transport);
    }

    // Standalone / non-dispatched entry: the manifest is keyed by route
    // pattern, so an exact-path lookup only resolves static routes. Dynamic
    // routes flow through `streaming_handler_with_match` instead, which
    // carries the matched pattern + params resolved by `CompiledRouter`.
    serve_manifest_route(app, req, path, HashMap::new(), None).await
}

/// Dispatch-path entry for the manifest-streaming arm. The caller has already
/// matched the request through [`crate::routing::CompiledRouter`], so it passes
/// the resolved manifest key (`route_pattern`, e.g. `/essays/[slug]`) and the
/// extracted route `params` (e.g. `{ slug: "..." }`) directly — this is what
/// makes dynamic `[slug]` routes render their async body + per-request
/// `generateMetadata()` on serve.
pub async fn streaming_handler_with_match(
    app: Arc<StreamingAppState>,
    req: Request,
    route_pattern: String,
    params: HashMap<String, String>,
    // AUTH · the request's principal, resolved by the dispatcher. P0 carried it
    // this far; **P1 is what reads it** — `user.id` is a key source now, so this
    // page's topics depend on who asked for it.
    identity: crate::auth::Identity,
) -> Response {
    if let Some(who) = identity.principal() {
        tracing::debug!(
            target: "albedo.auth",
            principal = %who.id,
            route = %route_pattern,
            "page rendered under a resolved principal"
        );
    }
    let principal = identity.principal().map(|who| who.id.clone());
    serve_manifest_route(app, req, route_pattern, params, principal).await
}

/// Shared body of the streaming handler. `route_pattern` is the manifest key to
/// render; `params` are the parsed dynamic-segment values threaded into the
/// Tier-B [`TierBRequestContext`] so `ctx.resolve("params")` / dynamic props /
/// `generateMetadata()` see them.
async fn serve_manifest_route(
    app: Arc<StreamingAppState>,
    req: Request,
    route_pattern: String,
    params: HashMap<String, String>,
    // AUTH item 5 P1 · `None` on the standalone entry below, which is not a
    // dispatched request and therefore has no resolved session. That is the
    // correct value rather than a shortcut: an undispatched render must not
    // invent an identity, and an identity-keyed binding on it resolves to no
    // topic, which is the same answer an anonymous visitor gets.
    principal: Option<dom_render_compiler::auth::PrincipalId>,
) -> Response {
    let path = req.uri().path().to_string();
    let negotiated_transport = negotiate_transport(&req, &app.transport);

    let Some(route) = app.manifest.routes.get(route_pattern.as_str()) else {
        return not_found_response();
    };

    let transport_config = app.transport.clone();
    let mut response_transport = negotiated_transport;
    let route = route.clone();
    let ctx = request_context_from_request(&req, params, principal);

    // Phase L · resolve the per-session id used to address the CSRF
    // token table. Read from the `__Host-albedo-session` cookie when the
    // browser carries one; mint a fresh id otherwise. We track
    // `is_fresh_session` so we know whether to emit a Set-Cookie on
    // the response — repeat visits don't pay the header cost.
    let (page_session, is_fresh_session) =
        match crate::render::csrf::read_session_cookie(req.headers()) {
            Some(existing) => (existing, false),
            None => (
                dom_render_compiler::runtime::SessionId::random(),
                true,
            ),
        };

    if negotiated_transport == NegotiatedTransport::WebTransport {
        match maybe_webtransport_session_id(&req) {
            Some(session_id) => {
                match stream_route_over_webtransport(
                    route.clone(),
                    ctx.clone(),
                    app.clone(),
                    session_id,
                )
                .await
                {
                    Ok(()) => {
                        // `debug`, not `info`: this fires on every streamed
                        // route, so at `info` it buries the once-per-boot and
                        // once-per-session lines someone turned logging on for.
                        debug!(
                            session_id = %session_id,
                            route = %path,
                            transport = "webtransport",
                            "route streamed over webtransport"
                        );
                        return webtransport_ack_response(&transport_config);
                    }
                    Err(err) => {
                        warn!(
                            session_id = %session_id,
                            route = %path,
                            error = %err,
                            "webtransport stream bridge failed; falling back to sse"
                        );
                        response_transport = NegotiatedTransport::Sse;
                    }
                }
            }
            None => {
                warn!(
                    route = %path,
                    "webtransport negotiated without session id header; falling back to sse"
                );
                response_transport = NegotiatedTransport::Sse;
            }
        }
    }

    let stream = build_stream(route, ctx, app, response_transport, page_session);

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::TRANSFER_ENCODING, "chunked")
        .header("x-content-type-options", "nosniff")
        .header("cache-control", "no-store")
        .header("x-albedo-transport", response_transport.as_header_value());

    // Phase L · pin the session id in a cookie the first time we
    // see this browser so subsequent action POSTs route back to the
    // same CsrfRegistry entry.
    if is_fresh_session {
        response = response.header(
            header::SET_COOKIE,
            crate::render::csrf::build_session_set_cookie(page_session),
        );
    }

    if let Some(alt_svc) = transport_config.alt_svc {
        response = response.header("alt-svc", alt_svc);
    }

    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| Response::new(Body::from("failed to build streaming response")))
}

fn webtransport_capability_response(
    app: &StreamingAppState,
    negotiated_transport: NegotiatedTransport,
) -> Response {
    let payload = json!({
        "transport": negotiated_transport.as_header_value(),
        "webtransport_enabled": app.transport.webtransport_enabled,
        "webtransport_path": app.transport.webtransport_path,
        "active_sessions": app
            .webtransport_sessions
            .as_ref()
            .map(WebTransportSessionRegistry::count)
            .unwrap_or(0),
    });

    let body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header("cache-control", "no-store")
        .header("x-albedo-transport", negotiated_transport.as_header_value());

    if let Some(alt_svc) = app.transport.alt_svc.as_ref() {
        response = response.header("alt-svc", alt_svc);
    }

    response
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::from("{}")))
}

fn webtransport_ack_response(transport: &StreamingTransportConfig) -> Response {
    let mut response = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header("cache-control", "no-store")
        .header(
            "x-albedo-transport",
            NegotiatedTransport::WebTransport.as_header_value(),
        );

    if let Some(alt_svc) = transport.alt_svc.as_ref() {
        response = response.header("alt-svc", alt_svc);
    }

    response
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Render a route boundary component (`error.tsx` / `loading.tsx`) to HTML
/// through the same warmed Tier-B registry that renders async server
/// components. Returns `None` when the route declares no such boundary, or
/// when rendering it fails (logged) — the caller then falls back to the
/// generic stub. Boundary components are registered in the boot
/// `TierBRenderPlan` keyed by their bare component name (see
/// `RendererRuntime::build_tier_b_render_plan`).
async fn render_route_boundary(
    app: &StreamingAppState,
    component: Option<&str>,
    props: &serde_json::Value,
) -> Option<String> {
    let name = component?;
    match app
        .services
        .registry
        .call(name, props, &HashMap::new())
        .await
    {
        Ok(html) => Some(html),
        Err(err) => {
            warn!(
                target: "albedo.render",
                component = %name,
                error = %err,
                "route boundary render failed; using generic fallback"
            );
            None
        }
    }
}

/// Everything a Tier-B node needs from the request, gathered once so the two
/// passes below can share one resolver rather than growing two copies of the
/// timeout / error-boundary / loading-boundary ladder.
struct TierBResolveCtx<'a> {
    app: &'a Arc<StreamingAppState>,
    ctx: &'a TierBRequestContext,
    error_component: Option<String>,
    loading_component: Option<String>,
    island_fills: std::sync::Arc<Vec<(String, String)>>,
    csrf_token: std::sync::Arc<str>,
    return_path: std::sync::Arc<str>,
}

/// Resolve one Tier-B node to the chunk that represents it — success, the
/// route's `error.tsx`, the route's `loading.tsx`, or the blank error stub.
///
/// This is the single implementation of that ladder. It is called from **two**
/// places that differ only in what they do with the result: the inline pass
/// paints the chunk into the shell before the response starts, and the streamed
/// pass ships it as a `<script>__albedo_inject(…)</script>` after. Keeping one
/// resolver is the point — an error path that only one of them handled would be
/// a difference nobody notices until the boundary fails to render on whichever
/// path is rarer.
async fn resolve_tier_b_node(node: TierBNode, shared: &TierBResolveCtx<'_>) -> InjectionChunk {
    let render_result = timeout(
        Duration::from_millis(node.timeout_ms.max(1)),
        render_tier_b(
            &node,
            shared.ctx,
            shared.app.services.registry.as_ref(),
            shared.app.services.data_fetcher.as_ref(),
        ),
    )
    .await;

    // Every arm below that carries component-rendered markup goes through the
    // same fill as the shell — including the boundaries, since an `error.tsx`
    // may well render a retry form. The two stub arms (`error` / `fallback`)
    // build their own markup from a constant and have nothing to fill.
    let fill = |html: String| {
        fill_server_placeholders(
            html,
            &shared.island_fills,
            &shared.csrf_token,
            &shared.return_path,
        )
    };

    match render_result {
        Ok(Ok(html)) => InjectionChunk::success(&node, fill(html)),
        Ok(Err(err)) => {
            // Logged before anything is decided about the response. Without
            // this a throwing Tier-B node with no `error.tsx` produced a blank
            // placeholder and **nothing anywhere** — the diagnostic chain the
            // error carries for exactly this purpose was built and then dropped
            // on the floor.
            warn!(
                target: "albedo.render",
                node = %node.placeholder_id,
                error = %err,
                "tier-B component failed to render"
            );
            // The component threw. Render the route's `error.tsx` boundary and
            // use its HTML; only if there is no boundary (or it too fails) do we
            // fall back to the blank error stub.
            //
            // Reader-facing copy: the raw thrown message only, never the wrapped
            // diagnostic chain or a filesystem path. The full `err` still
            // reaches logs/overlay (Display) on the boundary-render failure path.
            let error_props = json!({
                "error": { "message": err.user_message() }
            });
            match render_route_boundary(
                shared.app.as_ref(),
                shared.error_component.as_deref(),
                &error_props,
            )
            .await
            {
                Some(html) => InjectionChunk::error_boundary(&node, fill(html)),
                None => InjectionChunk::error(&node, err),
            }
        }
        Err(_) => {
            // The component timed out. Prefer the route's `loading.tsx` UI over
            // the generic timeout div.
            match render_route_boundary(
                shared.app.as_ref(),
                shared.loading_component.as_deref(),
                &json!({}),
            )
            .await
            {
                Some(html) => InjectionChunk::fallback_with_html(&node, fill(html)),
                None => InjectionChunk::fallback(&node),
            }
        }
    }
}

/// Which Tier-B nodes are resolved *before* the shell is sent, and which stream
/// in after it.
///
/// ## The rule, and why it is this one
///
/// A node is painted inline when its render needs **no external data**
/// (`data_deps.is_empty()`). That render is a function of the request's props
/// and state this process already holds — a materialized FORGE topic, the
/// resolved principal — so it is CPU-bound and local, and awaiting it is the
/// ordinary server-render cost every non-streaming framework pays.
///
/// A node **with** `data_deps` declares an external fetch. That is the case
/// streaming exists for, and it keeps streaming: blocking the first byte on a
/// remote host is the thing the shell-first design is protecting.
///
/// ## What this replaced, and why the old cut was wrong
///
/// The previous rule seeded a placeholder only when the node had **neither**
/// dynamic props nor data deps, and it seeded the HTML the *build* had rendered.
/// Both halves of that were forced by the same limitation: build-time HTML saw
/// no request, so any node needing one had to be left empty. The consequence was
/// that a route reading `user` or `params` — every auth-aware page, and every
/// `[id]` route — reached the browser with its content existing **only as a
/// JavaScript string argument**, so a reader without JS got a blank page and a
/// crawler that does not execute scripts got nothing.
///
/// Rendering on the request path removes the reason for the narrower cut: there
/// *is* a request now, so dynamic props are no longer a disqualifier. It also
/// makes the painted markup strictly fresher than the build-time seed it
/// supersedes, which could be stale for a collection that had gained rows.
fn painted_inline(node: &TierBNode) -> bool {
    node.data_deps.is_empty()
}

fn build_stream(
    route: RouteManifest,
    ctx: TierBRequestContext,
    app: Arc<StreamingAppState>,
    negotiated_transport: NegotiatedTransport,
    page_session: dom_render_compiler::runtime::SessionId,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    use dom_render_compiler::manifest::metadata::{
        lower_metadata_object, render_head_metadata, DYNAMIC_HEAD_MARKER,
    };
    stream! {
        let hydration = app.hydration.get(route.route.as_str());

        // Phase L · one token for the whole response. Minted here rather than
        // per chunk because the shell and every Tier-B chunk must agree — a
        // form in the shell and a form in an async page carry the same session's
        // token. `token_for` mints on first read and is stable per session, so
        // this is also one fewer registry hit per chunk.
        let csrf_token = app.csrf().token_for(page_session);

        // The path a JS-less form submit returns to. `ctx.path` and not the
        // route *pattern*: a form on `/room/42` must send its reader back to
        // `/room/42`, not to `/room/[id]`.
        let return_path: std::sync::Arc<str> = std::sync::Arc::from(ctx.path.as_str());

        // Route boundary component names (`error.tsx` / `loading.tsx`), if any,
        // and the island fill data — hoisted above the shell because the inline
        // pass below runs *before* it and needs the same values the streamed
        // pass does.
        let error_component = route.error_component.clone();
        let loading_component = route.loading_component.clone();

        // Island fill data for this route, shared across the Tier-B futures. An
        // island nested in an `async function Page()` renders (via its client
        // reference stub) to the empty placeholder inside the page's HTML; this
        // is the same marked SSR markup the shell fill uses, applied to the
        // resolved chunk so async-page islands are byte-identical to Tier-A ones.
        let island_fills: std::sync::Arc<Vec<(String, String)>> =
            std::sync::Arc::new(hydration.map(|h| h.placeholders.clone()).unwrap_or_default());

        // Shared with each Tier-B future so its resolved HTML goes through the
        // same fill as the shell. Without this a `<form action="action:NAME">`
        // inside an `async function Page()` reaches the browser with
        // `value=""` and the submit arrives tokenless.
        let csrf_token: std::sync::Arc<str> = std::sync::Arc::from(csrf_token.as_str());

        let shared = TierBResolveCtx {
            app: &app,
            ctx: &ctx,
            error_component,
            loading_component,
            island_fills,
            csrf_token: csrf_token.clone(),
            return_path: return_path.clone(),
        };

        // ── Pass 1 · the nodes that are painted into the shell ──
        //
        // Resolved concurrently and awaited before the first byte, so the served
        // document carries the page's content as *markup* rather than as a
        // script argument. Each node keeps its own `timeout_ms`, which is what
        // bounds how long this can hold the response.
        let (inline_nodes, streamed_nodes): (Vec<TierBNode>, Vec<TierBNode>) =
            route.tier_b.iter().cloned().partition(painted_inline);

        let mut painted: HashMap<String, InjectionChunk> = HashMap::new();
        if !inline_nodes.is_empty() {
            let mut inline_futures: FuturesUnordered<_> = inline_nodes
                .into_iter()
                .map(|node| {
                    let id = node.placeholder_id.clone();
                    let shared = &shared;
                    async move { (id, resolve_tier_b_node(node, shared).await) }
                })
                .collect();
            while let Some((id, chunk)) = inline_futures.next().await {
                painted.insert(id, chunk);
            }
        }

        let mut shell = build_shell_chunk(
            &route,
            negotiated_transport,
            app.transport.webtransport_path.as_str(),
            &csrf_token,
            &return_path,
            hydration,
            &painted,
        );

        // Slice 3 — a route exporting `generateMetadata` carries a head marker
        // instead of a static `<title>`/`<meta>` block. Resolve the real
        // metadata per request (static base merged with the dynamic result) and
        // substitute it in. A failed or absent eval degrades to the static base,
        // so the marker is always replaced — a stray comment never ships.
        if let Some(key) = route.dynamic_metadata.as_deref() {
            let props = json!({ "params": ctx.params.clone(), "path": ctx.path.clone() });
            let mut resolved = route.metadata.clone();
            match app.services.registry.call_metadata(key, &props).await {
                Ok(Some(value)) => resolved.merge(lower_metadata_object(&value)),
                Ok(None) => {}
                Err(err) => warn!(
                    target: "albedo.render",
                    route = %route.route,
                    error = %err,
                    "generateMetadata failed; falling back to static head"
                ),
            }
            shell = shell.replace(
                DYNAMIC_HEAD_MARKER,
                &render_head_metadata(route.route.as_str(), &resolved),
            );
        }

        yield Ok(Bytes::from(shell));

        // ── Pass 2 · the nodes that stream ──
        //
        // Only the ones with declared external data reach here; the rest are
        // already in the document above. A painted node ships **no** inject:
        // `__albedo_inject` assigns `outerHTML`, so re-sending identical markup
        // would replace live nodes for no reason — losing focus and selection,
        // which is the property this framework's delta path exists to keep.
        let mut tier_b_futures: FuturesUnordered<_> = streamed_nodes
            .into_iter()
            .map(|node| {
                let shared = &shared;
                async move { resolve_tier_b_node(node, shared).await }
            })
            .collect();

        while let Some(chunk) = tier_b_futures.next().await {
            yield Ok(Bytes::from(chunk.into_script_tag()));
        }

        // A3 · emit the client runtime + per-island IIFEs + hydration payload +
        // bootstrap precomputed at boot. Replaces the legacy `bundle_path`
        // (`/_albedo/chunks/*.js`, never emitted → 404) + `__albedo_hydrate`
        // path. Absent for Tier-A-only routes.
        let mut closing = String::new();
        if let Some(hydration) = hydration {
            closing.push_str(&hydration.closing_scripts);
        }

        // Dev mode — inject the shared SSE channel, then the error overlay and
        // the slot-preserving HMR client that subscribe to it. This is what
        // makes `albedo dev` the SAME production pipeline plus the dev
        // affordances, instead of a second renderer. `defer` so they don't
        // block the island runtime.
        //
        // ORDER IS LOAD-BEARING: `stream.js` installs `window.__albedoDev`,
        // and the two consumers read it at init. `defer` scripts execute in
        // document order, so the channel must be emitted first. It also owns
        // the only EventSource — dev used to spend three of the browser's six
        // per-origin HTTP/1.1 connections per tab, which froze the page (and
        // blocked reloads) as soon as a second tab was open.
        if app.dev_mode {
            closing.push_str(
                "<script src=\"/_albedo/dev/stream.js\" defer></script>\
                 <script src=\"/_albedo/dev/overlay.js\" defer></script>\
                 <script src=\"/_albedo/dev/hmr-apply.js\" defer></script>",
            );
        }

        closing.push_str(&route.shell.body_close);
        yield Ok(Bytes::from(closing));
    }
}

fn build_shell_chunk(
    route: &RouteManifest,
    negotiated_transport: NegotiatedTransport,
    webtransport_path: &str,
    csrf_token: &str,
    request_path: &str,
    hydration: Option<&crate::renderer_runtime::RouteHydration>,
    // Tier-B nodes already resolved on this request, keyed by placeholder id.
    // Empty on the WebTransport path, which ships opcode frames rather than
    // injected markup and is JS-guaranteed by definition.
    painted: &HashMap<String, InjectionChunk>,
) -> String {
    let mut shell = route.shell.doctype_and_head.clone();
    shell.push_str(&route.shell.body_open);
    shell.push_str(&transport_hint_script(
        negotiated_transport,
        webtransport_path,
        route_needs_live_lane(route),
    ));
    shell.push_str(&csrf_bootstrap_script(csrf_token));
    shell.push_str(&route.shell.shim_script);

    for node in &route.tier_a_root {
        shell = shell.replace(
            &format!("<!--__SLOT_{}-->", node.placeholder_id),
            &node.html,
        );
    }

    // Paint this request's own Tier-B renders into their placeholders. Done
    // before `seed_tier_b_placeholders` so the fresh markup wins: the seed
    // matches only an *empty* placeholder, so a painted node is already past it
    // and the build-time HTML can never overwrite a per-request render.
    shell = paint_tier_b_placeholders(shell, painted);

    // Seed before the fill pass, not after: a `<form action="action:…">` inside
    // the build-time HTML carries an unfilled CSRF placeholder, and
    // `fill_server_placeholders` below is the single stage that stamps it.
    //
    // This now covers only what pass 1 did not paint — a node that streams. Its
    // build-time HTML is still better than an empty hole while the real chunk is
    // in flight, on exactly the terms its own gate describes.
    shell = seed_tier_b_placeholders(shell, &route.tier_b);

    fill_server_placeholders(
        shell,
        hydration
            .map(|h| h.placeholders.as_slice())
            .unwrap_or_default(),
        csrf_token,
        request_path,
    )
}

/// The single server-side fill pass. Every chunk of rendered HTML goes
/// through exactly this on its way to the browser — the shell here, and
/// each resolved Tier-B chunk in [`build_stream`].
///
/// One function rather than a fill per call site because the previous
/// arrangement — the shell substituting its CSRF tokens inline while
/// the Tier-B path did only islands — is what let Tier-B forms reach
/// the browser with an empty token. A stage that applies to served HTML
/// belongs here, where a renderer can't be forgotten.
///
/// Order matters and is the reason these two are fused: islands are
/// spliced in FIRST, so a form nested inside an island is filled by the
/// same CSRF pass as one in the page body. Island markup is precomputed
/// once at boot and is therefore session-less — its tokens can only be
/// filled after it lands in a request's HTML.
fn fill_server_placeholders(
    html: String,
    islands: &[(String, String)],
    csrf_token: &str,
    request_path: &str,
) -> String {
    let with_islands = replace_island_placeholders(html, islands);
    // Phase L · stamp the per-session token into every hidden CSRF input
    // the renderers emitted. No-op for any page without a form.
    let with_csrf = crate::render::csrf::substitute_csrf_token_in_html(&with_islands, csrf_token);
    // …and, in the same pass, the path a JS-less submit returns to. Fused with
    // the CSRF fill rather than run as its own stage for the reason this
    // function exists at all: a form that got one fill and not the other
    // submits successfully and then lands the reader on `/`, which reads as the
    // app losing their place rather than as a missing substitution.
    let with_return =
        crate::render::csrf::substitute_return_path_in_html(&with_csrf, request_path);
    // …and the intent token, fused for the same reason: a form that reached the
    // browser with an empty one still submits, and still works — it simply
    // cannot be resumed after a crash, which is the failure that looks like
    // nothing until the day it is a double charge.
    //
    // Fresh per render, which is the whole semantic: see
    // `substitute_intent_token_in_html`.
    crate::render::csrf::substitute_intent_token_in_html(
        &with_return,
        &uuid::Uuid::new_v4().simple().to_string(),
    )
}

/// Replace each empty Tier-C island placeholder (`<div id="…"
/// data-albedo-tier="c"></div>`) with the island's marked SSR HTML, so every
/// island — whichever renderer emitted its hole — converges on identical served
/// markup carrying the `data-albedo-island` marker the client hydrates against.
fn replace_island_placeholders(mut html: String, placeholders: &[(String, String)]) -> String {
    for (placeholder_id, marked_html) in placeholders {
        let empty = format!("<div id=\"{placeholder_id}\" data-albedo-tier=\"c\"></div>");
        html = html.replace(&empty, marked_html);
    }
    html
}

/// Seed each Tier-B placeholder with the HTML the **build** already rendered
/// for it, so the shell is never served with an empty hole where the page's
/// content belongs.
///
/// `TierBNode::initial_html` has been produced at build time since Phase P
/// (`manifest::builder` renders it through the pure-Rust renderer against a
/// fresh, empty slot store) and nothing read it. Without this the body of a
/// Tier-B route exists only as a JavaScript string argument inside
/// `<script>__albedo_inject(…)</script>`, which means the page has no content
/// at all for a reader without JS, and none for a crawler that does not execute
/// it. The per-request render still arrives and still wins: `__albedo_inject`
/// assigns `el.outerHTML`, replacing the seeded div wholesale.
///
/// # Why this is gated
///
/// The build-time render saw no request. A node with `dynamic_prop_keys` or
/// `data_deps` was rendered without the values it actually needs — `/room/[id]`
/// carries `dynamic_prop_keys: ["params"]`, so its `initial_html` is a room with
/// no id — and seeding that would paint markup for the wrong entity before
/// correcting it. Seeding is therefore limited to nodes whose render is a pure
/// function of the build: no request props, no request-scoped data.
///
/// Live data may still have moved since the build (a FORGE collection gains
/// rows), so a seeded list can be briefly stale. That is the ordinary
/// server-streaming trade and it is strictly better than an empty element: the
/// content is correct in shape, correct in structure, and replaced within the
/// same response.
/// Replace each resolved Tier-B placeholder with this request's own rendered
/// markup, so the served document carries the page's content as HTML.
///
/// ## Why replace rather than fill
///
/// `__albedo_inject` assigns `el.outerHTML`, which replaces the placeholder
/// element. Painting does the same thing at the same position, so the document a
/// reader without JavaScript receives is **byte-identical to the one the
/// injector would have produced**. Filling instead would leave a wrapper `<div>`
/// that exists on one path and not the other, and every stylesheet, selector and
/// delta anchor written against one would be reasoning about the other.
///
/// A node painted here ships no inject (see pass 2 in `build_stream`), so this
/// is the only thing that writes it.
fn paint_tier_b_placeholders(
    mut html: String,
    painted: &HashMap<String, InjectionChunk>,
) -> String {
    for (placeholder_id, chunk) in painted {
        let empty = format!("<div id=\"{placeholder_id}\" data-albedo-tier=\"b\"></div>");
        // `clone` because the map is borrowed and the chunk owns its markup;
        // one clone per Tier-B node per request, against a render that just
        // executed a component.
        let markup = chunk.clone().into_painted_markup();
        html = html.replace(&empty, &markup);
    }
    html
}

fn seed_tier_b_placeholders(mut html: String, nodes: &[TierBNode]) -> String {
    for node in nodes {
        if !node.dynamic_prop_keys.is_empty() || !node.data_deps.is_empty() {
            continue;
        }
        let Some(initial) = node.initial_html.as_deref() else {
            continue;
        };
        if initial.is_empty() {
            continue;
        }
        let id = &node.placeholder_id;
        let empty = format!("<div id=\"{id}\" data-albedo-tier=\"b\"></div>");
        let seeded = format!("<div id=\"{id}\" data-albedo-tier=\"b\">{initial}</div>");
        html = html.replace(&empty, &seeded);
    }
    html
}

/// Hard cap on how long the WT path will tick + drain waiting for async
/// islands to resolve. Stuck resolvers don't block the request forever;
/// any still-pending islands at this point will be cancelled by the
/// next request anyway (their resolutions arrive at an mpsc no one is
/// reading).
const WT_ASYNC_DRAIN_TIMEOUT_MS: u64 = 5_000;

/// Inter-tick sleep while waiting for resolver Futures to complete.
/// Short enough that small islands appear within the same RAF cadence
/// the client expects; long enough that the loop doesn't spin.
const WT_ASYNC_DRAIN_SLEEP_MS: u64 = 5;

/// Phase-E: WT streaming flow. Ships shell as text on slot 1, opcode
/// frames (bootstrap intern + per-tier-B patches via async islands) as
/// binary on slot 2, and prefetch hints as JSON on slot 3.
///
/// Requires both an opcode pipeline (bound via
/// `StreamingAppState::with_pipeline`) and a `TierBOpcodeRegistry`
/// (set on `SharedRenderServices.opcode_registry`). Without these the
/// function errors out so the caller falls back to SSE.
async fn stream_route_over_webtransport(
    route: RouteManifest,
    ctx: TierBRequestContext,
    app: Arc<StreamingAppState>,
    session_id: Uuid,
) -> Result<(), String> {
    let sessions = app
        .webtransport_sessions
        .as_ref()
        .ok_or_else(|| "webtransport session registry unavailable".to_string())?;

    let pipeline = app
        .pipeline()
        .cloned()
        .ok_or_else(|| "opcode pipeline unavailable on WT path".to_string())?;

    let opcode_registry = app
        .services
        .opcode_registry
        .clone()
        .ok_or_else(|| "opcode registry unavailable on WT path".to_string())?;
    let data_fetcher = app.services.data_fetcher.clone();

    // Phase L · the WT session id doubles as the CSRF session id on
    // this path. The same uuid the client carries on the WT
    // handshake is what the action route will see in the
    // `__Host-albedo-session` cookie (or `x-albedo-wt-session` header) when
    // it later POSTs a form, so the token table keys align without
    // any cookie round-trip on the WT path.
    let page_session = dom_render_compiler::runtime::SessionId::new(session_id);

    // 1. Shell HTML on the text slot. A3 client hydration rides the SSE/HTTP
    //    path (`build_stream`); the WT path stays on its opcode-frame model, so
    //    no per-route hydration block is threaded here.
    let mut shell = build_shell_chunk(
        &route,
        NegotiatedTransport::WebTransport,
        app.transport.webtransport_path.as_str(),
        &app.csrf().token_for(page_session),
        // The request's own path, so a form in this shell returns here. A page
        // reached over WebTransport ran JavaScript by definition, so its forms
        // will be intercepted and this value never read — it is stamped anyway
        // because "this branch cannot need it" is how the CSRF input came to be
        // missing on the Tier-B path, and the cost of being wrong is a reader
        // silently landing on `/`.
        ctx.path.as_str(),
        None,
        // No painted nodes on this path. WebTransport delivers Tier-B as opcode
        // frames on the patches lane, not as injected markup, so painting here
        // would put the content in the document twice — and a page reached over
        // WebTransport ran JavaScript to open the session, so the no-JS reader
        // this painting exists for cannot be on this path at all.
        &HashMap::new(),
    );
    shell.push_str(&route.shell.body_close);
    sessions
        .send_payload(session_id, WT_STREAM_SLOT_SHELL, shell.into_bytes())
        .await
        .map_err(|err| err.to_string())?;

    // Phase P · Stream C.4 — auto-subscribe this session to every
    // broadcast topic the route's JSX references via
    // `useSharedSlot`. The patches-lane sender becomes the
    // per-subscriber sink the broadcast registry's `write_topic`
    // drives later via `try_send`. The returned `Vec<Instruction>`
    // is the initial-state SlotSet payload — wrap it in an
    // `OpcodeFrame` and ship it before the bootstrap intern table
    // so the client paints with current broadcast state before any
    // `SetTextRef` (from the Tier-B opcode frame baked into the
    // manifest by Stream B) references it.
    if !route.shared_slot_topics.is_empty() {
        if let Some(broadcast) = app.broadcast() {
            if let Some(patches_sender) =
                sessions.stream_sender(session_id, WT_STREAM_SLOT_PATCHES)
            {
                let initial = broadcast.auto_subscribe(
                    page_session,
                    patches_sender,
                    &route.shared_slot_topics,
                );
                if !initial.is_empty() {
                    let frame = dom_render_compiler::ir::opcode::OpcodeFrame {
                        frame_id: 0,
                        component_id: None,
                        instructions: initial,
                    };
                    let encoded = dom_render_compiler::ir::wire::encode_frame(&frame)
                        .map_err(|err| {
                            format!("auto_subscribe initial-state encode failed: {err}")
                        })?;
                    sessions
                        .send_payload(session_id, WT_STREAM_SLOT_PATCHES, encoded)
                        .await
                        .map_err(|err| err.to_string())?;
                }
            }
        }
    }

    // 2. Bootstrap intern table on the binary patches slot. The
    //    classifier is a stub for Phase E (Phase F+ will plug in a real
    //    one driven by the renderer's intern context); shipping an
    //    empty bootstrap is a valid no-op the bakabox VM tolerates.
    if let Some(chunk) = drain_pipeline_bootstrap(app.as_ref(), |_, _| None)
        .map_err(|err| err.to_string())?
    {
        ship_chunk(sessions, session_id, chunk)
            .await
            .map_err(|err| err.to_string())?;
    }

    // 3. Enqueue every Tier-B node as a Phase-D async island. The
    //    Future that resolves each island runs render_tier_b_opcodes
    //    inside the node's manifest-declared timeout; on error or
    //    timeout the island resolves to an empty instruction vector so
    //    the placeholder stays empty rather than crashing the tick.
    for node in &route.tier_b {
        let node_owned = node.clone();
        let ctx_owned = ctx.clone();
        let registry = opcode_registry.clone();
        let fetcher = data_fetcher.clone();
        let timeout_ms = node.timeout_ms.max(1);
        let placeholder_stable_id = stable_id_for_placeholder(&node.placeholder_id);

        let resolver = async move {
            let rendered = tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                render_tier_b_opcodes(
                    &node_owned,
                    &ctx_owned,
                    registry.as_ref(),
                    fetcher.as_ref(),
                ),
            )
            .await;
            match rendered {
                Ok(Ok(instructions)) => instructions,
                Ok(Err(err)) => {
                    warn!(
                        render_fn = %node_owned.render_fn,
                        error = %err,
                        "render_tier_b_opcodes failed; shipping empty patch"
                    );
                    Vec::new()
                }
                Err(_) => {
                    warn!(
                        render_fn = %node_owned.render_fn,
                        timeout_ms,
                        "render_tier_b_opcodes timed out; shipping empty patch"
                    );
                    Vec::new()
                }
            }
        };

        let _ = pipeline
            .lock()
            .map_err(|_| "pipeline mutex poisoned".to_string())?
            .enqueue_async_island(placeholder_stable_id, resolver)
            .map_err(|err| err.to_string())?;
    }

    // 4. Drive ticks + drain chunks until every island has resolved or
    //    the hard deadline elapses. Each drain ships Placeholder frames
    //    (on the first iteration) and Patch frames (as resolvers land).
    drain_async_islands_into_session(app.as_ref(), sessions, session_id).await?;

    // 5. Prefetch hints on slot 3 (JSON). Hydration triggers stay on
    //    the SSE path until Phase F ports them to opcodes.
    let prefetch_modules: Vec<String> = route
        .tier_c
        .iter()
        .filter(|node| node.hydration_mode != HydrationMode::None)
        .map(|node| node.bundle_path.clone())
        .collect();
    if !prefetch_modules.is_empty() {
        sessions
            .send_json(
                session_id,
                WT_STREAM_SLOT_PREFETCH,
                &json!({
                    "modules": prefetch_modules,
                    "assets": Vec::<String>::new(),
                }),
            )
            .await
            .map_err(|err| err.to_string())?;
    }

    // 6. Route-complete envelope on the JSON control slot.
    sessions
        .send_json(
            session_id,
            WT_STREAM_SLOT_CONTROL,
            &json!({
                "event": "route_complete",
                "session_id": session_id.to_string(),
                "route": route.route,
            }),
        )
        .await
        .map_err(|err| err.to_string())?;

    Ok(())
}

/// Ships a single chunk through the right WT slot. Centralises the
/// binary/text payload coercion so callers don't duplicate the match.
async fn ship_chunk(
    sessions: &WebTransportSessionRegistry,
    session_id: Uuid,
    chunk: LaneRenderedChunk,
) -> Result<(), RuntimeError> {
    let payload = match chunk.payload {
        FramePayload::Binary(bytes) => bytes,
        FramePayload::Text(text) => text.into_bytes(),
    };
    sessions
        .send_payload(session_id, chunk.lane as u8, payload)
        .await
}

/// Tick + drain loop. Yields after each iteration so spawned resolvers
/// can progress on the runtime's worker. Exits when no async islands
/// are still pending, or when the hard deadline elapses.
async fn drain_async_islands_into_session(
    app: &StreamingAppState,
    sessions: &WebTransportSessionRegistry,
    session_id: Uuid,
) -> Result<(), String> {
    let deadline = std::time::Instant::now()
        + Duration::from_millis(WT_ASYNC_DRAIN_TIMEOUT_MS);

    loop {
        let chunks = drive_pipeline_tick(app);
        for chunk in chunks {
            ship_chunk(sessions, session_id, chunk)
                .await
                .map_err(|err| err.to_string())?;
        }

        let pending = match app.pipeline() {
            Some(handle) => handle
                .lock()
                .map_err(|_| "pipeline mutex poisoned".to_string())?
                .pending_async_count(),
            None => 0,
        };
        if pending == 0 {
            return Ok(());
        }

        if std::time::Instant::now() >= deadline {
            warn!(
                pending,
                "async-island drain deadline reached; leaving {} islands unresolved",
                pending
            );
            return Ok(());
        }

        tokio::time::sleep(Duration::from_millis(WT_ASYNC_DRAIN_SLEEP_MS)).await;
    }
}

fn request_context_from_request(
    req: &Request,
    params: HashMap<String, String>,
    // AUTH item 5 P1 · resolved once by the dispatcher and carried, never
    // re-derived here from `cookies`. Two places deciding who the caller is is
    // two places that can disagree, and the one that renders would win silently.
    principal: Option<dom_render_compiler::auth::PrincipalId>,
) -> TierBRequestContext {
    let mut headers = HashMap::new();
    let mut cookies = HashMap::new();

    for (name, value) in req.headers() {
        if let Ok(value) = value.to_str() {
            headers.insert(name.as_str().to_ascii_lowercase(), value.to_string());
        }
    }

    if let Some(raw_cookie) = headers.get("cookie") {
        cookies = parse_cookie_header(raw_cookie);
    }

    TierBRequestContext {
        path: req.uri().path().to_string(),
        params,
        headers,
        cookies,
        principal,
    }
}

/// Collect a `Cookie` header into the map Tier-B's `cookie:` data source reads.
///
/// The policy is **first wins**, matching every other cookie reader in the tree.
/// It did not used to: this was built with `insert`, which silently means *last*
/// wins, so a duplicate name appended to the header overrode the one the browser
/// put first. Nobody chose that — it is just what `insert` does — and it made a
/// component reading `cookie:x` disagree with the CSRF path reading the very
/// same header. `or_insert_with` is the entire fix, and is why the shared
/// tokenizer yields entries in the order they arrived rather than a map.
fn parse_cookie_header(raw: &str) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    for (name, value) in dom_render_compiler::auth::cookie_entries(raw) {
        cookies
            .entry(name.to_string())
            .or_insert_with(|| value.to_string());
    }
    cookies
}

fn not_found_response() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("route not found"))
        .unwrap_or_else(|_| Response::new(Body::from("route not found")))
}

fn negotiate_transport(req: &Request, config: &StreamingTransportConfig) -> NegotiatedTransport {
    if !config.webtransport_enabled {
        return NegotiatedTransport::Sse;
    }

    if !request_wants_webtransport(req) {
        return NegotiatedTransport::Sse;
    }

    if request_supports_http3(req) {
        return NegotiatedTransport::WebTransport;
    }

    NegotiatedTransport::Sse
}

fn request_wants_webtransport(req: &Request) -> bool {
    req.headers().contains_key(WT_SESSION_HEADER)
        || header_value_contains(req.headers().get(WT_PREFER_HEADER), "webtransport")
        || header_has_token(req.headers().get(header::UPGRADE), "webtransport")
        || req
            .headers()
            .keys()
            .any(|name| name.as_str().starts_with("sec-webtransport-http3-draft"))
}

fn request_supports_http3(req: &Request) -> bool {
    req.headers().contains_key(WT_SESSION_HEADER)
        || req.version() == Version::HTTP_3
        || header_value_contains(req.headers().get("x-forwarded-proto"), "h3")
        || header_value_contains(req.headers().get("forwarded"), "proto=h3")
        || req.headers().contains_key("alt-used")
}

fn header_has_token(value: Option<&HeaderValue>, token: &str) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };

    value
        .split(',')
        .map(str::trim)
        .any(|entry| entry.eq_ignore_ascii_case(token))
}

fn header_value_contains(value: Option<&HeaderValue>, needle: &str) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    value
        .to_ascii_lowercase()
        .contains(needle.to_ascii_lowercase().as_str())
}

fn maybe_webtransport_session_id(req: &Request) -> Option<Uuid> {
    req.headers()
        .get(WT_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
}

/// Publishes the per-session CSRF token to same-origin JavaScript as
/// `globalThis.__ALBEDO_CSRF__`, so the client runtime can attach it as
/// the `x-albedo-csrf` header on every action POST. That header is the
/// only token channel click/input actions have — their bincode payload
/// carries no field for one — so without this the action gate would
/// reject every non-form action.
///
/// Safe to expose: this is not the session secret (that stays in the
/// `HttpOnly` `__Host-albedo-session` cookie) and it is already present in the
/// DOM as every form's hidden `_csrf` input, readable by any same-origin
/// script. A CSRF token defends against cross-*site* forgery, which the
/// same-origin policy already keeps from reading this value; it does not
/// defend against XSS, which could read the hidden input regardless.
///
/// The token is 32 hex chars from [`crate::render::csrf`], so JSON
/// string-encoding it is belt-and-suspenders against any character that
/// would need escaping inside the `<script>`.
fn csrf_bootstrap_script(csrf_token: &str) -> String {
    let literal = serde_json::to_string(csrf_token).unwrap_or_else(|_| "\"\"".to_string());
    format!("<script>globalThis.__ALBEDO_CSRF__={literal};</script>")
}

fn transport_hint_script(
    transport: NegotiatedTransport,
    webtransport_path: &str,
    needs_live_lane: bool,
) -> String {
    let endpoint = match transport {
        NegotiatedTransport::WebTransport => webtransport_path,
        NegotiatedTransport::Sse => "",
    };
    let endpoint_literal = serde_json::to_string(endpoint).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "<script>globalThis.__ALBEDO_ACTIVE_TRANSPORT__=\"{}\";globalThis.__ALBEDO_WT_ENDPOINT__={};globalThis.__ALBEDO_LIVE__={};</script>",
        transport.as_header_value(),
        endpoint_literal,
        needs_live_lane
    )
}

/// Does this route need the patches lane?
///
/// **The single source of truth.** The client used to answer this itself by
/// sniffing the DOM for `[data-albedo-tier="b"]`, `[data-albedo-tier="c"]` or
/// `[data-albedo-list-slot]` — re-deriving, from page shape, a decision the
/// server had already made from the manifest. The two answers were allowed to
/// disagree, and they did: a route whose only live surface is a **scalar**
/// `useSharedSlot` read matched none of those selectors, so the browser never
/// opened the lane and `broadcast()` could not reach it. A shared-slot *list*
/// had hit the same wall earlier and was patched by adding a third selector to
/// the client's list — which is what made the next surface silently regress.
///
/// So it is answered once, here, from the same facts
/// [`stream_route_over_webtransport`] uses to decide whether to `auto_subscribe`
/// — and shipped to the client as `__ALBEDO_LIVE__`. A new live surface can
/// never again need a matching edit in a selector string on the other side of
/// the wire.
fn route_needs_live_lane(route: &RouteManifest) -> bool {
    // A broadcast topic the route reads — scalar or list, it is the same
    // question, and it is exactly what auto-subscribe keys on.
    if !route.shared_slot_topics.is_empty() {
        return true;
    }
    // PRISM · a partition is a topic this route reads; it just has no name until
    // a request supplies one. Asking only about `shared_slot_topics` would be the
    // same class of mistake this function exists to have fixed once: today it is
    // masked in any project that also has a static topic, because that list is
    // project-wide. An app whose *only* live read is a partition — a chat, a
    // per-user dashboard, the thing this feature is for — would render correct
    // HTML, open no lane, and never update.
    if !route.shared_slot_partitions.is_empty() {
        return true;
    }
    // A hydrated or streamed island receives patch frames on the same lane even
    // when the route reads no topic at all.
    !route.tier_b.is_empty() || !route.tier_c.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webtransport::WebTransportSessionHandle;
    use axum::body::to_bytes;
    use dom_render_compiler::manifest::schema::{
        DataDep, DataSource, DomPosition, HtmlShell, PartitionTopicSpec, RenderedNode,
        RouteManifest, TierBNode,
    };
    use serde_json::Value;
    use tokio::sync::mpsc;

    /// The third cookie parser in the tree, and the one that feeds Tier-B's
    /// `cookie:` data source. A valueless entry must drop out of the map
    /// without taking its neighbours with it — the auth cookie's parser once
    /// aborted the whole scan on this shape, and here the blast radius is
    /// every cookie a Tier-B component reads, not just one.
    #[test]
    fn a_valueless_entry_does_not_swallow_the_cookies_around_it() {
        let name = crate::render::ALBEDO_SESSION_COOKIE;
        let cookies = parse_cookie_header(&format!("consent; {name}=abc; dnt; theme=dark"));
        assert_eq!(cookies.get(name).map(String::as_str), Some("abc"));
        assert_eq!(cookies.get("theme").map(String::as_str), Some("dark"));
        // The valueless entries are absent, not present-and-empty: a component
        // asking for `cookie:consent` must see "unset", not "".
        assert!(!cookies.contains_key("consent"));
        assert!(!cookies.contains_key("dnt"));
    }

    /// Was `Some("second")` before the three parsers were unified, because
    /// `insert` overwrites. A Tier-B component reading `cookie:x` and the CSRF
    /// path reading the same header would then disagree about which duplicate
    /// was real, which is the kind of divergence that only shows up under an
    /// attack or a client bug — the two situations where you least want the
    /// answer to depend on which function asked.
    #[test]
    fn the_first_of_two_duplicates_wins_as_it_does_everywhere_else() {
        let cookies = parse_cookie_header("dup=first; dup=second");
        assert_eq!(cookies.get("dup").map(String::as_str), Some("first"));
    }

    #[test]
    fn a_header_of_only_valueless_entries_yields_no_cookies() {
        assert!(parse_cookie_header("consent; dnt").is_empty());
    }

    fn test_request(headers: &[(&str, &str)], version: Version) -> Request {
        let mut builder = Request::builder()
            .method("GET")
            .uri("/stream")
            .version(version);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap()
    }

    fn position() -> DomPosition {
        DomPosition {
            parent_placeholder: None,
            slot: "default".to_string(),
            order: 0,
        }
    }

    fn tier_b_node() -> TierBNode {
        TierBNode {
            component_id: "Feature".to_string(),
            placeholder_id: "__b_feature".to_string(),
            render_fn: "render::Feature".to_string(),
            static_props: json!({}),
            dynamic_prop_keys: Vec::new(),
            data_deps: vec![DataDep {
                key: "path".to_string(),
                source: DataSource::RequestContext {
                    key: "path".to_string(),
                },
            }],
            tier_a_children: vec![RenderedNode {
                component_id: "Leaf".to_string(),
                placeholder_id: "__a_leaf".to_string(),
                html: "<p>leaf</p>".to_string(),
                position: position(),
            }],
            position: position(),
            timeout_ms: 100,
            fallback_html: Some("<p>fallback</p>".to_string()),
            initial_html: None,
            initial_opcode_frame: Vec::new(),
        }
    }

    /// The seed lands, so the served shell carries the page's content instead
    /// of an empty hole. Without this the body of a Tier-B route exists only as
    /// a JS string inside `__albedo_inject(…)`, invisible to a reader without
    /// scripting and to any crawler that doesn't execute them.
    #[test]
    fn tier_b_placeholder_is_seeded_with_the_build_time_html() {
        let mut node = tier_b_node();
        node.data_deps.clear();
        node.initial_html = Some("<section>built</section>".to_string());

        let html = seed_tier_b_placeholders(
            "<body><div id=\"__b_feature\" data-albedo-tier=\"b\"></div></body>".to_string(),
            std::slice::from_ref(&node),
        );

        assert_eq!(
            html,
            "<body><div id=\"__b_feature\" data-albedo-tier=\"b\">\
             <section>built</section></div></body>",
            "the placeholder keeps its id and tier attribute so __albedo_inject \
             can still find it and swap its outerHTML"
        );
    }

    /// The gate on the **build-time** seed. `/room/[id]` carries
    /// `dynamic_prop_keys: ["params"]`, and its build-time render therefore saw
    /// no id — seeding it would paint one entity's markup while the request is
    /// for another.
    ///
    /// 🔑 This is not a statement that such a node ships empty. It ships the
    /// render this request produced, painted by `paint_tier_b_placeholders`
    /// before this seed ever runs; the seed is the fallback for what streams.
    #[test]
    fn a_request_dependent_node_is_left_empty_by_the_build_time_seed() {
        let mut node = tier_b_node();
        node.data_deps.clear();
        node.dynamic_prop_keys = vec!["params".to_string()];
        node.initial_html = Some("<section>wrong room</section>".to_string());

        let shell = "<div id=\"__b_feature\" data-albedo-tier=\"b\"></div>".to_string();
        assert_eq!(
            seed_tier_b_placeholders(shell.clone(), std::slice::from_ref(&node)),
            shell,
            "a node whose render depends on the request must not be seeded"
        );

        // Same rule for request-scoped data, which is what the fixture carries
        // by default.
        let with_deps = tier_b_node();
        let mut with_deps = with_deps;
        with_deps.initial_html = Some("<section>stale</section>".to_string());
        assert_eq!(
            seed_tier_b_placeholders(shell.clone(), std::slice::from_ref(&with_deps)),
            shell,
            "a node with request-context data_deps must not be seeded"
        );
    }

    /// 🔑 **The painted form must be what the injector would have produced.**
    /// `__albedo_inject` assigns `el.outerHTML`, replacing the placeholder — so
    /// painting replaces it too. Filling instead would leave a wrapper `<div>`
    /// present without JavaScript and absent with it, and every selector or
    /// delta anchor written against one would be reasoning about the other.
    #[test]
    fn a_painted_node_replaces_the_placeholder_rather_than_filling_it() {
        let node = tier_b_node();
        let mut painted = HashMap::new();
        painted.insert(
            node.placeholder_id.clone(),
            InjectionChunk::success(&node, "<section>live</section>".to_string()),
        );

        let html = paint_tier_b_placeholders(
            "<body><div id=\"__b_feature\" data-albedo-tier=\"b\"></div></body>".to_string(),
            &painted,
        );

        assert_eq!(html, "<body><section>live</section></body>");
    }

    /// The per-request render supersedes the build-time seed. Painting runs
    /// first and consumes the empty placeholder, so the seed — which matches
    /// only an empty one — can no longer reach it. That ordering is what stops a
    /// stale build-time list from overwriting the rows this request just read.
    #[test]
    fn a_painted_node_cannot_be_overwritten_by_the_build_time_seed() {
        let mut node = tier_b_node();
        node.data_deps.clear();
        node.initial_html = Some("<section>stale build</section>".to_string());

        let mut painted = HashMap::new();
        painted.insert(
            node.placeholder_id.clone(),
            InjectionChunk::success(&node, "<section>fresh request</section>".to_string()),
        );

        let shell = "<div id=\"__b_feature\" data-albedo-tier=\"b\"></div>".to_string();
        let painted_html = paint_tier_b_placeholders(shell, &painted);
        let after_seed = seed_tier_b_placeholders(painted_html, std::slice::from_ref(&node));

        assert_eq!(after_seed, "<section>fresh request</section>");
        assert!(!after_seed.contains("stale build"));
    }

    /// A node that threw with no `error.tsx` keeps its placeholder and is
    /// marked, mirroring the injector's other branch (`setAttribute`, not
    /// `outerHTML`). It must NOT be left bare, or the build-time seed would
    /// then fill a failed component with markup that never rendered.
    #[test]
    fn a_failed_node_paints_a_marked_placeholder_the_seed_cannot_fill() {
        let mut node = tier_b_node();
        node.data_deps.clear();
        node.initial_html = Some("<section>stale build</section>".to_string());

        let mut painted = HashMap::new();
        painted.insert(
            node.placeholder_id.clone(),
            InjectionChunk::error(
                &node,
                crate::render::tier_b::RenderError::MissingDynamicProp {
                    key: "user".to_string(),
                },
            ),
        );

        let shell = "<div id=\"__b_feature\" data-albedo-tier=\"b\"></div>".to_string();
        let painted_html = paint_tier_b_placeholders(shell, &painted);
        assert!(painted_html.contains("data-albedo-error=\"error\""), "{painted_html}");

        let after_seed = seed_tier_b_placeholders(painted_html, std::slice::from_ref(&node));
        assert!(
            !after_seed.contains("stale build"),
            "a component that failed must not be papered over with build-time markup: {after_seed}"
        );
    }

    /// The cut between the two passes: external data streams, everything else is
    /// painted. A node reading only request props is painted precisely because
    /// there is now a request to read it from.
    #[test]
    fn only_external_data_defers_a_node_to_the_stream() {
        let mut request_props_only = tier_b_node();
        request_props_only.data_deps.clear();
        request_props_only.dynamic_prop_keys = vec!["user".to_string()];
        assert!(painted_inline(&request_props_only));

        let mut nothing_dynamic = tier_b_node();
        nothing_dynamic.data_deps.clear();
        nothing_dynamic.dynamic_prop_keys.clear();
        assert!(painted_inline(&nothing_dynamic));

        // The fixture carries `data_deps` by default.
        assert!(!painted_inline(&tier_b_node()));
    }

    /// A build that produced no `initial_html` degrades to exactly the old
    /// behaviour rather than emitting a malformed element.
    #[test]
    fn a_node_without_initial_html_is_untouched() {
        let mut node = tier_b_node();
        node.data_deps.clear();
        node.initial_html = None;

        let shell = "<div id=\"__b_feature\" data-albedo-tier=\"b\"></div>".to_string();
        assert_eq!(
            seed_tier_b_placeholders(shell.clone(), std::slice::from_ref(&node)),
            shell
        );

        node.initial_html = Some(String::new());
        assert_eq!(
            seed_tier_b_placeholders(shell.clone(), std::slice::from_ref(&node)),
            shell,
            "an empty string is not content"
        );
    }

    fn route_manifest() -> RouteManifest {
        RouteManifest {
            route: "/stream".to_string(),
            shell: HtmlShell {
                doctype_and_head: "<!doctype html><html><head></head>".to_string(),
                body_open: "<body><div id=\"__b_feature\" data-albedo-tier=\"b\"></div>"
                    .to_string(),
                body_close: "</body></html>".to_string(),
                shim_script: "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
                    .to_string(),
            },
            tier_a_root: Vec::new(),
            tier_b: vec![tier_b_node()],
            tier_c: Vec::new(),
            shared_slot_topics: Vec::new(),
            auth: Default::default(),
            shared_slot_partitions: Vec::new(),
            shared_slot_sources: Vec::new(),
            action_ids: Vec::new(),
            layout_chain: Vec::new(),
            error_component: None,
            loading_component: None,
            metadata: Default::default(),
            dynamic_metadata: None,
        }
    }

    #[test]
    fn test_negotiate_transport_prefers_sse_when_wt_disabled() {
        let req = test_request(
            &[("upgrade", "webtransport"), ("x-forwarded-proto", "h3")],
            Version::HTTP_11,
        );
        let config = StreamingTransportConfig::new(false, 443);
        assert_eq!(negotiate_transport(&req, &config), NegotiatedTransport::Sse);
    }

    #[test]
    fn test_negotiate_transport_uses_webtransport_when_upgrade_and_h3_present() {
        let req = test_request(
            &[("upgrade", "webtransport"), ("x-forwarded-proto", "h3")],
            Version::HTTP_11,
        );
        let config = StreamingTransportConfig::new(true, 443);
        assert_eq!(
            negotiate_transport(&req, &config),
            NegotiatedTransport::WebTransport
        );
    }

    #[test]
    fn test_negotiate_transport_uses_session_header_for_bridge_requests() {
        let req = test_request(
            &[(WT_SESSION_HEADER, "00000000-0000-0000-0000-000000000001")],
            Version::HTTP_11,
        );
        let config = StreamingTransportConfig::new(true, 443);
        assert_eq!(
            negotiate_transport(&req, &config),
            NegotiatedTransport::WebTransport
        );
    }

    #[test]
    fn test_negotiate_transport_falls_back_to_sse_without_h3_signal() {
        let req = test_request(&[("upgrade", "webtransport")], Version::HTTP_11);
        let config = StreamingTransportConfig::new(true, 443);
        assert_eq!(negotiate_transport(&req, &config), NegotiatedTransport::Sse);
    }

    #[test]
    fn test_transport_hint_script_disables_wt_endpoint_for_sse_fallback() {
        let script = transport_hint_script(NegotiatedTransport::Sse, "/_albedo/wt", false);
        assert!(script.contains("__ALBEDO_ACTIVE_TRANSPORT__=\"sse\""));
        assert!(script.contains("__ALBEDO_WT_ENDPOINT__=\"\""));
        assert!(script.contains("__ALBEDO_LIVE__=false"));
    }

    #[test]
    fn csrf_bootstrap_script_publishes_the_token_to_the_global() {
        // The client runtime reads `globalThis.__ALBEDO_CSRF__` to attach
        // the `x-albedo-csrf` header on every action POST. If this stops
        // emitting the token, every click/input action 403s — so pin both
        // the global name and that the real token lands in it.
        let script = csrf_bootstrap_script("cafebabecafebabecafebabecafebabe");
        assert!(
            script.contains("globalThis.__ALBEDO_CSRF__=\"cafebabecafebabecafebabecafebabe\""),
            "token must be published to the runtime's global: {script}"
        );
        assert!(!script.contains("=\"\""), "an empty token is the silent 403 this guards");
    }

    #[test]
    fn replace_island_placeholders_fills_holes_in_tier_b_html() {
        // The unified island fill runs over async-page (Tier-B) HTML the same way
        // it runs over the shell: an empty placeholder is swapped for the island's
        // marked SSR markup; unrelated holes and content are left untouched.
        let html = "<main><h1>Essay</h1>\
<div id=\"__c_progress_7\" data-albedo-tier=\"c\"></div></main>"
            .to_string();
        let fills = vec![(
            "__c_progress_7".to_string(),
            "<div class=\"bar\" data-albedo-island=\"7\"></div>".to_string(),
        )];
        let out = replace_island_placeholders(html, &fills);
        assert!(
            out.contains("<div class=\"bar\" data-albedo-island=\"7\"></div>"),
            "island hole must be filled with marked SSR markup: {out}"
        );
        assert!(
            !out.contains("data-albedo-tier=\"c\""),
            "the empty placeholder must be gone: {out}"
        );
        assert!(out.contains("<h1>Essay</h1>"), "page content preserved: {out}");
    }

    /// Phase L · the fill pass is shared by the shell and the Tier-B
    /// chunks, and this is the half that used to be missing: Tier-B HTML
    /// got islands filled but never had its CSRF tokens substituted, so
    /// a form inside an `async function Page()` reached the browser with
    /// `value=""` and its submit was tokenless.
    #[test]
    fn fill_server_placeholders_stamps_the_csrf_token_into_tier_b_html() {
        use dom_render_compiler::transforms::form::CSRF_PLACEHOLDER_INPUT;

        let html = format!("<main><form data-albedo-action=\"sign\">{CSRF_PLACEHOLDER_INPUT}</form></main>");
        let out = fill_server_placeholders(html, &[], "cafebabe", "/guestbook");

        assert!(
            out.contains("value=\"cafebabe\""),
            "the Tier-B chunk must carry the per-session token: {out}"
        );
        assert!(
            !out.contains("value=\"\""),
            "an empty token is the silent failure this guards: {out}"
        );
    }

    /// Ordering, pinned: islands are spliced in BEFORE the CSRF pass, so
    /// a form that arrives inside island markup is filled too. Island
    /// HTML is precomputed once at boot and has no session of its own —
    /// if the passes ran in the other order it would keep `value=""`
    /// forever.
    #[test]
    fn fill_server_placeholders_reaches_a_form_nested_inside_an_island() {
        use dom_render_compiler::transforms::form::CSRF_PLACEHOLDER_INPUT;

        let html = "<main><div id=\"__c_1\" data-albedo-tier=\"c\"></div></main>".to_string();
        let fills = vec![(
            "__c_1".to_string(),
            format!("<form data-albedo-action=\"sign\">{CSRF_PLACEHOLDER_INPUT}</form>"),
        )];

        let out = fill_server_placeholders(html, &fills, "deadbeef", "/guestbook");
        assert!(
            out.contains("value=\"deadbeef\""),
            "a form arriving via an island must still get a token: {out}"
        );
    }

    /// The return-path fill rides the same pass, and it must reach the same
    /// places — including inside an island, where the markup is precomputed at
    /// boot and has no request of its own. A form that gets a token but no
    /// return path submits successfully and drops the reader on `/`.
    #[test]
    fn fill_server_placeholders_stamps_the_return_path_beside_the_token() {
        use dom_render_compiler::transforms::form::FORM_HIDDEN_INPUTS;

        let html = "<main><div id=\"__c_1\" data-albedo-tier=\"c\"></div></main>".to_string();
        let fills = vec![(
            "__c_1".to_string(),
            format!("<form data-albedo-action=\"sign\">{FORM_HIDDEN_INPUTS}</form>"),
        )];

        let out = fill_server_placeholders(html, &fills, "deadbeef", "/room/42?tab=chat");
        assert!(out.contains("value=\"deadbeef\""), "{out}");
        assert!(
            out.contains("value=\"/room/42?tab=chat\""),
            "the request's own path, not the route pattern: {out}"
        );
        assert!(
            !out.contains("value=\"\""),
            "neither hidden input may ship empty: {out}"
        );
    }

    #[test]
    fn test_transport_hint_script_sets_wt_endpoint_for_webtransport_mode() {
        let script = transport_hint_script(NegotiatedTransport::WebTransport, "/_albedo/wt", true);
        assert!(script.contains("__ALBEDO_ACTIVE_TRANSPORT__=\"webtransport\""));
        assert!(script.contains("__ALBEDO_WT_ENDPOINT__=\"/_albedo/wt\""));
        assert!(script.contains("__ALBEDO_LIVE__=true"));
    }

    /// The invariant the whole `__ALBEDO_LIVE__` design exists to hold:
    /// **the flag agrees with auto-subscribe.** `stream_route_over_webtransport`
    /// subscribes a page when `shared_slot_topics` is non-empty; if this ever
    /// returned `false` for such a route, the server would subscribe a session
    /// whose browser never opened the lane — which is exactly the bug that let
    /// a scalar `useSharedSlot` render dead (§ 2e).
    #[test]
    fn a_route_that_auto_subscribes_always_asks_for_the_lane() {
        let mut route = route_manifest();
        route.shared_slot_topics = vec!["lobby:counter".to_string()];
        route.tier_b.clear();
        route.tier_c.clear();
        assert!(
            route_needs_live_lane(&route),
            "a scalar-only topic route must still get the lane"
        );
    }

    /// PRISM · the partition-only route — a chat, a per-user dashboard, the
    /// shape dynamic topics exist for. It has no compile-time topic at all, so
    /// asking `shared_slot_topics` alone answers "static" and the browser never
    /// opens the lane: correct HTML on load, dead forever after. In a project
    /// that also has a static topic this stays masked (that list is
    /// project-wide), which is exactly why it is pinned here rather than left to
    /// a demo app to reveal.
    #[test]
    fn a_partition_only_route_still_asks_for_the_lane() {
        let mut route = route_manifest();
        route.shared_slot_topics.clear();
        route.tier_b.clear();
        route.tier_c.clear();
        route.shared_slot_partitions = vec![PartitionTopicSpec {
            binding: "rows".to_string(),
            collection: "messages".to_string(),
            column: "room".to_string(),
            key: dom_render_compiler::manifest::schema::PartitionKeySource::RouteParam("id".to_string()),
        }];
        assert!(route_needs_live_lane(&route));
    }

    /// The converse: a genuinely static route must NOT pay for a connection.
    /// This is what stops the flag degrading into "always true", which would
    /// hold one socket per tab on pages with nothing to deliver — and the
    /// per-tab connection budget is already the scarcest thing we have.
    #[test]
    fn a_fully_static_route_does_not_open_a_lane() {
        let mut route = route_manifest();
        route.shared_slot_topics.clear();
        route.shared_slot_partitions.clear();
        route.tier_b.clear();
        route.tier_c.clear();
        assert!(!route_needs_live_lane(&route));
    }

    /// An island route with no topics still receives patch frames.
    #[test]
    fn an_island_route_without_topics_still_gets_the_lane() {
        let mut route = route_manifest();
        route.shared_slot_topics.clear();
        route.tier_c.clear();
        assert!(
            !route.tier_b.is_empty(),
            "fixture must carry a Tier-B node for this to mean anything"
        );
        assert!(route_needs_live_lane(&route));
    }

    #[test]
    fn test_parse_webtransport_session_header() {
        let req = test_request(
            &[(WT_SESSION_HEADER, "00000000-0000-0000-0000-000000000001")],
            Version::HTTP_11,
        );
        let session_id = maybe_webtransport_session_id(&req).unwrap();
        assert_eq!(
            session_id,
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
        );
    }

    /// Phase-E port of the old JSON-shell+JSON-patch+JSON-control test.
    /// The WT path now ships shell HTML as raw text on slot 1, binary
    /// opcode frames on slot 2, and a JSON `route_complete` envelope on
    /// slot 0. The test asserts each.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn stream_route_over_webtransport_ships_shell_binary_patches_and_route_complete() {
        use crate::render::tier_b::StubTierBOpcodeRegistry;
        use dom_render_compiler::graph::ComponentGraph;
        use dom_render_compiler::ir::wire::decode_frame;
        use dom_render_compiler::manifest::schema::Tier;
        use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
        use dom_render_compiler::runtime::scheduler::SchedulerConfig;
        use dom_render_compiler::runtime::webtransport::WT_STREAM_SLOT_PATCHES;
        use dom_render_compiler::types::{Component, ComponentAnalysis, ComponentId};
        use std::collections::HashMap;

        let session_id = Uuid::new_v4();
        let registry = WebTransportSessionRegistry::default();

        let (control_tx, mut control_rx) = mpsc::channel(8);
        let (shell_tx, mut shell_rx) = mpsc::channel(8);
        let (patch_tx, mut patch_rx) = mpsc::channel(8);
        let (prefetch_tx, _prefetch_rx) = mpsc::channel(8);

        registry.insert(WebTransportSessionHandle {
            session_id,
            remote_addr: "127.0.0.1:4433".parse().unwrap(),
            stream_senders: [control_tx, shell_tx, patch_tx, prefetch_tx],
        });

        // Build a minimal pipeline with one async-capable component so
        // the WT path has a valid pipeline + opcode registry to bind to.
        let graph = ComponentGraph::new();
        let id = graph.add_component(Component::new(ComponentId::new(0), "Feature".to_string()));
        let mut analyses = HashMap::new();
        analyses.insert(
            id,
            ComponentAnalysis {
                id,
                priority: 1.0,
                estimated_time_ms: 1.0,
                phase: 0.1,
                topological_level: 0,
            },
        );
        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .expect("pipeline must build");

        let services = SharedRenderServices {
            opcode_registry: Some(Arc::new(StubTierBOpcodeRegistry)),
            ..SharedRenderServices::default()
        };

        let app = Arc::new(
            StreamingAppState::new(
                Arc::new(RenderManifestV2::legacy_defaults()),
                services,
                StreamingTransportConfig::new(true, 443),
                Some(registry),
            )
            .with_pipeline(pipeline, tokio::runtime::Handle::current()),
        );

        let route = route_manifest();
        let ctx = TierBRequestContext {
            path: "/stream".to_string(),
            ..TierBRequestContext::default()
        };

        stream_route_over_webtransport(route, ctx, app, session_id)
            .await
            .unwrap();

        // Slot 1: shell HTML shipped as raw UTF-8.
        let shell_bytes = shell_rx.recv().await.unwrap();
        let shell_html = std::str::from_utf8(&shell_bytes).expect("shell must be UTF-8");
        assert!(
            shell_html.contains("data-albedo-tier=\"b\""),
            "shell HTML must include the Tier-B placeholder marker"
        );

        // Slot 2: at least one binary OpcodeFrame carrying the
        // Placeholder opcode for the lone Tier-B node. The multi-
        // thread runtime can race the stub resolver to completion
        // before the placeholder drain, in which case the Patch
        // arrives first and the Placeholder follows. Drain up to a
        // small bounded number of frames and assert any of them
        // carries the Placeholder. The wire shape that matters for
        // this test is "the Placeholder eventually ships", not
        // strict ordering against a same-tick resolution.
        let mut saw_placeholder = false;
        for _ in 0..4 {
            let Some(bytes) = patch_rx.recv().await else {
                break;
            };
            let (frame, _) = decode_frame(&bytes).expect("patch bytes must decode");
            if frame.instructions.iter().any(|instr| matches!(
                instr,
                dom_render_compiler::ir::opcode::Instruction::Placeholder { .. }
            )) {
                saw_placeholder = true;
                break;
            }
        }
        assert!(
            saw_placeholder,
            "binary frames on slot {WT_STREAM_SLOT_PATCHES} must include a Placeholder"
        );

        // Slot 0: route_complete JSON envelope.
        let control_payload: Value =
            serde_json::from_slice(&control_rx.recv().await.unwrap()).unwrap();
        assert_eq!(
            control_payload.get("event").and_then(Value::as_str),
            Some("route_complete")
        );
    }

    /// Phase P · Stream C.4 — when a route's manifest declares
    /// `shared_slot_topics`, the streaming handler must call
    /// `BroadcastRegistry::auto_subscribe` against the WT session's
    /// patches-lane sender and ship a `SlotSet` opcode frame
    /// carrying each topic's current value BEFORE the bootstrap
    /// intern table. Without this pass, the client paints a blank
    /// `useSharedSlot` binding until the first explicit `write_topic`
    /// — Stream C.4 closes that race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn stream_route_over_webtransport_auto_subscribes_to_shared_slot_topics() {
        use crate::render::tier_b::StubTierBOpcodeRegistry;
        use dom_render_compiler::graph::ComponentGraph;
        use dom_render_compiler::ir::opcode::Instruction;
        use dom_render_compiler::ir::wire::decode_frame;
        use dom_render_compiler::manifest::schema::Tier;
        use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
        use dom_render_compiler::runtime::scheduler::SchedulerConfig;
        use dom_render_compiler::runtime::{broadcast_slot_id, BroadcastRegistry};
        use dom_render_compiler::types::{Component, ComponentAnalysis, ComponentId};
        use std::collections::HashMap;

        let session_id = Uuid::new_v4();
        let registry = WebTransportSessionRegistry::default();

        let (control_tx, _control_rx) = mpsc::channel(8);
        let (shell_tx, mut shell_rx) = mpsc::channel(8);
        let (patch_tx, mut patch_rx) = mpsc::channel(8);
        let (prefetch_tx, _prefetch_rx) = mpsc::channel(8);

        registry.insert(WebTransportSessionHandle {
            session_id,
            remote_addr: "127.0.0.1:4433".parse().unwrap(),
            stream_senders: [control_tx, shell_tx, patch_tx, prefetch_tx],
        });

        // Build a pipeline + opcode registry just so
        // `stream_route_over_webtransport` clears its prerequisite
        // checks; the test's assertion is about the C.4 auto-subscribe
        // path, not the Tier-B / pipeline behaviour.
        let graph = ComponentGraph::new();
        let id = graph.add_component(Component::new(ComponentId::new(0), "Feature".to_string()));
        let mut analyses = HashMap::new();
        analyses.insert(
            id,
            ComponentAnalysis {
                id,
                priority: 1.0,
                estimated_time_ms: 1.0,
                phase: 0.1,
                topological_level: 0,
            },
        );
        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .expect("pipeline must build");

        let services = SharedRenderServices {
            opcode_registry: Some(Arc::new(StubTierBOpcodeRegistry)),
            ..SharedRenderServices::default()
        };

        // Pre-seed the topic so the auto-subscribe initial frame
        // carries a meaningful current value (not the `b"null"`
        // default).
        let broadcast = Arc::new(BroadcastRegistry::new());
        let seed_bytes = serde_json::to_vec(&serde_json::json!(["alpha", "beta"])).unwrap();
        broadcast.topic("chat:lobby", seed_bytes.clone());

        let app = Arc::new(
            StreamingAppState::new(
                Arc::new(RenderManifestV2::legacy_defaults()),
                services,
                StreamingTransportConfig::new(true, 443),
                Some(registry),
            )
            .with_pipeline(pipeline, tokio::runtime::Handle::current())
            .with_broadcast(broadcast.clone()),
        );

        // Route manifest that references one shared topic — Stream B
        // populates this field at build time from
        // `CompiledProject::shared_slot_topics()`.
        let mut route = route_manifest();
        route.shared_slot_topics = vec!["chat:lobby".to_string()];

        let ctx = TierBRequestContext {
            path: "/stream".to_string(),
            ..TierBRequestContext::default()
        };

        stream_route_over_webtransport(route, ctx, app, session_id)
            .await
            .unwrap();

        // Drain shell so the patches assertion isn't shadowed by the
        // unrelated shell payload (different lane anyway, but
        // belt-and-braces).
        let _ = shell_rx.recv().await;

        // FIRST patches-lane payload must be the auto-subscribe
        // initial-state frame: one SlotSet whose slot_id ==
        // broadcast_slot_id("chat:lobby"), value == the seeded JSON
        // bytes. The bootstrap intern table (step 2) ships after.
        let first_patch = patch_rx
            .recv()
            .await
            .expect("auto-subscribe must ship a patches-lane frame");
        let (frame, _) = decode_frame(&first_patch).expect("decode auto-subscribe frame");
        assert_eq!(
            frame.instructions.len(),
            1,
            "initial-state frame must carry exactly one SlotSet per topic"
        );
        match &frame.instructions[0] {
            Instruction::SlotSet { slot_id, value } => {
                assert_eq!(*slot_id, broadcast_slot_id("chat:lobby"));
                assert_eq!(value, &seed_bytes);
            }
            other => panic!("expected SlotSet, got {other:?}"),
        }
    }

    /// Phase P · C.4 negative — when the route declares no shared
    /// topics, the auto-subscribe pass is skipped and the very first
    /// patches-lane frame is the existing bootstrap (or whatever the
    /// pipeline ships). Pins the contract so a future refactor
    /// doesn't accidentally always-emit a SlotSet frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn stream_route_over_webtransport_skips_auto_subscribe_when_no_topics() {
        use crate::render::tier_b::StubTierBOpcodeRegistry;
        use dom_render_compiler::graph::ComponentGraph;
        use dom_render_compiler::ir::opcode::Instruction;
        use dom_render_compiler::ir::wire::decode_frame;
        use dom_render_compiler::manifest::schema::Tier;
        use dom_render_compiler::runtime::pipeline::FourLaneRuntimePipeline;
        use dom_render_compiler::runtime::scheduler::SchedulerConfig;
        use dom_render_compiler::runtime::BroadcastRegistry;
        use dom_render_compiler::types::{Component, ComponentAnalysis, ComponentId};
        use std::collections::HashMap;

        let session_id = Uuid::new_v4();
        let registry = WebTransportSessionRegistry::default();

        let (control_tx, _control_rx) = mpsc::channel(8);
        let (shell_tx, _shell_rx) = mpsc::channel(8);
        let (patch_tx, mut patch_rx) = mpsc::channel(8);
        let (prefetch_tx, _prefetch_rx) = mpsc::channel(8);

        registry.insert(WebTransportSessionHandle {
            session_id,
            remote_addr: "127.0.0.1:4433".parse().unwrap(),
            stream_senders: [control_tx, shell_tx, patch_tx, prefetch_tx],
        });

        let graph = ComponentGraph::new();
        let id = graph.add_component(Component::new(ComponentId::new(0), "Feature".to_string()));
        let mut analyses = HashMap::new();
        analyses.insert(
            id,
            ComponentAnalysis {
                id,
                priority: 1.0,
                estimated_time_ms: 1.0,
                phase: 0.1,
                topological_level: 0,
            },
        );
        let pipeline = FourLaneRuntimePipeline::new(
            &graph,
            analyses,
            HashMap::from([(id, Tier::B)]),
            &[],
            SchedulerConfig::default(),
            32,
        )
        .expect("pipeline must build");

        let app = Arc::new(
            StreamingAppState::new(
                Arc::new(RenderManifestV2::legacy_defaults()),
                SharedRenderServices {
                    opcode_registry: Some(Arc::new(StubTierBOpcodeRegistry)),
                    ..SharedRenderServices::default()
                },
                StreamingTransportConfig::new(true, 443),
                Some(registry),
            )
            .with_pipeline(pipeline, tokio::runtime::Handle::current())
            .with_broadcast(Arc::new(BroadcastRegistry::new())),
        );

        // No shared_slot_topics — auto-subscribe must skip entirely.
        let route = route_manifest();
        let ctx = TierBRequestContext {
            path: "/stream".to_string(),
            ..TierBRequestContext::default()
        };

        stream_route_over_webtransport(route, ctx, app, session_id)
            .await
            .unwrap();

        // Whatever the first patches-lane frame is, it must NOT be a
        // bare-SlotSet auto-subscribe frame (slot 0 is bootstrap,
        // which always carries either an empty instruction vec or an
        // intern table, never a top-level SlotSet).
        if let Some(first_patch) = patch_rx.recv().await {
            let (frame, _) = decode_frame(&first_patch).expect("decode patches frame");
            let is_bare_slot_set = frame.instructions.len() == 1
                && matches!(&frame.instructions[0], Instruction::SlotSet { .. });
            assert!(
                !is_bare_slot_set,
                "with no shared topics, the first patches-lane frame must not be a \
                 lone SlotSet (auto-subscribe should not have fired)"
            );
        }
    }

    #[tokio::test]
    async fn test_webtransport_capability_response_reports_session_count() {
        let session_id = Uuid::new_v4();
        let registry = WebTransportSessionRegistry::default();
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (shell_tx, _shell_rx) = mpsc::channel(1);
        let (patch_tx, _patch_rx) = mpsc::channel(1);
        let (prefetch_tx, _prefetch_rx) = mpsc::channel(1);

        registry.insert(WebTransportSessionHandle {
            session_id,
            remote_addr: "127.0.0.1:4433".parse().unwrap(),
            stream_senders: [control_tx, shell_tx, patch_tx, prefetch_tx],
        });

        let app = StreamingAppState::new(
            Arc::new(RenderManifestV2::legacy_defaults()),
            SharedRenderServices::default(),
            StreamingTransportConfig::new(true, 443),
            Some(registry),
        );

        let response = webtransport_capability_response(&app, NegotiatedTransport::WebTransport);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-albedo-transport")
                .and_then(|value| value.to_str().ok()),
            Some("webtransport")
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload.get("active_sessions").and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            payload.get("webtransport_path").and_then(Value::as_str),
            Some("/_albedo/wt")
        );
    }
}
