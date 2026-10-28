use crate::error::RuntimeError;
use crate::lifecycle::{RequestContext, ResponsePayload};
use crate::routing::AuthPolicy;
use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    Allow,
    Deny { reason: String },
}

#[async_trait]
pub trait AuthProvider: Send + Sync {
    async fn authorize(
        &self,
        ctx: &RequestContext,
        policy: &AuthPolicy,
    ) -> Result<AuthDecision, RuntimeError>;
}

/// Allows every request, whatever its route's policy.
///
/// **Not the default.** An embedder that genuinely wants every declared policy
/// ignored installs this explicitly with
/// [`AlbedoServerBuilder::with_auth_provider`](crate::AlbedoServerBuilder::with_auth_provider).
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllAuthProvider;

/// The default provider: what a server with no auth provider installed does
/// with a route that declares a policy.
///
/// **Fails closed.** `Optional` is allowed — the route asked for nothing. A
/// route declaring `Required` or `Role(..)` is refused, because nothing was
/// installed that could establish either; allowing it would turn a missing
/// line of configuration into a route anyone can reach.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnconfiguredAuthProvider;

#[async_trait]
impl AuthProvider for UnconfiguredAuthProvider {
    async fn authorize(
        &self,
        _ctx: &RequestContext,
        policy: &AuthPolicy,
    ) -> Result<AuthDecision, RuntimeError> {
        Ok(match policy {
            AuthPolicy::Optional => AuthDecision::Allow,
            AuthPolicy::Required | AuthPolicy::Role(_) => AuthDecision::Deny {
                reason: "this route requires authentication, and no auth provider is \
                         configured on this server"
                    .to_string(),
            },
        })
    }
}

#[async_trait]
impl AuthProvider for AllowAllAuthProvider {
    async fn authorize(
        &self,
        _ctx: &RequestContext,
        _policy: &AuthPolicy,
    ) -> Result<AuthDecision, RuntimeError> {
        Ok(AuthDecision::Allow)
    }
}

#[async_trait]
pub trait RuntimeMiddleware: Send + Sync {
    async fn on_request(&self, _ctx: &mut RequestContext) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn on_response(
        &self,
        _ctx: &RequestContext,
        _response: &mut ResponsePayload,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[async_trait]
pub trait LayoutHandler: Send + Sync {
    async fn wrap(&self, ctx: RequestContext, inner_html: String) -> Result<String, RuntimeError>;
}

#[async_trait]
impl<F, Fut> LayoutHandler for F
where
    F: Send + Sync + Fn(RequestContext, String) -> Fut,
    Fut: std::future::Future<Output = Result<String, RuntimeError>> + Send,
{
    async fn wrap(&self, ctx: RequestContext, inner_html: String) -> Result<String, RuntimeError> {
        (self)(ctx, inner_html).await
    }
}

#[async_trait]
pub trait RouteHandler: Send + Sync {
    async fn handle(&self, ctx: RequestContext) -> Result<ResponsePayload, RuntimeError>;
}

#[async_trait]
impl<F, Fut> RouteHandler for F
where
    F: Send + Sync + Fn(RequestContext) -> Fut,
    Fut: std::future::Future<Output = Result<ResponsePayload, RuntimeError>> + Send,
{
    async fn handle(&self, ctx: RequestContext) -> Result<ResponsePayload, RuntimeError> {
        (self)(ctx).await
    }
}

#[async_trait]
pub trait PropsLoader: Send + Sync {
    async fn load_props(&self, ctx: RequestContext) -> Result<Value, RuntimeError>;
}

#[async_trait]
impl<F, Fut> PropsLoader for F
where
    F: Send + Sync + Fn(RequestContext) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RuntimeError>> + Send,
{
    async fn load_props(&self, ctx: RequestContext) -> Result<Value, RuntimeError> {
        (self)(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::HttpMethod;

    fn ctx() -> RequestContext {
        RequestContext::new(
            HttpMethod::Get,
            "/admin".to_string(),
            None,
            std::collections::BTreeMap::new(),
            &axum::http::HeaderMap::new(),
            bytes::Bytes::new(),
        )
    }

    /// A route that declares a policy on a server that installed nothing to
    /// enforce it must be refused, not waved through.
    #[tokio::test]
    async fn the_default_provider_fails_closed() {
        let provider = UnconfiguredAuthProvider;
        assert_eq!(
            provider.authorize(&ctx(), &AuthPolicy::Optional).await.unwrap(),
            AuthDecision::Allow
        );
        for policy in [AuthPolicy::Required, AuthPolicy::Role("admin".into())] {
            assert!(
                matches!(
                    provider.authorize(&ctx(), &policy).await.unwrap(),
                    AuthDecision::Deny { .. }
                ),
                "{policy:?} must be refused without a provider"
            );
        }
    }
}
