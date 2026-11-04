//! The critical disk floor — ADR-0032 §2.
//!
//! The disk gate ([`crate::limiter`], ADR-0024 §1) decides what *starts*, and
//! nothing it decides can stop a job that outgrows its charge once it has
//! started. In October 2026 one dind job wrote gigabytes within minutes of
//! starting; the volume backing Docker fell to almost nothing free, and Docker
//! went down with every job on it. So below a critical floor faber freezes the
//! most recently admitted running Docker job with `docker pause`, its dind daemon
//! included. While free space stays below the floor it freezes the next newest on
//! each later tick, one at a time, and once there is room again it thaws them,
//! oldest first, one per tick.
//!
//! A freeze costs a job only if space does not come back. Frozen for long enough,
//! its runner loses its connection and GitHub fails the job, which auto-retry
//! (ADR-0003) may run again. A full disk costs Docker and every job on the host.
//!
//! **A job GitHub has finished is let go, and at most two are frozen at once**
//! (#111). Frozen past GitHub's ~10 minutes, a job is failed but keeps its disk:
//! the reclaim guard (ADR-0017) still sees its worker in `docker top`, so nothing
//! tears it down. If the disk stays short for reasons outside CI, every job would
//! end up frozen that way and CI would stay down until the runner lifetime cap. So
//! once the reconciler has seen the frozen runner's job terminal on GitHub
//! ([`Admitted::job_over`]), the job is thawed whatever the disk reads: its runner
//! then exits on its own, and the normal teardown frees its disk. Such a job is
//! never frozen again. And no more than [`MAX_FROZEN`] are frozen at a time, so a
//! disk nothing here is filling costs two jobs at a time, not all of them.
//!
//! **A frozen job is still a running job.** Its claim stays admitted and in
//! flight, and its memory stays reserved. Docker keeps a paused container's
//! `State.Running` true but reports its status as `paused`, which a
//! `status=running` filter leaves out, so every listing that means "this job is
//! alive" asks for both ([`crate::docker::LIVE_STATUSES`]): the reconciler's
//! container check and the drain, the janitor's running listings, the restart
//! preflight. The stale-runner sweep leaves a frozen runner's registration alone,
//! since a frozen runner reads `offline` only because faber paused it. And the
//! reclaim guard (ADR-0017) needs nothing new: `docker top` reads a frozen
//! container's process table, so a frozen job is still a job running in it.
//!
//! **What is frozen is read from Docker, never remembered.** Every tick lists
//! faber's own containers by their labels — `org.faber.kind` and `org.faber.id`,
//! never a name (AGENTS.md) — with their state, and joins them to the admitted
//! claims. A restart adopts a paused container like any other (ADR-0011), and the
//! next process thaws it on the same rule.
//!
//! **Scope.** Freezing happens only where `[host] disk_admission_floor_gib` is
//! stated and non-zero: unstated or `0` turns it off with the gate. Turning the
//! floor off stops freezing, not thawing, so the loop runs on every Docker host and
//! lets go of any job it finds frozen — one frozen under a floor since withdrawn —
//! rather than leave it paused for good, which with `runner_timeout_secs = 0` would
//! be for ever. A host that never stated a floor has nothing frozen by faber, so
//! the one change there is that a faber runner someone paused by hand is unpaused
//! on the next tick. Every freeze and thaw is logged and audited
//! (`kind: "freeze"`), and `/api/state` shows a frozen claim's `frozen`, with the
//! last failed attempt to thaw it, if any.
//!
//! **The memory freeze (ADR-0033 §3) uses the same machinery.** Under
//! `[autolimit] reserve = "p75"` the ledger debits less than each job's cap, so
//! a second line sits below Gate 2: when `MemAvailable` falls below half of
//! `headroom_mib`, faber freezes the newest running Docker job, decided on every
//! memory-tick reading rather than on this loop's 20 s, at most [`MAX_FROZEN`]
//! for memory, and thaws them oldest first above `headroom_mib + band_mib`. A
//! failed reading neither freezes nor thaws. Only the freeze is gated on `"p75"`
//! and on the fallback ([`crate::reserve`]); the thaw runs on every Docker host in
//! every mode, so a job frozen before the fallback tripped, or before an operator
//! went back to `"max"`, still comes back.
//!
//! **Each freeze records why it was made** ([`Reason`]), and each loop thaws only
//! its own: the disk loop thaws every frozen job with its floor off, and would
//! otherwise thaw a memory freeze at once. Docker cannot add a label to a running
//! container, so the reason is kept in the process and on the `freeze` audit line
//! (`reason`), and a restart reads it back from there. A paused container with no
//! recorded reason — paused by an older binary, or by hand — is the disk loop's,
//! as it was before.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::audit::{now_ms, Entry};
use crate::config::DiskAdmission;
use crate::docker::{self, JobContainer, KIND_DIND, KIND_RUNNER};
use crate::host::{fmt_gib, DiskVolume};
use crate::limiter::MemReading;
use crate::reconcile::{faber_mint_ts, Orchestrator};
use crate::reserve::TripCause;

/// Below this much free space on the Docker volume, faber freezes the newest
/// running Docker job — ADR-0032 §2.
pub const CRITICAL_FLOOR_MIB: u64 = 3 * 1024;

/// Frozen jobs are thawed only once the Docker volume has more than this free —
/// ADR-0032 §2. The gap above [`CRITICAL_FLOOR_MIB`] is the hysteresis: a job
/// thawed the moment free space crept back over the critical floor would write
/// it straight back under, and be frozen again a tick later.
pub const THAW_ABOVE_MIB: u64 = 6 * 1024;

/// How often the critical floor is weighed: twice the disk loop's 10 s reading
/// interval, so each step decides on a reading taken after the last step could
/// show. Still fast against the incident's job, which wrote gigabytes within
/// minutes.
pub const FREEZE_TICK: Duration = Duration::from_secs(20);

/// The most jobs the critical floor holds frozen at once (#111). A freeze buys
/// time for space to come back; past this many, the disk is short for reasons
/// freezing more CI will not fix, and every further freeze is a job GitHub fails
/// about 10 minutes later for nothing.
pub const MAX_FROZEN: usize = 2;

/// How many ticks a job whose freeze or thaw failed is passed over before it is
/// tried again: long enough for the next candidate to be acted on, short enough
/// that a passing failure costs about a minute.
const RETRY_AFTER_TICKS: u64 = 3;

/// One running Docker job, as the critical floor weighs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    pub job_id: u64,
    /// When it was admitted (epoch ms): the timestamp its runner was named with at
    /// admission (ADR-0018).
    pub admitted_ms: u64,
    /// Its runner container is paused.
    pub frozen: bool,
    /// A recent freeze or thaw of it failed, so it is passed over for now and the
    /// next candidate is taken instead.
    pub backoff: bool,
    /// GitHub has finished the job its runner holds ([`Admitted::job_over`]), so
    /// there is nothing left in it for a freeze to save.
    pub job_over: bool,
}

/// What the critical floor does on one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Freeze this job's containers with `docker pause`.
    Freeze(u64),
    /// Thaw them with `docker unpause`: there is room again, or the floor is off.
    Thaw(u64),
    /// Thaw them because GitHub has finished the job, so its runner exits and its
    /// disk is freed (#111).
    Release(u64),
}

/// The critical floor's decision for one tick — ADR-0032 §2. Pure, so the policy
/// is testable without a Docker host; [`run_loop`] does the I/O around it.
///
/// - First, whatever the floor and the disk read: release the oldest frozen job
///   GitHub has finished (#111). Kept frozen it only holds its disk, and thawed
///   its runner exits and is torn down.
/// - With the admission floor unstated or `0`: never freeze, and thaw any job
///   found frozen, oldest first. Turning the floor off stops freezing, not
///   thawing, so free space is not consulted.
/// - With the floor on but no fresh reading of the Docker volume (`free_mib` is
///   `None`): hold. A stale reading is evidence of neither pressure nor relief.
/// - Below [`CRITICAL_FLOOR_MIB`]: freeze the most recently admitted job not yet
///   frozen — the one most likely still in the gigabytes of its first minutes —
///   unless [`MAX_FROZEN`] are frozen already. A job GitHub has finished is never
///   a candidate: freezing it would only stop it exiting.
/// - Above [`THAW_ABOVE_MIB`]: thaw the frozen job admitted longest ago — the one
///   most likely past its first minutes, and nearest its end.
/// - In between, or with nothing left to act on: hold.
///
/// At most one action per tick, both ways. A job in `backoff` is passed over and
/// the next candidate taken, so one job that cannot be moved does not hold up the
/// rest; it still counts against [`MAX_FROZEN`] while it is frozen. Ties on the
/// admission time go to the job id, so a tick decides the same way however the
/// jobs were listed.
pub fn decide(admission: DiskAdmission, free_mib: Option<u64>, jobs: &[Job]) -> Option<Action> {
    let age = |j: &&Job| (j.admitted_ms, j.job_id);
    let oldest_frozen = |only_over: bool| {
        jobs.iter()
            .filter(|j| j.frozen && !j.backoff && (j.job_over || !only_over))
            .min_by_key(age)
            .map(|j| j.job_id)
    };
    if let Some(id) = oldest_frozen(true) {
        return Some(Action::Release(id));
    }
    if admission.floor_gib().is_none() {
        return oldest_frozen(false).map(Action::Thaw);
    }
    let free = free_mib?;
    if free < CRITICAL_FLOOR_MIB {
        if jobs.iter().filter(|j| j.frozen).count() >= MAX_FROZEN {
            return None;
        }
        jobs.iter()
            .filter(|j| !j.frozen && !j.backoff && !j.job_over)
            .max_by_key(age)
            .map(|j| Action::Freeze(j.job_id))
    } else if free > THAW_ABOVE_MIB {
        oldest_frozen(false).map(Action::Thaw)
    } else {
        None
    }
}

