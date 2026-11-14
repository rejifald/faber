//! The janitor — periodic reaping of leaked per-job Docker resources.
//!
//! [`crate::docker`]'s teardown removes a job's sandbox on every in-process
//! path, but it cannot cover a crashed faber, a forced shutdown, or a Docker
//! daemon restart: those leak exited `faber-*` containers, `faber-net-*`
//! networks and `faber-vol-*` volumes. And before the dind volume was named,
//! every job leaked its anonymous `/var/lib/docker` volume outright (hundreds of
//! GB across about a thousand volumes when first measured). The sweep runs once
//! at startup — reclaiming that backlog — and then on a fixed cadence.
//!
//! OWNERSHIP: candidates are found by the labels ADR-0007 stamps on everything
//! faber creates — `org.faber.owned`, `org.faber.kind`, `org.faber.id` — and the
//! job id is read from `org.faber.id` rather than carved back out of a name.
//! Ownership is then a fact the daemon stores, not a naming convention this
//! module has to defend against collisions; the pre-label name listings survive
//! only as a fallback, for the reasons [`candidates`] gives.
//!
//! ORDERING: candidates are listed *before* the in-flight snapshot is taken. A
//! resource present in a listing belongs to a job claimed before the snapshot,
//! so a live job is always in the snapshot and never touched; a job that
//! finished in between has already torn its resources down and the reap is a
//! no-op. Containers are reaped first — freeing them is what lets their
//! network and volume go, which is why the volume listing carries no
//! `dangling=true` filter (see [`volume_ls_args`]).
//!
//! Running runner containers are left alone: after a forced shutdown a previous
//! faber's runner may still be finishing a real job. Its network and volume
//! refuse removal while it lives ("active endpoints" / "in use"), which the
//! sweep treats as a skip, not an error. Running dind containers are the
//! exception — a dind's `dockerd` never exits on its own, so an orphaned one is
//! invisible to the exited-only listing and would leak forever; it is reaped
//! when its paired runner is gone (see [`running_dind_orphans`]).
//!
//! "Running" includes frozen: a container the critical floor paused (ADR-0032
//! §2) is a live job, only stopped. Its runner shields its dind like any other,
//! and a dind frozen beside a runner that has gone is reaped like any other —
//! which is why the running listings ask for both states
//! ([`crate::docker::LIVE_STATUSES`]).
//!
//! The final `docker volume prune` (no `--all`) removes only *dangling
//! anonymous* volumes — Docker Engine ≥ 23 semantics. Named volumes and
//! anything still referenced by a container, even a stopped one, are never
//! candidates.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::cache;
use crate::docker::{
    docker_lines, docker_rm, docker_stdout, live_status_filters, CLI_TIMEOUT, KIND_DIND, KIND_HOOK,
    KIND_NET, KIND_REGISTRY_AUTH, KIND_RUNNER, KIND_VOL, LABEL_ID, LABEL_KIND, LABEL_OWNED,
    TEARDOWN_TIMEOUT,
};
use crate::host;
use crate::image::EvictTier;
use crate::limiter::Pause;
use crate::reconcile::Orchestrator;
use crate::services::KIND_SERVICE;
use crate::untrusted;

/// Sweep once immediately, then every `interval`, forever. Spawned by the
/// daemon alongside the reconcile loop when a Docker profile is configured.
pub async fn run_loop(orch: Arc<Orchestrator>, interval: Duration) {
    let tick = janitor_tick(interval);
    info!(
        "janitor: checking disk every {tick:?}, sweeping leaked docker resources \
         every {interval:?}"
    );
    // Said once, at boot: the janitor is the only thing that ages the cache, so
    // this is where a limit it will not honour gets called out (ADR-0024 §4).
    let policy = orch.host_policy();
    let cache_age = policy.cache_max_age_days;
    if let Some(warning) = cache::boot_warning(cache_age) {
        warn!("janitor: {warning}");
    }
    let limits = cache::Limits::from_config(cache_age, policy.cache_max_gib);
    if let Some(days) = limits.max_age_days {
        info!(
            "janitor: ageing out cache-volume entries nothing has used for {days} days \
             (ADR-0024)"
        );
    }
    if let Some(gib) = policy.cache_max_gib.filter(|&g| g > 0) {
        info!(
            "janitor: keeping each cache volume to {gib} GiB, least recently used first \
             (ADR-0055)"
        );
    }
    if !cache::operator_set(cache_age, policy.cache_max_gib) {
        info!(
            "janitor: no cache limit set — cache entries are freed, least recently used \
             first, only when the disk falls below its floor (ADR-0055)"
        );
    }
    // Start with a full interval of credit so the first pass sweeps immediately:
    // a previous process — or a Docker restart — may have leaked, and reclaiming
    // that is exactly what a startup sweep is for.
    let mut since_sweep = interval;
    // What the last fruitless reclamation pass left behind (ADR-0024 §3). Local to
    // this loop, like `since_sweep`: it only ever describes this process's passes.
    let mut backoff = Backoff::default();
    loop {
        // Disk first, every tick. It is the cheapest of the three checks and the
        // only one whose delay can cost the daemon itself, so it must never queue
        // behind a long reap.
        //
        // All three stay in ONE task rather than running the disk check
        // concurrently: it and `evict_cold_variants` both take the admission
        // pause, and `PauseForReclaim` resumes on drop. Two concurrent holders
        // would let whichever finished first resume admission while the other was
        // still deleting — a bulk delete against a live pool, the precise hazard
        // ADR-0007 records having already cost a few jobs once.
        relieve_disk_pressure(&orch, &mut backoff, interval).await;
        if since_sweep >= interval {
            sweep(&orch).await;
            // After the reap, which is free: faber's own leavings go first, and
            // then whatever in the cache nothing has used for the window. It logs
            // what it removed itself; there is no reclamation line to add it to.
            sweep_cache_volumes(&orch).await;
            tend_git_mirrors(&orch).await;
            evict_cold_variants(&orch).await;
            since_sweep = Duration::ZERO;
        }
        tokio::time::sleep(tick).await;
        since_sweep = since_sweep.saturating_add(tick);
    }
}

/// Reclaim CI's own Docker leavings before the volume fills — ADR-0007.
///
/// The janitor above keeps faber tidy: it reaps the containers, networks and
/// volumes faber itself leaked. That is not the same as keeping the *host*
/// alive. Images, build cache and dangling layers accumulate with every run and
/// nothing was watching the total, so the disk filled until the daemon wedged —
/// the failure ADR-0007 exists to stop.
///
/// Only runs when free space is actually low, so a healthy host pays one `df`
/// per sweep and nothing else. What it prunes is deliberately narrow; see
/// [`host::reclaim_ci_disk`] for why nothing here can touch a data volume or an
/// image faber didn't create.
///
/// A pass that frees nothing is not repeated every tick (ADR-0024 §3): `backoff`
/// remembers where it left free space, and the next pass waits until the disk is
/// measurably worse or `interval` — the sweep interval — has gone by. On the host
/// ADR-0024 was written for, the space was held by data that was never CI's, and
/// about a hundred passes in a row each held admission for half a minute or so to
/// delete nothing.
async fn relieve_disk_pressure(
    orch: &Arc<Orchestrator>,
    backoff: &mut Backoff,
    interval: Duration,
) {
    let policy = orch.host_policy();
    if policy.disk_floor_gib == 0 {
        return;
    }
    // The Linux VM's disk first, where there is one (#410): it is what Docker
    // jobs fill, and the reading the limiter keeps may be ten seconds old. A
    // failure is a fault of its own, not pressure: the pass then weighs the host
    // volume alone, as it did before there were two. Not tried while Docker is
    // down: that has a fault of its own, and the probe would only time out.
    let docker_up = orch.limiter().docker_reachable();
    let vm = if docker_up {
        host::read_linux_vm_disk(orch.probe_image()).await
    } else {
        host::VmDisk::Absent
    };
    let vm_now = match &vm {
        host::VmDisk::Read(r) => Some(*r),
        _ => None,
    };
    match vm {
        host::VmDisk::Unreadable(why) => {
            orch.observe_fault(
                "host",
                "vm-disk",
                Some(format!(
                    "cannot read free space on the Linux VM's disk: {}",
                    crate::untrusted::for_log(&why, 200)
                )),
            )
            .await
        }
        host::VmDisk::Read(_) => orch.observe_fault("host", "vm-disk", None).await,
        host::VmDisk::Absent => {}
    }
    // `None` means neither volume could be read, or the platform layer couldn't
    // tell which volume Docker actually writes to (Linux without a readable
    // `docker info`, chiefly — see `crate::platform::host`'s module doc).
    // Guessing a path to `df` would risk watching the wrong volume entirely,
    // which is worse than skipping this tick's check: the same "don't act on an
    // unknown reading" policy `below_disk_floor` already applies to a failed
    // `df`.
    let tightest = host::docker_disk_free().await;
    let reading = judged_reading(tightest, vm_now);
    let free = reading.map(|r| r.free_mib);
    let on = reading.map_or("the Docker volume", |r| r.volume.describe());
    // ADR-0024 (amendment of 2026-10-31): the Mac volume holding the VM disks is
    // short, the Linux VM's disk is not. Nothing a pass deletes inside the VM
    // returns space to the Mac — the VM's disk file is sparse and never shrinks
    // (#423) — so a pass would hold admission and evict variants and mirrors for
    // nothing, every few minutes, for as long as the Mac stays full. Report it and
    // leave it: the admission gate and the macOS VM gate already read that volume.
    if let Some(mac) = tightest.filter(|t| t.volume == host::DiskVolume::MacVms) {
        if host::below_disk_floor(policy.disk_floor_gib, Some(mac.free_mib))
            && !host::below_disk_floor(policy.disk_floor_gib, free)
        {
            debug!(
                "janitor: {} MiB free on {}, below the {} GiB floor — not reclaiming: \
                 nothing deleted inside the Linux VM returns space to it",
                mac.free_mib,
                mac.volume.describe(),
                policy.disk_floor_gib
            );
            orch.observe_fault(
                "host",
                "disk",
                Some(format!(
                    "{} MiB free on {}, floor is {} GiB; faber reclaims inside the Linux VM, \
                     which cannot return space to the Mac — free space on the Mac",
                    mac.free_mib,
                    mac.volume.describe(),
                    policy.disk_floor_gib
                )),
            )
            .await;
            return;
        }
    }
    if !host::below_disk_floor(policy.disk_floor_gib, free) {
        // Clear a standing disk fault once the host is demonstrably healthy
        // again. Without this the fault raised below can only ever be cleared by
        // a *later pass that frees something*, so pressure that simply lifts —
        // the operator deletes a VM image, a build finishes — leaves faber
        // reporting a disk problem it no longer has, for the rest of the run.
        // A health signal that cannot go green is one an operator learns to
        // ignore, which costs more than the fault was worth.
        //
        // Gated on `free.is_some()` rather than on `below_disk_floor` alone: an
        // unreadable `df` also reads as "not below the floor", and calling that
        // a recovery would be asserting health from a missing measurement — the
        // same guess `below_disk_floor` refuses to make in the other direction.
        // `observe_fault` is transition-gated, so the healthy ticks after the
        // first cost nothing.
        //
        // The same reading ends a backoff: once the host is above the floor, the
        // next time it falls below is a new episode and gets a pass at once.
        if free.is_some() {
            backoff.clear();
            orch.observe_fault("host", "disk", None).await;
        }
        return;
    }
    let before = free.unwrap_or(0);
    // ADR-0024 §3. Nothing has changed since a pass that freed nothing, so
    // another one would free nothing again — and still take the admission pause
    // to do it. The disk fault that pass raised stays raised meanwhile.
    if !backoff.allows(before, Instant::now(), interval) {
        // ADR-0061 §2: a pass that backed off counts as one that freed nothing.
        crate::vm::grow::global().janitor(false, crate::audit::now_ms());
        debug!(
            "janitor: {before} MiB free on {on}, still below the {} GiB floor — the last \
             pass freed nothing, so the next waits for free space to fall {RETRY_DROP_MIB} MiB \
             below where it left it, or {interval:?}",
            policy.disk_floor_gib
        );
        return;
    }
    warn!(
        "janitor: {} MiB free on {on}, below the {} GiB floor — reclaiming CI disk",
        before, policy.disk_floor_gib
    );

    // Hold admission shut across the reclamation, and let any provisioning ramp
    // already under way finish before deleting anything. Learned the hard way: a
    // bulk cleanup run against a live pool made a few jobs fail with `network
    // faber-net-… not found`, the per-job network vanishing in the seconds
    // between faber creating it and starting dind. Those jobs self-healed on
    // retry, but a reclamation that costs CI runs is not maintenance, it is an
    // outage with a tidy motive.
    let _pause = PauseForReclaim::take(orch);
    tokio::time::sleep(RAMP_SETTLE).await;

    let mut reclaimed = host::reclaim_ci_disk().await;
    // ADR-0024 §4, ADR-0055: the operator's cache limits, or, when none is set,
    // the least recently used entries sized to what the floor still needs. After
    // the free prunes, before mirrors and variants — a cache entry costs a slower
    // run, a mirror a clone, a variant a build. An operator's limits are the same
    // ones the sweep uses: disk pressure never tightens them.
    // Sized from a fresh reading, after the prunes above, not from `before`: what
    // they freed is already counted. In faber mode only.
    let needed_mib = if cache::operator_set(policy.cache_max_age_days, policy.cache_max_gib) {
        0
    } else {
        // A failed VM reading would leave `docker_disk_free` the cached one from
        // before the prunes, so it deletes nothing rather than size from that.
        let vm_fresh = !docker_up
            || matches!(
                host::read_linux_vm_disk(orch.probe_image()).await,
                host::VmDisk::Read(_) | host::VmDisk::Absent
            );
        // Weighed on the disk the deletions can raise (#427): sized from the Mac
        // volume, a pass would delete another `needed` worth every time and never
        // see it come back.
        let fresh = if vm_fresh {
            judged_reading(host::docker_disk_free().await, host::latest_linux_vm_disk())
                .map(|r| r.free_mib)
        } else {
            None
        };
        cache_needed_mib(policy.disk_floor_gib, fresh)
    };
    if let Some(aged) = relieve_cache_volumes(orch, needed_mib).await {
        reclaimed.freed.push(aged);
    }
    // ADR-0035 §5: every git mirror nothing mounts. Before variants: a mirror
    // costs one clone to rebuild and a variant a build, and both come before
    // anything the operator declared.
    let mirrors = crate::git_mirror::global().reclaim().await;
    if !mirrors.is_empty() {
        reclaimed
            .freed
            .push(format!("git mirrors: {}", mirrors.join(", ")));
    }
    // ADR-0008 (amendment of 2026-10-30): under the margin a variant build must
    // leave, every variant no job holds goes — a variant costs a rebuild, a full
    // disk costs the daemon, and this host has already lost it that way once.
    // Above that margin only the cold ones do: a rebuild would be allowed there,
    // so evicting a warm variant buys nothing but the same build again; under a
    // floor the eviction cannot lift, the two repeat for ever. Weighed on the
    // reading this pass opened with, which the prunes above only improve on.
    let evicted = {
        let cache = orch.images();
        if cache.enabled() {
            let claimed = orch.claimed_images().await;
            let all = cache
                .evictable(cache.pressure_tier(Some(before)), &claimed)
                .await;
            cache.evict(&all).await
        } else {
            Vec::new()
        }
    };
    if !evicted.is_empty() {
        reclaimed
            .freed
            .push(format!("runner-image variants: {}", evicted.join(", ")));
    }
    // An unreadable `df` is not a recovery: it keeps the reading the pass began
    // with, which is below the floor.
    // The Linux VM's disk is read again first, so what the pass freed inside it
    // counts.
    let vm_after = if docker_up {
        match host::read_linux_vm_disk(orch.probe_image()).await {
            host::VmDisk::Read(r) => Some(r),
            _ => None,
        }
    } else {
        None
    };
    let after =
        judged_reading(host::docker_disk_free().await, vm_after).map_or(before, |r| r.free_mib);
    let verdict = pass_verdict(!reclaimed.is_empty(), before, after, policy.disk_floor_gib);
    // ADR-0061 §2: what the janitor's last pass came to, for the grow trigger.
    crate::vm::grow::global().janitor(verdict != PassVerdict::Fruitless, crate::audit::now_ms());
    match verdict {
        PassVerdict::Freed => {
            info!(
                "janitor: reclaimed {} — {before} -> {after} MiB free",
                reclaimed.freed.join(", ")
            );
            orch.observe_fault("host", "disk", None).await;
            backoff.clear();
        }
        PassVerdict::PressureLifted => {
            // Space came back during the pass from somewhere faber did not touch
            // — a job finished, the operator deleted something. Reporting this as
            // "freed nothing, still below the floor" with a figure above it is how,
            // say, 75,000 MiB once read as "still below the 25 GiB floor".
            info!(
                "janitor: reclaimed nothing, but {after} MiB is free again on {on} — the \
                 pressure lifted during the pass"
            );
            orch.observe_fault("host", "disk", None).await;
            backoff.clear();
        }
        PassVerdict::Fruitless => {
            // This is the state that let a host walk down to a few GiB free while
            // tens of GB of reclaimable images and volumes sat in plain sight: a
            // pass that frees nothing used to be one WARN among thousands, and it
            // fired dozens of times over days without anyone seeing it. It is a
            // fault now — collapsed across repeats by `observe_fault`, surfaced on
            // the dashboard, and cleared the moment a later pass frees something —
            // because faber failing at the one job ADR-0007 gave it should not be
            // quieter than a job that failed.
            let mut hint = String::new();
            if cache::operator_set(policy.cache_max_age_days, policy.cache_max_gib) {
                let limits =
                    cache::Limits::from_config(policy.cache_max_age_days, policy.cache_max_gib);
                let age = limits.max_age_days.map_or("no age limit".to_string(), |d| {
                    format!("a {d}-day age limit")
                });
                let size = policy
                    .cache_max_gib
                    .filter(|&g| g > 0)
                    .map_or("no size limit".to_string(), |g| {
                        format!("a {g} GiB size limit")
                    });
                hint.push_str(&format!(
                    " The cache has {age} and {size} (ADR-0024, ADR-0055)."
                ));
                if limits.max_kib.is_none() {
                    hint.push_str(
                        " Set host.cache_max_gib to bound a cache that is read on every run.",
                    );
                }
            }
            // Something may have gone — a git mirror the next job clones again, a
            // variant it rebuilds — without the disk rising by what a pause is
            // worth. Said as such, rather than "nothing", so the log matches the
            // list of what was deleted.
            let what = if reclaimed.is_empty() {
                "reclaimed nothing".to_string()
            } else {
                format!(
                    "reclaimed {} but free space rose only {} MiB",
                    reclaimed.freed.join(", "),
                    after.saturating_sub(before)
                )
            };
            warn!(
                "janitor: {what}, and {after} MiB free is still below the \
                 {} GiB floor — not trying again until it falls {RETRY_DROP_MIB} MiB further \
                 or {interval:?} passes.{hint}",
                policy.disk_floor_gib
            );
            orch.observe_fault(
                "host",
                "disk",
                Some(format!(
                    "disk reclamation freed nothing; {after} MiB free on {on}, floor is {} GiB",
                    policy.disk_floor_gib
                )),
            )
            .await;
            backoff.fruitless(after, Instant::now());
        }
    }
}

