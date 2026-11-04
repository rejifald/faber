//! Where the mirror may fetch from.
//!
//! The mirror is attached to every opted-in job's private network, so a request
//! it makes on a job's behalf must never land on something reachable only from
//! there: another job's services, the VM's own ports, a metadata endpoint. Every
//! download it follows is therefore checked, at the first hop and at each
//! redirect:
//!
//! - **https only**, on the default port, with no credentials in the URL;
//! - **the host is one the adapter's upstream names**, exactly: one of
//!   [`GITHUB_LFS_HOSTS`] for LFS, [`NPM_HOSTS`] for npm;
//! - **at most [`MAX_REDIRECTS`] redirects**, each re-checked;
//! - **the address it resolves to is public** ([`public`]): the resolver the
//!   HTTP client connects through ([`PublicOnly`]) drops loopback, private,
//!   link-local and other non-routable addresses, so an allowed name whose DNS
//!   points inward is refused, and the client connects only to an address that
//!   passed. There is no check-then-connect gap.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

/// The hosts GitHub serves LFS object downloads from.
///
/// Source: GitHub's docs list exactly these two as "Needed for Git Large File
/// Storage" in the self-hosted runner allowlist
/// (`data/reusables/actions/runner-essential-communications.md` in
/// github/docs). Checked against the batch API on 2026-10-21: a download
/// action for a public repo's object pointed at
/// `https://github-cloud.githubusercontent.com/alambic/media/…`, served `200`
/// with no redirect. A host GitHub adds later is refused (the job's LFS fetch
/// fails, nothing worse) until it is added here.
pub const GITHUB_LFS_HOSTS: &[&str] = &[
    "github-cloud.githubusercontent.com",
    "github-cloud.s3.amazonaws.com",
];

/// The hosts the npm adapter fetches from: the public registry, and nothing
/// else (ADR-0047 §5). It serves packuments and tarballs itself, with no
/// redirect today; a redirect to any other host is refused.
pub const NPM_HOSTS: &[&str] = &["registry.npmjs.org"];

/// The most redirects one download may follow. GitHub's own needs none today.
pub const MAX_REDIRECTS: usize = 3;

/// The download policy: which URLs the mirror follows.
#[derive(Debug, Clone)]
pub struct Egress {
    hosts: Vec<String>,
    /// What the hosts are, for the refusal: "GitHub's LFS hosts".
    what: &'static str,
    https_only: bool,
    allow_private: bool,
}

impl Egress {
    /// The policy the service runs with: [`GITHUB_LFS_HOSTS`], https, public
    /// addresses only.
    pub fn github() -> Self {
        Self {
            hosts: GITHUB_LFS_HOSTS.iter().map(|h| h.to_string()).collect(),
            what: "GitHub's LFS hosts",
            https_only: true,
            allow_private: false,
        }
    }

    /// The npm adapter's policy: [`NPM_HOSTS`], https, public addresses only.
    pub fn npmjs() -> Self {
        Self {
            hosts: NPM_HOSTS.iter().map(|h| h.to_string()).collect(),
            what: "npm's registry hosts",
            https_only: true,
            allow_private: false,
        }
    }

    /// A policy for a test server on this machine: `hosts`, plain http allowed,
    /// and private addresses allowed only when `allow_private`. Never used by
    /// the service itself.
    pub fn for_test(hosts: &[&str], allow_private: bool) -> Self {
        Self {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            what: "the test's hosts",
            https_only: false,
            allow_private,
        }
    }

    /// Whether the client may follow `url`, or why not.
    pub fn check(&self, url: &str) -> Result<reqwest::Url, String> {
        let url = reqwest::Url::parse(url).map_err(|_| "not a URL".to_string())?;
        let scheme_ok = match url.scheme() {
            "https" => true,
            "http" => !self.https_only,
            _ => false,
        };
        if !scheme_ok {
            return Err(format!("{} is not https", url.scheme()));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("the link carries credentials".into());
        }
        if self.https_only && url.port().is_some_and(|p| p != 443) {
            return Err("the link names a port other than 443".into());
        }
        let host = match url.host() {
            Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
            Some(url::Host::Ipv4(ip)) => {
                self.public_or_allowed(IpAddr::V4(ip))?;
                ip.to_string()
            }
            Some(url::Host::Ipv6(ip)) => {
                self.public_or_allowed(IpAddr::V6(ip))?;
                ip.to_string()
            }
            None => return Err("the link has no host".into()),
        };
        if !self.hosts.contains(&host) {
            return Err(format!("{host} is not one of {}", self.what));
        }
        Ok(url)
    }

