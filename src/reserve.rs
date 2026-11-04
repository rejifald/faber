//! What a Docker admission debits, and the fallback that takes the bet back —
//! ADR-0033 §2 and §4.
//!
//! `[autolimit] reserve` is an enum: `"max"` (absent, the default) debits each
//! class's cap, as faber always has; `"p75"` debits the smaller p75 reservation
//! ([`crate::limiter`], ADR-0033 §1). Either way the container's `--memory` limit
//! is the cap.
//!
//! Under `"p75"` a sign that the overcommit lost — a memory freeze (§3), a host OOM
//! (§5), or an unreadable memory probe right after a low reading — **trips** the
//! fallback: the host reserves as under `"max"` for a cool-off, then re-arms on its
//! own. The first trip cools off for 24 hours; each further trip within 7 armed
//! days of the last doubles it, up to 7 days; 7 armed days without a trip reset it
//! to 24 hours. **Armed time is time spent with p75 in force**: counted on the
//! memory tick while the daemon runs under `"p75"` outside a cool-off, so neither a
//! cool-off, nor time under `"max"`, nor the daemon being down counts.
//!
//! **One squeeze is one trip.** Every trip begins a squeeze, which lasts until a
//! memory reading above `headroom_mib + band_mib`. Through it the memory freeze
//! stays possible even while cooling off — falling back changes what is admitted
//! next, not the memory already in use — and nothing in it extends the cool-off.
//! A squeeze that outlives its cool-off trips again: once p75 is back in force, a
//! further freeze or host OOM is a new sign of overrun.
//!
//! Everything here is pure and takes its clock as an argument, so the policy is
//! tested without a host. The limiter holds one [`Fallback`]; the freeze loop and
//! the reconciler audit the [`Event`]s it returns (`kind: "reserve"`), and a
//! restart rebuilds the state from those lines ([`Fallback::replay`]), the way caps
//! are replayed.

use std::collections::VecDeque;

use serde::Serialize;

use crate::audit::Entry;

/// One day, in milliseconds.
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// The cool-off of a first trip, or of one after 7 armed days without a trip.
pub const FIRST_COOLOFF_MS: u64 = DAY_MS;
/// The longest a cool-off grows to.
pub const MAX_COOLOFF_MS: u64 = 7 * DAY_MS;
/// A trip within this much armed time of the last one doubles the cool-off; past
/// it, the next cool-off is back to [`FIRST_COOLOFF_MS`].
pub const STREAK_MS: u64 = 7 * DAY_MS;
/// The most one gap between memory readings adds to the armed time. Readings come
/// every 8 s; a longer gap is the daemon down or the loop stalled, and time nobody
/// was watching is not counted as armed.
pub const MAX_ARMED_STEP_MS: u64 = 60 * 1000;
/// How much armed time accrues between `armed` checkpoints in the audit log, so a
/// restart loses at most this much of it (and so doubles, if anything, sooner).
pub const ARMED_CHECKPOINT_MS: u64 = 6 * 60 * 60 * 1000;
/// Trips kept for the dashboard. The policy needs only the last one.
const RECENT_TRIPS: usize = 20;

/// The `kind` of a fallback audit line.
pub const AUDIT_KIND: &str = "reserve";

/// What admission debits for a Docker job (ADR-0033 §2), as the limiter runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReserveMode {
    /// Each class's cap — the rolling max × margin. Today's behaviour.
    #[default]
    Max,
    /// `min(cap, p75 × margin)`, with the memory freeze and the fallback behind it.
    P75,
}

impl ReserveMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReserveMode::Max => "max",
            ReserveMode::P75 => "p75",
        }
    }
}

/// What tripped the fallback (ADR-0033 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TripCause {
    /// faber froze a job because memory ran critically short (§3).
    Freeze,
    /// The VM-wide OOM killer took a process in a job's containers (§5).
    HostOom,
    /// The memory probe failed right after a reading below `headroom_mib` (§3).
    Probe,
}

