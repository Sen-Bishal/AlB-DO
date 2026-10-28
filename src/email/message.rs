//! A message, validated once at the trust boundary.
//!
//! `sendEmail()` runs in app code and its argument crosses from the engine as
//! JSON. Everything a provider will interpret — every address, the subject — is
//! checked **here, when it is queued**, not when it is sent: a malformed
//! message found at send time is a dead letter an hour later that nobody reads,
//! while one found at queue time is an exception in the action that made it.
//!
//! ## The one attack this module is shaped by
//!
//! **Header injection.** A subject or address carrying `\r\n` becomes a new
//! header line in any transport that writes RFC 5322 — `Bcc: everyone@…` —
//! which is how a contact form turns into a spam relay. The HTTP providers take
//! JSON and are not vulnerable themselves, but the `log` outbox writes real
//! `.eml` files and SMTP will slot in behind the same message type. So control
//! characters are refused in every header-bound field, whatever the transport.

use serde::{Deserialize, Serialize};

/// Most recipients one message may carry across `to`, `cc` and `bcc`.
///
/// Every provider caps this (Resend 50, Postmark 50, SendGrid 1000 per
/// personalization) and a message over the lowest cap would fail at send time.
/// Refusing at queue time is the same answer, earlier.
pub const MAX_RECIPIENTS: usize = 50;

/// Longest subject, in bytes — RFC 5322's line limit minus the `Subject: ` name.
pub const MAX_SUBJECT_BYTES: usize = 988;

/// Largest `text` + `html`, in bytes.
///
/// The body lives in a queue row until it is sent. 1 MiB is far past any
/// transactional email and bounds what one `sendEmail()` can make the database
/// hold.
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// Most attachments one message may carry.
pub const MAX_ATTACHMENTS: usize = 10;

/// Largest total of **inline** attachment bytes (decoded).
///
/// Inline content lives in the queue row, base64-encoded, until the message is
/// sent; this bounds that row. Anything bigger belongs in an upload, which the
/// row references by id and the runner reads at send time.
pub const MAX_INLINE_ATTACHMENT_BYTES: usize = 5 << 20;

/// Largest total of attachment bytes, inline and uploaded together.
///
/// Postmark's 10 MB message cap is the lowest of the four providers; a message
/// over it fails at send time on one provider and not another, which is the
/// kind of difference a queue should refuse up front.
pub const MAX_ATTACHMENT_BYTES: u64 = 10 << 20;

/// Longest address — RFC 5321's `Path` limit.
const MAX_ADDRESS_BYTES: usize = 254;

/// Longest display name we accept.
const MAX_NAME_BYTES: usize = 256;

/// Why a message or an address was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageError {
    /// Not a plausible single address.
    Address(String),
    /// A control character — `\r`, `\n`, NUL — in a header-bound field.
    ControlCharacter(&'static str),
    /// No `to` recipient.
    NoRecipients,
    /// More than [`MAX_RECIPIENTS`].
    TooManyRecipients(usize),
    /// Empty, or longer than [`MAX_SUBJECT_BYTES`].
    Subject,
    /// Neither `text` nor `html`.
    NoBody,
    /// `text` + `html` over [`MAX_BODY_BYTES`].
    BodyTooLarge(usize),
    /// The JSON did not have the message's shape.
    Shape(String),
    /// An attachment is malformed, too many, or too large.
    Attachment(String),
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Address(raw) => write!(f, "`{}` is not a usable email address", truncate(raw)),
            Self::ControlCharacter(field) => write!(
                f,
                "`{field}` contains a line break or control character, which would let it \
                 write extra mail headers"
            ),
            Self::NoRecipients => write!(f, "a message needs at least one `to` address"),
            Self::TooManyRecipients(n) => write!(
                f,
                "a message may have at most {MAX_RECIPIENTS} recipients across to/cc/bcc; this \
                 one has {n} — send one message per recipient instead"
            ),
            Self::Subject => write!(
                f,
                "`subject` must be non-empty and at most {MAX_SUBJECT_BYTES} bytes"
            ),
            Self::NoBody => write!(f, "a message needs `text`, `html`, or both"),
            Self::BodyTooLarge(n) => write!(
                f,
                "`text` + `html` is {n} bytes; the limit is {MAX_BODY_BYTES}"
            ),
            Self::Shape(reason) => write!(f, "{reason}"),
            Self::Attachment(reason) => write!(f, "attachment: {reason}"),
        }
    }
}

impl std::error::Error for MessageError {}

