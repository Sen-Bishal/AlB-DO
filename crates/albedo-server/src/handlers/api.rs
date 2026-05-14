//! Phase-F — API route dispatch.
//!
//! Mirrors `execute_route` in [`crate::server`] but produces an
//! [`crate::api::ApiResponse`] instead of a page-shaped
//! `ResponsePayload`. The auth gate runs BEFORE the handler so a
//! protected endpoint never sees a request the policy would deny.

use crate::api::{ApiHandler, ApiResponse};
use crate::contract::{AuthDecision, AuthProvider};
use crate::error::RuntimeError;
use crate::lifecycle::RequestContext;
use crate::routing::RouteTarget;
use std::sync::Arc;

/// Resolves and invokes the API handler for `target`. Runs the auth
/// policy first (if any), then the registered handler. The dispatcher
/// does not run middleware — Phase-F MVP keeps the API path free of the
/// page-route middleware chain to avoid coupling layout-oriented hooks
/// to API responses. Phase G/H may revisit this.
pub async fn dispatch_api_route(
    target: &RouteTarget,
    ctx: RequestContext,
    auth_provider: &Arc<dyn AuthProvider>,
    handler: &Arc<dyn ApiHandler>,
) -> Result<ApiResponse, RuntimeError> {
    if let Some(policy) = &target.auth {
        match auth_provider.authorize(&ctx, policy).await? {
            AuthDecision::Allow => {}
            AuthDecision::Deny { reason } => {
                return Err(RuntimeError::Authentication(reason));
            }
        }
    }

    handler.handle(ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiResponse;
    use crate::contract::AllowAllAuthProvider;
    use crate::routing::{AuthPolicy, HttpMethod};
    use async_trait::async_trait;
    use axum::http::StatusCode;
    use bytes::Bytes;
    use std::collections::BTreeMap;

    fn ctx(method: HttpMethod, body: &[u8]) -> RequestContext {
        RequestContext {
            request_id: "test".into(),
            method,
            path: "/api/echo".into(),
            query: BTreeMap::new(),
            params: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Bytes::copy_from_slice(body),
            metadata: BTreeMap::new(),
        }
    }

    fn target(auth: Option<AuthPolicy>) -> RouteTarget {
        RouteTarget {
            route_name: "echo".into(),
            handler_id: "echo".into(),
            entry_module: None,
            props_loader: None,
            layout_handlers: Vec::new(),
            middleware: Vec::new(),
            auth,
        }
    }

    struct DenyAllAuth;

    #[async_trait]
    impl AuthProvider for DenyAllAuth {
        async fn authorize(
            &self,
            _ctx: &RequestContext,
            _policy: &AuthPolicy,
        ) -> Result<AuthDecision, RuntimeError> {
            Ok(AuthDecision::Deny {
                reason: "blocked".into(),
            })
        }
    }

    #[tokio::test]
    async fn dispatch_invokes_handler_when_no_auth_policy() {
        let handler: Arc<dyn ApiHandler> = Arc::new(|ctx: RequestContext| async move {
            Ok(ApiResponse::ok(ctx.body))
        });
        let auth: Arc<dyn AuthProvider> = Arc::new(AllowAllAuthProvider);

        let response = dispatch_api_route(
            &target(None),
            ctx(HttpMethod::Post, b"hello"),
            &auth,
            &handler,
        )
        .await
        .unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn dispatch_blocks_when_auth_policy_denies() {
        let handler: Arc<dyn ApiHandler> = Arc::new(|_ctx: RequestContext| async move {
            Ok(ApiResponse::ok(Bytes::from_static(b"unreached")))
        });
        let auth: Arc<dyn AuthProvider> = Arc::new(DenyAllAuth);

        let err = dispatch_api_route(
            &target(Some(AuthPolicy::Required)),
            ctx(HttpMethod::Get, b""),
            &auth,
            &handler,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, RuntimeError::Authentication(_)),
            "denied auth must surface as RuntimeError::Authentication, got {err:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_runs_handler_when_auth_policy_allows() {
        let handler: Arc<dyn ApiHandler> = Arc::new(|_ctx: RequestContext| async move {
            Ok(ApiResponse::ok(Bytes::from_static(b"ok")))
        });
        let auth: Arc<dyn AuthProvider> = Arc::new(AllowAllAuthProvider);

        let response = dispatch_api_route(
            &target(Some(AuthPolicy::Role("admin".into()))),
            ctx(HttpMethod::Get, b""),
            &auth,
            &handler,
        )
        .await
        .unwrap();

        assert_eq!(response.body.as_ref(), b"ok");
    }
}
