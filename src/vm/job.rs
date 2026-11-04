//! The `macos-vm` backend's job lifecycle (ADR-0026, "Per job").
//!
//! One job is one VM, driven through the supervisor protocol ([`super::proto`])
//! in a fixed order:
//!
//! 1. **`start`**: the supervisor clones the golden image, gives the clone a fresh
//!    machine identifier and boots it ([`VmStep::Cloning`], then [`VmStep::Booting`]).
//! 2. **Wait for `ready`**: the guest agent reports the console user is `runner`.
//!    The supervisor folds "booted" and "console user is `runner`" into the one
//!    `booting` state, so this is one step here too.
//! 3. **`probe`, then the wall verdict** ([`VmStep::ProvingWall`]): the guest sends
//!    the wall probe and faber reads the verdict from the anchor's own rule
//!    counters ([`WallProof`]), never from the guest or the supervisor.
//! 4. **Mint the JIT runner and `begin`** ([`VmStep::StartingRunner`]): the config
//!    is minted only after the wall holds, so it never exists for a VM that fails
//!    the proof, and goes to the supervisor over the socket, never argv.
//! 5. **Run one job** ([`VmStep::Running`]) until the guest reports `job_finished`.
//!    A reclaim cuts it short only once GitHub has withdrawn the runner's
//!    registration ([`JitMint::withdraw`], ADR-0017); a refused one leaves it running.
//! 6. **`stop`** and wait for `stopped` ([`VmStep::Stopping`], [`VmStep::Deleted`]):
//!    a force-stop, and the supervisor deletes the clone with it.
//!
//! Every exit goes through step 6. A VM faber cannot confirm gone keeps its slot
//! ([`VmStep::Orphaned`]) rather than handing a slot the supervisor still holds to
//! the next job.
//!
//! # Seams
//!
//! Each of these is named so a test replaces one function, not this flow:
//!
//! - [`VmSupervisor`]: [`super::client::SupervisorClient`] on a host with a
//!   trusted setup record, and [`UnconnectedSupervisor`] without one, so a
//!   LEGACY host's `macos-vm` profile runs no job ([`ProductionSupervisor`]).
//! - [`WallProof`]: the rule counters the root wall daemon publishes, read
//!   before and after the probe ([`super::wall_proof::CounterWall`]). A host
//!   with no checker has no verdict file, so its proof fails closed.
//! - Admission (the two-VM cap, the machine share, the disk floor, the wall's
//!   verdict) is the limiter's ([`super::admit`]): the reconciler parks a claim
//!   in [`crate::limiter::Limiter::vm_admit`] before its runner is named, and
//!   [`run_one`] takes the [`VmRun`] it returns, so no VM starts unadmitted
//!   (#263). [`VmJobs::lease`] only picks a free slot within it.
//! - Adoption and the sweep ([`super::adopt`]): VMs outlive a faber restart, so
//!   at boot faber takes back the ones its claims name. A running job is watched
//!   to its end ([`watch_adopted`]) in a slot [`VmJobs::adopt_live`] marks
//!   held; one that cannot be resumed is stopped, and an `unmanaged` one is only
//!   held ([`VmJobs::hold`]), never stopped. Until that has answered,
//!   [`VmJobs::awaiting_adoption`] keeps every new VM job from starting. The
//!   sweep then stops VMs of faber's own that no job holds, and frees each held
//!   slot ([`VmJobs::settle`]) once `list` shows its VM gone. Neither deletes a
//!   file: the supervisor deletes clones, by their marker.
//! - Revival ([`super::revive`]): a VM still `starting` or `booting` at the boot
//!   deadline is stopped and started again in its slot, at most once per job and
//!   never after `begin`.
//! - [`JitMint`]: the runner's registration, minted in step 4 and withdrawn by a
//!   reclaim before the VM is stopped. Before `begin` no runner was started under
//!   it, so the withdrawal is cleanup: the VM is stopped whatever GitHub answers,
//!   and a registration it did not withdraw is the stale-runner sweep's (ADR-0018,
//!   #269). After `begin` the withdrawal is the reclaim's whole clearance (step 5).
//!   An adopted VM's runner is withdrawn by the name faber's own `mint` record
//!   gives it ([`MintRecord`], #105). The registration of a VM boot adoption
//!   stops before `begin` is still the sweep's.
//! - The tools disk is built (or found) before [`run_one`] is called
//!   ([`super::tools_disk::for_this_host`]); this flow only names it in `start`.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde::Serialize;
use tokio::sync::watch;
use tokio::time::{sleep, Instant};

use super::client::{Endpoint, SupervisorClient};
use super::proto::{
    Adopted, Check, ClaimId, ErrorCode, Expect, ImageName, JitConfig, ProtoError, Slot, Start,
    StopReason, VmKind, VmRecord, VmState, SLOTS,
};
use super::revive::{revive_decision, Revive, RevivePolicy};
use super::wall_proof::CounterWall;
use crate::audit::now_ms;
use crate::backend::MacosVmConfig;
use crate::docker::{
    claim_id, withdraw_minted, MintFailed, MintedRunner, RunReport, Withdrawal, RECLAIM_WITHDRAWAL,
    UNUSED_WITHDRAWAL,
};
use crate::github::Github;
use crate::limiter::VmRun;

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// Why a supervisor call failed.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// No connection to the supervisor: the request was never sent. (For `stop`, the
    /// VM it names may still be running, so that is not "stopped".)
    #[error("the VM supervisor is not connected")]
    Unreachable,
    /// The supervisor answered the handshake but faber cannot talk to it: another
    /// version (a half-applied release), the wrong peer, or a broken handshake.
    /// Nothing was sent. Not a lost connection: closing the VM-session gate would
    /// only flap it, since the gate's own probe still succeeds.
    #[error(
        "the VM supervisor cannot be used by this faber (version skew, wrong peer or a broken \
         handshake; see the log)"
    )]
    Misconfigured,
    /// The supervisor answered with an error code: it definitely did not act.
    #[error("the VM supervisor refused: {0:?}")]
    Refused(ErrorCode),
    /// The supervisor broke the protocol; faber closes the connection (the request
    /// may or may not have been seen).
    #[error("the VM supervisor broke the protocol: {0}")]
    Protocol(#[from] ProtoError),
    /// The request was sent and no reply came (a timeout, or the connection
    /// dropped): the supervisor may or may not have acted on it.
    #[error("the VM supervisor did not answer")]
    NoReply,
}

/// Everything faber asks of the VM supervisor. The wire is lockstep, one request
/// and its reply at a time ([`super::client`]).
pub trait VmSupervisor: Send + Sync {
    fn start(&self, req: Start) -> impl Future<Output = Result<VmRecord, SupervisorError>> + Send;
    fn probe(
        &self,
        claim: &ClaimId,
    ) -> impl Future<Output = Result<VmRecord, SupervisorError>> + Send;
    /// `jit` is required for a macOS VM and must be `None` for the Linux VM
    /// ([`super::engine`]); the supervisor refuses a mismatch.
    fn begin(
        &self,
        claim: &ClaimId,
        jit: Option<JitConfig>,
    ) -> impl Future<Output = Result<VmRecord, SupervisorError>> + Send;
    /// ADR-0049 §7: run the golden image's checks in a proven macOS VM, held to
    /// `expect`, instead of a job. Fails closed by default: only a supervisor that
    /// speaks it can verify, and a fake that does not care is refused.
    fn verify(
        &self,
        claim: &ClaimId,
        expect: Expect,
    ) -> impl Future<Output = Result<VmRecord, SupervisorError>> + Send {
        let _ = (claim, expect);
        async { Err(SupervisorError::Unreachable) }
    }
    /// Idempotent: a VM already stopping or stopped answers `done` again.
    fn stop(&self, claim: &ClaimId) -> impl Future<Output = Result<(), SupervisorError>> + Send;
    fn list(&self) -> impl Future<Output = Result<Vec<VmRecord>, SupervisorError>> + Send;
    /// At most [`super::proto::MAX_LIST`] claims per call.
    fn adopt(
        &self,
        claims: &[ClaimId],
    ) -> impl Future<Output = Result<Adopted, SupervisorError>> + Send;
}

/// What the wall's rule counters say about one VM's probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallVerdict {
    /// The anchor stopped the probe.
    Held,
    /// The probe got through. The VM is stopped and no job runs in it.
    Breached,
    /// The counters could not be read, or the probe has not been counted yet.
    /// Retried until the probe deadline, then treated as a failure: the wall fails
    /// closed.
    Unreadable,
}

/// The wall verdict for a VM's probe, read from the root wall daemon.
pub trait WallProof: Send + Sync {
    /// Called just before `probe` is sent for `claim`: what the verdict is
    /// later judged against. A verdict asked for without it is never `Held`.
    fn arm(&self, slot: Slot, claim: &ClaimId) -> impl Future<Output = ()> + Send;
    fn verdict(&self, slot: Slot, claim: &ClaimId) -> impl Future<Output = WallVerdict> + Send;
    /// The proof for `claim` is over, whatever its answer: drop anything
    /// [`Self::arm`] kept for it. Called on every exit from a proof, through
    /// [`Disarm`].
    fn disarm(&self, _claim: &ClaimId) {}
}

/// Disarms a claim's proof when dropped, so every way out of it — held,
/// breached, failed at the deadline, cancelled, or the whole future dropped —
/// leaves nothing armed behind.
pub struct Disarm<'a, W: WallProof> {
    wall: &'a W,
    claim: ClaimId,
}

impl<'a, W: WallProof> Disarm<'a, W> {
    pub fn new(wall: &'a W, claim: &ClaimId) -> Self {
        Self {
            wall,
            claim: claim.clone(),
        }
    }
}

impl<W: WallProof> Drop for Disarm<'_, W> {
    fn drop(&mut self) {
        self.wall.disarm(&self.claim);
    }
}

/// A minted JIT runner.
pub struct Minted {
    pub jit: JitConfig,
    /// The registration's id, which a reclaim withdraws it by ([`JitMint::withdraw`]).
    pub runner_id: Option<u64>,
}

/// The GitHub side of a job VM's runner: the JIT config it is minted, and the
/// withdrawal of its registration a reclaim asks for before it stops the VM.
pub trait JitMint: Send + Sync {
    fn mint(
        &self,
        runner_name: &str,
        labels: &[String],
    ) -> impl Future<Output = anyhow::Result<Minted>> + Send;
    /// Ask GitHub to forget the runner minted as `runner_name`, by `runner_id` when
    /// the mint answered with one. GitHub refuses while it has a job assigned to
    /// the runner, and once it agrees it can assign it nothing more (ADR-0017).
    /// `reason` is the write's audit line.
    fn withdraw(
        &self,
        runner_name: &str,
        runner_id: Option<u64>,
        reason: &str,
    ) -> impl Future<Output = Withdrawal> + Send;
}

/// GitHub's `generate-jitconfig`, for one repo.
pub struct GithubMint<'a> {
    gh: &'a Github,
    owner_repo: &'a str,
}

impl JitMint for GithubMint<'_> {
    async fn mint(&self, runner_name: &str, labels: &[String]) -> anyhow::Result<Minted> {
        let jit = self
            .gh
            .generate_jitconfig(
                self.owner_repo,
                runner_name,
                labels,
                "_work",
                "provision a job this host claimed, in a macOS VM",
            )
            .await
            .map_err(|e| anyhow::Error::new(e).context(MintFailed))?;
        // The protocol's own grammar and bound; a config outside it never leaves
        // faber, and the error never quotes it.
        let config = JitConfig::try_from(jit.encoded)
            .map_err(|e| anyhow!("GitHub's JIT config does not fit the VM protocol: {e}"))?;
        Ok(Minted {
            jit: config,
            runner_id: jit.runner_id,
        })
    }

    /// The runner this process minted, named by the mint itself, as a Docker
    /// reclaim names its own (#105).
    async fn withdraw(
        &self,
        runner_name: &str,
        runner_id: Option<u64>,
        reason: &str,
    ) -> Withdrawal {
        let minted = MintedRunner::from_mint(self.owner_repo, runner_name, runner_id);
        withdraw_minted(self.gh, Some(minted), reason).await
    }
}

/// The supervisor on a host without a trusted setup record, which has no
/// `_faber_vm` to trust at the other end: every call fails closed.
pub struct UnconnectedSupervisor;

impl VmSupervisor for UnconnectedSupervisor {
    async fn start(&self, _: Start) -> Result<VmRecord, SupervisorError> {
        Err(SupervisorError::Unreachable)
    }
    async fn probe(&self, _: &ClaimId) -> Result<VmRecord, SupervisorError> {
        Err(SupervisorError::Unreachable)
    }
    async fn begin(&self, _: &ClaimId, _: Option<JitConfig>) -> Result<VmRecord, SupervisorError> {
        Err(SupervisorError::Unreachable)
    }
    async fn stop(&self, _: &ClaimId) -> Result<(), SupervisorError> {
        Err(SupervisorError::Unreachable)
    }
    async fn list(&self) -> Result<Vec<VmRecord>, SupervisorError> {
        Err(SupervisorError::Unreachable)
    }
    async fn adopt(&self, _: &[ClaimId]) -> Result<Adopted, SupervisorError> {
        Err(SupervisorError::Unreachable)
    }
}

/// A wall that is never proven, for runs that must not reach the proof.
#[cfg(test)]
pub struct UnprovenWall;

#[cfg(test)]
impl WallProof for UnprovenWall {
    async fn arm(&self, _: Slot, _: &ClaimId) {}
    async fn verdict(&self, _: Slot, _: &ClaimId) -> WallVerdict {
        WallVerdict::Unreadable
    }
}

/// What one job needs from the outside.
pub struct VmEnv<'a, S, W, M> {
    pub supervisor: S,
    pub wall: W,
    pub mint: M,
    pub jobs: &'a VmJobs,
}

/// The supervisor a production job talks to.
pub enum ProductionSupervisor {
    /// The process's one socket client: jobs share its lockstep connection.
    Socket(&'static SupervisorClient),
    /// ADR-0051's session supervisor: every call is refused as unreachable
    /// unless the last check found the recorded uid to be this host's
    /// `_faber_mac` ([`super::session::account_trusted`]).
    Session(&'static SupervisorClient),
    Unconnected(UnconnectedSupervisor),
}

impl ProductionSupervisor {
    /// `_faber_vm`'s supervisor when the setup record names it, else
    /// [`UnconnectedSupervisor`]: the Linux VM's (the engine). The record is
    /// read once per process ([`crate::platform::setup_record`]), so the client
    /// is built at most once.
    pub fn for_this_host() -> Self {
        Self::of(process_client())
    }

    /// The supervisor macOS VM jobs run through: every start, adoption and
    /// sweep of one. Where the record names the session account, only ever
    /// the session supervisor ([`macos_job_endpoint`]), refused while the
    /// account check fails; never `_faber_vm`'s.
    pub fn for_macos_jobs() -> Self {
        match crate::platform::setup_record().and_then(session_endpoint) {
            Some(_) => match session_client() {
                Some(client) => Self::Session(client),
                None => Self::Unconnected(UnconnectedSupervisor),
            },
            None => Self::of(process_client()),
        }
    }

    /// The client a call goes to now, `None` for one that is refused.
    fn target(&self) -> Option<&'static SupervisorClient> {
        match self {
            Self::Socket(c) => Some(c),
            Self::Session(c) if super::session::account_trusted() => Some(c),
            Self::Session(_) | Self::Unconnected(_) => None,
        }
    }

    fn of(client: Option<&'static SupervisorClient>) -> Self {
        match client {
            Some(client) => Self::Socket(client),
            None => Self::Unconnected(UnconnectedSupervisor),
        }
    }
}

/// The process's `_faber_vm` supervisor client, or `None` when this host has
/// none. The engine and the updater's supervisor restart use it, and macOS VM
/// jobs too on a host whose record names no session account.
pub fn process_client() -> Option<&'static SupervisorClient> {
    static CLIENT: OnceLock<SupervisorClient> = OnceLock::new();
    crate::platform::setup_record().map(|record| {
        CLIENT.get_or_init(|| {
            SupervisorClient::new(Endpoint {
                path: crate::platform::vm_supervisor_socket(),
                peer_uid: record.faber_vm_uid(),
            })
        })
    })
}

