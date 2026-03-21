use crate::config::{LayoutSpec, RouteSpec};
use crate::error::RuntimeError;
use http::Method;
use matchit::Router;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl HttpMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

impl TryFrom<&Method> for HttpMethod {
    type Error = RuntimeError;

    fn try_from(value: &Method) -> Result<Self, Self::Error> {
        match *value {
            Method::GET => Ok(Self::Get),
            Method::POST => Ok(Self::Post),
            Method::PUT => Ok(Self::Put),
            Method::PATCH => Ok(Self::Patch),
            Method::DELETE => Ok(Self::Delete),
            Method::HEAD => Ok(Self::Head),
            Method::OPTIONS => Ok(Self::Options),
            _ => Err(RuntimeError::InvalidConfig(format!(
                "unsupported HTTP method '{}'",
                value
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPolicy {
    Optional,
    Required,
    Role(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    pub route_name: String,
    pub handler_id: String,
    #[serde(default)]
    pub entry_module: Option<String>,
    #[serde(default)]
    pub props_loader: Option<String>,
    #[serde(default)]
    pub layout_handlers: Vec<String>,
    #[serde(default)]
    pub middleware: Vec<String>,
    #[serde(default)]
    pub auth: Option<AuthPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRoute {
    pub target: RouteTarget,
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteMatch {
    Matched(MatchedRoute),
    MethodNotAllowed { allowed: Vec<HttpMethod> },
    NotFound,
}

#[derive(Debug, Clone)]
struct PathRouteEntry {
    methods: BTreeMap<HttpMethod, RouteTarget>,
}

#[derive(Debug, Clone)]
pub struct CompiledRouter {
    path_router: Router<usize>,
    entries: Vec<PathRouteEntry>,
}

impl CompiledRouter {
    pub fn from_route_specs(route_specs: &[RouteSpec]) -> Result<Self, RuntimeError> {
        Self::from_route_and_layout_specs(route_specs, &[])
    }

    pub fn from_route_and_layout_specs(
        route_specs: &[RouteSpec],
        layout_specs: &[LayoutSpec],
    ) -> Result<Self, RuntimeError> {
        let mut grouped: BTreeMap<String, BTreeMap<HttpMethod, RouteTarget>> = BTreeMap::new();
        let compiled_layouts = compile_layout_targets(layout_specs)?;

        let mut sorted_specs = route_specs.to_vec();
        sorted_specs.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.method.cmp(&right.method))
                .then_with(|| left.name.cmp(&right.name))
        });

        for route in sorted_specs {
            let normalized_paths = normalize_route_paths(&route.path)?;
            let target = RouteTarget {
                route_name: route.name.clone(),
                handler_id: route.handler.clone(),
                entry_module: route.entry_module.clone(),
                props_loader: route.props_loader.clone(),
                layout_handlers: Vec::new(),
                middleware: route.middleware.clone(),
                auth: route.auth.clone(),
            };

            for normalized_path in normalized_paths {
                let per_path = grouped.entry(normalized_path.clone()).or_default();
                if per_path.contains_key(&route.method) {
                    return Err(RuntimeError::RouteConflict {
                        method: route.method.as_str().to_string(),
                        path: normalized_path,
                        message: format!(
                            "duplicate method/path pair from route path '{}'",
                            route.path
                        ),
                    });
                }

                let mut route_target = target.clone();
                route_target.layout_handlers =
                    resolve_layout_handlers_for_route(&normalized_path, &compiled_layouts);
                per_path.insert(route.method, route_target);
            }
        }

        let mut path_router = Router::new();
        let mut entries = Vec::new();

        for (path, methods) in grouped {
            let idx = entries.len();
            path_router.insert(path.as_str(), idx).map_err(|err| {
                RuntimeError::InvalidRoutePath {
                    path: path.clone(),
                    message: err.to_string(),
                }
            })?;
            entries.push(PathRouteEntry { methods });
        }

        Ok(Self {
            path_router,
            entries,
        })
    }

    pub fn match_route(&self, method: HttpMethod, path: &str) -> RouteMatch {
        let matched = match self.path_router.at(path) {
            Ok(matched) => matched,
            Err(_) => return RouteMatch::NotFound,
        };

        let Some(entry) = self.entries.get(*matched.value) else {
            return RouteMatch::NotFound;
        };

        if let Some(target) = entry.methods.get(&method) {
            let mut params = BTreeMap::new();
            for (key, value) in matched.params.iter() {
                params.insert(key.to_string(), value.to_string());
            }
            return RouteMatch::Matched(MatchedRoute {
                target: target.clone(),
                params,
            });
        }

        let allowed = entry.methods.keys().copied().collect::<Vec<_>>();
        RouteMatch::MethodNotAllowed { allowed }
    }
}

pub fn parse_query_string(query: Option<&str>) -> BTreeMap<String, Vec<String>> {
    let mut parsed = BTreeMap::new();
    let Some(query) = query else {
        return parsed;
    };

    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        parsed
            .entry(key.to_string())
            .or_insert_with(Vec::new)
            .push(value.to_string());
    }

    parsed
}

fn validate_route_path(path: &str) -> Result<(), RuntimeError> {
    if !path.starts_with('/') {
        return Err(RuntimeError::InvalidRoutePath {
            path: path.to_string(),
            message: "path must start with '/'".to_string(),
        });
    }
    if path.contains("//") {
        return Err(RuntimeError::InvalidRoutePath {
            path: path.to_string(),
            message: "path must not contain consecutive '/' segments".to_string(),
        });
    }
    if path.len() > 1 && path.ends_with('/') {
        return Err(RuntimeError::InvalidRoutePath {
            path: path.to_string(),
            message: "path must not end with '/' unless it is root '/'".to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct CompiledLayoutTarget {
    normalized_path: String,
    handler_id: String,
    segment_count: usize,
    static_segment_count: usize,
}

fn normalize_route_paths(path: &str) -> Result<Vec<String>, RuntimeError> {
    validate_route_path(path)?;

    if path == "/" {
        return Ok(vec!["/".to_string()]);
    }

    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut normalized_segments = Vec::with_capacity(segments.len());
    let mut optional_catch_all: Option<String> = None;

    for (idx, segment) in segments.iter().enumerate() {
        let is_last = idx + 1 == segments.len();

        if segment.starts_with("[[...") && segment.ends_with("]]") {
            let name = &segment[5..(segment.len() - 2)];
            validate_param_name(path, name, "optional catch-all")?;
            if !is_last {
                return Err(RuntimeError::InvalidRoutePath {
                    path: path.to_string(),
                    message:
                        "optional catch-all segment must be the final segment in the route path"
                            .to_string(),
                });
            }
            optional_catch_all = Some(name.to_string());
            continue;
        }

        if segment.starts_with("[...") && segment.ends_with(']') {
            let name = &segment[4..(segment.len() - 1)];
            validate_param_name(path, name, "catch-all")?;
            if !is_last {
                return Err(RuntimeError::InvalidRoutePath {
                    path: path.to_string(),
                    message: "catch-all segment must be the final segment in the route path"
                        .to_string(),
                });
            }
            normalized_segments.push(format!("{{*{name}}}"));
            continue;
        }

        if segment.starts_with('[') && segment.ends_with(']') {
            let name = &segment[1..(segment.len() - 1)];
            validate_param_name(path, name, "dynamic")?;
            if name.starts_with("...") {
                return Err(RuntimeError::InvalidRoutePath {
                    path: path.to_string(),
                    message: "invalid dynamic segment syntax; use '[...name]' for catch-all"
                        .to_string(),
                });
            }
            normalized_segments.push(format!("{{{name}}}"));
            continue;
        }

        if segment.starts_with('{') && segment.ends_with('}') {
            let inner = &segment[1..(segment.len() - 1)];
            if let Some(name) = inner.strip_prefix('*') {
                validate_param_name(path, name, "legacy catch-all")?;
                if !is_last {
                    return Err(RuntimeError::InvalidRoutePath {
                        path: path.to_string(),
                        message:
                            "legacy catch-all segment must be the final segment in the route path"
                                .to_string(),
                    });
                }
                normalized_segments.push(format!("{{*{name}}}"));
            } else {
                validate_param_name(path, inner, "legacy dynamic")?;
                normalized_segments.push(format!("{{{inner}}}"));
            }
            continue;
        }

        if segment.contains('[')
            || segment.contains(']')
            || segment.contains('{')
            || segment.contains('}')
        {
            return Err(RuntimeError::InvalidRoutePath {
                path: path.to_string(),
                message: format!("invalid route segment syntax '{segment}'"),
            });
        }

        if segment.is_empty() {
            return Err(RuntimeError::InvalidRoutePath {
                path: path.to_string(),
                message: "path contains an empty segment".to_string(),
            });
        }

        normalized_segments.push((*segment).to_string());
    }

    let mut normalized_paths = Vec::new();
    if let Some(catch_all) = optional_catch_all {
        normalized_paths.push(join_route_segments(&normalized_segments));
        let mut with_catch_all = normalized_segments;
        with_catch_all.push(format!("{{*{catch_all}}}"));
        normalized_paths.push(join_route_segments(&with_catch_all));
    } else {
        normalized_paths.push(join_route_segments(&normalized_segments));
    }

    Ok(normalized_paths)
}

fn compile_layout_targets(
    layout_specs: &[LayoutSpec],
) -> Result<Vec<CompiledLayoutTarget>, RuntimeError> {
    let mut sorted_layouts = layout_specs.to_vec();
    sorted_layouts.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.handler.cmp(&right.handler))
    });

    let mut seen_paths = std::collections::BTreeSet::new();
    let mut compiled = Vec::new();
    for layout in sorted_layouts {
        for normalized_path in normalize_route_paths(&layout.path)? {
            if !seen_paths.insert(normalized_path.clone()) {
                return Err(RuntimeError::InvalidConfig(format!(
                    "duplicate normalized layout path '{}'",
                    normalized_path
                )));
            }

            let (segment_count, static_segment_count) = {
                let segments = split_path_segments(&normalized_path);
                let static_count = segments
                    .iter()
                    .filter(|segment| {
                        !is_dynamic_segment(segment) && !is_catch_all_segment(segment)
                    })
                    .count();
                (segments.len(), static_count)
            };

            compiled.push(CompiledLayoutTarget {
                normalized_path,
                handler_id: layout.handler.clone(),
                segment_count,
                static_segment_count,
            });
        }
    }

    Ok(compiled)
}

fn resolve_layout_handlers_for_route(
    route_path: &str,
    layouts: &[CompiledLayoutTarget],
) -> Vec<String> {
    let route_segments = split_path_segments(route_path);
    let mut matched: Vec<&CompiledLayoutTarget> = layouts
        .iter()
        .filter(|layout| {
            layout_matches_route_prefix(layout.normalized_path.as_str(), &route_segments)
        })
        .collect();

    matched.sort_by(|left, right| {
        left.segment_count
            .cmp(&right.segment_count)
            .then_with(|| right.static_segment_count.cmp(&left.static_segment_count))
            .then_with(|| left.normalized_path.cmp(&right.normalized_path))
            .then_with(|| left.handler_id.cmp(&right.handler_id))
    });

    matched
        .into_iter()
        .map(|layout| layout.handler_id.clone())
        .collect()
}

fn layout_matches_route_prefix(layout_path: &str, route_segments: &[&str]) -> bool {
    let layout_segments = split_path_segments(layout_path);
    if layout_segments.len() > route_segments.len() {
        return false;
    }

    for (index, layout_segment) in layout_segments.iter().enumerate() {
        if is_catch_all_segment(layout_segment) {
            return index + 1 == layout_segments.len();
        }

        let route_segment = route_segments[index];
        if is_dynamic_segment(layout_segment) {
            continue;
        }
        if layout_segment != &route_segment {
            return false;
        }
    }

    true
}

fn split_path_segments(path: &str) -> Vec<&str> {
    if path == "/" {
        Vec::new()
    } else {
        path.trim_start_matches('/').split('/').collect()
    }
}

fn is_dynamic_segment(segment: &str) -> bool {
    segment.starts_with('{') && segment.ends_with('}') && !segment.starts_with("{*")
}

fn is_catch_all_segment(segment: &str) -> bool {
    segment.starts_with("{*") && segment.ends_with('}')
}

fn validate_param_name(path: &str, name: &str, kind: &str) -> Result<(), RuntimeError> {
    if name.is_empty() {
        return Err(RuntimeError::InvalidRoutePath {
            path: path.to_string(),
            message: format!("{kind} segment name must not be empty"),
        });
    }

    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(RuntimeError::InvalidRoutePath {
            path: path.to_string(),
            message: format!(
                "{kind} segment name '{name}' contains unsupported characters; use [A-Za-z0-9_]"
            ),
        });
    }

    Ok(())
}

fn join_route_segments(segments: &[String]) -> String {
    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

pub fn query_singleton_map(
    values: &BTreeMap<String, Vec<String>>,
) -> Result<HashMap<String, String>, RuntimeError> {
    let mut out = HashMap::new();
    for (key, items) in values {
        if let Some(value) = items.first() {
            out.insert(key.clone(), value.clone());
        } else {
            return Err(RuntimeError::RequestHandling(format!(
                "query key '{key}' has no value"
            )));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LayoutSpec, RouteSpec};

    fn route(name: &str, method: HttpMethod, path: &str, handler: &str) -> RouteSpec {
        RouteSpec {
            name: name.to_string(),
            method,
            path: path.to_string(),
            handler: handler.to_string(),
            entry_module: None,
            props_loader: None,
            middleware: Vec::new(),
            auth: None,
        }
    }

    #[test]
    fn test_dynamic_route_match_extracts_params() {
        let router = CompiledRouter::from_route_specs(&[
            route("users_show", HttpMethod::Get, "/users/{id}", "users.show"),
            route(
                "users_update",
                HttpMethod::Patch,
                "/users/{id}",
                "users.update",
            ),
        ])
        .unwrap();

        match router.match_route(HttpMethod::Get, "/users/42") {
            RouteMatch::Matched(matched) => {
                assert_eq!(matched.target.handler_id, "users.show");
                assert_eq!(matched.params.get("id").map(String::as_str), Some("42"));
            }
            other => panic!("unexpected route match: {other:?}"),
        }
    }

    #[test]
    fn test_method_not_allowed_returns_allowed_methods() {
        let router = CompiledRouter::from_route_specs(&[route(
            "users_show",
            HttpMethod::Get,
            "/users/{id}",
            "users.show",
        )])
        .unwrap();

        match router.match_route(HttpMethod::Delete, "/users/42") {
            RouteMatch::MethodNotAllowed { allowed } => {
                assert_eq!(allowed, vec![HttpMethod::Get]);
            }
            other => panic!("unexpected route match: {other:?}"),
        }
    }

    #[test]
    fn test_not_found_when_path_missing() {
        let router =
            CompiledRouter::from_route_specs(&[route("health", HttpMethod::Get, "/health", "h")])
                .unwrap();
        assert_eq!(
            router.match_route(HttpMethod::Get, "/missing"),
            RouteMatch::NotFound
        );
    }

    #[test]
    fn test_parse_query_string_keeps_multi_values() {
        let parsed = parse_query_string(Some("tag=rust&tag=web&limit=10"));
        assert_eq!(
            parsed.get("tag"),
            Some(&vec!["rust".to_string(), "web".to_string()])
        );
        assert_eq!(parsed.get("limit"), Some(&vec!["10".to_string()]));
    }

    #[test]
    fn test_next_style_dynamic_segment_is_supported() {
        let router = CompiledRouter::from_route_specs(&[route(
            "users_show",
            HttpMethod::Get,
            "/users/[id]",
            "users.show",
        )])
        .unwrap();

        match router.match_route(HttpMethod::Get, "/users/42") {
            RouteMatch::Matched(matched) => {
                assert_eq!(matched.target.handler_id, "users.show");
                assert_eq!(matched.params.get("id").map(String::as_str), Some("42"));
            }
            other => panic!("unexpected route match: {other:?}"),
        }
    }

    fn layout(name: &str, path: &str, handler: &str) -> LayoutSpec {
        LayoutSpec {
            name: name.to_string(),
            path: path.to_string(),
            handler: handler.to_string(),
        }
    }

    #[test]
    fn test_next_style_catch_all_segment_is_supported() {
        let router = CompiledRouter::from_route_specs(&[route(
            "docs",
            HttpMethod::Get,
            "/docs/[...slug]",
            "docs.show",
        )])
        .unwrap();

        match router.match_route(HttpMethod::Get, "/docs/guides/routing") {
            RouteMatch::Matched(matched) => {
                assert_eq!(matched.target.handler_id, "docs.show");
                assert_eq!(
                    matched.params.get("slug").map(String::as_str),
                    Some("guides/routing")
                );
            }
            other => panic!("unexpected route match: {other:?}"),
        }
    }

    #[test]
    fn test_next_style_optional_catch_all_matches_base_and_nested_paths() {
        let router = CompiledRouter::from_route_specs(&[route(
            "catalog",
            HttpMethod::Get,
            "/catalog/[[...slug]]",
            "catalog.show",
        )])
        .unwrap();

        match router.match_route(HttpMethod::Get, "/catalog") {
            RouteMatch::Matched(matched) => {
                assert_eq!(matched.target.handler_id, "catalog.show");
                assert!(!matched.params.contains_key("slug"));
            }
            other => panic!("unexpected route match: {other:?}"),
        }

        match router.match_route(HttpMethod::Get, "/catalog/shoes/running") {
            RouteMatch::Matched(matched) => {
                assert_eq!(
                    matched.params.get("slug").map(String::as_str),
                    Some("shoes/running")
                );
            }
            other => panic!("unexpected route match: {other:?}"),
        }
    }

    #[test]
    fn test_next_style_catch_all_must_be_last_segment() {
        let err = CompiledRouter::from_route_specs(&[route(
            "bad",
            HttpMethod::Get,
            "/docs/[...slug]/index",
            "bad.handler",
        )])
        .unwrap_err();

        assert!(matches!(err, RuntimeError::InvalidRoutePath { .. }));
        assert!(err.to_string().contains("final segment"));
    }

    #[test]
    fn test_next_style_optional_catch_all_must_be_last_segment() {
        let err = CompiledRouter::from_route_specs(&[route(
            "bad",
            HttpMethod::Get,
            "/docs/[[...slug]]/index",
            "bad.handler",
        )])
        .unwrap_err();

        assert!(matches!(err, RuntimeError::InvalidRoutePath { .. }));
        assert!(err.to_string().contains("final segment"));
    }

    #[test]
    fn test_layout_handlers_resolve_from_path_hierarchy() {
        let router = CompiledRouter::from_route_and_layout_specs(
            &[route(
                "dashboard.settings",
                HttpMethod::Get,
                "/dashboard/settings",
                "settings.page",
            )],
            &[
                layout("root", "/", "layout.root"),
                layout("dashboard", "/dashboard", "layout.dashboard"),
            ],
        )
        .unwrap();

        match router.match_route(HttpMethod::Get, "/dashboard/settings") {
            RouteMatch::Matched(matched) => {
                assert_eq!(
                    matched.target.layout_handlers,
                    vec!["layout.root".to_string(), "layout.dashboard".to_string()]
                );
            }
            other => panic!("unexpected route match: {other:?}"),
        }
    }

    #[test]
    fn test_layout_handlers_support_dynamic_layout_paths() {
        let router = CompiledRouter::from_route_and_layout_specs(
            &[route(
                "teams.members",
                HttpMethod::Get,
                "/teams/[team]/members",
                "members.page",
            )],
            &[
                layout("root", "/", "layout.root"),
                layout("teams", "/teams/[team]", "layout.team"),
            ],
        )
        .unwrap();

        match router.match_route(HttpMethod::Get, "/teams/albedo/members") {
            RouteMatch::Matched(matched) => {
                assert_eq!(
                    matched.target.layout_handlers,
                    vec!["layout.root".to_string(), "layout.team".to_string()]
                );
            }
            other => panic!("unexpected route match: {other:?}"),
        }
    }
}
