//! Phase N · WARHEAD — file-based routing primitives.
//!
//! Walks `src/routes/` (or any user-configured root) and translates the
//! on-disk file layout into the same route-spec shape `albedo-server`
//! consumes. Dynamic params (`[slug]`) and catch-alls (`[...rest]`) are
//! preserved verbatim — the existing `CompiledRouter` normaliser handles
//! them. Layouts (`layout.tsx`) compose root-down per directory depth.

pub mod file_based;

pub use file_based::{
    discover_routes, file_path_to_url, DiscoveredLayout, DiscoveredRoute, RouteDiscovery,
    RouteDiscoveryError, ROUTES_DIRNAME,
};
