//! A claim that is likely wasted ranks last (ADR-0063).
//!
//! While a claim waits for room, the run it belongs to can stop being worth
//! running: its PR closes, moves on, conflicts with its base, or another job of
//! the run fails. This module reads those signals, at most once a minute per PR
//! and per run, and says what `[waste]` makes of them. What happens next — a
//! demote to the waste tier, a skip that cancels the run — is the reconciler's,
//! at the one point it checks: when a claim has passed every gate and is about to
//! be admitted (ADR-0063 §4).
//!
//! Every read fails open. No answer, a failed read, a timeout, a `404` or a
//! missing permission is "not fired", so a broken read leaves a job running as
//! it would without this module.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::{WasteMode, WastePolicy};

/// How long an answer is reused, per PR for the PR read and per run for the jobs
/// read (ADR-0063 §2): a run with twenty jobs reads each once.
pub const CACHE_MS: u64 = 60_000;

/// The longest one read may hold the slot it is checked from (ADR-0063 §4). A read
/// that takes longer is "not fired".
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// One reason a claim is likely wasted (ADR-0063 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Signal {
    /// The PR is closed.
    Closed,
    /// The PR's head has moved past the run's commit.
    Superseded,
    /// The PR conflicts with its base.
    Conflicted,
    /// Another job of the same run already failed.
    RunFailed,
}

impl Signal {
    /// The signal's `[waste]` key, as audit lines and refusals name it.
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Closed => "closed",
            Signal::Superseded => "superseded",
            Signal::Conflicted => "conflicted",
            Signal::RunFailed => "run_failed",
        }
    }

    /// The mode `policy` sets for this signal. `run_failed` never skips: the
    /// config check refuses it (ADR-0063 §6.2), and a file that reached the
    /// daemon without that check demotes instead.
    pub fn mode(self, policy: &WastePolicy) -> WasteMode {
        match self {
            Signal::Closed => policy.closed,
            Signal::Superseded => policy.superseded,
            Signal::Conflicted => policy.conflicted,
            Signal::RunFailed => policy.run_failed.min(WasteMode::Demote),
        }
    }
}

/// What one read of a pull request says, as far as waste goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullFacts {
    /// `open` or `closed`; anything else is no answer.
    pub state: Option<String>,
    /// The commit the PR's head is at now.
    pub head_sha: String,
    /// `None` until GitHub has computed it.
    pub mergeable: Option<bool>,
    /// When it closed, RFC 3339 UTC; `None` is no answer.
    pub closed_at: Option<String>,
}

impl From<crate::github::Pull> for PullFacts {
    fn from(pull: crate::github::Pull) -> Self {
        Self {
            state: pull.state,
            head_sha: pull.head.sha,
            mergeable: pull.mergeable,
            closed_at: pull.closed_at,
        }
    }
}

/// The ids of a run's jobs that concluded `failure`: all the jobs read keeps.
pub fn failed_jobs(jobs: &[crate::github::Job]) -> Vec<u64> {
    jobs.iter()
        .filter(|j| j.conclusion.as_deref() == Some("failure"))
        .map(|j| j.id)
        .collect()
}

/// The claim a check is about.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub repo: &'a str,
    pub pr: u64,
    pub run_id: u64,
    /// The commit the run is for.
    pub head_sha: &'a str,
    pub job_id: u64,
    /// When the run was created, RFC 3339 UTC; `None` is no answer.
    pub run_created_at: Option<&'a str>,
}

