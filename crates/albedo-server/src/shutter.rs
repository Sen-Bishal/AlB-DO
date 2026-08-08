//! SHUTTER · the HTTP half — who is asking, and what to tell them.
//!
//! The mechanism lives in [`dom_render_compiler::shutter`]. This is the part
//! that needs a request: deciding the subject, and writing a refusal a client
//! can act on.
//!
//! ## The subject, in priority order
//!
//! 1. **The principal**, when AUTH resolved one. Rationing the actor is strictly better than rationing their network position.
//! 2. **The client address**, otherwise — with the trust question below answered before it is believed.
//!
//! ## `X-Forwarded-For` is a vulnerability until it is configured
//!
//! Every deployment behind a load balancer sees the balancer's address as the
//! peer, so a limiter that only reads the socket buckets the entire internet
//! into one key and is useless. The obvious fix — read `X-Forwarded-For` — is
//! worse than useless: the header is client-supplied, so **any attacker can put
//! a fresh value in it on every request and get a fresh budget every time.**
//! That is a total bypass, and it ships by default in a lot of middleware.
//!
//! The rule here: the header is read **only** when the peer is a configured
//! trusted proxy, and the value taken is the **rightmost entry that is not
//! itself trusted** — walking right-to-left is what makes prepended values
//! (which an attacker controls) unreachable, because the attacker's forgeries
//! sit to the *left* of what the real proxy appended.
//!
//! Default: [`TrustedProxies::none`]. A deployment behind a balancer must say
//! so, and one that forgets gets a limiter that is too strict rather than one
//! that can be walked past.

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use dom_render_compiler::shutter::{Cost, Key, OperationClass, QuotaError, Shutter, Verdict};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Addresses whose `X-Forwarded-For` we believe.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    entries: Vec<IpAddr>,
}

impl TrustedProxies {
    /// Trust nothing. **The default**, and the safe direction to be wrong in:
    /// an unconfigured deployment behind a balancer rate-limits too
    /// aggressively, which is visible and fixable, rather than not at all,
    /// which is neither.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Trust these addresses to have appended an honest `X-Forwarded-For`.
    #[must_use]
    pub fn new(entries: Vec<IpAddr>) -> Self {
        Self { entries }
    }

    fn trusts(&self, addr: IpAddr) -> bool {
        self.entries.contains(&addr)
    }

    /// How many addresses are trusted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is trusted — the default.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The client address to ration, given the socket peer and the headers.
    ///
    /// Returns the peer unchanged when it is not a trusted proxy — the header is
    /// not merely ignored in that case, it is *unread*, so there is no path from
    /// a client-supplied string to a bucket key.
    #[must_use]
    pub fn client_addr(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        if !self.trusts(peer) {
            return peer;
        }
        let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
        else {
            return peer;
        };

        // Right to left: the rightmost entries were appended by infrastructure
        // we trust, and anything the client forged sits to the left of them.
        // The first entry that is not itself a trusted hop is the real client.
        forwarded
            .rsplit(',')
            .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
            .find(|addr| !self.trusts(*addr))
            .unwrap_or(peer)
    }
}

/// Build the rate-limit subject for a request.
///
/// `principal` is AUTH's answer; `client` is the address resolved through
/// [`TrustedProxies`].
#[must_use]
pub fn subject(
    identity: &crate::auth::Identity,
    client: IpAddr,
    class: OperationClass,
) -> Key {
    match identity.principal() {
        Some(principal) => Key::Principal {
            id: principal.id.as_str().to_string(),
            class,
        },
        None => Key::Address {
            addr: client,
            class,
        },
    }
}

/// Environment variable naming the load balancers whose `X-Forwarded-For` this
/// deployment believes. Comma-separated IP addresses.
///
/// **Deliberately the environment and not `albedo.config.ts`.** Which addresses
/// sit in front of the process is a property of where it is deployed, not of the
/// application: the same committed config runs on a laptop with nothing in front
/// of it, in a container behind one balancer, and behind two on a platform. A
/// value that must differ per environment does not belong in a file that is the
/// same in every environment.
pub const TRUSTED_PROXIES_ENV: &str = "ALBEDO_TRUSTED_PROXIES";

/// The bucket a request answers to when no peer address is available.
///
/// Only reachable when the router was mounted without connect info — an embedder
/// calling [`AlbedoServer::router`](crate::server::AlbedoServer::router) directly,
/// or an in-process test harness. Everything unattributed shares this one cell,
/// which is the strict direction: an unattributed flood throttles itself rather
/// than escaping rationing entirely. The serve path always supplies an address.
const UNATTRIBUTED: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

