//! Green or red, and what root may do about red.
//!
//! ADR-0026 lists the ways pf stops applying the anchor while VMs keep
//! booting: `pfctl -d` switches pf off for everyone, and a main ruleset loaded
//! without the `com.apple/*` anchor point stops evaluating it. Two more ways
//! carry a packet past a correct anchor: `set skip on` a VM bridge, and an
//! `rdr pass` or `binat pass` translation, whose packets are never filtered.
//! A bound bridge that pf's interface listing does not name at all is red too:
//! a missing line cannot show that pf filters the bridge, so it is never read
//! as "not skipped".
//! Each such way,
//! and each way the anchor could be outvoted or changed, is a [`Reason`], and
//! [`decide`] collects every one that holds, not only the first.
//!
//! [`plan_remedy`] maps reasons to the three repairs root may make: enable pf,
//! reload its own anchor from the stored rules, and refill its host table. The main ruleset is the operator's, so a missing,
//! shadowed or outranked anchor point stays red until the operator fixes it.

use std::fmt;
use std::net::Ipv4Addr;

use super::model::BindError;
use super::pfctl::{is_filter_bypass, PfInterface, PfStatus, RuleEntry};
use super::render::HOST_TABLE;
use super::stored::Instance;
use super::{ANCHOR, ANCHOR_POINT};

/// Everything one check observed.
#[derive(Debug, Clone)]
pub struct Observation {
    pub pf: PfStatus,
    /// `pfctl -sr`: the main ruleset.
    pub main_rules: Vec<RuleEntry>,
    /// `pfctl -a com.apple -sA`: the anchor point's children.
    pub anchors: Vec<String>,
    /// `pfctl -a ANCHOR -vsr`: what the anchor holds.
    pub anchor_rules: Vec<RuleEntry>,
    /// `pfctl -v -s Interfaces`. Read only when a network is bound, and then
    /// every bound bridge must be in it ([`Reason::InterfaceUnlisted`]).
    pub pf_interfaces: Vec<PfInterface>,
    /// `pfctl -s nat` for the main ruleset (named `""`) and for every anchor
    /// `pfctl -v -s Anchors` lists, at any depth. Read only when a network is
    /// bound.
    pub translation: Vec<(String, Vec<RuleEntry>)>,
    /// `pfctl -a ANCHOR -t faber_host -T show`, or why it could not be read
    /// (a table that does not exist yet fails). Read only when a network is
    /// bound: with nothing bound, no rule refers to it.
    pub host_table: Option<Result<Vec<Ipv4Addr>, String>>,
    /// The stored rules bound to the host as it is now.
    pub instance: Result<Instance, BindError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// pf is off: no anchor applies.
    PfDisabled,
    /// The main ruleset has no unconditional `anchor "com.apple/*" all`.
    AnchorPointMissing,
    /// A `quick` rule or another anchor comes before the anchor point, so it
    /// decides first.
    AnchorPointShadowed { rule: usize },
    /// Ours is not the first child of `com.apple`, so a sibling decides first.
    AnchorNotFirst { first: Option<String> },
    /// The anchor holds no rules, and should.
    AnchorEmpty,
    /// The anchor's rules are not the ones rendered; `first_mismatch` is the
    /// first index where they part.
    RulesDiffer { first_mismatch: usize },
    /// pf skips a bound VM interface (`set skip on`): no rule applies there.
    InterfaceSkipped(String),
    /// pf's interface listing does not name a bound VM interface at all, so
    /// nothing shows that pf filters it. Not repaired: the listing is pf's own.
    InterfaceUnlisted(String),
    /// A translation rule hands packets on unfiltered (`rdr pass`, `binat
    /// pass`). `anchor` is `""` for the main ruleset.
    FilterBypass { anchor: String, rule: String },
    /// The anchor's host table is not the host's addresses now (or could not
    /// be read): the host rules would miss an address.
    HostTableDiffers(String),
    /// The stored rules cannot be read, or do not bind to this host.
    StoredInvalid(String),
    /// `ifconfig -a` printed something this release cannot read, so the host
    /// addresses the rules must name are not known.
    InterfacesUnreadable(String),
    /// A command the check needed failed, so nothing it would have shown is
    /// known.
    CommandFailed(String),
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::PfDisabled => write!(f, "pf is disabled"),
            Reason::AnchorPointMissing => write!(
                f,
                "the main ruleset has no `anchor \"{ANCHOR_POINT}\" all` line"
            ),
            Reason::AnchorPointShadowed { rule } => write!(
                f,
                "main-ruleset rule {rule} is quick or an anchor, and comes before \"{ANCHOR_POINT}\""
            ),
            Reason::AnchorNotFirst { first: Some(first) } => write!(
                f,
                "{ANCHOR} is not the first child of com.apple: {} sorts first",
                first.escape_debug()
            ),
            Reason::AnchorNotFirst { first: None } => {
                write!(f, "{ANCHOR} is not loaded under com.apple")
            }
            Reason::AnchorEmpty => write!(f, "{ANCHOR} holds no rules"),
            Reason::RulesDiffer { first_mismatch } => write!(
                f,
                "{ANCHOR} differs from the rendered rules from rule {first_mismatch}"
            ),
            Reason::InterfaceSkipped(name) => write!(
                f,
                "pf skips {} (`set skip on`), so no rule applies to it",
                name.escape_debug()
            ),
            Reason::InterfaceUnlisted(name) => write!(
                f,
                "pf does not list the bound interface {}, so the wall cannot confirm pf filters it",
                name.escape_debug()
            ),
            Reason::FilterBypass { anchor, rule } => write!(
                f,
                "{} has a translation that skips the filter: {}",
                if anchor.is_empty() {
                    "the main ruleset".to_string()
                } else {
                    format!("anchor {}", anchor.escape_debug())
                },
                rule.escape_debug()
            ),
            Reason::HostTableDiffers(why) => write!(
                f,
                "{ANCHOR}'s table <{HOST_TABLE}> is not the host's addresses: {}",
                why.escape_debug()
            ),
            Reason::StoredInvalid(why) => {
                write!(f, "the stored wall cannot be applied: {}", why.escape_debug())
            }
            Reason::InterfacesUnreadable(why) => {
                write!(f, "ifconfig -a is unreadable: {}", why.escape_debug())
            }
            Reason::CommandFailed(why) => write!(f, "a check failed: {why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Green,
    Red(Vec<Reason>),
}

impl Verdict {
    pub fn is_green(&self) -> bool {
        matches!(self, Verdict::Green)
    }

