//! Holding off idle sleep while this host runs jobs — issue #90.
//!
//! A host that sleeps with a job in flight loses that job: its runner stops
//! answering, and GitHub fails the job about ten minutes later. Every in-flight
//! job goes at once that way, in bursts, the whole host going dark.
//! `pmset -a sleep 0` (`deploy/keep-awake.sh`, `faber setup`) prevents it by never
//! sleeping. `[host] prevent_sleep` ([`SleepPolicy`]) does the narrower thing with
//! a no-idle-sleep assertion ([`crate::platform::power`]), no sudo and no pmset:
//!
//! - `while_busy` holds it while any job's runner is up, and lets go once the
//!   host has been idle for [`RELEASE_AFTER`], so an idle host still sleeps.
//! - `always` holds it from the first tick for the daemon's whole life, jobs or
//!   not, for a host that must stay reachable.
//! - `off` (the default) never holds one.
//!
//! **What counts as busy.** An *admitted* claim: one whose runner exists, and so
//! may hold a GitHub job that a sleep would lose. A parked claim has no runner
//! yet, and a paused host keeps its parked claims parked indefinitely (ADR-0023),
//! so counting those would keep a paused host awake for as long as anything was
//! queued.
//!
//! **Hysteresis** (`while_busy`). Taken on the first tick that sees a runner up; released only
//! after [`RELEASE_AFTER`] with none. Jobs in a burst end and start seconds
//! apart, and releasing between them would hand a long-idle machine its sleep in
//! the gap. The decision is [`decide`], a pure function; [`run_loop`] only reads
//! the count, applies the decision, and publishes it to `/api/state`.
//!
//! **It cannot outlive the daemon.** The power manager owns the assertion on
//! faber's behalf and drops it when the process exits, however it exits. A
//! restart therefore lets go, and the next process takes it again on its first
//! tick once it has adopted the running jobs (ADR-0011).
//!
//! Off by default: this is host-affecting and reaches unattended followers
//! through the release channel (ADR-0006), so a host that did not ask is
//! unchanged. Idle sleep only: a closed laptop lid still sleeps. macOS only;
//! elsewhere the key is a no-op that boot warns about.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tracing::{info, warn};

use crate::audit::now_ms;
use crate::config::SleepPolicy;
use crate::platform::power::{self, Assertion};
use crate::reconcile::Orchestrator;

/// How often the count of running jobs is read. Short against any idle-sleep
/// timer, which is a minute at the least.
pub const TICK: Duration = Duration::from_secs(5);

/// How long the host must have had no runner up before the assertion is
/// released. Long enough to bridge the gap between one job of a burst ending and
/// the next being admitted, which a reconcile cycle or two closes.
pub const RELEASE_AFTER: Duration = Duration::from_secs(180);

/// What `pmset -g assertions` shows next to faber's pid.
const REASON: &str = "faber: holding off idle sleep (prevent_sleep)";

/// What one tick does to the assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Take,
    Release,
    Keep,
}

/// Decide what to do with the assertion under `mode`, given whether it is
/// `held`, how many runners are up (`busy`), and since when the host has had none
/// while holding it (`idle_since_ms`). Returns the step and the new
/// `idle_since_ms`.
///
/// `while_busy`: busy always means held. `always`: held regardless of `busy`.
/// Either way a failed take is retried on the next tick, since a `false` `held`
/// comes back here as a fresh [`Step::Take`]. `off` holds nothing, and lets go of
/// anything held.
pub fn decide(
    mode: SleepPolicy,
    held: bool,
    busy: usize,
    idle_since_ms: Option<u64>,
    now_ms: u64,
) -> (Step, Option<u64>) {
    match mode {
        SleepPolicy::Off => return (if held { Step::Release } else { Step::Keep }, None),
        SleepPolicy::Always => return (if held { Step::Keep } else { Step::Take }, None),
        SleepPolicy::WhileBusy => {}
    }
    if busy > 0 {
        return (if held { Step::Keep } else { Step::Take }, None);
    }
    if !held {
        return (Step::Keep, None);
    }
    let since = idle_since_ms.unwrap_or(now_ms);
    if now_ms.saturating_sub(since) >= RELEASE_AFTER.as_millis() as u64 {
        (Step::Release, None)
    } else {
        (Step::Keep, Some(since))
    }
}

