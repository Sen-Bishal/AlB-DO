//! APERTURE · A1 — app-declared sources: the `sources` block in
//! `albedo.config.ts`.
//!
//! The sibling of `forge/declare.rs`, and deliberately so. FORGE lets an author
//! declare *where rows live*; APERTURE lets them declare *where responses come
//! from*. Both lower a config block into something the runtime can resolve
//! against a request, and both make the resulting identity compiler-known so
//! nobody types a topic string.
//!
//! ```ts
//! sources: {
//!   github: {
//!     base: "https://api.github.com",
//!     auth: { bearerEnv: "GITHUB_TOKEN", scope: "app" },
//!     routes: {
//!       repo: { path: "/repos/{owner}/{name}", refresh: "60s" },
//!     },
//!   },
//! }
//! ```
//!
//! ```tsx
//! const repo = useSharedSlot(github.repo({ owner: "anthropics", name: "claude-code" }))
//! ```
//!
//! ## Why declaration is the point, not the syntax
//!
//! An imperative `fetch()` in a component body can never be upgraded to a
//! webhook, because the system never learned what was being fetched. A declared
//! route can: swapping `refresh: "60s"` for a webhook binding changes no
//! application code (`APERTURE.md` § 4.6). Declaration is the only form that can
//! be optimised later, which is why the ergonomics and the architecture point
//! the same way here.
//!
//! ## The param alphabet is PRISM's, and that is load-bearing twice
//!
//! A resolved param must satisfy [`is_valid_partition_key`] —
//! `[A-Za-z0-9_-]{1,64}`. That buys two unrelated things at once:
//!
//! 1. **A stable topic identity**, minted the same way on the render, subscribe and (later) refresh
//!    paths — PRISM invariant 5, inherited wholesale.
//! 2. **URL-path safety for free.** Every character in that alphabet is unreserved in RFC 3986, so
//!    a resolved param cannot introduce a `/`, a `?`, a `#`, a `..`, or a percent-escape. There is
//!    no path from a route param to a different URL than the template describes, and no encoder to
//!    get wrong.

use crate::aperture::cache::CacheScope;
use crate::runtime::broadcast::is_valid_partition_key;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use url::Url;

/// Default refresh window when a route does not name one.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(60);

/// How widely a source's responses may be shared.
///
/// **There is no `Default`, and that is the point.** `APERTURE.md` § 7: a cache
/// keyed on URL alone under a per-user token serves one principal's data to
/// another. The safe default and the unsafe default are indistinguishable at the
/// call site, so the compiler refuses to choose — declaring `auth` without
/// `scope` is [`SourceSchemaError::AuthWithoutScope`], a build error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthScope {
    /// One credential for the whole app. Every caller would have sent the
    /// identical request, so one cache entry serves all of them.
    App,
    /// A per-principal credential. Requires a `user` in scope, which is item 5 —
    /// so this parses, errors, and names it, exactly as PRISM handled `user.id`.
    User,
}

/// The credential a source presents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AuthDecl {
    /// Name of the environment variable holding a bearer token. **Preferred**:
    /// it keeps the secret out of a file that is usually committed.
    #[serde(default, rename = "bearerEnv", alias = "bearer_env")]
    pub bearer_env: Option<String>,
    /// A literal bearer token. Supported for local experimentation and
    /// deliberately second-class — a token in `albedo.config.ts` is a token in
    /// version control.
    #[serde(default)]
    pub bearer: Option<String>,
    /// Who the response may be shared with. No default; see [`AuthScope`].
    #[serde(default)]
    pub scope: Option<AuthScope>,
}

/// One route on a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteDecl {
    /// Path template relative to the source's `base`, e.g. `/repos/{owner}/{name}`.
    pub path: String,
    /// Refresh window, e.g. `"30s"`, `"5m"`, `"1h"`, `"250ms"`. Defaults to
    /// [`DEFAULT_REFRESH`].
    #[serde(default)]
    pub refresh: Option<String>,
    /// HTTP method. Defaults to `GET` and must be idempotent — a declared route
    /// is a *dependency* (`APERTURE.md` § 3 case 1), and a non-idempotent call is
    /// an effect that belongs on the A2 write path.
    #[serde(default)]
    pub method: Option<String>,
}

