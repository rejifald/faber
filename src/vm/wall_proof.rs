//! The production [`WallProof`]: a VM's probe proven from the anchor's own
//! rule counters (ADR-0026, "Per job" step 3).
//!
//! `_faber` cannot read pf's counters; the root checker does on every tick and
//! publishes them, on a green tick only, in the verdict file it writes
//! ([`crate::wall::report::CounterReport`]). Root writes, `_faber` reads, on
//! the verdict's own trust terms ([`crate::wall::reader`]); nothing `_faber`
//! writes reaches the checker.
//!
//! A proof takes two of those snapshots:
//!
//! 1. **[`WallProof::arm`]**, called before `probe` is sent: the latest
//!    counters, read before this moment, are the baseline. They must already
//!    hold the slot on a bound network: the checker binds a VM's network on
//!    its first tick after the bridge appears, and a guest can answer before
//!    that. A probe sent then crosses no rule and is never counted, so `arm`
//!    waits for the bind ([`BIND_WAIT`]) before the probe goes.
//! 2. **[`WallProof::verdict`]**, polled after `probe_sent`: the first call
//!    stamps the moment by which the probe was sent, and only counters the
//!    checker read after it can prove anything.
//!
//! "Before" and "after" are on the host's monotonic clock, which the checker
//! stamps each counter read with and `_faber` reads for itself
//! ([`crate::wall::reader::now_monotonic_nanos`]): one clock for both
//! processes, and one that never steps. The wall clock plays no part, so a
//! clock stepped back between the probe and the verdict cannot make a reading
//! taken before the probe look like one taken after it. Both snapshots must
//! carry one load generation, which a checker restart renews, so they are
//! always from one checker run in one boot, where that clock means one thing.
//!
//! Between the two, the slot's `host` label must have counted at least
//! [`PROBE_PACKETS`] packets, the number the probe sends to its gateway, under
//! one load generation of one stored set, on a network that holds that slot
//! alone ([`crate::wall::counters::prove_bound`]). Then the answer is
//! [`WallVerdict::Held`]. The counters cannot tell the probe's own packets
//! from any other the guest sent to a host address in that window: what the
//! proof shows is that the anchor's quick block-to-host rule for that slot
//! dropped at least that many packets from it, not which ones. Anything short of that — no baseline, a red or
//! unreadable verdict, no counters published, a reload, a different stored
//! set, too few packets counted — is [`WallVerdict::Unreadable`], retried
//! until the probe deadline and then a failure. The counters have no
//! "leaked" reading (the slot's `public` label also counts the guest's own
//! traffic), so this never answers [`WallVerdict::Breached`]: an unproven
//! probe keeps the VM from work just the same.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::{sleep, Instant};
use tracing::{debug, warn};

use super::job::{WallProof, WallVerdict};
use super::proto::{ClaimId, Slot};
use crate::wall::counters::{prove_bound, ProbeTarget, Proof, Snapshot};
use crate::wall::reader::Reading;
use crate::wall::report::{CounterReport, Judgement, VerdictFile};

/// The datagrams the launcher has the guest send to its gateway, port 9: the
/// macOS agent's probe and the Linux guest's alike (`launcher/Sources/Agent.swift`,
/// `launcher/Sources/LinuxGuest.swift`). The gateway is a host address, so the
/// slot's `host` rules must count each of them.
pub const PROBE_PACKETS: u64 = 3;

/// Where the verdict comes from, and the clock it is compared against.
pub trait VerdictSource: Send + Sync {
    fn read(&self) -> impl Future<Output = Reading> + Send;
    /// The monotonic clock the checker stamps its counter reads with
    /// ([`CounterReport::read_at_monotonic_nanos`]), in nanoseconds; 0 when it
    /// cannot be read, which proves nothing.
    fn now_monotonic_nanos(&self) -> u64;
}

/// The checker's verdict file where setup installs it.
pub struct VerdictFileSource;

