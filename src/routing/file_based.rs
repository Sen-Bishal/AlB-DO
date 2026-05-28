//! File-based route discovery — walks a `routes/` tree and emits one
//! [`DiscoveredRoute`] per entry file plus one [`DiscoveredLayout`] per
//! `layout.{tsx,jsx,ts,js}`.
//!
//! ## Path conventions
//!
//! | File                           | URL path             |
//! |--------------------------------|----------------------|
//! | `index.tsx`                    | `/`                  |
//! | `about.tsx`                    | `/about`             |
//! | `blog/index.tsx`               | `/blog`              |
//! | `blog/[slug].tsx`              | `/blog/[slug]`       |
//! | `blog/[...rest].tsx`           | `/blog/[...rest]`    |
//! | `users/[[...slug]].tsx`        | `/users/[[...slug]]` |
//!
//! ## Layout composition
//!
//! `layout.tsx` in a directory wraps every route at or below that
//! directory. Layouts are ordered root-down so the outermost layout
//! sits first in [`DiscoveredRoute::layout_chain`].
//!
//! ## Error + loading boundaries (Phase P · Stream E.2)
//!
//! `error.tsx` and `loading.tsx` are convention files alongside
//! `layout.tsx`. Each directory's `error.tsx` / `loading.tsx` covers
//! every route at or below that directory; the NEAREST boundary
//! (longest matching URL prefix) wins, mirroring Next.js's
//! closer-to-leaf semantics. Streaming handlers serve the matching
//! component's pre-rendered HTML when a Tier-C node fails (error) or
//! is still resolving (loading). Stream B's manifest schema already
//! carries `RouteManifest.error_component` / `loading_component` —
//! E.2 fills those fields.
//!
//! ## Skipped files
//!
//! - Names starting with `_` (private modules; not turned into routes).
//! - Non `{tsx,jsx,ts,js}` extensions.
//! - Hidden files / directories (any segment starting with `.`).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

/// Conventional sub-directory name. The dev contract surfaces this so
/// the CLI and userland code agree on where routes live.
pub const ROUTES_DIRNAME: &str = "routes";

const ROUTE_EXTENSIONS: &[&str] = &["tsx", "jsx", "ts", "js"];
const LAYOUT_FILE_STEM: &str = "layout";
const ERROR_FILE_STEM: &str = "error";
const LOADING_FILE_STEM: &str = "loading";
const INDEX_FILE_STEM: &str = "index";

/// Convention-file stems that look like routes but are NOT routes.
/// Used by both the discovery walker (skip in `file_path_to_url`)
/// and the manifest builder's `route_path_from_component` (skip
/// when enumerating route entries).
const CONVENTION_STEMS: &[&str] = &[LAYOUT_FILE_STEM, ERROR_FILE_STEM, LOADING_FILE_STEM];

/// One file discovered under the routes root that should become a
/// servable route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRoute {
    /// Translated URL pattern (with `[slug]` / `[...rest]` preserved).
    pub url_path: String,
    /// Path relative to the routes root, e.g. `blog/[slug].tsx`.
    pub source_rel_path: PathBuf,
    /// Layout files that wrap this route, ordered outer → inner.
    pub layout_chain: Vec<PathBuf>,
    /// Phase P · Stream E.2 — `error.tsx` covering this route, if
    /// any. Closest-to-leaf (longest matching URL prefix) wins. The
    /// streaming handler renders this component's HTML when a
    /// Tier-C node on this route fails.
    pub error_boundary: Option<PathBuf>,
    /// Phase P · Stream E.2 — `loading.tsx` covering this route, if
    /// any. Same closest-to-leaf rule as `error_boundary`. Streaming
    /// handler renders this component while a Tier-C node is still
    /// resolving (or as the suspense fallback before any island
    /// resolves).
    pub loading: Option<PathBuf>,
}