/// One app-declared source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceDecl {
    /// Origin the routes hang off, e.g. `https://api.github.com`. This is also
    /// the source's **egress allowlist entry** — APERTURE invariant 2.7.
    pub base: String,
    /// Credential, if the source needs one.
    #[serde(default)]
    pub auth: Option<AuthDecl>,
    /// Static headers sent with every request to this source.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The routes, keyed by the name the author calls: `github.repo(…)`.
    #[serde(default)]
    pub routes: BTreeMap<String, RouteDecl>,
}

/// Why a `sources` block was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSchemaError {
    /// `base` did not parse, or is not an `http(s)` origin.
    InvalidBase {
        /// Source name.
        source: String,
        /// What was wrong.
        reason: String,
    },
    /// `base` carried a query string or fragment. Those belong on the route.
    BaseHasQueryOrFragment {
        /// Source name.
        source: String,
    },
    /// A source declared no routes, so nothing can be called on it.
    NoRoutes {
        /// Source name.
        source: String,
    },
    /// A source or route name is not usable as a TSX identifier.
    InvalidName {
        /// `"source"` or `"route"`.
        kind: &'static str,
        /// The offending name.
        name: String,
    },
    /// A path template is malformed.
    InvalidPath {
        /// Source name.
        source: String,
        /// Route name.
        route: String,
        /// What was wrong.
        reason: String,
    },
    /// The same `{param}` appeared twice in one template.
    DuplicateParam {
        /// Source name.
        source: String,
        /// Route name.
        route: String,
        /// The repeated param.
        param: String,
    },
    /// A refresh window did not parse.
    InvalidRefresh {
        /// Source name.
        source: String,
        /// Route name.
        route: String,
        /// The offending value.
        value: String,
    },
    /// A declared route named a non-idempotent method.
    NonIdempotentMethod {
        /// Source name.
        source: String,
        /// Route name.
        route: String,
        /// The offending method.
        method: String,
    },
    /// `auth` was declared with no `scope`. **The § 7 build error.**
    AuthWithoutScope {
        /// Source name.
        source: String,
    },
    /// `scope: "user"` — parses, and names item 5.
    UserScopeNeedsAuth {
        /// Source name.
        source: String,
    },
    /// `bearerEnv` named a variable that is not set.
    MissingEnv {
        /// Source name.
        source: String,
        /// The variable name.
        variable: String,
    },
}

impl std::fmt::Display for SourceSchemaError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBase { source, reason } => {
                write!(f, "source `{source}`: invalid `base` — {reason}")
            }
            Self::BaseHasQueryOrFragment { source } => write!(
                f,
                "source `{source}`: `base` must be an origin and path prefix only — \
                 move the query or fragment onto the route's `path`"
            ),
            Self::NoRoutes { source } => {
                write!(
                    f,
                    "source `{source}`: declares no `routes`, so nothing can be read from it"
                )
            }
            Self::InvalidName { kind, name } => write!(
                f,
                "{kind} name `{name}` is not a valid identifier — it is called from TSX as \
                 `source.route(…)`, so it must be a plain JS name"
            ),
            Self::InvalidPath {
                source,
                route,
                reason,
            } => write!(
                f,
                "source `{source}`, route `{route}`: invalid `path` — {reason}"
            ),
            Self::DuplicateParam {
                source,
                route,
                param,
            } => write!(
                f,
                "source `{source}`, route `{route}`: `{{{param}}}` appears twice in `path`"
            ),
            Self::InvalidRefresh {
                source,
                route,
                value,
            } => write!(
                f,
                "source `{source}`, route `{route}`: could not parse `refresh: \"{value}\"` \
                 (expected e.g. \"250ms\", \"30s\", \"5m\", \"1h\")"
            ),
            Self::NonIdempotentMethod {
                source,
                route,
                method,
            } => write!(
                f,
                "source `{source}`, route `{route}`: `{method}` is not idempotent. A declared \
                 route is a dependency that may be cached, coalesced and refreshed; a \
                 non-idempotent call is an effect and belongs in an action body"
            ),
            Self::AuthWithoutScope { source } => write!(
                f,
                "source `{source}`: `auth` requires an explicit `scope`. Use `scope: \"app\"` \
                 when one credential serves every user, or `scope: \"user\"` when the \
                 credential is per-person. There is no default because the two are \
                 indistinguishable here and guessing wrong leaks one user's data to another"
            ),
            Self::UserScopeNeedsAuth { source } => write!(
                f,
                "source `{source}`: `scope: \"user\"` needs a signed-in principal to key the \
                 cache by, which arrives with auth (TODO #1 item 5). Until then, use \
                 `scope: \"app\"` with a service credential"
            ),
            Self::MissingEnv { source, variable } => write!(
                f,
                "source `{source}`: `bearerEnv` names `{variable}`, which is not set in the \
                 environment"
            ),
        }
    }
}

