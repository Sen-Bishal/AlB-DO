//! The form-submit substrate — what a browser sends when no JavaScript ran.
//!
//! `AUTH.md` § 8.1.3: *the login form is the floor's first instance, not a login
//! special case.* This module is the piece both instances share. Two callers, one
//! implementation:
//!
//! | caller | what it is |
//! |---|---|
//! | `POST /_albedo/action/{name}` | an app's own `<form action="action:NAME">`, submitted with no JS — `TODO.md` item 8 |
//! | `POST /_albedo/auth/{provider}/…` | the sign-in and sign-up forms — AUTH P2 |
//!
//! Building those separately was the named avoidable mistake: they need the same
//! body decoder, the same return-path rule, the same content negotiation, and the
//! same redirect. Only the handler in the middle differs.
//!
//! ## Why any of this is needed
//!
//! Before this, an action reached the server as a bincode `ActionEnvelope` POSTed
//! by `assets/albedo-link-forms.js` — which means **the page's only write path
//! ran through JavaScript.** A form on a Tier-B route rendered without an
//! `action` attribute at all (the renderer replaced it with
//! `data-albedo-action`), so a browser with no JS submitted to the current URL
//! and got a `405`. "Zero JS" was true of the render and false of every mutation.
//!
//! A login page is where that stops being tolerable, because the credential
//! ceremony is the one form that has to work before the app's own JavaScript is
//! trusted to.
//!
//! ## The three rules this module owns
//!
//! 1. **A submitted body is decoded, never trusted.** `application/x-www-form-urlencoded`
//!    only, size-capped before parsing, and percent-decoding failures are lossy rather than
//!    fatal — a browser will not send a malformed body, so a malformed body is not a browser and
//!    should not get a distinct error to probe.
//! 2. **A redirect target is a rooted, same-origin path or it is `/`.** See [`ReturnPath`]; the
//!    whole open-redirect family lives in the two characters this refuses.
//! 3. **The response shape is negotiated, not guessed.** A `fetch()` caller asks for the opcode
//!    frame it can apply; a browser form submit gets `303 See Other` and re-renders the page it
//!    came from. Deciding this from the `Accept` header rather than from "did we see a header the
//!    client runtime sets" means the no-JS path is the *default* and the enhanced path opts in —
//!    which is the correct direction for a progressive-enhancement claim.

use axum::body::Body;
use axum::http::{header, HeaderMap, Response, StatusCode};

/// Hidden field naming the page a no-JS submit should return to.
///
/// Stamped by the renderers next to the CSRF input and filled at request time by
/// the same pass, for the same reason: Tier-A markup is baked at build time, so
/// the renderer has no request to read a path from.
///
/// **Re-exported, not restated.** The emitting side owns the name
/// (`transforms::form`), and this crate reads it back off the wire — the CSRF
/// input already demonstrated, expensively, what happens when one markup
/// contract is spelled out on both sides of a crate boundary.
pub use dom_render_compiler::transforms::form::RETURN_FIELD_NAME;

/// The one content type a browser produces for a `<form method="post">` without
/// `enctype`. `multipart/form-data` is item 7 (uploads) and is deliberately not
/// accepted here — decoding it would mean buffering a file into memory on a path
/// whose entire purpose is to be cheap.
pub const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// The media type a JS caller asks for when it wants an opcode frame back.
pub const FRAME_CONTENT_TYPE: &str = "application/octet-stream";

/// Hard cap on a form body, applied **before** parsing.
///
/// 64 KiB is far above any credential form and far below anything that could be
/// used to make the decoder expensive. The general request cap upstream is much
/// larger because it also covers action envelopes and uploads; this one is the
/// tighter bound that a *form* justifies, and a tighter bound is worth having on
/// the one route an anonymous stranger can reach without a session.
pub const MAX_FORM_BODY_BYTES: usize = 64 * 1024;

/// Longest return path accepted. Beyond this the value is dropped for `/` rather
/// than refused — a too-long path is a bug or a probe, and neither deserves a
/// distinct response.
const MAX_RETURN_PATH_BYTES: usize = 2048;

/// Why a submitted form body was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormDecodeError {
    /// The request declared a content type this path does not decode.
    UnsupportedContentType {
        /// What arrived, truncated to something safe to log.
        found: String,
    },
    /// The body exceeded [`MAX_FORM_BODY_BYTES`].
    TooLarge {
        /// The declared or observed length.
        len: usize,
    },
}