/// One `layout.tsx` discovered under the routes root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredLayout {
    /// URL prefix the layout applies to (`/` for the root layout,
    /// `/dashboard` for `dashboard/layout.tsx`, etc.).
    pub url_prefix: String,
    /// Path relative to the routes root.
    pub source_rel_path: PathBuf,
}

/// Phase P · Stream E.2 — one `error.tsx` discovered under the
/// routes root. Same shape as [`DiscoveredLayout`] — a URL prefix +
/// the source file relative to the routes root. Picked per-route by
/// longest-matching-prefix to honour the closer-to-leaf convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredErrorBoundary {
    pub url_prefix: String,
    pub source_rel_path: PathBuf,
}

/// Phase P · Stream E.2 — one `loading.tsx` discovered under the
/// routes root. Same shape as [`DiscoveredErrorBoundary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredLoadingFallback {
    pub url_prefix: String,
    pub source_rel_path: PathBuf,
}

/// Aggregate output of [`discover_routes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDiscovery {
    pub routes: Vec<DiscoveredRoute>,
    pub layouts: Vec<DiscoveredLayout>,
    /// Phase P · Stream E.2 — all `error.tsx` files discovered.
    /// Sorted shortest-prefix-first. The picker walks this in
    /// reverse to apply closer-to-leaf semantics per route.
    pub error_boundaries: Vec<DiscoveredErrorBoundary>,
    /// Phase P · Stream E.2 — same shape as `error_boundaries` for
    /// `loading.tsx`.
    pub loading_fallbacks: Vec<DiscoveredLoadingFallback>,
}

/// Discovery failures surface to the caller; the dev CLI prints them
/// and refuses to continue rather than silently swallowing a typo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDiscoveryError {
    DuplicateRoute {
        url_path: String,
        first: PathBuf,
        second: PathBuf,
    },
    InvalidSegment {
        source: PathBuf,
        segment: String,
        reason: String,
    },
    RoutesDirMissing {
        path: PathBuf,
    },
}

impl std::fmt::Display for RouteDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateRoute {
                url_path,
                first,
                second,
            } => write!(
                f,
                "duplicate route '{}' produced by '{}' and '{}'",
                url_path,
                first.display(),
                second.display()
            ),
            Self::InvalidSegment {
                source,
                segment,
                reason,
            } => write!(
                f,
                "route file '{}' has invalid segment '{}': {}",
                source.display(),
                segment,
                reason
            ),
            Self::RoutesDirMissing { path } => {
                write!(f, "routes directory '{}' does not exist", path.display())
            }
        }
    }
}

impl std::error::Error for RouteDiscoveryError {}

