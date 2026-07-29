//! APERTURE · A1 — reading a declared source.
//!
//! The join between `declare.rs` (what the author wrote) and `client.rs` (the
//! cache, the coalescer, the wire). Given a source name, a route name and a
//! param lookup, this resolves a topic identity and a URL, then fetches under
//! that route's own policy — its refresh window, its sharing scope, its headers.
//!
//! ## Where invariant 2.7 actually closes
//!
//! A0 shipped an [`EgressPolicy`] that *could* carry declared hosts and never
//! had any, so every call fell through to the default address denies.
//! [`SourceReader::from_declarations`] is where that stops being theoretical: it
//! derives the allowlist from [`SourceRegistry::declared_hosts`], so the set of
//! hosts the app may reach is exactly the set it declared.
//!
//! Nothing is configured twice and nothing can drift, because there is only one
//! statement of intent. That is the security argument for declaration, and it is
//! the same shape as PRISM's: the safe thing and the ergonomic thing are the
//! same thing.
//!
//! ## Why the body must be JSON
//!
//! A topic's value **is** its JSON encoding — `bridge.rs:244` relies on exactly
//! that to hand a topic to a handler without a parse/re-encode round trip. So a
//! response that is not JSON cannot become a topic value; splicing it in would
//! turn one bad upstream into a `SyntaxError` in every handler that mentions the
//! topic. It is rejected here, at the boundary, where the offending source can
//! still be named.

use crate::aperture::cache::CachedResponse;
use crate::aperture::client::{ApertureClient, ApertureError, ApertureRequest, Disposition};
use crate::aperture::declare::{ResolvedSource, SourceDecl, SourceRegistry, SourceSchemaError};
use crate::aperture::egress::{EgressMode, EgressPolicy};
use crate::aperture::transport::ReqwestTransport;
use crate::aperture::{ResponseCache, Transport, DEFAULT_RESPONSE_BUDGET};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A successful read of a declared source.
#[derive(Debug, Clone)]
pub struct SourceRead {
    /// The minted topic identity this value belongs to.
    pub topic: String,
    /// The response, JSON-validated.
    pub response: CachedResponse,
    /// How it was obtained — the vocabulary the gates count in.
    pub disposition: Disposition,
}

impl SourceRead {
    /// The body bytes, which are valid JSON.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.response.body
    }
}

/// Why a read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceReadError {
    /// No such `source.route` in the registry. A build-time impossibility once
    /// the TSX binding lands, and a loud runtime error until then.
    UnknownRoute {
        /// Source name.
        source: String,
        /// Route name.
        route: String,
    },
    /// The upstream answered, but not with JSON.
    NotJson {
        /// The topic that would have carried it.
        topic: String,
        /// Content type the upstream claimed, if any.
        content_type: Option<String>,
    },
    /// The upstream answered with a non-success status.
    Status {
        /// The topic that would have carried it.
        topic: String,
        /// The status returned.
        status: u16,
    },
    /// The fetch itself failed.
    Fetch(ApertureError),
}

