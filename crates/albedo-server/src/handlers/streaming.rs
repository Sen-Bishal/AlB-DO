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
use dom_render_compiler::manifest::schema::{HydrationMode, RenderManifestV2, RouteManifest};
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
use tracing::{info, warn};
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
        }
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
    let negotiated_transport = negotiate_transport(&req, &app.transport);

    if path == app.transport.webtransport_path {
        return webtransport_capability_response(app.as_ref(), negotiated_transport);
    }

    let Some(route) = app.manifest.routes.get(path.as_str()) else {
        return not_found_response();
    };

    let transport_config = app.transport.clone();
    let mut response_transport = negotiated_transport;
    let route = route.clone();
    let ctx = request_context_from_request(&req);

    // Phase L · resolve the per-session id used to address the CSRF
    // token table. Read from the `albedo-session` cookie when the
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
                        info!(
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
        let mut shell = build_shell_chunk(
            &route,
            negotiated_transport,
            app.transport.webtransport_path.as_str(),
            app.csrf().as_ref(),
            page_session,
            hydration,
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

        // Route boundary component names (`error.tsx` / `loading.tsx`), if any.
        // Captured once and cloned into each island future so a throwing or
        // slow Tier-B node can fall back to the route's declared boundary UI
        // instead of a blank `data-albedo-error` stub.
        let error_component = route.error_component.clone();
        let loading_component = route.loading_component.clone();

        let mut tier_b_futures: FuturesUnordered<_> = route
            .tier_b
            .iter()
            .cloned()
            .map(|node| {
                let ctx = ctx.clone();
                let app = app.clone();
                let error_component = error_component.clone();
                let loading_component = loading_component.clone();
                async move {
                    let render_result = timeout(
                        Duration::from_millis(node.timeout_ms.max(1)),
                        render_tier_b(
                            &node,
                            &ctx,
                            app.services.registry.as_ref(),
                            app.services.data_fetcher.as_ref(),
                        ),
                    )
                    .await;

                    match render_result {
                        Ok(Ok(html)) => InjectionChunk::success(&node, html),
                        Ok(Err(err)) => {
                            // The component threw. Render the route's `error.tsx`
                            // boundary and inject its HTML; only if there is no
                            // boundary (or it too fails) do we fall back to the
                            // blank error stub.
                            let error_props = json!({
                                "error": { "message": err.to_string() }
                            });
                            match render_route_boundary(
                                app.as_ref(),
                                error_component.as_deref(),
                                &error_props,
                            )
                            .await
                            {
                                Some(html) => InjectionChunk::error_boundary(&node, html),
                                None => InjectionChunk::error(&node, err),
                            }
                        }
                        Err(_) => {
                            // The component timed out. Prefer the route's
                            // `loading.tsx` UI over the generic timeout div.
                            match render_route_boundary(
                                app.as_ref(),
                                loading_component.as_deref(),
                                &json!({}),
                            )
                            .await
                            {
                                Some(html) => InjectionChunk::fallback_with_html(&node, html),
                                None => InjectionChunk::fallback(&node),
                            }
                        }
                    }
                }
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

        closing.push_str(&route.shell.body_close);
        yield Ok(Bytes::from(closing));
    }
}

fn build_shell_chunk(
    route: &RouteManifest,
    negotiated_transport: NegotiatedTransport,
    webtransport_path: &str,
    csrf: &crate::render::csrf::CsrfRegistry,
    page_session: dom_render_compiler::runtime::SessionId,
    hydration: Option<&crate::renderer_runtime::RouteHydration>,
) -> String {
    let mut shell = route.shell.doctype_and_head.clone();
    shell.push_str(&route.shell.body_open);
    shell.push_str(&transport_hint_script(
        negotiated_transport,
        webtransport_path,
    ));
    shell.push_str(&route.shell.shim_script);

    for node in &route.tier_a_root {
        shell = shell.replace(
            &format!("<!--__SLOT_{}-->", node.placeholder_id),
            &node.html,
        );
    }

    // A3 · replace each empty Tier-C placeholder with the island's marked SSR
    // HTML so the browser has real markup to adopt (and the user something to
    // interact with). The marker rides on the island's own root element.
    if let Some(hydration) = hydration {
        for (placeholder_id, marked_html) in &hydration.placeholders {
            let empty = format!("<div id=\"{placeholder_id}\" data-albedo-tier=\"c\"></div>");
            shell = shell.replace(&empty, marked_html);
        }
    }

    // Phase L · fill any `value=""` CSRF placeholders the renderer
    // stamped on form-action inputs. Mints the per-session token on
    // first access; subsequent calls in the same session return the
    // same value. No-op when the shell carries no `data-albedo-csrf`
    // markers (Tier-A pages without forms).
    let token = csrf.token_for(page_session);
    crate::render::csrf::substitute_csrf_token_in_html(&shell, &token)
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
    // `albedo-session` cookie (or `x-albedo-wt-session` header) when
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
        app.csrf().as_ref(),
        page_session,
        None,
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

fn request_context_from_request(req: &Request) -> TierBRequestContext {
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
        params: HashMap::new(),
        headers,
        cookies,
    }
}

fn parse_cookie_header(raw: &str) -> HashMap<String, String> {
    let mut cookies = HashMap::new();
    for pair in raw.split(';') {
        let trimmed = pair.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((name, value)) = trimmed.split_once('=') {
            cookies.insert(name.trim().to_string(), value.trim().to_string());
        }
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

fn transport_hint_script(transport: NegotiatedTransport, webtransport_path: &str) -> String {
    let endpoint = match transport {
        NegotiatedTransport::WebTransport => webtransport_path,
        NegotiatedTransport::Sse => "",
    };
    let endpoint_literal = serde_json::to_string(endpoint).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "<script>globalThis.__ALBEDO_ACTIVE_TRANSPORT__=\"{}\";globalThis.__ALBEDO_WT_ENDPOINT__={};</script>",
        transport.as_header_value(),
        endpoint_literal
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webtransport::WebTransportSessionHandle;
    use axum::body::to_bytes;
    use dom_render_compiler::manifest::schema::{
        DataDep, DataSource, DomPosition, HtmlShell, RenderedNode, RouteManifest, TierBNode,
    };
    use serde_json::Value;
    use tokio::sync::mpsc;

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
        let script = transport_hint_script(NegotiatedTransport::Sse, "/_albedo/wt");
        assert!(script.contains("__ALBEDO_ACTIVE_TRANSPORT__=\"sse\""));
        assert!(script.contains("__ALBEDO_WT_ENDPOINT__=\"\""));
    }

    #[test]
    fn test_transport_hint_script_sets_wt_endpoint_for_webtransport_mode() {
        let script = transport_hint_script(NegotiatedTransport::WebTransport, "/_albedo/wt");
        assert!(script.contains("__ALBEDO_ACTIVE_TRANSPORT__=\"webtransport\""));
        assert!(script.contains("__ALBEDO_WT_ENDPOINT__=\"/_albedo/wt\""));
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
