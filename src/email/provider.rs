//! The four HTTP dialects, as pure functions: a message in, a described request
//! out; a status and body in, a verdict out.
//!
//! Nothing here touches the network. The server crate turns an
//! [`OutboundRequest`] into an APERTURE call — the same client, egress policy
//! and upstream budget as every other outbound byte — and hands the answer back
//! to [`classify`]. That split is what lets every provider's wire format be
//! pinned by a unit test instead of by a live account.
//!
//! ## What "retry" means for mail
//!
//! A send that timed out may have been accepted. Retrying it risks a duplicate;
//! not retrying risks a lost password reset. Duplicates are the lesser harm for
//! transactional mail, so transport failures retry — and where a provider
//! supports an idempotency key (Resend), the queue row's id is sent as one, so
//! a retry of the same row is the same send.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::auth::oauth::OutboundRequest;
use crate::email::declare::{EmailConfig, ProviderKind};
use crate::email::message::{Mailbox, Message, ResolvedAttachment};

/// How one send attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The provider accepted it. Carries the provider's message id when it
    /// returned one.
    Accepted(Option<String>),
    /// Worth trying again: throttled, a server error, or no answer.
    Retry(String),
    /// Trying again cannot help: a bad key, an unverified sender, a rejected
    /// recipient. Dead-letters immediately rather than after N identical failures.
    Reject(String),
}

