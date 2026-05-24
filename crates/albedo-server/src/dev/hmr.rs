//! Phase M.2 · slot-preserving HMR registry.
//!
//! Broadcasts route-update events from the file watcher to overlay
//! clients. Each event carries the new HTML for the affected route(s)
//! so the client can swap the DOM subtree in place — slot state lives
//! on the server (Phase H `SlotStore` keyed by `SessionId`), so the
//! cookie's albedo-session survives and the next render reads the
//! same per-session values.

use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Broadcast capacity. Large enough to absorb a save-burst (multiple
/// files changed in quick succession) without lagging clients on the
/// happy path.
const HMR_CHANNEL_CAPACITY: usize = 32;

/// One HMR replacement event. The client looks up the element by
/// `route` and swaps its inner HTML; if the element is the document
/// body (root route) the swap targets `document.body.innerHTML`.
#[derive(Debug, Clone, Serialize)]
pub struct HmrPayload {
    /// Route path the patch applies to (e.g. "/dashboard"). Empty
    /// string when the patch is global / not route-specific.
    pub route: String,
    /// New HTML to install. The renderer's `data-albedo-id` stamps
    /// keep subsequent action POSTs addressable after the swap.
    pub html: String,
    /// Monotonic revision number from the file watcher — the
    /// overlay uses it to dedupe out-of-order delivery.
    pub revision: u64,
}

/// Event surface the client listens to. `Apply` is the load-bearing
/// case; `Reload` is the escape hatch for cases the in-place swap
/// can't handle (manifest schema change, asset graph rewrite, etc.).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HmrEvent {
    /// Replace the route's DOM subtree with the supplied HTML.
    Apply(HmrPayload),
    /// Hard reload — preserves nothing client-side, used as the last
    /// resort when in-place swap isn't safe.
    Reload { revision: u64 },
}

/// Shared registry the file-watcher and the SSE handler both hold.
#[derive(Debug)]
pub struct HmrRegistry {
    bus: broadcast::Sender<HmrEvent>,
}

impl Default for HmrRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HmrRegistry {
    pub fn new() -> Self {
        let (bus, _rx) = broadcast::channel(HMR_CHANNEL_CAPACITY);
        Self { bus }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HmrEvent> {
        self.bus.subscribe()
    }

    /// Publish an in-place HTML swap for `route`. The watcher calls
    /// this after re-rendering the route on file change.
    pub fn apply(&self, route: impl Into<String>, html: impl Into<String>, revision: u64) {
        let _ = self.bus.send(HmrEvent::Apply(HmrPayload {
            route: route.into(),
            html: html.into(),
            revision,
        }));
    }

    /// Publish a hard reload — the client drops everything and
    /// reissues a GET. Used when the substrate changes underneath
    /// the running DOM (manifest version bump, new asset graph).
    pub fn reload(&self, revision: u64) {
        let _ = self.bus.send(HmrEvent::Reload { revision });
    }

    pub fn subscriber_count(&self) -> usize {
        self.bus.receiver_count()
    }
}

pub type SharedHmrRegistry = Arc<HmrRegistry>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apply_publishes_payload_to_subscribers() {
        let registry = HmrRegistry::new();
        let mut rx = registry.subscribe();
        registry.apply("/", "<h1>fresh</h1>", 7);
        match rx.recv().await.expect("event delivered") {
            HmrEvent::Apply(payload) => {
                assert_eq!(payload.route, "/");
                assert_eq!(payload.html, "<h1>fresh</h1>");
                assert_eq!(payload.revision, 7);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reload_publishes_reload_event_carrying_revision() {
        let registry = HmrRegistry::new();
        let mut rx = registry.subscribe();
        registry.reload(42);
        match rx.recv().await.expect("event delivered") {
            HmrEvent::Reload { revision } => assert_eq!(revision, 42),
            other => panic!("expected Reload, got {other:?}"),
        }
    }
}
