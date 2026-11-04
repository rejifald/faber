//! The Linux VM that serves faber's Docker (ADR-0026, "The Linux VM runs Docker
//! Engine under the same launcher"): "the engine".
//!
//! One long-lived VM per host, in slot [`LINUX_SLOT`] under the claim
//! [`ENGINE_CLAIM`], booted from the root-owned image `linux` that setup builds.
//! The supervisor brings it up the way it brings up a macOS job's VM, and faber
//! walks it through the same gates:
//!
//! 1. **`start`** (kind `linux`, no tools disk), or take back the VM a previous
//!    faber left running: the supervisor's `list` shows it in its real state.
//! 2. **Wait for `ready`**: on Linux, the guest's Docker relay answers.
//! 3. **`probe`, then the wall verdict** from the anchor's own rule counters
//!    ([`WallProof`]), never from the guest. A breach or an unreadable verdict
//!    stops the VM: Docker is never served from a VM the wall has not held.
//! 4. **`begin`**, with no JIT config: the supervisor opens the host Docker
//!    socket ([`crate::platform::vm_docker_socket`]). It does not exist before.
//! 5. **Watch** it with `list` every [`EngineTimeouts::watch`]. Any state but
//!    `in_service` is "not serving", and the driver brings it up again.
//!
//! What the driver learns goes to an [`EngineSink`]: the limiter, which treats an
//! engine that is not in service as Docker unreachable (Gate 0) with the reason
//! as its fault, and counts the VM's memory inside the machine share (ADR-0021).
//!
//! faber never deletes the engine's bundle: it holds the only copy of the
//! local-only runner image (AGENTS.md). `stop` stops the VM; the bundle stays.
//! Revival (ADR-0007, retargeted by ADR-0026 "Size and recovery") is a `stop`
//! from [`stop_for_revival`]; this driver then sees the VM leave service and
//! starts it again.

use std::time::Duration;

use tokio::time::{sleep, Instant};
use tracing::{info, warn};

use super::job::{Disarm, SupervisorError, VmSupervisor, WallProof, WallVerdict};
use super::proto::{
    ClaimId, Cpus, ErrorCode, ImageName, MemoryMib, Slot, Start, StopReason, VmKind, VmRecord,
    VmState,
};

/// The engine's launcher slot. Slot 15 is setup's build VM
/// ([`crate::setup::linux_image::BUILD_SLOT`]); macOS jobs lease neither.
pub const LINUX_SLOT: u8 = 14;

/// The engine's claim: fixed, since there is one engine per host.
pub const ENGINE_CLAIM: &str = "linux-engine";

/// The engine's image in the root-owned image store, as setup names it.
pub const ENGINE_IMAGE: &str = "linux";

/// vCPUs for the engine when `[linux_vm] cpus` does not say.
pub const DEFAULT_LINUX_VM_CPUS: u8 = 6;

/// Memory for the engine in MiB when `[linux_vm] memory_mib` does not say. With
/// a share of 32 GiB, say, two macOS VMs at 8 GiB leave 16 for it (ADR-0026,
/// "Size and recovery").
pub const DEFAULT_LINUX_VM_MEMORY_MIB: u32 = 16384;

/// How long each step of the engine's life may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineTimeouts {
    /// `start` to `ready`: building the VM, booting it and the relay answering.
    pub boot: Duration,
    /// The wall proof.
    pub probe: Duration,
    /// A stop, until the supervisor reports `stopped`. The launcher asks the
    /// guest first and forces it after 60 s.
    pub stop: Duration,
    /// How often state is polled while the engine is coming up or going down.
    pub poll: Duration,
    /// How often an engine in service is checked.
    pub watch: Duration,
    /// `list` failures in a row, while in service, before the engine counts as not
    /// serving. A supervisor that restarts onto a new release misses one or two.
    pub watch_misses: u32,
    /// The wait after a failed bring-up, doubling to `retry_max`: a VM that fails
    /// closed at the wall is not booted again every few seconds.
    pub retry_min: Duration,
    pub retry_max: Duration,
}

impl Default for EngineTimeouts {
    fn default() -> Self {
        Self {
            boot: Duration::from_secs(600),
            probe: Duration::from_secs(60),
            stop: Duration::from_secs(120),
            poll: Duration::from_secs(2),
            watch: Duration::from_secs(15),
            watch_misses: 4,
            retry_min: Duration::from_secs(60),
            retry_max: Duration::from_secs(1800),
        }
    }
}

/// The engine's size and timings, already in the protocol's own types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    pub cpus: Cpus,
    pub memory_mib: MemoryMib,
    /// `[linux_vm] disk_max_gib`: the ceiling the disk may grow to (ADR-0061 §1).
    pub disk_max_gib: u64,
    pub timeouts: EngineTimeouts,
}

impl EngineConfig {
    /// From `[linux_vm]`. A value outside the protocol's range is `faber
    /// check-config`'s to reject; a daemon booting on one warns and uses the
    /// default instead (AGENTS.md: never refuse to boot on a file).
    pub fn from_file(f: &crate::config::LinuxVmFile) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let default_cpus = Cpus::try_from(DEFAULT_LINUX_VM_CPUS).expect("in range");
        let default_mem = MemoryMib::try_from(DEFAULT_LINUX_VM_MEMORY_MIB).expect("in range");
        let cpus = match f.cpus.map(Cpus::try_from) {
            None => default_cpus,
            Some(Ok(c)) => c,
            Some(Err(_)) => {
                warnings.push(format!(
                    "linux_vm.cpus must be 1 to 64 — using {DEFAULT_LINUX_VM_CPUS}; run `faber \
                     check-config` to fix the file"
                ));
                default_cpus
            }
        };
        let memory_mib = match f.memory_mib.map(MemoryMib::try_from) {
            None => default_mem,
            Some(Ok(m)) => m,
            Some(Err(_)) => {
                warnings.push(format!(
                    "linux_vm.memory_mib must be 1024 to 262144 — using \
                     {DEFAULT_LINUX_VM_MEMORY_MIB}; run `faber check-config` to fix the file"
                ));
                default_mem
            }
        };
        use super::grow::{BUILT_GIB, DEFAULT_CEILING_GIB, MAX_CEILING_GIB};
        let disk_max_gib = match f.disk_max_gib {
            None => DEFAULT_CEILING_GIB,
            Some(g) if (BUILT_GIB..=MAX_CEILING_GIB).contains(&g) => g,
            Some(_) => {
                warnings.push(format!(
                    "linux_vm.disk_max_gib must be {BUILT_GIB} to {MAX_CEILING_GIB} — using \
                     {BUILT_GIB}, which grows nothing; run `faber check-config` to fix the file"
                ));
                BUILT_GIB
            }
        };
        (
            Self {
                cpus,
                memory_mib,
                disk_max_gib,
                timeouts: EngineTimeouts::default(),
            },
            warnings,
        )
    }
}

/// Where the engine is, in faber's own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    /// `start` sent, or about to be.
    Starting,
    /// The VM runs; its Docker relay has not answered yet.
    Booting,
    /// The probe is sent and the wall verdict is awaited.
    ProvingWall,
    /// The host Docker socket is bridged: Docker is served.
    InService,
    /// Bridged, but the kept runner image is still being put into the VM's Docker
    /// ([`super::runner_image`]): jobs wait for it instead of failing on a pull.
    LoadingImage,
    /// A stop was asked for, by faber or the supervisor.
    Stopping,
    /// Not serving, and why. The driver tries again after its back-off.
    Down(String),
}

