//! Provisioning backends and label-based routing.
//!
//! faber can mint runners two ways: [`Backend::MacosVm`] runs an `osx-arm64`
//! runner in a per-job macOS VM (ADR-0026, see [`crate::vm::job`]);
//! [`Backend::Docker`] runs a linux runner in a throwaway container (see
//! [`crate::docker`]). A runner on the host itself (the native backend) is
//! removed by ADR-0026, and a config naming it is refused (ADR-0052). Which
//! one a queued job gets is decided by its `runs-on` labels: each [`Profile`]
//! binds a required label set to a backend and a [`TrustPolicy`], and a job is
//! routed to the first profile whose labels it carries.

use std::time::Duration;

use tokio::sync::watch;

use crate::docker::{self, ClaimMarks, DockerConfig, Limits, RunReport, DEFAULT_RUNNER_TIMEOUT};
use crate::gate::TrustPolicy;
use crate::github::Github;
use crate::limiter::VmRun;
use crate::vm::proto::{Cpus, ImageName, MemoryMib};

/// How faber provisions a runner for a matched job.
#[derive(Debug, Clone)]
pub enum Backend {
    /// A linux runner in a per-job container + dind sandbox.
    Docker(DockerConfig),
    /// An `osx-arm64` runner in a per-job macOS VM (ADR-0026).
    MacosVm(MacosVmConfig),
}

/// How long each phase of a macOS VM's life may take before faber gives up and
/// force-stops it. Fixed defaults today; a profile key is a later, additive change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmTimeouts {
    /// Clone, boot and the console user becoming `runner` (the supervisor's
    /// `starting` and `booting`). A spike measured a few tens of seconds to SSH.
    pub boot: Duration,
    /// The wall proof: the probe is sent, then the rule counters are read.
    pub probe: Duration,
    /// The job itself, from `begin` to `job_finished`. `ZERO` = uncapped. Set from
    /// the top-level `runner_timeout_secs`, like the docker backend's.
    pub job: Duration,
    /// Force-stop and clone deletion.
    pub stop: Duration,
    /// How often faber asks the supervisor for the VM's state (lockstep wire:
    /// state changes are learned by asking).
    pub poll: Duration,
    /// How long a running job rides out a supervisor that does not answer
    /// (unreachable, or the connection dropped) before it counts as failed. A
    /// supervisor restart (the updater's `exit`, setup's `kickstart -k`) leaves
    /// every VM running and the new supervisor adopts them by their markers,
    /// so a gap shorter than this is not the job's failure. Twice the restart
    /// deadline, so a relaunch launchd throttles still fits.
    pub supervisor_gap: Duration,
}

impl Default for VmTimeouts {
    fn default() -> Self {
        Self {
            boot: Duration::from_secs(300),
            probe: Duration::from_secs(60),
            job: DEFAULT_RUNNER_TIMEOUT,
            stop: Duration::from_secs(120),
            poll: Duration::from_secs(1),
            supervisor_gap: crate::vm::restart::RESTART_DEADLINE.saturating_mul(2),
        }
    }
}

/// A `backend = "macos-vm"` profile (ADR-0026), already validated into the
/// supervisor protocol's own types, so a value that exists here cannot be refused
/// on the wire for its grammar or range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosVmConfig {
    /// The golden image's name in the root-owned image store.
    pub image: ImageName,
    /// The `actions/runner` version a job runs when it pins none: the host's,
    /// resolved once at boot. With the installed guest agent it names the job's
    /// tools disk, which faber builds itself ([`crate::vm::tools_disk::for_this_host`]),
    /// so there is no `tools` key to configure.
    pub runner_version: String,
    pub cpus: Cpus,
    pub memory_mib: MemoryMib,
    pub timeouts: VmTimeouts,
    /// Whether a VM wedged before its runner console is restarted once on a fresh
    /// clone (ADR-0026, ADR-0007 retargeted). Host-wide: `[host] revive_vms`.
    pub revive: crate::vm::revive::RevivePolicy,
}

impl MacosVmConfig {
    /// Build from a profile's keys and the host's runner version; every problem
    /// comes back at once, as `faber check-config` would print it.
    pub fn from_profile(
        p: &crate::config::FileProfile,
        runner_version: &str,
    ) -> Result<Self, String> {
        let Some(image) = p
            .image
            .as_deref()
            .map(str::trim)
            .and_then(|n| ImageName::try_from(n).ok())
        else {
            return Err("`image` must be a name in the store".into());
        };
        let cpus = Cpus::try_from(p.cpus.unwrap_or(crate::config::DEFAULT_VM_CPUS))
            .map_err(|_| "`cpus` must be 1 to 64".to_string())?;
        let memory_mib =
            MemoryMib::try_from(p.memory_mib.unwrap_or(crate::config::DEFAULT_VM_MEMORY_MIB))
                .map_err(|_| "`memory_mib` must be 1024 to 262144".to_string())?;
        Ok(Self {
            image,
            runner_version: runner_version.to_string(),
            cpus,
            memory_mib,
            timeouts: VmTimeouts::default(),
            revive: crate::vm::revive::RevivePolicy::default(),
        })
    }
}

