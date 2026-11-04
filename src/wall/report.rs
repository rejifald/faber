//! The verdict file: what the root checker writes after every check, and what
//! `_faber` reads before it admits a job and proves a VM's probe
//! ([`super::reader`]).
//!
//! The reader trusts the file only if root alone could have written it, the
//! same rule as the setup record (`crate::platform::record::trust_verdict`).
//! A file `_faber` could write would let the daemon's own account declare the
//! wall green, and so would a directory above it that `_faber` could write:
//! [`judge_at`] also applies ADR-0025's ancestor rule ([`super::trust`]). And the reader fails closed on time: the checker rewrites the
//! file every few seconds, so a stale one means a dead checker, which is red.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::counters::{BoundSlots, Snapshot};
use super::trust::root_only;
use super::verdict::Verdict;
use crate::platform::record::{trust_verdict, MetaFacts};
use crate::setup::exe::PathFacts;

/// The only schema this release writes and reads.
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Green,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictFile {
    pub schema: u32,
    pub state: State,
    /// Each reason, one line apiece. Empty when green.
    pub reasons: Vec<String>,
    /// The rules version of the stored set the checker enforces: the release
    /// that rendered it, not the checker's own ([`super::RULES_VERSION`]).
    /// 0 when no stored set could be read.
    pub rules_version: u32,
    /// The stored set's digest ([`super::stored::StoredWall::digest_of`]);
    /// empty when none could be read.
    pub digest: String,
    /// Networks with no bridge yet. Not red: no VM runs on them.
    pub pending_networks: Vec<u8>,
    pub checked_at_unix: u64,
    /// The checker's start time in unix nanoseconds, then the time of its
    /// last anchor load or host-table replace. Never repeats across restarts.
    /// Counter snapshots are compared only within one generation
    /// ([`super::counters::prove`]).
    pub load_generation: u64,
    /// The anchor's rule counters as this tick read them, on a green tick
    /// only ([`super::counters`]). Absent from a red one, and from a file an
    /// older checker wrote: then no probe can be proven, which fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counters: Option<CounterReport>,
}

/// What the root checker publishes of pf's counters, for the probe proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterReport {
    /// Unix nanoseconds just before the checker read the counters. For
    /// people only: the wall clock can step, so no proof compares it.
    pub read_at_unix_nanos: u64,
    /// The host's monotonic clock just before the checker read the counters
    /// ([`super::reader::now_monotonic_nanos`]). A snapshot proves a probe
    /// only if this is later than the moment the probe was sent, on the same
    /// clock. 0, or absent from an older checker's file, proves nothing.
    #[serde(default)]
    pub read_at_monotonic_nanos: u64,
    /// The networks bound when the counters were read, with their slots.
    pub networks: Vec<BoundSlots>,
    /// Packets per label ([`Snapshot::to_text`]).
    pub packets: BTreeMap<String, u64>,
}

impl CounterReport {
    /// The counters as a snapshot under `file`'s load generation.
    pub fn snapshot(&self, file: &VerdictFile) -> Result<Snapshot, String> {
        Snapshot::from_text(file.load_generation, &self.packets)
    }
}

impl VerdictFile {
    pub fn new(
        verdict: &Verdict,
        rules_version: u32,
        digest: &str,
        pending: &[u8],
        now_unix: u64,
        load_generation: u64,
    ) -> Self {
        Self {
            schema: SCHEMA,
            state: if verdict.is_green() {
                State::Green
            } else {
                State::Red
            },
            reasons: verdict.reasons().iter().map(ToString::to_string).collect(),
            rules_version,
            digest: digest.to_string(),
            pending_networks: pending.to_vec(),
            checked_at_unix: now_unix,
            load_generation,
            counters: None,
        }
    }

    /// This file with `counters` published.
    pub fn with_counters(mut self, counters: Option<CounterReport>) -> Self {
        self.counters = counters;
        self
    }

    pub fn to_json(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec_pretty(self).expect("a VerdictFile always serializes");
        out.push(b'\n');
        out
    }
}

/// What the reader concludes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    Green,
    Red(String),
    /// Green, but with older rules than this release wants. The old rules keep
    /// applying until setup re-runs (ADR-0026); show it, don't stop.
    OutOfDate {
        installed: u32,
        wanted: u32,
    },
}

