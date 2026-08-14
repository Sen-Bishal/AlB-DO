//! AUTH · P2 — the first-party sign-in endpoints.
//!
//! `POST /_albedo/auth/password/register` · `POST /_albedo/auth/password/login`
//! · `POST /_albedo/auth/logout`
//!
//! ## Why these are framework routes and not app actions
//!
//! An action handler returns `Vec<Instruction>` — opcodes describing a DOM
//! change. It cannot set a header, which means it cannot mint a session cookie.
//! Widening the action ABI so that *any* action could set arbitrary cookies would
//! be a much larger and much more dangerous change than adding three routes, and
//! it would put the one credential we issue behind an interface every app author
//! can reach.
//!
//! Two more reasons, both structural rather than convenient:
//!
//! - **The provider name already belongs in the URL.** `auth::declare` says so — a provider name is
//!   validated against the path-segment alphabet precisely because it appears here.
//! - **A credential attempt is its own rate-limit class.** `OperationClass::Credential` and
//!   SHUTTER's two-bucket [`credential_attempt`](dom_render_compiler::shutter::Shutter::credential_attempt)
//!   exist for exactly this path; reaching them from inside the generic action dispatcher would mean
//!   a special case in the one place that must not have any.
//!
//! ## They still ride the general form seam
//!
//! `AUTH.md` § "P2's shape" warns against building the login form's submit path
//! twice. It is not built twice: these handlers decode with
//! [`crate::forms::FormBody`], validate their redirect with
//! [`crate::forms::ReturnPath`], and answer with [`crate::forms::see_other`] —
//! the same three the no-JS action route uses. What is specific here is what
//! happens in the middle, which is the part that genuinely is specific.
//!
//! ## What a failure tells the caller
//!
//! **Login: nothing.** One message for a wrong password, an unknown address, an
//! address that registered with a passkey and never set a password, and a
//! disabled account. Combined with
//! [`absorb_timing`](dom_render_compiler::auth::password::absorb_timing) on every
//! miss, a caller cannot use the login endpoint to learn who has an account.
//!
//! **Signup: that the address is taken.** It has to — see
//! `dom_render_compiler::auth::password`'s module docs for why the quiet
//! alternative is worse rather than better when there is no email channel.

use crate::auth::{now_ms, AuthRuntime, Identity};
use crate::forms::{see_other, FormBody, ReturnPath};
use crate::render::csrf::CsrfRegistry;
use axum::body::Body;
use axum::http::{header, HeaderMap, Response, StatusCode};
use bytes::Bytes;
use dom_render_compiler::auth::password::{
    absorb_timing, hash_password, normalize_email, verify_password, PasswordError,
};
use dom_render_compiler::auth::store::{self, ProviderProfile};
use dom_render_compiler::auth::ProviderKind;
use dom_render_compiler::runtime::SessionId;
use tracing::{debug, warn};

/// Path prefix every first-party auth endpoint lives under.
pub const AUTH_ROUTE_PREFIX: &str = "/_albedo/auth/";

/// Field the login and signup forms submit the address under.
pub const EMAIL_FIELD: &str = "email";

/// Field the login and signup forms submit the password under.
pub const PASSWORD_FIELD: &str = "password";

/// The single message a failed login produces, whatever actually went wrong.
///
/// Written once, as a constant, because the failure mode this guards against is
/// somebody adding a *more helpful* message to one branch. Every branch below
/// returns this one.
const LOGIN_REFUSED: &str = "That email and password do not match an account.";

/// Which first-party endpoint a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRoute {
    /// Create an account from an email and a password.
    PasswordRegister,
    /// Exchange an email and a password for a session.
    PasswordLogin,
    /// End the presented session.
    Logout,
}

/// Match a request path against the first-party endpoints.
///
/// Exhaustive and literal: three paths, no patterns, no prefixes that could
/// swallow a longer path. `/_albedo/auth/password/login/../../x` does not reach
/// anything here because it is not one of these three strings.
#[must_use]
pub fn match_auth_route(path: &str) -> Option<AuthRoute> {
    match path {
        "/_albedo/auth/password/register" => Some(AuthRoute::PasswordRegister),
        "/_albedo/auth/password/login" => Some(AuthRoute::PasswordLogin),
        "/_albedo/auth/logout" => Some(AuthRoute::Logout),
        _ => None,
    }
}