    fn public_or_allowed(&self, ip: IpAddr) -> Result<(), String> {
        if self.allow_private || public(ip) {
            Ok(())
        } else {
            Err(format!("{ip} is not a public address"))
        }
    }

    /// The resolver the HTTP client connects through.
    pub fn resolver(&self) -> Arc<PublicOnly> {
        Arc::new(PublicOnly {
            allow_private: self.allow_private,
        })
    }
}

/// Whether `ip` is an ordinary public address. Refused: unspecified, loopback,
/// private (RFC 1918), carrier-grade NAT, link-local, multicast, broadcast,
/// documentation and benchmarking ranges, `0.0.0.0/8`, `240.0.0.0/4`, IPv6
/// unique-local and site-local, and any IPv4-mapped or -compatible IPv6 address
/// whose IPv4 is one of those.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && c == 0))
}

fn public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    let seg = ip.segments();
    // ::a.b.c.d, the deprecated IPv4-compatible form.
    if seg[..6].iter().all(|s| *s == 0) && !ip.is_loopback() && !ip.is_unspecified() {
        return public_v4(Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        ));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // unique local
        || (seg[0] & 0xffc0) == 0xfe80 // link-local
        || (seg[0] & 0xffc0) == 0xfec0 // site-local
        || (seg[0] == 0x2001 && seg[1] == 0x0db8)) // documentation
}

/// A DNS resolver that hands the client public addresses only. An allowed host
/// whose name resolves only inward fails to connect, with the reason.
#[derive(Debug)]
pub struct PublicOnly {
    allow_private: bool,
}

#[derive(Debug)]
struct Inward(String);

impl std::fmt::Display for Inward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} resolves to no public address", self.0)
    }
}

impl std::error::Error for Inward {}

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| allow_private || public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(Box::new(Inward(host)) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_to_githubs_lfs_hosts_is_followed() {
        let e = Egress::github();
        assert!(e
            .check("https://github-cloud.githubusercontent.com/alambic/media/1/aa/bb/x?sig=1")
            .is_ok());
        assert!(e.check("https://GITHUB-CLOUD.S3.AMAZONAWS.COM/x").is_ok());
        for (bad, says) in [
            ("http://github-cloud.githubusercontent.com/x", "not https"),
            ("https://evil.example/x", "not one of GitHub's LFS hosts"),
            (
                "https://github-cloud.githubusercontent.com.evil.example/x",
                "not one of",
            ),
            ("https://githubusercontent.com/x", "not one of"),
            ("https://github-cloud.githubusercontent.com:8443/x", "port"),
            (
                "https://u:p@github-cloud.githubusercontent.com/x",
                "credentials",
            ),
            ("https://10.0.0.1/x", "not a public address"),
            ("https://[::1]/x", "not a public address"),
            (
                "https://169.254.169.254/latest/meta-data",
                "not a public address",
            ),
            ("file:///etc/passwd", "not https"),
            ("faber-mirror:7470", "not https"),
        ] {
            let err = e.check(bad).unwrap_err();
            assert!(err.contains(says), "{bad}: {err}");
        }
    }

    #[test]
    fn only_https_to_the_public_npm_registry_is_followed() {
        let e = Egress::npmjs();
        assert!(e
            .check("https://registry.npmjs.org/@scope/demo/-/demo-1.0.0.tgz")
            .is_ok());
        for (bad, says) in [
            ("http://registry.npmjs.org/demo", "not https"),
            (
                "https://registry.yarnpkg.com/demo",
                "not one of npm's registry hosts",
            ),
            ("https://registry.npmjs.org.evil.example/demo", "not one of"),
            ("https://npmjs.org/demo", "not one of"),
            ("https://github-cloud.githubusercontent.com/x", "not one of"),
            ("https://registry.npmjs.org:8443/demo", "port"),
            ("https://u:p@registry.npmjs.org/demo", "credentials"),
            ("https://127.0.0.1/demo", "not a public address"),
        ] {
            let err = e.check(bad).unwrap_err();
            assert!(err.contains(says), "{bad}: {err}");
        }
    }

    #[test]
    fn inward_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "198.18.0.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "::10.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "140.82.112.3",
            "185.199.108.133",
            "2606:50c0:8000::154",
            "::ffff:140.82.112.3",
        ] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }
}