/// The signals a PR read fires for a run at `run_head_sha`. Only definite answers
/// fire: `mergeable: null` is not a conflict, and an empty head is not a move.
///
/// `closed` fires only for a run that started before the PR closed. A run created
/// after it was triggered by the close itself (`on: pull_request: types: [closed]`,
/// a preview teardown, a post-merge job) and is not wasted. Either timestamp
/// missing is no answer, so it does not fire. GitHub's RFC 3339 UTC strings
/// compare lexically.
pub fn pull_signals(
    pull: &PullFacts,
    run_head_sha: &str,
    run_created_at: Option<&str>,
) -> Vec<Signal> {
    let mut out = Vec::new();
    if pull.state.as_deref() == Some("closed")
        && matches!((pull.closed_at.as_deref(), run_created_at),
            (Some(closed), Some(created)) if created < closed)
    {
        out.push(Signal::Closed);
    }
    if !pull.head_sha.is_empty() && !run_head_sha.is_empty() && pull.head_sha != run_head_sha {
        out.push(Signal::Superseded);
    }
    if pull.mergeable == Some(false) {
        out.push(Signal::Conflicted);
    }
    out
}

/// What `[waste]` makes of the signals that fired for one claim: the strongest
/// action, and every fired signal that is on, strongest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub action: WasteMode,
    pub signals: Vec<(Signal, WasteMode)>,
}

impl Finding {
    /// The signal the action comes from, for a refusal or a status.
    pub fn signal(&self) -> Signal {
        self.signals[0].0
    }

