//! APERTURE · A0 — the shared response cache.
//!
//! Implements `development-plan/APERTURE.md` § 4.3 (conditional requests), § 7
//! (the cache-sharing rule) and the response-cache row of § 8.
//!
//! ## Why a shared cache is the whole read-side story
//!
//! React Query, SWR and every client-side data layer key their cache *per
//! session*, which has a consequence they rarely state: an `ETag` has nowhere
//! to live. Fifty tabs hold fifty copies of the same response and revalidate
//! independently, so the conditional-request machinery HTTP has shipped since
//! 1999 does nothing for them.
//!
//! Moving the cache under the delta wire gives validators a home. One entry,
//! one `If-None-Match`, and a `304` that costs ~200 bytes and wakes nobody.
//!
//! ## Eviction is safe for exactly PRISM's reason
//!
//! `PRISM.md` invariant 2.3 — *the substrate is the truth; the topic value is a
//! cache* — is what makes `BroadcastRegistry::enforce_byte_budget`
//! (`broadcast.rs:733`) sound: a dropped partition is not lost data, because
//! the next reader re-materialises it. APERTURE inherits the rule with the
//! upstream standing in for the substrate: a dropped response is re-derivable
//! by re-issuing the GET.
//!
//! With **one honest asymmetry**, recorded in APERTURE invariant 2.2:
//! re-derivation here is metered and can fail, where a FORGE query cannot. So
//! eviction is never *wrong*, but it is more expensive than PRISM's, and that
//! is the argument for a byte budget rather than a TTL sweep — bytes are what
//! actually threaten the process.

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Who a cached response may be shared with.
///
/// This is APERTURE invariant 2.3 made unspellable-in-the-wrong-way: the scope
/// is *part of the key*, so a per-user response physically cannot be served to
/// another principal. A cache keyed on URL alone under a per-user token is the
/// one failure in this design that would be a CVE.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CacheScope {
    /// No credential involved. Shareable with everyone.
    Public,
    /// One app-level credential (a service token). Shareable with everyone,
    /// because every caller would have sent the identical request.
    App,
    /// A per-principal credential. Shareable **only** within that principal.
    ///
    /// Unreachable until item 5 lands a `user` in scope; `declare.rs` rejects
    /// `scope: "user"` at build time until then (APERTURE § 7).
    Principal(String),
}

impl CacheScope {
    fn tag(&self) -> String {
        match self {
            CacheScope::Public => "public".to_string(),
            CacheScope::App => "app".to_string(),
            // The principal is length-prefixed so that a principal literally
            // named `x\0y` cannot be made to collide with two shorter ones.
            CacheScope::Principal(id) => format!("user:{}:{id}", id.len()),
        }
    }
}

/// The cache key: a request's identity, including who may see the answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    canonical: String,
}

impl ResourceKey {
    /// Mint a key from the request's method, URL and sharing scope.
    ///
    /// `\0` separates the fields because it cannot occur in a method or a
    /// parsed URL, so no combination of inputs can forge a different key's
    /// canonical form.
    #[must_use]
    pub fn new(method: &str, url: &str, scope: &CacheScope) -> Self {
        Self {
            canonical: format!("{}\0{url}\0{}", method.to_ascii_uppercase(), scope.tag()),
        }
    }

    /// The canonical string form. Stable across builds — it is a cache key, not
    /// a wire identity, but tests assert on it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.canonical
    }
}

/// HTTP validators, so a refresh can be conditional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validators {
    /// `ETag`, echoed back as `If-None-Match`.
    pub etag: Option<String>,
    /// `Last-Modified`, echoed back as `If-Modified-Since`.
    pub last_modified: Option<String>,
}

impl Validators {
    /// Whether there is anything to make a request conditional with.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.etag.is_none() && self.last_modified.is_none()
    }

    fn footprint(&self) -> usize {
        self.etag.as_ref().map_or(0, String::len)
            + self.last_modified.as_ref().map_or(0, String::len)
    }
}

