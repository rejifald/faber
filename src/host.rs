//! Host-level custody of the Docker daemon — ADR-0007.
//!
//! faber is the Docker manager on this box: the only thing that provisions,
//! sizes, runs and reaps Docker workloads. Everything else here is a consumer of
//! Docker; faber is its operator. That makes keeping the daemon *usable* faber's
//! job, not the operator's.
//!
//! [`crate::limiter`]'s Gate 0 already handles the defensive half — a daemon that
//! stops answering closes admission, so no job is fed into a corpse. What was
//! missing is the other half. In the incident that produced this module the
//! daemon wedged so completely that its own `/_ping` timed out, and faber logged
//! `daemon unresponsive` on every check for **hours** while CI was fully
//! blocked, waiting for a human to restart Docker Desktop by hand. The backend
//! process had been stuck for days. Detecting the failure and declining to make
//! it worse is a floor, not the job.
//!
//! Two obligations live here: bring a wedged daemon back, and keep disk pressure
//! from wedging it in the first place. Both are deliberately conservative —
//! under [ADR-0006](../docs/adrs/0006-fleet-autoupdate.md) this code reaches
//! follower hosts nobody is watching.
//!
//! [ADR-0024](../docs/adrs/0024-a-full-disk-stops-admission-not-docker.md)
//! narrowed the first obligation after a host whose disk filled was restarted
//! several times within an hour. Each restart killed the jobs in flight; most
//! booted the VM onto a disk with no room, and some of those logged filesystem
//! errors. So a restart now waits for room ([`short_of_room`]), and a restart
//! counts as having worked only once Docker has stayed up for a whole cooldown
//! after it ([`DockerCustody`]).
//!
//! [ADR-0026](../docs/adrs/0026-job-code-runs-only-inside-vms-faber-owns.md)
//! retargeted the first obligation at a VM faber created. On a set-up macOS host
//! faber's Docker runs in its own Linux VM, and reviving it is asking the VM
//! supervisor to stop it so the engine driver starts it again
//! ([`crate::vm::engine`]). Docker Desktop is never touched: it is the operator's
//! ([`Custodian`]).

use std::time::Duration;

use tracing::{info, warn};

use crate::config::{DiskAdmission, HostPolicy};
use crate::platform::{Layout, Os};

/// Why faber does not revive Docker on a Linux host where it runs as `_faber`
/// (ADR-0025).
///
/// The engine it cannot reach is then the machine's, not faber's to restart from
/// a role account, and ADR-0026's VM runtime is macOS-only for now. Admission
/// stays refused while Docker is unreachable, and nothing is restarted.
pub const NOT_OURS: &str =
    "docker unreachable (faber runs as _faber; ADR-0026 gives it its own VM)";

/// Why faber does not revive Docker on a macOS host that has not been set up
/// (ADR-0026, "Docker Desktop stops being faber's"). The Docker it reaches there
/// is Docker Desktop's, the operator's app, which faber never quits, kills or
/// relaunches.
pub const DESKTOP_NOT_OURS: &str = "docker unreachable (Docker Desktop is the operator's, not \
     faber's to restart under ADR-0026; `sudo faber setup` gives faber a Linux VM of its own)";

/// Who brings a wedged Docker back on this host: ADR-0007, as ADR-0025 and
/// ADR-0026 narrow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Custodian {
    /// faber's own Linux VM (macOS SYSTEM): a restart is a `stop` through the VM
    /// supervisor, after which the engine driver starts it again.
    Vm,
    /// The machine's `docker` service, restarted with `systemctl` (Linux LEGACY).
    Service,
    /// Docker is not faber's to restart, for the reason given.
    NotOurs(&'static str),
}

impl Custodian {
    /// The custodian for a host running `os` in `layout`.
    pub fn for_host(os: Os, layout: Layout) -> Self {
        match (os, layout) {
            (Os::MacOs, Layout::System) => Self::Vm,
            (Os::MacOs, Layout::Legacy) => Self::NotOurs(DESKTOP_NOT_OURS),
            (Os::Linux, Layout::Legacy) => Self::Service,
            (Os::Linux, Layout::System) => Self::NotOurs(NOT_OURS),
        }
    }

    /// This process's custodian. Fixed for its life, like the layout.
    pub fn for_this_host() -> Self {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Self::for_host(Os::HOST, crate::platform::layout())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Self::NotOurs("docker unreachable (reviving it is not implemented on this platform)")
        }
    }
}

/// How long to wait for a restarted daemon to answer before calling the revival
/// failed. It answered within about half a minute of a force-kill when this was
/// built; this is several times that, as headroom for a cold VM boot.
const REVIVE_WAIT: Duration = Duration::from_secs(240);

/// Poll gap while waiting for the daemon to come back.
const REVIVE_POLL: Duration = Duration::from_secs(5);

/// Restarts without a stable period in between before faber stops trying and
/// leaves it to a human. A daemon that keeps wedging after this many has
/// something wrong that restarting cannot fix, and an unattended host must not
/// sit in a restart loop.
///
/// "Stable" is Docker answering for a whole `revive_cooldown_secs` after a
/// restart (ADR-0024 §2). This used to count only *failed* restarts, and the
/// first answer reset it and the cooldown both: a VM on a full disk answers
/// briefly before it wedges again, so on the incident host the guard never
/// engaged, and Docker was restarted over and over.
pub const REVIVE_GIVE_UP_AFTER: u32 = 3;

/// How old a free-space reading of the Docker volume may be and still count as
/// evidence — ADR-0024 §1.
///
/// The limiter's disk loop reads every few seconds, so a reading this old means
/// the measuring has stopped. A figure nobody is refreshing must not hold
/// admission shut, or keep Docker down, on its own.
pub const DISK_READING_MAX_AGE_MS: u64 = 120_000;

/// Which filesystem a free-space reading is of (#410).
///
/// On a Linux host, and on a Mac with no VM home, there is one: the volume
/// Docker writes to. On a macOS SYSTEM host there are two, and they fill
/// separately. Docker's data root is inside the Linux VM, on that VM's own
/// fixed-size disk, which the Mac's `df` cannot see. The VM disks themselves,
/// that one included, are files on the Mac's volume, which a macOS clone and the
/// sparse Linux disk both grow into. A gate that reads only the second is blind
/// to a Linux VM whose disk is full, and in October 2026 jobs died of exactly
/// that with plenty of space free on the Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskVolume {
    /// The volume Docker writes to: dockerd's data-root on Linux, the root
    /// volume on a Mac with no VM home.
    #[default]
    Docker,
    /// The Mac's volume that holds faber's VM disks (macOS SYSTEM).
    MacVms,
    /// The Linux VM's own disk, which holds Docker's data root (macOS SYSTEM).
    LinuxVm,
}

impl DiskVolume {
    /// The volume in a sentence's words: what a log line, a fault and a parked
    /// claim say was short.
    pub fn describe(self) -> &'static str {
        match self {
            DiskVolume::Docker => "the Docker volume",
            DiskVolume::MacVms => "the Mac volume holding the VM disks",
            DiskVolume::LinuxVm => "the Linux VM's disk",
        }
    }
}

/// One measurement of free space on the volume backing Docker, and when it was
/// taken (epoch ms) — ADR-0024. `volume` says which one (#410).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskReading {
    pub free_mib: u64,
    pub at_ms: u64,
    pub volume: DiskVolume,
}

/// The reading a Docker-side gate acts on, of two that may both bear on it: the
/// tighter of those still evidence at `now_ms` (#410).
///
/// A reading that is stale, or missing, gives way to the other, so a Linux VM
/// that cannot be read leaves the gate exactly where one reading left it before:
/// an unknown is not a reason to act. Where neither is fresh, the newer is kept,
/// so the caller still sees how old the evidence is and refuses on it as it did.
/// A tie goes to `a`.
pub fn tighter(a: Option<DiskReading>, b: Option<DiskReading>, now_ms: u64) -> Option<DiskReading> {
    match (a, b) {
        (Some(a), Some(b)) => {
            let fresh = |r: DiskReading| r.fresh_free_mib(now_ms).is_some();
            Some(match (fresh(a), fresh(b)) {
                (true, true) if b.free_mib < a.free_mib => b,
                (true, _) => a,
                (false, true) => b,
                (false, false) if b.at_ms > a.at_ms => b,
                (false, false) => a,
            })
        }
        (a, None) => a,
        (None, b) => b,
    }
}

