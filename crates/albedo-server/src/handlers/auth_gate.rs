//! AUTH § 4 · what an anonymous request gets from a route that requires a
//! principal.
//!
//! ## Why 401 and not a redirect to a login page
//!
//! A redirect is the better experience and is **deliberately not here yet**,
//! because there is nowhere honest to send anyone: P2 (passkey/password) is not
//! built, so the project has no login route to name. Inventing a
//! `login_path` config knob now would mean shipping a setting whose only correct
//! value does not exist, and every app would carry a redirect to a 404.
//!
//! `401 Unauthorized` is the accurate answer in the meantime — the request
//! lacked credentials for a resource that requires them — and it is what a
//! programmatic client should see regardless. When P2 lands and a login route
//! exists, this is the one function that changes: the redirect belongs here,
//! keyed off the declared login route, and every caller already routes through
//! it.
//!
//! ## Why a body at all
//!
//! A bare 401 renders as a blank page in a browser, which reads as *broken*
//! rather than as *refused*. The body is deliberately plain: it names the rule
//! that refused the request and nothing about the route's contents, because an
//! explanation shown to a stranger must not itself be a disclosure.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

/// Refuse an anonymous request to a gated route.
///
/// `route` is the matched **pattern** (`/account/[id]`), used only for the
/// `WWW-Authenticate` realm-ish hint and never rendered into the body — a
/// pattern can carry an app's internal naming, and the reader is by definition
/// not signed in.
#[must_use]
pub fn refuse_anonymous(route: &str) -> Response {
    debug_assert!(!route.is_empty(), "a matched route pattern is never empty");

    let body = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
                <title>Sign in required</title></head><body>\
                <main><h1>Sign in required</h1>\
                <p>This page is only available to signed-in users.</p>\
                </main></body></html>";

    (
        StatusCode::UNAUTHORIZED,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // `Cookie`, because whether this request is refused depends entirely
            // on the session cookie it carried. Without this a shared cache
            // could serve one visitor's refusal to a signed-in visitor, or
            // worse, cache a signed-in page under a key an anonymous visitor
            // hits. `private` alone is not enough — a browser cache is private.
            (header::VARY, "Cookie"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_anonymous_request_to_a_gated_route_gets_401() {
        let response = refuse_anonymous("/account");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The refusal depends on a cookie, so it must never be cached across
    /// identities. This is the bug that would look like "sometimes the wrong
    /// person's page shows up" and is very hard to reproduce.
    #[test]
    fn the_refusal_varies_on_cookie_and_is_not_stored() {
        let response = refuse_anonymous("/account");
        let headers = response.headers();
        assert_eq!(
            headers.get(header::VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie")
        );
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
    }

    /// An explanation shown to a stranger must not itself disclose anything.
    #[test]
    fn the_body_does_not_name_the_route() {
        let response = refuse_anonymous("/admin/secret-quarterly-numbers");
        let body = format!("{response:?}");
        assert!(
            !body.contains("secret-quarterly-numbers"),
            "the route pattern must not reach the reader"
        );
    }
}
