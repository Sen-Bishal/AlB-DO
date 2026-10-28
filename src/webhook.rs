//! Inbound webhooks — a provider's signed `POST`, verified, then handed to a job.
//!
//! ```ts
//! // albedo.config.ts
//! webhooks: {
//!   stripe: {
//!     path: "/hooks/stripe",
//!     signature: "stripe",
//!     secret: { env: "STRIPE_WEBHOOK_SECRET" },
//!     job: "stripe_event",          // an export of src/jobs.ts
//!   },
//! }
//! ```
//!
//! ## What happens to a delivery
//!
//! 1. The body is read (at most [`MAX_BODY_BYTES`]) and its **signature is checked
//!    against the raw bytes** — before anything parses them. A payment provider's
//!    signature is the only thing that makes a `POST` from the internet mean
//!    anything; parsing first would hand unauthenticated input to a JSON parser
//!    for no reason.
//! 2. The body is parsed as a JSON object and its **event id** read — from the
//!    body or a header, per [`SignatureScheme`].
//! 3. The declared job is queued with the event as its `args` and
//!    `webhook:{name}:{event id}` as its **queue id**. The jobs queue inserts on
//!    that id with `OR IGNORE`, so a provider's retry of an event already
//!    received is a no-op — exactly-once is the queue's property, inherited, not
//!    rebuilt here.
//! 4. `200` once the row is durable. A provider that sees anything else retries,
//!    so an enqueue that failed is a `500` on purpose.
//!
//! The job runs with retries, backoff and dead-lettering like any other, and
//! **for no principal** (`user` is `null`): a webhook is a third party speaking,
//! not a person. Whose row a subscription event updates comes from the event —
//! a customer reference the app put there at checkout — and the signature is
//! what makes that reference trustworthy.
//!
//! ## Why a job, and not a handler that answers the request
//!
//! Providers time a delivery out in seconds and retry it for days. A handler that
//! did the work inline would turn a slow write into a retry storm and a crashed
//! process into a lost event; a queued row survives both, and its id collapses
//! every retry onto one run.
//!
//! ## Refused, at boot or build
//!
//! A path outside `/`, under `/_albedo/`, or claimed twice; a secret not read
//! from the environment; a `header` on a scheme that fixes its own; an `id` or
//! `header` missing where the scheme needs one; a job `src/jobs.ts` does not
//! declare (checked by the server, which holds the jobs declaration).

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::fmt;

/// The largest body a webhook accepts. Provider events are kilobytes; this
/// bounds what an unauthenticated caller can make the server buffer before the
/// signature has been checked.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// Default window, either side of now, for a scheme that signs a timestamp.
/// Stripe's own libraries use five minutes.
pub const DEFAULT_TOLERANCE_SECONDS: u64 = 300;

/// The longest event id kept. Real ones are tens of characters; this keeps a
/// hostile-but-signed body from minting an unbounded queue id.
const MAX_EVENT_ID_BYTES: usize = 200;

/// How a provider signs its deliveries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureScheme {
    /// `Stripe-Signature: t=…,v1=…` over `"{t}.{body}"`. Event id: body `id`.
    Stripe,
    /// Paddle Billing — `Paddle-Signature: ts=…;h1=…` over `"{ts}:{body}"`.
    /// Event id: body `event_id`.
    Paddle,
    /// `X-Hub-Signature-256: sha256=…` over the body. Event id: the
    /// `X-GitHub-Delivery` header.
    Github,
    /// `X-Razorpay-Signature: …` over the body. Event id: the
    /// `X-Razorpay-Event-Id` header.
    Razorpay,
    /// Any provider that sends a hex HMAC-SHA256 of the body in a header of its
    /// choosing — declare `header`, and where the event id is (`id`).
    HmacSha256,
}

