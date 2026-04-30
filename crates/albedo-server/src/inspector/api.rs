//! HTTP surface for the dev inspector — graph snapshot, SSE event stream,
//! metrics snapshot, and the static HTML shell.
//!
//! Dispatch is path-prefix matched on `/__albedo` from the parent server's
//! main router rather than mounted as a sub-router; the inspector lives in
//! the same Axum app, but its routes never collide with user routes because
//! the prefix is reserved.

use super::events::RenderEvent;
use super::state::InspectorState;
use axum::body::Body;
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use futures_util::stream::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;

const INSPECTOR_HTML: &str = include_str!("assets/inspector.html");

/// True when the request path falls under the inspector's reserved prefix.
pub fn matches_inspector_path(path: &str) -> bool {
    path == "/__albedo" || path.starts_with("/__albedo/")
}

/// Routes a request whose path passed [`matches_inspector_path`].
/// Always returns a `Response`; routes that don't exist render a 404.
pub fn dispatch(state: &Arc<InspectorState>, path: &str) -> Response<Body> {
    match path {
        "/__albedo" | "/__albedo/" => serve_html(),
        "/__albedo/api/graph" => serve_graph(state.as_ref()),
        "/__albedo/api/metrics" => serve_metrics(state.as_ref()),
        "/__albedo/api/events" => serve_events(state.clone()),
        _ => not_found(),
    }
}

fn serve_html() -> Response<Body> {
    let mut response = Response::new(Body::from(INSPECTOR_HTML));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

fn serve_graph(state: &InspectorState) -> Response<Body> {
    let snapshot = state.graph_snapshot();
    json_response(&snapshot)
}

fn serve_metrics(state: &InspectorState) -> Response<Body> {
    let mut snapshot = state.metrics().snapshot();
    // Lane utilization is owned by `InspectorState` (the runtime LaneObserver
    // writes it directly), not by the aggregator that consumes RenderEvents.
    // Overlay it here so `/api/metrics` returns one coherent shape.
    snapshot.lane_utilization = state.lane_utilization();
    json_response(&snapshot)
}

fn serve_events(state: Arc<InspectorState>) -> Response<Body> {
    let receiver = state.subscribe();
    let stream = BroadcastStream::new(receiver).filter_map(|item| async move {
        match item {
            Ok(event) => Some(Ok::<_, Infallible>(render_sse_event(&event))),
            // A lagged subscriber simply skips the dropped events; the next
            // successful frame brings them back in sync. This matches the
            // bounded-channel contract from `state.rs`.
            Err(_) => None,
        }
    });

    let sse = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    );

    sse.into_response()
}

fn render_sse_event(event: &RenderEvent) -> SseEvent {
    let payload = serde_json::to_string(event)
        .unwrap_or_else(|_| String::from("{\"error\":\"serialize_failed\"}"));
    SseEvent::default().event("render").data(payload)
}

fn json_response<T: serde::Serialize>(value: &T) -> Response<Body> {
    match serde_json::to_vec(value) {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
            response
        }
        Err(_) => {
            let mut response = Response::new(Body::from("{\"error\":\"serialize_failed\"}"));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response
        }
    }
}

fn not_found() -> Response<Body> {
    let mut response = Response::new(Body::from("inspector route not found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
