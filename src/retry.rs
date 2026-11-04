//! Auto-retry of faber-caused job failures (ADR-0003).
//!
//! When faber's own runner is killed under a running job — by the memory cap, at
//! its lifetime cap, by a reclaim, frozen for good, or lost when the daemon or VM
//! under it went away — the GitHub job fails through no fault of the code.
//!
//! **The trigger is faber's own evidence** (ADR-0003, amendment 2026-10-18). It
//! used to be GitHub's check-run annotation ("The self-hosted runner lost
//! communication with the server"), which needs Checks: read — a permission the
//! fine-grained token (ADR-0027) cannot hold. Now a failed job is faber's fault
//! only when faber's audit record shows that the `faber-*` runner GitHub names on
//! the job ended abnormally *while that job was running* ([`classify`]). The
//! sweep needs only the jobs API (Actions: read) and the re-run call (Actions:
//! write).
//!
//! Ambiguity is never retried: a clean exit, an ending long after GitHub marked
//! the job failed, and a runner faber has no record of (a peer host's, or one
//! from before a record existed) are all "not faber's fault".
//!
//! This module is the **pure core**: the classifier, the per-run re-run mode, an
//! RFC3339 timestamp parser for the lookback bound, a bounded dedup memory, and
//! the bounded [`EndLedger`] that indexes the audit records by runner. The I/O
//! sweep that drives them (`reconcile_retries`) lives in [`crate::reconcile`],
//! alongside the orchestrator state it records into — mirroring how `drift`
//! keeps its check pure and `reconcile` owns the recording.

use std::collections::{HashSet, VecDeque};

/// How to re-run a completed-failed run once its faber-caused deaths are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryMode {
    /// *Every* failed job in the run is a faber-death — recover the whole run with
    /// one `rerun-failed-jobs` call (the common redeploy case, where a run's
    /// runners all die at once).
    AllFailed,
    /// A *mix* of faber-deaths and genuine failures — re-run only these job ids
    /// individually, so a genuinely-failing job is never re-run.
    Surgical(Vec<u64>),
}

/// Decide how to re-run a completed-failed run, given the ids of *all* its failed
/// jobs and the subset that are faber-caused deaths. `None` when nothing is
/// faber-caused (leave the run alone). Pure — the testable core of the sweep's
/// per-run choice.
pub fn retry_mode(failed_job_ids: &[u64], faber_dead_ids: &[u64]) -> Option<RetryMode> {
    if faber_dead_ids.is_empty() {
        None
    } else if faber_dead_ids.len() == failed_job_ids.len() {
        Some(RetryMode::AllFailed)
    } else {
        Some(RetryMode::Surgical(faber_dead_ids.to_vec()))
    }
}

/// Whether a job ran on one of faber's own JIT runners (name prefixed `faber-`).
/// Required by the sweep so it never re-runs a job some *other* runner executed.
pub fn is_faber_runner(runner_name: Option<&str>) -> bool {
    runner_name.is_some_and(|n| n.starts_with("faber-"))
}

/// Whether a job's terminal conclusion is one the sweep considers — a `failure` or
/// a `cancelled` (a runner-death surfaces as either). The ledger still decides
/// faber-vs-genuine ([`classify`]); this only narrows the candidate set.
pub fn is_failed_conclusion(conclusion: Option<&str>) -> bool {
    matches!(conclusion, Some("failure") | Some("cancelled"))
}

/// How long after GitHub marked a job complete a runner may still end and be
/// blamed for it (ADR-0003, amendment 2026-10-18). Teardown takes seconds, so a
/// runner faber killed under the job is recorded within this; one that ends
/// minutes later ended for some other reason.
pub const END_GRACE_MS: u64 = 60_000;

/// How a runner's life ended, as faber's audit log records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndKind {
    /// faber's memory cap OOM-killed the runner or its dind container.
    Oom,
    /// faber killed it at its lifetime cap.
    LifetimeCap,
    /// faber reclaimed it.
    Reclaimed,
    /// The container vanished without an exit status — the Docker daemon or VM
    /// under it restarted, or the host slept — and faber tore down what was left.
    Lost,
    /// faber froze it (`docker pause`, ADR-0032) and had not thawed it yet.
    /// `thawed_ms` is when the thaw was recorded, once it was.
    Frozen { thawed_ms: Option<u64> },
    /// It exited on its own with status 0 and no OOM: nothing of faber's doing.
    Clean,
    /// Anything else — a runner that exited non-zero on its own, an infra failure
    /// before a job could run. Not faber's act, so never a reason to retry.
    Other,
}

