//! The typed description of the wall: the network plan setup writes, what the
//! host has right now, and the binding of one to the other.
//!
//! The plan is static: which VM networks exist, their gateways and the guest
//! addresses reserved in them. The interfaces are not. vmnet creates a bridge
//! only while a VM on it runs (the spike's first version failed on assuming
//! otherwise), and the host's own addresses change with the networks it joins.
//! So [`bind`] runs on every check, and the stored rules are bound to its
//! result ([`super::stored::instantiate`]): it supplies the interface each
//! network's rules apply on, and the host addresses, and nothing else.
//!
//! Everything here is spliced into pf text by [`super::render`] or
//! [`super::stored::instantiate`], so every value
//! is validated into a type that cannot carry anything else: an [`IfName`] is
//! letters then digits, and addresses are [`Ipv4Addr`]s.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::grants::valid_profile_name;

/// A network interface name that is safe to splice into a pf rule: 1–8 ASCII
/// lowercase letters, then 1–4 ASCII digits (`bridge100`, `en0`, `utun3`).
/// Anything else — a space, a quote, a `;`, a non-ASCII letter — is refused,
/// because the name ends up in rule text root loads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IfName(String);

/// The kernel's limit, `IFNAMSIZ` less its NUL. The pattern already caps a name
/// at 12 bytes; this is the backstop.
const IFNAME_MAX: usize = 15;

impl IfName {
    pub fn parse(s: &str) -> Result<Self, IfNameError> {
        let bad = || IfNameError(s.escape_debug().to_string());
        if s.is_empty() || s.len() > IFNAME_MAX {
            return Err(bad());
        }
        let letters = s.bytes().take_while(u8::is_ascii_lowercase).count();
        let digits = s.len() - letters;
        if !(1..=8).contains(&letters)
            || !(1..=4).contains(&digits)
            || !s.as_bytes()[letters..].iter().all(u8::is_ascii_digit)
        {
            return Err(bad());
        }
        Ok(Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IfName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An interface name [`IfName`] refused, escaped for printing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("interface name \"{0}\" is not letters then digits")]
pub struct IfNameError(String);

/// An IPv4 network: an address with no host bits set, and a prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ipv4Net {
    addr: Ipv4Addr,
    prefix: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NetParseError {
    #[error("\"{0}\" is not ADDRESS/PREFIX")]
    Shape(String),
    #[error("prefix /{0} is longer than 32")]
    Prefix(u8),
    #[error("{0} has host bits set below its prefix")]
    HostBits(String),
}

impl Ipv4Net {
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Result<Self, NetParseError> {
        if prefix > 32 {
            return Err(NetParseError::Prefix(prefix));
        }
        let net = Self { addr, prefix };
        if net.network() != addr {
            return Err(NetParseError::HostBits(format!("{addr}/{prefix}")));
        }
        Ok(net)
    }

    pub fn addr(&self) -> Ipv4Addr {
        self.addr
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    fn mask(&self) -> u32 {
        // `checked_shl` is `None` for a shift of 32, which is the /0 mask.
        u32::MAX
            .checked_shl(32 - u32::from(self.prefix))
            .unwrap_or(0)
    }

    pub fn network(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) & self.mask())
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) | !self.mask())
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.network())
    }

    /// Two networks overlap exactly when one contains the other's base.
    pub fn overlaps(&self, other: &Ipv4Net) -> bool {
        self.contains(other.addr) || other.contains(self.addr)
    }

    /// Whether `self` lies entirely inside `outer`.
    pub fn within(&self, outer: &Ipv4Net) -> bool {
        self.prefix >= outer.prefix && outer.contains(self.addr)
    }
}

/// pf's printed form: a single host has no `/32`.
impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix == 32 {
            write!(f, "{}", self.addr)
        } else {
            write!(f, "{}/{}", self.addr, self.prefix)
        }
    }
}

impl FromStr for Ipv4Net {
    type Err = NetParseError;

    /// `ADDRESS/PREFIX`, always with the prefix: a plan states its widths.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let shape = || NetParseError::Shape(s.escape_debug().to_string());
        let (addr, prefix) = s.split_once('/').ok_or_else(shape)?;
        let addr: Ipv4Addr = addr.parse().map_err(|_| shape())?;
        if prefix.is_empty() || prefix.len() > 2 || !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(shape());
        }
        let prefix: u8 = prefix.parse().map_err(|_| shape())?;
        Self::new(addr, prefix)
    }
}

