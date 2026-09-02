//! AUTH · P0 — app-declared providers: the `auth` block in `albedo.config.ts`.
//!
//! The third sibling of [`forge::declare`](crate::forge::declare) and
//! [`aperture::declare`](crate::aperture::declare). FORGE declares *where rows
//! live*; APERTURE declares *where responses come from*; AUTH declares *where
//! principals come from*. All three lower a config block into something the
//! runtime resolves against a request, and all three make the resulting identity
//! compiler-known so nobody types the string that identifies it.
//!
//! ```ts
//! auth: {
//!   session: { ttl: "30d" },
//!   providers: {
//!     passkey:  {},                                               // kind inferred
//!     google:   { clientId: { env: "GOOGLE_ID" },
//!                 clientSecret: { env: "GOOGLE_SECRET" } },       // kind inferred
//!     okta:     { kind: "oidc", issuer: "https://acme.okta.com",  // kind declared
//!                 clientId: { env: "OKTA_ID" },
//!                 clientSecret: { env: "OKTA_SECRET" } },
//!     clerk:    { domain: { env: "CLERK_DOMAIN" } },              // delegated
//!     legacy:   { kind: "custom", module: "./src/auth/legacy.ts" },
//!   },
//! }
//! ```
//!
//! ## Four generic kinds, and why that is the whole extensibility story
//!
//! The named presets (`google`, `clerk`, …) are convenience. The extensibility
//! claim rests entirely on the four kinds underneath them, which between them
//! have no gaps:
//!
//! | kind | covers | what the author supplies |
//! |---|---|---|
//! | [`ProviderKind::OAuth`] | any OAuth 2.0 server | the two endpoints |
//! | [`ProviderKind::Oidc`] | any OIDC-compliant IdP | an issuer; endpoints come from `.well-known` |
//! | [`ProviderKind::Delegated`] | anything that issues a verifiable token | JWKS + issuer + a claim map |
//! | [`ProviderKind::Custom`] | everything else, including auth that predates us | a module returning a principal |
//!
//! `Custom` is what makes "any implementation" literally true rather than
//! nearly true, and it is deliberately last: it is the only kind whose
//! correctness we cannot argue for, because the author supplies the code. It
//! still lands on the same [`Principal`](crate::auth::Principal), so everything
//! derived from a principal — the topic identity, the authorization matrix,
//! instant revocation — keeps working over an implementation we have never seen.
//!
//! ## Why `kind` is inferred only for known names
//!
//! `google: {}` should not require `kind: "oauth"`; the name already said it.
//! But inference over an *unknown* name has a silently wrong answer — a provider
//! called `acme` is not a kind, and guessing one would produce a login flow the
//! author did not ask for. So the rule is: **a known preset infers its kind, and
//! anything else must declare one**
//! ([`AuthSchemaError::ProviderNeedsKind`]). Same shape as `partition_by` being
//! declared rather than inferred, and for the same reason.

use crate::auth::principal::PrincipalId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use url::Url;

/// Default session lifetime when the block does not name one.
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Default name of the cookie carrying the session id.
///
/// `__Host-` is not decoration: the prefix is enforced by the browser and means
/// the cookie must be `Secure`, have no `Domain`, and have `Path=/`. That makes
/// a subdomain unable to set it, which is the cookie-fixation vector a plain
/// name leaves open.
pub const DEFAULT_SESSION_COOKIE: &str = "__Host-albedo_session";

/// How a secret reaches the runtime.
///
/// There is no bare-string form, deliberately. `clientSecret: "sk_live_…"` is
/// the spelling that puts a production credential in version control, and it is
/// also the spelling everyone reaches for first — so it is a build error that
/// names the alternative rather than a convenience that works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum SecretDecl {
    /// Read from an environment variable at boot. **The intended form.**
    Env {
        /// The variable name.
        env: String,
    },
    /// A literal, for local experimentation. Second-class on purpose.
    Value {
        /// The literal secret.
        value: String,
    },
}

impl SecretDecl {
    /// Resolve against the process environment.
    ///
    /// # Errors
    /// [`AuthSchemaError::MissingEnv`] when an `env` form names a variable that
    /// is not set. Checked at boot rather than at declaration time, matching
    /// `SourceReader::from_declarations` — a config file is valid on a machine
    /// that has not exported the variable yet.
    pub fn resolve(&self, provider: &str, field: &'static str) -> Result<String, AuthSchemaError> {
        match self {
            Self::Value { value } => Ok(value.clone()),
            Self::Env { env } => std::env::var(env).map_err(|_| AuthSchemaError::MissingEnv {
                provider: provider.to_string(),
                field,
                variable: env.clone(),
            }),
        }
    }
}

/// What kind of thing a declared provider is.
///
/// Serialized lowercase so `kind: "oidc"` reads as written in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// WebAuthn. First-party, and the default for a new app — a passkey needs no
    /// email deliverability, no reset flow and no password storage.
    Passkey,
    /// Password, argon2id. First-party, opt-in, never the path of least
    /// resistance.
    Password,
    /// Emailed one-time link. First-party; carries the deliverability burden
    /// [`Self::Passkey`] exists to avoid.
    MagicLink,
    /// OAuth 2.0 where **we** perform the code exchange. Endpoints come from a
    /// preset or are declared outright.
    OAuth,
    /// OpenID Connect where **we** perform the code exchange, and the endpoints
    /// come from the issuer's `.well-known/openid-configuration`. One reader
    /// covers every compliant IdP.
    Oidc,
    /// A third party owns the record and issues a token; we verify it against
    /// their JWKS and project the claims. Clerk, Auth0, WorkOS, Supabase, or any
    /// JWT issuer at all.
    Delegated,
    /// Author-supplied code returns a principal. The escape hatch that makes the
    /// provider list open rather than long.
    Custom,
}

impl ProviderKind {
    /// Human name for error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passkey => "passkey",
            Self::Password => "password",
            Self::MagicLink => "magiclink",
            Self::OAuth => "oauth",
            Self::Oidc => "oidc",
            Self::Delegated => "delegated",
            Self::Custom => "custom",
        }
    }

    /// Every kind, for error messages that list the alternatives.
    #[must_use]
    pub const fn all() -> [Self; 7] {
        [
            Self::Passkey,
            Self::Password,
            Self::MagicLink,
            Self::OAuth,
            Self::Oidc,
            Self::Delegated,
            Self::Custom,
        ]
    }
}