impl std::error::Error for SourceSchemaError {}

/// One piece of a lowered path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    /// Literal text, reproduced verbatim.
    Literal(String),
    /// A `{name}` hole, filled from the caller's params.
    Param(String),
}

/// A lowered, validated route: everything needed to turn a param set into a URL
/// and a topic identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRoute {
    /// Source name, as declared.
    pub source: String,
    /// Route name, as declared.
    pub route: String,
    /// HTTP method, uppercased. Always idempotent.
    pub method: String,
    /// Origin plus any base path, with no trailing slash.
    pub base: String,
    /// The host, for the egress allowlist.
    pub host: String,
    /// The lowered path template.
    pub segments: Vec<PathSegment>,
    /// Param names in template order. Stable, so the minted identity is stable.
    pub params: Vec<String>,
    /// Refresh window.
    pub refresh: Duration,
    /// Sharing scope for the response cache.
    pub scope: CacheScope,
    /// Headers to send, **including any credential**.
    ///
    /// Never part of the cache key or a journal entry — APERTURE § 11 R6. A
    /// digest over headers would turn a journal dump into a credential dump.
    pub headers: Vec<(String, String)>,
}

/// A [`SourceRoute`] resolved against a concrete param set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    /// The minted topic identity.
    pub topic: String,
    /// The absolute URL to request.
    pub url: String,
    /// Source name.
    pub source: String,
    /// Route name.
    pub route: String,
}

/// Mint the canonical topic identity for a route and its bound params.
///
/// Shape: `aperture:{source}.{route}` with no params, or
/// `aperture:{source}.{route}:{p1=v1,p2=v2}` with them, in template order.
///
/// The `aperture:` prefix keeps this namespace disjoint from FORGE's
/// `{collection}:{key}`, and the `=`/`,` separators are outside the partition-key
/// alphabet, so [`crate::runtime::topics::split_partition_topic`] can never
/// mistake an APERTURE topic for a FORGE partition — it splits at the last colon
/// and then rejects the key. The two namespaces cannot alias.
#[must_use]
pub fn source_topic_name(source: &str, route: &str, bound: &[(String, String)]) -> Option<String> {
    for (_, value) in bound {
        if !is_valid_partition_key(value) {
            return None;
        }
    }
    if bound.is_empty() {
        return Some(format!("aperture:{source}.{route}"));
    }
    let joined = bound
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("aperture:{source}.{route}:{joined}"))
}

impl SourceRoute {
    /// Resolve this route against a param lookup.
    ///
    /// Returns `None` when any param is missing or outside the key alphabet.
    /// **Not an error** — PRISM § 4's rule, inherited: a weird value in a URL
    /// yields no topic and a static page, never a failed render.
    pub fn resolve<'a, F>(&self, param: F) -> Option<ResolvedSource>
    where
        F: Fn(&str) -> Option<&'a str>,
    {
        let mut bound: Vec<(String, String)> = Vec::with_capacity(self.params.len());
        let mut path = String::new();
        for segment in &self.segments {
            match segment {
                PathSegment::Literal(text) => path.push_str(text),
                PathSegment::Param(name) => {
                    let value = param(name.as_str())?;
                    if !is_valid_partition_key(value) {
                        return None;
                    }
                    path.push_str(value);
                    bound.push((name.clone(), value.to_string()));
                }
            }
        }
        let topic = source_topic_name(&self.source, &self.route, &bound)?;
        Some(ResolvedSource {
            topic,
            url: format!("{}{path}", self.base),
            source: self.source.clone(),
            route: self.route.clone(),
        })
    }

