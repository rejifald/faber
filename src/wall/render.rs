//! The anchor's rules, rendered from the network plan and the grants into a
//! [`StoredWall`].
//!
//! Rendering needs no live facts: it runs when setup or `sudo faber grant`
//! stores the rules, never in the checker ([`super::stored`] says why). The two
//! things only the host knows right now stay open:
//!
//! - **The interface** is the literal token [`IF_TOKEN`], always right after
//!   `on `. [`super::stored::instantiate`] substitutes the bridge that holds
//!   the network's gateway.
//! - **The host's own addresses** are the table `<faber_host>`, which the
//!   checker fills from the addresses the host has now.
//!
//! Every rule is one line, already in the form `pfctl -a ANCHOR -sr` prints it
//! back: no macros, no `{}` lists, a single host without `/32`, and a table
//! reference as `<faber_host>`. So the text loaded and the text read back are
//! the same lines, and the checker compares them one by one instead of
//! re-implementing pf's normaliser. The forms were taken from `pfctl -nvf -` on
//! macOS, and a macOS-only test below pipes an instance through it to keep
//! them pinned.
//!
//! Per network, in order (networks by id, slots by id):
//!
//! 1. DHCP passes from `0.0.0.0` and from each reserved guest address, to
//!    the limited broadcast and to the gateway.
//! 2. All IPv6 is dropped.
//! 3. For each reserved guest address:
//!    1. **host**: everything to `<faber_host>` is dropped. First, on purpose:
//!       no resolver and no grant can open the host itself, even a grant whose
//!       network happens to contain one of the host's LAN addresses. The
//!       gateway is a host address, so DNS to it is dropped here too: its DNS
//!       proxy answers with the host's view of names, tailnet ones included
//!       (the spike measured it).
//!    2. **dns**: DNS (udp and tcp port 53) passes to each listed resolver.
//!    3. **dnsblock**: DNS to anywhere else is dropped, so a grant cannot open
//!       port 53 either; the resolvers change only through `[dns] resolvers`.
//!    4. **fence**: every VM subnet in the plan (the slot's own included, so
//!       its directed broadcast too), `0.0.0.0/8`, `127.0.0.0/8`, multicast
//!       `224.0.0.0/4` and `240.0.0.0/4` (which holds the limited broadcast)
//!       are dropped. Rendered for every slot, granted or not, so no grant —
//!       not `192.168.0.0/16`, not `0.0.0.0/0` — reaches another VM, a
//!       broadcast, or multicast such as mDNS.
//!    5. **grant**: each grant whose profile runs at this address passes to
//!       its network, port and protocol.
//!    6. **nonpublic**: every non-global range is dropped.
//!    7. **public**: anything else (public unicast) passes.
//! 4. Everything else on the interface is dropped, so a spoofed or unreserved
//!    source gets nothing.
//!
//! Every rule is `quick`, so the first match decides, and later anchors under
//! `com.apple/*` cannot loosen it. A grant naming a profile no slot lists
//! renders no rule ([`unmatched_profiles`] names them for the operator).

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use super::counters::Label;
use super::grants::{Grants, Proto};
use super::model::{Ipv4Net, NetworkPlan, PlanError, VmNetwork};
use super::stored::{StoredNetwork, StoredWall};

/// IANA's non-global IPv4 ranges, sorted. A guest reaches none of them unless
/// granted; what remains is public unicast. `224.0.0.0/4` is multicast and the
/// last two cover broadcast, other than the DHCP rules above them.
pub const NONPUBLIC: [(Ipv4Addr, u8); 16] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
    (Ipv4Addr::new(255, 255, 255, 255), 32),
];

/// Ranges no grant may open, besides the plan's own subnets
/// ([`fence_ranges`]): "this network", loopback, multicast and the reserved
/// block that holds the limited broadcast.
pub const FENCE: [(Ipv4Addr, u8); 4] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// The fence for `plan`: every network's subnet and [`FENCE`], sorted.
pub fn fence_ranges(plan: &NetworkPlan) -> BTreeSet<Ipv4Net> {
    plan.networks
        .iter()
        .map(|n| n.subnet)
        .chain(
            FENCE
                .iter()
                .map(|&(a, p)| Ipv4Net::new(a, p).expect("FENCE entries are networks")),
        )
        .collect()
}

