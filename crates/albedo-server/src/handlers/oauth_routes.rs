//! AUTH · 15.4 — the OAuth 2.0 sign-in endpoints.
//!
//! `GET /_albedo/auth/oauth/{provider}/start` ·
//! `GET /_albedo/auth/oauth/{provider}/callback`
//!
//! The thin half. Every decision this flow makes lives in
//! [`dom_render_compiler::auth::oauth`], which is pure and unit-tested; what is
//! here is the HTTP: two redirects, one cookie, and two outbound calls made
//! over APERTURE so they are subject to the same egress allowlist as every
//! other outbound call the app makes.
//!
//! ## Why these are `GET` when the password endpoints are `POST`
//!
//! `auth_routes` refuses `GET` because a credential must never travel in a URL.
//! Neither of these carries one: `start` carries nothing, and `callback`
//! carries an authorization code that is single-use, bound to our PKCE
//! verifier, and useless to anyone who cannot also present the client secret.
//! They are `GET` because they *have* to be — both are top-level browser
//! navigations performed by the authorization server, and a redirect cannot be
//! a `POST`.
//!
//! ## The pending-flow cookie
//!
//! One cookie holds the three things the callback needs and the URL must not
//! carry: the `state` to compare, the PKCE verifier to present, and where the
//! person was going before they were asked to sign in.
//!
//! - **`SameSite=Lax`, not `Strict`.** The callback is a cross-site top-level navigation from the
//!   provider. `Strict` withholds the cookie on exactly that, so the flow would fail on the last
//!   step with "there is no sign-in in progress" — for every user, always. `Lax` is sent on
//!   top-level `GET` navigations, which is this and nothing else.
//! - **`HttpOnly`**, so the verifier is not readable by script, and `Secure` with the `__Host-`
//!   prefix, so no subdomain can plant one. Same attributes as the session cookie
//!   ([`dom_render_compiler::auth::session::set_cookie_value`]) because divergence here would be a
//!   silent weakening nobody would notice.
//! - **Cleared on the callback, whatever the outcome.** A flow is single-use; leaving it live after
//!   a failure would let a `state` be replayed.
//!
//! ## What a failure tells the caller
//!
//! The provider's own `error` code, when it sent one, and otherwise a fixed
//! message. This is not the password path and there is no enumeration oracle to
//! protect — a person staring at "the provider refused" with no code has no way
//! to fix their own misconfigured client id, and that is the failure this
//! surface actually sees.

use crate::auth::{now_ms, AuthRuntime, Identity};
use crate::forms::ReturnPath;
use axum::body::Body;
use axum::http::{header, HeaderMap, Response, StatusCode};
use dom_render_compiler::aperture::{ApertureClient, ApertureRequest, CacheScope};
use dom_render_compiler::auth::declare::ResolvedProvider;
use dom_render_compiler::auth::oauth::{
    self, OAuthEndpoints, OAuthError, OutboundRequest, FLOW_TTL_SECONDS,
};
use dom_render_compiler::auth::store;
use dom_render_compiler::auth::ProviderKind;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// The cookie one in-flight sign-in lives in.
///
/// `__Host-` rather than a bare name: the prefix is a browser-enforced promise
/// that the cookie was set by this exact origin over HTTPS with `Path=/` and no
/// `Domain`, which is the one thing that stops a compromised sibling subdomain
/// from planting a `state` we would then happily match against.
pub const FLOW_COOKIE: &str = "__Host-albedo_oauth";

/// Which of the two legs a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRoute {
    /// Mint a flow and redirect to the provider.
    Start,
    /// Receive the code and open a session.
    Callback,
}