/// A secret read from the environment — the only form a signing secret takes.
///
/// No literal alternative, unlike an OAuth client id: a webhook secret in a
/// config file is a secret in version control, and anyone holding it can mint
/// events the app will believe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvSecret {
    /// The variable name.
    pub env: String,
}

/// One entry of the `webhooks` block, as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookDecl {
    /// The path the provider posts to — `/hooks/stripe`.
    pub path: String,
    /// How it signs.
    pub signature: SignatureScheme,
    /// The signing secret.
    pub secret: EnvSecret,
    /// The job in `src/jobs.ts` that each delivery queues.
    pub job: String,
    /// Where the event id is, when the scheme does not fix it: a dot path into
    /// the JSON body (`data.id`), or `header:<Name>`.
    #[serde(default)]
    pub id: Option<String>,
    /// `hmac-sha256` only — the header carrying the hex digest.
    #[serde(default)]
    pub header: Option<String>,
    /// `stripe` / `paddle` only — seconds a signed timestamp may be off by.
    #[serde(default)]
    pub tolerance: Option<u64>,
    /// Who a delivery is FOR: a dot path into the event naming a principal id
    /// the app put there — Stripe's `data.object.client_reference_id`, set to
    /// `user.id` at checkout. The job then runs for that principal, so it may
    /// write the rows that person owns (and only those). Absent, or naming no
    /// existing principal: the job runs for nobody.
    #[serde(default)]
    pub principal: Option<String>,
}

/// Why a `webhooks` block, or one entry of it, cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookError {
    /// The entry's name.
    pub webhook: String,
    /// What is wrong with it.
    pub reason: String,
}

impl fmt::Display for WebhookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "webhook `{}`: {}", self.webhook, self.reason)
    }
}

impl std::error::Error for WebhookError {}

/// Where an event's id comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IdSource {
    /// A dot path into the JSON body.
    Body(Vec<String>),
    /// A request header, lowercased.
    Header(String),
}

/// A lowered, ready webhook.
#[derive(Clone)]
pub struct Webhook {
    /// The entry's name — part of every queue id it mints.
    pub name: String,
    /// The path it answers.
    pub path: String,
    /// The job each delivery queues.
    pub job: String,
    scheme: SignatureScheme,
    secret: Vec<u8>,
    id: IdSource,
    header: String,
    tolerance: u64,
    principal: Option<Vec<String>>,
}

impl fmt::Debug for Webhook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the secret — not in a panic message, not in a log line.
        f.debug_struct("Webhook")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("job", &self.job)
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