impl DiskReading {
    /// How old this reading is at `now_ms`.
    pub fn age_ms(self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.at_ms)
    }

    /// Its free space, if it is still evidence at `now_ms`.
    pub fn fresh_free_mib(self, now_ms: u64) -> Option<u64> {
        (self.age_ms(now_ms) <= DISK_READING_MAX_AGE_MS).then_some(self.free_mib)
    }
}

/// The Docker volume found short of the admission floor, with the numbers that
/// say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortOfRoom {
    /// Free space as last measured.
    pub free_mib: u64,
    /// The volume that was measured short (#410).
    pub volume: DiskVolume,
    /// What was counted against it on top: the disk that recently admitted jobs,
    /// and the job being admitted, have yet to write (ADR-0024 §1, ADR-0032 §1).
    /// Zero where nothing is held.
    pub held_mib: u64,
    pub floor_gib: u64,
}

/// Is the Docker volume short of the admission floor, on evidence? — ADR-0024.
///
/// The one question behind both halves of that ADR. The admission gate (§1) asks
/// it with `held_mib` set to what recent admissions have yet to write, plus the
/// charge of the job it is deciding about (ADR-0032 §1). Revival (§2) asks it
/// with nothing held, since a restart kills whatever was ramping and admits
/// nothing.
///
/// `None` means room, or no way to tell: the floor is off or unstated, there is
/// no reading, or the reading is stale. That is the same refusal to act on a
/// guess that [`below_disk_floor`] applies to a failed `df`. Exactly at the floor
/// is not short: the floor is the least free space admission may leave.
pub fn short_of_room(
    admission: DiskAdmission,
    reading: Option<DiskReading>,
    held_mib: u64,
    now_ms: u64,
) -> Option<ShortOfRoom> {
    let floor_gib = admission.floor_gib()?;
    let reading = reading?;
    let free_mib = reading.fresh_free_mib(now_ms)?;
    (free_mib.saturating_sub(held_mib) < floor_gib.saturating_mul(1024)).then_some(ShortOfRoom {
        free_mib,
        volume: reading.volume,
        held_mib,
        floor_gib,
    })
}

/// MiB as GiB to one decimal, without a trailing `.0`: `8397` → `8.2`,
/// `6144` → `6`.
pub fn fmt_gib(mib: u64) -> String {
    let gib = format!("{:.1}", mib as f64 / 1024.0);
    match gib.strip_suffix(".0") {
        Some(whole) => whole.to_string(),
        None => gib,
    }
}

/// What to do about a Docker daemon that is not answering — ADR-0007, as
/// amended by ADR-0024 §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revival {
    /// Restart it now.
    Restart,
    /// Not now: revival is off, the daemon has not been unresponsive long enough
    /// to call it a wedge, or the last restart is inside the cooldown.
    Wait,
    /// A restart is due, but the Docker volume is under the admission floor. A
    /// restart cannot make room, and restarting onto a full disk is what killed
    /// the jobs in flight and left a filesystem reporting data loss (ADR-0024).
    /// The unresponsive clock keeps running, so the restart comes on the first
    /// cycle after room returns.
    NoRoom(ShortOfRoom),
    /// [`REVIVE_GIVE_UP_AFTER`] restarts without a stable period in between.
    /// Restarting has not fixed whatever is wrong, and this needs a human.
    GaveUp { restarts: u32 },
    /// The Docker faber cannot reach is not its to restart, for the reason given
    /// ([`Custodian::NotOurs`]). Never a restart, whatever the policy.
    NotOurs(&'static str),
}

/// Decide what to do about a daemon that has been unresponsive for
/// `unresponsive_ms`.
///
/// Split out from the I/O so the policy is testable without a Docker host. The
/// bounds matter for different reasons. `unresponsive_ms` separates a wedge from
/// a hiccup: a busy daemon under load misses probes routinely, and killing it for
/// that would be far worse than waiting. `since_restart_ms` (`None` when faber has
/// not restarted it since it was last stable) spaces restarts, and `restarts`
/// stops a daemon that keeps wedging from being restarted forever.
///
/// [`Custodian::Vm`] delegates to [`crate::vm::revive::linux_revival`], so the
/// Linux VM has one restart policy, under `revive_vms`; `revive_docker` governs
/// [`Custodian::Service`] only.
///
/// The custodian is checked first. Where Docker is not faber's the answer is
/// always [`Revival::NotOurs`]: no policy, clock or count can make faber quit or
/// relaunch the operator's Docker Desktop (ADR-0026), or the machine's engine from
/// its role account (ADR-0025).
///
/// Room is checked last, so [`Revival::NoRoom`] means exactly "the restart that is
/// due is not happening because of the disk".
pub fn revival(
    custodian: Custodian,
    policy: &HostPolicy,
    unresponsive_ms: u64,
    since_restart_ms: Option<u64>,
    restarts: u32,
    room: Option<ShortOfRoom>,
) -> Revival {
    if let Custodian::NotOurs(why) = custodian {
        return Revival::NotOurs(why);
    }
    if let Custodian::Vm = custodian {
        // One decision for the Linux VM, shared with `vm::revive` (ADR-0026):
        // `revive_vms` is its switch and `revive_docker` has no say. Custody only
        // reaches here while the engine is in service (`engine_down` handles every
        // other state, `unmanaged` included, and never asks for a restart), so the
        // VM's state is `InService`.
        let linux = crate::vm::revive::LinuxRevivePolicy {
            enabled: policy.revive_vms,
            revive_after: Duration::from_millis(policy.revive_after_ms()),
            cooldown: Duration::from_secs(policy.revive_cooldown_secs),
            give_up_after: REVIVE_GIVE_UP_AFTER,
        };
        return match crate::vm::revive::linux_revival(
            crate::vm::proto::VmState::InService,
            &linux,
            Duration::from_millis(unresponsive_ms),
            since_restart_ms.map(Duration::from_millis),
            restarts,
        ) {
            crate::vm::revive::LinuxRevival::Wait => Revival::Wait,
            crate::vm::revive::LinuxRevival::GaveUp { restarts } => Revival::GaveUp { restarts },
            crate::vm::revive::LinuxRevival::Restart => match room {
                Some(short) => Revival::NoRoom(short),
                None => Revival::Restart,
            },
        };
    }
    if !policy.revive_docker {
        return Revival::Wait;
    }
    if restarts >= REVIVE_GIVE_UP_AFTER {
        return Revival::GaveUp { restarts };
    }
    if unresponsive_ms < policy.revive_after_ms() {
        return Revival::Wait;
    }
    if since_restart_ms.is_some_and(|ms| ms < policy.revive_cooldown_secs.saturating_mul(1000)) {
        return Revival::Wait;
    }
    match room {
        Some(short) => Revival::NoRoom(short),
        None => Revival::Restart,
    }
}

/// What faber has done about the Docker daemon, across reconcile cycles —
/// ADR-0007's custody, with ADR-0024 §2's rule for when a restart counts as
/// having worked.
///
/// Fed once per cycle with whether the limiter's probe says Docker is answering.
/// Pure bookkeeping, so the whole policy is testable without a Docker host;
/// [`crate::reconcile`] does the I/O around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerCustody {
    /// Who restarts Docker here, which decides whether a restart is ever faber's
    /// to make ([`revival`]). Fixed for the life of the process, like the layout.
    custodian: Custodian,
    /// When the daemon was first seen unreachable in an unbroken run of cycles.
    down_since_ms: Option<u64>,
    /// When it was first seen answering in an unbroken run of cycles.
    up_since_ms: Option<u64>,
    /// When faber last restarted it.
    last_restart_ms: Option<u64>,
    /// Restarts since the daemon last stayed answering for a whole cooldown.
    restarts: u32,
    /// The reason the Docker fault last gave in this outage, so it is stated
    /// again when that changes and not every cycle.
    stated: Option<FaultReason>,
    /// The engine's reason the fault last gave, while faber's Linux VM is not in
    /// service ([`DockerCustody::engine_down`]).
    engine_said: Option<String>,
}

