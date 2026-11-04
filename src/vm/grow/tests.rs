use super::*;
use std::collections::VecDeque;

const MIN: u64 = 60_000;
const T0: u64 = 1_800_000_000_000;

fn disk(gib: u64, allocated_gib: u64) -> DiskFile {
    DiskFile {
        bytes: gib * GIB,
        allocated_bytes: allocated_gib * GIB,
    }
}

fn inputs() -> Inputs {
    Inputs {
        trigger: Some(Trigger::BelowFloor),
        disk: Some(disk(64, 60)),
        resized_at: Some(64 * GIB),
        fault: false,
        ceiling_bytes: 128 * GIB,
        host_free_bytes: Some(500 * GIB),
        floor_gib: 25,
    }
}

// --- the triggers ----------------------------------------------------------

#[test]
fn below_the_floor_counts_only_after_30_minutes_and_a_fruitless_pass() {
    let mut c = Clocks::default();
    c.vm_free(Some(20 * 1024), 25, T0);
    assert_eq!(c.trigger(T0 + 31 * MIN, false), None, "no janitor pass yet");
    c.janitor(true, T0 + 5 * MIN);
    assert_eq!(
        c.trigger(T0 + 31 * MIN, false),
        None,
        "the last pass freed something"
    );
    c.janitor(false, T0 + 10 * MIN);
    assert_eq!(c.trigger(T0 + 29 * MIN, false), None, "not yet 30 minutes");
    assert_eq!(c.trigger(T0 + 30 * MIN, false), Some(Trigger::BelowFloor));
    // Above the floor again: the clock and the janitor's verdict start over.
    c.vm_free(Some(30 * 1024), 25, T0 + 31 * MIN);
    assert_eq!(c.trigger(T0 + 40 * MIN, false), None);
    // An unread reading leaves the clock alone.
    c.vm_free(Some(20 * 1024), 25, T0 + 41 * MIN);
    c.vm_free(None, 25, T0 + 42 * MIN);
    c.janitor(false, T0 + 50 * MIN);
    assert_eq!(c.trigger(T0 + 72 * MIN, false), Some(Trigger::BelowFloor));
}

#[test]
fn a_pass_from_before_the_episode_does_not_count() {
    let mut c = Clocks::default();
    c.janitor(false, T0 - MIN);
    c.vm_free(Some(1024), 25, T0);
    assert_eq!(c.trigger(T0 + 60 * MIN, false), None);
}

#[test]
fn the_gate_trigger_needs_no_janitor_and_ends_when_it_admits() {
    let mut c = Clocks::default();
    // 28 GiB free, above the janitor's 25 GiB floor: no pass ever runs.
    c.vm_free(Some(28 * 1024), 25, T0);
    c.gate_refused(T0);
    c.gate_refused(T0 + 29 * MIN);
    assert_eq!(c.trigger(T0 + 29 * MIN, false), None);
    c.gate_refused(T0 + 30 * MIN);
    assert_eq!(c.trigger(T0 + 30 * MIN, false), Some(Trigger::GateRefusing));
    // Nothing asked for a while: no longer refusing.
    assert_eq!(c.trigger(T0 + 40 * MIN, false), None);
    c.gate_refused(T0 + 41 * MIN);
    assert_eq!(c.trigger(T0 + 41 * MIN, false), Some(Trigger::GateRefusing));
    c.gate_passed();
    c.gate_refused(T0 + 42 * MIN);
    assert_eq!(
        c.trigger(T0 + 42 * MIN, false),
        None,
        "the clock restarts after an admission"
    );
}

// --- pending and the step --------------------------------------------------

#[test]
fn a_grow_is_a_32_gib_step() {
    assert_eq!(
        pending(&inputs()),
        Pending::Grow {
            trigger: Trigger::BelowFloor,
            from: 64 * GIB,
            to: 96 * GIB
        }
    );
}

#[test]
fn nothing_is_pending_without_a_trigger() {
    let i = Inputs {
        trigger: None,
        ..inputs()
    };
    assert_eq!(pending(&i), Pending::No);
}

