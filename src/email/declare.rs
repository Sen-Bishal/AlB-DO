//! The `email` block — who mail is from, and which provider carries it.
//!
//! ```json
//! "email": {
//!   "from": "Acme <no-reply@acme.dev>",
//!   "provider": "resend",
//!   "apiKey": { "env": "RESEND_API_KEY" }
//! }
//! ```
//!
//! ## Why the provider is a closed list
//!
//! Every HTTP mail API differs in exactly the places that matter — the auth
//! header, the recipient shape, which status means "accepted" — and a generic
//! "POST this template to that URL" option would move all of that into config
//! where nothing can test it. Four providers cover the market; `endpoint`
//! covers regional hosts (Mailgun EU) and self-hosted relays that speak one of
//! the four dialects. SMTP arrives later behind the same `Mailer` seam in the
//! server crate.
//!
//! ## Why `log` exists and why `albedo serve` says so out loud
//!
//! `log` writes each message to `.albedo/outbox/` as an `.eml` file instead of
//! sending it. It is what makes password reset testable on a laptop with no
//! provider account. It is also a way to lose every email in production
//! without an error, so boot prints a warning when `serve` runs with it.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::auth::declare::SecretDecl;
use crate::email::message::{Mailbox, MessageError};
use crate::email::templates::TemplatesDecl;

/// A provider this block can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// <https://resend.com> — `POST /emails`, bearer key, honours `Idempotency-Key`.
    Resend,
    /// <https://postmarkapp.com> — `POST /email`, `X-Postmark-Server-Token`.
    Postmark,
    /// <https://sendgrid.com> — `POST /v3/mail/send`, bearer key, answers `202`.
    Sendgrid,
    /// <https://mailgun.com> — `POST /v3/{domain}/messages`, form-encoded, basic auth.
    Mailgun,
    /// Nothing leaves the process: each message is written to `.albedo/outbox/`.
    Log,
}

impl ProviderKind {
    /// The name as written in config.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resend => "resend",
            Self::Postmark => "postmark",
            Self::Sendgrid => "sendgrid",
            Self::Mailgun => "mailgun",
            Self::Log => "log",
        }
    }

    /// The default API base. `None` for [`Self::Log`], which has no network.
    fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::Resend => Some("https://api.resend.com"),
            Self::Postmark => Some("https://api.postmarkapp.com"),
            Self::Sendgrid => Some("https://api.sendgrid.com"),
            Self::Mailgun => Some("https://api.mailgun.net"),
            Self::Log => None,
        }
    }
}

/// The `email` block as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailDecl {
    /// Default sender, `"Name <addr>"` or a bare address. Required: a provider
    /// refuses mail from an unverified domain, and guessing one would turn that
    /// into a runtime failure on the first reset request.
    pub from: String,
    /// Which provider carries the mail.
    pub provider: ProviderKind,
    /// Provider credential. Required for everything but `log`.
    #[serde(default, rename = "apiKey", alias = "api_key")]
    pub api_key: Option<SecretDecl>,
    /// Mailgun's sending domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// Override the API base — Mailgun EU (`https://api.eu.mailgun.net`), or a
    /// relay that speaks the provider's dialect.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Default `Reply-To`.
    #[serde(default, rename = "replyTo", alias = "reply_to")]
    pub reply_to: Option<String>,
    /// Replacements for the framework's own AUTH emails.
    #[serde(default)]
    pub templates: TemplatesDecl,
}

/// A lowered, validated `email` block. The key is still unresolved — an env var
/// may legitimately be unset on the machine running `albedo build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailConfig {
    /// The provider.
    pub provider: ProviderKind,
    /// Default sender.
    pub from: Mailbox,
    /// Default `Reply-To`.
    pub reply_to: Option<Mailbox>,
    /// API base, without a trailing slash. `None` only for `log`.
    pub endpoint: Option<Url>,
    /// Mailgun's sending domain.
    pub domain: Option<String>,
    /// Provider credential, unresolved.
    pub api_key: Option<SecretDecl>,
    /// The app's overrides of the AUTH emails, validated.
    pub templates: TemplatesDecl,
}

/// Why an `email` block was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailDeclError {
    /// `from` / `replyTo` is not a usable mailbox.
    Address {
        /// Which field.
        field: &'static str,
        /// Why.
        reason: MessageError,
    },
    /// A networked provider with no `apiKey`.
    MissingKey(ProviderKind),
    /// Mailgun with no `domain`, or a domain that is not a hostname.
    MailgunDomain,
    /// `endpoint` is not an absolute `https` URL (`http` only for loopback).
    Endpoint(String),
    /// `apiKey` names an env var that is not set. Boot only.
    MissingEnv(String),
    /// A template override could never produce a working email.
    Template(String),
}

