//! AUTH × EMAIL · 15.3 — the flows that mail a link.
//!
//! | endpoint | does |
//! |---|---|
//! | `POST /_albedo/auth/password/forgot` | mails a reset link, if the address has a password |
//! | `GET  /_albedo/auth/password/reset?token=` | the new-password form |
//! | `POST /_albedo/auth/password/reset` | sets it, ends every session, signs in |
//! | `POST /_albedo/auth/magiclink/start` | mails a sign-in link |
//! | `GET  /_albedo/auth/magiclink/signin?token=` | a one-button confirm page |
//! | `POST /_albedo/auth/magiclink/signin` | signs in, creating the account on first use |
//! | `GET  /_albedo/auth/email/verify?token=` | a one-button confirm page |
//! | `POST /_albedo/auth/email/verify` | marks the address proven, signs in |
//!
//! ## Why every link lands on a page with a button
//!
//! Corporate mail scanners (Outlook Safe Links, Mimecast, Proofpoint) **fetch
//! every link in an inbound email** before the recipient sees it. A link whose
//! `GET` spends the token is spent by the scanner, and the person who clicks it
//! a minute later is told it has expired — or worse, for a sign-in link, the
//! *scanner* ends up holding the session. So a `GET` only checks the token and
//! renders a form; the `POST` behind the button, carrying the page's CSRF
//! token, is what spends it. Scanners follow links; they do not submit forms.
//!
//! ## What the "mail me a link" endpoints tell the caller: nothing
//!
//! `forgot` and `magiclink/start` answer the same `303` whether the address has
//! an account, has none, or has had enough mail for the hour — and they answer
//! it **before** the lookup runs: the account read, token write and queue write
//! happen on a spawned task. A caller holding a stopwatch learns nothing,
//! because there is nothing on the response path that differs.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Response, StatusCode};
use dom_render_compiler::auth::email_token::{self, Purpose};
use dom_render_compiler::auth::password::{hash_password, normalize_email, PasswordError};
use dom_render_compiler::auth::session::SessionToken;
use dom_render_compiler::auth::store::{self, ProviderProfile};
use dom_render_compiler::auth::{PrincipalId, ProviderKind, ResolvedProvider};
use dom_render_compiler::email::{templates, AuthEmail, Mailbox};
use dom_render_compiler::forge::DataSubstrate;
use dom_render_compiler::runtime::SessionId;
use tracing::{debug, warn};

use crate::auth::{now_ms, AuthRuntime};
use crate::email::EmailRuntime;
use crate::forms::{see_other, FormBody, ReturnPath};
use crate::handlers::auth_routes::{open_session, refusal, AuthRequest, EMAIL_FIELD, PASSWORD_FIELD};
use crate::render::csrf::CsrfRegistry;

/// Field a mailed token is submitted under.
pub const TOKEN_FIELD: &str = "token";

/// Where to land once the emailed link has been used — distinct from the
/// form's return path, which is where the browser goes *now* ("check your
/// inbox"). Captured when the link is issued, because the link is opened in a
/// browser that never saw the form.
pub const NEXT_FIELD: &str = "next";

/// Where each emailed link points.
pub const RESET_PATH: &str = "/_albedo/auth/password/reset";
/// The magic-link landing page.
pub const MAGIC_SIGNIN_PATH: &str = "/_albedo/auth/magiclink/signin";
/// The confirm-your-email landing page.
pub const VERIFY_PATH: &str = "/_albedo/auth/email/verify";

/// One message for every dead link, whatever killed it.
const LINK_DEAD: &str =
    "This link has expired or has already been used. Request a new one and use the newest email.";

/// A `GET` landing page for an emailed link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailPage {
    /// Choose a new password.
    ResetForm,
    /// Confirm a magic-link sign-in.
    MagicSignIn,
    /// Confirm an address.
    Verify,
}