/// Match a request path, yielding the leg and the provider name.
///
/// Unlike `auth_routes::match_auth_route` this cannot be three literals — the
/// provider name is a path segment by design (`auth::declare` validates names
/// against the path-segment alphabet for exactly this reason). It stays as
/// tight as a literal match by splitting on `/` and comparing every other
/// segment for equality: there is no prefix test, no pattern, and no way for a
/// longer path to be swallowed, because the segment count is checked first.
#[must_use]
pub fn match_oauth_route(path: &str) -> Option<(OAuthRoute, &str)> {
    let rest = path.strip_prefix("/_albedo/auth/oauth/")?;
    let mut segments = rest.split('/');
    let provider = segments.next()?;
    let leg = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    // The same alphabet the declaration was validated against. A name that
    // could not have been declared cannot match a declared provider, so this is
    // belt-and-braces — but it is the cheap half of the pair, and it means a
    // path segment never reaches a map lookup or a log line unfiltered.
    if !dom_render_compiler::auth::is_valid_provider_name(provider) {
        return None;
    }
    match leg {
        "start" => Some((OAuthRoute::Start, provider)),
        "callback" => Some((OAuthRoute::Callback, provider)),
        _ => None,
    }
}

/// Discovered endpoints, cached for the life of the process.
///
/// OIDC discovery is one network call whose answer is a deployment constant.
/// Doing it at boot would make every app with a `google` provider fail to start
/// when Google is briefly unreachable, and doing it per sign-in would put a
/// second round trip in front of every login — so it happens once, lazily, on
/// the first sign-in that needs it.
///
/// A failed discovery is deliberately **not** cached: the next attempt tries
/// again, because the failure is far more likely to be transient than the
/// success is to be wrong.
#[derive(Debug, Default)]
pub struct OAuthRuntime {
    discovered: RwLock<BTreeMap<String, OAuthEndpoints>>,
}

impl OAuthRuntime {
    /// Resolve a provider's endpoints, discovering them if this is an issuer.
    ///
    /// # Errors
    /// Whatever [`OAuthEndpoints::resolve`] refuses, plus a discovery fetch
    /// that failed — reported as [`OAuthError::Malformed`] naming the transport
    /// error, because from here it is indistinguishable from a document we
    /// could not read.
    pub async fn endpoints_for(
        &self,
        provider: &ResolvedProvider,
        client: Option<&Arc<ApertureClient>>,
    ) -> Result<OAuthEndpoints, OAuthError> {
        let discovery_url = match OAuthEndpoints::resolve(provider) {
            Ok(endpoints) => return Ok(endpoints),
            Err(OAuthError::NeedsDiscovery { discovery_url, .. }) => discovery_url,
            Err(err) => return Err(err),
        };

        if let Some(cached) = self.discovered.read().await.get(&provider.name) {
            return Ok(cached.clone());
        }

        let response = fetch(client, &OutboundRequest {
            method: "GET",
            url: discovery_url,
            headers: vec![("accept".to_string(), "application/json".to_string())],
            body: None,
        })
        .await?;

        let endpoints = OAuthEndpoints::from_discovery_document(&response)?;
        self.discovered
            .write()
            .await
            .insert(provider.name.clone(), endpoints.clone());
        Ok(endpoints)
    }
}

/// Everything a leg needs from the request.
pub struct OAuthRequest<'a> {
    /// The lowered `auth` block plus the substrate.
    pub auth: &'a AuthRuntime,
    /// Discovery cache.
    pub oauth: &'a OAuthRuntime,
    /// The outbound client. `None` for an app whose boot built none, which
    /// after the `boot.rs` fix can only mean an app with no providers at all.
    pub aperture: Option<&'a Arc<ApertureClient>>,
    /// Who is already signed in, if anyone — read by the session rotation.
    pub identity: &'a Identity,
    /// The request's query string, undecoded.
    pub query: &'a str,
    /// The origin browsers reach this app on, for the redirect URI.
    pub origin: &'a str,
    /// Request headers, for the pending-flow cookie.
    pub headers: &'a HeaderMap,
}

/// Run one leg.
pub async fn run_oauth_route(
    route: OAuthRoute,
    provider_name: &str,
    request: OAuthRequest<'_>,
) -> Response<Body> {
    let Some(provider) = request.auth.registry().provider(provider_name) else {
        // Not `501`. The route genuinely does not exist for this app, and
        // "not implemented" would imply it might.
        return refusal(
            StatusCode::NOT_FOUND,
            "This app does not offer that sign-in method.",
            &ReturnPath::root(),
        );
    };
    if !matches!(provider.kind, ProviderKind::OAuth | ProviderKind::Oidc) {
        return refusal(
            StatusCode::NOT_FOUND,
            "This app does not offer that sign-in method.",
            &ReturnPath::root(),
        );
    }

    match route {
        OAuthRoute::Start => start(provider, &request).await,
        OAuthRoute::Callback => callback(provider, &request).await,
    }
}

