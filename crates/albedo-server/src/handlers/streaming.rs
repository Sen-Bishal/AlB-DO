use crate::render::tier_b::{
    render_tier_b, InjectionChunk, RequestContext as TierBRequestContext, SharedRenderServices,
};
use async_stream::stream;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use dom_render_compiler::manifest::schema::{HydrationMode, RenderManifestV2, RouteManifest};
use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{timeout, Duration};

#[derive(Clone)]
pub struct StreamingAppState {
    pub manifest: Arc<RenderManifestV2>,
    pub services: SharedRenderServices,
}

impl StreamingAppState {
    pub fn new(manifest: Arc<RenderManifestV2>, services: SharedRenderServices) -> Self {
        Self { manifest, services }
    }
}

pub async fn streaming_handler(
    State(app): State<Arc<StreamingAppState>>,
    req: Request,
) -> impl IntoResponse {
    let path = req.uri().path().to_string();

    let Some(route) = app.manifest.routes.get(path.as_str()) else {
        return not_found_response();
    };

    let route = route.clone();
    let ctx = request_context_from_request(&req);
    let stream = build_stream(route, ctx, app);

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::TRANSFER_ENCODING, "chunked")
        .header("x-content-type-options", "nosniff")
        .header("cache-control", "no-store")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| Response::new(Body::from("failed to build streaming response")))
}

fn build_stream(
    route: RouteManifest,
    ctx: TierBRequestContext,
    app: Arc<StreamingAppState>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    stream! {
        let mut shell = route.shell.doctype_and_head.clone();
        shell.push_str(&route.shell.body_open);
        shell.push_str(&route.shell.shim_script);

        for node in &route.tier_a_root {
            shell = shell.replace(
                &format!("<!--__SLOT_{}-->", node.placeholder_id),
                &node.html,
            );
        }

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
            let component_id = serde_json::to_string(&node.component_id).unwrap_or_else(|_| "\"\"".to_string());
            let placeholder_id = serde_json::to_string(&node.placeholder_id).unwrap_or_else(|_| "\"\"".to_string());
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
        .status(404)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("route not found"))
        .unwrap_or_else(|_| Response::new(Body::from("route not found")))
}