impl EndKind {
    /// The stable word the audit log's `end` field carries.
    pub fn word(self) -> &'static str {
        match self {
            EndKind::Oom => "oom",
            EndKind::LifetimeCap => "lifetime_cap",
            EndKind::Reclaimed => "reclaimed",
            EndKind::Lost => "lost",
            EndKind::Frozen { .. } => "frozen",
            EndKind::Clean => "exited",
            EndKind::Other => "failed",
        }
    }

    /// Read an `end` word back. `frozen` is never an outcome's word (a freeze has
    /// its own record), and an unknown word is [`EndKind::Other`]: the safe side.
    pub fn from_word(word: &str) -> Self {
        match word {
            "oom" => EndKind::Oom,
            "lifetime_cap" => EndKind::LifetimeCap,
            "reclaimed" => EndKind::Reclaimed,
            "lost" => EndKind::Lost,
            "exited" => EndKind::Clean,
            _ => EndKind::Other,
        }
    }

    /// Whether this ending is one faber caused.
    fn is_faber_act(self) -> bool {
        !matches!(self, EndKind::Clean | EndKind::Other)
    }
}

/// One abnormal-or-not ending faber recorded for a runner it minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    /// The minted runner name (`faber-{job_id}-{ts}`) — the join key to GitHub's
    /// `job.runner_name`. Unique per mint, since the timestamp is the mint instant.
    pub runner: String,
    pub kind: EndKind,
    /// When faber recorded it, epoch ms (for a freeze, when it froze).
    pub at_ms: u64,
    /// The job GitHub told faber this runner was executing, when it managed to
    /// find out. `None` means unattributed — the runner name is still a single-run
    /// join, so it stands rather than being discarded on an absent field.
    pub executed_job_id: Option<u64>,
}

/// Why a failed job is *not* faber's fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotFaber {
    /// Not a `faber-*` runner, or none named.
    NotOurRunner,
    /// faber has no record of the runner's ending.
    NoRecord,
    /// The record says the runner ran a different job than this one.
    OtherJob,
    /// GitHub did not give both of the job's timestamps, or they are nonsense.
    UnknownWindow,
    /// The runner exited on its own, or failed in some way that was not faber's.
    NotFaberAct,
    /// The runner ended before the job started.
    BeforeJob,
    /// The runner ended more than [`END_GRACE_MS`] after GitHub marked the job complete.
    TooLate,
}

/// The verdict on one failed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryVerdict {
    /// faber's own record shows it ended the job's runner mid-job.
    FaberCaused {
        kind: EndKind,
        at_ms: u64,
    },
    NotFaber(NotFaber),
}

/// Decide whether a failed job was faber's fault, from faber's own record of how
/// the job's runner ended. Pure.
///
/// A job is faber's fault only when **all** of these hold, and anything unknown
/// reads as "no" — the safe, no-retry direction:
///
/// - the runner GitHub names on the job is a `faber-*` one;
/// - faber recorded an ending for it that faber caused ([`EndKind::Oom`],
///   [`EndKind::LifetimeCap`], [`EndKind::Reclaimed`], [`EndKind::Lost`], or a
///   [`EndKind::Frozen`] still in force when GitHub completed the job), and where
///   faber knows which job the runner executed, it is this one;
/// - the ending falls inside the job: not before `started_at`, and no later than
///   [`END_GRACE_MS`] after `completed_at`. An ending *before* `completed_at` is
///   the common case (a runner killed at 10:00 is failed by GitHub's runner-lock
///   expiry at 10:10).
///
/// `endings` is everything the ledger holds for the runner; any one that
/// qualifies is enough.
pub fn classify(
    runner_name: Option<&str>,
    job_id: u64,
    started_at: Option<&str>,
    completed_at: Option<&str>,
    endings: &[Ending],
) -> RetryVerdict {
    if !is_faber_runner(runner_name) {
        return RetryVerdict::NotFaber(NotFaber::NotOurRunner);
    }
    if endings.is_empty() {
        return RetryVerdict::NotFaber(NotFaber::NoRecord);
    }
    let window = started_at
        .and_then(parse_rfc3339_to_ms)
        .zip(completed_at.and_then(parse_rfc3339_to_ms))
        .filter(|(start, end)| start <= end);
    let Some((started, completed)) = window else {
        return RetryVerdict::NotFaber(NotFaber::UnknownWindow);
    };
    let mut why = None;
    for e in endings {
        match judge(e, job_id, started, completed) {
            Ok(caused) => return caused,
            Err(reason) => {
                why.get_or_insert(reason);
            }
        }
    }
    RetryVerdict::NotFaber(why.unwrap_or(NotFaber::NoRecord))
}

