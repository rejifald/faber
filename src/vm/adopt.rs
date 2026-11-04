//! Taking back VMs across a faber restart, and sweeping the ones nothing holds
//! (ADR-0026: "VMs outlive a faber restart"; ADR-0011's adoption, applied to VMs).
//!
//! The VMs belong to the supervisor, not to faber, so a restart or a self-update
//! leaves running jobs alone. What a new faber process must do is learn what is
//! already running before it starts anything, and stop what it can no longer
//! account for. Every decision is a pure function of the supervisor's own
//! vocabulary ([`super::proto`]) and of what [`VmJobs`] holds; the I/O wrappers
//! below only call them.
//!
//! **What is faber's.** A record is faber's to stop only when it is a macOS VM
//! whose claim is in faber's own claim grammar and names this host's instance id
//! ([`ours`]). The claim is compared as data: never a path, a bundle name or a
//! process name (AGENTS.md: read labels, never names). The Linux VM is never
//! faber's to stop here: it owns its own lifecycle (slice 7). An `unmanaged` VM is
//! never stopped by anything (the protocol's versioning rule).
//!
//! **What this never does.** No code here deletes a file or a directory. Clones
//! are the supervisor's, and it deletes one only when it carries the
//! `faber-clone` marker and no live claim marker names it. faber acts only by
//! asking it to `stop` a VM, which it can refuse.

use std::collections::HashSet;
use std::time::Duration;

use tracing::{info, warn};

use super::job::{SupervisorError, VmJobs, VmStep, VmSupervisor};
use super::proto::{ClaimId, ErrorCode, Slot, VmKind, VmRecord, VmState, MAX_LIST};

/// The least age a VM must have before the sweep may stop it, seconds. A VM's
/// slot is leased before `start` is sent, so a start in flight is already held;
/// this is the margin behind that, so the sweep never races one.
pub const MIN_SWEEP_GRACE_SECS: u64 = 120;

/// The grace the daemon sweeps with, seconds.
pub const SWEEP_GRACE_SECS: u64 = 300;

/// The most VMs one sweep stops. Two macOS VMs is Apple's limit, so more than
/// this in one pass means something is wrong that a burst of stops would not fix.
pub const MAX_STOPS_PER_SWEEP: usize = 4;

/// How often the daemon sweeps. One `list` on a local socket: cheap, and a slot
/// held for a VM that is gone is a third of the host's macOS capacity.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

/// Whose a VM record is, as far as stopping it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// A macOS VM this host's faber started: its claim is `{job}-{ts}-{instance}`
    /// with this host's instance id.
    Ours,
    /// A macOS VM whose claim is in faber's grammar but carries no instance id
    /// (`{job}-{ts}`, from before #94). Taken back and held, never stopped: with no
    /// instance in it, nothing says this host's faber minted it.
    PreInstance,
    /// Not faber's claim grammar, or another instance's.
    Foreign,
    /// The long-lived Linux VM, whatever its claim: never this module's to stop.
    Linux,
}

/// Whose `record` is, for this host's instance id `this_instance`.
pub fn ours(record: &VmRecord, this_instance: &str) -> Ownership {
    if record.kind == VmKind::Linux {
        return Ownership::Linux;
    }
    let claim = record.claim.as_str();
    if crate::untrusted::claim_id(claim).is_none() {
        return Ownership::Foreign;
    }
    match crate::instance::of_runner(&format!("faber-{claim}")) {
        Some(inst) if inst == this_instance => Ownership::Ours,
        Some(_) => Ownership::Foreign,
        None => Ownership::PreInstance,
    }
}

// ---------------------------------------------------------------------------
// Boot adoption
// ---------------------------------------------------------------------------