/// `GET /_albedo/auth/oauth/{provider}/start`.
async fn start(provider: &ResolvedProvider, request: &OAuthRequest<'_>) -> Response<Body> {
    let back = return_path_from(request.query);

    let endpoints = match request
        .oauth
        .endpoints_for(provider, request.aperture)
        .await
    {
        Ok(endpoints) => endpoints,
        Err(err) => return provider_failure(&err, &back),
    };

    let redirect_uri = redirect_uri(request.origin, &provider.name);
    let flow = match oauth::begin(provider, &endpoints, &redirect_uri, |name| {
        std::env::var(name).ok()
    }) {
        Ok(flow) => flow,
        Err(err) => return provider_failure(&err, &back),
    };

    let pending = PendingFlow {
        state: flow.state,
        verifier: flow.verifier,
        back: back.as_str().to_string(),
    };

    let mut response = redirect_to(&flow.authorize_url);
    set_cookie(&mut response, &pending.set_cookie());
    response
}

/// `GET /_albedo/auth/oauth/{provider}/callback`.
async fn callback(provider: &ResolvedProvider, request: &OAuthRequest<'_>) -> Response<Body> {
    let params = query_pairs(request.query);
    let pending = PendingFlow::read(request.headers);
    // Whatever happens below, this flow is spent. Cleared here, once, so no
    // early return can leave a replayable `state` in the browser.
    let clear = PendingFlow::clear_cookie();

    let back = pending
        .as_ref()
        .and_then(|flow| ReturnPath::parse(&flow.back))
        .unwrap_or_else(ReturnPath::root);

    let finish = |response: Response<Body>| {
        let mut response = response;
        set_cookie(&mut response, &clear);
        response
    };

    // The provider's own refusal — a denied consent screen arrives here, and it
    // is an ordinary outcome rather than an error.
    if let Some(code) = params.get("error") {
        return finish(provider_failure(
            &OAuthError::Provider {
                code: code.clone(),
                description: params.get("error_description").cloned(),
            },
            &back,
        ));
    }

    let Some(pending) = pending else {
        return finish(provider_failure(&OAuthError::NoPendingFlow, &back));
    };
    let Some(code) = params.get("code") else {
        return finish(provider_failure(
            &OAuthError::Malformed {
                what: "callback",
                reason: "no `code`".to_string(),
            },
            &back,
        ));
    };
    if let Err(err) = oauth::verify_state(
        &pending.state,
        params.get("state").map_or("", String::as_str),
    ) {
        warn!(target: "albedo.auth", provider = %provider.name, "oauth callback failed state check");
        return finish(provider_failure(&err, &back));
    }

    let endpoints = match request
        .oauth
        .endpoints_for(provider, request.aperture)
        .await
    {
        Ok(endpoints) => endpoints,
        Err(err) => return finish(provider_failure(&err, &back)),
    };
    let redirect_uri = redirect_uri(request.origin, &provider.name);

    let exchange = match oauth::token_request(
        provider,
        &endpoints,
        code,
        &pending.verifier,
        &redirect_uri,
        |name| std::env::var(name).ok(),
    ) {
        Ok(exchange) => exchange,
        Err(err) => return finish(provider_failure(&err, &back)),
    };
    let token = match fetch(request.aperture, &exchange).await {
        Ok(body) => match oauth::parse_token_response(&body) {
            Ok(token) => token,
            Err(err) => return finish(provider_failure(&err, &back)),
        },
        Err(err) => return finish(provider_failure(&err, &back)),
    };

    let profile_read = match oauth::profile_request(&endpoints, &token) {
        Ok(read) => read,
        Err(err) => return finish(provider_failure(&err, &back)),
    };
    let document = match fetch(request.aperture, &profile_read).await {
        Ok(body) => body,
        Err(err) => return finish(provider_failure(&err, &back)),
    };
    let (subject, profile) = match oauth::profile_from_document(&provider.claim_map, &document) {
        Ok(pair) => pair,
        Err(err) => {
            // The one failure worth a full-fidelity log line: it names a claim
            // and the keys the document had, and the fix is a one-line
            // `claimMap` edit the operator cannot guess without both.
            warn!(target: "albedo.auth", provider = %provider.name, %err, "oauth profile did not map");
            return finish(provider_failure(&err, &back));
        }
    };

    // Where the id becomes ours. `(provider, subject)` is unique in the schema,
    // so a race between two first logins for one human resolves to one
    // principal by the constraint rather than by a lock.
    let principal = match store::upsert_principal(
        request.auth.substrate().as_ref(),
        &provider.name,
        &subject,
        &profile,
        now_ms(),
    )
    .await
    {
        Ok(principal) => principal,
        Err(err) => {
            warn!(target: "albedo.auth", %err, "oauth principal upsert failed");
            return finish(refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Signed in with the provider, but the account could not be opened.",
                &back,
            ));
        }
    };

    debug!(
        target: "albedo.auth",
        principal = %principal.id,
        provider = %provider.name,
        "oauth sign-in"
    );
    finish(open_session(request, &principal.id, &provider.name, &back).await)
}