impl EngineStatus {
    pub fn serving(&self) -> bool {
        matches!(self, Self::InService)
    }

    /// Why Docker is unreachable because of the engine, or `None` while it is in
    /// service. The limiter's Gate 0 refuses on it, and it is the Docker fault's
    /// line.
    pub fn fault(&self) -> Option<String> {
        let what = match self {
            Self::InService => return None,
            Self::Starting => "starting".to_string(),
            Self::Booting => "booting".to_string(),
            Self::ProvingWall => "waiting for the wall to hold its probe".to_string(),
            Self::LoadingImage => "loading the kept runner image".to_string(),
            Self::Stopping => "stopping".to_string(),
            Self::Down(why) => format!("not serving: {why}"),
        };
        Some(format!("faber's Linux VM is {what} (ADR-0026)"))
    }
}

/// Where the driver reports to.
pub trait EngineSink: Send + Sync {
    /// The engine's latest status, and the memory its VM holds now (MiB; 0 when
    /// no VM runs).
    fn engine(&self, status: &EngineStatus, held_mib: u64);
}

impl EngineSink for std::sync::Arc<crate::limiter::Limiter> {
    fn engine(&self, status: &EngineStatus, held_mib: u64) {
        self.set_engine_fault(status.fault());
        self.set_linux_vm_mib(held_mib);
    }
}

fn claim() -> ClaimId {
    ClaimId::try_from(ENGINE_CLAIM).expect("a valid claim id")
}

fn slot() -> Slot {
    Slot::try_from(LINUX_SLOT).expect("a valid slot")
}

/// What polling `list` for the engine came to.
enum Seen {
    Reached(VmRecord),
    Gone,
    Stopped(Option<StopReason>),
    Unmanaged,
    Stopping,
    TimedOut,
    Failed(SupervisorError),
}

impl Seen {
    fn why(&self) -> String {
        match self {
            Seen::Reached(v) => format!("it is {:?}", v.state),
            Seen::Gone => "the supervisor no longer lists it".into(),
            Seen::Stopped(Some(r)) => format!("it stopped ({r:?})"),
            Seen::Stopped(None) => "it stopped".into(),
            Seen::Unmanaged => UNMANAGED.into(),
            Seen::Stopping => "it is stopping".into(),
            Seen::TimedOut => "it did not get there in time".into(),
            Seen::Failed(e) => format!("asking the supervisor: {e}"),
        }
    }
}

const UNMANAGED: &str = "an older launcher runs it that this supervisor cannot speak to; it is \
                         left alone until it exits";

/// A bring-up that did not reach service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub why: String,
    /// Whether a VM may still hold its memory: one faber could not confirm stopped.
    pub held: bool,
}

/// The engine driver: one VM, kept in service.
pub struct Engine<S, W, R> {
    pub supervisor: S,
    pub wall: W,
    pub sink: R,
    pub cfg: EngineConfig,
    /// The grow policy's shared state (ADR-0061): the size a start asks for, and
    /// where a grow restart is told the VM came into service or did not.
    pub grow: std::sync::Arc<super::grow::Grow>,
}

impl<S: VmSupervisor, W: WallProof, R: EngineSink> Engine<S, W, R> {
    fn say(&self, status: EngineStatus, held: bool) {
        let mib = if held {
            u64::from(self.cfg.memory_mib.get())
        } else {
            0
        };
        self.sink.engine(&status, mib);
    }

    async fn find(&self) -> Result<Option<VmRecord>, SupervisorError> {
        let claim = claim();
        Ok(self
            .supervisor
            .list()
            .await?
            .into_iter()
            .find(|v| v.claim == claim))
    }

