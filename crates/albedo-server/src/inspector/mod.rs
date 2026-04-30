//! Dev inspector served at `/__albedo`.
//!
//! Surfaces a real-time view of what the renderer is doing: a force-directed
//! component graph, a metrics sidebar, and an SSE-fed event log. The module
//! is self-contained — its only entry points from the rest of the server are
//! [`InspectorState`], the dispatch helpers in [`api`], and the optional
//! [`heartbeat`] task. No production routes change behaviour because of this
//! module unless the inspector is explicitly enabled at builder time.

pub mod api;
pub mod events;
pub mod graph;
pub mod heartbeat;
pub mod metrics;
pub mod publisher;
pub mod state;

pub use api::{dispatch, matches_inspector_path};
pub use events::{EventTier, RenderEvent};
pub use graph::{ComponentEdge, ComponentNode, GraphSnapshot, GraphSource};
pub use metrics::{HotComponent, MetricsAggregator, MetricsSnapshot, SlowComponent, TierBreakdown};
pub use publisher::InspectorPublisher;
pub use state::InspectorState;
