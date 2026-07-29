//! APERTURE · A0 — the outbound HTTP client.
//!
//! Implements `development-plan/APERTURE.md` § 4.2 (fan-out inversion), § 4.3
//! (conditional requests), and the single-flight and timeout rows of § 8.
//!
//! ## No JS surface
//!
//! Phase A0 deliberately stops below the engine. Nothing here is reachable from
//! a handler body, `bridge.rs` is untouched, and no global named `fetch` is
//! installed. That is not an accident of sequencing — it is what makes gates 3
//! and 6 unambiguous. They run against this module with no QuickJS involved, so
//! a failure is in the cache or the coalescer and nowhere else.
//!
//! ## The [`Transport`] seam
//!
//! The gates assert **counts** — *"200 cold callers produce exactly 1 upstream
//! request"*, *"100 unchanged refreshes produce 0 value changes"*. Asserting
//! that against a live network would make the suite slow, flaky and dependent
//! on someone else's uptime, which is how count assertions quietly decay into
//! timing assertions nobody trusts. `PRISM.md` § 11 learned this the hard way:
//! its index claim is proved by `EXPLAIN QUERY PLAN`, not by a stopwatch.
//!
//! So the network is a trait. [`ReqwestTransport`] is the real one;
//! [`CountingTransport`] is the test double that records every request it was
//! asked to make. The gates are then exact rather than probabilistic.

use crate::aperture::cache::{
    CacheScope, CachedResponse, Freshness, ResourceKey, ResponseCache, Validators,
};
use crate::aperture::egress::{EgressDenial, EgressPolicy};
use async_trait::async_trait;
use dashmap::mapref::entry::Entry as MapEntry;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use url::Url;

/// Default per-request timeout (APERTURE § 8).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A failure on the outbound path.
///
/// `Clone` because a single-flight leader shares one outcome with every
/// follower; the alternative is re-issuing the request per waiter, which is
/// precisely what coalescing exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApertureError {
    /// The URL did not parse.
    InvalidUrl(String),
    /// Egress policy refused the request.
    Egress(EgressDenial),
    /// The upstream could not be reached, or the transport failed.
    Transport(String),
    /// The request exceeded its timeout.
    Timeout {
        /// The configured limit that was exceeded.
        after: Duration,
    },
    /// The upstream answered `304` but nothing was cached to revalidate — a
    /// conditional request was sent on validators that were evicted mid-flight.
    DanglingRevalidation,
}

impl std::fmt::Display for ApertureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApertureError::InvalidUrl(url) => write!(f, "aperture: could not parse URL `{url}`"),
            ApertureError::Egress(denial) => write!(f, "aperture: {denial}"),
            ApertureError::Transport(msg) => write!(f, "aperture: transport failure: {msg}"),
            ApertureError::Timeout { after } => {
                write!(f, "aperture: request timed out after {after:?}")
            }
            ApertureError::DanglingRevalidation => write!(
                f,
                "aperture: upstream returned 304 but the cached entry was evicted mid-flight"
            ),
        }
    }
}

impl std::error::Error for ApertureError {}

/// A request as it goes on the wire, after conditional headers are attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRequest {
    /// HTTP method.
    pub method: String,
    /// Absolute URL.
    pub url: String,
    /// Headers, including any `If-None-Match` / `If-Modified-Since`.
    pub headers: Vec<(String, String)>,
    /// Request body, if any.
    pub body: Option<Vec<u8>>,
}

impl WireRequest {
    /// Whether this request carries a conditional validator header.
    #[must_use]
    pub fn is_conditional(&self) -> bool {
        self.headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("if-none-match")
                || name.eq_ignore_ascii_case("if-modified-since")
        })
    }
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireResponse {
    /// HTTP status.
    pub status: u16,
    /// Body bytes. Empty for a `304`.
    pub body: Vec<u8>,
    /// Every response header, **names lowercased**, in the order the upstream
    /// sent them.
    ///
    /// Carried whole rather than as the three fields below it because a
    /// workflow body reaches `res.headers.get(name)` (§ 5.5 — copy-pasted vendor
    /// code has to run), and rate limits, pagination `Link`s and `Location` all
    /// live outside the validator set. A response that answered a header and
    /// then reports `null` for it is a silent wrong answer, which is worse than
    /// having no header surface at all.
    ///
    /// The read path does **not** use this — a source topic's value is its body
    /// — so [`crate::aperture::CachedResponse`] deliberately does not store it
    /// and the cache's byte budget is unaffected.
    pub headers: Vec<(String, String)>,
    /// `ETag`, if the upstream sent one.
    pub etag: Option<String>,
    /// `Last-Modified`, if the upstream sent one.
    pub last_modified: Option<String>,
    /// `Content-Type`, if the upstream sent one.
    pub content_type: Option<String>,
}

