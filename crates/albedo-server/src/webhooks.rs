//! Inbound webhooks — the request half. The declaration, the signature schemes
//! and the event-id rules are `dom_render_compiler::webhook`; this is the HTTP.
//!
//! Verify → parse → queue → `200`. Every refusal is a status the provider acts
//! on: `401` for a signature it will never make valid (a misconfigured secret
//! shows up in its dashboard as a run of 401s), `400` for a verified body that
//! is not an event, `413` for an oversized one, and `500`/`503` only when **we**
//! failed — the answers a provider retries, which is what they are for.
//!
//! Served ahead of middleware, as a framework lane is. A webhook authenticates
//! by its signature, and an app-wide middleware rule written for people — "send
//! anyone without a session to /sign-in" — would otherwise turn every delivery
//! into a redirect the provider cannot follow, silently.

use crate::jobs::JobHandle;
use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Response, StatusCode};
use dom_render_compiler::forge::DataSubstrate;
use dom_render_compiler::webhook::{Rejection, Webhook, MAX_BODY_BYTES};
use std::sync::Arc;
use tracing::{debug, warn};

/// Receive one delivery for `hook`.
pub(crate) async fn receive(
    hook: &Webhook,
    method: &str,
    headers: &HeaderMap,
    body: Body,
    substrate: Option<Arc<dyn DataSubstrate>>,
    jobs: Option<JobHandle>,
) -> Response<Body> {
    if method != "POST" {
        return answer(StatusCode::METHOD_NOT_ALLOWED, "webhooks accept POST only")
            .with_allow("POST");
    }
    // Bounded before the signature is checked: an unauthenticated caller can
    // make us buffer this much and no more.
    let body = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return answer(StatusCode::PAYLOAD_TOO_LARGE, "body exceeds the webhook limit"),
    };
    let header_value = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };

    let now_s = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
    )
    .unwrap_or(i64::MAX);
    if let Err(rejection) = hook.verify(header_value, &body, now_s) {
        // Logged by name and reason, never with the body: an unverified body is
        // attacker text, and a verified one may carry a customer's details.
        warn!(target: "albedo.webhook", webhook = %hook.name, %rejection, "delivery refused");
        return answer(StatusCode::UNAUTHORIZED, &rejection.to_string());
    }

    let event: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => return answer(StatusCode::BAD_REQUEST, &Rejection::NotJson.to_string()),
    };
    let event_id = match hook.event_id(header_value, &event) {
        Ok(id) => id,
        Err(rejection) => return answer(StatusCode::BAD_REQUEST, &rejection.to_string()),
    };

    let (Some(substrate), Some(jobs)) = (substrate, jobs) else {
        // Cannot queue, so must not say "received": `503` makes the provider
        // keep the event and try again, instead of believing it was handled.
        warn!(target: "albedo.webhook", webhook = %hook.name, "no job queue; refusing so the provider retries");
        return answer(StatusCode::SERVICE_UNAVAILABLE, "not ready to receive");
    };

    let intent = dom_render_compiler::jobs::EnqueueIntent {
        name: hook.job.clone(),
        args: event,
        id: Some(hook.queue_id(&event_id)),
        delay: None,
    };
    let now_ms = now_s.saturating_mul(1000);
    // Who the job runs FOR. A webhook is a third party speaking, not a person,
    // so by default nobody: `user === null`. A declared `principal` path names
    // the person the event concerns — an id the app put into the provider's
    // object at checkout — and the signature just verified is what makes that
    // name trustworthy. Only an existing principal: an id naming nobody runs
    // the job for nobody, rather than minting ownership for a stranger.
    //
    // 🔑 Running FOR them is the whole authorization: the job's writes pass the
    // same owner check an action's do, so a subscription row it appends is
    // theirs and can be nobody else's. There is still no identity that writes
    // everyone's rows.
    let principal = match hook.principal_of(&intent.args) {
        Some(id) => match dom_render_compiler::auth::store::load_principal(substrate.as_ref(), &id).await {
            Ok(Some(_)) => Some(id.as_str().to_string()),
            Ok(None) => {
                warn!(target: "albedo.webhook", webhook = %hook.name, "event names an unknown principal; the job runs for nobody");
                None
            }
            Err(err) => {
                warn!(target: "albedo.webhook", webhook = %hook.name, %err, "principal lookup failed");
                return answer(StatusCode::SERVICE_UNAVAILABLE, "not ready to receive");
            }
        },
        None => None,
    };
    match crate::jobs::write_enqueue(substrate.as_ref(), &jobs, &intent, principal, now_ms).await {
        Ok(()) => {
            jobs.wake();
            debug!(target: "albedo.webhook", webhook = %hook.name, event = %event_id, "delivery queued");
            answer(StatusCode::OK, "{\"received\":true}").json()
        }
        Err(err) => {
            warn!(target: "albedo.webhook", webhook = %hook.name, %err, "queueing a delivery failed");
            answer(StatusCode::INTERNAL_SERVER_ERROR, "could not queue the event")
        }
    }
}

/// A short plain-text answer; providers show it in their delivery logs.
fn answer(status: StatusCode, message: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(message.to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

trait ResponseExt {
    fn with_allow(self, methods: &str) -> Self;
    fn json(self) -> Self;
}

impl ResponseExt for Response<Body> {
    fn with_allow(mut self, methods: &str) -> Self {
        if let Ok(value) = methods.parse() {
            self.headers_mut().insert(header::ALLOW, value);
        }
        self
    }

    fn json(mut self) -> Self {
        self.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        self
    }
}