/// Why a job was frozen, so each loop thaws only its own (ADR-0033 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// The critical disk floor (ADR-0032 §2). Also any paused job with no
    /// recorded reason, as before the memory freeze existed.
    Disk,
    /// The memory freeze (ADR-0033 §3).
    Memory,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Disk => "disk",
            Reason::Memory => "memory",
        }
    }
}

/// The `MemAvailable` below which the memory freeze freezes (ADR-0033 §3): half
/// of `headroom_mib`, so it moves when the operator moves Gate 2.
pub fn memory_line_mib(headroom_mib: u64) -> u64 {
    headroom_mib / 2
}

/// The memory freeze's decision on one reading — ADR-0033 §3. Pure, like
/// [`decide`], and shaped the same way:
///
/// - First, whatever the reading: release the oldest memory-frozen job GitHub has
///   finished (#111).
/// - A failed reading (`available_mib` is `None`): hold. It trips the fallback
///   instead ([`crate::reserve::Fallback::observe`]).
/// - Below [`memory_line_mib`]: freeze the newest job not yet frozen, unless
///   `may_freeze` is false (not `"p75"`, or cooling off outside the squeeze that
///   tripped it) or [`MAX_FROZEN`] are frozen for memory already.
/// - Above `headroom_mib + band_mib`: thaw the oldest — in every mode.
/// - In between: hold.
///
/// `jobs` holds only the jobs this loop may move: running ones, and those frozen
/// for memory. A job frozen for disk is not here at all.
pub fn decide_memory(
    may_freeze: bool,
    available_mib: Option<u64>,
    headroom_mib: u64,
    band_mib: u64,
    jobs: &[Job],
) -> Option<Action> {
    let age = |j: &&Job| (j.admitted_ms, j.job_id);
    let oldest_frozen = |only_over: bool| {
        jobs.iter()
            .filter(|j| j.frozen && !j.backoff && (j.job_over || !only_over))
            .min_by_key(age)
            .map(|j| j.job_id)
    };
    if let Some(id) = oldest_frozen(true) {
        return Some(Action::Release(id));
    }
    let available = available_mib?;
    if available < memory_line_mib(headroom_mib) {
        if !may_freeze || jobs.iter().filter(|j| j.frozen).count() >= MAX_FROZEN {
            return None;
        }
        jobs.iter()
            .filter(|j| !j.frozen && !j.backoff && !j.job_over)
            .max_by_key(age)
            .map(|j| Action::Freeze(j.job_id))
    } else if available > headroom_mib.saturating_add(band_mib) {
        oldest_frozen(false).map(Action::Thaw)
    } else {
        None
    }
}

/// The reason each runner this host has frozen was frozen for, by runner name:
/// what a restart reads back from the audit log's `freeze` lines, oldest first.
/// A runner whose last line thawed it is not here; nor is one with no reason on
/// its line, which the disk loop treats as its own.
pub fn reasons_from(entries: &[Entry]) -> HashMap<String, Reason> {
    let mut out = HashMap::new();
    for e in entries.iter().filter(|e| e.kind == "freeze") {
        let Some(runner) = e.runner.clone() else {
            continue;
        };
        match (e.action.as_deref(), e.reason.as_deref()) {
            (Some("frozen"), Some("memory")) => {
                out.insert(runner, Reason::Memory);
            }
            (Some("frozen"), _) => {
                out.insert(runner, Reason::Disk);
            }
            _ => {
                out.remove(&runner);
            }
        }
    }
    out
}

/// What one loop's tick decides on, and how it words what it did.
#[derive(Debug, Clone, Copy)]
enum Ctx {
    /// The critical disk floor.
    Disk {
        admission: DiskAdmission,
        /// The tighter fresh reading's free space, and which volume it is of
        /// (#410).
        free_mib: Option<u64>,
        volume: DiskVolume,
    },
    /// The memory freeze, on one reading.
    Memory {
        may_freeze: bool,
        available_mib: Option<u64>,
        headroom_mib: u64,
        band_mib: u64,
    },
}

impl Ctx {
    fn reason(&self) -> Reason {
        match self {
            Ctx::Disk { .. } => Reason::Disk,
            Ctx::Memory { .. } => Reason::Memory,
        }
    }

    fn decide(&self, jobs: &[Job]) -> Option<Action> {
        match *self {
            Ctx::Disk {
                admission,
                free_mib,
                ..
            } => decide(admission, free_mib, jobs),
            Ctx::Memory {
                may_freeze,
                available_mib,
                headroom_mib,
                band_mib,
            } => decide_memory(may_freeze, available_mib, headroom_mib, band_mib, jobs),
        }
    }

    /// Why a freeze (`freezing`) or a thaw happened.
    fn cause(&self, freezing: bool) -> String {
        match *self {
            Ctx::Disk {
                admission,
                free_mib,
                volume,
            } => cause(admission, free_mib, volume, freezing),
            Ctx::Memory {
                available_mib,
                headroom_mib,
                band_mib,
                ..
            } => memory_cause(available_mib, headroom_mib, band_mib, freezing),
        }
    }

    /// When a job this loop froze is thawed, for its claim's detail.
    fn thaws_when(&self) -> String {
        match *self {
            Ctx::Disk { volume, .. } => format!(
                "thaws once {} has more than {} GiB free",
                volume.describe(),
                fmt_gib(THAW_ABOVE_MIB)
            ),
            Ctx::Memory {
                headroom_mib,
                band_mib,
                ..
            } => format!(
                "thaws once the VM has more than {} MiB available",
                headroom_mib.saturating_add(band_mib)
            ),
        }
    }

    /// What a claim found frozen for this loop's reason reads.
    fn found(&self) -> String {
        match *self {
            Ctx::Disk { admission, .. } => match admission.floor_gib() {
                Some(_) => format!("found frozen; {}", self.thaws_when()),
                None => "found frozen with the disk admission floor off; thawing it".to_string(),
            },
            Ctx::Memory { .. } => format!("found frozen for memory; {}", self.thaws_when()),
        }
    }
}

/// Why the memory freeze froze or thawed a job.
fn memory_cause(
    available_mib: Option<u64>,
    headroom_mib: u64,
    band_mib: u64,
    freezing: bool,
) -> String {
    match available_mib {
        Some(a) if freezing => format!(
            "{a} MiB available in the VM, below the {} MiB critical line (half of headroom_mib; \
             ADR-0033 §3)",
            memory_line_mib(headroom_mib)
        ),
        Some(a) => format!(
            "{a} MiB available in the VM, above the {} MiB the memory freeze thaws at \
             (headroom_mib + band_mib)",
            headroom_mib.saturating_add(band_mib)
        ),
        None => "no reading of the VM's memory".to_string(),
    }
}

/// One of a job's two containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Runner,
    Dind,
}

/// The order a freeze or a thaw moves a job's containers in (ADR-0032 §2). A
/// freeze stops the job before its daemon: the runner, then dind. A thaw starts
/// the daemon before the job: dind, then the runner, so the job never runs
/// against a frozen daemon.
fn sequence(pause: bool) -> [Part; 2] {
    if pause {
        [Part::Runner, Part::Dind]
    } else {
        [Part::Dind, Part::Runner]
    }
}

/// The jobs whose last freeze or thaw failed, each with the tick it failed on.
///
/// Without this, a job whose `docker pause` keeps failing is the newest candidate
/// on every tick and nothing else is ever frozen — or, failing to unpause, the
/// oldest frozen one, and nothing else is ever thawed.
#[derive(Debug, Default)]
struct Failures {
    at: HashMap<u64, u64>,
}

impl Failures {
    /// A freeze or a thaw of `job_id` failed on tick `tick`.
    fn failed(&mut self, job_id: u64, tick: u64) {
        self.at.insert(job_id, tick);
    }

    /// One worked: nothing is held against the job any more.
    fn succeeded(&mut self, job_id: u64) {
        self.at.remove(&job_id);
    }

    /// Whether `job_id` is still passed over on tick `tick`.
    fn passes_over(&self, job_id: u64, tick: u64) -> bool {
        self.at
            .get(&job_id)
            .is_some_and(|&failed| tick.saturating_sub(failed) < RETRY_AFTER_TICKS)
    }

    /// Forget every job that is no longer running.
    fn retain(&mut self, running: impl IntoIterator<Item = u64>) {
        let running: HashSet<u64> = running.into_iter().collect();
        self.at.retain(|job_id, _| running.contains(job_id));
    }
}

/// A frozen claim as `/api/state` shows it (`inflight[].frozen`): since when it
/// has been frozen, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrozenView {
    pub since_ms: u64,
    /// What it was frozen for: `disk` or `memory` (ADR-0033 §3).
    pub reason: Reason,
    pub detail: String,
    /// The last attempt to thaw it, when that failed: without it, a job that
    /// cannot be unpaused reads as one simply waiting for room (#111). `null`
    /// until a thaw fails; the view goes with the freeze once a thaw works.
    pub thaw_failure: Option<ThawFailure>,
}

