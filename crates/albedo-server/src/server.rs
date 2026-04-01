use crate::config::AppConfig;
use crate::contract::{
    AllowAllAuthProvider, AuthDecision, AuthProvider, LayoutHandler, PropsLoader, RouteHandler,
    RuntimeMiddleware,
};
use crate::error::RuntimeError;
use crate::handlers::{streaming_handler, StreamingAppState, StreamingTransportConfig};
use crate::lifecycle::{RequestContext, ResponseBody, ResponsePayload};
use crate::render::tier_b::SharedRenderServices;
use crate::renderer_runtime::RendererRuntime;
use crate::routing::{CompiledRouter, HttpMethod, RouteMatch, RouteTarget};
use crate::webtransport::{WebTransportRuntime, WebTransportSessionRegistry};
use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{error, info};

const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

type SharedHandler = Arc<dyn RouteHandler>;
type SharedLayoutHandler = Arc<dyn LayoutHandler>;
type SharedMiddleware = Arc<dyn RuntimeMiddleware>;
type SharedAuthProvider = Arc<dyn AuthProvider>;
type SharedPropsLoader = Arc<dyn PropsLoader>;

#[derive(Clone)]
struct RuntimeState {
    router: Arc<CompiledRouter>,
    handlers: Arc<HashMap<String, SharedHandler>>,
    layouts: Arc<HashMap<String, SharedLayoutHandler>>,
    middleware: Arc<HashMap<String, SharedMiddleware>>,
    auth_provider: SharedAuthProvider,
    request_timeout: Duration,
    streaming_runtime: Option<Arc<StreamingAppState>>,
}

pub struct AlbedoServerBuilder {
    config: AppConfig,
    handlers: HashMap<String, SharedHandler>,
    props_loaders: HashMap<String, SharedPropsLoader>,
    layouts: HashMap<String, SharedLayoutHandler>,
    middleware: HashMap<String, SharedMiddleware>,
    auth_provider: SharedAuthProvider,
    renderer: Option<RendererRuntime>,
}

impl AlbedoServerBuilder {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config,
            handlers: HashMap::new(),
            props_loaders: HashMap::new(),
            layouts: HashMap::new(),
            middleware: HashMap::new(),
            auth_provider: Arc::new(AllowAllAuthProvider),
            renderer: None,
        }
    }

    pub fn register_handler(
        mut self,
        handler_id: impl Into<String>,
        handler: impl RouteHandler + 'static,
    ) -> Self {
        self.handlers.insert(handler_id.into(), Arc::new(handler));
        self
    }

    pub fn register_props_loader(
        mut self,
        loader_id: impl Into<String>,
        loader: impl PropsLoader + 'static,
    ) -> Self {
        self.props_loaders
            .insert(loader_id.into(), Arc::new(loader));
        self
    }

    pub fn register_layout(
        mut self,
        layout_id: impl Into<String>,
        layout_handler: impl LayoutHandler + 'static,
    ) -> Self {
        self.layouts
            .insert(layout_id.into(), Arc::new(layout_handler));
        self
    }

    pub fn register_middleware(
        mut self,
        middleware_id: impl Into<String>,
        middleware: impl RuntimeMiddleware + 'static,
    ) -> Self {
        self.middleware
            .insert(middleware_id.into(), Arc::new(middleware));
        self
    }

    pub fn with_auth_provider(mut self, auth_provider: impl AuthProvider + 'static) -> Self {
        self.auth_provider = Arc::new(auth_provider);
        self
    }

    pub fn with_renderer_runtime(mut self, renderer: RendererRuntime) -> Self {
        self.renderer = Some(renderer);
        self
    }

    pub fn build(self) -> Result<AlbedoServer, RuntimeError> {
        self.config.validate()?;

        let router = CompiledRouter::from_route_and_layout_specs(
            self.config.routes.as_slice(),
            self.config.layouts.as_slice(),
        )?;

        let mut renderer = self.renderer;
        if renderer.is_none() {
            if let Some(renderer_config) = &self.config.renderer {
                renderer = Some(RendererRuntime::from_config(renderer_config)?);
            }
        }

        let shared_wt_sessions = self
            .config
            .server
            .webtransport
            .enabled
            .then(WebTransportSessionRegistry::default);

        let streaming_runtime = renderer.as_ref().map(|runtime| {
            Arc::new(StreamingAppState::new(
                Arc::new(runtime.manifest().clone()),
                SharedRenderServices::default(),
                StreamingTransportConfig::new(
                    self.config.server.webtransport.enabled,
                    self.config.server.port,
                ),
                shared_wt_sessions.clone(),
            ))
        });

        let has_entry_routes = self
            .config
            .routes
            .iter()
            .any(|route| route.entry_module.is_some());

        for route in &self.config.routes {
            let has_layout_handlers = match router.match_route(route.method, route.path.as_str()) {
                RouteMatch::Matched(matched) => !matched.target.layout_handlers.is_empty(),
                RouteMatch::MethodNotAllowed { .. } | RouteMatch::NotFound => true,
            };

            let route_uses_manifest_streaming =
                matches!(route.method, HttpMethod::Get | HttpMethod::Head)
                    && route.entry_module.is_some()
                    && route.props_loader.is_none()
                    && route.auth.is_none()
                    && route.middleware.is_empty()
                    && !has_layout_handlers
                    && streaming_runtime
                        .as_ref()
                        .map(|runtime| runtime.manifest.routes.contains_key(route.path.as_str()))
                        .unwrap_or(false);

            if !route_uses_manifest_streaming && !self.handlers.contains_key(route.handler.as_str())
            {
                return Err(RuntimeError::HandlerNotFound {
                    handler_id: route.handler.clone(),
                });
            }
            if let Some(props_loader_id) = &route.props_loader {
                if !self.props_loaders.contains_key(props_loader_id) {
                    return Err(RuntimeError::PropsLoaderNotFound {
                        loader_id: props_loader_id.clone(),
                    });
                }
            }
            for middleware in &route.middleware {
                if !self.middleware.contains_key(middleware.as_str()) {
                    return Err(RuntimeError::MiddlewareNotFound {
                        middleware_id: middleware.clone(),
                    });
                }
            }
        }
        if has_entry_routes && renderer.is_none() {
            return Err(RuntimeError::RendererNotConfigured);
        }
        for layout in &self.config.layouts {
            if !self.layouts.contains_key(layout.handler.as_str()) {
                return Err(RuntimeError::LayoutNotFound {
                    layout_id: layout.handler.clone(),
                });
            }
        }

        let state = RuntimeState {
            router: Arc::new(router),
            handlers: Arc::new(self.handlers),
            layouts: Arc::new(self.layouts),
            middleware: Arc::new(self.middleware),
            auth_provider: self.auth_provider,
            request_timeout: Duration::from_millis(self.config.server.request_timeout_ms),
            streaming_runtime,
        };

        Ok(AlbedoServer {
            config: self.config,
            state,
        })
    }
}