    /// `"{source}.{route}"` — the registry's lookup key and the call site's
    /// spelling.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.source, self.route)
    }
}

/// Every lowered route an app declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceRegistry {
    routes: BTreeMap<String, SourceRoute>,
}

impl SourceRegistry {
    /// Lower and validate a whole `sources` block.
    ///
    /// `env` resolves `bearerEnv` names. Passed in rather than read from the
    /// process so lowering is a pure function and the validation tests do not
    /// mutate global state.
    ///
    /// # Errors
    /// Any [`SourceSchemaError`]. Boot should surface this verbatim and refuse
    /// to start — invariant 2.8, *fail closed, loudly*.
    pub fn from_declarations<F>(
        declarations: &BTreeMap<String, SourceDecl>,
        env: F,
    ) -> Result<Self, SourceSchemaError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut routes = BTreeMap::new();
        for (source_name, decl) in declarations {
            require_identifier("source", source_name)?;
            let (base, host) = lower_base(source_name, &decl.base)?;
            if decl.routes.is_empty() {
                return Err(SourceSchemaError::NoRoutes {
                    source: source_name.clone(),
                });
            }
            let (scope, auth_header) = lower_auth(source_name, decl.auth.as_ref(), &env)?;

            let mut headers: Vec<(String, String)> = decl
                .headers
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            if let Some(header) = auth_header {
                headers.push(header);
            }

            for (route_name, route) in &decl.routes {
                require_identifier("route", route_name)?;
                let (segments, params) = lower_path(source_name, route_name, &route.path)?;
                let refresh = match &route.refresh {
                    Some(text) => {
                        parse_duration(text).ok_or_else(|| SourceSchemaError::InvalidRefresh {
                            source: source_name.clone(),
                            route: route_name.clone(),
                            value: text.clone(),
                        })?
                    }
                    None => DEFAULT_REFRESH,
                };
                let method = route
                    .method
                    .as_deref()
                    .unwrap_or("GET")
                    .to_ascii_uppercase();
                if !matches!(method.as_str(), "GET" | "HEAD") {
                    return Err(SourceSchemaError::NonIdempotentMethod {
                        source: source_name.clone(),
                        route: route_name.clone(),
                        method,
                    });
                }

                let lowered = SourceRoute {
                    source: source_name.clone(),
                    route: route_name.clone(),
                    method,
                    base: base.clone(),
                    host: host.clone(),
                    segments,
                    params,
                    refresh,
                    scope: scope.clone(),
                    headers: headers.clone(),
                };
                routes.insert(lowered.qualified_name(), lowered);
            }
        }
        Ok(Self { routes })
    }

    /// Look up a route by source and route name.
    #[must_use]
    pub fn get(&self, source: &str, route: &str) -> Option<&SourceRoute> {
        self.routes.get(&format!("{source}.{route}"))
    }

    /// Look up by the qualified `"source.route"` spelling.
    #[must_use]
    pub fn get_qualified(&self, qualified: &str) -> Option<&SourceRoute> {
        self.routes.get(qualified)
    }

    /// Every declared host — **the egress allowlist**, APERTURE invariant 2.7.
    ///
    /// This is the whole security argument for declaration: the allowlist is
    /// derived from what the author already wrote, so there is nothing separate
    /// to configure and nothing to forget.
    #[must_use]
    pub fn declared_hosts(&self) -> BTreeSet<String> {
        self.routes
            .values()
            .map(|route| route.host.clone())
            .collect()
    }

    /// Iterate every lowered route, in stable `"source.route"` order.
    pub fn iter(&self) -> impl Iterator<Item = &SourceRoute> {
        self.routes.values()
    }

    /// How many routes were declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the app declared no sources at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Total path placeholders across every route.
    ///
    /// Only [`crate::aperture::typegen`] wants this, to size one buffer up
    /// front — but it is the registry that knows, and asking here is cheaper
    /// than the reallocation the guess avoids.
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.routes.values().map(|route| route.params.len()).sum()
    }
}

/// Names reaching TSX as `source.route(…)` must be plain JS identifiers.
fn require_identifier(kind: &'static str, name: &str) -> Result<(), SourceSchemaError> {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' || first == '$' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SourceSchemaError::InvalidName {
            kind,
            name: name.to_string(),
        })
    }
}