/// A failed thaw, as [`FrozenView::thaw_failure`] shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThawFailure {
    pub at_ms: u64,
    pub error: String,
}

/// An admitted Docker claim, as [`Orchestrator::admitted_docker_claims`] hands it
/// over.
#[derive(Debug)]
pub(crate) struct Admitted {
    pub job_id: u64,
    pub repo: String,
    /// The name its runner registered under (`faber-{job_id}-{ts}`). Its
    /// containers carry the same claim as `org.faber.id` ([`docker::claim_id`]).
    pub runner: String,
    /// When faber took the claim: the admission time to fall back on for a runner
    /// name with no timestamp in it.
    pub started_ms: u64,
    /// The claim's frozen state, which `/api/state` reads.
    pub frozen: Arc<std::sync::Mutex<Option<FrozenView>>>,
    /// The reconciler has seen GitHub finish the job this claim's frozen runner
    /// holds (#111). Set only while the job is frozen, and never cleared: a
    /// finished job stays finished.
    pub job_over: bool,
}

/// An admitted claim with its live containers, as this tick found them: the
/// runner, and its dind when the profile granted one.
struct Live<'a> {
    claim: &'a Admitted,
    admitted_ms: u64,
    runner: JobContainer,
    dind: Option<JobContainer>,
}

impl Live<'_> {
    /// The job as [`decide`] weighs it. Its runner's state is the job's.
    fn weighed(&self) -> Job {
        Job {
            job_id: self.claim.job_id,
            admitted_ms: self.admitted_ms,
            frozen: self.runner.paused,
            backoff: false,
            job_over: self.claim.job_over,
        }
    }

    /// The job's container for `part`, when it has one.
    fn part_mut(&mut self, part: Part) -> Option<&mut JobContainer> {
        match part {
            Part::Runner => Some(&mut self.runner),
            Part::Dind => self.dind.as_mut(),
        }
    }
}

/// Join the admitted claims to their live containers by `org.faber.id`. A claim
/// with no live runner container is not a running job yet — it is still ramping,
/// so there is nothing to freeze but a dind — or no longer one, and is left out.
fn live_jobs<'a>(claims: &'a [Admitted], containers: &[JobContainer]) -> Vec<Live<'a>> {
    claims
        .iter()
        .filter_map(|claim| {
            let id = docker::claim_id(&claim.runner);
            Some(Live {
                claim,
                admitted_ms: faber_mint_ts(&claim.runner).unwrap_or(claim.started_ms),
                runner: labelled(containers, id, KIND_RUNNER)?.clone(),
                dind: labelled(containers, id, KIND_DIND).cloned(),
            })
        })
        .collect()
}

/// The container of `kind` that belongs to `claim`, by its labels.
fn labelled<'c>(
    containers: &'c [JobContainer],
    claim: &str,
    kind: &str,
) -> Option<&'c JobContainer> {
    containers
        .iter()
        .find(|c| c.claim == claim && c.kind == kind)
}

/// The state a half-frozen job's dind has to be put in to match its runner —
/// `true` to pause it — or `None` when the two agree or there is no dind.
///
/// A freeze or a thaw that fails half-way leaves the two apart ([`apply`]), and a
/// job left half frozen is the worst of both: a frozen job whose daemon goes on
/// writing, or a running one whose daemon no longer answers. The runner's state is
/// the job's, so dind follows it.
fn dind_out_of_line(job: &Live<'_>) -> Option<bool> {
    let dind = job.dind.as_ref()?;
    (dind.paused != job.runner.paused).then_some(job.runner.paused)
}

/// What a claim's frozen state reads, from whether Docker has its runner paused:
/// kept while it stays frozen, so `since_ms` and the reason survive the ticks;
/// noted as found frozen when nothing in this process froze it (a previous one
/// did, and the restart adopted the container); cleared once it runs.
///
/// `reason` is what the job is frozen for, from the record of freezes, and
/// `found` the detail a job found frozen for it reads.
fn synced(
    prev: Option<FrozenView>,
    paused: bool,
    now_ms: u64,
    reason: Reason,
    found: impl FnOnce() -> String,
) -> Option<FrozenView> {
    match (prev, paused) {
        (_, false) => None,
        (Some(view), true) => Some(view),
        (None, true) => Some(FrozenView {
            since_ms: now_ms,
            reason,
            detail: found(),
            thaw_failure: None,
        }),
    }
}

/// Why a job GitHub has finished is thawed ([`Action::Release`]).
const RELEASE_CAUSE: &str =
    "GitHub has finished its job, so it is thawed to exit and free its disk";

/// Why a freeze or a thaw happened, for the log, the audit line and the claim.
fn cause(
    admission: DiskAdmission,
    free_mib: Option<u64>,
    volume: DiskVolume,
    freezing: bool,
) -> String {
    match (admission.floor_gib(), free_mib) {
        (None, _) => {
            "the disk admission floor is off, and nothing stays frozen without it".to_string()
        }
        (Some(_), Some(free)) if freezing => format!(
            "{} GiB free on {}, below the {} GiB critical floor",
            fmt_gib(free),
            volume.describe(),
            fmt_gib(CRITICAL_FLOOR_MIB)
        ),
        (Some(_), Some(free)) => format!(
            "{} GiB free on {}, above the {} GiB the critical floor thaws at",
            fmt_gib(free),
            volume.describe(),
            fmt_gib(THAW_ABOVE_MIB)
        ),
        (Some(_), None) => "no fresh reading of the Docker volume".to_string(),
    }
}

/// What both loops share: the reason each frozen runner was frozen for, keyed by
/// runner name. Behind one lock that a tick holds from start to finish, so the
/// two loops never move containers at the same time, and one never reads a
/// container the other has just paused before its reason is recorded.
type Book = Arc<tokio::sync::Mutex<HashMap<String, Reason>>>;

/// The reason `runner` is frozen for: what the book says, or [`Reason::Disk`] —
/// a paused container nobody recorded is the disk loop's, as it always was.
fn reason_of(book: &HashMap<String, Reason>, runner: &str) -> Reason {
    book.get(runner).copied().unwrap_or(Reason::Disk)
}

/// Run the critical disk floor and the memory freeze for the process life.
///
/// Spawned by the daemon on every host with a Docker profile, once adoption has
/// rebuilt the claims, so the first tick — taken at once — already sees what a
/// previous process left frozen, and the reasons are read back from the audit
/// log before it. With the admission floor unstated or `0` the disk loop never
/// freezes, but still thaws what it finds frozen for disk (ADR-0032 §2); the
/// memory loop never freezes outside `"p75"`, but still thaws what it finds
/// frozen for memory (ADR-0033 §3). `[host]` and `reserve` apply only on a
/// restart.
pub async fn run_loop(orch: Arc<Orchestrator>) {
    match orch.limiter().disk_admission().floor_gib() {
        Some(floor) => info!(
            "critical floor: below {} GiB free on the Docker volume, freezing the newest \
             Docker job, one every {}s and at most {MAX_FROZEN} at once, and thawing them \
             oldest first above {} GiB, or once GitHub has finished the job (ADR-0032; \
             admission floor {floor} GiB)",
            fmt_gib(CRITICAL_FLOOR_MIB),
            FREEZE_TICK.as_secs(),
            fmt_gib(THAW_ABOVE_MIB)
        ),
        None => debug!(
            "critical floor: the disk admission floor is off, so nothing is frozen; a job \
             found frozen is thawed (ADR-0032)"
        ),
    }
    let reasons = reasons_from(&orch.audit_log().read_kind("freeze"));
    // A job still frozen for memory means its squeeze is not over (ADR-0033 §3):
    // the second freeze the ADR allows through a cool-off survives the restart.
    if squeeze_at_boot(&reasons) {
        orch.limiter().resume_squeeze();
    }
    let book: Book = Arc::new(tokio::sync::Mutex::new(reasons));
    tokio::join!(
        disk_loop(orch.clone(), book.clone()),
        memory_loop(orch, book)
    );
}

/// The critical disk floor, every [`FREEZE_TICK`].
async fn disk_loop(orch: Arc<Orchestrator>, book: Book) {
    let mut failures = Failures::default();
    let mut tick_no: u64 = 0;
    loop {
        let limiter = orch.limiter();
        let reading = limiter.fresh_disk_reading(now_ms());
        let ctx = Ctx::Disk {
            admission: limiter.disk_admission(),
            free_mib: reading.map(|r| r.free_mib),
            volume: reading.map(|r| r.volume).unwrap_or_default(),
        };
        tick(&orch, &book, &mut failures, tick_no, ctx).await;
        tick_no += 1;
        tokio::time::sleep(FREEZE_TICK).await;
    }
}

