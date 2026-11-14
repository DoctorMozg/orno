//! Outbound-network guard shared by the agent policy gate and the
//! `WebFetch` redirect loop: IP classification, domain matching and the
//! DNS resolution path.

use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Whether an outbound request must never reach `ip`. Delegates to the
/// per-family predicates so callers holding an `IpAddr` need no `match`.
#[must_use]
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

/// Blocks every `IPv4` range that is not a routable public unicast
/// address: `0.0.0.0/8`, loopback, RFC 1918, link-local (cloud metadata
/// `169.254.169.254`), CGNAT `100.64.0.0/10`, `192.0.0.0/24`,
/// documentation, benchmarking `198.18.0.0/15`, multicast and the
/// reserved `240.0.0.0/4` block (which includes broadcast).
#[must_use]
pub fn is_blocked_ipv4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    a == 0
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || (a == 100 && (b & 0xc0) == 64)
        || (a == 192 && b == 0 && c == 0)
        || v4.is_documentation()
        || (a == 198 && (b & 0xfe) == 18)
        || v4.is_multicast()
        || a >= 240
}

/// Blocks every `IPv6` address that is non-public or that embeds a
/// blocked `IPv4` address: unspecified, loopback, multicast, link-local,
/// unique-local, documentation, NAT64 (`64:ff9b::/96`, `64:ff9b:1::/48`)
/// and IPv4-mapped, IPv4-compatible, 6to4 and Teredo forms.
#[must_use]
pub fn is_blocked_ipv6(v6: Ipv6Addr) -> bool {
    let s = v6.segments();
    let embedded_v4 =
        |hi: u16, lo: u16| is_blocked_ipv4(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo)));
    v6.is_unspecified()
        || v6.is_loopback()
        || v6.is_multicast()
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xfe00) == 0xfc00
        // 2001:db8::/32 documentation.
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        // 64:ff9b::/96 and 64:ff9b:1::/48 translate onto arbitrary v4 hosts.
        || (s[0] == 0x64 && s[1] == 0xff9b && (s[2] == 0 || s[2] == 1))
        || v6.to_ipv4_mapped().is_some_and(is_blocked_ipv4)
        || v6.to_ipv4().is_some_and(is_blocked_ipv4)
        // 6to4 2002::/16 carries the v4 address in bits 16..48.
        || (s[0] == 0x2002 && embedded_v4(s[1], s[2]))
        // Teredo 2001::/32: server v4 in the clear, client v4 bit-inverted.
        || (s[0] == 0x2001 && s[1] == 0 && (embedded_v4(s[2], s[3]) || embedded_v4(!s[6], !s[7])))
}

/// The IP address a URL's host literally spells, if any. Uses
/// `Url::domain` to tell names from IPs because `host_str` keeps the
/// brackets around an `IPv6` literal and would not parse.
#[must_use]
pub fn literal_ip(url: &reqwest::Url) -> Option<IpAddr> {
    if url.domain().is_some() {
        return None;
    }
    url.host_str()?.trim_matches(['[', ']']).parse().ok()
}

/// Whether `host` equals `domain` or is a subdomain of it, ignoring
/// ASCII case and one trailing root dot on either side. Matching is on a
/// label boundary, so `notevil.com` does not match `evil.com`.
#[must_use]
pub fn host_matches_domain(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.');
    let domain = domain.trim_end_matches('.');
    if host.eq_ignore_ascii_case(domain) {
        return true;
    }
    host.len() > domain.len()
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
        && host
            .get(host.len() - domain.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(domain))
}

/// Verdict of [`DomainRules::check`] for one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DomainVerdict {
    /// No allowlist is configured and the host is not blocked.
    Unrestricted,
    /// The host matched an entry of a non-empty allowlist.
    Allowlisted,
    /// The host matches `blocked_domains`.
    Blocked,
    /// An allowlist exists and the host matches none of it.
    NotAllowed,
}