/// Mint the session and set its cookie.
///
/// The same shape as `auth_routes::open_session`, and deliberately a second
/// implementation of nothing: it calls the identical two store functions, in
/// the identical order, with the identical rotation rule.
async fn open_session(
    request: &OAuthRequest<'_>,
    principal: &dom_render_compiler::auth::PrincipalId,
    provider: &str,
    back: &ReturnPath,
) -> Response<Body> {
    let substrate = request.auth.substrate().as_ref();
    let ttl = request.auth.ttl_ms();

    let minted = match request.identity.token() {
        Some(existing) => {
            store::rotate_session(substrate, existing, principal, provider, now_ms(), ttl).await
        }
        None => store::create_session(substrate, principal, provider, now_ms(), ttl).await,
    };

    let token = match minted {
        Ok(token) => token,
        Err(err) => {
            warn!(target: "albedo.auth", %err, "session creation failed");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Signed in, but the session could not be opened. Try again.",
                back,
            );
        }
    };

    let mut response = see_other(back);
    set_cookie(&mut response, &request.auth.set_cookie(&token));
    response
}

/// The three things the callback needs and the URL must not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFlow {
    /// The CSRF value minted at `start`.
    pub state: String,
    /// The PKCE verifier.
    pub verifier: String,
    /// Where the person was going.
    pub back: String,
}

impl PendingFlow {
    /// Encode as a cookie value.
    ///
    /// `state` and `verifier` are base64url alphabet by construction, so `.` is
    /// a separator neither of them can contain. `back` is percent-encoded
    /// because it is a path and can legitimately contain anything a path can.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{}.{}.{}",
            self.state,
            self.verifier,
            percent_encode(&self.back)
        )
    }

    /// Decode a cookie value, or `None` if it is not one of ours.
    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        let state = parts.next()?.to_string();
        let verifier = parts.next()?.to_string();
        let back = percent_decode(parts.next()?);
        // A fourth segment means this is not our encoding. Refusing rather than
        // ignoring the tail matters: `verify_state` is only sound if the value
        // it compares is the whole value we wrote.
        if parts.next().is_some() || state.is_empty() || verifier.is_empty() {
            return None;
        }
        Some(Self {
            state,
            verifier,
            back,
        })
    }

    /// Read it out of the request's `Cookie` header.
    #[must_use]
    pub fn read(headers: &HeaderMap) -> Option<Self> {
        let raw = dom_render_compiler::auth::read_cookie(
            headers
                .get(header::COOKIE)
                .and_then(|value| value.to_str().ok())?,
            FLOW_COOKIE,
        )?;
        Self::decode(raw)
    }

    /// The `Set-Cookie` that starts a flow.
    #[must_use]
    pub fn set_cookie(&self) -> String {
        format!(
            "{FLOW_COOKIE}={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={FLOW_TTL_SECONDS}",
            self.encode()
        )
    }

    /// The `Set-Cookie` that ends one. Attributes must match or the browser
    /// keeps the original.
    #[must_use]
    pub fn clear_cookie() -> String {
        format!("{FLOW_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
    }
}

