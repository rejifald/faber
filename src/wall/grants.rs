//! The operator's grants (ADR-0026, amended 2026-10-17): which resolvers
//! guests use, and which LAN or tailnet destinations a profile's jobs reach.
//!
//! Grants live in a root-owned `grants.toml` (`/etc/faber/grants.toml` on both
//! OSes), never in `config.toml` or the API, which may narrow a grant but never
//! define one (ADR-0025). The path is the caller's: nothing here names it.
//!
//! ```toml
//! [dns]
//! resolvers = ["1.1.1.1", "1.0.0.1"]   # optional; absent = this pair
//!
//! [[grant]]
//! profile = "macos-vm"
//! to = "10.0.0.5/32"
//! port = 5432
//! proto = "tcp"
//! ```
//!
//! A destination is an IPv4 network, a port and a protocol, never a hostname:
//! pf matches addresses, and a name that resolves somewhere else tomorrow
//! would quietly widen the wall. Everything is validated here, before it can
//! reach rule text, and a key this release does not know is refused rather
//! than skipped. An absent file is not this module's business: the caller
//! treats it as [`Grants::default`], the default resolvers and no grants.

use std::collections::BTreeSet;
use std::fmt;
use std::net::Ipv4Addr;

use serde::Deserialize;

use super::model::Ipv4Net;

/// Cloudflare's resolvers, the default pair: fast, and filtering nothing, so a
/// CI guest never hits a legitimate host blocked by a threat feed.
pub const DEFAULT_RESOLVERS: [Ipv4Addr; 2] = [Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(1, 0, 0, 1)];

/// A transport protocol a grant opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Proto {
    Tcp,
    Udp,
}

impl fmt::Display for Proto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        })
    }
}

/// One destination a profile's jobs may reach.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Grant {
    pub profile: String,
    pub to: Ipv4Net,
    pub port: u16,
    pub proto: Proto,
}

/// The parsed, validated grants file. Built only by [`Grants::parse`] or
/// [`Grants::default`], so every value in it has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grants {
    resolvers: Vec<Ipv4Addr>,
    grants: Vec<Grant>,
}

