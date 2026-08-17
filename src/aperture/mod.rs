//! # APERTURE — the outside world as a tier
//!
//! *A remote resource is a topic whose derivation is an HTTP GET; a remote
//! write is a journal entry whose position is its idempotency key.*
//!
//! Design: `development-plan/APERTURE.md`.
//!
//! ## Phase status
//!
//! - **A0 · the client** ✅ — outbound HTTP, the shared response cache, request
//!   coalescing, conditional requests, and the egress policy.
//! - **A1 · the read path** ✅ — declared `sources`, `useSharedSlot(ops.status())`,
//!   the refresh loop, and `.d.ts` codegen ([`typegen`]).
//! - **A2 · the protocol and the server seam** ✅ — the journal, suspend/replay,
//!   `await` lowering, and [`drive_workflow`] called from the action path. An
//!   authored `await fetch(…)` inside a handler body runs to completion; see
//!   `tests/aperture_workflow.rs` and the `hook_compile/fetching_handler`
//!   fixture, which is deliberately verbatim copy-paste-shaped code.
//! - **A2 · R1.3 hoisting** ❌ — there is no batching. Three independent GETs
//!   cost 4 passes and 3 round trips.
//! - **A3 · durable journal** ❌ — the journal is not persisted to FORGE, so
//!   there is no crash recovery and no retry policy.
//!
//! ⚠️ **This header claimed "A0 only, no JS surface" long after A1 and A2 had
//! landed**, and that stale sentence was copied into the public README as a
//! statement that outbound fetch did not work. A phase list in a doc comment is
//! a claim about the tree; when it goes stale it does not merely age, it
//! actively misinforms. Update it in the same commit as the phase.
//!
//! Sequencing the phases this way is what made the merge gates unambiguous:
//! gates 3 (conditional requests) and 6 (single-flight) run against A0 with no
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