    /// Every fired signal, comma-separated, for the audit line.
    pub fn names(&self) -> String {
        self.signals
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Judge `fired` against `policy`: `None` when no fired signal is on. When several
/// fire, the strongest action wins (ADR-0063 §3).
pub fn judge(policy: &WastePolicy, fired: &[Signal]) -> Option<Finding> {
    let mut signals: Vec<(Signal, WasteMode)> = fired
        .iter()
        .map(|s| (*s, s.mode(policy)))
        .filter(|(_, m)| *m != WasteMode::Off)
        .collect();
    // Strongest first; within one mode, in the order of the ADR's table.
    signals.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    signals.dedup();
    let action = signals.first()?.1;
    Some(Finding { action, signals })
}

/// One cached answer: when it was read, and what it said (`None`: no answer).
type Cached<T> = (u64, Option<T>);

/// Cached answers by repo and PR or run number.
type Cache<T> = Mutex<HashMap<(String, u64), Cached<T>>>;

/// The reads behind the check, each cached for [`CACHE_MS`].
#[derive(Default)]
pub struct Checker {
    pulls: Cache<PullFacts>,
    runs: Cache<Vec<u64>>,
}

impl Checker {
    /// Check `subject` against `policy` at `now_ms`. `read_pull` and `read_jobs`
    /// are the two reads; each is made only when a signal that needs it is on and
    /// no answer younger than [`CACHE_MS`] is held, and each is cut off at
    /// [`READ_TIMEOUT`]. A read that fails or times out answers `None`, and that is
    /// cached too, so a token without the permission costs one read a minute.
    pub async fn check<P, J>(
        &self,
        policy: &WastePolicy,
        subject: Subject<'_>,
        now_ms: u64,
        read_pull: impl FnOnce() -> P,
        read_jobs: impl FnOnce() -> J,
    ) -> Option<Finding>
    where
        P: Future<Output = Option<PullFacts>>,
        J: Future<Output = Option<Vec<u64>>>,
    {
        if policy.is_off() {
            return None;
        }
        let pull_key = (subject.repo.to_string(), subject.pr);
        let run_key = (subject.repo.to_string(), subject.run_id);
        let pull_read = policy
            .reads_pull()
            .then(|| cached(&self.pulls, &pull_key, now_ms));
        let jobs_read =
            (policy.run_failed != WasteMode::Off).then(|| cached(&self.runs, &run_key, now_ms));
        // Both reads at once, so the slot is held for one timeout at most.
        let (pull, jobs) = tokio::join!(
            async {
                match pull_read {
                    None => None,
                    Some(Some(hit)) => hit,
                    Some(None) => {
                        let got = tokio::time::timeout(READ_TIMEOUT, read_pull())
                            .await
                            .ok()
                            .flatten();
                        store(&self.pulls, pull_key, now_ms, got.clone());
                        got
                    }
                }
            },
            async {
                match jobs_read {
                    None => None,
                    Some(Some(hit)) => hit,
                    Some(None) => {
                        let got = tokio::time::timeout(READ_TIMEOUT, read_jobs())
                            .await
                            .ok()
                            .flatten();
                        store(&self.runs, run_key, now_ms, got.clone());
                        got
                    }
                }
            }
        );
        let mut fired = pull
            .map(|p| pull_signals(&p, subject.head_sha, subject.run_created_at))
            .unwrap_or_default();
        if jobs.is_some_and(|failed| failed.iter().any(|id| *id != subject.job_id)) {
            fired.push(Signal::RunFailed);
        }
        judge(policy, &fired)
    }
}

/// The cached answer for `key`, if one younger than [`CACHE_MS`] is held:
/// `Some(answer)` is a hit, `None` a miss.
fn cached<T: Clone>(cache: &Cache<T>, key: &(String, u64), now_ms: u64) -> Option<Option<T>> {
    let cache = cache.lock().unwrap();
    let (at, answer) = cache.get(key)?;
    (now_ms.saturating_sub(*at) < CACHE_MS).then(|| answer.clone())
}

/// Keep `answer` for `key`, dropping every entry past [`CACHE_MS`] so the cache
/// holds only what a minute of admissions read.
fn store<T>(cache: &Cache<T>, key: (String, u64), now_ms: u64, answer: Option<T>) {
    let mut cache = cache.lock().unwrap();
    cache.retain(|_, (at, _)| now_ms.saturating_sub(*at) < CACHE_MS);
    cache.insert(key, (now_ms, answer));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SHA: &str = "aaaa";
    /// When the fixture run was created: before `CLOSED_AT`.
    const RUN_AT: &str = "2026-11-01T10:00:00Z";
    const CLOSED_AT: &str = "2026-11-01T11:00:00Z";

    fn subject() -> Subject<'static> {
        Subject {
            repo: "owner/demo",
            pr: 7,
            run_id: 100,
            head_sha: SHA,
            job_id: 1,
            run_created_at: Some(RUN_AT),
        }
    }

    fn open() -> PullFacts {
        PullFacts {
            state: Some("open".into()),
            head_sha: SHA.into(),
            mergeable: Some(true),
            closed_at: None,
        }
    }

    fn policy_for(signal: Signal, mode: WasteMode) -> WastePolicy {
        let mut p = WastePolicy::default();
        match signal {
            Signal::Closed => p.closed = mode,
            Signal::Superseded => p.superseded = mode,
            Signal::Conflicted => p.conflicted = mode,
            Signal::RunFailed => p.run_failed = mode,
        }
        p
    }

    /// The PR read and the failed jobs that make `signal` fire.
    fn firing(signal: Signal) -> (PullFacts, Vec<u64>) {
        let mut pull = open();
        let mut failed = Vec::new();
        match signal {
            Signal::Closed => {
                pull.state = Some("closed".into());
                pull.closed_at = Some(CLOSED_AT.into());
            }
            Signal::Superseded => pull.head_sha = "bbbb".into(),
            Signal::Conflicted => pull.mergeable = Some(false),
            Signal::RunFailed => failed.push(2),
        }
        (pull, failed)
    }

    async fn check(
        checker: &Checker,
        policy: &WastePolicy,
        now: u64,
        pull: Option<PullFacts>,
        failed: Option<Vec<u64>>,
    ) -> Option<Finding> {
        checker
            .check(
                policy,
                subject(),
                now,
                || async move { pull },
                || async move { failed },
            )
            .await
    }

    const SIGNALS: [Signal; 4] = [
        Signal::Closed,
        Signal::Superseded,
        Signal::Conflicted,
        Signal::RunFailed,
    ];

    #[tokio::test]
    async fn every_signal_in_every_mode() {
        for signal in SIGNALS {
            for mode in [
                WasteMode::Off,
                WasteMode::Record,
                WasteMode::Demote,
                WasteMode::Skip,
            ] {
                let policy = policy_for(signal, mode);
                let (pull, failed) = firing(signal);
                let got = check(&Checker::default(), &policy, 0, Some(pull), Some(failed)).await;
                let acted = signal.mode(&policy);
                let want = (mode != WasteMode::Off).then(|| Finding {
                    action: acted,
                    signals: vec![(signal, acted)],
                });
                assert_eq!(got, want, "{} = {}", signal.as_str(), mode.as_str());
                // The same mode, with nothing fired: never a finding.
                let quiet =
                    check(&Checker::default(), &policy, 0, Some(open()), Some(vec![])).await;
                assert_eq!(
                    quiet,
                    None,
                    "{} = {} unfired",
                    signal.as_str(),
                    mode.as_str()
                );
            }
        }
    }

    #[tokio::test]
    async fn the_strongest_action_wins() {
        let policy = WastePolicy {
            closed: WasteMode::Record,
            superseded: WasteMode::Skip,
            conflicted: WasteMode::Demote,
            run_failed: WasteMode::Record,
        };
        let pull = PullFacts {
            state: Some("closed".into()),
            head_sha: "bbbb".into(),
            mergeable: Some(false),
            closed_at: Some(CLOSED_AT.into()),
        };
        let got = check(&Checker::default(), &policy, 0, Some(pull), Some(vec![2]))
            .await
            .unwrap();
        assert_eq!(got.action, WasteMode::Skip);
        assert_eq!(got.signal(), Signal::Superseded);
        assert_eq!(got.names(), "superseded,conflicted,closed,run_failed");
    }

    #[tokio::test]
    async fn run_failed_never_skips() {
        let policy = policy_for(Signal::RunFailed, WasteMode::Skip);
        assert_eq!(policy.problems().len(), 1, "the config check refuses it");
        let got = check(&Checker::default(), &policy, 0, None, Some(vec![2]))
            .await
            .unwrap();
        assert_eq!(got.action, WasteMode::Demote);
    }

    #[tokio::test]
    async fn a_signal_that_is_off_never_fires() {
        let policy = policy_for(Signal::Conflicted, WasteMode::Demote);
        let (pull, _) = firing(Signal::Closed);
        assert_eq!(
            check(&Checker::default(), &policy, 0, Some(pull), Some(vec![2])).await,
            None
        );
    }

    #[tokio::test]
    async fn closed_fires_only_for_a_run_older_than_the_close() {
        let policy = policy_for(Signal::Closed, WasteMode::Skip);
        let (pull, _) = firing(Signal::Closed);
        let at = |run_created_at: Option<&'static str>| Subject {
            run_created_at,
            ..subject()
        };
        let run = |subject: Subject<'static>, pull: PullFacts| {
            let policy = &policy;
            async move {
                Checker::default()
                    .check(
                        policy,
                        subject,
                        0,
                        || async move { Some(pull) },
                        || async { Some(vec![]) },
                    )
                    .await
            }
        };
        // Started before the PR closed: stale, fires.
        let got = run(at(Some(RUN_AT)), pull.clone()).await.unwrap();
        assert_eq!(got.signal(), Signal::Closed);
        // Created by the close itself (a `closed`-typed trigger): never wasted.
        assert_eq!(
            run(at(Some("2026-11-01T11:00:01Z")), pull.clone()).await,
            None
        );
        assert_eq!(run(at(Some(CLOSED_AT)), pull.clone()).await, None);
        // A missing timestamp on either side is no answer.
        assert_eq!(run(at(None), pull.clone()).await, None);
        let no_close = PullFacts {
            closed_at: None,
            ..pull
        };
        assert_eq!(run(at(Some(RUN_AT)), no_close).await, None);
    }

    #[tokio::test]
    async fn no_answer_is_not_fired() {
        let policy = WastePolicy {
            closed: WasteMode::Skip,
            superseded: WasteMode::Skip,
            conflicted: WasteMode::Skip,
            run_failed: WasteMode::Demote,
        };
        // `mergeable: null`: GitHub has not computed it yet.
        let unknown = PullFacts {
            mergeable: None,
            ..open()
        };
        assert_eq!(
            check(&Checker::default(), &policy, 0, Some(unknown), Some(vec![])).await,
            None
        );
        // A failed read (a 404, a missing permission) answers `None`.
        assert_eq!(
            check(&Checker::default(), &policy, 0, None, None).await,
            None
        );
        // An empty head is no answer either, never a move.
        let headless = PullFacts {
            head_sha: String::new(),
            ..open()
        };
        assert_eq!(
            check(
                &Checker::default(),
                &policy,
                0,
                Some(headless),
                Some(vec![])
            )
            .await,
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_is_not_fired() {
        let policy = policy_for(Signal::Conflicted, WasteMode::Skip);
        let (pull, _) = firing(Signal::Conflicted);
        let got = Checker::default()
            .check(
                &policy,
                subject(),
                0,
                || async move {
                    tokio::time::sleep(READ_TIMEOUT * 2).await;
                    Some(pull)
                },
                || async { None },
            )
            .await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn the_failure_of_this_job_is_not_another_jobs() {
        let policy = policy_for(Signal::RunFailed, WasteMode::Demote);
        // Job 1 is the subject: a failure of its own earlier attempt is no signal.
        assert_eq!(
            check(&Checker::default(), &policy, 0, None, Some(vec![1])).await,
            None
        );
    }

    #[tokio::test]
    async fn answers_are_cached_for_a_minute() {
        let policy = WastePolicy {
            conflicted: WasteMode::Demote,
            run_failed: WasteMode::Record,
            ..WastePolicy::default()
        };
        let checker = Checker::default();
        let pulls = AtomicUsize::new(0);
        let runs = AtomicUsize::new(0);
        let read = |now: u64, conflicted: bool| {
            let checker = &checker;
            let pulls = &pulls;
            let runs = &runs;
            async move {
                checker
                    .check(
                        &policy,
                        subject(),
                        now,
                        || async move {
                            pulls.fetch_add(1, Ordering::SeqCst);
                            Some(PullFacts {
                                mergeable: Some(!conflicted),
                                ..open()
                            })
                        },
                        || async move {
                            runs.fetch_add(1, Ordering::SeqCst);
                            Some(vec![])
                        },
                    )
                    .await
            }
        };
        assert_eq!(read(0, false).await, None);
        // Within the minute the cached answer stands, whatever GitHub would say now.
        assert_eq!(read(CACHE_MS - 1, true).await, None);
        assert_eq!(pulls.load(Ordering::SeqCst), 1);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // A minute on, both are read again.
        let got = read(CACHE_MS, true).await.unwrap();
        assert_eq!(got.signal(), Signal::Conflicted);
        assert_eq!(pulls.load(Ordering::SeqCst), 2);
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_read_is_cached_too() {
        let policy = policy_for(Signal::Closed, WasteMode::Record);
        let checker = Checker::default();
        let reads = AtomicUsize::new(0);
        for now in [0, 30_000] {
            let got = checker
                .check(
                    &policy,
                    subject(),
                    now,
                    || async {
                        reads.fetch_add(1, Ordering::SeqCst);
                        None
                    },
                    || async { None },
                )
                .await;
            assert_eq!(got, None);
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_off_signal_costs_no_read() {
        let policy = policy_for(Signal::RunFailed, WasteMode::Record);
        let pulls = AtomicUsize::new(0);
        Checker::default()
            .check(
                &policy,
                subject(),
                0,
                || async {
                    pulls.fetch_add(1, Ordering::SeqCst);
                    None
                },
                || async { Some(vec![]) },
            )
            .await;
        assert_eq!(pulls.load(Ordering::SeqCst), 0, "no PR signal is on");
    }
}