impl Default for Grants {
    /// No file: the default resolvers, and no access beyond the public
    /// internet.
    fn default() -> Self {
        Self {
            resolvers: DEFAULT_RESOLVERS.to_vec(),
            grants: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantsError {
    #[error("grants.toml does not parse: {0}")]
    Toml(String),
    #[error("[dns] resolvers is empty: guests would have no resolver")]
    NoResolvers,
    #[error("[dns] resolver \"{0}\" is not an IPv4 address")]
    ResolverShape(String),
    #[error("[dns] resolver {0} is not unicast")]
    ResolverNotUnicast(Ipv4Addr),
    #[error("[dns] resolver {0} is listed twice")]
    DuplicateResolver(Ipv4Addr),
    #[error("grant {index}: profile \"{name}\" is not lowercase letters, digits, '_' or '-'")]
    BadProfile { index: usize, name: String },
    #[error("grant {index}: \"{to}\" is not an IPv4 network: {why}")]
    BadDestination {
        index: usize,
        to: String,
        why: String,
    },
    #[error("grant {index}: port {port} is outside 1..=65535")]
    BadPort { index: usize, port: i64 },
    #[error("grant {index}: protocol \"{proto}\" is not tcp or udp")]
    BadProto { index: usize, proto: String },
    #[error("grant {index} repeats an earlier grant")]
    DuplicateGrant { index: usize },
    #[error("there is no such grant to remove")]
    NoSuchGrant,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    dns: Option<RawDns>,
    #[serde(default)]
    grant: Vec<RawGrant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDns {
    resolvers: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrant {
    profile: String,
    to: String,
    port: i64,
    proto: String,
}

impl Grants {
    /// Parse and validate a grants file's text. Grants are numbered from 1 in
    /// errors, in the order the file lists them.
    pub fn parse(text: &str) -> Result<Self, GrantsError> {
        let raw: RawFile = toml::from_str(text)
            .map_err(|e| GrantsError::Toml(e.to_string().lines().collect::<Vec<_>>().join(" ")))?;
        let resolvers = match raw.dns.and_then(|d| d.resolvers) {
            None => DEFAULT_RESOLVERS.to_vec(),
            Some(list) => parse_resolvers(&list)?,
        };
        let mut grants = Vec::new();
        let mut seen = BTreeSet::new();
        for (i, g) in raw.grant.into_iter().enumerate() {
            let index = i + 1;
            if !valid_profile_name(&g.profile) {
                return Err(GrantsError::BadProfile {
                    index,
                    name: g.profile.escape_debug().to_string(),
                });
            }
            let to: Ipv4Net = g.to.parse().map_err(|e: super::model::NetParseError| {
                GrantsError::BadDestination {
                    index,
                    to: g.to.escape_debug().to_string(),
                    why: e.to_string(),
                }
            })?;
            let port =
                u16::try_from(g.port)
                    .ok()
                    .filter(|&p| p != 0)
                    .ok_or(GrantsError::BadPort {
                        index,
                        port: g.port,
                    })?;
            let proto = match g.proto.as_str() {
                "tcp" => Proto::Tcp,
                "udp" => Proto::Udp,
                other => {
                    return Err(GrantsError::BadProto {
                        index,
                        proto: other.escape_debug().to_string(),
                    })
                }
            };
            let grant = Grant {
                profile: g.profile,
                to,
                port,
                proto,
            };
            if !seen.insert(grant.clone()) {
                return Err(GrantsError::DuplicateGrant { index });
            }
            grants.push(grant);
        }
        Ok(Self { resolvers, grants })
    }

    /// The resolvers, in the order the file lists them.
    pub fn resolvers(&self) -> &[Ipv4Addr] {
        &self.resolvers
    }

    /// The grants, in the order the file lists them.
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// These grants with `grant` appended, held to every rule [`Self::parse`]
    /// applies: a profile name of the one spelling, a port that is not 0, and
    /// no grant twice. Errors number it as the file would, after the others.
    pub fn with_grant(&self, grant: Grant) -> Result<Self, GrantsError> {
        let index = self.grants.len() + 1;
        if !valid_profile_name(&grant.profile) {
            return Err(GrantsError::BadProfile {
                index,
                name: grant.profile.escape_debug().to_string(),
            });
        }
        if grant.port == 0 {
            return Err(GrantsError::BadPort { index, port: 0 });
        }
        if self.grants.contains(&grant) {
            return Err(GrantsError::DuplicateGrant { index });
        }
        let mut next = self.clone();
        next.grants.push(grant);
        Ok(next)
    }

    /// These grants without `grant`, which must be one of them exactly.
    pub fn without_grant(&self, grant: &Grant) -> Result<Self, GrantsError> {
        let at = self
            .grants
            .iter()
            .position(|g| g == grant)
            .ok_or(GrantsError::NoSuchGrant)?;
        let mut next = self.clone();
        next.grants.remove(at);
        Ok(next)
    }

    /// These grants with only those `keep` accepts. Removing a grant can break
    /// no rule, so this cannot fail.
    pub fn retaining(&self, mut keep: impl FnMut(&Grant) -> bool) -> Self {
        let mut next = self.clone();
        next.grants.retain(|g| keep(g));
        next
    }

    /// These grants with `resolvers` in place of the list, held to the rules
    /// `[dns] resolvers` is: not empty, unicast, none twice.
    pub fn with_resolvers(&self, resolvers: &[Ipv4Addr]) -> Result<Self, GrantsError> {
        let mut next = self.clone();
        next.resolvers = check_resolvers(resolvers)?;
        Ok(next)
    }

    /// The file's text, which [`Self::parse`] reads back to an equal value.
    ///
    /// Deterministic: a header comment, `[dns] resolvers` only when the list is
    /// not the default pair in its order, then each grant in order. A
    /// destination is written with its prefix always (`10.0.0.5/32`), since
    /// that is the only form the parser accepts; [`Ipv4Net`]'s `Display` is
    /// pf's, which drops `/32`. Every value has been validated, so none needs
    /// quoting beyond its double quotes.
    pub fn to_toml(&self) -> String {
        let mut out = String::from(
            "# faber's grants (ADR-0026). Written by `sudo faber grant`: edit it with that,\n\
             # which checks every change and stores the rules it renders.\n",
        );
        if self.resolvers != DEFAULT_RESOLVERS {
            let list: Vec<String> = self.resolvers.iter().map(|r| format!("\"{r}\"")).collect();
            out.push_str(&format!("\n[dns]\nresolvers = [{}]\n", list.join(", ")));
        }
        for g in &self.grants {
            out.push_str(&format!(
                "\n[[grant]]\nprofile = \"{}\"\nto = \"{}\"\nport = {}\nproto = \"{}\"\n",
                g.profile,
                cidr(&g.to),
                g.port,
                g.proto
            ));
        }
        out
    }
}

/// `net` as `ADDRESS/PREFIX`, the prefix always written: the form a grants
/// file and the CLI take, unlike pf's printed form.
pub fn cidr(net: &Ipv4Net) -> String {
    format!("{}/{}", net.addr(), net.prefix())
}

fn parse_resolvers(list: &[String]) -> Result<Vec<Ipv4Addr>, GrantsError> {
    let mut addrs = Vec::new();
    for s in list {
        addrs.push(
            s.parse()
                .map_err(|_| GrantsError::ResolverShape(s.escape_debug().to_string()))?,
        );
    }
    check_resolvers(&addrs)
}

/// The rules for a resolver list, wherever it came from.
fn check_resolvers(list: &[Ipv4Addr]) -> Result<Vec<Ipv4Addr>, GrantsError> {
    if list.is_empty() {
        return Err(GrantsError::NoResolvers);
    }
    let mut out = Vec::new();
    for &addr in list {
        if !is_unicast(addr) {
            return Err(GrantsError::ResolverNotUnicast(addr));
        }
        if out.contains(&addr) {
            return Err(GrantsError::DuplicateResolver(addr));
        }
        out.push(addr);
    }
    Ok(out)
}

/// Not `0/8` ("this network"), loopback, multicast, `240/4` (reserved, and the
/// limited broadcast with it).
fn is_unicast(a: Ipv4Addr) -> bool {
    let first = a.octets()[0];
    first != 0 && first != 127 && first < 224
}

/// A profile name as grants and the network plan spell it:
/// `^[a-z0-9][a-z0-9_-]{0,62}$`. It reaches no rule text, but it is matched
/// exactly, so one spelling is allowed.
pub fn valid_profile_name(s: &str) -> bool {
    let b = s.as_bytes();
    let ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= 63
        && ok(&b[0])
        && b[1..].iter().all(|c| ok(c) || *c == b'_' || *c == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    const EXAMPLE: &str = r#"
[dns]
resolvers = ["1.1.1.1", "1.0.0.1"]

[[grant]]
profile = "macos-vm"
to = "10.0.0.5/32"
port = 5432
proto = "tcp"

[[grant]]
profile = "docker_1"
to = "100.64.0.0/10"
port = 53000
proto = "udp"
"#;

    #[test]
    fn the_example_parses() {
        let g = Grants::parse(EXAMPLE).unwrap();
        assert_eq!(g.resolvers(), &[ip("1.1.1.1"), ip("1.0.0.1")]);
        assert_eq!(
            g.grants()[0],
            Grant {
                profile: "macos-vm".into(),
                to: "10.0.0.5/32".parse().unwrap(),
                port: 5432,
                proto: Proto::Tcp,
            }
        );
        assert_eq!(g.grants()[1].proto, Proto::Udp);
    }

    #[test]
    fn absent_dns_means_the_default_pair() {
        for text in [
            "",
            "[dns]\n",
            "[[grant]]\nprofile = \"a\"\nto = \"10.0.0.0/8\"\nport = 1\nproto = \"tcp\"\n",
        ] {
            assert_eq!(Grants::parse(text).unwrap().resolvers(), &DEFAULT_RESOLVERS);
        }
        assert_eq!(Grants::default().resolvers(), &DEFAULT_RESOLVERS);
        assert!(Grants::default().grants().is_empty());
        assert_eq!(Grants::parse("").unwrap(), Grants::default());
    }

    #[test]
    fn the_resolver_list_can_be_replaced() {
        let g = Grants::parse("[dns]\nresolvers = [\"192.0.2.53\"]\n").unwrap();
        assert_eq!(g.resolvers(), &[ip("192.0.2.53")]);
    }

    #[test]
    fn bad_resolvers_are_refused() {
        let r = |list: &str| Grants::parse(&format!("[dns]\nresolvers = [{list}]\n"));
        assert_eq!(r(""), Err(GrantsError::NoResolvers));
        assert!(matches!(
            r("\"one.one.one.one\""),
            Err(GrantsError::ResolverShape(_))
        ));
        assert!(matches!(
            r("\"1.1.1.1/32\""),
            Err(GrantsError::ResolverShape(_))
        ));
        for bad in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.53",
            "224.0.0.251",
            "239.1.1.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert_eq!(
                r(&format!("\"{bad}\"")),
                Err(GrantsError::ResolverNotUnicast(ip(bad))),
                "{bad}"
            );
        }
        assert_eq!(
            r("\"1.1.1.1\", \"1.1.1.1\""),
            Err(GrantsError::DuplicateResolver(ip("1.1.1.1")))
        );
    }

    fn one(profile: &str, to: &str, port: &str, proto: &str) -> Result<Grants, GrantsError> {
        Grants::parse(&format!(
            "[[grant]]\nprofile = \"{profile}\"\nto = \"{to}\"\nport = {port}\nproto = \"{proto}\"\n"
        ))
    }

    #[test]
    fn each_grant_defect_is_its_own_error() {
        for bad in [
            "",
            "Macos",
            "-a",
            "_a",
            "a b",
            "a.b",
            "a\\\"",
            "a;",
            "é",
            &"a".repeat(64),
        ] {
            assert!(
                matches!(
                    one(bad, "10.0.0.5/32", "1", "tcp"),
                    Err(GrantsError::BadProfile { index: 1, .. })
                ),
                "{bad:?}"
            );
        }
        assert!(one(&"a".repeat(63), "10.0.0.5/32", "1", "tcp").is_ok());
        for bad in [
            "10.0.0.5",
            "10.0.0.5/24",
            "db.lan",
            "10.0.0.0/33",
            "::1/128",
        ] {
            assert!(
                matches!(
                    one("a", bad, "1", "tcp"),
                    Err(GrantsError::BadDestination { index: 1, .. })
                ),
                "{bad}"
            );
        }
        for bad in ["0", "65536", "-1"] {
            assert!(
                matches!(
                    one("a", "10.0.0.5/32", bad, "tcp"),
                    Err(GrantsError::BadPort { index: 1, .. })
                ),
                "{bad}"
            );
        }
        assert!(one("a", "10.0.0.5/32", "65535", "udp").is_ok());
        for bad in ["TCP", "icmp", "any", ""] {
            assert!(
                matches!(
                    one("a", "10.0.0.5/32", "1", bad),
                    Err(GrantsError::BadProto { index: 1, .. })
                ),
                "{bad}"
            );
        }
        let twice = "[[grant]]\nprofile = \"a\"\nto = \"10.0.0.5/32\"\nport = 1\nproto = \"tcp\"\n";
        assert_eq!(
            Grants::parse(&format!("{twice}{twice}")),
            Err(GrantsError::DuplicateGrant { index: 2 })
        );
        // The same destination for two profiles is two grants.
        let other = twice.replace("\"a\"", "\"b\"");
        assert!(Grants::parse(&format!("{twice}{other}")).is_ok());
    }

    #[test]
    fn unknown_keys_and_hostnames_are_refused() {
        for bad in [
            "[dns]\nresolvers = [\"1.1.1.1\"]\nfallback = true\n",
            "[[grant]]\nprofile = \"a\"\nto = \"10.0.0.5/32\"\nport = 1\nproto = \"tcp\"\nhost = \"db\"\n",
            "[grants]\n",
            "[[grant]]\nprofile = \"a\"\nport = 1\nproto = \"tcp\"\n",
            "[[grant]]\nprofile = \"a\"\nto = \"10.0.0.5/32\"\nport = \"5432\"\nproto = \"tcp\"\n",
            "not toml at all",
        ] {
            assert!(Grants::parse(bad).is_err(), "{bad:?}");
        }
    }

    fn grant(profile: &str, to: &str, port: u16, proto: Proto) -> Grant {
        Grant {
            profile: profile.into(),
            to: to.parse().unwrap(),
            port,
            proto,
        }
    }

    #[test]
    fn the_written_file_reads_back_equal() {
        let example = Grants::parse(EXAMPLE).unwrap();
        let replaced = Grants::parse("[dns]\nresolvers = [\"192.0.2.53\"]\n").unwrap();
        let reversed = Grants::parse("[dns]\nresolvers = [\"1.0.0.1\", \"1.1.1.1\"]\n").unwrap();
        let widest = Grants::default()
            .with_grant(grant("a", "0.0.0.0/0", 1, Proto::Udp))
            .unwrap()
            .with_grant(grant("b", "10.0.0.5/32", 65535, Proto::Tcp))
            .unwrap();
        for g in [Grants::default(), example, replaced, reversed, widest] {
            let text = g.to_toml();
            assert_eq!(Grants::parse(&text).unwrap(), g, "{text}");
            // Deterministic: writing what was read writes the same text.
            assert_eq!(Grants::parse(&text).unwrap().to_toml(), text);
        }
    }

    #[test]
    fn default_resolvers_are_omitted_and_any_other_list_is_written() {
        assert!(!Grants::default().to_toml().contains("[dns]"));
        // A file that spelled the default pair out reads back the same, and is
        // rewritten without it.
        let explicit = Grants::parse(EXAMPLE).unwrap();
        assert!(!explicit.to_toml().contains("[dns]"));
        let reversed = Grants::parse("[dns]\nresolvers = [\"1.0.0.1\", \"1.1.1.1\"]\n").unwrap();
        assert!(reversed
            .to_toml()
            .contains("resolvers = [\"1.0.0.1\", \"1.1.1.1\"]"));
    }

    #[test]
    fn a_single_host_is_written_with_its_prefix() {
        let g = Grants::default()
            .with_grant(grant("a", "10.0.0.5/32", 5432, Proto::Tcp))
            .unwrap();
        assert!(
            g.to_toml().contains("to = \"10.0.0.5/32\""),
            "{}",
            g.to_toml()
        );
        assert_eq!(cidr(&"10.0.0.0/8".parse().unwrap()), "10.0.0.0/8");
    }

    #[test]
    fn edits_keep_every_rule_parse_enforces() {
        let one = Grants::default()
            .with_grant(grant("a", "10.0.0.5/32", 1, Proto::Tcp))
            .unwrap();
        assert_eq!(
            one.with_grant(grant("a", "10.0.0.5/32", 1, Proto::Tcp)),
            Err(GrantsError::DuplicateGrant { index: 2 })
        );
        assert!(matches!(
            one.with_grant(grant("A", "10.0.0.5/32", 1, Proto::Tcp)),
            Err(GrantsError::BadProfile { index: 2, .. })
        ));
        assert_eq!(
            one.with_grant(grant("a", "10.0.0.5/32", 0, Proto::Tcp)),
            Err(GrantsError::BadPort { index: 2, port: 0 })
        );
        let back = one
            .without_grant(&grant("a", "10.0.0.5/32", 1, Proto::Tcp))
            .unwrap();
        assert_eq!(back, Grants::default());
        assert_eq!(
            one.without_grant(&grant("a", "10.0.0.5/32", 1, Proto::Udp)),
            Err(GrantsError::NoSuchGrant)
        );
        assert_eq!(one.with_resolvers(&[]), Err(GrantsError::NoResolvers));
        assert_eq!(
            one.with_resolvers(&[ip("224.0.0.251")]),
            Err(GrantsError::ResolverNotUnicast(ip("224.0.0.251")))
        );
        assert_eq!(
            one.with_resolvers(&[ip("9.9.9.9"), ip("9.9.9.9")]),
            Err(GrantsError::DuplicateResolver(ip("9.9.9.9")))
        );
        let nine = one.with_resolvers(&[ip("9.9.9.9")]).unwrap();
        assert_eq!(nine.resolvers(), &[ip("9.9.9.9")]);
        assert_eq!(nine.grants(), one.grants());
        assert!(one.retaining(|_| false).grants().is_empty());
    }

    #[test]
    fn a_toml_error_prints_as_one_line() {
        let e = Grants::parse("[[grant]]\nprofile = \n")
            .unwrap_err()
            .to_string();
        assert!(!e.contains('\n'), "{e}");
    }
}
