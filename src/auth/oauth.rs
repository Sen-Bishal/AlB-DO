//! AUTH · 15.4 — the OAuth 2.0 authorization-code flow.
//!
//! `declare.rs` has carried [`ProviderKind::OAuth`], both endpoints and a
//! preset table since P0, and until now **nothing consumed any of it**: the
//! server crate had zero occurrences of the word. This module is the consumer.
//!
//! ## Why it is pure
//!
//! Every function here is a value in, a value out. Nothing opens a socket,
//! reads the clock or touches the substrate. The flow is three network calls
//! (authorize redirect, token exchange, profile read) with a decision between
//! each, and it is the *decisions* that are worth testing — a wrong redirect
//! URI, a state compared with `==`, a subject read from a claim the server
//! never sends. Keeping the calls out means all of that is reachable from a
//! unit test with no fixture server, and the HTTP is a thin shell in
//! `albedo-server` that rides APERTURE like every other outbound call.
//!
//! ## What guards the callback
//!
//! Two things, and they do different jobs:
//!
//! - **`state`** stops a cross-site request forgery — an attacker completing *their* login in
//!   *your* browser, silently attaching your account to theirs. It is minted here, stored in an
//!   `HttpOnly` cookie by the caller, and compared against the query parameter on return. The
//!   comparison is [`constant_time_eq`], not `==`.
//! - **PKCE** ([RFC 7636]) stops an intercepted authorization code from being redeemed by anyone
//!   but us. A public client needs it; a confidential one with a client secret does not strictly,
//!   and it is sent anyway because a server that does not implement it ignores the two extra
//!   parameters — so there is no configuration in which sending it is wrong.
//!
//! Neither is optional and neither is a knob. `AUTH.md`'s rule that the safe
//! thing and the ergonomic thing are the same thing applies exactly here: the
//! only flow this module can construct is the guarded one.
//!
//! ## The two real-world details that are not in the RFC
//!
//! Both come from the providers' own documentation rather than the spec, and
//! both are silent failures:
//!
//! - **GitHub's token endpoint answers `application/x-www-form-urlencoded` unless you send
//!   `Accept: application/json`.** A JSON-only parser gets a `SyntaxError` on a *successful*
//!   exchange. [`parse_token_response`] therefore accepts both encodings rather than trusting the
//!   header to be honoured.
//! - **GitHub's API refuses a request with no `User-Agent`** — `403`, on a token that is perfectly
//!   valid. [`profile_request`] always sets one.
//!
//! [RFC 7636]: https://datatracker.ietf.org/doc/html/rfc7636

use crate::auth::declare::{Endpoints, ResolvedProvider, SecretDecl};
use crate::auth::store::ProviderProfile;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::RngCore;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use url::Url;

/// Bytes of entropy behind `state` and behind the PKCE verifier.
///
/// 32 bytes = 256 bits, the same figure as a session token and for the same
/// reason: both are single-use secrets compared for equality, and the cost of
/// over-provisioning is nothing. RFC 7636 § 4.1 requires a verifier of 43–128
/// characters, and 32 bytes base64url-encodes to exactly 43.
const ENTROPY_BYTES: usize = 32;

/// How long a started flow may take to come back.
///
/// Ten minutes is long enough for a person to find their password manager,
/// approve a consent screen and clear an MFA prompt, and short enough that an
/// abandoned flow's cookie is not lying around all day. Enforced by the
/// caller's cookie `Max-Age`, which is why it lives beside the flow it bounds
/// rather than in the HTTP shell.
pub const FLOW_TTL_SECONDS: i64 = 600;

/// The `User-Agent` every outbound call in this flow carries.
const USER_AGENT: &str = concat!("albedo/", env!("CARGO_PKG_VERSION"));