/// Why a delivery was not accepted. Each maps to a status the provider acts on:
/// a bad signature is never retried into success, a stale one is a replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The signature header is absent or unreadable.
    MissingSignature,
    /// It is present and does not match the body.
    BadSignature,
    /// The signed timestamp is outside the tolerance — a replay, or a clock.
    Stale,
    /// Verified, but not a JSON object.
    NotJson,
    /// Verified, but the event id is missing, empty or too long.
    NoEventId,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingSignature => "missing or unreadable signature header",
            Self::BadSignature => "signature does not match the body",
            Self::Stale => "signed timestamp is outside the tolerance",
            Self::NotJson => "body is not a JSON object",
            Self::NoEventId => "event id is missing",
        })
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Check a `webhooks` block without the environment — what `albedo build` runs.
///
/// # Errors
/// The first entry that is malformed, naming it.
pub fn validate_declarations(decls: &BTreeMap<String, WebhookDecl>) -> Result<(), WebhookError> {
    let mut paths: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, decl) in decls {
        let fail = |reason: String| {
            Err(WebhookError {
                webhook: name.clone(),
                reason,
            })
        };
        if !is_identifier(name) {
            return fail("the name must be an identifier (letters, digits, `_`)".to_string());
        }
        let path = decl.path.as_str();
        let path_ok = path.starts_with('/')
            && !path.starts_with("//")
            && path.len() > 1
            && path
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '-' | '_' | '.'));
        if !path_ok {
            return fail(format!(
                "`path` must be a plain absolute path like \"/hooks/{name}\"; `{path}` is not"
            ));
        }
        if path == "/_albedo" || path.starts_with("/_albedo/") {
            return fail("`/_albedo/` belongs to the framework".to_string());
        }
        if let Some(other) = paths.insert(path, name) {
            return fail(format!("`{path}` is already the path of webhook `{other}`"));
        }
        if !is_identifier(&decl.job) {
            return fail(format!("`job` must name an export of src/jobs.ts; `{}` cannot", decl.job));
        }
        if decl.secret.env.trim().is_empty() {
            return fail("`secret` must name an environment variable: { env: \"NAME\" }".to_string());
        }
        let fixes_its_own_header = !matches!(decl.signature, SignatureScheme::HmacSha256);
        match (&decl.header, fixes_its_own_header) {
            (Some(_), true) => {
                return fail(
                    "`header` is only for `hmac-sha256`; this scheme names its own".to_string(),
                )
            }
            (None, false) => {
                return fail("`hmac-sha256` needs `header`: the header carrying the digest".to_string())
            }
            (Some(header), false) if !is_header_name(header) => {
                return fail(format!("`{header}` is not a header name"))
            }
            _ => {}
        }
        if decl.tolerance.is_some()
            && !matches!(decl.signature, SignatureScheme::Stripe | SignatureScheme::Paddle)
        {
            return fail("`tolerance` is only for schemes that sign a timestamp (stripe, paddle)".to_string());
        }
        if matches!(decl.signature, SignatureScheme::HmacSha256) && decl.id.is_none() {
            return fail(
                "`hmac-sha256` needs `id`: where the event id is — a body path like \"id\", \
                 or \"header:X-Event-Id\""
                    .to_string(),
            );
        }
        if let Some(id) = &decl.id {
            if let Err(reason) = parse_id_source(id) {
                return fail(reason);
            }
        }
        if let Some(principal) = &decl.principal {
            if !matches!(parse_id_source(principal), Ok(IdSource::Body(_))) {
                return fail(format!(
                    "`principal: \"{principal}\"` must be a dot path into the event, like \
                     \"data.object.client_reference_id\""
                ));
            }
        }
    }
    Ok(())
}

fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn parse_id_source(raw: &str) -> Result<IdSource, String> {
    if let Some(header) = raw.strip_prefix("header:") {
        return if is_header_name(header) {
            Ok(IdSource::Header(header.to_ascii_lowercase()))
        } else {
            Err(format!("`id: \"{raw}\"` does not name a header"))
        };
    }
    let path: Vec<String> = raw.split('.').map(str::to_string).collect();
    if path.iter().any(|segment| segment.is_empty() || !is_header_name(segment)) {
        return Err(format!(
            "`id: \"{raw}\"` must be a dot path into the body, like \"id\" or \"data.id\""
        ));
    }
    Ok(IdSource::Body(path))
}

/// Every declared webhook, by path.
#[derive(Debug, Clone, Default)]
pub struct WebhookRegistry {
    by_path: BTreeMap<String, Webhook>,
}