/// [`judge`], and red as well unless root alone could have written every
/// directory above the file: `ancestors` is the file and each directory up
/// to `/`, as [`crate::setup::exe::chain_of`] gathers them.
pub fn judge_at(
    bytes: &[u8],
    facts: &MetaFacts,
    ancestors: &[PathFacts],
    now_unix: u64,
    max_age_secs: u64,
    wanted_version: u32,
) -> Judgement {
    if let Err(e) = root_only(ancestors) {
        return Judgement::Red(format!("the verdict file is not root's alone: {e}"));
    }
    judge(bytes, facts, now_unix, max_age_secs, wanted_version)
}

/// The reader's judgement of a verdict file's `bytes`, given the facts about
/// the file it read them from.
pub fn judge(
    bytes: &[u8],
    facts: &MetaFacts,
    now_unix: u64,
    max_age_secs: u64,
    wanted_version: u32,
) -> Judgement {
    if let Err(e) = trust_verdict(facts) {
        return Judgement::Red(format!("the verdict file {e}"));
    }
    let file: VerdictFile = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => return Judgement::Red(format!("the verdict file does not parse: {e}")),
    };
    if file.schema != SCHEMA {
        return Judgement::Red(format!(
            "the verdict file has schema {}, and this faber reads only {SCHEMA}",
            file.schema
        ));
    }
    if file.state == State::Red {
        let why = if file.reasons.is_empty() {
            "no reason given".to_string()
        } else {
            file.reasons.join("; ")
        };
        return Judgement::Red(format!("the wall is red: {why}"));
    }
    if now_unix.saturating_sub(file.checked_at_unix) > max_age_secs {
        return Judgement::Red(format!(
            "the verdict is {}s old: the checker has stopped",
            now_unix - file.checked_at_unix
        ));
    }
    if file.checked_at_unix.saturating_sub(now_unix) > max_age_secs {
        return Judgement::Red(format!(
            "the verdict is dated {}s in the future",
            file.checked_at_unix - now_unix
        ));
    }
    if file.rules_version < wanted_version {
        return Judgement::OutOfDate {
            installed: file.rules_version,
            wanted: wanted_version,
        };
    }
    Judgement::Green
}

#[cfg(test)]
mod tests {
    use super::super::render::tests::stored;
    use super::super::verdict::Reason;
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const MAX_AGE: u64 = 30;

    fn trusted() -> MetaFacts {
        MetaFacts {
            is_symlink: false,
            is_regular: true,
            uid: 0,
            mode: 0o100644,
            parent_is_dir: true,
            parent_uid: 0,
            parent_mode: 0o40755,
        }
    }

    fn file(verdict: &Verdict, at: u64) -> VerdictFile {
        let s = stored();
        VerdictFile::new(
            verdict,
            s.rules_version,
            &s.digest,
            &[3],
            at,
            1_700_000_000_000_000_000,
        )
    }

    fn green_bytes(at: u64) -> Vec<u8> {
        file(&Verdict::Green, at).to_json()
    }