impl VerdictSource for VerdictFileSource {
    async fn read(&self) -> Reading {
        crate::wall::reader::read().await
    }
    fn now_monotonic_nanos(&self) -> u64 {
        crate::wall::reader::now_monotonic_nanos()
    }
}

/// The counters before a probe, and their stored set.
#[derive(Debug, Clone)]
struct Baseline {
    snapshot: Snapshot,
    digest: String,
}

/// One claim's proof in progress.
#[derive(Debug, Clone)]
struct Armed {
    before: Result<Baseline, String>,
    /// When the first verdict was asked for, on the monotonic clock: the
    /// probe was sent by then.
    probed_by: Option<u64>,
}

/// How long [`WallProof::arm`] waits for the checker to bind a just-started
/// VM's network, and how often it looks. The checker binds a network on the
/// first tick after its bridge appears, every
/// [`crate::wall::cli::DEFAULT_INTERVAL_SECS`]; a guest can answer and be
/// probed before that tick, and a probe sent then crosses no rules at all.
pub const BIND_WAIT: Duration = Duration::from_secs(30);
pub const BIND_POLL: Duration = Duration::from_secs(1);

/// The wall proof from the root checker's published counters.
pub struct CounterWall<V = VerdictFileSource> {
    source: V,
    armed: Mutex<HashMap<ClaimId, Armed>>,
    bind_wait: Duration,
    bind_poll: Duration,
}

impl CounterWall {
    /// The proof against the verdict file setup installs.
    pub fn for_this_host() -> Self {
        Self::new(VerdictFileSource).waiting_for_bind(BIND_WAIT, BIND_POLL)
    }
}

impl<V> CounterWall<V> {
    /// A proof whose `arm` reads once and never waits for a bind.
    pub fn new(source: V) -> Self {
        Self {
            source,
            armed: Mutex::new(HashMap::new()),
            bind_wait: Duration::ZERO,
            bind_poll: BIND_POLL,
        }
    }

    /// `arm` waits up to `wait`, reading every `poll`, until the slot's network
    /// is bound in the counters.
    pub fn waiting_for_bind(mut self, wait: Duration, poll: Duration) -> Self {
        self.bind_wait = wait;
        self.bind_poll = poll;
        self
    }
}

/// The file and its counters, if the reading may be acted on: green, or green
/// with older rules (the stored rules are what applies, and admission decides
/// on out-of-date for itself), and carrying counters.
fn usable(reading: &Reading) -> Result<(&VerdictFile, &CounterReport), String> {
    match &reading.judgement {
        Judgement::Green | Judgement::OutOfDate { .. } => {}
        Judgement::Red(why) => return Err(why.clone()),
    }
    let file = reading
        .file
        .as_ref()
        .ok_or("the verdict file was not parsed")?;
    let counters = file
        .counters
        .as_ref()
        .ok_or("the checker published no counters")?;
    Ok((file, counters))
}

/// The counters a probe from `slot` is proven against: fresh green counters
/// from before the probe, with the slot on exactly one bound network, so the
/// anchor already holds rules that count its traffic. Pure.
fn baseline(reading: &Reading, armed_at: u64, slot: Slot) -> Result<Baseline, String> {
    let (file, counters) = usable(reading)?;
    if armed_at == 0 {
        return Err("the monotonic clock could not be read".into());
    }
    let read_at = counters.read_at_monotonic_nanos;
    if read_at == 0 || read_at >= armed_at {
        return Err("the checker's counters are not from before the probe".into());
    }
    let slot = slot.get();
    if counters
        .networks
        .iter()
        .filter(|b| b.slots.contains(&slot))
        .count()
        != 1
    {
        return Err(format!(
            "slot {slot} is not on exactly one bound network yet: no rule counts its traffic"
        ));
    }
    Ok(Baseline {
        snapshot: counters.snapshot(file)?,
        digest: file.digest.clone(),
    })
}