pub struct AlbedoServer {
    config: AppConfig,
    state: RuntimeState,
}

impl AlbedoServer {
    pub fn router(&self) -> Router {
        Router::new()
            .route("/", any(dispatch))
            .route("/{*path}", any(dispatch))
            .with_state(self.state.clone())
    }

    pub async fn run(self) -> Result<(), RuntimeError> {
        let addr = self.config.server.socket_addr()?;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|err| RuntimeError::ServerStartup(err.to_string()))?;
        info!("ALBEDO server listening on {}", addr);
        let router = self.router();

        let shutdown_timeout = Duration::from_millis(self.config.server.shutdown_timeout_ms);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let webtransport_task = if self.config.server.webtransport.enabled {
            let shared_sessions = self
                .state
                .streaming_runtime
                .as_ref()
                .and_then(|streaming| streaming.webtransport_sessions.clone())
                .unwrap_or_default();
            let runtime = WebTransportRuntime::bind_with_registry(
                addr,
                &self.config.server.webtransport,
                shared_sessions,
            )?;
            info!("ALBEDO WebTransport QUIC listener active on {}", addr);
            let wt_shutdown = shutdown_rx.clone();
            Some(tokio::spawn(async move { runtime.run(wt_shutdown).await }))
        } else {
            info!("ALBEDO WebTransport disabled; SSE/HTTP streaming fallback remains active");
            None
        };

        let graceful_shutdown = {
            let shutdown_tx = shutdown_tx.clone();
            async move {
                shutdown_signal(shutdown_timeout).await;
                let _ = shutdown_tx.send(true);
            }
        };

        let http_result = axum::serve(listener, router)
            .with_graceful_shutdown(graceful_shutdown)
            .await
            .map_err(|err| RuntimeError::ServerRuntime(err.to_string()));

        let _ = shutdown_tx.send(true);

        if let Some(task) = webtransport_task {
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(err) => {
                    return Err(RuntimeError::ServerRuntime(format!(
                        "webtransport task join failed: {err}"
                    )));
                }
            }
        }

        http_result
    }
}