impl TripCause {
    pub fn as_str(self) -> &'static str {
        match self {
            TripCause::Freeze => "freeze",
            TripCause::HostOom => "host_oom",
            TripCause::Probe => "probe",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "freeze" => Some(TripCause::Freeze),
            "host_oom" => Some(TripCause::HostOom),
            "probe" => Some(TripCause::Probe),
            _ => None,
        }
    }

    /// One line on what happened, for the log and the audit `cause`.
    pub fn describe(self) -> &'static str {
        match self {
            TripCause::Freeze => "memory ran critically short and faber froze a job",
            TripCause::HostOom => "the VM-wide OOM killer took a process in a job's containers",
            TripCause::Probe => "the memory probe failed right after a reading below headroom_mib",
        }
    }
}

/// One trip of the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Trip {
    pub at_ms: u64,
    pub cause: TripCause,
    pub cooloff_ms: u64,
}

impl Trip {
    /// When the host re-arms: reserves at p75 again.
    pub fn until_ms(&self) -> u64 {
        self.at_ms.saturating_add(self.cooloff_ms)
    }
}

/// Something the fallback did that is audited and logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The fallback tripped: max reservations until `trip.until_ms()`. `armed_ms`
    /// is the armed time since the trip before it.
    Tripped { trip: Trip, armed_ms: u64 },
    /// The cool-off of `trip` ended at `at_ms`: p75 reservations again.
    Rearmed { at_ms: u64, trip: Trip },
    /// A checkpoint of the armed time since the last trip, so a restart keeps it.
    Armed { trip: Trip, armed_ms: u64 },
}

/// The cool-off the next trip gets, after `last`, with `armed_ms` of armed time
/// since it (ADR-0033 §4). A trip during a cool-off has no armed time, and so
/// doubles.
pub fn next_cooloff(last: Option<&Trip>, armed_ms: u64) -> u64 {
    match last {
        None => FIRST_COOLOFF_MS,
        Some(_) if armed_ms >= STREAK_MS => FIRST_COOLOFF_MS,
        Some(t) => t.cooloff_ms.saturating_mul(2).min(MAX_COOLOFF_MS),
    }
}

/// The fallback's state (ADR-0033 §4), and the squeeze it tracks for §3.
#[derive(Debug, Default, Clone)]
pub struct Fallback {
    /// Trips, oldest first, at most [`RECENT_TRIPS`]. The last decides the policy.
    trips: VecDeque<Trip>,
    /// A squeeze is in progress: memory has not yet come back above
    /// `headroom_mib + band_mib` since the trip that began it.
    squeeze: bool,
    /// The last good memory reading was below `headroom_mib`, so a probe failure
    /// right after it trips the fallback.
    last_low: bool,
    /// The last trip's re-arm has been reported.
    rearm_noted: bool,
    /// Armed time (p75 in force) since the last trip, in ms.
    armed_ms: u64,
    /// The armed time last checkpointed to the audit log.
    checkpointed_ms: u64,
    /// When the last memory reading was folded in, to count armed time between
    /// readings. `None` until the first reading of this process.
    last_seen_ms: Option<u64>,
}

impl Fallback {
    /// The trip that decides the cool-off, if any.
    pub fn last(&self) -> Option<&Trip> {
        self.trips.back()
    }

    /// The host is cooling off: it reserves as under `"max"`.
    pub fn cooling(&self, now_ms: u64) -> bool {
        self.last().is_some_and(|t| now_ms < t.until_ms())
    }

    /// Whether admission debits the p75 reservation right now.
    pub fn reserves_p75(&self, mode: ReserveMode, now_ms: u64) -> bool {
        mode == ReserveMode::P75 && !self.cooling(now_ms)
    }

    /// Whether faber may freeze a job for memory right now (§3). Only under
    /// `"p75"`, and not in a cool-off, except through the squeeze that tripped it.
    pub fn may_freeze(&self, mode: ReserveMode, now_ms: u64) -> bool {
        mode == ReserveMode::P75 && (!self.cooling(now_ms) || self.squeeze)
    }

    /// Whether a squeeze is in progress.
    pub fn squeezing(&self) -> bool {
        self.squeeze
    }

    /// The armed time since the last trip.
    pub fn armed_ms(&self) -> u64 {
        self.armed_ms
    }

    /// Treat a squeeze as in progress: a restart found a job still frozen for
    /// memory, so the squeeze that froze it has not ended. The next reading above
    /// `headroom_mib + band_mib` ends it, as any squeeze.
    pub fn resume_squeeze(&mut self) {
        self.squeeze = true;
    }