/// Anything that can go wrong between the redirect and the principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthError {
    /// The provider has no browser leg — a passkey or password provider, by
    /// construction.
    NotAnOAuthProvider {
        /// The declared name.
        provider: String,
    },
    /// `kind: "oidc"` whose discovery document has not been fetched yet.
    ///
    /// Not a misconfiguration: [`Endpoints::Discovered`] records an issuer
    /// precisely because lowering is synchronous and cannot make the call.
    NeedsDiscovery {
        /// The declared name.
        provider: String,
        /// Where the document lives.
        discovery_url: String,
    },
    /// A required credential was declared but is unset in the environment.
    MissingCredential {
        /// The declared name.
        provider: String,
        /// Which one.
        field: &'static str,
    },
    /// The discovery document parsed but did not name an endpoint we need.
    IncompleteDiscovery {
        /// Which field was missing or unusable.
        field: &'static str,
    },
    /// The authorization server reported a failure, in its own words.
    Provider {
        /// The `error` code from the server.
        code: String,
        /// The `error_description`, when there was one.
        description: Option<String>,
    },
    /// The callback's `state` did not match the one we minted.
    StateMismatch,
    /// The callback arrived with no pending flow — an expired cookie, a
    /// bookmarked callback URL, or a request that never started here.
    NoPendingFlow,
    /// A response body did not parse, or parsed and was missing something.
    Malformed {
        /// What we were reading.
        what: &'static str,
        /// Why it did not work.
        reason: String,
    },
    /// The profile document had no value at the claim the map names.
    ///
    /// The one failure that names both halves, because the fix is always to
    /// change one of them and the message has to say which two to compare.
    SubjectNotFound {
        /// The claim the map named.
        claim: String,
        /// The keys the document actually had, sorted.
        available: Vec<String>,
    },
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnOAuthProvider { provider } => {
                write!(f, "provider `{provider}` is not an OAuth or OIDC provider")
            }
            Self::NeedsDiscovery {
                provider,
                discovery_url,
            } => write!(
                f,
                "provider `{provider}` needs its discovery document from {discovery_url} first"
            ),
            Self::MissingCredential { provider, field } => write!(
                f,
                "provider `{provider}` declares no {field}, or its environment variable is unset"
            ),
            Self::IncompleteDiscovery { field } => {
                write!(f, "the discovery document has no usable `{field}`")
            }
            Self::Provider { code, description } => match description {
                Some(description) => write!(f, "the provider refused: {code} — {description}"),
                None => write!(f, "the provider refused: {code}"),
            },
            Self::StateMismatch => write!(f, "the sign-in state did not match"),
            Self::NoPendingFlow => write!(f, "there is no sign-in in progress"),
            Self::Malformed { what, reason } => write!(f, "could not read the {what}: {reason}"),
            Self::SubjectNotFound { claim, available } => write!(
                f,
                "the profile has no `{claim}` to use as the subject; it has: {}",
                available.join(", ")
            ),
        }
    }
}

impl std::error::Error for OAuthError {}

type Result<T> = std::result::Result<T, OAuthError>;

/// The three URLs a flow needs, all resolved.
///
/// Separate from [`Endpoints`] because that enum records what the *author*
/// declared, including the `Discovered` case that is an issuer and not yet an
/// endpoint. Nothing downstream should have to re-ask whether discovery has
/// happened, so the type that can answer "not yet" is not the type the flow
/// takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthEndpoints {
    /// Where the browser is sent.
    pub authorize: Url,
    /// Where the code is exchanged.
    pub token: Url,
    /// Where the profile is read. `None` for an issuer that publishes no
    /// `userinfo_endpoint`, which is legitimate for a pure OIDC server whose
    /// claims all ride the id token.
    pub userinfo: Option<Url>,
}

impl OAuthEndpoints {
    /// The endpoints a provider already carries, if it carries them.
    ///
    /// # Errors
    /// [`OAuthError::NotAnOAuthProvider`] for a kind with no browser leg, and
    /// [`OAuthError::NeedsDiscovery`] for an issuer whose document has not been
    /// read yet — the caller answers that one by fetching and calling
    /// [`Self::from_discovery_document`].
    pub fn resolve(provider: &ResolvedProvider) -> Result<Self> {
        match &provider.endpoints {
            Endpoints::OAuth {
                authorize_url,
                token_url,
                userinfo_url,
            } => Ok(Self {
                authorize: authorize_url.clone(),
                token: token_url.clone(),
                userinfo: userinfo_url.clone(),
            }),
            Endpoints::Discovered { issuer } => Err(OAuthError::NeedsDiscovery {
                provider: provider.name.clone(),
                discovery_url: discovery_url(issuer),
            }),
            Endpoints::FirstParty | Endpoints::Jwks { .. } | Endpoints::Custom { .. } => {
                Err(OAuthError::NotAnOAuthProvider {
                    provider: provider.name.clone(),
                })
            }
        }
    }