/// Borrowed allow/block domain lists of the calling agent's policy.
/// `blocked` always wins over `allowed`; an empty `allowed` list means
/// no restriction.
#[derive(Debug, Clone, Copy, Default)]
pub struct DomainRules<'a> {
    /// Allowlist; empty means every non-blocked host is permitted.
    pub allowed: &'a [String],
    /// Blocklist, matched on subdomains too.
    pub blocked: &'a [String],
}

impl DomainRules<'_> {
    /// Classify `host` against both lists.
    #[must_use]
    pub fn check(&self, host: &str) -> DomainVerdict {
        if self.blocked.iter().any(|d| host_matches_domain(host, d)) {
            DomainVerdict::Blocked
        } else if self.allowed.is_empty() {
            DomainVerdict::Unrestricted
        } else if self.allowed.iter().any(|d| host_matches_domain(host, d)) {
            DomainVerdict::Allowlisted
        } else {
            DomainVerdict::NotAllowed
        }
    }
}

/// Upper bound on one DNS lookup. The blocking resolver thread cannot be
/// cancelled, but the caller stops waiting on it.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Name-to-address lookup, abstracted so tests can map hostnames to a
/// local server without real DNS.
#[async_trait]
pub trait HostResolver: Send + Sync + Debug {
    /// Resolve `host` to all of its A/AAAA records.
    async fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>>;
}

/// Resolver backed by the operating system, run on a blocking thread
/// because `tokio/net` is not enabled.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

#[async_trait]
impl HostResolver for SystemResolver {
    async fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
        use std::net::ToSocketAddrs;
        let target = format!("{host}:0");
        let join = tokio::task::spawn_blocking(move || {
            target
                .to_socket_addrs()
                .map(|addrs| addrs.map(|sa| sa.ip()).collect::<Vec<_>>())
        });
        match tokio::time::timeout(DNS_TIMEOUT, join).await {
            Ok(Ok(res)) => res,
            Ok(Err(join_err)) => Err(std::io::Error::other(join_err)),
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("DNS resolution timed out after {DNS_TIMEOUT:?}"),
            )),
        }
    }
}

/// `reqwest` DNS hook that drops blocked addresses at connect time, so
/// the address that is checked is the address that is dialed.
#[derive(Debug, Clone)]
pub struct GuardedDns {
    inner: Arc<dyn HostResolver>,
    is_blocked: fn(IpAddr) -> bool,
}

impl GuardedDns {
    /// Guard `inner` with the standard block list.
    #[must_use]
    pub fn new(inner: Arc<dyn HostResolver>) -> Self {
        Self {
            inner,
            is_blocked: is_blocked_ip,
        }
    }

    /// Guard `inner` with a custom predicate; lets tests dial loopback.
    #[cfg(test)]
    pub(crate) fn with_filter(
        inner: Arc<dyn HostResolver>,
        is_blocked: fn(IpAddr) -> bool,
    ) -> Self {
        Self { inner, is_blocked }
    }
}

