//! Serving HTTPS from the process itself.
//!
//! `axum::serve(listener, …)` speaks plain HTTP, so putting an ALBEDO app on a
//! domain has meant putting a reverse proxy in front of it — which is one of
//! the five operational pieces this stack exists to not need. Worse, it is a
//! piece you cannot skip: browsers refuse to store the session cookie over
//! plain HTTP (it is issued `Secure`, with the `__Host-` prefix), so an app
//! served on a domain without TLS **appears to work and cannot log anyone in**,
//! with no error on either side. See [`crate::tls::insecure_auth_refusal`].
//!
//! # What this module decides
//!
//! Only *whether and how* to wrap the listener. The accept loop lives in
//! `server.rs` beside the plain one, because the two must share graceful
//! shutdown and the `ConnectInfo` that SHUTTER keys its buckets on — and a
//! second copy of that wiring is how the TLS path ends up subtly different
//! from the one everybody tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// How the listener should be wrapped, once configuration has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsMode {
    /// Plain HTTP. The default, and what every existing deployment does.
    Disabled,
    /// HTTPS from a certificate and key already on disk — an internal CA, a
    /// Cloudflare origin certificate, or one certbot renews out of band.
    Files { cert: PathBuf, key: PathBuf },
    /// HTTPS with certificates this process obtains and renews itself, from
    /// Let's Encrypt over TLS-ALPN-01.
    ///
    /// TLS-ALPN-01 completes *inside the TLS handshake on the HTTPS port*, so
    /// unlike HTTP-01 it needs no port 80 and no proxy — which is the whole
    /// reason it is the challenge this uses.
    Acme {
        /// Every name the certificate should cover. DNS for all of them must
        /// already point at this machine.
        domains: Vec<String>,
        /// Optional account contact. Let's Encrypt stopped sending expiry mail
        /// in 2025, so this is genuinely optional and **not** a refusal.
        contact: Option<String>,
        /// Where the account key and issued certificates are kept.
        ///
        /// 🔴 Losing this directory means re-issuing from scratch, and Let's
        /// Encrypt allows only 5 duplicate certificates per week. A deployment
        /// that restarts often with a non-persistent cache gets locked out for
        /// days — which is why this has a real default rather than a temp dir.
        cache: PathBuf,
        /// Use the staging CA: untrusted certificates, but effectively no rate
        /// limit. The only way to rehearse a real deployment without spending
        /// the week's production quota.
        staging: bool,
    },
}

impl TlsMode {
    /// Is this deployment serving HTTPS?
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// Environment variable naming the PEM certificate chain to serve.
///
/// **Deliberately the environment, like [`crate::shutter::TRUSTED_PROXIES_ENV`]
/// and for the same reason.** Whether there is a certificate, and where it
/// lives, is a property of *where the process runs* — a laptop has none, a
/// container has one at a mounted path, a platform terminates TLS in front and
/// wants none again. A value that must differ per environment does not belong
/// in a file that is identical in every environment.
pub const TLS_CERT_ENV: &str = "ALBEDO_TLS_CERT";

/// Environment variable naming the PEM private key. See [`TLS_CERT_ENV`].
pub const TLS_KEY_ENV: &str = "ALBEDO_TLS_KEY";

/// Comma-separated domains to obtain a certificate for. `--domain` overrides.
pub const ACME_DOMAIN_ENV: &str = "ALBEDO_ACME_DOMAIN";

/// Optional Let's Encrypt account contact. `--acme-contact` overrides.
pub const ACME_CONTACT_ENV: &str = "ALBEDO_ACME_CONTACT";

/// Where the ACME account key and certificates are cached.
/// `--acme-cache` overrides. See [`TlsMode::Acme::cache`].
pub const ACME_CACHE_ENV: &str = "ALBEDO_ACME_CACHE";

/// Set to use the Let's Encrypt **staging** CA. `--acme-staging` overrides.
pub const ACME_STAGING_ENV: &str = "ALBEDO_ACME_STAGING";

/// The origin a browser will actually use to reach this server.
///
/// 🔑 **Exists because the bind address is not a usable signal in a container.**
/// [`insecure_auth_refusal`] wants to know one thing — will the browser store
/// the `__Host-` session cookie? — and answers it from the bind host, which is
/// a good proxy on a bare box (`127.0.0.1` is local-only, `0.0.0.0` is reachable
/// over a real hostname) and **carries no information at all inside a
/// container**, where binding `0.0.0.0` is mandatory and says nothing about how
/// anyone reaches you. `-p 127.0.0.1:3000:3000`, an ingress terminating TLS, and
/// a port open to the internet are indistinguishable from in there.
///
/// So the operator states the origin instead. It is strictly more informative
/// than every heuristic in this module, and it is checkable — unlike a boolean,
/// which anyone can set once and carry into production unnoticed.
pub const PUBLIC_ORIGIN_ENV: &str = "ALBEDO_PUBLIC_ORIGIN";

/// Default cache directory, relative to the project.
pub const DEFAULT_ACME_CACHE_DIR: &str = ".albedo-acme";

/// The `tls` block of the server config.
///
/// Every field is a **flag with an environment fallback**: the flag is what
/// the documentation and `--help` show, the variable is what a container or a
/// systemd unit sets. [`TlsSettings::from_env`] fills the fallbacks; the CLI
/// overwrites whatever it was given explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TlsSettings {
    /// PEM certificate chain, leaf first.
    #[serde(default)]
    pub cert_path: Option<String>,
    /// PEM private key — PKCS#8, PKCS#1 or SEC1.
    #[serde(default)]
    pub key_path: Option<String>,
    /// Domains to obtain a Let's Encrypt certificate for.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Optional ACME account contact.
    #[serde(default)]
    pub acme_contact: Option<String>,
    /// Where to persist the ACME account and certificates.
    #[serde(default)]
    pub acme_cache: Option<String>,
    /// Use the staging CA.
    #[serde(default)]
    pub acme_staging: bool,
}