impl WebhookRegistry {
    /// Validate and lower a `webhooks` block, reading each secret through `env`.
    ///
    /// # Errors
    /// A malformed entry, or a secret whose variable is unset or empty — at boot,
    /// where it is fixable, rather than as a `401` on every delivery.
    pub fn from_declarations(
        decls: &BTreeMap<String, WebhookDecl>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, WebhookError> {
        validate_declarations(decls)?;
        let mut by_path = BTreeMap::new();
        for (name, decl) in decls {
            let secret = env(&decl.secret.env)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| WebhookError {
                    webhook: name.clone(),
                    reason: format!("environment variable `{}` is not set", decl.secret.env),
                })?;
            let (default_header, default_id) = match decl.signature {
                SignatureScheme::Stripe => ("stripe-signature", IdSource::Body(vec!["id".into()])),
                SignatureScheme::Paddle => {
                    ("paddle-signature", IdSource::Body(vec!["event_id".into()]))
                }
                SignatureScheme::Github => (
                    "x-hub-signature-256",
                    IdSource::Header("x-github-delivery".into()),
                ),
                SignatureScheme::Razorpay => (
                    "x-razorpay-signature",
                    IdSource::Header("x-razorpay-event-id".into()),
                ),
                // Validated above: both are present for this scheme.
                SignatureScheme::HmacSha256 => ("", IdSource::Body(Vec::new())),
            };
            let header = decl
                .header
                .as_deref()
                .map_or_else(|| default_header.to_string(), str::to_ascii_lowercase);
            let id = match &decl.id {
                Some(raw) => parse_id_source(raw).map_err(|reason| WebhookError {
                    webhook: name.clone(),
                    reason,
                })?,
                None => default_id,
            };
            by_path.insert(
                decl.path.clone(),
                Webhook {
                    name: name.clone(),
                    path: decl.path.clone(),
                    job: decl.job.clone(),
                    scheme: decl.signature,
                    secret: secret.into_bytes(),
                    id,
                    header,
                    tolerance: decl.tolerance.unwrap_or(DEFAULT_TOLERANCE_SECONDS),
                    principal: decl
                        .principal
                        .as_deref()
                        .map(|path| path.split('.').map(str::to_string).collect()),
                },
            );
        }
        Ok(Self { by_path })
    }

    /// The webhook answering `path`, if any.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&Webhook> {
        self.by_path.get(path)
    }

    /// Every declared webhook.
    pub fn iter(&self) -> impl Iterator<Item = &Webhook> {
        self.by_path.values()
    }

    /// Whether any is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

impl Webhook {
    /// Check the signature of `body` against the request's headers. `header`
    /// looks a header up by its **lowercase** name; `now_s` is Unix seconds.
    ///
    /// # Errors
    /// Why the delivery must not be believed.
    pub fn verify(
        &self,
        header: impl Fn(&str) -> Option<String>,
        body: &[u8],
        now_s: i64,
    ) -> Result<(), Rejection> {
        let presented = header(&self.header).ok_or(Rejection::MissingSignature)?;
        match self.scheme {
            SignatureScheme::Stripe => {
                let fields = key_values(&presented, ',');
                let timestamp = single(&fields, "t").ok_or(Rejection::MissingSignature)?;
                self.check_fresh(timestamp, now_s)?;
                let candidates = all(&fields, "v1");
                self.check_any(&candidates, &[timestamp.as_bytes(), b".", body])
            }
            SignatureScheme::Paddle => {
                let fields = key_values(&presented, ';');
                let timestamp = single(&fields, "ts").ok_or(Rejection::MissingSignature)?;
                self.check_fresh(timestamp, now_s)?;
                let candidates = all(&fields, "h1");
                self.check_any(&candidates, &[timestamp.as_bytes(), b":", body])
            }
            SignatureScheme::Github | SignatureScheme::HmacSha256 => {
                let digest = presented.trim();
                let digest = digest.strip_prefix("sha256=").unwrap_or(digest);
                self.check_any(&[digest], &[body])
            }
            SignatureScheme::Razorpay => self.check_any(&[presented.trim()], &[body]),
        }
    }

