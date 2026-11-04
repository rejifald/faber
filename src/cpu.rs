//! Pure CPU-measurement arithmetic for ADR-0019: parsing a cgroup's `cpu.stat`,
//! the VM-wide `/proc/pressure/cpu` and `/proc/uptime`, and turning a sparse
//! series of `cpu.stat` reads into the [`CpuUsage`] a [`crate::docker::RunReport`]
//! carries.
//!
//! Split out of [`crate::docker`] — which owns *when* and *how* a container is
//! read — so the arithmetic, which has real edge cases (a missed poll, a reset
//! counter, a container whose cgroup carries no CFS quota lines at all), is
//! unit-testable without a Docker daemon in the loop.

use crate::docker::CpuUsage;

/// One reading of a cgroup v2 `cpu.stat` — the CFS controller's own file (e.g.
/// `/sys/fs/cgroup/cpu.stat`, read from inside the cgroup's own namespace, so
/// the path always names *that* cgroup's file regardless of which container it
/// is read from).
///
/// `nr_periods` / `nr_throttled` are `None` when the file carries no CFS quota
/// lines at all. On the verification host they are always present (`0` when no
/// period has elapsed), but the field stays optional because nothing here
/// should require it: a host or container runtime that only enables the quota
/// accounting when a `--cpus` limit is actually applied must still measure
/// `usage_usec` correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuStat {
    /// Cumulative CPU time this cgroup has consumed, in microseconds.
    /// Non-decreasing across one cgroup's lifetime — a freshly created cgroup
    /// (the same container name over a fresh cgroup, or a counter read racing a
    /// container restart) starts back at zero, which [`CpuTally::observe`]
    /// treats as a signal to re-baseline rather than a negative rate.
    pub usage_usec: u64,
    /// CFS periods elapsed since the cgroup was created. `None` when the file
    /// carries no CFS quota lines (see the struct doc).
    pub nr_periods: Option<u64>,
    /// Of those periods, how many the cgroup was throttled in — i.e. hit its
    /// `--cpus` quota. `None` alongside `nr_periods`.
    pub nr_throttled: Option<u64>,
    /// Cumulative time the cgroup spent throttled, in microseconds: the CPU time
    /// a job lost to its own quota (ADR-0056 §4). `None` when the file has no
    /// such line.
    pub throttled_usec: Option<u64>,
}

/// Parse a `cpu.stat` file's text. `None` only when `usage_usec` — the one line
/// every reading needs — is missing or unparsable; every other recognized line
/// is independently optional, so a shorter file (no CFS quota lines) still
/// yields a usable reading.
///
/// Tolerant of trailing content it does not recognize — in particular, the
/// `/proc/uptime` line a combined `cat cpu.stat /proc/uptime` read appends
/// (see [`parse_uptime_usec`]): that line's first field is a number, never one
/// of the keys matched below, so the `_ => {}` arm quietly ignores it.
pub fn parse_cpu_stat(text: &str) -> Option<CpuStat> {
    let mut usage_usec = None;
    let mut nr_periods = None;
    let mut nr_throttled = None;
    let mut throttled_usec = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(key), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        match key {
            "usage_usec" => usage_usec = value.parse().ok(),
            "nr_periods" => nr_periods = value.parse().ok(),
            "nr_throttled" => nr_throttled = value.parse().ok(),
            "throttled_usec" => throttled_usec = value.parse().ok(),
            _ => {}
        }
    }
    Some(CpuStat {
        usage_usec: usage_usec?,
        nr_periods,
        nr_throttled,
        throttled_usec,
    })
}

/// Parse the `some` line's `avg60=` field out of `/proc/pressure/cpu`
/// (`"some avg10=.. avg60=.. avg300=.. total=.."`). `None` when the `some` line
/// is absent or the field does not parse — a kernel exposing no PSI, or a probe
/// that read nothing — which the caller treats as "no CPU pressure signal", not
/// a probe failure (see [`crate::docker::VmPressure::cpu_some_avg60`]).
///
/// A figure that parses but is no percentage is held to one ([`pressure_pct`]),
/// never dropped: CPU Gate 2 reads `None` as "no signal" and admits on it (#105).
pub fn parse_psi_some_avg60(text: &str) -> Option<f64> {
    text.lines()
        .find(|line| line.starts_with("some "))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|f| f.strip_prefix("avg60="))
        })
        .and_then(|v| v.parse::<f64>().ok())
        .map(pressure_pct)
}