    /// Trip the fallback for `cause` at `now_ms`. `None` outside `"p75"`, and
    /// within a squeeze while it is still cooling off: that is one trip however
    /// many freezes it takes. A squeeze that outlives its cool-off trips again.
    /// Every trip begins a squeeze, so the freeze stays possible until memory is
    /// back above `headroom_mib + band_mib`.
    pub fn trip(&mut self, mode: ReserveMode, cause: TripCause, now_ms: u64) -> Option<Event> {
        if mode != ReserveMode::P75 || (self.squeeze && self.cooling(now_ms)) {
            return None;
        }
        let armed_ms = self.armed_ms;
        let trip = Trip {
            at_ms: now_ms,
            cause,
            cooloff_ms: next_cooloff(self.last(), armed_ms),
        };
        self.push(trip);
        self.rearm_noted = false;
        self.armed_ms = 0;
        self.checkpointed_ms = 0;
        self.squeeze = true;
        Some(Event::Tripped { trip, armed_ms })
    }

    fn push(&mut self, trip: Trip) {
        self.trips.push_back(trip);
        while self.trips.len() > RECENT_TRIPS {
            self.trips.pop_front();
        }
    }

    /// Fold in one memory reading (`None` = the probe failed) taken at `now_ms`,
    /// and report a probe-failure trip, a re-arm or an armed-time checkpoint.
    ///
    /// - A reading above `headroom_mib + band_mib` ends a squeeze.
    /// - A failed reading right after one below `headroom_mib` trips (§3: under
    ///   real pressure the probe is what fails). It neither freezes nor thaws.
    /// - The time since the previous reading, at most [`MAX_ARMED_STEP_MS`], is
    ///   armed time when p75 was in force across it.
    pub fn observe(
        &mut self,
        mode: ReserveMode,
        available_mib: Option<u64>,
        headroom_mib: u64,
        band_mib: u64,
        now_ms: u64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        if mode != ReserveMode::P75 {
            self.last_seen_ms = None;
            return out;
        }
        if let Some(prev) = self.last_seen_ms {
            if !self.cooling(prev) && !self.cooling(now_ms) {
                let step = now_ms.saturating_sub(prev).min(MAX_ARMED_STEP_MS);
                self.armed_ms = self.armed_ms.saturating_add(step);
            }
        }
        self.last_seen_ms = Some(now_ms);
        match available_mib {
            Some(a) => {
                if a > headroom_mib.saturating_add(band_mib) {
                    self.squeeze = false;
                }
                self.last_low = a < headroom_mib;
            }
            None => {
                if self.last_low {
                    out.extend(self.trip(mode, TripCause::Probe, now_ms));
                }
            }
        }
        if let Some(trip) = self.last().copied() {
            if !self.rearm_noted && now_ms >= trip.until_ms() {
                self.rearm_noted = true;
                out.push(Event::Rearmed {
                    at_ms: now_ms,
                    trip,
                });
            }
            // Checkpoints matter only until the streak would reset anyway.
            if self.checkpointed_ms < STREAK_MS
                && self.armed_ms >= self.checkpointed_ms.saturating_add(ARMED_CHECKPOINT_MS)
            {
                self.checkpointed_ms = self.armed_ms;
                out.push(Event::Armed {
                    trip,
                    armed_ms: self.armed_ms,
                });
            }
        }
        out
    }

    /// Rebuild the state from the audit log's `reserve` lines, oldest first: the
    /// trips, whether the last one's re-arm was already reported, and the armed
    /// time checkpointed since it. A squeeze is not in the log; the freeze loop
    /// resumes one if a job is still frozen for memory ([`Self::resume_squeeze`]).
    pub fn replay(entries: &[Entry]) -> Self {
        let mut out = Self::default();
        for e in entries.iter().filter(|e| e.kind == AUDIT_KIND) {
            match e.action.as_deref() {
                Some("trip") => {
                    let (Some(cause), Some(cooloff_ms)) =
                        (e.reason.as_deref().and_then(TripCause::parse), e.cooloff_ms)
                    else {
                        continue;
                    };
                    // The trip's own time, as the line recorded it; the line's
                    // stamp only for one that recorded no end.
                    let at_ms = e
                        .until_ms
                        .map_or(e.ts_ms, |until| until.saturating_sub(cooloff_ms));
                    out.push(Trip {
                        at_ms,
                        cause,
                        cooloff_ms,
                    });
                    out.rearm_noted = false;
                    out.armed_ms = 0;
                    out.checkpointed_ms = 0;
                }
                Some("rearm") => out.rearm_noted = true,
                Some("armed") => {
                    let armed = e.armed_ms.unwrap_or(0);
                    out.armed_ms = out.armed_ms.max(armed);
                    out.checkpointed_ms = out.armed_ms;
                }
                _ => {}
            }
        }
        out
    }