/// Whether an engine outage's line is new: the first of the outage, or a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Said {
    pub opened: bool,
    pub restated: bool,
}

/// Custody that may restart its daemon, as a Linux LEGACY host's does: the
/// restart policy itself, whatever host the code runs on.
impl Default for DockerCustody {
    fn default() -> Self {
        Self::new(Custodian::Service)
    }
}

/// The reason a Docker fault gives, apart from how long the outage has lasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultReason {
    Down,
    NoRoom,
    GaveUp,
    NotOurs,
}

impl FaultReason {
    fn of(revival: Revival) -> Self {
        match revival {
            Revival::Restart | Revival::Wait => Self::Down,
            Revival::NoRoom(_) => Self::NoRoom,
            Revival::GaveUp { .. } => Self::GaveUp,
            Revival::NotOurs(_) => Self::NotOurs,
        }
    }
}

/// One cycle of an outage: how long, what to do about it, and whether the fault
/// has something new to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outage {
    pub unresponsive_ms: u64,
    pub revival: Revival,
    /// This is the outage's first cycle, whose line opens the fault.
    pub opened: bool,
    /// The fault's reason changed since it was last stated in this outage — a
    /// restart now waiting for room, room back, or faber giving up — so it must
    /// be stated again. Never set together with `opened`.
    pub restated: bool,
}

impl Outage {
    /// The Docker fault's line: how long the daemon has been unresponsive and,
    /// when faber is not simply waiting, why it is not restarting it.
    pub fn fault(&self, policy: &HostPolicy) -> String {
        let down = format!(
            "Docker daemon unresponsive for {}s",
            self.unresponsive_ms / 1000
        );
        match self.revival {
            Revival::Restart | Revival::Wait => {
                format!("{down} — admission refused until it answers")
            }
            Revival::NoRoom(short) => format!(
                "{down} — not restarting it while the Docker volume has {} GiB free, below \
                 the {} GiB admission floor: a restart cannot make room (ADR-0024). \
                 Admission refused until it answers",
                fmt_gib(short.free_mib),
                short.floor_gib
            ),
            Revival::GaveUp { restarts } => format!(
                "{down} — faber restarted it {restarts} times without it staying up for {}s \
                 in between, and has stopped trying: this needs a human. Admission refused \
                 until it answers",
                policy.revive_cooldown_secs
            ),
            Revival::NotOurs(why) => {
                format!("{down} — {why}. Admission refused until it answers")
            }
        }
    }
}

impl DockerCustody {
    /// Custody for a daemon `custodian` restarts. Where that is
    /// [`Custodian::NotOurs`] it records outages and keeps admission shut, and
    /// never calls for a restart.
    pub fn new(custodian: Custodian) -> Self {
        Self {
            custodian,
            down_since_ms: None,
            up_since_ms: None,
            last_restart_ms: None,
            restarts: 0,
            stated: None,
            engine_said: None,
        }
    }

    /// A cycle in which Docker answered. `true` when that ends an outage, so the
    /// fault clears.
    ///
    /// Past restarts are forgiven only once the daemon has answered for a whole
    /// `revive_cooldown_secs` in a row (ADR-0024 §2), never on its first answer:
    /// a daemon that answers briefly and wedges again is the case the cooldown and
    /// the give-up count exist for, and resetting them on that answer is what let
    /// the incident host be restarted over and over with nothing to stop it.
    pub fn answered(&mut self, policy: &HostPolicy, now_ms: u64) -> bool {
        let ended = self.down_since_ms.take().is_some();
        self.stated = None;
        self.engine_said = None;
        let up_since = *self.up_since_ms.get_or_insert(now_ms);
        if now_ms.saturating_sub(up_since) >= policy.revive_cooldown_secs.saturating_mul(1000) {
            self.restarts = 0;
            self.last_restart_ms = None;
        }
        ended
    }

    /// A cycle in which faber's Linux VM is not in service (ADR-0026), so Docker
    /// is unreachable for a reason the engine driver owns and is already acting
    /// on. The outage stays open, so the fault clears when Docker answers, but its
    /// unresponsive clock starts over every such cycle: a restart is due only once
    /// the VM has been serving for `revive_after_secs` with Docker still not
    /// answering. Stopping a VM that is booting would only start the boot again.
    ///
    /// `line` is the fault's line; [`Said`] says whether it is new, so the caller
    /// states it when it changes and not every cycle.
    pub fn engine_down(&mut self, now_ms: u64, line: &str) -> Said {
        self.up_since_ms = None;
        let opened = self.down_since_ms.replace(now_ms).is_none();
        self.stated = None;
        let changed = self.engine_said.as_deref() != Some(line);
        let restated = changed && !opened;
        self.engine_said = Some(line.to_string());
        Said { opened, restated }
    }

    /// A cycle in which Docker did not answer. `room` is the evidence that the
    /// Docker volume is under the admission floor, if there is any.
    pub fn unanswered(
        &mut self,
        policy: &HostPolicy,
        now_ms: u64,
        room: Option<ShortOfRoom>,
    ) -> Outage {
        self.up_since_ms = None;
        self.engine_said = None;
        let since = *self.down_since_ms.get_or_insert(now_ms);
        let unresponsive_ms = now_ms.saturating_sub(since);
        let since_restart_ms = self.last_restart_ms.map(|t| now_ms.saturating_sub(t));
        let revival = revival(
            self.custodian,
            policy,
            unresponsive_ms,
            since_restart_ms,
            self.restarts,
            room,
        );
        let reason = FaultReason::of(revival);
        let said = self.stated.replace(reason);
        Outage {
            unresponsive_ms,
            revival,
            opened: said.is_none(),
            restated: said.is_some_and(|said| said != reason),
        }
    }

    /// Record the restart an [`Outage`] called for, and say which one it is.
    ///
    /// Counted before the restart is tried, and whatever its result: a restart
    /// after which Docker answered for a minute has not worked any better than one
    /// after which it never answered. Only a stable cooldown forgives it
    /// ([`DockerCustody::answered`]).
    pub fn restarting(&mut self, now_ms: u64) -> u32 {
        self.last_restart_ms = Some(now_ms);
        self.restarts += 1;
        self.restarts
    }
}

/// Restart a wedged Docker daemon and wait for it to answer.
///
/// *How* to restart Docker is genuinely platform-specific — faber's own Linux VM
/// to stop through the VM supervisor on a set-up macOS host (ADR-0026), a system
/// service to restart (best-effort, since a user-session faber often lacks the
/// privilege) on Linux — so that half lives in
/// [`crate::platform::host::revive_docker`]; see its module doc for the full
/// rationale. What's here is the portable half,
/// the same for every OS: confirm the daemon actually answers afterwards
/// rather than assuming a restart command that returned `Ok` means Docker is
/// back.
///
/// Returns `Ok(())` only when the daemon answers afterwards — a revival that
/// leaves Docker still dead is a failure, and the caller counts it as one. A
/// platform restart that itself fails (a Linux permission denial, chiefly)
/// propagates as the same kind of `Err`, via `?` — there is nothing further to
/// wait for if the OS never agreed to restart Docker in the first place.
pub async fn revive_docker() -> anyhow::Result<()> {
    warn!("host: Docker is wedged — restarting the daemon (ADR-0007)");

    crate::platform::host::revive_docker().await?;

    let deadline = tokio::time::Instant::now() + REVIVE_WAIT;
    while tokio::time::Instant::now() < deadline {
        if crate::docker::daemon_answers().await {
            info!("host: Docker answered again after a restart");
            return Ok(());
        }
        tokio::time::sleep(REVIVE_POLL).await;
    }
    anyhow::bail!("Docker did not answer within {REVIVE_WAIT:?} of being restarted")
}

