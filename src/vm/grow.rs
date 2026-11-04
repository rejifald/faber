//! The Linux VM's disk grows within a ceiling (ADR-0061).
//!
//! The engine's disk is a sparse raw file, built at [`BUILT_GIB`]. When cleaning
//! cannot keep room for Docker jobs, faber grows it, [`STEP_GIB`] at a time, up
//! to `[linux_vm] disk_max_gib`. Virtualization.framework reads a disk's size
//! only at boot, so a grow is applied at a VM start: the next one that happens
//! anyway, else an idle moment, else a drain after [`DRAIN_AFTER`]. No restart
//! for growth ever kills a job.
//!
//! The pieces, each testable on its own:
//!
//! - [`Clocks`]: the two triggers (§2), fed by the grow loop's reading of the
//!   VM's free space, the janitor's verdicts and the disk gate's refusals. In
//!   memory only: a daemon restart starts them again, which errs towards waiting.
//! - [`pending`]: whether a grow is pending and to what size, recomputed from
//!   the readings on every tick, never stored.
//! - [`Grow`]: the state the policy shares with the engine driver (the size the
//!   next `start` asks for), the limiter (the grow hold, the gate clock), the
//!   janitor, reconcile (revival held off and the Docker fault muted during a
//!   grow restart) and the dashboard.
//! - [`Applier::tick`]: one pass of the loop over a [`GrowHost`], the seam the
//!   supervisor, Docker and the orchestrator sit behind.
//!
//! faber never deletes or recreates the engine's disk (AGENTS.md); it only asks
//! the supervisor to extend it in place (protocol v4, §4), and the guest to grow
//! root into it (§5).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use tracing::{info, warn};

use super::engine::Left;

/// One GiB.
pub const GIB: u64 = 1 << 30;
/// The size setup builds the engine's image at ([`crate::setup::linux_image::DISK_BYTES`]).
pub const BUILT_GIB: u64 = 64;
/// `[linux_vm] disk_max_gib` when absent (ADR-0061 §1).
pub const DEFAULT_CEILING_GIB: u64 = 128;
/// The highest ceiling: the supervisor's own 1 TiB bound (§4).
pub const MAX_CEILING_GIB: u64 = 1024;
/// A grow's step (§1).
pub const STEP_GIB: u64 = 32;
/// The smallest step the Mac's free space may shrink a grow to (§2).
pub const MIN_STEP_GIB: u64 = 8;
/// How long a trigger must hold before a grow is pending (§2).
pub const SHORT_FOR: Duration = Duration::from_secs(30 * 60);
/// How long a pending grow waits for an idle moment before it drains (§3).
pub const DRAIN_AFTER: Duration = Duration::from_secs(2 * 60 * 60);
/// A gate refusal older than this no longer counts as "still refusing": the
/// gate is asked every few seconds while a claim parks behind it.
const REFUSAL_STALE: Duration = Duration::from_secs(5 * 60);
/// The fault scope a failed grow or resize raises (§5).
pub const FAULT: &str = "vm-disk-grow";
/// The reason a claim parks under while a grow drains (§3).
pub const HOLD_REASON: &str = "growing the Linux VM's disk (ADR-0061)";
/// How often the grow loop ticks.
pub const TICK: Duration = Duration::from_secs(60);

/// What a clock reading or a size is, in ms since the epoch.
type Ms = u64;

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Which of §2's two triggers holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The VM's disk has been below the janitor's floor for 30 minutes, and the
    /// janitor's last pass in that time freed nothing or backed off.
    BelowFloor,
    /// The disk gate has refused a Docker job for 30 minutes running.
    GateRefusing,
}

impl Trigger {
    fn words(self) -> &'static str {
        match self {
            Trigger::BelowFloor => {
                "the Linux VM's disk has been below the janitor's floor for 30 minutes and its \
                 last pass freed nothing"
            }
            Trigger::GateRefusing => "the disk gate has refused a Docker job for 30 minutes",
        }
    }
}

/// §2's clocks. Each is set on the first reading that starts it and cleared on
/// the first that ends it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Clocks {
    below_floor_since: Option<Ms>,
    /// The janitor's last pass while below the floor: when, and whether it freed
    /// anything.
    janitor_last: Option<(Ms, bool)>,
    gate_refusing_since: Option<Ms>,
    gate_refused_at: Option<Ms>,
}

