//! What the daemon tells the service, and what the service counts.
//!
//! The daemon reaches the service only by `docker exec` (no port is published
//! from the container). It runs `faber-mirror sync` with a [`SyncRequest`] on
//! its stdin; that process hands it to the server over the container's own
//! loopback ([`crate::ADMIN_PORT`], bound to `127.0.0.1`, so no job network can
//! reach it), and prints the [`Stats`] it gets back. Tokens therefore travel on
//! a pipe and in memory only: never an argument, an environment variable, a
//! file or a log line.
//!
//! A sync carries the whole table, so it is idempotent and heals a service that
//! restarted and forgot it: the daemon syncs at every janitor pass.
//!
//! The table has two keys into one grant: the container's token (LFS, which
//! authorizes) and the service's own address on the container's network (npm,
//! which only attributes: [`crate::Peer`]).

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use crate::{Fill, JobFailures, RepoCounters, Stats, SyncRequest};

/// The repo a token speaks for, and the container (by its network) it was
/// minted for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub repo: String,
}

/// How many containers' failures are remembered.
const FAILURES_CAP: usize = 4096;

/// How much of a failure's text is kept.
const FAILURE_CHARS: usize = 300;

/// The service's live settings and its counters.
pub struct Control {
    instance: String,
    tokens: RwLock<HashMap<String, Grant>>,
    /// The service's address on each container's network, to that container.
    addrs: RwLock<HashMap<IpAddr, Grant>>,
    lfs: RwLock<Option<Fill>>,
    npm: RwLock<Option<Fill>>,
    low_disk: AtomicBool,
    counters: Mutex<BTreeMap<String, RepoCounters>>,
    npm_counters: Mutex<BTreeMap<String, RepoCounters>>,
    failures: Mutex<BTreeMap<String, JobFailures>>,
}

impl Default for Control {
    fn default() -> Self {
        Self::new()
    }
}

