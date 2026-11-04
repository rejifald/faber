//! Parsers for what `pfctl` prints on stdout.
//!
//! The checker runs `pfctl` as root and reads back: whether pf is enabled
//! (`-s info`), the main ruleset and the anchor's rules (`-s rules`, with `-v`
//! for counters), the children of `com.apple` (`-s Anchors`), which interfaces
//! pf skips (`-v -s Interfaces`), the translation rules (`-s nat`) that
//! could carry a packet past the filter, and the host table (`-T show`).
//! Every parser here refuses what it does not recognise: an output this
//! release cannot read is red, never assumed green. The `ALTQ` notices pfctl
//! prints go to stderr, which these never see.

/// pf's state, from the first line of `pfctl -s info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PfStatus {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PfctlError {
    #[error("pfctl -s info did not start with a status line: \"{0}\"")]
    UnknownStatus(String),
    #[error("line {0}: counters with no rule before them")]
    OrphanCounters(usize),
    #[error("line {0}: unreadable counters")]
    BadCounters(usize),
    #[error("line {0}: indented text that is not a counters line")]
    UnknownIndented(usize),
    #[error(
        "pfctl -v -s Interfaces listed no `all` (or `ALL`) line, so it is not an interface listing"
    )]
    NoInterfaceListing,
    #[error("line {0}: not a plain IPv4 address in a table listing")]
    BadTableEntry(usize),
}

/// The first line of `pfctl -s info`:
/// `Status: Enabled for 0 days 00:12:34           Debug: Urgent`.
pub fn parse_info(out: &str) -> Result<PfStatus, PfctlError> {
    let first = out.lines().next().unwrap_or("");
    let mut words = first.split_whitespace();
    match (words.next(), words.next()) {
        (Some("Status:"), Some("Enabled")) => Ok(PfStatus::Enabled),
        (Some("Status:"), Some("Disabled")) => Ok(PfStatus::Disabled),
        _ => Err(PfctlError::UnknownStatus(
            first
                .chars()
                .take(80)
                .collect::<String>()
                .escape_debug()
                .to_string(),
        )),
    }
}

/// One rule's counters, from `pfctl -v`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    pub evaluations: u64,
    pub packets: u64,
    pub bytes: u64,
    pub states: u64,
}

/// One rule as pfctl printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEntry {
    /// The rule line, trimmed.
    pub text: String,
    /// The text of its `label "…"`, if it has one.
    pub label: Option<String>,
    /// Present for `-vsr` output.
    pub counters: Option<Counters>,
}

/// `pfctl -sr` or `pfctl -vsr`. A rule is an unindented line; `-v` adds
/// indented `[ … ]` lines under it. Blank lines are skipped.
pub fn parse_rules(out: &str) -> Result<Vec<RuleEntry>, PfctlError> {
    let mut rules: Vec<RuleEntry> = Vec::new();
    for (i, line) in out.lines().enumerate() {
        let n = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            let text = line.trim().to_string();
            let label = label_of(&text);
            rules.push(RuleEntry {
                text,
                label,
                counters: None,
            });
            continue;
        }
        let inner = line
            .trim()
            .strip_prefix('[')
            .and_then(|l| l.strip_suffix(']'))
            .ok_or(PfctlError::UnknownIndented(n))?;
        let rule = rules.last_mut().ok_or(PfctlError::OrphanCounters(n))?;
        if inner.trim_start().starts_with("Evaluations:") {
            rule.counters = Some(parse_counters(inner).ok_or(PfctlError::BadCounters(n))?);
        }
        // Any other bracket line (`Inserted: uid 0 pid …`) is informational.
    }
    Ok(rules)
}

/// `Evaluations: 12  Packets: 3  Bytes: 180  States: 0`, each key once.
fn parse_counters(inner: &str) -> Option<Counters> {
    let words: Vec<&str> = inner.split_whitespace().collect();
    if words.len() != 8 {
        return None;
    }
    let mut c = Counters::default();
    for pair in words.chunks(2) {
        let v: u64 = pair[1].parse().ok()?;
        match pair[0] {
            "Evaluations:" => c.evaluations = v,
            "Packets:" => c.packets = v,
            "Bytes:" => c.bytes = v,
            "States:" => c.states = v,
            _ => return None,
        }
    }
    let keys: Vec<&str> = words.iter().step_by(2).copied().collect();
    (keys == ["Evaluations:", "Packets:", "Bytes:", "States:"]).then_some(c)
}

