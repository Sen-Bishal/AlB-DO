//! A3 · the access log — one JSON line per request on stdout, with
//! `ALBEDO_ACCESS_LOG=json`.
//!
//! 🔴 Before: logs only with `RUST_LOG`, human-formatted, and no line per
//! request at all. Asked "what happened to request X", an operator had nothing
//! to search.
//!
//! - **One line per response head**, from the outermost layer, so a 429, a
//!   probe and a dispatch panic's 500 are logged like a 200.
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
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderValue, Request};
use axum::response::Response;

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
        // One write per line, under the lock, so lines from concurrent
        // requests never interleave mid-line.
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
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
}