/// A provider name whose kind is known without being declared.
///
/// Convenience only — every entry here is expressible as a generic kind plus
/// fields, and the table exists so the common cases are short. A name absent
/// from this table is not an error; it just has to say its `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// The provider name this preset matches.
    pub name: &'static str,
    /// The kind the name implies.
    pub kind: ProviderKind,
    /// For [`ProviderKind::Oidc`]: the fixed issuer.
    pub issuer: Option<&'static str>,
    /// For [`ProviderKind::OAuth`]: where the browser is sent.
    pub authorize_url: Option<&'static str>,
    /// For [`ProviderKind::OAuth`]: where the code is exchanged.
    pub token_url: Option<&'static str>,
    /// For [`ProviderKind::OAuth`]: where the profile is read.
    pub userinfo_url: Option<&'static str>,
    /// For [`ProviderKind::Delegated`]: JWKS URL template, `{domain}` substituted
    /// from the provider's `domain` field.
    pub jwks_template: Option<&'static str>,
    /// For [`ProviderKind::Delegated`]: issuer template, `{domain}` substituted.
    pub issuer_template: Option<&'static str>,
}

impl Preset {
    const fn oidc(name: &'static str, issuer: &'static str) -> Self {
        Self {
            name,
            kind: ProviderKind::Oidc,
            issuer: Some(issuer),
            authorize_url: None,
            token_url: None,
            userinfo_url: None,
            jwks_template: None,
            issuer_template: None,
        }
    }

    const fn oauth(
        name: &'static str,
        authorize_url: &'static str,
        token_url: &'static str,
        userinfo_url: &'static str,
    ) -> Self {
        Self {
            name,
            kind: ProviderKind::OAuth,
            issuer: None,
            authorize_url: Some(authorize_url),
            token_url: Some(token_url),
            userinfo_url: Some(userinfo_url),
            jwks_template: None,
            issuer_template: None,
        }
    }

    const fn delegated(
        name: &'static str,
        jwks_template: &'static str,
        issuer_template: &'static str,
    ) -> Self {
        Self {
            name,
            kind: ProviderKind::Delegated,
            issuer: None,
            authorize_url: None,
            token_url: None,
            userinfo_url: None,
            jwks_template: Some(jwks_template),
            issuer_template: Some(issuer_template),
        }
    }

    const fn first_party(name: &'static str, kind: ProviderKind) -> Self {
        Self {
            name,
            kind,
            issuer: None,
            authorize_url: None,
            token_url: None,
            userinfo_url: None,
            jwks_template: None,
            issuer_template: None,
        }
    }
}

/// The preset table.
///
/// Kept deliberately short. `AUTH.md` § 8.1: do not fight Auth.js's ~80
/// providers — a handful of common names plus generic OIDC discovery covers the
/// compliant long tail, and [`ProviderKind::Custom`] covers the rest. Growing
/// this table is additive and boring; needing to grow it to support a provider
/// would mean the generic kinds had a gap, which is the thing to avoid.
pub const PRESETS: &[Preset] = &[
    // ── first-party ──
    Preset::first_party("passkey", ProviderKind::Passkey),
    Preset::first_party("password", ProviderKind::Password),
    Preset::first_party("magiclink", ProviderKind::MagicLink),
    // ── OIDC: the issuer publishes its own endpoints ──
    Preset::oidc("google", "https://accounts.google.com"),
    Preset::oidc("microsoft", "https://login.microsoftonline.com/common/v2.0"),
    Preset::oidc("apple", "https://appleid.apple.com"),
    Preset::oidc("twitch", "https://id.twitch.tv/oauth2"),
    // ── plain OAuth 2.0: no discovery document, so the endpoints are named ──
    Preset::oauth(
        "github",
        "https://github.com/login/oauth/authorize",
        "https://github.com/login/oauth/access_token",
        "https://api.github.com/user",
    ),
    Preset::oauth(
        "gitlab",
        "https://gitlab.com/oauth/authorize",
        "https://gitlab.com/oauth/token",
        "https://gitlab.com/api/v4/user",
    ),
    Preset::oauth(
        "discord",
        "https://discord.com/oauth2/authorize",
        "https://discord.com/api/oauth2/token",
        "https://discord.com/api/users/@me",
    ),
    // ── delegated: they issue the token, we verify it ──
    Preset::delegated(
        "clerk",
        "https://{domain}/.well-known/jwks.json",
        "https://{domain}",
    ),
    Preset::delegated(
        "auth0",
        "https://{domain}/.well-known/jwks.json",
        "https://{domain}/",
    ),
    Preset::delegated(
        "workos",
        "https://{domain}/sso/jwks/{domain}",
        "https://{domain}",
    ),
    Preset::delegated(
        "supabase",
        "https://{domain}/auth/v1/.well-known/jwks.json",
        "https://{domain}/auth/v1",
    ),
];

/// Look a provider name up in [`PRESETS`].
#[must_use]
pub fn preset_for(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

/// Longest in-app path this crate will accept anywhere — a declared login
/// route, a submitted return path.
pub const MAX_APP_PATH_BYTES: usize = 2048;

/// Whether `raw` is a path on **this** app and nothing else.
///
/// ## One rule, two readers
///
/// Two places need this and they must not disagree: the `login` route declared
/// below, which the route gate redirects a stranger to, and
/// `albedo_server::forms::ReturnPath`, which decides where a form submit sends
/// the browser afterwards. Both are "a URL we are about to put in a `Location`
/// header", and the second one is **request-supplied** — so if the two spellings
/// of the rule ever drifted, the looser one would be the one attackers found.
///
/// ## The two characters that do all the work
///
/// `//evil.example` and `/\evil.example` are paths by any naive check — they
/// start with `/` — and both resolve in every browser as **protocol-relative
/// URLs pointing at another host**. That is why the second byte is tested
/// explicitly rather than the rule being written as "starts with `/`". For a
/// sign-in flow this is not a generic open redirect: "authenticate, then get
/// bounced somewhere else" is the credential-phishing chain, with the victim
/// already primed to trust wherever they land.
///
/// Control characters and non-ASCII are refused because the value ends up in a
/// header: a CR or LF is response splitting, and a raw non-ASCII byte is
/// something the HTTP layer would have to re-encode or reject. A browser
/// percent-encodes those anyway, so nothing legitimate is lost.
#[must_use]
pub fn is_rooted_app_path(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > MAX_APP_PATH_BYTES {
        return false;
    }
    let mut bytes = raw.bytes();
    if bytes.next() != Some(b'/') {
        return false;
    }
    if matches!(bytes.next(), Some(b'/') | Some(b'\\')) {
        return false;
    }
    !raw.bytes()
        .any(|byte| byte.is_ascii_control() || !byte.is_ascii() || byte == b' ')
}

/// Session handling for the whole app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionDecl {
    /// Lifetime, e.g. `"30d"`, `"12h"`, `"90m"`. Defaults to
    /// [`DEFAULT_SESSION_TTL`].
    #[serde(default)]
    pub ttl: Option<String>,
    /// Cookie name. Defaults to [`DEFAULT_SESSION_COOKIE`]; override only if a
    /// deployment cannot satisfy the `__Host-` prefix's requirements.
    #[serde(default)]
    pub cookie: Option<String>,
}

