//! APERTURE · A2 — driving a suspended action body to completion.
//!
//! `journal.rs` records what happened, `bridge.rs` suspends the body, and
//! `compiled.rs` runs **one** pass. This module is the thing that runs passes
//! *until the body finishes*: it issues what a pass asked for, appends the
//! outcomes, and hands the grown journal back for the next pass.
//!
//! ## Why the loop is here and not inside the dispatch
//!
//! `invoke_action_quickjs_pass` is synchronous and resolving a request is not,
//! so a loop written inside it would hold a QuickJS engine across the round trip.
//! That is invariant 2.6, and gate 5 priced the difference at 403.9 ms against
//! 52.7 ms with a peak of 2 engines in flight against 16 — a gap no pool size
//! closes, because under the blocking shape *the engine is the thing waiting*.
//!
//! So the loop lives above the sync/async boundary, and [`drive_workflow`] is
//! generic over "run one pass" rather than over the engine: the server hands it
//! a closure that checks an engine out of the pool, runs one pass, and returns
//! it. Between passes nothing is checked out.
//!
//! ## What this module refuses to decide
//!
//! Whether the effects of a completed body commit. It returns them; the caller
//! applies them, after the body returned `Ok`, exactly where `server.rs` already
//! did. A suspended pass's side channel is **dropped on the floor here** — see
//! [`drive_workflow`]'s contract — because a pass that did not finish did not
//! ask for its appends to stand on their own.

use crate::aperture::cache::CacheScope;
use crate::aperture::client::{ApertureClient, ApertureError, ApertureRequest};
use crate::aperture::journal::{Journal, JournalError, StepKind, StepOutcome, DEFAULT_PASS_CAP};
use crate::ir::opcode::Instruction;
use crate::runtime::bridge::PendingRequest;
use crate::runtime::compiled::ActionPass;
use serde_json::{Map, Value};
use std::future::Future;
use std::time::{Duration, Instant};

/// The header a derived idempotency key travels in.
///
/// Standards-track: `draft-ietf-httpapi-idempotency-key-header`. Emitting the
/// registered name rather than a private one is free credibility — Stripe and
/// every serious payments API already implement the server half, so a retry
/// under the same key is deduplicated by the upstream without the author having
/// arranged anything.
pub const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";

/// How long one workflow may take across **all** its passes (§ 8, R7).
///
/// This bounds the client's wait, not the server's capacity: a suspended body
/// holds no engine, so a workflow sitting on a slow upstream costs a socket and
/// a future. That asymmetry is why 30 s is affordable here and would not be
/// under a blocking host function.
pub const DEFAULT_WORKFLOW_DEADLINE: Duration = Duration::from_secs(30);

/// The caps one dispatch runs under.
#[derive(Debug, Clone, Copy)]
pub struct WorkflowLimits {
    /// Wall clock across every pass and every round trip.
    pub deadline: Duration,
    /// Hard ceiling on body passes, so a body that suspends without recording
    /// progress cannot replay forever.
    pub max_passes: usize,
}

impl Default for WorkflowLimits {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_WORKFLOW_DEADLINE,
            max_passes: DEFAULT_PASS_CAP,
        }
    }
}

