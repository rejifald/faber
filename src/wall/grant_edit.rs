//! What `sudo faber grant` decides before it writes anything: whether an
//! operator's request is a grant at all, whether it would do what they meant,
//! and what `config.toml` narrows it to. Pure: text and typed values in,
//! a [`Grant`] or a reason out, so all of it is tested on every OS.
//!
//! Everything an operator typed is checked here with an error of its own per
//! defect, and every error that quotes their input escapes it, so a terminal
//! never prints a control character root was handed.
//!
//! # A grant that would do nothing, or more than meant
//!
//! Two requests parse but are refused:
//!
//! - **Inert.** A destination entirely inside the fence ([`super::render`]: the
//!   plan's VM subnets, `0/8`, `127/8`, multicast, `240/4`) renders a rule the
//!   fence ahead of it always matches first. Storing it would tell the operator
//!   a job can reach something it never can.
//! - **Overlapping the host.** A destination holding one of the host's own
//!   addresses still never reaches the host (the host block comes first), but
//!   a range that wide is usually a typo for a narrower one. `--overlaps-host`
//!   is the operator saying it is meant.
//!
//! # Config narrows, never widens
//!
//! A profile's `drop_grants` in `config.toml` may remove grants of that
//! profile ([`effective`]); nothing there can add one or widen one
//! (ADR-0025, ADR-0026). A drop spec that does not parse is an error, not a
//! skip: the operator meant to narrow, and a root render that ignored it would
//! open what they closed.

use std::net::Ipv4Addr;

use super::grants::{cidr, valid_profile_name, Grant, Grants, Proto};
use super::model::{Ipv4Net, NetParseError};
use super::render::FENCE;

/// Why a destination was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DestError {
    #[error(
        "\"{0}\" looks like a hostname, and a destination is never one: a name can resolve \
         somewhere else tomorrow. Grant its IPv4 network instead (a tailnet host by its 100.x \
         address, as ADDRESS/32)"
    )]
    Hostname(String),
    #[error("\"{0}\" has no prefix: write {0}/32 for this one address, or the network it is in")]
    NoPrefix(String),
    #[error("{given} has host bits set below its prefix: the network is {network}")]
    HostBits { given: String, network: String },
    #[error("\"{0}\": the prefix must be 0 to 32")]
    Prefix(String),
    #[error("\"{0}\" is IPv6: the wall drops all IPv6, so a grant is IPv4 only")]
    Ipv6(String),
    #[error("\"{0}\" is not an IPv4 network written ADDRESS/PREFIX")]
    Shape(String),
}

/// A destination as the CLI and `drop_grants` take it: `ADDRESS/PREFIX`, the
/// prefix always written, no host bits set.
pub fn parse_destination(s: &str) -> Result<Ipv4Net, DestError> {
    let shown = || s.escape_debug().to_string();
    if s.contains(':') {
        return Err(DestError::Ipv6(shown()));
    }
    if s.bytes()
        .any(|b| b.is_ascii_alphabetic() || b == b'-' || b == b'_')
    {
        return Err(DestError::Hostname(shown()));
    }
    if s.is_empty()
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'/')
    {
        return Err(DestError::Shape(shown()));
    }
    let Some((addr, prefix)) = s.split_once('/') else {
        return match s.parse::<Ipv4Addr>() {
            Ok(_) => Err(DestError::NoPrefix(shown())),
            Err(_) => Err(DestError::Shape(shown())),
        };
    };
    let addr: Ipv4Addr = addr.parse().map_err(|_| DestError::Shape(shown()))?;
    // Digits only, and no leading zero: one spelling per prefix.
    let leading_zero = prefix.len() > 1 && prefix.starts_with('0');
    if prefix.is_empty() || prefix.contains('/') || leading_zero {
        return Err(DestError::Shape(shown()));
    }
    let prefix: u8 = match prefix.parse() {
        Ok(p) if p <= 32 => p,
        _ => return Err(DestError::Prefix(shown())),
    };
    Ipv4Net::new(addr, prefix).map_err(|e| match e {
        NetParseError::HostBits(given) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            let network = Ipv4Addr::from(u32::from(addr) & mask);
            DestError::HostBits {
                given,
                network: format!("{network}/{prefix}"),
            }
        }
        NetParseError::Prefix(_) => DestError::Prefix(shown()),
        NetParseError::Shape(_) => DestError::Shape(shown()),
    })
}

