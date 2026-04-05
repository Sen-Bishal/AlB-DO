//! # albedo-server
//!
//! Axum-based HTTP runtime for AlBDO compiled JSX/TSX applications.
//!
//! Consumes a [`RenderManifestV2`] produced by `dom-render-compiler` and wires it into
//! a production-ready axum server with a radix router, streaming support, WebTransport
//! muxing, middleware, layout injection, and lifecycle hooks.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use albedo_server::AlbedoServer;
//!
//! # async fn run() {
//! AlbedoServer::builder()
//!     .port(3000)
//!     .build()
//!     .serve()
//!     .await
//!     .unwrap();
//! # }
//! ```
//!
//! ## Architecture
//!
//! | Component | Role |
//! |-----------|------|
//! | [`AlbedoServer`] / [`AlbedoServerBuilder`] | Top-level server builder and entry point |
//! | [`CompiledRouter`] | Radix router over compiled route manifest |
//! | [`RendererRuntime`] | Loads manifest and module sources from disk |
//! | [`TierBRenderRegistry`] | Server-side island render registry |
//! | [`WebTransportRuntime`] | HTTP/3 WebTransport session manager |
//! | [`RequestContext`] / [`ResponsePayload`] | Per-request lifecycle types |

#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![warn(clippy::unwrap_used)]
#![warn(clippy::expect_used)]
#![deny(clippy::todo)]
#![warn(rustdoc::broken_intra_doc_links)]
#![warn(rustdoc::missing_crate_level_docs)]

pub mod config;
pub mod contract;
pub mod error;
pub mod handlers;
pub mod lifecycle;
pub mod render;
pub mod renderer_runtime;
pub mod routing;
pub mod server;
pub mod webtransport;

pub use config::{AppConfig, LayoutSpec, RendererConfig, RouteSpec, ServerConfig};
pub use contract::{
    AllowAllAuthProvider, AuthDecision, AuthProvider, LayoutHandler, PropsLoader, RouteHandler,
    RuntimeMiddleware,
};
pub use error::RuntimeError;
pub use handlers::{streaming_handler, StreamingAppState, StreamingTransportConfig};
pub use lifecycle::{RequestContext, ResponseBody, ResponsePayload};
pub use render::{
    InjectionChunk, RenderError as TierBRenderError, TierBDataFetcher, TierBRenderRegistry,
};
pub use renderer_runtime::{
    RendererRuntime, RENDER_MANIFEST_FILENAME, RUNTIME_MODULE_SOURCES_FILENAME,
};
pub use routing::{AuthPolicy, CompiledRouter, HttpMethod, MatchedRoute, RouteMatch, RouteTarget};
pub use server::{AlbedoServer, AlbedoServerBuilder};
pub use webtransport::{
    WebTransportRuntime, WebTransportSessionHandle, WebTransportSessionRegistry,
};
