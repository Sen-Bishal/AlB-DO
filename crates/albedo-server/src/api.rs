//! Phase-F — JSON / raw API handler surface.
//!
//! Distinct from [`crate::contract::RouteHandler`] which produces
//! page-shaped `ResponsePayload`s (layout-wrapped, optionally
//! streaming). `ApiHandler` returns a flat `ApiResponse` with explicit
//! status, headers, and body — the contract userland code wants for
//! `/api/*` endpoints.
//!
//! Handlers are registered via
//! [`crate::AlbedoServerBuilder::register_api_handler`] and dispatched
//! by [`crate::handlers::api::dispatch_api_route`]. Auth flows through
//! the existing [`crate::contract::AuthProvider`] using the
//! `RouteTarget.auth` policy attached to the matched route.

use crate::error::RuntimeError;
use crate::lifecycle::RequestContext;
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use bytes::Bytes;
use serde::Serialize;
use std::collections::BTreeMap;

/// API-endpoint response: explicit status, header map, and a single
/// `Bytes` body. No layout wrapping, no streaming — API handlers ship a
/// complete payload per request.
#[derive(Debug, Clone)]
pub struct ApiResponse {
    pub status: StatusCode,
    pub headers: BTreeMap<String, String>,
    pub body: Bytes,
}

impl ApiResponse {
    /// 200 OK with the supplied body. Caller is responsible for setting
    /// `content-type` via [`Self::with_header`] when the body is not
    /// plain bytes.
    #[must_use]
    pub fn ok(body: impl Into<Bytes>) -> Self {
        Self {
            status: StatusCode::OK,
            headers: BTreeMap::new(),
            body: body.into(),
        }
    }

    /// 200 OK with a JSON-encoded body and `content-type:
    /// application/json` set.
    pub fn json<T: Serialize>(value: &T) -> Result<Self, RuntimeError> {
        let encoded = serde_json::to_vec(value).map_err(|err| {
            RuntimeError::RequestHandling(format!("failed to encode JSON response: {err}"))
        })?;
        Ok(Self::ok(Bytes::from(encoded))
            .with_header("content-type", "application/json"))
    }

    /// Empty response with the supplied status — handy for 204, 404, etc.
    #[must_use]
    pub fn status(status: StatusCode) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: Bytes::new(),
        }
    }

    /// Returns `self` with the supplied header set. Header name is
    /// lower-cased to match the rest of the lifecycle layer's
    /// normalisation.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers
            .insert(name.into().to_lowercase(), value.into());
        self
    }

    /// Returns `self` with the body replaced.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }
}

impl IntoResponse for ApiResponse {
    fn into_response(self) -> Response<Body> {
        let mut response = Response::new(Body::from(self.body));
        *response.status_mut() = self.status;
        for (name, value) in self.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value.as_str()),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        response
    }
}

/// User-implemented API endpoint. Receives the fully-built
/// `RequestContext` (params, query, headers, body) and returns either
/// an [`ApiResponse`] or a [`RuntimeError`] that the dispatcher maps
/// to a typed HTTP error response.
#[async_trait]
pub trait ApiHandler: Send + Sync {
    async fn handle(&self, ctx: RequestContext) -> Result<ApiResponse, RuntimeError>;
}

/// Blanket impl so any `async Fn(RequestContext) -> Result<ApiResponse,
/// RuntimeError>` can be registered directly — mirrors the
/// `RouteHandler` ergonomics in [`crate::contract`].
#[async_trait]
impl<F, Fut> ApiHandler for F
where
    F: Send + Sync + Fn(RequestContext) -> Fut,
    Fut: std::future::Future<Output = Result<ApiResponse, RuntimeError>> + Send,
{
    async fn handle(&self, ctx: RequestContext) -> Result<ApiResponse, RuntimeError> {
        (self)(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_builds_a_200_response_with_no_headers() {
        let response = ApiResponse::ok(Bytes::from_static(b"hi"));
        assert_eq!(response.status, StatusCode::OK);
        assert!(response.headers.is_empty());
        assert_eq!(response.body.as_ref(), b"hi");
    }

    #[test]
    fn json_sets_content_type_and_encodes_value() {
        let response = ApiResponse::json(&serde_json::json!({"k": 1})).unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.headers.get("content-type").map(String::as_str),
            Some("application/json"),
        );
        assert_eq!(response.body.as_ref(), br#"{"k":1}"#);
    }

    #[test]
    fn with_header_lowercases_the_name() {
        let response = ApiResponse::status(StatusCode::NO_CONTENT)
            .with_header("X-Trace-Id", "abc");
        assert_eq!(
            response.headers.get("x-trace-id").map(String::as_str),
            Some("abc"),
        );
    }

    #[tokio::test]
    async fn closure_impl_lets_callers_register_async_fns() {
        let handler = |ctx: RequestContext| async move { Ok(ApiResponse::ok(ctx.body)) };
        let ctx = RequestContext {
            request_id: "t".into(),
            method: crate::routing::HttpMethod::Post,
            path: "/api/echo".into(),
            query: BTreeMap::new(),
            params: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Bytes::from_static(b"payload"),
            metadata: BTreeMap::new(),
        };
        let response = handler.handle(ctx).await.unwrap();
        assert_eq!(response.body.as_ref(), b"payload");
    }
}
