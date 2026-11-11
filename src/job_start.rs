//! The gate, run again when a job starts (ADR-0059): the Docker half's pieces
//! that need no I/O.
//!
//! A JIT runner is not pinned to the job it was minted for. GitHub hands it any
//! queued job its labels cover, refused ones included, so the mint-time verdict
//! decides only what faber mints for (ADR-0030). Every runner faber starts now
//! carries a job-started hook ([`HOOK_TEMPLATE`]) that the runner runs once a job
//! is assigned and before any of its steps. The hook decides nothing: it waits
//! for faber's verdict on the job GitHub actually gave the runner, and stops the
//! job from inside unless that verdict is an allow for its own run and attempt.
//!
//! faber's side is a per-runner [`Watch`]: it notices the assignment (the
//! runner's worker in `docker top`), asks for the job to be found by
//! `runner_name` and judged with `gate::trust_verdict` ([`Act::Search`]),
//! delivers the verdict only to a hook it sees waiting ([`Act::Deliver`]), and
//! stops the runner on a refusal ([`Act::Kill`]). A job in progress with no
//! waiting hook is a hook fault ([`Act::HookFault`]). The orchestrator does the
//! I/O each act names; [`Watch::step`] only decides, so every rule here is a
//! plain function of what was seen.

use std::time::Duration;

use serde::Serialize;

use crate::config::JobStartDeadlines;
use crate::gate::{self, BypassClass, OnBypass, RefuseReason, Verdict};

/// The directory the hook volume is mounted at, read-only, in every runner.
pub const HOOK_DIR: &str = "/opt/faber-hook";
/// The hook's file name inside [`HOOK_DIR`]. `.js`, so the runner runs it on
/// its own bundled Node and the hook needs nothing from the image.
pub const HOOK_FILE: &str = "job-started.js";
/// The variable the runner reads its job-started hook from. faber's alone: a
/// profile's `env` naming it is refused ([`crate::config::FABER_OWNED_ENV`]).
pub const HOOK_ENV: &str = "ACTIONS_RUNNER_HOOK_JOB_STARTED";
/// Where, inside the runner container, faber writes the verdict. The hook
/// empties and re-creates it before it waits, so nothing an image or an
/// earlier process left there is read as a verdict.
pub const VERDICT_DIR: &str = "/tmp/faber-verdict";
/// The process title the hook gives itself, which is what `docker top` shows
/// faber of a hook that is waiting. Fifteen characters at most: the kernel's
/// `comm` keeps no more.
pub const HOOK_COMM: &str = "faber-hook";
/// The runner agent's per-job process: present from assignment until the job,
/// post steps included, is done. The hook's parent.
pub const WORKER_COMM: &str = "Runner.Worker";

/// The hook, with its policy line still to fill in ([`HookPlan::script`]).
const HOOK_TEMPLATE: &str = include_str!("job_start/job-started.js");
/// The placeholder [`HookPlan::script`] replaces with the policy.
const POLICY_SLOT: &str = "/*FABER_POLICY*/ null";

/// The hook's full path in the runner, the value of [`HOOK_ENV`].
pub fn hook_path() -> String {
    format!("{HOOK_DIR}/{HOOK_FILE}")
}

/// What the hook does if no verdict reaches it by its deadline, decided once,
/// when the runner is minted (ADR-0059 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeadlineOutcome {
    /// Run the job, and say so: "unverified".
    Allow,
    /// Stop it: "no verdict".
    Refuse,
}

impl DeadlineOutcome {
    /// Allow on a private repository under `record`, since a JIT runner takes
    /// jobs of its own repository only; refuse otherwise (ADR-0059 §1, §5).
    pub fn at_mint(private: bool, on_bypass: OnBypass) -> Self {
        if private && on_bypass == OnBypass::Record {
            DeadlineOutcome::Allow
        } else {
            DeadlineOutcome::Refuse
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DeadlineOutcome::Allow => "allow",
            DeadlineOutcome::Refuse => "refuse",
        }
    }

    /// The inverse of [`Self::as_str`], for a container's label. Anything else
    /// is `None`: a container without a readable outcome is not watched.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(DeadlineOutcome::Allow),
            "refuse" => Some(DeadlineOutcome::Refuse),
            _ => None,
        }
    }
}

/// What a runner's hook is given when the runner is minted: the one gate value
/// shipped into a runner, and its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookPlan {
    pub outcome: DeadlineOutcome,
    pub deadline: Duration,
}

impl Default for HookPlan {
    /// The conservative plan: refuse at the default deadline.
    fn default() -> Self {
        Self {
            outcome: DeadlineOutcome::Refuse,
            deadline: JobStartDeadlines::default().hook,
        }
    }
}

impl HookPlan {
    /// The hook file's bytes for this plan: the template with its policy line
    /// filled in. The policy is JSON faber builds from its own values, so
    /// nothing a job or a profile wrote is in it.
    pub fn script(&self) -> String {
        let policy = serde_json::json!({
            "deadline_ms": u64::try_from(self.deadline.as_millis()).unwrap_or(u64::MAX),
            "outcome": self.outcome.as_str(),
            "dir": VERDICT_DIR,
        });
        HOOK_TEMPLATE.replacen(POLICY_SLOT, &policy.to_string(), 1)
    }
}