/// The assertion's state, for `/api/state` and the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AwakeView {
    /// `[host] prevent_sleep` is not `off`, as booted.
    pub enabled: bool,
    /// `[host] prevent_sleep`, as booted.
    pub mode: SleepPolicy,
    /// Whether this platform can hold the assertion. `enabled` without it is the
    /// documented no-op.
    pub supported: bool,
    /// Whether faber is holding off idle sleep right now.
    pub held: bool,
    /// When the assertion was taken (ms), while held.
    pub held_since_ms: Option<u64>,
    /// While held with no runner up: when it will be released (ms), unless a job
    /// starts first.
    pub release_at_ms: Option<u64>,
    /// Why the last take failed, until one succeeds.
    pub error: Option<String>,
}

impl AwakeView {
    /// The state at boot, before the loop's first tick.
    pub fn booted(mode: SleepPolicy) -> Self {
        Self {
            enabled: mode != SleepPolicy::Off,
            mode,
            supported: power::supported(),
            ..Self::default()
        }
    }
}

/// The boot warning for a key this platform cannot honour, if it is one.
pub fn unsupported_warning(mode: SleepPolicy, supported: bool) -> Option<String> {
    (mode != SleepPolicy::Off && !supported).then(|| {
        format!(
            "host.prevent_sleep = \"{}\" is set, but holding off idle sleep is implemented on \
             macOS only — it does nothing on this host",
            mode.as_str()
        )
    })
}

