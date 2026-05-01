//! Demo render-event publisher used when no real pipeline is wired into the
//! inspector. Emits a small loop of synthetic events so the SSE stream and
//! metrics panels render with live-looking data — useful for shaping the UI
//! before the pipeline plumbing lands, and as a smoke test in dev builds.
//!
//! The task is started by `AlbedoServer::run` only when no other source is
//! actively publishing — currently always, since pipeline integration is
//! deferred to a follow-up.

use super::events::{EventTier, RenderEvent};
use super::graph::{ComponentNode, GraphSnapshot};
use super::state::{now_ms, InspectorState};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const TICK_INTERVAL: Duration = Duration::from_millis(1_400);

pub fn spawn(state: Arc<InspectorState>, mut shutdown: watch::Receiver<bool>) {
    if state.graph_snapshot().nodes.is_empty() {
        state.set_graph(GraphSnapshot::demo());
    }

    tokio::spawn(async move {
        let nodes = state.graph_snapshot().nodes;
        if nodes.is_empty() {
            return;
        }
        let mut cursor: usize = 0;
        let mut interval = tokio::time::interval(TICK_INTERVAL);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let event = synthesise_event(&nodes, cursor);
                    state.publish_event(event);
                    cursor = cursor.wrapping_add(1);
                }
                Ok(()) = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });
}

fn synthesise_event(nodes: &[ComponentNode], cursor: usize) -> RenderEvent {
    let primary = &nodes[cursor % nodes.len()];
    let cascade_seed = cursor.wrapping_mul(2_654_435_761);
    let cascade_size = match primary.tier {
        EventTier::A => cascade_seed % 2,
        EventTier::B => 1 + (cascade_seed % 3),
        EventTier::C => 2 + (cascade_seed % 4),
    };
    let cascade_children: Vec<u64> = (0..cascade_size)
        .map(|i| {
            let idx = (cursor + i + 1) % nodes.len();
            nodes[idx].id
        })
        .collect();

    let base_us = match primary.tier {
        EventTier::A => 60,
        EventTier::B => 320,
        EventTier::C => 1_400,
    };
    let jitter = (cascade_seed % 240) as u64;

    RenderEvent {
        component_id: primary.id,
        component_name: primary.label.clone(),
        tier: primary.tier,
        duration_us: base_us + jitter,
        timestamp_ms: now_ms(),
        cascade_children,
        note: None,
    }
}
