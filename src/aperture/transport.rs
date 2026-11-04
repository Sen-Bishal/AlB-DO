//! APERTURE · A0 — the `reqwest`-backed [`Transport`], and the resolver that
//! makes the egress policy un-bypassable.
//!
//! Separated from `client.rs` so that everything above this file is free of
//! `reqwest` types: the cache, the coalescer and the metrics are all testable
//! against `CountingTransport` with no
//! network, no TLS and no DNS.

use crate::aperture::client::{ApertureError, Transport, WireRequest, WireResponse};
use crate::aperture::egress::EgressPolicy;
use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::sync::Arc;

/// Maximum body buffered for one outbound response (8 MiB). The cache and
/// QuickJS limits apply after transport buffering, so neither bounds this
/// Rust allocation. Applies to declared sources, fetches and provider calls.
pub const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

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

/// Lower a failed send, recovering an egress refusal from the error chain.
///
/// 🪤 **The resolver's refusal arrives wrapped.** [`ApertureResolver`] returns an
/// [`EgressDenial`](crate::aperture::egress::EgressDenial), `reqwest` wraps it as
/// a connect error, and that error's `Display` is only *"error sending request
/// for url (…)"* — the cause lives in `source()`. Lowered with `to_string()`, a
/// named host that resolves into a denied range read as a **transport failure**:
/// the exact misreport `workflow::resolve_one` names the `Egress` variant to
/// prevent, reached anyway because only IP-literal URLs ever produced that
/// variant. Found by a middleware test whose refusal came back looking like a
/// dead port.
fn send_error(err: reqwest::Error) -> ApertureError {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&err);
    while let Some(current) = cause {
        if let Some(denial) = current.downcast_ref::<crate::aperture::egress::EgressDenial>() {
            return ApertureError::Egress(denial.clone());
        }
        cause = current.source();
    }
    ApertureError::Transport(err.to_string())
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

        let mut response = builder.send().await.map_err(send_error)?;

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
            // Reject a known oversize before allocating. Still count streamed
            // bytes: chunked/EOF bodies have no length, and headers are not a
            // substitute for enforcing the bound while receiving the body.
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
            {
                return Err(ApertureError::ResponseTooLarge {
                    limit: MAX_RESPONSE_BODY_BYTES,
                });
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| ApertureError::Transport(err.to_string()))?
            {
                if chunk.len() > MAX_RESPONSE_BODY_BYTES - body.len() {
                    return Err(ApertureError::ResponseTooLarge {
                        limit: MAX_RESPONSE_BODY_BYTES,
                    });
                }
                body.extend_from_slice(&chunk);
            }
            body
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

    /// The whole path, not the resolver alone: a denial raised inside DNS
    /// resolution must reach the caller as `Egress`, never as `Transport`.
    #[tokio::test]
    async fn a_resolver_refusal_surfaces_as_egress_not_as_a_transport_failure() {
        let transport =
            ReqwestTransport::new(Arc::new(EgressPolicy::new(EgressMode::Serve))).expect("builds");
        let err = transport
            .send(&WireRequest {
                method: "GET".to_string(),
                url: "http://localhost:9/".to_string(),
                headers: Vec::new(),
                body: None,
            })
            .await
            .expect_err("serve refuses loopback");
        assert!(
            matches!(err, ApertureError::Egress(_)),
            "a policy decision must not read as a network fault: {err}"
        );
    }

    // Exercise the real HTTP body reader with a local server. A small private
    // client avoids machine-specific proxies; egress itself is tested above.
    async fn local_response(head: String, body: Vec<u8>) -> Result<WireResponse, ApertureError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap() == 0 {
                    return;
                }
                request.push(byte[0]);
            }
            let _ = socket.write_all(head.as_bytes()).await;
            // Early refusal can close the socket before the fixture finishes.
            let _ = socket.write_all(&body).await;
        });
        let transport = ReqwestTransport {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            transport.send(&WireRequest {
                method: "GET".into(),
                url: format!("http://{address}/"),
                headers: Vec::new(),
                body: None,
            }),
        )
        .await
        .expect("local response reader hung");
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn oversized_declared_response_is_refused_before_reading() {
        let error = local_response(
            "HTTP/1.1 200 OK\r\nContent-Length: 8388609\r\nConnection: close\r\n\r\n".into(),
            Vec::new(),
        )
        .await
        .err()
        .expect("oversized response was accepted");
        assert!(
            error.to_string().contains("response body exceeds"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn oversized_chunked_response_is_refused() {
        let bytes = vec![b'x'; 8 * 1024 * 1024 + 1];
        let mut body = format!("{:x}\r\n", bytes.len()).into_bytes();
        body.extend(bytes);
        body.extend(b"\r\n0\r\n\r\n");
        let error = local_response(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".into(),
            body,
        )
        .await
        .err()
        .expect("oversized response was accepted");
        assert!(
            error.to_string().contains("response body exceeds"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn oversized_response_without_a_length_is_refused() {
        let error = local_response(
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".into(),
            vec![b'x'; 8 * 1024 * 1024 + 1],
        )
        .await
        .err()
        .expect("oversized response was accepted");
        assert!(
            error.to_string().contains("response body exceeds"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_response_at_the_limit_is_preserved() {
        let body = vec![b'x'; 8 * 1024 * 1024];
        let response = local_response(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ),
            body.clone(),
        )
        .await
        .unwrap();
        assert_eq!(response.body, body);
    }

    #[tokio::test]
    async fn not_modified_does_not_read_or_reject_a_representation_length() {
        let response = local_response(
            "HTTP/1.1 304 Not Modified\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n"
                .into(),
            Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(response.status, 304);
        assert!(response.body.is_empty());
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn the_resolver_is_shareable_across_the_client_pool() {
        assert_send_sync::<ApertureResolver>();
        assert_send_sync::<ReqwestTransport>();
    }
}
