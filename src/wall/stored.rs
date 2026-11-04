//! The stored rule set, and its binding to the host as it is now.
//!
//! ADR-0026 (amended 2026-10-17): the anchor daemon enforces rules that setup
//! or `sudo faber grant` rendered and stored root-owned, never what its own
//! code would render now. So new code alone never changes what a guest can
//! reach: a release that renders differently changes nothing until an
//! operator's sudo act stores the new rules.
//!
//! Two facts cannot be stored, because they change while the rules stand:
//!
//! - which bridge holds each network's gateway (vmnet makes one only while a
//!   VM on it runs), and
//! - the host's own IPv4 addresses.
//!
//! [`instantiate`] supplies exactly those. It substitutes the bridge for
//! [`IF_TOKEN`] in each stored line and fills the `<faber_host>` table, and
//! produces no rule text of its own. Everything else is fixed at store time.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::model::{
    bind, BindError, Ipv4Net, LiveView, NetworkPlan, PlanError, VmNetwork, VmSlot, WallSpec,
};
use super::render::{HOST_TABLE, IF_TOKEN};
use super::RULES_VERSION;

/// The only schema this release writes and reads.
pub const STORED_SCHEMA: u32 = 1;

/// One network's stored rules: its plan entry, and its rule lines in
/// canonical form with [`IF_TOKEN`] where the interface goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredNetwork {
    pub id: u8,
    pub gateway: Ipv4Addr,
    pub subnet: Ipv4Net,
    /// The reserved guest addresses, for killing states and for counters.
    pub slots: Vec<VmSlot>,
    pub lines: Vec<String>,
}

/// The rule set root stores and the checker enforces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredWall {
    pub schema: u32,
    /// The rules version that rendered this set ([`RULES_VERSION`] then).
    pub rules_version: u32,
    /// [`StoredWall::digest_of`] the networks.
    pub digest: String,
    pub networks: Vec<StoredNetwork>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoredError {
    #[error("the stored wall does not parse: {0}")]
    Json(String),
    #[error("the stored wall has schema {0}, and this faber reads only {STORED_SCHEMA}")]
    Schema(u32),
    #[error("the stored wall's digest does not match its rules")]
    Digest,
    #[error("the stored wall's plan is invalid: {0}")]
    Plan(#[from] PlanError),
    #[error("network {net}, rule {index}: {why}")]
    Line {
        net: u8,
        index: usize,
        why: &'static str,
    },
}

impl StoredWall {
    /// Seal `networks` as rendered by this release.
    pub(crate) fn seal(networks: Vec<StoredNetwork>) -> Self {
        Self {
            schema: STORED_SCHEMA,
            rules_version: RULES_VERSION,
            digest: Self::digest_of(&networks),
            networks,
        }
    }

    /// SHA-256, lowercase hex, of the compact JSON encoding of `networks` as
    /// serde_json writes it: fields in declaration order, lists in order, no
    /// whitespace. That covers every line and everything the binding reads.
    pub fn digest_of(networks: &[StoredNetwork]) -> String {
        let bytes = serde_json::to_vec(networks).expect("stored networks always serialize");
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Parse a stored wall, and refuse it unless its schema is known, its
    /// digest matches and it [`validate`](Self::validate)s.
    pub fn from_json(bytes: &[u8]) -> Result<Self, StoredError> {
        let s: Self =
            serde_json::from_slice(bytes).map_err(|e| StoredError::Json(e.to_string()))?;
        if s.schema != STORED_SCHEMA {
            return Err(StoredError::Schema(s.schema));
        }
        if s.digest != Self::digest_of(&s.networks) {
            return Err(StoredError::Digest);
        }
        s.validate()?;
        Ok(s)
    }

    pub fn to_json(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("a StoredWall always serializes");
        out.push(b'\n');
        out
    }

    /// The plan is valid, and every line is one quick rule on [`IF_TOKEN`],
    /// with the token exactly once, right after `on `, and no other braces.
    pub fn validate(&self) -> Result<(), StoredError> {
        self.plan().validate()?;
        for n in &self.networks {
            for (index, line) in n.lines.iter().enumerate() {
                check_line(line).map_err(|why| StoredError::Line {
                    net: n.id,
                    index,
                    why,
                })?;
            }
        }
        Ok(())
    }

    /// The networks as a plan, for validation and binding.
    pub fn plan(&self) -> NetworkPlan {
        NetworkPlan {
            networks: self
                .networks
                .iter()
                .map(|n| VmNetwork {
                    id: n.id,
                    gateway: n.gateway,
                    subnet: n.subnet,
                    slots: n.slots.clone(),
                })
                .collect(),
        }
    }
}

fn check_line(line: &str) -> Result<(), &'static str> {
    if line.contains(['\n', '\r', '\0']) {
        return Err("holds a line break or NUL");
    }
    if line.matches(IF_TOKEN).count() != 1 {
        return Err("does not name the interface token exactly once");
    }
    let on = format!(" in quick on {IF_TOKEN} ");
    if !(line.starts_with(&format!("pass{on}")) || line.starts_with(&format!("block drop{on}"))) {
        return Err("is not a quick pass or block on the interface token");
    }
    if line.replacen(IF_TOKEN, "", 1).contains(['{', '}']) {
        return Err("holds a list or a stray brace");
    }
    Ok(())
}

/// A stored wall bound to the host as it is now: what the checker loads and
/// compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    /// The binding: each bound network's interface, the host addresses that
    /// fill `<faber_host>`, and the networks still pending.
    pub spec: WallSpec,
    /// The rules, in order, as `pfctl -sr` prints them: the stored lines of
    /// each bound network, by id, with the interface substituted.
    pub lines: Vec<String>,
    /// What is piped to `pfctl -a ANCHOR -f -`: a comment header, the table
    /// definition holding the host addresses, then [`Self::lines`].
    pub text: String,
}