/// Why a port was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    #[error("\"{0}\" is not PORT/PROTO, such as 5432/tcp or 53000/udp")]
    Shape(String),
    #[error("\"{0}\" is not a port number: plain digits, no sign, no leading zero")]
    Number(String),
    #[error("port {0} is outside 1..=65535")]
    Range(String),
    #[error("\"{0}\" is not tcp or udp (lowercase)")]
    Proto(String),
}

/// A port as the CLI and `drop_grants` take it: exactly `N/tcp` or `N/udp`,
/// `N` in `1..=65535` in plain ASCII digits.
pub fn parse_port(s: &str) -> Result<(u16, Proto), PortError> {
    let shown = |t: &str| t.escape_debug().to_string();
    let (n, proto) = s
        .split_once('/')
        .ok_or_else(|| PortError::Shape(shown(s)))?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) || (n.len() > 1 && n.starts_with('0'))
    {
        return Err(PortError::Number(shown(n)));
    }
    let port = n
        .parse::<u32>()
        .ok()
        .and_then(|p| u16::try_from(p).ok())
        .filter(|&p| p != 0)
        .ok_or_else(|| PortError::Range(shown(n)))?;
    let proto = match proto {
        "tcp" => Proto::Tcp,
        "udp" => Proto::Udp,
        other => return Err(PortError::Proto(shown(other))),
    };
    Ok((port, proto))
}

/// One grant as an operator typed it, on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRequest {
    pub profile: String,
    pub to: String,
    /// `N/tcp` or `N/udp`.
    pub port: String,
}

/// What an add is checked against, read before it is decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AddContext {
    /// The profile names `config.toml` defines.
    pub known_profiles: Vec<String>,
    /// The host's own IPv4 addresses now.
    pub host_addrs: Vec<Ipv4Addr>,
    /// The network plan's VM subnets; empty before setup writes a plan.
    pub vm_subnets: Vec<Ipv4Net>,
    /// `--overlaps-host`: the operator means a range holding a host address.
    pub confirm_host_overlap: bool,
}

/// Why an add was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddError {
    #[error(
        "profile \"{0}\" is not lowercase letters, digits, '_' or '-', so no grant can name it"
    )]
    BadProfile(String),
    #[error("config.toml defines no profile \"{name}\" (it defines: {known})")]
    UnknownProfile { name: String, known: String },
    #[error(transparent)]
    Destination(#[from] DestError),
    #[error(transparent)]
    Port(#[from] PortError),
    #[error(
        "{to} lies entirely inside {fence}, which the wall drops ahead of every grant, so this \
         grant could never match anything"
    )]
    Inert { to: String, fence: String },
    #[error(
        "{to} holds this host's own address {addrs}. The wall still blocks the host itself, \
         whatever a grant says; if this range is meant, run again with --overlaps-host"
    )]
    HostOverlap { to: String, addrs: String },
    #[error("profile {profile} already has the grant {to} {port}")]
    Duplicate {
        profile: String,
        to: String,
        port: String,
    },
}

/// Check an add and return the grant it is. In order: the profile's name, that
/// `config.toml` defines it, the destination, the port, that the grant is not
/// inert, that it holds no host address unless confirmed, and that it is new.
pub fn validate_add(
    req: &GrantRequest,
    ctx: &AddContext,
    current: &Grants,
) -> Result<Grant, AddError> {
    if !valid_profile_name(&req.profile) {
        return Err(AddError::BadProfile(req.profile.escape_debug().to_string()));
    }
    if !ctx.known_profiles.contains(&req.profile) {
        let known: Vec<String> = ctx
            .known_profiles
            .iter()
            .map(|p| p.escape_debug().to_string())
            .collect();
        return Err(AddError::UnknownProfile {
            name: req.profile.clone(),
            known: if known.is_empty() {
                "none".into()
            } else {
                known.join(", ")
            },
        });
    }
    let to = parse_destination(&req.to)?;
    let (port, proto) = parse_port(&req.port)?;
    if let Some(fence) = fence_holding(&to, &ctx.vm_subnets) {
        return Err(AddError::Inert {
            to: cidr(&to),
            fence: cidr(&fence),
        });
    }
    let held: Vec<String> = ctx
        .host_addrs
        .iter()
        .filter(|a| to.contains(**a))
        .map(Ipv4Addr::to_string)
        .collect();
    if !held.is_empty() && !ctx.confirm_host_overlap {
        return Err(AddError::HostOverlap {
            to: cidr(&to),
            addrs: held.join(", "),
        });
    }
    let grant = Grant {
        profile: req.profile.clone(),
        to,
        port,
        proto,
    };
    if current.grants().contains(&grant) {
        return Err(AddError::Duplicate {
            profile: grant.profile,
            to: cidr(&grant.to),
            port: format!("{port}/{proto}"),
        });
    }
    Ok(grant)
}