    /// Read an OIDC `.well-known/openid-configuration`.
    ///
    /// # Errors
    /// [`OAuthError::Malformed`] if the body is not JSON, and
    /// [`OAuthError::IncompleteDiscovery`] if it omits an endpoint the flow
    /// cannot proceed without.
    pub fn from_discovery_document(body: &[u8]) -> Result<Self> {
        let document: JsonValue =
            serde_json::from_slice(body).map_err(|err| OAuthError::Malformed {
                what: "discovery document",
                reason: err.to_string(),
            })?;
        let required = |field: &'static str| -> Result<Url> {
            document
                .get(field)
                .and_then(JsonValue::as_str)
                .and_then(|raw| Url::parse(raw).ok())
                .ok_or(OAuthError::IncompleteDiscovery { field })
        };
        Ok(Self {
            authorize: required("authorization_endpoint")?,
            token: required("token_endpoint")?,
            userinfo: document
                .get("userinfo_endpoint")
                .and_then(JsonValue::as_str)
                .and_then(|raw| Url::parse(raw).ok()),
        })
    }
}

/// Where an issuer's discovery document lives.
///
/// The path is fixed by OIDC Discovery § 4 and is **appended** to the issuer
/// rather than replacing its path: the `microsoft` preset's issuer is
/// `https://login.microsoftonline.com/common/v2.0`, and [`Url::join`] would
/// have silently discarded `/common/v2.0` and fetched a document that does not
/// exist.
#[must_use]
pub fn discovery_url(issuer: &Url) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    )
}

/// A started flow: what to send the browser to, and what to remember.
///
/// The two secrets are returned rather than stored because storage is the
/// caller's decision — the HTTP shell puts them in one `HttpOnly` cookie, and a
/// test puts them in a variable.
#[derive(Debug, Clone)]
pub struct FlowStart {
    /// Where to redirect the browser.
    pub authorize_url: String,
    /// The CSRF value to compare on return.
    pub state: String,
    /// The PKCE verifier to present at the token endpoint.
    pub verifier: String,
}

/// Begin an authorization-code flow.
///
/// `redirect_uri` must be byte-identical to the one presented at the token
/// endpoint — the authorization server compares them, and a mismatch is one of
/// the two errors every first OAuth integration hits. [`token_request`] takes
/// it again for that reason rather than this struct remembering it: the
/// requirement is then visible at both call sites instead of hidden in a field.
///
/// # Errors
/// [`OAuthError::MissingCredential`] when no client id resolves.
pub fn begin(
    provider: &ResolvedProvider,
    endpoints: &OAuthEndpoints,
    redirect_uri: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<FlowStart> {
    let client_id = credential(provider, provider.client_id.as_ref(), "clientId", &env)?;
    let state = random_token();
    let verifier = random_token();

    let mut url = endpoints.authorize.clone();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &client_id);
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("state", &state);
        query.append_pair("code_challenge", &pkce_challenge(&verifier));
        query.append_pair("code_challenge_method", "S256");
        if !provider.scopes.is_empty() {
            query.append_pair("scope", &provider.scopes.join(" "));
        }
    }

    Ok(FlowStart {
        authorize_url: url.into(),
        state,
        verifier,
    })
}

/// One outbound call, described but not made.
///
/// The seam between this module and APERTURE. `albedo-server` turns it into an
/// `ApertureRequest`; a test reads the fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundRequest {
    /// HTTP method.
    pub method: &'static str,
    /// Absolute URL.
    pub url: String,
    /// Headers, in the order they should be sent.
    pub headers: Vec<(String, String)>,
    /// Body, for a `POST`.
    pub body: Option<Vec<u8>>,
}