/// One app-declared provider.
///
/// A wide struct rather than an enum per kind, because the config is JSON and an
/// internally-tagged enum would make every unknown-field mistake surface as
/// "unknown variant" rather than as the missing field it actually is. Which
/// fields are required is decided in [`ProviderDecl::lower`], where the kind is
/// known and the error can name both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProviderDecl {
    /// What this provider is. Optional **only** when the name is in [`PRESETS`].
    #[serde(default)]
    pub kind: Option<ProviderKind>,

    // ── OAuth / OIDC ──
    /// OIDC issuer. Endpoints are discovered from
    /// `{issuer}/.well-known/openid-configuration`.
    #[serde(default)]
    pub issuer: Option<String>,
    /// OAuth client id. Not a secret, but declared the same way so one form
    /// covers both and neither ends up inlined by habit.
    #[serde(default, rename = "clientId", alias = "client_id")]
    pub client_id: Option<SecretDecl>,
    /// OAuth client secret.
    #[serde(default, rename = "clientSecret", alias = "client_secret")]
    pub client_secret: Option<SecretDecl>,
    /// Where the browser is sent. Overrides a preset; required for a bare
    /// `kind: "oauth"`.
    #[serde(default, rename = "authorizeUrl", alias = "authorize_url")]
    pub authorize_url: Option<String>,
    /// Where the authorization code is exchanged.
    #[serde(default, rename = "tokenUrl", alias = "token_url")]
    pub token_url: Option<String>,
    /// Where the profile is read, for OAuth servers with no `userinfo` in a
    /// discovery document.
    #[serde(default, rename = "userinfoUrl", alias = "userinfo_url")]
    pub userinfo_url: Option<String>,
    /// Scopes to request. Sensible per-preset defaults apply when absent.
    #[serde(default)]
    pub scopes: Vec<String>,

    // ── delegated ──
    /// Tenant domain, substituted into a preset's JWKS and issuer templates.
    #[serde(default)]
    pub domain: Option<SecretDecl>,
    /// JWKS URL. Overrides a preset; required for a bare `kind: "delegated"`.
    #[serde(default)]
    pub jwks: Option<String>,
    /// Expected `aud`. Absent means the audience is not checked, which is
    /// correct for issuers that do not set one.
    #[serde(default)]
    pub audience: Option<String>,
    /// Which claim carries each principal field. Defaults to the OIDC standard
    /// names (`sub`, `email`, `name`, `picture`); overridable because a
    /// non-OIDC issuer will not use them.
    #[serde(default, rename = "claimMap", alias = "claim_map")]
    pub claim_map: BTreeMap<String, String>,

    // ── custom ──
    /// Module exporting the principal resolver, relative to the project root.
    #[serde(default)]
    pub module: Option<String>,

    // ── password ──
    /// How a password is reset. Only `"email"` today; absent means no reset
    /// flow, which is a legitimate choice for an internal tool.
    #[serde(default)]
    pub reset: Option<String>,
}

/// The `auth` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AuthDeclaration {
    /// Session handling.
    #[serde(default)]
    pub session: SessionDecl,
    /// The providers, keyed by the name that appears in a login URL
    /// (`/_albedo/auth/google/start`) and in [`Principal::provider`].
    ///
    /// [`Principal::provider`]: crate::auth::Principal::provider
    ///
    /// A `BTreeMap` for the same reason `forge` and `sources` are ones: the
    /// lowering order must be identical on every machine and every build,
    /// because the hosts it yields become the egress allowlist.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDecl>,

    /// The app's own sign-in route, e.g. `"/sign-in"`.
    ///
    /// ## Why this is declared and not derived
    ///
    /// A route gate (`export const auth = "required"`) has to send a stranger
    /// *somewhere*, and until this existed there was nowhere honest to send them
    /// — so the gate answered `401`, which is accurate and useless. Now that P2
    /// can build a sign-in page, the app can name it.
    ///
    /// It is not inferred from "the route with a login form on it": that would
    /// make a security-adjacent redirect target change when somebody adds a
    /// second form somewhere, which is the same defect route gating itself
    /// refused to inherit.
    ///
    /// Absent is a legitimate choice — an app whose only gate is an API, or one
    /// that would rather show `401` than a sign-in page. The gate falls back to
    /// the refusal it gave before.
    #[serde(default)]
    pub login: Option<String>,
}

/// Why an `auth` block was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSchemaError {
    /// A provider name is not usable in a URL path segment or a `Principal`.
    InvalidName {
        /// The offending name.
        name: String,
    },
    /// The name is not a preset and no `kind` was declared. **The § 8.1 build
    /// error** — the one that keeps the provider list open without letting
    /// inference guess.
    ProviderNeedsKind {
        /// The offending name.
        provider: String,
    },
    /// A kind needs a field the declaration did not supply.
    MissingField {
        /// Provider name.
        provider: String,
        /// The kind that requires it.
        kind: ProviderKind,
        /// The absent field.
        field: &'static str,
        /// Why it is needed.
        reason: &'static str,
    },
    /// A declared URL did not parse, or is not an `https` origin.
    InvalidUrl {
        /// Provider name.
        provider: String,
        /// Which field.
        field: &'static str,
        /// The offending value.
        value: String,
        /// What was wrong.
        reason: String,
    },
    /// A session TTL did not parse.
    InvalidTtl {
        /// The offending value.
        value: String,
    },
    /// A cookie name is not a valid cookie token, or claims a `__Host-`/`__Secure-`
    /// prefix this deployment cannot honour.
    InvalidCookie {
        /// The offending value.
        value: String,
        /// What was wrong.
        reason: String,
    },
    /// The declared `login` route is not a path on this app.
    InvalidLoginPath {
        /// The offending value.
        value: String,
    },
    /// A `SecretDecl::Env` named a variable that is not set.
    MissingEnv {
        /// Provider name.
        provider: String,
        /// Which field.
        field: &'static str,
        /// The variable name.
        variable: String,
    },
}