/// The reason line a refused job's log shows, one of a fixed set (ADR-0059 §4):
/// anyone who can read the repository can read the job log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Event,
    Fork,
    Actor,
    Author,
    PublicRepository,
    NoVerdict,
    Unverified,
}

impl Reason {
    /// The word the hook prints. The hook accepts exactly these.
    pub fn word(self) -> &'static str {
        match self {
            Reason::Event => "event",
            Reason::Fork => "fork",
            Reason::Actor => "actor",
            Reason::Author => "author",
            Reason::PublicRepository => "public repository",
            Reason::NoVerdict => "no verdict",
            Reason::Unverified => "unverified",
        }
    }

    /// The audit `action` for a refusal with this reason.
    pub fn key(self) -> &'static str {
        match self {
            Reason::Event => "event",
            Reason::Fork => "fork",
            Reason::Actor => "actor",
            Reason::Author => "author",
            Reason::PublicRepository => "public_repository",
            Reason::NoVerdict => "no_verdict",
            Reason::Unverified => "unverified",
        }
    }

    /// Which word a gate refusal is shown as. A dispatch made with a job's own
    /// token is its actor's; a stranger's code, a destroyed merge group and code
    /// faber could not attribute are all about whose code it is.
    pub fn of(r: &RefuseReason) -> Self {
        match r {
            RefuseReason::PublicRepo => Reason::PublicRepository,
            RefuseReason::DisallowedEvent(_) => Reason::Event,
            RefuseReason::Fork => Reason::Fork,
            RefuseReason::UntrustedActor(_) | RefuseReason::JobTokenDispatch => Reason::Actor,
            RefuseReason::ThirdParty(_) | RefuseReason::DestroyedGroup | RefuseReason::Unread => {
                Reason::Author
            }
        }
    }
}

/// What the job-start check decides for one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartDecision {
    Allow,
    Refuse(Reason),
}

/// ADR-0059 §5's table: what a mint-time verdict means at job start under
/// `on_bypass`.
///
/// - Third-party code and a dispatch made with a job's own token are refused in
///   both modes.
/// - A trust refusal (fork, untrusted actor, public repo, an event outside the
///   ceiling) is refused under `enforce`, and allowed under `record`, where
///   detection records it as a bypass as before (ADR-0030).
/// - A policy refusal (an event the operator narrowed away) is allowed in both,
///   and recorded as today.
pub fn start_decision(verdict: &Verdict, on_bypass: OnBypass) -> StartDecision {
    let Verdict::Refuse(reason) = verdict else {
        return StartDecision::Allow;
    };
    match gate::classify(reason) {
        BypassClass::ThirdParty => StartDecision::Refuse(Reason::of(reason)),
        BypassClass::Trust if on_bypass == OnBypass::Enforce => {
            StartDecision::Refuse(Reason::of(reason))
        }
        BypassClass::Trust | BypassClass::Policy => StartDecision::Allow,
    }
}

/// One verdict as faber writes it into a runner: the decision, and the run and
/// attempt it is for. The hook acts on an allow only when both equal its own
/// `GITHUB_RUN_ID` and `GITHUB_RUN_ATTEMPT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    pub decision: StartDecision,
    pub run_id: u64,
    pub run_attempt: u64,
}

impl Delivery {
    /// The `verdict.json` the hook reads.
    pub fn json(&self) -> String {
        let v = match self.decision {
            StartDecision::Allow => serde_json::json!({
                "verdict": "allow",
                "run_id": self.run_id,
                "run_attempt": self.run_attempt,
            }),
            StartDecision::Refuse(r) => serde_json::json!({
                "verdict": "refuse",
                "reason": r.word(),
                "run_id": self.run_id,
                "run_attempt": self.run_attempt,
            }),
        };
        v.to_string()
    }

    /// The archive `docker cp -` extracts into [`VERDICT_DIR`]: `verdict.json`,
    /// then an empty `ready`. Docker extracts entries in order, so the hook,
    /// which reads the verdict only once `ready` exists, never acts on a
    /// verdict half written. That is what makes the delivery atomic.
    pub fn archive(&self) -> Vec<u8> {
        let mut out = Vec::new();
        tar_entry(&mut out, "verdict.json", self.json().as_bytes());
        tar_entry(&mut out, "ready", b"");
        // Two zero blocks end a tar archive.
        out.extend_from_slice(&[0u8; 1024]);
        out
    }
}