impl std::fmt::Display for FormDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedContentType { found } => write!(
                f,
                "a form submit must be `{FORM_CONTENT_TYPE}`; this request declared `{found}`"
            ),
            Self::TooLarge { len } => write!(
                f,
                "form body is {len} bytes, over the {MAX_FORM_BODY_BYTES}-byte limit"
            ),
        }
    }
}

impl std::error::Error for FormDecodeError {}

/// A decoded `application/x-www-form-urlencoded` body.
///
/// Field order is preserved and repeats are kept, because both are meaningful:
/// a same-name checkbox group submits several times, and "first one wins" is the
/// rule for a single-valued lookup. A map would have thrown that away.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormBody {
    fields: Vec<(String, String)>,
}

impl FormBody {
    /// Decode a submitted body.
    ///
    /// # Errors
    /// [`FormDecodeError`] when the content type is not a form encoding or the
    /// body is over [`MAX_FORM_BODY_BYTES`].
    pub fn decode(content_type: Option<&str>, bytes: &[u8]) -> Result<Self, FormDecodeError> {
        let declared = content_type.unwrap_or_default();
        if !is_form_content_type(declared) {
            return Err(FormDecodeError::UnsupportedContentType {
                found: declared.chars().take(64).collect(),
            });
        }
        if bytes.len() > MAX_FORM_BODY_BYTES {
            return Err(FormDecodeError::TooLarge { len: bytes.len() });
        }
        Ok(Self::parse(bytes))
    }

    /// Parse without the content-type or size gate.
    ///
    /// Percent-decoding is lossy (`from_utf8_lossy`) rather than fallible on
    /// purpose. A browser encodes correctly by construction, so invalid UTF-8
    /// here is a hand-built request — and giving it a distinct error would hand a
    /// prober a way to distinguish "this field exists" from "this body is
    /// malformed" before any handler runs.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Self {
        let fields = url::form_urlencoded::parse(bytes)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        Self { fields }
    }

    /// First value submitted under `name`, if any.
    ///
    /// First rather than last: a hidden field the renderer stamped comes before
    /// anything an author appended, so "first wins" means a later duplicate
    /// cannot shadow `_csrf` or [`RETURN_FIELD_NAME`]. HTTP parameter pollution
    /// is exactly the trick this closes, and it is closed by the *choice of which
    /// duplicate to read*, not by a check.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every value submitted under `name`, in order.
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Every `(name, value)` pair, in submitted order.
    #[must_use]
    pub fn pairs(&self) -> &[(String, String)] {
        &self.fields
    }

    /// Whether anything was submitted at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The return path this submit named, or `/` when it named none or named one
    /// that is not ours to redirect to.
    #[must_use]
    pub fn return_path(&self) -> ReturnPath {
        self.get(RETURN_FIELD_NAME)
            .and_then(ReturnPath::parse)
            .unwrap_or_else(ReturnPath::root)
    }

    /// Re-encode as the flat JSON object the action payload carries.
    ///
    /// **This is the interop point**, and the reason it exists rather than each
    /// caller doing its own thing: `assets/albedo-link-forms.js` already
    /// serializes a form to exactly this shape for the JS path, so producing it
    /// here means an action handler sees the *same bytes* whether or not the
    /// browser ran JavaScript. A handler that behaves differently on the no-JS
    /// path is the defect this shape exists to make impossible.
    ///
    /// Repeated names coalesce into an array, matching that file's `appendField`.
    /// Values stay **strings** — a browser submits `on` for a checked box, and
    /// deciding that means `true` requires the field's declared kind, which lives
    /// in the compiled form extract and not in an HTTP body.
    #[must_use]
    pub fn to_action_payload(&self) -> Vec<u8> {
        let mut object = serde_json::Map::new();
        for (key, value) in &self.fields {
            let incoming = serde_json::Value::String(value.clone());
            match object.get_mut(key) {
                None => {
                    object.insert(key.clone(), incoming);
                }
                Some(serde_json::Value::Array(existing)) => existing.push(incoming),
                Some(slot) => {
                    let first = slot.take();
                    *slot = serde_json::Value::Array(vec![first, incoming]);
                }
            }
        }
        serde_json::to_vec(&serde_json::Value::Object(object)).unwrap_or_else(|_| b"{}".to_vec())
    }
}