/// Hold off idle sleep as `mode` says, for the life of the process. Spawned only
/// when `[host] prevent_sleep` is not `off`.
pub async fn run_loop(orch: Arc<Orchestrator>, mode: SleepPolicy) {
    if let Some(msg) = unsupported_warning(mode, power::supported()) {
        warn!("{msg}");
        return;
    }
    match mode {
        SleepPolicy::Always => {
            info!("sleep: holding off idle sleep for as long as the daemon runs");
        }
        _ => info!(
            "sleep: holding off idle sleep while a job's runner is up, and releasing it {}s \
             after the last one ends",
            RELEASE_AFTER.as_secs()
        ),
    }
    let mut held: Option<(Assertion, u64)> = None;
    let mut idle_since: Option<u64> = None;
    let mut error: Option<String> = None;
    loop {
        let busy = orch.admitted_count().await;
        let now = now_ms();
        let (step, idle) = decide(mode, held.is_some(), busy, idle_since, now);
        idle_since = idle;
        match step {
            Step::Take => match power::prevent_idle_sleep(REASON) {
                Ok(a) => {
                    if mode == SleepPolicy::Always {
                        info!("sleep: holding off idle sleep");
                    } else {
                        info!("sleep: holding off idle sleep — {busy} job(s) running");
                    }
                    held = Some((a, now));
                    error = None;
                }
                Err(e) => {
                    // Once per distinct failure, not every tick it repeats.
                    if error.as_deref() != Some(e.as_str()) {
                        warn!("sleep: could not hold off idle sleep, retrying: {e}");
                    }
                    error = Some(e);
                }
            },
            Step::Release => {
                held = None;
                info!(
                    "sleep: released — no job running for {}s, the host may idle-sleep again",
                    RELEASE_AFTER.as_secs()
                );
            }
            Step::Keep => {}
        }
        orch.publish_awake(AwakeView {
            enabled: true,
            mode,
            supported: true,
            held: held.is_some(),
            held_since_ms: held.as_ref().map(|(_, at)| *at),
            release_at_ms: idle_since.map(|t| t + RELEASE_AFTER.as_millis() as u64),
            error: error.clone(),
        });
        tokio::time::sleep(TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: u64 = RELEASE_AFTER.as_millis() as u64;

    /// Drive [`decide`] through a sequence of `(now_ms, busy)` readings, applying
    /// each step as the loop does, and return the steps.
    fn run(readings: &[(u64, usize)]) -> Vec<Step> {
        run_as(SleepPolicy::WhileBusy, readings)
    }

    fn run_as(mode: SleepPolicy, readings: &[(u64, usize)]) -> Vec<Step> {
        let (mut held, mut idle) = (false, None);
        readings
            .iter()
            .map(|&(now, busy)| {
                let (step, next) = decide(mode, held, busy, idle, now);
                idle = next;
                match step {
                    Step::Take => held = true,
                    Step::Release => held = false,
                    Step::Keep => {}
                }
                step
            })
            .collect()
    }

    #[test]
    fn an_idle_host_never_takes_it() {
        assert_eq!(
            run(&[(0, 0), (5_000, 0), (GRACE * 10, 0)]),
            [Step::Keep, Step::Keep, Step::Keep]
        );
    }

    #[test]
    fn the_first_running_job_takes_it_and_more_keep_it() {
        assert_eq!(
            run(&[(0, 0), (5_000, 1), (10_000, 3), (15_000, 1)]),
            [Step::Keep, Step::Take, Step::Keep, Step::Keep]
        );
    }

    #[test]
    fn it_is_released_only_after_the_grace_with_nothing_running() {
        let end = 60_000;
        assert_eq!(
            run(&[
                (0, 1),
                (end, 0),
                (end + GRACE - 1, 0),
                (end + GRACE, 0),
                (end + GRACE + 5_000, 0),
            ]),
            [
                Step::Take,
                Step::Keep,
                Step::Keep,
                Step::Release,
                Step::Keep
            ]
        );
    }

    /// The hysteresis: a burst whose jobs end and start inside the grace never
    /// lets go, and the grace starts over from the last job's end.
    #[test]
    fn a_gap_between_jobs_does_not_flap() {
        let gap = GRACE / 2;
        assert_eq!(
            run(&[
                (0, 1),
                (10_000, 0),
                (10_000 + gap, 1),
                (20_000 + gap, 0),
                (20_000 + gap + GRACE - 1, 0),
                (20_000 + gap + GRACE, 0),
            ]),
            [
                Step::Take,
                Step::Keep,
                Step::Keep,
                Step::Keep,
                Step::Keep,
                Step::Release
            ]
        );
    }

    /// A take that failed leaves `held` false, so the next busy tick tries again
    /// rather than believing it holds something.
    #[test]
    fn a_failed_take_is_retried() {
        assert_eq!(
            decide(SleepPolicy::WhileBusy, false, 1, None, 0),
            (Step::Take, None)
        );
        assert_eq!(
            decide(SleepPolicy::WhileBusy, false, 1, None, 5_000),
            (Step::Take, None)
        );
    }

    #[test]
    fn a_job_during_the_grace_clears_the_countdown() {
        assert_eq!(
            decide(SleepPolicy::WhileBusy, true, 0, None, 100),
            (Step::Keep, Some(100))
        );
        assert_eq!(
            decide(SleepPolicy::WhileBusy, true, 2, Some(100), 200),
            (Step::Keep, None)
        );
    }

    /// `always` takes on the first tick with nothing running, never lets go, and
    /// retries a failed take.
    #[test]
    fn always_holds_regardless_of_jobs() {
        assert_eq!(
            run_as(
                SleepPolicy::Always,
                &[(0, 0), (5_000, 0), (GRACE * 10, 0), (GRACE * 10 + 5_000, 3)]
            ),
            [Step::Take, Step::Keep, Step::Keep, Step::Keep]
        );
        assert_eq!(
            decide(SleepPolicy::Always, false, 0, None, 5_000),
            (Step::Take, None)
        );
    }

    /// `off` never takes, and drops anything it somehow holds.
    #[test]
    fn off_never_holds() {
        assert_eq!(
            run_as(SleepPolicy::Off, &[(0, 0), (5_000, 4), (10_000, 0)]),
            [Step::Keep, Step::Keep, Step::Keep]
        );
        assert_eq!(
            decide(SleepPolicy::Off, true, 2, Some(1), 9),
            (Step::Release, None)
        );
    }

    #[test]
    fn the_key_warns_only_where_it_cannot_work() {
        use SleepPolicy::*;
        assert!(unsupported_warning(Off, false).is_none());
        assert!(unsupported_warning(Off, true).is_none());
        assert!(unsupported_warning(WhileBusy, true).is_none());
        assert!(unsupported_warning(Always, true).is_none());
        for mode in [WhileBusy, Always] {
            let msg = unsupported_warning(mode, false).unwrap();
            assert!(
                msg.starts_with(&format!(
                    "host.prevent_sleep = \"{}\" is set",
                    mode.as_str()
                )),
                "{msg}"
            );
        }
    }

    #[test]
    fn the_boot_view_is_off_and_not_held() {
        let v = AwakeView::booted(SleepPolicy::Off);
        assert!(!v.enabled && !v.held && v.held_since_ms.is_none());
        let v = AwakeView::booted(SleepPolicy::Always);
        assert!(v.enabled && v.mode == SleepPolicy::Always && !v.held);
        assert_eq!(v.supported, power::supported());
    }
}
