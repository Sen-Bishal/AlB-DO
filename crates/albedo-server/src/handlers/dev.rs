//! Phase M · dev-only HTTP handlers.
//!
//! Wires the dev `DevErrorRegistry` + `HmrRegistry` to public SSE
//! endpoints the in-browser overlay subscribes to. Only mounted when
//! the server has dev mode enabled — production routers skip these
//! handlers entirely.

use crate::dev::{HmrEvent, HmrRegistry, OverlayEvent, SharedErrorRegistry};
use axum::body::Body;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use futures_util::stream::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;

/// Embedded overlay script. Inlined via `include_str!` so the dev
/// server can serve it from `/_albedo/dev/overlay.js` without
/// touching the filesystem at runtime.
const OVERLAY_SCRIPT: &str = include_str!("../../../../assets/albedo-error-overlay.js");

/// Embedded slot-preserving HMR client. Same delivery model as the
/// overlay script above; production builds skip the route.
const HMR_APPLY_SCRIPT: &str = include_str!("../../../../assets/albedo-hmr-apply.js");

/// Returns the static overlay JS asset. Cache-control is `no-store`
/// because dev assets evolve mid-session; the browser must always
/// fetch the latest.
pub fn serve_overlay_script() -> Response<Body> {
    plain_asset(OVERLAY_SCRIPT, "application/javascript; charset=utf-8")
}

/// Returns the static HMR client JS asset.
pub fn serve_hmr_apply_script() -> Response<Body> {
    plain_asset(HMR_APPLY_SCRIPT, "application/javascript; charset=utf-8")
}

fn plain_asset(body: &'static str, content_type: &'static str) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

/// SSE stream for the floating error overlay. Each event is emitted
/// under the `overlay` event name with the serialized
/// `OverlayEvent` JSON as the data payload. Keep-alive every 15s to
/// survive idle proxies and dev-tunnel layers.
pub fn serve_error_stream(registry: SharedErrorRegistry) -> Response<Body> {
    let receiver = registry.subscribe();
    let stream = BroadcastStream::new(receiver).filter_map(|item| async move {
        match item {
            Ok(event) => Some(Ok::<_, Infallible>(render_overlay_event(&event))),
            // Lagged → skip silently; the next live event resyncs.
            Err(_) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

/// SSE stream for slot-preserving HMR applies. The wire shape
/// mirrors `serve_error_stream` so the client side can share its
/// connection-bookkeeping code if userland decides to.
pub fn serve_hmr_stream(registry: Arc<HmrRegistry>) -> Response<Body> {
    let receiver = registry.subscribe();
    let stream = BroadcastStream::new(receiver).filter_map(|item| async move {
        match item {
            Ok(event) => Some(Ok::<_, Infallible>(render_hmr_event(&event))),
            Err(_) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

fn render_overlay_event(event: &OverlayEvent) -> SseEvent {
    let payload = serde_json::to_string(event)
        .unwrap_or_else(|_| String::from("{\"event\":\"error\",\"id\":0,\"kind\":\"runtime\",\"message\":\"serialize_failed\",\"timestamp_ms\":0}"));
    SseEvent::default().event("overlay").data(payload)
}

fn render_hmr_event(event: &HmrEvent) -> SseEvent {
    let payload = serde_json::to_string(event)
        .unwrap_or_else(|_| String::from("{\"event\":\"reload\",\"revision\":0}"));
    SseEvent::default().event("hmr").data(payload)
}

/// 404 with a known shape for unmatched dev paths so misrouted
/// requests don't fall through to userland 500s.
pub fn dev_not_found() -> Response<Body> {
    let mut response = Response::new(Body::from("dev route not found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