/// The limiter, plus the trust question that has to be answered before an
/// address means anything.
///
/// Lives on the server's **persistent** tier, beside the broadcast registry and
/// the FORGE substrate: a dev hot reload replaces build output, and a reload that
/// also reset every accumulated limit would hand an attacker a budget refill for
/// every file the author saves.
pub struct Limiter {
    shutter: Shutter,
    proxies: TrustedProxies,
}

impl Limiter {
    /// Build with the default limits and the deployment's declared proxies.
    ///
    /// # Errors
    /// A string naming the problem — an unparseable [`TRUSTED_PROXIES_ENV`]
    /// entry, or limits that could not admit their own heaviest operation.
    /// Both are boot failures on purpose: a limiter that cannot admit its
    /// heaviest write refuses that write at every instant, and the symptom
    /// looks exactly like load.
    pub fn from_env() -> Result<Self, String> {
        let proxies = match std::env::var(TRUSTED_PROXIES_ENV) {
            Ok(raw) => parse_trusted_proxies(&raw)?,
            Err(_) => TrustedProxies::none(),
        };
        let shutter = Shutter::new().map_err(|err: QuotaError| err.to_string())?;
        Ok(Self { shutter, proxies })
    }

    /// Build from parts. Tests inject a [`ManualClock`](dom_render_compiler::shutter::ManualClock)
    /// through the [`Shutter`]; nothing in production does.
    #[must_use]
    pub fn with(shutter: Shutter, proxies: TrustedProxies) -> Self {
        Self { shutter, proxies }
    }

    /// How many addresses this deployment trusts to have appended an honest
    /// `X-Forwarded-For`. Reported at boot, because zero behind a balancer is a
    /// misconfiguration whose only symptom is over-strict limiting.
    #[must_use]
    pub fn trusted_proxies(&self) -> usize {
        self.proxies.len()
    }

    /// Who this request is rationed as.
    ///
    /// A resolved principal wins outright; otherwise the address, run through the
    /// trust rule first so a forged header can never choose the bucket.
    #[must_use]
    pub fn key(
        &self,
        identity: &crate::auth::Identity,
        peer: Option<IpAddr>,
        headers: &HeaderMap,
        class: OperationClass,
    ) -> Key {
        let client = peer.map_or(UNATTRIBUTED, |peer| self.proxies.client_addr(peer, headers));
        subject(identity, client, class)
    }

    /// Charge a derived cost. The admission decision.
    #[must_use]
    pub fn charge(&self, key: &Key, cost: Cost) -> Verdict {
        self.shutter.charge(key, cost)
    }

    /// Record a cost that only became knowable after the work ran. See
    /// [`Shutter::debit`].
    pub fn debit(&self, key: &Key, cost: Cost) {
        self.shutter.debit(key, cost);
    }

    /// The underlying limiter, for the login endpoints' two-bucket path and for
    /// diagnostics.
    #[must_use]
    pub fn shutter(&self) -> &Shutter {
        &self.shutter
    }
}

fn parse_trusted_proxies(raw: &str) -> Result<TrustedProxies, String> {
    let mut entries = Vec::new();
    for field in raw.split(',') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let addr: IpAddr = field.parse().map_err(|_| {
            format!(
                "{TRUSTED_PROXIES_ENV} contains '{field}', which is not an IP address. It takes a \
                 comma-separated list of the addresses of the load balancers in front of this \
                 process — the ones whose X-Forwarded-For may be believed. Leave it unset when \
                 nothing is in front."
            )
        })?;
        entries.push(addr);
    }
    Ok(TrustedProxies::new(entries))
}

tokio::task_local! {
    /// The fan-out one action dispatch observed, in units of subscribed lanes.
    ///
    /// A write's blast radius is discovered deep inside the write path — inside
    /// the transaction, where an update finally knows which partition it moves —
    /// and it has to reach the dispatcher, which is the only place that knows
    /// *who* to charge. A task-local is the channel between them because the two
    /// ends are the same task and nothing in between has any business carrying a
    /// rate limiter through its signature.
    ///
    /// Mirrors [`install_forge_write_collector`](dom_render_compiler::forge::install_forge_write_collector),
    /// which solves the same shape one layer down — with the difference that the
    /// write collector is a *thread*-local because a body runs on a pooled engine
    /// thread, while the write itself is awaited back on the request's own task.
    static FAN_OUT: Arc<AtomicU32>;
}

