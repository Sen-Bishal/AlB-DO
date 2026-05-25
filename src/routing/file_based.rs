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
const INDEX_FILE_STEM: &str = "index";

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

/// Aggregate output of [`discover_routes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDiscovery {
    pub routes: Vec<DiscoveredRoute>,
    pub layouts: Vec<DiscoveredLayout>,
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
        });
    }

    // Stable, dedup, deterministic order: shortest prefix first.
    layouts.sort_by(|a, b| {
        a.url_prefix
            .len()
            .cmp(&b.url_prefix.len())
            .then_with(|| a.url_prefix.cmp(&b.url_prefix))
    });
    raw_routes.sort_by(|a, b| a.url_path.cmp(&b.url_path));

    for route in &mut raw_routes {
        route.layout_chain = layouts_for_route(&route.url_path, &layouts);
    }

    Ok(RouteDiscovery {
        routes: raw_routes,
        layouts,
    })
}

/// Translate a path relative to the routes root into a URL pattern.
///
/// Returns `None` for paths that should not become routes (layouts,
/// underscore-prefixed files, unsupported extensions).
pub fn file_path_to_url(rel_path: &Path) -> Option<String> {
    let stem = file_stem(rel_path)?;
    if stem.starts_with('_') || stem.eq_ignore_ascii_case(LAYOUT_FILE_STEM) {
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
}