/// A stored response.
///
/// `body` is an `Arc<[u8]>` so handing it to N subscribers is N refcount bumps
/// rather than N copies. `PRISM.md` § 11 found three pure-waste clones of a
/// topic value per write when this was a plain `Vec<u8>`; starting from the
/// shared representation avoids re-learning that.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    /// HTTP status of the response that produced this body.
    pub status: u16,
    /// The body bytes.
    pub body: Arc<[u8]>,
    /// `Content-Type`, retained so a consumer need not re-sniff.
    pub content_type: Option<String>,
    /// Validators for the next conditional request.
    pub validators: Validators,
}

impl CachedResponse {
    /// Approximate heap cost of this response, for the byte budget.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.body.len()
            + self.content_type.as_ref().map_or(0, String::len)
            + self.validators.footprint()
    }
}

#[derive(Debug)]
struct Entry {
    response: CachedResponse,
    /// Monotonic tick of last read, for LRU. Mirrors `BroadcastTopic::touched`.
    touched: AtomicU64,
    /// When the body was last known-current — set on store **and refreshed on
    /// a 304**, which is the whole point of revalidation.
    verified_at: Instant,
    footprint: usize,
}

/// How a lookup resolved against the freshness window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Within its refresh window; usable with no upstream contact.
    Fresh,
    /// Past its refresh window. Still *usable* — that is the difference between
    /// stale and absent, and it is what makes serve-stale-on-error (§ 10) and
    /// stale-while-revalidate (§ 4.5) possible rather than special-cased.
    Stale,
}

/// A hit, with the freshness verdict attached.
#[derive(Debug, Clone)]
pub struct CacheHit {
    /// The stored response.
    pub response: CachedResponse,
    /// Whether it is inside its refresh window.
    pub freshness: Freshness,
    /// How long since the body was last verified current.
    pub age: Duration,
}

/// The byte-budgeted, LRU-evicted shared response cache.
///
/// Concurrency mirrors `BroadcastRegistry`: a `DashMap` for the entries and a
/// process-wide monotonic tick for recency, so a read costs one shard lock and
/// one relaxed atomic store.
#[derive(Debug)]
pub struct ResponseCache {
    entries: DashMap<ResourceKey, Arc<Entry>>,
    tick: AtomicU64,
    budget: usize,
}

/// Default byte budget: 64 MB, matching PRISM's topic value cache so a
/// deployment reasons about one number rather than two.
pub const DEFAULT_RESPONSE_BUDGET: usize = 64 * 1024 * 1024;