/// One address, with an optional display name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    /// Display name, unquoted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `local@domain`, case preserved.
    pub address: String,
}

impl Mailbox {
    /// Parse `addr` or `Name <addr>` (`"Quoted, Name" <addr>` too).
    ///
    /// Case is **preserved**. [`crate::auth::password::normalize_email`] folds
    /// case because its output is an account key; here the output is where a
    /// human's mail goes, and the local part is theirs to spell.
    ///
    /// # Errors
    /// [`MessageError::ControlCharacter`] or [`MessageError::Address`].
    pub fn parse(raw: &str) -> Result<Self, MessageError> {
        if raw.chars().any(char::is_control) {
            return Err(MessageError::ControlCharacter("address"));
        }
        let trimmed = raw.trim();
        let (name, address) = match (trimmed.rfind('<'), trimmed.ends_with('>')) {
            (Some(open), true) => {
                let name = trimmed[..open].trim();
                let name = name
                    .strip_prefix('"')
                    .and_then(|n| n.strip_suffix('"'))
                    .unwrap_or(name)
                    .trim();
                (
                    (!name.is_empty()).then(|| name.to_string()),
                    &trimmed[open + 1..trimmed.len() - 1],
                )
            }
            (None, false) => (None, trimmed),
            _ => return Err(MessageError::Address(raw.to_string())),
        };
        if name.as_ref().is_some_and(|n| n.len() > MAX_NAME_BYTES || n.contains(['<', '>', '"'])) {
            return Err(MessageError::Address(raw.to_string()));
        }
        if !is_plausible_address(address) {
            return Err(MessageError::Address(raw.to_string()));
        }
        Ok(Self {
            name,
            address: address.to_string(),
        })
    }

    /// `"Name" <addr>`, or the bare address. The form every string-taking
    /// provider and RFC 5322 accept; the name is always quoted so a comma or a
    /// dot in it cannot split it into two mailboxes.
    #[must_use]
    pub fn formatted(&self) -> String {
        match &self.name {
            Some(name) => format!("\"{}\" <{}>", name.replace('\\', "\\\\"), self.address),
            None => self.address.clone(),
        }
    }
}

/// One `@`, something plausible on each side, no whitespace, a dotted domain.
/// Deliberately not RFC 5322: the grammar admits comments and quoted strings no
/// provider will deliver to, and a parser for it is a liability.
fn is_plausible_address(address: &str) -> bool {
    if address.is_empty() || address.len() > MAX_ADDRESS_BYTES {
        return false;
    }
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !domain.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with(['.', '-'])
        && !domain.ends_with(['.', '-'])
        && !domain.contains("..")
        && !address
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')' | ',' | ';' | '"' | '\\'))
}

/// A message ready to hand to a transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Sender. `None` means the `email` block's `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Mailbox>,
    /// Primary recipients. Never empty.
    pub to: Vec<Mailbox>,
    /// Carbon copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<Mailbox>,
    /// Blind carbon copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<Mailbox>,
    /// `Reply-To`. `None` means the block's default, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Mailbox>,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// HTML body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// Files, by value or by upload reference.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// One attached file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// The name the recipient sees. Empty for an upload attached without one:
    /// the runner substitutes the name it was uploaded under.
    pub filename: String,
    /// `type/subtype`. `None` means "whatever the upload was stored as", or
    /// `application/octet-stream` for inline content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Where the bytes come from.
    pub source: AttachmentSource,
}

/// Where an attachment's bytes live until it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentSource {
    /// Standard base64, validated at queue time, held in the row.
    Inline(String),
    /// A stored upload's id, read from disk at send time. The server checks the
    /// upload belongs to the principal the send runs for — a content-addressed
    /// id is a name, not a permission.
    Upload(String),
}

/// An attachment with its bytes in hand — what a transport receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAttachment {
    /// The name the recipient sees.
    pub filename: String,
    /// `type/subtype`.
    pub content_type: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// The shape `sendEmail()` accepts from app code. Strings, and a string or an
/// array for each recipient list — `to: "a@b.dev"` is the spelling everybody
/// writes first. Unknown fields are refused, so `body:` is an error rather than
/// a message with no body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Authored {
    #[serde(default)]
    from: Option<String>,
    to: OneOrMany,
    #[serde(default)]
    cc: Option<OneOrMany>,
    #[serde(default)]
    bcc: Option<OneOrMany>,
    #[serde(default)]
    reply_to: Option<String>,
    subject: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    attachments: Vec<AuthoredAttachment>,
}

