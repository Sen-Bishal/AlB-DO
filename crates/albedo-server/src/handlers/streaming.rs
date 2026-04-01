use crate::render::tier_b::{
    render_tier_b, InjectionChunk, RequestContext as TierBRequestContext, SharedRenderServices,
};
use crate::webtransport::WebTransportSessionRegistry;
use async_stream::stream;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode, Version};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use dom_render_compiler::manifest::schema::{
    HydrationMode, RenderManifestV2, RouteManifest, TierBNode,
};
use dom_render_compiler::runtime::webtransport::{
    WT_STREAM_SLOT_CONTROL, WT_STREAM_SLOT_PATCHES, WT_STREAM_SLOT_PREFETCH, WT_STREAM_SLOT_SHELL,
};
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};
use uuid::Uuid;

const WT_SESSION_HEADER: &str = "x-albedo-wt-session";
const WT_PREFER_HEADER: &str = "x-albedo-wt-prefer";

#[derive(Clone)]
pub struct StreamingAppState {
    pub manifest: Arc<RenderManifestV2>,
    pub services: SharedRenderServices,
    pub transport: StreamingTransportConfig,
    pub webtransport_sessions: Option<WebTransportSessionRegistry>,
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
        }
    }
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

    let mut shell = build_shell_chunk(
        &route,
        NegotiatedTransport::WebTransport,
        app.transport.webtransport_path.as_str(),
    );
    shell.push_str(&route.shell.body_close);

    sessions
        .send_json(session_id, WT_STREAM_SLOT_SHELL, &json!({ "html": shell }))
        .await
        .map_err(|err| err.to_string())?;

    let mut tier_b_futures: FuturesUnordered<_> = route
        .tier_b
        .iter()
        .cloned()
        .map(|node| {
            let ctx = ctx.clone();
            let app = app.clone();
            async move { render_tier_b_patch_payload(node, ctx, app).await }
        })
        .collect();

    while let Some(patch_payload) = tier_b_futures.next().await {
        sessions
            .send_json(session_id, WT_STREAM_SLOT_PATCHES, &patch_payload)
            .await
            .map_err(|err| err.to_string())?;
    }

    let mut prefetch_modules = Vec::new();
    for node in &route.tier_c {
        if node.hydration_mode == HydrationMode::None {
            continue;
        }

        prefetch_modules.push(node.bundle_path.clone());

        sessions
            .send_json(
                session_id,
                WT_STREAM_SLOT_PATCHES,
                &json!({
                    "hydrate": {
                        "component_id": node.component_id,
                        "placeholder_id": node.placeholder_id,
                        "props": node.initial_props,
                    }
                }),
            )
            .await
            .map_err(|err| err.to_string())?;
    }

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

async fn render_tier_b_patch_payload(
    node: TierBNode,
    ctx: TierBRequestContext,
    app: Arc<StreamingAppState>,
) -> Value {
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
        Ok(Ok(html)) => json!({
            "placeholder_id": node.placeholder_id,
            "html": html,
        }),
        Ok(Err(_)) => json!({
            "placeholder_id": node.placeholder_id,
            "html": Value::Null,
            "status": "error",
        }),
        Err(_) => json!({
            "placeholder_id": node.placeholder_id,
            "html": fallback_html(&node),
            "status": "fallback",
        }),
    }
}

fn fallback_html(node: &TierBNode) -> String {
    node.fallback_html
        .clone()
        .unwrap_or_else(|| "<div data-albedo-fallback=\"timeout\"></div>".to_string())
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

    #[tokio::test]
    async fn test_stream_route_over_webtransport_sends_shell_patch_and_control_frames() {
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

        let app = Arc::new(StreamingAppState::new(
            Arc::new(RenderManifestV2::legacy_defaults()),
            SharedRenderServices::default(),
            StreamingTransportConfig::new(true, 443),
            Some(registry),
        ));

        let route = route_manifest();
        let ctx = TierBRequestContext {
            path: "/stream".to_string(),
            ..TierBRequestContext::default()
        };

        stream_route_over_webtransport(route, ctx, app, session_id)
            .await
            .unwrap();

        let shell_payload: Value = serde_json::from_slice(&shell_rx.recv().await.unwrap()).unwrap();
        assert!(shell_payload
            .get("html")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("data-albedo-tier=\"b\""));

        let patch_payload: Value = serde_json::from_slice(&patch_rx.recv().await.unwrap()).unwrap();
        assert_eq!(
            patch_payload.get("placeholder_id").and_then(Value::as_str),
            Some("__b_feature")
        );
        assert!(patch_payload.get("html").and_then(Value::as_str).is_some());

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
