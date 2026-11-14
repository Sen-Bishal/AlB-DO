//! A3 · the access log — one JSON line per request on stdout, with
//! `ALBEDO_ACCESS_LOG=json`.
//!
//! 🔴 Before: logs only with `RUST_LOG`, human-formatted, and no line per
//! request at all. Asked "what happened to request X", an operator had nothing
//! to search.
//!
//! - **One line per response**, from the outermost layer, so a 429, a
//!   probe and a dispatch panic's 500 are logged like a 200. Tier-B bodies
//!   finish their line at body end/drop so render errors are included; `ms`
//!   still measures time to the response head.
//! - **`x-request-id` on every response**, logged or not: the caller's own when
//!   it is a plausible id (a proxy's id threads straight through), otherwise
//!   minted. It is the one thing a user's bug report and a log line share, and
//!   the request carries it inward too, so a handler's own error log names it.
//! - **The path without its query.** Queries carry sign-in tokens
//!   (`/_albedo/auth/magic?token=…`), reset links and search terms; a log file
//!   is the wrong place for every one of them.
//! - **Who, without a second lookup.** The principal the dispatcher already
//!   resolved is noted on the way past ([`note_identity`]); an access log that
//!   doubled the session queries is a cost nobody would leave switched on.
//! - **The client behind a proxy** is the address SHUTTER rations — the same
//!   trust rule, so a forged `X-Forwarded-For` cannot write someone else's
//!   address into the log.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderValue, Request};
use axum::response::Response;
use futures_util::StreamExt;

/// The header the id rides in, both ways.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

tokio::task_local! {
    /// The principal resolved while dispatching this request, if any.
    static WHO: RefCell<Option<String>>;
}

/// Record who a request turned out to be, if a request is being logged.
/// A no-op outside [`scope`], which is every non-request caller.
pub fn note_identity(identity: &crate::auth::Identity) {
    if let Some(principal) = identity.principal() {
        let _ = WHO.try_with(|who| *who.borrow_mut() = Some(principal.id.to_string()));
    }
}

/// Run one request's dispatch with somewhere for [`note_identity`] to write.
pub async fn scope<F: std::future::Future>(dispatch: F) -> (F::Output, Option<String>) {
    WHO.scope(RefCell::new(None), async move {
        let response = dispatch.await;
        let who = WHO.with(|who| who.borrow_mut().take());
        (response, who)
    })
    .await
}

/// The principal [`scope`] found, carried out to the log on the response.
#[derive(Debug, Clone)]
pub struct Principal(pub String);

/// Shared with a streaming body: its render runs after dispatch returned.
#[derive(Clone, Default)]
pub(crate) struct RenderErrors(Arc<Mutex<Vec<String>>>);

impl RenderErrors {
    pub(crate) fn note(&self, message: &str) {
        let mut errors = self.0.lock().unwrap_or_else(|err| err.into_inner());
        if !errors.iter().any(|error| error == message) {
            errors.push(message.to_string());
        }
    }
}

/// Logs even if the client drops an unfinished body. Ordinary responses keep
/// their response-head log; Tier-B bodies wait so their errors can be included.
struct PendingLine {
    text: String,
    errors: Option<RenderErrors>,
}

impl Drop for PendingLine {
    fn drop(&mut self) {
        if let Some(errors) = &self.errors {
            let errors = errors.0.lock().unwrap_or_else(|err| err.into_inner());
            if !errors.is_empty() {
                self.text.truncate(self.text.len().saturating_sub(2));
                let _ = write!(
                    self.text,
                    ",\"render_error\":{}}}\n",
                    json(&errors.join("\n"))
                );
            }
        }
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(self.text.as_bytes());
    }
}

/// What the layer needs.
pub struct AccessLog {
    /// Write lines. The request id is set either way.
    pub enabled: bool,
    /// For the client address behind a trusted proxy.
    pub limiter: Arc<crate::shutter::Limiter>,
}