impl Serialize for Ipv4Net {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}/{}", self.addr, self.prefix))
    }
}

impl<'de> Deserialize<'de> for Ipv4Net {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// One guest address reserved in a VM network. Its id names it in rule labels.
///
/// `profiles` are the profiles whose jobs run in the VM at this address: one
/// for a macOS VM, every Docker profile for the Linux VM (a Docker grant covers
/// the whole Linux VM, ADR-0026). A grant applies to the slot exactly when its
/// profile is listed here. Absent is none, so no grant applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSlot {
    pub id: u8,
    pub addr: Ipv4Addr,
    #[serde(default)]
    pub profiles: Vec<String>,
}

/// One VM network: the host side's gateway address, its subnet, and the guest
/// addresses reserved in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmNetwork {
    pub id: u8,
    pub gateway: Ipv4Addr,
    pub subnet: Ipv4Net,
    pub slots: Vec<VmSlot>,
}

/// The plan setup writes: every VM network faber may run guests on. Root
/// renders rules from it, so a field this release does not know is refused
/// rather than skipped.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPlan {
    pub networks: Vec<VmNetwork>,
}

/// Why a plan is refused. Each is its own variant so a test can name it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("network {0} is listed twice")]
    DuplicateNetworkId(u8),
    #[error("network {net}: prefix /{prefix} is outside /16../30")]
    PrefixOutOfRange { net: u8, prefix: u8 },
    #[error("network {net}: {subnet} is not inside 10/8, 172.16/12 or 192.168/16")]
    NotPrivate { net: u8, subnet: Ipv4Net },
    #[error("network {0} reserves no guest address")]
    NoSlots(u8),
    #[error("network {net}: gateway {gateway} is outside {subnet}")]
    GatewayOutside {
        net: u8,
        gateway: Ipv4Addr,
        subnet: Ipv4Net,
    },
    #[error("network {net}: gateway {gateway} is the network or broadcast address")]
    GatewayReserved { net: u8, gateway: Ipv4Addr },
    #[error("network {net}, slot {slot}: {addr} is outside the subnet")]
    SlotOutside { net: u8, slot: u8, addr: Ipv4Addr },
    #[error("network {net}, slot {slot}: {addr} is the gateway, network or broadcast address")]
    SlotReserved { net: u8, slot: u8, addr: Ipv4Addr },
    #[error("network {net}: slot {slot} is listed twice")]
    DuplicateSlotId { net: u8, slot: u8 },
    #[error("network {net}: {addr} is reserved twice")]
    DuplicateSlotAddr { net: u8, addr: Ipv4Addr },
    #[error("networks {a} and {b} overlap")]
    Overlap { a: u8, b: u8 },
    #[error("network {net}, slot {slot}: profile \"{name}\" is not lowercase letters, digits, '_' or '-'")]
    BadProfile { net: u8, slot: u8, name: String },
    #[error("network {net}, slot {slot}: profile {name} is listed twice")]
    DuplicateProfile { net: u8, slot: u8, name: String },
    #[error("[dns] resolver {resolver} is inside network {net}'s subnet {subnet}: it would open port 53 on another guest")]
    ResolverInsideSubnet {
        net: u8,
        resolver: Ipv4Addr,
        subnet: Ipv4Net,
    },
}

/// The private ranges a VM subnet must lie in. Keeping guests there means the
/// rendered nonpublic block already covers their subnet's directed broadcast
/// and every other guest.
const PRIVATE: [(Ipv4Addr, u8); 3] = [
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
];

impl NetworkPlan {
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// The first defect in the plan, or `Ok`. Networks are checked in the order
    /// listed, each completely, before the checks between networks.
    pub fn validate(&self) -> Result<(), PlanError> {
        let mut ids = BTreeSet::new();
        for n in &self.networks {
            if !ids.insert(n.id) {
                return Err(PlanError::DuplicateNetworkId(n.id));
            }
            validate_network(n)?;
        }
        for (i, a) in self.networks.iter().enumerate() {
            for b in &self.networks[i + 1..] {
                if a.subnet.overlaps(&b.subnet) {
                    return Err(PlanError::Overlap { a: a.id, b: b.id });
                }
            }
        }
        Ok(())
    }
}