impl TlsSettings {
    /// Read every TLS variable from the real environment.
    ///
    /// Split from [`resolve`] so the precedence rules stay testable without
    /// mutating process-global state — a test that sets an environment
    /// variable passes alone and fails in a suite.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            cert_path: std::env::var(TLS_CERT_ENV).ok(),
            key_path: std::env::var(TLS_KEY_ENV).ok(),
            domains: std::env::var(ACME_DOMAIN_ENV)
                .ok()
                .map(|raw| split_domains(&raw))
                .unwrap_or_default(),
            acme_contact: std::env::var(ACME_CONTACT_ENV).ok(),
            acme_cache: std::env::var(ACME_CACHE_ENV).ok(),
            acme_staging: std::env::var(ACME_STAGING_ENV)
                .ok()
                .is_some_and(|raw| is_truthy(&raw)),
        }
    }

    /// Whether anything was configured at all.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        let blank = |value: &Option<String>| {
            value.as_deref().map(str::trim).unwrap_or_default().is_empty()
        };
        blank(&self.cert_path) && blank(&self.key_path) && self.domains.is_empty()
    }
}

/// Split a comma-separated domain list, dropping blanks.
#[must_use]
pub fn split_domains(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// Read an environment flag the way an operator expects.
///
/// 🪤 A bare `is_ok()` on the variable would make `ALBEDO_ACME_STAGING=0` and
/// `=false` *enable* staging — the operator writes the thing that means "off"
/// and gets the opposite, then wonders why the browser distrusts the site.
fn is_truthy(raw: &str) -> bool {
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Decide the mode, resolving relative paths against the project.
///
/// # Errors
/// A message naming what is missing. Half a TLS configuration is always a
/// mistake and never a preference: silently falling back to plain HTTP would
/// serve the site on the wrong scheme with no indication, which for an app
/// with a login is indistinguishable from the auth being broken.
pub fn resolve(settings: &TlsSettings, project_root: &Path) -> Result<TlsMode, String> {
    let cert = settings
        .cert_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let key = settings
        .key_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let domains: Vec<String> = settings
        .domains
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();

    // 🔴 Two sources of certificate is not a preference to resolve, it is a
    // question with no right answer: serving the file would silently ignore
    // `--domain`, and serving ACME would silently ignore a certificate the
    // operator deliberately supplied. Either way the site comes up presenting
    // something other than what was asked for.
    if !domains.is_empty() && (cert.is_some() || key.is_some()) {
        return Err(format!(
            "both a certificate file and `--domain` were given ({}). Use one: `--tls-cert`/\
             `--tls-key` to serve a certificate you already have, or `--domain` to have one \
             obtained and renewed automatically.",
            domains.join(", ")
        ));
    }

    if !domains.is_empty() {
        for domain in &domains {
            validate_domain(domain)?;
        }
        return Ok(TlsMode::Acme {
            contact: settings
                .acme_contact
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            cache: settings
                .acme_cache
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || project_root.join(DEFAULT_ACME_CACHE_DIR),
                    |value| resolve_against(value, project_root),
                ),
            staging: settings.acme_staging,
            domains,
        });
    }

    match (cert, key) {
        (None, None) => Ok(TlsMode::Disabled),
        (Some(_), None) => Err(
            "a TLS certificate was configured without a private key. Set `--tls-key` (or \
             ALBEDO_TLS_KEY) to the PEM key that goes with it."
                .to_string(),
        ),
        (None, Some(_)) => Err(
            "a TLS private key was configured without a certificate. Set `--tls-cert` (or \
             ALBEDO_TLS_CERT) to the PEM chain that goes with it."
                .to_string(),
        ),
        (Some(cert), Some(key)) => Ok(TlsMode::Files {
            cert: resolve_against(cert, project_root),
            key: resolve_against(key, project_root),
        }),
    }
}

/// Refuse a `--domain` value that Let's Encrypt could never issue for.
///
/// 🔑 Checked **before** the listener binds, because the alternative is finding
/// out from the CA. A rejected order still counts against the failed-validation
/// rate limit, and the error comes back as an ACME problem document rather than
/// anything an operator can act on. A typo caught here costs nothing.
fn validate_domain(domain: &str) -> Result<(), String> {
    let reason = if domain.contains("://") || domain.contains('/') {
        Some("a URL, not a hostname — drop the scheme and any path")
    } else if domain.contains(':') {
        Some("carrying a port — a certificate covers a name, not a port")
    } else if domain.parse::<std::net::IpAddr>().is_ok() {
        Some("an IP address — Let's Encrypt issues for DNS names only")
    // 🪤 Before the has-a-dot check, or `localhost` is reported as merely "not
    // fully qualified" and the operator never sees the advice that actually
    // helps — that no public CA can certify it and local work wants plain HTTP.
    } else if domain.eq_ignore_ascii_case("localhost") {
        Some("localhost — no public CA can validate it; bind 127.0.0.1 and serve plain HTTP for local work")
    } else if !domain.contains('.') {
        Some("not a fully-qualified domain name")
    } else if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        Some("not a well-formed hostname")
    } else if !domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '*')
    {
        Some("carrying characters a hostname cannot contain")
    } else {
        None
    };

    match reason {
        None => Ok(()),
        Some(why) => Err(format!(
            "`{domain}` cannot be certified: it is {why}. DNS for every `--domain` must already \
             resolve to this machine before the certificate can be issued."
        )),
    }
}