impl Backend {
    /// A short, stable label for the dashboard/logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Backend::Docker(_) => "docker",
            Backend::MacosVm(_) => "macos-vm",
        }
    }

    /// Provision one single-use runner and run exactly one job to completion.
    ///
    /// `limits` are the current per-job container ceilings (read live at the
    /// call site so a control-panel change applies to the next job); the macOS VM
    /// backend ignores them.
    ///
    /// `cancel` is the cooperative-reclaim signal (see `reconcile::reconcile_claims`):
    /// the Docker backend cuts a running-but-idle runner short on it.
    ///
    /// The macOS VM backend takes the cancel signal at every step boundary and
    /// force-stops its VM on it. It needs `vm`, the limiter's admission of this
    /// job (the wall gate, the two-VM cap, the machine share, the disk floor:
    /// [`crate::limiter::Limiter::vm_admit`]), and holds it until its VM is
    /// stopped; without one it starts nothing (#263). The other backends take
    /// none. Each VM's own wall proof still gates its job.
    ///
    /// `claim` is stamped on the Docker container as labels so a restarting faber
    /// can rebuild this claim without draining (ADR-0011).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_one(
        &self,
        gh: &Github,
        owner_repo: &str,
        runner_name: &str,
        labels: &[String],
        limits: &Limits,
        claim: &ClaimMarks,
        vm: Option<VmRun>,
        cancel: watch::Receiver<bool>,
    ) -> anyhow::Result<RunReport> {
        match self {
            Backend::MacosVm(cfg) => {
                let _ = (limits, claim);
                // Fail closed: a VM job the limiter never admitted starts nothing.
                let Some(admission) = vm else {
                    anyhow::bail!("a macOS VM job reached its backend without VM admission");
                };
                // Built on the first job of a (runner, agent) pair, reused after.
                // Nothing is minted yet, so a failure leaves GitHub's job queued.
                let tools = crate::vm::tools_disk::for_this_host(gh, &cfg.runner_version)
                    .await
                    .map_err(|e| anyhow::anyhow!(e).context("the job's tools disk"))?;
                crate::vm::job::run_one(
                    &crate::vm::job::production_env(gh, owner_repo),
                    admission,
                    runner_name,
                    labels,
                    cfg,
                    &tools,
                    cancel,
                )
                .await
            }
            Backend::Docker(cfg) => {
                let _ = vm;
                docker::run_one(
                    gh,
                    owner_repo,
                    runner_name,
                    labels,
                    cfg,
                    limits,
                    claim,
                    cancel,
                )
                .await
            }
        }
    }
}

/// A routing profile: the label set that opts a job in, the backend that serves
/// it, and the trust policy it is gated against. `policy.required_labels` mirrors
/// `labels` so the gate's own label check stays authoritative.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub labels: Vec<String>,
    pub backend: Backend,
    pub policy: TrustPolicy,
}

/// The first profile whose required labels are all present on the job, or `None`
/// if the job targets no configured profile (routine — not ours). Returns the
/// index too so callers can hand it to a spawned task without borrowing.
pub fn match_profile<'a>(
    profiles: &'a [Profile],
    job_labels: &[String],
) -> Option<(usize, &'a Profile)> {
    profiles.iter().enumerate().find(|(_, p)| {
        !p.labels.is_empty()
            && p.labels
                .iter()
                .all(|req| job_labels.iter().any(|have| have == req))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, labels: &[&str], backend: Backend) -> Profile {
        let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        let policy = TrustPolicy::new([], ["dependabot[bot]".to_string()], labels.clone());
        Profile {
            name: name.to_string(),
            labels,
            backend,
            policy,
        }
    }

    fn profiles() -> Vec<Profile> {
        vec![
            profile(
                "macos-vm",
                &["self-hosted", "macOS", "ARM64"],
                Backend::MacosVm(MacosVmConfig {
                    image: ImageName::try_from("golden").unwrap(),
                    runner_version: "2.330.0".to_string(),
                    cpus: Cpus::try_from(4).unwrap(),
                    memory_mib: MemoryMib::try_from(8192).unwrap(),
                    timeouts: VmTimeouts::default(),
                    revive: crate::vm::revive::RevivePolicy::default(),
                }),
            ),
            profile(
                "linux-docker",
                &["self-hosted", "docker"],
                Backend::Docker(DockerConfig::new("img:latest".to_string())),
            ),
        ]
    }

    fn labels(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn routes_docker_job_to_docker_profile() {
        let ps = profiles();
        let (idx, p) = match_profile(&ps, &labels(&["self-hosted", "docker"])).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(p.backend.kind(), "docker");
    }

    #[test]
    fn routes_mac_job_to_macos_vm_profile() {
        let ps = profiles();
        let (_, p) = match_profile(&ps, &labels(&["self-hosted", "macOS", "ARM64"])).unwrap();
        assert_eq!(p.backend.kind(), "macos-vm");
    }

    #[test]
    fn unmatched_labels_route_nowhere() {
        let ps = profiles();
        assert!(match_profile(&ps, &labels(&["ubuntu-latest"])).is_none());
    }

    #[test]
    fn partial_label_match_routes_nowhere() {
        // `docker` alone doesn't satisfy the docker profile (needs self-hosted too).
        let ps = profiles();
        assert!(match_profile(&ps, &labels(&["docker"])).is_none());
    }
}
