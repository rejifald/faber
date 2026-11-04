//! The seam between the wall's pure core and the host: what the checker runs,
//! and the tick that observes, repairs and observes again.
//!
//! [`WallHost`] is everything the checker does to the machine. [`tick`] and
//! [`run_once`] drive it, and are tested against a fake host that simulates
//! pf. The real one, [`MacHost`], runs `/sbin/pfctl` and `/sbin/ifconfig` by
//! absolute path, never through a shell, each under a time limit.
//!
//! The checker enforces a [`StoredWall`] and never renders: each tick binds the
//! stored lines to the host as it is now ([`instantiate`]) and makes pf hold
//! exactly that.
//!
//! Any command that fails is an observation like any other: it makes the
//! verdict red. Nothing here panics on what a command printed.
//!
//! # Interfaces the plan does not own
//!
//! `ifconfig -a` lists every bridge on the host, Docker Desktop's and the
//! operator's own VMs' included. Only a bridge holding a stored network's
//! gateway is bound, and a network whose bridge never binds gets no rules at
//! all. That is not a hole the checker can see: what keeps work off a VM on
//! such a network is the in-guest probe ([`super::counters::prove`]), which
//! finds no counters for it and stays unproven.
//!
//! # A reload and the states it leaves behind
//!
//! A `keep state` connection opened while a destination was still allowed
//! survives the reload that blocks it: pf matches the state before any rule.
//! So after every load or host-table replace the checker kills the states
//! from each bound network's subnet (not only its reserved addresses: a state
//! an unreserved or spoofed source opened before the first load must die too)
//! to every host address and every non-global range (`pfctl -k S -k D`), and
//! leaves public states alone. That also ends a granted connection inside a
//! non-global range, which simply reconnects.
//! The subnet-source kill also matches a state the host itself opened towards
//! a guest (its source is the host address, its destination in the subnet,
//! and `pfctl -k` reads a state in either direction). Stock macOS keeps none,
//! but a host whose own rules keep state for host-to-guest traffic loses those
//! connections on every reload; `pfctl -k` offers no way to tell them apart.
//! `pfctl -k` cannot select a port, so a DNS state to a resolver the stored set
//! no longer lists is left to expire rather than killed with every public
//! state.

use std::io;
use std::net::Ipv4Addr;
use std::path::Path;

use super::counters::{bound_slots, Snapshot};
use super::model::{HostInterface, Ipv4Net, LiveView, WallSpec};
use super::pfctl::{
    parse_anchors, parse_enable_token, parse_info, parse_interfaces, parse_rules, parse_table,
    RuleEntry,
};
use super::render::{HOST_TABLE, NONPUBLIC};
use super::report::{CounterReport, VerdictFile};
use super::stored::{instantiate, Instance, StoredWall};
use super::verdict::{decide, plan_remedy, Action, Observation, Reason, Verdict};
use super::{ANCHOR, ANCHOR_PARENT};

/// What a command left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdOutput {
    pub status_ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Everything the checker does to the host. Synchronous: it runs on a thread
/// of its own, or under `spawn_blocking`.
pub trait WallHost {
    /// `ifconfig -a`'s stdout.
    fn ifconfig(&self) -> io::Result<String>;
    /// `pfctl` with `args`, and `stdin` on its standard input.
    fn pfctl(&self, args: &[&str], stdin: Option<&str>) -> io::Result<CmdOutput>;
    /// The stored wall at `path`. It decides the rules root loads, so an
    /// implementation reads it only if root alone could have written it and
    /// every directory above it (ADR-0025's root invariant,
    /// [`super::trust::root_only`]), and only if it parses and its digest
    /// matches ([`StoredWall::from_json`]).
    fn read_stored(&self, path: &Path) -> anyhow::Result<StoredWall>;
    /// Replace the verdict file at `path` atomically, mode 0644, in a
    /// directory only root could have written.
    fn write_verdict(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn now_unix(&self) -> u64;
    /// The time in unix nanoseconds: a load's generation.
    fn now_unix_nanos(&self) -> u64;
    /// The host's monotonic clock in nanoseconds, the one the probe proof
    /// compares against ([`super::reader::now_monotonic_nanos`]).
    fn now_monotonic_nanos(&self) -> u64;
}

/// The argument vectors the checker passes to `pfctl`, in one place so the
/// tests can assert exactly what was issued.
pub const PF_INFO: &[&str] = &["-s", "info"];
pub const PF_MAIN_RULES: &[&str] = &["-s", "rules"];
pub const PF_ANCHORS: &[&str] = &["-a", ANCHOR_PARENT, "-s", "Anchors"];
pub const PF_ANCHOR_RULES: &[&str] = &["-a", ANCHOR, "-v", "-s", "rules"];
pub const PF_TABLE_SHOW: &[&str] = &["-a", ANCHOR, "-t", HOST_TABLE, "-T", "show"];
pub const PF_INTERFACES: &[&str] = &["-v", "-s", "Interfaces"];
pub const PF_MAIN_NAT: &[&str] = &["-s", "nat"];
/// `pfctl -v -s Anchors`: every anchor at every depth, one path per line
/// (`-v` is what makes pfctl recurse).
pub const PF_ALL_ANCHORS: &[&str] = &["-v", "-s", "Anchors"];
pub const PF_ENABLE: &[&str] = &["-E"];
pub const PF_LOAD_ANCHOR: &[&str] = &["-a", ANCHOR, "-f", "-"];

/// Most anchors the translation walk will visit before giving up (red).
const MAX_ANCHORS: usize = 256;

/// `pfctl -a PATH -s Anchors`: one anchor's direct children.
pub fn pf_anchor_children(path: &str) -> [&str; 4] {
    ["-a", path, "-s", "Anchors"]
}

/// `pfctl -a PATH -s nat`: one anchor's translation rules.
pub fn pf_anchor_nat(path: &str) -> [&str; 4] {
    ["-a", path, "-s", "nat"]
}

/// `pfctl -a ANCHOR -t faber_host -T replace ADDR…`: the host table, set to
/// `addrs`. Addresses are typed, so nothing else reaches the argument list.
pub fn table_replace_args(addrs: &[Ipv4Addr]) -> Vec<String> {
    let mut v: Vec<String> = ["-a", ANCHOR, "-t", HOST_TABLE, "-T", "replace"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    v.extend(addrs.iter().map(Ipv4Addr::to_string));
    v
}

/// The `pfctl -k S -k D` calls that end what a load newly forbids: for each
/// bound network, states from anywhere in its subnet to every host address
/// and every non-global range. By subnet, not by reserved address, so a state
/// an unreserved or spoofed source opened before the first load dies too. A
/// slot's public states are never killed.
pub fn kill_args(spec: &WallSpec) -> Vec<[String; 4]> {
    let mut blocked: Vec<String> = spec.host_addrs.iter().map(Ipv4Addr::to_string).collect();
    blocked.extend(NONPUBLIC.iter().map(|&(addr, prefix)| {
        Ipv4Net::new(addr, prefix)
            .expect("NONPUBLIC entries are networks")
            .to_string()
    }));
    let mut out = Vec::new();
    for bound in &spec.networks {
        for d in &blocked {
            out.push([
                "-k".to_string(),
                bound.net.subnet.to_string(),
                "-k".to_string(),
                d.clone(),
            ]);
        }
    }
    out
}

/// What the checker remembers between ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckerState {
    /// The unix-nanosecond time this checker started, then of its last
    /// successful anchor load or host-table replace. Starting at the start
    /// time, not 0, means a restarted checker never repeats a generation, so
    /// counters read across a restart never compare as one load's (fail
    /// closed).
    pub load_generation: u64,
}

impl CheckerState {
    /// A checker starting now.
    pub fn start(host: &dyn WallHost) -> Self {
        Self {
            load_generation: host.now_unix_nanos().max(1),
        }
    }