/// Relative paths belong to the project, not to whatever shell started it —
/// the same rule as `forge.db`, for the same reason.
fn resolve_against(value: &str, project_root: &Path) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

/// Build a rustls server configuration from a PEM chain and key on disk.
///
/// # Errors
/// A message naming the offending file. Note in particular that a file
/// containing **no** certificates is an error rather than an empty chain: an
/// empty chain builds a server that fails every handshake with a protocol
/// error the browser reports as an unhelpful connection failure, which is the
/// silent-failure shape this codebase keeps closing.
pub fn server_config_from_files(
    cert: &Path,
    key: &Path,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let cert_pem = std::fs::read(cert)
        .map_err(|err| format!("failed to read TLS certificate '{}': {err}", cert.display()))?;
    let key_pem = std::fs::read(key)
        .map_err(|err| format!("failed to read TLS private key '{}': {err}", key.display()))?;

    let chain = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("TLS certificate '{}' is not valid PEM: {err}", cert.display()))?;
    if chain.is_empty() {
        return Err(format!(
            "TLS certificate '{}' contains no CERTIFICATE block. A chain has to hold at least the \
             leaf certificate, or every handshake fails with no usable diagnostic.",
            cert.display()
        ));
    }

    let private_key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|err| format!("TLS private key '{}' is not valid PEM: {err}", key.display()))?
        .ok_or_else(|| {
            format!(
                "TLS private key '{}' contains no PRIVATE KEY block (PKCS#8, PKCS#1 and SEC1 are \
                 all accepted).",
                key.display()
            )
        })?;

    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, private_key)
        .map_err(|err| {
            format!(
                "TLS certificate '{}' and key '{}' do not form a usable pair: {err}",
                cert.display(),
                key.display()
            )
        })?;

    // HTTP/2 first, then HTTP/1.1. Without ALPN advertised here a browser that
    // offered h2 falls back to HTTP/1.1 silently, which works but throws away
    // multiplexing on exactly the streaming responses this server specialises
    // in.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Build a rustls configuration whose certificates this process obtains and