/// The process's client of ADR-0051's session supervisor, `_faber_mac`'s, or
/// `None` when the record names no session account.
pub fn session_client() -> Option<&'static SupervisorClient> {
    static CLIENT: OnceLock<SupervisorClient> = OnceLock::new();
    let endpoint = session_endpoint(crate::platform::setup_record()?)?;
    Some(CLIENT.get_or_init(|| SupervisorClient::new(endpoint)))
}

/// Where macOS VM jobs are reached for `record`: the session supervisor when
/// the record names the session account, `None` to keep `_faber_vm`'s (a
/// record from before ADR-0051's session, or a host without one). Pure.
pub fn session_endpoint(record: &crate::platform::record::SetupRecord) -> Option<Endpoint> {
    record.faber_mac_uid().map(|uid| Endpoint {
        path: crate::platform::session_supervisor_socket(),
        peer_uid: uid,
    })
}

/// Where [`ProductionSupervisor::for_macos_jobs`] connects for `record`, as a
/// pure function: the
/// session supervisor whenever the record names the account, never
/// `_faber_vm`'s then.
pub fn macos_job_endpoint(record: &crate::platform::record::SetupRecord) -> Endpoint {
    session_endpoint(record).unwrap_or_else(|| Endpoint {
        path: crate::platform::vm_supervisor_socket(),
        peer_uid: record.faber_vm_uid(),
    })
}

impl VmSupervisor for ProductionSupervisor {
    async fn start(&self, req: Start) -> Result<VmRecord, SupervisorError> {
        match self.target() {
            Some(c) => VmSupervisor::start(c, req).await,
            None => UnconnectedSupervisor.start(req).await,
        }
    }
    async fn probe(&self, claim: &ClaimId) -> Result<VmRecord, SupervisorError> {
        match self.target() {
            Some(c) => VmSupervisor::probe(c, claim).await,
            None => UnconnectedSupervisor.probe(claim).await,
        }
    }
    async fn begin(
        &self,
        claim: &ClaimId,
        jit: Option<JitConfig>,
    ) -> Result<VmRecord, SupervisorError> {
        match self.target() {
            Some(c) => VmSupervisor::begin(c, claim, jit).await,
            None => UnconnectedSupervisor.begin(claim, jit).await,
        }
    }
    async fn verify(&self, claim: &ClaimId, expect: Expect) -> Result<VmRecord, SupervisorError> {
        match self.target() {
            Some(c) => VmSupervisor::verify(c, claim, expect).await,
            None => UnconnectedSupervisor.verify(claim, expect).await,
        }
    }
    async fn stop(&self, claim: &ClaimId) -> Result<(), SupervisorError> {
        match self.target() {
            Some(c) => VmSupervisor::stop(c, claim).await,
            None => UnconnectedSupervisor.stop(claim).await,
        }
    }
    async fn list(&self) -> Result<Vec<VmRecord>, SupervisorError> {
        match self.target() {
            Some(c) => VmSupervisor::list(c).await,
            None => UnconnectedSupervisor.list().await,
        }
    }
    async fn adopt(&self, claims: &[ClaimId]) -> Result<Adopted, SupervisorError> {
        match self.target() {
            Some(c) => VmSupervisor::adopt(c, claims).await,
            None => UnconnectedSupervisor.adopt(claims).await,
        }
    }
}

/// A mint for a run that never mints: a verification's, and the supervisor
/// access boot adoption and the sweep go through ([`adopted_env`]). It has no
/// registration to withdraw.
pub struct NoMint;

impl JitMint for NoMint {
    async fn mint(&self, _: &str, _: &[String]) -> anyhow::Result<Minted> {
        bail!("this run is never given a JIT config")
    }

    async fn withdraw(&self, _: &str, _: Option<u64>, _: &str) -> Withdrawal {
        Withdrawal::Failed(
            "nothing was minted here, so there is no registration to withdraw".into(),
        )
    }
}

/// An adopted VM's runner, as faber's own `mint` record names it (#105): never
/// given a new JIT config, since its job has begun, and withdrawn by that
/// record's name, since its id died with the process that minted it.
pub struct MintRecord<'a> {
    gh: &'a Github,
    /// `None` when no record of faber's own names the runner. Nothing about it
    /// is then written to GitHub, so a reclaim of it is always refused.
    minted: Option<MintedRunner>,
}

impl JitMint for MintRecord<'_> {
    async fn mint(&self, _: &str, _: &[String]) -> anyhow::Result<Minted> {
        bail!("an adopted VM is never given a new JIT config")
    }

    /// The record names the registration, never the VM's claim (#105).
    async fn withdraw(&self, _: &str, _: Option<u64>, reason: &str) -> Withdrawal {
        withdraw_minted(self.gh, self.minted.clone(), reason).await
    }
}

/// The production supervisor, wall proof and registry, and no mint: what boot
/// adoption, the sweep and the restart preflight reach the supervisor through.
/// An adopted job is watched in [`watched_env`], which can withdraw its runner.
pub fn adopted_env() -> VmEnv<'static, ProductionSupervisor, CounterWall, NoMint> {
    VmEnv {
        supervisor: ProductionSupervisor::for_macos_jobs(),
        wall: CounterWall::for_this_host(),
        mint: NoMint,
        jobs: global(),
    }
}

/// The environment an adopted VM's job is watched in ([`watch_adopted`]):
/// [`adopted_env`]'s, with the runner `minted` names for a reclaim to withdraw
/// first. [`watch_adopted`] never asks for a wall verdict.
pub fn watched_env(
    gh: &Github,
    minted: Option<MintedRunner>,
) -> VmEnv<'_, ProductionSupervisor, CounterWall, MintRecord<'_>> {
    VmEnv {
        supervisor: ProductionSupervisor::for_macos_jobs(),
        wall: CounterWall::for_this_host(),
        mint: MintRecord { gh, minted },
        jobs: global(),
    }
}

/// The production environment: the wall is proven from the root checker's
/// counters, and a VM whose probe they do not show stopped gets no job.
pub fn production_env<'a>(
    gh: &'a Github,
    owner_repo: &'a str,
) -> VmEnv<'a, ProductionSupervisor, CounterWall, GithubMint<'a>> {
    VmEnv {
        supervisor: ProductionSupervisor::for_macos_jobs(),
        wall: CounterWall::for_this_host(),
        mint: GithubMint { gh, owner_repo },
        jobs: global(),
    }
}

// ---------------------------------------------------------------------------
// The typed steps, and what `/api/state` shows
// ---------------------------------------------------------------------------

/// Where a job's VM is in its life, in faber's own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VmStep {
    /// `start` sent; the supervisor is cloning the image and building the VM.
    Cloning,
    /// The VM boots, until the console user is `runner`.
    Booting,
    /// The probe is sent and the wall verdict is awaited.
    ProvingWall,
    /// The JIT runner is minted and handed to the guest.
    StartingRunner,
    /// The job runs.
    Running,
    /// A verification (ADR-0049 §7): the guest runs the image's checks.
    Verifying,
    /// A force-stop was asked for; the clone is being deleted.
    Stopping,
    /// The VM is stopped and its clone deleted.
    Deleted,
    /// A stop could not be confirmed. The slot stays held until the sweep
    /// ([`super::adopt`]) sees the VM gone and settles it.
    Orphaned,
    /// A VM faber holds the slot of and never stops: an `unmanaged` child, or one
    /// whose claim predates instance ids. Freed once it is `stopped` or gone.
    Unmanaged,
}

/// One macOS VM job in `/api/state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmJobView {
    pub claim: String,
    pub slot: u8,
    pub step: VmStep,
    pub step_since_ms: u64,
}

#[derive(Default)]
struct JobsInner {
    jobs: BTreeMap<String, VmJobView>,
    /// Slots a live job holds.
    leased: u16,
    /// Slots whose VM faber could not confirm gone.
    quarantined: u16,
    #[cfg(test)]
    trail: Vec<(String, VmStep)>,
}

/// The macOS VM jobs this daemon is running, and the slots they hold.
#[derive(Default)]
pub struct VmJobs {
    inner: Mutex<JobsInner>,
    /// Set until boot adoption has said what the supervisor already runs: no new
    /// VM job starts meanwhile (ADR-0011 §5 for VMs). Off by default, so a
    /// registry nobody adopts into (every test) admits at once.
    adoption_pending: AtomicBool,
}

/// Slots no macOS job may lease: the Linux VM's ([`super::engine::LINUX_SLOT`])
/// and setup's build VM's ([`crate::setup::linux_image::BUILD_SLOT`]). The
/// supervisor would refuse either with `slot_busy` while it is held, but a job
/// that leased one while it was free would then hold the slot the engine needs.
const RESERVED_SLOTS: u16 =
    (1 << super::engine::LINUX_SLOT) | (1 << crate::setup::linux_image::BUILD_SLOT);

/// The daemon's registry.
pub fn global() -> &'static VmJobs {
    static JOBS: OnceLock<VmJobs> = OnceLock::new();
    JOBS.get_or_init(VmJobs::default)
}

impl VmJobs {
    /// The lowest slot no job holds, that is not quarantined, not reserved
    /// ([`RESERVED_SLOTS`]) and not in `skip` (bit per slot), recorded against
    /// `claim` in step [`VmStep::Cloning`].
    pub fn lease(&self, claim: &ClaimId, skip: u16) -> Option<Slot> {
        let mut g = self.inner.lock().unwrap();
        let taken = g.leased | g.quarantined | skip | RESERVED_SLOTS;
        let n = (0..SLOTS).find(|n| taken & (1 << n) == 0)?;
        g.leased |= 1 << n;
        let view = VmJobView {
            claim: claim.as_str().to_string(),
            slot: n,
            step: VmStep::Cloning,
            step_since_ms: now_ms(),
        };
        g.jobs.insert(claim.as_str().to_string(), view);
        #[cfg(test)]
        g.trail.push((claim.as_str().to_string(), VmStep::Cloning));
        Some(Slot::try_from(n).expect("n < SLOTS"))
    }

    fn set_step(&self, claim: &ClaimId, step: VmStep) {
        let mut g = self.inner.lock().unwrap();
        if let Some(v) = g.jobs.get_mut(claim.as_str()) {
            if v.step != step {
                v.step = step;
                v.step_since_ms = now_ms();
                #[cfg(test)]
                g.trail.push((claim.as_str().to_string(), step));
            }
        }
    }

    /// The job is over and its VM confirmed gone: free the slot and drop the entry.
    fn release(&self, claim: &ClaimId) {
        let mut g = self.inner.lock().unwrap();
        if let Some(v) = g.jobs.remove(claim.as_str()) {
            g.leased &= !(1 << v.slot);
        }
    }

    /// The VM could not be confirmed gone: keep the slot out of circulation and
    /// the entry visible as [`VmStep::Orphaned`].
    fn quarantine(&self, claim: &ClaimId) {
        let mut g = self.inner.lock().unwrap();
        if let Some(v) = g.jobs.get_mut(claim.as_str()) {
            let slot = v.slot;
            v.step = VmStep::Orphaned;
            v.step_since_ms = now_ms();
            g.leased &= !(1 << slot);
            g.quarantined |= 1 << slot;
            #[cfg(test)]
            g.trail.push((claim.as_str().to_string(), VmStep::Orphaned));
        }
    }

    /// The slots held for a VM faber could not confirm gone, or holds but never
    /// stops.
    pub fn quarantined(&self) -> Vec<u8> {
        let g = self.inner.lock().unwrap();
        (0..SLOTS)
            .filter(|n| g.quarantined & (1 << n) != 0)
            .collect()
    }

    /// Record a VM boot adoption found running a job: its slot is held as a live
    /// job's, so [`Self::lease`] never hands it out and the sweep leaves it alone.
    /// `false` when the claim or the slot is already taken.
    pub fn adopt_live(&self, claim: &ClaimId, slot: Slot, step: VmStep) -> bool {
        self.insert(claim, slot, step, false)
    }

    /// Hold `slot` for a VM faber is not running a job in: one being stopped, one
    /// still `stopping`, or one it must never stop. The sweep settles it once
    /// `list` shows it `stopped` or gone. `false` when the claim or the slot is
    /// already taken.
    pub fn hold(&self, claim: &ClaimId, slot: Slot, step: VmStep) -> bool {
        self.insert(claim, slot, step, true)
    }

    fn insert(&self, claim: &ClaimId, slot: Slot, step: VmStep, quarantined: bool) -> bool {
        let mut g = self.inner.lock().unwrap();
        let bit = 1u16 << slot.get();
        if (g.leased | g.quarantined) & bit != 0 || g.jobs.contains_key(claim.as_str()) {
            return false;
        }
        if quarantined {
            g.quarantined |= bit;
        } else {
            g.leased |= bit;
        }
        g.jobs.insert(
            claim.as_str().to_string(),
            VmJobView {
                claim: claim.as_str().to_string(),
                slot: slot.get(),
                step,
                step_since_ms: now_ms(),
            },
        );
        #[cfg(test)]
        g.trail.push((claim.as_str().to_string(), step));
        true
    }

    /// The claims a live job holds, and the claims held in quarantine: what the
    /// sweep judges `list` against ([`super::adopt::plan_sweep`]).
    pub fn held(&self) -> (HashSet<String>, HashSet<String>) {
        let g = self.inner.lock().unwrap();
        let mut live = HashSet::new();
        let mut quarantined = HashSet::new();
        for (claim, v) in &g.jobs {
            if g.quarantined & (1 << v.slot) != 0 {
                quarantined.insert(claim.clone());
            } else {
                live.insert(claim.clone());
            }
        }
        (live, quarantined)
    }

    /// Refuse every new VM job until [`Self::adoption_settled`].
    pub fn hold_for_adoption(&self) {
        self.adoption_pending.store(true, Ordering::SeqCst);
    }

    /// Boot adoption has answered: new VM jobs may start, and the sweep may run.
    pub fn adoption_settled(&self) {
        self.adoption_pending.store(false, Ordering::SeqCst);
    }

    /// Whether boot adoption has yet to answer.
    pub fn awaiting_adoption(&self) -> bool {
        self.adoption_pending.load(Ordering::SeqCst)
    }