/// Free space (MiB) on the volume backing `path`, or `None` when it can't be read.
pub async fn disk_free_mib(path: &str) -> Option<u64> {
    let out = run_bounded_output("df", &["-k", path], Duration::from_secs(10)).await?;
    parse_df_avail_mib(&out)
}

/// Parse `df -k`'s available column into MiB.
///
/// `df` output is a header line plus one row per filesystem; the available blocks
/// are the fourth field, in 1K blocks. A row can wrap when the device name is
/// long, so the *last* line with enough numeric fields is the one to read.
pub fn parse_df_avail_mib(text: &str) -> Option<u64> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // device, 1K-blocks, used, available, ...
            f.get(3).and_then(|a| a.parse::<u64>().ok())
        })
        .last()
        .map(|kb| kb / 1024)
}

/// The path whose free space decides whether Docker is about to be squeezed.
///
/// Which volume that actually is genuinely differs by OS — a VM disk image on
/// macOS, dockerd's `data-root` on Linux — so the decision is
/// [`crate::platform::host::docker_disk_path`]'s to make; see that module's doc
/// for the full rationale, including why a `$HOME` fallback is safe on macOS
/// and a correctness bug on Linux.
///
/// `None` when it can't be determined (Linux without a readable `docker info`,
/// chiefly). Callers must treat that exactly like an unreadable `df`: nothing
/// special to branch on, because [`below_disk_floor`] already reads a `None`
/// free-space figure as "not breached" rather than a guess — just skip calling
/// [`disk_free_mib`] and let that same `None` reach it.
pub async fn docker_data_path() -> Option<String> {
    crate::platform::host::docker_disk_path().await
}

/// How long the probe that reads the Linux VM's disk gets. It starts a container
/// from an image already there, so a hang past this is a daemon not answering.
const VM_DISK_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The latest good reading of the Linux VM's disk (#410), whoever took it. The
/// limiter's disk loop reads it every tick; the one-shot gates (the janitor, a
/// variant build, a first mirror clone) combine it with their own reading of
/// the host volume rather than starting a container each.
static LINUX_VM_DISK: std::sync::Mutex<Option<DiskReading>> = std::sync::Mutex::new(None);

/// What reading the Linux VM's disk came to (#410).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmDisk {
    /// This host has no Linux VM: Docker's disk is the host volume itself.
    Absent,
    /// The probe failed, with why. Treated as no reading: nothing acts on it.
    Unreadable(String),
    /// A reading.
    Read(DiskReading),
}

/// Read free space on the Linux VM's disk, where Docker's data root is (#410),
/// and keep it for [`latest_linux_vm_disk`]. [`VmDisk::Absent`] where this host
/// has no Linux VM.
///
/// Read over the VM's Docker socket, by a throwaway container from
/// `probe_image` (see [`crate::docker::probe_vm_disk_free_mib`]). Its root is an
/// overlay whose writable layer lives under Docker's data root, and an overlay
/// reports the free space of that layer's filesystem, so `df /` inside it is the
/// VM disk Docker fills, with no mount of the host's paths and no privilege.
pub async fn read_linux_vm_disk(probe_image: &str) -> VmDisk {
    if !crate::platform::host::has_linux_vm() {
        return VmDisk::Absent;
    }
    match crate::docker::probe_vm_disk_free_mib(probe_image, VM_DISK_PROBE_TIMEOUT).await {
        Ok(free_mib) => {
            let reading = DiskReading {
                free_mib,
                at_ms: crate::audit::now_ms(),
                volume: DiskVolume::LinuxVm,
            };
            *LINUX_VM_DISK.lock().unwrap() = Some(reading);
            VmDisk::Read(reading)
        }
        Err(e) => VmDisk::Unreadable(e.to_string()),
    }
}

/// The latest good reading of the Linux VM's disk, fresh or not; `None` before
/// the first, and always where there is no Linux VM.
pub fn latest_linux_vm_disk() -> Option<DiskReading> {
    *LINUX_VM_DISK.lock().unwrap()
}

/// Free space on the volume a Docker job fills, for a gate that measures once
/// rather than every tick: the host volume measured now, against the latest
/// reading of the Linux VM's disk where there is one, and the tighter of the two
/// while both are evidence ([`tighter`], #410).
///
/// `None` when neither can be read. A caller treats that as it always treated an
/// unreadable `df`: not evidence of pressure.
pub async fn docker_disk_free() -> Option<DiskReading> {
    let now = crate::audit::now_ms();
    let host = match docker_data_path().await {
        Some(path) => disk_free_mib(&path).await.map(|free_mib| DiskReading {
            free_mib,
            at_ms: now,
            volume: crate::platform::host::docker_disk_volume(),
        }),
        None => None,
    };
    tighter(host, latest_linux_vm_disk(), now).filter(|r| r.fresh_free_mib(now).is_some())
}

/// Whether free space has fallen below the floor that marks this host as under
/// disk pressure — shared by the janitor's reclaim pass
/// ([`crate::sweep::relieve_disk_pressure`]) and the pre-build gate on a
/// runner-image variant build ([`crate::image::VariantCache::ensure`],
/// ADR-0008), since both are asking the same question: is it safe to let
/// something put more on this disk right now.
///
/// `0` disables the floor entirely. Split from the I/O so the threshold is
/// testable, and expressed as "free is below the floor" rather than a percentage
/// because what wedges a daemon is absolute headroom, not a ratio — the incident
/// host was wedged with, say, 8 GiB free on a 1 TiB volume.
///
/// Takes the floor as a bare `u64` rather than `&HostPolicy`, so the variant
/// cache — which holds no other part of this host's policy — isn't forced to
/// carry a whole `HostPolicy` merely to reach the one field it needs.
pub fn below_disk_floor(disk_floor_gib: u64, free_mib: Option<u64>) -> bool {
    if disk_floor_gib == 0 {
        return false;
    }
    match free_mib {
        // Unreadable free space is not evidence of pressure; don't act on a guess.
        None => false,
        Some(free) => free < disk_floor_gib.saturating_mul(1024),
    }
}

/// What a reclamation pass freed, for the log line and the audit trail.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reclaimed {
    /// Human-readable totals, one per prune that reported anything.
    pub freed: Vec<String>,
}

impl Reclaimed {
    pub fn is_empty(&self) -> bool {
        self.freed.is_empty()
    }
}

/// Reclaim Docker space that **CI itself created**, and nothing else.
///
/// The two prunes here are the safe ones, and their narrowness is the point.
/// `image prune` without `--all` removes only *dangling* images — layers no tag
/// points at any more — so the runner image and every stack image a job or an
/// unrelated project still references survive. `builder prune` drops build cache,
/// which is regenerable by definition.
///
/// What is deliberately **not** here: `system prune -a --volumes`, `image prune
/// -a`, and any volume reap. This host runs unrelated stacks with real data
/// volumes beside CI, and `image prune -a` would evict the runner image itself —
/// correct by the letter of "unused", disastrous in effect, since the next job
/// re-pulls gigabytes. faber's own leaked containers, networks and volumes are
/// already reaped by [`crate::sweep`], which knows which ones are its.
pub async fn reclaim_ci_disk() -> Reclaimed {
    let mut out = Reclaimed::default();
    let gate = crate::setup::docker_cli::plugin_gate();
    if let Some(why) = &gate {
        warn!("host: build cache not pruned: {why}");
    }
    for (what, args) in reclaim_prunes(gate.is_some()) {
        match crate::docker::prune(&args).await {
            Ok(text) => {
                if let Some(space) = parse_reclaimed(&text) {
                    info!("host: pruned {what}, reclaimed {space}");
                    out.freed.push(format!("{what}: {space}"));
                }
            }
            Err(e) => warn!("host: prune {what}: {e}"),
        }
    }
    out
}