#[test]
fn each_condition_blocks_a_grow() {
    let b = |i: Inputs| match pending(&i) {
        Pending::Blocked(_, b) => b,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        b(Inputs {
            disk: None,
            ..inputs()
        }),
        Blocked::SizeUnknown
    );
    assert_eq!(
        b(Inputs {
            fault: true,
            ..inputs()
        }),
        Blocked::Fault
    );
    // No resize report at this length: a failed resize never sets off another
    // restart and another 32 GiB.
    assert_eq!(
        b(Inputs {
            resized_at: None,
            ..inputs()
        }),
        Blocked::NotResized
    );
    assert_eq!(
        b(Inputs {
            resized_at: Some(32 * GIB),
            ..inputs()
        }),
        Blocked::NotResized
    );
    assert_eq!(
        b(Inputs {
            ceiling_bytes: 64 * GIB,
            ..inputs()
        }),
        Blocked::Ceiling,
        "64 turns growth off"
    );
    assert_eq!(
        b(Inputs {
            ceiling_bytes: 48 * GIB,
            ..inputs()
        }),
        Blocked::Ceiling,
        "a ceiling under the size changes nothing on disk"
    );
    assert_eq!(
        b(Inputs {
            host_free_bytes: None,
            ..inputs()
        }),
        Blocked::HostUnread
    );
}

#[test]
fn the_step_never_passes_the_ceiling() {
    let i = Inputs {
        disk: Some(disk(112, 100)),
        resized_at: Some(112 * GIB),
        ..inputs()
    };
    assert!(matches!(pending(&i), Pending::Grow { to, .. } if to == 128 * GIB));
    // A ceiling closer than the smallest step is still reached.
    let i = Inputs {
        disk: Some(disk(124, 100)),
        resized_at: Some(124 * GIB),
        ..inputs()
    };
    assert!(matches!(pending(&i), Pending::Grow { to, .. } if to == 128 * GIB));
}

#[test]
fn the_host_check_counts_the_sparse_debt_and_shrinks_the_step() {
    // 64 GiB file holding 30: the guest may still write 34 GiB the Mac must hold.
    // 100 free − 34 debt − 25 floor = 41 spare: a full step.
    let i = Inputs {
        disk: Some(disk(64, 30)),
        host_free_bytes: Some(100 * GIB),
        ..inputs()
    };
    assert!(matches!(pending(&i), Pending::Grow { to, .. } if to == 96 * GIB));
    // 80 free: 21 spare, so a 21 GiB step.
    let i = Inputs {
        host_free_bytes: Some(80 * GIB),
        ..i
    };
    assert!(matches!(pending(&i), Pending::Grow { to, .. } if to == 85 * GIB));
    // 66 free: 7 spare, under the 8 GiB smallest step: nothing pending.
    let i = Inputs {
        host_free_bytes: Some(66 * GIB),
        ..i
    };
    assert_eq!(
        pending(&i),
        Pending::Blocked(
            Trigger::BelowFloor,
            Blocked::HostRoom {
                spare_bytes: 7 * GIB
            }
        )
    );
    // Without the debt the same Mac would look roomy: the check must not pass on it.
    let full = Inputs {
        disk: Some(disk(64, 64)),
        ..i
    };
    assert!(matches!(pending(&full), Pending::Grow { .. }));
}

#[test]
fn the_built_size_matches_setup() {
    assert_eq!(BUILT_GIB * GIB, crate::setup::linux_image::DISK_BYTES);
    assert_eq!(MAX_CEILING_GIB * GIB, crate::vm::proto::DISK_BYTES_MAX);
}

// --- the applier ------------------------------------------------------------

#[derive(Default)]
struct World {
    in_service: bool,
    disk: Option<DiskFile>,
    host_free: Option<u64>,
    vm_free_mib: Option<u64>,
    busy: usize,
    /// Each retire pass's answer, in order; empty = no runner left.
    retires: VecDeque<Option<Left>>,
    held: bool,
    stops: usize,
    resize: Option<Result<(), String>>,
    resizes: usize,
    grown: Vec<(u64, u64)>,
    fault: Option<String>,
    log: Vec<String>,
}

#[derive(Default)]
struct Fake(Mutex<World>);

impl Fake {
    fn w(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap()
    }
}

fn jobs(n: usize) -> Option<Left> {
    let mut l = Left::default();
    for _ in 0..n {
        l.add(Some(T0 + 3 * 60 * MIN));
    }
    Some(l)
}

