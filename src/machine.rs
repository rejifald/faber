//! The machine faber runs on, as distinct from the Docker daemon's view of it —
//! ADR-0021, with the swap signal ADR-0032 §3 added to it.
//!
//! Every other budget faber keeps is sized from `docker info`. On macOS that is a
//! VM carved out of the operator's Mac, and nothing else in faber accounts for
//! the Mac itself. This module is the pure half of doing so: what a reading of
//! the machine says, the share the operator declared for it, and three decisions
//! made from the two.
//!
//! - **Planning within the share** (§2b). Gate 1's memory term becomes
//!   `min(MemTotal, machine RAM − keep_memory_gib)` and ADR-0019's core count
//!   `min(NCPU, machine cores − keep_cpus)`.
//! - **Checking the VM against it** (§2a). A VM larger than the share leaves is a
//!   warning worded as its fix, because on macOS the VM never gives memory back
//!   and its size is the only memory reservation the machine has.
//! - **Yielding to the machine's memory pressure** (§2c). Admission pauses while
//!   macOS reports `warn` or `critical`, and while swap use has grown by 1 GiB in
//!   the last 5 minutes. It covers macOS VM jobs as well, since their VMs take
//!   the machine's own memory.
//!
//! - **Measuring the co-tenant reserve** (§3). Gate 1 holds back the larger of
//!   `reserve_mib` and the 24-hour peak of what the VM uses beyond faber's own
//!   containers. The window is [`crate::cotenant`]; its state lives here, beside
//!   the share that turns it on.
//!
//! The reading itself is taken behind the platform seam
//! ([`crate::platform::host::read_machine`]); the limiter owns the state
//! ([`MachineState`]) and the loop that feeds it.
//!
//! # Absent keys
//!
//! Both keys tighten admission on hosts that update unattended (ADR-0006), so an
//! absent key keeps the behaviour from before them, as `dind` (ADR-0013) and the
//! disk floor (ADR-0024) do: no share, no pressure gate, a warning at every boot
//! with the measured split, `unstated` on the dashboard, and an explicit `0`
//! written by the first settings save. `0` is stated and changes nothing either;
//! for memory it also leaves the pressure gate off, since a share of nothing has
//! nothing to yield for, and the co-tenant reserve at `reserve_mib`.

use std::collections::VecDeque;

use serde::Serialize;

use crate::cotenant::{self, CoTenantPeak, CoTenantView};
use crate::host::fmt_gib;

/// How much swap growth inside [`SWAP_WINDOW_MS`] pauses admission
/// (ADR-0032 §3): 1 GiB.
pub const SWAP_GROWTH_MIB: u64 = 1024;

/// The window swap growth is measured over, and how long it must then stay flat
/// before admission resumes (ADR-0032 §3): 5 minutes.
pub const SWAP_WINDOW_MS: u64 = 5 * 60 * 1000;

/// The most swap may still rise inside the window and count as flat.
///
/// Swap use under memory pressure is never perfectly still: macOS pages a few
/// megabytes in and out every tick. A threshold of zero would hold admission
/// indefinitely on that noise, so "flat" allows a quarter of the growth that
/// pauses — far below what a machine short of memory adds in five minutes.
pub const SWAP_FLAT_MIB: u64 = 256;

/// How old a reading of the machine may be and still be evidence. Past this the
/// pressure gate opens, as every other gate does on a reading that has stopped:
/// a probe that broke must not hold admission shut forever.
pub const MACHINE_READING_MAX_AGE_MS: u64 = 2 * 60 * 1000;

/// macOS's memory-pressure level, `kern.memorystatus_vm_pressure_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PressureLevel {
    /// `1`.
    Normal,
    /// `2`. Admission pauses (ADR-0021 §2c).
    Warn,
    /// `4`.
    Critical,
}

impl PressureLevel {
    /// The sysctl's value, or `None` for one the kernel does not define.
    pub fn from_sysctl(level: u64) -> Option<Self> {
        match level {
            1 => Some(Self::Normal),
            2 => Some(Self::Warn),
            4 => Some(Self::Critical),
            _ => None,
        }
    }

    /// Whether admission pauses at this level. The kernel's levels carry their own
    /// hysteresis, so there is nothing to tune: pause above normal, resume at it.
    pub fn pauses(self) -> bool {
        !matches!(self, Self::Normal)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }
}

/// One reading of the machine (ADR-0021 §1).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MachineReading {
    /// Physical memory (MiB): `hw.memsize` on macOS, `MemTotal` on Linux.
    pub mem_mib: u64,
    /// Cores: `hw.ncpu` on macOS, the process's CPU affinity on Linux.
    pub cpus: u64,
    /// The performance/efficiency split, where the machine has one (Apple
    /// silicon). For display only.
    pub perf_cpus: Option<u64>,
    pub efficiency_cpus: Option<u64>,
    /// macOS's pressure level. `None` on Linux, where the gate is inert until
    /// thresholds are chosen from a Linux host's measurements (ADR-0021 §2c).
    pub pressure: Option<PressureLevel>,
    /// Linux's `/proc/pressure/memory` `some avg60`, shown and not gated on.
    pub psi_some_avg60: Option<f64>,
    pub swap_used_mib: Option<u64>,
    pub swap_total_mib: Option<u64>,
    /// `vm.compressor_bytes_used` (MiB), macOS only.
    pub compressor_mib: Option<u64>,
}

/// The machine's size alone: what `faber check-config` and the settings save
/// need to say whether a share fits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineSize {
    pub mem_mib: u64,
    pub cpus: u64,
}

impl From<&MachineReading> for MachineSize {
    fn from(r: &MachineReading) -> Self {
        Self {
            mem_mib: r.mem_mib,
            cpus: r.cpus,
        }
    }
}