/// The fence range `to` lies entirely inside, if any: [`FENCE`] and the
/// plan's VM subnets, as [`super::render::fence_ranges`] renders them.
fn fence_holding(to: &Ipv4Net, vm_subnets: &[Ipv4Net]) -> Option<Ipv4Net> {
    FENCE
        .iter()
        .filter_map(|&(a, p)| Ipv4Net::new(a, p).ok())
        .chain(vm_subnets.iter().copied())
        .find(|range| to.within(range))
}

/// Why a remove was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoveError {
    #[error(transparent)]
    Destination(#[from] DestError),
    #[error(transparent)]
    Port(#[from] PortError),
    #[error("profile \"{profile}\" has no grant {to} {port}; its grants: {current}")]
    NotFound {
        profile: String,
        to: String,
        port: String,
        current: String,
    },
}

/// Check a remove: it must name an existing grant exactly.
pub fn validate_remove(req: &GrantRequest, current: &Grants) -> Result<Grant, RemoveError> {
    let to = parse_destination(&req.to)?;
    let (port, proto) = parse_port(&req.port)?;
    let grant = Grant {
        profile: req.profile.clone(),
        to,
        port,
        proto,
    };
    if current.grants().contains(&grant) {
        return Ok(grant);
    }
    let theirs: Vec<String> = current
        .grants()
        .iter()
        .filter(|g| g.profile == req.profile)
        .map(|g| format!("{} {}/{}", cidr(&g.to), g.port, g.proto))
        .collect();
    Err(RemoveError::NotFound {
        profile: req.profile.escape_debug().to_string(),
        to: cidr(&to),
        port: format!("{port}/{proto}"),
        current: if theirs.is_empty() {
            "none".into()
        } else {
            theirs.join(", ")
        },
    })
}

/// Why a `drop_grants` entry does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DropSpecError {
    #[error("\"{0}\" is not \"ADDRESS/PREFIX PORT/PROTO\", such as \"10.0.0.5/32 5432/tcp\"")]
    Shape(String),
    #[error(transparent)]
    Destination(#[from] DestError),
    #[error(transparent)]
    Port(#[from] PortError),
}

/// One `drop_grants` entry: `"<CIDR> <N>/<proto>"`, the two separated by
/// whitespace, nothing else.
pub fn parse_drop_spec(s: &str) -> Result<(Ipv4Net, u16, Proto), DropSpecError> {
    let parts: Vec<&str> = s.split_ascii_whitespace().collect();
    let [to, port] = parts[..] else {
        return Err(DropSpecError::Shape(s.escape_debug().to_string()));
    };
    let to = parse_destination(to)?;
    let (port, proto) = parse_port(port)?;
    Ok((to, port, proto))
}

/// A `drop_grants` entry that could not be applied, so nothing was rendered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("config.toml, profile \"{profile}\", drop_grants: {why}")]
pub struct DropError {
    pub profile: String,
    pub why: DropSpecError,
}

/// Something worth telling the operator about the narrowing, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropNote {
    /// A drop names no grant of its profile: there was nothing to narrow.
    NoMatch { profile: String, spec: String },
}

impl std::fmt::Display for DropNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DropNote::NoMatch { profile, spec } => write!(
                f,
                "config.toml drops \"{spec}\" from profile \"{profile}\", which has no such grant"
            ),
        }
    }
}