/// `{ filename, content, contentType? }` or `{ upload, filename?, contentType? }`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AuthoredAttachment {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    upload: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
}

impl AuthoredAttachment {
    fn parse(self) -> Result<Attachment, MessageError> {
        use base64::Engine as _;
        let content_type = self.content_type.map(check_content_type).transpose()?;
        let source = match (self.content, self.upload) {
            (Some(content), None) => {
                base64::engine::general_purpose::STANDARD
                    .decode(content.as_bytes())
                    .map_err(|_| {
                        MessageError::Attachment(
                            "`content` must be standard base64 (use `upload` for a stored file)"
                                .to_string(),
                        )
                    })?;
                AttachmentSource::Inline(content)
            }
            (None, Some(upload)) => {
                if !crate::upload::store::is_upload_id(&upload) {
                    return Err(MessageError::Attachment(format!(
                        "`upload` must be an upload id (64 hex characters), not `{}`",
                        truncate(&upload)
                    )));
                }
                AttachmentSource::Upload(upload)
            }
            _ => {
                return Err(MessageError::Attachment(
                    "give exactly one of `content` (base64) or `upload` (an upload id)".to_string(),
                ))
            }
        };
        let filename = match (self.filename, &source) {
            (Some(name), _) => {
                check_filename(&name)?;
                name
            }
            (None, AttachmentSource::Upload(_)) => String::new(),
            (None, AttachmentSource::Inline(_)) => {
                return Err(MessageError::Attachment(
                    "inline content needs a `filename`".to_string(),
                ))
            }
        };
        Ok(Attachment {
            filename,
            content_type,
            source,
        })
    }
}

/// A name that is safe in a `Content-Disposition` header and meaningless as a
/// path. Refused rather than repaired: the author should hear about it.
///
/// # Errors
/// [`MessageError::Attachment`].
pub fn check_filename(name: &str) -> Result<(), MessageError> {
    let bad = name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | '"'));
    if bad {
        return Err(MessageError::Attachment(format!(
            "`{}` is not a usable filename — no slashes, quotes or control characters, at most \
             255 bytes",
            truncate(name)
        )));
    }
    Ok(())
}

fn check_content_type(raw: String) -> Result<String, MessageError> {
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
    };
    match raw.split_once('/') {
        Some((kind, sub)) if token(kind) && token(sub) => Ok(raw.to_ascii_lowercase()),
        _ => Err(MessageError::Attachment(format!(
            "`contentType` must look like `type/subtype`, not `{}`",
            truncate(&raw)
        ))),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn parse(self) -> Result<Vec<Mailbox>, MessageError> {
        match self {
            Self::One(one) => Ok(vec![Mailbox::parse(&one)?]),
            Self::Many(many) => many.iter().map(|raw| Mailbox::parse(raw)).collect(),
        }
    }
}

impl Message {
    /// Validate the value a `sendEmail()` call staged.
    ///
    /// # Errors
    /// [`MessageError`] naming what is wrong.
    pub fn from_authored(value: &serde_json::Value) -> Result<Self, MessageError> {
        let authored: Authored = serde_json::from_value(value.clone()).map_err(|err| {
            MessageError::Shape(format!(
                "sendEmail(message): {err} — expected {{ to, subject, text?, html?, cc?, bcc?, \
                 replyTo?, from? }}"
            ))
        })?;
        let message = Self {
            from: authored.from.as_deref().map(Mailbox::parse).transpose()?,
            to: authored.to.parse()?,
            cc: authored.cc.map(OneOrMany::parse).transpose()?.unwrap_or_default(),
            bcc: authored.bcc.map(OneOrMany::parse).transpose()?.unwrap_or_default(),
            reply_to: authored.reply_to.as_deref().map(Mailbox::parse).transpose()?,
            subject: authored.subject,
            text: authored.text,
            html: authored.html,
            attachments: authored
                .attachments
                .into_iter()
                .map(AuthoredAttachment::parse)
                .collect::<Result<_, _>>()?,
        };
        message.validate()?;
        Ok(message)
    }

