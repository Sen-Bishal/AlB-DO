//! S4 · the render half of the delta beam — FORGE's [`RowProjector`], backed by
//! the same pooled QuickJS engines that serve Tier-B requests.
//!
//! A `SlotDelta`'s payload has to be *the* markup SSR produces for that row.
//! Not equivalent markup — the same bytes, from the same template, or the page
//! a client reconciles into drifts from the page a reload would give it. So
//! this projector does the only thing that guarantees that: it renders the
//! collection through the ordinary Tier-B render path with the post-write value
//! seeded into the component's shared-slot host, then reads the rows back out
//! of its own output by `data-albedo-key`
//! ([`extract_keyed_rows`](dom_render_compiler::transforms::shared_slot_lists::extract_keyed_rows)).
//! One template, one renderer, two consumers.
//!
//! # Why the value is passed in, never read
//!
//! [`PooledTierBRenderRegistry`](super::tier_b::PooledTierBRenderRegistry)
//! seeds its host from the *live* broadcast registry, which is right for a
//! request — the whole point is that topics are live. It is exactly wrong here.
//! This runs while FORGE is preparing a topic write, before the new value is
//! stored, and inside nothing yet but about to be handed to a closure that runs
//! under the topic's linearization lock. Reading the registry would render the
//! pre-write collection, and reading it from inside that closure would deadlock
//! on the topic's own mutex. The value therefore arrives as bytes and the
//! host is built from those bytes alone.
//!
//! # Ambiguity is refused, not resolved
//!
//! A row payload is keyed by topic, so every anchor bound to that topic on
//! every client receives the same bytes. If two components render the same
//! collection with different templates, no single payload can serve both — so a
//! topic claimed by more than one component projects `None`, and FORGE falls
//! back to snapshot fan-out for it. Slower, still correct, and it fails as a
//! whole rather than painting half the clients wrong.

use async_trait::async_trait;
use dom_render_compiler::forge::{RenderedRows, RowProjector};
use dom_render_compiler::transforms::shared_slot_lists::extract_keyed_rows;
use serde_json::Value;
use std::sync::Arc;

use super::tier_b::{TierBEntryPlan, TierBRenderPlan};

/// FORGE's row projector over the pooled Tier-B renderer.
pub struct PooledRowProjector {
    pool: Arc<crate::engine_pool::QuickJsEnginePool>,
    plan: TierBRenderPlan,
    /// P6 · the same per-action error-span seed the request path passes, so a
    /// projected render of a component containing a form produces the identical
    /// markup a request would. Rows from a render missing it would differ from
    /// SSR's by exactly those spans — the class of near-miss this whole design
    /// exists to rule out.
    form_error_spans: serde_json::Map<String, Value>,
}

impl PooledRowProjector {
    #[must_use]
    pub fn new(
        pool: Arc<crate::engine_pool::QuickJsEnginePool>,
        plan: TierBRenderPlan,
        form_error_spans: serde_json::Map<String, Value>,
    ) -> Self {
        Self {
            pool,
            plan,
            form_error_spans,
        }
    }

    /// The single component that reads `collection`, or `None` when none or
    /// more than one does.
    fn sole_reader(&self, collection: &str) -> Option<(&String, &TierBEntryPlan)> {
        let mut found = None;
        for entry in &self.plan {
            if entry
                .1
                .shared_topics
                .iter()
                .any(|topic| topic == collection)
            {
                if found.is_some() {
                    return None;
                }
                found = Some(entry);
            }
        }
        found
    }

    /// The `host` object for a projected render: the collection at the value
    /// being written, plus the project-global form error spans. Deliberately
    /// mirrors `tier_b::host_seed_for` minus its registry read.
    fn host_seed(&self, collection: &str, value: &[u8]) -> Option<String> {
        // Same lowering the registry-backed seed performs: topic bytes are the
        // materialised JSON the component's `useSharedSlot` sees as a value.
        let parsed: Value = serde_json::from_slice(value).ok()?;
        let mut shared = serde_json::Map::new();
        shared.insert(collection.to_string(), parsed);

        let mut host = serde_json::Map::new();
        host.insert("shared".to_string(), Value::Object(shared));
        if !self.form_error_spans.is_empty() {
            host.insert(
                "formErrorSpans".to_string(),
                Value::Object(self.form_error_spans.clone()),
            );
        }
        serde_json::to_string(&Value::Object(host)).ok()
    }
}

#[async_trait]
impl RowProjector for PooledRowProjector {
    async fn project_rows(&self, collection: &str, value: &[u8]) -> Option<RenderedRows> {
        let (render_fn, plan) = self.sole_reader(collection)?;
        let render_fn = render_fn.clone();
        let plan = plan.clone();
        let host_json = self.host_seed(collection, value)?;

        let html = self
            .pool
            .with_engine(move |engine| -> Result<String, String> {
                use dom_render_compiler::runtime::engine::RuntimeEngine;
                for (specifier, code) in &plan.modules {
                    engine
                        .load_module(specifier, code)
                        .map_err(|err| err.to_string())?;
                }
                engine
                    .render_component_with_host(&plan.entry, "{}", &host_json)
                    .map(|output| output.html)
                    .map_err(|err| err.to_string())
            })
            .await;

        // A projection failure is never fatal: the write is already durable and
        // the snapshot still ships. Log it, though — a topic that silently
        // stopped producing deltas would otherwise look like a slow page rather
        // than a broken one.
        let html = match html {
            Ok(Ok(html)) => html,
            Ok(Err(message)) => {
                tracing::warn!(
                    target: "albedo.forge",
                    collection,
                    render_fn,
                    error = %message,
                    "row projection render failed; falling back to snapshot fan-out"
                );
                return None;
            }
            Err(err) => {
                tracing::warn!(
                    target: "albedo.forge",
                    collection,
                    render_fn,
                    error = %err,
                    "row projection could not check out an engine; falling back to snapshot fan-out"
                );
                return None;
            }
        };

        let rows = extract_keyed_rows(&html, collection);
        if rows.is_none() {
            // The component rendered, but its markup has no keyed anchor for
            // this topic — a keyless list, or a `.map()` the B2 pass did not
            // mark. That is a *tier* answer, not an error: this collection is
            // on the coarse path and stays there.
            tracing::debug!(
                target: "albedo.forge",
                collection,
                render_fn,
                "no keyed list anchor in the projected render; snapshot fan-out"
            );
        }
        rows
    }
}