/// pf's label buffer is 64 bytes with its NUL.
pub const LABEL_MAX: usize = 63;

/// Where the interface goes in a stored rule. It appears exactly once per
/// rule, right after `on `.
pub const IF_TOKEN: &str = "{if}";

/// The anchor's table of the host's own IPv4 addresses.
pub const HOST_TABLE: &str = "faber_host";

/// Render the rules for `plan` under `grants`. Deterministic: networks and
/// slots are sorted by id, resolvers and grant destinations sorted and
/// deduplicated, and nothing here reads a clock, the environment or the host.
pub fn render(plan: &NetworkPlan, grants: &Grants) -> Result<StoredWall, PlanError> {
    plan.validate()?;
    let mut networks: Vec<VmNetwork> = plan.networks.clone();
    networks.sort_by_key(|n| n.id);
    let resolvers: BTreeSet<Ipv4Addr> = grants.resolvers().iter().copied().collect();
    // Resolver passes come before the fence, so one inside a VM subnet would
    // open port 53 on another guest. Grants cannot see the plan; this can.
    for net in &networks {
        if let Some(&resolver) = resolvers.iter().find(|&&r| net.subnet.contains(r)) {
            return Err(PlanError::ResolverInsideSubnet {
                net: net.id,
                resolver,
                subnet: net.subnet,
            });
        }
    }
    let fence = fence_ranges(plan);
    let stored = networks
        .into_iter()
        .map(|mut net| {
            net.slots.sort_by_key(|s| s.id);
            let lines = network_lines(&net, &resolvers, &fence, grants);
            StoredNetwork {
                id: net.id,
                gateway: net.gateway,
                subnet: net.subnet,
                slots: net.slots,
                lines,
            }
        })
        .collect();
    Ok(StoredWall::seal(stored))
}

fn network_lines(
    net: &VmNetwork,
    resolvers: &BTreeSet<Ipv4Addr>,
    fence: &BTreeSet<Ipv4Net>,
    grants: &Grants,
) -> Vec<String> {
    let n = net.id;
    let on = format!("in quick on {IF_TOKEN}");
    let mut lines = Vec::new();
    // DHCP from a guest that has no address yet, and from each reserved
    // address renewing its lease: to the limited broadcast and to the
    // gateway. Never `from any`, which would let a spoofed source reach the
    // gateway's port 67.
    let dhcp = labelled(&Label::Dhcp { net: n });
    let sources = std::iter::once(Ipv4Addr::UNSPECIFIED).chain(net.slots.iter().map(|s| s.addr));
    for from in sources {
        for to in [Ipv4Addr::BROADCAST, net.gateway] {
            lines.push(format!(
                "pass {on} inet proto udp from {from} port = 68 to {to} port = 67 keep state label \"{dhcp}\""
            ));
        }
    }
    lines.push(format!(
        "block drop {on} inet6 all label \"{}\"",
        labelled(&Label::Inet6 { net: n })
    ));
    for slot in &net.slots {
        let (a, s) = (slot.addr, slot.id);
        let host = labelled(&Label::Host { net: n, slot: s });
        lines.push(format!(
            "block drop {on} inet from {a} to <{HOST_TABLE}> label \"{host}\""
        ));
        let dns = labelled(&Label::Dns { net: n, slot: s });
        for r in resolvers {
            lines.push(pass(&on, a, &r.to_string(), 53, Proto::Udp, &dns));
            lines.push(pass(&on, a, &r.to_string(), 53, Proto::Tcp, &dns));
        }
        let dnsblock = labelled(&Label::DnsBlock { net: n, slot: s });
        for proto in [Proto::Udp, Proto::Tcp] {
            lines.push(format!(
                "block drop {on} inet proto {proto} from {a} to any port = 53 label \"{dnsblock}\""
            ));
        }
        // The wall's own invariants, ahead of any grant: no VM reaches
        // another, a broadcast or multicast, whatever a grant's network holds.
        let fenced = labelled(&Label::Fence { net: n, slot: s });
        for range in fence {
            lines.push(format!(
                "block drop {on} inet from {a} to {range} label \"{fenced}\""
            ));
        }
        let granted: BTreeSet<(Ipv4Net, u16, Proto)> = grants
            .grants()
            .iter()
            .filter(|g| slot.profiles.contains(&g.profile))
            .map(|g| (g.to, g.port, g.proto))
            .collect();
        let grant = labelled(&Label::Grant { net: n, slot: s });
        for (to, port, proto) in granted {
            // pf prints `0.0.0.0/0` back as `any`.
            let to = if to.prefix() == 0 {
                "any".to_string()
            } else {
                to.to_string()
            };
            lines.push(pass(&on, a, &to, port, proto, &grant));
        }
        let nonpublic = labelled(&Label::Nonpublic { net: n, slot: s });
        for (addr, prefix) in NONPUBLIC {
            let range = Ipv4Net::new(addr, prefix).expect("NONPUBLIC entries are networks");
            lines.push(format!(
                "block drop {on} inet from {a} to {range} label \"{nonpublic}\""
            ));
        }
        lines.push(format!(
            "pass {on} inet from {a} to any flags S/SA keep state label \"{}\"",
            labelled(&Label::Public { net: n, slot: s })
        ));
    }
    lines.push(format!(
        "block drop {on} all label \"{}\"",
        labelled(&Label::Rest { net: n })
    ));
    lines
}