    /// The supervisor's `list` or `adopt` confirmed `claim`'s VM gone, so its slot
    /// is free again.
    pub fn settle(&self, claim: &ClaimId) {
        let mut g = self.inner.lock().unwrap();
        if let Some(v) = g.jobs.remove(claim.as_str()) {
            g.quarantined &= !(1 << v.slot);
            g.leased &= !(1 << v.slot);
        }
    }

    /// Every job's step, ordered by slot, for `/api/state`.
    pub fn view(&self) -> Vec<VmJobView> {
        let g = self.inner.lock().unwrap();
        let mut v: Vec<VmJobView> = g.jobs.values().cloned().collect();
        v.sort_by_key(|j| j.slot);
        v
    }

    #[cfg(test)]
    fn trail(&self, claim: &str) -> Vec<VmStep> {
        let g = self.inner.lock().unwrap();
        g.trail
            .iter()
            .filter(|(c, _)| c == claim)
            .map(|(_, s)| *s)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The lifecycle
// ---------------------------------------------------------------------------

/// How a run ended, before teardown has said whether the VM is gone.
enum End {
    /// The guest reported `job_finished`.
    Finished,
    /// A reclaim won, at the named seam.
    Reclaimed(&'static str),
    /// The job outlived its cap.
    JobTimedOut,
    /// Faber's own infrastructure failed before the job could finish.
    Failed(anyhow::Error),
}

enum Wait {
    Reached,
    Stopped(Option<StopReason>),
    Gone,
    TimedOut,
    Cancelled,
    Failed(SupervisorError),
}

async fn cancelled(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow() {
            return;
        }
        if rx.changed().await.is_err() {
            // The sender is gone: nothing will ever reclaim this claim.
            std::future::pending::<()>().await;
        }
    }
}

/// Resolve on a reclaim request not yet answered: the flag set, as [`cancelled`],
/// or, once the request standing was `refused`, the next one sent. Each request
/// is a fresh `send_replace(true)` from the reconciler, which wakes a receiver
/// even when the flag is already `true`. A dropped sender is never a request.
async fn requested(rx: &mut watch::Receiver<bool>, refused: bool) {
    if !refused {
        return cancelled(rx).await;
    }
    loop {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *rx.borrow() {
            return;
        }
    }
}

/// How a run reclaimed in service is reported: GitHub agreed to forget its
/// runner, which it does only for a runner with no job assigned.
const RECLAIMED_IN_SERVICE: &str =
    "in service, once GitHub had withdrawn its runner (no job was assigned to it)";

/// The registration a job's runner holds once `begin` has handed it over.
struct Registration {
    /// The name it registered under: `faber-{claim}`.
    runner: String,
    /// GitHub's id for it, when the mint answered with one.
    id: Option<u64>,
}

struct Run<'a, S, W, M> {
    env: &'a VmEnv<'a, S, W, M>,
    claim: ClaimId,
    slot: Slot,
    timeouts: crate::backend::VmTimeouts,
    cancel: watch::Receiver<bool>,
    /// What started this VM, sent again by a restart. `None` for an adopted VM,
    /// which is never restarted: its job has begun.
    start: Option<Start>,
    revive: RevivePolicy,
    /// Restarts this job's VM has had.
    restarts: u32,
    /// The state `list` last showed, for the revive decision.
    last: Option<VmState>,
    /// The checks the last `list` showed failed, once the VM is `verified`.
    failed: Option<Vec<Check>>,
    /// The wall proof found the probe got through.
    breached: bool,
    /// A job's run, not a verification's: a VM that stops on its own has its
    /// console kept as `job-<image>-failed-latest.log` ([`job_failed_log_path`]).
    job: bool,
    /// Set once GitHub may hand the runner a job: from then on a reclaim stops the
    /// VM only after withdrawing this ([`Self::reclaim_cleared`]).
    registration: Option<Registration>,
    /// The reclaim request standing now was refused: only one sent since is asked.
    refused: bool,
    /// When the JIT runner began inside the booted clone: where the run's
    /// `sandbox` phase ends (ADR-0056 §2).
    runner_started_ms: Option<u64>,
}

impl<S: VmSupervisor, W: WallProof, M: JitMint> Run<'_, S, W, M> {
    fn step(&self, step: VmStep) {
        self.env.jobs.set_step(&self.claim, step);
    }

    /// Poll `list` until this claim's VM is in a state `until` accepts. A VM that
    /// stops or vanishes ends the wait; `limit` of zero is no deadline.
    ///
    /// A supervisor that does not answer ends it only after
    /// [`VmTimeouts::supervisor_gap`](crate::backend::VmTimeouts::supervisor_gap):
    /// a restarted supervisor leaves the VM running and adopts it again by its
    /// claim marker, so its next `list` is the definitive answer, the VM listed
    /// (the wait goes on) or not (gone). Until then the job holds its claim, so
    /// the sweep leaves the VM alone ([`super::adopt::sweep_action`]).
    ///
    /// A reclaim ends the wait as [`Wait::Cancelled`] once
    /// [`Self::reclaim_cleared`] lets it. One it refuses ends nothing: the
    /// deadline, the gap and a stop in progress are timed on, as if it never came.
    async fn wait_for(
        &mut self,
        until: fn(VmState) -> bool,
        limit: Duration,
        stopping_ok: bool,
    ) -> Wait {
        let deadline = (!limit.is_zero()).then(|| Instant::now() + limit);
        let mut gap: Option<Instant> = None;
        // A VM stopping on its own: since when, so its `stopped` (which carries the
        // supervisor's reason) is waited for, at most `timeouts.stop`.
        let mut stopping: Option<Instant> = None;
        loop {
            if self.reclaim_cleared().await {
                return Wait::Cancelled;
            }
            match self.env.supervisor.list().await {
                Err(
                    e @ (SupervisorError::Unreachable
                    | SupervisorError::Misconfigured
                    | SupervisorError::NoReply),
                ) => {
                    let since = *gap.get_or_insert_with(|| {
                        tracing::warn!(
                            "vm {}: the VM supervisor does not answer ({e}); riding it out for \
                             up to {:?}, a restart keeps the VM",
                            self.claim.as_str(),
                            self.timeouts.supervisor_gap
                        );
                        Instant::now()
                    });
                    if since.elapsed() >= self.timeouts.supervisor_gap {
                        return Wait::Failed(e);
                    }
                }
                Err(e) => return Wait::Failed(e),
                Ok(vms) => match vms.into_iter().find(|v| v.claim == self.claim) {
                    None => return Wait::Gone,
                    Some(v) => {
                        if let Some(since) = gap.take() {
                            tracing::info!(
                                "vm {}: the VM supervisor answers again after {:?} and still \
                                 runs the VM",
                                self.claim.as_str(),
                                since.elapsed()
                            );
                        }
                        self.last = Some(v.state);
                        self.failed = v.failed.clone();
                        match v.state {
                            VmState::Starting => self.step(VmStep::Cloning),
                            VmState::Booting => self.step(VmStep::Booting),
                            _ => {}
                        }
                        if until(v.state) {
                            return Wait::Reached;
                        }
                        match v.state {
                            VmState::Stopped => return Wait::Stopped(v.stop_reason),
                            // A stop we asked for passes through `stopping`; the
                            // slot is free only at `stopped`. `unmanaged` holds its
                            // slot until its process exits, so it is never "gone".
                            VmState::Stopping if stopping_ok => {}
                            // Not asked for: the reason comes with `stopped`, a
                            // teardown later. Wait for it, briefly, so the log
                            // says why (an agent error, the wall, the guest).
                            VmState::Stopping => {
                                let since = *stopping.get_or_insert_with(Instant::now);
                                if since.elapsed() >= self.timeouts.stop {
                                    return Wait::Stopped(None);
                                }
                                self.nap(self.timeouts.poll).await;
                                continue;
                            }
                            VmState::Unmanaged => return Wait::Stopped(None),
                            _ => {}
                        }
                    }
                },
            }
            let nap = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Wait::TimedOut;
                    }
                    left.min(self.timeouts.poll)
                }
                None => self.timeouts.poll,
            };
            self.nap(nap).await;
        }
    }

    /// Sleep for `d`, or until a reclaim request [`Self::reclaim_cleared`] has yet
    /// to answer arrives.
    async fn nap(&mut self, d: Duration) {
        tokio::select! {
            biased;
            _ = requested(&mut self.cancel, self.refused) => self.refused = false,
            _ = sleep(d) => {}
        }
    }

    /// Whether a reclaim ends the run now: a request not yet answered has come,
    /// and is cleared.
    ///
    /// Before `begin` no job can be in the VM, so any request is cleared. After
    /// it the runner may be running one, and not necessarily its claim's: a JIT
    /// runner is handed the oldest queued job its labels match. The reconciler
    /// decides from GitHub's view of the runner, read a cycle earlier, and that
    /// view has been wrong in exactly the direction that kills a running job. So
    /// the request is cleared as ADR-0017 clears a Docker one, by withdrawing the
    /// runner's registration. GitHub refuses that while it has a job assigned to
    /// the runner, checked when the request arrives, and once it agrees it can
    /// assign the runner nothing more, so the answer holds until the stop lands.
    ///
    /// The withdrawal is the whole clearance. A Docker reclaim also reads the
    /// runner's process table before and after it
    /// ([`crate::docker::reclaim_clearance`]); a guest's cannot be read from here,
    /// so the VM path deliberately clears on the withdrawal alone. That is narrower
    /// than ADR-0017's clearance: it keeps the window step 3's second look closes,
    /// a job accepted just before the withdrawal lands. A refusal, or a withdrawal
    /// that failed, ends nothing: the job runs on, and the reconciler's next
    /// request is asked afresh.
    async fn reclaim_cleared(&mut self) -> bool {
        if self.refused || !*self.cancel.borrow_and_update() {
            return false;
        }
        let Some(registration) = &self.registration else {
            return true;
        };
        let why = match self
            .env
            .mint
            .withdraw(&registration.runner, registration.id, RECLAIM_WITHDRAWAL)
            .await
        {
            Withdrawal::Withdrawn => {
                tracing::info!(
                    "macos-vm {}: reclaimed in service, its runner withdrawn from GitHub (no \
                     job assigned to it) — stopping the VM",
                    self.claim.as_str()
                );
                return true;
            }
            Withdrawal::JobAssigned => "GitHub has a job assigned to its runner".to_string(),
            Withdrawal::Failed(e) => {
                format!("its runner's registration could not be withdrawn: {e}")
            }
        };
        tracing::info!(
            "macos-vm {}: not reclaiming — {why}; its job keeps running",
            self.claim.as_str()
        );
        self.refused = true;
        false
    }

    /// A reclaim after the mint and before `begin` (#269). No runner was started
    /// under the registration, so no job can be running in the VM, which is
    /// stopped whatever GitHub answers. Withdrawing the registration now spares
    /// the pool a runner counted as connecting supply until the stale-runner sweep
    /// removes it (ADR-0018).
    async fn withdraw_unused(&self, runner_name: &str, runner_id: Option<u64>) {
        let claim = self.claim.as_str();
        match self
            .env
            .mint
            .withdraw(runner_name, runner_id, UNUSED_WITHDRAWAL)
            .await
        {
            Withdrawal::Withdrawn => tracing::info!(
                "macos-vm {claim}: reclaimed after the JIT mint — its unused registration is \
                 withdrawn"
            ),
            Withdrawal::JobAssigned => tracing::warn!(
                "macos-vm {claim}: reclaimed after the JIT mint, and GitHub reports a job \
                 assigned to a runner that never started — the stale-runner sweep removes its \
                 registration"
            ),
            Withdrawal::Failed(e) => tracing::warn!(
                "macos-vm {claim}: reclaimed after the JIT mint, and its registration could not \
                 be withdrawn ({e}) — the stale-runner sweep removes it"
            ),
        }
    }

    /// Steps 1-3: the VM boots to the runner console and proves the wall. Never
    /// stops the VM. A job and a verification (ADR-0049 §7) share it.
    async fn prove(&mut self) -> Result<(), End> {
        // Steps 1-2: the supervisor clones and boots; wait for the runner console.
        // A VM still starting or booting at the deadline is wedged, and may be
        // restarted on a fresh clone (ADR-0007 retargeted): no job code has run.
        loop {
            let since = Instant::now();
            match self
                .wait_for(|s| s == VmState::Ready, self.timeouts.boot, false)
                .await
            {
                Wait::Reached => break,
                Wait::TimedOut => {
                    let state = self.last.unwrap_or(VmState::Starting);
                    let decision = revive_decision(
                        state,
                        since.elapsed(),
                        self.timeouts.boot,
                        self.restarts,
                        &self.revive,
                    );
                    if decision != Revive::Restart || self.start.is_none() {
                        return Err(self.ended_before("reach the runner console", Wait::TimedOut));
                    }
                    self.restart(state).await?;
                }
                other => return Err(self.ended_before("reach the runner console", other)),
            }
        }

        // Step 3: the wall. The proof takes its baseline before the probe, and
        // is disarmed on every way out of this step.
        self.step(VmStep::ProvingWall);
        let env = self.env;
        let disarm = Disarm::new(&env.wall, &self.claim);
        self.env.wall.arm(self.slot, &self.claim).await;
        match self.env.supervisor.probe(&self.claim).await {
            Ok(r) if r.state == VmState::ProbeSent => {}
            Ok(r) => {
                return Err(End::Failed(anyhow!(
                    "probe left the VM in state {:?}",
                    r.state
                )));
            }
            Err(e) => return Err(End::Failed(anyhow!(e).context("sending the wall probe"))),
        }
        let deadline = Instant::now() + self.timeouts.probe;
        loop {
            match self.env.wall.verdict(self.slot, &self.claim).await {
                WallVerdict::Held => break,
                WallVerdict::Breached => {
                    self.breached = true;
                    return Err(End::Failed(anyhow!(
                        "the wall did not stop the guest's probe — no job runs in this VM"
                    )));
                }
                WallVerdict::Unreadable => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(End::Failed(anyhow!(
                    "the wall verdict could not be read within {:?} — failing closed",
                    self.timeouts.probe
                )));
            }
            tokio::select! {
                biased;
                _ = cancelled(&mut self.cancel) => return Err(End::Reclaimed("during the wall proof")),
                _ = sleep(left.min(self.timeouts.poll)) => {}
            }
        }
        drop(disarm);
        Ok(())
    }

    /// Steps 1-5: everything up to and including the job. Never stops the VM.
    async fn drive(&mut self, runner_name: &str, labels: &[String]) -> End {
        if let Err(end) = self.prove().await {
            return end;
        }

        // Step 4: mint only now that the wall holds.
        self.step(VmStep::StartingRunner);
        if *self.cancel.borrow() {
            return End::Reclaimed("before the JIT mint");
        }
        let minted = match self.env.mint.mint(runner_name, labels).await {
            Ok(m) => m,
            Err(e) => return End::Failed(e),
        };
        if *self.cancel.borrow() {
            self.withdraw_unused(runner_name, minted.runner_id).await;
            return End::Reclaimed("after the JIT mint");
        }
        match self
            .env
            .supervisor
            .begin(&self.claim, Some(minted.jit))
            .await
        {
            Ok(r) if r.state == VmState::InService => {
                self.runner_started_ms = Some(now_ms());
            }
            Ok(r) => return End::Failed(anyhow!("begin left the VM in state {:?}", r.state)),
            Err(e) => return End::Failed(anyhow!(e).context("starting the JIT runner")),
        }

        // Step 5: the job. GitHub may hand the runner one from here, so a reclaim
        // has to withdraw it first.
        self.registration = Some(Registration {
            runner: runner_name.to_string(),
            id: minted.runner_id,
        });
        self.step(VmStep::Running);
        match self
            .wait_for(|s| s == VmState::JobFinished, self.timeouts.job, false)
            .await
        {
            Wait::Reached => End::Finished,
            Wait::TimedOut => End::JobTimedOut,
            Wait::Cancelled => End::Reclaimed(RECLAIMED_IN_SERVICE),
            other => self.ended_before("finish the job", other),
        }
    }

    /// After [`Self::prove`]: send `verify` and wait for `verified`, at most
    /// `limit`. The failed checks are the supervisor's fixed codes, never the
    /// guest's words.
    async fn check(&mut self, expect: Expect, limit: Duration) -> VerifyEnd {
        self.step(VmStep::Verifying);
        if *self.cancel.borrow() {
            return VerifyEnd::Cancelled;
        }
        match self.env.supervisor.verify(&self.claim, expect).await {
            Ok(r) if r.state == VmState::Verifying => {}
            Ok(r) => {
                return VerifyEnd::Failed(anyhow!("verify left the VM in state {:?}", r.state))
            }
            Err(e) => return VerifyEnd::Failed(anyhow!(e).context("asking the guest to verify")),
        }
        match self
            .wait_for(|s| s == VmState::Verified, limit, false)
            .await
        {
            Wait::Reached => match self.failed.take() {
                Some(failed) => VerifyEnd::Checked(failed),
                None => VerifyEnd::Failed(anyhow!("the VM reported verified with no result")),
            },
            Wait::Cancelled => VerifyEnd::Cancelled,
            other => match self.ended_before("finish its checks", other) {
                End::Failed(e) => VerifyEnd::Failed(e),
                _ => VerifyEnd::Cancelled,
            },
        }
    }

    /// Stop a wedged VM, wait for `stopped`, and start the same claim in the same
    /// slot on a fresh clone. A reclaim during it ends the run as reclaimed, and
    /// [`Self::teardown`] still stops whatever is left.
    async fn restart(&mut self, state: VmState) -> Result<(), End> {
        let Some(start) = self.start.clone() else {
            return Err(End::Failed(anyhow!("an adopted VM is never restarted")));
        };
        self.restarts += 1;
        tracing::warn!(
            "macos-vm {}: still {state:?} after {:?} — restarting it on a fresh clone \
             (restart {} of {})",
            self.claim.as_str(),
            self.timeouts.boot,
            self.restarts,
            self.revive.max_restarts
        );
        self.step(VmStep::Stopping);
        tracing::info!(
            "vm job: asking the VM supervisor to stop {}",
            self.claim.as_str()
        );
        match self.env.supervisor.stop(&self.claim).await {
            // Already gone: nothing to wait for.
            Ok(()) | Err(SupervisorError::Refused(ErrorCode::UnknownClaim)) => {}
            Err(e) => {
                return Err(End::Failed(
                    anyhow!(e).context("stopping a wedged VM to restart it"),
                ));
            }
        }
        match self
            .wait_for(|s| s == VmState::Stopped, self.timeouts.stop, true)
            .await
        {
            Wait::Reached | Wait::Gone => {}
            Wait::Cancelled => return Err(End::Reclaimed("while restarting a wedged VM")),
            other => return Err(self.ended_before("stop for its restart", other)),
        }
        if *self.cancel.borrow() {
            return Err(End::Reclaimed("while restarting a wedged VM"));
        }
        self.step(VmStep::Cloning);
        self.last = None;
        match self.env.supervisor.start(start).await {
            Ok(_) => Ok(()),
            Err(e) => Err(End::Failed(anyhow!(e).context("restarting a wedged VM"))),
        }
    }

    fn ended_before(&self, what: &str, w: Wait) -> End {
        match w {
            Wait::Cancelled => End::Reclaimed("while waiting for the VM to progress"),
            Wait::TimedOut => End::Failed(anyhow!("the VM did not {what} in time")),
            Wait::Stopped(Some(r)) => {
                // The guest agent's closed word that the runner never took a job: not a
                // VM that broke, so it is said as what happened (#392).
                let no_job = r == StopReason::RunnerExitedWithoutJob;
                let mut msg = if no_job {
                    format!(
                        "the JIT runner exited without taking a job (stop_reason={})",
                        r.as_str()
                    )
                } else {
                    format!(
                        "the VM stopped (stop_reason={}) before it could {what}",
                        r.as_str()
                    )
                };
                if self.job && r != StopReason::Requested {
                    let image = self.start.as_ref().map(|s| s.image.as_str());
                    let lines = if no_job {
                        "the runner's last lines"
                    } else {
                        "a line per hand-off step"
                    };
                    msg.push_str(&format!(
                        "; the guest's console, with {lines}, is kept: `{}`",
                        job_failed_log_command(image.unwrap_or("<image>"))
                    ));
                }
                End::Failed(anyhow!(msg))
            }
            Wait::Stopped(None) => End::Failed(anyhow!("the VM stopped before it could {what}")),
            Wait::Gone => End::Failed(anyhow!("the supervisor no longer lists the VM")),
            Wait::Failed(e) => {
                End::Failed(anyhow!(e).context(format!("waiting for the VM to {what}")))
            }
            Wait::Reached => End::Failed(anyhow!("unexpected state")),
        }
    }

    /// Step 6: force-stop, wait for `stopped`, free the slot. `true` when the VM is
    /// confirmed gone.
    async fn teardown(&mut self) -> bool {
        self.step(VmStep::Stopping);
        tracing::info!(
            "vm job: asking the VM supervisor to stop {}",
            self.claim.as_str()
        );
        match self.env.supervisor.stop(&self.claim).await {
            Ok(()) => {}
            // No VM carries the claim: there is nothing to stop.
            Err(SupervisorError::Refused(ErrorCode::UnknownClaim)) => return self.gone(),
            Err(e) => {
                tracing::warn!("macos-vm {}: stop not confirmed ({e})", self.claim.as_str());
                return self.orphaned();
            }
        }
        // A reclaim must not cut short the wait for the clone's deletion.
        let (_keep, mut never) = watch::channel(false);
        std::mem::swap(&mut self.cancel, &mut never);
        let waited = self
            .wait_for(|s| s == VmState::Stopped, self.timeouts.stop, true)
            .await;
        std::mem::swap(&mut self.cancel, &mut never);
        match waited {
            Wait::Reached | Wait::Gone => self.gone(),
            _ => {
                tracing::warn!(
                    "macos-vm {}: the VM was not confirmed stopped within {:?}",
                    self.claim.as_str(),
                    self.timeouts.stop
                );
                self.orphaned()
            }
        }
    }

    fn gone(&self) -> bool {
        self.step(VmStep::Deleted);
        self.env.jobs.release(&self.claim);
        true
    }

    fn orphaned(&self) -> bool {
        self.env.jobs.quarantine(&self.claim);
        false
    }
}