/// The network, behind a trait so the gates need not touch it.
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    /// Perform one request. Implementations must not retry — retry policy is
    /// A3 (APERTURE § 14) and belongs above this seam.
    ///
    /// # Errors
    /// Any transport-level failure, as [`ApertureError::Transport`].
    async fn send(&self, request: &WireRequest) -> Result<WireResponse, ApertureError>;
}

/// How a fetch resolved. The vocabulary the gates count in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Served from cache inside its refresh window. No upstream contact.
    FreshHit,
    /// Conditional request answered `304`. The body never moved.
    NotModified,
    /// A full response was fetched and stored.
    Fetched,
    /// The upstream failed and a stale cached body was served instead
    /// (APERTURE § 10, permitted by invariant 2.2's asymmetry).
    StaleOnError,
}

/// A completed fetch.
#[derive(Debug, Clone)]
pub struct FetchOutcome {
    /// The response body and validators.
    pub response: CachedResponse,
    /// How it was obtained.
    pub disposition: Disposition,
}

/// What the caller asks for.
#[derive(Debug, Clone)]
pub struct ApertureRequest {
    /// HTTP method. A0 exercises `GET`; the write path is A2.
    pub method: String,
    /// Absolute URL.
    pub url: String,
    /// Who may share the answer (APERTURE § 7).
    pub scope: CacheScope,
    /// Refresh window. `Duration::ZERO` means "always revalidate".
    pub ttl: Duration,
    /// Extra headers to send.
    pub headers: Vec<(String, String)>,
    /// Request body, if any.
    pub body: Option<Vec<u8>>,
}

impl ApertureRequest {
    /// A cacheable `GET` under app-level authority.
    #[must_use]
    pub fn get(url: impl Into<String>, ttl: Duration) -> Self {
        Self {
            method: "GET".to_string(),
            url: url.into(),
            scope: CacheScope::App,
            ttl,
            headers: Vec::new(),
            body: None,
        }
    }

    /// Whether this request's result may be shared and stored.
    ///
    /// Only idempotent methods are cacheable. This is § 3's read/write split
    /// showing up at runtime: a `POST` is an effect, and an effect that
    /// answered once must never answer again from a cache.
    #[must_use]
    pub fn is_cacheable(&self) -> bool {
        matches!(self.method.to_ascii_uppercase().as_str(), "GET" | "HEAD")
    }
}

/// Counters the merge gates assert against.
///
/// Every field is a count, not a duration. `PRISM.md` § 11's lesson, applied
/// before rather than after: a claim proved by timing is a claim that will be
/// re-litigated on someone else's machine.
#[derive(Debug, Default)]
pub struct Metrics {
    upstream_requests: AtomicU64,
    conditional_requests: AtomicU64,
    not_modified: AtomicU64,
    value_changes: AtomicU64,
    fresh_hits: AtomicU64,
    coalesced: AtomicU64,
    stale_on_error: AtomicU64,
}

/// An immutable read of [`Metrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Requests actually put on the wire. **Gate 2 and gate 6 assert on this.**
    pub upstream_requests: u64,
    /// Of those, how many carried a validator header.
    pub conditional_requests: u64,
    /// How many were answered `304`. **Gate 3 asserts on this.**
    pub not_modified: u64,
    /// How many times a stored body actually changed. In A0 this is the
    /// standing proxy for "subscriber notifications", which do not exist until
    /// A1 wires the delta wire up. **Gate 3 asserts this is zero.**
    pub value_changes: u64,
    /// Served from cache with no upstream contact.
    pub fresh_hits: u64,
    /// Callers that joined an in-flight request instead of starting one.
    pub coalesced: u64,
    /// Times a stale body was served because the upstream failed.
    pub stale_on_error: u64,
}

