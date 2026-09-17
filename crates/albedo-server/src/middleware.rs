//! MIDDLEWARE · 15.6 — the request half of `dom_render_compiler::middleware`.
//!
//! The compiler crate finds the file, compiles the matcher and validates what
//! the body returns. This module owns the parts that need a request: which
//! requests are in scope, what the body is shown, running it on the render
//! pool, and turning its decision into a response.
//!
//! ## Which requests
//!
//! **Everything an app serves** — pages, `public/` files, actions, form actions
//! and stored uploads — and **nothing the framework serves to itself**: the live
//! lanes, the dev and inspector endpoints, the client runtime scripts, the npm
//! chunks and the sign-in endpoints. Those are either long-lived streams (where a
//! header or a refusal means something different from on a page), or the
//! machinery a page needs in order to work at all, or a credential surface whose
//! rate limiting is ordered around being reached directly.
//!
//! Uploads are in scope on purpose: a content-addressed id is not a capability
//! (see `upload::store`), and a middleware is the one place an app can put an
//! authorization check in front of that route without a framework change.
//!
//! ## What the body is shown
//!
//! Method, path, query, headers and cookies — **minus the framework's own
//! session cookies**, and with no raw `cookie` header. A session token belongs to
//! Rust: packages load into the same realm the middleware runs in, and the
//! identity the token proves is handed over already resolved, as `user`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use dom_render_compiler::middleware::outcome::Headers;
use dom_render_compiler::middleware::{Matcher, Outcome};
use serde_json::{json, Map, Value};

use dom_render_compiler::middleware::outcome::FRAMEWORK_COOKIE_PREFIX;

use crate::engine_pool::QuickJsEnginePool;

/// A project's middleware, ready to run: what the build found, the modules it
/// loads, and the pool it loads them on.
pub(crate) struct MiddlewarePlan {
    entry: Arc<str>,
    matcher: Matcher,
    modules: Arc<Vec<(String, String)>>,
    pool: Arc<QuickJsEnginePool>,
}

impl MiddlewarePlan {
    pub(crate) fn new(
        entry: String,
        matcher: Matcher,
        modules: Vec<(String, String)>,
        pool: Arc<QuickJsEnginePool>,
    ) -> Self {
        Self {
            entry: Arc::from(entry),
            matcher,
            modules: Arc::new(modules),
            pool,
        }
    }

    /// Whether this request enters the middleware: in scope, and matched.
    pub(crate) fn applies_to(&self, path: &str) -> bool {
        in_scope(path) && self.matcher.matches(path)
    }

    /// Run the body to a decision and validate it.
    ///
    /// ## `fetch()`
    ///
    /// Each pass runs on a pooled engine and either completes or suspends with
    /// the calls it could not answer. A suspension releases the engine, charges
    /// every staged call to the caller's **outbound** bucket and its host's
    /// shared one, resolves them
    /// through APERTURE — egress policy, response cache and idempotency keys
    /// included — appends the outcomes to an in-memory journal, and runs the next
    /// pass. The same protocol an action uses, and the same `resolve_pending`.
    ///
    /// No durable ledger: a middleware decides a response and writes nothing, so
    /// there is no submit to answer from a log after a crash. The journal lives
    /// exactly as long as the request.
    ///
    /// # Errors
    /// [`RunError::Refused`] when a call is over the caller's outbound budget;
    /// otherwise [`RunError::Failed`], naming the entry.
    pub(crate) async fn run(
        &self,
        request_json: String,
        user_json: String,
        session_cookie: Option<&str>,
        timeout: Duration,
        outbound: Outbound<'_>,
    ) -> Result<Outcome, RunError> {
        let value = match tokio::time::timeout(
            timeout,
            self.drive(&request_json, &user_json, outbound),
        )
        .await
        {
            Err(_) => {
                return Err(RunError::Failed(format!(
                    "middleware {} did not finish within {} ms",
                    self.entry,
                    timeout.as_millis()
                )))
            }
            Ok(result) => result?,
        };

        let outcome = dom_render_compiler::middleware::outcome::decode(&value)
            .map_err(|message| RunError::Failed(format!("middleware {}: {message}", self.entry)))?;

        // The prefix is refused by `decode`; the configured session cookie name
        // is only known here. Overwriting it could not forge a session — the
        // token is a hash lookup — but it could sign everyone out, and no
        // middleware has a reason to touch it.
        if let (Some(name), Some(cookies)) = (session_cookie, outcome.headers().get("set-cookie")) {
            if cookies.iter().any(|cookie| cookie_name(cookie) == name) {
                return Err(RunError::Failed(format!(
                    "middleware {}: sets the session cookie `{name}`, which belongs to the framework",
                    self.entry
                )));
            }
        }
        Ok(outcome)
    }