/// Describe the request that sends `message` through `config`'s provider.
///
/// `key` is the resolved credential; `idempotency_key` is the queue row id;
/// `attachments` are the message's files with their bytes already read — the
/// message itself only names them.
/// Returns `None` for [`ProviderKind::Log`], which has no request.
#[must_use]
pub fn build_request(
    config: &EmailConfig,
    key: &str,
    message: &Message,
    attachments: &[ResolvedAttachment],
    idempotency_key: &str,
) -> Option<OutboundRequest> {
    let base = config.endpoint.as_ref()?.as_str().trim_end_matches('/');
    let from = message.from.as_ref().unwrap_or(&config.from);
    let reply_to = message.reply_to.as_ref().or(config.reply_to.as_ref());
    let text = message.text.as_deref().filter(|t| !t.is_empty());
    let html = message.html.as_deref().filter(|h| !h.is_empty());

    let request = match config.provider {
        ProviderKind::Log => return None,
        ProviderKind::Resend => {
            let mut body = Map::new();
            body.insert("from".into(), from.formatted().into());
            body.insert("to".into(), formatted_list(&message.to));
            if !message.cc.is_empty() {
                body.insert("cc".into(), formatted_list(&message.cc));
            }
            if !message.bcc.is_empty() {
                body.insert("bcc".into(), formatted_list(&message.bcc));
            }
            if let Some(reply_to) = reply_to {
                body.insert("reply_to".into(), json!([reply_to.formatted()]));
            }
            body.insert("subject".into(), message.subject.clone().into());
            insert_opt(&mut body, "text", text);
            insert_opt(&mut body, "html", html);
            if !attachments.is_empty() {
                body.insert(
                    "attachments".into(),
                    attachment_list(attachments, |a| {
                        json!({ "filename": a.filename, "content": STANDARD.encode(&a.bytes),
                                "content_type": a.content_type })
                    }),
                );
            }
            json_post(
                format!("{base}/emails"),
                vec![
                    ("authorization".into(), format!("Bearer {key}")),
                    // Resend holds a key for 24 h and answers a repeat with the
                    // first send's result instead of sending again.
                    ("idempotency-key".into(), idempotency_key.to_string()),
                ],
                &Value::Object(body),
            )
        }
        ProviderKind::Postmark => {
            let mut body = Map::new();
            body.insert("From".into(), from.formatted().into());
            body.insert("To".into(), joined(&message.to).into());
            if !message.cc.is_empty() {
                body.insert("Cc".into(), joined(&message.cc).into());
            }
            if !message.bcc.is_empty() {
                body.insert("Bcc".into(), joined(&message.bcc).into());
            }
            if let Some(reply_to) = reply_to {
                body.insert("ReplyTo".into(), reply_to.formatted().into());
            }
            body.insert("Subject".into(), message.subject.clone().into());
            insert_opt(&mut body, "TextBody", text);
            insert_opt(&mut body, "HtmlBody", html);
            if !attachments.is_empty() {
                body.insert(
                    "Attachments".into(),
                    attachment_list(attachments, |a| {
                        json!({ "Name": a.filename, "Content": STANDARD.encode(&a.bytes),
                                "ContentType": a.content_type })
                    }),
                );
            }
            // Transactional stream, explicitly: Postmark refuses broadcast mail
            // on it, which is the separation its deliverability depends on.
            body.insert("MessageStream".into(), "outbound".into());
            json_post(
                format!("{base}/email"),
                vec![
                    ("x-postmark-server-token".into(), key.to_string()),
                    ("accept".into(), "application/json".into()),
                ],
                &Value::Object(body),
            )
        }
        ProviderKind::Sendgrid => {
            let mut personalization = Map::new();
            personalization.insert("to".into(), object_list(&message.to));
            if !message.cc.is_empty() {
                personalization.insert("cc".into(), object_list(&message.cc));
            }
            if !message.bcc.is_empty() {
                personalization.insert("bcc".into(), object_list(&message.bcc));
            }
            // `text/plain` must precede `text/html`; SendGrid refuses the
            // other order with a 400.
            let content: Vec<Value> = [("text/plain", text), ("text/html", html)]
                .into_iter()
                .filter_map(|(kind, value)| value.map(|v| json!({ "type": kind, "value": v })))
                .collect();
            let mut body = Map::new();
            body.insert("personalizations".into(), json!([personalization]));
            body.insert("from".into(), mailbox_object(from));
            if let Some(reply_to) = reply_to {
                body.insert("reply_to".into(), mailbox_object(reply_to));
            }
            body.insert("subject".into(), message.subject.clone().into());
            body.insert("content".into(), Value::Array(content));
            if !attachments.is_empty() {
                body.insert(
                    "attachments".into(),
                    attachment_list(attachments, |a| {
                        json!({ "filename": a.filename, "content": STANDARD.encode(&a.bytes),
                                "type": a.content_type, "disposition": "attachment" })
                    }),
                );
            }
            json_post(
                format!("{base}/v3/mail/send"),
                vec![("authorization".into(), format!("Bearer {key}"))],
                &Value::Object(body),
            )
        }
        ProviderKind::Mailgun => {
            let domain = config.domain.as_deref()?;
            // Repeated fields, not a joined string: Mailgun parses a joined
            // value itself, and a display name with a comma in it splits.
            let mut fields: Vec<(&str, String)> = vec![("from", from.formatted())];
            for (field, list) in [("to", &message.to), ("cc", &message.cc), ("bcc", &message.bcc)] {
                fields.extend(list.iter().map(|mailbox| (field, mailbox.formatted())));
            }
            if let Some(reply_to) = reply_to {
                fields.push(("h:Reply-To", reply_to.formatted()));
            }
            fields.push(("subject", message.subject.clone()));
            if let Some(text) = text {
                fields.push(("text", text.to_string()));
            }
            if let Some(html) = html {
                fields.push(("html", html.to_string()));
            }
            // Files need multipart; everything else stays urlencoded, which is
            // smaller and is what Mailgun's own examples send.
            let (content_type, body) = if attachments.is_empty() {
                let mut form = url::form_urlencoded::Serializer::new(String::new());
                for (name, value) in &fields {
                    form.append_pair(name, value);
                }
                (
                    "application/x-www-form-urlencoded".to_string(),
                    form.finish().into_bytes(),
                )
            } else {
                // 🔒 Random per request, never derived from anything an app or a
                // user can choose (the row id can be author-supplied). A file
                // whose bytes contained a predictable boundary could open a new
                // part — `name="bcc"` — and add recipients.
                let boundary = format!("albedo-{:032x}", rand::random::<u128>());
                (
                    format!("multipart/form-data; boundary={boundary}"),
                    multipart(&boundary, &fields, attachments),
                )
            };
            OutboundRequest {
                method: "POST",
                url: format!("{base}/v3/{domain}/messages"),
                headers: vec![
                    (
                        "authorization".into(),
                        format!("Basic {}", STANDARD.encode(format!("api:{key}"))),
                    ),
                    ("content-type".into(), content_type),
                ],
                body: Some(body),
            }
        }
    };
    Some(request)
}