impl Clocks {
    /// One reading of the VM's free space against the janitor's floor. `None`
    /// (unread) leaves the clock as it is: an unknown is not evidence either way.
    pub fn vm_free(&mut self, free_mib: Option<u64>, floor_gib: u64, now: Ms) {
        let Some(free) = free_mib else { return };
        if crate::host::below_disk_floor(floor_gib, Some(free)) {
            self.below_floor_since.get_or_insert(now);
        } else {
            self.below_floor_since = None;
            self.janitor_last = None;
        }
    }

    /// A janitor pass ended: `freed` when it freed something or the pressure
    /// lifted, `false` when it freed nothing or backed off.
    pub fn janitor(&mut self, freed: bool, now: Ms) {
        self.janitor_last = Some((now, freed));
    }

    /// The disk gate refused a Docker job.
    pub fn gate_refused(&mut self, now: Ms) {
        self.gate_refusing_since.get_or_insert(now);
        self.gate_refused_at = Some(now);
    }

    /// The disk gate let a Docker job through.
    pub fn gate_passed(&mut self) {
        self.gate_refusing_since = None;
        self.gate_refused_at = None;
    }

    /// The trigger that holds at `now`, if any. Below the floor counts only once
    /// the janitor has had a pass in that time that freed nothing; above it the
    /// janitor runs none, so the gate's own clock is the other trigger.
    ///
    /// `held`: the grow's own hold is on. No claim reaches the disk gate then, so
    /// its refusals stop because of the hold, not because there is room: the
    /// gate's clock does not age while it is held.
    pub fn trigger(&self, now: Ms, held: bool) -> Option<Trigger> {
        let long = |since: Ms| now.saturating_sub(since) >= ms(SHORT_FOR);
        if let Some(since) = self.below_floor_since {
            if long(since) && matches!(self.janitor_last, Some((at, false)) if at >= since) {
                return Some(Trigger::BelowFloor);
            }
        }
        if let (Some(since), Some(last)) = (self.gate_refusing_since, self.gate_refused_at) {
            if long(since) && (held || now.saturating_sub(last) <= ms(REFUSAL_STALE)) {
                return Some(Trigger::GateRefusing);
            }
        }
        None
    }
}

/// A Linux VM's disk file, as the supervisor reports it (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiskFile {
    /// The file's length: the disk's size.
    pub bytes: u64,
    /// The bytes its blocks hold on the Mac.
    pub allocated_bytes: u64,
}

/// What [`pending`] decides from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    pub trigger: Option<Trigger>,
    /// The engine's disk file; `None` while the supervisor does not report it.
    pub disk: Option<DiskFile>,
    /// The file length at which the guest's last resize found nothing left to
    /// grow (§5).
    pub resized_at: Option<u64>,
    /// Whether a `vm-disk-grow` fault stands.
    pub fault: bool,
    /// `[linux_vm] disk_max_gib`, in bytes.
    pub ceiling_bytes: u64,
    /// Free space on the Mac volume holding the disk; `None` unread.
    pub host_free_bytes: Option<u64>,
    /// `[host] disk_floor_gib`.
    pub floor_gib: u64,
}

/// Why a short disk is not growing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// The supervisor has not reported the disk's size.
    SizeUnknown,
    /// No resize report at this file length: the next start retries it.
    NotResized,
    /// A `vm-disk-grow` fault stands.
    Fault,
    /// The disk is at `disk_max_gib`.
    Ceiling,
    /// The Mac's free space is unread.
    HostUnread,
    /// The Mac's volume cannot hold even the smallest step, counting what the
    /// sparse file may still fill.
    HostRoom { spare_bytes: u64 },
}

impl Blocked {
    pub fn words(&self) -> String {
        match self {
            Blocked::SizeUnknown => {
                "waiting for the VM supervisor to report the disk's size".into()
            }
            Blocked::NotResized => {
                "waiting for the guest to grow root into the disk it has; the next start tries \
                 again"
                    .into()
            }
            Blocked::Fault => format!("held by the {FAULT} fault"),
            Blocked::Ceiling => "at [linux_vm] disk_max_gib".into(),
            Blocked::HostUnread => "waiting for a reading of the Mac's free space".into(),
            Blocked::HostRoom { spare_bytes } => format!(
                "the Mac's volume has room for {} GiB more after what the VM may still fill and \
                 the floor, less than the {MIN_STEP_GIB} GiB smallest step",
                spare_bytes / GIB
            ),
        }
    }
}