/// `[host] keep_memory_gib` and `keep_cpus`: what the Docker VM and CI must leave
/// to the machine (ADR-0021 §2). `None` is unstated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MachineShare {
    pub keep_memory_gib: Option<u64>,
    pub keep_cpus: Option<u64>,
}

impl MachineShare {
    /// `declared` when both keys are stated, `unstated` when neither is, and
    /// `partial` in between — each key governs its own half.
    pub fn as_str(self) -> &'static str {
        match (self.keep_memory_gib, self.keep_cpus) {
            (Some(_), Some(_)) => "declared",
            (None, None) => "unstated",
            _ => "partial",
        }
    }

    /// Whether admission yields to the machine's memory pressure and to growing
    /// swap (ADR-0021 §2c, ADR-0032 §3): once a memory share is declared, and one
    /// of more than nothing. Absent or `0` keeps the behaviour from before.
    pub fn yields_to_pressure(self) -> bool {
        self.keep_memory_gib.is_some_and(|gib| gib > 0)
    }

    /// Whether Gate 1's reserve is measured (ADR-0021 §3): on the same terms as
    /// the pressure gate, since §3 applies "with the share declared". Absent or
    /// `0` keeps `reserve_mib` alone, the behaviour from before.
    pub fn measures_reserve(self) -> bool {
        self.yields_to_pressure()
    }

    /// What the share leaves CI of the machine's memory (MiB), when a share is
    /// stated and fits the machine. A share as large as the machine is refused
    /// where a human is looking ([`MachineShare::errors`]) and ignored where one
    /// is not: planning against zero would park every claim forever.
    ///
    /// `0` is no share, so it leaves nothing to plan within: `min(VM, machine)`
    /// is not a no-op where the machine reads smaller than the daemon — a Linux
    /// process confined by affinity or a cgroup quota — and the first settings
    /// save writes `0` on every host.
    pub fn memory_left_mib(self, machine: MachineSize) -> Option<u64> {
        let keep = self.keep_memory_gib.filter(|&gib| gib > 0)?;
        machine
            .mem_mib
            .checked_sub(keep.saturating_mul(1024))
            .filter(|&left| left > 0)
    }

    /// What the share leaves CI of the machine's cores, on the same terms.
    pub fn cpus_left(self, machine: MachineSize) -> Option<u64> {
        let keep = self.keep_cpus.filter(|&n| n > 0)?;
        machine.cpus.checked_sub(keep).filter(|&left| left > 0)
    }

    /// Why this share does not fit `machine`, if it does not: one sentence per
    /// key. `faber check-config` and the settings save reject on these; boot only
    /// warns, and the share is then ignored (AGENTS.md: reject where a human is
    /// looking, warn where they are not).
    pub fn errors(self, machine: MachineSize) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(keep) = self.keep_memory_gib {
            if keep > 0 && self.memory_left_mib(machine).is_none() {
                out.push(format!(
                    "host.keep_memory_gib ({keep} GiB) is not smaller than this machine's \
                     {} GiB of memory: it would leave CI nothing (ADR-0021)",
                    fmt_gib(machine.mem_mib)
                ));
            }
        }
        if let Some(keep) = self.keep_cpus {
            if keep > 0 && self.cpus_left(machine).is_none() {
                out.push(format!(
                    "host.keep_cpus ({keep}) is not smaller than this machine's {} cores: it \
                     would leave CI none (ADR-0021)",
                    machine.cpus
                ));
            }
        }
        out
    }
}

/// The memory Gate 1 plans against (MiB): the VM's total, or what the share
/// leaves CI of the machine if that is less (ADR-0021 §2b). For memory this
/// limits CI's share of the VM, and so the paging CI causes; it cannot shrink the
/// VM's footprint. An unknown machine or an unstated share plans as before.
pub fn plan_mem_mib(vm_mib: u64, machine: Option<MachineSize>, share: MachineShare) -> u64 {
    match machine.and_then(|m| share.memory_left_mib(m)) {
        Some(left) => vm_mib.min(left),
        None => vm_mib,
    }
}

/// The core count ADR-0019's CPU budget is built on: the VM's, or what the share
/// leaves CI of the machine if that is less. For CPU this is a real reservation,
/// even before the VM is resized: a vCPU costs the machine only while CI runs on
/// it.
pub fn plan_ncpu(vm_ncpu: u64, machine: Option<MachineSize>, share: MachineShare) -> u64 {
    match machine.and_then(|m| share.cpus_left(m)) {
        Some(left) => vm_ncpu.min(left),
        None => vm_ncpu,
    }
}

/// Whether the daemon runs behind a VM boundary: its memory or cores are fewer
/// than the machine's. The comparison decides it, not the platform — faber's
/// Linux VM (ADR-0026) is one, and a native Linux daemon has none.
pub fn vm_boundary(vm_mib: u64, vm_ncpu: u64, machine: MachineSize) -> bool {
    (vm_mib > 0 && vm_mib < machine.mem_mib) || (vm_ncpu > 0 && vm_ncpu < machine.cpus)
}

