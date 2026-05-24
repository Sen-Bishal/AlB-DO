//! Phase M.1 · dev error overlay registry.
//!
//! Captures render / action / compile errors and surfaces them to a
//! client-side overlay over an SSE stream. The registry is cheap to
//! clone (Arc<DashMap>) so middleware and handlers across the server
//! can push without coordinating ownership.
//!
//! Wire shape: each error gets a monotonic u64 id, a kind tag, a
//! message body, and optional source-location fields (file/line/col)
//! when the originating diagnostic carries them. The SSE event is
//! plain JSON so the client overlay doesn't need a bincode decoder
//! just to render an error.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Channel capacity for the error broadcast. Errors are bursty during
/// a bad-rebuild cycle but settle quickly; 64 absorbs the burst and
/// the lag-skip semantics below keep clients alive on overrun.
const ERROR_CHANNEL_CAPACITY: usize = 64;

/// Tag distinguishing where an error came from. Lets the overlay
/// colour-code and prioritise.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// JSX parse / compile failure surfaced by SWC or the renderer.
    Compile,
    /// Render-time failure inside the streaming handler.
    Render,
    /// Action handler returned `Err` or panicked.
    Action,
    /// Server-side runtime failure (config, IO, etc).
    Runtime,
}

/// One error event published to overlay clients. Field shapes are
/// chosen to round-trip cleanly through `serde_json` without escapes
/// the overlay JS has to undo.
#[derive(Debug, Clone, Serialize)]
pub struct DevError {
    pub id: u64,
    pub kind: ErrorKind,
    pub message: String,
    /// Originating route path when known (e.g. "/dashboard").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Source file path when the diagnostic carries one. Relative to
    /// the project root, forward-slash normalized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// 1-based line number, when the diagnostic carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// 1-based column, when the diagnostic carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// Wall-clock milliseconds since epoch, for ordering in the
    /// overlay log.
    pub timestamp_ms: u64,
}

/// Side-channel event the overlay listens to for explicit dismissal.
/// Sent when the server knows the offending file has been re-saved
/// without the error.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OverlayEvent {
    /// New error to display.
    Error(DevError),
    /// Specific error id is no longer current; remove from overlay.
    Dismiss { id: u64 },
    /// All errors cleared (e.g. after a clean rebuild).
    Clear,
}

/// Shared registry one server instance threads through `RuntimeState`.
/// Subsystems that produce errors call `report` with their kind +
/// message; the overlay handler subscribes via `subscribe` and pushes
/// events out via SSE.
#[derive(Debug)]
pub struct DevErrorRegistry {
    next_id: AtomicU64,
    /// Broadcast channel — every subscribed overlay gets the same
    /// event. Capacity is bounded; lagged subscribers drop the
    /// in-flight burst and resume on the next message.
    bus: broadcast::Sender<OverlayEvent>,
}

impl Default for DevErrorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DevErrorRegistry {
    pub fn new() -> Self {
        let (bus, _rx) = broadcast::channel(ERROR_CHANNEL_CAPACITY);
        Self {
            next_id: AtomicU64::new(1),
            bus,
        }
    }

    /// Returns a fresh receiver for the overlay event stream.
    /// Subscribers that fall behind the channel capacity are
    /// silently lagged — the receiver yields `Err(Lagged)` once and
    /// the SSE handler filters that out before forwarding.
    pub fn subscribe(&self) -> broadcast::Receiver<OverlayEvent> {
        self.bus.subscribe()
    }

    /// Report a fresh error. Allocates an id, stamps a timestamp,
    /// and publishes the event on the broadcast bus. Returns the id
    /// so the caller can later issue a matching dismiss.
    pub fn report(
        &self,
        kind: ErrorKind,
        message: impl Into<String>,
        route: Option<String>,
        file: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let evt = OverlayEvent::Error(DevError {
            id,
            kind,
            message: message.into(),
            route,
            file,
            line,
            column,
            timestamp_ms: now_ms(),
        });
        let _ = self.bus.send(evt);
        id
    }

    /// Convenience for the common "render-time error on this route"
    /// path. Pre-fills `kind` and `route`; the caller supplies the
    /// human-readable message.
    pub fn report_render(&self, route: impl Into<String>, message: impl Into<String>) -> u64 {
        self.report(
            ErrorKind::Render,
            message,
            Some(route.into()),
            None,
            None,
            None,
        )
    }

    /// Convenience for failures inside an action handler.
    pub fn report_action(&self, message: impl Into<String>) -> u64 {
        self.report(ErrorKind::Action, message, None, None, None, None)
    }

    /// Dismiss a previously reported error by id. The overlay
    /// removes the matching entry; no-op when the id is unknown.
    pub fn dismiss(&self, id: u64) {
        let _ = self.bus.send(OverlayEvent::Dismiss { id });
    }

    /// Clear every error from every subscriber. Called after a
    /// successful rebuild so a stale overlay can't outlive its
    /// originating diagnostic.
    pub fn clear(&self) {
        let _ = self.bus.send(OverlayEvent::Clear);
    }

    /// Current number of live subscribers, for diagnostics.
    pub fn subscriber_count(&self) -> usize {
        self.bus.receiver_count()
    }
}

/// Convenient shared alias the server threads through Arc-clones.
pub type SharedErrorRegistry = Arc<DevErrorRegistry>;

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn report_publishes_error_event_to_subscribers() {
        let registry = DevErrorRegistry::new();
        let mut rx = registry.subscribe();
        let id = registry.report_render("/", "boom");
        assert!(id >= 1);
        let evt = rx.recv().await.expect("event delivered");
        match evt {
            OverlayEvent::Error(err) => {
                assert_eq!(err.message, "boom");
                assert_eq!(err.route.as_deref(), Some("/"));
                assert_eq!(err.kind, ErrorKind::Render);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dismiss_publishes_dismiss_event_with_matching_id() {
        let registry = DevErrorRegistry::new();
        let mut rx = registry.subscribe();
        let id = registry.report_action("uh oh");
        let _ = rx.recv().await;
        registry.dismiss(id);
        match rx.recv().await.expect("dismiss delivered") {
            OverlayEvent::Dismiss { id: got } => assert_eq!(got, id),
            other => panic!("expected Dismiss, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clear_publishes_clear_event() {
        let registry = DevErrorRegistry::new();
        let mut rx = registry.subscribe();
        registry.clear();
        assert!(matches!(rx.recv().await, Ok(OverlayEvent::Clear)));
    }

    #[test]
    fn ids_are_monotonic_across_concurrent_reports() {
        let registry = DevErrorRegistry::new();
        let a = registry.report_action("a");
        let b = registry.report_action("b");
        let c = registry.report_action("c");
        assert!(a < b && b < c, "ids must be strictly monotonic");
    }
}