/// Run `future` with a fan-out meter installed, and report what it observed.
///
/// Outside such a scope [`note_fan_out`] is a no-op, so the write path is not
/// obliged to know whether anyone is measuring.
pub async fn metered<F, T>(future: F) -> (T, u32)
where
    F: std::future::Future<Output = T>,
{
    let meter = Arc::new(AtomicU32::new(0));
    let observed = Arc::clone(&meter);
    let value = FAN_OUT.scope(meter, future).await;
    (value, observed.load(Ordering::Relaxed))
}

/// Record the lanes a write is about to reach.
///
/// Accumulates rather than overwrites: one action may apply several write
/// batches, and the price of the dispatch is what all of them reached.
pub fn note_fan_out(subscribers: u32) {
    let _ = FAN_OUT.try_with(|meter| {
        meter.fetch_add(subscribers, Ordering::Relaxed);
    });
}

/// Header names from the IETF `ratelimit-headers` draft, which is what modern
/// clients and SDKs look for.
const LIMIT: &str = "ratelimit-limit";
const REMAINING: &str = "ratelimit-remaining";
const RESET: &str = "ratelimit-reset";
const POLICY: &str = "ratelimit-policy";

/// Stamp rate-limit headers onto any response.
///
/// Emitted on **success as well as refusal**, deliberately: a client that only
/// learns its budget when it has already run out cannot pace itself, which is
/// how a well-behaved integration turns into a thundering herd. Telling it
/// `remaining` on every response is what lets it slow down before being told to.
pub fn stamp(headers: &mut HeaderMap, verdict: &Verdict) {
    let mut set = |name: &'static str, value: u64| {
        if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
            headers.insert(name, value);
        }
    };
    set(LIMIT, u64::from(verdict.limit));
    set(REMAINING, u64::from(verdict.decision.remaining()));
    set(RESET, verdict.decision.reset_after().as_secs());

    if let Ok(policy) = HeaderValue::from_str(&format!(
        "{};w={};class={}",
        verdict.limit,
        verdict.decision.reset_after().as_secs().max(1),
        verdict.cost.class.as_str()
    )) {
        headers.insert(POLICY, policy);
    }

    if let Some(retry) = verdict.retry_after_secs() {
        if let Ok(value) = HeaderValue::from_str(&retry.to_string()) {
            headers.insert(axum::http::header::RETRY_AFTER, value);
        }
    }
}

/// The 429.
///
/// The body explains the *derivation*, not just the refusal. A limit that fires
/// without saying why is a limit somebody disables — and because ours is
/// derived rather than typed, "why" is a sentence we can actually produce:
/// *write costs 13 (4 base + 9 for fan-out to 512 subscribed lanes)*. No
/// path-and-IP limiter can say that, because it does not know it.
#[must_use]
pub fn too_many_requests(verdict: &Verdict) -> Response {
    let retry = verdict.retry_after_secs().unwrap_or(1);
    let body = serde_json::json!({
        "error": "rate_limited",
        "class": verdict.cost.class.as_str(),
        "retryAfterSeconds": retry,
        "why": verdict.cost.explain(),
        // Surfaced rather than hidden: when the exact table is saturated the
        // limiter is sharing cells, so a caller may be paying for a neighbour.
        // An operator seeing this in the wild is seeing a cardinality attack.
        "degraded": verdict.degraded,
    });

    let mut response = Response::new(axum::body::Body::from(body.to_string()));
    *response.status_mut() = StatusCode::TOO_MANY_REQUESTS;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    stamp(response.headers_mut(), verdict);
    response
}