impl std::fmt::Display for AuthSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName { name } => write!(
                f,
                "auth provider `{name}`: a provider name appears in a login URL and on every \
                 principal it mints, so it must match [a-z0-9_-]{{1,32}}"
            ),
            Self::ProviderNeedsKind { provider } => {
                let presets: Vec<&str> = PRESETS.iter().map(|preset| preset.name).collect();
                let kinds: Vec<&str> = ProviderKind::all()
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect();
                write!(
                    f,
                    "auth provider `{provider}`: needs a `kind`, because `{provider}` is not a \
                     known preset. Presets that infer their own kind: {}. Otherwise declare one \
                     of: {}. Anything not covered by the first six is `kind: \"custom\"` with a \
                     `module` — that path is open on purpose",
                    presets.join(", "),
                    kinds.join(", "),
                )
            }
            Self::MissingField {
                provider,
                kind,
                field,
                reason,
            } => write!(
                f,
                "auth provider `{provider}` (kind `{}`): missing `{field}` — {reason}",
                kind.as_str()
            ),
            Self::InvalidUrl {
                provider,
                field,
                value,
                reason,
            } => write!(
                f,
                "auth provider `{provider}`: `{field}` = `{value}` is not usable — {reason}"
            ),
            Self::InvalidTtl { value } => write!(
                f,
                "auth session `ttl` = `{value}` did not parse — expected a duration like \
                 `30d`, `12h`, `90m` or `3600s`"
            ),
            Self::InvalidCookie { value, reason } => {
                write!(f, "auth session `cookie` = `{value}`: {reason}")
            }
            Self::InvalidLoginPath { value } => write!(
                f,
                "auth `login` = `{value}` is not a route on this app. It becomes a `Location` \
                 header the moment a stranger reaches a gated page, so it must be a rooted path \
                 like `/sign-in` — not an absolute URL, not protocol-relative (`//host`), and \
                 with no spaces or control characters"
            ),
            Self::MissingEnv {
                provider,
                field,
                variable,
            } => write!(
                f,
                "auth provider `{provider}`: `{field}` reads environment variable `{variable}`, \
                 which is not set"
            ),
        }
    }
}

impl std::error::Error for AuthSchemaError {}

/// Where a provider's endpoints come from once the kind is known.
///
/// Discovery is *recorded*, not performed: resolving an issuer needs a network
/// call, and lowering is synchronous and runs in the compiler. The boot path
/// performs it, over the egress allowlist this same lowering produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoints {
    /// No outbound endpoints — a first-party kind.
    FirstParty,
    /// Explicit OAuth 2.0 endpoints.
    OAuth {
        /// Where the browser is sent.
        authorize_url: Url,
        /// Where the code is exchanged.
        token_url: Url,
        /// Where the profile is read, when the server has one.
        userinfo_url: Option<Url>,
    },
    /// An OIDC issuer whose `.well-known/openid-configuration` is read at boot.
    Discovered {
        /// The issuer.
        issuer: Url,
    },
    /// A JWKS to verify a third party's tokens against.
    Jwks {
        /// Where the keys are published.
        jwks: Url,
        /// The `iss` every accepted token must carry.
        issuer: String,
    },
    /// Author-supplied code. No endpoints, and nothing for us to allowlist.
    Custom {
        /// Module path, relative to the project root.
        module: String,
    },
}

/// A validated provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    /// The declared name.
    pub name: String,
    /// The kind, declared or inferred.
    pub kind: ProviderKind,
    /// Where it talks to, if anywhere.
    pub endpoints: Endpoints,
    /// Scopes to request, with preset defaults applied.
    pub scopes: Vec<String>,
    /// Expected `aud`, for a delegated provider that sets one.
    pub audience: Option<String>,
    /// Claim → principal field, with OIDC defaults applied.
    pub claim_map: BTreeMap<String, String>,
    /// The client id, still unresolved — an env var may legitimately be unset on
    /// the machine that runs the build.
    pub client_id: Option<SecretDecl>,
    /// The client secret, still unresolved.
    pub client_secret: Option<SecretDecl>,
}

/// The lowered `auth` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRegistry {
    /// Session lifetime.
    pub session_ttl: Duration,
    /// Session cookie name.
    pub session_cookie: String,
    /// Providers, in declaration order.
    pub providers: Vec<ResolvedProvider>,
    /// The app's sign-in route, validated. `None` means a gated route answers
    /// `401` rather than redirecting.
    pub login_path: Option<String>,
}

impl AuthRegistry {
    /// An app that declared no `auth` block.
    ///
    /// Not the same as an app with auth misconfigured: no providers means every
    /// request is anonymous, which is exactly what an app without login wants.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            session_ttl: DEFAULT_SESSION_TTL,
            session_cookie: DEFAULT_SESSION_COOKIE.to_string(),
            providers: Vec::new(),
            login_path: None,
        }
    }

    /// Whether this app has any way to authenticate anyone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Look a provider up by declared name.
    #[must_use]
    pub fn provider(&self, name: &str) -> Option<&ResolvedProvider> {
        self.providers
            .iter()
            .find(|provider| provider.name == name)
    }

    /// Every host this block will talk to — the **egress allowlist** contribution.
    ///
    /// Derived exactly as `SourceReader::from_declarations` derives APERTURE's,
    /// and for the same reason: the outbound token exchange and the JWKS fetch
    /// ride APERTURE's client, and an allowlist assembled by hand somewhere else
    /// would be a second source of truth about where this app connects.
    ///
    /// A `BTreeSet` so the allowlist is byte-identical on every build.
    #[must_use]
    pub fn egress_hosts(&self) -> BTreeSet<String> {
        let mut hosts = BTreeSet::new();
        for provider in &self.providers {
            match &provider.endpoints {
                Endpoints::FirstParty | Endpoints::Custom { .. } => {}
                Endpoints::OAuth {
                    authorize_url,
                    token_url,
                    userinfo_url,
                } => {
                    // The authorize URL is included even though the *browser*
                    // visits it, not us: a deployment that allowlists only what
                    // the server dials would break the moment discovery or a
                    // redirect check wanted it, and the host is the same
                    // origin's anyway.
                    for url in [Some(authorize_url), Some(token_url), userinfo_url.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        if let Some(host) = url.host_str() {
                            hosts.insert(host.to_string());
                        }
                    }
                }
                Endpoints::Discovered { issuer } => {
                    if let Some(host) = issuer.host_str() {
                        hosts.insert(host.to_string());
                    }
                }
                Endpoints::Jwks { jwks, .. } => {
                    if let Some(host) = jwks.host_str() {
                        hosts.insert(host.to_string());
                    }
                }
            }
        }
        hosts
    }
}