    /// The event's id, from the verified body or a header.
    ///
    /// # Errors
    /// [`Rejection::NoEventId`] when it is absent, empty, not a string or a
    /// number, or longer than [`MAX_EVENT_ID_BYTES`].
    pub fn event_id(
        &self,
        header: impl Fn(&str) -> Option<String>,
        event: &Value,
    ) -> Result<String, Rejection> {
        let raw = match &self.id {
            IdSource::Header(name) => header(name),
            IdSource::Body(path) => {
                let mut cursor = event;
                for segment in path {
                    cursor = cursor.get(segment).ok_or(Rejection::NoEventId)?;
                }
                match cursor {
                    Value::String(text) => Some(text.clone()),
                    Value::Number(number) => Some(number.to_string()),
                    _ => None,
                }
            }
        };
        raw.map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty() && id.len() <= MAX_EVENT_ID_BYTES)
            .ok_or(Rejection::NoEventId)
    }

    /// The principal id the verified event names at the declared `principal`
    /// path, if it names one inside the partition-key alphabet. Existence is the
    /// caller's to check — it holds the database.
    #[must_use]
    pub fn principal_of(&self, event: &Value) -> Option<crate::auth::PrincipalId> {
        let path = self.principal.as_ref()?;
        let mut cursor = event;
        for segment in path {
            cursor = cursor.get(segment)?;
        }
        crate::auth::PrincipalId::parse(cursor.as_str()?).ok()
    }

    /// The queue id a delivery of `event_id` becomes — what makes a retried
    /// delivery land once.
    #[must_use]
    pub fn queue_id(&self, event_id: &str) -> String {
        format!("webhook:{}:{event_id}", self.name)
    }

    fn check_fresh(&self, timestamp: &str, now_s: i64) -> Result<(), Rejection> {
        let signed: i64 = timestamp.trim().parse().map_err(|_| Rejection::MissingSignature)?;
        let skew = now_s.saturating_sub(signed).unsigned_abs();
        if skew > self.tolerance {
            return Err(Rejection::Stale);
        }
        Ok(())
    }

    /// Whether any of `candidates` (hex digests) is the HMAC of `parts`
    /// concatenated. Compared in constant time by `verify_slice`.
    fn check_any(&self, candidates: &[&str], parts: &[&[u8]]) -> Result<(), Rejection> {
        if candidates.is_empty() {
            return Err(Rejection::MissingSignature);
        }
        for candidate in candidates {
            let Some(expected) = decode_hex(candidate) else {
                continue;
            };
            let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
                .expect("HMAC accepts a key of any length");
            for part in parts {
                mac.update(part);
            }
            if mac.verify_slice(&expected).is_ok() {
                return Ok(());
            }
        }
        Err(Rejection::BadSignature)
    }
}

/// `a=1,b=2` (or `;`-separated) as pairs, keys lowercased, order kept — a
/// header may repeat a key (Stripe lists one `v1` per active secret).
fn key_values(raw: &str, separator: char) -> Vec<(String, String)> {
    raw.split(separator)
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect()
}

fn single<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