impl std::fmt::Display for EmailDeclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Address { field, reason } => write!(f, "`email.{field}`: {reason}"),
            Self::MissingKey(kind) => write!(
                f,
                "`email.provider` is \"{}\" but no `apiKey` is declared; write \
                 `\"apiKey\": {{ \"env\": \"{}_API_KEY\" }}`",
                kind.as_str(),
                kind.as_str().to_ascii_uppercase()
            ),
            Self::MailgunDomain => write!(
                f,
                "`email.provider` is \"mailgun\", which needs `domain` — the sending domain \
                 configured in Mailgun (e.g. \"mg.example.com\")"
            ),
            Self::Endpoint(reason) => write!(f, "`email.endpoint`: {reason}"),
            Self::MissingEnv(var) => write!(
                f,
                "`email.apiKey` reads the environment variable `{var}`, which is not set"
            ),
            Self::Template(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for EmailDeclError {}

impl EmailDecl {
    /// Validate and lower.
    ///
    /// # Errors
    /// [`EmailDeclError`] naming the field at fault.
    pub fn lower(&self) -> Result<EmailConfig, EmailDeclError> {
        let from = Mailbox::parse(&self.from)
            .map_err(|reason| EmailDeclError::Address { field: "from", reason })?;
        let reply_to = self
            .reply_to
            .as_deref()
            .map(Mailbox::parse)
            .transpose()
            .map_err(|reason| EmailDeclError::Address { field: "replyTo", reason })?;

        if self.provider != ProviderKind::Log && self.api_key.is_none() {
            return Err(EmailDeclError::MissingKey(self.provider));
        }
        let domain = match self.provider {
            ProviderKind::Mailgun => {
                let domain = self.domain.as_deref().map(str::trim).unwrap_or_default();
                let plausible = !domain.is_empty()
                    && domain.contains('.')
                    && domain
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
                if !plausible {
                    return Err(EmailDeclError::MailgunDomain);
                }
                Some(domain.to_ascii_lowercase())
            }
            _ => None,
        };

        let endpoint = match (self.endpoint.as_deref(), self.provider.default_endpoint()) {
            (_, None) => None,
            (Some(raw), Some(_)) => Some(parse_endpoint(raw)?),
            (None, Some(default)) => Some(Url::parse(default).expect("default endpoints parse")),
        };

        self.templates.validate().map_err(EmailDeclError::Template)?;

        Ok(EmailConfig {
            provider: self.provider,
            from,
            reply_to,
            endpoint,
            domain,
            api_key: self.api_key.clone(),
            templates: self.templates.clone(),
        })
    }
}

impl EmailConfig {
    /// The host mail is sent to — its contribution to the egress allowlist.
    #[must_use]
    pub fn egress_host(&self) -> Option<String> {
        self.endpoint
            .as_ref()
            .and_then(Url::host_str)
            .map(str::to_string)
    }

    /// Resolve the credential against the process environment. `None` for `log`.
    ///
    /// # Errors
    /// [`EmailDeclError::MissingEnv`].
    pub fn resolve_key(&self) -> Result<Option<String>, EmailDeclError> {
        match &self.api_key {
            None => Ok(None),
            Some(SecretDecl::Value { value }) => Ok(Some(value.clone())),
            Some(SecretDecl::Env { env }) => std::env::var(env)
                .map(Some)
                .map_err(|_| EmailDeclError::MissingEnv(env.clone())),
        }
    }
}

/// An `endpoint` must be `https`, except to loopback — a relay on the same box
/// is the one case where plaintext never crosses a network.
fn parse_endpoint(raw: &str) -> Result<Url, EmailDeclError> {
    let url = Url::parse(raw.trim())
        .map_err(|err| EmailDeclError::Endpoint(format!("`{raw}` is not a URL: {err}")))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        other => {
            return Err(EmailDeclError::Endpoint(format!(
                "`{raw}` uses `{other}`; an API key only travels over https (http is allowed \
                 for a loopback relay)"
            )))
        }
    }
    if url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
        return Err(EmailDeclError::Endpoint(format!(
            "`{raw}` must be a base URL with a host and no query or fragment"
        )));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(value: serde_json::Value) -> EmailDecl {
        serde_json::from_value(value).expect("parses")
    }

    #[test]
    fn a_resend_block_lowers_with_its_default_endpoint() {
        let config = decl(serde_json::json!({
            "from": "Acme <no-reply@acme.dev>",
            "provider": "resend",
            "apiKey": { "env": "RESEND_API_KEY" }
        }))
        .lower()
        .expect("lowers");
        assert_eq!(config.from.address, "no-reply@acme.dev");
        assert_eq!(config.from.name.as_deref(), Some("Acme"));
        assert_eq!(config.egress_host().as_deref(), Some("api.resend.com"));
    }

    #[test]
    fn log_needs_no_key_and_talks_to_no_host() {
        let config = decl(serde_json::json!({ "from": "a@b.dev", "provider": "log" }))
            .lower()
            .expect("lowers");
        assert_eq!(config.egress_host(), None);
        assert_eq!(config.resolve_key(), Ok(None));
    }

    #[test]
    fn a_networked_provider_without_a_key_is_refused_by_name() {
        let err = decl(serde_json::json!({ "from": "a@b.dev", "provider": "postmark" }))
            .lower()
            .unwrap_err();
        assert_eq!(err, EmailDeclError::MissingKey(ProviderKind::Postmark));
        assert!(err.to_string().contains("POSTMARK_API_KEY"), "{err}");
    }

    #[test]
    fn mailgun_needs_a_domain_and_honours_an_eu_endpoint() {
        let base = serde_json::json!({
            "from": "a@b.dev", "provider": "mailgun", "apiKey": { "value": "k" }
        });
        assert_eq!(decl(base.clone()).lower().unwrap_err(), EmailDeclError::MailgunDomain);

        let mut eu = base;
        eu["domain"] = "mg.b.dev".into();
        eu["endpoint"] = "https://api.eu.mailgun.net".into();
        let config = decl(eu).lower().expect("lowers");
        assert_eq!(config.domain.as_deref(), Some("mg.b.dev"));
        assert_eq!(config.egress_host().as_deref(), Some("api.eu.mailgun.net"));
    }

    /// An API key over plaintext is a credential on the wire.
    #[test]
    fn an_http_endpoint_is_refused_unless_it_is_loopback() {
        let with = |endpoint: &str| {
            decl(serde_json::json!({
                "from": "a@b.dev", "provider": "resend",
                "apiKey": { "value": "k" }, "endpoint": endpoint
            }))
            .lower()
        };
        assert!(matches!(with("http://relay.example.com"), Err(EmailDeclError::Endpoint(_))));
        assert!(matches!(with("ftp://x.dev"), Err(EmailDeclError::Endpoint(_))));
        assert!(matches!(with("https://x.dev/?a=1"), Err(EmailDeclError::Endpoint(_))));
        assert!(with("http://127.0.0.1:8025").is_ok());
        assert!(with("https://relay.example.com").is_ok());
    }

    #[test]
    fn a_header_injection_in_from_is_refused_at_build() {
        let err = decl(serde_json::json!({
            "from": "a@b.dev\r\nBcc: victim@x.dev", "provider": "log"
        }))
        .lower()
        .unwrap_err();
        assert!(matches!(err, EmailDeclError::Address { field: "from", .. }), "{err}");
    }

    #[test]
    fn a_template_that_cannot_work_fails_the_block() {
        let err = decl(serde_json::json!({
            "from": "a@b.dev", "provider": "log",
            "templates": { "magicLink": { "text": "Sign in at {{url}}" } }
        }))
        .lower()
        .unwrap_err();
        assert!(matches!(err, EmailDeclError::Template(_)), "{err}");
        assert!(err.to_string().contains("magicLink"), "{err}");

        let typo = serde_json::from_value::<EmailDecl>(serde_json::json!({
            "from": "a@b.dev", "provider": "log",
            "templates": { "passwordRest": { "text": "{{link}}" } }
        }));
        assert!(typo.is_err(), "a misspelled template name must not be ignored");
    }

    /// `deny_unknown_fields` — `apikey` (lowercase k) must not silently mean
    /// "no key".
    #[test]
    fn a_misspelled_field_is_a_parse_error_not_a_missing_value() {
        let parsed = serde_json::from_value::<EmailDecl>(serde_json::json!({
            "from": "a@b.dev", "provider": "resend", "apikey": { "env": "K" }
        }));
        assert!(parsed.is_err());
    }
}