    async fn drive(
        &self,
        request_json: &str,
        user_json: &str,
        outbound: Outbound<'_>,
    ) -> Result<Value, RunError> {
        use dom_render_compiler::aperture::{resolve_pending, Journal, DEFAULT_PASS_CAP};
        use dom_render_compiler::runtime::quickjs_engine::MiddlewareRun;

        let fail = |message: String| RunError::Failed(format!("middleware {}: {message}", self.entry));
        // Shared, not re-copied per pass: a middleware that calls out runs its
        // body once per round trip, and the request it sees does not change.
        let request_json: Arc<str> = Arc::from(request_json);
        let user_json: Arc<str> = Arc::from(user_json);
        // Not built until a pass stages a call. Almost every middleware never
        // does, and the journal's id is a random draw plus a format — work a
        // header-setting middleware would otherwise pay on every request.
        let mut journal: Option<Journal> = None;

        for _ in 0..DEFAULT_PASS_CAP {
            let modules = Arc::clone(&self.modules);
            let entry = Arc::clone(&self.entry);
            let request_json = Arc::clone(&request_json);
            let user_json = Arc::clone(&user_json);
            let journal_json = journal.as_ref().map(|j| j.to_script_value().to_string());

            let pass = self
                .pool
                .with_engine(move |engine| -> Result<MiddlewareRun, String> {
                    use dom_render_compiler::runtime::engine::RuntimeEngine;
                    for (specifier, code) in modules.iter() {
                        engine
                            .load_module(specifier, code)
                            .map_err(|err| err.to_string())?;
                    }
                    engine
                        .eval_middleware(
                            &entry,
                            &request_json,
                            &user_json,
                            journal_json.as_deref().unwrap_or("[]"),
                        )
                        .map_err(|err| err.to_string())
                })
                .await
                .map_err(|err| fail(err.to_string()))?
                .map_err(RunError::Failed)?;

            let pending = match pass {
                MiddlewareRun::Completed(value) => return Ok(value),
                MiddlewareRun::Suspended {
                    pending,
                    journal_len,
                } => {
                    let held = journal.as_ref().map_or(0, Journal::len);
                    if journal_len as usize != held {
                        return Err(fail(format!(
                            "a pass saw {journal_len} recorded fetch() answers but {held} exist"
                        )));
                    }
                    pending
                }
            };

            let Some(client) = outbound.client else {
                return Err(fail(
                    "called fetch(), but no APERTURE client is installed to send it".to_string(),
                ));
            };
            // Charged before anything leaves the process: an anonymous page view
            // that fans out to somebody else's API is the cheapest way to spend
            // an operator's quota, and a refusal after the fact saves nothing.
            for request in &pending {
                (outbound.admit)(&request.url).map_err(RunError::Refused)?;
            }
            // Random per request: this id only keys the `Idempotency-Key` a POST
            // carries, and a request that is retried by its client is a new decision.
            let journal = journal.get_or_insert_with(|| {
                Journal::new(format!("mw_{:016x}", rand::random::<u64>()), "middleware")
            });
            resolve_pending(client, journal, &pending, None)
                .await
                .map_err(|err| fail(err.to_string()))?;
        }

        Err(fail(format!(
            "made fetch() calls across more than {DEFAULT_PASS_CAP} passes"
        )))
    }
}