impl Metrics {
    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            upstream_requests: self.upstream_requests.load(Ordering::Relaxed),
            conditional_requests: self.conditional_requests.load(Ordering::Relaxed),
            not_modified: self.not_modified.load(Ordering::Relaxed),
            value_changes: self.value_changes.load(Ordering::Relaxed),
            fresh_hits: self.fresh_hits.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            stale_on_error: self.stale_on_error.load(Ordering::Relaxed),
        }
    }
}

type SharedOutcome = Arc<Result<FetchOutcome, ApertureError>>;

/// The A0 client: cache, coalescer, egress policy and transport in one place.
#[derive(Debug)]
pub struct ApertureClient {
    transport: Arc<dyn Transport>,
    cache: Arc<ResponseCache>,
    policy: Arc<EgressPolicy>,
    inflight: DashMap<ResourceKey, watch::Receiver<Option<SharedOutcome>>>,
    metrics: Metrics,
    timeout: Duration,
}

impl ApertureClient {
    /// Build a client over an explicit transport.
    #[must_use]
    pub fn new(
        transport: Arc<dyn Transport>,
        cache: Arc<ResponseCache>,
        policy: Arc<EgressPolicy>,
    ) -> Self {
        Self {
            transport,
            cache,
            policy,
            inflight: DashMap::new(),
            metrics: Metrics::default(),
            timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Override the per-request timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Current counters.
    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    /// The shared response cache.
    #[must_use]
    pub fn cache(&self) -> &Arc<ResponseCache> {
        &self.cache
    }

    /// Fetch `request`, using and maintaining the shared cache.
    ///
    /// # Errors
    /// [`ApertureError::InvalidUrl`] or [`ApertureError::Egress`] before any
    /// network contact; [`ApertureError::Transport`] or
    /// [`ApertureError::Timeout`] from the transport, unless a stale cached
    /// body can be served instead.
    pub async fn fetch(&self, request: &ApertureRequest) -> Result<FetchOutcome, ApertureError> {
        let url =
            Url::parse(&request.url).map_err(|_| ApertureError::InvalidUrl(request.url.clone()))?;
        self.policy.check_url(&url).map_err(ApertureError::Egress)?;

        if !request.is_cacheable() {
            // An effect. No cache, no coalescing — two identical POSTs are two
            // distinct intentions, and merging them would be the exact bug
            // derived idempotency keys exist to prevent (APERTURE § 5.3).
            return self.send_uncached(request).await;
        }

        let key = ResourceKey::new(&request.method, &request.url, &request.scope);

        if let Some(hit) = self.cache.get(&key, request.ttl) {
            if hit.freshness == Freshness::Fresh {
                self.metrics.fresh_hits.fetch_add(1, Ordering::Relaxed);
                return Ok(FetchOutcome {
                    response: hit.response,
                    disposition: Disposition::FreshHit,
                });
            }
        }

        self.coalesced_fetch(&key, request).await
    }

    /// Single-flight: one leader does the work, every other caller waits on the
    /// same result.
    async fn coalesced_fetch(
        &self,
        key: &ResourceKey,
        request: &ApertureRequest,
    ) -> Result<FetchOutcome, ApertureError> {
        // Two attempts at most. A follower only retries if the leader vanished
        // without publishing (a panic), which must not become an infinite loop.
        for _ in 0..2 {
            let role = {
                // The shard guard MUST NOT be held across an await. Everything
                // in this block is synchronous and the guard is dropped at its
                // end; awaiting inside it would deadlock every other caller
                // hashing to the same shard.
                match self.inflight.entry(key.clone()) {
                    MapEntry::Occupied(occupied) => Role::Follower(occupied.get().clone()),
                    MapEntry::Vacant(vacant) => {
                        let (tx, rx) = watch::channel(None);
                        vacant.insert(rx);
                        Role::Leader(tx)
                    }
                }
            };

            match role {
                Role::Leader(tx) => {
                    let outcome = Arc::new(self.perform(key, request).await);
                    // Publish before removing, so a follower that cloned the
                    // receiver a moment ago still observes a value rather than
                    // a closed channel.
                    let _ = tx.send(Some(Arc::clone(&outcome)));
                    self.inflight.remove(key);
                    return (*outcome).clone();
                }
                Role::Follower(mut rx) => {
                    self.metrics.coalesced.fetch_add(1, Ordering::Relaxed);
                    if let Ok(value) = rx.wait_for(Option::is_some).await {
                        if let Some(outcome) = value.clone() {
                            return (*outcome).clone();
                        }
                    }
                    // Leader disappeared without publishing. Undo the coalesced
                    // count and take another lap; the next pass will most
                    // likely find the value already in cache.
                    self.metrics.coalesced.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }

        self.perform(key, request).await
    }

    /// The actual round trip, plus cache maintenance. Only ever run by a
    /// single-flight leader.
    async fn perform(
        &self,
        key: &ResourceKey,
        request: &ApertureRequest,
    ) -> Result<FetchOutcome, ApertureError> {
        let validators = self.cache.validators(key).unwrap_or_default();
        let wire = build_wire_request(request, &validators);

        let result = self.send_with_timeout(&wire).await;

        match result {
            Ok(response) if response.status == 304 => {
                self.metrics.not_modified.fetch_add(1, Ordering::Relaxed);
                if self.cache.mark_revalidated(key) {
                    // Deliberately no `value_changes` increment and no body
                    // copy: this is § 4.3's claim, and gate 3 asserts it.
                    let hit = self
                        .cache
                        .get(key, request.ttl)
                        .ok_or(ApertureError::DanglingRevalidation)?;
                    Ok(FetchOutcome {
                        response: hit.response,
                        disposition: Disposition::NotModified,
                    })
                } else {
                    Err(ApertureError::DanglingRevalidation)
                }
            }
            Ok(response) => {
                let stored = CachedResponse {
                    status: response.status,
                    body: Arc::from(response.body.as_slice()),
                    content_type: response.content_type,
                    validators: Validators {
                        etag: response.etag,
                        last_modified: response.last_modified,
                    },
                };
                // `Option::is_none_or` would read better but is stable only
                // since 1.82, and the workspace MSRV is 1.77.
                let changed = match self.cache.get(key, Duration::MAX) {
                    Some(prior) => *prior.response.body != *stored.body,
                    None => true,
                };
                if changed {
                    self.metrics.value_changes.fetch_add(1, Ordering::Relaxed);
                }
                self.cache.insert(key.clone(), stored.clone());
                self.cache.enforce_byte_budget();
                Ok(FetchOutcome {
                    response: stored,
                    disposition: Disposition::Fetched,
                })
            }
            Err(err) => {
                // APERTURE § 10 — a read whose upstream failed serves its last
                // good value. Legal here and nowhere else in the system,
                // because invariant 2.2's re-derivation is metered and fallible
                // where FORGE's is neither.
                if let Some(hit) = self.cache.get(key, Duration::MAX) {
                    self.metrics.stale_on_error.fetch_add(1, Ordering::Relaxed);
                    return Ok(FetchOutcome {
                        response: hit.response,
                        disposition: Disposition::StaleOnError,
                    });
                }
                Err(err)
            }
        }
    }

    /// Issue one request as a **workflow step**: egress-checked, timed out, and
    /// neither cached nor coalesced. Returns the raw response, headers included.
    ///
    /// ## Why a journal step never touches the cache
    ///
    /// [`Self::fetch`] decides cacheability from the method, which is right for
    /// a *declared read* — the author wrote a refresh window and a sharing scope
    /// and the client honours them. A bare `fetch()` inside an action body has
    /// neither. Serving it from the shared store would key a response on its URL
    /// while its authority lives in a header the key never saw, which is
    /// invariant 2.3's failure and § 11 R5's CVE-class risk: user A's bearer
    /// token fetches `/me`, user B reads it back.
    ///
    /// § 11 R5 allows a bare call single-flight without a cross-session cache.
    /// This goes one step further and coalesces nothing either, on the same
    /// ground the non-idempotent path already stands on: **a step in a workflow
    /// is an effect.** Two bodies asking for the same URL at the same moment are
    /// two distinct intentions holding two distinct idempotency keys, and
    /// merging them would be exactly the bug derived keys exist to prevent.
    /// Reads that *should* be shared have a declaration and go through
    /// [`Self::fetch`].
    ///
    /// # Errors
    /// [`ApertureError::InvalidUrl`] or [`ApertureError::Egress`] before any
    /// network contact; [`ApertureError::Transport`] or
    /// [`ApertureError::Timeout`] from the transport. There is no last-good
    /// value to fall back on and offering one would be a fabricated answer, so
    /// a failed step fails.
    pub async fn send_effect(
        &self,
        request: &ApertureRequest,
    ) -> Result<WireResponse, ApertureError> {
        let url =
            Url::parse(&request.url).map_err(|_| ApertureError::InvalidUrl(request.url.clone()))?;
        self.policy.check_url(&url).map_err(ApertureError::Egress)?;
        let wire = build_wire_request(request, &Validators::default());
        self.send_with_timeout(&wire).await
    }

    /// Non-idempotent path: straight to the wire, nothing stored.
    async fn send_uncached(
        &self,
        request: &ApertureRequest,
    ) -> Result<FetchOutcome, ApertureError> {
        let wire = build_wire_request(request, &Validators::default());
        let response = self.send_with_timeout(&wire).await?;
        Ok(FetchOutcome {
            response: CachedResponse {
                status: response.status,
                body: Arc::from(response.body.as_slice()),
                content_type: response.content_type,
                validators: Validators {
                    etag: response.etag,
                    last_modified: response.last_modified,
                },
            },
            disposition: Disposition::Fetched,
        })
    }

    async fn send_with_timeout(&self, wire: &WireRequest) -> Result<WireResponse, ApertureError> {
        self.metrics
            .upstream_requests
            .fetch_add(1, Ordering::Relaxed);
        if wire.is_conditional() {
            self.metrics
                .conditional_requests
                .fetch_add(1, Ordering::Relaxed);
        }
        match tokio::time::timeout(self.timeout, self.transport.send(wire)).await {
            Ok(result) => result,
            Err(_) => Err(ApertureError::Timeout {
                after: self.timeout,
            }),
        }
    }
}

enum Role {
    Leader(watch::Sender<Option<SharedOutcome>>),
    Follower(watch::Receiver<Option<SharedOutcome>>),
}

/// Attach conditional headers to a request when validators are available.
fn build_wire_request(request: &ApertureRequest, validators: &Validators) -> WireRequest {
    let mut headers = request.headers.clone();
    if request.is_cacheable() {
        if let Some(etag) = &validators.etag {
            headers.push(("if-none-match".to_string(), etag.clone()));
        } else if let Some(last_modified) = &validators.last_modified {
            // Only when there is no `ETag`. Sending both invites an upstream to
            // apply the weaker validator and answer 200 where it could have
            // answered 304.
            headers.push(("if-modified-since".to_string(), last_modified.clone()));
        }
    }
    WireRequest {
        method: request.method.to_ascii_uppercase(),
        url: request.url.clone(),
        headers,
        body: request.body.clone(),
    }
}

/// A [`Transport`] that records every request and replays scripted responses.
///
/// The gates' instrument. Not `#[cfg(test)]` because the benches in
/// `benches/aperture_gates.rs` are a separate crate and need it too.
#[derive(Debug)]
pub struct CountingTransport {
    responses: std::sync::Mutex<Vec<Result<WireResponse, ApertureError>>>,
    default: std::sync::Mutex<Option<WireResponse>>,
    seen: std::sync::Mutex<Vec<WireRequest>>,
    calls: AtomicU64,
    delay: Duration,
}

impl CountingTransport {
    /// A transport that answers every request with `response`.
    #[must_use]
    pub fn always(response: WireResponse) -> Self {
        Self {
            responses: std::sync::Mutex::new(Vec::new()),
            default: std::sync::Mutex::new(Some(response)),
            seen: std::sync::Mutex::new(Vec::new()),
            calls: AtomicU64::new(0),
            delay: Duration::ZERO,
        }
    }

    /// A transport that answers from `script` in order, then fails.
    #[must_use]
    pub fn scripted(script: Vec<Result<WireResponse, ApertureError>>) -> Self {
        let mut reversed = script;
        reversed.reverse();
        Self {
            responses: std::sync::Mutex::new(reversed),
            default: std::sync::Mutex::new(None),
            seen: std::sync::Mutex::new(Vec::new()),
            calls: AtomicU64::new(0),
            delay: Duration::ZERO,
        }
    }

    /// Make each response take `delay`, so a coalescing test has a window in
    /// which followers can actually pile up behind the leader.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// How many requests reached the wire.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// Every request seen, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<WireRequest> {
        self.seen.lock().expect("transport log poisoned").clone()
    }
}

#[async_trait]
impl Transport for CountingTransport {
    async fn send(&self, request: &WireRequest) -> Result<WireResponse, ApertureError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.seen
            .lock()
            .expect("transport log poisoned")
            .push(request.clone());
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let scripted = self
            .responses
            .lock()
            .expect("transport script poisoned")
            .pop();
        if let Some(result) = scripted {
            return result;
        }
        self.default
            .lock()
            .expect("transport default poisoned")
            .clone()
            .ok_or_else(|| ApertureError::Transport("script exhausted".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aperture::egress::EgressMode;

    fn ok_response(body: &[u8], etag: Option<&str>) -> WireResponse {
        WireResponse {
            status: 200,
            body: body.to_vec(),
            headers: Vec::new(),
            etag: etag.map(str::to_string),
            last_modified: None,
            content_type: Some("application/json".to_string()),
        }
    }

    fn not_modified() -> WireResponse {
        WireResponse {
            status: 304,
            body: Vec::new(),
            headers: Vec::new(),
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
            content_type: None,
        }
    }

    fn client(transport: Arc<dyn Transport>) -> ApertureClient {
        ApertureClient::new(
            transport,
            Arc::new(ResponseCache::new(1 << 20)),
            Arc::new(EgressPolicy::new(EgressMode::Dev)),
        )
    }

    #[tokio::test]
    async fn a_fresh_entry_is_served_without_upstream_contact() {
        let transport = Arc::new(CountingTransport::always(ok_response(b"{}", None)));
        let client = client(transport.clone());
        let request = ApertureRequest::get("https://x.test/a", Duration::from_secs(60));

        let first = client.fetch(&request).await.unwrap();
        assert_eq!(first.disposition, Disposition::Fetched);
        let second = client.fetch(&request).await.unwrap();
        assert_eq!(second.disposition, Disposition::FreshHit);

        assert_eq!(transport.calls(), 1);
        assert_eq!(client.metrics().fresh_hits, 1);
    }

    #[tokio::test]
    async fn a_stale_entry_sends_a_conditional_request() {
        let transport = Arc::new(CountingTransport::scripted(vec![
            Ok(ok_response(b"{\"n\":1}", Some("\"v1\""))),
            Ok(not_modified()),
        ]));
        let client = client(transport.clone());
        let request = ApertureRequest::get("https://x.test/a", Duration::ZERO);

        client.fetch(&request).await.unwrap();
        let second = client.fetch(&request).await.unwrap();

        assert_eq!(second.disposition, Disposition::NotModified);
        assert_eq!(&*second.response.body, b"{\"n\":1}", "304 keeps the body");

        let sent = transport.requests();
        assert!(!sent[0].is_conditional(), "nothing to validate with yet");
        assert!(sent[1].is_conditional(), "second must carry If-None-Match");
        assert_eq!(
            sent[1]
                .headers
                .iter()
                .find(|(name, _)| name == "if-none-match")
                .map(|(_, value)| value.as_str()),
            Some("\"v1\"")
        );
    }

    #[tokio::test]
    async fn an_identical_body_is_not_counted_as_a_change() {
        // A 200 that happens to carry the same bytes is not a change either.
        // Gate 3 counts value changes, so this must not inflate them.
        let transport = Arc::new(CountingTransport::always(ok_response(b"same", None)));
        let client = client(transport);
        let request = ApertureRequest::get("https://x.test/a", Duration::ZERO);

        client.fetch(&request).await.unwrap();
        client.fetch(&request).await.unwrap();
        client.fetch(&request).await.unwrap();

        assert_eq!(
            client.metrics().value_changes,
            1,
            "only the first is a change"
        );
    }

    #[tokio::test]
    async fn an_upstream_failure_serves_the_last_good_value() {
        let transport = Arc::new(CountingTransport::scripted(vec![
            Ok(ok_response(b"good", None)),
            Err(ApertureError::Transport("connection reset".to_string())),
        ]));
        let client = client(transport);
        let request = ApertureRequest::get("https://x.test/a", Duration::ZERO);

        client.fetch(&request).await.unwrap();
        let second = client.fetch(&request).await.unwrap();

        assert_eq!(second.disposition, Disposition::StaleOnError);
        assert_eq!(&*second.response.body, b"good");
        assert_eq!(client.metrics().stale_on_error, 1);
    }

    #[tokio::test]
    async fn an_upstream_failure_with_nothing_cached_is_an_error() {
        let transport = Arc::new(CountingTransport::scripted(vec![Err(
            ApertureError::Transport("dns".to_string()),
        )]));
        let client = client(transport);
        let request = ApertureRequest::get("https://x.test/a", Duration::ZERO);
        assert!(client.fetch(&request).await.is_err());
    }

    #[tokio::test]
    async fn scopes_do_not_share_a_cache_entry() {
        // APERTURE invariant 2.3 at the client level: two principals asking for
        // the same URL each pay their own request and never see each other's body.
        let transport = Arc::new(CountingTransport::scripted(vec![
            Ok(ok_response(b"alice", None)),
            Ok(ok_response(b"bob", None)),
        ]));
        let client = client(transport.clone());

        let mut alice = ApertureRequest::get("https://x.test/me", Duration::from_secs(60));
        alice.scope = CacheScope::Principal("alice".to_string());
        let mut bob = alice.clone();
        bob.scope = CacheScope::Principal("bob".to_string());

        let a = client.fetch(&alice).await.unwrap();
        let b = client.fetch(&bob).await.unwrap();

        assert_eq!(&*a.response.body, b"alice");
        assert_eq!(&*b.response.body, b"bob");
        assert_eq!(transport.calls(), 2, "must not coalesce across principals");
    }

    #[tokio::test]
    async fn a_post_is_never_cached_or_coalesced() {
        let transport = Arc::new(CountingTransport::always(ok_response(b"ok", None)));
        let client = client(transport.clone());
        let request = ApertureRequest {
            method: "POST".to_string(),
            url: "https://x.test/charge".to_string(),
            scope: CacheScope::App,
            ttl: Duration::from_secs(600),
            headers: Vec::new(),
            body: Some(b"{}".to_vec()),
        };

        client.fetch(&request).await.unwrap();
        client.fetch(&request).await.unwrap();

        assert_eq!(
            transport.calls(),
            2,
            "two identical POSTs are two distinct intentions"
        );
        assert!(client.cache().is_empty());
    }

    #[tokio::test]
    async fn egress_denial_happens_before_any_network_contact() {
        let transport = Arc::new(CountingTransport::always(ok_response(b"{}", None)));
        let client = ApertureClient::new(
            transport.clone(),
            Arc::new(ResponseCache::new(1 << 20)),
            Arc::new(EgressPolicy::new(EgressMode::Serve)),
        );
        let request = ApertureRequest::get("file:///etc/passwd", Duration::ZERO);
        assert!(matches!(
            client.fetch(&request).await,
            Err(ApertureError::Egress(_))
        ));
        assert_eq!(transport.calls(), 0);
    }

    #[tokio::test]
    async fn a_timeout_is_reported_as_a_timeout() {
        let transport = Arc::new(
            CountingTransport::always(ok_response(b"{}", None))
                .with_delay(Duration::from_millis(200)),
        );
        let client = client(transport).with_timeout(Duration::from_millis(20));
        let request = ApertureRequest::get("https://x.test/slow", Duration::ZERO);
        assert!(matches!(
            client.fetch(&request).await,
            Err(ApertureError::Timeout { .. })
        ));
    }
}
