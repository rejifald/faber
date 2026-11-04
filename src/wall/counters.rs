//! Rule labels, and what their counters prove about a probe from a guest.
//!
//! ADR-0026: before a VM gets work, a probe sent from inside it must be
//! stopped by the anchor itself, and the daemon confirms that from the rule's
//! own counters. An unanswered probe proves nothing — the application firewall,
//! or a host that is simply down, would leave it unanswered too.
//!
//! Every rule carries a [`Label`], and pf keeps packet counts per rule. The
//! `host`, `dns`, `dnsblock`, `fence`, `grant`, `nonpublic` and `public`
//! labels name one reserved guest address (network `n`, slot `s`), so their
//! counters belong to one VM. `dhcp`, `inet6` and `rest` are per network: a
//! VM's share of them cannot be told apart, so [`prove`] reads only the
//! slot's own labels, and of those only `host` and `nonpublic`: a probe
//! targets a destination the wall blocks outright, never one a resolver or a
//! grant may open. The `fence` rules come before `nonpublic`, so a
//! [`ProbeTarget::Nonpublic`] probe must go to a range the fence does not
//! cover (`10.0.0.0/8`, say, outside every VM subnet): one to `127.0.0.1` or
//! a VM subnet is counted under `host` or `fence` instead.
//!
//! Even a slot's labels name a source address, not a VM. A neighbour on the
//! same bridge could send from the slot's address and raise its counters, so
//! a proof needs the network to hold exactly one slot: one VM per network,
//! which the launcher provides. A network with more is never proven.
//!
//! A reload resets every counter, and a host-table replace changes what the
//! `host` rules match, so a delta across either means nothing. The checker
//! stamps each with a generation (`load_generation` in the verdict file: the
//! checker's start time in unix nanoseconds, then the time of its last load
//! or replace), and two snapshots prove something only when they carry the
//! same one. A restarted checker starts a new generation, so a delta across
//! a restart is never trusted either.
//!
//! # How `_faber` gets the counters
//!
//! Only root can read pf's counters, and the root checker already reads every
//! anchor rule with its counters (`pfctl -a ANCHOR -vsr`) on each tick. On a
//! green tick it publishes them in the verdict file it writes anyway
//! ([`super::report::CounterReport`]): root writes, `_faber` reads, and the
//! file is trusted on the same terms as the verdict ([`super::report::judge_at`]).
//! Nothing `_faber` sends ever reaches the checker. The reader takes one
//! snapshot before the probe and one read after it ([`crate::vm::wall_proof`]).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::model::WallSpec;
use super::pfctl::RuleEntry;
use super::LABEL_PREFIX;

/// A label faber puts on a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Label {
    Dhcp { net: u8 },
    Inet6 { net: u8 },
    Rest { net: u8 },
    Host { net: u8, slot: u8 },
    Dns { net: u8, slot: u8 },
    DnsBlock { net: u8, slot: u8 },
    Fence { net: u8, slot: u8 },
    Grant { net: u8, slot: u8 },
    Nonpublic { net: u8, slot: u8 },
    Public { net: u8, slot: u8 },
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = LABEL_PREFIX;
        match *self {
            Label::Dhcp { net } => write!(f, "{p}/n{net}/dhcp"),
            Label::Inet6 { net } => write!(f, "{p}/n{net}/inet6"),
            Label::Rest { net } => write!(f, "{p}/n{net}/rest"),
            Label::Host { net, slot } => write!(f, "{p}/n{net}/s{slot}/host"),
            Label::Dns { net, slot } => write!(f, "{p}/n{net}/s{slot}/dns"),
            Label::DnsBlock { net, slot } => write!(f, "{p}/n{net}/s{slot}/dnsblock"),
            Label::Fence { net, slot } => write!(f, "{p}/n{net}/s{slot}/fence"),
            Label::Grant { net, slot } => write!(f, "{p}/n{net}/s{slot}/grant"),
            Label::Nonpublic { net, slot } => write!(f, "{p}/n{net}/s{slot}/nonpublic"),
            Label::Public { net, slot } => write!(f, "{p}/n{net}/s{slot}/public"),
        }
    }
}

