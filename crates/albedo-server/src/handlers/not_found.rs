//! The page a browser gets for a URL no route answers.
//!
//! 🔴 Until 2026-10-07 every unmatched URL answered `{"error":"route not found:
//! GET /nope"}` with `content-type: application/json` — to a person who had
//! followed a broken link, a raw JSON object in place of a page. An API client
//! asking the same question still gets exactly that: the JSON body is the
//! contract it was written against, and only a request that asked for HTML is
//! answered with HTML.
//!
//! An app owns its 404 page by declaring a `/404` route — `src/routes/404.tsx`
//! — which is rendered like any other page and served with status 404. Without
//! one, [`builtin_page`] is the answer.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode};

/// The route an app declares its own not-found page at.
pub const NOT_FOUND_ROUTE: &str = "/404";

/// Did this request ask for a page? A browser navigating says so in `Accept`;
/// `fetch()` without one sends `*/*`, and an API client sends JSON, and both
/// keep the JSON body they were written against.
#[must_use]
pub fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|accept| accept.to_ascii_lowercase().contains("text/html"))
}

/// Whether a header may be copied onto the internal request for the app's
/// `/404` page. A validator would let that page answer `304 Not Modified` to a
/// URL that does not exist, and a length or encoding describes a body this
/// request does not have.
#[must_use]
pub fn carries_over(name: &HeaderName) -> bool {
    !matches!(
        name.as_str(),
        "if-none-match"
            | "if-modified-since"
            | "if-match"
            | "if-unmodified-since"
            | "if-range"
            | "range"
            | "content-length"
            | "content-type"
            | "transfer-encoding"
    )
}

/// A minimal, self-contained 404 page. The path is echoed escaped — it is the
/// one thing a person who mistyped a URL needs to see — and nothing else about
/// the server is.
#[must_use]
pub fn builtin_page(path: &str) -> Response<Body> {
    let body = format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Not found</title><style>\
         :root{{color-scheme:light dark;font-family:system-ui,sans-serif}}\
         body{{margin:0;min-height:100vh;display:grid;place-items:center}}\
         main{{padding:24px;max-width:36rem}}h1{{font-size:3rem;margin:0 0 .5rem}}\
         code{{word-break:break-all}}</style></head><body><main>\
         <h1>404</h1><p>There is no page at <code>{}</code>.</p>\
         <p><a href=\"/\">Go to the home page</a></p></main></body></html>",
        escape(path)
    );
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::NOT_FOUND;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
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

    #[test]
    fn a_browser_wants_html_and_fetch_and_api_clients_do_not() {
        let mut browser = HeaderMap::new();
        browser.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,*/*;q=0.8"),
        );
        assert!(wants_html(&browser));

        let mut fetch = HeaderMap::new();
        fetch.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        assert!(!wants_html(&fetch));
        assert!(!wants_html(&HeaderMap::new()));

        let mut api = HeaderMap::new();
        api.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert!(!wants_html(&api));
    }

    #[test]
    fn the_path_is_echoed_escaped() {
        let response = builtin_page("/<script>alert('x')</script>");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = futures_body(response);
        assert!(body.contains("/&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"), "{body}");
        assert!(!body.contains("<script>"), "{body}");
    }

    fn futures_body(response: Response<Body>) -> String {
        let bytes = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }
}