    #[test]
    fn the_file_round_trips_and_says_what_it_is() {
        let f = file(&Verdict::Red(vec![Reason::PfDisabled]), NOW);
        let json = String::from_utf8(f.to_json()).unwrap();
        assert!(json.contains("\"state\": \"red\""), "{json}");
        assert!(json.contains("pf is disabled"), "{json}");
        let back: VerdictFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.pending_networks, vec![3]);
        assert_eq!(back.rules_version, super::super::RULES_VERSION);
        assert_eq!(back.digest.len(), 64);
        assert!(
            json.contains("\"load_generation\": 1700000000000000000"),
            "{json}"
        );
    }

    #[test]
    fn a_fresh_green_file_is_green() {
        assert_eq!(
            judge(&green_bytes(NOW - 5), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Green
        );
        // At exactly the limit, still green.
        assert_eq!(
            judge(&green_bytes(NOW - MAX_AGE), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Green
        );
    }

    #[test]
    fn an_untrusted_file_is_red_whatever_it_says() {
        let bytes = green_bytes(NOW);
        let cases = [
            MetaFacts {
                uid: 501,
                ..trusted()
            },
            MetaFacts {
                mode: 0o100664,
                ..trusted()
            },
            MetaFacts {
                is_symlink: true,
                ..trusted()
            },
            MetaFacts {
                parent_uid: 501,
                ..trusted()
            },
            MetaFacts {
                parent_mode: 0o40777,
                ..trusted()
            },
        ];
        for facts in cases {
            assert!(
                matches!(judge(&bytes, &facts, NOW, MAX_AGE, 1), Judgement::Red(_)),
                "{facts:?}"
            );
        }
    }

    #[test]
    fn unparseable_or_unknown_schema_is_red() {
        for bytes in [&b""[..], b"{", b"null", b"{\"schema\":1}"] {
            assert!(matches!(
                judge(bytes, &trusted(), NOW, MAX_AGE, 1),
                Judgement::Red(_)
            ));
        }
        let mut f = file(&Verdict::Green, NOW);
        f.schema = 2;
        assert!(matches!(
            judge(&f.to_json(), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Red(m) if m.contains("schema 2")
        ));
    }

    #[test]
    fn a_red_file_is_red() {
        let f = file(&Verdict::Red(vec![Reason::AnchorPointMissing]), NOW);
        assert!(matches!(
            judge(&f.to_json(), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Red(m) if m.contains("anchor")
        ));
        // A red state with its reasons stripped is still red.
        let mut f = f;
        f.reasons.clear();
        assert!(matches!(
            judge(&f.to_json(), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Red(_)
        ));
    }

    #[test]
    fn a_stale_or_future_file_is_red() {
        assert!(matches!(
            judge(&green_bytes(NOW - MAX_AGE - 1), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Red(m) if m.contains("stopped")
        ));
        assert!(matches!(
            judge(&green_bytes(0), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Red(_)
        ));
        assert!(matches!(
            judge(&green_bytes(NOW + MAX_AGE + 1), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Red(m) if m.contains("future")
        ));
        assert_eq!(
            judge(&green_bytes(NOW + 2), &trusted(), NOW, MAX_AGE, 1),
            Judgement::Green
        );
    }

    #[test]
    fn older_rules_are_out_of_date_and_newer_are_green() {
        let mut f = file(&Verdict::Green, NOW);
        f.rules_version = 1;
        assert_eq!(
            judge(&f.to_json(), &trusted(), NOW, MAX_AGE, 2),
            Judgement::OutOfDate {
                installed: 1,
                wanted: 2
            }
        );
        f.rules_version = 3;
        assert_eq!(
            judge(&f.to_json(), &trusted(), NOW, MAX_AGE, 2),
            Judgement::Green
        );
        // Out of date never hides red.
        f.rules_version = 1;
        f.state = State::Red;
        assert!(matches!(
            judge(&f.to_json(), &trusted(), NOW, MAX_AGE, 2),
            Judgement::Red(_)
        ));
    }

    fn chain(dir_uid: u32, dir_mode: u32) -> Vec<PathFacts> {
        let f = |path: &str, uid: u32, mode: u32| PathFacts {
            path: path.into(),
            uid,
            gid: 0,
            mode,
        };
        vec![
            f("/var/run/faber/wall.json", 0, 0o100644),
            f("/var/run/faber", dir_uid, dir_mode),
            f("/var/run", 0, 0o40755),
            f("/var", 0, 0o40755),
            f("/", 0, 0o40755),
        ]
    }

    #[test]
    fn a_writable_ancestor_makes_any_verdict_red() {
        let bytes = green_bytes(NOW);
        assert_eq!(
            judge_at(&bytes, &trusted(), &chain(0, 0o40755), NOW, MAX_AGE, 1),
            Judgement::Green
        );
        for c in [chain(451, 0o40755), chain(0, 0o40775), chain(0, 0o40777)] {
            assert!(matches!(
                judge_at(&bytes, &trusted(), &c, NOW, MAX_AGE, 1),
                Judgement::Red(m) if m.contains("not root's alone")
            ));
        }
        assert!(matches!(
            judge_at(&bytes, &trusted(), &[], NOW, MAX_AGE, 1),
            Judgement::Red(_)
        ));
        // The file's own facts still count.
        let untrusted = MetaFacts {
            uid: 451,
            ..trusted()
        };
        assert!(matches!(
            judge_at(&bytes, &untrusted, &chain(0, 0o40755), NOW, MAX_AGE, 1),
            Judgement::Red(_)
        ));
    }
}