/// One ending against one job's `[started, completed]` window.
fn judge(e: &Ending, job_id: u64, started: u64, completed: u64) -> Result<RetryVerdict, NotFaber> {
    if e.executed_job_id.is_some_and(|ran| ran != job_id) {
        return Err(NotFaber::OtherJob);
    }
    if !e.kind.is_faber_act() {
        return Err(NotFaber::NotFaberAct);
    }
    if e.at_ms < started {
        return Err(NotFaber::BeforeJob);
    }
    if e.at_ms > completed.saturating_add(END_GRACE_MS) {
        return Err(NotFaber::TooLate);
    }
    // A freeze counts only if it was still in force when GitHub completed the job;
    // a job that failed after being thawed failed on its own.
    if let EndKind::Frozen { thawed_ms: Some(t) } = e.kind {
        if t <= completed {
            return Err(NotFaber::NotFaberAct);
        }
    }
    Ok(RetryVerdict::FaberCaused {
        kind: e.kind,
        at_ms: e.at_ms,
    })
}

/// Whether `updated_ms` falls within `lookback_ms` of `now`. The sweep retries
/// only *recent* failures, so a freshly-started daemon doesn't re-run a pile of
/// historical faber-deaths returned by the failed-runs scan.
pub fn within_lookback(updated_ms: u64, now: u64, lookback_ms: u64) -> bool {
    now.saturating_sub(updated_ms) <= lookback_ms
}

/// Whether a run at `run_attempt` may still be auto-retried under `max_attempts`.
/// The cap is the highest attempt faber will *create*, so a run is retryable while
/// strictly below it (attempt 1 with cap 2 → retry once → attempt 2 → stop).
pub fn attempt_ok(run_attempt: u64, max_attempts: u64) -> bool {
    run_attempt < max_attempts
}

/// Parse an RFC3339 UTC timestamp (`2025-01-01T12:00:00Z`, the shape GitHub emits
/// for a run's `updated_at`) to epoch milliseconds, with no date-crate dependency.
///
/// Tolerant by design: a fractional-seconds part and a trailing `Z`/offset are
/// ignored (the value is treated as UTC, which is what GitHub sends). Any shape it
/// doesn't recognize returns `None`, so an unparsable stamp reads as "outside the
/// lookback" — the safe, no-retry direction — at the call site.
pub fn parse_rfc3339_to_ms(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Keep only the leading HH:MM:SS, dropping a trailing `Z`, fractional seconds,
    // or a numeric offset (all treated as UTC).
    let hms_end = time
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(time.len());
    let mut t = time[..hms_end].split(':');
    let hh: u64 = t.next()?.parse().ok()?;
    let mm: u64 = t.next()?.parse().ok()?;
    let ss: u64 = t.next().unwrap_or("0").parse().ok()?;
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days since 1970-01-01 via Howard Hinnant's `days_from_civil`.
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146097 + doe - 719468;
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86400 + hh * 3600 + mm * 60 + ss) * 1000)
}

/// Upper bound on remembered `(run_id, run_attempt)` keys. The working set is only
/// the runs failing within the lookback window, so this comfortably exceeds it and
/// FIFO eviction never drops a still-relevant key.
const RETRY_MEMORY_CAP: usize = 512;

/// Bounded memory of the `(run_id, run_attempt)` pairs the sweep has finished with
/// — re-run, or examined and found terminally genuine — so neither is redone every
/// cycle while the run sits in the lookback window. FIFO-evicted at
/// [`RETRY_MEMORY_CAP`].
///
/// Deliberately in-memory only: the real cap on runaway retries is the run's
/// `run_attempt` (see [`attempt_ok`]), which is GitHub-side and survives a restart.
/// This set only suppresses same-attempt rework within one daemon lifetime.
#[derive(Default)]
pub struct RetryMemory {
    seen: HashSet<(u64, u64)>,
    order: VecDeque<(u64, u64)>,
}

