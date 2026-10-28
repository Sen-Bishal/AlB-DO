//! A message as an RFC 5322 file — what the `log` provider writes to
//! `.albedo/outbox/`, so a developer can open a reset email in any mail client
//! and click the link.
//!
//! Every header value here has already been through
//! [`Message::validate`](crate::email::message::Message::validate), which
//! refuses control characters; that is what makes plain concatenation safe.
//! Non-ASCII subjects and names are RFC 2047 encoded-words, bodies are base64,
//! so the file is 7-bit clean whatever the content.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use crate::email::message::{Mailbox, Message, ResolvedAttachment};

/// Render `message` as a complete `.eml` document.
///
/// `from` / `reply_to` are the resolved defaults; `message_id` names the file
/// and the `Message-ID` header; `date` is an RFC 2822 date string.
#[must_use]
pub fn render(
    message: &Message,
    attachments: &[ResolvedAttachment],
    from: &Mailbox,
    reply_to: Option<&Mailbox>,
    message_id: &str,
    date: &str,
) -> String {
    let mut out = String::with_capacity(
        512 + message.text.as_ref().map_or(0, String::len) * 4 / 3
            + message.html.as_ref().map_or(0, String::len) * 4 / 3,
    );
    header(&mut out, "From", &mailbox(message.from.as_ref().unwrap_or(from)));
    header(&mut out, "To", &list(&message.to));
    if !message.cc.is_empty() {
        header(&mut out, "Cc", &list(&message.cc));
    }
    // Kept in the local copy: this file is the developer's record of what would
    // have been sent, and a Bcc they cannot see is a Bcc they cannot check.
    if !message.bcc.is_empty() {
        header(&mut out, "Bcc", &list(&message.bcc));
    }
    if let Some(reply_to) = message.reply_to.as_ref().or(reply_to) {
        header(&mut out, "Reply-To", &mailbox(reply_to));
    }
    header(&mut out, "Subject", &encode_word(&message.subject));
    header(&mut out, "Date", date);
    header(&mut out, "Message-ID", &format!("<{message_id}@albedo.local>"));
    header(&mut out, "MIME-Version", "1.0");

    // Boundaries cannot collide with content: every part is base64, whose
    // alphabet has no `-`, and every boundary starts with `albedo-`.
    if attachments.is_empty() {
        body_section(&mut out, message, &format!("albedo-alt-{message_id}"));
        return out;
    }
    let mixed = format!("albedo-mixed-{message_id}");
    header(
        &mut out,
        "Content-Type",
        &format!("multipart/mixed; boundary=\"{mixed}\""),
    );
    out.push_str("\r\n");
    out.push_str(&format!("--{mixed}\r\n"));
    body_section(&mut out, message, &format!("albedo-alt-{message_id}"));
    for file in attachments {
        out.push_str(&format!("--{mixed}\r\n"));
        header(&mut out, "Content-Type", &file.content_type);
        header(
            &mut out,
            "Content-Disposition",
            &format!("attachment; {}", filename_param(&file.filename)),
        );
        header(&mut out, "Content-Transfer-Encoding", "base64");
        out.push_str("\r\n");
        base64_lines(&mut out, &file.bytes);
    }
    out.push_str(&format!("--{mixed}--\r\n"));
    out
}

/// The text/html body: one part, or `multipart/alternative` for both.
fn body_section(out: &mut String, message: &Message, boundary: &str) {
    let text = message.text.as_deref().filter(|t| !t.is_empty());
    let html = message.html.as_deref().filter(|h| !h.is_empty());
    match (text, html) {
        (Some(text), Some(html)) => {
            header(
                out,
                "Content-Type",
                &format!("multipart/alternative; boundary=\"{boundary}\""),
            );
            out.push_str("\r\n");
            for (kind, body) in [("text/plain", text), ("text/html", html)] {
                out.push_str(&format!("--{boundary}\r\n"));
                part(out, kind, body);
            }
            out.push_str(&format!("--{boundary}--\r\n"));
        }
        (Some(body), None) => part(out, "text/plain", body),
        (None, Some(body)) => part(out, "text/html", body),
        (None, None) => out.push_str("\r\n"),
    }
}

/// `filename="…"`, or RFC 2231's `filename*=utf-8''…` for a non-ASCII name.
/// Quotes and control characters were refused at queue time.
fn filename_param(name: &str) -> String {
    if name.is_ascii() {
        return format!("filename=\"{name}\"");
    }
    let mut encoded = String::with_capacity(name.len() * 3);
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("filename*=utf-8''{encoded}")
}

