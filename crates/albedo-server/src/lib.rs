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
//! use albedo_server::{AlbedoServerBuilder, AppConfig};
//!
//! # async fn run() {
//! AlbedoServerBuilder::new(AppConfig::default())
//!     .build()
//!     .unwrap()
//!     .run()
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

pub mod actions;
pub mod api;
pub mod config;
pub mod contract;
pub mod dev;
pub mod error;
pub mod handlers;
pub mod inspector;
pub mod lifecycle;
pub mod render;
pub mod renderer_runtime;
pub mod routing;
pub mod server;
pub mod webtransport;

pub use actions::{ActionHandler, SessionSlots};
pub use api::{ApiHandler, ApiResponse};
pub use config::{AppConfig, LayoutSpec, RendererConfig, RouteSpec, ServerConfig};
pub use contract::{
    AllowAllAuthProvider, AuthDecision, AuthProvider, LayoutHandler, PropsLoader, RouteHandler,
    RuntimeMiddleware,
};
pub use error::RuntimeError;
pub use handlers::{
    public_assets::PublicAssets, streaming_handler, StreamingAppState, StreamingTransportConfig,
};
pub use inspector::{
    EventTier as InspectorEventTier, GraphSnapshot as InspectorGraphSnapshot, InspectorState,
    MetricsSnapshot as InspectorMetricsSnapshot, RenderEvent as InspectorRenderEvent,
};
pub use lifecycle::{RequestContext, ResponseBody, ResponsePayload};
pub use render::{
    InjectionChunk, RenderError as TierBRenderError, TierBDataFetcher, TierBOpcodeRegistry,
    TierBRenderRegistry,
};
pub use renderer_runtime::{
    RendererRuntime, RENDER_MANIFEST_FILENAME, RUNTIME_MODULE_SOURCES_FILENAME,
};
pub use routing::{AuthPolicy, CompiledRouter, HttpMethod, MatchedRoute, RouteMatch, RouteTarget};
pub use server::{AlbedoServer, AlbedoServerBuilder};
pub use webtransport::{
    WebTransportRuntime, WebTransportSessionHandle, WebTransportSessionRegistry,
};
