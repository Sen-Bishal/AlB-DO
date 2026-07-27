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
    ///
    /// PRISM · a partitioned read counts. `shared_topics` alone would miss it —
    /// `messages.where({ … })` contributes no compile-time topic — and the
    /// consequence is not a slower path but a dead one: no sole reader means no
    /// projected rows, which means a partitioned write ships its snapshot with
    /// `ListUpdate::None` and every keyed list anchor on the page sits still.
    fn sole_reader(&self, collection: &str) -> Option<(&String, &TierBEntryPlan)> {
        let mut found = None;
        for entry in &self.plan {
            let reads = entry.1.shared_topics.iter().any(|topic| topic == collection)
                || entry
                    .1
                    .shared_partitions
                    .iter()
                    .any(|spec| spec.collection == collection);
            if reads {
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
    ///
    /// PRISM · for a partitioned reader the seed also has to satisfy
    /// `__albedo_topic(binding)`, or the component reads null and renders no
    /// rows at all. It is seeded with the **collection name** standing in for the
    /// topic, and that substitution is exact rather than convenient: the topic
    /// string reaches the output in one place only — the anchor attribute on the
    /// list's container — and rows are the container's keyed *children*.
    /// [`extract_keyed_rows`] is handed the same stand-in, so it finds the same
    /// anchor, and every row it lifts out is byte-identical to the one a real
    /// request rendered.
    ///
    /// Doing it this way keeps the projector keyed by collection, which is what
    /// it should be: the row **template** belongs to the collection, while the
    /// subscriber set belongs to the partition. The write path already routes the
    /// fan-out by channel.
    /// The props a projected render runs with.
    ///
    /// `{}` for an unpartitioned collection, exactly as before. For a
    /// partitioned one it carries `params`, rebuilt from the partition key and
    /// the param name the binding declared — because a component that reads a
    /// partition read a route param to name it, and very likely renders that
    /// param too (`<h1>Room {params.id}</h1>`, a hidden field carrying the room
    /// into the write). Rendering it with no props does not merely lose a
    /// heading: `params.id` throws, the projection fails, and the write silently
    /// degrades to a snapshot no keyed anchor repaints from.
    fn props_for(&self, collection: &str, partition: Option<&str>) -> String {
        let Some(key) = partition else {
            return "{}".to_string();
        };
        let params = self
            .sole_reader(collection)
            .map(|(_, plan)| {
                plan.shared_partitions
                    .iter()
                    .filter(|spec| spec.collection == collection)
                    .map(|spec| (spec.param.clone(), Value::String(key.to_string())))
                    .collect::<serde_json::Map<String, Value>>()
            })
            .unwrap_or_default();
        serde_json::to_string(&serde_json::json!({ "params": params }))
            .unwrap_or_else(|_| "{}".to_string())
    }

    fn host_seed(&self, collection: &str, value: &[u8]) -> Option<String> {
        // Same lowering the registry-backed seed performs: topic bytes are the
        // materialised JSON the component's `useSharedSlot` sees as a value.
        let parsed: Value = serde_json::from_slice(value).ok()?;
        let mut shared = serde_json::Map::new();
        shared.insert(collection.to_string(), parsed);

        let mut host = serde_json::Map::new();
        host.insert("shared".to_string(), Value::Object(shared));
        if let Some((_, plan)) = self.sole_reader(collection) {
            let topics = plan
                .shared_partitions
                .iter()
                .filter(|spec| spec.collection == collection)
                .map(|spec| {
                    (
                        spec.binding.clone(),
                        Value::String(collection.to_string()),
                    )
                })
                .collect::<serde_json::Map<String, Value>>();
            if !topics.is_empty() {
                host.insert("topics".to_string(), Value::Object(topics));
            }
        }
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
    fn projection_class(
        &self,
        collection: &str,
    ) -> dom_render_compiler::transforms::shared_slot_lists::RowProjection {
        use dom_render_compiler::transforms::shared_slot_lists::RowProjection;
        // The class only applies when a single component reads the collection —
        // the same condition `project_rows` needs a `sole_reader` for. A topic
        // read by several components is ambiguous to the projector anyway, so it
        // stays on the always-correct whole-view path.
        match self.sole_reader(collection) {
            Some((_, plan)) => plan
                .shared_topic_classes
                .get(collection)
                .copied()
                .unwrap_or(RowProjection::WholeView),
            None => RowProjection::WholeView,
        }
    }

    async fn project_rows(
        &self,
        collection: &str,
        partition: Option<&str>,
        value: &[u8],
    ) -> Option<RenderedRows> {
        let (render_fn, plan) = self.sole_reader(collection)?;
        let render_fn = render_fn.clone();
        let plan = plan.clone();
        let host_json = self.host_seed(collection, value)?;
        let props_json = self.props_for(collection, partition);

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
                    .render_component_with_host(&plan.entry, &props_json, &host_json)
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
