//! AUTH § 4 · what an anonymous request gets from a route that requires a
//! principal.
//!
//! ## A declared login route gets a redirect; everything else still gets 401
//!
//! This module used to say a redirect was *deliberately not here yet*, because
//! P2 was unbuilt and there was nowhere honest to send anyone. **P2 landed, and
//! the note did not.** `auth.login` is declared, validated against the same
//! same-origin rule as a form's return path, and carried through lowering as
//! `AuthRegistry::login_path` — and until now **nothing read it**: a config
//! field with no consumer, which is the shape this tree keeps paying for.
//!
//! So: a **document navigation** to a gated route now redirects to the declared
//! login route. Everything else still gets `401`, which is the accurate answer
//! for a programmatic client and the only answer for an app that declared no
//! login route at all — absent is a legitimate choice, and the fallback is the
//! refusal this module already gave.
//!
//! ⚖️ **The destination rides a cookie, not the URL.** The note that stood
//! here said carrying the origin back "is a real feature and it starts at the
//! sign-in page" — true, and it was needed the moment a flow had to be
//! "buy → sign up → back to checkout". It does not start at the sign-in page
//! after all: four different flows complete a sign-in, and none of them can
//! rely on a page threading a `?next=` through its forms and links. So the gate
//! remembers the destination ([`NEXT_COOKIE`]) and `open_session` — where every
//! sign-in ends — consumes it. See [`refuse_anonymous_returning`].
//!
//! 🔑 **Fail closed.** The declared path is re-parsed through
//! [`ReturnPath`] before it reaches a `Location` header, even though
//! `auth::declare` validated it at boot. It is the one value here that becomes
//! a header, and a redirect target that is merely *probably* same-origin is an
//! open redirect; if it does not parse, the refusal falls back to `401`.
//!//! ## Why a body at all
//!
//! A bare 401 renders as a blank page in a browser, which reads as *broken*
//! rather than as *refused*. The body is deliberately plain: it names the rule
//! that refused the request and nothing about the route's contents, because an
//! explanation shown to a stranger must not itself be a disclosure.

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::forms::{see_other, ReturnPath};

/// Refuse an anonymous request to a gated route.
///
/// `route` is the matched **pattern** (`/account/[id]`), used only for the
/// `WWW-Authenticate` realm-ish hint and never rendered into the body — a
/// pattern can carry an app's internal naming, and the reader is by definition
/// not signed in.
/// Whether this request is a **document navigation** — a person pointing a
/// browser at the route — rather than a programmatic fetch.
///
/// `Sec-Fetch-Mode: navigate` is the precise signal and every current browser
/// sends it. `Accept: text/html` is the fallback for clients that do not, and
/// is why `curl -H 'Accept: text/html'` follows the same path a browser does
/// rather than a different one nobody tests.
///
/// 🪤 A bare `Accept: */*` (what `fetch()` defaults to) is deliberately NOT
/// a navigation: redirecting an XHR to an HTML page turns a clean 401 into a
/// 200 full of markup the caller will try to parse as data.
#[must_use]
pub fn is_document_navigation(headers: &HeaderMap) -> bool {
    if let Some(mode) = headers.get("sec-fetch-mode").and_then(|v| v.to_str().ok()) {
        return mode.eq_ignore_ascii_case("navigate");
    }
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

#[must_use]
pub fn refuse_anonymous(route: &str, login_path: Option<&str>, navigating: bool) -> Response {
    refuse_anonymous_returning(route, login_path, navigating, None)
}

/// Where a signed-out visitor was going when the gate sent them to sign in.
///
/// ## Why a cookie, not `?next=`
///
/// Sign-in completes in four places — a password form, a sign-up form, an OAuth
/// callback, a mailed link — and three of them never see the sign-in page's URL:
/// a form posts back its own path, an OAuth start link is written by the app,
/// a mailed link opens in a fresh tab. A query parameter would have to be
/// threaded through every one of them by every app. A cookie set here, where the
/// destination is known, and read in `open_session`, where every sign-in ends,
/// covers all four with nothing to author.
///
/// `__Host-`, `HttpOnly`, `SameSite=Lax` like the session cookie; ten minutes,
/// because it answers "where was I going *just now*"; and re-validated through
/// [`ReturnPath`] when read, so a planted value can only ever name a path on
/// this origin.
pub const NEXT_COOKIE: &str = "__Host-albedo_next";
const NEXT_TTL_SECONDS: u32 = 600;

/// The `Set-Cookie` that remembers `destination`.
fn remember_destination(destination: &ReturnPath) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(destination.as_str().as_bytes()).collect();
    format!("{NEXT_COOKIE}={encoded}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={NEXT_TTL_SECONDS}")
}