    /// The invariants every message holds, however it was built. Called again
    /// by the runner on a row read back from the queue, because a row is a
    /// database value and a database can be edited.
    ///
    /// # Errors
    /// [`MessageError`].
    pub fn validate(&self) -> Result<(), MessageError> {
        if self.to.is_empty() {
            return Err(MessageError::NoRecipients);
        }
        let recipients = self.to.len() + self.cc.len() + self.bcc.len();
        if recipients > MAX_RECIPIENTS {
            return Err(MessageError::TooManyRecipients(recipients));
        }
        if self.subject.chars().any(char::is_control) {
            return Err(MessageError::ControlCharacter("subject"));
        }
        if self.subject.trim().is_empty() || self.subject.len() > MAX_SUBJECT_BYTES {
            return Err(MessageError::Subject);
        }
        let text = self.text.as_deref().unwrap_or_default();
        let html = self.html.as_deref().unwrap_or_default();
        if text.is_empty() && html.is_empty() {
            return Err(MessageError::NoBody);
        }
        let body = text.len() + html.len();
        if body > MAX_BODY_BYTES {
            return Err(MessageError::BodyTooLarge(body));
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(MessageError::Attachment(format!(
                "at most {MAX_ATTACHMENTS} per message; this one has {}",
                self.attachments.len()
            )));
        }
        // base64 is 4 bytes per 3; the decoded size is what a provider counts.
        let inline: usize = self
            .attachments
            .iter()
            .filter_map(|a| match &a.source {
                AttachmentSource::Inline(content) => Some(content.len() / 4 * 3),
                AttachmentSource::Upload(_) => None,
            })
            .sum();
        if inline > MAX_INLINE_ATTACHMENT_BYTES {
            return Err(MessageError::Attachment(format!(
                "inline attachments total about {inline} bytes; the limit is \
                 {MAX_INLINE_ATTACHMENT_BYTES} — store larger files as uploads and attach them by \
                 `upload` id"
            )));
        }
        Ok(())
    }

    /// Every recipient, in to → cc → bcc order.
    pub fn recipients(&self) -> impl Iterator<Item = &Mailbox> {
        self.to.iter().chain(&self.cc).chain(&self.bcc)
    }
}

/// What sits in a queue row's `args`.
///
/// `sensitive` marks a message carrying a credential — a reset or sign-in link.
/// The runner scrubs every sent message's row (a delivered body has no further
/// use and is personal data), but only a sensitive one is also scrubbed when it
/// dead-letters: an app's dead-lettered newsletter is something an operator
/// wants to replay, a dead-lettered reset link is a live credential in a table.
///
/// Not settable from app code: `sendEmail()`'s argument is parsed through
/// [`Message::from_authored`], which refuses unknown fields, and wrapped here by
/// the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedEmail {
    /// The message.
    pub message: Message,
    /// Carries a credential.
    #[serde(default)]
    pub sensitive: bool,
}