/// Where the provider sends the browser back to.
///
/// Absolute, because the authorization server compares it byte-for-byte against
/// the one registered with the client — and against the one `start` sent. Built
/// from one function called by both legs for that reason.
fn redirect_uri(origin: &str, provider: &str) -> String {
    format!(
        "{}/_albedo/auth/oauth/{provider}/callback",
        origin.trim_end_matches('/')
    )
}

/// Perform one described call over APERTURE.
///
/// Every outbound byte of this flow goes through the same client, cache policy
/// and egress allowlist as an app's own `sources` reads. `CacheScope::App` with
/// a zero TTL on the `GET`s and `send_effect` for the `POST`: a token exchange
/// is single-use by definition, and a cached one would be a correctness bug
/// rather than an optimisation.
async fn fetch(
    client: Option<&Arc<ApertureClient>>,
    request: &OutboundRequest,
) -> Result<Vec<u8>, OAuthError> {
    let client = client.ok_or_else(|| OAuthError::Malformed {
        what: "outbound client",
        reason: "this app has no outbound HTTP client".to_string(),
    })?;

    let outbound = ApertureRequest {
        method: request.method.to_string(),
        url: request.url.clone(),
        scope: CacheScope::App,
        ttl: Duration::ZERO,
        headers: request.headers.clone(),
        body: request.body.clone(),
    };

    let response = client
        .send_effect(&outbound)
        .await
        .map_err(|err| OAuthError::Malformed {
            what: "provider response",
            reason: err.to_string(),
        })?;
    Ok(response.body)
}

/// Split a query string into owned pairs.
fn query_pairs(query: &str) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

/// The `next=` a sign-in link may carry, validated the same way a form's is.
fn return_path_from(query: &str) -> ReturnPath {
    query_pairs(query)
        .get("next")
        .and_then(|raw| ReturnPath::parse(raw))
        .unwrap_or_else(ReturnPath::root)
}

/// A `302` to the authorization server.
fn redirect_to(url: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, url)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// A `303` back into the app.
fn see_other(back: &ReturnPath) -> Response<Body> {
    crate::forms::see_other(back)
}

/// Append a `Set-Cookie`, dropping it rather than panicking if it will not
/// parse — the same reasoning as `auth_routes::set_cookie`.
fn set_cookie(response: &mut Response<Body>, value: &str) {
    match value.parse() {
        Ok(header) => {
            response.headers_mut().append(header::SET_COOKIE, header);
        }
        Err(_) => warn!(
            target: "albedo.auth",
            "could not build an oauth cookie header; the sign-in will not complete"
        ),
    }
}

