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
    FramePayload, LaneRenderedChunk, WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PREFETCH,
    WT_STREAM_SLOT_SHELL,
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
        }
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

    let stream = build_stream(route, ctx, app, response_transport);

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::TRANSFER_ENCODING, "chunked")
        .header("x-content-type-options", "nosniff")
        .header("cache-control", "no-store")
        .header("x-albedo-transport", response_transport.as_header_value());

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

fn build_stream(
    route: RouteManifest,
    ctx: TierBRequestContext,
    app: Arc<StreamingAppState>,
    negotiated_transport: NegotiatedTransport,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    stream! {
        let shell = build_shell_chunk(
            &route,
            negotiated_transport,
            app.transport.webtransport_path.as_str(),
        );

        yield Ok(Bytes::from(shell));

        let mut tier_b_futures: FuturesUnordered<_> = route
            .tier_b
            .iter()
            .cloned()
            .map(|node| {
                let ctx = ctx.clone();
                let app = app.clone();
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
                        Ok(Err(err)) => InjectionChunk::error(&node, err),
                        Err(_) => InjectionChunk::fallback(&node),
                    }
                }
            })
            .collect();

        while let Some(chunk) = tier_b_futures.next().await {
            yield Ok(Bytes::from(chunk.into_script_tag()));
        }

        let mut closing = String::new();
        for node in &route.tier_c {
            if node.hydration_mode == HydrationMode::None {
                continue;
            }
            closing.push_str(&format!(
                "<script type=\"module\" src=\"{}\"></script>",
                node.bundle_path
            ));
            let component_id = serde_json::to_string(&node.component_id)
                .unwrap_or_else(|_| "\"\"".to_string());
            let placeholder_id = serde_json::to_string(&node.placeholder_id)
                .unwrap_or_else(|_| "\"\"".to_string());
            closing.push_str(&format!(
                "<script>__albedo_hydrate({},{},{})</script>",
                component_id,
                placeholder_id,
                node.initial_props
            ));
        }

        closing.push_str(&route.shell.body_close);
        yield Ok(Bytes::from(closing));
    }
}

fn build_shell_chunk(
    route: &RouteManifest,
    negotiated_transport: NegotiatedTransport,
    webtransport_path: &str,
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

    shell
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

    // 1. Shell HTML on the text slot.
    let mut shell = build_shell_chunk(
        &route,
        NegotiatedTransport::WebTransport,
        app.transport.webtransport_path.as_str(),
    );
    shell.push_str(&route.shell.body_close);
    sessions
        .send_payload(session_id, WT_STREAM_SLOT_SHELL, shell.into_bytes())
        .await
        .map_err(|err| err.to_string())?;

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

        // Slot 2: at least one binary OpcodeFrame carrying the Placeholder
        // opcode for the lone Tier-B node. Drain everything available; the
        // first frame must be a Placeholder. The Patch frame for the
        // resolution may or may not follow before the channel idles,
        // depending on scheduler timing — the wire shape that matters
        // for the test is the Placeholder.
        let first_patch = patch_rx.recv().await.expect("at least one binary patch frame");
        let (frame, _) = decode_frame(&first_patch).expect("patch bytes must decode");
        assert!(
            frame.instructions.iter().any(|instr| matches!(
                instr,
                dom_render_compiler::ir::opcode::Instruction::Placeholder { .. }
            )),
            "first binary frame on slot {WT_STREAM_SLOT_PATCHES} must carry a Placeholder"
        );

        // Slot 0: route_complete JSON envelope.
        let control_payload: Value =
            serde_json::from_slice(&control_rx.recv().await.unwrap()).unwrap();
        assert_eq!(
            control_payload.get("event").and_then(Value::as_str),
            Some("route_complete")
        );
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