/// What boot adoption does with one VM the supervisor reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptAction {
    /// Not faber's to take back, or already `stopped`.
    Ignore,
    /// A job runs (`in_service`): rebuild its claim and watch it to its end.
    Watch,
    /// Stop it and hold its slot until `stopped`: the previous process's teardown
    /// (`job_finished`), or a ramp no new process can resume (before `begin`).
    Stop(&'static str),
    /// Already `stopping`: hold its slot until `stopped`.
    HoldUntilStopped,
    /// Hold its slot and never stop it.
    HoldNeverStop(&'static str),
}

/// What boot adoption does with `record`.
///
/// Before `begin` no job code has run, so stopping loses nothing: GitHub's job is
/// still queued and is claimed again. A runner registration minted at
/// `probe_sent` is left behind; only the process that minted it could withdraw
/// it by id ([`super::job::Minted::runner_id`]), so the stale-runner sweep
/// (ADR-0018) reaps it.
pub fn adoption_action(record: &VmRecord, this_instance: &str) -> AdoptAction {
    match ours(record, this_instance) {
        Ownership::Linux | Ownership::Foreign => return AdoptAction::Ignore,
        Ownership::PreInstance => {
            return match record.state {
                VmState::Stopped => AdoptAction::Ignore,
                _ => AdoptAction::HoldNeverStop("its claim names no instance"),
            };
        }
        Ownership::Ours => {}
    }
    match record.state {
        VmState::Unmanaged => AdoptAction::HoldNeverStop("unmanaged: an older launcher runs it"),
        VmState::Stopped => AdoptAction::Ignore,
        VmState::Stopping => AdoptAction::HoldUntilStopped,
        VmState::InService => AdoptAction::Watch,
        VmState::JobFinished => {
            AdoptAction::Stop("its job finished and the previous process never stopped it")
        }
        VmState::Starting | VmState::Booting | VmState::Ready | VmState::ProbeSent => {
            AdoptAction::Stop("no job code ran yet, and the previous process's ramp cannot resume")
        }
        // A verification (ADR-0049 §7) runs no job: the process that asked for it
        // is gone, so its result has no one to record it. The next one re-runs it.
        VmState::Verifying | VmState::Verified => {
            AdoptAction::Stop("a verification the previous process ran, with no job in it")
        }
    }
}

/// A VM boot adoption found running a job, already held in [`VmJobs`]: the
/// caller rebuilds its claim and watches it ([`super::job::watch_adopted`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    pub claim: ClaimId,
    pub slot: Slot,
    /// The job id its claim was minted for.
    pub job_id: u64,
    /// When the supervisor started it, so the job cap and the elapsed readout
    /// keep its age.
    pub started_unix: u64,
}

/// Take back the VMs the supervisor runs for this host: `list`, then `adopt` the
/// claims that are faber's, then act on each as [`adoption_action`] says.
///
/// Runs once, before the first VM job of the process and before the first sweep
/// (the caller holds [`VmJobs::hold_for_adoption`] until it answers), for the
/// reason [`crate::reconcile::Orchestrator::adopt`] gives: a lease against an
/// empty registry would hand out a slot a job holds, and a sweep first would stop
/// the very VMs this takes back.
///
/// `None` means the supervisor could not be read, which is **not** "nothing is
/// running": the caller keeps VM admission shut and tries again.
pub async fn adopt_at_boot<S: VmSupervisor>(
    sup: &S,
    jobs: &VmJobs,
    this_instance: &str,
) -> Option<Vec<Rebuilt>> {
    let listed = match sup.list().await {
        Ok(v) => v,
        Err(e) => {
            // Debug: the caller warns once, and retries every few seconds.
            tracing::debug!("vm adopt: cannot list the supervisor's VMs ({e})");
            return None;
        }
    };
    let asked: Vec<ClaimId> = listed
        .iter()
        .filter(|r| adoption_action(r, this_instance) != AdoptAction::Ignore)
        .map(|r| r.claim.clone())
        .collect();
    // The supervisor answers from the claim markers it has verified, never from a
    // slot alone, so its `found` is what is acted on, not the listing.
    let mut found = Vec::new();
    for chunk in asked.chunks(MAX_LIST) {
        match sup.adopt(chunk).await {
            Ok(a) => {
                for gone in &a.unknown {
                    info!(
                        "vm adopt: {} is gone (no live claim marker carries it)",
                        gone.as_str()
                    );
                }
                found.extend(a.found);
            }
            Err(e) => {
                tracing::debug!("vm adopt: the supervisor did not answer `adopt` ({e})");
                return None;
            }
        }
    }

    let mut rebuilt = Vec::new();
    for r in found {
        let claim = r.claim.clone();
        let action = adoption_action(&r, this_instance);
        let registered = match action {
            AdoptAction::Ignore => continue,
            AdoptAction::Watch => jobs.adopt_live(&claim, r.slot, VmStep::Running),
            AdoptAction::Stop(_) | AdoptAction::HoldUntilStopped => {
                jobs.hold(&claim, r.slot, VmStep::Stopping)
            }
            AdoptAction::HoldNeverStop(_) => jobs.hold(&claim, r.slot, VmStep::Unmanaged),
        };
        if !registered {
            warn!(
                "vm adopt: {} in slot {} collides with a VM already held — leaving it to the \
                 sweep",
                claim.as_str(),
                r.slot.get()
            );
            continue;
        }
        match action {
            AdoptAction::Watch => {
                let job_id = crate::untrusted::claim_id(claim.as_str())
                    .map(|(job, _)| job)
                    .expect("ours() parsed it");
                info!(
                    "vm adopt: {} in slot {} is running job {job_id} — watching it",
                    claim.as_str(),
                    r.slot.get()
                );
                rebuilt.push(Rebuilt {
                    claim,
                    slot: r.slot,
                    job_id,
                    started_unix: r.started_unix.get(),
                });
            }
            AdoptAction::Stop(why) => {
                info!(
                    "vm adopt: stopping {} in slot {} ({:?}: {why})",
                    claim.as_str(),
                    r.slot.get(),
                    r.state
                );
                stop_held(sup, jobs, &claim).await;
            }
            AdoptAction::HoldUntilStopped => info!(
                "vm adopt: {} in slot {} is already stopping — holding its slot until it stops",
                claim.as_str(),
                r.slot.get()
            ),
            AdoptAction::HoldNeverStop(why) => warn!(
                "vm adopt: {} in slot {} is held and will not be stopped ({why}) — it keeps \
                 its slot until it exits",
                claim.as_str(),
                r.slot.get()
            ),
            AdoptAction::Ignore => unreachable!("skipped above"),
        }
    }
    Some(rebuilt)
}