    pub fn reasons(&self) -> &[Reason] {
        match self {
            Verdict::Green => &[],
            Verdict::Red(r) => r,
        }
    }
}

/// The exact main-ruleset line that evaluates our anchor. A conditional form
/// (`on en0`, `in`) applies it to less than all traffic, so it does not count.
fn anchor_point_line() -> String {
    format!("anchor \"{ANCHOR_POINT}\" all")
}

/// Judge `obs` against the stored rules as bound to the host
/// ([`Observation::instance`]).
///
/// When they do not bind, the anchor's contents are not compared: there is
/// nothing right to compare them with, and red already stops admission. The
/// rules that are loaded keep applying.
///
/// With nothing bound — no VM network has a bridge — the instance holds no
/// rules and the anchor need not exist, so its place among siblings, its
/// table and pf's interface listing are not checked. A VM that starts then
/// runs unwalled until the next check, which is why each VM also proves the
/// wall from inside before it gets work.
///
/// With something bound, every bound bridge must be in pf's interface listing
/// and not skipped (`interface_reason`).
pub fn decide(obs: &Observation) -> Verdict {
    let mut reasons = Vec::new();
    if obs.pf == PfStatus::Disabled {
        reasons.push(Reason::PfDisabled);
    }

    let point = anchor_point_line();
    match obs.main_rules.iter().position(|r| r.text == point) {
        None => reasons.push(Reason::AnchorPointMissing),
        Some(at) => {
            if let Some(rule) = obs.main_rules[..at].iter().position(decides_first) {
                reasons.push(Reason::AnchorPointShadowed { rule });
            }
        }
    }

    match &obs.instance {
        Err(e) => reasons.push(Reason::StoredInvalid(e.to_string())),
        Ok(expected) => {
            let spec = &expected.spec;
            for bound in &spec.networks {
                reasons.extend(interface_reason(
                    bound.interface.as_str(),
                    &obs.pf_interfaces,
                ));
            }
            if !spec.networks.is_empty() {
                for (anchor, rules) in &obs.translation {
                    for r in rules.iter().filter(|r| is_filter_bypass(&r.text)) {
                        reasons.push(Reason::FilterBypass {
                            anchor: anchor.clone(),
                            rule: r.text.clone(),
                        });
                    }
                }
            }
            if !expected.lines.is_empty() {
                let first = obs.anchors.iter().min();
                if first.map(String::as_str) != Some(ANCHOR) {
                    reasons.push(Reason::AnchorNotFirst {
                        first: first
                            .filter(|_| obs.anchors.iter().any(|a| a == ANCHOR))
                            .cloned(),
                    });
                }
            }
            let loaded: Vec<&str> = obs.anchor_rules.iter().map(|r| r.text.as_str()).collect();
            if loaded.is_empty() && !expected.lines.is_empty() {
                reasons.push(Reason::AnchorEmpty);
            } else if loaded != expected.lines {
                let first_mismatch = loaded
                    .iter()
                    .zip(&expected.lines)
                    .position(|(a, b)| a != b)
                    .unwrap_or(loaded.len().min(expected.lines.len()));
                reasons.push(Reason::RulesDiffer { first_mismatch });
            }
            if !spec.networks.is_empty() {
                if let Some(why) = table_differs(obs.host_table.as_ref(), &spec.host_addrs) {
                    reasons.push(Reason::HostTableDiffers(why));
                }
            }
        }
    }

    if reasons.is_empty() {
        Verdict::Green
    } else {
        Verdict::Red(reasons)
    }
}

/// What pf's interface listing says against the bound bridge `name`.
///
/// Red when pf skips it, and red when pf does not list it at all: an absent
/// line is not "not skipped", it is no evidence either way, so the wall fails
/// closed on it (#309). A name matches whole, so a listed `bridge100` says
/// nothing of a bound `bridge10`, nor the reverse. Every other listed
/// interface, `bridge101` left over from a VM that has stopped included (pf
/// keeps the interfaces it has seen), is not ours and is not judged.
fn interface_reason(name: &str, listing: &[PfInterface]) -> Option<Reason> {
    let mut listed = false;
    let mut skipped = false;
    for entry in listing.iter().filter(|i| i.name == name) {
        listed = true;
        skipped |= entry.skipped;
    }
    if !listed {
        Some(Reason::InterfaceUnlisted(name.to_string()))
    } else if skipped {
        Some(Reason::InterfaceSkipped(name.to_string()))
    } else {
        None
    }
}

/// Why the table read is not `want` (sorted, deduplicated), or `None`.
fn table_differs(
    read: Option<&Result<Vec<Ipv4Addr>, String>>,
    want: &[Ipv4Addr],
) -> Option<String> {
    let have = match read {
        None => return Some("it was not read".into()),
        Some(Err(why)) => return Some(why.clone()),
        Some(Ok(have)) => have,
    };
    let mut have = have.clone();
    have.sort();
    have.dedup();
    if have.is_empty() {
        return Some("it is empty".into());
    }
    (have != want).then(|| {
        let missing = want.iter().filter(|a| !have.contains(a)).count();
        let extra = have.iter().filter(|a| !want.contains(a)).count();
        format!("{missing} missing, {extra} extra")
    })
}

/// A main-ruleset rule that would decide before an anchor evaluated after it:
/// a `quick` rule, or an anchor of its own (whose quick rules would). Plain
/// rules only set a default that our quick rules override.
fn decides_first(rule: &RuleEntry) -> bool {
    let mut words = rule.text.split_whitespace();
    words.next() == Some("anchor") || words.any(|w| w == "quick")
}

/// A repair root may make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `pfctl -E`.
    EnablePf,
    /// `pfctl -a ANCHOR -f -` with the instantiated text. Also how a new
    /// bridge reaches the rules.
    LoadAnchor,
    /// `pfctl -a ANCHOR -t faber_host -T replace ADDR…` with the host's
    /// addresses now. How a changed host address reaches the rules.
    ReplaceHostTable,
}