/// Build the token exchange.
///
/// # Errors
/// [`OAuthError::MissingCredential`] when the client id or secret does not
/// resolve.
pub fn token_request(
    provider: &ResolvedProvider,
    endpoints: &OAuthEndpoints,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<OutboundRequest> {
    let client_id = credential(provider, provider.client_id.as_ref(), "clientId", &env)?;
    let client_secret = credential(
        provider,
        provider.client_secret.as_ref(),
        "clientSecret",
        &env,
    )?;

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", &client_id)
        .append_pair("client_secret", &client_secret)
        .append_pair("code_verifier", verifier)
        .finish();

    Ok(OutboundRequest {
        method: "POST",
        url: endpoints.token.to_string(),
        headers: vec![
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            // Not politeness. GitHub answers form-encoded without it; see the
            // module docs. `parse_token_response` copes either way, and asking
            // for the encoding we want is still the right request to send.
            ("accept".to_string(), "application/json".to_string()),
            ("user-agent".to_string(), USER_AGENT.to_string()),
        ],
        body: Some(body.into_bytes()),
    })
}

/// What a token endpoint gave back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenResponse {
    /// The bearer token for the profile read.
    pub access_token: String,
    /// The OIDC id token, when the server issued one.
    pub id_token: Option<String>,
}

/// Read a token response in either encoding.
///
/// # Errors
/// [`OAuthError::Provider`] when the body is a well-formed OAuth error — which
/// is the *usual* shape of a failure here, and arrives with HTTP 200 from more
/// than one popular server, so the status line alone must not decide.
/// [`OAuthError::Malformed`] when it is neither.
pub fn parse_token_response(body: &[u8]) -> Result<TokenResponse> {
    let fields = decode_body(body).ok_or_else(|| OAuthError::Malformed {
        what: "token response",
        reason: "body is neither JSON nor form-encoded".to_string(),
    })?;

    if let Some(code) = fields.get("error") {
        return Err(OAuthError::Provider {
            code: code.clone(),
            description: fields.get("error_description").cloned(),
        });
    }

    let access_token = fields
        .get("access_token")
        .cloned()
        .ok_or_else(|| OAuthError::Malformed {
            what: "token response",
            reason: "no `access_token`".to_string(),
        })?;

    Ok(TokenResponse {
        access_token,
        id_token: fields.get("id_token").cloned(),
    })
}

/// Build the profile read.
///
/// # Errors
/// [`OAuthError::IncompleteDiscovery`] when the provider publishes no
/// `userinfo` endpoint — this names the absent field rather than returning an
/// empty profile that would land in the database as a real one.
pub fn profile_request(
    endpoints: &OAuthEndpoints,
    token: &TokenResponse,
) -> Result<OutboundRequest> {
    let userinfo = endpoints
        .userinfo
        .as_ref()
        .ok_or(OAuthError::IncompleteDiscovery {
            field: "userinfo_endpoint",
        })?;
    Ok(OutboundRequest {
        method: "GET",
        url: userinfo.to_string(),
        headers: vec![
            (
                "authorization".to_string(),
                format!("Bearer {}", token.access_token),
            ),
            ("accept".to_string(), "application/json".to_string()),
            // GitHub answers 403 without one, on a token that is valid.
            ("user-agent".to_string(), USER_AGENT.to_string()),
        ],
        body: None,
    })
}

/// Turn a profile document into the subject and the profile we store.
///
/// The claim map is the only thing that decides where each field comes from —
/// there is no per-provider branch here, because a branch is what a claim map
/// exists to avoid.
///
/// # Errors
/// [`OAuthError::Malformed`] if the body is not a JSON object.
/// [`OAuthError::SubjectNotFound`] if the mapped subject claim is absent, which
/// is the one field with no sensible default.
pub fn profile_from_document(
    claim_map: &BTreeMap<String, String>,
    body: &[u8],
) -> Result<(String, ProviderProfile)> {
    let document: JsonValue = serde_json::from_slice(body).map_err(|err| OAuthError::Malformed {
        what: "profile",
        reason: err.to_string(),
    })?;
    let JsonValue::Object(claims) = document else {
        return Err(OAuthError::Malformed {
            what: "profile",
            reason: "not a JSON object".to_string(),
        });
    };

    let subject_claim = claim_map
        .get("subject")
        .map_or("sub", String::as_str)
        .to_string();
    let subject = claims
        .get(&subject_claim)
        .and_then(scalar_to_string)
        .ok_or_else(|| OAuthError::SubjectNotFound {
            claim: subject_claim,
            available: claims.keys().cloned().collect(),
        })?;

    let field = |name: &str| -> Option<String> {
        claim_map
            .get(name)
            .and_then(|claim| claims.get(claim))
            .and_then(scalar_to_string)
    };

    Ok((
        subject,
        ProviderProfile {
            email: field("email"),
            name: field("name"),
            image: field("image"),
            // The whole document, so a later feature can read a claim we did
            // not map without a second round trip to the provider.
            claims,
        },
    ))
}

