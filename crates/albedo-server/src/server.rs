//The following changes are gonna take place in this dir
//HotSetRegistry — the DashMap<ComponentId, RenderPriority> with register/deregister methods and a bounded size cap.
//RingNode — a struct with a ComponentId, an AtomicU8 dirty bit, and a raw pointer to the next node. The sentinel is a RingNode with a sentinel flag instead of a component ID.
//SentinelRing — owns the nodes, owns the AtomicU32 dirty counter, exposes two methods: mark_dirty(ComponentId) called by data sources, and drain(callback) called by the scheduler each frame which walks from sentinel, calls the callback on each dirty node, clears the bit, and stops at sentinel.

//Why raw pointers for the ring?
//The circular structure creates a reference cycle that Rust's borrow checker will reject if you use Box or Arc naively. Raw pointers with a clear ownership model — SentinelRing owns all nodes, no one else does — is the correct and idiomatic approach here. unsafe is contained entirely inside SentinelRing and the public API is fully safe.

//The only decision to make before writing code is which Ordering to use on the dirty bit. The safe default is AcqRel on the flip and Acquire on the read — guarantees the scheduler sees all writes that happened before the bit was set. We can tighten this to Relaxed later if benchmarks show the acquire fence is costing anything, but start correct then optimize.

// std::sync::atomic — AtomicU8 for the dirty bit on each node, AtomicU32 for the global dirty counter, AtomicUsize for the ring size. Ordering::Acquire and Ordering::Release for the memory ordering on dirty bit flips so the scheduler thread always sees a consistent write from the data source thread.
//std::ptr::NonNull — for the next pointer on each ring node. Safer than a raw *mut because it encodes non-nullability in the type, so you get a compile-time guarantee that your ring never has a broken link.
//std::ptr — read, write, for node traversal inside the unsafe block in drain.
//std::collections::HashSet — for the bounded size check on HotSetRegistry before allowing a new registration.

//From existing dependencies — already in Cargo.toml
//dashmap — already present. DashMap<ComponentId, RenderPriority> for the hot set registry. Lock-free concurrent reads from the scheduler, concurrent writes from data sources registering components.
//crossbeam::queue::ArrayQueue — already pulling in crossbeam. This is the bounded lock-free queue that sits between the ring drain and the render queue. The scheduler drains the ring into this, the renderer reads from it. Fixed capacity, no allocation on push.

use crate::config::AppConfig;
use crate::contract::{
    AllowAllAuthProvider, AuthDecision, AuthProvider, LayoutHandler, PropsLoader, RouteHandler,
    RuntimeMiddleware,
};
use crate::error::RuntimeError;
use crate::lifecycle::{RequestContext, ResponseBody, ResponsePayload};
use crate::renderer_runtime::RendererRuntime;
use crate::routing::{CompiledRouter, HttpMethod, RouteMatch, RouteTarget};
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
    props_loaders: Arc<HashMap<String, SharedPropsLoader>>,
    layouts: Arc<HashMap<String, SharedLayoutHandler>>,
    middleware: Arc<HashMap<String, SharedMiddleware>>,
    auth_provider: SharedAuthProvider,
    request_timeout: Duration,
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

        let has_entry_routes = self
            .config
            .routes
            .iter()
            .any(|route| route.entry_module.is_some());

        for route in &self.config.routes {
            if route.entry_module.is_none() && !self.handlers.contains_key(route.handler.as_str()) {
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
            props_loaders: Arc::new(self.props_loaders),
            layouts: Arc::new(self.layouts),
            middleware: Arc::new(self.middleware),
            auth_provider: self.auth_provider,
            request_timeout: Duration::from_millis(self.config.server.request_timeout_ms),
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

        let shutdown_timeout = Duration::from_millis(self.config.server.shutdown_timeout_ms);
        axum::serve(listener, self.router())
            .with_graceful_shutdown(shutdown_signal(shutdown_timeout))
            .await
            .map_err(|err| RuntimeError::ServerRuntime(err.to_string()))
    }
}

async fn dispatch(State(state): State<RuntimeState>, request: Request<Body>) -> Response {
    let method = match HttpMethod::try_from(request.method()) {
        Ok(method) => method,
        Err(err) => return err.into_response(),
    };

    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let (parts, body) = request.into_parts();
    let body = match to_bytes(body, MAX_REQUEST_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => {
            return RuntimeError::RequestBodyRead(err.to_string()).into_response();
        }
    };

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