/// Ask the supervisor to stop a held VM. Settled at once when it has none by that
/// claim; any other failure leaves it held, for the next sweep to stop again.
async fn stop_held<S: VmSupervisor>(sup: &S, jobs: &VmJobs, claim: &ClaimId) {
    info!(
        "vm adopt: asking the VM supervisor to stop {}",
        claim.as_str()
    );
    match sup.stop(claim).await {
        Ok(()) => {}
        Err(SupervisorError::Refused(ErrorCode::UnknownClaim)) => jobs.settle(claim),
        Err(e) => warn!(
            "vm: stop of {} not confirmed ({e}) — its slot stays held",
            claim.as_str()
        ),
    }
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// What the sweep does with one VM in `list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepAction {
    Leave,
    /// Ask the supervisor to stop it, for the reason given.
    Stop(&'static str),
    /// It is `stopped`: free the slot faber holds for it.
    Settle,
}

/// What the sweep does with `record`, given the claims a live job holds
/// (`held`) and those held in quarantine (`quarantined`).
///
/// Stop-only and marker-only: a `Stop` is a request the supervisor may refuse,
/// and it is produced only for a macOS VM of this host's own ([`ours`]) that no
/// live job holds, that is older than `grace_secs` (at least
/// [`MIN_SWEEP_GRACE_SECS`]), and that is not on its way out by itself.
pub fn sweep_action(
    record: &VmRecord,
    held: &HashSet<String>,
    quarantined: &HashSet<String>,
    this_instance: &str,
    now_unix: u64,
    grace_secs: u64,
) -> SweepAction {
    if record.kind == VmKind::Linux || record.state == VmState::Unmanaged {
        return SweepAction::Leave;
    }
    let claim = record.claim.as_str();
    // A live job watches its own VM, through every state.
    if held.contains(claim) {
        return SweepAction::Leave;
    }
    let in_quarantine = quarantined.contains(claim);
    if record.state == VmState::Stopped {
        return if in_quarantine {
            SweepAction::Settle
        } else {
            SweepAction::Leave
        };
    }
    if ours(record, this_instance) != Ownership::Ours {
        return SweepAction::Leave;
    }
    // Stopping by itself; a stop faber asked for and could not confirm is asked
    // again (it is idempotent).
    if record.state == VmState::Stopping && !in_quarantine {
        return SweepAction::Leave;
    }
    let age = now_unix.saturating_sub(record.started_unix.get());
    if age < grace_secs.max(MIN_SWEEP_GRACE_SECS) {
        return SweepAction::Leave;
    }
    SweepAction::Stop(if in_quarantine {
        "its stop was never confirmed"
    } else {
        "no job of this faber holds it"
    })
}

/// One sweep's work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// VMs to stop, with why: at most the cap passed to [`plan_sweep`].
    pub stop: Vec<(ClaimId, &'static str)>,
    /// Held claims whose VM is confirmed gone: `stopped`, or absent from `list`.
    pub settle: Vec<ClaimId>,
    /// Stops left for a later sweep by the cap.
    pub deferred: usize,
}

/// The whole sweep over one `list`, as [`sweep_action`] decides each record,
/// plus every quarantined claim `list` no longer shows (its VM is gone). Each
/// claim gets at most one action, and at most `max_stops` VMs are stopped.
pub fn plan_sweep(
    records: &[VmRecord],
    held: &HashSet<String>,
    quarantined: &HashSet<String>,
    this_instance: &str,
    now_unix: u64,
    grace_secs: u64,
    max_stops: usize,
) -> SweepPlan {
    let mut plan = SweepPlan::default();
    for r in records {
        match sweep_action(r, held, quarantined, this_instance, now_unix, grace_secs) {
            SweepAction::Leave => {}
            SweepAction::Settle => plan.settle.push(r.claim.clone()),
            SweepAction::Stop(why) if plan.stop.len() < max_stops => {
                plan.stop.push((r.claim.clone(), why));
            }
            SweepAction::Stop(_) => plan.deferred += 1,
        }
    }
    let listed: HashSet<&str> = records.iter().map(|r| r.claim.as_str()).collect();
    let mut gone: Vec<&String> = quarantined
        .iter()
        .filter(|c| !listed.contains(c.as_str()))
        .collect();
    gone.sort();
    plan.settle.extend(
        gone.into_iter()
            .filter_map(|c| ClaimId::try_from(c.as_str()).ok()),
    );
    plan
}

/// One sweep: `list`, then act on [`plan_sweep`]. `None` when it did not run:
/// boot adoption has not answered yet, or the supervisor did not.
///
/// `list` is read **before** the registry is snapshotted. A job's slot is leased
/// before its `start` is sent, so every VM in the listing was held by then, and a
/// live job is always in the snapshot (the janitor's ordering, [`crate::sweep`]).
///
/// `observe` hears each listing that answered, before anything is acted on: the
/// limiter publishes it to `/api/state` ([`crate::limiter::Limiter::observe_vms`]).
pub async fn sweep_once<S: VmSupervisor>(
    sup: &S,
    jobs: &VmJobs,
    this_instance: &str,
    now_unix: u64,
    observe: impl FnOnce(&[VmRecord]),
) -> Option<SweepPlan> {
    if jobs.awaiting_adoption() {
        return None;
    }
    let records = match sup.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("vm sweep: skipped, the supervisor did not answer ({e})");
            return None;
        }
    };
    observe(&records);
    let (held, quarantined) = jobs.held();
    let plan = plan_sweep(
        &records,
        &held,
        &quarantined,
        this_instance,
        now_unix,
        SWEEP_GRACE_SECS,
        MAX_STOPS_PER_SWEEP,
    );
    for claim in &plan.settle {
        info!(
            "vm sweep: {} is gone — its slot is free again",
            claim.as_str()
        );
        jobs.settle(claim);
    }
    for (claim, why) in &plan.stop {
        info!("vm sweep: stopping {} ({why})", claim.as_str());
        match sup.stop(claim).await {
            Ok(()) => {}
            Err(SupervisorError::Refused(ErrorCode::UnknownClaim)) => {
                if quarantined.contains(claim.as_str()) {
                    jobs.settle(claim);
                }
            }
            Err(e) => warn!("vm sweep: stop of {} not confirmed ({e})", claim.as_str()),
        }
    }
    if plan.deferred > 0 {
        warn!(
            "vm sweep: {} more VM(s) to stop, left for the next sweep (at most \
             {MAX_STOPS_PER_SWEEP} per sweep)",
            plan.deferred
        );
    }
    Some(plan)
}

