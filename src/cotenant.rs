//! The co-tenant reserve, measured rather than assumed — ADR-0021 §3.
//!
//! Gate 1 holds back `reserve_mib` for everything in the Docker VM that faber did
//! not start: Docker Desktop's own overhead, and the co-tenant stacks sharing the
//! VM. That figure was chosen once, when the co-tenants used a little under it
//! at idle, and nothing checked it afterwards. By the time ADR-0021 measured
//! them they held about three times as much, so Gate 1 was planning with twice
//! the reserve's worth of memory the VM did not have.
//!
//! So once the machine's share is declared, Gate 1 holds back the larger of
//! `reserve_mib` and the co-tenants' **peak over the last 24 hours**. The
//! co-tenant figure is the VM's unavailable memory (`MemTotal − MemAvailable`)
//! less the working set of faber's own containers, which faber finds by its
//! `org.faber.owned` label and never by name. That covers everything else,
//! whoever started it, with no list of names to keep. It is a peak, so a stack
//! that happens to be idle when sampled is not planned at its idle size.
//!
//! The measurement only ever raises the reserve: `reserve_mib` stays the floor
//! and the operator's override. That is also what makes the state safe to lose.
//! A missing or unreadable window falls back to `reserve_mib`, which is the
//! behaviour from before §3, never to an optimistic zero. It is kept on disk as
//! well, so a restart keeps the day's peak instead of relearning it from scratch.
//!
//! The window is hour buckets, each keeping its own peak: at most 25 numbers,
//! however long faber runs. A bucket counts while any of its hour is inside the
//! last 24, so the window is between 24 and 25 hours long, never shorter.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::untrusted::MAX_MEM_MIB;

/// How far back the peak looks (ADR-0021 §3): 24 hours.
pub const WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// The width of one bucket of the window: an hour.
pub const BUCKET_MS: u64 = 60 * 60 * 1000;

/// The most buckets the window can hold: 24 whole hours and the one under way.
const MAX_BUCKETS: usize = (WINDOW_MS / BUCKET_MS) as usize + 1;

/// The file the window is kept in, under faber's state directory.
pub const STATE_FILE: &str = "cotenant.json";

/// What the VM uses beyond faber's own containers (MiB), from one sample:
/// `total − available − faber`.
///
/// `None` for a sample that is no evidence: no `MemTotal` yet, or a
/// `MemAvailable` of zero or above the total. A probe that reads no available
/// memory at all is clamped to zero on purpose (Gate 2 must hold on it), but
/// here it would read as the whole VM being taken by co-tenants, and the peak
/// would keep that for a day. faber's working set is subtracted saturating: a
/// daemon that overstates it can bring the figure down to zero, and so the
/// reserve back to `reserve_mib`, which is where it stood before §3.
pub fn cotenant_mib(total_mib: u64, available_mib: u64, faber_mib: u64) -> Option<u64> {
    if total_mib == 0 || available_mib == 0 || available_mib > total_mib {
        return None;
    }
    Some((total_mib - available_mib).saturating_sub(faber_mib))
}

/// The reserve Gate 1 holds back (MiB): `reserve_mib`, or the co-tenants' peak
/// if that is larger and the reserve is measured at all.
pub fn reserve_in_force(floor_mib: u64, peak_mib: Option<u64>, measured: bool) -> u64 {
    match peak_mib {
        Some(peak) if measured => floor_mib.max(peak),
        _ => floor_mib,
    }
}

/// One hour of the window: when it starts and the largest sample taken in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Bucket {
    start_ms: u64,
    peak_mib: u64,
}

/// The co-tenants' peak over the last 24 hours, in hour buckets, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoTenantPeak {
    buckets: Vec<Bucket>,
}