fn all<'a>(fields: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    fields
        .iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
        .collect()
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    if raw.len() % 2 != 0 {
        return None;
    }
    (0..raw.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(raw.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test";
    const NOW: i64 = 1_760_000_000;

    fn hmac_hex(parts: &[&[u8]]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        for part in parts {
            mac.update(part);
        }
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn decl(scheme: SignatureScheme) -> WebhookDecl {
        WebhookDecl {
            path: "/hooks/pay".into(),
            signature: scheme,
            secret: EnvSecret { env: "PAY_SECRET".into() },
            job: "pay_event".into(),
            id: None,
            header: None,
            tolerance: None,
            principal: None,
        }
    }

    /// The event names who it is for; only a well-formed id counts.
    #[test]
    fn the_principal_comes_from_the_declared_path_and_must_be_an_id() {
        let mut d = decl(SignatureScheme::Stripe);
        d.principal = Some("data.object.client_reference_id".into());
        let hook = one(d);
        let event = serde_json::json!({ "data": { "object": { "client_reference_id": "u_7f3a" } } });
        assert_eq!(hook.principal_of(&event).unwrap().as_str(), "u_7f3a");
        let hostile = serde_json::json!({ "data": { "object": { "client_reference_id": "u:admin" } } });
        assert_eq!(hook.principal_of(&hostile), None);
        assert_eq!(hook.principal_of(&serde_json::json!({ "id": "evt" })), None);
        assert_eq!(one(decl(SignatureScheme::Stripe)).principal_of(&event), None);
    }

    fn one(decl: WebhookDecl) -> Webhook {
        let mut decls = BTreeMap::new();
        decls.insert("pay".to_string(), decl);
        let registry = WebhookRegistry::from_declarations(&decls, |name| {
            (name == "PAY_SECRET").then(|| SECRET.to_string())
        })
        .expect("valid");
        registry.get("/hooks/pay").cloned().expect("registered")
    }

    fn headers<'a>(pairs: &'a [(&'a str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn a_stripe_delivery_verifies_and_yields_its_event_id() {
        let hook = one(decl(SignatureScheme::Stripe));
        let body = br#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        let t = NOW.to_string();
        let signature = format!("t={t},v1={}", hmac_hex(&[t.as_bytes(), b".", body]));
        let h = [("Stripe-Signature", signature)];
        assert_eq!(hook.verify(headers(&h), body, NOW), Ok(()));
        let event: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(hook.event_id(headers(&h), &event).unwrap(), "evt_1");
        assert_eq!(hook.queue_id("evt_1"), "webhook:pay:evt_1");
    }

    /// One flipped byte, and the event is not believed — the whole point.
    #[test]
    fn a_tampered_body_or_a_wrong_secret_is_refused() {
        let hook = one(decl(SignatureScheme::Stripe));
        let body = br#"{"id":"evt_1","amount":100}"#;
        let t = NOW.to_string();
        let good = hmac_hex(&[t.as_bytes(), b".", body]);
        let h = [("stripe-signature", format!("t={t},v1={good}"))];
        let tampered = br#"{"id":"evt_1","amount":999}"#;
        assert_eq!(hook.verify(headers(&h), tampered, NOW), Err(Rejection::BadSignature));

        let forged = [("stripe-signature", format!("t={t},v1={}", "00".repeat(32)))];
        assert_eq!(hook.verify(headers(&forged), body, NOW), Err(Rejection::BadSignature));
        assert_eq!(hook.verify(headers(&[]), body, NOW), Err(Rejection::MissingSignature));
    }

    /// A captured delivery replayed later is refused by its signed timestamp —
    /// and the timestamp cannot be refreshed without the secret.
    #[test]
    fn a_stale_timestamp_is_a_replay() {
        let hook = one(decl(SignatureScheme::Stripe));
        let body = br#"{"id":"evt_1"}"#;
        let old = (NOW - 3600).to_string();
        let h = [(
            "stripe-signature",
            format!("t={old},v1={}", hmac_hex(&[old.as_bytes(), b".", body])),
        )];
        assert_eq!(hook.verify(headers(&h), body, NOW), Err(Rejection::Stale));
    }

    /// Stripe sends one `v1` per active secret during a rotation; any may match.
    #[test]
    fn any_of_several_signatures_may_match() {
        let hook = one(decl(SignatureScheme::Stripe));
        let body = br#"{"id":"evt_2"}"#;
        let t = NOW.to_string();
        let good = hmac_hex(&[t.as_bytes(), b".", body]);
        let h = [("stripe-signature", format!("t={t},v1={},v1={good}", "ab".repeat(32)))];
        assert_eq!(hook.verify(headers(&h), body, NOW), Ok(()));
    }

    #[test]
    fn paddle_github_razorpay_and_generic_hmac_verify() {
        let body = br#"{"event_id":"ntf_1","data":{"id":"sub_1"}}"#;

        let paddle = one(decl(SignatureScheme::Paddle));
        let ts = NOW.to_string();
        let h = [(
            "Paddle-Signature",
            format!("ts={ts};h1={}", hmac_hex(&[ts.as_bytes(), b":", body])),
        )];
        assert_eq!(paddle.verify(headers(&h), body, NOW), Ok(()));
        let event: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(paddle.event_id(headers(&h), &event).unwrap(), "ntf_1");

        let github = one(decl(SignatureScheme::Github));
        let h = [
            ("X-Hub-Signature-256", format!("sha256={}", hmac_hex(&[body]))),
            ("X-GitHub-Delivery", "d-1".to_string()),
        ];
        assert_eq!(github.verify(headers(&h), body, NOW), Ok(()));
        assert_eq!(github.event_id(headers(&h), &event).unwrap(), "d-1");

        let razorpay = one(decl(SignatureScheme::Razorpay));
        let h = [
            ("X-Razorpay-Signature", hmac_hex(&[body])),
            ("X-Razorpay-Event-Id", "rz-1".to_string()),
        ];
        assert_eq!(razorpay.verify(headers(&h), body, NOW), Ok(()));

        let mut generic = decl(SignatureScheme::HmacSha256);
        generic.header = Some("X-Signature".into());
        generic.id = Some("data.id".into());
        let generic = one(generic);
        let h = [("x-signature", hmac_hex(&[body]))];
        assert_eq!(generic.verify(headers(&h), body, NOW), Ok(()));
        assert_eq!(generic.event_id(headers(&h), &event).unwrap(), "sub_1");
    }

    #[test]
    fn a_verified_body_without_an_event_id_is_refused() {
        let hook = one(decl(SignatureScheme::Stripe));
        let event = serde_json::json!({ "type": "x" });
        assert_eq!(hook.event_id(headers(&[]), &event), Err(Rejection::NoEventId));
        let empty = serde_json::json!({ "id": "  " });
        assert_eq!(hook.event_id(headers(&[]), &empty), Err(Rejection::NoEventId));
    }

    #[test]
    fn malformed_declarations_are_refused_by_name() {
        let refuse = |edit: fn(&mut WebhookDecl)| {
            let mut d = decl(SignatureScheme::Stripe);
            edit(&mut d);
            let mut decls = BTreeMap::new();
            decls.insert("pay".to_string(), d);
            validate_declarations(&decls).unwrap_err().reason
        };
        assert!(refuse(|d| d.path = "hooks/pay".into()).contains("path"));
        assert!(refuse(|d| d.path = "/_albedo/hooks".into()).contains("framework"));
        assert!(refuse(|d| d.header = Some("X-Sig".into())).contains("only for `hmac-sha256`"));
        assert!(refuse(|d| d.job = "not a job".into()).contains("job"));
        assert!(refuse(|d| {
            d.signature = SignatureScheme::HmacSha256;
            d.header = Some("X-Sig".into());
        })
        .contains("needs `id`"));
        assert!(refuse(|d| {
            d.signature = SignatureScheme::Github;
            d.tolerance = Some(10);
        })
        .contains("tolerance"));
    }

    #[test]
    fn a_missing_secret_stops_the_boot_not_the_first_delivery() {
        let mut decls = BTreeMap::new();
        decls.insert("pay".to_string(), decl(SignatureScheme::Stripe));
        let err = WebhookRegistry::from_declarations(&decls, |_| None).unwrap_err();
        assert!(err.reason.contains("PAY_SECRET"), "{err}");
    }

    #[test]
    fn two_webhooks_cannot_share_a_path() {
        let mut decls = BTreeMap::new();
        decls.insert("a".to_string(), decl(SignatureScheme::Stripe));
        decls.insert("b".to_string(), decl(SignatureScheme::Paddle));
        let err = validate_declarations(&decls).unwrap_err();
        assert!(err.reason.contains("already the path"), "{err}");
    }

    /// The secret never appears in a `Debug` rendering.
    #[test]
    fn debug_output_never_carries_the_secret() {
        let hook = one(decl(SignatureScheme::Stripe));
        assert!(!format!("{hook:?}").contains(SECRET));
    }
}