impl Label {
    /// The label `s` names, or `None` for any text faber did not write in this
    /// exact form (a foreign label, a leading zero, an id over 255).
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix(LABEL_PREFIX)?.strip_prefix('/')?;
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            [n, kind] => {
                let net = id(n, 'n')?;
                match *kind {
                    "dhcp" => Some(Label::Dhcp { net }),
                    "inet6" => Some(Label::Inet6 { net }),
                    "rest" => Some(Label::Rest { net }),
                    _ => None,
                }
            }
            [n, s, kind] => {
                let (net, slot) = (id(n, 'n')?, id(s, 's')?);
                match *kind {
                    "host" => Some(Label::Host { net, slot }),
                    "dns" => Some(Label::Dns { net, slot }),
                    "dnsblock" => Some(Label::DnsBlock { net, slot }),
                    "fence" => Some(Label::Fence { net, slot }),
                    "grant" => Some(Label::Grant { net, slot }),
                    "nonpublic" => Some(Label::Nonpublic { net, slot }),
                    "public" => Some(Label::Public { net, slot }),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// `n12` → 12, in canonical decimal only, so parse and format round-trip.
fn id(part: &str, tag: char) -> Option<u8> {
    let digits = part.strip_prefix(tag)?;
    if digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    digits.parse().ok()
}

/// Packets per label, summed over the rules that share it (one slot's
/// `nonpublic` label is on sixteen rules), and the load they were read under.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// The checker's load generation when the counters were read.
    pub generation: u64,
    pub packets: BTreeMap<Label, u64>,
}

impl Snapshot {
    /// From `pfctl -a ANCHOR -vsr`, read under load `generation`. Rules with a
    /// foreign label, no label or no counters are left out, so a label with
    /// no counted rule is absent.
    pub fn from_rules(rules: &[RuleEntry], generation: u64) -> Self {
        let mut packets = BTreeMap::new();
        for r in rules {
            let (Some(label), Some(c)) = (r.label.as_deref().and_then(Label::parse), r.counters)
            else {
                continue;
            };
            let sum: &mut u64 = packets.entry(label).or_default();
            *sum = sum.saturating_add(c.packets);
        }
        Snapshot {
            generation,
            packets,
        }
    }

    pub fn get(&self, label: &Label) -> Option<u64> {
        self.packets.get(label).copied()
    }

    /// The packets keyed by each label's text, as the verdict file carries them.
    pub fn to_text(&self) -> BTreeMap<String, u64> {
        self.packets
            .iter()
            .map(|(l, &p)| (l.to_string(), p))
            .collect()
    }

    /// The inverse of [`Self::to_text`]. Every key must be a label faber
    /// writes, in its exact form: a key that is not is refused, never skipped.
    pub fn from_text(generation: u64, packets: &BTreeMap<String, u64>) -> Result<Self, String> {
        let mut out = BTreeMap::new();
        for (text, &p) in packets {
            let label = Label::parse(text)
                .ok_or_else(|| format!("{text:?} is not a label faber writes"))?;
            out.insert(label, p);
        }
        Ok(Snapshot {
            generation,
            packets: out,
        })
    }
}

/// A bound network's id and its slots' ids: all [`prove`] needs of a
/// [`WallSpec`], and what the checker publishes beside its counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundSlots {
    pub net: u8,
    pub slots: Vec<u8>,
}

/// Each bound network of `spec`, with its slots' ids. A pending network is
/// not bound, so it is not here.
pub fn bound_slots(spec: &WallSpec) -> Vec<BoundSlots> {
    spec.networks
        .iter()
        .map(|b| BoundSlots {
            net: b.net.id,
            slots: b.net.slots.iter().map(|s| s.id).collect(),
        })
        .collect()
}

/// Which blocked destination a probe was sent to. One target per pair of
/// snapshots. The delta on its label is every packet that rule dropped from
/// the slot between them: the probe's, and any other the guest sent to that
/// kind of destination meanwhile. It shows the rule dropping the slot's
/// traffic, not which packets were the probe's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeTarget {
    /// A host address: the slot's `host` rules must count it.
    Host,
    /// A non-global range: the slot's `nonpublic` rules must count it.
    Nonpublic,
}