impl RetryMemory {
    /// Whether `key` has already been actioned (and not yet evicted).
    pub fn contains(&self, key: &(u64, u64)) -> bool {
        self.seen.contains(key)
    }

    /// Remember `key`, evicting the oldest entry if at capacity. Idempotent — a
    /// repeat insert neither duplicates the key nor churns the eviction order.
    pub fn record(&mut self, key: (u64, u64)) {
        if self.seen.insert(key) {
            self.order.push_back(key);
            if self.order.len() > RETRY_MEMORY_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }
}

/// What an `outcome` audit line says about how its runner ended — the one place
/// the live write path and the startup replay agree on how to read it.
pub struct OutcomeFacts<'a> {
    pub runner: &'a str,
    /// Either container was OOM-killed.
    pub oom: bool,
    /// The outcome's `end` word; `None` on a line older than the field.
    pub end: Option<&'a str>,
    /// Whether faber learned which job the run executed (the learner `class` is
    /// set): see below.
    pub attributed: bool,
    pub job_id: u64,
    pub executed_job_id: Option<u64>,
    pub at_ms: u64,
}

/// The ending an `outcome` line records, or `None` when it records none.
///
/// An OOM is kept only when attributed. The retry it arms is sold on one promise:
/// the class cap is force-bumped before the re-run, so the re-run is a real
/// second chance and not a repeat of the same kill. That bump fires only when
/// faber learned which job the runner executed; an unattributed OOM would only
/// spend the run's one retry on the same cap. A line from before the `end` field
/// has no word, and counts only through its OOM flag.
pub fn outcome_ending(f: &OutcomeFacts<'_>) -> Option<Ending> {
    let kind = if f.oom {
        if !f.attributed {
            return None;
        }
        EndKind::Oom
    } else {
        EndKind::from_word(f.end?)
    };
    Some(Ending {
        runner: f.runner.to_string(),
        kind,
        at_ms: f.at_ms,
        // Not `executed_job_id` alone: that is recorded only when the executed job
        // *differs* from the minted-for one, so a runner that ran the job it was
        // named for leaves it `None` — which would read as "faber never found
        // out". `attributed` is what separates "ran the minted-for job" from
        // "genuinely unattributable".
        executed_job_id: f.executed_job_id.or(f.attributed.then_some(f.job_id)),
    })
}

/// Rebuild the ledger from the audit log, so a restart — the commonest way a
/// runner is lost — does not forget the evidence. `entries` are in append order;
/// only those at or after `since_ms` are read.
pub fn ledger_from_audit(entries: &[crate::audit::Entry], since_ms: u64) -> EndLedger {
    let mut ledger = EndLedger::default();
    for e in entries.iter().filter(|e| e.ts_ms >= since_ms) {
        let Some(runner) = e.runner.as_deref().filter(|r| is_faber_runner(Some(r))) else {
            continue;
        };
        match e.kind.as_str() {
            "outcome" => {
                if let Some(ending) = outcome_ending(&OutcomeFacts {
                    runner,
                    oom: e.oom_runner == Some(true) || e.oom_dind == Some(true),
                    end: e.end.as_deref(),
                    attributed: e.class.is_some(),
                    job_id: e.job_id,
                    executed_job_id: e.executed_job_id,
                    at_ms: e.ts_ms,
                }) {
                    ledger.record(ending);
                }
            }
            "freeze" => match e.action.as_deref() {
                Some("frozen") => ledger.record(Ending {
                    runner: runner.to_string(),
                    kind: EndKind::Frozen { thawed_ms: None },
                    at_ms: e.ts_ms,
                    executed_job_id: None,
                }),
                Some("thawed") => ledger.thawed(runner, e.ts_ms),
                _ => {}
            },
            _ => {}
        }
    }
    ledger
}

/// Upper bound on remembered endings. Sized well above the number of runs a host
/// can finish inside one retry lookback window (default 30 min), so the ending
/// that explains a just-failed job is never evicted before the sweep looks for it.
/// Deliberately independent of `OUTCOME_RECENT_CAP`, which is a *display* budget
/// for the dashboard and is free to shrink without silently disarming retries.
const END_LEDGER_CAP: usize = 1024;