/// A PSI figure as the percentage CPU Gate 2 weighs: clamped to 0–100, and NaN
/// read as 100 — the most pressure there is (#105).
///
/// `inf` and `NaN` both parse as `f64`, and a figure past 100 is one no kernel
/// prints. Dropping such a reading would turn it into "no signal", which the gate
/// admits on; a figure over 100 used to hold admission, and must go on holding
/// it. So bad input admits less, never more.
fn pressure_pct(pct: f64) -> f64 {
    if pct.is_nan() {
        100.0
    } else {
        pct.clamp(0.0, 100.0)
    }
}

/// Parse `/proc/uptime`'s uptime field — the file's first number, seconds since
/// boot, at 10ms resolution — into microseconds. `/proc/uptime` is VM-wide, not
/// namespaced by cgroup or container, so every reader on the host sees the same
/// clock; that is what lets [`crate::docker::read_cpu_stat`] read it in the
/// very same `docker exec` as `cpu.stat`, right next to the counter it times,
/// rather than timing the read from the caller's side of a call whose own
/// latency can run to minutes under the load ADR-0019 exists to measure.
///
/// Identified by *shape* — the one line with exactly two whitespace-separated
/// fields, both of which parse as a float — rather than by position, so this
/// still finds it at the tail of a combined `cat cpu.stat /proc/uptime` read:
/// every `cpu.stat` line's first field is a name (`usage_usec`, `nr_periods`,
/// …), never a number, so none of them can match. `None` when no line has that
/// shape.
pub fn parse_uptime_usec(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let uptime = fields.next()?;
        let idle = fields.next()?;
        if fields.next().is_some() {
            return None; // more than two fields — not the uptime line
        }
        let uptime: f64 = uptime.parse().ok()?;
        idle.parse::<f64>().ok()?; // must be numeric too, just unused
                                   // A clock no VM could show — negative, `inf`, `NaN`, or past a century of
                                   // uptime — times nothing (#105).
        if !(0.0..=MAX_UPTIME_SECS).contains(&uptime) {
            return None;
        }
        Some((uptime * 1_000_000.0).round() as u64)
    })
}

/// The longest uptime [`parse_uptime_usec`] believes: a century, in seconds.
const MAX_UPTIME_SECS: f64 = 100.0 * 365.0 * 86_400.0;

/// Rate of a `usage_usec` delta over a wall-clock delta, in millicores. Widens
/// to `u128` for the multiply so a long run's microsecond products cannot
/// overflow, and returns `0` for `dwall_us == 0` rather than dividing by it —
/// both callers already guard that case for their own reasons (see
/// [`CpuTally::observe`] and [`CpuTally::finish`]), so this is a last-resort
/// net, not the primary check.
fn rate_mcpu(dusage_us: u64, dwall_us: u64) -> u64 {
    if dwall_us == 0 {
        return 0;
    }
    ((dusage_us as u128 * 1000) / dwall_us as u128) as u64
}