/// Walks `routes_dir` and returns the discovered routes + layouts.
///
/// `routes_dir` is the absolute or relative path of the conventional
/// routes root (typically `<project>/src/routes/`).
pub fn discover_routes(routes_dir: &Path) -> Result<RouteDiscovery, RouteDiscoveryError> {
    if !routes_dir.is_dir() {
        return Err(RouteDiscoveryError::RoutesDirMissing {
            path: routes_dir.to_path_buf(),
        });
    }

    let mut raw_routes: Vec<DiscoveredRoute> = Vec::new();
    let mut layouts: Vec<DiscoveredLayout> = Vec::new();
    let mut error_boundaries: Vec<DiscoveredErrorBoundary> = Vec::new();
    let mut loading_fallbacks: Vec<DiscoveredLoadingFallback> = Vec::new();
    let mut seen_routes: BTreeMap<String, PathBuf> = BTreeMap::new();

    for entry in WalkDir::new(routes_dir)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel_path = match path.strip_prefix(routes_dir) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => continue,
        };
        if !is_visible_path(&rel_path) {
            continue;
        }
        if !has_route_extension(&rel_path) {
            continue;
        }

        let stem = file_stem(&rel_path).unwrap_or_default();
        if stem.starts_with('_') {
            continue;
        }

        if stem.eq_ignore_ascii_case(LAYOUT_FILE_STEM) {
            let prefix = directory_url_prefix(&rel_path);
            layouts.push(DiscoveredLayout {
                url_prefix: prefix,
                source_rel_path: rel_path,
            });
            continue;
        }

        // Phase P · Stream E.2 — `error.tsx` and `loading.tsx` follow
        // the same closure rule layouts use: a file in directory
        // `dashboard/` covers every route at or below `/dashboard`.
        // Same skip-as-route behaviour — they don't appear in
        // `raw_routes`.
        if stem.eq_ignore_ascii_case(ERROR_FILE_STEM) {
            let prefix = directory_url_prefix(&rel_path);
            error_boundaries.push(DiscoveredErrorBoundary {
                url_prefix: prefix,
                source_rel_path: rel_path,
            });
            continue;
        }
        if stem.eq_ignore_ascii_case(LOADING_FILE_STEM) {
            let prefix = directory_url_prefix(&rel_path);
            loading_fallbacks.push(DiscoveredLoadingFallback {
                url_prefix: prefix,
                source_rel_path: rel_path,
            });
            continue;
        }

        let url_path = match file_path_to_url(&rel_path) {
            Some(p) => p,
            None => continue,
        };
        validate_url_segments(&rel_path, &url_path)?;

        if let Some(prior) = seen_routes.get(&url_path) {
            return Err(RouteDiscoveryError::DuplicateRoute {
                url_path,
                first: prior.clone(),
                second: rel_path,
            });
        }
        seen_routes.insert(url_path.clone(), rel_path.clone());
        raw_routes.push(DiscoveredRoute {
            url_path,
            source_rel_path: rel_path,
            layout_chain: Vec::new(),
            error_boundary: None,
            loading: None,
        });
    }

    // Stable, dedup, deterministic order: shortest prefix first.
    let prefix_sort = |a_len: usize, a: &str, b_len: usize, b: &str| {
        a_len.cmp(&b_len).then_with(|| a.cmp(b))
    };
    layouts.sort_by(|a, b| prefix_sort(a.url_prefix.len(), &a.url_prefix, b.url_prefix.len(), &b.url_prefix));
    error_boundaries.sort_by(|a, b| {
        prefix_sort(a.url_prefix.len(), &a.url_prefix, b.url_prefix.len(), &b.url_prefix)
    });
    loading_fallbacks.sort_by(|a, b| {
        prefix_sort(a.url_prefix.len(), &a.url_prefix, b.url_prefix.len(), &b.url_prefix)
    });
    raw_routes.sort_by(|a, b| a.url_path.cmp(&b.url_path));

    for route in &mut raw_routes {
        route.layout_chain = layouts_for_route(&route.url_path, &layouts);
        route.error_boundary =
            nearest_boundary_for_route(&route.url_path, &error_boundaries);
        route.loading = nearest_loading_for_route(&route.url_path, &loading_fallbacks);
    }

    Ok(RouteDiscovery {
        routes: raw_routes,
        layouts,
        error_boundaries,
        loading_fallbacks,
    })
}

/// Translate a path relative to the routes root into a URL pattern.
///
/// Returns `None` for paths that should not become routes (layouts,
/// `error.tsx` / `loading.tsx` boundaries, underscore-prefixed files,
/// unsupported extensions).
pub fn file_path_to_url(rel_path: &Path) -> Option<String> {
    let stem = file_stem(rel_path)?;
    if stem.starts_with('_') || is_convention_stem(stem) {
        return None;
    }
    if !has_route_extension(rel_path) {
        return None;
    }

    let parent = rel_path.parent().unwrap_or_else(|| Path::new(""));
    let mut segments: Vec<String> = Vec::new();
    for component in parent.components() {
        if let Component::Normal(name) = component {
            let s = name.to_string_lossy().to_string();
            if s.is_empty() || s.starts_with('.') {
                continue;
            }
            segments.push(s);
        }
    }

    // `index.tsx` in any directory maps to the parent dir's path; other
    // names contribute their stem as the final segment.
    if !stem.eq_ignore_ascii_case(INDEX_FILE_STEM) {
        segments.push(stem.to_string());
    }

    if segments.is_empty() {
        Some("/".to_string())
    } else {
        Some(format!("/{}", segments.join("/")))
    }
}

