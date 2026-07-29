//! APERTURE · A0 — egress policy.
//!
//! Implements `development-plan/APERTURE.md` invariant 2.7 (*"a declared
//! source's `base` is its egress allowlist"*) and the egress row of § 8.
//!
//! ## Where the check lives, and why it matters
//!
//! The naive implementation resolves the hostname, inspects the addresses,
//! decides, and then hands the *hostname* to the HTTP client — which resolves
//! it again. Between those two resolutions the answer can change, so a DNS
//! server that returns a public address on the first query and
//! `169.254.169.254` on the second walks straight through the check. That is
//! DNS rebinding, and it is the standard bypass for exactly this control.
//!
//! So the check is not performed *before* resolution: it **is** resolution.
//! [`ApertureResolver`] implements [`reqwest::dns::Resolve`], which is the only
//! place the client learns an address, and it filters there. There is no
//! second lookup and therefore no window.
//!
//! ## Two layers, because the resolver sees only a hostname
//!
//! A resolver is handed a bare name — no scheme, no port, no path. So the
//! policy splits:
//!
//! * [`EgressPolicy::check_url`] runs on the request path, where the full URL exists: it enforces
//!   the scheme and decides whether the host is covered by a declaration.
//! * [`ApertureResolver`] enforces address classes, and is bypassed for hosts a declaration already
//!   vouched for.
//!
//! ## Declared hosts bypass the address denies, deliberately
//!
//! `base: "http://payments.internal"` is a legitimate thing to declare, and an
//! internal service mesh is a normal deployment. The denies exist to stop an
//! *undeclared* URL — a bare `fetch()` over a string the author assembled at
//! runtime — from reaching link-local metadata or an RFC1918 neighbour. A
//! declaration is an author stating intent at build time, which is precisely
//! the authority the allowlist is meant to carry.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use url::Url;

/// Which deployment the policy is enforcing for.
///
/// `albedo dev` is permissive because a developer's machine legitimately talks
/// to `localhost` — a mock API, a neighbouring container, a tunnel. `serve` is
/// not, because the paywall is hosted compute and a tenant's `fetch()` must not
/// be able to read the instance metadata endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressMode {
    /// Address-class denies are not enforced. Scheme rules still are.
    Dev,
    /// Address-class denies are enforced for every non-declared host.
    Serve,
}

/// The class that caused an address to be refused.
///
/// Carried in the denial so the error message can name the actual reason
/// rather than a generic refusal — invariant 2.8, *fail closed, loudly*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    /// `127.0.0.0/8`, `::1` — the server itself.
    Loopback,
    /// `169.254.0.0/16`, `fe80::/10`. **The cloud metadata endpoint lives
    /// here**; this is the single most important row in the enum.
    LinkLocal,
    /// `10/8`, `172.16/12`, `192.168/16`, `fc00::/7` — the neighbours.
    Private,
    /// `100.64.0.0/10` — carrier-grade NAT space.
    SharedAddress,
    /// `0.0.0.0`, `::` — unspecified.
    Unspecified,
    /// Multicast in either family.
    Multicast,
    /// `255.255.255.255`.
    Broadcast,
    /// `198.18.0.0/15` — benchmarking range.
    Benchmarking,
    /// Documentation ranges (`192.0.2.0/24`, `2001:db8::/32`, …).
    Documentation,
}

impl AddressClass {
    /// A short human-readable name for the error message.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            AddressClass::Loopback => "loopback",
            AddressClass::LinkLocal => "link-local (cloud metadata range)",
            AddressClass::Private => "private",
            AddressClass::SharedAddress => "carrier-grade NAT",
            AddressClass::Unspecified => "unspecified",
            AddressClass::Multicast => "multicast",
            AddressClass::Broadcast => "broadcast",
            AddressClass::Benchmarking => "benchmarking",
            AddressClass::Documentation => "documentation",
        }
    }
}

