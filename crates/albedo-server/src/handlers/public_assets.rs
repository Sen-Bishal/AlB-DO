//! Phase N · `public/` static asset serving.
//!
//! Files under the configured public directory are exposed verbatim at
//! the URL root (`public/logo.svg` → `GET /logo.svg`). Paths are
//! resolved through [`sanitize_public_path`] which rejects parent-dir
//! traversal, absolute paths, and Windows drive prefixes before
//! touching the filesystem.

use axum::body::Body;
use axum::http::{header, HeaderValue, Response, StatusCode};
use std::path::{Component, Path, PathBuf};

/// Set of mounted public-asset roots. Multiple roots are supported so
/// the dev CLI and a userland mount can coexist. Lookups try each root
/// in registration order and return the first hit.
#[derive(Debug, Clone)]
pub struct PublicAssets {
    roots: Vec<PathBuf>,
    cache_header: HeaderValue,
}

impl PublicAssets {
    /// Build with the given roots and a `Cache-Control` directive
    /// applied to every successful response.
    pub fn new(roots: Vec<PathBuf>, cache_control: &str) -> Self {
        Self {
            roots,
            cache_header: HeaderValue::from_str(cache_control)
                .unwrap_or_else(|_| HeaderValue::from_static("no-store")),
        }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Resolve `url_path` against every mounted root in turn and
    /// return the first existing file. The path is sanitised — any
    /// attempt to escape the root yields `None`.
    pub fn resolve(&self, url_path: &str) -> Option<PathBuf> {
        let rel = sanitize_public_path(url_path)?;
        for root in &self.roots {
            let candidate = root.join(&rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    /// Build a complete axum response for `path`. The body is the
    /// file contents; the content-type is inferred from the
    /// extension; cache-control is whatever the registry was built
    /// with.
    pub fn read_response(&self, path: &Path) -> Response<Body> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let mut response =
                    Response::builder().status(StatusCode::OK).body(Body::from(bytes));
                if let Ok(ref mut resp) = response {
                    resp.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static(content_type_for_path(path)),
                    );
                    resp.headers_mut()
                        .insert(header::CACHE_CONTROL, self.cache_header.clone());
                }
                response.unwrap_or_else(|_| Response::new(Body::empty()))
            }
            Err(_) => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap_or_else(|_| Response::new(Body::empty())),
        }
    }
}

/// Translate a request URL into a relative subpath, rejecting any
/// traversal attempt. Returns `None` for `/` and other shapes that
/// must not resolve through the public mount.
pub fn sanitize_public_path(url_path: &str) -> Option<PathBuf> {
    let trimmed = url_path.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('\0') {
        return None;
    }
    let candidate = Path::new(trimmed);
    let mut out = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(segment) => {
                let s = segment.to_string_lossy();
                if s.starts_with("..") || s.contains('\0') {
                    return None;
                }
                out.push(segment);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if out.as_os_str().is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Minimal extension → MIME type table. Covers the assets typical
/// `public/` directories hold; anything unknown falls back to the
/// IANA generic binary type.
pub fn content_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("txt") | Some("md") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn sanitize_rejects_traversal_and_absolute_paths() {
        assert!(sanitize_public_path("/").is_none());
        assert!(sanitize_public_path("/../etc/passwd").is_none());
        assert!(sanitize_public_path("/a/../b").is_none());
        assert!(sanitize_public_path("/a/\0b").is_none());
        assert_eq!(
            sanitize_public_path("/logo.svg"),
            Some(PathBuf::from("logo.svg"))
        );
        assert_eq!(
            sanitize_public_path("/images/cover.png"),
            Some(PathBuf::from("images/cover.png"))
        );
    }

    #[test]
    fn resolve_returns_first_matching_root() {
        let temp = tempdir().unwrap();
        let root_a = temp.path().join("a");
        let root_b = temp.path().join("b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        fs::write(root_a.join("only-a.txt"), "a").unwrap();
        fs::write(root_b.join("only-b.txt"), "b").unwrap();

        let assets = PublicAssets::new(vec![root_a.clone(), root_b.clone()], "no-store");
        assert_eq!(
            assets.resolve("/only-a.txt"),
            Some(root_a.join("only-a.txt"))
        );
        assert_eq!(
            assets.resolve("/only-b.txt"),
            Some(root_b.join("only-b.txt"))
        );
        assert_eq!(assets.resolve("/missing.txt"), None);
    }

    #[test]
    fn resolve_blocks_traversal_even_with_existing_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("pub");
        fs::create_dir_all(&root).unwrap();
        fs::write(temp.path().join("secret.txt"), "nope").unwrap();
        let assets = PublicAssets::new(vec![root], "no-store");
        assert!(assets.resolve("/../secret.txt").is_none());
    }

    #[test]
    fn content_type_table_covers_expected_extensions() {
        assert_eq!(
            content_type_for_path(Path::new("x.svg")),
            "image/svg+xml"
        );
        assert_eq!(
            content_type_for_path(Path::new("x.woff2")),
            "font/woff2"
        );
        assert_eq!(
            content_type_for_path(Path::new("x.unknown")),
            "application/octet-stream"
        );
    }

    #[test]
    fn read_response_returns_404_for_missing_file() {
        let assets = PublicAssets::new(vec![PathBuf::from("/nope/zero")], "no-store");
        let resp = assets.read_response(Path::new("/no/such/file"));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