/// What a middleware's `fetch()` goes out through, and what it is charged to.
pub(crate) struct Outbound<'a> {
    /// `None` only on a server built without one; a `fetch()` then fails loudly.
    pub client: Option<&'a dom_render_compiler::aperture::ApertureClient>,
    /// Charge one outbound call to this URL — the caller's bucket and the
    /// host's shared one — or say why not.
    pub admit: &'a (dyn Fn(&str) -> Result<(), dom_render_compiler::shutter::Verdict> + Sync),
}

/// Why a middleware produced no decision.
pub(crate) enum RunError {
    /// It failed — threw, timed out, diverged, or returned something invalid.
    Failed(String),
    /// A `fetch()` was over the caller's outbound budget.
    Refused(dom_render_compiler::shutter::Verdict),
}

fn cookie_name(set_cookie: &str) -> &str {
    set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
        .map_or("", |(name, _)| name.trim())
}

/// App-served paths, plus the three framework routes an app's requests arrive
/// on. See the module docs for why the rest of `/_albedo/` is excluded.
fn in_scope(path: &str) -> bool {
    let decoded = dom_render_compiler::middleware::matcher::percent_decode_path(path);
    if decoded != "/_albedo" && !decoded.starts_with("/_albedo/") {
        return true;
    }
    decoded == "/_albedo/action"
        || decoded.starts_with("/_albedo/action/")
        || decoded.starts_with(crate::uploads::SERVE_PREFIX)
}

/// Whether a request path is a framework lane — the lanes a rewrite may not
/// start from, because their branch has already been chosen by the request line.
pub(crate) fn is_framework_path(path: &str) -> bool {
    let decoded = dom_render_compiler::middleware::matcher::percent_decode_path(path);
    decoded == "/_albedo" || decoded.starts_with("/_albedo/")
}

/// The object `middleware(request, …)` receives.
pub(crate) fn request_json(
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    session_cookie: Option<&str>,
) -> String {
    let is_framework_cookie = |name: &str| {
        name.starts_with(FRAMEWORK_COOKIE_PREFIX) || Some(name) == session_cookie
    };

    // Built straight into the JSON value the engine parses — no intermediate
    // map to copy from. Repeated headers are joined the way HTTP folds them.
    let mut header_map = Map::new();
    for (name, value) in headers {
        if name == header::COOKIE {
            continue;
        }
        let Ok(value) = value.to_str() else { continue };
        match header_map.get_mut(name.as_str()) {
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(value);
            }
            _ => {
                header_map.insert(name.as_str().to_string(), Value::String(value.to_string()));
            }
        }
    }

    let mut cookies = Map::new();
    for value in headers.get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else { continue };
        for pair in value.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if name.is_empty() || is_framework_cookie(name) {
                continue;
            }
            cookies
                .entry(name.to_string())
                .or_insert_with(|| Value::String(value.to_string()));
        }
    }

    let mut request = Map::with_capacity(5);
    request.insert("method".to_string(), Value::String(method.to_string()));
    request.insert("path".to_string(), Value::String(path.to_string()));
    request.insert(
        "query".to_string(),
        query.map_or(Value::Null, |q| Value::String(q.to_string())),
    );
    request.insert("headers".to_string(), Value::Object(header_map));
    request.insert("cookies".to_string(), Value::Object(cookies));
    Value::Object(request).to_string()
}

/// `user` as every render sees it: `{ id }`, or `null`.
pub(crate) fn user_json(identity: &crate::auth::Identity) -> String {
    match identity.principal() {
        Some(principal) => json!({ "id": principal.id.as_str() }).to_string(),
        None => "null".to_string(),
    }
}

