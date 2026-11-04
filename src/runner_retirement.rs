//! Warning before GitHub retires the runner a profile's image bakes (#64).
//!
//! A Docker profile runs the operator's image, and that image bakes one fixed
//! `actions/runner` release. GitHub retires old releases on a date it announces,
//! and from that date a runner on one can no longer take a job: it registers,
//! never connects, and is swept, for every job, until the image is rebuilt. On
//! one watched host that took every Docker job down at once.
//!
//! GitHub's only warning was an annotation on each job such a runner ran — "Runner
//! Version X will no longer be able to run jobs on <date>, please update your
//! runner version." — weeks ahead, on jobs that otherwise passed. faber read none
//! of them. Now it reads them on jobs its own runners ran, and turns them into a
//! per-profile warning on the dashboard, the tray, `/api/state` and the log.
//!
//! **A warning, never a stop.** Nothing here touches admission, health or the
//! image. A host whose runners are current sees no change at all, and one whose
//! runners are not keeps running jobs exactly as before until GitHub itself stops
//! them; the warning is only how the operator hears of it in time to rebuild.
//!
//! **Sampled, cheaply.** At most one finished job per profile per
//! [`SAMPLE_EVERY`], two API calls each. (The retry sweep used to hand failed
//! jobs' annotations here for free; since ADR-0003's amendment of 2026-10-18 it
//! reads none, so sampling is the only source.)
//!
//! **The text is GitHub's, so it is untrusted.** The parser is strict and pure: a
//! bounded message, an exact sentence shape, a validated version and a real
//! calendar date. Nothing of the raw annotation reaches a log or a page; only the
//! fields this parses out of it, each already checked.
//!
//! This module is the pure state; the fetches live in [`crate::reconcile`].

use std::collections::BTreeMap;
use std::time::Duration;

use crate::retry::parse_rfc3339_to_ms;

/// The longest annotation message the parser reads. GitHub's sentence is about a
/// hundred characters; anything far past that is not it, and is not read.
pub const MAX_ANNOTATION_LEN: usize = 512;

/// How often a profile's finished jobs are sampled for the annotation, when no
/// failed job has brought one in for free. GitHub announces a retirement weeks
/// ahead, so an hour is prompt and costs at most two calls per profile per hour.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(60 * 60);

/// At or below this many days before the cutoff the warning escalates.
pub const URGENT_DAYS: i64 = 3;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// A calendar date, as GitHub's annotation names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl Date {
    /// `2026-09-24`.
    pub fn iso(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// The first instant of this day, UTC, in epoch ms.
    fn start_ms(&self) -> Option<u64> {
        parse_rfc3339_to_ms(&format!("{}T00:00:00Z", self.iso()))
    }
}

/// What GitHub's deprecation annotation says: which runner release, and from
/// when it stops running jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deprecation {
    /// The runner release, `major.minor.patch`.
    pub version: String,
    pub cutoff: Date,
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// A runner release as GitHub names one: three dot-separated numbers of at most
/// six digits each. Shared with the tools-disk cache ([`crate::vm::tools_disk`]),
/// which builds a disk for the version a job pins.
pub(crate) fn runner_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()))
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Parse GitHub's runner-deprecation annotation:
///
/// `Runner Version 2.335.1 will no longer be able to run jobs on September 24,
/// 2026, please update your runner version.`
///
/// Strict: the message must start with that sentence, word for word, with a
/// `major.minor.patch` version and a real date in a plausible year. Whatever
/// follows the year is ignored, so a reworded closing clause still parses. Any
/// other shape is `None`, which reads as "no warning" — the direction that
/// changes nothing on the host.
pub fn parse_deprecation(msg: &str) -> Option<Deprecation> {
    if msg.len() > MAX_ANNOTATION_LEN {
        return None;
    }
    let rest = msg.trim_start().strip_prefix("Runner Version ")?;
    let (version, rest) = rest.split_once(' ')?;
    let version = version.strip_prefix('v').unwrap_or(version);
    if !runner_version(version) {
        return None;
    }
    let rest = rest.strip_prefix("will no longer be able to run jobs on ")?;
    let (month, rest) = rest.split_once(' ')?;
    let month = MONTHS.iter().position(|m| *m == month)? as u8 + 1;
    let (day, rest) = rest.split_once(", ")?;
    if day.is_empty() || day.len() > 2 || !day.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let day: u8 = day.parse().ok()?;
    let year = rest.get(..4)?;
    if !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // The year must end there: `20261` is not a year followed by text.
    if rest[4..].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let year: u16 = year.parse().ok()?;
    if !(2000..=2999).contains(&year) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(Deprecation {
        version: version.to_string(),
        cutoff: Date { year, month, day },
    })
}