/// [`pending`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Nothing is short.
    No,
    /// Short, but not growing, and why.
    Blocked(Trigger, Blocked),
    /// A grow is pending: the disk goes from `from` to `to` bytes.
    Grow {
        trigger: Trigger,
        from: u64,
        to: u64,
    },
}

/// Whether a grow is pending (§2), from this tick's readings.
///
/// The step is [`STEP_GIB`], never past the ceiling. It shrinks to what the Mac's
/// volume can hold if the VM fills everything it is promised (`host free −
/// (length − allocated) − step ≥ floor`), down to [`MIN_STEP_GIB`]; below that
/// nothing is pending. A ceiling closer than the smallest step is still reached.
pub fn pending(i: &Inputs) -> Pending {
    let Some(trigger) = i.trigger else {
        return Pending::No;
    };
    let blocked = |b| Pending::Blocked(trigger, b);
    let Some(disk) = i.disk else {
        return blocked(Blocked::SizeUnknown);
    };
    if i.fault {
        return blocked(Blocked::Fault);
    }
    if i.resized_at != Some(disk.bytes) {
        return blocked(Blocked::NotResized);
    }
    let room = i.ceiling_bytes.saturating_sub(disk.bytes);
    if room < super::proto::DISK_BYTES_ALIGN {
        return blocked(Blocked::Ceiling);
    }
    let Some(free) = i.host_free_bytes else {
        return blocked(Blocked::HostUnread);
    };
    let debt = disk.bytes.saturating_sub(disk.allocated_bytes);
    let spare = free
        .saturating_sub(debt)
        .saturating_sub(i.floor_gib.saturating_mul(GIB));
    let mut step = (STEP_GIB * GIB).min(room);
    if spare < step {
        if spare < (MIN_STEP_GIB * GIB).min(room) {
            return blocked(Blocked::HostRoom { spare_bytes: spare });
        }
        // Whole GiB when the Mac is what limits it.
        step = spare / GIB * GIB;
    }
    let to = (disk.bytes + step) / super::proto::DISK_BYTES_ALIGN * super::proto::DISK_BYTES_ALIGN;
    if to <= disk.bytes {
        return blocked(Blocked::Ceiling);
    }
    Pending::Grow {
        trigger,
        from: disk.bytes,
        to,
    }
}

/// The grow as `/api/state` and `faber status` show it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct GrowView {
    /// The disk's size and what it holds on the Mac (§6); `null` unreported.
    pub disk_bytes: Option<u64>,
    pub disk_allocated_bytes: Option<u64>,
    /// `[linux_vm] disk_max_gib`, in bytes.
    pub max_bytes: u64,
    /// A pending grow's target size; `null` when none is pending.
    pub pending_to_bytes: Option<u64>,
    /// What a pending grow waits for, or why a short disk does not grow; `null`
    /// when nothing is short.
    pub waiting_for: Option<String>,
    /// The `vm-disk-grow` fault, while it stands.
    pub fault: Option<String>,
}

impl GrowView {
    /// `faber status`'s `disk` line, or `None` when there is nothing to say.
    pub fn line(&self) -> Option<String> {
        let size = |b: u64| format!("{} GiB", b / GIB);
        let of = match self.disk_bytes {
            Some(b) => format!("{} of at most {}", size(b), size(self.max_bytes)),
            None => format!("at most {}", size(self.max_bytes)),
        };
        if let Some(f) = &self.fault {
            return Some(format!("linux VM {of}; {FAULT}: {f}"));
        }
        match (self.pending_to_bytes, &self.waiting_for) {
            (Some(to), w) => Some(format!(
                "linux VM {of}; growing to {}{}",
                size(to),
                w.as_deref().map(|w| format!(" — {w}")).unwrap_or_default()
            )),
            (None, Some(w)) => Some(format!("linux VM {of}; short, not growing: {w}")),
            (None, None) => None,
        }
    }
}

/// Where a grow restart is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No hold: watching for an idle moment, or for the drain deadline.
    Watching,
    /// Docker claims held; idle runners being retired. `drain` waits for busy
    /// ones; an idle-moment attempt gives up on the first busy runner.
    Holding { drain: bool },
    /// The VM was stopped for the grow; the driver is starting it at the new size.
    Restarting,
}

