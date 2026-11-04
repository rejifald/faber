//! Which faber instance minted a runner (#94).
//!
//! Two faber instances can serve one repository with the same labels, and
//! nothing in a runner's name used to say which of them minted it. When a peer's
//! runners died, the healthy instance saw a burst of failures it had not caused,
//! and only a cross-log investigation showed the dead runners were never its own.
//!
//! So every runner faber registers now ends in a short instance id:
//! `faber-{job_id}-{ts}-{inst}`, and carries a `faber-host-{inst}` label.
//!
//! **The id goes last.** Every parser of a runner name — this binary's and every
//! older one a peer may still run — reads the job id from the first field after
//! `faber-` and the mint time from the second. A suffix keeps both where they
//! were, so an older peer reads a new name exactly as it reads its own.
//!
//! **The id is never the hostname.** It is the operator's `[host] instance` name
//! when set, and otherwise a short hash of the hostname: stable across restarts,
//! different between machines, and not a host's name published in every runner
//! listing on GitHub.

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::github::Runner;

/// The longest instance id faber accepts. Short, since it rides in every runner
/// and container name.
pub const MAX_LEN: usize = 16;

/// Whether `s` is an instance id: a lowercase letter, then up to
/// [`MAX_LEN`]` - 1` lowercase letters or digits.
///
/// The leading letter is what keeps the suffix unambiguous: a mint timestamp is
/// all digits, so `faber-7-1000-2` can never be read as carrying an instance. And
/// the alphabet sits inside Docker's own for container names, so the id can go
/// into `faber-runner-{job}-{ts}-{inst}` unescaped.
pub fn valid(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_LEN
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The id derived from a hostname: `h` and the first seven hex digits of a
/// SHA-256 of it. A hash, not the name, so the runner list does not publish it.
pub fn from_hostname(hostname: &str) -> String {
    let digest = Sha256::digest(format!("faber-instance:{}", hostname.trim().to_lowercase()));
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("h{}", &hex[..7])
}

/// The instance id this daemon mints under: the configured name when it is a
/// valid one, else the hostname's hash. The `Err` carries a warning for a
/// configured name that had to be ignored — `faber check-config` refuses it, and
/// a daemon that boots on one warns rather than refusing to start (AGENTS.md).
pub fn resolve(configured: &str, hostname: &str) -> (String, Option<String>) {
    let configured = configured.trim();
    if configured.is_empty() {
        return (from_hostname(hostname), None);
    }
    if valid(configured) {
        return (configured.to_string(), None);
    }
    (
        from_hostname(hostname),
        Some(format!(
            "host.instance {configured:?} is not an instance id (a lowercase letter, then up to \
             {} lowercase letters or digits) — using the hostname's hash instead",
            MAX_LEN - 1
        )),
    )
}

/// This machine's hostname, or an empty string when it cannot be read.
pub fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The runner name faber registers for `job_id`, minted at `ts`, by `instance`.
pub fn runner_name(job_id: u64, ts: u64, instance: &str) -> String {
    format!("faber-{job_id}-{ts}-{instance}")
}

/// The label every runner `instance` mints carries.
pub fn host_label(instance: &str) -> String {
    format!("faber-host-{instance}")
}

/// The instance that minted the runner `name`, or `None` for a name that is not
/// faber's or that predates instance ids (`faber-{job_id}-{ts}`).
pub fn of_runner(name: &str) -> Option<&str> {
    let mut fields = name.strip_prefix("faber-")?.splitn(3, '-');
    let job = fields.next()?;
    let ts = fields.next()?;
    let inst = fields.next()?;
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (numeric(job) && numeric(ts) && valid(inst)).then_some(inst)
}

/// Another faber instance seen minting into a pool this one serves.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PeerView {
    pub instance: String,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// Its runners in each repo's latest listing, by repo: online or offline,
    /// busy or idle.
    pub runners: BTreeMap<String, usize>,
    /// Of those, the online ones running a job, by repo. The popover draws a
    /// running pip per busy runner and an idle one per `idle` runner, so it never
    /// shows work a peer is not doing.
    pub busy: BTreeMap<String, usize>,
    /// Of those, the online ones with no job, by repo.
    pub idle: BTreeMap<String, usize>,
}

/// The most peers remembered. Far past any real fleet; only a bound on a
/// listing that invents instance ids.
const PEERS_CAP: usize = 32;

/// The most lost peer runners remembered, so each is reported once.
const LOST_CAP: usize = 512;