/// Bind `stored` to `live`: each network to the one interface holding its
/// gateway (pending if none does; an error if more than one does, or the
/// name is not a safe [`super::model::IfName`]), then substitute.
///
/// The table definition carries the host addresses itself, so a load never
/// leaves the host rules matching an empty table, not even for the moment
/// before a separate replace.
pub fn instantiate(stored: &StoredWall, live: &LiveView) -> Result<Instance, BindError> {
    let spec = bind(&stored.plan(), live)?;
    let mut lines = Vec::new();
    for bound in &spec.networks {
        let Some(n) = stored.networks.iter().find(|n| n.id == bound.net.id) else {
            continue;
        };
        let on = format!("on {}", bound.interface);
        lines.extend(
            n.lines
                .iter()
                .map(|l| l.replacen(&format!("on {IF_TOKEN}"), &on, 1)),
        );
    }
    let mut text = format!(
        "# faber VM wall (ADR-0026), rules version {}, stored digest {}.\n\
         # Loaded by faber's root checker; it reloads this anchor whenever it differs.\n",
        stored.rules_version, stored.digest
    );
    text.push_str(&table_line(&spec.host_addrs));
    text.push('\n');
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    Ok(Instance { spec, lines, text })
}

/// `table <faber_host> persist { A B … }`, as `pfctl -nvf` prints it back.
fn table_line(addrs: &[Ipv4Addr]) -> String {
    if addrs.is_empty() {
        return format!("table <{HOST_TABLE}> persist");
    }
    let list: Vec<String> = addrs.iter().map(Ipv4Addr::to_string).collect();
    format!("table <{HOST_TABLE}> persist {{ {} }}", list.join(" "))
}

#[cfg(test)]
mod tests {
    use super::super::grants::Grants;
    use super::super::model::tests::{iface, ip, live, plan};
    use super::super::model::LiveView;
    use super::super::render::render;
    use super::super::render::tests::stored;
    use super::*;

    #[test]
    fn instance_lines_are_the_stored_lines_with_the_interface_substituted() {
        let s = stored();
        let inst = instantiate(&s, &live()).unwrap();
        let want: Vec<String> = s
            .networks
            .iter()
            .zip(["bridge100", "bridge101"])
            .flat_map(|(n, ifname)| n.lines.iter().map(move |l| l.replace(IF_TOKEN, ifname)))
            .collect();
        assert_eq!(inst.lines, want);
        assert!(inst.lines.iter().all(|l| !l.contains(IF_TOKEN)));
        assert_eq!(
            inst.spec.host_addrs,
            vec![
                ip("127.0.0.1"),
                ip("192.0.2.10"),
                ip("192.168.64.1"),
                ip("192.168.65.1")
            ]
        );
    }

    #[test]
    fn the_text_is_a_header_the_table_then_the_lines() {
        let inst = instantiate(&stored(), &live()).unwrap();
        let body: Vec<&str> = inst.text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            body[0],
            "table <faber_host> persist { 127.0.0.1 192.0.2.10 192.168.64.1 192.168.65.1 }"
        );
        assert_eq!(
            &body[1..],
            inst.lines.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert!(inst.text.ends_with('\n'));
        assert_eq!(table_line(&[]), "table <faber_host> persist");
    }