/// The warnings for VMs larger than the share leaves CI (ADR-0021 §2a, ADR-0026
/// "Size and recovery"), one per resource, each worded as its fix and carrying the
/// reason: nothing else will tell the operator the VM never returns memory. Empty
/// when they fit, or when there is no share or no measurement to check it against.
/// faber never resizes a running VM itself.
///
/// `vm_mib` and `vm_ncpu` are the Linux VM's that serves Docker; `macos_mib` is
/// what the macOS VMs may take beside it at their cap (zero where faber runs
/// none). The share covers every VM faber runs, so memory counts both; cores
/// count the Linux VM only, since a vCPU costs the machine only while it runs.
pub fn encroachment(
    vm_mib: u64,
    vm_ncpu: u64,
    macos_mib: u64,
    machine: MachineSize,
    share: MachineShare,
) -> Vec<String> {
    let mut out = Vec::new();
    if let (Some(left), Some(keep)) = (share.memory_left_mib(machine), share.keep_memory_gib) {
        let held = vm_mib.saturating_add(macos_mib);
        if vm_mib > 0 && held > left {
            let what = if macos_mib == 0 {
                format!("the Docker VM holds {} GiB", fmt_gib(vm_mib))
            } else {
                format!(
                    "faber's VMs hold {} GiB (the Linux VM's {} and up to {} for macOS VMs)",
                    fmt_gib(held),
                    fmt_gib(vm_mib),
                    fmt_gib(macos_mib)
                )
            };
            let fix = match left.checked_sub(macos_mib).filter(|fit| *fit > 0) {
                Some(fit) => format!("set [linux_vm] memory_mib to {fit} or less"),
                None => format!(
                    "the macOS VMs alone may take {} GiB of it, so lower host.keep_memory_gib",
                    fmt_gib(macos_mib)
                ),
            };
            out.push(format!(
                "{what} of this machine's {} GiB, more than the {} GiB that \
                 host.keep_memory_gib = {keep} leaves CI: {fix}. The Linux VM never gives \
                 memory back to the machine, so its size is the machine's only memory \
                 reservation, and macOS memory pressure will not clear while it is larger: \
                 faber runs one Docker job at a time meanwhile (ADR-0021, ADR-0026)",
                fmt_gib(machine.mem_mib),
                fmt_gib(left),
            ));
        }
    }
    if let (Some(left), Some(keep)) = (share.cpus_left(machine), share.keep_cpus) {
        if vm_ncpu > left {
            out.push(format!(
                "the Docker VM has {vm_ncpu} of this machine's {} cores, more than the {left} \
                 that host.keep_cpus = {keep} leaves CI: set [linux_vm] cpus to {left} or \
                 less. faber plans within {left} meanwhile (ADR-0021)",
                machine.cpus
            ));
        }
    }
    out
}

/// The boot warnings for an unstated share (ADR-0021 §2): one per absent key,
/// each with the measured split where there is one to measure. `vm_mib` and
/// `vm_ncpu` are `0` when the daemon has not been measured, as on a host with no
/// Docker profile.
pub fn unstated_warnings(
    share: MachineShare,
    machine: Option<MachineSize>,
    vm_mib: u64,
    vm_ncpu: u64,
) -> Vec<String> {
    let mut out = Vec::new();
    if share.keep_memory_gib.is_none() {
        let split = match machine {
            Some(m) if vm_mib > 0 && vm_mib < m.mem_mib => format!(
                " The Docker VM holds {} of this machine's {} GiB, leaving it {}.",
                fmt_gib(vm_mib),
                fmt_gib(m.mem_mib),
                fmt_gib(m.mem_mib - vm_mib)
            ),
            Some(m) => format!(" This machine has {} GiB.", fmt_gib(m.mem_mib)),
            None => String::new(),
        };
        out.push(format!(
            "host.keep_memory_gib is unstated — faber plans CI against the whole Docker VM, \
             does not pause for this machine's memory pressure or growing swap, and holds back \
             reserve_mib rather than what the VM's other tenants were measured using \
             (ADR-0021, ADR-0032).{split} Set the RAM the machine keeps in GiB (e.g. half of it) \
             to enable all three, or 0 to silence this"
        ));
    }
    if share.keep_cpus.is_none() {
        let split = match machine {
            Some(m) if vm_ncpu > 0 && vm_ncpu < m.cpus => format!(
                " The Docker VM has {vm_ncpu} of this machine's {} cores, leaving it {}.",
                m.cpus,
                m.cpus - vm_ncpu
            ),
            Some(m) => format!(" This machine has {} cores.", m.cpus),
            None => String::new(),
        };
        out.push(format!(
            "host.keep_cpus is unstated — faber plans CI's CPU against every core the Docker VM \
             has (ADR-0021).{split} Set the cores CI must leave idle (e.g. 4) to reserve them, \
             or 0 to silence this"
        ));
    }
    out
}

/// Why the machine is holding admission (ADR-0021 §2c, ADR-0032 §3). Docker and
/// macOS VM claims alike park on it and say so.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MachineHold {
    /// macOS reports `warn` or `critical`.
    Pressure(PressureLevel),
    /// Swap use rose by `rise_mib` inside the window, and has not yet been flat
    /// for a whole one.
    Swap { rise_mib: u64 },
}

impl MachineHold {
    /// The gate's key and a line saying why — see [`crate::limiter::WaitView`].
    pub fn view(&self) -> (&'static str, String) {
        match self {
            MachineHold::Pressure(level) => (
                "machine-pressure",
                format!(
                    "this machine's memory pressure is {} — no more than one job runs until it \
                     is normal",
                    level.as_str()
                ),
            ),
            MachineHold::Swap { rise_mib } => (
                "swap",
                format!(
                    "swap grew {} GiB in the last 5 minutes — nothing new starts until it has \
                     been flat for 5",
                    fmt_gib(*rise_mib)
                ),
            ),
        }
    }
}

/// Swap use over the last [`SWAP_WINDOW_MS`], and whether it counts as growing
/// (ADR-0032 §3).
///
/// Growth is the largest rise from any reading to a later one inside the window,
/// so a dip followed by a climb counts and a steady fall does not. It starts at
/// [`SWAP_GROWTH_MIB`] and ends below [`SWAP_FLAT_MIB`], holding in between.
/// Because the rise that started it stays in the window for the window's length,
/// the end comes no sooner than five flat minutes after it.
#[derive(Debug, Clone, Default)]
pub struct SwapWindow {
    /// `(at_ms, used_mib)`, oldest first.
    samples: VecDeque<(u64, u64)>,
    growing: bool,
}