/// The grants a root render uses: `grants` less every grant a profile's
/// `drop_grants` in `config.toml` names exactly. `drops` is each profile's
/// name and its drop specs. It only ever removes: the resolvers and every
/// grant not named are unchanged.
pub fn effective(
    grants: &Grants,
    drops: &[(String, Vec<String>)],
) -> Result<(Grants, Vec<DropNote>), DropError> {
    let mut dropped: Vec<Grant> = Vec::new();
    let mut notes = Vec::new();
    for (profile, specs) in drops {
        for spec in specs {
            let (to, port, proto) = parse_drop_spec(spec).map_err(|why| DropError {
                profile: profile.escape_debug().to_string(),
                why,
            })?;
            let g = Grant {
                profile: profile.clone(),
                to,
                port,
                proto,
            };
            if grants.grants().contains(&g) {
                dropped.push(g);
            } else {
                notes.push(DropNote::NoMatch {
                    profile: profile.escape_debug().to_string(),
                    spec: spec.escape_debug().to_string(),
                });
            }
        }
    }
    Ok((grants.retaining(|g| !dropped.contains(g)), notes))
}

/// One grant as `list` prints it: `profile  10.0.0.5/32  5432/tcp`.
pub fn grant_line(g: &Grant) -> String {
    format!("{}  {}  {}/{}", g.profile, cidr(&g.to), g.port, g.proto)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn req(profile: &str, to: &str, port: &str) -> GrantRequest {
        GrantRequest {
            profile: profile.into(),
            to: to.into(),
            port: port.into(),
        }
    }

    fn ctx() -> AddContext {
        AddContext {
            known_profiles: vec!["macos-vm".into(), "docker".into()],
            host_addrs: vec![ip("127.0.0.1"), ip("192.0.2.10"), ip("192.168.64.1")],
            vm_subnets: vec![net("192.168.64.0/24")],
            confirm_host_overlap: false,
        }
    }

    fn grant(profile: &str, to: &str, port: u16, proto: Proto) -> Grant {
        Grant {
            profile: profile.into(),
            to: net(to),
            port,
            proto,
        }
    }

    #[test]
    fn a_destination_is_cidr_only() {
        assert_eq!(parse_destination("10.0.0.5/32"), Ok(net("10.0.0.5/32")));
        assert_eq!(parse_destination("100.64.0.0/10"), Ok(net("100.64.0.0/10")));
        assert_eq!(parse_destination("0.0.0.0/0"), Ok(net("0.0.0.0/0")));
    }

    #[test]
    fn each_destination_defect_is_its_own_error() {
        for host in [
            "db.lan",
            "db",
            "my-db",
            "a_b",
            "db.lan/32",
            "1.2.3.4.example",
        ] {
            assert!(
                matches!(parse_destination(host), Err(DestError::Hostname(_))),
                "{host}"
            );
        }
        let e = parse_destination("db.lan").unwrap_err().to_string();
        assert!(e.contains("100.x") && e.contains("never"), "{e}");
        assert_eq!(
            parse_destination("10.0.0.5"),
            Err(DestError::NoPrefix("10.0.0.5".into()))
        );
        assert!(parse_destination("10.0.0.5")
            .unwrap_err()
            .to_string()
            .contains("10.0.0.5/32"));
        assert_eq!(
            parse_destination("10.0.0.5/24"),
            Err(DestError::HostBits {
                given: "10.0.0.5/24".into(),
                network: "10.0.0.0/24".into()
            })
        );
        for bad in ["10.0.0.0/33", "10.0.0.0/300"] {
            assert!(
                matches!(parse_destination(bad), Err(DestError::Prefix(_))),
                "{bad}"
            );
        }
        for v6 in ["::1/128", "fd00::/8", "::"] {
            assert!(
                matches!(parse_destination(v6), Err(DestError::Ipv6(_))),
                "{v6}"
            );
        }
        for bad in [
            "",
            "10.0.0/8",
            "10.0.0.0/",
            "10.0.0.0/08",
            "10.0.0.0/8/8",
            "010.0.0.0/8",
            " 10.0.0.0/8",
            "10.0.0.0/8\n",
            "10.0.0.0/+8",
        ] {
            assert!(
                matches!(parse_destination(bad), Err(DestError::Shape(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_port_is_exactly_number_slash_proto() {
        assert_eq!(parse_port("5432/tcp"), Ok((5432, Proto::Tcp)));
        assert_eq!(parse_port("1/udp"), Ok((1, Proto::Udp)));
        assert_eq!(parse_port("65535/udp"), Ok((65535, Proto::Udp)));
        for bad in ["5432", "5432 tcp", ""] {
            assert!(matches!(parse_port(bad), Err(PortError::Shape(_))), "{bad}");
        }
        for bad in [
            "+5432/tcp",
            "05432/tcp",
            "/tcp",
            "5４32/tcp",
            "-1/tcp",
            "54 32/tcp",
        ] {
            assert!(
                matches!(parse_port(bad), Err(PortError::Number(_))),
                "{bad}"
            );
        }
        for bad in ["0/tcp", "65536/tcp", "99999999999/tcp"] {
            assert!(matches!(parse_port(bad), Err(PortError::Range(_))), "{bad}");
        }
        for bad in ["5432/TCP", "5432/icmp", "5432/", "5432/tcp/udp"] {
            assert!(matches!(parse_port(bad), Err(PortError::Proto(_))), "{bad}");
        }
    }

    #[test]
    fn an_add_is_checked_in_order() {
        let g = Grants::default();
        assert_eq!(
            validate_add(&req("macos-vm", "10.0.0.5/32", "5432/tcp"), &ctx(), &g),
            Ok(grant("macos-vm", "10.0.0.5/32", 5432, Proto::Tcp))
        );
        // The profile's shape is judged before anything else is read.
        assert!(matches!(
            validate_add(&req("Bad Name", "db.lan", "x"), &ctx(), &g),
            Err(AddError::BadProfile(_))
        ));
        let e = validate_add(&req("linux", "db.lan", "x"), &ctx(), &g).unwrap_err();
        assert_eq!(
            e,
            AddError::UnknownProfile {
                name: "linux".into(),
                known: "macos-vm, docker".into()
            }
        );
        let none = AddContext {
            known_profiles: vec![],
            ..ctx()
        };
        assert!(
            validate_add(&req("linux", "10.0.0.5/32", "1/tcp"), &none, &g)
                .unwrap_err()
                .to_string()
                .contains("it defines: none")
        );
        assert!(matches!(
            validate_add(&req("docker", "db.lan", "x"), &ctx(), &g),
            Err(AddError::Destination(DestError::Hostname(_)))
        ));
        assert!(matches!(
            validate_add(&req("docker", "10.0.0.5/32", "x"), &ctx(), &g),
            Err(AddError::Port(PortError::Shape(_)))
        ));
    }

    #[test]
    fn a_grant_inside_the_fence_is_refused_as_inert() {
        let g = Grants::default();
        for (to, fence) in [
            ("192.168.64.7/32", "192.168.64.0/24"),
            ("192.168.64.0/24", "192.168.64.0/24"),
            ("127.0.0.1/32", "127.0.0.0/8"),
            ("224.0.0.251/32", "224.0.0.0/4"),
            ("255.255.255.255/32", "240.0.0.0/4"),
            ("0.0.0.0/8", "0.0.0.0/8"),
        ] {
            assert_eq!(
                validate_add(&req("docker", to, "1/udp"), &ctx(), &g),
                Err(AddError::Inert {
                    to: to.into(),
                    fence: fence.into()
                }),
                "{to}"
            );
        }
        // Wider than a fence range is not inert: the part outside it matches.
        let wide = AddContext {
            confirm_host_overlap: true,
            ..ctx()
        };
        assert!(validate_add(&req("docker", "192.168.0.0/16", "22/tcp"), &wide, &g).is_ok());
        assert!(validate_add(&req("docker", "0.0.0.0/0", "22/tcp"), &wide, &g).is_ok());
    }

    #[test]
    fn a_range_holding_a_host_address_needs_confirming() {
        let g = Grants::default();
        let e = validate_add(&req("docker", "192.0.2.0/24", "22/tcp"), &ctx(), &g).unwrap_err();
        assert_eq!(
            e,
            AddError::HostOverlap {
                to: "192.0.2.0/24".into(),
                addrs: "192.0.2.10".into()
            }
        );
        assert!(e.to_string().contains("--overlaps-host"), "{e}");
        assert!(e.to_string().contains("still blocks the host"), "{e}");
        let e = validate_add(&req("docker", "0.0.0.0/0", "22/tcp"), &ctx(), &g).unwrap_err();
        assert!(matches!(&e, AddError::HostOverlap { addrs, .. }
            if addrs == "127.0.0.1, 192.0.2.10, 192.168.64.1"));
        let yes = AddContext {
            confirm_host_overlap: true,
            ..ctx()
        };
        assert!(validate_add(&req("docker", "192.0.2.0/24", "22/tcp"), &yes, &g).is_ok());
    }

    #[test]
    fn an_add_already_granted_is_refused() {
        let g = Grants::default()
            .with_grant(grant("docker", "10.0.0.5/32", 5432, Proto::Tcp))
            .unwrap();
        assert!(matches!(
            validate_add(&req("docker", "10.0.0.5/32", "5432/tcp"), &ctx(), &g),
            Err(AddError::Duplicate { .. })
        ));
        // Another protocol or profile is another grant.
        assert!(validate_add(&req("docker", "10.0.0.5/32", "5432/udp"), &ctx(), &g).is_ok());
        assert!(validate_add(&req("macos-vm", "10.0.0.5/32", "5432/tcp"), &ctx(), &g).is_ok());
    }

    #[test]
    fn errors_never_carry_control_characters() {
        let g = Grants::default();
        let nasty = "\u{1b}[31mx\n";
        let errs = [
            validate_add(&req(nasty, "1.2.3.4/32", "1/tcp"), &ctx(), &g)
                .unwrap_err()
                .to_string(),
            validate_add(&req("docker", nasty, "1/tcp"), &ctx(), &g)
                .unwrap_err()
                .to_string(),
            validate_add(&req("docker", "10.0.0.5/32", nasty), &ctx(), &g)
                .unwrap_err()
                .to_string(),
            validate_add(
                &req("docker", "10.0.0.5/32", &format!("1/{nasty}")),
                &ctx(),
                &g,
            )
            .unwrap_err()
            .to_string(),
            validate_remove(&req(nasty, "10.0.0.5/32", "1/tcp"), &g)
                .unwrap_err()
                .to_string(),
            effective(&g, &[(nasty.into(), vec![nasty.into()])])
                .unwrap_err()
                .to_string(),
        ];
        for e in errs {
            assert!(!e.chars().any(char::is_control), "{e:?}");
        }
        let known = AddContext {
            known_profiles: vec![nasty.into()],
            ..ctx()
        };
        let e = validate_add(&req("docker", "10.0.0.5/32", "1/tcp"), &known, &g)
            .unwrap_err()
            .to_string();
        assert!(!e.chars().any(char::is_control), "{e:?}");
    }

    #[test]
    fn a_remove_must_match_exactly_and_names_what_there_is() {
        let g = Grants::default()
            .with_grant(grant("docker", "10.0.0.5/32", 5432, Proto::Tcp))
            .unwrap()
            .with_grant(grant("docker", "10.0.0.0/24", 22, Proto::Tcp))
            .unwrap();
        assert_eq!(
            validate_remove(&req("docker", "10.0.0.5/32", "5432/tcp"), &g),
            Ok(grant("docker", "10.0.0.5/32", 5432, Proto::Tcp))
        );
        let e = validate_remove(&req("docker", "10.0.0.5/32", "5432/udp"), &g).unwrap_err();
        assert_eq!(
            e,
            RemoveError::NotFound {
                profile: "docker".into(),
                to: "10.0.0.5/32".into(),
                port: "5432/udp".into(),
                current: "10.0.0.5/32 5432/tcp, 10.0.0.0/24 22/tcp".into()
            }
        );
        assert!(
            validate_remove(&req("macos-vm", "10.0.0.5/32", "5432/tcp"), &g)
                .unwrap_err()
                .to_string()
                .ends_with("its grants: none")
        );
        assert!(matches!(
            validate_remove(&req("docker", "10.0.0.5", "5432/tcp"), &g),
            Err(RemoveError::Destination(DestError::NoPrefix(_)))
        ));
    }

    #[test]
    fn a_drop_spec_is_a_destination_and_a_port() {
        assert_eq!(
            parse_drop_spec("10.0.0.5/32 5432/tcp"),
            Ok((net("10.0.0.5/32"), 5432, Proto::Tcp))
        );
        assert_eq!(
            parse_drop_spec("  10.0.0.0/8\t22/udp "),
            Ok((net("10.0.0.0/8"), 22, Proto::Udp))
        );
        for bad in [
            "",
            "10.0.0.5/32",
            "10.0.0.5/32 5432/tcp extra",
            "10.0.0.5/32,5432/tcp",
        ] {
            assert!(
                matches!(parse_drop_spec(bad), Err(DropSpecError::Shape(_))),
                "{bad:?}"
            );
        }
        assert!(matches!(
            parse_drop_spec("db.lan 5432/tcp"),
            Err(DropSpecError::Destination(DestError::Hostname(_)))
        ));
        assert!(matches!(
            parse_drop_spec("10.0.0.5/32 5432"),
            Err(DropSpecError::Port(PortError::Shape(_)))
        ));
    }

    fn two() -> Grants {
        Grants::default()
            .with_grant(grant("docker", "10.0.0.5/32", 5432, Proto::Tcp))
            .unwrap()
            .with_grant(grant("macos-vm", "10.0.0.5/32", 5432, Proto::Tcp))
            .unwrap()
    }

    #[test]
    fn a_drop_removes_only_its_own_profiles_exact_grant() {
        let drops = vec![(
            "docker".to_string(),
            vec!["10.0.0.5/32 5432/tcp".to_string()],
        )];
        let (eff, notes) = effective(&two(), &drops).unwrap();
        assert_eq!(
            eff.grants(),
            &[grant("macos-vm", "10.0.0.5/32", 5432, Proto::Tcp)]
        );
        assert!(notes.is_empty());
        assert_eq!(eff.resolvers(), two().resolvers());
        // No drops: unchanged.
        assert_eq!(effective(&two(), &[]).unwrap().0, two());
    }

    #[test]
    fn a_drop_naming_no_grant_is_a_note_and_a_bad_one_an_error() {
        let drops = vec![(
            "docker".to_string(),
            vec!["10.0.0.0/24 5432/tcp".to_string()],
        )];
        let (eff, notes) = effective(&two(), &drops).unwrap();
        assert_eq!(eff, two());
        assert_eq!(
            notes,
            vec![DropNote::NoMatch {
                profile: "docker".into(),
                spec: "10.0.0.0/24 5432/tcp".into()
            }]
        );
        let bad = vec![("docker".to_string(), vec!["db.lan 5432/tcp".to_string()])];
        let e = effective(&two(), &bad).unwrap_err();
        assert_eq!(e.profile, "docker");
        assert!(matches!(e.why, DropSpecError::Destination(_)));
    }

    /// A small deterministic generator: the property below needs variety, not
    /// cryptographic randomness, and must not flake.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    #[test]
    fn narrowing_never_adds_widens_or_touches_the_resolvers() {
        let profiles = ["a", "b", "c"];
        let nets = ["10.0.0.5/32", "10.0.0.0/24", "10.0.0.0/8", "0.0.0.0/0"];
        let ports = ["22/tcp", "22/udp", "5432/tcp"];
        let mut rng = Lcg(7);
        for round in 0..500 {
            let mut g = if round % 2 == 0 {
                Grants::default()
            } else {
                Grants::default().with_resolvers(&[ip("9.9.9.9")]).unwrap()
            };
            for _ in 0..rng.below(6) {
                let (port, proto) = parse_port(ports[rng.below(3) as usize]).unwrap();
                let candidate = grant(
                    profiles[rng.below(3) as usize],
                    nets[rng.below(4) as usize],
                    port,
                    proto,
                );
                if let Ok(next) = g.with_grant(candidate) {
                    g = next;
                }
            }
            let drops: Vec<(String, Vec<String>)> = (0..rng.below(4))
                .map(|_| {
                    let specs = (0..rng.below(3))
                        .map(|_| {
                            format!(
                                "{} {}",
                                nets[rng.below(4) as usize],
                                ports[rng.below(3) as usize]
                            )
                        })
                        .collect();
                    (profiles[rng.below(3) as usize].to_string(), specs)
                })
                .collect();
            let (eff, _) = effective(&g, &drops).unwrap();
            assert_eq!(eff.resolvers(), g.resolvers());
            for e in eff.grants() {
                assert!(g.grants().contains(e), "{e:?} was added");
            }
            // Exactly the named grants are gone.
            for orig in g.grants() {
                let named = drops.iter().any(|(p, specs)| {
                    *p == orig.profile
                        && specs.iter().any(|s| {
                            parse_drop_spec(s).unwrap() == (orig.to, orig.port, orig.proto)
                        })
                });
                assert_eq!(eff.grants().contains(orig), !named, "{orig:?}");
            }
        }
    }

    #[test]
    fn a_grant_lists_with_its_prefix() {
        assert_eq!(
            grant_line(&grant("macos-vm", "10.0.0.5/32", 5432, Proto::Tcp)),
            "macos-vm  10.0.0.5/32  5432/tcp"
        );
    }
}