/// The quoted text after ` label `, or `None` if there is none or its quote
/// never closes.
fn label_of(rule: &str) -> Option<String> {
    let (_, after) = rule.split_once(" label \"")?;
    let (label, _) = after.split_once('"')?;
    Some(label.to_string())
}

/// `pfctl -a com.apple -s Anchors` (the children) or `pfctl -v -s Anchors`
/// (every anchor, recursively): one path per line, possibly indented.
/// Returned trimmed, in the order printed.
pub fn parse_anchors(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// One interface as `pfctl -v -s Interfaces` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PfInterface {
    pub name: String,
    /// `set skip on` it: pf neither filters nor translates anything there.
    pub skipped: bool,
}

/// `pfctl -v -s Interfaces`: one interface or group per line, `lo0 (skip)`
/// for a skipped one. pf always lists the `all` group (printed `ALL` as root
/// on macOS 27), so an output without it is not a listing at all and is
/// refused. Indented detail lines (`-vv`) and anything else unrecognised are
/// skipped: they cannot hide a `(skip)` on an interface's own line.
///
/// The listing is what pf has seen, not what is up. As root on macOS 27 it
/// holds a bridge that no longer exists, and carries `(skip)` only where the
/// main ruleset says `set skip`. A line that is missing proves nothing about
/// the interface, so the verdict reads a bound bridge missing from it as red
/// ([`super::verdict::Reason::InterfaceUnlisted`]), never as "not skipped".
pub fn parse_interfaces(out: &str) -> Result<Vec<PfInterface>, PfctlError> {
    let mut found = Vec::new();
    for line in out.lines() {
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(name) = words.next() else { continue };
        let skipped = words.any(|w| w == "(skip)");
        found.push(PfInterface {
            name: name.to_string(),
            skipped,
        });
    }
    // macOS 27 prints the group as `ALL` when run as root (#206); the
    // documentation and the unprivileged `-n` form spell it `all`.
    if !found.iter().any(|i| i.name.eq_ignore_ascii_case("all")) {
        return Err(PfctlError::NoInterfaceListing);
    }
    Ok(found)
}

/// Whether a translation rule from `pfctl -s nat` skips the filter: `rdr pass`
/// and `binat pass` hand the translated packet on unfiltered, so an anchor's
/// block rules never see it. A plain `rdr` or `nat` is fine, since the filter
/// still sees the translated packet, and `no rdr` translates nothing. `nat
/// pass` translates on the way out, after the inbound rules on a VM bridge
/// have already decided.
pub fn is_filter_bypass(rule: &str) -> bool {
    let words: Vec<&str> = rule.split_whitespace().collect();
    words
        .windows(2)
        .any(|w| matches!(w[0], "rdr" | "binat") && w[1] == "pass")
}

/// `pfctl -a ANCHOR -t TABLE -T show`: one address per line, indented by
/// spaces. Anything but a plain IPv4 address (a network, a negation `!`, an
/// IPv6 address) is refused: faber's host table holds host addresses only.
pub fn parse_table(out: &str) -> Result<Vec<std::net::Ipv4Addr>, PfctlError> {
    let mut addrs = Vec::new();
    for (i, line) in out.lines().enumerate() {
        let entry = line.trim();
        if entry.is_empty() {
            continue;
        }
        addrs.push(
            entry
                .parse()
                .map_err(|_| PfctlError::BadTableEntry(i + 1))?,
        );
    }
    Ok(addrs)
}