/// How one reclamation pass ended — ADR-0024 §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassVerdict {
    /// Free space rose by at least [`RETRY_DROP_MIB`], or the pass reclaimed
    /// something and left the disk at or above the floor. Clears the disk fault:
    /// faber is doing its job, and the next tick measures again.
    Freed,
    /// Nothing was reclaimed, but free space is back at or above the floor. The
    /// pressure lifted during the pass by some other hand; the fault clears, and
    /// "freed nothing" would be a false alarm.
    PressureLifted,
    /// Free space is still under the floor and rose by less than
    /// [`RETRY_DROP_MIB`], whether or not anything was deleted (amendment of
    /// 2026-10-31). Raises the fault, and starts the backoff.
    Fruitless,
}

/// Judge a finished pass: did it free anything, and where did it take free space
/// from (`before_mib`) and leave it (`after_mib`) against the floor? Split from
/// the I/O so the outcomes are testable, and reads "below the floor" through
/// [`host::below_disk_floor`] so this and the check that started the pass cannot
/// disagree about it.
///
/// Deleting something is not the same as freeing room. A git mirror the next job
/// clones again, or a variant it rebuilds, is "reclaimed" and leaves the disk where
/// it was; counted as freed, it cleared the backoff, and in October 2026 a host
/// held admission for half a minute of every few minutes for hours with free
/// space flat within a fraction of a GiB of the 25 GiB floor.
fn pass_verdict(freed: bool, before_mib: u64, after_mib: u64, floor_gib: u64) -> PassVerdict {
    if !host::below_disk_floor(floor_gib, Some(after_mib)) {
        if freed {
            PassVerdict::Freed
        } else {
            PassVerdict::PressureLifted
        }
    } else if freed && after_mib >= before_mib.saturating_add(RETRY_DROP_MIB) {
        PassVerdict::Freed
    } else {
        PassVerdict::Fruitless
    }
}

/// The reading a reclamation pass is judged on: the one its deletions can raise.
///
/// `tightest` is what the admission gate binds on, the lower of the Mac volume
/// holding the VM disks and the Linux VM's disk (ADR-0024's amendment of
/// 2026-10-28). Everything the pass deletes lives inside the Linux VM, whose disk
/// file never shrinks on the Mac (#423), so when the Mac volume is the tighter one
/// and `vm` — this tick's reading of the VM's disk — is in hand, the pass weighs
/// the VM's disk. Without a VM reading it weighs `tightest`, as before: an
/// unreadable VM disk is no evidence that it has room.
fn judged_reading(
    tightest: Option<host::DiskReading>,
    vm: Option<host::DiskReading>,
) -> Option<host::DiskReading> {
    match (tightest, vm) {
        (Some(t), Some(vm)) if t.volume == host::DiskVolume::MacVms => Some(vm),
        _ => tightest,
    }
}

/// How far free space must fall below where a fruitless pass left it before the
/// janitor tries again inside the sweep interval — ADR-0024 §3.
///
/// A disk that is getting measurably worse may have new CI leavings on it — a
/// finished job's layers, a build's cache — which is exactly what a pass can free.
/// One that is holding steady or recovering has nothing new for it.
const RETRY_DROP_MIB: u64 = 1024;

/// The janitor's memory of its last reclamation pass that freed nothing —
/// ADR-0024 §3.
///
/// Before this, a pass that freed nothing was simply run again two minutes later,
/// with the same admission pause and the same result. The incident host ran about
/// a hundred of them back to back, roughly an hour of CI held to delete nothing.
#[derive(Debug, Default)]
struct Backoff {
    last: Option<Fruitless>,
}

/// Where a fruitless pass left free space, and when.
#[derive(Debug, Clone, Copy)]
struct Fruitless {
    free_mib: u64,
    at: Instant,
}

impl Backoff {
    /// Whether a pass may run now, with `free_mib` free, `interval` being the sweep
    /// interval. Always, unless the last pass freed nothing — then only once free
    /// space has fallen [`RETRY_DROP_MIB`] below where it left it, or `interval`
    /// has passed since.
    fn allows(&self, free_mib: u64, now: Instant, interval: Duration) -> bool {
        match self.last {
            None => true,
            Some(last) => {
                free_mib.saturating_add(RETRY_DROP_MIB) <= last.free_mib
                    || now.saturating_duration_since(last.at) >= interval
            }
        }
    }

    /// A pass freed nothing and left `free_mib` free.
    fn fruitless(&mut self, free_mib: u64, at: Instant) {
        self.last = Some(Fruitless { free_mib, at });
    }

    /// A pass freed something, or free space is at or above the floor: the next
    /// pass, whenever one is needed, runs at once.
    fn clear(&mut self) {
        self.last = None;
    }
}

/// Free space the pressure pass aims to leave above the floor when it is faber,
/// not the operator, deciding how much cache to delete (ADR-0055 §2): 5 GiB.
const CACHE_RELIEF_MARGIN_MIB: u64 = 5 * 1024;

/// The served repos, for the pre-label cache volumes. Losing them narrows a pass to
/// the labelled volumes rather than stopping it.
async fn served_for_cache(orch: &Orchestrator) -> Vec<String> {
    match orch.served_repos().await {
        Ok(repos) => repos,
        Err(e) => {
            warn!(
                "janitor: could not list the repos faber serves ({e}) — trimming only labelled \
                 cache volumes this pass"
            );
            Vec::new()
        }
    }
}

/// How much cache the pressure pass should free in faber mode, in MiB: what it
/// takes to bring `free_mib` (read after the cheap reclaims) up to the floor plus
/// [`CACHE_RELIEF_MARGIN_MIB`]. `None` — an unreadable disk — is zero: faber does
/// not delete cache on a guess.
fn cache_needed_mib(floor_gib: u64, free_mib: Option<u64>) -> u64 {
    free_mib.map_or(0, |free| {
        floor_gib
            .saturating_mul(1024)
            .saturating_add(CACHE_RELIEF_MARGIN_MIB)
            .saturating_sub(free)
    })
}

/// The sweep's cache step: the operator's limits (ADR-0024 §4, ADR-0055 §1), after
/// every sweep. With no key set it does nothing: faber mode acts only under
/// pressure.
///
/// A size-driven delete picks entries by recency alone, so it runs with admission
/// paused and after the settle wait, as the pressure pass does: no job can mount
/// the volume between the mount check and the delete. The scans and age-only
/// deletes take no pause. This is the only entry point that pauses; the pressure pass
/// already holds the pause and calls [`trim_cache_volumes`] directly, so the two
/// never double-pause.
async fn sweep_cache_volumes(orch: &Orchestrator) {
    let policy = orch.host_policy();
    let limits = cache::Limits::from_config(policy.cache_max_age_days, policy.cache_max_gib);
    if !limits.any() {
        return;
    }
    let served = served_for_cache(orch).await;
    // The scans can take minutes on a big volume and change nothing, so they run
    // with admission open; only a plan holding a size-driven delete pauses it.
    let plans = cache::trim_plan(limits, orch.probe_image(), &served).await;
    if plans.is_empty() {
        return;
    }
    let _pause = cache::needs_pause(&plans).then(|| PauseForReclaim::take(orch));
    if _pause.is_some() {
        tokio::time::sleep(RAMP_SETTLE).await;
    }
    let aged = cache::trim_execute(plans, limits, orch.probe_image()).await;
    if !aged.is_empty() {
        info!("janitor: {}", aged.summary());
    }
}