    /// Poll until the engine is in a state `until` accepts; `stopping_ok` waits
    /// through `stopping` instead of ending there.
    async fn wait_for(
        &self,
        until: fn(VmState) -> bool,
        limit: Duration,
        stopping_ok: bool,
    ) -> Seen {
        let deadline = Instant::now() + limit;
        loop {
            match self.find().await {
                Err(e) => return Seen::Failed(e),
                Ok(None) => return Seen::Gone,
                Ok(Some(v)) => {
                    if until(v.state) {
                        return Seen::Reached(v);
                    }
                    match v.state {
                        VmState::Starting => self.say(EngineStatus::Starting, true),
                        VmState::Booting => self.say(EngineStatus::Booting, true),
                        VmState::Stopped => return Seen::Stopped(v.stop_reason),
                        VmState::Unmanaged => return Seen::Unmanaged,
                        VmState::Stopping if stopping_ok => {}
                        VmState::Stopping => return Seen::Stopping,
                        _ => {}
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Seen::TimedOut;
            }
            sleep(left.min(self.cfg.timeouts.poll)).await;
        }
    }

    /// Stop the engine and wait for `stopped`. `true` when it is confirmed gone.
    async fn stop_and_wait(&self) -> bool {
        self.say(EngineStatus::Stopping, true);
        info!("engine: asking the VM supervisor to stop faber's Linux VM (a failed step)");
        match self.supervisor.stop(&claim()).await {
            Ok(()) => {}
            Err(SupervisorError::Refused(ErrorCode::UnknownClaim)) => return true,
            Err(e) => {
                warn!("engine: stop not confirmed ({e})");
                return false;
            }
        }
        matches!(
            self.wait_for(|s| s == VmState::Stopped, self.cfg.timeouts.stop, true)
                .await,
            Seen::Reached(_) | Seen::Gone
        )
    }

    /// Stop the VM on a failed step, and say whether it is gone.
    async fn fail(&self, why: String) -> Failure {
        let gone = self.stop_and_wait().await;
        Failure {
            why: if gone {
                why
            } else {
                format!("{why}; it was not confirmed stopped")
            },
            held: !gone,
        }
    }

    /// Bring the engine into service from wherever it is: absent, stopped, mid-boot
    /// or already serving (a VM a previous faber left running). Every failure after
    /// a `start` was sent stops the VM, so a VM the wall has not held never serves.
    pub async fn bring_up(&self) -> Result<(), Failure> {
        let t = self.cfg.timeouts;
        let claim = claim();
        let mut vm = self.find().await.map_err(|e| Failure {
            why: format!("listing VMs: {e}"),
            held: true,
        })?;

        // A stop in flight (a revival's, or the supervisor's own) is waited out,
        // then the VM is started afresh.
        if vm.as_ref().is_some_and(|v| v.state == VmState::Stopping) {
            self.say(EngineStatus::Stopping, true);
            match self.wait_for(|s| s == VmState::Stopped, t.stop, true).await {
                Seen::Reached(_) | Seen::Gone => vm = None,
                other => {
                    return Err(Failure {
                        why: format!("it did not finish stopping: {}", other.why()),
                        held: true,
                    })
                }
            }
        }

        match vm.as_ref().map(|v| v.state) {
            Some(VmState::InService) => {
                self.say(EngineStatus::InService, true);
                return Ok(());
            }
            Some(VmState::Unmanaged) => {
                return Err(Failure {
                    why: UNMANAGED.into(),
                    held: true,
                })
            }
            Some(VmState::JobFinished) => {
                return Err(self
                    .fail("it reports job_finished, which a Linux VM never does".into())
                    .await)
            }
            None | Some(VmState::Stopped) => {
                self.say(EngineStatus::Starting, true);
                // ADR-0061 §3 step 1: every start takes the pending grow's size,
                // whatever made it start. Anything the policy could not turn into
                // a valid size is left out, and the disk keeps the size it has.
                let grow_to = self
                    .grow
                    .start_size()
                    .and_then(|b| super::proto::DiskBytes::try_from(b).ok());
                let request = |disk_bytes| Start {
                    slot: slot(),
                    kind: VmKind::Linux,
                    claim: claim.clone(),
                    image: ImageName::try_from(ENGINE_IMAGE).expect("a valid image name"),
                    tools: None,
                    cpus: self.cfg.cpus,
                    memory_mib: self.cfg.memory_mib,
                    disk_bytes,
                };
                let mut grown = grow_to;
                let mut started = self.supervisor.start(request(grow_to)).await;
                if let (Some(to), Err(SupervisorError::Refused(code))) = (grow_to, &started) {
                    if !matches!(code, ErrorCode::SlotBusy | ErrorCode::ClaimExists) {
                        // The supervisor refused a start that asked to grow: try
                        // once at the size the disk has. Only if that works is the
                        // grow to blame; if it fails too, the failure is unrelated
                        // and the retry's error is the one reported.
                        grown = None;
                        let retry = self.supervisor.start(request(None)).await;
                        if retry.is_ok() {
                            warn!(
                                "engine: the VM supervisor refused to grow the Linux VM's \
                                 disk to {} bytes; started it at its current size",
                                to.get()
                            );
                            self.grow.start_refused(to.get());
                        }
                        started = retry;
                    }
                }
                match started {
                    Ok(_) => match grown {
                        Some(to) => info!(
                            "engine: started faber's Linux VM in slot {LINUX_SLOT}, its disk \
                             grown to {} GiB",
                            to.get() / super::grow::GIB
                        ),
                        _ => info!("engine: started faber's Linux VM in slot {LINUX_SLOT}"),
                    },
                    // Definite refusals: nothing was started.
                    Err(SupervisorError::Refused(ErrorCode::SlotBusy)) => {
                        return Err(Failure {
                            why: format!("slot {LINUX_SLOT} is held by a VM that is not faber's"),
                            held: false,
                        })
                    }
                    Err(SupervisorError::Refused(code)) => {
                        return Err(Failure {
                            why: format!("the VM supervisor refused to start it: {code:?}"),
                            held: false,
                        })
                    }
                    Err(e @ (SupervisorError::Unreachable | SupervisorError::Misconfigured)) => {
                        return Err(Failure {
                            why: format!("starting it: {e}"),
                            held: false,
                        })
                    }
                    // May have been seen: stop whatever it started.
                    Err(e) => return Err(self.fail(format!("starting it: {e}")).await),
                }
            }
            // Starting, booting, ready, probe_sent: carry on from there.
            Some(_) => {}
        }

        // Step 2: the Docker relay answers.
        let ready = match self
            .wait_for(
                |s| matches!(s, VmState::Ready | VmState::ProbeSent),
                t.boot,
                false,
            )
            .await
        {
            Seen::Reached(v) => v,
            other => {
                return Err(self
                    .fail(format!("it did not become ready: {}", other.why()))
                    .await)
            }
        };

        // Step 3: the wall. The proof's baseline must predate the probe, so a
        // VM found already `probe_sent` (a probe an earlier faber sent) cannot
        // be proven: it is stopped, and the driver starts it afresh.
        self.say(EngineStatus::ProvingWall, true);
        if ready.state != VmState::Ready {
            return Err(self
                .fail("it was found with a probe this faber did not see sent".into())
                .await);
        }
        // Disarmed on every way out of the proof.
        let disarm = Disarm::new(&self.wall, &claim);
        self.wall.arm(slot(), &claim).await;
        match self.supervisor.probe(&claim).await {
            Ok(r) if r.state == VmState::ProbeSent => {}
            Ok(r) => {
                return Err(self
                    .fail(format!("probe left it in state {:?}", r.state))
                    .await)
            }
            Err(e) => return Err(self.fail(format!("sending the wall probe: {e}")).await),
        }
        let deadline = Instant::now() + t.probe;
        loop {
            match self.wall.verdict(slot(), &claim).await {
                WallVerdict::Held => break,
                WallVerdict::Breached => {
                    return Err(self
                        .fail(
                            "the wall did not stop its probe — Docker is not served from it".into(),
                        )
                        .await)
                }
                WallVerdict::Unreadable => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(self
                    .fail(format!(
                        "the wall verdict could not be read within {:?} — failing closed",
                        t.probe
                    ))
                    .await);
            }
            sleep(left.min(t.poll)).await;
        }
        drop(disarm);

        // Step 4: bridge the Docker socket.
        match self.supervisor.begin(&claim, None).await {
            Ok(r) if r.state == VmState::InService => {}
            Ok(r) => {
                return Err(self
                    .fail(format!("begin left it in state {:?}", r.state))
                    .await)
            }
            Err(e) => return Err(self.fail(format!("bridging Docker: {e}")).await),
        }
        info!("engine: faber's Linux VM is in service");
        self.say(EngineStatus::InService, true);
        Ok(())
    }

    /// Step 5: watch an engine in service until it is not, and say why.
    pub async fn watch(&self) -> String {
        let mut misses = 0;
        loop {
            sleep(self.cfg.timeouts.watch).await;
            match self.find().await {
                Ok(Some(v)) if v.state == VmState::InService => misses = 0,
                Ok(Some(v)) => {
                    return match v.stop_reason {
                        Some(r) => format!("it left service ({:?}, {r:?})", v.state),
                        None => format!("it left service ({:?})", v.state),
                    }
                }
                Ok(None) => return "the supervisor no longer lists it".into(),
                Err(e) => {
                    misses += 1;
                    if misses >= self.cfg.timeouts.watch_misses {
                        return format!(
                            "the VM supervisor did not answer {misses} checks in a row: {e}"
                        );
                    }
                }
            }
        }
    }

    /// Keep the engine in service for the life of the process.
    pub async fn run(self) {
        let t = self.cfg.timeouts;
        let mut backoff = t.retry_min;
        loop {
            match self.bring_up().await {
                Ok(()) => {
                    self.grow.in_service();
                    backoff = t.retry_min;
                    let why = self.watch().await;
                    warn!("engine: faber's Linux VM is not serving Docker: {why}");
                    self.say(EngineStatus::Down(why), true);
                }
                Err(f) => {
                    self.grow.bring_up_failed();
                    warn!(
                        "engine: faber's Linux VM did not come into service: {} — trying again \
                         in {backoff:?}",
                        f.why
                    );
                    self.say(EngineStatus::Down(f.why), f.held);
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(t.retry_max);
                }
            }
        }
    }
}

/// Revival's half (ADR-0007 as ADR-0026 retargets it): stop the engine and wait
/// until the supervisor reports it stopped. Starting it again is the driver's,
/// which sees it leave service. An engine the supervisor does not know is already
/// stopped.
pub async fn stop_for_revival<S: VmSupervisor>(
    supervisor: &S,
    timeouts: &EngineTimeouts,
) -> anyhow::Result<()> {
    let claim = claim();
    info!(
        "engine: asking the VM supervisor to stop faber's Linux VM (revival, `faber engine \
         stop` or `[linux_vm] enabled = false`)"
    );
    match supervisor.stop(&claim).await {
        Ok(()) => {}
        Err(SupervisorError::Refused(ErrorCode::UnknownClaim)) => return Ok(()),
        Err(e) => anyhow::bail!("asking the VM supervisor to stop faber's Linux VM: {e}"),
    }
    let deadline = Instant::now() + timeouts.stop;
    loop {
        let listed = supervisor.list().await.map_err(|e| {
            anyhow::anyhow!("asking the VM supervisor whether faber's Linux VM stopped: {e}")
        })?;
        match listed.into_iter().find(|v| v.claim == claim) {
            None => return Ok(()),
            Some(v) if v.state == VmState::Stopped => return Ok(()),
            Some(_) => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            anyhow::bail!(
                "faber's Linux VM was not confirmed stopped within {:?}",
                timeouts.stop
            );
        }
        sleep(left.min(timeouts.poll)).await;
    }
}

/// Start the driver for this host's engine, when faber runs as `_faber` with a
/// supervisor to ask (the setup record names `_faber_vm`). `false` where it does
/// not: a LEGACY host has no engine.
#[cfg(target_os = "macos")]
pub fn spawn_for_this_host(
    limiter: std::sync::Arc<crate::limiter::Limiter>,
    cfg: EngineConfig,
    wanted_images: Vec<String>,
) -> bool {
    if crate::platform::setup_record().is_none() {
        return false;
    }
    // popover/vms/anatomy.md O2: the Linux VM's row in `/api/state` is kept fresh
    // by a 15 s list of this supervisor, beside the driver's own loop.
    tokio::spawn(super::adopt::list_poll_loop(
        super::job::ProductionSupervisor::for_this_host(),
        super::proto::VmKind::Linux,
        limiter.clone(),
    ));
    let engine = Engine {
        supervisor: super::job::ProductionSupervisor::for_this_host(),
        // Docker is served only from a VM whose probe the root checker's
        // counters show the wall stopped.
        wall: super::wall_proof::CounterWall::for_this_host(),
        // Each time the engine comes into service, the kept runner image is put
        // into its Docker first ([`super::runner_image`]).
        sink: super::runner_image::LoadingSink::new(
            limiter,
            super::runner_image::Loader::new(
                super::runner_image::SocketEngine::new(crate::platform::vm_docker_socket()),
                crate::setup::vm_images::runner_dir(),
                super::runner_image::LoadTimeouts::default(),
            ),
            wanted_images,
        ),
        cfg,
        grow: super::grow::global().clone(),
    };
    engine.say(EngineStatus::Starting, false);
    tokio::spawn(engine.run());
    true
}

/// Start the grow loop for this host's engine (ADR-0061): the engine's disk
/// grows within `[linux_vm] disk_max_gib`. Beside the driver, against the same
/// supervisor.
#[cfg(target_os = "macos")]
pub fn spawn_grow_for_this_host(
    orch: std::sync::Arc<crate::reconcile::Orchestrator>,
    boot: super::grow::GrowBoot,
) {
    tokio::spawn(super::grow::run(super::grow::ThisHost {
        supervisor: super::job::ProductionSupervisor::for_this_host(),
        orch,
        boot,
    }));
}

/// The docker jobs a switched-off Linux VM still runs, after one pass of
/// [`keep_off`]'s `retire` (ADR-0051): what is left, and when the last lifetime
/// cap among them runs out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Left {
    /// Runner containers still running, busy or not yet capped.
    pub jobs: usize,
    /// The latest cap deadline among them, Unix ms.
    pub latest_deadline_ms: Option<u64>,
    /// How many of them have no cap at all (`runner_timeout_secs = 0`, or a
    /// start time that does not read): only their own exit ends them.
    pub uncapped: usize,
}

impl Left {
    /// Count one runner still running, capped at `deadline_ms` or not at all.
    pub fn add(&mut self, deadline_ms: Option<u64>) {
        self.jobs += 1;
        match deadline_ms {
            Some(d) => self.latest_deadline_ms = self.latest_deadline_ms.max(Some(d)),
            None => self.uncapped += 1,
        }
    }