/// The prunes [`reclaim_ci_disk`] runs. `builder prune` loads the buildx
/// plugin (docker/cli's `processBuilder`), so it is left out while
/// `plugins_unsafe` ([`crate::setup::docker_cli::plugin_gate`]).
fn reclaim_prunes(plugins_unsafe: bool) -> Vec<(&'static str, Vec<&'static str>)> {
    let mut v = vec![("dangling images", vec!["image", "prune", "-f"])];
    if !plugins_unsafe {
        v.push(("build cache", vec!["builder", "prune", "-f"]));
    }
    v
}

/// Pull the `Total reclaimed space: N` figure out of a prune's output.
///
/// `None` when the prune reported nothing to free — docker still prints the line
/// with a zero, which is not worth a log line or an audit entry.
pub fn parse_reclaimed(text: &str) -> Option<String> {
    let raw = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Total reclaimed space:"))?
        .trim()
        .to_string();
    // The daemon's text, bound for a log line and an audit record: only a figure
    // in the form Docker prints a size in is taken as one (#105).
    if !size_figure(&raw) {
        return None;
    }
    // "0B" / "0" is docker reporting it freed nothing; "0.5MB" is a real figure.
    let zero = raw.starts_with('0') && !raw.starts_with("0.");
    (!zero).then_some(raw)
}

/// Whether `s` reads as a size the way Docker prints one (`0B`, `1.234GB`,
/// `512kB`): a decimal number and a unit, short, and nothing else.
pub fn size_figure(s: &str) -> bool {
    let unit_at = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(unit_at);
    !number.is_empty()
        && number.parse::<f64>().is_ok()
        && unit.len() <= 3
        && unit.chars().all(|c| c.is_ascii_alphabetic())
}

// --- bounded process helpers ------------------------------------------------

/// Only Linux's `systemctl restart docker` runs a bare command now: macOS revival
/// asks the VM supervisor instead (ADR-0026).
#[cfg(target_os = "linux")]
pub(crate) async fn run_bounded(
    program: &str,
    args: &[&str],
    limit: Duration,
) -> anyhow::Result<()> {
    let fut = tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => anyhow::bail!("{program} exited {status}"),
        Ok(Err(e)) => anyhow::bail!("{program}: {e}"),
        Err(_) => anyhow::bail!("{program} exceeded {limit:?}"),
    }
}

/// Run `program` and return its stdout on success, `None` on any failure: a
/// non-zero exit, a timeout, or output past [`crate::untrusted::OUTPUT_CAP`]. On
/// Linux this is how `docker info` is read, so it is bounded in size like every
/// other read of the daemon (#105), and its process is killed when it overruns.
pub(crate) async fn run_bounded_output(
    program: &str,
    args: &[&str],
    limit: Duration,
) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    run_bounded_output_cmd(cmd, limit, program).await
}