impl AuthDeclaration {
    /// Validate and lower the whole block.
    ///
    /// # Errors
    /// The first [`AuthSchemaError`] encountered, in declaration order, so the
    /// message is stable across builds.
    pub fn lower(&self) -> Result<AuthRegistry, AuthSchemaError> {
        let session_ttl = match &self.session.ttl {
            Some(raw) => parse_duration(raw).ok_or_else(|| AuthSchemaError::InvalidTtl {
                value: raw.clone(),
            })?,
            None => DEFAULT_SESSION_TTL,
        };

        let session_cookie = match &self.session.cookie {
            Some(raw) => {
                validate_cookie_name(raw)?;
                raw.clone()
            }
            None => DEFAULT_SESSION_COOKIE.to_string(),
        };

        let mut providers = Vec::with_capacity(self.providers.len());
        for (name, decl) in &self.providers {
            providers.push(decl.lower(name)?);
        }

        let login_path = match self.login.as_deref() {
            Some(raw) if !is_rooted_app_path(raw) => {
                return Err(AuthSchemaError::InvalidLoginPath {
                    value: raw.to_string(),
                })
            }
            Some(raw) => Some(raw.to_string()),
            None => None,
        };

        Ok(AuthRegistry {
            session_ttl,
            session_cookie,
            providers,
            // 🔴 This read `login_path: None` with a note saying "when P2 lands,
            // this is where the declared route gets lowered". P2 landed; the note
            // did not. `login` was declared, documented and **validated** while
            // being dropped on the floor here, so the route gate could only ever
            // see `None` and every gated page was a 401 dead end — including in
            // the scaffold, whose own config comment promises a stranger is
            // "sent to `login` below". Found 2026-09-02 by building a real app.
            //
            // 🔑 Validated with [`is_rooted_app_path`], the same rule
            // `forms::ReturnPath` applies to a request-supplied return path.
            // The declared value is author-supplied rather than attacker-supplied,
            // but it becomes a `Location` header either way, and one rule with two
            // spellings is how the looser spelling gets found.
            login_path,
        })
    }
}

impl ProviderDecl {
    /// Validate and lower one provider.
    ///
    /// # Errors
    /// See [`AuthSchemaError`].
    pub fn lower(&self, name: &str) -> Result<ResolvedProvider, AuthSchemaError> {
        if !is_valid_provider_name(name) {
            return Err(AuthSchemaError::InvalidName {
                name: name.to_string(),
            });
        }

        let preset = preset_for(name);
        let kind = match (self.kind, preset) {
            // A declared kind always wins: `okta: { kind: "oidc" }` is the
            // author being specific, and a preset of the same name (there is
            // none today, but there could be) must not override them.
            (Some(kind), _) => kind,
            (None, Some(preset)) => preset.kind,
            (None, None) => {
                return Err(AuthSchemaError::ProviderNeedsKind {
                    provider: name.to_string(),
                })
            }
        };

        let endpoints = self.endpoints_for(name, kind, preset)?;
        let scopes = self.scopes_for(kind, preset);
        let claim_map = self.claim_map_with_defaults();

        Ok(ResolvedProvider {
            name: name.to_string(),
            kind,
            endpoints,
            scopes,
            audience: self.audience.clone(),
            claim_map,
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
        })
    }

    fn endpoints_for(
        &self,
        name: &str,
        kind: ProviderKind,
        preset: Option<&'static Preset>,
    ) -> Result<Endpoints, AuthSchemaError> {
        match kind {
            ProviderKind::Passkey | ProviderKind::Password | ProviderKind::MagicLink => {
                Ok(Endpoints::FirstParty)
            }

            ProviderKind::Custom => {
                let module = self.module.clone().ok_or(AuthSchemaError::MissingField {
                    provider: name.to_string(),
                    kind,
                    field: "module",
                    reason: "a custom provider is code we do not supply, so it must say where \
                             that code lives",
                })?;
                Ok(Endpoints::Custom { module })
            }

            ProviderKind::Oidc => {
                let raw = self
                    .issuer
                    .clone()
                    .or_else(|| preset.and_then(|preset| preset.issuer.map(str::to_string)))
                    .ok_or(AuthSchemaError::MissingField {
                        provider: name.to_string(),
                        kind,
                        field: "issuer",
                        reason: "OIDC endpoints are read from \
                                 `{issuer}/.well-known/openid-configuration`, so the issuer is \
                                 the one thing that cannot be discovered",
                    })?;
                Ok(Endpoints::Discovered {
                    issuer: require_https(name, "issuer", &raw)?,
                })
            }

            ProviderKind::OAuth => {
                let authorize = self
                    .authorize_url
                    .clone()
                    .or_else(|| preset.and_then(|preset| preset.authorize_url.map(str::to_string)))
                    .ok_or(AuthSchemaError::MissingField {
                        provider: name.to_string(),
                        kind,
                        field: "authorizeUrl",
                        reason: "a plain OAuth 2.0 server publishes no discovery document, so \
                                 its endpoints have to be named. If this provider is \
                                 OIDC-compliant, use `kind: \"oidc\"` with an `issuer` instead \
                                 and both endpoints come for free",
                    })?;
                let token = self
                    .token_url
                    .clone()
                    .or_else(|| preset.and_then(|preset| preset.token_url.map(str::to_string)))
                    .ok_or(AuthSchemaError::MissingField {
                        provider: name.to_string(),
                        kind,
                        field: "tokenUrl",
                        reason: "the authorization code has to be exchanged somewhere",
                    })?;
                let userinfo = self
                    .userinfo_url
                    .clone()
                    .or_else(|| preset.and_then(|preset| preset.userinfo_url.map(str::to_string)));

                Ok(Endpoints::OAuth {
                    authorize_url: require_https(name, "authorizeUrl", &authorize)?,
                    token_url: require_https(name, "tokenUrl", &token)?,
                    userinfo_url: match userinfo {
                        Some(raw) => Some(require_https(name, "userinfoUrl", &raw)?),
                        None => None,
                    },
                })
            }

            ProviderKind::Delegated => {
                // A preset's templates need a domain; an explicit `jwks` does
                // not. Resolve the domain first so both paths can report the
                // same missing-field error when neither is available.
                let domain = match &self.domain {
                    Some(secret) => Some(secret.resolve(name, "domain")?),
                    None => None,
                };

                let jwks_raw = match (&self.jwks, preset, &domain) {
                    (Some(explicit), _, _) => explicit.clone(),
                    (None, Some(preset), Some(domain)) => preset
                        .jwks_template
                        .ok_or(AuthSchemaError::MissingField {
                            provider: name.to_string(),
                            kind,
                            field: "jwks",
                            reason: "this preset publishes no JWKS template",
                        })?
                        .replace("{domain}", domain),
                    _ => {
                        return Err(AuthSchemaError::MissingField {
                            provider: name.to_string(),
                            kind,
                            field: "jwks",
                            reason: "a delegated provider's tokens are verified against its \
                                     public keys, so we need the JWKS URL — or a `domain`, if \
                                     this is a preset that can build one",
                        })
                    }
                };

                let issuer = match (&self.issuer, preset, &domain) {
                    (Some(explicit), _, _) => explicit.clone(),
                    (None, Some(preset), Some(domain)) => preset
                        .issuer_template
                        .ok_or(AuthSchemaError::MissingField {
                            provider: name.to_string(),
                            kind,
                            field: "issuer",
                            reason: "this preset publishes no issuer template",
                        })?
                        .replace("{domain}", domain),
                    _ => {
                        return Err(AuthSchemaError::MissingField {
                            provider: name.to_string(),
                            kind,
                            field: "issuer",
                            reason: "a token is only accepted if its `iss` matches, so an \
                                     unchecked issuer would accept any JWT this JWKS happens to \
                                     verify",
                        })
                    }
                };

                Ok(Endpoints::Jwks {
                    jwks: require_https(name, "jwks", &jwks_raw)?,
                    issuer,
                })
            }
        }
    }