    /// The `/api/state` notice while the VM waits for them, at `now_ms`.
    pub fn notice(&self, now_ms: u64) -> String {
        let jobs = format!(
            "{} docker job{}",
            self.jobs,
            if self.jobs == 1 { "" } else { "s" }
        );
        let by = match (self.latest_deadline_ms, self.uncapped) {
            (_, n) if n == self.jobs => {
                " No lifetime cap applies to them (runner_timeout_secs = 0), so only their \
                 own end stops it."
                    .to_string()
            }
            (Some(d), n) => {
                let mins = d.saturating_sub(now_ms).div_ceil(60_000);
                let uncapped = if n > 0 {
                    format!(" ({n} of them uncapped)")
                } else {
                    String::new()
                };
                format!(
                    " Their lifetime cap ends them within {}h {:02}m at the latest{uncapped}.",
                    mins / 60,
                    mins % 60
                )
            }
            (None, _) => String::new(),
        };
        format!(
            "faber's Linux VM is switched off ([linux_vm] enabled = false, ADR-0051) but still \
             runs {jobs} from before the restart; it stops once they finish, and stays counted \
             in the machine share until then. Idle runners are withdrawn and removed at once.{by}"
        )
    }
}

/// Where [`keep_off`] reports to.
pub trait OffSink {
    /// The memory the VM may hold now, MiB: 0 only once it is confirmed stopped.
    fn held(&self, mib: u64);
    /// What the VM still waits for, or `None` once it waits for nothing.
    fn waiting(&self, left: Option<&Left>);
}

impl OffSink for std::sync::Arc<crate::limiter::Limiter> {
    fn held(&self, mib: u64) {
        self.set_linux_vm_mib(mib);
    }
    fn waiting(&self, left: Option<&Left>) {
        self.set_linux_vm_waiting(left.map(|l| l.notice(crate::audit::now_ms())));
    }
}

/// Keep the engine off, for `[linux_vm] enabled = false` (ADR-0051), and keep it
/// counted inside the machine share until it is. No driver runs, so nothing starts
/// it again. Returns once the VM is confirmed stopped, or the supervisor no longer
/// lists it. Never deletes it: its disk holds the only copy of the runner image
/// (AGENTS.md).
///
/// A VM a previous boot left in service may still run the docker jobs that boot
/// started. A restart is safe for them (ADR-0011), so switching the VM off must
/// not kill them: the boot drops the docker profiles, which stops new claims at
/// once, and the VM is stopped only once `retire` leaves no runner running.
/// `retire` is one pass over them: idle runners withdrawn and removed, capped
/// ones removed at their lifetime cap, busy ones left to finish
/// ([`crate::docker::retire_unwatched`]). So an idle runner never holds the VM,
/// and a busy one holds it until its cap at the latest.
///
/// - Not listed, or `stopped`: done, and the share holds nothing for it.
/// - `in_service`: `Some` with no job left stops it. Jobs left, or `None` (Docker
///   unread, which is not "none", ADR-0011 §5), wait [`EngineTimeouts::watch`]
///   and pass again; the sink is told what is left.
/// - `unmanaged`: an older launcher runs it, left alone until it exits.
/// - Any other state serves no Docker, so no job can run in it: stopped at once.
/// - A stop not confirmed leaves the VM counted, and is tried again after the
///   driver's back-off.
pub async fn keep_off<S, F, Fut>(
    supervisor: &S,
    cfg: &EngineConfig,
    sink: &impl OffSink,
    mut retire: F,
) where
    S: VmSupervisor,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<Left>>,
{
    let t = &cfg.timeouts;
    let mib = u64::from(cfg.memory_mib.get());
    let mut backoff = t.retry_min;
    let mut said = String::new();
    let mut say = |line: String| {
        if line != said {
            info!("{line}");
            said = line;
        }
    };
    let off = |sink: &dyn OffSink| {
        sink.waiting(None);
        sink.held(0);
        info!("engine: faber's Linux VM is off ([linux_vm] enabled = false)");
    };
    sink.held(mib);
    loop {
        let found = match supervisor.list().await {
            Ok(list) => list.into_iter().find(|v| v.claim == claim()),
            Err(e) => {
                say(format!(
                    "engine: [linux_vm] enabled = false, but the VM supervisor does not answer \
                     ({e}); counting faber's Linux VM as running until it does"
                ));
                sleep(t.watch).await;
                continue;
            }
        };
        match found.map(|v| v.state) {
            None | Some(VmState::Stopped) => return off(sink),
            Some(VmState::Unmanaged) => {
                say(format!(
                    "engine: [linux_vm] enabled = false, but {UNMANAGED}"
                ));
                sleep(t.watch).await;
                continue;
            }
            Some(VmState::InService) => match retire().await {
                Some(left) if left.jobs == 0 => {}
                left => {
                    say(match left {
                        Some(l) => format!(
                            "engine: [linux_vm] enabled = false; keeping faber's Linux VM \
                             running until the {} docker job(s) left in it finish (ADR-0011)",
                            l.jobs
                        ),
                        None => "engine: [linux_vm] enabled = false; cannot read the Linux \
                                 VM's Docker, so it keeps running until it shows no docker \
                                 job left"
                            .to_string(),
                    });
                    if let Some(l) = &left {
                        sink.waiting(Some(l));
                    }
                    sleep(t.watch).await;
                    continue;
                }
            },
            Some(_) => {}
        }
        sink.waiting(None);
        match stop_for_revival(supervisor, t).await {
            Ok(()) => return off(sink),
            Err(e) => {
                warn!(
                    "engine: [linux_vm] enabled = false, but stopping the VM failed: {e}; it \
                     stays counted in the machine share, trying again in {backoff:?}"
                );
                sleep(backoff).await;
                backoff = (backoff * 2).min(t.retry_max);
            }
        }
    }
}

/// [`keep_off`] for this host, for `[linux_vm] enabled = false` (ADR-0051), with
/// `retire` the orchestrator's pass over the runners left in the VM. The share
/// counts the VM before this returns, so no macOS VM is admitted beside one that
/// may still run.
#[cfg(target_os = "macos")]
pub fn keep_off_for_this_host<F, Fut>(
    limiter: std::sync::Arc<crate::limiter::Limiter>,
    cfg: EngineConfig,
    retire: F,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Option<Left>> + Send,
{
    limiter.set_linux_vm_mib(u64::from(cfg.memory_mib.get()));
    tokio::spawn(async move {
        keep_off(
            &super::job::ProductionSupervisor::for_this_host(),
            &cfg,
            &limiter,
            retire,
        )
        .await
    });
}

/// [`stop_for_revival`] against this host's supervisor.
#[cfg(target_os = "macos")]
pub async fn restart_this_host() -> anyhow::Result<()> {
    stop_for_revival(
        &super::job::ProductionSupervisor::for_this_host(),
        &EngineTimeouts::default(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests, against a fake supervisor
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::proto::{Adopted, JitConfig, UnixSecs};
    use std::sync::{Arc, Mutex};

    use crate::vm::wall_proof::tests::TickingPf;
    use crate::vm::wall_proof::CounterWall;

    #[derive(Default)]
    struct Script {
        refuse_start: Option<ErrorCode>,
        /// Never advance past this state.
        stall_at: Option<VmState>,
        /// `stop` is accepted but the VM never reaches `stopped`.
        stop_never_lands: bool,
        /// `list` fails from this call on (0 = never).
        list_fails_from: usize,
        /// `list` reports `stopping` this many times before `stopped`.
        stopping_polls: u32,
        /// A `start` that asks to grow the disk is refused `bad_request`.
        refuse_grow: bool,
        /// A `start` that asks to grow the disk fails `start_failed`.
        fail_grow_start: bool,
    }

    #[derive(Default)]
    struct World {
        script: Script,
        vm: Option<VmRecord>,
        calls: Vec<String>,
        lists: usize,
        stopping_seen: u32,
        /// Each `start`'s `disk_bytes`, in order.
        disk_asked: Vec<Option<u64>>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<World>>);

    impl Fake {
        fn with(script: Script) -> Self {
            Self(Arc::new(Mutex::new(World {
                script,
                ..Default::default()
            })))
        }
        fn holding(state: VmState) -> Self {
            let f = Self::default();
            f.0.lock().unwrap().vm = Some(record(state));
            f
        }
        fn wire(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| *c != "list")
                .cloned()
                .collect()
        }
        fn state(&self) -> Option<VmState> {
            self.0.lock().unwrap().vm.as_ref().map(|v| v.state)
        }
        fn set(&self, state: VmState) {
            self.0.lock().unwrap().vm.as_mut().unwrap().state = state;
        }
    }

    fn record(state: VmState) -> VmRecord {
        VmRecord {
            slot: slot(),
            kind: VmKind::Linux,
            claim: claim(),
            state,
            stop_reason: (state == VmState::Stopped).then_some(StopReason::Requested),
            failed: None,
            started_unix: UnixSecs::try_from(1_760_000_000).unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        }
    }

    impl VmSupervisor for Fake {
        async fn start(&self, req: Start) -> Result<VmRecord, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push(format!("start:{}", req.slot.get()));
            assert_eq!(req.kind, VmKind::Linux);
            assert_eq!(req.claim.as_str(), ENGINE_CLAIM);
            assert_eq!(req.image.as_str(), ENGINE_IMAGE);
            assert!(req.tools.is_none(), "a Linux start names no tools disk");
            assert_eq!((req.cpus.get(), req.memory_mib.get()), (6, 16384));
            w.disk_asked.push(req.disk_bytes.map(|d| d.get()));
            if w.script.refuse_grow && req.disk_bytes.is_some() {
                return Err(SupervisorError::Refused(ErrorCode::BadRequest));
            }
            if w.script.fail_grow_start && req.disk_bytes.is_some() {
                return Err(SupervisorError::Refused(ErrorCode::StartFailed));
            }
            if let Some(code) = w.script.refuse_start {
                return Err(SupervisorError::Refused(code));
            }
            let r = record(VmState::Starting);
            w.vm = Some(r.clone());
            Ok(r)
        }
        async fn probe(&self, _: &ClaimId) -> Result<VmRecord, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("probe".into());
            let vm =
                w.vm.as_mut()
                    .ok_or(SupervisorError::Refused(ErrorCode::UnknownClaim))?;
            if vm.state != VmState::Ready {
                return Err(SupervisorError::Refused(ErrorCode::WrongState));
            }
            vm.state = VmState::ProbeSent;
            Ok(vm.clone())
        }
        async fn begin(
            &self,
            _: &ClaimId,
            jit: Option<JitConfig>,
        ) -> Result<VmRecord, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("begin".into());
            assert!(jit.is_none(), "the Linux VM is never handed a JIT config");
            let vm =
                w.vm.as_mut()
                    .ok_or(SupervisorError::Refused(ErrorCode::UnknownClaim))?;
            if vm.state != VmState::ProbeSent {
                return Err(SupervisorError::Refused(ErrorCode::WrongState));
            }
            vm.state = VmState::InService;
            Ok(vm.clone())
        }
        async fn stop(&self, _: &ClaimId) -> Result<(), SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("stop".into());
            let never = w.script.stop_never_lands;
            match w.vm.as_mut() {
                None => Err(SupervisorError::Refused(ErrorCode::UnknownClaim)),
                Some(vm) => {
                    if vm.state != VmState::Stopped && !never {
                        vm.state = VmState::Stopping;
                    }
                    Ok(())
                }
            }
        }
        async fn adopt(&self, _: &[ClaimId]) -> Result<Adopted, SupervisorError> {
            unreachable!("the engine driver never adopts: the Linux VM is not a job")
        }
        async fn list(&self) -> Result<Vec<VmRecord>, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("list".into());
            w.lists += 1;
            if w.script.list_fails_from > 0 && w.lists >= w.script.list_fails_from {
                return Err(SupervisorError::NoReply);
            }
            let stall = w.script.stall_at;
            let lingering = w.script.stopping_polls > w.stopping_seen;
            if lingering && w.vm.as_ref().is_some_and(|v| v.state == VmState::Stopping) {
                w.stopping_seen += 1;
                return Ok(w.vm.iter().cloned().collect());
            }
            if let Some(vm) = w.vm.as_mut() {
                if Some(vm.state) != stall {
                    vm.state = match vm.state {
                        VmState::Starting => VmState::Booting,
                        VmState::Booting => VmState::Ready,
                        VmState::Stopping => {
                            vm.stop_reason = Some(StopReason::Requested);
                            VmState::Stopped
                        }
                        s => s,
                    };
                }
            }
            Ok(w.vm.iter().cloned().collect())
        }
    }

    struct Wall(WallVerdict);
    impl WallProof for Wall {
        async fn arm(&self, s: Slot, c: &ClaimId) {
            assert_eq!((s.get(), c.as_str()), (LINUX_SLOT, ENGINE_CLAIM));
        }
        async fn verdict(&self, s: Slot, c: &ClaimId) -> WallVerdict {
            assert_eq!((s.get(), c.as_str()), (LINUX_SLOT, ENGINE_CLAIM));
            self.0
        }
    }

    /// Every report, in order.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<(EngineStatus, u64)>>>);
    impl EngineSink for Sink {
        fn engine(&self, status: &EngineStatus, held_mib: u64) {
            self.0.lock().unwrap().push((status.clone(), held_mib));
        }
    }
    impl Sink {
        fn last(&self) -> (EngineStatus, u64) {
            self.0.lock().unwrap().last().cloned().unwrap()
        }
        fn statuses(&self) -> Vec<EngineStatus> {
            let mut out: Vec<EngineStatus> = Vec::new();
            for (s, _) in self.0.lock().unwrap().iter() {
                if out.last() != Some(s) {
                    out.push(s.clone());
                }
            }
            out
        }
    }

    fn cfg() -> EngineConfig {
        EngineConfig::from_file(&crate::config::LinuxVmFile::default()).0
    }

    fn engine(fake: &Fake, wall: WallVerdict, sink: &Sink) -> Engine<Fake, Wall, Sink> {
        Engine {
            supervisor: fake.clone(),
            wall: Wall(wall),
            sink: sink.clone(),
            cfg: cfg(),
            grow: super::super::grow::Grow::new(128, 25),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_absent_engine_is_started_proven_and_bridged_in_order() {
        let (fake, sink) = (Fake::default(), Sink::default());
        engine(&fake, WallVerdict::Held, &sink)
            .bring_up()
            .await
            .unwrap();
        assert_eq!(fake.wire(), ["start:14", "probe", "begin"]);
        assert_eq!(
            sink.statuses(),
            [
                EngineStatus::Starting,
                EngineStatus::Booting,
                EngineStatus::ProvingWall,
                EngineStatus::InService
            ]
        );
        assert_eq!(sink.last(), (EngineStatus::InService, 16384));
        assert_eq!(EngineStatus::InService.fault(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_asks_for_the_pending_grow_and_none_without_one() {
        let (fake, sink) = (Fake::default(), Sink::default());
        let e = engine(&fake, WallVerdict::Held, &sink);
        e.bring_up().await.unwrap();
        assert_eq!(fake.0.lock().unwrap().disk_asked, [None]);

        let fake = Fake::default();
        let mut e = engine(&fake, WallVerdict::Held, &sink);
        e.grow = grow_pending(96);
        e.bring_up().await.unwrap();
        assert_eq!(fake.0.lock().unwrap().disk_asked, [Some(96 << 30)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_grow_starts_the_vm_at_its_size_and_raises_the_fault() {
        let fake = Fake::with(Script {
            refuse_grow: true,
            ..Default::default()
        });
        let sink = Sink::default();
        let mut e = engine(&fake, WallVerdict::Held, &sink);
        e.grow = grow_pending(96);
        e.bring_up().await.unwrap();
        assert_eq!(fake.0.lock().unwrap().disk_asked, [Some(96 << 30), None]);
        assert_eq!(fake.wire(), ["start:14", "start:14", "probe", "begin"]);
        assert_eq!(e.grow.start_size(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_grow_that_fails_the_start_is_dropped_and_the_vm_comes_up() {
        let fake = Fake::with(Script {
            fail_grow_start: true,
            ..Default::default()
        });
        let sink = Sink::default();
        let mut e = engine(&fake, WallVerdict::Held, &sink);
        e.grow = grow_pending(96);
        e.bring_up().await.unwrap();
        assert_eq!(fake.0.lock().unwrap().disk_asked, [Some(96 << 30), None]);
        assert_eq!(e.grow.start_size(), None);
        assert_eq!(sink.last().0, EngineStatus::InService);
    }

    /// A grow state with a grow to `gib` pending, as the policy loop leaves it.
    fn grow_pending(gib: u64) -> std::sync::Arc<crate::vm::grow::Grow> {
        crate::vm::grow::Grow::with_target_for_test(gib << 30)
    }

    #[tokio::test(start_paused = true)]
    async fn an_engine_left_in_service_is_taken_back_without_a_start() {
        let fake = Fake::holding(VmState::InService);
        let sink = Sink::default();
        engine(&fake, WallVerdict::Held, &sink)
            .bring_up()
            .await
            .unwrap();
        assert!(fake.wire().is_empty(), "{:?}", fake.wire());
        assert_eq!(sink.last().0, EngineStatus::InService);
    }

    #[tokio::test(start_paused = true)]
    async fn an_engine_found_probe_sent_is_stopped_never_begun() {
        let fake = Fake::holding(VmState::ProbeSent);
        let sink = Sink::default();
        let f = engine(&fake, WallVerdict::Held, &sink)
            .bring_up()
            .await
            .unwrap_err();
        assert!(f.why.contains("did not see sent"), "{}", f.why);
        assert_eq!(fake.wire(), ["stop"]);
        assert!(!f.held);
    }

    /// How many probes the fake pf has seen sent.
    type Probes = Box<dyn Fn() -> u64 + Send + Sync>;

    fn counted(
        fake: &Fake,
        counting: bool,
        red: bool,
        sink: &Sink,
    ) -> Engine<Fake, CounterWall<TickingPf<Probes>>, Sink> {
        let seen = fake.clone();
        let probes: Probes = Box::new(move || {
            if counting {
                seen.wire().iter().filter(|c| *c == "probe").count() as u64
            } else {
                0
            }
        });
        Engine {
            supervisor: fake.clone(),
            wall: CounterWall::new(TickingPf::new(probes, red)),
            sink: sink.clone(),
            cfg: cfg(),
            grow: super::super::grow::Grow::new(128, 25),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_the_checkers_counters_show_stopped_brings_the_engine_to_begin() {
        let (fake, sink) = (Fake::default(), Sink::default());
        let engine = counted(&fake, true, false, &sink);
        engine.bring_up().await.unwrap();
        assert_eq!(engine.wall.armed_len(), 0);
        assert_eq!(fake.wire(), ["start:14", "probe", "begin"]);
        assert_eq!(sink.last().0, EngineStatus::InService);
    }

    #[tokio::test(start_paused = true)]
    async fn an_uncounted_probe_or_a_red_wall_never_bridges_docker() {
        for (what, counting, red) in [("uncounted", false, false), ("red", true, true)] {
            let (fake, sink) = (Fake::default(), Sink::default());
            let engine = counted(&fake, counting, red, &sink);
            let f = engine.bring_up().await.unwrap_err();
            assert!(f.why.contains("failing closed"), "{what}: {}", f.why);
            // A failed proof leaves nothing armed, as a held one does.
            assert_eq!(engine.wall.armed_len(), 0, "{what}");
            assert!(!fake.wire().contains(&"begin".to_string()), "{what}");
            assert_eq!(fake.state(), Some(VmState::Stopped), "{what}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_breached_wall_stops_the_vm_and_never_bridges_docker() {
        let (fake, sink) = (Fake::default(), Sink::default());
        let f = engine(&fake, WallVerdict::Breached, &sink)
            .bring_up()
            .await
            .unwrap_err();
        assert!(f.why.contains("did not stop its probe"), "{}", f.why);
        assert!(!f.held, "confirmed stopped");
        assert_eq!(fake.wire(), ["start:14", "probe", "stop"]);
        assert_eq!(fake.state(), Some(VmState::Stopped));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_wall_fails_closed_at_the_probe_deadline() {
        let (fake, sink) = (Fake::default(), Sink::default());
        let f = engine(&fake, WallVerdict::Unreadable, &sink)
            .bring_up()
            .await
            .unwrap_err();
        assert!(f.why.contains("failing closed"), "{}", f.why);
        assert_eq!(fake.wire(), ["start:14", "probe", "stop"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_engine_that_never_becomes_ready_is_stopped() {
        let fake = Fake::with(Script {
            stall_at: Some(VmState::Booting),
            ..Default::default()
        });
        let sink = Sink::default();
        let f = engine(&fake, WallVerdict::Held, &sink)
            .bring_up()
            .await
            .unwrap_err();
        assert!(f.why.contains("did not become ready"), "{}", f.why);
        assert_eq!(fake.wire(), ["start:14", "stop"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_stop_reports_the_memory_still_held() {
        let fake = Fake::with(Script {
            stop_never_lands: true,
            ..Default::default()
        });
        let sink = Sink::default();
        let f = engine(&fake, WallVerdict::Breached, &sink)
            .bring_up()
            .await
            .unwrap_err();
        assert!(f.held && f.why.contains("not confirmed stopped"), "{f:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_start_holds_nothing() {
        for code in [ErrorCode::SlotBusy, ErrorCode::ImageNotFound] {
            let fake = Fake::with(Script {
                refuse_start: Some(code),
                ..Default::default()
            });
            let f = engine(&fake, WallVerdict::Held, &Sink::default())
                .bring_up()
                .await
                .unwrap_err();
            assert!(!f.held, "{f:?}");
            assert_eq!(fake.wire(), ["start:14"], "nothing to stop");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unmanaged_engine_is_left_alone() {
        let fake = Fake::holding(VmState::Unmanaged);
        let f = engine(&fake, WallVerdict::Held, &Sink::default())
            .bring_up()
            .await
            .unwrap_err();
        assert!(f.why.contains("left alone") && f.held, "{f:?}");
        assert!(fake.wire().is_empty(), "never stopped, never started");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopping_engine_is_waited_out_then_started_afresh() {
        let fake = Fake::with(Script {
            stopping_polls: 3,
            ..Default::default()
        });
        fake.0.lock().unwrap().vm = Some(record(VmState::Stopping));
        let sink = Sink::default();
        engine(&fake, WallVerdict::Held, &sink)
            .bring_up()
            .await
            .unwrap();
        assert_eq!(fake.wire(), ["start:14", "probe", "begin"]);
        assert_eq!(sink.statuses()[0], EngineStatus::Stopping);
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_ends_when_the_engine_leaves_service() {
        let fake = Fake::holding(VmState::InService);
        let e = engine(&fake, WallVerdict::Held, &Sink::default());
        let watch = e.watch();
        let leave = async {
            sleep(Duration::from_secs(40)).await;
            fake.set(VmState::Stopping);
        };
        let (why, ()) = tokio::join!(watch, leave);
        assert!(why.contains("left service"), "{why}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_tolerates_a_few_missed_lists_and_not_more() {
        let fake = Fake::with(Script {
            list_fails_from: 2,
            ..Default::default()
        });
        fake.0.lock().unwrap().vm = Some(record(VmState::InService));
        let e = engine(&fake, WallVerdict::Held, &Sink::default());
        let started = Instant::now();
        let why = e.watch().await;
        assert!(why.contains("did not answer 4 checks"), "{why}");
        assert!(started.elapsed() >= Duration::from_secs(4 * 15));
    }

    #[tokio::test(start_paused = true)]
    async fn the_driver_backs_off_after_a_failure_and_reports_it_down() {
        let (fake, sink) = (Fake::default(), Sink::default());
        let e = engine(&fake, WallVerdict::Breached, &sink);
        let _ = tokio::time::timeout(Duration::from_secs(200), e.run()).await;
        // 0 s, then 60 s and 180 s: three boots in 200 s, not one per poll.
        let starts = fake
            .wire()
            .iter()
            .filter(|c| c.starts_with("start"))
            .count();
        assert_eq!(starts, 3);
        let (status, held) = sink.last();
        assert!(matches!(status, EngineStatus::Down(_)), "{status:?}");
        assert_eq!(held, 0, "the VM was confirmed stopped");
        assert!(status.fault().unwrap().contains("not serving"));
    }

    #[tokio::test(start_paused = true)]
    async fn revival_stops_the_engine_and_waits_for_stopped() {
        let fake = Fake::holding(VmState::InService);
        stop_for_revival(&fake, &EngineTimeouts::default())
            .await
            .unwrap();
        assert_eq!(fake.wire(), ["stop"]);
        assert_eq!(fake.state(), Some(VmState::Stopped));
        // Nothing to stop is a stopped engine.
        stop_for_revival(&Fake::default(), &EngineTimeouts::default())
            .await
            .unwrap();
        // A stop that never lands is an error, after the bound.
        let stuck = Fake::with(Script {
            stop_never_lands: true,
            ..Default::default()
        });
        stuck.0.lock().unwrap().vm = Some(record(VmState::InService));
        let err = stop_for_revival(&stuck, &EngineTimeouts::default())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not confirmed stopped"), "{err}");
    }

    // --- ADR-0051: `[linux_vm] enabled = false` ---------------------------------

    /// What [`keep_off`] told the share and the notice, in order.
    #[derive(Clone, Default)]
    struct Held(Arc<Mutex<Vec<u64>>>, Arc<Mutex<Vec<Option<Left>>>>);
    impl OffSink for Held {
        fn held(&self, mib: u64) {
            self.0.lock().unwrap().push(mib)
        }
        fn waiting(&self, left: Option<&Left>) {
            self.1.lock().unwrap().push(left.copied())
        }
    }
    impl Held {
        fn seen(&self) -> Vec<u64> {
            let mut out: Vec<u64> = Vec::new();
            for m in self.0.lock().unwrap().iter() {
                if out.last() != Some(m) {
                    out.push(*m);
                }
            }
            out
        }
        fn notices(&self) -> Vec<Option<Left>> {
            self.1.lock().unwrap().clone()
        }
    }

    /// Jobs left after each pass, `deadline` the cap of each; then the last
    /// answer forever.
    fn left(jobs: usize, deadline: u64) -> Option<Left> {
        let mut l = Left::default();
        for _ in 0..jobs {
            l.add(Some(deadline));
        }
        Some(l)
    }

    /// A retire pass that answers each count in turn, then the last one forever.
    fn docker(
        answers: &[Option<Left>],
    ) -> (
        Arc<Mutex<usize>>,
        impl FnMut() -> std::future::Ready<Option<Left>>,
    ) {
        let asked = Arc::new(Mutex::new(0usize));
        let (answers, seen) = (answers.to_vec(), asked.clone());
        (asked, move || {
            let mut n = seen.lock().unwrap();
            let a = answers[(*n).min(answers.len() - 1)];
            *n += 1;
            std::future::ready(a)
        })
    }

    #[tokio::test(start_paused = true)]
    async fn switched_off_the_engine_waits_for_its_docker_jobs_then_stops() {
        let fake = Fake::holding(VmState::InService);
        let held = Held::default();
        let (asked, jobs) = docker(&[left(2, 9_000), left(1, 9_000), left(0, 0)]);
        let started = Instant::now();
        keep_off(&fake, &cfg(), &held, jobs).await;
        // Never stopped under a job a previous boot started (ADR-0011): two
        // watch periods of waiting, then one stop, never a delete.
        assert_eq!(*asked.lock().unwrap(), 3);
        assert!(started.elapsed() >= Duration::from_secs(2 * 15));
        assert_eq!(fake.wire(), ["stop"]);
        assert_eq!(fake.state(), Some(VmState::Stopped));
        // Counted in the share until confirmed stopped, then not; the notice
        // named what was left each pass, and is gone once nothing is.
        assert_eq!(held.seen(), [16384, 0]);
        let notices = held.notices();
        assert_eq!(notices[0].unwrap().jobs, 2);
        assert_eq!(notices[1].unwrap().jobs, 1);
        assert_eq!(notices.last(), Some(&None));
    }

    #[tokio::test(start_paused = true)]
    async fn switched_off_an_unread_docker_is_not_an_empty_one() {
        let fake = Fake::holding(VmState::InService);
        let held = Held::default();
        let (_, jobs) = docker(&[None]);
        let ran = tokio::time::timeout(
            Duration::from_secs(3600),
            keep_off(&fake, &cfg(), &held, jobs),
        )
        .await;
        assert!(ran.is_err(), "kept waiting");
        assert!(fake.wire().is_empty(), "{:?}", fake.wire());
        assert_eq!(held.seen(), [16384]);
    }

    #[tokio::test(start_paused = true)]
    async fn switched_off_a_stop_that_does_not_land_keeps_the_vm_counted() {
        let fake = Fake::with(Script {
            stop_never_lands: true,
            ..Default::default()
        });
        fake.0.lock().unwrap().vm = Some(record(VmState::InService));
        let held = Held::default();
        let (_, jobs) = docker(&[left(0, 0)]);
        let ran = tokio::time::timeout(
            Duration::from_secs(600),
            keep_off(&fake, &cfg(), &held, jobs),
        )
        .await;
        assert!(ran.is_err(), "the VM is never confirmed stopped");
        // Tried again after the back-off, and never counted as gone: no macOS VM
        // is admitted beside a VM that still holds its memory.
        let stops = fake.wire().iter().filter(|c| *c == "stop").count();
        assert!(stops >= 2, "{:?}", fake.wire());
        assert_eq!(held.seen(), [16384]);
    }

    #[tokio::test(start_paused = true)]
    async fn switched_off_a_vm_serving_no_docker_is_stopped_at_once() {
        // Gone already: nothing to stop, and nothing held.
        let (fake, held) = (Fake::default(), Held::default());
        let (asked, jobs) = docker(&[left(3, 9_000)]);
        keep_off(&fake, &cfg(), &held, jobs).await;
        assert!(fake.wire().is_empty());
        assert_eq!(held.seen(), [16384, 0]);
        assert_eq!(*asked.lock().unwrap(), 0);
        // Still booting: no Docker is served, so no job can be in it.
        let (fake, held) = (Fake::holding(VmState::Booting), Held::default());
        let (asked, jobs) = docker(&[left(3, 9_000)]);
        keep_off(&fake, &cfg(), &held, jobs).await;
        assert_eq!(fake.wire(), ["stop"]);
        assert_eq!(*asked.lock().unwrap(), 0, "Docker is never asked");
        assert_eq!(held.seen(), [16384, 0]);
    }

    #[test]
    fn the_waiting_notice_names_the_jobs_and_the_latest_deadline() {
        let mut l = Left::default();
        l.add(Some(60 * 60_000));
        l.add(Some(2 * 60 * 60_000 + 30 * 60_000));
        assert_eq!((l.jobs, l.latest_deadline_ms), (2, Some(150 * 60_000)));
        let text = l.notice(0);
        assert!(text.contains("still runs 2 docker jobs"), "{text}");
        assert!(text.contains("within 2h 30m at the latest"), "{text}");
        l.add(None);
        assert!(l.notice(0).contains("(1 of them uncapped)"));
        let mut none = Left::default();
        none.add(None);
        let text = none.notice(0);
        assert!(text.contains("1 docker job from"), "{text}");
        assert!(text.contains("No lifetime cap applies"), "{text}");
    }

    #[test]
    fn the_config_takes_defaults_and_warns_on_a_bad_value() {
        let (c, w) = EngineConfig::from_file(&crate::config::LinuxVmFile::default());
        assert_eq!((c.cpus.get(), c.memory_mib.get()), (6, 16384));
        assert_eq!(c.disk_max_gib, 128);
        assert!(w.is_empty());
        let (c, w) = EngineConfig::from_file(&crate::config::LinuxVmFile {
            disk_max_gib: Some(32),
            ..Default::default()
        });
        assert_eq!(c.disk_max_gib, 64, "a bad ceiling grows nothing");
        assert!(w[0].starts_with("linux_vm.disk_max_gib"), "{w:?}");
        let (c, w) = EngineConfig::from_file(&crate::config::LinuxVmFile {
            cpus: Some(0),
            memory_mib: Some(20480),
            ..Default::default()
        });
        assert_eq!((c.cpus.get(), c.memory_mib.get()), (6, 20480));
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("linux_vm.cpus"), "{w:?}");
    }

    #[test]
    fn the_fault_names_the_vm_and_the_reason() {
        assert_eq!(
            EngineStatus::Down("it stopped".into()).fault().as_deref(),
            Some("faber's Linux VM is not serving: it stopped (ADR-0026)")
        );
        assert!(EngineStatus::Booting.fault().unwrap().contains("booting"));
        assert!(!EngineStatus::Booting.serving());
    }
}