impl Resolve for GuardedDns {
    fn resolve(&self, name: Name) -> Resolving {
        let inner = Arc::clone(&self.inner);
        let is_blocked = self.is_blocked;
        Box::pin(async move {
            let host = name.as_str();
            let addrs = inner.resolve(host).await?;
            let permitted: Vec<SocketAddr> = addrs
                .into_iter()
                .filter(|ip| !is_blocked(*ip))
                .map(|ip| SocketAddr::new(ip, 0))
                .collect();
            if permitted.is_empty() {
                tracing::warn!(net.host = %host, "every resolved address is in a blocked range");
                return Err(std::io::Error::other(format!(
                    "host `{host}` resolves only to blocked addresses"
                ))
                .into());
            }
            Ok(Box::new(permitted.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("0.0.0.0")]
    #[case("0.1.2.3")]
    #[case("127.0.0.1")]
    #[case("127.255.255.254")]
    #[case("10.0.0.1")]
    #[case("172.31.255.255")]
    #[case("192.168.0.1")]
    #[case("100.64.0.1")]
    #[case("100.127.255.255")]
    #[case("169.254.169.254")]
    #[case("192.0.0.1")]
    #[case("192.0.2.1")]
    #[case("198.51.100.1")]
    #[case("203.0.113.1")]
    #[case("198.18.0.1")]
    #[case("198.19.255.255")]
    #[case("224.0.0.1")]
    #[case("239.255.255.255")]
    #[case("240.0.0.1")]
    #[case("255.255.255.255")]
    #[case("::")]
    #[case("::1")]
    #[case("fe80::1")]
    #[case("fd00::1")]
    #[case("ff02::1")]
    #[case("2001:db8::1")]
    #[case("64:ff9b::7f00:1")]
    #[case("64:ff9b::808:808")]
    #[case("64:ff9b:1::1")]
    #[case("::ffff:127.0.0.1")]
    #[case("::ffff:192.168.1.1")]
    #[case("2002:7f00:1::")]
    #[case("2002:a9fe:a9fe::1")]
    #[case("2001:0:7f00:1::1")]
    #[case("2001:0:4136:e378:8000:63bf:80ff:fffe")]
    fn blocked_addresses(#[case] ip: &str) {
        let ip: IpAddr = ip.parse().expect("test literal must parse");
        assert!(is_blocked_ip(ip), "{ip} must be blocked");
    }

    #[rstest]
    #[case("8.8.8.8")]
    #[case("1.1.1.1")]
    #[case("93.184.216.34")]
    #[case("100.63.255.255")]
    #[case("100.128.0.1")]
    #[case("198.17.255.255")]
    #[case("198.20.0.1")]
    #[case("223.255.255.255")]
    #[case("2001:4860:4860::8888")]
    #[case("2606:4700:4700::1111")]
    #[case("2002:808:808::1")]
    #[case("2001:0:808:808::f7f7:f7f7")]
    fn public_addresses(#[case] ip: &str) {
        let ip: IpAddr = ip.parse().expect("test literal must parse");
        assert!(!is_blocked_ip(ip), "{ip} must stay reachable");
    }

    #[rstest]
    #[case("http://[::1]/", Some("::1"))]
    #[case("http://127.0.0.1:8080/", Some("127.0.0.1"))]
    #[case("http://0x7f.1/", Some("127.0.0.1"))]
    #[case("http://example.com/", None)]
    fn literal_ip_reads_bracketed_and_numeric_hosts(
        #[case] url: &str,
        #[case] expected: Option<&str>,
    ) {
        let url = reqwest::Url::parse(url).expect("test URL must parse");
        assert_eq!(literal_ip(&url), expected.map(|s| s.parse().unwrap()));
    }

    #[rstest]
    #[case("evil.com", true)]
    #[case("evil.com.", true)]
    #[case("EVIL.com", true)]
    #[case("sub.Evil.COM.", true)]
    #[case("a.b.evil.com", true)]
    #[case("notevil.com", false)]
    #[case("evil.com.au", false)]
    #[case("com", false)]
    fn domain_matching(#[case] host: &str, #[case] expected: bool) {
        assert_eq!(host_matches_domain(host, "evil.com"), expected, "{host}");
    }

    #[test]
    fn blocklist_wins_over_allowlist() {
        let both = vec!["evil.com".to_string()];
        let rules = DomainRules {
            allowed: &both,
            blocked: &both,
        };
        assert_eq!(rules.check("evil.com"), DomainVerdict::Blocked);
    }

    #[test]
    fn empty_allowlist_is_unrestricted_and_non_empty_one_confines() {
        let allowed = vec!["good.com".to_string()];
        assert_eq!(
            DomainRules::default().check("anything.test"),
            DomainVerdict::Unrestricted
        );
        let rules = DomainRules {
            allowed: &allowed,
            blocked: &[],
        };
        assert_eq!(rules.check("api.good.com"), DomainVerdict::Allowlisted);
        assert_eq!(rules.check("bad.com"), DomainVerdict::NotAllowed);
    }
}