/// Apply the operator's cache limits. `None` unless something was removed;
/// otherwise the fragment for a reclamation line. What it removed, it has logged.
/// Takes no pause itself: see [`sweep_cache_volumes`].
async fn trim_cache_volumes(orch: &Orchestrator) -> Option<String> {
    let policy = orch.host_policy();
    let limits = cache::Limits::from_config(policy.cache_max_age_days, policy.cache_max_gib);
    if !limits.any() {
        return None;
    }
    let served = served_for_cache(orch).await;
    let aged = cache::trim(limits, orch.probe_image(), &served).await;
    (!aged.is_empty()).then(|| aged.summary())
}

/// The disk-pressure pass's cache step (ADR-0055 §2): the operator's limits when
/// either key is set, otherwise the least recently used entries sized to
/// `needed_mib` (zero when the pass's own reading is already enough).
async fn relieve_cache_volumes(orch: &Orchestrator, needed_mib: u64) -> Option<String> {
    let policy = orch.host_policy();
    if cache::operator_set(policy.cache_max_age_days, policy.cache_max_gib) {
        return trim_cache_volumes(orch).await;
    }
    if needed_mib == 0 {
        return None;
    }
    let served = served_for_cache(orch).await;
    let aged = cache::relieve(needed_mib.saturating_mul(1024), orch.probe_image(), &served).await;
    (!aged.is_empty()).then(|| aged.summary())
}

/// The git mirrors' share of the sweep (ADR-0035 §4, §5): delete each mirror no
/// `git_mirror` profile serves any more, once nothing mounts it, and run each
/// served mirror's daily prune while nothing mounts it.
///
/// With mirrors off, every mirror volume is unserved, which is how turning the
/// key off cleans up. On a host that never turned it on the listing is empty.
/// A watched-repo listing that fails deletes nothing this pass.
async fn tend_git_mirrors(orch: &Orchestrator) {
    let mirrors = crate::git_mirror::global();
    let served = if mirrors.enabled() {
        match orch.served_repos().await {
            Ok(repos) => Some(repos),
            Err(e) => {
                warn!(
                    "janitor: could not list the repos faber serves ({e}) — deleting no git \
                     mirror this pass"
                );
                None
            }
        }
    } else {
        Some(Vec::new())
    };
    mirrors.janitor(served.as_deref()).await;
}

/// Evict the runner-image variants that have gone cold — ADR-0008.
///
/// The one image faber is ever allowed to remove is one it built itself, because
/// that is the only image it can rebuild: the operator's base exists on this
/// builder and in no registry, and removing it would take CI down with nothing to
/// re-pull (AGENTS.md, ADR-0007). Reproducibility is the licence, and
/// `org.faber.kind=image` is how a variant is told apart from everything else.
///
/// Candidates are computed *before* deciding to pause, so a host whose version set
/// is stable — which is most hosts, most of the time — pays two metadata calls a
/// sweep and never disturbs admission. When there is something to remove, the
/// deletion takes the same pause-and-settle as bulk disk reclamation: an image can
/// vanish between a job resolving it and starting its container, and a
/// reclamation that costs CI runs is an outage with a tidy motive.
async fn evict_cold_variants(orch: &Arc<Orchestrator>) {
    let cache = orch.images();
    if !cache.enabled() {
        return;
    }
    if cache
        .evictable(EvictTier::Cold, &orch.claimed_images().await)
        .await
        .is_empty()
    {
        return;
    }
    let _pause = PauseForReclaim::take(orch);
    tokio::time::sleep(RAMP_SETTLE).await;
    // Re-decide after the settle rather than acting on the pre-pause list: a claim
    // that was parked when it was computed may have resolved onto one of those
    // variants since, which makes it the hottest thing in the cache and not a
    // candidate at all. The first call only answered "is there work here".
    let claimed = orch.claimed_images().await;
    let gone = cache
        .evict(&cache.evictable(EvictTier::Cold, &claimed).await)
        .await;
    if !gone.is_empty() {
        info!(
            "janitor: evicted cold runner-image variants — {}",
            gone.join(", ")
        );
    }
}

/// How long to let an in-flight provisioning ramp finish after admission is shut
/// before deleting anything.
///
/// A fixed window rather than a precise wait, because `admitted` stays true for
/// a claim's whole run — faber cannot currently distinguish "half-way through
/// creating its sandbox" from "running a twenty-minute job", and waiting for the
/// latter would mean never reclaiming on a busy host. The ramp is a network
/// create, a volume create, a dind start and a readiness poll: seconds, and this
/// is an order of magnitude more.
const RAMP_SETTLE: Duration = Duration::from_secs(30);

/// How often the disk-pressure check runs, independent of the sweep's cadence.
///
/// The sweep is the expensive half — it lists and reaps containers, networks and
/// volumes — so it stays on the operator's `sweep_interval_secs` (900s by
/// default). The disk check is one `df` on a healthy host, and it has to be much
/// tighter than that. Measured under an ordinary CI ramp: free space fell from
/// above the floor to well under it in roughly fifteen minutes, so a 900s
/// cadence can sleep through most of a slide. The check also used to sit
/// *behind* the sweep in the same pass, which made its real period the interval
/// plus however long reaping took — a few minutes more, observed.
///
/// Checking first and more often costs one `df` per tick on a healthy host and
/// buys back the reaction time a fast-filling disk consumes.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(120);

/// The janitor's tick: fast enough for the disk check, but never slower than the
/// sweep cadence the operator actually asked for. Without the `min`, setting
/// `sweep_interval_secs` below [`DISK_CHECK_INTERVAL`] would silently stretch it.
fn janitor_tick(sweep_interval: Duration) -> Duration {
    DISK_CHECK_INTERVAL.min(sweep_interval)
}

/// Holds admission shut for a bulk reclamation and lifts it on drop — including
/// on an early return or a panic, which is the whole reason it is a guard and
/// not two statements. While it is held nothing new starts: no claim is taken,
/// and a claim already parked stays parked and says why (ADR-0023).
///
/// Never lifts a pause the *operator* set: if faber was already paused when the
/// janitor arrived, that is someone's deliberate decision and resuming would
/// silently overrule it. The same goes for a pause the operator sets while the
/// reclamation runs, which replaces the janitor's, so the drop leaves it standing.
struct PauseForReclaim<'a> {
    orch: &'a Orchestrator,
    /// Whether this guard is what paused faber, and so what may resume it.
    ours: bool,
}

impl<'a> PauseForReclaim<'a> {
    fn take(orch: &'a Orchestrator) -> Self {
        let ours = orch.pause(Pause::Reclaim);
        if ours {
            info!("janitor: pausing admission for the duration of the reclamation");
        }
        Self { orch, ours }
    }
}

impl Drop for PauseForReclaim<'_> {
    fn drop(&mut self) {
        if self.ours && self.orch.resume(Pause::Reclaim) {
            self.orch.trigger_reconcile();
            info!("janitor: reclamation done — admission resumed");
        }
    }
}

/// One pass: reap every faber-owned container/network/volume whose job is no
/// longer in flight, then prune dangling anonymous volumes.
async fn sweep(orch: &Orchestrator) {
    // Every listing is taken as a pair — faber's labels first, the legacy name
    // prefix second (see [`candidates`]). Containers are limited to the
    // non-running states: a running container may be a prior process's runner
    // legitimately finishing its job (see [`container_ps_args`] and the module
    // doc).
    let containers = candidates(&container_ps_args(), &legacy_container_ps_args()).await;
    let networks = candidates(&network_ls_args(), &legacy_network_ls_args()).await;
    let mut volumes = candidates(&volume_ls_args(), &legacy_volume_ls_args()).await;
    // The registry-credential volumes an earlier release made (none is made while
    // `[registry_auth]` is held): labelled from their first moment, so there is
    // no legacy listing to pair with. A separate
    // listing because repeated `label=` filters are ANDed (see
    // [`container_ps_args`]).
    volumes.extend(list(&credential_volume_ls_args()).await);
    // The job-started hook volumes (ADR-0059 §1), labelled from their first
    // moment like the credential volumes, and listed on their own for the same
    // reason.
    volumes.extend(list(&kind_volume_ls_args(KIND_HOOK)).await);
    // A dind's `dockerd` never exits on its own, so a dind orphaned by a crash /
    // forced shutdown / skipped teardown is invisible to the exited-only
    // `containers` listing above and would leak forever (its memory and its
    // inner job containers). List the *running* dind, plus the *running* runners
    // whose paired dind must be kept alive, so `running_dind_orphans` can reap a
    // dind whose runner is gone. Listed before the snapshot, like every other
    // candidate, to preserve the "live job is always in the snapshot" invariant.
    let running_dind = candidates(&running_dind_ps_args(), &legacy_running_dind_ps_args()).await;
    // The same leak for a per-job service container (ADR-0046 §10): a Redis or a
    // Mailpit runs until killed, so one whose runner is gone is invisible to the
    // exited-only listing. Labelled from its first moment, so there is no legacy
    // listing to pair with.
    let running_services = list(&running_service_ps_args()).await;
    // The runners only ever shield, so they are read whole or not at all (#105).
    let running_runners = shield(&running_runner_ps_args(), &legacy_running_runner_ps_args()).await;
    // Snapshot *after* listing — the ordering that makes live jobs untouchable.
    let active = orch.inflight_job_ids().await;

    let (mut c, mut n, mut v) = (0usize, 0usize, 0usize);
    for name in orphans(containers, &active) {
        match docker_rm(&["rm", "-f", "-v", &name]).await {
            Ok(removed) => c += removed as usize,
            Err(e) => warn!("sweep: container {name}: {e}"),
        }
    }
    // Reap running dind whose runner has gone — the leak the exited-only listing
    // can't catch. Before the network/volume reaps, so a just-freed dind's
    // network and volume become removable in this same pass.
    for name in dinds_to_reap(running_dind, &active, running_runners.as_deref()) {
        match docker_rm(&["rm", "-f", "-v", &name]).await {
            Ok(removed) => c += removed as usize,
            Err(e) => warn!("sweep: dind {name}: {e}"),
        }
    }
    for name in services_to_reap(running_services, &active, running_runners.as_deref()) {
        match docker_rm(&["rm", "-f", "-v", &name]).await {
            Ok(removed) => c += removed as usize,
            Err(e) => warn!("sweep: service container {name}: {e}"),
        }
    }
    // Whether a shared server exists, not whether services are configured: one
    // left from a run that had them still holds endpoints on job networks.
    let mut servers = None;
    for name in orphans(networks, &active) {
        // A shared service server still connected to a network it was never
        // detached from holds an endpoint on it, and `network rm` would refuse
        // (ADR-0046 §5). Released first, so the network goes in this pass.
        if servers.is_none() {
            servers = Some(crate::services::any_server().await);
        }
        if servers == Some(true) {
            crate::services::release_network(&name).await;
        }
        // The pull-through mirror holds one too while connected. A no-op with
        // `[mirror]` absent.
        crate::mirror::global().release_network(&name).await;
        match docker_rm(&["network", "rm", &name]).await {
            Ok(removed) => n += removed as usize,
            Err(e) if e.to_string().contains("active endpoints") => {
                debug!("sweep: network {name} still has endpoints; leaving it")
            }
            Err(e) => warn!("sweep: network {name}: {e}"),
        }
    }
    for name in orphans(volumes, &active) {
        match docker_rm(&["volume", "rm", &name]).await {
            Ok(removed) => v += removed as usize,
            Err(e) if e.to_string().contains("in use") => {
                debug!("sweep: volume {name} is in use; leaving it")
            }
            Err(e) => warn!("sweep: volume {name}: {e}"),
        }
    }
    if c + n + v > 0 {
        info!("sweep: reaped {c} container(s), {n} network(s), {v} volume(s)");
    }

    // The anonymous backlog: volumes dind leaked before faber named them. A bulk
    // deletion, not a listing — it gets the teardown budget, not the metadata one.
    match docker_lines(&["volume", "prune", "-f"], TEARDOWN_TIMEOUT).await {
        Ok(lines) => {
            // The figure is the daemon's text, logged as it is: held to the form
            // Docker prints a size in, or not logged at all (#105).
            let reclaimed = lines
                .iter()
                .rev()
                .find_map(|l| l.strip_prefix("Total reclaimed space: "))
                .filter(|space| host::size_figure(space));
            if let Some(space) = reclaimed {
                if space != "0B" {
                    info!("sweep: pruned dangling anonymous volumes, reclaimed {space}");
                }
            }
        }
        Err(e) => warn!("sweep: volume prune: {e}"),
    }
}

/// One reap candidate as the daemon reported it: its name, plus the two
/// ownership marks the decision reads.
///
/// `kind` and `id` are empty for a resource carrying no faber labels — either
/// minted before ADR-0007's marks shipped, or the one live path that still
/// produces an unmarked resource today (a per-job volume whose explicit
/// `volume create` failed and which `docker run -v` then conjured unlabelled;
/// see `docker::run_one`). Those fall back to the name rules in
/// [`Candidate::is_reapable`], which is the whole reason the legacy listings
/// still run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    name: String,
    /// `org.faber.kind` — `runner`, `dind`, `net`, `vol`, `cache`.
    kind: String,
    /// `org.faber.id`, i.e. `<job_id>-<mint_ts>`.
    id: String,
}

/// One row of a candidate listing ([`candidate_format`]), as Docker prints it.
#[derive(Debug, Deserialize)]
struct Listed {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Kind", default)]
    kind: String,
    #[serde(rename = "Id", default)]
    id: String,
}