    fn scopes_for(&self, kind: ProviderKind, preset: Option<&'static Preset>) -> Vec<String> {
        if !self.scopes.is_empty() {
            return self.scopes.clone();
        }
        match (kind, preset.map(|preset| preset.name)) {
            // GitHub is the one common provider whose default scope set yields
            // no email — `read:user` alone returns a profile with `email: null`
            // whenever the address is private, which is the default.
            (ProviderKind::OAuth, Some("github")) => {
                vec!["read:user".to_string(), "user:email".to_string()]
            }
            (ProviderKind::OAuth, Some("gitlab")) => vec!["read_user".to_string()],
            (ProviderKind::OAuth, Some("discord")) => {
                vec!["identify".to_string(), "email".to_string()]
            }
            (ProviderKind::Oidc, _) => vec!["openid".to_string(), "email".to_string(), "profile".to_string()],
            _ => Vec::new(),
        }
    }

    /// OIDC standard claim names, overridden by anything the author declared.
    ///
    /// Defaults rather than requirements: an OIDC provider uses these, and a
    /// bespoke issuer that does not can say so without us having to know about
    /// it in advance.
    fn claim_map_with_defaults(&self) -> BTreeMap<String, String> {
        let mut map: BTreeMap<String, String> = [
            ("subject", "sub"),
            ("email", "email"),
            ("name", "name"),
            ("image", "picture"),
        ]
        .into_iter()
        .map(|(field, claim)| (field.to_string(), claim.to_string()))
        .collect();
        for (field, claim) in &self.claim_map {
            map.insert(field.clone(), claim.clone());
        }
        map
    }
}

/// A provider name has to survive being a URL path segment, a JSON key and a
/// column value, so it gets the narrow alphabet rather than a general one.
///
/// Lowercase-only because the name appears in `/_albedo/auth/{name}/start` and a
/// case-sensitive route that reads like a case-insensitive one is a support
/// ticket waiting to happen.
#[must_use]
pub fn is_valid_provider_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Require an `https` URL.
///
/// `http` is refused for every provider endpoint, including in development: an
/// authorization code or a client secret crossing plaintext is the same mistake
/// in both environments, and a dev-only exemption is how it reaches production.
fn require_https(provider: &str, field: &'static str, raw: &str) -> Result<Url, AuthSchemaError> {
    let url = Url::parse(raw).map_err(|err| AuthSchemaError::InvalidUrl {
        provider: provider.to_string(),
        field,
        value: raw.to_string(),
        reason: err.to_string(),
    })?;
    if url.scheme() != "https" {
        return Err(AuthSchemaError::InvalidUrl {
            provider: provider.to_string(),
            field,
            value: raw.to_string(),
            reason: format!(
                "scheme is `{}`; provider endpoints carry authorization codes and client \
                 secrets, so https is required even in development",
                url.scheme()
            ),
        });
    }
    if url.host_str().is_none() {
        return Err(AuthSchemaError::InvalidUrl {
            provider: provider.to_string(),
            field,
            value: raw.to_string(),
            reason: "no host".to_string(),
        });
    }
    Ok(url)
}

/// Cookie names are RFC 6265 tokens, and the `__Host-` prefix carries
/// requirements a deployment has to actually meet.
fn validate_cookie_name(name: &str) -> Result<(), AuthSchemaError> {
    if name.is_empty() || name.len() > 64 {
        return Err(AuthSchemaError::InvalidCookie {
            value: name.to_string(),
            reason: "a cookie name must be 1–64 characters".to_string(),
        });
    }
    // RFC 6265 token: no separators, no control characters, ASCII only.
    const SEPARATORS: &[u8] = b"()<>@,;:\\\"/[]?={} \t";
    if !name
        .bytes()
        .all(|b| b.is_ascii_graphic() && !SEPARATORS.contains(&b))
    {
        return Err(AuthSchemaError::InvalidCookie {
            value: name.to_string(),
            reason: "a cookie name must be an RFC 6265 token — no spaces, separators or \
                     control characters"
                .to_string(),
        });
    }
    Ok(())
}

/// `30d`, `12h`, `90m`, `3600s`, `250ms`.
///
/// A local parser rather than a dependency, matching
/// [`aperture::declare`](crate::aperture::declare)'s `refresh` handling — the
/// grammar is four suffixes and sharing one implementation across two blocks is
/// the next change, not this one.
fn parse_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    // `ms` before `m`, or `250ms` parses as 250 minutes.
    for (suffix, scale) in [
        ("ms", None),
        ("s", Some(1u64)),
        ("m", Some(60)),
        ("h", Some(60 * 60)),
        ("d", Some(24 * 60 * 60)),
    ] {
        let Some(digits) = raw.strip_suffix(suffix) else {
            continue;
        };
        let value: u64 = digits.trim().parse().ok()?;
        return Some(match scale {
            Some(seconds) => Duration::from_secs(value.checked_mul(seconds)?),
            None => Duration::from_millis(value),
        });
    }
    None
}