fn validate_network(n: &VmNetwork) -> Result<(), PlanError> {
    let net = n.id;
    let subnet = n.subnet;
    if !(16..=30).contains(&subnet.prefix()) {
        return Err(PlanError::PrefixOutOfRange {
            net,
            prefix: subnet.prefix(),
        });
    }
    let private = PRIVATE
        .iter()
        .any(|&(addr, prefix)| Ipv4Net::new(addr, prefix).is_ok_and(|outer| subnet.within(&outer)));
    if !private {
        return Err(PlanError::NotPrivate { net, subnet });
    }
    if n.slots.is_empty() {
        return Err(PlanError::NoSlots(net));
    }
    let reserved = [subnet.network(), subnet.broadcast()];
    if !subnet.contains(n.gateway) {
        return Err(PlanError::GatewayOutside {
            net,
            gateway: n.gateway,
            subnet,
        });
    }
    if reserved.contains(&n.gateway) {
        return Err(PlanError::GatewayReserved {
            net,
            gateway: n.gateway,
        });
    }
    let mut slot_ids = BTreeSet::new();
    let mut addrs = BTreeSet::new();
    for s in &n.slots {
        if !subnet.contains(s.addr) {
            return Err(PlanError::SlotOutside {
                net,
                slot: s.id,
                addr: s.addr,
            });
        }
        if s.addr == n.gateway || reserved.contains(&s.addr) {
            return Err(PlanError::SlotReserved {
                net,
                slot: s.id,
                addr: s.addr,
            });
        }
        if !slot_ids.insert(s.id) {
            return Err(PlanError::DuplicateSlotId { net, slot: s.id });
        }
        if !addrs.insert(s.addr) {
            return Err(PlanError::DuplicateSlotAddr { net, addr: s.addr });
        }
        let mut profiles = BTreeSet::new();
        for name in &s.profiles {
            if !valid_profile_name(name) {
                return Err(PlanError::BadProfile {
                    net,
                    slot: s.id,
                    name: name.escape_debug().to_string(),
                });
            }
            if !profiles.insert(name) {
                return Err(PlanError::DuplicateProfile {
                    net,
                    slot: s.id,
                    name: name.clone(),
                });
            }
        }
    }
    Ok(())
}

/// One interface the host has right now, and its IPv4 addresses with their
/// prefix lengths. The name is as the host printed it, not yet validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInterface {
    pub name: String,
    pub addrs: Vec<(Ipv4Addr, u8)>,
}

/// What the host has right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveView {
    pub interfaces: Vec<HostInterface>,
}

/// A VM network whose bridge exists, and that bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundNetwork {
    pub net: VmNetwork,
    pub interface: IfName,
}

/// What the rules are rendered from. Networks are sorted by id and their slots
/// by id, so the rendering does not depend on the order anything was listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallSpec {
    pub networks: Vec<BoundNetwork>,
    /// Every IPv4 address on every host interface, sorted and deduplicated,
    /// loopback and the VM gateways included. The rules name them one by one:
    /// in the spike the dynamic `(self)` form matched nothing.
    pub host_addrs: Vec<Ipv4Addr>,
    /// Ids of networks whose gateway no interface holds: no VM runs there, so
    /// nothing can send on them, and they get no rules until one does.
    pub pending: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error("the plan is invalid: {0}")]
    Plan(#[from] PlanError),
    #[error("network {net}: its gateway is on more than one interface ({interfaces})")]
    GatewayOnManyInterfaces { net: u8, interfaces: String },
    #[error("network {net}: {source}")]
    BadInterfaceName { net: u8, source: IfNameError },
    #[error("networks {a} and {b} are both on {interface}")]
    SharedInterface { a: u8, b: u8, interface: IfName },
}