async fn dispatch(State(state): State<RuntimeState>, request: Request<Body>) -> Response {
    let method = match HttpMethod::try_from(request.method()) {
        Ok(method) => method,
        Err(err) => return err.into_response(),
    };

    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);

    if path == "/_albedo/wt" {
        if let Some(streaming_runtime) = &state.streaming_runtime {
            return streaming_handler(State(streaming_runtime.clone()), request)
                .await
                .into_response();
        }
    }

    let route_match = state.router.match_route(method, path.as_str());
    let response = match route_match {
        RouteMatch::NotFound => RuntimeError::RouteNotFound {
            method: method.as_str().to_string(),
            path,
        }
        .into_response(),
        RouteMatch::MethodNotAllowed { allowed } => ResponsePayload::new(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("method '{}' is not allowed for this route", method.as_str()),
        )
        .with_header(
            "allow",
            allowed
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
        .into_response(),
        RouteMatch::Matched(matched) => {
            if should_use_manifest_streaming(&state, &matched.target, method, path.as_str()) {
                if let Some(streaming_runtime) = &state.streaming_runtime {
                    return streaming_handler(State(streaming_runtime.clone()), request)
                        .await
                        .into_response();
                }
            }

            let (parts, body) = request.into_parts();
            let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
                Ok(body) => body,
                Err(err) => {
                    return RuntimeError::RequestBodyRead(err.to_string()).into_response();
                }
            };

            let mut request_context = RequestContext::new(
                method,
                path.clone(),
                query.as_deref(),
                matched.params,
                &parts.headers,
                body,
            );

            match execute_route(&state, matched.target, &mut request_context).await {
                Ok(response) => response.into_response(),
                Err(err) => {
                    error!(request_id = request_context.request_id, error = %err, "request failed");
                    err.into_response()
                }
            }
        }
    };

    response
}

async fn execute_route(
    state: &RuntimeState,
    target: RouteTarget,
    ctx: &mut RequestContext,
) -> Result<ResponsePayload, RuntimeError> {
    for middleware_id in &target.middleware {
        let middleware = state.middleware.get(middleware_id).ok_or_else(|| {
            RuntimeError::MiddlewareNotFound {
                middleware_id: middleware_id.clone(),
            }
        })?;
        middleware.on_request(ctx).await?;
    }

    if let Some(policy) = &target.auth {
        match state.auth_provider.authorize(ctx, policy).await? {
            AuthDecision::Allow => {}
            AuthDecision::Deny { reason } => {
                return Err(RuntimeError::Authentication(reason));
            }
        }
    }

    let handler = state
        .handlers
        .get(target.handler_id.as_str())
        .ok_or_else(|| RuntimeError::HandlerNotFound {
            handler_id: target.handler_id.clone(),
        })?
        .clone();

    let ctx_for_response_hooks = ctx.clone();
    let response_fut = handler.handle(ctx.clone());
    let mut response = tokio::time::timeout(state.request_timeout, response_fut)
        .await
        .map_err(|_| {
            RuntimeError::RequestHandling(format!(
                "request timed out after {} ms",
                state.request_timeout.as_millis()
            ))
        })??;

    if !target.layout_handlers.is_empty() {
        apply_layout_handlers(state, target.layout_handlers.as_slice(), ctx, &mut response).await?;
    }

    for middleware_id in target.middleware.iter().rev() {
        let middleware = state.middleware.get(middleware_id).ok_or_else(|| {
            RuntimeError::MiddlewareNotFound {
                middleware_id: middleware_id.clone(),
            }
        })?;
        middleware
            .on_response(&ctx_for_response_hooks, &mut response)
            .await?;
    }

    Ok(response)
}
fn should_use_manifest_streaming(
    state: &RuntimeState,
    target: &RouteTarget,
    method: HttpMethod,
    path: &str,
) -> bool {
    if !matches!(method, HttpMethod::Get | HttpMethod::Head) {
        return false;
    }

    if target.entry_module.is_none() {
        return false;
    }

    if target.props_loader.is_some() || target.auth.is_some() {
        return false;
    }

    if !target.middleware.is_empty() || !target.layout_handlers.is_empty() {
        return false;
    }

    state
        .streaming_runtime
        .as_ref()
        .map(|runtime| runtime.manifest.routes.contains_key(path))
        .unwrap_or(false)
}

async fn apply_layout_handlers(
    state: &RuntimeState,
    layout_handlers: &[String],
    ctx: &RequestContext,
    response: &mut ResponsePayload,
) -> Result<(), RuntimeError> {
    if !response_is_html(response) {
        return Ok(());
    }

    let mut wrapped_html = match &response.body {
        ResponseBody::Full(body) => std::str::from_utf8(body.as_ref())
            .map_err(|err| {
                RuntimeError::RequestHandling(format!("failed to decode HTML body: {err}"))
            })?
            .to_string(),
        ResponseBody::Stream(chunks) => {
            let mut combined = Vec::new();
            for chunk in chunks {
                combined.extend_from_slice(chunk.as_ref());
            }
            std::str::from_utf8(combined.as_slice())
                .map_err(|err| {
                    RuntimeError::RequestHandling(format!(
                        "failed to decode streamed HTML body: {err}"
                    ))
                })?
                .to_string()
        }
    };

    for layout_id in layout_handlers.iter().rev() {
        let layout = state
            .layouts
            .get(layout_id)
            .ok_or_else(|| RuntimeError::LayoutNotFound {
                layout_id: layout_id.clone(),
            })?;
        wrapped_html = layout.wrap(ctx.clone(), wrapped_html).await?;
    }

    response.body = ResponseBody::Full(wrapped_html.into_bytes().into());
    response.headers.insert(
        "content-type".to_string(),
        "text/html; charset=utf-8".to_string(),
    );
    Ok(())
}