/// Is a caller's id one to keep? Short, and only characters that need no
/// escaping anywhere it ends up — a header, a JSON string, a terminal.
fn plausible(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn mint() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The outermost layer.
pub async fn layer(
    log: Arc<AccessLog>,
    mut request: Request<Body>,
    next: axum::middleware::Next,
) -> Response {
    let id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| plausible(value))
        .map_or_else(mint, str::to_string);
    let header = HeaderValue::from_str(&id).ok();
    if let Some(value) = &header {
        request.headers_mut().insert(REQUEST_ID_HEADER, value.clone());
    }

    let line = log.enabled.then(|| {
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|connect| connect.0.ip());
        let client: Option<IpAddr> =
            peer.map(|peer| log.limiter.client_addr(peer, request.headers()));
        (request.method().clone(), request.uri().path().to_string(), client)
    });
    if log.enabled {
        request.extensions_mut().insert(RenderErrors::default());
    }
    let started = Instant::now();
    let mut response = next.run(request).await;
    if let Some(value) = header {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }

    if let Some((method, path, client)) = line {
        let who = response.extensions().get::<Principal>().map(|who| who.0.as_str());
        let text = render(
            &id,
            method.as_str(),
            &path,
            response.status().as_u16(),
            started.elapsed().as_secs_f64() * 1000.0,
            client,
            who,
        );
        let errors = response.extensions().get::<RenderErrors>().cloned();
        let pending = PendingLine { text, errors };
        if pending.errors.is_some() {
            let (parts, body) = response.into_parts();
            let stream = async_stream::stream! {
                let _pending = pending;
                let mut body = body.into_data_stream();
                while let Some(chunk) = body.next().await {
                    yield chunk;
                }
            };
            response = Response::from_parts(parts, Body::from_stream(stream));
        }
        // Otherwise `pending` drops here, under one stdout lock per line.
    }
    response
}

fn render(
    id: &str,
    method: &str,
    path: &str,
    status: u16,
    ms: f64,
    client: Option<IpAddr>,
    who: Option<&str>,
) -> String {
    let mut line = String::with_capacity(160);
    let ts = chrono_free_now();
    let _ = write!(
        line,
        "{{\"ts\":\"{ts}\",\"id\":{},\"method\":{},\"path\":{},\"status\":{status},\"ms\":{ms:.2}",
        json(id),
        json(method),
        json(path),
    );
    if let Some(client) = client {
        let _ = write!(line, ",\"client\":\"{client}\"");
    }
    if let Some(who) = who {
        let _ = write!(line, ",\"principal\":{}", json(who));
    }
    line.push_str("}\n");
    line
}