/// Append one regular file to a ustar archive: a 512-byte header, then the
/// data padded to a whole block. Enough for the two small files a verdict is,
/// without a dependency.
fn tar_entry(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    let mut h = [0u8; 512];
    let put = |h: &mut [u8; 512], at: usize, len: usize, v: &[u8]| {
        let n = v.len().min(len);
        h[at..at + n].copy_from_slice(&v[..n]);
    };
    let octal = |v: u64, width: usize| format!("{v:0w$o}\0", w = width - 1);
    put(&mut h, 0, 100, name.as_bytes());
    put(&mut h, 100, 8, octal(0o644, 8).as_bytes());
    put(&mut h, 108, 8, octal(0, 8).as_bytes());
    put(&mut h, 116, 8, octal(0, 8).as_bytes());
    put(&mut h, 124, 12, octal(data.len() as u64, 12).as_bytes());
    put(&mut h, 136, 12, octal(0, 12).as_bytes());
    h[156] = b'0';
    put(&mut h, 257, 6, b"ustar\0");
    put(&mut h, 263, 2, b"00");
    // The checksum is computed with its own field read as spaces.
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    put(&mut h, 148, 8, format!("{sum:06o}\0 ").as_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(data);
    let pad = (512 - data.len() % 512) % 512;
    out.extend(std::iter::repeat_n(0u8, pad));
}

/// What a runner container's process table says, from `docker top <c> -eo
/// pid,ppid,comm`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Top {
    /// The runner's worker is running: a job is assigned.
    pub worker: bool,
    /// faber's hook is running as a child of that worker: it is waiting for a
    /// verdict. Its title alone is not enough: the hook's parent must be the
    /// worker, as the runner starts it.
    pub hook_waiting: bool,
}

impl Top {
    /// Read `docker top … -eo pid,ppid,comm` output: a header, then one `<pid>
    /// <ppid> <comm>` row per process. A `comm` may contain spaces, so
    /// everything after the second field is the name.
    pub fn parse(stdout: &str) -> Self {
        let rows: Vec<(&str, &str, &str)> = stdout
            .lines()
            .filter_map(|row| {
                let row = row.trim();
                let (pid, rest) = row.split_once(char::is_whitespace)?;
                let (ppid, comm) = rest.trim_start().split_once(char::is_whitespace)?;
                Some((pid, ppid, comm.trim()))
            })
            .collect();
        let workers: Vec<&str> = rows
            .iter()
            .filter(|(_, _, comm)| *comm == WORKER_COMM)
            .map(|(pid, _, _)| *pid)
            .collect();
        let hook_waiting = rows
            .iter()
            .any(|(_, ppid, comm)| *comm == HOOK_COMM && workers.contains(ppid));
        Top {
            worker: !workers.is_empty(),
            hook_waiting,
        }
    }
}

/// What faber could read of a runner container this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// Running, with this process table.
    Running(Top),
    /// Not running, or not there: nothing in it runs anything.
    Gone,
    /// Docker would not say. Not evidence of anything.
    Unknown,
}

/// The job a runner took, as faber found and judged it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judged {
    pub job_id: u64,
    pub run_id: u64,
    pub run_attempt: u64,
    pub job_name: String,
    pub workflow: String,
    /// The gate's verdict label, such as `refuse:job_token_dispatch`.
    pub verdict: String,
    pub decision: StartDecision,
}

/// What [`Watch::step`] asks the orchestrator to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Find the job whose `runner_name` is this runner, judge it, and hand the
    /// result to [`Watch::judge`].
    Search,
    /// Write this verdict into the runner, whose hook was seen waiting.
    Deliver(Delivery),
    /// Kill the runner's containers without asking it (ADR-0059 §4). Asked
    /// again every pass until the container is gone.
    Kill,
    /// Record the job as refused at start: not a bypass (§6).
    RefusedAtStart(Reason),
    /// Record that the job ran at the deadline with no verdict, as the plan
    /// allowed on a private repository under `record` (§5).
    Unverified,
    /// Record a hook fault: a job in progress with no waiting hook (§2). `stop`
    /// says whether faber also stops it.
    HookFault { stop: bool, why: &'static str },
}

/// How often one runner's job is looked for while it is not yet found: every
/// lookup is a few GitHub calls.
pub const SEARCH_EVERY: Duration = Duration::from_secs(4);

/// One runner faber started, from its mint until its job is decided and done.
#[derive(Debug, Clone)]
pub struct Watch {
    pub repo: String,
    /// The name the runner registered with GitHub (`faber-<job>-<time>-…`),
    /// which is what a job's `runner_name` carries.
    pub runner: String,
    /// The job it was minted for, and that job's run (0 when unknown).
    pub minted_for: u64,
    pub minted_run: u64,
    /// The profile it was minted under, whose policy judges what it took.
    pub profile_idx: usize,
    /// The hook's outcome at its deadline, decided at mint.
    pub outcome: DeadlineOutcome,
    worker_since: Option<u64>,
    hook_since: Option<u64>,
    hook_gone: bool,
    last_search: Option<u64>,
    gave_up: bool,
    judged: Option<Judged>,
    delivered: Option<StartDecision>,
    kill: bool,
    recorded: bool,
    ended: bool,
    /// Whether the container has been seen running. A runner is watched from
    /// its mint, before its container exists, so until then "not there" means
    /// "not yet", not "gone".
    started: bool,
}

