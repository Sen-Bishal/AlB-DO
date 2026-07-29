//! APERTURE · A0 — the `reqwest`-backed [`Transport`], and the resolver that
//! makes the egress policy un-bypassable.
//!
//! Separated from `client.rs` so that everything above this file is free of
//! `reqwest` types: the cache, the coalescer and the metrics are all testable
//! against [`CountingTransport`](super::client::CountingTransport) with no
//! network, no TLS and no DNS.

use crate::aperture::client::{ApertureError, Transport, WireRequest, WireResponse};
use crate::aperture::egress::EgressPolicy;
use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::sync::Arc;

/// A DNS resolver that refuses addresses the egress policy denies.
///
/// **This is the only place the client learns an address**, which is the entire
/// point. A policy that resolves, checks, and then hands the *hostname* to the
/// HTTP client has told the client to resolve again — and a DNS server that
/// answers differently the second time walks straight through. Doing the check
/// here means there is no second lookup and therefore no window.
#[derive(Debug)]
pub struct ApertureResolver {
    policy: Arc<EgressPolicy>,
}

impl ApertureResolver {
    /// A resolver enforcing `policy`.
    #[must_use]
    pub fn new(policy: Arc<EgressPolicy>) -> Self {
        Self { policy }
    }
}

impl Resolve for ApertureResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = Arc::clone(&self.policy);
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port 0: `reqwest` overwrites it with the URL's port. The resolver
            // contract is about addresses, not endpoints.
            let resolved = tokio::net::lookup_host((host.as_str(), 0u16))
                .await
                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> { Box::new(err) })?;

            let (permitted, denial) = policy.filter_addresses(&host, resolved);
            if permitted.is_empty() {
                if let Some(denial) = denial {
                    // Annotated rather than `as`-cast: the async block's error
                    // type is inferred from this arm, and `Box::new` alone
                    // would fix it to `Box<EgressDenial>`.
                    let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(denial);
                    return Err(boxed);
                }
            }
            let addrs: Addrs = Box::new(permitted.into_iter());
            Ok(addrs)
        })
    }
}

/// The production [`Transport`].
#[derive(Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build a transport whose DNS resolution enforces `policy`.
    ///
    /// Redirects are **disabled**. A 302 is an upstream choosing a new URL, and
    /// following it silently would let a permitted host redirect to a denied
    /// address after the policy already ran — the same bypass the resolver
    /// closes, re-opened at the HTTP layer. A caller that wants a redirect can
    /// see the `Location` header and decide.
    ///
    /// # Errors
    /// Propagates `reqwest`'s builder failure (TLS backend initialisation).
    pub fn new(policy: Arc<EgressPolicy>) -> Result<Self, ApertureError> {
        let client = reqwest::Client::builder()
            .dns_resolver(Arc::new(ApertureResolver::new(policy)))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("albedo-aperture/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| ApertureError::Transport(err.to_string()))?;
        Ok(Self { client })
    }
}

fn header_string(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, request: &WireRequest) -> Result<WireResponse, ApertureError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|err| ApertureError::Transport(err.to_string()))?;
        let mut builder = self.client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }

        let response = builder
            .send()
            .await
            .map_err(|err| ApertureError::Transport(err.to_string()))?;

        let status = response.status().as_u16();
        let etag = header_string(&response, "etag");
        let last_modified = header_string(&response, "last-modified");
        let content_type = header_string(&response, "content-type");

        // Names arrive lowercased from `http::HeaderMap` already; `to_str`
        // drops any value that is not visible ASCII rather than lossily
        // reconstructing one, because a header a workflow body reads back
        // mangled is worse than one it does not find.
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect();

        // A 304 carries no body by definition; asking for one is a wasted
        // allocation on the path whose whole point is that it is nearly free.
        let body = if status == 304 {
            Vec::new()
        } else {
            response
                .bytes()
                .await
                .map_err(|err| ApertureError::Transport(err.to_string()))?
                .to_vec()
        };

        Ok(WireResponse {
            status,
            body,
            headers,
            etag,
            last_modified,
            content_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aperture::egress::EgressMode;

    #[test]
    fn a_transport_builds_for_both_modes() {
        assert!(ReqwestTransport::new(Arc::new(EgressPolicy::new(EgressMode::Serve))).is_ok());
        assert!(ReqwestTransport::new(Arc::new(EgressPolicy::new(EgressMode::Dev))).is_ok());
    }

    #[tokio::test]
    async fn the_resolver_refuses_a_name_that_resolves_to_loopback() {
        // `localhost` is the one name every machine resolves to a denied
        // address, so this exercises the real resolver path end to end without
        // needing a network or a controlled DNS server.
        let resolver = ApertureResolver::new(Arc::new(EgressPolicy::new(EgressMode::Serve)));
        let name: Name = "localhost".parse().expect("valid DNS name");
        assert!(
            resolver.resolve(name).await.is_err(),
            "serve must refuse loopback"
        );
    }

    #[tokio::test]
    async fn the_resolver_permits_loopback_in_dev_and_for_declared_hosts() {
        let dev = ApertureResolver::new(Arc::new(EgressPolicy::new(EgressMode::Dev)));
        let name: Name = "localhost".parse().expect("valid DNS name");
        assert!(dev.resolve(name).await.is_ok(), "dev is permissive");

        let declared = ApertureResolver::new(Arc::new(EgressPolicy::with_declared_hosts(
            EgressMode::Serve,
            ["localhost"],
        )));
        let name: Name = "localhost".parse().expect("valid DNS name");
        assert!(
            declared.resolve(name).await.is_ok(),
            "a declaration is the authority the allowlist carries"
        );
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn the_resolver_is_shareable_across_the_client_pool() {
        assert_send_sync::<ApertureResolver>();
        assert_send_sync::<ReqwestTransport>();
    }
}