    #[test]
    fn a_new_host_address_changes_the_table_not_the_rules() {
        let s = stored();
        let mut l = live();
        l.interfaces.push(iface("en1", &[("198.51.100.7", 24)]));
        let a = instantiate(&s, &live()).unwrap();
        let b = instantiate(&s, &l).unwrap();
        assert_eq!(a.lines, b.lines);
        assert!(b.spec.host_addrs.contains(&ip("198.51.100.7")));
        assert_ne!(a.text, b.text);
    }

    #[test]
    fn pending_networks_get_no_rules() {
        let mut l = live();
        l.interfaces.retain(|i| i.name != "bridge101");
        let inst = instantiate(&stored(), &l).unwrap();
        assert_eq!(inst.spec.pending, vec![2]);
        assert!(inst.lines.iter().all(|l| l.contains("on bridge100 ")));
        let none = instantiate(&stored(), &LiveView::default()).unwrap();
        assert!(none.lines.is_empty());
        assert_eq!(none.spec.pending, vec![1, 2]);
    }

    #[test]
    fn a_bad_binding_is_an_error() {
        let mut l = live();
        l.interfaces[2].name = "bridge100 all".into();
        assert!(matches!(
            instantiate(&stored(), &l),
            Err(BindError::BadInterfaceName { net: 1, .. })
        ));
    }

    #[test]
    fn the_stored_wall_round_trips_and_is_sealed() {
        let s = stored();
        assert_eq!(s.schema, STORED_SCHEMA);
        assert_eq!(s.rules_version, RULES_VERSION);
        assert_eq!(s.digest.len(), 64);
        let back = StoredWall::from_json(&s.to_json()).unwrap();
        assert_eq!(back, s);
    }

    fn reseal(edit: impl FnOnce(&mut StoredWall)) -> Result<StoredWall, StoredError> {
        let mut s = stored();
        edit(&mut s);
        s.digest = StoredWall::digest_of(&s.networks);
        StoredWall::from_json(&s.to_json())
    }

    #[test]
    fn a_tampered_or_unknown_stored_wall_is_refused() {
        let mut s = stored();
        s.networks[0].lines[5] = "pass in quick on {if} all".into();
        assert_eq!(
            StoredWall::from_json(&s.to_json()),
            Err(StoredError::Digest)
        );
        let mut s = stored();
        s.networks[0].slots[0].addr = ip("192.168.64.9");
        assert_eq!(
            StoredWall::from_json(&s.to_json()),
            Err(StoredError::Digest)
        );

        assert_eq!(reseal(|s| s.schema = 2), Err(StoredError::Schema(2)));
        assert!(matches!(
            reseal(|s| s.networks[0].slots.clear()),
            Err(StoredError::Plan(PlanError::NoSlots(1)))
        ));
        for bad in [
            "pass in quick on bridge100 all",
            "pass in quick on {if} {if} all",
            "pass in quick on{if} all",
            "pass quick on {if} all",
            "pass out quick on {if} all",
            "match in on {if} all",
            "pass in quick on {if} from { 10.0.0.1 } to any",
            "pass in quick on {if} all\nblock all",
            "pass in quick on bridge100 to {if} all",
        ] {
            assert!(
                matches!(
                    reseal(|s| s.networks[0].lines[0] = bad.into()),
                    Err(StoredError::Line {
                        net: 1,
                        index: 0,
                        ..
                    })
                ),
                "{bad:?}"
            );
        }
        for bad in [&b""[..], b"{}", b"{\"schema\":1}", b"null"] {
            assert!(matches!(
                StoredWall::from_json(bad),
                Err(StoredError::Json(_))
            ));
        }
        let mut v: serde_json::Value = serde_json::from_slice(&stored().to_json()).unwrap();
        v["grants"] = serde_json::json!([]);
        assert!(matches!(
            StoredWall::from_json(&serde_json::to_vec(&v).unwrap()),
            Err(StoredError::Json(_))
        ));
    }

    #[test]
    fn the_digest_covers_the_lines_and_the_binding_inputs() {
        let base = stored().digest;
        let g = Grants::default();
        assert_ne!(render(&plan(), &g).unwrap().digest, base);
        let mut p = plan();
        p.networks[1].gateway = ip("192.168.65.254");
        assert_ne!(
            render(&p, &super::super::render::tests::grants())
                .unwrap()
                .digest,
            base
        );
    }
}