// A `write_cost(broadcast, topic)` helper used to live here, deriving a write's
// price from one topic's subscriber count. It was wrong in a way that only
// showed up when the write path was actually wired: the caller does not know the
// topic. A partitioned write lands on `messages:u_7f3a`, not `messages`, and an
// update that moves a row across partitions lands on two — neither knowable
// until the row has been read inside the transaction. So the count is now taken
// where it is true, by `apply_writes`, and arrives here as
// [`dom_render_compiler::forge::FanOut`].

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::auth::principal::{Principal, PrincipalId};
    use dom_render_compiler::shutter::{Decision, Limits, ManualClock, Shutter};
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::time::Duration;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    fn headers_with_xff(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", value.parse().unwrap());
        headers
    }

    /// **The bypass.** An untrusted peer's header must not be read at all — not
    /// sanitised, not preferred-but-checked. Unread.
    #[test]
    fn an_untrusted_peer_cannot_choose_its_own_bucket() {
        let proxies = TrustedProxies::none();
        let peer = ip(203, 0, 113, 9);

        for forged in [
            "1.2.3.4",
            "9.9.9.9, 8.8.8.8",
            "not-an-ip",
            "127.0.0.1, 1.1.1.1, 2.2.2.2",
        ] {
            assert_eq!(
                proxies.client_addr(peer, &headers_with_xff(forged)),
                peer,
                "header `{forged}` from an untrusted peer changed the bucket"
            );
        }
    }

    #[test]
    fn a_trusted_proxy_reveals_the_real_client() {
        let balancer = ip(10, 0, 0, 1);
        let proxies = TrustedProxies::new(vec![balancer]);
        assert_eq!(
            proxies.client_addr(balancer, &headers_with_xff("198.51.100.7")),
            ip(198, 51, 100, 7)
        );
    }

    /// The reason for walking right-to-left: an attacker prepends whatever they
    /// like, and the real proxy appends the truth to the right of it.
    #[test]
    fn a_forged_prefix_behind_a_trusted_proxy_is_unreachable() {
        let balancer = ip(10, 0, 0, 1);
        let proxies = TrustedProxies::new(vec![balancer]);

        // The attacker sent `X-Forwarded-For: 1.2.3.4`; the balancer appended
        // their actual address.
        let seen = proxies.client_addr(balancer, &headers_with_xff("1.2.3.4, 198.51.100.7"));
        assert_eq!(
            seen,
            ip(198, 51, 100, 7),
            "the attacker's forged entry was believed"
        );
    }

    /// A chain of trusted hops is walked through to the first untrusted one.
    #[test]
    fn a_chain_of_trusted_hops_resolves_to_the_first_untrusted_entry() {
        let edge = ip(10, 0, 0, 1);
        let inner = ip(10, 0, 0, 2);
        let proxies = TrustedProxies::new(vec![edge, inner]);
        assert_eq!(
            proxies.client_addr(edge, &headers_with_xff("198.51.100.7, 10.0.0.2")),
            ip(198, 51, 100, 7)
        );
    }

    #[test]
    fn a_trusted_proxy_with_no_header_falls_back_to_the_peer() {
        let balancer = ip(10, 0, 0, 1);
        let proxies = TrustedProxies::new(vec![balancer]);
        assert_eq!(
            proxies.client_addr(balancer, &HeaderMap::new()),
            balancer
        );
    }

    /// A principal outranks an address: the same person behind a different
    /// address must carry their budget with them, and two people behind one
    /// address must not share.
    #[test]
    fn a_principal_outranks_an_address() {
        let principal = crate::auth::Identity::Authenticated {
            principal: Box::new(Principal::new(
                PrincipalId::parse("u_7f3a").unwrap(),
                "passkey",
            )),
            token: dom_render_compiler::auth::TokenHash::of("t"),
        };

        let from_home = subject(&principal, ip(198, 51, 100, 7), OperationClass::Write);
        let from_cafe = subject(&principal, ip(203, 0, 113, 9), OperationClass::Write);
        assert_eq!(from_home, from_cafe, "a principal's budget follows them");

        let anonymous = crate::auth::Identity::Anonymous;
        assert_ne!(
            subject(&anonymous, ip(198, 51, 100, 7), OperationClass::Write),
            from_home
        );
    }

    /// The channel between the write path (which learns the blast radius) and
    /// the dispatcher (which knows who to charge). It accumulates, because one
    /// action may apply several write batches and the dispatch is priced by what
    /// all of them reached.
    #[tokio::test]
    async fn the_meter_collects_every_fan_out_one_dispatch_reported() {
        let (value, observed) = metered(async {
            note_fan_out(3);
            note_fan_out(9);
            "done"
        })
        .await;

        assert_eq!(value, "done");
        assert_eq!(observed, 12);
    }

    /// Outside a metered scope this must be inert. The write path calls it
    /// unconditionally and has no business knowing whether anyone is measuring —
    /// and a panic there would turn "nobody is limiting this" into a 500 on a
    /// committed write.
    #[tokio::test]
    async fn reporting_fan_out_with_nobody_measuring_is_a_no_op() {
        note_fan_out(42);

        // And a scope opened afterwards starts from zero rather than inheriting
        // anything.
        let (_, observed) = metered(async {}).await;
        assert_eq!(observed, 0);
    }

    /// The trust list is a deployment fact, so it arrives as one. A typo in it
    /// must stop the boot naming itself — silently trusting nothing would look
    /// exactly like the default, and the symptom (everyone behind the balancer
    /// sharing one bucket) appears only under load.
    #[test]
    fn a_malformed_trusted_proxy_entry_is_refused_by_name() {
        let err = parse_trusted_proxies("10.0.0.1, not-an-ip").unwrap_err();
        assert!(err.contains("not-an-ip"), "{err}");
        assert!(err.contains(TRUSTED_PROXIES_ENV), "{err}");

        let parsed = parse_trusted_proxies(" 10.0.0.1 , 10.0.0.2 ,").unwrap();
        assert_eq!(parsed.len(), 2, "whitespace and a trailing comma are fine");
        assert!(parse_trusted_proxies("").unwrap().is_empty());
    }

    /// A request with no peer address must still be rationed. That is the
    /// embedder's path (`router()` mounted without connect info) and the strict
    /// direction is the right one: an unattributed flood throttles itself rather
    /// than escaping rationing altogether.
    #[test]
    fn a_request_with_no_peer_address_is_still_given_a_bucket() {
        let limiter = Limiter::with(Shutter::new().unwrap(), TrustedProxies::none());
        let key = limiter.key(
            &crate::auth::Identity::Anonymous,
            None,
            &HeaderMap::new(),
            OperationClass::Read,
        );
        assert_eq!(
            key,
            Key::Address {
                addr: UNATTRIBUTED,
                class: OperationClass::Read,
            }
        );
    }

    fn refused_verdict() -> Verdict {
        let clock = Arc::new(ManualClock::new());
        let shutter =
            Shutter::with(Limits::default(), clock as Arc<dyn dom_render_compiler::shutter::Clock>, 64)
                .unwrap();
        let key = Key::Address {
            addr: ip(198, 51, 100, 7),
            class: OperationClass::Credential,
        };
        let cost = Cost::flat(OperationClass::Credential);
        while shutter.charge(&key, cost).is_admitted() {}
        shutter.charge(&key, cost)
    }

    #[test]
    fn a_refusal_carries_every_header_a_client_needs() {
        let verdict = refused_verdict();
        let response = too_many_requests(&verdict);

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        for header in [LIMIT, REMAINING, RESET, POLICY] {
            assert!(
                response.headers().contains_key(header),
                "missing `{header}`"
            );
        }
        assert!(response
            .headers()
            .contains_key(axum::http::header::RETRY_AFTER));
        assert_eq!(response.headers()[REMAINING], "0");
    }

    /// A client that only learns its budget after running out cannot pace
    /// itself — which is how an integration becomes a thundering herd.
    #[test]
    fn an_admitted_response_also_reports_the_remaining_budget() {
        let clock = Arc::new(ManualClock::new());
        let shutter = Shutter::with(
            Limits::default(),
            clock as Arc<dyn dom_render_compiler::shutter::Clock>,
            64,
        )
        .unwrap();
        let verdict = shutter.charge(
            &Key::Address {
                addr: ip(198, 51, 100, 7),
                class: OperationClass::Read,
            },
            Cost::flat(OperationClass::Read),
        );
        assert!(verdict.is_admitted());

        let mut headers = HeaderMap::new();
        stamp(&mut headers, &verdict);
        assert!(headers.contains_key(REMAINING));
        assert_ne!(headers[REMAINING], "0");
        assert!(
            !headers.contains_key(axum::http::header::RETRY_AFTER),
            "an admitted request must not carry Retry-After"
        );
    }

    /// A limit that fires without saying why is a limit somebody disables. Ours
    /// is derived, so "why" is producible.
    #[tokio::test]
    async fn the_refusal_body_explains_the_derivation() {
        let verdict = Verdict {
            decision: Decision::Refuse {
                retry_after: Duration::from_secs(3),
                reset_after: Duration::from_secs(9),
            },
            cost: Cost::fan_out(OperationClass::Write, 512),
            limit: 60,
            degraded: false,
        };
        let response = too_many_requests(&verdict);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(body.contains("rate_limited"), "{body}");
        assert!(body.contains("fan-out to 512 subscribed lanes"), "{body}");
        assert!(body.contains("\"retryAfterSeconds\":3"), "{body}");
    }
}