/// Read a provider's answer.
///
/// `2xx` is accepted. `408`, `425`, `429` and `5xx` are worth retrying; any
/// other `4xx` is the request itself being wrong — a revoked key, an unverified
/// sender domain, a recipient on a suppression list — and N more attempts are N
/// more identical refusals, so it dead-letters now with the provider's reason.
#[must_use]
pub fn classify(
    provider: ProviderKind,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
) -> SendOutcome {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    match status {
        200..=299 => {
            let id = match provider {
                ProviderKind::Resend | ProviderKind::Mailgun => parsed
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ProviderKind::Postmark => {
                    // Postmark reports some refusals as 200 with a non-zero
                    // `ErrorCode` in older API versions; honour that.
                    let code = parsed
                        .as_ref()
                        .and_then(|v| v.get("ErrorCode"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    if code != 0 {
                        return SendOutcome::Reject(reason(status, parsed.as_ref(), body));
                    }
                    parsed
                        .as_ref()
                        .and_then(|v| v.get("MessageID"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
                // SendGrid answers `202` with an empty body; the id is a header.
                ProviderKind::Sendgrid => headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("x-message-id"))
                    .map(|(_, value)| value.clone()),
                ProviderKind::Log => None,
            };
            SendOutcome::Accepted(id)
        }
        408 | 425 | 429 | 500..=599 => SendOutcome::Retry(reason(status, parsed.as_ref(), body)),
        _ => SendOutcome::Reject(reason(status, parsed.as_ref(), body)),
    }
}

/// The provider's own words, bounded — every one of them nests its message
/// differently, and the raw body is the fallback.
fn reason(status: u16, parsed: Option<&Value>, body: &[u8]) -> String {
    const MAX: usize = 300;
    let from_json = parsed.and_then(|v| {
        v.get("message")
            .or_else(|| v.get("Message"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                v.get("errors")
                    .and_then(|e| e.get(0))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
    });
    let mut text = from_json.unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    if text.len() > MAX {
        let mut cut = MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    format!("provider answered {status}: {}", text.trim())
}

fn json_post(url: String, mut headers: Vec<(String, String)>, body: &Value) -> OutboundRequest {
    headers.push(("content-type".into(), "application/json".into()));
    OutboundRequest {
        method: "POST",
        url,
        headers,
        body: Some(body.to_string().into_bytes()),
    }
}

fn attachment_list(
    attachments: &[ResolvedAttachment],
    shape: impl Fn(&ResolvedAttachment) -> Value,
) -> Value {
    Value::Array(attachments.iter().map(shape).collect())
}

/// RFC 7578 `multipart/form-data`. Filenames have already been refused if they
/// carry a quote or a control character, so they can sit in a quoted parameter
/// as they are. The boundary is 128 random bits chosen per request, so no
/// content can be crafted to contain it.
fn multipart(boundary: &str, fields: &[(&str, String)], files: &[ResolvedAttachment]) -> Vec<u8> {
    let size = fields.iter().map(|(_, v)| v.len() + 96).sum::<usize>()
        + files.iter().map(|f| f.bytes.len() + 160).sum::<usize>();
    let mut out = Vec::with_capacity(size);
    for (name, value) in fields {
        out.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                .as_bytes(),
        );
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for file in files {
        out.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"attachment\"; \
                 filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
                file.filename, file.content_type
            )
            .as_bytes(),
        );
        out.extend_from_slice(&file.bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

fn insert_opt(body: &mut Map<String, Value>, field: &str, value: Option<&str>) {
    if let Some(value) = value {
        body.insert(field.to_string(), value.into());
    }
}

fn formatted_list(list: &[Mailbox]) -> Value {
    Value::Array(list.iter().map(|m| m.formatted().into()).collect())
}

fn joined(list: &[Mailbox]) -> String {
    list.iter()
        .map(Mailbox::formatted)
        .collect::<Vec<_>>()
        .join(", ")
}

fn mailbox_object(mailbox: &Mailbox) -> Value {
    match &mailbox.name {
        Some(name) => json!({ "email": mailbox.address, "name": name }),
        None => json!({ "email": mailbox.address }),
    }
}

fn object_list(list: &[Mailbox]) -> Value {
    Value::Array(list.iter().map(mailbox_object).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::declare::EmailDecl;

    fn config(provider: &str, extra: Value) -> EmailConfig {
        let mut block = json!({
            "from": "Acme <no-reply@acme.dev>",
            "provider": provider,
            "apiKey": { "value": "sk_test" },
            "replyTo": "support@acme.dev"
        });
        if let (Value::Object(block), Value::Object(extra)) = (&mut block, extra) {
            block.extend(extra);
        }
        serde_json::from_value::<EmailDecl>(block)
            .expect("parses")
            .lower()
            .expect("lowers")
    }

    fn message() -> Message {
        Message::from_authored(&json!({
            "to": ["Ada <ada@x.dev>", "bob@x.dev"],
            "bcc": "audit@acme.dev",
            "subject": "Welcome",
            "text": "Hello",
            "html": "<p>Hello</p>"
        }))
        .expect("valid")
    }

    fn header<'a>(request: &'a OutboundRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn json_body(request: &OutboundRequest) -> Value {
        serde_json::from_slice(request.body.as_deref().expect("has a body")).expect("json")
    }

    #[test]
    fn resend_carries_the_row_id_as_its_idempotency_key() {
        let request = build_request(&config("resend", json!({})), "sk_test", &message(), &[], "row-7")
            .expect("a request");
        assert_eq!(request.url, "https://api.resend.com/emails");
        assert_eq!(header(&request, "authorization"), Some("Bearer sk_test"));
        assert_eq!(header(&request, "idempotency-key"), Some("row-7"));
        let body = json_body(&request);
        assert_eq!(body["from"], "\"Acme\" <no-reply@acme.dev>");
        assert_eq!(body["to"], json!(["\"Ada\" <ada@x.dev>", "bob@x.dev"]));
        assert_eq!(body["bcc"], json!(["audit@acme.dev"]));
        assert_eq!(body["reply_to"], json!(["support@acme.dev"]));
        assert_eq!(body["text"], "Hello");
        assert!(body.get("cc").is_none(), "empty lists are omitted");
    }

    #[test]
    fn postmark_uses_its_own_header_and_the_transactional_stream() {
        let request = build_request(&config("postmark", json!({})), "pm", &message(), &[], "r")
            .expect("a request");
        assert_eq!(request.url, "https://api.postmarkapp.com/email");
        assert_eq!(header(&request, "x-postmark-server-token"), Some("pm"));
        let body = json_body(&request);
        assert_eq!(body["To"], "\"Ada\" <ada@x.dev>, bob@x.dev");
        assert_eq!(body["TextBody"], "Hello");
        assert_eq!(body["HtmlBody"], "<p>Hello</p>");
        assert_eq!(body["MessageStream"], "outbound");
    }

    #[test]
    fn sendgrid_puts_plain_text_before_html() {
        let request = build_request(&config("sendgrid", json!({})), "sg", &message(), &[], "r")
            .expect("a request");
        assert_eq!(request.url, "https://api.sendgrid.com/v3/mail/send");
        let body = json_body(&request);
        assert_eq!(body["content"][0]["type"], "text/plain");
        assert_eq!(body["content"][1]["type"], "text/html");
        assert_eq!(body["personalizations"][0]["to"][0], json!({ "email": "ada@x.dev", "name": "Ada" }));
        assert_eq!(body["from"], json!({ "email": "no-reply@acme.dev", "name": "Acme" }));
    }

    #[test]
    fn mailgun_is_form_encoded_with_basic_auth_and_repeated_recipients() {
        let config = config("mailgun", json!({ "domain": "mg.acme.dev" }));
        let request = build_request(&config, "key-1", &message(), &[], "r").expect("a request");
        assert_eq!(request.url, "https://api.mailgun.net/v3/mg.acme.dev/messages");
        assert_eq!(
            header(&request, "authorization"),
            Some(format!("Basic {}", STANDARD.encode("api:key-1")).as_str())
        );
        let form: Vec<(String, String)> =
            url::form_urlencoded::parse(request.body.as_deref().expect("body"))
                .into_owned()
                .collect();
        let to: Vec<&str> = form.iter().filter(|(k, _)| k == "to").map(|(_, v)| v.as_str()).collect();
        assert_eq!(to, ["\"Ada\" <ada@x.dev>", "bob@x.dev"]);
        assert!(form.contains(&("h:Reply-To".into(), "support@acme.dev".into())));
    }

    #[test]
    fn a_per_message_sender_overrides_the_block() {
        let mut m = message();
        m.from = Some(Mailbox::parse("billing@acme.dev").expect("parses"));
        let request = build_request(&config("resend", json!({})), "k", &m, &[], "r").expect("a request");
        assert_eq!(json_body(&request)["from"], "billing@acme.dev");
    }

    fn files() -> Vec<ResolvedAttachment> {
        vec![ResolvedAttachment {
            filename: "report.pdf".into(),
            content_type: "application/pdf".into(),
            bytes: b"%PDF-1.4 hi".to_vec(),
        }]
    }

    /// Each JSON dialect names the same three facts differently.
    #[test]
    fn attachments_travel_in_each_json_dialect() {
        let encoded = STANDARD.encode(b"%PDF-1.4 hi");
        let cases = [
            ("resend", "attachments", ["filename", "content", "content_type"]),
            ("postmark", "Attachments", ["Name", "Content", "ContentType"]),
            ("sendgrid", "attachments", ["filename", "content", "type"]),
        ];
        for (provider, list, [name, content, kind]) in cases {
            let request = build_request(&config(provider, json!({})), "k", &message(), &files(), "r")
                .expect("a request");
            let body = json_body(&request);
            let first = &body[list][0];
            assert_eq!(first[name], "report.pdf", "{provider}");
            assert_eq!(first[content], encoded, "{provider}");
            assert_eq!(first[kind], "application/pdf", "{provider}");
        }
    }

    /// Mailgun takes files only as multipart, and the boundary must be one no
    /// content can have been crafted to contain.
    #[test]
    fn mailgun_switches_to_multipart_with_an_unpredictable_boundary() {
        let config = config("mailgun", json!({ "domain": "mg.acme.dev" }));
        let a = build_request(&config, "k", &message(), &files(), "row").expect("a request");
        let b = build_request(&config, "k", &message(), &files(), "row").expect("a request");
        let kind = header(&a, "content-type").expect("content type");
        let boundary = kind
            .strip_prefix("multipart/form-data; boundary=")
            .expect("multipart");
        assert_ne!(kind, header(&b, "content-type").unwrap(), "same row, different boundary");
        assert!(!boundary.contains("row"), "never derived from the row id");

        let body = String::from_utf8_lossy(a.body.as_deref().expect("body")).into_owned();
        assert!(body.contains(
            "Content-Disposition: form-data; name=\"attachment\"; filename=\"report.pdf\"\r\n\
             Content-Type: application/pdf\r\n\r\n%PDF-1.4 hi\r\n"
        ));
        assert!(body.contains("name=\"to\"\r\n\r\n\"Ada\" <ada@x.dev>\r\n"));
        assert!(body.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn log_describes_no_request() {
        let config = serde_json::from_value::<EmailDecl>(json!({ "from": "a@b.dev", "provider": "log" }))
            .expect("parses")
            .lower()
            .expect("lowers");
        assert!(build_request(&config, "", &message(), &[], "r").is_none());
    }

    #[test]
    fn classification_separates_retryable_from_permanent() {
        let ok = classify(ProviderKind::Resend, 200, &[], br#"{"id":"re_1"}"#);
        assert_eq!(ok, SendOutcome::Accepted(Some("re_1".into())));

        let sg = classify(
            ProviderKind::Sendgrid,
            202,
            &[("x-message-id".into(), "sg_1".into())],
            b"",
        );
        assert_eq!(sg, SendOutcome::Accepted(Some("sg_1".into())));

        for status in [408, 425, 429, 500, 502, 503] {
            assert!(
                matches!(classify(ProviderKind::Resend, status, &[], b"{}"), SendOutcome::Retry(_)),
                "{status} is transient"
            );
        }
        for status in [400, 401, 403, 404, 422] {
            assert!(
                matches!(classify(ProviderKind::Resend, status, &[], b"{}"), SendOutcome::Reject(_)),
                "{status} is permanent"
            );
        }
    }

    #[test]
    fn the_reject_reason_is_the_providers_own_words() {
        let SendOutcome::Reject(reason) = classify(
            ProviderKind::Resend,
            403,
            &[],
            br#"{"message":"The acme.dev domain is not verified"}"#,
        ) else {
            panic!("expected a reject");
        };
        assert!(reason.contains("403"), "{reason}");
        assert!(reason.contains("not verified"), "{reason}");

        let SendOutcome::Reject(sg) = classify(
            ProviderKind::Sendgrid,
            400,
            &[],
            br#"{"errors":[{"message":"Invalid from"}]}"#,
        ) else {
            panic!("expected a reject");
        };
        assert!(sg.contains("Invalid from"), "{sg}");
    }

    /// The four bodies each provider REALLY returned for a revoked key, captured
    /// 2026-09-23 by sending a well-formed request with a bogus credential. A
    /// bad key must dead-letter at once, carrying the provider's own words —
    /// not burn thirteen attempts over an hour saying "401".
    #[test]
    fn an_invalid_key_is_a_reject_with_the_providers_words_for_all_four() {
        let observed: [(ProviderKind, &[u8], &str); 4] = [
            (
                ProviderKind::Resend,
                br#"{"statusCode":401,"name":"validation_error","message":"API key is invalid"}"#,
                "API key is invalid",
            ),
            (
                ProviderKind::Postmark,
                br#"{"ErrorCode":10,"Message":"Request does not contain a valid Server token."}"#,
                "valid Server token",
            ),
            (
                ProviderKind::Sendgrid,
                br#"{"errors":[{"message":"The provided authorization grant is invalid, expired, or revoked","field":null,"help":null}]}"#,
                "authorization grant is invalid",
            ),
            (ProviderKind::Mailgun, b"Forbidden", "Forbidden"),
        ];
        for (provider, body, words) in observed {
            match classify(provider, 401, &[], body) {
                SendOutcome::Reject(reason) => {
                    assert!(reason.contains(words), "{provider:?}: {reason}");
                }
                other => panic!("{provider:?}: a bad key must not be retried, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_postmark_200_with_an_error_code_is_a_reject() {
        let outcome = classify(
            ProviderKind::Postmark,
            200,
            &[],
            br#"{"ErrorCode":406,"Message":"Inactive recipient"}"#,
        );
        assert!(matches!(outcome, SendOutcome::Reject(ref r) if r.contains("Inactive")), "{outcome:?}");
    }
}