fn validate_url_segments(source: &Path, url: &str) -> Result<(), RouteDiscoveryError> {
    if url == "/" {
        return Ok(());
    }
    for segment in url.trim_start_matches('/').split('/') {
        if segment.is_empty() {
            return Err(RouteDiscoveryError::InvalidSegment {
                source: source.to_path_buf(),
                segment: segment.to_string(),
                reason: "empty segment".to_string(),
            });
        }
        // Reject bare `[` / `]` mismatch — a misnamed `[slug.tsx` would
        // otherwise sneak through as a literal segment and surprise
        // the user with `/[slug` showing up in the route table.
        let open = segment.matches('[').count();
        let close = segment.matches(']').count();
        if open != close {
            return Err(RouteDiscoveryError::InvalidSegment {
                source: source.to_path_buf(),
                segment: segment.to_string(),
                reason: "unbalanced '[' / ']'".to_string(),
            });
        }
    }
    Ok(())
}

fn layouts_for_route(url_path: &str, layouts: &[DiscoveredLayout]) -> Vec<PathBuf> {
    let mut matched: Vec<&DiscoveredLayout> = layouts
        .iter()
        .filter(|layout| url_prefix_covers(layout.url_prefix.as_str(), url_path))
        .collect();
    matched.sort_by(|a, b| {
        a.url_prefix
            .len()
            .cmp(&b.url_prefix.len())
            .then_with(|| a.url_prefix.cmp(&b.url_prefix))
    });
    matched
        .into_iter()
        .map(|layout| layout.source_rel_path.clone())
        .collect()
}

/// Phase P · Stream E.2 — pick the nearest (longest URL prefix that
/// covers `url_path`) `error.tsx` for a route. Closer-to-leaf
/// boundaries win, matching the Next.js convention.
fn nearest_boundary_for_route(
    url_path: &str,
    boundaries: &[DiscoveredErrorBoundary],
) -> Option<PathBuf> {
    boundaries
        .iter()
        .filter(|b| url_prefix_covers(b.url_prefix.as_str(), url_path))
        .max_by_key(|b| b.url_prefix.len())
        .map(|b| b.source_rel_path.clone())
}

/// Phase P · Stream E.2 — same pick rule as `nearest_boundary_for_route`
/// but for `loading.tsx`. Kept as a sibling fn rather than a generic
/// so the call sites in `discover_routes` stay literal — the two
/// shapes differ only by the wrapper struct.
fn nearest_loading_for_route(
    url_path: &str,
    fallbacks: &[DiscoveredLoadingFallback],
) -> Option<PathBuf> {
    fallbacks
        .iter()
        .filter(|f| url_prefix_covers(f.url_prefix.as_str(), url_path))
        .max_by_key(|f| f.url_prefix.len())
        .map(|f| f.source_rel_path.clone())
}

/// Phase P · Stream E.2 — true when `stem` names a convention file
/// (`layout` / `error` / `loading`) — these all sit in `routes/` but
/// MUST NOT become route entries.
fn is_convention_stem(stem: &str) -> bool {
    CONVENTION_STEMS
        .iter()
        .any(|conv| stem.eq_ignore_ascii_case(conv))
}

fn url_prefix_covers(prefix: &str, url: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    if url == prefix {
        return true;
    }
    let with_slash = format!("{prefix}/");
    url.starts_with(with_slash.as_str())
}

fn directory_url_prefix(rel_path: &Path) -> String {
    let mut segments = Vec::new();
    if let Some(parent) = rel_path.parent() {
        for component in parent.components() {
            if let Component::Normal(name) = component {
                let s = name.to_string_lossy().to_string();
                if s.is_empty() || s.starts_with('.') {
                    continue;
                }
                segments.push(s);
            }
        }
    }
    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

fn file_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|s| s.to_str())
}