/// Sweep every [`SWEEP_INTERVAL`], forever. Spawned by the daemon on a host with
/// a `macos-vm` profile; each pass waits for boot adoption to have answered.
pub async fn sweep_loop(this_instance: String, limiter: std::sync::Arc<crate::limiter::Limiter>) {
    let env = super::job::adopted_env();
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        let now_unix = crate::audit::now_ms() / 1000;
        sweep_once(
            &env.supervisor,
            env.jobs,
            &this_instance,
            now_unix,
            |records| limiter.observe_vms_of(VmKind::Macos, records),
        )
        .await;
    }
}

/// How often the VM list is polled for `/api/state` (popover/vms/anatomy.md O2).
/// The popover shows an "as of" once the list is over twice this old.
pub const LIST_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Poll one supervisor's `list` every [`LIST_POLL_INTERVAL`] and publish the rows
/// of `kind` to `/api/state`, forever. Display only: it acts on nothing, so it
/// needs no boot adoption and takes no part in the sweep. A refused or failed
/// list leaves the last rows and lets `listed_age_s` grow.
pub async fn list_poll_loop<S: VmSupervisor>(
    sup: S,
    kind: VmKind,
    limiter: std::sync::Arc<crate::limiter::Limiter>,
) {
    loop {
        match sup.list().await {
            Ok(records) => limiter.observe_vms_of(kind, &records),
            Err(e) => tracing::debug!("vm list poll: the supervisor did not answer ({e})"),
        }
        tokio::time::sleep(LIST_POLL_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A VM a live job claims is never the sweep's, whether a listing shows it
    /// or, during a supervisor restart's gap, does not: the sweep only stops
    /// what no job holds, and settles only quarantined claims.
    #[test]
    fn the_sweep_leaves_a_claimed_vm_alone_seen_or_unseen() {
        let claim = "42-1760000000-abcd";
        let held: HashSet<String> = [claim.to_string()].into();
        let none = HashSet::new();
        // Unseen: the listing lacks it.
        let plan = plan_sweep(
            &[],
            &held,
            &none,
            "abcd",
            1_760_100_000,
            SWEEP_GRACE_SECS,
            4,
        );
        assert_eq!(plan, SweepPlan::default());
        // Seen again, old enough to sweep were it not held: left.
        let r = VmRecord {
            slot: 0.try_into().unwrap(),
            kind: VmKind::Macos,
            claim: claim.to_string().try_into().unwrap(),
            state: VmState::InService,
            stop_reason: None,
            failed: None,
            started_unix: 1_760_000_000.try_into().unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        };
        let plan = plan_sweep(
            &[r],
            &held,
            &none,
            "abcd",
            1_760_100_000,
            SWEEP_GRACE_SECS,
            4,
        );
        assert_eq!(plan, SweepPlan::default());
    }
    use crate::vm::proto::{Adopted, JitConfig, Start, StopReason, UnixSecs};
    use std::sync::Mutex;

    const ME: &str = "hab12cde";
    const NOW: u64 = 1_760_010_000;
    const OLD: u64 = NOW - 3_600;

    const STATES: [VmState; 9] = [
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

    fn rec(slot: u8, kind: VmKind, claim: &str, state: VmState, started: u64) -> VmRecord {
        VmRecord {
            slot: Slot::try_from(slot).unwrap(),
            kind,
            claim: ClaimId::try_from(claim).unwrap(),
            state,
            stop_reason: (state == VmState::Stopped).then_some(StopReason::Requested),
            failed: None,
            started_unix: UnixSecs::try_from(started).unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        }
    }

    fn mac(slot: u8, claim: &str, state: VmState) -> VmRecord {
        rec(slot, VmKind::Macos, claim, state, OLD)
    }

    fn set(claims: &[&str]) -> HashSet<String> {
        claims.iter().map(|c| c.to_string()).collect()
    }

    /// One claim per ownership, for the tables below.
    const CLAIMS: [(&str, Ownership); 5] = [
        ("42-1760000000000-hab12cde", Ownership::Ours),
        ("42-1760000000000", Ownership::PreInstance),
        ("42-1760000000000-hpeer001", Ownership::Foreign),
        ("linux-engine", Ownership::Foreign),
        ("42-x-hab12cde", Ownership::Foreign),
    ];

    #[test]
    fn ownership_is_read_from_the_claim_and_the_kind_only() {
        for (claim, want) in CLAIMS {
            assert_eq!(
                ours(&mac(0, claim, VmState::InService), ME),
                want,
                "{claim}"
            );
            assert_eq!(
                ours(&rec(0, VmKind::Linux, claim, VmState::InService, OLD), ME),
                Ownership::Linux,
                "a Linux VM is never ours to stop, whatever it is called: {claim}"
            );
        }
    }

    #[test]
    fn boot_adoption_acts_on_each_state_of_a_vm_of_ours() {
        let c = CLAIMS[0].0;
        let table = [
            (VmState::InService, AdoptAction::Watch),
            (VmState::Stopping, AdoptAction::HoldUntilStopped),
            (VmState::Stopped, AdoptAction::Ignore),
        ];
        for (state, want) in table {
            assert_eq!(adoption_action(&mac(0, c, state), ME), want, "{state:?}");
        }
        for state in [
            VmState::Starting,
            VmState::Booting,
            VmState::Ready,
            VmState::ProbeSent,
            VmState::JobFinished,
        ] {
            assert!(
                matches!(adoption_action(&mac(0, c, state), ME), AdoptAction::Stop(_)),
                "{state:?}"
            );
        }
        assert!(matches!(
            adoption_action(&mac(0, c, VmState::Unmanaged), ME),
            AdoptAction::HoldNeverStop(_)
        ));
    }

    #[test]
    fn a_claim_without_an_instance_is_held_and_never_stopped() {
        let c = CLAIMS[1].0;
        for state in STATES {
            let a = adoption_action(&mac(0, c, state), ME);
            if state == VmState::Stopped {
                assert_eq!(a, AdoptAction::Ignore);
            } else {
                assert!(
                    matches!(a, AdoptAction::HoldNeverStop(_)),
                    "{state:?}: {a:?}"
                );
            }
        }
    }

    #[test]
    fn the_sweep_stops_only_an_old_unheld_vm_of_ours() {
        let c = CLAIMS[0].0;
        let none = HashSet::new();
        let at = |r: &VmRecord, held: &HashSet<String>, q: &HashSet<String>| {
            sweep_action(r, held, q, ME, NOW, SWEEP_GRACE_SECS)
        };
        assert!(matches!(
            at(&mac(0, c, VmState::InService), &none, &none),
            SweepAction::Stop(_)
        ));
        assert_eq!(
            at(&mac(0, c, VmState::InService), &set(&[c]), &none),
            SweepAction::Leave,
            "a live job holds it"
        );
        let young = rec(0, VmKind::Macos, c, VmState::Starting, NOW - 60);
        assert_eq!(
            at(&young, &none, &none),
            SweepAction::Leave,
            "a start in flight is never raced"
        );
        let young_but_short = sweep_action(&young, &none, &none, ME, NOW, 1);
        assert_eq!(
            young_but_short,
            SweepAction::Leave,
            "the grace is never below {MIN_SWEEP_GRACE_SECS} s"
        );
        assert_eq!(
            at(&mac(0, c, VmState::Stopping), &none, &none),
            SweepAction::Leave,
            "already on its way out"
        );
        assert!(matches!(
            at(&mac(0, c, VmState::Stopping), &none, &set(&[c])),
            SweepAction::Stop(_)
        ));
        assert_eq!(
            at(&mac(0, c, VmState::Stopped), &none, &set(&[c])),
            SweepAction::Settle
        );
        assert_eq!(
            at(&mac(0, c, VmState::Stopped), &none, &none),
            SweepAction::Leave
        );
        assert_eq!(
            at(&mac(0, c, VmState::Unmanaged), &none, &set(&[c])),
            SweepAction::Leave,
            "an unmanaged VM keeps its slot until it exits"
        );
    }

    /// Every state × kind × ownership × held × quarantined × age: a Stop is never
    /// produced for a Linux VM, an `unmanaged` one, or one that is not ours — by
    /// the sweep or by boot adoption.
    #[test]
    fn no_path_stops_a_linux_unmanaged_or_foreign_vm() {
        for kind in [VmKind::Macos, VmKind::Linux] {
            for (claim, ownership) in CLAIMS {
                for state in STATES {
                    for started in [NOW - 10, OLD, 0] {
                        let r = rec(3, kind, claim, state, started);
                        let protected = kind == VmKind::Linux
                            || state == VmState::Unmanaged
                            || ownership != Ownership::Ours;
                        for held in [set(&[]), set(&[claim])] {
                            for q in [set(&[]), set(&[claim])] {
                                for grace in [0, SWEEP_GRACE_SECS] {
                                    let a = sweep_action(&r, &held, &q, ME, NOW, grace);
                                    if protected {
                                        assert!(
                                            !matches!(a, SweepAction::Stop(_)),
                                            "{kind:?} {claim} {state:?}: {a:?}"
                                        );
                                    }
                                }
                                let plan = plan_sweep(
                                    std::slice::from_ref(&r),
                                    &held,
                                    &q,
                                    ME,
                                    NOW,
                                    SWEEP_GRACE_SECS,
                                    MAX_STOPS_PER_SWEEP,
                                );
                                if protected {
                                    assert!(plan.stop.is_empty(), "{kind:?} {claim} {state:?}");
                                }
                            }
                        }
                        let a = adoption_action(&r, ME);
                        if protected {
                            assert!(
                                !matches!(a, AdoptAction::Stop(_) | AdoptAction::Watch),
                                "{kind:?} {claim} {state:?}: {a:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_plan_caps_stops_and_settles_quarantined_claims_that_are_gone() {
        let claims: Vec<String> = (0..6).map(|n| format!("{n}-1760000000000-{ME}")).collect();
        let records: Vec<VmRecord> = claims
            .iter()
            .enumerate()
            .map(|(n, c)| mac(n as u8, c, VmState::InService))
            .collect();
        let gone = format!("9-1760000000000-{ME}");
        let plan = plan_sweep(
            &records,
            &HashSet::new(),
            &set(&[gone.as_str()]),
            ME,
            NOW,
            SWEEP_GRACE_SECS,
            MAX_STOPS_PER_SWEEP,
        );
        assert_eq!(plan.stop.len(), MAX_STOPS_PER_SWEEP);
        assert_eq!(plan.deferred, 2);
        assert_eq!(
            plan.settle.iter().map(ClaimId::as_str).collect::<Vec<_>>(),
            [gone.as_str()],
            "absent from list: confirmed gone"
        );
    }

    // --- the I/O wrappers, against a fake supervisor --------------------------

    #[derive(Default)]
    struct Fake {
        vms: Mutex<Vec<VmRecord>>,
        calls: Mutex<Vec<String>>,
        down: bool,
    }

    impl Fake {
        fn with(vms: Vec<VmRecord>) -> Self {
            Self {
                vms: Mutex::new(vms),
                ..Default::default()
            }
        }
        fn stops(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| c.strip_prefix("stop:").map(str::to_string))
                .collect()
        }
    }

    impl VmSupervisor for Fake {
        async fn start(&self, _: Start) -> Result<VmRecord, SupervisorError> {
            unreachable!("adoption and the sweep never start a VM")
        }
        async fn probe(&self, _: &ClaimId) -> Result<VmRecord, SupervisorError> {
            unreachable!()
        }
        async fn begin(
            &self,
            _: &ClaimId,
            _: Option<JitConfig>,
        ) -> Result<VmRecord, SupervisorError> {
            unreachable!()
        }
        async fn stop(&self, claim: &ClaimId) -> Result<(), SupervisorError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("stop:{}", claim.as_str()));
            let mut vms = self.vms.lock().unwrap();
            match vms.iter_mut().find(|v| &v.claim == claim) {
                None => Err(SupervisorError::Refused(ErrorCode::UnknownClaim)),
                Some(v) if v.state == VmState::Unmanaged => {
                    Err(SupervisorError::Refused(ErrorCode::Unmanaged))
                }
                Some(v) => {
                    if v.state != VmState::Stopped {
                        v.state = VmState::Stopping;
                    }
                    Ok(())
                }
            }
        }
        async fn list(&self) -> Result<Vec<VmRecord>, SupervisorError> {
            if self.down {
                return Err(SupervisorError::Unreachable);
            }
            self.calls.lock().unwrap().push("list".into());
            Ok(self.vms.lock().unwrap().clone())
        }
        async fn adopt(&self, claims: &[ClaimId]) -> Result<Adopted, SupervisorError> {
            assert!(claims.len() <= MAX_LIST);
            self.calls
                .lock()
                .unwrap()
                .push(format!("adopt:{}", claims.len()));
            let vms = self.vms.lock().unwrap();
            let (mut found, mut unknown) = (Vec::new(), Vec::new());
            for c in claims {
                match vms.iter().find(|v| &v.claim == c) {
                    Some(v) => found.push(v.clone()),
                    None => unknown.push(c.clone()),
                }
            }
            Ok(Adopted { found, unknown })
        }
    }

    fn claim(n: u64) -> String {
        format!("{n}-1760000000000-{ME}")
    }

    #[tokio::test]
    async fn boot_adoption_watches_jobs_stops_ramps_and_never_stops_unmanaged() {
        let fake = Fake::with(vec![
            mac(0, &claim(1), VmState::InService),
            mac(1, &claim(2), VmState::JobFinished),
            mac(2, &claim(3), VmState::Booting),
            mac(3, &claim(4), VmState::Unmanaged),
            mac(4, &claim(5), VmState::Stopping),
            mac(5, &claim(6), VmState::Stopped),
            mac(6, "7-1760000000000", VmState::InService),
            mac(7, "8-1760000000000-hpeer001", VmState::InService),
            rec(8, VmKind::Linux, "linux-engine", VmState::InService, OLD),
        ]);
        let jobs = VmJobs::default();
        let rebuilt = adopt_at_boot(&fake, &jobs, ME).await.unwrap();

        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].job_id, 1);
        assert_eq!(rebuilt[0].slot.get(), 0);
        assert_eq!(rebuilt[0].started_unix, OLD);

        assert_eq!(
            fake.stops(),
            [claim(2), claim(3)],
            "only the finished job and the ramp are stopped"
        );
        let (live, held) = jobs.held();
        assert_eq!(live, set(&[claim(1).as_str()]));
        assert_eq!(
            held,
            set(&[
                claim(2).as_str(),
                claim(3).as_str(),
                claim(4).as_str(),
                claim(5).as_str(),
                "7-1760000000000"
            ])
        );
        // The peer's VM and the Linux VM were never asked about.
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(calls.iter().filter(|c| c.starts_with("adopt")).count(), 1);
        assert!(calls.contains(&"adopt:6".to_string()), "{calls:?}");
        // No new job gets a held slot.
        let next = jobs.lease(&ClaimId::try_from("next").unwrap(), 0).unwrap();
        assert_eq!(next.get(), 5, "slot 5's VM is stopped; 0-4 and 6 are held");
    }

    #[tokio::test]
    async fn an_unreachable_supervisor_is_not_an_empty_one() {
        let fake = Fake {
            down: true,
            ..Default::default()
        };
        let jobs = VmJobs::default();
        assert_eq!(adopt_at_boot(&fake, &jobs, ME).await, None);
    }

    #[tokio::test]
    async fn the_sweep_waits_for_adoption_then_settles_and_stops() {
        let orphan = claim(1);
        let held_gone = claim(2);
        let unheld = claim(3);
        let unmanaged = claim(4);
        let fake = Fake::with(vec![
            mac(0, &orphan, VmState::InService),
            mac(2, &unheld, VmState::InService),
            mac(3, &unmanaged, VmState::Unmanaged),
            rec(4, VmKind::Linux, "linux-engine", VmState::InService, OLD),
        ]);
        let jobs = VmJobs::default();
        let id = |c: &str| ClaimId::try_from(c).unwrap();
        let slot = |n: u8| Slot::try_from(n).unwrap();
        assert!(jobs.hold(&id(&orphan), slot(0), VmStep::Orphaned));
        assert!(jobs.hold(&id(&held_gone), slot(1), VmStep::Orphaned));
        assert!(jobs.hold(&id(&unmanaged), slot(3), VmStep::Unmanaged));

        jobs.hold_for_adoption();
        assert_eq!(sweep_once(&fake, &jobs, ME, NOW, |_| {}).await, None);
        assert!(fake.calls.lock().unwrap().is_empty(), "not even a list");
        jobs.adoption_settled();

        // Each listing is published before it is acted on (`/api/state`).
        let mut listed = 0;
        let plan = sweep_once(&fake, &jobs, ME, NOW, |r| listed = r.len())
            .await
            .unwrap();
        assert_eq!(listed, 4, "the whole listing");
        assert_eq!(
            plan.settle.iter().map(ClaimId::as_str).collect::<Vec<_>>(),
            [held_gone.as_str()]
        );
        assert_eq!(fake.stops(), [orphan.clone(), unheld.clone()]);
        assert_eq!(jobs.quarantined(), [0, 3], "slot 1 is free again");

        // The orphan reaches `stopped`: the next sweep settles it.
        fake.vms.lock().unwrap()[0].state = VmState::Stopped;
        let plan = sweep_once(&fake, &jobs, ME, NOW, |_| {}).await.unwrap();
        assert_eq!(
            plan.settle.iter().map(ClaimId::as_str).collect::<Vec<_>>(),
            [orphan.as_str()]
        );
        assert_eq!(jobs.quarantined(), [3], "the unmanaged VM is still held");
        assert!(
            !fake.stops().contains(&unmanaged),
            "never asked to stop an unmanaged VM"
        );
    }
}