impl GrowHost for Fake {
    async fn engine(&self) -> Option<(bool, Option<DiskFile>)> {
        let w = self.w();
        Some((w.in_service, w.disk))
    }
    async fn host_free_bytes(&self) -> Option<u64> {
        self.w().host_free
    }
    fn vm_free_mib(&self) -> Option<u64> {
        self.w().vm_free_mib
    }
    async fn busy(&self) -> usize {
        self.w().busy
    }
    async fn retire(&self) -> Option<Left> {
        let mut w = self.w();
        w.log.push("retire".into());
        w.retires.pop_front().unwrap_or_else(|| jobs(0))
    }
    fn hold(&self, on: bool) {
        let mut w = self.w();
        w.held = on;
        w.log.push(format!("hold:{on}"));
    }
    async fn stop_engine(&self) -> anyhow::Result<()> {
        let mut w = self.w();
        // The test that matters: a stop never happens with a runner left.
        w.stops += 1;
        w.in_service = false;
        w.log.push("stop".into());
        Ok(())
    }
    async fn resize(&self) -> Result<(), String> {
        let mut w = self.w();
        w.resizes += 1;
        w.resize.clone().unwrap_or(Ok(()))
    }
    async fn grown(&self, from: u64, to: u64) {
        self.w().grown.push((from, to));
    }
    async fn fault(&self, fault: Option<String>) {
        self.w().fault = fault;
    }
}

/// A host whose disk has been short long enough, with the VM in service at 64 GiB.
fn short_host() -> (Applier, Fake) {
    let grow = Grow::new(128, 25);
    {
        let mut s = grow.lock();
        s.clocks.gate_refused(T0);
    }
    grow.in_service();
    let fake = Fake::default();
    {
        let mut w = fake.w();
        w.in_service = true;
        w.disk = Some(disk(64, 60));
        w.host_free = Some(500 * GIB);
        w.vm_free_mib = Some(28 * 1024);
    }
    (Applier { grow }, fake)
}

/// The gate keeps refusing, as it does while claims park behind it.
fn refusing(a: &Applier, now: u64) {
    a.grow.gate_refused(now);
}

#[tokio::test]
async fn the_resize_runs_once_per_start_and_its_report_unblocks_growth() {
    let (a, host) = short_host();
    a.tick(&host, T0 + MIN).await;
    assert_eq!(host.w().resizes, 1);
    a.tick(&host, T0 + 2 * MIN).await;
    assert_eq!(host.w().resizes, 1, "once per start, not per tick");
    a.grow.in_service();
    a.tick(&host, T0 + 3 * MIN).await;
    assert_eq!(host.w().resizes, 2);
}

#[tokio::test]
async fn a_failed_resize_raises_the_fault_and_never_grows() {
    let (a, host) = short_host();
    host.w().resize = Some(Err("growpart is missing in the guest".into()));
    for m in [1, 31, 40] {
        refusing(&a, T0 + m * MIN);
        a.tick(&host, T0 + m * MIN).await;
    }
    let w = host.w();
    assert_eq!(w.fault.as_deref(), Some("growpart is missing in the guest"));
    assert_eq!(w.stops, 0);
    assert!(!w.held);
    assert_eq!(a.grow.start_size(), None);
    let v = a.grow.view().unwrap();
    assert_eq!(v.pending_to_bytes, None);
    assert_eq!(v.fault.as_deref(), Some("growpart is missing in the guest"));
}

#[tokio::test]
async fn the_next_start_takes_the_target_while_a_grow_waits() {
    let (a, host) = short_host();
    host.w().busy = 2;
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    assert_eq!(a.grow.start_size(), Some(96 * GIB));
    assert!(!host.w().held, "busy runners: no idle moment yet");
    let v = a.grow.view().unwrap();
    assert_eq!(v.pending_to_bytes, Some(96 * GIB));
    assert!(v.waiting_for.unwrap().contains("idle moment"));
}