/// The deprecation among a job's annotation messages, if any. Should a job ever
/// carry two, the earlier cutoff is the one that matters.
pub fn find_deprecation(messages: &[&str]) -> Option<Deprecation> {
    messages
        .iter()
        .filter_map(|m| parse_deprecation(m))
        .min_by_key(|d| d.cutoff)
}

/// Whole days from today (UTC) to the cutoff: `0` on the cutoff day itself,
/// negative once it has passed.
pub fn days_remaining(cutoff: Date, now_ms: u64) -> i64 {
    let Some(cutoff_ms) = cutoff.start_ms() else {
        return 0;
    };
    let today = (now_ms / DAY_MS) as i64;
    (cutoff_ms / DAY_MS) as i64 - today
}

/// The warning for one profile, in the words the log and the dashboard both use.
/// From [`URGENT_DAYS`] out it stops being advice and says what will break.
pub fn warning_text(profile: &str, image: Option<&str>, d: &Deprecation, now_ms: u64) -> String {
    let days = days_remaining(d.cutoff, now_ms);
    let date = d.cutoff.iso();
    let image = image.map(|i| format!(" (image {i})")).unwrap_or_default();
    let v = &d.version;
    let fix = "rebuild the image with a newer RUNNER_VERSION, point the profile at the new \
               tag, then drain and restart faber";
    let plural = |n: i64| if n == 1 { "" } else { "s" };
    match days {
        n if n > URGENT_DAYS => format!(
            "profile {profile}{image} runs actions/runner {v}, which GitHub retires on {date} \
             ({n} days from now) — {fix}"
        ),
        n if n > 0 => format!(
            "URGENT: profile {profile}{image} runs actions/runner {v}, which stops running jobs \
             on {date}, in {n} day{} — every job on this profile fails from then until you {fix}",
            plural(n)
        ),
        0 => format!(
            "URGENT: profile {profile}{image} runs actions/runner {v}, which GitHub retires \
             today ({date}) — every job on this profile fails until you {fix}"
        ),
        n => format!(
            "URGENT: profile {profile}{image} runs actions/runner {v}, which GitHub retired on \
             {date} ({} day{} ago) — its jobs cannot run until you {fix}",
            -n,
            plural(-n)
        ),
    }
}

/// A finished job worth sampling: one of this host's runners ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub repo: String,
    pub job_id: u64,
    /// The runner faber minted, which the job must name for the sample to count.
    pub runner: String,
}

/// A profile's standing warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub deprecation: Deprecation,
    /// The job whose annotation it came from, and when faber read it.
    pub job_id: u64,
    pub seen_ms: u64,
}

/// What one observation changed, so the caller logs each edge once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A warning appeared, or now names another version or date.
    Raised,
    /// A job ran without the annotation, so the profile's runner is current.
    Cleared,
    Unchanged,
}

#[derive(Debug, Default)]
struct Entry {
    last_sample_ms: Option<u64>,
    pending: Option<Candidate>,
    warning: Option<Warning>,
    /// The runner release GitHub last named for a job on this profile.
    runner_version: Option<String>,
    /// The UTC day the warning was last logged, for once-a-day logging.
    logged_day: Option<u64>,
}

/// The per-profile state, keyed by profile name.
#[derive(Debug, Default)]
pub struct Retirements {
    profiles: BTreeMap<String, Entry>,
}

impl Retirements {
    /// A job one of this host's runners ran for `profile` has finished. Only the
    /// latest is kept: one is all a sample needs.
    pub fn finished(&mut self, profile: &str, c: Candidate) {
        self.profiles
            .entry(profile.to_string())
            .or_default()
            .pending = Some(c);
    }

    /// The candidates to sample now: one per profile that has one and was not
    /// sampled within [`SAMPLE_EVERY`]. Taking one starts the profile's clock,
    /// whether or not the fetch that follows succeeds, so a failing API is
    /// asked no more often than a working one.
    pub fn due(&mut self, now_ms: u64) -> Vec<(String, Candidate)> {
        let every = SAMPLE_EVERY.as_millis() as u64;
        let mut out = Vec::new();
        for (profile, e) in &mut self.profiles {
            let due = e
                .last_sample_ms
                .is_none_or(|t| now_ms.saturating_sub(t) >= every);
            if !due {
                continue;
            }
            if let Some(c) = e.pending.take() {
                e.last_sample_ms = Some(now_ms);
                out.push((profile.clone(), c));
            }
        }
        out
    }