/// True for `application/x-www-form-urlencoded`, with or without parameters.
///
/// Split on `;` because a browser sends `…; charset=UTF-8` and matching the
/// whole header string would reject every real submit.
#[must_use]
pub fn is_form_content_type(raw: &str) -> bool {
    raw.split(';')
        .next()
        .map(str::trim)
        .is_some_and(|media| media.eq_ignore_ascii_case(FORM_CONTENT_TYPE))
}

/// A validated redirect target: a rooted path on **this** origin.
///
/// ## What this type is for
///
/// A form submit has to send the browser somewhere afterwards, and the somewhere
/// is supplied by the request. That is the open-redirect shape in its purest
/// form — and for a *login* form it is worse than the usual case, because
/// "authenticate, then get bounced to an attacker's page" is the classic
/// credential-phishing chain, with the victim already primed to trust the flow.
///
/// So the value is not sanitized, it is **parsed**: either it is a rooted path on
/// this origin, or there is no value and the caller falls back to `/`.
///
/// ## The two characters that do all the work
///
/// `//evil.example` and `/\evil.example` are both *paths* by any naive check —
/// they start with `/` — and both are resolved by browsers as **protocol-relative
/// URLs pointing at another host**. A check that only asserts a leading `/` lets
/// both through. That is why the refusal is written as an explicit second-byte
/// test rather than as "starts with `/`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnPath(String);

impl ReturnPath {
    /// The fallback: the app's root.
    #[must_use]
    pub fn root() -> Self {
        Self("/".to_string())
    }

    /// Parse a candidate, returning `None` when it is not a same-origin path.
    ///
    /// Returns `None` — never a sanitized approximation of the input. Trimming a
    /// hostile value produces something that *looks* accepted, and the accepted
    /// thing is then whatever survived the trimming.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.len() > MAX_RETURN_PATH_BYTES {
            return None;
        }
        let mut bytes = raw.bytes();
        if bytes.next() != Some(b'/') {
            return None;
        }
        // `//host` and `/\host` are protocol-relative: same-origin by inspection,
        // cross-origin in every browser.
        if matches!(bytes.next(), Some(b'/') | Some(b'\\')) {
            return None;
        }
        // A control character in a `Location` value is header injection. ASCII
        // only, because a non-ASCII path must arrive percent-encoded — a browser
        // encodes it that way, and accepting raw UTF-8 here would mean emitting a
        // header value the HTTP layer has to re-encode or refuse.
        if raw
            .bytes()
            .any(|byte| byte.is_ascii_control() || !byte.is_ascii() || byte == b' ')
        {
            return None;
        }
        Some(Self(raw.to_string()))
    }

    /// The path, ready for a `Location` header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path with any fragment removed — what a browser actually requests,
    /// and so what a render of this page is keyed by.
    #[must_use]
    pub fn without_fragment(&self) -> &str {
        self.0.split('#').next().unwrap_or("/")
    }

    /// The same page, scrolled to `anchor`. Re-parsed rather than concatenated,
    /// so the result is held to every rule the original was.
    #[must_use]
    pub fn at_anchor(&self, anchor: &str) -> Self {
        Self::parse(&format!("{}#{anchor}", self.without_fragment())).unwrap_or_else(|| self.clone())
    }
}

impl Default for ReturnPath {
    fn default() -> Self {
        Self::root()
    }
}

/// What the caller wants back from a submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wants {
    /// A binary `OpcodeFrame`, applied in place. The JS path.
    Frame,
    /// `303 See Other` back to the submitting page. The browser path, and the
    /// default — a client that says nothing is a browser.
    Redirect,
}

/// Decide the response shape from the request's `Accept` header.
///
/// The frame is opt-in and the redirect is the default, which is the direction
/// that keeps the no-JS path honest: forget to negotiate and you get the
/// browser-correct answer, not a binary blob rendered as mojibake.
///
/// A browser's `Accept` on a form submit begins `text/html,…` and — importantly
/// — often ends in `*/*`, so "does it accept octet-stream" cannot be answered by
/// a substring search for `*/*`. The test is for the concrete type, spelled out.
#[must_use]
pub fn negotiate(headers: &HeaderMap) -> Wants {
    negotiate_accept(
        headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    )
}