/// Whether `reading` proves the probe sent from `slot` by `probed_by` was
/// stopped, against the counters taken before it. Pure.
fn judge(
    before: &Result<Baseline, String>,
    probed_by: u64,
    reading: &Reading,
    slot: Slot,
) -> Result<(), String> {
    let before = before
        .as_ref()
        .map_err(|why| format!("no counters from before the probe: {why}"))?;
    let (file, counters) = usable(reading)?;
    if probed_by == 0 {
        return Err("the monotonic clock could not be read".into());
    }
    if counters.read_at_monotonic_nanos <= probed_by {
        return Err("the checker has not read the counters since the probe".into());
    }
    if file.digest != before.digest {
        return Err("the stored rule set changed during the probe".into());
    }
    let after = counters.snapshot(file)?;
    // Protocol slot N runs on the plan's one network holding slot N, as the
    // launcher picks it.
    let slot = slot.get();
    let mut holding = counters.networks.iter().filter(|b| b.slots.contains(&slot));
    let (Some(network), None) = (holding.next(), holding.next()) else {
        return Err(format!("slot {slot} is not on exactly one bound network"));
    };
    match prove_bound(
        &before.snapshot,
        &after,
        &counters.networks,
        network.net,
        slot,
        ProbeTarget::Host,
        PROBE_PACKETS,
    ) {
        Proof::Stopped => Ok(()),
        Proof::Unproven(why) => Err(why),
    }
}

impl<V: VerdictSource> WallProof for CounterWall<V> {
    /// Waits, up to the bind wait, for a baseline: the probe is sent only once
    /// the anchor counts the slot's traffic, or it would prove nothing.
    async fn arm(&self, slot: Slot, claim: &ClaimId) {
        let deadline = Instant::now() + self.bind_wait;
        let before = loop {
            let armed_at = self.source.now_monotonic_nanos();
            let reading = self.source.read().await;
            let before = baseline(&reading, armed_at, slot);
            let left = deadline.saturating_duration_since(Instant::now());
            if before.is_ok() || left.is_zero() {
                break before;
            }
            sleep(left.min(self.bind_poll)).await;
        };
        if let Err(why) = &before {
            if self.bind_wait.is_zero() {
                debug!(claim = claim.as_str(), "wall proof: no baseline: {why}");
            } else {
                warn!(
                    claim = claim.as_str(),
                    "wall proof: no baseline within {:?}, so the probe proves nothing: {why}",
                    self.bind_wait
                );
            }
        }
        self.armed.lock().unwrap().insert(
            claim.clone(),
            Armed {
                before,
                probed_by: None,
            },
        );
    }

    async fn verdict(&self, slot: Slot, claim: &ClaimId) -> WallVerdict {
        let now = self.source.now_monotonic_nanos();
        let armed = {
            let mut armed = self.armed.lock().unwrap();
            let Some(a) = armed.get_mut(claim) else {
                debug!(claim = claim.as_str(), "wall proof: never armed");
                return WallVerdict::Unreadable;
            };
            a.probed_by.get_or_insert(now);
            a.clone()
        };
        let reading = self.source.read().await;
        let probed_by = armed.probed_by.unwrap_or(u64::MAX);
        match judge(&armed.before, probed_by, &reading, slot) {
            Ok(()) => {
                self.armed.lock().unwrap().remove(claim);
                WallVerdict::Held
            }
            Err(why) => {
                debug!(claim = claim.as_str(), "wall proof: not yet: {why}");
                WallVerdict::Unreadable
            }
        }
    }

    fn disarm(&self, claim: &ClaimId) {
        self.armed.lock().unwrap().remove(claim);
    }
}