impl Candidate {
    /// One [`candidate_format`] line, for a test: [`Self::from_row`] of the JSON
    /// object it holds, and `Ok(None)` for a line that is not one.
    #[cfg(test)]
    fn parse(line: &str) -> Result<Option<Self>, &'static str> {
        let Ok(row) = serde_json::from_str::<Listed>(line.trim()) else {
            return Ok(None);
        };
        Self::from_row(row)
    }

    /// The candidate in one listed row: its name and its two marks. A missing
    /// label prints as an empty string rather than a placeholder (Docker's `Label`
    /// accessor is a plain map lookup), so empty marks simply mean an unlabelled
    /// resource.
    ///
    /// `Ok(None)` for a row with no name, and `Err` for one that is not in the form
    /// faber reads (#105). This path deletes, so a row it cannot read is a row it
    /// leaves alone: a name outside Docker's own alphabet — the name goes on
    /// `docker rm`'s command line — or marks faber did not write, such as a faber
    /// kind with an `org.faber.id` that is missing or is not `{job_id}-{ts}`. faber
    /// stamps the two together on everything it creates.
    fn from_row(row: Listed) -> Result<Option<Self>, &'static str> {
        if row.name.is_empty() {
            return Ok(None);
        }
        if !untrusted::object_name(&row.name) {
            return Err("a name is outside Docker's own alphabet");
        }
        if !row.kind.is_empty() && untrusted::claim_id(&row.id).is_none() {
            return Err("a faber-marked resource without a job-and-timestamp id");
        }
        Ok(Some(Self {
            name: row.name,
            kind: row.kind,
            id: row.id,
        }))
    }

    /// The claim this resource belongs to.
    ///
    /// `org.faber.id` is the fact and is read first; the name is consulted only
    /// when the label is absent, which is the case only for an unlabelled resource
    /// ([`Self::from_row`] refuses a marked one without a readable id). The two cannot
    /// disagree — both are built from the same `Names::id` — but this path deletes
    /// things, and falling back can only ever *add* an id to check against the
    /// in-flight set, i.e. shield more.
    fn job_id(&self) -> Option<u64> {
        untrusted::claim_id(&self.id)
            .map(|(job, _)| job)
            .or_else(|| job_id(&self.name))
    }

    /// Is this resource faber's to reap, given what is in flight?
    fn is_reapable(&self, active: &HashSet<u64>) -> bool {
        if !self.kind.is_empty() {
            // Labelled: ownership is a fact, so the name is not consulted at all
            // and no foreign-name shielding is needed. The kind allowlist is what
            // keeps the persistent build cache (`kind=cache`) — and any kind added
            // later — out of the janitor's reach; it is an explicit exclusion
            // here, not an omission from a list of name prefixes.
            if !REAPABLE_KINDS.contains(&self.kind.as_str()) {
                return false;
            }
            // Its id is readable: [`Self::from_row`] refused one that is not.
            return match self.job_id() {
                Some(id) => !active.contains(&id),
                None => false,
            };
        }
        // Unlabelled: the pre-ADR-0007 rules, unchanged. A name that parses to a
        // job id is reaped iff that job is not in flight, so a legacy bare
        // `faber-<digits>` stays reapable; a name that does not parse is reaped
        // only when it carries a real faber kind prefix (a genuine stray like
        // `faber-vol-weird`), which is what shields a foreign `faber-web-1`.
        match job_id(&self.name) {
            Some(id) => !active.contains(&id),
            None => has_kind_prefix(&self.name),
        }
    }

    /// Is this a dind, by the strongest evidence it carries?
    fn is_dind(&self) -> bool {
        if self.kind.is_empty() {
            // Unlabelled: the name prefix, which is also what shields a foreign
            // `myproj-faber-dind-1` that the legacy substring filter listed.
            self.name.starts_with("faber-dind-")
        } else {
            self.kind == KIND_DIND
        }
    }
}

/// Both listings for one resource kind, unioned by name.
///
/// `labelled` is the authoritative one: `org.faber.owned` / `org.faber.kind` is a
/// fact the daemon stores, it cannot be produced by accident, and it is what
/// AGENTS.md means by "read labels, never names".
///
/// `legacy` is the name-anchored listing this replaces, kept as a *fallback* and
/// contributing only resources the label listing did not already return. It is
/// not merely back-compat for a host that predates ADR-0007's marks: `run_one`
/// tolerates a failed `docker volume create` and lets the dind mount conjure the
/// per-job volume instead, and that volume carries no labels at all. Drop the
/// fallback and every one of those leaks forever, silently, holding multi-GB of
/// a finished job's image pulls. It goes when that path does.
async fn candidates(labelled: &[String], legacy: &[String]) -> Vec<Candidate> {
    let mut out = list(labelled).await;
    let seen: HashSet<String> = out.iter().map(|c| c.name.clone()).collect();
    out.extend(
        list(legacy)
            .await
            .into_iter()
            .filter(|c| !seen.contains(&c.name)),
    );
    out
}

/// A failed listing yields no candidates — the pass degrades to a no-op rather
/// than aborting (docker may simply not be up yet at login).
async fn list(args: &[String]) -> Vec<Candidate> {
    match docker_stdout(args, CLI_TIMEOUT, untrusted::OUTPUT_CAP).await {
        Ok(text) => candidates_from(&text),
        Err(e) => {
            warn!("sweep: listing candidates (docker {}): {e}", args.join(" "));
            Vec::new()
        }
    }
}

/// The candidates in a [`candidate_format`] listing. A row that does not parse,
/// or is not in the form faber reads, is no candidate: skipped, with one warning
/// for the listing (#105). Skipped is never reaped.
fn candidates_from(text: &str) -> Vec<Candidate> {
    let rows = untrusted::json_lines::<Listed>(text, "sweep: candidate listing");
    untrusted::accept(rows, "sweep: candidate listing", Candidate::from_row)
}

/// Both listings of the running runners that shield their dinds, read whole or
/// not at all (#105).
///
/// [`candidates`] degrades what it cannot read to no candidates, which is right
/// for a listing that feeds deletions: skipped is never reaped. This one only ever
/// shields, and there the same degradation kills a live dind. A runner no claim
/// accounts for, such as one minted by an older faber, has nothing but this
/// listing between its dind and [`running_dind_orphans`]. So `None` when either
/// listing fails or any row in it is not a candidate, and [`dinds_to_reap`] then
/// reaps no running dind at all.
async fn shield(labelled: &[String], legacy: &[String]) -> Option<Vec<Candidate>> {
    let mut out = list_whole(labelled).await?;
    out.extend(list_whole(legacy).await?);
    Some(out)
}

/// One of [`shield`]'s listings: `None` when Docker does not answer, or when a
/// row is not a candidate ([`every_candidate`]).
async fn list_whole(args: &[String]) -> Option<Vec<Candidate>> {
    match docker_stdout(args, CLI_TIMEOUT, untrusted::OUTPUT_CAP).await {
        Ok(text) => every_candidate(&text),
        Err(e) => {
            warn!(
                "sweep: listing running runners (docker {}): {e}",
                args.join(" ")
            );
            None
        }
    }
}

/// Every row of a [`candidate_format`] listing as a candidate, or `None` when any
/// row is not one: it does not parse, has no name, or is not in faber's form.
fn every_candidate(text: &str) -> Option<Vec<Candidate>> {
    let rows = untrusted::json_lines_all::<Listed>(text, "sweep: running-runner listing")?;
    untrusted::accept_all(
        rows,
        "sweep: running-runner listing",
        |row| match Candidate::from_row(row)? {
            Some(c) => Ok(Some(c)),
            None => Err("a row with no name"),
        },
    )
}

/// [`running_dind_orphans`], behind a shield that may not have been read: with
/// none (`None`, see [`shield`]) no running dind is reaped this pass. A dind left
/// a pass too long costs one sweep interval, and one killed under a live runner
/// costs a job.
fn dinds_to_reap(
    dind: Vec<Candidate>,
    active: &HashSet<u64>,
    running_runners: Option<&[Candidate]>,
) -> Vec<String> {
    match running_runners {
        Some(runners) => running_dind_orphans(dind, active, runners),
        None => {
            if !dind.is_empty() {
                warn!(
                    "sweep: the running runners could not be read whole — reaping no running \
                     dind this pass"
                );
            }
            Vec::new()
        }
    }
}

/// [`running_service_orphans`], behind the same shield as [`dinds_to_reap`]: with
/// the running runners unread, no running service container is reaped this pass.
fn services_to_reap(
    services: Vec<Candidate>,
    active: &HashSet<u64>,
    running_runners: Option<&[Candidate]>,
) -> Vec<String> {
    match running_runners {
        Some(runners) => running_service_orphans(services, active, runners),
        None => {
            if !services.is_empty() {
                warn!(
                    "sweep: the running runners could not be read whole — reaping no running \
                     service container this pass"
                );
            }
            Vec::new()
        }
    }
}

/// The `org.faber.kind` values the janitor may reap — faber's **per-job** kinds,
/// the ones that die with their claim. Also, as `faber-<kind>-…`, the name
/// prefixes the legacy fallback listings anchor to, so [`job_id`] and
/// [`has_kind_prefix`] can never disagree with the label allowlist about what
/// counts as a reapable faber resource.
///
/// [`crate::docker::KIND_CACHE`] is deliberately absent: the persistent build
/// cache (`faber-cache-<repo>`, see [`crate::docker::cache_volume_name`]) is
/// dangling between jobs and would otherwise be reaped on the very next sweep.
/// Anything added here must be a resource that dies with its job.
///
/// [`KIND_REGISTRY_AUTH`] is here because it is per-job: the volume that held a
/// runner's registry credential. None is made while `[registry_auth]` is held,
/// but one an earlier release or a crash left behind is reaped like the dind
/// volume, once its claim is no longer in flight.
///
/// [`KIND_HOOK`] is per-job too: the read-only volume holding a runner's
/// job-started hook (ADR-0059 §1), and the container that filled it. Teardown
/// removes both; one a crash left is reaped once its claim is out of flight.
///
/// [`KIND_SERVICE`] is a per-job service container (ADR-0046 §3), torn down with
/// its runner. [`crate::services::KIND_SERVICE_SERVER`] is deliberately absent:
/// the shared server, its volume and its home network carry no job id, outlive
/// every job, and are retired by idle stop, never by the janitor (§7).
const REAPABLE_KINDS: [&str; 7] = [
    KIND_RUNNER,
    KIND_DIND,
    KIND_NET,
    KIND_VOL,
    KIND_REGISTRY_AUTH,
    KIND_HOOK,
    KIND_SERVICE,
];

/// The `--format` field printing one label as a JSON string, e.g.
/// `{{json (.Label "org.faber.kind")}}`. Supported by the container, network and
/// volume formatters alike.
fn label_field(key: &str) -> String {
    format!(r#"{{{{json (.Label "{key}")}}}}"#)
}

/// The `--format` every candidate listing uses: one JSON object per resource, with
/// its name — `.Names` for a container, `.Name` for a network or volume — and the
/// two marks [`Candidate`] reads. JSON, every value escaped by Docker's own `json`
/// function, so no value can forge a field or a row (#105): tab-separated, a mark
/// carrying a tab or a newline could.
fn candidate_format(name_field: &str) -> String {
    format!(
        r#"{{"Name":{{{{json {name_field}}}}},"Kind":{},"Id":{}}}"#,
        label_field(LABEL_KIND),
        label_field(LABEL_ID)
    )
}

/// The `docker ps` argv for listing reap-candidate containers: faber's own, by
/// the ownership label, in the non-running states only.
///
/// One filter on `org.faber.owned` rather than two on `org.faber.kind`, because
/// repeated `label=` filters are **AND**ed by the daemon (unlike `name=`, which
/// is OR'd) — `--filter label=…kind=runner --filter label=…kind=dind` matches
/// nothing at all. The two container kinds are told apart from the `kind` column
/// instead, in [`Candidate::is_reapable`], which also keeps any future faber
/// container kind out of the janitor's reach until it is listed as reapable.
///
/// Status is limited to the non-running states: a running container may be a
/// prior process's runner legitimately finishing its job (see the module doc).
fn container_ps_args() -> Vec<String> {
    let mut args = owned_filter();
    args.extend(non_running_statuses());
    args.extend(["--format".into(), candidate_format(".Names")]);
    args
}

/// The pre-label container listing, kept as the fallback [`candidates`]
/// describes. Anchored to the two container kinds faber actually creates via
/// repeated `name` filters — Docker OR's them — so a foreign container whose
/// name merely *contains* `faber-`, e.g. a `docker compose` service
/// `faber-web-1`, is at least not listed. Carries the same `--format`, so a
/// resource that turns out to be labelled after all is judged by its labels.
fn legacy_container_ps_args() -> Vec<String> {
    let mut args = vec![
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("name=faber-{KIND_RUNNER}-"),
        "--filter".into(),
        format!("name=faber-{KIND_DIND}-"),
    ];
    args.extend(non_running_statuses());
    args.extend(["--format".into(), candidate_format(".Names")]);
    args
}

/// `docker ps -a --filter label=org.faber.owned=true` — the ownership fact, and
/// the whole safety property of every listing built on it.
fn owned_filter() -> Vec<String> {
    vec![
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("label={LABEL_OWNED}=true"),
    ]
}

/// The states a container may be reaped in. Never `running`: see the module doc.
fn non_running_statuses() -> Vec<String> {
    ["exited", "created", "dead"]
        .into_iter()
        .flat_map(|s| ["--filter".to_string(), format!("status={s}")])
        .collect()
}

/// The `docker network ls` argv for reap-candidate networks.
fn network_ls_args() -> Vec<String> {
    vec![
        "network".into(),
        "ls".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_NET}"),
        "--format".into(),
        candidate_format(".Name"),
    ]
}

