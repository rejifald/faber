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
//! waiting hook is a hook fault ([`Act::HookFault`]), where faber can show one.
//! The orchestrator does the I/O each act names; [`Watch::step`] only decides,
//! so every rule here is a plain function of what was seen.
//!
//! A hook fault is a claim about the runner, so the watch makes it only on
//! what it saw. The hook lets a job run in two ways: on an allow it accepts,
//! and at its own deadline where its outcome is allow. A hook that leaves the
//! job running under a refuse outcome therefore failed, whenever it started.
//! Under an allow outcome it failed only if it left before its deadline, and
//! the watch can show that only when it saw the runner with no hook less than
//! one hook deadline ago. A first sight that came late leaves the hook's start
//! unknown, and then the watch claims nothing ([`Act::RanUnverified`], or
//! [`Act::StoppedAfterStart`] on a host that enforces).
//!
//! A watch lives in one process, and a runner can outlive it: the next process
//! adopts the runner (ADR-0011) and begins a new watch mid-job, with the worker
//! running and no hook waiting. That is what a job whose hook never ran looks
//! like too. What tells them apart is faber's own record that the job got past
//! its hook ([`Passed`]), written when it happened and given to the new watch
//! ([`Watch::adopted`]). That watch judges the job again, and its judgement
//! decides: a job the gate now refuses is stopped, as a record of its own and
//! not as a hook fault. Where it cannot judge by the search deadline, the
//! record stands in for a judgement, as far as it holds under the mode in
//! force and only until there is one ([`Act::Stands`]). With no record and no
//! hook in sight, the watch did not see the job start, and under an allow
//! outcome it cannot say the hook never ran ([`Act::NotWatched`]).

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

/// Whether [`start_decision`] allows `verdict` under `on_bypass` only because
/// the host records: a trust refusal, which `enforce` refuses at start. Every
/// other allow is the same decision in either mode. Kept with the allow faber
/// records ([`Passed::Allowed`]), so that one made under `record` is not
/// carried into `enforce` by a restart.
pub fn allowed_only_under_record(verdict: &Verdict, on_bypass: OnBypass) -> bool {
    start_decision(verdict, on_bypass) == StartDecision::Allow
        && start_decision(verdict, OnBypass::Enforce) != StartDecision::Allow
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

/// How a job got past its job-started hook while faber watched: what a
/// [`Watch`] knew when it ended with its job running on.
///
/// The orchestrator writes it down when it happens (`audit::Record::job_start`)
/// and gives it to a watch a later process begins on the same runner
/// ([`Watch::adopted`]). It says why no hook is waiting there. That watch
/// judges the job again, and its judgement decides. Only when it cannot judge
/// by the search deadline does the record stand in for one, and then only as
/// far as it would hold under the mode in force ([`Self::holds`]), and only
/// until the watch has a judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passed {
    /// faber delivered an allow for this run and attempt to the hook it saw
    /// waiting. The hook acts on an allow for no other. `record_only` is
    /// whether the allow was a trust refusal let through because the host
    /// records ([`allowed_only_under_record`]).
    Allowed {
        run_id: u64,
        run_attempt: u64,
        record_only: bool,
    },
    /// The hook, seen waiting, left at its own deadline under an allow
    /// outcome, and the job runs with no verdict (ADR-0059 §5).
    Unverified,
}

impl Passed {
    /// The audit `action` of this record.
    pub fn word(self) -> &'static str {
        match self {
            Passed::Allowed {
                record_only: false, ..
            } => "allowed",
            Passed::Allowed {
                record_only: true, ..
            } => "allowed_under_record",
            Passed::Unverified => "unverified",
        }
    }

    /// The run and attempt a delivered allow was for.
    pub fn run(self) -> Option<(u64, u64)> {
        match self {
            Passed::Allowed {
                run_id,
                run_attempt,
                ..
            } => Some((run_id, run_attempt)),
            Passed::Unverified => None,
        }
    }

    /// The inverse of [`Self::word`] and [`Self::run`], for an audit line. An
    /// allow that does not say which run and attempt it was for is `None`,
    /// and so is any other word: a record that cannot be read is no record.
    pub fn read(word: &str, run: Option<(u64, u64)>) -> Option<Self> {
        let allowed = |record_only| {
            run.map(|(run_id, run_attempt)| Passed::Allowed {
                run_id,
                run_attempt,
                record_only,
            })
        };
        match word {
            "allowed" => allowed(false),
            "allowed_under_record" => allowed(true),
            "unverified" => Some(Passed::Unverified),
            _ => None,
        }
    }

    /// Whether what was decided before the restart is still what ADR-0059 §5
    /// decides under `on_bypass`, the mode now in force. An allow the gate
    /// gives in either mode holds in both. A trust refusal let through under
    /// `record`, and a job that ran with no verdict, hold only under `record`:
    /// `enforce` refuses both at start.
    ///
    /// The mode is the one change to the gate this can see. A gate changed in
    /// another way since, or a fact GitHub has changed, takes a judgement to
    /// find.
    pub fn holds(self, on_bypass: OnBypass) -> bool {
        match self {
            Passed::Allowed {
                record_only: false, ..
            } => true,
            Passed::Allowed {
                record_only: true, ..
            }
            | Passed::Unverified => on_bypass == OnBypass::Record,
        }
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
    /// Whether `decision` is an allow that only `record` gives
    /// ([`allowed_only_under_record`]).
    pub record_only: bool,
}

/// What [`Watch::step`] asks the orchestrator to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Find the job whose `runner_name` is this runner, judge it, and hand the
    /// result to [`Watch::judge`], or why there is none to [`Watch::missed`].
    /// `wide` also looks past the runs GitHub lists as queued or in progress.
    Search { wide: bool },
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
    /// Write down that the hook, seen waiting, left at its own deadline and
    /// the job runs on ([`Passed::Unverified`]), for a watch a later process
    /// begins on this runner. An allow's record is written once its
    /// [`Act::Deliver`] has landed ([`Watch::passed_by`]).
    RanUnverified,
    /// Record that a job past its hook before this watch began could not be
    /// judged again by the search deadline, and that what faber recorded of
    /// it stands for now ([`Passed::holds`]). The job runs on, and the watch
    /// goes on looking for a judgement ([`LATE_SEARCH_EVERY`]).
    Stands(Passed),
    /// Record that the job on an adopted runner started while faber was not
    /// watching: no hook was seen and nothing says one let the job through,
    /// under an allow outcome on a host that records. It runs unverified. Not
    /// a hook fault: faber cannot show that its hook never ran.
    NotWatched,
    /// Record that a job faber let run without a fresh judgement
    /// ([`Act::Stands`], [`Act::NotWatched`]) has been judged after all, and
    /// the gate allows it.
    Recovered,
    /// Record that faber stops a job that is past its start, for a reason
    /// that is neither a refusal at start nor a hook fault it can show. An
    /// [`Act::Kill`] follows it.
    StoppedAfterStart(StopWhy),
    /// Record a hook fault: a job in progress with no waiting hook (§2). `stop`
    /// says whether faber also stops it. A watch asks once. The watch a later
    /// process begins on the same runner asks again, and the orchestrator
    /// keeps the fault to one for the runner.
    HookFault { stop: bool, why: &'static str },
}