fn truncate(raw: &str) -> String {
    const MAX: usize = 80;
    if raw.len() <= MAX {
        return raw.to_string();
    }
    let mut cut = MAX;
    while !raw.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &raw[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_bare_address_and_a_named_one_both_parse() {
        assert_eq!(
            Mailbox::parse(" Ada@Example.com "),
            Ok(Mailbox { name: None, address: "Ada@Example.com".into() })
        );
        let named = Mailbox::parse("\"Lovelace, Ada\" <ada@example.com>").expect("parses");
        assert_eq!(named.name.as_deref(), Some("Lovelace, Ada"));
        assert_eq!(named.formatted(), "\"Lovelace, Ada\" <ada@example.com>");
    }

    #[test]
    fn implausible_addresses_are_refused() {
        for raw in [
            "", "ada", "@x.dev", "ada@", "ada@x", "a@@x.dev", "ada@.x.dev", "ada@x..dev",
            "ada lovelace@x.dev", "<ada@x.dev", "ada@x.dev>", "a,b@x.dev", "Ada <>",
            "Evil <a@x.dev> <b@y.dev>",
        ] {
            assert!(Mailbox::parse(raw).is_err(), "`{raw}` must be refused");
        }
    }

    /// The attack the module exists for.
    #[test]
    fn a_line_break_anywhere_header_bound_is_refused() {
        assert_eq!(
            Mailbox::parse("ada@x.dev\r\nBcc: all@x.dev"),
            Err(MessageError::ControlCharacter("address"))
        );
        let err = Message::from_authored(&json!({
            "to": "ada@x.dev", "subject": "hi\nBcc: all@x.dev", "text": "t"
        }))
        .unwrap_err();
        assert_eq!(err, MessageError::ControlCharacter("subject"));
    }

    #[test]
    fn to_accepts_a_string_or_an_array() {
        let one = Message::from_authored(&json!({ "to": "a@x.dev", "subject": "s", "text": "t" }))
            .expect("valid");
        assert_eq!(one.to.len(), 1);
        let many = Message::from_authored(&json!({
            "to": ["a@x.dev", "B <b@x.dev>"], "cc": "c@x.dev", "subject": "s", "html": "<p>h</p>"
        }))
        .expect("valid");
        assert_eq!(many.recipients().count(), 3);
    }

    #[test]
    fn a_misspelled_field_is_refused_not_ignored() {
        let err = Message::from_authored(&json!({ "to": "a@x.dev", "subject": "s", "body": "t" }))
            .unwrap_err();
        assert!(matches!(err, MessageError::Shape(_)), "{err}");
        assert!(err.to_string().contains("body"), "{err}");
    }

    #[test]
    fn app_code_cannot_mark_its_own_message_sensitive() {
        assert!(Message::from_authored(&json!({
            "to": "a@x.dev", "subject": "s", "text": "t", "sensitive": true
        }))
        .is_err());
    }

    #[test]
    fn the_bounds_hold() {
        let to: Vec<String> = (0..=MAX_RECIPIENTS).map(|i| format!("u{i}@x.dev")).collect();
        assert_eq!(
            Message::from_authored(&json!({ "to": to, "subject": "s", "text": "t" })).unwrap_err(),
            MessageError::TooManyRecipients(MAX_RECIPIENTS + 1)
        );
        assert_eq!(
            Message::from_authored(&json!({ "to": "a@x.dev", "subject": "s" })).unwrap_err(),
            MessageError::NoBody
        );
        assert_eq!(
            Message::from_authored(&json!({ "to": "a@x.dev", "subject": "  ", "text": "t" }))
                .unwrap_err(),
            MessageError::Subject
        );
        let huge = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(matches!(
            Message::from_authored(&json!({ "to": "a@x.dev", "subject": "s", "text": huge })),
            Err(MessageError::BodyTooLarge(_))
        ));
    }

    #[test]
    fn attachments_come_inline_or_by_upload_and_are_checked_at_the_boundary() {
        let upload = "a".repeat(64);
        let message = Message::from_authored(&json!({
            "to": "a@x.dev", "subject": "s", "text": "t",
            "attachments": [
                { "filename": "hi.txt", "content": "aGk=", "contentType": "text/plain" },
                { "upload": upload }
            ]
        }))
        .expect("valid");
        assert_eq!(message.attachments.len(), 2);
        assert_eq!(message.attachments[0].source, AttachmentSource::Inline("aGk=".into()));
        assert_eq!(message.attachments[1].source, AttachmentSource::Upload(upload.clone()));

        for bad in [
            json!({ "filename": "x", "content": "not base64!!" }),
            json!({ "content": "aGk=" }),
            json!({ "filename": "x", "upload": "../../etc/passwd" }),
            json!({ "filename": "x", "content": "aGk=", "upload": upload }),
            json!({ "filename": "../evil", "content": "aGk=" }),
            json!({ "filename": "a\r\nb", "content": "aGk=" }),
            json!({ "filename": "x", "content": "aGk=", "contentType": "text/plain\r\nX: y" }),
            json!({ "filename": "x", "content": "aGk=", "path": "/etc/passwd" }),
        ] {
            let result = Message::from_authored(&json!({
                "to": "a@x.dev", "subject": "s", "text": "t", "attachments": [bad.clone()]
            }));
            assert!(result.is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn attachment_count_and_inline_size_are_bounded() {
        use base64::Engine as _;
        let one = json!({ "filename": "x", "content": "aGk=" });
        let many: Vec<_> = (0..=MAX_ATTACHMENTS).map(|_| one.clone()).collect();
        assert!(matches!(
            Message::from_authored(&json!({
                "to": "a@x.dev", "subject": "s", "text": "t", "attachments": many
            })),
            Err(MessageError::Attachment(_))
        ));
        let big = base64::engine::general_purpose::STANDARD
            .encode(vec![0u8; MAX_INLINE_ATTACHMENT_BYTES + 3]);
        assert!(matches!(
            Message::from_authored(&json!({
                "to": "a@x.dev", "subject": "s", "text": "t",
                "attachments": [{ "filename": "big.bin", "content": big }]
            })),
            Err(MessageError::Attachment(_))
        ));
    }

    #[test]
    fn a_queued_email_round_trips_through_its_row() {
        let message =
            Message::from_authored(&json!({ "to": "a@x.dev", "subject": "s", "text": "t" }))
                .expect("valid");
        let queued = QueuedEmail { message, sensitive: true };
        let row = serde_json::to_string(&queued).expect("serializes");
        assert_eq!(serde_json::from_str::<QueuedEmail>(&row).expect("parses"), queued);
    }
}