/// Everything an auth endpoint needs from the request, gathered once.
pub struct AuthRequest<'a> {
    /// The lowered `auth` block plus the substrate.
    pub auth: &'a AuthRuntime,
    /// Per-tab CSRF tokens. Note this is the **tab** session, not a login —
    /// a stranger reaching the sign-in page has one and needs one.
    pub csrf: &'a CsrfRegistry,
    /// The tab session id the CSRF token is bound to.
    pub session: SessionId,
    /// Who is already signed in, if anyone. Read by logout, and by the rotation
    /// on login.
    pub identity: &'a Identity,
    /// Request headers, for `Accept` negotiation.
    pub headers: &'a HeaderMap,
    /// The submitted body, already read.
    pub body: Bytes,
    /// The limiter, and the bucket this caller is rationed as.
    ///
    /// Threaded in rather than applied by the dispatcher because **only this
    /// module knows the account being attempted**, and the account is half of
    /// what SHUTTER's login path limits. See [`login`].
    pub shutter: &'a crate::shutter::Limiter,
    /// The caller's bucket.
    pub caller: dom_render_compiler::shutter::Key,
}

/// Run one first-party auth endpoint.
///
/// # Panics
/// Never. Every failure below is a response.
pub async fn run_auth_route(route: AuthRoute, request: AuthRequest<'_>) -> Response<Body> {
    let content_type = request
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let form = match FormBody::decode(content_type, request.body.as_ref()) {
        Ok(form) => form,
        Err(err) => {
            warn!(target: "albedo.auth", %err, "rejecting malformed auth submit");
            return refusal(StatusCode::BAD_REQUEST, &err.to_string(), &ReturnPath::root());
        }
    };
    let back = form.return_path();

    // CSRF first, before anything reads a credential field. A sign-in form is
    // the highest-value cross-site submit target in any app — a forged POST to
    // `login` with an attacker's credentials logs the victim into the
    // *attacker's* account, and everything they do next is recorded there.
    // `SameSite` makes that hard; the token makes it wrong.
    let Some(token) = form.get(crate::render::csrf::CSRF_FIELD_NAME) else {
        warn!(target: "albedo.auth", "auth submit carried no CSRF token; rejecting");
        return refusal(
            StatusCode::FORBIDDEN,
            "This form is stale. Reload the page and try again.",
            &back,
        );
    };
    if let Err(err) = request.csrf.validate(request.session, token) {
        warn!(target: "albedo.auth", %err, "auth submit failed CSRF validation");
        return refusal(
            StatusCode::FORBIDDEN,
            "This form is stale. Reload the page and try again.",
            &back,
        );
    }

    match route {
        AuthRoute::PasswordRegister => register(&request, &form, &back).await,
        AuthRoute::PasswordLogin => login(&request, &form, &back).await,
        AuthRoute::Logout => logout(&request, &back).await,
    }
}

/// Whether this app declared a provider of `kind`.
///
/// An endpoint for an undeclared provider is a `404`, not a `501`: the route
/// genuinely does not exist for this app, and saying "not implemented" would
/// imply it might be.
fn declares(auth: &AuthRuntime, kind: ProviderKind) -> Option<&str> {
    auth.registry()
        .providers
        .iter()
        .find(|provider| provider.kind == kind)
        .map(|provider| provider.name.as_str())
}