/// Why faber stops a job that is past its start ([`Act::StoppedAfterStart`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopWhy {
    /// A judgement refuses a job that faber's own record says it let past
    /// its hook.
    Refused,
    /// What faber recorded of the job does not hold under `enforce`
    /// ([`Passed::holds`]), and there is no judgement by the search deadline.
    DoesNotHold,
    /// The job's hook left under an allow outcome and faber cannot place
    /// when it started, on a host that now enforces: an unverified job does
    /// not run on there.
    HookStartUnknown,
    /// faber did not watch the job start, and nothing says its hook let it
    /// through. The host enforces, or the gate refuses the job.
    NotWatched,
}

impl StopWhy {
    /// The reason in words, for the log and the fault line.
    pub fn words(self) -> &'static str {
        match self {
            StopWhy::Refused => "the gate now refuses a job faber allowed at its start",
            StopWhy::DoesNotHold => {
                "what faber recorded of this job does not hold under enforce, and it could not \
                 be judged again by the search deadline"
            }
            StopWhy::HookStartUnknown => {
                "its hook left with no verdict and faber cannot tell when it started, and an \
                 unverified job does not run on under enforce"
            }
            StopWhy::NotWatched => {
                "it started while faber was not watching, and nothing says its hook let it \
                 through"
            }
        }
    }
}

/// How often one runner's job is looked for while it is not yet found: every
/// lookup is a few GitHub calls.
pub const SEARCH_EVERY: Duration = Duration::from_secs(4);

/// How often a watch looks again for a job it let run without a fresh
/// judgement ([`Act::Stands`], [`Act::NotWatched`]), once its search deadline
/// has passed. Slow, because that job is already running and each look is a
/// few GitHub calls.
pub const LATE_SEARCH_EVERY: Duration = Duration::from_secs(60);

/// How long the ordinary search may miss before a search goes wide, and how
/// often one does after that. A job's run is nearly always listed as queued or
/// in progress within seconds of its start, so the wide search, which reads
/// every recent run GitHub has not completed, is for the run those two listings
/// leave out.
pub const WIDE_SEARCH_AFTER: Duration = Duration::from_secs(20);
pub const WIDE_SEARCH_EVERY: Duration = Duration::from_secs(20);

/// Why a search came back without a judged job. Kept on the [`Watch`] so a
/// refusal for want of a verdict says what stood in the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    /// No run GitHub listed holds a job naming this runner.
    NotListed,
    /// The job was found, and GitHub did not say whose code its run carries.
    Unread { job_id: u64, why: String },
    /// GitHub did not answer a read the search needs.
    Github(String),
}

impl Miss {
    /// The miss in words, for the log and the fault.
    pub fn words(&self) -> String {
        match self {
            Miss::NotListed => "no run GitHub listed holds a job on this runner".to_string(),
            Miss::Unread { job_id, why } => {
                format!("job {job_id} was found, and GitHub did not say whose code it runs: {why}")
            }
            Miss::Github(why) => format!("GitHub did not answer: {why}"),
        }
    }
}

/// How late faber may first see a hook that is already waiting: one pass of the
/// job-start loop (2 s, `JOB_START_TICK` in reconcile.rs), a second for the pass
/// that sees it leave, and the hook's own 250 ms poll, rounded up.
pub const HOOK_SEEN_GRACE: Duration = Duration::from_secs(5);

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
    last_wide: Option<u64>,
    last_miss: Option<Miss>,
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
    /// What faber's own record says of this runner's job and its hook from
    /// before this watch began ([`Self::adopted`]). `None` on a watch begun at
    /// the mint, which sees for itself, and once a judgement shows the record
    /// was for another job.
    passed: Option<Passed>,
    /// Whether the watch was begun on a runner found running
    /// ([`Self::adopted`]): it did not see the job start.
    adopted: bool,
    /// The latest pass, before this watch first saw a hook waiting, in which
    /// the runner had none: its container was not running yet, or ran with
    /// no hook waiting. A hook seen later started after it, which is the one
    /// thing that bounds the hook's age from above ([`Self::left_early`]).
    hook_absent_at: Option<u64>,
    /// Past its search deadline, the watch lets the job run without a fresh
    /// judgement and goes on looking for one ([`Act::Stands`],
    /// [`Act::NotWatched`]).
    late: bool,
}