/// The lines [`render`] stores for `net`, a network of `plan`, when no grant
/// applies to any of its slots: what a network whose slots list no profile
/// always gets, whatever the grants file holds. `resolvers` are the `[dns]
/// resolvers` the set was rendered with. Setup compares the Linux image's
/// build network against it before it boots the build VM
/// ([`crate::setup::linux_image::wall_verdict`]).
pub fn ungranted_lines(
    net: &VmNetwork,
    resolvers: &BTreeSet<Ipv4Addr>,
    plan: &NetworkPlan,
) -> Vec<String> {
    let mut net = net.clone();
    net.slots.sort_by_key(|s| s.id);
    network_lines(&net, resolvers, &fence_ranges(plan), &Grants::default())
}

/// A pass to one destination port, in pf's printed form: tcp keeps state
/// from the SYN, udp keeps state from the first packet.
fn pass(on: &str, from: Ipv4Addr, to: &str, port: u16, proto: Proto, label: &str) -> String {
    let flags = match proto {
        Proto::Tcp => "flags S/SA ",
        Proto::Udp => "",
    };
    format!(
        "pass {on} inet proto {proto} from {from} to {to} port = {port} {flags}keep state label \"{label}\""
    )
}

/// Grant profiles no slot in `plan` lists, sorted. Not an error: the grant
/// renders no rule, and the operator is told.
pub fn unmatched_profiles(plan: &NetworkPlan, grants: &Grants) -> Vec<String> {
    let listed: BTreeSet<&str> = plan
        .networks
        .iter()
        .flat_map(|n| &n.slots)
        .flat_map(|s| s.profiles.iter().map(String::as_str))
        .collect();
    let named: BTreeSet<&str> = grants.grants().iter().map(|g| g.profile.as_str()).collect();
    named.difference(&listed).map(|s| s.to_string()).collect()
}