#[cfg(test)]
impl<V> CounterWall<V> {
    /// How many claims are armed.
    pub(crate) fn armed_len(&self) -> usize {
        self.armed.lock().unwrap().len()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::wall::counters::BoundSlots;
    use crate::wall::reader::tests::{evidence, green_at, NOW};
    use crate::wall::reader::{judge_reading, MAX_AGE_SECS};
    use crate::wall::report::State;

    /// A verdict file the test writes, and a clock the test moves. Each read
    /// is judged by the real reader ([`judge_reading`]).
    #[derive(Clone, Default)]
    pub(crate) struct FakeChecker(pub(crate) Arc<Mutex<FakeState>>);

    #[derive(Default)]
    pub(crate) struct FakeState {
        pub(crate) file: Option<VerdictFile>,
        /// The wall clock, in unix nanoseconds: what the reader judges the
        /// file's freshness by.
        pub(crate) now_nanos: u64,
        /// The host's monotonic clock: what the proof compares.
        pub(crate) mono_nanos: u64,
    }

    /// Where the fake monotonic clock starts: a host up for an hour.
    pub(crate) const MONO_START: u64 = 3_600 * SEC;

    pub(crate) const SEC: u64 = 1_000_000_000;

    impl FakeChecker {
        /// A checker whose wall clock reads [`NOW`].
        pub(crate) fn new() -> Self {
            let f = Self::default();
            {
                let mut s = f.0.lock().unwrap();
                s.now_nanos = NOW * SEC;
                s.mono_nanos = MONO_START;
            }
            f
        }
        /// Time passes: both clocks move on.
        pub(crate) fn advance(&self, secs: u64) {
            let mut s = self.0.lock().unwrap();
            s.now_nanos += secs * SEC;
            s.mono_nanos += secs * SEC;
        }
        /// The wall clock is stepped back, as NTP or an operator may; the
        /// monotonic clock is not.
        pub(crate) fn step_wall_back(&self, secs: u64) {
            self.0.lock().unwrap().now_nanos -= secs * SEC;
        }
        pub(crate) fn now(&self) -> u64 {
            self.0.lock().unwrap().now_nanos
        }
        pub(crate) fn mono(&self) -> u64 {
            self.0.lock().unwrap().mono_nanos
        }
        /// The checker ticks now: a green file with `host` packets on network
        /// `slot + 1`'s one slot, read a moment ago.
        pub(crate) fn tick(&self, slot: u8, host_packets: u64) {
            let now = self.now();
            let mut f = green_at(now / SEC);
            f.counters = Some(counters(slot, host_packets, now - 1, self.mono() - 1));
            self.set(f);
        }
        pub(crate) fn set(&self, f: VerdictFile) {
            self.0.lock().unwrap().file = Some(f);
        }
    }

    impl VerdictSource for FakeChecker {
        async fn read(&self) -> Reading {
            let s = self.0.lock().unwrap();
            let evidence = s
                .file
                .as_ref()
                .map(evidence)
                .ok_or_else(|| "ENOENT".to_string());
            judge_reading(evidence, s.now_nanos / SEC)
        }
        fn now_monotonic_nanos(&self) -> u64 {
            self.0.lock().unwrap().mono_nanos
        }
    }

    /// pf as a test supervisor drives it: every read is a fresh tick, and
    /// every slot's `host` rule has counted [`PROBE_PACKETS`] for each probe
    /// `probes` reports sent, as an anchor that stops them would. `red` makes
    /// every tick red, with no counters.
    pub(crate) struct TickingPf<F> {
        pub(crate) checker: FakeChecker,
        pub(crate) probes: F,
        pub(crate) red: bool,
    }

    impl<F: Fn() -> u64 + Send + Sync> TickingPf<F> {
        pub(crate) fn new(probes: F, red: bool) -> Self {
            Self {
                checker: FakeChecker::new(),
                probes,
                red,
            }
        }
    }

    impl<F: Fn() -> u64 + Send + Sync> VerdictSource for TickingPf<F> {
        async fn read(&self) -> Reading {
            // The checker read the counters a moment ago; its clock then moves on.
            let now = self.checker.now();
            let mono = self.checker.mono();
            let mut f = green_at(now / SEC);
            if self.red {
                f.state = State::Red;
                f.reasons = vec!["pf is disabled".into()];
            } else {
                let host = 10 + PROBE_PACKETS * (self.probes)();
                let mut all = counters(0, host, now - 1, mono - 1);
                all.networks.clear();
                for slot in 0..=15u8 {
                    let one = counters(slot, host, now - 1, mono - 1);
                    all.networks.extend(one.networks.into_iter().take(1));
                    all.packets.extend(one.packets);
                }
                f.counters = Some(all);
            }
            self.checker.set(f);
            let reading = self.checker.read().await;
            self.checker.advance(1);
            reading
        }
        fn now_monotonic_nanos(&self) -> u64 {
            self.checker.now_monotonic_nanos()
        }
    }

    pub(crate) fn counters(
        slot: u8,
        host_packets: u64,
        read_at: u64,
        read_at_mono: u64,
    ) -> CounterReport {
        let net = slot + 1;
        let mut packets = BTreeMap::new();
        packets.insert(format!("faber-wall/n{net}/s{slot}/host"), host_packets);
        packets.insert(format!("faber-wall/n{net}/s{slot}/public"), 40);
        packets.insert(format!("faber-wall/n{net}/rest"), 2);
        CounterReport {
            read_at_unix_nanos: read_at,
            read_at_monotonic_nanos: read_at_mono,
            networks: vec![
                BoundSlots {
                    net,
                    slots: vec![slot],
                },
                BoundSlots {
                    net: 99,
                    slots: vec![98],
                },
            ],
            packets,
        }
    }

    fn slot(n: u8) -> Slot {
        Slot::try_from(n).unwrap()
    }

    fn claim() -> ClaimId {
        ClaimId::try_from("c0").unwrap()
    }

    /// Arm, send a probe the anchor counts `counted` of, tick, and ask.
    async fn proof(counted: u64) -> WallVerdict {
        let checker = FakeChecker::new();
        checker.tick(3, 10);
        let wall = CounterWall::new(checker.clone());
        checker.advance(1);
        wall.arm(slot(3), &claim()).await;
        // The probe: no newer counters yet.
        assert_eq!(
            wall.verdict(slot(3), &claim()).await,
            WallVerdict::Unreadable
        );
        checker.advance(1);
        checker.tick(3, 10 + counted);
        wall.verdict(slot(3), &claim()).await
    }

    #[tokio::test]
    async fn every_probe_packet_counted_on_the_slots_host_rule_is_held() {
        assert_eq!(proof(PROBE_PACKETS).await, WallVerdict::Held);
        assert_eq!(proof(PROBE_PACKETS + 5).await, WallVerdict::Held);
    }

    #[tokio::test]
    async fn fewer_packets_counted_than_sent_is_never_held() {
        assert_eq!(proof(PROBE_PACKETS - 1).await, WallVerdict::Unreadable);
        assert_eq!(proof(0).await, WallVerdict::Unreadable);
    }

    #[tokio::test]
    async fn a_verdict_never_armed_is_unreadable() {
        let checker = FakeChecker::new();
        checker.tick(3, 1000);
        let wall = CounterWall::new(checker);
        assert_eq!(
            wall.verdict(slot(3), &claim()).await,
            WallVerdict::Unreadable
        );
    }

    /// A change the test makes to the checker's file.
    type Mutation = Box<dyn Fn(&FakeChecker)>;

    /// Each way the second reading can fail, from a good baseline.
    #[tokio::test]
    async fn anything_short_of_fresh_green_counters_fails_closed() {
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "no counters published",
                Box::new(|c| {
                    c.set(green_at(c.now() / SEC));
                }),
            ),
            (
                "a red verdict",
                Box::new(|c| {
                    c.tick(3, 100);
                    let mut f = c.0.lock().unwrap().file.clone().unwrap();
                    f.state = State::Red;
                    f.reasons = vec!["pf is off".into()];
                    c.set(f);
                }),
            ),
            (
                "a stale verdict",
                Box::new(|c| {
                    c.tick(3, 100);
                    c.advance(MAX_AGE_SECS + 1);
                }),
            ),
            (
                "the file is gone",
                Box::new(|c| {
                    c.0.lock().unwrap().file = None;
                }),
            ),
            (
                "a schema this release does not read",
                Box::new(|c| {
                    c.tick(3, 100);
                    c.0.lock().unwrap().file.as_mut().unwrap().schema = 99;
                }),
            ),
            (
                "a reload between the snapshots",
                Box::new(|c| {
                    c.tick(3, 100);
                    c.0.lock().unwrap().file.as_mut().unwrap().load_generation = 8;
                }),
            ),
            (
                "another stored set",
                Box::new(|c| {
                    c.tick(3, 100);
                    c.0.lock().unwrap().file.as_mut().unwrap().digest = "e".repeat(64);
                }),
            ),
            (
                "counters read before the probe",
                Box::new(|c| {
                    c.tick(3, 100);
                    let mut f = c.0.lock().unwrap().file.clone().unwrap();
                    f.counters.as_mut().unwrap().read_at_monotonic_nanos = 0;
                    c.set(f);
                }),
            ),
            (
                "a foreign label",
                Box::new(|c| {
                    c.tick(3, 100);
                    let mut f = c.0.lock().unwrap().file.clone().unwrap();
                    f.counters
                        .as_mut()
                        .unwrap()
                        .packets
                        .insert("faber-wall/n4/s3/HOST".into(), 1);
                    c.set(f);
                }),
            ),
            (
                "a second slot on the network",
                Box::new(|c| {
                    c.tick(3, 100);
                    let mut f = c.0.lock().unwrap().file.clone().unwrap();
                    f.counters.as_mut().unwrap().networks[0].slots.push(9);
                    c.set(f);
                }),
            ),
            (
                "the slot on two networks",
                Box::new(|c| {
                    c.tick(3, 100);
                    let mut f = c.0.lock().unwrap().file.clone().unwrap();
                    f.counters.as_mut().unwrap().networks[1].slots = vec![3];
                    c.set(f);
                }),
            ),
            (
                "the network not bound",
                Box::new(|c| {
                    c.tick(3, 100);
                    let mut f = c.0.lock().unwrap().file.clone().unwrap();
                    f.counters.as_mut().unwrap().networks.remove(0);
                    c.set(f);
                }),
            ),
        ];
        for (what, after) in cases {
            let checker = FakeChecker::new();
            checker.tick(3, 10);
            let wall = CounterWall::new(checker.clone());
            checker.advance(1);
            wall.arm(slot(3), &claim()).await;
            assert_eq!(
                wall.verdict(slot(3), &claim()).await,
                WallVerdict::Unreadable
            );
            checker.advance(1);
            after(&checker);
            assert_eq!(
                wall.verdict(slot(3), &claim()).await,
                WallVerdict::Unreadable,
                "{what}"
            );
        }
    }

    #[tokio::test]
    async fn a_baseline_that_is_not_fresh_green_counters_proves_nothing() {
        for (what, before) in [("no file", None), ("no counters", Some(green_at(NOW)))] {
            let checker = FakeChecker::new();
            if let Some(f) = before {
                checker.set(f);
            }
            let wall = CounterWall::new(checker.clone());
            wall.arm(slot(3), &claim()).await;
            let _ = wall.verdict(slot(3), &claim()).await;
            checker.advance(1);
            checker.tick(3, 1000);
            assert_eq!(
                wall.verdict(slot(3), &claim()).await,
                WallVerdict::Unreadable,
                "{what}"
            );
        }
    }

    #[tokio::test]
    async fn another_slots_counters_prove_nothing_for_this_one() {
        let checker = FakeChecker::new();
        checker.tick(3, 10);
        let wall = CounterWall::new(checker.clone());
        checker.advance(1);
        wall.arm(slot(4), &claim()).await;
        let _ = wall.verdict(slot(4), &claim()).await;
        checker.advance(1);
        checker.tick(3, 1000);
        assert_eq!(
            wall.verdict(slot(4), &claim()).await,
            WallVerdict::Unreadable
        );
    }

    /// The wall clock steps back between a reading taken before the probe
    /// and the verdict. On the wall clock that reading would look newer than
    /// the probe; on the monotonic clock it never does.
    #[tokio::test]
    async fn a_reading_from_before_the_probe_never_passes_when_the_clock_steps_back() {
        let checker = FakeChecker::new();
        checker.tick(3, 10);
        let wall = CounterWall::new(checker.clone());
        checker.advance(1);
        wall.arm(slot(3), &claim()).await;
        // Before the probe, the guest's own traffic to a host address is
        // counted: as many packets as a probe sends.
        checker.advance(1);
        checker.tick(3, 10 + PROBE_PACKETS);
        let early = checker.0.lock().unwrap().file.clone().unwrap();
        // The clock steps back, by less than the verdict's freshness window
        // so the file still reads green, and the probe is sent.
        checker.advance(1);
        checker.step_wall_back(5);
        let early_at = early.counters.as_ref().unwrap().read_at_unix_nanos;
        assert!(
            early_at > checker.now(),
            "the test must put the early reading after the probe on the wall clock"
        );
        assert_eq!(
            wall.verdict(slot(3), &claim()).await,
            WallVerdict::Unreadable,
            "a reading from before the probe passed"
        );
        // A reading taken after the probe still proves it.
        checker.advance(1);
        checker.tick(3, 10 + 2 * PROBE_PACKETS);
        assert_eq!(wall.verdict(slot(3), &claim()).await, WallVerdict::Held);
    }

    /// A monotonic clock that cannot be read (0) proves nothing, at either end.
    #[tokio::test]
    async fn an_unreadable_monotonic_clock_proves_nothing() {
        for at_arm in [true, false] {
            let checker = FakeChecker::new();
            checker.tick(3, 10);
            let wall = CounterWall::new(checker.clone());
            checker.advance(1);
            let saved = checker.mono();
            if at_arm {
                checker.0.lock().unwrap().mono_nanos = 0;
            }
            wall.arm(slot(3), &claim()).await;
            checker.0.lock().unwrap().mono_nanos = if at_arm { saved } else { 0 };
            let _ = wall.verdict(slot(3), &claim()).await;
            checker.0.lock().unwrap().mono_nanos = saved;
            checker.advance(1);
            checker.tick(3, 1000);
            assert_eq!(
                wall.verdict(slot(3), &claim()).await,
                WallVerdict::Unreadable,
                "unreadable at {}",
                if at_arm { "arm" } else { "the probe" }
            );
        }
    }

    #[tokio::test]
    async fn every_final_answer_leaves_nothing_armed() {
        use crate::vm::job::Disarm;
        // An unproven proof, disarmed by hand.
        let checker = FakeChecker::new();
        checker.tick(3, 10);
        let wall = CounterWall::new(checker.clone());
        checker.advance(1);
        wall.arm(slot(3), &claim()).await;
        assert_eq!(
            wall.verdict(slot(3), &claim()).await,
            WallVerdict::Unreadable
        );
        assert_eq!(wall.armed_len(), 1);
        wall.disarm(&claim());
        assert_eq!(wall.armed_len(), 0);
        // Asked again after it: nothing to prove it against.
        checker.advance(1);
        checker.tick(3, 1000);
        assert_eq!(
            wall.verdict(slot(3), &claim()).await,
            WallVerdict::Unreadable
        );

        // The guard disarms on any way out of its scope, and leaves other
        // claims armed.
        let other = ClaimId::try_from("c1").unwrap();
        wall.arm(slot(4), &other).await;
        {
            let _disarm = Disarm::new(&wall, &claim());
            wall.arm(slot(3), &claim()).await;
            assert_eq!(wall.armed_len(), 2);
        }
        assert_eq!(wall.armed_len(), 1);
        wall.disarm(&other);
        assert_eq!(wall.armed_len(), 0);
    }

    #[tokio::test]
    async fn a_held_proof_is_spent() {
        let checker = FakeChecker::new();
        checker.tick(3, 10);
        let wall = CounterWall::new(checker.clone());
        checker.advance(1);
        wall.arm(slot(3), &claim()).await;
        let _ = wall.verdict(slot(3), &claim()).await;
        checker.advance(1);
        checker.tick(3, 20);
        assert_eq!(wall.verdict(slot(3), &claim()).await, WallVerdict::Held);
        // Asked again without a new arm: nothing to prove it against.
        assert_eq!(
            wall.verdict(slot(3), &claim()).await,
            WallVerdict::Unreadable
        );
    }

    /// A checker whose network for slot 3 is pending until its `bound_at`th
    /// tick: each read is a fresh tick, and only slot 9 is bound before then.
    /// `host` is what slot 3's `host` rule has counted.
    struct BindsLater {
        checker: FakeChecker,
        reads: Mutex<u32>,
        bound_at: u32,
        host: Mutex<u64>,
    }

    impl VerdictSource for BindsLater {
        async fn read(&self) -> Reading {
            let n = {
                let mut reads = self.reads.lock().unwrap();
                *reads += 1;
                *reads
            };
            let host = *self.host.lock().unwrap();
            self.checker
                .tick(if n >= self.bound_at { 3 } else { 9 }, host);
            let reading = self.checker.read().await;
            self.checker.advance(1);
            reading
        }
        fn now_monotonic_nanos(&self) -> u64 {
            self.checker.now_monotonic_nanos()
        }
    }

    fn binds_later(bound_at: u32) -> CounterWall<BindsLater> {
        CounterWall::new(BindsLater {
            checker: FakeChecker::new(),
            reads: Mutex::new(0),
            bound_at,
            host: Mutex::new(10),
        })
        .waiting_for_bind(BIND_WAIT, BIND_POLL)
    }

    /// The probe after arm: the anchor counts its packets, and the verdict is
    /// asked until a tick after it.
    async fn probe_counted(wall: &CounterWall<BindsLater>) -> WallVerdict {
        let _ = wall.verdict(slot(3), &claim()).await;
        *wall.source.host.lock().unwrap() += PROBE_PACKETS;
        wall.verdict(slot(3), &claim()).await
    }

    #[tokio::test(start_paused = true)]
    async fn arm_waits_for_the_slots_network_to_be_bound() {
        // The guest answers before the checker's next tick binds its network:
        // arm holds the probe back until the anchor counts the slot.
        let wall = binds_later(4);
        let started = Instant::now();
        wall.arm(slot(3), &claim()).await;
        assert_eq!(*wall.source.reads.lock().unwrap(), 4);
        assert!(Instant::now() - started < BIND_WAIT);
        assert_eq!(probe_counted(&wall).await, WallVerdict::Held);
    }

    #[tokio::test(start_paused = true)]
    async fn a_network_never_bound_within_the_wait_is_never_held() {
        let wall = binds_later(u32::MAX);
        let started = Instant::now();
        wall.arm(slot(3), &claim()).await;
        assert!(Instant::now() - started >= BIND_WAIT, "arm gave up early");
        assert_eq!(probe_counted(&wall).await, WallVerdict::Unreadable);
    }

    #[tokio::test]
    async fn a_baseline_without_the_slots_network_proves_nothing() {
        // The bug this guards: the baseline was taken while the network was
        // pending, so no rule counted the probe and no later tick could prove it.
        let checker = FakeChecker::new();
        checker.tick(9, 10);
        let wall = CounterWall::new(checker.clone());
        checker.advance(1);
        wall.arm(slot(3), &claim()).await;
        let _ = wall.verdict(slot(3), &claim()).await;
        checker.advance(1);
        checker.tick(3, 10 + PROBE_PACKETS);
        assert_eq!(
            wall.verdict(slot(3), &claim()).await,
            WallVerdict::Unreadable
        );
    }
}