#[tokio::test]
async fn an_idle_moment_holds_retires_checks_again_and_stops() {
    let (a, host) = short_host();
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    {
        let w = host.w();
        assert_eq!(w.log, ["hold:true", "retire", "retire", "stop"]);
        assert_eq!(w.stops, 1);
    }
    assert!(
        a.grow.restarting(),
        "revival held off, the Docker fault muted"
    );
    assert_eq!(a.grow.start_size(), Some(96 * GIB));
    // The driver starts it at the new size and it comes into service.
    {
        let mut w = host.w();
        w.in_service = true;
        w.disk = Some(disk(96, 60));
    }
    a.grow.in_service();
    assert!(!a.grow.restarting());
    refusing(&a, T0 + 32 * MIN);
    a.tick(&host, T0 + 32 * MIN).await;
    let w = host.w();
    assert!(!w.held, "claiming again after the restart");
    assert_eq!(w.grown, [(64 * GIB, 96 * GIB)]);
    assert_eq!(w.resizes, 2, "the guest grew root at the new start");
    assert_eq!(w.stops, 1);
}

#[tokio::test]
async fn the_idle_race_resumes_claiming_without_a_stop() {
    let (a, host) = short_host();
    // The first pass leaves nothing; by the second, GitHub handed a runner a job.
    host.w().retires = VecDeque::from([jobs(0), jobs(1)]);
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    let w = host.w();
    assert_eq!(w.log, ["hold:true", "retire", "retire", "hold:false"]);
    assert_eq!(w.stops, 0);
    assert!(!a.grow.restarting());
}

#[tokio::test]
async fn a_busy_runner_on_the_first_pass_ends_the_idle_attempt() {
    let (a, host) = short_host();
    host.w().retires = VecDeque::from([jobs(1)]);
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    let w = host.w();
    assert_eq!(w.log, ["hold:true", "retire", "hold:false"]);
    assert_eq!(w.stops, 0);
}

#[tokio::test]
async fn after_two_hours_it_drains_and_never_stops_a_vm_with_a_job_in_it() {
    let (a, host) = short_host();
    host.w().busy = 1;
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    assert!(!host.w().held);
    // Two hours pending: drain. The busy runner keeps it waiting, tick after tick.
    host.w().retires = VecDeque::from([jobs(1), jobs(1), jobs(1), None]);
    for m in [151, 152, 153, 154] {
        refusing(&a, T0 + m * MIN);
        a.tick(&host, T0 + m * MIN).await;
        let w = host.w();
        assert!(w.held, "minute {m}: claims stay held while draining");
        assert_eq!(w.stops, 0, "minute {m}: never stopped with a job left");
    }
    assert!(a
        .grow
        .view()
        .unwrap()
        .waiting_for
        .unwrap()
        .contains("Docker"));
    // The job finishes: the next pass leaves nothing, the check agrees, it stops.
    refusing(&a, T0 + 155 * MIN);
    a.tick(&host, T0 + 155 * MIN).await;
    assert_eq!(host.w().stops, 1);
}

#[tokio::test]
async fn a_drain_outlasts_the_gate_refusals_its_own_hold_stops() {
    let (a, host) = short_host();
    host.w().busy = 1;
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    // Drain from minute 151; from then on no claim reaches the gate.
    host.w().retires = VecDeque::from([jobs(1); 30]);
    refusing(&a, T0 + 151 * MIN);
    for m in 151..170 {
        a.tick(&host, T0 + m * MIN).await;
        assert!(host.w().held, "minute {m}: the drain holds on");
    }
    assert_eq!(host.w().stops, 0);
}

#[test]
fn the_gate_clock_does_not_age_while_the_grow_holds_claims() {
    let mut c = Clocks::default();
    c.gate_refused(T0);
    c.gate_refused(T0 + 30 * MIN);
    assert_eq!(c.trigger(T0 + 60 * MIN, false), None);
    assert_eq!(c.trigger(T0 + 60 * MIN, true), Some(Trigger::GateRefusing));
}

#[tokio::test]
async fn the_operator_applies_a_pending_grow_at_once() {
    let (a, host) = short_host();
    host.w().busy = 1;
    assert!(
        a.grow.force().is_err(),
        "nothing pending before the first tick"
    );
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    assert!(a.grow.force().unwrap().contains("96 GiB"));
    host.w().retires = VecDeque::from([jobs(1)]);
    refusing(&a, T0 + 32 * MIN);
    a.tick(&host, T0 + 32 * MIN).await;
    assert!(host.w().held, "forced: drains rather than waiting for idle");
    assert_eq!(host.w().stops, 0);
}