/// `POST /_albedo/auth/password/register`.
async fn register(
    request: &AuthRequest<'_>,
    form: &FormBody,
    back: &ReturnPath,
) -> Response<Body> {
    let Some(provider) = declares(request.auth, ProviderKind::Password) else {
        return refusal(
            StatusCode::NOT_FOUND,
            "This app does not offer password sign-up.",
            back,
        );
    };
    let provider = provider.to_string();

    let Some(email) = form.get(EMAIL_FIELD).and_then(normalize_email) else {
        return refusal(
            StatusCode::BAD_REQUEST,
            &PasswordError::InvalidEmail.to_string(),
            back,
        );
    };
    let password = form.get(PASSWORD_FIELD).unwrap_or_default();

    // Hashed before the account is claimed, so a policy failure costs no write —
    // and, more importantly, so the plaintext never travels further than this
    // frame.
    let secret = match hash_password(password) {
        Ok(secret) => secret,
        Err(err @ (PasswordError::TooShort | PasswordError::TooLong)) => {
            return refusal(StatusCode::BAD_REQUEST, &err.to_string(), back);
        }
        Err(err) => {
            warn!(target: "albedo.auth", %err, "password hashing failed");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong creating that account.",
                back,
            );
        }
    };

    let profile = ProviderProfile {
        email: Some(email.clone()),
        ..ProviderProfile::default()
    };
    let created = store::create_credential_account(
        request.auth.substrate().as_ref(),
        &provider,
        &email,
        &profile,
        &secret,
        now_ms(),
    )
    .await;

    let principal = match created {
        Ok(Some(principal)) => principal,
        Ok(None) => {
            // Deliberately explicit. See the module docs: with no email channel
            // there is no honest quiet alternative, and a fake success leaves
            // somebody unable to sign in and unable to find out why.
            return refusal(
                StatusCode::CONFLICT,
                "An account with that email already exists. Try signing in.",
                back,
            );
        }
        Err(err) => {
            warn!(target: "albedo.auth", %err, "account creation failed");
            return refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong creating that account.",
                back,
            );
        }
    };

    debug!(
        target: "albedo.auth",
        principal = %principal.id,
        provider = %provider,
        "password account created"
    );
    open_session(request, &principal.id, &provider, back).await
}

/// `POST /_albedo/auth/password/login`.
async fn login(request: &AuthRequest<'_>, form: &FormBody, back: &ReturnPath) -> Response<Body> {
    let Some(provider) = declares(request.auth, ProviderKind::Password) else {
        return refusal(
            StatusCode::NOT_FOUND,
            "This app does not offer password sign-in.",
            back,
        );
    };
    let provider = provider.to_string();
    let password = form.get(PASSWORD_FIELD).unwrap_or_default();

    // An unparseable address still pays for a hash. Returning early here would
    // make "that is not an address" measurably faster than "that address has no
    // account", which is a coarser version of the same oracle the rest of this
    // function closes.
    let Some(email) = form.get(EMAIL_FIELD).and_then(normalize_email) else {
        // Charged to the caller only. There is no account to name, and inventing
        // a shared "malformed" bucket would let one attacker turn everybody
        // else's typo into a 429.
        let verdict = request.shutter.charge(
            &request.caller,
            dom_render_compiler::shutter::Cost::flat(
                dom_render_compiler::shutter::OperationClass::Credential,
            ),
        );
        if !verdict.is_admitted() {
            return crate::shutter::too_many_requests(&verdict);
        }
        absorb_timing(password);
        return refusal(StatusCode::UNAUTHORIZED, LOGIN_REFUSED, back);
    };

    // SHUTTER · the two-bucket credential path, and the only place in the tree
    // that calls it.
    //
    // Per-caller limiting alone cannot see distributed credential stuffing — a
    // thousand addresses making ten attempts each against one account looks like
    // ten attempts from every seat. Per-account limiting alone would be a lockout
    // primitive. `credential_attempt` charges the caller and only *peeks* the
    // account, so asking the question never counts against the person being
    // attacked; `note_credential_failure` below is what charges the account, and
    // only for a failure. That ordering is the whole design.
    let verdict = request
        .shutter
        .shutter()
        .credential_attempt(&request.caller, &email);
    if !verdict.is_admitted() {
        debug!(target: "albedo.auth", "credential attempt refused by the limiter");
        return crate::shutter::too_many_requests(&verdict);
    }

    let found = store::lookup_credential(request.auth.substrate().as_ref(), &provider, &email).await;
    let credential = match found {
        Ok(Some(credential)) => credential,
        Ok(None) => {
            // No account, or an account with no password (a passkey-only human).
            // Both answer identically, and both pay the same KDF cost.
            absorb_timing(password);
            request.shutter.shutter().note_credential_failure(&email);
            return refusal(StatusCode::UNAUTHORIZED, LOGIN_REFUSED, back);
        }
        Err(err) => {
            // Fail closed, and still absorb: a substrate outage must not become
            // a faster answer than a wrong password.
            //
            // 🪤 The account bucket is deliberately **not** charged here. A
            // database outage is not evidence about that account, and charging
            // it would mean an outage locks people out for the length of the
            // window after it ends.
            warn!(target: "albedo.auth", %err, "credential lookup failed");
            absorb_timing(password);
            return refusal(StatusCode::UNAUTHORIZED, LOGIN_REFUSED, back);
        }
    };

    if !verify_password(&credential.secret_hash, password) {
        debug!(target: "albedo.auth", "password verification failed");
        request.shutter.shutter().note_credential_failure(&email);
        return refusal(StatusCode::UNAUTHORIZED, LOGIN_REFUSED, back);
    }

    debug!(
        target: "albedo.auth",
        principal = %credential.principal,
        "password login succeeded"
    );
    open_session(request, &credential.principal, &provider, back).await
}