/// Bind the plan to the interfaces the host has now: each network goes to the
/// one interface holding its gateway address, or to `pending` if none does.
/// The plan is validated first, so an invalid one binds nothing.
pub fn bind(plan: &NetworkPlan, live: &LiveView) -> Result<WallSpec, BindError> {
    plan.validate()?;
    let mut networks: Vec<VmNetwork> = plan.networks.clone();
    networks.sort_by_key(|n| n.id);

    let mut bound = Vec::new();
    let mut pending = Vec::new();
    let mut taken: BTreeMap<IfName, u8> = BTreeMap::new();
    for mut net in networks {
        let holders: BTreeSet<&str> = live
            .interfaces
            .iter()
            .filter(|i| i.addrs.iter().any(|&(a, _)| a == net.gateway))
            .map(|i| i.name.as_str())
            .collect();
        let name = match holders.len() {
            0 => {
                pending.push(net.id);
                continue;
            }
            1 => holders.into_iter().next().unwrap_or_default(),
            _ => {
                let names: Vec<String> = holders
                    .iter()
                    .map(|h| h.escape_debug().to_string())
                    .collect();
                return Err(BindError::GatewayOnManyInterfaces {
                    net: net.id,
                    interfaces: names.join(", "),
                });
            }
        };
        let interface = IfName::parse(name).map_err(|source| BindError::BadInterfaceName {
            net: net.id,
            source,
        })?;
        if let Some(&a) = taken.get(&interface) {
            return Err(BindError::SharedInterface {
                a,
                b: net.id,
                interface,
            });
        }
        taken.insert(interface.clone(), net.id);
        net.slots.sort_by_key(|s| s.id);
        bound.push(BoundNetwork { net, interface });
    }

    let host_addrs: BTreeSet<Ipv4Addr> = live
        .interfaces
        .iter()
        .flat_map(|i| i.addrs.iter().map(|&(a, _)| a))
        .collect();
    Ok(WallSpec {
        networks: bound,
        host_addrs: host_addrs.into_iter().collect(),
        pending,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    pub(crate) fn net(s: &str) -> Ipv4Net {
        s.parse().unwrap()
    }

    /// Two networks of two slots each, as setup would write them.
    pub(crate) fn plan() -> NetworkPlan {
        NetworkPlan {
            networks: vec![
                VmNetwork {
                    id: 1,
                    gateway: ip("192.168.64.1"),
                    subnet: net("192.168.64.0/24"),
                    slots: vec![
                        VmSlot {
                            id: 1,
                            addr: ip("192.168.64.2"),
                            profiles: vec!["docker".into(), "docker-big".into()],
                        },
                        VmSlot {
                            id: 2,
                            addr: ip("192.168.64.3"),
                            profiles: vec!["docker".into()],
                        },
                    ],
                },
                VmNetwork {
                    id: 2,
                    gateway: ip("192.168.65.1"),
                    subnet: net("192.168.65.0/24"),
                    slots: vec![VmSlot {
                        id: 1,
                        addr: ip("192.168.65.2"),
                        profiles: vec!["macos-vm".into()],
                    }],
                },
            ],
        }
    }

    pub(crate) fn iface(name: &str, addrs: &[(&str, u8)]) -> HostInterface {
        HostInterface {
            name: name.into(),
            addrs: addrs.iter().map(|&(a, p)| (ip(a), p)).collect(),
        }
    }

    /// A host with both bridges up, a LAN address and loopback.
    pub(crate) fn live() -> LiveView {
        LiveView {
            interfaces: vec![
                iface("lo0", &[("127.0.0.1", 8)]),
                iface("en0", &[("192.0.2.10", 24)]),
                iface("bridge100", &[("192.168.64.1", 24)]),
                iface("bridge101", &[("192.168.65.1", 24)]),
            ],
        }
    }

    #[test]
    fn ifname_accepts_letters_then_digits() {
        for ok in ["bridge100", "en0", "lo0", "utun1234", "abcdefgh1"] {
            assert_eq!(IfName::parse(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn ifname_refuses_anything_that_could_change_a_rule() {
        for bad in [
            "",
            "bridge100 all",
            "en0;",
            "\"x\"",
            "en0\"",
            "brídge0",
            "bridge\u{0660}",
            "EN0",
            "en",
            "0",
            "en0a",
            "abcdefghi1",
            "en12345",
            "en-0",
            "en0\n",
            "en0/",
            "{en0}",
        ] {
            assert!(IfName::parse(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn ifname_error_escapes_what_it_quotes() {
        let e = IfName::parse("x\"\n").unwrap_err().to_string();
        assert!(!e.contains('\n'), "{e}");
        assert!(e.contains("\\\""), "{e}");
    }

    #[test]
    fn ipv4net_parse_and_arithmetic() {
        let n = net("192.168.64.0/24");
        assert_eq!(n.network(), ip("192.168.64.0"));
        assert_eq!(n.broadcast(), ip("192.168.64.255"));
        assert!(n.contains(ip("192.168.64.200")));
        assert!(!n.contains(ip("192.168.65.1")));
        assert_eq!(n.to_string(), "192.168.64.0/24");
        assert_eq!(net("255.255.255.255/32").to_string(), "255.255.255.255");
        let all = net("0.0.0.0/0");
        assert!(all.contains(ip("203.0.113.5")));
        assert_eq!(all.broadcast(), ip("255.255.255.255"));
        assert!(net("172.31.250.0/24").within(&net("172.16.0.0/12")));
        assert!(!net("172.16.0.0/12").within(&net("172.31.250.0/24")));
    }

    #[test]
    fn ipv4net_refuses_host_bits_and_bad_shapes() {
        assert!(matches!(
            "192.168.64.1/24".parse::<Ipv4Net>(),
            Err(NetParseError::HostBits(_))
        ));
        assert!(matches!(
            "10.0.0.0/33".parse::<Ipv4Net>(),
            Err(NetParseError::Prefix(33))
        ));
        for bad in [
            "10.0.0.0",
            "10.0.0.0/",
            "10.0.0.0/+8",
            "10.0.0/8",
            "x/8",
            "10.0.0.0/008",
        ] {
            assert!(bad.parse::<Ipv4Net>().is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn plan_round_trips_through_json() {
        let p = plan();
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"subnet\":\"192.168.64.0/24\""), "{json}");
        assert_eq!(NetworkPlan::from_json(json.as_bytes()).unwrap(), p);
        // A field this release does not know is refused, not skipped.
        let extra = r#"{"networks":[],"grants":[]}"#;
        assert!(NetworkPlan::from_json(extra.as_bytes()).is_err());
        // A subnet with host bits set is refused at parse.
        let hostbits = r#"{"networks":[{"id":1,"gateway":"192.168.64.1","subnet":"192.168.64.1/24","slots":[]}]}"#;
        assert!(NetworkPlan::from_json(hostbits.as_bytes()).is_err());
    }

    #[test]
    fn the_example_plan_is_valid() {
        assert_eq!(plan().validate(), Ok(()));
        assert_eq!(NetworkPlan::default().validate(), Ok(()));
    }

    fn with(edit: impl FnOnce(&mut NetworkPlan)) -> Result<(), PlanError> {
        let mut p = plan();
        edit(&mut p);
        p.validate()
    }

    #[test]
    fn each_plan_defect_is_its_own_error() {
        assert_eq!(
            with(|p| p.networks[1].id = 1),
            Err(PlanError::DuplicateNetworkId(1))
        );
        assert_eq!(
            with(|p| {
                p.networks[0].subnet = net("192.168.0.0/31");
            }),
            Err(PlanError::PrefixOutOfRange { net: 1, prefix: 31 })
        );
        assert_eq!(
            with(|p| p.networks[0].subnet = net("10.0.0.0/15")),
            Err(PlanError::PrefixOutOfRange { net: 1, prefix: 15 })
        );
        assert_eq!(
            with(|p| p.networks[0].subnet = net("203.0.113.0/24")),
            Err(PlanError::NotPrivate {
                net: 1,
                subnet: net("203.0.113.0/24")
            })
        );
        // Just either side of 172.16/12 is not inside it.
        for outside in [
            "172.15.0.0/16",
            "172.32.0.0/16",
            "192.169.0.0/16",
            "11.0.0.0/16",
        ] {
            assert!(
                matches!(
                    with(|p| p.networks[0].subnet = net(outside)),
                    Err(PlanError::NotPrivate { .. })
                ),
                "{outside}"
            );
        }
        assert_eq!(
            with(|p| p.networks[0].slots.clear()),
            Err(PlanError::NoSlots(1))
        );
        assert!(matches!(
            with(|p| p.networks[0].gateway = ip("192.168.65.1")),
            Err(PlanError::GatewayOutside { net: 1, .. })
        ));
        for reserved in ["192.168.64.0", "192.168.64.255"] {
            assert_eq!(
                with(|p| p.networks[0].gateway = ip(reserved)),
                Err(PlanError::GatewayReserved {
                    net: 1,
                    gateway: ip(reserved)
                })
            );
        }
        assert_eq!(
            with(|p| p.networks[0].slots[0].addr = ip("192.168.65.9")),
            Err(PlanError::SlotOutside {
                net: 1,
                slot: 1,
                addr: ip("192.168.65.9")
            })
        );
        for reserved in ["192.168.64.1", "192.168.64.0", "192.168.64.255"] {
            assert_eq!(
                with(|p| p.networks[0].slots[1].addr = ip(reserved)),
                Err(PlanError::SlotReserved {
                    net: 1,
                    slot: 2,
                    addr: ip(reserved)
                })
            );
        }
        assert_eq!(
            with(|p| p.networks[0].slots[1].id = 1),
            Err(PlanError::DuplicateSlotId { net: 1, slot: 1 })
        );
        assert_eq!(
            with(|p| p.networks[0].slots[1].addr = ip("192.168.64.2")),
            Err(PlanError::DuplicateSlotAddr {
                net: 1,
                addr: ip("192.168.64.2")
            })
        );
        assert_eq!(
            with(|p| {
                let n = &mut p.networks[1];
                n.subnet = net("192.168.0.0/16");
                n.gateway = ip("192.168.200.1");
                n.slots[0].addr = ip("192.168.200.2");
            }),
            Err(PlanError::Overlap { a: 1, b: 2 })
        );
        for bad in ["", "Docker", "a b", "-x", "x\"y"] {
            assert!(
                matches!(
                    with(|p| p.networks[1].slots[0].profiles = vec![bad.into()]),
                    Err(PlanError::BadProfile {
                        net: 2,
                        slot: 1,
                        ..
                    })
                ),
                "{bad:?}"
            );
        }
        assert_eq!(
            with(|p| p.networks[1].slots[0].profiles = vec!["a".into(), "a".into()]),
            Err(PlanError::DuplicateProfile {
                net: 2,
                slot: 1,
                name: "a".into()
            })
        );
        assert_eq!(with(|p| p.networks[1].slots[0].profiles.clear()), Ok(()));
    }

    #[test]
    fn bind_finds_each_gateways_interface() {
        let spec = bind(&plan(), &live()).unwrap();
        let got: Vec<(u8, &str)> = spec
            .networks
            .iter()
            .map(|b| (b.net.id, b.interface.as_str()))
            .collect();
        assert_eq!(got, vec![(1, "bridge100"), (2, "bridge101")]);
        assert!(spec.pending.is_empty());
        assert_eq!(
            spec.host_addrs,
            vec![
                ip("127.0.0.1"),
                ip("192.0.2.10"),
                ip("192.168.64.1"),
                ip("192.168.65.1")
            ]
        );
    }

    #[test]
    fn a_network_with_no_bridge_is_pending() {
        let mut l = live();
        l.interfaces.retain(|i| i.name != "bridge101");
        let spec = bind(&plan(), &l).unwrap();
        assert_eq!(spec.networks.len(), 1);
        assert_eq!(spec.pending, vec![2]);
        // A different address on the bridge is not the gateway.
        l.interfaces[2].addrs = vec![(ip("192.168.64.9"), 24)];
        assert_eq!(bind(&plan(), &l).unwrap().pending, vec![1, 2]);
    }

    #[test]
    fn host_addrs_are_deduplicated_and_sorted() {
        let mut l = live();
        l.interfaces.push(iface(
            "en1",
            &[("198.51.100.7", 24), ("192.0.2.10", 24), ("10.9.9.9", 8)],
        ));
        let spec = bind(&plan(), &l).unwrap();
        let mut sorted = spec.host_addrs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(spec.host_addrs, sorted);
        assert_eq!(spec.host_addrs.len(), 6);
    }

    #[test]
    fn bind_refuses_an_ambiguous_or_unsafe_binding() {
        let mut l = live();
        l.interfaces
            .push(iface("bridge102", &[("192.168.64.1", 24)]));
        assert_eq!(
            bind(&plan(), &l),
            Err(BindError::GatewayOnManyInterfaces {
                net: 1,
                interfaces: "bridge100, bridge102".into()
            })
        );

        let mut l = live();
        l.interfaces[2].name = "bridge100 all".into();
        assert!(matches!(
            bind(&plan(), &l),
            Err(BindError::BadInterfaceName { net: 1, .. })
        ));

        let mut l = live();
        l.interfaces[2].addrs.push((ip("192.168.65.1"), 24));
        l.interfaces.remove(3);
        assert_eq!(
            bind(&plan(), &l),
            Err(BindError::SharedInterface {
                a: 1,
                b: 2,
                interface: IfName::parse("bridge100").unwrap()
            })
        );

        let mut p = plan();
        p.networks[0].slots.clear();
        assert_eq!(
            bind(&p, &live()),
            Err(BindError::Plan(PlanError::NoSlots(1)))
        );
    }

    #[test]
    fn bind_sorts_networks_and_slots() {
        let mut p = plan();
        p.networks.reverse();
        p.networks[1].slots.reverse();
        let mut l = live();
        l.interfaces.reverse();
        assert_eq!(bind(&p, &l), bind(&plan(), &live()));
    }
}