/// Bounded index of faber's own audit records of how its runners ended, keyed for
/// the retry sweep by runner name. FIFO-evicted at [`END_LEDGER_CAP`].
///
/// It is written as faber writes the audit line (`record_outcome`, a freeze or
/// thaw) and **seeded from the audit log at startup**, so a daemon restart — the
/// commonest way a runner is lost — does not forget the evidence.
///
/// It only ever holds runners *this* host minted. faber hosts share a runner pool
/// (ADR-0004), so a `faber-*` runner may belong to a peer daemon whose audit log
/// this host does not have. A miss therefore means "no record", which is "not
/// faber's fault" and is never retried; the peer reaches its own verdict for its
/// own runners.
#[derive(Default)]
pub struct EndLedger {
    ends: VecDeque<Ending>,
}

impl EndLedger {
    /// Remember one ending, evicting the oldest at capacity.
    pub fn record(&mut self, ending: Ending) {
        self.ends.push_back(ending);
        while self.ends.len() > END_LEDGER_CAP {
            self.ends.pop_front();
        }
    }

    /// Note that `runner` was thawed at `at_ms`: closes its open freeze, so a job
    /// that failed after the thaw is not blamed on it.
    pub fn thawed(&mut self, runner: &str, at_ms: u64) {
        for e in self.ends.iter_mut().rev() {
            if e.runner == runner {
                if let EndKind::Frozen { thawed_ms } = &mut e.kind {
                    thawed_ms.get_or_insert(at_ms);
                    return;
                }
            }
        }
    }

    /// Every ending recorded for `runner`, oldest first.
    pub fn for_runner(&self, runner: &str) -> Vec<Ending> {
        self.ends
            .iter()
            .filter(|e| e.runner == runner)
            .cloned()
            .collect()
    }