/// Match a `GET` path. Literal, like [`crate::handlers::auth_routes::match_auth_route`].
#[must_use]
pub fn match_email_page(path: &str) -> Option<EmailPage> {
    match path {
        RESET_PATH => Some(EmailPage::ResetForm),
        MAGIC_SIGNIN_PATH => Some(EmailPage::MagicSignIn),
        VERIFY_PATH => Some(EmailPage::Verify),
        _ => None,
    }
}

/// What a landing page needs.
pub struct PageRequest<'a> {
    /// The auth runtime.
    pub auth: &'a AuthRuntime,
    /// Per-tab CSRF tokens, to arm the page's form.
    pub csrf: &'a CsrfRegistry,
    /// The tab session, if the browser presented one.
    pub session: Option<SessionId>,
    /// The raw query string.
    pub query: &'a str,
    /// The mail runtime.
    pub email: Option<&'a EmailRuntime>,
}

/// Render a landing page.
pub async fn run_email_page(page: EmailPage, request: PageRequest<'_>) -> Response<Body> {
    let (kind, purpose) = match page {
        EmailPage::ResetForm => (ProviderKind::Password, Purpose::PasswordReset),
        EmailPage::MagicSignIn => (ProviderKind::MagicLink, Purpose::MagicLink),
        EmailPage::Verify => (ProviderKind::Password, Purpose::VerifyEmail),
    };
    let offered = provider(request.auth, kind).is_some_and(|p| offers(p, purpose));
    if !offered || request.email.is_none() {
        return refusal(StatusCode::NOT_FOUND, "This app does not offer that.", &ReturnPath::root());
    }

    let token = url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(key, _)| key == TOKEN_FIELD)
        .and_then(|(_, value)| SessionToken::from_presented(&value));
    let live = match &token {
        None => false,
        Some(token) => {
            match email_token::peek(request.auth.substrate().as_ref(), purpose, token, now_ms()).await
            {
                Ok(live) => live,
                Err(err) => {
                    warn!(target: "albedo.auth", %err, "email token lookup failed");
                    false
                }
            }
        }
    };
    let (Some(token), true) = (token, live) else {
        return landing(StatusCode::GONE, "Link expired", &format!("<p>{}</p>", html(LINK_DEAD)), None);
    };

    // The form's CSRF token is bound to the tab session. A browser arriving from
    // an email client often has none yet, so one is minted and set here.
    let (session, minted) = match request.session {
        Some(session) => (session, None),
        None => {
            let session = SessionId::random();
            (session, Some(crate::render::csrf::build_session_set_cookie(session)))
        }
    };
    let csrf = crate::render::csrf::csrf_hidden_input_html(&request.csrf.token_for(session));
    let hidden = format!(
        "{csrf}<input type=\"hidden\" name=\"{TOKEN_FIELD}\" value=\"{}\">",
        html(token.expose_for_cookie())
    );
    let app = request.email.map(EmailRuntime::app_name).unwrap_or_default();

    let (title, body) = match page {
        EmailPage::ResetForm => (
            "Choose a new password",
            format!(
                "<h1>Choose a new password</h1><form method=\"post\" action=\"{RESET_PATH}\">\
                 {hidden}<label for=\"p\">New password</label><input id=\"p\" type=\"password\" \
                 name=\"{PASSWORD_FIELD}\" autocomplete=\"new-password\" minlength=\"{}\" \
                 required autofocus><button type=\"submit\">Set password</button></form>\
                 <p class=\"note\">Every device signed in to this account will be signed out.</p>",
                dom_render_compiler::auth::MIN_PASSWORD_BYTES
            ),
        ),
        EmailPage::MagicSignIn => (
            "Sign in",
            format!(
                "<h1>Sign in to {}</h1><form method=\"post\" action=\"{MAGIC_SIGNIN_PATH}\">\
                 {hidden}<button type=\"submit\" autofocus>Continue</button></form>",
                html(&app)
            ),
        ),
        EmailPage::Verify => (
            "Confirm your email",
            format!(
                "<h1>Confirm your email</h1><form method=\"post\" action=\"{VERIFY_PATH}\">\
                 {hidden}<button type=\"submit\" autofocus>Confirm and continue</button></form>"
            ),
        ),
    };
    landing(StatusCode::OK, title, &body, minted)
}