/// Turns a sparse series of [`CpuStat`] reads, taken over the life of one
/// container, into a [`CpuUsage`]. `Copy`/`Default` so a caller can keep one per
/// sampled container as a plain local (see `sample_usage` in
/// [`crate::docker`]) with no `Option` boilerplate at the call site.
///
/// Timestamped in microseconds off `/proc/uptime` (see [`parse_uptime_usec`]),
/// not [`std::time::Instant`]: the reading and its timestamp have to come from
/// the same `docker exec`, so the wall-clock delta between two readings never
/// includes the latency of the `docker` CLI call itself — which, under the CPU
/// load this measurement exists to catch, has run to minutes on this host.
///
/// # The two rates it reports
///
/// `avg_mcpu` answers "how much CPU did the whole sampled run use, on
/// average" — first reading to last, one division. `peak_mcpu` answers "what
/// was the worst sampling interval" — accumulated incrementally as each new
/// reading arrives, so a container that idles for most of the run and then
/// spikes for a single interval is not smoothed away by the average.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuTally {
    /// The reading `finish` computes `avg_mcpu` from: a `(timestamp_us, stat)`
    /// pair. Reset to the current reading whenever `observe` sees the counter
    /// go backwards — see there.
    baseline: Option<(u64, CpuStat)>,
    /// The most recent reading folded in — what the next `observe` measures its
    /// interval against, and what `finish` reads `nr_periods`/`nr_throttled`
    /// from (the throttle ratio as of the end of the run, not summed over it).
    last: Option<(u64, CpuStat)>,
    /// The highest per-interval rate seen so far. Deliberately *not* reset by a
    /// rebaseline (below): it was measured from real intervals before the
    /// reset, and a counter reset does not undo CPU the container already used.
    peak_mcpu: u64,
    /// Readings folded in since the last reset (the first-ever call, or a
    /// counter-backwards rebaseline). `finish` needs at least 2 to have a span
    /// to divide across.
    readings: u32,
}

impl CpuTally {
    /// Fold in one reading: `stat` and the `/proc/uptime` timestamp `at_us`
    /// (microseconds) it was read alongside, in the very same `docker exec` —
    /// see the struct doc for why the timestamp has to come from there rather
    /// than the caller's own clock.
    ///
    /// The first-ever call seeds the baseline and returns. Every later call
    /// computes this interval's rate against the *previous* call (not the
    /// baseline) and folds it into the running peak — except:
    ///
    /// - **the counter went backwards** (`stat.usage_usec` less than the
    ///   previous reading's): the cgroup was recreated under this caller's
    ///   nose, so there is no meaningful interval to compute — this reading
    ///   becomes the new baseline instead of being compared against the old
    ///   one.
    /// - **the timestamp does not advance** (`at_us` at or before the previous
    ///   reading's — an ordinary zero-Δ poll, or in principle a bad reading):
    ///   dividing by zero, or subtracting into a negative on an unsigned
    ///   timestamp, are both off the table, and there is no elapsed time to
    ///   have a rate over either way — so the reading is dropped on the floor,
    ///   as if this call never happened.
    pub fn observe(&mut self, at_us: u64, stat: CpuStat) {
        let Some((prev_us, prev_stat)) = self.last else {
            self.baseline = Some((at_us, stat));
            self.last = Some((at_us, stat));
            self.readings = 1;
            return;
        };

        if stat.usage_usec < prev_stat.usage_usec {
            self.baseline = Some((at_us, stat));
            self.last = Some((at_us, stat));
            self.readings = 1;
            return;
        }

        if at_us <= prev_us {
            return;
        }

        let rate = rate_mcpu(stat.usage_usec - prev_stat.usage_usec, at_us - prev_us);
        self.peak_mcpu = self.peak_mcpu.max(rate);
        self.last = Some((at_us, stat));
        self.readings += 1;
    }

    /// The time the container spent throttled, as of the last reading. A
    /// cumulative counter, so the last reading covers the sampled run; `None`
    /// before any reading carried one.
    pub fn throttled_usec(&self) -> Option<u64> {
        self.last?.1.throttled_usec
    }