/// Validate `base` and split out its host.
fn lower_base(source: &str, base: &str) -> Result<(String, String), SourceSchemaError> {
    let url = Url::parse(base).map_err(|err| SourceSchemaError::InvalidBase {
        source: source.to_string(),
        reason: err.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(SourceSchemaError::InvalidBase {
            source: source.to_string(),
            reason: format!("scheme `{}` is not http or https", url.scheme()),
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(SourceSchemaError::BaseHasQueryOrFragment {
            source: source.to_string(),
        });
    }
    let host = url
        .host_str()
        .ok_or_else(|| SourceSchemaError::InvalidBase {
            source: source.to_string(),
            reason: "no host".to_string(),
        })?
        .to_ascii_lowercase();

    // Trailing slash removed so joining with a path template that starts with
    // `/` cannot produce a double slash — which some upstreams treat as a
    // different resource, and which would therefore be a silent cache split.
    let normalized = base.trim_end_matches('/').to_string();
    Ok((normalized, host))
}

/// Resolve the credential and the sharing scope together, because § 7 ties them.
fn lower_auth<F>(
    source: &str,
    auth: Option<&AuthDecl>,
    env: &F,
) -> Result<(CacheScope, Option<(String, String)>), SourceSchemaError>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(auth) = auth else {
        // No credential, so the response is public and shareable with everyone.
        return Ok((CacheScope::Public, None));
    };

    match auth.scope {
        None => {
            return Err(SourceSchemaError::AuthWithoutScope {
                source: source.to_string(),
            })
        }
        Some(AuthScope::User) => {
            return Err(SourceSchemaError::UserScopeNeedsAuth {
                source: source.to_string(),
            })
        }
        Some(AuthScope::App) => {}
    }

    let token = if let Some(variable) = &auth.bearer_env {
        Some(env(variable).ok_or_else(|| SourceSchemaError::MissingEnv {
            source: source.to_string(),
            variable: variable.clone(),
        })?)
    } else {
        auth.bearer.clone()
    };

    let header = token.map(|token| ("authorization".to_string(), format!("Bearer {token}")));
    Ok((CacheScope::App, header))
}

/// Parse `/repos/{owner}/{name}` into segments and its ordered param list.
fn lower_path(
    source: &str,
    route: &str,
    path: &str,
) -> Result<(Vec<PathSegment>, Vec<String>), SourceSchemaError> {
    let invalid = |reason: &str| SourceSchemaError::InvalidPath {
        source: source.to_string(),
        route: route.to_string(),
        reason: reason.to_string(),
    };

    if !path.starts_with('/') {
        return Err(invalid("must start with `/`"));
    }
    if path.contains("//") {
        return Err(invalid("contains an empty path segment (`//`)"));
    }
    // `..` in a template would let a declaration reach outside its own base,
    // which is the one thing the base is supposed to guarantee.
    if path.split('/').any(|segment| segment == "..") {
        return Err(invalid("contains `..`"));
    }

    let mut segments = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut literal = String::new();
    let mut rest = path;

    while let Some(open) = rest.find('{') {
        literal.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or_else(|| invalid("unclosed `{`"))?;
        let name = &after[..close];
        if name.is_empty() {
            return Err(invalid("empty `{}` placeholder"));
        }
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(invalid(&format!(
                "placeholder `{{{name}}}` must be alphanumeric or `_`"
            )));
        }
        if params.iter().any(|existing| existing == name) {
            return Err(SourceSchemaError::DuplicateParam {
                source: source.to_string(),
                route: route.to_string(),
                param: name.to_string(),
            });
        }
        if !literal.is_empty() {
            segments.push(PathSegment::Literal(std::mem::take(&mut literal)));
        }
        segments.push(PathSegment::Param(name.to_string()));
        params.push(name.to_string());
        rest = &after[close + 1..];
    }

    if rest.contains('}') {
        return Err(invalid("unmatched `}`"));
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        segments.push(PathSegment::Literal(literal));
    }

    Ok((segments, params))
}

/// Parse `"250ms"`, `"30s"`, `"5m"`, `"1h"`.
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    if digits.is_empty() {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(value)),
        "s" => Some(Duration::from_secs(value)),
        "m" => value.checked_mul(60).map(Duration::from_secs),
        "h" => value.checked_mul(3_600).map(Duration::from_secs),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn source(base: &str, routes: &[(&str, &str)]) -> SourceDecl {
        SourceDecl {
            base: base.to_string(),
            auth: None,
            headers: BTreeMap::new(),
            routes: routes
                .iter()
                .map(|(name, path)| {
                    (
                        (*name).to_string(),
                        RouteDecl {
                            path: (*path).to_string(),
                            refresh: None,
                            method: None,
                        },
                    )
                })
                .collect(),
        }
    }

    fn registry(decls: &[(&str, SourceDecl)]) -> Result<SourceRegistry, SourceSchemaError> {
        let map: BTreeMap<String, SourceDecl> = decls
            .iter()
            .map(|(name, decl)| ((*name).to_string(), decl.clone()))
            .collect();
        SourceRegistry::from_declarations(&map, no_env)
    }

    #[test]
    fn a_declared_route_resolves_to_a_url_and_a_topic() {
        let registry = registry(&[(
            "github",
            source(
                "https://api.github.com",
                &[("repo", "/repos/{owner}/{name}")],
            ),
        )])
        .expect("lowers");

        let route = registry.get("github", "repo").expect("route exists");
        let bound = [("owner", "anthropics"), ("name", "claude-code")];
        let resolved = route
            .resolve(|param| {
                bound
                    .iter()
                    .find(|(key, _)| *key == param)
                    .map(|(_, value)| *value)
            })
            .expect("resolves");

        assert_eq!(
            resolved.url,
            "https://api.github.com/repos/anthropics/claude-code"
        );
        assert_eq!(
            resolved.topic,
            "aperture:github.repo:owner=anthropics,name=claude-code"
        );
    }

    #[test]
    fn the_minted_identity_is_stable_and_order_follows_the_template() {
        // PRISM invariant 5: render, subscribe and refresh must mint the same
        // identity. Template order rather than argument order is what makes that
        // true regardless of how the caller spells the object literal.
        let registry = registry(&[(
            "api",
            source("https://x.test", &[("thing", "/a/{b}/c/{d}")]),
        )])
        .expect("lowers");
        let route = registry.get("api", "thing").unwrap();

        let forward = route.resolve(|p| match p {
            "b" => Some("1"),
            "d" => Some("2"),
            _ => None,
        });
        let reverse = route.resolve(|p| match p {
            "d" => Some("2"),
            "b" => Some("1"),
            _ => None,
        });
        assert_eq!(forward, reverse);
        assert_eq!(forward.unwrap().topic, "aperture:api.thing:b=1,d=2");
    }

    #[test]
    fn an_aperture_topic_can_never_be_read_as_a_forge_partition() {
        // The two namespaces share a broadcast registry. If `split_partition_topic`
        // could claim an APERTURE topic, a source would alias onto a collection.
        use crate::runtime::topics::split_partition_topic;
        let registry = registry(&[(
            "github",
            source("https://api.github.com", &[("repo", "/repos/{owner}")]),
        )])
        .expect("lowers");
        let resolved = registry
            .get("github", "repo")
            .unwrap()
            .resolve(|_| Some("anthropics"))
            .unwrap();
        assert_eq!(split_partition_topic(&resolved.topic), None);
    }

    #[test]
    fn a_param_outside_the_alphabet_resolves_to_nothing() {
        // A URL segment is attacker-controlled and reaches a URL path here, so
        // anything that could change which resource is addressed must mint no
        // topic at all rather than a sanitised one.
        let registry = registry(&[("api", source("https://x.test", &[("thing", "/a/{id}")]))])
            .expect("lowers");
        let route = registry.get("api", "thing").unwrap();

        for hostile in [
            "../../etc/passwd",
            "a/b",
            "a?b",
            "a#b",
            "a%2Fb",
            "",
            "a:b",
            &"x".repeat(65),
        ] {
            assert!(
                route.resolve(|_| Some(hostile)).is_none(),
                "param {hostile:?} must not resolve"
            );
        }
    }

    #[test]
    fn a_missing_param_resolves_to_nothing_rather_than_erroring() {
        let registry =
            registry(&[("api", source("https://x.test", &[("thing", "/a/{id}")]))]).unwrap();
        assert!(registry
            .get("api", "thing")
            .unwrap()
            .resolve(|_| None)
            .is_none());
    }

    #[test]
    fn declared_hosts_are_the_egress_allowlist() {
        let registry = registry(&[
            (
                "github",
                source("https://api.github.com", &[("repo", "/r")]),
            ),
            (
                "internal",
                source("http://payments.internal", &[("fee", "/f")]),
            ),
        ])
        .expect("lowers");

        let hosts = registry.declared_hosts();
        assert!(hosts.contains("api.github.com"));
        assert!(hosts.contains("payments.internal"));
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn auth_without_scope_is_a_build_error() {
        // APERTURE § 7 — the CVE-class default is unspellable.
        let mut decl = source("https://x.test", &[("thing", "/a")]);
        decl.auth = Some(AuthDecl {
            bearer: Some("t".to_string()),
            bearer_env: None,
            scope: None,
        });
        assert!(matches!(
            registry(&[("api", decl)]),
            Err(SourceSchemaError::AuthWithoutScope { .. })
        ));
    }

    #[test]
    fn user_scope_parses_and_names_item_five() {
        let mut decl = source("https://x.test", &[("thing", "/a")]);
        decl.auth = Some(AuthDecl {
            bearer: Some("t".to_string()),
            bearer_env: None,
            scope: Some(AuthScope::User),
        });
        let err = registry(&[("api", decl)]).expect_err("must refuse");
        assert!(matches!(err, SourceSchemaError::UserScopeNeedsAuth { .. }));
        assert!(
            err.to_string().contains("item 5"),
            "the error must name the item"
        );
    }

    #[test]
    fn app_scope_with_a_bearer_env_becomes_an_authorization_header() {
        let mut decl = source("https://x.test", &[("thing", "/a")]);
        decl.auth = Some(AuthDecl {
            bearer: None,
            bearer_env: Some("TOKEN".to_string()),
            scope: Some(AuthScope::App),
        });
        let map: BTreeMap<String, SourceDecl> = [("api".to_string(), decl)].into_iter().collect();
        let registry = SourceRegistry::from_declarations(&map, |name| {
            (name == "TOKEN").then(|| "secret-value".to_string())
        })
        .expect("lowers");

        let route = registry.get("api", "thing").unwrap();
        assert_eq!(route.scope, CacheScope::App);
        assert_eq!(
            route.headers,
            vec![(
                "authorization".to_string(),
                "Bearer secret-value".to_string()
            )]
        );
    }

    #[test]
    fn a_credential_never_reaches_the_topic_identity() {
        // APERTURE § 11 R6. The identity is derived from source, route and
        // params — never from headers.
        let mut decl = source("https://x.test", &[("thing", "/a/{id}")]);
        decl.auth = Some(AuthDecl {
            bearer: Some("super-secret".to_string()),
            bearer_env: None,
            scope: Some(AuthScope::App),
        });
        let registry = registry(&[("api", decl)]).expect("lowers");
        let resolved = registry
            .get("api", "thing")
            .unwrap()
            .resolve(|_| Some("7"))
            .unwrap();
        assert!(!resolved.topic.contains("secret"));
        assert!(!resolved.url.contains("secret"));
    }

    #[test]
    fn a_missing_env_variable_fails_the_build() {
        let mut decl = source("https://x.test", &[("thing", "/a")]);
        decl.auth = Some(AuthDecl {
            bearer: None,
            bearer_env: Some("ABSENT".to_string()),
            scope: Some(AuthScope::App),
        });
        assert!(matches!(
            registry(&[("api", decl)]),
            Err(SourceSchemaError::MissingEnv { .. })
        ));
    }

    #[test]
    fn a_source_with_no_auth_is_public_scope() {
        let registry = registry(&[("api", source("https://x.test", &[("thing", "/a")]))]).unwrap();
        assert_eq!(
            registry.get("api", "thing").unwrap().scope,
            CacheScope::Public
        );
    }

    #[test]
    fn non_idempotent_methods_are_refused() {
        let mut decl = source("https://x.test", &[("charge", "/charge")]);
        decl.routes.get_mut("charge").unwrap().method = Some("post".to_string());
        let err = registry(&[("api", decl)]).expect_err("must refuse");
        assert!(matches!(err, SourceSchemaError::NonIdempotentMethod { .. }));
        assert!(err.to_string().contains("action body"));
    }

    #[test]
    fn malformed_paths_are_refused() {
        for (path, label) in [
            ("repos/{owner}", "no leading slash"),
            ("/a//b", "empty segment"),
            ("/a/../b", "dot dot"),
            ("/a/{unclosed", "unclosed brace"),
            ("/a/}", "unmatched close"),
            ("/a/{}", "empty placeholder"),
            ("/a/{bad-name}", "invalid placeholder"),
        ] {
            let decl = source("https://x.test", &[("thing", path)]);
            assert!(
                matches!(
                    registry(&[("api", decl)]),
                    Err(SourceSchemaError::InvalidPath { .. })
                ),
                "path {path:?} ({label}) must be refused"
            );
        }
    }

    #[test]
    fn a_repeated_placeholder_is_refused() {
        let decl = source("https://x.test", &[("thing", "/a/{id}/b/{id}")]);
        assert!(matches!(
            registry(&[("api", decl)]),
            Err(SourceSchemaError::DuplicateParam { .. })
        ));
    }

    #[test]
    fn bad_bases_are_refused() {
        for base in ["not a url", "ftp://x.test", "file:///etc"] {
            let decl = source(base, &[("thing", "/a")]);
            assert!(
                matches!(
                    registry(&[("api", decl)]),
                    Err(SourceSchemaError::InvalidBase { .. })
                ),
                "base {base:?} must be refused"
            );
        }
        let decl = source("https://x.test?a=1", &[("thing", "/a")]);
        assert!(matches!(
            registry(&[("api", decl)]),
            Err(SourceSchemaError::BaseHasQueryOrFragment { .. })
        ));
    }

    #[test]
    fn a_trailing_slash_on_base_does_not_produce_a_double_slash() {
        // Two spellings of the same resource would otherwise be two cache
        // entries — a silent halving of the hit rate.
        let registry = registry(&[("api", source("https://x.test/", &[("thing", "/a")]))]).unwrap();
        let resolved = registry
            .get("api", "thing")
            .unwrap()
            .resolve(|_| None)
            .unwrap();
        assert_eq!(resolved.url, "https://x.test/a");
    }

    #[test]
    fn names_that_are_not_identifiers_are_refused() {
        let decl = source("https://x.test", &[("thing", "/a")]);
        assert!(matches!(
            registry(&[("my-source", decl.clone())]),
            Err(SourceSchemaError::InvalidName { kind: "source", .. })
        ));

        let bad_route = source("https://x.test", &[("my-route", "/a")]);
        assert!(matches!(
            registry(&[("api", bad_route)]),
            Err(SourceSchemaError::InvalidName { kind: "route", .. })
        ));
    }

    #[test]
    fn a_source_with_no_routes_is_refused() {
        let decl = SourceDecl {
            base: "https://x.test".to_string(),
            auth: None,
            headers: BTreeMap::new(),
            routes: BTreeMap::new(),
        };
        assert!(matches!(
            registry(&[("api", decl)]),
            Err(SourceSchemaError::NoRoutes { .. })
        ));
    }

    #[test]
    fn refresh_windows_parse() {
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration(" 60s "), Some(Duration::from_secs(60)));
        for bad in ["", "s", "60", "60x", "-1s", "1.5s"] {
            assert_eq!(parse_duration(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn an_unspecified_refresh_takes_the_default() {
        let registry = registry(&[("api", source("https://x.test", &[("thing", "/a")]))]).unwrap();
        assert_eq!(
            registry.get("api", "thing").unwrap().refresh,
            DEFAULT_REFRESH
        );
    }

    #[test]
    fn a_paramless_route_mints_a_bare_identity() {
        let registry =
            registry(&[("api", source("https://x.test", &[("status", "/status")]))]).unwrap();
        let resolved = registry
            .get("api", "status")
            .unwrap()
            .resolve(|_| None)
            .unwrap();
        assert_eq!(resolved.topic, "aperture:api.status");
        assert_eq!(resolved.url, "https://x.test/status");
    }
}