/// [`run_bounded_output`], for a caller that must build its own [`Command`] —
/// today only [`crate::platform::host::docker_disk_path`] on Linux, whose
/// `docker info` call has to go through [`crate::docker::docker_command`]'s
/// environment allowlist (AGENTS.md/ADR-0025 §8) like every other `docker`
/// spawn, rather than inherit this process's environment wholesale the way a
/// bare `Command::new(program)` here does. `what` names the call in a timeout
/// or overrun log.
///
/// [`Command`]: tokio::process::Command
pub(crate) async fn run_bounded_output_cmd(
    cmd: tokio::process::Command,
    limit: Duration,
    what: &str,
) -> Option<String> {
    match crate::untrusted::output_within(cmd, limit, crate::untrusted::OUTPUT_CAP, what).await {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HostPolicy {
        HostPolicy {
            revive_docker: true,
            revive_vms: true,
            revive_after_secs: 600,
            revive_unable_to_start_secs: None,
            revive_cooldown_secs: 1800,
            disk_floor_gib: 25,
            disk_admission_floor_gib: None,
            keep_memory_gib: None,
            keep_cpus: None,
            recreatable_projects: Vec::new(),
            recreatable_volumes: Vec::new(),
            network_pool: String::new(),
            network_subnet_size: 24,
            cache_max_age_days: None,
            cache_max_gib: None,
            instance: String::new(),
            pull_unqualified_images: None,
            prevent_sleep: None,
            prevent_sleep_while_busy: None,
            dind_requires_label: None,
        }
    }

    #[test]
    fn a_hiccup_is_not_a_wedge() {
        // The whole reason for a duration bound: a busy daemon misses probes, and
        // restarting Docker for that would be far worse than waiting it out.
        assert_eq!(
            revival(Custodian::Service, &policy(), 60_000, None, 0, None),
            Revival::Wait
        );
        assert_eq!(
            revival(Custodian::Service, &policy(), 600_000, None, 0, None),
            Revival::Restart
        );
    }

    #[test]
    fn the_build_cache_is_not_pruned_while_plugin_dirs_are_unsafe() {
        fn names(v: Vec<(&'static str, Vec<&'static str>)>) -> Vec<Vec<&'static str>> {
            v.into_iter().map(|(_, a)| a).collect()
        }
        assert_eq!(
            names(reclaim_prunes(false)),
            [vec!["image", "prune", "-f"], vec!["builder", "prune", "-f"]]
        );
        assert_eq!(names(reclaim_prunes(true)), [vec!["image", "prune", "-f"]]);
        // Unit tests run LEGACY, where nothing is gated.
        assert_eq!(crate::setup::docker_cli::plugin_gate(), None);
    }

    #[test]
    fn the_cooldown_stops_a_restart_loop() {
        let p = policy();
        // Wedged long enough, but we just tried.
        assert_eq!(
            revival(Custodian::Service, &p, 3_600_000, Some(60_000), 1, None),
            Revival::Wait
        );
        // Wedged long enough and the cooldown has passed.
        assert_eq!(
            revival(Custodian::Service, &p, 3_600_000, Some(1_800_000), 1, None),
            Revival::Restart
        );
    }

    #[test]
    fn faber_gives_up_rather_than_restarting_docker_forever() {
        let p = policy();
        let last = REVIVE_GIVE_UP_AFTER - 1;
        assert_eq!(
            revival(
                Custodian::Service,
                &p,
                3_600_000,
                Some(9_999_999),
                last,
                None
            ),
            Revival::Restart
        );
        assert_eq!(
            revival(
                Custodian::Service,
                &p,
                3_600_000,
                Some(9_999_999),
                REVIVE_GIVE_UP_AFTER,
                None
            ),
            Revival::GaveUp {
                restarts: REVIVE_GIVE_UP_AFTER
            }
        );
    }

    #[test]
    fn the_operator_can_turn_it_off() {
        let mut p = policy();
        p.revive_docker = false;
        assert_eq!(
            revival(Custodian::Service, &p, u64::MAX, None, 0, None),
            Revival::Wait
        );
    }

    // --- ADR-0025/0026: who restarts Docker, by OS and layout ---------------

    #[test]
    fn the_custodian_follows_the_os_and_the_layout() {
        assert_eq!(
            Custodian::for_host(Os::MacOs, Layout::System),
            Custodian::Vm
        );
        assert_eq!(
            Custodian::for_host(Os::MacOs, Layout::Legacy),
            Custodian::NotOurs(DESKTOP_NOT_OURS)
        );
        assert_eq!(
            Custodian::for_host(Os::Linux, Layout::Legacy),
            Custodian::Service
        );
        assert_eq!(
            Custodian::for_host(Os::Linux, Layout::System),
            Custodian::NotOurs(NOT_OURS)
        );
        // Unit tests run LEGACY: a Mac never restarts Docker Desktop, a Linux
        // host restarts its service.
        #[cfg(target_os = "macos")]
        assert_eq!(
            Custodian::for_this_host(),
            Custodian::NotOurs(DESKTOP_NOT_OURS)
        );
        #[cfg(target_os = "linux")]
        assert_eq!(Custodian::for_this_host(), Custodian::Service);
    }

    /// Every combination that restarts Docker where faber may, and every one that
    /// doesn't, is `NotOurs` where it may not: no policy, clock, count or disk
    /// reading can make faber quit or relaunch the operator's Docker Desktop, or
    /// the machine's engine from `_faber`. faber's own VM restarts exactly where
    /// the service would.
    #[test]
    fn revival_never_restarts_a_docker_that_is_not_faber_s() {
        let short = short_of_room(DiskAdmission::Floor(10), read(2, 0), 0, NOW);
        let mut restarts = 0;
        for revive_docker in [true, false] {
            // The VM's switch is `revive_vms` (ADR-0026), not `revive_docker`:
            // set both so "exactly where the service would" is comparable.
            let p = HostPolicy {
                revive_docker,
                revive_vms: revive_docker,
                ..policy()
            };
            for unresponsive_ms in [0, 60_000, 600_000, u64::MAX] {
                for since_restart_ms in [None, Some(60_000), Some(9_999_999)] {
                    for count in [0, 1, REVIVE_GIVE_UP_AFTER] {
                        for room in [None, short] {
                            let args = (unresponsive_ms, since_restart_ms, count, room);
                            let service =
                                revival(Custodian::Service, &p, args.0, args.1, args.2, args.3);
                            if service == Revival::Restart {
                                restarts += 1;
                            }
                            assert_eq!(
                                revival(Custodian::Vm, &p, args.0, args.1, args.2, args.3),
                                service,
                                "{args:?}"
                            );
                            for why in [NOT_OURS, DESKTOP_NOT_OURS] {
                                assert_eq!(
                                    revival(
                                        Custodian::NotOurs(why),
                                        &p,
                                        args.0,
                                        args.1,
                                        args.2,
                                        args.3
                                    ),
                                    Revival::NotOurs(why),
                                    "{args:?} revive_docker={revive_docker}"
                                );
                            }
                        }
                    }
                }
            }
        }
        // The grid does reach the restart, so `NotOurs` is refusing something
        // real rather than a set of inputs that never restart anyway.
        assert!(restarts > 0);
    }

    /// Custody of a Docker that is not faber's keeps the outage on the record
    /// and admission shut, says why nothing is restarted, and never counts a
    /// restart.
    #[test]
    fn custody_of_a_docker_that_is_not_faber_s_reports_the_outage_and_restarts_nothing() {
        let p = policy();
        for why in [NOT_OURS, DESKTOP_NOT_OURS] {
            let mut c = DockerCustody::new(Custodian::NotOurs(why));
            let mut last = None;
            for at in (0..20_000).step_by(20) {
                let outage = cycle(&mut c, &p, at, false).unwrap();
                assert_eq!(outage.revival, Revival::NotOurs(why), "at {at}s");
                last = Some(outage);
            }
            assert_eq!((c.restarts, c.last_restart_ms), (0, None));
            let line = last.unwrap().fault(&p);
            assert!(line.contains(why), "{line}");
            assert!(
                line.ends_with("Admission refused until it answers"),
                "{line}"
            );
        }
        assert!(DESKTOP_NOT_OURS.contains("Docker Desktop is the operator's"));
        assert_eq!(
            DockerCustody::default(),
            DockerCustody::new(Custodian::Service)
        );
    }

    /// While faber's Linux VM is not in service the engine driver owns it: the
    /// outage stays open, but no restart comes due until the VM has served for a
    /// whole `revive_after_secs` with Docker still silent.
    #[test]
    fn a_vm_that_is_not_serving_is_left_to_the_engine_driver() {
        let p = policy();
        let mut c = DockerCustody::new(Custodian::Vm);
        let at = |s: u64| NOW + s * 1000;
        let first = c.engine_down(at(0), "booting");
        assert_eq!((first.opened, first.restated), (true, false));
        for s in (20..3_600).step_by(20) {
            let said = c.engine_down(at(s), "booting");
            assert_eq!((said.opened, said.restated), (false, false), "at {s}s");
        }
        let changed = c.engine_down(at(3_590), "not serving");
        assert_eq!((changed.opened, changed.restated), (false, true));
        // In service from 3 600 s, Docker still silent: the clock counts from the
        // VM's last cycle out of service, not from the outage's start.
        let serving = c.unanswered(&p, at(3_600), None);
        assert_eq!(serving.revival, Revival::Wait);
        assert_eq!(serving.unresponsive_ms, 10_000);
        assert_eq!(c.unanswered(&p, at(4_100), None).revival, Revival::Wait);
        assert_eq!(c.unanswered(&p, at(4_200), None).revival, Revival::Restart);
        // And an answer still ends the outage the engine opened.
        let mut c = DockerCustody::new(Custodian::Vm);
        c.engine_down(at(0), "booting");
        assert!(c.answered(&p, at(20)));
    }

    // --- ADR-0024: room on the Docker volume --------------------------------

    const NOW: u64 = 1_790_000_000_000;
    const GIB: u64 = 1024;

    fn read(free_gib_tenths: u64, age_ms: u64) -> Option<DiskReading> {
        Some(DiskReading {
            free_mib: free_gib_tenths * GIB / 10,
            volume: DiskVolume::Docker,
            at_ms: NOW - age_ms,
        })
    }

    fn on(volume: DiskVolume, free_mib: u64, age_ms: u64) -> Option<DiskReading> {
        Some(DiskReading {
            free_mib,
            at_ms: NOW - age_ms,
            volume,
        })
    }

    #[test]
    fn the_tighter_fresh_reading_is_the_one_a_docker_gate_acts_on() {
        // #410: 200 GiB free on the Mac, 1 GiB inside the Linux VM. The VM binds.
        let mac = on(DiskVolume::MacVms, 200 * GIB, 1_000);
        let vm = on(DiskVolume::LinuxVm, GIB, 5_000);
        assert_eq!(tighter(mac, vm, NOW), vm);
        assert_eq!(tighter(vm, mac, NOW), vm, "whichever side it is on");
        // And the other way round: a Mac volume running out binds over a roomy VM.
        let mac_low = on(DiskVolume::MacVms, 2 * GIB, 1_000);
        let vm_roomy = on(DiskVolume::LinuxVm, 40 * GIB, 1_000);
        assert_eq!(tighter(mac_low, vm_roomy, NOW), mac_low);
        // A tie keeps the first.
        let vm_tie = on(DiskVolume::LinuxVm, 2 * GIB, 1_000);
        assert_eq!(tighter(mac_low, vm_tie, NOW), mac_low);
    }

    #[test]
    fn an_unread_or_stale_vm_reading_leaves_the_host_volume_alone_in_charge() {
        // An unreadable VM is today's unknown: no action on a guess, so the gate
        // is exactly where the host volume alone puts it.
        let mac = on(DiskVolume::MacVms, 200 * GIB, 1_000);
        assert_eq!(tighter(mac, None, NOW), mac);
        assert_eq!(tighter(None, mac, NOW), mac);
        let stale_vm = on(DiskVolume::LinuxVm, 0, DISK_READING_MAX_AGE_MS + 1);
        assert_eq!(tighter(mac, stale_vm, NOW), mac);
        assert_eq!(tighter(stale_vm, mac, NOW), mac);
        // Neither fresh: the newer is kept, so the caller still sees it is stale
        // and refuses to act on it.
        let stale_mac = on(DiskVolume::MacVms, 0, DISK_READING_MAX_AGE_MS + 10);
        assert_eq!(tighter(stale_mac, stale_vm, NOW), stale_vm);
        assert_eq!(
            short_of_room(
                DiskAdmission::Floor(10),
                tighter(stale_mac, stale_vm, NOW),
                0,
                NOW
            ),
            None
        );
        assert_eq!(tighter(None, None, NOW), None);
    }

    #[test]
    fn a_short_volume_is_named_by_what_it_is() {
        let vm = on(DiskVolume::LinuxVm, GIB, 0);
        let short = short_of_room(DiskAdmission::Floor(10), vm, 0, NOW).unwrap();
        assert_eq!(short.volume, DiskVolume::LinuxVm);
        assert_eq!(DiskVolume::LinuxVm.describe(), "the Linux VM's disk");
        assert_eq!(
            DiskVolume::MacVms.describe(),
            "the Mac volume holding the VM disks"
        );
        // The wording every Linux host has always shown is unchanged.
        assert_eq!(DiskVolume::Docker.describe(), "the Docker volume");
        // `/api/state` spells it in snake case.
        assert_eq!(
            serde_json::to_value(DiskVolume::LinuxVm).unwrap(),
            serde_json::json!("linux_vm")
        );
        assert_eq!(
            serde_json::to_value(DiskVolume::MacVms).unwrap(),
            serde_json::json!("mac_vms")
        );
    }

    #[test]
    fn only_a_stated_floor_and_a_fresh_reading_find_the_disk_short() {
        let floor = DiskAdmission::Floor(10);
        let below = read(82, 5_000); // 8.2 GiB, five seconds old
        assert_eq!(
            short_of_room(floor, below, 0, NOW),
            Some(ShortOfRoom {
                free_mib: 82 * GIB / 10,
                volume: DiskVolume::Docker,
                held_mib: 0,
                floor_gib: 10,
            })
        );
        // No floor, stated or not: no gate, however full the disk.
        assert_eq!(short_of_room(DiskAdmission::Off, below, 0, NOW), None);
        assert_eq!(short_of_room(DiskAdmission::Unstated, below, 0, NOW), None);
        // No reading, or one too old to be evidence: not a reason to stop anything.
        assert_eq!(short_of_room(floor, None, 0, NOW), None);
        assert!(short_of_room(floor, read(82, DISK_READING_MAX_AGE_MS), 0, NOW).is_some());
        assert_eq!(
            short_of_room(floor, read(82, DISK_READING_MAX_AGE_MS + 1), 0, NOW),
            None
        );
        // A reading from "the future" (a clock step backwards) is not stale.
        assert!(short_of_room(
            floor,
            Some(DiskReading {
                free_mib: GIB,
                volume: DiskVolume::Docker,
                at_ms: NOW + 60_000
            }),
            0,
            NOW
        )
        .is_some());
    }

    #[test]
    fn the_floor_is_the_least_free_space_admission_may_leave() {
        let floor = DiskAdmission::Floor(10);
        assert_eq!(short_of_room(floor, read(100, 0), 0, NOW), None, "at it");
        assert!(short_of_room(floor, read(99, 0), 0, NOW).is_some());
        // What is held for ramping jobs counts against the reading: 12 GiB free
        // less 6 GiB still to be written is 6, under the floor.
        let held = short_of_room(floor, read(120, 0), 6 * GIB, NOW).unwrap();
        assert_eq!((held.free_mib, held.held_mib), (12 * GIB, 6 * GIB));
        assert_eq!(short_of_room(floor, read(160, 0), 6 * GIB, NOW), None);
        // Holding more than is free is simply short, not an underflow.
        assert!(short_of_room(floor, read(20, 0), 6 * GIB, NOW).is_some());
    }

    fn vm_revival(p: &HostPolicy, unresponsive_ms: u64, since: Option<u64>, n: u32) -> Revival {
        revival(Custodian::Vm, p, unresponsive_ms, since, n, None)
    }

    #[test]
    fn a_vm_custodian_follows_revive_vms_and_not_revive_docker() {
        let mut p = policy();
        p.revive_vms = false;
        assert_eq!(vm_revival(&p, u64::MAX, None, 0), Revival::Wait);
        p.revive_vms = true;
        assert_eq!(vm_revival(&p, 60_000, None, 0), Revival::Wait, "a hiccup");
        assert_eq!(vm_revival(&p, 600_000, None, 0), Revival::Restart);
        p.revive_docker = false;
        assert_eq!(
            vm_revival(&p, 600_000, None, 0),
            Revival::Restart,
            "revive_docker governs the Service custodian only"
        );
    }

    #[test]
    fn a_vm_custodian_honours_cooldown_give_up_and_room() {
        let p = policy();
        assert_eq!(vm_revival(&p, 3_600_000, Some(60_000), 1), Revival::Wait);
        assert_eq!(
            vm_revival(&p, 3_600_000, Some(1_800_000), 1),
            Revival::Restart
        );
        assert_eq!(
            vm_revival(&p, 3_600_000, None, REVIVE_GIVE_UP_AFTER),
            Revival::GaveUp {
                restarts: REVIVE_GIVE_UP_AFTER
            }
        );
        let short = short_of_room(DiskAdmission::Floor(10), read(2, 0), 0, NOW);
        assert_eq!(
            revival(Custodian::Vm, &p, 3_600_000, None, 0, short),
            Revival::NoRoom(short.unwrap())
        );
    }

    #[test]
    fn an_unmanaged_engine_is_never_restarted() {
        // An unmanaged engine reports `Down`, so custody runs `engine_down`, which
        // never asks for a restart; the shared policy agrees for that state.
        use crate::vm::{proto::VmState, revive::*};
        let linux = LinuxRevivePolicy {
            enabled: true,
            revive_after: Duration::from_secs(600),
            cooldown: Duration::from_secs(1800),
            give_up_after: REVIVE_GIVE_UP_AFTER,
        };
        assert_eq!(
            linux_revival(VmState::Unmanaged, &linux, Duration::MAX, None, 0),
            LinuxRevival::Wait
        );
        let mut c = DockerCustody::new(Custodian::Vm);
        c.engine_down(NOW, "faber's Linux VM is not serving: unmanaged");
        c.engine_down(
            NOW + 10_000_000,
            "faber's Linux VM is not serving: unmanaged",
        );
        let p = policy();
        assert_eq!(
            c.unanswered(&p, NOW + 10_000_001, None).revival,
            Revival::Wait
        );
    }

    #[test]
    fn revival_waits_for_room_and_takes_it_the_first_cycle_it_returns() {
        let p = policy();
        let short = short_of_room(DiskAdmission::Floor(10), read(2, 0), 0, NOW);
        assert_eq!(
            revival(Custodian::Service, &p, 3_600_000, None, 0, short),
            Revival::NoRoom(short.unwrap())
        );
        // Room back: the unresponsive clock kept running, so it fires at once.
        assert_eq!(
            revival(Custodian::Service, &p, 3_600_000, None, 0, None),
            Revival::Restart
        );
        // The disk is the reason only when a restart is otherwise due: a hiccup is
        // still a hiccup, and the cooldown still spaces restarts.
        assert_eq!(
            revival(Custodian::Service, &p, 60_000, None, 0, short),
            Revival::Wait
        );
        assert_eq!(
            revival(Custodian::Service, &p, 3_600_000, Some(60_000), 1, short),
            Revival::Wait
        );
        // A host that gave up says so, not that it is waiting for room.
        assert_eq!(
            revival(
                Custodian::Service,
                &p,
                3_600_000,
                None,
                REVIVE_GIVE_UP_AFTER,
                short
            ),
            Revival::GaveUp {
                restarts: REVIVE_GIVE_UP_AFTER
            }
        );
    }

    /// Drive custody through one cycle at `at` seconds, as `reconcile_host` does:
    /// the verdict, and the restart it calls for.
    fn cycle(c: &mut DockerCustody, p: &HostPolicy, at: u64, up: bool) -> Option<Outage> {
        let now = NOW + at * 1000;
        if up {
            c.answered(p, now);
            return None;
        }
        let outage = c.unanswered(p, now, None);
        if outage.revival == Revival::Restart {
            c.restarting(now);
        }
        Some(outage)
    }

    #[test]
    fn a_first_answer_after_a_restart_forgives_nothing() {
        // The incident: restarted, answered briefly, wedged again. The restart
        // must still count, and the cooldown still run from it.
        let p = policy();
        let mut c = DockerCustody::default();
        cycle(&mut c, &p, 0, false);
        let restart = cycle(&mut c, &p, 600, false).unwrap();
        assert_eq!(restart.revival, Revival::Restart);
        assert_eq!(c.restarts, 1);
        cycle(&mut c, &p, 630, true);
        cycle(&mut c, &p, 690, true);
        assert_eq!((c.restarts, c.last_restart_ms), (1, Some(NOW + 600_000)));
        // Down again, and wedged again — but still inside the cooldown that runs
        // from the last restart, so it waits it out.
        cycle(&mut c, &p, 700, false);
        assert_eq!(
            cycle(&mut c, &p, 1_300, false).unwrap().revival,
            Revival::Wait
        );
        assert_eq!(
            cycle(&mut c, &p, 2_400, false).unwrap().revival,
            Revival::Restart
        );
        assert_eq!(c.restarts, 2);
    }

    #[test]
    fn a_daemon_that_keeps_wedging_is_restarted_once_per_cooldown_then_left_to_a_human() {
        let p = policy();
        let mut c = DockerCustody::default();
        let mut restarts_at = Vec::new();
        let mut last = None;
        // Down for a long time with a minute of life after every restart.
        for at in (0..20_000).step_by(20) {
            let revived_recently = restarts_at.last().is_some_and(|r| at > *r && at <= r + 60);
            let outage = cycle(&mut c, &p, at, revived_recently);
            if let Some(o) = outage {
                if o.revival == Revival::Restart {
                    restarts_at.push(at);
                }
                last = Some(o);
            }
        }
        // At most once per cooldown, and never more than the give-up count.
        assert_eq!(restarts_at.len(), REVIVE_GIVE_UP_AFTER as usize);
        assert!(restarts_at.windows(2).all(|w| w[1] - w[0] >= 1_800));
        let last = last.unwrap();
        assert_eq!(
            last.revival,
            Revival::GaveUp {
                restarts: REVIVE_GIVE_UP_AFTER
            }
        );
        let line = last.fault(&p);
        assert!(
            line.contains("faber restarted it 3 times without it staying up for 1800s in between"),
            "{line}"
        );
        assert!(line.contains("this needs a human"), "{line}");
    }

    #[test]
    fn a_stable_cooldown_forgives_every_restart() {
        let p = policy();
        let mut c = DockerCustody::default();
        for (at, up) in [(0, false), (600, false), (2_400, false), (4_200, false)] {
            cycle(&mut c, &p, at, up);
        }
        assert_eq!(c.restarts, 3);
        // Up for a whole cooldown after the last restart, then some.
        cycle(&mut c, &p, 4_230, true);
        cycle(&mut c, &p, 6_000, true);
        assert_eq!(c.restarts, 3, "1770 s is not yet the whole cooldown");
        cycle(&mut c, &p, 6_030, true);
        assert_eq!((c.restarts, c.last_restart_ms), (0, None));
        // So the next wedge gets a full set of restarts again.
        cycle(&mut c, &p, 7_000, false);
        let next = cycle(&mut c, &p, 7_600, false).unwrap();
        assert_eq!(next.revival, Revival::Restart);
    }

    #[test]
    fn an_outage_states_its_reason_again_only_when_the_reason_changes() {
        let p = policy();
        let mut c = DockerCustody::default();
        let short = short_of_room(DiskAdmission::Floor(10), read(2, 0), 0, NOW);
        let at = |s: u64| NOW + s * 1000;
        // The first cycle opens the fault; waiting says nothing new.
        let open = c.unanswered(&p, at(0), short);
        assert_eq!(
            (open.revival, open.opened, open.restated),
            (Revival::Wait, true, false)
        );
        assert_eq!(
            open.fault(&p),
            "Docker daemon unresponsive for 0s — admission refused until it answers"
        );
        let waiting = c.unanswered(&p, at(300), short);
        assert!(!waiting.opened && !waiting.restated);
        // Due, and the disk is why it is not happening: said once.
        let no_room = c.unanswered(&p, at(600), short);
        assert!(no_room.restated && !no_room.opened);
        assert_eq!(
            no_room.fault(&p),
            "Docker daemon unresponsive for 600s — not restarting it while the Docker \
             volume has 0.2 GiB free, below the 10 GiB admission floor: a restart cannot \
             make room (ADR-0024). Admission refused until it answers"
        );
        assert!(!c.unanswered(&p, at(620), short).restated);
        // Room back: restarting, which is worth saying since the last line said not.
        let back = c.unanswered(&p, at(640), None);
        assert_eq!((back.revival, back.restated), (Revival::Restart, true));
        // Answering ends the outage; the next one opens its own fault.
        assert!(c.answered(&p, at(700)));
        assert!(!c.answered(&p, at(720)), "an outage ends once");
        let next = c.unanswered(&p, at(800), None);
        assert!(next.opened && !next.restated);
    }

    #[test]
    fn gib_figures_read_short() {
        assert_eq!(fmt_gib(8397), "8.2");
        assert_eq!(fmt_gib(6 * GIB), "6");
        assert_eq!(fmt_gib(170), "0.2");
        assert_eq!(fmt_gib(0), "0");
    }

    #[test]
    fn the_disk_floor_is_absolute_not_a_ratio() {
        assert!(below_disk_floor(25, Some(14 * 1024)));
        assert!(!below_disk_floor(25, Some(40 * 1024)));
        // Unreadable free space must not trigger action on a guess.
        assert!(!below_disk_floor(25, None));
    }

    #[test]
    fn a_zero_floor_disables_reclamation() {
        assert!(!below_disk_floor(0, Some(0)));
    }

    #[test]
    fn an_unresolvable_disk_path_is_not_evidence_of_pressure() {
        // `docker_data_path` returns `None` when the platform layer can't tell
        // which volume Docker actually writes to (Linux, `docker info`
        // unreadable, chiefly). The only sound reaction to not knowing is the
        // same one an unreadable `df` already gets: don't guess, don't act.
        // Guessing `$HOME` here — the bug this whole seam exists to avoid, see
        // `crate::platform::host`'s module doc — would be worse than a false
        // negative, since it can also fire on a volume that was never full.
        let unresolved_free: Option<u64> = None;
        assert!(!below_disk_floor(25, unresolved_free));
    }

    #[test]
    fn df_available_is_read_from_the_last_row() {
        let out = "Filesystem 1024-blocks      Used Available Capacity  Mounted on\n\
                   /dev/disk1s1  1024000000 921600000 102400000    90%    /\n";
        assert_eq!(parse_df_avail_mib(out), Some(100_000));
        // A wrapped row (long device name) still resolves.
        assert_eq!(
            parse_df_avail_mib("h1 h2 h3 h4\nx 100 50 2048 1% /\n"),
            Some(2)
        );
        assert_eq!(parse_df_avail_mib(""), None);
        assert_eq!(parse_df_avail_mib("Filesystem blocks\n"), None);
    }

    #[test]
    fn a_prune_that_freed_nothing_is_not_reported() {
        assert_eq!(
            parse_reclaimed("Deleted Images:\nsha256:abc\n\nTotal reclaimed space: 1.459GB\n"),
            Some("1.459GB".to_string())
        );
        assert_eq!(parse_reclaimed("Total reclaimed space: 0B\n"), None);
        assert_eq!(parse_reclaimed("Total reclaimed space: 0\n"), None);
        // A sub-1 figure is real and must survive the zero check.
        assert_eq!(
            parse_reclaimed("Total reclaimed space: 0.5MB\n"),
            Some("0.5MB".to_string())
        );
        assert_eq!(parse_reclaimed("nothing here\n"), None);
    }

    #[test]
    fn a_reclaimed_figure_that_is_not_a_size_is_not_reported() {
        // #105: the figure is the daemon's text, bound for the log and the audit
        // trail, so only one in the form Docker prints a size in is taken.
        for bad in [
            "Total reclaimed space: 1GB; rm -rf /\n",
            "Total reclaimed space: lots\n",
            "Total reclaimed space: 1.2GB\u{1b}[2J\n",
        ] {
            assert_eq!(parse_reclaimed(bad), None, "{bad:?}");
        }
        assert!(size_figure("512kB"));
        assert!(!size_figure(""));
        assert!(!size_figure("1.2.3GB"));
    }
}
