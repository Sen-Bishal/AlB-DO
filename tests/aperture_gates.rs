//! APERTURE · A0 — the merge gates from `development-plan/APERTURE.md` § 12.
//!
//! These are **assertions, not benchmarks**. Every claim A0 makes is a claim
//! about a *count* — one upstream request, zero value changes — and a count can
//! be asserted exactly. `PRISM.md` § 12 records what happens when that
//! discipline slips: its "a write costs the same" gate was written as a timing
//! claim, the timing did not hold, and the *wording* turned out to be the thing
//! that was wrong. The claim it could actually prove — *a write never re-runs
//! your query* — was a query **count** all along.
//!
//! So: gates 2, 3 and 6 live here rather than in `benches/`, and they run in
//! CI with no network, no TLS and no DNS.

use dom_render_compiler::aperture::{
    ApertureClient, ApertureRequest, CountingTransport, Disposition, EgressMode, EgressPolicy,
    ResponseCache, Transport, WireResponse,
};
use std::sync::Arc;
use std::time::Duration;

fn body_response(body: &[u8], etag: &str) -> WireResponse {
    WireResponse {
        status: 200,
        body: body.to_vec(),
        headers: Vec::new(),
        etag: Some(etag.to_string()),
        last_modified: None,
        content_type: Some("application/json".to_string()),
    }
}

fn not_modified(etag: &str) -> WireResponse {
    WireResponse {
        status: 304,
        body: Vec::new(),
        headers: Vec::new(),
        etag: Some(etag.to_string()),
        last_modified: None,
        content_type: None,
    }
}

fn client(transport: Arc<dyn Transport>) -> ApertureClient {
    ApertureClient::new(
        transport,
        Arc::new(ResponseCache::new(8 << 20)),
        // Dev: the gates are about the cache and the coalescer, and routing
        // them through address policy would only add a way to fail for an
        // unrelated reason.
        Arc::new(EgressPolicy::new(EgressMode::Dev)),
    )
}

/// **Gate 3 — conditional requests.**
///
/// *100 refreshes against an unchanged upstream produce 0 subscriber
/// notifications, and 100 of the responses are 304.*
///
/// Subscribers do not exist until A1, so the standing proxy for "notification"
/// is `value_changes`: the number of times the stored body actually moved.
/// Asserting it is zero across 100 revalidations is the same claim one phase
/// early, and it is the claim that makes a 304 nearly free — no parse, no diff,
/// no delta, no wake-up.
#[tokio::test]
async fn gate_3_unchanged_refreshes_change_nothing() {
    const REFRESHES: usize = 100;

    let mut script = vec![Ok(body_response(b"{\"stars\":42}", "\"v1\""))];
    script.extend((0..REFRESHES).map(|_| Ok(not_modified("\"v1\""))));

    let transport = Arc::new(CountingTransport::scripted(script));
    let client = client(transport.clone());
    // TTL zero: every call revalidates, which is the worst case for this gate.
    let request = ApertureRequest::get("https://api.test/repo", Duration::ZERO);

    let first = client.fetch(&request).await.expect("initial fetch");
    assert_eq!(first.disposition, Disposition::Fetched);

    for _ in 0..REFRESHES {
        let outcome = client.fetch(&request).await.expect("refresh");
        assert_eq!(outcome.disposition, Disposition::NotModified);
        assert_eq!(
            &*outcome.response.body, b"{\"stars\":42}",
            "a 304 must keep serving the stored body"
        );
    }

    let metrics = client.metrics();
    assert_eq!(
        metrics.value_changes, 1,
        "GATE 3: only the initial store is a change; {REFRESHES} refreshes changed nothing"
    );
    assert_eq!(
        metrics.not_modified, REFRESHES as u64,
        "GATE 3: every refresh must be answered 304"
    );
    assert_eq!(metrics.upstream_requests, REFRESHES as u64 + 1);

    // Every refresh must actually have carried a validator — a 304 the upstream
    // volunteered without being asked would pass the counts above while proving
    // nothing about our conditional-request machinery.
    let sent = transport.requests();
    assert!(!sent[0].is_conditional(), "nothing to validate with yet");
    assert!(
        sent[1..].iter().all(|request| request.is_conditional()),
        "GATE 3: every refresh must carry If-None-Match"
    );
    assert_eq!(metrics.conditional_requests, REFRESHES as u64);
}