/// The pre-label network listing — the fallback [`candidates`] describes.
fn legacy_network_ls_args() -> Vec<String> {
    vec![
        "network".into(),
        "ls".into(),
        "--filter".into(),
        format!("name=faber-{KIND_NET}-"),
        "--format".into(),
        candidate_format(".Name"),
    ]
}

/// The `docker volume ls` argv for reap-candidate volumes.
///
/// Filtered on `org.faber.kind=vol`, which the persistent per-repo build cache
/// does not carry (it is `cache`) — so the volume that must outlive every job is
/// now excluded from the listing outright, not merely rejected afterwards.
///
/// Deliberately **not** filtered `dangling=true`. Candidates are listed before
/// the container reaps (the ordering that makes live jobs untouchable), so a
/// dangling-only snapshot excludes precisely the volumes this pass is about to
/// free — every volume pinned by a leaked dind, or by a container the daemon was
/// still deleting, sat out a whole extra interval holding multi-GB of that job's
/// image pulls. That is the opposite of a backstop.
///
/// Listing all of them is safe: [`orphans`] shields in-flight jobs, and for
/// anything else the daemon's own `in use` refusal is the authority (handled as a
/// skip, not an error, in [`sweep`]).
fn volume_ls_args() -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_VOL}"),
        "--format".into(),
        candidate_format(".Name"),
    ]
}

/// The `docker volume ls` argv for leftover registry-credential volumes, by their
/// kind label. Judged like every labelled candidate: reaped once their
/// `org.faber.id` is out of flight, and the daemon's `in use` refusal shields one
/// a runner still mounts.
fn credential_volume_ls_args() -> Vec<String> {
    kind_volume_ls_args(KIND_REGISTRY_AUTH)
}

/// The `docker volume ls` argv for one per-job volume `kind`, by its label.
fn kind_volume_ls_args(kind: &str) -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={kind}"),
        "--format".into(),
        candidate_format(".Name"),
    ]
}

/// The pre-label volume listing — the fallback [`candidates`] describes, and the
/// one that matters most: an unlabelled per-job volume is a resource faber still
/// mints today when `volume create` fails (see [`candidates`]).
///
/// `name=faber-vol-` is a *substring* filter, so it also returns the contrived
/// `faber-cache-faber-vol-x`. That is shielded downstream, by its `cache` label
/// and — for a pre-label one — by `cache-` not being a reapable kind prefix.
fn legacy_volume_ls_args() -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--filter".into(),
        format!("name=faber-{KIND_VOL}-"),
        "--format".into(),
        candidate_format(".Name"),
    ]
}

/// The `docker ps` argv for listing *running* dind containers, frozen ones
/// included (ADR-0032 §2). A dind's `dockerd` runs until killed, so — unlike a
/// runner, which exits when its job ends — a dind is never caught by the
/// non-running [`container_ps_args`]; one whose runner has gone leaks until
/// reaped via [`running_dind_orphans`].
///
/// One kind, so this one *can* filter on `org.faber.kind` directly.
fn running_dind_ps_args() -> Vec<String> {
    running_kind_ps_args(KIND_DIND)
}

/// The `docker ps` argv for listing *running* runner containers — the set whose
/// paired dind must be kept alive. A dind is reaped only when no running runner
/// shares its claim (see [`running_dind_orphans`]), and a frozen runner is a
/// running one (ADR-0032 §2): left out, it would no longer shield its dind.
fn running_runner_ps_args() -> Vec<String> {
    running_kind_ps_args(KIND_RUNNER)
}

/// The `docker ps` argv for listing *running* per-job service containers
/// (ADR-0046), frozen ones included, for [`running_service_orphans`].
fn running_service_ps_args() -> Vec<String> {
    running_kind_ps_args(KIND_SERVICE)
}

/// The pre-label running-dind listing — the fallback [`candidates`] describes.
fn legacy_running_dind_ps_args() -> Vec<String> {
    legacy_running_ps_args(KIND_DIND)
}

/// The pre-label running-runner listing. Kept for the same reason the others
/// are, and with an extra one of its own: this listing only ever *shields*, so
/// losing an entry from it would kill a dind out from under a live runner.
fn legacy_running_runner_ps_args() -> Vec<String> {
    legacy_running_ps_args(KIND_RUNNER)
}

/// Running or frozen ([`live_status_filters`]): a container the critical floor
/// paused is a live job, and must be listed as one.
fn running_kind_ps_args(kind: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "ps".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={kind}"),
    ];
    args.extend(live_status_filters());
    args.extend(["--format".into(), candidate_format(".Names")]);
    args
}

fn legacy_running_ps_args(kind: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "ps".into(),
        "--filter".into(),
        format!("name=faber-{kind}-"),
    ];
    args.extend(live_status_filters());
    args.extend(["--format".into(), candidate_format(".Names")]);
    args
}

/// The kind segment of a faber resource *name* — `faber-<kind>-<rest>` — with
/// the separator consumed, or `None` when `rest` carries no reapable kind.
/// Derived from [`REAPABLE_KINDS`] so the name rules and the label allowlist
/// cannot drift apart.
fn strip_kind(rest: &str) -> Option<&str> {
    REAPABLE_KINDS
        .iter()
        .find_map(|kind| rest.strip_prefix(kind)?.strip_prefix('-'))
}

/// The job id embedded in a faber resource name —
/// `faber-{runner,dind,net,vol}-<job_id>-<mint_ts>` (older names had no kind
/// and/or no timestamp). Only for an *unlabelled* resource: everything faber
/// mints today carries `org.faber.id`, which [`Candidate::job_id`] reads first.
fn job_id(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("faber-")?;
    let rest = strip_kind(rest).unwrap_or(rest);
    rest.split('-').next()?.parse().ok()
}

/// Does `name` carry a real faber resource *kind* prefix
/// (`faber-runner-`/`faber-dind-`/`faber-net-`/`faber-vol-`)? This separates a
/// genuine unlabelled faber stray whose id doesn't parse (`faber-vol-weird`)
/// from a foreign name that merely starts with `faber-` (`faber-web-1`).
fn has_kind_prefix(name: &str) -> bool {
    name.strip_prefix("faber-").and_then(strip_kind).is_some()
}

/// Keep only the candidates eligible for reaping, as names to remove.
///
/// A labelled candidate is judged entirely on its marks: faber's, of a per-job
/// kind, and its claim not in flight. Nothing about its name is consulted, so
/// the foreign-name hazard the old rules worked around cannot arise —
/// `myproj-faber-dind-1` and a compose project literally named `faber` are both
/// simply absent from a label listing.
///
/// KNOWN RESIDUAL, for the *unlabelled* fallback only: a foreign compose project
/// literally named `faber` with a service `runner`/`dind` produces
/// `faber-runner-1`/`faber-dind-1`, which parses to a small id and would still be
/// reaped if that id isn't active. It goes when the fallback does (see
/// [`candidates`]); until then the rule below still eliminates the realistic
/// collision with ordinary service names (web/db/app/postgres).
fn orphans(candidates: Vec<Candidate>, active: &HashSet<u64>) -> Vec<String> {
    candidates
        .into_iter()
        .filter(|c| c.is_reapable(active))
        .map(|c| c.name)
        .collect()
}

/// Keep only the *running* dind containers safe to reap. Unlike a runner, a
/// dind never exits on its own, so this is the only path that can reclaim one
/// leaked by a crash / forced shutdown. A dind is reaped iff **both**:
/// 1. its job id is not in flight — shielding the provisioning window, where
///    faber creates the dind before its runner (and before the job is live), and
///    every actively-served job; and
/// 2. no *running* runner shares its claim — so a dind still backing a live
///    runner is never killed, even if faber's in-flight view is momentarily
///    stale (e.g. just after a restart re-adopts runners).
///
/// The pairing is on `org.faber.id`, which is the claim itself; for a pair that
/// predates the marks it falls back to the exact `<job_id>-<mint_ts>` name
/// suffix, as before. Either match shields, because this is the one reap that
/// kills a *running* container and an over-shielded dind costs one sweep
/// interval where an under-shielded one costs a job.
///
/// A candidate that is not a dind at all — a foreign container the legacy
/// substring filter happened to list, e.g. `myproj-faber-dind-1` — is left
/// alone, mirroring [`orphans`]'s foreign-name shielding.
fn running_dind_orphans(
    dind: Vec<Candidate>,
    active: &HashSet<u64>,
    running_runners: &[Candidate],
) -> Vec<String> {
    let runner_ids: HashSet<&str> = running_runners
        .iter()
        .filter(|r| r.kind == KIND_RUNNER && !r.id.is_empty())
        .map(|r| r.id.as_str())
        .collect();
    let runner_names: HashSet<&str> = running_runners.iter().map(|r| r.name.as_str()).collect();
    dind.into_iter()
        .filter(|c| {
            if !c.is_dind() {
                return false;
            }
            if let Some(id) = c.job_id() {
                if active.contains(&id) {
                    return false;
                }
            }
            if !c.id.is_empty() && runner_ids.contains(c.id.as_str()) {
                return false;
            }
            match c.name.strip_prefix("faber-dind-") {
                Some(suffix) => !runner_names.contains(format!("faber-runner-{suffix}").as_str()),
                None => true,
            }
        })
        .map(|c| c.name)
        .collect()
}