    /// A load or replace just succeeded: a new generation, strictly later.
    fn bump(&mut self, host: &dyn WallHost) {
        self.load_generation = host
            .now_unix_nanos()
            .max(self.load_generation.saturating_add(1));
    }
}

/// What one tick did and concluded.
#[derive(Debug, Clone)]
pub struct TickOutcome {
    pub verdict: Verdict,
    pub file: VerdictFile,
    /// The repairs attempted, in order.
    pub applied: Vec<Action>,
    /// The stored set as bound on the last observation, if it bound.
    pub instance: Option<Instance>,
}

/// `ifconfig -a` on macOS, reduced to each interface's IPv4 addresses. A
/// header line is `name: flags=…`; an address line is
/// `\tinet ADDR [--> PEER] netmask 0xHEX [broadcast B]`. IPv6 is ignored, and
/// an alias is just another `inet` line.
///
/// A header or `inet` line it cannot read is an error, never skipped: a
/// missed address is one the rules would fail to block. Every other indented
/// line (`ether`, `inet6`, `media:`, `status:`, `member:`, `options=`, `nd6`,
/// a `Configuration:` block) is ignored.
pub fn parse_ifconfig(out: &str) -> Result<Vec<HostInterface>, String> {
    let mut interfaces: Vec<HostInterface> = Vec::new();
    for (i, line) in out.lines().enumerate() {
        let n = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            let header = line
                .split_once(':')
                .filter(|(name, rest)| {
                    !name.is_empty()
                        && !name.contains(char::is_whitespace)
                        && rest.trim_start().starts_with("flags=")
                })
                .ok_or_else(|| format!("line {n}: not an interface header: {}", shown(line)))?;
            interfaces.push(HostInterface {
                name: header.0.to_string(),
                addrs: Vec::new(),
            });
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() != Some(&"inet") {
            continue;
        }
        let bad = || format!("line {n}: unreadable inet line: {}", shown(line));
        let current = interfaces
            .last_mut()
            .ok_or_else(|| format!("line {n}: an inet line before any interface"))?;
        let addr: Ipv4Addr = words.get(1).and_then(|w| w.parse().ok()).ok_or_else(bad)?;
        let prefix = words
            .iter()
            .position(|w| *w == "netmask")
            .and_then(|i| words.get(i + 1))
            .and_then(|m| prefix_of(m))
            .ok_or_else(bad)?;
        current.addrs.push((addr, prefix));
    }
    Ok(interfaces)
}

fn shown(line: &str) -> String {
    line.chars()
        .take(80)
        .collect::<String>()
        .escape_debug()
        .to_string()
}

/// `0xffffff00` → 24; `None` for a mask that is not contiguous.
fn prefix_of(mask: &str) -> Option<u8> {
    let bits = u32::from_str_radix(mask.strip_prefix("0x")?, 16).ok()?;
    let ones = bits.leading_ones();
    (bits.checked_shl(ones).unwrap_or(0) == 0).then_some(ones as u8)
}

/// The output of a `pfctl` call that must succeed.
fn pfctl_ok(host: &dyn WallHost, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let shown = format!("pfctl {}", args.join(" "));
    match host.pfctl(args, stdin) {
        Err(e) => Err(format!("{shown}: {e}")),
        Ok(out) if !out.status_ok => {
            let why = out.stderr.lines().rfind(|l| !l.trim().is_empty());
            Err(format!(
                "{shown} failed: {}",
                why.unwrap_or("no message").escape_debug()
            ))
        }
        Ok(out) => Ok(out.stdout),
    }
}

/// One look at the host: the stored rules bound to it, and what pf actually
/// holds. `Err` is the first thing that could not be read. `counters_at` is
/// set to the time just before the anchor's rules and counters were read.
fn observe(
    host: &dyn WallHost,
    stored: &StoredWall,
    counters_at: &mut CountersAt,
) -> Result<Observation, Reason> {
    let out = host
        .ifconfig()
        .map_err(|e| Reason::CommandFailed(format!("ifconfig -a: {e}")))?;
    let live = LiveView {
        interfaces: parse_ifconfig(&out).map_err(Reason::InterfacesUnreadable)?,
    };
    let instance = instantiate(stored, &live);
    let any_bound = instance.as_ref().is_ok_and(|i| !i.spec.networks.is_empty());
    let pf = parse_info(&pfctl_ok(host, PF_INFO, None).map_err(Reason::CommandFailed)?)
        .map_err(|e| Reason::CommandFailed(e.to_string()))?;
    let rules = |args: &[&str]| -> Result<Vec<RuleEntry>, Reason> {
        parse_rules(&pfctl_ok(host, args, None).map_err(Reason::CommandFailed)?)
            .map_err(|e| Reason::CommandFailed(e.to_string()))
    };
    let main_rules = rules(PF_MAIN_RULES)?;
    let anchors = parse_anchors(&pfctl_ok(host, PF_ANCHORS, None).map_err(Reason::CommandFailed)?);
    *counters_at = CountersAt {
        unix_nanos: host.now_unix_nanos(),
        monotonic_nanos: host.now_monotonic_nanos(),
    };
    let anchor_rules = rules(PF_ANCHOR_RULES)?;
    // The table, the interface listing (skips, and the bound bridges being in
    // it) and filter-skipping translations matter only where a VM can send,
    // and with nothing bound nothing can.
    let mut host_table = None;
    let mut pf_interfaces = Vec::new();
    let mut translation: Vec<(String, Vec<RuleEntry>)> = Vec::new();
    if any_bound {
        // A table that does not exist yet fails `-T show`: that is a table to
        // repair, not a check that could not run.
        host_table = Some(
            pfctl_ok(host, PF_TABLE_SHOW, None)
                .and_then(|out| parse_table(&out).map_err(|e| e.to_string())),
        );
        pf_interfaces =
            parse_interfaces(&pfctl_ok(host, PF_INTERFACES, None).map_err(Reason::CommandFailed)?)
                .map_err(|e| Reason::CommandFailed(e.to_string()))?;
        // A filter-skipping translation can sit in any anchor at any depth
        // that an `rdr-anchor` reaches, the operator's own included, so every
        // one is read. A listing or read that fails is red, never a skip.
        translation.push((String::new(), rules(PF_MAIN_NAT)?));
        // `-v -s Anchors` should list every depth, but that was written from
        // pfctl's documentation, not a root run. So each listed anchor's own
        // children are asked for too: a nested translation anchor is never
        // missed whether or not the first listing recursed.
        let mut all =
            parse_anchors(&pfctl_ok(host, PF_ALL_ANCHORS, None).map_err(Reason::CommandFailed)?);
        let mut seen = std::collections::BTreeSet::new();
        let mut next = 0;
        while next < all.len() {
            let path = all[next].clone();
            next += 1;
            if !seen.insert(path.clone()) {
                continue;
            }
            if seen.len() > MAX_ANCHORS {
                return Err(Reason::CommandFailed(format!(
                    "more than {MAX_ANCHORS} pf anchors"
                )));
            }
            let kids = parse_anchors(
                &pfctl_ok(host, &pf_anchor_children(&path), None).map_err(Reason::CommandFailed)?,
            );
            for kid in kids {
                // A child listed bare is relative to its parent.
                let full = if kid.starts_with(&format!("{path}/")) {
                    kid
                } else {
                    format!("{path}/{kid}")
                };
                all.push(full);
            }
            let nat = rules(&pf_anchor_nat(&path))?;
            translation.push((path, nat));
        }
    }
    Ok(Observation {
        pf,
        main_rules,
        anchors,
        anchor_rules,
        pf_interfaces,
        translation,
        host_table,
        instance,
    })
}

/// When the checker read the anchor's counters, on both clocks.
#[derive(Debug, Clone, Copy, Default)]
struct CountersAt {
    unix_nanos: u64,
    monotonic_nanos: u64,
}

fn judge_observation(obs: &Result<Observation, Reason>) -> Verdict {
    match obs {
        Ok(obs) => decide(obs),
        Err(why) => Verdict::Red(vec![why.clone()]),
    }
}

fn instance_of(obs: &Result<Observation, Reason>) -> Option<&Instance> {
    obs.as_ref().ok().and_then(|o| o.instance.as_ref().ok())
}

/// The counters to publish from `obs`: only on a green verdict, so a snapshot
/// is never of rules other than the stored set bound to this host.
fn counters_of(
    obs: &Result<Observation, Reason>,
    verdict: &Verdict,
    read_at: CountersAt,
) -> Option<CounterReport> {
    if !verdict.is_green() {
        return None;
    }
    let obs = obs.as_ref().ok()?;
    let instance = obs.instance.as_ref().ok()?;
    Some(CounterReport {
        read_at_unix_nanos: read_at.unix_nanos,
        read_at_monotonic_nanos: read_at.monotonic_nanos,
        networks: bound_slots(&instance.spec),
        packets: Snapshot::from_rules(&obs.anchor_rules, 0).to_text(),
    })
}

fn outcome(
    host: &dyn WallHost,
    stored: &StoredWall,
    state: &CheckerState,
    verdict: Verdict,
    applied: Vec<Action>,
    instance: Option<Instance>,
    counters: Option<CounterReport>,
) -> TickOutcome {
    let pending = instance
        .as_ref()
        .map(|i| i.spec.pending.clone())
        .unwrap_or_default();
    let file = VerdictFile::new(
        &verdict,
        stored.rules_version,
        &stored.digest,
        &pending,
        host.now_unix(),
        state.load_generation,
    )
    .with_counters(counters);
    TickOutcome {
        verdict,
        file,
        applied,
        instance,
    }
}

/// Observe, decide, repair what root may, then observe and decide again. The
/// verdict returned is the second one, with any repair that failed added to it.
/// Nothing here renders: the rules loaded are `stored`'s, bound to the host.
pub fn tick(host: &dyn WallHost, stored: &StoredWall, state: &mut CheckerState) -> TickOutcome {
    let mut counters_at = CountersAt::default();
    let obs = observe(host, stored, &mut counters_at);
    let verdict = judge_observation(&obs);
    let actions = plan_remedy(&verdict);
    let instance = instance_of(&obs).cloned();
    if actions.is_empty() {
        let counters = counters_of(&obs, &verdict, counters_at);
        return outcome(host, stored, state, verdict, actions, instance, counters);
    }

    let mut failures = Vec::new();
    let mut changed = false;
    let mut loaded = false;
    for action in &actions {
        match action {
            Action::EnablePf => {
                // Runs only when pf is disabled. `-E` takes an enable
                // reference, and `pfctl -d` drops every reference, so repairs
                // after repeated operator disables do not pile references up.
                // The token is logged, never released (no `-X`): releasing it
                // would let pf switch off again.
                match pfctl_ok(host, PF_ENABLE, None) {
                    Ok(out) => match parse_enable_token(&out) {
                        Some(token) => tracing::info!(token, "faber wall: enabled pf"),
                        None => tracing::info!("faber wall: enabled pf, no token printed"),
                    },
                    Err(why) => failures.push(Reason::CommandFailed(why)),
                }
            }
            // Both are planned only from reasons that need a bound instance.
            Action::LoadAnchor => {
                if let Some(inst) = &instance {
                    match pfctl_ok(host, PF_LOAD_ANCHOR, Some(&inst.text)) {
                        Ok(_) => {
                            state.bump(host);
                            changed = true;
                            loaded = true;
                        }
                        Err(why) => failures.push(Reason::CommandFailed(why)),
                    }
                }
            }
            // A load redefines the table from the instance's own definition,
            // so a replace after a successful load would repeat it.
            Action::ReplaceHostTable if loaded => {}
            Action::ReplaceHostTable => {
                if let Some(inst) = &instance {
                    let args = table_replace_args(&inst.spec.host_addrs);
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    match pfctl_ok(host, &args, None) {
                        Ok(_) => {
                            state.bump(host);
                            changed = true;
                        }
                        Err(why) => failures.push(Reason::CommandFailed(why)),
                    }
                }
            }
        }
    }
    if changed {
        for args in instance
            .as_ref()
            .map(|i| kill_args(&i.spec))
            .unwrap_or_default()
        {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(why) = pfctl_ok(host, &args, None) {
                failures.push(Reason::CommandFailed(why));
            }
        }
    }

    let obs = observe(host, stored, &mut counters_at);
    let mut verdict = judge_observation(&obs);
    if !failures.is_empty() {
        let mut reasons = verdict.reasons().to_vec();
        reasons.extend(failures);
        verdict = Verdict::Red(reasons);
    }
    let instance = instance_of(&obs).cloned();
    let counters = counters_of(&obs, &verdict, counters_at);
    outcome(host, stored, state, verdict, actions, instance, counters)
}

/// One check, end to end: read the stored wall, tick, write the verdict. A
/// stored wall that cannot be read is red and changes nothing in pf. Only the
/// verdict write can fail this; a reader then sees the old file age into red.
pub fn run_once(
    host: &dyn WallHost,
    state: &mut CheckerState,
    stored_path: &Path,
    verdict_path: &Path,
) -> io::Result<TickOutcome> {
    let outcome = match host.read_stored(stored_path) {
        Ok(stored) => tick(host, &stored, state),
        Err(e) => {
            let verdict = Verdict::Red(vec![Reason::StoredInvalid(format!(
                "{}: {e:#}",
                stored_path.display()
            ))]);
            let file =
                VerdictFile::new(&verdict, 0, "", &[], host.now_unix(), state.load_generation);
            TickOutcome {
                verdict,
                file,
                applied: Vec::new(),
                instance: None,
            }
        }
    };
    host.write_verdict(verdict_path, &outcome.file.to_json())?;
    Ok(outcome)
}

/// Run `program` directly, with a fixed environment, and kill it if it
/// outlives `timeout`. Output is read on threads so a full pipe cannot
/// stall the child. The checker runs `pfctl` and `ifconfig` through it, and
/// `sudo faber grant` the host's address listing.
#[cfg(unix)]
pub(crate) fn run_bounded(
    program: &str,
    args: &[&str],
    stdin: Option<&str>,
    timeout: std::time::Duration,
) -> io::Result<CmdOutput> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LC_ALL", "C")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let writer = match (stdin, child.stdin.take()) {
        (Some(text), Some(mut pipe)) => {
            let text = text.to_owned();
            Some(std::thread::spawn(move || pipe.write_all(text.as_bytes())))
        }
        _ => None,
    };
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{program} did not finish in {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if let Some(w) = writer {
        // A child that exits without reading its input breaks the pipe;
        // its status already says whether that mattered.
        let _ = w.join();
    }
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    Ok(CmdOutput {
        status_ok: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(target_os = "macos")]
pub use mac::MacHost;

#[cfg(target_os = "macos")]
mod mac {
    use std::io::{self, Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use anyhow::Context;

    use super::super::stored::StoredWall;
    use super::super::trust::root_only;
    use super::{run_bounded, CmdOutput, WallHost};
    use crate::setup::exe::chain_of;

    const PFCTL: &str = "/sbin/pfctl";
    const IFCONFIG: &str = "/sbin/ifconfig";
    /// Longer than any healthy `pfctl` call; a wedged one is killed and the
    /// tick goes red.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// The real host: macOS's own `pfctl` and `ifconfig`.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct MacHost;

    impl WallHost for MacHost {
        fn ifconfig(&self) -> io::Result<String> {
            let out = run_bounded(IFCONFIG, &["-a"], None, TIMEOUT)?;
            if out.status_ok {
                Ok(out.stdout)
            } else {
                Err(io::Error::other(format!(
                    "exited unsuccessfully: {}",
                    out.stderr.trim().escape_debug()
                )))
            }
        }

        fn pfctl(&self, args: &[&str], stdin: Option<&str>) -> io::Result<CmdOutput> {
            run_bounded(PFCTL, args, stdin, TIMEOUT)
        }

        fn read_stored(&self, path: &Path) -> anyhow::Result<StoredWall> {
            let shown = path.display();
            root_owned(path).with_context(|| format!("refusing the stored wall {shown}"))?;
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW)
                .open(path)
                .with_context(|| format!("opening {shown}"))?;
            // Judged again by the descriptor read from, not only by the name.
            let meta = f.metadata()?;
            if !meta.file_type().is_file() {
                anyhow::bail!("{shown} is not a regular file");
            }
            root_only(&[crate::setup::exe::PathFacts {
                path: path.to_path_buf(),
                uid: meta.uid(),
                gid: meta.gid(),
                mode: meta.mode(),
            }])
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("refusing the stored wall {shown}"))?;
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)
                .with_context(|| format!("reading {shown}"))?;
            StoredWall::from_json(&bytes).with_context(|| format!("parsing {shown}"))
        }

        fn write_verdict(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .ok_or_else(|| io::Error::other("the verdict path has no directory"))?;
            root_owned(dir).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing to write into {}: {e}", dir.display()),
                )
            })?;
            write_atomic(dir, path, bytes)
        }

        fn now_unix(&self) -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        }

        fn now_unix_nanos(&self) -> u64 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        }

        fn now_monotonic_nanos(&self) -> u64 {
            crate::wall::reader::now_monotonic_nanos()
        }
    }

    /// macOS's `O_NOFOLLOW`: an open of a symlink fails instead of following it.
    const O_NOFOLLOW: i32 = 0x0100;

    /// `Ok` when `path` is absolute, names itself with no symlink anywhere
    /// along it, and root alone could have written it and every directory
    /// above it.
    fn root_owned(path: &Path) -> anyhow::Result<()> {
        if !path.is_absolute() {
            anyhow::bail!("{} is not an absolute path", path.display());
        }
        let canonical = std::fs::canonicalize(path)?;
        if canonical != path {
            anyhow::bail!(
                "{} is not canonical (it resolves to {}): a symlink lies along it",
                path.display(),
                canonical.display()
            );
        }
        let chain = chain_of(path)?;
        root_only(&chain).map_err(anyhow::Error::msg)
    }

    /// Write `bytes` to a new temp file in `dir`, never following a symlink
    /// planted there, then rename it over `path`.
    fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("the verdict path has no file name"))?;
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            name.to_string_lossy(),
            std::process::id()
        ));
        // A leftover from a killed run would make `create_new` fail.
        let _ = std::fs::remove_file(&tmp);
        let result = (|| {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(O_NOFOLLOW)
                .mode(0o644)
                .open(&tmp)?;
            f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
            f.write_all(bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_bounded_command_returns_its_output() {
            let out = run_bounded("/bin/cat", &[], Some("hello\n"), TIMEOUT).unwrap();
            assert!(out.status_ok);
            assert_eq!(out.stdout, "hello\n");
            let out = run_bounded("/usr/bin/false", &[], None, TIMEOUT).unwrap();
            assert!(!out.status_ok);
        }

        #[test]
        fn the_verdict_write_is_atomic_and_0644() {
            use std::os::unix::fs::MetadataExt;
            let dir = std::env::temp_dir().join(format!("faber-wall-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("verdict.json");
            write_atomic(&dir, &path, b"one").unwrap();
            write_atomic(&dir, &path, b"two").unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"two");
            let mode = std::fs::metadata(&path).unwrap().mode() & 0o7777;
            assert_eq!(mode, 0o644);
            let leftovers = std::fs::read_dir(&dir).unwrap().count();
            assert_eq!(leftovers, 1);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn the_temp_file_never_follows_a_planted_symlink() {
            let dir = std::env::temp_dir().join(format!("faber-wall-ln-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("verdict.json");
            let target = dir.join("target");
            let tmp = dir.join(format!(".verdict.json.{}.tmp", std::process::id()));
            std::os::unix::fs::symlink(&target, &tmp).unwrap();
            // The leftover is removed first, so the write still succeeds and
            // the link's target is never created.
            write_atomic(&dir, &path, b"x").unwrap();
            assert!(!target.exists());
            assert_eq!(std::fs::read(&path).unwrap(), b"x");
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_stored_wall_or_verdict_dir_the_caller_could_write_is_refused() {
            // Under `cargo test` the temp dir and the file are the tester's,
            // not root's, so the trust rule refuses them.
            if crate::platform::record::current_euid() == 0 {
                return;
            }
            let dir = std::env::temp_dir().join(format!("faber-wall-plan-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let dir = std::fs::canonicalize(&dir).unwrap();
            let path = dir.join("wall.json");
            std::fs::write(&path, br#"{"networks":[]}"#).unwrap();
            let e = format!("{:#}", MacHost.read_stored(&path).unwrap_err());
            assert!(e.contains("not root"), "{e}");
            let e = MacHost
                .write_verdict(&dir.join("verdict.json"), b"{}")
                .unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
            assert!(!dir.join("verdict.json").exists());
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_relative_or_symlinked_path_is_refused() {
            let e = format!(
                "{:#}",
                MacHost.read_stored(Path::new("wall.json")).unwrap_err()
            );
            assert!(e.contains("not an absolute path"), "{e}");
            // `/etc` is a symlink to `/private/etc` on macOS, and root's own.
            let e = format!(
                "{:#}",
                MacHost.read_stored(Path::new("/etc/hosts")).unwrap_err()
            );
            assert!(e.contains("not canonical"), "{e}");
            // The canonical path is root's alone, so it gets as far as parsing.
            let e = format!(
                "{:#}",
                MacHost
                    .read_stored(Path::new("/private/etc/hosts"))
                    .unwrap_err()
            );
            assert!(e.contains("parsing"), "{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::super::model::bind;
    use super::super::model::tests::{iface, plan};
    use super::super::render::tests::stored;
    use super::super::report::State;
    use super::*;

    /// `ifconfig -a` on a host with one bridge up, example addresses only.
    const IFCONFIG_ONE_BRIDGE: &str = "\
lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
\toptions=1203<RXCSUM,TXCSUM,TXSTATUS,SW_TIMESTAMP>
\tinet 127.0.0.1 netmask 0xff000000
\tinet6 ::1 prefixlen 128
\tinet6 fe80::1%lo0 prefixlen 64 scopeid 0x1
\tnd6 options=201<PERFORMNUD,DAD>
gif0: flags=8010<POINTOPOINT,MULTICAST> mtu 1280
stf0: flags=0<> mtu 1280
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\toptions=6460<TSO4,TSO6,CHANNEL_IO,PARTIAL_CSUM,ZEROINVERT_CSUM>
\tether 02:00:00:00:00:01
\tinet6 fe80::1c2b:3d4e:5f60:7a8b%en0 prefixlen 64 secured scopeid 0xb
\tinet 192.0.2.10 netmask 0xffffff00 broadcast 192.0.2.255
\tinet 198.51.100.7 netmask 0xffffff00 broadcast 198.51.100.255
\tnd6 options=201<PERFORMNUD,DAD>
\tmedia: autoselect (1000baseT <full-duplex>)
\tstatus: active
utun3: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280
\tinet 100.64.0.5 --> 100.64.0.5 netmask 0xffffffff
bridge100: flags=8a63<UP,BROADCAST,SMART,RUNNING,ALLMULTI,SIMPLEX,MULTICAST> mtu 1500
\toptions=3<RXCSUM,TXCSUM>
\tether 02:00:00:00:00:64
\tinet 192.168.64.1 netmask 0xffffff00 broadcast 192.168.64.255
\tinet6 fe80::1%bridge100 prefixlen 64 scopeid 0x12
\tConfiguration:
\t\tid 0:0:0:0:0:0 priority 0 hellotime 0 fwddelay 0
\t\tmaxage 0 holdcnt 0 proto stp maxaddr 100 timeout 1200
\tmember: vmenet0 flags=3<LEARNING,DISCOVER>
\t        ifmaxaddr 0 port 19 priority 0 path cost 0
\tnd6 options=201<PERFORMNUD,DAD>
\tmedia: autoselect
\tstatus: active
";

    #[test]
    fn ifconfig_is_reduced_to_ipv4_per_interface() {
        let got = parse_ifconfig(IFCONFIG_ONE_BRIDGE).unwrap();
        let names: Vec<&str> = got.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["lo0", "gif0", "stf0", "en0", "utun3", "bridge100"]);
        assert_eq!(got[0], iface("lo0", &[("127.0.0.1", 8)]));
        assert!(got[1].addrs.is_empty());
        assert_eq!(
            got[3],
            iface("en0", &[("192.0.2.10", 24), ("198.51.100.7", 24)])
        );
        assert_eq!(got[4], iface("utun3", &[("100.64.0.5", 32)]));
        assert_eq!(got[5], iface("bridge100", &[("192.168.64.1", 24)]));
        assert_eq!(parse_ifconfig("").unwrap(), vec![]);
    }

    #[test]
    fn an_unreadable_header_or_inet_line_is_an_error() {
        let header = "en0: flags=8863<UP> mtu 1500\n";
        for bad in [
            "\tinet 192.0.2.1 netmask 0xffffff00\n".to_string(),
            "x: notflags\n".into(),
            "no colon here\n".into(),
            ": flags=8863<UP> mtu 1500\n".into(),
            "en 0: flags=8863<UP> mtu 1500\n".into(),
            format!("{header}\tinet notanaddress netmask 0xffffff00\n"),
            format!("{header}\tinet\n"),
            format!("{header}\tinet 192.0.2.10 netmask 0xff00ff00\n"),
            format!("{header}\tinet 192.0.2.11 netmask zz\n"),
            format!("{header}\tinet 192.0.2.12\n"),
            format!("{header}\tinet 192.0.2.13 netmask\n"),
        ] {
            assert!(parse_ifconfig(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(prefix_of("0x00000000"), Some(0));
        assert_eq!(prefix_of("0xffffffff"), Some(32));
        assert_eq!(prefix_of("0xfffffffe"), Some(31));
    }

    #[test]
    fn a_foreign_bridge_is_read_and_left_unbound() {
        // Another VM manager's NAT bridge: read like any interface, its
        // address named as a host address, and never bound or red.
        let mut out = IFCONFIG_ONE_BRIDGE.to_string();
        out.push_str(
            "bridge105: flags=8a63<UP,BROADCAST,SMART,RUNNING> mtu 1500\n\tinet 172.31.250.1 netmask 0xffffff00 broadcast 172.31.250.255\n\tmember: vmenet3 flags=3<LEARNING,DISCOVER>\n",
        );
        let live = LiveView {
            interfaces: parse_ifconfig(&out).unwrap(),
        };
        let spec = bind(&plan(), &live).unwrap();
        assert_eq!(spec.networks.len(), 1);
        assert!(spec.host_addrs.contains(&"172.31.250.1".parse().unwrap()));
    }

    /// A host that simulates pf: `-E` enables it, a load replaces the anchor's
    /// rules and its table and makes the anchor exist, `-T replace` sets the
    /// table. Every `pfctl` call is recorded.
    struct FakeHost {
        ifconfig: RefCell<String>,
        enabled: Cell<bool>,
        main_rules: RefCell<String>,
        siblings: Vec<String>,
        /// Anchors outside `com.apple`, at any depth, which only the
        /// recursive listing shows.
        others: RefCell<Vec<String>>,
        /// `-v -s Anchors` lists only the top level (no recursion).
        flat_listing: Cell<bool>,
        /// `-a PATH -s Anchors` prints children without the parent's path.
        bare_children: Cell<bool>,
        anchor: RefCell<Option<Vec<String>>>,
        table: RefCell<Option<Vec<String>>>,
        pf_interfaces: RefCell<String>,
        nat: RefCell<Vec<(String, String)>>,
        fail: RefCell<Vec<Vec<String>>>,
        ifconfig_fails: Cell<bool>,
        stored: Option<StoredWall>,
        calls: RefCell<Vec<(Vec<String>, Option<String>)>>,
        written: RefCell<Vec<Vec<u8>>>,
        clock_nanos: Cell<u64>,
    }

    const STOCK_MAIN: &str =
        "scrub-anchor \"com.apple/*\" all fragment reassemble\nanchor \"com.apple/*\" all\n";
    const STOCK_NAT: &str = "nat-anchor \"com.apple/*\" all\nrdr-anchor \"com.apple/*\" all\n";
    const APPLE_FW: &str = "com.apple/250.ApplicationFirewall";
    /// The stock host's addresses, sorted.
    const STOCK_ADDRS: [&str; 5] = [
        "100.64.0.5",
        "127.0.0.1",
        "192.0.2.10",
        "192.168.64.1",
        "198.51.100.7",
    ];

    impl FakeHost {
        fn new() -> Self {
            Self {
                ifconfig: RefCell::new(IFCONFIG_ONE_BRIDGE.into()),
                enabled: Cell::new(true),
                main_rules: RefCell::new(STOCK_MAIN.into()),
                siblings: vec![APPLE_FW.into()],
                others: RefCell::new(Vec::new()),
                flat_listing: Cell::new(false),
                bare_children: Cell::new(false),
                anchor: RefCell::new(None),
                table: RefCell::new(None),
                pf_interfaces: RefCell::new("all\nbridge100\nen0\nlo0 (skip)\n".into()),
                nat: RefCell::new(vec![(String::new(), STOCK_NAT.into())]),
                fail: RefCell::new(Vec::new()),
                ifconfig_fails: Cell::new(false),
                stored: Some(stored()),
                calls: RefCell::new(Vec::new()),
                written: RefCell::new(Vec::new()),
                clock_nanos: Cell::new(1_800_000_000_000_000_000),
            }
        }

        /// A host whose anchor already holds the stored rules, bound.
        fn walled() -> Self {
            let host = Self::new();
            let _ = tick(&host, &stored(), &mut CheckerState::start(&host));
            host.calls.borrow_mut().clear();
            host
        }

        fn fail(&self, args: &[&str]) {
            self.fail
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.borrow().iter().map(|(a, _)| a.clone()).collect()
        }

        fn issued(&self, args: &[&str]) -> usize {
            self.calls().iter().filter(|c| *c == args).count()
        }

        fn kills(&self) -> Vec<Vec<String>> {
            self.calls()
                .into_iter()
                .filter(|c| c.first().map(String::as_str) == Some("-k"))
                .collect()
        }
    }

    impl WallHost for FakeHost {
        fn ifconfig(&self) -> io::Result<String> {
            if self.ifconfig_fails.get() {
                return Err(io::Error::other("ifconfig exploded"));
            }
            Ok(self.ifconfig.borrow().clone())
        }

        fn pfctl(&self, args: &[&str], stdin: Option<&str>) -> io::Result<CmdOutput> {
            self.calls.borrow_mut().push((
                args.iter().map(|s| s.to_string()).collect(),
                stdin.map(str::to_string),
            ));
            let failed = |stderr: &str| {
                Ok(CmdOutput {
                    status_ok: false,
                    stdout: String::new(),
                    stderr: stderr.into(),
                })
            };
            if self.fail.borrow().iter().any(|f| f.iter().eq(args.iter())) {
                return failed("pfctl: DIOCGETRULES: Operation not permitted\n");
            }
            let ok = |stdout: String| {
                Ok(CmdOutput {
                    status_ok: true,
                    stdout,
                    stderr: "No ALTQ support in kernel\nALTQ related functions disabled\n".into(),
                })
            };
            match args {
                a if a == PF_INFO => ok(format!(
                    "Status: {} for 0 days 00:12:34           Debug: Urgent\n",
                    if self.enabled.get() { "Enabled" } else { "Disabled" }
                )),
                a if a == PF_MAIN_RULES => ok(self.main_rules.borrow().clone()),
                a if a == PF_ANCHORS => {
                    let mut list = self.siblings.clone();
                    if self.anchor.borrow().is_some() {
                        list.push(ANCHOR.into());
                    }
                    list.sort();
                    ok(list.iter().map(|a| format!("  {a}\n")).collect())
                }
                a if a == PF_ALL_ANCHORS => {
                    let mut children = self.siblings.clone();
                    if self.anchor.borrow().is_some() {
                        children.push(ANCHOR.into());
                    }
                    children.sort();
                    let list = std::iter::once(ANCHOR_PARENT.to_string())
                        .chain(children)
                        .chain(
                            self.others
                                .borrow()
                                .iter()
                                .filter(|a| !self.flat_listing.get() || !a.contains('/'))
                                .cloned()
                                .collect::<Vec<_>>(),
                        );
                    ok(list.map(|a| format!("  {a}\n")).collect())
                }
                ["-a", path, "-s", "Anchors"] => {
                    // The direct children of `path`, whatever else exists.
                    let mut all = self.siblings.clone();
                    if self.anchor.borrow().is_some() {
                        all.push(ANCHOR.into());
                    }
                    all.extend(self.others.borrow().clone());
                    let prefix = format!("{path}/");
                    let mut kids: Vec<String> = all
                        .into_iter()
                        .filter(|a| {
                            a.strip_prefix(&prefix).is_some_and(|rest| !rest.contains('/'))
                        })
                        .collect();
                    kids.sort();
                    if self.bare_children.get() {
                        kids = kids
                            .into_iter()
                            .map(|k| k.strip_prefix(&prefix).unwrap_or(&k).to_string())
                            .collect();
                    }
                    ok(kids.iter().map(|a| format!("  {a}\n")).collect())
                }
                a if a == PF_ANCHOR_RULES => ok(self
                    .anchor
                    .borrow()
                    .iter()
                    .flatten()
                    .map(|r| {
                        format!(
                            "{r}\n  [ Evaluations: 0         Packets: 0         Bytes: 0           States: 0     ]\n  [ Inserted: uid 0 pid 4242 State Creations: 0     ]\n"
                        )
                    })
                    .collect()),
                a if a == PF_TABLE_SHOW => match &*self.table.borrow() {
                    None => failed("pfctl: Table does not exist.\n"),
                    Some(t) => ok(t.iter().map(|a| format!("   {a}\n")).collect()),
                },
                ["-a", ANCHOR, "-t", HOST_TABLE, "-T", "replace", addrs @ ..] => {
                    *self.table.borrow_mut() = Some(addrs.iter().map(|s| s.to_string()).collect());
                    ok(String::new())
                }
                a if a == PF_INTERFACES => ok(self.pf_interfaces.borrow().clone()),
                ["-s", "nat"] | ["-a", _, "-s", "nat"] => {
                    let anchor = if args.len() == 4 { args[1] } else { "" };
                    let nat = self.nat.borrow();
                    let rules = nat.iter().find(|(a, _)| a == anchor).map(|(_, r)| r.clone());
                    ok(rules.unwrap_or_default())
                }
                ["-k", _, "-k", _] => ok(String::new()),
                a if a == PF_ENABLE => {
                    self.enabled.set(true);
                    ok("Token : 1234567890\n".into())
                }
                a if a == PF_LOAD_ANCHOR => {
                    let text = stdin.unwrap_or("");
                    let mut rules = Vec::new();
                    let mut table = Vec::new();
                    for l in text.lines() {
                        if let Some(rest) = l.strip_prefix("table <faber_host> persist") {
                            let inner = rest.trim().trim_start_matches('{').trim_end_matches('}');
                            table = inner.split_whitespace().map(str::to_string).collect();
                        } else if !l.starts_with('#') && !l.is_empty() {
                            rules.push(l.to_string());
                        }
                    }
                    // A load redefines the anchor's tables with what it lists.
                    *self.anchor.borrow_mut() = Some(rules);
                    *self.table.borrow_mut() = Some(table);
                    ok(String::new())
                }
                _ => Err(io::Error::other("unexpected pfctl call")),
            }
        }

        fn read_stored(&self, _: &Path) -> anyhow::Result<StoredWall> {
            self.stored
                .clone()
                .ok_or_else(|| anyhow::anyhow!("is owned by uid 501, not root"))
        }

        fn write_verdict(&self, _: &Path, bytes: &[u8]) -> io::Result<()> {
            self.written.borrow_mut().push(bytes.to_vec());
            Ok(())
        }

        fn now_unix(&self) -> u64 {
            1_800_000_000
        }

        fn now_unix_nanos(&self) -> u64 {
            let t = self.clock_nanos.get() + 1;
            self.clock_nanos.set(t);
            t
        }

        fn now_monotonic_nanos(&self) -> u64 {
            self.now_unix_nanos()
        }
    }

    fn args(list: &[&[&str]]) -> Vec<Vec<String>> {
        list.iter()
            .map(|a| a.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    /// The reads of one observation with a network bound: the four pf reads,
    /// the host table, the skips, then the translations: the main ruleset's,
    /// the recursive anchor listing, and one `-s nat` per anchor it lists.
    fn observe_calls(ours_loaded: bool) -> Vec<Vec<String>> {
        let mut v = args(&[
            PF_INFO,
            PF_MAIN_RULES,
            PF_ANCHORS,
            PF_ANCHOR_RULES,
            PF_TABLE_SHOW,
            PF_INTERFACES,
            PF_MAIN_NAT,
            PF_ALL_ANCHORS,
            // Each listed anchor: its children, then its translations.
            &pf_anchor_children(ANCHOR_PARENT),
            &pf_anchor_nat(ANCHOR_PARENT),
        ]);
        if ours_loaded {
            v.extend(args(&[&pf_anchor_children(ANCHOR), &pf_anchor_nat(ANCHOR)]));
        }
        v.extend(args(&[
            &pf_anchor_children(APPLE_FW),
            &pf_anchor_nat(APPLE_FW),
        ]));
        v
    }

    fn addrs(list: &[&str]) -> Vec<Ipv4Addr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }

    /// The kills a load on a host with these addresses issues: network 1's
    /// subnet, to the host's addresses, then to every non-global range.
    fn kills_for(host_addrs: &[&str]) -> Vec<Vec<String>> {
        host_addrs
            .iter()
            .map(|s| s.to_string())
            .chain(
                NONPUBLIC
                    .iter()
                    .map(|&(a, p)| Ipv4Net::new(a, p).unwrap().to_string()),
            )
            .map(|d| vec!["-k".into(), "192.168.64.0/24".into(), "-k".into(), d])
            .collect()
    }

    fn stock_kills() -> Vec<Vec<String>> {
        kills_for(&STOCK_ADDRS)
    }

    /// No `pfctl` call ever writes outside our own anchor.
    fn assert_main_ruleset_untouched(host: &FakeHost) {
        for (call, _) in host.calls.borrow().iter() {
            if call
                .iter()
                .any(|a| a == "-f" || a == "-F" || a == "-d" || a == "-X" || a == "-T")
            {
                assert_eq!(&call[..2], ["-a", ANCHOR], "{call:?}");
                assert!(
                    call == &args(&[PF_LOAD_ANCHOR])[0]
                        || call == &args(&[PF_TABLE_SHOW])[0]
                        || call[2..6] == ["-t", HOST_TABLE, "-T", "replace"],
                    "{call:?}"
                );
            }
        }
    }

    #[test]
    fn a_first_tick_loads_the_stored_rules_kills_newly_blocked_states_and_turns_green() {
        let host = FakeHost::new();
        let mut state = CheckerState::start(&host);
        let started = state.load_generation;
        let out = tick(&host, &stored(), &mut state);
        assert_eq!(out.verdict, Verdict::Green, "{:?}", out.verdict);
        // The load's table definition fills the table, so no replace follows.
        assert_eq!(
            out.applied,
            vec![Action::LoadAnchor, Action::ReplaceHostTable]
        );
        let mut want = observe_calls(false);
        let load_at = want.len();
        want.extend(args(&[PF_LOAD_ANCHOR]));
        want.extend(stock_kills());
        want.extend(observe_calls(true));
        assert_eq!(host.calls(), want);
        let loaded = host.calls.borrow()[load_at].1.clone().unwrap();
        let inst = out.instance.clone().unwrap();
        assert_eq!(loaded, inst.text);
        assert_eq!(
            *host.table.borrow(),
            Some(STOCK_ADDRS.map(String::from).to_vec())
        );
        assert_eq!(out.file.state, State::Green);
        // Network 2's bridge is not up.
        assert_eq!(out.file.pending_networks, vec![2]);
        assert_main_ruleset_untouched(&host);
        // The load stamped a later generation, and the file carries it.
        assert!(state.load_generation > started);
        assert_eq!(out.file.load_generation, state.load_generation);
        // The file names the stored set, not anything this binary rendered.
        assert_eq!(out.file.digest, stored().digest);
        assert_eq!(out.file.rules_version, stored().rules_version);
    }

    #[test]
    fn a_green_tick_publishes_the_anchors_counters_and_a_red_one_none() {
        let host = FakeHost::new();
        let mut state = CheckerState::start(&host);
        let out = tick(&host, &stored(), &mut state);
        assert_eq!(out.verdict, Verdict::Green, "{:?}", out.verdict);
        let c = out.file.counters.clone().expect("green publishes counters");
        let inst = out.instance.clone().unwrap();
        assert_eq!(c.networks, bound_slots(&inst.spec));
        assert!(!c.packets.is_empty());
        // Every key is a label faber writes, and the snapshot is this load's.
        let snap = c.snapshot(&out.file).unwrap();
        assert_eq!(snap.generation, state.load_generation);
        assert!(snap
            .packets
            .keys()
            .any(|l| matches!(l, super::super::counters::Label::Host { net: 1, .. })));
        assert!(c.read_at_unix_nanos > 0);
        assert!(c.read_at_monotonic_nanos > 0);
        // It survives the file's own round trip.
        let back: VerdictFile = serde_json::from_slice(&out.file.to_json()).unwrap();
        assert_eq!(back.counters, Some(c));

        // A tick that cannot see the host is red, and publishes nothing.
        host.ifconfig_fails.set(true);
        let out = tick(&host, &stored(), &mut state);
        assert!(!out.verdict.is_green());
        assert_eq!(out.file.counters, None);
        assert!(!String::from_utf8(out.file.to_json())
            .unwrap()
            .contains("counters"));
    }

    #[test]
    fn the_checker_loads_the_stored_lines_not_its_own_rendering() {
        // A stored set this release would never render: one extra rule.
        let mut s = stored();
        let extra = "block drop in quick on {if} inet from 192.168.64.2 to 203.0.113.5 label \"faber-wall/n1/s1/host\"";
        s.networks[0].lines.insert(7, extra.into());
        s.digest = StoredWall::digest_of(&s.networks);
        let s = StoredWall::from_json(&s.to_json()).unwrap();
        let host = FakeHost::new();
        let out = tick(&host, &s, &mut CheckerState::start(&host));
        assert!(out.verdict.is_green(), "{:?}", out.verdict);
        let loaded = host.anchor.borrow().clone().unwrap();
        assert_eq!(loaded[7], extra.replace("{if}", "bridge100"));
        assert_eq!(out.file.digest, s.digest);
    }

    #[test]
    fn kills_never_touch_a_slots_public_states() {
        let host = FakeHost::new();
        let _ = tick(&host, &stored(), &mut CheckerState::start(&host));
        let kills = host.kills();
        // One bound network: its whole subnet as the source, never a slot's
        // address alone, and never to anywhere public.
        assert_eq!(kills.len(), 5 + NONPUBLIC.len());
        for k in &kills {
            assert_eq!(k[1], "192.168.64.0/24");
            assert_ne!(k[3], "0.0.0.0/0");
            assert_ne!(k[3], "any");
        }
        let empty = WallSpec {
            networks: Vec::new(),
            host_addrs: Vec::new(),
            pending: Vec::new(),
        };
        assert_eq!(kill_args(&empty), Vec::<[String; 4]>::new());
    }

    #[test]
    fn a_walled_host_issues_only_reads_and_keeps_its_generation() {
        let host = FakeHost::walled();
        let mut state = CheckerState {
            load_generation: 42,
        };
        let out = tick(&host, &stored(), &mut state);
        assert_eq!(out.verdict, Verdict::Green);
        assert!(out.applied.is_empty());
        assert_eq!(host.calls(), observe_calls(true));
        assert_eq!(state.load_generation, 42);
        assert_eq!(out.file.load_generation, 42);
    }

    #[test]
    fn generations_never_repeat_across_checker_restarts() {
        let host = FakeHost::walled();
        let a = CheckerState::start(&host);
        let b = CheckerState::start(&host);
        assert_ne!(a, b);
        assert!(a.load_generation > 0);
        // Neither restart reloads a walled host, so each keeps its own.
        let (mut a2, mut b2) = (a, b);
        let fa = tick(&host, &stored(), &mut a2).file.load_generation;
        let fb = tick(&host, &stored(), &mut b2).file.load_generation;
        assert_ne!(fa, fb);
        // Even a clock that reads 0 gives a non-zero start.
        struct Zero;
        impl WallHost for Zero {
            fn ifconfig(&self) -> io::Result<String> {
                unreachable!()
            }
            fn pfctl(&self, _: &[&str], _: Option<&str>) -> io::Result<CmdOutput> {
                unreachable!()
            }
            fn read_stored(&self, _: &Path) -> anyhow::Result<StoredWall> {
                unreachable!()
            }
            fn write_verdict(&self, _: &Path, _: &[u8]) -> io::Result<()> {
                unreachable!()
            }
            fn now_unix(&self) -> u64 {
                0
            }
            fn now_unix_nanos(&self) -> u64 {
                0
            }
            fn now_monotonic_nanos(&self) -> u64 {
                0
            }
        }
        assert_eq!(CheckerState::start(&Zero).load_generation, 1);
        let mut s = CheckerState { load_generation: 9 };
        s.bump(&Zero);
        assert_eq!(s.load_generation, 10);
    }

    #[test]
    fn pf_disabled_is_re_enabled_without_a_reload_or_kills() {
        let host = FakeHost::walled();
        host.enabled.set(false);
        let mut state = CheckerState::start(&host);
        let started = state.load_generation;
        let out = tick(&host, &stored(), &mut state);
        assert_eq!(out.verdict, Verdict::Green);
        assert_eq!(out.applied, vec![Action::EnablePf]);
        let mut want = observe_calls(true);
        want.extend(args(&[PF_ENABLE]));
        want.extend(observe_calls(true));
        assert_eq!(host.calls(), want);
        assert!(host.kills().is_empty());
        assert_eq!(state.load_generation, started);
    }

    #[test]
    fn tampered_rules_are_reloaded_and_each_reload_is_a_new_generation() {
        let host = FakeHost::walled();
        let mut state = CheckerState::start(&host);
        let mut last = state.load_generation;
        for _ in 1..=2 {
            if let Some(rules) = host.anchor.borrow_mut().as_mut() {
                rules[3] = "pass in quick on bridge100 all".into();
            }
            host.calls.borrow_mut().clear();
            let out = tick(&host, &stored(), &mut state);
            assert_eq!(out.verdict, Verdict::Green);
            assert_eq!(out.applied, vec![Action::LoadAnchor]);
            assert_eq!(host.issued(PF_LOAD_ANCHOR), 1);
            assert_eq!(host.kills(), stock_kills());
            assert!(state.load_generation > last);
            last = state.load_generation;
        }
    }

    #[test]
    fn a_failing_kill_is_red() {
        let host = FakeHost::new();
        let first = stock_kills()[0].clone();
        host.fail.borrow_mut().push(first.clone());
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        let reasons = out.verdict.reasons();
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert!(
            matches!(&reasons[0], Reason::CommandFailed(m) if m.contains(&first.join(" "))),
            "{reasons:?}"
        );
        // Every other kill was still issued.
        assert_eq!(host.kills(), stock_kills());
    }

    #[test]
    fn a_new_host_address_replaces_the_table_not_the_rules() {
        let host = FakeHost::walled();
        let rules_before = host.anchor.borrow().clone();
        let mut state = CheckerState::start(&host);
        let started = state.load_generation;
        host.ifconfig.borrow_mut().push_str(
            "en1: flags=8863<UP,BROADCAST,RUNNING> mtu 1500\n\tinet 203.0.113.5 netmask 0xffffff00 broadcast 203.0.113.255\n",
        );
        let out = tick(&host, &stored(), &mut state);
        assert_eq!(out.verdict, Verdict::Green, "{:?}", out.verdict);
        assert_eq!(out.applied, vec![Action::ReplaceHostTable]);
        assert_eq!(host.issued(PF_LOAD_ANCHOR), 0);
        assert_eq!(*host.anchor.borrow(), rules_before);
        let mut now = STOCK_ADDRS.to_vec();
        now.push("203.0.113.5");
        let replace = table_replace_args(&addrs(&now));
        let replace: Vec<&str> = replace.iter().map(String::as_str).collect();
        assert_eq!(host.issued(&replace), 1);
        assert_eq!(host.kills(), kills_for(&now));
        // A replace is a new generation too.
        assert!(state.load_generation > started);
        assert_eq!(out.file.load_generation, state.load_generation);
    }

    #[test]
    fn an_emptied_or_unreadable_table_is_replaced() {
        for table in [Some(vec![]), None] {
            let host = FakeHost::walled();
            *host.table.borrow_mut() = table;
            let out = tick(&host, &stored(), &mut CheckerState::start(&host));
            assert_eq!(out.verdict, Verdict::Green, "{:?}", out.verdict);
            assert_eq!(out.applied, vec![Action::ReplaceHostTable]);
            assert_eq!(
                *host.table.borrow(),
                Some(STOCK_ADDRS.map(String::from).to_vec())
            );
        }
        // A replace that fails stays red.
        let host = FakeHost::walled();
        *host.table.borrow_mut() = Some(vec![]);
        let replace = table_replace_args(&addrs(&STOCK_ADDRS));
        host.fail(&replace.iter().map(String::as_str).collect::<Vec<_>>());
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        let reasons = out.verdict.reasons();
        assert!(
            reasons
                .iter()
                .any(|r| matches!(r, Reason::HostTableDiffers(_))),
            "{reasons:?}"
        );
        assert!(reasons
            .iter()
            .any(|r| matches!(r, Reason::CommandFailed(m) if m.contains("-T replace"))));
        assert!(host.kills().is_empty());
    }

    #[test]
    fn a_new_bridge_gets_rules_and_leaves_pending() {
        let host = FakeHost::walled();
        host.ifconfig.borrow_mut().push_str(
            "bridge101: flags=8a63<UP,BROADCAST,RUNNING> mtu 1500\n\tinet 192.168.65.1 netmask 0xffffff00 broadcast 192.168.65.255\n",
        );
        // A bound bridge must be in pf's listing or the wall is red (#309), so
        // this host's pf lists the new one.
        *host.pf_interfaces.borrow_mut() = "all\nbridge100\nbridge101\nen0\nlo0 (skip)\n".into();
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(out.verdict, Verdict::Green);
        assert!(out.file.pending_networks.is_empty());
        // The load carried the new gateway in its table, so no replace.
        assert_eq!(
            out.applied,
            vec![Action::LoadAnchor, Action::ReplaceHostTable]
        );
        assert_eq!(
            host.calls()
                .iter()
                .filter(|c| c.contains(&"replace".to_string()))
                .count(),
            0
        );
        assert!(host
            .anchor
            .borrow()
            .iter()
            .flatten()
            .any(|r| r.contains("on bridge101")));
        assert_eq!(
            host.issued(&["-k", "192.168.65.0/24", "-k", "10.0.0.0/8"]),
            1
        );
    }

    #[test]
    fn unreadable_ifconfig_is_red_and_issues_nothing() {
        let host = FakeHost::walled();
        host.ifconfig.borrow_mut().push_str(
            "bridge101: flags=8a63<UP> mtu 1500\n\tinet 192.168.65.1 netmask 0xfff0ff00\n",
        );
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert!(matches!(
            out.verdict.reasons(),
            [Reason::InterfacesUnreadable(m)] if m.contains("inet")
        ));
        assert!(out.applied.is_empty());
        assert!(host.calls().is_empty());
    }

    #[test]
    fn a_skipped_bridge_is_red_and_repairs_nothing() {
        let host = FakeHost::walled();
        *host.pf_interfaces.borrow_mut() = "all\nbridge100 (skip)\nlo0 (skip)\n".into();
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(
            out.verdict,
            Verdict::Red(vec![Reason::InterfaceSkipped("bridge100".into())])
        );
        assert!(out.applied.is_empty());
        assert_main_ruleset_untouched(&host);
    }

    #[test]
    fn a_bound_bridge_pf_does_not_list_is_red_until_pf_lists_it() {
        let host = FakeHost::walled();
        let mut state = CheckerState::start(&host);
        *host.pf_interfaces.borrow_mut() = "all\nen0\nlo0 (skip)\n".into();
        let out = tick(&host, &stored(), &mut state);
        assert_eq!(
            out.verdict,
            Verdict::Red(vec![Reason::InterfaceUnlisted("bridge100".into())])
        );
        assert_eq!(out.file.state, State::Red);
        assert!(out.file.counters.is_none());
        // Nothing to repair: the listing is pf's own.
        assert!(out.applied.is_empty());
        assert_main_ruleset_untouched(&host);

        // The red is not remembered: with the same checker state, the next
        // tick judges what pf lists then, and writes that.
        *host.pf_interfaces.borrow_mut() = "all\nbridge100\nen0\nlo0 (skip)\n".into();
        let out = run_once(
            &host,
            &mut state,
            Path::new("wall.json"),
            Path::new("verdict.json"),
        )
        .unwrap();
        assert_eq!(out.verdict, Verdict::Green);
        let written: VerdictFile = serde_json::from_slice(&host.written.borrow()[0]).unwrap();
        assert_eq!(written.state, State::Green);
        assert!(written.reasons.is_empty());
    }

    #[test]
    fn an_empty_interface_listing_is_red() {
        let host = FakeHost::walled();
        *host.pf_interfaces.borrow_mut() = String::new();
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert!(matches!(
            out.verdict.reasons(),
            [Reason::CommandFailed(m)] if m.contains("all")
        ));
    }

    #[test]
    fn an_rdr_pass_in_a_child_anchor_is_red() {
        let host = FakeHost::walled();
        let rule =
            "rdr pass on bridge100 inet proto tcp from any to any port = 80 -> 127.0.0.1 port 8080";
        host.nat
            .borrow_mut()
            .push((APPLE_FW.into(), format!("{rule}\n")));
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(
            out.verdict,
            Verdict::Red(vec![Reason::FilterBypass {
                anchor: APPLE_FW.into(),
                rule: rule.into()
            }])
        );
        assert!(out.applied.is_empty());
    }

    #[test]
    fn an_rdr_pass_in_a_nested_operator_anchor_is_red() {
        // An operator's `rdr-anchor "opfwd"` in the main ruleset, its
        // bypass two levels down: only the recursive listing finds it.
        let host = FakeHost::walled();
        let rule =
            "rdr pass on bridge100 inet proto tcp from any to any port = 80 -> 127.0.0.1 port 8080";
        host.others
            .borrow_mut()
            .extend(["opfwd".to_string(), "opfwd/inner".to_string()]);
        host.nat
            .borrow_mut()
            .push(("opfwd/inner".into(), format!("{rule}\n")));
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(
            out.verdict,
            Verdict::Red(vec![Reason::FilterBypass {
                anchor: "opfwd/inner".into(),
                rule: rule.into()
            }])
        );
        assert!(out.applied.is_empty());
        let mut want = observe_calls(true);
        want.extend(args(&[
            &pf_anchor_children("opfwd"),
            &pf_anchor_nat("opfwd"),
            &pf_anchor_children("opfwd/inner"),
            &pf_anchor_nat("opfwd/inner"),
        ]));
        assert_eq!(host.calls(), want);
    }

    /// `pfctl -v -s Anchors` was written from documentation, not a root run.
    /// If it lists only the top level, a nested `rdr pass` anchor is still
    /// found through each anchor's own children.
    #[test]
    fn a_non_recursive_listing_still_finds_a_nested_anchor() {
        let rule =
            "rdr pass on bridge100 inet proto tcp from any to any port = 80 -> 127.0.0.1 port 8080";
        let host = FakeHost::walled();
        host.flat_listing.set(true);
        host.others
            .borrow_mut()
            .extend(["opfwd".to_string(), "opfwd/inner".to_string()]);
        host.nat
            .borrow_mut()
            .push(("opfwd/inner".into(), format!("{rule}\n")));
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(
            out.verdict,
            Verdict::Red(vec![Reason::FilterBypass {
                anchor: "opfwd/inner".into(),
                rule: rule.into()
            }])
        );
    }

    #[test]
    fn a_bare_child_name_is_taken_relative_to_its_parent() {
        // pfctl may print a child without its parent's path.
        let rule =
            "rdr pass on bridge100 inet proto tcp from any to any port = 80 -> 127.0.0.1 port 8080";
        let host = FakeHost::walled();
        host.flat_listing.set(true);
        host.bare_children.set(true);
        host.others
            .borrow_mut()
            .extend(["opfwd".to_string(), "opfwd/inner".to_string()]);
        host.nat
            .borrow_mut()
            .push(("opfwd/inner".into(), format!("{rule}\n")));
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert!(matches!(
            out.verdict.reasons(),
            [Reason::FilterBypass { anchor, .. }] if anchor == "opfwd/inner"
        ));
    }

    #[test]
    fn a_failing_anchor_listing_or_nat_read_is_red() {
        for failing in [PF_ALL_ANCHORS, &pf_anchor_nat(APPLE_FW)] {
            let host = FakeHost::walled();
            host.fail(failing);
            let out = tick(&host, &stored(), &mut CheckerState::start(&host));
            assert!(
                matches!(
                    out.verdict.reasons(),
                    [Reason::CommandFailed(m)] if m.contains(&failing.join(" "))
                ),
                "{failing:?}: {:?}",
                out.verdict
            );
            assert!(out.applied.is_empty());
            assert!(host.kills().is_empty());
        }
    }

    #[test]
    fn a_listed_anchor_is_read_once() {
        let host = FakeHost::walled();
        host.others.borrow_mut().push(APPLE_FW.into());
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(out.verdict, Verdict::Green, "{:?}", out.verdict);
        assert_eq!(host.issued(&pf_anchor_nat(APPLE_FW)), 1);
    }

    #[test]
    fn with_nothing_bound_the_table_skips_and_translations_are_not_read() {
        let host = FakeHost::new();
        *host.ifconfig.borrow_mut() =
            "lo0: flags=8049<UP,LOOPBACK> mtu 16384\n\tinet 127.0.0.1 netmask 0xff000000\n".into();
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(out.verdict, Verdict::Green, "{:?}", out.verdict);
        assert_eq!(
            host.calls(),
            args(&[PF_INFO, PF_MAIN_RULES, PF_ANCHORS, PF_ANCHOR_RULES])
        );
        assert_eq!(out.file.pending_networks, vec![1, 2]);
    }

    #[test]
    fn a_missing_anchor_point_stays_red_and_writes_nothing() {
        let host = FakeHost::walled();
        *host.main_rules.borrow_mut() = "pass all flags S/SA keep state\n".into();
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(out.verdict, Verdict::Red(vec![Reason::AnchorPointMissing]));
        assert!(out.applied.is_empty());
        assert_eq!(host.calls(), observe_calls(true));
        assert_eq!(out.file.state, State::Red);
        assert_main_ruleset_untouched(&host);
    }

    #[test]
    fn a_missing_anchor_point_still_reloads_our_own_anchor() {
        let host = FakeHost::new();
        *host.main_rules.borrow_mut() = String::new();
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert_eq!(out.verdict, Verdict::Red(vec![Reason::AnchorPointMissing]));
        assert_eq!(
            out.applied,
            vec![Action::LoadAnchor, Action::ReplaceHostTable]
        );
        assert_main_ruleset_untouched(&host);
    }

    #[test]
    fn a_failing_pfctl_read_is_red_and_repairs_nothing() {
        // The table read is the one exception: see the replace tests.
        let table = args(&[PF_TABLE_SHOW])[0].clone();
        for failing in observe_calls(true).into_iter().filter(|c| *c != table) {
            let host = FakeHost::walled();
            host.enabled.set(false);
            host.fail.borrow_mut().push(failing.clone());
            let out = tick(&host, &stored(), &mut CheckerState::start(&host));
            let reasons = out.verdict.reasons();
            assert_eq!(reasons.len(), 1, "{failing:?}: {reasons:?}");
            assert!(
                matches!(&reasons[0], Reason::CommandFailed(m) if m.contains("Operation not permitted")),
                "{reasons:?}"
            );
            assert!(out.applied.is_empty());
            assert_eq!(host.issued(PF_ENABLE), 0);
        }
    }

    #[test]
    fn a_failing_load_is_reported_and_the_table_is_still_repaired() {
        let host = FakeHost::new();
        host.fail(PF_LOAD_ANCHOR);
        let mut state = CheckerState::start(&host);
        let out = tick(&host, &stored(), &mut state);
        let reasons = out.verdict.reasons();
        assert!(reasons.contains(&Reason::AnchorEmpty), "{reasons:?}");
        assert!(reasons
            .iter()
            .any(|r| matches!(r, Reason::CommandFailed(m) if m.contains("-f -"))));
        // With no load to fill it, the table is replaced on its own.
        assert_eq!(
            *host.table.borrow(),
            Some(STOCK_ADDRS.map(String::from).to_vec())
        );
        assert!(host.anchor.borrow().is_none());
    }

    #[test]
    fn a_failing_ifconfig_is_red_and_issues_nothing() {
        let host = FakeHost::walled();
        host.ifconfig_fails.set(true);
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert!(matches!(
            out.verdict.reasons(),
            [Reason::CommandFailed(m)] if m.contains("ifconfig")
        ));
        assert!(host.calls().is_empty());
    }

    #[test]
    fn unreadable_pfctl_output_is_red() {
        let host = FakeHost::walled();
        *host.main_rules.borrow_mut() =
            "  [ Evaluations: 1 Packets: 1 Bytes: 1 States: 0 ]\n".into();
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert!(matches!(out.verdict.reasons(), [Reason::CommandFailed(_)]));
    }

    #[test]
    fn an_unbindable_stored_wall_is_red_and_keeps_the_loaded_rules() {
        let host = FakeHost::walled();
        let loaded = host.anchor.borrow().clone();
        host.ifconfig.borrow_mut().push_str(
            "bridge102: flags=8a63<UP> mtu 1500\n\tinet 192.168.64.1 netmask 0xffffff00\n",
        );
        let out = tick(&host, &stored(), &mut CheckerState::start(&host));
        assert!(matches!(out.verdict.reasons(), [Reason::StoredInvalid(_)]));
        assert!(out.applied.is_empty());
        assert!(out.instance.is_none());
        assert_eq!(*host.anchor.borrow(), loaded);
    }

    #[test]
    fn run_once_writes_the_verdict_and_an_unreadable_stored_wall_is_red() {
        let host = FakeHost::new();
        let mut state = CheckerState::start(&host);
        let out = run_once(
            &host,
            &mut state,
            Path::new("wall.json"),
            Path::new("verdict.json"),
        )
        .unwrap();
        assert!(out.verdict.is_green());
        let written: VerdictFile = serde_json::from_slice(&host.written.borrow()[0]).unwrap();
        assert_eq!(written, out.file);
        assert_eq!(written.checked_at_unix, 1_800_000_000);
        assert_eq!(written.load_generation, state.load_generation);

        let host = FakeHost {
            stored: None,
            ..FakeHost::new()
        };
        let mut state = CheckerState::start(&host);
        let out = run_once(
            &host,
            &mut state,
            Path::new("wall.json"),
            Path::new("verdict.json"),
        )
        .unwrap();
        assert!(matches!(
            out.verdict.reasons(),
            [Reason::StoredInvalid(m)] if m.contains("not root")
        ));
        assert_eq!((out.file.rules_version, out.file.digest.as_str()), (0, ""));
        assert!(host.calls().is_empty());
        assert_eq!(host.written.borrow().len(), 1);
    }
}