/// [`negotiate`] over a raw `Accept` value.
///
/// Exists because the two callers hold the header differently — the dispatcher
/// has an `http::HeaderMap`, a handler has [`crate::lifecycle::RequestContext`]'s
/// lowercased `HashMap`. One rule, two ways in.
#[must_use]
pub fn negotiate_accept(accept: &str) -> Wants {
    let asks_for_frame = accept
        .split(',')
        .map(|entry| entry.split(';').next().unwrap_or_default().trim())
        .any(|media| media.eq_ignore_ascii_case(FRAME_CONTENT_TYPE));
    if asks_for_frame {
        Wants::Frame
    } else {
        Wants::Redirect
    }
}

/// `303 See Other` to `path`.
///
/// 303 and not 302: 303 requires the follow-up request to be a `GET` regardless
/// of the method that produced it, which is the entire POST/Redirect/GET
/// pattern. A 302 leaves that to the client's discretion, and the discretion
/// historically went both ways.
///
/// `no-store` because the response depends on a session cookie and on a mutation
/// that just happened; there is nothing here a cache should ever replay.
#[must_use]
pub fn see_other(path: &ReturnPath) -> Response<Body> {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, path.as_str())
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::VARY, "Cookie")
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

// ── Form feedback across the redirect ──────────────────────────────────────

/// Cookie carrying a form's feedback from a JS-less submit to the render its
/// `303` lands on. `__Host-` for the same reasons as the session cookie: no
/// sibling subdomain can plant one, so the only writer is this server.
pub const FEEDBACK_COOKIE: &str = "__Host-albedo-feedback";

/// Encoded-size ceiling: far above any sentence a form says back, far under the
/// 4 KiB per cookie every browser guarantees.
const FEEDBACK_MAX_BYTES: usize = 1536;

/// Long enough to survive the redirect; short enough that feedback the reader
/// never came back for cannot surface on some later visit.
const FEEDBACK_MAX_AGE_SECS: u32 = 60;

/// A form's feedback, carried across a JS-less submit's redirect.
///
/// The enhanced path applies the action's opcode frame in place — including the
/// `SetText`s `runtime::form_result` projects onto the form's error and success
/// sinks. A browser with no JavaScript gets `303` instead, and until this the
/// frame was simply dropped: the no-JS reader learned **nothing** about their
/// submit — not that it worked, not what was wrong. That is the asymmetry
/// [`run_form_action_request`](crate::handlers::action) exists to rule out.
///
/// So the frame is not dropped; it is **deferred**. Its non-empty `SetText`s ride
/// this cookie to the next render of the return path, where
/// `transforms::form::fill_form_feedback` writes them into the sinks that render
/// reserved. Same opcodes, same ids, same text — applied on the server one
/// request later instead of in the browser in place.
///
/// The cookie is not trusted for anything. A reader can only rewrite their own,
/// the fill only ever writes into an *empty feedback sink*, and it escapes what
/// it writes — so the worst a forged value achieves is odd text on the forger's
/// own screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFeedback {
    path: String,
    entries: Vec<(u32, String)>,
}

impl FormFeedback {
    /// What the frame would have painted: every `SetText` that carries text.
    /// Empty ones are clears, and a fresh render's sinks are already empty.
    ///
    /// Read **last-first**, because the projection is appended after the
    /// handler's own opcodes — so when the budget runs out, what is lost is
    /// never the form's feedback. `None` when there is nothing to say.
    pub fn from_frame(
        path: &ReturnPath,
        frame: &[dom_render_compiler::ir::opcode::Instruction],
    ) -> Option<Self> {
        use dom_render_compiler::ir::opcode::Instruction;
        let path = path.without_fragment().to_string();
        let mut budget = FEEDBACK_MAX_BYTES.saturating_sub(encoded_len("p", &path));
        let mut entries = Vec::new();
        for op in frame.iter().rev() {
            let Instruction::SetText { stable_id, text } = op else { continue };
            let Ok(text) = std::str::from_utf8(text) else { continue };
            let cost = encoded_len(&stable_id.0.to_string(), text);
            if text.is_empty() || cost > budget {
                continue;
            }
            budget -= cost;
            entries.push((stable_id.0, text.to_string()));
        }
        (!entries.is_empty()).then_some(Self { path, entries })
    }