/// Keep only the *running* per-job service containers safe to reap (ADR-0046
/// §10): a Redis or a Mailpit whose job is not in flight and whose runner is no
/// longer running. The rules of [`running_dind_orphans`], on labels alone: a
/// service container is labelled from its first moment, so it is paired with
/// its runner on `org.faber.id` and nothing else. One without that id, or of any
/// other kind (a shared server above all), is never reaped here.
fn running_service_orphans(
    services: Vec<Candidate>,
    active: &HashSet<u64>,
    running_runners: &[Candidate],
) -> Vec<String> {
    let runner_ids: HashSet<&str> = running_runners
        .iter()
        .filter(|r| r.kind == KIND_RUNNER && !r.id.is_empty())
        .map(|r| r.id.as_str())
        .collect();
    services
        .into_iter()
        .filter(|c| {
            c.kind == KIND_SERVICE
                && c.job_id().is_some_and(|id| !active.contains(&id))
                && !runner_ids.contains(c.id.as_str())
        })
        .map(|c| c.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::janitor_tick;
    use std::time::Duration;

    #[test]
    fn the_disk_check_ticks_faster_than_the_sweep() {
        // The default: sweep on the operator's 900s, disk every 120s. Before this
        // the disk check ran once per sweep pass *after* the reap, so its real
        // period was 900s plus however long reaping took — a few minutes more,
        // observed while free space was falling fast.
        let tick = janitor_tick(Duration::from_secs(900));
        assert_eq!(tick, Duration::from_secs(120));
        assert!(tick < Duration::from_secs(900));
    }

    #[test]
    fn a_faster_sweep_cadence_is_never_stretched() {
        // An operator who asks for a 60s sweep must get one. Ticking on the disk
        // interval regardless would silently double it, which is the kind of
        // config that reads as honoured and isn't.
        assert_eq!(
            janitor_tick(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }

    /// The guard exists so a reclamation can never do to CI what a bulk cleanup
    /// run against a live pool already did once: a few jobs failed with
    /// `network faber-net-… not found`, the per-job network deleted in the
    /// seconds between faber creating it and starting dind.
    #[tokio::test]
    async fn reclamation_holds_admission_shut_and_lifts_only_its_own_pause() {
        let (orch, _tmp) = crate::reconcile::tests::test_orch();
        assert!(!orch.is_paused());
        {
            let _g = PauseForReclaim::take(&orch);
            assert!(orch.is_paused(), "reclamation must hold admission shut");
        }
        assert!(!orch.is_paused(), "and lift it again on drop");

        // An operator-set pause is someone's deliberate decision; resuming it
        // would silently overrule them.
        orch.pause(Pause::Operator);
        {
            let _g = PauseForReclaim::take(&orch);
            assert!(orch.is_paused());
        }
        assert!(
            orch.is_paused(),
            "faber must not resume a pause it did not set"
        );

        // Nor one the operator sets while the reclamation runs: theirs replaces
        // the janitor's, so there is nothing of the janitor's left to lift.
        assert!(orch.resume(Pause::Operator));
        {
            let _g = PauseForReclaim::take(&orch);
            orch.pause(Pause::Operator);
        }
        assert_eq!(
            orch.limiter().current_pause(),
            Some(Pause::Operator),
            "an operator's pause taken mid-reclamation must outlive it"
        );
    }

    /// ADR-0007 always said a reclamation shuts admission. Until ADR-0023 it only
    /// stopped claiming: a claim already parked could start its runner in the
    /// middle of the delete.
    #[tokio::test]
    async fn a_claim_parked_through_a_reclamation_waits_it_out() {
        let (orch, _tmp) = crate::reconcile::tests::test_orch();
        let lim = orch.limiter().clone();
        lim.set_vm_total_for_test(1 << 20); // room to spare: only the pause refuses
        let guard = PauseForReclaim::take(&orch);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let admission = tokio::spawn({
            let (lim, seen) = (lim.clone(), seen.clone());
            async move {
                lim.admit(100.into(), move |w| seen.lock().unwrap().push(w.view()))
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while seen.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the parked claim says why it waits");
        assert_eq!(
            seen.lock().unwrap()[0],
            crate::limiter::WaitView::new("paused", "paused while the janitor reclaims disk")
        );
        assert!(!admission.is_finished(), "nothing starts mid-reclamation");

        drop(guard);
        let admitted = tokio::time::timeout(Duration::from_millis(500), admission)
            .await
            .expect("admitted as soon as the reclamation ended")
            .expect("admission task");
        assert_eq!(lim.snapshot().docker_inflight, 1);
        drop(admitted);
    }

    use super::*;

    // --- helpers -----------------------------------------------------------

    /// A candidate carrying faber's marks — what every listing returns for a
    /// resource minted since ADR-0007.
    fn marked(name: &str, kind: &str, id: &str) -> Candidate {
        Candidate {
            name: name.into(),
            kind: kind.into(),
            id: id.into(),
        }
    }

    /// A candidate carrying no labels at all — a resource from before the marks
    /// shipped, or a per-job volume `docker run -v` conjured after an explicit
    /// `volume create` failed. Only the legacy fallback listings turn these up.
    fn unmarked(name: &str) -> Candidate {
        Candidate {
            name: name.into(),
            kind: String::new(),
            id: String::new(),
        }
    }

    fn has(args: &[String], needle: &str) -> bool {
        args.iter().any(|a| a == needle)
    }

    /// Every candidate listing, label-filtered and legacy alike.
    fn all_listings() -> Vec<(&'static str, Vec<String>)> {
        vec![
            ("container", container_ps_args()),
            ("network", network_ls_args()),
            ("volume", volume_ls_args()),
            ("running dind", running_dind_ps_args()),
            ("running runner", running_runner_ps_args()),
            ("legacy container", legacy_container_ps_args()),
            ("legacy network", legacy_network_ls_args()),
            ("legacy volume", legacy_volume_ls_args()),
            ("legacy running dind", legacy_running_dind_ps_args()),
            ("legacy running runner", legacy_running_runner_ps_args()),
        ]
    }

    // --- reading the marks -------------------------------------------------

    /// One candidate-listing row, as Docker's `json` function prints it.
    fn row(name: &str, kind: &str, id: &str) -> String {
        serde_json::json!({"Name": name, "Kind": kind, "Id": id}).to_string()
    }

    #[test]
    fn candidate_parses_the_listing_format() {
        assert_eq!(
            Candidate::parse(&row("faber-net-42-99", "net", "42-99")),
            Ok(Some(marked("faber-net-42-99", "net", "42-99")))
        );
        // A resource with no labels prints empty strings, not a placeholder —
        // Docker's `Label` accessor is a plain map lookup.
        assert_eq!(
            Candidate::parse(&row("faber-vol-42-99", "", "")),
            Ok(Some(unmarked("faber-vol-42-99")))
        );
        assert_eq!(Candidate::parse(""), Ok(None));
        // The tab-separated form this replaced is not a row at all.
        assert_eq!(Candidate::parse("faber-net-42-99\tnet\t42-99"), Ok(None));
    }

    /// #105: this path deletes, so a row it cannot read is a row it leaves alone.
    /// A name that is not Docker's goes on `docker rm`'s command line nowhere, and
    /// faber's marks in a form faber never writes are not taken as faber's.
    #[test]
    fn a_row_not_in_fabers_form_is_never_a_candidate() {
        let text = [
            row("faber-net-42-99", "net", "42-99"),
            row("-rf", "", ""),
            row("faber-vol-1-2;rm -rf /", "", ""),
            row("x/../../etc", "vol", "1-2"),
            row("faber-vol-weird", "vol", ""),
            row("faber-vol-7-8", "vol", "junk"),
            row("faber-vol-7-8", "vol", "7-8\n{\"Name\":\"forged\"}"),
            row(&"a".repeat(300), "", ""),
            "not json".to_string(),
        ]
        .join("\n");
        assert_eq!(
            candidates_from(&text),
            vec![marked("faber-net-42-99", "net", "42-99")]
        );
    }

    #[test]
    fn the_job_id_comes_from_the_label_not_the_name() {
        // The point of the change: nothing about the name is parsed when the
        // resource says which claim it belongs to.
        let c = marked(
            "a-name-that-parses-to-nothing",
            KIND_VOL,
            "1001-1700000000000",
        );
        assert_eq!(c.job_id(), Some(1001));
    }

    #[test]
    fn an_unlabelled_candidate_still_falls_back_to_its_name() {
        assert_eq!(unmarked("faber-vol-42-99").job_id(), Some(42));
        // And so does a labelled one whose id is unreadable: the fallback can
        // only ever add an id to check against the in-flight set, i.e. shield
        // more, which is the right direction on a delete path.
        assert_eq!(marked("faber-vol-42-99", KIND_VOL, "").job_id(), Some(42));
        assert_eq!(
            marked("faber-vol-42-99", KIND_VOL, "junk").job_id(),
            Some(42)
        );
    }

    #[test]
    fn job_id_parses_every_resource_kind() {
        for name in [
            "faber-runner-1001-1700000000000",
            "faber-dind-1001-1700000000000",
            "faber-net-1001-1700000000000",
            "faber-vol-1001-1700000000000",
        ] {
            assert_eq!(job_id(name), Some(1001), "{name}");
        }
    }

    #[test]
    fn job_id_parses_names_that_carry_an_instance() {
        // #94: the minting instance rides at the end, after the two fields every
        // parser reads.
        for name in [
            "faber-runner-1001-1700000000000-builder",
            "faber-dind-1001-1700000000000-h1a2b3c4",
        ] {
            assert_eq!(job_id(name), Some(1001), "{name}");
        }
        assert_eq!(
            marked("x", KIND_VOL, "1001-1700000000000-builder").job_id(),
            Some(1001)
        );
    }

    #[test]
    fn job_id_parses_legacy_names() {
        // Before the per-attempt timestamp suffix…
        assert_eq!(job_id("faber-dind-1001"), Some(1001));
        // …and the bare `provision` CLI name.
        assert_eq!(job_id("faber-51423"), Some(51423));
    }

    #[test]
    fn job_id_rejects_foreign_names() {
        assert_eq!(job_id("k8s-net-123"), None);
        assert_eq!(job_id("faber-net-abc"), None);
    }

    #[test]
    fn a_kind_prefix_is_a_whole_segment() {
        // `strip_kind` requires the separator, so a longer word starting with a
        // kind is not one. Without that, `faber-network-1` from a foreign
        // compose project would read as a faber `net` resource.
        assert!(has_kind_prefix("faber-net-1"));
        assert!(!has_kind_prefix("faber-network-1"));
        assert!(!has_kind_prefix("faber-volume-1"));
        assert_eq!(job_id("faber-network-1"), None);
    }

    // --- what may be reaped ------------------------------------------------

    #[test]
    fn reapable_kinds_are_the_per_job_kinds_only() {
        // The exclusion that keeps the persistent build cache alive, asserted
        // against `docker`'s own constants so the two cannot drift.
        assert!(!REAPABLE_KINDS.contains(&crate::docker::KIND_CACHE));
        assert!(!REAPABLE_KINDS.contains(&crate::docker::KIND_IMAGE));
        // ADR-0035: the git mirror outlives every job, and a refresh container
        // must never be killed mid-fetch by a sweep.
        assert!(!REAPABLE_KINDS.contains(&crate::docker::KIND_GIT_MIRROR));
        assert!(!REAPABLE_KINDS.contains(&crate::docker::KIND_GIT_MIRROR_WORK));
        // A refresh's credential carries no claim; the git mirror's own janitor
        // reaps it under the repo's writer lock.
        assert!(!REAPABLE_KINDS.contains(&crate::docker::KIND_GIT_AUTH));
        // The pull-through mirror's container and cache volume outlive every job.
        assert!(!REAPABLE_KINDS.contains(&crate::docker::KIND_MIRROR));
        // Nor its GitHub credential, which lives as long as the container.
        assert!(!REAPABLE_KINDS.contains(&crate::docker::KIND_MIRROR_AUTH));
        for kind in [
            KIND_RUNNER,
            KIND_DIND,
            KIND_NET,
            KIND_VOL,
            KIND_REGISTRY_AUTH,
            KIND_HOOK,
        ] {
            assert!(REAPABLE_KINDS.contains(&kind), "{kind}");
        }
    }

    #[test]
    fn orphans_shields_inflight_jobs() {
        let active: HashSet<u64> = [111, 222].into_iter().collect();
        let listed = vec![
            marked("faber-dind-111-1", KIND_DIND, "111-1"), // in flight — shielded
            marked("faber-net-333-1", KIND_NET, "333-1"),   // orphan     — reaped
            unmarked("faber-vol-weird"),                    // pre-label stray — reaped
            // faber's kind without the id faber always stamps beside it is not in
            // faber's form (#105): never a listed candidate, and not reaped even
            // if it got this far.
            marked("faber-vol-odd", KIND_VOL, ""),
        ];
        assert_eq!(
            orphans(listed, &active),
            vec!["faber-net-333-1".to_string(), "faber-vol-weird".to_string()]
        );
    }

    #[test]
    fn a_leftover_registry_credential_volume_is_reaped_and_a_live_one_kept() {
        // No release makes this volume while `[registry_auth]` is held; one an
        // earlier release or a crash left behind has only the janitor. It is listed by its kind label
        // and judged on its claim, like the dind volume.
        let args = credential_volume_ls_args();
        assert!(args.contains(&format!("label={LABEL_KIND}={KIND_REGISTRY_AUTH}")));
        let active: HashSet<u64> = [111].into_iter().collect();
        let listed = vec![
            marked("faber-registry-auth-111-1", KIND_REGISTRY_AUTH, "111-1"), // live
            marked("faber-registry-auth-333-1", KIND_REGISTRY_AUTH, "333-1"), // owner gone
            // Judged on the label, not the name.
            marked("renamed-by-hand", KIND_REGISTRY_AUTH, "444-1"),
        ];
        assert_eq!(
            orphans(listed, &active),
            vec![
                "faber-registry-auth-333-1".to_string(),
                "renamed-by-hand".to_string()
            ]
        );
        // A listed row comes back as the candidate it marks.
        let row = r#"{"Name":"faber-registry-auth-333-1","Kind":"registry-auth","Id":"333-1"}"#;
        assert_eq!(
            Candidate::parse(row),
            Ok(Some(marked(
                "faber-registry-auth-333-1",
                KIND_REGISTRY_AUTH,
                "333-1"
            )))
        );
    }

    #[test]
    fn orphans_judges_a_labelled_resource_on_its_labels_alone() {
        // The property the label filters buy. A resource faber owns is reaped
        // whatever it is called, and a name that would have shielded it under the
        // old prefix rules no longer gets a say.
        let active: HashSet<u64> = [111].into_iter().collect();
        let listed = vec![
            marked("renamed-by-hand", KIND_NET, "333-1"), // faber's -> reaped
            marked("renamed-by-hand-too", KIND_VOL, "111-1"), // in flight -> shielded
        ];
        assert_eq!(
            orphans(listed, &active),
            vec!["renamed-by-hand".to_string()]
        );
    }

    #[test]
    fn orphans_shields_the_persistent_cache_volume() {
        // `faber-cache-<repo>` (see `docker::cache_volume_name`) must outlive every
        // job, and between jobs it is dangling — exactly the state the volume
        // listing hunts for. Reaping it silently throws the cache away on every
        // sweep and the only symptom is a workflow that is mysteriously never warm.
        //
        // It now survives on the label: `cache` is not a reapable kind, so it is
        // excluded from the `kind=vol` listing outright *and* rejected if the
        // legacy substring listing turns it up anyway. Before, it survived only
        // because `cache-` happened not to be a name prefix the sweep knew.
        let active = HashSet::new(); // nothing in flight — maximally reapable
        let listed = vec![
            marked("faber-cache-owner-demo", crate::docker::KIND_CACHE, "42-99"),
            // A pre-label cache volume: no marks, so the name rules must still
            // shield it. These exist on real hosts.
            unmarked("faber-cache-owner-demo"),
            // The contrived worst case: a repo whose slug reintroduces the `vol-`
            // token, so even the `name=faber-vol-` substring filter would list it.
            marked(
                "faber-cache-faber-vol-x",
                crate::docker::KIND_CACHE,
                "42-99",
            ),
            unmarked("faber-cache-faber-vol-x"),
            marked("faber-vol-42-99", KIND_VOL, "42-99"), // a real per-job volume -> reap
        ];
        assert_eq!(
            orphans(listed, &active),
            vec!["faber-vol-42-99".to_string()],
            "the persistent cache volume must never be reaped"
        );
    }

    #[test]
    fn orphans_shields_the_git_mirror_volume() {
        // ADR-0035 §1: the mirror is not a per-job kind, so the sweep never reaps
        // it — only `git_mirror`'s own janitor and pressure passes delete one.
        // Even a repo slug that reintroduces a per-job prefix stays shielded.
        let active = HashSet::new();
        let listed = vec![
            marked(
                "faber-git-owner-demo",
                crate::docker::KIND_GIT_MIRROR,
                "mirror",
            ),
            marked(
                "faber-git-faber-vol-x",
                crate::docker::KIND_GIT_MIRROR,
                "mirror",
            ),
            marked("faber-vol-42-99", KIND_VOL, "42-99"),
        ];
        assert_eq!(
            orphans(listed, &active),
            vec!["faber-vol-42-99".to_string()]
        );
    }

    #[test]
    fn orphans_shields_the_mirror_container_and_its_cache_volume() {
        // The pull-through mirror (docs/mirrors.md) is faber's, outlives every job
        // and carries no claim. Shielded on its label, and by name too when a
        // listing turns it up unmarked: neither name has a per-job prefix.
        let active = HashSet::new();
        let mirror = crate::docker::KIND_MIRROR;
        let listed = vec![
            marked(crate::mirror::CONTAINER, mirror, ""),
            marked(crate::mirror::VOLUME, mirror, ""),
            marked(
                crate::mirror::AUTH_VOLUME,
                crate::docker::KIND_MIRROR_AUTH,
                "",
            ),
            unmarked(crate::mirror::AUTH_VOLUME),
            // Even one that somehow carried a claim id.
            marked(crate::mirror::VOLUME, mirror, "42-99"),
            unmarked(crate::mirror::CONTAINER),
            unmarked(crate::mirror::VOLUME),
            marked("faber-vol-42-99", KIND_VOL, "42-99"),
        ];
        assert_eq!(
            orphans(listed.clone(), &active),
            vec!["faber-vol-42-99".to_string()]
        );
        // Nor is a running mirror taken for a service whose runner is gone.
        assert!(running_service_orphans(listed, &active, &[])
            .iter()
            .all(|n| n == "faber-vol-42-99"));
    }

    #[test]
    fn orphans_leaves_foreign_faber_prefixed_names() {
        // Names that merely *contain* `faber-` but aren't faber resources — e.g.
        // `docker compose` containers in a faber/ dir, or manually-named ones.
        // Only the legacy fallback listing can turn these up at all (a label
        // filter cannot see them), and it must not reap them.
        let active = HashSet::new(); // nothing in flight
        let listed = vec![
            unmarked("faber-web-1"), // foreign compose service -> shield
            unmarked("faber-db-1"),  // foreign compose service -> shield
            marked("faber-runner-42-99", KIND_RUNNER, "42-99"), // real faber runner -> reap
            unmarked("faber-runner-7-99"), // pre-label faber runner   -> reap
        ];
        assert_eq!(
            orphans(listed, &active),
            vec![
                "faber-runner-42-99".to_string(),
                "faber-runner-7-99".to_string()
            ],
            "foreign faber-* names must be shielded; only real faber resources reaped"
        );
    }

    #[test]
    fn a_volume_freed_by_this_same_pass_is_still_a_candidate() {
        // The listing is taken before the container reaps, so at list time this
        // volume was still mounted by a leaked dind — i.e. not dangling. With no
        // dangling filter it is in the listing, and `orphans` clears it because its
        // job isn't in flight, so the reap that follows the dind reap catches it
        // in the same pass instead of 900s later.
        let active: HashSet<u64> = [777].into_iter().collect();
        let listed = vec![
            marked("faber-vol-555-1", KIND_VOL, "555-1"), // dind just reaped -> reap now
            marked("faber-vol-777-1", KIND_VOL, "777-1"), // live job         -> shielded
        ];
        assert_eq!(
            orphans(listed, &active),
            vec!["faber-vol-555-1".to_string()]
        );
    }

    // --- declared services (ADR-0046) -------------------------------------

    #[test]
    fn an_exited_per_job_service_container_is_reaped_like_any_job_resource() {
        let active: HashSet<u64> = [777].into_iter().collect();
        let listed = vec![
            marked("faber-redis-555-1", KIND_SERVICE, "555-1"), // job gone -> reap
            marked("faber-redis-777-1", KIND_SERVICE, "777-1"), // live job -> shield
        ];
        assert_eq!(
            orphans(listed, &active),
            vec!["faber-redis-555-1".to_string()]
        );
    }

    #[test]
    fn the_shared_service_server_is_never_reaped() {
        // §7: the server, its volume and `faber-svc` are faber's own resources,
        // retired by idle stop. Not by the janitor, even with nothing in flight
        // and even if one carried an id.
        let server = crate::services::KIND_SERVICE_SERVER;
        assert!(!REAPABLE_KINDS.contains(&server));
        let active = HashSet::new();
        let listed = vec![
            marked("faber-svc-postgres-16", server, "1-1"),
            marked("faber-svc", server, "1-1"),
        ];
        assert!(orphans(listed.clone(), &active).is_empty());
        assert!(running_service_orphans(listed, &active, &[]).is_empty());
    }

    #[test]
    fn a_running_service_container_whose_runner_is_gone_is_reaped() {
        let active = HashSet::new();
        let services = vec![marked("faber-redis-555-1", KIND_SERVICE, "555-1")];
        assert_eq!(
            running_service_orphans(services, &active, &[]),
            vec!["faber-redis-555-1".to_string()]
        );
    }

    #[test]
    fn a_running_service_container_is_shielded_by_its_job_and_its_runner() {
        // In flight: the provisioning window, where services start before the
        // runner exists.
        let active: HashSet<u64> = [555].into_iter().collect();
        let services = vec![marked("faber-redis-555-1", KIND_SERVICE, "555-1")];
        assert!(running_service_orphans(services.clone(), &active, &[]).is_empty());
        // Its runner is running, whatever the in-flight view says (just after a
        // restart, say). Paired on `org.faber.id`, never on a name.
        let runners = vec![marked("runner-renamed", KIND_RUNNER, "555-1")];
        assert!(running_service_orphans(services.clone(), &HashSet::new(), &runners).is_empty());
        // A runner of another attempt of the same job does not shield it.
        let other = vec![marked("faber-runner-555-2", KIND_RUNNER, "555-2")];
        assert_eq!(
            running_service_orphans(services, &HashSet::new(), &other),
            vec!["faber-redis-555-1".to_string()]
        );
    }

    #[test]
    fn running_service_orphans_reads_labels_only() {
        // An unlabelled container is never a service, whatever it is called, and
        // a labelled dind or runner is not one either: this pass kills running
        // containers, so it takes only what says it is a per-job service.
        let active = HashSet::new();
        let listed = vec![
            unmarked("faber-redis-555-1"),
            marked("faber-dind-555-1", KIND_DIND, "555-1"),
            marked("faber-runner-555-1", KIND_RUNNER, "555-1"),
        ];
        assert!(running_service_orphans(listed, &active, &[]).is_empty());
    }

    #[test]
    fn no_running_service_container_is_reaped_behind_an_unread_shield() {
        let services = vec![marked("faber-redis-555-1", KIND_SERVICE, "555-1")];
        assert!(services_to_reap(services.clone(), &HashSet::new(), None).is_empty());
        assert_eq!(
            services_to_reap(services, &HashSet::new(), Some(&[])),
            vec!["faber-redis-555-1".to_string()]
        );
    }

    #[test]
    fn the_running_service_listing_filters_on_its_kind_label() {
        let args = running_service_ps_args();
        assert!(args.contains(&format!("label={LABEL_KIND}={KIND_SERVICE}")));
        assert!(!args.iter().any(|a| a.starts_with("name=")));
    }

    // --- the running-dind orphan ------------------------------------------

    #[test]
    fn running_dind_orphans_reaps_dind_whose_runner_is_gone() {
        // The core leak: a dind still running but its paired runner removed, and
        // the job no longer in flight — never caught by the exited-only listing.
        let active = HashSet::new();
        let dind = vec![marked("faber-dind-555-1", KIND_DIND, "555-1")];
        assert_eq!(
            running_dind_orphans(dind, &active, &[]),
            vec!["faber-dind-555-1".to_string()]
        );
    }

    #[test]
    fn running_dind_orphans_shields_inflight_job() {
        // Provisioning window (and any live job): the dind exists before its
        // runner, so an in-flight job id shields it.
        let active: HashSet<u64> = [555].into_iter().collect();
        let dind = vec![marked("faber-dind-555-1", KIND_DIND, "555-1")];
        assert!(running_dind_orphans(dind, &active, &[]).is_empty());
    }

    #[test]
    fn running_dind_orphans_pairs_on_the_claim_label() {
        // The pairing is `org.faber.id`, which is the claim itself — so a dind
        // backing a live runner is kept even if neither is named conventionally,
        // and the job id is not in the (momentarily stale) in-flight set.
        let active = HashSet::new();
        let runners = vec![marked("runner-renamed", KIND_RUNNER, "555-1")];
        let dind = vec![marked("dind-renamed", KIND_DIND, "555-1")];
        assert!(running_dind_orphans(dind, &active, &runners).is_empty());
    }

    #[test]
    fn running_dind_orphans_shields_dind_backing_a_live_runner() {
        // Its exact paired runner is running — keep the dind even though the job
        // id isn't in the in-flight set. Holds for a pre-label pair too, where
        // the exact name suffix is the only evidence there is.
        let active = HashSet::new();
        let runners = vec![unmarked("faber-runner-555-1")];
        let dind = vec![unmarked("faber-dind-555-1")];
        assert!(running_dind_orphans(dind, &active, &runners).is_empty());
    }

    #[test]
    fn running_dind_orphans_reaps_stale_reprovision_attempt() {
        // A re-provision left dind+runner for two attempts (ts 1 and 2). The ts-2
        // runner is live but the ts-1 runner is gone; with the job not in flight,
        // the orphaned ts-1 dind is reaped while the live ts-2 dind is kept —
        // proof the match is per-attempt (the whole `org.faber.id`), not per-job-id.
        let active = HashSet::new();
        let runners = vec![marked("faber-runner-555-2", KIND_RUNNER, "555-2")];
        let dind = vec![
            marked("faber-dind-555-1", KIND_DIND, "555-1"), // runner gone -> reap
            marked("faber-dind-555-2", KIND_DIND, "555-2"), // runner live -> keep
        ];
        assert_eq!(
            running_dind_orphans(dind, &active, &runners),
            vec!["faber-dind-555-1".to_string()]
        );
    }

    #[test]
    fn running_dind_orphans_ignores_foreign_dind_names() {
        // A foreign container the legacy substring `name=faber-dind-` filter
        // happened to list (its name only *contains* the token) must never be
        // reaped. A label listing cannot return it at all.
        let active = HashSet::new();
        let listed = vec![unmarked("myproj-faber-dind-1")];
        assert!(running_dind_orphans(listed, &active, &[]).is_empty());
    }

    #[test]
    fn running_dind_orphans_ignores_a_labelled_non_dind() {
        // Belt and braces: whatever a listing returns, only a dind is killed on
        // this path — the one reap that kills a *running* container.
        let active = HashSet::new();
        let listed = vec![marked("faber-runner-555-1", KIND_RUNNER, "555-1")];
        assert!(running_dind_orphans(listed, &active, &[]).is_empty());
    }

    /// #105: the running runners only ever shield, so they are read whole or not
    /// at all. A runner no claim accounts for has nothing else between its live
    /// dind and this reap, so a listing that failed, or one row faber cannot read,
    /// reaps no running dind that pass.
    #[test]
    fn a_shield_that_cannot_be_read_whole_reaps_no_running_dind() {
        let active = HashSet::new();
        let dind = || vec![marked("faber-dind-555-1", KIND_DIND, "555-1")];
        let live = row("faber-runner-555-1", KIND_RUNNER, "555-1");
        let runners = every_candidate(&live).expect("a readable shield");
        assert!(dinds_to_reap(dind(), &active, Some(runners.as_slice())).is_empty());
        for bad in [
            row("faber-runner-555-1;id", KIND_RUNNER, "555-1"),
            row("faber-runner-556-1", KIND_RUNNER, "junk"),
            row("", KIND_RUNNER, "555-1"),
            "not json".to_string(),
        ] {
            assert_eq!(every_candidate(&format!("{live}\n{bad}")), None, "{bad}");
        }
        assert!(
            dinds_to_reap(dind(), &active, None).is_empty(),
            "no shield read: not even a dind no runner could be shielding"
        );
        assert_eq!(
            dinds_to_reap(dind(), &active, Some(&[][..])),
            vec!["faber-dind-555-1".to_string()],
            "a shield read whole still lets the orphan go"
        );
    }

    // --- the argv the listings actually run --------------------------------

    #[test]
    fn every_listing_asks_for_the_ownership_marks() {
        // The decision reads `org.faber.kind` and `org.faber.id`, so every
        // listing — legacy ones included — has to print them. A legacy listing
        // without them would demote a perfectly well-marked resource to the name
        // rules purely because of which filter happened to find it.
        for (what, args) in all_listings() {
            let fmt = args
                .iter()
                .skip_while(|a| *a != "--format")
                .nth(1)
                .unwrap_or_else(|| panic!("{what} listing has no --format"));
            assert!(
                fmt.contains(r#""Kind":{{json (.Label "org.faber.kind")}}"#),
                "{what}: {fmt}"
            );
            assert!(
                fmt.contains(r#""Id":{{json (.Label "org.faber.id")}}"#),
                "{what}: {fmt}"
            );
            // And the name, as JSON too, so no field can be forged by another.
            assert!(fmt.starts_with(r#"{"Name":{{json .Name"#), "{what}: {fmt}");
        }
    }

    #[test]
    fn no_listing_repeats_a_label_filter() {
        // Docker ANDs repeated `label=` filters (unlike `name=`, which it ORs), so
        // two of them on the same listing match nothing at all — a sweep that
        // silently reaps nothing, forever. Verified against the daemon before this
        // was written; pinned here because the failure is invisible.
        for (what, args) in all_listings() {
            let labels = args.iter().filter(|a| a.starts_with("label=")).count();
            assert!(labels <= 1, "{what} repeats a label filter: {args:?}");
        }
    }

    #[test]
    fn candidate_listings_filter_on_ownership_not_names() {
        // The rule AGENTS.md states for every reclamation path: read labels,
        // never names. These four are the primary listings; the `legacy_*` ones
        // beside them are the transitional fallback, tested separately.
        assert!(has(&container_ps_args(), "label=org.faber.owned=true"));
        assert!(has(&network_ls_args(), "label=org.faber.kind=net"));
        assert!(has(&volume_ls_args(), "label=org.faber.kind=vol"));
        assert!(has(&running_dind_ps_args(), "label=org.faber.kind=dind"));
        assert!(has(
            &running_runner_ps_args(),
            "label=org.faber.kind=runner"
        ));
        for (what, args) in all_listings()
            .into_iter()
            .filter(|(w, _)| !w.starts_with("legacy"))
        {
            assert!(
                !args.iter().any(|a| a.starts_with("name=")),
                "{what} still filters by name: {args:?}"
            );
        }
    }

    #[test]
    fn the_container_listing_covers_both_container_kinds_in_one_filter() {
        // `--filter label=…kind=runner --filter label=…kind=dind` would match
        // nothing (see `no_listing_repeats_a_label_filter`), so the container
        // listing keys on ownership and `Candidate::is_reapable` tells the kinds
        // apart. Both must therefore be reapable kinds, and nothing else faber
        // might one day put in a container may be.
        let args = container_ps_args();
        assert!(has(&args, "label=org.faber.owned=true"));
        assert!(!args.iter().any(|a| a.starts_with("label=org.faber.kind")));
        let active = HashSet::new();
        let listed = vec![
            marked("faber-runner-42-99", KIND_RUNNER, "42-99"),
            marked("faber-dind-42-99", KIND_DIND, "42-99"),
            // A faber-owned container of some future kind: listed by `owned=true`,
            // and not reapable until someone says it is.
            marked("faber-sidecar-42-99", "sidecar", "42-99"),
        ];
        assert_eq!(
            orphans(listed, &active),
            vec![
                "faber-runner-42-99".to_string(),
                "faber-dind-42-99".to_string()
            ]
        );
    }

    #[test]
    fn legacy_container_ps_args_anchor_to_faber_container_kinds() {
        // The fallback keeps the anchoring the label filters made unnecessary:
        // while it runs, it is still the only thing standing between the janitor
        // and a foreign `faber-web-1`.
        let args = legacy_container_ps_args();
        assert!(
            has(&args, "name=faber-runner-"),
            "must list runner containers by anchored prefix"
        );
        assert!(
            has(&args, "name=faber-dind-"),
            "must list dind containers by anchored prefix"
        );
        assert!(
            !has(&args, "name=faber-"),
            "must NOT use the broad substring filter that also lists foreign faber-* containers"
        );
    }

    #[test]
    fn legacy_listings_still_anchor_to_the_faber_prefix() {
        assert!(has(&legacy_network_ls_args(), "name=faber-net-"));
        assert!(has(&legacy_volume_ls_args(), "name=faber-vol-"));
        assert!(has(&legacy_running_dind_ps_args(), "name=faber-dind-"));
        assert!(has(&legacy_running_runner_ps_args(), "name=faber-runner-"));
    }

    #[test]
    fn container_listings_never_include_running_containers() {
        // A running container may be a prior process's runner finishing a real
        // job. The dind exception goes through `running_dind_orphans`, which
        // checks its paired runner first.
        for args in [container_ps_args(), legacy_container_ps_args()] {
            assert!(has(&args, "status=exited"));
            assert!(has(&args, "status=created"));
            assert!(has(&args, "status=dead"));
            assert!(!has(&args, "status=running"), "{args:?}");
        }
    }

    #[test]
    fn running_ps_args_are_running_or_frozen_only() {
        // A container the critical floor paused (ADR-0032 §2) is still a live job:
        // its runner must shield its dind, and a frozen orphan dind must be found.
        for args in [
            running_dind_ps_args(),
            legacy_running_dind_ps_args(),
            running_runner_ps_args(),
            legacy_running_runner_ps_args(),
        ] {
            assert!(has(&args, "status=running"), "running, not exited");
            assert!(has(&args, "status=paused"), "frozen is running: {args:?}");
            assert!(!has(&args, "status=exited"));
        }
        // And a frozen container is never a non-running one to reap outright.
        for args in [container_ps_args(), legacy_container_ps_args()] {
            assert!(!has(&args, "status=paused"), "{args:?}");
        }
    }

    #[test]
    fn volume_ls_args_do_not_filter_dangling() {
        for args in [volume_ls_args(), legacy_volume_ls_args()] {
            assert!(
                !args.iter().any(|a| a.contains("dangling")),
                "a dangling-only snapshot excludes exactly the volumes this pass is about \
                 to free by reaping their container, delaying every one of them a full \
                 sweep interval: {args:?}"
            );
        }
    }

    // --- ADR-0024 §3: a pass that frees nothing is not repeated every tick --

    const SWEEP: Duration = Duration::from_secs(900);

    #[test]
    fn with_no_fruitless_pass_behind_it_a_pass_always_runs() {
        let b = Backoff::default();
        assert!(b.allows(0, Instant::now(), SWEEP));
        assert!(b.allows(10 * 1024, Instant::now(), SWEEP));
    }

    #[test]
    fn after_a_fruitless_pass_nothing_changing_means_no_new_pass() {
        // The incident: free space stuck low (say 10 GiB), held by data that was
        // never CI's, and a pass every tick that freed nothing and paused admission
        // to do it.
        let t0 = Instant::now();
        let mut b = Backoff::default();
        b.fruitless(10 * 1024, t0);
        for later in [
            Duration::ZERO,
            DISK_CHECK_INTERVAL,
            SWEEP - Duration::from_secs(1),
        ] {
            assert!(!b.allows(10 * 1024, t0 + later, SWEEP), "{later:?}");
        }
        // Recovering, but still under the floor: not worse, so no new pass either.
        assert!(!b.allows(20 * 1024, t0 + DISK_CHECK_INTERVAL, SWEEP));
    }

    #[test]
    fn a_disk_one_gib_worse_gets_a_pass_at_once() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        b.fruitless(10 * 1024, t0);
        let soon = t0 + DISK_CHECK_INTERVAL;
        assert!(b.allows(9 * 1024, soon, SWEEP), "exactly 1 GiB below");
        assert!(b.allows(3 * 1024, soon, SWEEP));
        assert!(!b.allows(9 * 1024 + 1, soon, SWEEP), "1 MiB short of it");
    }

    #[test]
    fn a_sweep_interval_later_a_pass_runs_whatever_the_disk_did() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        b.fruitless(10 * 1024, t0);
        assert!(b.allows(10 * 1024, t0 + SWEEP, SWEEP));
        assert!(b.allows(20 * 1024, t0 + SWEEP * 3, SWEEP));
    }

    #[test]
    fn clearing_the_memory_restores_the_immediate_pass() {
        // Cleared by a pass that frees something, or a reading at/above the floor.
        let t0 = Instant::now();
        let mut b = Backoff::default();
        b.fruitless(10 * 1024, t0);
        b.clear();
        assert!(b.allows(10 * 1024, t0, SWEEP));
    }

    #[test]
    fn a_pass_that_raised_free_space_is_freed() {
        // By at least the backoff's step, anywhere under the floor.
        assert_eq!(
            pass_verdict(true, 4 * 1024, 5 * 1024, 25),
            PassVerdict::Freed
        );
        // Or to the floor or above, by however much.
        assert_eq!(
            pass_verdict(true, 24 * 1024, 80 * 1024, 25),
            PassVerdict::Freed
        );
        assert_eq!(
            pass_verdict(true, 25 * 1024 - 1, 25 * 1024, 25),
            PassVerdict::Freed
        );
    }

    #[test]
    fn deleting_what_is_rebuilt_at_once_is_not_freeing_room() {
        // Observed in October 2026: every pass deleted the git mirrors and free
        // space stayed flat, just under a 25 GiB floor. Counted as freed, each one
        // cleared the backoff and the next held admission again two minutes on.
        assert_eq!(
            pass_verdict(true, 25_400, 25_300, 25),
            PassVerdict::Fruitless
        );
        assert_eq!(
            pass_verdict(true, 25_300, 25_400, 25),
            PassVerdict::Fruitless
        );
        // One MiB short of the step is still short.
        assert_eq!(
            pass_verdict(true, 10 * 1024, 11 * 1024 - 1, 25),
            PassVerdict::Fruitless
        );
    }

    #[test]
    fn a_pass_that_freed_nothing_and_left_the_disk_short_is_a_fault() {
        assert_eq!(
            pass_verdict(false, 10 * 1024, 10 * 1024, 25),
            PassVerdict::Fruitless
        );
        assert_eq!(
            pass_verdict(false, 10 * 1024, 25 * 1024 - 1, 25),
            PassVerdict::Fruitless
        );
    }

    #[test]
    fn pressure_that_lifted_during_the_pass_is_not_reported_as_freed_nothing() {
        // Observed: a reading of, say, 75,000 MiB reported as "still below the
        // 25 GiB floor", after space came back by hand in the middle of a pass.
        assert_eq!(
            pass_verdict(false, 20 * 1024, 75_000, 25),
            PassVerdict::PressureLifted
        );
        // At the floor exactly is not below it — the same line the pass began on.
        assert_eq!(
            pass_verdict(false, 20 * 1024, 25 * 1024, 25),
            PassVerdict::PressureLifted
        );
    }

    fn disk(free_mib: u64, volume: host::DiskVolume) -> host::DiskReading {
        host::DiskReading {
            free_mib,
            at_ms: 0,
            volume,
        }
    }

    #[test]
    fn a_short_mac_volume_is_weighed_by_the_vm_disk_the_pass_can_raise() {
        // Observed in October 2026: the Mac volume short for hours, the VM's disk
        // roomier. Deleting inside the VM never returned a byte to the Mac.
        let mac = disk(8 * 1024, host::DiskVolume::MacVms);
        let vm = disk(30 * 1024, host::DiskVolume::LinuxVm);
        assert_eq!(judged_reading(Some(mac), Some(vm)), Some(vm));
    }

    #[test]
    fn without_a_vm_reading_the_tightest_still_binds() {
        // An unreadable VM disk is no evidence it has room.
        let mac = disk(8 * 1024, host::DiskVolume::MacVms);
        assert_eq!(judged_reading(Some(mac), None), Some(mac));
        assert_eq!(judged_reading(None, None), None);
    }

    #[test]
    fn a_tighter_vm_disk_or_a_plain_docker_volume_is_judged_as_read() {
        let vm = disk(20 * 1024, host::DiskVolume::LinuxVm);
        assert_eq!(judged_reading(Some(vm), Some(vm)), Some(vm));
        // A Linux host: one reading, the Docker volume, and no VM disk.
        let docker = disk(5 * 1024, host::DiskVolume::Docker);
        assert_eq!(judged_reading(Some(docker), None), Some(docker));
    }

    // --- ADR-0024 §4: the cache is aged inside, never reaped ------------------

    #[test]
    fn the_cache_volume_is_aged_inside_and_never_reaped() {
        // Ageing entries inside the cache volume is not licence to remove the
        // volume, and the two paths must not meet: the sweep still rejects it by
        // label and by name, and the ageing still finds it by label and by exact
        // name.
        let active = HashSet::new(); // nothing in flight: maximally reapable
        let listed = vec![
            marked("faber-cache-owner-demo", crate::docker::KIND_CACHE, "42-99"),
            unmarked("faber-cache-owner-legacy"),
        ];
        assert!(orphans(listed, &active).is_empty());
        assert!(!volume_ls_args()
            .iter()
            .any(|a| a.contains(crate::docker::KIND_CACHE)));

        let listing = [
            r#"{"Name":"faber-cache-owner-demo","Kind":"cache"}"#,
            r#"{"Name":"faber-cache-owner-legacy","Kind":""}"#,
        ]
        .join("\n");
        let served = vec!["owner/legacy".to_string()];
        assert_eq!(
            cache::target_volumes(&listing, &served),
            vec!["faber-cache-owner-demo", "faber-cache-owner-legacy"]
        );
    }

    #[test]
    fn faber_mode_sizes_the_cache_delete_from_the_reading_after_the_prunes() {
        // Floor 25 GiB + 5 GiB margin = 30720 MiB wanted.
        assert_eq!(cache_needed_mib(25, Some(20_000)), 10_720);
        assert_eq!(cache_needed_mib(25, Some(30_720)), 0, "exactly there");
        assert_eq!(
            cache_needed_mib(25, Some(90_000)),
            0,
            "the prunes were enough"
        );
        assert_eq!(
            cache_needed_mib(25, None),
            0,
            "an unreadable disk deletes nothing"
        );
    }

    #[tokio::test]
    async fn faber_mode_trims_nothing_on_the_sweep_path() {
        // No key set: no limit in force, so the trim returns before any Docker call.
        let limits = cache::Limits::from_config(None, None);
        assert!(!limits.any());
        assert!(!cache::operator_set(None, None));
        assert!(cache::trim(limits, "busybox", &[]).await.is_empty());
    }
}