/// Turn a flow error into a page.
///
/// The provider's own code is shown. See the module docs: there is no
/// enumeration oracle here to protect, and the operator debugging a wrong
/// client id has nothing else to go on.
fn provider_failure(err: &OAuthError, back: &ReturnPath) -> Response<Body> {
    let status = match err {
        OAuthError::NotAnOAuthProvider { .. } => StatusCode::NOT_FOUND,
        OAuthError::StateMismatch | OAuthError::NoPendingFlow => StatusCode::BAD_REQUEST,
        OAuthError::MissingCredential { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_GATEWAY,
    };
    refusal(status, &err.to_string(), back)
}

/// A refusal a browser can read. Shares `auth_routes`' shape deliberately.
fn refusal(status: StatusCode, message: &str, back: &ReturnPath) -> Response<Body> {
    let body = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>Sign-in</title></head>\
         <body><main><p>{}</p><p><a href=\"{}\">Back</a></p></main></body></html>",
        escape_html(message),
        escape_html(back.as_str()),
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::VARY, "Cookie")
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn percent_encode(raw: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("v", raw)
        .finish()
        .trim_start_matches("v=")
        .to_string()
}

fn percent_decode(raw: &str) -> String {
    url::form_urlencoded::parse(format!("v={raw}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_legs_match_and_carry_the_provider() {
        assert_eq!(
            match_oauth_route("/_albedo/auth/oauth/github/start"),
            Some((OAuthRoute::Start, "github"))
        );
        assert_eq!(
            match_oauth_route("/_albedo/auth/oauth/google/callback"),
            Some((OAuthRoute::Callback, "google"))
        );
    }

    /// The property the literal match in `auth_routes` gets for free and this
    /// one has to earn: no longer path is swallowed, and no traversal segment
    /// survives as a provider name.
    #[test]
    fn nothing_longer_or_stranger_matches() {
        for path in [
            "/_albedo/auth/oauth/github",
            "/_albedo/auth/oauth/github/start/extra",
            "/_albedo/auth/oauth//start",
            "/_albedo/auth/oauth/../../etc/start",
            "/_albedo/auth/oauth/git hub/start",
            "/_albedo/auth/oauth/github/token",
            "/_albedo/auth/password/login",
        ] {
            assert_eq!(match_oauth_route(path), None, "{path} must not match");
        }
    }

    #[test]
    fn a_pending_flow_round_trips_through_a_cookie_value() {
        let flow = PendingFlow {
            state: "s-tate".to_string(),
            verifier: "veri_fier".to_string(),
            back: "/dashboard?tab=1&q=a b".to_string(),
        };
        assert_eq!(PendingFlow::decode(&flow.encode()), Some(flow));
    }

    /// A tail we did not write means the value is not ours, and `verify_state`
    /// is only sound over the whole value we wrote.
    #[test]
    fn a_value_with_extra_segments_is_not_ours() {
        assert_eq!(PendingFlow::decode("a.b.c.d"), None);
        assert_eq!(PendingFlow::decode("a"), None);
        assert_eq!(PendingFlow::decode(".b.c"), None);
        assert_eq!(PendingFlow::decode("a..c"), None);
    }

    /// `Strict` would withhold this cookie on the provider's cross-site
    /// navigation back to us — the flow would then fail on its last step, for
    /// every user, with a message saying no sign-in was in progress.
    #[test]
    fn the_flow_cookie_survives_the_cross_site_navigation_that_carries_the_code() {
        let flow = PendingFlow {
            state: "s".to_string(),
            verifier: "v".to_string(),
            back: "/".to_string(),
        };
        let cookie = flow.set_cookie();
        assert!(cookie.contains("SameSite=Lax"), "{cookie}");
        assert!(!cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
        assert!(cookie.starts_with("__Host-"), "{cookie}");
        assert!(cookie.contains("Max-Age=600"), "{cookie}");
    }

    /// Attributes must match the set or the browser keeps the original, and a
    /// flow that survives its own callback is a replayable `state`.
    #[test]
    fn clearing_matches_the_attributes_it_was_set_with() {
        let set = PendingFlow {
            state: "s".to_string(),
            verifier: "v".to_string(),
            back: "/".to_string(),
        }
        .set_cookie();
        let clear = PendingFlow::clear_cookie();
        for attribute in ["Path=/", "HttpOnly", "Secure", "SameSite=Lax"] {
            assert!(set.contains(attribute) && clear.contains(attribute), "{attribute}");
        }
        assert!(clear.contains("Max-Age=0"));
    }

    #[test]
    fn the_redirect_uri_is_absolute_and_tolerates_a_trailing_slash() {
        assert_eq!(
            redirect_uri("https://app.example", "github"),
            "https://app.example/_albedo/auth/oauth/github/callback"
        );
        assert_eq!(
            redirect_uri("https://app.example/", "github"),
            "https://app.example/_albedo/auth/oauth/github/callback"
        );
    }

    /// `next=` is attacker-supplied and ends up in a `Location`. It goes
    /// through the same `ReturnPath` the no-JS form path uses, so an absolute
    /// URL is not a redirect target.
    #[test]
    fn an_off_site_next_is_not_honoured() {
        assert_eq!(return_path_from("next=/dashboard").as_str(), "/dashboard");
        assert_eq!(return_path_from("next=//evil.example").as_str(), "/");
        assert_eq!(return_path_from("next=https://evil.example").as_str(), "/");
        assert_eq!(return_path_from("").as_str(), "/");
    }
}