/// A decision that ends the request, as a response.
pub(crate) fn terminal_response(outcome: &Outcome) -> Option<Response<Body>> {
    let mut response = match outcome {
        Outcome::Redirect {
            location, status, ..
        } => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() =
                StatusCode::from_u16(*status).unwrap_or(StatusCode::TEMPORARY_REDIRECT);
            // Validated as CR/LF-free by `outcome::decode`; a value the header
            // type still rejects is a server error, not a silently dropped
            // redirect.
            match HeaderValue::from_str(location) {
                Ok(value) => {
                    response.headers_mut().insert(header::LOCATION, value);
                }
                Err(_) => return Some(failure_response("middleware redirect location is not a valid header value")),
            }
            response
        }
        Outcome::Respond { status, body, .. } => {
            let mut response = Response::new(Body::from(body.clone()));
            *response.status_mut() =
                StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            response
        }
        Outcome::Next { .. } | Outcome::Rewrite { .. } => return None,
    };
    stamp(response.headers_mut(), outcome.headers());
    Some(response)
}

/// Apply a middleware's headers to a response the route produced.
///
/// **The middleware wins** over a header the route set, except `set-cookie`,
/// which is appended: a middleware that sets `cache-control` means it, and a
/// cookie set by a route must not be erased by an unrelated one.
pub(crate) fn stamp(target: &mut HeaderMap, headers: &Headers) {
    for (name, values) in headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let values: Vec<HeaderValue> = values
            .iter()
            .filter_map(|value| HeaderValue::from_str(value).ok())
            .collect();
        if name == header::SET_COOKIE {
            for value in values {
                target.append(name.clone(), value);
            }
        } else {
            target.remove(&name);
            for value in values {
                target.append(name.clone(), value);
            }
        }
    }
}

/// The response for a middleware that failed. Loud on the terminal — `tracing`
/// reaches nobody without `RUST_LOG`, which is how island SSR failed silently
/// three times — and generic on the wire.
pub(crate) fn failure_response(message: &str) -> Response<Body> {
    eprintln!("albedo: {message}");
    tracing::error!(target: "albedo.middleware", error = %message, "middleware failed");
    let mut response = Response::new(Body::from("middleware failed"));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framework_lanes_are_out_of_scope_and_app_requests_are_in() {
        for path in ["/", "/logo.png", "/admin/x", "/_albedo/action", "/_albedo/action/sign", "/_albedo/uploads/abc"] {
            assert!(in_scope(path), "{path}");
        }
        for path in [
            "/_albedo/phosphor",
            "/_albedo/patches",
            "/_albedo/runtime.js",
            "/_albedo/auth/password/login",
            "/_albedo/dev/events",
            "/%5Falbedo/phosphor",
        ] {
            assert!(!in_scope(path), "{path}");
        }
    }

    #[test]
    fn the_framework_session_cookies_never_reach_the_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static(
                "theme=dark; __Host-albedo_session=secret; __Host-albedo-session=tab; my_sess=also-secret",
            ),
        );
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let json: Value =
            serde_json::from_str(&request_json("GET", "/a", Some("q=1"), &headers, Some("my_sess")))
                .unwrap();
        assert_eq!(json["cookies"], json!({ "theme": "dark" }));
        assert!(json["headers"].get("cookie").is_none(), "{json}");
        assert_eq!(json["headers"]["x-forwarded-for"], "1.2.3.4");
        assert_eq!(json["query"], "q=1");
        assert!(!json.to_string().contains("secret"), "{json}");
    }

    #[test]
    fn stamped_headers_replace_the_routes_but_cookies_accumulate() {
        let mut target = HeaderMap::new();
        target.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        target.insert(header::SET_COOKIE, HeaderValue::from_static("route=1"));
        let mut headers = Headers::new();
        headers.insert("cache-control".into(), vec!["public, max-age=60".into()]);
        headers.insert("set-cookie".into(), vec!["mw=1".into()]);
        stamp(&mut target, &headers);
        assert_eq!(target[header::CACHE_CONTROL], "public, max-age=60");
        assert_eq!(target.get_all(header::SET_COOKIE).iter().count(), 2);
    }
}