impl SwapWindow {
    /// Add one reading taken at `at_ms`, forget those older than the window, and
    /// step the hysteresis.
    pub fn observe(&mut self, at_ms: u64, used_mib: u64) {
        self.samples.push_back((at_ms, used_mib));
        while self
            .samples
            .front()
            .is_some_and(|&(t, _)| at_ms.saturating_sub(t) > SWAP_WINDOW_MS)
        {
            self.samples.pop_front();
        }
        let rise = self.rise_mib();
        if rise >= SWAP_GROWTH_MIB {
            self.growing = true;
        } else if rise < SWAP_FLAT_MIB {
            self.growing = false;
        }
    }

    /// The largest rise inside the window (MiB).
    pub fn rise_mib(&self) -> u64 {
        let mut low = u64::MAX;
        let mut rise = 0;
        for &(_, mib) in &self.samples {
            low = low.min(mib);
            rise = rise.max(mib - low);
        }
        rise
    }

    pub fn growing(&self) -> bool {
        self.growing
    }
}

/// The limiter's view of the machine: the share, the latest reading, and the
/// swap window. Held under its own lock, apart from the admission ledger.
#[derive(Debug, Clone, Default)]
pub struct MachineState {
    share: MachineShare,
    /// The latest successful reading and when it was taken. A failed one replaces
    /// nothing, so this ages out rather than vanishing.
    reading: Option<(MachineReading, u64)>,
    swap: SwapWindow,
    /// What the VM uses beyond faber's containers: the 24-hour peak (§3), and the
    /// latest sample with when it was taken. Sampled whether or not a share is
    /// declared, so the dashboard shows what the measurement would hold back
    /// before anyone turns it on.
    cotenant: CoTenantPeak,
    cotenant_latest: Option<(u64, u64)>,
}

impl MachineState {
    pub fn set_share(&mut self, share: MachineShare) {
        self.share = share;
    }

    pub fn share(&self) -> MachineShare {
        self.share
    }

    /// Store one reading taken at `at_ms`. `None` is a reading that failed: it
    /// stores nothing, and the last good one stands until it is too old.
    pub fn observe(&mut self, reading: Option<MachineReading>, at_ms: u64) {
        let Some(reading) = reading else {
            return;
        };
        if let Some(used) = reading.swap_used_mib {
            self.swap.observe(at_ms, used);
        }
        self.reading = Some((reading, at_ms));
    }

    /// The machine's size, from the latest reading however old: memory and cores
    /// do not change while faber runs, so a stale reading still sizes the plan.
    pub fn size(&self) -> Option<MachineSize> {
        self.reading.as_ref().map(|(r, _)| MachineSize::from(r))
    }

    /// The latest reading while it is still evidence at `now_ms`.
    fn fresh(&self, now_ms: u64) -> Option<&MachineReading> {
        self.reading
            .as_ref()
            .filter(|(_, at)| now_ms.saturating_sub(*at) <= MACHINE_READING_MAX_AGE_MS)
            .map(|(r, _)| r)
    }

    /// Whether the machine holds admission at `now_ms`, and why. Only once a
    /// memory share is declared; never on a reading too old to be evidence.
    pub fn hold(&self, now_ms: u64) -> Option<MachineHold> {
        if !self.share.yields_to_pressure() {
            return None;
        }
        let reading = self.fresh(now_ms)?;
        // Growing swap is named first: it holds even where memory pressure's
        // floor of one job lets work through (#160), so the reason must be it.
        if self.swap.growing() {
            return Some(MachineHold::Swap {
                rise_mib: self.swap.rise_mib(),
            });
        }
        reading
            .pressure
            .filter(|l| l.pauses())
            .map(MachineHold::Pressure)
    }

    /// Put back a window kept from before a restart (§3), so the reserve starts
    /// at the day's peak rather than relearning it.
    pub fn restore_cotenant(&mut self, peak: CoTenantPeak) {
        self.cotenant = peak;
    }

    /// The window as it stands, to write to disk.
    pub fn cotenant_peak(&self) -> &CoTenantPeak {
        &self.cotenant
    }

    /// Store one sample of what the VM uses beyond faber's containers, taken at
    /// `at_ms`. Returns whether the window changed.
    pub fn observe_cotenant(&mut self, mib: u64, at_ms: u64) -> bool {
        self.cotenant_latest = Some((mib, at_ms));
        self.cotenant.observe(at_ms, mib)
    }

    /// The reserve Gate 1 holds back at `now_ms` (§3): `floor_mib`
    /// (`reserve_mib`), or the co-tenants' 24-hour peak if that is larger and the
    /// share is declared.
    pub fn reserve_mib(&self, floor_mib: u64, now_ms: u64) -> u64 {
        cotenant::reserve_in_force(
            floor_mib,
            self.cotenant.peak(now_ms),
            self.share.measures_reserve(),
        )
    }

    /// The co-tenant reserve as the dashboard reads it.
    pub fn cotenant_view(&self, floor_mib: u64, now_ms: u64) -> CoTenantView {
        let reserve_mib = self.reserve_mib(floor_mib, now_ms);
        CoTenantView {
            mode: if self.share.measures_reserve() {
                "measured"
            } else {
                "floor"
            },
            floor_mib,
            latest_mib: self.cotenant_latest.map(|(mib, _)| mib),
            latest_age_s: self
                .cotenant_latest
                .map(|(_, at)| now_ms.saturating_sub(at) / 1000),
            peak_mib: self.cotenant.peak(now_ms),
            history_s: self.cotenant.history_ms(now_ms) / 1000,
            reserve_mib,
            peak_binds: reserve_mib > floor_mib,
        }
    }

