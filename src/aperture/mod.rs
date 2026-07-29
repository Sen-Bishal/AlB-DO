//! # APERTURE — the outside world as a tier
//!
//! *A remote resource is a topic whose derivation is an HTTP GET; a remote
//! write is a journal entry whose position is its idempotency key.*
//!
//! Design: `development-plan/APERTURE.md`. This module is **phase A0** of that
//! plan: the outbound HTTP client, the shared response cache, request
//! coalescing, conditional requests, and the egress policy.
//!
//! ## What A0 deliberately is not
//!
//! There is **no JS surface here**. `bridge.rs` is untouched, no global named
//! `fetch` is installed, and nothing in this module is reachable from a handler
//! body. Phase A1 wires the read path to `useSharedSlot`; phase A2 adds the
//! journal and the suspend protocol that let an action body call outward.
//!
//! Sequencing it this way is what makes the merge gates unambiguous: gates 3
//! (conditional requests) and 6 (single-flight) run against this module with no
//! engine involved, so a failure is in the cache or the coalescer and nowhere
//! else.
//!
//! ## Shape
//!
//! - [`egress`] — the policy, and the resolver-level enforcement that makes it un-bypassable by DNS
//!   rebinding.
//! - [`cache`] — the shared, byte-budgeted, LRU-evicted response store. The thing that gives an
//!   `ETag` somewhere to live.
//! - [`client`] — cache lookup, single-flight coalescing, conditional revalidation, and the
//!   counters the gates assert on.
//! - [`transport`] — the `reqwest` implementation of the network seam.
//! - [`workflow`] — A2's driver: the pass loop that resolves what a suspended body asked for and
//!   runs it again, above the sync/async boundary so no engine is held across a round trip.
//!
//! ## The two invariants this phase carries
//!
//! **2.2 · the upstream is the truth; the slot value is a cache.** Every stored
//! response must be re-derivable by re-issuing the request, which is what makes
//! [`cache::ResponseCache::enforce_byte_budget`] safe — the same argument
//! `PRISM.md` invariant 2.3 makes for topic values. With one honest asymmetry:
//! re-derivation here is metered and can fail, so a stale body is a legitimate
//! degraded result where FORGE would have none.
//!
//! **2.3 · a cached response is shared only among callers presenting the same
//! authority.** [`cache::CacheScope`] is part of the cache key, so a per-user
//! response cannot be served to another principal. This is the one failure in
//! the design that would be a CVE rather than a bug, and it is closed by
//! construction rather than by review.

pub mod bindings;
pub mod cache;
pub mod client;
pub mod declare;
pub mod egress;
pub mod journal;
pub mod reader;
pub mod refresh;
pub mod transport;
pub mod typegen;
pub mod workflow;

pub use bindings::{validate_source_bindings, SourceBinding, SourceBindingProblem};
pub use declare::{
    source_topic_name, AuthDecl, AuthScope, PathSegment, ResolvedSource, RouteDecl, SourceDecl,
    SourceRegistry, SourceRoute, SourceSchemaError, DEFAULT_REFRESH,
};
pub use journal::{
    Journal, JournalError, Step, StepKind, StepOutcome, DEFAULT_PASS_CAP, DEFAULT_STEP_CAP,
};
pub use reader::{SourceRead, SourceReadError, SourceReader};
pub use refresh::{
    refresh_topic, RefreshLoop, RefreshOutcome, RefreshReport, DEFAULT_MAX_IN_FLIGHT, DEFAULT_TICK,
};
pub use typegen::emit_sources_dts;
pub use workflow::{
    drive_workflow, resolve_pending, WorkflowError, WorkflowLimits, DEFAULT_WORKFLOW_DEADLINE,
    IDEMPOTENCY_KEY_HEADER,
};

pub use cache::{
    CacheHit, CacheScope, CachedResponse, Freshness, ResourceKey, ResponseCache, Validators,
    DEFAULT_RESPONSE_BUDGET,
};
pub use client::{
    ApertureClient, ApertureError, ApertureRequest, CountingTransport, Disposition, FetchOutcome,
    Metrics, MetricsSnapshot, Transport, WireRequest, WireResponse, DEFAULT_REQUEST_TIMEOUT,
};
pub use egress::{AddressClass, EgressDenial, EgressMode, EgressPolicy};
pub use transport::{ApertureResolver, ReqwestTransport};