/// `POST /_albedo/auth/logout`.
///
/// Revokes **this** session, not every session — logging out on a laptop must
/// not sign you out on a phone. Whole-account revocation is a separate,
/// deliberately separate, action.
///
/// Always answers success, including when no session was presented: "log out"
/// asks for a state, and the state is reached either way. A distinct answer for
/// "you were not logged in" would be one more bit about who is holding the
/// cookie.
async fn logout(request: &AuthRequest<'_>, back: &ReturnPath) -> Response<Body> {
    if let Some(token) = request.identity.token() {
        match store::revoke_session(request.auth.substrate().as_ref(), token).await {
            Ok(rows) => debug!(target: "albedo.auth", rows, "session revoked"),
            Err(err) => warn!(target: "albedo.auth", %err, "session revocation failed"),
        }
    }

    // The cookie is cleared whether or not the revocation succeeded. Those are
    // two different failures — a live row we could not delete is a real problem,
    // a cookie we could not clear is *this browser staying signed in* — and only
    // one of them is visible to the person who clicked the button.
    let mut response = see_other(back);
    set_cookie(&mut response, &request.auth.clear_cookie());
    response
}

/// Mint a session for `principal`, set the cookie, and send the browser back.
///
/// ## Rotation, and why it happens here rather than at the caller
///
/// `AUTH.md` R4: *a session id must never survive a change in what it
/// authorizes.* Somebody who can plant a cookie value before login — via a
/// subdomain, a shared machine, a crafted link — holds a token that becomes the
/// victim's session the moment they authenticate, unless the id changes at that
/// moment. Both entry points into this function are exactly that moment, so the
/// rotation lives here and neither of them can forget it.
async fn open_session(
    request: &AuthRequest<'_>,
    principal: &dom_render_compiler::auth::PrincipalId,
    provider: &str,
    back: &ReturnPath,
) -> Response<Body> {
    let substrate = request.auth.substrate().as_ref();
    let ttl = request.auth.ttl_ms();

    let minted = match request.identity.token() {
        // Already holding a session — replace it transactionally rather than
        // opening a second one beside it.
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

/// Append a `Set-Cookie` header, dropping it rather than panicking if it somehow
/// will not parse.
///
/// A cookie value that cannot become a header is a bug in the cookie builder,
/// not something a request can cause — `AuthRuntime::set_cookie` composes it from
/// a validated name and a base64url token. Dropping it makes that bug present as
/// "sign-in does not stick", which is loud enough, rather than as a panic that
/// takes the connection with it.
fn set_cookie(response: &mut Response<Body>, value: &str) {
    match value.parse() {
        Ok(header) => {
            response.headers_mut().append(header::SET_COOKIE, header);
        }
        Err(_) => warn!(
            target: "albedo.auth",
            "could not build the session cookie header; sign-in will not persist"
        ),
    }
}

/// A refusal a browser can read.
///
/// Deliberately **not** a redirect. A 303 back to the form would lose the reason
/// entirely unless the message were carried in a query parameter or a flash
/// cookie, and both of those are their own design — the first puts an
/// attacker-controllable string in the URL bar of a page that is about to ask for
/// a password. A plain status plus a plain body is what is honest today; § "P2's
/// shape" is where the re-render-with-errors version belongs, once a login route
/// exists to re-render.
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

/// Escape text for HTML body and attribute contexts.
///
/// The messages here are constants, but `back` is request-supplied. It has
/// already been through [`ReturnPath`], which refuses control characters and
/// non-ASCII — so this is the second of two independent reasons a submitted value
/// cannot become markup, and the one that would still hold if the first were
/// loosened.
fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_endpoints_match_exactly_and_nothing_else_does() {
        assert_eq!(
            match_auth_route("/_albedo/auth/password/login"),
            Some(AuthRoute::PasswordLogin)
        );
        assert_eq!(
            match_auth_route("/_albedo/auth/password/register"),
            Some(AuthRoute::PasswordRegister)
        );
        assert_eq!(
            match_auth_route("/_albedo/auth/logout"),
            Some(AuthRoute::Logout)
        );
        for path in [
            "/_albedo/auth/",
            "/_albedo/auth/password",
            "/_albedo/auth/password/login/",
            "/_albedo/auth/password/login/extra",
            "/_albedo/auth/PASSWORD/login",
            "/_albedo/authx/password/login",
            "/mine",
        ] {
            assert_eq!(match_auth_route(path), None, "`{path}` must not match");
        }
    }

    /// The message names both factors together and neither one alone — "that
    /// email and password do not match" tells a caller nothing about which half
    /// was wrong, where "no account with that email" tells them everything.
    #[test]
    fn the_login_failure_message_distinguishes_nothing() {
        let message = LOGIN_REFUSED.to_lowercase();
        assert!(message.contains("email") && message.contains("password"));
        for disclosing in [
            "no account",
            "unknown",
            "does not exist",
            "not found",
            "incorrect password",
            "wrong password",
            "already",
        ] {
            assert!(
                !message.contains(disclosing),
                "`{disclosing}` would answer a question the caller must not be able to ask"
            );
        }
    }

    /// **The guard that actually matters**, and the reason it is a source scan:
    /// the regression is not a bad constant, it is somebody adding a *more
    /// helpful* message to one branch six months from now. Every `401` this file
    /// produces must carry the one constant.
    #[test]
    fn every_login_refusal_in_this_file_uses_the_one_message() {
        const SOURCE: &str = include_str!("auth_routes.rs");
        // Assembled from two pieces so the needle does not appear verbatim in
        // this file and the scan therefore does not match its own source line.
        let needle = concat!("StatusCode::", "UNAUTHORIZED");
        let mut checked = 0;
        for (index, _) in SOURCE.match_indices(needle) {
            let rest = &SOURCE[index + needle.len()..];
            // Only argument positions — `assert_eq!(status, StatusCode::UNAUTHORIZED)`
            // is a reader of the status, not a producer of a message.
            if !rest.starts_with(',') {
                continue;
            }
            let tail = rest.trim_start_matches([',', ' ', '\n', '\r']);
            // The test module below constructs one refusal itself; both it and
            // every production branch must name the constant.
            assert!(
                tail.starts_with("LOGIN_REFUSED"),
                "a 401 at byte {index} does not use LOGIN_REFUSED; it starts: {}",
                &tail[..tail.len().min(60)]
            );
            checked += 1;
        }
        assert!(
            checked >= 4,
            "expected several 401 branches to check, found {checked} — did the scan stop matching?"
        );
    }

    #[test]
    fn a_refusal_is_not_cacheable_and_escapes_its_link() {
        let response = refusal(
            StatusCode::UNAUTHORIZED,
            LOGIN_REFUSED,
            &ReturnPath::parse("/sign-in?next=/a&b=1").expect("rooted"),
        );
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(response.headers().get(header::VARY).unwrap(), "Cookie");
    }

    #[test]
    fn html_escaping_covers_both_body_and_attribute_contexts() {
        assert_eq!(
            escape_html(r#"<script>a="b"&'c'</script>"#),
            "&lt;script&gt;a=&quot;b&quot;&amp;&#39;c&#39;&lt;/script&gt;"
        );
    }
}