impl CoTenantPeak {
    /// Add one sample of `mib` taken at `at_ms`, and forget the hours that have
    /// left the window. Returns whether anything changed, so the caller writes
    /// the file only when there is something new in it.
    ///
    /// A sample dated before the newest bucket (the clock stepped back) is folded
    /// into that bucket rather than an older one. It then lasts as long as the
    /// newest bucket does, which is the longer of the two. A newest bucket dated
    /// more than a window ahead (the clock stepped back further than that) is
    /// forgotten first, so a sample is never folded into it.
    pub fn observe(&mut self, at_ms: u64, mib: u64) -> bool {
        let mib = mib.min(MAX_MEM_MIB);
        let start = at_ms - at_ms % BUCKET_MS;
        let before = self.buckets.clone();
        self.prune(at_ms);
        match self.buckets.last_mut() {
            Some(last) if start <= last.start_ms => last.peak_mib = last.peak_mib.max(mib),
            _ => self.buckets.push(Bucket {
                start_ms: start,
                peak_mib: mib,
            }),
        }
        self.prune(at_ms);
        self.buckets != before
    }

    /// Forget the buckets with none of their hour left inside the window at
    /// `now_ms`, and keep at most [`MAX_BUCKETS`] of the rest.
    fn prune(&mut self, now_ms: u64) {
        self.buckets.retain(|b| in_window(b, now_ms));
        let extra = self.buckets.len().saturating_sub(MAX_BUCKETS);
        self.buckets.drain(..extra);
    }

    /// The peak over the window at `now_ms`, or `None` with no sample in it.
    pub fn peak(&self, now_ms: u64) -> Option<u64> {
        self.buckets
            .iter()
            .filter(|b| in_window(b, now_ms))
            .map(|b| b.peak_mib)
            .max()
    }

    /// How much of the window the samples cover at `now_ms`: from the start of
    /// the oldest bucket still in it, at most the whole 24 hours. Shown beside the
    /// peak, so a peak learned over an hour does not read like one learned over a
    /// day.
    pub fn history_ms(&self, now_ms: u64) -> u64 {
        self.buckets
            .iter()
            .find(|b| in_window(b, now_ms))
            .map_or(0, |b| now_ms.saturating_sub(b.start_ms).min(WINDOW_MS))
    }

    /// The window as written to [`STATE_FILE`].
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The window read back from [`STATE_FILE`] at `now_ms`.
    ///
    /// faber wrote the file, but it is read the way anything on disk is: sorted,
    /// held to [`MAX_MEM_MIB`], at most [`MAX_BUCKETS`] long, and pruned to the
    /// window. A file that does not parse is an empty window, with a warning. That
    /// falls back to `reserve_mib` until the peak is relearned, which is the
    /// behaviour from before §3.
    pub fn from_json(text: &str, now_ms: u64) -> Self {
        let mut peak: Self = match serde_json::from_str(text) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "co-tenant reserve: {STATE_FILE} is not readable ({e}) — starting from \
                     reserve_mib and relearning the peak (ADR-0021 §3)"
                );
                return Self::default();
            }
        };
        peak.buckets.sort_by_key(|b| b.start_ms);
        for b in &mut peak.buckets {
            b.peak_mib = b.peak_mib.min(MAX_MEM_MIB);
        }
        peak.prune(now_ms);
        peak
    }

    /// Read the window from `path`, or an empty one when there is no file.
    pub fn load(path: &Path, now_ms: u64) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text, now_ms),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                warn!(
                    "co-tenant reserve: reading {}: {e} — starting from reserve_mib (ADR-0021 §3)",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write the window to `path`, through a temporary file so a crash mid-write
    /// leaves the last good one. Best-effort: a write that fails costs a restart
    /// the day's peak, never a job.
    pub fn save(&self, path: &Path) {
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, self.to_json())?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            warn!("co-tenant reserve: writing {}: {e}", path.display());
        }
    }
}

/// Whether any of `b`'s hour is inside the 24 hours before `now_ms`. A bucket
/// dated after `now_ms` (the clock stepped back) is still in it, up to a window
/// ahead. One dated further ahead would otherwise stay in the window until the
/// clock caught up with it, and every later sample would fold into it, so its
/// peak could only rise: a clock that ran a month fast and was corrected would
/// hold a stale reserve for that month, across restarts.
fn in_window(b: &Bucket, now_ms: u64) -> bool {
    b.start_ms <= now_ms.saturating_add(WINDOW_MS)
        && now_ms.saturating_sub(b.start_ms) < WINDOW_MS + BUCKET_MS
}

