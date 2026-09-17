//! What a middleware decided, validated before any of it touches a response.
//!
//! The JS helpers (`next`, `redirect`, `rewrite`, `respond` from
//! `"albedo/middleware"`) build a branded plain object; the engine hands it back
//! as JSON and this module is the only thing that turns it into something the
//! server acts on.
//!
//! 🔴 **Every field is checked here, in Rust, and nothing is trusted because the
//! helper built it.** The helpers are a convenience, not a boundary: a
//! middleware can return a hand-written object, and a header value with a CR/LF
//! in it is response splitting whether or not a helper produced it.

use std::collections::BTreeMap;

use serde_json::Value;

/// The brand the helpers stamp. A returned object without it is refused, so a
/// middleware that returns a stray value (`return user`) fails loudly instead of
/// being read as "continue".
pub const BRAND: &str = "__albedo_middleware";

/// Headers a middleware may not set. Each is owned by the transport: getting one
/// wrong corrupts the framing of the response rather than its content.
const TRANSPORT_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
];

/// Every cookie the framework sets starts with this. A middleware may not set
/// one: the session and pending-login cookies are written only by Rust.
pub const FRAMEWORK_COOKIE_PREFIX: &str = "__Host-albedo";

/// A validated middleware decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Carry on to the route, adding `headers` to whatever it answers.
    Next { headers: Headers },
    /// Answer with a redirect and stop.
    Redirect {
        location: String,
        status: u16,
        headers: Headers,
    },
    /// Serve `path` (and `query`, when given) instead, adding `headers`.
    Rewrite {
        path: String,
        query: Option<String>,
        headers: Headers,
    },
    /// Answer with this and stop.
    Respond {
        status: u16,
        body: String,
        headers: Headers,
    },
}

/// Response headers, lower-cased. `set-cookie` may repeat, so values are a list.
pub type Headers = BTreeMap<String, Vec<String>>;

impl Outcome {
    /// The headers every outcome carries.
    #[must_use]
    pub fn headers(&self) -> &Headers {
        match self {
            Self::Next { headers }
            | Self::Redirect { headers, .. }
            | Self::Rewrite { headers, .. }
            | Self::Respond { headers, .. } => headers,
        }
    }
}

/// Lower a middleware's return value. `null` (it returned nothing) continues.
///
/// # Errors
/// A message an author can act on, naming the field that was wrong.
pub fn decode(value: &Value) -> Result<Outcome, String> {
    let object = match value {
        Value::Null => {
            return Ok(Outcome::Next {
                headers: Headers::new(),
            })
        }
        Value::Object(object) => object,
        other => {
            return Err(format!(
                "middleware returned {}; return nothing to continue, or one of next(), \
                 redirect(), rewrite() or respond() from \"albedo/middleware\"",
                describe(other)
            ))
        }
    };

    let Some(kind) = object.get(BRAND).and_then(Value::as_str) else {
        return Err(
            "middleware returned a plain object; return nothing to continue, or one of next(), \
             redirect(), rewrite() or respond() from \"albedo/middleware\""
                .to_string(),
        );
    };

    let headers = decode_headers(object.get("headers"))?;

    match kind {
        "next" => Ok(Outcome::Next { headers }),
        "redirect" => {
            let location = string_field(object, "location", "redirect(location)")?;
            validate_location(&location)?;
            let status = match object.get("status") {
                None | Some(Value::Null) => 307,
                Some(value) => {
                    let status = status_of(value, "redirect status")?;
                    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                        return Err(format!(
                            "redirect status {status} is not a redirect; use 301, 302, 303, 307 \
                             or 308"
                        ));
                    }
                    status
                }
            };
            Ok(Outcome::Redirect {
                location,
                status,
                headers,
            })
        }
        "rewrite" => {
            let target = string_field(object, "path", "rewrite(path)")?;
            let (path, query) = validate_rewrite(&target)?;
            Ok(Outcome::Rewrite {
                path,
                query,
                headers,
            })
        }
        "respond" => {
            let status = status_of(
                object.get("status").unwrap_or(&Value::Null),
                "respond status",
            )?;
            if !(200..=599).contains(&status) {
                return Err(format!(
                    "respond status {status} is outside 200–599; an informational status cannot \
                     end a request"
                ));
            }
            let body = match object.get("body") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(body)) => body.clone(),
                Some(other) => {
                    return Err(format!(
                        "respond body must be a string, got {}; JSON.stringify an object first",
                        describe(other)
                    ))
                }
            };
            Ok(Outcome::Respond {
                status,
                body,
                headers,
            })
        }
        other => Err(format!("unknown middleware outcome `{other}`")),
    }
}

