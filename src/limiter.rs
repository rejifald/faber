//! The autolimiter — budget admission control + per-class cap learning.
//!
//! Replaces the old fixed-`concurrency` semaphore for Docker jobs. Concurrency is
//! emergent: a job is admitted only if it fits a memory budget — and, once
//! `autolimit.cpu = "auto"`, a CPU budget too (ADR-0019). See
//! `docs/autolimiter.md` for the full design; the short version:
//!
//! - **Gate 1** (feed-forward, faber-internal): `Σ reservations + next ≤ budget`,
//!   debited at admission so a batch is counted before it ramps.
//! - **Gate 2** (feedback, external-aware): `MemAvailable ≥ next + headroom`, with
//!   an anti-flap freeze band — sees dev stacks the ledger can't.
//! - A job's **reservation** is its class's Docker `--memory` **cap** (runner +
//!   dind), so the ledger bounds the cohort's aggregate footprint and an
//!   over-hungry job OOMs in isolation.
//! - Under `[autolimit] reserve = "p75"` (ADR-0033) the ledger debits a smaller
//!   number than the cap — `min(cap, p75 × margin)` — while the container's limit
//!   stays the cap. A memory freeze ([`crate::freeze`]) and a fallback to max with
//!   a doubling cool-off ([`crate::reserve`]) catch the bet when it loses. Absent or
//!   `"max"`, every debit is the cap, as before.
//! - **CPU is reserved the same way** (ADR-0019), in millicores: each class learns
//!   a runner and a dind share, the runner's share is enforced as its `--cpus`
//!   quota, and a VM CPU-pressure gate (PSI) pauses admission when the host is
//!   saturated anyway. "CPU is compressible" held for correctness but not for time:
//!   a memory budget alone admitted more heavy jobs than the VM had cores, each
//!   sized its worker pools to every core, and jobs failed on their own timeouts.
//! - The **cap a container is launched with** is sized for the whole *cohort* of
//!   queued jobs the runner could be handed, not just the one it was minted for —
//!   GitHub, not faber, decides which job a JIT runner gets. See
//!   [`Limiter::sizing_for`].
//! - Caps are **per-class** and learned: `rolling-max(recent peaks) × margin`
//!   (raise-fast via the max, lower-slow as highs age out); an OOM force-bumps; an
//!   unlearned class profiles at a bounded ceiling; a cap over a budget fraction
//!   escalates to a warning instead of auto-applying. CPU shares learn by the same
//!   rule, with a throttled run standing in for an OOM.
//! - **Free disk** gates it too, once `[host] disk_admission_floor_gib` is set
//!   (ADR-0024): the Docker volume is measured every few seconds apart from any
//!   Docker call, and a job is admitted only if free space stays at or above the
//!   floor once two charges are taken off — those of the jobs admitted in the last
//!   ten minutes, and its own (ADR-0032 §1). One burst of admissions reads one
//!   figure, while a job's disk arrives over its first minutes. A job whose
//!   profile grants dind is charged 10 GiB, any other 3 GiB.
//! - **The machine** has a share too, once `[host] keep_memory_gib` / `keep_cpus`
//!   are stated (ADR-0021): Gate 1 and the CPU budget plan within what the share
//!   leaves CI of the machine, and admission pauses — macOS VM jobs included —
//!   while the machine reports memory pressure or its swap is growing
//!   (ADR-0032 §3). See [`crate::machine`].
//! - With that share declared, Gate 1's reserve is **measured** too (ADR-0021
//!   §3): the larger of `reserve_mib` and the 24-hour peak of what the VM uses
//!   beyond faber's own containers, sampled once a minute. See
//!   [`crate::cotenant`].
//!
//! A single serialized arbiter owns the ledger, so admission never races. It's
//! woken by job completion (`AdmissionGuard::drop`) and by a periodic memory-tick.
//!
//! Ahead of every gate sits the operator's **hold** (issue #66): a named, dated
//! stop on new admissions that survives restarts. Right behind it sits **pause**
//! (ADR-0023): transient and global, the operator's or the janitor's, and while it
//! lasts nothing new starts at all — no Docker admission, and no VM admission either.
//! And a refusal is never silent — each one is a [`Wait`] naming the gate that
//! said no, which a parked claim carries to the dashboard, and which a per-gate
//! tally counts for `/api/state` (issue #85, [`crate::parking`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::audit::now_ms;
use crate::config::{AutoLimitConfig, CpuConfig, DiskAdmission, HardCeiling, Hold};
use crate::cotenant::{self, CoTenantPeak, CoTenantView};
use crate::docker::{self, CpuUsage, Limits, ResourceLimits, RunReport};
use crate::host::{self, DiskReading, ShortOfRoom};
use crate::machine::{
    MachineHold, MachineReading, MachineShare, MachineSize, MachineState, MachineView,
    PressureLevel,
};
use crate::parking::{GateCount, GateTally};
use crate::reserve::{self, Fallback, ReserveMode, ReserveView, TripCause};
use crate::vm::admit::{
    self, docker_wall_refusal, MacosInput, SessionMode, VmRefusal, VmSession, WallGate,
    MACOS_VM_DISK_CHARGE_MIB,
};
use crate::vm::proto::{StopReason, VmKind, VmRecord, VmState};
use crate::vm::verify::{Standing, Summary};
use crate::wall::report::Judgement;

/// A per-class key: `repo::workflow_id::job_name`. Stable across runs (matrix
/// cells stay distinct via `job_name`), immutable across renames (`workflow_id`).
pub fn class_key(repo: &str, workflow_id: u64, job_name: &str) -> String {
    format!("{repo}::{workflow_id}::{job_name}")
}

/// Millicores as a short core count for log lines and warnings: `4000` → `4`,
/// `1250` → `1.25`.
pub fn fmt_cores(mcpu: u64) -> String {
    let s = format!("{:.2}", mcpu as f64 / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `available_mib` sentinel for "not probed yet / probe failing" — Gate 2 then
/// fails open (Gate 1 still bounds faber's own footprint).
const AVAILABLE_UNKNOWN: u64 = u64::MAX;
/// The [`crate::logline::Repeats`] key of the VM-larger-than-the-share warning.
const ENCROACHMENT_KEY: &str = "machine:encroachment";
/// The [`crate::logline::Repeats`] key of the failing VM-pressure probe.
const PRESSURE_PROBE_KEY: &str = "autolimiter:pressure-probe";
/// Consecutive failed availability probes before faber calls Docker *unreachable*
/// and stops admitting (Gate 0).
///
/// The probe runs a throwaway container, so a single failure is ambiguous — a
/// missing image or a momentary blip reads the same as a dead daemon. Requiring a
/// run of them keeps a transient miss from stalling CI, while a real outage still
/// closes the gate in `UNHEALTHY_AFTER × MEM_TICK` (~24s). One success clears it.
const UNHEALTHY_AFTER: u32 = 3;
/// Floor on any single container cap, so a trivially-small learned value can't
/// OOM the job on ordinary startup overhead.
const MIN_CAP_MIB: u64 = 256;
/// Multiplier applied to a container's cap when it OOMs (a censored sample).
const OOM_BUMP: f64 = 1.5;
/// How many of a class's most recent runs an OOM must be among for its hold at
/// the escalation threshold to be put to the owner (ADR-0053): an OOM still in
/// the window but older than this is history, not a job failing now.
const RECENT_OOM_RUNS: usize = 5;
/// Floor on a runner's CPU share (millicores). The share is also the runner's
/// `--cpus` quota, and below one core a job's own toolchain start-up is throttled
/// into the very timeouts ADR-0019 exists to prevent.
const MIN_RUNNER_MCPU: u64 = 1000;
/// Floor on dind's CPU share (millicores). Reserved only — never a quota.
const MIN_DIND_MCPU: u64 = 100;
/// dind's CPU share while its class has no CPU sample yet.
const PROFILING_DIND_MCPU: u64 = 1000;
/// A container that ran under a quota and was throttled in at least this share of
/// its CFS periods hit that quota: its measured peak is only "at least this much",
/// the CPU analogue of an OOM kill.
const THROTTLE_CENSOR: f64 = 0.10;
/// Multiplier applied to the quota a throttled container ran under (a censored
/// sample), as [`OOM_BUMP`] is to a memory cap.
const CPU_BUMP: f64 = 1.5;
/// `cpu_pressure` sentinel for "no PSI reading" — CPU Gate 2 then fails open.
const PRESSURE_UNKNOWN: u64 = u64::MAX;
/// How often the memory-tick re-probes availability and re-checks waiters.
const MEM_TICK: Duration = Duration::from_secs(8);
/// How often (in ticks) to also re-probe the VM total and core count (tracks a
/// Docker resize).
const TOTAL_EVERY: u32 = 8;
/// Bounded wait between admission retries — a backstop so a missed wake only
/// costs a short delay, never a hang.
const ADMIT_RETRY: Duration = Duration::from_secs(1);
/// Warnings kept for the panel: one per class and kind, the newest last.
const WARN_CAP: usize = 20;
/// How often the disk loop re-reads free space on the Docker volume (ADR-0024
/// §1). The gate reads the stored figure, so this bounds how late it sees the
/// disk change.
const DISK_TICK: Duration = Duration::from_secs(10);
/// How much the co-tenant reserve in force must move before it is logged again
/// (ADR-0021 §3). The peak rises in small steps while a co-tenant ramps up.
const RESERVE_LOG_STEP_MIB: u64 = 512;

/// Cadence of the machine reading (ADR-0021 §1). Local `sysctl` or `/proc`
/// reads, independent of Docker, and frequent enough that 5 minutes of swap
/// growth is thirty readings.
const MACHINE_TICK: Duration = Duration::from_secs(10);
/// Cadence of the co-tenant sample (ADR-0021 §3). Each takes two `docker stats`
/// passes and a probe container, so it runs on a loop of its own rather than
/// stretching the memory-tick Gate 2 reads, and a minute is plenty for a 24-hour
/// peak of stacks that stay up for hours.
const COTENANT_TICK: Duration = Duration::from_secs(60);
/// How long a Docker admission keeps counting against free disk (ADR-0024 §1).
///
/// A job writes most of its disk in its first minutes — a dependency tarball
/// restored from its cache volume, then the workspace — and a free-space reading
/// taken as a batch is admitted sees none of it. On the incident host a batch of
/// admissions took about 2 GiB each within the first few minutes: the window
/// sits above that on purpose.
const DISK_RAMP_WINDOW_MS: u64 = 10 * 60 * 1000;
/// What a Docker job whose profile grants no daemon is charged against free disk,
/// from its own admission until [`DISK_RAMP_WINDOW_MS`] after it (ADR-0032 §1):
/// above the ≈2 GiB per job ADR-0024 measured, on purpose. A constant until faber
/// measures per-job disk, when a learned per-class figure can replace it the way
/// memory's is learned.
pub const DISK_CHARGE_MIB: u64 = 3 * 1024;
/// The same charge for a job whose profile grants dind (ADR-0013) — `Unstated`
/// included, since that grants too (ADR-0032 §1). Its daemon pulls and builds
/// images into a volume of its own: the job that filled a builder's disk in
/// October 2026 wrote gigabytes within minutes of starting.
pub const DISK_CHARGE_DIND_MIB: u64 = 10 * 1024;
/// Bound on the admissions remembered for their ramp charges, far past any
/// admission rate a host sustains for ten minutes, so the record cannot grow
/// without limit whatever the clock does.
const DISK_RAMP_CAP: usize = 256;
/// Ticket for a caller that isn't queued as a waiter, so the fairness set-aside
/// treats it as "never the head". Only reachable from tests, which drive
/// [`Limiter::try_admit`] directly rather than through [`Limiter::admit`].
#[cfg(test)]
const NO_TICKET: u64 = u64::MAX;

/// What admitting one job debits from the ledger: memory in MiB and CPU in
/// millicores (1000 = one core), both held for the job's life — and the disk it
/// is charged against free space while it ramps (ADR-0032 §1), which is held
/// only while the job runs, and ages out of [`DISK_RAMP_WINDOW_MS`].
///
/// The CPU half is debited whatever `autolimit.cpu` says, so the dashboard shows
/// what a host is committing before anyone turns the CPU gates on; only the gates
/// themselves wait for `cpu = "auto"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reservation {
    pub mib: u64,
    pub mcpu: u64,
    /// The disk gate's charge for this job, in MiB: [`DISK_CHARGE_DIND_MIB`] when
    /// its profile grants dind, [`DISK_CHARGE_MIB`] otherwise. Zero for an
    /// adoption, which is not an admission and adds nothing to the ramp.
    pub disk_mib: u64,
    /// What `mib` would have been under ADR-0033's p75 reservation: measured, never
    /// debited. The ledger reads `mib` only; this rides along so the dashboard can
    /// total what the running jobs would hold under `"p75"` (rollout step 1).
    /// Equal to `mib` wherever no p75 figure exists (an adoption, a profiling
    /// class).
    pub shadow_mib: u64,
}

/// A memory-only reservation — the shape every admission had before ADR-0019, so
/// the ledger tests written against it still read as what they test. Its disk
/// charge is a light job's (ADR-0032 §1), which is what the disk gate's tests
/// built on it were written for.
#[cfg(test)]
impl From<u64> for Reservation {
    fn from(mib: u64) -> Self {
        Self {
            mib,
            mcpu: 0,
            disk_mib: DISK_CHARGE_MIB,
            shadow_mib: mib,
        }
    }
}

/// The learning + enforcement state for one job class.
#[derive(Debug, Clone)]
pub struct ClassCap {
    /// Human label for the panel: `repo · workflow · job`.
    pub label: String,
    pub runner_mib: u64,
    pub dind_mib: u64,
    pub status: CapStatus,
    pub pinned: bool,
    /// CPU shares in millicores (ADR-0019): what admission reserves for the runner
    /// and dind containers. The runner's is also its `--cpus` quota; dind's is
    /// reserved only.
    pub runner_mcpu: u64,
    pub dind_mcpu: u64,
    /// Where the CPU shares came from. Independent of `status`: a class can have a
    /// learned memory cap and still be profiling CPU, or the reverse.
    pub cpu_status: CapStatus,
    /// Operator-pinned CPU shares the learner won't touch. Independent of
    /// `pinned`, because every pin written before ADR-0019 pins memory alone.
    pub cpu_pinned: bool,
    peaks_runner: VecDeque<u64>,
    peaks_dind: VecDeque<u64>,
    cpu_peaks_runner: VecDeque<u64>,
    cpu_peaks_dind: VecDeque<u64>,
    /// Parallel to `peaks_runner` / `peaks_dind`: whether each sample was censored
    /// (an OOM stood in for the peak). ADR-0033 §1: a window holding one is not a
    /// distribution yet.
    censored_runner: VecDeque<bool>,
    censored_dind: VecDeque<bool>,
    /// The would-be p75 reservation of each container (ADR-0033 §1), in MiB:
    /// `min(cap, p75(window) × margin)`, equal to the cap while the window is not
    /// full, holds a censored sample, or the class is pinned. Measured only —
    /// admission debits the cap.
    shadow_runner_mib: u64,
    shadow_dind_mib: u64,
    /// Finished runs judged against the p75 reservation (those where it was below
    /// the cap for at least one container), and how many of them peaked above it.
    /// Rebuilt by the startup replay, so they need no storage of their own.
    p75_runs: u64,
    p75_trips: u64,
    /// ADR-0053: the class was OOM-killed and wants more than the escalation
    /// threshold, so an OOMing container is held there. Set by [`apply_want`].
    oom_held: bool,
    /// While `oom_held`: the caps a pin needs to stop the OOMs — each proven
    /// container at its want, the other at its cap. Set by [`apply_want`].
    held_pin: (u64, u64),
}

impl ClassCap {
    /// A class nothing has been learned about yet: every cap and share at its
    /// profiling default.
    fn profiling(label: &str, defaults: Shares) -> Self {
        Self {
            label: label.to_string(),
            runner_mib: defaults.runner_mib,
            dind_mib: defaults.dind_mib,
            status: CapStatus::Profiling,
            pinned: false,
            runner_mcpu: defaults.runner_mcpu,
            dind_mcpu: defaults.dind_mcpu,
            cpu_status: CapStatus::Profiling,
            cpu_pinned: false,
            peaks_runner: VecDeque::new(),
            peaks_dind: VecDeque::new(),
            cpu_peaks_runner: VecDeque::new(),
            cpu_peaks_dind: VecDeque::new(),
            censored_runner: VecDeque::new(),
            censored_dind: VecDeque::new(),
            shadow_runner_mib: defaults.runner_mib,
            shadow_dind_mib: defaults.dind_mib,
            p75_runs: 0,
            p75_trips: 0,
            oom_held: false,
            held_pin: (0, 0),
        }
    }
}

/// One class's caps and CPU shares — stored, or the profiling defaults a class
/// that has never run would launch at.
#[derive(Debug, Clone, Copy)]
struct Shares {
    runner_mib: u64,
    dind_mib: u64,
    /// The would-be p75 reservations (ADR-0033), never debited.
    shadow_runner_mib: u64,
    shadow_dind_mib: u64,
    runner_mcpu: u64,
    dind_mcpu: u64,
}

impl Shares {
    /// The profiling defaults: the memory ceiling for both containers, the CPU
    /// ceiling for the runner, [`PROFILING_DIND_MCPU`] for dind.
    fn profiling(cfg: &AutoLimitConfig) -> Self {
        Self {
            runner_mib: cfg.profiling_cap_mib,
            dind_mib: cfg.profiling_cap_mib,
            shadow_runner_mib: cfg.profiling_cap_mib,
            shadow_dind_mib: cfg.profiling_cap_mib,
            runner_mcpu: cpu_ceiling(&cfg.cpu),
            dind_mcpu: PROFILING_DIND_MCPU,
        }
    }

    /// `key`'s stored caps, or `defaults` for a class never seen.
    fn of(classes: &HashMap<String, ClassCap>, key: &str, defaults: Shares) -> Self {
        match classes.get(key) {
            Some(c) => Self {
                runner_mib: c.runner_mib,
                dind_mib: c.dind_mib,
                shadow_runner_mib: c.shadow_runner_mib,
                shadow_dind_mib: c.shadow_dind_mib,
                runner_mcpu: c.runner_mcpu,
                dind_mcpu: c.dind_mcpu,
            },
            None => defaults,
        }
    }

    /// What admitting a job of this class debits. A class served by a profile that
    /// was granted no daemon pays for one container, in both units (issue #43),
    /// and is charged a light job's disk (ADR-0032 §1).
    fn reservation(self, dind: bool) -> Reservation {
        Reservation {
            mib: self.runner_mib + if dind { self.dind_mib } else { 0 },
            mcpu: self.runner_mcpu + if dind { self.dind_mcpu } else { 0 },
            disk_mib: disk_charge_mib(dind),
            shadow_mib: self.shadow_runner_mib + if dind { self.shadow_dind_mib } else { 0 },
        }
    }
}

/// What a job is charged against free disk while it ramps (ADR-0032 §1): more
/// when its profile grants a daemon, which pulls and builds images into a volume
/// of its own. The grant is the profile's, so every class in one sizing cohort is
/// charged alike, whichever of them GitHub hands the runner.
fn disk_charge_mib(dind: bool) -> u64 {
    if dind {
        DISK_CHARGE_DIND_MIB
    } else {
        DISK_CHARGE_MIB
    }
}

/// How one about-to-be-minted runner should be sized: what its two containers are
/// capped at, and what the ledger debits for it. The two are deliberately
/// different — see [`Limiter::sizing_for`].
#[derive(Debug, Clone, PartialEq)]
pub struct Sizing {
    /// Docker `--memory` for the runner and dind containers, and the runner's
    /// `--cpus` quota: the worst case over every class this runner could be handed.
    pub limits: Limits,
    /// What admission debits — the minted-for class's own reservation.
    pub reservation: Reservation,
    /// The minted-for class has never run: this is its profiling run.
    pub profiling: bool,
    /// Set when a *peer* class in the cohort, not the minted-for one, set the
    /// ceiling. Carries that class's label for the provisioning log line.
    pub raised_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CapStatus {
    /// No samples yet — runs at the bounded profiling ceiling.
    Profiling,
    /// Cap derived from observed peaks.
    Learned,
    /// Learned cap exceeded the escalation threshold; held at the ceiling pending
    /// operator confirmation.
    Warned,
    /// Operator-pinned; the learner won't touch it.
    Pinned,
}

/// Which learner a panel warning came from. A class can be held on memory and
/// on CPU at once, and each clears on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarnKind {
    Memory,
    Cpu,
}

/// A panel warning's identity: the class key and the learner that held it.
type WarnKey = (String, WarnKind);

/// A panel warning as the snapshot serves it: `key` is `memory:<class>` or
/// `cpu:<class>`, stable across restarts; `text` is the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WarningItem {
    pub key: String,
    pub text: String,
}

/// The [`crate::logline::Repeats`] key of a class's hold warning of `kind`.
fn hold_key(class: &str, kind: WarnKind) -> String {
    format!("class-hold:{kind:?}:{class}")
}

#[derive(Default)]
struct Inner {
    /// Sum of reservations of admitted-and-running Docker jobs.
    reserved_mib: u64,
    /// What the same jobs would hold under ADR-0033's p75 reservation. Measured
    /// beside the ledger, read by nothing that admits.
    shadow_reserved_mib: u64,
    /// The CPU half of the same sum, in millicores. Tracked in every CPU mode.
    reserved_mcpu: u64,
    /// Count of admitted-and-running Docker jobs (for the hard ceiling + readout).
    docker_inflight: usize,
    /// Gate 2 freeze hysteresis: once available < headroom, stay frozen until it
    /// climbs back above headroom + band.
    frozen: bool,
    /// CPU Gate 2 freeze hysteresis: set once VM CPU pressure reaches
    /// `pause_pressure`, cleared once it falls to `resume_pressure`.
    cpu_frozen: bool,
    /// Parked admission requests: ticket → reservation. Tickets are monotonic, so
    /// the first entry is the longest-waiting request — the fairness "head" whose
    /// memory and CPU younger waiters may not occupy. Entries are removed on
    /// admission and on cancel (the `WaiterTicket` guard), so this is the live
    /// parked set.
    waiters: BTreeMap<u64, Reservation>,
    /// Tickets of parked claims from a priority repo (ADR-0036), a subset of
    /// `waiters`. The head is the oldest of these when there is one, else the
    /// oldest waiter: empty means today's strict first-come-first-served.
    priority_tickets: BTreeSet<u64>,
    /// Tickets of parked claims demoted as likely wasted (ADR-0063 §5), a subset
    /// of `waiters` disjoint from `priority_tickets`. They rank below every other
    /// claim, in their own ticket order.
    waste_tickets: BTreeSet<u64>,
    /// Next ticket to hand out.
    next_ticket: u64,
    /// The disk gate's inputs (ADR-0024 §1). Under this lock, beside the ledger,
    /// so an admission and the ramp entry it adds are one step: two claims can
    /// never both pass the gate on room only one of them will get.
    disk: DiskLedger,
    /// What VMs add to admission (ADR-0026 slice 8b). Under this lock, beside the
    /// disk ledger, so a VM job's count and its disk charge are one step.
    vm: VmLedger,
}

/// The VM half of admission (ADR-0026): the wall's verdict, whether Docker is
/// behind it, and what macOS VM jobs and the Linux VM hold.
#[derive(Default)]
struct VmLedger {
    /// The wall's latest judgement. Starts [`WallGate::Unread`], which refuses.
    wall: WallGate,
    /// When it was judged (epoch ms): an `Open` older than
    /// [`admit::WALL_MAX_AGE_MS`] reads as red, so a dead poller fails closed.
    wall_at_ms: u64,
    /// The macOS VM session's supervisor (ADR-0051). [`VmSession::NotRequired`]
    /// until [`Limiter::require_vm_session`]: a host whose record names no
    /// session account keeps admitting as before.
    session: VmSession,
    /// The session account's reboot mode, from the record, for the words of
    /// a refusal.
    session_mode: SessionMode,
    /// Docker runs in the Linux VM faber owns, so Docker admission needs the wall
    /// too. Off by default: Docker Desktop's jobs are not behind it, and a host
    /// that never installed the wall must not lose Docker admission to a release
    /// that added it (AGENTS.md).
    wall_required: bool,
    /// macOS VM jobs holding a slot ([`VmRun`]).
    macos_running: usize,
    /// The long-lived Linux VM's memory, MiB, reserved inside the machine share.
    linux_vm_mib: u64,
    /// Whether a `macos-vm` profile is configured, so a macOS VM can start here
    /// at all. Without one the share holds nothing back for macOS VMs.
    runs_macos_vms: bool,
    /// The per-host VM layout (ADR-0051): the macOS class's cap and what each of
    /// its VMs takes of the share. Defaults to Apple's two at 8 GiB each.
    layout: crate::config::VmLayout,
    /// While a switched-off Linux VM waits for the docker jobs left in it
    /// (ADR-0051, [`crate::vm::engine::keep_off`]): the `/api/state` notice.
    linux_vm_waiting: Option<String>,
    /// The supervisor's latest list of VMs, for `/api/state` only: nothing here
    /// gates admission.
    vms: Vec<VmRow>,
    /// When each kind's supervisor list last answered (epoch ms), for
    /// `vm.listed_age_s`. A kind is absent until its supervisor has answered.
    listed_at_ms: HashMap<VmKind, u64>,
    /// The Linux VM's size `(vCPUs, MiB)` from `[linux_vm]`, set at boot where the
    /// engine runs. Its row carries it: the one Linux VM is always started at the
    /// engine's size (popover/vms/anatomy.md O1).
    linux_vm_size: Option<(u8, u32)>,
    /// Whether each VM image matches the pinned one, for `/api/state` only.
    images: ImagesView,
    /// Whether macOS jobs need a verified golden image (ADR-0049 §7): set at boot
    /// on a host with a `macos-vm` profile. Off by default, so a limiter told
    /// nothing of images gates none.
    golden_required: bool,
    /// How each golden image stands, by name. An image absent here is
    /// [`Standing::Unknown`], which refuses once `golden_required`.
    golden: BTreeMap<String, Standing>,
    /// Each verified image's identity and where it lives: admission re-checks it
    /// every time ([`crate::vm::verify::Identity`]), so an image changed since its
    /// pass is refused at once.
    golden_pins: BTreeMap<String, (std::path::PathBuf, crate::vm::verify::Identity)>,
    /// What the runner-image loader last found wrong (ADR-0026, "The runner
    /// image moves first"), for `/api/state` only; `None` while nothing is.
    runner_image: Option<String>,
}

/// A macOS VM job counted against the class cap, the share and the disk floor;
/// released on drop, which wakes the claims waiting on any of them.
pub struct VmRun {
    limiter: Arc<Limiter>,
    /// This admission's entry in the disk ledger. `None` for an adopted VM, which
    /// is not an admission and adds nothing to the ramp (ADR-0024 §1).
    ramp: Option<u64>,
}

impl VmRun {
    /// The macOS VM supervisor this admission was let through to did not answer
    /// when its VM was to start, `why` (#393). Where the "no VM session" gate is
    /// in force (ADR-0051) it closes at once, so the claims behind this one wait
    /// for the next probe's handshake rather than each meeting the same closed
    /// socket, and this answers `true`. `false`, and nothing changed, where
    /// nothing is gated: there the gap is no session's, and stays a fault.
    pub fn session_lost(&self, why: String) -> bool {
        let gated = self.limiter.inner.lock().unwrap().vm.session != VmSession::NotRequired;
        if gated {
            self.limiter.observe_vm_session(Err(why));
        }
        gated
    }
}

impl Drop for VmRun {
    fn drop(&mut self) {
        {
            let mut inner = self.limiter.inner.lock().unwrap();
            inner.vm.macos_running = inner.vm.macos_running.saturating_sub(1);
            if let Some(ramp) = self.ramp {
                inner.disk.release(ramp);
            }
        }
        self.limiter.wake.notify_waiters();
    }
}

/// What the disk gate reads (ADR-0024 §1): the operator's floor, the latest
/// measurement of the Docker volume, and the recent admissions still counted as
/// writing their first gigabytes.
#[derive(Default)]
struct DiskLedger {
    admission: DiskAdmission,
    /// The latest *successful* reading of the host volume. A failed one replaces
    /// nothing, so this ages out rather than vanishing (see
    /// [`Limiter::observe_disk_free`]). On a macOS SYSTEM host it is the Mac's
    /// volume holding the VM disks, and on its own it gates macOS VM jobs and
    /// revival.
    reading: Option<DiskReading>,
    /// The latest successful reading of the Linux VM's disk, where Docker's data
    /// root is, on a host that has one (#410). Ages out as `reading` does.
    vm: Option<DiskReading>,
    /// Why the last attempt to read the Linux VM's disk failed, while it did.
    /// Surfaced, and never acted on: the gate falls back to `reading`.
    vm_unreadable: Option<String>,
    /// Each recent Docker admission, oldest first: when it was made and what it
    /// was charged. Each keeps its own charge, so a window holding dind and light
    /// jobs at once sums what each was charged (ADR-0032 §1). Recorded in every
    /// mode, so the dashboard shows what a floor would hold before anyone sets
    /// one. An adoption is not an admission and is not in it: its container ran
    /// before the restart, and how far into its first minutes it is, nothing here
    /// knows.
    ramp: VecDeque<Ramping>,
    /// The id the next admission's ramp entry gets, so the guard that admission
    /// returns can give its own entry back (#89).
    next_ramp: u64,
    /// Whether the last reading found the gate closed to every job, so a crossing
    /// is logged once rather than every tick.
    closed: bool,
}

/// One admission still counted against free disk (ADR-0032 §1).
#[derive(Debug, Clone, Copy)]
struct Ramping {
    /// Which admission this is, for [`DiskLedger::release`].
    id: u64,
    /// When it was admitted (epoch ms).
    at_ms: u64,
    /// What it was charged (MiB): [`DISK_CHARGE_DIND_MIB`] or [`DISK_CHARGE_MIB`].
    charge_mib: u64,
    /// The fresh reading of the volume it writes to, as it was admitted: what
    /// [`DiskLedger::held_against`] measures its writes from. None when there was
    /// no fresh reading, and then the whole charge stands for its window.
    baseline: Option<DiskReading>,
}

impl DiskLedger {
    /// The reading a Docker job is gated on: the tighter of the host volume and
    /// the Linux VM's disk while both are evidence (#410). Docker's data root is
    /// in the VM, and the VM's disk file is on the host volume, so a job can run
    /// either out.
    fn docker_reading(&self, now_ms: u64) -> Option<DiskReading> {
        host::tighter(self.reading, self.vm, now_ms)
    }

    /// The admissions still inside [`DISK_RAMP_WINDOW_MS`] at `now_ms`, against
    /// the reading a Docker job is gated on: how many, and what they have yet to
    /// write between them.
    fn held(&self, now_ms: u64) -> (usize, u64) {
        self.held_against(now_ms, self.docker_reading(now_ms))
    }

    /// The admissions still inside [`DISK_RAMP_WINDOW_MS`] at `now_ms`: how many,
    /// and what they have yet to write against `reading` — their charges, less
    /// what the volume has lost since they were admitted.
    ///
    /// A charge stands for disk a job has not written *yet*. Once the job writes
    /// it, `reading` already shows it gone, and holding the whole charge on top
    /// counts the same gigabytes twice: on a small Linux VM disk that held three
    /// or four jobs' worth of room for ten minutes and admitted in batches. So the
    /// drop in free space since the oldest admission measured on this volume is
    /// taken off the charges of it and every later one on the volume, down to
    /// zero. That is the gate judging free space as it was before the window
    /// started writing, less every charge in it, or as it is now if that is less.
    /// What else wrote in the window (an older job, an image pull) is credited to
    /// the window too; it was never charged, and once the window ends the reading
    /// counts it in full.
    fn held_against(&self, now_ms: u64, reading: Option<DiskReading>) -> (usize, u64) {
        let live: Vec<&Ramping> = self
            .ramp
            .iter()
            .filter(|r| now_ms.saturating_sub(r.at_ms) < DISK_RAMP_WINDOW_MS)
            .collect();
        let charged = live
            .iter()
            .fold(0u64, |mib, r| mib.saturating_add(r.charge_mib));
        let written = reading
            .filter(|now| now.fresh_free_mib(now_ms).is_some())
            .and_then(|now| {
                let on_volume = |r: &&&Ramping| r.baseline.is_some_and(|b| b.volume == now.volume);
                let first = live.iter().find(on_volume)?.baseline?;
                let creditable = live
                    .iter()
                    .filter(on_volume)
                    .fold(0u64, |mib, r| mib.saturating_add(r.charge_mib));
                Some(first.free_mib.saturating_sub(now.free_mib).min(creditable))
            })
            .unwrap_or(0);
        (live.len(), charged - written)
    }

    /// The gate's refusal at `now_ms` of a job charged `charge_mib`, if it
    /// refuses: the floor is on, the reading is fresh, and free space less what
    /// the window holds *and* less the job's own charge is below the floor
    /// (ADR-0032 §1). Before the job's own charge was taken, a job admitted alone
    /// was charged nothing, and a dind job got in with free space barely over the
    /// floor.
    fn refusal(&self, now_ms: u64, charge_mib: u64) -> Option<Wait> {
        self.admission.floor_gib()?;
        let (ramping, held_mib) = self.held(now_ms);
        host::short_of_room(
            self.admission,
            self.docker_reading(now_ms),
            held_mib.saturating_add(charge_mib),
            now_ms,
        )
        .map(|short| Wait::Disk {
            free_mib: short.free_mib,
            volume: short.volume,
            ramping,
            held_mib,
            charge_mib,
            floor_gib: short.floor_gib,
        })
    }

    /// Count one admission made at `now_ms` and charged `charge_mib`, against
    /// `baseline`, the reading of the volume it writes to, forgetting those that
    /// have had the whole window to write. The id names its entry for
    /// [`Self::release`].
    fn admitted(&mut self, now_ms: u64, charge_mib: u64, baseline: Option<DiskReading>) -> u64 {
        self.ramp
            .retain(|r| now_ms.saturating_sub(r.at_ms) < DISK_RAMP_WINDOW_MS);
        let id = self.next_ramp;
        self.next_ramp += 1;
        self.ramp.push_back(Ramping {
            id,
            at_ms: now_ms,
            charge_mib,
            baseline: baseline.filter(|r| r.fresh_free_mib(now_ms).is_some()),
        });
        while self.ramp.len() > DISK_RAMP_CAP {
            self.ramp.pop_front();
        }
        id
    }

    /// Give back the charge of admission `id`: its job is over, or never started
    /// a container, so it will write nothing more (#89, #161). A no-op once the
    /// entry has aged out or was already given back.
    fn release(&mut self, id: u64) {
        self.ramp.retain(|r| r.id != id);
    }

    /// The gate as the dashboard reads it. `parked` is every parked claim's own
    /// charge, longest-waiting first: the first of them the gate refuses is the
    /// charge it is holding a claim back for.
    fn view(&self, now_ms: u64, parked: impl IntoIterator<Item = u64>) -> DiskView {
        let (ramping, held_mib) = self.held(now_ms);
        let reading = self.docker_reading(now_ms);
        DiskView {
            mode: self.admission.as_str(),
            floor_gib: self.admission.floor_gib(),
            free_mib: reading.map(|r| r.free_mib),
            volume: reading.map(|r| r.volume),
            reading_age_s: reading.map(|r| r.age_ms(now_ms) / 1000),
            fresh: reading.is_some_and(|r| r.fresh_free_mib(now_ms).is_some()),
            host_free_mib: self.reading.map(|r| r.free_mib),
            vm_free_mib: self.vm.map(|r| r.free_mib),
            vm_reading_age_s: self.vm.map(|r| r.age_ms(now_ms) / 1000),
            vm_unreadable: self.vm_unreadable.clone(),
            ramping,
            held_mib,
            charge_mib: parked
                .into_iter()
                .find(|&charge| self.refusal(now_ms, charge).is_some()),
            closed: self.refusal(now_ms, DISK_CHARGE_MIB).is_some(),
            vm_grow: crate::vm::grow::global().view(),
        }
    }
}

/// Who paused faber (ADR-0023). Either way nothing new starts until it resumes —
/// no Docker admission, no VM admission — and running jobs finish. The two differ
/// only in who may lift the pause and in what a parked claim says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// The operator: `POST /api/pause`, or Pause in the dashboard or the tray.
    Operator,
    /// The janitor, for the length of a bulk reclamation (ADR-0007): reclaiming
    /// disk, or evicting cold runner-image variants.
    Reclaim,
}

/// One memory-tick reading of the VM's `MemAvailable`, as the memory freeze
/// (ADR-0033 §3) decides on it: `None` when the probe failed. `seq` counts
/// readings, so a watcher that missed some still knows it is looking at a new one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemReading {
    pub seq: u64,
    pub available_mib: Option<u64>,
    pub at_ms: u64,
}

/// The ADR-0033 §5 learner rule under `"p75"`: the report as the learner should
/// see it, or `None` to keep the run out of the learner altogether.
///
/// Under `"max"` the report is learned exactly as it always was. Under `"p75"` a
/// container whose latched hierarchical `memory.events` shows a host OOM
/// (`oom_kill` rose, `oom` did not) is not a class sample: its OOM flag is dropped
/// and its truncated peak with it. A raised `oom` — the runner's own, or dind's
/// local one — stays a class sample, censored and bumped as today. A run whose only
/// OOM signal was a host OOM is kept out entirely, like an idle reclaim or an infra
/// failure. A runner with no `memory.events` reading at all is never called a host
/// OOM, so an `OOMKilled` there stays a class OOM, which errs safe.
pub fn learnable_under(mode: ReserveMode, report: &RunReport) -> Option<RunReport> {
    if mode != ReserveMode::P75 {
        return Some(report.clone());
    }
    let runner_host = report.host_oom_runner();
    // dind's flag is its *local* `oom`: raised, it is a breach of dind's own cap.
    let dind_host = report.host_oom_dind() && !report.oom_dind;
    if !runner_host && !dind_host {
        return Some(report.clone());
    }
    let mut learned = report.clone();
    if runner_host {
        learned.oom_runner = false;
        learned.runner_peak_mib = None;
    }
    if dind_host {
        learned.dind_peak_mib = None;
    }
    (learned.oom_runner || learned.oom_dind).then_some(learned)
}

/// What the ledger debits for `res` (ADR-0033 §1): its cap, or its p75 shadow
/// while `"p75"` is in force. A reservation with no p75 figure (`shadow_mib` 0)
/// debits its cap.
fn debit_mib(res: &Reservation, p75: bool) -> u64 {
    if p75 && res.shadow_mib > 0 {
        res.shadow_mib.min(res.mib)
    } else {
        res.mib
    }
}

/// The autolimiter. Held behind an `Arc` (the guard credits back on drop).
pub struct Limiter {
    cfg: RwLock<AutoLimitConfig>,
    /// `MemTotal − reserve`, in MiB. Recomputed when the reserve changes or the
    /// VM total is re-probed.
    budget_mib: AtomicU64,
    /// Last probed VM total (MiB), so a reserve change can recompute the budget.
    mem_total_mib: AtomicU64,
    /// The VM's core count (`docker info`), `0` until probed. Unknown skips CPU
    /// Gate 1 and applies no `--cpus` quota: memory still bounds admission.
    ncpu: AtomicU64,
    /// `ncpu × 1000 × overcommit − reserve_cpus × 1000` millicores, saturating at
    /// zero. Recomputed alongside `budget_mib`.
    cpu_budget_mcpu: AtomicU64,
    /// Last VM CPU PSI `some avg60` in hundredths of a percent, or
    /// [`PRESSURE_UNKNOWN`].
    cpu_pressure: AtomicU64,
    /// ADR-0011 §5: while set, Gate 1 refuses everything.
    ///
    /// Raised at startup on a host with a Docker profile and lowered only once
    /// adoption has enumerated what is already running. Admitting before that
    /// would spend a budget whose ledger still reads zero while real containers
    /// hold real memory — the same over-admission ADR-0007 exists to stop, just
    /// arriving through ignorance rather than through a dead daemon.
    ///
    /// Default *false*: a `Limiter` is not born waiting for anything, so nothing
    /// that constructs one without an adoption pass (every test, a host with no
    /// Docker profile) is affected. `main` raises it deliberately.
    adoption_pending: AtomicBool,
    /// The operator's hold (issue #66): while set, no new Docker job is admitted,
    /// whatever the gates below would say. Adoption ignores it — a running
    /// container is not an admission.
    hold: Mutex<Option<Hold>>,
    /// A grow of the Linux VM's disk is restarting it (ADR-0061 §3): no new
    /// Docker job is admitted, and a parked claim says why. macOS VM jobs are not
    /// held, and the operator's hold stays the operator's.
    grow_hold: AtomicBool,
    /// Who has paused faber, if anyone (ADR-0023). While set nothing is admitted,
    /// Docker or VM. It is the only pause flag: the orchestrator's pause is this
    /// one, so claiming and admission can never disagree about whether faber is
    /// paused. Adoption ignores
    /// it, as it does the hold.
    pause: watch::Sender<Option<Pause>>,
    /// The machine faber runs on and the share declared for it (ADR-0021).
    /// Locked on its own, never while `inner` is held.
    machine: Mutex<MachineState>,
    /// Why the machine is holding admission, if it is: its memory pressure or
    /// growing swap. A channel rather than a flag so that a wait can hear the
    /// hold lift ([`Limiter::machine_signal`]).
    machine_hold: watch::Sender<Option<MachineHold>>,
    /// Where the co-tenant window is kept across restarts (ADR-0021 §3). `None`
    /// until [`Limiter::restore_cotenant`] names it, which only `main`'s path
    /// does: a test never writes the operator's file.
    cotenant_path: Mutex<Option<PathBuf>>,
    /// The co-tenant reserve last logged, so only a real move is logged.
    reserve_logged: AtomicU64,
    /// Per-gate counters of every claim parked since this process started (issue
    /// #85). Each claim's parking cell counts into it.
    gates: Arc<GateTally>,
    /// Last probed `MemAvailable` (MiB), or [`AVAILABLE_UNKNOWN`].
    available_mib: AtomicU64,
    /// Consecutive availability-probe failures; at [`UNHEALTHY_AFTER`] Gate 0
    /// treats Docker as unreachable and refuses admission.
    probe_misses: AtomicU32,
    /// Why faber's Linux VM is not serving Docker (ADR-0026), from the engine
    /// driver ([`crate::vm::engine`]); `None` while it is in service, and always on
    /// a host without one. Gate 0 refuses while it is set.
    engine_fault: Mutex<Option<String>>,
    inner: Mutex<Inner>,
    classes: Mutex<HashMap<String, ClassCap>>,
    /// The panel's warnings, keyed by class and kind: a class that warns again
    /// replaces its entry rather than adding one, and loses it once the learner
    /// or a pin no longer holds it.
    warnings: Mutex<VecDeque<(WarnKey, String)>>,
    wake: tokio::sync::Notify,
    /// The p75 fallback (ADR-0033 §4). Locked on its own, never while `inner` is
    /// held.
    fallback: Mutex<Fallback>,
    /// Fallback trips and re-arms the probe loop saw, waiting for the memory
    /// freeze loop to audit them.
    reserve_events: Mutex<Vec<reserve::Event>>,
    /// Every memory-tick reading, for the memory freeze to decide on (§3).
    readings: watch::Sender<MemReading>,
}

/// A shared service server's memory (ADR-0046 §7), held while the server runs
/// and charged to no job. It debits the Docker budget like an admitted job's
/// reservation, so admission sees the server's memory as spent, but it is not a
/// job: it does not touch `docker_inflight`, so it takes no slot under the
/// concurrency ceiling and does not keep the host from reading as idle. Credited
/// back, and the parked claims woken, on drop.
pub struct ServiceHold {
    limiter: Arc<Limiter>,
    mib: u64,
}

impl Drop for ServiceHold {
    fn drop(&mut self) {
        {
            let mut inner = self.limiter.inner.lock().unwrap();
            inner.reserved_mib = inner.reserved_mib.saturating_sub(self.mib);
        }
        self.limiter.wake.notify_waiters();
    }
}

/// Held for a job's lifetime; releases its reservation back to the ledger on drop.
///
/// The disk charge is for what the job writes in its first minutes, so it stands
/// for the ramp window only while the job runs. The guard drops once the backend
/// has torn the job's containers down, and the charge goes back with its memory
/// (#161): what the job wrote is then in the free-space reading or freed by the
/// teardown, and keeping the charge counted it twice. Short jobs closed the gate
/// with plenty of space free. The same holds for a provision that failed or
/// bailed before any container existed (#89): a burst of failed
/// `generate-jitconfig` calls held 3 GiB each for ten minutes and closed the
/// gate on a near-empty disk.
pub struct AdmissionGuard {
    limiter: Arc<Limiter>,
    reservation: Reservation,
    /// This admission's entry in the disk ledger. `None` for an adoption, which
    /// was never charged.
    ramp: Option<u64>,
    /// The ticket this admission queued on. `None` for an adoption, which never
    /// queued.
    ticket: Option<u64>,
}

impl AdmissionGuard {
    /// The ticket this admission queued on, which a demote keeps (ADR-0063 §5).
    pub fn ticket(&self) -> Option<u64> {
        self.ticket
    }

    /// What this admission debits: `mib` is what the ledger holds for it — the
    /// cap, or the p75 reservation under `"p75"` (ADR-0033 §1) — and so what its
    /// `org.faber.reserved-mib` label must say for an adoption to re-charge it.
    pub fn reservation(&self) -> Reservation {
        self.reservation
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        {
            let mut inner = self.limiter.inner.lock().unwrap();
            inner.reserved_mib = inner.reserved_mib.saturating_sub(self.reservation.mib);
            inner.shadow_reserved_mib = inner
                .shadow_reserved_mib
                .saturating_sub(self.reservation.shadow_mib);
            inner.reserved_mcpu = inner.reserved_mcpu.saturating_sub(self.reservation.mcpu);
            inner.docker_inflight = inner.docker_inflight.saturating_sub(1);
            if let Some(id) = self.ramp {
                inner.disk.release(id);
            }
        }
        // A freed reservation may let a waiter in — re-check every parked admit.
        self.limiter.wake.notify_waiters();
    }
}

/// Where a claim queues: the tier that decides who is the head (ADR-0036,
/// ADR-0063 §5). Every gate binds every tier alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rank {
    /// A repo on `[autolimit] priority_repos`: ahead of everything else.
    Priority,
    /// Every other claim.
    Normal,
    /// Demoted as likely wasted: below every other claim, on `ticket`, the one
    /// it took when it first queued, so demoted claims keep their order.
    Waste { ticket: u64 },
}

/// A place in the admission queue, held for as long as a caller is parked in
/// [`Limiter::admit`]. Dropping it leaves the queue — which must happen on the
/// cancel path too (the `admit` future is dropped when a claim is reclaimed while
/// parked), or a vanished waiter would keep set-aside memory reserved forever.
struct WaiterTicket {
    limiter: Arc<Limiter>,
    id: u64,
}

impl Drop for WaiterTicket {
    fn drop(&mut self) {
        let mut inner = self.limiter.inner.lock().unwrap();
        inner.waiters.remove(&self.id);
        inner.priority_tickets.remove(&self.id);
        inner.waste_tickets.remove(&self.id);
        drop(inner);
        // The head may have just left: younger waiters can stop setting its
        // reservation aside, so re-check every parked admit.
        self.limiter.wake.notify_waiters();
    }
}

impl Limiter {
    pub fn new(cfg: AutoLimitConfig, pinned: &HashMap<String, Limits>) -> Arc<Self> {
        let defaults = Shares::profiling(&cfg);
        let mut classes = HashMap::new();
        for (key, limits) in pinned {
            let mut cap = ClassCap::profiling(key, defaults);
            cap.runner_mib = limits.runner.memory_mib;
            cap.dind_mib = limits.dind.memory_mib;
            cap.status = CapStatus::Pinned;
            cap.pinned = true;
            pin_cpu(&mut cap, limits);
            refresh_shadow(&mut cap, cfg.margin, cfg.window);
            classes.insert(key.clone(), cap);
        }
        Arc::new(Self {
            cfg: RwLock::new(cfg),
            budget_mib: AtomicU64::new(0),
            mem_total_mib: AtomicU64::new(0),
            ncpu: AtomicU64::new(0),
            cpu_budget_mcpu: AtomicU64::new(0),
            cpu_pressure: AtomicU64::new(PRESSURE_UNKNOWN),
            adoption_pending: AtomicBool::new(false),
            hold: Mutex::new(None),
            grow_hold: AtomicBool::new(false),
            pause: watch::Sender::new(None),
            machine: Mutex::new(MachineState::default()),
            machine_hold: watch::Sender::new(None),
            cotenant_path: Mutex::new(None),
            reserve_logged: AtomicU64::new(0),
            gates: Arc::default(),
            available_mib: AtomicU64::new(AVAILABLE_UNKNOWN),
            probe_misses: AtomicU32::new(0),
            engine_fault: Mutex::new(None),
            inner: Mutex::new(Inner::default()),
            classes: Mutex::new(classes),
            warnings: Mutex::new(VecDeque::new()),
            wake: tokio::sync::Notify::new(),
            fallback: Mutex::new(Fallback::default()),
            reserve_events: Mutex::new(Vec::new()),
            readings: watch::Sender::new(MemReading::default()),
        })
    }

    // --- ADR-0033: the reserve mode and its fallback ---------------------------

    /// The configured `[autolimit] reserve` mode (an unknown value is `"max"`).
    pub fn reserve_mode(&self) -> ReserveMode {
        self.cfg.read().unwrap().reserve.mode()
    }

    /// Whether admission debits the p75 reservation at `now_ms`: `"p75"` is
    /// configured and the fallback is not cooling off.
    pub fn reserving_p75(&self, now_ms: u64) -> bool {
        let mode = self.reserve_mode();
        self.fallback.lock().unwrap().reserves_p75(mode, now_ms)
    }

    /// Whether the memory freeze (§3) may freeze a job at `now_ms`.
    pub fn may_freeze_for_memory(&self, now_ms: u64) -> bool {
        let mode = self.reserve_mode();
        self.fallback.lock().unwrap().may_freeze(mode, now_ms)
    }

    /// Gate 2's `headroom_mib` and `band_mib`, which the memory freeze is set
    /// relative to (§3).
    pub fn memory_thresholds(&self) -> (u64, u64) {
        let cfg = self.cfg.read().unwrap();
        (cfg.headroom_mib, cfg.band_mib)
    }

    /// Rebuild the fallback from the audit log's `reserve` lines (startup only).
    pub fn restore_reserve(&self, entries: &[crate::audit::Entry]) {
        *self.fallback.lock().unwrap() = Fallback::replay(entries);
    }

    /// A restart found a job still frozen for memory: the squeeze that froze it is
    /// not over, so the memory freeze stays possible through a cool-off until a
    /// reading above `headroom_mib + band_mib` (ADR-0033 §3).
    pub fn resume_squeeze(&self) {
        self.fallback.lock().unwrap().resume_squeeze();
    }

    /// Trip the fallback for `cause` (a memory freeze, a host OOM). `None` when
    /// nothing tripped: not `"p75"`, or the same squeeze.
    pub fn trip_reserve(&self, cause: TripCause, now_ms: u64) -> Option<reserve::Event> {
        let mode = self.reserve_mode();
        let event = self.fallback.lock().unwrap().trip(mode, cause, now_ms);
        if event.is_some() {
            self.wake.notify_waiters();
        }
        event
    }

    /// The trips and re-arms the probe loop saw since the last call, for the
    /// caller to audit.
    pub fn take_reserve_events(&self) -> Vec<reserve::Event> {
        std::mem::take(&mut *self.reserve_events.lock().unwrap())
    }

    /// Every memory reading as it is taken, for the memory freeze.
    pub fn memory_readings(&self) -> watch::Receiver<MemReading> {
        self.readings.subscribe()
    }

    /// Fold one memory reading (`None` = the probe failed) into the fallback and
    /// hand it to the memory freeze. Called on every memory tick.
    fn observe_memory(&self, available_mib: Option<u64>, now_ms: u64) {
        let mode = self.reserve_mode();
        let (headroom, band) = self.memory_thresholds();
        let events =
            self.fallback
                .lock()
                .unwrap()
                .observe(mode, available_mib, headroom, band, now_ms);
        if !events.is_empty() {
            // A re-arm shrinks every parked claim's debit: let them re-check.
            self.wake.notify_waiters();
            self.reserve_events.lock().unwrap().extend(events);
        }
        self.readings.send_modify(|r| {
            r.seq += 1;
            r.available_mib = available_mib;
            r.at_ms = now_ms;
        });
    }

    /// Feed a memory reading as the probe loop would: the stored availability
    /// Gate 2 reads, and the fallback.
    #[cfg(test)]
    pub(crate) fn observe_memory_for_test(&self, available_mib: Option<u64>, now_ms: u64) {
        self.available_mib
            .store(available_mib.unwrap_or(AVAILABLE_UNKNOWN), Ordering::SeqCst);
        self.observe_memory(available_mib, now_ms);
    }

    /// Set `[autolimit] reserve` as a restart with it in the config would.
    #[cfg(test)]
    pub(crate) fn set_reserve_for_test(&self, setting: crate::config::ReserveSetting) {
        self.cfg.write().unwrap().reserve = setting;
    }

    /// The report as the learner should see it (ADR-0033 §5), or `None` to keep
    /// the run out of it. See [`learnable_under`].
    pub fn learnable(&self, report: &RunReport) -> Option<RunReport> {
        learnable_under(self.reserve_mode(), report)
    }

    /// The per-container caps (and whether this is a profiling run) to launch a
    /// class's next job with. An unknown class profiles at the bounded ceiling.
    /// With CPU admission on, the runner also gets its class's CPU share as a
    /// `--cpus` quota (see [`Limiter::sizing_for`] for the cohort version).
    ///
    /// `dind` is the profile's daemon grant. A class served by a profile that was
    /// granted none is sized — and charged — for one container, because that is
    /// how many will exist. Without this the ledger would hold the whole profiling
    /// ceiling (gigabytes) against a container faber never starts, and, since no
    /// dind sample is ever taken for such a class, would keep holding it forever:
    /// the learner only lowers a cap on evidence, and there is none coming.
    ///
    /// Tests only: production sizes a container for its whole queued cohort
    /// ([`Limiter::sizing_for`]), which this is the one-class case of.
    #[cfg(test)]
    pub fn limits_for(&self, class: &str, dind: bool) -> (Limits, bool) {
        let cfg = self.cfg.read().unwrap();
        let classes = self.classes.lock().unwrap();
        let own = Shares::of(&classes, class, Shares::profiling(&cfg));
        let limits = container_limits(
            own.runner_mib,
            own.dind_mib,
            self.runner_quota(&cfg.cpu, own.runner_mcpu),
        );
        (without_dind(limits, dind), !classes.contains_key(class))
    }

    /// What a class's next job will reserve: runner + dind cap, and runner + dind
    /// CPU share.
    pub fn reservation_for(&self, class: &str, dind: bool) -> Reservation {
        let cfg = self.cfg.read().unwrap();
        let classes = self.classes.lock().unwrap();
        Shares::of(&classes, class, Shares::profiling(&cfg)).reservation(dind)
    }

    /// The CPU to charge for an adopted container that carries no
    /// `org.faber.reserved-mcpu` label — one a binary from before ADR-0019 minted:
    /// its class's current share, or nothing for a class this process knows
    /// nothing about. Unlike memory, a guess here cannot hurt the host: CPU is
    /// compressible, and the pressure gate sees what the ledger misses.
    pub fn known_mcpu_for(&self, class: &str, dind: bool) -> u64 {
        let classes = self.classes.lock().unwrap();
        classes
            .get(class)
            .map_or(0, |c| c.runner_mcpu + if dind { c.dind_mcpu } else { 0 })
    }

    /// Size a runner for **every job it could be handed**, not just the one it was
    /// minted for.
    ///
    /// A JIT runner is not job-pinned: faber mints it for one queued job, but
    /// GitHub dispatches whichever queued job it likes to the freshly-registered
    /// runner, and the two come apart routinely (a few percent of mints across a
    /// long live log; every mint in some short windows). Sizing the containers
    /// for the minted-for class alone therefore runs job B under the cgroup limit
    /// computed for job A — and when B is the heavier of the two, the kernel kills
    /// it mid-step. The same commit then passes or fails run to run depending on a
    /// pairing nothing in the workflow controls.
    ///
    /// So the **container ceiling** is the per-container max over `cohort` — the
    /// classes of every job currently queued that this runner's labels can serve.
    /// Whichever of them GitHub picks, the container is already big enough, so the
    /// OOM is structurally impossible rather than merely unlikely. The runner's
    /// `--cpus` quota (ADR-0019, only with CPU admission on) is the same max over
    /// the cohort's runner CPU shares, for the same reason: a quota sized for a
    /// light class would throttle the heavy job GitHub hands it into its timeouts.
    ///
    /// The **reservation** stays the minted-for class's own, deliberately decoupled
    /// from the ceiling. The ledger's job is to bound the *aggregate* footprint, and
    /// dispatch is a permutation: every cohort job runs exactly once, so the total
    /// memory the cohort consumes is the same whichever runner takes which job.
    /// Reserving the ceiling for every runner would instead count the single
    /// heaviest job once per runner — one 10 GiB class in a cohort of six would
    /// demand 60 GiB of an 18 GiB budget and collapse concurrency to one runner —
    /// while buying no protection for the one container that will actually hold it.
    /// What that leaves exposed is only an *ordering* effect (light-minted runners
    /// admitted first, then handed the heavy job), which is what Gate 2's live
    /// `MemAvailable` check, `headroom_mib` and `reserve_mib` are there to absorb.
    ///
    /// The ceiling maxes over each class's **stored** cap — already clamped by
    /// [`apply_want`] — never over a raw `want`, so a cohort can never grant a class
    /// more than the escalation guard would.
    pub fn sizing_for(&self, class: &str, cohort: &[String], dind: bool) -> Sizing {
        let cfg = self.cfg.read().unwrap();
        let classes = self.classes.lock().unwrap();
        // An unknown class would profile at the bounded ceiling, so a runner that
        // could be handed it must be able to hold that much too.
        let defaults = Shares::profiling(&cfg);
        let label_of = |key: &str| {
            classes
                .get(key)
                .map_or_else(|| key.to_string(), |c| c.label.clone())
        };
        let own = Shares::of(&classes, class, defaults);
        let profiling = !classes.contains_key(class);

        let mut runner_mib = own.runner_mib;
        let mut dind_mib = own.dind_mib;
        let mut runner_mcpu = own.runner_mcpu;
        // The peer that pushed the ceiling furthest above our own, for the log line.
        let mut raised_by: Option<(u64, String)> = None;
        // The peer holding the largest runner CPU share, if it beat our own.
        let mut quota_raised_by: Option<String> = None;
        for peer in cohort.iter().filter(|p| p.as_str() != class) {
            let s = Shares::of(&classes, peer, defaults);
            if s.runner_mib > runner_mib || s.dind_mib > dind_mib {
                let (r, d) = (s.runner_mib, s.dind_mib);
                if raised_by.as_ref().is_none_or(|(w, _)| r + d > *w) {
                    raised_by = Some((r + d, label_of(peer)));
                }
                runner_mib = runner_mib.max(r);
                dind_mib = dind_mib.max(d);
            }
            if s.runner_mcpu > runner_mcpu {
                runner_mcpu = s.runner_mcpu;
                quota_raised_by = Some(label_of(peer));
            }
        }
        let quota = self.runner_quota(&cfg.cpu, runner_mcpu);
        // A peer that raised only the quota still sized this runner; name it when
        // no memory peer did, and only when a quota is actually applied.
        let raised_by = raised_by
            .map(|(_, label)| label)
            .or(quota_raised_by.filter(|_| quota > 0.0));
        // Both numbers lose their dind half together when the profile was granted
        // no daemon — the cohort is one profile's queue, so every class in it is
        // served by the same grant and there is no peer whose dind could raise it.
        Sizing {
            limits: without_dind(container_limits(runner_mib, dind_mib, quota), dind),
            reservation: own.reservation(dind),
            profiling,
            raised_by,
        }
    }

    /// Wait until `reservation` fits every gate, then admit. The returned guard
    /// holds the reservation until dropped (job teardown).
    ///
    /// Admission is **first-come-first-served with a head set-aside**: the caller
    /// takes a ticket, and Gate 1 refuses anyone whose reservation would encroach
    /// on what the longest-waiting caller needs. Without it, admission is a lottery
    /// every waiter re-enters each second — and a large reservation loses it
    /// indefinitely, because a stream of small jobs keeps the ledger just full
    /// enough that the big one never fits (observed live: a large claim parked
    /// for over an hour while small ones were served continuously).
    ///
    /// `on_wait` hears every refusal, so a caller can say which gate it is parked
    /// behind — and a stall always has a name rather than only a duration.
    pub async fn admit(
        self: &Arc<Self>,
        reservation: Reservation,
        on_wait: impl FnMut(&Wait),
    ) -> AdmissionGuard {
        self.admit_as(reservation, false, on_wait).await
    }

    /// [`admit`](Self::admit) for a claim whose repo is (`priority`) or is not on
    /// `[autolimit] priority_repos` (ADR-0036). A priority claim queues ahead of
    /// every non-priority one, first-come-first-served within each tier; every
    /// gate still applies to it, and nothing already admitted is preempted.
    pub async fn admit_as(
        self: &Arc<Self>,
        reservation: Reservation,
        priority: bool,
        on_wait: impl FnMut(&Wait),
    ) -> AdmissionGuard {
        let rank = if priority {
            Rank::Priority
        } else {
            Rank::Normal
        };
        self.admit_ranked(reservation, rank, on_wait).await
    }

    /// [`admit`](Self::admit) at `rank`: the tiers of ADR-0036, and the waste tier
    /// of ADR-0063 below them both. A [`Rank::Waste`] claim re-queues on the
    /// ticket it first took, so demoted claims go in the order they arrived.
    pub async fn admit_ranked(
        self: &Arc<Self>,
        reservation: Reservation,
        rank: Rank,
        mut on_wait: impl FnMut(&Wait),
    ) -> AdmissionGuard {
        // Dropped on both exits below — admission and cancel — so the queue only
        // ever holds callers that are still parked.
        let ticket = self.take_ticket_ranked(reservation, rank);
        loop {
            match self.try_admit_charged(ticket.id, reservation) {
                Ok((ramp, debited)) => {
                    return AdmissionGuard {
                        limiter: self.clone(),
                        reservation: debited,
                        ramp: Some(ramp),
                        ticket: Some(ticket.id),
                    }
                }
                Err(wait) => on_wait(&wait),
            }
            let _ = tokio::time::timeout(ADMIT_RETRY, self.wake.notified()).await;
        }
    }

    /// Hold new Docker admissions for `reason` until
    /// [`release_hold`](Self::release_hold) — the operator's stop (issue #66).
    ///
    /// Holding again replaces the reason and keeps the original `since_ms`, so
    /// "held since" keeps meaning when admission stopped; `now_ms` dates only a
    /// hold that is new. Restoring a persisted hold at startup passes its own
    /// `since_ms` here, which is exactly that case.
    ///
    /// Stops only what [`admit`](Self::admit) would start: admitted and adopted
    /// containers run on, and a claim taken while held parks and says it is held.
    pub fn hold(&self, reason: String, now_ms: u64) {
        {
            let mut hold = self.hold.lock().unwrap();
            let since_ms = hold.as_ref().map_or(now_ms, |h| h.since_ms);
            *hold = Some(Hold { reason, since_ms });
        }
        // Parked claims re-check at once, so they report the hold now rather than
        // at their next retry.
        self.wake.notify_waiters();
    }

    /// Lift the hold, if one is placed; `true` when there was one. Parked claims
    /// re-check at once rather than at their next retry.
    pub fn release_hold(&self) -> bool {
        let released = self.hold.lock().unwrap().take().is_some();
        if released {
            self.wake.notify_waiters();
        }
        released
    }

    /// Hold or release Docker admission for a grow of the Linux VM's disk
    /// (ADR-0061 §3). Parked claims re-check at once.
    pub fn set_grow_hold(&self, on: bool) {
        if self.grow_hold.swap(on, Ordering::SeqCst) != on {
            self.wake.notify_waiters();
        }
    }

    /// The hold in force, if any.
    pub fn current_hold(&self) -> Option<Hold> {
        self.hold.lock().unwrap().clone()
    }

    /// Pause: nothing new starts until [`resume`](Self::resume) — no Docker or VM
    /// job is admitted (ADR-0023). Admitted and adopted containers are not starts, and run on.
    ///
    /// The operator's pause always takes, replacing the janitor's, so the end of a
    /// reclamation can never lift what the operator asked for. The janitor's takes
    /// only on an unpaused faber. `true` when the pause in force is now the
    /// caller's and was not before — for the janitor, whether it is its own to lift.
    pub fn pause(&self, by: Pause) -> bool {
        let took = self.pause.send_if_modified(|pause| {
            let take = match by {
                Pause::Operator => *pause != Some(Pause::Operator),
                Pause::Reclaim => pause.is_none(),
            };
            if take {
                *pause = Some(by);
            }
            take
        });
        if took {
            // Parked claims re-check at once, so they report the pause now rather
            // than at their next retry.
            self.wake.notify_waiters();
        }
        took
    }

    /// Lift a pause; every parked claim re-checks at once. The operator's resume
    /// lifts any pause, the janitor's included, as it always has. The janitor's
    /// lifts only its own. `true` when this call lifted one.
    pub fn resume(&self, by: Pause) -> bool {
        let lifted = self.pause.send_if_modified(|pause| {
            let lift = match by {
                Pause::Operator => pause.is_some(),
                Pause::Reclaim => *pause == Some(Pause::Reclaim),
            };
            if lift {
                *pause = None;
            }
            lift
        });
        if lifted {
            self.wake.notify_waiters();
        }
        lifted
    }

    /// Who has paused faber, if anyone.
    pub fn current_pause(&self) -> Option<Pause> {
        *self.pause.borrow()
    }

    /// Refuse every admission until [`adoption_settled`](Self::adoption_settled).
    /// Called once at startup, before the reconcile loop can claim anything.
    pub fn hold_for_adoption(&self) {
        self.adoption_pending.store(true, Ordering::SeqCst);
    }

    /// Adoption has enumerated the host; admission may proceed (ADR-0011 §5).
    pub fn adoption_settled(&self) {
        if self.adoption_pending.swap(false, Ordering::SeqCst) {
            self.wake.notify_waiters();
        }
    }

    /// Whether admission is still held shut waiting on adoption — for the
    /// dashboard, so "nothing is starting" has a visible reason.
    pub fn awaiting_adoption(&self) -> bool {
        self.adoption_pending.load(Ordering::SeqCst)
    }

    /// The human label already learned for `class`, if any.
    ///
    /// An adopted run has a class key but no workflow *name* (that never reached
    /// a container label, and the key carries only the workflow id). Reusing what
    /// the class already answers to is what stops an adoption from renaming a
    /// class in the dashboard to a degraded form of its own name.
    pub fn label_for(&self, class: &str) -> Option<String> {
        self.classes
            .lock()
            .unwrap()
            .get(class)
            .map(|c| c.label.clone())
    }

    /// Charge a reservation for a container that is **already running** — a claim
    /// rebuilt from Docker after a restart (ADR-0011).
    ///
    /// Unlike [`admit`](Self::admit) this never waits and consults no gate. The
    /// memory is already spent: the container holds it whatever the ledger says,
    /// and refusing here would free nothing — it would only make the ledger lie
    /// about a host that is genuinely full. The gates still bind every *new*
    /// claim, which is the point: they now queue behind an honest number instead
    /// of a zero.
    ///
    /// The returned guard releases on drop exactly like an admitted one, so an
    /// adopted run and a minted one leave the ledger by the same path.
    pub fn adopt(self: &Arc<Self>, reservation: Reservation) -> AdmissionGuard {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.reserved_mib += reservation.mib;
            inner.shadow_reserved_mib += reservation.shadow_mib;
            inner.reserved_mcpu += reservation.mcpu;
            inner.docker_inflight += 1;
        }
        AdmissionGuard {
            limiter: self.clone(),
            reservation,
            ramp: None,
            ticket: None,
        }
    }

    /// Hold `mib` of the Docker budget for a shared service server
    /// ([`ServiceHold`]). Like [`adopt`](Self::adopt) it never waits and consults
    /// no gate: the caller holds it for a server it is about to run or already
    /// runs, and the gates bind the jobs that come after.
    pub fn hold_service(self: &Arc<Self>, mib: u64) -> ServiceHold {
        self.inner.lock().unwrap().reserved_mib += mib;
        ServiceHold {
            limiter: self.clone(),
            mib,
        }
    }

    /// Size the budget as if the VM had `total_mib`, for a test with no Docker to
    /// probe. A limiter starts with a zero budget, which Gate 1 reads as "not probed
    /// yet" and admits nothing against.
    #[cfg(test)]
    pub(crate) fn set_vm_total_for_test(&self, total_mib: u64) {
        self.mem_total_mib.store(total_mib, Ordering::SeqCst);
        self.recompute_budget();
    }

    /// Size the CPU budget as if the VM had `ncpu` cores (`0` = unprobed).
    #[cfg(test)]
    pub(crate) fn set_ncpu_for_test(&self, ncpu: u64) {
        self.ncpu.store(ncpu, Ordering::SeqCst);
        self.recompute_budget();
    }

    /// Feed a VM CPU-pressure reading as the probe loop would (`None` = no PSI).
    #[cfg(test)]
    pub(crate) fn set_cpu_pressure_for_test(&self, some_avg60: Option<f64>) {
        self.observe_cpu_pressure(some_avg60);
    }

    /// Feed a free-space reading of the Docker volume as the disk loop would,
    /// taken at `at_ms` (`None` = a measurement that failed).
    #[cfg(test)]
    pub(crate) fn set_disk_free_for_test(&self, free_mib: Option<u64>, at_ms: u64) {
        self.observe_disk_free(free_mib, at_ms);
    }

    /// Feed what a read of the Linux VM's disk came to, as the disk loop would.
    #[cfg(test)]
    pub(crate) fn set_vm_disk_for_test(&self, vm: host::VmDisk, at_ms: u64) {
        self.observe_vm_disk(vm, at_ms);
    }

    /// Feed a co-tenant sample as the probe loop would, taken at `at_ms`.
    #[cfg(test)]
    pub(crate) fn observe_cotenant_for_test(&self, mib: u64, at_ms: u64) {
        self.observe_cotenant(mib, at_ms);
    }

    /// Feed a reading of the machine as the machine loop would, taken at `at_ms`
    /// (`None` = one that failed).
    #[cfg(test)]
    pub(crate) fn set_machine_for_test(&self, reading: Option<MachineReading>, at_ms: u64) {
        self.observe_machine(reading, at_ms);
    }

    /// Close Gate 0 as a run of failed availability probes would, or open it as
    /// one good probe does.
    #[cfg(test)]
    pub(crate) fn set_docker_reachable_for_test(&self, reachable: bool) {
        let misses = if reachable { 0 } else { UNHEALTHY_AFTER };
        self.probe_misses.store(misses, Ordering::SeqCst);
    }

    /// How many callers are parked in [`Self::admit`] right now.
    #[cfg(test)]
    pub(crate) fn parked(&self) -> usize {
        self.inner.lock().unwrap().waiters.len()
    }

    /// Join the admission queue as a non-priority claim. Tests only: production
    /// joins through [`admit_as`](Self::admit_as), which names the tier.
    #[cfg(test)]
    fn take_ticket(self: &Arc<Self>, reservation: impl Into<Reservation>) -> WaiterTicket {
        self.take_ticket_ranked(reservation, Rank::Normal)
    }

    /// Whether `repo` is on `[autolimit] priority_repos` (ADR-0036). Case-insensitive.
    pub fn is_priority_repo(&self, repo: &str) -> bool {
        let repo = repo.trim();
        self.cfg
            .read()
            .unwrap()
            .priority_repos
            .iter()
            .any(|p| p.trim().eq_ignore_ascii_case(repo))
    }

    fn take_ticket_ranked(
        self: &Arc<Self>,
        reservation: impl Into<Reservation>,
        rank: Rank,
    ) -> WaiterTicket {
        let mut inner = self.inner.lock().unwrap();
        let id = match rank {
            // Tickets are never handed out twice, and the one a demoted claim took
            // left the queue with its admission, so it is free to take back.
            Rank::Waste { ticket } => {
                inner.waste_tickets.insert(ticket);
                ticket
            }
            Rank::Normal | Rank::Priority => {
                let id = inner.next_ticket;
                inner.next_ticket += 1;
                if rank == Rank::Priority {
                    inner.priority_tickets.insert(id);
                }
                id
            }
        };
        inner.waiters.insert(id, reservation.into());
        WaiterTicket {
            limiter: self.clone(),
            id,
        }
    }

    /// Whether the Docker daemon is answering, per the availability probe's recent
    /// record. `false` only after [`UNHEALTHY_AFTER`] consecutive misses, so a
    /// one-off failure doesn't read as an outage.
    ///
    /// Also `false` while faber's Linux VM is not in service ([`Self::engine_fault`]):
    /// the Docker it serves is unreachable then whatever a probe says, and the fault
    /// says why.
    pub fn docker_reachable(&self) -> bool {
        self.probe_misses.load(Ordering::SeqCst) < UNHEALTHY_AFTER && self.engine_fault().is_none()
    }

    /// Why faber's Linux VM is not serving Docker, while it is not (ADR-0026).
    pub fn engine_fault(&self) -> Option<String> {
        self.engine_fault.lock().unwrap().clone()
    }

    /// The engine driver's report: `None` once its VM is in service. A change wakes
    /// the parked claims, so admission reopens the moment it serves.
    pub fn set_engine_fault(&self, fault: Option<String>) {
        let changed = {
            let mut g = self.engine_fault.lock().unwrap();
            let changed = *g != fault;
            *g = fault;
            changed
        };
        if changed {
            self.wake.notify_waiters();
        }
    }

    /// Set the machine's share (ADR-0021 §2) from `[host] keep_memory_gib` and
    /// `keep_cpus`. Called once at startup: like the rest of `[host]`, it applies
    /// on restart.
    pub fn set_machine_share(&self, share: MachineShare) {
        self.machine.lock().unwrap().set_share(share);
        self.recompute_budget();
        self.wake.notify_waiters();
    }

    /// The machine's memory and cores, once read — what the settings save checks
    /// a share against.
    pub fn machine_size(&self) -> Option<MachineSize> {
        self.machine.lock().unwrap().size()
    }

    /// Record the wall's latest judgement (ADR-0026). Called by whatever polls the
    /// verdict file; a judgement that stops coming goes stale in the reader
    /// ([`crate::wall::report::judge`]) and so arrives as red.
    pub fn observe_wall(&self, judgement: Judgement) {
        let gate = WallGate::from_judgement(judgement);
        let changed = {
            let mut inner = self.inner.lock().unwrap();
            let changed = inner.vm.wall.as_str() != gate.as_str();
            inner.vm.wall = gate.clone();
            inner.vm.wall_at_ms = now_ms();
            changed
        };
        if changed {
            match gate.detail() {
                Some(why) => warn!(
                    "autolimiter: wall {} — starting no VM job: {why}",
                    gate.as_str()
                ),
                None => info!("autolimiter: wall is green — VM admission open"),
            }
        }
        self.wake.notify_waiters();
    }

    /// Gate macOS VM jobs on the session account's supervisor (ADR-0051), in
    /// reboot mode `mode`: set at boot where the record names the account and a
    /// `macos-vm` profile is configured. Closed until the first probe answers.
    pub fn require_vm_session(&self, mode: SessionMode) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.vm.session_mode = mode;
            inner.vm.session = VmSession::Unprobed(mode);
        }
        self.wake.notify_waiters();
    }

    /// Record a probe of the session supervisor: `Ok` when it answered the
    /// handshake, else why not. Ignored unless [`Self::require_vm_session`] was
    /// called; logged only when the state changes.
    pub fn observe_vm_session(&self, probe: Result<(), String>) {
        self.set_vm_session(|mode| match probe {
            Ok(()) => VmSession::Up,
            Err(why) => VmSession::Down { mode, why },
        });
    }

    /// Record that the record's session account is not this host's
    /// `_faber_mac` (`why` says how), so faber does not connect to it.
    pub fn observe_no_session_account(&self, why: String) {
        self.set_vm_session(|_| VmSession::NoAccount(why));
    }

    fn set_vm_session(&self, next: impl FnOnce(SessionMode) -> VmSession) {
        let (before, after) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.vm.session == VmSession::NotRequired {
                return;
            }
            let next = next(inner.vm.session_mode);
            let before = std::mem::replace(&mut inner.vm.session, next.clone());
            (before, next)
        };
        if before.as_str() != after.as_str() {
            match &after {
                VmSession::Up => {
                    info!("autolimiter: the macOS VM session answers — macOS VM admission open")
                }
                VmSession::Down { mode, why } => warn!(
                    "autolimiter: no macOS VM session ({why}) — starting no macOS VM job: {}",
                    mode.action()
                ),
                VmSession::NoAccount(why) => {
                    warn!("autolimiter: {why} — starting no macOS VM job")
                }
                _ => {}
            }
        }
        self.wake.notify_waiters();
    }

    /// The Linux VM's size as the engine starts it, for its `/api/state` row
    /// (popover/vms/anatomy.md O1: no protocol change, the daemon joins sizes in).
    pub fn set_linux_vm_size(&self, cpus: u8, memory_mib: u32) {
        self.inner.lock().unwrap().vm.linux_vm_size = Some((cpus, memory_mib));
    }

    /// Publish the supervisor's list of VMs to `/api/state` (display only),
    /// replacing every row.
    pub fn observe_vms(&self, records: &[VmRecord]) {
        self.publish_vms(None, records);
    }

    /// Publish one supervisor's list, replacing only the rows of `kind`. A host
    /// can reach two supervisors (the Linux VM's and the macOS jobs' session
    /// one); each lists its own kind, so neither list may clear the other's rows.
    pub fn observe_vms_of(&self, kind: VmKind, records: &[VmRecord]) {
        self.publish_vms(Some(kind), records);
    }

    fn publish_vms(&self, only: Option<VmKind>, records: &[VmRecord]) {
        let mut inner = self.inner.lock().unwrap();
        let linux = inner.vm.linux_vm_size;
        let fresh = records
            .iter()
            .filter(|r| only.is_none_or(|k| r.kind == k))
            .map(|r| {
                let (cpus, memory_mib) = match (r.kind, linux) {
                    (VmKind::Linux, Some((c, m))) => (Some(c), Some(m)),
                    _ => (None, None),
                };
                VmRow {
                    slot: r.slot.get(),
                    kind: r.kind,
                    state: r.state,
                    stop_reason: r.stop_reason,
                    started_unix: r.started_unix.get(),
                    cpus,
                    memory_mib,
                    job: None,
                    claim: r.claim.as_str().to_string(),
                }
            });
        inner.vm.vms = match only {
            None => fresh.collect(),
            Some(k) => {
                let mut rows: Vec<VmRow> = inner.vm.vms.drain(..).filter(|r| r.kind != k).collect();
                rows.extend(fresh);
                rows
            }
        };
        let now = now_ms();
        match only {
            None => {
                inner.vm.listed_at_ms.insert(VmKind::Linux, now);
                inner.vm.listed_at_ms.insert(VmKind::Macos, now);
            }
            Some(k) => {
                inner.vm.listed_at_ms.insert(k, now);
            }
        }
    }

    /// Gate macOS VM jobs on their golden image's verification (ADR-0049 §7):
    /// set at boot on a host with a `macos-vm` profile. Every image is
    /// [`Standing::Unknown`], and refuses, until [`Self::observe_golden`].
    pub fn require_verified_images(&self) {
        self.inner.lock().unwrap().vm.golden_required = true;
        self.wake.notify_waiters();
    }

    /// Record how golden image `image` stands, and publish the images' standing
    /// as one in `vm.images.macos` (#268). Logged when it changes.
    pub fn observe_golden(&self, image: &str, standing: Standing) {
        let changed = {
            let mut inner = self.inner.lock().unwrap();
            if !standing.admits() {
                inner.vm.golden_pins.remove(image);
            }
            let before = inner.vm.golden.insert(image.to_string(), standing.clone());
            inner.vm.images.macos = Summary::worst(inner.vm.golden.values().map(Standing::summary));
            before.as_ref() != Some(&standing)
        };
        if changed {
            match standing.detail(image) {
                None => {
                    info!("autolimiter: golden image {image} is verified — macOS jobs may use it")
                }
                Some(why) if standing == Standing::Verifying => info!("autolimiter: {why}"),
                Some(why) => warn!("autolimiter: starting no macOS job on it: {why}"),
            }
        }
        self.wake.notify_waiters();
    }

    /// Pin verified image `image` to `identity`, at `dir`: from now on admission
    /// checks the image and the host against it before it admits a job.
    pub fn pin_golden(
        &self,
        image: &str,
        dir: std::path::PathBuf,
        identity: crate::vm::verify::Identity,
    ) {
        self.inner
            .lock()
            .unwrap()
            .vm
            .golden_pins
            .insert(image.to_string(), (dir, identity));
    }

    /// Re-check a pinned image against what is on disk now, outside the lock: a
    /// drift makes it unverified at once and asks the verifier to run.
    fn recheck_golden(&self, image: &str) {
        let pinned = self
            .inner
            .lock()
            .unwrap()
            .vm
            .golden_pins
            .get(image)
            .cloned();
        let Some((dir, verified)) = pinned else {
            return;
        };
        let why = match crate::vm::verify::Identity::now(&dir) {
            Ok(now) => verified.drift(&now),
            Err(e) => Some(crate::vm::verify::Why::Unreadable(e)),
        };
        if let Some(why) = why {
            self.observe_golden(image, Standing::Unverified(why));
            let _ = crate::vm::verify::request(image);
        }
    }

    /// How golden image `image` stands, as [`Self::observe_golden`] last said.
    pub fn golden(&self, image: &str) -> Standing {
        self.inner
            .lock()
            .unwrap()
            .vm
            .golden
            .get(image)
            .cloned()
            .unwrap_or_default()
    }

    /// Publish what the runner-image loader found wrong in the Linux VM's Docker,
    /// or `None` once nothing is (display only: the engine driver holds Docker
    /// back itself while a load runs).
    pub fn observe_runner_image(&self, problem: Option<String>) {
        self.inner.lock().unwrap().vm.runner_image = problem;
    }

    /// Publish the Linux image's standing alone, leaving the golden image's as
    /// it is: each image is judged by its own owner (display only).
    pub fn observe_linux_image(&self, standing: ImageStanding) {
        self.inner.lock().unwrap().vm.images.linux = standing;
    }

    /// Make Docker admission wait on the wall too: set once Docker runs in the
    /// Linux VM faber owns (ADR-0026). Unset, Docker admission is as before.
    pub fn set_wall_required(&self, required: bool) {
        self.inner.lock().unwrap().vm.wall_required = required;
        self.wake.notify_waiters();
    }

    /// The Linux VM's memory (MiB), reserved inside the machine share, from the
    /// engine driver: what its VM holds while one runs, zero when none does. Zero
    /// too on a host without one, where Docker is outside the share.
    pub fn set_linux_vm_mib(&self, mib: u64) {
        self.inner.lock().unwrap().vm.linux_vm_mib = mib;
        self.wake.notify_waiters();
    }

    /// Record whether a `macos-vm` profile is configured, read from the config at
    /// boot. Unset, the share counts no macOS VMs beside the Linux VM.
    pub fn set_runs_macos_vms(&self, runs: bool) {
        self.inner.lock().unwrap().vm.runs_macos_vms = runs;
    }

    /// What a switched-off Linux VM still waits for, as the notice to show, or
    /// `None` once it waits for nothing (ADR-0051).
    pub fn set_linux_vm_waiting(&self, notice: Option<String>) {
        self.inner.lock().unwrap().vm.linux_vm_waiting = notice;
    }

    /// The notice [`Self::set_linux_vm_waiting`] last set.
    pub fn linux_vm_waiting(&self) -> Option<String> {
        self.inner.lock().unwrap().vm.linux_vm_waiting.clone()
    }

    /// Record the per-host VM layout (ADR-0051), read from the config at boot:
    /// the macOS class's cap and what each VM takes of the share, for admission
    /// and for the share's allowance alike. Unset, Apple's two at 8 GiB.
    pub fn set_vm_layout(&self, layout: crate::config::VmLayout) {
        self.inner.lock().unwrap().vm.layout = layout;
        self.wake.notify_waiters();
    }

    /// Start a macOS VM job (ADR-0026): the operator's hold, a pause and the
    /// machine's own pressure first, as for a Docker job, then the wall, the disk
    /// floor, the class cap (two, or the layout's, ADR-0051) and the machine share
    /// ([`admit::decide_macos`]). Decided under the same lock as Docker's
    /// admission. The job counts until the guard drops.
    ///
    /// `image` is the job's golden image, gated on its verification once
    /// [`Self::require_verified_images`] (ADR-0049 §7). `None` is a verification
    /// run, which takes a macOS place like a job but is how an image becomes
    /// verified.
    pub fn vm_begin(self: &Arc<Self>, image: Option<&str>) -> Result<VmRun, Wait> {
        self.vm_begin_with_held(crate::vm::job::global().quarantined().len(), image)
    }

    /// [`Self::vm_begin`] given the VMs faber holds without running a job in
    /// them ([`MacosInput::macos_held`]).
    fn vm_begin_with_held(
        self: &Arc<Self>,
        macos_held: usize,
        image: Option<&str>,
    ) -> Result<VmRun, Wait> {
        if let Some(reason) = self.current_hold() {
            return Err(Wait::Held {
                reason: reason.reason,
            });
        }
        if let Some(by) = self.current_pause() {
            return Err(Wait::Paused { by });
        }
        // ADR-0049 §7: the image a pass verified must be the image on disk now.
        if let Some(name) = image {
            self.recheck_golden(name);
        }
        let share_left = {
            let machine = self.machine.lock().unwrap();
            machine
                .size()
                .and_then(|size| machine.share().memory_left_mib(size))
        };
        let machine_hold = self.current_machine_hold();
        let mut inner = self.inner.lock().unwrap();
        if let Some(hold) = machine_hold {
            let floor = matches!(hold, MachineHold::Pressure(_))
                && inner.docker_inflight + inner.vm.macos_running == 0;
            if !floor {
                return Err(self.machine_wait(hold));
            }
        }
        let now = now_ms();
        let (_, ramp_held) = inner.disk.held_against(now, inner.disk.reading);
        let disk = host::short_of_room(
            inner.disk.admission,
            inner.disk.reading,
            ramp_held.saturating_add(MACOS_VM_DISK_CHARGE_MIB),
            now,
        );
        let unknown = Standing::Unknown;
        let gated = image
            .filter(|_| inner.vm.golden_required)
            .map(|name| (name, inner.vm.golden.get(name).unwrap_or(&unknown)));
        admit::decide_macos(&MacosInput {
            wall: &inner.vm.wall.at(inner.vm.wall_at_ms, now),
            disk,
            session: inner.vm.session.clone(),
            image: gated,
            macos_running: inner.vm.macos_running,
            macos_held,
            macos_cap: inner.vm.layout.macos_vms,
            share_left_mib: share_left,
            linux_vm_mib: inner.vm.linux_vm_mib,
            vm_mib: inner.vm.layout.macos_vm_mib,
        })
        .map_err(Wait::Vm)?;
        inner.vm.macos_running += 1;
        let baseline = inner.disk.reading;
        let ramp = inner.disk.admitted(now, MACOS_VM_DISK_CHARGE_MIB, baseline);
        Ok(VmRun {
            limiter: self.clone(),
            ramp: Some(ramp),
        })
    }

    /// [`Self::vm_begin`], waiting until every gate admits (#263): the claim
    /// parks, as a Docker claim does behind [`Self::admit`], and `on_wait` hears
    /// each refusal so the claim names the gate it waits on. Re-checked whenever
    /// anything that gates it changes (a VM job ending, the wall, the share, a
    /// pause), and every [`ADMIT_RETRY`] besides. Dropping the future is clean:
    /// nothing is counted until it returns.
    pub async fn vm_admit(
        self: &Arc<Self>,
        image: Option<&str>,
        mut on_wait: impl FnMut(&Wait),
    ) -> VmRun {
        loop {
            // Taken before the check, so a release between the check and the
            // wait still wakes this claim.
            let woken = self.wake.notified();
            match self.vm_begin(image) {
                Ok(run) => return run,
                Err(wait) => on_wait(&wait),
            }
            let _ = tokio::time::timeout(ADMIT_RETRY, woken).await;
        }
    }

    /// Count a macOS VM boot adoption found running a job (ADR-0011 for VMs):
    /// unconditionally, past every gate, because the VM holds its memory whatever
    /// the ledger says and refusing here would free nothing. No disk charge: an
    /// adoption is not an admission. Released when the guard drops.
    pub fn vm_adopt(self: &Arc<Self>) -> VmRun {
        self.inner.lock().unwrap().vm.macos_running += 1;
        VmRun {
            limiter: self.clone(),
            ramp: None,
        }
    }

    /// The VM half of `/api/state`: the wall as its own state, apart from the
    /// holds a Docker claim parks behind, and the macOS class.
    fn vm_view(&self) -> VmView {
        let macos_held = crate::vm::job::global().quarantined().len();
        let share_left = {
            let machine = self.machine.lock().unwrap();
            machine
                .size()
                .and_then(|size| machine.share().memory_left_mib(size))
        };
        let inner = self.inner.lock().unwrap();
        let now = now_ms();
        let (_, ramp_held) = inner.disk.held_against(now, inner.disk.reading);
        let disk = host::short_of_room(
            inner.disk.admission,
            inner.disk.reading,
            ramp_held.saturating_add(MACOS_VM_DISK_CHARGE_MIB),
            now,
        );
        let wall = inner.vm.wall.at(inner.vm.wall_at_ms, now);
        let refusal = admit::decide_macos(&MacosInput {
            wall: &wall,
            disk,
            session: inner.vm.session.clone(),
            image: None,
            macos_running: inner.vm.macos_running,
            macos_held,
            macos_cap: inner.vm.layout.macos_vms,
            share_left_mib: share_left,
            linux_vm_mib: inner.vm.linux_vm_mib,
            vm_mib: inner.vm.layout.macos_vm_mib,
        })
        .err();
        VmView {
            wall: wall.as_str(),
            wall_detail: wall.detail(),
            wall_required: inner.vm.wall_required,
            session: inner.vm.session.as_str(),
            session_detail: match &inner.vm.session {
                VmSession::NoAccount(why) => Some(why.clone()),
                VmSession::Unprobed(mode) => Some(format!("not probed yet: {}", mode.action())),
                VmSession::Down { mode, why } => Some(format!("{}: {why}", mode.action())),
                VmSession::NotRequired | VmSession::Up => None,
            },
            macos_running: inner.vm.macos_running,
            macos_cap: inner.vm.layout.macos_vms,
            macos_vm_mib: inner.vm.layout.macos_vm_mib,
            linux_vm_mib: inner.vm.linux_vm_mib,
            refusal: refusal.map(|r| {
                let (gate, detail) = r.describe();
                WaitView::new(gate, detail)
            }),
            vms: inner.vm.vms.clone(),
            // The OLDEST kind that has listed: any stale supervisor dims the section.
            listed_age_s: inner
                .vm
                .listed_at_ms
                .values()
                .min()
                .map(|at| now.saturating_sub(*at) / 1000),
            images: inner.vm.images,
            golden_required: inner.vm.golden_required,
            golden: inner
                .vm
                .golden
                .iter()
                .map(|(image, standing)| GoldenView {
                    image: image.clone(),
                    standing: standing.summary(),
                    detail: standing.detail(image),
                })
                .collect(),
            runner_image: inner.vm.runner_image.clone(),
        }
    }

    /// The refusal for a machine hold on Docker admission: [`Wait::MachineVm`]
    /// when a memory-pressure hold meets a VM larger than the share leaves CI
    /// (#160), which says the VM is the fix, and [`Wait::Machine`] otherwise.
    fn machine_wait(&self, hold: MachineHold) -> Wait {
        if let MachineHold::Pressure(level) = hold {
            let vm_mib = self.mem_total_mib.load(Ordering::SeqCst);
            let machine = self.machine.lock().unwrap();
            let left = machine
                .size()
                .and_then(|size| machine.share().memory_left_mib(size));
            if let Some(left_mib) = left.filter(|&left| vm_mib > left) {
                return Wait::MachineVm {
                    level,
                    vm_mib,
                    left_mib,
                };
            }
        }
        Wait::Machine(hold)
    }

    /// Why the machine is holding admission right now, if it is.
    pub fn current_machine_hold(&self) -> Option<MachineHold> {
        *self.machine_hold.borrow()
    }

    /// Every change in the machine's hold as it happens (ADR-0021 §2c), for a
    /// wait to hear the hold lift without polling for it.
    pub fn machine_signal(&self) -> watch::Receiver<Option<MachineHold>> {
        self.machine_hold.subscribe()
    }

    /// The boot warnings about the machine's share (ADR-0021 §2): an unstated
    /// key, with the measured split; a share the machine cannot hold, which
    /// planning then ignores (the pressure and swap gates still apply); and a VM larger than the share leaves, worded as its fix.
    /// Read after [`Limiter::prime_machine`] and, on a Docker host,
    /// [`Limiter::prime_budget`], so the split is measured rather than missing.
    pub fn machine_warnings(&self) -> Vec<String> {
        let vm_mib = self.mem_total_mib.load(Ordering::SeqCst);
        let vm_ncpu = self.ncpu.load(Ordering::SeqCst);
        let macos_mib = self.macos_allowance_mib();
        let machine = self.machine.lock().unwrap();
        let share = machine.share();
        let size = machine.size();
        let mut out = crate::machine::unstated_warnings(share, size, vm_mib, vm_ncpu);
        if let Some(size) = size {
            out.extend(share.errors(size).into_iter().map(|e| {
                format!(
                    "{e}; too large to plan within, so planning ignores it until it fits \
                     (the pressure and swap gates still apply). Run `faber check-config` to fix \
                     the file."
                )
            }));
        }
        out.extend(machine.encroachment(vm_mib, vm_ncpu, macos_mib));
        out
    }

    /// What the macOS VMs may take of the machine share beside the Linux VM, in
    /// MiB, for [`MachineState::encroachment`]: the class at the layout's cap
    /// (ADR-0051), each VM at the layout's size, where faber
    /// runs its own Linux VM (ADR-0026, "Size and recovery") and a `macos-vm`
    /// profile can start them, zero elsewhere. A host with no such profile never
    /// starts one, and charging it for two would call a VM inside the share too
    /// large. The Linux VM itself is the daemon's `MemTotal`, which inside the VM
    /// is the VM's own size. Taken before `machine` is locked: it takes `inner`.
    fn macos_allowance_mib(&self) -> u64 {
        let vm = &self.inner.lock().unwrap().vm;
        if vm.linux_vm_mib > 0 && vm.runs_macos_vms {
            vm.layout.macos_vms as u64 * vm.layout.macos_vm_mib
        } else {
            0
        }
    }

    /// Store one reading of the machine, taken at `at_ms` (`None` = one that
    /// failed), re-plan within the share, and step the machine's hold. Only a
    /// change of gate is logged, so a swap figure that moves every tick does not
    /// repeat itself.
    fn observe_machine(&self, reading: Option<MachineReading>, at_ms: u64) {
        let hold = {
            let mut machine = self.machine.lock().unwrap();
            machine.observe(reading, at_ms);
            machine.hold(at_ms)
        };
        let was = self.current_machine_hold();
        self.machine_hold.send_if_modified(|h| {
            let changed = *h != hold;
            *h = hold;
            changed
        });
        let gate = |h: Option<MachineHold>| h.map(|h| h.view().0);
        if gate(was) != gate(hold) {
            match hold {
                Some(h) => warn!(
                    "machine: {} (ADR-0021; memory pressure still lets one Docker job run)",
                    h.view().1
                ),
                None => info!("machine: memory pressure normal and swap flat — resuming admission"),
            }
        }
        // The machine's size arrives with its first reading, and the plan with it.
        self.recompute_budget();
        self.wake.notify_waiters();
    }

    /// Read the machine once, before the boot warnings and the first admission.
    pub async fn prime_machine(&self) {
        let reading = crate::platform::host::read_machine().await;
        if reading.is_none() {
            warn!("machine: could not read this machine's memory and cores (ADR-0021)");
        }
        self.observe_machine(reading, now_ms());
    }

    /// The machine tick (ADR-0021 §1): read the machine every [`MACHINE_TICK`] for
    /// the share's plan, its pressure gate and the dashboard. Runs for the process
    /// life, on every host: the pressure gate covers macOS VM jobs, and the reading
    /// is shown whether or not a share is declared.
    pub async fn machine_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(MACHINE_TICK).await;
            let reading = crate::platform::host::read_machine().await;
            self.observe_machine(reading, now_ms());
        }
    }

    /// The per-gate tally a claim's parking cell counts into (issue #85).
    pub fn gate_tally(&self) -> Arc<GateTally> {
        self.gates.clone()
    }

    /// Read the co-tenant window kept at `path` (ADR-0021 §3) and keep it there
    /// from now on. Called once at startup, after the share is set, so a declared
    /// share starts from the day's peak instead of relearning it — and says so,
    /// since that peak is then holding back more than `reserve_mib`.
    pub fn restore_cotenant(&self, path: PathBuf) {
        let now = now_ms();
        let peak = CoTenantPeak::load(&path, now);
        *self.cotenant_path.lock().unwrap() = Some(path);
        let floor = self.cfg.read().unwrap().reserve_mib;
        let reserve = {
            let mut machine = self.machine.lock().unwrap();
            machine.restore_cotenant(peak);
            machine.reserve_mib(floor, now)
        };
        self.reserve_logged.store(reserve, Ordering::SeqCst);
        if reserve > floor {
            warn!("autolimiter: {}", reserve_line(reserve, floor));
        }
        self.recompute_budget();
    }

    /// Take one co-tenant sample (ADR-0021 §3): what the VM uses beyond faber's
    /// own containers, into the 24-hour window, which is written back to disk
    /// when it changed. A sample that fails, or that the bracketing refuses, is
    /// skipped: the next one is a minute away, and the window keeps its peak.
    async fn sample_cotenant(&self, probe_image: &str) {
        let total = self.mem_total_mib.load(Ordering::SeqCst);
        if total == 0 {
            return;
        }
        let used = match docker::sample_faber_use(probe_image).await {
            Ok(used) => used,
            Err(e) => {
                debug!("co-tenant reserve: sample skipped: {e}");
                return;
            }
        };
        let Some(mib) = cotenant::cotenant_mib(total, used.available_mib, used.faber_mib) else {
            return;
        };
        self.observe_cotenant(mib, now_ms());
    }

    /// The co-tenant tick (ADR-0021 §3): sample what the VM uses beyond faber's
    /// containers every [`COTENANT_TICK`]. Sampled in every mode, so the
    /// dashboard shows the figure before a share makes it bind; re-planning on
    /// every sample also ages an expired peak out of the budget. Runs for the
    /// process life, on a host with a Docker profile.
    pub async fn cotenant_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(COTENANT_TICK).await;
            let probe_image = self.cfg.read().unwrap().probe_image.clone();
            self.sample_cotenant(&probe_image).await;
        }
    }

    /// Store one co-tenant sample taken at `at_ms`, persist the window if it
    /// changed, re-plan Gate 1, and log the reserve when it has moved.
    fn observe_cotenant(&self, mib: u64, at_ms: u64) {
        let floor = self.cfg.read().unwrap().reserve_mib;
        let (changed, snapshot, reserve) = {
            let mut machine = self.machine.lock().unwrap();
            let changed = machine.observe_cotenant(mib, at_ms);
            (
                changed,
                changed.then(|| machine.cotenant_peak().clone()),
                machine.reserve_mib(floor, at_ms),
            )
        };
        if let (true, Some(peak)) = (changed, snapshot) {
            if let Some(path) = self.cotenant_path.lock().unwrap().clone() {
                peak.save(&path);
            }
        }
        let logged = self.reserve_logged.load(Ordering::SeqCst);
        if reserve.abs_diff(logged) >= RESERVE_LOG_STEP_MIB {
            self.reserve_logged.store(reserve, Ordering::SeqCst);
            info!("autolimiter: {}", reserve_line(reserve, floor));
        }
        self.recompute_budget();
        self.wake.notify_waiters();
    }

    /// Set the disk admission floor (ADR-0024 §1) from `[host]
    /// disk_admission_floor_gib`. Called once at startup: like the rest of
    /// `[host]`, it applies on restart.
    pub fn set_disk_admission(&self, admission: DiskAdmission) {
        self.inner.lock().unwrap().disk.admission = admission;
        self.wake.notify_waiters();
    }

    /// The disk admission floor in force (ADR-0024 §1), as set at startup — and
    /// so whether the critical floor applies at all (ADR-0032 §2).
    pub fn disk_admission(&self) -> DiskAdmission {
        self.inner.lock().unwrap().disk.admission
    }

    /// The reading a Docker job is gated on, while it is still evidence at
    /// `now_ms` — what the critical floor decides on (ADR-0032 §2): the tighter of
    /// the host volume and the Linux VM's disk (#410). `None` before the first
    /// reading, and once the last good ones are too old to count
    /// ([`host::DISK_READING_MAX_AGE_MS`]).
    pub fn fresh_disk_reading(&self, now_ms: u64) -> Option<DiskReading> {
        let inner = self.inner.lock().unwrap();
        inner
            .disk
            .docker_reading(now_ms)
            .filter(|r| r.fresh_free_mib(now_ms).is_some())
    }

    /// Whether the latest fresh reading has the Docker volume under the admission
    /// floor, counting nothing for ramping jobs — revival's question (ADR-0024 §2).
    /// A restart kills whatever was ramping, so nothing is held for it, and it
    /// cannot make room, so it waits for some.
    ///
    /// The host volume only, never the Linux VM's disk (#410). What a restart
    /// must not boot onto is a full host volume, which the VM's disk file lives
    /// on. A full VM disk is room only Docker can make, by the janitor's prunes,
    /// so holding a restart for it would hold Docker down until a reading that
    /// needs Docker says otherwise. And a VM reading is taken only while Docker
    /// answers, so it is stale by the time revival asks.
    pub fn disk_short_of_floor(&self, now_ms: u64) -> Option<ShortOfRoom> {
        let inner = self.inner.lock().unwrap();
        host::short_of_room(inner.disk.admission, inner.disk.reading, 0, now_ms)
    }

    /// Store one free-space reading of the Docker volume, taken at `at_ms`, for
    /// the disk gate (ADR-0024 §1).
    ///
    /// A failed measurement stores nothing: the last good reading stands until it
    /// is too old to be evidence ([`host::DISK_READING_MAX_AGE_MS`]), and then the
    /// gate opens. One `df` that timed out must not let a parked queue through a
    /// gate that a reading ten seconds earlier had shut, and a measurement that has
    /// stopped altogether must not hold it shut forever.
    ///
    /// The crossing logged is the gate closing to *every* job: one whose charge is
    /// a light job's [`DISK_CHARGE_MIB`] (ADR-0032 §1). A dind job parks well
    /// before that, and says so on its own claim.
    fn observe_disk_free(&self, free_mib: Option<u64>, at_ms: u64) {
        let Some(free_mib) = free_mib else {
            return;
        };
        self.inner.lock().unwrap().disk.reading = Some(DiskReading {
            free_mib,
            at_ms,
            volume: crate::platform::host::docker_disk_volume(),
        });
        self.disk_gate_moved(at_ms);
    }

    /// Store what reading the Linux VM's disk came to (#410). A reading joins the
    /// host volume's, and the gate acts on the tighter. A failure stores nothing,
    /// as a failed `df` does, so the last good reading ages out and the gate falls
    /// back to the host volume alone; it is logged once, when it starts, and kept
    /// for the dashboard until a reading succeeds again.
    fn observe_vm_disk(&self, vm: host::VmDisk, at_ms: u64) {
        let (began, ended) = {
            let mut inner = self.inner.lock().unwrap();
            let disk = &mut inner.disk;
            match vm {
                host::VmDisk::Absent => return,
                host::VmDisk::Read(reading) => {
                    disk.vm = Some(reading);
                    (None, disk.vm_unreadable.take().is_some())
                }
                host::VmDisk::Unreadable(why) => {
                    let began = disk.vm_unreadable.is_none().then(|| why.clone());
                    disk.vm_unreadable = Some(why);
                    (began, false)
                }
            }
        };
        if let Some(why) = began {
            warn!(
                "autolimiter: cannot read free space on the Linux VM's disk ({}) — gating Docker \
                 jobs on the host volume alone until it can be read",
                crate::untrusted::for_log(&why, 200)
            );
        }
        if ended {
            info!("autolimiter: the Linux VM's disk can be read again");
        }
        self.disk_gate_moved(at_ms);
    }

    /// Log the disk gate closing to every job, or opening again, once per
    /// crossing, after either reading has moved.
    fn disk_gate_moved(&self, at_ms: u64) {
        let (refusal, floor_gib, crossed, reading) = {
            let mut inner = self.inner.lock().unwrap();
            let disk = &mut inner.disk;
            let refusal = disk.refusal(at_ms, DISK_CHARGE_MIB);
            let crossed = disk.closed != refusal.is_some();
            disk.closed = refusal.is_some();
            (
                refusal,
                disk.admission.floor_gib(),
                crossed,
                disk.docker_reading(at_ms),
            )
        };
        if crossed {
            match (&refusal, floor_gib, reading) {
                (Some(wait), _, _) => warn!(
                    "autolimiter: {} — starting no Docker job until there is room (ADR-0024)",
                    wait.view().detail
                ),
                (None, Some(floor), Some(reading)) => info!(
                    "autolimiter: {} GiB free on {}, room for a {} GiB charge above the {floor} \
                     GiB admission floor — resuming Docker admission",
                    host::fmt_gib(reading.free_mib),
                    reading.volume.describe(),
                    host::fmt_gib(DISK_CHARGE_MIB)
                ),
                (None, _, _) => {}
            }
        }
        // Parked claims re-check at once, so room that has come back is taken now
        // rather than at their next retry.
        self.wake.notify_waiters();
    }

    /// [`Self::try_admit_charged`] for a test that has no guard to hand the
    /// ledger entry to.
    #[cfg(test)]
    fn try_admit(&self, ticket: u64, reservation: impl Into<Reservation>) -> Result<(), Wait> {
        self.try_admit_charged(ticket, reservation).map(|_| ())
    }

    /// The serialized admission critical section: check every gate and, on pass,
    /// debit the ledger. Sync (no await while the lock is held) so it can't race.
    ///
    /// A refusal names the first gate that said no, with the numbers it said no
    /// on. The hold is checked first: it is the operator's word, and the gates
    /// below it are only the machine's. Pause comes next: a deliberate stop too,
    /// but a passing one, where the hold names why and lasts until released. Then
    /// the machine's own memory (ADR-0021 §2c), which holds macOS VM jobs too, and
    /// then the host itself — a live daemon, then room on its disk — before any of
    /// the ledgers.
    ///
    /// On a pass, returns the admission's entry in the disk ledger, which its
    /// guard gives back when its job ends (#89, #161), and the reservation as
    /// debited: its `mib` is the p75 reservation while `"p75"` is in force
    /// (ADR-0033 §1), the cap otherwise.
    fn try_admit_charged(
        &self,
        ticket: u64,
        reservation: impl Into<Reservation>,
    ) -> Result<(u64, Reservation), Wait> {
        let res = reservation.into();
        // Decided once, before any other lock: what the ledger debits. Gate 1 and
        // the head set-aside read it; Gate 2 still asks for room for the cap.
        let p75 = self.reserving_p75(now_ms());
        let need = debit_mib(&res, p75);
        if let Some(hold) = self.hold.lock().unwrap().as_ref() {
            return Err(Wait::Held {
                reason: hold.reason.clone(),
            });
        }
        if let Some(by) = self.current_pause() {
            return Err(Wait::Paused { by });
        }
        if self.grow_hold.load(Ordering::SeqCst) {
            return Err(Wait::Held {
                reason: crate::vm::grow::HOLD_REASON.to_string(),
            });
        }
        let (headroom, band, hard_ceiling, cpu) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.headroom_mib,
                cfg.band_mib,
                cfg.hard_ceiling.limit(),
                cfg.cpu,
            )
        };
        // Taken here, after `cfg` is let go (`snapshot` takes `cfg` then `inner`),
        // so the machine gate's floor reads the same count the admission below
        // adds to: two callers cannot both see an empty host.
        let mut inner = self.inner.lock().unwrap();
        // The machine before the daemon (ADR-0021 §2c): it stops CI adding work to
        // a machine that is already paging, whatever the VM's own gates say.
        //
        // Memory pressure keeps a floor of one Docker job (#160): the VM never
        // returns memory, so with nothing running the pressure cannot fall, and a
        // gate that held at zero would take the host out of the pool for good. Swap
        // has no floor: it clears on its own once nothing adds to it.
        if let Some(hold) = self.current_machine_hold() {
            let floor = matches!(hold, MachineHold::Pressure(_))
                && inner.docker_inflight + inner.vm.macos_running == 0;
            if !floor {
                return Err(self.machine_wait(hold));
            }
        }
        let budget = self.budget_mib.load(Ordering::SeqCst);
        let available = self.available_mib.load(Ordering::SeqCst);
        let cpu_budget = self.cpu_budget_mcpu.load(Ordering::SeqCst);
        // An unprobed VM has no CPU budget to speak of; memory still bounds it.
        let cpu_budget_known = self.ncpu.load(Ordering::SeqCst) > 0;
        let pressure = self.cpu_pressure();

        // Gate 0 — liveness. A dead Docker is indistinguishable from an idle one
        // to the gates below: `budget` keeps its last good value and `reserved_mib`
        // drops to zero as the dead jobs' guards release, so the ledger reads
        // "plenty of room" and Gate 2 is skipped for want of a probe. Admitting
        // there mints claims whose every `docker` call is doomed. Fail closed
        // instead: no daemon, no admission.
        if !self.docker_reachable() {
            return Err(Wait::DockerUnreachable);
        }

        // The disk gate — ADR-0024 §1, right behind liveness because it guards the
        // same thing: a VM that can keep running what it already has. The memory
        // gates below know nothing of disk. On the incident host they admitted
        // several jobs into a volume with little free, then another at even less,
        // and the jobs filled it until containerd's own writes failed. A reading
        // that is stale, failed or missing is no evidence, and the gate stays open.
        //
        // The job is charged its own disk as well as the window's (ADR-0032 §1): a
        // dind job admitted alone, with free space barely over the floor, once got
        // in on a charge of nothing and filled the disk inside minutes.
        let now = now_ms();
        // ADR-0061 §2: thirty minutes of refusals is one of the two triggers for
        // growing the Linux VM's disk; one admission restarts the clock.
        if let Some(refusal) = inner.disk.refusal(now, res.disk_mib) {
            crate::vm::grow::global().gate_refused(now);
            return Err(refusal);
        }
        crate::vm::grow::global().gate_passed();
        // The wall, once Docker runs in a VM behind it (ADR-0026): no admission
        // while its verdict is not green. Jobs already running finish.
        if let Some(refusal) = docker_wall_refusal(
            inner.vm.wall_required,
            &inner.vm.wall.at(inner.vm.wall_at_ms, now),
        ) {
            return Err(Wait::Vm(refusal));
        }

        if let Some(max) = hard_ceiling {
            if inner.docker_inflight >= max {
                return Err(Wait::Ceiling {
                    inflight: inner.docker_inflight,
                    max,
                });
            }
        }
        // Fairness set-aside: unless *we* are the longest-waiting caller, the head's
        // reservation is off-limits, so freed memory accumulates toward it instead
        // of being taken by whoever happens to fit. Only the head is protected, so
        // smaller jobs still fill whatever room is left beyond it.
        //
        // A head that cannot fit the whole budget is unsatisfiable and is *not* set
        // aside — protecting it would wedge the pool for everyone, forever. That
        // goes for its CPU too: it is never admitted, so CPU held for it is idle.
        //
        // The head's CPU is set aside clamped to the CPU budget, exactly as CPU Gate
        // 1 clamps its need — so a CPU-heavy head is protected until the ledger has
        // drained enough for it to run, alone if it must, and never wedges.
        //
        // The head is the oldest waiter of the highest tier (ADR-0036): the oldest
        // priority claim if any is parked, else the oldest claim not demoted as
        // likely wasted, else the oldest demoted one (ADR-0063 §5). With no
        // priority repos declared and nothing demoted that is the first entry,
        // exactly as before.
        let head = inner
            .priority_tickets
            .iter()
            .next()
            .and_then(|t| inner.waiters.get_key_value(t))
            .or_else(|| {
                inner
                    .waiters
                    .iter()
                    .find(|(t, _)| !inner.waste_tickets.contains(t))
            })
            .or_else(|| inner.waiters.iter().next());
        let head_is_priority = head.is_some_and(|(h, _)| inner.priority_tickets.contains(h));
        let setaside_priority = head_is_priority && !inner.priority_tickets.contains(&ticket);
        let (setaside_mib, setaside_mcpu) = match head {
            Some((&head, head_res)) if head != ticket && debit_mib(head_res, p75) <= budget => {
                (debit_mib(head_res, p75), head_res.mcpu.min(cpu_budget))
            }
            _ => (0, 0),
        };
        // Gate 0b — ADR-0011 §5. Until adoption has said what is already running,
        // the ledger below is not a claim about this host, it is an absence of one.
        if self.adoption_pending.load(Ordering::SeqCst) {
            return Err(Wait::Adoption);
        }
        // Gate 1 — reservation ledger. A zero budget means "not probed yet";
        // don't admit against an unknown ceiling.
        if budget == 0 || inner.reserved_mib + need + setaside_mib > budget {
            return Err(Wait::Memory {
                need_mib: need,
                reserved_mib: inner.reserved_mib,
                budget_mib: budget,
                setaside_mib,
                setaside_priority,
            });
        }
        // CPU Gate 1 — the same ledger in millicores (ADR-0019). A need larger than
        // the whole budget is clamped to it, so a class that wants more than the VM
        // has still runs — alone — rather than never. A VM whose reserve consumes
        // its entire budget therefore admits CPU work one job at a time.
        if cpu.mode.enabled() && cpu_budget_known {
            let need = res.mcpu.min(cpu_budget);
            if inner.reserved_mcpu + need + setaside_mcpu > cpu_budget {
                return Err(Wait::Cpu {
                    need_mcpu: need,
                    reserved_mcpu: inner.reserved_mcpu,
                    budget_mcpu: cpu_budget,
                    setaside_mcpu,
                    setaside_priority,
                });
            }
        }
        // Gate 2 — live pressure, with freeze hysteresis. Skipped entirely while
        // availability is unknown (probe failing) — Gate 1 still bounds us.
        if available != AVAILABLE_UNKNOWN {
            if available < headroom {
                inner.frozen = true;
            } else if available >= headroom + band {
                inner.frozen = false;
            }
            if inner.frozen || available < res.mib + headroom {
                return Err(Wait::VmMemory {
                    need_mib: res.mib,
                    available_mib: available,
                    headroom_mib: headroom,
                    resume_mib: headroom + band,
                    frozen: inner.frozen,
                });
            }
        }
        // CPU Gate 2 — VM CPU pressure (PSI `some avg60`), with its own hysteresis.
        // Sees the co-tenant stacks and opaque containers the ledger cannot name.
        // No reading (no PSI, probe failing) fails open, as Gate 2 does.
        if let Some(pct) = pressure {
            inner.cpu_frozen = cpu_hysteresis(inner.cpu_frozen, pct, &cpu);
            if cpu.mode.enabled() && inner.cpu_frozen {
                return Err(Wait::CpuPressure {
                    pressure: pct,
                    resume: cpu.resume_pressure,
                });
            }
        }
        inner.reserved_mib += need;
        inner.shadow_reserved_mib += res.shadow_mib;
        inner.reserved_mcpu += res.mcpu;
        inner.docker_inflight += 1;
        Ok((
            {
                let baseline = inner.disk.docker_reading(now);
                inner.disk.admitted(now, res.disk_mib, baseline)
            },
            Reservation { mib: need, ..res },
        ))
    }

    /// The runner `--cpus` quota (cores) for a CPU share of `mcpu`.
    ///
    /// None unless CPU admission is on. Never above the VM's core count: Docker
    /// refuses to start a container whose `--cpus` exceeds it, which would turn a
    /// pin or a profiling share larger than a small VM into an infra error on every
    /// launch. With the core count unknown there is nothing to bound it by, so no
    /// quota is applied at all — fail open, like both CPU gates.
    fn runner_quota(&self, cpu: &CpuConfig, mcpu: u64) -> f64 {
        let ncpu = self.ncpu.load(Ordering::SeqCst);
        if !cpu.mode.enabled() || ncpu == 0 {
            return 0.0;
        }
        cores(mcpu.min(ncpu.saturating_mul(1000)))
    }

    /// The last VM CPU-pressure reading, if there is one.
    fn cpu_pressure(&self) -> Option<f64> {
        match self.cpu_pressure.load(Ordering::SeqCst) {
            PRESSURE_UNKNOWN => None,
            hundredths => Some(hundredths as f64 / 100.0),
        }
    }

    /// Record one VM CPU-pressure reading and step the CPU Gate 2 hysteresis.
    ///
    /// Stepped here, on every reading, and not only when an admission happens to
    /// look: a spike that nobody was waiting through must still freeze the gate, or
    /// the next arrival would be admitted at a pressure between the two thresholds
    /// on the strength of a history it never saw. Tracked in every mode, so the
    /// dashboard can show what `auto` would do before it is on.
    fn observe_cpu_pressure(&self, some_avg60: Option<f64>) {
        let stored = match some_avg60 {
            Some(p) if p.is_finite() && p >= 0.0 => (p * 100.0).round() as u64,
            _ => PRESSURE_UNKNOWN,
        };
        self.cpu_pressure.store(stored, Ordering::SeqCst);
        let Some(pct) = self.cpu_pressure() else {
            return;
        };
        let cpu = self.cfg.read().unwrap().cpu;
        let mut inner = self.inner.lock().unwrap();
        let was = inner.cpu_frozen;
        inner.cpu_frozen = cpu_hysteresis(was, pct, &cpu);
        if cpu.mode.enabled() && inner.cpu_frozen != was {
            if inner.cpu_frozen {
                warn!(
                    "autolimiter: VM CPU pressure {pct:.1}% ≥ {}% — pausing admission until it \
                     falls to {}%",
                    cpu.pause_pressure, cpu.resume_pressure
                );
            } else {
                info!(
                    "autolimiter: VM CPU pressure {pct:.1}% ≤ {}% — resuming admission",
                    cpu.resume_pressure
                );
            }
        }
    }

    /// Fold one finished run into the class's learned caps and CPU shares. Pinned
    /// caps and pinned shares are each left untouched, independently. Surfaces a
    /// warning when a run escalated.
    pub fn record_run(&self, class: &str, label: &str, report: &RunReport) {
        let (margin, window, defaults, escalation_frac, cpu) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.margin,
                cfg.window,
                Shares::profiling(&cfg),
                cfg.escalation_frac,
                cfg.cpu,
            )
        };
        let profiling_cap = defaults.runner_mib;
        let budget = self.budget_mib.load(Ordering::SeqCst);
        let mut classes = self.classes.lock().unwrap();
        let entry = classes
            .entry(class.to_string())
            .or_insert_with(|| ClassCap::profiling(label, defaults));
        entry.label = label.to_string();

        // CPU first: the memory pin's early return below must not freeze the CPU
        // shares as well. Learned in every CPU mode, so turning `auto` on starts
        // from history rather than from a profiling storm.
        if !entry.cpu_pinned {
            let runner = cpu_sample(
                report.runner_cpu,
                report.applied_runner_mcpu,
                entry.runner_mcpu,
            );
            let dind = cpu_sample(report.dind_cpu, report.applied_dind_mcpu, entry.dind_mcpu);
            if let Some(msg) = learn_cpu(entry, runner, dind, window, margin, cpu_ceiling(&cpu)) {
                // A held share only holds anything once CPU admission is on; before
                // that it would bury the memory warnings under advice with no effect.
                // The class still reads `warned`, so the dashboard shows what `auto`
                // would hold.
                if cpu.mode.enabled() {
                    self.push_warning(class, WarnKind::Cpu, msg);
                } else {
                    self.clear_warning(class, WarnKind::Cpu);
                }
            } else if entry.cpu_status != CapStatus::Warned {
                self.clear_warning(class, WarnKind::Cpu);
            }
            // Same reasoning as the dind memory cap below: no share for a container
            // that does not exist.
            if report.dind == Some(false) {
                entry.dind_mcpu = 0;
            }
        }

        if entry.pinned {
            return; // operator owns this cap
        }

        // An OOM is a censored sample: the true peak is at least the cap, so feed
        // the current cap (bumped) as the sample for the offending container.
        //
        // The cap to censor at is the one the container *actually ran under*, which
        // cohort sizing can raise above this class's own (see
        // [`Limiter::sizing_for`]). Censoring at the class cap instead would
        // under-bump a class killed at a much higher ceiling — it would "learn" a
        // value below the peak it had already demonstrably exceeded, and OOM again.
        let runner_cap = entry.runner_mib.max(report.applied_runner_mib);
        let dind_cap = entry.dind_mib.max(report.applied_dind_mib);
        let runner_sample = sample_for(report.runner_peak_mib, report.oom_runner, runner_cap);
        let dind_sample = sample_for(report.dind_peak_mib, report.oom_dind, dind_cap);
        // ADR-0033 rollout step 1: judge the run against the p75 reservation the
        // class held *before* it, then fold it in. Measurement only.
        judge_p75(
            entry,
            (report.runner_peak_mib, report.oom_runner),
            (report.dind_peak_mib, report.oom_dind),
        );
        if let Some(s) = runner_sample {
            push_sample(
                &mut entry.peaks_runner,
                &mut entry.censored_runner,
                s,
                report.oom_runner,
                window,
            );
        }
        if let Some(s) = dind_sample {
            push_sample(
                &mut entry.peaks_dind,
                &mut entry.censored_dind,
                s,
                report.oom_dind,
                window,
            );
        }

        // rolling-max × margin gives raise-fast (a new high lifts the max at once)
        // and lower-slow (the max only drops once every high sample ages out).
        let want_runner = cap_from(&entry.peaks_runner, margin);
        let want_dind = cap_from(&entry.peaks_dind, margin);

        // Escalation: a class wanting more than a budget fraction is held at the
        // profiling ceiling pending a pin, and warned — don't silently feed it.
        match apply_want(
            entry,
            want_runner,
            want_dind,
            profiling_cap,
            budget,
            escalation_frac,
        ) {
            Some(msg) => self.push_warning(class, WarnKind::Memory, msg),
            None => {
                self.clear_warning(class, WarnKind::Memory);
                if report.oom_runner || report.oom_dind {
                    info!(
                        "{label}: OOM force-bumped cap to runner={} dind={} MiB",
                        entry.runner_mib, entry.dind_mib
                    );
                }
            }
        }
        // A job that ran without a daemon has no dind cap to hold, and
        // `apply_want` would otherwise floor it at `MIN_CAP_MIB` off a sample
        // window that will never fill. The stored value no longer decides anything
        // — every read goes through `without_dind` — but the panel shows it, and a
        // cap for a container that does not exist is a lie in the one view an
        // operator uses to reason about the budget.
        //
        // Keyed on the report's own `dind` fact rather than on a zero
        // `applied_dind_mib`: a run that never reached a container reports zero
        // ceilings too, and that is not the same claim.
        if report.dind == Some(false) {
            entry.dind_mib = 0;
        }
        refresh_shadow(entry, margin, window);
        // A shrunk cap frees ledger room — nudge waiters.
        self.wake.notify_waiters();
    }

    /// Log a warning and keep it for the panel, replacing the class's earlier
    /// warning of the same kind: a class held run after run is one warning, not
    /// a list that buries every other class. The log gets it once per hold, and
    /// hourly while it stands, not once per run: its figures move with every run,
    /// so only a change in its wording (a learned hold turning into an OOM one)
    /// logs it again, and the panel carries the current text.
    fn push_warning(&self, class: &str, kind: WarnKind, msg: String) {
        let wording = crate::logline::wording(&msg);
        if crate::logline::repeats().fresh(
            &hold_key(class, kind),
            &wording,
            std::time::Instant::now(),
        ) {
            warn!("{msg}");
        }
        let mut w = self.warnings.lock().unwrap();
        w.retain(|((c, k), _)| !(c == class && *k == kind));
        w.push_back(((class.to_string(), kind), msg));
        while w.len() > WARN_CAP {
            w.pop_front();
        }
    }

    /// Drop a class's warning of this kind: the learner or a pin no longer holds it.
    fn clear_warning(&self, class: &str, kind: WarnKind) {
        crate::logline::repeats().cleared(&hold_key(class, kind));
        self.warnings
            .lock()
            .unwrap()
            .retain(|((c, k), _)| !(c == class && *k == kind));
    }

    /// Pin a class to an operator-set cap the learner won't touch.
    ///
    /// Memory is always pinned. CPU is pinned only when `limits.runner.cpus` is
    /// set: every pin written before ADR-0019 carries `0.0` there, and must keep
    /// meaning exactly what it meant — memory only, CPU still learned. A zero
    /// therefore leaves the CPU side as it is rather than clearing a CPU pin, so a
    /// client that knows nothing about CPU cannot silently withdraw one; `unpin`
    /// is what releases both.
    pub fn pin(&self, class: &str, limits: Limits) {
        {
            let (defaults, margin, window) = {
                let cfg = self.cfg.read().unwrap();
                (Shares::profiling(&cfg), cfg.margin, cfg.window)
            };
            let mut classes = self.classes.lock().unwrap();
            let entry = classes
                .entry(class.to_string())
                .or_insert_with(|| ClassCap::profiling(class, defaults));
            entry.runner_mib = limits.runner.memory_mib.max(MIN_CAP_MIB);
            entry.dind_mib = limits.dind.memory_mib.max(MIN_CAP_MIB);
            entry.pinned = true;
            entry.status = CapStatus::Pinned;
            entry.oom_held = false;
            pin_cpu(entry, &limits);
            refresh_shadow(entry, margin, window);
            self.clear_warning(class, WarnKind::Memory);
            if entry.cpu_pinned {
                self.clear_warning(class, WarnKind::Cpu);
            }
        }
        self.wake.notify_waiters();
    }

    /// Un-pin a class, returning its caps — and its CPU shares, if pinned — to
    /// learned control.
    pub fn unpin(&self, class: &str) {
        let (margin, window) = {
            let cfg = self.cfg.read().unwrap();
            (cfg.margin, cfg.window)
        };
        if let Some(entry) = self.classes.lock().unwrap().get_mut(class) {
            entry.pinned = false;
            entry.status = CapStatus::Learned;
            if entry.cpu_pinned {
                entry.cpu_pinned = false;
                entry.cpu_status = CapStatus::Learned;
            }
            refresh_shadow(entry, margin, window);
        }
    }

    /// The pinned caps, for persistence to the overrides file. `cpus` carries a
    /// class's pinned CPU shares, and stays `0.0` — "not pinned" — for a class
    /// whose CPU is learned.
    pub fn pinned_caps(&self) -> HashMap<String, Limits> {
        self.classes
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, c)| c.pinned)
            .map(|(k, c)| {
                let pinned_cores = |mcpu: u64| if c.cpu_pinned { cores(mcpu) } else { 0.0 };
                let mut limits =
                    container_limits(c.runner_mib, c.dind_mib, pinned_cores(c.runner_mcpu));
                limits.dind.cpus = pinned_cores(c.dind_mcpu);
                (k.clone(), limits)
            })
            .collect()
    }

    /// Seed a class's rolling window from replayed audit history (startup only,
    /// so learned caps survive a restart without a re-profiling storm). Skips
    /// classes already pinned.
    pub fn seed(&self, class: &str, label: &str, runner_peak: Option<u64>, dind_peak: Option<u64>) {
        self.seed_censored(class, label, (runner_peak, false), (dind_peak, false));
    }

    /// [`Self::seed`], told which replayed samples were censored by an OOM
    /// (ADR-0033 §1: without it a restart would forget the OOM and drop straight
    /// to p75). The peaks themselves replay raw, exactly as before.
    pub fn seed_censored(
        &self,
        class: &str,
        label: &str,
        (runner_peak, runner_oom): (Option<u64>, bool),
        (dind_peak, dind_oom): (Option<u64>, bool),
    ) {
        let (margin, window, defaults, escalation_frac) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.margin,
                cfg.window,
                Shares::profiling(&cfg),
                cfg.escalation_frac,
            )
        };
        let profiling_cap = defaults.runner_mib;
        let budget = self.budget_mib.load(Ordering::SeqCst);
        let mut classes = self.classes.lock().unwrap();
        let entry = classes
            .entry(class.to_string())
            .or_insert_with(|| ClassCap::profiling(label, defaults));
        if entry.pinned {
            return;
        }
        judge_p75(entry, (runner_peak, runner_oom), (dind_peak, dind_oom));
        if let Some(r) = runner_peak {
            push_sample(
                &mut entry.peaks_runner,
                &mut entry.censored_runner,
                r,
                runner_oom,
                window,
            );
        }
        if let Some(d) = dind_peak {
            push_sample(
                &mut entry.peaks_dind,
                &mut entry.censored_dind,
                d,
                dind_oom,
                window,
            );
        }
        if entry.peaks_runner.is_empty() && entry.peaks_dind.is_empty() {
            return; // nothing replayed for either container — stay Profiling
        }
        // Through the same guard a live run gets: replayed history must not be able
        // to seed a cap the escalation guard would never grant. Seeding runs before
        // the first budget probe, so the escalation branch is usually inert here
        // (`budget == 0`) and the ceiling clamp is what bounds the replay; the
        // first measured budget then re-evaluates every class ([`Self::relearn_all`]).
        relearn(
            entry,
            margin,
            window,
            profiling_cap,
            budget,
            escalation_frac,
        );
    }

    /// Re-run every learned class's window through [`apply_want`] against the
    /// budget now in force. Called once, when the budget is first measured: the
    /// startup replay ran with none, so it could neither grow an OOM-proven cap
    /// past the profiling ceiling (ADR-0053) nor judge the escalation threshold,
    /// and without this the first live run of every such class would be killed
    /// again at the ceiling before it learned anything.
    fn relearn_all(&self) {
        let (margin, window, profiling_cap, escalation_frac) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.margin,
                cfg.window,
                cfg.profiling_cap_mib,
                cfg.escalation_frac,
            )
        };
        let budget = self.budget_mib.load(Ordering::SeqCst);
        for entry in self.classes.lock().unwrap().values_mut() {
            if entry.pinned || (entry.peaks_runner.is_empty() && entry.peaks_dind.is_empty()) {
                continue;
            }
            relearn(
                entry,
                margin,
                window,
                profiling_cap,
                budget,
                escalation_frac,
            );
        }
        self.wake.notify_waiters();
    }

    /// Seed a class's CPU windows from one replayed outcome (startup only), through
    /// the same censoring rule a live run gets — so a sample that was throttled at
    /// its quota replays as "at least this much" rather than as the peak the quota
    /// forced. Skips classes whose CPU is pinned; memory pinning does not matter.
    ///
    /// `applied_runner_mcpu` is the runner quota the run recorded (`0` = none).
    /// dind is never under a quota, so its samples are never censored.
    pub fn seed_cpu(
        &self,
        class: &str,
        label: &str,
        runner: Option<CpuUsage>,
        applied_runner_mcpu: u64,
        dind: Option<CpuUsage>,
    ) {
        let (margin, window, defaults, ceiling) = {
            let cfg = self.cfg.read().unwrap();
            (
                cfg.margin,
                cfg.window,
                Shares::profiling(&cfg),
                cpu_ceiling(&cfg.cpu),
            )
        };
        let mut classes = self.classes.lock().unwrap();
        let entry = classes
            .entry(class.to_string())
            .or_insert_with(|| ClassCap::profiling(label, defaults));
        if entry.cpu_pinned {
            return;
        }
        let runner = cpu_sample(runner, applied_runner_mcpu, entry.runner_mcpu);
        let dind = cpu_sample(dind, 0, entry.dind_mcpu);
        // Warnings are for live runs; a replay only restores the state they left.
        learn_cpu(entry, runner, dind, window, margin, ceiling);
    }

    // --- live tuning + probes ---------------------------------------------

    /// Apply a live tuning change to the autolimiter knobs and recompute the
    /// budget. Any `None` leaves that knob unchanged.
    pub fn set_tuning(
        &self,
        reserve_mib: Option<u64>,
        headroom_mib: Option<u64>,
        margin: Option<f64>,
        native_concurrency: Option<usize>,
        hard_ceiling: Option<HardCeiling>,
    ) {
        // The margin moves the would-be p75 reservations with it.
        let mut refresh: Option<(f64, usize)> = None;
        {
            let mut cfg = self.cfg.write().unwrap();
            if let Some(r) = reserve_mib {
                cfg.reserve_mib = r;
            }
            if let Some(h) = headroom_mib {
                cfg.headroom_mib = h;
            }
            if let Some(m) = margin {
                cfg.margin = m;
                refresh = Some((m, cfg.window));
            }
            if let Some(n) = native_concurrency {
                cfg.native_concurrency = n;
            }
            if let Some(c) = hard_ceiling {
                cfg.hard_ceiling = c;
            }
        }
        if let Some((margin, window)) = refresh {
            for entry in self.classes.lock().unwrap().values_mut() {
                refresh_shadow(entry, margin, window);
            }
        }
        self.recompute_budget();
        self.wake.notify_waiters();
    }

    fn recompute_budget(&self) {
        let (floor, cpu) = {
            let cfg = self.cfg.read().unwrap();
            (cfg.reserve_mib, cfg.cpu)
        };
        // Planned within the machine's share, where one is declared (ADR-0021
        // §2b): the smaller of the VM and what the share leaves CI. The reserve is
        // measured on the same terms (§3): `reserve_mib`, or the co-tenants'
        // 24-hour peak if that is larger.
        let (total, ncpu, reserve) = {
            let machine = self.machine.lock().unwrap();
            (
                machine.plan_mem_mib(self.mem_total_mib.load(Ordering::SeqCst)),
                machine.plan_ncpu(self.ncpu.load(Ordering::SeqCst)),
                machine.reserve_mib(floor, now_ms()),
            )
        };
        // A budget of zero means "not measured yet" to Gate 1. A measured VM whose
        // reserve takes all of it gets one MiB instead, so a parked claim says the
        // budget is too small rather than that nobody has measured it.
        let budget = match total {
            0 => 0,
            total => total.saturating_sub(reserve).max(1),
        };
        let was = self.budget_mib.swap(budget, Ordering::SeqCst);
        self.cpu_budget_mcpu
            .store(cpu_budget_mcpu(ncpu, &cpu), Ordering::SeqCst);
        if was == 0 && budget > 0 {
            self.relearn_all();
        }
    }

    /// Probe the VM total and core count and set the initial budgets. Call once at
    /// startup before admitting, so Gate 1 has a real ceiling.
    pub async fn prime_budget(&self) {
        match docker::probe_mem_total_mib().await {
            Ok(total) => {
                self.mem_total_mib.store(total, Ordering::SeqCst);
                self.recompute_budget();
                let planned = self.machine.lock().unwrap().plan_mem_mib(total);
                let floor = self.cfg.read().unwrap().reserve_mib;
                let reserve = self.machine.lock().unwrap().reserve_mib(floor, now_ms());
                info!(
                    "autolimiter: budget {} MiB (VM {}{} − reserve {}{})",
                    self.budget_mib.load(Ordering::SeqCst),
                    total,
                    if planned < total {
                        format!(", planned within the machine's share at {planned}")
                    } else {
                        String::new()
                    },
                    reserve,
                    if reserve > floor {
                        format!(", the co-tenants' 24 h peak over reserve_mib {floor}")
                    } else {
                        String::new()
                    }
                );
            }
            Err(e) => warn!("autolimiter: initial VM-memory probe failed: {e}"),
        }
        match docker::probe_ncpu().await {
            Ok(ncpu) => {
                self.ncpu.store(ncpu, Ordering::SeqCst);
                self.recompute_budget();
                let cpu = self.cfg.read().unwrap().cpu;
                let planned = self.machine.lock().unwrap().plan_ncpu(ncpu);
                let cores = if planned < ncpu {
                    format!("{planned} of the VM's {ncpu}, within the machine's share,")
                } else {
                    format!("VM {ncpu}")
                };
                info!(
                    "autolimiter: CPU budget {} cores ({cores} × {} − reserve {}), CPU admission {}",
                    fmt_cores(self.cpu_budget_mcpu.load(Ordering::SeqCst)),
                    cpu.overcommit,
                    cpu.reserve_cpus,
                    cpu.mode.as_str()
                );
            }
            Err(e) => warn!("autolimiter: initial VM CPU-count probe failed: {e}"),
        }
    }

    /// The background memory-tick: re-probe the VM's pressure (and, less often, its
    /// total and core count), then wake waiters so both Gate 2s re-check. Runs for
    /// the process life.
    pub async fn probe_loop(self: Arc<Self>) {
        let mut tick: u32 = 0;
        loop {
            tokio::time::sleep(MEM_TICK).await;
            let probe_image = self.cfg.read().unwrap().probe_image.clone();
            match docker::probe_vm_pressure(&probe_image).await {
                Ok(pressure) => {
                    self.available_mib
                        .store(pressure.mem_available_mib, Ordering::SeqCst);
                    self.observe_memory(Some(pressure.mem_available_mib), now_ms());
                    // A kernel without PSI answers the memory half and not this
                    // one: CPU Gate 2 then fails open, and liveness is unaffected.
                    self.observe_cpu_pressure(pressure.cpu_some_avg60);
                    // One good probe is proof of life: reopen Gate 0 immediately
                    // rather than making a recovered daemon serve out a penalty.
                    if self.probe_misses.swap(0, Ordering::SeqCst) >= UNHEALTHY_AFTER {
                        info!("autolimiter: Docker reachable again — resuming admission");
                    }
                    crate::logline::repeats().cleared(PRESSURE_PROBE_KEY);
                }
                Err(e) => {
                    // Every tick while Docker is down: once per error, and hourly.
                    if crate::logline::repeats().fresh(
                        PRESSURE_PROBE_KEY,
                        &e.to_string(),
                        std::time::Instant::now(),
                    ) {
                        warn!("autolimiter: VM pressure probe failed: {e}");
                    }
                    self.available_mib
                        .store(AVAILABLE_UNKNOWN, Ordering::SeqCst);
                    self.observe_cpu_pressure(None);
                    // ADR-0033 §3: under real pressure the probe is what fails, so
                    // a failure right after a low reading trips the fallback.
                    self.observe_memory(None, now_ms());
                    // Only the crossing logs, so a long outage doesn't spam.
                    let misses = self.probe_misses.fetch_add(1, Ordering::SeqCst) + 1;
                    if misses == UNHEALTHY_AFTER {
                        warn!(
                            "autolimiter: Docker unreachable after {misses} probes — \
                             refusing admission until it answers"
                        );
                    }
                }
            }
            tick = tick.wrapping_add(1);
            let periodic = tick.is_multiple_of(TOTAL_EVERY);
            // Re-probe the VM total periodically — or every tick until the budget
            // is first established, so a failed startup probe recovers in seconds
            // (not a full TOTAL_EVERY cycle) once Docker is up.
            if periodic || self.budget_mib.load(Ordering::SeqCst) == 0 {
                if let Ok(total) = docker::probe_mem_total_mib().await {
                    self.mem_total_mib.store(total, Ordering::SeqCst);
                    self.recompute_budget();
                }
            }
            // The core count on the same terms, so a VM CPU resize is
            // tracked without a restart.
            if periodic || self.ncpu.load(Ordering::SeqCst) == 0 {
                if let Ok(ncpu) = docker::probe_ncpu().await {
                    self.ncpu.store(ncpu, Ordering::SeqCst);
                    self.recompute_budget();
                }
            }
            // ADR-0021 §2a: a VM larger than the machine's share is checked on every
            // re-probe, not only at boot, since a VM resize is tracked without a
            // restart — but warned about only when it changes, and hourly while it
            // stands (`logline::Repeats`), and said once when it clears.
            if periodic {
                let macos_mib = self.macos_allowance_mib();
                let found = self.machine.lock().unwrap().encroachment(
                    self.mem_total_mib.load(Ordering::SeqCst),
                    self.ncpu.load(Ordering::SeqCst),
                    macos_mib,
                );
                let repeats = crate::logline::repeats();
                if found.is_empty() {
                    if repeats.cleared(ENCROACHMENT_KEY) {
                        info!("machine: faber's VMs fit the machine share again");
                    }
                } else if repeats.fresh(
                    ENCROACHMENT_KEY,
                    &found.join("\n"),
                    std::time::Instant::now(),
                ) {
                    for msg in found {
                        warn!("machine: {msg}");
                    }
                }
            }
            self.wake.notify_waiters();
        }
    }

    /// The disk tick (ADR-0024 §1): read free space on the volume backing Docker
    /// every [`DISK_TICK`] and store it for the disk gate, which may do no I/O of
    /// its own. Runs for the process life.
    ///
    /// Deliberately apart from every Docker call, the probe loop's included. The
    /// reading matters most once Docker has stopped answering, because it is also
    /// what holds a restart back from a full disk (ADR-0024 §2). So the volume's
    /// path is resolved once and kept — resolving it asks `docker info` on Linux —
    /// and resolved again only after a measurement fails, in case Docker's
    /// data-root moved. A resolution that fails keeps the path already known.
    ///
    /// On a host whose Docker runs in faber's Linux VM, the VM's own disk is read
    /// too, after the host volume and only while Docker answers (#410): it is the
    /// one Docker jobs fill, and the Mac's `df` cannot see into it.
    pub async fn disk_loop(self: Arc<Self>) {
        let mut path: Option<String> = None;
        let mut resolve = true;
        loop {
            if resolve {
                if let Some(found) = host::docker_data_path().await {
                    path = Some(found);
                }
            }
            let free = match &path {
                Some(p) => host::disk_free_mib(p).await,
                None => None,
            };
            resolve = free.is_none();
            self.observe_disk_free(free, now_ms());
            // The Linux VM's disk, where there is one (#410), after the host
            // volume so a slow container start never delays that reading. Only
            // while Docker answers: a probe of a daemon known to be down would
            // only time out, and the last reading ages out meanwhile.
            if crate::platform::host::has_linux_vm() && self.docker_reachable() {
                let probe_image = self.cfg.read().unwrap().probe_image.clone();
                let vm = host::read_linux_vm_disk(&probe_image).await;
                self.observe_vm_disk(vm, now_ms());
            }
            tokio::time::sleep(DISK_TICK).await;
        }
    }

    /// A snapshot for the dashboard.
    pub fn snapshot(&self) -> LimiterView {
        // Read on its own, before the other locks: `try_admit` takes this one
        // alone and lets it go before it takes `inner`.
        let hold = self.current_hold();
        let paused = self.current_pause().is_some();
        let machine_hold = self.current_machine_hold();
        let adoption_pending = self.adoption_pending.load(Ordering::SeqCst);
        let warnings: Vec<WarningItem> = self
            .warnings
            .lock()
            .unwrap()
            .iter()
            .rev()
            .map(|((class, kind), text)| WarningItem {
                key: format!(
                    "{}:{class}",
                    match kind {
                        WarnKind::Memory => "memory",
                        WarnKind::Cpu => "cpu",
                    }
                ),
                text: text.clone(),
            })
            .collect();
        let docker_reachable = self.docker_reachable();
        // Before `inner` is taken: it locks `machine` and then `inner` itself.
        let vm = self.vm_view();
        let floor = self.cfg.read().unwrap().reserve_mib;
        let macos_mib = self.macos_allowance_mib();
        let (machine, cotenant) = {
            let m = self.machine.lock().unwrap();
            let now = now_ms();
            (
                m.view(
                    now,
                    self.mem_total_mib.load(Ordering::SeqCst),
                    self.ncpu.load(Ordering::SeqCst),
                    macos_mib,
                ),
                m.cotenant_view(floor, now),
            )
        };
        let gates = self.gates.snapshot();
        let reserve = {
            let (mode, unknown) = {
                let cfg = self.cfg.read().unwrap();
                (
                    cfg.reserve.mode(),
                    cfg.reserve.unknown().map(str::to_string),
                )
            };
            self.fallback.lock().unwrap().view(mode, unknown, now_ms())
        };
        let cfg = self.cfg.read().unwrap();
        let inner = self.inner.lock().unwrap();
        let available = self.available_mib.load(Ordering::SeqCst);
        let ncpu = self.ncpu.load(Ordering::SeqCst);
        let pressure = self.cpu_pressure();
        let (mut classes, mut oom_held): (Vec<ClassCapView>, Vec<OomHeldView>) = {
            let all = self.classes.lock().unwrap();
            let held = all
                .iter()
                .filter_map(|(k, c)| oom_held_view(k, c))
                .collect();
            (
                all.iter().map(|(key, c)| class_view(key, c)).collect(),
                held,
            )
        };
        oom_held.sort_by(|a, b| b.ooms.cmp(&a.ooms).then_with(|| a.label.cmp(&b.label)));
        classes.sort_by_key(|c| std::cmp::Reverse(c.reservation()));
        let budget_mib = self.budget_mib.load(Ordering::SeqCst);
        let available_mib = (available != AVAILABLE_UNKNOWN).then_some(available);
        let budget_mcpu = self.cpu_budget_mcpu.load(Ordering::SeqCst);
        let disk = inner
            .disk
            .view(now_ms(), inner.waiters.values().map(|r| r.disk_mib));
        // Computed from values already in hand: `reservation_for` would take `cfg`
        // and `classes` again while they are held here.
        let unknown = Shares::profiling(&cfg);
        // The ledger holds p75 debits while p75 is in force, so the typical job
        // must be sized in the same unit.
        let p75 = reserve.in_force == "p75";
        let headroom = headroom(&HeadroomInputs {
            budget_mib,
            reserved_mib: inner.reserved_mib,
            available_mib,
            headroom_mib: cfg.headroom_mib,
            budget_mcpu,
            reserved_mcpu: inner.reserved_mcpu,
            cpu_auto: cfg.cpu.mode.enabled(),
            docker_inflight: inner.docker_inflight,
            ceiling: cfg.hard_ceiling.limit(),
            disk: &disk,
            class_sizes: classes
                .iter()
                .map(|c| (c.debit(p75), c.runner_mcpu + c.dind_mcpu))
                .collect(),
            unknown_size: (
                unknown.runner_mib + unknown.dind_mib,
                unknown.runner_mcpu + unknown.dind_mcpu,
            ),
            shut: if hold.is_some() {
                Some("held")
            } else if paused {
                Some("paused")
            } else if let Some(m) = machine_hold {
                Some(m.view().0)
            } else if !docker_reachable {
                Some("docker")
            } else if disk.closed {
                Some("disk")
            } else if adoption_pending {
                Some("adoption")
            } else if inner.frozen && available_mib.is_some() {
                Some("vm-memory")
            } else if cfg.cpu.mode.enabled() && inner.cpu_frozen && pressure.is_some() {
                Some("cpu-pressure")
            } else {
                None
            },
        });
        LimiterView {
            budget_mib,
            available_mib,
            docker_reachable,
            reserved_mib: inner.reserved_mib,
            reserved_p75_mib: inner.shadow_reserved_mib,
            reserve,
            docker_inflight: inner.docker_inflight,
            frozen: inner.frozen,
            reserve_mib: cfg.reserve_mib,
            headroom_mib: cfg.headroom_mib,
            margin: cfg.margin,
            native_concurrency: cfg.native_concurrency,
            hard_ceiling: cfg.hard_ceiling.limit(),
            hard_ceiling_setting: cfg.hard_ceiling,
            hold,
            cpu: CpuView {
                mode: cfg.cpu.mode.as_str(),
                ncpu: (ncpu > 0).then_some(ncpu),
                budget_mcpu,
                reserved_mcpu: inner.reserved_mcpu,
                pressure,
                // Without a reading the gate fails open, so a stale flag must not
                // read as a pause.
                frozen: inner.cpu_frozen && pressure.is_some(),
                overcommit: cfg.cpu.overcommit,
                reserve_cpus: cfg.cpu.reserve_cpus,
                profiling_cpus: cfg.cpu.profiling_cpus,
            },
            disk,
            vm,
            machine,
            cotenant,
            gates,
            headroom,
            classes,
            warnings: warnings.iter().map(|w| w.text.clone()).collect(),
            warning_items: warnings,
            oom_held,
        }
    }
}

/// The co-tenant reserve in force, as the log says it (ADR-0021 §3).
fn reserve_line(reserve: u64, floor: u64) -> String {
    if reserve > floor {
        format!(
            "Gate 1 holds back {} GiB, the peak the Docker VM's other tenants used in the \
             last 24 h, rather than reserve_mib's {} GiB (ADR-0021 §3)",
            host::fmt_gib(reserve),
            host::fmt_gib(floor)
        )
    } else {
        format!(
            "Gate 1 holds back reserve_mib's {} GiB: the Docker VM's other tenants used no \
             more in the last 24 h (ADR-0021 §3)",
            host::fmt_gib(floor)
        )
    }
}

/// Write a class's freshly computed want into its caps. Returns the warning to
/// surface when the want exceeds the escalation threshold.
///
/// A container with no OOM in its window is held at the profiling ceiling. The
/// ceiling is a **clamp, never a floor**: each container gets
/// `min(want, ceiling)`, so holding a class back can only ever *lower* its
/// reservation. Assigning the ceiling to both containers instead — the original
/// behavior — inflated the very classes the guard flagged. With a 4096 MiB
/// ceiling and a 12288 MiB budget, say, a dind that had never exceeded about
/// 600 MiB across a thousand runs was handed the whole ceiling, so a flagged
/// class reserved 8192 MiB: more than the want that tripped the guard (7168 MiB,
/// say), enough that only one such job could hold the pool at a time, and every
/// other claim parked behind it.
///
/// A container whose window holds an OOM (a censored sample), or a measured peak
/// above the profiling ceiling, has shown it needs more than the ceiling gives,
/// so the ceiling cannot be its limit: held
/// there, it is killed on every run (ADR-0053). Its ceiling is the escalation
/// threshold instead, less the other container's cap, so the class's whole
/// reservation stays within the threshold; it never drops below the profiling
/// ceiling. Past the threshold the class is held and `Warned`, and
/// [`ClassCap::oom_held`] puts it to the owner. With no budget measured yet there
/// is no threshold to grow to, so the profiling ceiling holds until there is one
/// ([`Limiter::relearn_all`]).
fn apply_want(
    entry: &mut ClassCap,
    want_runner: u64,
    want_dind: u64,
    profiling_cap: u64,
    budget: u64,
    escalation_frac: f64,
) -> Option<String> {
    let threshold = (budget as f64 * escalation_frac) as u64;
    // Proof the ceiling is too low: an OOM in the window, or a measured peak above
    // the ceiling — reachable only under a cap already grown past it. Without the
    // second, a grown cap would fall back to the ceiling once its OOMs aged out of
    // the window, and the class would OOM again once per window, forever. A want
    // above the ceiling built from peaks *below* it is neither, and still clamps.
    let proven = |peaks: &VecDeque<u64>, censored: &VecDeque<bool>| {
        censored.iter().any(|c| *c) || peaks.iter().any(|p| *p > profiling_cap)
    };
    let oom_runner = proven(&entry.peaks_runner, &entry.censored_runner);
    let oom_dind = proven(&entry.peaks_dind, &entry.censored_dind);
    // An OOM-proven container grows into `room`, never below the profiling clamp.
    let grow = |want: u64, room: u64| want.min(room.max(profiling_cap)).max(MIN_CAP_MIB);
    let mut runner = want_runner.min(profiling_cap).max(MIN_CAP_MIB);
    let mut dind = want_dind.min(profiling_cap).max(MIN_CAP_MIB);
    if budget > 0 {
        match (oom_runner, oom_dind) {
            (true, false) => runner = grow(want_runner, threshold.saturating_sub(dind)),
            (false, true) => dind = grow(want_dind, threshold.saturating_sub(runner)),
            (true, true) => {
                // Both proven: the threshold is shared in proportion to the wants.
                let sum = want_runner + want_dind;
                let share = if sum <= threshold {
                    want_runner
                } else {
                    (u128::from(threshold) * u128::from(want_runner) / u128::from(sum.max(1)))
                        as u64
                };
                runner = grow(want_runner, share);
                dind = grow(want_dind, threshold.saturating_sub(runner));
            }
            (false, false) => {}
        }
    }
    entry.runner_mib = runner;
    entry.dind_mib = dind;
    entry.oom_held = false;
    if budget > 0 && want_runner + want_dind > threshold {
        entry.status = CapStatus::Warned;
        // A proven container held below what it has shown it needs: the job keeps
        // failing until the owner pins it higher or makes it lighter. Whether to
        // ask the owner is `oom_held_view`'s call, on recent OOMs only.
        if (oom_runner && runner < want_runner) || (oom_dind && dind < want_dind) {
            entry.oom_held = true;
            entry.held_pin = held_pin(
                (runner, dind),
                (oom_runner, oom_dind),
                (want_runner, want_dind),
                budget,
            );
            return Some(format!(
                "{}: outgrew the profiling ceiling (OOM-killed, or peaked past it) and wants {} MiB, more than {:.0}% of budget ({threshold} MiB) — held at {} MiB (runner {} + dind {}); pin it higher or make the job lighter",
                entry.label,
                want_runner + want_dind,
                escalation_frac * 100.0,
                runner + dind,
                runner,
                dind,
            ));
        }
        return Some(format!(
            "{}: learned reservation {} MiB exceeds {:.0}% of budget ({threshold} MiB) — held at {} MiB (runner {} + dind {}), pin to allow",
            entry.label,
            want_runner + want_dind,
            escalation_frac * 100.0,
            runner + dind,
            runner,
            dind,
        ));
    }
    entry.status = CapStatus::Learned;
    None
}

/// Recompute a class's caps from its windows as they stand, outside a live run
/// (startup replay, the first measured budget). A container with no samples keeps
/// its current cap — the profiling default — rather than collapsing to the MIN
/// floor. The warning is not pushed: the next live run says it again if it holds.
fn relearn(
    entry: &mut ClassCap,
    margin: f64,
    window: usize,
    profiling_cap: u64,
    budget: u64,
    escalation_frac: f64,
) {
    let want_runner = if entry.peaks_runner.is_empty() {
        entry.runner_mib
    } else {
        cap_from(&entry.peaks_runner, margin)
    };
    let want_dind = if entry.peaks_dind.is_empty() {
        entry.dind_mib
    } else {
        cap_from(&entry.peaks_dind, margin)
    };
    // A class that ran without a daemon holds no dind cap (see `record_run`).
    let no_dind = entry.dind_mib == 0;
    apply_want(
        entry,
        want_runner,
        want_dind,
        profiling_cap,
        budget,
        escalation_frac,
    );
    if no_dind {
        entry.dind_mib = 0;
    }
    refresh_shadow(entry, margin, window);
}

/// One class as the dashboard and `/api/state` show it.
fn class_view(key: &str, c: &ClassCap) -> ClassCapView {
    ClassCapView {
        key: key.to_string(),
        label: c.label.clone(),
        runner_mib: c.runner_mib,
        dind_mib: c.dind_mib,
        status: c.status,
        pinned: c.pinned,
        samples: c.peaks_runner.len().max(c.peaks_dind.len()),
        runner_mcpu: c.runner_mcpu,
        dind_mcpu: c.dind_mcpu,
        cpu_status: c.cpu_status,
        cpu_pinned: c.cpu_pinned,
        reserve_p75_runner_mib: c.shadow_runner_mib,
        reserve_p75_dind_mib: c.shadow_dind_mib,
        reserve_p75_mib: c.shadow_runner_mib + c.shadow_dind_mib,
        p75_runs: c.p75_runs,
        p75_trips: c.p75_trips,
        peaks_runner_mib: c.peaks_runner.iter().copied().collect(),
        peaks_dind_mib: c.peaks_dind.iter().copied().collect(),
        censored_runner: c.censored_runner.iter().copied().collect(),
        censored_dind: c.censored_dind.iter().copied().collect(),
    }
}

/// The caps a pin needs to stop a held class's OOMs: each proven container at
/// its want, the other at its cap — but never more than the whole budget, which
/// no pin can be admitted past. A want past the budget is the job to make lighter.
fn held_pin(caps: (u64, u64), proven: (bool, bool), wants: (u64, u64), budget: u64) -> (u64, u64) {
    let (runner, dind) = caps;
    let mut pin_runner = if proven.0 {
        wants.0.max(runner)
    } else {
        runner
    };
    let mut pin_dind = if proven.1 { wants.1.max(dind) } else { dind };
    if pin_runner + pin_dind > budget {
        match proven {
            // Both proven: the budget is shared in proportion to the wants, as the
            // threshold is in `apply_want`.
            (true, true) => {
                let share = u128::from(budget) * u128::from(pin_runner)
                    / u128::from((pin_runner + pin_dind).max(1));
                pin_runner = (share as u64).max(runner);
                pin_dind = budget.saturating_sub(pin_runner).max(dind);
            }
            (true, false) => pin_runner = budget.saturating_sub(pin_dind).max(runner),
            (false, true) => pin_dind = budget.saturating_sub(pin_runner).max(dind),
            (false, false) => {}
        }
    }
    (pin_runner, pin_dind)
}

/// The owner's attention item for a class held at the escalation threshold with
/// an OOM among its last [`RECENT_OOM_RUNS`] runs (ADR-0053). It names the
/// container that is held below its want, which is the one being killed. An
/// older OOM still in the window keeps the cap up, but asks nothing of anyone.
fn oom_held_view(key: &str, c: &ClassCap) -> Option<OomHeldView> {
    if !c.oom_held || c.pinned {
        return None;
    }
    let dind_held = c.held_pin.1 > c.dind_mib && c.held_pin.0 <= c.runner_mib;
    let (container, censored, cap_mib) = if dind_held {
        ("dind", &c.censored_dind, c.dind_mib)
    } else {
        ("runner", &c.censored_runner, c.runner_mib)
    };
    if !censored.iter().rev().take(RECENT_OOM_RUNS).any(|o| *o) {
        return None;
    }
    Some(OomHeldView {
        key: key.to_string(),
        label: c.label.clone(),
        container,
        cap_mib,
        reservation_mib: c.runner_mib + c.dind_mib,
        pin_runner_mib: c.held_pin.0,
        pin_dind_mib: c.held_pin.1,
        ooms: censored.iter().filter(|o| **o).count(),
        runs: censored.len(),
    })
}

/// The CPU half of a pin (see [`Limiter::pin`]): a set runner `cpus` pins the
/// runner share (floored) and, when set too, dind's; a zero runner `cpus` pins
/// nothing and leaves the CPU side exactly as it was.
fn pin_cpu(entry: &mut ClassCap, limits: &Limits) {
    if limits.runner.cpus > 0.0 {
        entry.runner_mcpu = docker::mcpu(limits.runner.cpus).max(MIN_RUNNER_MCPU);
        if limits.dind.cpus > 0.0 {
            entry.dind_mcpu = docker::mcpu(limits.dind.cpus);
        }
        entry.cpu_pinned = true;
        entry.cpu_status = CapStatus::Pinned;
    }
}

/// The CPU sample to record for one container, or `None` when its CPU was never
/// read.
///
/// A container that ran under a quota and was throttled in at least
/// [`THROTTLE_CENSOR`] of its periods hit that quota, so its measured peak is the
/// quota's doing, not the job's want: the sample is censored to
/// `max(peak, quota × CPU_BUMP)` so the share grows past it — the CPU analogue of
/// an OOM. The quota censored at is the larger of the class's share and the quota
/// the container actually ran under, which cohort sizing can raise (as memory does
/// with `applied_runner_mib`). With no quota there is nothing to have hit, so the
/// peak stands whatever the throttle counter says.
fn cpu_sample(usage: Option<CpuUsage>, applied_mcpu: u64, share_mcpu: u64) -> Option<u64> {
    let usage = usage?;
    if applied_mcpu > 0 && usage.throttled_ratio >= THROTTLE_CENSOR {
        let quota = share_mcpu.max(applied_mcpu);
        Some(usage.peak_mcpu.max((quota as f64 * CPU_BUMP) as u64))
    } else {
        Some(usage.peak_mcpu)
    }
}

/// Push a run's CPU samples and re-derive the class's shares: rolling max ×
/// `margin`, clamped to `[floor, ceiling]` per container.
///
/// A container with no samples keeps its current share — no reading is not a
/// reading of zero — and a class with none for either keeps its status too. A want
/// above `ceiling` is held at it and marks the class `warned`; the returned message
/// says what was held, for the caller to surface.
fn learn_cpu(
    entry: &mut ClassCap,
    runner: Option<u64>,
    dind: Option<u64>,
    window: usize,
    margin: f64,
    ceiling: u64,
) -> Option<String> {
    if let Some(s) = runner {
        push(&mut entry.cpu_peaks_runner, s, window);
    }
    if let Some(s) = dind {
        push(&mut entry.cpu_peaks_dind, s, window);
    }
    if entry.cpu_peaks_runner.is_empty() && entry.cpu_peaks_dind.is_empty() {
        return None; // never measured — stay at the profiling shares
    }
    let mut held = Vec::new();
    if !entry.cpu_peaks_runner.is_empty() {
        let want = cap_from(&entry.cpu_peaks_runner, margin);
        entry.runner_mcpu = want.min(ceiling).max(MIN_RUNNER_MCPU);
        if want > ceiling {
            held.push(format!(
                "runner wants {} cores, held at {}",
                fmt_cores(want),
                fmt_cores(entry.runner_mcpu)
            ));
        }
    }
    if !entry.cpu_peaks_dind.is_empty() {
        let want = cap_from(&entry.cpu_peaks_dind, margin);
        entry.dind_mcpu = want.min(ceiling).max(MIN_DIND_MCPU);
        if want > ceiling {
            held.push(format!(
                "dind wants {} cores, held at {}",
                fmt_cores(want),
                fmt_cores(entry.dind_mcpu)
            ));
        }
    }
    if held.is_empty() {
        entry.cpu_status = CapStatus::Learned;
        return None;
    }
    entry.cpu_status = CapStatus::Warned;
    Some(format!(
        "{}: {} — pin to allow",
        entry.label,
        held.join("; ")
    ))
}

/// The most a learned CPU share may grow to without a pin, and the runner share of
/// a class not yet measured: `profiling_cpus`, never below the runner floor.
fn cpu_ceiling(cpu: &CpuConfig) -> u64 {
    docker::mcpu(cpu.profiling_cpus).max(MIN_RUNNER_MCPU)
}

/// `ncpu × 1000 × overcommit − reserve_cpus × 1000` millicores, saturating at zero.
fn cpu_budget_mcpu(ncpu: u64, cpu: &CpuConfig) -> u64 {
    let budget = (ncpu as f64 * 1000.0 * cpu.overcommit - cpu.reserve_cpus * 1000.0).round();
    if budget.is_finite() && budget > 0.0 {
        budget as u64
    } else {
        0
    }
}

/// Step the CPU Gate 2 hysteresis with one reading: freeze at or above the pause
/// threshold, thaw at or below the resume threshold, hold in between.
fn cpu_hysteresis(frozen: bool, pct: f64, cpu: &CpuConfig) -> bool {
    if pct >= cpu.pause_pressure {
        true
    } else if pct <= cpu.resume_pressure {
        false
    } else {
        frozen
    }
}

/// Millicores as a Docker `--cpus` value.
fn cores(mcpu: u64) -> f64 {
    mcpu as f64 / 1000.0
}

/// Zero the dind ceiling when the profile was granted no daemon, leaving the
/// runner's untouched. A zero `--memory` emits no flag, which is right in both
/// readings: nothing to cap, because nothing to run.
fn without_dind(mut limits: Limits, dind: bool) -> Limits {
    if !dind {
        limits.dind.memory_mib = 0;
    }
    limits
}

/// Build the [`Limits`] a job launches with: both memory caps, and the runner's
/// `--cpus` quota (`0.0` = none). dind never gets a quota — its CPU share is
/// reserved, not enforced (ADR-0019).
fn container_limits(runner_mib: u64, dind_mib: u64, runner_cpus: f64) -> Limits {
    Limits {
        runner: ResourceLimits {
            cpus: runner_cpus,
            memory_mib: runner_mib,
        },
        dind: ResourceLimits {
            cpus: 0.0,
            memory_mib: dind_mib,
        },
    }
}

/// The sample to record for a container: its measured peak, or — on OOM — the cap
/// it was killed at (a censored "at least this much"), so the cap force-bumps.
fn sample_for(measured: Option<u64>, oom: bool, cap: u64) -> Option<u64> {
    if oom {
        Some(measured.unwrap_or(0).max((cap as f64 * OOM_BUMP) as u64))
    } else {
        measured
    }
}

fn push(window: &mut VecDeque<u64>, sample: u64, cap: usize) {
    window.push_back(sample);
    while window.len() > cap.max(1) {
        window.pop_front();
    }
}

/// [`push`] a sample together with whether it was censored by an OOM, keeping
/// the two windows the same length.
fn push_sample(
    window: &mut VecDeque<u64>,
    censored: &mut VecDeque<bool>,
    sample: u64,
    oom: bool,
    cap: usize,
) {
    push(window, sample, cap);
    censored.push_back(oom);
    while censored.len() > window.len() {
        censored.pop_front();
    }
}

/// The 75th percentile of a window, by nearest rank: the smallest sample with at
/// least 75% of the window at or below it. `0` for an empty window.
fn p75(window: &VecDeque<u64>) -> u64 {
    if window.is_empty() {
        return 0;
    }
    let mut sorted: Vec<u64> = window.iter().copied().collect();
    sorted.sort_unstable();
    let rank = (sorted.len() * 3).div_ceil(4);
    sorted[rank.max(1) - 1]
}

/// The reservation ADR-0033 §1 would debit for one container:
/// `min(cap, p75(window) × margin)`.
///
/// Equal to the cap whenever the window is not full (`full` samples), holds a
/// censored sample, or the class is pinned — the cases where a p75 means little or
/// the operator has spoken. Measured only in this step: admission debits the cap.
fn p75_reservation(
    window: &VecDeque<u64>,
    censored: &VecDeque<bool>,
    cap: u64,
    margin: f64,
    full: usize,
    pinned: bool,
) -> u64 {
    if pinned || window.len() < full.max(1) || censored.iter().any(|c| *c) {
        return cap;
    }
    cap.min((p75(window) as f64 * margin).ceil() as u64)
}

/// Recompute a class's stored would-be p75 reservations from its windows.
fn refresh_shadow(entry: &mut ClassCap, margin: f64, full: usize) {
    entry.shadow_runner_mib = p75_reservation(
        &entry.peaks_runner,
        &entry.censored_runner,
        entry.runner_mib,
        margin,
        full,
        entry.pinned,
    );
    entry.shadow_dind_mib = p75_reservation(
        &entry.peaks_dind,
        &entry.censored_dind,
        entry.dind_mib,
        margin,
        full,
        entry.pinned,
    );
}

/// Judge a finished run against the p75 reservation its class held before it
/// (rollout step 1: "how often it would have tripped"). A run counts only where a
/// p75 reservation was actually below the cap, since otherwise `"p75"` and `"max"`
/// debit the same and nothing could trip. It trips when a container with such a
/// reservation was OOM-killed or peaked above it.
fn judge_p75(entry: &mut ClassCap, runner: (Option<u64>, bool), dind: (Option<u64>, bool)) {
    let exceeds = |(peak, oom): (Option<u64>, bool), reserved: u64, cap: u64| {
        (reserved < cap).then(|| oom || peak.is_some_and(|p| p > reserved))
    };
    let verdicts = [
        exceeds(runner, entry.shadow_runner_mib, entry.runner_mib),
        exceeds(dind, entry.shadow_dind_mib, entry.dind_mib),
    ];
    if verdicts.iter().any(Option::is_some) {
        entry.p75_runs += 1;
        if verdicts.contains(&Some(true)) {
            entry.p75_trips += 1;
        }
    }
}

fn cap_from(window: &VecDeque<u64>, margin: f64) -> u64 {
    let peak = window.iter().copied().max().unwrap_or(0);
    (peak as f64 * margin).ceil() as u64
}

// --- dashboard views -------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ClassCapView {
    pub key: String,
    pub label: String,
    pub runner_mib: u64,
    pub dind_mib: u64,
    pub status: CapStatus,
    pub pinned: bool,
    pub samples: usize,
    /// CPU shares in millicores (ADR-0019) — shown in every CPU mode.
    pub runner_mcpu: u64,
    pub dind_mcpu: u64,
    pub cpu_status: CapStatus,
    pub cpu_pinned: bool,
    /// What ADR-0033's p75 reservation would debit for this class, per container
    /// and together (MiB): `min(cap, p75 × margin)`, equal to the cap until the
    /// window is full, while it holds an OOM sample, and while pinned. Measured
    /// only — admission still debits `runner_mib` + `dind_mib`.
    pub reserve_p75_runner_mib: u64,
    pub reserve_p75_dind_mib: u64,
    pub reserve_p75_mib: u64,
    /// Finished runs that had a p75 reservation below the cap to be judged
    /// against, and how many of them peaked above it (or were OOM-killed): how
    /// often `"p75"` would have tripped.
    pub p75_runs: u64,
    pub p75_trips: u64,
    /// The windows of measured peaks the caps are learned from, oldest first
    /// (MiB), bounded by the learner's `window`; empty until the class has
    /// samples. The two are separate windows, not one per run: a run that left
    /// no dind sample adds to the runner's window only.
    pub peaks_runner_mib: Vec<u64>,
    pub peaks_dind_mib: Vec<u64>,
    /// Parallel to `peaks_runner_mib` / `peaks_dind_mib`: true where that
    /// sample was an OOM stand-in, not a measurement.
    pub censored_runner: Vec<bool>,
    pub censored_dind: Vec<bool>,
}

impl ClassCapView {
    fn reservation(&self) -> u64 {
        self.runner_mib + self.dind_mib
    }

    /// What admitting a job of this class debits in the mode in force: the cap,
    /// or the p75 shadow while `p75` (the same rule admission applies).
    fn debit(&self, p75: bool) -> u64 {
        debit_mib(
            &Reservation {
                mib: self.reservation(),
                shadow_mib: self.reserve_p75_mib,
                ..Default::default()
            },
            p75,
        )
    }
}

/// The CPU half of the limiter's state (ADR-0019).
#[derive(Debug, Clone, Serialize)]
pub struct CpuView {
    /// `"auto"`, `"off"` or `"unstated"`.
    pub mode: &'static str,
    /// The VM's core count, once probed.
    pub ncpu: Option<u64>,
    /// `0` until the core count is known.
    pub budget_mcpu: u64,
    /// CPU reserved by running jobs — tracked in every mode.
    pub reserved_mcpu: u64,
    /// VM CPU PSI `some avg60` (percent), when the kernel reports one.
    pub pressure: Option<f64>,
    /// The pressure hysteresis is in its paused state. It is tracked in every mode
    /// but holds admission only in `auto`; `false` while there is no reading.
    pub frozen: bool,
    pub overcommit: f64,
    pub reserve_cpus: f64,
    pub profiling_cpus: f64,
}

/// The disk gate's state (ADR-0024 §1).
#[derive(Debug, Clone, Serialize)]
pub struct DiskView {
    /// `"on"`, `"off"` or `"unstated"`.
    pub mode: &'static str,
    /// The admission floor in GiB — `null` unless `on`.
    pub floor_gib: Option<u64>,
    /// The latest reading of free space on the Docker volume (MiB), fresh or not;
    /// `null` before the first. Measured in every mode, so the dashboard shows what
    /// a floor would see before anyone sets one. Where two volumes are read, the
    /// tighter of those still fresh: the one the gate acts on (#410).
    pub free_mib: Option<u64>,
    /// Which volume `free_mib` is of: `docker`, `mac_vms` or `linux_vm` (#410).
    /// `null` with it.
    pub volume: Option<host::DiskVolume>,
    /// How old that reading is, in seconds.
    pub reading_age_s: Option<u64>,
    /// Whether it is recent enough to be evidence. A stale one holds nothing shut.
    pub fresh: bool,
    /// The host volume's own latest reading (MiB), fresh or not (#410). On a
    /// macOS SYSTEM host, the Mac's volume holding the VM disks; elsewhere the
    /// same as `free_mib`.
    pub host_free_mib: Option<u64>,
    /// The Linux VM's disk's latest reading (MiB), fresh or not; `null` where
    /// there is no Linux VM or it has never been read (#410).
    pub vm_free_mib: Option<u64>,
    /// How old that reading is, in seconds.
    pub vm_reading_age_s: Option<u64>,
    /// Why the Linux VM's disk could not be read at the last attempt; `null`
    /// while it can. The gate then acts on the host volume alone (#410).
    pub vm_unreadable: Option<String>,
    /// Docker admissions of the last ten minutes, still counted as writing their
    /// first gigabytes…
    pub ramping: usize,
    /// …and what they were charged between them, in MiB: each its own — 10 GiB
    /// for a dind job, 3 GiB for any other (ADR-0032 §1).
    pub held_mib: u64,
    /// The charge the gate is holding a parked claim back for, in MiB: that of the
    /// longest-waiting claim whose own charge free space cannot cover above the
    /// floor. `null` when it holds none back. Beside the free space and the floor,
    /// it is what says a dind job parks while light jobs still run.
    pub charge_mib: Option<u64>,
    /// The gate refuses every Docker job right now, even one charged a light job's
    /// 3 GiB. Only ever while `on`.
    pub closed: bool,
    /// The Linux VM's disk: its real size, its ceiling, and a pending grow and
    /// what it waits for (ADR-0061 §2, §6). `null` where no grow loop runs.
    pub vm_grow: Option<crate::vm::grow::GrowView>,
}

/// The VMs' admission (ADR-0026): the wall as a state of its own, and the macOS
/// class. Additive: an older reader ignores it.
#[derive(Debug, Clone, Serialize)]
pub struct VmView {
    /// `"open"`, `"red"`, `"out_of_date"` or `"unread"`: the wall's verdict.
    /// Anything but `open` stops every new VM job, and Docker's too once
    /// `wall_required`. Jobs already running finish.
    pub wall: &'static str,
    /// Why the wall is not open, in a sentence; `null` when it is.
    pub wall_detail: Option<String>,
    /// Docker admission waits on the wall too (Docker runs in the Linux VM).
    pub wall_required: bool,
    /// The macOS VM session (ADR-0051): `"not_required"` (no session account
    /// recorded), `"unprobed"`, `"up"`, `"down"` or `"no_account"` (the record
    /// names an account this host lacks). Anything but `up` and `not_required`
    /// refuses new macOS VM jobs.
    pub session: &'static str,
    /// What to do about a session that is not up, and why; `null` otherwise.
    pub session_detail: Option<String>,
    pub macos_running: usize,
    pub macos_cap: usize,
    /// What one macOS VM is planned at (MiB).
    pub macos_vm_mib: u64,
    /// The Linux VM's memory reserved inside the share (MiB).
    pub linux_vm_mib: u64,
    /// The gate refusing a macOS VM job right now, if one does.
    pub refusal: Option<WaitView>,
    /// Every VM the supervisor reports, with its state from the supervisor's
    /// fixed set. Empty until a supervisor has listed.
    pub vms: Vec<VmRow>,
    /// Seconds since the least recently answered supervisor list (per VM kind)
    /// last answered; `null` before any has.
    /// The popover shows an "as of" once it is over twice the poll interval.
    pub listed_age_s: Option<u64>,
    /// Whether the Linux image is the pinned one, and how the golden images
    /// stand as one (`verified`, `unverified`, `verifying`, `failed:<check>` or
    /// `unknown`, ADR-0049 §7).
    pub images: ImagesView,
    /// macOS jobs wait for a verified golden image (ADR-0049 §7).
    pub golden_required: bool,
    /// Each golden image a `macos-vm` profile names, and how it stands.
    pub golden: Vec<GoldenView>,
    /// The kept runner image is missing from the Linux VM's Docker, or could not
    /// be loaded into it; `null` when it is there or none is configured.
    pub runner_image: Option<String>,
}

/// One VM as the supervisor reports it, with what the daemon knows of its size
/// and job (popover/vms/anatomy.md § 5). Display only: nothing gates on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmRow {
    pub slot: u8,
    pub kind: VmKind,
    pub state: VmState,
    pub stop_reason: Option<StopReason>,
    /// When the supervisor started it (epoch seconds), for the row's age.
    pub started_unix: u64,
    /// `null` = not known: a macOS VM whose claim matches no in-flight job.
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    /// The job a macOS VM serves; `null` for the Linux VM (its jobs are
    /// `docker_inflight`) and for a VM no in-flight job claims.
    pub job: Option<VmJob>,
    /// The VM's claim: the join key to its in-flight job, never serialized (it
    /// names a job).
    #[serde(skip)]
    pub claim: String,
}

/// The job a macOS VM serves. `job_name` is GitHub's text and a fork or
/// untrusted-actor job controls it, so it is `null` for those: the row names
/// the repo only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmJob {
    pub repo: String,
    pub job_name: Option<String>,
}

/// What an in-flight macOS VM job tells its VM's row: the orchestrator holds the
/// claims, the limiter holds the rows, and [`join_vm_jobs`] joins them on `claim`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmJobFacts {
    /// `claim_id(runner_name)`: the claim the VM was started under.
    pub claim: String,
    /// The serving profile's VM size.
    pub cpus: u8,
    pub memory_mib: u32,
    pub repo: String,
    /// `None` for a fork or untrusted-actor job, or one with no name known.
    pub job_name: Option<String>,
}

/// Fill the size and job of each macOS row whose claim an in-flight job holds.
/// A row no job claims (a VM adopted after a restart, say) keeps `null`s.
pub fn join_vm_jobs(vms: &mut [VmRow], jobs: &[VmJobFacts]) {
    for row in vms.iter_mut().filter(|r| r.kind == VmKind::Macos) {
        if let Some(j) = jobs.iter().find(|j| j.claim == row.claim) {
            row.cpus = Some(j.cpus);
            row.memory_mib = Some(j.memory_mib);
            row.job = Some(VmJob {
                repo: j.repo.clone(),
                job_name: j.job_name.clone(),
            });
        }
    }
}

/// How one image stands against the digest this release pins (ADR-0026: a release
/// that bumps it shows the image out of date until `sudo faber setup` runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImageStanding {
    /// Not judged yet, or no such image on this host.
    #[default]
    Unknown,
    Current,
    OutOfDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct ImagesView {
    pub linux: ImageStanding,
    /// The golden images' verification, the worst of them (#268): judged on this
    /// host, not against a pinned digest, since every host builds its own.
    pub macos: Summary,
}

/// One golden image in `/api/state` (ADR-0049 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoldenView {
    pub image: String,
    /// `verified`, `unverified`, `verifying`, `failed:<check>` or `unknown`.
    pub standing: Summary,
    /// What is wrong and what to do; `null` when verified.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LimiterView {
    pub budget_mib: u64,
    pub available_mib: Option<u64>,
    /// `false` once Gate 0 has closed — the admission stall is a dead daemon, not
    /// a full budget. Without this the two look identical from the outside.
    pub docker_reachable: bool,
    pub reserved_mib: u64,
    /// What the running jobs would hold under ADR-0033's p75 reservation (MiB),
    /// summed as `reserved_mib` is. Measured only; no gate reads it.
    pub reserved_p75_mib: u64,
    /// ADR-0033 §2, §4: the configured reserve mode, the one in force, and the
    /// fallback's trips and next re-arm. Additive.
    pub reserve: ReserveView,
    pub docker_inflight: usize,
    pub frozen: bool,
    pub reserve_mib: u64,
    pub headroom_mib: u64,
    pub margin: f64,
    pub native_concurrency: usize,
    /// The ceiling in force (`auto` resolved to its number, `off` = `None`). Kept
    /// numeric so an older menu-bar app still reads this view.
    pub hard_ceiling: Option<usize>,
    /// The ceiling *setting*: `"auto"`, `"off"` or a number.
    pub hard_ceiling_setting: HardCeiling,
    /// The operator's hold on new Docker admissions (issue #66): its reason and
    /// since when. Always present in the JSON — `null` when admission is not held.
    pub hold: Option<Hold>,
    /// CPU-aware admission (ADR-0019).
    pub cpu: CpuView,
    /// The disk gate (ADR-0024).
    pub disk: DiskView,
    /// VM admission and the wall (ADR-0026).
    pub vm: VmView,
    /// The machine, its share, and what the VM leaves it (ADR-0021).
    pub machine: MachineView,
    /// Gate 1's reserve and the co-tenant peak behind it (ADR-0021 §3).
    /// `reserve_mib` above is the floor; `cotenant.reserve_mib` is in force.
    pub cotenant: CoTenantView,
    /// Per gate, since this process started: claims parked there now, arrivals,
    /// time spent, and admissions it bound (issue #85). Keyed by
    /// [`WaitView::gate`]; a gate nothing has parked at is absent.
    pub gates: BTreeMap<String, GateCount>,
    /// How many more Docker jobs of a typical size admission would take right now,
    /// and the gate that runs out first (for the tray's hero). `null` while the
    /// memory budget is not measured yet.
    pub headroom: Option<HeadroomView>,
    pub classes: Vec<ClassCapView>,
    pub warnings: Vec<String>,
    /// The same warnings with a stable `key` each, in the same order, so the
    /// operator can dismiss one (ADR a-warning-that-does-not-stop-jobs-can-be-dismissed).
    /// `warnings` stays for older clients.
    pub warning_items: Vec<WarningItem>,
    /// ADR-0053: classes OOM-killed recently and held at the escalation threshold,
    /// most OOMs first. The popover and the dashboard raise each as an attention
    /// row; a pin clears it.
    pub oom_held: Vec<OomHeldView>,
}

/// A class faber stopped growing (ADR-0053): it was OOM-killed among its last few
/// runs, and the cap it has shown it needs is past the escalation threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OomHeldView {
    pub key: String,
    pub label: String,
    /// The container that was killed: `"runner"` or `"dind"`.
    pub container: &'static str,
    /// That container's cap now, held at the threshold (MiB).
    pub cap_mib: u64,
    /// The class's whole reservation, runner + dind (MiB).
    pub reservation_mib: u64,
    /// The caps a pin needs to stop the OOMs (MiB): each killed container at
    /// what it has shown it wants, the other at its cap. The popover's Pin.
    pub pin_runner_mib: u64,
    pub pin_dind_mib: u64,
    /// OOMs of that container in the class's window, and the runs in it.
    pub ooms: usize,
    pub runs: usize,
}

/// How much more Docker work admission would take, and what stops it.
///
/// The gates each answer yes or no for one job; the tray wants "room for N more"
/// and the gate that will say no first, which `/api/state` alone cannot give it
/// honestly (the ledgers, the ceiling and the disk floor are in different units).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeadroomView {
    /// More Docker jobs of `job_mib`/`job_mcpu` that would be admitted now.
    pub jobs: u64,
    /// The gate that binds first, keyed like [`WaitView::gate`]: `ceiling`,
    /// `memory`, `vm-memory`, `cpu`, `disk`, or — when admission is shut outright,
    /// `jobs` 0 — `held`, `paused`, `machine-pressure`/`swap`, `docker`, `disk`,
    /// `adoption`, `vm-memory` (frozen) or `cpu-pressure`.
    pub binds: String,
    /// The typical job size the count is in.
    pub job_mib: u64,
    pub job_mcpu: u64,
}

/// What [`headroom`] reads, gathered by [`Limiter::snapshot`] under its locks so
/// the arithmetic itself needs no limiter.
struct HeadroomInputs<'a> {
    budget_mib: u64,
    reserved_mib: u64,
    available_mib: Option<u64>,
    headroom_mib: u64,
    budget_mcpu: u64,
    reserved_mcpu: u64,
    cpu_auto: bool,
    docker_inflight: usize,
    ceiling: Option<usize>,
    disk: &'a DiskView,
    /// `(mib, mcpu)` per learned class.
    class_sizes: Vec<(u64, u64)>,
    /// What a class never seen would reserve; the size when there are no classes.
    unknown_size: (u64, u64),
    /// The gate that is shut outright, in admission order, if any.
    shut: Option<&'static str>,
}

/// The typical job is the median learned class, so one outlier does not set the
/// count; with no classes, an unknown class's profiling reservation.
fn typical_job(sizes: &[(u64, u64)], unknown: (u64, u64)) -> (u64, u64) {
    if sizes.is_empty() {
        return unknown;
    }
    let mut mib: Vec<u64> = sizes.iter().map(|s| s.0).collect();
    let mut mcpu: Vec<u64> = sizes.iter().map(|s| s.1).collect();
    mib.sort_unstable();
    mcpu.sort_unstable();
    (mib[mib.len() / 2], mcpu[mcpu.len() / 2])
}

/// Jobs of the typical size that fit, and the gate that fits fewest. Ties go to
/// the earlier gate in admission order. `None` before the budget is measured.
fn headroom(i: &HeadroomInputs<'_>) -> Option<HeadroomView> {
    if i.budget_mib == 0 {
        return None;
    }
    let (job_mib, job_mcpu) = typical_job(&i.class_sizes, i.unknown_size);
    let view = |jobs: u64, binds: &str| HeadroomView {
        jobs,
        binds: binds.to_string(),
        job_mib,
        job_mcpu,
    };
    if let Some(gate) = i.shut {
        return Some(view(0, gate));
    }
    let mib = job_mib.max(1);
    let mut gates: Vec<(&str, u64)> = Vec::new();
    if let Some(max) = i.ceiling {
        gates.push(("ceiling", max.saturating_sub(i.docker_inflight) as u64));
    }
    gates.push(("memory", i.budget_mib.saturating_sub(i.reserved_mib) / mib));
    if let Some(avail) = i.available_mib {
        gates.push(("vm-memory", avail.saturating_sub(i.headroom_mib) / mib));
    }
    if i.cpu_auto && i.budget_mcpu > 0 {
        gates.push((
            "cpu",
            i.budget_mcpu.saturating_sub(i.reserved_mcpu) / job_mcpu.max(1),
        ));
    }
    if let (Some(floor), Some(free), true) = (i.disk.floor_gib, i.disk.free_mib, i.disk.fresh) {
        let spare = free
            .saturating_sub(i.disk.held_mib)
            .saturating_sub(floor.saturating_mul(1024));
        gates.push(("disk", spare / DISK_CHARGE_MIB));
    }
    // `min_by_key` keeps the first of equal minima.
    let (binds, jobs) = gates.into_iter().min_by_key(|g| g.1)?;
    Some(view(jobs, binds))
}

/// Who a set-aside is held for, in a refusal's text (ADR-0036).
fn setaside_for(priority: bool) -> &'static str {
    if priority {
        "a priority claim"
    } else {
        "an older claim"
    }
}

/// Why [`Limiter::try_admit_charged`] refused: the first gate that said no, with the
/// numbers it said no on, in the order the gates are checked.
///
/// Every parked Docker claim carries its latest one (see [`Wait::view`]), so a
/// host admitting nothing always says why. Before this, a claim parked behind a
/// zero ceiling nobody remembered setting looked exactly like one parked behind a
/// full budget.
#[derive(Debug, Clone, PartialEq)]
pub enum Wait {
    /// The operator's hold, with the reason they gave.
    Held { reason: String },
    /// faber is paused (ADR-0023), by the operator or by the janitor for the
    /// length of a reclamation.
    Paused { by: Pause },
    /// The machine's own memory (ADR-0021 §2c, ADR-0032 §3): macOS reports
    /// pressure, or swap is growing. Holds macOS VM claims too.
    Machine(MachineHold),
    /// [`Wait::Machine`] while the Docker VM is itself larger than the share
    /// leaves CI (#160): the VM never returns memory, so the pressure will not
    /// clear until it is resized, and faber runs one job at a time meanwhile.
    MachineVm {
        level: PressureLevel,
        vm_mib: u64,
        left_mib: u64,
    },
    /// Gate 0: the availability probe has missed enough times in a row that
    /// Docker counts as down.
    DockerUnreachable,
    /// The disk gate (ADR-0024 §1): free space on the Docker volume, less what
    /// `ramping` recent admissions were charged (`held_mib`) and less this job's
    /// own charge (`charge_mib`, ADR-0032 §1), is under the admission floor.
    Disk {
        free_mib: u64,
        /// Which volume was short (#410).
        volume: host::DiskVolume,
        ramping: usize,
        held_mib: u64,
        charge_mib: u64,
        floor_gib: u64,
    },
    /// Gate 0b (ADR-0011 §5): containers from before the restart are still being
    /// enumerated, so the ledger does not yet describe this host.
    Adoption,
    /// The hard ceiling on concurrent Docker jobs.
    Ceiling { inflight: usize, max: usize },
    /// Gate 1: the memory ledger. `budget_mib == 0` is a VM not measured yet.
    /// `setaside_mib` is what the longest-waiting claim has set aside, which only
    /// the head itself may spend.
    Memory {
        need_mib: u64,
        reserved_mib: u64,
        budget_mib: u64,
        setaside_mib: u64,
        /// The set-aside is held for a priority repo's claim that this one,
        /// being from another repo, queues behind (ADR-0036).
        setaside_priority: bool,
    },
    /// CPU Gate 1 (ADR-0019), on the same terms in millicores. `need_mcpu` is
    /// already clamped to the budget, as the gate checks it.
    Cpu {
        need_mcpu: u64,
        reserved_mcpu: u64,
        budget_mcpu: u64,
        setaside_mcpu: u64,
        setaside_priority: bool,
    },
    /// Gate 2: live VM memory, too low for this job or frozen below `headroom`
    /// until it recovers past `resume_mib`.
    VmMemory {
        need_mib: u64,
        available_mib: u64,
        headroom_mib: u64,
        resume_mib: u64,
        frozen: bool,
    },
    /// CPU Gate 2 (ADR-0019): VM CPU pressure, paused until it falls to `resume`.
    CpuPressure { pressure: f64, resume: f64 },
    /// A VM gate (ADR-0026): the wall, the macOS class cap, or a VM's share of
    /// memory and disk.
    Vm(VmRefusal),
}

impl Wait {
    /// The gate's key and a line saying why, short enough for a narrow row.
    pub fn view(&self) -> WaitView {
        let (gate, detail) = match self {
            Wait::Held { reason } => ("held", format!("Docker admission held: {reason}")),
            Wait::Paused { by } => (
                "paused",
                match by {
                    Pause::Operator => "faber is paused — nothing new starts until it resumes",
                    Pause::Reclaim => "paused while the janitor reclaims disk",
                }
                .to_string(),
            ),
            Wait::Machine(hold) => hold.view(),
            Wait::MachineVm {
                level,
                vm_mib,
                left_mib,
            } => (
                "machine-pressure",
                format!(
                    "macOS memory pressure is {} and the Docker VM ({} GiB) is larger than the \
                     machine share leaves for CI ({} GiB): resize the VM — running one job at a \
                     time",
                    level.as_str(),
                    crate::host::fmt_gib(*vm_mib),
                    crate::host::fmt_gib(*left_mib)
                ),
            ),
            Wait::DockerUnreachable => ("docker", "Docker is not answering".to_string()),
            Wait::Disk {
                free_mib,
                volume,
                ramping,
                held_mib,
                charge_mib,
                floor_gib,
            } => {
                // What is held is said only when something is: then free space may
                // read well above the floor and still be refused, and this is why.
                let held = match ramping {
                    0 => String::new(),
                    n => format!(
                        " ({} GiB held for {n} job{} still starting)",
                        host::fmt_gib(*held_mib),
                        if *n == 1 { "" } else { "s" }
                    ),
                };
                // The job's own charge is always said (ADR-0032 §1): it is what
                // tells a parked dind job from the light jobs still running.
                (
                    "disk",
                    format!(
                        "{} GiB free on {}{held}, needs {} GiB above the {floor_gib} GiB \
                         admission floor",
                        host::fmt_gib(*free_mib),
                        volume.describe(),
                        host::fmt_gib(*charge_mib)
                    ),
                )
            }
            Wait::Vm(refusal) => refusal.describe(),
            Wait::Adoption => (
                "adoption",
                "adopting containers from before the restart".to_string(),
            ),
            Wait::Ceiling { inflight, max } => (
                "ceiling",
                format!("hard ceiling: {inflight} of {max} Docker jobs running"),
            ),
            Wait::Memory {
                need_mib,
                reserved_mib,
                budget_mib,
                setaside_mib,
                setaside_priority,
            } => {
                let free = budget_mib.saturating_sub(*reserved_mib);
                let detail = if *budget_mib == 0 {
                    "memory budget not measured yet".to_string()
                } else if *need_mib > *budget_mib {
                    format!("needs {need_mib} MiB, more than the whole {budget_mib} MiB budget")
                } else if *need_mib <= free {
                    // It fits on its own: what it may not take is the head's room.
                    format!(
                        "needs {need_mib} MiB, {free} MiB free of {budget_mib} MiB, \
                         {setaside_mib} MiB set aside for {}",
                        setaside_for(*setaside_priority)
                    )
                } else {
                    format!("needs {need_mib} MiB, {free} MiB free of {budget_mib} MiB")
                };
                ("memory", detail)
            }
            Wait::Cpu {
                need_mcpu,
                reserved_mcpu,
                budget_mcpu,
                setaside_mcpu,
                setaside_priority,
            } => {
                let free = budget_mcpu.saturating_sub(*reserved_mcpu);
                let detail = if *budget_mcpu == 0 {
                    "no CPU budget left after the reserve: one job at a time".to_string()
                } else if *need_mcpu <= free {
                    format!(
                        "needs {} cores, {} free of {}, {} set aside for {}",
                        fmt_cores(*need_mcpu),
                        fmt_cores(free),
                        fmt_cores(*budget_mcpu),
                        fmt_cores(*setaside_mcpu),
                        setaside_for(*setaside_priority)
                    )
                } else {
                    format!(
                        "needs {} cores, {} free of {}",
                        fmt_cores(*need_mcpu),
                        fmt_cores(free),
                        fmt_cores(*budget_mcpu)
                    )
                };
                ("cpu", detail)
            }
            Wait::VmMemory {
                need_mib,
                available_mib,
                headroom_mib,
                resume_mib,
                frozen,
            } => {
                let detail = if *frozen {
                    format!(
                        "VM memory frozen under pressure: {available_mib} MiB available, \
                         resumes at {resume_mib} MiB"
                    )
                } else {
                    format!(
                        "VM memory: {available_mib} MiB available, needs {need_mib} + \
                         {headroom_mib} MiB headroom"
                    )
                };
                ("vm-memory", detail)
            }
            Wait::CpuPressure { pressure, resume } => (
                "cpu-pressure",
                format!("VM CPU pressure {pressure:.0}%, resumes at {resume:.0}%"),
            ),
        };
        WaitView::new(gate, detail)
    }
}

/// Why a parked claim is waiting, as the dashboard and the tray read it: the gate
/// that holds it, and a line saying why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WaitView {
    /// `held`, `paused`, `machine-pressure`, `swap`, `docker`, `disk`, `adoption`,
    /// `ceiling`, `memory`, `cpu`, `vm-memory` or `cpu-pressure` from admission
    /// ([`Wait`]); `image`, `image-pull`, `pool`, `repo-held`, `refused-job`,
    /// `image-missing` or `github` from the waits a claim can park in before it
    /// gets there; `wasted` for a claim demoted as likely wasted (ADR-0063 §5),
    /// whatever gate refused it.
    pub gate: String,
    pub detail: String,
}

impl WaitView {
    pub fn new(gate: &str, detail: impl Into<String>) -> Self {
        Self {
            gate: gate.to_string(),
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::PressureLevel;

    fn cfg() -> AutoLimitConfig {
        AutoLimitConfig {
            reserve_mib: 2048,
            headroom_mib: 1024,
            band_mib: 512,
            profiling_cap_mib: 4096,
            margin: 1.5,
            escalation_frac: 0.5,
            window: 3,
            native_concurrency: 2,
            probe_image: "busybox".to_string(),
            hard_ceiling: HardCeiling::Off,
            cpu: crate::config::CpuConfig::default(),
            reserve: crate::config::ReserveSetting::Unstated,
            priority_repos: vec!["Acme/Tooling".to_string()],
        }
    }

    fn report(runner: u64, dind: u64) -> RunReport {
        RunReport {
            job_ok: true,
            runner_peak_mib: Some(runner),
            dind_peak_mib: Some(dind),
            ..Default::default()
        }
    }

    fn disk_view(floor_gib: Option<u64>, free_mib: Option<u64>, fresh: bool) -> DiskView {
        DiskView {
            mode: if floor_gib.is_some() { "on" } else { "off" },
            floor_gib,
            free_mib,
            volume: None,
            reading_age_s: None,
            fresh,
            host_free_mib: free_mib,
            vm_free_mib: None,
            vm_reading_age_s: None,
            vm_unreadable: None,
            ramping: 0,
            held_mib: 0,
            charge_mib: None,
            closed: false,
            vm_grow: None,
        }
    }

    /// An open host with room for `budget_mib / 1024` 1 GiB jobs and nothing else
    /// binding; each test narrows one thing.
    fn open_inputs(disk: &DiskView) -> HeadroomInputs<'_> {
        HeadroomInputs {
            budget_mib: 10 * 1024,
            reserved_mib: 0,
            available_mib: None,
            headroom_mib: 1024,
            budget_mcpu: 8000,
            reserved_mcpu: 0,
            cpu_auto: false,
            docker_inflight: 0,
            ceiling: None,
            disk,
            class_sizes: vec![(1024, 1000)],
            unknown_size: (8192, 4000),
            shut: None,
        }
    }

    #[test]
    fn headroom_ceiling_binds() {
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.ceiling = Some(4);
        i.docker_inflight = 1;
        let h = headroom(&i).unwrap();
        assert_eq!((h.jobs, h.binds.as_str()), (3, "ceiling"));
        assert_eq!((h.job_mib, h.job_mcpu), (1024, 1000));
    }

    #[test]
    fn headroom_sizes_the_typical_job_by_what_admission_debits() {
        let class = ClassCapView {
            key: "k".into(),
            label: "k".into(),
            runner_mib: 3072,
            dind_mib: 1024,
            status: CapStatus::Learned,
            pinned: false,
            samples: 3,
            runner_mcpu: 1000,
            dind_mcpu: 0,
            cpu_status: CapStatus::Learned,
            cpu_pinned: false,
            reserve_p75_runner_mib: 1536,
            reserve_p75_dind_mib: 512,
            reserve_p75_mib: 2048,
            p75_runs: 0,
            p75_trips: 0,
            peaks_runner_mib: vec![],
            peaks_dind_mib: vec![],
            censored_runner: vec![],
            censored_dind: vec![],
        };
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.reserved_mib = 2048;
        // "max": the cap sizes the job, as before.
        i.class_sizes = vec![(class.debit(false), 1000)];
        let h = headroom(&i).unwrap();
        assert_eq!((h.job_mib, h.jobs), (4096, 2));
        // "p75": the ledger holds p75 debits, so the job is sized in p75 too.
        i.class_sizes = vec![(class.debit(true), 1000)];
        let h = headroom(&i).unwrap();
        assert_eq!((h.job_mib, h.jobs), (2048, 4));
        // No p75 figure yet: debits the cap in either mode.
        let bare = ClassCapView {
            reserve_p75_mib: 0,
            ..class
        };
        assert_eq!(bare.debit(true), 4096);
    }

    #[test]
    fn headroom_memory_binds_and_wins_a_tie_by_admission_order() {
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.reserved_mib = 7 * 1024;
        let h = headroom(&i).unwrap();
        assert_eq!((h.jobs, h.binds.as_str()), (3, "memory"));
        // The ceiling ties at 3 and comes first.
        i.ceiling = Some(3);
        assert_eq!(headroom(&i).unwrap().binds, "ceiling");
    }

    #[test]
    fn headroom_vm_memory_binds_on_live_availability() {
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.available_mib = Some(3 * 1024 + 1024);
        let h = headroom(&i).unwrap();
        assert_eq!((h.jobs, h.binds.as_str()), (3, "vm-memory"));
    }

    #[test]
    fn headroom_ignores_cpu_unless_auto() {
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.reserved_mcpu = 7000;
        assert_eq!(headroom(&i).unwrap().binds, "memory");
        i.cpu_auto = true;
        let h = headroom(&i).unwrap();
        assert_eq!((h.jobs, h.binds.as_str()), (1, "cpu"));
    }

    #[test]
    fn headroom_disk_binds_when_on_and_fresh_and_is_ignored_when_stale() {
        // 20 GiB free over a 10 GiB floor: 10 GiB spare = 3 light jobs of 3 GiB.
        let fresh = disk_view(Some(10), Some(20 * 1024), true);
        let h = headroom(&open_inputs(&fresh)).unwrap();
        assert_eq!((h.jobs, h.binds.as_str()), (3, "disk"));
        let stale = disk_view(Some(10), Some(20 * 1024), false);
        assert_eq!(headroom(&open_inputs(&stale)).unwrap().binds, "memory");
        let off = disk_view(None, Some(20 * 1024), true);
        assert_eq!(headroom(&open_inputs(&off)).unwrap().binds, "memory");
    }

    #[test]
    fn headroom_is_zero_when_admission_is_shut() {
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.shut = Some("held");
        let h = headroom(&i).unwrap();
        assert_eq!((h.jobs, h.binds.as_str()), (0, "held"));
    }

    #[test]
    fn headroom_job_size_is_the_median_class_else_the_unknown_reservation() {
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.class_sizes = vec![(500, 100), (4000, 900), (1000, 300)];
        let h = headroom(&i).unwrap();
        assert_eq!((h.job_mib, h.job_mcpu), (1000, 300));
        i.class_sizes = vec![];
        let h = headroom(&i).unwrap();
        assert_eq!((h.job_mib, h.job_mcpu), (8192, 4000));
        // A zero-sized class must not divide by zero.
        i.class_sizes = vec![(0, 0)];
        assert!(headroom(&i).is_some());
    }

    #[test]
    fn headroom_is_none_until_the_budget_is_measured() {
        let disk = disk_view(None, None, false);
        let mut i = open_inputs(&disk);
        i.budget_mib = 0;
        assert_eq!(headroom(&i), None);
    }

    #[test]
    fn snapshot_serializes_headroom_with_its_four_keys() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        let v = serde_json::to_value(lim.snapshot()).unwrap();
        assert!(v["headroom"].is_null(), "no budget yet");
        lim.set_vm_total_for_test(1 << 20);
        let v = serde_json::to_value(lim.snapshot()).unwrap();
        let h = v["headroom"].as_object().expect("headroom object");
        for k in ["jobs", "binds", "job_mib", "job_mcpu"] {
            assert!(h.contains_key(k), "missing {k}");
        }
    }

    #[test]
    fn unknown_class_reserves_the_profiling_ceiling() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        // 2 × 4096 profiling ceiling.
        assert_eq!(lim.reservation_for("repo::1::build", true).mib, 8192);
        let (_, profiling) = lim.limits_for("repo::1::build", true);
        assert!(profiling);
    }

    #[tokio::test]
    async fn auto_hard_ceiling_caps_concurrent_docker_admissions() {
        // AUTO_HARD_CEILING is 4: a budget with plenty of room must still refuse
        // a 5th concurrent job, and freeing one of the four must let it in.
        let mut c = cfg();
        c.hard_ceiling = HardCeiling::Auto;
        let lim = Limiter::new(c, &HashMap::new());
        lim.set_vm_total_for_test(1 << 20); // budget far larger than 4 × 100 MiB

        let mut guards = Vec::new();
        for _ in 0..4 {
            guards.push(lim.admit(100.into(), |_| {}).await);
        }
        assert_eq!(lim.inner.lock().unwrap().docker_inflight, 4);

        // A 5th caller is refused by the ceiling, not the budget — it must park
        // rather than being admitted.
        let fifth = lim.admit(100.into(), |_| {});
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            _ = fifth => panic!("admitted a 5th job over the auto hard ceiling"),
        }

        // Freeing one of the four lets a new admission back in.
        guards.pop();
        let guard = lim.admit(100.into(), |_| {}).await;
        assert_eq!(lim.inner.lock().unwrap().docker_inflight, 4);
        drop(guard);
        drop(guards);
    }

    // --- a profile granted no daemon (issue #43) ---------------------------

    #[test]
    fn a_class_without_a_daemon_pays_for_one_container() {
        // Without this the ledger holds the whole profiling ceiling against a
        // container faber never starts — and holds it forever, because no dind
        // sample is ever taken for such a class and the learner only lowers a cap
        // on evidence.
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert_eq!(lim.reservation_for("repo::1::build", true).mib, 8192);
        assert_eq!(lim.reservation_for("repo::1::build", false).mib, 4096);
        let (limits, _) = lim.limits_for("repo::1::build", false);
        assert_eq!(limits.runner.memory_mib, 4096);
        // Zero emits no `--memory` flag at all, which is the right shape for a
        // container that does not exist.
        assert_eq!(limits.dind.memory_mib, 0);
    }

    #[test]
    fn a_cohort_without_daemons_cannot_inflate_a_dind_ceiling() {
        // The cohort is one profile's queue, so every class in it shares a grant.
        // A heavy peer may still raise the *runner* ceiling — that is the whole
        // point of cohort sizing — but there is no dind for it to raise.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        lim.record_run("repo::1::light", "repo · ci · light", &report(300, 200));
        lim.record_run("repo::1::heavy", "repo · ci · heavy", &report(2000, 1500));
        let cohort = vec!["repo::1::light".to_string(), "repo::1::heavy".to_string()];
        let sizing = lim.sizing_for("repo::1::light", &cohort, false);
        assert_eq!(sizing.limits.runner.memory_mib, 3000, "2000 × 1.5");
        assert_eq!(sizing.limits.dind.memory_mib, 0);
        // Charged for its own runner alone (300 × 1.5), not for the peer's.
        assert_eq!(sizing.reservation.mib, 450);
    }

    #[test]
    fn a_run_without_a_daemon_learns_a_zero_dind_cap() {
        // The panel is the one place an operator reasons about the budget from, so
        // a cap for a container that does not exist is a lie there even once it
        // stops deciding anything. `MIN_CAP_MIB` would otherwise floor it at 256.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        lim.record_run(
            "repo::1::build",
            "repo · ci · build",
            &RunReport {
                job_ok: true,
                runner_peak_mib: Some(1000),
                dind_peak_mib: None,
                dind: Some(false),
                ..Default::default()
            },
        );
        let view = lim.snapshot();
        let entry = view
            .classes
            .iter()
            .find(|c| c.key == "repo::1::build")
            .unwrap();
        assert_eq!(entry.runner_mib, 1500, "1000 × 1.5");
        assert_eq!(entry.dind_mib, 0);
    }

    #[test]
    fn a_report_that_never_reached_a_container_does_not_zero_a_dind_cap() {
        // `dind: None` is "no job ran", not "no daemon" — a claim reclaimed
        // mid-ramp reports zero ceilings too, and reading that as a denial would
        // wipe a real learned cap on the next reclaim.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        lim.record_run("repo::1::build", "repo · ci · build", &report(1000, 400));
        lim.record_run(
            "repo::1::build",
            "repo · ci · build",
            &RunReport {
                reclaimed: true,
                ..Default::default()
            },
        );
        let view = lim.snapshot();
        let entry = view
            .classes
            .iter()
            .find(|c| c.key == "repo::1::build")
            .unwrap();
        assert_eq!(entry.dind_mib, 600, "400 × 1.5, untouched");
    }

    #[test]
    fn learns_cap_from_peak_times_margin() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        lim.record_run("repo::1::build", "repo · ci · build", &report(1000, 400));
        // 1000 × 1.5 = 1500, 400 × 1.5 = 600.
        assert_eq!(lim.reservation_for("repo::1::build", true).mib, 2100);
        let (_, profiling) = lim.limits_for("repo::1::build", true);
        assert!(!profiling);
    }

    #[test]
    fn rolling_max_lowers_slowly() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        let k = "repo::1::build";
        lim.record_run(k, "l", &report(2000, 100)); // high
        lim.record_run(k, "l", &report(100, 100)); // window still holds 2000
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 3000); // 2000×1.5
                                                                       // window=3; push two more lows to age the 2000 out.
        lim.record_run(k, "l", &report(100, 100));
        lim.record_run(k, "l", &report(100, 100));
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 256); // 100×1.5=150 → MIN floor
    }

    #[test]
    fn oom_force_bumps_the_cap() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        let k = "repo::1::build";
        lim.record_run(k, "l", &report(1000, 400)); // runner cap → 1500
        let oom = RunReport {
            job_ok: false,
            oom_runner: true,
            runner_peak_mib: Some(1500),
            dind_peak_mib: Some(400),
            ..Default::default()
        };
        lim.record_run(k, "l", &oom);
        // Censored sample = cap × OOM_BUMP = 2250 → ×margin 1.5 = 3375.
        assert!(lim.limits_for(k, true).0.runner.memory_mib >= 3375);
    }

    // --- ADR-0011 ---------------------------------------------------------

    #[test]
    fn adoption_charges_the_ledger_with_no_gate_and_releases_like_any_claim() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        // Budget deliberately left unprobed (0): Gate 1 would refuse everything,
        // and adoption must charge anyway — the container already holds the
        // memory, so refusing here would free nothing and only make the ledger
        // lie about a host that is genuinely full.
        let guard = lim.adopt(3000.into());
        assert_eq!(lim.inner.lock().unwrap().reserved_mib, 3000);
        assert_eq!(lim.inner.lock().unwrap().docker_inflight, 1);

        // With a real budget, a new claim now queues behind an honest number
        // rather than behind a zero.
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096
        lim.recompute_budget();
        assert!(
            lim.try_admit(NO_TICKET, 2000).is_err(),
            "3000 + 2000 > 4096"
        );
        assert!(lim.try_admit(NO_TICKET, 1000).is_ok());

        drop(guard);
        assert_eq!(lim.inner.lock().unwrap().reserved_mib, 1000);
    }

    /// ADR-0046 §7: a shared service server's memory debits the Docker budget
    /// while it runs, takes no job slot, and comes back on drop.
    #[test]
    fn a_service_hold_debits_the_budget_without_taking_a_job_slot() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096
        lim.recompute_budget();
        let hold = lim.hold_service(2048);
        {
            let inner = lim.inner.lock().unwrap();
            assert_eq!(inner.reserved_mib, 2048);
            assert_eq!(inner.docker_inflight, 0, "a server is not a job");
        }
        assert!(
            lim.try_admit(NO_TICKET, 3000).is_err(),
            "2048 + 3000 > 4096: admission sees the server's memory as spent"
        );
        assert!(lim.try_admit(NO_TICKET, 2000).is_ok());
        drop(hold);
        assert_eq!(lim.inner.lock().unwrap().reserved_mib, 2000);
    }

    #[test]
    fn admission_is_held_shut_until_adoption_settles() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        // Everything else is open, so only the hold can be refusing.
        assert!(lim.try_admit(NO_TICKET, 100).is_ok());

        lim.hold_for_adoption();
        assert!(lim.awaiting_adoption());
        assert!(
            lim.try_admit(NO_TICKET, 100).is_err(),
            "a host that has not yet said what is running must admit nothing"
        );

        lim.adoption_settled();
        assert!(!lim.awaiting_adoption());
        assert!(lim.try_admit(NO_TICKET, 100).is_ok());
    }

    #[test]
    fn a_limiter_is_not_born_waiting_for_an_adoption_that_never_comes() {
        // Hosts with no Docker profile and every test construct a Limiter without an
        // adoption pass; defaulting the hold to *on* would deadlock them.
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert!(!lim.awaiting_adoption());
    }

    #[test]
    fn gate1_blocks_when_ledger_full() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget = 6144 − 2048 = 4096
        lim.recompute_budget();
        // available unknown → Gate 2 open; Gate 1 alone.
        assert!(lim.try_admit(NO_TICKET, 4000).is_ok());
        assert!(lim.try_admit(NO_TICKET, 200).is_err()); // 4000 + 200 > 4096
    }

    #[test]
    fn gate2_freeze_band_hysteresis() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        // available just below headroom → freeze.
        lim.available_mib.store(900, Ordering::SeqCst);
        assert!(lim.try_admit(NO_TICKET, 100).is_err());
        // climbs to headroom but not headroom+band → stays frozen.
        lim.available_mib.store(1024, Ordering::SeqCst);
        assert!(lim.try_admit(NO_TICKET, 100).is_err());
        // clears the band → admits.
        lim.available_mib.store(4000, Ordering::SeqCst);
        assert!(lim.try_admit(NO_TICKET, 100).is_ok());
    }

    /// The regression this gate exists for: a crashed Docker VM left `budget`
    /// holding its last good value while the dead jobs' guards drained
    /// `reserved_mib` back to zero, and Gate 2 was skipped for want of a probe —
    /// so admission looked wide open and minted claims into a daemon that was
    /// never coming back.
    #[test]
    fn gate0_refuses_admission_when_docker_unreachable() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        // Exactly the post-crash shape: healthy stale budget, empty ledger, no
        // availability reading at all.
        lim.available_mib.store(AVAILABLE_UNKNOWN, Ordering::SeqCst);
        assert!(
            lim.try_admit(NO_TICKET, 1024).is_ok(),
            "a reachable daemon still admits"
        );

        lim.probe_misses.store(UNHEALTHY_AFTER, Ordering::SeqCst);
        assert!(!lim.docker_reachable());
        assert!(
            lim.try_admit(NO_TICKET, 1024).is_err(),
            "unreachable Docker must refuse even with budget to spare"
        );
        assert!(!lim.snapshot().docker_reachable);
    }

    #[test]
    fn gate0_refuses_while_the_linux_vm_is_not_in_service() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        // The probe is green, but the engine driver says its VM is booting: the
        // Docker faber would admit into is not served yet (ADR-0026).
        lim.set_engine_fault(Some("faber's Linux VM is booting (ADR-0026)".into()));
        assert!(!lim.docker_reachable());
        assert!(lim.try_admit(NO_TICKET, 1024).is_err());
        assert_eq!(
            lim.engine_fault().as_deref(),
            Some("faber's Linux VM is booting (ADR-0026)")
        );
        lim.set_engine_fault(None);
        assert!(lim.docker_reachable());
        assert!(lim.try_admit(NO_TICKET, 1024).is_ok());
    }

    #[test]
    fn gate0_tolerates_a_transient_probe_miss() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        // A blip short of the run still counts as reachable — a single missing
        // image or slow probe must not stall CI.
        lim.probe_misses
            .store(UNHEALTHY_AFTER - 1, Ordering::SeqCst);
        assert!(lim.docker_reachable());
        assert!(lim.try_admit(NO_TICKET, 1024).is_ok());
    }

    #[test]
    fn gate0_reopens_on_recovery() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(20480, Ordering::SeqCst);
        lim.recompute_budget();
        lim.probe_misses
            .store(UNHEALTHY_AFTER + 5, Ordering::SeqCst);
        assert!(lim.try_admit(NO_TICKET, 1024).is_err());
        // One good probe clears the whole streak, however long the outage ran.
        lim.probe_misses.store(0, Ordering::SeqCst);
        assert!(lim.docker_reachable());
        assert!(lim.try_admit(NO_TICKET, 1024).is_ok());
    }

    #[test]
    fn escalation_holds_and_warns() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096, threshold 2048
        lim.recompute_budget();
        let k = "repo::1::heavy";
        // peak 2000 → want 3000 reservation on runner alone > 2048 threshold.
        lim.record_run(k, "heavy", &report(2000, 200));
        let (limits, _) = lim.limits_for(k, true);
        // Held at min(want, ceiling): the want (3000) is under the 4096 ceiling, so
        // the clamp leaves it alone — the guard must not round it *up* to 4096.
        assert_eq!(limits.runner.memory_mib, 3000);
        assert_eq!(limits.dind.memory_mib, 300);
        assert!(!lim.snapshot().warnings.is_empty());
        assert_eq!(lim.snapshot().classes[0].status, CapStatus::Warned);
    }

    #[test]
    fn escalation_clamps_to_ceiling_without_inflating_the_other_container() {
        // The live regression: a runner that genuinely wants more than the ceiling
        // is clamped to it, while a dind that wants almost nothing keeps its small
        // cap. The old guard assigned the ceiling to *both*, so this class reserved
        // 2 × ceiling and starved the pool. The peak stays under the ceiling, as a
        // capped run's must: a peak above it is proof the ceiling is too low
        // (ADR-0053), and grows the cap instead.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(10240, Ordering::SeqCst); // budget 8192, threshold 4096
        lim.recompute_budget();
        let k = "repo::1::visual";
        lim.record_run(k, "visual", &report(4000, 40)); // want 6000 + 60 > threshold
        let (limits, _) = lim.limits_for(k, true);
        assert_eq!(limits.runner.memory_mib, 4096); // clamped to the ceiling
        assert_eq!(limits.dind.memory_mib, 256); // 60 → MIN floor, NOT the ceiling
        assert_eq!(lim.reservation_for(k, true).mib, 4352);
        assert_eq!(lim.snapshot().classes[0].status, CapStatus::Warned);
    }

    #[test]
    fn escalation_never_raises_the_reservation_it_refused() {
        // Property the old code violated: whatever the guard grants must never
        // exceed what it refused. Observed live: the grant came out larger than the
        // want it had refused.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(22528, Ordering::SeqCst); // budget 20480, threshold 10240
        lim.recompute_budget();
        for (i, (runner, dind)) in [(7000u64, 30u64), (9000, 300), (6900, 100)]
            .into_iter()
            .enumerate()
        {
            let k = format!("repo::1::c{i}");
            lim.record_run(&k, &k, &report(runner, dind));
            let want = (runner as f64 * 1.5).ceil() as u64 + (dind as f64 * 1.5).ceil() as u64;
            assert!(
                lim.reservation_for(&k, true).mib <= want,
                "class {k}: granted {} MiB for a want of {want} MiB",
                lim.reservation_for(&k, true).mib
            );
        }
    }

    #[test]
    fn seed_clamps_replayed_history_to_the_ceiling() {
        // Replay runs before the first budget probe, so the escalation branch is
        // inert — the ceiling clamp is the only bound, and it must apply. The old
        // `seed` wrote peak × margin straight through, so a restart could seed a cap
        // (13500 MiB here) that no live run would ever have been granted.
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert_eq!(lim.budget_mib.load(Ordering::SeqCst), 0, "budget unprimed");
        lim.seed("repo::1::visual", "visual", Some(9000), Some(40));
        let (limits, _) = lim.limits_for("repo::1::visual", true);
        assert_eq!(limits.runner.memory_mib, 4096); // clamped, not 13500
        assert_eq!(limits.dind.memory_mib, 256);
    }

    #[test]
    fn seed_keeps_the_profiling_default_for_an_unsampled_container() {
        // Only a runner peak replayed: dind must keep its profiling default rather
        // than collapse to the MIN floor (which would OOM dind on ordinary startup).
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.seed("repo::1::build", "build", Some(1000), None);
        let (limits, _) = lim.limits_for("repo::1::build", true);
        assert_eq!(limits.runner.memory_mib, 1500);
        assert_eq!(limits.dind.memory_mib, 4096);
    }

    #[test]
    fn seed_without_samples_stays_profiling() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.seed("repo::1::build", "build", None, None);
        assert_eq!(lim.snapshot().classes[0].status, CapStatus::Profiling);
        assert_eq!(lim.reservation_for("repo::1::build", true).mib, 8192);
    }

    // --- cohort sizing (issue #13) ----------------------------------------
    //
    // A JIT runner is not job-pinned: GitHub hands the freshly-registered runner
    // whichever queued job it likes, so the container must fit the heaviest class
    // it could be given, not the one it was minted for.

    /// A learned cohort, so every class below is `Learned` rather than profiling.
    fn learned(lim: &Arc<Limiter>, key: &str, runner: u64, dind: u64) {
        lim.record_run(key, key, &report(runner, dind));
    }

    #[test]
    fn cohort_ceiling_covers_the_heaviest_queued_peer() {
        // The live failure: a light class was minted while a heavy one sat queued,
        // GitHub gave the heavy job to the light runner, and the kernel killed it.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(40960, Ordering::SeqCst);
        lim.recompute_budget();
        learned(&lim, "repo::1::warm", 200, 100); // → 300 / 256
        learned(&lim, "repo::1::e2e", 2600, 400); // → 3900 / 600

        let cohort = vec!["repo::1::warm".to_string(), "repo::1::e2e".to_string()];
        let s = lim.sizing_for("repo::1::warm", &cohort, true);
        assert_eq!(
            s.limits.runner.memory_mib, 3900,
            "capped for the heavy peer"
        );
        assert_eq!(s.limits.dind.memory_mib, 600);
        // ...but the ledger still debits only our own class: reserving the ceiling
        // per runner would count the one heavy job once per cohort member.
        assert_eq!(s.reservation.mib, 556); // 300 + 256
        assert_eq!(s.raised_by.as_deref(), Some("repo::1::e2e"));
    }

    #[test]
    fn cohort_never_shrinks_a_class_below_its_own_cap() {
        // The heavy class itself must keep its own (larger) cap when the cohort is
        // otherwise light — the ceiling is a max, never an average or a swap.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(40960, Ordering::SeqCst);
        lim.recompute_budget();
        learned(&lim, "repo::1::lint", 200, 100);
        learned(&lim, "repo::1::e2e", 2600, 400);

        let cohort = vec!["repo::1::lint".to_string(), "repo::1::e2e".to_string()];
        let s = lim.sizing_for("repo::1::e2e", &cohort, true);
        assert_eq!(s.limits.runner.memory_mib, 3900);
        assert_eq!(s.limits.dind.memory_mib, 600);
        assert_eq!(s.reservation.mib, 4500);
        assert!(s.raised_by.is_none(), "nobody raised us");
    }

    #[test]
    fn cohort_sizes_the_two_containers_independently() {
        // The dind OOM signature in the issue (`unexpected EOF` on an image pull) is
        // its own failure: a heavy-dind peer must lift dind without dragging the
        // runner up with it, and vice versa.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(40960, Ordering::SeqCst);
        lim.recompute_budget();
        learned(&lim, "repo::1::a", 200, 100); // 300 / 256
        learned(&lim, "repo::1::big_runner", 2000, 100); // 3000 / 256
        learned(&lim, "repo::1::big_dind", 200, 1400); // 300 / 2100

        let cohort = vec![
            "repo::1::a".to_string(),
            "repo::1::big_runner".to_string(),
            "repo::1::big_dind".to_string(),
        ];
        let s = lim.sizing_for("repo::1::a", &cohort, true);
        assert_eq!(s.limits.runner.memory_mib, 3000);
        assert_eq!(s.limits.dind.memory_mib, 2100);
    }

    #[test]
    fn an_unprofiled_peer_lifts_the_cohort_to_the_profiling_ceiling() {
        // A class that has never run would itself launch at the bounded profiling
        // ceiling, so a runner that could be handed it must hold that much too.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(40960, Ordering::SeqCst);
        lim.recompute_budget();
        learned(&lim, "repo::1::lint", 200, 100);

        let cohort = vec![
            "repo::1::lint".to_string(),
            "repo::1::brand_new".to_string(),
        ];
        let s = lim.sizing_for("repo::1::lint", &cohort, true);
        assert_eq!(s.limits.runner.memory_mib, 4096);
        assert_eq!(s.limits.dind.memory_mib, 4096);
        assert_eq!(s.reservation.mib, 556, "reservation is still ours");
        assert!(!s.profiling, "*we* are learned; the peer isn't");
    }

    #[test]
    fn an_empty_cohort_sizes_exactly_as_before() {
        // Startup, an idle repo, or a scan that hasn't landed yet: degrade to the
        // minted-for class's own caps, never to something smaller.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(40960, Ordering::SeqCst);
        lim.recompute_budget();
        learned(&lim, "repo::1::lint", 200, 100);
        let s = lim.sizing_for("repo::1::lint", &[], true);
        assert_eq!(s.limits, lim.limits_for("repo::1::lint", true).0);
        assert_eq!(s.reservation, lim.reservation_for("repo::1::lint", true));

        // And an unknown class with no cohort still profiles at the ceiling.
        let s = lim.sizing_for("repo::1::unknown", &[], true);
        assert!(s.profiling);
        assert_eq!(s.reservation.mib, 8192);
    }

    #[test]
    fn cohort_cannot_re_inflate_a_cap_the_escalation_guard_clamped() {
        // Prior art: a guard bug that assigned the ceiling to *both* containers
        // doubled the reservations it meant to hold back. Cohort sizing maxes over
        // each class's *stored* (post-clamp) cap, never over its raw `want`, so a
        // flagged class can't smuggle its refused want into its neighbours.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(10240, Ordering::SeqCst); // budget 8192, threshold 4096
        lim.recompute_budget();
        learned(&lim, "repo::1::lint", 200, 100);
        // Peak under the ceiling, as a capped run's must be (see ADR-0053).
        lim.record_run("repo::1::hog", "hog", &report(4000, 40)); // want 6000 + 60
        assert_eq!(lim.snapshot().classes[0].status, CapStatus::Warned);

        let cohort = vec!["repo::1::lint".to_string(), "repo::1::hog".to_string()];
        let s = lim.sizing_for("repo::1::lint", &cohort, true);
        assert_eq!(s.limits.runner.memory_mib, 4096, "the clamp, not the want");
        assert_eq!(
            s.limits.dind.memory_mib, 256,
            "hog's dind stayed at the floor"
        );
    }

    #[test]
    fn oom_censors_at_the_cap_the_container_actually_ran_under() {
        // With cohort sizing the applied ceiling can exceed the class's own cap. An
        // OOM is then evidence of a peak ≥ *that* ceiling; censoring at the class's
        // own cap would "learn" a value the job has already been shown to exceed,
        // and the very next run would OOM again.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(40960, Ordering::SeqCst);
        lim.recompute_budget();
        let k = "repo::1::light";
        learned(&lim, k, 200, 100); // own cap 300 / 256
        lim.record_run(
            k,
            "light",
            &RunReport {
                job_ok: false,
                oom_runner: true,
                runner_peak_mib: None,
                dind_peak_mib: Some(100),
                applied_runner_mib: 1200, // launched at the cohort ceiling
                applied_dind_mib: 600,
                ..Default::default()
            },
        );
        // 1200 × OOM_BUMP 1.5 = 1800, × margin 1.5 = 2700. Censoring at our own
        // 300 MiB cap would have landed on 675 — below the peak just demonstrated.
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 2700);
    }

    // --- ADR-0053: an OOM at the profiling ceiling grows the cap ------------

    /// A run whose `runner` (or `dind`) was OOM-killed at the cap it ran under.
    fn oom_run(runner_oom: bool, dind_oom: bool, runner: u64, dind: u64) -> RunReport {
        RunReport {
            job_ok: false,
            oom_runner: runner_oom,
            oom_dind: dind_oom,
            runner_peak_mib: Some(runner),
            dind_peak_mib: Some(dind),
            ..Default::default()
        }
    }

    /// A limiter whose budget is `budget_mib` (reserve 2048 on top), threshold half.
    fn budgeted(c: AutoLimitConfig, budget_mib: u64) -> Arc<Limiter> {
        let lim = Limiter::new(c, &HashMap::new());
        lim.mem_total_mib.store(budget_mib + 2048, Ordering::SeqCst);
        lim.recompute_budget();
        assert_eq!(lim.budget_mib.load(Ordering::SeqCst), budget_mib);
        lim
    }

    #[test]
    fn an_oom_at_the_profiling_ceiling_grows_the_cap_past_it() {
        // The live failure (#304): killed at the ceiling on every run, because the
        // bumped want was clamped straight back to the ceiling it was killed at.
        let lim = budgeted(cfg(), 20480); // threshold 10240
        let k = "repo::1::ci";
        lim.record_run(k, "ci", &report(3000, 100)); // want 4500 → clamped 4096
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 4096);
        lim.record_run(k, "ci", &oom_run(true, false, 4090, 100));
        // 4096 × OOM_BUMP 1.5 = 6144, × margin 1.5 = 9216: past the 4096 ceiling,
        // within the 10240 threshold, so it is granted outright.
        let (limits, _) = lim.limits_for(k, true);
        assert_eq!(limits.runner.memory_mib, 9216);
        assert_eq!(
            limits.dind.memory_mib, 256,
            "dind is not dragged up with it"
        );
        let snap = lim.snapshot();
        assert_eq!(snap.classes[0].status, CapStatus::Learned);
        assert!(snap.oom_held.is_empty(), "nothing to ask: it fits");
        assert!(snap.warnings.is_empty());
    }

    #[test]
    fn a_want_above_the_ceiling_without_an_oom_still_clamps() {
        // The dind-inflation guard stays: a measured peak is not evidence the
        // ceiling is too low, only an OOM is.
        let lim = budgeted(cfg(), 20480); // threshold 10240
        let k = "repo::1::big";
        lim.record_run(k, "big", &report(3000, 3000)); // wants 4500 + 4500, no OOM
        let (limits, _) = lim.limits_for(k, true);
        assert_eq!(limits.runner.memory_mib, 4096);
        assert_eq!(limits.dind.memory_mib, 4096);
        let snap = lim.snapshot();
        assert_eq!(snap.classes[0].status, CapStatus::Learned);
        assert!(snap.oom_held.is_empty());
    }

    #[test]
    fn the_held_pin_raises_only_proven_containers_and_stops_at_the_budget() {
        // Runner proven: to its want; dind keeps its cap.
        assert_eq!(
            held_pin((5888, 256), (true, false), (9216, 150), 16384),
            (9216, 256)
        );
        // Past the budget: the runner takes what dind leaves.
        assert_eq!(
            held_pin((5888, 256), (true, false), (20000, 150), 12288),
            (12032, 256)
        );
        // Both proven and past it: the budget shared in proportion to the wants
        // (9000 : 6000), never below either cap.
        assert_eq!(
            held_pin((4000, 2000), (true, true), (9000, 6000), 12288),
            (7372, 4916)
        );
        // Dind proven alone: dind to its want.
        assert_eq!(
            held_pin((3000, 3000), (false, true), (100, 7000), 16384),
            (3000, 7000)
        );
    }

    #[test]
    fn an_oom_past_the_threshold_holds_there_warns_and_asks_the_owner() {
        let lim = budgeted(cfg(), 12288); // threshold 6144
        let k = "repo::1::ci";
        lim.record_run(k, "owner/demo · ci · test", &report(3000, 100));
        lim.record_run(
            k,
            "owner/demo · ci · test",
            &oom_run(true, false, 4090, 100),
        );
        // Wants 9216 + 150, past 6144: the runner is held at 6144 − dind's 256.
        let (limits, _) = lim.limits_for(k, true);
        assert_eq!(limits.runner.memory_mib, 5888);
        assert_eq!(limits.dind.memory_mib, 256);
        assert_eq!(
            lim.reservation_for(k, true).mib,
            6144,
            "exactly the threshold"
        );
        let snap = lim.snapshot();
        assert_eq!(snap.classes[0].status, CapStatus::Warned);
        assert!(
            snap.warnings[0].contains("outgrew the profiling ceiling"),
            "{:?}",
            snap.warnings
        );
        assert!(snap.warnings[0].contains("pin it higher or make the job lighter"));
        assert_eq!(
            snap.oom_held,
            vec![OomHeldView {
                key: k.to_string(),
                label: "owner/demo · ci · test".to_string(),
                container: "runner",
                cap_mib: 5888,
                reservation_mib: 6144,
                pin_runner_mib: 9216,
                pin_dind_mib: 256,
                ooms: 1,
                runs: 2,
            }]
        );
        // The next OOM, at the held cap, does not grow it further.
        lim.record_run(
            k,
            "owner/demo · ci · test",
            &oom_run(true, false, 5880, 100),
        );
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 5888);
        assert_eq!(lim.snapshot().oom_held[0].ooms, 2);
        // An OOM at the held cap bumps the want past the whole budget; the Pin it
        // offers stops at the budget, which no pin can be admitted past.
        let held = &lim.snapshot().oom_held[0];
        assert_eq!(held.pin_runner_mib + held.pin_dind_mib, 12288, "{held:?}");
        // The popover's Pin takes the view's figures: the class runs at them, and
        // the owner is no longer asked.
        let held = lim.snapshot().oom_held[0].clone();
        lim.pin(
            k,
            container_limits(held.pin_runner_mib, held.pin_dind_mib, 0.0),
        );
        let (limits, _) = lim.limits_for(k, true);
        assert_eq!(limits.runner.memory_mib, held.pin_runner_mib);
        assert!(held.pin_runner_mib > 5888, "{held:?}");
        assert!(lim.snapshot().oom_held.is_empty());
    }

    #[test]
    fn an_oom_grown_cap_keeps_runner_and_dind_within_the_threshold() {
        let lim = budgeted(cfg(), 20480); // threshold 10240
        let k = "repo::1::ci";
        // dind wants 3000 and stays under the ceiling; the runner OOMs at 4096.
        lim.record_run(k, "ci", &report(3000, 2000));
        lim.record_run(k, "ci", &oom_run(true, false, 4090, 2000));
        let (limits, _) = lim.limits_for(k, true);
        assert_eq!(limits.dind.memory_mib, 3000);
        assert_eq!(limits.runner.memory_mib, 7240, "10240 − dind's 3000");
        assert_eq!(lim.reservation_for(k, true).mib, 10240);

        // Both containers OOM-proven: the threshold is shared by want, and the sum
        // still fits it.
        let both = "repo::1::both";
        lim.record_run(both, "both", &report(3000, 3000));
        lim.record_run(both, "both", &oom_run(true, true, 4090, 4090));
        let (limits, _) = lim.limits_for(both, true);
        assert_eq!(limits.runner.memory_mib, 5120);
        assert_eq!(limits.dind.memory_mib, 5120);
        assert!(lim.reservation_for(both, true).mib <= 10240);
    }

    #[test]
    fn an_oom_grown_cap_never_drops_below_the_profiling_ceiling() {
        // A threshold below 2 × the ceiling leaves an OOMing runner less room than
        // the clamp already gave it: growth must not become a shrink.
        let lim = budgeted(cfg(), 12288); // threshold 6144
        let k = "repo::1::ci";
        lim.record_run(k, "ci", &report(3000, 3000)); // 4096 / 4096
        lim.record_run(k, "ci", &oom_run(true, false, 4090, 3000));
        let (limits, _) = lim.limits_for(k, true);
        assert_eq!(
            limits.runner.memory_mib, 4096,
            "held, never below the clamp"
        );
        assert_eq!(limits.dind.memory_mib, 4096);
    }

    #[test]
    fn a_grown_cap_does_not_fall_back_once_its_ooms_age_out() {
        // Review finding on ADR-0053: with only OOMs as proof, a grown cap snapped
        // back to the ceiling once every OOM left the window, though the window
        // then held measured peaks above it — so it OOMed again once per window.
        let mut c = cfg();
        c.window = 20;
        let lim = budgeted(c, 20480); // threshold 10240
        let k = "repo::1::ci";
        lim.record_run(k, "ci", &oom_run(true, false, 4090, 100));
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 9216);
        for _ in 0..25 {
            lim.record_run(k, "ci", &report(5000, 100)); // above the 4096 ceiling
        }
        // Every OOM is out of the window; the measured 5000 × margin 1.5 holds.
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 7500);
        let snap = lim.snapshot();
        assert_eq!(snap.classes[0].status, CapStatus::Learned);
        assert!(snap.oom_held.is_empty());
    }

    #[test]
    fn only_a_recent_oom_asks_the_owner() {
        // The cap stays held while the old OOM is in the window, but once the last
        // RECENT_OOM_RUNS runs passed at the held cap there is nothing failing to
        // put to anyone.
        let mut c = cfg();
        c.window = 10;
        let lim = budgeted(c, 12288); // threshold 6144
        let k = "repo::1::ci";
        lim.record_run(k, "ci", &oom_run(true, false, 4090, 100));
        assert_eq!(lim.snapshot().oom_held.len(), 1);
        for _ in 0..RECENT_OOM_RUNS {
            lim.record_run(k, "ci", &report(5000, 100));
        }
        let snap = lim.snapshot();
        assert_eq!(
            snap.classes[0].status,
            CapStatus::Warned,
            "the OOM still holds it up"
        );
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 5888);
        assert!(snap.oom_held.is_empty(), "{:?}", snap.oom_held);
    }

    #[test]
    fn a_pin_clears_the_oom_hold() {
        let lim = budgeted(cfg(), 12288);
        let k = "repo::1::ci";
        lim.record_run(k, "ci", &oom_run(true, false, 4090, 100));
        assert_eq!(lim.snapshot().oom_held.len(), 1);
        lim.pin(k, container_limits(9000, 256, 0.0));
        assert!(lim.snapshot().oom_held.is_empty());
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 9000);
        lim.record_run(k, "ci", &oom_run(true, false, 8990, 100));
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 9000, "pinned");
        assert!(lim.snapshot().oom_held.is_empty());
    }

    #[test]
    fn a_replayed_oom_grows_once_the_budget_is_measured() {
        // Replay runs with no budget, so it can only clamp; the first measured
        // budget re-evaluates, or the first live run would die at the ceiling again.
        let lim = Limiter::new(cfg(), &HashMap::new());
        let k = "repo::1::ci";
        lim.seed_censored(k, "ci", (Some(4090), true), (Some(100), false));
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 4096);
        lim.mem_total_mib.store(20480 + 2048, Ordering::SeqCst);
        lim.recompute_budget();
        // 4090 × margin 1.5 = 6135: past the ceiling, under the 10240 threshold.
        assert_eq!(lim.limits_for(k, true).0.runner.memory_mib, 6135);
        assert_eq!(lim.snapshot().classes[0].status, CapStatus::Learned);
    }

    #[test]
    fn head_of_queue_reservation_is_set_aside_for_the_oldest_waiter() {
        // budget 4096. A 3000 MiB request parks first; a 2000 MiB one must not take
        // room the head needs, even though it would fit on its own.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        let head = lim.take_ticket(3000);
        let junior = lim.take_ticket(2000);
        // 1500 already reserved leaves 2596 — enough for the junior alone, but it
        // would strand the head, so it is refused.
        assert!(lim.try_admit(head.id, 1500).is_ok());
        assert!(lim.try_admit(junior.id, 2000).is_err());
        // The head itself is still free to take what it set aside.
        assert!(lim.try_admit(head.id, 2500).is_ok());
    }

    #[test]
    fn a_younger_priority_waiter_is_the_head_ahead_of_an_older_normal_one() {
        // ADR-0036. budget 4096. A normal 3000 MiB claim parked first, then a
        // priority 2000 MiB one. The priority claim is the head, so the older
        // normal claim is the one set aside around it.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        let normal = lim.take_ticket(3000);
        let prio = lim.take_ticket_ranked(2000, Rank::Priority);
        // 1500 reserved leaves 2596: the normal claim fits alone, but would strand
        // the priority head's 2000, so it is refused and says why.
        assert!(lim.try_admit(prio.id, 1500).is_ok());
        let wait = lim.try_admit(normal.id, 1000).unwrap_err();
        assert_eq!(
            wait.view().detail,
            "needs 1000 MiB, 2596 MiB free of 4096 MiB, 2000 MiB set aside for a priority claim"
        );
        // The head itself takes what it set aside.
        assert!(lim.try_admit(prio.id, 2000).is_ok());
    }

    #[test]
    fn two_priority_waiters_stay_first_come_first_served() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        let _normal = lim.take_ticket(500);
        let first = lim.take_ticket_ranked(3000, Rank::Priority);
        let second = lim.take_ticket_ranked(2000, Rank::Priority);
        // The older priority claim is the head: the younger one is set aside
        // around it, and is told it is an older (same-tier) claim.
        assert!(lim.try_admit(first.id, 1500).is_ok());
        let wait = lim.try_admit(second.id, 2000).unwrap_err();
        assert_eq!(
            wait.view().detail,
            "needs 2000 MiB, 2596 MiB free of 4096 MiB, 3000 MiB set aside for an older claim"
        );
        assert!(lim.try_admit(first.id, 2500).is_ok());
    }

    #[test]
    fn a_demoted_claim_ranks_below_a_younger_normal_one() {
        // ADR-0063 §5. budget 4096. A claim demoted from ticket 0 is older than the
        // normal claim behind it, but the normal one is the head.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        let first = lim.take_ticket(3000);
        let ticket = first.id;
        drop(first);
        let normal = lim.take_ticket(2000);
        let wasted = lim.take_ticket_ranked(3000, Rank::Waste { ticket });
        assert_eq!(wasted.id, ticket, "a demote keeps its ticket");
        // 1500 reserved leaves 2596: the demoted claim fits alone, but would
        // strand the normal head's 2000.
        assert!(lim.try_admit(normal.id, 1500).is_ok());
        let wait = lim.try_admit(wasted.id, 1000).unwrap_err();
        assert_eq!(
            wait.view().detail,
            "needs 1000 MiB, 2596 MiB free of 4096 MiB, 2000 MiB set aside for an older claim"
        );
        assert!(lim.try_admit(normal.id, 2000).is_ok());
    }

    #[test]
    fn demoted_claims_keep_their_ticket_order() {
        // Two claims demoted in the opposite order to the one they queued in: the
        // older ticket is still the head of the waste tier.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        let (old, young) = (lim.take_ticket(3000), lim.take_ticket(2000));
        let (old_id, young_id) = (old.id, young.id);
        drop((old, young));
        let young = lim.take_ticket_ranked(2000, Rank::Waste { ticket: young_id });
        let old = lim.take_ticket_ranked(3000, Rank::Waste { ticket: old_id });
        assert!(lim.try_admit(old.id, 1500).is_ok());
        let wait = lim.try_admit(young.id, 2000).unwrap_err();
        assert_eq!(
            wait.view().detail,
            "needs 2000 MiB, 2596 MiB free of 4096 MiB, 3000 MiB set aside for an older claim"
        );
        // With no other claim parked, the waste tier's head is admitted.
        assert!(lim.try_admit(old.id, 2500).is_ok());
    }

    #[tokio::test]
    async fn a_demote_returns_the_reservation_and_requeues_on_its_ticket() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        lim.set_docker_reachable_for_test(true);
        let res = Reservation::from(1000);
        let guard = lim.admit_ranked(res, Rank::Normal, |_| {}).await;
        let ticket = guard.ticket().unwrap();
        assert_eq!(lim.inner.lock().unwrap().reserved_mib, 1000);
        drop(guard);
        assert_eq!(lim.inner.lock().unwrap().reserved_mib, 0);
        let again = lim.admit_ranked(res, Rank::Waste { ticket }, |_| {}).await;
        assert_eq!(again.ticket(), Some(ticket));
        drop(again);
        let inner = lim.inner.lock().unwrap();
        assert!(inner.waste_tickets.is_empty() && inner.waiters.is_empty());
    }

    #[test]
    fn priority_repos_match_case_insensitively_and_empty_matches_nothing() {
        let lim = Limiter::new(cfg(), &HashMap::new()); // cfg(): ["Acme/Tooling"]
        assert!(lim.is_priority_repo("acme/tooling"));
        assert!(lim.is_priority_repo("ACME/TOOLING"));
        assert!(!lim.is_priority_repo("acme/app"));
        let mut empty = cfg();
        empty.priority_repos.clear();
        let lim = Limiter::new(empty, &HashMap::new());
        assert!(!lim.is_priority_repo("acme/tooling"));
    }

    #[test]
    fn cancelled_priority_waiter_stops_outranking() {
        // The priority set must shrink with the queue, or a vanished priority
        // claim would keep setting room aside forever.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        let normal = lim.take_ticket(2000);
        {
            let _prio = lim.take_ticket_ranked(3000, Rank::Priority);
            assert!(lim.try_admit(normal.id, 1500).is_err()); // 1500 + 3000 > 4096
        }
        {
            let inner = lim.inner.lock().unwrap();
            assert!(inner.priority_tickets.is_empty());
            assert_eq!(inner.waiters.len(), 1);
        }
        assert!(lim.try_admit(normal.id, 1500).is_ok());
    }

    #[test]
    fn junior_still_fills_room_beyond_the_head() {
        // Fairness must not idle the pool: a junior that fits *alongside* the head's
        // set-aside is admitted immediately.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096
        lim.recompute_budget();
        let head = lim.take_ticket(3000);
        let junior = lim.take_ticket(1000);
        assert!(lim.try_admit(junior.id, 1000).is_ok()); // 1000 + 3000 set-aside == budget
        assert!(lim.try_admit(junior.id, 200).is_err()); // 1000 + 200 + 3000 > budget
        drop(head);
        assert!(lim.try_admit(junior.id, 3000).is_ok()); // head gone → its room is free
    }

    #[test]
    fn an_unsatisfiable_head_is_not_set_aside() {
        // A head that cannot fit the budget at all must not wedge the pool: without
        // this, one over-budget class would block every other job forever.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096
        lim.recompute_budget();
        let _impossible = lim.take_ticket(99_999);
        let junior = lim.take_ticket(1000);
        assert!(lim.try_admit(junior.id, 1000).is_ok());
    }

    #[test]
    fn cancelled_waiter_leaves_the_queue() {
        // The admit future is dropped when a parked claim is reclaimed. If the
        // ticket outlived it, the vanished waiter's set-aside would be held forever.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096
        lim.recompute_budget();
        let junior = {
            let _head = lim.take_ticket(3000);
            let junior = lim.take_ticket(2000);
            assert!(lim.try_admit(junior.id, 2000).is_err()); // blocked by the head
            junior
        };
        assert!(lim.inner.lock().unwrap().waiters.len() == 1);
        assert!(lim.try_admit(junior.id, 2000).is_ok()); // head cancelled → junior proceeds
    }

    #[tokio::test]
    async fn admit_dequeues_on_both_admission_and_cancel() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096
        lim.recompute_budget();
        let guard = lim.admit(1000.into(), |_| {}).await;
        assert!(
            lim.inner.lock().unwrap().waiters.is_empty(),
            "an admitted caller must not stay queued"
        );
        // A second request that cannot fit parks; dropping the future dequeues it.
        let parked = lim.admit(4000.into(), |_| {});
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            _ = parked => panic!("admitted over budget"),
        }
        assert!(
            lim.inner.lock().unwrap().waiters.is_empty(),
            "a cancelled caller must not stay queued"
        );
        drop(guard);
        assert_eq!(lim.inner.lock().unwrap().reserved_mib, 0);
    }

    // --- ADR-0019: CPU-aware admission --------------------------------------
    //
    // Memory alone admitted more heavy jobs than the VM had cores; every container
    // saw every core and sized its worker pools to them, the load average climbed
    // far past the core count, and jobs failed on their own timeouts. CPU is
    // reserved, enforced and learned like memory, and a PSI gate backs the ledger
    // up.

    use crate::config::{CpuConfig, CpuMode};

    /// `cfg()` with CPU admission in `mode` and the default CPU knobs: overcommit
    /// 1.25, one core reserved, a four-core profiling share, pause at 40% pressure
    /// and resume at 25%.
    fn cfg_cpu(mode: CpuMode) -> AutoLimitConfig {
        AutoLimitConfig {
            cpu: CpuConfig {
                mode,
                ..CpuConfig::default()
            },
            ..cfg()
        }
    }

    /// A limiter with room to spare on memory (so only CPU can refuse) and a VM
    /// of `ncpu` cores: a CPU budget of `ncpu × 1250 − 1000` millicores.
    fn cpu_limiter(mode: CpuMode, ncpu: u64) -> Arc<Limiter> {
        let lim = Limiter::new(cfg_cpu(mode), &HashMap::new());
        lim.set_vm_total_for_test(40_960);
        lim.set_ncpu_for_test(ncpu);
        lim
    }

    fn res(mib: u64, mcpu: u64) -> Reservation {
        Reservation {
            mib,
            mcpu,
            disk_mib: DISK_CHARGE_MIB,
            shadow_mib: mib,
        }
    }

    fn usage(peak_mcpu: u64, throttled_ratio: f64) -> Option<CpuUsage> {
        Some(CpuUsage {
            peak_mcpu,
            avg_mcpu: peak_mcpu / 2,
            throttled_ratio,
        })
    }

    /// An unthrottled run with a small memory footprint and these CPU peaks.
    fn cpu_report(runner_peak_mcpu: u64, dind_peak_mcpu: u64) -> RunReport {
        RunReport {
            job_ok: true,
            runner_peak_mib: Some(500),
            dind_peak_mib: Some(100),
            runner_cpu: usage(runner_peak_mcpu, 0.0),
            dind_cpu: usage(dind_peak_mcpu, 0.0),
            ..Default::default()
        }
    }

    fn class_view(lim: &Limiter, key: &str) -> ClassCapView {
        lim.snapshot()
            .classes
            .into_iter()
            .find(|c| c.key == key)
            .expect("class is known")
    }

    #[test]
    fn cpu_gate1_refuses_a_job_over_the_cpu_budget_in_auto() {
        let lim = cpu_limiter(CpuMode::Auto, 4); // 4 × 1250 − 1000 = 4000
        assert!(lim.try_admit(NO_TICKET, res(100, 2500)).is_ok());
        assert!(
            lim.try_admit(NO_TICKET, res(100, 2000)).is_err(),
            "2500 + 2000 > 4000, with memory to spare"
        );
        assert!(
            lim.try_admit(NO_TICKET, res(100, 1500)).is_ok(),
            "exactly full fits"
        );
        let cpu = lim.snapshot().cpu;
        assert_eq!(cpu.mode, "auto");
        assert_eq!(cpu.ncpu, Some(4));
        assert_eq!(cpu.budget_mcpu, 4000);
        assert_eq!(cpu.reserved_mcpu, 4000);
    }

    #[test]
    fn cpu_off_and_unstated_never_gate_but_still_track_the_reservation() {
        // Tracking in every mode is what lets the dashboard show CPU demand before
        // an operator turns the gates on — and an absent key must change nothing.
        for mode in [CpuMode::Off, CpuMode::Unstated] {
            let lim = cpu_limiter(mode, 4);
            lim.set_cpu_pressure_for_test(Some(95.0));
            for _ in 0..3 {
                assert!(
                    lim.try_admit(NO_TICKET, res(100, 3000)).is_ok(),
                    "{mode:?}: neither the CPU ledger nor pressure may refuse"
                );
            }
            let cpu = lim.snapshot().cpu;
            assert_eq!(cpu.mode, mode.as_str());
            assert_eq!(cpu.reserved_mcpu, 9000, "{mode:?}: 3 × 3000 tracked");
            assert!(cpu.frozen, "{mode:?}: the hysteresis is still shown");
        }
    }

    #[tokio::test]
    async fn a_cpu_need_above_the_whole_budget_runs_alone_rather_than_never() {
        let lim = cpu_limiter(CpuMode::Auto, 2); // 2 × 1250 − 1000 = 1500
        let small = lim.admit(res(100, 1000), |_| {}).await;
        // Clamped to the budget, the big job still cannot join a non-empty ledger…
        assert!(lim.try_admit(NO_TICKET, res(100, 5000)).is_err());
        drop(small);
        // …but it is admitted the moment the ledger is empty, and then holds the
        // whole budget by itself.
        let big = lim.admit(res(100, 5000), |_| {}).await;
        assert!(lim.try_admit(NO_TICKET, res(100, 100)).is_err());
        drop(big);
        assert!(lim.try_admit(NO_TICKET, res(100, 100)).is_ok());
    }

    #[test]
    fn a_vm_whose_reserve_eats_the_cpu_budget_admits_one_job_at_a_time() {
        // A known VM with nothing left after the reserve is not an unknown one:
        // it runs CPU work serially instead of not gating at all.
        let lim = Limiter::new(
            AutoLimitConfig {
                cpu: CpuConfig {
                    mode: CpuMode::Auto,
                    reserve_cpus: 8.0,
                    ..CpuConfig::default()
                },
                ..cfg()
            },
            &HashMap::new(),
        );
        lim.set_vm_total_for_test(40_960);
        lim.set_ncpu_for_test(2);
        assert_eq!(lim.snapshot().cpu.budget_mcpu, 0);
        assert!(lim.try_admit(NO_TICKET, res(100, 1000)).is_ok());
        assert!(lim.try_admit(NO_TICKET, res(100, 1000)).is_err());
    }

    #[test]
    fn an_unprobed_vm_skips_the_cpu_ledger() {
        // Memory still bounds admission; refusing on a budget nobody has measured
        // would stall CI on a probe failure.
        let lim = cpu_limiter(CpuMode::Auto, 0);
        assert_eq!(lim.snapshot().cpu.ncpu, None);
        for _ in 0..4 {
            assert!(lim.try_admit(NO_TICKET, res(100, 8000)).is_ok());
        }
    }

    #[test]
    fn the_head_set_aside_protects_a_cpu_heavy_head_from_cpu_small_juniors() {
        // The memory starvation this set-aside was built for, in CPU: a stream of
        // small jobs keeping the CPU ledger just full enough that a large share
        // never fits.
        let lim = cpu_limiter(CpuMode::Auto, 4); // budget 4000
        let running = lim.adopt(res(100, 2000));
        let head = lim.take_ticket(res(100, 2500));
        let junior = lim.take_ticket(res(100, 1500));
        assert!(
            lim.try_admit(junior.id, res(100, 1500)).is_err(),
            "2000 + 1500 fits alone, but would strand the head's 2500"
        );
        assert!(
            lim.try_admit(head.id, res(100, 2500)).is_err(),
            "2000 + 2500 > 4000"
        );
        drop(running);
        assert!(lim.try_admit(head.id, res(100, 2500)).is_ok());
        drop(head);
        assert!(
            lim.try_admit(junior.id, res(100, 1500)).is_ok(),
            "room beyond the head is still filled"
        );
    }

    #[test]
    fn a_head_that_can_never_fit_in_memory_sets_no_cpu_aside() {
        // It is never admitted, so CPU held for it would idle for good — and a
        // clamped set-aside the size of the whole budget would wedge the pool.
        let lim = cpu_limiter(CpuMode::Auto, 4);
        let _impossible = lim.take_ticket(res(99_999, 4000));
        let junior = lim.take_ticket(res(100, 1000));
        assert!(lim.try_admit(junior.id, res(100, 1000)).is_ok());
    }

    #[test]
    fn cpu_pressure_pauses_admission_with_hysteresis_and_fails_open_when_unknown() {
        let lim = cpu_limiter(CpuMode::Auto, 12);
        let small = res(100, 100);
        assert!(
            lim.try_admit(NO_TICKET, small).is_ok(),
            "no reading yet: fail open"
        );

        lim.set_cpu_pressure_for_test(Some(45.0));
        assert!(lim.try_admit(NO_TICKET, small).is_err(), "≥ 40% pauses");
        let cpu = lim.snapshot().cpu;
        assert_eq!(cpu.pressure, Some(45.0));
        assert!(cpu.frozen);

        lim.set_cpu_pressure_for_test(Some(30.0));
        assert!(
            lim.try_admit(NO_TICKET, small).is_err(),
            "30% is inside the band"
        );

        lim.set_cpu_pressure_for_test(Some(25.0));
        assert!(lim.try_admit(NO_TICKET, small).is_ok(), "≤ 25% resumes");

        lim.set_cpu_pressure_for_test(Some(39.0));
        assert!(
            lim.try_admit(NO_TICKET, small).is_ok(),
            "rising inside the band stays open"
        );

        lim.set_cpu_pressure_for_test(Some(40.0));
        assert!(
            lim.try_admit(NO_TICKET, small).is_err(),
            "exactly the pause threshold"
        );

        lim.set_cpu_pressure_for_test(None);
        assert!(
            lim.try_admit(NO_TICKET, small).is_ok(),
            "a lost reading fails open"
        );
        assert!(!lim.snapshot().cpu.frozen, "and does not read as a pause");
    }

    /// #105: a PSI figure that is no percentage — past 100, `inf`, `NaN` — holds
    /// admission, as a figure past 100 always did. Read as a lost signal it would
    /// fail open.
    #[test]
    fn a_psi_figure_that_is_no_percentage_holds_admission() {
        for avg60 in ["150.00", "inf", "NaN"] {
            let lim = cpu_limiter(CpuMode::Auto, 12);
            let line = format!("some avg10=0.00 avg60={avg60} avg300=0.00 total=1\n");
            lim.set_cpu_pressure_for_test(crate::cpu::parse_psi_some_avg60(&line));
            assert!(
                lim.try_admit(NO_TICKET, res(100, 100)).is_err(),
                "{avg60} must hold admission"
            );
            assert!(lim.snapshot().cpu.frozen, "{avg60}");
        }
    }

    #[test]
    fn a_spike_between_admissions_still_freezes_the_gate() {
        // The hysteresis steps on every reading, not only when someone is waiting:
        // a job arriving at 30% after an unwatched spike to 60% must still wait.
        let lim = cpu_limiter(CpuMode::Auto, 12);
        lim.set_cpu_pressure_for_test(Some(60.0));
        lim.set_cpu_pressure_for_test(Some(30.0));
        assert!(lim.try_admit(NO_TICKET, res(100, 100)).is_err());
    }

    #[test]
    fn the_runner_quota_is_the_cohort_max_only_in_auto_and_dind_never_gets_one() {
        let learned_pair = |mode| {
            let lim = cpu_limiter(mode, 12);
            lim.record_run("repo::1::light", "light", &cpu_report(1000, 200)); // 1500 / 300
            lim.record_run("repo::1::heavy", "heavy", &cpu_report(2400, 200)); // 3600 / 300
            lim
        };
        let cohort = vec!["repo::1::light".to_string(), "repo::1::heavy".to_string()];

        let lim = learned_pair(CpuMode::Auto);
        let s = lim.sizing_for("repo::1::light", &cohort, true);
        assert_eq!(s.limits.runner.cpus, 3.6, "sized for the heavy peer");
        assert_eq!(s.limits.dind.cpus, 0.0);
        assert_eq!(
            s.reservation.mcpu, 1800,
            "charged its own shares, not the peer's"
        );
        assert_eq!(
            s.raised_by.as_deref(),
            Some("heavy"),
            "only the quota was raised"
        );
        // Our own share when we are the heaviest, and via `limits_for` too.
        let (own, _) = lim.limits_for("repo::1::heavy", true);
        assert_eq!(own.runner.cpus, 3.6);
        assert_eq!(own.dind.cpus, 0.0);
        // A class nobody has measured would profile at the ceiling, so a runner
        // that could be handed it gets that quota.
        let with_new = vec!["repo::1::light".to_string(), "repo::1::new".to_string()];
        let s = lim.sizing_for("repo::1::light", &with_new, true);
        assert_eq!(s.limits.runner.cpus, 4.0);

        for mode in [CpuMode::Off, CpuMode::Unstated] {
            let lim = learned_pair(mode);
            let s = lim.sizing_for("repo::1::light", &cohort, true);
            assert_eq!(s.limits.runner.cpus, 0.0, "{mode:?}: no quota");
            assert_eq!(s.limits.dind.cpus, 0.0);
            assert_eq!(s.reservation.mcpu, 1800, "{mode:?}: still reserved");
            assert!(s.raised_by.is_none(), "{mode:?}: nothing was raised");
        }
    }

    #[test]
    fn the_runner_quota_never_exceeds_the_vm_and_needs_a_known_vm() {
        // Docker refuses to start a container whose `--cpus` exceeds the VM's
        // cores, so an oversized share must never reach `docker run` as one.
        let lim = cpu_limiter(CpuMode::Auto, 2);
        lim.pin("repo::1::big", pinned(8.0, 0.0));
        assert_eq!(lim.limits_for("repo::1::big", true).0.runner.cpus, 2.0);
        // An unknown VM gives nothing to bound the quota by: none at all.
        lim.set_ncpu_for_test(0);
        assert_eq!(lim.limits_for("repo::1::big", true).0.runner.cpus, 0.0);
    }

    #[test]
    fn cpu_shares_learn_the_unthrottled_peak_times_margin_in_every_mode() {
        // Learned with CPU admission off, so turning it on starts from history.
        let lim = Limiter::new(cfg(), &HashMap::new());
        let k = "repo::1::build";
        lim.record_run(k, "build", &cpu_report(2000, 400));
        let c = class_view(&lim, k);
        assert_eq!(c.runner_mcpu, 3000, "2000 × 1.5");
        assert_eq!(c.dind_mcpu, 600, "400 × 1.5");
        assert_eq!(c.cpu_status, CapStatus::Learned);
        assert_eq!(lim.reservation_for(k, true).mcpu, 3600);
        assert_eq!(
            lim.reservation_for(k, false).mcpu,
            3000,
            "no daemon, no dind share"
        );
    }

    #[test]
    fn cpu_shares_have_floors() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.record_run("repo::1::lint", "lint", &cpu_report(100, 10));
        let c = class_view(&lim, "repo::1::lint");
        assert_eq!(c.runner_mcpu, 1000, "150 → one core");
        assert_eq!(c.dind_mcpu, 100, "15 → the dind floor");
    }

    #[test]
    fn a_run_without_cpu_readings_keeps_the_profiling_shares() {
        // No reading is not a reading of zero: collapsing to the floors here would
        // throttle a heavy class the first time sampling missed.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.record_run("repo::1::build", "build", &report(1000, 400));
        let c = class_view(&lim, "repo::1::build");
        assert_eq!(c.runner_mcpu, 4000);
        assert_eq!(c.dind_mcpu, 1000);
        assert_eq!(c.cpu_status, CapStatus::Profiling);
        assert_eq!(c.status, CapStatus::Learned, "memory learned regardless");
    }

    #[test]
    fn a_share_wanting_more_than_the_ceiling_is_held_there_and_warned() {
        let lim = cpu_limiter(CpuMode::Auto, 12);
        let k = "repo::1::e2e";
        lim.record_run(k, "repo · ci · e2e", &cpu_report(6000, 100)); // wants 9000
        let c = class_view(&lim, k);
        assert_eq!(c.runner_mcpu, 4000, "held at profiling_cpus");
        assert_eq!(c.dind_mcpu, 150);
        assert_eq!(c.cpu_status, CapStatus::Warned);
        let snap = lim.snapshot();
        let warnings = snap.warnings;
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("runner wants 9 cores, held at 4 — pin to allow")),
            "{warnings:?}"
        );
        // The keyed twin: same texts, same order, a stable key per class and learner.
        assert_eq!(snap.warning_items.len(), warnings.len());
        assert_eq!(snap.warning_items[0].key, format!("cpu:{k}"));
        assert_eq!(snap.warning_items[0].text, warnings[0]);

        // With CPU admission off nothing is held, so nothing is said — but the
        // class still shows what `auto` would hold.
        let lim = cpu_limiter(CpuMode::Off, 12);
        lim.record_run(k, "repo · ci · e2e", &cpu_report(6000, 100));
        assert_eq!(class_view(&lim, k).cpu_status, CapStatus::Warned);
        assert!(lim.snapshot().warnings.is_empty());
    }

    #[test]
    fn a_class_held_run_after_run_is_one_warning_until_a_pin_clears_it() {
        // The panel keeps the newest warnings; one busy matrix class used to fill
        // all of them with copies of itself and push every other class out.
        let lim = cpu_limiter(CpuMode::Auto, 12);
        let (a, b) = ("repo::1::visual (1)", "repo::1::e2e");
        for peak in [6000, 6100, 6200] {
            lim.record_run(a, "repo · ci · visual (1)", &cpu_report(peak, 100));
        }
        lim.record_run(b, "repo · ci · e2e", &cpu_report(6000, 100));
        let warnings = lim.snapshot().warnings;
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("repo · ci · e2e:"), "newest first");
        assert!(
            warnings[1].contains("runner wants 9.3 cores"),
            "the repeat replaced the earlier entry: {warnings:?}"
        );

        lim.pin(a, pinned(8.0, 0.0));
        let warnings = lim.snapshot().warnings;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("repo · ci · e2e:"));
    }

    /// A run heavy on memory and, when `cpu_peak` is set, on CPU.
    fn heavy_report(cpu_peak: Option<u64>) -> RunReport {
        RunReport {
            runner_cpu: cpu_peak.and_then(|p| usage(p, 0.0)),
            ..report(2000, 200)
        }
    }

    #[test]
    fn a_memory_warning_is_one_per_class_and_clears_when_the_hold_lifts() {
        let lim = cpu_limiter(CpuMode::Auto, 12);
        lim.mem_total_mib.store(6144, Ordering::SeqCst); // budget 4096, threshold 2048
        lim.recompute_budget();
        let k = "repo::1::heavy";
        for _ in 0..3 {
            lim.record_run(k, "heavy", &heavy_report(None));
        }
        let warnings = lim.snapshot().warnings;
        assert_eq!(warnings.len(), 1, "repeats replace the entry: {warnings:?}");
        // The heavy samples age out of the window: the hold lifts, the warning goes.
        for _ in 0..3 {
            lim.record_run(k, "heavy", &report(100, 50));
        }
        let warnings = lim.snapshot().warnings;
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_memory_pin_clears_the_memory_warning_but_not_the_cpu_one() {
        let lim = cpu_limiter(CpuMode::Auto, 12);
        lim.mem_total_mib.store(6144, Ordering::SeqCst);
        lim.recompute_budget();
        let k = "repo::1::heavy";
        lim.record_run(k, "heavy", &heavy_report(Some(6000)));
        assert_eq!(lim.snapshot().warnings.len(), 2, "memory and CPU");
        // `cpus` 0.0 pins memory only, so the CPU hold still stands.
        lim.pin(k, pinned(0.0, 0.0));
        let warnings = lim.snapshot().warnings;
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("runner wants"), "{warnings:?}");
    }

    #[test]
    fn a_throttled_run_is_a_censored_sample() {
        let lim = cpu_limiter(CpuMode::Auto, 12);
        let k = "repo::1::test";
        let throttled = |peak, ratio, applied| RunReport {
            runner_cpu: usage(peak, ratio),
            applied_runner_mcpu: applied,
            ..cpu_report(0, 100)
        };
        lim.record_run(k, "test", &cpu_report(1000, 100)); // share 1500
        assert_eq!(class_view(&lim, k).runner_mcpu, 1500);

        // Throttled 25% at a 1500 quota: the peak is the quota's, not the job's.
        // Censored at 1500 × 1.5 = 2250, × margin = 3375.
        lim.record_run(k, "test", &throttled(1500, 0.25, 1500));
        assert_eq!(class_view(&lim, k).runner_mcpu, 3375);

        // Below the 10% line the peak stands (the window still holds 2250).
        let lim2 = cpu_limiter(CpuMode::Auto, 12);
        lim2.record_run(k, "test", &cpu_report(1000, 100));
        lim2.record_run(k, "test", &throttled(1500, 0.05, 1500));
        assert_eq!(class_view(&lim2, k).runner_mcpu, 2250, "1500 × 1.5");

        // With no quota there was nothing to hit, whatever the counter says.
        let lim3 = cpu_limiter(CpuMode::Off, 12);
        lim3.record_run(k, "test", &cpu_report(1000, 100));
        lim3.record_run(k, "test", &throttled(1500, 0.5, 0));
        assert_eq!(class_view(&lim3, k).runner_mcpu, 2250);

        // Censored at the quota the container actually ran under when cohort
        // sizing raised it above the class's share: 3000 × 1.5 = 4500, × 1.5 =
        // 6750, held at the four-core ceiling.
        let lim4 = cpu_limiter(CpuMode::Auto, 12);
        lim4.record_run(k, "test", &cpu_report(1000, 100));
        lim4.record_run(k, "test", &throttled(2900, 0.2, 3000));
        let c = class_view(&lim4, k);
        assert_eq!(c.runner_mcpu, 4000);
        assert_eq!(c.cpu_status, CapStatus::Warned);
    }

    /// Operator limits as the pin endpoint builds them: memory plus these cores.
    fn pinned(runner_cpus: f64, dind_cpus: f64) -> Limits {
        Limits {
            runner: ResourceLimits {
                cpus: runner_cpus,
                memory_mib: 1024,
            },
            dind: ResourceLimits {
                cpus: dind_cpus,
                memory_mib: 512,
            },
        }
    }

    #[test]
    fn a_cpu_pin_freezes_cpu_learning() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        let k = "repo::1::pinned";
        lim.pin(k, pinned(2.0, 0.5));
        lim.record_run(k, "pinned", &cpu_report(6000, 3000));
        let c = class_view(&lim, k);
        assert!(c.cpu_pinned);
        assert_eq!(c.cpu_status, CapStatus::Pinned);
        assert_eq!((c.runner_mcpu, c.dind_mcpu), (2000, 500));
        let caps = lim.pinned_caps();
        assert_eq!(caps[k].runner.cpus, 2.0, "persisted for the next boot");
        assert_eq!(caps[k].dind.cpus, 0.5);

        // A sub-core runner pin is floored like a learned share.
        lim.pin("repo::1::tiny", pinned(0.25, 0.0));
        assert_eq!(class_view(&lim, "repo::1::tiny").runner_mcpu, 1000);

        // Unpinning hands both back to the learner.
        lim.unpin(k);
        let c = class_view(&lim, k);
        assert!(!c.cpu_pinned);
        assert_eq!(c.cpu_status, CapStatus::Learned);
        lim.record_run(k, "pinned", &cpu_report(2000, 400));
        assert_eq!(class_view(&lim, k).runner_mcpu, 3000);
    }

    #[test]
    fn a_memory_only_pin_leaves_cpu_learned() {
        // Every pin in an overrides file written before ADR-0019 carries `cpus:
        // 0.0`, and must mean what it meant: memory only.
        let k = "repo::1::mem";
        let legacy = HashMap::from([(k.to_string(), pinned(0.0, 0.0))]);
        let lim = Limiter::new(cfg(), &legacy);
        lim.record_run(k, "mem", &cpu_report(2000, 400));
        let c = class_view(&lim, k);
        assert!(c.pinned && !c.cpu_pinned);
        assert_eq!(c.runner_mib, 1024, "memory stays pinned");
        assert_eq!(c.runner_mcpu, 3000, "CPU still learns");
        assert_eq!(c.cpu_status, CapStatus::Learned);
        assert_eq!(
            lim.pinned_caps()[k].runner.cpus,
            0.0,
            "and persists as none"
        );

        // Likewise through the live endpoint — and a memory-only re-pin of a
        // CPU-pinned class does not quietly withdraw its CPU pin.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.pin(k, pinned(0.0, 0.0));
        lim.record_run(k, "mem", &cpu_report(2000, 400));
        assert_eq!(class_view(&lim, k).runner_mcpu, 3000);
        lim.pin(k, pinned(3.0, 0.0));
        lim.pin(k, pinned(0.0, 0.0));
        let c = class_view(&lim, k);
        assert!(c.cpu_pinned);
        assert_eq!(c.runner_mcpu, 3000);
        assert_eq!(
            c.dind_mcpu, 600,
            "an unset dind pin keeps the learned share"
        );

        // A CPU pin read back from overrides is a CPU pin.
        let persisted = HashMap::from([(k.to_string(), pinned(3.0, 0.0))]);
        let lim = Limiter::new(cfg(), &persisted);
        assert!(class_view(&lim, k).cpu_pinned);
    }

    #[test]
    fn seeding_replays_cpu_through_the_same_censoring_rule() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        let k = "repo::1::e2e";
        lim.seed_cpu(k, "e2e", usage(1000, 0.0), 0, usage(200, 0.0));
        let c = class_view(&lim, k);
        assert_eq!((c.runner_mcpu, c.dind_mcpu), (1500, 300));
        assert_eq!(c.cpu_status, CapStatus::Learned);

        // A sample throttled at its 3000 quota replays as ≥ 4500, not as 3000:
        // × margin 6750, held at the ceiling. Replay restores state, so it warns
        // nobody.
        lim.seed_cpu(k, "e2e", usage(3000, 0.3), 3000, None);
        let c = class_view(&lim, k);
        assert_eq!(c.runner_mcpu, 4000);
        assert_eq!(c.cpu_status, CapStatus::Warned);
        assert_eq!(c.dind_mcpu, 300, "an unsampled container keeps its share");
        assert!(lim.snapshot().warnings.is_empty());

        // Nothing replayed: still profiling.
        lim.seed_cpu("repo::1::quiet", "quiet", None, 0, None);
        let c = class_view(&lim, "repo::1::quiet");
        assert_eq!(c.cpu_status, CapStatus::Profiling);
        assert_eq!((c.runner_mcpu, c.dind_mcpu), (4000, 1000));

        // A CPU-pinned class ignores its history.
        lim.pin("repo::1::held", pinned(2.0, 0.0));
        lim.seed_cpu("repo::1::held", "held", usage(9000, 0.0), 0, None);
        assert_eq!(class_view(&lim, "repo::1::held").runner_mcpu, 2000);
    }

    #[tokio::test]
    async fn adoption_and_admission_carry_cpu_and_their_guards_release_it() {
        let lim = cpu_limiter(CpuMode::Auto, 4); // budget 4000
        let adopted = lim.adopt(res(1000, 3000));
        let view = lim.snapshot();
        assert_eq!(view.cpu.reserved_mcpu, 3000);
        assert_eq!(view.docker_inflight, 1);
        assert!(
            lim.try_admit(NO_TICKET, res(100, 1500)).is_err(),
            "new work queues behind the adopted CPU"
        );
        drop(adopted);
        assert_eq!(lim.snapshot().cpu.reserved_mcpu, 0);

        let admitted = lim.admit(res(100, 1500), |_| {}).await;
        assert_eq!(lim.snapshot().cpu.reserved_mcpu, 1500);
        drop(admitted);
        assert_eq!(lim.snapshot().cpu.reserved_mcpu, 0);
    }

    #[test]
    fn an_unlabelled_adoption_is_charged_its_classs_current_share() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert_eq!(lim.known_mcpu_for("repo::1::never", true), 0);
        lim.record_run("repo::1::known", "known", &cpu_report(2000, 400));
        assert_eq!(lim.known_mcpu_for("repo::1::known", true), 3600);
        assert_eq!(lim.known_mcpu_for("repo::1::known", false), 3000);
    }

    #[test]
    fn the_dashboard_contract_field_names() {
        // The dashboard and the menu-bar app read these by name.
        let lim = cpu_limiter(CpuMode::Auto, 12);
        lim.set_cpu_pressure_for_test(Some(12.5));
        lim.record_run("repo::1::build", "build", &cpu_report(2000, 400));
        let json = serde_json::to_value(lim.snapshot()).unwrap();
        let cpu = &json["cpu"];
        assert_eq!(cpu["mode"], "auto");
        assert_eq!(cpu["ncpu"], 12);
        assert_eq!(cpu["budget_mcpu"], 14000);
        assert_eq!(cpu["reserved_mcpu"], 0);
        assert_eq!(cpu["pressure"], 12.5);
        assert_eq!(cpu["frozen"], false);
        assert_eq!(cpu["overcommit"], 1.25);
        assert_eq!(cpu["reserve_cpus"], 1.0);
        assert_eq!(cpu["profiling_cpus"], 4.0);
        let class = &json["classes"][0];
        assert_eq!(class["runner_mcpu"], 3000);
        assert_eq!(class["dind_mcpu"], 600);
        assert_eq!(class["cpu_status"], "learned");
        assert_eq!(class["cpu_pinned"], false);
        // The machine (ADR-0021): present before it is read, with its share.
        assert_eq!(json["machine"]["share"], "unstated");
        assert_eq!(json["machine"]["mem_mib"], serde_json::Value::Null);
        assert_eq!(json["machine"]["hold"], serde_json::Value::Null);
    }

    // --- the hold, and every refusal named (issue #66) ------------------------
    //
    // A zero ceiling held one host's Docker admission for hours while the
    // dashboard read healthy and every claim sat parked with no reason given. The
    // hold is the named, dated replacement, and a refusal now says which gate.

    #[test]
    fn a_hold_is_checked_before_every_other_gate_and_the_rest_keep_their_order() {
        // Every gate closed at once, then opened one at a time: each refusal must
        // name the first gate still closed, in the order `try_admit` checks them.
        let mut c = cfg();
        c.hard_ceiling = HardCeiling::Fixed(1);
        let lim = Limiter::new(c, &HashMap::new());
        let running = lim.adopt(100.into()); // the ceiling's one slot; budget unprobed
        lim.hold_for_adoption();
        lim.probe_misses.store(UNHEALTHY_AFTER, Ordering::SeqCst);
        lim.hold("VM resize pending".to_string(), 1_000);
        lim.pause(Pause::Operator);
        lim.set_machine_share(example_share());
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Warn))), now_ms());
        lim.set_disk_admission(DiskAdmission::Floor(10));
        lim.set_disk_free_for_test(Some(1024), now_ms());

        assert_eq!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Held {
                reason: "VM resize pending".to_string()
            })
        );
        assert!(lim.release_hold());
        // Pause next (ADR-0023): after the operator's word, before the machine's.
        assert_eq!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Paused {
                by: Pause::Operator
            })
        );
        assert!(lim.resume(Pause::Operator));
        // The machine next (ADR-0021 §2c): it holds macOS VM jobs too, so it sits
        // with pause, ahead of everything about the daemon.
        assert_eq!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Machine(MachineHold::Pressure(PressureLevel::Warn)))
        );
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Normal))), now_ms());
        assert_eq!(lim.try_admit(NO_TICKET, 100), Err(Wait::DockerUnreachable));
        lim.probe_misses.store(0, Ordering::SeqCst);
        // The disk right behind liveness (ADR-0024): both are about the VM.
        assert_eq!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Disk {
                free_mib: 1024,
                volume: crate::host::DiskVolume::Docker,
                ramping: 0,
                held_mib: 0,
                charge_mib: DISK_CHARGE_MIB,
                floor_gib: 10,
            })
        );
        lim.set_disk_free_for_test(Some(40 * 1024), now_ms());
        assert_eq!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Ceiling {
                inflight: 1,
                max: 1
            })
        );
        drop(running);
        assert_eq!(lim.try_admit(NO_TICKET, 100), Err(Wait::Adoption));
        lim.adoption_settled();
        let unmeasured = lim.try_admit(NO_TICKET, 100).unwrap_err();
        assert_eq!(
            unmeasured,
            Wait::Memory {
                need_mib: 100,
                reserved_mib: 0,
                budget_mib: 0,
                setaside_mib: 0,
                setaside_priority: false,
            }
        );
        assert_eq!(unmeasured.view().detail, "memory budget not measured yet");
        lim.set_vm_total_for_test(6144);
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
    }

    #[test]
    fn a_memory_refusal_says_what_blocks_it() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(6144); // budget 4096
        let _running = lim.adopt(1000.into());
        let full = lim.try_admit(NO_TICKET, 3500).unwrap_err();
        assert_eq!(
            full,
            Wait::Memory {
                need_mib: 3500,
                reserved_mib: 1000,
                budget_mib: 4096,
                setaside_mib: 0,
                setaside_priority: false,
            }
        );
        assert_eq!(full.view().gate, "memory");
        assert_eq!(
            full.view().detail,
            "needs 3500 MiB, 3096 MiB free of 4096 MiB"
        );

        // Fits on its own, but not beside what the head of the queue has set aside.
        let _head = lim.take_ticket(3000);
        let junior = lim.take_ticket(1000);
        assert_eq!(
            lim.try_admit(junior.id, 1000).unwrap_err().view().detail,
            "needs 1000 MiB, 3096 MiB free of 4096 MiB, 3000 MiB set aside for an older claim"
        );

        // A reservation the whole budget cannot hold is said to be one.
        assert_eq!(
            lim.try_admit(NO_TICKET, 5000).unwrap_err().view().detail,
            "needs 5000 MiB, more than the whole 4096 MiB budget"
        );
    }

    #[test]
    fn the_live_pressure_gates_say_what_they_read() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(20_480);
        // Below headroom: frozen until it clears headroom + band.
        lim.available_mib.store(900, Ordering::SeqCst);
        let frozen = lim.try_admit(NO_TICKET, 100).unwrap_err();
        assert_eq!(
            frozen,
            Wait::VmMemory {
                need_mib: 100,
                available_mib: 900,
                headroom_mib: 1024,
                resume_mib: 1536,
                frozen: true,
            }
        );
        assert_eq!(frozen.view().gate, "vm-memory");
        assert_eq!(
            frozen.view().detail,
            "VM memory frozen under pressure: 900 MiB available, resumes at 1536 MiB"
        );
        // Thawed, but not enough for this job plus headroom.
        lim.available_mib.store(2000, Ordering::SeqCst);
        assert_eq!(
            lim.try_admit(NO_TICKET, 1500).unwrap_err().view().detail,
            "VM memory: 2000 MiB available, needs 1500 + 1024 MiB headroom"
        );

        // CPU: the ledger, then the pressure gate.
        let lim = cpu_limiter(CpuMode::Auto, 4); // budget 4000
        assert!(lim.try_admit(NO_TICKET, res(100, 2500)).is_ok());
        let ledger = lim.try_admit(NO_TICKET, res(100, 2000)).unwrap_err();
        assert_eq!(ledger.view().gate, "cpu");
        assert_eq!(ledger.view().detail, "needs 2 cores, 1.5 free of 4");
        lim.set_cpu_pressure_for_test(Some(45.0));
        let pressure = lim.try_admit(NO_TICKET, res(100, 100)).unwrap_err();
        assert_eq!(
            pressure,
            Wait::CpuPressure {
                pressure: 45.0,
                resume: 25.0
            }
        );
        assert_eq!(pressure.view().gate, "cpu-pressure");
        assert_eq!(
            pressure.view().detail,
            "VM CPU pressure 45%, resumes at 25%"
        );
    }

    #[test]
    fn every_gate_has_its_contract_key() {
        // The dashboard and the menu-bar app read `gate` by name.
        let keys: Vec<String> = [
            Wait::Held {
                reason: "resize".to_string(),
            },
            Wait::Paused {
                by: Pause::Operator,
            },
            Wait::Machine(MachineHold::Pressure(PressureLevel::Warn)),
            Wait::Machine(MachineHold::Swap { rise_mib: 1024 }),
            Wait::DockerUnreachable,
            Wait::Disk {
                free_mib: 0,
                volume: crate::host::DiskVolume::Docker,
                ramping: 0,
                held_mib: 0,
                charge_mib: DISK_CHARGE_MIB,
                floor_gib: 10,
            },
            Wait::Adoption,
            Wait::Ceiling {
                inflight: 4,
                max: 4,
            },
            Wait::Memory {
                need_mib: 1,
                reserved_mib: 0,
                budget_mib: 0,
                setaside_mib: 0,
                setaside_priority: false,
            },
            Wait::Cpu {
                need_mcpu: 1,
                reserved_mcpu: 0,
                budget_mcpu: 0,
                setaside_mcpu: 0,
                setaside_priority: false,
            },
            Wait::VmMemory {
                need_mib: 1,
                available_mib: 0,
                headroom_mib: 0,
                resume_mib: 0,
                frozen: true,
            },
            Wait::CpuPressure {
                pressure: 50.0,
                resume: 25.0,
            },
        ]
        .iter()
        .map(|w| w.view().gate)
        .collect();
        assert_eq!(
            keys,
            [
                "held",
                "paused",
                "machine-pressure",
                "swap",
                "docker",
                "disk",
                "adoption",
                "ceiling",
                "memory",
                "cpu",
                "vm-memory",
                "cpu-pressure"
            ]
        );
        let held = Wait::Held {
            reason: "VM resize".to_string(),
        };
        assert_eq!(held.view().detail, "Docker admission held: VM resize");
        // Who paused is told apart: the janitor's lifts itself, the operator's
        // waits for someone to resume.
        assert_eq!(
            Wait::Paused {
                by: Pause::Operator
            }
            .view()
            .detail,
            "faber is paused — nothing new starts until it resumes"
        );
        assert_eq!(
            Wait::Paused { by: Pause::Reclaim }.view(),
            WaitView::new("paused", "paused while the janitor reclaims disk")
        );
        let ceiling = Wait::Ceiling {
            inflight: 4,
            max: 4,
        };
        assert_eq!(
            ceiling.view().detail,
            "hard ceiling: 4 of 4 Docker jobs running"
        );
        assert_eq!(
            serde_json::to_value(held.view()).unwrap(),
            serde_json::json!({"gate": "held", "detail": "Docker admission held: VM resize"})
        );
    }

    // --- the machine's share (ADR-0021) ----------------------------------------

    /// An example machine: 32 GiB and 10 cores.
    fn machine(pressure: Option<PressureLevel>) -> MachineReading {
        MachineReading {
            mem_mib: 32 * 1024,
            cpus: 10,
            pressure,
            swap_used_mib: Some(0),
            ..MachineReading::default()
        }
    }

    fn example_share() -> MachineShare {
        MachineShare {
            keep_memory_gib: Some(16),
            keep_cpus: Some(2),
        }
    }

    #[test]
    fn the_budgets_are_planned_within_the_machines_share() {
        let lim = cpu_limiter(CpuMode::Auto, 10);
        lim.set_vm_total_for_test(24 * 1024);
        // Unstated: the whole VM, as before.
        assert_eq!(lim.snapshot().budget_mib, 24 * 1024 - 2048);
        assert_eq!(lim.snapshot().cpu.budget_mcpu, 11500);
        lim.set_machine_for_test(Some(machine(None)), now_ms());
        assert_eq!(lim.snapshot().budget_mib, 24 * 1024 - 2048);
        // Declared: min(VM, machine − keep), less the same reserves.
        lim.set_machine_share(example_share());
        assert_eq!(lim.snapshot().budget_mib, 16 * 1024 - 2048);
        // 8 cores × 1.25 − 1.
        assert_eq!(lim.snapshot().cpu.budget_mcpu, 9000);
        // The runner's quota stays bounded by the VM, not the plan.
        assert_eq!(lim.snapshot().machine.vm_cpus, Some(10));
        assert_eq!(lim.snapshot().machine.encroaches.len(), 2);
        // A VM already inside the share plans as it always did.
        lim.set_vm_total_for_test(12 * 1024);
        assert_eq!(lim.snapshot().budget_mib, 12 * 1024 - 2048);
    }

    #[test]
    fn the_share_counts_macos_vms_only_where_a_profile_can_start_one() {
        // An 8 GiB Linux VM on a 32 GiB machine that keeps 16: it fits on its own,
        // and only two 8 GiB macOS VMs beside it would not.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(8 * 1024);
        lim.set_machine_for_test(Some(machine(None)), now_ms());
        lim.set_machine_share(MachineShare {
            keep_memory_gib: Some(16),
            keep_cpus: None,
        });
        lim.set_linux_vm_mib(8 * 1024);
        assert!(lim.snapshot().machine.encroaches.is_empty());
        assert!(!lim
            .machine_warnings()
            .iter()
            .any(|w| w.contains("for macOS VMs")));

        lim.set_runs_macos_vms(true);
        let encroaches = lim.snapshot().machine.encroaches;
        assert_eq!(encroaches.len(), 1);
        assert!(encroaches[0].contains("up to 16 for macOS VMs"));

        // ADR-0051: the layout's macOS VMs, not Apple's limit. One 8 GiB VM beside
        // the 8 GiB Linux VM fits the 16 GiB share; none holds nothing back.
        lim.set_vm_layout(crate::config::VmLayout {
            macos_vms: 1,
            ..Default::default()
        });
        assert!(lim.snapshot().machine.encroaches.is_empty());
        lim.set_vm_layout(crate::config::VmLayout {
            macos_vms: 0,
            ..Default::default()
        });
        assert!(!lim
            .machine_warnings()
            .iter()
            .any(|w| w.contains("for macOS VMs")));
    }

    #[test]
    fn gate1_holds_back_the_co_tenant_peak_once_the_share_is_declared() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(24 * 1024);
        lim.set_machine_for_test(Some(machine(None)), now_ms());
        // Say the co-tenants held 8 GiB.
        lim.observe_cotenant_for_test(8 * 1024, now_ms());
        // Unstated: measured and shown, but `reserve_mib` alone binds, as before.
        let snap = lim.snapshot();
        assert_eq!(snap.budget_mib, 24 * 1024 - 2048);
        assert_eq!(snap.cotenant.mode, "floor");
        assert_eq!(snap.cotenant.peak_mib, Some(8 * 1024));
        assert!(!snap.cotenant.peak_binds);
        // Declared: 16 GiB planned, less the 8 GiB peak.
        lim.set_machine_share(example_share());
        let snap = lim.snapshot();
        assert_eq!(snap.budget_mib, 8 * 1024);
        assert_eq!(snap.cotenant.reserve_mib, 8 * 1024);
        assert!(snap.cotenant.peak_binds);
        assert_eq!(
            snap.reserve_mib, 2048,
            "the configured floor is still shown"
        );
        // A floor above the peak still binds.
        lim.set_tuning(Some(12 * 1024), None, None, None, None);
        assert_eq!(lim.snapshot().budget_mib, 4 * 1024);
    }

    #[test]
    fn a_reserve_that_takes_the_whole_plan_is_too_small_not_unmeasured() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(24 * 1024);
        lim.set_machine_for_test(Some(machine(None)), now_ms());
        lim.set_machine_share(example_share());
        lim.observe_cotenant_for_test(24 * 1024, now_ms());
        assert_eq!(lim.snapshot().budget_mib, 1);
        let wait = lim.try_admit(NO_TICKET, 1024).unwrap_err();
        assert_eq!(
            wait.view().detail,
            "needs 1024 MiB, more than the whole 1 MiB budget"
        );
    }

    #[test]
    fn the_budget_view_carries_the_per_gate_counters() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert!(lim.snapshot().gates.is_empty());
        let mut cell = crate::parking::Parking::new(lim.gate_tally());
        cell.publish(WaitView::new("memory", "needs 6000 MiB"), 1_000);
        let gates = lim.snapshot().gates;
        assert_eq!(gates["memory"].parked_now, 1);
        cell.settle(61_000);
        let v = serde_json::to_value(lim.snapshot()).unwrap();
        assert_eq!(v["gates"]["memory"]["parked_now"], 0);
        assert_eq!(v["gates"]["memory"]["parked_ms"], 60_000);
        assert_eq!(v["gates"]["memory"]["binding"], 1);
        assert_eq!(v["cotenant"]["mode"], "floor");
    }

    #[tokio::test]
    async fn machine_pressure_parks_admit_and_normal_admits_at_once() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(12 * 1024);
        lim.set_machine_share(example_share());
        // One job already running: the pressure gate's floor (#160) is spent.
        let _running = lim.adopt(100.into());
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Warn))), now_ms());
        let mut held = lim.machine_signal();
        assert_eq!(
            *held.borrow_and_update(),
            Some(MachineHold::Pressure(PressureLevel::Warn))
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let (lim, seen) = (lim.clone(), seen.clone());
            tokio::spawn(async move {
                lim.admit(Reservation::from(1024), |w| {
                    seen.lock().unwrap().push(w.clone())
                })
                .await
            })
        };
        until_last_wait(
            &seen,
            Wait::Machine(MachineHold::Pressure(PressureLevel::Warn)),
        )
        .await;
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Normal))), now_ms());
        let _guard = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("admitted at once, not at the next retry")
            .unwrap();
        // A watcher of the hold hears the same: it lifted.
        held.changed().await.unwrap();
        assert_eq!(*held.borrow(), None);
    }

    #[test]
    fn pressure_keeps_one_docker_job_moving_and_parks_the_second() {
        // #160: the VM never returns memory, so a gate that held at zero jobs
        // would wait for pressure that nothing CI does can lower.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(12 * 1024);
        lim.set_machine_share(example_share());
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Critical))), now_ms());
        assert!(lim.current_machine_hold().is_some());
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        assert_eq!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Machine(MachineHold::Pressure(
                PressureLevel::Critical
            )))
        );
        // Swap has no floor: it clears once nothing adds to it.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(1 << 20);
        lim.set_machine_share(example_share());
        let t0 = now_ms();
        let swap = |mib| MachineReading {
            swap_used_mib: Some(mib),
            ..machine(Some(PressureLevel::Normal))
        };
        lim.set_machine_for_test(Some(swap(3_000)), t0);
        lim.set_machine_for_test(Some(swap(4_200)), t0 + 1);
        assert!(matches!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Machine(MachineHold::Swap { .. }))
        ));
    }

    #[test]
    fn growing_swap_holds_even_under_pressure_with_nothing_admitted() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(12 * 1024);
        lim.set_machine_share(example_share());
        let t0 = now_ms();
        let both = |mib| MachineReading {
            swap_used_mib: Some(mib),
            ..machine(Some(PressureLevel::Warn))
        };
        lim.set_machine_for_test(Some(both(3_000)), t0);
        lim.set_machine_for_test(Some(both(4_200)), t0 + 1);
        assert!(matches!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Machine(MachineHold::Swap { .. }))
        ));
    }

    #[test]
    fn a_vm_larger_than_the_share_is_named_as_the_reason() {
        // 24 GiB VM on a 32 GiB machine that keeps 16: 16 GiB left for CI.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(24 * 1024);
        lim.set_machine_share(example_share());
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Warn))), now_ms());
        let _running = lim.adopt(100.into());
        let refused = lim.try_admit(NO_TICKET, 100).unwrap_err();
        assert_eq!(
            refused.view(),
            WaitView::new(
                "machine-pressure",
                "macOS memory pressure is warn and the Docker VM (24 GiB) is larger than the \
                 machine share leaves for CI (16 GiB): resize the VM — running one job at a time"
            )
        );
        // A VM that fits the share gets the plain wording.
        lim.set_vm_total_for_test(12 * 1024);
        assert_eq!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Machine(MachineHold::Pressure(PressureLevel::Warn)))
        );
    }

    #[test]
    fn an_unstated_or_zero_share_never_holds_but_still_measures() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(1 << 20);
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Critical))), now_ms());
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        let view = lim.snapshot().machine;
        assert_eq!(
            (view.share, view.pressure, view.hold),
            ("unstated", Some("critical"), None)
        );
        lim.set_machine_share(MachineShare {
            keep_memory_gib: Some(0),
            keep_cpus: Some(0),
        });
        lim.set_machine_for_test(Some(machine(Some(PressureLevel::Critical))), now_ms());
        assert_eq!(lim.current_machine_hold(), None);
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
    }

    #[test]
    fn growing_swap_holds_admission_once_the_share_is_declared() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(1 << 20);
        lim.set_machine_share(example_share());
        let t0 = now_ms();
        let swap = |mib| MachineReading {
            swap_used_mib: Some(mib),
            ..machine(Some(PressureLevel::Normal))
        };
        lim.set_machine_for_test(Some(swap(3_000)), t0);
        lim.set_machine_for_test(Some(swap(4_200)), t0 + 1);
        let refused = lim.try_admit(NO_TICKET, 100).unwrap_err();
        assert_eq!(
            refused,
            Wait::Machine(MachineHold::Swap { rise_mib: 1_200 })
        );
        assert_eq!(refused.view().gate, "swap");
        assert_eq!(lim.snapshot().machine.hold, Some("swap"));
    }

    #[test]
    fn the_boot_warnings_name_the_split_an_invalid_share_and_the_fix() {
        let lim = cpu_limiter(CpuMode::Auto, 10);
        lim.set_vm_total_for_test(24 * 1024);
        lim.set_machine_for_test(Some(machine(None)), now_ms());
        let unstated = lim.machine_warnings();
        assert_eq!(unstated.len(), 2, "{unstated:?}");
        assert!(
            unstated[0].contains("The Docker VM holds 24 of this machine's 32 GiB, leaving it 8.")
        );
        lim.set_machine_share(MachineShare {
            keep_memory_gib: Some(32),
            keep_cpus: Some(2),
        });
        let warnings = lim.machine_warnings();
        assert!(
            warnings[0].starts_with("host.keep_memory_gib (32 GiB) is not smaller")
                && warnings[0].contains("planning ignores it until it fits")
                && warnings[0].contains("the pressure and swap gates still apply"),
            "{warnings:?}"
        );
        assert!(warnings[1].starts_with("the Docker VM has 10 of this machine's 10 cores"));
        // Ignored, not planned against: the whole VM still bounds memory.
        assert_eq!(lim.snapshot().budget_mib, 24 * 1024 - 2048);
    }

    #[tokio::test]
    async fn a_hold_parks_admit_and_releasing_it_admits_at_once() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(1 << 20);
        lim.hold("VM resize pending".to_string(), 1_000);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let admission = tokio::spawn({
            let (lim, seen) = (lim.clone(), seen.clone());
            async move {
                lim.admit(100.into(), move |w| seen.lock().unwrap().push(w.clone()))
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while seen.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the held admission reports why it waits");
        assert_eq!(
            seen.lock().unwrap()[0],
            Wait::Held {
                reason: "VM resize pending".to_string()
            }
        );
        assert!(!admission.is_finished(), "a hold admits nothing");

        // Released: the parked caller is woken rather than left to its next retry.
        assert!(lim.release_hold());
        let guard = tokio::time::timeout(ADMIT_RETRY / 2, admission)
            .await
            .expect("admitted as soon as the hold was released")
            .expect("admission task");
        assert_eq!(lim.snapshot().docker_inflight, 1);
        drop(guard);
    }

    #[test]
    fn a_hold_keeps_its_date_across_a_new_reason_and_releases_once() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert_eq!(lim.current_hold(), None);
        assert!(!lim.release_hold(), "nothing to release");
        lim.hold("VM resize".to_string(), 100);
        lim.hold("VM resize, then a Docker upgrade".to_string(), 200);
        assert_eq!(
            lim.current_hold(),
            Some(Hold {
                reason: "VM resize, then a Docker upgrade".to_string(),
                since_ms: 100,
            })
        );
        assert!(lim.release_hold());
        assert_eq!(lim.current_hold(), None);
        assert!(!lim.release_hold());
    }

    #[test]
    fn a_hold_stops_new_admissions_only() {
        // Running and adopted containers are not admissions: a hold must never
        // strand the ledger's account of what is already on the host.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(20_480);
        assert!(lim.try_admit(NO_TICKET, 1000).is_ok());
        lim.hold("resize".to_string(), 5);
        let adopted = lim.adopt(2000.into());
        let view = lim.snapshot();
        assert_eq!(view.docker_inflight, 2);
        assert_eq!(view.reserved_mib, 3000);
        assert!(lim.try_admit(NO_TICKET, 100).is_err());
        drop(adopted);
        assert_eq!(lim.snapshot().reserved_mib, 1000);
    }

    #[test]
    fn the_budget_view_always_carries_the_hold_key() {
        // `null` rather than absent, so a client can tell "not held" from "a
        // daemon too old to say".
        let lim = Limiter::new(cfg(), &HashMap::new());
        let json = serde_json::to_value(lim.snapshot()).unwrap();
        assert_eq!(json["hold"], serde_json::Value::Null);
        assert!(json.as_object().unwrap().contains_key("hold"));

        lim.hold("VM resize".to_string(), 1_790_000_000_000);
        let json = serde_json::to_value(lim.snapshot()).unwrap();
        assert_eq!(
            json["hold"],
            serde_json::json!({"reason": "VM resize", "since_ms": 1_790_000_000_000u64})
        );
    }

    // --- pause stops what starts (ADR-0023) ----------------------------------
    //
    // Pause used to stop claiming only: a claim already parked here was admitted
    // the moment it fit, under the operator's pause and in the middle of the
    // janitor's reclamation alike.

    /// Poll until `seen`'s latest refusal is `want` — the parked caller has run
    /// its re-check — or fail after five seconds.
    async fn until_last_wait(seen: &Mutex<Vec<Wait>>, want: Wait) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while seen.lock().unwrap().last() != Some(&want) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the parked admission never said {want:?}"));
    }

    #[tokio::test]
    async fn a_pause_parks_admit_and_resuming_admits_at_once() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(1 << 20); // room to spare: only the pause refuses
        assert!(lim.pause(Pause::Reclaim));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let admission = tokio::spawn({
            let (lim, seen) = (lim.clone(), seen.clone());
            async move {
                lim.admit(100.into(), move |w| seen.lock().unwrap().push(w.clone()))
                    .await
            }
        });
        until_last_wait(&seen, Wait::Paused { by: Pause::Reclaim }).await;
        assert!(!admission.is_finished(), "a pause admits nothing");

        // The operator pausing mid-reclamation takes the pause over: the parked
        // claim says so at once, and the janitor's resume no longer lifts it.
        assert!(lim.pause(Pause::Operator));
        until_last_wait(
            &seen,
            Wait::Paused {
                by: Pause::Operator,
            },
        )
        .await;
        assert!(!lim.resume(Pause::Reclaim));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!admission.is_finished(), "still the operator's pause");

        // Resumed: the parked caller is woken rather than left to its next retry.
        assert!(lim.resume(Pause::Operator));
        let guard = tokio::time::timeout(ADMIT_RETRY / 2, admission)
            .await
            .expect("admitted as soon as faber resumed")
            .expect("admission task");
        assert_eq!(lim.snapshot().docker_inflight, 1);
        drop(guard);
    }

    #[test]
    fn the_janitor_takes_only_a_free_pause_and_lifts_only_its_own() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert_eq!(lim.current_pause(), None);
        assert!(!lim.resume(Pause::Operator), "nothing to lift");

        assert!(lim.pause(Pause::Reclaim));
        assert!(!lim.pause(Pause::Reclaim), "already paused");
        assert!(lim.resume(Pause::Reclaim));
        assert_eq!(lim.current_pause(), None);

        // The operator's pause is never the janitor's to take or to lift.
        assert!(lim.pause(Pause::Operator));
        assert!(!lim.pause(Pause::Reclaim));
        assert!(!lim.resume(Pause::Reclaim));
        assert_eq!(lim.current_pause(), Some(Pause::Operator));
        assert!(!lim.pause(Pause::Operator), "pausing twice changes nothing");
        assert!(lim.resume(Pause::Operator));

        // The operator's resume lifts the janitor's, as it always has.
        assert!(lim.pause(Pause::Reclaim));
        assert!(lim.resume(Pause::Operator));
        assert_eq!(lim.current_pause(), None);
        assert!(!lim.resume(Pause::Reclaim), "nothing left for the janitor");
    }

    #[test]
    fn a_pause_stops_new_admissions_only() {
        // As with a hold: an adopted container is already running, so a pause
        // must never keep it off the ledger.
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(20_480);
        lim.pause(Pause::Operator);
        let adopted = lim.adopt(2000.into());
        assert_eq!(lim.snapshot().reserved_mib, 2000);
        assert!(lim.try_admit(NO_TICKET, 100).is_err());
        drop(adopted);
        assert_eq!(lim.snapshot().reserved_mib, 0);
    }

    // --- the disk gate (ADR-0024 §1) -----------------------------------------
    //
    // Several Docker jobs were admitted into a volume with little free space, then
    // more at even less, and filled it within minutes: the memory gates know
    // nothing of disk.

    /// A limiter with memory to spare and a 10 GiB admission floor, so the disk
    /// gate is the only one that can refuse.
    fn disk_limiter(free_mib: u64) -> Arc<Limiter> {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(1 << 20);
        lim.set_disk_admission(DiskAdmission::Floor(10));
        lim.set_disk_free_for_test(Some(free_mib), now_ms());
        lim
    }

    #[test]
    fn the_disk_gate_charges_the_job_it_admits_and_the_ramping_ones() {
        // 16 GiB free over a 10 GiB floor. Each admission holds 3 GiB for the next
        // ten minutes, and the job being admitted is charged its own 3 GiB too
        // (ADR-0032 §1): the second lands exactly on the floor — allowed — and
        // the third is refused.
        let lim = disk_limiter(16 * 1024);
        for _ in 0..2 {
            assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        }
        let refused = lim.try_admit(NO_TICKET, 100).unwrap_err();
        assert_eq!(
            refused,
            Wait::Disk {
                free_mib: 16 * 1024,
                volume: crate::host::DiskVolume::Docker,
                ramping: 2,
                held_mib: 6 * 1024,
                charge_mib: DISK_CHARGE_MIB,
                floor_gib: 10,
            }
        );
        assert_eq!(
            refused.view(),
            WaitView::new(
                "disk",
                "16 GiB free on the Docker volume (6 GiB held for 2 jobs still starting), \
                 needs 3 GiB above the 10 GiB admission floor"
            )
        );
        // Ten minutes on, those jobs have written what they were going to, and the
        // reading and the job's own charge alone decide again.
        for r in lim.inner.lock().unwrap().disk.ramp.iter_mut() {
            r.at_ms -= DISK_RAMP_WINDOW_MS;
        }
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        assert_eq!(lim.snapshot().disk.ramping, 1, "the aged-out two forgotten");
    }

    // --- A ramp charge is what a job has yet to write -------------------------
    //
    // In November 2026 a Linux VM with limited free space admitted three or four
    // jobs, then nothing for up to ten minutes while free space barely moved. Each
    // charge was held whole on top of a reading that already showed the job's
    // writes.

    #[test]
    fn what_ramping_jobs_have_written_is_not_held_twice() {
        let lim = disk_limiter(16 * 1024);
        for _ in 0..2 {
            assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        }
        assert!(
            lim.try_admit(NO_TICKET, 100).is_err(),
            "6 GiB held, as before"
        );
        // The two write 4 GiB between them: 12 GiB free, 2 GiB still to come.
        lim.set_disk_free_for_test(Some(12 * 1024), now_ms());
        assert_eq!(lim.snapshot().disk.held_mib, 2 * 1024);
        // 12 - 2 - 3 = 7 GiB, under the floor: still refused, on the real figure.
        assert!(lim.try_admit(NO_TICKET, 100).is_err());
        // The janitor frees 4 GiB: 16 GiB free, judged as before the window
        // started writing — 16 - 6 - 3 = 7 GiB — so nothing is credited twice.
        lim.set_disk_free_for_test(Some(16 * 1024), now_ms());
        assert_eq!(lim.snapshot().disk.held_mib, 6 * 1024);
        // Plenty more room: admitted, with the two still ramping.
        lim.set_disk_free_for_test(Some(30 * 1024), now_ms());
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
    }

    #[test]
    fn the_credit_never_exceeds_the_charges() {
        // A job that writes far more than its charge frees no one else's: the
        // credit stops at what the window was charged, and the reading does the rest.
        let mut disk = DiskLedger::default();
        let at = |free_mib| DiskReading {
            free_mib,
            at_ms: 1_000,
            volume: crate::host::DiskVolume::Docker,
        };
        disk.admitted(1_000, DISK_CHARGE_MIB, Some(at(40 * 1024)));
        assert_eq!(disk.held_against(2_000, Some(at(39 * 1024))), (1, 2 * 1024));
        assert_eq!(disk.held_against(2_000, Some(at(20 * 1024))), (1, 0));
        // Free space that rose is no write: the whole charge stands.
        assert_eq!(
            disk.held_against(2_000, Some(at(45 * 1024))),
            (1, DISK_CHARGE_MIB)
        );
    }

    #[test]
    fn writes_are_credited_only_against_the_volume_they_were_measured_on() {
        use crate::host::DiskVolume::{LinuxVm, MacVms};
        let mut disk = DiskLedger::default();
        let reading = |volume, free_mib| DiskReading {
            free_mib,
            at_ms: 1_000,
            volume,
        };
        // A macOS VM, measured on the Mac's volume, and a Docker job on the
        // Linux VM's disk.
        disk.admitted(
            1_000,
            DISK_CHARGE_DIND_MIB,
            Some(reading(MacVms, 100 * 1024)),
        );
        disk.admitted(1_000, DISK_CHARGE_MIB, Some(reading(LinuxVm, 30 * 1024)));
        let gross = DISK_CHARGE_DIND_MIB + DISK_CHARGE_MIB;
        // The Linux disk lost 2 GiB: only the Docker job's charge shrinks.
        assert_eq!(
            disk.held_against(2_000, Some(reading(LinuxVm, 28 * 1024))),
            (2, gross - 2 * 1024)
        );
        // An entry with no baseline is never credited.
        let mut bare = DiskLedger::default();
        bare.admitted(1_000, DISK_CHARGE_MIB, None);
        assert_eq!(
            bare.held_against(2_000, Some(reading(LinuxVm, 1024))),
            (1, DISK_CHARGE_MIB)
        );
        // A stale reading credits nothing.
        let stale = DiskReading {
            at_ms: 0,
            ..reading(LinuxVm, 1024)
        };
        let late = crate::host::DISK_READING_MAX_AGE_MS + 1;
        assert_eq!(disk.held_against(late, Some(stale)).1, gross);
    }

    // --- ADR-0032 §1: the gate charges the job it admits ---------------------
    //
    // In October 2026 nothing had been admitted for ten minutes, so the gate held
    // nothing back, and a dind job got in with free space barely over the floor.
    // It wrote gigabytes within minutes and took Docker down.

    #[test]
    fn a_dind_job_admitted_alone_is_charged_its_own_ten_gib() {
        let lim = disk_limiter(11 * 1024);
        let dind = lim.reservation_for("owner/demo::1::e2e", true);
        assert_eq!(dind.disk_mib, DISK_CHARGE_DIND_MIB);
        let refused = lim.try_admit(NO_TICKET, dind).unwrap_err();
        assert_eq!(
            refused,
            Wait::Disk {
                free_mib: 11 * 1024,
                volume: crate::host::DiskVolume::Docker,
                ramping: 0,
                held_mib: 0,
                charge_mib: DISK_CHARGE_DIND_MIB,
                floor_gib: 10,
            }
        );
        assert_eq!(
            refused.view().detail,
            "11 GiB free on the Docker volume, needs 10 GiB above the 10 GiB admission floor"
        );
        // It needs 20 GiB free, and at 20 it lands exactly on the floor.
        lim.set_disk_free_for_test(Some(20 * 1024 - 1), now_ms());
        assert!(lim.try_admit(NO_TICKET, dind).is_err());
        lim.set_disk_free_for_test(Some(20 * 1024), now_ms());
        assert_eq!(lim.try_admit(NO_TICKET, dind), Ok(()));
        assert_eq!(lim.snapshot().disk.held_mib, DISK_CHARGE_DIND_MIB);
    }

    #[test]
    fn a_light_job_still_runs_down_to_thirteen_gib() {
        // A profile granted no daemon is charged 3 GiB: over a 10 GiB floor it is
        // admitted at 13.5 GiB free and refused at 12.5.
        let lim = disk_limiter(13 * 1024 + 512);
        let light = lim.reservation_for("owner/demo::1::lint", false);
        assert_eq!(light.disk_mib, DISK_CHARGE_MIB);
        assert_eq!(lim.try_admit(NO_TICKET, light), Ok(()));

        let lim = disk_limiter(12 * 1024 + 512);
        assert_eq!(
            lim.try_admit(NO_TICKET, light),
            Err(Wait::Disk {
                free_mib: 12 * 1024 + 512,
                volume: crate::host::DiskVolume::Docker,
                ramping: 0,
                held_mib: 0,
                charge_mib: DISK_CHARGE_MIB,
                floor_gib: 10,
            })
        );
    }

    #[test]
    fn the_charge_follows_the_dind_grant_for_every_class() {
        // The charge is per class of job, decided by the profile's grant — the
        // learned memory caps and a pin change nothing about it.
        let mut pinned = HashMap::new();
        pinned.insert(
            "owner/demo::1::pinned".to_string(),
            container_limits(512, 512, 0.0),
        );
        let lim = Limiter::new(cfg(), &pinned);
        for class in ["owner/demo::1::new", "owner/demo::1::pinned"] {
            assert_eq!(
                lim.reservation_for(class, true).disk_mib,
                DISK_CHARGE_DIND_MIB
            );
            assert_eq!(lim.reservation_for(class, false).disk_mib, DISK_CHARGE_MIB);
        }
        let cohort = ["owner/demo::1::new".to_string()];
        assert_eq!(
            lim.sizing_for("owner/demo::1::pinned", &cohort, true)
                .reservation
                .disk_mib,
            DISK_CHARGE_DIND_MIB
        );
    }

    #[test]
    fn window_charges_of_mixed_classes_sum_what_each_was_charged() {
        // 30 GiB free over a 10 GiB floor leaves 20 GiB to charge. The window keeps
        // each admission's own charge, so a dind job and a light one hold 13 GiB
        // between them — not 6, and not 20.
        let lim = disk_limiter(30 * 1024);
        let dind = lim.reservation_for("owner/demo::1::e2e", true);
        let light = lim.reservation_for("owner/demo::1::lint", false);
        assert_eq!(lim.try_admit(NO_TICKET, dind), Ok(()));
        assert_eq!(lim.try_admit(NO_TICKET, light), Ok(()));
        // 13 held + 10 of its own is 23: the next dind job parks…
        assert_eq!(
            lim.try_admit(NO_TICKET, dind),
            Err(Wait::Disk {
                free_mib: 30 * 1024,
                volume: crate::host::DiskVolume::Docker,
                ramping: 2,
                held_mib: 13 * 1024,
                charge_mib: DISK_CHARGE_DIND_MIB,
                floor_gib: 10,
            })
        );
        let view = lim.snapshot().disk;
        assert!(!view.closed, "a light job still fits: {view:?}");
        // …while light ones still run: 13 + 3, then 16 + 3.
        assert_eq!(lim.try_admit(NO_TICKET, light), Ok(()));
        assert_eq!(lim.try_admit(NO_TICKET, light), Ok(()));
        // 19 + 3 is 22: now a light one parks too, and the gate is closed.
        assert_eq!(
            lim.try_admit(NO_TICKET, light),
            Err(Wait::Disk {
                free_mib: 30 * 1024,
                volume: crate::host::DiskVolume::Docker,
                ramping: 4,
                held_mib: 19 * 1024,
                charge_mib: DISK_CHARGE_MIB,
                floor_gib: 10,
            })
        );
        assert!(lim.snapshot().disk.closed);
        // The dind admission ages out first, and takes its whole 10 GiB with it.
        lim.inner.lock().unwrap().disk.ramp[0].at_ms -= DISK_RAMP_WINDOW_MS;
        let view = lim.snapshot().disk;
        assert_eq!((view.ramping, view.held_mib), (3, 9 * 1024));
        assert_eq!(lim.try_admit(NO_TICKET, dind), Ok(()));
        assert_eq!(lim.snapshot().disk.held_mib, 19 * 1024);
    }

    #[test]
    fn the_disk_view_names_the_charge_a_parked_claim_waits_on() {
        // 15 GiB free over a 10 GiB floor: room for a light job's 3 GiB, not for a
        // dind job's 10. The dashboard says which charge is being waited on.
        let lim = disk_limiter(15 * 1024);
        let dind = lim.reservation_for("owner/demo::1::e2e", true);
        let light = lim.reservation_for("owner/demo::1::lint", false);
        assert_eq!(lim.snapshot().disk.charge_mib, None, "nothing is parked");
        let _light_parked = lim.take_ticket(light);
        assert_eq!(
            lim.snapshot().disk.charge_mib,
            None,
            "the light claim's charge fits, so the gate holds nothing back"
        );
        let _dind_parked = lim.take_ticket(dind);
        let view = lim.snapshot().disk;
        assert_eq!(view.charge_mib, Some(DISK_CHARGE_DIND_MIB));
        assert!(!view.closed, "light jobs still start: {view:?}");
        let json = serde_json::to_value(lim.snapshot()).unwrap();
        assert_eq!(json["disk"]["charge_mib"], DISK_CHARGE_DIND_MIB);
    }

    #[test]
    fn a_disk_refusal_says_what_it_read() {
        let one = Wait::Disk {
            free_mib: 8397,
            volume: crate::host::DiskVolume::Docker,
            ramping: 1,
            held_mib: DISK_CHARGE_MIB,
            charge_mib: DISK_CHARGE_MIB,
            floor_gib: 10,
        };
        assert_eq!(
            one.view().detail,
            "8.2 GiB free on the Docker volume (3 GiB held for 1 job still starting), needs \
             3 GiB above the 10 GiB admission floor"
        );
        // Nothing held, nothing said about it — but the job's own charge always is.
        let none = Wait::Disk {
            free_mib: 170,
            volume: crate::host::DiskVolume::Docker,
            ramping: 0,
            held_mib: 0,
            charge_mib: DISK_CHARGE_DIND_MIB,
            floor_gib: 10,
        };
        assert_eq!(
            none.view().detail,
            "0.2 GiB free on the Docker volume, needs 10 GiB above the 10 GiB admission floor"
        );
    }

    #[test]
    fn a_stale_failed_or_missing_disk_reading_holds_nothing_shut() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(1 << 20);
        lim.set_disk_admission(DiskAdmission::Floor(10));
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()), "never measured");

        lim.set_disk_free_for_test(Some(1024), now_ms());
        assert!(matches!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Disk { .. })
        ));
        // A failed measurement replaces nothing: one `df` that timed out must not
        // let a parked queue through a gate a reading seconds ago had shut.
        lim.set_disk_free_for_test(None, now_ms());
        assert!(matches!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Disk { .. })
        ));
        // A reading nobody has refreshed for two minutes is no evidence.
        let stale = now_ms() - host::DISK_READING_MAX_AGE_MS - 1_000;
        lim.set_disk_free_for_test(Some(1024), stale);
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        let view = lim.snapshot().disk;
        assert_eq!(view.free_mib, Some(1024));
        assert!(!view.fresh && !view.closed, "{view:?}");
        assert!(view.reading_age_s.unwrap() > 120, "{view:?}");
        assert_eq!(lim.disk_short_of_floor(now_ms()), None);
    }

    #[test]
    fn an_unstated_or_off_disk_gate_admits_however_full_but_still_measures() {
        for admission in [DiskAdmission::Unstated, DiskAdmission::Off] {
            let lim = Limiter::new(cfg(), &HashMap::new());
            lim.set_vm_total_for_test(1 << 20);
            lim.set_disk_admission(admission);
            lim.set_disk_free_for_test(Some(0), now_ms());
            assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
            assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
            assert_eq!(
                lim.disk_short_of_floor(now_ms()),
                None,
                "nor does it hold a restart back"
            );
            // Measured and counted all the same, so the dashboard shows what a
            // floor would see before anyone sets one.
            let view = lim.snapshot().disk;
            assert_eq!(view.mode, admission.as_str());
            assert_eq!((view.floor_gib, view.closed), (None, false));
            assert_eq!(
                (view.free_mib, view.ramping, view.held_mib),
                (Some(0), 2, 2 * DISK_CHARGE_MIB)
            );
            assert_eq!(view.charge_mib, None, "and it holds no claim back");
        }
    }

    #[test]
    fn revival_asks_about_the_disk_without_the_ramp_allowance() {
        // A restart kills whatever was ramping, so only the reading itself can
        // hold one back (ADR-0024 §2) — and it admits nothing, so no job's own
        // charge is taken either (ADR-0032 §1 leaves revival as it was).
        let lim = disk_limiter(14 * 1024);
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        assert!(matches!(
            lim.try_admit(NO_TICKET, 100),
            Err(Wait::Disk { .. })
        ));
        assert_eq!(lim.disk_short_of_floor(now_ms()), None);
        lim.set_disk_free_for_test(Some(9 * 1024), now_ms());
        assert_eq!(
            lim.disk_short_of_floor(now_ms()),
            Some(ShortOfRoom {
                free_mib: 9 * 1024,
                volume: crate::host::DiskVolume::Docker,
                held_mib: 0,
                floor_gib: 10,
            })
        );
    }

    #[test]
    fn the_disk_view_contract_field_names() {
        // The dashboard reads these by name, and an unstated host must say so.
        let lim = Limiter::new(cfg(), &HashMap::new());
        assert_eq!(
            serde_json::to_value(lim.snapshot()).unwrap()["disk"],
            serde_json::json!({
                "mode": "unstated", "floor_gib": null, "free_mib": null, "volume": null,
                "reading_age_s": null, "fresh": false, "host_free_mib": null,
                "vm_free_mib": null, "vm_reading_age_s": null, "vm_unreadable": null,
                "ramping": 0, "held_mib": 0, "charge_mib": null, "closed": false,
                "vm_grow": null
            })
        );
        let lim = disk_limiter(8 * 1024);
        let disk = &serde_json::to_value(lim.snapshot()).unwrap()["disk"];
        assert_eq!(disk["mode"], "on");
        assert_eq!(disk["floor_gib"], 10);
        assert_eq!(disk["free_mib"], 8 * 1024);
        assert!(disk["reading_age_s"].as_u64().unwrap() < 60);
        assert_eq!(disk["fresh"], true);
        assert_eq!(disk["closed"], true);
        assert_eq!(disk["volume"], "docker");
    }

    fn vm_reading(free_mib: u64, at_ms: u64) -> host::VmDisk {
        host::VmDisk::Read(DiskReading {
            free_mib,
            at_ms,
            volume: host::DiskVolume::LinuxVm,
        })
    }

    #[test]
    fn a_full_linux_vm_disk_closes_the_gate_whatever_the_host_volume_reads() {
        // #410: 200 GiB free on the host volume, 1 GiB left inside the VM. The
        // gate used to read only the first and admitted into the second.
        let lim = disk_limiter(200 * 1024);
        lim.set_vm_disk_for_test(vm_reading(1024, now_ms()), now_ms());
        let refused = lim.try_admit(NO_TICKET, 100).unwrap_err();
        assert_eq!(
            refused,
            Wait::Disk {
                free_mib: 1024,
                volume: host::DiskVolume::LinuxVm,
                ramping: 0,
                held_mib: 0,
                charge_mib: DISK_CHARGE_MIB,
                floor_gib: 10,
            }
        );
        assert!(
            refused
                .view()
                .detail
                .starts_with("1 GiB free on the Linux VM's disk"),
            "the claim names the volume that is short: {}",
            refused.view().detail
        );
        // The critical floor reads the same tighter reading.
        let fresh = lim.fresh_disk_reading(now_ms()).unwrap();
        assert_eq!(
            (fresh.free_mib, fresh.volume),
            (1024, host::DiskVolume::LinuxVm)
        );
        // `/api/state` says which volume bound, and shows both.
        let view = lim.snapshot().disk;
        assert_eq!(view.volume, Some(host::DiskVolume::LinuxVm));
        assert_eq!(
            (view.free_mib, view.host_free_mib, view.vm_free_mib),
            (Some(1024), Some(200 * 1024), Some(1024))
        );
        assert!(view.closed);
        // Revival asks about the host volume only: a full VM disk is room only
        // Docker can make, so it must not hold Docker down.
        assert_eq!(lim.disk_short_of_floor(now_ms()), None);
        // Room back inside the VM opens the gate again.
        lim.set_vm_disk_for_test(vm_reading(40 * 1024, now_ms()), now_ms());
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
    }

    #[test]
    fn an_unreadable_linux_vm_disk_is_surfaced_and_not_acted_on() {
        let lim = disk_limiter(200 * 1024);
        lim.set_vm_disk_for_test(
            host::VmDisk::Unreadable("vm-disk probe failed: no such image".into()),
            now_ms(),
        );
        // Not breached: the gate is where the host volume alone puts it.
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        let view = lim.snapshot().disk;
        assert_eq!(
            view.vm_unreadable.as_deref(),
            Some("vm-disk probe failed: no such image")
        );
        assert_eq!(view.volume, Some(host::DiskVolume::Docker));
        assert_eq!(view.vm_free_mib, None);
        // A last good reading that has aged out counts for nothing either.
        lim.set_vm_disk_for_test(
            vm_reading(0, now_ms() - host::DISK_READING_MAX_AGE_MS - 1),
            now_ms(),
        );
        assert_eq!(
            lim.snapshot().disk.vm_unreadable,
            None,
            "a reading clears it"
        );
        assert_eq!(lim.try_admit(NO_TICKET, 100), Ok(()));
        // And a host without a Linux VM is unchanged by the call.
        lim.set_vm_disk_for_test(host::VmDisk::Absent, now_ms());
        assert_eq!(lim.snapshot().disk.vm_free_mib, Some(0));
    }

    #[test]
    fn a_probe_failing_with_enospc_keeps_the_gate_shut() {
        // The probe reads a full disk as a zero reading (docker.rs), not as
        // unreadable, so it neither ages out to the Mac volume nor opens the gate.
        let lim = disk_limiter(200 * 1024);
        lim.set_vm_disk_for_test(vm_reading(0, now_ms()), now_ms());
        assert!(lim.try_admit(NO_TICKET, 100).is_err());
        let view = lim.snapshot().disk;
        assert_eq!(view.volume, Some(host::DiskVolume::LinuxVm));
        assert_eq!(view.vm_free_mib, Some(0));
        assert_eq!(view.vm_unreadable, None);
    }

    #[tokio::test]
    async fn a_claim_parked_on_disk_is_admitted_as_soon_as_room_returns() {
        let lim = disk_limiter(2 * 1024);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let admission = tokio::spawn({
            let (lim, seen) = (lim.clone(), seen.clone());
            async move {
                lim.admit(100.into(), move |w| seen.lock().unwrap().push(w.clone()))
                    .await
            }
        });
        until_last_wait(
            &seen,
            Wait::Disk {
                free_mib: 2 * 1024,
                volume: crate::host::DiskVolume::Docker,
                ramping: 0,
                held_mib: 0,
                charge_mib: DISK_CHARGE_MIB,
                floor_gib: 10,
            },
        )
        .await;
        assert!(!admission.is_finished(), "a full disk admits nothing");

        // Room back: the reading wakes the parked claim rather than leaving it to
        // its next retry.
        lim.set_disk_free_for_test(Some(40 * 1024), now_ms());
        let guard = tokio::time::timeout(ADMIT_RETRY / 2, admission)
            .await
            .expect("admitted as soon as there was room")
            .expect("admission task");
        assert_eq!(lim.snapshot().disk.ramping, 1);
        drop(guard);
    }

    #[tokio::test]
    async fn a_failed_provision_leaves_ramping_unchanged() {
        // #89: a GitHub 5xx burst failed dozens of mints in an hour, each holding
        // 3 GiB for ten minutes, and the gate refused with plenty of space free. A
        // guard dropped before any container started gives its charge back with its
        // memory.
        let lim = disk_limiter(40 * 1024);
        let failed = lim.admit(100.into(), |_| {}).await;
        assert_eq!(
            lim.snapshot().disk.ramping,
            1,
            "charged while it provisions"
        );
        drop(failed);
        let disk = lim.snapshot().disk;
        assert_eq!((disk.ramping, disk.held_mib), (0, 0));

        // Giving one back takes only its own entry, not a neighbour's.
        let neighbour = lim.admit(100.into(), |_| {}).await;
        let failed = lim.admit(100.into(), |_| {}).await;
        assert_eq!(lim.snapshot().disk.ramping, 2);
        drop(failed);
        assert_eq!(lim.snapshot().disk.ramping, 1);

        // An adoption was never charged, so it has nothing to give back.
        drop(lim.adopt(100.into()));
        assert_eq!(lim.snapshot().disk.ramping, 1);
        drop(neighbour);
    }

    #[tokio::test]
    async fn a_finished_job_gives_its_ramp_charge_back() {
        // #161: short jobs kept their charge for the whole window after their
        // containers were gone, and a handful of them held tens of GiB on a disk
        // with plenty free.
        let lim = disk_limiter(40 * 1024);
        let running = lim.admit(100.into(), |_| {}).await;
        let ended = lim.admit(100.into(), |_| {}).await;
        assert_eq!(lim.snapshot().disk.held_mib, 2 * DISK_CHARGE_MIB);

        // The one that ends inside the window leaves `held_mib` lower than the
        // one still running, which keeps its charge.
        drop(ended);
        let disk = lim.snapshot().disk;
        assert_eq!((disk.ramping, disk.held_mib), (1, DISK_CHARGE_MIB));

        // Still running when the window passes: aged out, not released.
        lim.inner.lock().unwrap().disk.ramp[0].at_ms -= DISK_RAMP_WINDOW_MS;
        assert_eq!(lim.snapshot().disk.held_mib, 0);
        drop(running);
        assert_eq!(lim.snapshot().disk.ramping, 0);
    }

    #[tokio::test]
    async fn a_running_job_keeps_its_ramp_charge_until_the_window_passes() {
        let lim = disk_limiter(40 * 1024);
        let running = lim.admit(100.into(), |_| {}).await;
        assert_eq!(lim.snapshot().disk.held_mib, DISK_CHARGE_MIB);
        // Nine minutes in, still charged.
        lim.inner.lock().unwrap().disk.ramp[0].at_ms -= 9 * 60 * 1000;
        assert_eq!(lim.snapshot().disk.held_mib, DISK_CHARGE_MIB);
        // Past the window, forgotten while the job still runs.
        lim.inner.lock().unwrap().disk.ramp[0].at_ms -= 60 * 1000;
        assert_eq!(lim.snapshot().disk.held_mib, 0);
        drop(running);
    }

    #[test]
    fn releasing_a_ramp_entry_twice_or_after_it_aged_out_is_a_no_op() {
        let mut disk = DiskLedger::default();
        let a = disk.admitted(1_000, DISK_CHARGE_MIB, None);
        let b = disk.admitted(2_000, DISK_CHARGE_DIND_MIB, None);
        disk.release(a);
        disk.release(a);
        assert_eq!(disk.held(3_000), (1, DISK_CHARGE_DIND_MIB));
        // `b` aged out of the window, then is released: nothing changes.
        let later = 2_000 + DISK_RAMP_WINDOW_MS;
        assert_eq!(disk.held(later), (0, 0));
        disk.release(b);
        disk.release(b);
        assert_eq!(disk.held(3_000), (0, 0));
        // An unknown id takes nothing.
        let c = disk.admitted(4_000, DISK_CHARGE_MIB, None);
        disk.release(c + 99);
        assert_eq!(disk.held(4_000), (1, DISK_CHARGE_MIB));
    }

    #[test]
    fn cores_format_without_trailing_zeros() {
        assert_eq!(fmt_cores(4000), "4");
        assert_eq!(fmt_cores(1250), "1.25");
        assert_eq!(fmt_cores(1500), "1.5");
        assert_eq!(fmt_cores(10_000), "10");
        assert_eq!(fmt_cores(0), "0");
    }

    // --- ADR-0033 rollout step 1: the shadow p75 reservation -----------------
    //
    // Measured and shown, never debited. Each test below pins one clause of §1, or
    // the promise that admission is exactly what it was.

    fn window_of(samples: &[u64]) -> VecDeque<u64> {
        samples.iter().copied().collect()
    }

    fn clean(n: usize) -> VecDeque<bool> {
        std::iter::repeat_n(false, n).collect()
    }

    #[test]
    fn p75_is_the_nearest_rank_of_the_window() {
        assert_eq!(p75(&VecDeque::new()), 0);
        assert_eq!(p75(&window_of(&[7])), 7);
        // 20 samples: rank ceil(0.75 × 20) = 15.
        let w: Vec<u64> = (1..=20).collect();
        assert_eq!(p75(&window_of(&w)), 15);
        // Order does not matter.
        let mut rev = w.clone();
        rev.reverse();
        assert_eq!(p75(&window_of(&rev)), 15);
    }

    #[test]
    fn the_reservation_is_the_cap_until_the_window_is_full() {
        let w = window_of(&[1000; 19]);
        assert_eq!(p75_reservation(&w, &clean(19), 5000, 1.4, 20, false), 5000);
    }

    #[test]
    fn the_reservation_is_the_cap_while_the_window_holds_an_oom() {
        let w = window_of(&[1000; 20]);
        let mut censored = clean(20);
        assert_eq!(p75_reservation(&w, &censored, 5000, 1.4, 20, false), 1400);
        censored[3] = true;
        assert_eq!(p75_reservation(&w, &censored, 5000, 1.4, 20, false), 5000);
    }

    #[test]
    fn the_reservation_is_the_cap_for_a_pinned_class() {
        let w = window_of(&[1000; 20]);
        assert_eq!(p75_reservation(&w, &clean(20), 5000, 1.4, 20, true), 5000);
    }

    #[test]
    fn a_full_window_reserves_min_of_cap_and_p75_times_margin() {
        let w = window_of(&[1000; 20]);
        assert_eq!(p75_reservation(&w, &clean(20), 5000, 1.4, 20, false), 1400);
        // Never above the cap, whatever the margin does to a flat window.
        assert_eq!(p75_reservation(&w, &clean(20), 1200, 1.4, 20, false), 1200);
    }

    /// The case the ADR is about: a class that is mostly ~2 GiB with one rare 9 GiB
    /// run reserves its worst case today.
    #[test]
    fn a_skewed_class_reserves_far_less_than_its_cap() {
        let mut c = cfg();
        c.window = 20;
        c.margin = 1.4;
        c.profiling_cap_mib = 16 * 1024;
        let lim = Limiter::new(c, &HashMap::new());
        let k = "owner/demo::1::visual";
        for i in 0..20 {
            let peak = if i == 7 { 9 * 1024 } else { 2 * 1024 };
            lim.record_run(k, "visual", &report(peak, 100));
        }
        let view = class_view(&lim, k);
        // Cap: 9 GiB × 1.4. Shadow: p75 = 2 GiB × 1.4.
        assert_eq!(view.runner_mib, (9.0 * 1024.0 * 1.4_f64).ceil() as u64);
        assert_eq!(
            view.reserve_p75_runner_mib,
            (2.0 * 1024.0 * 1.4_f64).ceil() as u64
        );
        assert!(
            view.reserve_p75_runner_mib * 4 < view.runner_mib,
            "{} vs {}",
            view.reserve_p75_runner_mib,
            view.runner_mib
        );
        assert_eq!(
            view.reserve_p75_mib,
            view.reserve_p75_runner_mib + view.reserve_p75_dind_mib
        );
        assert!(view.reserve_p75_mib < view.runner_mib + view.dind_mib);
    }

    #[test]
    fn an_oom_run_holds_the_shadow_at_the_cap_until_it_ages_out() {
        let mut c = cfg();
        c.window = 4;
        c.margin = 1.5;
        let lim = Limiter::new(c, &HashMap::new());
        let k = "owner/demo::1::build";
        for _ in 0..4 {
            lim.record_run(k, "build", &report(1000, 100));
        }
        lim.record_run(
            k,
            "build",
            &RunReport {
                oom_runner: true,
                ..report(1000, 100)
            },
        );
        let v = class_view(&lim, k);
        assert_eq!(
            v.reserve_p75_runner_mib, v.runner_mib,
            "an OOM is in the window"
        );
        // Four clean runs later it has aged out.
        for _ in 0..4 {
            lim.record_run(k, "build", &report(1000, 100));
        }
        let v = class_view(&lim, k);
        assert_eq!(v.reserve_p75_runner_mib, 1500);
    }

    #[test]
    fn the_view_carries_the_peak_window_and_censored_flags() {
        let mut c = cfg();
        c.window = 3;
        let lim = Limiter::new(c, &HashMap::new());
        let k = "owner/demo::1::build";
        lim.seed_censored(k, "build", (Some(900), false), (Some(100), false));
        lim.seed_censored(k, "build", (Some(1000), true), (Some(110), false));
        lim.seed_censored(k, "build", (Some(800), false), (Some(120), false));
        let v = class_view(&lim, k);
        assert_eq!(v.peaks_runner_mib, vec![900, 1000, 800]);
        assert_eq!(v.peaks_dind_mib, vec![100, 110, 120]);
        assert_eq!(v.censored_runner, vec![false, true, false]);
        assert_eq!(v.censored_dind, vec![false, false, false]);
        // The window bound is the limiter's own: the oldest sample ages out.
        lim.seed_censored(k, "build", (Some(700), false), (Some(130), false));
        let v = class_view(&lim, k);
        assert_eq!(v.peaks_runner_mib, vec![1000, 800, 700]);
        assert_eq!(v.censored_runner, vec![true, false, false]);
        // A run with no dind sample adds to the runner's window only: the two
        // windows are separate, each with its own flags.
        lim.seed_censored(k, "build", (Some(600), true), (None, false));
        let v = class_view(&lim, k);
        assert_eq!(v.peaks_runner_mib, vec![800, 700, 600]);
        assert_eq!(v.censored_runner, vec![false, false, true]);
        assert_eq!(v.peaks_dind_mib, vec![110, 120, 130]);
        assert_eq!(v.censored_dind, vec![false, false, false]);
        // A class with no samples serves empty vectors.
        let fresh = Limiter::new(cfg(), &HashMap::new());
        fresh.seed_censored(k, "build", (None, false), (None, false));
        let v = class_view(&fresh, k);
        assert!(v.peaks_runner_mib.is_empty() && v.peaks_dind_mib.is_empty());
        assert!(v.censored_runner.is_empty() && v.censored_dind.is_empty());
    }

    #[test]
    fn a_restart_replays_the_censored_flag() {
        let mut c = cfg();
        c.window = 3;
        c.margin = 1.5;
        let lim = Limiter::new(c, &HashMap::new());
        let k = "owner/demo::1::build";
        lim.seed_censored(k, "build", (Some(1000), false), (Some(100), false));
        lim.seed_censored(k, "build", (Some(1000), true), (Some(100), false));
        lim.seed_censored(k, "build", (Some(1000), false), (Some(100), false));
        let v = class_view(&lim, k);
        assert_eq!(
            v.reserve_p75_runner_mib, v.runner_mib,
            "a restart must not forget the OOM and drop straight to p75"
        );
    }

    #[test]
    fn a_class_pinned_from_config_shadows_its_pin_from_boot() {
        let pins = HashMap::from([(
            "owner/demo::1::build".to_string(),
            container_limits(8000, 2000, 0.0),
        )]);
        let lim = Limiter::new(cfg(), &pins);
        let v = class_view(&lim, "owner/demo::1::build");
        assert_eq!(
            (v.reserve_p75_runner_mib, v.reserve_p75_dind_mib),
            (8000, 2000)
        );
        let res = lim.reservation_for("owner/demo::1::build", true);
        assert_eq!((res.mib, res.shadow_mib), (10000, 10000));
    }

    #[test]
    fn a_pinned_class_shadows_its_pin() {
        let mut c = cfg();
        c.window = 3;
        let lim = Limiter::new(c, &HashMap::new());
        let k = "owner/demo::1::build";
        for _ in 0..3 {
            lim.record_run(k, "build", &report(1000, 100));
        }
        lim.pin(k, container_limits(8000, 2000, 0.0));
        let v = class_view(&lim, k);
        assert_eq!(
            (v.reserve_p75_runner_mib, v.reserve_p75_dind_mib),
            (8000, 2000)
        );
        lim.unpin(k);
        let v = class_view(&lim, k);
        assert!(v.reserve_p75_runner_mib <= v.runner_mib);
    }

    #[test]
    fn the_view_counts_how_often_p75_would_have_tripped() {
        let mut c = cfg();
        c.window = 4;
        c.margin = 1.0;
        c.profiling_cap_mib = 16 * 1024;
        let lim = Limiter::new(c, &HashMap::new());
        let k = "owner/demo::1::visual";
        // Fill the window: 3 × 1000 and one 9000. p75 = 1000 (rank 3), cap 9000.
        for peak in [1000, 1000, 9000, 1000] {
            lim.record_run(k, "visual", &report(peak, 100));
        }
        let v = class_view(&lim, k);
        assert_eq!((v.runner_mib, v.reserve_p75_runner_mib), (9000, 1000));
        let before = (v.p75_runs, v.p75_trips);
        // Under the would-be reservation: judged, no trip.
        lim.record_run(k, "visual", &report(900, 100));
        let v = class_view(&lim, k);
        assert_eq!((v.p75_runs, v.p75_trips), (before.0 + 1, before.1));
        // Over it, though well under the cap: judged, and it would have tripped.
        lim.record_run(k, "visual", &report(3000, 100));
        let v = class_view(&lim, k);
        assert_eq!((v.p75_runs, v.p75_trips), (before.0 + 2, before.1 + 1));
        // The JSON carries the counters and fields additively.
        let json = serde_json::to_value(lim.snapshot()).unwrap();
        let class = &json["classes"][0];
        for key in [
            "reserve_p75_mib",
            "reserve_p75_runner_mib",
            "reserve_p75_dind_mib",
            "p75_runs",
            "p75_trips",
        ] {
            assert!(class.get(key).is_some(), "{key}");
        }
        assert!(json.get("reserved_p75_mib").is_some());
    }

    #[test]
    fn a_class_still_at_its_cap_is_never_judged() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        let k = "owner/demo::1::build";
        // Window of 3 not yet full for the first runs: shadow == cap.
        lim.record_run(k, "build", &report(1000, 100));
        lim.record_run(k, "build", &report(5000, 100));
        let v = class_view(&lim, k);
        assert_eq!((v.p75_runs, v.p75_trips), (0, 0));
    }

    /// The promise of step 1: admission debits exactly what it debited before, and
    /// the p75 total is a second number beside it.
    #[test]
    fn admission_debits_the_cap_not_the_shadow() {
        let mut c = cfg();
        c.window = 4;
        c.margin = 1.0;
        c.profiling_cap_mib = 16 * 1024;
        let lim = Limiter::new(c, &HashMap::new());
        lim.set_vm_total_for_test(64 * 1024);
        let k = "owner/demo::1::visual";
        for peak in [1000, 1000, 9000, 1000] {
            lim.record_run(k, "visual", &report(peak, 0));
        }
        let res = lim.reservation_for(k, false);
        assert!(res.shadow_mib < res.mib, "the shadow is smaller: {res:?}");
        assert_eq!(res.mib, 9000, "and the debit is the cap");

        let ticket = lim.take_ticket(res);
        lim.try_admit_charged(ticket.id, res).unwrap();
        let v = lim.snapshot();
        assert_eq!(v.reserved_mib, 9000, "Gate 1 debited the cap");
        assert_eq!(
            v.reserved_p75_mib, res.shadow_mib,
            "the shadow is only shown"
        );

        // And Gate 1 still refuses on the cap: 9000 + 9000 > a 14 GiB budget.
        lim.set_vm_total_for_test(2048 + 14 * 1024);
        assert!(matches!(
            lim.try_admit(NO_TICKET, res),
            Err(Wait::Memory { need_mib: 9000, .. })
        ));
    }

    #[test]
    fn the_shadow_total_leaves_with_its_job() {
        let lim = Limiter::new(cfg(), &HashMap::new());
        lim.set_vm_total_for_test(64 * 1024);
        let guard = lim.adopt(Reservation {
            mib: 3000,
            shadow_mib: 1200,
            ..Default::default()
        });
        assert_eq!(lim.snapshot().reserved_p75_mib, 1200);
        assert_eq!(lim.snapshot().reserved_mib, 3000);
        drop(guard);
        assert_eq!(lim.snapshot().reserved_p75_mib, 0);
    }

    // --- ADR-0033 rollout step 2: reserve = "max" | "p75" ----------------------

    use crate::config::ReserveSetting;
    use crate::docker::MemEvents;

    /// A skewed class (three runs at 1000, one at 9000, window 4, margin 1.0): a
    /// 9000 MiB cap and a 1000 MiB p75 reservation, on a VM with room to spare.
    fn skewed(reserve: ReserveSetting) -> (Arc<Limiter>, &'static str) {
        let mut c = cfg();
        c.window = 4;
        c.margin = 1.0;
        c.profiling_cap_mib = 16 * 1024;
        c.reserve = reserve;
        let lim = Limiter::new(c, &HashMap::new());
        lim.set_vm_total_for_test(64 * 1024);
        let k = "owner/demo::1::visual";
        for peak in [1000, 1000, 9000, 1000] {
            lim.record_run(k, "visual", &report(peak, 0));
        }
        (lim, k)
    }

    #[tokio::test]
    async fn absent_and_max_debit_the_cap_exactly_as_before() {
        for setting in [ReserveSetting::Unstated, ReserveSetting::Max] {
            let (lim, k) = skewed(setting.clone());
            let res = lim.reservation_for(k, false);
            assert_eq!((res.mib, res.shadow_mib), (9000, 1000));
            let guard = lim.admit(res, |_| {}).await;
            assert_eq!(
                guard.reservation().mib,
                9000,
                "{setting:?}: the label says the cap"
            );
            let v = lim.snapshot();
            assert_eq!(v.reserved_mib, 9000, "{setting:?}");
            assert_eq!((v.reserve.mode, v.reserve.in_force), ("max", "max"));
            assert!(!v.reserve.freeze_armed, "no memory freeze under max");
            // A freeze or a host OOM trips nothing under max.
            assert!(lim.trip_reserve(TripCause::Freeze, now_ms()).is_none());
            assert!(lim.trip_reserve(TripCause::HostOom, now_ms()).is_none());
            lim.observe_memory_for_test(Some(100), now_ms());
            lim.observe_memory_for_test(None, now_ms());
            assert!(lim.take_reserve_events().is_empty());
            assert!(!lim.may_freeze_for_memory(now_ms()));
            drop(guard);
            assert_eq!(lim.snapshot().reserved_mib, 0);
        }
    }

    #[test]
    fn an_unknown_reserve_value_runs_as_max() {
        let (lim, k) = skewed(ReserveSetting::Unknown("p50".into()));
        assert_eq!(lim.reserve_mode(), ReserveMode::Max);
        let res = lim.reservation_for(k, false);
        lim.try_admit(NO_TICKET, res).unwrap();
        assert_eq!(lim.snapshot().reserved_mib, 9000);
        assert_eq!(lim.snapshot().reserve.unknown.as_deref(), Some("p50"));
    }

    #[tokio::test]
    async fn p75_debits_the_shadow_and_the_guard_and_label_carry_it() {
        let (lim, k) = skewed(ReserveSetting::P75);
        let res = lim.reservation_for(k, false);
        // The reservation still carries the cap: the container limit is the cap.
        assert_eq!((res.mib, res.shadow_mib), (9000, 1000));
        let (limits, _) = lim.limits_for(k, false);
        assert_eq!(
            limits.runner.memory_mib, 9000,
            "the container cap is unchanged"
        );
        let guard = lim.admit(res, |_| {}).await;
        assert_eq!(guard.reservation().mib, 1000, "what the label records");
        let v = lim.snapshot();
        assert_eq!(v.reserved_mib, 1000, "Gate 1 debited the p75 reservation");
        assert_eq!((v.reserve.mode, v.reserve.in_force), ("p75", "p75"));
        drop(guard);
        assert_eq!(
            lim.snapshot().reserved_mib,
            0,
            "and gives back what it debited"
        );
    }

    #[test]
    fn p75_admits_what_the_cap_would_refuse() {
        // A 14 GiB budget: two 9000 MiB caps do not fit, two 1000 MiB p75s do.
        for (setting, second_fits) in [(ReserveSetting::Max, false), (ReserveSetting::P75, true)] {
            let (lim, k) = skewed(setting.clone());
            lim.set_vm_total_for_test(2048 + 14 * 1024);
            let res = lim.reservation_for(k, false);
            lim.try_admit(NO_TICKET, res).unwrap();
            assert_eq!(
                lim.try_admit(NO_TICKET, res).is_ok(),
                second_fits,
                "{setting:?}"
            );
        }
    }

    #[test]
    fn a_trip_falls_back_to_the_cap_until_the_cool_off_ends() {
        let (lim, k) = skewed(ReserveSetting::P75);
        let now = now_ms();
        assert!(matches!(
            lim.trip_reserve(TripCause::HostOom, now),
            Some(reserve::Event::Tripped { .. })
        ));
        let res = lim.reservation_for(k, false);
        lim.try_admit(NO_TICKET, res).unwrap();
        assert_eq!(lim.snapshot().reserved_mib, 9000, "cooling off: the cap");
        let v = lim.snapshot().reserve;
        assert_eq!((v.mode, v.in_force, v.cooling), ("p75", "max", true));
        assert_eq!(v.trips_7d, 1);
        assert!(
            lim.reserving_p75(now + reserve::DAY_MS),
            "re-armed after 24 h"
        );
    }

    #[test]
    fn under_p75_the_head_set_aside_holds_the_heads_p75() {
        let (lim, k) = skewed(ReserveSetting::P75);
        lim.set_vm_total_for_test(2048 + 14 * 1024); // budget 14336
        let head = lim.take_ticket(lim.reservation_for(k, false));
        let other = Reservation {
            mib: 12_000,
            shadow_mib: 12_000,
            ..Default::default()
        };
        // 12000 + the head's 1000 fits; under max the head would hold 9000.
        assert!(lim.try_admit(NO_TICKET, other).is_ok());
        drop(head);
        let (max, k) = skewed(ReserveSetting::Max);
        max.set_vm_total_for_test(2048 + 14 * 1024);
        let _head = max.take_ticket(max.reservation_for(k, false));
        assert!(matches!(
            max.try_admit(NO_TICKET, other),
            Err(Wait::Memory {
                setaside_mib: 9000,
                ..
            })
        ));
    }

    #[test]
    fn a_failed_probe_after_a_low_reading_trips_the_fallback() {
        let (lim, _) = skewed(ReserveSetting::P75);
        let now = now_ms();
        // cfg(): headroom 1024.
        lim.observe_memory_for_test(Some(1000), now);
        assert!(lim.take_reserve_events().is_empty());
        lim.observe_memory_for_test(None, now + 8_000);
        let events = lim.take_reserve_events();
        assert!(
            matches!(
                events.as_slice(),
                [reserve::Event::Tripped {
                    trip: reserve::Trip {
                        cause: TripCause::Probe,
                        ..
                    },
                    ..
                }]
            ),
            "{events:?}"
        );
        assert!(!lim.reserving_p75(now + 9_000));
        // Through the squeeze the freeze is still possible; once memory is back
        // above headroom + band, the cool-off refuses it.
        assert!(lim.may_freeze_for_memory(now + 9_000));
        lim.observe_memory_for_test(Some(1024 + 512 + 1), now + 16_000);
        assert!(!lim.may_freeze_for_memory(now + 17_000));
        // Every reading reaches the memory freeze.
        let readings = lim.memory_readings();
        assert_eq!(readings.borrow().seq, 3);
        assert_eq!(readings.borrow().available_mib, Some(1537));
    }

    // §5 — the learner, under "p75" only.

    fn host_killed_runner() -> RunReport {
        RunReport {
            oom_runner: true,
            runner_mem_events: Some(MemEvents {
                oom: 0,
                oom_kill: 1,
            }),
            ..report(5000, 200)
        }
    }

    #[test]
    fn under_max_a_host_oom_is_learned_exactly_as_today() {
        let r = host_killed_runner();
        let learned = learnable_under(ReserveMode::Max, &r).expect("learned");
        assert!(learned.oom_runner, "still a censored OOM");
        assert_eq!(learned.runner_peak_mib, Some(5000));
    }

    #[test]
    fn under_p75_a_host_oom_is_kept_out_of_the_learner() {
        assert!(learnable_under(ReserveMode::P75, &host_killed_runner()).is_none());
        // A dind whose subtree saw a host kill, its own cap not breached.
        let dind_host = RunReport {
            dind_mem_events: Some(MemEvents {
                oom: 0,
                oom_kill: 2,
            }),
            ..report(1000, 900)
        };
        assert!(learnable_under(ReserveMode::P75, &dind_host).is_none());
    }

    #[test]
    fn under_p75_a_raised_oom_stays_a_class_sample() {
        // The runner's own cap: `oom` raised.
        let breach = RunReport {
            oom_runner: true,
            runner_mem_events: Some(MemEvents {
                oom: 1,
                oom_kill: 1,
            }),
            ..report(5000, 200)
        };
        assert!(
            learnable_under(ReserveMode::P75, &breach)
                .unwrap()
                .oom_runner
        );
        // No reading at all: an OOMKilled runner is a class OOM, as today.
        let unread = RunReport {
            oom_runner: true,
            ..report(5000, 200)
        };
        assert!(
            learnable_under(ReserveMode::P75, &unread)
                .unwrap()
                .oom_runner
        );
        // A host kill in the runner beside dind's own cap breach: dind is still
        // learned as a censored OOM, the runner's truncated sample is dropped.
        let both = RunReport {
            oom_dind: true,
            ..host_killed_runner()
        };
        let learned = learnable_under(ReserveMode::P75, &both).expect("dind's breach");
        assert!(!learned.oom_runner && learned.oom_dind);
        assert_eq!(learned.runner_peak_mib, None);
        // A run with no OOM at all is untouched.
        let plain = report(1000, 100);
        assert_eq!(
            learnable_under(ReserveMode::P75, &plain)
                .unwrap()
                .runner_peak_mib,
            Some(1000)
        );
    }

    #[test]
    fn the_learner_bumps_on_a_host_oom_under_max_and_not_under_p75() {
        for (setting, bumped) in [(ReserveSetting::Max, true), (ReserveSetting::P75, false)] {
            let mut c = cfg();
            c.reserve = setting.clone();
            let lim = Limiter::new(c, &HashMap::new());
            let k = "owner/demo::1::e2e";
            for _ in 0..3 {
                lim.record_run(k, "e2e", &report(1000, 100));
            }
            let before = class_view(&lim, k).runner_mib;
            if let Some(learned) = lim.learnable(&host_killed_runner()) {
                lim.record_run(k, "e2e", &learned);
            }
            let after = class_view(&lim, k).runner_mib;
            assert_eq!(after > before, bumped, "{setting:?}: {before} -> {after}");
        }
    }

    #[test]
    fn the_state_view_carries_the_reserve_block_additively() {
        let (lim, _) = skewed(ReserveSetting::P75);
        let json = serde_json::to_value(lim.snapshot()).unwrap();
        let r = &json["reserve"];
        for key in [
            "mode",
            "in_force",
            "cooling",
            "squeeze",
            "freeze_armed",
            "trips_7d",
            "last_trip",
            "next_rearm_ms",
            "next_cooloff_ms",
            "trips",
        ] {
            assert!(r.get(key).is_some(), "{key}: {r}");
        }
        assert_eq!(r["mode"], "p75");
    }
}

#[cfg(test)]
mod vm_admission_tests {
    use super::*;
    use crate::machine::{MachineReading, PressureLevel};

    fn cfg() -> AutoLimitConfig {
        AutoLimitConfig {
            reserve_mib: 2048,
            headroom_mib: 1024,
            band_mib: 512,
            profiling_cap_mib: 4096,
            margin: 1.5,
            escalation_frac: 0.5,
            window: 3,
            native_concurrency: 2,
            probe_image: "busybox".to_string(),
            hard_ceiling: HardCeiling::Off,
            cpu: crate::config::CpuConfig::default(),
            reserve: crate::config::ReserveSetting::Unstated,
            priority_repos: Vec::new(),
        }
    }

    fn lim() -> Arc<Limiter> {
        let l = Limiter::new(cfg(), &HashMap::new());
        // An example machine: 32 GiB keeping 16, so the share leaves 16 GiB.
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
        l
    }

    fn red() -> Judgement {
        Judgement::Red("pf is off".to_string())
    }

    fn gate_of(w: Result<VmRun, Wait>) -> &'static str {
        match w {
            Err(w) => w.view().gate.leak(),
            Ok(_) => "admitted",
        }
    }

    #[test]
    fn a_wall_nobody_has_read_admits_no_vm_job() {
        let l = lim();
        assert_eq!(gate_of(l.vm_begin(None)), "wall");
        assert_eq!(l.snapshot().vm.wall, "unread");
    }

    #[test]
    fn a_green_wall_admits_two_macos_vms_and_not_a_third() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        // No Linux VM here: two 8 GiB macOS VMs fill the 16 GiB share exactly.
        let a = l.vm_begin(None).expect("first");
        let _b = l.vm_begin(None).expect("second");
        assert_eq!(gate_of(l.vm_begin(None)), "macos_cap");
        assert_eq!(l.snapshot().vm.macos_running, 2);
        // A finished job gives its slot back.
        drop(a);
        assert_eq!(l.snapshot().vm.macos_running, 1);
        let _c = l.vm_begin(None).expect("slot freed");
    }

    #[test]
    fn the_layout_sets_the_macos_cap_and_what_each_vm_takes() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        // ADR-0051: one macOS VM on this host.
        l.set_vm_layout(crate::config::VmLayout {
            macos_vms: 1,
            ..Default::default()
        });
        let a = l.vm_begin(None).expect("first");
        assert_eq!(gate_of(l.vm_begin(None)), "macos_cap");
        assert_eq!(l.snapshot().vm.macos_cap, 1);
        drop(a);
        // Two at 12 GiB: a 16 GiB share holds one.
        l.set_vm_layout(crate::config::VmLayout {
            macos_vm_mib: 12 * 1024,
            ..Default::default()
        });
        let _b = l.vm_begin(None).expect("one fits");
        assert_eq!(gate_of(l.vm_begin(None)), "vm_memory");
        assert_eq!(l.snapshot().vm.macos_vm_mib, 12 * 1024);
        // None: the class is closed.
        drop(_b);
        l.set_vm_layout(crate::config::VmLayout {
            macos_vms: 0,
            ..Default::default()
        });
        assert_eq!(gate_of(l.vm_begin(None)), "macos_cap");
    }

    #[test]
    fn an_adopted_vm_counts_against_the_cap_whatever_the_gates_say() {
        let l = lim();
        // The wall is unread, which would refuse an admission: an adoption is not one.
        let adopted = l.vm_adopt();
        assert_eq!(l.snapshot().vm.macos_running, 1);
        l.observe_wall(Judgement::Green);
        let _b = l.vm_begin(None).expect("one more fits");
        assert_eq!(gate_of(l.vm_begin(None)), "macos_cap");
        drop(adopted);
        assert_eq!(l.snapshot().vm.macos_running, 1);
    }

    #[test]
    fn a_held_vm_counts_against_the_cap_though_no_job_runs_in_it() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        // One VM adoption holds (stopping, unmanaged or pre-instance): it is no
        // running job, but Apple's licence counts it.
        let a = l.vm_begin_with_held(1, None).expect("one slot is left");
        assert_eq!(l.snapshot().vm.macos_running, 1);
        assert_eq!(gate_of(l.vm_begin_with_held(1, None)), "macos_cap");
        // The held VM stopped: its slot is back.
        let _b = l.vm_begin_with_held(0, None).expect("the hold is gone");
        drop(a);
    }

    #[test]
    fn two_held_vms_leave_no_macos_slot() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        l.set_linux_vm_mib(8 * 1024);
        assert_eq!(gate_of(l.vm_begin_with_held(2, None)), "macos_cap");
    }

    #[test]
    fn the_linux_vm_leaves_the_share_less_for_macos() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        // 16 GiB share, Linux VM at 8 GiB: one 8 GiB macOS VM fits, a second does not.
        l.set_linux_vm_mib(8 * 1024);
        let _a = l.vm_begin(None).expect("first");
        assert_eq!(gate_of(l.vm_begin(None)), "vm_memory");
    }

    #[test]
    fn an_undeclared_share_admits_no_vm() {
        let l = Limiter::new(cfg(), &HashMap::new());
        l.observe_wall(Judgement::Green);
        assert_eq!(gate_of(l.vm_begin(None)), "share");
    }

    /// The poller's path end to end: the verdict file the root checker wrote,
    /// judged by the reader, gates admission.
    #[test]
    fn the_verdict_file_as_read_opens_and_closes_vm_admission() {
        use crate::wall::reader::tests::{evidence, green_at, NOW};
        use crate::wall::reader::{judge_reading, MAX_AGE_SECS};
        let l = lim();
        l.observe_wall(judge_reading(Ok(evidence(&green_at(NOW))), NOW).judgement);
        let running = l.vm_begin(None).expect("a fresh green file admits");
        let mut red = green_at(NOW);
        red.state = crate::wall::report::State::Red;
        let mut old = green_at(NOW);
        old.rules_version = 0;
        for (what, reading) in [
            ("red", judge_reading(Ok(evidence(&red)), NOW)),
            ("out of date", judge_reading(Ok(evidence(&old)), NOW)),
            (
                "stale",
                judge_reading(Ok(evidence(&green_at(NOW - MAX_AGE_SECS - 1))), NOW),
            ),
            ("unreadable", judge_reading(Err("ENOENT".into()), NOW)),
        ] {
            l.observe_wall(reading.judgement);
            assert_eq!(gate_of(l.vm_begin(None)), "wall", "{what}");
        }
        drop(running);
    }

    struct Probes(std::sync::Mutex<Vec<Result<(), String>>>);

    impl crate::vm::session::Handshake for Probes {
        async fn reach(&self) -> Result<(), String> {
            self.0.lock().unwrap().remove(0)
        }
    }

    /// ADR-0051's "no VM session" gate: absent until the record's session
    /// account turns it on, closed until a probe answers, and fed by each
    /// probe after, ahead of the class cap and behind the wall.
    #[tokio::test]
    async fn no_vm_session_refuses_macos_jobs_until_its_supervisor_answers() {
        use crate::vm::session::probe_once;
        let l = lim();
        l.observe_wall(Judgement::Green);
        let probes = Probes(std::sync::Mutex::new(vec![
            Err("ignored".into()),
            Err("the supervisor's socket is not there".into()),
            Ok(()),
            Err("connection refused".into()),
        ]));
        // No session account recorded: no gate, and a probe changes nothing.
        probe_once(&probes, &l).await;
        assert_eq!(l.snapshot().vm.session, "not_required");
        let a = l
            .vm_begin(None)
            .expect("ungated, as before ADR-0051's session");
        drop(a);

        l.require_vm_session(SessionMode::Unlock);
        assert_eq!(
            gate_of(l.vm_begin(None)),
            "vm_session",
            "closed until probed"
        );
        let view = l.snapshot().vm;
        assert_eq!(view.session, "unprobed");
        assert_eq!(
            view.refusal.map(|r| r.detail),
            Some(
                "no macOS VM session: unlock the disk as _faber_mac after a reboot, at the \
                 FileVault screen"
                    .to_string()
            )
        );

        probe_once(&probes, &l).await;
        assert_eq!(gate_of(l.vm_begin(None)), "vm_session");
        let view = l.snapshot().vm;
        assert_eq!(view.session, "down");
        assert!(view
            .session_detail
            .unwrap()
            .ends_with("the supervisor's socket is not there"));

        probe_once(&probes, &l).await;
        assert_eq!(l.snapshot().vm.session, "up");
        let a = l.vm_begin(None).expect("the session answers");

        // Down again: the running job keeps its slot, new ones wait, and the
        // words still name the mode.
        probe_once(&probes, &l).await;
        assert_eq!(gate_of(l.vm_begin(None)), "vm_session");
        assert_eq!(l.snapshot().vm.macos_running, 1);
        assert!(l
            .snapshot()
            .vm
            .session_detail
            .unwrap()
            .starts_with("unlock the disk as _faber_mac"));
        drop(a);

        // The wall is still checked first.
        l.observe_wall(red());
        assert_eq!(gate_of(l.vm_begin(None)), "wall");
    }

    /// The record names a session account this host lacks (a setup run that
    /// stopped after writing the record, or a removed account): faber neither
    /// connects nor says "log in", it says to re-run setup; once the account is
    /// there the handshake decides again. The only test that drives the
    /// process-wide account flag.
    #[tokio::test]
    async fn a_missing_session_account_says_re_run_setup_and_is_never_reached() {
        use crate::vm::session::{account_check, account_trusted, probe_checked};
        let l = lim();
        l.observe_wall(Judgement::Green);
        l.require_vm_session(SessionMode::Manual);
        let probes = Probes(std::sync::Mutex::new(vec![Ok(())]));

        probe_checked(&probes, account_check(600, None), &l).await;
        assert!(!account_trusted());
        assert_eq!(
            probes.0.lock().unwrap().len(),
            1,
            "no handshake to that uid"
        );
        assert_eq!(gate_of(l.vm_begin(None)), "vm_session");
        let view = l.snapshot().vm;
        assert_eq!(view.session, "no_account");
        let detail = view.refusal.unwrap().detail;
        assert!(detail.contains("no such account"), "{detail}");
        assert!(detail.ends_with("re-run `sudo faber setup`"), "{detail}");
        assert!(!detail.contains("log _faber_mac in"), "{detail}");

        // A uid the account no longer holds is refused the same way.
        probe_checked(&probes, account_check(600, Some(601)), &l).await;
        assert_eq!(l.snapshot().vm.session, "no_account");

        // The account is there: the handshake decides.
        probe_checked(&probes, account_check(600, Some(600)), &l).await;
        assert!(account_trusted());
        assert_eq!(l.snapshot().vm.session, "up");
        assert!(l.vm_begin(None).is_ok());
    }

    #[test]
    fn a_red_or_out_of_date_wall_stops_new_vms_and_running_ones_keep_their_slot() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        let running = l.vm_begin(None).expect("admitted while green");
        l.observe_wall(red());
        assert_eq!(gate_of(l.vm_begin(None)), "wall");
        assert_eq!(l.snapshot().vm.wall, "red");
        assert_eq!(l.snapshot().vm.macos_running, 1, "the running job finishes");
        l.observe_wall(Judgement::OutOfDate {
            installed: 1,
            wanted: 2,
        });
        assert_eq!(gate_of(l.vm_begin(None)), "wall");
        let view = l.snapshot().vm;
        assert_eq!(view.wall, "out_of_date");
        assert_eq!(view.refusal.map(|r| r.gate), Some("wall".to_string()));
        l.observe_wall(Judgement::Green);
        assert!(l.snapshot().vm.refusal.is_none());
        drop(running);
    }

    #[test]
    fn state_shows_the_supervisors_vms_and_the_image_standing() {
        let l = lim();
        let v = l.snapshot().vm;
        assert!(v.vms.is_empty());
        assert_eq!(v.images, ImagesView::default());
        l.observe_vms(&[VmRecord {
            slot: 1.try_into().unwrap(),
            kind: VmKind::Macos,
            claim: "c1".to_string().try_into().unwrap(),
            state: VmState::ProbeSent,
            stop_reason: None,
            failed: None,
            started_unix: 1.try_into().unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        }]);
        l.observe_linux_image(ImageStanding::OutOfDate);
        l.observe_golden("xcode-27.0", Standing::Verified);
        let json = serde_json::to_value(l.snapshot().vm).unwrap();
        assert_eq!(json["vms"][0]["state"], "probe_sent");
        assert_eq!(json["vms"][0]["kind"], "macos");
        assert_eq!(json["images"]["linux"], "out_of_date");
        assert_eq!(json["images"]["macos"], "verified");
        assert_eq!(json["golden"][0]["image"], "xcode-27.0");
        assert_eq!(json["golden"][0]["standing"], "verified");
        assert!(json["golden"][0]["detail"].is_null());

        // Each image is judged by its own owner: neither setter touches the other.
        l.observe_linux_image(ImageStanding::Current);
        let images = l.snapshot().vm.images;
        assert_eq!(images.linux, ImageStanding::Current);
        assert_eq!(images.macos, Summary::Verified);
        // #268: the worst golden image is what `vm.images.macos` shows.
        l.observe_golden(
            "xcode-26.4",
            Standing::Failed {
                failed: vec![crate::vm::verify::Failure::Guest(
                    crate::vm::proto::Check::Filevault,
                )],
                why: None,
            },
        );
        let json = serde_json::to_value(l.snapshot().vm).unwrap();
        assert_eq!(json["images"]["macos"], "failed:filevault");
        assert_eq!(json["images"]["linux"], "current");
    }

    fn vm_record(kind: VmKind, slot: u8, claim: &str, started: u64) -> VmRecord {
        VmRecord {
            slot: slot.try_into().unwrap(),
            kind,
            claim: claim.to_string().try_into().unwrap(),
            state: VmState::InService,
            stop_reason: None,
            failed: None,
            started_unix: started.try_into().unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        }
    }

    fn macos_job(claim: &str, job_name: Option<&str>) -> VmJobFacts {
        VmJobFacts {
            claim: claim.to_string(),
            cpus: 4,
            memory_mib: 8192,
            repo: "owner/demo".to_string(),
            job_name: job_name.map(str::to_string),
        }
    }

    /// popover/vms/anatomy.md O1: the Linux VM's row carries the engine's size and
    /// no job, and `listed_age_s` is null until a list answers.
    #[test]
    fn the_linux_vm_row_carries_the_engine_size_and_no_job() {
        let l = lim();
        assert_eq!(l.snapshot().vm.listed_age_s, None);
        l.set_linux_vm_size(6, 12288);
        l.observe_vms(&[vm_record(VmKind::Linux, 0, "linux-engine", 1_761_700_000)]);
        let v = l.snapshot().vm;
        assert!(v.listed_age_s.is_some_and(|a| a < 5));
        let json = serde_json::to_value(&v.vms[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "kind": "linux", "slot": 0, "state": "in_service", "stop_reason": null,
                "started_unix": 1_761_700_000u64, "cpus": 6, "memory_mib": 12288,
                "job": null
            })
        );
    }

    /// A macOS row joined from its in-flight claim gets the profile's size and the
    /// job; an unmatched claim stays null.
    #[test]
    fn a_macos_row_is_joined_from_its_in_flight_claim() {
        let l = lim();
        l.observe_vms(&[
            vm_record(VmKind::Macos, 1, "42-1761700000-abcd", 1_761_700_100),
            vm_record(VmKind::Macos, 2, "43-1761700001-abcd", 1_761_700_101),
        ]);
        let mut vms = l.snapshot().vm.vms;
        join_vm_jobs(
            &mut vms,
            &[macos_job("42-1761700000-abcd", Some("release"))],
        );
        assert_eq!(
            serde_json::to_value(&vms[0]).unwrap(),
            serde_json::json!({
                "kind": "macos", "slot": 1, "state": "in_service", "stop_reason": null,
                "started_unix": 1_761_700_100u64, "cpus": 4, "memory_mib": 8192,
                "job": { "repo": "owner/demo", "job_name": "release" }
            })
        );
        let unmatched = serde_json::to_value(&vms[1]).unwrap();
        assert!(unmatched["cpus"].is_null() && unmatched["memory_mib"].is_null());
        assert!(unmatched["job"].is_null());
    }

    /// A fork or untrusted-actor job reaches the row with no job name.
    #[test]
    fn a_macos_row_for_a_fork_job_names_the_repo_only() {
        let l = lim();
        l.observe_vms(&[vm_record(VmKind::Macos, 1, "42-1-abcd", 1)]);
        let mut vms = l.snapshot().vm.vms;
        join_vm_jobs(&mut vms, &[macos_job("42-1-abcd", None)]);
        let json = serde_json::to_value(&vms[0]).unwrap();
        assert_eq!(json["job"]["repo"], "owner/demo");
        assert!(json["job"]["job_name"].is_null());
    }

    /// A live Linux list must not hide a dead macOS supervisor: the age is the
    /// oldest kind's.
    #[test]
    fn listed_age_is_that_of_the_stalest_kind() {
        let l = lim();
        l.observe_vms_of(VmKind::Macos, &[]);
        l.inner
            .lock()
            .unwrap()
            .vm
            .listed_at_ms
            .insert(VmKind::Macos, now_ms() - 60_000);
        l.observe_vms_of(VmKind::Linux, &[]);
        assert!(l.snapshot().vm.listed_age_s.unwrap() >= 60);
    }

    /// Two supervisors, one list each: a list of one kind never clears the other's.
    #[test]
    fn a_list_of_one_kind_leaves_the_other_kinds_rows() {
        let l = lim();
        l.observe_vms_of(
            VmKind::Linux,
            &[vm_record(VmKind::Linux, 0, "linux-engine", 1)],
        );
        l.observe_vms_of(
            VmKind::Macos,
            &[vm_record(VmKind::Macos, 1, "42-1-abcd", 1)],
        );
        assert_eq!(l.snapshot().vm.vms.len(), 2);
        l.observe_vms_of(VmKind::Macos, &[]);
        let vms = l.snapshot().vm.vms;
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].kind, VmKind::Linux);
    }

    /// ADR-0049 §7: no macOS job starts on an image that is not verified, once
    /// the gate is required; a verification run (no image) is not held by it,
    /// and the refusal names the check and the fix.
    #[test]
    fn an_unverified_golden_image_admits_no_macos_job() {
        use crate::vm::verify::{Failure, Why};
        let l = lim();
        l.set_vm_total_for_test(30 * 1024);
        l.observe_wall(Judgement::Green);
        // Not required: nothing is gated, as before.
        assert!(l.vm_begin(Some("img")).is_ok());
        l.require_verified_images();
        let refusal = |l: &Arc<Limiter>| match l.vm_begin(Some("img")) {
            Err(Wait::Vm(r)) => r.describe(),
            Err(other) => panic!("refused by something else: {other:?}"),
            Ok(_) => panic!("admitted"),
        };
        // Never judged: unknown, refused.
        assert_eq!(refusal(&l).0, "golden_image");
        assert!(refusal(&l).1.contains("has not been judged"));
        // A verification run takes a place regardless.
        let held = l
            .vm_begin(None)
            .expect("a verification is not gated on its own image");
        drop(held);
        l.observe_golden("img", Standing::Verifying);
        assert!(
            refusal(&l).1.contains("is being verified"),
            "{}",
            refusal(&l).1
        );
        l.observe_golden(
            "img",
            Standing::Unverified(Why::HostChanged {
                was: "26A434".into(),
                now: "26B12".into(),
            }),
        );
        let (_, text) = refusal(&l);
        assert!(text.contains("26A434 to 26B12"), "{text}");
        assert!(text.contains("faber golden verify img"), "{text}");
        l.observe_golden(
            "img",
            Standing::Failed {
                failed: vec![Failure::Guest(crate::vm::proto::Check::AppleId)],
                why: None,
            },
        );
        let (gate, text) = refusal(&l);
        assert_eq!(gate, "golden_image");
        assert!(text.contains("(apple_id)"), "{text}");
        assert!(text.contains("Apple ID is signed in"), "{text}");
        assert!(text.contains("faber golden prepare --from img"), "{text}");
        // Another image's pass does not open this one.
        l.observe_golden("other", Standing::Verified);
        assert!(l.vm_begin(Some("img")).is_err());
        l.observe_golden("img", Standing::Verified);
        assert!(l.vm_begin(Some("img")).is_ok());
    }

    /// ADR-0049 §7: admission re-checks a verified image's identity every time, so
    /// an image changed since its pass is refused at once, not at the next pass.
    #[test]
    fn a_verified_image_changed_on_disk_is_refused_at_once() {
        use crate::vm::verify::{hash_image, Identity};
        let dir = std::env::temp_dir().join(format!("faber-golden-pin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in crate::setup::vm_images::GOLDEN_FILES {
            std::fs::write(dir.join(f), f.as_bytes()).unwrap();
        }
        let l = lim();
        l.set_vm_total_for_test(30 * 1024);
        l.observe_wall(Judgement::Green);
        l.require_verified_images();
        let mut cache = std::collections::BTreeMap::new();
        let files = hash_image(&dir, &mut cache).unwrap();
        // Pinned to the identity on disk and the host's build, as a pass pins it.
        let build = Identity::now(&dir)
            .map(|i| i.macos_build)
            .unwrap_or_default();
        let pinned = Identity::of_cache(&cache, &files, &build).unwrap();
        l.observe_golden("img", Standing::Verified);
        l.pin_golden("img", dir.clone(), pinned);
        let ok = l.vm_begin(Some("img"));
        if crate::vm::verify::host_build_now().is_ok() {
            assert!(ok.is_ok(), "unchanged: admitted");
        }
        drop(ok);
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("disk"), b"a rebuilt disk").unwrap();
        match l.vm_begin(Some("img")) {
            Err(Wait::Vm(r)) => {
                let (gate, text) = r.describe();
                assert_eq!(gate, "golden_image");
                assert!(
                    text.contains("changed since it was verified")
                        || text.contains("cannot be judged"),
                    "{text}"
                );
            }
            other => panic!("admitted a changed image: {:?}", other.is_ok()),
        }
        assert!(!l.golden("img").admits(), "and the view says so");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_green_wall_nobody_refreshes_goes_red_and_stops_vm_and_docker_admission() {
        let l = lim();
        l.set_vm_total_for_test(30 * 1024);
        l.set_wall_required(true);
        l.observe_wall(Judgement::Green);
        assert!(l.vm_begin(None).is_ok());
        // The poller dies: the verdict is a minute and a second old.
        l.inner.lock().unwrap().vm.wall_at_ms = now_ms() - admit::WALL_MAX_AGE_MS - 1_000;
        assert_eq!(gate_of(l.vm_begin(None)), "wall");
        assert_eq!(l.snapshot().vm.wall, "red");
        assert_eq!(
            l.try_admit(NO_TICKET, 100).map_err(|w| w.view().gate),
            Err("wall".to_string())
        );
        l.observe_wall(Judgement::Green);
        assert!(l.vm_begin(None).is_ok());
    }

    #[test]
    fn the_disk_floor_gates_a_vm_job_and_its_charge_is_given_back() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        l.set_disk_admission(DiskAdmission::Floor(10));
        // 18 GiB free: a 10 GiB charge on top leaves 8, under the 10 GiB floor.
        l.set_disk_free_for_test(Some(18 * 1024), now_ms());
        assert_eq!(gate_of(l.vm_begin(None)), "disk");
        l.set_disk_free_for_test(Some(21 * 1024), now_ms());
        let a = l.vm_begin(None).expect("room for one");
        // The first job's charge is held: the second would not fit.
        assert_eq!(gate_of(l.vm_begin(None)), "disk");
        drop(a);
        let _b = l.vm_begin(None).expect("charge returned with the job");
    }

    #[test]
    fn a_grow_hold_parks_docker_jobs_with_its_reason_and_leaves_vm_jobs_alone() {
        // ADR-0061 §3: the drain before a grow restart holds Docker claims only.
        let l = lim();
        l.observe_wall(Judgement::Green);
        l.set_grow_hold(true);
        assert_eq!(
            l.try_admit(NO_TICKET, 100),
            Err(Wait::Held {
                reason: crate::vm::grow::HOLD_REASON.to_string()
            })
        );
        assert!(l.vm_begin(None).is_ok(), "macOS VM jobs are not affected");
        l.set_grow_hold(false);
        assert!(!matches!(
            l.try_admit(NO_TICKET, 100),
            Err(Wait::Held { .. })
        ));
    }

    #[test]
    fn a_hold_a_pause_and_machine_pressure_stop_a_vm_job_too() {
        let l = lim();
        l.observe_wall(Judgement::Green);
        l.hold("resize".to_string(), 1);
        assert!(matches!(l.vm_begin(None), Err(Wait::Held { .. })));
        l.release_hold();
        l.pause(Pause::Operator);
        assert!(matches!(l.vm_begin(None), Err(Wait::Paused { .. })));
        l.resume(Pause::Operator);
        l.set_machine_for_test(
            Some(MachineReading {
                mem_mib: 32 * 1024,
                cpus: 10,
                pressure: Some(PressureLevel::Critical),
                swap_used_mib: Some(0),
                ..MachineReading::default()
            }),
            now_ms(),
        );
        // The floor of one job in total: the first runs, the second parks.
        let _a = l.vm_begin(None).expect("floor of one");
        assert!(matches!(l.vm_begin(None), Err(Wait::Machine(_))));
    }

    #[test]
    fn docker_admission_ignores_the_wall_until_it_is_required() {
        let l = Limiter::new(cfg(), &HashMap::new());
        l.set_vm_total_for_test(30 * 1024);
        // Unread wall, not required: Docker admits as before.
        assert_eq!(l.try_admit(NO_TICKET, 100), Ok(()));
        l.set_wall_required(true);
        assert_eq!(
            l.try_admit(NO_TICKET, 100).map_err(|w| w.view().gate),
            Err("wall".to_string())
        );
        l.observe_wall(Judgement::Green);
        assert_eq!(l.try_admit(NO_TICKET, 100), Ok(()));
        l.observe_wall(red());
        assert!(l.try_admit(NO_TICKET, 100).is_err());
    }
}