/// An RFC 2822 `Date:` value for a Unix-millisecond instant.
#[must_use]
pub fn rfc2822(ms: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_millis_opt(ms)
        .single()
        .map_or_else(|| "Thu, 01 Jan 1970 00:00:00 +0000".to_string(), |dt| dt.to_rfc2822())
}

fn header(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str(": ");
    out.push_str(value);
    out.push_str("\r\n");
}

fn part(out: &mut String, kind: &str, body: &str) {
    header(out, "Content-Type", &format!("{kind}; charset=utf-8"));
    header(out, "Content-Transfer-Encoding", "base64");
    out.push_str("\r\n");
    base64_lines(out, body.as_bytes());
}

/// base64 in 76-column lines, RFC 2045 § 6.8. base64 output is ASCII, so byte
/// chunks are character chunks.
fn base64_lines(out: &mut String, bytes: &[u8]) {
    let encoded = STANDARD.encode(bytes);
    for line in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push_str("\r\n");
    }
}

fn mailbox(mailbox: &Mailbox) -> String {
    match &mailbox.name {
        Some(name) if !name.is_ascii() => format!("{} <{}>", encode_word(name), mailbox.address),
        _ => mailbox.formatted(),
    }
}

fn list(list: &[Mailbox]) -> String {
    list.iter().map(mailbox).collect::<Vec<_>>().join(", ")
}

/// RFC 2047 `=?utf-8?B?…?=` for anything non-ASCII; ASCII passes through.
fn encode_word(value: &str) -> String {
    if value.is_ascii() {
        value.to_string()
    } else {
        format!("=?utf-8?B?{}?=", STANDARD.encode(value.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn from() -> Mailbox {
        Mailbox::parse("Acme <no-reply@acme.dev>").expect("parses")
    }

    #[test]
    fn a_two_part_message_is_multipart_alternative_and_decodes_back() {
        let message = Message::from_authored(&json!({
            "to": "ada@x.dev", "subject": "Reset", "text": "Go to https://a.dev/r?t=1",
            "html": "<a href=\"https://a.dev/r?t=1\">Reset</a>"
        }))
        .expect("valid");
        let eml = render(&message, &[], &from(), None, "m1", "Tue, 1 Sep 2026 00:00:00 +0000");
        assert!(eml.contains("From: \"Acme\" <no-reply@acme.dev>\r\n"), "{eml}");
        assert!(eml.contains("Content-Type: multipart/alternative; boundary=\"albedo-alt-m1\""));
        assert!(eml.contains("--albedo-alt-m1--\r\n"));
        let encoded = STANDARD.encode("Go to https://a.dev/r?t=1");
        assert!(eml.contains(&encoded), "the text part is base64 of the body");
        assert!(eml.lines().all(|l| l.len() <= 998), "RFC 5322 line limit");
        assert!(eml.is_ascii(), "7-bit clean");
    }

    #[test]
    fn attachments_wrap_the_body_in_multipart_mixed() {
        let message = Message::from_authored(&json!({
            "to": "ada@x.dev", "subject": "Invoice", "text": "attached", "html": "<p>attached</p>"
        }))
        .expect("valid");
        let files = [
            ResolvedAttachment {
                filename: "invoice.pdf".into(),
                content_type: "application/pdf".into(),
                bytes: b"%PDF-1.4".to_vec(),
            },
            ResolvedAttachment {
                filename: "reçu.txt".into(),
                content_type: "text/plain".into(),
                bytes: b"ok".to_vec(),
            },
        ];
        let eml = render(&message, &files, &from(), None, "m3", "d");
        assert!(eml.contains("Content-Type: multipart/mixed; boundary=\"albedo-mixed-m3\"\r\n"));
        assert!(eml.contains("Content-Type: multipart/alternative; boundary=\"albedo-alt-m3\""));
        assert!(eml.contains("Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n"));
        assert!(eml.contains("Content-Disposition: attachment; filename*=utf-8''re%C3%A7u.txt\r\n"));
        assert!(eml.contains(&STANDARD.encode(b"%PDF-1.4")));
        assert!(eml.ends_with("--albedo-mixed-m3--\r\n"));
        assert!(eml.is_ascii());
    }

    #[test]
    fn a_non_ascii_subject_is_an_encoded_word() {
        let message = Message::from_authored(&json!({
            "to": "Zoë <zoe@x.dev>", "subject": "Bienvenue, Zoë", "text": "é"
        }))
        .expect("valid");
        let eml = render(&message, &[], &from(), None, "m2", "d");
        assert!(eml.contains(&format!("Subject: =?utf-8?B?{}?=", STANDARD.encode("Bienvenue, Zoë"))));
        assert!(eml.contains(&format!("To: =?utf-8?B?{}?= <zoe@x.dev>", STANDARD.encode("Zoë"))));
        assert!(eml.is_ascii());
    }
}