impl Watch {
    pub fn new(
        repo: &str,
        runner: &str,
        minted_for: u64,
        minted_run: u64,
        profile_idx: usize,
        outcome: DeadlineOutcome,
    ) -> Self {
        Self {
            repo: repo.to_string(),
            runner: runner.to_string(),
            minted_for,
            minted_run,
            profile_idx,
            outcome,
            worker_since: None,
            hook_since: None,
            hook_gone: false,
            last_search: None,
            gave_up: false,
            judged: None,
            delivered: None,
            kill: false,
            recorded: false,
            ended: false,
            started: false,
        }
    }

    /// The job found and judged, once it is.
    pub fn judged(&self) -> Option<&Judged> {
        self.judged.as_ref()
    }

    /// Hand over what a [`Act::Search`] found.
    pub fn judge(&mut self, judged: Judged) {
        if self.judged.is_none() {
            self.judged = Some(judged);
        }
    }

    /// Whether there is nothing left to watch: the container is gone, or the
    /// job was allowed and runs on.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Whether faber has decided to stop this runner.
    pub fn stopping(&self) -> bool {
        self.kill
    }

    /// The kill landed: the container is gone.
    pub fn killed(&mut self) {
        self.ended = true;
    }

    /// An allow did not reach the hook: deliver it again while it waits.
    pub fn redeliver(&mut self) {
        if self.delivered == Some(StartDecision::Allow) {
            self.delivered = None;
            self.ended = false;
        }
    }

    /// The decision in force: the one faber found, or, once its search has
    /// given up, the plan's outcome at the deadline.
    fn decision(&self) -> Option<StartDecision> {
        if let Some(j) = &self.judged {
            return Some(j.decision);
        }
        (self.gave_up && self.outcome == DeadlineOutcome::Refuse)
            .then_some(StartDecision::Refuse(Reason::NoVerdict))
    }

    /// Decide what to do about this runner, given what was seen of it `now`
    /// (epoch ms).
    pub fn step(
        &mut self,
        now: u64,
        seen: Seen,
        deadlines: &JobStartDeadlines,
        on_bypass: OnBypass,
    ) -> Vec<Act> {
        if self.ended {
            return Vec::new();
        }
        let top = match seen {
            Seen::Gone => {
                // Before its container first ran, absent is not yet started.
                // A kill that was asked for is done either way.
                if self.started || self.kill {
                    self.ended = true;
                }
                return Vec::new();
            }
            Seen::Unknown => {
                return if self.kill {
                    vec![Act::Kill]
                } else {
                    Vec::new()
                }
            }
            Seen::Running(top) => top,
        };
        self.started = true;
        if self.kill {
            return vec![Act::Kill];
        }
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let mut acts = Vec::new();

        if !top.worker {
            if self.worker_since.is_none() {
                // Not assigned yet: nothing to do.
                return acts;
            }
            // The worker went. Either the job ran to its end after an allow, or
            // the hook stopped it from inside, which it does on every path but an
            // accepted allow. A hook seen waiting that faber delivered no allow
            // to stopped its job: that is a refusal at start, faber's stop is the
            // cleanup, and the record is faber's to keep.
            if self.hook_since.is_some()
                && self.delivered.is_none()
                && !self.allowed_at_deadline(now, deadlines)
            {
                let reason = match self.decision() {
                    Some(StartDecision::Refuse(r)) => r,
                    _ => Reason::NoVerdict,
                };
                self.kill = true;
                if !self.recorded {
                    self.recorded = true;
                    acts.push(Act::RefusedAtStart(reason));
                }
                acts.push(Act::Kill);
                return acts;
            }
            self.ended = true;
            return acts;
        }

        let since = *self.worker_since.get_or_insert(now);
        if top.hook_waiting {
            self.hook_since.get_or_insert(now);
        } else if self.hook_since.is_some() {
            self.hook_gone = true;
        }

        // Find and judge the job, until the search deadline.
        if self.judged.is_none() && !self.gave_up {
            if now.saturating_sub(since) < ms(deadlines.search) {
                let due = self
                    .last_search
                    .is_none_or(|t| now.saturating_sub(t) >= ms(SEARCH_EVERY));
                if due {
                    self.last_search = Some(now);
                    acts.push(Act::Search);
                }
            } else {
                self.gave_up = true;
                if self.outcome == DeadlineOutcome::Allow && !self.recorded {
                    // The hook will run the job at its own deadline; faber has
                    // nothing to deliver that the hook would accept.
                    self.recorded = true;
                    acts.push(Act::Unverified);
                }
            }
        }

        // Deliver, only to a hook seen waiting now.
        if let Some(decision) = self.decision() {
            if self.delivered.is_none() && top.hook_waiting {
                let (run_id, run_attempt) = self
                    .judged
                    .as_ref()
                    .map_or((0, 0), |j| (j.run_id, j.run_attempt));
                acts.push(Act::Deliver(Delivery {
                    decision,
                    run_id,
                    run_attempt,
                }));
                self.delivered = Some(decision);
                match decision {
                    StartDecision::Allow => self.ended = true,
                    StartDecision::Refuse(r) => {
                        self.kill = true;
                        if !self.recorded {
                            self.recorded = true;
                            acts.push(Act::RefusedAtStart(r));
                        }
                        acts.push(Act::Kill);
                    }
                }
                return acts;
            }
        }

        // A job in progress with no waiting hook: never seen by the search
        // deadline, or gone with the worker still running and no allow given.
        let never_seen =
            self.hook_since.is_none() && now.saturating_sub(since) >= ms(deadlines.search);
        let vanished = self.hook_gone && !self.allowed_at_deadline(now, deadlines);
        if self.delivered.is_none() && (never_seen || vanished) {
            let refused = matches!(self.decision(), Some(StartDecision::Refuse(_)));
            let stop = on_bypass == OnBypass::Enforce || refused;
            acts.push(Act::HookFault {
                stop,
                why: if never_seen {
                    "no job-started hook was seen waiting"
                } else {
                    "the job-started hook exited without a verdict and the job ran on"
                },
            });
            if stop {
                self.kill = true;
                acts.push(Act::Kill);
            } else {
                self.ended = true;
            }
        }
        acts
    }

