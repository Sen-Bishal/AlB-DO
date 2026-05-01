//! Wire types for the dev-inspector event stream.
//!
//! These are the JSON shapes the SSE stream and graph payload speak. They are
//! intentionally decoupled from the runtime types in the parent compiler crate
//! so the wire contract can move independently.

use dom_render_compiler::manifest::schema::Tier;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum EventTier {
    A,
    B,
    C,
}

impl From<Tier> for EventTier {
    fn from(value: Tier) -> Self {
        match value {
            Tier::A => Self::A,
            Tier::B => Self::B,
            Tier::C => Self::C,
        }
    }
}

impl EventTier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }
}

/// One render event published to subscribers via the broadcast channel.
///
/// Cheap to clone — `name` is the only allocating field, and the whole struct
/// is a few hundred bytes at most.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderEvent {
    pub component_id: u64,
    pub component_name: String,
    pub tier: EventTier,
    pub duration_us: u64,
    pub timestamp_ms: u64,
    pub cascade_children: Vec<u64>,
    /// Optional free-form note attached by the publisher. The frontend renders
    /// this in the event drawer as a quiet aside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl RenderEvent {
    pub fn cascade_count(&self) -> usize {
        self.cascade_children.len()
    }
}