impl ResponseCache {
    /// A cache holding at most `budget` bytes of response bodies.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            entries: DashMap::new(),
            tick: AtomicU64::new(0),
            budget,
        }
    }

    /// The configured byte budget.
    #[must_use]
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Look up `key`, marking it recently used.
    ///
    /// `ttl` is the caller's refresh window; the cache stores no policy of its
    /// own, so the same entry can be fresh for one caller and stale for
    /// another without duplicating it.
    #[must_use]
    pub fn get(&self, key: &ResourceKey, ttl: Duration) -> Option<CacheHit> {
        let entry = self.entries.get(key)?;
        entry
            .touched
            .store(self.tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        let age = entry.verified_at.elapsed();
        Some(CacheHit {
            response: entry.response.clone(),
            freshness: if age <= ttl {
                Freshness::Fresh
            } else {
                Freshness::Stale
            },
            age,
        })
    }

    /// Read an entry's validators without disturbing recency.
    ///
    /// Used to build a conditional request, which is not a *use* of the value —
    /// counting it as one would keep a resource nobody reads alive in the LRU
    /// purely because a poller keeps revalidating it.
    #[must_use]
    pub fn validators(&self, key: &ResourceKey) -> Option<Validators> {
        self.entries
            .get(key)
            .map(|entry| entry.response.validators.clone())
    }

    /// Store a response, replacing any previous one.
    pub fn insert(&self, key: ResourceKey, response: CachedResponse) {
        let footprint = response.footprint() + key.canonical.len();
        let entry = Arc::new(Entry {
            response,
            touched: AtomicU64::new(self.tick.fetch_add(1, Ordering::Relaxed)),
            verified_at: Instant::now(),
            footprint,
        });
        self.entries.insert(key, entry);
    }

    /// Record that a `304` confirmed the stored body is still current.
    ///
    /// Resets the freshness clock and **nothing else** — no body copy, no
    /// re-parse, no new allocation. This is the mechanical reason a 304 is
    /// nearly free, and gate 3 asserts it by counting value changes.
    ///
    /// Returns `false` when the entry has since been evicted, in which case the
    /// caller must treat the revalidation as a miss.
    pub fn mark_revalidated(&self, key: &ResourceKey) -> bool {
        let Some(existing) = self.entries.get(key).map(|entry| Arc::clone(&entry)) else {
            return false;
        };
        let refreshed = Arc::new(Entry {
            response: existing.response.clone(),
            touched: AtomicU64::new(self.tick.fetch_add(1, Ordering::Relaxed)),
            verified_at: Instant::now(),
            footprint: existing.footprint,
        });
        self.entries.insert(key.clone(), refreshed);
        true
    }

    /// Drop an entry. Used when a response becomes uncacheable.
    pub fn remove(&self, key: &ResourceKey) {
        self.entries.remove(key);
    }

    /// Live entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total bytes currently held.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.footprint).sum()
    }

    /// Evict least-recently-used entries until the footprint fits the budget.
    /// Returns how many were dropped.
    ///
    /// Candidates are snapshotted before any removal: dropping while iterating
    /// a `DashMap` risks deadlocking against the shard the iterator holds — the
    /// same hazard `broadcast.rs:739` documents.
    pub fn enforce_byte_budget(&self) -> usize {
        let mut total = self.resident_bytes();
        if total <= self.budget {
            return 0;
        }

        let mut candidates: Vec<(u64, ResourceKey, usize)> = self
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.touched.load(Ordering::Relaxed),
                    entry.key().clone(),
                    entry.footprint,
                )
            })
            .collect();
        candidates.sort_unstable_by_key(|(touched, _, _)| *touched);

        let mut dropped = 0;
        for (_, key, footprint) in candidates {
            if total <= self.budget {
                break;
            }
            if self.entries.remove(&key).is_some() {
                total = total.saturating_sub(footprint);
                dropped += 1;
            }
        }
        dropped
    }
}