    /// Whether the hook's own deadline has passed under an allow outcome, so a
    /// hook that left without faber's verdict ran its job as the plan said.
    fn allowed_at_deadline(&self, now: u64, deadlines: &JobStartDeadlines) -> bool {
        let hook = u64::try_from(deadlines.hook.as_millis()).unwrap_or(u64::MAX);
        self.outcome == DeadlineOutcome::Allow
            && self
                .hook_since
                .is_some_and(|t| now.saturating_sub(t) >= hook)
    }
}

/// Whether a step after the job-started hook's has started, by GitHub's record
/// of `steps` (ADR-0059 §6). The hook runs as the step "Set up runner", right
/// after "Set up job". Only a step in that place counts as the hook's: a
/// workflow can name any step "Set up runner", so with none there, everything
/// after the first step counts. A step counts as started once it is in
/// progress, or completed other than skipped.
///
/// `None` steps (the field absent) is unknown, and reads as started: a
/// refused-at-start record holds only on positive evidence.
pub fn ran_past_hook(steps: Option<&[crate::github::Step]>) -> bool {
    let Some(steps) = steps else {
        return true;
    };
    let hook = if steps
        .iter()
        .any(|s| s.number == 2 && s.name == "Set up runner")
    {
        2
    } else {
        1
    };
    steps.iter().filter(|s| s.number > hook).any(|s| {
        s.status == "in_progress"
            || (s.status == "completed" && s.conclusion.as_deref() != Some("skipped"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNER: &str = "faber-5-100-alpha";

    fn deadlines() -> JobStartDeadlines {
        JobStartDeadlines {
            search: Duration::from_secs(60),
            hook: Duration::from_secs(90),
        }
    }

    fn watch(outcome: DeadlineOutcome) -> Watch {
        Watch::new("owner/demo", RUNNER, 5, 50, 0, outcome)
    }

    fn top(worker: bool, hook: bool) -> Seen {
        Seen::Running(Top {
            worker,
            hook_waiting: hook,
        })
    }

    fn judged(decision: StartDecision) -> Judged {
        Judged {
            job_id: 9,
            run_id: 70,
            run_attempt: 2,
            job_name: "build".into(),
            workflow: "CI".into(),
            verdict: "refuse:job_token_dispatch".into(),
            decision,
        }
    }

    #[test]
    fn the_deadline_outcome_is_allow_only_on_a_private_repo_under_record() {
        use DeadlineOutcome::*;
        assert_eq!(DeadlineOutcome::at_mint(true, OnBypass::Record), Allow);
        assert_eq!(DeadlineOutcome::at_mint(true, OnBypass::Enforce), Refuse);
        assert_eq!(DeadlineOutcome::at_mint(false, OnBypass::Record), Refuse);
        assert_eq!(DeadlineOutcome::at_mint(false, OnBypass::Enforce), Refuse);
        assert_eq!(DeadlineOutcome::parse("allow"), Some(Allow));
        assert_eq!(DeadlineOutcome::parse("ALLOW"), None);
    }

    /// ADR-0059 §5, row by row.
    #[test]
    fn start_decisions_follow_the_table() {
        use RefuseReason as R;
        let refuse = |r: R| Verdict::Refuse(r);
        for mode in [OnBypass::Record, OnBypass::Enforce] {
            assert_eq!(
                start_decision(&Verdict::Provision, mode),
                StartDecision::Allow
            );
            // Third-party code and a job-token dispatch: refused in both modes.
            for (r, why) in [
                (R::ThirdParty("a-stranger".into()), Reason::Author),
                (R::DestroyedGroup, Reason::Author),
                (R::Unread, Reason::Author),
                (R::JobTokenDispatch, Reason::Actor),
            ] {
                assert_eq!(start_decision(&refuse(r), mode), StartDecision::Refuse(why));
            }
            // Policy: allowed in both.
            assert_eq!(
                start_decision(&refuse(R::DisallowedEvent("release".into())), mode),
                StartDecision::Allow
            );
        }
        // Trust: refused under enforce, allowed (and left to detection) under record.
        for (r, why) in [
            (R::Fork, Reason::Fork),
            (R::PublicRepo, Reason::PublicRepository),
            (R::UntrustedActor("octo-bot".into()), Reason::Actor),
            (
                R::DisallowedEvent("pull_request_target".into()),
                Reason::Event,
            ),
        ] {
            assert_eq!(
                start_decision(&refuse(r.clone()), OnBypass::Enforce),
                StartDecision::Refuse(why)
            );
            assert_eq!(
                start_decision(&refuse(r), OnBypass::Record),
                StartDecision::Allow
            );
        }
    }

    #[test]
    fn the_hook_script_carries_the_plan_and_only_fixed_reasons() {
        let s = HookPlan {
            outcome: DeadlineOutcome::Allow,
            deadline: Duration::from_secs(180),
        }
        .script();
        assert!(!s.contains(POLICY_SLOT));
        assert!(
            s.contains(r#"{"deadline_ms":180000,"dir":"/tmp/faber-verdict","outcome":"allow"}"#)
        );
        assert!(s.contains(&format!("process.title = '{HOOK_COMM}'")));
        for r in [
            Reason::Event,
            Reason::Fork,
            Reason::Actor,
            Reason::Author,
            Reason::PublicRepository,
            Reason::NoVerdict,
            Reason::Unverified,
        ] {
            assert!(s.contains(&format!("'{}'", r.word())), "{r:?}");
        }
        assert!(HOOK_COMM.len() <= 15);
        assert!(hook_path().ends_with(".js"));
    }

    #[test]
    fn a_verdict_archive_is_a_tar_with_the_verdict_before_ready() {
        let d = Delivery {
            decision: StartDecision::Refuse(Reason::Actor),
            run_id: 70,
            run_attempt: 2,
        };
        assert_eq!(
            d.json(),
            r#"{"reason":"actor","run_attempt":2,"run_id":70,"verdict":"refuse"}"#
        );
        let a = d.archive();
        assert_eq!(a.len() % 512, 0);
        assert_eq!(&a[..12], b"verdict.json");
        assert_eq!(&a[257..262], b"ustar");
        // The header checksum is the sum of its bytes with the field as spaces.
        let mut h = a[..512].to_vec();
        let field = String::from_utf8_lossy(&h[148..154]).to_string();
        h[148..156].copy_from_slice(b"        ");
        let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
        assert_eq!(u64::from_str_radix(&field, 8).unwrap(), sum);
        // The data block, then `ready`'s header.
        assert_eq!(&a[512..512 + d.json().len()], d.json().as_bytes());
        assert_eq!(&a[1024..1029], b"ready");
    }

    #[test]
    fn top_sees_the_worker_and_only_its_own_hook() {
        let out = "PID PPID COMMAND\n1 0 run.sh\n40 1 Runner.Listener\n90 40 Runner.Worker\n\
                   120 90 faber-hook\n";
        assert_eq!(
            Top::parse(out),
            Top {
                worker: true,
                hook_waiting: true
            }
        );
        // A process that only calls itself the hook, not under the worker.
        let forged = "PID PPID COMMAND\n90 40 Runner.Worker\n130 1 faber-hook\n";
        assert_eq!(
            Top::parse(forged),
            Top {
                worker: true,
                hook_waiting: false
            }
        );
        assert_eq!(Top::parse("PID PPID COMMAND\n1 0 run.sh\n"), Top::default());
    }

    #[test]
    fn nothing_happens_before_the_assignment() {
        let mut w = watch(DeadlineOutcome::Refuse);
        // Watched from the mint: a container not there yet is not gone.
        assert!(w
            .step(0, Seen::Gone, &deadlines(), OnBypass::Enforce)
            .is_empty());
        assert!(!w.ended());
        assert!(w
            .step(0, top(false, false), &deadlines(), OnBypass::Enforce)
            .is_empty());
        assert!(!w.ended());
        // Once it has run, not there is gone.
        w.step(1, Seen::Gone, &deadlines(), OnBypass::Enforce);
        assert!(w.ended());
    }

    #[test]
    fn an_allow_reaches_a_waiting_hook_and_ends_the_watch() {
        let mut w = watch(DeadlineOutcome::Refuse);
        let d = deadlines();
        assert_eq!(
            w.step(1_000, top(true, false), &d, OnBypass::Record),
            vec![Act::Search]
        );
        // Searching again waits SEARCH_EVERY.
        assert!(w
            .step(2_000, top(true, false), &d, OnBypass::Record)
            .is_empty());
        w.judge(judged(StartDecision::Allow));
        // Found, but no hook waiting yet: nothing delivered.
        assert!(w
            .step(3_000, top(true, false), &d, OnBypass::Record)
            .is_empty());
        assert_eq!(
            w.step(4_000, top(true, true), &d, OnBypass::Record),
            vec![Act::Deliver(Delivery {
                decision: StartDecision::Allow,
                run_id: 70,
                run_attempt: 2
            })]
        );
        assert!(w.ended());
    }

    #[test]
    fn a_refusal_is_delivered_recorded_and_killed_until_gone() {
        let mut w = watch(DeadlineOutcome::Allow);
        let d = deadlines();
        w.step(1_000, top(true, true), &d, OnBypass::Record);
        w.judge(judged(StartDecision::Refuse(Reason::Actor)));
        let acts = w.step(2_000, top(true, true), &d, OnBypass::Record);
        assert_eq!(
            acts,
            vec![
                Act::Deliver(Delivery {
                    decision: StartDecision::Refuse(Reason::Actor),
                    run_id: 70,
                    run_attempt: 2
                }),
                Act::RefusedAtStart(Reason::Actor),
                Act::Kill
            ]
        );
        // The kill is asked for again until the container is gone.
        assert_eq!(
            w.step(3_000, top(false, false), &d, OnBypass::Record),
            vec![Act::Kill]
        );
        assert_eq!(
            w.step(4_000, Seen::Unknown, &d, OnBypass::Record),
            vec![Act::Kill]
        );
        assert!(w.step(5_000, Seen::Gone, &d, OnBypass::Record).is_empty());
        assert!(w.ended());
    }

    #[test]
    fn no_verdict_by_the_search_deadline_applies_the_plan() {
        let d = deadlines();
        // Refuse: faber gives up and stops it, as the hook would at its deadline.
        let mut w = watch(DeadlineOutcome::Refuse);
        w.step(0, top(true, true), &d, OnBypass::Record);
        let acts = w.step(60_000, top(true, true), &d, OnBypass::Record);
        assert_eq!(
            acts,
            vec![
                Act::Deliver(Delivery {
                    decision: StartDecision::Refuse(Reason::NoVerdict),
                    run_id: 0,
                    run_attempt: 0
                }),
                Act::RefusedAtStart(Reason::NoVerdict),
                Act::Kill
            ]
        );
        // Allow: recorded unverified, and left to the hook's own deadline.
        let mut w = watch(DeadlineOutcome::Allow);
        w.step(0, top(true, true), &d, OnBypass::Record);
        assert_eq!(
            w.step(60_000, top(true, true), &d, OnBypass::Record),
            vec![Act::Unverified]
        );
        // The hook leaves at its deadline and the job runs: not a fault.
        assert!(w
            .step(91_000, top(true, false), &d, OnBypass::Record)
            .is_empty());
    }

    #[test]
    fn a_job_in_progress_with_no_waiting_hook_is_a_hook_fault() {
        let d = deadlines();
        // Under record, an allowed job is only recorded.
        let mut w = watch(DeadlineOutcome::Allow);
        w.step(0, top(true, false), &d, OnBypass::Record);
        w.judge(judged(StartDecision::Allow));
        let acts = w.step(60_000, top(true, false), &d, OnBypass::Record);
        assert!(acts.contains(&Act::HookFault {
            stop: false,
            why: "no job-started hook was seen waiting"
        }));
        assert!(!acts.contains(&Act::Kill));
        // Under record, a job §5 refuses at start is stopped.
        let mut w = watch(DeadlineOutcome::Allow);
        w.step(0, top(true, false), &d, OnBypass::Record);
        w.judge(judged(StartDecision::Refuse(Reason::Author)));
        let acts = w.step(60_000, top(true, false), &d, OnBypass::Record);
        assert!(acts.contains(&Act::Kill), "{acts:?}");
        // Under enforce, every such job is stopped.
        let mut w = watch(DeadlineOutcome::Refuse);
        w.step(0, top(true, false), &d, OnBypass::Enforce);
        let acts = w.step(60_000, top(true, false), &d, OnBypass::Enforce);
        assert!(acts
            .iter()
            .any(|a| matches!(a, Act::HookFault { stop: true, .. })));
        // A hook that left without a verdict while the worker runs on.
        let mut w = watch(DeadlineOutcome::Refuse);
        w.step(0, top(true, true), &d, OnBypass::Enforce);
        let acts = w.step(5_000, top(true, false), &d, OnBypass::Enforce);
        assert!(acts
            .iter()
            .any(|a| matches!(a, Act::HookFault { stop: true, .. })));
    }

    #[test]
    fn a_hook_that_stopped_its_own_job_is_a_refusal_at_start() {
        let d = deadlines();
        let mut w = watch(DeadlineOutcome::Refuse);
        w.step(0, top(true, true), &d, OnBypass::Record);
        // The worker and the hook are gone, and faber delivered nothing.
        let acts = w.step(5_000, top(false, false), &d, OnBypass::Record);
        assert_eq!(
            acts,
            vec![Act::RefusedAtStart(Reason::NoVerdict), Act::Kill]
        );
        // A job that ran to its end after an allow is just over.
        let mut w = watch(DeadlineOutcome::Refuse);
        w.step(0, top(true, false), &d, OnBypass::Record);
        assert!(w
            .step(5_000, top(false, false), &d, OnBypass::Record)
            .is_empty());
        assert!(w.ended());
    }

    /// The hook itself, run on Node when this machine has it: under a shell
    /// that stands in for the runner's worker, against a verdict delivered as
    /// the archive faber writes. Skipped where there is no `node`.
    #[test]
    fn the_hook_acts_on_a_delivered_verdict_and_kills_its_parent_otherwise() {
        if std::process::Command::new("node")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("no node on this machine: the hook is not run");
            return;
        }
        let root = std::env::temp_dir().join(format!("faber-hook-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Run the hook as the worker would, with `deliver` written into its
        // verdict directory once it waits. The shell is the hook's parent: it
        // prints `survived` only if the hook let it live.
        let run =
            |name: &str, outcome: DeadlineOutcome, deadline_ms: u64, deliver: Option<Delivery>| {
                let dir = root.join(name);
                let script = HookPlan {
                    outcome,
                    deadline: Duration::from_millis(deadline_ms),
                }
                .script()
                .replace(VERDICT_DIR, dir.to_str().unwrap());
                let hook = root.join(format!("{name}.js"));
                std::fs::write(&hook, script).unwrap();
                let archive = root.join(format!("{name}.tar"));
                let deliver_cmd = match deliver {
                    Some(d) => {
                        std::fs::write(&archive, d.archive()).unwrap();
                        format!(
                            "(while [ ! -d '{dir}' ]; do sleep 0.05; done; sleep 0.2; \
                         tar -xf '{tar}' -C '{dir}') &",
                            dir = dir.display(),
                            tar = archive.display()
                        )
                    }
                    None => String::new(),
                };
                std::process::Command::new("sh")
                    .arg("-c")
                    .arg(format!(
                        "{deliver_cmd} node '{}'; echo survived",
                        hook.display()
                    ))
                    .env("GITHUB_RUN_ID", "70")
                    .env("GITHUB_RUN_ATTEMPT", "2")
                    .output()
                    .unwrap()
            };
        let allow = |run_id| Delivery {
            decision: StartDecision::Allow,
            run_id,
            run_attempt: 2,
        };
        let survived =
            |o: &std::process::Output| String::from_utf8_lossy(&o.stdout).contains("survived");

        // An allow for this run and attempt: the job goes on.
        let o = run("allow", DeadlineOutcome::Refuse, 10_000, Some(allow(70)));
        assert!(survived(&o), "{o:?}");
        // An allow for another run is not this job's: stopped.
        let o = run("other", DeadlineOutcome::Allow, 10_000, Some(allow(71)));
        assert!(!survived(&o), "{o:?}");
        assert!(String::from_utf8_lossy(&o.stdout).contains("(no verdict)"));
        // A refusal: its one fixed line, and the worker is gone.
        let refuse = Delivery {
            decision: StartDecision::Refuse(Reason::Author),
            run_id: 70,
            run_attempt: 2,
        };
        let o = run("refuse", DeadlineOutcome::Allow, 10_000, Some(refuse));
        assert!(!survived(&o), "{o:?}");
        assert!(String::from_utf8_lossy(&o.stdout)
            .contains("faber: this job was refused before its first step (author)"));
        // No verdict by the deadline: the plan decides.
        let o = run("late-refuse", DeadlineOutcome::Refuse, 300, None);
        assert!(!survived(&o), "{o:?}");
        let o = run("late-allow", DeadlineOutcome::Allow, 300, None);
        assert!(survived(&o), "{o:?}");
        assert!(String::from_utf8_lossy(&o.stdout).contains("(unverified)"));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn step(n: i64, name: &str, status: &str, conclusion: Option<&str>) -> crate::github::Step {
        crate::github::Step {
            name: name.into(),
            status: status.into(),
            number: n,
            conclusion: conclusion.map(Into::into),
            started_at: None,
            completed_at: None,
        }
    }

    #[test]
    fn a_step_after_the_hook_is_what_makes_a_refusal_a_bypass() {
        let refused = [
            step(1, "Set up job", "completed", Some("success")),
            step(2, "Set up runner", "completed", Some("failure")),
            step(3, "Run tests", "queued", None),
            step(4, "Post checkout", "completed", Some("skipped")),
        ];
        assert!(!ran_past_hook(Some(&refused)));
        let ran = [
            step(1, "Set up job", "completed", Some("success")),
            step(2, "Set up runner", "completed", Some("failure")),
            step(3, "Run tests", "in_progress", None),
        ];
        assert!(ran_past_hook(Some(&ran)));
        // A workflow step named like the hook's, later in the job, does not
        // hide the steps before it.
        let renamed = [
            step(1, "Set up job", "completed", Some("success")),
            step(2, "Build", "completed", Some("success")),
            step(3, "Set up runner", "in_progress", None),
        ];
        assert!(ran_past_hook(Some(&renamed)));
        // No record of steps is no evidence: it reads as ran.
        assert!(ran_past_hook(None));
    }
}