/// The `Set-Cookie` that forgets it — sent with every completed sign-in.
#[must_use]
pub fn forget_destination() -> String {
    format!("{NEXT_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// The remembered destination, if the request carries a valid one.
#[must_use]
pub fn remembered_destination(headers: &HeaderMap) -> Option<ReturnPath> {
    let raw = dom_render_compiler::auth::read_cookie(
        headers.get(header::COOKIE)?.to_str().ok()?,
        NEXT_COOKIE,
    )?;
    let decoded: String = url::form_urlencoded::parse(format!("v={raw}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())?;
    ReturnPath::parse(&decoded)
}

/// [`refuse_anonymous`], remembering `requested` (the path and query the
/// visitor asked for) so the sign-in they are sent to returns them there.
#[must_use]
pub fn refuse_anonymous_returning(
    route: &str,
    login_path: Option<&str>,
    navigating: bool,
    requested: Option<&str>,
) -> Response {
    debug_assert!(!route.is_empty(), "a matched route pattern is never empty");

    // A person, and somewhere honest to send them.
    if navigating {
        if let Some(target) = login_path.and_then(ReturnPath::parse) {
            let mut response = see_other(&target).into_response();
            if let Some(destination) = requested.and_then(ReturnPath::parse) {
                if let Ok(cookie) = remember_destination(&destination).parse() {
                    response.headers_mut().append(header::SET_COOKIE, cookie);
                }
            }
            return response;
        }
    }

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

    /// The redirect to sign in remembers where the visitor was going, and the
    /// remembered value reads back — as a path on this origin, only.
    #[test]
    fn the_sign_in_redirect_remembers_the_destination_and_only_a_local_one() {
        let response = refuse_anonymous_returning(
            "/account/upgrade",
            Some("/sign-in"),
            true,
            Some("/account/upgrade?plan=pro"),
        );
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(NEXT_COOKIE))
            .expect("the destination is remembered")
            .to_string();
        assert!(cookie.contains("HttpOnly") && cookie.contains("Secure"), "{cookie}");

        let value = cookie.split(';').next().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, value.parse().unwrap());
        assert_eq!(
            remembered_destination(&headers).map(|p| p.as_str().to_string()),
            Some("/account/upgrade?plan=pro".to_string())
        );

        // A planted cookie naming another origin reads as nothing.
        let mut planted = HeaderMap::new();
        planted.insert(header::COOKIE, format!("{NEXT_COOKIE}=%2F%2Fevil.example").parse().unwrap());
        assert!(remembered_destination(&planted).is_none());

        // A programmatic client gets the 401 and no cookie.
        let api = refuse_anonymous_returning("/account", Some("/sign-in"), false, Some("/account"));
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
        assert!(api.headers().get(header::SET_COOKIE).is_none());
    }

    #[test]
    fn an_anonymous_request_to_a_gated_route_gets_401() {
        let response = refuse_anonymous("/account", None, true);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// The refusal depends on a cookie, so it must never be cached across
    /// identities. This is the bug that would look like "sometimes the wrong
    /// person's page shows up" and is very hard to reproduce.
    #[test]
    fn the_refusal_varies_on_cookie_and_is_not_stored() {
        let response = refuse_anonymous("/account", None, true);
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
        let response = refuse_anonymous("/admin/secret-quarterly-numbers", None, true);
        let body = format!("{response:?}");
        assert!(
            !body.contains("secret-quarterly-numbers"),
            "the route pattern must not reach the reader"
        );
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                value.parse().expect("header value"),
            );
        }
        map
    }

    /// The finding this replaced: an app that declares `login: "/sign-in"` sent
    /// a person to a 401 dead-end with no link on it, while the scaffold's own
    /// config comment promised they would be "sent to `login` below".
    #[test]
    fn a_person_navigating_to_a_gated_route_is_sent_to_the_declared_login() {
        let response = refuse_anonymous("/posts/new", Some("/sign-in"), true);
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/sign-in")
        );
    }

    /// 🪤 A programmatic client must not be redirected into HTML it will try to
    /// parse as data. 401 stays the answer even when a login route exists.
    #[test]
    fn a_programmatic_client_still_gets_401_even_with_a_login_route() {
        let response = refuse_anonymous("/posts/new", Some("/sign-in"), false);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Declaring no login route is a legitimate choice, and its fallback is the
    /// refusal this module always gave.
    #[test]
    fn without_a_declared_login_route_a_navigation_still_gets_401() {
        let response = refuse_anonymous("/posts/new", None, true);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// 🔑 Fail closed. `auth::declare` validates the declared path at boot, but
    /// this is where it becomes a `Location` header, and a redirect target that
    /// is only *probably* same-origin is an open redirect. Each of these is a
    /// real bypass shape: absolute, protocol-relative, backslash-relative, and
    /// header injection.
    #[test]
    fn a_login_path_that_is_not_a_same_origin_path_falls_back_to_401() {
        for hostile in [
            "https://evil.example/login",
            "//evil.example/login",
            "/\\evil.example/login",
            "/sign-in
Set-Cookie: a=b",
            "sign-in",
        ] {
            let response = refuse_anonymous("/posts/new", Some(hostile), true);
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{hostile:?} must never reach a Location header"
            );
        }
    }

    #[test]
    fn a_browser_navigation_is_detected_by_sec_fetch_mode_or_accept() {
        assert!(is_document_navigation(&headers(&[(
            "sec-fetch-mode",
            "navigate"
        )])));
        assert!(is_document_navigation(&headers(&[(
            "accept",
            "text/html,application/xhtml+xml"
        )])));
        // `fetch()`'s default, and an explicit non-navigation, are not.
        assert!(!is_document_navigation(&headers(&[("accept", "*/*")])));
        assert!(!is_document_navigation(&headers(&[
            ("sec-fetch-mode", "cors"),
            ("accept", "text/html"),
        ])));
    }
}
