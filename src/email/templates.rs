//! The framework's own emails — the four AUTH sends.
//!
//! Plain, short, and the same shape every mainstream product converged on: what
//! happened, one link, how long it lasts, and what to do if it was not you. The
//! HTML is a single table-free block with inline styles, because mail clients
//! strip `<style>` and a transactional email is not where a layout engine
//! should be tested. The text part is complete on its own — it is what screen
//! readers, plain-text clients and spam filters read.
//!
//! Every interpolated value is escaped; the link is built by the server from a
//! configured origin and a token from our own alphabet, and is escaped anyway.

use serde::{Deserialize, Serialize};

use crate::email::message::{Mailbox, Message};

/// The placeholders an override may use.
pub const PLACEHOLDERS: &[&str] = &["app", "link", "expires"];

/// An app's replacement for one of the framework's emails. Each field is
/// optional; an absent one keeps the built-in.
///
/// `{{app}}`, `{{link}}` and `{{expires}}` are substituted. In `html` every
/// value is escaped, so an app name cannot become markup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateOverride {
    /// Subject line. May not contain `{{link}}` — a subject is shown in
    /// notifications and logged by mail systems, and the link is a credential.
    #[serde(default)]
    pub subject: Option<String>,
    /// Plain-text body. Must contain `{{link}}`.
    #[serde(default)]
    pub text: Option<String>,
    /// HTML body. Must contain `{{link}}`.
    #[serde(default)]
    pub html: Option<String>,
}

/// The `email.templates` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplatesDecl {
    /// The reset link.
    #[serde(default)]
    pub password_reset: Option<TemplateOverride>,
    /// The confirm-your-address link.
    #[serde(default)]
    pub verify_email: Option<TemplateOverride>,
    /// The sign-in link.
    #[serde(default)]
    pub magic_link: Option<TemplateOverride>,
    /// The "someone tried to sign up as you" notice.
    #[serde(default)]
    pub already_registered: Option<TemplateOverride>,
}

impl TemplatesDecl {
    /// The override for `kind`, if the app declared one.
    #[must_use]
    pub fn get(&self, kind: AuthEmail) -> Option<&TemplateOverride> {
        match kind {
            AuthEmail::PasswordReset => self.password_reset.as_ref(),
            AuthEmail::VerifyEmail => self.verify_email.as_ref(),
            AuthEmail::MagicLink => self.magic_link.as_ref(),
            AuthEmail::AlreadyRegistered => self.already_registered.as_ref(),
        }
    }