#[tokio::test]
async fn a_grow_that_stops_being_pending_releases_the_hold() {
    let (a, host) = short_host();
    host.w().busy = 1;
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    a.grow.force().unwrap();
    host.w().retires = VecDeque::from([jobs(1)]);
    refusing(&a, T0 + 32 * MIN);
    a.tick(&host, T0 + 32 * MIN).await;
    assert!(host.w().held);
    // The gate lets a job through: no longer short.
    a.grow.gate_passed();
    a.tick(&host, T0 + 33 * MIN).await;
    assert!(!host.w().held);
    assert_eq!(a.grow.start_size(), None);
}

#[tokio::test]
async fn a_refused_start_raises_the_fault_and_drops_the_target() {
    let (a, host) = short_host();
    host.w().busy = 1;
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    assert_eq!(a.grow.start_size(), Some(96 * GIB));
    a.grow.start_refused(96 * GIB);
    assert_eq!(a.grow.start_size(), None);
    refusing(&a, T0 + 32 * MIN);
    a.tick(&host, T0 + 32 * MIN).await;
    assert_eq!(a.grow.start_size(), None, "held by the fault");
    assert!(a.grow.view().unwrap().fault.is_some());
    assert!(
        host.w().fault.is_some(),
        "the refusal reaches the dashboard"
    );
    // A resize that succeeds does not clear it; the operator does.
    a.grow.in_service();
    refusing(&a, T0 + 33 * MIN);
    a.tick(&host, T0 + 33 * MIN).await;
    assert!(host.w().fault.is_some());
    assert!(a.grow.force().is_ok());
    refusing(&a, T0 + 34 * MIN);
    a.tick(&host, T0 + 34 * MIN).await;
    assert_eq!(host.w().fault, None);
    assert_eq!(a.grow.start_size(), Some(96 * GIB));
}

#[tokio::test]
async fn a_failed_bring_up_ends_the_restart() {
    let (a, host) = short_host();
    refusing(&a, T0 + 31 * MIN);
    a.tick(&host, T0 + 31 * MIN).await;
    assert!(a.grow.restarting());
    a.grow.bring_up_failed();
    assert!(!a.grow.restarting());
    a.tick(&host, T0 + 32 * MIN).await;
    assert!(!host.w().held, "claims resume once the restart is over");
}

// --- the view and §7 ---------------------------------------------------------

#[test]
fn the_status_line_says_what_a_grow_waits_for() {
    let v = GrowView {
        disk_bytes: Some(64 * GIB),
        disk_allocated_bytes: Some(60 * GIB),
        max_bytes: 128 * GIB,
        pending_to_bytes: Some(96 * GIB),
        waiting_for: Some("waiting for an idle moment".into()),
        fault: None,
    };
    assert_eq!(
        v.line().unwrap(),
        "linux VM 64 GiB of at most 128 GiB; growing to 96 GiB — waiting for an idle moment"
    );
    assert_eq!(
        GrowView {
            pending_to_bytes: None,
            waiting_for: None,
            ..v.clone()
        }
        .line(),
        None
    );
}

#[test]
fn the_cache_sum_counts_as_section_7_defines() {
    let mut s = CacheSum {
        mirror_gib: Some(20),
        cache_volumes_gib: vec![3.0, 5.0],
        cache_max_gib: Some(12),
        git_mirrors_gib: 1.0,
        variants_gib: 2.0,
        floor_gib: 25,
    };
    // 20 + 2 × 12 + 1 + 2 + 25 = 72 > 32: over half a 64 GiB disk.
    let w = s.warning(64 * GIB).unwrap();
    assert!(w.contains("72 GiB"), "{w}");
    assert!(
        w.contains("2 cache volume(s) at host.cache_max_gib = 12"),
        "{w}"
    );
    // Under half a 160 GiB disk.
    assert_eq!(s.warning(160 * GIB), None);
    // No limit: each volume at its current size, and the warning says so.
    s.cache_max_gib = None;
    let w = s.warning(64 * GIB).unwrap();
    assert!(
        w.contains("56 GiB") && w.contains("no host.cache_max_gib"),
        "{w}"
    );
    // No [mirror]: it counts nothing.
    s.mirror_gib = None;
    assert_eq!(s.warning(96 * GIB), None);
}