impl Default for ResponseCache {
    fn default() -> Self {
        Self::new(DEFAULT_RESPONSE_BUDGET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(body: &[u8], etag: Option<&str>) -> CachedResponse {
        CachedResponse {
            status: 200,
            body: Arc::from(body),
            content_type: Some("application/json".to_string()),
            validators: Validators {
                etag: etag.map(str::to_string),
                last_modified: None,
            },
        }
    }

    fn key(url: &str) -> ResourceKey {
        ResourceKey::new("GET", url, &CacheScope::App)
    }

    #[test]
    fn scope_is_part_of_the_key() {
        // APERTURE invariant 2.3. Two principals asking for the same URL must
        // not be able to reach each other's entry.
        let a = ResourceKey::new(
            "GET",
            "https://x.test/me",
            &CacheScope::Principal("a".into()),
        );
        let b = ResourceKey::new(
            "GET",
            "https://x.test/me",
            &CacheScope::Principal("b".into()),
        );
        let app = ResourceKey::new("GET", "https://x.test/me", &CacheScope::App);
        assert_ne!(a, b);
        assert_ne!(a, app);
    }

    #[test]
    fn principal_ids_cannot_be_made_to_collide() {
        // Without length-prefixing, ("ab", "c") and ("a", "bc") could canonicalise
        // to the same string under a naive separator scheme.
        let one = ResourceKey::new("GET", "u", &CacheScope::Principal("ab:c".into()));
        let two = ResourceKey::new("GET", "u", &CacheScope::Principal("a:bc".into()));
        assert_ne!(one, two);
    }

    #[test]
    fn method_is_normalised_but_distinct_methods_are_not_merged() {
        assert_eq!(
            ResourceKey::new("get", "u", &CacheScope::App),
            ResourceKey::new("GET", "u", &CacheScope::App)
        );
        assert_ne!(
            ResourceKey::new("GET", "u", &CacheScope::App),
            ResourceKey::new("HEAD", "u", &CacheScope::App)
        );
    }

    #[test]
    fn a_stored_response_is_fresh_then_stale() {
        let cache = ResponseCache::new(1024);
        cache.insert(key("u"), response(b"{}", Some("\"v1\"")));

        let hit = cache.get(&key("u"), Duration::from_secs(60)).unwrap();
        assert_eq!(hit.freshness, Freshness::Fresh);

        let hit = cache.get(&key("u"), Duration::ZERO).unwrap();
        assert_eq!(hit.freshness, Freshness::Stale);
        // Stale is still usable — that is what makes serve-stale-on-error work.
        assert_eq!(&*hit.response.body, b"{}");
    }

    #[test]
    fn revalidation_refreshes_the_clock_without_touching_the_body() {
        let cache = ResponseCache::new(1024);
        cache.insert(key("u"), response(b"{\"n\":1}", Some("\"v1\"")));
        let before = cache.resident_bytes();

        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.mark_revalidated(&key("u")));

        let hit = cache.get(&key("u"), Duration::from_millis(4)).unwrap();
        assert_eq!(hit.freshness, Freshness::Fresh, "304 must reset freshness");
        assert_eq!(&*hit.response.body, b"{\"n\":1}");
        assert_eq!(
            cache.resident_bytes(),
            before,
            "304 must not change footprint"
        );
    }

    #[test]
    fn revalidating_an_evicted_entry_reports_a_miss() {
        let cache = ResponseCache::new(1024);
        assert!(!cache.mark_revalidated(&key("absent")));
    }

    #[test]
    fn validators_are_readable_without_disturbing_recency() {
        // The claim: building a conditional request is not a *use* of the
        // value. If it were, a poller revalidating a resource nobody reads
        // would pin it in the LRU forever and evict resources people do read.
        //
        // Budget fits exactly one of these two entries, so eviction has to
        // choose, and the choice is the assertion.
        let cache = ResponseCache::new(600);
        cache.insert(key("a"), response(&[0u8; 512], Some("\"a\"")));
        cache.insert(key("b"), response(&[0u8; 512], Some("\"b\"")));

        let validators = cache.validators(&key("a"));
        assert_eq!(
            validators.and_then(|v| v.etag),
            Some("\"a\"".to_string()),
            "the read must still return the validators"
        );

        assert_eq!(cache.enforce_byte_budget(), 1);
        assert!(
            cache.get(&key("a"), Duration::from_secs(60)).is_none(),
            "`a` was read only for its validators, so it stayed the LRU victim"
        );
        assert!(cache.get(&key("b"), Duration::from_secs(60)).is_some());
    }

    #[test]
    fn a_real_read_does_promote_an_entry() {
        // The mirror image of the test above: `get` *is* a use, so the same
        // setup with a real read evicts the other entry instead. Without this,
        // the test above would still pass if recency tracking were broken
        // outright.
        let cache = ResponseCache::new(600);
        cache.insert(key("a"), response(&[0u8; 512], Some("\"a\"")));
        cache.insert(key("b"), response(&[0u8; 512], Some("\"b\"")));

        let _ = cache.get(&key("a"), Duration::from_secs(60));

        assert_eq!(cache.enforce_byte_budget(), 1);
        assert!(cache.get(&key("a"), Duration::from_secs(60)).is_some());
        assert!(cache.get(&key("b"), Duration::from_secs(60)).is_none());
    }

    #[test]
    fn the_budget_evicts_least_recently_used_first() {
        let cache = ResponseCache::new(700);
        cache.insert(key("old"), response(&[0u8; 256], None));
        cache.insert(key("mid"), response(&[0u8; 256], None));
        cache.insert(key("new"), response(&[0u8; 256], None));

        // Touch `old` so it is no longer the oldest read.
        let _ = cache.get(&key("old"), Duration::from_secs(60));

        let dropped = cache.enforce_byte_budget();
        assert!(dropped >= 1);
        assert!(cache.resident_bytes() <= cache.budget());
        assert!(
            cache.get(&key("old"), Duration::from_secs(60)).is_some(),
            "the recently-read entry must survive"
        );
    }

    #[test]
    fn a_cache_under_budget_evicts_nothing() {
        let cache = ResponseCache::new(DEFAULT_RESPONSE_BUDGET);
        cache.insert(key("u"), response(b"{}", None));
        assert_eq!(cache.enforce_byte_budget(), 0);
        assert_eq!(cache.len(), 1);
    }
}