/// One of a peer's runners that went offline while GitHub still had a job
/// assigned to it — the shape that, before #94, read as this host's own failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostPeerRunner {
    pub id: u64,
    pub name: String,
    pub instance: String,
}

/// The peers this instance has seen in GitHub's runner listings, and which of
/// their lost runners it has already reported. In memory only: a restart warns
/// about each peer once more, which is the point of a warning.
///
/// Only a name that carries an instance id counts. A pre-#94 name says nothing
/// about who minted it — it may be this host's own, from before it upgraded — so
/// a peer still on an older release is invisible here until it updates.
#[derive(Debug, Default)]
pub struct Peers {
    seen: BTreeMap<String, PeerView>,
    lost: HashSet<u64>,
    lost_order: VecDeque<u64>,
}

impl Peers {
    /// Fold one repo's runner listing in. Returns each peer seen for the first
    /// time, with its runner count in this listing, for the caller to warn about
    /// once.
    pub fn observe(
        &mut self,
        own: &str,
        repo: &str,
        runners: &[Runner],
        now_ms: u64,
    ) -> Vec<(String, usize)> {
        // Per peer: all its runners, then the online busy and online idle ones.
        let mut counts: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
        for r in runners {
            if let Some(inst) = of_runner(&r.name).filter(|i| *i != own) {
                let c = counts.entry(inst).or_default();
                c.0 += 1;
                if r.status == "online" {
                    if r.busy {
                        c.1 += 1;
                    } else {
                        c.2 += 1;
                    }
                }
            }
        }
        // A peer that has left this repo's listing has no runners in it now.
        for peer in self.seen.values_mut() {
            if !counts.contains_key(peer.instance.as_str()) {
                peer.runners.remove(repo);
                peer.busy.remove(repo);
                peer.idle.remove(repo);
            }
        }
        let mut new = Vec::new();
        for (inst, (n, busy, idle)) in counts {
            if !self.seen.contains_key(inst) {
                if self.seen.len() >= PEERS_CAP {
                    continue;
                }
                self.seen.insert(
                    inst.to_string(),
                    PeerView {
                        instance: inst.to_string(),
                        first_seen_ms: now_ms,
                        last_seen_ms: now_ms,
                        runners: BTreeMap::new(),
                        busy: BTreeMap::new(),
                        idle: BTreeMap::new(),
                    },
                );
                new.push((inst.to_string(), n));
            }
            let peer = self.seen.get_mut(inst).expect("inserted above");
            peer.last_seen_ms = now_ms;
            peer.runners.insert(repo.to_string(), n);
            peer.busy.insert(repo.to_string(), busy);
            peer.idle.insert(repo.to_string(), idle);
        }
        new
    }

    /// The peers' runners in this listing that are `offline` with a job still
    /// assigned, each reported once. This host's own runners never appear: its
    /// own losses are its own outcomes, recorded where it watches them.
    pub fn newly_lost(&mut self, own: &str, runners: &[Runner]) -> Vec<LostPeerRunner> {
        let mut out = Vec::new();
        for r in runners {
            let Some(inst) = of_runner(&r.name).filter(|i| *i != own) else {
                continue;
            };
            if r.status != "offline" || !r.busy || !self.lost.insert(r.id) {
                continue;
            }
            self.lost_order.push_back(r.id);
            if self.lost_order.len() > LOST_CAP {
                if let Some(old) = self.lost_order.pop_front() {
                    self.lost.remove(&old);
                }
            }
            out.push(LostPeerRunner {
                id: r.id,
                name: r.name.clone(),
                instance: inst.to_string(),
            });
        }
        out
    }