/// Where the macOS VM supervisor keeps the guest console of `image`'s latest job
/// VM that stopped without finishing and without faber asking: the agent's
/// `FABER-AGENT handoff` line per step of starting the runner
/// (docs/vm-agent-protocol.md, *The hand-off's console lines*). Like the
/// verification's ([`super::verify::verify_log_path`]), it is in the session
/// account's state directory, readable by root and that account only, and faber
/// never reads it. Pure.
pub fn job_failed_log_path(image: &str) -> std::path::PathBuf {
    std::path::Path::new(crate::platform::layout::FABER_MAC_HOME)
        .join("state")
        .join(format!("job-{image}-failed-latest.log"))
}

/// The operator's command to read [`job_failed_log_path`]: `cat -v`, so no guest
/// byte reaches the terminal as a control sequence. Display only. Pure.
pub fn job_failed_log_command(image: &str) -> String {
    format!("sudo cat -v {}", job_failed_log_path(image).display())
}

/// Provision a macOS VM for one job and run it to completion (ADR-0026).
///
/// Returns a report for a finished, reclaimed or timed-out job, and an error for
/// faber's own infrastructure failing; the VM is force-stopped on every path.
///
/// `admission` is the limiter's count of this job ([`Limiter::vm_admit`]), held
/// until teardown has answered and released on every return. A VM teardown could
/// not confirm gone is quarantined first, and counts as held from then on
/// ([`VmJobs::quarantined`]), so the class cap never sees a gap.
///
/// [`Limiter::vm_admit`]: crate::limiter::Limiter::vm_admit
pub async fn run_one<S: VmSupervisor, W: WallProof, M: JitMint>(
    env: &VmEnv<'_, S, W, M>,
    admission: VmRun,
    runner_name: &str,
    labels: &[String],
    cfg: &MacosVmConfig,
    tools: &ImageName,
    cancel: watch::Receiver<bool>,
) -> anyhow::Result<RunReport> {
    let image = Some(cfg.image.as_str().to_string());
    if *cancel.borrow() {
        return Ok(reclaimed("before the clone", image));
    }
    let claim = ClaimId::try_from(claim_id(runner_name))
        .map_err(|e| anyhow!("runner name {runner_name:?} is not a VM claim id: {e}"))?;
    // Until boot adoption has said which slots the supervisor already runs a job
    // in, a lease could hand one out twice. Nothing is minted yet, so GitHub's job
    // stays queued and is claimed again.
    if env.jobs.awaiting_adoption() {
        bail!(
            "the VMs already running have not been adopted yet (the VM supervisor has not \
             answered since faber started) — no new VM starts until it does"
        );
    }

    let mut run = match launch(env, claim, cfg, tools, cancel).await {
        Ok(run) => run,
        Err(e) => {
            // `start` never reached the session's supervisor (#393): one setup is
            // restarting, or a session that went away after the gate's last probe.
            // Nothing started and nothing was minted, so GitHub's job is still
            // queued: the gate closes until a handshake answers again, and the
            // claim is given back rather than counted as a fault.
            if never_reached(&e) && admission.session_lost(format!("{e:#}")) {
                return Ok(reclaimed_cause(
                    format!("{e:#}, so no VM started and the job is claimed again"),
                    image,
                ));
            }
            return Err(e);
        }
    };
    run.job = true;
    let end = run.drive(runner_name, labels).await;
    let confirmed = run.teardown().await;
    report(end, confirmed, image, run.timeouts.job).map(|r| RunReport {
        container_started_ms: run.runner_started_ms,
        ..r
    })
}

/// Whether `e` is a request that never reached the supervisor
/// ([`SupervisorError::Unreachable`]), through whatever context was added to it.
fn never_reached(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<SupervisorError>(),
        Some(SupervisorError::Unreachable)
    )
}

/// How a verification run ended (ADR-0049 §7), before faber records it.
#[derive(Debug)]
pub enum VerifyEnd {
    /// The guest ran every check: the ones that failed, in [`Check::ALL`]'s order.
    /// Empty is a pass.
    Checked(Vec<Check>),
    /// The wall did not stop the guest's probe.
    WallBreached,
    /// Reclaimed before the checks finished: nothing is learnt.
    Cancelled,
    /// faber's own path failed first (boot, the proof's deadline, the supervisor),
    /// so nothing is learnt about the image.
    Failed(anyhow::Error),
}

/// Boot one clone of `cfg.image` exactly as a job does, under `claim`, prove the
/// wall, and have the guest run the image's checks against `expect` instead of
/// a job (ADR-0049 §7). No JIT config is minted or sent. The VM is force-stopped
/// on every path, and `admission`, one of the macOS places, is held until then.
///
/// `limit` bounds the checks themselves, from `verify` to `verified`.
#[allow(clippy::too_many_arguments)]
pub async fn run_verify<S: VmSupervisor, W: WallProof, M: JitMint>(
    env: &VmEnv<'_, S, W, M>,
    admission: VmRun,
    claim: ClaimId,
    cfg: &MacosVmConfig,
    tools: &ImageName,
    expect: Expect,
    limit: Duration,
    cancel: watch::Receiver<bool>,
) -> VerifyEnd {
    let _admission = admission;
    if env.jobs.awaiting_adoption() {
        return VerifyEnd::Failed(anyhow!(
            "the VMs already running have not been adopted yet — no new VM starts until they are"
        ));
    }
    let mut run = match launch(env, claim, cfg, tools, cancel).await {
        Ok(run) => run,
        Err(e) => return VerifyEnd::Failed(e),
    };
    let end = match run.prove().await {
        Err(_) if run.breached => VerifyEnd::WallBreached,
        Err(End::Reclaimed(_)) => VerifyEnd::Cancelled,
        Err(End::Failed(e)) => VerifyEnd::Failed(e),
        Err(_) => VerifyEnd::Failed(anyhow!("the VM did not reach its checks")),
        Ok(()) => run.check(expect, limit).await,
    };
    run.teardown().await;
    end
}

/// Lease the lowest free slot for `claim` and `start` its VM there: a clone of
/// `cfg.image` with `tools` as its second disk. A supervisor that says a slot is
/// busy holds a VM faber does not know about; that slot is set aside and the next
/// tried. On an error nothing is left leased, and a VM a protocol break may have
/// started is stopped.
async fn launch<'a, S: VmSupervisor, W: WallProof, M: JitMint>(
    env: &'a VmEnv<'a, S, W, M>,
    claim: ClaimId,
    cfg: &MacosVmConfig,
    tools: &ImageName,
    cancel: watch::Receiver<bool>,
) -> anyhow::Result<Run<'a, S, W, M>> {
    let mut skip = 0u16;
    let (slot, request) = loop {
        let Some(slot) = env.jobs.lease(&claim, skip) else {
            bail!("no free VM slot: every slot is held or set aside");
        };
        let request = Start {
            slot,
            kind: VmKind::Macos,
            claim: claim.clone(),
            image: cfg.image.clone(),
            tools: Some(tools.clone()),
            cpus: cfg.cpus,
            memory_mib: cfg.memory_mib,
            disk_bytes: None,
        };
        match env.supervisor.start(request.clone()).await {
            Ok(_) => break (slot, request),
            Err(SupervisorError::Refused(ErrorCode::SlotBusy)) => {
                skip |= 1 << slot.get();
                env.jobs.release(&claim);
            }
            // A definite refusal: nothing was started, nothing to stop.
            Err(SupervisorError::Refused(code)) => {
                env.jobs.release(&claim);
                bail!("the VM supervisor refused to start the VM: {code:?}");
            }
            // Never sent, so nothing was started either.
            Err(e @ (SupervisorError::Unreachable | SupervisorError::Misconfigured)) => {
                env.jobs.release(&claim);
                return Err(anyhow!(e).context("starting the VM"));
            }
            // A protocol break: the request may have been seen. Stop whatever it started.
            Err(e) => {
                let mut run = Run {
                    env,
                    claim: claim.clone(),
                    slot,
                    timeouts: cfg.timeouts,
                    cancel,
                    start: None,
                    revive: RevivePolicy::OFF,
                    restarts: 0,
                    last: None,
                    failed: None,
                    breached: false,
                    job: false,
                    registration: None,
                    refused: false,
                    runner_started_ms: None,
                };
                run.teardown().await;
                return Err(anyhow!(e).context("starting the VM"));
            }
        }
    };

    Ok(Run {
        env,
        claim,
        slot,
        timeouts: cfg.timeouts,
        cancel,
        start: Some(request),
        revive: cfg.revive,
        restarts: 0,
        last: None,
        failed: None,
        breached: false,
        job: false,
        registration: None,
        refused: false,
        runner_started_ms: None,
    })
}