    /// See [`plan_mem_mib`].
    pub fn plan_mem_mib(&self, vm_mib: u64) -> u64 {
        plan_mem_mib(vm_mib, self.size(), self.share)
    }

    /// See [`plan_ncpu`].
    pub fn plan_ncpu(&self, vm_ncpu: u64) -> u64 {
        plan_ncpu(vm_ncpu, self.size(), self.share)
    }

    /// See [`encroachment`]. Empty until both the machine and the VM are measured.
    pub fn encroachment(&self, vm_mib: u64, vm_ncpu: u64, macos_mib: u64) -> Vec<String> {
        match self.size() {
            Some(m) => encroachment(vm_mib, vm_ncpu, macos_mib, m, self.share),
            None => Vec::new(),
        }
    }

    /// The split as the dashboard and the tray read it.
    /// `macos_mib` as for [`encroachment`].
    pub fn view(&self, now_ms: u64, vm_mib: u64, vm_ncpu: u64, macos_mib: u64) -> MachineView {
        let size = self.size();
        let latest = self.reading.as_ref().map(|(r, _)| r);
        let fresh = self.fresh(now_ms);
        MachineView {
            share: self.share.as_str(),
            keep_memory_gib: self.share.keep_memory_gib,
            keep_cpus: self.share.keep_cpus,
            yields: self.share.yields_to_pressure(),
            mem_mib: size.map(|m| m.mem_mib),
            cpus: size.map(|m| m.cpus),
            perf_cpus: latest.and_then(|r| r.perf_cpus),
            efficiency_cpus: latest.and_then(|r| r.efficiency_cpus),
            vm_mem_mib: (vm_mib > 0).then_some(vm_mib),
            vm_cpus: (vm_ncpu > 0).then_some(vm_ncpu),
            boundary: size.is_some_and(|m| vm_boundary(vm_mib, vm_ncpu, m)),
            leaves_mib: size
                .filter(|_| vm_mib > 0)
                .map(|m| m.mem_mib.saturating_sub(vm_mib)),
            leaves_cpus: size
                .filter(|_| vm_ncpu > 0)
                .map(|m| m.cpus.saturating_sub(vm_ncpu)),
            fresh: fresh.is_some(),
            pressure: fresh.and_then(|r| r.pressure).map(PressureLevel::as_str),
            psi_some_avg60: fresh.and_then(|r| r.psi_some_avg60),
            swap_used_mib: fresh.and_then(|r| r.swap_used_mib),
            swap_total_mib: fresh.and_then(|r| r.swap_total_mib),
            swap_rise_mib: self.swap.rise_mib(),
            compressor_mib: fresh.and_then(|r| r.compressor_mib),
            hold: self.hold(now_ms).map(|h| h.view().0),
            encroaches: self.encroachment(vm_mib, vm_ncpu, macos_mib),
        }
    }
}