/// renews itself.
///
/// Returns the config **and** the state future that does the work: ACME is not
/// a one-shot call but a stream that must be polled for the lifetime of the
/// server — it performs the initial order, answers challenges, and renews
/// weeks later. Returning it rather than spawning here keeps the caller in
/// charge of the task's lifetime, and makes it impossible to forget the half
/// that matters: a config with nobody driving the state serves a handshake
/// that never completes.
///
/// # Errors
/// A cache directory that cannot be created. Issuance failures are *not*
/// errors here — they happen later, on the state stream, because the listener
/// must already be bound for TLS-ALPN-01 to be answerable at all.
pub fn acme_config(
    domains: &[String],
    contact: Option<&str>,
    cache: &Path,
    staging: bool,
) -> Result<(Arc<rustls::ServerConfig>, AcmeDriver), String> {
    std::fs::create_dir_all(cache).map_err(|err| {
        format!(
            "failed to create the ACME cache directory '{}': {err}. Certificates and the account \
             key live here; without it every restart would re-issue and Let's Encrypt allows only \
             5 duplicate certificates per week.",
            cache.display()
        )
    })?;

    let mut config = rustls_acme::AcmeConfig::new(domains)
        .cache(rustls_acme::caches::DirCache::new(cache.to_path_buf()))
        .directory_lets_encrypt(!staging);

    if let Some(contact) = contact {
        config = config.contact_push(format!("mailto:{contact}"));
    }

    let state = config.state();
    let resolver = state.resolver();

    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);

    // 🔑 `acme-tls/1` is the whole mechanism. The CA opens a TLS connection
    // negotiating exactly this protocol, and the resolver answers it with a
    // throwaway challenge certificate instead of the site's. Omit it and
    // validation can never succeed — on the same port, with no other symptom
    // than an order that stays pending.
    server_config.alpn_protocols = vec![
        rustls_acme::acme::ACME_TLS_ALPN_NAME.to_vec(),
        b"h2".to_vec(),
        b"http/1.1".to_vec(),
    ];

    Ok((Arc::new(server_config), AcmeDriver { state }))
}

/// The half of ACME that has to keep running.
///
/// Held separately so a caller cannot take the `ServerConfig` and drop this:
/// the config alone answers handshakes with a resolver that has no certificate
/// and never will.
pub struct AcmeDriver {
    state: rustls_acme::AcmeState<std::io::Error>,
}

impl AcmeDriver {
    /// Drive issuance and renewal until the process ends.
    ///
    /// Each event is logged rather than propagated: a failed order must be
    /// visible and must be retried, not fatal. DNS that has not propagated
    /// yet is the ordinary first-boot case, and killing the server for it
    /// would turn a wait into an outage.
    pub async fn run(mut self) {
        use futures_util::StreamExt;
        loop {
            match self.state.next().await {
                Some(Ok(ok)) => tracing::info!(target: "albedo.tls.acme", event = ?ok, "ACME"),
                Some(Err(err)) => tracing::error!(
                    target: "albedo.tls.acme",
                    error = %err,
                    "ACME order failed — the certificate will be retried; check that DNS for \
                     every --domain resolves to this machine and that port 443 is reachable"
                ),
                None => break,
            }
        }
    }
}