/// The token `pfctl -E` prints on stdout (`Token : 1234`), if it printed one.
pub fn parse_enable_token(out: &str) -> Option<u64> {
    out.lines().find_map(|l| {
        let (key, value) = l.split_once(':')?;
        (key.trim() == "Token").then(|| value.trim().parse().ok())?
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn info_enabled_and_disabled() {
        let enabled = "Status: Enabled for 0 days 00:12:34           Debug: Urgent\n\nState Table                          Total             Rate\n  current entries                        4               \n";
        assert_eq!(parse_info(enabled), Ok(PfStatus::Enabled));
        let disabled = "Status: Disabled for 0 days 00:00:07          Debug: Urgent\n";
        assert_eq!(parse_info(disabled), Ok(PfStatus::Disabled));
    }

    #[test]
    fn info_anything_else_is_an_error() {
        for bad in [
            "",
            "\n",
            "No ALTQ support in kernel\nStatus: Enabled",
            "Status:",
            "Status: Unknown for 0 days",
            "status: Enabled",
            "Status:Enabled",
            "pfctl: /dev/pf: Permission denied",
        ] {
            assert!(parse_info(bad).is_err(), "{bad:?}");
        }
    }

    /// `pfctl -a com.apple/000.faber-vm-wall -vsr`, two rules.
    pub(crate) const VERBOSE: &str = "\
pass in quick on bridge100 inet proto udp from any port = 68 to 255.255.255.255 port = 67 keep state label \"faber-wall/n1/dhcp\"
  [ Evaluations: 12        Packets: 3         Bytes: 180         States: 0     ]
  [ Inserted: uid 0 pid 4242 State Creations: 0     ]
block drop in quick on bridge100 all label \"faber-wall/n1/rest\"
  [ Evaluations: 9         Packets: 9         Bytes: 540         States: 0     ]
  [ Inserted: uid 0 pid 4242 State Creations: 0     ]
";

    #[test]
    fn verbose_rules_with_counters() {
        let rules = parse_rules(VERBOSE).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].label.as_deref(), Some("faber-wall/n1/dhcp"));
        assert_eq!(
            rules[0].counters,
            Some(Counters {
                evaluations: 12,
                packets: 3,
                bytes: 180,
                states: 0
            })
        );
        assert_eq!(
            rules[1].text,
            "block drop in quick on bridge100 all label \"faber-wall/n1/rest\""
        );
        assert_eq!(rules[1].counters.unwrap().packets, 9);
    }

    #[test]
    fn plain_rules_and_blank_lines() {
        let out =
            "scrub-anchor \"com.apple/*\" all fragment reassemble\n\nanchor \"com.apple/*\" all\n";
        let rules = parse_rules(out).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[1].text, "anchor \"com.apple/*\" all");
        assert!(rules
            .iter()
            .all(|r| r.counters.is_none() && r.label.is_none()));
        assert_eq!(parse_rules("").unwrap(), vec![]);
    }

    /// What `pfctl -sr` prints for the stock main ruleset is not confirmed as
    /// root (see docs/vm-wall-pfctl-capture.md). Both forms the issue names
    /// parse into the same shape, so the verdict, not the parser, decides
    /// which one counts as the anchor point (only `com.apple/*` does).
    #[test]
    fn both_anchor_point_print_backs_parse() {
        let root =
            "scrub-anchor \"com.apple/*\" all fragment reassemble\nanchor \"com.apple/*\" all\n";
        let unprivileged = "scrub-anchor \"/*\" all fragment reassemble\nanchor \"/*\" all\n";
        assert_eq!(
            parse_rules(root).unwrap()[1].text,
            "anchor \"com.apple/*\" all"
        );
        assert_eq!(
            parse_rules(unprivileged).unwrap()[1].text,
            "anchor \"/*\" all"
        );
    }

    /// `-v -s Anchors` recursing (nested paths, indented) or not (top level
    /// only): both parse to one trimmed path per line.
    #[test]
    fn recursive_and_flat_anchor_listings_parse() {
        let recursive =
            "  com.apple\n  com.apple/250.ApplicationFirewall\n  opfwd\n  opfwd/inner\n";
        let flat = "com.apple\nopfwd\n";
        assert_eq!(parse_anchors(recursive).len(), 4);
        assert_eq!(parse_anchors(flat), vec!["com.apple", "opfwd"]);
    }

    #[test]
    fn counters_with_no_rule_or_bad_counters_are_errors() {
        assert_eq!(
            parse_rules("  [ Evaluations: 1 Packets: 1 Bytes: 1 States: 0 ]\n"),
            Err(PfctlError::OrphanCounters(1))
        );
        let rule = "block drop in quick on bridge100 all\n";
        for bad in [
            "  [ Evaluations: x Packets: 1 Bytes: 1 States: 0 ]",
            "  [ Evaluations: 1 Packets: 1 Bytes: 1 ]",
            "  [ Evaluations: 1 Packets: 1 Bytes: 1 States: 0 Extra: 1 ]",
            "  [ Evaluations: 1 Bytes: 1 Packets: 1 States: 0 ]",
            "  [ Evaluations: -1 Packets: 1 Bytes: 1 States: 0 ]",
        ] {
            assert_eq!(
                parse_rules(&format!("{rule}{bad}\n")),
                Err(PfctlError::BadCounters(2)),
                "{bad}"
            );
        }
        assert_eq!(
            parse_rules(&format!("{rule}  something else\n")),
            Err(PfctlError::UnknownIndented(2))
        );
    }

    #[test]
    fn odd_labels_do_not_panic() {
        for (line, want) in [
            ("pass all label \"\"", Some("")),
            ("pass all label \"unterminated", None),
            ("pass all label", None),
            ("pass all label \"a\" label \"b\"", Some("a")),
            ("pass all label \"é/ü\"", Some("é/ü")),
            ("pass all", None),
        ] {
            let r = parse_rules(line).unwrap();
            assert_eq!(r[0].label.as_deref(), want, "{line}");
        }
    }

    #[test]
    fn anchors_one_per_line() {
        let out = "  com.apple/000.faber-vm-wall\n  com.apple/250.ApplicationFirewall\n\n";
        assert_eq!(
            parse_anchors(out),
            vec![
                "com.apple/000.faber-vm-wall".to_string(),
                "com.apple/250.ApplicationFirewall".to_string()
            ]
        );
        assert!(parse_anchors("").is_empty());
    }

    #[test]
    fn interfaces_and_their_skip_flag() {
        let out = "all\nbridge100\nbridge101 (skip)\negress\nen0\nlo\nlo0 (skip)\n";
        let got = parse_interfaces(out).unwrap();
        assert_eq!(got.len(), 7);
        assert_eq!(
            got[2],
            PfInterface {
                name: "bridge101".into(),
                skipped: true
            }
        );
        assert!(!got[1].skipped);
        assert!(got[6].skipped);
    }

    #[test]
    fn interface_listing_tolerates_detail_and_refuses_empty() {
        let out = "all\nbridge100 (skip)\n\tCleared:     Thu Jan  1 00:00:00 1970\n\tReferences:  [ States:  0  Rules: 3 ]\n\n";
        let got = parse_interfaces(out).unwrap();
        assert_eq!(got.len(), 2);
        assert!(got[1].skipped);
        for bad in ["", "\n", "bridge100\nlo0 (skip)\n", "  all\n"] {
            assert_eq!(
                parse_interfaces(bad),
                Err(PfctlError::NoInterfaceListing),
                "{bad:?}"
            );
        }
    }

    /// The shape `sudo pfctl` prints on macOS 27 before setup (#206), with
    /// synthetic values: the interface group comes back as `ALL`, and each rule
    /// carries an `Inserted:` line beside its counters.
    #[test]
    fn root_output_from_macos_27_parses() {
        let interfaces = "ALL\nanpi0\nap1\nawdl0\nbridge0\nen0\nen1\nlo0\nutun0\n";
        let got = parse_interfaces(interfaces).unwrap();
        assert_eq!(got.len(), 9);
        assert!(got.iter().all(|i| !i.skipped));

        let rules = "\
scrub-anchor \"com.apple/*\" all fragment reassemble
  [ Evaluations: 3000000000  Packets: 0         Bytes: 0           States: 0     ]
  [ Inserted: uid 0 pid 100 ]
anchor \"com.apple/*\" all
  [ Evaluations: 100000000  Packets: 0         Bytes: 0           States: 0     ]
  [ Inserted: uid 0 pid 100 ]
anchor \"com.apple.internet-sharing\" all
  [ Evaluations: 100000000  Packets: 200000000  Bytes: 800000000000  States: 100   ]
  [ Inserted: uid 0 pid 200 ]
";
        let got = parse_rules(rules).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].text, "anchor \"com.apple/*\" all");
        assert_eq!(got[1].counters.as_ref().unwrap().evaluations, 100000000);
        assert_eq!(got[2].counters.as_ref().unwrap().bytes, 800000000000);

        let anchors = "  com.apple\n  com.apple/200.AirDrop\n  com.apple/250.ApplicationFirewall\n  com.apple.internet-sharing\n  com.apple.internet-sharing/shared_v4\n";
        assert_eq!(parse_anchors(anchors)[1], "com.apple/200.AirDrop");
    }

    /// The shape of `sudo pfctl -v -s Interfaces` on macOS 27 after setup, with
    /// `bridge100` bound (#206, #309); a synthetic listing, interface names
    /// only. The group is `ALL`, no line carries `(skip)` (the stock `pf.conf`
    /// has no `set skip`), the bound bridge is listed, and so is `bridge101`, a
    /// bridge from a VM that stopped and no longer exists.
    pub(crate) const ROOT_INTERFACES_BRIDGE100_BOUND: &str = "\
ALL
anpi0
ap1
awdl0
bridge0
bridge100
bridge101
en0
en1
gif0
llw0
lo0
stf0
utun0
utun1
utun2
vmenet0
";

    #[test]
    fn root_interface_listing_with_a_bridge_bound_parses() {
        let got = parse_interfaces(ROOT_INTERFACES_BRIDGE100_BOUND).unwrap();
        assert_eq!(got.len(), 17);
        assert_eq!(got[0].name, "ALL");
        assert!(got.iter().all(|i| !i.skipped));
        // The bound bridge, and the stale one pf keeps.
        for name in ["bridge100", "bridge101"] {
            assert!(got.iter().any(|i| i.name == name), "{name}");
        }
    }

    #[test]
    fn rdr_pass_and_binat_pass_bypass_the_filter() {
        for bypass in [
            "rdr pass on bridge100 inet proto tcp from any to 192.168.64.1 port = 80 -> 127.0.0.1 port 8080",
            "rdr pass log on en0 inet proto tcp from any to any port = 22 -> 192.0.2.10",
            "binat pass on en0 inet from 192.168.64.2 to any -> 198.51.100.7",
        ] {
            assert!(is_filter_bypass(bypass), "{bypass}");
        }
        for fine in [
            "nat on en0 inet from 192.168.64.0/24 to any -> (en0) round-robin",
            "nat pass on en0 inet from 192.168.64.0/24 to any -> 192.0.2.10",
            "rdr on bridge100 inet proto tcp from any to any port = 80 -> 127.0.0.1",
            "binat on en0 inet from 192.168.64.2 to any -> 198.51.100.7",
            "no rdr on bridge100 inet proto tcp from any to any port = 80",
            "nat-anchor \"com.apple/*\" all",
            "rdr-anchor \"com.apple/*\" all",
            "",
        ] {
            assert!(!is_filter_bypass(fine), "{fine}");
        }
    }

    #[test]
    fn the_enable_token() {
        assert_eq!(parse_enable_token("Token : 1234567\n"), Some(1234567));
        assert_eq!(parse_enable_token("pf enabled\nToken : 42\n"), Some(42));
        assert_eq!(parse_enable_token(""), None);
        assert_eq!(parse_enable_token("Token : x\n"), None);
    }

    #[test]
    fn a_table_listing_is_one_address_per_line() {
        let out = "   127.0.0.1\n   192.0.2.10\n\n   192.168.64.1\n";
        let got = parse_table(out).unwrap();
        assert_eq!(
            got,
            ["127.0.0.1", "192.0.2.10", "192.168.64.1"]
                .map(|a| a.parse::<std::net::Ipv4Addr>().unwrap())
        );
        assert!(parse_table("").unwrap().is_empty());
        for bad in [
            "   10.0.0.0/8\n",
            "   !192.0.2.10\n",
            "   ::1\n",
            "   192.0.2.10 x\n",
        ] {
            assert!(parse_table(bad).is_err(), "{bad:?}");
        }
    }
}