/// The co-tenant reserve (ADR-0021 §3), as `/api/state`'s `budget.cotenant`.
#[derive(Debug, Clone, Serialize)]
pub struct CoTenantView {
    /// `"measured"` once `[host] keep_memory_gib` is above 0: Gate 1 holds back
    /// the larger of `reserve_mib` and the peak. `"floor"` otherwise:
    /// `reserve_mib` alone, as before §3. Measured in both.
    pub mode: &'static str,
    /// `reserve_mib`: the floor, and the operator's override.
    pub floor_mib: u64,
    /// The latest sample of what the VM uses beyond faber's containers, and how
    /// old it is in seconds.
    pub latest_mib: Option<u64>,
    pub latest_age_s: Option<u64>,
    /// The largest sample of the last 24 hours.
    pub peak_mib: Option<u64>,
    /// How much of those 24 hours the samples cover, in seconds.
    pub history_s: u64,
    /// What Gate 1 holds back now.
    pub reserve_mib: u64,
    /// Whether the peak is what sets it, rather than `reserve_mib`.
    pub peak_binds: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = BUCKET_MS;
    const GIB: u64 = 1024;
    /// An hour boundary, so bucket arithmetic reads plainly.
    const T0: u64 = 1_000 * H;

    #[test]
    fn the_cotenant_figure_is_what_the_vm_uses_beyond_faber() {
        // For example, a 24 GiB VM with 12 GiB available, faber's jobs holding 4
        // of the 12 in use — 8 GiB of co-tenants.
        assert_eq!(cotenant_mib(24 * GIB, 12 * GIB, 4 * GIB), Some(8 * GIB));
        // faber overstated past what is in use reads as no co-tenants, not as a
        // negative: the floor then holds, as before §3.
        assert_eq!(cotenant_mib(24 * GIB, 12 * GIB, 40 * GIB), Some(0));
        // No evidence: unprobed, a clamped-to-zero reading, an impossible one.
        assert_eq!(cotenant_mib(0, 12 * GIB, 0), None);
        assert_eq!(cotenant_mib(24 * GIB, 0, 0), None);
        assert_eq!(cotenant_mib(24 * GIB, 25 * GIB, 0), None);
    }

    #[test]
    fn the_reserve_is_the_larger_of_the_floor_and_the_peak_once_measured() {
        assert_eq!(reserve_in_force(5 * GIB, Some(8 * GIB), true), 8 * GIB);
        // The measurement only ever raises the reserve.
        assert_eq!(reserve_in_force(5 * GIB, Some(3 * GIB), true), 5 * GIB);
        // Nothing learned yet: the floor, never an optimistic zero.
        assert_eq!(reserve_in_force(5 * GIB, None, true), 5 * GIB);
        // Not measured (no share declared): the floor alone, as before §3.
        assert_eq!(reserve_in_force(5 * GIB, Some(8 * GIB), false), 5 * GIB);
    }

    #[test]
    fn the_peak_holds_for_a_day_and_then_ages_out() {
        let mut p = CoTenantPeak::default();
        assert_eq!(p.peak(T0), None);
        // An evening spike, then an idle night.
        assert!(p.observe(T0 + 10, 8 * GIB));
        for hour in 1..20 {
            p.observe(T0 + hour * H, 4 * GIB);
        }
        assert_eq!(
            p.peak(T0 + 19 * H),
            Some(8 * GIB),
            "idle when sampled is not idle"
        );
        // Still in the window at the end of the 24th hour…
        assert_eq!(p.peak(T0 + 24 * H + H - 1), Some(8 * GIB));
        // …and gone once none of its hour is.
        assert_eq!(p.peak(T0 + 25 * H), Some(4 * GIB));
        p.observe(T0 + 25 * H, 4 * GIB);
        assert!(p.buckets.len() <= MAX_BUCKETS);
        assert_eq!(p.peak(T0 + 25 * H), Some(4 * GIB));
    }

    #[test]
    fn one_hour_keeps_one_number_and_reports_a_change_only_when_it_rises() {
        let mut p = CoTenantPeak::default();
        assert!(p.observe(T0, 4 * GIB));
        assert!(
            !p.observe(T0 + 60_000, 3 * GIB),
            "a lower sample changes nothing"
        );
        assert!(p.observe(T0 + 120_000, 5 * GIB));
        assert_eq!(p.buckets.len(), 1);
        // A clock that steps back folds into the newest bucket, never an older one.
        assert!(p.observe(T0 + 2 * H, GIB));
        p.observe(T0 + H, 9 * GIB);
        assert_eq!(p.buckets.len(), 2);
        assert_eq!(p.buckets[1].peak_mib, 9 * GIB);
    }