/// The refusal for an app that has login configured but is serving plain HTTP.
///
/// 🔑 **This is the silent failure TLS is really about.** The session cookie is
/// issued with the `__Host-` prefix, which *requires* the `Secure` attribute,
/// which browsers only honour over HTTPS. Serve a login on a domain over plain
/// HTTP and the browser accepts the response, discards the cookie, and the next
/// request arrives anonymous — so signing in appears to do nothing, forever,
/// and neither the server log nor the browser console says why.
///
/// Loopback is exempt because browsers treat `http://localhost` as a secure
/// context, which is what makes `albedo dev` work without certificates.
///
/// Returns `None` when there is nothing to refuse.
#[must_use]
pub fn insecure_auth_refusal(
    tls: &TlsMode,
    has_auth_providers: bool,
    has_trusted_proxies: bool,
    host: &str,
    public_origin: Option<&str>,
) -> Option<String> {
    if tls.is_enabled() || !has_auth_providers {
        return None;
    }

    // 🔑 **A declared origin is authoritative, in both directions.** Every other
    // branch here infers the browser's view from the server's bind address;
    // this one is told it. So it decides — including when it says *no*, which
    // catches `http://app.example.com` even on a loopback bind behind a
    // forwarder, a misconfiguration the bind-address heuristic cannot see.
    if let Some(origin) = public_origin.map(str::trim).filter(|o| !o.is_empty()) {
        return match origin_stores_secure_cookies(origin) {
            Some(true) => None,
            Some(false) => Some(format!(
                "{PUBLIC_ORIGIN_ENV} is `{origin}`, which is plain HTTP on a host browsers do \
                 not treat as a secure context — so the `__Host-` session cookie will not be \
                 stored and signing in will silently do nothing. Serve that origin over HTTPS \
                 (terminate TLS in front, or set {TLS_CERT_ENV} and {TLS_KEY_ENV}), or point \
                 {PUBLIC_ORIGIN_ENV} at the origin users really use."
            )),
            None => Some(format!(
                "{PUBLIC_ORIGIN_ENV} is `{origin}`, which is not an origin this can read. Use a \
                 scheme and host, like `https://app.example.com` or `http://localhost:3000`."
            )),
        };
    }
    // 🔑 **Plain HTTP behind a TLS-terminating proxy is correct, not broken.**
    // The browser's connection is HTTPS all the way to the proxy, so the
    // `Secure` cookie is stored exactly as intended; only the private hop
    // between proxy and process is plain. Refusing that would break the most
    // common production deployment there is, so a declared proxy is the
    // operator saying "TLS is handled in front" and is taken at its word —
    // the same declaration SHUTTER already requires to believe a forwarded
    // address.
    if has_trusted_proxies {
        return None;
    }
    if is_loopback_host(host) {
        return None;
    }

    // 🪤 Name the env vars, not the config fields: `albedo serve` reads
    // `TlsSettings::from_env`, so telling an operator to edit `tls.cert_path`
    // sends them to a setting this path never looks at. A remedy that does not
    // work is worse than no remedy.
    Some(format!(
        "this app configures authentication but is about to serve plain HTTP on {host}. Session \
         cookies are issued with the `__Host-` prefix, so browsers will refuse to store them over \
         HTTP and signing in will silently do nothing — with no error anywhere. Any one of \
         these fixes it:\n  · say how users reach you: {PUBLIC_ORIGIN_ENV}=https://app.example.com \
         (or http://localhost:3000 for local work)\n  · serve HTTPS here: {TLS_CERT_ENV} and \
         {TLS_KEY_ENV}, a PEM chain and key\n  · terminate TLS in a proxy in front and name it: \
         {}=<its address or CIDR>\n  · bind 127.0.0.1 for local work — browsers treat localhost \
         as a secure context.\n\nIn a container the first one is the answer: binding 0.0.0.0 is \
         mandatory there, so it tells this check nothing about how anyone reaches you.",
        crate::shutter::TRUSTED_PROXIES_ENV
    ))
}

/// Will a browser store a `Secure` cookie from this origin?
///
/// `None` when the value is not an origin this can read — which is refused
/// rather than ignored, because a typo that silently disarms the check is the
/// failure this whole module exists to prevent.
///
/// 🪤 `http://localhost` is `true`, not a special case being lenient: browsers
/// define localhost as a *secure context* and do store `Secure` cookies from
/// it. That is the same rule [`is_loopback_host`] relies on, stated once more
/// precisely — which is why this can replace it rather than sit beside it.
#[must_use]
pub fn origin_stores_secure_cookies(origin: &str) -> Option<bool> {
    let origin = origin.trim();
    if let Some(rest) = origin.strip_prefix("https://") {
        return (!rest.is_empty()).then_some(true);
    }

    let rest = origin.strip_prefix("http://")?;
    // Authority is everything before the first `/`, `?` or `#`.
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return None;
    }
    // Drop userinfo, then the port — taking care that an IPv6 literal is
    // bracketed and full of the same colon we are splitting on.
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(end) = host_port.strip_prefix('[').and_then(|r| r.find(']')) {
        &host_port[1..=end]
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };

    Some(is_loopback_host(host))
}