/// The memory freeze (ADR-0033 §3), on every memory-tick reading. Also where
/// the fallback's trips and re-arms that the probe loop saw are audited.
async fn memory_loop(orch: Arc<Orchestrator>, book: Book) {
    let limiter = orch.limiter().clone();
    let (headroom, _) = limiter.memory_thresholds();
    if limiter.may_freeze_for_memory(now_ms()) {
        info!(
            "memory freeze: reserve = \"p75\" — below {} MiB available in the VM, freezing the \
             newest Docker job, at most {MAX_FROZEN} at once, and thawing them oldest first \
             above headroom_mib + band_mib (ADR-0033 §3)",
            memory_line_mib(headroom)
        );
    }
    let mut readings = limiter.memory_readings();
    let mut failures = Failures::default();
    let mut tick_no: u64 = 0;
    loop {
        if readings.changed().await.is_err() {
            return;
        }
        let reading: MemReading = *readings.borrow_and_update();
        for event in limiter.take_reserve_events() {
            orch.record_reserve(event).await;
        }
        let now = now_ms();
        let (headroom_mib, band_mib) = limiter.memory_thresholds();
        let may_freeze = limiter.may_freeze_for_memory(now);
        // Most readings need nothing listed: no memory freeze to hold or let go,
        // and none to make. Only those that do pay for the `docker ps`.
        let holding = book.lock().await.values().any(|r| *r == Reason::Memory);
        if !memory_acts(holding, may_freeze, reading.available_mib, headroom_mib) {
            continue;
        }
        let ctx = Ctx::Memory {
            may_freeze,
            available_mib: reading.available_mib,
            headroom_mib,
            band_mib,
        };
        let moved = tick(&orch, &book, &mut failures, tick_no, ctx).await;
        if let Some(event) = trip_after(&limiter, moved, now) {
            orch.record_reserve(event).await;
        }
        tick_no += 1;
    }
}

/// Whether a memory reading is worth listing containers for: a memory freeze to
/// hold or let go (`holding`), or one to make — freezing allowed and the reading
/// below the line. Most readings are neither, and cost no `docker ps`.
fn memory_acts(
    holding: bool,
    may_freeze: bool,
    available_mib: Option<u64>,
    headroom_mib: u64,
) -> bool {
    let low = available_mib.is_some_and(|a| a < memory_line_mib(headroom_mib));
    holding || (may_freeze && low)
}

/// The fallback trip a memory-loop action makes (ADR-0033 §4): a freeze trips
/// it, a thaw or a release does not.
fn trip_after(
    limiter: &crate::limiter::Limiter,
    moved: Option<Action>,
    now_ms: u64,
) -> Option<crate::reserve::Event> {
    match moved {
        Some(Action::Freeze(_)) => limiter.trip_reserve(TripCause::Freeze, now_ms),
        _ => None,
    }
}

/// Whether a restart finds a squeeze still on: a job its record of freezes says
/// is frozen for memory.
fn squeeze_at_boot(reasons: &HashMap<String, Reason>) -> bool {
    reasons.values().any(|r| *r == Reason::Memory)
}