/// Compare a returned `state` against the one we minted.
///
/// # Errors
/// [`OAuthError::StateMismatch`], which is deliberately the *only* thing the
/// caller learns. A caller able to tell "wrong" from "empty" from "absent"
/// could use the callback to probe.
pub fn verify_state(expected: &str, presented: &str) -> Result<()> {
    if expected.is_empty() || !constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
        return Err(OAuthError::StateMismatch);
    }
    Ok(())
}

/// Equality that does not leak how much of the value matched.
///
/// `state` is a secret compared against attacker-supplied input, which is the
/// exact shape an early-exit `==` leaks. Written out rather than pulled from a
/// crate because it is six lines and the dependency would be the larger risk.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // `fold` rather than `all`, so every byte is read whatever the first one
    // says. An `&&` here would reintroduce the early exit this exists to avoid.
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// 256 bits from the OS CSPRNG, base64url-encoded.
fn random_token() -> String {
    let mut bytes = [0u8; ENTROPY_BYTES];
    // `rand::thread_rng` is a CSPRNG seeded from the OS entropy source — the
    // same generator `SessionToken::mint` uses, and for the same reason.
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `BASE64URL(SHA256(ASCII(verifier)))` — RFC 7636 § 4.2, method `S256`.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Resolve a declared secret, turning "declared but unset" into one error.
fn credential(
    provider: &ResolvedProvider,
    decl: Option<&SecretDecl>,
    field: &'static str,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<String> {
    let decl = decl.ok_or(OAuthError::MissingCredential {
        provider: provider.name.clone(),
        field,
    })?;
    match decl {
        SecretDecl::Value { value } => Ok(value.clone()),
        SecretDecl::Env { env: name } => env(name).ok_or(OAuthError::MissingCredential {
            provider: provider.name.clone(),
            field,
        }),
    }
}

/// Flatten a JSON object or a form-encoded body to string fields.
///
/// JSON is tried first because a server that honours `Accept` sends it; the
/// form-encoded arm is the GitHub case. Nested values are dropped rather than
/// stringified — every field a token response carries is scalar, and a nested
/// one would be something we do not understand.
fn decode_body(body: &[u8]) -> Option<BTreeMap<String, String>> {
    if let Ok(JsonValue::Object(object)) = serde_json::from_slice::<JsonValue>(body) {
        return Some(
            object
                .into_iter()
                .filter_map(|(key, value)| scalar_to_string(&value).map(|value| (key, value)))
                .collect(),
        );
    }
    let text = std::str::from_utf8(body).ok()?;
    let pairs: BTreeMap<String, String> = url::form_urlencoded::parse(text.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    // An arbitrary blob percent-decodes to *something*, so emptiness is the
    // only honest signal that this was not a form body.
    (!pairs.is_empty()).then_some(pairs)
}

/// A JSON scalar as a string.
///
/// Numbers matter, and are why this is not `as_str`: GitHub, GitLab and Discord
/// all return an integer `id`, and that integer is the subject the whole account
/// mapping is keyed on. `as_str` on it yields `None`, which
/// [`profile_from_document`] would then report as a missing claim on a document
/// that plainly contains it.
fn scalar_to_string(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Number(number) => Some(number.to_string()),
        JsonValue::Bool(flag) => Some(flag.to_string()),
        JsonValue::Null | JsonValue::Array(_) | JsonValue::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::declare::AuthDeclaration;
    use crate::auth::AuthRegistry;

    /// Lower a one-provider `auth` block, the way boot does.
    fn registry(provider: &str, extra: serde_json::Value) -> AuthRegistry {
        let mut decl = serde_json::json!({
            "clientId": { "value": "cid" },
            "clientSecret": { "value": "shh" }
        });
        let (serde_json::Value::Object(base), serde_json::Value::Object(extra)) =
            (&mut decl, extra)
        else {
            panic!("both must be objects");
        };
        base.extend(extra);
        let block = serde_json::json!({ "providers": { provider: decl } });
        serde_json::from_value::<AuthDeclaration>(block)
            .expect("parses")
            .lower()
            .expect("lowers")
    }

    fn github() -> AuthRegistry {
        registry("github", serde_json::json!({}))
    }

    fn endpoints(registry: &AuthRegistry, name: &str) -> OAuthEndpoints {
        OAuthEndpoints::resolve(registry.provider(name).expect("declared")).expect("resolves")
    }

    #[test]
    fn a_first_party_provider_has_no_oauth_flow() {
        let block = serde_json::json!({ "providers": { "password": {} } });
        let registry = serde_json::from_value::<AuthDeclaration>(block)
            .expect("parses")
            .lower()
            .expect("lowers");
        assert!(matches!(
            OAuthEndpoints::resolve(registry.provider("password").unwrap()),
            Err(OAuthError::NotAnOAuthProvider { .. })
        ));
    }

    /// An OIDC provider is not *broken*, it is *not yet resolved*. The
    /// distinction is the whole reason `Endpoints::Discovered` exists, and the
    /// error carries the URL the caller has to fetch so it cannot be rebuilt
    /// wrongly at the call site.
    #[test]
    fn an_oidc_provider_asks_for_its_discovery_document_and_says_where() {
        let registry = registry("google", serde_json::json!({}));
        let Err(OAuthError::NeedsDiscovery { discovery_url, .. }) =
            OAuthEndpoints::resolve(registry.provider("google").unwrap())
        else {
            panic!("an issuer with no document must ask for one");
        };
        assert_eq!(
            discovery_url,
            "https://accounts.google.com/.well-known/openid-configuration"
        );
    }

    /// The bug `Url::join` would have caused, pinned: Microsoft's issuer has a
    /// path, and joining an absolute path onto it discards that path.
    #[test]
    fn discovery_appends_to_an_issuer_path_instead_of_replacing_it() {
        let issuer = Url::parse("https://login.microsoftonline.com/common/v2.0").unwrap();
        assert_eq!(
            discovery_url(&issuer),
            "https://login.microsoftonline.com/common/v2.0/.well-known/openid-configuration"
        );
        // A trailing slash must not double up.
        let slashed = Url::parse("https://accounts.google.com/").unwrap();
        assert_eq!(
            discovery_url(&slashed),
            "https://accounts.google.com/.well-known/openid-configuration"
        );
    }

    #[test]
    fn a_discovery_document_yields_the_three_endpoints() {
        let body = br#"{
            "authorization_endpoint": "https://issuer.example/authorize",
            "token_endpoint": "https://issuer.example/token",
            "userinfo_endpoint": "https://issuer.example/userinfo"
        }"#;
        let resolved = OAuthEndpoints::from_discovery_document(body).expect("reads");
        assert_eq!(resolved.token.as_str(), "https://issuer.example/token");
        assert!(resolved.userinfo.is_some());
    }

    /// A document missing `token_endpoint` must fail here, at the boundary,
    /// naming the field — not three calls later as an unexplained refusal.
    #[test]
    fn a_discovery_document_without_a_token_endpoint_is_refused_by_name() {
        let body = br#"{"authorization_endpoint": "https://issuer.example/authorize"}"#;
        assert_eq!(
            OAuthEndpoints::from_discovery_document(body),
            Err(OAuthError::IncompleteDiscovery {
                field: "token_endpoint"
            })
        );
    }

    #[test]
    fn begin_builds_a_guarded_authorize_url() {
        let registry = github();
        let start = begin(
            registry.provider("github").unwrap(),
            &endpoints(&registry, "github"),
            "https://app.example/_albedo/auth/oauth/github/callback",
            |_| None,
        )
        .expect("begins");

        let url = Url::parse(&start.authorize_url).expect("a URL");
        let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.get("response_type").unwrap(), "code");
        assert_eq!(params.get("client_id").unwrap(), "cid");
        assert_eq!(
            params.get("redirect_uri").unwrap(),
            "https://app.example/_albedo/auth/oauth/github/callback"
        );
        assert_eq!(params.get("state").unwrap(), &start.state);
        assert_eq!(params.get("code_challenge_method").unwrap(), "S256");
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some("read:user user:email"),
            "the preset's scopes must reach the authorize URL"
        );
    }

    /// PKCE is not a knob. There is no argument to turn it off, so the only
    /// URL this module can build carries a challenge derived from the verifier
    /// it hands back — and the derivation is the one RFC 7636 § 4.2 specifies.
    #[test]
    fn the_challenge_is_the_s256_hash_of_the_verifier_it_returns() {
        let registry = github();
        let start = begin(
            registry.provider("github").unwrap(),
            &endpoints(&registry, "github"),
            "https://app.example/cb",
            |_| None,
        )
        .expect("begins");
        let url = Url::parse(&start.authorize_url).unwrap();
        let params: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("code_challenge").unwrap(),
            &URL_SAFE_NO_PAD.encode(Sha256::digest(start.verifier.as_bytes()))
        );
        // RFC 7636 § 4.1: 43–128 characters.
        assert_eq!(start.verifier.len(), 43);
    }

    /// The RFC's own worked example, so the encoding is checked against
    /// something outside this file. RFC 7636 Appendix B.
    #[test]
    fn the_s256_transform_matches_the_rfc_test_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn two_flows_never_share_a_state() {
        let registry = github();
        let provider = registry.provider("github").unwrap();
        let endpoints = endpoints(&registry, "github");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            let start = begin(provider, &endpoints, "https://app.example/cb", |_| None)
                .expect("begins");
            assert!(seen.insert(start.state.clone()), "the CSPRNG repeated");
            assert!(seen.insert(start.verifier), "verifier collided with a state");
        }
    }

    #[test]
    fn an_unset_client_id_names_the_provider_and_the_field() {
        let block = serde_json::json!({
            "providers": { "github": { "clientId": { "env": "NOT_SET_ANYWHERE" },
                                       "clientSecret": { "value": "shh" } } }
        });
        let registry = serde_json::from_value::<AuthDeclaration>(block)
            .expect("parses")
            .lower()
            .expect("lowers");
        let outcome = begin(
            registry.provider("github").unwrap(),
            &endpoints(&registry, "github"),
            "https://app.example/cb",
            |_| None,
        );
        assert!(matches!(
            outcome.as_ref().err(),
            Some(OAuthError::MissingCredential {
                provider,
                field: "clientId"
            }) if provider == "github"
        ));
    }

    #[test]
    fn the_token_request_carries_the_verifier_and_the_same_redirect_uri() {
        let registry = github();
        let request = token_request(
            registry.provider("github").unwrap(),
            &endpoints(&registry, "github"),
            "the-code",
            "the-verifier",
            "https://app.example/cb",
            |_| None,
        )
        .expect("builds");

        assert_eq!(request.method, "POST");
        let body = String::from_utf8(request.body.clone().unwrap()).unwrap();
        let fields: BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(fields.get("grant_type").unwrap(), "authorization_code");
        assert_eq!(fields.get("code").unwrap(), "the-code");
        assert_eq!(fields.get("code_verifier").unwrap(), "the-verifier");
        assert_eq!(fields.get("redirect_uri").unwrap(), "https://app.example/cb");
        assert_eq!(fields.get("client_secret").unwrap(), "shh");
    }

    /// GitHub's default. A JSON-only parser raises a syntax error on a
    /// *successful* exchange, which reads as "the code was bad" and is not.
    #[test]
    fn a_form_encoded_token_response_parses() {
        let body = b"access_token=gho_abc&scope=read%3Auser&token_type=bearer";
        assert_eq!(
            parse_token_response(body).expect("parses").access_token,
            "gho_abc"
        );
    }

    #[test]
    fn a_json_token_response_parses_and_keeps_the_id_token() {
        let body = br#"{"access_token":"at","token_type":"Bearer","id_token":"idt"}"#;
        let token = parse_token_response(body).expect("parses");
        assert_eq!(token.access_token, "at");
        assert_eq!(token.id_token.as_deref(), Some("idt"));
    }

    /// More than one popular server answers an *error* with HTTP 200, so the
    /// body has to be what decides. A parser that only looked at the status
    /// would hand a nonexistent `access_token` to the next step.
    #[test]
    fn an_oauth_error_body_is_a_refusal_whatever_the_status_line_said() {
        let body = br#"{"error":"bad_verification_code","error_description":"expired"}"#;
        assert_eq!(
            parse_token_response(body),
            Err(OAuthError::Provider {
                code: "bad_verification_code".to_string(),
                description: Some("expired".to_string())
            })
        );
    }

    /// The defect this whole claim-map layering exists to prevent, end to end:
    /// GitHub's subject is an integer, and `as_str` on it is `None`.
    #[test]
    fn a_github_profile_yields_its_integer_id_as_the_subject() {
        let registry = github();
        let claim_map = &registry.provider("github").unwrap().claim_map;
        let body = br#"{
            "id": 1024,
            "login": "octocat",
            "name": "The Octocat",
            "email": "octo@example.com",
            "avatar_url": "https://avatars.example/octo.png"
        }"#;
        let (subject, profile) = profile_from_document(claim_map, body).expect("reads");
        assert_eq!(subject, "1024");
        assert_eq!(profile.name.as_deref(), Some("The Octocat"));
        assert_eq!(profile.email.as_deref(), Some("octo@example.com"));
        assert_eq!(
            profile.image.as_deref(),
            Some("https://avatars.example/octo.png")
        );
    }

    /// GitHub returns `"email": null` for an account that has not made an
    /// address public. That is an ordinary outcome — `albedo_users.email` is
    /// nullable and deliberately not unique — and it must not read as the
    /// string `"null"`.
    #[test]
    fn a_null_claim_is_absent_rather_than_the_word_null() {
        let registry = github();
        let claim_map = &registry.provider("github").unwrap().claim_map;
        let body = br#"{"id": 7, "email": null, "name": "Anon"}"#;
        let (_, profile) = profile_from_document(claim_map, body).expect("reads");
        assert_eq!(profile.email, None);
    }

    /// The error a person can act on: it names the claim we looked for *and*
    /// the keys the document actually had, because the fix is always to change
    /// one of the two.
    #[test]
    fn a_missing_subject_names_both_halves_of_the_mismatch() {
        let registry = github();
        let claim_map = &registry.provider("github").unwrap().claim_map;
        let body = br#"{"user_id": 7, "name": "Anon"}"#;
        let Err(OAuthError::SubjectNotFound { claim, available }) =
            profile_from_document(claim_map, body)
        else {
            panic!("a profile with no subject must be refused");
        };
        assert_eq!(claim, "id");
        assert_eq!(available, vec!["name".to_string(), "user_id".to_string()]);
    }

    #[test]
    fn the_profile_request_is_a_bearer_read_with_a_user_agent() {
        let registry = github();
        let request = profile_request(
            &endpoints(&registry, "github"),
            &TokenResponse {
                access_token: "at".to_string(),
                id_token: None,
            },
        )
        .expect("builds");
        assert_eq!(request.method, "GET");
        assert_eq!(request.url, "https://api.github.com/user");
        let headers: BTreeMap<_, _> = request.headers.into_iter().collect();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer at");
        assert!(
            headers.contains_key("user-agent"),
            "GitHub answers 403 without one"
        );
    }

    #[test]
    fn state_verification_accepts_only_the_minted_value() {
        assert!(verify_state("abc", "abc").is_ok());
        assert_eq!(verify_state("abc", "abd"), Err(OAuthError::StateMismatch));
        assert_eq!(verify_state("abc", "ab"), Err(OAuthError::StateMismatch));
    }

    /// The one that matters: an *absent* pending state must not make every
    /// callback valid. `"" == ""` is true, and that is the whole bypass.
    #[test]
    fn an_empty_expected_state_never_matches() {
        assert_eq!(verify_state("", ""), Err(OAuthError::StateMismatch));
    }

    #[test]
    fn constant_time_eq_agrees_with_equality() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sane"));
        assert!(!constant_time_eq(b"same", b"same-but-longer"));
        assert!(constant_time_eq(b"", b""));
    }
}