    /// The state as `/api/state` shows it (`budget.reserve`).
    pub fn view(&self, mode: ReserveMode, unknown: Option<String>, now_ms: u64) -> ReserveView {
        let cooling = self.cooling(now_ms);
        let in_force = if self.reserves_p75(mode, now_ms) {
            ReserveMode::P75
        } else {
            ReserveMode::Max
        };
        ReserveView {
            mode: mode.as_str(),
            unknown,
            in_force: in_force.as_str(),
            cooling,
            squeeze: self.squeeze,
            freeze_armed: self.may_freeze(mode, now_ms),
            trips_7d: self
                .trips
                .iter()
                .filter(|t| now_ms.saturating_sub(t.at_ms) < STREAK_MS)
                .count(),
            last_trip: self.last().map(|t| TripView::of(*t)),
            next_rearm_ms: self.last().filter(|_| cooling).map(Trip::until_ms),
            armed_ms: self.armed_ms,
            next_cooloff_ms: next_cooloff(self.last(), self.armed_ms),
            trips: self.trips.iter().rev().map(|t| TripView::of(*t)).collect(),
        }
    }
}

/// A trip as the dashboard reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TripView {
    pub at_ms: u64,
    pub cause: TripCause,
    pub cooloff_ms: u64,
    pub until_ms: u64,
}

impl TripView {
    fn of(t: Trip) -> Self {
        Self {
            at_ms: t.at_ms,
            cause: t.cause,
            cooloff_ms: t.cooloff_ms,
            until_ms: t.until_ms(),
        }
    }
}

