//! Restarting a wedged VM (ADR-0026 § "Size and recovery": ADR-0007's revival,
//! retargeted at a VM faber created).
//!
//! faber does not repair a VM. It asks the supervisor to stop it and start the
//! same claim again in the same slot, on a fresh clone. Two pure decisions say
//! when, so the policy is testable without a supervisor:
//!
//! - [`revive_decision`], for a per-job macOS VM, which [`super::job`] calls
//!   from its boot wait. A VM is **wedged** only while job code cannot have run:
//!   it has not left `starting` or `booting` within the boot deadline. Once
//!   `begin` has been sent a JIT config was handed over and job code may be
//!   running, so a restart there would run a job twice or lose its log; it is
//!   never restarted after `begin`, and never while `unmanaged`.
//! - [`linux_revival`], for the long-lived Linux VM (a wedge threshold, a
//!   cooldown between restarts, a give-up count). [`crate::host::revival`]
//!   delegates to it for `Custodian::Vm`, so there is one decision: custody
//!   decides, reconcile restarts through the engine, the engine driver starts it
//!   again.
//!
//! One switch covers both: `[host] revive_vms` (default on). A `macos-vm`
//! profile and the Linux VM are new, so turning the default on changes no
//! existing host (AGENTS.md: a release must not withdraw a capability).

use std::time::Duration;

use super::proto::VmState;

/// How a per-job VM may be revived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevivePolicy {
    /// `[host] revive_vms`.
    pub enabled: bool,
    /// Restarts one job's VM may have. One: a VM that wedges twice on a fresh
    /// clone of the same image is not wedging by chance, and the job is better
    /// failed (GitHub's job stays queued for another try) than held.
    pub max_restarts: u32,
}

impl RevivePolicy {
    /// Revival off: a wedged VM fails its job at once, as before this existed.
    pub const OFF: Self = Self {
        enabled: false,
        max_restarts: 0,
    };

    /// The policy `[host] revive_vms` asks for.
    pub fn from_switch(enabled: bool) -> Self {
        if enabled {
            Self::default()
        } else {
            Self::OFF
        }
    }
}

impl Default for RevivePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_restarts: 1,
        }
    }
}

/// What to do about a per-job VM at its boot deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revive {
    /// Not wedged (yet): keep waiting.
    Wait,
    /// Stop it, wait for `stopped`, and start the same claim in the same slot.
    Restart,
    /// Wedged, and no restart is allowed: today's failure path.
    GiveUp,
}

/// Whether a per-job macOS VM last seen in `state`, stuck there for
/// `stuck_for`, should be restarted, given `restarts_so_far` and the boot
/// `deadline` [`super::job`] already holds it to.
///
/// Only `starting` and `booting` can be wedged: no job code runs before
/// `begin`, so a fresh clone loses nothing. Every other state is [`Revive::Wait`]
/// here: `ready` and `probe_sent` have their own deadlines and failures,
/// `in_service` and `job_finished` are past `begin`, `stopping`/`stopped` are
/// already on their way out, and `unmanaged` is never acted on.
pub fn revive_decision(
    state: VmState,
    stuck_for: Duration,
    deadline: Duration,
    restarts_so_far: u32,
    policy: &RevivePolicy,
) -> Revive {
    if !matches!(state, VmState::Starting | VmState::Booting) {
        return Revive::Wait;
    }
    if stuck_for < deadline {
        return Revive::Wait;
    }
    if policy.enabled && restarts_so_far < policy.max_restarts {
        Revive::Restart
    } else {
        Revive::GiveUp
    }
}

/// How the long-lived Linux VM is revived, mirroring `[host]`'s Docker knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxRevivePolicy {
    /// `[host] revive_vms`.
    pub enabled: bool,
    /// Unhealthy this long is a wedge, not a hiccup (the Docker relay not
    /// answering under load is routine).
    pub revive_after: Duration,
    /// The least gap between two restarts, and how long the VM must then stay
    /// healthy before the restarts count as having worked (ADR-0024 §2's rule).
    pub cooldown: Duration,
    /// Restarts without a healthy cooldown in between before faber leaves it to
    /// a human, as [`crate::host::REVIVE_GIVE_UP_AFTER`] does for Docker.
    pub give_up_after: u32,
}

/// What to do about the Linux VM, once per health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxRevival {
    Wait,
    Restart,
    GaveUp { restarts: u32 },
}