    /// Turn the accumulated readings into a [`CpuUsage`], or `None` if there is
    /// no span to report over — fewer than two usable readings since the last
    /// reset, including a tally that has seen only one reading, ever.
    pub fn finish(&self) -> Option<CpuUsage> {
        if self.readings < 2 {
            return None;
        }
        let (first_us, first_stat) = self.baseline?;
        let (last_us, last_stat) = self.last?;
        if last_us <= first_us {
            return None;
        }

        let avg_mcpu = rate_mcpu(
            last_stat.usage_usec.saturating_sub(first_stat.usage_usec),
            last_us - first_us,
        );
        // `nr_throttled`/`nr_periods` are cumulative counters, so the ratio as
        // of the *last* reading already covers the whole run — summing across
        // readings would double-count.
        let throttled_ratio = match (last_stat.nr_throttled, last_stat.nr_periods) {
            (Some(throttled), Some(periods)) if periods > 0 => throttled as f64 / periods as f64,
            _ => 0.0,
        };
        Some(CpuUsage {
            // Guards against integer-truncation making the whole-run average
            // land a hair above the largest single interval it was drawn from.
            peak_mcpu: self.peak_mcpu.max(avg_mcpu),
            avg_mcpu,
            throttled_ratio,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_cpu_stat: fixtures -------------------------------------------

    /// `cat /sys/fs/cgroup/cpu.stat` from inside a faber runner container
    /// mid-job: a synthetic file of the shape ADR-0019 reads.
    const CPU_STAT_UNDER_LOAD: &str = "\
usage_usec 60000
user_usec 20000
system_usec 40000
nice_usec 0
nr_periods 1
nr_throttled 0
throttled_usec 0
nr_bursts 0
burst_usec 0
";

    /// An idle container started with a `--cpus` quota, synthetic. `nr_periods`
    /// is present but small — the CFS controller was enabled but had little to
    /// measure yet.
    const CPU_STAT_IDLE_WITH_QUOTA: &str = "\
usage_usec 70000
user_usec 20000
system_usec 50000
nice_usec 0
nr_periods 2
nr_throttled 0
throttled_usec 0
nr_bursts 0
burst_usec 0
";

    /// Some hosts only enable the CFS quota lines once a `--cpus` limit applies,
    /// although a cpu controller may expose them even at `nr_periods 0`. An
    /// unlimited container's `usage_usec`/`user_usec`/`system_usec` with the
    /// quota lines stripped, so the parser is proven against the shape it must
    /// also accept.
    const CPU_STAT_NO_QUOTA_LINES: &str = "\
usage_usec 25000
user_usec 10000
system_usec 15000
nice_usec 0
";

    #[test]
    fn parses_a_full_cpu_stat_with_quota_lines() {
        let stat = parse_cpu_stat(CPU_STAT_UNDER_LOAD).expect("usage_usec present");
        assert_eq!(stat.usage_usec, 60000);
        assert_eq!(stat.nr_periods, Some(1));
        assert_eq!(stat.nr_throttled, Some(0));
        assert_eq!(stat.throttled_usec, Some(0));

        let stat = parse_cpu_stat(CPU_STAT_IDLE_WITH_QUOTA).expect("usage_usec present");
        assert_eq!(stat.usage_usec, 70000);
        assert_eq!(stat.nr_periods, Some(2));
        assert_eq!(stat.nr_throttled, Some(0));
    }

    #[test]
    fn parses_a_cpu_stat_with_no_quota_lines() {
        let stat = parse_cpu_stat(CPU_STAT_NO_QUOTA_LINES).expect("usage_usec present");
        assert_eq!(stat.usage_usec, 25000);
        assert_eq!(stat.nr_periods, None);
        assert_eq!(stat.nr_throttled, None);
        assert_eq!(stat.throttled_usec, None);
    }

    #[test]
    fn throttled_time_is_the_last_reading() {
        let mut tally = CpuTally::default();
        assert_eq!(tally.throttled_usec(), None);
        for (at, usec) in [(0, 10), (1_000_000, 4_000)] {
            tally.observe(
                at,
                CpuStat {
                    usage_usec: at,
                    throttled_usec: Some(usec),
                    ..stat(0)
                },
            );
        }
        assert_eq!(tally.throttled_usec(), Some(4_000));
    }

    #[test]
    fn a_cpu_stat_with_no_usage_line_is_unreadable() {
        assert_eq!(parse_cpu_stat(""), None);
        assert_eq!(parse_cpu_stat("user_usec 100\nsystem_usec 50\n"), None);
        // A truncated or corrupted number must not silently become zero.
        assert_eq!(parse_cpu_stat("usage_usec notanumber\n"), None);
    }

    // --- parse_psi_some_avg60: fixtures -------------------------------------

    /// A synthetic `cat /proc/pressure/cpu`.
    const PSI_CPU_SAMPLE: &str = "\
some avg10=40.00 avg60=35.50 avg300=30.25 total=2000000000
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
";

    #[test]
    fn parses_avg60_off_the_some_line() {
        assert_eq!(parse_psi_some_avg60(PSI_CPU_SAMPLE), Some(35.5));
    }

    #[test]
    fn a_kernel_with_no_psi_reads_as_no_pressure_signal() {
        // What the probe's `cat /proc/pressure/cpu 2>/dev/null || true` prints
        // when the file does not exist: nothing on stdout.
        assert_eq!(parse_psi_some_avg60(""), None);
        // Present but missing the one field asked for.
        assert_eq!(
            parse_psi_some_avg60("some avg10=1.00 avg300=2.00 total=5\n"),
            None
        );
    }

    // --- parse_uptime_usec: fixtures ----------------------------------------

    /// A synthetic `cat /proc/uptime`.
    const UPTIME_SAMPLE: &str = "100000.50 1000000.25\n";

    /// `cat /sys/fs/cgroup/cpu.stat /proc/uptime` — the exact combined read
    /// [`crate::docker::read_cpu_stat`] issues in one `docker exec` — with
    /// synthetic values, cpu.stat first and the uptime line last.
    const CPU_STAT_AND_UPTIME: &str = "\
usage_usec 50000
user_usec 10000
system_usec 40000
nice_usec 0
nr_periods 0
nr_throttled 0
throttled_usec 0
nr_bursts 0
burst_usec 0
100020.75 1000015.50
";

    #[test]
    fn parses_uptime_seconds_into_microseconds() {
        assert_eq!(parse_uptime_usec(UPTIME_SAMPLE), Some(100_000_500_000));
    }

    /// What `read_cpu_stat` actually hands both parsers: one combined `docker
    /// exec` output. Each must read only its own line back out of it.
    #[test]
    fn each_parser_reads_only_its_own_line_off_the_combined_exec_output() {
        let stat = parse_cpu_stat(CPU_STAT_AND_UPTIME).expect("usage_usec present");
        assert_eq!(stat.usage_usec, 50000);
        assert_eq!(
            parse_uptime_usec(CPU_STAT_AND_UPTIME),
            Some(100_020_750_000)
        );
    }

    #[test]
    fn malformed_or_absent_uptime_lines_are_rejected() {
        assert_eq!(parse_uptime_usec(""), None);
        // One field, not two.
        assert_eq!(parse_uptime_usec("100000.50\n"), None);
        // Three fields — not the shape, even though the first two are floats.
        assert_eq!(parse_uptime_usec("100000.50 1000000.25 extra\n"), None);
        // Two fields, but not both numeric.
        assert_eq!(parse_uptime_usec("usage_usec 50000\n"), None);
        assert_eq!(parse_uptime_usec("100000.50 notanumber\n"), None);
        // #105: numbers, but no clock a VM could show.
        assert_eq!(parse_uptime_usec("inf 1.0\n"), None);
        assert_eq!(parse_uptime_usec("NaN 1.0\n"), None);
        assert_eq!(parse_uptime_usec("-5.0 1.0\n"), None);
        assert_eq!(parse_uptime_usec("1e300 1.0\n"), None);
    }

    #[test]
    fn a_pressure_reading_that_is_not_a_percentage_admits_less_never_more() {
        // #105: CPU Gate 2 admits against this figure, and reads `None` as "no
        // signal", which it admits on. So a figure past 100 — which used to hold
        // admission — still reads as full pressure, and so do `inf` and `NaN`.
        let psi = |avg60: &str| {
            parse_psi_some_avg60(&format!(
                "some avg10=0.00 avg60={avg60} avg300=0.00 total=1\n"
            ))
        };
        for (reading, held_at) in [
            ("100.01", 100.0),
            ("1e9", 100.0),
            ("inf", 100.0),
            ("NaN", 100.0),
            ("-1.00", 0.0),
            ("-inf", 0.0),
            ("40.78", 40.78),
            ("100.00", 100.0),
        ] {
            assert_eq!(psi(reading), Some(held_at), "{reading}");
        }
        // No figure at all is still "no signal": a kernel without PSI.
        assert_eq!(psi("garbage"), None);
    }

    // --- CpuTally ------------------------------------------------------------

    fn stat(usage_usec: u64) -> CpuStat {
        CpuStat {
            usage_usec,
            nr_periods: None,
            nr_throttled: None,
            throttled_usec: None,
        }
    }

    #[test]
    fn a_single_reading_has_no_interval_to_report() {
        let mut tally = CpuTally::default();
        tally.observe(0, stat(1_000));
        assert_eq!(tally.finish(), None);
    }

    #[test]
    fn steady_load_gives_the_same_peak_and_average() {
        let mut tally = CpuTally::default();
        // Half a core, every second, for three seconds.
        tally.observe(0, stat(0));
        tally.observe(1_000_000, stat(500_000));
        tally.observe(2_000_000, stat(1_000_000));
        tally.observe(3_000_000, stat(1_500_000));

        let usage = tally.finish().expect("four readings is enough");
        assert_eq!(usage.avg_mcpu, 500);
        assert_eq!(usage.peak_mcpu, 500);
        assert_eq!(usage.throttled_ratio, 0.0);
    }

    /// A single busy interval among idle ones must show up in `peak_mcpu`
    /// without dragging `avg_mcpu` up to match — that smoothing is exactly what
    /// the average is for, and exactly what would hide the spike if faber only
    /// ever reported one number.
    #[test]
    fn a_spike_raises_the_peak_but_not_the_average() {
        let mut tally = CpuTally::default();
        tally.observe(0, stat(0));
        tally.observe(1_000_000, stat(100_000)); // 100 mcpu
        tally.observe(2_000_000, stat(1_100_000)); // 1000 mcpu spike
        tally.observe(3_000_000, stat(1_200_000)); // 100 mcpu

        let usage = tally.finish().expect("four readings is enough");
        assert_eq!(usage.peak_mcpu, 1000);
        assert_eq!(usage.avg_mcpu, 400);
        assert!(usage.peak_mcpu > usage.avg_mcpu);
    }

    /// Two reads that carry the same timestamp cannot yield a rate (division
    /// by zero) and carry no elapsed time regardless — `observe` must drop the
    /// second one rather than corrupt the running state or panic.
    #[test]
    fn a_reading_with_no_elapsed_time_is_dropped_not_divided_by_zero() {
        let mut tally = CpuTally::default();
        tally.observe(0, stat(0));
        tally.observe(0, stat(999_999)); // same timestamp — must be a no-op
        assert_eq!(tally.finish(), None, "still only one usable reading");

        tally.observe(1_000_000, stat(500_000));
        let usage = tally.finish().expect("now two usable readings");
        // Computed from the original baseline (usage 0), proving the dropped
        // same-timestamp reading never became the baseline or the last reading.
        assert_eq!(usage.avg_mcpu, 500);
    }

    /// A timestamp that goes backwards — `/proc/uptime` should never do this,
    /// but a misread must not be trusted to be well-formed — is dropped the
    /// same way a zero-Δ reading is, rather than underflowing the unsigned
    /// subtraction into a huge bogus rate.
    #[test]
    fn a_backwards_timestamp_is_dropped_not_subtracted_into_a_wraparound() {
        let mut tally = CpuTally::default();
        tally.observe(2_000_000, stat(0));
        tally.observe(1_000_000, stat(500_000)); // timestamp went backwards
        assert_eq!(tally.finish(), None, "still only one usable reading");

        tally.observe(3_000_000, stat(500_000));
        let usage = tally.finish().expect("now two usable readings");
        // (500_000 - 0) * 1000 / (3_000_000 - 2_000_000) = 500 — computed from
        // the original baseline, proving the backwards reading was dropped.
        assert_eq!(usage.avg_mcpu, 500);
    }

    /// The cgroup counter going backwards (a recreated cgroup) must not be
    /// read as a huge negative rate. `observe` re-baselines on it instead, and
    /// a peak already measured before the reset is kept — the reset does not
    /// undo CPU the container genuinely used earlier in the run.
    #[test]
    fn a_backwards_counter_rebaselines_instead_of_going_negative() {
        let mut tally = CpuTally::default();
        tally.observe(0, stat(0));
        tally.observe(1_000_000, stat(2_000_000)); // 2000 mcpu, pre-reset
        tally.observe(2_000_000, stat(1_000_000)); // went backwards: rebaseline
        tally.observe(3_000_000, stat(1_100_000)); // 100 mcpu, post-reset

        let usage = tally.finish().expect("two readings survive the reset");
        assert_eq!(
            usage.avg_mcpu, 100,
            "averaged only over the post-reset span"
        );
        assert_eq!(
            usage.peak_mcpu, 2000,
            "a peak measured before the reset must not be forgotten"
        );
    }

    /// A reset that leaves only one reading before the run ends has no span to
    /// divide across — `finish` must say so rather than panic on a zero-width
    /// interval.
    #[test]
    fn a_rebaseline_with_nothing_after_it_yields_none() {
        let mut tally = CpuTally::default();
        tally.observe(0, stat(0));
        tally.observe(1_000_000, stat(500_000));
        tally.observe(2_000_000, stat(100)); // backwards: rebaseline, then nothing
        assert_eq!(tally.finish(), None);
    }

    /// `nr_throttled`/`nr_periods` are cumulative, so the ratio must come from
    /// the last reading alone — not the first (which would over-report a
    /// container that started rough and settled down) and not a sum across
    /// readings (which would double-count).
    #[test]
    fn throttled_ratio_comes_from_the_last_reading_only() {
        let mut tally = CpuTally::default();
        tally.observe(
            0,
            CpuStat {
                usage_usec: 0,
                nr_periods: Some(100),
                nr_throttled: Some(90),
                throttled_usec: None,
            },
        );
        tally.observe(
            1_000_000,
            CpuStat {
                usage_usec: 500_000,
                nr_periods: Some(120),
                nr_throttled: Some(6),
                throttled_usec: None,
            },
        );
        let usage = tally.finish().expect("two readings");
        assert_eq!(usage.throttled_ratio, 0.05);
    }

    #[test]
    fn throttled_ratio_is_zero_without_quota_counters() {
        // No `nr_periods`/`nr_throttled` at all (a cgroup with no quota lines).
        let mut tally = CpuTally::default();
        tally.observe(0, stat(0));
        tally.observe(1_000_000, stat(500_000));
        assert_eq!(tally.finish().unwrap().throttled_ratio, 0.0);

        // Present but `nr_periods == 0` — guarded rather than dividing 0/0.
        let mut tally = CpuTally::default();
        tally.observe(
            0,
            CpuStat {
                usage_usec: 0,
                nr_periods: Some(0),
                nr_throttled: Some(0),
                throttled_usec: None,
            },
        );
        tally.observe(
            1_000_000,
            CpuStat {
                usage_usec: 100,
                nr_periods: Some(0),
                nr_throttled: Some(0),
                throttled_usec: None,
            },
        );
        assert_eq!(tally.finish().unwrap().throttled_ratio, 0.0);
    }

    /// The whole point of timestamping off `/proc/uptime` text instead of the
    /// caller's own clock: these `observe` calls run in microseconds of real
    /// wall-clock time, yet the gap encoded in their timestamps is five
    /// simulated minutes — and the computed rate must reflect *that* gap, not
    /// how fast the test itself ran.
    #[test]
    fn the_rate_comes_from_the_encoded_timestamps_not_from_real_elapsed_time() {
        let five_min_us: u64 = 5 * 60 * 1_000_000;
        let mut tally = CpuTally::default();
        tally.observe(0, stat(0));
        // Usage grew by exactly the wall gap: one full core, on paper — a
        // figure this test's actual (microsecond-scale) run time could never
        // produce if the tally were reading a real clock instead.
        tally.observe(five_min_us, stat(five_min_us));
        let usage = tally.finish().expect("two readings");
        assert_eq!(usage.avg_mcpu, 1000);
    }
}