    /// Every peer seen, by instance id.
    pub fn view(&self) -> Vec<PeerView> {
        self.seen.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_id_starts_with_a_letter_and_stays_short() {
        for good in ["a", "builder", "h1a2b3c4", "mac2"] {
            assert!(valid(good), "{good:?}");
        }
        for bad in [
            "",
            "2",
            "12345678",
            "Builder",
            "my-host",
            "my_host",
            "a.b",
            "abcdefghijklmnopq",
        ] {
            assert!(!valid(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_derived_id_is_stable_short_and_not_the_hostname() {
        let a = from_hostname("builder.local");
        assert_eq!(a, from_hostname("builder.local"));
        assert_eq!(a, from_hostname("Builder.local\n"), "case and whitespace");
        assert_ne!(a, from_hostname("follower.local"));
        assert!(valid(&a), "{a}");
        assert_eq!(a.len(), 8);
        assert!(!a.contains("builder"));
    }

    #[test]
    fn a_configured_name_wins_and_a_bad_one_falls_back_loudly() {
        assert_eq!(resolve("builder", "x"), ("builder".to_string(), None));
        assert_eq!(resolve("", "x"), (from_hostname("x"), None));
        let (id, warn) = resolve("My Host", "x");
        assert_eq!(id, from_hostname("x"));
        assert!(warn.is_some());
    }

    #[test]
    fn a_new_name_carries_its_instance_and_an_old_one_does_not() {
        let name = runner_name(1001, 1756000000000, "builder");
        assert_eq!(name, "faber-1001-1756000000000-builder");
        assert_eq!(of_runner(&name), Some("builder"));
        assert_eq!(of_runner("faber-1001-1756000000000"), None);
        assert_eq!(
            of_runner("faber-7-1000-2"),
            None,
            "a digit is not an instance"
        );
        assert_eq!(of_runner("faber-runner-7-1000-builder"), None);
        assert_eq!(of_runner("gh-runner-7-1000-builder"), None);
        assert_eq!(host_label("builder"), "faber-host-builder");
    }

    fn runner(id: u64, name: &str, status: &str, busy: bool) -> Runner {
        Runner {
            id,
            name: name.to_string(),
            status: status.to_string(),
            busy,
            labels: Vec::new(),
        }
    }

    #[test]
    fn a_peer_is_warned_about_once_and_its_own_runners_never_count() {
        let mut p = Peers::default();
        let listing = [
            runner(1, "faber-10-1000-builder", "online", true),
            runner(2, "faber-11-1000-follower", "online", true),
            runner(3, "faber-12-1000-follower", "online", false),
            // Pre-#94: could be anyone's, including this host's own.
            runner(4, "faber-13-1000", "online", true),
            runner(5, "laptop-runner", "online", false),
        ];
        assert_eq!(
            p.observe("builder", "owner/demo", &listing, 1_000),
            vec![("follower".to_string(), 2)]
        );
        assert!(p
            .observe("builder", "owner/demo", &listing, 2_000)
            .is_empty());
        let v = p.view();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].instance, "follower");
        assert_eq!(v[0].first_seen_ms, 1_000);
        assert_eq!(v[0].last_seen_ms, 2_000);
        assert_eq!(v[0].runners.get("owner/demo"), Some(&2));
        assert_eq!(v[0].busy.get("owner/demo"), Some(&1));
        assert_eq!(v[0].idle.get("owner/demo"), Some(&1));
        // Its runners leave the listing: still a known peer, with none here now.
        p.observe("builder", "owner/demo", &listing[..1], 3_000);
        assert!(p.view()[0].runners.is_empty());
        assert!(p.view()[0].busy.is_empty() && p.view()[0].idle.is_empty());
    }

    #[test]
    fn an_offline_peer_runner_is_counted_but_neither_busy_nor_idle() {
        let mut p = Peers::default();
        let listing = [
            runner(1, "faber-11-1000-follower", "online", true),
            runner(2, "faber-12-1000-follower", "offline", true),
            runner(3, "faber-13-1000-follower", "offline", false),
        ];
        p.observe("builder", "owner/demo", &listing, 1_000);
        let v = &p.view()[0];
        assert_eq!(v.runners.get("owner/demo"), Some(&3));
        assert_eq!(v.busy.get("owner/demo"), Some(&1));
        assert_eq!(v.idle.get("owner/demo"), Some(&0));
    }

    #[test]
    fn a_peers_offline_runner_holding_a_job_is_reported_once() {
        let mut p = Peers::default();
        let listing = [
            runner(1, "faber-10-1000-builder", "offline", true), // our own
            runner(2, "faber-11-1000-follower", "offline", true), // lost
            runner(3, "faber-12-1000-follower", "offline", false), // idle, dead
            runner(4, "faber-13-1000-follower", "online", true), // working
            runner(5, "faber-14-1000", "offline", true),         // unattributable
        ];
        let lost = p.newly_lost("builder", &listing);
        assert_eq!(
            lost,
            vec![LostPeerRunner {
                id: 2,
                name: "faber-11-1000-follower".to_string(),
                instance: "follower".to_string(),
            }]
        );
        assert!(p.newly_lost("builder", &listing).is_empty(), "once");
    }
}