    /// Record what one job's annotations said about `profile`'s runner.
    ///
    /// `ran` is whether the job reported any step. Only a job that ran can clear
    /// a warning: a runner that died before its first step leaves no annotations
    /// at all, and that silence says nothing about its version.
    pub fn observed(
        &mut self,
        profile: &str,
        job_id: u64,
        found: Option<Deprecation>,
        ran: bool,
        now_ms: u64,
    ) -> Change {
        let e = self.profiles.entry(profile.to_string()).or_default();
        // An annotation read for free counts as this profile's sample.
        e.last_sample_ms = Some(now_ms);
        match found {
            Some(d) => {
                e.runner_version = Some(d.version.clone());
                let new = e.warning.as_ref().is_none_or(|w| w.deprecation != d);
                e.warning = Some(Warning {
                    deprecation: d,
                    job_id,
                    seen_ms: now_ms,
                });
                if new {
                    // A new or changed warning is logged at once, not tomorrow.
                    e.logged_day = None;
                    Change::Raised
                } else {
                    Change::Unchanged
                }
            }
            None if ran && e.warning.is_some() => {
                e.warning = None;
                e.logged_day = None;
                Change::Cleared
            }
            None => Change::Unchanged,
        }
    }

    /// The profiles whose warning has not been logged yet today (UTC), marking
    /// each as logged.
    pub fn log_due(&mut self, now_ms: u64) -> Vec<(String, Deprecation)> {
        let today = now_ms / DAY_MS;
        let mut out = Vec::new();
        for (profile, e) in &mut self.profiles {
            let Some(w) = &e.warning else { continue };
            if e.logged_day == Some(today) {
                continue;
            }
            e.logged_day = Some(today);
            out.push((profile.clone(), w.deprecation.clone()));
        }
        out
    }

    /// Every standing warning, by profile name.
    pub fn warnings(&self) -> Vec<(String, Warning)> {
        self.profiles
            .iter()
            .filter_map(|(p, e)| e.warning.clone().map(|w| (p.clone(), w)))
            .collect()
    }