fn has_route_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ROUTE_EXTENSIONS.iter().any(|allowed| allowed == &ext))
        .unwrap_or(false)
}

fn is_visible_path(rel_path: &Path) -> bool {
    rel_path.components().all(|component| {
        if let Component::Normal(name) = component {
            !name.to_string_lossy().starts_with('.')
        } else {
            true
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn index_tsx_maps_to_root() {
        assert_eq!(
            file_path_to_url(Path::new("index.tsx")).as_deref(),
            Some("/")
        );
    }

    #[test]
    fn flat_file_uses_stem_as_segment() {
        assert_eq!(
            file_path_to_url(Path::new("about.tsx")).as_deref(),
            Some("/about")
        );
    }

    #[test]
    fn nested_index_uses_parent_dir() {
        assert_eq!(
            file_path_to_url(Path::new("blog/index.tsx")).as_deref(),
            Some("/blog")
        );
    }

    #[test]
    fn dynamic_segment_is_preserved() {
        assert_eq!(
            file_path_to_url(Path::new("blog/[slug].tsx")).as_deref(),
            Some("/blog/[slug]")
        );
    }

    #[test]
    fn catch_all_segment_is_preserved() {
        assert_eq!(
            file_path_to_url(Path::new("docs/[...rest].tsx")).as_deref(),
            Some("/docs/[...rest]")
        );
    }

    #[test]
    fn optional_catch_all_segment_is_preserved() {
        assert_eq!(
            file_path_to_url(Path::new("catalog/[[...slug]].tsx")).as_deref(),
            Some("/catalog/[[...slug]]")
        );
    }

    #[test]
    fn layout_files_yield_no_url() {
        assert_eq!(file_path_to_url(Path::new("layout.tsx")), None);
        assert_eq!(file_path_to_url(Path::new("dashboard/layout.tsx")), None);
    }

    #[test]
    fn underscore_files_yield_no_url() {
        assert_eq!(file_path_to_url(Path::new("_helpers.tsx")), None);
    }

    #[test]
    fn discover_routes_walks_tree_and_pairs_layouts() {
        let temp = tempdir().unwrap();
        let routes = temp.path().join("routes");
        fs::create_dir_all(&routes).unwrap();
        write(&routes, "index.tsx", "export default function Home() {}");
        write(&routes, "about.tsx", "export default function About() {}");
        write(&routes, "layout.tsx", "export default function Root() {}");
        write(
            &routes,
            "blog/[slug].tsx",
            "export default function Post() {}",
        );
        write(
            &routes,
            "blog/layout.tsx",
            "export default function BlogLayout() {}",
        );

        let discovery = discover_routes(&routes).unwrap();
        let urls: Vec<&str> = discovery
            .routes
            .iter()
            .map(|r| r.url_path.as_str())
            .collect();
        assert_eq!(urls, vec!["/", "/about", "/blog/[slug]"]);

        let root_route = &discovery.routes[0];
        assert_eq!(root_route.layout_chain, vec![PathBuf::from("layout.tsx")]);

        let blog_route = discovery
            .routes
            .iter()
            .find(|r| r.url_path == "/blog/[slug]")
            .unwrap();
        assert_eq!(
            blog_route.layout_chain,
            vec![
                PathBuf::from("layout.tsx"),
                PathBuf::from("blog/layout.tsx"),
            ]
        );
    }

    #[test]
    fn discover_routes_rejects_duplicates() {
        let temp = tempdir().unwrap();
        let routes = temp.path().join("routes");
        fs::create_dir_all(&routes).unwrap();
        write(&routes, "about.tsx", "");
        write(&routes, "about.jsx", "");
        let err = discover_routes(&routes).unwrap_err();
        assert!(matches!(err, RouteDiscoveryError::DuplicateRoute { .. }));
    }

    #[test]
    fn discover_routes_skips_underscore_and_hidden_files() {
        let temp = tempdir().unwrap();
        let routes = temp.path().join("routes");
        fs::create_dir_all(&routes).unwrap();
        write(&routes, "_helpers.tsx", "");
        write(&routes, "index.tsx", "");
        write(&routes, ".cache/leftover.tsx", "");

        let discovery = discover_routes(&routes).unwrap();
        let urls: Vec<&str> = discovery
            .routes
            .iter()
            .map(|r| r.url_path.as_str())
            .collect();
        assert_eq!(urls, vec!["/"]);
    }

    #[test]
    fn discover_routes_errors_when_dir_missing() {
        let temp = tempdir().unwrap();
        let err = discover_routes(&temp.path().join("nope")).unwrap_err();
        assert!(matches!(err, RouteDiscoveryError::RoutesDirMissing { .. }));
    }

    // ── Phase P · Stream E.2 tests ──────────────────────────────────

    #[test]
    fn error_and_loading_files_are_discovered_and_attached_to_root_route() {
        let temp = tempdir().unwrap();
        let routes = temp.path().join("routes");
        fs::create_dir_all(&routes).unwrap();
        write(&routes, "index.tsx", "export default function Home() {}");
        write(
            &routes,
            "error.tsx",
            "export default function RootError() {}",
        );
        write(
            &routes,
            "loading.tsx",
            "export default function RootLoading() {}",
        );

        let discovery = discover_routes(&routes).unwrap();
        assert_eq!(
            discovery.routes.len(),
            1,
            "error.tsx and loading.tsx must NOT become their own routes"
        );
        let root = &discovery.routes[0];
        assert_eq!(root.url_path, "/");
        assert_eq!(root.error_boundary, Some(PathBuf::from("error.tsx")));
        assert_eq!(root.loading, Some(PathBuf::from("loading.tsx")));

        assert_eq!(discovery.error_boundaries.len(), 1);
        assert_eq!(discovery.loading_fallbacks.len(), 1);
    }

    #[test]
    fn nested_error_boundary_wins_over_root_boundary_closer_to_leaf() {
        let temp = tempdir().unwrap();
        let routes = temp.path().join("routes");
        fs::create_dir_all(&routes).unwrap();
        // Two error.tsx files: one at root, one at /dashboard.
        // /dashboard/settings should pick the dashboard error
        // (longer matching prefix), while /about picks the root one.
        write(&routes, "index.tsx", "");
        write(&routes, "about.tsx", "");
        write(&routes, "dashboard/index.tsx", "");
        write(&routes, "dashboard/settings.tsx", "");
        write(&routes, "error.tsx", "export default function RootError() {}");
        write(
            &routes,
            "dashboard/error.tsx",
            "export default function DashError() {}",
        );

        let discovery = discover_routes(&routes).unwrap();

        let about = discovery
            .routes
            .iter()
            .find(|r| r.url_path == "/about")
            .expect("/about route");
        assert_eq!(
            about.error_boundary,
            Some(PathBuf::from("error.tsx")),
            "/about must inherit the root error boundary"
        );

        let settings = discovery
            .routes
            .iter()
            .find(|r| r.url_path == "/dashboard/settings")
            .expect("/dashboard/settings route");
        assert_eq!(
            settings.error_boundary,
            Some(PathBuf::from("dashboard/error.tsx")),
            "/dashboard/settings must pick the closer-to-leaf boundary"
        );

        // Index of the same subtree picks the dashboard boundary too.
        let dash_root = discovery
            .routes
            .iter()
            .find(|r| r.url_path == "/dashboard")
            .expect("/dashboard route");
        assert_eq!(
            dash_root.error_boundary,
            Some(PathBuf::from("dashboard/error.tsx"))
        );
    }

    #[test]
    fn error_loading_files_yield_no_url() {
        assert_eq!(file_path_to_url(Path::new("error.tsx")), None);
        assert_eq!(file_path_to_url(Path::new("loading.tsx")), None);
        assert_eq!(file_path_to_url(Path::new("blog/error.tsx")), None);
        assert_eq!(file_path_to_url(Path::new("blog/loading.tsx")), None);
    }
}