fn json(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

/// RFC 3339 UTC with milliseconds, from the system clock.
fn chrono_free_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
    let (y, mo, d) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{:03}Z",
        now.subsec_millis()
    )
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_one_json_object_and_never_carries_the_query() {
        let line = render(
            "abc",
            "GET",
            "/_albedo/auth/magic",
            302,
            1.5,
            Some("203.0.113.4".parse().unwrap()),
            Some("u_1"),
        );
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(value["path"], "/_albedo/auth/magic");
        assert_eq!(value["status"], 302);
        assert_eq!(value["client"], "203.0.113.4");
        assert_eq!(value["principal"], "u_1");
        assert!(line.ends_with("}\n") && line.matches('\n').count() == 1);
    }

    #[test]
    fn only_a_plausible_caller_id_is_kept() {
        assert!(plausible("req-01HF:abc.def_1"));
        assert!(!plausible(""));
        assert!(!plausible("has space"));
        assert!(!plausible("line\nbreak"));
        assert!(!plausible(&"x".repeat(65)));
    }

    #[test]
    fn the_clock_is_rfc_3339() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_391), (2025, 10, 30));
        let now = chrono_free_now();
        assert_eq!(now.len(), "2026-10-30T13:45:01.123Z".len(), "{now}");
    }

    const BODY_LOG_CHILD: &str = "ALBEDO_TEST_ACCESS_BODY_CHILD";
    const LATE_ERROR: &str = "deferred \"render\" error\nsecond line";

    async fn diagnostic_body(request: Request<Body>) -> Response {
        let errors = request.extensions().get::<RenderErrors>().unwrap().clone();
        let healthy = request.uri().path() == "/healthy";
        let body_errors = errors.clone();
        let stream = async_stream::stream! {
            yield Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(b"shell"));
            if !healthy {
                body_errors.note(LATE_ERROR);
                body_errors.note(LATE_ERROR);
            }
            yield Ok(bytes::Bytes::from_static(b"tail"));
        };
        let mut response = Response::new(Body::from_stream(stream));
        response.extensions_mut().insert(errors);
        response
    }

    fn body_log_router() -> axum::Router {
        let log = Arc::new(AccessLog {
            enabled: true,
            limiter: Arc::new(crate::shutter::Limiter::with(
                dom_render_compiler::shutter::Shutter::new().unwrap(),
                crate::shutter::TrustedProxies::none(),
            )),
        });
        axum::Router::new()
            .fallback(diagnostic_body)
            .layer(axum::middleware::from_fn(move |request, next| {
                layer(log.clone(), request, next)
            }))
    }

    async fn body_log_response(router: &axum::Router, id: &str, path: &str) -> Response {
        use tower::ServiceExt;
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(REQUEST_ID_HEADER, id)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    // Stdout is process-wide. An exact-test child captures the real logger's
    // writes without redirecting a handle used by unrelated parallel tests.
    fn captured_body_lines(test_name: &str) -> Vec<serde_json::Value> {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--format", "terse"])
            .env(BODY_LOG_CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn deferred_body_error_is_logged_once_without_cross_request_bleed() {
        if std::env::var_os(BODY_LOG_CHILD).is_some() {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    let router = body_log_router();
                    let response = body_log_response(&router, "late", "/error?token=private").await;
                    let bytes = axum::body::to_bytes(response.into_body(), 1024)
                        .await
                        .unwrap();
                    assert_eq!(bytes.as_ref(), b"shelltail");
                    let healthy = body_log_response(&router, "healthy", "/healthy").await;
                    let bytes = axum::body::to_bytes(healthy.into_body(), 1024)
                        .await
                        .unwrap();
                    assert_eq!(bytes.as_ref(), b"shelltail");
                });
            return;
        }
        let lines = captured_body_lines(
            "access_log::tests::deferred_body_error_is_logged_once_without_cross_request_bleed",
        );
        assert_eq!(
            lines.len(),
            2,
            "one completed access line per request: {lines:?}"
        );
        assert_eq!(lines[0]["id"], "late");
        assert_eq!(lines[0]["status"], 200);
        assert_eq!(lines[0]["path"], "/error");
        assert_eq!(lines[0]["render_error"], LATE_ERROR);
        assert_eq!(lines[1]["id"], "healthy");
        assert!(lines[1].get("render_error").is_none(), "{lines:?}");
    }

    #[test]
    fn dropping_a_partly_consumed_body_logs_its_observed_render_error_once() {
        if std::env::var_os(BODY_LOG_CHILD).is_some() {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    let response = body_log_response(&body_log_router(), "dropped", "/error").await;
                    let mut stream = response.into_body().into_data_stream();
                    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"shell");
                    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"tail");
                    // Do not poll EOF: the consumer can leave after any chunk.
                    drop(stream);
                });
            return;
        }
        let lines = captured_body_lines(
            "access_log::tests::dropping_a_partly_consumed_body_logs_its_observed_render_error_once",
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["id"], "dropped");
        assert_eq!(lines[0]["render_error"], LATE_ERROR);
    }

    #[test]
    fn dropping_an_unpolled_body_still_logs_one_access_line() {
        if std::env::var_os(BODY_LOG_CHILD).is_some() {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    let response =
                        body_log_response(&body_log_router(), "unpolled", "/error").await;
                    drop(response);
                });
            return;
        }
        let lines = captured_body_lines(
            "access_log::tests::dropping_an_unpolled_body_still_logs_one_access_line",
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["id"], "unpolled");
        assert!(
            lines[0].get("render_error").is_none(),
            "an unrun render cannot fail"
        );
    }
}