/// What two snapshots around a probe prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proof {
    /// The target's rule counted at least as many packets from the slot as
    /// the probe sent: the anchor itself drops that traffic.
    Stopped,
    /// Nothing can be concluded; the VM gets no work.
    Unproven(String),
}

/// What the counters say about `sent` probe packets sent from network `net`,
/// slot `slot`, to `target`, between `before` and `after`.
///
/// There is no "leaked" verdict: the slot's `public` counter also counts the
/// guest's own traffic, so a rise there says nothing about the probe. A probe
/// that is not counted where it should be is unproven, which keeps the VM
/// from work just the same.
pub fn prove(
    before: &Snapshot,
    after: &Snapshot,
    spec: &WallSpec,
    net: u8,
    slot: u8,
    target: ProbeTarget,
    sent: u64,
) -> Proof {
    prove_bound(before, after, &bound_slots(spec), net, slot, target, sent)
}

/// [`prove`], given the bound networks as the checker published them.
pub fn prove_bound(
    before: &Snapshot,
    after: &Snapshot,
    bound: &[BoundSlots],
    net: u8,
    slot: u8,
    target: ProbeTarget,
    sent: u64,
) -> Proof {
    let unproven = Proof::Unproven;
    if sent == 0 {
        return unproven("no probe packets were sent".into());
    }
    let mut matching = bound.iter().filter(|b| b.net == net);
    let (Some(bound), None) = (matching.next(), matching.next()) else {
        return unproven(format!("network {net} is not bound exactly once"));
    };
    if !bound.slots.contains(&slot) {
        return unproven(format!("network {net} has no slot {slot}"));
    }
    if bound.slots.len() != 1 {
        return unproven(format!(
            "network {net} has {} slots: a neighbour could send from slot {slot}'s address",
            bound.slots.len()
        ));
    }
    if before.generation != after.generation {
        return unproven(format!(
            "the anchor was reloaded between the snapshots (load {} then {})",
            before.generation, after.generation
        ));
    }
    for (label, &b) in &before.packets {
        if let Some(a) = after.get(label) {
            if a < b {
                return unproven(format!(
                    "counter {label} went from {b} to {a}: the anchor was reloaded"
                ));
            }
        }
    }
    let label = match target {
        ProbeTarget::Host => Label::Host { net, slot },
        ProbeTarget::Nonpublic => Label::Nonpublic { net, slot },
    };
    let (Some(b), Some(a)) = (before.get(&label), after.get(&label)) else {
        return unproven(format!("no counters for {label}"));
    };
    let counted = a - b;
    if counted >= sent {
        Proof::Stopped
    } else {
        unproven(format!("{label} counted {counted} of {sent} probe packets"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::bind;
    use super::super::model::tests::{live, plan};
    use super::super::pfctl::{parse_rules, Counters};
    use super::*;

    fn all_kinds() -> Vec<Label> {
        let mut v = Vec::new();
        for net in [0, 1, 9, 10, 255] {
            v.extend([
                Label::Dhcp { net },
                Label::Inet6 { net },
                Label::Rest { net },
            ]);
            for slot in [0, 7, 100, 255] {
                v.extend([
                    Label::Host { net, slot },
                    Label::Dns { net, slot },
                    Label::DnsBlock { net, slot },
                    Label::Fence { net, slot },
                    Label::Grant { net, slot },
                    Label::Nonpublic { net, slot },
                    Label::Public { net, slot },
                ]);
            }
        }
        v
    }

    #[test]
    fn labels_round_trip() {
        for l in all_kinds() {
            assert_eq!(Label::parse(&l.to_string()), Some(l), "{l}");
        }
        assert_eq!(
            Label::Nonpublic { net: 2, slot: 3 }.to_string(),
            "faber-wall/n2/s3/nonpublic"
        );
    }

    #[test]
    fn foreign_and_non_canonical_labels_are_refused() {
        for bad in [
            "",
            "faber-wall",
            "faber-wall/",
            "faber-wall/n1",
            "faber-wall/n1/s1",
            "faber-wall/n01/dhcp",
            "faber-wall/n256/dhcp",
            "faber-wall/n-1/dhcp",
            "faber-wall/n+1/dhcp",
            "faber-wall/x1/dhcp",
            "faber-wall/n1/s1/dhcp",
            "faber-wall/n1/host",
            "faber-wall/n1/s1/host/extra",
            "faber-wall/n1/s/host",
            "faber-wall/n1/s1/HOST",
            "faber-wall/n1/dns",
            "faber-wall/n1/s1/grants",
            "faber-wall/n1/s1/dns-block",
            "faber-wall/n1/fence",
            "faber-wall/n1/s1/fences",
            "faber-wallx/n1/dhcp",
            "other/n1/dhcp",
            "com.apple/n1/dhcp",
            "faber-wall/n１/dhcp",
        ] {
            assert_eq!(Label::parse(bad), None, "{bad:?}");
        }
    }

    fn entry(label: &str, packets: u64) -> RuleEntry {
        RuleEntry {
            text: format!("block drop in quick on bridge100 all label \"{label}\""),
            label: Some(label.into()),
            counters: Some(Counters {
                evaluations: packets,
                packets,
                bytes: packets * 60,
                states: 0,
            }),
        }
    }

    fn snap(rows: &[(&str, u64)]) -> Snapshot {
        snap_at(7, rows)
    }

    fn snap_at(generation: u64, rows: &[(&str, u64)]) -> Snapshot {
        Snapshot::from_rules(
            &rows.iter().map(|&(l, p)| entry(l, p)).collect::<Vec<_>>(),
            generation,
        )
    }

    /// Network 2 of the test plan holds one slot; network 1 holds two.
    fn spec() -> WallSpec {
        bind(&plan(), &live()).unwrap()
    }

    const H: &str = "faber-wall/n2/s1/host";
    const NP: &str = "faber-wall/n2/s1/nonpublic";
    const P: &str = "faber-wall/n2/s1/public";

    fn go(before: &Snapshot, after: &Snapshot, target: ProbeTarget, sent: u64) -> Proof {
        prove(before, after, &spec(), 2, 1, target, sent)
    }

    #[test]
    fn rules_sharing_a_label_are_summed() {
        let s = snap(&[(NP, 2), (NP, 3), (H, 1), ("other", 9)]);
        assert_eq!(s.get(&Label::Nonpublic { net: 2, slot: 1 }), Some(5));
        assert_eq!(s.get(&Label::Host { net: 2, slot: 1 }), Some(1));
        assert_eq!(s.packets.len(), 2);
        assert_eq!(s.generation, 7);
    }

    #[test]
    fn a_snapshot_from_real_pfctl_output() {
        let out = "\
block drop in quick on bridge100 inet from 192.168.64.2 to 10.0.0.0/8 label \"faber-wall/n1/s1/nonpublic\"
  [ Evaluations: 40        Packets: 4         Bytes: 240         States: 0     ]
  [ Inserted: uid 0 pid 4242 State Creations: 0     ]
block drop in quick on bridge100 inet from 192.168.64.2 to 172.16.0.0/12 label \"faber-wall/n1/s1/nonpublic\"
  [ Evaluations: 36        Packets: 1         Bytes: 60          States: 0     ]
  [ Inserted: uid 0 pid 4242 State Creations: 0     ]
";
        let s = Snapshot::from_rules(&parse_rules(out).unwrap(), 1);
        assert_eq!(s.get(&Label::Nonpublic { net: 1, slot: 1 }), Some(5));
    }

    #[test]
    fn the_targets_own_label_rising_by_what_was_sent_is_stopped() {
        let before = snap(&[(H, 0), (NP, 3), (P, 10)]);
        let after = snap(&[(H, 2), (NP, 3), (P, 10)]);
        assert_eq!(go(&before, &after, ProbeTarget::Host, 2), Proof::Stopped);
        assert_eq!(go(&before, &after, ProbeTarget::Host, 1), Proof::Stopped);
        let after = snap(&[(H, 0), (NP, 4), (P, 12)]);
        assert_eq!(
            go(&before, &after, ProbeTarget::Nonpublic, 1),
            Proof::Stopped
        );
    }

    #[test]
    fn the_other_label_rising_proves_nothing() {
        let before = snap(&[(H, 0), (NP, 3), (P, 10)]);
        let after = snap(&[(H, 5), (NP, 3), (P, 10)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Nonpublic, 1),
            Proof::Unproven(m) if m.contains("0 of 1")
        ));
    }

    #[test]
    fn fewer_counted_than_sent_or_nothing_sent_proves_nothing() {
        let before = snap(&[(H, 0), (NP, 3), (P, 10)]);
        let after = snap(&[(H, 2), (NP, 3), (P, 10)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 3),
            Proof::Unproven(m) if m.contains("2 of 3")
        ));
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 0),
            Proof::Unproven(_)
        ));
    }

    #[test]
    fn public_traffic_alone_is_not_a_verdict() {
        let before = snap(&[(H, 0), (NP, 3), (P, 10)]);
        let after = snap(&[(H, 0), (NP, 3), (P, 11)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 1),
            Proof::Unproven(_)
        ));
    }

    #[test]
    fn a_decrease_means_a_reload_and_proves_nothing() {
        let before = snap(&[(H, 5), (NP, 3), (P, 10)]);
        let after = snap(&[(H, 6), (NP, 0), (P, 10)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 1),
            Proof::Unproven(m) if m.contains("reloaded")
        ));
        // Even on another network's label: the whole anchor was reloaded.
        let before = snap(&[(H, 0), (NP, 3), (P, 10), ("faber-wall/n1/rest", 9)]);
        let after = snap(&[(H, 1), (NP, 3), (P, 10), ("faber-wall/n1/rest", 0)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 1),
            Proof::Unproven(_)
        ));
    }

    #[test]
    fn a_different_load_generation_proves_nothing() {
        let before = snap_at(100, &[(H, 0), (NP, 3), (P, 10)]);
        let after = snap_at(200, &[(H, 9), (NP, 3), (P, 10)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 1),
            Proof::Unproven(m) if m.contains("reloaded")
        ));
        // A restarted checker starts a generation of its own, which differs too.
        let after = snap_at(300, &[(H, 9), (NP, 3), (P, 10)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 1),
            Proof::Unproven(_)
        ));
    }

    #[test]
    fn a_network_with_more_than_one_slot_is_never_proven() {
        let before = snap(&[("faber-wall/n1/s1/host", 0)]);
        let after = snap(&[("faber-wall/n1/s1/host", 9)]);
        assert!(matches!(
            prove(&before, &after, &spec(), 1, 1, ProbeTarget::Host, 1),
            Proof::Unproven(m) if m.contains("neighbour")
        ));
    }

    #[test]
    fn an_absent_label_or_unknown_slot_proves_nothing() {
        let full = snap(&[(H, 0), (NP, 3), (P, 10)]);
        let no_host = snap(&[(NP, 9), (P, 10)]);
        assert!(matches!(
            go(&no_host, &full, ProbeTarget::Host, 1),
            Proof::Unproven(_)
        ));
        assert!(matches!(
            go(&full, &no_host, ProbeTarget::Host, 1),
            Proof::Unproven(_)
        ));
        let after = snap(&[(H, 1), (NP, 3), (P, 10)]);
        for (net, slot) in [(2, 2), (3, 1)] {
            assert!(matches!(
                prove(&full, &after, &spec(), net, slot, ProbeTarget::Host, 1),
                Proof::Unproven(_)
            ));
        }
        // A pending network is not bound, so not proven either.
        let mut pending = spec();
        pending.networks.retain(|b| b.net.id != 2);
        assert!(matches!(
            prove(&full, &after, &pending, 2, 1, ProbeTarget::Host, 1),
            Proof::Unproven(_)
        ));
        // Per-network labels never prove a slot's probe.
        let before = snap(&[("faber-wall/n2/rest", 0)]);
        let after = snap(&[("faber-wall/n2/rest", 5)]);
        assert!(matches!(
            go(&before, &after, ProbeTarget::Host, 1),
            Proof::Unproven(_)
        ));
    }
}