impl Control {
    pub fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Self {
            instance: format!("{nanos:x}-{}", std::process::id()),
            tokens: RwLock::new(HashMap::new()),
            addrs: RwLock::new(HashMap::new()),
            lfs: RwLock::new(None),
            npm: RwLock::new(None),
            low_disk: AtomicBool::new(false),
            counters: Mutex::new(BTreeMap::new()),
            npm_counters: Mutex::new(BTreeMap::new()),
            failures: Mutex::new(BTreeMap::new()),
        }
    }

    /// Apply a sync. The table becomes exactly its entries: an entry with a
    /// token sets it; one without keeps whatever token and repo the service
    /// already has for that id (a container the daemon adopted across its own
    /// restart, whose token it never knew); every other token is revoked. With
    /// `adopt`, the daemon's first sync after it starts, nothing is revoked: the
    /// entries are added to the table, and the answer's `ids` tell the daemon
    /// which containers to adopt. Addresses follow the same rules: an entry
    /// with one sets it, one without keeps the address the service has for the
    /// id. Returns whether the low-disk flag turned on.
    pub fn apply(&self, req: SyncRequest) -> bool {
        self.apply_addrs(&req);
        let mut tokens = self.tokens.write().unwrap();
        let by_id: HashMap<String, (String, Grant)> = tokens
            .iter()
            .map(|(t, g)| (g.id.clone(), (t.clone(), g.clone())))
            .collect();
        let mut next = if req.adopt {
            tokens.clone()
        } else {
            HashMap::new()
        };
        for e in req.entries {
            match e.token.filter(|t| !t.is_empty()) {
                Some(token) => {
                    next.retain(|_, g| g.id != e.id);
                    next.insert(
                        token,
                        Grant {
                            id: e.id,
                            repo: e.repo,
                        },
                    );
                }
                None => {
                    if let Some((token, grant)) = by_id.get(&e.id) {
                        next.insert(token.clone(), grant.clone());
                    }
                }
            }
        }
        *tokens = next;
        drop(tokens);
        *self.lfs.write().unwrap() = req.lfs;
        *self.npm.write().unwrap() = req.npm;
        let was = self.low_disk.swap(req.low_disk, Ordering::Relaxed);
        req.low_disk && !was
    }

    /// The address half of [`Control::apply`]. An entry that names no repo (a
    /// container the daemon adopted across its restart) keeps the repo the
    /// service already knows for its id, from either table; one the service
    /// knows nothing about is not attributed.
    fn apply_addrs(&self, req: &SyncRequest) {
        let known_repo = |id: &str| -> Option<String> {
            let tokens = self.tokens.read().unwrap();
            tokens.values().find(|g| g.id == id).map(|g| g.repo.clone())
        };
        let mut addrs = self.addrs.write().unwrap();
        let by_id: HashMap<String, (IpAddr, Grant)> = addrs
            .iter()
            .map(|(a, g)| (g.id.clone(), (*a, g.clone())))
            .collect();
        let mut next = if req.adopt {
            addrs.clone()
        } else {
            HashMap::new()
        };
        for e in &req.entries {
            let addr = e
                .addr
                .as_deref()
                .and_then(|a| a.trim().parse::<IpAddr>().ok());
            match addr {
                Some(addr) => {
                    let repo = if e.repo.is_empty() {
                        by_id
                            .get(&e.id)
                            .map(|(_, g)| g.repo.clone())
                            .or_else(|| known_repo(&e.id))
                    } else {
                        Some(e.repo.clone())
                    };
                    let Some(repo) = repo else { continue };
                    next.retain(|_, g| g.id != e.id);
                    next.insert(
                        addr,
                        Grant {
                            id: e.id.clone(),
                            repo,
                        },
                    );
                }
                None => {
                    if let Some((addr, grant)) = by_id.get(&e.id) {
                        next.insert(*addr, grant.clone());
                    }
                }
            }
        }
        *addrs = next;
    }

    /// The grant behind `token`.
    pub fn grant(&self, token: &str) -> Option<Grant> {
        self.tokens.read().unwrap().get(token).cloned()
    }

    /// The container whose network a request arrived on, by the service's own
    /// address there ([`crate::Peer`]). Attributes; never authorizes.
    pub fn by_addr(&self, addr: IpAddr) -> Option<Grant> {
        let addr = match addr {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
            v4 => v4,
        };
        self.addrs.read().unwrap().get(&addr).cloned()
    }

    /// The LFS fill mode, or `None` when LFS is not served.
    pub fn lfs(&self) -> Option<Fill> {
        *self.lfs.read().unwrap()
    }

    /// The npm fill mode, or `None` when npm is not served.
    pub fn npm(&self) -> Option<Fill> {
        *self.npm.read().unwrap()
    }

    /// Whether the daemon reports the host short of disk (ADR-0047 §8).
    pub fn low_disk(&self) -> bool {
        self.low_disk.load(Ordering::Relaxed)
    }

    /// Count an npm request for `repo` ([`crate::UNATTRIBUTED`] when no sync
    /// named the address it came on).
    pub fn count_npm(&self, repo: &str, f: impl FnOnce(&mut RepoCounters)) {
        f(self
            .npm_counters
            .lock()
            .unwrap()
            .entry(repo.to_string())
            .or_default());
    }

    /// Count something for `repo`.
    pub fn count(&self, repo: &str, f: impl FnOnce(&mut RepoCounters)) {
        f(self
            .counters
            .lock()
            .unwrap()
            .entry(repo.to_string())
            .or_default());
    }

    /// Record a request from container `id` that failed because of the mirror
    /// itself, not upstream (ADR-0047 §7).
    pub fn fail(&self, id: &str, why: &str) {
        let mut failures = self.failures.lock().unwrap();
        if failures.len() >= FAILURES_CAP && !failures.contains_key(id) {
            failures.clear();
        }
        let f = failures.entry(id.to_string()).or_default();
        f.count += 1;
        f.last = why.chars().take(FAILURE_CHARS).collect();
    }

    /// What the daemon reads back. `passing_through` and `store` are the
    /// caller's, which knows the store.
    pub fn stats(&self) -> Stats {
        let mut ids: Vec<String> = self
            .tokens
            .read()
            .unwrap()
            .values()
            .map(|g| g.id.clone())
            .chain(self.addrs.read().unwrap().values().map(|g| g.id.clone()))
            .collect();
        ids.sort();
        ids.dedup();
        Stats {
            instance: self.instance.clone(),
            ids,
            repos: self.counters.lock().unwrap().clone(),
            npm: self.npm_counters.lock().unwrap().clone(),
            failures: self.failures.lock().unwrap().clone(),
            ..Stats::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyncEntry;

    fn entry(id: &str, repo: &str, token: Option<&str>) -> SyncEntry {
        SyncEntry {
            id: id.into(),
            repo: repo.into(),
            token: token.map(String::from),
            addr: None,
        }
    }

    fn at(id: &str, repo: &str, addr: Option<&str>) -> SyncEntry {
        SyncEntry {
            addr: addr.map(String::from),
            ..entry(id, repo, None)
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn addresses_attribute_to_a_container_and_follow_the_table() {
        let c = Control::new();
        c.apply(SyncRequest {
            entries: vec![
                at("net-1", "owner/a", Some("172.20.0.3")),
                // An LFS container's address comes with its token.
                SyncEntry {
                    addr: Some("172.21.0.3".into()),
                    ..entry("net-2", "owner/b", Some("t2"))
                },
                at("net-bad", "owner/c", Some("not an address")),
            ],
            npm: Some(Fill::Relaxed),
            ..SyncRequest::default()
        });
        assert_eq!(c.npm(), Some(Fill::Relaxed));
        assert_eq!(c.by_addr(ip("172.20.0.3")).unwrap().repo, "owner/a");
        assert_eq!(c.by_addr(ip("::ffff:172.21.0.3")).unwrap().id, "net-2");
        assert!(c.by_addr(ip("172.22.0.3")).is_none());
        assert_eq!(
            c.stats().ids,
            vec!["net-1", "net-2"],
            "no token, still listed"
        );
        // net-1 sent with no address keeps it; net-2 is gone, so is its address.
        c.apply(SyncRequest {
            entries: vec![at("net-1", "owner/a", None)],
            ..SyncRequest::default()
        });
        assert_eq!(c.by_addr(ip("172.20.0.3")).unwrap().id, "net-1");
        assert!(c.by_addr(ip("172.21.0.3")).is_none());
        assert_eq!(c.npm(), None, "npm off when the sync says so");
        // A replace moved the service to another address on net-1.
        c.apply(SyncRequest {
            entries: vec![at("net-1", "owner/a", Some("172.20.0.9"))],
            ..SyncRequest::default()
        });
        assert!(c.by_addr(ip("172.20.0.3")).is_none());
        assert_eq!(c.by_addr(ip("172.20.0.9")).unwrap().id, "net-1");
    }

    #[test]
    fn a_daemon_restart_keeps_addresses_and_their_repos() {
        let c = Control::new();
        c.apply(SyncRequest {
            entries: vec![at("net-1", "owner/a", Some("172.20.0.3"))],
            ..SyncRequest::default()
        });
        // A fresh daemon adopts, then lists the id with no repo; the inspect it
        // runs before each sync names the address again.
        c.apply(SyncRequest {
            adopt: true,
            ..SyncRequest::default()
        });
        assert_eq!(c.stats().ids, vec!["net-1"]);
        c.apply(SyncRequest {
            entries: vec![at("net-1", "", Some("172.20.0.3"))],
            ..SyncRequest::default()
        });
        assert_eq!(c.by_addr(ip("172.20.0.3")).unwrap().repo, "owner/a");
        // An address for an id the service never knew, with no repo: nothing
        // to attribute it to.
        c.apply(SyncRequest {
            entries: vec![at("net-9", "", Some("172.29.0.3"))],
            ..SyncRequest::default()
        });
        assert!(c.by_addr(ip("172.29.0.3")).is_none());
    }

    #[test]
    fn a_sync_replaces_the_table_and_keeps_adopted_tokens_by_id() {
        let c = Control::new();
        c.apply(SyncRequest {
            entries: vec![
                entry("net-1", "owner/a", Some("t1")),
                entry("net-2", "owner/b", Some("t2")),
            ],
            lfs: Some(Fill::Relaxed),
            ..SyncRequest::default()
        });
        assert_eq!(c.grant("t1").unwrap().repo, "owner/a");
        assert_eq!(c.lfs(), Some(Fill::Relaxed));
        // net-2 is adopted (no token sent); net-1 was revoked; net-3 is new.
        c.apply(SyncRequest {
            entries: vec![
                entry("net-2", "owner/b", None),
                entry("net-3", "owner/a", Some("t3")),
            ],
            ..SyncRequest::default()
        });
        assert!(c.grant("t1").is_none(), "revoked");
        assert_eq!(c.grant("t2").unwrap().id, "net-2", "kept by id");
        assert_eq!(c.grant("t3").unwrap().id, "net-3");
        assert_eq!(c.stats().ids, vec!["net-2", "net-3"]);
        assert_eq!(c.lfs(), None, "LFS off when the sync says so");
    }

    #[test]
    fn a_daemon_restart_adopts_rather_than_revokes() {
        let c = Control::new();
        c.apply(SyncRequest {
            entries: vec![entry("net-1", "owner/a", Some("t1"))],
            ..SyncRequest::default()
        });
        // A fresh daemon knows no tokens: its first sync adopts.
        c.apply(SyncRequest {
            entries: vec![entry("net-2", "owner/b", Some("t2"))],
            adopt: true,
            ..SyncRequest::default()
        });
        assert_eq!(c.stats().ids, vec!["net-1", "net-2"]);
        // Then it lists the adopted id with no token and no repo; the service
        // keeps both.
        c.apply(SyncRequest {
            entries: vec![entry("net-1", "", None), entry("net-2", "owner/b", None)],
            ..SyncRequest::default()
        });
        assert_eq!(c.grant("t1").unwrap().repo, "owner/a");
        assert_eq!(c.grant("t2").unwrap().repo, "owner/b");
    }

    #[test]
    fn low_disk_reports_only_its_rising_edge() {
        let c = Control::new();
        let low = SyncRequest {
            low_disk: true,
            ..SyncRequest::default()
        };
        assert!(c.apply(low.clone()));
        assert!(!c.apply(low));
        assert!(c.low_disk());
        assert!(!c.apply(SyncRequest::default()));
        assert!(!c.low_disk());
    }

    #[test]
    fn failures_and_counters_are_kept_per_container_and_per_repo() {
        let c = Control::new();
        c.fail("net-1", "store read failed");
        c.fail("net-1", "again");
        c.count("owner/a", |r| r.hits += 2);
        c.count_npm("owner/a", |r| r.misses += 1);
        let s = c.stats();
        assert_eq!(s.npm["owner/a"].misses, 1);
        assert_eq!(
            s.repos["owner/a"].misses, 0,
            "LFS and npm are counted apart"
        );
        assert_eq!(s.failures["net-1"].count, 2);
        assert_eq!(s.failures["net-1"].last, "again");
        assert_eq!(s.repos["owner/a"].hits, 2);
    }
}
