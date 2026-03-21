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

#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllAuthProvider;

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
