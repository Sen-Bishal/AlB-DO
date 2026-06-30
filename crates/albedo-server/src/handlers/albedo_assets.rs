//! Phase P · post-P wire-through — embedded bakabox client assets.
//!
//! `boot_production_server` used to mount `<dist>` as a `public_dir`
//! so the bakabox runtime files (written by the build step under
//! `<dist>/_albedo/`) resolved at `/_albedo/runtime.js`. The
//! side-effect was that `<dist>/index.html` (the static-deploy
//! fallback) shadowed `/`, so the manifest-streaming arm never ran
//! for the root route.
//!
//! This module replaces that mount with a focused dispatch arm: the
//! `include_str!`-baked client templates are served directly from
//! the binary, mirroring the dev path's `dev_static_asset` helper.
//! Production no longer depends on the dist mirror being present;
//! the bytes ride with the binary.

use axum::body::Body;
use axum::http::{header, HeaderValue, Response, StatusCode};

const RUNTIME_JS: &str = include_str!("../../../../assets/albedo-runtime.js");
const BINCODE_JS: &str = include_str!("../../../../assets/bincode.js");
const LINK_FORMS_JS: &str = include_str!("../../../../assets/albedo-link-forms.js");
const HYDRATION_JS: &str = include_str!("../../../../assets/albedo-hydration.js");
const WT_BOOTSTRAP_JS: &str = include_str!("../../../../assets/albedo-wt-bootstrap.js");
// A3 · the Tier-C client runtime (Preact-compatible VDOM + hooks). Installs the
// `h`/`useState`/… globals and `__ALBEDO_HYDRATE_ISLAND` the bootstrap calls.
const CLIENT_JS: &str = include_str!("../../../../assets/albedo-client.js");

/// Resolve `path` to one of the in-binary bakabox client assets.
/// Returns `Some(body)` for the known framework-reserved URLs;
/// `None` for everything else (caller falls through to the
/// regular dispatch).
fn resolve_albedo_asset(path: &str) -> Option<&'static str> {
    match path {
        "/_albedo/runtime.js" => Some(RUNTIME_JS),
        "/_albedo/bincode.js" => Some(BINCODE_JS),
        "/_albedo/link-forms.js" => Some(LINK_FORMS_JS),
        "/_albedo/hydration.js" => Some(HYDRATION_JS),
        "/_albedo/client.js" => Some(CLIENT_JS),
        "/_albedo/wt-bootstrap.js" => Some(WT_BOOTSTRAP_JS),
        _ => None,
    }
}

/// Build a 200 response carrying one of the embedded bakabox
/// assets, or `None` for unrecognised paths.
///
/// `cache-control` is `no-cache` (revalidate before reuse). These assets live
/// at FIXED, non-content-hashed URLs (`/_albedo/runtime.js`), so a binary rev
/// that changes the bytes keeps the same URL — a long `max-age` would leave
/// browsers running a stale client runtime after a deploy (drifting from the
/// server). Content-hashed chunks (`/_albedo/chunks/<name>.<hash>.js`) are the
/// ones safe to cache immutably; these are not.
pub fn dispatch_albedo_asset(path: &str) -> Option<Response<Body>> {
    let body = resolve_albedo_asset(path)?;
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/javascript; charset=utf-8"),
        )
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))
        .body(Body::from(body))
        .expect("static asset response builds");
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_assets_resolve_to_non_empty_bodies() {
        for path in [
            "/_albedo/runtime.js",
            "/_albedo/bincode.js",
            "/_albedo/link-forms.js",
            "/_albedo/hydration.js",
            "/_albedo/client.js",
            "/_albedo/wt-bootstrap.js",
        ] {
            let body = resolve_albedo_asset(path).unwrap_or_else(|| {
                panic!("expected asset to resolve: {path}")
            });
            assert!(
                !body.trim().is_empty(),
                "asset body must be non-empty: {path}"
            );
        }
    }

    #[test]
    fn unrelated_paths_return_none() {
        assert!(resolve_albedo_asset("/").is_none());
        assert!(resolve_albedo_asset("/_albedo/action").is_none());
        assert!(resolve_albedo_asset("/_albedo/runtime.js.map").is_none());
        assert!(resolve_albedo_asset("/runtime.js").is_none());
    }

    #[tokio::test]
    async fn dispatch_returns_javascript_content_type() {
        let response = dispatch_albedo_asset("/_albedo/runtime.js").unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok());
        assert_eq!(content_type, Some("text/javascript; charset=utf-8"));
    }

    /// Framework assets sit at fixed (non-hashed) URLs, so they must revalidate
    /// — a long max-age would strand browsers on a stale client runtime after a
    /// deploy. Regression guard for the cache-staleness bug.
    #[tokio::test]
    async fn dispatch_marks_framework_assets_no_cache() {
        let response = dispatch_albedo_asset("/_albedo/runtime.js").unwrap();
        let cache = response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok());
        assert_eq!(cache, Some("no-cache"));
    }

    /// The embedded runtime carries the Tier-B inject-queue drain (paired with
    /// the head bootstrap stub). Guards against shipping a runtime that can't
    /// replay calls buffered before it loads.
    #[test]
    fn embedded_runtime_drains_inject_queue() {
        assert!(RUNTIME_JS.contains("__ALBEDO_INJECT_QUEUE"));
    }
}