/// Is this bind host one browsers treat as a secure context?
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(addr) => addr.is_loopback(),
        // 🪤 `0.0.0.0` parses and is **not** loopback: binding every interface
        // is exactly the case that reaches a browser over a real hostname, so
        // it must not be exempt.
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔑 The case the bind address can never answer: a container binds
    /// `0.0.0.0` because it must, and that says nothing about how anyone
    /// reaches it. A stated origin does.
    #[test]
    fn a_declared_https_origin_lets_a_container_boot_on_all_interfaces() {
        assert_eq!(
            insecure_auth_refusal(
                &TlsMode::Disabled,
                true,
                false,
                "0.0.0.0",
                Some("https://app.example.com"),
            ),
            None
        );
    }

    /// Local docker testing: `-p 3000:3000` reached at `http://localhost:3000`.
    /// Browsers define localhost as a secure context and DO store `Secure`
    /// cookies from it, so this is correct rather than lenient.
    #[test]
    fn a_declared_localhost_origin_is_a_secure_context() {
        for origin in [
            "http://localhost:3000",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
            "http://localhost",
        ] {
            assert_eq!(
                insecure_auth_refusal(&TlsMode::Disabled, true, false, "0.0.0.0", Some(origin)),
                None,
                "{origin} is a secure context"
            );
        }
    }

    /// 🪤 The declared origin is authoritative in BOTH directions — this is a
    /// case the old bind-address check could not see at all: bound to loopback
    /// (so "exempt"), but actually reached over plain HTTP on a real hostname
    /// through a forwarder. The cookie is silently dropped.
    #[test]
    fn a_declared_plain_http_origin_is_refused_even_on_a_loopback_bind() {
        let refusal = insecure_auth_refusal(
            &TlsMode::Disabled,
            true,
            false,
            "127.0.0.1",
            Some("http://app.example.com"),
        )
        .expect("must refuse");
        assert!(refusal.contains("app.example.com"), "{refusal}");
    }

    /// A typo must not silently disarm the check — that is the failure this
    /// module exists to prevent, arriving by a different door.
    #[test]
    fn an_unreadable_origin_is_refused_rather_than_ignored() {
        for bad in ["app.example.com", "ftp://app.example.com", "https://", "http://"] {
            assert!(
                insecure_auth_refusal(&TlsMode::Disabled, true, false, "0.0.0.0", Some(bad))
                    .is_some(),
                "{bad} should be refused, not treated as satisfied"
            );
        }
    }

    /// An empty or whitespace value is "unset", not "unreadable" — an env var
    /// declared and left blank is the shape a compose file or a k8s manifest
    /// produces, and it should fall through to the other rules rather than
    /// fail with a parse complaint.
    #[test]
    fn a_blank_origin_falls_through_to_the_other_rules() {
        assert_eq!(
            insecure_auth_refusal(&TlsMode::Disabled, true, false, "127.0.0.1", Some("   ")),
            None,
            "blank should fall through, and a loopback bind is exempt"
        );
        assert!(
            insecure_auth_refusal(&TlsMode::Disabled, true, false, "0.0.0.0", Some(""))
                .is_some(),
            "blank should fall through, and 0.0.0.0 without TLS is refused"
        );
    }

    #[test]
    fn origin_parsing_handles_ports_paths_and_ipv6() {
        assert_eq!(origin_stores_secure_cookies("https://a.example.com"), Some(true));
        assert_eq!(origin_stores_secure_cookies("http://localhost:3000/app"), Some(true));
        assert_eq!(origin_stores_secure_cookies("http://[::1]:8080"), Some(true));
        assert_eq!(origin_stores_secure_cookies("http://127.0.0.1"), Some(true));
        assert_eq!(origin_stores_secure_cookies("http://example.com:3000"), Some(false));
        // 🪤 A hostname that merely *contains* "localhost" is not localhost.
        assert_eq!(origin_stores_secure_cookies("http://localhost.evil.com"), Some(false));
        assert_eq!(origin_stores_secure_cookies("nonsense"), None);
    }

    const TEST_CERT: &str = include_str!("../tests/fixtures/tls/localhost-cert.pem");
    const TEST_KEY: &str = include_str!("../tests/fixtures/tls/localhost-key.pem");
    const UNRELATED_KEY: &str = include_str!("../tests/fixtures/tls/unrelated-key.pem");

    fn write_pair(dir: &Path) -> (PathBuf, PathBuf) {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, TEST_CERT).expect("write cert");
        std::fs::write(&key, TEST_KEY).expect("write key");
        (cert, key)
    }

    #[test]
    fn an_unconfigured_server_stays_on_plain_http() {
        assert_eq!(
            resolve(&TlsSettings::default(), Path::new("/srv/app")),
            Ok(TlsMode::Disabled)
        );
    }

    /// Half a configuration is a mistake, never a preference. Falling back to
    /// HTTP would serve the wrong scheme with nothing said.
    #[test]
    fn a_certificate_without_a_key_is_refused_by_name() {
        let settings = TlsSettings {
            cert_path: Some("cert.pem".into()),
            key_path: None,
            ..TlsSettings::default()
        };
        let err = resolve(&settings, Path::new("/srv/app")).unwrap_err();
        assert!(err.contains("--tls-key"), "the remedy must name the flag: {err}");
    }

    #[test]
    fn a_key_without_a_certificate_is_refused_by_name() {
        let settings = TlsSettings {
            cert_path: None,
            key_path: Some("key.pem".into()),
            ..TlsSettings::default()
        };
        let err = resolve(&settings, Path::new("/srv/app")).unwrap_err();
        assert!(err.contains("--tls-cert"), "the remedy must name the flag: {err}");
    }

    /// Same rule as `forge.db`: a relative path belongs to the project, not to
    /// the shell that happened to start the process.
    #[test]
    fn relative_paths_resolve_against_the_project_not_the_cwd() {
        let settings = TlsSettings {
            cert_path: Some("certs/app.pem".into()),
            key_path: Some("certs/app.key".into()),
            ..TlsSettings::default()
        };
        assert_eq!(
            resolve(&settings, Path::new("/srv/app")),
            Ok(TlsMode::Files {
                cert: PathBuf::from("/srv/app").join("certs/app.pem"),
                key: PathBuf::from("/srv/app").join("certs/app.key"),
            })
        );
    }

    #[test]
    fn a_real_pem_pair_builds_a_server_configuration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert, key) = write_pair(dir.path());
        let config = server_config_from_files(&cert, &key).expect("a valid pair loads");
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            "h2 must be advertised or every browser silently drops to HTTP/1.1"
        );
    }

    /// 🪤 `rustls_pemfile::certs` returns an **empty vector** for a file with no
    /// CERTIFICATE block rather than an error. An empty chain builds a server
    /// that fails every handshake, which reaches the user as an unexplained
    /// connection reset.
    #[test]
    fn a_certificate_file_with_no_certificate_in_it_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert, key) = write_pair(dir.path());
        std::fs::write(&cert, "# nothing here\n").expect("overwrite cert");
        let err = server_config_from_files(&cert, &key).unwrap_err();
        assert!(err.contains("no CERTIFICATE block"), "{err}");
    }

    #[test]
    fn a_missing_file_is_refused_by_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, key) = write_pair(dir.path());
        let missing = dir.path().join("absent.pem");
        let err = server_config_from_files(&missing, &key).unwrap_err();
        assert!(err.contains("absent.pem"), "{err}");
    }

    /// A **valid** key that belongs to a different certificate — copying the
    /// wrong file out of a certs directory, which is the realistic operator
    /// error. Rejecting garbage proves nothing here; the pair has to be
    /// individually well-formed and still refused, or the mismatch surfaces
    /// only as a handshake failure once real traffic arrives.
    #[test]
    fn a_valid_key_belonging_to_a_different_certificate_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cert, key) = write_pair(dir.path());
        std::fs::write(&key, UNRELATED_KEY).expect("overwrite key");

        // Both halves parse on their own; only together are they wrong.
        assert!(
            rustls_pemfile::private_key(&mut UNRELATED_KEY.as_bytes())
                .expect("the unrelated key is well-formed PEM")
                .is_some(),
            "the fixture must be a real key, or this test degrades to the garbage case"
        );
        assert!(
            server_config_from_files(&cert, &key).is_err(),
            "a mismatched pair must not build a server that fails at handshake time"
        );
    }

    /// 🔑 The whole reason TLS is not optional for an app with a login.
    #[test]
    fn an_app_with_auth_on_a_public_host_refuses_to_serve_plain_http() {
        let refusal = insecure_auth_refusal(&TlsMode::Disabled, true, false, "0.0.0.0", None)
            .expect("this is the silent-login failure");
        assert!(refusal.contains("__Host-"), "{refusal}");
    }

    /// 🪤 `0.0.0.0` parses as an address and is **not** loopback. Treating any
    /// parseable address as local would exempt exactly the bind that reaches a
    /// real browser over a real hostname.
    #[test]
    fn binding_every_interface_is_not_treated_as_local() {
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
    }

    /// `albedo dev` must keep working with no certificates: browsers treat
    /// `http://localhost` as a secure context, so the cookie is stored.
    #[test]
    fn local_development_over_http_is_not_refused() {
        assert_eq!(
            insecure_auth_refusal(&TlsMode::Disabled, true, false, "127.0.0.1", None),
            None
        );
    }

    /// An app with no login has nothing to lose to a missing cookie.
    #[test]
    fn an_app_without_auth_is_free_to_serve_plain_http() {
        assert_eq!(
            insecure_auth_refusal(&TlsMode::Disabled, false, false, "0.0.0.0", None),
            None
        );
    }

    /// And once HTTPS is on, there is nothing to refuse.
    #[test]
    fn serving_https_removes_the_refusal() {
        let mode = TlsMode::Files {
            cert: PathBuf::from("c"),
            key: PathBuf::from("k"),
        };
        assert_eq!(insecure_auth_refusal(&mode, true, false, "0.0.0.0", None), None);
    }

    /// 🔑 The most common production shape there is: TLS terminated in a proxy,
    /// plain HTTP on the private hop behind it. The browser's connection is
    /// HTTPS, so the `Secure` cookie is stored — refusing this would break a
    /// correct deployment, which is a worse failure than the one being guarded.
    #[test]
    fn plain_http_behind_a_declared_proxy_is_allowed() {
        assert_eq!(
            insecure_auth_refusal(&TlsMode::Disabled, true, true, "0.0.0.0", None),
            None
        );
    }

    fn acme(domains: &[&str]) -> TlsSettings {
        TlsSettings {
            domains: domains.iter().map(|d| (*d).to_string()).collect(),
            ..TlsSettings::default()
        }
    }

    /// The sentence the whole push is aimed at: one domain, nothing else.
    #[test]
    fn a_domain_alone_selects_automatic_certificates() {
        let mode = resolve(&acme(&["app.example.com"]), Path::new("/srv/app")).expect("resolves");
        match mode {
            TlsMode::Acme { domains, cache, contact, staging } => {
                assert_eq!(domains, vec!["app.example.com".to_string()]);
                assert_eq!(cache, PathBuf::from("/srv/app").join(DEFAULT_ACME_CACHE_DIR));
                assert_eq!(contact, None, "Let's Encrypt made contact optional in 2025");
                assert!(!staging);
            }
            other => panic!("expected ACME, got {other:?}"),
        }
    }

    /// 🔴 Two certificate sources is a question with no right answer: one of
    /// them would be silently ignored and the site would present something
    /// other than what was asked for.
    #[test]
    fn a_certificate_file_and_a_domain_together_are_refused() {
        let settings = TlsSettings {
            cert_path: Some("cert.pem".into()),
            key_path: Some("key.pem".into()),
            domains: vec!["app.example.com".into()],
            ..TlsSettings::default()
        };
        let err = resolve(&settings, Path::new("/srv/app")).unwrap_err();
        assert!(err.contains("app.example.com"), "{err}");
        assert!(err.contains("Use one"), "{err}");
    }

    /// 🔑 The cache must persist or Let's Encrypt's 5-duplicates-per-week
    /// limit locks the deployment out for days. Defaulting into the project
    /// rather than a temp directory is what makes a restart free.
    #[test]
    fn the_certificate_cache_defaults_into_the_project() {
        let mode = resolve(&acme(&["app.example.com"]), Path::new("/srv/app")).expect("resolves");
        let TlsMode::Acme { cache, .. } = mode else {
            panic!("expected ACME")
        };
        assert!(
            cache.starts_with("/srv/app"),
            "a cache outside the project would be lost on restart: {}",
            cache.display()
        );
    }

    /// Several names on one certificate is ordinary — apex plus www.
    #[test]
    fn several_domains_share_one_certificate() {
        let mode = resolve(&acme(&["example.com", "www.example.com"]), Path::new("/srv/app"))
            .expect("resolves");
        let TlsMode::Acme { domains, .. } = mode else {
            panic!("expected ACME")
        };
        assert_eq!(domains.len(), 2);
    }

    /// A comma-separated environment value is one variable, many names.
    #[test]
    fn the_environment_form_splits_on_commas() {
        assert_eq!(
            split_domains(" example.com , www.example.com ,"),
            vec!["example.com".to_string(), "www.example.com".to_string()]
        );
    }

    /// 🪤 `ALBEDO_ACME_STAGING=0` must mean off. A bare presence check would
    /// turn the value that means "off" into "on", and the operator would find
    /// out from the browser distrusting their site.
    #[test]
    fn a_falsy_staging_value_means_off() {
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
        assert!(is_truthy("1"));
        assert!(is_truthy("true"));
        assert!(is_truthy("YES"));
    }

    /// 🔑 Caught before binding, because the alternative is learning it from
    /// the CA — and a failed order still spends the failed-validation quota.
    #[test]
    fn a_domain_no_ca_could_issue_for_is_refused_before_binding() {
        for bad in [
            "https://app.example.com",
            "app.example.com:8443",
            "192.0.2.10",
            "localhost",
            "notadomain",
            "app..example.com",
            "app example.com",
        ] {
            assert!(
                resolve(&acme(&[bad]), Path::new("/srv/app")).is_err(),
                "`{bad}` should be refused before the listener binds"
            );
        }
    }

    /// A wildcard is well-formed. It needs DNS-01 rather than TLS-ALPN-01 to
    /// actually issue, which is the CA's problem to report — but the shape is
    /// not a typo and must not be rejected as one.
    #[test]
    fn a_wildcard_is_well_formed_even_though_alpn_cannot_issue_it() {
        assert!(resolve(&acme(&["*.example.com"]), Path::new("/srv/app")).is_ok());
    }

    /// ACME is HTTPS, so the plain-HTTP auth refusal must not fire.
    #[test]
    fn automatic_certificates_satisfy_the_auth_over_http_refusal() {
        let mode = TlsMode::Acme {
            domains: vec!["app.example.com".into()],
            contact: None,
            cache: PathBuf::from("/srv/app/.albedo-acme"),
            staging: false,
        };
        assert!(mode.is_enabled());
        assert_eq!(insecure_auth_refusal(&mode, true, false, "0.0.0.0", None), None);
    }
}