    /// The runner release GitHub last named for a job on `profile`.
    pub fn runner_version(&self, profile: &str) -> Option<&str> {
        self.profiles
            .get(profile)
            .and_then(|e| e.runner_version.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GitHub's annotation, word for word, as it appeared on a watched repo's jobs.
    const ANN: &str = "Runner Version 2.335.1 will no longer be able to run jobs on \
                       September 24, 2026, please update your runner version.";
    const SEPT_24: Date = Date {
        year: 2026,
        month: 9,
        day: 24,
    };
    const IMG: &str = "ghcr.io/owner/my-runner:2.335.1";

    fn at(date: &str) -> u64 {
        parse_rfc3339_to_ms(date).unwrap()
    }

    fn dep() -> Deprecation {
        parse_deprecation(ANN).unwrap()
    }

    fn cand(job_id: u64) -> Candidate {
        Candidate {
            repo: "owner/demo".into(),
            job_id,
            runner: format!("faber-{job_id}-1-abc"),
        }
    }

    // --- parser ----------------------------------------------------------------

    #[test]
    fn parses_githubs_annotation() {
        assert_eq!(
            parse_deprecation(ANN),
            Some(Deprecation {
                version: "2.335.1".into(),
                cutoff: SEPT_24,
            })
        );
    }

    #[test]
    fn tolerates_a_reworded_tail_and_a_v_prefix() {
        let d = parse_deprecation(
            "Runner Version v2.335.1 will no longer be able to run jobs on March 1, 2027.",
        )
        .unwrap();
        assert_eq!(d.version, "2.335.1");
        assert_eq!(d.cutoff.iso(), "2027-03-01");
    }

    #[test]
    fn rejects_every_other_shape() {
        for bad in [
            "",
            "Process completed with exit code 1.",
            "The self-hosted runner lost communication with the server.",
            // Not at the start of the message.
            "Note: Runner Version 2.335.1 will no longer be able to run jobs on September 24, 2026.",
            // Case and wording are GitHub's exact sentence.
            "runner version 2.335.1 will no longer be able to run jobs on September 24, 2026.",
            "Runner Version 2.335.1 will stop running jobs on September 24, 2026.",
            // Versions.
            "Runner Version 2.335 will no longer be able to run jobs on September 24, 2026.",
            "Runner Version 2.335.1.4 will no longer be able to run jobs on September 24, 2026.",
            "Runner Version 2.33x.1 will no longer be able to run jobs on September 24, 2026.",
            "Runner Version 2..1 will no longer be able to run jobs on September 24, 2026.",
            "Runner Version 1234567.0.0 will no longer be able to run jobs on September 24, 2026.",
            // Dates.
            "Runner Version 2.335.1 will no longer be able to run jobs on Sept 24, 2026.",
            "Runner Version 2.335.1 will no longer be able to run jobs on september 24, 2026.",
            "Runner Version 2.335.1 will no longer be able to run jobs on September 31, 2026.",
            "Runner Version 2.335.1 will no longer be able to run jobs on February 29, 2027.",
            "Runner Version 2.335.1 will no longer be able to run jobs on September 0, 2026.",
            "Runner Version 2.335.1 will no longer be able to run jobs on September 024, 2026.",
            "Runner Version 2.335.1 will no longer be able to run jobs on September 24 2026.",
            "Runner Version 2.335.1 will no longer be able to run jobs on September 24, 20261.",
            "Runner Version 2.335.1 will no longer be able to run jobs on September 24, 1999.",
            "Runner Version 2.335.1 will no longer be able to run jobs on September 24, 26.",
        ] {
            assert_eq!(parse_deprecation(bad), None, "{bad}");
        }
        // A year with nothing after it is still a year.
        assert!(parse_deprecation(
            "Runner Version 2.335.1 will no longer be able to run jobs on September 24, 2026"
        )
        .is_some());
    }

    #[test]
    fn a_leap_day_is_a_real_date() {
        let d = parse_deprecation(
            "Runner Version 2.400.0 will no longer be able to run jobs on February 29, 2028, \
             please update your runner version.",
        )
        .unwrap();
        assert_eq!(d.cutoff.iso(), "2028-02-29");
    }

    #[test]
    fn an_oversized_message_is_not_read() {
        let long = format!("{ANN}{}", " ".repeat(MAX_ANNOTATION_LEN));
        assert_eq!(parse_deprecation(&long), None);
    }

    #[test]
    fn the_earliest_cutoff_among_a_jobs_annotations_wins() {
        let later = "Runner Version 2.336.0 will no longer be able to run jobs on \
                     December 1, 2026, please update your runner version.";
        let found = find_deprecation(&["Process completed with exit code 1.", later, ANN]);
        assert_eq!(found.unwrap().cutoff, SEPT_24);
        assert_eq!(
            find_deprecation(&["Process completed with exit code 1."]),
            None
        );
        assert_eq!(find_deprecation(&[]), None);
    }

    // --- days and wording ------------------------------------------------------

    #[test]
    fn days_remaining_counts_utc_calendar_days() {
        assert_eq!(days_remaining(SEPT_24, at("2026-09-01T23:59:59Z")), 23);
        assert_eq!(days_remaining(SEPT_24, at("2026-09-21T00:00:00Z")), 3);
        assert_eq!(days_remaining(SEPT_24, at("2026-09-23T23:59:59Z")), 1);
        assert_eq!(days_remaining(SEPT_24, at("2026-09-24T12:00:00Z")), 0);
        assert_eq!(days_remaining(SEPT_24, at("2026-09-26T00:00:00Z")), -2);
    }

    #[test]
    fn the_warning_names_profile_image_version_date_and_days() {
        let t = warning_text("docker", Some(IMG), &dep(), at("2026-09-01T10:00:00Z"));
        for part in [
            "docker",
            IMG,
            "2.335.1",
            "2026-09-24",
            "23 days",
            "RUNNER_VERSION",
        ] {
            assert!(t.contains(part), "{part} missing from: {t}");
        }
        assert!(!t.contains("URGENT"), "{t}");
    }

    #[test]
    fn the_warning_escalates_from_three_days_out() {
        let d = dep();
        let four = warning_text("docker", Some(IMG), &d, at("2026-09-20T10:00:00Z"));
        assert!(!four.contains("URGENT"), "{four}");
        let three = warning_text("docker", Some(IMG), &d, at("2026-09-21T10:00:00Z"));
        assert!(three.starts_with("URGENT"), "{three}");
        assert!(three.contains("in 3 days"), "{three}");
        let one = warning_text("docker", None, &d, at("2026-09-23T10:00:00Z"));
        assert!(one.contains("in 1 day "), "{one}");
        let today = warning_text("docker", None, &d, at("2026-09-24T10:00:00Z"));
        assert!(today.contains("today (2026-09-24)"), "{today}");
        let past = warning_text("docker", None, &d, at("2026-09-25T10:00:00Z"));
        assert!(past.contains("retired on 2026-09-24 (1 day ago)"), "{past}");
    }

    // --- per-profile state -----------------------------------------------------

    #[test]
    fn a_profile_is_sampled_at_most_once_per_interval() {
        let every = SAMPLE_EVERY.as_millis() as u64;
        let mut r = Retirements::default();
        assert!(
            r.due(1_000).is_empty(),
            "nothing finished, nothing to sample"
        );
        r.finished("docker", cand(1));
        r.finished("docker", cand(2));
        assert_eq!(r.due(1_000), vec![("docker".to_string(), cand(2))]);
        r.finished("docker", cand(3));
        assert!(r.due(1_000 + every - 1).is_empty(), "inside the interval");
        assert_eq!(r.due(1_000 + every), vec![("docker".to_string(), cand(3))]);
        assert!(r.due(1_000 + 3 * every).is_empty(), "taken once");
    }

    #[test]
    fn an_annotation_read_for_free_counts_as_the_sample() {
        let mut r = Retirements::default();
        r.observed("docker", 7, None, true, 1_000);
        r.finished("docker", cand(8));
        assert!(r.due(2_000).is_empty());
    }

    #[test]
    fn a_warning_is_raised_once_and_cleared_by_a_job_that_ran_clean() {
        let mut r = Retirements::default();
        assert_eq!(
            r.observed("docker", 1, Some(dep()), true, 1_000),
            Change::Raised
        );
        assert_eq!(
            r.observed("docker", 2, Some(dep()), true, 2_000),
            Change::Unchanged
        );
        assert_eq!(r.runner_version("docker"), Some("2.335.1"));
        let w = r.warnings();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].1.job_id, w[0].1.seen_ms), (2, 2_000));
        // A runner that died before its first step says nothing either way.
        assert_eq!(
            r.observed("docker", 3, None, false, 3_000),
            Change::Unchanged
        );
        assert_eq!(r.warnings().len(), 1);
        assert_eq!(r.observed("docker", 4, None, true, 4_000), Change::Cleared);
        assert!(r.warnings().is_empty());
        assert_eq!(
            r.observed("docker", 5, None, true, 5_000),
            Change::Unchanged
        );
    }

    #[test]
    fn a_changed_deprecation_is_raised_again() {
        let mut r = Retirements::default();
        r.observed("docker", 1, Some(dep()), true, 1_000);
        let next = parse_deprecation(
            "Runner Version 2.337.0 will no longer be able to run jobs on March 1, 2027.",
        );
        assert_eq!(r.observed("docker", 2, next, true, 2_000), Change::Raised);
        assert_eq!(r.runner_version("docker"), Some("2.337.0"));
    }

    #[test]
    fn a_warning_is_logged_once_per_utc_day_and_at_once_when_new() {
        let mut r = Retirements::default();
        let day1 = at("2026-09-01T10:00:00Z");
        assert!(r.log_due(day1).is_empty());
        r.observed("docker", 1, Some(dep()), true, day1);
        assert_eq!(r.log_due(day1).len(), 1);
        assert!(r.log_due(day1 + 60_000).is_empty(), "same day");
        assert_eq!(r.log_due(at("2026-09-02T00:00:01Z")).len(), 1, "next day");
        // A changed deprecation does not wait for tomorrow.
        let next = parse_deprecation(
            "Runner Version 2.337.0 will no longer be able to run jobs on March 1, 2027.",
        );
        r.observed("docker", 2, next, true, at("2026-09-02T00:00:02Z"));
        assert_eq!(r.log_due(at("2026-09-02T00:00:03Z")).len(), 1);
    }

    #[test]
    fn profiles_are_independent() {
        let mut r = Retirements::default();
        r.observed("docker", 1, Some(dep()), true, 1_000);
        r.observed("other", 2, None, true, 1_000);
        assert_eq!(r.warnings().len(), 1);
        assert_eq!(r.warnings()[0].0, "docker");
        assert_eq!(r.runner_version("other"), None);
    }
}