    /// One message for one sink: a framework endpoint's refusal, carried back to
    /// the form that posted it (`auth_routes::into_form_feedback`). `None` when
    /// there is nothing to say or it would not fit the cookie.
    pub fn message(path: &ReturnPath, sink: u32, text: &str) -> Option<Self> {
        let path = path.without_fragment().to_string();
        let cost = encoded_len("p", &path) + encoded_len(&sink.to_string(), text);
        (!text.is_empty() && cost <= FEEDBACK_MAX_BYTES).then(|| Self {
            path,
            entries: vec![(sink, text.to_string())],
        })
    }

    /// The `Set-Cookie` value that carries this feedback across the redirect.
    /// Form encoding needs no second pass: its whole alphabet — alphanumerics,
    /// `*-._`, `%XX`, `+`, `=`, `&` — sits inside RFC 6265's cookie-octet set.
    pub fn set_cookie(&self) -> String {
        let mut value = url::form_urlencoded::Serializer::new(String::new());
        value.append_pair("p", &self.path);
        for (id, text) in &self.entries {
            value.append_pair(&id.to_string(), text);
        }
        format!(
            "{FEEDBACK_COOKIE}={}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={FEEDBACK_MAX_AGE_SECS}",
            value.finish()
        )
    }

    /// Feedback is one-shot: the render that consumes it also expires it.
    pub fn clear_cookie() -> String {
        format!("{FEEDBACK_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
    }

    /// `Some(feedback)` when the request carries the cookie — `Some` with no
    /// entries if its value is mangled, so the caller still expires it. A bad
    /// cookie costs a message, never a page.
    pub fn read(headers: &HeaderMap) -> Option<Self> {
        let raw = headers.get(header::COOKIE)?.to_str().ok()?;
        let (_, value) = dom_render_compiler::auth::cookie_entries(raw)
            .find(|(name, _)| *name == FEEDBACK_COOKIE)?;
        let mut path = String::new();
        let mut entries = Vec::new();
        for (key, text) in url::form_urlencoded::parse(value.as_bytes()) {
            if key == "p" {
                path = text.into_owned();
            } else if let Ok(id) = key.parse::<u32>() {
                entries.push((id, text.into_owned()));
            }
        }
        Some(Self { path, entries })
    }

    /// The entries to paint on a render of `request_path` — only the page the
    /// submit returned to. Anywhere else the feedback is stale by definition.
    pub fn entries_for(&self, request_path: &str) -> &[(u32, String)] {
        if self.path == request_path {
            &self.entries
        } else {
            &[]
        }
    }
}

/// Bytes `key=value&` costs once form-encoded — the exact unit of the budget.
fn encoded_len(key: &str, value: &str) -> usize {
    let encoded = |s: &str| {
        url::form_urlencoded::byte_serialize(s.as_bytes())
            .map(str::len)
            .sum::<usize>()
    };
    encoded(key) + encoded(value) + 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_form_submit_decodes() {
        let body = FormBody::decode(
            Some("application/x-www-form-urlencoded"),
            b"email=ada%40example.com&password=hunter2",
        )
        .expect("decodes");
        assert_eq!(body.get("email"), Some("ada@example.com"));
        assert_eq!(body.get("password"), Some("hunter2"));
    }

    /// Every real browser appends a charset parameter. Matching the whole header
    /// string would reject every genuine submit and accept only hand-built ones.
    #[test]
    fn the_charset_parameter_does_not_break_the_content_type_check() {
        assert!(is_form_content_type(
            "application/x-www-form-urlencoded; charset=UTF-8"
        ));
        assert!(is_form_content_type("APPLICATION/X-WWW-FORM-URLENCODED"));
        assert!(!is_form_content_type("application/json"));
        assert!(!is_form_content_type("multipart/form-data; boundary=x"));
    }

    #[test]
    fn a_json_body_is_refused_rather_than_guessed_at() {
        let error = FormBody::decode(Some("application/json"), br#"{"email":"a"}"#)
            .expect_err("not a form encoding");
        assert!(matches!(
            error,
            FormDecodeError::UnsupportedContentType { .. }
        ));
    }

    #[test]
    fn an_oversized_body_is_refused_before_it_is_parsed() {
        let huge = vec![b'a'; MAX_FORM_BODY_BYTES + 1];
        assert!(matches!(
            FormBody::decode(Some(FORM_CONTENT_TYPE), &huge),
            Err(FormDecodeError::TooLarge { .. })
        ));
    }

    /// HTTP parameter pollution: a second `_csrf` appended after the stamped one
    /// must not be the one that is read. Closed by reading the *first*, which is
    /// the one the renderer emitted.
    #[test]
    fn the_first_value_wins_so_a_duplicate_cannot_shadow_a_stamped_field() {
        let body = FormBody::parse(b"_csrf=real&_csrf=forged&x=1");
        assert_eq!(body.get("_csrf"), Some("real"));
        assert_eq!(body.all("_csrf").collect::<Vec<_>>(), vec!["real", "forged"]);
    }

    #[test]
    fn repeated_fields_coalesce_into_an_array_like_the_js_serializer() {
        let payload = FormBody::parse(b"tag=a&tag=b&name=x").to_action_payload();
        let value: serde_json::Value = serde_json::from_slice(&payload).expect("json");
        assert_eq!(value["tag"], serde_json::json!(["a", "b"]));
        assert_eq!(value["name"], serde_json::json!("x"));
    }

    #[test]
    fn an_empty_body_is_an_empty_object_not_an_error() {
        let body = FormBody::parse(b"");
        assert!(body.is_empty());
        assert_eq!(body.to_action_payload(), b"{}");
    }

    // ── ReturnPath ───────────────────────────────────────────────────

    #[test]
    fn a_rooted_path_survives_with_its_query_intact() {
        let path = ReturnPath::parse("/account/settings?tab=security").expect("accepted");
        assert_eq!(path.as_str(), "/account/settings?tab=security");
    }

    /// **The open-redirect test.** Every entry here starts with `/` and every one
    /// of them leaves this origin. A leading-slash check alone accepts all of
    /// them, which is why the rule is written as it is.
    #[test]
    fn protocol_relative_and_absolute_targets_are_refused() {
        for hostile in [
            "//evil.example/login",
            "/\\evil.example/login",
            "https://evil.example",
            "http://evil.example",
            "//evil.example",
            "javascript:alert(1)",
            "evil.example",
            "",
        ] {
            assert_eq!(
                ReturnPath::parse(hostile),
                None,
                "`{hostile}` must not be a redirect target"
            );
        }
    }

    /// A `Location` value carrying CR/LF is response splitting.
    #[test]
    fn control_characters_are_refused() {
        for hostile in [
            "/ok\r\nSet-Cookie: a=b",
            "/ok\nX-Injected: 1",
            "/ok\0",
            "/two words",
        ] {
            assert_eq!(ReturnPath::parse(hostile), None, "`{hostile}`");
        }
    }

    #[test]
    fn an_absurdly_long_path_falls_back_rather_than_erroring() {
        let long = format!("/{}", "a".repeat(MAX_RETURN_PATH_BYTES));
        assert_eq!(ReturnPath::parse(&long), None);
        let body = FormBody::parse(format!("{RETURN_FIELD_NAME}={long}").as_bytes());
        assert_eq!(body.return_path(), ReturnPath::root());
    }

    #[test]
    fn a_submit_naming_no_return_path_goes_to_the_root() {
        assert_eq!(FormBody::parse(b"x=1").return_path(), ReturnPath::root());
        assert_eq!(
            FormBody::parse(b"_albedo_return=%2Fmine").return_path().as_str(),
            "/mine"
        );
    }

    // ── negotiation ──────────────────────────────────────────────────

    /// A browser is the default. This is the assertion that keeps the
    /// progressive-enhancement claim true: nothing has to be present for the
    /// no-JS path to be chosen.
    #[test]
    fn a_browser_accept_header_gets_a_redirect() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
                .parse()
                .unwrap(),
        );
        assert_eq!(negotiate(&headers), Wants::Redirect);
        // …and so does a request that says nothing at all.
        assert_eq!(negotiate(&HeaderMap::new()), Wants::Redirect);
    }