impl Watch {
    /// Watch a runner from its mint, before its container exists: this watch
    /// sees the worker and the hook appear. A runner found already running is
    /// watched with [`Self::adopted`].
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
            last_wide: None,
            last_miss: None,
            gave_up: false,
            judged: None,
            delivered: None,
            kill: false,
            recorded: false,
            ended: false,
            started: false,
            passed: None,
            adopted: false,
            hook_absent_at: None,
            late: false,
        }
    }

    /// Watch a runner this process did not start, found running after a
    /// restart (ADR-0011). Its job may already be past its hook, so this watch
    /// may never see one wait.
    ///
    /// `passed` is what faber's own record says of that: the allow it
    /// delivered to the hook it saw waiting, or the hook leaving at its
    /// deadline as planned. With it, a job the gate still allows is not a hook
    /// fault, and one faber cannot judge again in time runs on for now, as far
    /// as the record holds under the mode in force ([`Self::step`]).
    ///
    /// `None` is no such record. This watch did not see the job start, so it
    /// cannot show that the job's hook never ran. Where the hook would have
    /// stopped the job at its deadline (a refuse outcome), a job still running
    /// is a hook fault all the same. Under an allow outcome the hook may have
    /// let the job run at its deadline while nobody watched, and that is not
    /// one ([`Act::NotWatched`]).
    ///
    /// It takes the record as an argument so that every watch begun on a
    /// running runner has to answer for it.
    pub fn adopted(
        repo: &str,
        runner: &str,
        minted_for: u64,
        minted_run: u64,
        profile_idx: usize,
        outcome: DeadlineOutcome,
        passed: Option<Passed>,
    ) -> Self {
        Self {
            passed,
            adopted: true,
            ..Self::new(repo, runner, minted_for, minted_run, profile_idx, outcome)
        }
    }

    /// What faber's own record says of this runner's job: what the watch was
    /// begun with, until a judgement shows that record was for another job.
    pub fn passed(&self) -> Option<Passed> {
        self.passed
    }

    /// Whether the watch is past its search deadline, letting its job run
    /// without a fresh judgement and still looking for one.
    pub fn late(&self) -> bool {
        self.late
    }

    /// What faber may write down once `d`, a verdict this watch asked to
    /// deliver, has landed: an allow let the job past its hook. A refusal lets
    /// nothing past.
    pub fn passed_by(&self, d: &Delivery) -> Option<Passed> {
        (d.decision == StartDecision::Allow).then(|| Passed::Allowed {
            run_id: d.run_id,
            run_attempt: d.run_attempt,
            // An allow is delivered only for a judged job. Were there none,
            // nothing would say the allow holds under `enforce`.
            record_only: self.judged.as_ref().is_none_or(|j| j.record_only),
        })
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

    /// Hand over why a [`Act::Search`] found no job to judge.
    pub fn missed(&mut self, miss: Miss) {
        self.last_miss = Some(miss);
    }

    /// Why the latest search found no job to judge, if one has missed.
    pub fn last_miss(&self) -> Option<&Miss> {
        self.last_miss.as_ref()
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
                } else {
                    // No container yet, so no hook either.
                    self.note_no_hook(now);
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
                self.note_no_hook(now);
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
        } else {
            self.note_no_hook(now);
        }
        let past_search = now.saturating_sub(since) >= ms(deadlines.search);

        // An allow faber recorded for another run or attempt than the job
        // found on this runner is not this job's: the job has no record.
        if let (
            Some(Passed::Allowed {
                run_id,
                run_attempt,
                ..
            }),
            Some(j),
        ) = (self.passed, &self.judged)
        {
            if (j.run_id, j.run_attempt) != (run_id, run_attempt) {
                self.passed = None;
                self.late = false;
            }
        }

        // Find and judge the job.
        if self.judged.is_none() && !self.gave_up {
            if self.late {
                // Past the search deadline with the job running on: look
                // again, slowly, until there is a judgement.
                let due = self
                    .last_search
                    .is_none_or(|t| now.saturating_sub(t) >= ms(LATE_SEARCH_EVERY));
                if due {
                    self.last_search = Some(now);
                    acts.push(Act::Search { wide: true });
                }
            } else if !past_search {
                let due = self
                    .last_search
                    .is_none_or(|t| now.saturating_sub(t) >= ms(SEARCH_EVERY));
                if due {
                    self.last_search = Some(now);
                    let wide = now.saturating_sub(since) >= ms(WIDE_SEARCH_AFTER)
                        && self
                            .last_wide
                            .is_none_or(|t| now.saturating_sub(t) >= ms(WIDE_SEARCH_EVERY));
                    if wide {
                        self.last_wide = Some(now);
                    }
                    acts.push(Act::Search { wide });
                }
            } else if self.hook_since.is_some() {
                // No verdict by the search deadline for a hook this watch has
                // seen. Under an allow outcome the hook will run the job at
                // its own deadline; faber has nothing to deliver that the hook
                // would accept.
                self.give_up(&mut acts);
            } else if let Some(passed) = self.passed {
                // No verdict by the search deadline, for a job that was past
                // its hook before this watch began. Its hook did not fail, so
                // this is no fault, and the job is not at its start, so there
                // is no hook to hand the deadline's outcome to.
                if passed.holds(on_bypass) {
                    // What faber decided before the restart stands for now,
                    // as it would have with no restart, and is said aloud.
                    // It stands in for a judgement only until there is one.
                    self.late = true;
                    self.last_search = Some(now);
                    acts.push(Act::Stands(passed));
                } else {
                    return self.stop_after_start(acts, StopWhy::DoesNotHold);
                }
            } else if self.unwatched() {
                // No verdict by the search deadline, for a job this watch did
                // not see start, with no record that its hook let it through.
                // Under an allow outcome the hook may have run it at its
                // deadline while nobody watched, so faber cannot show a hook
                // fault. On a host that records it runs on, unverified, and
                // faber goes on looking. On one that enforces it does not.
                match on_bypass {
                    OnBypass::Record => {
                        self.late = true;
                        self.last_search = Some(now);
                        acts.push(Act::NotWatched);
                    }
                    OnBypass::Enforce => return self.stop_after_start(acts, StopWhy::NotWatched),
                }
            } else {
                self.give_up(&mut acts);
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

        // A job this watch saw no hook on, once faber has judged it. The
        // judgement decides, not the record.
        if self.hook_since.is_none() {
            if let Some(decision) = self.judged.as_ref().map(|j| j.decision) {
                if self.passed.is_some() {
                    // It was past its hook before this watch began: no hook
                    // was seen because it had already let the job through,
                    // which faber's own record says. While the gate allows
                    // the job there is nothing left to watch. A job it now
                    // refuses is stopped at once: the record already
                    // explains the missing hook, so no hook is waited for.
                    return match decision {
                        StartDecision::Allow => {
                            if self.late {
                                acts.push(Act::Recovered);
                            }
                            self.ended = true;
                            acts
                        }
                        StartDecision::Refuse(_) => self.stop_after_start(acts, StopWhy::Refused),
                    };
                }
                if self.unwatched() && past_search {
                    // A job faber did not watch start. Until the search
                    // deadline its hook may still appear, and is then served.
                    return match (on_bypass, decision) {
                        (OnBypass::Record, StartDecision::Allow) => {
                            acts.push(if self.late {
                                Act::Recovered
                            } else {
                                Act::NotWatched
                            });
                            self.ended = true;
                            acts
                        }
                        (OnBypass::Record, StartDecision::Refuse(_)) | (OnBypass::Enforce, _) => {
                            self.stop_after_start(acts, StopWhy::NotWatched)
                        }
                    };
                }
            }
        }
        if self.late {
            // The job runs on, and the watch waits for a judgement.
            return acts;
        }

        // A job in progress with no waiting hook: never seen by the search
        // deadline, or gone with the worker still running and no allow given.
        let never_seen = self.hook_since.is_none() && past_search;
        if self.hook_gone && self.delivered.is_none() {
            let at_deadline = self.allowed_at_deadline(now, deadlines);
            // Under an allow outcome the hook lets the job run at its own
            // deadline. Where faber can neither see that the deadline has
            // come nor show that the hook left before it, it does not know
            // when the hook started: its first sight of it came late, after a
            // restart or a slow pass.
            let cannot_place = self.outcome == DeadlineOutcome::Allow
                && !at_deadline
                && !self.left_early(now, deadlines);
            if at_deadline || (cannot_place && on_bypass == OnBypass::Record) {
                // The job runs as planned, so there is nothing left to watch
                // and the bypass check judges it as before. Written down,
                // because a watch begun on this runner later sees only that
                // no hook is waiting.
                self.ended = true;
                acts.push(Act::RanUnverified);
                return acts;
            }
            if cannot_place {
                // On a host that enforces, an unverified job does not run on.
                return self.stop_after_start(acts, StopWhy::HookStartUnknown);
            }
        }
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

    /// The search gave up with no judgement: from here the plan's outcome at
    /// the deadline is the decision in force ([`Self::decision`]). Under an
    /// allow outcome that is said once.
    fn give_up(&mut self, acts: &mut Vec<Act>) {
        self.gave_up = true;
        if self.outcome == DeadlineOutcome::Allow && !self.recorded {
            self.recorded = true;
            acts.push(Act::Unverified);
        }
    }

    /// Stop a job that is past its start: `why` is recorded and the runner is
    /// killed until it is gone.
    fn stop_after_start(&mut self, mut acts: Vec<Act>, why: StopWhy) -> Vec<Act> {
        self.kill = true;
        acts.push(Act::StoppedAfterStart(why));
        acts.push(Act::Kill);
        acts
    }

    /// This pass saw the runner with no hook waiting, before any hook was
    /// seen on it.
    fn note_no_hook(&mut self, now: u64) {
        if self.hook_since.is_none() {
            self.hook_absent_at = Some(now);
        }
    }

    /// Whether this watch did not see its job start and has nothing that says
    /// the job's hook let it through, on a runner whose hook lets a job run
    /// at its deadline. faber can then show no hook fault: the hook may have
    /// waited and left while nobody watched. Under a refuse outcome the hook
    /// would have stopped the job, so a job still running there is one.
    fn unwatched(&self) -> bool {
        self.adopted
            && self.hook_since.is_none()
            && self.passed.is_none()
            && self.outcome == DeadlineOutcome::Allow
    }

    /// Whether faber can show that a hook it saw waiting, and now sees gone,
    /// left before its own deadline: this watch saw the runner with no hook
    /// waiting less than one hook deadline ago. A hook that started after
    /// that pass and waited out its deadline would be seen gone later than
    /// this.
    fn left_early(&self, now: u64, deadlines: &JobStartDeadlines) -> bool {
        let hook = u64::try_from(deadlines.hook.as_millis()).unwrap_or(u64::MAX);
        self.hook_absent_at
            .is_some_and(|t| now.saturating_sub(t) < hook)
    }

    /// Whether the hook's own deadline has passed under an allow outcome, so a
    /// hook that left without faber's verdict ran its job as the plan said.
    fn allowed_at_deadline(&self, now: u64, deadlines: &JobStartDeadlines) -> bool {
        // `hook_since` is when faber first saw the hook, never before the hook
        // started, so the hook's deadline can fall up to a grace earlier on it.
        let hook = u64::try_from(deadlines.hook.saturating_sub(HOOK_SEEN_GRACE).as_millis())
            .unwrap_or(u64::MAX);
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
            record_only: false,
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
            // None of these depends on the mode: an allow here is one in both,
            // and a refusal is no allow at all.
            for v in [
                Verdict::Provision,
                refuse(R::ThirdParty("a-stranger".into())),
                refuse(R::JobTokenDispatch),
                refuse(R::DisallowedEvent("release".into())),
            ] {
                assert!(!allowed_only_under_record(&v, mode), "{v:?} {mode:?}");
            }
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
                start_decision(&refuse(r.clone()), OnBypass::Record),
                StartDecision::Allow
            );
            // The one allow that only `record` gives.
            assert!(allowed_only_under_record(
                &refuse(r.clone()),
                OnBypass::Record
            ));
            assert!(!allowed_only_under_record(&refuse(r), OnBypass::Enforce));
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

    /// The run GitHub's two active listings leave out: after WIDE_SEARCH_AFTER
    /// of misses a search goes wide, then only every WIDE_SEARCH_EVERY, and
    /// the latest miss is kept for the fault.
    #[test]
    fn a_search_that_keeps_missing_goes_wide_on_its_own_cadence() {
        let mut w = watch(DeadlineOutcome::Refuse);
        let d = deadlines();
        let search = |w: &mut Watch, now: u64| {
            w.step(now, top(true, true), &d, OnBypass::Enforce)
                .into_iter()
                .find(|a| matches!(a, Act::Search { .. }))
        };
        assert_eq!(search(&mut w, 0), Some(Act::Search { wide: false }));
        assert_eq!(w.last_miss(), None);
        w.missed(Miss::NotListed);
        assert_eq!(search(&mut w, 16_000), Some(Act::Search { wide: false }));
        assert_eq!(search(&mut w, 20_000), Some(Act::Search { wide: true }));
        assert_eq!(search(&mut w, 24_000), Some(Act::Search { wide: false }));
        assert_eq!(search(&mut w, 36_000), Some(Act::Search { wide: false }));
        assert_eq!(search(&mut w, 40_000), Some(Act::Search { wide: true }));
        w.missed(Miss::Github("timed out".to_string()));
        assert_eq!(
            w.last_miss().map(Miss::words),
            Some("GitHub did not answer: timed out".to_string())
        );
    }

    #[test]
    fn an_allow_reaches_a_waiting_hook_and_ends_the_watch() {
        let mut w = watch(DeadlineOutcome::Refuse);
        let d = deadlines();
        assert_eq!(
            w.step(1_000, top(true, false), &d, OnBypass::Record),
            vec![Act::Search { wide: false }]
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
        // The hook leaves at its deadline and the job runs: not a fault, and
        // written down for a watch begun on this runner later.
        assert_eq!(
            w.step(91_000, top(true, false), &d, OnBypass::Record),
            vec![Act::RanUnverified]
        );
        assert!(w.ended());
    }

    #[test]
    fn a_hook_seen_late_that_leaves_on_time_is_not_a_fault() {
        let d = deadlines();
        let mut w = watch(DeadlineOutcome::Allow);
        // The worker is seen at 0; the hook, which started then, only at 1.5 s.
        w.step(0, top(true, false), &d, OnBypass::Record);
        w.step(1_500, top(true, true), &d, OnBypass::Record);
        // It leaves at its deadline (90 s after it started), 1 s early by
        // faber's clock.
        let acts = w.step(
            1_500 + 90_000 - 1_000,
            top(true, false),
            &d,
            OnBypass::Enforce,
        );
        assert!(
            !acts.iter().any(|a| matches!(a, Act::HookFault { .. })),
            "{acts:?}"
        );
        assert!(w.ended());
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

    // --- a watch begun mid-job (ADR-0011) ------------------------------------

    /// Each way faber records a job as past its hook: an allow the gate gives
    /// in either mode, one only `record` gives, and the hook's own deadline.
    /// The allows are for the run and attempt [`judged`] finds.
    fn past_its_hook() -> [Passed; 3] {
        let allowed = |record_only| Passed::Allowed {
            run_id: 70,
            run_attempt: 2,
            record_only,
        };
        let all = [allowed(false), allowed(true), Passed::Unverified];
        // A new way past the hook belongs in the list above, so the tests of
        // the class cover it: this match stops compiling until it is named.
        for p in all {
            match p {
                Passed::Allowed {
                    record_only: false, ..
                }
                | Passed::Allowed {
                    record_only: true, ..
                }
                | Passed::Unverified => {}
            }
        }
        all
    }

    fn adopted(outcome: DeadlineOutcome, passed: Option<Passed>) -> Watch {
        Watch::adopted("owner/demo", RUNNER, 5, 50, 0, outcome, passed)
    }

    /// Every act `w` asks for when it is begun on a job in mid-run: the worker
    /// running and no hook waiting, from its first pass until after the search
    /// deadline. `verdict` is what its first search finds, if anything.
    fn mid_job(w: &mut Watch, verdict: Option<StartDecision>, mode: OnBypass) -> Vec<Act> {
        let d = deadlines();
        let mut acts = w.step(0, top(true, false), &d, mode);
        if let Some(decision) = verdict {
            w.judge(judged(decision));
        }
        for now in [2_000, 60_000, 62_000] {
            acts.extend(w.step(now, top(true, false), &d, mode));
        }
        acts
    }

    const NEVER_SEEN: &str = "no job-started hook was seen waiting";

    /// The class, not one path: any watch begun on a runner whose job faber
    /// recorded as past its hook, however it got past. Its worker runs and no
    /// hook waits, because the hook already let the job through. While the
    /// gate still allows the job that is no fault, and nothing is stopped, in
    /// either mode and under either deadline outcome.
    #[test]
    fn a_watch_begun_on_a_job_already_past_its_hook_is_not_a_hook_fault() {
        for passed in past_its_hook() {
            for mode in [OnBypass::Record, OnBypass::Enforce] {
                for outcome in [DeadlineOutcome::Allow, DeadlineOutcome::Refuse] {
                    let mut w = adopted(outcome, Some(passed));
                    let acts = mid_job(&mut w, Some(StartDecision::Allow), mode);
                    // It judged the job again, and that is all it did.
                    assert_eq!(
                        acts,
                        vec![Act::Search { wide: false }],
                        "{passed:?} {mode:?} {outcome:?}"
                    );
                    assert!(w.ended(), "{passed:?} {mode:?} {outcome:?}");
                }
            }
        }
    }

    /// Where faber can judge the job again, its judgement decides and the
    /// record does not. A job the gate now refuses is stopped in both modes,
    /// whatever was recorded of it. It is stopped at once, on the pass that
    /// has the judgement and not at the search deadline, because the record
    /// already explains the missing hook. It is not a hook fault: by faber's
    /// own record its hook did its work.
    #[test]
    fn a_judgement_decides_whatever_the_record_says() {
        let d = deadlines();
        for passed in past_its_hook() {
            for mode in [OnBypass::Record, OnBypass::Enforce] {
                for outcome in [DeadlineOutcome::Allow, DeadlineOutcome::Refuse] {
                    let case = format!("{passed:?} {mode:?} {outcome:?}");
                    let mut w = adopted(outcome, Some(passed));
                    assert_eq!(
                        w.step(0, top(true, false), &d, mode),
                        vec![Act::Search { wide: false }],
                        "{case}"
                    );
                    w.judge(judged(StartDecision::Refuse(Reason::Author)));
                    assert_eq!(
                        w.step(2_000, top(true, false), &d, mode),
                        vec![Act::StoppedAfterStart(StopWhy::Refused), Act::Kill],
                        "{case}"
                    );
                    assert!(w.stopping(), "{case}");
                    // Asked again until the container is gone, and no more.
                    assert_eq!(
                        w.step(60_000, top(true, false), &d, mode),
                        vec![Act::Kill],
                        "{case}"
                    );
                    w.step(62_000, Seen::Gone, &d, mode);
                    assert!(w.ended(), "{case}");
                }
            }
        }
    }

    /// An allow faber recorded for another run or attempt than the job found
    /// on the runner is not that job's. From the judgement on, the job has no
    /// record, and is handled as any job with none.
    #[test]
    fn a_record_for_another_run_or_attempt_is_no_record_of_this_job() {
        for (run_id, run_attempt) in [(71, 2), (70, 1)] {
            let other = Passed::Allowed {
                run_id,
                run_attempt,
                record_only: false,
            };
            // Where the hook would have stopped the job: a hook fault.
            let mut w = adopted(DeadlineOutcome::Refuse, Some(other));
            let mut none = adopted(DeadlineOutcome::Refuse, None);
            let acts = mid_job(&mut w, Some(StartDecision::Allow), OnBypass::Enforce);
            assert_eq!(
                acts,
                mid_job(&mut none, Some(StartDecision::Allow), OnBypass::Enforce),
                "{other:?}"
            );
            assert!(
                acts.contains(&Act::HookFault {
                    stop: true,
                    why: NEVER_SEEN
                }) && acts.contains(&Act::Kill),
                "{other:?}: {acts:?}"
            );
            assert_eq!(w.passed(), None, "{other:?}");

            // A refusal is not stopped at once on the strength of it either.
            let mut w = adopted(DeadlineOutcome::Refuse, Some(other));
            let refuse = Some(StartDecision::Refuse(Reason::Author));
            let acts = mid_job(&mut w, refuse, OnBypass::Enforce);
            assert!(
                !acts
                    .iter()
                    .any(|a| matches!(a, Act::StoppedAfterStart(StopWhy::Refused))),
                "{other:?}: {acts:?}"
            );

            // Where the hook lets a job run at its deadline: not watched.
            let mut w = adopted(DeadlineOutcome::Allow, Some(other));
            assert_eq!(
                mid_job(&mut w, Some(StartDecision::Allow), OnBypass::Record),
                vec![Act::Search { wide: false }, Act::NotWatched],
                "{other:?}"
            );
        }
        // A record that stood for a while is dropped the same way once the
        // judgement comes, and is not closed as if it had been right.
        let d = deadlines();
        let other = Passed::Allowed {
            run_id: 71,
            run_attempt: 2,
            record_only: false,
        };
        let mut w = adopted(DeadlineOutcome::Allow, Some(other));
        let acts = mid_job(&mut w, None, OnBypass::Record);
        assert_eq!(acts.last(), Some(&Act::Stands(other)), "{acts:?}");
        w.judge(judged(StartDecision::Allow));
        assert_eq!(
            w.step(64_000, top(true, false), &d, OnBypass::Record),
            vec![Act::NotWatched]
        );
        assert!(w.ended() && w.passed().is_none());
    }

    /// A restart while GitHub does not answer: the watch begun on a job that
    /// was past its hook cannot judge it again by the search deadline. What
    /// faber recorded before the restart stands for now, as far as ADR-0059 §5
    /// would still decide it under the mode in force: no fault, nothing
    /// stopped, and it is said aloud. The watch stays and looks again, and
    /// the judgement decides when it comes. A record that does not hold under
    /// `enforce` stops the job at the deadline, and that is no hook fault.
    #[test]
    fn what_faber_recorded_stands_until_it_can_judge_again() {
        let [either_mode, record_only, unverified] = past_its_hook();
        for (passed, under_record, under_enforce) in [
            (either_mode, true, true),
            // A trust refusal `record` let through, and a job with no verdict:
            // `enforce` refuses both at start.
            (record_only, true, false),
            (unverified, true, false),
        ] {
            assert_eq!(passed.holds(OnBypass::Record), under_record, "{passed:?}");
            assert_eq!(passed.holds(OnBypass::Enforce), under_enforce, "{passed:?}");
        }
        let d = deadlines();
        for passed in past_its_hook() {
            for mode in [OnBypass::Record, OnBypass::Enforce] {
                for outcome in [DeadlineOutcome::Allow, DeadlineOutcome::Refuse] {
                    let case = format!("{passed:?} {mode:?} {outcome:?}");
                    let mut w = adopted(outcome, Some(passed));
                    let acts = mid_job(&mut w, None, mode);
                    if !passed.holds(mode) {
                        assert_eq!(
                            acts,
                            vec![
                                Act::Search { wide: false },
                                Act::StoppedAfterStart(StopWhy::DoesNotHold),
                                Act::Kill,
                                Act::Kill,
                            ],
                            "{case}"
                        );
                        assert!(w.stopping(), "{case}");
                        continue;
                    }
                    // It looked for the job until the deadline, then let the
                    // record stand, and stayed.
                    assert_eq!(
                        acts,
                        vec![Act::Search { wide: false }, Act::Stands(passed)],
                        "{case}"
                    );
                    assert!(w.late() && !w.ended() && !w.stopping(), "{case}");

                    // A later judgement that allows closes it.
                    let mut allowed = w.clone();
                    allowed.judge(judged(StartDecision::Allow));
                    assert_eq!(
                        allowed.step(64_000, top(true, false), &d, mode),
                        vec![Act::Recovered],
                        "{case}"
                    );
                    assert!(allowed.ended(), "{case}");

                    // One that refuses stops the job, at once.
                    let mut refused = w.clone();
                    refused.judge(judged(StartDecision::Refuse(Reason::Author)));
                    assert_eq!(
                        refused.step(64_000, top(true, false), &d, mode),
                        vec![Act::StoppedAfterStart(StopWhy::Refused), Act::Kill],
                        "{case}"
                    );

                    // And a job that ends first ends the watch.
                    let mut done = w.clone();
                    assert!(done.step(64_000, top(false, false), &d, mode).is_empty());
                    assert!(done.ended(), "{case}");
                }
            }
        }
    }

    /// A watch that lets its job run without a fresh judgement looks again
    /// once a minute, with a wide search, for as long as there is none.
    #[test]
    fn a_watch_that_stands_looks_again_once_a_minute() {
        let d = deadlines();
        let [allowed, _, _] = past_its_hook();
        for (mut w, said) in [
            (
                adopted(DeadlineOutcome::Refuse, Some(allowed)),
                Act::Stands(allowed),
            ),
            (adopted(DeadlineOutcome::Allow, None), Act::NotWatched),
        ] {
            let acts = mid_job(&mut w, None, OnBypass::Record);
            assert_eq!(acts, vec![Act::Search { wide: false }, said.clone()]);
            let mut looks = Vec::new();
            for now in (62_000..=300_000).step_by(2_000) {
                let acts = w.step(now, top(true, false), &d, OnBypass::Record);
                assert!(
                    acts.iter().all(|a| *a == Act::Search { wide: true }),
                    "{said:?} at {now}: {acts:?}"
                );
                if !acts.is_empty() {
                    looks.push(now);
                }
            }
            // The deadline pass was at 60 s.
            assert_eq!(looks, vec![120_000, 180_000, 240_000, 300_000], "{said:?}");
            assert!(w.late() && !w.ended());
        }
    }

    /// Where the hook would have stopped the job at its deadline, a job in
    /// progress with no waiting hook is a hook fault on an adopted runner
    /// too, and is stopped under `enforce`: the hook never ran, or was killed.
    /// There a watch begun on a running runner with no record behaves, act
    /// for act, as one begun at the mint.
    #[test]
    fn a_job_whose_hook_would_have_stopped_it_is_a_hook_fault_on_an_adopted_runner_too() {
        let mut w = adopted(DeadlineOutcome::Refuse, None);
        assert_eq!(
            mid_job(&mut w, Some(StartDecision::Allow), OnBypass::Enforce),
            vec![
                Act::Search { wide: false },
                Act::HookFault {
                    stop: true,
                    why: NEVER_SEEN
                },
                Act::Kill,
                // Asked again until the container is gone.
                Act::Kill,
            ]
        );
        for mode in [OnBypass::Record, OnBypass::Enforce] {
            for verdict in [
                None,
                Some(StartDecision::Allow),
                Some(StartDecision::Refuse(Reason::Author)),
            ] {
                let mut w = adopted(DeadlineOutcome::Refuse, None);
                let mut at_mint = watch(DeadlineOutcome::Refuse);
                assert_eq!(
                    mid_job(&mut w, verdict, mode),
                    mid_job(&mut at_mint, verdict, mode),
                    "{mode:?} {verdict:?}"
                );
            }
        }
    }

    /// Under an allow outcome the hook lets its job run at its own deadline.
    /// A watch begun on a running runner, with no hook in sight and no record,
    /// cannot tell a job whose hook never ran from one whose hook did that
    /// while nobody watched. So it claims no hook fault. On a host that
    /// records, the job runs on as not watched, and is judged when it can be.
    /// On one that enforces it is stopped. A watch that saw the job start
    /// claims the fault, as before.
    #[test]
    fn a_job_faber_did_not_watch_start_is_not_a_hook_fault_under_an_allow_outcome() {
        let d = deadlines();
        let refuse = StartDecision::Refuse(Reason::Author);
        let claims_a_fault = |acts: &[Act]| {
            acts.iter()
                .any(|a| matches!(a, Act::HookFault { .. } | Act::Unverified))
        };

        // On a host that records.
        let mut w = adopted(DeadlineOutcome::Allow, None);
        assert_eq!(
            mid_job(&mut w, Some(StartDecision::Allow), OnBypass::Record),
            vec![Act::Search { wide: false }, Act::NotWatched]
        );
        assert!(w.ended());

        let mut w = adopted(DeadlineOutcome::Allow, None);
        assert_eq!(
            mid_job(&mut w, Some(refuse), OnBypass::Record),
            vec![
                Act::Search { wide: false },
                // At the search deadline, not before: until then its hook may
                // still appear, and is then given the refusal.
                Act::StoppedAfterStart(StopWhy::NotWatched),
                Act::Kill,
                Act::Kill,
            ]
        );

        let mut w = adopted(DeadlineOutcome::Allow, None);
        assert_eq!(
            mid_job(&mut w, None, OnBypass::Record),
            vec![Act::Search { wide: false }, Act::NotWatched]
        );
        assert!(w.late() && !w.ended());
        let mut allowed = w.clone();
        allowed.judge(judged(StartDecision::Allow));
        assert_eq!(
            allowed.step(64_000, top(true, false), &d, OnBypass::Record),
            vec![Act::Recovered]
        );
        assert!(allowed.ended());
        w.judge(judged(refuse));
        assert_eq!(
            w.step(64_000, top(true, false), &d, OnBypass::Record),
            vec![Act::StoppedAfterStart(StopWhy::NotWatched), Act::Kill]
        );

        // On a host that enforces, whatever faber makes of the job.
        for verdict in [None, Some(StartDecision::Allow), Some(refuse)] {
            let mut w = adopted(DeadlineOutcome::Allow, None);
            let acts = mid_job(&mut w, verdict, OnBypass::Enforce);
            assert_eq!(
                acts,
                vec![
                    Act::Search { wide: false },
                    Act::StoppedAfterStart(StopWhy::NotWatched),
                    Act::Kill,
                    Act::Kill,
                ],
                "{verdict:?}"
            );
            assert!(!claims_a_fault(&acts), "{verdict:?}");
        }

        // A hook that does appear before the deadline is served.
        let mut w = adopted(DeadlineOutcome::Allow, None);
        w.step(0, top(true, false), &d, OnBypass::Record);
        w.judge(judged(refuse));
        assert!(w
            .step(2_000, top(true, false), &d, OnBypass::Record)
            .is_empty());
        assert_eq!(
            w.step(4_000, top(true, true), &d, OnBypass::Record),
            vec![
                Act::Deliver(Delivery {
                    decision: refuse,
                    run_id: 70,
                    run_attempt: 2
                }),
                Act::RefusedAtStart(Reason::Author),
                Act::Kill
            ]
        );

        // The control: a watch that saw the job start saw that no hook came.
        let mut at_mint = watch(DeadlineOutcome::Allow);
        assert_eq!(
            mid_job(&mut at_mint, Some(StartDecision::Allow), OnBypass::Record),
            vec![
                Act::Search { wide: false },
                Act::HookFault {
                    stop: false,
                    why: NEVER_SEEN
                }
            ]
        );
    }

    const LEFT_EARLY: &str = "the job-started hook exited without a verdict and the job ran on";

    /// A hook that leaves with the job running and no verdict of faber's is a
    /// hook fault only where faber can show it left before its own deadline:
    /// it saw the runner with no hook waiting less than one hook deadline
    /// ago. A first sight that comes late, after a slow pass or a restart,
    /// leaves the hook's start unknown, and then faber does not claim one.
    #[test]
    fn a_hook_that_leaves_is_a_fault_only_where_faber_can_show_it_left_early() {
        let d = deadlines();
        let fault = |stop| Act::HookFault {
            stop,
            why: LEFT_EARLY,
        };
        // What a watch asks for on a pass that sees the worker and no hook,
        // besides what belongs to its search: the searches it still makes
        // while it has no judgement, and the note that its search gave up.
        let gone = |w: &mut Watch, now: u64, mode| -> Vec<Act> {
            w.step(now, top(true, false), &d, mode)
                .into_iter()
                .filter(|a| !matches!(a, Act::Search { .. } | Act::Unverified))
                .collect()
        };
        // The hook's deadline is 90 s. A watch begun at the mint sees the
        // runner before its container runs, at 0, and its hook at `first`.
        let seen_at = |first: u64, mode| {
            let mut w = watch(DeadlineOutcome::Allow);
            w.step(0, Seen::Gone, &d, mode);
            w.step(first, top(true, true), &d, mode);
            w
        };
        for mode in [OnBypass::Record, OnBypass::Enforce] {
            // Seen from its start, and gone a minute later: it left early.
            let mut w = seen_at(2_000, mode);
            assert_eq!(
                gone(&mut w, 60_000, mode),
                vec![fault(mode == OnBypass::Enforce)]
                    .into_iter()
                    .chain((mode == OnBypass::Enforce).then_some(Act::Kill))
                    .collect::<Vec<_>>(),
                "{mode:?}"
            );

            // Seen for its whole deadline: it ran the job as planned.
            let mut w = seen_at(2_000, mode);
            assert_eq!(
                gone(&mut w, 90_000, mode),
                vec![Act::RanUnverified],
                "{mode:?}"
            );

            // First seen 20 s late, after a slow pass, and gone at its own
            // deadline. By faber's sight it waited 72 s, and by what faber
            // can show it may have waited 92: faber cannot place its start.
            let mut w = seen_at(20_000, mode);
            let acts = gone(&mut w, 92_000, mode);
            match mode {
                OnBypass::Record => assert_eq!(acts, vec![Act::RanUnverified]),
                OnBypass::Enforce => assert_eq!(
                    acts,
                    vec![Act::StoppedAfterStart(StopWhy::HookStartUnknown), Act::Kill]
                ),
            }

            // The same late first sight, and gone at 60 s: it cannot have
            // waited 90, so it left early.
            let mut w = seen_at(20_000, mode);
            assert!(
                gone(&mut w, 60_000, mode).contains(&fault(mode == OnBypass::Enforce)),
                "{mode:?}"
            );

            // A pass that could not read the container shows nothing.
            let mut w = watch(DeadlineOutcome::Allow);
            w.step(0, Seen::Unknown, &d, mode);
            w.step(20_000, top(true, true), &d, mode);
            let acts = gone(&mut w, 60_000, mode);
            assert!(
                !acts.contains(&fault(mode == OnBypass::Enforce)),
                "{acts:?}"
            );

            // A watch begun on a running runner whose hook is already
            // waiting never saw the runner without it.
            let mut w = adopted(DeadlineOutcome::Allow, None);
            w.step(0, top(true, true), &d, mode);
            let acts = gone(&mut w, 30_000, mode);
            match mode {
                OnBypass::Record => assert_eq!(acts, vec![Act::RanUnverified]),
                OnBypass::Enforce => assert_eq!(
                    acts,
                    vec![Act::StoppedAfterStart(StopWhy::HookStartUnknown), Act::Kill]
                ),
            }

            // One that saw the job start, hook and all, can show it.
            let mut w = adopted(DeadlineOutcome::Allow, None);
            w.step(0, top(true, false), &d, mode);
            w.step(2_000, top(true, true), &d, mode);
            assert!(
                gone(&mut w, 30_000, mode).contains(&fault(mode == OnBypass::Enforce)),
                "{mode:?}"
            );

            // Under a refuse outcome the hook stops the job at its deadline,
            // so one that left the job running is a fault whenever it
            // started.
            let mut w = adopted(DeadlineOutcome::Refuse, None);
            w.step(0, top(true, true), &d, mode);
            assert!(
                gone(&mut w, 30_000, mode).contains(&fault(mode == OnBypass::Enforce)),
                "{mode:?}"
            );
        }
    }

    /// A hook this watch sees waiting is served as on any watch, whatever the
    /// record says: the record only ever accounts for a hook that is not there.
    #[test]
    fn a_hook_seen_waiting_is_served_whatever_the_record_says() {
        let d = deadlines();
        for passed in past_its_hook() {
            let mut w = adopted(DeadlineOutcome::Refuse, Some(passed));
            w.step(0, top(true, true), &d, OnBypass::Enforce);
            w.judge(judged(StartDecision::Allow));
            assert_eq!(
                w.step(2_000, top(true, true), &d, OnBypass::Enforce),
                vec![Act::Deliver(Delivery {
                    decision: StartDecision::Allow,
                    run_id: 70,
                    run_attempt: 2
                })],
                "{passed:?}"
            );

            let mut w = adopted(DeadlineOutcome::Refuse, Some(passed));
            w.step(0, top(true, true), &d, OnBypass::Enforce);
            w.judge(judged(StartDecision::Refuse(Reason::Actor)));
            assert_eq!(
                w.step(2_000, top(true, true), &d, OnBypass::Enforce),
                vec![
                    Act::Deliver(Delivery {
                        decision: StartDecision::Refuse(Reason::Actor),
                        run_id: 70,
                        run_attempt: 2
                    }),
                    Act::RefusedAtStart(Reason::Actor),
                    Act::Kill
                ],
                "{passed:?}"
            );

            // A hook seen waiting that leaves with no verdict while the worker
            // runs on is a fault, record or not.
            let mut w = adopted(DeadlineOutcome::Refuse, Some(passed));
            w.step(0, top(true, true), &d, OnBypass::Enforce);
            let acts = w.step(5_000, top(true, false), &d, OnBypass::Enforce);
            assert!(
                acts.iter()
                    .any(|a| matches!(a, Act::HookFault { stop: true, .. })),
                "{passed:?}: {acts:?}"
            );

            // No verdict by the search deadline while a hook waits: the hook
            // gets the plan's outcome, as on any watch. A record stands in for
            // a judgement only where no hook was seen, and under `record`
            // every one of these would otherwise hold.
            let mut w = adopted(DeadlineOutcome::Refuse, Some(passed));
            w.step(0, top(true, true), &d, OnBypass::Record);
            assert_eq!(
                w.step(60_000, top(true, true), &d, OnBypass::Record),
                vec![
                    Act::Deliver(Delivery {
                        decision: StartDecision::Refuse(Reason::NoVerdict),
                        run_id: 0,
                        run_attempt: 0
                    }),
                    Act::RefusedAtStart(Reason::NoVerdict),
                    Act::Kill
                ],
                "{passed:?}"
            );
        }
    }

    /// Only an allow that landed lets a job past its hook. Its record says
    /// whether the allow was one only `record` gives, and reads back as it was
    /// written. A line that does not say which run an allow was for, or says
    /// anything else, is no record.
    #[test]
    fn a_passed_record_says_what_let_the_job_through_and_reads_back_whole() {
        let allow = Delivery {
            decision: StartDecision::Allow,
            run_id: 70,
            run_attempt: 2,
        };
        let refuse = Delivery {
            decision: StartDecision::Refuse(Reason::Actor),
            ..allow
        };
        let allowed = |record_only| {
            Some(Passed::Allowed {
                run_id: 70,
                run_attempt: 2,
                record_only,
            })
        };
        for record_only in [false, true] {
            let mut w = watch(DeadlineOutcome::Allow);
            w.judge(Judged {
                record_only,
                ..judged(StartDecision::Allow)
            });
            assert_eq!(w.passed_by(&allow), allowed(record_only));
            assert_eq!(w.passed_by(&refuse), None);
        }
        // An allow with no judgement behind it is not taken to hold under
        // `enforce`.
        assert_eq!(
            watch(DeadlineOutcome::Allow).passed_by(&allow),
            allowed(true)
        );

        assert_eq!(
            past_its_hook().map(Passed::word),
            ["allowed", "allowed_under_record", "unverified"]
        );
        for p in past_its_hook() {
            assert_eq!(Passed::read(p.word(), p.run()), Some(p));
        }
        for word in ["allowed", "allowed_under_record"] {
            assert_eq!(Passed::read(word, None), None, "{word}");
        }
        assert_eq!(Passed::read("refused", Some((70, 2))), None);
        assert_eq!(Passed::read("", None), None);
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