/// `budget.reserve` in `/api/state` (ADR-0033 §2, §4). Additive: an older client
/// ignores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReserveView {
    /// The configured mode, `"max"` or `"p75"`. An unknown value runs as `"max"`.
    pub mode: &'static str,
    /// The unknown value the config gave, if it gave one (boot warns about it).
    pub unknown: Option<String>,
    /// What admission debits right now: `"max"` under `"max"` and in a cool-off.
    pub in_force: &'static str,
    pub cooling: bool,
    pub squeeze: bool,
    /// Whether a memory freeze could happen now (§3).
    pub freeze_armed: bool,
    /// Trips in the last 7 days.
    pub trips_7d: usize,
    pub last_trip: Option<TripView>,
    /// When the host re-arms, while it is cooling off.
    pub next_rearm_ms: Option<u64>,
    /// Armed time (p75 in force) since the last trip.
    pub armed_ms: u64,
    /// The cool-off a trip now would get.
    pub next_cooloff_ms: u64,
    /// Recent trips, newest first.
    pub trips: Vec<TripView>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const P75: ReserveMode = ReserveMode::P75;
    const MAX: ReserveMode = ReserveMode::Max;
    const H: u64 = 1536;
    const B: u64 = 512;
    /// A comfortable reading: ends any squeeze.
    const OK: Option<u64> = Some(H + B + 1);

    fn tripped(e: Option<Event>) -> Trip {
        match e {
            Some(Event::Tripped { trip, .. }) => trip,
            other => panic!("expected a trip, got {other:?}"),
        }
    }

    /// Feed comfortable readings every 8 s over `[from, to)`, as the memory tick
    /// does, returning the events.
    fn tick_through(f: &mut Fallback, from: u64, to: u64) -> Vec<Event> {
        let mut out = Vec::new();
        let mut t = from;
        while t < to {
            out.extend(f.observe(P75, OK, H, B, t));
            t += 8_000;
        }
        out
    }

    #[test]
    fn under_max_nothing_trips_and_admission_debits_the_cap() {
        let mut f = Fallback::default();
        assert_eq!(f.trip(MAX, TripCause::Freeze, 10), None);
        assert_eq!(f.trip(MAX, TripCause::HostOom, 10), None);
        f.observe(MAX, Some(100), H, B, 20);
        assert!(f.observe(MAX, None, H, B, 30).is_empty());
        assert!(!f.reserves_p75(MAX, 40));
        assert!(!f.may_freeze(MAX, 40));
        assert!(f.last().is_none());
    }

    #[test]
    fn a_freeze_trips_to_max_for_24_hours_then_re_arms() {
        let mut f = Fallback::default();
        assert!(f.reserves_p75(P75, 0));
        let t = tripped(f.trip(P75, TripCause::Freeze, 1_000));
        assert_eq!(t.cooloff_ms, DAY_MS);
        assert!(!f.reserves_p75(P75, 1_000 + DAY_MS - 1));
        assert!(f.reserves_p75(P75, 1_000 + DAY_MS));
        // The re-arm is reported once, on the first reading after it.
        f.observe(P75, OK, H, B, 2_000);
        assert_eq!(
            f.observe(P75, Some(10_000), H, B, 1_000 + DAY_MS),
            [Event::Rearmed {
                at_ms: 1_000 + DAY_MS,
                trip: t
            }]
        );
        assert!(f
            .observe(P75, Some(10_000), H, B, 1_008 + DAY_MS)
            .is_empty());
    }

    #[test]
    fn one_squeeze_is_one_trip_while_cooling() {
        let mut f = Fallback::default();
        tripped(f.trip(P75, TripCause::Freeze, 0));
        // A second freeze, and a host OOM, before memory recovers: nothing extends.
        assert_eq!(f.trip(P75, TripCause::Freeze, 100), None);
        assert_eq!(f.trip(P75, TripCause::HostOom, 200), None);
        assert_eq!(f.last().unwrap().cooloff_ms, DAY_MS);
        // Through the squeeze the freeze stays possible, though cooling off.
        assert!(f.may_freeze(P75, 300));
        // Between headroom and headroom + band: still squeezed.
        f.observe(P75, Some(H + B), H, B, 400);
        assert!(f.may_freeze(P75, 500));
        // Above headroom + band the squeeze ends, and the cool-off refuses freezes.
        f.observe(P75, OK, H, B, 600);
        assert!(!f.squeezing());
        assert!(!f.may_freeze(P75, 700));
    }

    #[test]
    fn a_host_oom_trip_keeps_the_freeze_path_open_until_memory_recovers() {
        // ADR-0033 §3: falling back changes what is admitted next, not the memory
        // the jobs already admitted at p75 hold.
        let mut f = Fallback::default();
        let t = tripped(f.trip(P75, TripCause::HostOom, 5));
        assert_eq!(t.cause, TripCause::HostOom);
        assert!(f.squeezing());
        assert!(f.may_freeze(P75, 6), "cooling, but still squeezed");
        // And its freeze is the same squeeze: no second trip.
        assert_eq!(f.trip(P75, TripCause::Freeze, 7), None);
        f.observe(P75, OK, H, B, 8);
        assert!(!f.may_freeze(P75, 9));
    }

    #[test]
    fn a_squeeze_that_outlives_its_cool_off_trips_again() {
        let mut f = Fallback::default();
        let first = tripped(f.trip(P75, TripCause::Freeze, 0));
        // Memory never comes back above headroom + band, and the cool-off ends.
        f.observe(P75, Some(H), H, B, first.until_ms());
        assert!(f.squeezing() && f.reserves_p75(P75, first.until_ms()));
        // p75 is back in force: a further freeze is a new sign of overrun.
        let second = tripped(f.trip(P75, TripCause::Freeze, first.until_ms() + 8_000));
        assert_eq!(
            second.cooloff_ms,
            2 * DAY_MS,
            "no armed time since: doubled"
        );
        assert!(!f.reserves_p75(P75, first.until_ms() + 16_000));
    }

    #[test]
    fn each_further_trip_within_seven_armed_days_doubles_the_cool_off() {
        let mut f = Fallback::default();
        let mut at = 0;
        let mut seen = Vec::new();
        for _ in 0..6 {
            let t = tripped(f.trip(P75, TripCause::HostOom, at));
            seen.push(t.cooloff_ms / DAY_MS);
            // Memory recovers; re-armed for one armed day, then trips again.
            tick_through(&mut f, t.until_ms(), t.until_ms() + DAY_MS);
            at = t.until_ms() + DAY_MS;
        }
        assert_eq!(seen, [1, 2, 4, 7, 7, 7], "doubling, up to 7 days");
    }

    #[test]
    fn armed_time_is_only_time_with_p75_in_force() {
        let mut f = Fallback::default();
        let t = tripped(f.trip(P75, TripCause::Freeze, 0));
        // Readings through the cool-off count nothing.
        tick_through(&mut f, 0, t.until_ms());
        assert_eq!(f.armed_ms(), 0);
        // A day of readings with p75 in force counts (to within one tick).
        tick_through(&mut f, t.until_ms(), t.until_ms() + DAY_MS);
        let day = f.armed_ms();
        assert!(day > DAY_MS - 10_000 && day <= DAY_MS, "{day}");
        // The daemon down for 10 days: the gap counts at most one step.
        f.observe(P75, OK, H, B, t.until_ms() + 11 * DAY_MS);
        assert!(f.armed_ms() <= day + MAX_ARMED_STEP_MS);
        // Time under "max" counts nothing, and breaks the run of readings.
        let before = f.armed_ms();
        f.observe(MAX, OK, H, B, t.until_ms() + 11 * DAY_MS + 8_000);
        f.observe(P75, OK, H, B, t.until_ms() + 11 * DAY_MS + 16_000);
        assert_eq!(f.armed_ms(), before);
        // So 11 wall-clock days later the next trip still doubles.
        let next = tripped(f.trip(P75, TripCause::Freeze, t.until_ms() + 11 * DAY_MS + 24_000));
        assert_eq!(next.cooloff_ms, 2 * DAY_MS);
    }

    #[test]
    fn seven_armed_days_without_a_trip_reset_the_cool_off() {
        let mut f = Fallback::default();
        let t = tripped(f.trip(P75, TripCause::Freeze, 0));
        tick_through(&mut f, t.until_ms(), t.until_ms() + STREAK_MS + 16_000);
        assert!(f.armed_ms() >= STREAK_MS);
        let next = tripped(f.trip(P75, TripCause::Freeze, t.until_ms() + STREAK_MS + 16_000));
        assert_eq!(next.cooloff_ms, FIRST_COOLOFF_MS);
        assert_eq!(next_cooloff(None, 0), FIRST_COOLOFF_MS);
        let last = Trip {
            at_ms: 0,
            cause: TripCause::Freeze,
            cooloff_ms: 4 * DAY_MS,
        };
        assert_eq!(next_cooloff(Some(&last), STREAK_MS - 1), 7 * DAY_MS);
        assert_eq!(next_cooloff(Some(&last), STREAK_MS), FIRST_COOLOFF_MS);
    }

    #[test]
    fn armed_time_is_checkpointed_every_six_armed_hours() {
        let mut f = Fallback::default();
        let t = tripped(f.trip(P75, TripCause::Freeze, 0));
        let events = tick_through(&mut f, t.until_ms(), t.until_ms() + 13 * 60 * 60 * 1000);
        let checkpoints: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                Event::Armed { armed_ms, .. } => Some(*armed_ms),
                _ => None,
            })
            .collect();
        assert_eq!(checkpoints.len(), 2, "{checkpoints:?}");
        assert!(checkpoints[0] >= ARMED_CHECKPOINT_MS);
    }

    #[test]
    fn a_failed_probe_trips_only_right_after_a_low_reading() {
        let mut f = Fallback::default();
        f.observe(P75, Some(H), H, B, 0);
        assert!(
            f.observe(P75, None, H, B, 8).is_empty(),
            "not below headroom"
        );
        f.observe(P75, Some(H - 1), H, B, 16);
        let events = f.observe(P75, None, H, B, 24);
        assert!(
            matches!(
                events.as_slice(),
                [Event::Tripped {
                    trip: Trip {
                        cause: TripCause::Probe,
                        at_ms: 24,
                        ..
                    },
                    ..
                }]
            ),
            "{events:?}"
        );
        assert!(f.squeezing());
        // Still failing: the same squeeze, no second trip.
        assert!(f.observe(P75, None, H, B, 32).is_empty());
    }

    fn line(ts: u64, action: &str, reason: Option<&str>, cooloff: Option<u64>) -> Entry {
        Entry {
            ts_ms: ts,
            kind: AUDIT_KIND.to_string(),
            action: Some(action.to_string()),
            reason: reason.map(str::to_string),
            cooloff_ms: cooloff,
            until_ms: cooloff.map(|c| ts + c),
            ..Entry::default()
        }
    }

    #[test]
    fn a_restart_rebuilds_the_fallback_from_the_audit_lines() {
        let entries = vec![
            line(10, "trip", Some("freeze"), Some(DAY_MS)),
            line(10 + DAY_MS, "rearm", None, None),
            line(10 + 2 * DAY_MS, "trip", Some("host_oom"), Some(2 * DAY_MS)),
            // Lines of other kinds are ignored.
            Entry {
                kind: "freeze".to_string(),
                ..line(5, "trip", Some("freeze"), Some(1))
            },
        ];
        let mut f = Fallback::replay(&entries);
        let last = *f.last().unwrap();
        assert_eq!(last.cause, TripCause::HostOom);
        assert_eq!(last.cooloff_ms, 2 * DAY_MS);
        assert_eq!(last.at_ms, 10 + 2 * DAY_MS);
        let now = 10 + 3 * DAY_MS;
        assert!(f.cooling(now));
        assert!(!f.reserves_p75(P75, now));
        // The next trip doubles from the replayed one.
        assert_eq!(next_cooloff(f.last(), f.armed_ms()), 4 * DAY_MS);
        // Its re-arm was never reported, so the first reading after it reports it.
        assert!(matches!(
            f.observe(P75, Some(9_999), H, B, last.until_ms())
                .as_slice(),
            [Event::Rearmed { .. }]
        ));
    }

    #[test]
    fn replay_takes_the_trips_own_time_and_the_armed_checkpoint() {
        // The line was stamped a little after the trip it records.
        let trip = Entry {
            ts_ms: 1_500,
            until_ms: Some(1_000 + DAY_MS),
            ..line(1_000, "trip", Some("freeze"), Some(DAY_MS))
        };
        let armed = Entry {
            armed_ms: Some(3 * DAY_MS),
            ..line(5 * DAY_MS, "armed", Some("freeze"), None)
        };
        let f = Fallback::replay(&[trip.clone(), armed]);
        assert_eq!(f.last().unwrap().at_ms, 1_000);
        assert_eq!(f.armed_ms(), 3 * DAY_MS);
        // A trip after the checkpoint starts the count again.
        let f = Fallback::replay(&[
            trip.clone(),
            Entry {
                armed_ms: Some(3 * DAY_MS),
                ..line(5 * DAY_MS, "armed", None, None)
            },
            line(6 * DAY_MS, "trip", Some("probe"), Some(2 * DAY_MS)),
        ]);
        assert_eq!(f.armed_ms(), 0);
    }

    #[test]
    fn a_resumed_squeeze_allows_the_second_freeze_through_a_cool_off() {
        let mut f = Fallback::replay(&[line(0, "trip", Some("freeze"), Some(DAY_MS))]);
        assert!(!f.may_freeze(P75, 100), "cooling, no squeeze known");
        f.resume_squeeze();
        assert!(f.may_freeze(P75, 100));
        assert_eq!(
            f.trip(P75, TripCause::Freeze, 200),
            None,
            "the same squeeze"
        );
        f.observe(P75, OK, H, B, 300);
        assert!(!f.may_freeze(P75, 400));
    }

    #[test]
    fn the_view_names_the_mode_in_force_and_the_next_re_arm() {
        let mut f = Fallback::default();
        let v = f.view(P75, None, 0);
        assert_eq!((v.mode, v.in_force, v.cooling), ("p75", "p75", false));
        assert!(v.freeze_armed);
        f.trip(P75, TripCause::Freeze, 100);
        let v = f.view(P75, None, 200);
        assert_eq!((v.mode, v.in_force, v.cooling), ("p75", "max", true));
        assert_eq!(v.next_rearm_ms, Some(100 + DAY_MS));
        assert_eq!(v.trips_7d, 1);
        assert_eq!(v.next_cooloff_ms, 2 * DAY_MS);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["last_trip"]["cause"], "freeze");
        assert_eq!(json["last_trip"]["until_ms"], 100 + DAY_MS);
        assert_eq!(json["armed_ms"], 0);
        let max = Fallback::default().view(MAX, Some("p50".into()), 0);
        assert_eq!((max.mode, max.in_force), ("max", "max"));
        assert_eq!(max.unknown.as_deref(), Some("p50"));
        assert!(!max.freeze_armed);
    }
}