    /// Refuse an override that could never produce a working email.
    ///
    /// # Errors
    /// A message naming the template and field.
    pub fn validate(&self) -> Result<(), String> {
        let all = [
            ("passwordReset", &self.password_reset),
            ("verifyEmail", &self.verify_email),
            ("magicLink", &self.magic_link),
            ("alreadyRegistered", &self.already_registered),
        ];
        for (name, over) in all {
            let Some(over) = over else { continue };
            for (field, value, needs_link) in [
                ("subject", &over.subject, false),
                ("text", &over.text, true),
                ("html", &over.html, true),
            ] {
                let Some(value) = value else { continue };
                let at = format!("`email.templates.{name}.{field}`");
                let used = placeholders(value).map_err(|err| format!("{at}: {err}"))?;
                if let Some(unknown) = used.iter().find(|p| !PLACEHOLDERS.contains(&p.as_str())) {
                    return Err(format!(
                        "{at} uses `{{{{{unknown}}}}}`; the placeholders are {{{{app}}}}, \
                         {{{{link}}}} and {{{{expires}}}}"
                    ));
                }
                let has_link = used.iter().any(|p| p == "link");
                if needs_link && !has_link {
                    return Err(format!(
                        "{at} has no `{{{{link}}}}` — the email would arrive with nothing to click"
                    ));
                }
                if field == "subject" {
                    if has_link {
                        return Err(format!(
                            "{at} contains `{{{{link}}}}`; a subject is shown in notifications \
                             and mail logs, and the link is a credential"
                        ));
                    }
                    if value.chars().any(char::is_control) || value.trim().is_empty() {
                        return Err(format!("{at} must be one non-empty line"));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The names between `{{` and `}}`, in order.
fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open + 2..];
        let close = after
            .find("}}")
            .ok_or_else(|| "an opened `{{` is never closed".to_string())?;
        out.push(after[..close].trim().to_string());
        rest = &after[close + 2..];
    }
    Ok(out)
}

/// Substitute placeholders in one pass. `escape_values` for HTML. A value is
/// never re-scanned, so an app name containing `{{link}}` stays literal.
fn fill(template: &str, values: &[(&str, &str)], escape_values: bool) -> String {
    let mut out = String::with_capacity(template.len() + 128);
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            // Validated at build; kept literal rather than panicking.
            out.push_str(&rest[open..]);
            return out;
        };
        let name = after[..close].trim();
        match values.iter().find(|(key, _)| *key == name) {
            Some((_, value)) if escape_values => out.push_str(&escape(value)),
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[open..open + 2 + close + 2]),
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// Which AUTH email this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthEmail {
    /// A password reset link.
    PasswordReset,
    /// Confirm the address a password account signed up with.
    VerifyEmail,
    /// A one-time sign-in link.
    MagicLink,
    /// Somebody tried to sign up with an address that already has an account.
    /// Sent instead of telling the requester so — see `auth_routes`.
    AlreadyRegistered,
}

/// Build one. `app` is the product's name as the recipient knows it, `link` the
/// absolute URL, `ttl_minutes` how long the link works, `custom` the app's
/// override (validated at build), field by field over the built-in.
#[must_use]
pub fn render(
    kind: AuthEmail,
    to: Mailbox,
    app: &str,
    link: &str,
    ttl_minutes: u64,
    custom: Option<&TemplateOverride>,
) -> Message {
    let mut message = builtin(kind, to, app, link, ttl_minutes);
    let Some(custom) = custom else {
        return message;
    };
    let expires = human_duration(ttl_minutes);
    let values = [("app", app), ("link", link), ("expires", expires.as_str())];
    if let Some(subject) = &custom.subject {
        message.subject = fill(subject, &values, false);
    }
    if let Some(text) = &custom.text {
        message.text = Some(fill(text, &values, false));
    }
    if let Some(html) = &custom.html {
        message.html = Some(fill(html, &values, true));
    }
    message
}

fn builtin(kind: AuthEmail, to: Mailbox, app: &str, link: &str, ttl_minutes: u64) -> Message {
    let lasts = human_duration(ttl_minutes);
    let (subject, lead, action, tail) = match kind {
        AuthEmail::PasswordReset => (
            format!("Reset your {app} password"),
            format!("Someone asked to reset the password for your {app} account."),
            "Choose a new password",
            format!(
                "This link works once and expires in {lasts}. If you did not ask for this, \
                 ignore this email — your password has not changed."
            ),
        ),
        AuthEmail::VerifyEmail => (
            format!("Confirm your email for {app}"),
            format!("Confirm this address to finish setting up your {app} account."),
            "Confirm email",
            format!(
                "This link expires in {lasts}. If you did not create an account, ignore this \
                 email."
            ),
        ),
        AuthEmail::MagicLink => (
            format!("Your {app} sign-in link"),
            format!("Use this link to sign in to {app}."),
            "Sign in",
            format!(
                "This link works once and expires in {lasts}. If you did not ask for it, ignore \
                 this email — nobody can sign in without it."
            ),
        ),
        AuthEmail::AlreadyRegistered => (
            format!("Sign-up attempt for your {app} account"),
            format!(
                "Someone tried to create a {app} account with this address, which already has \
                 one."
            ),
            "Sign in instead",
            "If that was you, sign in with your existing password, or reset it from the \
             sign-in page. If it was not, you can ignore this email."
                .to_string(),
        ),
    };

    let text = format!("{lead}\n\n{action}:\n{link}\n\n{tail}\n\n— {app}\n");
    let html = format!(
        "<!DOCTYPE html><html><body style=\"margin:0;padding:24px;background:#f6f6f4;\
         font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;\
         color:#1a1a1a\"><div style=\"max-width:480px;margin:0 auto;background:#ffffff;\
         border-radius:8px;padding:32px\"><p style=\"margin:0 0 24px;font-size:16px;\
         line-height:1.5\">{lead}</p><p style=\"margin:0 0 24px\"><a href=\"{href}\" \
         style=\"display:inline-block;background:#1a1a1a;color:#ffffff;text-decoration:none;\
         padding:12px 20px;border-radius:6px;font-weight:600\">{action}</a></p><p \
         style=\"margin:0 0 16px;font-size:13px;line-height:1.5;color:#555\">{tail}</p><p \
         style=\"margin:0;font-size:12px;line-height:1.5;color:#888;word-break:break-all\">\
         Or paste this into your browser:<br>{href}</p></div></body></html>",
        lead = escape(&lead),
        href = escape(link),
        action = escape(action),
        tail = escape(&tail),
    );

    Message {
        from: None,
        to: vec![to],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: None,
        subject,
        text: Some(text),
        html: Some(html),
        attachments: Vec::new(),
    }
}

fn human_duration(minutes: u64) -> String {
    match minutes {
        m if m % 1440 == 0 && m >= 1440 => plural(m / 1440, "day"),
        m if m % 60 == 0 && m >= 60 => plural(m / 60, "hour"),
        m => plural(m, "minute"),
    }
}

fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
    }
}