/// Reserved so a later pass can assert it: the `PrincipalId` type is the only
/// thing that may name a principal in a topic. Referenced here to keep the
/// declaration module honest about what it is ultimately producing.
const _: fn() -> Option<PrincipalId> = || None;

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(json: serde_json::Value) -> AuthDeclaration {
        serde_json::from_value(json).expect("declaration parses")
    }

    #[test]
    fn a_preset_name_infers_its_kind() {
        let registry = decl(serde_json::json!({
            "providers": { "passkey": {}, "github": {}, "google": {} }
        }))
        .lower()
        .expect("lowers");

        assert_eq!(registry.provider("passkey").unwrap().kind, ProviderKind::Passkey);
        assert_eq!(registry.provider("github").unwrap().kind, ProviderKind::OAuth);
        assert_eq!(registry.provider("google").unwrap().kind, ProviderKind::Oidc);
    }

    /// **The § 8.1 rule.** Inference over an unknown name has a silently wrong
    /// answer, so it is refused — and the message has to name the way out, or
    /// the open provider list is only theoretically open.
    #[test]
    fn an_unknown_name_must_declare_its_kind() {
        let error = decl(serde_json::json!({ "providers": { "acme": {} } }))
            .lower()
            .expect_err("no preset named acme");
        assert_eq!(
            error,
            AuthSchemaError::ProviderNeedsKind {
                provider: "acme".to_string()
            }
        );
        let rendered = error.to_string();
        assert!(rendered.contains("custom"), "must name the escape hatch");
        assert!(rendered.contains("oidc"), "must list the generic kinds");
    }

    /// The extensibility claim, asserted directly: a provider nobody has heard
    /// of works with no code change, through each of the four generic kinds.
    #[test]
    fn every_generic_kind_accepts_a_provider_we_never_shipped() {
        let registry = decl(serde_json::json!({
            "providers": {
                "acme-oauth": {
                    "kind": "oauth",
                    "authorizeUrl": "https://acme.example/authorize",
                    "tokenUrl": "https://acme.example/token",
                },
                "acme-oidc": { "kind": "oidc", "issuer": "https://id.acme.example" },
                "acme-jwt": {
                    "kind": "delegated",
                    "jwks": "https://acme.example/.well-known/jwks.json",
                    "issuer": "https://acme.example",
                },
                "acme-legacy": { "kind": "custom", "module": "./src/auth/legacy.ts" },
            }
        }))
        .lower()
        .expect("all four generic kinds lower without a preset");

        assert_eq!(registry.providers.len(), 4);
        assert_eq!(
            registry.provider("acme-legacy").unwrap().endpoints,
            Endpoints::Custom {
                module: "./src/auth/legacy.ts".to_string()
            }
        );
    }

    #[test]
    fn a_declared_kind_overrides_a_preset() {
        let registry = decl(serde_json::json!({
            "providers": {
                "google": { "kind": "delegated",
                            "jwks": "https://acme.example/jwks.json",
                            "issuer": "https://acme.example" }
            }
        }))
        .lower()
        .expect("lowers");
        assert_eq!(
            registry.provider("google").unwrap().kind,
            ProviderKind::Delegated
        );
    }

    #[test]
    fn a_delegated_preset_builds_its_urls_from_the_domain() {
        let registry = decl(serde_json::json!({
            "providers": { "clerk": { "domain": { "value": "acme.clerk.accounts.dev" } } }
        }))
        .lower()
        .expect("lowers");

        assert_eq!(
            registry.provider("clerk").unwrap().endpoints,
            Endpoints::Jwks {
                jwks: Url::parse("https://acme.clerk.accounts.dev/.well-known/jwks.json").unwrap(),
                issuer: "https://acme.clerk.accounts.dev".to_string(),
            }
        );
    }

    /// A JWKS with no issuer check accepts any token those keys happen to
    /// verify, which for a multi-tenant provider is every other tenant.
    #[test]
    fn a_delegated_provider_without_an_issuer_is_refused() {
        let error = decl(serde_json::json!({
            "providers": { "custom-jwt": { "kind": "delegated",
                                           "jwks": "https://acme.example/jwks.json" } }
        }))
        .lower()
        .expect_err("issuer is required");
        assert!(matches!(
            error,
            AuthSchemaError::MissingField { field: "issuer", .. }
        ));
    }

    #[test]
    fn a_custom_provider_without_a_module_is_refused() {
        let error = decl(serde_json::json!({
            "providers": { "legacy": { "kind": "custom" } }
        }))
        .lower()
        .expect_err("module is required");
        assert!(matches!(
            error,
            AuthSchemaError::MissingField { field: "module", .. }
        ));
    }

    #[test]
    fn plaintext_endpoints_are_refused_in_every_environment() {
        let error = decl(serde_json::json!({
            "providers": { "acme": { "kind": "oidc", "issuer": "http://id.acme.example" } }
        }))
        .lower()
        .expect_err("http is refused");
        assert!(matches!(
            error,
            AuthSchemaError::InvalidUrl { field: "issuer", .. }
        ));
    }

    /// The egress allowlist is derived, not maintained — the same rule APERTURE
    /// applies to `sources`.
    #[test]
    fn egress_hosts_are_derived_from_the_declaration() {
        let registry = decl(serde_json::json!({
            "providers": {
                "passkey": {},
                "github": {},
                "google": {},
                "clerk": { "domain": { "value": "acme.clerk.accounts.dev" } },
                "legacy": { "kind": "custom", "module": "./legacy.ts" },
            }
        }))
        .lower()
        .expect("lowers");

        let hosts = registry.egress_hosts();
        assert!(hosts.contains("github.com"));
        assert!(hosts.contains("api.github.com"));
        assert!(hosts.contains("accounts.google.com"));
        assert!(hosts.contains("acme.clerk.accounts.dev"));
        // A first-party or custom provider dials nothing, so it must widen the
        // allowlist by nothing.
        assert_eq!(hosts.len(), 4, "got {hosts:?}");
    }

    #[test]
    fn github_asks_for_the_scope_that_actually_returns_an_email() {
        let registry = decl(serde_json::json!({ "providers": { "github": {} } }))
            .lower()
            .expect("lowers");
        assert!(registry
            .provider("github")
            .unwrap()
            .scopes
            .contains(&"user:email".to_string()));
    }

    #[test]
    fn declared_scopes_replace_the_defaults() {
        let registry = decl(serde_json::json!({
            "providers": { "github": { "scopes": ["repo"] } }
        }))
        .lower()
        .expect("lowers");
        assert_eq!(registry.provider("github").unwrap().scopes, vec!["repo"]);
    }

    #[test]
    fn claim_map_defaults_to_oidc_names_and_is_overridable() {
        let registry = decl(serde_json::json!({
            "providers": { "acme": { "kind": "delegated",
                                     "jwks": "https://acme.example/jwks.json",
                                     "issuer": "https://acme.example",
                                     "claimMap": { "email": "mail" } } }
        }))
        .lower()
        .expect("lowers");
        let map = &registry.provider("acme").unwrap().claim_map;
        assert_eq!(map.get("subject").map(String::as_str), Some("sub"));
        assert_eq!(map.get("email").map(String::as_str), Some("mail"));
        assert_eq!(map.get("image").map(String::as_str), Some("picture"));
    }

    #[test]
    fn a_bare_string_secret_is_refused() {
        // The spelling that commits a production credential. It must not parse.
        let parsed: Result<AuthDeclaration, _> = serde_json::from_value(serde_json::json!({
            "providers": { "github": { "clientSecret": "sk_live_abcdef" } }
        }));
        assert!(parsed.is_err());
    }

    #[test]
    fn session_defaults_are_the_host_prefixed_cookie_and_thirty_days() {
        let registry = AuthDeclaration::default().lower().expect("lowers");
        assert_eq!(registry.session_cookie, DEFAULT_SESSION_COOKIE);
        assert_eq!(registry.session_ttl, DEFAULT_SESSION_TTL);
        assert!(registry.is_empty());
    }

    #[test]
    fn ttl_parses_every_documented_suffix() {
        for (raw, expected) in [
            ("30d", Duration::from_secs(30 * 86_400)),
            ("12h", Duration::from_secs(12 * 3_600)),
            ("90m", Duration::from_secs(90 * 60)),
            ("3600s", Duration::from_secs(3_600)),
            ("250ms", Duration::from_millis(250)),
        ] {
            assert_eq!(parse_duration(raw), Some(expected), "parsing `{raw}`");
        }
        assert_eq!(parse_duration("forever"), None);
        assert_eq!(parse_duration("30"), None);
    }

    #[test]
    fn an_unparseable_ttl_is_a_build_error() {
        let error = decl(serde_json::json!({ "session": { "ttl": "forever" } }))
            .lower()
            .expect_err("refused");
        assert_eq!(
            error,
            AuthSchemaError::InvalidTtl {
                value: "forever".to_string()
            }
        );
    }

    #[test]
    fn provider_names_are_checked_against_the_url_alphabet() {
        for name in ["Google", "acme provider", "acme/../admin", ""] {
            let decl = ProviderDecl {
                kind: Some(ProviderKind::Passkey),
                ..ProviderDecl::default()
            };
            assert!(
                matches!(decl.lower(name), Err(AuthSchemaError::InvalidName { .. })),
                "`{name}` must be refused"
            );
        }
    }

    #[test]
    fn a_cookie_name_with_separators_is_refused() {
        let error = decl(serde_json::json!({ "session": { "cookie": "my session" } }))
            .lower()
            .expect_err("refused");
        assert!(matches!(error, AuthSchemaError::InvalidCookie { .. }));
    }

    #[test]
    fn an_unset_environment_variable_is_reported_with_its_name() {
        let error = decl(serde_json::json!({
            "providers": { "clerk": { "domain": { "env": "ALBEDO_TEST_UNSET_CLERK_DOMAIN" } } }
        }))
        .lower()
        .expect_err("variable is not set");
        assert_eq!(
            error,
            AuthSchemaError::MissingEnv {
                provider: "clerk".to_string(),
                field: "domain",
                variable: "ALBEDO_TEST_UNSET_CLERK_DOMAIN".to_string(),
            }
        );
    }

    /// Lowering order must be declaration order, because [`AuthRegistry::egress_hosts`]
    /// derives from it and an allowlist that depends on map iteration order is a
    /// build that depends on the machine.
    #[test]
    fn lowering_order_is_stable() {
        let block = decl(serde_json::json!({
            "providers": { "google": {}, "github": {}, "passkey": {} }
        }));
        let first = block.lower().expect("lowers");
        let second = block.lower().expect("lowers");
        let names: Vec<&str> = first
            .providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect();
        assert_eq!(names, ["github", "google", "passkey"], "BTreeMap order");
        assert_eq!(first, second);
    }

    /// The declared `login` route must survive lowering. It was declared,
    /// documented and validated while `lower` hardcoded `None`, so the route
    /// gate could only ever see "no login route" and every gated page was a 401
    /// dead end — in the scaffold too, whose config comment promises otherwise.
    #[test]
    fn a_declared_login_route_reaches_the_registry() {
        let block = decl(serde_json::json!({
            "providers": { "password": {} },
            "login": "/sign-in"
        }));
        assert_eq!(
            block.lower().expect("lowers").login_path.as_deref(),
            Some("/sign-in")
        );
    }

    /// Absent is a legitimate choice — an app whose only gate is an API — and
    /// its fallback is the 401 the gate always gave.
    #[test]
    fn no_declared_login_route_lowers_to_none() {
        let block = decl(serde_json::json!({ "providers": { "password": {} } }));
        assert!(block.lower().expect("lowers").login_path.is_none());
    }

    /// 🔑 The declared route becomes a `Location` header, so it is held to the
    /// same rule as a request-supplied return path. Each of these is a real
    /// bypass shape, and "authenticate, then get bounced somewhere else" is the
    /// credential-phishing chain with the victim already primed to trust the
    /// destination.
    #[test]
    fn a_login_route_that_is_not_a_rooted_app_path_is_refused() {
        for hostile in [
            "https://evil.example/login",
            "//evil.example/login",
            "/\\evil.example/login",
            "sign-in",
            "/sign in",
            "",
        ] {
            let block = decl(serde_json::json!({
                "providers": { "password": {} },
                "login": hostile
            }));
            assert!(
                matches!(
                    block.lower(),
                    Err(AuthSchemaError::InvalidLoginPath { .. })
                ),
                "{hostile:?} must not lower into a Location header"
            );
        }
    }
}