    /// [`classify`] a failed job against what this ledger holds for its runner.
    pub fn verdict(
        &self,
        runner_name: Option<&str>,
        job_id: u64,
        started_at: Option<&str>,
        completed_at: Option<&str>,
    ) -> RetryVerdict {
        let endings = runner_name
            .filter(|n| is_faber_runner(Some(n)))
            .map(|n| self.for_runner(n))
            .unwrap_or_default();
        classify(runner_name, job_id, started_at, completed_at, &endings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_mode_none_when_no_faber_deaths() {
        assert_eq!(retry_mode(&[1, 2, 3], &[]), None);
    }

    #[test]
    fn retry_mode_all_failed_when_every_failure_is_faber() {
        // Order-insensitive: the same id set, differently ordered, is still "all".
        assert_eq!(
            retry_mode(&[1, 2, 3], &[3, 1, 2]),
            Some(RetryMode::AllFailed)
        );
    }

    #[test]
    fn retry_mode_surgical_preserves_genuine_failures() {
        // 2 failed, only 1 faber-caused → re-run just that one, never the genuine.
        assert_eq!(
            retry_mode(&[10, 20], &[20]),
            Some(RetryMode::Surgical(vec![20]))
        );
    }

    #[test]
    fn faber_runner_prefix_gate() {
        assert!(is_faber_runner(Some("faber-1001-1756000000000")));
        assert!(!is_faber_runner(Some("gh-hosted-runner-7")));
        assert!(!is_faber_runner(None));
    }

    #[test]
    fn only_failure_and_cancelled_are_candidates() {
        assert!(is_failed_conclusion(Some("failure")));
        assert!(is_failed_conclusion(Some("cancelled")));
        assert!(!is_failed_conclusion(Some("success")));
        assert!(!is_failed_conclusion(Some("skipped")));
        assert!(!is_failed_conclusion(None));
    }

    // --- the classifier -----------------------------------------------------
    //
    // ADR-0003, amendment 2026-10-18. The job window below is
    // 10:00:00Z -> 10:10:00Z, the shape of a runner-lock expiry: the runner dies
    // early and GitHub fails the job ten minutes later.
    const RUNNER: &str = "faber-1001-1756000000000";
    const JOB: u64 = 1_001;
    const STARTED: &str = "2026-10-05T10:00:00Z";
    const COMPLETED: &str = "2026-10-05T10:10:00Z";

    fn ms(stamp: &str) -> u64 {
        parse_rfc3339_to_ms(stamp).expect("a stamp in the tests parses")
    }

    fn ending(kind: EndKind, at_ms: u64) -> Ending {
        Ending {
            runner: RUNNER.to_string(),
            kind,
            at_ms,
            executed_job_id: Some(JOB),
        }
    }

    fn verdict(endings: &[Ending]) -> RetryVerdict {
        classify(Some(RUNNER), JOB, Some(STARTED), Some(COMPLETED), endings)
    }

    fn caused(kind: EndKind, at_ms: u64) -> RetryVerdict {
        RetryVerdict::FaberCaused { kind, at_ms }
    }

    #[test]
    fn each_faber_made_ending_during_the_job_is_faber_caused() {
        let during = ms(STARTED) + 30_000;
        for kind in [
            EndKind::Oom,
            EndKind::LifetimeCap,
            EndKind::Reclaimed,
            EndKind::Lost,
        ] {
            assert_eq!(verdict(&[ending(kind, during)]), caused(kind, during));
        }
    }

    #[test]
    fn a_clean_exit_or_a_failure_of_its_own_is_never_faber_caused() {
        let during = ms(STARTED) + 30_000;
        for kind in [EndKind::Clean, EndKind::Other] {
            assert_eq!(
                verdict(&[ending(kind, during)]),
                RetryVerdict::NotFaber(NotFaber::NotFaberAct),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn the_timing_rule_before_during_and_the_sixty_second_edge() {
        let completed = ms(COMPLETED);
        let kind = EndKind::Oom;
        // Before the job started: not the job's story.
        assert_eq!(
            verdict(&[ending(kind, ms(STARTED) - 1)]),
            RetryVerdict::NotFaber(NotFaber::BeforeJob)
        );
        // At the job's start, mid-job and at completion: during.
        for at in [ms(STARTED), ms(STARTED) + 1, completed - 1, completed] {
            assert_eq!(verdict(&[ending(kind, at)]), caused(kind, at), "{at}");
        }
        // The grace after completion: +59 s and +60 s count, +61 s does not.
        assert_eq!(
            verdict(&[ending(kind, completed + 59_000)]),
            caused(kind, completed + 59_000)
        );
        assert_eq!(
            verdict(&[ending(kind, completed + 60_000)]),
            caused(kind, completed + 60_000),
            "\"more than 60 s\" excludes exactly 60 s"
        );
        assert_eq!(
            verdict(&[ending(kind, completed + 61_000)]),
            RetryVerdict::NotFaber(NotFaber::TooLate)
        );
    }

    #[test]
    fn a_missing_record_is_not_faber_caused() {
        assert_eq!(verdict(&[]), RetryVerdict::NotFaber(NotFaber::NoRecord));
    }

    #[test]
    fn a_runner_that_is_not_ours_is_never_faber_caused() {
        let during = ms(STARTED) + 30_000;
        for name in [Some("gh-hosted-runner-7"), None] {
            assert_eq!(
                classify(
                    name,
                    JOB,
                    Some(STARTED),
                    Some(COMPLETED),
                    &[ending(EndKind::Oom, during)]
                ),
                RetryVerdict::NotFaber(NotFaber::NotOurRunner)
            );
        }
    }

    #[test]
    fn an_unknown_or_nonsense_job_window_is_not_faber_caused() {
        let during = [ending(EndKind::Oom, ms(STARTED) + 30_000)];
        for (s, c) in [
            (None, Some(COMPLETED)),
            (Some(STARTED), None),
            (Some("nonsense"), Some(COMPLETED)),
            (Some(COMPLETED), Some(STARTED)), // completed before it started
        ] {
            assert_eq!(
                classify(Some(RUNNER), JOB, s, c, &during),
                RetryVerdict::NotFaber(NotFaber::UnknownWindow),
                "{s:?} {c:?}"
            );
        }
    }

    #[test]
    fn an_ending_is_not_borrowed_by_a_job_the_runner_did_not_run() {
        // A JIT runner is not job-pinned: when faber knows which job it ran, an
        // ending on it is that job's story and nobody else's.
        let during = ms(STARTED) + 30_000;
        let e = ending(EndKind::Oom, during);
        assert_eq!(
            classify(Some(RUNNER), 12_345, Some(STARTED), Some(COMPLETED), &[e]),
            RetryVerdict::NotFaber(NotFaber::OtherJob)
        );
        // Unattributed: the runner name is still a single-run join.
        let unattributed = Ending {
            executed_job_id: None,
            ..ending(EndKind::Oom, during)
        };
        assert_eq!(verdict(&[unattributed]), caused(EndKind::Oom, during));
    }

    #[test]
    fn a_freeze_counts_only_while_it_was_in_force_when_the_job_completed() {
        let frozen_at = ms(STARTED) + 60_000;
        let open = EndKind::Frozen { thawed_ms: None };
        assert_eq!(verdict(&[ending(open, frozen_at)]), caused(open, frozen_at));
        // Thawed before GitHub completed the job: it failed on its own.
        let thawed_early = EndKind::Frozen {
            thawed_ms: Some(frozen_at + 5_000),
        };
        assert_eq!(
            verdict(&[ending(thawed_early, frozen_at)]),
            RetryVerdict::NotFaber(NotFaber::NotFaberAct)
        );
        // Thawed only after GitHub gave up on the job (#111): still faber's.
        let thawed_late = EndKind::Frozen {
            thawed_ms: Some(ms(COMPLETED) + 1_000),
        };
        assert_eq!(
            verdict(&[ending(thawed_late, frozen_at)]),
            caused(thawed_late, frozen_at)
        );
    }

    #[test]
    fn any_qualifying_ending_is_enough_and_a_clean_one_does_not_cancel_it() {
        // A frozen runner that was thawed late and then exited 0.
        let frozen_at = ms(STARTED) + 60_000;
        let open = EndKind::Frozen { thawed_ms: None };
        let clean = ending(EndKind::Clean, ms(COMPLETED) + 30_000);
        assert_eq!(
            verdict(&[ending(open, frozen_at), clean.clone()]),
            caused(open, frozen_at)
        );
        // But a clean exit on its own stays clean.
        assert_eq!(
            verdict(&[clean]),
            RetryVerdict::NotFaber(NotFaber::NotFaberAct)
        );
    }

    #[test]
    fn end_words_round_trip_and_unknown_words_are_the_safe_side() {
        for kind in [
            EndKind::Oom,
            EndKind::LifetimeCap,
            EndKind::Reclaimed,
            EndKind::Lost,
            EndKind::Clean,
            EndKind::Other,
        ] {
            assert_eq!(EndKind::from_word(kind.word()), kind);
        }
        assert_eq!(EndKind::from_word("frozen"), EndKind::Other);
        assert_eq!(EndKind::from_word("something-new"), EndKind::Other);
    }

    #[test]
    fn the_ledger_answers_by_runner_and_closes_a_freeze_on_thaw() {
        let mut l = EndLedger::default();
        let frozen_at = ms(STARTED) + 60_000;
        l.record(ending(EndKind::Frozen { thawed_ms: None }, frozen_at));
        l.record(Ending {
            runner: "faber-1-1".to_string(),
            ..ending(EndKind::Oom, frozen_at)
        });
        let v = |l: &EndLedger| l.verdict(Some(RUNNER), JOB, Some(STARTED), Some(COMPLETED));
        assert!(matches!(v(&l), RetryVerdict::FaberCaused { .. }));
        l.thawed(RUNNER, frozen_at + 1_000);
        assert_eq!(v(&l), RetryVerdict::NotFaber(NotFaber::NotFaberAct));
        // Another runner's ending never excuses this one's job.
        assert_eq!(
            l.verdict(Some("faber-2-2"), JOB, Some(STARTED), Some(COMPLETED)),
            RetryVerdict::NotFaber(NotFaber::NoRecord)
        );
        // And a ledger never vouches for a runner that is not ours.
        assert_eq!(
            l.verdict(None, JOB, Some(STARTED), Some(COMPLETED)),
            RetryVerdict::NotFaber(NotFaber::NotOurRunner)
        );
    }

    #[test]
    fn the_ledger_is_rebuilt_from_the_audit_log() {
        use crate::audit::Entry;
        let at = ms(STARTED) + 30_000;
        let line = |kind: &str, action: Option<&str>| Entry {
            ts_ms: at,
            kind: kind.to_string(),
            job_id: JOB,
            runner: Some(RUNNER.to_string()),
            action: action.map(str::to_string),
            ..Default::default()
        };
        // An OOM line from before the `end` field, attributed.
        let oom = Entry {
            oom_runner: Some(true),
            class: Some("o/r::1::build".to_string()),
            ..line("outcome", None)
        };
        let l = ledger_from_audit(std::slice::from_ref(&oom), 0);
        assert_eq!(
            l.verdict(Some(RUNNER), JOB, Some(STARTED), Some(COMPLETED)),
            caused(EndKind::Oom, at)
        );
        // Unattributed, the OOM arms nothing.
        let unattributed = Entry { class: None, ..oom };
        assert!(ledger_from_audit(&[unattributed], 0)
            .for_runner(RUNNER)
            .is_empty());
        // A line with no `end` and no OOM is no record at all.
        assert!(ledger_from_audit(&[line("outcome", None)], 0)
            .for_runner(RUNNER)
            .is_empty());
        // A word, and since_ms.
        let lost = Entry {
            end: Some("lost".to_string()),
            ..line("outcome", None)
        };
        assert_eq!(
            ledger_from_audit(std::slice::from_ref(&lost), 0).for_runner(RUNNER)[0].kind,
            EndKind::Lost
        );
        assert!(ledger_from_audit(&[lost], at + 1)
            .for_runner(RUNNER)
            .is_empty());
        // A freeze, then its thaw.
        let thaw = Entry {
            ts_ms: at + 1_000,
            ..line("freeze", Some("thawed"))
        };
        let l = ledger_from_audit(&[line("freeze", Some("frozen")), thaw], 0);
        assert_eq!(
            l.for_runner(RUNNER)[0].kind,
            EndKind::Frozen {
                thawed_ms: Some(at + 1_000)
            }
        );
    }

    #[test]
    fn the_end_ledger_stays_bounded() {
        let mut l = EndLedger::default();
        for i in 0..(END_LEDGER_CAP as u64 + 10) {
            l.record(Ending {
                runner: format!("faber-{i}-1000"),
                kind: EndKind::Oom,
                at_ms: 0,
                executed_job_id: None,
            });
        }
        assert!(l.ends.len() <= END_LEDGER_CAP);
        assert!(l.for_runner("faber-0-1000").is_empty(), "oldest evicted");
        assert_eq!(
            l.for_runner(&format!("faber-{}-1000", END_LEDGER_CAP as u64 + 9))
                .len(),
            1,
            "newest retained"
        );
    }

    #[test]
    fn lookback_and_attempt_bounds() {
        let now = 1_000_000;
        assert!(within_lookback(now - 500, now, 1000));
        assert!(within_lookback(now - 1000, now, 1000)); // inclusive edge
        assert!(!within_lookback(now - 1001, now, 1000));
        // A future stamp (clock skew) saturates to 0 elapsed → within window.
        assert!(within_lookback(now + 5000, now, 1000));

        assert!(attempt_ok(1, 2)); // first attempt, cap 2 → retry
        assert!(!attempt_ok(2, 2)); // already re-run once → stop
        assert!(!attempt_ok(3, 2));
    }

    #[test]
    fn rfc3339_parses_known_anchors() {
        // Unix epoch and Y2K — both well-known epoch-second values.
        assert_eq!(parse_rfc3339_to_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_to_ms("2000-01-01T00:00:00Z"),
            Some(946_684_800_000)
        );
    }

    #[test]
    fn rfc3339_is_monotonic_and_handles_offsets_and_fractions() {
        let a = parse_rfc3339_to_ms("2025-01-01T12:00:00Z").unwrap();
        let b = parse_rfc3339_to_ms("2025-01-01T12:00:01Z").unwrap();
        assert_eq!(b - a, 1000, "one second apart");
        // A fractional-seconds part is tolerated (truncated to the second).
        assert_eq!(parse_rfc3339_to_ms("2025-01-01T12:00:00.500Z"), Some(a));
    }

    #[test]
    fn rfc3339_rejects_garbage() {
        assert_eq!(parse_rfc3339_to_ms(""), None);
        assert_eq!(parse_rfc3339_to_ms("2025-01-01"), None); // no time part
        assert_eq!(parse_rfc3339_to_ms("not-a-date"), None);
        assert_eq!(parse_rfc3339_to_ms("2026-13-01T00:00:00Z"), None); // bad month
    }

    #[test]
    fn retry_memory_dedups_and_stays_bounded() {
        let mut m = RetryMemory::default();
        m.record((100, 1));
        assert!(m.contains(&(100, 1)));
        assert!(!m.contains(&(100, 2)));
        // Idempotent: re-recording doesn't grow the eviction order.
        m.record((100, 1));

        // Overflow the cap; the oldest key must be the one evicted.
        for run in 0..(RETRY_MEMORY_CAP as u64 + 10) {
            m.record((run, 9));
        }
        assert!(m.seen.len() <= RETRY_MEMORY_CAP);
        assert!(!m.contains(&(0, 9)), "oldest overflow key evicted");
        assert!(
            m.contains(&(RETRY_MEMORY_CAP as u64 + 9, 9)),
            "newest key retained"
        );
    }
}