#[derive(Debug)]
struct State {
    clocks: Clocks,
    ceiling_bytes: u64,
    floor_gib: u64,
    /// The size the next `start` asks for: the pending grow's target.
    target: Option<u64>,
    resized_at: Option<u64>,
    fault: Option<String>,
    /// The fault is a refused start's, which only the operator or a restart
    /// clears; a resize's clears at the next resize that succeeds.
    refused: bool,
    /// The fault as last raised or cleared on the dashboard.
    shown_fault: Option<String>,
    /// The engine has come into service this many times; the resize runs once
    /// for each.
    service_gen: u64,
    resized_gen: u64,
    /// A grow restart is in flight: from the stop until the driver settles.
    restarting: bool,
    pending_since: Option<Ms>,
    phase: Phase,
    /// `faber engine grow`: apply the pending grow now, by draining.
    forced: bool,
    last_disk: Option<DiskFile>,
    view: Option<GrowView>,
}

/// The grow state shared between the policy loop and the parts it touches.
#[derive(Debug)]
pub struct Grow(Mutex<State>);

impl Grow {
    pub fn new(ceiling_gib: u64, floor_gib: u64) -> Arc<Self> {
        Arc::new(Self(Mutex::new(State {
            clocks: Clocks::default(),
            ceiling_bytes: ceiling_gib.saturating_mul(GIB),
            floor_gib,
            target: None,
            resized_at: None,
            fault: None,
            refused: false,
            shown_fault: None,
            service_gen: 0,
            resized_gen: 0,
            restarting: false,
            pending_since: None,
            phase: Phase::Watching,
            forced: false,
            last_disk: None,
            view: None,
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// Set the ceiling and the janitor's floor, from config at boot.
    pub fn configure(&self, ceiling_gib: u64, floor_gib: u64) {
        let mut s = self.lock();
        s.ceiling_bytes = ceiling_gib.saturating_mul(GIB);
        s.floor_gib = floor_gib;
    }

    // --- the engine driver -------------------------------------------------

    /// The size the next `start` asks for (§3 step 1): the pending grow's
    /// target, or `None` to keep the disk as it is.
    pub fn start_size(&self) -> Option<u64> {
        self.lock().target
    }

    /// The supervisor refused the grow (`bad_request`): raise the fault, which
    /// holds further growth until the operator applies one or faber restarts, and
    /// let the driver start the VM at the size it has.
    pub fn start_refused(&self, to: u64) {
        let mut s = self.lock();
        s.target = None;
        s.refused = true;
        s.fault = Some(format!(
            "the VM supervisor refused to grow the Linux VM's disk to {} GiB; it starts at \
             its current size",
            to / GIB
        ));
    }

    /// The driver brought the engine into service: the resize runs once more.
    pub fn in_service(&self) {
        let mut s = self.lock();
        s.service_gen += 1;
        s.restarting = false;
    }

    /// A bring-up ended without service: a grow restart, if one was in flight,
    /// is over, and revival may act again.
    pub fn bring_up_failed(&self) {
        self.lock().restarting = false;
    }

    // --- reconcile ----------------------------------------------------------

    /// A grow restart is in flight: revival holds off and Gate 0's "Docker is
    /// down" fault is muted (§3).
    pub fn restarting(&self) -> bool {
        self.lock().restarting
    }

    // --- the limiter and the janitor ---------------------------------------

    pub fn gate_refused(&self, now: Ms) {
        self.lock().clocks.gate_refused(now);
    }

    pub fn gate_passed(&self) {
        self.lock().clocks.gate_passed();
    }

    pub fn janitor(&self, freed: bool, now: Ms) {
        self.lock().clocks.janitor(freed, now);
    }

    // --- the operator and the dashboard ------------------------------------

    /// `faber engine grow`: apply the pending grow now, by the drain, with no
    /// wait. Clears a refused-start fault, so a grow the operator asks for is
    /// tried again. `Err` with why when none is pending.
    pub fn force(&self) -> Result<String, String> {
        let mut s = self.lock();
        let refused = std::mem::take(&mut s.refused);
        if refused {
            s.fault = None;
        }
        match s.view.as_ref().and_then(|v| v.pending_to_bytes) {
            Some(to) => {
                s.forced = true;
                Ok(format!(
                    "growing the Linux VM's disk to {} GiB: Docker claims are held until the \
                     jobs in it finish, then it restarts",
                    to / GIB
                ))
            }
            None if refused => {
                s.forced = true;
                Ok("the refused grow is tried again at the next check, within a minute".into())
            }
            None => Err(match s.view.as_ref().and_then(|v| v.waiting_for.clone()) {
                Some(why) => format!("no grow is pending: {why}"),
                None => "no grow is pending: the Linux VM's disk is not short".into(),
            }),
        }
    }

    pub fn view(&self) -> Option<GrowView> {
        self.lock().view.clone()
    }
    /// A state with a grow to `to` bytes pending, for the engine's tests.
    #[cfg(test)]
    pub(crate) fn with_target_for_test(to: u64) -> Arc<Self> {
        let g = Self::new(DEFAULT_CEILING_GIB, 25);
        g.lock().target = Some(to);
        g
    }
}

/// The process's one grow state. Configured at boot by the loop
/// ([`spawn`]); until then it holds the defaults and grows nothing.
pub fn global() -> &'static Arc<Grow> {
    static G: OnceLock<Arc<Grow>> = OnceLock::new();
    G.get_or_init(|| Grow::new(DEFAULT_CEILING_GIB, 0))
}

/// What the loop reads and acts through.
#[allow(async_fn_in_trait)]
pub trait GrowHost {
    /// The engine's record: whether it is in service, and its disk file.
    async fn engine(&self) -> Option<(bool, Option<DiskFile>)>;
    /// Free space on the Mac volume holding the disk, bytes.
    async fn host_free_bytes(&self) -> Option<u64>;
    /// The Linux VM's free space (MiB) from the latest fresh reading.
    fn vm_free_mib(&self) -> Option<u64>;
    /// Docker runners up now: admitted claims. An idle moment is none.
    async fn busy(&self) -> usize;
    /// One retire pass (ADR-0051's `retire_unwatched`): idle runners withdrawn and
    /// removed, capped ones removed at their cap, busy ones left. `None`: Docker
    /// unread, which is not "none left".
    async fn retire(&self) -> Option<Left>;
    /// Hold or release Docker claims under [`HOLD_REASON`].
    fn hold(&self, on: bool);
    /// Stop the engine; the driver starts it again at the target size.
    async fn stop_engine(&self) -> anyhow::Result<()>;
    /// Run §5's resize in the guest. `Ok` once root fills the disk.
    async fn resize(&self) -> Result<(), String>;
    /// The disk grew: log and audit `engine_disk_grown`.
    async fn grown(&self, from: u64, to: u64);
    /// Raise or clear the `vm-disk-grow` fault.
    async fn fault(&self, fault: Option<String>);
}

/// The loop's own state across ticks.
pub struct Applier {
    pub grow: Arc<Grow>,
}

impl Applier {
    /// One pass: read, resize if the engine came into service, decide, apply.
    pub async fn tick(&self, host: &impl GrowHost, now: Ms) {
        let engine = host.engine().await;
        let (in_service, disk) = engine.unwrap_or((false, None));
        let host_free = host.host_free_bytes().await;
        let vm_free = host.vm_free_mib();

        let (grew, resize_due) = {
            let mut s = self.grow.lock();
            let floor = s.floor_gib;
            s.clocks.vm_free(vm_free, floor, now);
            let grew = match (s.last_disk, disk) {
                (Some(a), Some(b)) if b.bytes > a.bytes => Some((a.bytes, b.bytes)),
                _ => None,
            };
            // A grown disk is a new situation: the 30-minute clocks start over, so
            // one long refusal never sets off a second step before the first is
            // seen to help.
            if grew.is_some() {
                s.clocks = Clocks::default();
            }
            if disk.is_some() {
                s.last_disk = disk;
            }
            (grew, in_service && s.service_gen != s.resized_gen)
        };
        if let Some((from, to)) = grew {
            host.grown(from, to).await;
        }

        // §5: the guest grows root at every start, before anything is decided on
        // the size it now has.
        if resize_due {
            let result = host.resize().await;
            let mut s = self.grow.lock();
            s.resized_gen = s.service_gen;
            match result {
                Ok(()) => {
                    s.resized_at = disk.map(|d| d.bytes);
                    if !s.refused {
                        s.fault = None;
                    }
                }
                Err(why) => {
                    s.fault = Some(why);
                    s.refused = false;
                }
            }
        }
        // The fault on the dashboard follows the state, whoever changed it: a
        // resize here, or a refused start in the driver.
        let changed = {
            let mut s = self.grow.lock();
            (s.fault != s.shown_fault).then(|| {
                s.shown_fault = s.fault.clone();
                s.fault.clone()
            })
        };
        if let Some(fault) = changed {
            host.fault(fault).await;
        }

        let decided = {
            let s = self.grow.lock();
            let held = matches!(s.phase, Phase::Holding { .. } | Phase::Restarting);
            pending(&Inputs {
                trigger: s.clocks.trigger(now, held),
                disk,
                resized_at: s.resized_at,
                fault: s.fault.is_some(),
                ceiling_bytes: s.ceiling_bytes,
                host_free_bytes: host_free,
                floor_gib: s.floor_gib,
            })
        };
        self.apply(host, decided, disk, in_service, now).await;
    }

    fn publish(
        &self,
        s: &mut State,
        decided: &Pending,
        disk: Option<DiskFile>,
        waits: Option<String>,
    ) {
        let (to, waiting_for) = match decided {
            Pending::No => (None, None),
            Pending::Blocked(_, b) => (None, Some(b.words())),
            Pending::Grow { trigger, to, .. } => (
                Some(*to),
                Some(waits.unwrap_or_else(|| format!("pending because {}", trigger.words()))),
            ),
        };
        s.view = Some(GrowView {
            disk_bytes: disk.map(|d| d.bytes),
            disk_allocated_bytes: disk.map(|d| d.allocated_bytes),
            max_bytes: s.ceiling_bytes,
            pending_to_bytes: to,
            waiting_for,
            fault: s.fault.clone(),
        });
    }

    async fn apply(
        &self,
        host: &impl GrowHost,
        decided: Pending,
        disk: Option<DiskFile>,
        in_service: bool,
        now: Ms,
    ) {
        let to = match decided {
            Pending::Grow { to, .. } => Some(to),
            _ => None,
        };
        let phase = {
            let mut s = self.grow.lock();
            match s.phase {
                // The driver settled: the restart is over either way.
                Phase::Restarting if !s.restarting => {
                    s.phase = Phase::Watching;
                    s.forced = false;
                    drop(s);
                    host.hold(false);
                    s = self.grow.lock();
                }
                _ => {}
            }
            if s.phase != Phase::Restarting {
                s.target = to;
                if to.is_some() {
                    s.pending_since.get_or_insert(now);
                } else {
                    s.pending_since = None;
                    s.forced = false;
                }
            }
            s.phase
        };

        let waits = match (phase, to) {
            (Phase::Restarting, _) => Some("restarting the Linux VM at the new size".to_string()),
            (Phase::Holding { .. }, None) => {
                self.grow.lock().phase = Phase::Watching;
                host.hold(false);
                None
            }
            (Phase::Watching, None) => None,
            // A VM not in service is starting anyway, at the target size: nothing
            // to stop.
            (Phase::Watching, Some(_)) if !in_service => {
                Some("the Linux VM is not in service; its next start grows the disk".to_string())
            }
            (Phase::Watching, Some(_)) => {
                let (since, forced) = {
                    let s = self.grow.lock();
                    (s.pending_since.unwrap_or(now), s.forced)
                };
                let drain = forced || now.saturating_sub(since) >= ms(DRAIN_AFTER);
                if drain || host.busy().await == 0 {
                    info!(
                        "grow: holding Docker claims to grow the Linux VM's disk ({})",
                        if drain { "draining" } else { "an idle moment" }
                    );
                    host.hold(true);
                    self.grow.lock().phase = Phase::Holding { drain };
                    self.retire_and_stop(host, drain).await
                } else {
                    let left = ms(DRAIN_AFTER).saturating_sub(now.saturating_sub(since));
                    Some(format!(
                        "waiting for an idle moment; drains in {} min if none comes",
                        left.div_ceil(60_000)
                    ))
                }
            }
            (Phase::Holding { drain }, Some(_)) => {
                let forced = self.grow.lock().forced;
                self.retire_and_stop(host, drain || forced).await
            }
        };
        let mut s = self.grow.lock();
        self.publish(&mut s, &decided, disk, waits);
    }

    /// With claims held: one retire pass, and once it leaves no runner, a second
    /// to check none became busy in between (a JIT runner is not pinned to a job,
    /// and GitHub may hand an idle one a job at any moment). Then the stop. An
    /// idle-moment attempt that finds a busy runner releases the hold and waits
    /// for the next; a drain keeps holding until the busy ones finish. Nothing
    /// here stops a VM with a runner in it.
    async fn retire_and_stop(&self, host: &impl GrowHost, drain: bool) -> Option<String> {
        let give_up = |why: String| {
            if drain {
                Some(why)
            } else {
                self.grow.lock().phase = Phase::Watching;
                host.hold(false);
                info!("grow: no idle moment after all ({why}); claiming again");
                Some("waiting for an idle moment".to_string())
            }
        };
        match host.retire().await {
            None => return give_up("cannot read the Linux VM's Docker".into()),
            Some(l) if l.jobs > 0 => {
                return give_up(format!(
                    "draining: waiting for {} docker job{} to finish",
                    l.jobs,
                    if l.jobs == 1 { "" } else { "s" }
                ))
            }
            Some(_) => {}
        }
        match host.retire().await {
            Some(l) if l.jobs == 0 => {}
            None => return give_up("cannot read the Linux VM's Docker".into()),
            Some(_) => return give_up("a runner took a job as the idle ones were retired".into()),
        }
        {
            let mut s = self.grow.lock();
            s.restarting = true;
            s.phase = Phase::Restarting;
        }
        info!("grow: no docker job left in the Linux VM; restarting it to grow its disk");
        if let Err(e) = host.stop_engine().await {
            warn!("grow: stopping the Linux VM failed: {e}; claiming again");
            let mut s = self.grow.lock();
            s.restarting = false;
            s.phase = Phase::Watching;
            drop(s);
            host.hold(false);
            return Some(format!("stopping the Linux VM failed: {e}"));
        }
        Some("restarting the Linux VM at the new size".to_string())
    }
}

/// What the grow loop is booted with, from config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrowBoot {
    /// `[linux_vm] disk_max_gib`.
    pub ceiling_gib: u64,
    /// `[host] disk_floor_gib`.
    pub floor_gib: u64,
    /// `[mirror] max_size_gib`, when `[mirror]` is set (§7).
    pub mirror_gib: Option<u64>,
    /// `[host] cache_max_gib` (§7).
    pub cache_max_gib: Option<u64>,
    /// The runner lifetime cap a retire pass removes runners at.
    pub runner_timeout: Duration,
}

/// The grow loop's [`GrowHost`] on this host: the VM supervisor, the
/// orchestrator, and Docker inside the Linux VM.
pub struct ThisHost<S> {
    pub supervisor: S,
    pub orch: Arc<crate::reconcile::Orchestrator>,
    pub boot: GrowBoot,
}

impl<S: super::job::VmSupervisor> ThisHost<S> {
    /// §7, at boot and after every grow: warn when the caches' limits and the
    /// floor leave less than half the disk for jobs and images. Keeps the
    /// operator's numbers either way.
    async fn check_caches(&self, disk_bytes: u64) {
        let Some(u) = crate::docker::cache_usage(Duration::from_secs(60)).await else {
            return;
        };
        let sum = CacheSum {
            mirror_gib: self.boot.mirror_gib,
            cache_volumes_gib: u.cache_volumes_gib,
            cache_max_gib: self.boot.cache_max_gib,
            git_mirrors_gib: u.git_mirrors_gib,
            variants_gib: u.variants_gib,
            floor_gib: self.boot.floor_gib,
        };
        if let Some(w) = sum.warning(disk_bytes) {
            warn!("grow: {w}");
        }
    }
}

impl<S: super::job::VmSupervisor> GrowHost for ThisHost<S> {
    async fn engine(&self) -> Option<(bool, Option<DiskFile>)> {
        let v = self
            .supervisor
            .list()
            .await
            .ok()?
            .into_iter()
            .find(|v| v.claim.as_str() == super::engine::ENGINE_CLAIM)?;
        let disk = v
            .disk_bytes
            .zip(v.disk_allocated_bytes)
            .map(|(b, a)| DiskFile {
                bytes: b,
                allocated_bytes: a,
            });
        Some((v.state == super::proto::VmState::InService, disk))
    }
    async fn host_free_bytes(&self) -> Option<u64> {
        let path = crate::host::docker_data_path().await?;
        crate::host::disk_free_mib(&path)
            .await
            .map(|m| m.saturating_mul(1 << 20))
    }
    fn vm_free_mib(&self) -> Option<u64> {
        let now = crate::audit::now_ms();
        crate::host::latest_linux_vm_disk().and_then(|r| r.fresh_free_mib(now))
    }
    async fn busy(&self) -> usize {
        self.orch.admitted_docker_count().await
    }
    async fn retire(&self) -> Option<Left> {
        self.orch.retire_unwatched(self.boot.runner_timeout).await
    }
    fn hold(&self, on: bool) {
        self.orch.limiter().set_grow_hold(on);
    }
    async fn stop_engine(&self) -> anyhow::Result<()> {
        super::engine::stop_for_revival(&self.supervisor, &super::engine::EngineTimeouts::default())
            .await
    }
    async fn resize(&self) -> Result<(), String> {
        crate::docker::grow_guest_root(Duration::from_secs(300)).await
    }
    async fn grown(&self, from: u64, to: u64) {
        info!(
            "grow: the Linux VM's disk grew from {} to {} GiB",
            from / GIB,
            to / GIB
        );
        self.orch.record_engine_disk_grown(from, to).await;
        self.check_caches(to).await;
    }
    async fn fault(&self, fault: Option<String>) {
        self.orch.observe_fault("host", FAULT, fault).await;
    }
}

/// `faber engine grow`: ask the daemon over its control socket to apply the
/// pending grow now (§3). No `sudo`: a routine request (ADR-0028).
pub async fn cli() -> anyhow::Result<()> {
    use crate::control::{self, Fallback};
    let socket = crate::platform::control_socket()
        .ok_or_else(|| anyhow::anyhow!("no HOME to find the daemon's control socket under"))?;
    let (status, said) = control::request(
        &socket,
        Fallback::Never,
        reqwest::Method::POST,
        "/api/engine/grow",
        None,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", crate::drain::client_error_message(&e)))?;
    let said = crate::term::sanitize_bounded(said.trim(), 512).into_owned();
    if !status.is_success() {
        anyhow::bail!("{said}");
    }
    println!("{said}; `faber status` shows where it is");
    Ok(())
}

/// Run the grow loop for this host's engine (ADR-0061), every [`TICK`].
/// Configures [`global`] first, so the engine driver, the limiter and the
/// janitor share its state. Never returns.
pub async fn run<S: super::job::VmSupervisor>(host: ThisHost<S>) {
    let grow = global().clone();
    grow.configure(host.boot.ceiling_gib, host.boot.floor_gib);
    let applier = Applier { grow };
    let mut checked = false;
    loop {
        applier.tick(&host, crate::audit::now_ms()).await;
        // §7 at boot: once the disk's size is known.
        if !checked {
            if let Some(b) = applier.grow.view().and_then(|v| v.disk_bytes) {
                host.check_caches(b).await;
                checked = true;
            }
        }
        tokio::time::sleep(TICK).await;
    }
}

/// §7: the caches' limits against the disk they live on. What counts, in GiB:
/// the mirror's one cache volume at `[mirror] max_size_gib` when `[mirror]` is
/// set; each existing cache volume at `cache_max_gib`, or at its current size
/// where no limit is set; git mirrors and runner-image variants at their current
/// size. A warning when that plus `disk_floor_gib` leaves less than half the
/// disk for jobs and images; `None` otherwise.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheSum {
    pub mirror_gib: Option<u64>,
    /// Each existing cache volume's current size, GiB.
    pub cache_volumes_gib: Vec<f64>,
    pub cache_max_gib: Option<u64>,
    pub git_mirrors_gib: f64,
    pub variants_gib: f64,
    pub floor_gib: u64,
}

impl CacheSum {
    pub fn warning(&self, disk_bytes: u64) -> Option<String> {
        let disk_gib = disk_bytes as f64 / GIB as f64;
        let limit = self.cache_max_gib.filter(|&g| g > 0);
        let caches: f64 = match limit {
            Some(max) => max as f64 * self.cache_volumes_gib.len() as f64,
            None => self.cache_volumes_gib.iter().sum(),
        };
        let sum = self.mirror_gib.unwrap_or(0) as f64
            + caches
            + self.git_mirrors_gib
            + self.variants_gib
            + self.floor_gib as f64;
        if sum <= disk_gib / 2.0 {
            return None;
        }
        let per_volume = match limit {
            Some(max) => format!(
                "{} cache volume(s) at host.cache_max_gib = {max}",
                self.cache_volumes_gib.len()
            ),
            None => format!(
                "{} cache volume(s) at their current size (no host.cache_max_gib is set)",
                self.cache_volumes_gib.len()
            ),
        };
        Some(format!(
            "the caches' limits and the janitor's floor add up to {sum:.0} GiB, more than half \
             the Linux VM's {disk_gib:.0} GiB disk (ADR-0061 §7): the mirror's cache at {} GiB, \
             {per_volume}, git mirrors at {:.0} GiB, runner-image variants at {:.0} GiB and \
             host.disk_floor_gib = {}. Jobs and images get less than half the disk; lower a \
             limit or the floor",
            self.mirror_gib.unwrap_or(0),
            self.git_mirrors_gib,
            self.variants_gib,
            self.floor_gib
        ))
    }
}

#[cfg(test)]
mod tests;
