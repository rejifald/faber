//! The reconcile loop — faber's autonomous orchestrator.
//!
//! Each cycle: discover opted-in repos (by topic), poll their `queued` *and*
//! `in_progress` runs, join the jobs, run each through the [`gate`], and
//! provision a JIT runner for every job the gate approves — capped by a
//! concurrency limit, deduped by the stable `job.id`, each runner in its own
//! working copy.
//!
//! The [`Orchestrator`] holds the shared, observable state (counts, recent
//! decisions, in-flight runners) that the web dashboard reads and the controls
//! (pause / resume / force-reconcile) it drives. The pause flag itself is the
//! limiter's, so admission reads the same one (ADR-0023).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{watch, Mutex, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::audit::{now_ms, Audit, OutcomeReplay, Record};
use crate::authorship::{self, Deferred};
use crate::backend::{match_profile, Backend, Profile};
use crate::config::{
    AutoLimitConfig, DriftPolicy, Hold, HostPolicy, ProvisioningMode, RepoHold, RetryPolicy,
    RunnerImagePolicy, ToolNeeds, WasteMode,
};
use crate::credential::{Feature, Mismatches, Permission};
use crate::docker::{ClaimMarks, ImagePresence, Limits, RunReport};
use crate::drift::{self, DriftOutcome, SkipCause};
use crate::footprint::{Footprint, JobTimes, Timeline};
use crate::gate::{
    self, BypassClass, Code, JobContext, OnBypass, RefuseReason, SkipReason, Verdict,
};
use crate::github::{Github, GithubError, Job, Runner, Step, WorkflowRun};
use crate::host;
use crate::image::{VariantCache, VariantDisk};
use crate::instance::{self, PeerView, Peers};
use crate::limiter::{
    class_key, fmt_cores, Limiter, LimiterView, Pause, Rank, Reservation, VmRun, WaitView,
};
use crate::needs;
use crate::parking::{Parked, Parking, NOT_PARKED};
use crate::profile_image::{ProfileImages, Wait as ImageWait};
use crate::retry::{
    self, EndKind, EndLedger, Ending, OutcomeFacts, RetryMemory, RetryMode, RetryVerdict,
};
use crate::runner_retirement::{self, Candidate, Change, Retirements};

/// Run-level statuses that may hide a job still waiting for a runner. A run goes
/// `in_progress` the moment *any* job starts, so a self-hosted job queued under a
/// running multi-job run is only found by scanning both.
const ACTIVE_STATUSES: [&str; 2] = ["queued", "in_progress"];

/// How many recent decisions the dashboard keeps in memory.
const RECENT_CAP: usize = 50;

/// How many recent drift events the dashboard keeps in memory. Small: drift is
/// rare, and only actionable outcomes (a mismatch or an indeterminate read) land
/// here — the silent in-sync / no-label common case never does.
const DRIFT_RECENT_CAP: usize = 25;

/// How many recent auto-retry events the dashboard keeps in memory (ADR-0003).
const RETRY_RECENT_CAP: usize = 25;

/// How many finished runs the dashboard keeps in memory. Deeper than the decision
/// feed: a decision is one line of a job's story and an outcome is the ending, and
/// the ending is what a reader is looking for.
const OUTCOME_RECENT_CAP: usize = 50;

/// How many faber-level faults the dashboard keeps in memory. Small — these are
/// transition-gated, so an outage costs two entries however long it lasts.
const FAULT_RECENT_CAP: usize = 25;

// --- gate bypasses (ADR-0030) --------------------------------------------------
//
// A JIT runner is not pinned to the job it was minted for, so GitHub can hand it a
// job faber's gate refused. faber cannot prevent that; it can see it. Every cycle
// the jobs that ran on a runner *this host* minted are re-judged by the gate's trust
// checks, and a refusal there is a gate bypass: recorded always, and answered under
// `[gate] on_bypass = "enforce"` when it is a trust bypass.

/// How far back "trust bypasses seen recently" reaches — the window behind the
/// boot warning and the dashboard line while enforcement is off (ADR-0030), and
/// how long a bypassed job is remembered so it is recorded once.
const BYPASS_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How many recent bypasses the dashboard keeps in memory.
const BYPASS_RECENT_CAP: usize = 25;

/// How often a claim parked behind a repo hold or a refused job re-checks. The
/// check is lock-guarded reads with no I/O, as for the pool hold.
const REPO_BLOCK_POLL: Duration = Duration::from_secs(5);

/// Who a hold set by gate-bypass enforcement names as having set it. Also how a
/// restart tells such a hold from any other, to finish deleting its repo's cache.
pub const BYPASS_HOLD_BY: &str = "faber: gate bypass enforcement (ADR-0030)";

// --- provision-retry throttle (bug-002) --------------------------------------
//
// A job stuck `queued` — its runner never consumes it, whether from a persistent
// Docker failure, the documented stale-runner-version case (`run_one` returns Ok
// but the runner self-updates and exits without running the job), or a GitHub-side
// block (a busy concurrency group, an unmet `needs:` dep, or a not-job-pinned JIT
// runner whose slot keeps being consumed by sibling jobs) — would otherwise be
// re-provisioned every poll cycle forever, minting a fresh GitHub JIT runner
// registration and churning Docker each time. These bound that storm *without*
// ever abandoning the job: fast retries are capped, after which faber drops to a
// slow self-heal cadence (see [`PROVISION_MAX_ATTEMPTS`]).

/// Provision attempts allowed with no backoff, so recovery from a *transient*
/// runner death stays immediate (the first re-provision must not be delayed).
const PROVISION_GRACE_ATTEMPTS: u32 = 2;
/// Base delay once the grace is spent; doubles each further attempt.
const PROVISION_BACKOFF_BASE_MS: u64 = 30_000;
/// Ceiling the exponential backoff is clamped to. Once the fast-attempt cap is
/// spent this doubles as the *self-heal* cadence: a still-stuck job keeps being
/// re-provisioned, but only once per this interval (~5 min), not every cycle.
const PROVISION_BACKOFF_CEILING_MS: u64 = 300_000;
/// Fast-retry attempts (grace + exponential backoff) before faber throttles a
/// still-`queued` job down to the slow self-heal cadence
/// ([`PROVISION_BACKOFF_CEILING_MS`]). Past this cap faber does NOT give up — it
/// keeps re-provisioning once per ceiling window, so a GitHub-side block clearing
/// (or a runner slot freeing) is served with no human intervention. A job that
/// actually finishes leaves the queued/in-progress set and is pruned, which stops
/// the slow retry on its own. Bounds the fast registration storm (bug-002 defect
/// B: the old terminal "give up and leave it queued for a human" stranded jobs).
const PROVISION_MAX_ATTEMPTS: u32 = 8;
/// Consecutive reconcile cycles a tracked job may go unobserved before its per-job
/// state ([`JobState`]: throttle + record-once dedup) is pruned. `seen` is built
/// only from repos whose fetch fully succeeded, so a single transient
/// `runs_with_status` / `run_jobs` / `list_runners` error — or GitHub
/// eventual-consistency briefly returning a run with zero jobs (no error at all) —
/// drops a still-live job from `seen` for one cycle without meaning it is gone.
/// Pruning on that blip (bug-002 defect A) would reset `provision_attempts` to
/// zero and silently defeat the storm cap. Tolerating a few misses holds the cap
/// across discovery gaps while keeping the map bounded — a job genuinely gone is
/// forgotten `PRUNE_MISS_GRACE + 1` cycles later.
const PRUNE_MISS_GRACE: u32 = 3;

// --- claim reconciliation (issues #1, #16) ------------------------------------
//
// A claim's `inflight` entry was historically released only when its own
// provision task's runner exited. Nothing reconciled it against GitHub, so a job
// that finished, was cancelled, or was served (under not-job-pinned assignment) by
// a sibling left its claim stranded — lying on the dashboard and, once a parked
// task admitted, spawning an idle runner that held a memory reservation until the
// 3 h lifetime cap. `reconcile_claims` closes that gap each cycle.
//
// Issue #1's fix reconciled a *parked* claim against its job but an *admitted* one
// against its runner's `busy` flag, on the reasoning that a not-job-pinned runner
// may be serving a sibling. That made "absent" unreclaimable — a JIT runner
// deregisters when it exits, so the claim of a job that had finished long ago read
// as "not idle" and was kept forever. Seen live in July 2026: most admitted claims
// were doing no work, against a fully committed memory budget with about twenty
// jobs parked waiting for memory (issue #16). `admitted_verdict` now weighs the
// job, the runner and the container together — the runner's `busy` flag demoted
// from the decision to a veto over it.

/// How long an *admitted* claim must be continuously observed doing no work
/// before faber reclaims it (see [`admitted_verdict`]). A healthy JIT runner
/// picks up its job within seconds of registering, so a claim that is confirmed
/// idle — or whose job GitHub has already finished — for this long will never do
/// useful work. The window must elapse over *confirmed* observations: any
/// unreadable signal clears it, so an API blip can never age a claim into a
/// reclaim (which is what turns a reclaim into a re-provisioning storm).
const IDLE_RECLAIM_GRACE: Duration = Duration::from_secs(120);

/// How often [`Orchestrator::await_drain`] re-checks while it waits. The drain is
/// woken by every claim release, so this is only a backstop for a claim whose
/// runner container goes away *during* the drain without its task noticing (a
/// wedged `docker` client) — without it, that claim would hold the drain open
/// until launchd's `ExitTimeOut` turned a graceful stop into a SIGKILL.
const DRAIN_RECHECK: Duration = Duration::from_secs(15);

// --- learning attribution ----------------------------------------------------
//
// A JIT runner is *not* job-pinned: faber mints it for a specific job, but GitHub
// dispatches whichever queued job it likes to the freshly-registered runner. The
// names come apart routinely — observed live, consecutive runs executed a job
// other than the one their runner was named for. The autolimiter used to feed
// each run's measured memory into the class of the job the runner was *minted*
// for, so a lint class absorbed an E2E job's footprint and learned caps drifted far
// above anything those jobs really use (a small job learned a runner peak of many
// GiB). Inflated caps then trip the escalation guard and exhaust the
// budget, which is what parks every other claim. Attribution therefore keys on
// GitHub's own `job.runner_name`, harvested each discovery cycle.

/// The job GitHub actually dispatched to one of faber's runners — enough to build
/// the autolimiter's class key and human label for it.
#[derive(Debug, Clone)]
struct ExecutedJob {
    job_id: u64,
    workflow_id: u64,
    workflow_name: Option<String>,
    job_name: String,
    /// When this mapping was harvested, so the map can be bounded by age.
    seen_ms: u64,
}

/// How long a runner→job mapping is kept. Only needs to outlive one job (a runner
/// takes exactly one), with slack for a long job plus its teardown.
const EXECUTED_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// How many recently changed runs a direct runner→job lookup reads the jobs of,
/// so a busy repo bounds what one miss costs in API calls.
const EXECUTED_LOOKUP_RUNS: usize = 10;

/// Clock slack on a direct lookup's "changed since admission" bound: GitHub's
/// `updated_at` and this host's clock need not agree to the second.
const EXECUTED_LOOKUP_SLACK: Duration = Duration::from_secs(60);

/// How long a class stays in a repo's sizing cohort after it was last seen queued.
///
/// Long enough to cover this repo's slowest cadence — a weekly workflow shares the
/// same runner pool as every push — because the cost is asymmetric: an over-sized
/// container is a cgroup ceiling nothing reaches, while an under-sized one is a
/// killed job. See [`merge_cohorts`].
const COHORT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

// --- stale offline-runner supply + sweep -------------------------------------
//
// A JIT runner faber mints registers, comes `online`, runs its one job, and
// deregisters. If its container dies (or never starts) the registration lingers
// `offline` forever while GitHub keeps its name (`faber-{job_id}-{ts_ms}`). Demand
// provisioning once counted any such offline runner whose namesake job was still
// queued as "connecting supply", so a dead runner masked its own still-queued job
// indefinitely — demand minted nothing and the job never ran (a self-reinforcing
// deadlock: the job stays queued *because* nothing replaced the dead runner, which
// keeps the job live, which keeps crediting the dead runner). The mint timestamp in
// the name bounds both the credit window and the reap.
//
// Both windows count from that timestamp, so it has to be the moment the runner
// registers. It used to be read when the provision task started, before a claim can
// park for image resolution, the pool hold and memory admission (many minutes on a
// busy host). A runner whose claim had parked registered already older than both
// windows: demand read it as dead rather than connecting and minted one runner too
// many, and a sweep deleted its registration before the listener connected. The name
// is now minted at admission, by `admit_claim` (ADR-0018).

/// How long a freshly-minted runner may sit `offline` and still count as
/// connecting supply. A healthy JIT runner registers and comes online within
/// seconds; past this it has failed to start and is dead, not connecting, so it
/// must stop masking its still-queued job (see [`runner_is_supply`]). Mirrors
/// [`IDLE_RECLAIM_GRACE`] — the same "a live runner acts within seconds" bound.
///
/// Counted from registration. What follows it is the sandbox ramp: from admission
/// to the runner container launching took about a minute at worst across thousands
/// of provisions, so a runner still offline at this age is not coming.
const CONNECTING_GRACE: Duration = Duration::from_secs(120);

/// How long a faber runner must sit `offline` before the reconcile loop
/// deregisters it as a dead orphan ([`stale_runner_ids`]). Comfortably above
/// [`CONNECTING_GRACE`] so a runner still legitimately connecting is never reaped;
/// the gap is pure housekeeping latency, not correctness (the grace already stops a
/// dead runner from masking demand).
const STALE_RUNNER_TTL: Duration = Duration::from_secs(600);

/// How long a stale `offline` runner GitHub still reports `busy` is left alone
/// before faber attempts the deregistration anyway ([`stale_runner_ids`]).
///
/// `busy` means GitHub still has a job assigned to that runner, and it answers a
/// `DELETE` for one with a `422` — a guard, not a fault: in a shared pool the
/// runner may be a *peer's*, its heartbeat merely lagging, and deleting it would
/// pull a live job out from under the peer. Deferring while `busy` is set is
/// therefore both cheaper (no call that is certain to fail) and safer than asking
/// and being refused.
///
/// The grace stops the deferral being unconditional, so a registration GitHub
/// never un-busies cannot sit ignored forever: past it faber asks regardless, and
/// a `422` at *that* age is genuinely unexpected and arms the backoff below.
/// Matches GitHub's own default job timeout (`timeout-minutes: 360`), the longest
/// a job can legitimately hold a runner busy.
const BUSY_RUNNER_GRACE: Duration = Duration::from_secs(6 * 60 * 60);

/// Base backoff before retrying a deregistration GitHub answered with a
/// permanent-shaped `422` (see [`is_permanent_dereg_failure`]); doubles each
/// further consecutive failure for that runner id. Unlike the provision throttle
/// there is no backoff-free grace: nothing faber does can change GitHub's mind
/// about a runner it believes is busy, so an immediate second attempt buys
/// nothing (#21).
const DEREG_BACKOFF_BASE_MS: u64 = 60_000;
/// Ceiling the dereg backoff is clamped to (~1h). Frequent enough to notice if
/// GitHub's own state ever does clear on its own; rare enough that a runner
/// permanently wedged this way costs at most one API call an hour, forever,
/// instead of one every poll cycle (#21).
const DEREG_BACKOFF_CEILING_MS: u64 = 3_600_000;

// --- runner-image pool coherence (ADR-0009) ----------------------------------
//
// ADR-0008 made the runner image a function of the *job*. GitHub matches queued
// jobs to runners by **label**, never by name or job id, so the runner faber
// mints for job J is routinely handed job K instead (`ExecutedJob` above exists
// because of exactly that). While every runner was interchangeable that was
// harmless — ADR-0004 says so out loud: "correctness never depends on it". Once
// the image varies per job it stops being harmless, because K can land on a
// container built for J's pin.
//
// faber cannot pin a runner to a job — no GitHub API offers it. What it *can*
// control is which images it puts into the pool, so the rule is about that:
// never add a runner that serves *less* than one already live for the repo, and
// where two images are genuinely incomparable, hold rather than run both at
// once (see `pool_decision`).

/// How long a claim may be held waiting for a repo's runner-image pool to drain
/// before faber mints anyway.
///
/// The hold exists to keep the pool homogeneous; it must never become a way to
/// starve a queued job, which is strictly worse than a surplus container (the
/// same asymmetry [`compute_supply`] fails open on). Past this bound faber mints
/// the image it resolved, logs that it did, and accepts the heterogeneous pool —
/// the job runs, possibly on the wrong image, which is exactly the pre-ADR-0009
/// behaviour and never worse than it. Sized to comfortably outlast a normal job
/// so an ordinary drain is never cut short.
const POOL_HOLD_MAX: Duration = Duration::from_secs(15 * 60);

/// How often a held claim re-checks the pool. The check is a lock-guarded read of
/// `inflight` with no I/O in it, so this is cheap enough to keep the wake-up
/// latency low without needing a signal to hang off (a `Notify` would have to be
/// shared with the drain, whose `notify_one` cannot serve two kinds of waiter).
const POOL_HOLD_POLL: Duration = Duration::from_secs(5);

/// The commit-status context a `skip` of a conflicted PR posts (ADR-0063 §6.3).
const WASTE_STATUS_CONTEXT: &str = "faber/waste";

/// What that status says.
const CONFLICTED_STATUS: &str =
    "Skipped: this PR conflicts with its base. Resolve the conflict to run CI.";

/// Static configuration for the reconcile loop.
pub struct Config {
    /// Repo topic that opts a repo into faber.
    pub topic: String,
    /// Delay between cycles.
    pub poll_interval: Duration,
    /// Autolimiter tuning: the memory budget, gates and learning.
    pub autolimit: AutoLimitConfig,
    /// Operator-pinned per-class caps, seeded into the limiter at startup.
    pub pinned_caps: HashMap<String, Limits>,
    /// The operator's hold on Docker admission (issue #66), restored from the
    /// overrides file so that a restart keeps it — and says so at every boot.
    pub hold: Option<Hold>,
    /// Per-repo holds on minting (ADR-0030), restored from the overrides file so
    /// a restart keeps them.
    pub repo_holds: Vec<RepoHold>,
    /// Routing profiles: each binds a label set to a backend + trust policy. A
    /// job is served by the first profile whose labels it carries.
    pub profiles: Vec<Profile>,
    /// Global runner-image drift policy (ADR-0001), the default a watched repo's
    /// `.faber/config.toml` may override. Surface-only in Phase 1.
    pub playwright_drift: DriftPolicy,
    /// Auto-retry of faber-caused failures (ADR-0003). Disabled by default.
    pub retry: RetryPolicy,
    /// What faber does with a claim that is likely wasted (ADR-0063). Every
    /// signal off by default, in which case nothing is read.
    pub waste: crate::config::WastePolicy,
    /// Host custody of the Docker daemon (ADR-0007).
    pub host: HostPolicy,
    /// On-demand runner-image variants (ADR-0008). Disabled by default, in which
    /// case the image stays a static property of the profile.
    pub runner_image: RunnerImagePolicy,
    /// This host's `[needs.<tool>]` allowlist (ADR-0010 §4), handed to the variant
    /// cache so the ceiling sits on the one call that can start a build.
    pub needs: HashMap<String, ToolNeeds>,
    /// Provisioning strategy (ADR-0004): `per-job` (mint one runner per observed
    /// queued job, deduped only in this process) or `demand` (mint only the
    /// shortfall vs. runners already registered on GitHub, so several instances
    /// share one repo pool). Default `per-job`.
    pub provisioning: ProvisioningMode,
    /// The services a job may declare (ADR-0046 §2). Empty unless the operator
    /// wrote a `[services.*]` table, in which case provisioning is unchanged.
    pub services: crate::services::ServicesPolicy,
    /// The pull-through mirror (`[mirror]`, docs/mirrors.md). `None` is the
    /// feature off: no Docker call is made for it at all.
    pub mirror: Option<crate::mirror::MirrorPolicy>,
    /// The per-host VM layout (`[linux_vm]`, `[macos_vm]`, ADR-0051). The
    /// default is the layout before the keys existed.
    pub vm_layout: crate::config::VmLayout,
}

impl Config {
    /// The images this host cannot re-obtain, which no deletion path may ever
    /// take. Delegated reclamation (ADR-0016) checked against it until ADR-0026
    /// took that route out of faber's scope; any future path that deletes an
    /// image faber did not build must refuse these first.
    ///
    /// Every entry is an image faber *itself* depends on to run a job: a
    /// profile's runner base, its dind sibling, and the autolimiter's probe. The
    /// runner base is the one that matters most — it is a local-only tag on this
    /// builder and in no registry (AGENTS.md), so removing it is not a slow
    /// rebuild but an outage with nothing to re-pull. Faber-built variants are
    /// absent on purpose: they have their own eviction (ADR-0008), which is
    /// licensed by the fact that faber can rebuild them from a recipe it owns.
    ///
    /// Repo-only spellings are added alongside `repo:tag` so a delegation that
    /// names an image either way is caught.
    pub fn protected_images(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut add = |name: &str| {
            let name = name.trim();
            if name.is_empty() {
                return;
            }
            out.insert(name.to_string());
            if let Some(repo) = name.split(':').next() {
                out.insert(repo.to_string());
            }
        };
        for p in &self.profiles {
            if let Backend::Docker(dc) = &p.backend {
                add(&dc.image);
                add(&dc.dind_image);
            }
        }
        add(&self.autolimit.probe_image);
        add(crate::image::VARIANT_REPO);
        out
    }
}

/// Running tallies, surfaced on the dashboard.
///
/// Each field is something faber itself decided or did. Notably absent: a "skip"
/// tally. The only skip that survives to be recorded is [`SkipReason::NotQueued`]
/// (`NotTargeted` returns before recording, and non-matching labels never reach
/// the gate at all) — faber re-observing a job it already minted a runner for.
/// Counting it just tracks `provisioned` at a constant multiple.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Counts {
    pub provisioned: u64,
    /// Runs whose runner executed a job and exited cleanly.
    ///
    /// This is faber's delivery, **not** the CI verdict: an ephemeral Actions
    /// runner exits 0 whether the job it ran went green or red, so a failing job
    /// lands here too. faber never learns a job's conclusion.
    pub completed: u64,
    /// Runs faber's own infrastructure failed to deliver: the sandbox errored
    /// before the job could start, the runner outlived its lifetime cap, or it
    /// exited non-zero (in practice a killed container). Excludes `reclaimed` —
    /// a deliberate, clean stand-down, not a fault.
    pub faults: u64,
    pub refused: u64,
    /// Jobs auto-re-run because faber's own runner killed/lost them (ADR-0003).
    pub retried: u64,
    /// Docker claims started with a daemon: the job asked for `dind` and its
    /// profile grants one (ADR-0026 §Consequences). Per claim, so a job whose
    /// provisioning is retried counts again.
    pub dind_provided: u64,
    /// Docker claims started without a daemon because the job did not ask for
    /// `dind`, whatever its profile grants. Per claim, as above.
    pub dind_withheld: u64,
    /// Jobs that asked for `dind` on a profile that denies it, and got no
    /// runner. Per job, not per retry: see `dind_refused` on [`StateView`].
    pub dind_refused_total: u64,
    /// Jobs that declared a service this host does not provide, and got no
    /// runner (ADR-0046 §2). Per job, not per retry: see `service_refused` on
    /// [`StateView`].
    pub service_refused_total: u64,
    /// Claims a waste check found likely wasted, whatever it did about them
    /// (ADR-0063 §3): each is one `waste` audit line.
    pub wasted: u64,
}

/// A single gate decision, for the dashboard's recent-activity feed.
#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub ts_ms: u64,
    pub repo: String,
    pub job_id: u64,
    pub event: String,
    pub actor: String,
    pub verdict: String,
    /// The routing profile that judged this job — which trust policy produced the
    /// verdict, and which backend a `provision` is headed for.
    pub profile: String,
}

/// A recent runner-image drift check, for the dashboard (ADR-0001). Only the
/// actionable outcomes reach this feed: a confirmed mismatch (`status = "drift"`),
/// an indeterminate read (`status = "indeterminate"`), or a half-wired opt-in
/// (`status = "misconfigured"` — repo configured, image unlabelled).
#[derive(Debug, Clone, Serialize)]
pub struct DriftEvent {
    pub ts_ms: u64,
    pub repo: String,
    pub run_id: u64,
    pub job_id: u64,
    /// `drift` | `indeterminate` | `misconfigured` — outcomes worth a human's eye.
    pub status: String,
    /// The version baked into the runner image (drift only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baked: Option<String>,
    /// The version the repo's lockfile pins (drift only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
    /// Resolved mode (`off` / `surface` / `gate`) (drift only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// What faber did (`surfaced`, `silent`, `gate_noop`, …) or why it couldn't.
    pub action: String,
}

/// A recent auto-retry action, for the dashboard (ADR-0003). One per run faber
/// re-ran because its own runner killed/lost the job(s).
#[derive(Debug, Clone, Serialize)]
pub struct RetryEvent {
    pub ts_ms: u64,
    pub repo: String,
    pub run_id: u64,
    /// The run attempt faber re-ran *from* (the re-run creates `attempt + 1`).
    pub attempt: u64,
    /// How many faber-caused jobs were re-run.
    pub jobs: usize,
    /// `all` (whole run via rerun-failed-jobs) or `surgical` (only the faber jobs,
    /// leaving genuine failures alone).
    pub mode: String,
}

/// A finished run, for the dashboard. The audit log's `outcome` record in the
/// shape the panel wants: the ending, and — for anything that isn't a plain
/// success — the one-line `cause` that says why.
///
/// The panel had no such feed at all: it showed gate decisions (what faber chose
/// to do) and live in-flight runners (what it is doing), and a job that finished
/// badly simply vanished from the display. "What went wrong ten minutes ago" was
/// answerable only by reading two files by hand.
#[derive(Debug, Clone, Serialize)]
pub struct OutcomeEvent {
    pub ts_ms: u64,
    pub repo: String,
    /// The job that actually ran (the executed job when it differed from the one
    /// the runner was minted for — that distinction is the audit log's job, not
    /// the panel's).
    pub job_id: u64,
    pub profile: String,
    /// The minted runner name — the join key to GitHub's runner list and to the
    /// host's `faber-runner-*` container.
    pub runner: String,
    /// `repo · workflow · job`, when the run could be attributed to a class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `success` | `failure` | `infra_error` | `timeout` | `reclaimed`.
    pub result: String,
    pub duration_s: u64,
    /// Of `duration_s`, how long was spent parked waiting for memory budget.
    pub wait_s: u64,
    /// The gate that held it longest while parked (issue #85).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_gate: Option<String>,
    pub attempt: u32,
    /// Why, for everything but a plain success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// The executed job's identity, in [`InflightView`]'s shapes, so the popover
    /// keys a finished job into the group it ran in. Copied from the claim when
    /// the executed job is the minted one; otherwise read off the executed job by
    /// the verdict fetch, with no PR, because the minted job's PR describes a
    /// different job. Always in the JSON, `null` when unknown.
    pub pr: Option<u64>,
    pub branch: Option<String>,
    pub workflow: Option<String>,
    pub job_name: Option<String>,
    pub event: Option<String>,
    /// GitHub's conclusion for the executed job, spelled as GitHub spells it
    /// (`success`, `failure`, `timed_out`, `cancelled`, …), filled in by
    /// [`reconcile_verdicts`] once GitHub reports the job completed and names
    /// this run's runner. `null` until then, and for good when it never does:
    /// faber shows a verdict only as GitHub reported it (ADR-0044). Display data
    /// only; nothing counts, admits or retries on it.
    pub conclusion: Option<String>,
    /// When faber read `conclusion` from GitHub (ms). The popover counts a CI
    /// pip's linger from here, not from `ts_ms`, so a verdict that arrives late
    /// still shows for its full time. `null` with `conclusion`.
    pub conclusion_ts_ms: Option<u64>,
}

/// How many times [`reconcile_verdicts`] asks GitHub for a finished job's
/// conclusion before leaving it absent (ADR-0044). GitHub marks a job completed a
/// few seconds after its runner exits, so the first cycle usually answers.
const VERDICT_TRIES: u32 = 3;

/// How many finished jobs one [`reconcile_verdicts`] batch asks GitHub about,
/// oldest first. The rest wait for the next cycle, untried, so a burst of
/// finished jobs costs a few GETs a cycle rather than fifty.
const VERDICT_BATCH: usize = 8;

/// When a finished run's one early verdict fetch fires: GitHub marks the job
/// completed a few seconds after its runner exits, and a verdict that waits for
/// the next cycle (20 s by default) would eat most of the passed pip's 30 s.
const VERDICT_EARLY: Duration = Duration::from_secs(5);

/// A finished run whose CI conclusion has not been read yet. Keyed back to its
/// [`OutcomeEvent`] by `(job_id, runner, ts_ms)`.
#[derive(Debug, Clone)]
struct PendingVerdict {
    repo: String,
    /// The executed job.
    job_id: u64,
    /// The runner GitHub must name on the job before its conclusion is this run's.
    runner: String,
    ts_ms: u64,
    tries: u32,
    /// The executed job was not the minted one, so the identity comes from the
    /// fetched job rather than the claim.
    identity_from_job: bool,
}

/// What one fetch of a finished job settled.
#[derive(Debug, PartialEq, Eq)]
enum VerdictRead {
    /// Not completed yet, or the fetch failed: try again on a later cycle.
    Pending,
    /// Nothing more to learn. `None` when GitHub reported no conclusion this run
    /// may claim: the job is gone, or another runner ran it.
    Settled(Option<JobVerdict>),
}

#[derive(Debug, PartialEq, Eq)]
struct JobVerdict {
    conclusion: Option<String>,
    job_name: String,
    workflow: Option<String>,
    branch: Option<String>,
}

/// Read a fetched job for this run's verdict. Pure, so the rule is testable: a
/// conclusion is taken only from a `completed` job whose runner is this run's
/// own, never inferred.
fn read_verdict(job: Option<&Job>, runner: &str) -> VerdictRead {
    let Some(job) = job else {
        return VerdictRead::Settled(None);
    };
    if job.status != "completed" {
        return VerdictRead::Pending;
    }
    if job.runner_name.as_deref() != Some(runner) {
        return VerdictRead::Settled(None);
    }
    VerdictRead::Settled(Some(JobVerdict {
        conclusion: job.conclusion.clone(),
        job_name: job.name.clone(),
        workflow: job.workflow_name.clone(),
        branch: job.head_branch.clone(),
    }))
}

/// A fault in faber itself, for the dashboard: the daemon couldn't do its job,
/// rather than a job that failed. `action` is `failed` or `recovered`, so the two
/// entries bracket the outage they describe.
#[derive(Debug, Clone, Serialize)]
pub struct FaultEvent {
    pub ts_ms: u64,
    /// The repo whose scan failed, or `-` when the fault isn't about one repo.
    pub repo: String,
    /// Which part of the pipeline broke — `discovery`, `repo_scan`, …
    pub scope: String,
    /// `failed` | `recovered`.
    pub action: String,
    pub cause: String,
}

/// Per-job cross-cycle state, keyed by the stable `job.id`. Folds the recording
/// dedup (bug-001) and the provision-retry throttle (bug-002) into one entry, so a
/// single [`Metrics::prune`] bounds both to the live working set.
#[derive(Default)]
struct JobState {
    /// Last verdict label recorded for this job — the record-once dedup key.
    last_verdict: Option<String>,
    /// Provision attempts made while the job stayed `queued`. Only bumped when a
    /// runner is actually spawned; a healthy job is consumed and pruned before
    /// this climbs, so only a genuinely stuck job accumulates attempts. Past
    /// [`PROVISION_MAX_ATTEMPTS`] it keeps climbing (each slow self-heal retry
    /// counts) — the storm is bounded by cadence, not by a hard stop.
    provision_attempts: u32,
    /// Earliest wall-clock ms at which the next provision attempt is allowed.
    next_ok_ms: u64,
    /// Set once the fast-attempt cap is first crossed, so the one-shot
    /// "throttled to slow self-heal retry" `warn!` fires exactly once, not every
    /// slow-retry cycle.
    exhausted_warned: bool,
    /// Consecutive reconcile cycles this job has been absent from `seen`. Reset to
    /// zero whenever the job is re-observed; drives [`Metrics::prune`]'s miss-grace
    /// so a single transient discovery gap doesn't wipe the throttle/dedup state
    /// (bug-002 defect A). Bounded by [`PRUNE_MISS_GRACE`].
    misses: u32,
}

/// Per-runner-id state for the stale-runner deregistration sweep, keyed by the
/// runner's GitHub id. An entry exists only for a runner GitHub has refused to
/// delete with a **permanent**-shaped failure (see [`is_permanent_dereg_failure`])
/// — a runner deregistered cleanly, or one whose attempt failed only transiently,
/// never gets one. Needs no [`Metrics::prune`]-style aging: in steady state this
/// map is empty, and even at its worst it is bounded by the number of runners
/// that have ever hit this specific edge case (#21) — not by cycle count — so it
/// can never grow unbounded the way an un-pruned per-cycle map could.
#[derive(Default)]
struct DeregState {
    /// Consecutive permanent-shaped failures observed for this runner id.
    attempts: u32,
    /// Earliest wall-clock ms at which the next attempt is allowed.
    next_ok_ms: u64,
    /// Set once the first failure has been logged at `WARN`, so every further
    /// repeat — the runner is *still* wedged, not new information — logs at
    /// `debug` instead. This is what stops the flood #21 reported (hundreds of
    /// identical `WARN`s in one log).
    warned: bool,
    /// Set once a busy-deferral for this runner has been reported. The deferral is
    /// re-derived every cycle from GitHub's listing, so without this the report
    /// would repeat every ~40s for as long as the job runs — reintroducing exactly
    /// the flood #21 was about, one severity lower.
    deferred_logged: bool,
}

/// Most refused-queued jobs listed at once, oldest kept. Outsiders can cause
/// entries — a fork PR on a misrouted workflow targets this host's labels and is
/// refused — so the set must not grow with whatever they open; the rest are only
/// counted (`refused_queued_overflow`).
const REFUSED_QUEUED_CAP: usize = 50;

/// Most dind refusals listed at once (ADR-0026 §Consequences), newest kept: the
/// list says what is wrong *now*, and a workflow author can add the label to any
/// number of jobs.
const DIND_REFUSED_CAP: usize = 50;

/// How long a dind refusal stays listed without being re-made. A refused job is
/// still queued, so the provision throttle re-tries it at least every
/// [`PROVISION_BACKOFF_CEILING_MS`] and each try refreshes its entry; one that
/// has not been refreshed for this long has started elsewhere, finished or been
/// cancelled.
const DIND_REFUSED_TTL_MS: u64 = 60 * 60 * 1000;

/// Most service refusals listed at once (ADR-0046 §2), newest kept, for the
/// reason [`DIND_REFUSED_CAP`] gives.
const SERVICE_REFUSED_CAP: usize = 50;

/// How long a service refusal stays listed without being re-made, for the
/// reason [`DIND_REFUSED_TTL_MS`] gives.
const SERVICE_REFUSED_TTL_MS: u64 = 60 * 60 * 1000;

/// The `DOCKER_HOST` a job that did not ask for `dind` is given (ADR-0026). The
/// socket does not exist, so the job's first `docker` call fails at once and the
/// error message carries the path: the path *is* the instruction.
const NO_DIND_DOCKER_HOST: &str = "unix:///faber-no-dind/add-dind-to-runs-on";

#[derive(Default)]
struct Metrics {
    counts: Counts,
    recent: VecDeque<Decision>,
    /// Jobs queued and refused by the gate right now, by job id (issue #95).
    /// Rewritten per repo each time that repo is scanned ([`Metrics::sync_refused`]),
    /// so a repo whose listing failed keeps its entries until it can be read again.
    refused_queued: HashMap<u64, RefusedJobView>,
    /// Per repo, how many refused jobs were left out of `refused_queued` by
    /// [`REFUSED_QUEUED_CAP`]. Summed into `StateView::refused_queued_overflow`.
    refused_overflow: HashMap<String, usize>,
    /// Jobs that asked for `dind` on a profile that denies it, by job id
    /// (ADR-0026 §Consequences). Kept apart from `refused_queued` on purpose:
    /// that set feeds repo holds through `refused_could_take`, and a dind refusal
    /// is a routing mismatch, not a trust verdict. Bounded by [`DIND_REFUSED_CAP`]
    /// and aged out by [`DIND_REFUSED_TTL_MS`] in [`Metrics::note_dind_refused`].
    dind_refused: HashMap<u64, DindRefusedView>,
    /// Jobs that declared a service this host does not provide, by job id
    /// (ADR-0046 §2). Kept apart from `refused_queued` for the reason
    /// `dind_refused` is. Bounded by [`SERVICE_REFUSED_CAP`] and aged out by
    /// [`SERVICE_REFUSED_TTL_MS`] in [`Metrics::note_service_refused`].
    service_refused: HashMap<u64, ServiceRefusedView>,
    /// Recent actionable drift checks (ADR-0001), newest last. Bounded by
    /// [`DRIFT_RECENT_CAP`].
    recent_drift: VecDeque<DriftEvent>,
    /// Recent auto-retry actions (ADR-0003), newest last. Bounded by
    /// [`RETRY_RECENT_CAP`].
    recent_retries: VecDeque<RetryEvent>,
    /// Recent finished runs, newest last. Bounded by [`OUTCOME_RECENT_CAP`].
    recent_outcomes: VecDeque<OutcomeEvent>,
    /// Finished runs whose CI conclusion is still to be read, oldest first
    /// (ADR-0044). Bounded by [`OUTCOME_RECENT_CAP`] and by [`VERDICT_TRIES`]
    /// per entry.
    pending_verdicts: VecDeque<PendingVerdict>,
    /// Recent faber-level faults + recoveries, newest last. Bounded by
    /// [`FAULT_RECENT_CAP`].
    recent_faults: VecDeque<FaultEvent>,
    /// The `scope/repo` keys currently in a faulted state — the transition gate
    /// behind [`Metrics::note_fault`]. Needs no pruning for the same reason
    /// `dereg` doesn't: in steady state it is empty, and its worst case is bounded
    /// by the number of distinct scopes/repos that have ever faulted, not by cycle
    /// count. (A repo that faults and then leaves the topic keeps one entry for
    /// this process's lifetime — one `String`, and a restart clears it.)
    faulted: HashSet<String>,
    /// Per-job cross-cycle state (recording dedup + provision throttle). Bounded
    /// by [`Metrics::prune`] to the live working set each cycle, so it never
    /// becomes an unbounded leak.
    jobs: HashMap<u64, JobState>,
    /// Per-runner-id backoff state for the stale-runner dereg sweep (#21). See
    /// [`DeregState`].
    dereg: HashMap<u64, DeregState>,
    /// Repos whose no-opt-in-label state has already been probed this daemon
    /// lifetime (ADR-0001). The first Docker job of a repo probes for a half-wired
    /// opt-in (repo configured, image unlabelled) and diagnoses it once; later jobs
    /// skip the extra `.faber/config.toml` fetch. Naturally bounded by the count of
    /// served repos (not jobs), so it needs no pruning; a restart re-arms it, so a
    /// standing misconfiguration re-surfaces once per daemon start.
    drift_probed: HashSet<String>,
    /// Jobs recorded as gate bypasses (ADR-0030) → when, and which class. The
    /// record-once dedup, and the source of the 7-day trust count. Seeded from the
    /// audit log at boot, and bounded by [`BYPASS_WINDOW`].
    bypassed: HashMap<u64, (u64, BypassClass)>,
    /// Recent bypasses, newest last. Bounded by [`BYPASS_RECENT_CAP`].
    recent_bypasses: VecDeque<BypassView>,
    /// Jobs whose runner faber force-killed as an enforced trust bypass → when.
    /// Kept out of auto-retry; seeded from the audit log, bounded by
    /// [`BYPASS_WINDOW`].
    killed: HashMap<u64, u64>,
}

/// The provision-throttle verdict for one job this cycle (bug-002).
#[derive(Debug, PartialEq, Eq)]
enum ProvisionGate {
    /// Clear to (re-)provision now, still within the fast-attempt regime.
    Ready,
    /// Inside the post-attempt backoff window; skip this cycle.
    BackingOff,
    /// Past the fast-attempt cap ([`PROVISION_MAX_ATTEMPTS`]) but the ceiling
    /// backoff window has elapsed: (re-)provision now on the slow *self-heal*
    /// cadence rather than abandoning the job (bug-002 defect B). Behaves like
    /// [`ProvisionGate::Ready`] for the caller — it provisions — but `warn` is true
    /// on exactly the first crossover so the caller logs the throttle-to-slow-retry
    /// transition once.
    SlowRetry { warn: bool },
}

/// Backoff before the next attempt, given how many have already been made: zero
/// through the grace, then exponential from the base, clamped to the ceiling.
/// Pure and deterministic — the testable core of the throttle.
fn provision_backoff_ms(attempts: u32) -> u64 {
    if attempts < PROVISION_GRACE_ATTEMPTS {
        return 0;
    }
    let shift = (attempts - PROVISION_GRACE_ATTEMPTS).min(20);
    PROVISION_BACKOFF_BASE_MS
        .saturating_mul(1u64 << shift)
        .min(PROVISION_BACKOFF_CEILING_MS)
}

/// Backoff before the next dereg attempt for a runner id, given how many
/// permanent-shaped failures it has already accumulated: exponential from the
/// base, clamped to the ceiling. Pure and deterministic, mirroring
/// [`provision_backoff_ms`] — but with no backoff-free grace, since nothing here
/// is expected to resolve itself between one attempt and the next (#21).
fn dereg_backoff_ms(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(20);
    DEREG_BACKOFF_BASE_MS
        .saturating_mul(1u64 << shift)
        .min(DEREG_BACKOFF_CEILING_MS)
}

impl Metrics {
    /// Write a fetched verdict onto its outcome, in place and without re-pushing
    /// it. An outcome already evicted by [`OUTCOME_RECENT_CAP`] is left gone.
    fn apply_verdict(&mut self, p: &PendingVerdict, v: JobVerdict) {
        let Some(o) = self
            .recent_outcomes
            .iter_mut()
            .find(|o| o.job_id == p.job_id && o.ts_ms == p.ts_ms && o.runner == p.runner)
        else {
            return;
        };
        o.conclusion_ts_ms = v.conclusion.as_ref().map(|_| now_ms());
        o.conclusion = v.conclusion;
        if p.identity_from_job {
            o.pr = None;
            o.job_name = Some(v.job_name);
            o.workflow = v.workflow;
            o.branch = v.branch;
        }
    }

    /// Record-once gate. Returns `true` only when `verdict_label` differs from the
    /// last label recorded for `job_id` (or the job is unseen), updating the stored
    /// label as a side effect. Observing the same job with the same verdict on a
    /// later cycle returns `false`, so its counter/feed/audit side effects fire
    /// once per verdict *transition*, not once per poll. A genuine verdict change
    /// returns `true` again (recorded once at the new verdict).
    fn note(&mut self, job_id: u64, verdict_label: &str) -> bool {
        let entry = self.jobs.entry(job_id).or_default();
        if entry.last_verdict.as_deref() == Some(verdict_label) {
            false
        } else {
            entry.last_verdict = Some(verdict_label.to_string());
            true
        }
    }

    /// Replace `repo`'s refused-queued entries with `current` (this cycle's scan).
    /// A job already present keeps its original `since_ms`; one that is absent
    /// from `current` (started, finished, cancelled, or no longer refused) is
    /// dropped. Other repos' entries are untouched. Returns the jobs that are new,
    /// so the caller warns once per job rather than once per cycle.
    fn sync_refused(&mut self, repo: &str, current: Vec<RefusedJobView>) -> Vec<RefusedJobView> {
        let keep: HashSet<u64> = current.iter().map(|v| v.job_id).collect();
        self.refused_queued
            .retain(|id, v| v.repo != repo || keep.contains(id));
        self.refused_overflow.remove(repo);
        let mut added = Vec::new();
        for mut v in current {
            match self.refused_queued.get_mut(&v.job_id) {
                Some(old) => {
                    v.since_ms = old.since_ms;
                    *old = v;
                }
                None => {
                    added.push(v.clone());
                    self.refused_queued.insert(v.job_id, v);
                }
            }
        }
        // Over the cap: keep the oldest, evict the newest, and count what was left
        // out against the repo it belongs to (it may be another repo's entry).
        if self.refused_queued.len() > REFUSED_QUEUED_CAP {
            let mut all: Vec<(u64, u64)> = self
                .refused_queued
                .values()
                .map(|v| (v.since_ms, v.job_id))
                .collect();
            all.sort_unstable();
            for (_, id) in all.split_off(REFUSED_QUEUED_CAP) {
                if let Some(v) = self.refused_queued.remove(&id) {
                    *self.refused_overflow.entry(v.repo).or_default() += 1;
                }
            }
        }
        added.retain(|v| self.refused_queued.contains_key(&v.job_id));
        added
    }

    /// Forget refused entries of repos no longer served (left the topic).
    fn retain_refused_repos(&mut self, repos: &[String]) {
        self.refused_queued.retain(|_, v| repos.contains(&v.repo));
        self.refused_overflow.retain(|r, _| repos.contains(r));
    }

    /// Record (or refresh) a dind refusal. Entries not refreshed within
    /// [`DIND_REFUSED_TTL_MS`] are dropped first, then the newest
    /// [`DIND_REFUSED_CAP`] are kept. Returns whether the job is new to the list,
    /// so the refusal is counted once per job rather than once per retry.
    fn note_dind_refused(&mut self, v: DindRefusedView) -> bool {
        let now = v.at_ms;
        self.dind_refused
            .retain(|_, e| now.saturating_sub(e.at_ms) < DIND_REFUSED_TTL_MS);
        let new = self.dind_refused.insert(v.job_id, v).is_none();
        if new {
            self.counts.dind_refused_total += 1;
        }
        if self.dind_refused.len() > DIND_REFUSED_CAP {
            let mut all: Vec<(u64, u64)> = self
                .dind_refused
                .values()
                .map(|e| (e.at_ms, e.job_id))
                .collect();
            // Newest first; everything past the cap is the oldest, and goes.
            all.sort_unstable_by(|a, b| b.cmp(a));
            for (_, id) in all.split_off(DIND_REFUSED_CAP) {
                self.dind_refused.remove(&id);
            }
        }
        new
    }

    /// The live refusals, oldest first. Entries past [`DIND_REFUSED_TTL_MS`]
    /// are filtered out here too, so a stale one never lingers in `/api/state`
    /// waiting for the next refusal to evict it.
    fn dind_refused_sorted(&self, now_ms: u64) -> Vec<DindRefusedView> {
        let mut v: Vec<DindRefusedView> = self
            .dind_refused
            .values()
            .filter(|e| now_ms.saturating_sub(e.at_ms) < DIND_REFUSED_TTL_MS)
            .cloned()
            .collect();
        v.sort_unstable_by_key(|r| (r.at_ms, r.job_id));
        v
    }

    /// Record (or refresh) a service refusal. Entries not refreshed within
    /// [`SERVICE_REFUSED_TTL_MS`] are dropped first, then the newest
    /// [`SERVICE_REFUSED_CAP`] are kept. Returns whether the job is new to the
    /// list, so the refusal is counted once per job rather than once per retry.
    fn note_service_refused(&mut self, v: ServiceRefusedView) -> bool {
        let now = v.at_ms;
        self.service_refused
            .retain(|_, e| now.saturating_sub(e.at_ms) < SERVICE_REFUSED_TTL_MS);
        let new = self.service_refused.insert(v.job_id, v).is_none();
        if new {
            self.counts.service_refused_total += 1;
        }
        if self.service_refused.len() > SERVICE_REFUSED_CAP {
            let mut all: Vec<(u64, u64)> = self
                .service_refused
                .values()
                .map(|e| (e.at_ms, e.job_id))
                .collect();
            // Newest first; everything past the cap is the oldest, and goes.
            all.sort_unstable_by(|a, b| b.cmp(a));
            for (_, id) in all.split_off(SERVICE_REFUSED_CAP) {
                self.service_refused.remove(&id);
            }
        }
        new
    }

    /// The live service refusals, oldest first, filtered by
    /// [`SERVICE_REFUSED_TTL_MS`] as [`Metrics::dind_refused_sorted`] is.
    fn service_refused_sorted(&self, now_ms: u64) -> Vec<ServiceRefusedView> {
        let mut v: Vec<ServiceRefusedView> = self
            .service_refused
            .values()
            .filter(|e| now_ms.saturating_sub(e.at_ms) < SERVICE_REFUSED_TTL_MS)
            .cloned()
            .collect();
        v.sort_unstable_by_key(|r| (r.at_ms, r.job_id));
        v
    }

    fn refused_overflow_total(&self) -> usize {
        self.refused_overflow.values().sum()
    }

    fn refused_queued_sorted(&self) -> Vec<RefusedJobView> {
        let mut v: Vec<RefusedJobView> = self.refused_queued.values().cloned().collect();
        v.sort_unstable_by_key(|r| (r.since_ms, r.job_id));
        v
    }

    /// Age out state for jobs no longer queued or in-progress anywhere, keeping the
    /// map to the live working set so it can't grow without bound — but with a
    /// consecutive-miss grace so a single transient discovery gap doesn't wipe it
    /// (bug-002 defect A). A job present in `seen` has its miss counter reset and is
    /// kept; a job absent has it bumped, and is dropped only once it has been missed
    /// more than [`PRUNE_MISS_GRACE`] cycles in a row. A genuinely finished job is
    /// thus forgotten a few cycles later (still bounded), while a one-cycle blip —
    /// a transient per-repo fetch error, or GitHub momentarily returning a run with
    /// zero jobs — no longer resets its `provision_attempts`/`last_verdict`.
    fn prune(&mut self, seen: &HashSet<u64>) {
        self.jobs.retain(|job_id, st| {
            if seen.contains(job_id) {
                st.misses = 0;
                true
            } else {
                st.misses += 1;
                st.misses <= PRUNE_MISS_GRACE
            }
        });
    }

    /// Fault transition gate. Returns `true` only when `failed` differs from the
    /// state already recorded for `key` (`scope/repo`), updating it as a side
    /// effect — so a fault records once when it starts and its recovery records
    /// once when it clears, however many poll cycles the outage spans.
    ///
    /// A recovery for something that never faulted returns `false`: the steady
    /// state is "everything works", and writing a `recovered` line every cycle
    /// would bury the two lines that matter under thousands that don't.
    fn note_fault(&mut self, key: &str, failed: bool) -> bool {
        if failed {
            self.faulted.insert(key.to_string())
        } else {
            self.faulted.remove(key)
        }
    }

    /// Reopen the faults audit `entries` leave open ([`replayed_open_faults`]),
    /// in the transition gate and on the dashboard, so the first healthy check
    /// after a restart closes each with a `recovered` line.
    fn restore_open_faults(&mut self, entries: &[crate::audit::Entry]) {
        for fault in replayed_open_faults(entries) {
            self.faulted
                .insert(format!("{}/{}", fault.scope, fault.repo));
            self.recent_faults.push_back(fault);
        }
        while self.recent_faults.len() > FAULT_RECENT_CAP {
            self.recent_faults.pop_front();
        }
    }

    /// Provision-throttle gate (bug-002), consulted before the `inflight` claim.
    /// The backoff window is checked first so it governs *both* regimes: while
    /// inside it the answer is always [`ProvisionGate::BackingOff`] (skip). Once the
    /// window has elapsed, an under-cap job is [`ProvisionGate::Ready`]; a job past
    /// [`PROVISION_MAX_ATTEMPTS`] is [`ProvisionGate::SlowRetry`] — it still
    /// provisions, but `note_provision_attempt` re-arms the window at the ceiling
    /// cadence, so past the cap this naturally throttles to one self-heal retry per
    /// ceiling interval instead of stranding the job (bug-002 defect B). `now` is
    /// passed in so the decision is pure and testable.
    fn provision_gate(&mut self, job_id: u64, now: u64) -> ProvisionGate {
        let entry = self.jobs.entry(job_id).or_default();
        if now < entry.next_ok_ms {
            ProvisionGate::BackingOff
        } else if entry.provision_attempts >= PROVISION_MAX_ATTEMPTS {
            let warn = !entry.exhausted_warned;
            entry.exhausted_warned = true;
            ProvisionGate::SlowRetry { warn }
        } else {
            ProvisionGate::Ready
        }
    }

    /// Record that a runner was just spawned for `job_id` at `now`: bump the
    /// attempt count and arm the backoff before the next attempt. Called only
    /// after a successful claim, so it counts real spawns, not poll ticks. Past the
    /// fast-attempt cap it keeps counting — `provision_backoff_ms` ceilings, so the
    /// re-armed window is the fixed self-heal cadence (`saturating_add` guards the
    /// unbounded climb of a perpetually-stuck job).
    ///
    /// Returns the attempt number this spawn *is* (1 for the first), which the
    /// outcome record carries so a repeatedly re-provisioned job is legible as one.
    fn note_provision_attempt(&mut self, job_id: u64, now: u64) -> u32 {
        let entry = self.jobs.entry(job_id).or_default();
        entry.provision_attempts = entry.provision_attempts.saturating_add(1);
        entry.next_ok_ms = now.saturating_add(provision_backoff_ms(entry.provision_attempts));
        entry.provision_attempts
    }

    /// Whether `runner_id` is clear to attempt deregistration now, or still
    /// backing off from a prior permanent-shaped failure ([`DeregState`], #21). A
    /// runner with no recorded failure is always clear — the common case (a clean
    /// success, or a runner never yet attempted) never allocates an entry. `now`
    /// is passed in so the decision is pure and testable. Read-only: unlike
    /// [`Metrics::provision_gate`] this never needs to create an entry just to
    /// answer the question.
    fn dereg_gate(&self, runner_id: u64, now: u64) -> bool {
        match self.dereg.get(&runner_id) {
            Some(st) => now >= st.next_ok_ms,
            None => true,
        }
    }

    /// Record a permanent-shaped dereg failure for `runner_id` at `now`: bump the
    /// attempt count and arm the backoff before the next attempt (#21). Returns
    /// `true` the first time this runner id ever fails this way — telling the
    /// caller to log at `WARN` — and `false` on every later repeat, so the caller
    /// can drop to `debug` instead; that downgrade is what stops the flood.
    fn note_dereg_failure(&mut self, runner_id: u64, now: u64) -> bool {
        let entry = self.dereg.entry(runner_id).or_default();
        entry.attempts = entry.attempts.saturating_add(1);
        entry.next_ok_ms = now.saturating_add(dereg_backoff_ms(entry.attempts));
        let first = !entry.warned;
        entry.warned = true;
        first
    }

    /// Whether this runner's busy-deferral is worth logging: `true` the first time
    /// `runner_id` is deferred, `false` on every cycle it stays deferred. Mirrors
    /// [`Metrics::note_dereg_failure`]'s once-per-runner discipline.
    ///
    /// The entry this allocates never gates anything: [`Metrics::dereg_gate`] reads
    /// `next_ok_ms`, which a deferral leaves at its `0` default, so a runner that is
    /// only ever deferred stays immediately eligible the moment GitHub un-busies it.
    fn note_deferral(&mut self, runner_id: u64) -> bool {
        let entry = self.dereg.entry(runner_id).or_default();
        let first = !entry.deferred_logged;
        entry.deferred_logged = true;
        first
    }

    /// Clear any backoff state for `runner_id` — it deregistered successfully, so
    /// there is nothing left to back off from. Not load-bearing for correctness (a
    /// deleted runner never reappears in a future [`stale_runner_ids`] listing, so
    /// a stale entry would simply sit unused), but keeps the map from carrying
    /// dead weight for a runner that took a few attempts before succeeding.
    fn clear_dereg(&mut self, runner_id: u64) {
        self.dereg.remove(&runner_id);
    }

    /// Drop backoff state for every runner id not in `listed` — GitHub's current
    /// runner listing. [`Metrics::clear_dereg`] only fires on a deregistration
    /// *faber* performed, so a runner that left the list any other way (a peer
    /// instance reaped it, or GitHub GC'd it) would otherwise keep its entry for
    /// the daemon's whole lifetime. Unlike [`Metrics::prune`] this needs no miss
    /// grace: an entry wrongly dropped costs nothing but one un-gated attempt, and
    /// a runner absent from the listing is not a sweep candidate anyway.
    fn retain_dereg(&mut self, listed: &HashSet<u64>) {
        self.dereg.retain(|id, _| listed.contains(id));
    }

    /// Record-once gate for a gate bypass (ADR-0030): `true` the first time
    /// `job_id` is seen as one, `false` on every later cycle that sees it still
    /// running on the same runner. Entries older than [`BYPASS_WINDOW`] are
    /// dropped on the way, so the map is the window and nothing older.
    fn note_bypass(&mut self, job_id: u64, class: BypassClass, now: u64) -> bool {
        let window = BYPASS_WINDOW.as_millis() as u64;
        self.bypassed
            .retain(|_, (ts, _)| now.saturating_sub(*ts) <= window);
        if self.bypassed.contains_key(&job_id) {
            return false;
        }
        self.bypassed.insert(job_id, (now, class));
        true
    }

    /// Remember that `job_id`'s runner was force-killed at `now`.
    fn note_killed(&mut self, job_id: u64, now: u64) {
        let window = BYPASS_WINDOW.as_millis() as u64;
        self.killed
            .retain(|_, ts| now.saturating_sub(*ts) <= window);
        self.killed.insert(job_id, now);
    }

    /// Bypasses whose code may be hostile, trust and third-party alike,
    /// recorded within [`BYPASS_WINDOW`] of `now`.
    fn trust_bypasses_since(&self, now: u64) -> usize {
        let window = BYPASS_WINDOW.as_millis() as u64;
        self.bypassed
            .values()
            .filter(|(ts, class)| class.may_be_hostile() && now.saturating_sub(*ts) <= window)
            .count()
    }
}

/// A Docker profile whose own image is confirmed missing from this host (#63).
#[derive(Debug, Clone, Serialize)]
pub struct MissingImageView {
    pub profile: String,
    pub image: String,
    /// When faber confirmed it missing, epoch ms.
    pub since_ms: u64,
    /// Why pulling it failed, in the daemon's words (bounded and escaped).
    pub cause: String,
}

/// A Docker profile whose image faber would pull from a Docker Hub user namespace
/// only because `[host] pull_unqualified_images` is unstated (#156). A warning
/// only: nothing about admission or health changes while it stands.
#[derive(Debug, Clone, Serialize)]
pub struct UnqualifiedImageView {
    pub profile: String,
    pub image: String,
    /// What the risk is and how to settle it, as boot logs it.
    pub message: String,
}

/// The profiles [`UnqualifiedImageView`] lists. Boot warns about each, on the
/// terms of [`cpu_unstated_warning`]: the old behaviour stands, and says so.
fn unqualified_images(profiles: &[Profile], host: &HostPolicy) -> Vec<UnqualifiedImageView> {
    profiles
        .iter()
        .filter_map(|p| match &p.backend {
            Backend::Docker(d)
                if crate::profile_image::unstated_risk(&d.image, host.pull_unqualified_images) =>
            {
                Some(UnqualifiedImageView {
                    profile: p.name.clone(),
                    image: d.image.clone(),
                    message: crate::profile_image::unstated_warning(&p.name, &d.image),
                })
            }
            _ => None,
        })
        .collect()
}

/// A profile whose runner GitHub has announced it will retire (#64). A warning
/// only: nothing about admission or health changes while it stands.
#[derive(Debug, Clone, Serialize)]
pub struct RunnerRetirementView {
    pub profile: String,
    /// The image the profile names, or `None` for a macOS VM profile.
    pub image: Option<String>,
    /// The `actions/runner` release GitHub named, which the image bakes.
    pub runner_version: String,
    /// The day it stops running jobs, `YYYY-MM-DD`.
    pub cutoff: String,
    /// Whole UTC days until then; `0` on the day, negative once it has passed.
    pub days_remaining: i64,
    /// At most three days remain, or none do.
    pub urgent: bool,
    /// The job whose annotation said so, and when faber read it, epoch ms.
    pub job_id: u64,
    pub seen_ms: u64,
    /// The warning in the words the log uses.
    pub message: String,
}

/// A configured routing profile, as shown on the dashboard.
#[derive(Debug, Serialize)]
pub struct ProfileView {
    pub name: String,
    pub labels: Vec<String>,
    pub backend: String,
    /// Whether this profile's jobs get a Docker daemon — `granted`, `denied` or
    /// `unstated` — or `None` for a macOS VM profile, which has no containers.
    ///
    /// On the dashboard because it is the widest privilege faber hands out and it
    /// was previously readable only from the source (issue #43). `unstated` is
    /// shown as its own word rather than folded into `granted`: it grants, but
    /// nobody chose to.
    pub dind: Option<String>,
    /// Whether AGENTS.md's Linux dind wall (ADR-0025 §"On Linux, dind waits for
    /// a wall") applies to this profile right now — `false` for a macOS VM
    /// profile, for a denied grant, or on any host but Linux.
    ///
    /// Judged against today's [`crate::config::WallFacts::current`] — PR2
    /// (ADR-0026) changes it there, once jobs run inside faber's own VM, with
    /// no change needed here. See [`crate::config::dind_needs_wall`].
    pub needs_wall: bool,
    /// The `actions/runner` release GitHub last named for a job on this profile
    /// (#64). Only GitHub's deprecation annotation names it, so it is `None` until
    /// a job carries one.
    pub runner_version: Option<String>,
}

/// The step a job is running right now: its name and 1-based position within the
/// job's step list (`index` of `total`). Absent until the runner reports an
/// in-progress step.
#[derive(Debug, Clone, Serialize)]
pub struct StepView {
    pub name: String,
    pub index: u32,
    pub total: u32,
}

/// One in-flight runner, with the identity a human reads on the dashboard: which
/// repo, which PR (or branch, when there's no PR), which workflow/job, how long
/// it's been running, whether its runner has spun up yet (`admitted`), and the
/// live step it's on. `profile` (the executor) is kept for tooling but demoted
/// in the UI.
#[derive(Debug, Serialize)]
pub struct InflightView {
    pub job_id: u64,
    pub profile: String,
    pub repo: String,
    /// PR number when the run belongs to a same-repo PR; else `None` (fork PRs
    /// and non-PR events fall back to `branch`).
    pub pr: Option<u64>,
    pub branch: Option<String>,
    pub workflow: Option<String>,
    pub job_name: String,
    /// The triggering event (`push`, `pull_request`, `schedule`, …).
    pub event: String,
    /// Seconds since faber claimed the job (its elapsed wall-clock).
    pub elapsed_s: u64,
    /// True once a runner container exists (past provisioning). Drives the "is it
    /// still spinning up vs actually running" distinction the row's dot shows.
    pub admitted: bool,
    /// The live step, polled from GitHub for admitted jobs. `None` while
    /// provisioning or between steps.
    pub step: Option<StepView>,
    /// Why a parked claim is still parked: the gate holding it and a line saying
    /// why (issue #66). Always in the JSON; `null` once admitted.
    pub waiting: Option<WaitView>,
    /// Set while the critical floor has this job's containers frozen with
    /// `docker pause` (ADR-0032 §2): since when, and why. Always in the JSON;
    /// `null` for a job that is not frozen. A frozen job is still admitted and
    /// still running — only stopped until the Docker volume has room again.
    pub frozen: Option<crate::freeze::FrozenView>,
    /// Where the job is in its life on this host — the one field every count of
    /// "running" vs "waiting" reads, so the menu bar, popover and dashboard agree.
    pub phase: JobPhase,
}

/// Where an in-flight job is, as a person counts it: `running` is the only
/// phase in which the job's steps are actually executing.
///
/// `admitted` alone is not "running": an admitted claim may still be booting its
/// runner or waiting for GitHub to hand it a job, and a frozen one is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobPhase {
    /// Parked: not admitted yet (`waiting` says which gate holds it).
    Queued,
    /// Admitted, but its runner has not been seen working yet: still booting,
    /// registering, or online and idle, waiting for GitHub to hand it a job.
    Starting,
    /// Its runner has been seen executing a job: the claim's own job, started on
    /// one of this host's runners, or a sibling GitHub handed the runner instead
    /// (JIT runners are not job-pinned). Latched, so it stays here between steps
    /// and between jobs.
    Running,
    /// Admitted and started (or not), but its containers are paused (ADR-0032).
    Frozen,
}

impl JobPhase {
    pub fn of(admitted: bool, started: bool, frozen: bool) -> Self {
        match (admitted, frozen, started) {
            (false, _, _) => JobPhase::Queued,
            (true, true, _) => JobPhase::Frozen,
            (true, false, true) => JobPhase::Running,
            (true, false, false) => JobPhase::Starting,
        }
    }
}

/// A live provisioning claim. Keyed by `job.id` in [`Orchestrator::inflight`], it
/// dedups a job across cycles *and* carries the handles [`reconcile_claims`] needs
/// to reclaim it when the job it was minted for no longer needs a runner.
struct Claim {
    /// `owner/repo` — needed to fetch this job / this repo's runners for reconcile.
    repo: String,
    /// The profile serving it (the dashboard's [`InflightView`] label).
    profile: String,
    /// Static identity captured at claim time, so the dashboard can show what the
    /// job *is* without re-fetching the run: the PR (same-repo PRs only), head
    /// branch, workflow display name, job name, and triggering event.
    pr: Option<u64>,
    branch: Option<String>,
    workflow: Option<String>,
    job_name: String,
    event: String,
    /// When faber claimed the job (ms), for the dashboard's elapsed readout.
    started_ms: u64,
    /// The live step the job is on, refreshed by [`reconcile_claims`] for admitted
    /// claims from the per-job GitHub poll. A small cell shared with `view`, kept
    /// out of the big `inflight` lock so the reconciler can update it after its
    /// (awaited) job fetch without re-locking the map.
    step: Arc<std::sync::Mutex<Option<StepView>>>,
    /// Latched true by [`reconcile_claims`] the first time this claim's runner is
    /// seen working: GitHub reports the claim's own job past `queued` on one of
    /// this host's runners, or the runner itself reads [`RunnerLife::Busy`] —
    /// whichever job it holds, since a JIT runner is not pinned to the job it was
    /// minted for. [`JobPhase`] reads `running` from then on and does not flicker
    /// back to `starting` in the gap between two steps. Never cleared.
    started: Arc<AtomicBool>,
    /// Cooperative-cancel signal. The reconciler flips it true; the provision task,
    /// on seeing it, exits before spawning (parked) or lets its idle runner be
    /// killed (admitted) — always through its own cleanup path, so a `Claim` stays
    /// owned by exactly one task and the reconciler only ever *signals*.
    cancel: watch::Sender<bool>,
    /// Flipped true by the task once it has admitted: the memory reservation is
    /// debited and a runner container is being stood up. Selects the reconcile
    /// predicate — a *parked* claim keys on its job alone, an *admitted* one on
    /// the job, its runner, and its container together (see [`admitted_verdict`]).
    admitted: Arc<AtomicBool>,
    /// Whether this claim's backend is Docker, i.e. whether a `faber-runner-*`
    /// container is the thing that carries its work. False for a macOS VM's
    /// runner, which must never be judged by a container listing that could not
    /// possibly contain it.
    containerized: bool,
    /// The exact name this claim's runner registers under (`faber-{job_id}-{ts}`),
    /// published by the provision task the moment it mints one — just before the
    /// claim's `admitted` flag flips, so every admitted claim has it. A parked claim
    /// has none: the name is minted at admission rather than when the claim is
    /// taken, because its timestamp is the runner's age (ADR-0018). A small shared
    /// cell like `step`.
    ///
    /// The reconciler finds the runner by this, never by the `faber-{job_id}-`
    /// prefix: a peer instance minting for the same queued job registers under the
    /// same prefix, and one of the two runners being busy said nothing about the
    /// other.
    runner_name: Arc<std::sync::Mutex<Option<String>>>,
    /// Whether faber holds its own record of minting this claim's runner (#105).
    /// Always for a claim this process took. For an adopted one, only when a
    /// `mint` line in the audit log matches the container exactly (see
    /// [`crate::docker::MintedRunner`]).
    ///
    /// A claim without one came off a container whose labels nothing of faber's
    /// vouches for. It is watched, charged, frozen and torn down like any other —
    /// all on the Docker side — but [`reconcile_claims`] asks GitHub nothing about
    /// it and never requests its reclaim: its repo and runner name are only what
    /// the daemon said, and a reclaim of it would be refused anyway.
    own_mint: bool,
    /// The image this claim's runner will run on, once resolved (ADR-0009). A
    /// small shared cell like `step`, for the same reason: the provision task
    /// publishes into it without re-locking the map.
    ///
    /// `None` until the task has resolved and been cleared to mint — so a claim
    /// still *holding* for the pool is invisible to
    /// [`Orchestrator::claim_pool_image`], which is what makes two claims unable
    /// to hold for each other. Also `None` for a macOS VM claim and whenever
    /// ADR-0008 is off, both of which have no per-job image to weigh.
    image: Arc<std::sync::Mutex<Option<PoolImage>>>,
    /// Why this claim is parked, published by its provision task at every wait it
    /// can sit in — the image resolution, the pool hold, admission, VM
    /// admission — and cleared by [`admit_claim`] (issue #66). A small shared cell like
    /// `step`, and kept apart from it: `step` is an admitted job's progress, this
    /// is a parked claim's reason. It keeps time as well, gate by gate, so the
    /// outcome can say which gate held the job longest (issue #85).
    waiting: Arc<std::sync::Mutex<Parking>>,
    /// Whether the critical floor has this claim's containers frozen (ADR-0032
    /// §2), published by [`crate::freeze`] from what Docker reports every tick. A
    /// small shared cell like `step`. `None` on every claim it has not frozen.
    frozen: Arc<std::sync::Mutex<Option<crate::freeze::FrozenView>>>,
    /// Set by [`reconcile_claims`] once GitHub has finished the job this claim's
    /// frozen runner holds ([`frozen_job_over`]), for the critical floor to thaw
    /// it so it can exit (#111). Never cleared, so a job released this way is not
    /// frozen again.
    job_over: Arc<AtomicBool>,
    /// Where this claim's time goes (ADR-0056): faber's own marks, set by the
    /// provision task, and GitHub's times for its job, kept by
    /// [`reconcile_claims`] once its poll sees the job completed. A small shared
    /// cell like `step`.
    footprint: Arc<std::sync::Mutex<Timeline>>,
}

/// A job queued on GitHub that this host's trust gate refuses right now (issue
/// #95). Nothing will ever run it here, so it waits on GitHub with no other
/// trace than a deduped decision in the feed.
#[derive(Debug, Clone, Serialize)]
pub struct RefusedJobView {
    pub repo: String,
    pub job_id: u64,
    pub run_id: u64,
    pub job_name: String,
    pub workflow: String,
    pub event: String,
    /// The verdict label, e.g. `refuse:event:repository_dispatch`.
    pub verdict: String,
    /// When faber first saw this job refused. Kept across cycles.
    pub since_ms: u64,
    /// The job's `runs-on` labels: a runner whose labels cover these can be
    /// handed it, which is what the pre-emptive pause weighs (ADR-0030).
    pub labels: Vec<String>,
    /// Whether the refusal is trust-class ([`gate::classify`]). Only a trust
    /// refusal pauses minting under `enforce`.
    #[serde(skip)]
    pub trust: bool,
}

/// A job that asked for a Docker daemon (`runs-on` includes `dind`) on a profile
/// that does not grant one, so no runner was minted for it (ADR-0026
/// §Consequences). Unlike [`RefusedJobView`] this is not a trust verdict: the
/// fix is the workflow's labels or the profile's `dind` key.
#[derive(Debug, Clone, Serialize)]
pub struct DindRefusedView {
    pub repo: String,
    pub run_id: u64,
    pub job_id: u64,
    pub job_name: String,
    pub profile: String,
    pub reason: String,
    /// When this refusal was last made. A queued job is re-tried on the provision
    /// throttle's cadence, so this keeps moving while the job still waits.
    pub at_ms: u64,
}

/// A job that declared a service (`faber.postgres-16`) this host does not
/// provide, or at a version outside its `versions`, so no runner was minted for
/// it (ADR-0046 §2). Not a trust verdict: the fix is the workflow's labels or the
/// host's `[services.*]` table.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceRefusedView {
    pub repo: String,
    pub run_id: u64,
    pub job_id: u64,
    pub job_name: String,
    pub profile: String,
    /// The service label that was refused, verbatim.
    pub label: String,
    pub reason: String,
    /// When this refusal was last made; refreshed on every re-try.
    pub at_ms: u64,
}

/// A job that ran on a runner this host minted although the gate refuses it — a
/// gate bypass (ADR-0030).
#[derive(Debug, Clone, Serialize)]
pub struct BypassView {
    pub ts_ms: u64,
    pub repo: String,
    pub run_id: u64,
    /// The job that ran.
    pub job_id: u64,
    pub job_name: String,
    pub workflow: String,
    /// The runner it ran on.
    pub runner: String,
    /// The job that runner was minted for.
    pub minted_for: u64,
    pub class: BypassClass,
    /// The verdict label, e.g. `refuse:actor:dependabot[bot]`.
    pub verdict: String,
}

/// A runner this host minted, remembered so bypass detection can tell its own
/// runners from a peer's (ADR-0030). A peer's `faber-*` runner is the peer's to
/// judge: by name it is indistinguishable, so only this record says "ours".
#[derive(Debug, Clone)]
struct Minted {
    /// The job the runner was minted for.
    minted_for: u64,
    /// The profile it was minted under, whose policy re-judges what it ran.
    profile_idx: usize,
    /// When it was minted, so the map can be bounded by [`EXECUTED_TTL`].
    at_ms: u64,
}

/// A snapshot of orchestrator state for the dashboard (`GET /api/state`).
#[derive(Debug, Serialize)]
pub struct StateView {
    pub owner: String,
    pub topic: String,
    pub profiles: Vec<ProfileView>,
    /// The trust boundary in force (issue #42). Host-wide, so it is read off the
    /// first profile — `into_profiles` gives every profile the same one.
    pub gate: Option<crate::gate::Boundary>,
    /// Emergent concurrency: the count of Docker jobs currently admitted. Not a
    /// knob any more — a readout (kept named `concurrency` for panel compatibility).
    pub concurrency: usize,
    /// Paused (ADR-0023): nothing is claimed or started until resumed. Which of
    /// the operator or the janitor paused it, each parked claim's `waiting` says.
    pub paused: bool,
    pub draining: bool,
    pub uptime_s: u64,
    pub audit_path: String,
    pub inflight: Vec<InflightView>,
    pub counts: Counts,
    /// Jobs queued on GitHub and refused by this host's gate *right now*, oldest
    /// first (issue #95). A refused job waits silently — it looks like a slow
    /// deploy — so this is a live set that empties when the job starts, finishes
    /// or is cancelled. `recent` and `counts.refused` are history, not this.
    pub refused_queued: Vec<RefusedJobView>,
    /// Refused jobs beyond [`REFUSED_QUEUED_CAP`] that `refused_queued` does not
    /// list: a current count, not a history.
    pub refused_queued_overflow: usize,
    /// Jobs that asked for `dind` on a profile that does not grant it, oldest
    /// first (ADR-0026 §Consequences). No runner is minted for them, so like
    /// `refused_queued` they wait silently on GitHub; entries age out an hour
    /// after the last refusal.
    pub dind_refused: Vec<DindRefusedView>,
    /// Jobs that declared a service this host does not provide, oldest first
    /// (ADR-0046 §2). No runner is minted for them; entries age out an hour
    /// after the last refusal.
    pub service_refused: Vec<ServiceRefusedView>,
    /// `[host] dind_requires_label`, resolved: `on`, `off` or `unstated`
    /// (ADR-0026 § Consequences). Anything but `on` means every job on a granting
    /// profile gets a daemon, whatever its labels — the dashboard warns.
    pub dind_label_rule: &'static str,
    /// Recent gate bypasses, newest first (ADR-0030): jobs the gate refuses that
    /// ran anyway, on a runner this host minted for another job.
    pub bypasses: Vec<BypassView>,
    /// Trust-class bypasses in the last 7 days. With `gate.on_bypass` at `record`
    /// a non-zero count is the dashboard's "enforcement off" line.
    pub trust_bypasses_7d: usize,
    /// Repos whose minting is held (ADR-0030), each with its reason, who set it
    /// and since when. Cleared only by the operator.
    pub repo_holds: Vec<RepoHold>,
    pub recent: Vec<Decision>,
    /// Recent finished runs, newest first — what actually happened to the jobs
    /// faber took on, and why, when it wasn't a success.
    pub outcomes: Vec<OutcomeEvent>,
    /// Recent faults in faber itself, newest first: the daemon unable to do its
    /// job, bracketed by the recovery that ended it.
    pub faults: Vec<FaultEvent>,
    /// Recent actionable runner-image drift checks, newest first (ADR-0001).
    pub drift: Vec<DriftEvent>,
    /// Recent auto-retry actions, newest first (ADR-0003).
    pub retries: Vec<RetryEvent>,
    /// False when the last reconcile cycle couldn't reach GitHub — i.e. the
    /// daemon can't obtain jobs, so an empty in-flight list is a fault, not idle.
    /// A deliberate pause or drain stays healthy (it isn't *trying* to poll), and
    /// so does a hold (`budget.hold`), which is just as deliberate.
    pub healthy: bool,
    /// The error from the last failed cycle, if any (diagnostics / tooltips). When
    /// the cycle itself is fine but a profile image is missing, it names that
    /// instead, so a tray that predates `missing_images` still says why.
    pub last_error: Option<String>,
    /// Docker profiles whose own image is confirmed missing from this host (#63).
    /// Any entry makes `healthy` false: claims for that profile are parked and no
    /// runner is minted for them until the image is back.
    pub missing_images: Vec<MissingImageView>,
    /// Profiles whose runner GitHub has announced it will retire (#64). Unlike
    /// `missing_images` this leaves `healthy` alone: jobs still run, until the
    /// cutoff, and the entry is only how the operator hears of it in time.
    pub runner_retirements: Vec<RunnerRetirementView>,
    /// Docker profiles whose image would be pulled from a Docker Hub user
    /// namespace only because `[host] pull_unqualified_images` is unstated
    /// (#156). Like `runner_retirements`, this leaves `healthy` alone.
    pub unqualified_images: Vec<UnqualifiedImageView>,
    /// This daemon's instance id (#94), the suffix of every runner it mints.
    pub instance: String,
    /// Other faber instances seen minting into the pools this one serves (#94).
    pub peers: Vec<PeerView>,
    /// The autolimiter snapshot: budget, live availability, ledger, per-class caps,
    /// and pending warnings — everything the panel needs to show and tune it.
    pub budget: LimiterView,
    /// Autoupdate status (ADR-0006): which faber this is, what release is
    /// available, and where an in-progress update has got to.
    ///
    /// Filled in by the web layer, which owns the updater handle — the
    /// orchestrator knows nothing about updates and leaves this at its default.
    /// That keeps the whole feature out of this file, which has enough to do.
    pub update: crate::update::UpdateView,
    /// Whether faber is holding off idle sleep for its running jobs (issue #90,
    /// `[host] prevent_sleep`).
    pub awake: crate::awake::AwakeView,
    /// Standing conditions of this boot, shown as dashboard banners until the
    /// daemon restarts (ADR-0025): a host still on the LEGACY layout, an
    /// untrusted image recipe. Decided once, at boot, and filled in by
    /// the web layer like `update`; the orchestrator leaves it empty.
    pub notices: Vec<Notice>,
    /// Each git mirror's size, last refresh and last refresh error (ADR-0035 §5).
    /// Empty with mirrors off. Additive: older clients ignore it.
    pub git_mirrors: Vec<crate::git_mirror::MirrorView>,
    /// The pull-through mirror's state and its attention rows (docs/mirrors.md).
    /// `null` with `[mirror]` absent. Additive: older clients ignore it.
    pub mirror: Option<crate::mirror::MirrorView>,
    /// The GitHub credential (ADR-0027): whether it is usable, whose it is, when
    /// it expires, and what it lacks where. Never the token.
    pub credential: crate::credential::CredentialView,
    /// The macOS VM jobs running now, each with its typed step (ADR-0026).
    /// Always in the JSON, empty on a host with no `macos-vm` profile. Additive:
    /// older clients ignore it.
    pub vm_jobs: Vec<crate::vm::job::VmJobView>,
    /// The root-owned CLI copy's version and whether it is behind this daemon
    /// (ADR-0034), read from its bytes, never run. `null` under LEGACY or when
    /// the copy cannot be read. Filled in by the web layer like `update`.
    /// Additive: older clients ignore it.
    pub cli_copy: Option<crate::upgrade_cli::CliCopyView>,
    /// Every trust-widening change allowed in the last 7 days, newest first
    /// (ADR-0028 addendum B). Filled in by the web layer like `notices`; the
    /// orchestrator leaves it empty. Additive: older clients ignore it.
    pub trust_changes: Vec<crate::audit::TrustChange>,
    /// The operator's dismissed attention rows and trust acknowledgement (ADR
    /// a-warning-that-does-not-stop-jobs-can-be-dismissed, ADR-0064 §3). Filled
    /// in by the web layer like `trust_changes`. Additive: older clients ignore it.
    pub attention: crate::attention::AttentionView,
}

/// One standing condition of this boot, for a dashboard banner (ADR-0025).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    /// What it is about, stable across releases: `legacy_layout`,
    /// `recipe_untrusted`, `registry_auth_held`. (`native_ignored` is gone since ADR-0052 made a
    /// native profile a config error.)
    pub kind: String,
    /// The sentence to show, written by faber.
    pub text: String,
}

/// Who is draining faber. Either way nothing new is claimed and parked claims
/// are cancelled; the two differ only in who lifts the drain. faber drains while
/// either holds it, so ending one never ends the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// A drain that ends with the process: `faber drain` (ADR-0011 §5), or a
    /// shutdown that cannot hand its claims to the next process. Never lifted.
    Stop,
    /// The updater's escalation (ADR-0006). The updater lifts it when the install
    /// it drained for does not happen, which otherwise left a follower claiming
    /// nothing until its next restart.
    Update,
}

impl Drain {
    /// This holder's bit in [`Orchestrator`]'s `draining`.
    fn bit(self) -> u8 {
        match self {
            Drain::Stop => 1,
            Drain::Update => 2,
        }
    }
}

/// Shared orchestrator state. Held behind an `Arc` so the reconcile loop, the
/// provision tasks, and the web server all share one instance.
pub struct Orchestrator {
    gh: Arc<Github>,
    cfg: Config,
    owner: String,
    /// Job ids currently being provisioned → the [`Claim`] serving each. Also the
    /// claim set that dedups a job across cycles and that [`reconcile_claims`]
    /// reconciles against GitHub.
    inflight: Mutex<HashMap<u64, Claim>>,
    /// Per-job "first observed doing no work" timestamps (ms) for admitted claims,
    /// driving the [`IDLE_RECLAIM_GRACE`] window (see [`admitted_verdict`]). Cleared
    /// whenever the claim looks live *or* a signal can't be read, so the window only
    /// ever accumulates confirmed deadness. Set/cleared only by [`reconcile_claims`];
    /// an entry is dropped when its claim is released ([`Orchestrator::release_claim`]).
    idle_since: Mutex<HashMap<u64, u64>>,
    /// Short-lived memory of `(run_id, run_attempt)` pairs the auto-retry sweep has
    /// finished with — either re-run, or examined and found terminally genuine — so
    /// neither is redone every cycle while the run sits in the lookback window
    /// (ADR-0003). The durable cap on re-runs is the run's own `run_attempt`.
    retried: Mutex<RetryMemory>,
    /// How faber's runners ended, indexed by runner name — bounded, and the only
    /// evidence the auto-retry sweep acts on (ADR-0003, amendment 2026-10-18). It
    /// mirrors the audit log: written alongside each `outcome` and `freeze` line by
    /// [`Orchestrator::record_outcome`] and [`Orchestrator::record_freeze`], and
    /// rebuilt from the log at startup. Read by [`reconcile_retries`].
    ends: Mutex<EndLedger>,
    /// Runner name → the job GitHub actually dispatched to it, harvested from the
    /// discovery scan (a job carries its `runner_name` from the moment it starts).
    /// The autolimiter's per-class learning keys on this, not on the job the runner
    /// was minted for — see [`ExecutedJob`].
    executed: Mutex<HashMap<String, ExecutedJob>>,
    /// repo → profile index → class → when that class was last seen queued.
    ///
    /// The classes a runner minted for that profile could be handed. Accumulated
    /// across discovery scans and read at launch time, so a runner is sized for the
    /// whole set GitHub may dispatch to it rather than for the one job it was minted
    /// for (issue #13). See [`merge_cohorts`] for why this accumulates rather than
    /// holding only what is queued right now.
    cohorts: Mutex<HashMap<String, CohortSightings>>,
    /// The autolimiter: memory-budget admission + per-class cap learning for
    /// Docker jobs. Concurrency is emergent from the budget, not a fixed count.
    limiter: Arc<Limiter>,
    /// Who is draining, one [`Drain::bit`] per holder; zero when nobody is. While
    /// any bit is set no new job is accepted, and in-flight runners finish.
    draining: AtomicU8,
    wake: Notify,
    /// Pulsed whenever a provision task removes itself from `inflight`, so
    /// [`Orchestrator::await_drain`] can re-check whether the box is now empty.
    runner_done: Notify,
    metrics: Mutex<Metrics>,
    audit: Audit,
    /// The waste check's reads, each cached for a minute (ADR-0063 §2).
    waste: crate::waste::Checker,
    started_ms: u64,
    /// Health of the reconcile pipeline: `None` once the last cycle reached
    /// GitHub cleanly; `Some(err)` when discovery/polling failed. Lets the view
    /// distinguish "idle — no work" from "idle — can't obtain jobs".
    health: Mutex<Option<String>>,
    /// Docker custody bookkeeping (ADR-0007, ADR-0024 §2): when the daemon first
    /// stopped answering, what faber has already tried about it, and how long it
    /// has answered since.
    docker_custody: Mutex<host::DockerCustody>,
    /// The runner-image variant cache (ADR-0008): per-version images built on
    /// demand and kept while hot. Inert unless the operator turned it on.
    images: Arc<VariantCache>,
    /// Runner name → what this host minted it for (ADR-0030). Written at
    /// admission and adoption, bounded by [`EXECUTED_TTL`].
    minted: std::sync::Mutex<HashMap<String, Minted>>,
    /// Per-repo holds on minting, by repo (ADR-0030). Restored from and saved to
    /// the overrides file.
    repo_holds: std::sync::Mutex<BTreeMap<String, RepoHold>>,
    /// Repos whose cache volume an enforced trust bypass condemned, still to be
    /// deleted once nothing mounts it (ADR-0030).
    cache_purges: std::sync::Mutex<BTreeSet<String>>,
    /// Which Docker profiles' own images are confirmed missing from this host
    /// (#63). Refreshed once a cycle by [`reconcile_profile_images`]; read by every
    /// claim about to mint, so the check costs one inspect per profile per cycle
    /// rather than one per claim.
    profile_images: std::sync::Mutex<ProfileImages>,
    /// What GitHub's runner-deprecation annotations said about each profile's
    /// runner (#64), and when each profile's finished jobs were last sampled.
    retirements: std::sync::Mutex<Retirements>,
    /// Set while a [`reconcile_verdicts`] batch runs off the loop, so a slow GitHub
    /// never stacks a second batch behind the first (ADR-0044).
    verdicts_running: Arc<AtomicBool>,
    /// How long after a run ends its one early verdict fetch fires, in ms; 0 is
    /// off (ADR-0044, amended 2026-10-19). [`VERDICT_EARLY`] in a daemon; off in
    /// the tests that do not ask for it, which must not reach GitHub.
    verdict_early_ms: AtomicU64,
    /// This daemon's instance id (#94): the suffix of every runner name it mints
    /// and its `faber-host-<id>` label. Fixed for the process's life.
    instance: String,
    /// The other faber instances seen minting into the pools this one serves
    /// (#94), and which of their lost runners have been recorded.
    peers: std::sync::Mutex<Peers>,
    /// Whether repo holds are written to the overrides file. Off only in tests,
    /// which must never touch the operator's real one.
    persist_holds: bool,
    /// The idle-sleep assertion's state, as [`crate::awake::run_loop`] last
    /// published it (issue #90).
    awake: std::sync::Mutex<crate::awake::AwakeView>,
    /// When the token's permissions and repositories were last checked, and a
    /// rejected token last re-tried (ADR-0027).
    credential_checks: std::sync::Mutex<CredentialChecks>,
}

/// When [`reconcile_credential`] last asked GitHub about the token.
#[derive(Debug, Default)]
struct CredentialChecks {
    /// The last permission probe and repository comparison, epoch ms. `None`
    /// until the first, which runs on the first cycle.
    probed_ms: Option<u64>,
    /// The last time a token GitHub rejected was tried again, epoch ms.
    rejected_retry_ms: u64,
    /// The mismatches last logged, so a standing one is logged once.
    logged: Mismatches,
}

/// How often the token's read permissions are probed per served repository,
/// and its repositories compared with the opted-in ones (ADR-0027).
const PERMISSION_PROBE_MS: u64 = 60 * 60 * 1000;

/// How often a token GitHub rejected is tried again when nothing on this host
/// changed. Rarely: GitHub answers a run of bad credentials by refusing every
/// authentication for the account for a while, the operator's own included.
const REJECTED_RETRY_MS: u64 = 10 * 60 * 1000;

/// The most served repositories one probe covers, so a large topic cannot turn
/// the hourly probe into a burst.
const PROBE_REPOS: usize = 50;

impl Orchestrator {
    /// Build the orchestrator for the account `owner` and open the audit log.
    ///
    /// `owner` is the GitHub login the boot already resolved, as one of the
    /// prerequisites [`crate::standby`] waits for rather than exiting over
    /// (ADR-0025). Asking GitHub here instead made a login failure fatal, and
    /// under launchd's `KeepAlive` a fatal boot is a crash loop.
    pub async fn new(gh: Arc<Github>, owner: String, cfg: Config) -> anyhow::Result<Arc<Self>> {
        let audit = Audit::open()?;
        // Every write faber makes with the token is recorded (ADR-0027), in the
        // client itself, so no write can be added that skips it.
        gh.set_audit(audit.clone());
        // Which instance this is (#94). A configured name `check-config` would
        // refuse is warned about and replaced, never fatal to the boot.
        let (own_instance, bad_name) = instance::resolve(&cfg.host.instance, &instance::hostname());
        if let Some(w) = bad_name {
            warn!("{w}");
        }
        info!("instance id {own_instance}: runners are named faber-<job>-<ts>-{own_instance}");
        // Build the autolimiter: seed operator pins, rebuild learned caps from the
        // audit log (so a restart doesn't re-profile everything), prime the budget
        // from a VM probe, and start the memory-tick.
        let limiter = Limiter::new(cfg.autolimit.clone(), &cfg.pinned_caps);
        limiter.set_disk_admission(cfg.host.disk_admission());
        // Read before the VM is probed, so the first budget is already planned
        // within the machine's share (ADR-0021 §2b).
        limiter.set_machine_share(cfg.host.machine_share());
        limiter.set_runs_macos_vms(
            cfg.profiles
                .iter()
                .any(|p| matches!(p.backend, Backend::MacosVm(_))),
        );
        limiter.set_vm_layout(cfg.vm_layout);
        limiter.prime_machine().await;
        // Restored, not placed: `since_ms` is when the operator held admission,
        // so "held since" survives the restart along with the hold itself.
        if let Some(hold) = &cfg.hold {
            limiter.hold(hold.reason.clone(), hold.since_ms);
            warn!("{}", hold_warning(hold));
        }
        let replayed = audit.replay_outcomes();
        seed_limiter(&limiter, &replayed);
        // ADR-0033 §4: the p75 fallback's trips, so a restart neither forgets a
        // cool-off nor resets its doubling.
        limiter.restore_reserve(&audit.read_kind(crate::reserve::AUDIT_KIND));
        if let Some(v) = cfg.autolimit.reserve.unknown() {
            warn!(
                "autolimit.reserve = {v:?} is not \"max\" or \"p75\" — reserving as under \"max\"; \
                 run `faber check-config` to fix the file (ADR-0033 §2)"
            );
        }
        // Rebuild the sizing cohorts from the same history, so a restart doesn't
        // reopen the gap this whole mechanism exists to close.
        let cohorts = seeded_cohorts(&cfg.profiles, &replayed, now_ms());
        // Only a Docker profile needs the VM-memory probes; a macOS-VM-only
        // deployment may not even have Docker running, so don't spin failing
        // `docker` calls at it.
        if cfg
            .profiles
            .iter()
            .any(|p| matches!(p.backend, Backend::Docker(_)))
        {
            if let Some(msg) = cpu_unstated_warning(&cfg.profiles, cfg.autolimit.cpu.mode) {
                warn!("{msg}");
            }
            for msg in disk_admission_warnings(&cfg.profiles, &cfg.host) {
                warn!("{msg}");
            }
            if let Some(msg) = dind_label_rule_warning(&cfg.profiles, &cfg.host) {
                warn!("{msg}");
            }
            for u in unqualified_images(&cfg.profiles, &cfg.host) {
                warn!("{}", u.message);
            }
            // ADR-0021 §3: the co-tenants' peak from before the restart, so a
            // declared share starts from the day's peak rather than at
            // `reserve_mib` while it relearns it.
            if let Some(dir) = crate::platform::state_dir() {
                limiter.restore_cotenant(dir.join(crate::cotenant::STATE_FILE));
            }
            limiter.prime_budget().await;
            tokio::spawn(limiter.clone().probe_loop());
            tokio::spawn(limiter.clone().cotenant_loop());
            // The disk gate's reading (ADR-0024), on a loop of its own: it must go
            // on measuring while the probe loop's Docker calls are failing.
            tokio::spawn(limiter.clone().disk_loop());
        }
        // The machine itself (ADR-0021), on every host: its pressure gate holds
        // macOS VM jobs too. Warned about after the VM is measured, so an unstated
        // share's warning carries the split it is about.
        for msg in limiter.machine_warnings() {
            warn!("{msg}");
        }
        tokio::spawn(limiter.clone().machine_loop());

        // ADR-0035: the git mirrors, when a docker profile asks for them. The
        // refresh runs on the first such profile's runner image, which has git
        // because `actions/checkout` needs it. Beside dind, boot has already
        // taken `git_mirror` off every profile (`mirror_beside_dind`).
        if let Some(image) = git_mirror_image(&cfg.profiles) {
            crate::git_mirror::global().configure(crate::git_mirror::Ctx {
                gh: gh.clone(),
                image,
                floor_gib: crate::git_mirror::clone_floor_gib(
                    cfg.host.disk_floor_gib,
                    cfg.host.disk_admission_floor_gib,
                ),
            });
        }

        // The pull-through mirror, only when `[mirror]` is present and valid.
        // Without it nothing is configured, and the launch path, teardown and the
        // janitor never reach Docker on its behalf.
        if let Some(policy) = &cfg.mirror {
            crate::mirror::global().configure(policy.clone());
        }

        // ADR-0046: declared services, only when the operator wrote a
        // `[services.*]` table. Without one nothing is configured, and the
        // provisioning path never reaches the services (§9).
        if !cfg.services.is_empty() {
            crate::services::global().configure(crate::services::Ctx {
                policy: cfg.services.clone(),
                limiter: limiter.clone(),
                pull_unqualified: cfg.host.pull_unqualified_images,
            });
        }

        let images = Arc::new(
            VariantCache::new(
                cfg.runner_image.clone(),
                VariantDisk::new(
                    cfg.host.disk_floor_gib,
                    cfg.host.disk_admission().floor_gib(),
                ),
            )
            // ADR-0010 §4: the operator's ceiling travels with the thing that
            // builds, so no future call path can start a build around it.
            .with_needs(cfg.needs.clone()),
        );
        if images.enabled() {
            info!(
                "runner images: building per-version variants on demand, keeping {} warm \
                 (ADR-0008)",
                cfg.runner_image.keep
            );
        }

        // ADR-0030: the bypasses of the last week, from the audit log, so the
        // record-once dedup and the "enforcement off" count survive a restart —
        // and so a host still recording says so at every boot, not only once it
        // next sees one.
        let mut metrics = Metrics::default();
        let since = now_ms().saturating_sub(BYPASS_WINDOW.as_millis() as u64);
        let week = audit.read_since(since);
        for (job_id, ts, class) in replayed_bypasses(&week) {
            metrics.bypassed.insert(job_id, (ts, class));
        }
        for (job_id, ts) in replayed_kills(&week) {
            metrics.killed.insert(job_id, ts);
        }
        metrics.restore_open_faults(&week);
        let on_bypass = cfg
            .profiles
            .first()
            .map(|p| p.policy.on_bypass())
            .unwrap_or_default();
        if let Some(msg) = bypass_record_warning(on_bypass, metrics.trust_bypasses_since(now_ms()))
        {
            warn!("{msg}");
        }
        let mut repo_holds = BTreeMap::new();
        let mut cache_purges = BTreeSet::new();
        for h in &cfg.repo_holds {
            warn!("{}", repo_hold_warning(h));
            // A hold enforcement set may have outlived its cache deletion (a restart
            // between the two). Deleting again is harmless: a held repo mints
            // nothing, so nothing recreates the volume, and an absent one is done.
            if h.by == BYPASS_HOLD_BY {
                cache_purges.insert(h.repo.clone());
            }
            repo_holds.insert(h.repo.clone(), h.clone());
        }

        // The auto-retry sweep's evidence survives a restart, which is the commonest
        // way a runner is lost: rebuilt from the same week of the log.
        let ends = retry::ledger_from_audit(
            &week,
            now_ms().saturating_sub(2 * cfg.retry.lookback().as_millis() as u64),
        );
        let cfg_awake = cfg.host.sleep_policy();
        Ok(Arc::new(Self {
            gh,
            cfg,
            owner,
            inflight: Mutex::new(HashMap::new()),
            idle_since: Mutex::new(HashMap::new()),
            retried: Mutex::new(RetryMemory::default()),
            ends: Mutex::new(ends),
            executed: Mutex::new(HashMap::new()),
            cohorts: Mutex::new(cohorts),
            limiter,
            draining: AtomicU8::new(0),
            wake: Notify::new(),
            runner_done: Notify::new(),
            metrics: Mutex::new(metrics),
            audit,
            waste: Default::default(),
            started_ms: now_ms(),
            health: Mutex::new(None),
            // Who restarts Docker here (ADR-0025, ADR-0026): faber's own Linux VM
            // on a set-up Mac, the service on a LEGACY Linux host, and nothing
            // where the daemon faber cannot reach is not its own.
            docker_custody: Mutex::new(host::DockerCustody::new(host::Custodian::for_this_host())),
            images,
            minted: std::sync::Mutex::new(HashMap::new()),
            repo_holds: std::sync::Mutex::new(repo_holds),
            cache_purges: std::sync::Mutex::new(cache_purges),
            profile_images: std::sync::Mutex::new(ProfileImages::default()),
            retirements: std::sync::Mutex::new(Retirements::default()),
            verdicts_running: Arc::new(AtomicBool::new(false)),
            verdict_early_ms: AtomicU64::new(VERDICT_EARLY.as_millis() as u64),
            instance: own_instance,
            peers: std::sync::Mutex::new(Peers::default()),
            persist_holds: true,
            awake: std::sync::Mutex::new(crate::awake::AwakeView::booted(cfg_awake)),
            credential_checks: std::sync::Mutex::new(CredentialChecks::default()),
        }))
    }

    // --- learning attribution ----------------------------------------------

    /// Record which job GitHub dispatched to each of faber's runners, from the jobs
    /// this cycle's discovery scan already fetched.
    async fn harvest_executed(&self, runs: &[(WorkflowRun, Vec<Job>)], now: u64) {
        harvest_executed(&mut *self.executed.lock().await, runs, now);
    }

    /// The class a finished runner's measurements belong to: the job GitHub actually
    /// dispatched to it. `None` when that is unknown — the run is then kept out of
    /// the learner entirely rather than attributed to a guess.
    ///
    /// The map is fed by the discovery scan, which a short job can beat: a runner
    /// that took a job and exited between two scans is not in it yet, and a run
    /// that went from queued to completed in that gap never reaches the scan at
    /// all (it lists active runs only). So a miss asks GitHub itself, once, over
    /// the runs that changed since `since_ms` (the runner's admission).
    async fn executed_class(&self, repo: &str, runner: &str, since_ms: u64) -> Option<Attribution> {
        if let Some(found) = executed_class(&*self.executed.lock().await, repo, runner) {
            return Some(found);
        }
        let runs = match self.runs_changed_since(repo, since_ms).await {
            Ok(runs) => runs,
            Err(e) => {
                warn!("runner {runner}: cannot ask GitHub which job it ran: {e}");
                return None;
            }
        };
        let mut map = self.executed.lock().await;
        harvest_executed(&mut map, &runs, now_ms());
        executed_class(&map, repo, runner)
    }

    /// The runs (with their jobs) GitHub last changed at or after `since_ms`, newest
    /// first and at most [`EXECUTED_LOOKUP_RUNS`] of them, for a direct
    /// [`Self::executed_class`] lookup. A job starting or finishing updates its run,
    /// so a run untouched since the runner was admitted cannot hold its job.
    async fn runs_changed_since(
        &self,
        repo: &str,
        since_ms: u64,
    ) -> Result<Vec<(WorkflowRun, Vec<Job>)>, GithubError> {
        let floor = since_ms.saturating_sub(EXECUTED_LOOKUP_SLACK.as_millis() as u64);
        let mut runs: Vec<WorkflowRun> = Vec::new();
        for status in ACTIVE_STATUSES.iter().chain(&["completed"]) {
            runs.extend(self.gh.runs_with_status(repo, status).await?);
        }
        let mut ids = HashSet::new();
        runs.retain(|r| {
            ids.insert(r.id)
                && r.updated_at
                    .as_deref()
                    .and_then(crate::retry::parse_rfc3339_to_ms)
                    .is_some_and(|at| at >= floor)
        });
        runs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        runs.truncate(EXECUTED_LOOKUP_RUNS);
        let mut out = Vec::with_capacity(runs.len());
        for run in runs {
            let jobs = self.gh.run_jobs(repo, run.id).await?;
            out.push((run, jobs));
        }
        Ok(out)
    }

    /// This host's Docker-custody policy (ADR-0007), for the janitor.
    pub fn host_policy(&self) -> &HostPolicy {
        &self.cfg.host
    }

    /// The runner-image variant cache (ADR-0008), for the janitor's eviction pass.
    pub fn images(&self) -> &Arc<VariantCache> {
        &self.images
    }

    /// The images live claims have been cleared to run on (ADR-0008, ADR-0009),
    /// which eviction shields: a parked claim holding one has no container yet,
    /// so the daemon cannot say it is in use.
    pub async fn claimed_images(&self) -> HashSet<String> {
        self.inflight
            .lock()
            .await
            .values()
            .filter_map(|claim| claim.image.lock().unwrap().as_ref().map(|i| i.tag.clone()))
            .collect()
    }

    // --- cohort sizing -----------------------------------------------------

    /// Fold what the latest discovery scan saw into this repo's cohort set.
    async fn set_cohorts(&self, repo: &str, cohorts: HashMap<usize, Vec<String>>, now: u64) {
        merge_cohorts(
            self.cohorts
                .lock()
                .await
                .entry(repo.to_string())
                .or_default(),
            cohorts,
            now,
        );
    }

    /// The classes a runner about to be minted for `profile_idx` in `repo` could be
    /// handed. Empty when the repo hasn't been scanned yet — which degrades to the
    /// old minted-for-class sizing, never to anything smaller.
    async fn cohort(&self, repo: &str, profile_idx: usize) -> Vec<String> {
        self.cohorts
            .lock()
            .await
            .get(repo)
            .and_then(|by_profile| by_profile.get(&profile_idx))
            .map(|classes| classes.keys().cloned().collect())
            .unwrap_or_default()
    }

    // --- controls + views for the web panel --------------------------------

    /// Pause (ADR-0023): claim nothing new, and start nothing already claimed —
    /// no Docker admission, no VM admission — until resumed. Running jobs finish;
    /// parked claims stay parked and say they are paused.
    ///
    /// The flag is the limiter's, so this loop's claiming, admission and the
    /// VM admission read one pause and can never disagree about it. `by` decides
    /// who may lift it (see [`Limiter::pause`]); `true` when this call paused faber.
    pub fn pause(&self, by: Pause) -> bool {
        self.limiter.pause(by)
    }

    /// Lift a pause (see [`Limiter::resume`]): parked claims go at once, and the
    /// next cycle claims again. `true` when this call lifted one.
    pub fn resume(&self, by: Pause) -> bool {
        self.limiter.resume(by)
    }

    pub fn is_paused(&self) -> bool {
        self.limiter.current_pause().is_some()
    }

    /// Wake the loop immediately (force a reconcile / resume).
    pub fn trigger_reconcile(&self) {
        self.wake.notify_one();
    }

    /// The autolimiter, for the web layer (tuning, pinning, snapshot).
    pub fn limiter(&self) -> &Arc<Limiter> {
        &self.limiter
    }

    /// faber's GitHub client: the pull-through mirror's LFS adapter fetches
    /// with its token (ADR-0047 §4), handed over as the git mirror's is.
    pub(crate) fn github(&self) -> &Arc<Github> {
        &self.gh
    }

    /// The instance id this daemon mints under (#94): what tells its own VM
    /// claims from any other's ([`crate::vm::adopt::ours`]).
    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    /// Apply a live tuning change. Each `None` leaves that knob unchanged.
    /// `native_concurrency` is stored and reported, but limits nothing since
    /// ADR-0052 removed the last native job.
    pub fn apply_tuning(
        &self,
        reserve_mib: Option<u64>,
        headroom_mib: Option<u64>,
        margin: Option<f64>,
        native_concurrency: Option<usize>,
        hard_ceiling: Option<crate::config::HardCeiling>,
    ) {
        self.limiter.set_tuning(
            reserve_mib,
            headroom_mib,
            margin,
            native_concurrency,
            hard_ceiling,
        );
    }

    /// A macOS VM job's admission (ADR-0026, #263): the operator's hold, a pause,
    /// the machine's pressure, then the wall, the disk floor, the class cap of two
    /// and the machine share ([`Limiter::vm_admit`]), waited out while the claim is
    /// still parked and published through `waiting`. `None` means the claim was
    /// reclaimed while it waited, and nothing was counted.
    ///
    /// `image` is the job's golden image, which must be verified (ADR-0049 §7).
    async fn vm_slot(
        &self,
        image: Option<&str>,
        cancel: &mut watch::Receiver<bool>,
        waiting: &std::sync::Mutex<Parking>,
    ) -> Option<VmRun> {
        tokio::select! {
            biased;
            _ = wait_cancelled(cancel) => None,
            run = self.limiter.vm_admit(image, |w| publish_wait(waiting, w.view())) => Some(run),
        }
    }

    /// Rebuild this host's in-flight claims from the runner containers already
    /// running on it — ADR-0011.
    ///
    /// Runs once, before the first claim of the process's life and before the
    /// janitor's startup sweep. The ordering is load-bearing in both directions:
    /// admitting anything first would spend a budget whose ledger still reads
    /// zero, and sweeping first would reap the very corpses this turns into
    /// outcome records.
    ///
    /// `None` means Docker could not be read — **not** "nothing is running". The
    /// caller must keep admission shut on `None`, because a faber that cannot
    /// enumerate what is running does not know what memory is already spent.
    /// `Some(n)` is the number of claims rebuilt, `n == 0` included.
    pub async fn adopt(self: &Arc<Self>) -> Option<usize> {
        let scan = crate::docker::scan().await?;
        // ADR-0046 §7: every runner container this scan found keeps its database,
        // whether it is adopted, finishing unwatched under a profile this config
        // dropped, or opaque. Only a runner that is gone loses its role: dropping
        // one under a live job fails the job, and keeping one too long costs a
        // role until the next restart.
        let live = live_claim_ids(&scan);
        let adopted = self
            .adopt_scan(scan, |orch, a, minted, profile_idx, guard| {
                spawn_adopted(orch, a, minted, profile_idx, guard);
            })
            .await;
        // Unconditionally: with `[services]` removed, a server left from the last
        // run must still be stopped once unused, and its volume removed.
        crate::services::global().adopt(&live).await;
        Some(adopted)
    }

    /// ADR-0051: one pass over the runner containers a switched-off Linux VM still
    /// runs (`[linux_vm] enabled = false`), for [`crate::vm::engine::keep_off`].
    /// Their docker profiles are gone from this boot, so [`Self::adopt`] never ran
    /// for them and no claim watches them; this ends them as a watched claim would
    /// end. An idle runner is withdrawn and removed now, as a reclaim does
    /// (ADR-0017); any runner is removed at its lifetime cap, `runner_timeout`
    /// from its own start, as an adopted one is (ADR-0011); a busy one is left to
    /// finish. Its GitHub registration is touched only when faber's own mint
    /// record vouches for it (#105).
    ///
    /// `None` when Docker could not be read, which is not "nothing is left".
    pub async fn retire_unwatched(
        self: &Arc<Self>,
        runner_timeout: Duration,
    ) -> Option<crate::vm::engine::Left> {
        let scan = crate::docker::scan().await?;
        let since = scan
            .adoptable
            .iter()
            .filter_map(|a| faber_mint_ts(&a.registration()))
            .min()
            .unwrap_or(0)
            .saturating_sub(MINT_RECORD_SLACK_MS);
        let records = mint_records(&self.audit.read_since(since));
        let now = now_ms();
        let passed = |d: Option<u64>| d.is_some_and(|d| now >= d);
        let mut left = crate::vm::engine::Left::default();
        for a in scan.adoptable.iter().filter(|a| a.running) {
            let deadline = crate::docker::cap_deadline_ms(a.started_ms, runner_timeout);
            let minted = vouched_mint(&records, a);
            if !crate::docker::retire_unwatched(&self.gh, a, minted, passed(deadline)).await {
                left.add(deadline);
            }
        }
        for o in &scan.opaque {
            let deadline = o
                .started_ms
                .and_then(|s| crate::docker::cap_deadline_ms(s, runner_timeout));
            if !crate::docker::retire_opaque(o, passed(deadline)).await {
                left.add(deadline);
            }
        }
        Some(left)
    }

    /// [`Self::adopt`] past its read of Docker: rebuild a claim from each container
    /// `scan` found, and hand it to `spawn` to watch. A seam, so a test sees what
    /// every container is adopted with — its mint record above all (#105) —
    /// without a daemon to watch it on.
    async fn adopt_scan(
        self: &Arc<Self>,
        scan: crate::docker::Scan,
        mut spawn: impl FnMut(
            Arc<Self>,
            crate::docker::Adopted,
            Option<crate::docker::MintedRunner>,
            usize,
            crate::limiter::AdmissionGuard,
        ),
    ) -> usize {
        if scan.adoptable.is_empty() && scan.opaque.is_empty() {
            return 0;
        }
        // #105: which of these containers faber's own records say it minted. Read
        // from the audit log back to the oldest mint any of them claims, which is
        // the only window a record of theirs can be in.
        let since = scan
            .adoptable
            .iter()
            .filter_map(|a| faber_mint_ts(&a.registration()))
            .min()
            .unwrap_or(0)
            .saturating_sub(MINT_RECORD_SLACK_MS);
        let records = mint_records(&self.audit.read_since(since));
        let mut adopted = 0usize;
        for a in scan.adoptable {
            let Some(profile_idx) = self.cfg.profiles.iter().position(|p| p.name == a.profile)
            else {
                // The profile that minted it is gone from this binary's config.
                // Leave the container alone rather than guess a backend for it:
                // it still finishes its job, and the janitor reaps it after.
                warn!(
                    "adopt: {} ran under profile '{}', which this config no longer defines — \
                     letting it finish unwatched",
                    a.runner(),
                    a.profile
                );
                // Whether it had a daemon went with the profile; assume it did, so
                // a CPU estimate errs toward charging too much rather than too little.
                self.account_for(a.runner().to_string(), self.adopted_reservation(&a, true));
                continue;
            };
            // The profile's grant, not the job's (ADR-0026): `Adopted` records no
            // per-job dind fact, so a daemonless job on a granting profile is
            // charged as if it had one — over-charging, the safe direction.
            let dind = match &self.cfg.profiles[profile_idx].backend {
                Backend::Docker(d) => d.dind.granted(),
                Backend::MacosVm(_) => false,
            };
            let reservation = self.adopted_reservation(&a, dind);
            let minted = vouched_mint(&records, &a);
            if minted.is_none() {
                warn!(
                    "adopt: {} has no mint record of faber's own in the audit log — watching \
                     and tearing it down, but leaving its GitHub registration alone",
                    a.runner()
                );
            }
            // Everything the display needs is already in the class key
            // (`repo::workflow_id::job_name`) and in what the limiter has already
            // learned about that class — so adoption asks GitHub nothing at all,
            // which is what lets it run before the network is known to work.
            let job_name = a.class.rsplit("::").next().unwrap_or_default().to_string();
            let known_label = self.limiter.label_for(&a.class);
            let workflow = known_label
                .as_deref()
                .and_then(|l| l.split(" · ").nth(1))
                .map(|w| w.to_string());
            {
                let mut map = self.inflight.lock().await;
                if map.contains_key(&a.job_id) {
                    // Two containers for one job: a re-provision left an older one
                    // behind. Only the first is adopted — a job has one claim — and
                    // the janitor reaps the other. Said out loud because a silent
                    // skip here is indistinguishable from a container that was
                    // never seen at all.
                    warn!(
                        "adopt: {} is a second container for job {} — accounting for it \
                         and leaving it to the janitor",
                        a.runner(),
                        a.job_id
                    );
                    drop(map);
                    self.account_for(a.runner().to_string(), reservation);
                    continue;
                }
                let (cancel, _rx) = watch::channel(false);
                map.insert(
                    a.job_id,
                    Claim {
                        repo: a.repo.clone(),
                        profile: a.profile.clone(),
                        // Never reached a label: presentational only, and re-fetching
                        // them would make adoption depend on GitHub at the one moment
                        // it must not.
                        pr: None,
                        branch: None,
                        workflow,
                        job_name,
                        event: String::new(),
                        // From the container's own StartedAt, so the dashboard's
                        // elapsed readout does not restart with the daemon.
                        started_ms: a.started_ms,
                        step: Arc::new(std::sync::Mutex::new(None)),
                        started: Arc::new(AtomicBool::new(false)),
                        cancel,
                        // Adopted claims are admitted by construction: a container
                        // exists, so the reconciler must judge this claim on the
                        // job, the runner and the container together.
                        admitted: Arc::new(AtomicBool::new(true)),
                        containerized: true,
                        runner_name: Arc::new(std::sync::Mutex::new(Some(a.registration()))),
                        own_mint: minted.is_some(),
                        // Republished with what it *serves*, not merely its tag —
                        // an unknown image is filtered straight back out of the
                        // pool, which would leave this runner live and invisible.
                        image: Arc::new(std::sync::Mutex::new(Some(PoolImage {
                            tag: a.image.clone(),
                            serves: a.serves.iter().cloned().collect(),
                        }))),
                        // Admitted, so waiting on nothing.
                        waiting: Arc::default(),
                        // A container a previous process froze is found frozen on
                        // the critical floor's next tick, which reads Docker
                        // rather than anything remembered (ADR-0032 §2).
                        frozen: Arc::new(std::sync::Mutex::new(None)),
                        job_over: Arc::new(AtomicBool::new(false)),
                        footprint: Arc::default(),
                    },
                );
            }
            // Charged unconditionally: the container holds this memory whatever the
            // ledger says, and a gate here would free nothing.
            let guard = self.limiter.adopt(reservation);
            info!(
                "adopt: {} — {} job {} ({} MiB / {} cores reserved{}, running {}, started {}s ago)",
                a.runner(),
                a.repo,
                a.job_id,
                reservation.mib,
                fmt_cores(reservation.mcpu),
                if a.reserved_mcpu.is_none() {
                    " — cores estimated from its class, minted before ADR-0019"
                } else {
                    ""
                },
                a.running,
                now_ms().saturating_sub(a.started_ms) / 1000,
            );
            // Noted whether or not a record vouches for it, so a gate bypass on a
            // runner adopted across a restart is still caught (ADR-0030). That is
            // safe on a name the daemon reported: every answer to a bypass is on
            // this host — a kill of its containers, a hold on minting, a cache
            // deletion — and none of them writes to GitHub.
            self.note_minted(&a.registration(), a.job_id, profile_idx);
            spawn(self.clone(), a, minted, profile_idx, guard);
            adopted += 1;
        }
        // Containers faber owns but cannot rebuild: minted before ADR-0011, or
        // labelled in a way this binary cannot read. They are not claims and get
        // no outcome record — but they are holding memory, and a ledger that
        // ignores them will admit new work on top of them. This is the whole of
        // the migration window's safety, and it is not migration-specific.
        for o in scan.opaque {
            warn!(
                "adopt: {} cannot be rebuilt ({}) — charging its {} MiB to the ledger \
                 and letting it finish unwatched",
                o.name, o.reason, o.mem_mib
            );
            // No CPU charge: an opaque container carries no share to read back, and
            // its quota (if any) is not one. CPU is compressible, and whatever it is
            // burning shows up in the VM's CPU pressure, which CPU Gate 2 watches
            // precisely for load the ledger cannot name.
            self.account_for(
                o.name,
                Reservation {
                    mib: o.mem_mib,
                    mcpu: 0,
                    // Charged no disk: it is not an admission (ADR-0024 §1).
                    disk_mib: 0,
                    // No class, so no p75 figure: the shadow total counts it whole.
                    shadow_mib: o.mem_mib,
                },
            );
        }
        adopted
    }

    /// What an adopted container is charged: the memory its claim label records,
    /// and the CPU its ADR-0019 label records — or, on a container minted before
    /// that label existed, its class's current share (nothing for a class this
    /// process has never learned). No disk: an adoption is not an admission, and
    /// how far into its first minutes the container is, nothing here knows
    /// (ADR-0024 §1).
    fn adopted_reservation(&self, a: &crate::docker::Adopted, dind: bool) -> Reservation {
        Reservation {
            mib: a.reserved_mib,
            mcpu: a
                .reserved_mcpu
                .unwrap_or_else(|| self.limiter.known_mcpu_for(&a.class, dind)),
            disk_mib: 0,
            // What the label says was debited; no p75 figure for a container
            // adopted across a restart.
            shadow_mib: a.reserved_mib,
        }
    }

    /// Hold a ledger charge for a container faber owns but does not claim, for
    /// exactly as long as the container holds the memory.
    ///
    /// The invariant this serves is *every faber-owned running container is
    /// charged to the ledger* — which is strictly wider than "every claim is
    /// charged", and is the one that actually keeps the host off its OOM killer.
    /// No outcome record and no teardown: there is no claim to report on, and the
    /// janitor already reaps by ownership.
    fn account_for(self: &Arc<Self>, name: String, reservation: Reservation) {
        let guard = self.limiter.adopt(reservation);
        tokio::spawn(async move {
            let _guard = guard;
            crate::docker::wait_until_gone(&name).await;
            info!(
                "adopt: {name} is gone — releasing its {} MiB / {} cores",
                reservation.mib,
                fmt_cores(reservation.mcpu)
            );
        });
    }

    /// Take back the macOS VMs the supervisor already runs for this host (ADR-0026:
    /// VMs outlive a restart; ADR-0011 for VMs), and rebuild a claim for each job
    /// one of them is running ([`crate::vm::adopt::adopt_at_boot`] decides which).
    ///
    /// Runs once, before the first VM job and the first VM sweep, while
    /// [`crate::vm::job::VmJobs::hold_for_adoption`] keeps new VM jobs out. `None`
    /// means the supervisor could not be read — not "nothing is running" — and the
    /// caller keeps that hold and tries again.
    pub async fn adopt_vms(self: &Arc<Self>) -> Option<usize> {
        let env = crate::vm::job::adopted_env();
        let rebuilt =
            crate::vm::adopt::adopt_at_boot(&env.supervisor, env.jobs, &self.instance).await?;
        let mut claims = 0;
        for r in rebuilt {
            claims += usize::from(self.adopt_vm(r).await);
        }
        Some(claims)
    }

    /// Watch one VM boot adoption found running a job, rebuilding its claim when
    /// faber's own records say what it is. `true` when a claim was rebuilt.
    ///
    /// A VM carries only its claim, so the repo and profile come from the `mint`
    /// line faber wrote when it named the runner (#105), never from the VM. With
    /// no such line (or no `macos-vm` profile it can be pinned to) nothing about
    /// it is known to ask GitHub: it is charged, watched and stopped when its job
    /// ends, but gets no claim — ADR-0011's container it cannot rebuild.
    async fn adopt_vm(self: &Arc<Self>, r: crate::vm::adopt::Rebuilt) -> bool {
        let runner = format!("faber-{}", r.claim.as_str());
        let since = faber_mint_ts(&runner)
            .unwrap_or(0)
            .saturating_sub(MINT_RECORD_SLACK_MS);
        let entries = self.audit.read_since(since);
        let mint = entries
            .iter()
            .rev()
            .find(|e| e.kind == crate::audit::MINT && e.runner.as_deref() == Some(&runner));
        let vm_profiles: Vec<usize> = self
            .cfg
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.backend, Backend::MacosVm(_)))
            .map(|(i, _)| i)
            .collect();
        let named = mint.and_then(|e| e.profile.as_deref()).and_then(|name| {
            vm_profiles
                .iter()
                .copied()
                .find(|&i| self.cfg.profiles[i].name == name)
        });
        let profile_idx = named.or_else(|| (vm_profiles.len() == 1).then(|| vm_profiles[0]));
        let timeouts = profile_idx
            .or(vm_profiles.first().copied())
            .and_then(|i| match &self.cfg.profiles[i].backend {
                Backend::MacosVm(vm) => Some(vm.timeouts),
                _ => None,
            })
            .unwrap_or_default();
        // Charged past every gate: the VM holds its memory and a macOS slot
        // whatever the ledger says, so a new VM job's admission waits behind it.
        let charge = self.limiter.vm_adopt();
        let started_ms = r.started_unix.saturating_mul(1000);

        let rebuilt = match (mint, profile_idx) {
            (Some(e), Some(idx)) => {
                let mut map = self.inflight.lock().await;
                match map.entry(r.job_id) {
                    std::collections::hash_map::Entry::Occupied(_) => false,
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        let (cancel, _rx) = watch::channel(false);
                        slot.insert(Claim {
                            repo: e.repo.clone(),
                            profile: self.cfg.profiles[idx].name.clone(),
                            // Presentational, and re-fetching them would make
                            // adoption depend on GitHub, as for a container.
                            pr: None,
                            branch: None,
                            workflow: None,
                            job_name: String::new(),
                            event: String::new(),
                            started_ms,
                            step: Arc::new(std::sync::Mutex::new(None)),
                            started: Arc::new(AtomicBool::new(false)),
                            cancel,
                            // A job runs in it: judged on the job and the runner
                            // (it has no container).
                            admitted: Arc::new(AtomicBool::new(true)),
                            containerized: false,
                            runner_name: Arc::new(std::sync::Mutex::new(Some(runner.clone()))),
                            own_mint: true,
                            image: Arc::new(std::sync::Mutex::new(None)),
                            waiting: Arc::default(),
                            frozen: Arc::new(std::sync::Mutex::new(None)),
                            job_over: Arc::new(AtomicBool::new(false)),
                            footprint: Arc::default(),
                        });
                        true
                    }
                }
            }
            _ => false,
        };
        if rebuilt {
            let idx = profile_idx.expect("rebuilt only with a profile");
            self.note_minted(&runner, r.job_id, idx);
            info!(
                "adopt: {runner} — VM in slot {} running job {} (started {}s ago)",
                r.slot.get(),
                r.job_id,
                now_ms().saturating_sub(started_ms) / 1000
            );
        } else {
            warn!(
                "adopt: {runner} has no claim this process can rebuild (no `mint` record \
                 of faber's own names a macos-vm profile for it, or the job is already \
                 claimed) — watching its VM and stopping it when its job ends"
            );
        }

        let orch = self.clone();
        let repo = mint.map(|e| e.repo.clone()).unwrap_or_default();
        // What a reclaim withdraws before it may stop the VM (ADR-0017): the runner
        // the `mint` line found above names, its repo and its exact name (#105).
        let minted = crate::docker::MintedRunner::recorded(&repo, &runner);
        tokio::spawn(async move {
            let env = crate::vm::job::watched_env(&orch.gh, minted);
            let (_keep, never) = watch::channel(false);
            let cancel = if rebuilt {
                let map = orch.inflight.lock().await;
                map.get(&r.job_id).map(|c| c.cancel.subscribe())
            } else {
                None
            }
            .unwrap_or(never);
            let report = match crate::vm::job::watch_adopted(
                &env,
                charge,
                r.claim.clone(),
                r.slot,
                timeouts,
                r.started_unix,
                cancel,
            )
            .await
            {
                Ok(report) => report,
                Err(e) => {
                    error!("adopted VM {runner} failed: {e:#}");
                    RunReport {
                        infra_error: true,
                        cause: Some(format!("{e:#}")),
                        ..Default::default()
                    }
                }
            };
            if !rebuilt {
                return;
            }
            let idx = profile_idx.expect("rebuilt only with a profile");
            orch.record_outcome(
                Finished {
                    repo: &repo,
                    job_id: r.job_id,
                    executed_job_id: None,
                    // No class: nothing about a VM job feeds the learner.
                    class: None,
                    label: None,
                    profile: &orch.cfg.profiles[idx].name,
                    runner: &runner,
                    // The mint record carries no attempt; the first is the honest
                    // default, as nothing here retried it.
                    attempt: 1,
                    wait_ms: 0,
                    parked: &NOT_PARKED,
                    duration_ms: now_ms().saturating_sub(started_ms),
                },
                &report,
            )
            .await;
            orch.release_claim(r.job_id).await;
        });
        rebuilt
    }

    /// Whether this process could exit now and be re-adopted by the next one
    /// (ADR-0011 §2) — the preflight that decides whether a restart has to drain.
    ///
    /// Three conditions, and the reason each is here rather than assumed:
    ///
    /// - **Docker answers.** Adoption reads containers; if the daemon is wedged
    ///   the next process enumerates nothing and admits against an empty ledger.
    ///   The precedent is ADR-0007, where faber admitted several jobs into a dead
    ///   daemon for hours.
    /// - **Every claim is containerized, or in a macOS VM the supervisor answers
    ///   for.** A claim with neither has nothing to rebuild it from (ADR-0011
    ///   §8); none should exist since the native backend went (ADR-0052), and
    ///   one that does still forces a drain. A macOS VM outlives the process and is adopted by its claim (ADR-0026), so it
    ///   needs only the supervisor to answer.
    /// - **Every live runner carries the claim labels.** A container minted by a
    ///   faber older than ADR-0011 has no reservation to re-charge, and adopting
    ///   it on a guessed number would be worse than not adopting it — this is the
    ///   one-time migration window, and it closes itself.
    ///
    /// Says *why* when the answer is no: a redeploy that unexpectedly drains for
    /// hours should not require reading source to find out which condition failed.
    pub async fn adoptable(&self) -> bool {
        if !self.limiter.docker_reachable() {
            info!("preflight: Docker is not answering — a restart would have to drain");
            return false;
        }
        let is_vm = |c: &Claim| {
            self.cfg
                .profiles
                .iter()
                .any(|p| p.name == c.profile && matches!(p.backend, Backend::MacosVm(_)))
        };
        let (bare, vms): (Vec<u64>, usize) = {
            let map = self.inflight.lock().await;
            let bare = map
                .iter()
                .filter(|(_, c)| !c.containerized && !is_vm(c))
                .map(|(id, _)| *id)
                .collect();
            (bare, map.values().filter(|c| is_vm(c)).count())
        };
        // A macOS VM belongs to the supervisor and outlives this process, and the
        // next one adopts it by its claim (ADR-0026) — provided the supervisor
        // answers, which is what the next process will ask first.
        if vms > 0 {
            let env = crate::vm::job::adopted_env();
            if let Err(e) = crate::vm::job::VmSupervisor::list(&env.supervisor).await {
                info!(
                    "preflight: {vms} macOS VM claim(s) in flight and the VM supervisor is not \
                     answering ({e}) — a restart would have to drain"
                );
                return false;
            }
        }
        if !bare.is_empty() {
            info!(
                "preflight: {} claim(s) in flight with no container or VM ({bare:?}) — \
                 nothing to adopt them from, so a restart would have to drain",
                bare.len()
            );
            return false;
        }
        match crate::docker::unlabelled_runners().await {
            Some(stale) if stale.is_empty() => true,
            Some(stale) => {
                info!(
                    "preflight: {} running container(s) predate ADR-0011 labelling ({}) — \
                     draining once to clear them",
                    stale.len(),
                    stale.join(", ")
                );
                false
            }
            None => {
                info!(
                    "preflight: could not list runner containers — a restart would have to drain"
                );
                false
            }
        }
    }

    /// Begin draining on `by`'s behalf: stop claiming new jobs; admitted runners
    /// keep going. Only `by` can end it again, through
    /// [`end_drain`](Self::end_drain).
    ///
    /// Also cancels every currently-*parked* claim (admitted == false). A parked
    /// task is blocked waiting on limiter admission and holds no container — there
    /// is nothing to let finish, and it may never win admission (the escalation
    /// guard can hold a class at its ceiling indefinitely). Signalling its cancel
    /// (via `send_replace`, so it lands even with zero receivers) makes the task
    /// drop the pending admission and [`release_claim`](Self::release_claim)
    /// promptly, so [`await_drain`](Self::await_drain) — which waits only on claims
    /// that still back a live runner container — converges instead of hanging on a
    /// claim that will never back one. Async because it must lock `inflight`.
    pub async fn begin_drain(&self, by: Drain) {
        self.draining.fetch_or(by.bit(), Ordering::SeqCst);
        let map = self.inflight.lock().await;
        for claim in map.values() {
            if !claim.admitted.load(Ordering::SeqCst) {
                let _ = claim.cancel.send_replace(true);
            }
        }
    }

    /// End `by`'s drain. Another holder's drain stays in force, so the updater
    /// can never lift a `faber drain` or a shutdown's. `true` when faber is no
    /// longer draining; the next cycle then runs at once, and the jobs of the
    /// parked claims the drain cancelled are claimed afresh.
    pub fn end_drain(&self, by: Drain) -> bool {
        let before = self.draining.fetch_and(!by.bit(), Ordering::SeqCst);
        let lifted = (before & !by.bit()) == 0;
        if lifted {
            self.trigger_reconcile();
        }
        lifted
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst) != 0
    }

    /// List a job whose declared services were refused, and warn the first time
    /// (ADR-0046 §2): once per job, as a dind refusal is, because the job stays
    /// queued and is re-tried on the provision throttle's cadence. The caller
    /// releases the claim: a refused job gets no runner.
    async fn refuse_services(&self, v: ServiceRefusedView) {
        let (repo, job_id, label, reason) =
            (v.repo.clone(), v.job_id, v.label.clone(), v.reason.clone());
        if self.metrics.lock().await.note_service_refused(v) {
            warn!(
                "{repo} job {job_id}: declared service {label} refused — {reason}; not minting \
                 a runner"
            );
        } else {
            debug!("{repo} job {job_id}: service {label} still refused — {reason}");
        }
    }

    /// Resolve once no container-backed runner is in flight. Used on shutdown to
    /// let genuinely-running jobs finish before exiting, so a stop/restart never
    /// orphans a live runner.
    ///
    /// A *parked* claim (admitted == false) holds no container — its provision task
    /// is still blocked on limiter admission, and the escalation guard can hold a
    /// class at its ceiling indefinitely — so waiting on one would hang the drain
    /// forever. [`begin_drain`](Self::begin_drain) cancels every parked claim
    /// precisely so those tasks bail and release.
    ///
    /// *Admitted* is necessary but not sufficient: the flag is set when the memory
    /// reservation is debited, before the container exists, and stays set after the
    /// container has gone if the attached `docker` client is wedged. In July 2026 a
    /// shutdown sat on several admitted claims with no `faber-` container on the
    /// host and had to be SIGTERM'd a second time (issue #16). So a claim is waited
    /// on only while Docker still shows its runner container running; one that has
    /// demonstrably gone away is cancelled — there is nothing left to let finish,
    /// and blocking on it converts a graceful stop into launchd's SIGKILL. A failed
    /// probe reads as "still running", the safe direction.
    pub async fn await_drain(&self) {
        self.drain_until_idle(live_runner_containers).await;
    }

    /// [`await_drain`](Self::await_drain) over an injectable container probe, so the
    /// convergence rules are testable without a Docker daemon in the loop.
    ///
    /// Registers for the next completion *before* reading the count so a runner that
    /// finishes in the gap can't be missed; `notify_one` stores a permit if no
    /// waiter is parked yet, so the wake-up is never lost. [`DRAIN_RECHECK`] bounds
    /// the wait so a container that dies mid-drain is noticed even when nothing
    /// releases a claim.
    async fn drain_until_idle<F, Fut>(&self, probe: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Option<HashSet<u64>>>,
    {
        loop {
            let next = self.runner_done.notified();
            let live = probe().await;
            let (mut remaining, mut abandoned) = (0usize, 0usize);
            {
                let map = self.inflight.lock().await;
                for (job_id, claim) in map.iter() {
                    if !claim.admitted.load(Ordering::SeqCst) {
                        continue;
                    }
                    // A macOS VM claim has no container to look for, and an
                    // unreadable probe proves nothing: both keep the drain waiting.
                    let gone = claim.containerized
                        && live.as_ref().is_some_and(|ids| !ids.contains(job_id));
                    if gone {
                        // Signal it so its task unwinds through its own cleanup —
                        // the reconciler never removes a claim itself.
                        let _ = claim.cancel.send_replace(true);
                        abandoned += 1;
                    } else {
                        remaining += 1;
                    }
                }
            }
            if remaining == 0 {
                if abandoned > 0 {
                    warn!(
                        "draining: {abandoned} admitted claim(s) had no live runner container \
                         — cancelled rather than waited on"
                    );
                }
                return;
            }
            info!("draining: waiting for {remaining} in-flight runner(s) to finish");
            tokio::select! {
                _ = next => {}
                _ = tokio::time::sleep(DRAIN_RECHECK) => {}
            }
        }
    }

    /// How many claims have a runner up: admitted, not parked. What holds off idle
    /// sleep (issue #90) — a parked claim has no runner a sleep could lose.
    pub(crate) async fn admitted_count(&self) -> usize {
        self.inflight
            .lock()
            .await
            .values()
            .filter(|c| c.admitted.load(Ordering::SeqCst))
            .count()
    }

    /// How many Docker claims have a runner up: admitted and containerized. A
    /// grow of the Linux VM's disk waits for none (ADR-0061 §3).
    pub(crate) async fn admitted_docker_count(&self) -> usize {
        self.inflight
            .lock()
            .await
            .values()
            .filter(|c| c.containerized && c.admitted.load(Ordering::SeqCst))
            .count()
    }

    /// Audit a grown Linux VM disk (ADR-0061, Rollout): `engine_disk_grown`, from
    /// and to, in bytes.
    pub(crate) async fn record_engine_disk_grown(&self, from: u64, to: u64) {
        let record = Record {
            action: Some("grown".to_string()),
            cause: Some(format!("{from} -> {to} bytes")),
            ..Record::new("engine_disk_grown", "-", 0)
        };
        if let Err(e) = self.audit.append(&record).await {
            warn!("audit: recording engine_disk_grown: {e}");
        }
    }

    /// Publish the idle-sleep assertion's state for the view (issue #90).
    pub(crate) fn publish_awake(&self, view: crate::awake::AwakeView) {
        *self.awake.lock().unwrap() = view;
    }

    /// The ids of jobs currently claimed by a provision task — the sweep's
    /// "do not touch" set. Ids are claimed before any Docker resource is
    /// created and released only after teardown completes.
    pub async fn inflight_job_ids(&self) -> HashSet<u64> {
        self.inflight.lock().await.keys().copied().collect()
    }

    // --- the critical disk floor (ADR-0032 §2) ------------------------------

    /// Every admitted Docker claim, for the critical floor to weigh: its job, its
    /// repo, the name its runner registered under, and the cell its frozen state
    /// is published through. A claim without a runner name yet has no runner to
    /// freeze, and is left out.
    pub(crate) async fn admitted_docker_claims(&self) -> Vec<crate::freeze::Admitted> {
        self.inflight
            .lock()
            .await
            .iter()
            .filter(|(_, c)| c.containerized && c.admitted.load(Ordering::SeqCst))
            .filter_map(|(job_id, c)| {
                Some(crate::freeze::Admitted {
                    job_id: *job_id,
                    repo: c.repo.clone(),
                    runner: c.runner_name.lock().unwrap().clone()?,
                    started_ms: c.started_ms,
                    frozen: c.frozen.clone(),
                    job_over: c.job_over.load(Ordering::SeqCst),
                })
            })
            .collect()
    }

    /// The registration names of this host's frozen runners (ADR-0032 §2).
    async fn frozen_runners(&self) -> HashSet<String> {
        self.inflight
            .lock()
            .await
            .values()
            .filter(|c| c.frozen.lock().unwrap().is_some())
            .filter_map(|c| c.runner_name.lock().unwrap().clone())
            .collect()
    }

    /// Record a freeze or a thaw (ADR-0032 §2): one `freeze` line in the audit
    /// log, `action` saying which.
    pub(crate) async fn record_freeze(
        &self,
        repo: &str,
        job_id: u64,
        runner: &str,
        action: &str,
        reason: &str,
        cause: String,
    ) {
        // The retry sweep's evidence, beside the audit line it mirrors: a runner
        // still frozen when GitHub gave up on its job is faber's doing.
        match action {
            "frozen" => self.ends.lock().await.record(Ending {
                runner: runner.to_string(),
                kind: EndKind::Frozen { thawed_ms: None },
                at_ms: now_ms(),
                executed_job_id: None,
            }),
            "thawed" => self.ends.lock().await.thawed(runner, now_ms()),
            _ => {}
        }
        let rec = Record::freeze(repo, job_id, runner, action, reason, cause);
        if let Err(e) = self.audit.append(&rec).await {
            warn!("audit append failed: {e}");
        }
    }

    /// Log and audit a trip or a re-arm of the p75 fallback (ADR-0033 §4): one
    /// `reserve` line, which is also what a restart rebuilds the fallback from.
    pub(crate) async fn record_reserve(&self, event: crate::reserve::Event) {
        use crate::reserve::Event;
        let rec = match event {
            Event::Tripped { trip: t, armed_ms } => {
                warn!(
                    "reserve: the p75 fallback tripped — {}; reserving as under \"max\" for {} h, \
                     re-arming at {} (ADR-0033 §4)",
                    t.cause.describe(),
                    t.cooloff_ms / 3_600_000,
                    t.until_ms()
                );
                Record::reserve(
                    "trip",
                    t.cause.as_str(),
                    t.cooloff_ms,
                    t.until_ms(),
                    Some(armed_ms),
                    format!(
                        "{}; max reservations for {} h",
                        t.cause.describe(),
                        t.cooloff_ms / 3_600_000
                    ),
                )
            }
            Event::Rearmed { trip, .. } => {
                info!(
                    "reserve: the {} h cool-off is over — reserving at p75 again (ADR-0033 §4)",
                    trip.cooloff_ms / 3_600_000
                );
                Record::reserve(
                    "rearm",
                    trip.cause.as_str(),
                    trip.cooloff_ms,
                    trip.until_ms(),
                    None,
                    "the cool-off is over; p75 reservations again".to_string(),
                )
            }
            Event::Armed { trip, armed_ms } => Record::reserve(
                "armed",
                trip.cause.as_str(),
                trip.cooloff_ms,
                trip.until_ms(),
                Some(armed_ms),
                format!(
                    "{} h with p75 in force since the last trip",
                    armed_ms / 3_600_000
                ),
            ),
        };
        if let Err(e) = self.audit.append(&rec).await {
            warn!("audit append failed: {e}");
        }
    }

    /// Check a claim about to be admitted for likely waste (ADR-0063 §4): the PR
    /// read and the run's jobs, each cached for a minute and cut off at five
    /// seconds. Every failure is "not fired".
    async fn check_waste(
        &self,
        subject: crate::waste::Subject<'_>,
    ) -> Option<crate::waste::Finding> {
        let gh = &self.gh;
        self.waste
            .check(
                &self.cfg.waste,
                subject,
                now_ms(),
                || async move {
                    match gh.pull(subject.repo, subject.pr).await {
                        Ok(pull) => Some(crate::waste::PullFacts::from(pull)),
                        Err(e) => {
                            debug!("waste: PR #{} of {} unread: {e}", subject.pr, subject.repo);
                            None
                        }
                    }
                },
                || async move {
                    match gh.run_jobs(subject.repo, subject.run_id).await {
                        Ok(jobs) => Some(crate::waste::failed_jobs(&jobs)),
                        Err(e) => {
                            debug!(
                                "waste: run {} of {} unread: {e}",
                                subject.run_id, subject.repo
                            );
                            None
                        }
                    }
                },
            )
            .await
    }

    /// Audit and count a waste finding: one `waste` line, carrying what faber
    /// did about it (ADR-0063 §3).
    async fn record_waste(
        &self,
        subject: crate::waste::Subject<'_>,
        finding: &crate::waste::Finding,
        action: &str,
        wait_ms: u64,
        label: &str,
    ) {
        self.metrics.lock().await.counts.wasted += 1;
        let rec = Record::waste(
            subject.repo,
            subject.run_id,
            subject.job_id,
            subject.pr,
            finding.names(),
            action,
            wait_ms,
            label.to_string(),
        );
        if let Err(e) = self.audit.append(&rec).await {
            warn!("audit append failed: {e}");
        }
    }

    /// A `skip` (ADR-0063 §6): cancel the claim's run, which also stops a runner
    /// minted for another job of the repo from taking this one, and for a
    /// conflicted PR say why on its head commit. The caller releases the claim.
    /// A write that fails is logged and nothing else: the claim is released
    /// either way, and the job keeps waiting for any runner, as it would have.
    async fn skip_wasted(
        &self,
        subject: crate::waste::Subject<'_>,
        finding: &crate::waste::Finding,
    ) {
        use crate::waste::Signal;
        let signal = finding.signal();
        let reason = format!("likely wasted: {} (PR #{})", signal.as_str(), subject.pr);
        info!(
            "{} job {}: {reason} — cancelling run {}",
            subject.repo, subject.job_id, subject.run_id
        );
        if let Err(e) = self
            .gh
            .cancel_run(subject.repo, subject.run_id, &reason)
            .await
        {
            warn!(
                "{}: could not cancel run {} ({reason}): {e}",
                subject.repo, subject.run_id
            );
        }
        if signal == Signal::Conflicted {
            if let Err(e) = self
                .gh
                .post_commit_status(
                    subject.repo,
                    subject.head_sha,
                    "error",
                    WASTE_STATUS_CONTEXT,
                    Some(CONFLICTED_STATUS),
                    &reason,
                )
                .await
            {
                warn!(
                    "{}: could not post {WASTE_STATUS_CONTEXT} on {}: {e}",
                    subject.repo, subject.head_sha
                );
            }
        }
    }

    /// The audit log, for a loop that rebuilds its state from it at startup.
    pub(crate) fn audit_log(&self) -> &Audit {
        &self.audit
    }

    /// Release a claim: drop its `inflight` entry and idle timer, then wake any
    /// drain waiter. The **single** place a claim is removed — a provision task
    /// calls it on its normal exit *and* on a cancel bail-out, so the reconciler
    /// never touches the map itself (it only signals the claim's cancel token).
    async fn release_claim(&self, job_id: u64) {
        self.inflight.lock().await.remove(&job_id);
        self.idle_since.lock().await.remove(&job_id);
        self.runner_done.notify_one();
    }

    /// Record the outcome of a reconcile cycle. `Ok` clears the fault (or leaves
    /// it clear); `Err` marks the pipeline unhealthy with the cycle's error.
    async fn record_cycle_health(&self, result: &Result<Vec<JoinHandle<()>>, GithubError>) {
        let err = result.as_ref().err().map(|e| e.to_string());
        *self.health.lock().await = err.clone();
        // `health` is a *live* field — it says whether the last cycle worked and
        // is overwritten by the next one, so an outage that has since cleared
        // leaves no trace in it at all. The fault records are the durable half:
        // without them, a daemon that couldn't see GitHub between 03:00 and 03:20
        // shows up in the audit log only as twenty minutes in which nothing
        // happened, indistinguishable from twenty minutes with nothing to do.
        self.observe_fault("-", "discovery", err).await;
    }

    /// A snapshot for the dashboard.
    pub async fn view(&self) -> StateView {
        let metrics = self.metrics.lock().await;
        let missing_images: Vec<MissingImageView> = self
            .profile_images
            .lock()
            .unwrap()
            .missing()
            .into_iter()
            .map(|(profile, m)| MissingImageView {
                profile,
                image: m.image,
                since_ms: m.since_ms,
                cause: m.cause,
            })
            .collect();
        let runner_retirements = self.runner_retirements(now_ms());
        // A credential that claims nothing makes the host unhealthy, so the
        // menu-bar app, which reads only `healthy` and `last_error`, shows it.
        let last_error = self
            .health
            .lock()
            .await
            .clone()
            .or_else(|| {
                self.gh
                    .claim_block()
                    .map(|why| format!("GitHub credential: {why}"))
            })
            .or_else(|| {
                missing_images
                    .first()
                    .map(|m| format!("profile image missing: {} ({})", m.profile, m.image))
            });
        let mut budget = self.limiter.snapshot();
        let now = now_ms();
        self.join_vm_rows(&mut budget.vm.vms).await;
        let mut inflight: Vec<InflightView> = self
            .inflight
            .lock()
            .await
            .iter()
            .map(|(job_id, claim)| {
                // Read once into a local: a guard taken inside the struct literal
                // lives to the end of it, so a second `lock()` of the same mutex
                // there would wait on itself for ever.
                let frozen = claim.frozen.lock().unwrap().clone();
                InflightView {
                    job_id: *job_id,
                    profile: claim.profile.clone(),
                    repo: claim.repo.clone(),
                    pr: claim.pr,
                    branch: claim.branch.clone(),
                    workflow: claim.workflow.clone(),
                    job_name: claim.job_name.clone(),
                    event: claim.event.clone(),
                    elapsed_s: now.saturating_sub(claim.started_ms) / 1000,
                    admitted: claim.admitted.load(Ordering::SeqCst),
                    step: claim.step.lock().unwrap().clone(),
                    waiting: claim.waiting.lock().unwrap().waiting(),
                    // A claim with no mint record is never polled (#105), so nothing
                    // would ever latch `started`; its container is running a job.
                    phase: JobPhase::of(
                        claim.admitted.load(Ordering::SeqCst),
                        claim.started.load(Ordering::SeqCst) || !claim.own_mint,
                        frozen.is_some(),
                    ),
                    frozen,
                }
            })
            .collect();
        inflight.sort_unstable_by_key(|v| v.job_id);
        StateView {
            owner: self.owner.clone(),
            topic: self.cfg.topic.clone(),
            gate: self.cfg.profiles.first().map(|p| p.policy.boundary()),
            profiles: self
                .cfg
                .profiles
                .iter()
                .map(|p| ProfileView {
                    name: p.name.clone(),
                    labels: p.labels.clone(),
                    backend: p.backend.kind().to_string(),
                    dind: match &p.backend {
                        Backend::Docker(d) => Some(d.dind.as_str().to_string()),
                        Backend::MacosVm(_) => None,
                    },
                    needs_wall: match &p.backend {
                        Backend::Docker(d) => crate::config::dind_needs_wall(
                            crate::config::WallFacts::current(),
                            d.dind,
                        ),
                        Backend::MacosVm(_) => false,
                    },
                    runner_version: self
                        .retirements
                        .lock()
                        .unwrap()
                        .runner_version(&p.name)
                        .map(str::to_string),
                })
                .collect(),
            concurrency: budget.docker_inflight,
            paused: self.is_paused(),
            draining: self.is_draining(),
            uptime_s: now_ms().saturating_sub(self.started_ms) / 1000,
            audit_path: self.audit.path().display().to_string(),
            inflight,
            counts: metrics.counts.clone(),
            refused_queued: metrics.refused_queued_sorted(),
            refused_queued_overflow: metrics.refused_overflow_total(),
            dind_refused: metrics.dind_refused_sorted(now),
            service_refused: metrics.service_refused_sorted(now),
            dind_label_rule: self.cfg.host.dind_label_rule().as_str(),
            bypasses: metrics.recent_bypasses.iter().rev().cloned().collect(),
            trust_bypasses_7d: metrics.trust_bypasses_since(now),
            repo_holds: self.repo_holds(),
            recent: metrics.recent.iter().rev().cloned().collect(),
            outcomes: metrics.recent_outcomes.iter().rev().cloned().collect(),
            faults: metrics.recent_faults.iter().rev().cloned().collect(),
            drift: metrics.recent_drift.iter().rev().cloned().collect(),
            retries: metrics.recent_retries.iter().rev().cloned().collect(),
            healthy: last_error.is_none(),
            last_error,
            missing_images,
            runner_retirements,
            unqualified_images: unqualified_images(&self.cfg.profiles, &self.cfg.host),
            instance: self.instance.clone(),
            peers: self.peers.lock().unwrap().view(),
            budget,
            update: crate::update::UpdateView::default(),
            notices: Vec::new(),
            awake: self.awake.lock().unwrap().clone(),
            git_mirrors: crate::git_mirror::global().view(),
            mirror: crate::mirror::global().view(),
            credential: self.gh.credential_view(),
            vm_jobs: crate::vm::job::global().view(),
            cli_copy: None,
            trust_changes: Vec::new(),
            attention: Default::default(),
        }
    }

    /// Fill each macOS VM row from the in-flight claim that holds it (popover/vms/
    /// anatomy.md O1): the claim is `claim_id(runner_name)`, its profile gives the
    /// VM's size, and the claim gives the repo and job name. The job name is
    /// withheld for a fork PR (a `pull_request*` event with no same-repo PR), whose
    /// author controls it, and for an adopted claim, which never learned one.
    async fn join_vm_rows(&self, vms: &mut [crate::limiter::VmRow]) {
        if !vms
            .iter()
            .any(|r| r.kind == crate::vm::proto::VmKind::Macos)
        {
            return;
        }
        let facts: Vec<crate::limiter::VmJobFacts> = self
            .inflight
            .lock()
            .await
            .values()
            .filter_map(|claim| {
                let runner = claim.runner_name.lock().unwrap().clone()?;
                let Backend::MacosVm(vm) = &self
                    .cfg
                    .profiles
                    .iter()
                    .find(|p| p.name == claim.profile)?
                    .backend
                else {
                    return None;
                };
                let outsider = claim.event.starts_with("pull_request") && claim.pr.is_none();
                Some(crate::limiter::VmJobFacts {
                    claim: crate::docker::claim_id(&runner).to_string(),
                    cpus: vm.cpus.get(),
                    memory_mib: vm.memory_mib.get(),
                    repo: claim.repo.clone(),
                    job_name: (!outsider && !claim.job_name.is_empty())
                        .then(|| claim.job_name.clone()),
                })
            })
            .collect();
        crate::limiter::join_vm_jobs(vms, &facts);
    }

    // --- recording ---------------------------------------------------------

    /// Append the `outcome` record for a finished runner, and mirror it onto the
    /// dashboard's outcome feed. `class` always describes the job that *ran*, since
    /// that record is the learner's replay substrate (see [`ExecutedJob`]).
    /// Write the trust boundary in force to the audit log, once per boot.
    ///
    /// The log is the record of what this host *did*, and a refusal only means
    /// something against the rule that produced it — a rule that used to be
    /// readable only from the source, and is now a config an operator can change
    /// between boots. Without this line, "these jobs were refused last Tuesday"
    /// has no accompanying "and here is what the machine was enforcing then".
    ///
    /// One line, at startup, rather than a field on every decision: the boundary
    /// cannot change without a restart, so per-job it would be the same string
    /// repeated forever.
    async fn record_boundary(&self) {
        let Some(b) = self.cfg.profiles.first().map(|p| p.policy.boundary()) else {
            return;
        };
        let rec = Record {
            verdict: Some(if b.allowed_events.is_empty() {
                "boundary:none".to_string()
            } else {
                format!("boundary:{}", b.allowed_events.join(","))
            }),
            // Reuses `action` rather than adding a field: the two relaxable edges
            // are what a reader is looking for, and they belong on the same line
            // as the events they qualify.
            action: Some(format!(
                "public_repos={} fork_prs={}",
                b.allow_public_repos, b.allow_fork_prs
            )),
            ..Record::new("gate", "", 0)
        };
        if let Err(e) = self.audit.append(&rec).await {
            warn!("audit append failed: {e}");
        }
    }

    /// Where a finished run's time and resources went (ADR-0056), for its
    /// `outcome` line: the claim's own marks, GitHub's times and steps for the job
    /// that ran, and what the sampler read.
    ///
    /// GitHub's side comes from the claim's poll when that poll saw the job that
    /// ran complete. Otherwise one more read of that job, made only for a run whose
    /// runner started, now that its reservation is back, and dropped first when
    /// GitHub's rate budget is low: then the line carries no steps rather than an
    /// empty list. A job GitHub has not yet marked completed gives its queue and
    /// start times and no steps.
    async fn footprint_of(
        &self,
        run: &Finished<'_>,
        report: &RunReport,
        finished_ms: u64,
    ) -> Footprint {
        let timeline = self
            .inflight
            .lock()
            .await
            .get(&run.job_id)
            .map(|c| c.footprint.lock().unwrap().clone())
            .unwrap_or_default();
        let ran = run.executed_job_id.unwrap_or(run.job_id);
        let polled = timeline.job.clone().filter(|j| j.job_id == ran);
        let job = match polled {
            Some(j) if j.is_complete() => Some(j),
            known => {
                let fetch = report.container_started_ms.is_some() && !self.gh.rate_budget_low();
                let fetched = if fetch {
                    match self.gh.job(run.repo, ran).await {
                        Ok(job) => job.as_ref().map(JobTimes::of),
                        Err(e) => {
                            debug!("footprint: fetch job {ran} in {}: {e}", run.repo);
                            None
                        }
                    }
                } else {
                    None
                };
                fetched.or(known)
            }
        };
        let job = job.unwrap_or_default();
        let mut footprint = Footprint {
            queued_ms: job.queued_ms,
            seen_ms: timeline.seen_ms,
            prepared_ms: timeline.prepared_ms,
            admitted_ms: timeline.admitted_ms,
            container_ms: report.container_started_ms,
            job_started_ms: job.started_ms,
            job_completed_ms: job.completed_ms,
            skewed: None,
            steps: job.steps,
            cpu_reserved_mcpu: timeline.reserved_mcpu,
            cpu_throttled_runner_usec: report.runner_throttled_usec,
            cpu_throttled_dind_usec: report.dind_throttled_usec,
            io_runner: report.runner_io,
            io_dind: report.dind_io,
        };
        footprint.skewed = footprint.marks(finished_ms).skewed();
        footprint
    }

    async fn record_outcome(self: &Arc<Self>, run: Finished<'_>, report: &RunReport) {
        let result = result_of(report);
        let finished_ms = now_ms();
        // Remember how this runner ended, as the auto-retry sweep will need it: keyed
        // by the runner name GitHub also reports on the job, and judged against the
        // job's own window so an ending can only ever excuse the job it happened
        // during (ADR-0003, amendment 2026-10-18). Recorded here because this is the
        // only moment faber holds all the facts at once — which container the kernel
        // killed, which runner it was, and which job GitHub said that runner was
        // executing. The same facts go to the audit line below as `end`, and
        // `retry::outcome_ending` is shared with the startup replay, so a restart
        // reads them back the same way.
        //
        // The OOM flags decide first and deliberately *not* `result`: an OOM reaches
        // the log as `failure` when the kernel took the runner listener and as
        // `success` when it took a child of a step and the listener exited 0 anyway
        // (the common shape, and a success on purpose — the container did what it
        // was asked). `outcome_ending` also drops an OOM that is not attributed; see
        // there for why.
        let end = end_of(report);
        if let Some(ending) = retry::outcome_ending(&OutcomeFacts {
            runner: run.runner,
            oom: report.oom_runner || report.oom_dind,
            end: Some(end.word()),
            attributed: run.class.is_some(),
            job_id: run.job_id,
            executed_job_id: run.executed_job_id,
            at_ms: finished_ms,
        }) {
            self.ends.lock().await.record(ending);
        }
        // The executed job's identity (ADR-0044). The claim describes the job it
        // was minted for, so it is borrowed only when that is the job that ran; a
        // different executed job gets its identity from the verdict fetch, never
        // the minted job's PR. The claim is still in the map: it is released after
        // this returns.
        let minted = run.executed_job_id.is_none_or(|e| e == run.job_id);
        let claimed = if minted {
            self.inflight.lock().await.get(&run.job_id).map(|c| {
                (
                    c.pr,
                    c.branch.clone(),
                    c.workflow.clone(),
                    c.job_name.clone(),
                    c.event.clone(),
                )
            })
        } else {
            None
        };
        let (pr, branch, workflow, job_name, event_name) = match claimed {
            Some((pr, branch, workflow, job_name, event)) => {
                (pr, branch, workflow, Some(job_name), Some(event))
            }
            None => (None, None, None, None, None),
        };
        // A reclaimed runner ran no job, so no conclusion is this run's to show.
        let pending = (!report.reclaimed).then(|| PendingVerdict {
            repo: run.repo.to_string(),
            job_id: run.executed_job_id.unwrap_or(run.job_id),
            runner: run.runner.to_string(),
            ts_ms: finished_ms,
            tries: 0,
            identity_from_job: !minted,
        });
        let event = OutcomeEvent {
            ts_ms: finished_ms,
            repo: run.repo.to_string(),
            job_id: run.executed_job_id.unwrap_or(run.job_id),
            profile: run.profile.to_string(),
            runner: run.runner.to_string(),
            label: run.label.map(str::to_string),
            result: result.to_string(),
            duration_s: run.duration_ms / 1000,
            wait_s: run.wait_ms / 1000,
            wait_gate: run.parked.binding().map(str::to_string),
            attempt: run.attempt,
            cause: report.cause.clone(),
            pr,
            branch,
            workflow,
            job_name,
            event: event_name,
            conclusion: None,
            conclusion_ts_ms: None,
        };
        {
            let mut m = self.metrics.lock().await;
            match tally_of(report) {
                Tally::Completed => m.counts.completed += 1,
                Tally::Faulted => m.counts.faults += 1,
                Tally::Neither => {}
            }
            m.recent_outcomes.push_back(event);
            while m.recent_outcomes.len() > OUTCOME_RECENT_CAP {
                m.recent_outcomes.pop_front();
            }
            if let Some(p) = pending {
                spawn_early_verdict(self, &p);
                m.pending_verdicts.push_back(p);
                while m.pending_verdicts.len() > OUTCOME_RECENT_CAP {
                    m.pending_verdicts.pop_front();
                }
            }
        }
        let footprint = self.footprint_of(&run, report, finished_ms).await;
        let rec = Record {
            result: Some(result.to_string()),
            duration_ms: Some(run.duration_ms),
            executed_job_id: run.executed_job_id,
            class: run.class.map(str::to_string),
            label: run.label.map(str::to_string),
            mem_peak_runner_mib: report.runner_peak_mib,
            mem_peak_dind_mib: report.dind_peak_mib,
            // `oom` stays the collapsed either-container boolean that replay and
            // every historical line depend on; the two below say *which*, which is
            // what makes a peak/OOM pair readable after the fact.
            oom: Some(report.oom_runner || report.oom_dind),
            oom_runner: Some(report.oom_runner),
            oom_dind: Some(report.oom_dind),
            // ADR-0019. Peaks and averages whenever the CPU was read, zero included;
            // throttle ratios and the quota only when non-zero, which is how replay
            // reads their absence anyway. Recorded for every outcome, and replayed
            // only for the ones carrying a `class`, like the memory peaks.
            cpu_peak_runner_mcpu: report.runner_cpu.map(|u| u.peak_mcpu),
            cpu_peak_dind_mcpu: report.dind_cpu.map(|u| u.peak_mcpu),
            cpu_avg_runner_mcpu: report.runner_cpu.map(|u| u.avg_mcpu),
            cpu_avg_dind_mcpu: report.dind_cpu.map(|u| u.avg_mcpu),
            cpu_throttled_runner: report
                .runner_cpu
                .map(|u| u.throttled_ratio)
                .filter(|r| *r > 0.0),
            cpu_throttled_dind: report
                .dind_cpu
                .map(|u| u.throttled_ratio)
                .filter(|r| *r > 0.0),
            cpu_quota_runner_mcpu: (report.applied_runner_mcpu > 0)
                .then_some(report.applied_runner_mcpu),
            profile: Some(run.profile.to_string()),
            // Taken from the run, never from the config: the config is mutable and
            // this line is not, so a job that already ran keeps the answer that was
            // true for it.
            dind: report.dind.map(|d| match d {
                true => "granted".to_string(),
                false => "denied".to_string(),
            }),
            runner: Some(run.runner.to_string()),
            // Which image served this job, and which content that tag actually
            // resolved to. Kept as a pair on purpose: the tag is what faber chose
            // and the id is what ran, and a mismatch between them is only visible
            // if the log carries both.
            image: report.image.clone(),
            image_id: report.image_id.clone(),
            wait_ms: Some(run.wait_ms),
            // Which gate bound this job, and the whole wait gate by gate (#85).
            wait_gate: run.parked.binding().map(str::to_string),
            wait_gates: (!run.parked.by_gate.is_empty()).then(|| run.parked.by_gate.clone()),
            attempt: Some(run.attempt),
            cause: report.cause.clone(),
            end: Some(end.word().to_string()),
            // ADR-0046 §9: absent when the runner had none.
            services: (!report.services.is_empty()).then(|| report.services.clone()),
            footprint,
            ..Record {
                ts_ms: finished_ms,
                ..Record::new("outcome", run.repo, run.job_id)
            }
        };
        if let Err(e) = self.audit.append(&rec).await {
            warn!("audit append failed: {e}");
        }
        // ADR-0033 §5: a kill the VM-wide OOM killer made, told
        // from a cap breach by the cgroup counters. Under "max" the learner has
        // treated the run as it always has; under "p75" it kept the run out
        // (ADR-0033 §5). Either way this line is what lets an operator see how often
        // it was not the class's doing.
        let p75 = self.limiter.reserve_mode() == crate::reserve::ReserveMode::P75;
        let mut host_oom = false;
        for (which, events) in [
            ("runner", report.runner_mem_events),
            ("dind", report.dind_mem_events),
        ] {
            let Some(ev) = events.filter(|e| e.is_host_oom()) else {
                continue;
            };
            host_oom = true;
            let rec = Record::host_oom(
                run.repo,
                run.job_id,
                run.runner,
                run.label.map(str::to_string),
                report.oom_runner,
                report.oom_dind,
                format!(
                    "{which} cgroup memory.events: oom_kill={} oom={}: a process was killed \
                     with no memory limit breached, so the VM-wide OOM killer took it, not \
                     the container's cap. {}",
                    ev.oom_kill,
                    ev.oom,
                    if p75 {
                        "Under reserve = \"p75\" it is kept out of the learner and bumps no \
                         cap (ADR-0033 §5)"
                            .to_string()
                    } else {
                        format!(
                            "The learner is unchanged under reserve = \"max\" and still treats \
                             this run as {} (ADR-0033)",
                            match which {
                                "runner" if report.oom_runner => "a censored OOM",
                                "dind" if report.oom_dind => "a censored OOM",
                                _ => "an ordinary sample (no OOM flag was raised)",
                            }
                        )
                    },
                ),
            );
            if let Err(e) = self.audit.append(&rec).await {
                warn!("audit append failed: {e}");
            }
        }
        // ADR-0033 §4: under "p75" a host OOM is a sign the overcommit lost, and
        // trips the fallback to max. A no-op under "max".
        if host_oom {
            if let Some(event) = self
                .limiter
                .trip_reserve(crate::reserve::TripCause::HostOom, finished_ms)
            {
                self.record_reserve(event).await;
            }
        }
        // A job this host's runner finished is the one kind worth sampling for
        // GitHub's runner-deprecation annotation (#64). A run that never got going
        // ran no job, so it would only spend the profile's sample.
        if !report.infra_error {
            self.retirements.lock().unwrap().finished(
                run.profile,
                Candidate {
                    repo: run.repo.to_string(),
                    job_id: run.executed_job_id.unwrap_or(run.job_id),
                    runner: run.runner.to_string(),
                },
            );
        }
    }

    // --- runner retirement (#64) --------------------------------------------

    /// The image `profile` names, or `None` for a macOS VM profile.
    fn profile_image(&self, profile: &str) -> Option<&str> {
        self.cfg
            .profiles
            .iter()
            .find(|p| p.name == profile)
            .and_then(|p| match &p.backend {
                Backend::Docker(d) => Some(d.image.as_str()),
                Backend::MacosVm(_) => None,
            })
    }

    /// Read GitHub's runner-deprecation annotation, if any, among a finished
    /// job's annotation `messages`, and record it against the profile that minted
    /// `runner`. A runner this host did not mint is a peer's or someone else's,
    /// and says nothing about this host's image. `ran` is whether the job reported
    /// any step, the only kind whose silence clears a warning.
    #[cfg(test)]
    fn observe_runner_annotations(
        &self,
        runner: Option<&str>,
        job_id: u64,
        ran: bool,
        messages: &[&str],
    ) {
        let profile_idx = runner.and_then(|r| {
            let minted = self.minted.lock().unwrap();
            minted.get(r).map(|m| m.profile_idx)
        });
        let Some(profile) = profile_idx.and_then(|i| self.cfg.profiles.get(i)) else {
            return;
        };
        self.observe_retirement(&profile.name, job_id, ran, messages);
    }

    /// Record what one job's annotations said about `profile`'s runner, and log
    /// the edge: the warning appearing, changing or clearing.
    fn observe_retirement(&self, profile: &str, job_id: u64, ran: bool, messages: &[&str]) {
        let found = runner_retirement::find_deprecation(messages);
        let now = now_ms();
        let change = self
            .retirements
            .lock()
            .unwrap()
            .observed(profile, job_id, found, ran, now);
        match change {
            // The warning itself follows from `log_retirements`, for which a new
            // one is always due.
            Change::Raised => {
                info!("profile {profile}: job {job_id} carries GitHub's runner-deprecation annotation")
            }
            Change::Cleared => info!(
                "profile {profile}: job {job_id} ran without GitHub's runner-deprecation \
                 annotation — the runner-retirement warning is cleared"
            ),
            Change::Unchanged => {}
        }
        self.log_retirements(now);
    }

    /// Log each standing runner-retirement warning not yet logged today: a
    /// warning while the cutoff is more than [`runner_retirement::URGENT_DAYS`]
    /// away, an error from then on.
    fn log_retirements(&self, now: u64) {
        let due = self.retirements.lock().unwrap().log_due(now);
        for (profile, d) in due {
            let text =
                runner_retirement::warning_text(&profile, self.profile_image(&profile), &d, now);
            if runner_retirement::days_remaining(d.cutoff, now) <= runner_retirement::URGENT_DAYS {
                error!("{text}");
            } else {
                warn!("{text}");
            }
        }
    }

    /// Every standing runner-retirement warning, for the dashboard.
    fn runner_retirements(&self, now: u64) -> Vec<RunnerRetirementView> {
        let warnings = self.retirements.lock().unwrap().warnings();
        warnings
            .into_iter()
            .map(|(profile, w)| {
                let d = &w.deprecation;
                let image = self.profile_image(&profile);
                let days = runner_retirement::days_remaining(d.cutoff, now);
                RunnerRetirementView {
                    message: runner_retirement::warning_text(&profile, image, d, now),
                    image: image.map(str::to_string),
                    runner_version: d.version.clone(),
                    cutoff: d.cutoff.iso(),
                    days_remaining: days,
                    urgent: days <= runner_retirement::URGENT_DAYS,
                    job_id: w.job_id,
                    seen_ms: w.seen_ms,
                    profile,
                }
            })
            .collect()
    }

    /// Fold one pass/fail observation of `scope` (for `repo`) into the fault feed,
    /// recording only the **transitions** — the edges of an outage, not its
    /// duration. A twenty-minute GitHub outage at a 20 s poll interval is two
    /// lines, not sixty.
    pub(crate) async fn observe_fault(&self, repo: &str, scope: &str, err: Option<String>) {
        let failed = err.is_some();
        // Scoped so the metrics guard is released before `record_fault` takes it.
        let changed = {
            self.metrics
                .lock()
                .await
                .note_fault(&format!("{scope}/{repo}"), failed)
        };
        if !changed {
            return;
        }
        match err {
            Some(cause) => self.record_fault(repo, scope, "failed", Some(cause)).await,
            None => self.record_fault(repo, scope, "recovered", None).await,
        }
    }

    /// Record a **fault**: faber's own pipeline failing, as opposed to a job
    /// failing. Durable (audit `kind: "fault"`) *and* on the dashboard, because
    /// these are precisely the failures an operator finds out about by noticing
    /// that nothing happened.
    ///
    /// Transition-gated by the caller (see [`Metrics::note_fault`]) so a GitHub
    /// outage that spans a hundred poll cycles writes two lines — one when it
    /// starts, one when it clears — not two hundred.
    pub(crate) async fn record_fault(
        &self,
        repo: &str,
        scope: &str,
        action: &str,
        cause: Option<String>,
    ) {
        {
            let mut m = self.metrics.lock().await;
            // Deliberately no `counts.faults` bump: that card counts *runs* faber
            // failed to deliver, and a cycle that couldn't reach GitHub delivered
            // no run either way. Folding the two in would make the one number that
            // is supposed to mean "jobs I broke" quietly also mean "minutes I was
            // blind", which is the kind of conflation this whole change is undoing.
            m.recent_faults.push_back(FaultEvent {
                ts_ms: now_ms(),
                repo: repo.to_string(),
                scope: scope.to_string(),
                action: action.to_string(),
                cause: cause.clone().unwrap_or_default(),
            });
            while m.recent_faults.len() > FAULT_RECENT_CAP {
                m.recent_faults.pop_front();
            }
        }
        let rec = Record::fault(repo, scope, action, cause);
        if let Err(e) = self.audit.append(&rec).await {
            warn!("audit append failed: {e}");
        }
    }

    /// Record an auto-retry action (ADR-0003): one audit line per re-run job (a
    /// precise durable trail), the `retried` counter bumped by the count, and one
    /// dashboard event for the run. `mode` is `all` or `surgical`.
    async fn record_retry(
        &self,
        repo: &str,
        run_id: u64,
        attempt: u64,
        jobs: &[(u64, String)],
        mode: &str,
    ) {
        {
            let mut m = self.metrics.lock().await;
            m.counts.retried += jobs.len() as u64;
            m.recent_retries.push_back(RetryEvent {
                ts_ms: now_ms(),
                repo: repo.to_string(),
                run_id,
                attempt,
                jobs: jobs.len(),
                mode: mode.to_string(),
            });
            while m.recent_retries.len() > RETRY_RECENT_CAP {
                m.recent_retries.pop_front();
            }
        }
        for (job_id, evidence) in jobs {
            let rec = Record::retry(repo, run_id, *job_id, mode, evidence.clone());
            if let Err(e) = self.audit.append(&rec).await {
                warn!("audit append failed: {e}");
            }
        }
    }

    /// Fire-and-forget a Playwright drift check for a Docker job (ADR-0001).
    /// Detached so it never delays, gates, or blocks provisioning — Phase 1 only
    /// reports. No head SHA ⇒ nothing to read repo files at, so skip silently.
    ///
    /// `content_ref` is where repo files are actually read (a PR merge ref for a
    /// same-repo `pull_request` run, `head_sha` otherwise — see
    /// `WorkflowRun::content_ref`); `head_sha` is still what the commit status
    /// lands on.
    fn spawn_playwright_drift_check(
        self: &Arc<Self>,
        repo: String,
        run_id: u64,
        job_id: u64,
        head_sha: String,
        content_ref: String,
        image: String,
    ) {
        if head_sha.is_empty() {
            return;
        }
        // Off for a repo whose token cannot read its files or post a status
        // (ADR-0027); the client warned once, naming the fix.
        if self.gh.blocked(&repo, Permission::ContentsRead)
            || self.gh.blocked(&repo, Permission::StatusesWrite)
        {
            return;
        }
        let orch = self.clone();
        tokio::spawn(async move {
            // Probe the half-wired-opt-in diagnostic only on the first job of each
            // repo (this daemon lifetime): `insert` is true exactly once per repo,
            // so an unlabelled image's `.faber/config.toml` is fetched once, not
            // per job — keeping the no-label common path cheap.
            let probe = orch.metrics.lock().await.drift_probed.insert(repo.clone());
            let outcome = drift::check_playwright_drift(
                &orch.gh,
                &repo,
                drift::RunRefs {
                    head_sha: &head_sha,
                    content_ref: &content_ref,
                },
                &image,
                &orch.cfg.playwright_drift,
                probe,
            )
            .await;
            orch.record_drift(&repo, run_id, job_id, &outcome).await;
        });
    }

    /// The image this job should run on, healing a runner-image drift by building
    /// a per-version variant when one is needed (ADR-0008).
    ///
    /// Returns the tag to start a container from — the profile's base when it
    /// already bakes what the repo pins, when the repo isn't opted in, or when a
    /// variant couldn't be produced. That last case is deliberately not an error
    /// the job inherits: it runs exactly as it would have without ADR-0008, with
    /// the mismatch surfaced by ADR-0001.
    ///
    /// The [`PoolImage`] carries the versions that tag *serves* alongside it,
    /// because the caller's next question is not "which tag" but "can this tag
    /// coexist with the ones already running for this repo" (ADR-0009). An empty
    /// `serves` means faber could not tell, and constrains nothing.
    ///
    /// Unlike [`Self::spawn_playwright_drift_check`] this is **inline** on the
    /// provisioning path: its answer decides which container to start, so it
    /// cannot be fire-and-forget. The caller keeps the claim parked across it.
    ///
    /// `content_ref` is where the lockfile that decides this is actually read (a
    /// PR merge ref for a same-repo `pull_request` run, `head_sha` otherwise —
    /// see `WorkflowRun::content_ref`); `head_sha` is still what a surfaced
    /// mismatch's commit status lands on.
    async fn resolve_runner_image(
        self: &Arc<Self>,
        repo: &str,
        run_id: u64,
        job_id: u64,
        head_sha: &str,
        content_ref: &str,
        base: &str,
    ) -> PoolImage {
        if head_sha.is_empty() {
            return PoolImage::unknown(base);
        }
        // Same one-shot-per-repo diagnostic as the detached path: the half-wired
        // opt-in is a per-repo misconfiguration, not a per-job one.
        let probe = self
            .metrics
            .lock()
            .await
            .drift_probed
            .insert(repo.to_string());
        let (image, outcome) = drift::heal_playwright_drift(
            &self.gh,
            repo,
            drift::RunRefs {
                head_sha,
                content_ref,
            },
            base,
            &self.cfg.playwright_drift,
            &self.images,
            probe,
        )
        .await;
        let serves = outcome.serves();
        self.record_drift(repo, run_id, job_id, &outcome).await;
        PoolImage {
            tag: image.unwrap_or_else(|| base.to_string()),
            serves,
        }
    }

    /// Hold this claim — still parked, no container, no memory reservation —
    /// until the repo's live runner-image pool can accept `candidate`, and return
    /// the image to actually run on. `None` means the claim was reclaimed while
    /// waiting; the caller releases and stops.
    ///
    /// The hold is bounded by [`POOL_HOLD_MAX`]: past it faber mints the resolved
    /// image anyway. That is deliberate and is the load-bearing half of the
    /// fail-open bias — a heterogeneous pool risks a job landing on the wrong
    /// image, which is where this repo already was before ADR-0009; a job held
    /// forever never runs at all, which is worse than anything the rule prevents.
    ///
    /// Waiting here rather than releasing the claim and retrying next cycle keeps
    /// the job visible on the dashboard, spends no GitHub reads re-resolving a pin
    /// that cannot have changed, and leaves the provision throttle
    /// ([`Metrics::provision_gate`]) measuring what it is for — a runner that
    /// never consumed its job — instead of counting faber's own deliberate waits
    /// as failures and backing the job off to the five-minute self-heal cadence.
    ///
    /// While it is actually holding, the claim says so through `waiting`; a pool
    /// that clears at once publishes nothing.
    async fn await_pool_image(
        &self,
        job_id: u64,
        repo: &str,
        candidate: PoolImage,
        cancel: &mut watch::Receiver<bool>,
        waiting: &std::sync::Mutex<Parking>,
    ) -> Option<PoolImage> {
        let deadline = now_ms().saturating_add(POOL_HOLD_MAX.as_millis() as u64);
        let mut held_since: Option<u64> = None;
        loop {
            if let PoolDecision::Mint(image) = self.claim_pool_image(job_id, repo, &candidate).await
            {
                if let Some(since) = held_since {
                    info!(
                        "{repo} job {job_id}: runner-image pool drained after {}s; running on {}",
                        now_ms().saturating_sub(since) / 1000,
                        image.tag
                    );
                }
                return Some(image);
            }
            if now_ms() >= deadline {
                warn!(
                    "{repo} job {job_id}: runner-image pool still holding {} after {}s — \
                     minting {} anyway rather than starving the job; it may be handed to a \
                     peer job that pins a version this image does not bake",
                    peer_tags(&self.pool_images(job_id, repo).await).join(", "),
                    POOL_HOLD_MAX.as_secs(),
                    candidate.tag,
                );
                self.publish_pool_image(job_id, &candidate).await;
                return Some(candidate);
            }
            if held_since.is_none() {
                held_since = Some(now_ms());
                let live = peer_tags(&self.pool_images(job_id, repo).await).join(", ");
                info!(
                    "{repo} job {job_id}: holding for the runner-image pool to drain — \
                     resolved {}, live {live}",
                    candidate.tag,
                );
                publish_wait(
                    waiting,
                    WaitView::new(
                        "pool",
                        format!("runner-image pool: waiting for {live} to drain"),
                    ),
                );
            }
            tokio::select! {
                biased;
                _ = wait_cancelled(cancel) => return None,
                _ = tokio::time::sleep(POOL_HOLD_POLL) => {}
            }
        }
    }

    /// [`live_pool`], for the log lines — a snapshot, taken and dropped, with no
    /// decision hanging off it.
    ///
    /// A peer faber instance's runners (ADR-0004's shared pool) are not in it:
    /// this host cannot see what image another host started a container from, so
    /// under demand provisioning the rule is enforced per instance and fails open
    /// across them.
    async fn pool_images(&self, job_id: u64, repo: &str) -> Vec<PoolImage> {
        let map = self.inflight.lock().await;
        live_pool(&map, job_id, repo)
    }

    /// Weigh `candidate` against this repo's live pool and, when the answer is to
    /// mint, publish the image onto this claim — **both under one held `inflight`
    /// lock**.
    ///
    /// The atomicity is the point. Two sibling jobs resolving concurrently to
    /// incomparable images would otherwise both read an empty pool and both mint,
    /// creating exactly the split this rule exists to prevent. Serialised through
    /// the map lock, the second one sees the first's published image and holds.
    /// Nothing is awaited inside, so holding the async lock across the decision
    /// costs a few map reads.
    async fn claim_pool_image(
        &self,
        job_id: u64,
        repo: &str,
        candidate: &PoolImage,
    ) -> PoolDecision {
        let map = self.inflight.lock().await;
        let decision = pool_decision(&live_pool(&map, job_id, repo), candidate);
        if let PoolDecision::Mint(image) = &decision {
            // Absent ⇒ the claim was reclaimed while we resolved. Publishing is
            // then pointless but harmless; the caller's next cancel check stops it.
            if let Some(claim) = map.get(&job_id) {
                *claim.image.lock().unwrap() = Some(image.clone());
            }
        }
        decision
    }

    /// Publish `image` onto this claim without consulting the rule — the
    /// [`POOL_HOLD_MAX`] fail-open path, where faber has decided to mint an image
    /// the rule declined and later siblings must weigh it as live all the same.
    async fn publish_pool_image(&self, job_id: u64, image: &PoolImage) {
        if let Some(claim) = self.inflight.lock().await.get(&job_id) {
            *claim.image.lock().unwrap() = Some(image.clone());
        }
    }

    /// Record a drift outcome to the audit log and, for the actionable cases, the
    /// dashboard feed. The silent common cases (no opt-in label; no head SHA) are
    /// pure noise and recorded nowhere.
    async fn record_drift(&self, repo: &str, run_id: u64, job_id: u64, outcome: &DriftOutcome) {
        let (record, event) = match outcome {
            DriftOutcome::Skipped(SkipCause::NoLabel)
            | DriftOutcome::Skipped(SkipCause::NoHeadSha) => return,
            // A half-wired opt-in: the repo configured drift but its image bakes no
            // label. Low-severity and one-shot (the caller probes once per repo),
            // so it surfaces the footgun without posting any commit status — there
            // is no baked version to judge, only a misconfiguration to flag.
            DriftOutcome::Skipped(cause @ SkipCause::ConfiguredNoLabel) => {
                let reason = cause.reason();
                warn!("{repo} job {job_id}: Playwright drift misconfigured — {reason}");
                let rec = Record::drift(
                    repo,
                    run_id,
                    job_id,
                    None,
                    None,
                    None,
                    "misconfigured".to_string(),
                );
                let ev = DriftEvent {
                    ts_ms: now_ms(),
                    repo: repo.to_string(),
                    run_id,
                    job_id,
                    status: "misconfigured".to_string(),
                    baked: None,
                    pinned: None,
                    mode: None,
                    action: reason,
                };
                (rec, Some(ev))
            }
            DriftOutcome::Skipped(cause) => {
                warn!(
                    "{repo} job {job_id}: drift check skipped — {}",
                    cause.reason()
                );
                let rec = Record::drift(
                    repo,
                    run_id,
                    job_id,
                    None,
                    None,
                    None,
                    "skipped".to_string(),
                );
                (rec, None)
            }
            DriftOutcome::InSync { version } => {
                let rec = Record::drift(
                    repo,
                    run_id,
                    job_id,
                    Some(version.clone()),
                    Some(version.clone()),
                    None,
                    "in_sync".to_string(),
                );
                (rec, None)
            }
            DriftOutcome::Drift {
                baked,
                pinned,
                mode,
                action,
            } => {
                info!(
                    "{repo} job {job_id}: Playwright drift — image bakes {baked}, repo pins \
                     {pinned} ({})",
                    action.as_str()
                );
                let rec = Record::drift(
                    repo,
                    run_id,
                    job_id,
                    Some(baked.clone()),
                    Some(pinned.clone()),
                    Some(mode.as_str().to_string()),
                    action.as_str().to_string(),
                );
                let ev = DriftEvent {
                    ts_ms: now_ms(),
                    repo: repo.to_string(),
                    run_id,
                    job_id,
                    status: "drift".to_string(),
                    baked: Some(baked.clone()),
                    pinned: Some(pinned.clone()),
                    mode: Some(mode.as_str().to_string()),
                    action: action.as_str().to_string(),
                };
                (rec, Some(ev))
            }
            // Healed (ADR-0008): the job runs on a variant that bakes what the
            // repo pins, so there is no *fresh* mismatch to surface — only the
            // fact that faber resolved the job onto a different image than the
            // profile's, which an operator reading the feed should see. `action`
            // additionally records whether faber cleared a stale `failure` this
            // SHA might still carry from before the heal existed (see
            // `drift::act_healed`) — under `surface` that is a `success` post
            // (`cleared`) or a failed one (`status_error`); under `off`/`gate`
            // there was nothing to clear (`silent` / `gate_noop`).
            DriftOutcome::Healed {
                baked,
                pinned,
                image,
                built,
                action,
            } => {
                info!(
                    "{repo} job {job_id}: healed Playwright drift — base bakes {baked}, repo pins \
                     {pinned}; running on {image} ({}, {})",
                    if *built { "built" } else { "cached" },
                    action.as_str()
                );
                let detail = format!(
                    "{} {image} ({})",
                    if *built { "built" } else { "cached" },
                    action.as_str()
                );
                let rec = Record::drift(
                    repo,
                    run_id,
                    job_id,
                    Some(baked.clone()),
                    Some(pinned.clone()),
                    None,
                    detail.clone(),
                );
                let ev = DriftEvent {
                    ts_ms: now_ms(),
                    repo: repo.to_string(),
                    run_id,
                    job_id,
                    status: "healed".to_string(),
                    baked: Some(baked.clone()),
                    pinned: Some(pinned.clone()),
                    mode: None,
                    action: detail,
                };
                (rec, Some(ev))
            }
            // Healing was on and didn't work. The job is running on the base
            // image, so this is a plain drift *plus* the reason the variant
            // couldn't be built — which is the half an operator needs to fix it.
            DriftOutcome::HealFailed {
                baked,
                pinned,
                cause,
                mode,
                action,
            } => {
                // A parked job is re-checked every pass: logged once per cause, and
                // hourly while it stands. The audit record below is still written.
                if crate::logline::repeats().fresh(
                    &format!("drift-unhealed:{job_id}"),
                    &format!("{baked} {pinned} {}", crate::logline::wording(cause)),
                    std::time::Instant::now(),
                ) {
                    warn!(
                        "{repo} job {job_id}: Playwright drift not healed — image bakes {baked}, \
                         repo pins {pinned}; running on the base image ({}). {cause}",
                        action.as_str()
                    );
                }
                let detail = format!("heal_failed ({}): {cause}", action.as_str());
                let rec = Record::drift(
                    repo,
                    run_id,
                    job_id,
                    Some(baked.clone()),
                    Some(pinned.clone()),
                    Some(mode.as_str().to_string()),
                    detail.clone(),
                );
                let ev = DriftEvent {
                    ts_ms: now_ms(),
                    repo: repo.to_string(),
                    run_id,
                    job_id,
                    status: "drift".to_string(),
                    baked: Some(baked.clone()),
                    pinned: Some(pinned.clone()),
                    mode: Some(mode.as_str().to_string()),
                    action: detail,
                };
                (rec, Some(ev))
            }
            DriftOutcome::Indeterminate(reason) => {
                if crate::logline::repeats().fresh(
                    &format!("drift-indeterminate:{job_id}"),
                    reason,
                    std::time::Instant::now(),
                ) {
                    warn!("{repo} job {job_id}: drift check indeterminate — {reason}");
                }
                let rec = Record::drift(
                    repo,
                    run_id,
                    job_id,
                    None,
                    None,
                    None,
                    "indeterminate".to_string(),
                );
                let ev = DriftEvent {
                    ts_ms: now_ms(),
                    repo: repo.to_string(),
                    run_id,
                    job_id,
                    status: "indeterminate".to_string(),
                    baked: None,
                    pinned: None,
                    mode: None,
                    action: reason.clone(),
                };
                (rec, Some(ev))
            }
        };
        if let Err(e) = self.audit.append(&record).await {
            warn!("drift audit append failed: {e}");
        }
        if let Some(ev) = event {
            let mut m = self.metrics.lock().await;
            m.recent_drift.push_back(ev);
            while m.recent_drift.len() > DRIFT_RECENT_CAP {
                m.recent_drift.pop_front();
            }
        }
    }
}

/// Run the loop forever, or for a single cycle when `once` is set (testing).
pub async fn run_loop(orch: Arc<Orchestrator>, once: bool) -> anyhow::Result<()> {
    let profiles = orch
        .cfg
        .profiles
        .iter()
        .map(|p| format!("{}[{}→{}]", p.name, p.labels.join(","), p.backend.kind()))
        .collect::<Vec<_>>()
        .join(" ");
    info!(
        owner = %orch.owner,
        topic = %orch.cfg.topic,
        profiles = %profiles,
        budget_mib = orch.limiter.snapshot().budget_mib,
        "faber watching"
    );
    orch.record_boundary().await;
    loop {
        let result = reconcile_once(&orch).await;
        orch.record_cycle_health(&result).await;
        if once {
            match result {
                Ok(handles) => {
                    for h in handles {
                        let _ = h.await;
                    }
                }
                Err(e) => error!("cycle error: {e}"),
            }
            return Ok(());
        }
        if let Err(e) = &result {
            error!("discovery failed this cycle: {e}");
        }
        tokio::select! {
            _ = tokio::time::sleep(orch.cfg.poll_interval) => {}
            _ = orch.wake.notified() => info!("manual reconcile triggered"),
        }
    }
}

/// One discovery → poll → gate → provision pass. Returns the provision tasks it
/// spawned (awaited by `--once`, left detached by the loop).
async fn reconcile_once(orch: &Arc<Orchestrator>) -> Result<Vec<JoinHandle<()>>, GithubError> {
    // ADR-0027: take a token `sudo faber auth set` stored, and warn of expiry.
    reconcile_credential(orch).await;
    // A credential that is refused, rejected, for another owner, not yet
    // confirmed by GitHub or expired claims nothing new. Running jobs finish on their own runners' credentials.
    // A rejected one also asks GitHub nothing more this cycle: a run of bad
    // credentials gets every authentication for the account refused for a while.
    if let Some(why) = orch.gh.claim_block() {
        debug!("claiming nothing: GitHub credential: {why}");
        if !orch.gh.is_rejected() {
            reconcile_claims(orch, &HashSet::new()).await;
        }
        return Ok(Vec::new());
    }
    // Read finished jobs' CI conclusions for the popover's pips (ADR-0044), off
    // the loop: display data must never hold up discovery, claims or reclaim. Run
    // here, ahead of the pause and the topic search, so a paused or draining host
    // and a failed search still show what its last jobs concluded.
    let mut spawned: Vec<JoinHandle<()>> = spawn_verdicts(orch).into_iter().collect();
    // While paused or draining we claim no new jobs; draining additionally lets
    // the already-spawned provision tasks run to completion (see `await_drain`).
    // A pause also stops the claims already taken from starting, but that is
    // enforced where they start — Docker and VM admission (ADR-0023).
    //
    // Claim reconciliation still runs. Pause means "take on no new work", not "stop
    // noticing that existing claims are dead": skipping it here left every stranded
    // claim frozen in place for as long as the pause lasted, so a resume started
    // from a phantom-filled in-flight map. An empty `live` set is exactly right —
    // nothing was scanned, so every parked claim gets an authoritative job fetch.
    if orch.is_paused() || orch.is_draining() {
        reconcile_claims(orch, &HashSet::new()).await;
        return Ok(spawned);
    }
    let repos = orch
        .gh
        .repos_with_topic(&orch.owner, &orch.cfg.topic)
        .await?;
    // Every job id observed this cycle, across all repos. Used to prune the dedup
    // map so it tracks the live working set instead of growing forever.
    let mut seen = HashSet::new();
    // The subset GitHub still reports as queued/in-progress — the claim-liveness
    // signal. Deliberately *not* `seen`: that one intentionally holds every routed
    // job of an active run, including its finished ones, so their dedup entries
    // survive the prune.
    let mut live = HashSet::new();
    for repo in &repos {
        // One repo's transient API error must not abort the whole cycle — but
        // "must not abort" is not "must not be recorded". A repo whose scan fails
        // every cycle (a token that lost `repo` scope, a repo made public,
        // sustained 403s) is silently never serviced, and the only prior evidence
        // was a `tracing` line among thousands. Recorded per repo, on transitions
        // only, so the persistent case stands out from the transient one.
        let err = reconcile_repo(orch, repo, &mut spawned, &mut seen, &mut live)
            .await
            .err()
            .map(|e| {
                error!("{repo}: {e}");
                e.to_string()
            });
        orch.observe_fault(repo, "repo_scan", err).await;
    }
    // Bound the dedup state: forget jobs no longer queued/in-progress anywhere.
    {
        let mut m = orch.metrics.lock().await;
        m.prune(&seen);
        m.retain_refused_repos(&repos);
    }
    // Same for the sizing cohorts: a repo that has left the topic is never scanned
    // again, so its snapshot would sit there forever.
    orch.cohorts.lock().await.retain(|r, _| repos.contains(r));
    // Reconcile the claim set against GitHub: release parked claims whose job has
    // finished, and reclaim admitted runners that have gone idle (see issue #1).
    reconcile_claims(orch, &live).await;
    // Delete the cache volumes enforced trust bypasses condemned, once nothing
    // mounts them (ADR-0030). A no-op unless one did.
    purge_bypassed_caches(orch).await;
    // Keep the Docker daemon itself usable (ADR-0007) before anything that
    // depends on it. A no-op while Docker is answering.
    reconcile_host(orch).await;
    // Notice a profile image that has gone from this host (#63), and its return.
    reconcile_profile_images(orch).await;
    // Probe the token's permissions on the served repositories, about hourly
    // (ADR-0027). Its own errors are logged, never fatal.
    reconcile_permissions(orch, &repos).await;
    // Auto-retry jobs that failed because faber's own runner died (ADR-0003). A
    // no-op unless `[retry] enabled`; its own errors are logged, never fatal.
    reconcile_retries(orch, &repos).await;
    // Read GitHub's runner-deprecation annotation off a finished job per profile
    // (#64), and repeat a standing warning once a day. A warning only.
    reconcile_runner_retirement(orch).await;
    Ok(spawned)
}

/// Scan one repo and spawn provision tasks for its gated, queued jobs. All the
/// network I/O lives here; the route → gate → record decision is delegated to
/// the pure, spawn-free [`process_run_jobs`] seam so it can be unit-tested.
async fn reconcile_repo(
    orch: &Arc<Orchestrator>,
    repo: &str,
    spawned: &mut Vec<JoinHandle<()>>,
    seen: &mut HashSet<u64>,
    live: &mut HashSet<u64>,
) -> Result<(), GithubError> {
    let mut runs: Vec<WorkflowRun> = Vec::new();
    for status in ACTIVE_STATUSES {
        runs.extend(orch.gh.runs_with_status(repo, status).await?);
    }
    // A run can surface under both statuses across the two calls; de-dup by id.
    let mut seen_runs = HashSet::new();
    runs.retain(|r| seen_runs.insert(r.id));
    // A new push or pull_request run is the event that refreshes the repo's git
    // mirror (ADR-0035 §1): faber has no webhook. A no-op with mirrors off.
    crate::git_mirror::global().observe_runs(repo, runs.iter().map(|r| (r.id, r.event.as_str())));

    // Fetch each run's jobs (network), then hand the assembled set to the
    // I/O-free decision seam.
    let mut run_jobs: Vec<(WorkflowRun, Vec<Job>)> = Vec::with_capacity(runs.len());
    for run in runs {
        let jobs = orch.gh.run_jobs(repo, run.id).await?;
        run_jobs.push((run, jobs));
    }

    // Which job GitHub actually gave each of our runners, and which jobs are still
    // live. Both read the scan we just did, before it's consumed by the decision
    // seam. `live` is the claim-liveness signal: only a job GitHub still reports as
    // queued/in-progress keeps its claim un-checked (see `reconcile_claims`).
    orch.harvest_executed(&run_jobs, now_ms()).await;
    live.extend(live_job_ids(&run_jobs));
    // Whose code each run the gate judges this cycle carries (ADR-0059 §3).
    let codes = orch.read_codes(repo, &run_jobs).await;
    // The same scan, re-judged: did any runner this host minted take a job the
    // gate refuses (ADR-0030)?
    orch.detect_bypasses(repo, &run_jobs, &codes).await;

    // Refresh the sizing cohorts from the same scan: which classes a runner minted
    // for each profile could be handed once it registers. Read at launch time by
    // `spawn_provision`, so a runner is capped for the whole set GitHub can dispatch
    // to it instead of only the job it was named for (issue #13).
    orch.set_cohorts(
        repo,
        queued_cohorts(&orch.cfg.profiles, repo, &run_jobs),
        now_ms(),
    )
    .await;

    // Two consumers share one listing: demand-based provisioning (ADR-0004), which
    // weighs queued jobs against the runners already registered on GitHub — the
    // shared signal that coordinates several instances — and the stale-runner sweep
    // below. So fetch it whenever the repo has *any* active run, in **either**
    // provisioning mode.
    //
    // The mode is deliberately not part of that condition. Gating the fetch on
    // `Demand` silently disabled the sweep under `per-job`: the list was always
    // empty there, so `stale_runner_ids` saw no candidates and dead registrations
    // accumulated on GitHub forever, unreaped and unreported. That matters even
    // where per-job ignores supply, because `list_runners` reads a single
    // `per_page=100` page — enough dead entries and the listing truncates, blinding
    // the sweep and (for anyone later switching to `demand`) the supply count too.
    // `process_run_jobs` ignores `runners` entirely under per-job, so handing it a
    // real list changes no provisioning decision there (pinned by
    // `per_job_mode_ignores_every_runner_shape`).
    //
    // A fully-idle repo still fetches nothing: it has no supply to weigh and no
    // fresh orphans to reap, so its stragglers wait for the next active cycle —
    // harmless once the grace bound stops them masking demand. The added per-job
    // call is an ETag-conditional GET like every other, so a repo whose runner list
    // has not changed costs a `304` that GitHub does not bill against the rate
    // limit.
    //
    // On a fetch error, fall back to an empty list for this repo this cycle: zero
    // supply means every gated job mints — never starving a queued job on a failed
    // supply read — and the sweep simply no-ops.
    //
    // `listed` says whether `runners` is GitHub's answer or that fallback: an
    // empty answer is a fact about the pool, an empty fallback is not.
    let (runners, listed) = if run_jobs.is_empty() {
        (Vec::new(), false)
    } else {
        match orch.gh.list_runners(repo).await {
            Ok(rs) => (rs, true),
            Err(e) if orch.cfg.provisioning == ProvisioningMode::Demand => {
                warn!(
                    "{repo}: demand: list_runners failed ({e}); minting per-job this \
                     cycle and skipping the stale-runner sweep"
                );
                (Vec::new(), false)
            }
            Err(e) => {
                warn!("{repo}: list_runners failed ({e}); skipping the stale-runner sweep");
                (Vec::new(), false)
            }
        }
    };

    // Other faber instances minting into this pool, and their lost runners (#94).
    // Only from a listing GitHub returned, empty included: an empty one is what
    // clears a peer's count here once its runners have gone.
    if listed {
        orch.observe_peers(repo, &runners).await;
    }

    let rc = ReconcileCtx {
        profiles: &orch.cfg.profiles,
        services: &orch.cfg.services,
        metrics: &orch.metrics,
        inflight: &orch.inflight,
        audit: &orch.audit,
    };
    let intents = process_run_jobs(
        &rc,
        repo,
        &run_jobs,
        &codes,
        &runners,
        orch.cfg.provisioning,
        seen,
        now_ms(),
    )
    .await;

    for intent in intents {
        spawned.push(spawn_provision(orch.clone(), repo.to_string(), intent));
    }

    // Self-heal the runner list: deregister JIT runners stuck `offline` past the
    // stale TTL (their container died or never started). Done after spawning this
    // cycle's provisions so cleanup never delays a mint, and only for faber-minted
    // names — a human's runner is never touched. A *peer instance's* runner is in
    // scope, though: its name is faber-shaped too, and in a shared pool either
    // instance reaping the other's dead registrations is the point. That is exactly
    // why the `busy` check below matters — it is what keeps "reap a peer's dead
    // runner" from becoming "reap a peer's live one". Each delete is idempotent
    // (404 = already gone), and one failure never aborts the rest or the cycle. The
    // grace bound in `runner_is_supply` already stops these from masking demand;
    // this keeps GitHub's list from accumulating dead entries.
    //
    // A runner GitHub still reports `busy` is held back rather than attempted: that
    // flag is in this same listing, and asking anyway earns a `422` refusal which is
    // a guard, not a fault — `offline` only means the heartbeat stopped, and in a
    // shared pool the job GitHub is protecting may be a peer instance's. The
    // deferral is bounded ([`BUSY_RUNNER_GRACE`]) and reported, so nothing leaks
    // unaccounted-for. A `422` that still gets through is unpredicted, and arms a
    // per-runner backoff rather than being retried every cycle forever (which once
    // put hundreds of identical `WARN`s in one log — #21). Any other failure (a
    // network blip, a rate limit, a `5xx`) is unaffected and keeps retrying as
    // before.
    let now = now_ms();
    // Drop backoff state for runners GitHub no longer lists — deregistered by a
    // peer, or GC'd by GitHub itself, neither of which reaches `clear_dereg`. Only
    // when a listing was actually read: an empty `runners` also means "not demand
    // mode" or "the fetch failed", and must not be mistaken for "no runners exist".
    if !runners.is_empty() {
        let listed: HashSet<u64> = runners.iter().map(|r| r.id).collect();
        orch.metrics.lock().await.retain_dereg(&listed);
    }
    // A runner this host has frozen (ADR-0032 §2) stopped heartbeating because
    // faber paused it, not because it died, so it is not the sweep's to judge
    // while it is frozen: deleting its registration would cost its job for
    // certain when it thaws.
    //
    // #105: every runner this sweep deletes comes from GitHub's own listing of
    // this repo — never from Docker. The frozen set is the one input Docker has
    // here, and it can only hold a runner back, never put one in front of a
    // delete.
    let runners = sweepable_runners(&runners, &orch.frozen_runners().await);
    for id in deferred_busy_runner_ids(&runners, now) {
        if orch.metrics.lock().await.note_deferral(id) {
            info!(
                "{repo}: stale runner {id} still has a job assigned on GitHub; \
                 deferring its deregistration until that clears"
            );
        }
    }
    let stale = stale_runner_ids(&runners, now);
    if !stale.is_empty() {
        let mut attempted = 0usize;
        let mut reaped = 0usize;
        for id in &stale {
            if !orch.metrics.lock().await.dereg_gate(*id, now) {
                continue; // still backing off from a prior permanent-shaped failure
            }
            attempted += 1;
            match orch
                .gh
                .delete_runner(
                    repo,
                    *id,
                    "stale-runner sweep: offline past its grace without taking its job",
                )
                .await
            {
                Ok(()) => {
                    reaped += 1;
                    orch.metrics.lock().await.clear_dereg(*id);
                }
                Err(e) if is_permanent_dereg_failure(&e) => {
                    if orch.metrics.lock().await.note_dereg_failure(*id, now) {
                        warn!(
                            "{repo}: stale runner {id} cannot be deregistered — GitHub \
                             still reports a job running on it more than {:?} after it \
                             was minted, long past when one should have finished; backing \
                             off (will keep retrying quietly, not every cycle): {e}",
                            BUSY_RUNNER_GRACE
                        );
                    } else {
                        debug!(
                            "{repo}: stale runner {id} still not deregisterable; backing \
                             off further: {e}"
                        );
                    }
                }
                Err(e) => warn!("{repo}: failed to deregister stale runner {id}: {e}"),
            }
        }
        if attempted > 0 {
            info!("{repo}: swept {reaped}/{attempted} stale offline runner(s)");
        }
    }
    Ok(())
}

/// The shared orchestrator state the decision seam reads and mutates, grouped so
/// `process_run_jobs` stays under the argument-count lint and the pieces travel
/// together. All borrows; the seam holds each lock only briefly, never across an
/// `.await` of another.
struct ReconcileCtx<'a> {
    profiles: &'a [Profile],
    /// The services this host provides (ADR-0046), which decide whether a
    /// `faber.<tool>` label on a runner is a service's.
    services: &'a crate::services::ServicesPolicy,
    metrics: &'a Mutex<Metrics>,
    inflight: &'a Mutex<HashMap<u64, Claim>>,
    audit: &'a Audit,
}

/// Whose code each run carries, by run id, as [`Orchestrator::read_codes`]
/// read it this cycle (ADR-0059 §3). A run that needs no read is absent, and
/// so is one whose read GitHub deferred: [`read_code`] tells them apart.
type Codes = HashMap<u64, Code>;

/// A job the gate approved for provisioning — the caller turns each into a
/// [`spawn_provision`] task. Kept spawn-free so the decision logic that produces
/// it is testable without Docker or the network.
#[derive(Debug, PartialEq, Eq)]
struct ProvisionIntent {
    job_id: u64,
    profile_idx: usize,
    /// Job name + workflow identity, carried through so `spawn_provision` can form
    /// the autolimiter's per-class key without re-fetching the run.
    job_name: String,
    workflow_id: u64,
    workflow_name: Option<String>,
    /// The run's id and head SHA, carried so `spawn_provision` can run the
    /// drift check (ADR-0001) — the SHA the commit status is posted on.
    run_id: u64,
    head_sha: String,
    /// The ref to read repo files at (`WorkflowRun::content_ref`) — a PR merge
    /// ref for a same-repo `pull_request` run, `head_sha` otherwise. Carried
    /// separately from `head_sha` because the two can pin different lockfile
    /// versions when a PR branch is stale relative to its base (see `drift.rs`'s
    /// module docs) — the tree `actions/checkout` actually runs is this one.
    content_ref: String,
    /// The PR a `pull_request` run belongs to (`WorkflowRun::pull_number`): what
    /// the waste check reads (ADR-0063 §1). `None` for every other run.
    pr: Option<u64>,
    /// When the run was created (`WorkflowRun::created_at`), so the waste check
    /// can tell a run the PR's close triggered from one it outlived.
    run_created_at: Option<String>,
    /// Which provision attempt for this job this is (1-based), from the throttle's
    /// own counter. Recorded on the outcome so a job faber has minted for five
    /// times reads as five times in the log, instead of five unrelated-looking
    /// lines that happen to share a job id.
    attempt: u32,
    /// The job's own `runs-on` labels, carried so `spawn_provision` can read the
    /// `faber.*` needs it declares (ADR-0010) and register the runner with them.
    /// Without this the runner would carry only its profile's labels and a job
    /// asking for a need could never land on it.
    job_labels: Vec<String>,
}

// --- demand-based provisioning (ADR-0004) ------------------------------------
//
// `per-job` mints one runner per observed queued job, deduped only by this
// process's own claim set — so two instances watching the same repos each mint a
// runner for the same job (GitHub runs it once; the surplus is idle-reclaimed).
// `demand` instead subtracts the runners already registered on GitHub — the signal
// *both* instances see — and mints only the shortfall, so they split one repo pool
// with no extra coordination. The residual double-mint (two instances polling
// before a fresh runner appears in the list) is bounded and idle-reclaimed as
// before; correctness never depends on it (GitHub stays the single job broker).

/// Parse the job id from a faber runner name (`faber-{job_id}-{ts}`), or `None` for
/// a name faber didn't mint. The id is the first `-`-delimited field after the
/// `faber-` prefix.
fn faber_job_id(name: &str) -> Option<u64> {
    name.strip_prefix("faber-")?.split('-').next()?.parse().ok()
}

/// Whether a failed job's runner belongs to a claim **this host is still running**,
/// so faber has no local verdict on it yet.
///
/// The retry sweep needs this because the two sides of a finished job do not land
/// at the same instant: GitHub learns the result from the runner and marks the job
/// complete immediately, while faber writes the outcome — and any OOM recorded in
/// it — only once the container has exited and teardown has finished, seconds
/// later. Live, that gap was about six seconds on one OOM kill. A sweep landing in
/// it would find a failed `faber-*` job with no local evidence and file the whole
/// run as terminally genuine in the dedup memory, never to be re-examined.
/// Deferring one cycle costs nothing; concluding early costs the retry entirely.
///
/// The runner name embeds the job it was minted for, which is exactly the key
/// `inflight` is a map of. It is deliberately coarse — a *re-provision* of the same
/// job id matches too — because every false match only defers a decision, which is
/// the safe direction.
fn runner_still_claimed(runner_name: Option<&str>, claimed: &HashSet<u64>) -> bool {
    runner_name
        .and_then(faber_job_id)
        .is_some_and(|minted| claimed.contains(&minted))
}

/// Parse the mint timestamp (ms since epoch) from a faber runner name
/// (`faber-{job_id}-{ts}`) — the second `-`-delimited field. `None` for a name
/// faber didn't mint or one without a numeric ts. Used to age a runner: how long
/// it has been registered, for the connecting-supply and stale-reap windows — which
/// it is only because the name is minted as the runner registers ([`admit_claim`]).
/// For the same reason it is when the claim was admitted, which is how the
/// critical floor orders its freezes (ADR-0032 §2).
pub(crate) fn faber_mint_ts(name: &str) -> Option<u64> {
    name.strip_prefix("faber-")?.split('-').nth(1)?.parse().ok()
}

/// Whether a registered runner can absorb a *queued* job this cycle — the supply
/// demand-based provisioning subtracts. Counts a runner that is online and idle (it
/// will pick a queued job up), or one still `offline` whose `faber-{id}-` name
/// belongs to a job still live this cycle **and** was minted within
/// [`CONNECTING_GRACE`] (a peer's — or our own — fresh mint, about to come online).
/// A busy runner is already working (its job isn't queued); a stale offline orphan
/// (its namesake job gone, *or* minted too long ago to still be connecting) is
/// ignored so it can't mask real demand; a non-faber offline runner may be
/// permanently dead, so it never counts.
///
/// The mint-age bound is what breaks the demand deadlock: without it a dead
/// `offline` runner whose job stays queued (because the runner died) credits that
/// job forever, so demand never mints a replacement. `now_ms` is the current wall
/// clock; a runner whose ts is in the future (clock skew) ages to 0 — treated as
/// fresh, the safe direction.
fn runner_is_supply(r: &Runner, live_ids: &HashSet<u64>, now_ms: u64) -> bool {
    match r.status.as_str() {
        "online" => !r.busy,
        "offline" => {
            faber_job_id(&r.name).is_some_and(|id| live_ids.contains(&id))
                && faber_mint_ts(&r.name).is_some_and(|ts| {
                    now_ms.saturating_sub(ts) <= CONNECTING_GRACE.as_millis() as u64
                })
        }
        _ => false,
    }
}

/// The registered runners that can absorb this repo's queued demand this cycle
/// (ADR-0004), held as their own label sets rather than counted per profile.
///
/// Supply is spent against a **job's** labels, because that is how GitHub matches:
/// a runner may serve a job when it carries every label the job asks for, and
/// extra labels on the runner never exclude it (`job ⊆ runner`, the same rule
/// [`labels_compatible`] documents for cohort sizing).
///
/// Counting per profile — the shape this replaced — silently assumed a profile's
/// labels were the whole story about which runner could take which job. Once a
/// runner can carry narrower per-job labels (ADR-0010), that assumption
/// **under-mints**: a runner registered for `faber.playwright-1.60.0` would mask a
/// queued job asking for `faber.playwright-1.62.1` that can never land on it, and
/// the job would wait for a runner which is never coming. Starving a queued job is
/// the one direction ADR-0004 says faber must not fail in, so supply is now
/// matched exactly the way GitHub will match it.
#[derive(Debug, Default)]
struct Supply {
    /// One entry per runner still available to absorb a job this cycle, removed as
    /// it is spent so no runner is counted as covering two jobs.
    runners: Vec<Vec<String>>,
    /// Which `faber.<tool>` labels are services' (ADR-0046).
    services: crate::services::ServicesPolicy,
}

impl Supply {
    /// Spend one registered runner that could actually be handed `job_labels`.
    ///
    /// `false` means nothing registered can serve this job, so the shortfall mints
    /// — the same outcome as an exhausted supply, and the same fail-open direction
    /// an errored `list_runners` takes.
    ///
    /// Labels compare ASCII case-insensitively, as GitHub matches them.
    ///
    /// A runner carrying the `dind` label has a Docker daemon (ADR-0026), and only
    /// a job that asks for `[faber, dind]` gets one. GitHub could still hand such a
    /// runner to a job that did not ask, but faber must not count on it: doing so
    /// would skip minting for that job and steer it onto a daemon it never asked
    /// for. So a job without `dind` never takes a `dind`-labelled runner.
    ///
    /// Not gated on `[host] dind_requires_label`: with the rule off, this host
    /// registers no runner with `dind` (a profile whose own labels include it only
    /// routes jobs that carry it too), so the exclusion changes nothing here. A
    /// runner that does carry it then comes from a peer host with the rule on, and
    /// has a daemon all the same.
    ///
    /// The same holds for a runner carrying service labels (ADR-0046): a job takes
    /// it only when it asks for every service the runner has. Not a privilege
    /// question this time, but a resource one: that runner's per-job containers
    /// and its database are spent the moment it exists, and its reservation was
    /// sized for them. Counting it as supply for a job that did not ask would
    /// skip a mint for that job on the strength of a runner GitHub may well hand
    /// to the service job it was minted for. Under-minting is the direction
    /// ADR-0004 says faber must not fail in; an extra mint is the safe one.
    fn take_for(&mut self, job_labels: &[String]) -> bool {
        let is_dind = |l: &String| l.eq_ignore_ascii_case(crate::dind::LABEL);
        let asks_dind = job_labels.iter().any(is_dind);
        let asks_for = |l: &String| job_labels.iter().any(|w| w.eq_ignore_ascii_case(l));
        let Some(i) = self.runners.iter().position(|r| {
            (asks_dind || !r.iter().any(is_dind))
                && r.iter()
                    .filter(|l| is_service_label(l, &self.services))
                    .all(&asks_for)
                && job_labels
                    .iter()
                    .all(|want| r.iter().any(|have| have.eq_ignore_ascii_case(want)))
        }) else {
            return false;
        };
        self.runners.remove(i);
        true
    }
}

/// The registered runners that already cover queued demand this cycle (ADR-0004).
/// Each runner must still route to a configured profile — one faber would not
/// route to is not faber's supply, whatever labels it carries — but *which* job it
/// can absorb is decided per job by [`Supply::take_for`]. `live_ids` are the job
/// ids still queued/in-progress in this repo, for the offline-namesake guard in
/// [`runner_is_supply`]; `now_ms` bounds that guard's connecting window.
fn compute_supply(
    runners: &[Runner],
    profiles: &[Profile],
    services: &crate::services::ServicesPolicy,
    live_ids: &HashSet<u64>,
    now_ms: u64,
) -> Supply {
    let mut supply = Supply {
        services: services.clone(),
        ..Supply::default()
    };
    for r in runners {
        if !runner_is_supply(r, live_ids, now_ms) {
            continue;
        }
        if match_profile(profiles, &r.labels).is_some() {
            supply.runners.push(r.labels.clone());
        }
    }
    supply
}

/// A faber-minted runner that has sat `offline` longer than [`STALE_RUNNER_TTL`]
/// — the age test the reap and the busy-deferral below share, so the two can never
/// disagree about which runners are even in scope. A non-faber runner (no parseable
/// `faber-{id}-{ts}` name) is never in scope; neither is an `online` one (live —
/// busy, or idle supply) or a recently-minted `offline` one (may still be
/// connecting).
fn is_stale_offline(r: &Runner, now_ms: u64) -> bool {
    r.status == "offline"
        && faber_mint_ts(&r.name)
            .is_some_and(|ts| now_ms.saturating_sub(ts) > STALE_RUNNER_TTL.as_millis() as u64)
}

/// The ids of stale offline runners safe to deregister this cycle: dead weight on
/// the runner list, from a JIT runner whose container died or never started.
///
/// Excludes one GitHub reports `busy` — it still has a job assigned to that runner
/// and answers the `DELETE` with a `422` ("... is currently running a job and
/// cannot be deleted"). That refusal is a **guard, not a fault**: `offline` only
/// means the runner has stopped heartbeating, and in a shared pool the runner may
/// be a *peer instance's* with a live job on it, which deleting would orphan. The
/// flag is in the same listing that made the runner a candidate, so faber can
/// decline to ask rather than ask and be refused — no wasted call, no alarming log
/// line for what is ordinarily a transient that clears within minutes, and nothing
/// to remember between cycles (the next listing re-derives it, so this survives a
/// restart for free, unlike the backoff it replaces).
///
/// Deferral is bounded by [`BUSY_RUNNER_GRACE`]: past it the runner is attempted
/// regardless, so a registration GitHub never un-busies can't be ignored forever.
/// A `422` at that age is genuinely unexpected and arms
/// [`Metrics::note_dereg_failure`]'s backoff. Reaping only offline runners still
/// means a runner mid-job (reported `online busy=true`) is never aborted.
fn stale_runner_ids(runners: &[Runner], now_ms: u64) -> Vec<u64> {
    runners
        .iter()
        .filter(|r| is_stale_offline(r, now_ms))
        .filter(|r| !r.busy || past_busy_grace(r, now_ms))
        .map(|r| r.id)
        .collect()
}

/// The runners the stale-runner sweep may judge: every one listed, less those of
/// this host's frozen claims (ADR-0032 §2), by exact registration name. A frozen
/// runner reads `offline` because faber paused it, and its registration is what
/// lets its job resume when it thaws.
fn sweepable_runners(runners: &[Runner], frozen: &HashSet<String>) -> Vec<Runner> {
    runners
        .iter()
        .filter(|r| !frozen.contains(&r.name))
        .cloned()
        .collect()
}

/// The ids [`stale_runner_ids`] is holding back this cycle because GitHub still
/// reports a job running on them — the exact complement of that function over
/// [`is_stale_offline`]. Reported by the caller so a deferral is visible rather
/// than silent, and no runner can leak unaccounted-for.
fn deferred_busy_runner_ids(runners: &[Runner], now_ms: u64) -> Vec<u64> {
    runners
        .iter()
        .filter(|r| is_stale_offline(r, now_ms))
        .filter(|r| r.busy && !past_busy_grace(r, now_ms))
        .map(|r| r.id)
        .collect()
}

/// Whether a runner has been `busy` past [`BUSY_RUNNER_GRACE`], measured from its
/// mint timestamp — the only age faber has for it (GitHub's listing carries no
/// "busy since"). The runner registered then and cannot have started a job any
/// earlier, so this over-states how long it has been busy by its ramp and GitHub's
/// dispatch delay, and the deferral can end that much early, never late. That is
/// the safe direction: GitHub still refuses (`422`) to delete a runner with a job.
/// A name without a parseable ts never reaches here ([`is_stale_offline`] already
/// required one).
fn past_busy_grace(r: &Runner, now_ms: u64) -> bool {
    faber_mint_ts(&r.name)
        .is_some_and(|ts| now_ms.saturating_sub(ts) > BUSY_RUNNER_GRACE.as_millis() as u64)
}

/// Whether a failed deregistration is **permanent**-shaped: GitHub will answer the
/// same way on every future attempt, because nothing in this codepath can change
/// the fact it's objecting to (#21). Classified on the response's status code
/// alone, not GitHub's message prose — the prose is not a stable API and could
/// reword without notice, whereas the status is the contract.
///
/// `DELETE /actions/runners/{id}` takes no caller-supplied input beyond the id
/// already embedded in the URL, so it has nothing else to validate; its only
/// observed `422` is GitHub believing the runner still has a job running
/// (`"... is currently running a job and cannot be deleted"`).
///
/// **"Permanent" here means "won't clear between one attempt and the next", not
/// "never".** It does clear — when the job GitHub is holding reaches a terminal
/// state, `busy` drops and the same `DELETE` succeeds (observed clearing in
/// minutes). So the backoff must never give up; it only stops the retry being
/// per-cycle.
///
/// Nearly every such `422` is now avoided rather than absorbed: [`stale_runner_ids`]
/// reads the `busy` flag from the same listing and declines to ask. What still
/// reaches here is a runner busy past [`BUSY_RUNNER_GRACE`], or a `422` faber
/// genuinely did not predict — both worth the backoff and the log line. Every other
/// error — transport failures, decode errors, a bad/rotated token, rate limiting, a
/// `5xx` — is left alone and keeps retrying next cycle, unchanged.
fn is_permanent_dereg_failure(err: &GithubError) -> bool {
    matches!(err, GithubError::Status { status: 422, .. })
}

/// The pure decision core of a reconcile pass over already-fetched runs+jobs:
/// route → gate → record, returning the jobs to provision. No network, no
/// spawning — every side effect is a lock-guarded mutation of `metrics`/
/// `inflight` or an `audit` append, so this is exercised directly by tests.
///
/// `seen` accumulates every routed job id for the caller's end-of-cycle prune.
/// Recording obeys two different dedups by design:
/// * **Skip / Refuse** have no claim to bound them, so they dedup across cycles
///   via [`Metrics::note`] — a job lingering in the active set records once per
///   verdict transition, not once per poll.
/// * **Provision** is bounded by its `inflight` claim: it is recorded
///   *unconditionally* once the claim succeeds, so one claim = one `provisioned`.
///   A legitimately re-spawned runner (a prior one died with the job still
///   queued, releasing the claim) re-claims and is counted again — which
///   `note`-gating would wrongly suppress, since the verdict label is unchanged.
///
/// A `Provision` additionally passes the per-job retry throttle
/// ([`Metrics::provision_gate`]) *before* its claim and bumps
/// [`Metrics::note_provision_attempt`] *after* it, so a job stuck `queued` (its
/// runner never consumes it) backs off and eventually stops instead of being
/// re-provisioned every cycle forever. `now` is threaded in for deterministic
/// tests.
///
/// Under [`ProvisioningMode::Demand`] (ADR-0004), a `Provision` is additionally
/// weighed against `runners` — the repo's GitHub-registered runners: if an
/// already-registered runner (this instance's or a peer's) can take the job, faber
/// declines to mint a duplicate and spends one unit of that profile's supply
/// instead. In [`ProvisioningMode::PerJob`] `runners` is unused and every gated
/// queued job mints, as before.
///
/// `codes` is whose code each run carries ([`Orchestrator::read_codes`]); a
/// queued job whose run's read was deferred is judged on provenance alone
/// (`Code::Own`): a refusal that needs no read is recorded, anything else waits
/// for the next poll.
// The scan's parts, each as the I/O-free seam's tests hand it in.
#[allow(clippy::too_many_arguments)]
async fn process_run_jobs(
    rc: &ReconcileCtx<'_>,
    repo: &str,
    runs: &[(WorkflowRun, Vec<Job>)],
    codes: &Codes,
    runners: &[Runner],
    provisioning: ProvisioningMode,
    seen: &mut HashSet<u64>,
    now: u64,
) -> Vec<ProvisionIntent> {
    let mut intents = Vec::new();
    // Demand mode: precompute how much of each profile's queued backlog is already
    // covered by registered runners (ours + any peer's). Each job we then decline to
    // mint for spends one unit of its profile's supply. `None` in per-job mode, so
    // the claim path below is unchanged there.
    let mut remaining_supply: Option<Supply> = if provisioning == ProvisioningMode::Demand {
        let live_ids: HashSet<u64> = runs
            .iter()
            .flat_map(|(_, jobs)| jobs.iter())
            .filter(|j| j.status == "queued" || j.status == "in_progress")
            .map(|j| j.id)
            .collect();
        Some(compute_supply(
            runners,
            rc.profiles,
            rc.services,
            &live_ids,
            now,
        ))
    } else {
        None
    };
    // Queued jobs covered by existing supply this cycle — logged once per repo for
    // live visibility into what demand mode suppressed.
    let mut suppressed = 0usize;
    // Jobs the gate refuses this cycle (issue #95); synced into the live set below.
    let mut refused_now: Vec<RefusedJobView> = Vec::new();
    for (run, jobs) in runs {
        for job in jobs {
            // Route the job to the first profile whose labels it carries. No
            // match = not ours at all: routine noise, don't record it.
            let Some((profile_idx, profile)) = match_profile(rc.profiles, &job.labels) else {
                continue;
            };
            // Observed a job faber routes: mark it seen so end-of-cycle pruning
            // keeps its dedup entry alive (including on cycles where the claim
            // below short-circuits before recording).
            seen.insert(job.id);
            // A queued job whose read GitHub deferred is judged on provenance
            // alone (`Code::Own`, today's rules): a refusal that needs no read
            // (fork PR, denied actor, public repo, disallowed event) is real and
            // recorded now; anything else waits for the next poll.
            let mut deferred = false;
            let code = match read_code(codes, run) {
                Some(code) => code,
                // Not queued, so `evaluate` skips it ahead of the checks that
                // would read its code.
                None if job.status != "queued" => Code::Unread("not read: not queued".to_string()),
                None => {
                    deferred = true;
                    Code::Own
                }
            };
            let ctx = job_context(run, job, code);
            let verdict = gate::evaluate(&ctx, &profile.policy);
            if matches!(verdict, Verdict::Skip(SkipReason::NotTargeted)) {
                continue;
            }
            if deferred && !matches!(verdict, Verdict::Refuse(_)) {
                continue;
            }
            if verdict != Verdict::Provision {
                // Refuse / Skip: record (deduped across cycles), never provision.
                if let Verdict::Refuse(reason) = &verdict {
                    refused_now.push(RefusedJobView {
                        repo: repo.to_string(),
                        job_id: job.id,
                        run_id: run.id,
                        job_name: job.name.clone(),
                        workflow: run.name.clone().unwrap_or_default(),
                        event: run.event.clone(),
                        verdict: verdict_str(&verdict),
                        since_ms: now,
                        labels: job.labels.clone(),
                        trust: gate::classify(reason).may_be_hostile(),
                    });
                }
                record_decision(rc, repo, run, job, &ctx, &verdict, profile).await;
                continue;
            }
            // bug-002: throttle a job stuck `queued` (its runner never consumes
            // it) so it isn't re-provisioned every cycle forever. Checked before
            // the claim so a throttled job needs no claim/release churn.
            let gate = rc.metrics.lock().await.provision_gate(job.id, now);
            match gate {
                ProvisionGate::Ready => {}
                ProvisionGate::BackingOff => continue,
                ProvisionGate::SlowRetry { warn } => {
                    // bug-002 defect B: past the fast-attempt cap the job is NOT
                    // abandoned. We fall through and provision, but only once per
                    // ceiling window, so it self-heals the moment its GitHub-side
                    // blocker (busy concurrency group / unmet `needs:` dep) clears
                    // or a runner slot frees. A finished job drops out of the
                    // queued/in-progress set and is pruned, so this slow retry
                    // stops on its own — no GitHub status re-check needed. Log the
                    // crossover once for visibility, then provision.
                    if warn {
                        warn!(
                            "{repo} job {}: still queued after {} fast attempts; \
                             throttling to slow retry (~{}s cadence) until it clears \
                             — self-healing, no manual intervention needed",
                            job.id,
                            PROVISION_MAX_ATTEMPTS,
                            PROVISION_BACKOFF_CEILING_MS / 1000,
                        );
                    }
                }
            }
            // Provision: claim the job *before* recording, so `provisioned` counts
            // a runner we actually spawn — exactly once — not the 2-4 poll ticks
            // the job spends queued while the runner connects. A claim already
            // present means a prior cycle owns it — leave *its* live handles intact
            // (never overwrite, or the owning task is orphaned from its cancel token).
            {
                let mut map = rc.inflight.lock().await;
                if map.contains_key(&job.id) {
                    continue;
                }
                // Demand mode: a runner already registered on GitHub (ours or a
                // peer's, via the shared runner list) can take this queued job —
                // don't mint a duplicate. Each covered job spends one unit of its
                // profile's supply for this cycle; once spent, the shortfall mints.
                // A job we already own (above) is skipped before this, so it never
                // spends a slot. Checked after the throttle read, which is harmless
                // for a job we won't mint.
                if let Some(rem) = remaining_supply.as_mut() {
                    if rem.take_for(&job.labels) {
                        suppressed += 1;
                        continue;
                    }
                }
                let (cancel, _rx) = watch::channel(false);
                map.insert(
                    job.id,
                    Claim {
                        repo: repo.to_string(),
                        profile: profile.name.clone(),
                        pr: run.pull_requests.first().map(|p| p.number),
                        branch: run.head_branch.clone(),
                        workflow: run.name.clone(),
                        job_name: job.name.clone(),
                        event: run.event.clone(),
                        started_ms: now,
                        step: Arc::new(std::sync::Mutex::new(None)),
                        started: Arc::new(AtomicBool::new(false)),
                        cancel,
                        admitted: Arc::new(AtomicBool::new(false)),
                        containerized: matches!(profile.backend, Backend::Docker(_)),
                        runner_name: Arc::new(std::sync::Mutex::new(None)),
                        // This process takes the claim and will mint its runner.
                        own_mint: true,
                        image: Arc::new(std::sync::Mutex::new(None)),
                        waiting: Arc::default(),
                        frozen: Arc::new(std::sync::Mutex::new(None)),
                        job_over: Arc::new(AtomicBool::new(false)),
                        // ADR-0056 §2: the poll that took the job is where `pickup`
                        // ends, and the job's own `created_at` where it began.
                        footprint: Arc::new(std::sync::Mutex::new(Timeline {
                            seen_ms: Some(now),
                            job: Some(JobTimes::of(job)),
                            ..Timeline::default()
                        })),
                    },
                );
            }
            record_decision(rc, repo, run, job, &ctx, &verdict, profile).await;
            // Count this attempt and arm the backoff before the next, bounding the
            // retry storm should the job stay queued.
            let attempt = rc.metrics.lock().await.note_provision_attempt(job.id, now);
            intents.push(ProvisionIntent {
                job_labels: job.labels.clone(),
                job_id: job.id,
                profile_idx,
                job_name: job.name.clone(),
                workflow_id: run.workflow_id,
                workflow_name: run.name.clone(),
                run_id: run.id,
                head_sha: run.head_sha.clone(),
                content_ref: run.content_ref(),
                pr: run.pull_number(),
                run_created_at: run.created_at.clone(),
                attempt,
            });
        }
    }
    let added = rc.metrics.lock().await.sync_refused(repo, refused_now);
    for v in added {
        warn!(
            "{repo} job {} ({}) is queued on GitHub but refused here ({}); it will \
             not run on this host",
            v.job_id, v.workflow, v.verdict
        );
    }
    if suppressed > 0 {
        info!(
            "{repo}: demand — {suppressed} queued job(s) covered by existing runners; \
             minted {}",
            intents.len()
        );
    }
    intents
}

/// Record one gate decision: bump the tally, push the recent-activity feed, and
/// append an audit line. Skip/Refuse are deduped across cycles (see
/// [`Metrics::note`]); a Provision arrives here only after a successful claim and
/// is recorded unconditionally (the claim is its once-per-spawn dedup).
///
/// The dedup check and the mutation it guards share the one `metrics` lock (no
/// check-then-act race); the audit append happens after the lock is released, so
/// no lock is held across its `.await`.
async fn record_decision(
    rc: &ReconcileCtx<'_>,
    repo: &str,
    run: &WorkflowRun,
    job: &Job,
    ctx: &JobContext,
    verdict: &Verdict,
    profile: &Profile,
) {
    let verdict_label = verdict_str(verdict);
    {
        let mut m = rc.metrics.lock().await;
        // Provision is deduped by its claim, so it is never note-gated; Skip and
        // Refuse record once per verdict transition rather than once per poll.
        if !matches!(verdict, Verdict::Provision) && !m.note(job.id, &verdict_label) {
            return;
        }
        match verdict {
            Verdict::Provision => m.counts.provisioned += 1,
            Verdict::Refuse(_) => m.counts.refused += 1,
            // Skips still reach the feed and the audit, but carry no tally — see
            // [`Counts`] for why the number was never worth a slot.
            Verdict::Skip(_) => {}
        }
        m.recent.push_back(Decision {
            ts_ms: now_ms(),
            repo: repo.to_string(),
            job_id: job.id,
            event: ctx.event.clone(),
            actor: ctx.actor.clone(),
            verdict: verdict_label.clone(),
            profile: profile.name.clone(),
        });
        while m.recent.len() > RECENT_CAP {
            m.recent.pop_front();
        }
    }
    let rec = Record {
        run_id: Some(run.id),
        event: Some(ctx.event.clone()),
        actor: Some(ctx.actor.clone()),
        verdict: Some(verdict_label),
        // The profile is what turns a refusal into a reproducible one: the same job
        // under a different profile is gated by a different `TrustPolicy`, so
        // "refuse:actor:x" without it names a rule the reader can't locate. Always
        // present — a job matching no profile is skipped before it reaches here.
        profile: Some(profile.name.clone()),
        // Names the job, so a refusal reads as a workflow and a step rather than as
        // a bare id you then have to look up on GitHub to understand.
        label: Some(job_label(repo, run.name.as_deref(), &job.name)),
        ..Record::new("decision", repo, job.id)
    };
    if let Err(e) = rc.audit.append(&rec).await {
        warn!("audit append failed: {e}");
    }
}

/// The image one claim's runner runs on, and every toolchain version that image
/// can serve (ADR-0009).
///
/// `serves` is the whole point: two tags are *not* interchangeable because they
/// are equal, they are interchangeable because one bakes everything the other
/// does. A variant bakes two versions (its base's and its own), which is what
/// lets one image cover a repo's `main` and its bump PR at once — see
/// [`crate::drift::DriftOutcome::serves`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct PoolImage {
    /// The tag a container would be started from.
    tag: String,
    /// Versions the tag bakes. **Empty means unknown**, not "none".
    serves: BTreeSet<String>,
}

impl PoolImage {
    /// An image faber can say nothing about — no opt-in label, no readable
    /// lockfile, drift off. It constrains nothing and is constrained by nothing.
    fn unknown(tag: &str) -> Self {
        Self {
            tag: tag.to_string(),
            serves: BTreeSet::new(),
        }
    }

    /// Whether faber knows enough about this image for it to weigh in a decision.
    fn known(&self) -> bool {
        !self.serves.is_empty()
    }

    /// Whether this image bakes everything `other` bakes — so any job `other`
    /// could have served runs here too.
    fn covers(&self, other: &PoolImage) -> bool {
        other.serves.is_subset(&self.serves)
    }
}

/// What [`pool_decision`] says to do with a freshly-resolved image.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PoolDecision {
    /// Start the container from this image. Not always the one the job resolved:
    /// when a live runner already covers this job, the job joins *it* rather than
    /// narrowing the pool with a second image.
    Mint(PoolImage),
    /// Neither image covers the other, so running both at once would guarantee
    /// some job lands somewhere it cannot run. Wait for the live ones to drain.
    Hold,
}

/// Decide which image a job may run on, given what this repo's live faber runners
/// are already running (ADR-0009).
///
/// The invariant being held: **the images live for one repo are totally ordered by
/// what they serve, and faber only ever adds the maximum.** A pool that satisfies
/// it is one where every runner can serve every job any *other* live runner could
/// have taken — so GitHub handing a job to the "wrong" runner stops being a
/// failure mode.
///
/// Three cases, in the order they are tested:
///
/// * **The candidate covers everything live** ⇒ mint it. Widening is always safe:
///   the new runner can serve every job the pool could already serve. This is both
///   the empty-pool case and the bump-PR case (a variant bakes the base's version
///   too, so it dominates the base).
/// * **Something live covers the candidate *and* everything else live** ⇒ mint on
///   *that* image instead. This is the case a naive "images differ ⇒ wait" rule
///   gets catastrophically wrong: with a variant hot, every ordinary `main` job
///   resolves back to the base, and making each one wait for a drain — then making
///   the next PR job wait for *those* to drain — serialises the repo's whole CI
///   behind alternating drains. Substituting instead keeps the pool homogeneous at
///   no cost, since the live image already bakes what this job pins.
/// * **Neither dominates** ⇒ [`PoolDecision::Hold`]. Genuinely incomparable
///   images (a third Playwright version in flight) cannot both be live without one
///   of them being wrong for somebody, so faber runs one generation at a time.
///
/// **Every unknown fails open.** `Hold` requires faber to positively know the
/// candidate's versions *and* at least one live runner's, and to have found them
/// incomparable; an unlabelled image, an unreadable lockfile or a peer instance's
/// runner (whose image this host cannot see) constrains nothing. Under-minting
/// starves CI, which is far worse than a surplus container — the same asymmetry
/// [`compute_supply`] is built on.
fn pool_decision(live: &[PoolImage], candidate: &PoolImage) -> PoolDecision {
    if !candidate.known() {
        return PoolDecision::Mint(candidate.clone());
    }
    let known: Vec<&PoolImage> = live.iter().filter(|p| p.known()).collect();
    // Covers the empty pool too: `all` over nothing is true, so the first live
    // image of a generation is minted as resolved.
    if known.iter().all(|l| candidate.covers(l)) {
        return PoolDecision::Mint(candidate.clone());
    }
    match known
        .iter()
        .find(|d| d.covers(candidate) && known.iter().all(|l| d.covers(l)))
    {
        Some(dominant) => PoolDecision::Mint((*dominant).clone()),
        None => PoolDecision::Hold,
    }
}

/// The images this repo's *other* live claims have been cleared to run on — the
/// pool [`pool_decision`] weighs a candidate against.
///
/// Only a claim that has been cleared to mint publishes an image, so one still
/// holding contributes nothing. That is what makes a hold cycle impossible: a
/// holding claim cannot be the reason another claim holds.
fn live_pool(map: &HashMap<u64, Claim>, job_id: u64, repo: &str) -> Vec<PoolImage> {
    map.iter()
        .filter(|(id, claim)| **id != job_id && claim.repo == repo)
        .filter_map(|(_, claim)| claim.image.lock().unwrap().clone())
        .collect()
}

/// The distinct tags in a pool snapshot, for a log line. Deduped and ordered so a
/// repo running four containers off one image reads as one tag, not four.
fn peer_tags(live: &[PoolImage]) -> Vec<String> {
    let tags: BTreeSet<&str> = live.iter().map(|p| p.tag.as_str()).collect();
    if tags.is_empty() {
        return vec!["nothing".to_string()];
    }
    tags.into_iter().map(|t| t.to_string()).collect()
}

/// How far before the oldest mint an adopted container claims [`Orchestrator::adopt`]
/// reads the audit log from. A `mint` line is written moments after its name's
/// timestamp is taken, so this only covers clock steps between the two.
const MINT_RECORD_SLACK_MS: u64 = 60 * 60 * 1000;

/// The `(repo, runner name)` of every `mint` line in `entries`: the runners this
/// host's own faber recorded minting (#105).
///
/// The record is only as trustworthy as the audit log. The gap this once had
/// belonged to the Docker Desktop backend, which shared `/Users` into its VM by
/// default, so root in the VM — which a granted dind is — could forge a `mint`
/// line (moot only because the same share exposed faber's binary and config).
/// ADR-0026's VMs close it on a set-up host: none shares a host folder
/// (`checkWall`, in `launcher/Sources/Wall.swift`, refuses any directory-sharing
/// device), so no guest can write to the log through one.
fn mint_records(entries: &[crate::audit::Entry]) -> HashSet<(String, String)> {
    entries
        .iter()
        .filter(|e| e.kind == crate::audit::MINT)
        .filter_map(|e| Some((e.repo.clone(), e.runner.clone()?)))
        .collect()
}

/// The GitHub identity of an adopted container, when faber's own records vouch
/// for it — and `None` otherwise (#105).
///
/// The container says which repo it serves and which runner it is: its
/// `org.faber.repo` label and its name. Both are only what the daemon reports, and
/// on a host that grants dind the daemon may be lying. So the identity is taken
/// from the `mint` line faber wrote when it chose the runner's name, and only when
/// the container's claim matches that line exactly, repo and name. A container
/// named after a peer's runner, or after one of this host's own runners but
/// claiming another repo, matches nothing and gets no identity: it is still
/// watched and torn down, and nothing about it is written to GitHub. The line is
/// as trustworthy as the audit log it is read from, which no guest can reach on a
/// set-up host (ADR-0026): see [`mint_records`].
fn vouched_mint(
    records: &HashSet<(String, String)>,
    a: &crate::docker::Adopted,
) -> Option<crate::docker::MintedRunner> {
    let name = a.registration();
    if !records.contains(&(a.repo.clone(), name.clone())) {
        return None;
    }
    crate::docker::MintedRunner::recorded(&a.repo, &name)
}

/// Watch a claim rebuilt from an existing container through to its outcome
/// (ADR-0011).
///
/// The tail of [`spawn_provision`] with the whole head removed: no admission (the
/// caller already charged the ledger), no image resolution, no JIT mint, no ramp.
/// What is left is identical, and deliberately so — an adopted run must produce
/// the same outcome record, the same teardown and the same claim release as one
/// this process started, or a restart would show up as a hole in the audit log.
///
/// One thing differs, and it is a decision rather than an omission: **an adopted
/// run may widen a class cap but never narrow one.** Its memory peak is a
/// high-water mark that started when the adoption did, so it under-reports
/// whatever happened before the restart, and teaching a smaller cap from it would
/// surface later as an OOM in an unrelated run with nothing pointing back here.
/// The OOM verdict itself survives on the corpse and is kept.
///
/// `minted` is the runner's GitHub identity as faber's own records give it
/// ([`vouched_mint`]), or `None`, in which case nothing about it is written to
/// GitHub (#105).
fn spawn_adopted(
    orch: Arc<Orchestrator>,
    a: crate::docker::Adopted,
    minted: Option<crate::docker::MintedRunner>,
    profile_idx: usize,
    guard: crate::limiter::AdmissionGuard,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _guard = guard;
        let start = now_ms();
        let Some(cancel) = ({
            let map = orch.inflight.lock().await;
            map.get(&a.job_id).map(|c| c.cancel.subscribe())
        }) else {
            return;
        };
        // What is *left* of the lifetime cap, measured from the container's own
        // start. A restart must not hand every in-flight job another three hours;
        // an already-expired container gets a nominal second so it is killed on
        // the next poll rather than being read as uncapped (`ZERO` means "no cap").
        let configured = match &orch.cfg.profiles[profile_idx].backend {
            Backend::Docker(d) => d.runner_timeout,
            Backend::MacosVm(_) => Duration::ZERO,
        };
        let lifetime = if configured.is_zero() {
            Duration::ZERO
        } else {
            configured
                .saturating_sub(Duration::from_millis(now_ms().saturating_sub(a.started_ms)))
                .max(Duration::from_secs(1))
        };

        let report = match crate::docker::adopt_one(&orch.gh, &a, minted, lifetime, cancel).await {
            Ok(report) => report,
            Err(e) => {
                error!("adopted runner {} failed: {e:#}", a.runner());
                RunReport {
                    infra_error: true,
                    lost: crate::docker::runner_was_lost(&e),
                    cause: Some(format!("{e:#}")),
                    ..Default::default()
                }
            }
        };

        let label = orch
            .limiter
            .label_for(&a.class)
            .unwrap_or_else(|| job_label(&a.repo, None, a.class.rsplit("::").next().unwrap_or("")));
        let mut class = None;
        if !report.timed_out && !report.reclaimed && !report.infra_error {
            // Peaks stripped, OOM kept: `sample_for` then yields a censored sample
            // for an OOM and nothing at all otherwise, which is exactly "may widen,
            // never narrow" without the learner needing to know about adoption.
            // CPU goes too: whatever this process sampled covers only the part of
            // the run after adoption, and a partial window can narrow a share.
            let oom_only = RunReport {
                runner_peak_mib: None,
                dind_peak_mib: None,
                runner_cpu: None,
                dind_cpu: None,
                ..report.clone()
            };
            if report.oom_runner || report.oom_dind {
                warn!(
                    "adopted runner {} was OOM-killed — bumping class {} on the kill alone \
                     (its peak predates the adoption and is not learnable)",
                    a.runner(),
                    a.class
                );
                // ADR-0033 §5, under "p75" only: a host OOM bumps nothing.
                if let Some(learned) = orch.limiter.learnable(&oom_only) {
                    orch.limiter.record_run(&a.class, &label, &learned);
                    class = Some(a.class.clone());
                }
            }
        }

        orch.record_outcome(
            Finished {
                repo: &a.repo,
                job_id: a.job_id,
                executed_job_id: None,
                class: class.as_deref(),
                label: Some(&label),
                profile: &orch.cfg.profiles[profile_idx].name,
                // The name GitHub knows the runner by, as for a run this process
                // started — not the container's name. The OOM ledger, the audit
                // record and the runner-retirement sample (#64) all match it
                // against the `runner_name` GitHub reports on the job.
                runner: &a.registration(),
                attempt: a.attempt,
                // Nothing waited: the container was already running when this
                // process met it. Zero is the honest number, not a missing one.
                wait_ms: 0,
                parked: &NOT_PARKED,
                // Measured from the container's start, not from adoption, so the
                // record says how long the job actually took.
                duration_ms: now_ms().saturating_sub(a.started_ms.min(start)),
            },
            &report,
        )
        .await;
        orch.release_claim(a.job_id).await;
    })
}

/// Spawn a single provision task. Docker jobs pass the memory-budget admission
/// gates and carry a learned per-class cap; macOS VM jobs pass VM admission (ADR-0026, #263). The matched profile (by index into
/// `cfg.profiles`) selects the backend.
fn spawn_provision(
    orch: Arc<Orchestrator>,
    repo: String,
    intent: ProvisionIntent,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let ProvisionIntent {
            job_id,
            profile_idx,
            job_labels,
            job_name,
            workflow_id,
            workflow_name,
            run_id,
            head_sha,
            content_ref,
            pr,
            run_created_at,
            attempt,
        } = intent;
        // Take our cancel receiver, admitted flag and shared cells from the claim
        // the reconcile cycle inserted. Absent ⇒ the claim was reconciled away
        // before this task ran; nothing to do.
        let (mut cancel, admitted, runner_name, waiting, footprint) = {
            let map = orch.inflight.lock().await;
            match map.get(&job_id) {
                Some(claim) => (
                    claim.cancel.subscribe(),
                    claim.admitted.clone(),
                    claim.runner_name.clone(),
                    claim.waiting.clone(),
                    claim.footprint.clone(),
                ),
                None => return,
            }
        };
        // The claim was inserted before this task ran, by a cycle that holds no
        // limiter; its parking cell counts into the process-wide per-gate tally
        // from its first wait on (issue #85).
        waiting
            .lock()
            .unwrap()
            .count_into(orch.limiter.gate_tally());
        // Already reclaimed before we started (e.g. a whole run cancelled at once):
        // release the claim without spawning anything. Likewise a claim that a cycle
        // already under way took after the drain began: the drain cancelled only
        // the claims it could see, and one parked behind a pause would otherwise
        // wait for a resume that a draining host may never get (ADR-0023).
        if *cancel.borrow() || orch.is_draining() {
            orch.release_claim(job_id).await;
            return;
        }

        // Clone what the backend needs so no borrow of `cfg` is held across the
        // (potentially long) provisioning await. Mutable because the Docker image
        // is resolved per job (ADR-0008), not fixed by the profile.
        let mut backend = orch.cfg.profiles[profile_idx].backend.clone();
        let mut labels = orch.cfg.profiles[profile_idx].labels.clone();
        // Which host minted it, readable from the runner and from the job's own
        // page (#94). Routing never asks for it: a profile's labels stay a subset.
        labels.push(instance::host_label(&orch.instance));
        // What the image this claim ends up on serves (ADR-0009), captured so it
        // can be stamped on the container. Empty stays empty when ADR-0008 is off
        // or the image is unknown — both of which mean the same thing to
        // `pool_decision`, and must keep meaning it after an adoption.
        let mut serves: Vec<String> = Vec::new();
        // The runner's name, minted by `admit_claim` once the claim stops waiting,
        // never here: this task can still park for image resolution, the pool hold
        // and admission, and the name's timestamp is the runner's age to everything
        // that reads GitHub's runner list (ADR-0018). Every path that reaches the
        // outcome below passes through one of the two admissions first.
        let name;
        let start = now_ms();
        // How long this claim sat parked before a container existed. `duration_ms`
        // alone can't separate a slow job from a starved one, and under budget
        // pressure the wait is most of the elapsed time — the thing an operator is
        // actually trying to see when a job "took 40 minutes". Left uninitialized:
        // both branches set it the instant they are admitted, and every path that
        // reaches neither has already returned.
        let wait_ms;
        // …and at which gates, for the outcome to name the one that bound it.
        let parked;

        let report;
        // The class this run's sample was *learned into*. Present only when the run
        // actually fed the learner: the audit record's `class` is the replay
        // substrate, so leaving it out is what stops a restart from re-learning a
        // sample the live learner deliberately refused.
        let mut class = None;
        let mut label;
        // The job GitHub actually dispatched to this runner, when it wasn't ours.
        let mut executed = None;
        if matches!(backend, Backend::Docker(_)) {
            let profile_name = orch.cfg.profiles[profile_idx].name.clone();
            // ADR-0026 §Consequences: a daemon only for a job that asks for
            // `[faber, dind]`, on a profile that grants one (`crate::dind`).
            // Decided first, on the per-job clone, so everything below — the
            // ramp, teardown, the audit outcome, the memory reservation, cohort
            // sizing and the disk charge — reads this job's answer rather than
            // the profile's grant, and a refused job never resolves (or builds)
            // an image.
            //
            // The rule is `[host] dind_requires_label` until the cut (#213).
            // Off or unstated, `decide` reads the grant alone and this block
            // changes nothing a runner gets: no `dind` label, no sentinel
            // `DOCKER_HOST`, never a refusal — main before the rule, byte for
            // byte. Only the counters count in both modes.
            let label_rule = orch.cfg.host.dind_label_rule().requires_label();
            if let Backend::Docker(dcfg) = &mut backend {
                match crate::dind::decide(&job_labels, dcfg.dind, label_rule, &profile_name) {
                    crate::dind::Decision::Provide => {
                        // The runner carries `dind` too, so GitHub can hand a dind
                        // job only to a runner that has a daemon, and a daemonless
                        // runner never advertises one. Only under the rule: off,
                        // runners register as they always have, without `dind`.
                        if label_rule
                            && !labels
                                .iter()
                                .any(|l| l.eq_ignore_ascii_case(crate::dind::LABEL))
                        {
                            labels.push(crate::dind::LABEL.to_string());
                        }
                        orch.metrics.lock().await.counts.dind_provided += 1;
                    }
                    crate::dind::Decision::Withhold if !label_rule => {
                        // The rule is off, so this is a profile that denies dind:
                        // it is already `Denied`, and ADR-0013 sends a denied
                        // runner no `DOCKER_HOST` at all.
                        orch.metrics.lock().await.counts.dind_withheld += 1;
                    }
                    crate::dind::Decision::Withhold => {
                        dcfg.dind = crate::docker::DindGrant::Denied;
                        // Tell the job why `docker` fails. A unix socket that does
                        // not exist fails at once, with the path in the message —
                        // "Cannot connect to the Docker daemon at unix:///faber-no-
                        // dind/add-dind-to-runs-on" — where a TCP address could
                        // hang out a connect timeout, which is why ADR-0013 sent
                        // a denied runner no `DOCKER_HOST` at all. A profile's own
                        // `DOCKER_HOST` is the operator's and wins.
                        dcfg.env
                            .entry("DOCKER_HOST".to_string())
                            .or_insert_with(|| NO_DIND_DOCKER_HOST.to_string());
                        orch.metrics.lock().await.counts.dind_withheld += 1;
                    }
                    crate::dind::Decision::Refuse { reason } => {
                        // Closed, like a refused `needs` below: no runner at all.
                        // A daemonless runner carrying `dind` could be handed a
                        // sibling's dind job routed to a profile that does grant
                        // one, and one without the label is a runner this job
                        // can never land on.
                        //
                        // The job stays queued and is re-tried on the provision
                        // throttle's cadence; only the first refusal warns.
                        let new = orch
                            .metrics
                            .lock()
                            .await
                            .note_dind_refused(DindRefusedView {
                                repo: repo.clone(),
                                run_id,
                                job_id,
                                job_name: job_name.clone(),
                                profile: profile_name.clone(),
                                reason: reason.clone(),
                                at_ms: now_ms(),
                            });
                        if new {
                            warn!(
                                "{repo} job {job_id}: asked for a Docker daemon and was refused — \
                                 {reason}; not minting a runner"
                            );
                        } else {
                            debug!("{repo} job {job_id}: dind still refused — {reason}");
                        }
                        orch.release_claim(job_id).await;
                        return;
                    }
                }
            }
            // ADR-0046: the services this job declared (`faber.postgres-16`,
            // `faber.redis`), decided next to `dind` and before the image, so a
            // refused job resolves nothing. Independent of dind: a job that asks
            // for services and not for a daemon gets the services and no daemon.
            // A job with no service label is Unlabelled without asking, which
            // keeps the path of every other job exactly what it was.
            let declares = job_labels
                .iter()
                .any(|l| is_service_label(l, &orch.cfg.services));
            if let (true, Backend::Docker(dcfg)) = (declares, &mut backend) {
                match needs::services(&job_labels, &orch.cfg.services) {
                    needs::ServiceOutcome::Unlabelled => {}
                    needs::ServiceOutcome::Provide(plan) => {
                        // The runner carries each service label, so GitHub hands
                        // a service job only to a runner whose services exist
                        // (ADR-0010 §3).
                        for l in plan.labels() {
                            if !labels.iter().any(|have| have.eq_ignore_ascii_case(&l)) {
                                labels.push(l);
                            }
                        }
                        dcfg.services = plan;
                    }
                    needs::ServiceOutcome::Refuse { label, reason } => {
                        orch.refuse_services(ServiceRefusedView {
                            repo: repo.clone(),
                            run_id,
                            job_id,
                            job_name: job_name.clone(),
                            profile: profile_name.clone(),
                            label,
                            reason,
                            at_ms: now_ms(),
                        })
                        .await;
                        orch.release_claim(job_id).await;
                        return;
                    }
                }
            }
            // Which image this job runs on. With ADR-0008 off this is only the
            // detached, surface-only drift check faber has always done; with it on
            // the answer decides what container to start, so it is resolved inline
            // — while the claim is still parked, so a build neither spends the
            // memory budget nor outlives a reclaim.
            if let Backend::Docker(dcfg) = &mut backend {
                if orch.images.enabled() {
                    // Usually a lockfile read; a variant build when the pin is new.
                    publish_wait(
                        &waiting,
                        WaitView::new("image", "resolving the runner image (may build a variant)"),
                    );
                    let resolved = tokio::select! {
                        biased;
                        _ = wait_cancelled(&mut cancel) => ImageResolution::Cancelled,
                        image = orch.resolve_runner_image(
                            &repo,
                            run_id,
                            job_id,
                            &head_sha,
                            &content_ref,
                            &dcfg.image,
                        ) => ImageResolution::Resolved(image),
                    };
                    let candidate = match resolved {
                        // Reclaimed mid-resolution (a whole run cancelled, say).
                        // Still parked, no container: release and stop, rather
                        // than finishing a build for a job that no longer wants one.
                        ImageResolution::Cancelled => {
                            orch.release_claim(job_id).await;
                            return;
                        }
                        ImageResolution::Resolved(candidate) => candidate,
                    };
                    // ADR-0009: a JIT runner is not job-pinned, so this container
                    // may be handed a *sibling's* job. Only start it once its
                    // image can serve everything this repo's live runners can —
                    // holding here, still parked, if it can't yet.
                    match orch
                        .await_pool_image(job_id, &repo, candidate, &mut cancel, &waiting)
                        .await
                    {
                        Some(image) => {
                            // ADR-0010: the job's `faber.*` needs become labels on
                            // the runner about to be registered, so GitHub itself
                            // will only hand this runner a job it can serve. They
                            // are judged against the image the pool decision
                            // settled on — the one the container really starts
                            // from — because a runner carrying a label its image
                            // cannot honour is a pool that lies about itself.
                            match needs::evaluate(&job_labels, &image.serves, &orch.cfg.services) {
                                needs::Outcome::Unlabelled => {}
                                needs::Outcome::Satisfied { labels: declared } => {
                                    labels.extend(declared)
                                }
                                needs::Outcome::Refused { reason } => {
                                    // The job stays queued and is claimed again
                                    // next pass: once per reason, and hourly.
                                    if crate::logline::repeats().fresh(
                                        &format!("needs-refused:{job_id}"),
                                        &reason,
                                        std::time::Instant::now(),
                                    ) {
                                        warn!(
                                            "{repo} job {job_id}: declared needs refused — \
                                             {reason}; not minting a runner it could never \
                                             land on"
                                        );
                                    }
                                    orch.release_claim(job_id).await;
                                    return;
                                }
                            }
                            // ADR-0011: what the chosen image serves, kept so it
                            // can be stamped on the container and rejoin the pool
                            // intact after a restart.
                            serves = image.serves.iter().cloned().collect();
                            dcfg.image = image.tag;
                        }
                        None => {
                            orch.release_claim(job_id).await;
                            return;
                        }
                    }
                } else {
                    orch.spawn_playwright_drift_check(
                        repo.clone(),
                        run_id,
                        job_id,
                        head_sha.clone(),
                        content_ref.clone(),
                        dcfg.image.clone(),
                    );
                }
            }
            // Docker: learned per-class caps + two-gate admission.
            //
            // Whether this job gets a daemon decides both what the ramp builds and
            // what the ledger is charged, so it is read once here and carried
            // through the whole claim (issue #43). `backend` is the per-job clone,
            // already narrowed by the dind decision above (ADR-0026).
            let dind = match &backend {
                Backend::Docker(d) => d.dind.granted(),
                Backend::MacosVm(_) => false,
            };
            let key = class_key(&repo, workflow_id, &job_name);
            let human = job_label(&repo, workflow_name.as_deref(), &job_name);
            // The ledger debits our own class, even though the containers are sized
            // for the whole queued cohort below (issue #13): the ceiling is a
            // per-container worst case, while the reservation is aggregate and
            // dispatch is a permutation over the cohort — see `Limiter::sizing_for`.
            let reservation = with_services(orch.limiter.reservation_for(&key, dind), &backend);
            // Admission can block a long time under budget pressure. If the claim is
            // reclaimed while we wait here — still parked, no container — bail before
            // spending a reservation or spawning a doomed runner. Dropping the
            // `admit` future is clean (it debits the ledger only on the synchronous
            // success path), so no reservation leaks. Every refusal is published, so
            // the claim names the gate it is parked behind (issue #66).
            //
            // A repo hold or a refused job that could take this runner (ADR-0030)
            // parks the claim ahead of admission, and is checked again once
            // admission is won: a block that appeared during a long admission wait
            // gives the reservation back and parks again, so nothing is minted
            // into it.
            //
            // A profile image confirmed missing (#63) parks the same way, and on the
            // same re-check: no runner is registered into an image that cannot start.
            //
            // While GitHub is failing mints, the claim waits out the backoff
            // (#89) ahead of admission, so the wait holds no memory or disk, and
            // takes its turn to mint only once admission is won. Taken any
            // earlier, every claim already parked in admission would mint the
            // moment a failed one freed its reservation. The turn is taken last,
            // after the repo and image re-checks, so a claim those send back to
            // park never spends a turn it cannot use.
            let start_image = match &backend {
                Backend::Docker(d) => d.image.clone(),
                Backend::MacosVm(_) => String::new(),
            };
            // ADR-0036: the operator's priority repos queue ahead of the rest.
            let mut rank = if orch.limiter.is_priority_repo(&repo) {
                Rank::Priority
            } else {
                Rank::Normal
            };
            // ADR-0063: what a waste check found once this claim was demoted, so
            // its refusals name the signal. A demote is never undone.
            let mut demoted: Option<crate::waste::Finding> = None;
            // Whether a `record` finding was already audited, so a claim that
            // loops back (a failed mint turn) is counted once.
            let mut recorded = false;
            // ADR-0056 §2: the image is settled, so `prepare` ends and `parked`
            // begins.
            footprint.lock().unwrap().prepared_ms = Some(now_ms());
            let guard = loop {
                if !orch
                    .await_repo_clear(&repo, &labels, &mut cancel, &waiting)
                    .await
                    || !orch.await_mint_clear(&mut cancel, &waiting).await
                {
                    break None;
                }
                if !orch
                    .await_profile_image(&profile_name, &start_image, &mut cancel, &waiting)
                    .await
                {
                    break None;
                }
                let g = tokio::select! {
                    biased;
                    _ = wait_cancelled(&mut cancel) => None,
                    g = orch.limiter.admit_ranked(reservation, rank, |w| {
                        publish_wait(&waiting, wasted_wait(demoted.as_ref(), pr, w.view()))
                    }) => Some(g),
                };
                let Some(g) = g else { break None };
                if orch.repo_block(&repo, &labels).await.is_some()
                    || orch.image_block(&profile_name, &start_image).is_some()
                {
                    drop(g);
                    continue;
                }
                // ADR-0063 §4: the gates passed and nothing is minted yet. The
                // claim holds its slot for one read at most.
                if let Some(pr) = pr {
                    let subject = crate::waste::Subject {
                        repo: &repo,
                        pr,
                        run_id,
                        head_sha: &head_sha,
                        job_id,
                        run_created_at: run_created_at.as_deref(),
                    };
                    let finding = orch.check_waste(subject).await;
                    let wait_ms = now_ms().saturating_sub(start);
                    match finding.as_ref().map(|f| f.action) {
                        Some(WasteMode::Skip) => {
                            let f = finding.unwrap();
                            orch.record_waste(subject, &f, "skip", wait_ms, &human)
                                .await;
                            drop(g);
                            orch.skip_wasted(subject, &f).await;
                            break None;
                        }
                        // A demoted claim is already last: it reached here because
                        // nothing ahead of it could use the room, so it runs.
                        Some(WasteMode::Demote) if demoted.is_none() => {
                            let f = finding.unwrap();
                            orch.record_waste(subject, &f, "demote", wait_ms, &human)
                                .await;
                            info!(
                                "{repo} job {job_id}: likely wasted ({}, PR #{pr}) — moved to \
                                 the waste tier",
                                f.names()
                            );
                            if let Some(ticket) = g.ticket() {
                                rank = Rank::Waste { ticket };
                            }
                            demoted = Some(f);
                            drop(g);
                            continue;
                        }
                        Some(WasteMode::Record) if demoted.is_none() && !recorded => {
                            let f = finding.unwrap();
                            orch.record_waste(subject, &f, "record", wait_ms, &human)
                                .await;
                            recorded = true;
                        }
                        _ => {}
                    }
                }
                if orch.gh.mint_turn(now_ms()).is_err() {
                    drop(g);
                    continue;
                }
                break Some(g);
            };
            // Dropped on any bail-out below, or once the backend has torn the job
            // down, this gives the job's disk charge back with its memory (#89, #161).
            let Some(held) = guard else {
                orch.release_claim(job_id).await;
                return;
            };
            // What admission actually debited: the p75 reservation under
            // `reserve = "p75"` (ADR-0033 §1), the cap otherwise. The label below
            // must carry this, so an adoption re-charges what was debited.
            let reservation = held.reservation();
            wait_ms = now_ms().saturating_sub(start);
            {
                let mut f = footprint.lock().unwrap();
                f.admitted_ms = Some(start + wait_ms);
                f.reserved_mcpu = Some(reservation.mcpu);
            }
            // Won admission at the very instant drain began: bail before spawning a
            // container. Still parked here — no runner to orphan — so release
            // cleanly rather than making the drain wait on a container it never saw.
            if orch.is_draining() {
                orch.release_claim(job_id).await;
                return;
            }
            // Reclaimed in the instant between the admission `select!` resolving and
            // this line. Narrow, but worth closing at the cheapest point there is:
            // bailing here returns the reservation while the claim is still parked,
            // whereas one line later it would fund a whole provisioning ramp for a
            // job that no longer needs a runner.
            if *cancel.borrow() {
                orch.release_claim(job_id).await;
                return;
            }
            // A container is about to exist: from here the reconciler judges the
            // claim on its job, its runner, and this container together. The ramp
            // that follows is bounded *and* cancellable at every seam (see
            // [`crate::docker::run_one`]) — a reclaim issued during it is honoured
            // within one step rather than after the whole ramp.
            (name, parked) = admit_claim(job_id, &orch.instance, &admitted, &runner_name, &waiting);
            orch.record_mint(&repo, job_id, &name, profile_idx).await;
            // Re-read the cohort now that we're launching: a claim can park for a
            // long time under budget pressure, and the queued set moves on while it
            // waits. The reservation is already debited and stays as admitted; only
            // the container ceiling is (re-)derived here.
            let sizing =
                orch.limiter
                    .sizing_for(&key, &orch.cohort(&repo, profile_idx).await, dind);
            let limits = sizing.limits;
            let capped = limits.runner.memory_mib + limits.dind.memory_mib;
            info!(
                "provisioning docker runner for {repo} job {job_id} ({}reserve {} MiB / {} cores, \
                 cap {capped} MiB{}{})",
                if sizing.profiling { "profiling, " } else { "" },
                reservation.mib,
                fmt_cores(reservation.mcpu),
                // The quota only exists with CPU admission on (ADR-0019).
                if limits.runner.cpus > 0.0 {
                    format!(
                        ", runner quota {} cores",
                        fmt_cores(crate::docker::mcpu(limits.runner.cpus))
                    )
                } else {
                    String::new()
                },
                match &sizing.raised_by {
                    Some(peer) => format!(" — sized for queued peer {peer}"),
                    None => String::new(),
                }
            );
            // ADR-0011: everything a later process needs to rebuild this claim,
            // stamped on the container it is about to start. Assembled here because
            // this is the one point that knows all five at once — the reservation in
            // particular exists nowhere else, the container being sized for the whole
            // cohort rather than for what this claim was charged.
            let claim_marks = ClaimMarks {
                run_id,
                attempt,
                profile: orch.cfg.profiles[profile_idx].name.clone(),
                class: key.clone(),
                reserved_mib: reservation.mib,
                reserved_mcpu: reservation.mcpu,
                serves: serves.join(","),
                // ADR-0046 §10: which services this runner has, so an adopted
                // claim can detach them and its outcome can name them.
                services: services_of(&backend).mark(),
            };
            let mut ran = run_backend(
                &backend,
                &orch.gh,
                &repo,
                job_id,
                &name,
                &labels,
                &limits,
                &claim_marks,
                None,
                cancel.clone(),
            )
            .await;
            // A backend error carries no report of its own (`run_backend`), and a
            // service that failed to start is exactly the infra error an operator
            // needs to see named on the outcome.
            if ran.services.is_empty() {
                ran.services = services_of(&backend).names();
            }
            report = ran;
            // A lifetime-cap kill, an idle reclaim, or an infra failure ran no
            // representative job, so keep all three out of the learner — but still
            // record the outcome below for visibility.
            label = Some(human);
            if !report.timed_out && !report.reclaimed && !report.infra_error {
                // Attribute to the job GitHub actually dispatched to this runner, not
                // the one it was minted for — they differ routinely (see `ExecutedJob`).
                // Unknown ⇒ don't guess: an unattributable sample would poison a class.
                match orch.executed_class(&repo, &name, start + wait_ms).await {
                    Some((ran_key, ran_human, ran_job)) => {
                        if ran_job != job_id {
                            info!(
                                "runner {name} ran job {ran_job} ({ran_human}), not the job it was \
                                 minted for ({job_id}) — learning attributed to the job that ran"
                            );
                            executed = Some(ran_job);
                            label = Some(ran_human.clone());
                            // Cohort sizing covers every class seen queued in this
                            // repo within COHORT_TTL, so a shortfall here means the
                            // class had never been seen at all — a brand-new job
                            // name, or one dormant longer than the TTL. Name it: it
                            // is exactly what an otherwise-inexplicable OOM looks
                            // like from the job log's side.
                            let wanted = orch.limiter.reservation_for(&ran_key, dind).mib;
                            if wanted > capped {
                                warn!(
                                    "runner {name} ran a job whose class wants {wanted} MiB in \
                                     containers capped at {capped} MiB — that class was not in \
                                     this repo's sizing cohort when the runner launched \
                                     (oom_runner={} oom_dind={})",
                                    report.oom_runner, report.oom_dind
                                );
                            }
                        }
                        // ADR-0033 §5, under "p75" only: a host OOM is not a
                        // class sample. Under "max" this is the report itself.
                        match orch.limiter.learnable(&report) {
                            Some(learned) => {
                                orch.limiter.record_run(&ran_key, &ran_human, &learned);
                                class = Some(ran_key);
                            }
                            None => info!(
                                "runner {name}: killed by the VM-wide OOM killer, not by its \
                                 own cap — keeping this run out of the learner (ADR-0033 §5)"
                            ),
                        }
                    }
                    None => info!(
                        "runner {name}: no GitHub job records it as its runner — \
                         keeping this run out of the learner"
                    ),
                }
            }
            // `guard` drops here, releasing the reservation back to the ledger.
        } else {
            // A macOS VM job takes VM admission (ADR-0026, #263): the hold, the
            // wall, the class cap, the share and the disk floor. A repo hold and a
            // refused job that could take this runner are per repo (ADR-0030), and
            // it is minted the same way as a Docker runner, so it waits its turn.
            //
            // ADR-0046 §10: only Docker profiles provide services. A service label
            // routed here is refused, not ignored: a runner without the label is
            // one the job can never land on, and one carrying it would promise a
            // database nothing started.
            if let Some(label) = service_label_on_non_docker(&job_labels, &orch.cfg.services) {
                orch.refuse_services(ServiceRefusedView {
                    repo: repo.clone(),
                    run_id,
                    job_id,
                    job_name: job_name.clone(),
                    profile: orch.cfg.profiles[profile_idx].name.clone(),
                    label,
                    reason: NON_DOCKER_SERVICE_REASON.to_string(),
                    at_ms: now_ms(),
                })
                .await;
                orch.release_claim(job_id).await;
                return;
            }
            // ADR-0056 §2: a VM job resolves no image, so it starts waiting
            // for admission at once.
            footprint.lock().unwrap().prepared_ms = Some(now_ms());
            let slot = loop {
                if !orch
                    .await_repo_clear(&repo, &labels, &mut cancel, &waiting)
                    .await
                    || !orch.await_mint_clear(&mut cancel, &waiting).await
                {
                    break None;
                }
                let image = match &orch.cfg.profiles[profile_idx].backend {
                    Backend::MacosVm(vm) => Some(vm.image.as_str()),
                    Backend::Docker(_) => None,
                };
                let slot = orch.vm_slot(image, &mut cancel, &waiting).await;
                match slot {
                    Some(p) if orch.repo_block(&repo, &labels).await.is_some() => drop(p),
                    Some(p) if orch.gh.mint_turn(now_ms()).is_err() => drop(p),
                    other => break other,
                }
            };
            // The admission goes to the backend, which holds it until the VM is
            // stopped.
            let Some(vm) = slot else {
                orch.release_claim(job_id).await;
                return;
            };
            wait_ms = now_ms().saturating_sub(start);
            footprint.lock().unwrap().admitted_ms = Some(start + wait_ms);
            // Same drain race as the Docker branch: if drain began while we waited
            // on a permit, bail before spawning — still parked, no runner to orphan.
            if orch.is_draining() {
                orch.release_claim(job_id).await;
                return;
            }
            // Same reclaim race, too: a reclaim already in hand is reason not to
            // start a VM at all.
            if *cancel.borrow() {
                orch.release_claim(job_id).await;
                return;
            }
            (name, parked) = admit_claim(job_id, &orch.instance, &admitted, &runner_name, &waiting);
            orch.record_mint(&repo, job_id, &name, profile_idx).await;
            info!(
                "provisioning {} runner for {repo} job {job_id}",
                backend.kind()
            );
            report = run_backend(
                &backend,
                &orch.gh,
                &repo,
                job_id,
                &name,
                &labels,
                &Limits::default(),
                // No container to stamp, so nothing to adopt (ADR-0011 §8).
                &ClaimMarks::default(),
                Some(vm),
                cancel.clone(),
            )
            .await;
            // A VM run is not measured by the Docker learner, so it feeds no class.
            // It still names the job GitHub ran on its runner (ADR-0044), as a Docker
            // run does: a JIT runner is not job-pinned, and a macOS runner is handed
            // whichever queued job with its labels is oldest, so the verdict fetch
            // would otherwise ask about the job it was minted for, which another
            // runner ran or is still queued, and find no conclusion.
            label = Some(job_label(&repo, workflow_name.as_deref(), &job_name));
            if !report.timed_out && !report.reclaimed && !report.infra_error {
                match orch.executed_class(&repo, &name, start + wait_ms).await {
                    Some((_, ran_human, ran_job)) if ran_job != job_id => {
                        info!(
                            "runner {name} ran job {ran_job} ({ran_human}), not the job it was \
                             minted for ({job_id})"
                        );
                        executed = Some(ran_job);
                        label = Some(ran_human);
                    }
                    Some(_) => {}
                    None => info!("runner {name}: no GitHub job records it as its runner"),
                }
            }
        }

        orch.record_outcome(
            Finished {
                repo: &repo,
                job_id,
                executed_job_id: executed,
                class: class.as_deref(),
                label: label.as_deref(),
                profile: &orch.cfg.profiles[profile_idx].name,
                runner: &name,
                attempt,
                wait_ms,
                parked: &parked,
                duration_ms: now_ms().saturating_sub(start),
            },
            &report,
        )
        .await;
        // Release the claim (and its idle timer) and wake any drain waiter. The
        // sole normal-exit release; a cancel bail-out above uses the same path.
        orch.release_claim(job_id).await;
    })
}

/// Mark a claim admitted, and mint the name of the runner it is about to register.
///
/// The name is `faber-{job_id}-{ts}`, and `ts` is read **here**, past every wait a
/// claim can park in (image resolution, the pool hold, admission) and moments
/// before the backend registers the runner. GitHub's runner list carries no age,
/// so that timestamp is the only one anything has: [`runner_is_supply`] credits a
/// connecting runner by it and [`stale_runner_ids`] reaps an offline one by it, on
/// this host and on every peer sharing the pool. Read when the claim was taken, a
/// runner whose claim parked for ten minutes registered already outside both
/// windows, so demand minted one runner too many and a sweep could delete its
/// registration before the listener connected (ADR-0018).
///
/// The timestamp is also what keeps a name unique per attempt rather than per job.
/// A runner that dies without taking its job leaves the job queued and its
/// registration listed, and a re-provision under the same name would be refused
/// with a `409`. On one host, a later attempt for the same job is admitted only
/// after this claim is released, so its timestamp is later.
///
/// The name is published before the flag flips (ADR-0017), so the reconciler never
/// judges an admitted claim without knowing exactly which runner is its own.
///
/// The claim's `waiting` reason is cleared here too, the one point both kinds of
/// admission pass through, and before the flag flips — so no reader ever sees an
/// admitted claim still giving a reason to wait.
///
/// Settling the cell also hands back how long the claim waited at each gate, for
/// the outcome's audit line, and counts its binding gate (issue #85).
fn admit_claim(
    job_id: u64,
    own_instance: &str,
    admitted: &AtomicBool,
    runner_name: &std::sync::Mutex<Option<String>>,
    waiting: &std::sync::Mutex<Parking>,
) -> (String, Parked) {
    let at = now_ms();
    // The instance goes last (#94): every parser, this binary's and an older
    // peer's, reads the job id and mint time from the first two fields.
    let name = instance::runner_name(job_id, at, own_instance);
    let parked = waiting.lock().unwrap().settle(at);
    *runner_name.lock().unwrap() = Some(name.clone());
    admitted.store(true, Ordering::SeqCst);
    (name, parked)
}

/// Say why a parked claim is waiting, overwriting whatever it said before.
fn publish_wait(waiting: &std::sync::Mutex<Parking>, wait: WaitView) {
    waiting.lock().unwrap().publish(wait, now_ms());
}

/// The refusal a claim demoted as likely wasted publishes (ADR-0063 §5): gate
/// `wasted`, naming the signal and the PR, then the gate that refused it. Any
/// other claim's refusal passes through unchanged.
fn wasted_wait(
    demoted: Option<&crate::waste::Finding>,
    pr: Option<u64>,
    view: WaitView,
) -> WaitView {
    match (demoted, pr) {
        (Some(f), Some(pr)) => WaitView::new(
            "wasted",
            format!(
                "set aside: likely wasted ({}, PR #{pr}); {}",
                f.signal().as_str(),
                view.detail
            ),
        ),
        _ => view,
    }
}

/// How resolving a job's runner image (ADR-0008) ended. Its own type because
/// "cancelled" and "resolved" are different instructions to the caller — the
/// first stops the provision, the second carries on to the pool-coherence check
/// — and an `Option<PoolImage>` says neither out loud.
enum ImageResolution {
    /// The claim was reclaimed while the image was being resolved. Nothing has
    /// been created; the caller releases and stops.
    Cancelled,
    /// Resolved: the tag this job would run on, and what that tag serves. Still
    /// a candidate — [`Orchestrator::await_pool_image`] has the last word.
    Resolved(PoolImage),
}

/// The claim id (`org.faber.id`, `<job>-<ts>[-<instance>]`) of every runner
/// container a scan found, adoptable or not: the set whose service roles survive
/// a restart (ADR-0046 §7). An opaque runner counts by its name, the only thing
/// it is known by; one whose name carries no claim id has no role to keep.
fn live_claim_ids(scan: &crate::docker::Scan) -> Vec<String> {
    let mut ids: Vec<String> = scan
        .adoptable
        .iter()
        .map(|a| a.claim_id().to_string())
        .chain(
            scan.opaque
                .iter()
                .filter_map(|o| o.name.strip_prefix("faber-runner-"))
                .filter(|id| crate::untrusted::claim_id(id).is_some())
                .map(str::to_string),
        )
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Why a service label routed to a macOS VM profile is refused
/// (ADR-0046 §10): services run beside a runner container, in the Linux VM.
const NON_DOCKER_SERVICE_REASON: &str =
    "only Docker profiles provide services; route this job to a Docker profile";

/// Whether `label` declares a service (`faber.postgres-16`, `faber.redis`),
/// parsed as [`needs::parse`] parses every `faber.*` label, ignoring ASCII case
/// as GitHub does, so it errs toward finding one. Whether the host provides it,
/// and in that form, is [`needs::services`]' question, not this one's.
fn is_service_label(label: &str, services: &crate::services::ServicesPolicy) -> bool {
    needs::parse(std::slice::from_ref(&label.to_ascii_lowercase()))
        .first()
        .is_some_and(|n| services.is_service_label(&n.tool, n.version.as_deref()))
}

/// The first service label of a job that was routed to a profile which is not
/// Docker, which ADR-0046 §10 refuses. `None` for a job that declared none.
fn service_label_on_non_docker(
    job_labels: &[String],
    services: &crate::services::ServicesPolicy,
) -> Option<String> {
    job_labels
        .iter()
        .find(|l| is_service_label(l, services))
        .cloned()
}

/// The services the per-job backend clone carries (ADR-0046), or none for a
/// backend that cannot have any.
fn services_of(backend: &Backend) -> crate::services::ServicePlan {
    match backend {
        Backend::Docker(d) => d.services.clone(),
        Backend::MacosVm(_) => crate::services::ServicePlan::default(),
    }
}

/// `reservation` with the job's per-job service containers added (ADR-0046 §8):
/// each runs under its table's `memory_mib` as its cap, and that memory is the
/// job's. Added to the p75 shadow too, because the shadow is what `"p75"` would
/// debit and the service containers are spent under either mode. A shared
/// server's memory is held by the server, not here (§7).
fn with_services(mut reservation: Reservation, backend: &Backend) -> Reservation {
    let extra = services_of(backend).per_job_mib();
    reservation.mib = reservation.mib.saturating_add(extra);
    reservation.shadow_mib = reservation.shadow_mib.saturating_add(extra);
    reservation
}

/// Run one job through a backend, turning a transport/setup error into a failed
/// [`RunReport`] (the job simply never ran) so the outcome + learning paths stay
/// uniform whether the backend errored or the job itself exited non-zero.
#[allow(clippy::too_many_arguments)]
async fn run_backend(
    backend: &Backend,
    gh: &Github,
    repo: &str,
    job_id: u64,
    name: &str,
    labels: &[String],
    limits: &Limits,
    claim: &ClaimMarks,
    vm: Option<VmRun>,
    cancel: watch::Receiver<bool>,
) -> RunReport {
    match backend
        .run_one(gh, repo, name, labels, limits, claim, vm, cancel)
        .await
    {
        Ok(report) => report,
        Err(e) => {
            error!("runner for {repo} job {job_id} failed: {e:#}");
            let lost = crate::docker::runner_was_lost(&e);
            // The backend errored before a job could run (sandbox setup / JIT mint /
            // launch) — faber's own infra, not a job that ran and failed. Flag it so
            // the outcome records `infra_error`, distinct from a clean non-zero exit.
            //
            // `{e:#}` is the whole chain, not just the outermost message: the
            // backend's errors are layered ("starting dind faber-dind-x: docker run
            // failed: …"), and the outer half alone names the step without saying
            // what went wrong at it. Carrying it into the report is what turns
            // `infra_error` from a category into a diagnosis — before this the
            // cause reached stderr and nowhere else, so the audit log recorded that
            // something broke and destroyed the only account of what.
            RunReport {
                infra_error: true,
                lost,
                cause: Some(format!("{e:#}")),
                ..Default::default()
            }
        }
    }
}

/// The autolimiter class + human label a finished runner's measurements belong to,
/// plus the id of the job that actually ran (for the log line when it differs from
/// the claim).
type Attribution = (String, String, u64);

/// Whether a runner registered with `runner_labels` could be handed a job asking
/// for `job_labels`.
///
/// GitHub dispatches a job to any runner carrying **all** of the job's `runs-on`
/// labels, so `job ⊆ runner` is the operative rule. faber's own routing uses the
/// other containment (`profile ⊆ job` — see [`match_profile`]), and the two agree
/// in the normal case where a profile's labels *are* the workflow's `runs-on`.
/// Accepting either direction keeps the cohort a superset of what GitHub can
/// actually dispatch under both conventions, and erring wide can only over-size a
/// container — never under-size it, which is the failure this exists to prevent.
fn labels_compatible(runner_labels: &[String], job_labels: &[String]) -> bool {
    !runner_labels.is_empty()
        && !job_labels.is_empty()
        && (job_labels.iter().all(|l| runner_labels.contains(l))
            || runner_labels.iter().all(|l| job_labels.contains(l)))
}

/// The **sizing cohort** per profile: the autolimiter classes of every job queued
/// in this repo that a runner minted for that profile could be handed.
///
/// A JIT runner is registered against one repo, so the cohort never crosses repos.
/// Only `queued` jobs count — an `in_progress` one already has a runner and will
/// not be dispatched again.
///
/// Gate verdicts are deliberately *not* consulted: GitHub knows nothing about
/// faber's trust policy, so a queued job faber would refuse to provision for can
/// still be handed to a runner that is already registered. Sizing has to cover it.
fn queued_cohorts(
    profiles: &[Profile],
    repo: &str,
    runs: &[(WorkflowRun, Vec<Job>)],
) -> HashMap<usize, Vec<String>> {
    // Sorted + deduped: many jobs collapse onto few classes, and a stable order
    // keeps the snapshot (and its tests) deterministic.
    let mut out: HashMap<usize, BTreeSet<String>> = HashMap::new();
    for (run, jobs) in runs {
        for job in jobs.iter().filter(|j| j.status == "queued") {
            let key = class_key(repo, run.workflow_id, &job.name);
            for (idx, profile) in profiles.iter().enumerate() {
                if labels_compatible(&profile.labels, &job.labels) {
                    out.entry(idx).or_default().insert(key.clone());
                }
            }
        }
    }
    out.into_iter()
        .map(|(idx, keys)| (idx, keys.into_iter().collect()))
        .collect()
}

/// One repo's sizing cohort: profile index → class → when it was last seen queued.
type CohortSightings = HashMap<usize, HashMap<String, u64>>;

/// Fold one scan's queued cohort into the accumulated per-profile set, then drop
/// classes unseen for [`COHORT_TTL`].
///
/// # Why the cohort outlives the scan that found it
///
/// [`queued_cohorts`] answers "what is queued *right now*", and sizing a container
/// from that alone leaves the hole this exists to close. A JIT runner is eligible
/// for a job from the moment it registers until it picks one up or hits its
/// lifetime cap — and GitHub dispatches to any runner carrying the job's labels, so
/// a job that queues *after* the runner launched can still land on it. It was never
/// in the snapshot, so nothing sized for it.
///
/// That is not a corner case. Every `runs-on` in the observed repo is the same
/// `[faber, docker]` pair, so the pool is undifferentiated: every runner can be
/// handed every job. Across one live log, about two hundred runs executed a class
/// that was absent from their launch cohort and about two thirds of those were
/// OOM-killed — a job wanting several GiB in containers capped well under one GiB
/// because the runner had been minted, a couple of minutes earlier, for a deploy
/// job whose learned cap sits at the floor. The same shortfall kills the dind
/// sidecar: at the 256 MiB floor it is OOM-killed part-way through the concurrent
/// image pull that boots an e2e stack, which the job sees as a truncated pull
/// (`unexpected EOF`) followed by a daemon that no longer resolves — a registry
/// flake, from the job's side.
///
/// Accumulating instead means the ceiling covers every class that could still be
/// dispatched here, not just the ones mid-flight when the scan ran, which is what
/// [`Limiter::sizing_for`] always meant by "cohort". The set can only grow the
/// ceiling, never shrink it: `sizing_for` maxes over each class's *stored* cap,
/// already clamped by the escalation guard, and the reservation stays the
/// minted-for class's own — so this changes what a container may use, not what the
/// ledger believes is in use.
fn merge_cohorts(acc: &mut CohortSightings, fresh: HashMap<usize, Vec<String>>, now: u64) {
    for (idx, classes) in fresh {
        let seen = acc.entry(idx).or_default();
        for class in classes {
            seen.insert(class, now);
        }
    }
    let ttl = COHORT_TTL.as_millis() as u64;
    for classes in acc.values_mut() {
        classes.retain(|_, seen| now.saturating_sub(*seen) <= ttl);
    }
    acc.retain(|_, classes| !classes.is_empty());
}

/// Rebuild the limiter's learned caps and CPU shares from replayed audit history,
/// so a restart does not re-profile every class.
///
/// Every replayed record already qualified for learning (it carries a `class`),
/// and CPU is seeded from exactly those records, through the same censoring rule a
/// live run gets — a throttled sample comes back as "at least this much", not as
/// the quota it was held to.
fn seed_limiter(limiter: &Limiter, replayed: &[OutcomeReplay]) {
    for o in replayed {
        let label = o
            .label
            .clone()
            .unwrap_or_else(|| o.class.replace("::", " · "));
        let (oom_runner, oom_dind) = o.censored();
        limiter.seed_censored(
            &o.class,
            &label,
            (o.mem_peak_runner_mib, oom_runner),
            (o.mem_peak_dind_mib, oom_dind),
        );
        limiter.seed_cpu(
            &o.class,
            &label,
            o.runner_cpu(),
            o.cpu_quota_runner_mcpu.unwrap_or(0),
            o.dind_cpu(),
        );
    }
}

/// The boot warning for a Docker host whose config never said whether admission
/// should be CPU-aware (ADR-0019).
///
/// Per AGENTS.md an absent key that would tighten admission keeps the old
/// behaviour, and the new state is made loud instead. Emitted by
/// [`Orchestrator::new`] — once per daemon start, never on a settings save — and
/// only where there is a Docker profile, since a host without one has nothing the
/// setting could change.
fn cpu_unstated_warning(
    profiles: &[Profile],
    mode: crate::config::CpuMode,
) -> Option<&'static str> {
    let docker = profiles
        .iter()
        .any(|p| matches!(p.backend, Backend::Docker(_)));
    (docker && mode.is_unstated()).then_some(
        "autolimit.cpu is unstated — CPU-aware admission (ADR-0019) is off; set cpu = \"auto\" \
         to enable it or cpu = \"off\" to silence this",
    )
}

/// The boot warnings about a Docker host's disk admission floor (ADR-0024 §1).
///
/// Two, on the same terms as [`cpu_unstated_warning`] — once per daemon start,
/// only where there is a Docker profile. An unstated floor keeps the old
/// behaviour and says so, per AGENTS.md. A floor above the reclamation floor is
/// what `faber check-config` and the settings save reject; a file that arrived by
/// unattended update is only warned about, since refusing to boot on it would be
/// a dead daemon with ADR-0006's rollback flapping against it.
fn disk_admission_warnings(profiles: &[Profile], host: &HostPolicy) -> Vec<String> {
    let docker = profiles
        .iter()
        .any(|p| matches!(p.backend, Backend::Docker(_)));
    if !docker {
        return Vec::new();
    }
    let mut out = Vec::new();
    if host.disk_admission().is_unstated() {
        out.push(
            "host.disk_admission_floor_gib is unstated — the disk admission gate (ADR-0024) is \
             off: faber admits Docker jobs, and restarts a wedged Docker, however full the disk \
             is; set a floor in GiB (e.g. 10) to enable it, or 0 to silence this"
                .to_string(),
        );
    }
    if let Some(out_of_order) = host.disk_floors_out_of_order() {
        out.push(format!(
            "{out_of_order}. Run `faber check-config` to fix the file."
        ));
    }
    out
}

/// The boot warning while `[host] dind_requires_label` is not `true` (ADR-0026
/// § Consequences).
///
/// Every boot, only where there is a Docker profile, for unstated *and* off: the
/// key is a scheduled switch, not a standing choice — the cut's host sitting
/// sets it `true` and #213 then removes it — so a stated `false` is a host that
/// has not reached the cut yet, and says so too.
fn dind_label_rule_warning(profiles: &[Profile], host: &HostPolicy) -> Option<String> {
    let docker = profiles
        .iter()
        .any(|p| matches!(p.backend, Backend::Docker(_)));
    let rule = host.dind_label_rule();
    (docker && !rule.requires_label()).then(|| {
        format!(
            "host.dind_requires_label is {} — the dind label rule (ADR-0026) is off: every job \
             on a profile that grants dind gets a Docker daemon, whether or not its runs-on \
             includes `dind`. The cut's host sitting sets `[host] dind_requires_label = true` \
             (add `dind` to the runs-on of every workflow that runs `docker` first)",
            rule.as_str()
        )
    })
}

/// The boot warning for a host whose Docker admission is held (issue #66).
///
/// Emitted by [`Orchestrator::new`] at every start while the hold lasts. The zero
/// ceiling a hold replaces went through several restarts without a word, so this
/// names the reason, the date and the way out every time the daemon comes up.
fn hold_warning(hold: &Hold) -> String {
    format!(
        "Docker admission is held since {}: {:?} — running jobs finish, but no new Docker \
         job starts until it is released (POST /api/unhold, or Release in the dashboard or \
         tray)",
        hold.since_utc(),
        hold.reason
    )
}

/// Rebuild every repo's sizing cohort from replayed audit history.
///
/// Without this a restart starts every repo at an empty cohort, and the first
/// runners minted are sized for whatever happens to be queued in that instant —
/// the exact hole [`merge_cohorts`] closes, reopened for as long as it takes each
/// class to be *seen queued* again. That is not a poll interval: a class only
/// re-enters a live cohort when it next queues, so a nightly or weekly workflow
/// leaves its slot empty for a day or a week. Observed directly — a redeploy, then
/// a build job OOM-killed at an undersized cap on a runner minted seconds after
/// startup, while the same job on the same commit passed once the cohort had
/// filled.
///
/// A run is replayed into the profile that actually served it, which the outcome
/// record names, rather than re-derived from labels no longer in hand. Lines
/// written before that field existed carry no profile and are skipped: the cohort
/// they would join is a guess, and the live scan re-adds the class the first time
/// it queues anyway. Sightings age on the same [`COHORT_TTL`] clock as live ones,
/// from the timestamp of the run itself.
fn seeded_cohorts(
    profiles: &[Profile],
    outcomes: &[OutcomeReplay],
    now: u64,
) -> HashMap<String, CohortSightings> {
    let index_of = |name: &str| profiles.iter().position(|p| p.name == name);
    let ttl = COHORT_TTL.as_millis() as u64;
    let mut out: HashMap<String, CohortSightings> = HashMap::new();
    for o in outcomes {
        if o.repo.is_empty() || now.saturating_sub(o.ts_ms) > ttl {
            continue;
        }
        let Some(idx) = o.profile.as_deref().and_then(index_of) else {
            continue;
        };
        let seen = out
            .entry(o.repo.clone())
            .or_default()
            .entry(idx)
            .or_default()
            .entry(o.class.clone())
            .or_insert(o.ts_ms);
        *seen = (*seen).max(o.ts_ms);
    }
    out
}

/// Job ids GitHub still reports as `queued`/`in_progress` in this scan — the
/// liveness signal for parked claims.
///
/// Deliberately narrower than the caller's `seen` set, which also holds the
/// *finished* jobs of active runs so their dedup entries survive the prune. Reusing
/// `seen` for liveness kept a finished job's claim alive for as long as any sibling
/// in its run was still running.
fn live_job_ids(runs: &[(WorkflowRun, Vec<Job>)]) -> HashSet<u64> {
    runs.iter()
        .flat_map(|(_, jobs)| jobs.iter())
        .filter(|j| j.status == "queued" || j.status == "in_progress")
        .map(|j| j.id)
        .collect()
}

/// Fold this cycle's scan into the runner→executed-job map, then drop mappings past
/// [`EXECUTED_TTL`] so it tracks the live working set instead of growing forever.
///
/// A job carries its `runner_name` from the moment GitHub dispatches it, so a run
/// lasting longer than one poll interval is always captured before its runner tears
/// down. Only faber's own runners are recorded — a peer's or a human's runner is not
/// ours to learn from.
fn harvest_executed(
    map: &mut HashMap<String, ExecutedJob>,
    runs: &[(WorkflowRun, Vec<Job>)],
    now: u64,
) {
    for (run, jobs) in runs {
        for job in jobs {
            let Some(runner) = job.runner_name.as_deref() else {
                continue; // still queued — no runner assigned yet
            };
            if !runner.starts_with("faber-") {
                continue;
            }
            map.insert(
                runner.to_string(),
                ExecutedJob {
                    job_id: job.id,
                    workflow_id: run.workflow_id,
                    workflow_name: run.name.clone(),
                    job_name: job.name.clone(),
                    seen_ms: now,
                },
            );
        }
    }
    let ttl = EXECUTED_TTL.as_millis() as u64;
    map.retain(|_, e| now.saturating_sub(e.seen_ms) <= ttl);
}

/// Look up the class to attribute a finished runner's measurements to.
fn executed_class(
    map: &HashMap<String, ExecutedJob>,
    repo: &str,
    runner: &str,
) -> Option<Attribution> {
    let e = map.get(runner)?;
    Some((
        class_key(repo, e.workflow_id, &e.job_name),
        format!(
            "{repo} · {} · {}",
            e.workflow_name.as_deref().unwrap_or("workflow"),
            e.job_name
        ),
        e.job_id,
    ))
}

/// Resolve only on a genuine cancel; pend forever if every sender has dropped, so
/// a dropped sender never reads as a cancel. Mirrors the docker-side helper; used
/// to race an admission wait against a reclaim.
async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    if cancel.wait_for(|c| *c).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Only `completed` is a terminal job status. `queued` / `in_progress` / `waiting`
/// (an environment gate) may still want a runner, so are never treated as done.
fn is_terminal(status: &str) -> bool {
    status == "completed"
}

/// Signal a claim to release itself. The owning provision task, seeing the flip,
/// exits through its own cleanup: parked ⇒ before spawning; admitted ⇒ ask the
/// runner whether it is really idle and, only if it is, kill it, teardown, release
/// (ADR-0017). A no-op if the task already finished.
fn reclaim(job_id: u64, cancel: &watch::Sender<bool>, why: &str) {
    info!("reconcile: requesting reclaim of job {job_id}'s claim ({why})");
    // `send_replace` (not `send`) stores the flag even with zero receivers, so a
    // task that has not yet `subscribe`d still sees it via its initial `borrow()` —
    // and it wakes a runner's watcher again even when an earlier request it refused
    // left the flag `true`.
    let _ = cancel.send_replace(true);
}

/// The reconcile decision for a single claim: keep it, or reclaim it (carrying a
/// reason for the log). The pure decision core of [`reconcile_claims`], split out
/// from the GitHub I/O so the phase-split, busy-gate, and grace logic are
/// exhaustively unit-testable without a live burst.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimVerdict {
    Keep,
    Reclaim(&'static str),
}

/// Parked-claim decision from the authoritative job fetch. A job that is gone
/// (`None` = 404) or `completed` will never want a runner again ⇒ reclaim;
/// anything still active ⇒ keep. (A *fetch error* is handled by the caller as
/// keep — a claim is never released unconfirmed.)
fn parked_verdict(job: Option<&Job>) -> ClaimVerdict {
    match job {
        None => ClaimVerdict::Reclaim("job gone (404)"),
        Some(j) if is_terminal(&j.status) => ClaimVerdict::Reclaim("job completed"),
        Some(_) => ClaimVerdict::Keep,
    }
}

/// What the authoritative fetch says about an admitted claim's **own** job.
/// `Unknown` is a *fetch failure*, never an answer: an unreadable job is always
/// kept, so a GitHub blip cannot mass-reclaim (which re-provisions, i.e. storms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobLife {
    /// Still `queued` / `in_progress` / `waiting` — it may yet want a runner.
    Active,
    /// `completed`, or gone (404). It will never want a runner again.
    Terminal,
    /// The fetch failed. Not evidence of anything.
    Unknown,
}

/// [`JobLife`] from an authoritative job fetch that *succeeded* (`None` = 404).
/// Mirrors [`parked_verdict`]'s reading of the same fetch, so the two phases can
/// never disagree about what "this job is done" means.
fn job_life(job: Option<&Job>) -> JobLife {
    match job {
        None => JobLife::Terminal,
        Some(j) if is_terminal(&j.status) => JobLife::Terminal,
        Some(_) => JobLife::Active,
    }
}

/// What GitHub says about the runner faber minted for a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnerLife {
    /// Registered and executing a job — this claim's, or (under not-job-pinned
    /// assignment) a sibling's. Either way it is doing real work.
    Busy,
    /// Registered, online, and neither signal shows it executing anything.
    Idle,
    /// No online registration: never registered, still registering, or already
    /// deregistered — a JIT runner deregisters when it exits. Ambiguous alone.
    Missing,
    /// The runner list couldn't be fetched. Not evidence of anything.
    Unknown,
}

/// [`RunnerLife`] for the runner registered as `name`, from a runner list that was
/// fetched successfully.
///
/// `executing` is the second witness: whether this cycle's scan found a job GitHub
/// still reports live with this runner as its `runner_name` (see
/// [`executing_live_job`]). Either witness alone makes the runner `Busy`. The list's
/// `busy` flag is read once a cycle and has been seen to trail a runner that is
/// minutes into a job; a job record can trail the other way. Neither disproves the
/// other, so a runner reads as idle only when both are clear.
///
/// Matched by exact name, never by the `faber-{job_id}-` prefix. A peer instance
/// minting for the same queued job registers under that same prefix, so a prefix
/// match let a peer's busy runner keep this claim's idle one alive, and a peer's
/// idle one stand in for this claim's runner when it was missing from the list.
fn runner_life(runners: &[Runner], name: &str, executing: bool) -> RunnerLife {
    if executing {
        return RunnerLife::Busy;
    }
    match runners.iter().find(|r| r.name == name) {
        Some(r) if r.busy => RunnerLife::Busy,
        Some(r) if r.status == "online" => RunnerLife::Idle,
        _ => RunnerLife::Missing,
    }
}

/// Whether GitHub has finished the job a frozen claim's runner holds, so the
/// critical floor can thaw it and let it exit (#111, ADR-0032 §2).
///
/// `own` is the claim's own job, as the cycle's fetch read it. `sibling` is the
/// job the scan saw its runner take instead, read by a fetch of its own, when it
/// took one: a JIT runner is not job-pinned, and the job it holds is the one that
/// matters. Finished means that job reads terminal and the runner is not seen
/// working — the two halves of [`admitted_verdict`]'s "job finished" rows. Any
/// unreadable signal is not an answer, and the job stays frozen.
///
/// A wrong answer costs a freeze, not a job: the job runs on, thawed and not
/// frozen again, and the runner's own clearance (ADR-0017) still decides whether
/// it is torn down.
fn frozen_job_over(own: JobLife, sibling: Option<JobLife>, runner: RunnerLife) -> bool {
    sibling.unwrap_or(own) == JobLife::Terminal
        && matches!(runner, RunnerLife::Idle | RunnerLife::Missing)
}

/// Whether this cycle's scan saw the runner registered as `name` executing a job
/// GitHub still reports queued or in progress.
///
/// Read from the same `runner_name` harvest the learner attributes runs by (see
/// [`ExecutedJob`]): a job carries its runner's name from the moment it is
/// dispatched, which is exactly the swap this guards — a runner minted for a job
/// that has since finished elsewhere, busy with a sibling's.
fn executing_live_job(
    executed: &HashMap<String, ExecutedJob>,
    name: &str,
    live: &HashSet<u64>,
) -> bool {
    executed.get(name).is_some_and(|e| live.contains(&e.job_id))
}

/// Whether the claim's runner **container** is running on this host — the one
/// signal in the set that no GitHub outage can distort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerLife {
    /// A running `faber-runner-{job_id}-*` container exists — frozen by the
    /// critical floor included (ADR-0032 §2): a paused runner is a live one.
    Present,
    /// The listing succeeded and this claim owns no running runner container.
    Gone,
    /// The probe failed, or the claim isn't containerized (a macOS VM's runner).
    Unknown,
}

/// Parse the job id from a runner *container* name (`faber-runner-{job_id}-{ts}`
/// — [`crate::docker`]'s `Names::for_runner` of `faber-{job_id}-{ts}`).
fn container_job_id(name: &str) -> Option<u64> {
    name.strip_prefix("faber-runner-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// The job ids whose runner container is running on this host right now, or
/// `None` when the probe itself failed (Docker down, not installed, unresponsive).
///
/// `None` is deliberately *not* "no containers": every caller must read it as
/// "can't tell" and keep the claim. Treating an unreadable Docker as an empty host
/// would reclaim every admitted claim the moment the daemon hiccups — the same
/// storm the GitHub-side signals are guarded against. Bounded by
/// [`crate::docker::CLI_TIMEOUT`], so an unresponsive daemon degrades to `None`
/// rather than stalling the cycle (or the drain).
///
/// A runner the critical floor froze (ADR-0032 §2) is listed too. Docker reports
/// a paused container with the status `paused`, which `status=running` alone
/// leaves out, and read as gone a frozen job would be reclaimed as "no runner
/// container and no registration" and drained past as dead.
async fn live_runner_containers() -> Option<HashSet<u64>> {
    match crate::docker::docker_lines(&live_runner_ps_args(), crate::docker::CLI_TIMEOUT).await {
        Ok(names) => runner_container_job_ids(names),
        Err(e) => {
            debug!("reconcile: probing live runner containers: {e}");
            None
        }
    }
}

/// The job ids in a [`live_runner_ps_args`] listing, or `None` when a name in it
/// is outside Docker's own alphabet (#105). A container left out would read as
/// gone, which reclaims its claim, so a listing faber cannot read whole is "can't
/// tell", which keeps every claim.
fn runner_container_job_ids(names: Vec<String>) -> Option<HashSet<u64>> {
    let names = crate::untrusted::accept_all(names, "reconcile: live runner listing", |n| {
        if crate::untrusted::object_name(&n) {
            Ok(Some(n))
        } else {
            Err("a container name is outside Docker's own alphabet")
        }
    })?;
    Some(names.iter().filter_map(|n| container_job_id(n)).collect())
}

/// The `docker ps` argv behind [`live_runner_containers`]: runner containers,
/// running or frozen ([`crate::docker::LIVE_STATUSES`]), by name.
fn live_runner_ps_args() -> Vec<String> {
    let mut args = vec![
        "ps".to_string(),
        "--filter".to_string(),
        "name=faber-runner-".to_string(),
    ];
    args.extend(crate::docker::live_status_filters());
    args.extend(["--format".to_string(), "{{.Names}}".to_string()]);
    args
}

/// Admitted-claim decision, from the three independent signals plus the grace
/// timer. Returns the verdict and the timer to persist: `Some(first_idle_ms)` to
/// arm or keep the window, `None` to clear it.
///
/// A `Reclaim` here is a *request*, not a kill (ADR-0017). Everything below is
/// GitHub's view of the runner, read once a cycle and acted on later, and that view
/// has been wrong in exactly the direction that matters: runners minutes into a
/// sibling's job read as idle and were killed mid-job. So the provision task that
/// owns the runner kills only once the runner itself confirms it is idle (see
/// [`crate::docker::reclaim_clearance`]), and may refuse. A request therefore
/// restarts the window rather than keeping it: a refused claim is asked again only
/// after another full grace of confirmed idleness, not on every cycle.
///
/// The predicate this replaced asked only whether the claim's *runner* was idle.
/// That is unsound because a JIT runner is not job-pinned (issue #13): the runner
/// minted for job A routinely executes job B, so its `busy` flag says nothing
/// about A. Worse, a runner that had **deregistered** — the normal end of a JIT
/// runner's life — read as "not idle" and so kept its claim forever: a finished
/// job's memory reservation was held until something unrelated released it. Seen
/// live in July 2026, most admitted claims were doing no work (issue #16).
///
/// So the claim is judged on the job it was minted for, with the runner as a veto:
///
/// | job | runner | container | verdict |
/// | --- | --- | --- | --- |
/// | any | `Busy` | any | keep — real work, possibly a stolen sibling's |
/// | `Unknown` | any | any | keep — never act on an unreadable signal |
/// | any | `Unknown` | any | keep — likewise |
/// | `Terminal` | `Idle`/`Missing` | any | request a reclaim past grace |
/// | `Active` | `Idle` | any | request a reclaim past grace — dead supply |
/// | `Active` | `Missing` | `Gone` | request a reclaim past grace — nothing can run |
/// | `Active` | `Missing` | else | keep — it may still be starting up |
///
/// The last two rows are the delicate ones. A runner that has not registered yet
/// is indistinguishable from one that never will, and reclaiming on that alone
/// would kill every runner whose image pull outran the grace — a re-provisioning
/// storm. The container listing settles it locally: no container, nothing is going
/// to register, so the claim is dead. Anything unreadable falls back to keeping.
fn admitted_verdict(
    job: JobLife,
    runner: RunnerLife,
    container: ContainerLife,
    idle_since: Option<u64>,
    now: u64,
    grace_ms: u64,
) -> (ClaimVerdict, Option<u64>) {
    let why = match (job, runner) {
        // Executing something — ours or a sibling's. Never abort real work.
        (_, RunnerLife::Busy) => None,
        // Ignorance is not evidence: keep, and reset the window so a reclaim
        // always rests on `grace_ms` of *confirmed* deadness.
        (JobLife::Unknown, _) | (_, RunnerLife::Unknown) => None,
        // Our job is finished and nothing shows the runner working: if that holds,
        // the claim (and its memory reservation) is pure overhead. Worded as what
        // GitHub shows, because that is all this is — the runner has the last word.
        (JobLife::Terminal, RunnerLife::Idle) => Some("job finished, runner looks idle"),
        (JobLife::Terminal, RunnerLife::Missing) => Some("job finished, runner not registered"),
        // The job still wants a runner but ours has sat idle: dead supply. Standing
        // down lets the next cycle mint a runner that will actually take it.
        (JobLife::Active, RunnerLife::Idle) => Some("runner idle past grace"),
        // No registration — only reclaimable once Docker confirms there is no
        // container either, i.e. nothing is coming.
        (JobLife::Active, RunnerLife::Missing) => match container {
            ContainerLife::Gone => Some("no runner container and no registration"),
            ContainerLife::Present | ContainerLife::Unknown => None,
        },
    };
    let Some(why) = why else {
        return (ClaimVerdict::Keep, None);
    };
    let first = idle_since.unwrap_or(now);
    if now.saturating_sub(first) >= grace_ms {
        // Asked now; if the runner refuses, not asked again for another grace.
        (ClaimVerdict::Reclaim(why), Some(now))
    } else {
        (ClaimVerdict::Keep, Some(first))
    }
}

/// GitHub has handed the job to a runner: anything past `queued` (`in_progress`,
/// or already `completed`). A `waiting` job (environment approval) has not.
fn job_started(job: &Job) -> bool {
    matches!(job.status.as_str(), "in_progress" | "completed")
}

/// `job_started`, and on a runner this host minted. Under demand provisioning
/// several hosts claim the same queued job; one that a sibling's runner took
/// is running there, not here, so it must not read as running on this host.
fn job_started_here(job: &Job, local_runners: &HashSet<String>) -> bool {
    job_started(job)
        && job
            .runner_name
            .as_deref()
            .is_some_and(|r| local_runners.contains(r))
}

/// The step a job is running right now, distilled from its GitHub step list for
/// the dashboard. GitHub advances steps `queued` → `in_progress` → `completed`
/// with normally exactly one `in_progress`, so that's "what it's doing now",
/// reported as its name and 1-based ordinal over the total. `None` when nothing
/// has started yet (all `queued`) or everything is already `completed` — the
/// caller shows a phase word ("Provisioning" / "Wrapping up") instead.
fn current_step(steps: &[Step]) -> Option<StepView> {
    let total = steps.len() as u32;
    steps
        .iter()
        .find(|s| s.status == "in_progress")
        .map(|s| StepView {
            name: s.name.clone(),
            index: u32::try_from(s.number).unwrap_or(0),
            total,
        })
}

/// Reconcile the `inflight` claim set against GitHub each cycle (issue #1),
/// phase-split by whether a claim has admitted (spawned a runner) yet.
///
/// `live` holds every job GitHub still reports as `queued`/`in_progress` across all
/// repos this cycle — the liveness signal a *parked* claim is checked against. It
/// must not be confused with the caller's `seen` set, which also carries the
/// *finished* jobs of still-active runs (it exists to keep their dedup entries from
/// being pruned). Checking claims against `seen` made a finished job's claim
/// immortal for as long as any sibling in its run was still going: a wide fan-out
/// run with a single job left kept several dead claims alive, each re-minting
/// runners for a job that no longer existed.
///
/// * **Parked** (no runner yet) and not in `live`: fetch the job. `completed`/404 ⇒
///   gone ⇒ reclaim. Still active (a later-page job the single-page scan missed) or
///   a transient error ⇒ keep — a claim is never released unconfirmed, which would
///   double-provision a job faber is legitimately still serving.
/// * **Admitted** (a runner was minted): judged on its own job first, with the
///   runner as a veto and the local container listing as a tie-breaker — see
///   [`admitted_verdict`] for the full table. Reclaim needs [`IDLE_RECLAIM_GRACE`]
///   of *confirmed* deadness; a busy runner, or any signal that couldn't be read,
///   keeps the claim. The runner is busy if GitHub's list says so **or** this
///   cycle's scan found it running a live job, and even then a reclaim is only a
///   request the runner itself can refuse (ADR-0017).
///
/// Only ever *signals* via each claim's cancel token; the owning task does the
/// teardown and the release, so claim ownership stays with exactly one task.
async fn reconcile_claims(orch: &Arc<Orchestrator>, live: &HashSet<u64>) {
    struct Snap {
        job_id: u64,
        repo: String,
        admitted: bool,
        containerized: bool,
        runner_name: Option<String>,
        cancel: watch::Sender<bool>,
        step: Arc<std::sync::Mutex<Option<StepView>>>,
        started: Arc<AtomicBool>,
        frozen: bool,
        job_over: Arc<AtomicBool>,
        footprint: Arc<std::sync::Mutex<Timeline>>,
    }
    // Snapshot so no inflight lock is held across an await below. The `step` cell
    // is a shared handle (Arc), so writing the freshly-polled step through it
    // updates the live claim without re-locking the map.
    //
    // A claim faber holds no mint record for is left out (#105): its repo and its
    // runner's name are only what a container reported, so GitHub is asked nothing
    // about them, and the reclaim this would end in is one its runner would refuse.
    // It keeps its claim until its container exits, which is the Docker side's to
    // see.
    let claims: Vec<Snap> = {
        let map = orch.inflight.lock().await;
        map.iter()
            .filter(|(_, c)| c.own_mint)
            .map(|(id, c)| Snap {
                job_id: *id,
                repo: c.repo.clone(),
                admitted: c.admitted.load(Ordering::SeqCst),
                containerized: c.containerized,
                runner_name: c.runner_name.lock().unwrap().clone(),
                cancel: c.cancel.clone(),
                step: c.step.clone(),
                started: c.started.clone(),
                frozen: c.frozen.lock().unwrap().is_some(),
                job_over: c.job_over.clone(),
                footprint: c.footprint.clone(),
            })
            .collect()
    };
    if claims.is_empty() {
        return;
    }
    // Every runner name this host has minted for a live claim: a job counts as
    // started here only when GitHub put it on one of these.
    let local_runners: HashSet<String> = claims
        .iter()
        .filter_map(|c| c.runner_name.clone())
        .collect();

    let now = now_ms();
    // One runners list per repo, fetched lazily the first time an admitted claim in
    // it needs one, then reused for the rest of this cycle. `None` is a *failed*
    // fetch (an entry exists either way), which reads as `RunnerLife::Unknown`.
    let mut runners_by_repo: HashMap<String, Option<Vec<Runner>>> = HashMap::new();
    // The host's running runner containers, probed at most once per cycle and only
    // when some claim's verdict actually turns on it.
    let mut containers: Option<Option<HashSet<u64>>> = None;

    let grace_ms = IDLE_RECLAIM_GRACE.as_millis() as u64;
    for c in claims {
        if !c.admitted {
            // Parked. A claim GitHub still reports queued/in-progress is live — leave
            // it be. Anything else gets the authoritative fetch below.
            if live.contains(&c.job_id) {
                continue;
            }
            // Authoritative fetch; a transport error is kept (never released
            // unconfirmed), decoupled from the pure verdict.
            let verdict = match orch.gh.job(&c.repo, c.job_id).await {
                Ok(job) => parked_verdict(job.as_ref()),
                Err(e) => {
                    warn!("reconcile: fetch job {} in {}: {e}", c.job_id, c.repo);
                    ClaimVerdict::Keep
                }
            };
            if let ClaimVerdict::Reclaim(why) = verdict {
                reclaim(c.job_id, &c.cancel, why);
            }
            continue;
        }

        // Admitted. The same authoritative fetch serves two purposes: it refreshes
        // the dashboard's live step, and it is the primary reclaim signal — the
        // claim is reconciled against *its own job*, not against whichever job
        // GitHub happened to hand its runner. A transport error leaves the
        // last-known step rather than blanking it, and reads as `Unknown`.
        let job = match orch.gh.job(&c.repo, c.job_id).await {
            Ok(Some(job)) => {
                *c.step.lock().unwrap() = current_step(job.steps.as_deref().unwrap_or_default());
                if job_started_here(&job, &local_runners) {
                    c.started.store(true, Ordering::SeqCst);
                }
                // ADR-0056 §3: the poll that sees the job completed is the read
                // its steps are recorded from, at no extra request.
                let times = JobTimes::of(&job);
                if times.is_complete() {
                    c.footprint.lock().unwrap().job = Some(times);
                }
                job_life(Some(&job))
            }
            Ok(None) => {
                *c.step.lock().unwrap() = None;
                job_life(None)
            }
            Err(e) => {
                warn!("reconcile: fetch job {} in {}: {e}", c.job_id, c.repo);
                JobLife::Unknown
            }
        };

        // The runner is the veto: busy means real work, whoever's job it is. The
        // entry's *presence* records that this repo was attempted, so a failure is
        // memoized too — one bad response isn't re-requested per claim, and every
        // claim in the repo decides on the same view.
        if !runners_by_repo.contains_key(&c.repo) {
            let fetched = match orch.gh.list_runners(&c.repo).await {
                Ok(rs) => Some(rs),
                Err(e) => {
                    warn!("reconcile: list runners for {}: {e}", c.repo);
                    None
                }
            };
            runners_by_repo.insert(c.repo.clone(), fetched);
        }
        // An admitted claim always has its runner's name (published just before its
        // `admitted` flag flips); without one there is no runner to judge, so
        // nothing is known.
        let runner = match c.runner_name.as_deref() {
            Some(name) => {
                let executing = executing_live_job(&*orch.executed.lock().await, name, live);
                match runners_by_repo[&c.repo].as_deref() {
                    Some(rs) => runner_life(rs, name, executing),
                    None if executing => RunnerLife::Busy,
                    None => RunnerLife::Unknown,
                }
            }
            None => RunnerLife::Unknown,
        };
        // A busy runner is a running job, whoever's it is: GitHub hands a JIT
        // runner any matching queued job, so the claim's own job may still be
        // queued, or have run on a peer, while its runner works on a sibling. The
        // own-job check above alone left such a claim `starting` for good. The
        // dashboard's `step` stays the own job's: the sibling's steps would cost
        // a fetch this pass does not otherwise make.
        if runner == RunnerLife::Busy {
            c.started.store(true, Ordering::SeqCst);
        }

        // A frozen claim (ADR-0032 §2) is let go once GitHub has finished the job its
        // runner holds (#111): until then a reclaim of it is refused (ADR-0017), and
        // its disk stays taken. Its runner may hold a sibling's job rather than its
        // own, and only that one costs a fetch of its own — at most
        // `freeze::MAX_FROZEN` a cycle, and none once the claim is marked.
        if c.frozen && !c.job_over.load(Ordering::SeqCst) {
            let sibling = match c.runner_name.as_deref() {
                Some(name) => orch
                    .executed
                    .lock()
                    .await
                    .get(name)
                    .map(|e| e.job_id)
                    .filter(|&id| id != c.job_id),
                None => None,
            };
            let sibling = match sibling {
                Some(id) => Some(match orch.gh.job(&c.repo, id).await {
                    Ok(found) => job_life(found.as_ref()),
                    Err(e) => {
                        warn!("reconcile: fetch job {id} in {}: {e}", c.repo);
                        JobLife::Unknown
                    }
                }),
                None => None,
            };
            if frozen_job_over(job, sibling, runner) {
                info!(
                    "reconcile: the frozen runner of job {} in {} holds a job GitHub has \
                     finished; the critical floor thaws it so it exits",
                    c.job_id, c.repo
                );
                c.job_over.store(true, Ordering::SeqCst);
            }
        }

        // Only an active job with no registration turns on the container listing,
        // so the probe is skipped entirely on a healthy cycle.
        let container =
            if c.containerized && (job, runner) == (JobLife::Active, RunnerLife::Missing) {
                if containers.is_none() {
                    containers = Some(live_runner_containers().await);
                }
                match containers.as_ref().expect("probed just above") {
                    Some(ids) if ids.contains(&c.job_id) => ContainerLife::Present,
                    Some(_) => ContainerLife::Gone,
                    None => ContainerLife::Unknown,
                }
            } else {
                ContainerLife::Unknown
            };

        let prev = orch.idle_since.lock().await.get(&c.job_id).copied();
        let (verdict, timer) = admitted_verdict(job, runner, container, prev, now, grace_ms);
        {
            let mut idle_since = orch.idle_since.lock().await;
            match timer {
                Some(t) => {
                    idle_since.insert(c.job_id, t);
                }
                None => {
                    idle_since.remove(&c.job_id);
                }
            }
        }
        if let ClaimVerdict::Reclaim(why) = verdict {
            reclaim(c.job_id, &c.cancel, why);
        }
    }
}

/// Keep the Docker daemon itself usable — ADR-0007, as amended by ADR-0024 §2.
///
/// Gate 0 already refuses admission while Docker is down, which is the right
/// defensive move and was *all* faber did: in the incident behind ADR-0007 it
/// logged `daemon unresponsive` every three minutes for about half a day while CI
/// was blocked, waiting for a human to restart Docker Desktop by hand. faber is
/// the Docker manager on this host, so bringing it back is faber's job.
///
/// Not at any cost, though. With the admission floor set, a restart waits for
/// room on the Docker volume: restarting onto a full disk cannot make room, and
/// in the incident behind ADR-0024 it killed the jobs in flight and left the
/// VM's filesystem reporting data loss. And a restart is forgiven only once
/// Docker has then stayed up for a whole cooldown, so a daemon that answers for a
/// minute and wedges again is restarted at most once per cooldown, and left to a
/// human after [`host::REVIVE_GIVE_UP_AFTER`]. The policy is
/// [`host::DockerCustody`]'s; this is the I/O around it.
///
/// Driven from the reconcile cycle rather than the limiter's probe loop because
/// the audit trail lives here — an outage and the recovery that ended it are
/// recorded as the same fault pair a GitHub outage produces, and
/// [`Orchestrator::observe_fault`] already collapses a hundred cycles of it into
/// two lines. A reason that changes mid-outage (waiting for room, giving up) is
/// the one thing stated again in between.
async fn reconcile_host(orch: &Arc<Orchestrator>) {
    let now = now_ms();
    let Some(outage) = observe_host(orch, now).await else {
        return;
    };
    if outage.revival != host::Revival::Restart {
        return;
    }
    let restart = orch.docker_custody.lock().await.restarting(now);
    // Not declared recovered on `Ok` — the limiter's probe is the authority, and
    // the next healthy cycle records the edge. Nor forgiven there: only a stable
    // cooldown after this restart resets the count (ADR-0024 §2).
    if let Err(e) = host::revive_docker().await {
        warn!(
            "host: restarting Docker failed (restart {restart} of at most {} without a stable \
             period): {e}",
            host::REVIVE_GIVE_UP_AFTER
        );
    }
}

/// [`reconcile_host`]'s bookkeeping, short of acting on it: fold this cycle's
/// liveness into the custody record and keep the Docker fault saying why. The
/// outage, when Docker is not answering, for the caller to act on.
///
/// Split out so the decision and what it tells the operator are testable without
/// a test ever being one wrong verdict away from restarting a real Docker.
///
/// While faber's Linux VM is not in service (ADR-0026) the engine driver owns the
/// outage: the fault is the driver's reason, and custody waits for the VM to serve
/// before its clock runs ([`host::DockerCustody::engine_down`]).
async fn observe_host(orch: &Orchestrator, now: u64) -> Option<host::Outage> {
    // ADR-0061 §3: a grow restart is faber's own. Revival holds off and the
    // Docker fault stays quiet until the driver reports the VM in service or the
    // start fails; admission is still refused meanwhile (Gate 0 reads the engine).
    if crate::vm::grow::global().restarting() {
        return None;
    }
    if let Some(engine) = orch.limiter.engine_fault() {
        let line = format!("Docker unreachable — {engine}. Admission refused until it answers");
        let said = orch.docker_custody.lock().await.engine_down(now, &line);
        // Opens the fault on the outage's first cycle, and is a no-op after it.
        orch.observe_fault("host", "docker", Some(line.clone()))
            .await;
        if said.restated {
            orch.record_fault("host", "docker", "failed", Some(line.clone()))
                .await;
        }
        if said.opened || said.restated {
            warn!("host: {line}");
        }
        return None;
    }
    if orch.limiter.docker_reachable() {
        orch.docker_custody
            .lock()
            .await
            .answered(&orch.cfg.host, now);
        // Every cycle Docker answers, not only when custody saw an outage end:
        // custody starts empty after a restart, so a replayed open fault would
        // never close. `observe_fault` is transition-gated, so this stays quiet.
        orch.observe_fault("host", "docker", None).await;
        return None;
    }

    let room = orch.limiter.disk_short_of_floor(now);
    let outage = orch
        .docker_custody
        .lock()
        .await
        .unanswered(&orch.cfg.host, now, room);
    let line = outage.fault(&orch.cfg.host);
    // Opens the fault on the outage's first cycle, and is a no-op after it.
    orch.observe_fault("host", "docker", Some(line.clone()))
        .await;
    if outage.restated {
        orch.record_fault("host", "docker", "failed", Some(line.clone()))
            .await;
    }
    // Waiting for room, giving up, and a Docker that is not faber's to restart
    // (ADR-0025) are decisions an operator needs in the log as well as on the
    // dashboard: said each time they become the reason, not every cycle they
    // stay it.
    let withheld = matches!(
        outage.revival,
        host::Revival::NoRoom(_) | host::Revival::GaveUp { .. } | host::Revival::NotOurs(_)
    );
    if withheld && (outage.opened || outage.restated) {
        warn!("host: {line}");
    }
    Some(outage)
}

/// Check once a cycle that each Docker profile's own image is on this host (#63).
///
/// One `image inspect` per distinct profile image. An absent image is pulled in
/// the background — what `docker run` would have done — and only one still
/// absent after a failed pull is confirmed missing: then every claim for that
/// profile parks (see [`Orchestrator::image_block`]), the fault is recorded once,
/// and the state reads unhealthy until the image is back. An absent image whose
/// reference names no registry host is not pulled at all under `[host]
/// pull_unqualified_images = false`, and is missing at once (#156). Nothing here
/// builds or deletes an image: the profile image is the operator's (AGENTS.md).
///
/// Skipped while Docker is not answering: the Docker fault already covers that,
/// and an unanswered inspect proves nothing about the image.
async fn reconcile_profile_images(orch: &Arc<Orchestrator>) {
    if !orch.limiter.docker_reachable() {
        return;
    }
    for profile in &orch.cfg.profiles {
        let Backend::Docker(d) = &profile.backend else {
            continue;
        };
        let (name, image) = (profile.name.clone(), d.image.clone());
        match crate::docker::image_presence(&image).await {
            ImagePresence::Present => {
                let was = orch.profile_images.lock().unwrap().present(&name, &image);
                if was.is_some() {
                    info!("profile {name}: image {image} is back — minting resumes");
                }
                // Unconditional: `profile_images` starts empty after a restart,
                // so `was` is None for a replayed open fault. Transition-gated.
                orch.observe_fault("host", &profile_image_scope(&name), None)
                    .await;
            }
            // #156: an image faber may not pull is missing the moment it is
            // absent. Nothing else would bring it back: runners start with
            // `--pull=never`.
            ImagePresence::Absent
                if !crate::profile_image::pull_allowed(
                    &image,
                    orch.cfg.host.pull_unqualified_images,
                ) =>
            {
                let cause = "its reference names no registry host and \
                             host.pull_unqualified_images = false, so faber does not pull it"
                    .to_string();
                let new = orch.profile_images.lock().unwrap().unpullable(
                    &name,
                    &image,
                    cause.clone(),
                    now_ms(),
                );
                if new {
                    let line = format!(
                        "profile image missing: {name} ({image}) is not on this host and {cause} \
                         — no runner is registered for it until it is back"
                    );
                    warn!("{line}");
                    orch.observe_fault("host", &profile_image_scope(&name), Some(line))
                        .await;
                }
            }
            ImagePresence::Absent => {
                let pull = orch
                    .profile_images
                    .lock()
                    .unwrap()
                    .absent(&name, &image, now_ms());
                if pull {
                    info!("profile {name}: image {image} is not on this host — pulling it");
                    tokio::spawn(pull_profile_image(orch.clone(), name, image));
                }
            }
            ImagePresence::Unknown(why) => {
                orch.profile_images.lock().unwrap().unknown(&name, &image);
                debug!("profile {name}: could not tell whether {image} is present: {why}");
            }
        }
    }
}

/// The fault scope a missing profile image is recorded under, one per profile.
fn profile_image_scope(profile: &str) -> String {
    format!("profile_image:{profile}")
}

/// Pull a profile image that is absent, and settle what that proved: back, still
/// missing, or nothing learnt. Detached from the cycle, since a real pull can
/// take minutes.
async fn pull_profile_image(orch: Arc<Orchestrator>, profile: String, image: String) {
    let pulled = crate::docker::pull_image(&image).await;
    let after = crate::docker::image_presence(&image).await;
    let still_missing = match (&pulled, after) {
        (_, ImagePresence::Present) => None,
        (Err(cause), ImagePresence::Absent) => Some(cause.clone()),
        // A pull that reported success but left nothing, or a daemon that stopped
        // answering: no verdict either way, and the next cycle looks again.
        _ => None,
    };
    let confirmed = orch.profile_images.lock().unwrap().pulled(
        &profile,
        &image,
        still_missing.clone(),
        now_ms(),
    );
    match (confirmed, still_missing) {
        (true, Some(cause)) => {
            let line = format!(
                "profile image missing: {profile} ({image}) is not on this host and could not \
                 be pulled — no runner is registered for it until it is back: {cause}"
            );
            warn!("{line}");
            orch.observe_fault("host", &profile_image_scope(&profile), Some(line))
                .await;
        }
        (false, None) if pulled.is_ok() => {
            info!("profile {profile}: pulled {image}");
        }
        _ => {}
    }
}

/// The credential's part of a cycle (ADR-0027): take a token `sudo faber auth
/// set` stored, confirm a new one with GitHub, try a rejected one again now and
/// then, and warn of an expiry within days, once a day.
///
/// A new token is confirmed with `GET /user`, which is what ends a rejection:
/// the client clears it on the first answer GitHub accepts.
async fn reconcile_credential(orch: &Arc<Orchestrator>) {
    let now = now_ms();
    // A token taken earlier (by a `401`'s re-read, or a check that failed to
    // reach GitHub) is confirmed too: until it is, a standing ActsAsOther
    // block stays.
    let retaken = orch.gh.reload_if_changed().await;
    // A token still unconfirmed blocks claims, so it is checked again each
    // cycle while GitHub has not answered; once GitHub rejects it, it joins the
    // rejected token's throttle, so a bad credential is never re-sent faster
    // than REJECTED_RETRY_MS. That holds under a standing ActsAsOther block
    // too: `is_rejected` is GitHub's last answer to the token, not the reason
    // claims are blocked.
    let rejected = orch.gh.is_rejected();
    let recheck_unconfirmed = !retaken && !rejected && orch.gh.unconfirmed();
    let retry_rejected = !retaken && rejected && {
        let mut checks = orch.credential_checks.lock().unwrap();
        let due = now.saturating_sub(checks.rejected_retry_ms) >= REJECTED_RETRY_MS;
        if due {
            checks.rejected_retry_ms = now;
        }
        due
    };
    let retaken_or_unconfirmed = retaken || recheck_unconfirmed;
    if retaken_or_unconfirmed || retry_rejected {
        // Any check starts the rejected-token throttle from now, so a token
        // GitHub rejects on this answer is not sent again before it is due.
        orch.credential_checks.lock().unwrap().rejected_retry_ms = now;
        match orch.gh.current_login().await {
            Ok(login) if login.eq_ignore_ascii_case(&orch.owner) => {
                if orch.gh.owner_confirmed() {
                    info!(
                        "GitHub token: GitHub says it acts as {login}, the owner this daemon \
                         serves; claiming resumes"
                    );
                }
                if retaken {
                    // What the new token may do is learned afresh.
                    orch.credential_checks.lock().unwrap().probed_ms = None;
                }
            }
            // The store files the token under the served owner, yet GitHub
            // says it acts as someone else: claiming on it would run another
            // account's jobs here. Block claims until a right token is stored.
            Ok(login) => {
                if orch.gh.acts_as_other(&login, &orch.owner) {
                    warn!(
                        "GitHub token: GitHub says it acts as {login}, but this daemon serves \
                         {}; nothing new is claimed until `sudo faber auth set` stores a token \
                         for {}",
                        orch.owner, orch.owner
                    );
                }
            }
            Err(e) => debug!("checking the GitHub token: {e}"),
        }
    }
    if let Some(warning) = orch.gh.daily_expiry_warning() {
        warn!("{warning}");
    }
}

/// Probe the token's read permissions on each served repo, and compare the
/// repos it reaches with the opted-in ones, about hourly (ADR-0027). A refusal
/// is recorded by the client, which turns off what needs it for that repo and
/// warns once. Bounded: at most [`PROBE_REPOS`] repos, a handful of
/// ETag-cached reads each, and a few pages of the token's repo list.
async fn reconcile_permissions(orch: &Arc<Orchestrator>, repos: &[String]) {
    let now = now_ms();
    {
        let mut checks = orch.credential_checks.lock().unwrap();
        if checks
            .probed_ms
            .is_some_and(|at| now.saturating_sub(at) < PERMISSION_PROBE_MS)
        {
            return;
        }
        checks.probed_ms = Some(now);
    }
    let probed = crate::credential::probed(&orch.gh.needs());
    for repo in repos.iter().take(PROBE_REPOS) {
        for permission in &probed {
            if let Err(e) = orch.gh.probe(repo, *permission).await {
                debug!("permission probe: {} on {repo}: {e}", permission.label());
            }
        }
    }
    if probed.is_empty() {
        // A client that was told nothing about the features in use (a test,
        // `run` without a store) has nothing to compare either.
        return;
    }
    let granted = match orch.gh.granted_repos().await {
        Ok(granted) => granted,
        Err(e) => {
            debug!("listing the GitHub token's repositories: {e}");
            return;
        }
    };
    let granted: Vec<String> = granted.into_iter().map(|r| r.full_name).collect();
    let found = crate::credential::mismatches(&orch.owner, &granted, repos, |r| {
        orch.gh.blocked(r, Permission::Administration)
    });
    {
        let mut checks = orch.credential_checks.lock().unwrap();
        for repo in &found.granted_not_opted_in {
            if !checks.logged.granted_not_opted_in.contains(repo) {
                warn!(
                    "the GitHub token reaches {repo}, which does not carry the `{}` topic: \
                     remove it from the token on GitHub. faber cannot read a token's grant, so \
                     this is the only way an over-broad one shows",
                    orch.cfg.topic
                );
            }
        }
        for repo in &found.opted_in_not_granted {
            if !checks.logged.opted_in_not_granted.contains(repo) {
                warn!(
                    "{repo} carries the `{}` topic, but the GitHub token cannot administer it, \
                     so none of its jobs run here: add it to the token's repositories on GitHub",
                    orch.cfg.topic
                );
            }
        }
        checks.logged = found.clone();
    }
    orch.gh.set_mismatches(found);
}

/// Auto-retry sweep (ADR-0003): re-run jobs that failed because faber's *own*
/// runner was ended under them (OOM-killed at faber's cap, killed at its lifetime
/// cap, reclaimed, frozen for good, or lost with the daemon or VM under it).
///
/// Once per cycle, per repo: scan recently-failed runs; for each whose
/// `run_attempt` is under the cap and whose last update is inside the lookback
/// window, take the jobs run by a `faber-*` runner and ask [`EndLedger::verdict`]
/// whether faber's own audit record shows that runner ended abnormally while the
/// job was running (ADR-0003, amendment 2026-10-18). Those are re-run — the whole
/// run when *every* failure is faber-caused (one `rerun-failed-jobs` call), else
/// only the faber jobs (leaving genuine step-failures untouched). Each re-run is
/// audited with its evidence. Gated behind `[retry] enabled`; every error is
/// logged, never fatal to the cycle.
///
/// The jobs API (Actions: read) names each job's runner; nothing here reads a
/// check-run annotation, so it needs no Checks: read. A missing record is not
/// evidence of a fault: the runner may be a fleet peer's, or faber may have
/// restarted before writing one. Such a job is simply not retried.
async fn reconcile_retries(orch: &Arc<Orchestrator>, repos: &[String]) {
    let policy = &orch.cfg.retry;
    if !policy.enabled {
        return;
    }
    let cap = policy.attempt_cap();
    let lookback_ms = policy.lookback().as_millis() as u64;
    let now = now_ms();
    for repo in repos {
        // Off for a repo whose token cannot re-run (ADR-0027). The client warned
        // once, naming the permission and the fix.
        if orch.gh.blocked(repo, Permission::ActionsWrite) {
            continue;
        }
        let runs = match orch.gh.runs_with_status(repo, "failure").await {
            Ok(runs) => runs,
            Err(e) => {
                warn!("retry: list failed runs for {repo}: {e}");
                continue;
            }
        };
        for run in runs {
            // Cheap, I/O-free gates first: attempt cap, then the lookback window (an
            // unparsable/absent stamp reads as "outside" — the safe, no-retry side).
            if !retry::attempt_ok(run.run_attempt, cap) {
                continue;
            }
            let fresh = run
                .updated_at
                .as_deref()
                .and_then(retry::parse_rfc3339_to_ms)
                .is_some_and(|updated| retry::within_lookback(updated, now, lookback_ms));
            if !fresh {
                continue;
            }
            let key = (run.id, run.run_attempt);
            if orch.retried.lock().await.contains(&key) {
                continue;
            }

            // Identify the faber-caused failures among the run's failed jobs.
            let jobs = match orch.gh.run_jobs(repo, run.id).await {
                Ok(jobs) => jobs,
                Err(e) => {
                    warn!("retry: fetch jobs for run {} in {repo}: {e}", run.id);
                    continue;
                }
            };
            let failed: Vec<&Job> = jobs
                .iter()
                .filter(|j| retry::is_failed_conclusion(j.conclusion.as_deref()))
                .collect();
            if failed.is_empty() {
                continue;
            }
            let mut faber_dead = Vec::new();
            // Each faber-caused job's evidence, for the audit line of its retry.
            let mut evidence: HashMap<u64, String> = HashMap::new();
            // Set when at least one job's evidence is *incomplete* rather than
            // exonerating. It keeps the run out of the "terminally genuine" memory,
            // so it is examined again next cycle instead of being written off on
            // evidence that hadn't finished arriving.
            let mut incomplete = false;
            let claimed = orch.inflight_job_ids().await;
            for j in &failed {
                if !retry::is_faber_runner(j.runner_name.as_deref()) {
                    continue;
                }
                // A gate bypass faber killed under `enforce` is never re-run
                // (ADR-0030): GitHub reports that kill exactly as it reports a
                // runner faber lost, and re-running it would hand the refused job a
                // second go. A bypass that was only recorded is left to the sweep.
                if orch.was_force_killed(j.id).await {
                    continue;
                }
                // A job whose runner faber is *still running* has no local verdict
                // yet — see [`runner_still_claimed`]. Defer rather than conclude:
                // the dedup memory is permanent for this attempt, so a sweep landing
                // in the teardown gap is the one way a timing accident becomes a
                // permanent verdict.
                if runner_still_claimed(j.runner_name.as_deref(), &claimed) {
                    incomplete = true;
                    continue;
                }
                let verdict = orch.ends.lock().await.verdict(
                    j.runner_name.as_deref(),
                    j.id,
                    j.started_at.as_deref(),
                    j.completed_at.as_deref(),
                );
                let runner = j.runner_name.as_deref().unwrap_or("?");
                match verdict {
                    RetryVerdict::FaberCaused { kind, at_ms } => {
                        let why = format!(
                            "faber's audit record: runner {runner} ended `{}` at {at_ms} ms, \
                             inside job {} (started {}, completed {})",
                            kind.word(),
                            j.id,
                            j.started_at.as_deref().unwrap_or("?"),
                            j.completed_at.as_deref().unwrap_or("?"),
                        );
                        info!("retry: job {} in {repo} is faber-caused — {why}", j.id);
                        evidence.insert(j.id, why);
                        faber_dead.push(j.id);
                    }
                    // Not faber's fault, or not provably so: never re-run.
                    RetryVerdict::NotFaber(why) => debug!(
                        "retry: job {} in {repo} on {runner} is not retried: {why:?}",
                        j.id
                    ),
                }
            }
            let failed_ids: Vec<u64> = failed.iter().map(|j| j.id).collect();
            match retry::retry_mode(&failed_ids, &faber_dead) {
                // Nothing faber-caused. A completed run's jobs are terminal, so this
                // run is failed for good — remember it (cheaply) so it isn't
                // re-examined every cycle for the whole lookback window. A run whose
                // evidence was incomplete — a runner faber is still running and has
                // not yet reported on — gets a fresh look next cycle instead.
                None => {
                    if !incomplete {
                        orch.retried.lock().await.record(key);
                    }
                }
                // Re-run; mark the run actioned only when accepted (or GitHub says it
                // is already re-running). A transient error leaves it for next cycle.
                Some(mode) => {
                    if issue_retry(orch, repo, &run, &faber_dead, &evidence, &mode).await {
                        orch.retried.lock().await.record(key);
                    }
                }
            }
        }
    }
}

/// Sample finished jobs for GitHub's runner-deprecation annotation (#64), and
/// repeat each standing warning once a day.
///
/// One job per profile per [`runner_retirement::SAMPLE_EVERY`]. (The retry sweep
/// used to read failed jobs' annotations and feed this for free; it no longer
/// reads any, so the sample is the only source — ADR-0003, amendment 2026-10-18.)
/// Each sample is two calls: the job, to confirm this host's runner is the one that ran it
/// (GitHub hands a runner whichever queued job matches its labels, not the one it
/// was minted for), then its annotations. Every failure is logged at debug and
/// waits for the next sample; nothing here can stop a job.
async fn reconcile_runner_retirement(orch: &Arc<Orchestrator>) {
    let due = orch.retirements.lock().unwrap().due(now_ms());
    for (profile, c) in due {
        // Without Checks: read the annotation cannot be read here, so the
        // warning is off for this repo; the client warned once (ADR-0027).
        if orch.gh.blocked(&c.repo, Permission::ChecksRead) {
            continue;
        }
        let job = match orch.gh.job(&c.repo, c.job_id).await {
            Ok(Some(job)) => job,
            Ok(None) => continue,
            Err(e) => {
                debug!("runner retirement: job {} in {}: {e}", c.job_id, c.repo);
                continue;
            }
        };
        if job.runner_name.as_deref() != Some(c.runner.as_str()) {
            continue;
        }
        let Some(url) = job.check_run_url.as_deref() else {
            continue;
        };
        let annotations = match orch.gh.job_annotations(&c.repo, url).await {
            Ok(Some(anns)) => anns,
            Ok(None) => continue,
            Err(e) => {
                debug!(
                    "runner retirement: annotations for job {} in {}: {e}",
                    c.job_id, c.repo
                );
                continue;
            }
        };
        let messages: Vec<&str> = annotations.iter().map(String::as_str).collect();
        let ran = job.steps.as_ref().is_some_and(|s| !s.is_empty());
        orch.observe_retirement(&profile, job.id, ran, &messages);
    }
    orch.log_retirements(now_ms());
}

/// What one try at a run's verdict came to.
enum VerdictTry {
    /// Settled: a verdict to write, or `None` when there is none to claim.
    Settled(PendingVerdict, Option<JobVerdict>),
    /// Asked and not settled, its try spent: back in the queue while tries remain.
    Again(PendingVerdict),
    /// GitHub gave no response at all. Nothing was learned and no try is spent.
    Unreached(PendingVerdict),
}

/// One try at `p`'s verdict: a single job GET, or none while the repo's
/// `Actions: read` is blocked. Shared by the batch and the early fetch, so both
/// spend the same per-run budget.
async fn try_verdict(orch: &Orchestrator, mut p: PendingVerdict) -> VerdictTry {
    // A credential that claims nothing asks GitHub nothing for display either: a
    // run of bad-credential requests gets the whole account refused for a while
    // (see `reconcile_once`). The run waits, untried, for a usable credential.
    if orch.gh.claim_block().is_some() {
        return VerdictTry::Unreached(p);
    }
    let read = if orch.gh.blocked(&p.repo, Permission::ActionsRead) {
        VerdictRead::Pending
    } else {
        match orch.gh.job(&p.repo, p.job_id).await {
            Ok(job) => read_verdict(job.as_ref(), &p.runner),
            Err(GithubError::Http(e)) => {
                debug!("verdict: GitHub not reached for job {}: {e}", p.job_id);
                return VerdictTry::Unreached(p);
            }
            Err(e) => {
                debug!("verdict: job {} in {}: {e}", p.job_id, p.repo);
                VerdictRead::Pending
            }
        }
    };
    p.tries += 1;
    match read {
        VerdictRead::Settled(v) => VerdictTry::Settled(p, v),
        VerdictRead::Pending if p.tries < VERDICT_TRIES => VerdictTry::Again(p),
        VerdictRead::Pending => {
            debug!(
                "verdict: job {} in {} gave none in {VERDICT_TRIES} tries",
                p.job_id, p.repo
            );
            VerdictTry::Settled(p, None)
        }
    }
}

/// Fetch one finished run's verdict once, [`Orchestrator::verdict_early_ms`]
/// after it ended, on its own task (ADR-0044, amended 2026-10-19). It takes the
/// run out of the queue first, so it and a batch never both ask about it; a run
/// a batch already took is left to the batch. The try counts against the run's
/// budget like any other.
fn spawn_early_verdict(orch: &Arc<Orchestrator>, p: &PendingVerdict) {
    let delay = orch.verdict_early_ms.load(Ordering::Relaxed);
    if delay == 0 {
        return;
    }
    let orch = Arc::clone(orch);
    let (job_id, ts_ms, runner) = (p.job_id, p.ts_ms, p.runner.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let taken = {
            let mut m = orch.metrics.lock().await;
            m.pending_verdicts
                .iter()
                .position(|q| q.job_id == job_id && q.ts_ms == ts_ms && q.runner == runner)
                .and_then(|i| m.pending_verdicts.remove(i))
        };
        let Some(p) = taken else {
            return;
        };
        let tried = try_verdict(&orch, p).await;
        let mut m = orch.metrics.lock().await;
        match tried {
            VerdictTry::Settled(p, Some(v)) => m.apply_verdict(&p, v),
            VerdictTry::Settled(_, None) => {}
            // Back in its place by age, for the cycle's batch.
            VerdictTry::Again(p) | VerdictTry::Unreached(p) => {
                let at = m
                    .pending_verdicts
                    .iter()
                    .take_while(|q| q.ts_ms <= p.ts_ms)
                    .count();
                m.pending_verdicts.insert(at, p);
            }
        }
    });
}

/// Start a [`reconcile_verdicts`] batch on its own task, unless one is still
/// running. Returns its handle, which the loop leaves detached and `--once` awaits.
fn spawn_verdicts(orch: &Arc<Orchestrator>) -> Option<JoinHandle<()>> {
    if orch.verdicts_running.swap(true, Ordering::AcqRel) {
        return None;
    }
    let orch = Arc::clone(orch);
    Some(tokio::spawn(async move {
        // Cleared on drop, so a batch that panics cannot stop every later one.
        let _running = ClearOnDrop(Arc::clone(&orch.verdicts_running));
        reconcile_verdicts(&orch).await;
    }))
}

/// Clears its flag when dropped, panic or not.
struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Read GitHub's conclusion for the oldest finished runs still waiting on one,
/// at most [`VERDICT_BATCH`] of them, and write each onto the run's
/// [`OutcomeEvent`] in place (ADR-0044). Each run has [`VERDICT_TRIES`] tries in
/// all, the early fetch's included: a job GitHub has not marked completed yet,
/// or a refused or blocked fetch, spends one, and a run out of tries keeps
/// `conclusion` absent. An error is never a verdict. A request that got no
/// response at all (a timeout, a refused connection) ends the batch without
/// spending anyone's try: GitHub was not reached, so nothing was learned.
async fn reconcile_verdicts(orch: &Arc<Orchestrator>) {
    let due: Vec<PendingVerdict> = {
        let mut m = orch.metrics.lock().await;
        let n = m.pending_verdicts.len().min(VERDICT_BATCH);
        m.pending_verdicts.drain(..n).collect()
    };
    if due.is_empty() {
        return;
    }
    let mut settled: Vec<(PendingVerdict, JobVerdict)> = Vec::new();
    let mut again: Vec<PendingVerdict> = Vec::new();
    let mut due = due.into_iter();
    while let Some(p) = due.next() {
        match try_verdict(orch, p).await {
            VerdictTry::Settled(p, Some(v)) => settled.push((p, v)),
            VerdictTry::Settled(_, None) => {}
            VerdictTry::Again(p) => again.push(p),
            VerdictTry::Unreached(p) => {
                again.push(p);
                again.extend(due.by_ref());
            }
        }
    }
    let mut m = orch.metrics.lock().await;
    for (p, v) in settled {
        m.apply_verdict(&p, v);
    }
    // Ahead of anything recorded while the fetches ran, so the oldest go first.
    for p in again.into_iter().rev() {
        m.pending_verdicts.push_front(p);
    }
    while m.pending_verdicts.len() > OUTCOME_RECENT_CAP {
        m.pending_verdicts.pop_front();
    }
}

/// Perform the re-run for one run and record it. Returns `true` when the run
/// should be marked actioned — the re-run was accepted, or GitHub reports it is
/// already re-running — and `false` on a transient error worth another cycle.
async fn issue_retry(
    orch: &Arc<Orchestrator>,
    repo: &str,
    run: &WorkflowRun,
    faber_dead: &[u64],
    evidence: &HashMap<u64, String>,
    mode: &RetryMode,
) -> bool {
    let (result, mode_label, retried_ids): (Result<(), GithubError>, &str, Vec<u64>) = match mode {
        // Every failure is faber-caused → one call recovers the whole run. In this
        // mode `faber_dead` *is* the full failed set, so it is exactly what re-ran.
        RetryMode::AllFailed => (
            orch.gh
                .rerun_failed_jobs(
                    repo,
                    run.id,
                    "auto-retry: every failed job of this run lost its faber runner (ADR-0003)",
                )
                .await,
            "all",
            faber_dead.to_vec(),
        ),
        // Mixed run → re-run only faber jobs, never the genuine ones. The first
        // per-job re-run flips the whole run to in_progress, so only it (and its
        // dependents) actually re-runs this attempt; extras in a mixed run fall back
        // to a manual re-run (rare, and never re-runs a genuine failure).
        RetryMode::Surgical(ids) => {
            let first = ids[0];
            if ids.len() > 1 {
                info!(
                    "retry: run {} in {repo} is mixed with {} faber-deaths — re-running job \
                     {first}; {} deferred (the run locks after the first re-run)",
                    run.id,
                    ids.len(),
                    ids.len() - 1
                );
            }
            (
                orch.gh
                    .rerun_job(
                        repo,
                        first,
                        "auto-retry: this job lost its faber runner (ADR-0003)",
                    )
                    .await,
                "surgical",
                vec![first],
            )
        }
    };
    match result {
        Ok(()) => {
            info!(
                "retry: re-ran {} job(s) of run {} in {repo} (from attempt {}, mode {mode_label})",
                retried_ids.len(),
                run.id,
                run.run_attempt
            );
            let evidence: Vec<(u64, String)> = retried_ids
                .iter()
                .map(|id| (*id, evidence.get(id).cloned().unwrap_or_default()))
                .collect();
            orch.record_retry(repo, run.id, run.run_attempt, &evidence, mode_label)
                .await;
            true
        }
        // 403 because the token lacks Actions: write (ADR-0027): not actioned, so
        // it is tried again once the permission is ticked. The client warned.
        Err(GithubError::Status { status: 403, .. })
            if orch.gh.blocked(repo, Permission::ActionsWrite) =>
        {
            false
        }
        // 403: the run is already re-running (a human clicked re-run, or our own
        // prior fire in a race). Nothing to do — mark it actioned so we don't hammer.
        Err(GithubError::Status { status: 403, .. }) => {
            info!(
                "retry: run {} in {repo} already re-running — skipping",
                run.id
            );
            true
        }
        Err(e) => {
            warn!("retry: re-run run {} in {repo} failed: {e}", run.id);
            false
        }
    }
}

/// Distil a job's provenance into a [`JobContext`], with `code`, what faber
/// read of whose code the run carries ([`Orchestrator::read_codes`]). The gate
/// judges both actors: who first caused the run, and who caused this attempt.
pub(crate) fn job_context(run: &WorkflowRun, job: &Job, code: Code) -> JobContext {
    JobContext {
        event: run.event.clone(),
        actor: run
            .actor
            .as_ref()
            .map(|a| a.login.clone())
            .unwrap_or_default(),
        triggering_actor: run.triggering_actor.as_ref().map(|a| a.login.clone()),
        base_repo: run.repository.full_name.clone(),
        // Sourced from the freshly-fetched run payload, so it reflects the repo's
        // current visibility — not a possibly-stale discovery-search index.
        private: run.repository.private,
        head_repo: run.head_repository.as_ref().map(|r| r.full_name.clone()),
        code,
        labels: job.labels.clone(),
        status: job.status.clone(),
    }
}

/// Whose code `run` carries, as this cycle read it, or as it is without a read.
/// `None` for a run whose read GitHub deferred to the next poll.
fn read_code(codes: &Codes, run: &WorkflowRun) -> Option<Code> {
    codes
        .get(&run.id)
        .cloned()
        .or_else(|| authorship::without_reads(run))
}

/// Resolve when the process is asked to terminate — `SIGTERM` (how launchd
/// stops a LaunchAgent) or `SIGINT` (Ctrl-C in a foreground run). The caller
/// turns this into a graceful drain; a second signal during the drain forces an
/// immediate exit.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => info!("received SIGTERM"),
        _ = int.recv() => info!("received SIGINT"),
    }
}

/// Which [`Counts`] bucket a finished run lands in.
///
/// The input is `job_ok` — the runner *container's* own exit — so this measures
/// what faber delivered, never what CI concluded. An ephemeral Actions runner
/// exits 0 whether the job it ran passed or failed, so a red job is `Completed`.
/// The container's exit carries no CI verdict and must not be read as one. The
/// popover shows GitHub's own conclusion beside it (`OutcomeEvent::conclusion`,
/// ADR-0044), but only as GitHub reported it, and it never reaches this tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tally {
    /// The runner executed a job and exited cleanly. faber did its part.
    Completed,
    /// faber's own infrastructure didn't deliver a job run: sandbox setup error,
    /// lifetime-cap timeout, or a non-zero exit (in practice a killed container).
    Faulted,
    /// A deliberate stand-down — the runner ran no job, and that was correct.
    /// Counting it either way would misreport: it's neither delivery nor fault.
    Neither,
}

/// Classify a finished run. See [`Tally`] — reclaim is checked before the
/// non-zero exit, because faber kills a reclaimed runner itself.
fn tally_of(report: &RunReport) -> Tally {
    if report.job_ok {
        Tally::Completed
    } else if report.reclaimed {
        Tally::Neither
    } else {
        Tally::Faulted
    }
}

/// The `result` label a finished run is recorded under, in the audit log and on
/// the panel. Ordered so the *cause* of the ending wins over its side effects: a
/// reclaimed or lifetime-capped runner also exits non-zero, and reading either as
/// a plain `failure` is what made those two invisible.
/// How the run's runner ended, for the outcome's `end` and the retry ledger
/// (ADR-0003, amendment 2026-10-18). Anything that is not one of faber's own acts
/// — a non-zero exit, a failed setup — is [`EndKind::Other`], never a reason to
/// retry.
fn end_of(report: &RunReport) -> EndKind {
    if report.oom_runner || report.oom_dind {
        EndKind::Oom
    } else if report.reclaimed {
        EndKind::Reclaimed
    } else if report.timed_out {
        EndKind::LifetimeCap
    } else if report.lost {
        EndKind::Lost
    } else if report.job_ok {
        EndKind::Clean
    } else {
        EndKind::Other
    }
}

fn result_of(report: &RunReport) -> &'static str {
    if report.job_ok {
        "success"
    } else if report.reclaimed {
        "reclaimed"
    } else if report.timed_out {
        "timeout"
    } else if report.infra_error {
        "infra_error"
    } else {
        "failure"
    }
}

/// Everything about a finished run that its [`RunReport`] doesn't carry: what
/// faber minted for it, how long it waited to be admitted, and which learner class
/// the sample was attributed to. Passed as one value because these travel
/// together and always have — as nine positional arguments they were a
/// `too_many_arguments` allow and an invitation to transpose two of them.
struct Finished<'a> {
    repo: &'a str,
    /// The job the runner was minted for.
    job_id: u64,
    /// The job GitHub actually dispatched to it, when that differs from `job_id`.
    executed_job_id: Option<u64>,
    /// The learner class this sample was fed into — `None` when it was refused
    /// (see the `class` field of [`Record`]).
    class: Option<&'a str>,
    /// `repo · workflow · job`, recorded even when `class` isn't.
    label: Option<&'a str>,
    /// The routing profile that served it.
    profile: &'a str,
    /// The minted runner name.
    runner: &'a str,
    /// Which provision attempt for this job this run was (1-based).
    attempt: u32,
    /// Time parked in admission before a container existed.
    wait_ms: u64,
    /// The same time, gate by gate (issue #85).
    parked: &'a Parked,
    /// Claim → finish, `wait_ms` included.
    duration_ms: u64,
}

/// The human label for a job: `repo · workflow · job`. One shape, one function,
/// because a decision line and its outcome line are meant to be greppable by the
/// same string — two independently-formatted "labels" that differ by a token are
/// exactly the sort of thing that makes a log searchable only by accident.
fn job_label(repo: &str, workflow: Option<&str>, job_name: &str) -> String {
    format!("{repo} · {} · {job_name}", workflow.unwrap_or("workflow"))
}

/// A short, stable label for a verdict (dashboard + audit).
fn verdict_str(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Provision => "provision".to_string(),
        Verdict::Skip(SkipReason::NotTargeted) => "skip:not_targeted".to_string(),
        Verdict::Skip(SkipReason::NotQueued) => "skip:not_queued".to_string(),
        Verdict::Refuse(RefuseReason::PublicRepo) => "refuse:public_repo".to_string(),
        Verdict::Refuse(RefuseReason::Fork) => "refuse:fork".to_string(),
        Verdict::Refuse(RefuseReason::DisallowedEvent(e)) => format!("refuse:event:{e}"),
        Verdict::Refuse(RefuseReason::UntrustedActor(a)) => format!("refuse:actor:{a}"),
        Verdict::Refuse(RefuseReason::ThirdParty(a)) => format!("refuse:third_party:{a}"),
        Verdict::Refuse(RefuseReason::DestroyedGroup) => "refuse:destroyed_group".to_string(),
        Verdict::Refuse(RefuseReason::Unread) => "refuse:unread".to_string(),
        Verdict::Refuse(RefuseReason::JobTokenDispatch) => "refuse:job_token_dispatch".to_string(),
    }
}

// --- ADR-0030: gate bypasses ----------------------------------------------------

impl Orchestrator {
    /// What a gate bypass triggers on this host. Host-wide, so it is read off the
    /// first profile, like the boundary it is reported with.
    fn on_bypass(&self) -> OnBypass {
        self.cfg
            .profiles
            .first()
            .map(|p| p.policy.on_bypass())
            .unwrap_or_default()
    }

    /// Fold one repo's runner listing into what this host knows about its peers
    /// (#94): warn once about each faber instance seen minting here that is not
    /// this one, and record each of their runners that went offline still holding
    /// a job as `peer_runner_lost` — before this, the only trace was a deferral
    /// line that read like this host's own trouble.
    async fn observe_peers(&self, repo: &str, runners: &[Runner]) {
        let (new, lost) = {
            let mut peers = self.peers.lock().unwrap();
            (
                peers.observe(&self.instance, repo, runners, now_ms()),
                peers.newly_lost(&self.instance, runners),
            )
        };
        for (peer, n) in new {
            warn!(
                "peer faber instance detected: {peer} has {n} runner(s) on {repo}, which this \
                 instance ({}) also serves",
                self.instance
            );
        }
        for r in lost {
            // The job GitHub dispatched to it when the discovery scan saw one,
            // else the job its name says it was minted for.
            let executed = self.executed.lock().await.get(&r.name).map(|e| e.job_id);
            let job_id = executed.or_else(|| faber_job_id(&r.name)).unwrap_or(0);
            info!(
                "{repo}: peer {}'s runner {} went offline holding job {job_id}",
                r.instance, r.name
            );
            let rec = Record {
                runner: Some(r.name.clone()),
                action: Some(format!("peer {}", r.instance)),
                cause: Some("offline on GitHub with a job still assigned".to_string()),
                ..Record::new("peer_runner_lost", repo, job_id)
            };
            if let Err(e) = self.audit.append(&rec).await {
                warn!("audit append failed: {e}");
            }
        }
    }

    /// Record that this host is minting `name` on `repo` for `job_id`: in memory,
    /// for bypass detection ([`Self::note_minted`]), and as a `mint` line in the
    /// audit log, which is what lets a later process tell this runner from any
    /// other after a restart (#105, [`vouched_mint`]).
    ///
    /// Called the moment the name is minted, before the runner registers. A line
    /// that fails to append costs nothing now — this process holds the mint in
    /// memory — and after a restart only the GitHub side of the runner: with no
    /// record, a reclaim of it is refused rather than guessed at.
    async fn record_mint(&self, repo: &str, job_id: u64, name: &str, profile_idx: usize) {
        self.note_minted(name, job_id, profile_idx);
        let profile = &self.cfg.profiles[profile_idx].name;
        if let Err(e) = self
            .audit
            .append(&Record::mint(repo, job_id, name, profile))
            .await
        {
            warn!("audit append failed for the mint of {name}: {e}");
        }
    }

    /// Remember that this host minted `name` for `job_id` under `profile_idx`, so
    /// detection can tell its own runners from a peer's (ADR-0030).
    fn note_minted(&self, name: &str, job_id: u64, profile_idx: usize) {
        let now = now_ms();
        let mut minted = self.minted.lock().unwrap();
        prune_minted(&mut minted, now);
        minted.insert(
            name.to_string(),
            Minted {
                minted_for: job_id,
                profile_idx,
                at_ms: now,
            },
        );
    }

    /// Whether `job_id` was recorded as a gate bypass within the last week.
    #[cfg(test)]
    async fn was_bypass(&self, job_id: u64) -> bool {
        self.metrics.lock().await.bypassed.contains_key(&job_id)
    }

    /// Whether faber force-killed `job_id`'s runner as an enforced trust bypass
    /// within the last week — the only bypasses auto-retry must leave alone.
    async fn was_force_killed(&self, job_id: u64) -> bool {
        self.metrics.lock().await.killed.contains_key(&job_id)
    }

    /// Read whose code each run in `runs` carries that the gate judges this
    /// cycle (ADR-0059 §3): one with a queued job faber routes, or with a job a
    /// runner this host minted took in place of its own. A run that needs no
    /// read is left to [`read_code`].
    ///
    /// A run GitHub said nothing about is left out, so its jobs wait for the
    /// next poll; one whose code faber could not read is refused. The fault
    /// feed records each on its transitions, with the first one's cause.
    async fn read_codes(&self, repo: &str, runs: &[(WorkflowRun, Vec<Job>)]) -> Codes {
        let judged: Vec<&WorkflowRun> = {
            let minted = self.minted.lock().unwrap();
            runs.iter()
                .filter(|(run, jobs)| {
                    authorship::without_reads(run).is_none()
                        && jobs.iter().any(|job| {
                            let mints = job.status == "queued"
                                && match_profile(&self.cfg.profiles, &job.labels).is_some();
                            let took = job
                                .runner_name
                                .as_deref()
                                .and_then(|r| minted.get(r))
                                .is_some_and(|m| m.minted_for != job.id);
                            mints || took
                        })
                })
                .map(|(run, _)| run)
                .collect()
        };
        let mut codes = Codes::new();
        let mut deferred = Vec::new();
        let mut unread = Vec::new();
        for (id, read) in authorship::codes_of(&*self.gh, judged).await {
            match read {
                Ok(code) => {
                    if let Code::Unread(why) = &code {
                        unread.push(format!("run {id}: {why}"));
                    }
                    codes.insert(id, code);
                }
                Err(Deferred(why)) => deferred.push(format!("run {id}: {why}")),
            }
        }
        let summary = |found: &[String], what: &str| {
            found
                .first()
                .map(|first| format!("{} run(s) {what}: {first}", found.len()))
        };
        self.observe_fault(
            repo,
            "gate_read",
            summary(
                &deferred,
                "wait for the next poll, GitHub did not say whose code they carry",
            ),
        )
        .await;
        self.observe_fault(
            repo,
            "gate_unread",
            summary(
                &unread,
                "are refused, faber could not read whose code they carry",
            ),
        )
        .await;
        codes
    }

    /// Re-judge the jobs this cycle's scan saw on runners this host minted, and
    /// record each one the gate refuses — once — as a gate bypass (ADR-0030).
    /// Under `on_bypass = "enforce"` a trust bypass is also answered.
    async fn detect_bypasses(&self, repo: &str, runs: &[(WorkflowRun, Vec<Job>)], codes: &Codes) {
        let now = now_ms();
        let found = {
            let mut minted = self.minted.lock().unwrap();
            prune_minted(&mut minted, now);
            find_bypasses(repo, runs, codes, &minted, &self.cfg.profiles, now)
        };
        if found.is_empty() {
            return;
        }
        let on_bypass = self.on_bypass();
        for b in found {
            let trust_7d = {
                let mut m = self.metrics.lock().await;
                if !m.note_bypass(b.job_id, b.class, now) {
                    continue;
                }
                m.recent_bypasses.push_back(b.clone());
                while m.recent_bypasses.len() > BYPASS_RECENT_CAP {
                    m.recent_bypasses.pop_front();
                }
                m.trust_bypasses_since(now)
            };
            warn!(
                "{repo}: gate bypass ({}) — job {} ran on runner {}, minted for job {}, \
                 although the gate refuses it ({})",
                b.class.as_str(),
                b.job_id,
                b.runner,
                b.minted_for,
                b.verdict
            );
            self.record_bypass_line(&b, b.class.as_str(), bypass_cause(&b))
                .await;
            if let Some(msg) = bypass_record_warning(on_bypass, trust_7d) {
                if b.class.may_be_hostile() {
                    warn!("{msg}");
                }
            }
            let response = bypass_response(on_bypass, b.class);
            if response.kill {
                self.kill_bypassed_runner(&b).await;
            }
            if response.purge_cache {
                self.cache_purges.lock().unwrap().insert(b.repo.clone());
            }
            if response.hold {
                self.hold_repo(&b, now).await;
            }
        }
    }

    /// One `gate_bypass` line on the faults feed and in the audit log, naming the
    /// job that ran, the runner, and (in `cause`) the job that runner was minted
    /// for. `action` is the class for the detection itself, or what enforcement
    /// did about it.
    async fn record_bypass_line(&self, b: &BypassView, action: &str, cause: String) {
        {
            let mut m = self.metrics.lock().await;
            m.recent_faults.push_back(FaultEvent {
                ts_ms: now_ms(),
                repo: b.repo.clone(),
                scope: "gate_bypass".to_string(),
                action: action.to_string(),
                cause: cause.clone(),
            });
            while m.recent_faults.len() > FAULT_RECENT_CAP {
                m.recent_faults.pop_front();
            }
        }
        let rec = Record {
            job_id: b.job_id,
            run_id: Some(b.run_id),
            runner: Some(b.runner.clone()),
            verdict: Some(b.verdict.clone()),
            label: Some(job_label(&b.repo, Some(&b.workflow), &b.job_name)),
            ..Record::fault(&b.repo, "gate_bypass", action, Some(cause))
        };
        if let Err(e) = self.audit.append(&rec).await {
            warn!("audit append failed: {e}");
        }
    }

    /// Kill the runner a trust bypass is running on, without asking it first
    /// (ADR-0030; ADR-0017's clearance protects legitimate work, and this is not).
    /// Nothing to do once its claim is gone: the runner already exited.
    async fn kill_bypassed_runner(&self, b: &BypassView) {
        let containerized = self
            .inflight
            .lock()
            .await
            .values()
            .find(|c| c.runner_name.lock().unwrap().as_deref() == Some(b.runner.as_str()))
            .map(|c| c.containerized);
        match containerized {
            None => info!(
                "gate bypass: runner {} has no live claim — it has already exited",
                b.runner
            ),
            Some(false) => warn!(
                "gate bypass: runner {} is not in a container, so there is no container \
                 for faber to kill — it runs to the end of its job",
                b.runner
            ),
            Some(true) => match crate::docker::force_kill_runner(&b.runner).await {
                Ok(()) => {
                    self.metrics.lock().await.note_killed(b.job_id, now_ms());
                    self.record_bypass_line(
                        b,
                        "killed",
                        format!(
                            "killed runner {} mid-job: job {} is a trust bypass ({})",
                            b.runner, b.job_id, b.verdict
                        ),
                    )
                    .await
                }
                Err(e) => warn!("gate bypass: killing runner {} failed: {e:#}", b.runner),
            },
        }
    }

    /// Hold minting for the bypassed job's repo until the operator clears it
    /// (ADR-0030). Holding an already-held repo replaces the reason and keeps the
    /// original start time, as ADR-0022's hold does.
    async fn hold_repo(&self, b: &BypassView, now: u64) {
        let reason = format!(
            "trust bypass: job {} ran on runner {} ({})",
            b.job_id, b.runner, b.verdict
        );
        let fresh = {
            let mut holds = self.repo_holds.lock().unwrap();
            match holds.get_mut(&b.repo) {
                Some(h) => {
                    h.reason = reason.clone();
                    false
                }
                None => {
                    holds.insert(
                        b.repo.clone(),
                        RepoHold {
                            repo: b.repo.clone(),
                            reason: reason.clone(),
                            by: BYPASS_HOLD_BY.to_string(),
                            since_ms: now,
                        },
                    );
                    true
                }
            }
        };
        self.persist_repo_holds();
        if fresh {
            warn!("{}: minting held — {reason}", b.repo);
            self.record_bypass_line(b, "held", format!("minting for {} held: {reason}", b.repo))
                .await;
        }
    }

    /// The repo holds in force, by repo.
    pub fn repo_holds(&self) -> Vec<RepoHold> {
        self.repo_holds.lock().unwrap().values().cloned().collect()
    }

    /// Hold `hold.repo` as faber would after a bypass, for the web layer's
    /// tests of clearing one.
    #[cfg(test)]
    pub(crate) fn insert_repo_hold(&self, hold: RepoHold) {
        self.repo_holds
            .lock()
            .unwrap()
            .insert(hold.repo.clone(), hold);
    }

    /// Clear `repo`'s hold, returning it if there was one. Parked claims for the
    /// repo notice within [`REPO_BLOCK_POLL`].
    pub async fn clear_repo_hold(&self, repo: &str) -> Option<RepoHold> {
        let cleared = self.repo_holds.lock().unwrap().remove(repo)?;
        self.persist_repo_holds();
        info!(
            "{repo}: minting hold cleared (held since {}: {})",
            cleared.since_utc(),
            cleared.reason
        );
        self.record_fault(
            repo,
            "gate_bypass",
            "hold_cleared",
            Some(format!(
                "hold set by {} cleared by the operator",
                cleared.by
            )),
        )
        .await;
        Some(cleared)
    }

    /// Write the repo holds into the overrides file, leaving the rest of it as it
    /// is. Best-effort, like every overrides write.
    fn persist_repo_holds(&self) {
        if !self.persist_holds {
            return;
        }
        // Under the overrides lock, so no other writer's fields are lost, and read
        // strictly: a file that does not parse is left alone rather than replaced
        // with defaults plus the holds.
        if let Err(e) = crate::config::Overrides::update(|o| o.repo_holds = self.repo_holds()) {
            warn!("persisting repo holds skipped: {e:#}");
        }
    }

    /// Why a runner with `runner_labels` must not be minted for `repo` right now,
    /// if it must not (ADR-0030): the repo is held, or — under `enforce` — a
    /// trust-refused job queued in the repo could be handed that runner.
    async fn repo_block(&self, repo: &str, runner_labels: &[String]) -> Option<WaitView> {
        // ADR-0027: nothing is minted on a credential that claims nothing, and
        // nothing on a repo the token cannot administer.
        if let Some(why) = self.gh.claim_block() {
            return Some(WaitView::new(
                "credential",
                format!("GitHub credential: {why}"),
            ));
        }
        if self.gh.blocked(repo, Permission::Administration) {
            return Some(WaitView::new(
                "permission",
                format!(
                    "the GitHub token lacks {} on {repo}: {}",
                    Permission::Administration.label(),
                    Permission::Administration.fix(Feature::Core)
                ),
            ));
        }
        let hold = self.repo_holds.lock().unwrap().get(repo).cloned();
        if let Some(h) = hold {
            return Some(WaitView::new(
                "repo-held",
                format!(
                    "minting for {repo} held since {}: {}",
                    h.since_utc(),
                    h.reason
                ),
            ));
        }
        if self.on_bypass() != OnBypass::Enforce {
            return None;
        }
        let m = self.metrics.lock().await;
        refused_could_take(m.refused_queued.values(), repo, runner_labels).map(|r| {
            WaitView::new(
                "refused-job",
                format!(
                    "a refused {} job in this repo could take this runner",
                    r.verdict.strip_prefix("refuse:").unwrap_or(&r.verdict)
                ),
            )
        })
    }

    /// Why a runner for `profile` starting from `image` must not be minted right
    /// now, if it must not (#63): the image is the profile's own and is confirmed
    /// missing from this host. Registering a runner into it would only fail to
    /// start and leave an offline registration behind.
    ///
    /// Since runners start with `--pull=never` (#156) an image that is only
    /// absent waits too, under `image-pull`, where `docker run` used to pull it
    /// itself: until the first check of it has run, and while faber's own pull is
    /// under way or due. A daemon that is not answering decides nothing here.
    fn image_block(&self, profile: &str, image: &str) -> Option<WaitView> {
        let profile_image = self.profile_image(profile)?;
        let images = self.profile_images.lock().unwrap();
        match images.wait(profile, profile_image, image)? {
            ImageWait::Missing(m) => Some(WaitView::new(
                "image-missing",
                format!(
                    "profile image missing: {profile} ({}) since {} — rebuild or pull it; \
                     faber resumes on its own once it is back",
                    m.image,
                    crate::config::rfc3339_utc(m.since_ms)
                ),
            )),
            ImageWait::Unchecked if self.limiter.docker_reachable() => Some(WaitView::new(
                "image-pull",
                format!("checking that {image} is on this host before starting a runner from it"),
            )),
            ImageWait::Unchecked => None,
            ImageWait::Absent => Some(WaitView::new(
                "image-pull",
                format!(
                    "{image} is not on this host — faber is pulling it, at most every {} min, \
                     and starts runners only from a local image",
                    crate::profile_image::PULL_RETRY.as_secs() / 60
                ),
            )),
        }
    }

    /// Park until [`Orchestrator::image_block`] clears, saying why through
    /// `waiting`. `false` means the claim was reclaimed while it waited.
    async fn await_profile_image(
        &self,
        profile: &str,
        image: &str,
        cancel: &mut watch::Receiver<bool>,
        waiting: &std::sync::Mutex<Parking>,
    ) -> bool {
        loop {
            let Some(why) = self.image_block(profile, image) else {
                return true;
            };
            publish_wait(waiting, why);
            tokio::select! {
                biased;
                _ = wait_cancelled(cancel) => return false,
                _ = tokio::time::sleep(REPO_BLOCK_POLL) => {}
            }
        }
    }

    /// Park until a runner may be minted while `generate-jitconfig` is failing
    /// with 5xx ([`crate::github::MintBackoff`], #89), saying so through
    /// `waiting`. `false` when the claim was cancelled while it waited.
    ///
    /// This takes no turn: the caller takes one with [`crate::github::Github::mint_turn`]
    /// once it has won admission, and comes back here if another claim took it.
    async fn await_mint_clear(
        &self,
        cancel: &mut watch::Receiver<bool>,
        waiting: &std::sync::Mutex<Parking>,
    ) -> bool {
        loop {
            let Some(wait_ms) = self.gh.mint_wait(now_ms()) else {
                return true;
            };
            publish_wait(
                waiting,
                WaitView::new(
                    "github",
                    format!(
                        "GitHub failed the last {} runner mints with a server error — \
                         next try in {}s",
                        self.gh.mint_failures(),
                        wait_ms.div_ceil(1000)
                    ),
                ),
            );
            tokio::select! {
                biased;
                _ = wait_cancelled(cancel) => return false,
                _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {}
            }
        }
    }

    /// Park until [`Orchestrator::repo_block`] clears, saying why through
    /// `waiting`. Returns at once when nothing blocks; `false` means the claim was
    /// reclaimed while it waited.
    async fn await_repo_clear(
        &self,
        repo: &str,
        runner_labels: &[String],
        cancel: &mut watch::Receiver<bool>,
        waiting: &std::sync::Mutex<Parking>,
    ) -> bool {
        loop {
            let Some(why) = self.repo_block(repo, runner_labels).await else {
                return true;
            };
            publish_wait(waiting, why);
            tokio::select! {
                biased;
                _ = wait_cancelled(cancel) => return false,
                _ = tokio::time::sleep(REPO_BLOCK_POLL) => {}
            }
        }
    }
}

/// The image git-mirror refreshes run on (ADR-0035 §1): the first docker
/// profile with `git_mirror = true`, or `None` when no profile asks for a
/// mirror, which leaves mirrors off for the whole process.
pub(crate) fn git_mirror_image(profiles: &[Profile]) -> Option<String> {
    profiles.iter().find_map(|p| match &p.backend {
        Backend::Docker(d) if d.git_mirror => Some(d.image.clone()),
        _ => None,
    })
}

/// Delete each cache volume an enforced trust bypass condemned, once no claim of
/// its repo has a container that could mount it (ADR-0030). A volume still in
/// use waits for the next cycle.
async fn purge_bypassed_caches(orch: &Arc<Orchestrator>) {
    let repos: Vec<String> = orch.cache_purges.lock().unwrap().iter().cloned().collect();
    if repos.is_empty() {
        return;
    }
    // A host with no Docker profile has no cache volume, and maybe no Docker.
    if !orch
        .cfg
        .profiles
        .iter()
        .any(|p| matches!(p.backend, Backend::Docker(_)))
    {
        orch.cache_purges.lock().unwrap().clear();
        return;
    }
    for repo in repos {
        if cache_mounted(&*orch.inflight.lock().await, &repo) {
            debug!("{repo}: cache deletion deferred — a running claim may mount it");
            continue;
        }
        match crate::docker::remove_repo_cache_volumes(&repo).await {
            Ok(crate::docker::CachePurge::InUse(name)) => {
                info!("{repo}: cache volume {name} still in use — deleting it next cycle")
            }
            Ok(done) => {
                orch.cache_purges.lock().unwrap().remove(&repo);
                for (action, cause) in purge_report(&done) {
                    if action == "cache_unlabelled" {
                        warn!("{repo}: gate bypass — {cause}");
                    } else {
                        info!("{repo}: gate bypass — {cause}");
                    }
                    orch.record_fault(&repo, "gate_bypass", action, Some(cause))
                        .await;
                }
            }
            Err(e) => {
                warn!("{repo}: deleting the cache volume failed ({e:#}); retrying next cycle")
            }
        }
    }
}

/// The fault lines a finished cache purge records: `(action, cause)`. An
/// unlabelled same-named volume gets its own line and is never reported as
/// clean — "no cache volume to delete" would be a false all-clear.
fn purge_report(done: &crate::docker::CachePurge) -> Vec<(&'static str, String)> {
    use crate::docker::CachePurge;
    let deleted = |names: &[String]| -> Option<(&'static str, String)> {
        (!names.is_empty()).then(|| {
            (
                "cache_deleted",
                format!(
                    "deleted cache volume {} after a trust bypass",
                    names.join(", ")
                ),
            )
        })
    };
    match done {
        CachePurge::InUse(_) => Vec::new(),
        CachePurge::Removed(names) => vec![deleted(names).unwrap_or((
            "cache_deleted",
            "no cache volume to delete after a trust bypass".to_string(),
        ))],
        CachePurge::Unlabelled { removed, name } => deleted(removed)
            .into_iter()
            .chain([(
                "cache_unlabelled",
                format!(
                    "unlabelled cache volume {name} found — not deleted (no provenance \
                     label); remove it by hand"
                ),
            )])
            .collect(),
    }
}

/// Whether a claim of `repo` has (or is about to have) a container, which may be
/// mounting the repo's cache volume.
fn cache_mounted(claims: &HashMap<u64, Claim>, repo: &str) -> bool {
    claims
        .values()
        .any(|c| c.repo == repo && c.containerized && c.admitted.load(Ordering::SeqCst))
}

/// Drop runner records older than [`EXECUTED_TTL`]: a runner takes one job.
fn prune_minted(minted: &mut HashMap<String, Minted>, now: u64) {
    let ttl = EXECUTED_TTL.as_millis() as u64;
    minted.retain(|_, m| now.saturating_sub(m.at_ms) <= ttl);
}

/// The gate bypasses in one repo's scan: jobs that ran on a runner **this host**
/// minted, other than the job it was minted for, that the gate's trust checks
/// refuse under the policy of the profile that minted it (ADR-0030).
///
/// A runner absent from `minted` is not this host's — a peer's, a human's, or
/// one minted before a restart and not adopted — and is the peer's to judge.
fn find_bypasses(
    repo: &str,
    runs: &[(WorkflowRun, Vec<Job>)],
    codes: &Codes,
    minted: &HashMap<String, Minted>,
    profiles: &[Profile],
    now: u64,
) -> Vec<BypassView> {
    let mut out = Vec::new();
    for (run, jobs) in runs {
        for job in jobs {
            let Some(runner) = job.runner_name.as_deref() else {
                continue;
            };
            let Some(m) = minted.get(runner) else {
                continue;
            };
            // The job it was minted for passed the gate to be minted for.
            if job.id == m.minted_for {
                continue;
            }
            let Some(profile) = profiles.get(m.profile_idx) else {
                continue;
            };
            // A run whose read was deferred is judged on provenance alone
            // (`Code::Own`): a refusal that needs no read is real, but never
            // judge it as unread, which would be a bypass GitHub's silence made up.
            let code = read_code(codes, run).unwrap_or(Code::Own);
            let verdict = gate::trust_verdict(&job_context(run, job, code), &profile.policy);
            let Verdict::Refuse(reason) = &verdict else {
                continue;
            };
            out.push(BypassView {
                ts_ms: now,
                repo: repo.to_string(),
                run_id: run.id,
                job_id: job.id,
                job_name: job.name.clone(),
                workflow: run.name.clone().unwrap_or_default(),
                runner: runner.to_string(),
                minted_for: m.minted_for,
                class: gate::classify(reason),
                verdict: verdict_str(&verdict),
            });
        }
    }
    out
}

/// What enforcement does about one bypass (ADR-0030). Everything or nothing:
/// only `enforce`, and only a trust bypass. A policy bypass is the repo's own
/// trusted code, and killing it helps nobody.
#[derive(Debug, Default, PartialEq, Eq)]
struct BypassResponse {
    kill: bool,
    purge_cache: bool,
    hold: bool,
}

fn bypass_response(on_bypass: OnBypass, class: BypassClass) -> BypassResponse {
    let act = on_bypass == OnBypass::Enforce && class.may_be_hostile();
    BypassResponse {
        kill: act,
        purge_cache: act,
        hold: act,
    }
}

/// The trust-refused job queued in `repo` that a runner with `runner_labels`
/// could be handed, if any — oldest first. GitHub hands a job with labels `L` to
/// a runner with labels `S` iff `L ⊆ S`.
fn refused_could_take<'a>(
    refused: impl IntoIterator<Item = &'a RefusedJobView>,
    repo: &str,
    runner_labels: &[String],
) -> Option<&'a RefusedJobView> {
    refused
        .into_iter()
        .filter(|r| r.trust && r.repo == repo)
        .filter(|r| r.labels.iter().all(|l| runner_labels.contains(l)))
        .min_by_key(|r| (r.since_ms, r.job_id))
}

/// The fault-feed cause for a detected bypass.
fn bypass_cause(b: &BypassView) -> String {
    format!(
        "job {} ({} · {}) ran on runner {}, minted for job {}; the gate refuses it: {} \
         ({} bypass)",
        b.job_id,
        if b.workflow.is_empty() {
            "workflow"
        } else {
            &b.workflow
        },
        b.job_name,
        b.runner,
        b.minted_for,
        b.verdict,
        b.class.as_str()
    )
}

/// The bypasses recorded in audit `entries`: `(job_id, ts_ms, class)`.
fn replayed_bypasses(entries: &[crate::audit::Entry]) -> Vec<(u64, u64, BypassClass)> {
    entries
        .iter()
        .filter(|e| e.kind == "fault" && e.result.as_deref() == Some("gate_bypass"))
        .filter_map(|e| {
            let class = match e.action.as_deref()? {
                "trust" => BypassClass::Trust,
                "policy" => BypassClass::Policy,
                "third_party" => BypassClass::ThirdParty,
                _ => return None,
            };
            Some((e.job_id, e.ts_ms, class))
        })
        .collect()
}

/// The force-kills recorded in audit `entries`: `(job_id, ts_ms)`.
fn replayed_kills(entries: &[crate::audit::Entry]) -> Vec<(u64, u64)> {
    entries
        .iter()
        .filter(|e| e.kind == "fault" && e.result.as_deref() == Some("gate_bypass"))
        .filter(|e| e.action.as_deref() == Some("killed") && e.job_id != 0)
        .map(|e| (e.job_id, e.ts_ms))
        .collect()
}

/// The faults still open at the end of audit `entries`, oldest first: every
/// `scope/repo` whose last edge is `failed` or `passing_through` rather than
/// `recovered`. One-shot lines (`request_failed`, the `gate_bypass` actions)
/// open nothing.
///
/// A restart used to forget them. The fault gate then saw the first healthy
/// check as no change and wrote no `recovered`, so the `failed` line from before
/// the restart read as an open fault in `faber status` until it aged out of the
/// window — a disk fault left that way by a lowered `disk_floor_gib` for a day.
pub(crate) fn replayed_open_faults(entries: &[crate::audit::Entry]) -> Vec<FaultEvent> {
    let mut open: HashMap<String, FaultEvent> = HashMap::new();
    for e in entries.iter().filter(|e| e.kind == "fault") {
        let (Some(scope), Some(action)) = (e.result.as_deref(), e.action.as_deref()) else {
            continue;
        };
        let key = format!("{scope}/{}", e.repo);
        match action {
            "failed" | "passing_through" => {
                open.insert(
                    key,
                    FaultEvent {
                        ts_ms: e.ts_ms,
                        repo: e.repo.clone(),
                        scope: scope.to_string(),
                        action: action.to_string(),
                        cause: e.cause.clone().unwrap_or_default(),
                    },
                );
            }
            "recovered" => {
                open.remove(&key);
            }
            _ => {}
        }
    }
    let mut faults: Vec<FaultEvent> = open.into_values().collect();
    faults.sort_by_key(|f| f.ts_ms);
    faults
}

/// The boot (and first-sighting) warning while `record` sees trust bypasses.
fn bypass_record_warning(on_bypass: OnBypass, trust_7d: usize) -> Option<String> {
    (on_bypass == OnBypass::Record && trust_7d > 0).then(|| {
        format!(
            "{trust_7d} trust bypass{} in the last 7 days — enforcement off: jobs the gate \
             refuses as possibly hostile ran on runners this host minted, and gate.on_bypass \
             is \"record\". Fix the workflows' runs-on routing, or set [gate] on_bypass = \
             \"enforce\" (ADR-0030).",
            if trust_7d == 1 { "" } else { "es" }
        )
    })
}

/// The boot warning for a repo hold restored from the overrides file.
fn repo_hold_warning(h: &RepoHold) -> String {
    format!(
        "minting for {} is held since {} by {}: {} — clear it from the dashboard or the \
         tray (ADR-0030)",
        h.repo,
        h.since_utc(),
        h.by,
        h.reason
    )
}

// --- ADR-0024 §4: what the janitor's cache ageing reads off the orchestrator ---

impl Orchestrator {
    /// The repos faber serves right now: the topic's discovery set, asked fresh
    /// rather than remembered, because nothing here keeps the last cycle's list.
    ///
    /// For the janitor's cache ageing ([`crate::cache`]), which needs the exact
    /// cache-volume name faber mounts for each of them: a volume created before
    /// ADR-0007's labels carries none, so its name is the only thing that says it
    /// is a cache. An error narrows that pass to the labelled volumes — ageing
    /// fewer volumes is the safe direction, so the caller logs it and goes on.
    ///
    /// The same ETag-conditional request the reconcile loop makes every cycle, so
    /// an unchanged answer costs a `304` GitHub does not bill.
    pub async fn served_repos(&self) -> Result<Vec<String>, GithubError> {
        self.gh.repos_with_topic(&self.owner, &self.cfg.topic).await
    }

    /// The autolimiter's probe image. Cache ageing runs its throwaway containers
    /// on it because it is already on the host, already pulled by the memory
    /// probe, and already in [`Config::protected_images`], so reclamation can
    /// never remove the image reclamation itself depends on.
    pub fn probe_image(&self) -> &str {
        &self.cfg.autolimit.probe_image
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A host with no `[services.*]`: the built-in names alone are services.
    static NO_SERVICES: crate::services::ServicesPolicy = crate::services::ServicesPolicy {
        postgres: None,
        per_job: std::collections::BTreeMap::new(),
    };
    use crate::backend::Backend;
    use crate::gate::TrustPolicy;
    use crate::github::{Actor, MinimalRepo};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;

    // --- pure note/prune seam ----------------------------------------------
    //
    // The cross-cycle dedup core: `record_decision` gates Skip/Refuse side
    // effects (counter + recent feed + audit append) on `note`, so pinning
    // `note`/`prune` here pins that behavior in isolation.

    #[test]
    fn persisting_job_same_verdict_records_once() {
        // The common case: a job stays queued/in-progress across many cycles.
        let mut m = Metrics::default();
        assert!(m.note(42, "skip:not_queued")); // first observation: record
        assert!(!m.note(42, "skip:not_queued")); // next cycle, same verdict: don't
        assert!(!m.note(42, "skip:not_queued")); // ...and again: still don't
    }

    #[test]
    fn genuine_verdict_change_records_again() {
        let mut m = Metrics::default();
        assert!(m.note(7, "provision"));
        assert!(!m.note(7, "provision"));
        // A real transition to a different verdict records exactly once...
        assert!(m.note(7, "skip:not_queued"));
        // ...then dedups again at the new verdict.
        assert!(!m.note(7, "skip:not_queued"));
    }

    #[test]
    fn distinct_jobs_are_tracked_independently() {
        let mut m = Metrics::default();
        assert!(m.note(1, "provision"));
        assert!(m.note(2, "provision")); // different job, same label: still records
        assert!(!m.note(1, "provision"));
        assert!(!m.note(2, "provision"));
    }

    fn refused_view(repo: &str, job_id: u64, verdict: &str, since_ms: u64) -> RefusedJobView {
        RefusedJobView {
            repo: repo.into(),
            job_id,
            run_id: 9,
            job_name: "deploy".into(),
            workflow: "CD".into(),
            event: "repository_dispatch".into(),
            verdict: verdict.into(),
            since_ms,
            labels: vec!["self-hosted".into(), "docker".into()],
            trust: verdict != "refuse:event:repository_dispatch",
        }
    }

    #[test]
    fn refused_queued_is_capped_keeping_the_oldest_and_counting_the_rest() {
        let mut m = Metrics::default();
        let v = "refuse:fork";
        let n = REFUSED_QUEUED_CAP as u64;
        let first: Vec<_> = (0..n)
            .map(|i| refused_view("owner/demo", i, v, 100 + i))
            .collect();
        assert_eq!(
            m.sync_refused("owner/demo", first).len(),
            REFUSED_QUEUED_CAP
        );
        // Ten newer jobs from another repo: none kept, none announced.
        let extra: Vec<_> = (0..10)
            .map(|i| refused_view("owner/other", 1000 + i, v, 9000 + i))
            .collect();
        assert!(m.sync_refused("owner/other", extra).is_empty());
        assert_eq!(m.refused_queued.len(), REFUSED_QUEUED_CAP);
        assert_eq!(m.refused_overflow_total(), 10);
        assert!(m.refused_queued.values().all(|r| r.repo == "owner/demo"));
        // Rescanning with fewer jobs recomputes the count rather than adding to it.
        let few: Vec<_> = (0..3)
            .map(|i| refused_view("owner/other", 1000 + i, v, 9000 + i))
            .collect();
        m.sync_refused("owner/other", few);
        assert_eq!(m.refused_overflow_total(), 3);
        // An older job from the second repo displaces the newest of the first.
        m.sync_refused("owner/other", vec![refused_view("owner/other", 2000, v, 1)]);
        assert!(m.refused_queued.contains_key(&2000));
        assert_eq!(m.refused_queued.len(), REFUSED_QUEUED_CAP);
        assert_eq!(m.refused_overflow_total(), 1);
    }

    #[test]
    fn refused_queued_inserts_keeps_since_and_prunes_per_repo() {
        let mut m = Metrics::default();
        let v = "refuse:event:repository_dispatch";
        let added = m.sync_refused(
            "owner/demo",
            vec![
                refused_view("owner/demo", 1, v, 100),
                refused_view("owner/demo", 2, v, 100),
            ],
        );
        assert_eq!(added.len(), 2, "both are new");
        m.sync_refused("owner/other", vec![refused_view("owner/other", 7, v, 50)]);

        // Next cycle: job 1 still refused (later "now"), job 2 gone, job 3 new.
        let added = m.sync_refused(
            "owner/demo",
            vec![
                refused_view("owner/demo", 1, v, 500),
                refused_view("owner/demo", 3, v, 500),
            ],
        );
        assert_eq!(added.iter().map(|a| a.job_id).collect::<Vec<_>>(), vec![3]);
        let ids: Vec<u64> = m.refused_queued_sorted().iter().map(|r| r.job_id).collect();
        assert_eq!(ids, vec![7, 1, 3], "oldest first; job 2 pruned");
        assert_eq!(
            m.refused_queued[&1].since_ms, 100,
            "first-seen time is kept"
        );

        // A verdict change updates the label but not since_ms.
        m.sync_refused(
            "owner/demo",
            vec![refused_view("owner/demo", 1, "refuse:fork", 900)],
        );
        assert_eq!(m.refused_queued[&1].verdict, "refuse:fork");
        assert_eq!(m.refused_queued[&1].since_ms, 100);

        // Scanning one repo never touches another's entries; an empty scan clears it.
        m.sync_refused("owner/demo", vec![]);
        assert_eq!(m.refused_queued.len(), 1);
        assert!(m.refused_queued.contains_key(&7));

        // A repo that left the topic is forgotten.
        m.retain_refused_repos(&["owner/demo".to_string()]);
        assert!(m.refused_queued.is_empty());
    }

    fn dind_refused_view(job_id: u64, at_ms: u64) -> DindRefusedView {
        DindRefusedView {
            repo: "owner/demo".into(),
            run_id: 9,
            job_id,
            job_name: "build".into(),
            profile: "linux-docker".into(),
            reason: "no daemon here".into(),
            at_ms,
        }
    }

    /// ADR-0026: a dind refusal is listed once per job (a retry refreshes it and
    /// is not counted again), ages out an hour after its last refusal, keeps the
    /// newest past the cap — and never lands in `refused_queued`, which would
    /// feed repo holds.
    #[test]
    fn dind_refusals_count_once_age_out_and_stay_out_of_refused_queued() {
        let mut m = Metrics::default();
        assert!(m.note_dind_refused(dind_refused_view(1, 1_000)));
        assert!(
            !m.note_dind_refused(dind_refused_view(1, 2_000)),
            "a retry refreshes"
        );
        assert_eq!(m.counts.dind_refused_total, 1);
        assert_eq!(m.dind_refused[&1].at_ms, 2_000);
        assert!(m.refused_queued.is_empty());

        // Job 1 is not refreshed for an hour; job 2's refusal drops it.
        assert!(m.note_dind_refused(dind_refused_view(2, 2_000 + DIND_REFUSED_TTL_MS)));
        let at = 2_000 + DIND_REFUSED_TTL_MS;
        let ids: Vec<u64> = m.dind_refused_sorted(at).iter().map(|r| r.job_id).collect();
        assert_eq!(ids, vec![2]);
        // Past the TTL it is not listed, though nothing new was inserted.
        assert!(m.dind_refused_sorted(at + DIND_REFUSED_TTL_MS).is_empty());

        // Past the cap the oldest go, the newest stay.
        let base = 10 * DIND_REFUSED_TTL_MS;
        for id in 100..(100 + DIND_REFUSED_CAP as u64 + 5) {
            m.note_dind_refused(dind_refused_view(id, base + id));
        }
        assert_eq!(m.dind_refused.len(), DIND_REFUSED_CAP);
        let sorted = m.dind_refused_sorted(base + 1_000);
        assert_eq!(
            sorted.first().map(|r| r.job_id),
            Some(105),
            "oldest first, 100..105 evicted"
        );
        assert_eq!(
            sorted.last().map(|r| r.job_id),
            Some(100 + DIND_REFUSED_CAP as u64 + 4)
        );
    }

    #[test]
    fn prune_drops_unseen_jobs_and_bounds_state() {
        let mut m = Metrics::default();
        m.note(1, "provision");
        m.note(2, "skip:not_queued");
        m.note(3, "refuse:fork");
        assert_eq!(m.jobs.len(), 3);

        // Jobs 1 and 3 are re-observed every cycle; job 2 never is. The miss grace
        // keeps a gone job for a few cycles, so drive past it: after > grace
        // consecutive misses only the truly-gone job is dropped, and the live set
        // is retained (its misses keep resetting).
        let seen: HashSet<u64> = [1, 3].into_iter().collect();
        for _ in 0..=PRUNE_MISS_GRACE {
            m.prune(&seen);
        }

        assert_eq!(m.jobs.len(), 2); // state shrank to the live working set
        assert!(m.jobs.contains_key(&1));
        assert!(!m.jobs.contains_key(&2));
        assert!(m.jobs.contains_key(&3));
        assert_eq!(
            m.jobs[&1].misses, 0,
            "a re-observed job never accrues misses"
        );

        // A pruned job reappearing is treated as new again (records once).
        assert!(m.note(2, "skip:not_queued"));
    }

    #[test]
    fn prune_to_empty_seen_clears_all_state() {
        let mut m = Metrics::default();
        m.note(10, "provision");
        m.note(11, "refuse:fork");
        // A run of fully-empty cycles (nothing queued/in-progress anywhere) clears
        // the map — but only once every job has been missed past the grace, so the
        // bound is a few cycles, not immediate.
        for _ in 0..=PRUNE_MISS_GRACE {
            m.prune(&HashSet::new());
        }
        assert!(m.jobs.is_empty());
    }

    // --- integration: the two spots where the bug lived --------------------
    //
    // These drive the real `process_run_jobs` seam (route → gate → record,
    // spawn-free) across two poll cycles and assert on the observable state
    // (`Counts`, the recent feed, the audit file, the returned intents). They
    // fail if the `record_decision` note-gate is removed (Skip re-records) or if
    // Provision recording moves back before the `inflight` claim (double-count).

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A throwaway directory removed (with its contents) on drop — so the
    /// audit-log assertions leave nothing behind, no external dependency needed.
    /// Drop runs on the unwinding path too, so a failed assertion still cleans up.
    pub(crate) struct TempDir {
        pub(crate) path: PathBuf,
    }

    impl TempDir {
        pub(crate) fn new() -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "faber-reconcile-test-{}-{}",
                std::process::id(),
                TMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A macOS VM backend for fixtures that never provision: the one backend
    /// that needs no Docker daemon to route, gate or report on.
    fn vm_backend() -> Backend {
        use crate::backend::{MacosVmConfig, VmTimeouts};
        use crate::vm::proto::{Cpus, ImageName, MemoryMib};
        Backend::MacosVm(MacosVmConfig {
            image: ImageName::try_from("macos-27").unwrap(),
            runner_version: "2.330.0".into(),
            cpus: Cpus::try_from(4).unwrap(),
            memory_mib: MemoryMib::try_from(8192).unwrap(),
            timeouts: VmTimeouts::default(),
            revive: crate::vm::revive::RevivePolicy::OFF,
        })
    }

    /// One profile whose labels a fixture job carries; empty allow-list, deny
    /// Dependabot — the default daemon policy.
    fn profiles() -> Vec<Profile> {
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        vec![Profile {
            name: "linux-docker".to_string(),
            labels: labels.clone(),
            backend: vm_backend(),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels),
        }]
    }

    /// A trusted owner-push run carrying one job in `status` with the profile's
    /// labels: `queued` gates to Provision, anything else to Skip(NotQueued).
    fn run_with_job(job_id: u64, status: &str) -> (WorkflowRun, Vec<Job>) {
        let run = WorkflowRun {
            id: 900,
            event: "push".to_string(),
            workflow_id: 900,
            name: None,
            status: Some("in_progress".to_string()),
            actor: Some(Actor {
                login: "owner".to_string(),
            }),
            triggering_actor: Some(Actor {
                login: "owner".to_string(),
            }),
            repository: MinimalRepo {
                full_name: "owner/demo".to_string(),
                fork: false,
                private: true,
            },
            head_repository: None,
            head_sha: "abc123".to_string(),
            run_attempt: 1,
            updated_at: None,
            created_at: None,
            head_branch: None,
            pull_requests: vec![],
        };
        let job = Job {
            id: job_id,
            name: "build".to_string(),
            status: status.to_string(),
            conclusion: None,
            labels: vec!["self-hosted".to_string(), "docker".to_string()],
            runner_name: None,
            workflow_name: None,
            head_branch: None,
            started_at: None,
            completed_at: None,
            created_at: None,
            check_run_url: None,
            steps: None,
        };
        (run, vec![job])
    }

    #[tokio::test]
    async fn a_deferred_read_still_records_a_provenance_refusal() {
        let profiles = profiles();
        let metrics = Mutex::new(Metrics::default());
        let inflight = Mutex::new(HashMap::new());
        let tmp = TempDir::new();
        let audit = Audit::at(tmp.path.join("audit.jsonl"));
        let rc = ReconcileCtx {
            profiles: &profiles,
            services: &NO_SERVICES,
            metrics: &metrics,
            inflight: &inflight,
            audit: &audit,
        };
        // A fork PR, queued, whose authorship read was deferred (empty Codes).
        let mut fork = run_with_job(81, "queued");
        fork.0.event = "pull_request".to_string();
        fork.0.head_repository = Some(MinimalRepo {
            full_name: "stranger/demo".to_string(),
            fork: true,
            private: false,
        });
        // A same-repo push whose read was deferred: neither refused nor minted.
        let mut own = run_with_job(82, "queued");
        own.0.event = "pull_request".to_string();
        own.0.head_repository = Some(MinimalRepo {
            full_name: "owner/demo".to_string(),
            fork: false,
            private: true,
        });
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &[fork, own],
            &Codes::new(),
            &[],
            ProvisioningMode::PerJob,
            &mut seen,
            0,
        )
        .await;
        assert!(intents.is_empty(), "a deferred read never mints");
        let m = metrics.lock().await;
        assert!(m.refused_queued.contains_key(&81), "fork PR is refused");
        assert!(m.refused_queued[&81].trust);
        assert!(!m.refused_queued.contains_key(&82), "own code just waits");
    }

    #[test]
    fn tally_counts_a_red_job_as_completed_not_faulted() {
        // The regression this guards: an ephemeral Actions runner exits 0 whether
        // the job it ran went green or red, so `job_ok` can never mean "CI passed".
        // A tally that treated it as one would report ~0 faults forever and quietly
        // mean something no counter can measure.
        let red_job = RunReport {
            job_ok: true,
            ..Default::default()
        };
        assert_eq!(tally_of(&red_job), Tally::Completed);
    }

    #[test]
    fn tally_separates_faber_faults_from_a_clean_reclaim() {
        let killed = RunReport::default();
        let timed_out = RunReport {
            timed_out: true,
            ..Default::default()
        };
        let infra = RunReport {
            infra_error: true,
            ..Default::default()
        };
        for r in [killed, timed_out, infra] {
            assert_eq!(tally_of(&r), Tally::Faulted, "{r:?}");
        }
        // Reclaim is faber standing a surplus runner down on purpose. It ran no
        // job, so it is neither a delivery nor a fault — counting it as either
        // would make a healthy daemon look like a broken one.
        let reclaimed = RunReport {
            reclaimed: true,
            ..Default::default()
        };
        assert_eq!(tally_of(&reclaimed), Tally::Neither);
    }

    /// A fan-out run: many jobs under one run id, each with its own status and the
    /// runner GitHub gave it. Mirrors the live shape that stranded claims — a wide
    /// deploy run where all but one job had finished.
    fn fanout_run(jobs: &[(u64, &str, Option<&str>)]) -> (WorkflowRun, Vec<Job>) {
        let (run, _) = run_with_job(0, "queued");
        let jobs = jobs
            .iter()
            .map(|(id, status, runner)| Job {
                id: *id,
                name: format!("job{id}"),
                status: status.to_string(),
                conclusion: None,
                labels: vec!["self-hosted".to_string(), "docker".to_string()],
                runner_name: runner.map(|r| r.to_string()),
                workflow_name: None,
                head_branch: None,
                started_at: None,
                completed_at: None,
                created_at: None,
                check_run_url: None,
                steps: None,
            })
            .collect();
        (run, jobs)
    }

    #[test]
    fn live_ids_exclude_finished_jobs_of_an_active_run() {
        // The bug this fixes: one still-queued job kept its run active, and every
        // finished sibling counted as live — so their claims were never reconciled.
        let runs = vec![fanout_run(&[
            (1, "completed", Some("faber-1-100")),
            (2, "completed", Some("faber-2-100")),
            (3, "in_progress", Some("faber-3-100")),
            (4, "queued", None),
        ])];
        let live = live_job_ids(&runs);
        assert_eq!(live, [3, 4].into_iter().collect::<HashSet<u64>>());
    }

    // --- cohort sizing (issue #13) ----------------------------------------

    #[test]
    fn cohort_holds_every_queued_class_a_runner_could_be_handed() {
        // The whole point: a runner minted for job1 registers with the profile's
        // labels, and GitHub may hand it job2 or job3 instead. All three classes
        // must be in the set the container is sized against.
        let runs = vec![fanout_run(&[
            (1, "queued", None),
            (2, "queued", None),
            (3, "queued", None),
        ])];
        let cohorts = queued_cohorts(&profiles(), "owner/demo", &runs);
        assert_eq!(
            cohorts.get(&0).map(Vec::as_slice),
            Some(
                [
                    class_key("owner/demo", 900, "job1"),
                    class_key("owner/demo", 900, "job2"),
                    class_key("owner/demo", 900, "job3"),
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn cohort_ignores_jobs_that_already_have_a_runner() {
        // Only `queued` jobs are dispatchable; an in-progress or finished one is
        // never handed out again, so sizing for it would only waste memory.
        let runs = vec![fanout_run(&[
            (1, "queued", None),
            (2, "in_progress", Some("faber-2-100")),
            (3, "completed", Some("faber-3-100")),
        ])];
        let cohorts = queued_cohorts(&profiles(), "owner/demo", &runs);
        assert_eq!(
            cohorts.get(&0).map(Vec::as_slice),
            Some([class_key("owner/demo", 900, "job1")].as_slice())
        );
    }

    #[test]
    fn cohort_excludes_jobs_this_runner_cannot_be_given() {
        // A job targeting labels the minted runner doesn't carry can never land on
        // it, so it must not inflate the cap.
        let (run, mut jobs) = fanout_run(&[(1, "queued", None), (2, "queued", None)]);
        jobs[1].labels = vec!["macOS".to_string()];
        let cohorts = queued_cohorts(&profiles(), "owner/demo", &[(run, jobs)]);
        assert_eq!(
            cohorts.get(&0).map(Vec::as_slice),
            Some([class_key("owner/demo", 900, "job1")].as_slice())
        );
    }

    /// The hole cohort sizing still had: a class that queues *after* a runner
    /// launched can be dispatched to it, and a snapshot of "queued right now"
    /// cannot contain it. Live, this killed over a hundred jobs — a build job
    /// wanting several GiB landing in the sub-GiB containers of a runner minted a
    /// couple of minutes earlier for a deploy job.
    #[test]
    fn cohort_keeps_a_class_seen_in_an_earlier_scan() {
        let mut acc = HashMap::new();
        // Scan 1: only the deploy job is queued — a runner minted here is sized
        // against this alone.
        merge_cohorts(
            &mut acc,
            queued_cohorts(
                &profiles(),
                "owner/demo",
                &[fanout_run(&[(1, "queued", None)])],
            ),
            10_000,
        );
        // Scan 2: the deploy job has started (so it is no longer queued) and a
        // heavier job has queued behind it.
        merge_cohorts(
            &mut acc,
            queued_cohorts(
                &profiles(),
                "owner/demo",
                &[fanout_run(&[
                    (1, "in_progress", Some("faber-1-100")),
                    (2, "queued", None),
                ])],
            ),
            20_000,
        );
        let mut classes: Vec<_> = acc[&0].keys().cloned().collect();
        classes.sort();
        assert_eq!(
            classes,
            vec![
                class_key("owner/demo", 900, "job1"),
                class_key("owner/demo", 900, "job2"),
            ],
            "the still-idle runner minted in scan 1 can be handed job2, so job2 \
             must size it — even though job1 is what was queued when it launched"
        );
    }

    #[test]
    fn cohort_forgets_a_class_dormant_past_the_ttl() {
        let mut acc = HashMap::new();
        merge_cohorts(
            &mut acc,
            queued_cohorts(
                &profiles(),
                "owner/demo",
                &[fanout_run(&[(1, "queued", None)])],
            ),
            10_000,
        );
        let later = 10_000 + COHORT_TTL.as_millis() as u64 + 1;
        merge_cohorts(
            &mut acc,
            queued_cohorts(
                &profiles(),
                "owner/demo",
                &[fanout_run(&[(2, "queued", None)])],
            ),
            later,
        );
        assert_eq!(
            acc[&0].keys().cloned().collect::<Vec<_>>(),
            vec![class_key("owner/demo", 900, "job2")],
            "a retired job name must stop inflating every container forever"
        );
    }

    #[test]
    fn cohort_ttl_is_measured_from_the_last_sighting_not_the_first() {
        // A class queued on every scan must never age out from under a live runner.
        let mut acc = HashMap::new();
        let fresh = || {
            queued_cohorts(
                &profiles(),
                "owner/demo",
                &[fanout_run(&[(1, "queued", None)])],
            )
        };
        merge_cohorts(&mut acc, fresh(), 10_000);
        let ttl = COHORT_TTL.as_millis() as u64;
        merge_cohorts(&mut acc, fresh(), 10_000 + ttl);
        merge_cohorts(&mut acc, HashMap::new(), 10_000 + ttl + ttl / 2);
        assert_eq!(
            acc[&0].keys().cloned().collect::<Vec<_>>(),
            vec![class_key("owner/demo", 900, "job1")],
            "re-sighting refreshes the entry"
        );
    }

    /// A restart must not reopen the gap: the very first runners minted are sized
    /// from whatever is queued in that instant, and a class only rejoins a live
    /// cohort when it next queues — a day or a week for a scheduled workflow.
    /// Observed live as a build job OOM-killed at an undersized cap on a runner
    /// minted seconds after startup.
    #[test]
    fn startup_rebuilds_the_cohort_from_replayed_history() {
        let replay = |class: &str, profile: Option<&str>, ts: u64| OutcomeReplay {
            class: class.to_string(),
            repo: "owner/demo".to_string(),
            profile: profile.map(str::to_string),
            ts_ms: ts,
            label: None,
            mem_peak_runner_mib: Some(100),
            mem_peak_dind_mib: Some(10),
            oom: Some(false),
            ..Default::default()
        };
        let now = 1_000_000_000;
        let stale = now - COHORT_TTL.as_millis() as u64 - 1;
        let out = seeded_cohorts(
            &profiles(),
            &[
                replay("owner/demo::900::heavy", Some("linux-docker"), now - 1_000),
                // No profile recorded (a line predating the field): the cohort it
                // would join is a guess, so it is left to the live scan.
                replay("owner/demo::900::unprofiled", None, now - 1_000),
                // A profile that no longer exists in the config.
                replay("owner/demo::900::retired", Some("gone"), now - 1_000),
                // Older than the TTL — the same clock a live sighting ages on.
                replay("owner/demo::900::ancient", Some("linux-docker"), stale),
            ],
            now,
        );
        assert_eq!(
            out["owner/demo"][&0].keys().cloned().collect::<Vec<_>>(),
            vec!["owner/demo::900::heavy".to_string()],
        );
    }

    #[test]
    fn startup_seeding_keeps_the_most_recent_sighting_of_a_class() {
        let replay = |ts: u64| OutcomeReplay {
            class: "owner/demo::900::job1".to_string(),
            repo: "owner/demo".to_string(),
            profile: Some("linux-docker".to_string()),
            ts_ms: ts,
            label: None,
            mem_peak_runner_mib: None,
            mem_peak_dind_mib: None,
            oom: None,
            ..Default::default()
        };
        // Out of order on purpose: the log is append-only but the newest sighting
        // is what the TTL must measure from.
        let out = seeded_cohorts(&profiles(), &[replay(50_000), replay(10_000)], 60_000);
        assert_eq!(out["owner/demo"][&0]["owner/demo::900::job1"], 50_000);
    }

    #[test]
    fn cohort_sizing_caps_a_light_runner_for_the_heavy_job_it_may_be_handed() {
        // The whole path issue #13 runs through: scan → cohort → sizing. A runner
        // minted for the light class must launch big enough for the heavy job
        // GitHub can hand it instead, while still reserving only its own footprint.
        let runs = vec![fanout_run(&[(1, "queued", None), (2, "queued", None)])];
        let cohort = queued_cohorts(&profiles(), "owner/demo", &runs)
            .remove(&0)
            .expect("the docker profile has a cohort");

        let lim = Limiter::new(
            AutoLimitConfig {
                reserve_mib: 2048,
                headroom_mib: 1024,
                band_mib: 512,
                profiling_cap_mib: 4096,
                margin: 1.5,
                escalation_frac: 0.5,
                window: 3,
                native_concurrency: 1,
                probe_image: "busybox".to_string(),
                hard_ceiling: crate::config::HardCeiling::Off,
                cpu: crate::config::CpuConfig::default(),
                reserve: crate::config::ReserveSetting::Unstated,
                priority_repos: Vec::new(),
            },
            &HashMap::new(),
        );
        let light = class_key("owner/demo", 900, "job1");
        let heavy = class_key("owner/demo", 900, "job2");
        let peaks = |runner, dind| RunReport {
            job_ok: true,
            runner_peak_mib: Some(runner),
            dind_peak_mib: Some(dind),
            ..Default::default()
        };
        lim.record_run(&light, "light", &peaks(200, 100));
        lim.record_run(&heavy, "heavy", &peaks(2000, 400));

        let sizing = lim.sizing_for(&light, &cohort, true);
        // Containers sized for the heavy peer: 2000 × 1.5 and 400 × 1.5.
        assert_eq!(sizing.limits.runner.memory_mib, 3000);
        assert_eq!(sizing.limits.dind.memory_mib, 600);
        // Ledger still debits the light class: 200 × 1.5, and dind at the floor.
        assert_eq!(sizing.reservation.mib, 556);
        assert_eq!(sizing.raised_by.as_deref(), Some("heavy"));
    }

    #[test]
    fn label_compatibility_accepts_either_containment() {
        let runner = vec!["faber".to_string(), "docker".to_string()];
        // The normal case: `runs-on` is exactly the profile's labels.
        assert!(labels_compatible(&runner, &runner));
        // A job asking for less than the runner carries: GitHub can still dispatch
        // it here, so it belongs to the cohort.
        assert!(labels_compatible(&runner, &["faber".to_string()]));
        // A job asking for more than the runner carries: faber routes it to this
        // profile (`match_profile` uses the other containment), so cover it too.
        assert!(labels_compatible(
            &runner,
            &["faber".to_string(), "docker".to_string(), "gpu".to_string()]
        ));
        // Genuinely unrelated label sets never share a runner.
        assert!(!labels_compatible(&runner, &["macOS".to_string()]));
        // A profile with no labels routes nothing (mirrors `match_profile`).
        assert!(!labels_compatible(&[], &["faber".to_string()]));
    }

    #[test]
    fn attribution_keys_on_the_job_the_runner_actually_ran() {
        // Live-observed scrambling: the runner minted for job 2 was handed job 1.
        // Learning must land on job 1's class, not job 2's.
        let mut map = HashMap::new();
        let runs = vec![fanout_run(&[(1, "completed", Some("faber-2-100"))])];
        harvest_executed(&mut map, &runs, 1_000);
        let (key, label, ran) =
            executed_class(&map, "owner/demo", "faber-2-100").expect("mapping harvested");
        assert_eq!(key, class_key("owner/demo", 900, "job1"));
        assert_eq!(label, "owner/demo · workflow · job1");
        assert_eq!(ran, 1);
    }

    #[test]
    fn attribution_is_absent_for_an_unknown_or_foreign_runner() {
        let mut map = HashMap::new();
        // A queued job has no runner; a non-faber runner is not ours to learn from.
        let runs = vec![fanout_run(&[
            (1, "queued", None),
            (2, "completed", Some("gh-hosted-7")),
        ])];
        harvest_executed(&mut map, &runs, 1_000);
        assert!(map.is_empty());
        // An unmapped runner yields no attribution, so its run is kept out of the
        // learner rather than attributed to the job it was minted for.
        assert!(executed_class(&map, "owner/demo", "faber-9-100").is_none());
    }

    /// The October 2026 miss: a runner minted for one job took an older job of its
    /// run, ran it in seconds, and exited before the next scan, so the scan-fed map
    /// had nothing and the run went down as the minted-for job's. A miss asks GitHub
    /// over the runs changed since admission, and only those.
    #[tokio::test]
    async fn attribution_asks_github_when_the_runner_beat_the_scan() {
        let (base, seen) = spawn_routed_api(|req| match req {
            r if r.contains("/actions/runs?status=completed") => (
                "200 OK",
                r#"{"workflow_runs":[
                    {"id":41,"event":"release","workflow_id":900,"name":"Release",
                     "repository":{"full_name":"owner/demo","private":true},
                     "updated_at":"2026-03-02T10:01:30Z"},
                    {"id":7,"event":"push","workflow_id":901,"name":"CI",
                     "repository":{"full_name":"owner/demo","private":true},
                     "updated_at":"2026-03-02T04:00:00Z"}]}"#,
            ),
            r if r.contains("/actions/runs?status=") => ("200 OK", r#"{"workflow_runs":[]}"#),
            r if r.contains("/actions/runs/41/jobs") => (
                "200 OK",
                r#"{"jobs":[
                    {"id":671,"name":"build","status":"completed",
                     "runner_name":"faber-904-100"},
                    {"id":904,"name":"test","status":"queued"}]}"#,
            ),
            _ => ("404 Not Found", r#"{"message":"Not Found"}"#),
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        let admitted = crate::retry::parse_rfc3339_to_ms("2026-03-02T10:00:00Z").unwrap();

        let (key, label, ran) = orch
            .executed_class("owner/demo", "faber-904-100", admitted)
            .await
            .expect("found by asking GitHub");
        assert_eq!(ran, 671);
        assert_eq!(key, class_key("owner/demo", 900, "build"));
        assert_eq!(label, "owner/demo · Release · build");
        let asked = seen.lock().unwrap().clone();
        assert!(
            !asked.iter().any(|r| r.contains("/runs/7/jobs")),
            "a run untouched since admission is not read: {asked:?}"
        );

        // The answer is kept, so asking again costs nothing.
        let before = seen.lock().unwrap().len();
        assert!(orch
            .executed_class("owner/demo", "faber-904-100", admitted)
            .await
            .is_some());
        assert_eq!(seen.lock().unwrap().len(), before);
    }

    /// A runner no job names stays unattributed after the lookup, rather than
    /// falling back to the job it was minted for.
    #[tokio::test]
    async fn attribution_stays_absent_when_github_names_no_job() {
        let (base, _seen) = spawn_routed_api(|_| ("200 OK", r#"{"workflow_runs":[]}"#)).await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        assert!(orch
            .executed_class("owner/demo", "faber-9-100", 0)
            .await
            .is_none());
    }

    #[test]
    fn attribution_map_is_bounded_by_ttl() {
        let mut map = HashMap::new();
        let runs = vec![fanout_run(&[(1, "completed", Some("faber-1-100"))])];
        harvest_executed(&mut map, &runs, 1_000);
        assert_eq!(map.len(), 1);
        // A later cycle with nothing new ages the stale mapping out.
        let ttl = EXECUTED_TTL.as_millis() as u64;
        harvest_executed(&mut map, &[], 1_000 + ttl);
        assert_eq!(map.len(), 1, "still inside the TTL");
        harvest_executed(&mut map, &[], 1_001 + ttl);
        assert!(map.is_empty(), "past the TTL");
    }

    #[tokio::test]
    async fn persisting_skip_records_once_across_cycles() {
        let profiles = profiles();
        let metrics = Mutex::new(Metrics::default());
        let inflight = Mutex::new(HashMap::new());
        // A throwaway audit target keeps these assertions off the real log. Its
        // contents aren't read back: the audit append is coupled to the recent
        // feed under the same dedup gate (so `recent` pins the audit dedup too),
        // and a read-after-async-write would race tokio's buffered file flush.
        let tmp = TempDir::new();
        let audit = Audit::at(tmp.path.join("audit.jsonl"));
        let rc = ReconcileCtx {
            profiles: &profiles,
            services: &NO_SERVICES,
            metrics: &metrics,
            inflight: &inflight,
            audit: &audit,
        };
        // An in_progress job → Skip(NotQueued), lingering across cycles.
        let runs = vec![run_with_job(42, "in_progress")];

        // Two cycles with a fresh `seen` each, pruning at the end — exactly what
        // `reconcile_once` does. The job is in `seen` both cycles, so prune keeps
        // its dedup entry and the second cycle must NOT re-record.
        for _ in 0..2 {
            let mut seen = HashSet::new();
            let intents = process_run_jobs(
                &rc,
                "owner/demo",
                &runs,
                &Codes::new(),
                &[],
                ProvisioningMode::PerJob,
                &mut seen,
                0,
            )
            .await;
            assert!(intents.is_empty(), "a skip never provisions");
            metrics.lock().await.prune(&seen);
        }

        let m = metrics.lock().await;
        assert_eq!(m.recent.len(), 1, "recorded once, not once per cycle");
    }

    #[tokio::test]
    async fn persisting_provision_counts_once_while_claim_held() {
        let profiles = profiles();
        let metrics = Mutex::new(Metrics::default());
        let inflight = Mutex::new(HashMap::new());
        let tmp = TempDir::new();
        let audit = Audit::at(tmp.path.join("audit.jsonl"));
        let rc = ReconcileCtx {
            profiles: &profiles,
            services: &NO_SERVICES,
            metrics: &metrics,
            inflight: &inflight,
            audit: &audit,
        };
        // A queued job → Provision. We never spawn/release, so the claim from
        // cycle 1 is still held in cycle 2 (as while a runner connects).
        let runs = vec![run_with_job(77, "queued")];

        let mut seen1 = HashSet::new();
        let c1 = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &[],
            ProvisioningMode::PerJob,
            &mut seen1,
            0,
        )
        .await;
        metrics.lock().await.prune(&seen1);

        let mut seen2 = HashSet::new();
        let c2 = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &[],
            ProvisioningMode::PerJob,
            &mut seen2,
            1,
        )
        .await;
        metrics.lock().await.prune(&seen2);

        assert_eq!(
            c1,
            vec![ProvisionIntent {
                // Carried through to the mint so a job's `faber.*` needs can
                // become labels on the runner (ADR-0010).
                job_labels: vec!["self-hosted".into(), "docker".into()],
                job_id: 77,
                profile_idx: 0,
                job_name: "build".to_string(),
                workflow_id: 900,
                workflow_name: None,
                run_id: 900,
                head_sha: "abc123".to_string(),
                content_ref: "abc123".to_string(),
                pr: None,
                run_created_at: None,
                attempt: 1,
            }],
            "cycle 1 claims + intends"
        );
        assert!(
            c2.is_empty(),
            "cycle 2 sees the held claim: no second intent"
        );
        assert_eq!(
            metrics.lock().await.counts.provisioned,
            1,
            "one runner, counted once — not 2-4x"
        );
    }

    #[tokio::test]
    async fn re_provision_after_claim_release_counts_again() {
        // Change 1: Provision is NOT note-gated. A runner that dies with the job
        // still queued releases its claim; the next cycle legitimately re-claims
        // and spawns a SECOND runner, which must be counted and audited again —
        // note-gating (unchanged "provision" label) would wrongly suppress it.
        // bug-002: the first re-provision stays immediate — it is within the
        // throttle grace (PROVISION_GRACE_ATTEMPTS >= 2), so backoff never delays
        // recovery from a transient runner death.
        let profiles = profiles();
        let metrics = Mutex::new(Metrics::default());
        let inflight = Mutex::new(HashMap::new());
        let tmp = TempDir::new();
        let audit = Audit::at(tmp.path.join("audit.jsonl"));
        let rc = ReconcileCtx {
            profiles: &profiles,
            services: &NO_SERVICES,
            metrics: &metrics,
            inflight: &inflight,
            audit: &audit,
        };
        let runs = vec![run_with_job(88, "queued")];

        let mut seen1 = HashSet::new();
        let c1 = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &[],
            ProvisioningMode::PerJob,
            &mut seen1,
            0,
        )
        .await;
        metrics.lock().await.prune(&seen1);
        // Simulate the runner dying without the job leaving `queued`: its claim
        // is released, as `spawn_provision` does on task completion.
        inflight.lock().await.remove(&88);

        let mut seen2 = HashSet::new();
        let c2 = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &[],
            ProvisioningMode::PerJob,
            &mut seen2,
            1,
        )
        .await;
        metrics.lock().await.prune(&seen2);

        assert_eq!(c1.len(), 1, "cycle 1 spawns a runner");
        assert_eq!(
            c2.len(),
            1,
            "cycle 2 re-spawns after the claim was released"
        );
        assert_eq!(
            metrics.lock().await.counts.provisioned,
            2,
            "each real spawn counts once"
        );
    }

    // --- bug-002: provision-retry throttle ---------------------------------
    //
    // Pure seam first (now passed in, deterministic), then an integration test
    // driving `process_run_jobs` across cycles with a threaded clock.

    #[test]
    fn provision_backoff_ms_is_exponential_and_capped() {
        // Grace attempts: no delay, so recovery stays prompt.
        assert_eq!(provision_backoff_ms(PROVISION_GRACE_ATTEMPTS - 1), 0);
        // Then doubling from the base...
        assert_eq!(
            provision_backoff_ms(PROVISION_GRACE_ATTEMPTS),
            PROVISION_BACKOFF_BASE_MS
        );
        assert_eq!(
            provision_backoff_ms(PROVISION_GRACE_ATTEMPTS + 1),
            PROVISION_BACKOFF_BASE_MS * 2
        );
        // ...never above the ceiling, even far out.
        assert_eq!(
            provision_backoff_ms(PROVISION_MAX_ATTEMPTS),
            PROVISION_BACKOFF_CEILING_MS
        );
        assert!(provision_backoff_ms(1000) <= PROVISION_BACKOFF_CEILING_MS);
    }

    // --- claim reconciliation (issue #1) -----------------------------------

    #[test]
    fn is_terminal_only_for_completed() {
        // Only a completed job will never want a runner again. Everything else may
        // still (or soon) need one, so a parked claim on it is never released.
        assert!(is_terminal("completed"));
        for s in ["queued", "in_progress", "waiting", "requested", "pending"] {
            assert!(!is_terminal(s), "{s} must not be treated as terminal");
        }
    }

    #[tokio::test]
    async fn reclaim_is_visible_to_a_late_subscriber() {
        // The parked-cancel path's correctness rests on this: a reclaim signalled
        // between a claim's insertion and its task's `subscribe()` must still be
        // seen. `send_replace` stores the flag with zero receivers, so a receiver
        // created afterwards reads it via its initial `borrow()`.
        let (tx, rx0) = watch::channel(false);
        drop(rx0); // no receivers yet — as right after the claim is inserted
        reclaim(42, &tx, "test");
        let rx = tx.subscribe();
        assert!(
            *rx.borrow(),
            "a task subscribing after the reclaim must see it"
        );
    }

    #[tokio::test]
    async fn wait_cancelled_resolves_only_on_a_true_flag() {
        // False never resolves; flipping true does. Guards the admission-wait select
        // from firing a reclaim on a still-live claim.
        let (tx, rx) = watch::channel(false);
        let mut rx2 = rx.clone();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), wait_cancelled(&mut rx2))
                .await
                .is_err(),
            "a false flag must not resolve wait_cancelled"
        );
        let _ = tx.send_replace(true);
        let mut rx3 = rx;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), wait_cancelled(&mut rx3))
                .await
                .is_ok(),
            "a true flag must resolve wait_cancelled promptly"
        );
    }

    fn job_with_status(id: u64, status: &str) -> Job {
        Job {
            id,
            name: "j".to_string(),
            status: status.to_string(),
            conclusion: None,
            labels: vec![],
            runner_name: None,
            workflow_name: None,
            head_branch: None,
            started_at: None,
            completed_at: None,
            created_at: None,
            check_run_url: None,
            steps: None,
        }
    }

    fn runner(name: &str, busy: bool, status: &str) -> Runner {
        Runner {
            id: 0,
            name: name.to_string(),
            status: status.to_string(),
            busy,
            labels: Vec::new(),
        }
    }

    /// A runner carrying `labels` — the demand-mode supply tests need the label set
    /// so `match_profile` can attribute the runner to a profile.
    fn runner_labeled(name: &str, busy: bool, status: &str, labels: &[&str]) -> Runner {
        Runner {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            ..runner(name, busy, status)
        }
    }

    #[test]
    fn parked_verdict_reclaims_gone_and_completed_keeps_active() {
        use ClaimVerdict::*;
        // 404 (job/run deleted) and completed ⇒ the job will never want a runner.
        assert_eq!(parked_verdict(None), Reclaim("job gone (404)"));
        assert_eq!(
            parked_verdict(Some(&job_with_status(1, "completed"))),
            Reclaim("job completed")
        );
        // Anything still active is kept — never released unconfirmed (this is what
        // stops a still-queued, later-page job from being double-provisioned).
        for s in ["queued", "in_progress", "waiting", "requested", "pending"] {
            assert_eq!(
                parked_verdict(Some(&job_with_status(1, s))),
                Keep,
                "parked job in status {s} must be kept"
            );
        }
    }

    #[test]
    fn a_job_counts_as_started_once_github_hands_it_to_a_runner() {
        assert!(!job_started(&job_with_status(1, "queued")));
        assert!(!job_started(&job_with_status(1, "waiting")));
        assert!(job_started(&job_with_status(1, "in_progress")));
        assert!(job_started(&job_with_status(1, "completed")));
    }

    #[test]
    fn a_job_counts_as_started_here_only_on_a_runner_this_host_minted() {
        let local: HashSet<String> = ["faber-1-100".to_string()].into();
        let on = |status: &str, runner: Option<&str>| {
            let mut j = job_with_status(1, status);
            j.runner_name = runner.map(str::to_string);
            j
        };
        assert!(job_started_here(
            &on("in_progress", Some("faber-1-100")),
            &local
        ));
        assert!(!job_started_here(
            &on("in_progress", Some("faber-9-900")),
            &local
        ));
        assert!(!job_started_here(
            &on("completed", Some("faber-9-900")),
            &local
        ));
        assert!(!job_started_here(&on("in_progress", None), &local));
        assert!(!job_started_here(
            &on("queued", Some("faber-1-100")),
            &local
        ));
    }

    #[test]
    fn only_a_started_unfrozen_admitted_job_is_running() {
        use JobPhase::*;
        // A parked claim is queued whatever else is set.
        assert_eq!(JobPhase::of(false, false, false), Queued);
        assert_eq!(JobPhase::of(false, true, true), Queued);
        // Admitted is not running until GitHub says the job started …
        assert_eq!(JobPhase::of(true, false, false), Starting);
        assert_eq!(JobPhase::of(true, true, false), Running);
        // … and a paused one is frozen, started or not.
        assert_eq!(JobPhase::of(true, true, true), Frozen);
        assert_eq!(JobPhase::of(true, false, true), Frozen);
        assert_eq!(serde_json::to_value(Starting).unwrap(), "starting");
    }

    #[test]
    fn current_step_picks_the_in_progress_step() {
        let step = |name: &str, number: i64, status: &str| Step {
            name: name.to_string(),
            status: status.to_string(),
            number,
            conclusion: None,
            started_at: None,
            completed_at: None,
        };
        // The single in_progress step is "what it's doing now", as index/total.
        let steps = vec![
            step("Set up job", 1, "completed"),
            step("Build", 2, "completed"),
            step("Test", 3, "in_progress"),
            step("Deploy", 4, "queued"),
        ];
        let cur = current_step(&steps).expect("an in-progress step");
        assert_eq!(cur.name, "Test");
        assert_eq!((cur.index, cur.total), (3, 4));

        // Nothing running yet (all queued) or everything already done ⇒ None, so
        // the UI shows a phase word rather than a stale step.
        assert!(current_step(&[step("Set up job", 1, "queued")]).is_none());
        assert!(current_step(&[step("Build", 1, "completed")]).is_none());
        assert!(current_step(&[]).is_none());
    }

    #[test]
    fn runner_life_is_idle_only_when_present_idle_and_online() {
        let ours = "faber-42-1700";
        // Present, not busy, online ⇒ idle (reclaimable).
        assert_eq!(
            runner_life(&[runner(ours, false, "online")], ours, false),
            RunnerLife::Idle
        );
        // Busy ⇒ NOT idle — the reassignment guard: a runner minted for a now-done
        // job but busy on a sibling must never be reclaimed (criterion 2).
        assert_eq!(
            runner_life(&[runner(ours, true, "online")], ours, false),
            RunnerLife::Busy
        );
        // Offline (registering / orphaned) ⇒ not idle.
        assert_eq!(
            runner_life(&[runner(ours, false, "offline")], ours, false),
            RunnerLife::Missing
        );
        // Our runner busy while an unrelated runner is idle ⇒ still busy.
        assert_eq!(
            runner_life(
                &[
                    runner(ours, true, "online"),
                    runner("faber-7-1700", false, "online"),
                ],
                ours,
                false
            ),
            RunnerLife::Busy
        );
    }

    #[test]
    fn runner_life_judges_its_own_runner_never_a_peers_twin() {
        // Two instances sharing a pool routinely mint for the same queued job within
        // milliseconds of each other: same `faber-42-` prefix, different timestamp.
        let ours = "faber-42-1700";
        let peers = "faber-42-1703";
        // A peer's busy twin is not evidence that *our* runner is working — under a
        // prefix match it kept an idle runner (and its reservation) alive.
        assert_eq!(
            runner_life(
                &[runner(ours, false, "online"), runner(peers, true, "online")],
                ours,
                false
            ),
            RunnerLife::Idle
        );
        // Nor is a peer's idle twin evidence that ours is idle: ours being absent
        // from the list is `Missing`, which never reclaims an active job's claim
        // while its container is up.
        assert_eq!(
            runner_life(&[runner(peers, false, "online")], ours, false),
            RunnerLife::Missing
        );
    }

    #[test]
    fn a_runner_the_scan_saw_running_a_live_job_is_busy_whatever_the_list_says() {
        let ours = "faber-42-1700";
        // The list's `busy` flag has been seen to trail a runner minutes into a
        // sibling's job. The job record naming the runner is a second witness, and
        // either one is enough.
        for listed in [
            vec![runner(ours, false, "online")],
            vec![runner(ours, false, "offline")],
            vec![],
        ] {
            assert_eq!(
                runner_life(&listed, ours, true),
                RunnerLife::Busy,
                "{listed:?}: a runner running a live job must read busy"
            );
        }
    }

    #[test]
    fn executing_live_job_needs_a_still_live_job_that_names_the_runner() {
        let ours = "faber-42-1700";
        let executed: HashMap<String, ExecutedJob> = [(
            ours.to_string(),
            ExecutedJob {
                job_id: 7,
                workflow_id: 1,
                workflow_name: None,
                job_name: "test".to_string(),
                seen_ms: 0,
            },
        )]
        .into();
        assert!(executing_live_job(&executed, ours, &[7].into()));
        // The job it ran has finished: a JIT runner runs exactly one, so it is done.
        assert!(!executing_live_job(&executed, ours, &HashSet::new()));
        // Another runner, including a peer's twin, is not ours.
        assert!(!executing_live_job(&executed, "faber-42-1703", &[7].into()));
    }

    #[test]
    fn admitted_verdict_reclaims_only_after_sustained_idle() {
        use ClaimVerdict::*;
        let grace = 120_000u64;
        let idle = |since, now| {
            admitted_verdict(
                JobLife::Active,
                RunnerLife::Idle,
                ContainerLife::Unknown,
                since,
                now,
                grace,
            )
        };
        // Busy ⇒ keep, and the idle window is cleared.
        assert_eq!(
            admitted_verdict(
                JobLife::Active,
                RunnerLife::Busy,
                ContainerLife::Unknown,
                Some(50_000),
                100_000,
                grace
            ),
            (Keep, None)
        );
        // First seen idle ⇒ keep, arm the timer at `now`.
        assert_eq!(idle(None, 1_000), (Keep, Some(1_000)));
        // Idle but within grace ⇒ keep, timer preserved.
        assert_eq!(idle(Some(1_000), 1_000 + grace - 1), (Keep, Some(1_000)));
        // Idle exactly at the grace boundary ⇒ request a reclaim, and restart the
        // window from the request.
        assert_eq!(
            idle(Some(1_000), 1_000 + grace),
            (Reclaim("runner idle past grace"), Some(1_000 + grace))
        );
        // Idle well past grace ⇒ likewise.
        assert_eq!(
            idle(Some(1_000), 1_000 + grace + 5),
            (Reclaim("runner idle past grace"), Some(1_000 + grace + 5))
        );
    }

    #[test]
    fn a_refused_reclaim_is_asked_again_only_after_another_full_grace() {
        use ClaimVerdict::*;
        let grace = 120_000u64;
        // The runner's owner may refuse a request (ADR-0017) — the runner was busy
        // after all. Asking again every cycle would spend a process-table read and a
        // log line per cycle on a runner that is plainly working; so the request
        // restarts the window, and the next one needs a whole grace of its own.
        let idle = |since, now| {
            admitted_verdict(
                JobLife::Terminal,
                RunnerLife::Idle,
                ContainerLife::Unknown,
                since,
                now,
                grace,
            )
        };
        let (verdict, window) = idle(Some(0), grace);
        assert!(matches!(verdict, Reclaim(_)));
        assert_eq!(window, Some(grace));
        // Still judged idle a cycle later: not asked again yet.
        assert_eq!(idle(window, grace + 40_000), (Keep, Some(grace)));
        assert_eq!(idle(window, 2 * grace - 1), (Keep, Some(grace)));
        // A full grace after the first request: asked again.
        assert!(matches!(idle(window, 2 * grace).0, Reclaim(_)));
    }

    #[test]
    fn admitted_verdict_releases_a_claim_whose_job_is_finished() {
        use ClaimVerdict::*;
        let grace = 120_000u64;
        // Issue #16: the job GitHub already finished, on a runner that has since
        // deregistered — the shape that held a memory reservation forever, because
        // an absent runner read as "not idle" and so never reclaimed.
        for (runner, why) in [
            (RunnerLife::Idle, "job finished, runner looks idle"),
            (RunnerLife::Missing, "job finished, runner not registered"),
        ] {
            assert_eq!(
                admitted_verdict(
                    JobLife::Terminal,
                    runner,
                    ContainerLife::Unknown,
                    None,
                    1_000,
                    grace
                ),
                (Keep, Some(1_000)),
                "{runner:?}: the first sighting only arms the window"
            );
            assert_eq!(
                admitted_verdict(
                    JobLife::Terminal,
                    runner,
                    ContainerLife::Unknown,
                    Some(1_000),
                    1_000 + grace,
                    grace
                ),
                (Reclaim(why), Some(1_000 + grace)),
                "{runner:?}: a finished job must release its claim past the grace"
            );
        }
    }

    #[test]
    fn admitted_verdict_never_reclaims_a_busy_stolen_runner() {
        use ClaimVerdict::*;
        let grace = 120_000u64;
        // A JIT runner is not job-pinned (#13): the runner minted for this claim may
        // be executing a *sibling's* job. Whatever our own job is doing — still
        // queued, running, or long finished — reclaiming would `docker kill` live
        // work. Busy vetoes every job state, and clears the window.
        for job in [JobLife::Active, JobLife::Terminal, JobLife::Unknown] {
            for container in [
                ContainerLife::Present,
                ContainerLife::Gone,
                ContainerLife::Unknown,
            ] {
                assert_eq!(
                    admitted_verdict(
                        job,
                        RunnerLife::Busy,
                        container,
                        Some(1_000),
                        1_000 + grace * 10,
                        grace
                    ),
                    (Keep, None),
                    "{job:?}/{container:?}: a busy runner must never be reclaimed"
                );
            }
        }
    }

    #[test]
    fn admitted_verdict_keeps_everything_when_a_signal_is_unreadable() {
        use ClaimVerdict::*;
        let grace = 120_000u64;
        // The reclaim-storm guard: a GitHub blip must neither reclaim nor let the
        // window age. Both are checked well past the grace with the timer armed —
        // the exact state in which a wrong answer reclaims immediately.
        for runner in [
            RunnerLife::Busy,
            RunnerLife::Idle,
            RunnerLife::Missing,
            RunnerLife::Unknown,
        ] {
            assert_eq!(
                admitted_verdict(
                    JobLife::Unknown,
                    runner,
                    ContainerLife::Gone,
                    Some(1_000),
                    1_000 + grace * 10,
                    grace
                ),
                (Keep, None),
                "{runner:?}: an unreadable job must keep the claim and reset the window"
            );
        }
        for job in [JobLife::Active, JobLife::Terminal, JobLife::Unknown] {
            assert_eq!(
                admitted_verdict(
                    job,
                    RunnerLife::Unknown,
                    ContainerLife::Gone,
                    Some(1_000),
                    1_000 + grace * 10,
                    grace
                ),
                (Keep, None),
                "{job:?}: an unreadable runner list must keep the claim"
            );
        }
    }

    #[test]
    fn admitted_verdict_waits_out_a_runner_that_may_still_be_starting() {
        use ClaimVerdict::*;
        let grace = 120_000u64;
        let unregistered = |container, since, now| {
            admitted_verdict(
                JobLife::Active,
                RunnerLife::Missing,
                container,
                since,
                now,
                grace,
            )
        };
        // No registration yet, but a container is up (or Docker can't be read): the
        // runner may simply still be pulling its image. Reclaiming here is exactly
        // the re-provisioning storm — keep, and don't age the window.
        for container in [ContainerLife::Present, ContainerLife::Unknown] {
            assert_eq!(
                unregistered(container, Some(1_000), 1_000 + grace * 10),
                (Keep, None),
                "{container:?}: a runner that may still be starting must be kept"
            );
        }
        // Docker confirms there is no container: nothing is ever going to register,
        // so the claim is dead weight. Still graced, never immediate.
        assert_eq!(
            unregistered(ContainerLife::Gone, None, 1_000),
            (Keep, Some(1_000))
        );
        assert_eq!(
            unregistered(ContainerLife::Gone, Some(1_000), 1_000 + grace),
            (
                Reclaim("no runner container and no registration"),
                Some(1_000 + grace)
            )
        );
    }

    #[test]
    fn runner_life_reads_busy_idle_and_missing() {
        let ours = "faber-42-1700";
        // Busy wins over everything — the not-job-pinned veto.
        assert_eq!(
            runner_life(&[runner(ours, true, "online")], ours, false),
            RunnerLife::Busy
        );
        assert_eq!(
            runner_life(&[runner(ours, false, "online")], ours, false),
            RunnerLife::Idle
        );
        // Offline (registering or dead) and absent are both `Missing`: neither is
        // evidence on its own, which is why the container listing breaks the tie.
        assert_eq!(
            runner_life(&[runner(ours, false, "offline")], ours, false),
            RunnerLife::Missing
        );
        assert_eq!(
            runner_life(&[runner("faber-99-1700", false, "online")], ours, false),
            RunnerLife::Missing
        );
        assert_eq!(runner_life(&[], ours, false), RunnerLife::Missing);
    }

    /// The swap that killed runners mid-job (ADR-0017), end to end through the real
    /// decision and the real kill site.
    ///
    /// Two jobs queue together. GitHub hands job 2 to runner A (minted for job 1)
    /// and job 1 to runner B (minted for job 2); job 1 finishes on B and B exits.
    /// Runner A's claim is now for a finished job, and GitHub's runner list goes
    /// stale — A reads online and not busy for minutes, as it did live — while A
    /// works through job 2. Every cycle is replayed against the claim, and every
    /// reclaim request is delivered to A's watcher. A must still be running, and
    /// never have been killed, until job 2 itself finishes.
    #[tokio::test]
    async fn a_runner_serving_a_swapped_job_is_not_killed_until_that_job_ends() {
        use crate::docker::{
            await_end, reclaim_clearance, Clearance, RunEnd, RunnerActivity, Withdrawal,
        };
        use std::os::unix::process::ExitStatusExt;
        use std::sync::atomic::AtomicUsize;

        const JOB_1: u64 = 101;
        const JOB_2: u64 = 102;
        let runner_a = "faber-101-1000"; // minted for job 1, running job 2
        let runner_b = "faber-102-1000"; // minted for job 2, ran job 1
        let grace = IDLE_RECLAIM_GRACE.as_millis() as u64;
        let cycle = 40_000u64;

        // Runner A's container, as its watcher sees it: job 2 runs until finished.
        let job_2_running = Arc::new(AtomicBool::new(true));
        let withdrawals = Arc::new(AtomicUsize::new(0));
        let (finish_job_2, job_2_exit) = tokio::sync::oneshot::channel::<()>();
        let (cancel, mut requests) = watch::channel(false);
        let (answer_tx, mut answers) = tokio::sync::mpsc::unbounded_channel::<Clearance>();
        let watcher = tokio::spawn({
            let job_2_running = job_2_running.clone();
            let withdrawals = withdrawals.clone();
            async move {
                let run = async move {
                    let _ = job_2_exit.await;
                    Ok::<_, std::io::Error>(std::process::ExitStatus::from_raw(0))
                };
                await_end(run, Duration::ZERO, &mut requests, || {
                    let job_2_running = job_2_running.clone();
                    let withdrawals = withdrawals.clone();
                    let answer_tx = answer_tx.clone();
                    async move {
                        let working = job_2_running.clone();
                        let clearance = reclaim_clearance(
                            move || {
                                let working = working.clone();
                                async move {
                                    if working.load(Ordering::SeqCst) {
                                        RunnerActivity::Executing
                                    } else {
                                        RunnerActivity::Idle
                                    }
                                }
                            },
                            || async move {
                                withdrawals.fetch_add(1, Ordering::SeqCst);
                                if job_2_running.load(Ordering::SeqCst) {
                                    Withdrawal::JobAssigned
                                } else {
                                    Withdrawal::Withdrawn
                                }
                            },
                        )
                        .await;
                        let _ = answer_tx.send(clearance.clone());
                        clearance
                    }
                })
                .await
            }
        });

        // The job GitHub records as running on runner A, harvested from the scan.
        let executed: HashMap<String, ExecutedJob> = [(
            runner_a.to_string(),
            ExecutedJob {
                job_id: JOB_2,
                workflow_id: 1,
                workflow_name: None,
                job_name: "job 2".to_string(),
                seen_ms: 0,
            },
        )]
        .into();

        let mut idle_since = None;
        let mut asked = 0;
        for n in 0..16u64 {
            let now = n * cycle;
            // Cycle 0: both jobs running, each on the other's runner. From cycle 1
            // job 1 is finished and B is gone.
            let job_1 = if n == 0 {
                JobLife::Active
            } else {
                JobLife::Terminal
            };
            let listed = match n {
                0 => vec![
                    runner(runner_a, true, "online"),
                    runner(runner_b, true, "online"),
                ],
                1 => vec![runner(runner_a, true, "online")],
                // From cycle 2 the list is stale: A reads online and idle.
                _ => vec![runner(runner_a, false, "online")],
            };
            // Through cycle 6 the scan still shows job 2 live on A — long enough that
            // the stale list alone would have asked for a reclaim by cycle 5. After
            // that the scan loses it too (a run past the scan's single page), and A
            // has only its own watcher to defend it.
            let live: HashSet<u64> = match n {
                0 => [JOB_1, JOB_2].into(),
                1..=6 => [JOB_2].into(),
                _ => HashSet::new(),
            };
            let executing = executing_live_job(&executed, runner_a, &live);
            let life = runner_life(&listed, runner_a, executing);
            let (verdict, window) =
                admitted_verdict(job_1, life, ContainerLife::Unknown, idle_since, now, grace);
            idle_since = window;
            if n <= 6 {
                assert_eq!(
                    verdict,
                    ClaimVerdict::Keep,
                    "cycle {n}: the scan shows A running job 2, so no reclaim may be asked"
                );
            }
            if let ClaimVerdict::Reclaim(why) = verdict {
                asked += 1;
                reclaim(JOB_1, &cancel, why);
                let answer = answers
                    .recv()
                    .await
                    .expect("A's watcher answers every request");
                assert!(
                    matches!(answer, Clearance::Refused(_)),
                    "cycle {n}: a runner mid-job must refuse a reclaim, got {answer:?}"
                );
            }
            tokio::task::yield_now().await;
            assert!(
                !watcher.is_finished(),
                "cycle {n}: runner A stopped while job 2 was still running on it"
            );
        }
        assert!(
            asked >= 2,
            "the stale view must have provoked repeated requests, or this proves nothing \
             (asked {asked})"
        );
        assert_eq!(
            withdrawals.load(Ordering::SeqCst),
            0,
            "GitHub is never even asked to withdraw a runner that has a job running in it"
        );

        // Job 2 finishes: the run ends on its own, as a run.
        job_2_running.store(false, Ordering::SeqCst);
        finish_job_2.send(()).expect("watcher still waiting");
        let end = watcher.await.expect("watcher task");
        assert!(
            matches!(end, RunEnd::Ran(Ok(_))),
            "runner A must end because job 2 did, got {end:?}"
        );
    }

    #[test]
    fn a_frozen_runners_job_is_over_only_when_its_held_job_is_terminal_and_it_is_not_working() {
        use JobLife::{Active, Terminal, Unknown};
        use RunnerLife::{Busy, Idle, Missing};
        // The usual case (#111): its own job, failed by GitHub ~10 minutes into the
        // freeze, and a runner GitHub no longer shows working.
        assert!(frozen_job_over(Terminal, None, Missing));
        assert!(frozen_job_over(Terminal, None, Idle));
        // Its runner took a sibling's job: that one decides, either way round.
        assert!(frozen_job_over(Active, Some(Terminal), Missing));
        assert!(!frozen_job_over(Terminal, Some(Active), Missing));
        // A runner still seen working, or any unreadable signal, keeps it frozen.
        assert!(!frozen_job_over(Terminal, None, Busy));
        assert!(!frozen_job_over(Terminal, None, RunnerLife::Unknown));
        assert!(!frozen_job_over(Unknown, None, Missing));
        assert!(!frozen_job_over(Terminal, Some(Unknown), Missing));
        // Still running on GitHub: nothing to let go of.
        assert!(!frozen_job_over(Active, None, Missing));
    }

    #[test]
    fn job_life_matches_the_parked_reading_of_the_same_fetch() {
        // 404 and completed are terminal; everything else may still want a runner —
        // the same split `parked_verdict` makes, so the two phases can't disagree.
        assert_eq!(job_life(None), JobLife::Terminal);
        assert_eq!(
            job_life(Some(&job_with_status(1, "completed"))),
            JobLife::Terminal
        );
        for s in ["queued", "in_progress", "waiting", "requested", "pending"] {
            assert_eq!(
                job_life(Some(&job_with_status(1, s))),
                JobLife::Active,
                "a {s} job is still active"
            );
        }
    }

    #[test]
    fn container_job_id_parses_only_faber_runner_containers() {
        assert_eq!(container_job_id("faber-runner-42-1700"), Some(42));
        // dind, network and volume names share the prefix but not the kind.
        assert_eq!(container_job_id("faber-dind-42-1700"), None);
        assert_eq!(container_job_id("faber-net-42-1700"), None);
        // A foreign container that merely contains "faber-" is never claimed.
        assert_eq!(container_job_id("my-faber-runner-42-1700"), None);
        assert_eq!(container_job_id("faber-runner-abc-1700"), None);
    }

    /// #105: a container the listing left out would read as gone, and an active
    /// job with no registration and no container is reclaimed. So one name faber
    /// cannot read makes the whole listing "can't tell", which keeps every claim.
    #[test]
    fn a_live_runner_listing_faber_cannot_read_whole_cannot_tell() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            runner_container_job_ids(names(&[
                "faber-runner-42-1700",
                "myproj-faber-runner-1",
                "faber-runner-43-1800"
            ])),
            Some(HashSet::from([42, 43]))
        );
        assert_eq!(
            runner_container_job_ids(names(&["faber-runner-42-1700", "faber-runner-43-1800;id"])),
            None
        );
    }

    // --- ADR-0032 §2: a frozen job is still a running job --------------------

    #[test]
    fn the_live_runner_listing_counts_a_frozen_runner_as_running() {
        // The reconciler's container check and the drain both read this listing. A
        // runner the critical floor paused has the status `paused`; left out, it
        // would read as "no runner container", be reclaimed as dead, and be
        // drained past.
        let args = live_runner_ps_args();
        for status in ["status=running", "status=paused"] {
            assert!(args.iter().any(|a| a == status), "{args:?}");
        }
    }

    #[test]
    fn the_stale_runner_sweep_leaves_a_frozen_runner_alone() {
        // Frozen, a runner stops heartbeating and reads offline, soon past the
        // stale TTL. Deleting its registration would cost its job when it thaws.
        let now: u64 = 1_800_000_000_000;
        let minted = now - STALE_RUNNER_TTL.as_millis() as u64 - 60_000;
        let frozen_name = format!("faber-7-{minted}");
        let runners = [
            Runner {
                id: 1,
                ..runner(&frozen_name, false, "offline")
            },
            Runner {
                id: 2,
                ..runner(&format!("faber-8-{minted}"), false, "offline")
            },
        ];
        assert_eq!(
            stale_runner_ids(&runners, now),
            vec![1, 2],
            "both read dead"
        );
        let frozen: HashSet<String> = [frozen_name].into_iter().collect();
        assert_eq!(
            stale_runner_ids(&sweepable_runners(&runners, &frozen), now),
            vec![2]
        );
    }

    #[tokio::test]
    async fn the_critical_floor_weighs_admitted_docker_claims_only() {
        let (orch, _tmp) = test_orch();
        let claim = |runner: Option<&str>, admitted: bool, containerized: bool| {
            let claim = Claim {
                containerized,
                ..drain_claim(watch::channel(false).0, admitted)
            };
            *claim.runner_name.lock().unwrap() = runner.map(str::to_string);
            claim
        };
        {
            let mut map = orch.inflight.lock().await;
            map.insert(1, claim(Some("faber-1-100"), true, true));
            // Parked: no runner yet, so nothing to freeze.
            map.insert(2, claim(None, false, true));
            // A macOS VM's: no container to pause.
            map.insert(3, claim(Some("faber-3-300"), true, false));
        }
        let weighed = orch.admitted_docker_claims().await;
        assert_eq!(
            weighed
                .iter()
                .map(|c| (c.job_id, c.runner.as_str()))
                .collect::<Vec<_>>(),
            [(1, "faber-1-100")]
        );
    }

    #[tokio::test]
    async fn a_frozen_claim_is_its_own_state_in_the_state_view() {
        // `/api/state` always carries `frozen`: `null` for a job that is not frozen,
        // since-when and why for one the critical floor froze (ADR-0032 §2).
        let (orch, _tmp) = test_orch();
        let claim = drain_claim(watch::channel(false).0, true);
        *claim.runner_name.lock().unwrap() = Some("faber-7-1000".to_string());
        let frozen = claim.frozen.clone();
        orch.inflight.lock().await.insert(7, claim);
        let json = serde_json::to_value(orch.view().await).unwrap();
        assert!(json["inflight"][0]
            .as_object()
            .unwrap()
            .contains_key("frozen"));
        assert_eq!(json["inflight"][0]["frozen"], serde_json::Value::Null);
        assert!(orch.frozen_runners().await.is_empty());

        *frozen.lock().unwrap() = Some(crate::freeze::FrozenView {
            since_ms: 1_790_000_000_000,
            reason: crate::freeze::Reason::Disk,
            detail: "frozen at 2.8 GiB free".to_string(),
            thaw_failure: Some(crate::freeze::ThawFailure {
                at_ms: 1_790_000_060_000,
                error: "unpausing the runner container r7: timed out".to_string(),
            }),
        });
        let json = serde_json::to_value(orch.view().await).unwrap();
        assert_eq!(
            json["inflight"][0]["frozen"],
            serde_json::json!({
                "since_ms": 1_790_000_000_000u64,
                "reason": "disk",
                "detail": "frozen at 2.8 GiB free",
                // A job that cannot be unpaused says so, rather than reading as one
                // waiting for room (#111).
                "thaw_failure": {
                    "at_ms": 1_790_000_060_000u64,
                    "error": "unpausing the runner container r7: timed out",
                },
            })
        );
        assert_eq!(json["inflight"][0]["admitted"], true, "still admitted");
        assert_eq!(
            orch.frozen_runners().await,
            ["faber-7-1000".to_string()]
                .into_iter()
                .collect::<HashSet<_>>()
        );
    }

    #[tokio::test]
    async fn a_freeze_and_its_thaw_are_audited() {
        let (orch, tmp) = test_orch();
        for (action, cause) in [
            (
                "frozen",
                "2.8 GiB free on the Docker volume, below the 3 GiB critical floor",
            ),
            (
                "thawed",
                "6.5 GiB free on the Docker volume, above the 6 GiB the critical floor thaws at",
            ),
        ] {
            orch.record_freeze(
                "owner/demo",
                7,
                "faber-7-1000",
                action,
                "disk",
                cause.to_string(),
            )
            .await;
        }
        let lines = audit_lines(&tmp);
        assert_eq!(
            lines
                .iter()
                .map(|l| (l["kind"].as_str(), l["action"].as_str()))
                .collect::<Vec<_>>(),
            [
                (Some("freeze"), Some("frozen")),
                (Some("freeze"), Some("thawed"))
            ]
        );
        assert_eq!(lines[0]["runner"], "faber-7-1000");
        assert_eq!(lines[0]["job_id"], 7);
        assert_eq!(lines[0]["reason"], "disk");
    }

    #[test]
    fn provision_throttle_grace_is_prompt_then_backs_off() {
        let mut m = Metrics::default();
        let job = 9;
        // The first PROVISION_GRACE_ATTEMPTS provisions are ready even back-to-
        // back at the same instant.
        for i in 0..PROVISION_GRACE_ATTEMPTS {
            assert_eq!(
                m.provision_gate(job, 0),
                ProvisionGate::Ready,
                "grace attempt {i} is prompt"
            );
            m.note_provision_attempt(job, 0);
        }
        // Now backoff engages: throttled within the window, ready once past it.
        assert_eq!(m.provision_gate(job, 0), ProvisionGate::BackingOff);
        assert_eq!(
            m.provision_gate(job, PROVISION_BACKOFF_BASE_MS - 1),
            ProvisionGate::BackingOff
        );
        assert_eq!(
            m.provision_gate(job, PROVISION_BACKOFF_BASE_MS),
            ProvisionGate::Ready
        );
    }

    #[test]
    fn provision_throttle_slow_retries_past_cap() {
        // bug-002 defect B: past the fast-attempt cap the gate must NOT strand the
        // job. It keeps clearing at the ceiling cadence (self-heal), warning once.
        let mut m = Metrics::default();
        let job = 5;
        let mut now = 0u64;
        // Drive attempts to the cap, always stepping past the (growing) window.
        for _ in 0..PROVISION_MAX_ATTEMPTS {
            assert_eq!(m.provision_gate(job, now), ProvisionGate::Ready);
            m.note_provision_attempt(job, now);
            now += PROVISION_BACKOFF_CEILING_MS;
        }
        // Cap spent. `now` sits exactly at the re-armed ceiling window, so just
        // inside it the job still backs off (slow cadence, not every cycle)...
        assert_eq!(m.provision_gate(job, now - 1), ProvisionGate::BackingOff);
        // ...and at the ceiling boundary it self-heals — provisions again, warning
        // on exactly this first crossover.
        assert_eq!(
            m.provision_gate(job, now),
            ProvisionGate::SlowRetry { warn: true }
        );
        m.note_provision_attempt(job, now);
        // The next ceiling window behaves the same: backs off inside it...
        now += PROVISION_BACKOFF_CEILING_MS;
        assert_eq!(m.provision_gate(job, now - 1), ProvisionGate::BackingOff);
        // ...then slow-retries again, but never warns a second time. The job is
        // never permanently blocked — it is served once per ceiling interval for as
        // long as it stays queued (and stops only when it finishes and is pruned).
        assert_eq!(
            m.provision_gate(job, now),
            ProvisionGate::SlowRetry { warn: false }
        );
    }

    #[test]
    fn single_miss_preserves_provision_throttle() {
        // bug-002 defect A: a job dropped from `seen` for ONE cycle (a transient
        // per-repo fetch error, or GitHub briefly returning a run with zero jobs)
        // must not reset its storm counter — else the fast-attempt cap never holds.
        let mut m = Metrics::default();
        let job = 7;
        for _ in 0..PROVISION_MAX_ATTEMPTS {
            m.note_provision_attempt(job, 0);
        }
        assert_eq!(m.jobs[&job].provision_attempts, PROVISION_MAX_ATTEMPTS);

        // One unobserved cycle: kept (within the miss grace), counter intact.
        m.prune(&HashSet::new());
        assert!(
            m.jobs.contains_key(&job),
            "a single miss must not drop the job"
        );
        assert_eq!(
            m.jobs[&job].provision_attempts, PROVISION_MAX_ATTEMPTS,
            "a single miss must not reset the storm counter"
        );
        assert_eq!(m.jobs[&job].misses, 1);

        // Re-observing it resets the miss counter, so a job that keeps being seen
        // survives indefinitely with its throttle state intact.
        let seen: HashSet<u64> = [job].into_iter().collect();
        m.prune(&seen);
        assert_eq!(m.jobs[&job].misses, 0);
        assert_eq!(m.jobs[&job].provision_attempts, PROVISION_MAX_ATTEMPTS);
    }

    #[test]
    fn stuck_job_state_pruned_after_miss_grace() {
        // Requirement 2: the map still stays bounded — a job genuinely gone is
        // forgotten a few cycles later, even with throttle state accrued.
        let mut m = Metrics::default();
        let job = 3;
        for _ in 0..PROVISION_MAX_ATTEMPTS {
            m.note_provision_attempt(job, 0);
        }
        // Survives exactly PRUNE_MISS_GRACE consecutive unobserved cycles...
        for cycle in 1..=PRUNE_MISS_GRACE {
            m.prune(&HashSet::new());
            assert!(m.jobs.contains_key(&job), "still tracked at miss {cycle}");
            assert_eq!(m.jobs[&job].misses, cycle);
        }
        // ...then the next miss drops it (misses now exceed the grace).
        m.prune(&HashSet::new());
        assert!(m.jobs.is_empty(), "dropped once misses exceed the grace");
        // A fresh entry provisions promptly again, from zero.
        assert_eq!(m.provision_gate(job, 0), ProvisionGate::Ready);
    }

    /// Run one reconcile cycle over `runs` at `now`, then release the job's
    /// inflight claim (its runner "died" without consuming the job) and prune
    /// with the job still seen — the stuck-`queued` loop `reconcile_once` drives.
    /// Returns the number of `ProvisionIntent`s produced (0 or 1 here).
    async fn drive_stuck_cycle(
        rc: &ReconcileCtx<'_>,
        runs: &[(WorkflowRun, Vec<Job>)],
        job_id: u64,
        now: u64,
    ) -> usize {
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            rc,
            "owner/demo",
            runs,
            &Codes::new(),
            &[],
            ProvisioningMode::PerJob,
            &mut seen,
            now,
        )
        .await;
        rc.inflight.lock().await.remove(&job_id);
        rc.metrics.lock().await.prune(&seen);
        intents.len()
    }

    #[tokio::test]
    async fn stuck_job_slow_retries_past_cap_and_self_heals() {
        let profiles = profiles();
        let metrics = Mutex::new(Metrics::default());
        let inflight = Mutex::new(HashMap::new());
        let tmp = TempDir::new();
        let audit = Audit::at(tmp.path.join("audit.jsonl"));
        let rc = ReconcileCtx {
            profiles: &profiles,
            services: &NO_SERVICES,
            metrics: &metrics,
            inflight: &inflight,
            audit: &audit,
        };
        // A queued job whose runner never consumes it — re-`queued` every cycle.
        let runs = vec![run_with_job(88, "queued")];

        // Grace: the first K attempts re-provision promptly (back-to-back).
        for _ in 0..PROVISION_GRACE_ATTEMPTS {
            assert_eq!(
                drive_stuck_cycle(&rc, &runs, 88, 0).await,
                1,
                "grace is prompt"
            );
        }
        // Within the backoff window: no re-provision.
        assert_eq!(
            drive_stuck_cycle(&rc, &runs, 88, 1).await,
            0,
            "throttled within the window"
        );
        assert_eq!(
            drive_stuck_cycle(&rc, &runs, 88, PROVISION_BACKOFF_BASE_MS - 1).await,
            0,
            "still throttled just before it closes"
        );
        // Past the window: re-provisions once more.
        let mut now = PROVISION_BACKOFF_BASE_MS;
        assert_eq!(
            drive_stuck_cycle(&rc, &runs, 88, now).await,
            1,
            "re-provisions past the window"
        );
        // Drive the remaining fast attempts up to the cap, always past the (capped)
        // window. `note_provision_attempt` re-arms `next_ok_ms` at the ceiling once
        // the backoff saturates, so each step advances by exactly one ceiling.
        let mut attempts = PROVISION_GRACE_ATTEMPTS + 1;
        while attempts < PROVISION_MAX_ATTEMPTS {
            now += PROVISION_BACKOFF_CEILING_MS;
            assert_eq!(
                drive_stuck_cycle(&rc, &runs, 88, now).await,
                1,
                "attempt allowed"
            );
            attempts += 1;
        }
        // Fast-attempt cap spent — but the job now SELF-HEALS (bug-002 defect B),
        // it is not abandoned. Just inside the re-armed ceiling window it still
        // holds off (slow cadence, not every cycle)...
        assert_eq!(
            drive_stuck_cycle(&rc, &runs, 88, now + 1).await,
            0,
            "slow-retry: still throttled inside the ceiling window past the cap"
        );
        // ...then re-provisions once each ceiling interval, indefinitely, so the
        // moment the GitHub-side blocker clears the job is served.
        now += PROVISION_BACKOFF_CEILING_MS;
        assert_eq!(
            drive_stuck_cycle(&rc, &runs, 88, now).await,
            1,
            "self-heals at the ceiling cadence past the cap"
        );
        now += PROVISION_BACKOFF_CEILING_MS;
        assert_eq!(
            drive_stuck_cycle(&rc, &runs, 88, now).await,
            1,
            "and keeps slow-retrying — never abandoned"
        );

        assert_eq!(
            metrics.lock().await.counts.provisioned,
            PROVISION_MAX_ATTEMPTS as u64 + 2,
            "MAX fast attempts plus continuing slow-retry provisions — self-healing, \
             still cadence-bounded (never a per-cycle storm)"
        );
    }

    // --- demand-based provisioning (ADR-0004) ------------------------------
    //
    // The pure supply math first, then the `process_run_jobs` seam driven in demand
    // mode: the shared GitHub runner list decides how much of the queued backlog is
    // already covered (by this instance's runners or a peer's), and only the
    // shortfall mints. The last test pins the flag gate — the same setup that
    // suppresses in demand mode still mints in per-job mode.

    #[test]
    fn faber_job_id_parses_only_faber_names() {
        assert_eq!(faber_job_id("faber-42-1700000000000"), Some(42));
        assert_eq!(faber_job_id("faber-42"), Some(42)); // no ts suffix
        assert_eq!(faber_job_id("runner-1"), None); // not a faber name
        assert_eq!(faber_job_id("faber-abc-1"), None); // non-numeric id
        assert_eq!(faber_job_id("faber-"), None); // empty id
        assert_eq!(faber_job_id("faber--1"), None); // empty first field
                                                    // #94: the instance suffix leaves the first two fields where every
                                                    // parser — this one, and an older peer's — reads them.
        assert_eq!(faber_job_id("faber-42-1700000000000-builder"), Some(42));
        assert_eq!(
            faber_mint_ts("faber-42-1700000000000-builder"),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            faber_mint_ts("faber-42-1700000000000"),
            Some(1_700_000_000_000)
        );
        let claimed: HashSet<u64> = [42].into_iter().collect();
        assert!(runner_still_claimed(
            Some("faber-42-1700000000000-builder"),
            &claimed
        ));
    }

    /// #94: every runner this host mints ends in its instance id, and the name
    /// still reads as faber's to every check that guards a request.
    #[test]
    fn a_minted_name_ends_in_this_instance() {
        let admitted = AtomicBool::new(false);
        let runner = std::sync::Mutex::new(None);
        let waiting = std::sync::Mutex::new(Parking::default());
        let (name, _) = admit_claim(42, "builder", &admitted, &runner, &waiting);
        assert!(name.starts_with("faber-42-"), "{name}");
        assert!(name.ends_with("-builder"), "{name}");
        assert_eq!(crate::instance::of_runner(&name), Some("builder"));
        assert_eq!(faber_job_id(&name), Some(42));
        assert!(crate::untrusted::runner_name(&name).is_some());
    }

    /// The retry sweep must not write a run off as genuine while faber is still
    /// running the runner that failed it: the outcome (with any OOM in it) lands
    /// seconds after GitHub already calls the job complete — about six seconds on
    /// one live OOM kill — and the dedup memory is permanent for that attempt.
    /// Deferring one cycle is free; concluding early loses the retry for good.
    #[test]
    fn a_runner_this_host_is_still_running_defers_the_retry_verdict() {
        let claimed: HashSet<u64> = [1001].into_iter().collect();
        assert!(runner_still_claimed(
            Some("faber-1001-1700000000000"),
            &claimed
        ));
        // A re-provision of the same job carries a different mint timestamp and
        // still matches — deliberately coarse, because every extra match only
        // defers a decision, which is the safe direction.
        assert!(runner_still_claimed(
            Some("faber-1001-1700000999999"),
            &claimed
        ));
        // Released claim, a peer host's runner, or no runner at all: nothing to wait
        // for, so the sweep may conclude on the evidence it has.
        assert!(!runner_still_claimed(
            Some("faber-1002-1700000000000"),
            &claimed
        ));
        assert!(!runner_still_claimed(Some("gh-hosted-7"), &claimed));
        assert!(!runner_still_claimed(None, &claimed));
        assert!(!runner_still_claimed(
            Some("faber-1001-1700000000000"),
            &HashSet::new()
        ));
    }

    #[test]
    fn runner_is_supply_counts_idle_and_fresh_connecting_only() {
        const NOW: u64 = 1_700_000_000_000;
        let grace = CONNECTING_GRACE.as_millis() as u64;
        let live: HashSet<u64> = [77].into_iter().collect();
        // Online + idle: will grab a queued job (ts is immaterial for online).
        assert!(runner_is_supply(
            &runner("faber-1-1", false, "online"),
            &live,
            NOW
        ));
        // Online + busy: already working — not queued-backlog supply.
        assert!(!runner_is_supply(
            &runner("faber-1-1", true, "online"),
            &live,
            NOW
        ));
        // Offline faber runner, namesake job live, minted just now: connecting supply.
        let fresh = format!("faber-77-{NOW}");
        assert!(runner_is_supply(
            &runner(&fresh, false, "offline"),
            &live,
            NOW
        ));
        // Same runner one ms past the connecting grace: dead, not connecting — the
        // masking-deadlock regression. It must stop counting so demand re-mints.
        let dead = format!("faber-77-{}", NOW - grace - 1);
        assert!(!runner_is_supply(
            &runner(&dead, false, "offline"),
            &live,
            NOW
        ));
        // Offline faber runner whose namesake is gone: a stale orphan, ignored even
        // when freshly minted.
        let orphan = format!("faber-999-{NOW}");
        assert!(!runner_is_supply(
            &runner(&orphan, false, "offline"),
            &live,
            NOW
        ));
        // Offline non-faber runner: possibly permanently dead — never counted.
        assert!(!runner_is_supply(
            &runner("other", false, "offline"),
            &live,
            NOW
        ));
    }

    #[test]
    fn stale_runner_ids_reaps_only_old_offline_faber_runners() {
        const NOW: u64 = 1_700_000_000_000;
        let ttl = STALE_RUNNER_TTL.as_millis() as u64;
        let old = NOW - ttl - 1; // past the reap threshold
        let recent = NOW - 1_000; // still within it
        let mk = |id: u64, name: &str, status: &str| Runner {
            id,
            ..runner(name, false, status)
        };
        let runners = vec![
            mk(1, &format!("faber-10-{old}"), "offline"), // dead orphan → reap
            mk(2, &format!("faber-11-{recent}"), "offline"), // maybe still connecting → keep
            mk(3, &format!("faber-12-{old}"), "online"),  // online (busy/idle) → keep
            mk(4, &format!("other-{old}"), "offline"),    // not faber-minted → keep
        ];
        assert_eq!(
            stale_runner_ids(&runners, NOW),
            vec![1],
            "only the old, offline, faber-minted runner is reaped"
        );
    }

    #[test]
    fn stale_runner_ids_defers_a_runner_github_still_reports_busy() {
        // The live fault: GitHub refuses `DELETE` for a runner it still has a job
        // on, with a `422`. That `busy` flag is in the very listing that made the
        // runner a candidate, so faber declines to ask instead of asking and being
        // refused — the refusal guards a job that, in a shared pool, may be a peer
        // instance's.
        const NOW: u64 = 1_700_000_000_000;
        let old = NOW - STALE_RUNNER_TTL.as_millis() as u64 - 1;
        let mk = |id: u64, busy: bool| Runner {
            id,
            ..runner(&format!("faber-{id}-{old}"), busy, "offline")
        };
        let runners = vec![mk(1, false), mk(2, true)];

        assert_eq!(
            stale_runner_ids(&runners, NOW),
            vec![1],
            "a stale runner GitHub reports busy is not attempted"
        );
        assert_eq!(
            deferred_busy_runner_ids(&runners, NOW),
            vec![2],
            "and is reported as deferred rather than silently dropped"
        );

        // The deferral is not a state machine: the moment GitHub clears the job,
        // the same listing makes the runner reapable. Nothing is remembered between
        // cycles, so this holds identically after a daemon restart.
        let cleared = vec![mk(1, false), mk(2, false)];
        assert_eq!(
            stale_runner_ids(&cleared, NOW),
            vec![1, 2],
            "once GitHub un-busies it, the next sweep reaps it"
        );
        assert!(deferred_busy_runner_ids(&cleared, NOW).is_empty());
    }

    #[test]
    fn stale_runner_ids_attempts_a_runner_busy_past_the_grace() {
        // Deferral is bounded: a registration GitHub never un-busies must not sit
        // ignored forever. Past `BUSY_RUNNER_GRACE` faber asks regardless — GitHub
        // either allows it or refuses, and a refusal at that age is genuinely
        // unexpected, so it arms the backoff and logs.
        const NOW: u64 = 1_700_000_000_000;
        let grace = BUSY_RUNNER_GRACE.as_millis() as u64;
        let mk = |id: u64, minted: u64| Runner {
            id,
            ..runner(&format!("faber-{id}-{minted}"), true, "offline")
        };
        let within = mk(1, NOW - grace); // exactly at the bound → still deferred
        let past = mk(2, NOW - grace - 1);

        let runners = vec![within, past];
        assert_eq!(
            stale_runner_ids(&runners, NOW),
            vec![2],
            "only the runner busy past the grace is attempted"
        );
        assert_eq!(deferred_busy_runner_ids(&runners, NOW), vec![1]);
    }

    #[test]
    fn reap_and_deferral_sets_partition_the_stale_runners() {
        // The two functions must together account for every stale offline runner
        // and never both claim one — otherwise a runner leaks unnoticed (in neither
        // set) or is attempted while deferred (in both).
        const NOW: u64 = 1_700_000_000_000;
        let ttl = STALE_RUNNER_TTL.as_millis() as u64;
        let grace = BUSY_RUNNER_GRACE.as_millis() as u64;
        let mk = |id: u64, busy: bool, status: &str, minted: u64| Runner {
            id,
            ..runner(&format!("faber-{id}-{minted}"), busy, status)
        };
        let runners = vec![
            mk(1, false, "offline", NOW - ttl - 1),  // stale, idle
            mk(2, true, "offline", NOW - ttl - 1),   // stale, busy, within grace
            mk(3, true, "offline", NOW - grace - 1), // stale, busy, past grace
            mk(4, false, "offline", NOW - 1_000),    // too fresh → not stale
            mk(5, true, "online", NOW - grace - 1),  // online → not stale
            Runner {
                id: 6,
                ..runner("human-runner", true, "offline") // not faber-minted
            },
        ];

        let reaped = stale_runner_ids(&runners, NOW);
        let deferred = deferred_busy_runner_ids(&runners, NOW);
        assert_eq!(reaped, vec![1, 3]);
        assert_eq!(deferred, vec![2]);

        let in_scope: Vec<u64> = runners
            .iter()
            .filter(|r| is_stale_offline(r, NOW))
            .map(|r| r.id)
            .collect();
        assert_eq!(in_scope, vec![1, 2, 3], "the shared age/name test");
        let mut union: Vec<u64> = reaped.iter().chain(deferred.iter()).copied().collect();
        union.sort_unstable();
        assert_eq!(
            union, in_scope,
            "every in-scope runner lands in exactly one set"
        );
    }

    #[test]
    fn retain_dereg_drops_state_for_runners_no_longer_listed() {
        // `clear_dereg` only fires on a deregistration faber performed, so a runner
        // that left GitHub's list another way — a peer instance reaped it, or GitHub
        // GC'd it — would keep its entry for the daemon's whole lifetime.
        let mut m = Metrics::default();
        const NOW: u64 = 1_700_000_000_000;
        m.note_dereg_failure(111, NOW);
        m.note_dereg_failure(222, NOW);
        assert_eq!(m.dereg.len(), 2);

        // 111 is still listed; 222 has vanished from GitHub's side.
        let listed: HashSet<u64> = [111].into_iter().collect();
        m.retain_dereg(&listed);
        assert_eq!(m.dereg.len(), 1);
        assert!(
            !m.dereg_gate(111, NOW),
            "a still-listed runner keeps backing off"
        );
        assert!(
            m.dereg_gate(222, NOW),
            "a runner GitHub no longer lists carries no state"
        );
    }

    #[test]
    fn note_deferral_reports_once_and_never_gates() {
        // The deferral is re-derived from GitHub's listing every cycle, so the
        // report has to be once-per-runner or it floods at ~40s intervals for as
        // long as the job runs — the #21 mistake, one severity lower.
        let mut m = Metrics::default();
        const NOW: u64 = 1_700_000_000_000;
        assert!(m.note_deferral(16734), "the first deferral is reported");
        for _ in 0..100 {
            assert!(!m.note_deferral(16734), "later cycles stay silent");
        }
        assert!(
            m.note_deferral(16807),
            "an unrelated runner reports on its own"
        );

        // Critically, being deferred must not gate the runner: the moment GitHub
        // un-busies it, the very next sweep has to be free to reap it.
        assert!(
            m.dereg_gate(16734, NOW),
            "a deferral leaves the runner immediately eligible"
        );

        // And the entry it allocated is pruned like any other once GitHub drops the
        // runner from its listing.
        m.retain_dereg(&HashSet::new());
        assert!(m.dereg.is_empty());
    }

    // --- stale-runner dereg backoff (#21) -------------------------------------

    #[test]
    fn is_permanent_dereg_failure_classifies_on_status_code_only() {
        // The exact shape GitHub returns for a wedged runner (#21): a `422` whose
        // prose says "currently running a job and cannot be deleted".
        let busy_422 = GithubError::Status {
            status: 422,
            url: "https://api.github.com/repos/owner/demo/actions/runners/7273".to_string(),
            body: "{\"message\":\"Bad request - Runner faber-1-1 is currently running \
                    a job and cannot be deleted.\"}"
                .to_string(),
        };
        assert!(is_permanent_dereg_failure(&busy_422));

        // Classification never looks at the body — a completely different message
        // on the same status still counts as permanent. The status is the
        // contract; the prose is not (see the function's own doc comment).
        let differently_worded_422 = GithubError::Status {
            status: 422,
            url: "https://api.github.com/repos/owner/demo/actions/runners/1".to_string(),
            body: "{\"message\":\"some unrelated validation problem\"}".to_string(),
        };
        assert!(is_permanent_dereg_failure(&differently_worded_422));

        // Every other status is transient-shaped — must keep retrying next cycle.
        for status in [404, 403, 401, 429, 500, 502, 503] {
            let e = GithubError::Status {
                status,
                url: "https://api.github.com/x".to_string(),
                body: String::new(),
            };
            assert!(
                !is_permanent_dereg_failure(&e),
                "status {status} must not be treated as permanent"
            );
        }
        // Non-Status variants (transport, token, decode, cache) are all transient.
        assert!(!is_permanent_dereg_failure(&GithubError::Token(
            "no token".to_string()
        )));
        assert!(!is_permanent_dereg_failure(&GithubError::StaleCache(
            "runners".to_string()
        )));
        let decode_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(!is_permanent_dereg_failure(&GithubError::Decode(
            decode_err
        )));
    }

    #[test]
    fn dereg_backoff_ms_is_exponential_and_capped() {
        assert_eq!(dereg_backoff_ms(1), DEREG_BACKOFF_BASE_MS);
        assert_eq!(dereg_backoff_ms(2), DEREG_BACKOFF_BASE_MS * 2);
        assert_eq!(dereg_backoff_ms(3), DEREG_BACKOFF_BASE_MS * 4);
        assert_eq!(dereg_backoff_ms(1000), DEREG_BACKOFF_CEILING_MS, "clamped");
    }

    #[test]
    fn dereg_gate_backs_off_after_a_permanent_failure_then_recovers() {
        let mut m = Metrics::default();
        const NOW: u64 = 1_700_000_000_000;
        // Untouched runner id: always clear to attempt — the common (success)
        // case is never gated.
        assert!(m.dereg_gate(7273, NOW));

        // First permanent-shaped failure: told to warn, and immediately backs off
        // — this is the #21 fix, replacing "retried every cycle forever".
        assert!(m.note_dereg_failure(7273, NOW), "first failure must warn");
        assert!(
            !m.dereg_gate(7273, NOW),
            "must not be eligible again at the same instant it just failed"
        );

        let window = dereg_backoff_ms(1);
        assert!(
            !m.dereg_gate(7273, NOW + window - 1),
            "one ms short of the window must still back off"
        );
        assert!(
            m.dereg_gate(7273, NOW + window),
            "clear again once the window elapses"
        );

        // A second failure is not the first any more (no re-warn), and re-arms a
        // strictly longer window — the exponential part of the backoff.
        let now2 = NOW + window;
        assert!(
            !m.note_dereg_failure(7273, now2),
            "second failure must not re-warn"
        );
        let window2 = dereg_backoff_ms(2);
        assert!(window2 > window, "backoff must grow on a repeat failure");
        assert!(!m.dereg_gate(7273, now2 + window2 - 1));
        assert!(m.dereg_gate(7273, now2 + window2));
    }

    #[test]
    fn dereg_state_does_not_leak_across_unrelated_runners() {
        let mut m = Metrics::default();
        const NOW: u64 = 1_700_000_000_000;
        assert!(m.note_dereg_failure(111, NOW));
        assert!(!m.dereg_gate(111, NOW), "the failing runner backs off");
        assert!(
            m.dereg_gate(222, NOW),
            "an unrelated runner id must be entirely unaffected by another's failure"
        );
        // Failing the second runner doesn't disturb the first's already-armed
        // state, and is itself a fresh "first" failure with its own warn.
        assert!(m.note_dereg_failure(222, NOW));
        assert!(!m.dereg_gate(111, NOW));
        assert!(!m.dereg_gate(222, NOW));
    }

    #[test]
    fn a_successful_deregistration_is_never_gated_and_clears_prior_backoff() {
        let mut m = Metrics::default();
        const NOW: u64 = 1_700_000_000_000;
        // The ordinary path — a runner that has never failed — is never gated,
        // i.e. unaffected by the new backoff machinery.
        assert!(m.dereg_gate(9, NOW));

        // A runner that failed once, then succeeds on a later attempt (once its
        // backoff window has elapsed): success clears its state rather than
        // leaving a stale entry sitting in the map.
        m.note_dereg_failure(9, NOW);
        let window = dereg_backoff_ms(1);
        assert!(m.dereg_gate(9, NOW + window));
        m.clear_dereg(9);
        assert!(
            m.dereg_gate(9, NOW + window),
            "cleared state must not still gate"
        );

        // clear_dereg on an id with no entry is a harmless no-op.
        m.clear_dereg(999);
        assert!(m.dereg_gate(999, NOW));
    }

    /// Build a routing profile with `labels` (a macOS VM backend; the demand tests
    /// never provision, so the backend is immaterial).
    fn profile_named(name: &str, labels: &[&str]) -> Profile {
        let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        Profile {
            name: name.to_string(),
            labels: labels.clone(),
            backend: vm_backend(),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels),
        }
    }

    #[test]
    fn compute_supply_attributes_runners_to_profiles() {
        let profiles = vec![
            profile_named("macos", &["self-hosted", "macOS", "ARM64"]),
            profile_named("docker", &["self-hosted", "docker"]),
        ];
        let live = HashSet::new();
        let runners = vec![
            runner_labeled("faber-1-1", false, "online", &["self-hosted", "docker"]),
            runner_labeled(
                "faber-2-1",
                false,
                "online",
                &["self-hosted", "macOS", "ARM64"],
            ),
            runner_labeled("faber-3-1", false, "online", &["self-hosted", "docker"]),
            // Busy → excluded; unroutable labels → attributed to no profile.
            runner_labeled("faber-4-1", true, "online", &["self-hosted", "docker"]),
            runner_labeled("ext-1", false, "online", &["ubuntu-latest"]),
        ];
        let mut supply = compute_supply(&runners, &profiles, &NO_SERVICES, &live, 0);
        assert_eq!(
            supply.runners.len(),
            3,
            "three routable idle runners; the busy and the unroutable ones excluded"
        );

        // Spent against the *job's* labels, and each runner covers only one job.
        let l = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let mac = l(&["self-hosted", "macOS", "ARM64"]);
        let docker = l(&["self-hosted", "docker"]);
        assert!(supply.take_for(&mac), "one idle macOS runner");
        assert!(!supply.take_for(&mac), "…and only one");
        assert!(supply.take_for(&docker), "two idle docker runners");
        assert!(supply.take_for(&docker));
        assert!(!supply.take_for(&docker), "…and no more");
    }

    #[test]
    fn supply_never_masks_a_job_the_runner_could_not_be_handed() {
        // The starvation ADR-0010 names: a runner carrying a narrower per-job label
        // is real supply for its own pool and *no* supply for a job pinning a
        // different version. Counting it per profile — both runners route to the
        // same `[self-hosted, docker]` profile — would decline to mint for a job
        // that can never land on it, and the job would wait for a runner which is
        // never coming.
        let profiles = vec![profile_named("docker", &["self-hosted", "docker"])];
        let live = HashSet::new();
        let runners = vec![runner_labeled(
            "faber-1-1",
            false,
            "online",
            &["self-hosted", "docker", "faber.playwright-1.60.0"],
        )];
        let mut supply = compute_supply(&runners, &profiles, &NO_SERVICES, &live, 0);

        let l = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let other_version = l(&["self-hosted", "docker", "faber.playwright-1.62.1"]);
        assert!(
            !supply.take_for(&other_version),
            "a 1.60.0 runner must not mask a job pinning 1.62.1 — that job would starve"
        );

        // Its own pool still counts, and an unlabelled job still matches it:
        // extra labels on a runner never exclude it (`job ⊆ runner`).
        let unlabelled = l(&["self-hosted", "docker"]);
        assert!(supply.take_for(&unlabelled));
    }

    #[test]
    fn supply_does_not_steer_a_job_that_did_not_ask_onto_a_dind_runner() {
        // ADR-0026: only a job asking for `dind` gets a daemon. A daemon runner
        // must not count as supply for a job that did not, or faber would skip
        // minting for it and steer it onto a daemon it never asked for.
        let l = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let daemon = || Supply {
            runners: vec![l(&["faber", "docker", "dind"])],
            ..Supply::default()
        };

        assert!(
            !daemon().take_for(&l(&["faber", "docker"])),
            "a job that did not ask for dind is not covered by a daemon runner"
        );
        assert!(daemon().take_for(&l(&["faber", "docker", "dind"])));
        assert!(
            daemon().take_for(&l(&["faber", "docker", "DinD"])),
            "labels match case-insensitively, as GitHub does"
        );
    }

    /// ADR-0046: a runner whose services were started for one job is not supply
    /// for a job that did not ask for every one of them. Counting it would skip
    /// a mint on a runner GitHub may hand to the service job it was minted for.
    #[test]
    fn supply_keeps_a_service_runner_for_jobs_that_asked_for_its_services() {
        let l = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let serviced = || Supply {
            runners: vec![l(&["faber", "docker", "faber.postgres-16", "faber.redis"])],
            ..Supply::default()
        };
        assert!(
            !serviced().take_for(&l(&["faber", "docker"])),
            "a job without services is not covered by a service runner"
        );
        assert!(
            !serviced().take_for(&l(&["faber", "docker", "faber.redis"])),
            "nor is one that asked for only some of them"
        );
        assert!(serviced().take_for(&l(&["faber", "docker", "faber.postgres-16", "faber.redis"])));
        // An ADR-0010 need is not a service: a runner carrying one is supply as
        // it always was.
        let needs = Supply {
            runners: vec![l(&["faber", "docker", "faber.playwright-1.62.1"])],
            ..Supply::default()
        }
        .take_for(&l(&["faber", "docker"]));
        assert!(needs);
        // A configured name is a service too (A1): its runner is a service
        // runner for jobs that did not ask for it, and an unconfigured one is a
        // need, as it always was.
        let mut policy = crate::services::ServicesPolicy::default();
        policy.per_job.insert("s3".into(), per_job_policy());
        let s3 = |policy: &crate::services::ServicesPolicy| Supply {
            runners: vec![l(&["faber", "docker", "faber.s3"])],
            services: policy.clone(),
        };
        assert!(!s3(&policy).take_for(&l(&["faber", "docker"])));
        assert!(s3(&policy).take_for(&l(&["faber", "docker", "faber.s3"])));
        assert!(s3(&NO_SERVICES).take_for(&l(&["faber", "docker", "faber.s3"])));
        assert!(s3(&NO_SERVICES).take_for(&l(&["faber", "docker"])));
    }

    fn per_job_policy() -> crate::services::PerJobPolicy {
        crate::services::PerJobPolicy {
            image: "ghcr.io/owner/demo:1".into(),
            memory_mib: 64,
            args: vec![],
            env: Default::default(),
            job_env: Default::default(),
            health_cmd: None,
        }
    }

    #[test]
    fn a_service_label_is_a_faber_label_naming_a_service() {
        for yes in [
            "faber.postgres-16",
            "faber.redis",
            "faber.mailpit",
            "faber.postgres",
            "Faber.Redis",
        ] {
            assert!(is_service_label(yes, &NO_SERVICES), "{yes}");
        }
        // A configured name is a service label, in any case; unconfigured it is
        // a tool (A1).
        let mut policy = crate::services::ServicesPolicy::default();
        policy.per_job.insert("s3".into(), per_job_policy());
        for yes in ["faber.s3", "Faber.S3"] {
            assert!(is_service_label(yes, &policy), "{yes}");
            assert!(!is_service_label(yes, &NO_SERVICES), "{yes}");
        }
        assert!(!is_service_label("faber.s4", &policy));
        // Only the bare form of a per-job name; Postgres takes either.
        assert!(!is_service_label("faber.s3-1", &policy));
        assert!(!is_service_label("faber.redis-7", &NO_SERVICES));
        assert!(is_service_label("faber.postgres-16", &NO_SERVICES));
        for no in [
            "faber.playwright-1.62.1",
            "faber.redis-cli",
            "redis",
            "postgres-16",
            "faber.",
            "dind",
            "self-hosted",
        ] {
            assert!(!is_service_label(no, &NO_SERVICES), "{no}");
        }
    }

    /// ADR-0046 §10: only Docker profiles provide services, so a service label
    /// routed to a VM profile is refused, with the label named.
    #[test]
    fn a_service_label_on_a_non_docker_profile_is_named_for_refusal() {
        let l = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(
            service_label_on_non_docker(&l(&["faber", "macos", "faber.redis"]), &NO_SERVICES)
                .as_deref(),
            Some("faber.redis")
        );
        assert_eq!(
            service_label_on_non_docker(&l(&["faber", "macos", "faber.xcode-16.2"]), &NO_SERVICES),
            None,
            "a job without services is untouched"
        );
        assert!(NON_DOCKER_SERVICE_REASON.contains("Docker"));
    }

    fn plan(specs: &[(&str, Option<&str>, u64)]) -> crate::services::ServicePlan {
        crate::services::ServicePlan {
            specs: specs
                .iter()
                .map(
                    |(service, version, memory_mib)| crate::services::ServiceSpec {
                        service: service.to_string(),
                        version: version.map(str::to_string),
                        image: format!("{service}:test"),
                        memory_mib: *memory_mib,
                        label: match version {
                            Some(v) => format!("faber.{service}-{v}"),
                            None => format!("faber.{service}"),
                        },
                        per_job: version.is_none().then(per_job_policy),
                    },
                )
                .collect(),
        }
    }

    /// ADR-0046 §7/§8: a per-job service's memory is the job's, added to what
    /// admission debits and to the p75 shadow; the shared server's is not.
    #[test]
    fn a_reservation_carries_the_per_job_services_but_not_the_shared_server() {
        let base = Reservation {
            mib: 4096,
            mcpu: 2000,
            disk_mib: 512,
            shadow_mib: 3000,
        };
        let mut d = crate::docker::DockerConfig::new("ghcr.io/owner/my-runner:latest".into());
        let none = with_services(base, &Backend::Docker(d.clone()));
        assert_eq!((none.mib, none.shadow_mib), (4096, 3000));

        d.services = plan(&[
            ("postgres", Some("16"), 2048),
            ("redis", None, 256),
            ("mailpit", None, 128),
        ]);
        let r = with_services(base, &Backend::Docker(d));
        assert_eq!((r.mib, r.shadow_mib), (4096 + 384, 3000 + 384));
        assert_eq!((r.mcpu, r.disk_mib), (2000, 512), "memory only");
    }

    /// ADR-0046 §7: after a restart every runner container still there keeps
    /// its database, by claim id.
    #[test]
    fn the_live_claim_ids_are_every_runner_the_scan_found() {
        let scan = crate::docker::Scan {
            adoptable: vec![
                adopted("/faber-runner-42-1000-b2", "owner/demo"),
                adopted("/faber-runner-43-1001", "owner/demo"),
            ],
            opaque: Vec::new(),
        };
        assert_eq!(live_claim_ids(&scan), ["42-1000-b2", "43-1001"]);
        assert!(live_claim_ids(&crate::docker::Scan::default()).is_empty());
    }

    /// ADR-0046 §2: a service refusal is listed once per job and stays out of
    /// `refused_queued`, like a dind refusal.
    #[tokio::test]
    async fn a_service_refusal_counts_once_and_is_listed() {
        let (orch, _tmp) = test_orch();
        let view = |at_ms| ServiceRefusedView {
            repo: "owner/demo".into(),
            run_id: 9,
            job_id: 7,
            job_name: "test".into(),
            profile: "linux-docker".into(),
            label: "faber.postgres-12".into(),
            reason: "postgres 12 is not in versions".into(),
            at_ms,
        };
        orch.refuse_services(view(now_ms())).await;
        orch.refuse_services(view(now_ms())).await;
        let m = orch.metrics.lock().await;
        assert_eq!(m.counts.service_refused_total, 1);
        assert!(m.refused_queued.is_empty());
        let listed = m.service_refused_sorted(now_ms());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "faber.postgres-12");
    }

    /// Owned pieces a demand-mode `process_run_jobs` call borrows. `profiles()` is a
    /// single docker-labelled profile at index 0 — the labels `run_with_job` gives
    /// its jobs, so a matching `runner_labeled(..&["self-hosted","docker"])` is
    /// attributed to it.
    struct Fixture {
        profiles: Vec<Profile>,
        metrics: Mutex<Metrics>,
        inflight: Mutex<HashMap<u64, Claim>>,
        audit: Audit,
        _tmp: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new();
            let audit = Audit::at(tmp.path.join("audit.jsonl"));
            Self {
                profiles: profiles(),
                metrics: Mutex::new(Metrics::default()),
                inflight: Mutex::new(HashMap::new()),
                audit,
                _tmp: tmp,
            }
        }

        fn ctx(&self) -> ReconcileCtx<'_> {
            ReconcileCtx {
                profiles: &self.profiles,
                services: &NO_SERVICES,
                metrics: &self.metrics,
                inflight: &self.inflight,
                audit: &self.audit,
            }
        }
    }

    fn docker_runner(name: &str, busy: bool, status: &str) -> Runner {
        runner_labeled(name, busy, status, &["self-hosted", "docker"])
    }

    #[tokio::test]
    async fn demand_skips_minting_when_an_idle_runner_covers_the_job() {
        let fx = Fixture::new();
        let rc = fx.ctx();
        let runs = vec![run_with_job(77, "queued")];
        let runners = vec![docker_runner("faber-other-1", false, "online")];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            0,
        )
        .await;
        assert!(
            intents.is_empty(),
            "an existing idle runner covers the job — no mint"
        );
        assert_eq!(fx.metrics.lock().await.counts.provisioned, 0);
        assert!(seen.contains(&77), "the job is still tracked for pruning");
    }

    #[tokio::test]
    async fn demand_mints_only_the_shortfall() {
        let fx = Fixture::new();
        let rc = fx.ctx();
        // Two queued jobs, one idle runner → net demand of one.
        let runs = vec![run_with_job(1, "queued"), run_with_job(2, "queued")];
        let runners = vec![docker_runner("faber-x-1", false, "online")];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            0,
        )
        .await;
        assert_eq!(
            intents.len(),
            1,
            "one runner covers one job; the other mints"
        );
        assert_eq!(fx.metrics.lock().await.counts.provisioned, 1);
    }

    #[tokio::test]
    async fn demand_ignores_a_busy_runner() {
        let fx = Fixture::new();
        let rc = fx.ctx();
        let runs = vec![run_with_job(77, "queued")];
        // Busy: already running a job, not available for the queued backlog.
        let runners = vec![docker_runner("faber-b-1", true, "online")];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            0,
        )
        .await;
        assert_eq!(
            intents.len(),
            1,
            "a busy runner is not supply — the job mints"
        );
    }

    #[tokio::test]
    async fn demand_counts_a_connecting_peer_runner() {
        let fx = Fixture::new();
        let rc = fx.ctx();
        // A peer just minted for this very job: its runner is offline (connecting)
        // and named for job 77, which is live this cycle → counted as supply. `now`
        // is 1s past the mint, well within CONNECTING_GRACE.
        let mint_ts = 1_700_000_000_000u64;
        let runs = vec![run_with_job(77, "queued")];
        let runners = vec![docker_runner(
            &format!("faber-77-{mint_ts}"),
            false,
            "offline",
        )];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            mint_ts + 1_000,
        )
        .await;
        assert!(
            intents.is_empty(),
            "a peer's connecting runner covers the job"
        );
    }

    #[tokio::test]
    async fn demand_re_mints_past_a_dead_offline_runner() {
        let fx = Fixture::new();
        let rc = fx.ctx();
        // The deadlock regression: an offline runner minted for job 77 that never
        // came online. Job 77 is still queued *because* that runner died, so its
        // name stays live-matched — but once minted past CONNECTING_GRACE it must no
        // longer count as supply, or demand mints nothing and the job hangs forever.
        let mint_ts = 1_700_000_000_000u64;
        let now = mint_ts + CONNECTING_GRACE.as_millis() as u64 + 1;
        let runs = vec![run_with_job(77, "queued")];
        let runners = vec![docker_runner(
            &format!("faber-77-{mint_ts}"),
            false,
            "offline",
        )];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            now,
        )
        .await;
        assert_eq!(
            intents.len(),
            1,
            "a dead offline runner past the grace no longer covers its job — re-mint"
        );
    }

    #[tokio::test]
    async fn demand_ignores_a_stale_offline_orphan() {
        let fx = Fixture::new();
        let rc = fx.ctx();
        // An offline runner named for a job no longer live: a dead orphan must not
        // mask the real, still-queued job 77.
        let runs = vec![run_with_job(77, "queued")];
        let runners = vec![docker_runner("faber-999-1", false, "offline")];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            0,
        )
        .await;
        assert_eq!(
            intents.len(),
            1,
            "a stale orphan is not supply — the job mints"
        );
    }

    #[tokio::test]
    async fn demand_ignores_a_leaked_busy_offline_runner() {
        // The cross-instance guarantee behind the deferral above: a registration
        // left behind because GitHub still reports a job on it must not be counted
        // as supply, or it would suppress a legitimate mint here *and* on the peer
        // Mac sharing this pool (both run this same calculation over the same
        // listing). `runner_is_supply` only credits an `offline` runner inside
        // `CONNECTING_GRACE`, and anything stale enough to be a sweep candidate is
        // far past it — so the shortfall is unaffected however long one lingers.
        let fx = Fixture::new();
        let rc = fx.ctx();
        let mint_ts = 1_000u64;
        let now = mint_ts + STALE_RUNNER_TTL.as_millis() as u64 + 1;
        let runs = vec![run_with_job(77, "queued")];
        // Named for job 77 itself — the worst case, since a namesake is the only
        // offline runner that can ever count as supply.
        let runners = vec![docker_runner(
            &format!("faber-77-{mint_ts}"),
            true,
            "offline",
        )];

        assert!(
            !runner_is_supply(&runners[0], &[77].into_iter().collect(), now),
            "a stale busy-offline runner is never supply"
        );

        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            now,
        )
        .await;
        assert_eq!(
            intents.len(),
            1,
            "the leaked registration does not mask its own still-queued job"
        );
    }

    #[tokio::test]
    async fn per_job_mode_ignores_available_supply() {
        // The flag gates the behavior: the same idle-runner setup that suppresses in
        // demand mode still mints in per-job mode.
        let fx = Fixture::new();
        let rc = fx.ctx();
        let runs = vec![run_with_job(77, "queued")];
        let runners = vec![docker_runner("faber-idle-1", false, "online")];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::PerJob,
            &mut seen,
            0,
        )
        .await;
        assert_eq!(intents.len(), 1, "per-job mode mints regardless of supply");
    }

    #[tokio::test]
    async fn per_job_mode_ignores_every_runner_shape() {
        // The stale-runner sweep needs the runner listing in *both* modes, so a
        // real, non-empty list now reaches `process_run_jobs` under per-job for the
        // first time — previously it was unconditionally empty there. Nothing in
        // that list may change a per-job decision: supply is a demand-mode concept.
        // Covers every shape the listing can carry, not just the idle runner
        // `per_job_mode_ignores_available_supply` pins.
        let mint_ts = 1_000u64;
        let now = mint_ts + STALE_RUNNER_TTL.as_millis() as u64 + 1;
        let runners = || {
            vec![
                docker_runner("faber-1-1", false, "online"), // idle — supply in demand mode
                docker_runner("faber-2-1", true, "online"),  // busy
                docker_runner(&format!("faber-77-{mint_ts}"), true, "offline"), // stale namesake
            ]
        };
        let runs = || vec![run_with_job(77, "queued")];

        let fx = Fixture::new();
        let intents = process_run_jobs(
            &fx.ctx(),
            "owner/demo",
            &runs(),
            &Codes::new(),
            &runners(),
            ProvisioningMode::PerJob,
            &mut HashSet::new(),
            now,
        )
        .await;
        assert_eq!(
            intents.len(),
            1,
            "per-job mints for its queued job whatever is registered"
        );

        // The contrast that keeps the assertion above from being vacuous: the very
        // same list *does* move the needle in demand mode, where the idle runner is
        // counted as supply and suppresses the mint. So per-job's indifference is a
        // real property of the mode, not an inert list.
        let fx = Fixture::new();
        let intents = process_run_jobs(
            &fx.ctx(),
            "owner/demo",
            &runs(),
            &Codes::new(),
            &runners(),
            ProvisioningMode::Demand,
            &mut HashSet::new(),
            now,
        )
        .await;
        assert!(
            intents.is_empty(),
            "demand mode reads the same list as one unit of supply"
        );
    }

    #[tokio::test]
    async fn demand_does_not_spend_supply_on_an_already_claimed_job() {
        // A job this instance already owns is skipped by the claim dedup before the
        // supply check, so it never consumes a supply slot — the one idle runner is
        // credited against the *other*, unclaimed job instead.
        let fx = Fixture::new();
        {
            let mut map = fx.inflight.lock().await;
            map.insert(
                1,
                Claim {
                    repo: "owner/demo".to_string(),
                    profile: "linux-docker".to_string(),
                    pr: None,
                    branch: None,
                    workflow: None,
                    job_name: "build".to_string(),
                    event: "push".to_string(),
                    started_ms: 0,
                    step: Arc::new(std::sync::Mutex::new(None)),
                    started: Arc::new(AtomicBool::new(false)),
                    cancel: watch::channel(false).0,
                    admitted: Arc::new(AtomicBool::new(false)),
                    containerized: true,
                    runner_name: Arc::new(std::sync::Mutex::new(None)),
                    own_mint: true,
                    image: Arc::new(std::sync::Mutex::new(None)),
                    waiting: Arc::default(),
                    frozen: Arc::new(std::sync::Mutex::new(None)),
                    job_over: Arc::new(AtomicBool::new(false)),
                    footprint: Arc::default(),
                },
            );
        }
        let rc = fx.ctx();
        let runs = vec![run_with_job(1, "queued"), run_with_job(2, "queued")];
        let runners = vec![docker_runner("faber-y-1", false, "online")];
        let mut seen = HashSet::new();
        let intents = process_run_jobs(
            &rc,
            "owner/demo",
            &runs,
            &Codes::new(),
            &runners,
            ProvisioningMode::Demand,
            &mut seen,
            0,
        )
        .await;
        assert!(
            intents.is_empty(),
            "job 1 is already ours (no slot spent); job 2 is covered by the idle runner"
        );
    }

    // --- a runner's age starts when it registers (ADR-0018) --------------------
    //
    // Connecting supply, the stale sweep and the sweep's busy deferral all age a runner
    // by the timestamp in its name, on this host and on every peer. That timestamp used
    // to be read when the provision task started, before a claim parks for memory, so
    // a runner whose claim had parked registered already past those windows.

    /// One `generate-jitconfig` request as the stand-in API received it: the name
    /// faber asked GitHub to register, when it asked, and what the claim's shared
    /// cells said at that moment.
    #[derive(Debug, Clone)]
    struct JitRequest {
        name: String,
        at_ms: u64,
        admitted: bool,
        published: Option<String>,
    }

    /// A stand-in for api.github.com that records every `generate-jitconfig` request
    /// and refuses it with a `503`, so a provision ends at the JIT mint as a backend
    /// error, with nothing started on the host. Anything else gets a `404` and is not
    /// recorded.
    async fn spawn_jit_api(
        admitted: Arc<AtomicBool>,
        published: Arc<std::sync::Mutex<Option<String>>>,
    ) -> (String, Arc<std::sync::Mutex<Vec<JitRequest>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                // The head, then as much body as its `Content-Length` promises.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break (head, buf[end + 4..end + 4 + len].to_vec());
                        }
                    }
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break (String::new(), Vec::new()),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let at_ms = now_ms();
                let mint = head
                    .lines()
                    .next()
                    .is_some_and(|l| l.contains("/actions/runners/generate-jitconfig"));
                let name = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["name"].as_str().map(str::to_string));
                let status = match (mint, name) {
                    (true, Some(name)) => {
                        log.lock().unwrap().push(JitRequest {
                            name,
                            at_ms,
                            admitted: admitted.load(Ordering::SeqCst),
                            published: published.lock().unwrap().clone(),
                        });
                        "503 Service Unavailable"
                    }
                    _ => "404 Not Found",
                };
                let body = r#"{"message":"stand-in"}"#;
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, seen)
    }

    /// The regression behind ADR-0018, through the real provision task.
    ///
    /// A claim parks in memory admission, then registers its runner. Every window that
    /// ages a runner reads the timestamp in its name, so this pins each one at its
    /// edge, counted from the moment admission was granted: the runner still covers
    /// its job at the connecting grace, is still unswept at the stale TTL, and is
    /// still deferred at the busy grace. Named when the claim was taken, the same
    /// runner is past every edge by the length of the park, so a park of any length
    /// fails this. The park here is a few milliseconds; live ones ran to many
    /// minutes (see the next test for what that did).
    #[tokio::test]
    async fn a_parked_claims_runner_is_aged_from_registration_not_from_its_claim() {
        const JOB: u64 = 77;
        let admitted = Arc::new(AtomicBool::new(false));
        let published = Arc::new(std::sync::Mutex::new(None));
        let (base, requests) = spawn_jit_api(admitted.clone(), published.clone()).await;
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        let profile = Profile {
            name: "linux-docker".to_string(),
            labels: labels.clone(),
            backend: Backend::Docker(crate::docker::DockerConfig::new(
                "ghcr.io/owner/my-runner:latest".to_string(),
            )),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels.clone()),
        };
        let (orch, _tmp) = test_orch_on(vec![profile], Github::at(base));

        // The whole budget is already spoken for, so the claim parks in admission.
        orch.limiter.set_vm_total_for_test(20_480);
        let host_full = orch.limiter.adopt(Reservation {
            mib: orch.limiter.snapshot().budget_mib,
            mcpu: 0,
            disk_mib: 0,
            shadow_mib: orch.limiter.snapshot().budget_mib,
        });
        orch.inflight.lock().await.insert(
            JOB,
            Claim {
                repo: "owner/demo".to_string(),
                profile: "linux-docker".to_string(),
                pr: None,
                branch: None,
                workflow: None,
                job_name: "build".to_string(),
                event: "push".to_string(),
                started_ms: now_ms(),
                step: Arc::new(std::sync::Mutex::new(None)),
                started: Arc::new(AtomicBool::new(false)),
                cancel: watch::channel(false).0,
                admitted: admitted.clone(),
                containerized: true,
                runner_name: published.clone(),
                own_mint: true,
                image: Arc::new(std::sync::Mutex::new(None)),
                waiting: Arc::default(),
                frozen: Arc::new(std::sync::Mutex::new(None)),
                job_over: Arc::new(AtomicBool::new(false)),
                footprint: Arc::default(),
            },
        );
        let task = spawn_provision(
            orch.clone(),
            "owner/demo".to_string(),
            ProvisionIntent {
                job_id: JOB,
                profile_idx: 0,
                job_name: "build".to_string(),
                workflow_id: 900,
                workflow_name: None,
                run_id: 900,
                // No SHA: the surface-only drift check is skipped, so the stand-in
                // API sees the registration and nothing else.
                head_sha: String::new(),
                content_ref: String::new(),
                pr: None,
                run_created_at: None,
                attempt: 1,
                job_labels: labels,
            },
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while orch.limiter.parked() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the claim parks in admission");
        // Let the clock move past anything the task stamped on its way in.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            requests.lock().unwrap().is_empty(),
            "nothing registers while the claim is parked"
        );
        let named_while_parked = published.lock().unwrap().clone();

        let admitted_at = now_ms();
        drop(host_full);
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("the provision ends at the refused registration")
            .expect("provision task");

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "exactly one registration: {requests:?}");
        let registered = &requests[0];
        let minted = faber_mint_ts(&registered.name).expect("a faber-minted name");
        assert_eq!(faber_job_id(&registered.name), Some(JOB));

        let offline = Runner {
            id: 1,
            ..docker_runner(&registered.name, false, "offline")
        };
        let busy = Runner {
            busy: true,
            ..offline.clone()
        };
        let grace = CONNECTING_GRACE.as_millis() as u64;
        let ttl = STALE_RUNNER_TTL.as_millis() as u64;
        let busy_grace = BUSY_RUNNER_GRACE.as_millis() as u64;

        // Still connecting at the edge of the grace: the runner covers its own job,
        // so demand mints no duplicate for it.
        assert_eq!(
            demand_mints(JOB, &offline, admitted_at + grace).await,
            0,
            "a runner whose claim parked must count as connecting supply for the whole \
             grace after it registered (name {}, admitted at {admitted_at})",
            registered.name
        );
        // At the edge of the stale TTL the sweep leaves it alone.
        assert!(
            stale_runner_ids(std::slice::from_ref(&offline), admitted_at + ttl).is_empty(),
            "a runner whose claim parked must not be swept until the stale TTL has passed \
             since it registered (name {}, admitted at {admitted_at})",
            registered.name
        );
        // And one GitHub still reports busy is deferred, not attempted, for the whole
        // busy grace.
        let at_busy_grace = admitted_at + busy_grace;
        assert!(stale_runner_ids(std::slice::from_ref(&busy), at_busy_grace).is_empty());
        assert_eq!(
            deferred_busy_runner_ids(std::slice::from_ref(&busy), at_busy_grace),
            vec![1]
        );

        // The timestamp is the registration's: after the park, and no later than the
        // request. A future one would let a dead registration mask its job.
        assert!(minted >= admitted_at && minted <= registered.at_ms);
        // ADR-0017's half: a parked claim names no runner, and by the time GitHub is
        // asked to register one the claim is admitted and names exactly that runner.
        assert_eq!(named_while_parked, None);
        assert!(registered.admitted);
        assert_eq!(
            registered.published.as_deref(),
            Some(registered.name.as_str())
        );
        // #105: the mint is on faber's own record, exactly as registered — the line a
        // later process vouches for this runner by, and for nothing else.
        let mints = mint_records(&orch.audit.read_since(0));
        assert_eq!(
            mints,
            HashSet::from([("owner/demo".to_string(), registered.name.clone())])
        );
    }

    /// The audit line a test writes for a mint, or for any other `kind`.
    fn audit_entry(kind: &str, repo: &str, runner: &str) -> crate::audit::Entry {
        serde_json::from_value(serde_json::json!({
            "ts_ms": 1,
            "kind": kind,
            "repo": repo,
            "job_id": 7,
            "runner": runner,
        }))
        .expect("an audit line")
    }

    /// A container as `docker::scan` would report it: named `container`, claiming
    /// `repo`, and in every other respect faber's own.
    fn adopted(container: &str, repo: &str) -> crate::docker::Adopted {
        let line = serde_json::json!({
            "Name": container,
            "Running": true,
            "StartedAt": "2026-08-28T10:00:00Z",
            "Memory": 1073741824i64,
            "NanoCpus": 0,
            "Image": "ghcr.io/owner/my-runner:latest",
            "Labels": {
                "org.faber.repo": repo,
                "org.faber.run-id": "900",
                "org.faber.provision-attempt": "1",
                "org.faber.profile": "linux-docker",
                "org.faber.class": format!("{repo}::900::build"),
                "org.faber.reserved-mib": "1024",
                "org.faber.serves": "",
            }
        });
        crate::docker::parse_adopted(&line.to_string()).expect("adoptable")
    }

    /// #105's third rule after a restart: an adopted container gets a GitHub
    /// identity only from a `mint` line of faber's own that names it exactly. A
    /// container labelled as another host's runner, or claiming another repo, or
    /// another claim's runner, gets none — so no GitHub write can ever target it.
    #[test]
    fn an_adopted_container_is_vouched_for_only_by_its_own_mint_record() {
        let records = mint_records(&[
            audit_entry("mint", "owner/demo", "faber-7-1000"),
            // Recorded, but as an outcome: not a mint.
            audit_entry("outcome", "owner/demo", "faber-8-2000"),
        ]);

        let ours = vouched_mint(&records, &adopted("/faber-runner-7-1000", "owner/demo"))
            .expect("faber's own mint vouches for it");
        assert_eq!((ours.repo(), ours.name()), ("owner/demo", "faber-7-1000"));

        for (container, repo, why) in [
            // A peer minting for the same job differs only in the timestamp.
            (
                "/faber-runner-7-1001",
                "owner/demo",
                "another host's runner",
            ),
            (
                "/faber-runner-7-1000",
                "owner/other",
                "this host's runner, another repo",
            ),
            (
                "/faber-runner-8-2000",
                "owner/demo",
                "another claim's runner",
            ),
            (
                "/faber-runner-9-3000",
                "owner/demo",
                "a runner nobody recorded",
            ),
        ] {
            assert_eq!(
                vouched_mint(&records, &adopted(container, repo)),
                None,
                "{why}"
            );
        }
    }

    /// And without that identity the claim is left out of GitHub-side reconciling
    /// altogether: no job fetch, no runner listing, no reclaim request.
    #[tokio::test]
    async fn a_claim_without_its_own_mint_record_is_never_reconciled_against_github() {
        let (base, seen) = spawn_recording_api().await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        let (tx, rx) = watch::channel(false);
        let claim = Claim {
            own_mint: false,
            ..drain_claim(tx, true)
        };
        *claim.runner_name.lock().unwrap() = Some("faber-7-1000".to_string());
        orch.inflight.lock().await.insert(7, claim);
        reconcile_claims(&orch, &HashSet::new()).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing may be asked of GitHub"
        );
        assert!(!*rx.borrow(), "and no reclaim requested");

        // The control: the same claim with its own record is reconciled as ever.
        let ours = drain_claim(watch::channel(false).0, true);
        *ours.runner_name.lock().unwrap() = Some("faber-8-2000".to_string());
        orch.inflight.lock().await.insert(8, ours);
        reconcile_claims(&orch, &HashSet::new()).await;
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|r| r.contains("/actions/jobs/8")),
            "a claim of faber's own is still checked against its job: {seen:?}"
        );
        assert!(
            !seen.iter().any(|r| r.contains("/actions/jobs/7")),
            "{seen:?}"
        );
    }

    /// A claim reads `running` once its runner is busy, whichever job it holds.
    /// A JIT runner is not pinned to the job it was minted for, so a claim's own
    /// job can stay queued, or finish on a peer, while its runner runs a sibling.
    /// Only a runner seen idle leaves the claim `starting`.
    #[tokio::test]
    async fn a_claim_whose_runner_is_busy_on_any_job_reads_running() {
        let (base, _seen) = spawn_routed_api(|request| {
            if request.starts_with("GET /repos/owner/demo/actions/jobs/7 ") {
                ("200 OK", r#"{"id":7,"name":"build","status":"queued"}"#)
            } else if request.starts_with("GET /repos/owner/demo/actions/jobs/8 ") {
                (
                    "200 OK",
                    r#"{"id":8,"name":"build","status":"completed","conclusion":"success","runner_name":"faber-8-9999"}"#,
                )
            } else if request.starts_with("GET /repos/owner/demo/actions/jobs/9 ") {
                ("200 OK", r#"{"id":9,"name":"build","status":"queued"}"#)
            } else if request.starts_with("GET /repos/owner/demo/actions/runners?per_page=100 ") {
                (
                    "200 OK",
                    r#"{"total_count":3,"runners":[
                        {"id":1,"name":"faber-7-1000","status":"online","busy":true,"labels":[]},
                        {"id":2,"name":"faber-8-2000","status":"online","busy":true,"labels":[]},
                        {"id":3,"name":"faber-9-3000","status":"online","busy":false,"labels":[]}
                    ]}"#,
                )
            } else {
                ("404 Not Found", r#"{"message":"stand-in"}"#)
            }
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        for (job, runner) in [
            (7, "faber-7-1000"),
            (8, "faber-8-2000"),
            (9, "faber-9-3000"),
        ] {
            let claim = drain_claim(watch::channel(false).0, true);
            *claim.runner_name.lock().unwrap() = Some(runner.to_string());
            orch.inflight.lock().await.insert(job, claim);
        }
        let phases = |view: StateView| -> Vec<(u64, JobPhase)> {
            view.inflight.iter().map(|v| (v.job_id, v.phase)).collect()
        };
        assert_eq!(
            phases(orch.view().await),
            vec![
                (7, JobPhase::Starting),
                (8, JobPhase::Starting),
                (9, JobPhase::Starting),
            ],
            "nothing is known before a pass"
        );

        reconcile_claims(&orch, &HashSet::new()).await;
        let want = vec![
            // Own job queued, runner busy on a sibling.
            (7, JobPhase::Running),
            // Own job finished on a peer's runner, this one busy on a sibling.
            (8, JobPhase::Running),
            // Own job queued, runner online and idle.
            (9, JobPhase::Starting),
        ];
        assert_eq!(phases(orch.view().await), want);
    }

    /// #105's third rule through adoption itself. The container a `mint` line of
    /// faber's own names is adopted with that record: its claim is reconciled, and
    /// its reclaim withdraws exactly the registration the line names. The one no
    /// line names is adopted all the same, but with none, and nothing about it
    /// reaches GitHub: no reconcile and no withdrawal.
    #[tokio::test]
    async fn adoption_hands_each_container_only_the_mint_record_that_names_it() {
        let (base, seen) = spawn_routed_api(|request| {
            if request.starts_with("GET /repos/owner/demo/actions/runners?name=faber-7-1000 ") {
                (
                    "200 OK",
                    r#"{"total_count":1,"runners":[{"id":30543,"name":"faber-7-1000","status":"online","busy":false,"labels":[]}]}"#,
                )
            } else if request.starts_with("DELETE /repos/owner/demo/actions/runners/30543 ") {
                ("200 OK", "{}")
            } else {
                ("404 Not Found", r#"{"message":"stand-in"}"#)
            }
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        orch.audit
            .append(&Record::mint(
                "owner/demo",
                7,
                "faber-7-1000",
                "linux-docker",
            ))
            .await
            .expect("audit append");
        let scan = crate::docker::Scan {
            adoptable: vec![
                adopted("/faber-runner-7-1000", "owner/demo"),
                adopted("/faber-runner-8-2000", "owner/demo"),
            ],
            opaque: Vec::new(),
        };
        let mut handed = HashMap::new();
        let adopted_n = orch
            .adopt_scan(scan, |_, a, minted, _, _| {
                handed.insert(a.job_id, minted);
            })
            .await;
        assert_eq!(adopted_n, 2, "both are adopted");
        {
            let map = orch.inflight.lock().await;
            assert!(map[&7].own_mint, "vouched for by faber's own mint line");
            assert!(!map[&8].own_mint, "no line names it");
        }

        let ours = handed.remove(&7).expect("handed on");
        assert_eq!(
            ours.as_ref().map(|m| (m.repo(), m.name())),
            Some(("owner/demo", "faber-7-1000"))
        );
        assert_eq!(
            crate::docker::withdraw_minted(&orch.gh, ours, crate::docker::RECLAIM_WITHDRAWAL).await,
            crate::docker::Withdrawal::Withdrawn
        );
        let requests = std::mem::take(&mut *seen.lock().unwrap());
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(
            requests[0].starts_with("GET /repos/owner/demo/actions/runners?name=faber-7-1000 "),
            "found by the name the record holds: {requests:?}"
        );
        assert!(
            requests[1].starts_with("DELETE /repos/owner/demo/actions/runners/30543 "),
            "and withdrawn by the id GitHub gave for it: {requests:?}"
        );

        let theirs = handed.remove(&8).expect("handed on");
        assert_eq!(theirs, None);
        assert!(matches!(
            crate::docker::withdraw_minted(&orch.gh, theirs, crate::docker::RECLAIM_WITHDRAWAL)
                .await,
            crate::docker::Withdrawal::Failed(_)
        ));
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing is sent about a runner no record names"
        );

        reconcile_claims(&orch, &HashSet::new()).await;
        let requests = seen.lock().unwrap().clone();
        assert!(
            requests.iter().any(|r| r.contains("/actions/jobs/7")),
            "{requests:?}"
        );
        assert!(
            !requests.iter().any(|r| r.contains("/actions/jobs/8")),
            "{requests:?}"
        );
    }

    /// A stand-in for api.github.com that records each request line and answers
    /// every one with a `404`.
    async fn spawn_recording_api() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        spawn_routed_api(|_| ("404 Not Found", r#"{"message":"stand-in"}"#)).await
    }

    /// A stand-in for api.github.com that records each request line and answers
    /// it with whatever `route` gives for that line: a status and a JSON body.
    async fn spawn_routed_api(
        route: fn(&str) -> (&'static str, &'static str),
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let request = head.lines().next().unwrap_or_default().to_string();
                let (status, body) = route(&request);
                log.lock().unwrap().push(request);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, seen)
    }

    /// How many runners demand provisioning mints for job `job`, queued alone, when
    /// `runner` is the only one registered.
    async fn demand_mints(job: u64, runner: &Runner, now: u64) -> usize {
        let fx = Fixture::new();
        process_run_jobs(
            &fx.ctx(),
            "owner/demo",
            &[run_with_job(job, "queued")],
            &Codes::new(),
            std::slice::from_ref(runner),
            ProvisioningMode::Demand,
            &mut HashSet::new(),
            now,
        )
        .await
        .len()
    }

    /// The incident behind ADR-0018, in its own proportions: a claim parked for
    /// over ten minutes, and a sweep ran a second or so after its runner registered,
    /// before the container had even launched. Named when the claim was taken, the
    /// runner was already past both windows: demand read it as dead and minted a
    /// duplicate, and the sweep deleted its registration. Named at registration, it
    /// is a runner connecting.
    ///
    /// Moving the origin does not loosen either bound. A registration that never
    /// connects stops covering its job and is reaped on the same schedule as
    /// before, counted from registration, so it still cannot starve a queued job
    /// (ADR-0004).
    #[tokio::test]
    async fn a_795_second_park_no_longer_ages_its_runner_past_both_windows() {
        const JOB: u64 = 77;
        const CLAIMED: u64 = 1_700_000_000_000;
        let registered = CLAIMED + 795_000;
        let swept = registered + 1_400;
        let named = |minted: u64| Runner {
            id: 1,
            ..docker_runner(&format!("faber-{JOB}-{minted}"), false, "offline")
        };

        assert_eq!(demand_mints(JOB, &named(registered), swept).await, 0);
        assert!(stale_runner_ids(&[named(registered)], swept).is_empty());
        // The same runner, named when its claim was taken.
        assert_eq!(demand_mints(JOB, &named(CLAIMED), swept).await, 1);
        assert_eq!(stale_runner_ids(&[named(CLAIMED)], swept), vec![1]);

        // It never connects: both bounds close on schedule, counted from registration.
        let grace = CONNECTING_GRACE.as_millis() as u64;
        let ttl = STALE_RUNNER_TTL.as_millis() as u64;
        assert_eq!(
            demand_mints(JOB, &named(registered), registered + grace).await,
            0
        );
        assert_eq!(
            demand_mints(JOB, &named(registered), registered + grace + 1).await,
            1
        );
        assert!(stale_runner_ids(&[named(registered)], registered + ttl).is_empty());
        assert_eq!(
            stale_runner_ids(&[named(registered)], registered + ttl + 1),
            vec![1]
        );
    }

    // --- graceful-drain contract -------------------------------------------
    //
    // On shutdown `begin_drain` cancels every parked claim (no container yet) and
    // `await_drain` waits only for claims that still back a live runner container.
    // A parked claim can be held on limiter admission indefinitely, and an admitted
    // one can outlive its container (issue #16) — if drain waited for either, the
    // daemon would hang until launchd's `ExitTimeOut` force-killed it, orphaning
    // exactly the runners a graceful drain exists to protect. These pin the "wait
    // only for work that is actually running" contract, in both directions: an
    // unreadable probe, and a claim that has no container to look for, must
    // still be waited on.

    /// A bare `Orchestrator` for the drain tests: real `inflight` / `draining` /
    /// `runner_done` state, dummy GitHub/limiter/audit it never touches. Built as a
    /// struct literal rather than via `new()`, which resolves the account login
    /// over the network. The returned `TempDir` guards the (unused) audit path and
    /// must be kept alive for the test's duration.
    pub(crate) fn test_orch() -> (Arc<Orchestrator>, TempDir) {
        test_orch_with(Vec::new())
    }

    /// [`test_orch`] with routing profiles, for the tests that need the gate
    /// boundary (which is read off them).
    pub(crate) fn test_orch_with(profiles: Vec<Profile>) -> (Arc<Orchestrator>, TempDir) {
        test_orch_on(
            profiles,
            Github::new("test-token".to_string()).expect("build github client"),
        )
    }

    /// An unconfirmed token that GitHub rejects is not sent again every cycle:
    /// it falls under the rejected token's throttle.
    #[tokio::test]
    async fn rejected_unconfirmed_token_is_not_resent_each_cycle() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                counter.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"message":"Bad credentials"}"#;
                let resp = format!(
                    "HTTP/1.1 401 X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let gh = Github::at(base);
        gh.mark_unconfirmed();
        let (orch, _tmp) = test_orch_on(Vec::new(), gh);
        reconcile_credential(&orch).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "checked once");
        assert!(orch.gh.is_rejected());
        reconcile_credential(&orch).await;
        reconcile_credential(&orch).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a rejected token waits out the throttle"
        );
    }

    /// A local API that answers every request with `status` and `body`, and
    /// counts them.
    async fn counting_api(status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                counter.fetch_add(1, Ordering::SeqCst);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, hits)
    }

    /// A new token GitHub rejects while an ActsAsOther block stands is not
    /// sent again every cycle: the block keeps its reason, but the token falls
    /// under the rejected token's throttle all the same.
    #[tokio::test]
    async fn rejected_token_under_acts_as_other_is_not_resent_each_cycle() {
        let (base, hits) = counting_api(401, r#"{"message":"Bad credentials"}"#).await;
        let gh = Github::at(base);
        assert!(gh.acts_as_other("intruder", "owner"));
        // A different token taken from the store, as `take_from_store` leaves it.
        gh.mark_unconfirmed();
        let (orch, _tmp) = test_orch_on(Vec::new(), gh);
        reconcile_credential(&orch).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1, "checked once");
        assert!(orch.gh.is_rejected());
        reconcile_credential(&orch).await;
        reconcile_credential(&orch).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a rejected token waits out the throttle"
        );
        let why = orch.gh.claim_block().expect("claims are blocked");
        assert!(why.contains("acts as intruder"), "{why}");
    }

    /// A token taken from the store blocks claims until `GET /user` says it
    /// acts as the served owner, and that check comes on the next cycle.
    #[tokio::test]
    async fn an_unconfirmed_token_claims_nothing_until_github_confirms_it() {
        let (base, hits) = counting_api(200, r#"{"login":"owner"}"#).await;
        let gh = Github::at(base);
        gh.mark_unconfirmed();
        let (orch, _tmp) = test_orch_on(Vec::new(), gh);
        assert_eq!(
            orch.gh.claim_block().as_deref(),
            Some(crate::credential::UNCONFIRMED)
        );
        reconcile_credential(&orch).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!orch.gh.unconfirmed());
        assert_eq!(orch.gh.claim_block(), None);
    }

    /// A check that cannot reach GitHub leaves the token unconfirmed and
    /// claims blocked, and the next cycle asks again rather than waiting out
    /// the rejected token's throttle.
    #[tokio::test]
    async fn an_unconfirmed_token_is_checked_again_next_cycle() {
        let (orch, _tmp) = test_orch_on(Vec::new(), Github::at("http://127.0.0.1:9".to_string()));
        orch.gh.mark_unconfirmed();
        reconcile_credential(&orch).await;
        assert!(orch.gh.unconfirmed());
        assert!(orch.gh.claim_block().is_some());
        let (base, _hits) = counting_api(200, r#"{"login":"owner"}"#).await;
        let gh = Github::at(base);
        gh.mark_unconfirmed();
        let (orch, _tmp2) = test_orch_on(Vec::new(), gh);
        // The throttle was started by the failed check, as on the first cycle.
        orch.credential_checks.lock().unwrap().rejected_retry_ms = now_ms();
        reconcile_credential(&orch).await;
        assert_eq!(orch.gh.claim_block(), None, "not held by the throttle");
    }

    /// [`test_orch_with`] talking to `gh`, for a test that drives a provision far
    /// enough to reach GitHub (see [`Github::at`]).
    fn test_orch_on(profiles: Vec<Profile>, gh: Github) -> (Arc<Orchestrator>, TempDir) {
        test_orch_full(profiles, gh, HostPolicy::default())
    }

    /// [`test_orch`] with its own `[host]` policy, handed to the limiter the way
    /// [`Orchestrator::new`] does.
    fn test_orch_host(host: HostPolicy) -> (Arc<Orchestrator>, TempDir) {
        test_orch_full(
            Vec::new(),
            Github::new("test-token".to_string()).expect("build github client"),
            host,
        )
    }

    fn test_orch_full(
        profiles: Vec<Profile>,
        gh: Github,
        host: HostPolicy,
    ) -> (Arc<Orchestrator>, TempDir) {
        test_orch_waste(profiles, gh, host, Default::default())
    }

    /// [`test_orch_full`] with its own `[waste]` policy (ADR-0063).
    fn test_orch_waste(
        profiles: Vec<Profile>,
        gh: Github,
        host: HostPolicy,
        waste: crate::config::WastePolicy,
    ) -> (Arc<Orchestrator>, TempDir) {
        let tmp = TempDir::new();
        let audit = Audit::at(tmp.path.join("audit.jsonl"));
        let autolimit = AutoLimitConfig {
            reserve_mib: 2048,
            headroom_mib: 1024,
            band_mib: 512,
            profiling_cap_mib: 4096,
            margin: 1.5,
            escalation_frac: 0.5,
            window: 3,
            native_concurrency: 1,
            probe_image: "busybox".to_string(),
            hard_ceiling: crate::config::HardCeiling::Off,
            cpu: crate::config::CpuConfig::default(),
            reserve: crate::config::ReserveSetting::Unstated,
            priority_repos: Vec::new(),
        };
        let limiter = Limiter::new(autolimit.clone(), &HashMap::new());
        limiter.set_disk_admission(host.disk_admission());
        // Every Docker profile's image as the first cycle's check finds it on a
        // working host: present. Otherwise each claim would wait on a check no
        // test runs (#156); a test about the image sets its own state.
        let mut profile_images = ProfileImages::default();
        for p in &profiles {
            if let Backend::Docker(d) = &p.backend {
                profile_images.present(&p.name, &d.image);
            }
        }
        let orch = Arc::new(Orchestrator {
            gh: Arc::new(gh),
            cfg: Config {
                topic: "faber".to_string(),
                poll_interval: Duration::from_secs(30),
                autolimit,
                pinned_caps: HashMap::new(),
                hold: None,
                repo_holds: Vec::new(),
                profiles,
                playwright_drift: DriftPolicy::default(),
                retry: RetryPolicy::default(),
                host,
                runner_image: RunnerImagePolicy::default(),
                needs: HashMap::new(),
                provisioning: ProvisioningMode::default(),
                services: Default::default(),
                mirror: None,
                vm_layout: Default::default(),
                waste,
            },
            owner: "owner".to_string(),
            inflight: Mutex::new(HashMap::new()),
            idle_since: Mutex::new(HashMap::new()),
            retried: Mutex::new(RetryMemory::default()),
            ends: Mutex::new(EndLedger::default()),
            executed: Mutex::new(HashMap::new()),
            cohorts: Mutex::new(HashMap::new()),
            limiter,
            draining: AtomicU8::new(0),
            wake: Notify::new(),
            runner_done: Notify::new(),
            metrics: Mutex::new(Metrics::default()),
            audit,
            waste: Default::default(),
            started_ms: now_ms(),
            health: Mutex::new(None),
            docker_custody: Mutex::new(host::DockerCustody::default()),
            images: Arc::new(VariantCache::new(
                RunnerImagePolicy::default(),
                VariantDisk::new(HostPolicy::default().disk_floor_gib, None),
            )),
            minted: std::sync::Mutex::new(HashMap::new()),
            repo_holds: std::sync::Mutex::new(BTreeMap::new()),
            cache_purges: std::sync::Mutex::new(BTreeSet::new()),
            profile_images: std::sync::Mutex::new(profile_images),
            retirements: std::sync::Mutex::new(Retirements::default()),
            verdicts_running: Arc::new(AtomicBool::new(false)),
            verdict_early_ms: AtomicU64::new(0),
            instance: "builder".to_string(),
            peers: std::sync::Mutex::new(Peers::default()),
            persist_holds: false,
            awake: std::sync::Mutex::new(crate::awake::AwakeView::booted(
                crate::config::SleepPolicy::Off,
            )),
            credential_checks: std::sync::Mutex::new(CredentialChecks::default()),
        });
        (orch, tmp)
    }

    /// The boundary line is the only record of what the gate was enforcing when a
    /// batch of refusals was written (issue #42), so its shape is pinned rather
    /// than assumed — and an empty profile list must produce no line at all
    /// rather than a record claiming a boundary nobody configured.
    #[tokio::test]
    async fn the_boundary_is_written_to_the_audit_log_at_boot() {
        let (orch, tmp) = test_orch();
        orch.record_boundary().await;
        assert!(
            audit_lines(&tmp).is_empty(),
            "no profiles means no boundary to report"
        );

        let (gated, tmp2) = test_orch_with(vec![Profile {
            name: "n".to_string(),
            labels: vec!["faber".to_string()],
            backend: vm_backend(),
            policy: crate::gate::TrustPolicy::new([], [], vec!["faber".to_string()])
                .with_boundary(["push".to_string(), "schedule".to_string()], true),
        }]);
        gated.record_boundary().await;

        let lines = audit_lines(&tmp2);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["kind"], "gate");
        assert_eq!(lines[0]["verdict"], "boundary:push,schedule");
        assert_eq!(lines[0]["action"], "public_repos=true fork_prs=false");
    }

    /// Read back every line the test orchestrator has appended.
    fn audit_lines(tmp: &TempDir) -> Vec<serde_json::Value> {
        std::fs::read_to_string(tmp.path.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("a well-formed audit line"))
            .collect()
    }

    #[tokio::test]
    async fn an_outcome_names_the_image_that_ran_and_the_content_it_resolved_to() {
        // ADR-0008 made the image a function of the job, so "which image ran this"
        // stopped being answerable from the config — and a tag alone does not answer
        // it either, because a tag is a mutable pointer. Recording the pair is what
        // lets a job that died on a missing toolchain name the content that served
        // it, instead of leaving a reader to inspect a host hours later and hope
        // nothing moved.
        let (orch, tmp) = test_orch();
        let report = RunReport {
            job_ok: true,
            image: Some("faber-runner:0123456789ab-pw-1.61.1".to_string()),
            image_id: Some("sha256:fedcba987654".to_string()),
            ..Default::default()
        };
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 7,
                executed_job_id: Some(9),
                class: None,
                label: Some("o/r · ci · visual"),
                profile: "linux-docker",
                runner: "faber-7-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 1_000,
            },
            &report,
        )
        .await;

        let lines = audit_lines(&tmp);
        assert_eq!(lines.len(), 1);
        let rec = &lines[0];
        assert_eq!(rec["image"], "faber-runner:0123456789ab-pw-1.61.1");
        assert_eq!(rec["image_id"], "sha256:fedcba987654");
        // The join that makes the pair worth having: a runner is not job-pinned, so
        // the image has to be readable next to the job that *actually* ran on it.
        assert_eq!(rec["executed_job_id"], 9);
        assert_eq!(rec["runner"], "faber-7-1000");
        // ADR-0046 §9: absent when the runner had no services.
        assert!(rec.get("services").is_none(), "{rec}");
    }

    #[tokio::test]
    async fn an_outcome_names_the_services_the_runner_had() {
        let (orch, tmp) = test_orch();
        let report = RunReport {
            job_ok: true,
            services: vec!["postgres-16".to_string(), "redis".to_string()],
            ..Default::default()
        };
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 7,
                executed_job_id: None,
                class: None,
                label: None,
                profile: "linux-docker",
                runner: "faber-7-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 1_000,
            },
            &report,
        )
        .await;
        let lines = audit_lines(&tmp);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0]["services"],
            serde_json::json!(["postgres-16", "redis"])
        );
    }

    #[tokio::test]
    async fn an_outcome_omits_the_image_when_no_container_ever_ran() {
        // Absent must read as "there was no container to ask", never as an empty
        // tag — a macOS VM job and a ramp that failed before launch both have nothing
        // truthful to say here, and a blank string would look like an answer.
        let (orch, tmp) = test_orch();
        let report = RunReport {
            infra_error: true,
            cause: Some("starting dind faber-dind-7: docker run failed".to_string()),
            ..Default::default()
        };
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 7,
                executed_job_id: None,
                class: None,
                label: None,
                profile: "macos",
                runner: "faber-7-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 1_000,
            },
            &report,
        )
        .await;

        let rec = &audit_lines(&tmp)[0];
        assert!(
            rec.get("image").is_none(),
            "an absent image must be omitted"
        );
        assert!(
            rec.get("image_id").is_none(),
            "an absent image id must be omitted"
        );
    }

    /// A claim for job `job` carrying `timeline`, for the outcome tests below.
    fn footprint_claim(timeline: Timeline) -> Claim {
        Claim {
            repo: "owner/demo".to_string(),
            profile: "linux-docker".to_string(),
            pr: None,
            branch: None,
            workflow: None,
            job_name: "build".to_string(),
            event: "push".to_string(),
            started_ms: 0,
            step: Arc::new(std::sync::Mutex::new(None)),
            started: Arc::new(AtomicBool::new(false)),
            cancel: watch::channel(false).0,
            admitted: Arc::new(AtomicBool::new(true)),
            containerized: true,
            runner_name: Arc::new(std::sync::Mutex::new(None)),
            own_mint: true,
            image: Arc::new(std::sync::Mutex::new(None)),
            waiting: Arc::default(),
            frozen: Arc::new(std::sync::Mutex::new(None)),
            job_over: Arc::new(AtomicBool::new(false)),
            footprint: Arc::new(std::sync::Mutex::new(timeline)),
        }
    }

    fn footprint_run(job_id: u64, executed: Option<u64>) -> Finished<'static> {
        Finished {
            repo: "owner/demo",
            job_id,
            executed_job_id: executed,
            class: None,
            label: Some("owner/demo · ci · build"),
            profile: "linux-docker",
            runner: "faber-7-1000",
            attempt: 1,
            wait_ms: 10_000,
            parked: &NOT_PARKED,
            duration_ms: 90_000,
        }
    }

    #[tokio::test]
    async fn an_outcome_records_where_the_run_went() {
        // ADR-0056: the claim's marks, GitHub's times and steps from the poll that
        // saw the job complete, and the sampler's throttle and I/O readings, all on
        // the one line.
        let (orch, tmp) = test_orch();
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 7, "name": "build", "status": "completed",
            "created_at": "2026-03-02T10:00:00Z",
            "started_at": "2026-03-02T10:00:40Z",
            "completed_at": "2026-03-02T10:01:40Z",
            "steps": [
                {"name": "Set up job", "status": "completed", "number": 1,
                 "started_at": "2026-03-02T10:00:40Z", "completed_at": "2026-03-02T10:00:42Z"},
                {"name": "Run cargo test", "status": "completed", "number": 2,
                 "started_at": "2026-03-02T10:00:42Z", "completed_at": "2026-03-02T10:01:38Z"},
                {"name": "Complete job", "status": "completed", "number": 3,
                 "started_at": "2026-03-02T10:01:38Z", "completed_at": "2026-03-02T10:01:40Z"},
            ],
        }))
        .unwrap();
        let t0 = crate::retry::parse_rfc3339_to_ms("2026-03-02T10:00:00Z").unwrap();
        orch.inflight.lock().await.insert(
            7,
            footprint_claim(Timeline {
                seen_ms: Some(t0 + 20_000),
                prepared_ms: Some(t0 + 21_000),
                admitted_ms: Some(t0 + 31_000),
                reserved_mcpu: Some(2_000),
                job: Some(JobTimes::of(&job)),
            }),
        );
        let io = crate::footprint::IoBytes {
            block_read: 1_000,
            block_written: 2_000,
            net_received: 3_000,
            net_sent: 4_000,
        };
        let report = RunReport {
            job_ok: true,
            container_started_ms: Some(t0 + 35_000),
            runner_throttled_usec: Some(1_500_000),
            runner_io: Some(io),
            ..Default::default()
        };
        orch.record_outcome(footprint_run(7, None), &report).await;

        let rec = &audit_lines(&tmp)[0];
        assert_eq!(rec["queued_ms"], t0);
        assert_eq!(rec["seen_ms"], t0 + 20_000);
        assert_eq!(rec["prepared_ms"], t0 + 21_000);
        assert_eq!(rec["admitted_ms"], t0 + 31_000);
        assert_eq!(rec["container_ms"], t0 + 35_000);
        assert_eq!(rec["job_started_ms"], t0 + 40_000);
        assert_eq!(rec["job_completed_ms"], t0 + 100_000);
        assert_eq!(rec["cpu_reserved_mcpu"], 2_000);
        assert_eq!(rec["cpu_throttled_runner_usec"], 1_500_000);
        assert_eq!(rec["io_runner"]["net_sent"], 4_000);
        assert!(
            rec.get("io_dind").is_none(),
            "an unread container is omitted"
        );
        assert!(rec.get("skewed").is_none());
        let buckets: Vec<&str> = rec["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["bucket"].as_str().unwrap())
            .collect();
        assert_eq!(buckets, ["setup", "steps", "teardown"]);
    }

    /// A local stand-in for GitHub that answers any `actions/jobs/{id}` with a
    /// completed job of that id, an empty runners list, and `x-ratelimit-remaining:
    /// {remaining}` when given. Returns its base URL and the request lines it saw.
    async fn spawn_job_api(remaining: Option<u64>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let line = head.lines().next().unwrap_or_default();
                let line = line.rsplit_once(' ').map_or(line, |(l, _)| l).to_string();
                log.lock().unwrap().push(line.clone());
                let body = match line.rsplit_once("/jobs/") {
                    Some((_, id)) => serde_json::json!({
                        "id": id.parse::<u64>().unwrap_or(0), "name": "build",
                        "status": "completed", "conclusion": "success",
                        "created_at": "2026-03-02T10:00:00Z",
                        "started_at": "2026-03-02T10:00:40Z",
                        "completed_at": "2026-03-02T10:01:40Z",
                        "steps": [
                            {"name": "Run cargo test", "status": "completed", "number": 1,
                             "started_at": "2026-03-02T10:00:42Z",
                             "completed_at": "2026-03-02T10:01:38Z"},
                        ],
                    })
                    .to_string(),
                    None => r#"{"runners":[]}"#.to_string(),
                };
                let rate = remaining
                    .map(|n| format!("x-ratelimit-remaining: {n}\r\n"))
                    .unwrap_or_default();
                let resp = format!(
                    "HTTP/1.1 200 X\r\nContent-Type: application/json\r\n{rate}\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, seen)
    }

    fn job_hits(seen: &Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
        seen.lock().unwrap().clone()
    }

    fn started_report() -> RunReport {
        RunReport {
            job_ok: true,
            container_started_ms: Some(1_000),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_unseen_completion_is_read_once_for_the_outcome() {
        // The poll never saw the job complete: one more read of the job that ran
        // gives the line its steps and its completion time.
        let (base, seen) = spawn_job_api(None).await;
        let (orch, tmp) = test_orch_on(Vec::new(), Github::at(base));
        orch.inflight
            .lock()
            .await
            .insert(7, footprint_claim(Timeline::default()));
        orch.record_outcome(footprint_run(7, None), &started_report())
            .await;
        assert_eq!(job_hits(&seen), ["GET /repos/owner/demo/actions/jobs/7"]);
        let rec = &audit_lines(&tmp)[0];
        assert!(rec["job_completed_ms"].as_u64().is_some());
        assert_eq!(rec["steps"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_read_falls_back_to_the_polls_partial_times() {
        // Nothing answers: the line keeps what the poll saw (queue and start
        // times) and carries no steps and no completion.
        let (orch, tmp) = test_orch_on(Vec::new(), Github::at("http://127.0.0.1:9".to_string()));
        let partial: Job = serde_json::from_value(serde_json::json!({
            "id": 7, "name": "build", "status": "in_progress",
            "created_at": "2026-03-02T10:00:00Z",
            "started_at": "2026-03-02T10:00:40Z",
        }))
        .unwrap();
        orch.inflight.lock().await.insert(
            7,
            footprint_claim(Timeline {
                job: Some(JobTimes::of(&partial)),
                ..Timeline::default()
            }),
        );
        orch.record_outcome(footprint_run(7, None), &started_report())
            .await;
        let rec = &audit_lines(&tmp)[0];
        assert!(rec["job_started_ms"].as_u64().is_some());
        assert!(rec.get("job_completed_ms").is_none());
        assert!(rec.get("steps").is_none());
    }

    #[tokio::test]
    async fn a_low_rate_budget_skips_the_outcome_read() {
        let (base, seen) = spawn_job_api(Some(10)).await;
        let gh = Github::at(base);
        // An earlier response reports 10 requests left.
        gh.job("owner/demo", 1).await.unwrap();
        assert!(gh.rate_budget_low());
        let (orch, tmp) = test_orch_on(Vec::new(), gh);
        orch.inflight
            .lock()
            .await
            .insert(7, footprint_claim(Timeline::default()));
        orch.record_outcome(footprint_run(7, None), &started_report())
            .await;
        assert_eq!(job_hits(&seen).len(), 1, "no request for the outcome");
        assert!(audit_lines(&tmp)[0].get("steps").is_none());
    }

    #[tokio::test]
    async fn the_outcome_read_asks_for_the_job_that_ran() {
        let (base, seen) = spawn_job_api(None).await;
        let (orch, _tmp) = test_orch_on(Vec::new(), Github::at(base));
        orch.inflight
            .lock()
            .await
            .insert(7, footprint_claim(Timeline::default()));
        orch.record_outcome(footprint_run(7, Some(8)), &started_report())
            .await;
        assert_eq!(job_hits(&seen), ["GET /repos/owner/demo/actions/jobs/8"]);
    }

    #[tokio::test]
    async fn a_claim_poll_that_sees_completion_stores_it_for_the_outcome() {
        let (base, _seen) = spawn_job_api(None).await;
        let (orch, _tmp) = test_orch_on(Vec::new(), Github::at(base));
        let mut claim = footprint_claim(Timeline::default());
        claim.containerized = false;
        *claim.runner_name.lock().unwrap() = Some("faber-7-1000".to_string());
        let footprint = claim.footprint.clone();
        orch.inflight.lock().await.insert(7, claim);
        reconcile_claims(&orch, &HashSet::new()).await;
        let stored = footprint.lock().unwrap().job.clone().expect("stored");
        assert_eq!(stored.job_id, 7);
        assert!(stored.is_complete());
    }

    #[tokio::test]
    async fn an_outcome_without_a_runner_records_no_steps() {
        // No runner started, so no read is made, and the line says nothing about
        // steps rather than claiming there were none.
        let (orch, tmp) = test_orch();
        orch.inflight.lock().await.insert(
            7,
            footprint_claim(Timeline {
                seen_ms: Some(1_000),
                ..Timeline::default()
            }),
        );
        orch.record_outcome(footprint_run(7, None), &RunReport::default())
            .await;
        let rec = &audit_lines(&tmp)[0];
        assert_eq!(rec["seen_ms"], 1_000);
        for absent in [
            "steps",
            "container_ms",
            "job_started_ms",
            "io_runner",
            "cpu_throttled_runner_usec",
        ] {
            assert!(rec.get(absent).is_none(), "{absent}");
        }
    }

    #[tokio::test]
    async fn steps_polled_for_another_job_are_not_the_executed_jobs() {
        // A JIT runner is not job-pinned: GitHub's times for the job it was minted
        // for say nothing about the job it ran.
        let (orch, tmp) = test_orch();
        let job: Job = serde_json::from_value(serde_json::json!({
            "id": 7, "name": "build", "status": "completed",
            "started_at": "2026-03-02T10:00:40Z",
            "completed_at": "2026-03-02T10:01:40Z",
            "steps": [],
        }))
        .unwrap();
        orch.inflight.lock().await.insert(
            7,
            footprint_claim(Timeline {
                job: Some(JobTimes::of(&job)),
                ..Timeline::default()
            }),
        );
        orch.record_outcome(footprint_run(7, Some(8)), &RunReport::default())
            .await;
        let rec = &audit_lines(&tmp)[0];
        assert!(rec.get("job_started_ms").is_none());
        assert!(rec.get("steps").is_none());
    }

    #[tokio::test]
    async fn an_infra_error_records_what_actually_broke() {
        // The headline property of the whole observability change. Before it, the
        // backend's error was `error!`-logged to stderr and then discarded, and the
        // audit log — faber's stated source of truth — recorded the bare word
        // `infra_error`. Reading the trail back told you a job broke and destroyed
        // the only account of why.
        let (orch, tmp) = test_orch();
        let report = RunReport {
            infra_error: true,
            cause: Some("starting dind faber-dind-7: docker run failed: no space left".to_string()),
            ..Default::default()
        };
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 7,
                executed_job_id: None,
                class: None,
                label: Some("o/r · ci · build"),
                profile: "linux-docker",
                runner: "faber-7-1000",
                attempt: 2,
                wait_ms: 4_000,
                parked: &Parked {
                    by_gate: [("image".to_string(), 500), ("memory".to_string(), 3_500)].into(),
                },
                duration_ms: 31_000,
            },
            &report,
        )
        .await;

        let lines = audit_lines(&tmp);
        assert_eq!(lines.len(), 1);
        let rec = &lines[0];
        assert_eq!(rec["result"], "infra_error");
        assert_eq!(
            rec["cause"], "starting dind faber-dind-7: docker run failed: no space left",
            "the audit line has to carry the cause, not just the category"
        );
        // The joins a reader needs to get from this line to anything else: the
        // runner name reaches GitHub's runner list and the host's containers, the
        // profile names the backend, and the attempt says this had already failed.
        assert_eq!(rec["runner"], "faber-7-1000");
        assert_eq!(rec["profile"], "linux-docker");
        assert_eq!(rec["attempt"], 2);
        assert_eq!(rec["wait_ms"], 4_000);
        // Which gate held it longest, and the whole wait gate by gate (#85).
        assert_eq!(rec["wait_gate"], "memory");
        assert_eq!(rec["wait_gates"]["image"], 500);
        assert_eq!(rec["wait_gates"]["memory"], 3_500);

        // And the same fact is on the panel, without a restart or a log read.
        let view = orch.view().await;
        assert_eq!(view.outcomes.len(), 1);
        assert_eq!(view.outcomes[0].result, "infra_error");
        assert_eq!(view.outcomes[0].wait_gate.as_deref(), Some("memory"));
        assert!(view.outcomes[0]
            .cause
            .as_deref()
            .unwrap()
            .contains("no space left"));
        assert_eq!(view.counts.faults, 1);
    }

    #[tokio::test]
    async fn a_clean_success_explains_nothing() {
        // `cause` is for what went wrong. A success carrying one would put a
        // sentence on every line of a healthy log, and the failures — the reason
        // anyone opens it — would be that much harder to find.
        let (orch, tmp) = test_orch();
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 7,
                executed_job_id: None,
                class: Some("o/r::1::build"),
                label: Some("o/r · ci · build"),
                profile: "linux-docker",
                runner: "faber-7-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 90_000,
            },
            &RunReport {
                job_ok: true,
                ..Default::default()
            },
        )
        .await;
        let lines = audit_lines(&tmp);
        assert_eq!(lines[0]["result"], "success");
        assert!(
            lines[0].get("cause").is_none(),
            "a success must not serialize a cause: {}",
            lines[0]
        );
        // Nor does a job that never parked name a gate.
        assert!(lines[0].get("wait_gate").is_none(), "{}", lines[0]);
        assert!(lines[0].get("wait_gates").is_none(), "{}", lines[0]);
    }

    /// What the retry ledger holds for `runner`, as seen by `job_id`: the first
    /// ending that is not pinned to some other job.
    async fn ending_for(orch: &Orchestrator, runner: &str, job_id: u64) -> Option<EndKind> {
        orch.ends
            .lock()
            .await
            .for_runner(runner)
            .into_iter()
            .find(|e| e.executed_job_id.is_none_or(|ran| ran == job_id))
            .map(|e| e.kind)
    }

    /// The canonical regression fixture for the OOM retry signal, modelled on both
    /// sides of one live event (times relative to the mint):
    ///
    /// ```text
    /// +0 s   faber mints faber-1002-1700000000000 for job 1002
    /// +8 s   GitHub dispatches job 1001 (CI · test) to it instead — a JIT runner
    ///        is not job-pinned
    /// +34 s  the job completes: conclusion "failure", annotations list EMPTY
    /// +40 s  faber records the outcome: oom_runner true, 512 MiB cap
    /// ```
    ///
    /// Two things have to hold at once, and they pull in opposite directions.
    ///
    /// The runner *listener* exited 0 — the kernel took a process inside the job —
    /// so the run is recorded a `success`, deliberately: the container did what it
    /// was asked, and faber never learns a job's conclusion. That classification is
    /// not what this change touches.
    ///
    /// But the job GitHub ran on that container did fail, because of a cap faber
    /// chose, and the retry sweep has to be able to see it. It cannot get there via
    /// `result` (this one says `success`) and it cannot get there via GitHub (this
    /// job carries *no annotations at all* yet did run steps, so
    /// `retry::runner_loss` finds no runner death and the run is filed as genuine). The ledger is the third path, and it keys
    /// off the `oom_runner` flag, which is true on both shapes of OOM.
    #[tokio::test]
    async fn an_oom_recorded_as_a_success_still_arms_the_retry_sweep() {
        const RUNNER: &str = "faber-1002-1700000000000";
        const MINTED_FOR: u64 = 1002;
        const RAN: u64 = 1001;

        let (orch, tmp) = test_orch();
        let report = RunReport {
            // The listener exited cleanly; the kill landed on a child of a step.
            job_ok: true,
            oom_runner: true,
            runner_peak_mib: Some(257),
            dind_peak_mib: Some(37),
            applied_runner_mib: 512,
            cause: Some(
                "runner container exited 0, but the kernel OOM-killed a process in its cgroup \
                 at the 512 MiB cap — a step inside the job died, not the agent"
                    .to_string(),
            ),
            ..Default::default()
        };
        orch.record_outcome(
            Finished {
                repo: "owner/demo",
                job_id: MINTED_FOR,
                executed_job_id: Some(RAN),
                class: Some("owner/demo::900::test"),
                label: Some("owner/demo · CI · test"),
                profile: "faber-docker",
                runner: RUNNER,
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 40_000,
            },
            &report,
        )
        .await;

        // Classification is unchanged, and pinned so the ledger can never be
        // mistaken for a licence to reclassify: the container exited 0, so this
        // stays a delivery, explained by its cause line.
        let lines = audit_lines(&tmp);
        assert_eq!(lines[0]["result"], "success", "{}", lines[0]);
        assert_eq!(lines[0]["oom_runner"], true);
        assert!(
            lines[0]["cause"]
                .as_str()
                .is_some_and(|c| c.contains("512 MiB cap")),
            "an OOM must never be an unexplained anything: {}",
            lines[0]
        );
        let view = orch.view().await;
        assert_eq!(view.counts.completed, 1);
        assert_eq!(view.counts.faults, 0);

        // And the sweep can now see the kill, keyed to the runner GitHub also names
        // on the failed job, and pinned to the job that actually ran on it.
        assert_eq!(
            ending_for(&orch, RUNNER, RAN).await,
            Some(EndKind::Oom),
            "the kill is keyed by the runner name GitHub reports on the job"
        );
        assert_eq!(
            ending_for(&orch, RUNNER, MINTED_FOR).await,
            None,
            "and is pinned to the job that ran, not to the one the runner was named for"
        );
        assert_eq!(lines[0]["end"], "oom");
    }

    /// A run faber could not attribute leaves `executed_job_id` unset — but so does
    /// a run that simply executed the job it was minted for. The ledger has to tell
    /// those apart, or one runner's OOM would excuse any job on it.
    #[tokio::test]
    async fn an_oom_on_the_minted_for_job_is_still_pinned_to_that_job() {
        let (orch, _tmp) = test_orch();
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 42,
                // Unset because the runner ran exactly the job it was minted for —
                // `class` being present is what says the attribution succeeded.
                executed_job_id: None,
                class: Some("o/r::1::build"),
                label: Some("o/r · ci · build"),
                profile: "linux-docker",
                runner: "faber-42-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 60_000,
            },
            &RunReport {
                job_ok: true,
                oom_runner: true,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            ending_for(&orch, "faber-42-1000", 42).await,
            Some(EndKind::Oom)
        );
        assert_eq!(
            ending_for(&orch, "faber-42-1000", 43).await,
            None,
            "a sibling job that merely shared the runner name must not be retried"
        );
    }

    /// The dind half of the same signal, and the reason the flag pair is what the
    /// ledger keys on. A dind kill leaves the runner container's own exit untouched,
    /// so this too records a `success` — while the job it was serving saw its image
    /// pull truncate and its daemon stop resolving.
    #[tokio::test]
    async fn a_dind_oom_arms_the_sweep_on_the_same_terms() {
        let (orch, tmp) = test_orch();
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 42,
                executed_job_id: None,
                class: Some("o/r::1::e2e"),
                label: Some("o/r · ci · e2e"),
                profile: "linux-docker",
                runner: "faber-42-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 60_000,
            },
            &RunReport {
                job_ok: true,
                oom_dind: true,
                applied_dind_mib: 256,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(audit_lines(&tmp)[0]["result"], "success");
        assert_eq!(
            ending_for(&orch, "faber-42-1000", 42).await,
            Some(EndKind::Oom)
        );
    }

    /// A run that ended on its own must never read as faber's act — otherwise every
    /// ordinary failure on a faber runner would be retried, which is the one thing
    /// ADR-0003 exists to prevent.
    #[tokio::test]
    async fn a_run_that_did_not_oom_is_not_a_faber_ending() {
        let (orch, _tmp) = test_orch();
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 42,
                executed_job_id: None,
                class: Some("o/r::1::build"),
                label: Some("o/r · ci · build"),
                profile: "linux-docker",
                runner: "faber-42-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 60_000,
            },
            // A genuine non-zero exit: the runner ran, the job's step failed.
            &RunReport::default(),
        )
        .await;
        assert_eq!(
            ending_for(&orch, "faber-42-1000", 42).await,
            Some(EndKind::Other),
            "recorded, so the sweep can say why — but never a reason to retry"
        );
    }

    /// Every way faber's report can end a runner maps to one `end` word, on the
    /// audit line and in the ledger alike (ADR-0003, amendment 2026-10-18).
    #[tokio::test]
    async fn each_ending_is_audited_and_indexed_by_its_word() {
        let cases: Vec<(RunReport, &str, EndKind)> = vec![
            (
                RunReport {
                    timed_out: true,
                    ..Default::default()
                },
                "lifetime_cap",
                EndKind::LifetimeCap,
            ),
            (
                RunReport {
                    reclaimed: true,
                    ..Default::default()
                },
                "reclaimed",
                EndKind::Reclaimed,
            ),
            (
                RunReport {
                    infra_error: true,
                    lost: true,
                    ..Default::default()
                },
                "lost",
                EndKind::Lost,
            ),
            (
                RunReport {
                    job_ok: true,
                    ..Default::default()
                },
                "exited",
                EndKind::Clean,
            ),
            (
                // A setup failure before any job ran: not a vanished runner.
                RunReport {
                    infra_error: true,
                    ..Default::default()
                },
                "failed",
                EndKind::Other,
            ),
        ];
        for (i, (report, word, kind)) in cases.into_iter().enumerate() {
            let (orch, tmp) = test_orch();
            let runner = format!("faber-{i}-1000");
            orch.record_outcome(
                Finished {
                    repo: "o/r",
                    job_id: i as u64,
                    executed_job_id: None,
                    class: None,
                    label: None,
                    profile: "linux-docker",
                    runner: &runner,
                    attempt: 1,
                    wait_ms: 0,
                    parked: &NOT_PARKED,
                    duration_ms: 1_000,
                },
                &report,
            )
            .await;
            assert_eq!(audit_lines(&tmp)[0]["end"], word);
            assert_eq!(ending_for(&orch, &runner, i as u64).await, Some(kind));
        }
    }

    /// A freeze and its thaw are indexed beside their audit lines.
    #[tokio::test]
    async fn a_freeze_opens_and_a_thaw_closes_the_runners_ending() {
        let (orch, _tmp) = test_orch();
        orch.record_freeze("o/r", 9, "faber-9-1000", "frozen", "disk", "low".into())
            .await;
        assert_eq!(
            ending_for(&orch, "faber-9-1000", 9).await,
            Some(EndKind::Frozen { thawed_ms: None })
        );
        orch.record_freeze("o/r", 9, "faber-9-1000", "thawed", "disk", "room".into())
            .await;
        assert!(matches!(
            ending_for(&orch, "faber-9-1000", 9).await,
            Some(EndKind::Frozen { thawed_ms: Some(_) })
        ));
    }

    /// The gate this fix adds. The ledger's whole reason to exist is the promise
    /// that a retry it arms is a *real* second chance: `spawn_provision`'s
    /// `executed_class` match force-bumps the class cap via `Limiter::record_run`
    /// before the re-run — but only in the arm where attribution succeeded. When it
    /// misses (the run finished between discovery scans, so faber never learned
    /// which job GitHub actually dispatched to the runner), `class` is `None`,
    /// `record_run` never runs, and the cap is untouched. Arming the ledger on that
    /// OOM anyway would let the sweep spend the job's one `attempt_cap` retry on a
    /// rerun at the identical cap — the same kill, and the only attempt burned for
    /// nothing.
    #[tokio::test]
    async fn an_unattributed_oom_never_arms_the_retry_sweep() {
        let (orch, _tmp) = test_orch();

        // Attribution succeeded: the compensating bump ran, so this one must arm.
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 42,
                executed_job_id: None,
                class: Some("o/r::1::build"),
                label: Some("o/r · ci · build"),
                profile: "linux-docker",
                runner: "faber-42-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 60_000,
            },
            &RunReport {
                job_ok: true,
                oom_runner: true,
                ..Default::default()
            },
        )
        .await;

        // Same OOM shape, but attribution missed — no `record_run`, no cap bump.
        orch.record_outcome(
            Finished {
                repo: "o/r",
                job_id: 43,
                executed_job_id: None,
                class: None,
                label: Some("o/r · ci · build"),
                profile: "linux-docker",
                runner: "faber-43-1000",
                attempt: 1,
                wait_ms: 0,
                parked: &NOT_PARKED,
                duration_ms: 60_000,
            },
            &RunReport {
                job_ok: true,
                oom_runner: true,
                ..Default::default()
            },
        )
        .await;

        assert_eq!(
            ending_for(&orch, "faber-42-1000", 42).await,
            Some(EndKind::Oom),
            "attribution succeeded, so the bumped-cap promise held — this one arms"
        );
        assert_eq!(
            ending_for(&orch, "faber-43-1000", 43).await,
            None,
            "attribution missed, so the cap was never bumped — arming this one would \
             burn the job's one retry on a rerun into the same unbumped cap"
        );
    }

    #[tokio::test]
    async fn a_sustained_outage_records_only_its_two_edges() {
        // The poll loop runs every 20 s, so an unguarded fault record would write a
        // line per cycle for as long as GitHub is unreachable — burying, in the
        // hundreds, the two lines that actually delimit the outage.
        let (orch, tmp) = test_orch();
        for _ in 0..5 {
            orch.observe_fault("-", "discovery", Some("http transport error".to_string()))
                .await;
        }
        for _ in 0..5 {
            orch.observe_fault("-", "discovery", None).await;
        }

        let lines = audit_lines(&tmp);
        assert_eq!(lines.len(), 2, "one line per transition, not per cycle");
        assert_eq!(lines[0]["action"], "failed");
        assert_eq!(lines[0]["cause"], "http transport error");
        assert_eq!(lines[1]["action"], "recovered");
        assert_eq!(lines[1]["result"], "discovery", "the scope that recovered");
    }

    #[tokio::test]
    async fn a_healthy_daemon_never_writes_a_recovery() {
        // The steady state is "everything works". A `recovered` line for something
        // that never failed would be the single most common line in the log.
        let (orch, tmp) = test_orch();
        for _ in 0..10 {
            orch.observe_fault("-", "discovery", None).await;
        }
        assert!(audit_lines(&tmp).is_empty());
        assert!(orch.view().await.faults.is_empty());
    }

    #[tokio::test]
    async fn faults_are_tracked_per_repo_and_per_scope() {
        // One repo's 403 must not suppress another repo's, and a repo-scan fault
        // must not be silenced by an unrelated discovery fault already standing.
        let (orch, tmp) = test_orch();
        orch.observe_fault("-", "discovery", Some("timeout".to_string()))
            .await;
        orch.observe_fault("me/a", "repo_scan", Some("403".to_string()))
            .await;
        orch.observe_fault("me/b", "repo_scan", Some("403".to_string()))
            .await;
        // Repeats of each are still suppressed.
        orch.observe_fault("me/a", "repo_scan", Some("403".to_string()))
            .await;

        let lines = audit_lines(&tmp);
        assert_eq!(lines.len(), 3);
        let repos: Vec<&str> = lines.iter().map(|l| l["repo"].as_str().unwrap()).collect();
        assert_eq!(repos, vec!["-", "me/a", "me/b"]);
    }

    /// A `Claim` for the drain tests, carrying an explicit cancel sender and
    /// admitted flag; every other field is inert. Mirrors the demand-test Claim.
    fn drain_claim(cancel: watch::Sender<bool>, admitted: bool) -> Claim {
        Claim {
            repo: "owner/demo".to_string(),
            profile: "linux-docker".to_string(),
            pr: None,
            branch: None,
            workflow: None,
            job_name: "build".to_string(),
            event: "push".to_string(),
            started_ms: 0,
            step: Arc::new(std::sync::Mutex::new(None)),
            started: Arc::new(AtomicBool::new(false)),
            cancel,
            admitted: Arc::new(AtomicBool::new(admitted)),
            containerized: true,
            runner_name: Arc::new(std::sync::Mutex::new(None)),
            own_mint: true,
            image: Arc::new(std::sync::Mutex::new(None)),
            waiting: Arc::default(),
            frozen: Arc::new(std::sync::Mutex::new(None)),
            job_over: Arc::new(AtomicBool::new(false)),
            footprint: Arc::default(),
        }
    }

    /// A container probe that can't read the host — every claim reads as "still
    /// running", which is how the drain must behave when Docker is unreadable.
    async fn probe_unreadable() -> Option<HashSet<u64>> {
        None
    }

    #[tokio::test]
    async fn await_drain_returns_when_only_parked_claims_remain() {
        // A parked claim holds no container, so drain must not wait on it — else
        // shutdown hangs forever. `await_drain` should return promptly despite the
        // map being non-empty.
        let (orch, _tmp) = test_orch();
        orch.inflight
            .lock()
            .await
            .insert(77, drain_claim(watch::channel(false).0, false));
        let r = tokio::time::timeout(
            Duration::from_secs(2),
            orch.drain_until_idle(probe_unreadable),
        )
        .await;
        assert!(
            r.is_ok(),
            "await_drain hung on a parked (no-container) claim"
        );
        assert!(
            orch.inflight.lock().await.contains_key(&77),
            "drain must not itself remove the parked claim (its task does that)"
        );
    }

    #[tokio::test]
    async fn await_drain_waits_for_admitted_claim() {
        // An admitted claim backs a real runner: drain must wait for it to finish
        // (its task releases the claim), then return. The probe can't read the host,
        // so the claim is presumed live — ignorance never shortens a drain.
        let (orch, _tmp) = test_orch();
        orch.inflight
            .lock()
            .await
            .insert(88, drain_claim(watch::channel(false).0, true));
        let pending = tokio::time::timeout(
            Duration::from_millis(200),
            orch.drain_until_idle(probe_unreadable),
        )
        .await;
        assert!(
            pending.is_err(),
            "await_drain returned while an admitted runner was still in flight"
        );
        // The runner finishes: its task releases the claim, which wakes the drain.
        orch.release_claim(88).await;
        let done = tokio::time::timeout(
            Duration::from_secs(2),
            orch.drain_until_idle(probe_unreadable),
        )
        .await;
        assert!(
            done.is_ok(),
            "await_drain didn't converge after the admitted claim was released"
        );
    }

    #[tokio::test]
    async fn await_drain_completes_when_an_admitted_claim_has_no_container() {
        // Issue #16's second symptom: several claims were admitted with not one
        // `faber-` container on the host, so the drain sat there until launchd
        // SIGKILLed the daemon. A claim Docker can prove is gone backs nothing —
        // cancel it (so its task unwinds) and stop waiting.
        let (orch, _tmp) = test_orch();
        let (tx, rx) = watch::channel(false);
        orch.inflight.lock().await.insert(99, drain_claim(tx, true));
        // The host is readable and runs a *different* job's runner: 99's is gone.
        let probe = || async { Some(HashSet::from([1234u64])) };
        let done = tokio::time::timeout(Duration::from_secs(2), orch.drain_until_idle(probe)).await;
        assert!(
            done.is_ok(),
            "drain hung on an admitted claim with no live runner container"
        );
        assert!(
            *rx.borrow(),
            "a container-less admitted claim must be cancelled so its task unwinds"
        );
        assert!(
            orch.inflight.lock().await.contains_key(&99),
            "drain must not remove the claim itself (its task does that)"
        );
    }

    // --- runner-image pool coherence (ADR-0009) -----------------------------

    /// A [`PoolImage`] with a known served set, for the pool tests.
    fn pool_image(tag: &str, serves: &[&str]) -> PoolImage {
        PoolImage {
            tag: tag.to_string(),
            serves: serves.iter().map(|v| v.to_string()).collect(),
        }
    }

    #[test]
    fn pool_decision_holds_the_pool_to_one_image_generation() {
        // The invariant ADR-0009 pins: the images live for one repo are totally
        // ordered by what they serve, and faber only ever adds the maximum. Each
        // case below is one way that can be true or false.
        let base = pool_image("base:1", &["1.61.1"]);
        let variant = pool_image("faber-runner:abc-pw-1.62.1", &["1.61.1", "1.62.1"]);
        let other = pool_image("faber-runner:abc-pw-1.60.0", &["1.61.1", "1.60.0"]);

        // An empty pool is the generation-switch point: whatever resolved, mint it.
        assert_eq!(
            pool_decision(&[], &variant),
            PoolDecision::Mint(variant.clone()),
            "the first runner of a generation must mint as resolved"
        );
        // The common case by far — every queued job shares a pin, so nothing moves.
        assert_eq!(
            pool_decision(&[base.clone(), base.clone()], &base),
            PoolDecision::Mint(base.clone()),
            "a homogeneous pool must be left exactly as it is"
        );
        // Widening: a variant bakes the base's version too, so it can serve every
        // job the live base runners could. Safe to add, and it drains them faster.
        assert_eq!(
            pool_decision(std::slice::from_ref(&base), &variant),
            PoolDecision::Mint(variant.clone()),
            "an image that covers the whole live pool must be minted, not held"
        );
        // Narrowing — a plain `main` job while a variant is hot. This is the case a
        // naive "images differ ⇒ wait" rule serialises the repo's CI behind
        // alternating drains: the job joins the live variant instead, which already
        // bakes what it pins.
        assert_eq!(
            pool_decision(std::slice::from_ref(&variant), &base),
            PoolDecision::Mint(variant.clone()),
            "a job the live image already serves must join it, never narrow the pool"
        );
        // Genuinely incomparable: a third version in flight. Neither image can serve
        // the other's jobs, so only one generation may be live at a time.
        assert_eq!(
            pool_decision(std::slice::from_ref(&variant), &other),
            PoolDecision::Hold,
            "two incomparable images must never be live at once"
        );
    }

    #[test]
    fn pool_decision_fails_open_on_anything_unknown() {
        // Under-minting starves CI; a surplus container does not. A `Hold` therefore
        // requires faber to *positively know* both sides and find them incomparable
        // — an unlabelled image or an unreadable lockfile constrains nothing.
        let variant = pool_image("faber-runner:abc-pw-1.62.1", &["1.61.1", "1.62.1"]);
        let other = pool_image("faber-runner:abc-pw-1.60.0", &["1.61.1", "1.60.0"]);
        let unknown = PoolImage::unknown("some/base:1");

        assert_eq!(
            pool_decision(std::slice::from_ref(&variant), &unknown),
            PoolDecision::Mint(unknown.clone()),
            "an image faber can say nothing about must never be held"
        );
        assert_eq!(
            pool_decision(std::slice::from_ref(&unknown), &other),
            PoolDecision::Mint(other.clone()),
            "an unreadable live image must not constrain what may be minted"
        );
        // The unknown entry is ignored rather than treated as a blocker, so the
        // known ones still decide — here they are incomparable, so the hold stands.
        assert_eq!(
            pool_decision(&[unknown, variant], &other),
            PoolDecision::Hold,
            "an unknown entry must be skipped, not allowed to mask a real conflict"
        );
    }

    #[tokio::test]
    async fn claim_pool_image_publishes_under_the_map_lock() {
        // Two siblings resolving at once would both read an empty pool and both mint
        // — recreating the split the rule exists to prevent — unless the decision and
        // the publish happen under one held lock. Serialised, the second sees the
        // first's image. A held claim publishes nothing, which is also what stops two
        // claims holding for each other.
        let (orch, _tmp) = test_orch();
        let variant = pool_image("faber-runner:abc-pw-1.62.1", &["1.61.1", "1.62.1"]);
        let other = pool_image("faber-runner:abc-pw-1.60.0", &["1.61.1", "1.60.0"]);
        {
            let mut map = orch.inflight.lock().await;
            map.insert(1, drain_claim(watch::channel(false).0, true));
            map.insert(2, drain_claim(watch::channel(false).0, false));
        }

        // First sibling: empty pool, mints as resolved and publishes.
        assert_eq!(
            orch.claim_pool_image(1, "owner/demo", &variant).await,
            PoolDecision::Mint(variant.clone())
        );
        assert_eq!(
            orch.pool_images(2, "owner/demo").await,
            vec![variant.clone()],
            "a cleared claim must publish its image for later siblings to weigh"
        );

        // Second sibling, incomparable: holds, and publishes nothing.
        assert_eq!(
            orch.claim_pool_image(2, "owner/demo", &other).await,
            PoolDecision::Hold
        );
        assert!(
            orch.pool_images(1, "owner/demo").await.is_empty(),
            "a holding claim must stay invisible to the pool"
        );

        // A different repo's runners are a different pool entirely.
        assert_eq!(
            orch.claim_pool_image(2, "owner/other", &other).await,
            PoolDecision::Mint(other)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_pool_image_gives_the_claim_up_on_a_reclaim() {
        // Holding happens with the claim still parked — no container, no memory
        // reservation — so a reclaim issued during one must be honoured without a
        // container ever having existed, exactly as it is during a variant build.
        let (orch, _tmp) = test_orch();
        let variant = pool_image("faber-runner:abc-pw-1.62.1", &["1.61.1", "1.62.1"]);
        let other = pool_image("faber-runner:abc-pw-1.60.0", &["1.61.1", "1.60.0"]);
        {
            let mut map = orch.inflight.lock().await;
            map.insert(1, drain_claim(watch::channel(false).0, true));
            map.insert(2, drain_claim(watch::channel(false).0, false));
        }
        assert_eq!(
            orch.claim_pool_image(1, "owner/demo", &variant).await,
            PoolDecision::Mint(variant)
        );

        let (tx, mut rx) = watch::channel(false);
        tx.send_replace(true);
        let waiting = std::sync::Mutex::new(Parking::default());
        assert_eq!(
            orch.await_pool_image(2, "owner/demo", other, &mut rx, &waiting)
                .await,
            None,
            "a reclaimed claim must stop holding rather than wait out the bound"
        );
        // It held before it was given up, and said what it was holding for.
        let waited = waiting
            .into_inner()
            .unwrap()
            .waiting()
            .expect("a holding claim says why");
        assert_eq!(waited.gate, "pool");
        assert!(
            waited.detail.contains("faber-runner:abc-pw-1.62.1"),
            "{}",
            waited.detail
        );
    }

    #[tokio::test]
    async fn await_drain_still_waits_for_a_claim_with_no_container() {
        // A macOS VM's runner is not a container: a readable — and necessarily
        // empty — container listing must not be read as "it's gone".
        let (orch, _tmp) = test_orch();
        let (tx, rx) = watch::channel(false);
        let claim = Claim {
            containerized: false,
            ..drain_claim(tx, true)
        };
        orch.inflight.lock().await.insert(55, claim);
        let probe = || async { Some(HashSet::new()) };
        let pending =
            tokio::time::timeout(Duration::from_millis(200), orch.drain_until_idle(probe)).await;
        assert!(
            pending.is_err(),
            "drain abandoned a VM claim on the strength of a container listing"
        );
        assert!(!*rx.borrow(), "a VM claim must not be cancelled");
    }

    #[tokio::test]
    async fn begin_drain_cancels_parked_not_admitted() {
        // begin_drain flips draining and cancels only the parked claim; the admitted
        // one is left running for await_drain to wait on.
        let (orch, _tmp) = test_orch();
        let (parked_tx, parked_rx) = watch::channel(false);
        let (admitted_tx, admitted_rx) = watch::channel(false);
        {
            let mut map = orch.inflight.lock().await;
            map.insert(1, drain_claim(parked_tx, false));
            map.insert(2, drain_claim(admitted_tx, true));
        }
        orch.begin_drain(Drain::Stop).await;
        assert!(orch.is_draining(), "begin_drain must set the draining flag");
        assert!(
            *parked_rx.borrow(),
            "the parked claim must be cancelled so its task bails admission"
        );
        assert!(
            !*admitted_rx.borrow(),
            "the admitted claim must be left running, not cancelled"
        );
    }

    #[tokio::test]
    async fn end_drain_lifts_only_the_drain_its_holder_began() {
        // The updater ends its own drain when the install it drained for does not
        // happen (ADR-0006). A stop's drain must outlive that, whichever came first.
        let (orch, _tmp) = test_orch();
        orch.begin_drain(Drain::Update).await;
        assert!(orch.end_drain(Drain::Update), "nobody else held a drain");
        assert!(!orch.is_draining());
        tokio::time::timeout(Duration::from_secs(2), orch.wake.notified())
            .await
            .expect("lifting the drain runs the next cycle at once");

        orch.begin_drain(Drain::Stop).await;
        orch.begin_drain(Drain::Update).await;
        assert!(!orch.end_drain(Drain::Update));
        assert!(orch.is_draining(), "a stop begun first stays");

        let (orch, _tmp) = test_orch();
        orch.begin_drain(Drain::Update).await;
        orch.begin_drain(Drain::Stop).await;
        assert!(!orch.end_drain(Drain::Update));
        assert!(orch.is_draining(), "a stop begun during the update's stays");
    }

    // --- ADR-0019: CPU-aware admission --------------------------------------

    fn cpu(peak_mcpu: u64, avg_mcpu: u64, throttled_ratio: f64) -> Option<crate::docker::CpuUsage> {
        Some(crate::docker::CpuUsage {
            peak_mcpu,
            avg_mcpu,
            throttled_ratio,
        })
    }

    fn finished<'a>(job_id: u64, class: Option<&'a str>) -> Finished<'a> {
        Finished {
            repo: "owner/demo",
            job_id,
            executed_job_id: None,
            class,
            label: Some("owner/demo · ci · e2e"),
            profile: "linux-docker",
            runner: "faber-7-1000",
            attempt: 1,
            wait_ms: 0,
            parked: &NOT_PARKED,
            duration_ms: 1_000,
        }
    }

    #[tokio::test]
    async fn an_outcome_records_the_cpu_each_container_used() {
        let (orch, tmp) = test_orch();
        let report = RunReport {
            job_ok: true,
            runner_cpu: cpu(3990, 2100, 0.42),
            dind_cpu: cpu(250, 40, 0.0),
            applied_runner_mcpu: 4000,
            ..Default::default()
        };
        orch.record_outcome(finished(7, Some("owner/demo::900::e2e")), &report)
            .await;
        // A run whose CPU was never read, under no quota, has nothing to say.
        orch.record_outcome(finished(8, None), &RunReport::default())
            .await;

        let lines = audit_lines(&tmp);
        let rec = &lines[0];
        assert_eq!(rec["cpu_peak_runner_mcpu"], 3990);
        assert_eq!(rec["cpu_avg_runner_mcpu"], 2100);
        assert_eq!(rec["cpu_throttled_runner"], 0.42);
        assert_eq!(rec["cpu_quota_runner_mcpu"], 4000);
        assert_eq!(rec["cpu_peak_dind_mcpu"], 250);
        assert_eq!(rec["cpu_avg_dind_mcpu"], 40);
        assert!(
            rec.get("cpu_throttled_dind").is_none(),
            "a zero ratio is omitted — replay reads absence as unthrottled"
        );
        let silent = lines[1].as_object().unwrap();
        assert!(silent.keys().all(|k| !k.starts_with("cpu_")), "{silent:?}");
    }

    #[tokio::test]
    async fn a_host_oom_in_the_counters_writes_its_own_audit_line() {
        use crate::docker::MemEvents;
        let (orch, tmp) = test_orch();
        // The runner was killed with no limit breached; dind's cap was breached.
        let report = RunReport {
            oom_runner: true,
            runner_mem_events: Some(MemEvents {
                oom: 0,
                oom_kill: 1,
            }),
            dind_mem_events: Some(MemEvents {
                oom: 1,
                oom_kill: 1,
            }),
            ..Default::default()
        };
        orch.record_outcome(finished(9, Some("owner/demo::900::e2e")), &report)
            .await;
        // A cap breach alone writes no host_oom line.
        let breach = RunReport {
            oom_runner: true,
            runner_mem_events: Some(MemEvents {
                oom: 1,
                oom_kill: 1,
            }),
            ..Default::default()
        };
        orch.record_outcome(finished(10, Some("owner/demo::900::e2e")), &breach)
            .await;
        let lines = audit_lines(&tmp);
        let host: Vec<_> = lines.iter().filter(|l| l["kind"] == "host_oom").collect();
        assert_eq!(host.len(), 1, "{lines:?}");
        assert_eq!(host[0]["job_id"], 9);
        assert_eq!(host[0]["oom_runner"], true);
        assert!(host[0]["cause"].as_str().unwrap().contains("runner cgroup"));
        // The outcome line is still written, learner flags untouched.
        assert!(lines
            .iter()
            .any(|l| l["kind"] == "outcome" && l["oom_runner"] == true));
    }

    /// A runner the VM-wide OOM killer took: `oom_kill` rose, `oom` did not.
    fn host_oom_report() -> RunReport {
        use crate::docker::MemEvents;
        RunReport {
            oom_runner: true,
            runner_mem_events: Some(MemEvents {
                oom: 0,
                oom_kill: 1,
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn under_max_a_host_oom_trips_no_fallback() {
        let (orch, tmp) = test_orch();
        orch.record_outcome(
            finished(9, Some("owner/demo::900::e2e")),
            &host_oom_report(),
        )
        .await;
        let lines = audit_lines(&tmp);
        assert!(lines.iter().all(|l| l["kind"] != "reserve"), "{lines:?}");
        let host = lines.iter().find(|l| l["kind"] == "host_oom").unwrap();
        assert!(
            host["cause"]
                .as_str()
                .unwrap()
                .contains("learner is unchanged"),
            "{host}"
        );
        assert_eq!(orch.limiter.snapshot().reserve.in_force, "max");
    }

    #[tokio::test]
    async fn under_p75_a_host_oom_trips_the_fallback_and_audits_it() {
        let (orch, tmp) = test_orch();
        orch.limiter
            .set_reserve_for_test(crate::config::ReserveSetting::P75);
        assert!(orch.limiter.reserving_p75(now_ms()));
        orch.record_outcome(
            finished(9, Some("owner/demo::900::e2e")),
            &host_oom_report(),
        )
        .await;
        let lines = audit_lines(&tmp);
        let trips: Vec<_> = lines.iter().filter(|l| l["kind"] == "reserve").collect();
        assert_eq!(trips.len(), 1, "{lines:?}");
        assert_eq!(trips[0]["action"], "trip");
        assert_eq!(trips[0]["reason"], "host_oom");
        assert_eq!(trips[0]["cooloff_ms"], crate::reserve::DAY_MS);
        let host = lines.iter().find(|l| l["kind"] == "host_oom").unwrap();
        assert!(
            host["cause"]
                .as_str()
                .unwrap()
                .contains("kept out of the learner"),
            "{host}"
        );
        assert!(
            !orch.limiter.reserving_p75(now_ms()),
            "max until the re-arm"
        );
        // A restart rebuilds the fallback from that line.
        let audit = Audit::at(tmp.path.join("audit.jsonl"));
        let reborn = Limiter::new(orch.cfg.autolimit.clone(), &HashMap::new());
        reborn.set_reserve_for_test(crate::config::ReserveSetting::P75);
        reborn.restore_reserve(&audit.read_kind(crate::reserve::AUDIT_KIND));
        assert!(
            !reborn.reserving_p75(now_ms()),
            "the cool-off survives a restart"
        );
        assert_eq!(reborn.snapshot().reserve.trips_7d, 1);
    }

    #[tokio::test]
    async fn a_restart_relearns_cpu_shares_from_the_audit_log() {
        // Written the way a live run writes them, replayed the way startup does.
        let (orch, _tmp) = test_orch();
        let class = "owner/demo::900::e2e";
        for (job, peak, throttled, quota) in [(1, 1000, 0.0, 0), (2, 1500, 0.3, 1500)] {
            let report = RunReport {
                job_ok: true,
                runner_peak_mib: Some(500),
                dind_peak_mib: Some(100),
                runner_cpu: cpu(peak, peak / 2, throttled),
                dind_cpu: cpu(200, 100, 0.0),
                applied_runner_mcpu: quota,
                ..Default::default()
            };
            orch.record_outcome(finished(job, Some(class)), &report)
                .await;
        }
        // Never fed to the learner (no class), so never replayed either.
        let unlearned = RunReport {
            runner_cpu: cpu(9000, 9000, 0.0),
            ..Default::default()
        };
        orch.record_outcome(finished(3, None), &unlearned).await;

        let fresh = Limiter::new(orch.cfg.autolimit.clone(), &HashMap::new());
        seed_limiter(&fresh, &orch.audit.replay_outcomes());
        // Runner: 1000, then the throttled 1500 censored at its 1500 quota × 1.5 =
        // 2250, × margin 1.5 = 3375. Replaying the raw peak would have learned
        // 1500 × 1.5 = 2250 — the quota read back as the job's want. dind: 200 × 1.5.
        assert_eq!(fresh.reservation_for(class, true).mcpu, 3375 + 300);
        let class_view = fresh
            .snapshot()
            .classes
            .into_iter()
            .find(|c| c.key == class)
            .unwrap();
        assert_eq!(class_view.cpu_status, crate::limiter::CapStatus::Learned);
        assert_eq!(class_view.runner_mib, 750, "memory seeded alongside");
    }

    #[test]
    fn an_unstated_cpu_mode_is_warned_about_at_boot_on_a_docker_host_only() {
        use crate::config::CpuMode;
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        let docker = Profile {
            name: "linux-docker".to_string(),
            labels: labels.clone(),
            backend: Backend::Docker(crate::docker::DockerConfig::new(
                "ghcr.io/owner/my-runner:latest".to_string(),
            )),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels),
        };
        let docker = std::slice::from_ref(&docker);
        assert_eq!(
            cpu_unstated_warning(docker, CpuMode::Unstated),
            Some(
                "autolimit.cpu is unstated — CPU-aware admission (ADR-0019) is off; set cpu = \
                 \"auto\" to enable it or cpu = \"off\" to silence this"
            )
        );
        assert_eq!(
            cpu_unstated_warning(docker, CpuMode::Off),
            None,
            "an operator chose"
        );
        assert_eq!(cpu_unstated_warning(docker, CpuMode::Auto), None);
        assert_eq!(
            cpu_unstated_warning(&profiles(), CpuMode::Unstated),
            None,
            "a host without a Docker profile has nothing the setting could change"
        );
    }

    // --- a full disk stops admission, not Docker (ADR-0024 §1–§2) -------------

    fn floor(admission_gib: Option<u64>, reclaim_gib: u64) -> HostPolicy {
        HostPolicy {
            disk_admission_floor_gib: admission_gib,
            disk_floor_gib: reclaim_gib,
            ..HostPolicy::default()
        }
    }

    #[test]
    fn the_disk_admission_floor_is_warned_about_at_boot_on_a_docker_host_only() {
        let labels = vec!["faber".to_string(), "docker".to_string()];
        let docker = [Profile {
            name: "linux-docker".to_string(),
            labels: labels.clone(),
            backend: Backend::Docker(crate::docker::DockerConfig::new(
                "ghcr.io/owner/my-runner:latest".to_string(),
            )),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels),
        }];
        assert_eq!(
            disk_admission_warnings(&docker, &floor(None, 25)),
            vec![
                "host.disk_admission_floor_gib is unstated — the disk admission gate (ADR-0024) \
                 is off: faber admits Docker jobs, and restarts a wedged Docker, however full \
                 the disk is; set a floor in GiB (e.g. 10) to enable it, or 0 to silence this"
                    .to_string()
            ]
        );
        assert!(disk_admission_warnings(&docker, &floor(Some(0), 25)).is_empty());
        assert!(disk_admission_warnings(&docker, &floor(Some(10), 25)).is_empty());
        // Refused by `faber check-config`, but a file that arrived by update boots
        // and says what is wrong with it.
        let out_of_order = disk_admission_warnings(&docker, &floor(Some(30), 25));
        assert_eq!(out_of_order.len(), 1);
        assert!(
            out_of_order[0].starts_with("host.disk_admission_floor_gib (30 GiB) is above")
                && out_of_order[0].ends_with("Run `faber check-config` to fix the file."),
            "{out_of_order:?}"
        );
        assert!(
            disk_admission_warnings(&profiles(), &floor(None, 25)).is_empty(),
            "a host without a Docker profile has nothing the setting could change"
        );
    }

    #[test]
    fn the_dind_label_rule_is_warned_about_at_boot_until_it_is_on() {
        let labels = vec!["faber".to_string(), "docker".to_string()];
        let docker = [Profile {
            name: "linux-docker".to_string(),
            labels: labels.clone(),
            backend: Backend::Docker(crate::docker::DockerConfig::new(
                "ghcr.io/owner/my-runner:latest".to_string(),
            )),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels),
        }];
        let rule = |v: Option<bool>| HostPolicy {
            dind_requires_label: v,
            ..HostPolicy::default()
        };
        for (v, word) in [(None, "unstated"), (Some(false), "off")] {
            let msg = dind_label_rule_warning(&docker, &rule(v)).expect(word);
            assert!(
                msg.starts_with(&format!("host.dind_requires_label is {word} — ")),
                "{msg}"
            );
            assert!(msg.contains("dind_requires_label = true"), "{msg}");
        }
        assert_eq!(dind_label_rule_warning(&docker, &rule(Some(true))), None);
        assert_eq!(
            dind_label_rule_warning(&profiles(), &rule(None)),
            None,
            "a host without a Docker profile has nothing the setting could change"
        );
    }

    #[tokio::test]
    async fn the_disk_gate_holds_docker_admission() {
        // The gate protects the Docker VM's volume.
        let (orch, _tmp) = test_orch_host(floor(Some(10), 25));
        orch.limiter.set_vm_total_for_test(20_480);
        orch.limiter.set_disk_free_for_test(Some(1024), now_ms());
        let seen = std::sync::Mutex::new(Vec::new());
        let parked = tokio::time::timeout(
            Duration::from_millis(50),
            orch.limiter.admit(Reservation::from(100), |w| {
                seen.lock().unwrap().push(w.view())
            }),
        )
        .await;
        assert!(
            parked.is_err(),
            "a Docker job is not started onto a full disk"
        );
        assert_eq!(
            seen.lock().unwrap().first(),
            Some(&WaitView::new(
                "disk",
                "1 GiB free on the Docker volume, needs 3 GiB above the 10 GiB admission floor"
            ))
        );
    }

    #[tokio::test]
    async fn a_wedged_docker_on_a_full_disk_waits_for_room_and_the_fault_says_why() {
        let (orch, _tmp) = test_orch_host(floor(Some(10), 25));
        orch.limiter.set_docker_reachable_for_test(false);
        let t0 = now_ms();
        let first = observe_host(&orch, t0).await.expect("an outage");
        assert_eq!(first.revival, host::Revival::Wait);

        // A wedge by now, and a restart due — but the volume has 1 GiB free.
        let t1 = t0 + 600_000;
        orch.limiter.set_disk_free_for_test(Some(1024), t1);
        let due = observe_host(&orch, t1).await.unwrap();
        assert!(matches!(due.revival, host::Revival::NoRoom(_)), "{due:?}");
        // Still no room a cycle later: nothing new to say.
        orch.limiter.set_disk_free_for_test(Some(1024), t1 + 20_000);
        observe_host(&orch, t1 + 20_000).await.unwrap();

        // The fault opened with the plain line, and was stated again once, with why.
        let faults = orch.view().await.faults; // newest first
        let causes: Vec<(&str, &str)> = faults
            .iter()
            .map(|f| (f.action.as_str(), f.cause.as_str()))
            .collect();
        assert_eq!(
            causes,
            [
                (
                    "failed",
                    "Docker daemon unresponsive for 600s — not restarting it while the \
                     Docker volume has 1 GiB free, below the 10 GiB admission floor: a \
                     restart cannot make room (ADR-0024). Admission refused until it answers"
                ),
                (
                    "failed",
                    "Docker daemon unresponsive for 0s — admission refused until it answers"
                ),
            ]
        );

        // Room back: the unresponsive clock kept running, so the restart is due at
        // once — and the fault says faber is no longer holding it back.
        let t2 = t1 + 40_000;
        orch.limiter.set_disk_free_for_test(Some(40 * 1024), t2);
        let back = observe_host(&orch, t2).await.unwrap();
        assert_eq!(back.revival, host::Revival::Restart);
        assert!(back.restated);
        assert_eq!(orch.view().await.faults.len(), 3);
    }

    #[tokio::test]
    async fn a_daemon_that_answers_briefly_after_a_restart_is_not_forgiven() {
        // No floor: the loop guard alone, which is not behind the key.
        let (orch, _tmp) = test_orch_host(floor(None, 25));
        orch.limiter.set_docker_reachable_for_test(false);
        let t = |s: u64| 1_790_000_000_000 + s * 1000;
        observe_host(&orch, t(0)).await.unwrap();
        let due = observe_host(&orch, t(600)).await.unwrap();
        assert_eq!(due.revival, host::Revival::Restart);
        // As `reconcile_host` would, short of touching a real Docker.
        assert_eq!(orch.docker_custody.lock().await.restarting(t(600)), 1);

        // It answers for a minute — the outage is over, the restart not forgiven…
        orch.limiter.set_docker_reachable_for_test(true);
        assert!(observe_host(&orch, t(630)).await.is_none());
        assert!(observe_host(&orch, t(690)).await.is_none());
        assert_eq!(orch.view().await.faults[0].action, "recovered");
        // …so when it wedges again, the next restart waits out the cooldown.
        orch.limiter.set_docker_reachable_for_test(false);
        observe_host(&orch, t(700)).await.unwrap();
        let wedged = observe_host(&orch, t(1_300)).await.unwrap();
        assert_eq!(wedged.revival, host::Revival::Wait);
        let due = observe_host(&orch, t(2_400)).await.unwrap();
        assert_eq!(due.revival, host::Revival::Restart);
        assert_eq!(orch.docker_custody.lock().await.restarting(t(2_400)), 2);
    }

    #[tokio::test]
    async fn a_linux_vm_out_of_service_is_the_docker_fault_and_restarts_nothing() {
        let (orch, _tmp) = test_orch_host(floor(None, 25));
        let t = |s: u64| 1_790_000_000_000 + s * 1000;
        // The engine driver reports its VM booting, for longer than a wedge takes.
        orch.limiter
            .set_engine_fault(Some("faber's Linux VM is booting (ADR-0026)".into()));
        for s in (0..1_200).step_by(20) {
            assert!(observe_host(&orch, t(s)).await.is_none(), "at {s}s");
        }
        // Then it fails closed at the wall: the reason changes, and is said once.
        orch.limiter.set_engine_fault(Some(
            "faber's Linux VM is not serving: the wall verdict could not be read (ADR-0026)".into(),
        ));
        assert!(observe_host(&orch, t(1_220)).await.is_none());
        assert!(observe_host(&orch, t(1_240)).await.is_none());
        let faults = orch.view().await.faults; // newest first
        let causes: Vec<&str> = faults.iter().map(|f| f.cause.as_str()).collect();
        assert_eq!(
            causes,
            [
                "Docker unreachable — faber's Linux VM is not serving: the wall verdict could \
                 not be read (ADR-0026). Admission refused until it answers",
                "Docker unreachable — faber's Linux VM is booting (ADR-0026). Admission \
                 refused until it answers",
            ]
        );
        // In service, Docker answering: the fault clears, and nothing was restarted.
        orch.limiter.set_engine_fault(None);
        assert!(observe_host(&orch, t(1_260)).await.is_none());
        assert_eq!(orch.view().await.faults[0].action, "recovered");
        assert_eq!(orch.docker_custody.lock().await.restarting(t(1_260)), 1);
    }

    // --- the hold, and every parked claim saying why (issue #66) -------------
    //
    // A zero ceiling held a host's Docker admission for hours: every claim parked
    // with `admitted: false, step: null`, the state read healthy and unpaused, and
    // the stall was put down to something else.

    /// Through the real provision task: a held claim parks at admission and says
    /// it is held, the host stays healthy, and releasing the hold lets the same
    /// claim go on to register its runner.
    #[tokio::test]
    async fn a_held_claim_parks_saying_why_and_runs_once_released() {
        const JOB: u64 = 66;
        let admitted = Arc::new(AtomicBool::new(false));
        let published = Arc::new(std::sync::Mutex::new(None));
        let (base, requests) = spawn_jit_api(admitted.clone(), published.clone()).await;
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        let profile = Profile {
            name: "linux-docker".to_string(),
            labels: labels.clone(),
            backend: Backend::Docker(crate::docker::DockerConfig::new(
                "ghcr.io/owner/my-runner:latest".to_string(),
            )),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels.clone()),
        };
        let (orch, _tmp) = test_orch_on(vec![profile], Github::at(base));
        orch.limiter.set_vm_total_for_test(20_480); // room to spare: only the hold refuses
        orch.limiter.hold("VM resize pending".to_string(), now_ms());
        let waiting = Arc::new(std::sync::Mutex::new(Parking::default()));
        orch.inflight.lock().await.insert(
            JOB,
            Claim {
                repo: "owner/demo".to_string(),
                profile: "linux-docker".to_string(),
                pr: None,
                branch: None,
                workflow: None,
                job_name: "build".to_string(),
                event: "push".to_string(),
                started_ms: now_ms(),
                step: Arc::new(std::sync::Mutex::new(None)),
                started: Arc::new(AtomicBool::new(false)),
                cancel: watch::channel(false).0,
                admitted: admitted.clone(),
                containerized: true,
                runner_name: published.clone(),
                own_mint: true,
                image: Arc::new(std::sync::Mutex::new(None)),
                waiting: waiting.clone(),
                frozen: Arc::new(std::sync::Mutex::new(None)),
                job_over: Arc::new(AtomicBool::new(false)),
                footprint: Arc::default(),
            },
        );
        let task = spawn_provision(
            orch.clone(),
            "owner/demo".to_string(),
            ProvisionIntent {
                job_id: JOB,
                profile_idx: 0,
                job_name: "build".to_string(),
                workflow_id: 900,
                workflow_name: None,
                run_id: 900,
                head_sha: String::new(),
                content_ref: String::new(),
                pr: None,
                run_created_at: None,
                attempt: 1,
                job_labels: labels,
            },
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while waiting.lock().unwrap().waiting().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the held claim says why it waits");

        let view = orch.view().await;
        assert!(!view.inflight[0].admitted);
        assert_eq!(
            view.inflight[0].waiting,
            Some(WaitView::new(
                "held",
                "Docker admission held: VM resize pending"
            ))
        );
        assert_eq!(
            serde_json::to_value(&view.inflight[0]).unwrap()["waiting"]["gate"],
            "held"
        );
        assert!(view.healthy, "a hold is deliberate, not a fault");
        assert!(!view.paused);
        assert_eq!(
            view.budget.hold.map(|h| h.reason),
            Some("VM resize pending".to_string())
        );
        assert!(
            requests.lock().unwrap().is_empty(),
            "nothing registers while held"
        );

        assert!(orch.limiter.release_hold());
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("the released claim runs on to its registration")
            .expect("provision task");
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "exactly one registration: {requests:?}");
        assert!(requests[0].admitted);
        assert_eq!(
            waiting.lock().unwrap().waiting(),
            None,
            "an admitted claim waits on nothing"
        );
    }

    // --- the waste check, through the real provision task (ADR-0063 §4) ------

    /// A stand-in for api.github.com for the waste check. `GET /pulls/7` answers
    /// `pull`; the run's jobs answer none failed; every `POST` and the JIT mint
    /// are recorded as `METHOD path` with the request body. The mint is refused
    /// with a `503`, so an admitted claim ends there with nothing started.
    async fn spawn_waste_api(pull: &'static str) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (line, body) = loop {
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len = head
                            .to_ascii_lowercase()
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(str::to_string))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            let line = head.lines().next().unwrap_or("").to_string();
                            break (line, buf[end + 4..end + 4 + len].to_vec());
                        }
                    }
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break (String::new(), Vec::new()),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let (status, reply) = if method == "GET" && path.ends_with("/pulls/7") {
                    ("200 OK", pull)
                } else if method == "GET" && path.contains("/actions/runs/900/jobs") {
                    ("200 OK", r#"{"total_count":0,"jobs":[]}"#)
                } else if path.ends_with("/cancel") {
                    ("202 Accepted", "{}")
                } else if path.contains("/statuses/") {
                    ("201 Created", "{}")
                } else if path.contains("/generate-jitconfig") {
                    ("503 Service Unavailable", r#"{"message":"stand-in"}"#)
                } else {
                    ("404 Not Found", r#"{"message":"stand-in"}"#)
                };
                if method == "POST" {
                    log.lock()
                        .unwrap()
                        .push(format!("POST {path} {}", String::from_utf8_lossy(&body)));
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, seen)
    }

    const WASTE_OPEN_CONFLICTED: &str =
        r#"{"number":7,"head":{"sha":"aaaa"},"state":"open","mergeable":false}"#;
    const WASTE_CLOSED: &str = r#"{"number":7,"head":{"sha":"aaaa"},"state":"closed","mergeable":true,"closed_at":"2026-11-01T11:00:00Z"}"#;

    /// Run one `pull_request` claim for PR #7 (created at 10:00) to the end of its
    /// provision task against [`spawn_waste_api`] with `waste` set. Returns the
    /// orchestrator (its `inflight` and metrics), the audit lines, and every
    /// recorded `POST`.
    async fn run_waste_claim(
        pull: &'static str,
        waste: crate::config::WastePolicy,
    ) -> (Arc<Orchestrator>, Vec<serde_json::Value>, Vec<String>) {
        const JOB: u64 = 88;
        let admitted = Arc::new(AtomicBool::new(false));
        let published = Arc::new(std::sync::Mutex::new(None));
        let (base, posts) = spawn_waste_api(pull).await;
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        let profile = Profile {
            name: "linux-docker".to_string(),
            labels: labels.clone(),
            backend: Backend::Docker(crate::docker::DockerConfig::new(
                "ghcr.io/owner/my-runner:latest".to_string(),
            )),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels.clone()),
        };
        let (orch, tmp) = test_orch_waste(
            vec![profile],
            Github::at(base),
            HostPolicy::default(),
            waste,
        );
        orch.limiter.set_vm_total_for_test(20_480); // room to spare: only the check decides
        orch.inflight.lock().await.insert(
            JOB,
            Claim {
                repo: "owner/demo".to_string(),
                profile: "linux-docker".to_string(),
                pr: Some(7),
                branch: None,
                workflow: None,
                job_name: "build".to_string(),
                event: "pull_request".to_string(),
                started_ms: now_ms(),
                step: Arc::new(std::sync::Mutex::new(None)),
                started: Arc::new(AtomicBool::new(false)),
                cancel: watch::channel(false).0,
                admitted: admitted.clone(),
                containerized: true,
                runner_name: published.clone(),
                own_mint: true,
                image: Arc::new(std::sync::Mutex::new(None)),
                waiting: Arc::new(std::sync::Mutex::new(Parking::default())),
                frozen: Arc::new(std::sync::Mutex::new(None)),
                job_over: Arc::new(AtomicBool::new(false)),
                footprint: Arc::default(),
            },
        );
        let task = spawn_provision(
            orch.clone(),
            "owner/demo".to_string(),
            ProvisionIntent {
                job_id: JOB,
                profile_idx: 0,
                job_name: "build".to_string(),
                workflow_id: 900,
                workflow_name: None,
                run_id: 900,
                head_sha: "aaaa".to_string(),
                content_ref: "aaaa".to_string(),
                pr: Some(7),
                run_created_at: Some("2026-11-01T10:00:00Z".to_string()),
                attempt: 1,
                job_labels: labels,
            },
        );
        tokio::time::timeout(Duration::from_secs(30), task)
            .await
            .expect("the provision task ends")
            .expect("provision task");
        let lines = audit_lines(&tmp)
            .into_iter()
            .filter(|l| l["kind"] == "waste")
            .collect();
        let posts = posts.lock().unwrap().clone();
        (orch, lines, posts)
    }

    fn registrations(posts: &[String]) -> usize {
        posts
            .iter()
            .filter(|p| p.contains("/generate-jitconfig"))
            .count()
    }

    #[tokio::test]
    async fn a_conflicted_skip_cancels_the_run_and_says_why_on_the_commit() {
        let waste = crate::config::WastePolicy {
            conflicted: WasteMode::Skip,
            ..Default::default()
        };
        let (orch, lines, posts) = run_waste_claim(WASTE_OPEN_CONFLICTED, waste).await;
        assert_eq!(registrations(&posts), 0, "nothing registers: {posts:?}");
        assert!(
            orch.inflight.lock().await.is_empty(),
            "the claim is released"
        );
        assert!(
            posts
                .iter()
                .any(|p| p.starts_with("POST /repos/owner/demo/actions/runs/900/cancel")),
            "the run is cancelled: {posts:?}"
        );
        let status = posts
            .iter()
            .find(|p| p.starts_with("POST /repos/owner/demo/statuses/aaaa"))
            .unwrap_or_else(|| panic!("a status is posted on the head: {posts:?}"));
        assert!(status.contains("faber/waste"), "{status}");
        assert_eq!(orch.metrics.lock().await.counts.wasted, 1);
        assert_eq!(lines.len(), 1, "audited once: {lines:?}");
        assert_eq!(lines[0]["action"], "skip");
        assert_eq!(lines[0]["cause"], "conflicted");
    }

    #[tokio::test]
    async fn a_closed_skip_cancels_the_run_but_posts_no_status() {
        let waste = crate::config::WastePolicy {
            closed: WasteMode::Skip,
            ..Default::default()
        };
        let (orch, lines, posts) = run_waste_claim(WASTE_CLOSED, waste).await;
        assert_eq!(registrations(&posts), 0, "nothing registers: {posts:?}");
        assert!(
            orch.inflight.lock().await.is_empty(),
            "the claim is released"
        );
        assert!(
            posts.iter().any(|p| p.contains("/actions/runs/900/cancel")),
            "the run is cancelled: {posts:?}"
        );
        assert!(
            !posts.iter().any(|p| p.contains("/statuses/")),
            "only a conflict says why on the commit: {posts:?}"
        );
        assert_eq!(orch.metrics.lock().await.counts.wasted, 1);
        assert_eq!(lines.len(), 1, "audited once: {lines:?}");
        assert_eq!(lines[0]["action"], "skip");
        assert_eq!(lines[0]["cause"], "closed");
    }

    #[tokio::test]
    async fn a_run_the_close_itself_triggered_is_not_wasted() {
        // The run is created at 10:00; this PR closed at 09:00, so the close
        // started it (`on: pull_request: types: [closed]`) and `closed = "skip"`
        // must leave it alone.
        const CLOSED_BEFORE_RUN: &str = r#"{"number":7,"head":{"sha":"aaaa"},"state":"closed","mergeable":true,"closed_at":"2026-11-01T09:00:00Z"}"#;
        let waste = crate::config::WastePolicy {
            closed: WasteMode::Skip,
            ..Default::default()
        };
        let (orch, lines, posts) = run_waste_claim(CLOSED_BEFORE_RUN, waste).await;
        assert_eq!(registrations(&posts), 1, "the claim registers: {posts:?}");
        assert!(!posts.iter().any(|p| p.contains("/cancel")), "{posts:?}");
        assert!(lines.is_empty(), "no waste line: {lines:?}");
        assert_eq!(orch.metrics.lock().await.counts.wasted, 0);
    }

    #[tokio::test]
    async fn a_conflicted_demote_still_runs_and_is_audited_once() {
        let waste = crate::config::WastePolicy {
            conflicted: WasteMode::Demote,
            ..Default::default()
        };
        let (orch, lines, posts) = run_waste_claim(WASTE_OPEN_CONFLICTED, waste).await;
        assert_eq!(
            registrations(&posts),
            1,
            "admitted on the next pass: {posts:?}"
        );
        assert!(
            !posts
                .iter()
                .any(|p| p.contains("/cancel") || p.contains("/statuses/")),
            "a demote writes nothing back: {posts:?}"
        );
        assert_eq!(lines.len(), 1, "audited once: {lines:?}");
        assert_eq!(lines[0]["action"], "demote");
        assert_eq!(lines[0]["cause"], "conflicted");
        assert_eq!(orch.metrics.lock().await.counts.wasted, 1);
    }

    fn vm_ready(orch: &Orchestrator) {
        use crate::machine::{MachineReading, MachineShare, PressureLevel};
        let l = orch.limiter();
        l.set_machine_share(MachineShare {
            keep_memory_gib: Some(16),
            keep_cpus: Some(2),
        });
        l.set_machine_for_test(
            Some(MachineReading {
                mem_mib: 32 * 1024,
                cpus: 10,
                pressure: Some(PressureLevel::Normal),
                swap_used_mib: Some(0),
                ..MachineReading::default()
            }),
            now_ms(),
        );
        l.observe_wall(crate::wall::report::Judgement::Green);
    }

    /// Poll `wait` briefly; it must still be parked, and `waiting` says on what.
    async fn parked_on<F: std::future::Future>(
        wait: std::pin::Pin<&mut F>,
        waiting: &std::sync::Mutex<Parking>,
    ) -> String {
        tokio::select! {
            biased;
            _ = wait => panic!("admitted past a gate that should hold it"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        waiting.lock().unwrap().waiting().expect("says why").gate
    }

    #[tokio::test]
    async fn a_third_macos_vm_job_parks_on_the_class_cap_until_one_ends() {
        let (orch, _tmp) = test_orch();
        vm_ready(&orch);
        let (_tx, mut rx) = watch::channel(false);
        let quiet = std::sync::Mutex::new(Parking::default());
        let a = orch.vm_slot(None, &mut rx, &quiet).await.expect("first");
        let _b = orch.vm_slot(None, &mut rx, &quiet).await.expect("second");
        assert_eq!(quiet.lock().unwrap().waiting(), None, "admitted at once");
        assert_eq!(orch.limiter.snapshot().vm.macos_running, 2);

        let waiting = std::sync::Mutex::new(Parking::default());
        let wait = orch.vm_slot(None, &mut rx, &waiting);
        tokio::pin!(wait);
        assert_eq!(parked_on(wait.as_mut(), &waiting).await, "macos_cap");
        assert_eq!(orch.limiter.snapshot().vm.macos_running, 2);
        // A VM job ending wakes it at once, not at its next retry.
        drop(a);
        let c = tokio::time::timeout(Duration::from_millis(500), wait)
            .await
            .expect("the freed slot is handed over");
        assert!(c.is_some());
        assert_eq!(orch.limiter.snapshot().vm.macos_running, 2);
    }

    #[tokio::test]
    async fn a_macos_vm_job_parks_on_the_machine_share() {
        let (orch, _tmp) = test_orch();
        vm_ready(&orch);
        // An 8 GiB Linux VM leaves 8 GiB of the 16 GiB share: one macOS VM.
        orch.limiter.set_linux_vm_mib(8 * 1024);
        let (_tx, mut rx) = watch::channel(false);
        let quiet = std::sync::Mutex::new(Parking::default());
        let _a = orch.vm_slot(None, &mut rx, &quiet).await.expect("one fits");

        let waiting = std::sync::Mutex::new(Parking::default());
        let wait = orch.vm_slot(None, &mut rx, &waiting);
        tokio::pin!(wait);
        assert_eq!(parked_on(wait.as_mut(), &waiting).await, "vm_memory");
        // The Linux VM shrinking makes room.
        orch.limiter.set_linux_vm_mib(0);
        assert!(tokio::time::timeout(Duration::from_millis(500), wait)
            .await
            .expect("admitted once the share has room")
            .is_some());
    }

    #[tokio::test]
    async fn an_undeclared_share_or_a_red_wall_admits_no_macos_vm_job() {
        let (orch, _tmp) = test_orch();
        let (_tx, mut rx) = watch::channel(false);
        let waiting = std::sync::Mutex::new(Parking::default());
        {
            // Nothing set: the wall is unread, which refuses first.
            let wait = orch.vm_slot(None, &mut rx, &waiting);
            tokio::pin!(wait);
            assert_eq!(parked_on(wait.as_mut(), &waiting).await, "wall");
        }
        orch.limiter
            .observe_wall(crate::wall::report::Judgement::Green);
        {
            let wait = orch.vm_slot(None, &mut rx, &waiting);
            tokio::pin!(wait);
            assert_eq!(parked_on(wait.as_mut(), &waiting).await, "share");
        }
        vm_ready(&orch);
        orch.limiter
            .observe_wall(crate::wall::report::Judgement::Red("pf is off".into()));
        let wait = orch.vm_slot(None, &mut rx, &waiting);
        tokio::pin!(wait);
        assert_eq!(parked_on(wait.as_mut(), &waiting).await, "wall");
        assert_eq!(orch.limiter.snapshot().vm.macos_running, 0);
    }

    #[tokio::test]
    async fn a_reclaim_while_parked_for_vm_admission_counts_nothing() {
        let (orch, _tmp) = test_orch();
        vm_ready(&orch);
        let (_tx, mut rx) = watch::channel(false);
        let quiet = std::sync::Mutex::new(Parking::default());
        let _a = orch.vm_slot(None, &mut rx, &quiet).await.unwrap();
        let _b = orch.vm_slot(None, &mut rx, &quiet).await.unwrap();

        let (tx, mut cancel) = watch::channel(false);
        let waiting = std::sync::Mutex::new(Parking::default());
        let wait = orch.vm_slot(None, &mut cancel, &waiting);
        tokio::pin!(wait);
        assert_eq!(parked_on(wait.as_mut(), &waiting).await, "macos_cap");
        tx.send(true).unwrap();
        assert!(wait.await.is_none(), "reclaimed, not admitted");
        assert_eq!(orch.limiter.snapshot().vm.macos_running, 2);
    }

    /// The claim flow end to end: a `macos-vm` claim the limiter does not admit
    /// parks, names its gate, and is never marked admitted, so no runner is
    /// named and nothing reaches the VM backend. Before #263 it went straight to
    /// `vm::job::run_one`.
    #[tokio::test]
    async fn an_unadmitted_macos_vm_claim_parks_and_never_reaches_its_backend() {
        use crate::backend::{MacosVmConfig, VmTimeouts};
        use crate::vm::proto::{Cpus, ImageName, MemoryMib};
        const JOB: u64 = 263;
        let labels: Vec<String> = ["faber", "macos-vm"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let profile = Profile {
            name: "macos-vm".to_string(),
            labels: labels.clone(),
            backend: Backend::MacosVm(MacosVmConfig {
                image: ImageName::try_from("macos-27").unwrap(),
                runner_version: "2.330.0".into(),
                cpus: Cpus::try_from(4).unwrap(),
                memory_mib: MemoryMib::try_from(8192).unwrap(),
                timeouts: VmTimeouts::default(),
                revive: crate::vm::revive::RevivePolicy::OFF,
            }),
            policy: TrustPolicy::new([], [], labels.clone()),
        };
        let (orch, _tmp) = test_orch_with(vec![profile]);
        // Room in the share, but the wall is unread: VM admission refuses.
        let admitted = Arc::new(AtomicBool::new(false));
        let published = Arc::new(std::sync::Mutex::new(None));
        let waiting = Arc::new(std::sync::Mutex::new(Parking::default()));
        let (cancel, _rx) = watch::channel(false);
        orch.inflight.lock().await.insert(
            JOB,
            Claim {
                repo: "owner/demo".to_string(),
                profile: "macos-vm".to_string(),
                pr: None,
                branch: None,
                workflow: None,
                job_name: "build".to_string(),
                event: "push".to_string(),
                started_ms: now_ms(),
                step: Arc::new(std::sync::Mutex::new(None)),
                started: Arc::new(AtomicBool::new(false)),
                cancel: cancel.clone(),
                admitted: admitted.clone(),
                containerized: false,
                runner_name: published.clone(),
                own_mint: true,
                image: Arc::new(std::sync::Mutex::new(None)),
                waiting: waiting.clone(),
                frozen: Arc::new(std::sync::Mutex::new(None)),
                job_over: Arc::new(AtomicBool::new(false)),
                footprint: Arc::default(),
            },
        );
        let task = spawn_provision(
            orch.clone(),
            "owner/demo".to_string(),
            ProvisionIntent {
                job_id: JOB,
                profile_idx: 0,
                job_name: "build".to_string(),
                workflow_id: 900,
                workflow_name: None,
                run_id: 900,
                head_sha: String::new(),
                content_ref: String::new(),
                pr: None,
                run_created_at: None,
                attempt: 1,
                job_labels: labels,
            },
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while waiting.lock().unwrap().waiting().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the parked claim says why it waits");
        assert_eq!(waiting.lock().unwrap().waiting().unwrap().gate, "wall");
        assert!(!admitted.load(Ordering::SeqCst));
        assert!(published.lock().unwrap().is_none(), "no runner named");
        assert_eq!(orch.limiter.snapshot().vm.macos_running, 0);

        // Reclaimed while parked: released, never admitted, nothing counted.
        cancel.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the reclaimed claim stops waiting")
            .expect("provision task");
        assert!(!admitted.load(Ordering::SeqCst));
        assert!(orch.inflight.lock().await.get(&JOB).is_none());
        assert_eq!(orch.limiter.snapshot().vm.macos_running, 0);
    }

    /// A drain cancels the parked claims it can see. One that a cycle already
    /// under way took just after it would park behind the pause and keep `faber
    /// drain`, which waits for the in-flight list to empty, waiting on a resume.
    #[tokio::test]
    async fn a_claim_taken_as_the_drain_began_is_released_even_while_paused() {
        let (orch, _tmp) = test_orch_with(profiles()); // one macOS VM profile
        assert!(orch.pause(Pause::Operator));
        orch.begin_drain(Drain::Stop).await; // nothing to cancel yet
        let claim = Claim {
            containerized: false,
            ..drain_claim(watch::channel(false).0, false)
        };
        orch.inflight.lock().await.insert(7, claim);
        let task = spawn_provision(
            orch.clone(),
            "owner/demo".to_string(),
            ProvisionIntent {
                job_id: 7,
                profile_idx: 0,
                job_name: "build".to_string(),
                workflow_id: 900,
                workflow_name: None,
                run_id: 900,
                head_sha: String::new(),
                content_ref: String::new(),
                pr: None,
                run_created_at: None,
                attempt: 1,
                job_labels: Vec::new(),
            },
        );
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("released, not parked behind the pause")
            .expect("provision task");
        assert!(orch.inflight.lock().await.is_empty());
    }

    #[test]
    fn a_held_host_says_so_at_every_boot() {
        let warning = hold_warning(&Hold {
            reason: "VM resize pending".to_string(),
            since_ms: 951_782_400_000,
        });
        for part in [
            "held since 2000-02-29T00:00:00Z",
            "\"VM resize pending\"",
            "POST /api/unhold",
            "Release in the dashboard or tray",
        ] {
            assert!(warning.contains(part), "{part:?} missing from: {warning}");
        }
    }

    // --- gate bypasses (ADR-0030) --------------------------------------------

    /// `profiles()` with its gate's bypass response set to `on_bypass`.
    fn bypass_profiles(on_bypass: OnBypass) -> Vec<Profile> {
        let mut p = profiles();
        p[0].policy = p[0].policy.clone().with_on_bypass(on_bypass);
        p
    }

    fn minted(entries: &[(&str, u64)]) -> HashMap<String, Minted> {
        entries
            .iter()
            .map(|(name, job)| {
                (
                    name.to_string(),
                    Minted {
                        minted_for: *job,
                        profile_idx: 0,
                        at_ms: 1_000,
                    },
                )
            })
            .collect()
    }

    /// A run in `owner/demo` whose job 7 (actor `actor`) is running on `runner`.
    fn ran_on(runner: &str, actor: &str) -> Vec<(WorkflowRun, Vec<Job>)> {
        let (mut run, mut jobs) = run_with_job(7, "in_progress");
        run.triggering_actor = Some(Actor {
            login: actor.to_string(),
        });
        jobs[0].runner_name = Some(runner.to_string());
        vec![(run, jobs)]
    }

    #[test]
    fn a_refused_job_on_our_runner_is_a_bypass_and_a_peers_runner_is_not_ours() {
        let ours = minted(&[("faber-5-100", 5)]);
        let found = find_bypasses(
            "owner/demo",
            &ran_on("faber-5-100", "dependabot[bot]"),
            &Codes::new(),
            &ours,
            &profiles(),
            2_000,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        let b = &found[0];
        assert_eq!((b.job_id, b.minted_for), (7, 5));
        assert_eq!(b.runner, "faber-5-100");
        assert_eq!(b.class, BypassClass::Trust);
        assert_eq!(b.verdict, "refuse:actor:dependabot[bot]");

        // A peer minting for the same job registers under the same prefix; only
        // this host's record says which runner is ours.
        assert!(find_bypasses(
            "owner/demo",
            &ran_on("faber-5-101", "dependabot[bot]"),
            &Codes::new(),
            &ours,
            &profiles(),
            2_000,
        )
        .is_empty());
        // A trusted job on our runner is ordinary not-job-pinned dispatch.
        assert!(find_bypasses(
            "owner/demo",
            &ran_on("faber-5-100", "owner"),
            &Codes::new(),
            &ours,
            &profiles(),
            2_000,
        )
        .is_empty());
        // The job the runner was minted for passed the gate to be minted for.
        let own = minted(&[("faber-7-100", 7)]);
        assert!(find_bypasses(
            "owner/demo",
            &ran_on("faber-7-100", "dependabot[bot]"),
            &Codes::new(),
            &own,
            &profiles(),
            2_000,
        )
        .is_empty());
    }

    #[test]
    fn a_narrowed_event_is_a_policy_bypass() {
        let mut runs = ran_on("faber-5-100", "owner");
        runs[0].0.event = "repository_dispatch".to_string();
        let found = find_bypasses(
            "owner/demo",
            &runs,
            &Codes::new(),
            &minted(&[("faber-5-100", 5)]),
            &profiles(),
            2_000,
        );
        assert_eq!(found[0].class, BypassClass::Policy);
        // Outside the ceiling, its actor's standing is read: a collaborator's
        // is a trust bypass, a stranger's a third-party one.
        runs[0].0.event = "workflow_run".to_string();
        for (third_party, class) in [(false, BypassClass::Trust), (true, BypassClass::ThirdParty)] {
            let code = Code::People(vec![gate::Person {
                login: "owner".to_string(),
                third_party,
            }]);
            let found = find_bypasses(
                "owner/demo",
                &runs,
                &Codes::from([(runs[0].0.id, code)]),
                &minted(&[("faber-5-100", 5)]),
                &profiles(),
                2_000,
            );
            assert_eq!(found[0].class, class);
        }
        // Unread this cycle, it is judged on provenance alone: the disallowed
        // event is refused as it is on main, but standing is never made up from
        // GitHub's silence.
        let found = find_bypasses(
            "owner/demo",
            &runs,
            &Codes::new(),
            &minted(&[("faber-5-100", 5)]),
            &profiles(),
            2_000,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].class, BypassClass::Trust);
    }

    #[test]
    fn only_enforce_answers_and_only_a_trust_bypass() {
        let all = BypassResponse {
            kill: true,
            purge_cache: true,
            hold: true,
        };
        assert_eq!(bypass_response(OnBypass::Enforce, BypassClass::Trust), all);
        assert_eq!(
            bypass_response(OnBypass::Enforce, BypassClass::Policy),
            BypassResponse::default()
        );
        // Absent key = record: nothing but the record.
        for class in [BypassClass::Trust, BypassClass::Policy] {
            assert_eq!(
                bypass_response(OnBypass::default(), class),
                BypassResponse::default()
            );
        }
    }

    #[tokio::test]
    async fn a_bypass_is_recorded_once_however_many_cycles_see_it() {
        let (orch, tmp) = test_orch_with(profiles());
        orch.note_minted("faber-5-100", 5, 0);
        let runs = ran_on("faber-5-100", "dependabot[bot]");
        orch.detect_bypasses("owner/demo", &runs, &Codes::new())
            .await;
        orch.detect_bypasses("owner/demo", &runs, &Codes::new())
            .await;

        let lines: Vec<_> = audit_lines(&tmp)
            .into_iter()
            .filter(|l| l["result"] == "gate_bypass")
            .collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["action"], "trust");
        assert_eq!(lines[0]["job_id"], 7);
        assert_eq!(lines[0]["runner"], "faber-5-100");
        assert!(lines[0]["cause"]
            .as_str()
            .unwrap()
            .contains("minted for job 5"));

        let view = orch.view().await;
        assert_eq!(view.bypasses.len(), 1);
        assert_eq!(view.trust_bypasses_7d, 1);
        assert!(orch.was_bypass(7).await);
        // Record mode (the absent key): no hold, no cache deletion queued.
        assert!(view.repo_holds.is_empty());
        assert!(orch.cache_purges.lock().unwrap().is_empty());

        // A restart reads the week back: dedup and the count survive it.
        let replayed = replayed_bypasses(&orch.audit.read_since(0));
        assert_eq!(replayed.len(), 1);
        assert_eq!((replayed[0].0, replayed[0].2), (7, BypassClass::Trust));
    }

    #[test]
    fn note_bypass_forgets_past_the_window() {
        let mut m = Metrics::default();
        let window = BYPASS_WINDOW.as_millis() as u64;
        assert!(m.note_bypass(1, BypassClass::Trust, 1_000));
        assert!(!m.note_bypass(1, BypassClass::Trust, 2_000));
        assert!(m.note_bypass(2, BypassClass::Policy, 2_000));
        assert_eq!(m.trust_bypasses_since(2_000), 1);
        assert_eq!(m.trust_bypasses_since(1_001 + window), 0);
        assert!(m.note_bypass(3, BypassClass::Trust, 1_001 + window));
        assert!(!m.bypassed.contains_key(&1), "pruned past the window");
    }

    /// A `fault` audit line for `scope` on `repo`, as `record_fault` writes it.
    fn fault_line(ts_ms: u64, repo: &str, scope: &str, action: &str) -> crate::audit::Entry {
        serde_json::from_value(serde_json::json!({
            "ts_ms": ts_ms,
            "kind": "fault",
            "repo": repo,
            "job_id": 0,
            "result": scope,
            "action": action,
            "cause": format!("{scope} {action} at {ts_ms}"),
        }))
        .expect("an audit line")
    }

    #[test]
    fn open_faults_are_the_ones_whose_last_edge_is_not_a_recovery() {
        let entries = [
            fault_line(1, "host", "disk", "failed"),
            fault_line(2, "host", "disk", "recovered"),
            fault_line(3, "host", "docker", "failed"),
            fault_line(4, "-", "mirror", "passing_through"),
            fault_line(5, "owner/demo", "mirror", "request_failed"),
            fault_line(6, "owner/demo", "gate_bypass", "policy"),
            fault_line(7, "host", "disk", "failed"),
            fault_line(8, "host", "docker", "failed"),
        ];
        let open = replayed_open_faults(&entries);
        let seen: Vec<_> = open
            .iter()
            .map(|f| (f.ts_ms, f.scope.as_str(), f.action.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (4, "mirror", "passing_through"),
                (7, "disk", "failed"),
                (8, "docker", "failed"),
            ],
            "one-shot lines open nothing; a restated failure keeps its latest cause"
        );
        assert_eq!(open[1].cause, "disk failed at 7");
    }

    /// The bug this guards: a disk fault open when the daemon restarted, with the
    /// disk back above the floor afterwards, never got its `recovered` line.
    #[test]
    fn a_fault_open_across_a_restart_is_closed_by_the_first_healthy_check() {
        let mut m = Metrics::default();
        m.restore_open_faults(&[
            fault_line(1, "host", "disk", "failed"),
            fault_line(2, "host", "vm-disk", "failed"),
            fault_line(3, "host", "vm-disk", "recovered"),
        ]);
        assert_eq!(m.recent_faults.len(), 1, "the dashboard shows it open");
        assert!(
            m.note_fault("disk/host", false),
            "the recovery is a transition"
        );
        assert!(
            !m.note_fault("vm-disk/host", false),
            "a closed one stays quiet"
        );
    }

    /// Scopes whose healthy observation used to be gated on in-memory state a
    /// restart resets (custody's outage clock, the profile-image set) must still
    /// close a replayed open fault: the healthy pass writes `recovered`.
    #[tokio::test]
    async fn a_docker_fault_open_across_a_restart_is_closed_by_a_healthy_host_pass() {
        let (orch, _tmp) = test_orch_host(floor(Some(10), 25));
        orch.metrics
            .lock()
            .await
            .restore_open_faults(&[fault_line(1, "host", "docker", "failed")]);
        orch.limiter.set_docker_reachable_for_test(true);
        assert!(observe_host(&orch, now_ms()).await.is_none());
        let faults = orch.view().await.faults;
        assert_eq!(faults.len(), 2, "the replayed failure plus its recovery");
        assert_eq!(faults[0].action, "recovered");
        // Steady state stays quiet.
        observe_host(&orch, now_ms()).await;
        assert_eq!(orch.view().await.faults.len(), 2);
    }

    #[tokio::test]
    async fn a_profile_image_fault_open_across_a_restart_is_closed_by_a_present_pass() {
        let (orch, _tmp) = test_orch_host(floor(Some(10), 25));
        let scope = profile_image_scope("docker");
        orch.metrics
            .lock()
            .await
            .restore_open_faults(&[fault_line(1, "host", &scope, "failed")]);
        // What the `Present` arm now does, with `ProfileImages` empty: no `was`.
        orch.observe_fault("host", &scope, None).await;
        let faults = orch.view().await.faults;
        assert_eq!(faults.len(), 2);
        assert_eq!(faults[0].action, "recovered");
    }

    #[test]
    fn record_mode_is_loud_only_when_it_has_something_to_say() {
        assert!(bypass_record_warning(OnBypass::Record, 0).is_none());
        assert!(bypass_record_warning(OnBypass::Enforce, 4).is_none());
        let w = bypass_record_warning(OnBypass::Record, 2).unwrap();
        assert!(w.contains("2 trust bypasses in the last 7 days"), "{w}");
        assert!(w.contains("enforcement off"), "{w}");
    }

    #[tokio::test]
    async fn enforce_holds_the_repo_of_a_trust_bypass_only() {
        let (orch, _tmp) = test_orch_with(bypass_profiles(OnBypass::Enforce));
        orch.note_minted("faber-5-100", 5, 0);
        // No live claim for the runner, so the kill has nothing to reach.
        let mut policy = ran_on("faber-5-100", "owner");
        policy[0].0.event = "repository_dispatch".to_string();
        orch.detect_bypasses("owner/demo", &policy, &Codes::new())
            .await;
        assert!(
            orch.repo_holds().is_empty(),
            "a policy bypass holds nothing"
        );
        assert!(orch.cache_purges.lock().unwrap().is_empty());

        let mut trust = ran_on("faber-5-100", "dependabot[bot]");
        trust[0].1[0].id = 8;
        orch.detect_bypasses("owner/demo", &trust, &Codes::new())
            .await;
        let holds = orch.repo_holds();
        assert_eq!(holds.len(), 1);
        assert_eq!(holds[0].repo, "owner/demo");
        assert_eq!(holds[0].by, BYPASS_HOLD_BY);
        assert!(holds[0].reason.contains("job 8"), "{}", holds[0].reason);
        assert!(orch.cache_purges.lock().unwrap().contains("owner/demo"));
    }

    #[tokio::test]
    async fn a_repo_hold_blocks_minting_for_that_repo_only_until_cleared() {
        let (orch, _tmp) = test_orch_with(bypass_profiles(OnBypass::Enforce));
        orch.note_minted("faber-5-100", 5, 0);
        orch.detect_bypasses(
            "owner/demo",
            &ran_on("faber-5-100", "dependabot[bot]"),
            &Codes::new(),
        )
        .await;
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        let why = orch.repo_block("owner/demo", &labels).await.unwrap();
        assert_eq!(why.gate, "repo-held");
        assert!(orch.repo_block("owner/other", &labels).await.is_none());

        assert!(orch.clear_repo_hold("owner/demo").await.is_some());
        assert!(orch.repo_block("owner/demo", &labels).await.is_none());
        assert!(orch.clear_repo_hold("owner/demo").await.is_none());
    }

    #[tokio::test]
    async fn a_held_claim_parks_saying_why_and_goes_on_once_cleared() {
        let (orch, _tmp) = test_orch_with(profiles());
        orch.repo_holds.lock().unwrap().insert(
            "owner/demo".to_string(),
            RepoHold {
                repo: "owner/demo".to_string(),
                reason: "test".to_string(),
                by: BYPASS_HOLD_BY.to_string(),
                since_ms: 1_000,
            },
        );
        let (tx, mut rx) = watch::channel(false);
        let waiting = Arc::new(std::sync::Mutex::new(Parking::default()));
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        let task = {
            let (orch, waiting, labels) = (orch.clone(), waiting.clone(), labels.clone());
            tokio::spawn(async move {
                orch.await_repo_clear("owner/demo", &labels, &mut rx, &waiting)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            waiting
                .lock()
                .unwrap()
                .waiting()
                .as_ref()
                .map(|w| w.gate.clone()),
            Some("repo-held".to_string())
        );
        assert!(!task.is_finished());
        orch.clear_repo_hold("owner/demo").await;
        let cleared = tokio::time::timeout(REPO_BLOCK_POLL * 3, task)
            .await
            .expect("a cleared hold lets the claim go on")
            .unwrap();
        assert!(cleared);
        drop(tx);
    }

    #[tokio::test]
    async fn a_queued_trust_refusal_pauses_only_runners_its_labels_fit_and_lifts() {
        let job = |id: u64, labels: &[&str], trust: bool| RefusedJobView {
            labels: labels.iter().map(|l| l.to_string()).collect(),
            trust,
            ..refused_view("owner/demo", id, "refuse:fork", 100)
        };
        let docker = vec!["self-hosted".to_string(), "docker".to_string()];
        let wider = vec![
            "self-hosted".to_string(),
            "docker".to_string(),
            "faber.playwright-1.60.0".to_string(),
        ];
        let narrower = vec!["self-hosted".to_string(), "macOS".to_string()];

        for (on_bypass, pauses) in [(OnBypass::Record, false), (OnBypass::Enforce, true)] {
            let (orch, _tmp) = test_orch_with(bypass_profiles(on_bypass));
            orch.metrics
                .lock()
                .await
                .sync_refused("owner/demo", vec![job(1, &["self-hosted", "docker"], true)]);
            // L ⊆ S: the refused job could be handed this runner.
            assert_eq!(
                orch.repo_block("owner/demo", &docker).await.is_some(),
                pauses
            );
            assert_eq!(
                orch.repo_block("owner/demo", &wider).await.is_some(),
                pauses
            );
            // L ⊄ S, or another repo: it could not.
            assert!(orch.repo_block("owner/demo", &narrower).await.is_none());
            assert!(orch.repo_block("owner/other", &docker).await.is_none());
            if pauses {
                let why = orch.repo_block("owner/demo", &docker).await.unwrap();
                assert_eq!(why.gate, "refused-job");
                assert_eq!(
                    why.detail,
                    "a refused fork job in this repo could take this runner"
                );
            }
            // A policy refusal never pauses.
            orch.metrics.lock().await.sync_refused(
                "owner/demo",
                vec![job(2, &["self-hosted", "docker"], false)],
            );
            assert!(orch.repo_block("owner/demo", &docker).await.is_none());
            // Lifts by itself once the job leaves the queue.
            orch.metrics
                .lock()
                .await
                .sync_refused("owner/demo", vec![job(1, &["self-hosted", "docker"], true)]);
            assert_eq!(
                orch.repo_block("owner/demo", &docker).await.is_some(),
                pauses
            );
            orch.metrics.lock().await.sync_refused("owner/demo", vec![]);
            assert!(orch.repo_block("owner/demo", &docker).await.is_none());
        }
    }

    #[test]
    fn the_cache_waits_while_a_claim_of_its_repo_has_a_container() {
        let mut claims = HashMap::new();
        let (tx, _) = watch::channel(false);
        claims.insert(1, drain_claim(tx, false));
        assert!(
            !cache_mounted(&claims, "owner/demo"),
            "a parked claim has no container"
        );
        let (tx, _) = watch::channel(false);
        claims.insert(2, drain_claim(tx, true));
        assert!(cache_mounted(&claims, "owner/demo"));
        assert!(!cache_mounted(&claims, "owner/other"));
        // A runner outside a container never mounts a volume.
        claims.get_mut(&2).unwrap().containerized = false;
        assert!(!cache_mounted(&claims, "owner/demo"));
    }

    #[tokio::test]
    async fn only_a_force_killed_bypass_is_kept_out_of_auto_retry() {
        let (orch, tmp) = test_orch_with(profiles());
        orch.note_minted("faber-5-100", 5, 0);
        orch.detect_bypasses(
            "owner/demo",
            &ran_on("faber-5-100", "dependabot[bot]"),
            &Codes::new(),
        )
        .await;
        // Recorded only (no kill): auto-retry may still judge it.
        assert!(orch.was_bypass(7).await);
        assert!(!orch.was_force_killed(7).await);
        // Killed: excluded, and a restart reads the kill back.
        orch.metrics.lock().await.note_killed(7, now_ms());
        assert!(orch.was_force_killed(7).await);
        let b = orch.view().await.bypasses[0].clone();
        orch.record_bypass_line(&b, "killed", "killed".into()).await;
        let kills = replayed_kills(&orch.audit.read_since(0));
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].0, 7);
        drop(tmp);
    }

    #[test]
    fn an_unlabelled_cache_volume_never_reads_as_a_clean_purge() {
        use crate::docker::CachePurge;
        let actions = |p: &CachePurge| -> Vec<&'static str> {
            purge_report(p).into_iter().map(|(a, _)| a).collect()
        };
        assert_eq!(actions(&CachePurge::Removed(vec![])), vec!["cache_deleted"]);
        assert!(actions(&CachePurge::InUse("v".into())).is_empty());
        let only_legacy = CachePurge::Unlabelled {
            removed: vec![],
            name: "faber-cache-owner-demo".into(),
        };
        let report = purge_report(&only_legacy);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].0, "cache_unlabelled");
        assert!(report[0].1.contains("faber-cache-owner-demo"));
        assert!(report[0].1.contains("remove it by hand"));
        assert!(!report.iter().any(|(_, c)| c.contains("no cache volume")));
        let both = CachePurge::Unlabelled {
            removed: vec!["faber-cache-owner-demo-new".into()],
            name: "faber-cache-owner-demo".into(),
        };
        assert_eq!(actions(&both), vec!["cache_deleted", "cache_unlabelled"]);
    }

    // --- a missing profile image takes a host out loudly (#63) ---------------

    /// The September 2026 shape: the profile's own image is gone and no registry
    /// holds it. Claims for that profile park under their own gate rather than
    /// minting runners that cannot start, the state reads unhealthy with the
    /// profile named, and the image's return clears all of it.
    #[tokio::test]
    async fn a_missing_profile_image_parks_its_claims_and_reads_unhealthy() {
        const IMG: &str = "ghcr.io/owner/my-runner:2.335.1";
        let (orch, _tmp) = test_orch_with(vec![docker_profile("docker", IMG)]);
        assert!(orch.image_block("docker", IMG).is_none());
        {
            let mut images = orch.profile_images.lock().unwrap();
            assert!(images.absent("docker", IMG, 1_000));
        }
        // Absent and being pulled: since `--pull=never` (#156) a runner could not
        // start from it yet, so the claim waits under its own gate.
        let why = orch.image_block("docker", IMG).expect("waits for the pull");
        assert_eq!(why.gate, "image-pull");
        assert!(orch.profile_images.lock().unwrap().pulled(
            "docker",
            IMG,
            Some("not found".into()),
            2_000
        ));

        let why = orch.image_block("docker", IMG).expect("parked");
        assert_eq!(why.gate, "image-missing");
        assert!(
            why.detail.contains("profile image missing: docker"),
            "{}",
            why.detail
        );
        // A variant faber built (ADR-0008) is another tag, and is not held back.
        assert!(orch
            .image_block("docker", "faber-variant:abc-1.58.0")
            .is_none());

        // A claim parked behind it is counted under its own gate in the
        // per-gate tally (#85), not folded into another one.
        let tally = orch.limiter.gate_tally();
        let waiting = std::sync::Mutex::new(Parking::new(tally.clone()));
        let (_tx, mut cancel) = watch::channel(true);
        assert!(
            !orch
                .await_profile_image("docker", IMG, &mut cancel, &waiting)
                .await
        );
        assert_eq!(
            waiting.lock().unwrap().waiting().map(|w| w.gate),
            Some("image-missing".to_string())
        );
        let gates = tally.snapshot();
        assert_eq!(gates["image-missing"].parked_now, 1);
        assert_eq!(gates["image-missing"].parks, 1);
        drop(waiting);
        assert_eq!(tally.snapshot()["image-missing"].parked_now, 0);

        let view = orch.view().await;
        assert!(
            !view.healthy,
            "a host that cannot start its runners is not healthy"
        );
        assert_eq!(
            view.last_error.as_deref(),
            Some("profile image missing: docker (ghcr.io/owner/my-runner:2.335.1)")
        );
        assert_eq!(view.missing_images.len(), 1);
        assert_eq!(view.missing_images[0].cause, "not found");

        orch.profile_images.lock().unwrap().present("docker", IMG);
        assert!(orch.image_block("docker", IMG).is_none());
        let view = orch.view().await;
        assert!(view.healthy);
        assert!(view.missing_images.is_empty());
    }

    fn docker_profile(name: &str, image: &str) -> Profile {
        let labels = vec!["self-hosted".to_string(), "docker".to_string()];
        Profile {
            name: name.to_string(),
            labels: labels.clone(),
            backend: Backend::Docker(crate::docker::DockerConfig::new(image.to_string())),
            policy: TrustPolicy::new([], ["dependabot[bot]".to_string()], labels),
        }
    }

    /// #156: before the first check of a profile's image, a claim waits for it
    /// rather than starting a runner that `--pull=never` may refuse — unless the
    /// daemon is not answering, when the check cannot run and decides nothing.
    #[tokio::test]
    async fn an_unchecked_profile_image_holds_a_claim_only_while_docker_answers() {
        const IMG: &str = "ghcr.io/owner/my-runner:2.335.1";
        let (orch, _tmp) = test_orch_with(vec![docker_profile("docker", IMG)]);
        *orch.profile_images.lock().unwrap() = ProfileImages::default();
        let why = orch
            .image_block("docker", IMG)
            .expect("waits for the check");
        assert_eq!(why.gate, "image-pull");
        assert!(why.detail.starts_with("checking that"), "{}", why.detail);
        orch.profile_images.lock().unwrap().unknown("docker", IMG);
        assert!(orch.image_block("docker", IMG).is_none());
        // A profile this host does not have is nothing this check watches.
        assert!(orch.image_block("other", IMG).is_none());
    }

    /// #156: a profile whose image sits in a Docker Hub user namespace is warned
    /// about while `pull_unqualified_images` is unstated, and only then; an
    /// official-library or registry-qualified image never is.
    #[tokio::test]
    async fn an_unqualified_hub_image_is_warned_about_only_while_unstated() {
        let profiles = vec![
            docker_profile("hub", "owner/my-runner:1"),
            docker_profile("official", "ubuntu:24.04"),
            docker_profile("ghcr", "ghcr.io/owner/my-runner:1"),
            profile_named("macos", &["self-hosted", "macOS"]),
        ];
        let unstated = unqualified_images(&profiles, &HostPolicy::default());
        assert_eq!(
            unstated
                .iter()
                .map(|u| u.profile.as_str())
                .collect::<Vec<_>>(),
            ["hub"]
        );
        assert!(unstated[0].message.contains("\"owner\" namespace"));
        for stated in [Some(true), Some(false)] {
            let host = HostPolicy {
                pull_unqualified_images: stated,
                ..HostPolicy::default()
            };
            assert!(
                unqualified_images(&profiles, &host).is_empty(),
                "{stated:?}"
            );
        }
        let (orch, _tmp) = test_orch_with(profiles);
        let view = orch.view().await;
        assert_eq!(view.unqualified_images.len(), 1);
        assert!(view.healthy, "a warning, not a fault");
    }

    // --- GitHub retiring a profile's runner warns, and only warns (#64) -----

    /// The September 2026 shape: weeks of jobs on a faber runner carried GitHub's
    /// deprecation annotation and nothing read it. Read off a job this host's
    /// runner ran, it is a per-profile warning in `/api/state` that leaves the
    /// host healthy, and a later job without it clears it.
    #[tokio::test]
    async fn a_runner_deprecation_annotation_warns_without_touching_health() {
        const ANN: &str = "Runner Version 2.335.1 will no longer be able to run jobs on \
                           September 24, 2026, please update your runner version.";
        let (orch, _tmp) = test_orch_with(profiles());
        orch.note_minted("faber-5-100", 5, 0);

        // A runner this host did not mint is a peer's: its annotation is not ours.
        orch.observe_runner_annotations(Some("faber-6-100-peer"), 6, true, &[ANN]);
        orch.observe_runner_annotations(None, 6, true, &[ANN]);
        assert!(orch.view().await.runner_retirements.is_empty());

        orch.observe_runner_annotations(Some("faber-5-100"), 5, true, &[ANN]);
        let view = orch.view().await;
        assert!(view.healthy, "a warning, not a fault");
        assert!(view.last_error.is_none());
        assert_eq!(view.runner_retirements.len(), 1);
        let r = &view.runner_retirements[0];
        assert_eq!(r.profile, "linux-docker");
        assert_eq!(r.runner_version, "2.335.1");
        assert_eq!(r.cutoff, "2026-09-24");
        assert_eq!(r.job_id, 5);
        assert!(r.message.contains("2.335.1"), "{}", r.message);
        assert_eq!(
            view.profiles[0].runner_version.as_deref(),
            Some("2.335.1"),
            "the version GitHub named is recorded against the profile"
        );

        // A job that ran with no such annotation means the runner was updated.
        orch.observe_runner_annotations(Some("faber-5-100"), 7, true, &[]);
        assert!(orch.view().await.runner_retirements.is_empty());
    }

    /// Only a finished run that got going is offered for sampling, and the
    /// sample names the runner that must have run the job.
    #[tokio::test]
    async fn a_finished_run_is_offered_for_sampling() {
        let (orch, _tmp) = test_orch_with(profiles());
        let finished = |job_id: u64| Finished {
            repo: "owner/demo",
            job_id,
            executed_job_id: Some(job_id + 100),
            class: None,
            label: None,
            profile: "linux-docker",
            runner: "faber-1-100",
            attempt: 1,
            wait_ms: 0,
            parked: &NOT_PARKED,
            duration_ms: 1_000,
        };
        let broken = RunReport {
            infra_error: true,
            ..RunReport::default()
        };
        orch.record_outcome(finished(1), &broken).await;
        assert!(orch.retirements.lock().unwrap().due(now_ms()).is_empty());

        orch.record_outcome(finished(2), &RunReport::default())
            .await;
        let due = orch.retirements.lock().unwrap().due(now_ms());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "linux-docker");
        assert_eq!(due[0].1.job_id, 102, "the job GitHub dispatched to it");
        assert_eq!(due[0].1.runner, "faber-1-100");
    }

    /// The sample end to end: the job is fetched to confirm this host's runner
    /// ran it, then its annotations are read. A job GitHub handed to some other
    /// runner is never read for this profile's warning.
    #[tokio::test]
    async fn a_sampled_job_warns_only_when_this_hosts_runner_ran_it() {
        let (base, seen) = spawn_routed_api(|request| {
            if request.starts_with("GET /repos/owner/demo/actions/jobs/102 ") {
                (
                    "200 OK",
                    r#"{"id":102,"name":"build","status":"completed","conclusion":"success","runner_name":"faber-1-100","check_run_url":"https://api.github.com/repos/owner/demo/check-runs/102","steps":[{"name":"Set up job","status":"completed","number":1,"conclusion":"success"}]}"#,
                )
            } else if request.starts_with("GET /repos/owner/demo/actions/jobs/103 ") {
                (
                    "200 OK",
                    r#"{"id":103,"name":"build","status":"completed","conclusion":"success","runner_name":"faber-9-900","check_run_url":"https://api.github.com/repos/owner/demo/check-runs/103","steps":[]}"#,
                )
            } else if request.starts_with("GET /repos/owner/demo/check-runs/102/annotations") {
                (
                    "200 OK",
                    r#"[{"message":"Runner Version 2.335.1 will no longer be able to run jobs on September 24, 2026, please update your runner version."}]"#,
                )
            } else {
                ("404 Not Found", r#"{"message":"stand-in"}"#)
            }
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        let sample = |job_id| Candidate {
            repo: "owner/demo".into(),
            job_id,
            runner: "faber-1-100".into(),
        };

        orch.retirements
            .lock()
            .unwrap()
            .finished("linux-docker", sample(103));
        reconcile_runner_retirement(&orch).await;
        assert!(orch.view().await.runner_retirements.is_empty());
        assert!(
            !seen
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("/check-runs/103")),
            "another runner's job is not read"
        );

        // The next sample waits out the interval; a fresh state stands in for it.
        *orch.retirements.lock().unwrap() = Retirements::default();
        orch.retirements
            .lock()
            .unwrap()
            .finished("linux-docker", sample(102));
        reconcile_runner_retirement(&orch).await;
        let view = orch.view().await;
        assert!(view.healthy);
        assert_eq!(view.runner_retirements.len(), 1);
        assert_eq!(view.runner_retirements[0].cutoff, "2026-09-24");
    }

    // --- CI verdicts on finished runs (ADR-0044) ---------------------------

    /// A run of job `job_id` on runner `faber-{job_id}-100`, executed as `executed`.
    fn verdict_run(job_id: u64, executed: Option<u64>) -> Finished<'static> {
        let runner: &'static str = Box::leak(format!("faber-{job_id}-100").into_boxed_str());
        Finished {
            repo: "owner/demo",
            job_id,
            executed_job_id: executed,
            class: None,
            label: Some("owner/demo · ci · build"),
            profile: "linux-docker",
            runner,
            attempt: 1,
            wait_ms: 0,
            parked: &NOT_PARKED,
            duration_ms: 1_000,
        }
    }

    fn pr_claim(pr: u64) -> Claim {
        Claim {
            pr: Some(pr),
            branch: Some("feature".to_string()),
            workflow: Some("ci".to_string()),
            job_name: "build".to_string(),
            event: "pull_request".to_string(),
            ..drain_claim(watch::channel(false).0, true)
        }
    }

    fn completed_job(conclusion: Option<&str>, runner: Option<&str>) -> Job {
        Job {
            runner_name: runner.map(str::to_string),
            conclusion: conclusion.map(str::to_string),
            workflow_name: Some("lint".to_string()),
            head_branch: Some("main".to_string()),
            ..job_with_status(107, "completed")
        }
    }

    /// The identity is the claim's when the executed job is the minted one, so
    /// the popover keys the finished job into its live group.
    #[tokio::test]
    async fn an_outcome_copies_the_claims_identity_when_the_minted_job_ran() {
        let (orch, _tmp) = test_orch_with(profiles());
        orch.inflight.lock().await.insert(7, pr_claim(2741));
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        // An executed id equal to the minted one is the same job.
        orch.inflight.lock().await.insert(8, pr_claim(2742));
        orch.record_outcome(verdict_run(8, Some(8)), &RunReport::default())
            .await;
        let view = orch.view().await;
        for (o, pr) in view.outcomes.iter().zip([2742, 2741]) {
            assert_eq!(o.pr, Some(pr));
            assert_eq!(o.branch.as_deref(), Some("feature"));
            assert_eq!(o.workflow.as_deref(), Some("ci"));
            assert_eq!(o.job_name.as_deref(), Some("build"));
            assert_eq!(o.event.as_deref(), Some("pull_request"));
            assert_eq!(o.conclusion, None, "not read yet");
        }
        let m = orch.metrics.lock().await;
        assert_eq!(m.pending_verdicts.len(), 2);
        assert!(m.pending_verdicts.iter().all(|p| !p.identity_from_job));
    }

    /// A runner that ran a different job than it was minted for never carries
    /// the minted job's PR: the identity is the executed job's own, from the
    /// verdict fetch, with no PR at all.
    #[tokio::test]
    async fn an_outcome_of_a_different_executed_job_has_no_pr() {
        let (base, _seen) = spawn_routed_api(|request| {
            if request.starts_with("GET /repos/owner/demo/actions/jobs/107 ") {
                (
                    "200 OK",
                    r#"{"id":107,"name":"unit","status":"completed","conclusion":"failure","runner_name":"faber-7-100","workflow_name":"lint","head_branch":"main"}"#,
                )
            } else {
                ("404 Not Found", r#"{"message":"stand-in"}"#)
            }
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        orch.inflight.lock().await.insert(7, pr_claim(2741));
        orch.record_outcome(verdict_run(7, Some(107)), &RunReport::default())
            .await;
        let before = orch.view().await.outcomes[0].clone();
        assert_eq!(before.job_id, 107);
        assert_eq!(before.pr, None, "never the minted job's PR");
        assert_eq!(before.job_name, None);
        assert_eq!(before.workflow, None);

        reconcile_verdicts(&orch).await;
        let view = orch.view().await;
        assert_eq!(view.outcomes.len(), 1, "updated in place, not re-pushed");
        let o = &view.outcomes[0];
        assert_eq!(o.pr, None);
        assert_eq!(o.job_name.as_deref(), Some("unit"));
        assert_eq!(o.workflow.as_deref(), Some("lint"));
        assert_eq!(o.branch.as_deref(), Some("main"));
        assert_eq!(o.conclusion.as_deref(), Some("failure"));
        assert!(orch.metrics.lock().await.pending_verdicts.is_empty());
    }

    /// GitHub marks a job completed seconds after its runner exits. A job not
    /// completed yet is asked again on later cycles, three times in all, and
    /// then left without a verdict.
    #[tokio::test]
    async fn a_job_not_completed_yet_is_retried_then_given_up_after_three_tries() {
        let (base, seen) = spawn_routed_api(|request| {
            if request.starts_with("GET /repos/owner/demo/actions/jobs/7 ") {
                (
                    "200 OK",
                    r#"{"id":7,"name":"build","status":"in_progress","runner_name":"faber-7-100"}"#,
                )
            } else {
                ("404 Not Found", r#"{"message":"stand-in"}"#)
            }
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        for tries in 1..=VERDICT_TRIES {
            reconcile_verdicts(&orch).await;
            assert_eq!(seen.lock().unwrap().len(), tries as usize);
        }
        assert!(orch.metrics.lock().await.pending_verdicts.is_empty());
        reconcile_verdicts(&orch).await;
        assert_eq!(seen.lock().unwrap().len(), 3, "no fourth try");
        assert_eq!(orch.view().await.outcomes[0].conclusion, None);
    }

    /// A refused fetch spends a try and is never a verdict. A 403 also blocks the
    /// repo for `Actions: read`, so the later tries ask GitHub nothing.
    #[tokio::test]
    async fn a_fetch_error_leaves_the_conclusion_absent() {
        let (base, seen) = spawn_routed_api(|_| {
            (
                "403 Forbidden",
                r#"{"message":"Resource not accessible by personal access token"}"#,
            )
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        for _ in 0..VERDICT_TRIES + 1 {
            reconcile_verdicts(&orch).await;
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "the 403 blocks the repo");
        assert!(orch.metrics.lock().await.pending_verdicts.is_empty());
        assert_eq!(orch.view().await.outcomes[0].conclusion, None);
    }

    /// GitHub not answering at all ends the batch and spends nobody's try:
    /// nothing was learned, and the runs wait for a cycle that reaches it.
    #[tokio::test]
    async fn an_unreachable_github_ends_the_batch_without_spending_tries() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        for job in [7, 8, 9] {
            orch.record_outcome(verdict_run(job, None), &RunReport::default())
                .await;
        }
        for _ in 0..VERDICT_TRIES + 2 {
            reconcile_verdicts(&orch).await;
        }
        let m = orch.metrics.lock().await;
        let pending: Vec<(u64, u32)> = m
            .pending_verdicts
            .iter()
            .map(|p| (p.job_id, p.tries))
            .collect();
        assert_eq!(
            pending,
            vec![(7, 0), (8, 0), (9, 0)],
            "kept, in order, untried"
        );
    }

    /// One batch asks about at most `VERDICT_BATCH` runs, oldest first; the
    /// rest wait, untried, for the next.
    #[tokio::test]
    async fn a_batch_asks_about_the_oldest_few_runs_only() {
        let (base, seen) = spawn_routed_api(|_| {
            (
                "200 OK",
                r#"{"id":1,"name":"build","status":"in_progress"}"#,
            )
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        let n = VERDICT_BATCH as u64 + 4;
        for job in 1..=n {
            orch.record_outcome(verdict_run(job, None), &RunReport::default())
                .await;
        }
        reconcile_verdicts(&orch).await;
        let asked: Vec<String> = seen.lock().unwrap().clone();
        assert_eq!(asked.len(), VERDICT_BATCH);
        for (i, r) in asked.iter().enumerate() {
            assert!(r.contains(&format!("/actions/jobs/{} ", i + 1)), "{r}");
        }
        let m = orch.metrics.lock().await;
        assert_eq!(m.pending_verdicts.len(), n as usize);
        assert_eq!(m.pending_verdicts[0].job_id, 1, "the tried ones stay first");
        assert!(m
            .pending_verdicts
            .iter()
            .skip(VERDICT_BATCH)
            .all(|p| p.tries == 0));
    }

    /// The janitor's view of the watched repos refuses a search GitHub marks
    /// incomplete, so the mirror janitor deletes nothing and cache ageing keeps to
    /// the labelled volumes that pass (October 2026: a short search deleted nearly
    /// all the mirrors).
    #[tokio::test]
    async fn served_repos_refuses_an_incomplete_search() {
        let (base, _seen) = spawn_routed_api(|request| {
            if request.starts_with("GET /search/repositories") {
                (
                    "200 OK",
                    r#"{"total_count":12,"incomplete_results":true,"items":[{"full_name":"owner/demo"}]}"#,
                )
            } else {
                ("404 Not Found", r#"{"message":"stand-in"}"#)
            }
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        assert!(orch.served_repos().await.is_err());
    }

    /// A paused host still reads its finished jobs' verdicts: the batch starts
    /// before the pause returns, on its own task.
    #[tokio::test]
    async fn a_paused_host_still_reads_verdicts() {
        let (base, _seen) = spawn_routed_api(|request| {
            if request.starts_with("GET /repos/owner/demo/actions/jobs/7 ") {
                (
                    "200 OK",
                    r#"{"id":7,"name":"build","status":"completed","conclusion":"success","runner_name":"faber-7-100"}"#,
                )
            } else {
                ("404 Not Found", r#"{"message":"stand-in"}"#)
            }
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        assert!(orch.pause(Pause::Operator));
        assert!(
            orch.gh.claim_block().is_none(),
            "the test reaches the pause"
        );
        let spawned = reconcile_once(&orch)
            .await
            .expect("a paused cycle succeeds");
        for h in spawned {
            h.await.unwrap();
        }
        assert_eq!(
            orch.view().await.outcomes[0].conclusion.as_deref(),
            Some("success")
        );
        // A batch still running is not joined by a second.
        orch.verdicts_running.store(true, Ordering::Release);
        assert!(spawn_verdicts(&orch).is_none());
    }

    /// A verdict for a run that has already left the 50-entry outcome feed is
    /// dropped, and touches no other run.
    #[tokio::test]
    async fn a_verdict_for_an_evicted_outcome_is_dropped() {
        let (orch, _tmp) = test_orch_with(profiles());
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        let gone = orch.metrics.lock().await.pending_verdicts[0].clone();
        for job in 100..100 + OUTCOME_RECENT_CAP as u64 {
            orch.record_outcome(verdict_run(job, None), &RunReport::default())
                .await;
        }
        let mut m = orch.metrics.lock().await;
        assert!(m.recent_outcomes.iter().all(|o| o.job_id != 7));
        m.apply_verdict(
            &gone,
            JobVerdict {
                conclusion: Some("success".to_string()),
                job_name: "build".to_string(),
                workflow: None,
                branch: None,
            },
        );
        assert!(m.recent_outcomes.iter().all(|o| o.conclusion.is_none()));
        assert_eq!(m.recent_outcomes.len(), OUTCOME_RECENT_CAP);
    }

    /// Waits, in real time and at most 5 s, until `done` holds: the mock API
    /// answers over a real socket, which paused tokio time cannot drive.
    async fn eventually(done: impl Fn() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("not within 5 s");
    }

    /// The early fetch fires once, `verdict_early_ms` after the run ends, on its
    /// own task, and records when the verdict arrived; the cycle's batch then has
    /// nothing left to ask. The delay runs on paused time, so only `advance`
    /// fires it.
    #[tokio::test]
    async fn the_early_fetch_fires_once_and_records_when_the_verdict_arrived() {
        let (base, seen) = spawn_routed_api(|_| {
            (
                "200 OK",
                r#"{"id":7,"name":"build","status":"completed","conclusion":"success","runner_name":"faber-7-100"}"#,
            )
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        orch.verdict_early_ms.store(60_000, Ordering::Relaxed);
        tokio::time::pause();
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        tokio::time::advance(Duration::from_millis(59_000)).await;
        assert!(seen.lock().unwrap().is_empty(), "not before its delay");
        tokio::time::advance(Duration::from_millis(1_000)).await;
        tokio::time::resume();
        eventually(|| {
            orch.metrics.try_lock().is_ok_and(|m| {
                m.recent_outcomes
                    .back()
                    .is_some_and(|o| o.conclusion.is_some())
            })
        })
        .await;
        let o = orch.view().await.outcomes[0].clone();
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(o.conclusion.as_deref(), Some("success"));
        let at = o.conclusion_ts_ms.expect("the verdict's own time");
        assert!(at >= o.ts_ms);
        reconcile_verdicts(&orch).await;
        assert_eq!(seen.lock().unwrap().len(), 1, "the batch asks nothing more");
    }

    /// An early fetch that finds the job still running spends one of the run's
    /// three tries, so the batch has two left.
    #[tokio::test]
    async fn the_early_fetch_counts_against_the_runs_tries() {
        let (base, seen) = spawn_routed_api(|_| {
            (
                "200 OK",
                r#"{"id":7,"name":"build","status":"in_progress","runner_name":"faber-7-100"}"#,
            )
        })
        .await;
        let (orch, _tmp) = test_orch_on(profiles(), Github::at(base));
        orch.verdict_early_ms.store(60_000, Ordering::Relaxed);
        tokio::time::pause();
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        tokio::time::advance(Duration::from_millis(60_000)).await;
        tokio::time::resume();
        eventually(|| {
            orch.metrics
                .try_lock()
                .is_ok_and(|m| m.pending_verdicts.iter().any(|p| p.tries == 1))
        })
        .await;
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "back in the queue, one try spent"
        );
        for _ in 0..VERDICT_TRIES {
            reconcile_verdicts(&orch).await;
        }
        assert_eq!(seen.lock().unwrap().len(), VERDICT_TRIES as usize);
        assert!(orch.metrics.lock().await.pending_verdicts.is_empty());
        let o = &orch.view().await.outcomes[0];
        assert_eq!((o.conclusion.clone(), o.conclusion_ts_ms), (None, None));
    }

    /// A rejected credential asks GitHub nothing for a verdict, early or in a
    /// batch: the run stays queued, untried, for a usable credential.
    #[tokio::test]
    async fn a_rejected_credential_asks_nothing_for_a_verdict() {
        let (base, seen) =
            spawn_routed_api(|_| ("401 Unauthorized", r#"{"message":"Bad credentials"}"#)).await;
        let gh = Github::at(base);
        gh.mark_unconfirmed();
        let (orch, _tmp) = test_orch_on(profiles(), gh);
        reconcile_credential(&orch).await;
        assert!(orch.gh.is_rejected());
        let before = seen.lock().unwrap().len();
        orch.verdict_early_ms.store(60_000, Ordering::Relaxed);
        tokio::time::pause();
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        tokio::time::advance(Duration::from_millis(60_000)).await;
        tokio::time::resume();
        // The early task takes the run and puts it back untried; wait for it.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        eventually(|| {
            orch.metrics
                .try_lock()
                .is_ok_and(|m| m.pending_verdicts.len() == 1)
        })
        .await;
        reconcile_verdicts(&orch).await;
        reconcile_verdicts(&orch).await;
        assert_eq!(seen.lock().unwrap().len(), before, "no request at all");
        let m = orch.metrics.lock().await;
        let pending: Vec<(u64, u32)> = m
            .pending_verdicts
            .iter()
            .map(|p| (p.job_id, p.tries))
            .collect();
        assert_eq!(pending, vec![(7, 0)]);
    }

    /// A batch that panics still clears its flag, so the next cycle starts one.
    #[tokio::test]
    async fn the_running_flag_clears_even_when_a_batch_panics() {
        let flag = Arc::new(AtomicBool::new(true));
        let held = Arc::clone(&flag);
        let joined = tokio::spawn(async move {
            let _running = ClearOnDrop(held);
            panic!("a batch that panics");
        })
        .await;
        assert!(joined.is_err());
        assert!(!flag.load(Ordering::Acquire));
    }

    /// A reclaimed runner ran no job, so nothing is asked about it.
    #[tokio::test]
    async fn a_reclaimed_run_asks_for_no_verdict() {
        let (orch, _tmp) = test_orch_with(profiles());
        let reclaimed = RunReport {
            reclaimed: true,
            ..RunReport::default()
        };
        orch.record_outcome(verdict_run(7, None), &reclaimed).await;
        assert!(orch.metrics.lock().await.pending_verdicts.is_empty());
    }

    /// Each conclusion passes through as GitHub spells it, and only from a
    /// completed job this run's runner ran.
    #[test]
    fn a_conclusion_is_passed_through_verbatim_and_only_for_this_runner() {
        for c in [
            "success",
            "failure",
            "timed_out",
            "cancelled",
            "skipped",
            "neutral",
            "action_required",
            "startup_failure",
        ] {
            let job = completed_job(Some(c), Some("faber-7-100"));
            let VerdictRead::Settled(Some(v)) = read_verdict(Some(&job), "faber-7-100") else {
                panic!("{c} settles");
            };
            assert_eq!(v.conclusion.as_deref(), Some(c));
        }
        let elsewhere = completed_job(Some("success"), Some("faber-9-900"));
        assert_eq!(
            read_verdict(Some(&elsewhere), "faber-7-100"),
            VerdictRead::Settled(None),
            "another runner's verdict is not this run's"
        );
        let unnamed = completed_job(Some("success"), None);
        assert_eq!(
            read_verdict(Some(&unnamed), "faber-7-100"),
            VerdictRead::Settled(None)
        );
        let running = Job {
            runner_name: Some("faber-7-100".to_string()),
            ..job_with_status(7, "in_progress")
        };
        assert_eq!(
            read_verdict(Some(&running), "faber-7-100"),
            VerdictRead::Pending
        );
        assert_eq!(
            read_verdict(None, "faber-7-100"),
            VerdictRead::Settled(None)
        );
    }

    /// The new fields are always in the JSON, `null` when unknown, so a reader
    /// can tell "no verdict" from "an older daemon" only by the key.
    #[tokio::test]
    async fn an_outcome_serialises_the_new_fields_as_null_when_absent() {
        let (orch, _tmp) = test_orch_with(profiles());
        orch.record_outcome(verdict_run(7, None), &RunReport::default())
            .await;
        let json = serde_json::to_value(&orch.view().await.outcomes[0]).unwrap();
        for key in [
            "pr",
            "branch",
            "workflow",
            "job_name",
            "event",
            "conclusion",
            "conclusion_ts_ms",
        ] {
            assert_eq!(json.get(key), Some(&serde_json::Value::Null), "{key}");
        }
    }

    // --- two instances on one pool (#94) -------------------------------------

    /// With a peer on the same repo, this host names it once and records the
    /// peer's runner that went dark holding a job as `peer_runner_lost` — never
    /// its own, and never twice.
    #[tokio::test]
    async fn a_peers_lost_runner_is_recorded_as_the_peers() {
        let (orch, tmp) = test_orch();
        let listing = vec![
            Runner {
                id: 1,
                ..runner("faber-10-1700000000000-builder", true, "offline")
            },
            Runner {
                id: 2,
                ..runner("faber-11-1700000000000-follower", true, "offline")
            },
            Runner {
                id: 3,
                ..runner("faber-12-1700000000000-follower", false, "online")
            },
        ];
        orch.observe_peers("owner/demo", &listing).await;
        orch.observe_peers("owner/demo", &listing).await;

        let lost: Vec<_> = audit_lines(&tmp)
            .into_iter()
            .filter(|l| l["kind"] == "peer_runner_lost")
            .collect();
        assert_eq!(lost.len(), 1, "{lost:?}");
        assert_eq!(lost[0]["job_id"], 11);
        assert_eq!(lost[0]["runner"], "faber-11-1700000000000-follower");
        assert_eq!(lost[0]["action"], "peer follower");

        let view = orch.view().await;
        assert_eq!(view.instance, "builder");
        assert_eq!(view.peers.len(), 1);
        assert_eq!(view.peers[0].instance, "follower");
        assert_eq!(view.peers[0].runners.get("owner/demo"), Some(&2));

        // GitHub then lists no runners on the repo at all: the peer is still
        // known, but no longer shown holding runners it has given up.
        orch.observe_peers("owner/demo", &[]).await;
        let view = orch.view().await;
        assert_eq!(view.peers.len(), 1);
        assert!(view.peers[0].runners.is_empty(), "{:?}", view.peers[0]);
    }
}

#[cfg(test)]
mod waste_tests {
    use super::*;
    use crate::waste::{judge, Signal};

    #[test]
    fn a_demoted_claim_names_its_signal_while_it_waits() {
        // ADR-0063 §5.
        let policy = crate::config::WastePolicy {
            conflicted: WasteMode::Demote,
            ..Default::default()
        };
        let finding = judge(&policy, &[Signal::Conflicted]).unwrap();
        let memory = WaitView::new("memory", "needs 1000 MiB");
        let got = wasted_wait(Some(&finding), Some(7), memory.clone());
        assert_eq!(got.gate, "wasted");
        assert_eq!(
            got.detail,
            "set aside: likely wasted (conflicted, PR #7); needs 1000 MiB"
        );
        // A claim nothing demoted reports the gate as it is.
        assert_eq!(wasted_wait(None, Some(7), memory.clone()), memory);
    }
}