/// The repairs for `verdict`, at most one of each, pf first. Nothing touches the
/// main ruleset: it is the operator's.
pub fn plan_remedy(verdict: &Verdict) -> Vec<Action> {
    let reasons = verdict.reasons();
    let mut actions = Vec::new();
    if reasons.contains(&Reason::PfDisabled) {
        actions.push(Action::EnablePf);
    }
    if reasons
        .iter()
        .any(|r| matches!(r, Reason::AnchorEmpty | Reason::RulesDiffer { .. }))
    {
        actions.push(Action::LoadAnchor);
    }
    if reasons
        .iter()
        .any(|r| matches!(r, Reason::HostTableDiffers(_)))
    {
        actions.push(Action::ReplaceHostTable);
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::super::model::tests::live;
    use super::super::model::{LiveView, PlanError};
    use super::super::pfctl::tests::ROOT_INTERFACES_BRIDGE100_BOUND;
    use super::super::pfctl::{parse_interfaces, parse_rules};
    use super::super::render::tests::stored;
    use super::super::stored::instantiate;
    use super::*;

    const APPLE_FW: &str = "com.apple/250.ApplicationFirewall";

    fn rules(texts: &[&str]) -> Vec<RuleEntry> {
        parse_rules(&texts.join("\n")).unwrap()
    }

    /// A healthy host: pf on, the stock main ruleset, our anchor loaded first,
    /// both networks bound (to `bridge100` and `bridge101`) and both listed.
    fn healthy() -> (Observation, Instance) {
        healthy_on(&live())
    }

    /// [`healthy`] on a host with the interfaces `live`. pf lists the same two
    /// bridges whatever `live` holds, so a test that binds another one sets
    /// `pf_interfaces` itself.
    fn healthy_on(live: &LiveView) -> (Observation, Instance) {
        let expected = instantiate(&stored(), live).unwrap();
        let anchor_rules = parse_rules(&expected.lines.join("\n")).unwrap();
        let obs = Observation {
            pf: PfStatus::Enabled,
            main_rules: rules(&[
                "scrub-anchor \"com.apple/*\" all fragment reassemble",
                "anchor \"com.apple/*\" all",
            ]),
            anchors: vec![APPLE_FW.into(), ANCHOR.into()],
            anchor_rules,
            pf_interfaces: parse_interfaces("all\nbridge100\nbridge101\nen0\nlo0 (skip)\n")
                .unwrap(),
            translation: vec![
                (
                    String::new(),
                    rules(&[
                        "nat-anchor \"com.apple/*\" all",
                        "rdr-anchor \"com.apple/*\" all",
                    ]),
                ),
                (APPLE_FW.into(), vec![]),
            ],
            host_table: Some(Ok(expected.spec.host_addrs.clone())),
            instance: Ok(expected.clone()),
        };
        (obs, expected)
    }

    /// `healthy()` on a host where only `bridge` is up, holding network 1's
    /// gateway: network 1 is bound to it, and network 2 waits for a bridge. As
    /// when only `bridge100` is bound. pf's listing is still `healthy()`'s, so a
    /// test sets it.
    fn healthy_with_only(bridge: &str) -> Observation {
        let mut view = live();
        view.interfaces.retain(|i| i.name != "bridge101");
        for i in view.interfaces.iter_mut().filter(|i| i.name == "bridge100") {
            i.name = bridge.to_string();
        }
        let (obs, expected) = healthy_on(&view);
        assert_eq!(
            expected.spec.networks.len(),
            1,
            "network 1 is the only one bound"
        );
        obs
    }

    /// The reasons `decide` gives `obs`.
    fn reasons_of(obs: &Observation) -> Vec<Reason> {
        decide(obs).reasons().to_vec()
    }

    #[test]
    fn a_healthy_host_is_green() {
        let (obs, _) = healthy();
        assert_eq!(decide(&obs), Verdict::Green);
        assert!(plan_remedy(&Verdict::Green).is_empty());
    }

    #[test]
    fn pf_disabled_is_red_and_enables_pf() {
        let (mut obs, _) = healthy();
        obs.pf = PfStatus::Disabled;
        let v = decide(&obs);
        assert_eq!(v, Verdict::Red(vec![Reason::PfDisabled]));
        assert_eq!(plan_remedy(&v), vec![Action::EnablePf]);
    }

    #[test]
    fn a_missing_or_conditional_anchor_point_is_red_and_never_repaired() {
        for main in [
            vec![],
            vec!["anchor \"com.apple/*\" on en0 all"],
            vec!["anchor \"com.apple/*\" in all"],
            vec!["anchor \"com.apple/*\" inet all"],
            vec!["anchor \"/*\" all"],
            vec!["pass all flags S/SA keep state"],
        ] {
            let (mut obs, _) = healthy();
            obs.main_rules = rules(&main);
            let v = decide(&obs);
            assert_eq!(
                v,
                Verdict::Red(vec![Reason::AnchorPointMissing]),
                "{main:?}"
            );
            assert!(plan_remedy(&v).is_empty());
        }
    }

    #[test]
    fn a_quick_rule_or_anchor_before_the_point_shadows_it() {
        for (main, at) in [
            (
                vec![
                    "scrub-anchor \"com.apple/*\" all fragment reassemble",
                    "pass in quick on bridge100 all flags S/SA keep state",
                    "anchor \"com.apple/*\" all",
                ],
                1,
            ),
            (
                vec!["anchor \"operator/*\" all", "anchor \"com.apple/*\" all"],
                0,
            ),
        ] {
            let (mut obs, _) = healthy();
            obs.main_rules = rules(&main);
            let v = decide(&obs);
            assert_eq!(
                v,
                Verdict::Red(vec![Reason::AnchorPointShadowed { rule: at }])
            );
            assert!(plan_remedy(&v).is_empty());
        }
    }

    #[test]
    fn non_quick_rules_before_or_anything_after_the_point_is_harmless() {
        let (mut obs, _) = healthy();
        obs.main_rules = rules(&[
            "scrub-anchor \"com.apple/*\" all fragment reassemble",
            "block drop in all",
            "pass out all flags S/SA keep state",
            "anchor \"com.apple/*\" all",
            "pass in quick all flags S/SA keep state",
            "anchor \"later/*\" all",
        ]);
        assert_eq!(decide(&obs), Verdict::Green);
    }

    #[test]
    fn a_sibling_sorting_first_or_our_absence_is_red() {
        let (mut obs, _) = healthy();
        obs.anchors = vec![ANCHOR.into(), "com.apple/000.aaa".into(), APPLE_FW.into()];
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![Reason::AnchorNotFirst {
                first: Some("com.apple/000.aaa".into())
            }])
        );
        assert!(plan_remedy(&v).is_empty());

        obs.anchors = vec![APPLE_FW.into()];
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![Reason::AnchorNotFirst { first: None }])
        );
    }

    #[test]
    fn an_empty_anchor_is_red_and_reloads() {
        let (mut obs, _) = healthy();
        obs.anchor_rules.clear();
        let v = decide(&obs);
        assert_eq!(v, Verdict::Red(vec![Reason::AnchorEmpty]));
        assert_eq!(plan_remedy(&v), vec![Action::LoadAnchor]);
    }

    #[test]
    fn tampered_rules_are_red_and_reload() {
        let (mut obs, _) = healthy();
        obs.anchor_rules[5].text = "pass in quick on bridge100 all".into();
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![Reason::RulesDiffer { first_mismatch: 5 }])
        );
        assert_eq!(plan_remedy(&v), vec![Action::LoadAnchor]);

        // A missing tail, and an extra rule, are differences too.
        let (mut obs, _) = healthy();
        let n = obs.anchor_rules.len();
        obs.anchor_rules.pop();
        assert_eq!(
            decide(&obs),
            Verdict::Red(vec![Reason::RulesDiffer {
                first_mismatch: n - 1
            }])
        );
        let (mut obs, _) = healthy();
        obs.anchor_rules.push(obs.anchor_rules[0].clone());
        assert_eq!(
            decide(&obs),
            Verdict::Red(vec![Reason::RulesDiffer { first_mismatch: n }])
        );
    }

    #[test]
    fn an_invalid_plan_is_red_and_leaves_the_anchor_alone() {
        let (mut obs, _) = healthy();
        obs.instance = Err(BindError::Plan(PlanError::NoSlots(1)));
        obs.anchor_rules.clear();
        obs.host_table = None;
        let v = decide(&obs);
        assert_eq!(v.reasons().len(), 1);
        assert!(matches!(v.reasons()[0], Reason::StoredInvalid(_)));
        assert!(plan_remedy(&v).is_empty());
    }

    #[test]
    fn with_nothing_bound_an_absent_anchor_is_green() {
        let (mut obs, _) = healthy();
        obs.instance = Ok(instantiate(&stored(), &LiveView::default()).unwrap());
        obs.host_table = None;
        obs.anchors = vec![APPLE_FW.into()];
        obs.anchor_rules.clear();
        assert_eq!(decide(&obs), Verdict::Green);
        // But stale rules left in it are reloaded away.
        let (healthy_obs, _) = healthy();
        obs.anchor_rules = healthy_obs.anchor_rules;
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![Reason::RulesDiffer { first_mismatch: 0 }])
        );
        assert_eq!(plan_remedy(&v), vec![Action::LoadAnchor]);
    }

    #[test]
    fn every_reason_is_collected_and_remedies_are_ordered() {
        let (mut obs, _) = healthy();
        obs.pf = PfStatus::Disabled;
        obs.main_rules.clear();
        obs.anchors.clear();
        obs.anchor_rules.clear();
        obs.host_table = Some(Err("pfctl: Table does not exist.".into()));
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![
                Reason::PfDisabled,
                Reason::AnchorPointMissing,
                Reason::AnchorNotFirst { first: None },
                Reason::AnchorEmpty,
                Reason::HostTableDiffers("pfctl: Table does not exist.".into()),
            ])
        );
        assert_eq!(
            plan_remedy(&v),
            vec![
                Action::EnablePf,
                Action::LoadAnchor,
                Action::ReplaceHostTable
            ]
        );
        assert!(plan_remedy(&Verdict::Red(vec![Reason::CommandFailed("x".into())])).is_empty());
    }

    #[test]
    fn reasons_print_as_one_line() {
        let all = [
            Reason::PfDisabled,
            Reason::AnchorPointMissing,
            Reason::AnchorPointShadowed { rule: 2 },
            Reason::AnchorNotFirst {
                first: Some("com.apple/0\n".into()),
            },
            Reason::AnchorNotFirst { first: None },
            Reason::AnchorEmpty,
            Reason::RulesDiffer { first_mismatch: 3 },
            Reason::InterfaceSkipped("bridge100".into()),
            Reason::InterfaceUnlisted("bridge100".into()),
            Reason::InterfaceUnlisted("bridge1\n".into()),
            Reason::FilterBypass {
                anchor: String::new(),
                rule: "rdr pass on en0 all".into(),
            },
            Reason::FilterBypass {
                anchor: "com.apple/x\n".into(),
                rule: "binat pass\n".into(),
            },
            Reason::HostTableDiffers("x\n".into()),
            Reason::StoredInvalid("x\n".into()),
            Reason::InterfacesUnreadable("en0:\n".into()),
            Reason::CommandFailed("y".into()),
        ];
        for r in all {
            let s = r.to_string();
            assert!(!s.is_empty() && !s.contains('\n'), "{s:?}");
        }
    }

    #[test]
    fn a_skipped_bound_interface_is_red_and_never_repaired() {
        let (mut obs, _) = healthy();
        obs.pf_interfaces =
            parse_interfaces("all\nbridge100 (skip)\nbridge101\nlo0 (skip)\n").unwrap();
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![Reason::InterfaceSkipped("bridge100".into())])
        );
        assert!(plan_remedy(&v).is_empty());
        // Skipped loopback, or an interface no network is bound to, is not ours.
        // Both bound bridges must be listed: `healthy()` binds `bridge101` too.
        let (mut obs, _) = healthy();
        obs.pf_interfaces =
            parse_interfaces("all\nbridge100\nbridge101\nbridge102 (skip)\nlo0 (skip)\n").unwrap();
        assert_eq!(decide(&obs), Verdict::Green);
    }

    /// A `pfctl -v -s Interfaces` listing shaped as it is after setup, with
    /// `bridge100` bound (#206): the bound bridge is listed, no line is skipped,
    /// and `bridge101`, a bridge that no longer exists, is still there.
    #[test]
    fn a_listing_with_bridge100_bound_and_a_stale_bridge101_adds_no_reason() {
        let mut obs = healthy_with_only("bridge100");
        obs.pf_interfaces = parse_interfaces(ROOT_INTERFACES_BRIDGE100_BOUND).unwrap();
        assert!(obs.pf_interfaces.iter().any(|i| i.name == "bridge100"));
        assert_eq!(decide(&obs), Verdict::Green);
    }

    #[test]
    fn a_bound_interface_pf_does_not_list_is_red_and_never_repaired() {
        let mut obs = healthy_with_only("bridge100");
        obs.pf_interfaces = parse_interfaces("ALL\nen0\nlo0\n").unwrap();
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![Reason::InterfaceUnlisted("bridge100".into())])
        );
        assert_eq!(
            v.reasons()[0].to_string(),
            "pf does not list the bound interface bridge100, so the wall cannot confirm pf filters it"
        );
        // The listing is pf's own, so there is nothing root may repair.
        assert!(plan_remedy(&v).is_empty());
    }

    /// pf keeps the interfaces it has seen, so a bridge a stopped VM left is
    /// still listed. Nothing is bound to it, so it is not judged, skipped or not.
    #[test]
    fn a_listed_bridge_nothing_is_bound_to_adds_no_reason() {
        let mut obs = healthy_with_only("bridge100");
        for listing in [
            "all\nbridge100\nbridge101\n",
            "all\nbridge100\nbridge101 (skip)\nbridge102 (skip)\n",
        ] {
            obs.pf_interfaces = parse_interfaces(listing).unwrap();
            assert_eq!(decide(&obs), Verdict::Green, "{listing:?}");
        }
    }

    /// A listed `(skip)` is still `InterfaceSkipped`, not "unlisted", and a
    /// skip on one line wins over a plain line for the same name.
    #[test]
    fn a_listed_and_skipped_bound_interface_is_skipped_not_unlisted() {
        let mut obs = healthy_with_only("bridge100");
        for listing in [
            "all\nbridge100 (skip)\n",
            "all\nbridge100\nbridge100 (skip)\n",
        ] {
            obs.pf_interfaces = parse_interfaces(listing).unwrap();
            assert_eq!(
                reasons_of(&obs),
                vec![Reason::InterfaceSkipped("bridge100".into())],
                "{listing:?}"
            );
        }
    }

    /// A name is matched whole: a listed `bridge100` says nothing about a bound
    /// `bridge10`, nor the reverse, and a skip on a name that only starts like
    /// ours is not ours.
    #[test]
    fn interface_names_match_whole() {
        for (bound, listing) in [
            ("bridge10", "all\nbridge100\n"),
            ("bridge100", "all\nbridge10\n"),
            ("bridge100", "all\nbridge1000\n"),
            ("bridge10", "all\nbridge100 (skip)\n"),
            ("bridge100", "all\nBRIDGE100\n"),
        ] {
            let mut obs = healthy_with_only(bound);
            obs.pf_interfaces = parse_interfaces(listing).unwrap();
            assert_eq!(
                reasons_of(&obs),
                vec![Reason::InterfaceUnlisted(bound.into())],
                "{bound} bound, {listing:?} listed"
            );
        }
    }

    /// With two networks bound, each bridge is judged on its own line.
    #[test]
    fn with_two_bridges_bound_only_the_unlisted_one_is_named() {
        let (base, _) = healthy();
        assert_eq!(base.instance.as_ref().unwrap().spec.networks.len(), 2);
        for (listing, missing, present) in [
            ("all\nbridge100\nen0\n", "bridge101", "bridge100"),
            ("all\nbridge101\nen0\n", "bridge100", "bridge101"),
        ] {
            let mut obs = base.clone();
            obs.pf_interfaces = parse_interfaces(listing).unwrap();
            let v = decide(&obs);
            assert_eq!(
                v,
                Verdict::Red(vec![Reason::InterfaceUnlisted(missing.into())]),
                "{listing:?}"
            );
            let said = v.reasons()[0].to_string();
            assert!(said.contains(missing) && !said.contains(present), "{said}");
            assert!(plan_remedy(&v).is_empty());
        }
        // Both missing: both named, in network order. One skipped and one
        // missing: each says its own.
        let mut obs = base.clone();
        obs.pf_interfaces = parse_interfaces("all\nen0\n").unwrap();
        assert_eq!(
            reasons_of(&obs),
            vec![
                Reason::InterfaceUnlisted("bridge100".into()),
                Reason::InterfaceUnlisted("bridge101".into()),
            ]
        );
        obs.pf_interfaces = parse_interfaces("all\nbridge100 (skip)\n").unwrap();
        assert_eq!(
            reasons_of(&obs),
            vec![
                Reason::InterfaceSkipped("bridge100".into()),
                Reason::InterfaceUnlisted("bridge101".into()),
            ]
        );
    }

    #[test]
    fn a_translation_that_skips_the_filter_is_red_wherever_it_is() {
        let (mut obs, _) = healthy();
        let main_bypass =
            "rdr pass on bridge100 inet proto tcp from any to any port = 80 -> 127.0.0.1 port 8080";
        let child_bypass = "binat pass on en0 inet from 192.168.64.2 to any -> 198.51.100.7";
        obs.translation[0].1.extend(rules(&[
            main_bypass,
            "nat on en0 inet from 192.168.64.0/24 to any -> 192.0.2.10",
        ]));
        obs.translation[1].1 = rules(&[child_bypass]);
        let v = decide(&obs);
        assert_eq!(
            v,
            Verdict::Red(vec![
                Reason::FilterBypass {
                    anchor: String::new(),
                    rule: main_bypass.into()
                },
                Reason::FilterBypass {
                    anchor: APPLE_FW.into(),
                    rule: child_bypass.into()
                },
            ])
        );
        assert!(plan_remedy(&v).is_empty());
    }

    #[test]
    fn with_nothing_bound_translation_and_skips_are_not_judged() {
        let (mut obs, _) = healthy();
        obs.instance = Ok(instantiate(&stored(), &LiveView::default()).unwrap());
        obs.host_table = None;
        obs.anchor_rules.clear();
        obs.translation[0].1 = rules(&["rdr pass on en0 all -> 127.0.0.1"]);
        assert_eq!(decide(&obs), Verdict::Green);
        // `observe` reads no interface listing then, so an empty one is not an
        // unlisted bridge.
        obs.pf_interfaces.clear();
        assert_eq!(decide(&obs), Verdict::Green);
    }

    #[test]
    fn a_host_table_that_differs_is_red_and_replaced() {
        let ip = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        let (base, _) = healthy();
        let want = base.instance.as_ref().unwrap().spec.host_addrs.clone();
        let mut fewer = want.clone();
        fewer.pop();
        let mut more = want.clone();
        more.push(ip("198.51.100.7"));
        for (read, why) in [
            (None, "not read"),
            (Some(Err("no such table".to_string())), "no such table"),
            (Some(Ok(vec![])), "empty"),
            (Some(Ok(fewer)), "1 missing, 0 extra"),
            (Some(Ok(more)), "0 missing, 1 extra"),
        ] {
            let mut obs = base.clone();
            obs.host_table = read;
            let v = decide(&obs);
            assert!(
                matches!(v.reasons(), [Reason::HostTableDiffers(m)] if m.contains(why)),
                "{v:?}"
            );
            assert_eq!(plan_remedy(&v), vec![Action::ReplaceHostTable]);
        }
        // Order and repeats do not matter.
        let mut obs = base.clone();
        let mut shuffled = want.clone();
        shuffled.reverse();
        shuffled.push(want[0]);
        obs.host_table = Some(Ok(shuffled));
        assert_eq!(decide(&obs), Verdict::Green);
    }
}