    #[test]
    fn a_clock_that_stepped_back_more_than_a_day_does_not_pin_the_peak() {
        let mut p = CoTenantPeak::default();
        // Sampled while the clock ran 30 days fast, then corrected.
        p.observe(T0 + 30 * 24 * H, 8 * GIB);
        // Not folded into the far-future bucket: the new sample stands alone.
        assert!(p.observe(T0, 4 * GIB));
        assert_eq!(p.buckets.len(), 1);
        assert_eq!(p.peak(T0), Some(4 * GIB));
        // A small step back still folds into the newest bucket.
        p.observe(T0 + 2 * H, 5 * GIB);
        p.observe(T0 + H, 9 * GIB);
        assert_eq!(p.peak(T0 + H), Some(9 * GIB));
        // And a file written while the clock ran fast restores nothing from then.
        let mut fast = CoTenantPeak::default();
        fast.observe(T0 + 30 * 24 * H, 8 * GIB);
        assert_eq!(CoTenantPeak::from_json(&fast.to_json(), T0).peak(T0), None);
    }

    #[test]
    fn a_week_of_samples_stays_a_day_of_buckets() {
        let mut p = CoTenantPeak::default();
        for minute in 0..7 * 24 * 60 {
            p.observe(T0 + minute * 60_000, minute % 5000);
        }
        assert!(p.buckets.len() <= MAX_BUCKETS, "{}", p.buckets.len());
        assert_eq!(p.history_ms(T0 + 7 * 24 * H), WINDOW_MS);
    }

    #[test]
    fn history_says_how_much_of_the_day_the_peak_has_seen() {
        let mut p = CoTenantPeak::default();
        assert_eq!(p.history_ms(T0), 0);
        p.observe(T0 + 30 * 60_000, GIB);
        assert_eq!(p.history_ms(T0 + 90 * 60_000), 90 * 60_000);
    }

    #[test]
    fn the_window_survives_a_restart_and_a_bad_file_falls_back_to_the_floor() {
        let mut p = CoTenantPeak::default();
        p.observe(T0, 8 * GIB);
        p.observe(T0 + 3 * H, 4 * GIB);
        let restored = CoTenantPeak::from_json(&p.to_json(), T0 + 4 * H);
        assert_eq!(restored, p);
        assert_eq!(restored.peak(T0 + 4 * H), Some(8 * GIB));
        // A file from long ago restores nothing stale.
        assert_eq!(
            CoTenantPeak::from_json(&p.to_json(), T0 + 30 * H).peak(T0 + 30 * H),
            None
        );
        // Unreadable: an empty window, so `reserve_mib` alone until relearned.
        assert_eq!(
            CoTenantPeak::from_json("not json", T0),
            CoTenantPeak::default()
        );
        // Out of order, oversized and too long: sorted, held and trimmed.
        let mut text = String::from(r#"{"buckets":["#);
        for i in (0..40u64).rev() {
            text.push_str(&format!(
                r#"{{"start_ms":{},"peak_mib":{}}},"#,
                T0 + i * H,
                if i == 39 { u64::MAX } else { i }
            ));
        }
        text.pop();
        text.push_str("]}");
        let read = CoTenantPeak::from_json(&text, T0 + 39 * H);
        assert!(read.buckets.len() <= MAX_BUCKETS);
        assert!(read
            .buckets
            .windows(2)
            .all(|w| w[0].start_ms < w[1].start_ms));
        assert_eq!(read.peak(T0 + 39 * H), Some(MAX_MEM_MIB));
    }

    #[test]
    fn the_file_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("faber-cotenant-{}", std::process::id()));
        let path = dir.join(STATE_FILE);
        assert_eq!(CoTenantPeak::load(&path, T0), CoTenantPeak::default());
        let mut p = CoTenantPeak::default();
        p.observe(T0, 7 * GIB);
        p.save(&path);
        assert_eq!(
            CoTenantPeak::load(&path, T0 + H).peak(T0 + H),
            Some(7 * GIB)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