/// `POST /_albedo/auth/password/forgot`.
pub(crate) async fn forgot(request: &AuthRequest<'_>, form: &FormBody, back: &ReturnPath) -> Response<Body> {
    let Some((provider, email_rt, jobs)) = mail_ready(request, ProviderKind::Password, Purpose::PasswordReset) else {
        return refusal(StatusCode::NOT_FOUND, "This app does not offer password reset.", back);
    };
    let Some(email) = form.get(EMAIL_FIELD).and_then(normalize_email) else {
        return refusal(StatusCode::BAD_REQUEST, &PasswordError::InvalidEmail.to_string(), back);
    };
    let db = Arc::clone(request.auth.substrate());
    let runtime = Arc::clone(&email_rt);
    runtime.spawn(async move {
        // Charged before the lookup, whether or not an account exists, so the
        // budget is not an oracle either.
        if !mail_admitted(db.as_ref(), &email).await {
            return;
        }
        let principal = match store::lookup_credential(db.as_ref(), &provider, &email).await {
            Ok(Some(credential)) => credential.principal,
            // No password — but a principal that PROVED this address (a
            // magic-link user) may set a first one through this same link.
            Ok(None) => match email_token::verified_principal(db.as_ref(), &email).await {
                Ok(Some(principal)) => principal,
                // Nobody: nothing mailed. Mailing "you have no account" to an
                // address on request would confirm that it is a person.
                Ok(None) => return,
                Err(err) => return warn!(target: "albedo.auth", %err, "reset lookup failed"),
            },
            Err(err) => return warn!(target: "albedo.auth", %err, "reset lookup failed"),
        };
        mail_link(
            db.as_ref(),
            &email_rt,
            &jobs,
            Link {
                kind: AuthEmail::PasswordReset,
                purpose: Purpose::PasswordReset,
                path: RESET_PATH,
                provider: &provider,
                principal: Some(&principal),
                email: &email,
                next: None,
            },
        )
        .await;
    });
    see_other(back)
}

/// `POST /_albedo/auth/password/set` — connect a password to the signed-in
/// account, by mailing it a link to choose one.
///
/// Account linking for the password provider. The OAuth providers link by a
/// completed provider sign-in; a password's identity is an **email address**,
/// and the only proof of an address is a link that reached it. So this mails the
/// same reset link `forgot` would, addressed to the account's own email, and the
/// existing reset path does the rest: `email_token::reset_password` creates the
/// `(password, email)` account and the credential for a principal that has none,
/// and refuses if that address already signs in to somebody else.
///
/// Why not "type a password here, now": the address on a GitHub profile may
/// never have been verified by anyone, and a password identity created on it
/// would let whoever holds the account claim `(password, that address)` —
/// blocking the address's real owner from ever signing up with it.
pub(crate) async fn set_password(request: &AuthRequest<'_>, back: &ReturnPath) -> Response<Body> {
    let Some((provider, email_rt, jobs)) =
        mail_ready(request, ProviderKind::Password, Purpose::PasswordReset)
    else {
        return refusal(
            StatusCode::NOT_FOUND,
            "This app does not offer setting a password by email.",
            back,
        );
    };
    let Some(principal) = request.identity.principal() else {
        return refusal(StatusCode::UNAUTHORIZED, "Sign in first.", back);
    };
    if principal.providers.iter().any(|connected| connected == &provider) {
        return refusal(
            StatusCode::CONFLICT,
            "This account already has a password. Use “forgot password” to change it.",
            back,
        );
    }
    let Some(email) = principal.email.as_deref().and_then(normalize_email) else {
        return refusal(
            StatusCode::CONFLICT,
            "This account has no email address to send the link to.",
            back,
        );
    };
    let db = Arc::clone(request.auth.substrate());
    let runtime = Arc::clone(&email_rt);
    let principal_id = principal.id.clone();
    runtime.spawn(async move {
        if !mail_admitted(db.as_ref(), &email).await {
            return;
        }
        mail_link(
            db.as_ref(),
            &email_rt,
            &jobs,
            Link {
                kind: AuthEmail::PasswordReset,
                purpose: Purpose::PasswordReset,
                path: RESET_PATH,
                provider: &provider,
                principal: Some(&principal_id),
                email: &email,
                next: None,
            },
        )
        .await;
    });
    see_other(back)
}