    /// `*/*` technically accepts octet-stream. It must not be read as *asking*
    /// for it, or every browser submit would download a binary frame.
    #[test]
    fn a_wildcard_accept_is_not_a_request_for_a_frame() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "*/*".parse().unwrap());
        assert_eq!(negotiate(&headers), Wants::Redirect);
    }

    #[test]
    fn an_explicit_octet_stream_accept_gets_a_frame() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            "application/octet-stream".parse().unwrap(),
        );
        assert_eq!(negotiate(&headers), Wants::Frame);
        headers.insert(
            header::ACCEPT,
            "application/octet-stream;q=1.0, */*;q=0.1".parse().unwrap(),
        );
        assert_eq!(negotiate(&headers), Wants::Frame);
    }

    // ── the redirect itself ──────────────────────────────────────────

    #[test]
    fn the_redirect_is_a_303_that_cannot_be_cached() {
        let response = see_other(&ReturnPath::parse("/mine").unwrap());
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/mine",
            "the browser must land back on the page that submitted"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        assert_eq!(response.headers().get(header::VARY).unwrap(), "Cookie");
    }

    // ── feedback across the redirect ─────────────────────────────────

    use dom_render_compiler::ir::opcode::{Instruction, StableId};

    fn set_text(id: u32, text: &str) -> Instruction {
        Instruction::SetText { stable_id: StableId(id), text: text.as_bytes().to_vec() }
    }

    /// Replay a `Set-Cookie` value the way a browser would send it back.
    fn sent_back(set_cookie: &str) -> HeaderMap {
        let pair = set_cookie.split(';').next().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("theme=dark; {pair}").parse().unwrap());
        headers
    }

    #[test]
    fn feedback_survives_the_round_trip_through_a_real_cookie() {
        let path = ReturnPath::parse("/room/42?tab=chat").unwrap();
        let frame = [set_text(7, ""), set_text(9, "You're on the list; see you & soon.")];
        let feedback = FormFeedback::from_frame(&path, &frame).expect("one sink has text");

        let cookie = feedback.set_cookie();
        for attr in ["Path=/", "HttpOnly", "Secure", "SameSite=Lax", "Max-Age=60"] {
            assert!(cookie.contains(attr), "missing {attr}: {cookie}");
        }
        let read = FormFeedback::read(&sent_back(&cookie)).expect("cookie is present");
        // The clear (empty text) was never carried; the message survives `;` and `&`.
        assert_eq!(read.entries_for("/room/42?tab=chat"), &[(9, "You're on the list; see you & soon.".to_string())]);
        // Keyed by the page it was issued for, and nowhere else.
        assert!(read.entries_for("/").is_empty());
    }

    #[test]
    fn a_frame_with_nothing_to_say_carries_nothing() {
        let path = ReturnPath::root();
        assert!(FormFeedback::from_frame(&path, &[set_text(1, "")]).is_none());
        assert!(FormFeedback::from_frame(&path, &[]).is_none());
    }

    #[test]
    fn the_budget_keeps_the_projection_which_comes_last() {
        // A handler's own large SetText first, the form's feedback last: when
        // both cannot fit, the one that goes is never the feedback.
        let huge = "x".repeat(FEEDBACK_MAX_BYTES);
        let frame = [set_text(1, &huge), set_text(2, "Enter a real address.")];
        let feedback = FormFeedback::from_frame(&ReturnPath::root(), &frame).unwrap();
        assert_eq!(feedback.entries_for("/"), &[(2, "Enter a real address.".to_string())]);
        assert!(feedback.set_cookie().len() < FEEDBACK_MAX_BYTES + 128);
    }

    #[test]
    fn a_mangled_cookie_still_reads_as_present_so_it_is_expired() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("{FEEDBACK_COOKIE}=%%%;junk").parse().unwrap());
        let read = FormFeedback::read(&headers).expect("present, so the render expires it");
        assert!(read.entries_for("/").is_empty());
        assert!(FormFeedback::clear_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn the_anchor_replaces_any_fragment_and_keeps_the_query() {
        let path = ReturnPath::parse("/room/42?tab=chat#old").unwrap();
        assert_eq!(path.at_anchor("albedo-feedback-join").as_str(), "/room/42?tab=chat#albedo-feedback-join");
        assert_eq!(path.without_fragment(), "/room/42?tab=chat");
    }
}
