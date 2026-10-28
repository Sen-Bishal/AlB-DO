//! EMAIL · 15.3 — transactional mail.
//!
//! ## The sentence
//!
//! **A message is a queue row.** `sendEmail()` does not send: it validates the
//! message and writes an `albedo:send_email` job, and the job runner sends it
//! with the lease, backoff and dead-lettering 15.5 already built. So a provider
//! outage turns a sign-up into a sign-up whose welcome email arrives late —
//! never into a failed sign-up — and a double-submitted form with an `id` sends
//! once.
//!
//! ## Module map
//!
//! | module | holds |
//! |---|---|
//! | [`declare`] | the `email` block: sender, provider, key, endpoint |
//! | [`message`] | [`Message`] and its trust-boundary validation — header injection, bounds |
//! | [`provider`] | the four HTTP dialects as pure functions, and response classification |
//! | [`eml`] | RFC 5322 rendering, for the `log` provider's outbox |
//! | [`templates`] | the framework's own AUTH emails |
//!
//! The network half lives in `albedo-server`'s `email` module, behind a
//! `Mailer` trait, so an SMTP transport slots in without touching any of this.

pub mod declare;
pub mod eml;
pub mod message;
pub mod provider;
pub mod templates;

pub use declare::{EmailConfig, EmailDecl, EmailDeclError, ProviderKind};
pub use message::{
    Attachment, AttachmentSource, Mailbox, Message, MessageError, QueuedEmail, ResolvedAttachment,
};
pub use provider::{build_request, classify, SendOutcome};
pub use templates::{AuthEmail, TemplateOverride, TemplatesDecl};