/// **Gate 6 — single-flight.**
///
/// *200 simultaneous cold callers for the same resource produce exactly 1
/// upstream request.*
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn gate_6_two_hundred_cold_callers_make_one_request() {
    const CALLERS: usize = 200;

    let transport = Arc::new(
        CountingTransport::always(body_response(b"{\"shared\":true}", "\"v1\""))
            // A window wide enough for all 200 to arrive while the leader is
            // still in flight. Without it the test could pass by being serial.
            .with_delay(Duration::from_millis(50)),
    );
    let client = Arc::new(client(transport.clone()));

    let mut tasks = Vec::with_capacity(CALLERS);
    for _ in 0..CALLERS {
        let client = Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            let request = ApertureRequest::get("https://api.test/hot", Duration::from_secs(60));
            client.fetch(&request).await
        }));
    }

    for task in tasks {
        let outcome = task.await.expect("task panicked").expect("fetch failed");
        assert_eq!(&*outcome.response.body, b"{\"shared\":true}");
    }

    assert_eq!(
        transport.calls(),
        1,
        "GATE 6: {CALLERS} cold callers must coalesce into one upstream request"
    );
    assert_eq!(client.metrics().upstream_requests, 1);
}

/// **Gate 2 — fan-out independence.**
///
/// *Upstream request count is a function of distinct resources and elapsed
/// time, never of viewer count.*
///
/// The headline read-side claim: a six-widget dashboard with fifty viewers
/// costs six upstream calls per interval, not three hundred. Asserted by
/// running the identical window at 1 caller and at 200 and requiring the two
/// counts to be **equal** — not "similar", not "better".
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn gate_2_upstream_cost_is_independent_of_viewer_count() {
    async fn upstream_calls_for(viewers: usize) -> u64 {
        let transport = Arc::new(
            CountingTransport::always(body_response(b"{\"v\":1}", "\"v1\""))
                .with_delay(Duration::from_millis(20)),
        );
        let client = Arc::new(client(transport.clone()));

        let mut tasks = Vec::with_capacity(viewers);
        for _ in 0..viewers {
            let client = Arc::clone(&client);
            tasks.push(tokio::spawn(async move {
                let request =
                    ApertureRequest::get("https://api.test/widget", Duration::from_secs(60));
                client
                    .fetch(&request)
                    .await
                    .map(|outcome| outcome.disposition)
            }));
        }
        for task in tasks {
            task.await.expect("task panicked").expect("fetch failed");
        }
        transport.calls()
    }

    let one = upstream_calls_for(1).await;
    let many = upstream_calls_for(200).await;

    assert_eq!(one, 1, "a single viewer costs one request");
    assert_eq!(
        one, many,
        "GATE 2: 200 viewers must cost exactly what 1 viewer costs ({one} vs {many})"
    );
}

/// **Gate 2, second axis — distinct resources still cost distinct requests.**
///
/// Gate 2 would be trivially satisfiable by a cache that merged everything, so
/// this pins the other side: cost scales with *resources*, and it really does
/// scale with them.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn gate_2_distinct_resources_are_not_merged() {
    const WIDGETS: usize = 6;
    const VIEWERS: usize = 50;

    let transport = Arc::new(
        CountingTransport::always(body_response(b"{\"v\":1}", "\"v1\""))
            .with_delay(Duration::from_millis(20)),
    );
    let client = Arc::new(client(transport.clone()));

    let mut tasks = Vec::with_capacity(WIDGETS * VIEWERS);
    for viewer in 0..VIEWERS {
        for widget in 0..WIDGETS {
            let client = Arc::clone(&client);
            tasks.push(tokio::spawn(async move {
                let request = ApertureRequest::get(
                    format!("https://api.test/widget/{widget}"),
                    Duration::from_secs(60),
                );
                let _ = viewer;
                client.fetch(&request).await
            }));
        }
    }
    for task in tasks {
        task.await.expect("task panicked").expect("fetch failed");
    }

    assert_eq!(
        transport.calls(),
        WIDGETS as u64,
        "GATE 2: {WIDGETS} widgets x {VIEWERS} viewers must cost {WIDGETS} requests, not {}",
        WIDGETS * VIEWERS
    );
}

/// The cache-sharing rule (§ 7 / invariant 2.3) at gate scale.
///
/// Not one of the numbered gates, but it is the failure in this design that
/// would be a CVE rather than a bug, so it gets an assertion next to them:
/// coalescing must never merge two principals into one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn principals_are_never_coalesced_together() {
    use dom_render_compiler::aperture::CacheScope;

    const PRINCIPALS: usize = 50;

    let transport = Arc::new(
        CountingTransport::always(body_response(b"{\"me\":true}", "\"v1\""))
            .with_delay(Duration::from_millis(20)),
    );
    let client = Arc::new(client(transport.clone()));

    let mut tasks = Vec::with_capacity(PRINCIPALS);
    for principal in 0..PRINCIPALS {
        let client = Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            let mut request = ApertureRequest::get("https://api.test/me", Duration::from_secs(60));
            request.scope = CacheScope::Principal(format!("user-{principal}"));
            client.fetch(&request).await
        }));
    }
    for task in tasks {
        task.await.expect("task panicked").expect("fetch failed");
    }

    assert_eq!(
        transport.calls(),
        PRINCIPALS as u64,
        "each principal must pay its own request — merging them is the CVE"
    );
}
