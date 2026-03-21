use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to read configuration file '{path}': {message}")]
    ConfigIo { path: String, message: String },
    #[error("failed to parse configuration file '{path}': {message}")]
    ConfigParse { path: String, message: String },
    #[error("route conflict for '{method} {path}': {message}")]
    RouteConflict {
        method: String,
        path: String,
        message: String,
    },
    #[error("invalid route path '{path}': {message}")]
    InvalidRoutePath { path: String, message: String },
    #[error("handler '{handler_id}' is not registered")]
    HandlerNotFound { handler_id: String },
    #[error("props loader '{loader_id}' is not registered")]
    PropsLoaderNotFound { loader_id: String },
    #[error("middleware '{middleware_id}' is not registered")]
    MiddlewareNotFound { middleware_id: String },
    #[error("layout handler '{layout_id}' is not registered")]
    LayoutNotFound { layout_id: String },
    #[error("renderer runtime is not configured")]
    RendererNotConfigured,
    #[error("renderer artifact read failed at '{path}': {message}")]
    RendererArtifactIo { path: String, message: String },
    #[error("renderer artifact parse failed at '{path}': {message}")]
    RendererArtifactParse { path: String, message: String },
    #[error("renderer failure: {0}")]
    RendererFailure(String),
    #[error("route not found: {method} {path}")]
    RouteNotFound { method: String, path: String },
    #[error("method not allowed for path '{path}'. allowed: {allowed:?}")]
    MethodNotAllowed { path: String, allowed: Vec<String> },
    #[error("request body read failed: {0}")]
    RequestBodyRead(String),
    #[error("request handling failed: {0}")]
    RequestHandling(String),
    #[error("authentication failed: {0}")]
    Authentication(String),
    #[error("server startup failed: {0}")]
    ServerStartup(String),
    #[error("server runtime failed: {0}")]
    ServerRuntime(String),
}

impl RuntimeError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidConfig(_)
            | Self::ConfigIo { .. }
            | Self::ConfigParse { .. }
            | Self::RouteConflict { .. }
            | Self::InvalidRoutePath { .. }
            | Self::HandlerNotFound { .. }
            | Self::PropsLoaderNotFound { .. }
            | Self::MiddlewareNotFound { .. }
            | Self::LayoutNotFound { .. }
            | Self::RendererNotConfigured
            | Self::RendererArtifactIo { .. }
            | Self::RendererArtifactParse { .. }
            | Self::RendererFailure(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::RouteNotFound { .. } => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            Self::RequestBodyRead(_) | Self::RequestHandling(_) => StatusCode::BAD_REQUEST,
            Self::Authentication(_) => StatusCode::UNAUTHORIZED,
            Self::ServerStartup(_) | Self::ServerRuntime(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for RuntimeError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let message = self.to_string();
        let body = axum::Json(ErrorBody { error: message });
        (status, body).into_response()
    }
}