/// Why a URL or address was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressDenial {
    /// Scheme was not `http` or `https`. Blocks `file:`, `data:`, `gopher:`
    /// and every other scheme a URL parser will happily accept.
    Scheme {
        /// The offending scheme.
        scheme: String,
    },
    /// The URL carried no host at all.
    NoHost,
    /// The host resolved to an address in a refused class.
    Address {
        /// The hostname as written.
        host: String,
        /// The address it resolved to.
        addr: IpAddr,
        /// Why that address is refused.
        class: AddressClass,
    },
}

impl std::fmt::Display for EgressDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressDenial::Scheme { scheme } => write!(
                f,
                "egress refused: scheme `{scheme}` is not permitted (only http and https)"
            ),
            EgressDenial::NoHost => write!(f, "egress refused: URL has no host"),
            EgressDenial::Address { host, addr, class } => write!(
                f,
                "egress refused: `{host}` resolves to {addr}, which is {}. \
                 Declare the host as a source `base` in albedo.config.ts if this is intended.",
                class.label()
            ),
        }
    }
}

impl std::error::Error for EgressDenial {}

/// Classify an address, returning `None` when it is ordinary public space.
///
/// ## Order is load-bearing
///
/// v6-native classes are tested **before** any embedded v4 address is
/// unwrapped, and reversing the two is a silent hole. `::1` satisfies
/// `to_ipv4()` — the first 96 bits are zero, so it is "IPv4-compatible" — and
/// unwrapping it first yields `0.0.0.1`, which is in no special v4 range at
/// all. Loopback would launder itself into ordinary public space. The first
/// draft of this function had exactly that bug and
/// `ipv6_local_ranges_are_classified` is the test that found it.
///
/// Unwrapping afterwards is still required: `http://[::ffff:169.254.169.254]/`
/// reaches the metadata endpoint through a v6 literal that no v4 check would
/// otherwise inspect. `to_ipv4` rather than `to_ipv4_mapped` so the deprecated
/// compatible form (`::169.254.169.254`) is caught too — by this point the
/// v6-native classes are already excluded, so the broader conversion can only
/// add denials, never remove one.
#[must_use]
pub fn classify(addr: IpAddr) -> Option<AddressClass> {
    match addr {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6).or_else(|| v6.to_ipv4().and_then(classify_v4)),
    }
}

fn classify_v4(addr: Ipv4Addr) -> Option<AddressClass> {
    let [a, b, ..] = addr.octets();
    if addr.is_loopback() {
        return Some(AddressClass::Loopback);
    }
    if addr.is_link_local() {
        return Some(AddressClass::LinkLocal);
    }
    if addr.is_private() {
        return Some(AddressClass::Private);
    }
    if addr.is_unspecified() {
        return Some(AddressClass::Unspecified);
    }
    if addr.is_broadcast() {
        return Some(AddressClass::Broadcast);
    }
    if addr.is_multicast() {
        return Some(AddressClass::Multicast);
    }
    // 100.64.0.0/10 — `Ipv4Addr::is_shared` is still unstable, so it is
    // spelled out rather than waiting for it.
    if a == 100 && (b & 0b1100_0000) == 64 {
        return Some(AddressClass::SharedAddress);
    }
    // 198.18.0.0/15
    if a == 198 && (b & 0b1111_1110) == 18 {
        return Some(AddressClass::Benchmarking);
    }
    if addr.is_documentation() {
        return Some(AddressClass::Documentation);
    }
    None
}

fn classify_v6(addr: Ipv6Addr) -> Option<AddressClass> {
    if addr.is_loopback() {
        return Some(AddressClass::Loopback);
    }
    if addr.is_unspecified() {
        return Some(AddressClass::Unspecified);
    }
    if addr.is_multicast() {
        return Some(AddressClass::Multicast);
    }
    let segments = addr.segments();
    // fe80::/10 — `is_unicast_link_local` is unstable.
    if (segments[0] & 0xffc0) == 0xfe80 {
        return Some(AddressClass::LinkLocal);
    }
    // fc00::/7 unique local — the v6 analogue of RFC1918.
    if (segments[0] & 0xfe00) == 0xfc00 {
        return Some(AddressClass::Private);
    }
    // 2001:db8::/32
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return Some(AddressClass::Documentation);
    }
    None
}