/// The Linux VM's decision ([`crate::host::revival`] calls it): the Linux VM has been unhealthy for `unhealthy_for`;
/// `since_restart` is the time since faber last restarted it (`None` when it has
/// not since it was last stable) and `restarts` the count since then.
///
/// The Linux VM holds the only copy of a local-only runner image (AGENTS.md), so
/// this only ever restarts it; nothing here may recreate or delete its disk.
/// `unmanaged` is never restarted: the caller passes such a VM's state as
/// `unmanaged` and this answers [`LinuxRevival::Wait`] for it, whatever the
/// clocks say.
pub fn linux_revival(
    state: VmState,
    policy: &LinuxRevivePolicy,
    unhealthy_for: Duration,
    since_restart: Option<Duration>,
    restarts: u32,
) -> LinuxRevival {
    if state == VmState::Unmanaged || !policy.enabled {
        return LinuxRevival::Wait;
    }
    if restarts >= policy.give_up_after {
        return LinuxRevival::GaveUp { restarts };
    }
    if unhealthy_for < policy.revive_after {
        return LinuxRevival::Wait;
    }
    if since_restart.is_some_and(|d| d < policy.cooldown) {
        return LinuxRevival::Wait;
    }
    LinuxRevival::Restart
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [VmState; 9] = [
        VmState::Starting,
        VmState::Booting,
        VmState::Ready,
        VmState::ProbeSent,
        VmState::InService,
        VmState::JobFinished,
        VmState::Stopping,
        VmState::Stopped,
        VmState::Unmanaged,
    ];
    const DEADLINE: Duration = Duration::from_secs(300);

    #[test]
    fn a_vm_stuck_before_the_runner_console_is_restarted_once_then_given_up() {
        let p = RevivePolicy::default();
        for s in [VmState::Starting, VmState::Booting] {
            assert_eq!(
                revive_decision(s, DEADLINE, DEADLINE, 0, &p),
                Revive::Restart
            );
            assert_eq!(
                revive_decision(s, DEADLINE, DEADLINE, 1, &p),
                Revive::GiveUp
            );
            assert_eq!(
                revive_decision(s, DEADLINE - Duration::from_secs(1), DEADLINE, 0, &p),
                Revive::Wait,
                "not wedged before the deadline"
            );
        }
    }

    #[test]
    fn nothing_past_booting_is_ever_restarted() {
        for policy in [RevivePolicy::default(), RevivePolicy::OFF] {
            for s in ALL {
                if matches!(s, VmState::Starting | VmState::Booting) {
                    continue;
                }
                for restarts in [0, 1, 5] {
                    assert_ne!(
                        revive_decision(s, Duration::MAX, DEADLINE, restarts, &policy),
                        Revive::Restart,
                        "{s:?} is past begin, on its way out, or unmanaged"
                    );
                }
            }
        }
    }

    #[test]
    fn off_gives_up_at_the_deadline_as_before() {
        assert_eq!(
            revive_decision(VmState::Booting, DEADLINE, DEADLINE, 0, &RevivePolicy::OFF),
            Revive::GiveUp
        );
        assert_eq!(RevivePolicy::from_switch(false), RevivePolicy::OFF);
        assert_eq!(RevivePolicy::from_switch(true).max_restarts, 1);
    }

    fn linux() -> LinuxRevivePolicy {
        LinuxRevivePolicy {
            enabled: true,
            revive_after: Duration::from_secs(600),
            cooldown: Duration::from_secs(1800),
            give_up_after: 3,
        }
    }

    #[test]
    fn the_linux_seam_mirrors_docker_revival() {
        let p = linux();
        let s = VmState::InService;
        let wedged = Duration::from_secs(600);
        assert_eq!(linux_revival(s, &p, wedged, None, 0), LinuxRevival::Restart);
        assert_eq!(
            linux_revival(s, &p, Duration::from_secs(599), None, 0),
            LinuxRevival::Wait,
            "a hiccup"
        );
        assert_eq!(
            linux_revival(s, &p, wedged, Some(Duration::from_secs(60)), 1),
            LinuxRevival::Wait,
            "inside the cooldown"
        );
        assert_eq!(
            linux_revival(s, &p, wedged, Some(Duration::from_secs(1800)), 1),
            LinuxRevival::Restart
        );
        assert_eq!(
            linux_revival(s, &p, wedged, None, 3),
            LinuxRevival::GaveUp { restarts: 3 }
        );
        let off = LinuxRevivePolicy {
            enabled: false,
            ..p
        };
        assert_eq!(linux_revival(s, &off, wedged, None, 0), LinuxRevival::Wait);
    }

    #[test]
    fn the_linux_seam_never_restarts_an_unmanaged_vm() {
        for restarts in [0, 1, 3] {
            assert_eq!(
                linux_revival(VmState::Unmanaged, &linux(), Duration::MAX, None, restarts),
                LinuxRevival::Wait
            );
        }
    }
}