/// Watch a VM boot adoption found running a job (`in_service`) through to its
/// end, then stop it and free its slot, exactly as [`run_one`]'s step 5 and 6
/// would have (ADR-0011 for VMs). The caller has already marked `slot` held
/// ([`VmJobs::adopt_live`]).
///
/// The job cap counts from `started_unix`, the supervisor's own start time, so a
/// restart does not hand a job a fresh cap. A reclaim on `cancel` is cleared as
/// one in [`run_one`]'s step 5 is, by withdrawing the runner through `env.mint`
/// first (ADR-0017): in production by the name faber's own `mint` record gives
/// it ([`watched_env`]), since its id died with the process that minted it.
///
/// `admission` is the adopted VM's count ([`Limiter::vm_adopt`]), held as
/// [`run_one`] holds its own.
///
/// [`Limiter::vm_adopt`]: crate::limiter::Limiter::vm_adopt
pub async fn watch_adopted<S: VmSupervisor, W: WallProof, M: JitMint>(
    env: &VmEnv<'_, S, W, M>,
    admission: VmRun,
    claim: ClaimId,
    slot: Slot,
    timeouts: crate::backend::VmTimeouts,
    started_unix: u64,
    cancel: watch::Receiver<bool>,
) -> anyhow::Result<RunReport> {
    let _admission = admission;
    // Its job has begun, so GitHub may have handed the runner one.
    let registration = Registration {
        runner: format!("faber-{}", claim.as_str()),
        id: None,
    };
    let mut run = Run {
        env,
        claim,
        slot,
        timeouts,
        cancel,
        start: None,
        revive: RevivePolicy::OFF,
        restarts: 0,
        last: None,
        failed: None,
        breached: false,
        job: true,
        registration: Some(registration),
        refused: false,
        runner_started_ms: None,
    };
    let left = if timeouts.job.is_zero() {
        Duration::ZERO
    } else {
        let ran = Duration::from_secs((now_ms() / 1000).saturating_sub(started_unix));
        timeouts.job.saturating_sub(ran).max(Duration::from_secs(1))
    };
    let end = match run
        .wait_for(|s| s == VmState::JobFinished, left, false)
        .await
    {
        Wait::Reached => End::Finished,
        Wait::TimedOut => End::JobTimedOut,
        Wait::Cancelled => End::Reclaimed(RECLAIMED_IN_SERVICE),
        other => run.ended_before("finish the job", other),
    };
    let confirmed = run.teardown().await;
    report(end, confirmed, None, timeouts.job)
}

/// The report for a run that ended in `end`, once teardown said whether its VM
/// is `confirmed` gone.
fn report(
    end: End,
    confirmed: bool,
    image: Option<String>,
    job_cap: Duration,
) -> anyhow::Result<RunReport> {
    let orphan = |msg: String| {
        if confirmed {
            msg
        } else {
            format!("{msg}; the VM was not confirmed stopped, so its slot stays held")
        }
    };

    match end {
        End::Finished => Ok(RunReport {
            job_ok: true,
            cause: (!confirmed).then(|| orphan("the job finished".into())),
            image,
            ..Default::default()
        }),
        End::Reclaimed(at) => Ok(reclaimed_cause(orphan(format!("reclaimed {at}")), image)),
        End::JobTimedOut => Ok(RunReport {
            timed_out: true,
            cause: Some(orphan(format!(
                "the job outlived its {job_cap:?} cap and its VM was force-stopped"
            ))),
            image,
            ..Default::default()
        }),
        End::Failed(e) => Err(if confirmed {
            e
        } else {
            e.context("the VM was not confirmed stopped, so its slot stays held")
        }),
    }
}

fn reclaimed(at: &str, image: Option<String>) -> RunReport {
    reclaimed_cause(format!("reclaimed {at}"), image)
}