/// `POST /_albedo/auth/password/reset`.
pub(crate) async fn reset(request: &AuthRequest<'_>, form: &FormBody) -> Response<Body> {
    let root = ReturnPath::root();
    let Some(provider) = provider(request.auth, ProviderKind::Password).filter(|p| p.reset_by_email)
    else {
        return refusal(StatusCode::NOT_FOUND, "This app does not offer password reset.", &root);
    };
    let Some(token) = form.get(TOKEN_FIELD).and_then(SessionToken::from_presented) else {
        return refusal(StatusCode::GONE, LINK_DEAD, &root);
    };
    // Policy before the token is spent: a too-short password must not cost the
    // person their link.
    let secret = match hash_password(form.get(PASSWORD_FIELD).unwrap_or_default()) {
        Ok(secret) => secret,
        Err(err @ (PasswordError::TooShort | PasswordError::TooLong)) => {
            let again = ReturnPath::parse(&format!(
                "{RESET_PATH}?{TOKEN_FIELD}={}",
                token.expose_for_cookie()
            ))
            .unwrap_or_else(ReturnPath::root);
            return refusal(StatusCode::BAD_REQUEST, &err.to_string(), &again);
        }
        Err(err) => {
            warn!(target: "albedo.auth", %err, "password hashing failed");
            return refusal(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.", &root);
        }
    };

    let db = request.auth.substrate().as_ref();
    let Some((principal, redeemed)) = redeem(db, Purpose::PasswordReset, &token, &provider.name).await else {
        return refusal(StatusCode::GONE, LINK_DEAD, &root);
    };
    match email_token::reset_password(db, &principal, &provider.name, &redeemed.email, &secret, now_ms())
        .await
    {
        Ok(true) => {}
        Ok(false) => return refusal(StatusCode::GONE, LINK_DEAD, &root),
        Err(err) => {
            warn!(target: "albedo.auth", %err, "password reset failed");
            return refusal(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.", &root);
        }
    }
    // A reset ended every session of the account; its open lanes go with them.
    crate::handlers::auth_routes::end_lanes_of(request, &principal);
    // Clicking the link proved the mailbox.
    note_verified(db, &principal, &redeemed.email).await;
    debug!(target: "albedo.auth", principal = %principal, "password reset");
    open_session(request, &principal, &provider.name, &next_of(&redeemed)).await
}

/// `POST /_albedo/auth/magiclink/start`.
pub(crate) async fn magic_start(request: &AuthRequest<'_>, form: &FormBody, back: &ReturnPath) -> Response<Body> {
    let Some((provider, email_rt, jobs)) = mail_ready(request, ProviderKind::MagicLink, Purpose::MagicLink) else {
        return refusal(StatusCode::NOT_FOUND, "This app does not offer email sign-in.", back);
    };
    let Some(email) = form.get(EMAIL_FIELD).and_then(normalize_email) else {
        return refusal(StatusCode::BAD_REQUEST, &PasswordError::InvalidEmail.to_string(), back);
    };
    let next = form.get(NEXT_FIELD).and_then(ReturnPath::parse);
    let db = Arc::clone(request.auth.substrate());
    let runtime = Arc::clone(&email_rt);
    runtime.spawn(async move {
        if !mail_admitted(db.as_ref(), &email).await {
            return;
        }
        mail_link(
            db.as_ref(),
            &email_rt,
            &jobs,
            Link {
                kind: AuthEmail::MagicLink,
                purpose: Purpose::MagicLink,
                path: MAGIC_SIGNIN_PATH,
                provider: &provider,
                principal: None,
                email: &email,
                next: next.as_ref().map(ReturnPath::as_str),
            },
        )
        .await;
    });
    see_other(back)
}

/// `POST /_albedo/auth/magiclink/signin`.
pub(crate) async fn magic_signin(request: &AuthRequest<'_>, form: &FormBody) -> Response<Body> {
    let root = ReturnPath::root();
    let Some(provider) = provider(request.auth, ProviderKind::MagicLink) else {
        return refusal(StatusCode::NOT_FOUND, "This app does not offer email sign-in.", &root);
    };
    let Some(token) = form.get(TOKEN_FIELD).and_then(SessionToken::from_presented) else {
        return refusal(StatusCode::GONE, LINK_DEAD, &root);
    };
    let db = request.auth.substrate().as_ref();
    let redeemed = match email_token::consume(db, Purpose::MagicLink, &token, now_ms()).await {
        Ok(Some(redeemed)) if redeemed.provider == provider.name => redeemed,
        Ok(_) => return refusal(StatusCode::GONE, LINK_DEAD, &root),
        Err(err) => {
            warn!(target: "albedo.auth", %err, "magic link redemption failed");
            return refusal(StatusCode::GONE, LINK_DEAD, &root);
        }
    };
    // Sign-in, sign-up or link, one call — see `resolve_magic_link` for the
    // order, and for why an unproven password on this address is removed.
    let password = self::provider(request.auth, ProviderKind::Password).map(|p| p.name.clone());
    let linked = match email_token::resolve_magic_link(
        db,
        &provider.name,
        password.as_deref(),
        &redeemed.email,
        now_ms(),
    )
    .await
    {
        Ok(linked) => linked,
        Err(err) => {
            warn!(target: "albedo.auth", %err, "magic link account resolution failed");
            return refusal(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong. Try again.", &root);
        }
    };
    if linked.took_over {
        crate::handlers::auth_routes::end_lanes_of(request, &linked.principal);
        warn!(
            target: "albedo.auth",
            principal = %linked.principal,
            "a magic link claimed an address whose password was never proven; that password \
             was removed and its sessions ended"
        );
    }
    let principal = linked.principal;
    note_verified(db, &principal, &redeemed.email).await;
    open_session(request, &principal, &provider.name, &next_of(&redeemed)).await
}

/// `POST /_albedo/auth/email/verify`.
pub(crate) async fn verify(request: &AuthRequest<'_>, form: &FormBody) -> Response<Body> {
    let root = ReturnPath::root();
    let Some(provider) = provider(request.auth, ProviderKind::Password).filter(|p| p.verify_email)
    else {
        return refusal(StatusCode::NOT_FOUND, "This app does not confirm email addresses.", &root);
    };
    let Some(token) = form.get(TOKEN_FIELD).and_then(SessionToken::from_presented) else {
        return refusal(StatusCode::GONE, LINK_DEAD, &root);
    };
    let db = request.auth.substrate().as_ref();
    let Some((principal, redeemed)) = redeem(db, Purpose::VerifyEmail, &token, &provider.name).await else {
        return refusal(StatusCode::GONE, LINK_DEAD, &root);
    };
    note_verified(db, &principal, &redeemed.email).await;
    open_session(request, &principal, &provider.name, &next_of(&redeemed)).await
}

/// `register` when `verify: "required"`: the whole outcome — create, or find
/// the address taken — happens after the response, so a new address and a
/// taken one answer identically in status, headers **and time**.
///
/// The hash is computed before this is called, on both paths, so the one
/// expensive step is on the response path for everyone.
pub(crate) fn register_quietly(
    request: &AuthRequest<'_>,
    provider: &ResolvedProvider,
    email: String,
    secret: String,
    form: &FormBody,
    back: &ReturnPath,
) -> Response<Body> {
    let (Some(email_rt), Some(jobs)) = (request.email.clone(), request.jobs.clone()) else {
        // Boot refuses `verify` without an `email` block and a runner.
        return refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong creating that account.",
            back,
        );
    };
    let db = Arc::clone(request.auth.substrate());
    let provider = provider.name.clone();
    let next = form
        .get(NEXT_FIELD)
        .and_then(ReturnPath::parse)
        .unwrap_or_else(ReturnPath::root)
        .as_str()
        .to_string();
    let login = request
        .auth
        .registry()
        .login_path
        .clone()
        .unwrap_or_else(|| "/".to_string());
    let runtime = Arc::clone(&email_rt);
    runtime.spawn(async move {
        let now = now_ms();
        // An address someone has PROVED is theirs, whichever way they signed up.
        let created = match email_token::verified_principal(db.as_ref(), &email).await {
            Ok(Some(_)) => None,
            Ok(None) => {
                let profile = ProviderProfile {
                    email: Some(email.clone()),
                    ..ProviderProfile::default()
                };
                match store::create_credential_account(
                    db.as_ref(),
                    &provider,
                    &email,
                    &profile,
                    &secret,
                    now,
                )
                .await
                {
                    Ok(created) => created.map(|principal| principal.id),
                    Err(err) => return warn!(target: "albedo.auth", %err, "account creation failed"),
                }
            }
            Err(err) => return warn!(target: "albedo.auth", %err, "account lookup failed"),
        };
        // The budget protects the inbox; the account is created regardless, and
        // signing in later sends a fresh link.
        if !mail_admitted(db.as_ref(), &email).await {
            return;
        }
        match created {
            Some(principal) => {
                mail_link(
                    db.as_ref(),
                    &email_rt,
                    &jobs,
                    Link {
                        kind: AuthEmail::VerifyEmail,
                        purpose: Purpose::VerifyEmail,
                        path: VERIFY_PATH,
                        provider: &provider,
                        principal: Some(&principal),
                        email: &email,
                        next: Some(&next),
                    },
                )
                .await;
            }
            // Taken: tell the owner, not the requester. No token — the link
            // goes to the sign-in page, which grants nothing.
            None => {
                let Some(link) = email_rt.link(&login) else {
                    return;
                };
                let kind = AuthEmail::AlreadyRegistered;
                let message = templates::render(
                    kind,
                    Mailbox {
                        name: None,
                        address: email.clone(),
                    },
                    &email_rt.app_name(),
                    &link,
                    0,
                    email_rt.config().templates.get(kind),
                );
                if let Err(err) = crate::jobs::queue_email(db.as_ref(), &jobs, message, now).await {
                    warn!(target: "albedo.auth", %err, "could not queue a sign-up notice");
                }
            }
        }
    });
    see_other(back)
}

/// Called by `login` when the password was right but the address is not yet
/// proven: refuse, and send a fresh link (budget permitting).
pub(crate) fn unverified_login(
    request: &AuthRequest<'_>,
    provider: &ResolvedProvider,
    principal: PrincipalId,
    email: String,
    back: &ReturnPath,
) -> Response<Body> {
    if let (Some(email_rt), Some(jobs)) = (request.email.clone(), request.jobs.clone()) {
        let db = Arc::clone(request.auth.substrate());
        let provider = provider.name.clone();
        // Where the login would have landed.
        let next = back.as_str().to_string();
        let runtime = Arc::clone(&email_rt);
        runtime.spawn(async move {
            if !mail_admitted(db.as_ref(), &email).await {
                return;
            }
            mail_link(
                db.as_ref(),
                &email_rt,
                &jobs,
                Link {
                    kind: AuthEmail::VerifyEmail,
                    purpose: Purpose::VerifyEmail,
                    path: VERIFY_PATH,
                    provider: &provider,
                    principal: Some(&principal),
                    email: &email,
                    next: Some(&next),
                },
            )
            .await;
        });
    }
    // The password was right, so this reveals nothing the caller did not know.
    refusal(
        StatusCode::FORBIDDEN,
        "Confirm your email address first — we have sent a new link to your inbox.",
        back,
    )
}

/// Whether the address behind a password login is proven. Fails closed.
pub(crate) async fn is_verified(auth: &AuthRuntime, principal: &PrincipalId, email: &str) -> bool {
    match email_token::is_verified(auth.substrate().as_ref(), principal, email).await {
        Ok(verified) => verified,
        Err(err) => {
            warn!(target: "albedo.auth", %err, "verification lookup failed; refusing");
            false
        }
    }
}

// ── internals ──────────────────────────────────────────────────────────

struct Link<'a> {
    kind: AuthEmail,
    purpose: Purpose,
    path: &'static str,
    provider: &'a str,
    principal: Option<&'a PrincipalId>,
    email: &'a str,
    next: Option<&'a str>,
}

/// Issue a token, build the link, queue the email. Failures are logged: this
/// runs after the response has gone, and the person's recourse is the same
/// button again.
async fn mail_link(
    db: &dyn DataSubstrate,
    email_rt: &EmailRuntime,
    jobs: &crate::jobs::JobHandle,
    link: Link<'_>,
) {
    let now = now_ms();
    let token = match email_token::issue(db, link.purpose, link.provider, link.principal, link.email, link.next, now).await {
        Ok(token) => token,
        Err(err) => return warn!(target: "albedo.auth", %err, "could not issue an email token"),
    };
    // base64url, so it needs no escaping in a query string.
    let Some(url) = email_rt.link(&format!("{}?{TOKEN_FIELD}={}", link.path, token.expose_for_cookie())) else {
        return warn!(target: "albedo.auth", "no link origin; boot should have refused this app");
    };
    let message = templates::render(
        link.kind,
        Mailbox {
            name: None,
            address: link.email.to_string(),
        },
        &email_rt.app_name(),
        &url,
        link.purpose.ttl_minutes(),
        email_rt.config().templates.get(link.kind),
    );
    if let Err(err) = crate::jobs::queue_email(db, jobs, message, now).await {
        warn!(target: "albedo.auth", %err, "could not queue an auth email");
    }
}

/// Spend a token that must name a principal under `provider`.
async fn redeem(
    db: &dyn DataSubstrate,
    purpose: Purpose,
    token: &SessionToken,
    provider: &str,
) -> Option<(PrincipalId, email_token::Redeemed)> {
    match email_token::consume(db, purpose, token, now_ms()).await {
        Ok(Some(redeemed)) if redeemed.provider == provider => {
            let principal = redeemed.principal.clone()?;
            Some((principal, redeemed))
        }
        Ok(_) => None,
        Err(err) => {
            warn!(target: "albedo.auth", %err, "email token redemption failed");
            None
        }
    }
}

/// Charge one link to `email`'s mailbox budget — the database row every process
/// sharing `forge.db` charges (`email_token::admit_mail`). Fails closed: a
/// budget nobody could read is not a budget that said yes.
async fn mail_admitted(db: &dyn DataSubstrate, email: &str) -> bool {
    match email_token::admit_mail(db, email, now_ms()).await {
        Ok(true) => true,
        Ok(false) => {
            debug!(target: "albedo.auth", "a mailbox's link budget is spent; not sending");
            false
        }
        Err(err) => {
            warn!(target: "albedo.auth", %err, "the mailbox budget could not be read; not sending");
            false
        }
    }
}

async fn note_verified(db: &dyn DataSubstrate, principal: &PrincipalId, email: &str) {
    if let Err(err) = email_token::mark_verified(db, principal, email, now_ms()).await {
        warn!(target: "albedo.auth", %err, "could not record a verified address");
    }
}

fn next_of(redeemed: &email_token::Redeemed) -> ReturnPath {
    redeemed
        .next
        .as_deref()
        .and_then(ReturnPath::parse)
        .unwrap_or_else(ReturnPath::root)
}

fn provider(auth: &AuthRuntime, kind: ProviderKind) -> Option<&ResolvedProvider> {
    auth.registry().providers.iter().find(|p| p.kind == kind)
}

fn offers(provider: &ResolvedProvider, purpose: Purpose) -> bool {
    match purpose {
        Purpose::PasswordReset => provider.reset_by_email,
        Purpose::VerifyEmail => provider.verify_email,
        Purpose::MagicLink => provider.kind == ProviderKind::MagicLink,
    }
}

/// The provider, the mail runtime and the queue — or `None` if any is missing.
fn mail_ready(
    request: &AuthRequest<'_>,
    kind: ProviderKind,
    purpose: Purpose,
) -> Option<(String, Arc<EmailRuntime>, crate::jobs::JobHandle)> {
    let provider = provider(request.auth, kind).filter(|p| offers(p, purpose))?;
    Some((provider.name.clone(), request.email.clone()?, request.jobs.clone()?))
}

/// A self-contained page. No external resources, so nothing on it can leak the
/// token in the URL through a `Referer`; and `no-referrer` besides, for the
/// links a browser extension might add.
fn landing(status: StatusCode, title: &str, body: &str, set_cookie: Option<String>) -> Response<Body> {
    let page = format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>body{{font-family:system-ui,sans-serif;background:#f6f6f4;\
         color:#1a1a1a;margin:0;display:grid;place-items:center;min-height:100vh}}\
         main{{background:#fff;border-radius:8px;padding:32px;max-width:360px;width:calc(100% - 32px);\
         box-sizing:border-box}}h1{{font-size:20px;margin:0 0 20px}}label{{display:block;\
         font-size:14px;margin-bottom:6px}}input[type=password]{{width:100%;box-sizing:border-box;\
         padding:10px;font-size:16px;border:1px solid #ccc;border-radius:6px;margin-bottom:16px}}\
         button{{width:100%;padding:12px;font-size:16px;font-weight:600;border:0;border-radius:6px;\
         background:#1a1a1a;color:#fff;cursor:pointer}}.note{{font-size:13px;color:#666;\
         margin:16px 0 0}}@media(prefers-color-scheme:dark){{body{{background:#111;color:#eee}}\
         main{{background:#1c1c1c}}button{{background:#eee;color:#111}}\
         input[type=password]{{background:#111;color:#eee;border-color:#444}}}}</style></head>\
         <body><main>{body}</main></body></html>",
        html(title)
    );
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("referrer-policy", "no-referrer")
        .header("x-frame-options", "DENY")
        .header(
            "content-security-policy",
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; \
             frame-ancestors 'none'; base-uri 'none'",
        );
    if let Some(cookie) = set_cookie {
        builder = builder.header(header::SET_COOKIE, cookie);
    }
    builder
        .body(Body::from(page))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn html(raw: &str) -> String {
    templates::escape(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_landing_paths_are_matched_literally() {
        assert_eq!(match_email_page(RESET_PATH), Some(EmailPage::ResetForm));
        assert_eq!(match_email_page(MAGIC_SIGNIN_PATH), Some(EmailPage::MagicSignIn));
        assert_eq!(match_email_page(VERIFY_PATH), Some(EmailPage::Verify));
        for path in [
            "/_albedo/auth/password/reset/",
            "/_albedo/auth/password/forgot",
            "/_albedo/auth/email/verify/x",
        ] {
            assert_eq!(match_email_page(path), None, "{path}");
        }
    }

    /// The page carrying a token must not be framed, cached, or leak it by
    /// `Referer`.
    #[test]
    fn a_landing_page_carries_its_protective_headers() {
        let response = landing(StatusCode::OK, "t", "<p>x</p>", None);
        let headers = response.headers();
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["x-frame-options"], "DENY");
        assert!(headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'none'"));
    }
}