/// The machine and its share (ADR-0021 §1), as `/api/state`'s `budget.machine`.
#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    /// `"declared"`, `"partial"` or `"unstated"`.
    pub share: &'static str,
    pub keep_memory_gib: Option<u64>,
    pub keep_cpus: Option<u64>,
    /// Whether admission yields to the machine's memory pressure and growing swap.
    pub yields: bool,
    /// The machine's memory (MiB) and cores, once read.
    pub mem_mib: Option<u64>,
    pub cpus: Option<u64>,
    pub perf_cpus: Option<u64>,
    pub efficiency_cpus: Option<u64>,
    /// The Docker VM's size, once probed.
    pub vm_mem_mib: Option<u64>,
    pub vm_cpus: Option<u64>,
    /// The daemon runs behind a VM smaller than the machine.
    pub boundary: bool,
    /// What the VM leaves the machine.
    pub leaves_mib: Option<u64>,
    pub leaves_cpus: Option<u64>,
    /// The latest reading is recent enough to be evidence. The fields below are
    /// `null` while it is not.
    pub fresh: bool,
    /// `"normal"`, `"warn"` or `"critical"` — macOS only.
    pub pressure: Option<&'static str>,
    /// `/proc/pressure/memory` `some avg60` — Linux only, and not gated on.
    pub psi_some_avg60: Option<f64>,
    pub swap_used_mib: Option<u64>,
    pub swap_total_mib: Option<u64>,
    /// The largest rise in swap use over the last 5 minutes (MiB).
    pub swap_rise_mib: u64,
    pub compressor_mib: Option<u64>,
    /// The gate holding admission, `"machine-pressure"` or `"swap"`, or `null`.
    pub hold: Option<&'static str>,
    /// The VM is larger than the share leaves CI: each warning, worded as its fix.
    pub encroaches: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024;

    /// An example machine: 32 GiB, 10 cores.
    fn mac() -> MachineSize {
        MachineSize {
            mem_mib: 32 * GIB,
            cpus: 10,
        }
    }

    fn share(mem: Option<u64>, cpus: Option<u64>) -> MachineShare {
        MachineShare {
            keep_memory_gib: mem,
            keep_cpus: cpus,
        }
    }

    fn reading(pressure: Option<PressureLevel>, swap_mib: Option<u64>) -> MachineReading {
        MachineReading {
            mem_mib: 32 * GIB,
            cpus: 10,
            pressure,
            swap_used_mib: swap_mib,
            ..MachineReading::default()
        }
    }

    #[test]
    fn the_sysctl_levels_map_and_only_normal_admits() {
        assert_eq!(PressureLevel::from_sysctl(1), Some(PressureLevel::Normal));
        assert_eq!(PressureLevel::from_sysctl(2), Some(PressureLevel::Warn));
        assert_eq!(PressureLevel::from_sysctl(4), Some(PressureLevel::Critical));
        assert_eq!(PressureLevel::from_sysctl(0), None);
        assert_eq!(PressureLevel::from_sysctl(3), None);
        assert!(!PressureLevel::Normal.pauses());
        assert!(PressureLevel::Warn.pauses());
        assert!(PressureLevel::Critical.pauses());
    }

    #[test]
    fn the_plan_is_the_smaller_of_the_vm_and_what_the_share_leaves() {
        let example = share(Some(16), Some(2));
        // A 24 GiB / 10 vCPU VM, say: planned at 16 GiB / 8 cores.
        assert_eq!(plan_mem_mib(24 * GIB, Some(mac()), example), 16 * GIB);
        assert_eq!(plan_ncpu(10, Some(mac()), example), 8);
        // A VM already inside the share plans as it always did.
        assert_eq!(plan_mem_mib(12 * GIB, Some(mac()), example), 12 * GIB);
        assert_eq!(plan_ncpu(6, Some(mac()), example), 6);
    }

    #[test]
    fn an_unstated_zero_or_unmeasured_share_plans_as_before() {
        for s in [share(None, None), share(Some(0), Some(0))] {
            assert_eq!(plan_mem_mib(24 * GIB, Some(mac()), s), 24 * GIB);
            assert_eq!(plan_ncpu(10, Some(mac()), s), 10);
        }
        let example = share(Some(16), Some(2));
        assert_eq!(plan_mem_mib(24 * GIB, None, example), 24 * GIB);
        assert_eq!(plan_ncpu(10, None, example), 10);
        // An unprobed VM stays unprobed: zero is "not measured", not a budget.
        assert_eq!(plan_mem_mib(0, Some(mac()), example), 0);
    }

    #[test]
    fn a_zero_share_changes_nothing_where_the_machine_reads_smaller_than_the_daemon() {
        // A Linux daemon reporting 16 cores to a faber confined to 8 by affinity
        // or a cgroup quota. The first settings save writes 0 there too, and 0 is
        // no share: no tighter plan, and no resize advice.
        let confined = MachineSize {
            mem_mib: 32 * GIB,
            cpus: 8,
        };
        let zero = share(Some(0), Some(0));
        assert_eq!(plan_ncpu(16, Some(confined), zero), 16);
        assert_eq!(plan_mem_mib(48 * GIB, Some(confined), zero), 48 * GIB);
        assert!(encroachment(48 * GIB, 16, 0, confined, zero).is_empty());
    }

    #[test]
    fn a_share_as_large_as_the_machine_is_an_error_and_is_ignored() {
        let whole = share(Some(32), Some(10));
        let errors = whole.errors(mac());
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].starts_with("host.keep_memory_gib (32 GiB) is not smaller"));
        assert!(errors[1].starts_with("host.keep_cpus (10) is not smaller"));
        // Ignored rather than planned against zero, which would park everything.
        assert_eq!(plan_mem_mib(24 * GIB, Some(mac()), whole), 24 * GIB);
        assert_eq!(plan_ncpu(10, Some(mac()), whole), 10);
        assert!(share(Some(16), Some(2)).errors(mac()).is_empty());
        assert!(share(Some(0), Some(0)).errors(mac()).is_empty());
        assert!(share(None, None).errors(mac()).is_empty());
    }

    #[test]
    fn an_encroaching_vm_is_told_the_fix_and_the_reason() {
        let warnings = encroachment(24 * GIB, 10, 0, mac(), share(Some(16), Some(2)));
        assert_eq!(
            warnings,
            [
                "the Docker VM holds 24 GiB of this machine's 32 GiB, more than the 16 GiB that \
                 host.keep_memory_gib = 16 leaves CI: set [linux_vm] memory_mib to 16384 or \
                 less. The Linux VM never gives memory back to the machine, so its size is the \
                 machine's only memory reservation, and macOS memory pressure will not clear \
                 while it is larger: faber runs one Docker job at a time meanwhile (ADR-0021, \
                 ADR-0026)",
                "the Docker VM has 10 of this machine's 10 cores, more than the 8 that \
                 host.keep_cpus = 2 leaves CI: set [linux_vm] cpus to 8 or less. faber plans \
                 within 8 meanwhile (ADR-0021)",
            ]
        );
        // A VM resized to what the share leaves, 16 GiB / 8 vCPU, fits: the guest
        // sees a little less than it was given.
        assert!(encroachment(15 * GIB + 300, 8, 0, mac(), share(Some(16), Some(2))).is_empty());
        // No share, nothing to encroach on.
        assert!(encroachment(24 * GIB, 10, 0, mac(), share(None, None)).is_empty());
    }

    #[test]
    fn the_linux_vm_and_the_macos_cap_are_counted_against_the_share_together() {
        let example = share(Some(16), Some(2));
        // An example layout: 8 GiB of Linux VM beside two 4 GiB macOS VMs fits.
        assert!(encroachment(8 * GIB, 6, 8 * GIB, mac(), example).is_empty());
        // 12 GiB does not: the fix is the Linux VM's key, sized to what is left.
        let over = encroachment(12 * GIB, 6, 8 * GIB, mac(), example);
        assert_eq!(over.len(), 1);
        assert!(
            over[0].starts_with(
                "faber's VMs hold 20 GiB (the Linux VM's 12 and up to 8 for macOS VMs) of \
                 this machine's 32 GiB, more than the 16 GiB"
            ),
            "{over:?}"
        );
        assert!(
            over[0].contains("set [linux_vm] memory_mib to 8192 or less"),
            "{over:?}"
        );
        // A share the macOS VMs alone fill is not fixed by the Linux VM's size.
        let tight = encroachment(2 * GIB, 6, 8 * GIB, mac(), share(Some(28), None));
        assert!(tight[0].contains("lower host.keep_memory_gib"), "{tight:?}");
        // An unmeasured Linux VM is no evidence of an encroachment.
        assert!(encroachment(0, 0, 8 * GIB, mac(), share(Some(28), None)).is_empty());
    }

    #[test]
    fn the_boundary_is_decided_by_the_comparison_not_the_platform() {
        assert!(vm_boundary(24 * GIB, 8, mac()));
        assert!(vm_boundary(32 * GIB, 8, mac()));
        // A native daemon sees the machine itself.
        assert!(!vm_boundary(32 * GIB, 10, mac()));
        // An unprobed daemon is no evidence of a VM.
        assert!(!vm_boundary(0, 0, mac()));
    }

    #[test]
    fn an_unstated_share_warns_with_the_measured_split() {
        let w = unstated_warnings(share(None, None), Some(mac()), 24 * GIB, 8);
        assert_eq!(w.len(), 2);
        assert!(
            w[0].contains("The Docker VM holds 24 of this machine's 32 GiB, leaving it 8."),
            "{}",
            w[0]
        );
        assert!(w[0].starts_with("host.keep_memory_gib is unstated"));
        assert!(
            w[1].contains("The Docker VM has 8 of this machine's 10 cores, leaving it 2."),
            "{}",
            w[1]
        );
        // Each key is its own warning, and a stated one — 0 included — is silent.
        assert_eq!(
            unstated_warnings(share(Some(0), None), Some(mac()), 24 * GIB, 8).len(),
            1
        );
        assert!(unstated_warnings(share(Some(0), Some(0)), Some(mac()), 24 * GIB, 8).is_empty());
        // A host with no Docker VM measured still hears it, with nothing to split.
        let unmeasured = unstated_warnings(share(None, None), Some(mac()), 0, 0);
        assert!(
            unmeasured[0].contains("This machine has 32 GiB."),
            "{}",
            unmeasured[0]
        );
    }

    #[test]
    fn swap_growth_of_a_gib_in_five_minutes_pauses_and_five_flat_minutes_resume() {
        let mut w = SwapWindow::default();
        let t0 = 1_000_000;
        // Some swap in use before a heavy job starts, then climbing.
        w.observe(t0, 2_000);
        w.observe(t0 + 60_000, 2_500);
        assert!(!w.growing());
        w.observe(t0 + 120_000, 3_100);
        assert!(w.growing(), "1.1 GiB inside the window");
        assert_eq!(w.rise_mib(), 1_100);
        // Flat from here: still paused while the rise is inside the window…
        for minute in 3..7 {
            w.observe(t0 + minute * 60_000, 3_100);
            assert!(w.growing(), "minute {minute}");
        }
        // …and resumed once five flat minutes have pushed it out.
        w.observe(t0 + 7 * 60_000 + 1, 3_100);
        assert!(!w.growing());
        assert_eq!(w.rise_mib(), 0);
    }

    #[test]
    fn swap_noise_is_flat_and_a_slow_rise_holds_the_pause() {
        let mut w = SwapWindow::default();
        let t0 = 1_000_000;
        for (i, mib) in [4_000, 4_100, 4_050, 4_150, 4_100].iter().enumerate() {
            w.observe(t0 + i as u64 * 60_000, *mib);
        }
        assert!(!w.growing(), "a few MiB of paging is not growth");
        // A falling reading never counts as growth.
        w.observe(t0 + 5 * 60_000, 1_000);
        assert!(!w.growing());
        // A dip then a climb does.
        w.observe(t0 + 6 * 60_000, 2_100);
        assert!(w.growing());
        // A rise between flat and growth holds the state it is in.
        let mut held = SwapWindow::default();
        held.observe(t0, 0);
        held.observe(t0 + 60_000, 1_024);
        assert!(held.growing());
        for minute in 2..=7 {
            held.observe(t0 + minute * 60_000, 1_024 + 100 * minute);
        }
        assert!(held.growing(), "still rising 500 MiB per window: not flat");
    }

    #[test]
    fn the_machine_holds_only_once_a_memory_share_is_declared() {
        let now = 10_000_000;
        let mut m = MachineState::default();
        m.observe(Some(reading(Some(PressureLevel::Warn), Some(0))), now);
        // Unstated, and stated as 0: the behaviour from before — no gate.
        assert_eq!(m.hold(now), None);
        m.set_share(share(Some(0), Some(2)));
        assert_eq!(m.hold(now), None);
        m.set_share(share(Some(16), None));
        assert_eq!(
            m.hold(now),
            Some(MachineHold::Pressure(PressureLevel::Warn))
        );
        m.observe(Some(reading(Some(PressureLevel::Critical), Some(0))), now);
        assert_eq!(
            m.hold(now),
            Some(MachineHold::Pressure(PressureLevel::Critical))
        );
        m.observe(Some(reading(Some(PressureLevel::Normal), Some(0))), now);
        assert_eq!(m.hold(now), None);
        // Linux reports no level: the pressure gate is inert there.
        m.observe(Some(reading(None, Some(0))), now);
        assert_eq!(m.hold(now), None);
    }

    #[test]
    fn growing_swap_holds_ahead_of_pressure_and_a_stale_reading_holds_nothing() {
        let t0 = 10_000_000;
        let mut m = MachineState::default();
        m.set_share(share(Some(16), Some(2)));
        m.observe(Some(reading(Some(PressureLevel::Normal), Some(0))), t0);
        m.observe(
            Some(reading(Some(PressureLevel::Normal), Some(2 * GIB))),
            t0 + 60_000,
        );
        assert_eq!(
            m.hold(t0 + 60_000),
            Some(MachineHold::Swap { rise_mib: 2 * GIB })
        );
        // Swap is named first when both hold: it has no floor (#160).
        m.observe(
            Some(reading(Some(PressureLevel::Warn), Some(2 * GIB))),
            t0 + 70_000,
        );
        assert_eq!(
            m.hold(t0 + 70_000),
            Some(MachineHold::Swap { rise_mib: 2 * GIB })
        );
        // A failed reading keeps the last; one too old to be evidence opens the gate.
        m.observe(None, t0 + 80_000);
        assert!(m.hold(t0 + 80_000).is_some());
        assert_eq!(m.hold(t0 + 70_000 + MACHINE_READING_MAX_AGE_MS + 1), None);
        // The plan keeps the machine's size from a stale reading.
        assert_eq!(m.plan_mem_mib(24 * GIB), 16 * GIB);
    }

    #[test]
    fn a_hold_names_its_gate() {
        assert_eq!(
            MachineHold::Pressure(PressureLevel::Warn).view(),
            (
                "machine-pressure",
                "this machine's memory pressure is warn — no more than one job runs until it is \
                 normal"
                    .to_string()
            )
        );
        assert_eq!(
            MachineHold::Swap { rise_mib: 1_536 }.view(),
            (
                "swap",
                "swap grew 1.5 GiB in the last 5 minutes — nothing new starts until it has been \
                 flat for 5"
                    .to_string()
            )
        );
    }

    #[test]
    fn the_co_tenant_peak_is_held_back_only_once_a_memory_share_is_declared() {
        let now = 10_000_000;
        let mut m = MachineState::default();
        // Measured in every mode, so the dashboard shows it before it binds.
        assert!(m.observe_cotenant(8 * GIB, now));
        assert_eq!(m.cotenant_view(5 * GIB, now).peak_mib, Some(8 * GIB));
        // Unstated, and stated as 0: `reserve_mib` alone, as before §3.
        assert_eq!(m.reserve_mib(5 * GIB, now), 5 * GIB);
        m.set_share(share(Some(0), Some(2)));
        assert_eq!(m.reserve_mib(5 * GIB, now), 5 * GIB);
        assert_eq!(m.cotenant_view(5 * GIB, now).mode, "floor");
        // Declared: the larger of the floor and the peak.
        m.set_share(share(Some(16), None));
        assert_eq!(m.reserve_mib(5 * GIB, now), 8 * GIB);
        assert_eq!(m.reserve_mib(10 * GIB, now), 10 * GIB);
        let v = m.cotenant_view(5 * GIB, now + 60_000);
        assert_eq!(v.mode, "measured");
        assert_eq!(v.latest_mib, Some(8 * GIB));
        assert_eq!(v.latest_age_s, Some(60));
        assert_eq!(v.reserve_mib, 8 * GIB);
        assert!(v.peak_binds);
        // A restart that kept the window starts at the day's peak, and one that
        // lost it starts at the floor — never below it.
        let kept = m.cotenant_peak().clone();
        let mut restarted = MachineState::default();
        restarted.set_share(share(Some(16), None));
        assert_eq!(restarted.reserve_mib(5 * GIB, now), 5 * GIB);
        restarted.restore_cotenant(kept);
        assert_eq!(restarted.reserve_mib(5 * GIB, now), 8 * GIB);
    }

    #[test]
    fn the_cotenant_view_contract_field_names() {
        let v = serde_json::to_value(MachineState::default().cotenant_view(5 * GIB, 0)).unwrap();
        for key in [
            "mode",
            "floor_mib",
            "latest_mib",
            "latest_age_s",
            "peak_mib",
            "history_s",
            "reserve_mib",
            "peak_binds",
        ] {
            assert!(v.get(key).is_some(), "{key}");
        }
    }

    #[test]
    fn the_view_contract_field_names() {
        // The dashboard and the menu-bar app read these by name.
        let now = 10_000_000;
        let mut m = MachineState::default();
        m.set_share(share(Some(16), None));
        m.observe(
            Some(MachineReading {
                perf_cpus: Some(4),
                efficiency_cpus: Some(6),
                compressor_mib: Some(3 * GIB),
                swap_total_mib: Some(6 * GIB),
                ..reading(Some(PressureLevel::Warn), Some(4 * GIB))
            }),
            now,
        );
        let v = serde_json::to_value(m.view(now, 24 * GIB, 8, 0)).unwrap();
        assert_eq!(v["share"], "partial");
        assert_eq!(v["keep_memory_gib"], 16);
        assert_eq!(v["keep_cpus"], serde_json::Value::Null);
        assert_eq!(v["yields"], true);
        assert_eq!(v["mem_mib"], 32 * GIB);
        assert_eq!(v["cpus"], 10);
        assert_eq!(v["perf_cpus"], 4);
        assert_eq!(v["efficiency_cpus"], 6);
        assert_eq!(v["vm_mem_mib"], 24 * GIB);
        assert_eq!(v["vm_cpus"], 8);
        assert_eq!(v["boundary"], true);
        assert_eq!(v["leaves_mib"], 8 * GIB);
        assert_eq!(v["leaves_cpus"], 2);
        assert_eq!(v["fresh"], true);
        assert_eq!(v["pressure"], "warn");
        assert_eq!(v["psi_some_avg60"], serde_json::Value::Null);
        assert_eq!(v["swap_used_mib"], 4 * GIB);
        assert_eq!(v["swap_total_mib"], 6 * GIB);
        assert_eq!(v["swap_rise_mib"], 0);
        assert_eq!(v["compressor_mib"], 3 * GIB);
        assert_eq!(v["hold"], "machine-pressure");
        assert_eq!(v["encroaches"].as_array().unwrap().len(), 1);
    }
}