/// Escape for HTML text and double-quoted attributes.
#[must_use]
pub fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 16);
    for c in raw.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to() -> Mailbox {
        Mailbox::parse("ada@x.dev").expect("parses")
    }

    #[test]
    fn every_template_is_a_valid_message_carrying_the_link_in_both_parts() {
        let link = "https://app.dev/_albedo/auth/password/reset?token=abc_-123";
        for kind in [
            AuthEmail::PasswordReset,
            AuthEmail::VerifyEmail,
            AuthEmail::MagicLink,
            AuthEmail::AlreadyRegistered,
        ] {
            let message = render(kind, to(), "Acme", link, 30, None);
            message.validate().expect("a template must pass the queue's own checks");
            assert!(message.text.as_deref().unwrap().contains(link), "{kind:?}");
            assert!(message.html.as_deref().unwrap().contains(link), "{kind:?}");
            assert!(message.subject.contains("Acme"), "{kind:?}");
        }
    }

    #[test]
    fn a_hostile_app_name_cannot_become_markup() {
        let message = render(AuthEmail::MagicLink, to(), "<script>x</script>", "https://a.dev/?t=1&u=2", 15, None);
        let html = message.html.unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("https://a.dev/?t=1&amp;u=2"), "{html}");
    }

    #[test]
    fn an_override_replaces_field_by_field_and_escapes_into_html() {
        let custom = TemplateOverride {
            subject: Some("{{app}}: new password".into()),
            text: None,
            html: Some("<h1>{{app}}</h1><a href=\"{{link}}\">go</a> ({{expires}})".into()),
        };
        let message = render(
            AuthEmail::PasswordReset,
            to(),
            "A&B <Co>",
            "https://a.dev/r?token=t&x=1",
            30,
            Some(&custom),
        );
        assert_eq!(message.subject, "A&B <Co>: new password", "subjects are plain text");
        assert!(message.text.unwrap().contains("https://a.dev/r?token=t&x=1"), "text kept the built-in");
        assert_eq!(
            message.html.unwrap(),
            "<h1>A&amp;B &lt;Co&gt;</h1><a href=\"https://a.dev/r?token=t&amp;x=1\">go</a> (30 minutes)"
        );
    }

    /// A value is substituted once and never re-scanned.
    #[test]
    fn a_value_containing_a_placeholder_stays_literal() {
        let custom = TemplateOverride {
            text: Some("{{app}} {{link}}".into()),
            ..TemplateOverride::default()
        };
        let message = render(AuthEmail::MagicLink, to(), "{{link}}", "https://a.dev/x", 15, Some(&custom));
        assert_eq!(message.text.unwrap(), "{{link}} https://a.dev/x");
    }

    #[test]
    fn an_override_that_could_not_work_is_refused_at_build() {
        let with = |over: TemplateOverride| TemplatesDecl {
            password_reset: Some(over),
            ..TemplatesDecl::default()
        }
        .validate();
        let text = |t: &str| TemplateOverride { text: Some(t.into()), ..TemplateOverride::default() };
        assert!(with(text("Click {{link}} within {{expires}}")).is_ok());
        assert!(with(text("Click here")).unwrap_err().contains("no `{{link}}`"));
        assert!(with(text("Click {{url}}")).unwrap_err().contains("{{url}}"));
        assert!(with(text("Click {{link")).unwrap_err().contains("never closed"));
        let subject = |s: &str| TemplateOverride { subject: Some(s.into()), ..TemplateOverride::default() };
        assert!(with(subject("Reset: {{link}}")).unwrap_err().contains("credential"));
        assert!(with(subject("a\nBcc: x@y.dev")).is_err());
        assert!(with(subject("Reset your {{app}} password")).is_ok());
    }

    #[test]
    fn durations_read_like_a_person_wrote_them() {
        assert_eq!(human_duration(15), "15 minutes");
        assert_eq!(human_duration(60), "1 hour");
        assert_eq!(human_duration(1440), "1 day");
        assert_eq!(human_duration(4320), "3 days");
        assert_eq!(human_duration(1), "1 minute");
    }
}