/// The egress decision for one deployment.
///
/// Cheap to clone-by-`Arc` and shared by the client and its resolver. Built
/// once at boot; `allow_hosts` is populated from declared source `base`s in
/// phase A1 and is empty in A0, where every call is therefore a bare `fetch()`
/// subject to the default denies.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    mode: EgressMode,
    allow_hosts: HashSet<String>,
}

impl EgressPolicy {
    /// A policy for `mode` with no declared hosts.
    #[must_use]
    pub fn new(mode: EgressMode) -> Self {
        Self {
            mode,
            allow_hosts: HashSet::new(),
        }
    }

    /// A policy whose declared hosts bypass the address-class denies.
    ///
    /// Hosts are lowercased on the way in so the resolver's lookup is a plain
    /// equality test against an already-normalised name.
    #[must_use]
    pub fn with_declared_hosts<I, S>(mode: EgressMode, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            mode,
            allow_hosts: hosts
                .into_iter()
                .map(|host| host.as_ref().to_ascii_lowercase())
                .collect(),
        }
    }

    /// The mode this policy enforces.
    #[must_use]
    pub fn mode(&self) -> EgressMode {
        self.mode
    }

    /// Whether `host` was declared, and therefore bypasses address denies.
    #[must_use]
    pub fn is_declared(&self, host: &str) -> bool {
        self.allow_hosts.contains(&host.to_ascii_lowercase())
    }

    /// Scheme and host checks, run on the request path where the full URL is
    /// still available.
    ///
    /// For a URL naming a **hostname**, address classes are not checked here —
    /// see the module docs; the resolver is where they belong, and this
    /// returning `Ok` does not mean the request will connect.
    ///
    /// For a URL naming an **IP literal** they are checked here, because
    /// otherwise nothing checks them at all. A resolver only runs when there is
    /// a name to resolve: `http://169.254.169.254/` gives `reqwest` an authority
    /// it can turn into a socket address directly, so [`ApertureResolver`] is
    /// never consulted and the entire address-class deny is skipped.
    ///
    /// [`ApertureResolver`]: crate::aperture::transport::ApertureResolver
    ///
    /// This was reachable only from a bare `fetch()` in an action body — the
    /// declared read path builds every URL from a `base`, and a `base` that is
    /// an IP literal is an author declaring exactly that address. So it became
    /// live with APERTURE A2's server seam, which is the change that first let
    /// userland name a URL. § 8: *the metadata-endpoint deny is not optional.*
    ///
    /// # Errors
    /// [`EgressDenial::Scheme`] for a non-HTTP scheme, [`EgressDenial::NoHost`]
    /// when the URL carries no host, [`EgressDenial::Address`] for an
    /// undeclared IP literal in a refused class.
    pub fn check_url(&self, url: &Url) -> Result<(), EgressDenial> {
        match url.scheme() {
            "http" | "https" => {}
            other => {
                return Err(EgressDenial::Scheme {
                    scheme: other.to_string(),
                })
            }
        }
        let Some(host) = url.host_str() else {
            return Err(EgressDenial::NoHost);
        };

        // `Url::host()` has already parsed the authority, so the literal case is
        // a match rather than a re-parse — and matching on the parsed form is
        // what makes this correct for the spellings a string test would miss:
        // `[::1]`, `[::ffff:127.0.0.1]`, and IPv4 in any of its accepted forms.
        match url.host() {
            Some(url::Host::Ipv4(addr)) => self.check_address(host, IpAddr::V4(addr)),
            Some(url::Host::Ipv6(addr)) => self.check_address(host, IpAddr::V6(addr)),
            _ => Ok(()),
        }
    }

    /// Whether a resolved address may be connected to on behalf of `host`.
    ///
    /// # Errors
    /// [`EgressDenial::Address`] when the address falls in a refused class and
    /// the host was not declared.
    pub fn check_address(&self, host: &str, addr: IpAddr) -> Result<(), EgressDenial> {
        if self.mode == EgressMode::Dev || self.is_declared(host) {
            return Ok(());
        }
        match classify(addr) {
            Some(class) => Err(EgressDenial::Address {
                host: host.to_string(),
                addr,
                class,
            }),
            None => Ok(()),
        }
    }

    /// Filter resolved addresses, keeping only those this policy permits.
    ///
    /// Returns the surviving addresses and the first denial seen, so a caller
    /// that ends up with nothing can report *why* rather than "no addresses".
    #[must_use]
    pub fn filter_addresses(
        &self,
        host: &str,
        addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> (Vec<SocketAddr>, Option<EgressDenial>) {
        let mut kept = Vec::new();
        let mut denial = None;
        for addr in addrs {
            match self.check_address(host, addr.ip()) {
                Ok(()) => kept.push(addr),
                Err(err) => {
                    if denial.is_none() {
                        denial = Some(err);
                    }
                }
            }
        }
        (kept, denial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn metadata_endpoint_is_link_local() {
        assert_eq!(
            classify(v4("169.254.169.254")),
            Some(AddressClass::LinkLocal)
        );
    }

    #[test]
    fn ipv4_mapped_v6_does_not_bypass_the_v4_checks() {
        // The bypass this test exists for: `http://[::ffff:169.254.169.254]/`.
        let mapped: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
        assert_eq!(classify(mapped), Some(AddressClass::LinkLocal));

        let mapped_private: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert_eq!(classify(mapped_private), Some(AddressClass::Private));
    }

    #[test]
    fn private_and_loopback_ranges_are_classified() {
        assert_eq!(classify(v4("127.0.0.1")), Some(AddressClass::Loopback));
        assert_eq!(classify(v4("10.1.2.3")), Some(AddressClass::Private));
        assert_eq!(classify(v4("172.16.0.1")), Some(AddressClass::Private));
        assert_eq!(classify(v4("192.168.1.1")), Some(AddressClass::Private));
        assert_eq!(
            classify(v4("100.64.0.1")),
            Some(AddressClass::SharedAddress)
        );
        assert_eq!(classify(v4("0.0.0.0")), Some(AddressClass::Unspecified));
    }

    #[test]
    fn ipv6_local_ranges_are_classified() {
        // `::1` is the regression guard: it is *also* an IPv4-compatible
        // address, so unwrapping before classifying turns it into `0.0.0.1`
        // and loses the loopback verdict entirely.
        assert_eq!(classify(v4("::1")), Some(AddressClass::Loopback));
        assert_eq!(classify(v4("::")), Some(AddressClass::Unspecified));
        assert_eq!(classify(v4("fe80::1")), Some(AddressClass::LinkLocal));
        assert_eq!(classify(v4("fd00::1")), Some(AddressClass::Private));
    }

    #[test]
    fn the_deprecated_ipv4_compatible_form_is_still_unwrapped() {
        // `::169.254.169.254` — same metadata endpoint, older spelling.
        let compatible: IpAddr = "::169.254.169.254".parse().unwrap();
        assert_eq!(classify(compatible), Some(AddressClass::LinkLocal));
    }

    #[test]
    fn public_addresses_are_not_classified() {
        assert_eq!(classify(v4("93.184.216.34")), None);
        assert_eq!(classify(v4("2606:2800:220:1:248:1893:25c8:1946")), None);
    }

    /// **The bypass this check exists for.**
    ///
    /// A resolver only runs when there is a name to resolve, so every
    /// address-class deny in this module was unreachable for a URL that named an
    /// address directly. `classify` was right the whole time and nothing ever
    /// asked it. Found by driving a real `albedo serve` — the unit tests all
    /// passed, because each one called `check_address` itself.
    #[test]
    fn an_ip_literal_url_is_classified_by_check_url_because_no_resolver_will_see_it() {
        let policy = EgressPolicy::new(EgressMode::Serve);

        for refused in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:4599/charge",
            "http://10.0.0.5/internal",
            "http://192.168.1.1/",
            "http://[::1]:8080/",
            "http://[fd00::1]/",
            // The mapped and compatible spellings of the metadata endpoint,
            // which `classify` already handled and nothing routed to it.
            "http://[::ffff:169.254.169.254]/",
        ] {
            let url = Url::parse(refused).expect("parses");
            assert!(
                matches!(
                    policy.check_url(&url),
                    Err(EgressDenial::Address { .. })
                ),
                "serve must refuse {refused}"
            );
        }

        // A public literal is fine — the rule is the address class, not the
        // spelling.
        assert!(policy
            .check_url(&Url::parse("http://93.184.216.34/").unwrap())
            .is_ok());
    }

    #[test]
    fn a_hostname_url_is_still_left_to_the_resolver() {
        // The split the module docs describe has to survive the fix: checking a
        // *name* here would mean resolving it here, and resolving it twice is
        // the DNS-rebinding window `ApertureResolver` exists to close.
        let policy = EgressPolicy::new(EgressMode::Serve);
        assert!(policy
            .check_url(&Url::parse("http://localhost:4599/charge").unwrap())
            .is_ok());
    }

    #[test]
    fn a_declared_literal_and_dev_mode_both_still_pass() {
        // A `base` that names an address is an author declaring that address,
        // which is the same authority a declared hostname carries.
        let declared =
            EgressPolicy::with_declared_hosts(EgressMode::Serve, ["127.0.0.1"]);
        assert!(declared
            .check_url(&Url::parse("http://127.0.0.1:4599/charge").unwrap())
            .is_ok());
        assert!(
            matches!(
                declared.check_url(&Url::parse("http://10.0.0.5/").unwrap()),
                Err(EgressDenial::Address { .. })
            ),
            "declaring one address must not vouch for another"
        );

        let dev = EgressPolicy::new(EgressMode::Dev);
        assert!(dev
            .check_url(&Url::parse("http://127.0.0.1:4599/charge").unwrap())
            .is_ok());
    }

    #[test]
    fn non_http_schemes_are_refused() {
        let policy = EgressPolicy::new(EgressMode::Serve);
        let url = Url::parse("file:///etc/passwd").unwrap();
        assert!(matches!(
            policy.check_url(&url),
            Err(EgressDenial::Scheme { .. })
        ));
    }

    #[test]
    fn serve_refuses_private_addresses_but_dev_permits_them() {
        let serve = EgressPolicy::new(EgressMode::Serve);
        assert!(serve
            .check_address("evil.test", v4("169.254.169.254"))
            .is_err());

        let dev = EgressPolicy::new(EgressMode::Dev);
        assert!(dev.check_address("localhost", v4("127.0.0.1")).is_ok());
    }

    #[test]
    fn a_declared_host_bypasses_the_address_denies() {
        // `base: "http://payments.internal"` — an author stating intent at
        // build time is the authority the allowlist carries.
        let policy = EgressPolicy::with_declared_hosts(EgressMode::Serve, ["payments.internal"]);
        assert!(policy
            .check_address("payments.internal", v4("10.0.0.9"))
            .is_ok());
        // …and only for that host.
        assert!(policy
            .check_address("other.internal", v4("10.0.0.9"))
            .is_err());
    }

    #[test]
    fn declared_host_matching_is_case_insensitive() {
        let policy = EgressPolicy::with_declared_hosts(EgressMode::Serve, ["API.Example.COM"]);
        assert!(policy.is_declared("api.example.com"));
    }

    #[test]
    fn filtering_keeps_public_addresses_and_reports_the_first_denial() {
        let policy = EgressPolicy::new(EgressMode::Serve);
        let addrs = vec![
            SocketAddr::new(v4("10.0.0.1"), 443),
            SocketAddr::new(v4("93.184.216.34"), 443),
        ];
        let (kept, denial) = policy.filter_addresses("mixed.test", addrs);
        assert_eq!(kept.len(), 1);
        assert!(matches!(denial, Some(EgressDenial::Address { .. })));
    }
}