/// A label's text, held to pf's limit. Ids are `u8`s, so the longest label is
/// 30 bytes; the assertion is the guard against a future label that is not.
fn labelled(label: &Label) -> String {
    let s = label.to_string();
    assert!(s.len() <= LABEL_MAX, "pf label over {LABEL_MAX} bytes: {s}");
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::model::tests::plan;
    use super::*;

    pub(crate) const GRANTS: &str = r#"
[[grant]]
profile = "macos-vm"
to = "10.0.0.5/32"
port = 5432
proto = "tcp"

[[grant]]
profile = "docker"
to = "100.64.0.0/10"
port = 8443
proto = "udp"

[[grant]]
profile = "nobody-runs-this"
to = "10.9.0.0/16"
port = 22
proto = "tcp"
"#;

    pub(crate) fn grants() -> Grants {
        Grants::parse(GRANTS).unwrap()
    }

    pub(crate) fn stored() -> StoredWall {
        render(&plan(), &grants()).unwrap()
    }

    /// A full plan, 16 networks, renders to more than the small read cap
    /// (around 100 KB) and well within the cap the stored rules are read with,
    /// so they are never read cut off.
    #[test]
    fn a_full_plan_fits_the_stored_read_cap() {
        use super::super::model::tests::{ip, net};
        use super::super::paths::{READ_CAP, STORED_READ_CAP};
        let mut full = plan();
        let base = full.networks[0].clone();
        full.networks = (1..=16u8)
            .map(|i| {
                let third = 200 + u16::from(i);
                let mut n = base.clone();
                n.id = i;
                n.gateway = ip(&format!("192.168.{third}.1"));
                n.subnet = net(&format!("192.168.{third}.0/24"));
                for (j, s) in n.slots.iter_mut().enumerate() {
                    s.addr = ip(&format!("192.168.{third}.{}", 2 + j));
                }
                n
            })
            .collect();
        let bytes = render(&full, &grants()).unwrap().to_json().len() as u64;
        assert!(bytes > READ_CAP / 4, "{bytes} bytes: not a full plan");
        assert!(
            bytes * 4 < STORED_READ_CAP,
            "{bytes} bytes: too close to the cap"
        );
    }

    fn lines_of(net: u8) -> Vec<String> {
        stored()
            .networks
            .iter()
            .find(|n| n.id == net)
            .unwrap()
            .lines
            .clone()
    }

    #[test]
    fn rules_come_in_the_documented_order() {
        // Network 2: one slot, profile macos-vm, one grant.
        let n2 = lines_of(2);
        let on = "in quick on {if}";
        let dhcp = "keep state label \"faber-wall/n2/dhcp\"";
        let want_head = [
            format!("pass {on} inet proto udp from 0.0.0.0 port = 68 to 255.255.255.255 port = 67 {dhcp}"),
            format!("pass {on} inet proto udp from 0.0.0.0 port = 68 to 192.168.65.1 port = 67 {dhcp}"),
            format!("pass {on} inet proto udp from 192.168.65.2 port = 68 to 255.255.255.255 port = 67 {dhcp}"),
            format!("pass {on} inet proto udp from 192.168.65.2 port = 68 to 192.168.65.1 port = 67 {dhcp}"),
            format!("block drop {on} inet6 all label \"faber-wall/n2/inet6\""),
            format!("block drop {on} inet from 192.168.65.2 to <faber_host> label \"faber-wall/n2/s1/host\""),
            format!("pass {on} inet proto udp from 192.168.65.2 to 1.0.0.1 port = 53 keep state label \"faber-wall/n2/s1/dns\""),
            format!("pass {on} inet proto tcp from 192.168.65.2 to 1.0.0.1 port = 53 flags S/SA keep state label \"faber-wall/n2/s1/dns\""),
            format!("pass {on} inet proto udp from 192.168.65.2 to 1.1.1.1 port = 53 keep state label \"faber-wall/n2/s1/dns\""),
            format!("pass {on} inet proto tcp from 192.168.65.2 to 1.1.1.1 port = 53 flags S/SA keep state label \"faber-wall/n2/s1/dns\""),
            format!("block drop {on} inet proto udp from 192.168.65.2 to any port = 53 label \"faber-wall/n2/s1/dnsblock\""),
            format!("block drop {on} inet proto tcp from 192.168.65.2 to any port = 53 label \"faber-wall/n2/s1/dnsblock\""),
            format!("block drop {on} inet from 192.168.65.2 to 0.0.0.0/8 label \"faber-wall/n2/s1/fence\""),
            format!("block drop {on} inet from 192.168.65.2 to 127.0.0.0/8 label \"faber-wall/n2/s1/fence\""),
            format!("block drop {on} inet from 192.168.65.2 to 192.168.64.0/24 label \"faber-wall/n2/s1/fence\""),
            format!("block drop {on} inet from 192.168.65.2 to 192.168.65.0/24 label \"faber-wall/n2/s1/fence\""),
            format!("block drop {on} inet from 192.168.65.2 to 224.0.0.0/4 label \"faber-wall/n2/s1/fence\""),
            format!("block drop {on} inet from 192.168.65.2 to 240.0.0.0/4 label \"faber-wall/n2/s1/fence\""),
            format!("pass {on} inet proto tcp from 192.168.65.2 to 10.0.0.5 port = 5432 flags S/SA keep state label \"faber-wall/n2/s1/grant\""),
            format!("block drop {on} inet from 192.168.65.2 to 0.0.0.0/8 label \"faber-wall/n2/s1/nonpublic\""),
        ];
        assert_eq!(&n2[..want_head.len()], want_head);
        let tail = &n2[want_head.len() - 1..];
        assert_eq!(tail.len(), NONPUBLIC.len() + 2);
        assert_eq!(
            tail[NONPUBLIC.len() - 1],
            format!("block drop {on} inet from 192.168.65.2 to 255.255.255.255 label \"faber-wall/n2/s1/nonpublic\"")
        );
        assert_eq!(
            tail[NONPUBLIC.len()],
            format!("pass {on} inet from 192.168.65.2 to any flags S/SA keep state label \"faber-wall/n2/s1/public\"")
        );
        assert_eq!(
            tail[NONPUBLIC.len() + 1],
            format!("block drop {on} all label \"faber-wall/n2/rest\"")
        );
    }

    #[test]
    fn a_grant_applies_only_to_slots_listing_its_profile() {
        let n1 = lines_of(1);
        let granted: Vec<&String> = n1.iter().filter(|l| l.contains("/grant\"")).collect();
        // Both slots of network 1 run "docker"; neither runs "macos-vm".
        assert_eq!(granted.len(), 2);
        assert!(granted
            .iter()
            .all(|l| l.contains("inet proto udp from 192.168.64.")
                && l.contains("to 100.64.0.0/10 port = 8443 keep state")));
        assert!(!n1.iter().any(|l| l.contains("5432")));
        assert!(!lines_of(2).iter().any(|l| l.contains("8443")));
        // The unmatched grant renders nowhere, and is named.
        let all: Vec<String> = stored()
            .networks
            .into_iter()
            .flat_map(|n| n.lines)
            .collect();
        assert!(!all.iter().any(|l| l.contains("10.9.0.0/16")));
        assert_eq!(
            unmatched_profiles(&plan(), &grants()),
            vec!["nobody-runs-this"]
        );
        assert!(unmatched_profiles(&plan(), &Grants::default()).is_empty());
    }

    #[test]
    fn the_host_block_comes_before_every_pass_for_the_slot() {
        // So a grant whose network holds a host address cannot open it.
        let g = Grants::parse(
            "[[grant]]\nprofile = \"macos-vm\"\nto = \"192.0.2.0/24\"\nport = 22\nproto = \"tcp\"\n",
        )
        .unwrap();
        let s = render(&plan(), &g).unwrap();
        let n2 = &s.networks[1].lines;
        let host = n2.iter().position(|l| l.ends_with("/s1/host\"")).unwrap();
        let grant = n2
            .iter()
            .position(|l| l.contains("to 192.0.2.0/24 port = 22"))
            .unwrap();
        assert!(host < grant);
        for net in [1, 2] {
            let lines = lines_of(net);
            // DHCP to the gateway's port 67 is the one pass ahead of it.
            for (i, l) in lines.iter().enumerate() {
                if !l.starts_with("pass") || l.contains("port = 68 to") {
                    continue;
                }
                let from = l
                    .split(" from ")
                    .nth(1)
                    .and_then(|r| r.split(' ').next())
                    .unwrap();
                let host = lines
                    .iter()
                    .position(|h| h.contains(&format!("from {from} to <faber_host>")))
                    .unwrap();
                assert!(host < i, "{l}");
            }
        }
    }

    #[test]
    fn a_resolver_inside_a_vm_subnet_is_refused() {
        // plan() has 192.168.64.0/24; .1 is the gateway, .50 another guest's.
        for inside in ["192.168.64.1", "192.168.64.50", "192.168.64.255"] {
            let g = Grants::parse(&format!("[dns]\nresolvers = [\"{inside}\"]\n")).unwrap();
            assert!(
                matches!(
                    render(&plan(), &g),
                    Err(PlanError::ResolverInsideSubnet { .. })
                ),
                "{inside}"
            );
        }
        let g = Grants::parse("[dns]\nresolvers = [\"192.168.66.1\"]\n").unwrap();
        assert!(render(&plan(), &g).is_ok());
    }

    #[test]
    fn dns_passes_only_to_the_resolvers_and_never_to_the_gateway() {
        let all: Vec<String> = stored()
            .networks
            .into_iter()
            .flat_map(|n| n.lines)
            .collect();
        let dns_pass: Vec<&String> = all
            .iter()
            .filter(|l| l.starts_with("pass") && l.contains("port = 53"))
            .collect();
        // Three slots, two resolvers, udp and tcp.
        assert_eq!(dns_pass.len(), 3 * 2 * 2);
        assert!(dns_pass
            .iter()
            .all(|l| l.contains("to 1.1.1.1 port") || l.contains("to 1.0.0.1 port")));
        assert!(!all
            .iter()
            .any(|l| l.starts_with("pass") && l.contains("to 192.168.64.1 port = 53")));
        // A replaced list is all that passes.
        let g = Grants::parse("[dns]\nresolvers = [\"192.0.2.53\"]\n").unwrap();
        let s = render(&plan(), &g).unwrap();
        let all: Vec<String> = s.networks.into_iter().flat_map(|n| n.lines).collect();
        assert!(all
            .iter()
            .filter(|l| l.contains("port = 53") && l.starts_with("pass"))
            .all(|l| l.contains("to 192.0.2.53 port")));
        assert!(!all.iter().any(|l| l.contains("1.1.1.1")));
    }

    #[test]
    fn dhcp_never_passes_from_any() {
        for n in stored().networks {
            for line in n.lines {
                assert!(!line.contains("from any"), "{line}");
            }
        }
    }

    #[test]
    fn every_rule_is_quick_on_the_interface_token_with_no_lists_or_macros() {
        for n in stored().networks {
            for line in n.lines {
                assert!(
                    line.starts_with("pass in quick on {if} ")
                        || line.starts_with("block drop in quick on {if} "),
                    "{line}"
                );
                assert_eq!(line.matches(IF_TOKEN).count(), 1, "{line}");
                let rest = line.replacen(IF_TOKEN, "", 1);
                for forbidden in ["{", "}", "$", "/32", "(self)", "\n"] {
                    assert!(!rest.contains(forbidden), "{line}");
                }
                // The only table is ours.
                let without_table = rest.replace(&format!("<{HOST_TABLE}>"), "");
                assert!(!without_table.contains(['<', '>']), "{line}");
            }
        }
    }

    #[test]
    fn rendering_is_deterministic_whatever_the_input_order() {
        let mut p = plan();
        p.networks.reverse();
        for n in &mut p.networks {
            n.slots.reverse();
        }
        let mut text: Vec<&str> = GRANTS.split("\n\n").collect();
        text.reverse();
        let g = Grants::parse(&text.join("\n\n")).unwrap();
        let g2 = Grants::parse("[dns]\nresolvers = [\"1.0.0.1\", \"1.1.1.1\"]\n").unwrap();
        assert_eq!(render(&p, &g).unwrap(), stored());
        assert_eq!(
            render(&plan(), &g2).unwrap().digest,
            render(&plan(), &Grants::default()).unwrap().digest
        );
    }

    #[test]
    fn a_grant_changes_the_digest() {
        assert_ne!(
            stored().digest,
            render(&plan(), &Grants::default()).unwrap().digest
        );
    }

    #[test]
    fn an_invalid_plan_renders_nothing() {
        let mut p = plan();
        p.networks[0].slots.clear();
        assert_eq!(render(&p, &grants()), Err(PlanError::NoSlots(1)));
    }

    #[test]
    fn the_longest_label_fits_pf() {
        let (net, slot) = (u8::MAX, u8::MAX);
        for l in [
            Label::Nonpublic { net, slot },
            Label::Fence { net, slot },
            Label::DnsBlock { net, slot },
            Label::Public { net, slot },
            Label::Inet6 { net },
        ] {
            assert!(labelled(&l).len() <= LABEL_MAX);
        }
        for n in stored().networks {
            for line in n.lines {
                let label = line
                    .rsplit_once(" label \"")
                    .unwrap()
                    .1
                    .trim_end_matches('"');
                assert!(label.len() <= LABEL_MAX, "{label}");
                assert!(Label::parse(label).is_some(), "{label}");
            }
        }
    }

    #[test]
    fn the_fence_comes_before_every_grant_and_holds_with_no_grants() {
        // The widest grants there are: neither may reach another VM, a
        // broadcast or multicast.
        let g = Grants::parse(
            "[[grant]]\nprofile = \"docker\"\nto = \"0.0.0.0/0\"\nport = 5353\nproto = \"udp\"\n\n\
             [[grant]]\nprofile = \"docker\"\nto = \"192.168.0.0/16\"\nport = 22\nproto = \"tcp\"\n",
        )
        .unwrap();
        let fence: Vec<String> = fence_ranges(&plan())
            .iter()
            .map(|r| r.to_string())
            .collect();
        assert_eq!(
            fence,
            [
                "0.0.0.0/8",
                "127.0.0.0/8",
                "192.168.64.0/24",
                "192.168.65.0/24",
                "224.0.0.0/4",
                "240.0.0.0/4"
            ]
        );
        for grants in [g, Grants::default()] {
            let s = render(&plan(), &grants).unwrap();
            for n in &s.networks {
                for slot in &n.slots {
                    let a = slot.addr;
                    let label = format!("faber-wall/n{}/s{}/fence", n.id, slot.id);
                    let fenced: Vec<usize> = fence
                        .iter()
                        .map(|r| {
                            let want = format!(
                                "block drop in quick on {{if}} inet from {a} to {r} label \"{label}\""
                            );
                            n.lines.iter().position(|l| *l == want).expect(&want)
                        })
                        .collect();
                    // Contiguous, after dnsblock, before every grant.
                    let dnsblock = n
                        .lines
                        .iter()
                        .rposition(|l| l.contains(&format!("from {a} to any port = 53 label")))
                        .unwrap();
                    assert!(fenced.windows(2).all(|w| w[1] == w[0] + 1));
                    assert_eq!(fenced[0], dnsblock + 1);
                    let last = *fenced.last().unwrap();
                    for (i, l) in n.lines.iter().enumerate() {
                        if l.contains(&format!("from {a} ")) && l.ends_with("/grant\"") {
                            assert!(i > last, "{l}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn nonpublic_is_sorted_and_each_entry_a_network() {
        let nets: Vec<Ipv4Net> = NONPUBLIC
            .iter()
            .map(|&(a, p)| Ipv4Net::new(a, p).unwrap())
            .collect();
        let mut sorted = nets.clone();
        sorted.sort();
        assert_eq!(nets, sorted);
    }

    /// Pins the canonical form: `pfctl -nvf -` parses without root and prints
    /// each rule as pf would store it, and the table definition as written.
    /// Its stdout must be our table line, then our lines, exactly.
    #[cfg(target_os = "macos")]
    #[test]
    fn pfctl_prints_the_instance_back_unchanged() {
        use crate::wall::model::tests::{iface, live};

        let mut l = live();
        l.interfaces.push(iface("en1", &[("203.0.113.5", 24)]));
        l.interfaces
            .push(iface("bridge102", &[("172.31.250.1", 24)]));
        let widest = Grants::parse(
            "[[grant]]\nprofile = \"docker\"\nto = \"0.0.0.0/0\"\nport = 5353\nproto = \"udp\"\n",
        )
        .unwrap();
        for grants in [grants(), widest] {
            round_trip(&render(&plan(), &grants).unwrap(), &l);
        }
    }

    #[cfg(target_os = "macos")]
    fn round_trip(stored: &StoredWall, l: &crate::wall::model::LiveView) {
        use std::io::Write;
        use std::process::{Command, Stdio};

        use crate::wall::stored::instantiate;

        let inst = instantiate(stored, l).unwrap();
        let mut child = Command::new("/sbin/pfctl")
            .args(["-nvf", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn /sbin/pfctl");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(inst.text.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let printed: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
        let table_line = inst.text.lines().find(|l| l.starts_with("table ")).unwrap();
        assert_eq!(printed[0], table_line);
        assert_eq!(
            &printed[1..],
            inst.lines.iter().map(String::as_str).collect::<Vec<_>>()
        );
    }
}