fn response_is_html(response: &ResponsePayload) -> bool {
    response
        .headers
        .get("content-type")
        .map(|value| value.to_ascii_lowercase().starts_with("text/html"))
        .unwrap_or(false)
}

async fn shutdown_signal(_timeout: Duration) {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RouteSpec, ServerConfig};
    use crate::routing::{AuthPolicy, HttpMethod};
    use axum::body::to_bytes;
    use bytes::Bytes;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_dynamic_route_dispatches_and_reads_param() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "users.show".to_string(),
                method: HttpMethod::Get,
                path: "/users/{id}".to_string(),
                handler: "users.show".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("users.show", |ctx: RequestContext| async move {
                let id = ctx.params.get("id").cloned().unwrap_or_default();
                Ok(ResponsePayload::ok_text(format!("user={id}")))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/users/42?include=profile")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "user=42");
    }

    #[tokio::test]
    async fn test_method_guard_returns_405_with_allow_header() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "users.show".to_string(),
                method: HttpMethod::Get,
                path: "/users/{id}".to_string(),
                handler: "users.show".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("users.show", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_text("ok"))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/users/42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = response
            .headers()
            .get("allow")
            .and_then(|value| value.to_str().ok());
        assert_eq!(allow, Some("GET"));
    }

    struct DenyAllAuth;

    #[async_trait::async_trait]
    impl AuthProvider for DenyAllAuth {
        async fn authorize(
            &self,
            _ctx: &RequestContext,
            _policy: &AuthPolicy,
        ) -> Result<AuthDecision, RuntimeError> {
            Ok(AuthDecision::Deny {
                reason: "blocked".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn test_auth_policy_blocks_request() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "private".to_string(),
                method: HttpMethod::Get,
                path: "/private".to_string(),
                handler: "private.handler".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: Some(AuthPolicy::Required),
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("private.handler", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_text("secret"))
            })
            .with_auth_provider(DenyAllAuth)
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/private")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_nested_layout_handlers_wrap_html_in_order() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: vec![
                crate::config::LayoutSpec {
                    name: "root".to_string(),
                    path: "/".to_string(),
                    handler: "layout.root".to_string(),
                },
                crate::config::LayoutSpec {
                    name: "dashboard".to_string(),
                    path: "/dashboard".to_string(),
                    handler: "layout.dashboard".to_string(),
                },
            ],
            routes: vec![RouteSpec {
                name: "dashboard.home".to_string(),
                method: HttpMethod::Get,
                path: "/dashboard".to_string(),
                handler: "dashboard.page".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("dashboard.page", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_html("<main>Dashboard</main>"))
            })
            .register_layout(
                "layout.root",
                |_ctx: RequestContext, inner: String| async move {
                    Ok(format!("<html><body>{inner}</body></html>"))
                },
            )
            .register_layout(
                "layout.dashboard",
                |_ctx: RequestContext, inner: String| async move {
                    Ok(format!("<section class=\"dashboard\">{inner}</section>"))
                },
            )
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/dashboard")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(
            body,
            "<html><body><section class=\"dashboard\"><main>Dashboard</main></section></body></html>"
        );
    }

    #[tokio::test]
    async fn test_streaming_html_response_chunks_are_emitted() {
        let config = AppConfig {
            server: ServerConfig::default(),
            renderer: None,
            layouts: Vec::new(),
            routes: vec![RouteSpec {
                name: "stream.page".to_string(),
                method: HttpMethod::Get,
                path: "/stream".to_string(),
                handler: "stream.page".to_string(),
                entry_module: None,
                props_loader: None,
                middleware: Vec::new(),
                auth: None,
            }],
        };

        let server = AlbedoServerBuilder::new(config)
            .register_handler("stream.page", |_ctx: RequestContext| async move {
                Ok(ResponsePayload::ok_html_stream([
                    Bytes::from_static(b"<main>"),
                    Bytes::from_static(b"ALBEDO"),
                    Bytes::from_static(b"</main>"),
                ]))
            })
            .build()
            .unwrap();

        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok());
        assert_eq!(content_type, Some("text/html; charset=utf-8"));
        let body = to_bytes(response.into_body(), MAX_REQUEST_BODY_BYTES)
            .await
            .unwrap();
        assert_eq!(body, "<main>ALBEDO</main>");
    }
}