fn reclaimed_cause(cause: String, image: Option<String>) -> RunReport {
    RunReport {
        reclaimed: true,
        cause: Some(cause),
        image,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests, against a fake supervisor
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0051: `_faber_vm`'s supervisor is reached only by the engine (which
    /// starts the Linux VM alone) and the updater (which only asks it to
    /// exit). Every macOS VM path goes through [`ProductionSupervisor::for_macos_jobs`], so no code
    /// can ask `_faber_vm`'s supervisor to start a macOS job once the session
    /// account is recorded. A new caller of the system client fails this.
    #[test]
    fn only_the_engine_and_the_updater_reach_faber_vms_supervisor() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        let mut callers = std::collections::BTreeSet::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            let rel = f.strip_prefix(&src).unwrap().display().to_string();
            for needle in ["ProductionSupervisor::for_this_host()", "process_client()"] {
                if text.contains(needle) {
                    callers.insert(rel.clone());
                }
            }
        }
        let want: std::collections::BTreeSet<String> = ["update.rs", "vm/engine.rs", "vm/job.rs"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(callers, want);
        // In job.rs itself, only the engine's constructor and the macOS route
        // for a record without the session account use it; with the account
        // the macOS route is the session client or nothing.
        let job = include_str!("job.rs");
        let code = &job[..job.find("#[cfg(test)]\nmod tests").unwrap()];
        let calls: Vec<&str> = code
            .lines()
            .filter(|l| l.contains("process_client()") && !l.trim_start().starts_with("//"))
            .map(str::trim)
            .collect();
        assert_eq!(
            calls,
            vec![
                "Self::of(process_client())",
                "None => Self::of(process_client()),",
                "pub fn process_client() -> Option<&'static SupervisorClient> {"
            ]
        );
        assert!(code.contains(
            "Some(_) => match session_client() {\n                Some(client) => Self::Session(client),\n                None => Self::Unconnected(UnconnectedSupervisor),"
        ));
    }

    /// ADR-0051: macOS VM jobs go to the session account's supervisor where
    /// the record names the account, and to `_faber_vm`'s where it does not.
    #[test]
    fn macos_jobs_are_routed_by_the_record() {
        let mut record = crate::platform::record::SetupRecord::from_json(
            br#"{"schema": 1, "operator": {"uid": 501, "name": "owner"},
                "accounts": {"faber": {"uid": 451, "gid": 451},
                             "faber_vm": {"uid": 452, "gid": 452}},
                "installed": {"version": "0.4.0"}, "migrated_from": null,
                "artifacts": []}"#,
        )
        .unwrap();
        assert_eq!(
            session_endpoint(&record),
            None,
            "an older record keeps _faber_vm"
        );
        assert_eq!(
            macos_job_endpoint(&record),
            Endpoint {
                path: crate::platform::vm_supervisor_socket(),
                peer_uid: 452,
            }
        );
        let mut recorded = record.clone();
        recorded.accounts.faber_mac = Some(crate::platform::record::SessionAccount {
            uid: 600,
            gid: 20,
            reboot_mode: None,
        });
        assert_eq!(
            macos_job_endpoint(&recorded),
            Endpoint {
                path: "/var/db/faber-mac/run/supervisor.sock".into(),
                peer_uid: 600,
            },
            "never _faber_vm's once the session account is recorded"
        );
        record.accounts.faber_mac = Some(crate::platform::record::SessionAccount {
            uid: 600,
            gid: 20,
            reboot_mode: Some("manual".into()),
        });
        assert_eq!(
            session_endpoint(&record),
            Some(Endpoint {
                path: "/var/db/faber-mac/run/supervisor.sock".into(),
                peer_uid: 600,
            })
        );
    }
    use crate::backend::VmTimeouts;
    use crate::limiter::Limiter;
    use crate::vm::proto::{Cpus, ImageName, MemoryMib, UnixSecs};
    use std::sync::Arc;

    /// How the fake supervisor misbehaves.
    #[derive(Default)]
    struct Script {
        refuse_start: Option<ErrorCode>,
        /// `start` finds no supervisor to send to (one restarting, #393).
        start_unreachable: bool,
        /// `start` reaches a supervisor this faber cannot use (version skew).
        start_skewed: bool,
        /// Slots that answer `slot_busy`.
        busy_slots: Vec<u8>,
        /// Never advance past this state.
        stall_at: Option<VmState>,
        /// `in_service` goes on to `job_finished`.
        job_finishes: bool,
        /// After `begin`, the guest shuts down instead.
        guest_shuts_down: bool,
        /// After `begin`, the child stops the VM itself (an agent error): `stopping`
        /// for a few `list`s, then `stopped` with `agent_error`.
        agent_error_after_begin: bool,
        /// After `begin`, the runner exits without taking a job: the child stops the
        /// VM itself, `stopping` and then `stopped` with `runner_exited_without_job`.
        runner_exits_without_job: bool,
        /// `stop` is accepted but the VM never reaches `stopped`.
        stop_never_lands: bool,
        /// `stop` itself fails.
        stop_fails: bool,
        /// `list` reports `stopping` this many times before `stopped`.
        stopping_polls: u32,
        /// The first this-many VMs started stall at `booting` (a wedge).
        wedged_starts: u32,
        /// Once the job is in service, this many `list`s find no supervisor (a
        /// restart's gap); `u32::MAX` is one that never comes back.
        gap_lists: u32,
        /// What the guest's checks find failed, once `verify` is sent.
        verify_fails: Vec<Check>,
        /// `verifying` never reaches `verified`.
        verify_stalls: bool,
        /// GitHub's answers to the fake [`Mint`]'s withdrawals, in order; withdrawn
        /// once they run out.
        withdrawals: Vec<Withdrawal>,
    }

    #[derive(Default)]
    struct World {
        script: Script,
        vm: Option<VmRecord>,
        calls: Vec<String>,
        /// The audit reason of each withdrawal, in order.
        withdraw_reasons: Vec<String>,
        jit_seen: bool,
        stopping_seen: u32,
        starts: u32,
        gaps_seen: u32,
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
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().calls.clone()
        }
        fn count(&self, name: &str) -> usize {
            self.calls().iter().filter(|c| c.starts_with(name)).count()
        }
    }

    fn record(slot: Slot, claim: ClaimId, state: VmState, reason: Option<StopReason>) -> VmRecord {
        VmRecord {
            slot,
            kind: VmKind::Macos,
            claim,
            state,
            stop_reason: reason,
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
            assert_eq!(req.kind, VmKind::Macos);
            assert!(req.tools.is_some(), "a macOS start names its tools disk");
            if let Some(code) = w.script.refuse_start {
                return Err(SupervisorError::Refused(code));
            }
            if w.script.start_unreachable {
                return Err(SupervisorError::Unreachable);
            }
            if w.script.start_skewed {
                return Err(SupervisorError::Misconfigured);
            }
            if w.script.busy_slots.contains(&req.slot.get()) {
                return Err(SupervisorError::Refused(ErrorCode::SlotBusy));
            }
            let r = record(req.slot, req.claim, VmState::Starting, None);
            w.vm = Some(r.clone());
            w.starts += 1;
            Ok(r)
        }
        async fn probe(&self, claim: &ClaimId) -> Result<VmRecord, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("probe".into());
            let vm =
                w.vm.as_mut()
                    .filter(|v| &v.claim == claim)
                    .ok_or(SupervisorError::Refused(ErrorCode::UnknownClaim))?;
            if vm.state != VmState::Ready {
                return Err(SupervisorError::Refused(ErrorCode::WrongState));
            }
            vm.state = VmState::ProbeSent;
            Ok(vm.clone())
        }
        async fn begin(
            &self,
            claim: &ClaimId,
            jit: Option<JitConfig>,
        ) -> Result<VmRecord, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("begin".into());
            w.jit_seen = jit.is_some_and(|j| !j.expose().is_empty());
            let shut = w.script.guest_shuts_down;
            let agent_error = w.script.agent_error_after_begin || w.script.runner_exits_without_job;
            let vm =
                w.vm.as_mut()
                    .filter(|v| &v.claim == claim)
                    .ok_or(SupervisorError::Refused(ErrorCode::UnknownClaim))?;
            if vm.state != VmState::ProbeSent {
                return Err(SupervisorError::Refused(ErrorCode::WrongState));
            }
            vm.state = VmState::InService;
            let out = vm.clone();
            if shut {
                vm.state = VmState::Stopped;
                vm.stop_reason = Some(StopReason::GuestShutdown);
            }
            if agent_error {
                vm.state = VmState::Stopping;
            }
            Ok(out)
        }
        async fn verify(
            &self,
            claim: &ClaimId,
            expect: Expect,
        ) -> Result<VmRecord, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push(format!("verify:{}", expect.runtimes.len()));
            let vm =
                w.vm.as_mut()
                    .filter(|v| &v.claim == claim)
                    .ok_or(SupervisorError::Refused(ErrorCode::UnknownClaim))?;
            if vm.state != VmState::ProbeSent {
                return Err(SupervisorError::Refused(ErrorCode::WrongState));
            }
            vm.state = VmState::Verifying;
            Ok(vm.clone())
        }
        async fn stop(&self, claim: &ClaimId) -> Result<(), SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("stop".into());
            if w.script.stop_fails {
                return Err(SupervisorError::Unreachable);
            }
            let never = w.script.stop_never_lands;
            match w.vm.as_mut().filter(|v| &v.claim == claim) {
                None => Err(SupervisorError::Refused(ErrorCode::UnknownClaim)),
                Some(vm) => {
                    if vm.state != VmState::Stopped && !never {
                        vm.state = VmState::Stopping;
                    }
                    Ok(())
                }
            }
        }
        async fn list(&self) -> Result<Vec<VmRecord>, SupervisorError> {
            let mut w = self.0.lock().unwrap();
            w.calls.push("list".into());
            let in_service = w.vm.as_ref().is_some_and(|v| v.state == VmState::InService);
            if in_service && w.gaps_seen < w.script.gap_lists {
                w.gaps_seen += 1;
                return Err(if w.gaps_seen.is_multiple_of(2) {
                    SupervisorError::NoReply
                } else {
                    SupervisorError::Unreachable
                });
            }
            let stall = if w.starts <= w.script.wedged_starts {
                Some(VmState::Booting)
            } else {
                w.script.stall_at
            };
            let finishes = w.script.job_finishes;
            let verdict = (!w.script.verify_stalls).then(|| w.script.verify_fails.clone());
            let stop_never = w.script.stop_never_lands;
            let lingering = w.script.stopping_polls > w.stopping_seen;
            let reason = if w.script.agent_error_after_begin {
                StopReason::AgentError
            } else if w.script.runner_exits_without_job {
                StopReason::RunnerExitedWithoutJob
            } else {
                StopReason::Requested
            };
            let mut bump = false;
            if let Some(vm) = w.vm.as_mut() {
                if Some(vm.state) != stall {
                    vm.state = match vm.state {
                        VmState::Starting => VmState::Booting,
                        VmState::Booting => VmState::Ready,
                        VmState::InService if finishes => VmState::JobFinished,
                        VmState::Verifying if verdict.is_some() => {
                            vm.failed = verdict;
                            VmState::Verified
                        }
                        VmState::Stopping if !stop_never && lingering => {
                            bump = true;
                            VmState::Stopping
                        }
                        VmState::Stopping if !stop_never => {
                            vm.stop_reason = Some(reason);
                            vm.failed = None;
                            VmState::Stopped
                        }
                        s => s,
                    };
                }
            }
            if bump {
                w.stopping_seen += 1;
            }
            Ok(w.vm.iter().cloned().collect())
        }
        async fn adopt(&self, _: &[ClaimId]) -> Result<Adopted, SupervisorError> {
            unreachable!("a job never adopts")
        }
    }

    struct Wall(WallVerdict);
    impl WallProof for Wall {
        async fn arm(&self, _: Slot, _: &ClaimId) {}
        async fn verdict(&self, _: Slot, _: &ClaimId) -> WallVerdict {
            self.0
        }
    }

    /// The fake GitHub side of a job's runner. A withdrawal is answered from the
    /// script ([`Script::withdrawals`]) and logged on the supervisor's wire, so one
    /// list orders it against `begin` and `stop`.
    struct Mint {
        calls: Arc<Mutex<u32>>,
        fail: bool,
        wire: Fake,
        /// Sent once the mint has answered: a reclaim that lands while it was in
        /// flight.
        reclaim_on_mint: Option<watch::Sender<bool>>,
    }
    impl JitMint for Mint {
        async fn mint(&self, _: &str, _: &[String]) -> anyhow::Result<Minted> {
            *self.calls.lock().unwrap() += 1;
            if self.fail {
                bail!("GitHub said no");
            }
            if let Some(tx) = &self.reclaim_on_mint {
                tx.send_replace(true);
            }
            Ok(Minted {
                jit: JitConfig::try_from("ZmFrZS1qaXQ=").unwrap(),
                runner_id: Some(7),
            })
        }
        async fn withdraw(
            &self,
            runner_name: &str,
            runner_id: Option<u64>,
            reason: &str,
        ) -> Withdrawal {
            let mut w = self.wire.0.lock().unwrap();
            w.calls
                .push(format!("withdraw:{runner_name}:{runner_id:?}"));
            w.withdraw_reasons.push(reason.to_string());
            if w.script.withdrawals.is_empty() {
                Withdrawal::Withdrawn
            } else {
                w.script.withdrawals.remove(0)
            }
        }
    }

    /// The tools disk a job was handed, as `tools_disk::for_this_host` names it.
    fn tools() -> ImageName {
        ImageName::try_from("tools-1").unwrap()
    }

    fn cfg() -> MacosVmConfig {
        MacosVmConfig {
            image: ImageName::try_from("macos-27").unwrap(),
            runner_version: "2.330.0".into(),
            cpus: Cpus::try_from(4).unwrap(),
            memory_mib: MemoryMib::try_from(8192).unwrap(),
            timeouts: VmTimeouts {
                boot: Duration::from_secs(30),
                probe: Duration::from_secs(10),
                job: Duration::from_secs(60),
                stop: Duration::from_secs(10),
                poll: Duration::from_secs(1),
                supervisor_gap: Duration::from_secs(20),
            },
            // The flow before revival existed; the revive tests opt in.
            revive: RevivePolicy::OFF,
        }
    }

    /// A limiter on faber's default tuning, for the admission a job is handed.
    fn limiter() -> Arc<Limiter> {
        let tuning =
            crate::config::AutoLimitFile::default().resolve(&crate::config::Overrides::default());
        Limiter::new(tuning, &std::collections::HashMap::new())
    }

    /// An admission no test reads back.
    fn admission() -> VmRun {
        limiter().vm_adopt()
    }

    struct Rig {
        fake: Fake,
        jobs: VmJobs,
        mints: Arc<Mutex<u32>>,
        /// Whose count each job run through the rig holds.
        limiter: Arc<Limiter>,
    }

    impl Rig {
        fn new(script: Script) -> Self {
            Self {
                fake: Fake::with(script),
                jobs: VmJobs::default(),
                mints: Arc::new(Mutex::new(0)),
                limiter: limiter(),
            }
        }
        /// An admission counted in the rig's limiter. `vm_adopt` rather than
        /// `vm_begin`: the gates are the limiter's tests; this is what holds one.
        fn admitted(&self) -> VmRun {
            self.limiter.vm_adopt()
        }
        /// The macOS VM jobs the rig's limiter counts.
        fn counted(&self) -> usize {
            self.limiter.snapshot().vm.macos_running
        }
        /// The fake GitHub side, logging on the rig's wire.
        fn mint(&self, fail: bool) -> Mint {
            Mint {
                calls: self.mints.clone(),
                fail,
                wire: self.fake.clone(),
                reclaim_on_mint: None,
            }
        }
        async fn run(
            &self,
            wall: WallVerdict,
            mint_fails: bool,
            cancel: watch::Receiver<bool>,
        ) -> anyhow::Result<RunReport> {
            self.run_with(&cfg(), wall, mint_fails, cancel).await
        }
        async fn run_with(
            &self,
            cfg: &MacosVmConfig,
            wall: WallVerdict,
            mint_fails: bool,
            cancel: watch::Receiver<bool>,
        ) -> anyhow::Result<RunReport> {
            let env = VmEnv {
                supervisor: self.fake.clone(),
                wall: Wall(wall),
                mint: self.mint(mint_fails),
                jobs: &self.jobs,
            };
            run_one(
                &env,
                self.admitted(),
                "faber-42-1760000000",
                &["faber".to_string()],
                cfg,
                &tools(),
                cancel,
            )
            .await
        }
        async fn ok(&self) -> anyhow::Result<RunReport> {
            let (_tx, rx) = watch::channel(false);
            self.run(WallVerdict::Held, false, rx).await
        }
        /// A verification (ADR-0049 §7) through the rig, expecting one runtime.
        async fn verify(&self, wall: WallVerdict) -> VerifyEnd {
            let env = VmEnv {
                supervisor: self.fake.clone(),
                wall: Wall(wall),
                mint: self.mint(false),
                jobs: &self.jobs,
            };
            let expect = crate::vm::verify::expect_from_manifest(Some(
                br#"{"schema":1,"installed":{"xcode_version":"27.0","xcode_build":"18A5123b",
                    "runtimes":[{"platform":"iOS","version":"27.0","build":"24A335"}]}}"#,
            ))
            .unwrap();
            let (_tx, rx) = watch::channel(false);
            run_verify(
                &env,
                self.admitted(),
                ClaimId::try_from("0-1760000000000").unwrap(),
                &cfg(),
                &tools(),
                expect,
                Duration::from_secs(20),
                rx,
            )
            .await
        }
    }

    /// ADR-0049 §7: a verification boots, proves the wall and runs the checks
    /// exactly as a job's VM would, mints nothing, sends no JIT config, and
    /// stops its VM; the failed checks come back as the supervisor's codes.
    #[tokio::test(start_paused = true)]
    async fn a_verification_runs_the_checks_in_place_of_a_job_and_mints_nothing() {
        let rig = Rig::new(Script {
            verify_fails: vec![Check::Filevault, Check::BrewOwner],
            ..Default::default()
        });
        match rig.verify(WallVerdict::Held).await {
            VerifyEnd::Checked(failed) => {
                assert_eq!(failed, vec![Check::Filevault, Check::BrewOwner])
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(*rig.mints.lock().unwrap(), 0, "no JIT config is minted");
        assert_eq!(rig.fake.count("begin"), 0, "no job begins");
        assert!(!rig.fake.0.lock().unwrap().jit_seen);
        let calls = rig.fake.calls();
        let pos = |c: &str| calls.iter().position(|x| x.starts_with(c)).unwrap();
        assert!(pos("start") < pos("probe") && pos("probe") < pos("verify:1"));
        assert!(pos("verify:1") < pos("stop"));
        assert_eq!(
            rig.jobs.trail("0-1760000000000"),
            vec![
                VmStep::Cloning,
                VmStep::Booting,
                VmStep::ProvingWall,
                VmStep::Verifying,
                VmStep::Stopping,
                VmStep::Deleted
            ]
        );
        assert!(rig.jobs.view().is_empty(), "its slot is free again");
        assert_eq!(rig.counted(), 0, "its macOS place is released");

        let pass = Rig::new(Script::default());
        assert!(
            matches!(pass.verify(WallVerdict::Held).await, VerifyEnd::Checked(f) if f.is_empty())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_breached_wall_fails_the_verification_before_any_check() {
        let rig = Rig::new(Script::default());
        assert!(matches!(
            rig.verify(WallVerdict::Breached).await,
            VerifyEnd::WallBreached
        ));
        assert_eq!(rig.fake.count("verify"), 0);
        assert_eq!(rig.fake.count("stop"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn checks_that_never_finish_fail_the_run_and_stop_the_vm() {
        let rig = Rig::new(Script {
            verify_stalls: true,
            ..Default::default()
        });
        assert!(matches!(
            rig.verify(WallVerdict::Held).await,
            VerifyEnd::Failed(_)
        ));
        assert_eq!(rig.fake.count("stop"), 1);
        assert!(rig.jobs.view().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_that_never_boots_is_a_failed_run_not_a_verdict() {
        let rig = Rig::new(Script {
            stall_at: Some(VmState::Booting),
            ..Default::default()
        });
        assert!(matches!(
            rig.verify(WallVerdict::Held).await,
            VerifyEnd::Failed(_)
        ));
        assert_eq!(rig.fake.count("verify"), 0);
    }

    fn finishing() -> Script {
        Script {
            job_finishes: true,
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_job_walks_every_step_in_order_and_leaves_nothing() {
        let rig = Rig::new(finishing());
        let report = rig.ok().await.unwrap();
        assert!(report.job_ok && !report.timed_out && !report.reclaimed);
        assert_eq!(report.image.as_deref(), Some("macos-27"));

        // The wire order: start, wait for ready, probe, begin, wait, stop.
        let calls = rig.fake.calls();
        let wire: Vec<&str> = calls
            .iter()
            .map(String::as_str)
            .filter(|c| *c != "list")
            .collect();
        assert_eq!(wire, ["start:0", "probe", "begin", "stop"]);
        assert!(rig.fake.0.lock().unwrap().jit_seen);
        assert_eq!(*rig.mints.lock().unwrap(), 1);

        assert_eq!(
            rig.jobs.trail("42-1760000000"),
            [
                VmStep::Cloning,
                VmStep::Booting,
                VmStep::ProvingWall,
                VmStep::StartingRunner,
                VmStep::Running,
                VmStep::Stopping,
                VmStep::Deleted
            ]
        );
        assert!(rig.jobs.view().is_empty(), "the entry goes with the job");
        assert!(rig.jobs.quarantined().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_jit_config_is_not_minted_until_the_wall_holds() {
        let rig = Rig::new(finishing());
        let (_tx, rx) = watch::channel(false);
        let err = rig.run(WallVerdict::Breached, false, rx).await.unwrap_err();
        assert!(format!("{err:#}").contains("did not stop"), "{err:#}");
        assert_eq!(
            *rig.mints.lock().unwrap(),
            0,
            "no credential for a breached VM"
        );
        assert_eq!(rig.fake.count("begin"), 0);
        assert_eq!(rig.fake.count("stop"), 1, "the VM is still stopped");
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_wall_fails_closed_at_the_probe_deadline() {
        let rig = Rig::new(finishing());
        let (_tx, rx) = watch::channel(false);
        let err = rig
            .run(WallVerdict::Unreadable, false, rx)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("failing closed"), "{err:#}");
        assert_eq!(rig.fake.count("begin"), 0);
        assert_eq!(*rig.mints.lock().unwrap(), 0);
        assert_eq!(rig.fake.count("stop"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_that_never_reaches_the_runner_console_is_stopped() {
        let rig = Rig::new(Script {
            stall_at: Some(VmState::Booting),
            ..Default::default()
        });
        let err = rig.ok().await.unwrap_err();
        assert!(format!("{err:#}").contains("in time"), "{err:#}");
        assert_eq!(rig.fake.count("probe"), 0);
        assert_eq!(rig.fake.count("stop"), 1);
        assert!(rig.jobs.view().is_empty(), "confirmed gone, slot freed");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_start_starts_nothing_and_stops_nothing() {
        let rig = Rig::new(Script {
            refuse_start: Some(ErrorCode::ImageNotFound),
            ..Default::default()
        });
        let err = rig.ok().await.unwrap_err();
        assert!(format!("{err:#}").contains("ImageNotFound"), "{err:#}");
        assert_eq!(rig.fake.count("stop"), 0);
        assert!(rig.jobs.view().is_empty());
        // The slot is free again.
        assert!(rig
            .jobs
            .lease(&ClaimId::try_from("x").unwrap(), 0)
            .is_some());
    }

    /// A `start` that never reached the supervisor, as when setup restarts the
    /// session's supervisor just after the new daemon's first handshake (#393):
    /// nothing started, so the claim is given back rather than counted as a
    /// fault, and the "no VM session" gate closes until a handshake answers.
    #[tokio::test(start_paused = true)]
    async fn a_start_that_never_reached_the_supervisor_gives_the_claim_back() {
        use crate::vm::admit::SessionMode;
        let rig = Rig::new(Script {
            start_unreachable: true,
            ..Default::default()
        });
        rig.limiter.require_vm_session(SessionMode::Unlock);
        rig.limiter.observe_vm_session(Ok(()));
        assert_eq!(rig.limiter.snapshot().vm.session, "up");
        let report = rig.ok().await.unwrap();
        assert!(report.reclaimed, "not a fault: {report:?}");
        assert!(!report.job_ok && !report.infra_error);
        let cause = report.cause.unwrap();
        assert!(cause.contains("not connected"), "{cause}");
        assert!(cause.ends_with("the job is claimed again"), "{cause}");
        assert_eq!(rig.limiter.snapshot().vm.session, "down", "the gate closed");
        assert_eq!(rig.fake.count("stop"), 0, "nothing to stop");
        assert_eq!(*rig.mints.lock().unwrap(), 0, "nothing minted");
        assert_eq!(rig.counted(), 0, "the admission is released");
        assert!(rig.jobs.view().is_empty(), "and the slot");
    }

    /// A supervisor of another version answers the gate's probe but refuses `start`:
    /// closing the gate would flap it and hide the cause, so the run stays an
    /// infra error and the gate stays up.
    #[tokio::test(start_paused = true)]
    async fn a_skewed_supervisor_is_an_infra_error_and_leaves_the_gate_up() {
        use crate::vm::admit::SessionMode;
        let rig = Rig::new(Script {
            start_skewed: true,
            ..Default::default()
        });
        rig.limiter.require_vm_session(SessionMode::Unlock);
        rig.limiter.observe_vm_session(Ok(()));
        let err = rig.ok().await.unwrap_err();
        assert!(format!("{err:#}").contains("cannot be used"), "{err:#}");
        assert_eq!(rig.limiter.snapshot().vm.session, "up", "the gate stays up");
        assert_eq!(rig.counted(), 0, "the admission is released");
        assert!(rig.jobs.view().is_empty(), "and the slot");
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_slot_is_set_aside_and_the_next_is_tried() {
        let rig = Rig::new(Script {
            busy_slots: vec![0],
            job_finishes: true,
            ..Default::default()
        });
        let report = rig.ok().await.unwrap();
        assert!(report.job_ok);
        let starts: Vec<String> = rig
            .fake
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("start"))
            .collect();
        assert_eq!(starts, ["start:0", "start:1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reclaim_while_booting_stops_the_vm_and_reports_reclaimed() {
        let rig = Rig::new(Script {
            stall_at: Some(VmState::Booting),
            ..Default::default()
        });
        let (tx, rx) = watch::channel(false);
        let run = rig.run(WallVerdict::Held, false, rx);
        let trip = async {
            sleep(Duration::from_secs(3)).await;
            tx.send(true).unwrap();
        };
        let (report, ()) = tokio::join!(run, trip);
        let report = report.unwrap();
        assert!(report.reclaimed && !report.job_ok);
        assert_eq!(rig.fake.count("stop"), 1);
        assert_eq!(rig.fake.count("begin"), 0);
        assert_eq!(rig.fake.count("withdraw"), 0, "nothing was minted");
        assert!(
            rig.jobs.view().is_empty(),
            "the stop still waited for `stopped`"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reclaim_before_anything_starts_touches_nothing() {
        let rig = Rig::new(finishing());
        let (_tx, rx) = watch::channel(true);
        let report = rig.run(WallVerdict::Held, false, rx).await.unwrap();
        assert!(report.reclaimed);
        assert!(rig.fake.calls().is_empty());
    }

    // --- a reclaim asks GitHub before it stops a runner's VM (ADR-0017) --------

    /// The supervisor's wire and the fake GitHub's withdrawals, in order, without
    /// the `list` polls.
    fn wire(rig: &Rig) -> Vec<String> {
        rig.fake
            .calls()
            .into_iter()
            .filter(|c| c != "list")
            .collect()
    }

    /// A runner GitHub agrees to forget holds no job: it is withdrawn, by the id
    /// its mint answered with, and only then is its VM stopped.
    #[tokio::test(start_paused = true)]
    async fn a_reclaim_in_service_withdraws_the_runner_then_stops_the_vm() {
        let rig = Rig::new(Script::default()); // the job never finishes
        let (tx, rx) = watch::channel(false);
        let run = rig.run(WallVerdict::Held, false, rx);
        let trip = async {
            sleep(Duration::from_secs(3)).await;
            tx.send_replace(true);
        };
        let (report, ()) = tokio::join!(run, trip);
        let report = report.unwrap();
        assert!(report.reclaimed && !report.job_ok);
        assert_eq!(
            report.cause.as_deref(),
            Some(
                "reclaimed in service, once GitHub had withdrawn its runner (no job was \
                 assigned to it)"
            )
        );
        assert_eq!(
            wire(&rig),
            [
                "start:0",
                "probe",
                "begin",
                "withdraw:faber-42-1760000000:Some(7)",
                "stop"
            ]
        );
        assert_eq!(
            rig.fake.0.lock().unwrap().withdraw_reasons,
            [RECLAIM_WITHDRAWAL]
        );
        assert!(rig.jobs.view().is_empty());
    }

    /// The swap ADR-0017 was written for, in a VM: the runner is busy with a job
    /// the reconciler cannot see, so GitHub refuses to forget it, or cannot be
    /// asked, and the VM runs to `job_finished` instead of being stopped.
    #[tokio::test(start_paused = true)]
    async fn a_reclaim_github_refuses_leaves_the_job_running_to_its_end() {
        for answer in [
            Withdrawal::JobAssigned,
            Withdrawal::Failed("GitHub did not answer".into()),
        ] {
            let rig = Rig::new(Script {
                withdrawals: vec![answer.clone()],
                ..Default::default()
            });
            let (tx, rx) = watch::channel(false);
            let run = rig.run(WallVerdict::Held, false, rx);
            let trip = async {
                sleep(Duration::from_secs(3)).await;
                tx.send_replace(true);
                sleep(Duration::from_secs(5)).await;
                assert_eq!(rig.fake.count("withdraw"), 1, "{answer:?}: asked once");
                assert_eq!(rig.fake.count("stop"), 0, "{answer:?}: the VM runs on");
                assert_eq!(rig.jobs.view()[0].step, VmStep::Running);
                assert_eq!(rig.counted(), 1, "{answer:?}: still holding its place");
                rig.fake.0.lock().unwrap().script.job_finishes = true;
            };
            let (report, ()) = tokio::join!(run, trip);
            let report = report.unwrap();
            assert!(
                report.job_ok && !report.reclaimed,
                "{answer:?}: {:?}",
                report.cause
            );
            assert_eq!(
                wire(&rig),
                [
                    "start:0",
                    "probe",
                    "begin",
                    "withdraw:faber-42-1760000000:Some(7)",
                    "stop"
                ],
                "{answer:?}: stopped once, after its job"
            );
            assert!(rig.jobs.view().is_empty());
        }
    }

    /// A refused request is not asked again on every poll. The reconciler's next
    /// one is, though the flag never went back to false, as a Docker runner's
    /// watcher answers it (`docker::await_end`).
    #[tokio::test(start_paused = true)]
    async fn a_refused_reclaim_is_asked_again_only_on_the_next_request() {
        let rig = Rig::new(Script {
            withdrawals: vec![Withdrawal::JobAssigned],
            ..Default::default()
        });
        let (tx, rx) = watch::channel(false);
        let run = rig.run(WallVerdict::Held, false, rx);
        let trip = async {
            sleep(Duration::from_secs(3)).await;
            tx.send_replace(true);
            sleep(Duration::from_secs(10)).await;
            assert_eq!(rig.fake.count("withdraw"), 1, "ten polls, one question");
            tx.send_replace(true);
        };
        let (report, ()) = tokio::join!(run, trip);
        let report = report.unwrap();
        assert!(report.reclaimed, "{:?}", report.cause);
        assert_eq!(rig.fake.count("withdraw"), 2);
        assert_eq!(rig.fake.count("stop"), 1);
        assert!(rig.jobs.view().is_empty());
    }

    /// #269: a reclaim that lands while the runner is minted withdraws the
    /// registration no runner was started under, and stops the VM without
    /// `begin` whatever GitHub answers: nothing ran in it.
    #[tokio::test(start_paused = true)]
    async fn a_reclaim_after_the_mint_withdraws_the_unused_registration() {
        for answer in [
            Withdrawal::Withdrawn,
            Withdrawal::JobAssigned,
            Withdrawal::Failed("GitHub did not answer".into()),
        ] {
            let rig = Rig::new(Script {
                withdrawals: vec![answer.clone()],
                ..finishing()
            });
            let (tx, rx) = watch::channel(false);
            let env = VmEnv {
                supervisor: rig.fake.clone(),
                wall: Wall(WallVerdict::Held),
                mint: Mint {
                    reclaim_on_mint: Some(tx),
                    ..rig.mint(false)
                },
                jobs: &rig.jobs,
            };
            let report = run_one(
                &env,
                rig.admitted(),
                "faber-42-1760000000",
                &[],
                &cfg(),
                &tools(),
                rx,
            )
            .await
            .unwrap();
            assert!(report.reclaimed && !report.job_ok, "{answer:?}");
            assert_eq!(
                report.cause.as_deref(),
                Some("reclaimed after the JIT mint"),
                "{answer:?}"
            );
            assert_eq!(
                wire(&rig),
                [
                    "start:0",
                    "probe",
                    "withdraw:faber-42-1760000000:Some(7)",
                    "stop"
                ],
                "{answer:?}"
            );
            assert_eq!(
                rig.fake.0.lock().unwrap().withdraw_reasons,
                [UNUSED_WITHDRAWAL],
                "audited as a registration no runner was launched for"
            );
            assert!(
                !rig.fake.0.lock().unwrap().jit_seen,
                "the config never left faber"
            );
            assert!(rig.jobs.view().is_empty(), "{answer:?}");
            assert_eq!(rig.counted(), 0, "{answer:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_job_past_its_cap_is_force_stopped_and_reported_timed_out() {
        let rig = Rig::new(Script::default()); // never reaches job_finished
        let report = rig.ok().await.unwrap();
        assert!(report.timed_out && !report.job_ok);
        assert_eq!(rig.fake.count("stop"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_that_shuts_down_mid_job_is_an_infrastructure_error() {
        let rig = Rig::new(Script {
            guest_shuts_down: true,
            ..Default::default()
        });
        let err = rig.ok().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("stop_reason=guest_shutdown"),
            "{err:#}"
        );
    }

    /// A VM the child stops itself passes through `stopping` first. The job waits
    /// for `stopped` and its reason, and the error names it in the wire's word with
    /// the operator's command for the kept console (the field report: a bare "the VM
    /// stopped before it could finish the job").
    #[tokio::test(start_paused = true)]
    async fn a_vm_stopping_on_its_own_is_reported_with_its_reason_and_kept_log() {
        let rig = Rig::new(Script {
            agent_error_after_begin: true,
            stopping_polls: 3,
            ..Default::default()
        });
        let err = format!("{:#}", rig.ok().await.unwrap_err());
        assert!(err.contains("stop_reason=agent_error"), "{err}");
        assert!(err.contains("before it could finish the job"), "{err}");
        assert!(
            err.contains("`sudo cat -v /var/db/faber-mac/state/job-macos-27-failed-latest.log`"),
            "{err}"
        );
    }

    /// A runner that exits without ever taking a job is not a finished job (#392):
    /// the child stops the VM with its own reason, and the run fails with what
    /// happened and the command for the console the supervisor kept.
    #[tokio::test(start_paused = true)]
    async fn a_runner_that_took_no_job_fails_the_run_and_names_the_kept_log() {
        let rig = Rig::new(Script {
            runner_exits_without_job: true,
            stopping_polls: 2,
            ..Default::default()
        });
        let err = format!("{:#}", rig.ok().await.unwrap_err());
        assert!(
            err.contains("the JIT runner exited without taking a job"),
            "{err}"
        );
        assert!(
            err.contains("stop_reason=runner_exited_without_job"),
            "{err}"
        );
        assert!(err.contains("the runner's last lines"), "{err}");
        assert!(
            err.contains("`sudo cat -v /var/db/faber-mac/state/job-macos-27-failed-latest.log`"),
            "{err}"
        );
        assert!(rig.jobs.quarantined().is_empty());
        assert!(rig.jobs.view().is_empty(), "the slot is freed");
    }

    /// A VM that stays `stopping` past the stop timeout ends the wait anyway,
    /// with no reason to give: no hint about a kept log either.
    #[tokio::test(start_paused = true)]
    async fn a_vm_stuck_stopping_ends_the_wait_without_a_reason() {
        let rig = Rig::new(Script {
            agent_error_after_begin: true,
            stopping_polls: 1_000,
            ..Default::default()
        });
        let err = format!("{:#}", rig.ok().await.unwrap_err());
        assert!(
            err.contains("the VM stopped before it could finish the job"),
            "{err}"
        );
        assert!(!err.contains("sudo cat"), "{err}");
    }

    #[test]
    fn the_failed_job_log_is_in_the_session_accounts_state_directory() {
        assert_eq!(
            job_failed_log_command("xcode-27.1"),
            "sudo cat -v /var/db/faber-mac/state/job-xcode-27.1-failed-latest.log"
        );
        for (r, word) in [
            (StopReason::Requested, "requested"),
            (StopReason::GuestShutdown, "guest_shutdown"),
            (StopReason::StartFailed, "start_failed"),
            (StopReason::WallViolation, "wall_violation"),
            (StopReason::AgentError, "agent_error"),
            (StopReason::LauncherExited, "launcher_exited"),
            (
                StopReason::RunnerExitedWithoutJob,
                "runner_exited_without_job",
            ),
        ] {
            assert_eq!(r.as_str(), word);
            assert_eq!(serde_json::to_string(&r).unwrap(), format!("\"{word}\""));
        }
    }

    /// A supervisor restart mid-job (the updater's `exit`, setup's
    /// `kickstart -k`) leaves the VM running: the job rides out the gap and
    /// finishes, nothing is stopped early and no slot is quarantined.
    #[tokio::test(start_paused = true)]
    async fn a_supervisor_restart_mid_job_keeps_the_job() {
        let rig = Rig::new(Script {
            job_finishes: true,
            gap_lists: 15,
            ..Default::default()
        });
        let report = rig.ok().await.unwrap();
        assert!(report.job_ok, "{:?}", report.cause);
        assert_eq!(rig.fake.0.lock().unwrap().gaps_seen, 15);
        assert_eq!(
            rig.fake.count("stop"),
            1,
            "stopped once, when the job ended"
        );
        assert!(rig.jobs.quarantined().is_empty());
        assert!(rig.jobs.view().is_empty());
    }

    /// A supervisor that never comes back fails the job after the grace, not on
    /// the first missed `list`.
    #[tokio::test(start_paused = true)]
    async fn a_supervisor_that_never_returns_fails_the_job_after_the_grace() {
        let rig = Rig::new(Script {
            job_finishes: true,
            gap_lists: u32::MAX,
            ..Default::default()
        });
        let started = Instant::now();
        let err = rig.ok().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("did not answer")
                || format!("{err:#}").contains("not connected"),
            "{err:#}"
        );
        let gaps = rig.fake.0.lock().unwrap().gaps_seen;
        assert!(gaps >= 20, "rode out the 20 s grace at 1 s polls: {gaps}");
        assert!(started.elapsed() >= Duration::from_secs(20));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_mint_stops_the_vm_without_beginning() {
        let rig = Rig::new(finishing());
        let (_tx, rx) = watch::channel(false);
        let err = rig.run(WallVerdict::Held, true, rx).await.unwrap_err();
        assert!(format!("{err:#}").contains("GitHub said no"), "{err:#}");
        assert_eq!(rig.fake.count("begin"), 0);
        assert_eq!(rig.fake.count("stop"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_stop_keeps_the_slot_and_shows_orphaned() {
        let rig = Rig::new(Script {
            job_finishes: true,
            stop_never_lands: true,
            ..Default::default()
        });
        let report = rig.ok().await.unwrap();
        assert!(report.job_ok);
        assert!(
            report
                .cause
                .as_deref()
                .unwrap_or("")
                .contains("not confirmed stopped"),
            "{:?}",
            report.cause
        );
        assert_eq!(rig.jobs.quarantined(), [0]);
        let view = rig.jobs.view();
        assert_eq!(view.len(), 1);
        assert_eq!(view[0].step, VmStep::Orphaned);
        // The next job does not get the held slot.
        let next = rig
            .jobs
            .lease(&ClaimId::try_from("next").unwrap(), 0)
            .unwrap();
        assert_eq!(next.get(), 1);
        // 8c's seam frees it.
        rig.jobs
            .settle(&ClaimId::try_from("42-1760000000").unwrap());
        assert!(rig.jobs.quarantined().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_slot_is_held_through_stopping_until_stopped() {
        let rig = Rig::new(Script {
            job_finishes: true,
            stopping_polls: 3,
            ..Default::default()
        });
        rig.ok().await.unwrap();
        assert_eq!(rig.fake.0.lock().unwrap().stopping_seen, 3);
        assert!(rig.jobs.quarantined().is_empty());
        assert!(rig.jobs.view().is_empty(), "released only once stopped");
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_stuck_stopping_is_not_released() {
        let rig = Rig::new(Script {
            job_finishes: true,
            stopping_polls: 1000,
            ..Default::default()
        });
        rig.ok().await.unwrap();
        assert_eq!(rig.jobs.quarantined(), [0]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_stop_call_is_unconfirmed_too() {
        let rig = Rig::new(Script {
            job_finishes: true,
            stop_fails: true,
            ..Default::default()
        });
        rig.ok().await.unwrap();
        assert_eq!(rig.jobs.quarantined(), [0]);
    }

    /// One job through the production proof: the checker's counters, as pf
    /// would count each probe the fake supervisor sends (`counting`), or a red
    /// wall.
    async fn run_counted(rig: &Rig, counting: bool, red: bool) -> anyhow::Result<RunReport> {
        use crate::vm::wall_proof::tests::TickingPf;
        use crate::vm::wall_proof::CounterWall;
        let fake = rig.fake.clone();
        let probes = move || {
            if counting {
                fake.count("probe") as u64
            } else {
                0
            }
        };
        let env = VmEnv {
            supervisor: rig.fake.clone(),
            wall: CounterWall::new(TickingPf::new(probes, red)),
            mint: rig.mint(false),
            jobs: &rig.jobs,
        };
        let (_tx, rx) = watch::channel(false);
        run_one(
            &env,
            rig.admitted(),
            "faber-42-1760000000",
            &[],
            &cfg(),
            &tools(),
            rx,
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_the_checkers_counters_show_stopped_lets_the_job_begin() {
        let rig = Rig::new(finishing());
        let report = run_counted(&rig, true, false).await.unwrap();
        assert!(report.job_ok);
        assert_eq!(rig.fake.count("begin"), 1);
        assert_eq!(*rig.mints.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_uncounted_probe_or_a_red_wall_never_mints_or_begins() {
        for (what, counting, red) in [("uncounted", false, false), ("red", true, true)] {
            let rig = Rig::new(finishing());
            let err = run_counted(&rig, counting, red).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("failing closed"),
                "{what}: {err:#}"
            );
            assert_eq!(rig.fake.count("begin"), 0, "{what}");
            assert_eq!(*rig.mints.lock().unwrap(), 0, "{what}");
            assert_eq!(rig.fake.count("stop"), 1, "{what}: the VM is stopped");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_unconnected_supervisor_fails_closed_with_nothing_held() {
        let jobs = VmJobs::default();
        let env = VmEnv {
            supervisor: UnconnectedSupervisor,
            wall: UnprovenWall,
            mint: Mint {
                calls: Arc::default(),
                fail: false,
                wire: Fake::default(),
                reclaim_on_mint: None,
            },
            jobs: &jobs,
        };
        let (_tx, rx) = watch::channel(false);
        let err = run_one(&env, admission(), "faber-1-2", &[], &cfg(), &tools(), rx)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not connected"), "{err:#}");
        assert!(jobs.view().is_empty());
        assert!(
            jobs.quarantined().is_empty(),
            "nothing was ever sent, so nothing is held"
        );
    }

    #[test]
    fn a_runner_name_that_is_not_a_claim_id_is_refused() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let rig = Rig::new(finishing());
        let (_tx, rx) = watch::channel(false);
        let env = VmEnv {
            supervisor: rig.fake.clone(),
            wall: Wall(WallVerdict::Held),
            mint: rig.mint(false),
            jobs: &rig.jobs,
        };
        let err = rt
            .block_on(run_one(
                &env,
                admission(),
                "faber-../etc",
                &[],
                &cfg(),
                &tools(),
                rx,
            ))
            .unwrap_err();
        assert!(err.to_string().contains("claim id"), "{err}");
        assert!(rig.fake.calls().is_empty());
    }

    fn reviving() -> MacosVmConfig {
        MacosVmConfig {
            revive: RevivePolicy::default(),
            ..cfg()
        }
    }

    fn starts(rig: &Rig) -> usize {
        rig.fake.count("start")
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_vm_is_restarted_once_and_the_job_then_runs() {
        let rig = Rig::new(Script {
            wedged_starts: 1,
            job_finishes: true,
            ..Default::default()
        });
        let (_tx, rx) = watch::channel(false);
        let report = rig
            .run_with(&reviving(), WallVerdict::Held, false, rx)
            .await
            .unwrap();
        assert!(report.job_ok, "{:?}", report.cause);
        let wire: Vec<String> = rig
            .fake
            .calls()
            .into_iter()
            .filter(|c| c != "list")
            .collect();
        assert_eq!(
            wire,
            ["start:0", "stop", "start:0", "probe", "begin", "stop"],
            "stopped, then started again in the same slot"
        );
        assert!(rig.jobs.view().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_that_wedges_again_after_its_restart_is_given_up() {
        let rig = Rig::new(Script {
            wedged_starts: 2,
            ..Default::default()
        });
        let (_tx, rx) = watch::channel(false);
        let err = rig
            .run_with(&reviving(), WallVerdict::Held, false, rx)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("in time"), "{err:#}");
        assert_eq!(starts(&rig), 2, "one restart, no more");
        assert_eq!(rig.fake.count("probe"), 0);
        assert!(rig.jobs.view().is_empty(), "still torn down");
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_is_never_restarted_after_begin() {
        // The job never finishes: the VM is past `begin` when its cap runs out.
        let rig = Rig::new(Script::default());
        let (_tx, rx) = watch::channel(false);
        let report = rig
            .run_with(&reviving(), WallVerdict::Held, false, rx)
            .await
            .unwrap();
        assert!(report.timed_out);
        assert_eq!(starts(&rig), 1);
        assert_eq!(rig.fake.count("begin"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reclaim_during_a_restart_still_tears_the_vm_down() {
        let rig = Rig::new(Script {
            wedged_starts: 1,
            stopping_polls: 6,
            ..Default::default()
        });
        let (tx, rx) = watch::channel(false);
        let cfg = reviving();
        let run = rig.run_with(&cfg, WallVerdict::Held, false, rx);
        let trip = async {
            // Past the 30 s boot deadline, while the restart's stop is landing.
            sleep(Duration::from_secs(32)).await;
            tx.send(true).unwrap();
        };
        let (report, ()) = tokio::join!(run, trip);
        let report = report.unwrap();
        assert!(report.reclaimed, "{:?}", report.cause);
        assert_eq!(starts(&rig), 1, "never started again");
        assert_eq!(rig.fake.count("stop"), 2, "the restart's, then teardown's");
        assert!(rig.jobs.view().is_empty(), "confirmed gone, slot freed");
    }

    #[tokio::test(start_paused = true)]
    async fn no_vm_starts_until_boot_adoption_has_answered() {
        let rig = Rig::new(finishing());
        rig.jobs.hold_for_adoption();
        let err = rig.ok().await.unwrap_err();
        assert!(format!("{err:#}").contains("adopted"), "{err:#}");
        assert!(rig.fake.calls().is_empty());
        rig.jobs.adoption_settled();
        assert!(rig.ok().await.unwrap().job_ok);
    }

    /// An adopted `in_service` VM, as boot adoption leaves it.
    fn adopted(rig: &Rig) -> (ClaimId, Slot) {
        let claim = ClaimId::try_from("42-1760000000-hab12cde").unwrap();
        let slot = Slot::try_from(3).unwrap();
        let mut r = record(slot, claim.clone(), VmState::InService, None);
        r.started_unix = UnixSecs::try_from(now_ms() / 1000).unwrap();
        rig.fake.0.lock().unwrap().vm = Some(r);
        assert!(rig.jobs.adopt_live(&claim, slot, VmStep::Running));
        (claim, slot)
    }

    #[tokio::test(start_paused = true)]
    async fn an_adopted_job_is_watched_to_its_end_and_its_vm_stopped() {
        let rig = Rig::new(finishing());
        let (claim, slot) = adopted(&rig);
        let env = VmEnv {
            supervisor: rig.fake.clone(),
            wall: UnprovenWall,
            mint: NoMint,
            jobs: &rig.jobs,
        };
        let (_tx, rx) = watch::channel(false);
        let started = now_ms() / 1000;
        let report = watch_adopted(
            &env,
            rig.admitted(),
            claim,
            slot,
            cfg().timeouts,
            started,
            rx,
        )
        .await
        .unwrap();
        assert!(report.job_ok);
        let wire: Vec<String> = rig
            .fake
            .calls()
            .into_iter()
            .filter(|c| c != "list")
            .collect();
        assert_eq!(wire, ["stop"], "no start, probe, begin or mint");
        assert!(rig.jobs.view().is_empty(), "its slot is free again");
    }

    /// An adopted VM's reclaim is cleared as one this process began is: its
    /// runner is withdrawn first, by name, since its id died with the process
    /// that minted it, and a refusal leaves the job running to its end.
    #[tokio::test(start_paused = true)]
    async fn a_reclaimed_adopted_job_is_stopped_only_once_github_withdraws_its_runner() {
        for (answer, cleared) in [
            (Withdrawal::Withdrawn, true),
            (Withdrawal::JobAssigned, false),
        ] {
            let rig = Rig::new(Script {
                withdrawals: vec![answer.clone()],
                ..Default::default()
            });
            let (claim, slot) = adopted(&rig);
            let env = VmEnv {
                supervisor: rig.fake.clone(),
                wall: UnprovenWall,
                mint: rig.mint(false),
                jobs: &rig.jobs,
            };
            let (tx, rx) = watch::channel(false);
            let started = now_ms() / 1000;
            let run = watch_adopted(
                &env,
                rig.admitted(),
                claim,
                slot,
                cfg().timeouts,
                started,
                rx,
            );
            let trip = async {
                sleep(Duration::from_secs(3)).await;
                tx.send_replace(true);
                sleep(Duration::from_secs(3)).await;
                rig.fake.0.lock().unwrap().script.job_finishes = true;
            };
            let (report, ()) = tokio::join!(run, trip);
            let report = report.unwrap();
            assert_eq!(report.reclaimed, cleared, "{answer:?}: {:?}", report.cause);
            assert_eq!(report.job_ok, !cleared, "{answer:?}");
            assert_eq!(
                wire(&rig),
                ["withdraw:faber-42-1760000000-hab12cde:None", "stop"],
                "{answer:?}"
            );
            assert_eq!(*rig.mints.lock().unwrap(), 0, "never minted again");
            assert!(rig.jobs.view().is_empty(), "{answer:?}");
        }
    }

    /// #105 for a VM: an adopted runner no `mint` record of faber's own names is
    /// never written to GitHub, so its reclaim is refused.
    #[tokio::test]
    async fn an_adopted_runner_no_record_names_is_never_withdrawn() {
        let gh = Github::at("http://127.0.0.1:9".into());
        let record = MintRecord {
            gh: &gh,
            minted: None,
        };
        assert!(
            record.mint("faber-1-2", &[]).await.is_err(),
            "never minted again"
        );
        assert!(
            matches!(
                record.withdraw("faber-1-2", None, RECLAIM_WITHDRAWAL).await,
                Withdrawal::Failed(e) if e.contains("no record of minting")
            ),
            "refused without a request"
        );
    }

    // --- the admission is held for the job's life (#263) ----------------------

    #[tokio::test(start_paused = true)]
    async fn the_admission_is_held_while_the_vm_runs_and_released_on_a_reclaim() {
        let rig = Rig::new(Script::default()); // the job never finishes
        let (tx, rx) = watch::channel(false);
        let run = rig.run(WallVerdict::Held, false, rx);
        let trip = async {
            sleep(Duration::from_secs(3)).await;
            assert_eq!(rig.counted(), 1, "held while the VM runs");
            tx.send(true).unwrap();
        };
        let (report, ()) = tokio::join!(run, trip);
        assert!(report.unwrap().reclaimed);
        assert_eq!(rig.counted(), 0, "released once its VM is stopped");
    }

    #[tokio::test(start_paused = true)]
    async fn the_admission_is_released_on_success_and_on_every_failure() {
        let cases: [(&str, Script, bool); 4] = [
            ("success", finishing(), false),
            ("mint fails", finishing(), true),
            (
                "start refused",
                Script {
                    refuse_start: Some(ErrorCode::Capacity),
                    ..Default::default()
                },
                false,
            ),
            (
                "boot never finishes",
                Script {
                    stall_at: Some(VmState::Booting),
                    ..Default::default()
                },
                false,
            ),
        ];
        for (what, script, mint_fails) in cases {
            let rig = Rig::new(script);
            let (_tx, rx) = watch::channel(false);
            let _ = rig.run(WallVerdict::Held, mint_fails, rx).await;
            assert_eq!(rig.counted(), 0, "{what}");
            assert!(rig.jobs.view().is_empty(), "{what}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reclaim_before_the_clone_releases_the_admission_unstarted() {
        let rig = Rig::new(finishing());
        let (_tx, rx) = watch::channel(true);
        assert!(
            rig.run(WallVerdict::Held, false, rx)
                .await
                .unwrap()
                .reclaimed
        );
        assert!(rig.fake.calls().is_empty());
        assert_eq!(rig.counted(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unconfirmed_stop_hands_the_count_to_the_quarantine() {
        let rig = Rig::new(Script {
            job_finishes: true,
            stop_never_lands: true,
            ..Default::default()
        });
        rig.ok().await.unwrap();
        // No gap: the job's own count is gone only because the held VM now
        // counts against the class cap (`Limiter::vm_begin` reads it).
        assert_eq!(rig.counted(), 0);
        assert_eq!(rig.jobs.quarantined(), [0]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_adopted_jobs_count_is_released_when_it_ends() {
        let rig = Rig::new(finishing());
        let (claim, slot) = adopted(&rig);
        let env = VmEnv {
            supervisor: rig.fake.clone(),
            wall: UnprovenWall,
            mint: NoMint,
            jobs: &rig.jobs,
        };
        let (_tx, rx) = watch::channel(false);
        let started = now_ms() / 1000;
        let admission = rig.admitted();
        assert_eq!(rig.counted(), 1);
        let report = watch_adopted(&env, admission, claim, slot, cfg().timeouts, started, rx)
            .await
            .unwrap();
        assert!(report.job_ok);
        assert_eq!(rig.counted(), 0);
    }

    #[test]
    fn a_macos_job_never_leases_the_engine_or_build_slot() {
        let jobs = VmJobs::default();
        let mut got = Vec::new();
        for n in 0..SLOTS {
            let claim = ClaimId::try_from(format!("job-{n}")).unwrap();
            if let Some(slot) = jobs.lease(&claim, 0) {
                got.push(slot.get());
            }
        }
        assert_eq!(got, (0..14).collect::<Vec<u8>>());
        assert!(!got.contains(&super::super::engine::LINUX_SLOT));
        assert!(!got.contains(&crate::setup::linux_image::BUILD_SLOT));
        // Skipping every ordinary slot leaves nothing, not 14 or 15.
        let fresh = VmJobs::default();
        let claim = ClaimId::try_from("late").unwrap();
        assert_eq!(fresh.lease(&claim, (1 << 14) - 1), None);
    }

    #[test]
    fn the_registry_serializes_for_the_state_view() {
        let jobs = VmJobs::default();
        let claim = ClaimId::try_from("9-9").unwrap();
        jobs.lease(&claim, 0).unwrap();
        jobs.set_step(&claim, VmStep::ProvingWall);
        let json = serde_json::to_value(jobs.view()).unwrap();
        assert_eq!(json[0]["claim"], "9-9");
        assert_eq!(json[0]["slot"], 0);
        assert_eq!(json[0]["step"], "proving_wall");
    }
}