impl std::fmt::Display for SourceReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRoute { source, route } => write!(
                f,
                "aperture: no declared route `{source}.{route}` — check the `sources` block in albedo.config.ts"
            ),
            Self::NotJson {
                topic,
                content_type,
            } => write!(
                f,
                "aperture: `{topic}` did not return JSON (content-type: {}). A topic's value is \
                 its JSON encoding, so a non-JSON body cannot become one",
                content_type.as_deref().unwrap_or("none")
            ),
            Self::Status { topic, status } => {
                write!(f, "aperture: `{topic}` returned HTTP {status}")
            }
            Self::Fetch(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SourceReadError {}

impl From<ApertureError> for SourceReadError {
    fn from(err: ApertureError) -> Self {
        Self::Fetch(err)
    }
}

/// Resolves and reads declared sources.
#[derive(Debug)]
pub struct SourceReader {
    registry: Arc<SourceRegistry>,
    client: Arc<ApertureClient>,
}

impl SourceReader {
    /// Build over an existing registry and client.
    #[must_use]
    pub fn new(registry: Arc<SourceRegistry>, client: Arc<ApertureClient>) -> Self {
        Self { registry, client }
    }

    /// Lower a `sources` block and build the whole read path from it —
    /// **including the egress allowlist**.
    ///
    /// This is the one constructor boot should call, because it is the one that
    /// cannot forget to pass the declared hosts to the policy.
    ///
    /// # Errors
    /// [`SourceSchemaError`] from lowering, or a transport construction failure
    /// surfaced as [`SourceSchemaError::InvalidBase`] on a synthetic source
    /// name — a TLS backend that will not initialise is not a per-source
    /// problem, but it must not be silently swallowed either.
    pub fn from_declarations<F>(
        declarations: &BTreeMap<String, SourceDecl>,
        mode: EgressMode,
        env: F,
    ) -> Result<Self, SourceSchemaError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let registry = SourceRegistry::from_declarations(declarations, env)?;
        let policy = Arc::new(EgressPolicy::with_declared_hosts(
            mode,
            registry.declared_hosts(),
        ));
        let transport = ReqwestTransport::new(Arc::clone(&policy)).map_err(|err| {
            SourceSchemaError::InvalidBase {
                source: "<transport>".to_string(),
                reason: err.to_string(),
            }
        })?;
        let client = ApertureClient::new(
            Arc::new(transport),
            Arc::new(ResponseCache::new(DEFAULT_RESPONSE_BUDGET)),
            policy,
        );
        Ok(Self::new(Arc::new(registry), Arc::new(client)))
    }

    /// Build with an explicit transport. The seam the A1 tests use.
    ///
    /// # Errors
    /// [`SourceSchemaError`] from lowering.
    pub fn with_transport<F>(
        declarations: &BTreeMap<String, SourceDecl>,
        mode: EgressMode,
        env: F,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, SourceSchemaError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let registry = SourceRegistry::from_declarations(declarations, env)?;
        let policy = Arc::new(EgressPolicy::with_declared_hosts(
            mode,
            registry.declared_hosts(),
        ));
        let client = ApertureClient::new(
            transport,
            Arc::new(ResponseCache::new(DEFAULT_RESPONSE_BUDGET)),
            policy,
        );
        Ok(Self::new(Arc::new(registry), Arc::new(client)))
    }

    /// The lowered registry.
    #[must_use]
    pub fn registry(&self) -> &Arc<SourceRegistry> {
        &self.registry
    }

    /// The underlying client, for metrics.
    #[must_use]
    pub fn client(&self) -> &Arc<ApertureClient> {
        &self.client
    }

    /// Resolve `source.route` against a param lookup.
    ///
    /// `None` means *no topic* — an unknown route, a missing param, or a param
    /// outside the key alphabet. PRISM § 4's rule: that is a static page with an
    /// empty slot, not a failed render.
    #[must_use]
    pub fn resolve<'a, F>(&self, source: &str, route: &str, param: F) -> Option<ResolvedSource>
    where
        F: Fn(&str) -> Option<&'a str>,
    {
        self.registry.get(source, route)?.resolve(param)
    }

    /// Fetch a resolved source under its declared policy.
    ///
    /// # Errors
    /// [`SourceReadError`] for an unknown route, a failed fetch, a non-success
    /// status, or a body that is not JSON.
    pub async fn read(&self, resolved: &ResolvedSource) -> Result<SourceRead, SourceReadError> {
        let route = self
            .registry
            .get(&resolved.source, &resolved.route)
            .ok_or_else(|| SourceReadError::UnknownRoute {
                source: resolved.source.clone(),
                route: resolved.route.clone(),
            })?;

        let request = ApertureRequest {
            method: route.method.clone(),
            url: resolved.url.clone(),
            scope: route.scope.clone(),
            ttl: route.refresh,
            headers: route.headers.clone(),
            body: None,
        };

        let outcome = self.client.fetch(&request).await?;

        if !(200..300).contains(&outcome.response.status) {
            return Err(SourceReadError::Status {
                topic: resolved.topic.clone(),
                status: outcome.response.status,
            });
        }

        // `IgnoredAny` walks the JSON without building a tree — the same trick
        // `bridge.rs:263` uses to validate topic bytes cheaply. A successful
        // parse also proves the bytes are UTF-8.
        if serde_json::from_slice::<serde::de::IgnoredAny>(&outcome.response.body).is_err() {
            return Err(SourceReadError::NotJson {
                topic: resolved.topic.clone(),
                content_type: outcome.response.content_type.clone(),
            });
        }

        Ok(SourceRead {
            topic: resolved.topic.clone(),
            response: outcome.response,
            disposition: outcome.disposition,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aperture::client::{CountingTransport, WireResponse};
    use crate::aperture::declare::{AuthDecl, AuthScope, RouteDecl};

    fn json(body: &str, etag: &str) -> WireResponse {
        WireResponse {
            status: 200,
            body: body.as_bytes().to_vec(),
            headers: Vec::new(),
            etag: Some(etag.to_string()),
            last_modified: None,
            content_type: Some("application/json".to_string()),
        }
    }

    fn github_block() -> BTreeMap<String, SourceDecl> {
        let mut routes = BTreeMap::new();
        routes.insert(
            "repo".to_string(),
            RouteDecl {
                path: "/repos/{owner}/{name}".to_string(),
                refresh: Some("60s".to_string()),
                method: None,
            },
        );
        [(
            "github".to_string(),
            SourceDecl {
                base: "https://api.github.com".to_string(),
                auth: None,
                headers: BTreeMap::new(),
                routes,
            },
        )]
        .into_iter()
        .collect()
    }

    fn reader(transport: Arc<dyn Transport>) -> SourceReader {
        SourceReader::with_transport(&github_block(), EgressMode::Dev, |_| None, transport)
            .expect("lowers")
    }

    fn bound<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| *value)
        }
    }

    #[tokio::test]
    async fn a_declared_route_reads_end_to_end() {
        let transport = Arc::new(CountingTransport::always(json(r#"{"stars":42}"#, "\"v1\"")));
        let reader = reader(transport.clone());

        let resolved = reader
            .resolve(
                "github",
                "repo",
                bound(&[("owner", "anthropics"), ("name", "claude-code")]),
            )
            .expect("resolves");
        let read = reader.read(&resolved).await.expect("reads");

        assert_eq!(
            read.topic,
            "aperture:github.repo:owner=anthropics,name=claude-code"
        );
        assert_eq!(read.body(), br#"{"stars":42}"#);
        assert_eq!(read.disposition, Disposition::Fetched);
        assert_eq!(
            transport.requests()[0].url,
            "https://api.github.com/repos/anthropics/claude-code"
        );
    }

    #[tokio::test]
    async fn the_declared_refresh_window_is_what_the_cache_uses() {
        // `refresh: "60s"` means a second read inside the window must not touch
        // the wire. This is the route's policy reaching the client, which is the
        // whole job of this module.
        let transport = Arc::new(CountingTransport::always(json(r#"{"n":1}"#, "\"v1\"")));
        let reader = reader(transport.clone());
        let resolved = reader
            .resolve("github", "repo", bound(&[("owner", "a"), ("name", "b")]))
            .unwrap();

        reader.read(&resolved).await.unwrap();
        let second = reader.read(&resolved).await.unwrap();

        assert_eq!(second.disposition, Disposition::FreshHit);
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn many_readers_of_one_resource_cost_one_request() {
        // Gate 2, at the level an app actually uses: same route, same params,
        // many callers.
        let transport = Arc::new(
            CountingTransport::always(json(r#"{"n":1}"#, "\"v1\""))
                .with_delay(std::time::Duration::from_millis(30)),
        );
        let reader = Arc::new(reader(transport.clone()));
        let resolved = reader
            .resolve("github", "repo", bound(&[("owner", "a"), ("name", "b")]))
            .unwrap();

        let mut tasks = Vec::new();
        for _ in 0..64 {
            let reader = Arc::clone(&reader);
            let resolved = resolved.clone();
            tasks.push(tokio::spawn(async move { reader.read(&resolved).await }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn distinct_params_are_distinct_topics_and_distinct_requests() {
        let transport = Arc::new(CountingTransport::always(json(r#"{"n":1}"#, "\"v1\"")));
        let reader = reader(transport.clone());

        let one = reader
            .resolve("github", "repo", bound(&[("owner", "a"), ("name", "one")]))
            .unwrap();
        let two = reader
            .resolve("github", "repo", bound(&[("owner", "a"), ("name", "two")]))
            .unwrap();
        assert_ne!(one.topic, two.topic);

        reader.read(&one).await.unwrap();
        reader.read(&two).await.unwrap();
        assert_eq!(transport.calls(), 2);
    }

    #[tokio::test]
    async fn a_non_json_body_is_refused_rather_than_becoming_a_topic_value() {
        let transport = Arc::new(CountingTransport::always(WireResponse {
            status: 200,
            body: b"<html>nope</html>".to_vec(),
            headers: Vec::new(),
            etag: None,
            last_modified: None,
            content_type: Some("text/html".to_string()),
        }));
        let reader = reader(transport);
        let resolved = reader
            .resolve("github", "repo", bound(&[("owner", "a"), ("name", "b")]))
            .unwrap();

        assert!(matches!(
            reader.read(&resolved).await,
            Err(SourceReadError::NotJson { .. })
        ));
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error_not_a_value() {
        let transport = Arc::new(CountingTransport::always(WireResponse {
            status: 404,
            body: br#"{"message":"Not Found"}"#.to_vec(),
            headers: Vec::new(),
            etag: None,
            last_modified: None,
            content_type: Some("application/json".to_string()),
        }));
        let reader = reader(transport);
        let resolved = reader
            .resolve("github", "repo", bound(&[("owner", "a"), ("name", "b")]))
            .unwrap();

        assert!(matches!(
            reader.read(&resolved).await,
            Err(SourceReadError::Status { status: 404, .. })
        ));
    }

    #[test]
    fn an_unresolvable_param_yields_no_topic_rather_than_an_error() {
        let transport = Arc::new(CountingTransport::always(json("{}", "\"v\"")));
        let reader = reader(transport);
        assert!(reader
            .resolve("github", "repo", bound(&[("owner", "a")]))
            .is_none());
        assert!(reader
            .resolve("github", "repo", bound(&[("owner", "../x"), ("name", "b")]))
            .is_none());
        assert!(reader.resolve("github", "nope", bound(&[])).is_none());
    }

    #[test]
    fn the_egress_allowlist_is_derived_from_the_declaration() {
        // Invariant 2.7. A0 could carry declared hosts and never had any; this
        // is where the wiring actually exists, so it gets an assertion.
        let registry =
            SourceRegistry::from_declarations(&github_block(), |_| None).expect("lowers");
        let policy =
            EgressPolicy::with_declared_hosts(EgressMode::Serve, registry.declared_hosts());

        assert!(policy.is_declared("api.github.com"));
        assert!(!policy.is_declared("evil.test"));

        // And the practical consequence: a declared host may resolve into
        // private space (a service mesh), an undeclared one may not.
        let private: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        assert!(policy.check_address("api.github.com", private).is_ok());
        assert!(policy.check_address("evil.test", private).is_err());
    }

    #[tokio::test]
    async fn a_declared_credential_is_sent_but_never_reaches_the_identity() {
        let mut block = github_block();
        block.get_mut("github").unwrap().auth = Some(AuthDecl {
            bearer: Some("super-secret".to_string()),
            bearer_env: None,
            scope: Some(AuthScope::App),
        });

        let transport = Arc::new(CountingTransport::always(json(r#"{"n":1}"#, "\"v1\"")));
        let reader =
            SourceReader::with_transport(&block, EgressMode::Dev, |_| None, transport.clone())
                .expect("lowers");

        let resolved = reader
            .resolve("github", "repo", bound(&[("owner", "a"), ("name", "b")]))
            .unwrap();
        let read = reader.read(&resolved).await.unwrap();

        let sent = &transport.requests()[0];
        assert!(
            sent.headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer super-secret"),
            "the credential must actually be sent"
        );
        assert!(
            !read.topic.contains("secret"),
            "and must never be in the identity"
        );
    }
}