/// Why a workflow did not complete.
///
/// Every variant means **no effects committed**. That is not a policy this type
/// enforces — it falls out of the protocol, since only `ActionPass::Completed`
/// carries instructions at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowError {
    /// The deadline passed with the body still suspended (§ 8, R7).
    Deadline {
        /// The configured limit.
        after: Duration,
        /// Passes run before it tripped.
        passes: usize,
    },
    /// The pass ceiling was reached.
    PassCap {
        /// The configured limit.
        cap: usize,
    },
    /// The journal refused an append — a divergent or out-of-order replay.
    Journal(JournalError),
    /// The `build_id` the workflow started under is not the one running now
    /// (§ 11 R8).
    BuildChanged {
        /// What the journal was opened against.
        started: String,
        /// What is running.
        now: String,
    },
    /// A pass suspended without staging anything, so another pass would ask the
    /// same question forever. A protocol violation rather than a user error.
    NoProgress {
        /// Which pass.
        pass: usize,
    },
    /// A pass was seeded with a journal that is not the one being grown.
    JournalDesync {
        /// What the body reported reading.
        seeded: u32,
        /// What the driver holds.
        held: usize,
    },
    /// The pass itself failed — the body threw, or the engine did.
    Pass(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deadline { after, passes } => write!(
                f,
                "aperture: this action exceeded its {after:?} workflow deadline after {passes} \
                 pass(es); nothing it did was committed"
            ),
            Self::PassCap { cap } => write!(
                f,
                "aperture: this action ran its body {cap} times without finishing; nothing it did \
                 was committed"
            ),
            Self::Journal(err) => write!(f, "{err}"),
            Self::BuildChanged { started, now } => write!(
                f,
                "aperture: this workflow started under build `{started}` and the server is now \
                 running build `{now}`; replaying new code against steps recorded by old code \
                 would diverge silently, so it was refused"
            ),
            Self::NoProgress { pass } => write!(
                f,
                "aperture: pass {pass} suspended without staging a request, which would replay \
                 forever"
            ),
            Self::JournalDesync { seeded, held } => write!(
                f,
                "aperture: the body read back {seeded} journal step(s) but the driver holds \
                 {held}; the pass was seeded from a different log"
            ),
            Self::Pass(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WorkflowError {}

impl From<JournalError> for WorkflowError {
    fn from(err: JournalError) -> Self {
        Self::Journal(err)
    }
}

/// Turn one staged request into the client request that goes on the wire.
///
/// `scope` and `ttl` are inert on this path — [`ApertureClient::send_effect`]
/// neither reads nor writes the cache — and are set to the narrowest values that
/// mean nothing rather than to values that would be wrong if the path ever
/// changed.
fn wire_request(pending: &PendingRequest, journal: &Journal) -> ApertureRequest {
    let mut headers = pending.headers.clone();

    // § 5.3 — the key is the journal position, and the author never types one.
    //
    // Only for methods that can have an effect: `Idempotency-Key` on a GET is
    // noise, and some upstreams reject headers they do not expect. A body that
    // supplied its own key keeps it — § 6's escape hatch is not a cage, and a
    // caller who wrote a key was reasoning about an upstream we cannot see.
    let idempotent = matches!(pending.method.as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE");
    let authored = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(IDEMPOTENCY_KEY_HEADER));
    if !idempotent && !authored {
        headers.push((
            IDEMPOTENCY_KEY_HEADER.to_string(),
            journal.idempotency_key(pending.step),
        ));
    }

    ApertureRequest {
        method: pending.method.clone(),
        url: pending.url.clone(),
        scope: CacheScope::App,
        ttl: Duration::ZERO,
        headers,
        body: pending.body.as_ref().map(|body| body.clone().into_bytes()),
    }
}

/// Encode a response as the record `__albedo_response` replays.
///
/// Shaped like the web platform's because § 5.5 requires copy-pasted vendor code
/// to run verbatim: `status`, `ok`, `url`, `headers.get()`, `text()`, `json()`.
fn response_record(url: &str, status: u16, headers: &[(String, String)], body: String) -> Value {
    let mut map = Map::new();
    for (name, value) in headers {
        let key = name.to_ascii_lowercase();
        match map.get_mut(&key) {
            // The platform joins repeats with ", " rather than keeping the last
            // one, and `set-cookie` aside that is what `Headers.get` returns.
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(value);
            }
            _ => {
                map.insert(key, Value::String(value.clone()));
            }
        }
    }

    serde_json::json!({
        "status": status,
        "url": url,
        "headers": Value::Object(map),
        "body": body,
    })
}

/// Issue one staged request and say what the journal should record.
///
/// ## An HTTP error is a completed step
///
/// A 500 is an *answer*. `fetch` on the web platform resolves it and the body
/// reads `res.ok`, so making it throw here would be an ALBEDO dialect — exactly
/// what § 5.5 forbids — and would take the author's own `if (!res.ok)` branch
/// away from them. Recording it is also what keeps a replay deterministic: the
/// upstream answered once, and a later pass must see the same answer or it has
/// diverged.
///
/// **This departs from § 10's "upstream 5xx on a write step → the body's `fetch`
/// throws" row**, which conflates *the call failed* with *the step failed*. The
/// row is wrong; the web semantics are kept.
///
/// ## A failure the body observes as a throw
///
/// Egress refusal, an unparseable URL, a transport error, a timeout, or a body
/// that is not text. Recorded as [`StepOutcome::Failed`] so a userland `catch`
/// sees a real error at the call site that caused it.
///
/// [`StepOutcome::Unknown`] is deliberately **not** produced here. It exists for
/// R4 — an indeterminate write retried under the same derived key — and a retry
/// policy is A3. Recording `Unknown` today would encode as a journal miss, so
/// the next pass would re-stage the same step and the append would be refused as
/// out-of-order: a confusing failure in place of a clear one, and not a single
/// upstream request different.
async fn resolve_one(client: &ApertureClient, request: &ApertureRequest) -> StepOutcome {
    match client.send_effect(request).await {
        Ok(response) => match String::from_utf8(response.body) {
            Ok(text) => StepOutcome::Completed(response_record(
                // No redirect is ever followed (`Policy::none()`), so the URL
                // that answered is the URL that was asked.
                &request.url,
                response.status,
                &response.headers,
                text,
            )),
            Err(_) => StepOutcome::Failed(format!(
                "aperture: {} {} answered with a body that is not text, and a workflow reads a \
                 response through text() or json()",
                request.method, request.url
            )),
        },
        Err(err) => StepOutcome::Failed(match err {
            // Named rather than folded into the generic arm: this is the one a
            // developer hits by accident, and "aperture: transport failure" for
            // a policy decision would send them looking at the network.
            ApertureError::Egress(denial) => format!("aperture: {denial}"),
            other => other.to_string(),
        }),
    }
}

/// Issue everything one pass asked for **concurrently**, then append the
/// outcomes in step order.
///
/// Concurrent because the requests in a single suspension are independent by
/// construction — the body staged them all before any of them ran. Appended in
/// order because the order *is* the keying: `Journal::append` refuses a gap, so
/// resolving out of order would be caught, and doing it right is cheaper than
/// explaining the error.
///
/// Note that a pass stages more than one request only when something put them
/// there before the body ran. A missed `fetch` throws, so three independent
/// calls written on three lines still cost three passes today — § 5.4's
/// correction, and R1.3's hoisting is what changes it.
///
/// # Errors
/// [`WorkflowError::Journal`] when an outcome does not land at the next index,
/// which means two passes disagree about where they are.
pub async fn resolve_pending(
    client: &ApertureClient,
    journal: &mut Journal,
    pending: &[PendingRequest],
) -> Result<(), WorkflowError> {
    let requests: Vec<ApertureRequest> = pending
        .iter()
        .map(|staged| wire_request(staged, journal))
        .collect();

    let outcomes = futures_util::future::join_all(
        requests
            .iter()
            .map(|request| resolve_one(client, request))
            .collect::<Vec<_>>(),
    )
    .await;

    for (staged, outcome) in pending.iter().zip(outcomes) {
        journal.append(staged.step, StepKind::Fetch, &staged.digest, outcome)?;
    }
    Ok(())
}

/// Run passes until the body completes, resolving what each one asks for.
///
/// `pass` is handed an owned [`Journal`] — the log as it stands — and returns
/// that pass's outcome plus whatever side channel the caller collected during it
/// (the server collects FORGE writes). **On a suspended pass that side channel
/// is dropped**, which is the commit rule the design already had: a body that
/// suspended halfway did not ask for its earlier appends to stand alone, and
/// `__albedo_effects` is rebuilt per pass so its slot writes are discarded on
/// the same terms.
///
/// `build_id` is compared against the journal's on every pass (§ 11 R8). Today a
/// dispatch holds one `CompiledProject` for all its passes, so the comparison
/// cannot fail — it is here because A3 persists the journal and resumes it in a
/// process that may be running different code, and the check belongs at the seam
/// that would be wrong without it rather than in the change that adds the risk.
///
/// # Errors
/// Any [`WorkflowError`]. In every case nothing has been committed, because only
/// a completing pass returns instructions at all.
pub async fn drive_workflow<T, F, Fut>(
    client: &ApertureClient,
    journal: &mut Journal,
    limits: &WorkflowLimits,
    build_id: &str,
    mut pass: F,
) -> Result<(Vec<Instruction>, T), WorkflowError>
where
    F: FnMut(Journal) -> Fut,
    Fut: Future<Output = Result<(ActionPass, T), WorkflowError>>,
{
    let started = Instant::now();
    let mut passes = 0usize;

    loop {
        if journal.build_id() != build_id {
            return Err(WorkflowError::BuildChanged {
                started: journal.build_id().to_string(),
                now: build_id.to_string(),
            });
        }
        if passes >= limits.max_passes {
            return Err(WorkflowError::PassCap {
                cap: limits.max_passes,
            });
        }
        passes += 1;

        // The engine is checked out for exactly this await and released before
        // the next one. Everything below happens with the pool free.
        let (outcome, side) = pass(journal.clone()).await?;

        let pending = match outcome {
            ActionPass::Completed(instructions) => return Ok((instructions, side)),
            ActionPass::Suspended {
                pending,
                journal_len,
            } => {
                if journal_len as usize != journal.len() {
                    return Err(WorkflowError::JournalDesync {
                        seeded: journal_len,
                        held: journal.len(),
                    });
                }
                if pending.is_empty() {
                    return Err(WorkflowError::NoProgress { pass: passes });
                }
                pending
            }
        };

        // Checked before going out again rather than before each pass: the
        // question a deadline answers is "may this still make another network
        // call", and a body that completes on its first pass should never
        // consult a clock at all.
        let elapsed = started.elapsed();
        if elapsed >= limits.deadline {
            return Err(WorkflowError::Deadline {
                after: limits.deadline,
                passes,
            });
        }

        resolve_pending(client, journal, &pending).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aperture::client::{CountingTransport, WireResponse};
    use crate::aperture::egress::{EgressMode, EgressPolicy};
    use crate::aperture::{ResponseCache, Transport, DEFAULT_RESPONSE_BUDGET};
    use std::sync::Arc;

    fn client(transport: Arc<dyn Transport>) -> ApertureClient {
        let policy = Arc::new(EgressPolicy::new(EgressMode::Dev));
        ApertureClient::new(
            transport,
            Arc::new(ResponseCache::new(DEFAULT_RESPONSE_BUDGET)),
            policy,
        )
    }

    fn ok_json(body: &str) -> WireResponse {
        WireResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            etag: None,
            last_modified: None,
            content_type: Some("application/json".to_string()),
        }
    }

    fn staged(step: u32, method: &str, url: &str) -> PendingRequest {
        PendingRequest {
            step,
            method: method.to_string(),
            url: url.to_string(),
            body: None,
            headers: Vec::new(),
            digest: format!("d{step}"),
        }
    }

    #[tokio::test]
    async fn a_resolved_step_lands_in_the_journal_as_the_body_will_read_it() {
        let transport = Arc::new(CountingTransport::always(ok_json(r#"{"state":"green"}"#)));
        let client = client(transport);
        let mut journal = Journal::new("w", "b");

        resolve_pending(&client, &mut journal, &[staged(0, "GET", "https://api.test/s")])
            .await
            .expect("resolves");

        let script = journal.to_script_value();
        let record = &script[0];
        assert_eq!(record["ok"], Value::Bool(true));
        assert_eq!(record["v"]["status"], 200);
        assert_eq!(record["v"]["body"], r#"{"state":"green"}"#);
        assert_eq!(record["v"]["url"], "https://api.test/s");
        assert_eq!(record["v"]["headers"]["content-type"], "application/json");
    }

    /// § 5.3 — and the reason it is worth a test rather than a comment: the key
    /// is derived from a position the author cannot see, so nothing else in the
    /// system would notice if it silently stopped being sent.
    #[tokio::test]
    async fn a_write_carries_a_derived_idempotency_key_and_a_read_does_not() {
        let transport = Arc::new(CountingTransport::always(ok_json("{}")));
        let client = client(transport.clone());
        let mut journal = Journal::new("w_abc", "b");

        resolve_pending(
            &client,
            &mut journal,
            &[
                staged(0, "POST", "https://api.test/charges"),
                staged(1, "GET", "https://api.test/me"),
            ],
        )
        .await
        .expect("resolves");

        let sent = transport.requests();
        let key_of = |request: &crate::aperture::client::WireRequest| {
            request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(IDEMPOTENCY_KEY_HEADER))
                .map(|(_, value)| value.clone())
        };
        let post = sent
            .iter()
            .find(|request| request.method == "POST")
            .expect("the POST went out");
        let get = sent
            .iter()
            .find(|request| request.method == "GET")
            .expect("the GET went out");

        assert_eq!(key_of(post).as_deref(), Some("w_abc:0"));
        assert_eq!(key_of(get), None, "a read has nothing to deduplicate");
    }

    #[tokio::test]
    async fn an_authored_key_is_left_alone() {
        let transport = Arc::new(CountingTransport::always(ok_json("{}")));
        let client = client(transport.clone());
        let mut journal = Journal::new("w_abc", "b");
        let mut request = staged(0, "POST", "https://api.test/charges");
        request
            .headers
            .push(("idempotency-key".to_string(), "mine".to_string()));

        resolve_pending(&client, &mut journal, &[request])
            .await
            .expect("resolves");

        let sent = transport.requests();
        let keys: Vec<&(String, String)> = sent[0]
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(IDEMPOTENCY_KEY_HEADER))
            .collect();
        assert_eq!(keys.len(), 1, "one key, not two");
        assert_eq!(keys[0].1, "mine");
    }

    /// The § 10 row this implementation deliberately departs from. Pinned so the
    /// departure is a decision someone can find rather than an accident.
    #[tokio::test]
    async fn an_http_error_is_a_completed_step_the_body_reads_as_not_ok() {
        let transport = Arc::new(CountingTransport::always(WireResponse {
            status: 503,
            body: b"upstream down".to_vec(),
            headers: Vec::new(),
            etag: None,
            last_modified: None,
            content_type: Some("text/plain".to_string()),
        }));
        let client = client(transport);
        let mut journal = Journal::new("w", "b");

        resolve_pending(&client, &mut journal, &[staged(0, "GET", "https://api.test/s")])
            .await
            .expect("resolves");

        let script = journal.to_script_value();
        assert_eq!(script[0]["ok"], Value::Bool(true), "the step completed");
        assert_eq!(script[0]["v"]["status"], 503, "and the body sees the status");
    }

    /// A refused host must not look like a network problem, and must not look
    /// like an empty response either.
    #[tokio::test]
    async fn an_egress_refusal_is_a_failed_step_the_body_can_catch() {
        let transport = Arc::new(CountingTransport::always(ok_json("{}")));
        let policy = Arc::new(EgressPolicy::new(EgressMode::Serve));
        let client = ApertureClient::new(
            transport.clone(),
            Arc::new(ResponseCache::new(DEFAULT_RESPONSE_BUDGET)),
            policy,
        );
        let mut journal = Journal::new("w", "b");

        resolve_pending(&client, &mut journal, &[staged(0, "GET", "ftp://api.test/s")])
            .await
            .expect("the step resolves — as a failure");

        let script = journal.to_script_value();
        assert_eq!(script[0]["ok"], Value::Bool(false));
        assert_eq!(transport.calls(), 0, "nothing reached the wire");
    }

    /// The property the whole design is for: a step already in the log is
    /// answered from it, and the upstream is not asked twice.
    #[tokio::test]
    async fn a_body_that_replays_its_step_costs_one_upstream_request() {
        let transport = Arc::new(CountingTransport::always(ok_json(r#"{"n":1}"#)));
        let client = client(transport.clone());
        let mut journal = Journal::new("w", "b");
        let mut seeded_lengths = Vec::new();

        let (instructions, side) = drive_workflow(
            &client,
            &mut journal,
            &WorkflowLimits::default(),
            "b",
            |seeded| {
                seeded_lengths.push(seeded.len());
                async move {
                    if seeded.is_empty() {
                        Ok((
                            ActionPass::Suspended {
                                pending: vec![staged(0, "GET", "https://api.test/s")],
                                journal_len: 0,
                            },
                            "discarded",
                        ))
                    } else {
                        Ok((ActionPass::Completed(Vec::new()), "kept"))
                    }
                }
            },
        )
        .await
        .expect("completes");

        assert_eq!(seeded_lengths, vec![0, 1], "one pass to ask, one to finish");
        assert_eq!(transport.calls(), 1);
        assert!(instructions.is_empty());
        assert_eq!(side, "kept", "the suspended pass's side channel is dropped");
    }

    #[tokio::test]
    async fn a_body_that_never_stops_asking_hits_the_pass_cap() {
        let transport = Arc::new(CountingTransport::always(ok_json("{}")));
        let client = client(transport.clone());
        let mut journal = Journal::new("w", "b");
        let limits = WorkflowLimits {
            max_passes: 3,
            ..WorkflowLimits::default()
        };

        let result = drive_workflow(&client, &mut journal, &limits, "b", |seeded| {
            let step = u32::try_from(seeded.len()).unwrap();
            async move {
                Ok::<_, WorkflowError>((
                    ActionPass::Suspended {
                        pending: vec![staged(step, "GET", "https://api.test/s")],
                        journal_len: step,
                    },
                    (),
                ))
            }
        })
        .await;

        assert_eq!(result.unwrap_err(), WorkflowError::PassCap { cap: 3 });
        assert_eq!(transport.calls(), 3, "and it stopped calling out");
    }

    /// Asserted with a zero deadline rather than by sleeping: the rule is
    /// "may this go out again", and a stopwatch in a test is a flake in waiting.
    #[tokio::test]
    async fn an_exhausted_deadline_stops_the_workflow_before_the_next_call() {
        let transport = Arc::new(CountingTransport::always(ok_json("{}")));
        let client = client(transport.clone());
        let mut journal = Journal::new("w", "b");
        let limits = WorkflowLimits {
            deadline: Duration::ZERO,
            ..WorkflowLimits::default()
        };

        let result = drive_workflow(&client, &mut journal, &limits, "b", |_| async {
            Ok::<_, WorkflowError>((
                ActionPass::Suspended {
                    pending: vec![staged(0, "GET", "https://api.test/s")],
                    journal_len: 0,
                },
                (),
            ))
        })
        .await;

        assert!(matches!(
            result.unwrap_err(),
            WorkflowError::Deadline { passes: 1, .. }
        ));
        assert_eq!(transport.calls(), 0, "the body ran; nothing went out");
    }

    #[tokio::test]
    async fn a_workflow_from_another_build_is_refused_before_it_runs() {
        let transport = Arc::new(CountingTransport::always(ok_json("{}")));
        let client = client(transport);
        let mut journal = Journal::new("w", "build-old");

        // Annotated because the closure never returns: with a diverging body
        // there is nothing for the side channel's type to be inferred from.
        let result: Result<(Vec<Instruction>, ()), WorkflowError> = drive_workflow(
            &client,
            &mut journal,
            &WorkflowLimits::default(),
            "build-new",
            |_| async { panic!("the body must not run") },
        )
        .await;

        assert_eq!(
            result.unwrap_err(),
            WorkflowError::BuildChanged {
                started: "build-old".to_string(),
                now: "build-new".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn a_suspension_that_stages_nothing_is_refused_rather_than_replayed() {
        let transport = Arc::new(CountingTransport::always(ok_json("{}")));
        let client = client(transport);
        let mut journal = Journal::new("w", "b");

        let result = drive_workflow(
            &client,
            &mut journal,
            &WorkflowLimits::default(),
            "b",
            |_| async {
                Ok::<_, WorkflowError>((
                    ActionPass::Suspended {
                        pending: Vec::new(),
                        journal_len: 0,
                    },
                    (),
                ))
            },
        )
        .await;

        assert_eq!(result.unwrap_err(), WorkflowError::NoProgress { pass: 1 });
    }
}