/// The jobs the loop for `own` may move, as [`decide`] / [`decide_memory`] weigh
/// them: every running one, and those frozen for `own`. A job frozen for the
/// other reason is the other loop's, and is left out altogether — neither a
/// freeze candidate, since it is already frozen, nor one to thaw.
fn movable(jobs: &[Live<'_>], book: &HashMap<String, Reason>, own: Reason) -> Vec<Job> {
    jobs.iter()
        .filter(|j| !j.runner.paused || reason_of(book, &j.claim.runner) == own)
        .map(Live::weighed)
        .collect()
}

/// One tick of one loop: read what is running and what is frozen, repair a job
/// left half frozen, publish every claim's state, then take at most one decision
/// over the jobs this loop may move — the running ones, and those frozen for its
/// own reason. Every failure is logged here and costs only this tick, except a
/// freeze or a thaw that fails, which passes that job over for
/// [`RETRY_AFTER_TICKS`]. Returns the action that was carried out.
async fn tick(
    orch: &Orchestrator,
    book: &Book,
    failures: &mut Failures,
    tick_no: u64,
    ctx: Ctx,
) -> Option<Action> {
    let limiter = orch.limiter();
    // With Docker down there is nothing to read or move.
    if !limiter.docker_reachable() {
        return None;
    }
    let mut book = book.lock().await;
    let claims = orch.admitted_docker_claims().await;
    // A runner no longer admitted is no longer frozen by anyone.
    book.retain(|runner, _| claims.iter().any(|c| c.runner == *runner));
    if claims.is_empty() {
        return None;
    }
    let containers = match docker::live_job_containers().await {
        Ok(found) => found,
        Err(e) => {
            debug!("critical floor: listing job containers: {e} — nothing decided this tick");
            return None;
        }
    };
    let now = now_ms();
    let own = ctx.reason();
    let mut jobs = live_jobs(&claims, &containers);
    failures.retain(jobs.iter().map(|j| j.claim.job_id));
    for job in &mut jobs {
        settle(job).await;
    }
    for claim in &claims {
        let paused = jobs
            .iter()
            .any(|j| j.claim.job_id == claim.job_id && j.runner.paused);
        let reason = reason_of(&book, &claim.runner);
        let mut cell = claim.frozen.lock().unwrap();
        // Each loop words only its own: the other loop states the rest.
        if paused && reason != own && cell.is_none() {
            continue;
        }
        *cell = synced(cell.take(), paused, now, reason, || ctx.found());
    }

    let weighed: Vec<Job> = movable(&jobs, &book, own)
        .into_iter()
        .map(|j| Job {
            backoff: failures.passes_over(j.job_id, tick_no),
            ..j
        })
        .collect();
    let action = ctx.decide(&weighed)?;
    let (Action::Freeze(id) | Action::Thaw(id) | Action::Release(id)) = action;
    let job = jobs.iter_mut().find(|j| j.claim.job_id == id)?;
    let moved = match action {
        Action::Freeze(_) => freeze(orch, job, &ctx, now).await,
        Action::Thaw(_) => thaw(orch, job, own, ctx.cause(false), now).await,
        Action::Release(_) => thaw(orch, job, own, RELEASE_CAUSE.to_string(), now).await,
    };
    if moved {
        failures.succeeded(id);
        match action {
            Action::Freeze(_) => {
                book.insert(job.claim.runner.clone(), own);
            }
            Action::Thaw(_) | Action::Release(_) => {
                book.remove(&job.claim.runner);
            }
        }
        Some(action)
    } else {
        failures.failed(id, tick_no);
        None
    }
}

/// Bring a half-frozen job's dind into line with its runner ([`dind_out_of_line`]).
async fn settle(job: &mut Live<'_>) {
    let Some(pause) = dind_out_of_line(job) else {
        return;
    };
    let job_id = job.claim.job_id;
    let Some(dind) = job.dind.as_mut() else {
        return;
    };
    info!(
        "critical floor: job {job_id}'s dind is {} while its runner is {} — {} it to match",
        if dind.paused { "frozen" } else { "running" },
        if pause { "frozen" } else { "running" },
        if pause { "freezing" } else { "thawing" }
    );
    if set_paused(dind, pause).await.is_ok() {
        dind.paused = pause;
    }
}

/// Freeze a job, then say so: in the log, in the audit trail, and on its claim.
/// `false` when its runner would not move.
async fn freeze(orch: &Orchestrator, job: &mut Live<'_>, ctx: &Ctx, now: u64) -> bool {
    if apply(job, true, move_container).await.is_err() {
        return false;
    }
    let claim = job.claim;
    let (cause, thaws, reason) = (ctx.cause(true), ctx.thaws_when(), ctx.reason());
    warn!(
        "{} freeze: froze job {} of {} (runner {}) — {cause}; it {thaws}",
        reason.as_str(),
        claim.job_id,
        claim.repo,
        claim.runner,
    );
    *claim.frozen.lock().unwrap() = Some(FrozenView {
        since_ms: now,
        reason,
        detail: format!("frozen at {cause}; {thaws}"),
        thaw_failure: None,
    });
    orch.record_freeze(
        &claim.repo,
        claim.job_id,
        &claim.runner,
        "frozen",
        reason.as_str(),
        cause,
    )
    .await;
    true
}

/// Thaw a job, then say so, the same three ways. `false` when it is still frozen,
/// and then the failure goes on its claim, so `/api/state` shows why.
async fn thaw(
    orch: &Orchestrator,
    job: &mut Live<'_>,
    reason: Reason,
    cause: String,
    now: u64,
) -> bool {
    if let Err(error) = apply(job, false, move_container).await {
        let mut cell = job.claim.frozen.lock().unwrap();
        *cell = thaw_failed(cell.take(), now, error);
        return false;
    }
    let claim = job.claim;
    info!(
        "{} freeze: thawed job {} of {} (runner {}) — {cause}",
        reason.as_str(),
        claim.job_id,
        claim.repo,
        claim.runner
    );
    *claim.frozen.lock().unwrap() = None;
    orch.record_freeze(
        &claim.repo,
        claim.job_id,
        &claim.runner,
        "thawed",
        reason.as_str(),
        cause,
    )
    .await;
    true
}

/// A frozen claim's view once a thaw of it has failed with `error`: the failure
/// replaces any earlier one, and the rest of the view is kept. A claim with no
/// view is not frozen as far as `/api/state` knows, and is left alone.
fn thaw_failed(view: Option<FrozenView>, at_ms: u64, error: String) -> Option<FrozenView> {
    view.map(|v| FrozenView {
        thaw_failure: Some(ThawFailure { at_ms, error }),
        ..v
    })
}

/// Freeze (`pause`) or thaw a job: move its containers in [`sequence`] through
/// `mover`, which is [`move_container`] outside a test. The first container that
/// will not move stops the rest, so a daemon that stays frozen never has its job
/// thawed against it. `Ok` when the runner — the job — ends in the state asked
/// for; a dind left behind is brought into line on the next tick ([`settle`]).
/// Otherwise the error of the container that would not move.
async fn apply<M, F>(job: &mut Live<'_>, pause: bool, mut mover: M) -> Result<(), String>
where
    M: FnMut(JobContainer, bool) -> F,
    F: std::future::Future<Output = Result<(), String>>,
{
    let mut failed = None;
    for part in sequence(pause) {
        let Some(container) = job.part_mut(part) else {
            continue;
        };
        if container.paused == pause {
            continue;
        }
        if let Err(e) = mover(container.clone(), pause).await {
            failed = Some(e);
            break;
        }
        container.paused = pause;
    }
    if job.runner.paused == pause {
        Ok(())
    } else {
        Err(failed.unwrap_or_default())
    }
}

/// Pause or unpause one container for real: [`apply`]'s mover outside a test.
async fn move_container(container: JobContainer, pause: bool) -> Result<(), String> {
    set_paused(&container, pause).await
}

/// `docker pause` (`pause`) or `docker unpause` one container, logging a failure.
/// The error says which container would not move, and why.
async fn set_paused(c: &JobContainer, pause: bool) -> Result<(), String> {
    let moved = if pause {
        docker::pause_container(&c.id).await
    } else {
        docker::unpause_container(&c.id).await
    };
    moved.map_err(|e| {
        let error = format!(
            "{} the {} container {}: {e}",
            if pause { "pausing" } else { "unpausing" },
            c.kind,
            c.id
        );
        warn!("critical floor: {error} of claim {}", c.claim);
        error
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024;

    fn job(job_id: u64, admitted_ms: u64, frozen: bool) -> Job {
        Job {
            job_id,
            admitted_ms,
            frozen,
            backoff: false,
            job_over: false,
        }
    }

    /// Three running jobs, admitted out of id order: 2 is the oldest, 3 the newest.
    fn three() -> Vec<Job> {
        vec![job(1, 200, false), job(2, 100, false), job(3, 300, false)]
    }

    /// The same three, all frozen.
    fn three_frozen() -> Vec<Job> {
        three()
            .into_iter()
            .map(|j| Job { frozen: true, ..j })
            .collect()
    }

    const FLOOR: DiskAdmission = DiskAdmission::Floor(10);

    /// Play `decide` forward the way the loop does — one action per tick, applied
    /// to the jobs before the next — and say what each tick did.
    fn run(
        admission: DiskAdmission,
        jobs: &mut [Job],
        free_mib: Option<u64>,
        ticks: usize,
    ) -> Vec<Option<Action>> {
        (0..ticks)
            .map(|_| {
                let action = decide(admission, free_mib, jobs);
                for j in jobs.iter_mut() {
                    match action {
                        Some(Action::Freeze(id)) if id == j.job_id => j.frozen = true,
                        Some(Action::Thaw(id)) if id == j.job_id => j.frozen = false,
                        _ => {}
                    }
                }
                action
            })
            .collect()
    }

    #[test]
    fn below_the_critical_floor_the_newest_job_is_frozen_first_then_one_per_tick() {
        let mut jobs = three();
        assert_eq!(
            run(FLOOR, &mut jobs, Some(2 * GIB), 4),
            [Some(Action::Freeze(3)), Some(Action::Freeze(1)), None, None],
            "newest first, one per tick, until {MAX_FROZEN} are frozen"
        );
        assert_eq!(
            jobs.iter().filter(|j| j.frozen).count(),
            MAX_FROZEN,
            "the oldest job is left running"
        );
        assert!(!jobs[1].frozen);
    }

    #[test]
    fn no_more_than_two_jobs_are_frozen_at_once() {
        // #111: a disk short for reasons outside CI would otherwise freeze every job
        // on the host, one per tick.
        let mut jobs: Vec<Job> = (1..=6).map(|id| job(id, id * 100, false)).collect();
        let froze = run(FLOOR, &mut jobs, Some(0), 6)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_eq!(froze, [Action::Freeze(6), Action::Freeze(5)]);
        // One that will not thaw still counts: it is still frozen, and still holds
        // its disk.
        let mut held = three();
        held[0].frozen = true;
        held[0].backoff = true;
        held[1].frozen = true;
        assert_eq!(decide(FLOOR, Some(GIB), &held), None);
        // Once one is thawed, the next newest can be frozen in its place.
        held[1].frozen = false;
        assert_eq!(decide(FLOOR, Some(GIB), &held), Some(Action::Freeze(3)));
    }

    #[test]
    fn a_frozen_job_github_has_finished_is_released_whatever_the_disk_reads() {
        // #111: GitHub fails a frozen job about 10 minutes in, and kept frozen it
        // only holds its disk, because the reclaim guard still sees its worker.
        // Thawed, its runner exits and the normal teardown frees its disk.
        let mut jobs = three_frozen();
        jobs[2].job_over = true;
        for admission in [FLOOR, DiskAdmission::Off, DiskAdmission::Unstated] {
            for free in [Some(0), Some(2 * GIB), Some(4 * GIB), Some(8 * GIB), None] {
                assert_eq!(
                    decide(admission, free, &jobs),
                    Some(Action::Release(3)),
                    "{admission:?} at {free:?}: before any other thaw"
                );
            }
        }
        // Oldest first among the finished, and one in backoff is passed over.
        jobs[1].job_over = true;
        assert_eq!(decide(FLOOR, Some(0), &jobs), Some(Action::Release(2)));
        jobs[1].backoff = true;
        assert_eq!(decide(FLOOR, Some(0), &jobs), Some(Action::Release(3)));
    }

    #[test]
    fn a_job_github_has_finished_is_never_frozen_again() {
        // Released, it runs only to exit. Freezing it again would stop that, and
        // the newest unfrozen job below the floor is exactly where it lands.
        let mut jobs = three();
        jobs[2].job_over = true;
        assert_eq!(decide(FLOOR, Some(GIB), &jobs), Some(Action::Freeze(1)));
        let over: Vec<Job> = three()
            .into_iter()
            .map(|j| Job {
                job_over: true,
                ..j
            })
            .collect();
        assert_eq!(decide(FLOOR, Some(GIB), &over), None);
    }

    #[test]
    fn a_frozen_cascade_on_a_disk_that_never_recovers_ends() {
        // The issue's scenario: free space stays under 3 GiB for reasons outside
        // CI. Two jobs are frozen; GitHub fails both; both are released and exit;
        // two more are frozen in their place. Nothing is frozen for longer than
        // GitHub takes to fail it, and no more than two at a time.
        let mut jobs: Vec<Job> = (1..=4).map(|id| job(id, id * 100, false)).collect();
        assert_eq!(
            run(FLOOR, &mut jobs, Some(GIB), 3),
            [Some(Action::Freeze(4)), Some(Action::Freeze(3)), None]
        );
        // GitHub fails the two frozen jobs.
        for j in jobs.iter_mut().filter(|j| j.frozen) {
            j.job_over = true;
        }
        let mut steps = Vec::new();
        for _ in 0..4 {
            let action = decide(FLOOR, Some(GIB), &jobs);
            steps.push(action);
            match action {
                Some(Action::Release(id)) => {
                    // Thawed, its runner exits and its claim is gone.
                    jobs.retain(|j| j.job_id != id);
                }
                Some(Action::Freeze(id)) => {
                    jobs.iter_mut().find(|j| j.job_id == id).unwrap().frozen = true;
                }
                _ => {}
            }
        }
        assert_eq!(
            steps,
            [
                Some(Action::Release(3)),
                Some(Action::Release(4)),
                Some(Action::Freeze(2)),
                Some(Action::Freeze(1)),
            ]
        );
    }

    #[test]
    fn once_there_is_room_the_oldest_frozen_job_is_thawed_first_then_one_per_tick() {
        let mut jobs = three_frozen();
        assert_eq!(
            run(FLOOR, &mut jobs, Some(6 * GIB + 1), 4),
            [
                Some(Action::Thaw(2)),
                Some(Action::Thaw(1)),
                Some(Action::Thaw(3)),
                None,
            ]
        );
        assert!(jobs.iter().all(|j| !j.frozen));
    }

    #[test]
    fn between_three_and_six_gib_nothing_moves() {
        // The hysteresis: 3 GiB exactly is not below the critical floor, and 6 GiB
        // exactly is not above the thaw level.
        let running = three();
        let mut frozen = three();
        frozen[0].frozen = true;
        for free in [3 * GIB, 4 * GIB + 512, 6 * GIB] {
            assert_eq!(decide(FLOOR, Some(free), &running), None, "{free} MiB");
            assert_eq!(decide(FLOOR, Some(free), &frozen), None, "{free} MiB");
        }
        assert_eq!(
            decide(FLOOR, Some(3 * GIB - 1), &running),
            Some(Action::Freeze(3))
        );
        assert_eq!(
            decide(FLOOR, Some(6 * GIB + 1), &frozen),
            Some(Action::Thaw(1))
        );
    }

    #[test]
    fn a_freeze_skips_the_jobs_already_frozen_and_a_thaw_the_ones_running() {
        let jobs = [job(1, 100, false), job(2, 200, false), job(3, 300, true)];
        assert_eq!(decide(FLOOR, Some(GIB), &jobs), Some(Action::Freeze(2)));
        assert_eq!(decide(FLOOR, Some(8 * GIB), &jobs), Some(Action::Thaw(3)));
        assert_eq!(decide(FLOOR, Some(GIB), &[]), None, "nothing to freeze");
    }

    #[test]
    fn ties_on_the_admission_time_go_to_the_job_id() {
        let jobs = [job(7, 100, false), job(5, 100, false)];
        assert_eq!(decide(FLOOR, Some(GIB), &jobs), Some(Action::Freeze(7)));
        let jobs = [job(7, 100, true), job(5, 100, true)];
        assert_eq!(decide(FLOOR, Some(8 * GIB), &jobs), Some(Action::Thaw(5)));
    }

    #[test]
    fn turning_the_floor_off_stops_freezing_not_thawing() {
        // Unstated or 0: nothing is ever frozen, however full the disk reads. A job
        // frozen under a floor since withdrawn is let go — oldest first, one per
        // tick, whatever the disk reads and whether or not it has been read —
        // rather than left paused, for ever with `runner_timeout_secs = 0`.
        for admission in [DiskAdmission::Unstated, DiskAdmission::Off] {
            for free in [Some(0), Some(GIB), None, Some(100 * GIB)] {
                assert_eq!(decide(admission, free, &three()), None, "{free:?}");
            }
            for free in [Some(0), None] {
                let mut frozen = three_frozen();
                assert_eq!(
                    run(admission, &mut frozen, free, 4),
                    [
                        Some(Action::Thaw(2)),
                        Some(Action::Thaw(1)),
                        Some(Action::Thaw(3)),
                        None,
                    ],
                    "{admission:?} at {free:?}"
                );
            }
        }
    }

    #[test]
    fn with_the_floor_on_and_no_fresh_reading_nothing_is_frozen_or_thawed() {
        let mut mixed = three();
        mixed[0].frozen = true;
        assert_eq!(decide(FLOOR, None, &mixed), None);
    }

    #[test]
    fn a_job_that_will_not_move_is_passed_over_for_the_next() {
        // The newest job cannot be frozen: the next newest is, not nobody.
        let mut jobs = three();
        jobs[2].backoff = true;
        assert_eq!(decide(FLOOR, Some(GIB), &jobs), Some(Action::Freeze(1)));
        // The oldest frozen one cannot be thawed: the next oldest is.
        let mut frozen = three_frozen();
        frozen[1].backoff = true;
        assert_eq!(decide(FLOOR, Some(8 * GIB), &frozen), Some(Action::Thaw(1)));
        assert_eq!(
            decide(DiskAdmission::Off, None, &frozen),
            Some(Action::Thaw(1))
        );
        // Every candidate passed over: nothing this tick.
        let stuck: Vec<Job> = three()
            .into_iter()
            .map(|j| Job { backoff: true, ..j })
            .collect();
        assert_eq!(decide(FLOOR, Some(GIB), &stuck), None);
    }

    #[test]
    fn a_failed_job_is_passed_over_for_a_few_ticks_then_tried_again() {
        let mut failures = Failures::default();
        assert!(!failures.passes_over(7, 5));
        failures.failed(7, 5);
        for tick in 5..5 + RETRY_AFTER_TICKS {
            assert!(failures.passes_over(7, tick), "tick {tick}");
        }
        assert!(
            !failures.passes_over(7, 5 + RETRY_AFTER_TICKS),
            "tried again"
        );
        assert!(!failures.passes_over(8, 6), "only the job that failed");
        // A move that works clears it.
        failures.failed(7, 9);
        failures.succeeded(7);
        assert!(!failures.passes_over(7, 9));
        // A job no longer running is forgotten.
        failures.failed(7, 10);
        failures.failed(8, 10);
        failures.retain([8]);
        assert!(!failures.passes_over(7, 10));
        assert!(failures.passes_over(8, 10));
    }

    // --- moving a job's containers -------------------------------------------

    #[test]
    fn a_freeze_stops_the_job_first_and_a_thaw_starts_its_daemon_first() {
        assert_eq!(sequence(true), [Part::Runner, Part::Dind]);
        assert_eq!(sequence(false), [Part::Dind, Part::Runner]);
    }

    /// `apply` against a mover that records every move and refuses to move a
    /// container of kind `refuse`.
    async fn moves(job: &mut Live<'_>, pause: bool, refuse: Option<&str>) -> (bool, Vec<String>) {
        let mut calls = Vec::new();
        let moved = apply(job, pause, |c: JobContainer, _| {
            let moved = if refuse == Some(c.kind.as_str()) {
                Err(format!("{} refused", c.kind))
            } else {
                Ok(())
            };
            calls.push(c.kind);
            async move { moved }
        })
        .await;
        (moved.is_ok(), calls)
    }

    #[tokio::test]
    async fn a_thaw_unpauses_the_daemon_before_the_runner() {
        let claim = admitted(7, "faber-7-1000");
        let mut job = live(&claim, true, Some(true));
        assert_eq!(
            moves(&mut job, false, None).await,
            (true, vec![KIND_DIND.to_string(), KIND_RUNNER.to_string()])
        );
        assert!(!job.runner.paused && job.dind.as_ref().is_some_and(|d| !d.paused));
        // And a freeze the other way round: the job stops before its daemon.
        assert_eq!(
            moves(&mut job, true, None).await,
            (true, vec![KIND_RUNNER.to_string(), KIND_DIND.to_string()])
        );
        assert!(job.runner.paused && job.dind.as_ref().is_some_and(|d| d.paused));
    }

    #[tokio::test]
    async fn a_daemon_that_will_not_thaw_keeps_its_job_frozen() {
        let claim = admitted(7, "faber-7-1000");
        let mut job = live(&claim, true, Some(true));
        assert_eq!(
            moves(&mut job, false, Some(KIND_DIND)).await,
            (false, vec![KIND_DIND.to_string()]),
            "the runner is never thawed against a frozen daemon"
        );
        assert!(job.runner.paused);
        // A runner that will not thaw after its daemon did is still frozen, and its
        // daemon is put back with it on the next tick.
        assert_eq!(
            moves(&mut job, false, Some(KIND_RUNNER)).await,
            (false, vec![KIND_DIND.to_string(), KIND_RUNNER.to_string()])
        );
        assert_eq!(dind_out_of_line(&job), Some(true));
    }

    #[tokio::test]
    async fn a_runner_that_will_not_freeze_leaves_its_daemon_running() {
        let claim = admitted(7, "faber-7-1000");
        let mut job = live(&claim, false, Some(false));
        assert_eq!(
            moves(&mut job, true, Some(KIND_RUNNER)).await,
            (false, vec![KIND_RUNNER.to_string()])
        );
        assert!(job.dind.as_ref().is_some_and(|d| !d.paused));
        // A daemon that will not freeze after its runner did: the job is frozen, and
        // its daemon follows on the next tick.
        assert_eq!(
            moves(&mut job, true, Some(KIND_DIND)).await,
            (true, vec![KIND_RUNNER.to_string(), KIND_DIND.to_string()])
        );
        assert_eq!(dind_out_of_line(&job), Some(true));
    }

    #[tokio::test]
    async fn only_the_containers_out_of_place_are_moved() {
        let claim = admitted(7, "faber-7-1000");
        // A job with no daemon is its runner alone.
        let mut job = live(&claim, true, None);
        assert_eq!(
            moves(&mut job, false, None).await,
            (true, vec![KIND_RUNNER.to_string()])
        );
        // A daemon already frozen is not frozen again.
        let mut job = live(&claim, false, Some(true));
        assert_eq!(
            moves(&mut job, true, None).await,
            (true, vec![KIND_RUNNER.to_string()])
        );
    }

    #[test]
    fn a_cause_says_what_decided_it() {
        assert_eq!(
            cause(FLOOR, Some(2867), DiskVolume::Docker, true),
            "2.8 GiB free on the Docker volume, below the 3 GiB critical floor"
        );
        assert_eq!(
            cause(FLOOR, Some(6656), DiskVolume::Docker, false),
            "6.5 GiB free on the Docker volume, above the 6 GiB the critical floor thaws at"
        );
        // Where two volumes are read, the one that bound is named (#410).
        assert_eq!(
            cause(FLOOR, Some(2867), DiskVolume::LinuxVm, true),
            "2.8 GiB free on the Linux VM's disk, below the 3 GiB critical floor"
        );
        assert_eq!(
            cause(DiskAdmission::Off, None, DiskVolume::Docker, false),
            "the disk admission floor is off, and nothing stays frozen without it"
        );
    }

    // --- joining claims to their containers -----------------------------------

    fn admitted(job_id: u64, runner: &str) -> Admitted {
        Admitted {
            job_id,
            repo: "owner/demo".to_string(),
            runner: runner.to_string(),
            started_ms: 42,
            frozen: Arc::new(std::sync::Mutex::new(None)),
            job_over: false,
        }
    }

    fn ctr(id: &str, kind: &str, claim: &str, paused: bool) -> JobContainer {
        JobContainer {
            id: id.to_string(),
            claim: claim.to_string(),
            kind: kind.to_string(),
            paused,
        }
    }

    /// Claim 7's job, its runner and dind paused or not.
    fn live(claim: &Admitted, runner: bool, dind: Option<bool>) -> Live<'_> {
        Live {
            claim,
            admitted_ms: 1000,
            runner: ctr("r7", KIND_RUNNER, "7-1000", runner),
            dind: dind.map(|paused| ctr("d7", KIND_DIND, "7-1000", paused)),
        }
    }

    #[test]
    fn a_claim_is_a_running_job_once_its_runner_container_is_live() {
        let claims = [
            admitted(7, "faber-7-1000"),
            admitted(8, "faber-8-2000"),
            admitted(9, "faber-9-3000"),
        ];
        let containers = [
            ctr("r7", KIND_RUNNER, "7-1000", true),
            ctr("d7", KIND_DIND, "7-1000", true),
            // A denied profile's job has no dind at all.
            ctr("r8", KIND_RUNNER, "8-2000", false),
            // Still ramping: its dind is up, its runner not yet.
            ctr("d9", KIND_DIND, "9-3000", false),
            // Another claim's leftover, joined to nothing here.
            ctr("r6", KIND_RUNNER, "6-500", false),
        ];
        let jobs = live_jobs(&claims, &containers);
        assert_eq!(
            jobs.iter().map(Live::weighed).collect::<Vec<_>>(),
            [job(7, 1000, true), job(8, 2000, false)],
            "admitted when its runner was named; frozen when its runner is paused"
        );
        assert_eq!(jobs[0].dind.as_ref().map(|d| d.id.as_str()), Some("d7"));
        assert!(jobs[1].dind.is_none());
    }

    /// #94: a runner named with this host's instance suffix joins its containers
    /// by the same claim id they were labelled with, and its admission time is
    /// still read from the name's second field.
    #[test]
    fn an_instance_suffixed_runner_joins_its_containers() {
        let claims = [admitted(7, "faber-7-1000-builder")];
        let containers = [
            ctr("r7", KIND_RUNNER, "7-1000-builder", true),
            ctr("d7", KIND_DIND, "7-1000-builder", true),
        ];
        let jobs = live_jobs(&claims, &containers);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].weighed(), job(7, 1000, true));
        assert_eq!(jobs[0].dind.as_ref().map(|d| d.id.as_str()), Some("d7"));
    }

    #[test]
    fn a_runner_name_without_a_timestamp_falls_back_to_the_claim() {
        let claims = [admitted(7, "faber-7")];
        let containers = [ctr("r7", KIND_RUNNER, "7", false)];
        let jobs = live_jobs(&claims, &containers);
        assert_eq!(jobs[0].weighed(), job(7, 42, false));
    }

    #[test]
    fn a_half_frozen_jobs_dind_follows_its_runner() {
        let claim = admitted(7, "faber-7-1000");
        // A runner frozen beside a dind still writing: freeze the dind.
        assert_eq!(
            dind_out_of_line(&live(&claim, true, Some(false))),
            Some(true)
        );
        // A runner thawed beside a frozen dind: thaw the dind.
        assert_eq!(
            dind_out_of_line(&live(&claim, false, Some(true))),
            Some(false)
        );
        // In line, or no dind: nothing to do.
        assert_eq!(dind_out_of_line(&live(&claim, true, Some(true))), None);
        assert_eq!(dind_out_of_line(&live(&claim, false, Some(false))), None);
        assert_eq!(dind_out_of_line(&live(&claim, true, None)), None);
    }

    #[test]
    fn a_frozen_state_is_kept_found_or_cleared_from_what_docker_reports() {
        let froze = FrozenView {
            since_ms: 1,
            reason: Reason::Disk,
            detail: "frozen at 2.8 GiB free".to_string(),
            thaw_failure: Some(ThawFailure {
                at_ms: 50,
                error: "unpausing the runner container r7: timed out".to_string(),
            }),
        };
        // Still frozen: kept, so since-when, the reason and a failed thaw survive
        // the ticks.
        let disk = |admission| Ctx::Disk {
            admission,
            free_mib: None,
            volume: DiskVolume::Docker,
        };
        let found_for = |ctx: Ctx| move || ctx.found();
        assert_eq!(
            synced(
                Some(froze.clone()),
                true,
                99,
                Reason::Disk,
                found_for(disk(FLOOR))
            ),
            Some(froze.clone())
        );
        // Frozen by a previous process, and adopted: found, from now. With the
        // floor off it is shown frozen until it is thawed, not as running.
        let found =
            synced(None, true, 99, Reason::Disk, found_for(disk(FLOOR))).expect("found frozen");
        assert_eq!(found.since_ms, 99);
        assert_eq!(found.reason, Reason::Disk);
        assert!(found.detail.starts_with("found frozen; thaws"), "{found:?}");
        let off = synced(
            None,
            true,
            99,
            Reason::Disk,
            found_for(disk(DiskAdmission::Off)),
        )
        .expect("found frozen");
        assert!(off.detail.contains("floor off; thawing it"), "{off:?}");
        // Found frozen for memory, after a restart: says so, and when it thaws.
        let memory = Ctx::Memory {
            may_freeze: false,
            available_mib: None,
            headroom_mib: 1536,
            band_mib: 512,
        };
        let mem = synced(None, true, 99, Reason::Memory, found_for(memory)).expect("found");
        assert_eq!(mem.reason, Reason::Memory);
        assert!(
            mem.detail.contains("for memory") && mem.detail.contains("2048 MiB"),
            "{mem:?}"
        );
        // Running again, whoever thawed it.
        assert_eq!(
            synced(Some(froze), false, 99, Reason::Disk, found_for(disk(FLOOR))),
            None
        );
        assert_eq!(
            synced(
                None,
                false,
                99,
                Reason::Disk,
                found_for(disk(DiskAdmission::Unstated))
            ),
            None
        );
    }

    #[test]
    fn the_frozen_view_contract_field_names() {
        // The dashboard reads these by name, and `thaw_failure` is always there:
        // `null` until a thaw fails.
        let view = FrozenView {
            since_ms: 1_790_000_000_000,
            reason: Reason::Memory,
            detail: "frozen at 2.8 GiB free".to_string(),
            thaw_failure: None,
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::json!({
                "since_ms": 1_790_000_000_000u64,
                "reason": "memory",
                "detail": "frozen at 2.8 GiB free",
                "thaw_failure": null,
            })
        );
        let failed = thaw_failed(Some(view), 1_790_000_060_000, "timed out".to_string());
        assert_eq!(
            serde_json::to_value(failed).unwrap()["thaw_failure"],
            serde_json::json!({"at_ms": 1_790_000_060_000u64, "error": "timed out"})
        );
    }

    #[test]
    fn a_failed_thaw_is_shown_on_the_frozen_claim() {
        let view = FrozenView {
            since_ms: 1,
            reason: Reason::Disk,
            detail: "frozen at 2.8 GiB free; thaws once it has more than 6 GiB free".to_string(),
            thaw_failure: None,
        };
        let failed = thaw_failed(Some(view.clone()), 5, "first".to_string()).expect("still frozen");
        assert_eq!(failed.since_ms, 1, "the freeze it belongs to is kept");
        assert_eq!(failed.detail, view.detail);
        assert_eq!(
            failed.thaw_failure,
            Some(ThawFailure {
                at_ms: 5,
                error: "first".to_string()
            })
        );
        // The latest failure replaces the one before it.
        let again = thaw_failed(Some(failed), 9, "second".to_string()).expect("still frozen");
        assert_eq!(
            again.thaw_failure.map(|f| (f.at_ms, f.error)),
            Some((9, "second".to_string()))
        );
        // A claim not shown frozen gains nothing.
        assert_eq!(thaw_failed(None, 9, "x".to_string()), None);
    }

    #[tokio::test]
    async fn a_failed_move_says_which_container_would_not_move() {
        let claim = admitted(7, "faber-7-1000");
        let mut job = live(&claim, true, Some(true));
        let refused = apply(&mut job, false, |c: JobContainer, _| {
            let moved = if c.kind == KIND_DIND {
                Err(format!(
                    "unpausing the {} container {}: no such container",
                    c.kind, c.id
                ))
            } else {
                Ok(())
            };
            async move { moved }
        })
        .await;
        assert_eq!(
            refused,
            Err("unpausing the dind container d7: no such container".to_string())
        );
        // A daemon that will not freeze after its runner did is not a failure: the
        // job is frozen, and its daemon follows on the next tick.
        let mut job = live(&claim, false, Some(false));
        let frozen = apply(&mut job, true, |c: JobContainer, _| {
            let moved = if c.kind == KIND_DIND {
                Err("refused".to_string())
            } else {
                Ok(())
            };
            async move { moved }
        })
        .await;
        assert_eq!(frozen, Ok(()));
    }

    // --- the memory freeze (ADR-0033 §3) -------------------------------------

    const HEADROOM: u64 = 1536;
    const BAND: u64 = 512;

    /// `decide_memory` at a reading of `available` MiB, freezing allowed or not.
    fn mem(may_freeze: bool, available: Option<u64>, jobs: &[Job]) -> Option<Action> {
        decide_memory(may_freeze, available, HEADROOM, BAND, jobs)
    }

    /// Play `decide_memory` forward one reading at a time, as the memory loop does.
    fn run_mem(jobs: &mut [Job], available: Option<u64>, readings: usize) -> Vec<Option<Action>> {
        (0..readings)
            .map(|_| {
                let action = mem(true, available, jobs);
                for j in jobs.iter_mut() {
                    match action {
                        Some(Action::Freeze(id)) if id == j.job_id => j.frozen = true,
                        Some(Action::Thaw(id)) if id == j.job_id => j.frozen = false,
                        _ => {}
                    }
                }
                action
            })
            .collect()
    }

    #[test]
    fn below_half_of_headroom_the_newest_job_is_frozen_then_one_per_reading_up_to_two() {
        assert_eq!(memory_line_mib(HEADROOM), 768, "768 MiB at the default");
        let mut jobs: Vec<Job> = (1..=4).map(|id| job(id, id * 100, false)).collect();
        assert_eq!(
            run_mem(&mut jobs, Some(767), 4),
            [Some(Action::Freeze(4)), Some(Action::Freeze(3)), None, None],
            "newest first, one per reading, at most {MAX_FROZEN}"
        );
        // 768 exactly is not below the line.
        assert_eq!(mem(true, Some(768), &three()), None);
    }

    #[test]
    fn the_memory_freeze_thaws_oldest_first_above_headroom_plus_band() {
        let mut frozen = three_frozen();
        assert_eq!(mem(true, Some(HEADROOM + BAND), &frozen), None, "not above");
        assert_eq!(
            run_mem(&mut frozen, Some(HEADROOM + BAND + 1), 4),
            [
                Some(Action::Thaw(2)),
                Some(Action::Thaw(1)),
                Some(Action::Thaw(3)),
                None
            ]
        );
        // Between the line and the thaw level, nothing moves either way.
        let mut one = three();
        one[0].frozen = true;
        for a in [768, 1000, HEADROOM, HEADROOM + BAND] {
            assert_eq!(mem(true, Some(a), &one), None, "{a}");
        }
    }

    #[test]
    fn under_max_or_in_a_cool_off_nothing_is_frozen_but_the_thaw_still_runs() {
        // `may_freeze` is false under "max", and in a cool-off outside the squeeze
        // that tripped it. Only the freeze is gated.
        assert_eq!(mem(false, Some(0), &three()), None);
        assert_eq!(
            mem(false, Some(HEADROOM + BAND + 1), &three_frozen()),
            Some(Action::Thaw(2))
        );
        // A finished job is let go however short memory is, and in every mode.
        let mut over = three_frozen();
        over[2].job_over = true;
        for may in [true, false] {
            for a in [Some(0), None, Some(10_000)] {
                assert_eq!(mem(may, a, &over), Some(Action::Release(3)), "{may} {a:?}");
            }
        }
    }

    #[test]
    fn a_failed_memory_reading_neither_freezes_nor_thaws() {
        assert_eq!(mem(true, None, &three()), None);
        assert_eq!(mem(true, None, &three_frozen()), None);
    }

    #[test]
    fn each_loop_moves_only_its_own_freezes() {
        // Job 7 frozen for memory, job 8 for disk, job 9 paused with no recorded
        // reason (an older binary, or by hand), job 6 running.
        let claims = [
            admitted(6, "faber-6-500"),
            admitted(7, "faber-7-1000"),
            admitted(8, "faber-8-2000"),
            admitted(9, "faber-9-3000"),
        ];
        let containers = [
            ctr("r6", KIND_RUNNER, "6-500", false),
            ctr("r7", KIND_RUNNER, "7-1000", true),
            ctr("r8", KIND_RUNNER, "8-2000", true),
            ctr("r9", KIND_RUNNER, "9-3000", true),
        ];
        let jobs = live_jobs(&claims, &containers);
        let book = HashMap::from([
            ("faber-7-1000".to_string(), Reason::Memory),
            ("faber-8-2000".to_string(), Reason::Disk),
        ]);
        let ids = |v: Vec<Job>| v.iter().map(|j| j.job_id).collect::<Vec<_>>();
        assert_eq!(
            ids(movable(&jobs, &book, Reason::Disk)),
            [6, 8, 9],
            "the disk loop owns its own freeze and the unlabelled one"
        );
        assert_eq!(ids(movable(&jobs, &book, Reason::Memory)), [6, 7]);
        // With its floor off the disk loop thaws every frozen job it sees — and it
        // never sees the memory freeze.
        let disk = movable(&jobs, &book, Reason::Disk);
        let mut thawed = Vec::new();
        let mut left = disk.clone();
        while let Some(Action::Thaw(id)) = decide(DiskAdmission::Off, None, &left) {
            thawed.push(id);
            left.iter_mut().find(|j| j.job_id == id).unwrap().frozen = false;
        }
        assert_eq!(thawed, [8, 9]);
        // And memory above the thaw level thaws only job 7, never the disk freeze.
        let memory = movable(&jobs, &book, Reason::Memory);
        assert_eq!(
            mem(false, Some(HEADROOM + BAND + 1), &memory),
            Some(Action::Thaw(7))
        );
        // The disk freeze does not count against the memory cap: one memory
        // freeze so far, so the running job is next.
        assert_eq!(mem(true, Some(0), &memory), Some(Action::Freeze(6)));
    }

    #[test]
    fn the_caps_are_per_reason() {
        // Two jobs frozen for disk; memory runs short: the memory loop still has
        // its own two to freeze, from the jobs still running.
        let claims: Vec<Admitted> = (1..=5)
            .map(|id| admitted(id, &format!("faber-{id}-{}", id * 100)))
            .collect();
        let containers: Vec<JobContainer> = (1..=5)
            .map(|id| {
                ctr(
                    &format!("r{id}"),
                    KIND_RUNNER,
                    &format!("{id}-{}", id * 100),
                    id >= 4,
                )
            })
            .collect();
        let jobs = live_jobs(&claims, &containers);
        let book = HashMap::from([
            ("faber-4-400".to_string(), Reason::Disk),
            ("faber-5-500".to_string(), Reason::Disk),
        ]);
        let mut memory = movable(&jobs, &book, Reason::Memory);
        assert_eq!(
            run_mem(&mut memory, Some(0), 3),
            [Some(Action::Freeze(3)), Some(Action::Freeze(2)), None]
        );
    }

    #[test]
    fn a_restart_reads_each_freezes_reason_back_from_the_audit_log() {
        let line = |runner: &str, action: &str, reason: Option<&str>| Entry {
            kind: "freeze".to_string(),
            runner: Some(runner.to_string()),
            action: Some(action.to_string()),
            reason: reason.map(str::to_string),
            ..Entry::default()
        };
        let book = reasons_from(&[
            line("faber-1-1", "frozen", Some("memory")),
            line("faber-2-2", "frozen", Some("disk")),
            // A line from before reasons were recorded: the disk loop's.
            line("faber-3-3", "frozen", None),
            line("faber-4-4", "frozen", Some("memory")),
            line("faber-4-4", "thawed", Some("memory")),
            Entry {
                kind: "outcome".to_string(),
                ..line("faber-5-5", "frozen", Some("memory"))
            },
        ]);
        assert_eq!(book.get("faber-1-1"), Some(&Reason::Memory));
        assert_eq!(book.get("faber-2-2"), Some(&Reason::Disk));
        assert_eq!(book.get("faber-3-3"), Some(&Reason::Disk));
        assert!(!book.contains_key("faber-4-4"), "thawed since");
        assert!(!book.contains_key("faber-5-5"), "not a freeze line");
        assert_eq!(
            reason_of(&book, "faber-9-9"),
            Reason::Disk,
            "unrecorded = disk"
        );
    }

    #[test]
    fn a_memory_cause_says_what_decided_it() {
        assert_eq!(
            memory_cause(Some(700), HEADROOM, BAND, true),
            "700 MiB available in the VM, below the 768 MiB critical line (half of headroom_mib; \
             ADR-0033 §3)"
        );
        assert!(memory_cause(Some(3000), HEADROOM, BAND, false).contains("above the 2048 MiB"));
    }

    // --- the memory loop's wiring (ADR-0033 §3, §4) ----------------------------

    #[test]
    fn the_memory_loop_lists_containers_only_when_it_could_act() {
        // Nothing frozen for memory, freezing allowed, reading low: act.
        assert!(memory_acts(false, true, Some(700), HEADROOM));
        // Not low, or not allowed, or no reading: nothing to do.
        assert!(!memory_acts(false, true, Some(768), HEADROOM));
        assert!(!memory_acts(false, false, Some(0), HEADROOM));
        assert!(!memory_acts(false, true, None, HEADROOM));
        // A memory freeze to hold or let go: always, in every mode.
        assert!(memory_acts(true, false, None, HEADROOM));
        assert!(memory_acts(true, false, Some(10_000), HEADROOM));
    }

    #[test]
    fn a_memory_freeze_trips_the_fallback_and_a_thaw_does_not() {
        use crate::config::ReserveSetting;
        use crate::limiter::Limiter;
        let mut c = crate::config::AutoLimitFile::default().resolve(&Default::default());
        c.reserve = ReserveSetting::P75;
        let lim = Limiter::new(c, &HashMap::new());
        assert!(trip_after(&lim, None, 10).is_none());
        assert!(trip_after(&lim, Some(Action::Thaw(1)), 10).is_none());
        assert!(trip_after(&lim, Some(Action::Release(1)), 10).is_none());
        assert!(matches!(
            trip_after(&lim, Some(Action::Freeze(1)), 10),
            Some(crate::reserve::Event::Tripped { .. })
        ));
        assert!(!lim.reserving_p75(20), "max until the re-arm");
        // The second freeze of the same squeeze trips nothing more.
        assert!(trip_after(&lim, Some(Action::Freeze(2)), 30).is_none());
        // Under max a freeze never happens, and would trip nothing if it did.
        let max = Limiter::new(
            crate::config::AutoLimitFile::default().resolve(&Default::default()),
            &HashMap::new(),
        );
        assert!(trip_after(&max, Some(Action::Freeze(1)), 10).is_none());
    }

    #[test]
    fn a_restart_with_a_job_frozen_for_memory_resumes_the_squeeze() {
        let memory = HashMap::from([("faber-1-1".to_string(), Reason::Memory)]);
        let disk = HashMap::from([("faber-2-2".to_string(), Reason::Disk)]);
        assert!(squeeze_at_boot(&memory));
        assert!(!squeeze_at_boot(&disk));
        assert!(!squeeze_at_boot(&HashMap::new()));
    }
}
