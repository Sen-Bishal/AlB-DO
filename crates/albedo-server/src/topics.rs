//! PRISM · read-through materialisation of a partition's value.
//!
//! [`ResolvedPartition`] says *which* topic a request needs; this says how its
//! value gets there. The rule is PRISM invariant 3 — **the substrate is the
//! truth and the topic value is a cache** — so warming is idempotent, a miss is
//! correct and merely slower, and nothing here can be the reason a page fails.
//!
//! Both callers that need a partition's bytes go through [`TopicWarmer`]:
//!
//! - **render**, before seeding `host.shared` for a Tier-B component;
//! - **subscribe**, before `auto_subscribe` snapshots each topic under its lock.
//!
//! Neither can mint a topic on its own, which is the point. If the render
//! materialised through one path and the subscribe through another, the two
//! would eventually disagree about a partition's *shape* — the page rendering
//! rows the lane then replaces with a differently-ordered set on first frame.

use async_trait::async_trait;
use dom_render_compiler::runtime::ResolvedPartition;

/// Materialise partition topics into the broadcast registry, on demand.
///
/// Implementations must be **fail-soft**: a partition that cannot be
/// materialised is left unregistered and logged, never propagated as an error.
/// The route then renders as a static page with an empty slot (PRISM § 10),
/// which is recoverable; a 500 on a malformed URL segment is not.
#[async_trait]
pub trait TopicWarmer: Send + Sync {
    /// Ensure every partition in `partitions` is registered and carries its
    /// current value. Safe to call repeatedly for the same partition.
    async fn warm(&self, partitions: &[ResolvedPartition]);
}

/// The warmer for a server with no FORGE substrate wired.
///
/// Not a silent no-op by accident: without a substrate a partition has nothing
/// to be derived *from*, so registering an empty topic would publish "this room
/// has no rows" as though it were an answer. Leaving it unregistered makes the
/// slot render its own null and keeps the difference between *empty* and
/// *unknown* visible.
pub struct NoTopicWarmer;

#[async_trait]
impl TopicWarmer for NoTopicWarmer {
    async fn warm(&self, _partitions: &[ResolvedPartition]) {}
}