fn decode_headers(value: Option<&Value>) -> Result<Headers, String> {
    let mut headers = Headers::new();
    let object = match value {
        None | Some(Value::Null) => return Ok(headers),
        Some(Value::Object(object)) => object,
        Some(other) => {
            return Err(format!(
                "headers must be an object of name → string, got {}",
                describe(other)
            ))
        }
    };
    for (name, value) in object {
        let lower = name.to_ascii_lowercase();
        if lower.is_empty() || !lower.bytes().all(is_token_byte) {
            return Err(format!("`{name}` is not a valid header name"));
        }
        if TRANSPORT_HEADERS.contains(&lower.as_str()) {
            return Err(format!(
                "`{name}` is set by the server for every response and cannot be set by middleware"
            ));
        }
        let values: Vec<String> = match value {
            Value::String(single) => vec![single.clone()],
            // Only `set-cookie` legitimately repeats; anything else as a list is
            // a mistake the author should hear about.
            Value::Array(items) if lower == "set-cookie" => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        format!("every `set-cookie` value must be a string, got {}", describe(item))
                    })
                })
                .collect::<Result<_, _>>()?,
            other => {
                return Err(format!(
                    "header `{name}` must be a string, got {}",
                    describe(other)
                ))
            }
        };
        for value in &values {
            if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
                return Err(format!(
                    "header `{name}` contains a line break, which would split the response"
                ));
            }
            if lower == "set-cookie" && value.trim_start().starts_with(FRAMEWORK_COOKIE_PREFIX) {
                return Err(format!(
                    "`set-cookie` names a `{FRAMEWORK_COOKIE_PREFIX}…` cookie, which belongs to the \
                     framework"
                ));
            }
        }
        headers.entry(lower).or_default().extend(values);
    }
    Ok(headers)
}

/// RFC 9110 `tchar`.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn validate_location(location: &str) -> Result<(), String> {
    if location.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err("redirect location contains a line break".to_string());
    }
    let lower = location.to_ascii_lowercase();
    let absolute = lower.starts_with("https://") || lower.starts_with("http://");
    // `//evil.example` is an absolute URL to a browser. Written as a path it is
    // almost always a bug (a doubled slash from string concatenation) that
    // becomes an open redirect; an author who means another host writes a scheme.
    let path = location.starts_with('/') && !location.starts_with("//") && !location.starts_with("/\\");
    if absolute || path {
        Ok(())
    } else {
        Err(format!(
            "redirect location \"{location}\" must be a path starting with `/` or an absolute \
             http(s) URL"
        ))
    }
}

/// A rewrite target is an in-app path: never another host, never a framework
/// lane, never a traversal.
fn validate_rewrite(target: &str) -> Result<(String, Option<String>), String> {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query.to_string())),
        None => (target, None),
    };
    if target.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0 || b == b'#') {
        return Err(format!(
            "rewrite target \"{target}\" contains a character a request path cannot"
        ));
    }
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(format!(
            "rewrite target \"{target}\" must be a path in this app starting with a single `/`; \
             to send the browser somewhere else, use redirect()"
        ));
    }
    let decoded = super::matcher::percent_decode_path(path);
    if decoded
        .split('/')
        .any(|segment| segment == "." || segment == ".." || segment.contains('\\'))
    {
        return Err(format!("rewrite target \"{target}\" contains a `.`, `..` or `\\` segment"));
    }
    // 🔒 The framework's own lanes — sign-in endpoints, actions, the live
    // streams — each run their own checks in an order that assumes they were
    // reached directly. A rewrite into one would enter it having skipped the
    // branch that set that order up.
    if decoded == "/_albedo" || decoded.starts_with("/_albedo/") {
        return Err(format!(
            "rewrite target \"{target}\" is under `/_albedo/`, which belongs to the framework"
        ));
    }
    Ok((path.to_string(), query))
}

fn string_field(
    object: &serde_json::Map<String, Value>,
    field: &str,
    call: &str,
) -> Result<String, String> {
    match object.get(field) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(other) => Err(format!("{call} needs a non-empty string, got {}", describe(other))),
        None => Err(format!("{call} needs a non-empty string")),
    }
}

fn status_of(value: &Value, what: &str) -> Result<u16, String> {
    value
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| format!("{what} must be an integer status code, got {}", describe(value)))
}

fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("the boolean {b}"),
        Value::Number(n) => format!("the number {n}"),
        Value::String(s) => format!("the string {s:?}"),
        Value::Array(_) => "an array".to_string(),
        Value::Object(_) => "an object".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn branded(kind: &str, rest: Value) -> Value {
        let mut object = rest.as_object().cloned().unwrap_or_default();
        object.insert(BRAND.to_string(), json!(kind));
        Value::Object(object)
    }

    #[test]
    fn returning_nothing_continues() {
        assert_eq!(
            decode(&Value::Null),
            Ok(Outcome::Next {
                headers: Headers::new()
            })
        );
    }

    #[test]
    fn a_stray_return_value_is_refused_rather_than_read_as_continue() {
        for value in [json!(true), json!("ok"), json!({ "user": 1 }), json!([1])] {
            let err = decode(&value).expect_err("refused");
            assert!(err.contains("next()"), "{err}");
        }
    }

    #[test]
    fn next_carries_lowercased_headers_and_repeated_cookies() {
        let outcome = decode(&branded(
            "next",
            json!({ "headers": { "X-Frame-Options": "DENY", "Set-Cookie": ["a=1", "b=2"] } }),
        ))
        .expect("valid");
        let headers = outcome.headers();
        assert_eq!(headers["x-frame-options"], vec!["DENY"]);
        assert_eq!(headers["set-cookie"], vec!["a=1", "b=2"]);
    }

    #[test]
    fn a_header_that_would_split_the_response_is_refused() {
        let err = decode(&branded("next", json!({ "headers": { "x-a": "1\r\nx-b: 2" } })))
            .expect_err("refused");
        assert!(err.contains("line break"), "{err}");
    }

    #[test]
    fn a_framework_cookie_cannot_be_set_by_middleware() {
        let err = decode(&branded(
            "next",
            json!({ "headers": { "set-cookie": ["ok=1", "__Host-albedo_session=x; Path=/"] } }),
        ))
        .expect_err("refused");
        assert!(err.contains("belongs to the framework"), "{err}");
    }

    #[test]
    fn transport_headers_are_refused_by_name() {
        let err = decode(&branded("next", json!({ "headers": { "Content-Length": "4" } })))
            .expect_err("refused");
        assert!(err.contains("Content-Length"), "{err}");
    }

    #[test]
    fn redirect_defaults_to_307_and_refuses_non_redirect_statuses() {
        let outcome = decode(&branded("redirect", json!({ "location": "/sign-in" }))).unwrap();
        assert!(matches!(outcome, Outcome::Redirect { status: 307, .. }));

        let err = decode(&branded("redirect", json!({ "location": "/x", "status": 200 })))
            .expect_err("refused");
        assert!(err.contains("not a redirect"), "{err}");
    }

    #[test]
    fn a_protocol_relative_redirect_is_refused() {
        for location in ["//evil.example", "/\\evil.example", "evil.example", "javascript:alert(1)"] {
            decode(&branded("redirect", json!({ "location": location })))
                .expect_err(location);
        }
        decode(&branded("redirect", json!({ "location": "https://example.com/x" })))
            .expect("an absolute URL with a scheme is the author's explicit choice");
    }

    #[test]
    fn rewrite_splits_the_query_and_stays_inside_the_app() {
        let outcome = decode(&branded("rewrite", json!({ "path": "/b?x=1" }))).unwrap();
        assert_eq!(
            outcome,
            Outcome::Rewrite {
                path: "/b".to_string(),
                query: Some("x=1".to_string()),
                headers: Headers::new()
            }
        );

        for target in [
            "/_albedo/auth/password/login",
            "/%5Falbedo/action",
            "/_albedo",
            "//other",
            "https://example.com/",
            "/a/../_albedo/x",
            "/a/%2e%2e/b",
            "relative",
        ] {
            decode(&branded("rewrite", json!({ "path": target }))).expect_err(target);
        }
    }

    #[test]
    fn respond_requires_a_final_status_and_a_string_body() {
        let outcome = decode(&branded("respond", json!({ "status": 403, "body": "no" }))).unwrap();
        assert!(matches!(outcome, Outcome::Respond { status: 403, .. }));

        decode(&branded("respond", json!({ "status": 101 }))).expect_err("1xx");
        decode(&branded("respond", json!({}))).expect_err("no status");
        let err = decode(&branded("respond", json!({ "status": 200, "body": { "a": 1 } })))
            .expect_err("object body");
        assert!(err.contains("JSON.stringify"), "{err}");
    }
}
