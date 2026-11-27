//! Speed-up hints: a faster way to run jobs that this host has and leaves off,
//! with what the step it shortens costs here (ADR
//! faber-says-when-a-faster-way-to-run-jobs-is-off).
//!
//! A hint is a fact for the dashboard, not a fault. It changes nothing faber
//! does, blocks no job, and is never delivered outward. The rules that keep it
//! honest:
//!
//! - **A host with nothing to gain gets none.** No docker profile, the
//!   capability already in use, or no docker run in the window: no hint.
//! - **The figure is this host's own.** It is read from the step timings on the
//!   audit log's `outcome` lines (ADR-0056). faber records no run as having used
//!   the capability, so it states what the step costs today and never what the
//!   capability would save.
//! - **Off is one key.** `[host] speed_hints = false` turns every hint off.
//!
//! What is off, and why, is fixed at boot ([`Plan`]): the keys it reads apply on
//! restart. The figures move, so they are recomputed from the audit log in the
//! background and served from memory ([`Hints`]); `/api/state` never waits on
//! the file.

use std::sync::Mutex;

use serde::Serialize;

use crate::audit::Audit;
use crate::config::{BackendKind, FileConfig};
use crate::jobs_view::RunLine;
use crate::mirror_beside_dind;

/// The window the figures cover, in days: the Jobs view's own default.
pub const WINDOW_DAYS: u64 = crate::jobs_view::DEFAULT_WINDOW_DAYS;

/// How old the served figures may get before the next state read starts a
/// recompute. Reading the audit log is a whole-file read (ADR-0056).
pub const REFRESH_MS: u64 = 10 * 60 * 1000;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The name GitHub gives a checkout step that has none of its own: the same
/// prefix [`crate::footprint`] buckets as setup.
const CHECKOUT_ACTION: &str = "Run actions/checkout@";

/// What a run's `outcome` line says of a job that was handed a Docker daemon
/// (`dind`, [`crate::audit::Record`]). The field is a string, `granted` or
/// `denied`, and absent for a run with no container: compare it, never test it
/// for presence.
const DIND_GRANTED: &str = "granted";

/// Why a capability is not in use here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// No docker profile asks for it.
    Off,
    /// Held off beside dind: a docker profile grants dind now.
    DindGranted,
    /// Held off beside dind: none grants it now, but this host's engine has.
    EngineMarked,
}

/// One hint, as `/api/state` carries it under `hints`. The dashboard words it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hint {
    /// The capability: `git_mirror` (ADR-0035).
    pub kind: &'static str,
    pub cause: Cause,
    /// The docker profiles that grant dind, for [`Cause::DindGranted`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<String>,
    /// The window every count and figure below covers.
    pub window_days: u64,
    /// Docker runs in the window, and how many were handed a Docker daemon.
    pub docker_runs: usize,
    pub dind_runs: usize,
    /// When the last run handed a Docker daemon finished, as far back as the
    /// Jobs view reads. Absent when no such run is on record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_dind_ms: Option<u64>,
    /// Runs in the window with a timed checkout step. Absent with none: the
    /// dashboard then says faber has no figure, and states none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs: Option<usize>,
    /// The median checkout time per such run, and their sum.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
}

/// What this boot's config leaves unused. Decided once: `git_mirror`, `dind` and
/// `speed_hints` all apply on restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Why the git mirror is not in use, and the profiles that grant dind.
    /// `None`: it is in use, or this host has no docker profile to use it.
    git_mirror: Option<(Cause, Vec<String>)>,
}

impl Plan {
    /// Decide from the config as written and boot's verdict on the mirrors that
    /// carry faber's token ([`mirror_beside_dind::Verdict`]).
    ///
    /// The mirror counts as in use when any docker profile sets `git_mirror`
    /// and nothing holds it off. Whether a workflow carries the step that
    /// borrows from it (docs/git-mirror.md) is not something faber can see.
    pub fn of(cfg: &FileConfig, beside_dind: &mirror_beside_dind::Verdict) -> Self {
        if !cfg.host.speed_hints {
            return Self::default();
        }
        let docker = || {
            cfg.profiles
                .iter()
                .filter(|p| p.backend == BackendKind::Docker)
        };
        if docker().next().is_none() {
            return Self::default();
        }
        let git_mirror = match &beside_dind.cause {
            Some(mirror_beside_dind::Cause::Granted(names)) => {
                Some((Cause::DindGranted, names.clone()))
            }
            Some(mirror_beside_dind::Cause::Marked) => Some((Cause::EngineMarked, Vec::new())),
            None if docker().any(|p| p.git_mirror) => None,
            None => Some((Cause::Off, Vec::new())),
        };
        Self { git_mirror }
    }

    /// Whether any hint could ever show on this boot.
    pub fn is_empty(&self) -> bool {
        self.git_mirror.is_none()
    }

    /// The hints `runs` support at `now_ms`. `runs` may reach further back than
    /// the window: the last dind grant is looked for in all of them.
    pub fn hints(&self, runs: &[RunLine], now_ms: u64) -> Vec<Hint> {
        self.git_mirror
            .iter()
            .filter_map(|(cause, profiles)| git_mirror_hint(cause, profiles, runs, now_ms))
            .collect()
    }
}

/// Whether GitHub's step `name` is a checkout: the action's own name, or a name
/// a workflow gave it that begins with the word. A workflow that calls its
/// checkout step something else is not counted, so the figure can only be low.
fn is_checkout(name: &str) -> bool {
    name.starts_with(CHECKOUT_ACTION)
        || name
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("checkout"))
}

/// The time `run` spent in checkout steps, or `None` when it has no timed one.
fn checkout_ms(run: &RunLine) -> Option<u64> {
    let steps = run.footprint.steps.as_ref()?;
    let timed: Vec<u64> = steps
        .iter()
        .filter(|s| is_checkout(&s.name))
        .filter_map(|s| s.duration_ms())
        .collect();
    (!timed.is_empty()).then(|| timed.iter().sum())
}

/// The middle value; the lower of the two middle ones for an even count.
fn median(values: &mut [u64]) -> Option<u64> {
    values.sort_unstable();
    values.get(values.len().checked_sub(1)? / 2).copied()
}

fn git_mirror_hint(
    cause: &Cause,
    profiles: &[String],
    runs: &[RunLine],
    now_ms: u64,
) -> Option<Hint> {
    let from_ms = now_ms.saturating_sub(WINDOW_DAYS * DAY_MS);
    // A docker run is one whose line says whether it had a daemon: a macOS VM
    // job, and a claim reclaimed before any runner existed, say neither.
    let docker: Vec<&RunLine> = runs.iter().filter(|r| r.dind.is_some()).collect();
    let granted = |r: &RunLine| r.dind.as_deref() == Some(DIND_GRANTED);
    let window: Vec<&RunLine> = docker
        .iter()
        .copied()
        .filter(|r| r.ts_ms >= from_ms)
        .collect();
    if window.is_empty() {
        return None;
    }
    let mut checkouts: Vec<u64> = window.iter().filter_map(|r| checkout_ms(r)).collect();
    let total_ms = (!checkouts.is_empty()).then(|| checkouts.iter().sum());
    Some(Hint {
        kind: "git_mirror",
        cause: cause.clone(),
        profiles: profiles.to_vec(),
        window_days: WINDOW_DAYS,
        docker_runs: window.len(),
        dind_runs: window.iter().filter(|r| granted(r)).count(),
        last_dind_ms: docker.iter().filter(|r| granted(r)).map(|r| r.ts_ms).max(),
        runs: (!checkouts.is_empty()).then_some(checkouts.len()),
        median_ms: median(&mut checkouts),
        total_ms,
    })
}

/// The hints `/api/state` serves: this boot's [`Plan`], and the figures last
/// computed for it.
pub struct Hints {
    plan: Plan,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    hints: Vec<Hint>,
    /// When the figures were last computed, or a recompute last began.
    at_ms: Option<u64>,
}

impl Hints {
    pub fn new(plan: Plan) -> Self {
        Self {
            plan,
            cache: Mutex::default(),
        }
    }

    /// No hints, ever: a host with nothing to gain, and tests.
    pub fn none() -> Self {
        Self::new(Plan::default())
    }

    /// The hints as last computed. Empty until the first [`Self::refresh`].
    pub fn view(&self) -> Vec<Hint> {
        self.cache.lock().unwrap().hints.clone()
    }

    /// Whether the figures are due a recompute at `now_ms`. Answers `true` once
    /// per [`REFRESH_MS`], so the caller that hears it is the one that runs
    /// [`Self::refresh`]. Never due with an empty plan: such a host reads nothing.
    pub fn due(&self, now_ms: u64) -> bool {
        if self.plan.is_empty() {
            return false;
        }
        let mut cache = self.cache.lock().unwrap();
        let due = cache
            .at_ms
            .is_none_or(|at| now_ms.saturating_sub(at) >= REFRESH_MS);
        if due {
            cache.at_ms = Some(now_ms);
        }
        due
    }

    /// Recompute from the audit log. Reads the whole file: call it off the
    /// async runtime.
    pub fn refresh(&self, audit: &Audit, now_ms: u64) {
        let from = now_ms.saturating_sub(crate::jobs_view::MAX_WINDOW_DAYS * DAY_MS);
        let runs = crate::jobs_view::read_runs(audit, from);
        self.set(&runs, now_ms);
    }

    fn set(&self, runs: &[RunLine], now_ms: u64) {
        let hints = self.plan.hints(runs, now_ms);
        let mut cache = self.cache.lock().unwrap();
        cache.hints = hints;
        cache.at_ms = Some(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::{bucket_of, Footprint, StepRecord};

    const NOW: u64 = 100 * DAY_MS;

    fn cfg(toml: &str) -> FileConfig {
        FileConfig::load_from_str(toml, "test").expect("a test config parses")
    }

    fn docker_profile(name: &str, rest: &str) -> String {
        format!(
            "[[profile]]\nname = \"{name}\"\nlabels = [\"faber\", \"{name}\"]\n\
             backend = \"docker\"\nimage = \"faber-runner:local\"\n{rest}\n"
        )
    }

    fn plan(toml: &str, marked: bool) -> Plan {
        let cfg = cfg(toml);
        Plan::of(&cfg, &mirror_beside_dind::decide(&cfg, marked))
    }

    fn step(name: &str, ms: u64) -> StepRecord {
        StepRecord {
            name: name.to_string(),
            bucket: bucket_of(name),
            started_ms: Some(1_000),
            completed_ms: Some(1_000 + ms),
            conclusion: Some("success".to_string()),
        }
    }

    /// A docker run that finished `days_ago` days before [`NOW`].
    fn run(days_ago: u64, dind: &str, steps: Option<Vec<StepRecord>>) -> RunLine {
        RunLine {
            ts_ms: NOW - days_ago * DAY_MS,
            dind: Some(dind.to_string()),
            footprint: Footprint {
                steps,
                ..Footprint::default()
            },
            ..RunLine::default()
        }
    }

    fn off() -> Plan {
        plan(&docker_profile("linux", "dind = false"), false)
    }

    // --- the plan ------------------------------------------------------------

    #[test]
    fn a_host_with_no_docker_profile_has_nothing_to_gain() {
        assert!(plan("", false).is_empty());
        assert!(plan("", true).is_empty(), "a marker alone is not a host");
    }

    #[test]
    fn a_mirror_in_use_is_no_hint() {
        let toml = docker_profile("linux", "dind = false\ngit_mirror = true");
        assert!(plan(&toml, false).is_empty());
    }

    #[test]
    fn a_mirror_no_profile_asks_for_is_off() {
        assert_eq!(off().git_mirror, Some((Cause::Off, Vec::new())));
    }

    #[test]
    fn a_granting_profile_holds_it_off_whether_or_not_the_config_asks() {
        for mirror in ["", "git_mirror = true"] {
            let toml = format!(
                "{}{}",
                docker_profile("linux", &format!("dind = false\n{mirror}")),
                docker_profile("linux-dind", "dind = true"),
            );
            assert_eq!(
                plan(&toml, true).git_mirror,
                Some((Cause::DindGranted, vec!["linux-dind".to_string()])),
                "{mirror:?}"
            );
        }
        // No `dind` key still grants (ADR-0013).
        assert_eq!(
            plan(&docker_profile("linux", ""), false).git_mirror,
            Some((Cause::DindGranted, vec!["linux".to_string()]))
        );
    }

    #[test]
    fn a_marked_engine_holds_it_off_after_every_profile_denies() {
        let toml = docker_profile("linux", "dind = false\ngit_mirror = true");
        assert_eq!(
            plan(&toml, true).git_mirror,
            Some((Cause::EngineMarked, Vec::new()))
        );
    }

    #[test]
    fn the_host_key_turns_every_hint_off() {
        let toml = format!(
            "[host]\nspeed_hints = false\n{}",
            docker_profile("linux", "dind = false")
        );
        assert!(plan(&toml, false).is_empty());
        assert!(plan(&toml, true).is_empty());
        // Absent, the key is on.
        assert!(!off().is_empty());
    }

    // --- the figures ---------------------------------------------------------

    #[test]
    fn no_docker_run_in_the_window_is_no_hint() {
        assert!(off().hints(&[], NOW).is_empty());
        // Older than the window, and a run with no container at all.
        let old = run(WINDOW_DAYS + 1, "denied", None);
        let vm = RunLine {
            ts_ms: NOW,
            ..RunLine::default()
        };
        assert!(off().hints(&[old, vm], NOW).is_empty());
    }

    #[test]
    fn checkout_time_is_the_median_and_the_sum_over_timed_runs() {
        let checkout = "Run actions/checkout@v4";
        let runs = [
            run(1, "denied", Some(vec![step(checkout, 10_000)])),
            run(2, "denied", Some(vec![step(checkout, 30_000)])),
            // Two checkouts in one run add up; other steps do not count.
            run(
                3,
                "denied",
                Some(vec![
                    step(checkout, 15_000),
                    step("Checkout the docs", 5_000),
                    step("Run npm ci", 60_000),
                    step("Post Run actions/checkout@v4", 2_000),
                ]),
            ),
            // No steps recorded, and steps with no checkout: counted as runs,
            // not as timed ones.
            run(1, "denied", None),
            run(1, "denied", Some(vec![step("Run make", 9_000)])),
            // Outside the window.
            run(
                WINDOW_DAYS + 2,
                "denied",
                Some(vec![step(checkout, 99_000)]),
            ),
        ];
        let hints = off().hints(&runs, NOW);
        assert_eq!(hints.len(), 1);
        let h = &hints[0];
        assert_eq!((h.kind, &h.cause), ("git_mirror", &Cause::Off));
        assert_eq!(h.window_days, WINDOW_DAYS);
        assert_eq!(h.docker_runs, 5);
        assert_eq!(h.runs, Some(3));
        assert_eq!(h.median_ms, Some(20_000));
        assert_eq!(h.total_ms, Some(60_000));
    }

    #[test]
    fn with_no_timed_checkout_the_hint_carries_no_figure() {
        let runs = [
            run(1, "denied", None),
            run(2, "denied", Some(vec![step("Run make", 9_000)])),
        ];
        let h = &off().hints(&runs, NOW)[0];
        assert_eq!(h.docker_runs, 2);
        assert_eq!((h.runs, h.median_ms, h.total_ms), (None, None, None));
        let json = serde_json::to_value(h).unwrap();
        for absent in ["runs", "median_ms", "total_ms", "last_dind_ms", "profiles"] {
            assert!(json.get(absent).is_none(), "{absent}");
        }
        assert_eq!(json["cause"], "off");
    }

    #[test]
    fn a_checkout_step_is_the_action_or_a_name_that_begins_with_the_word() {
        for name in [
            "Run actions/checkout@v4",
            "Run actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683",
            "Checkout",
            "checkout code",
            "Checkout owner/demo",
        ] {
            assert!(is_checkout(name), "{name}");
        }
        for name in [
            "Post Run actions/checkout@v4",
            "Post Checkout",
            "Run actions/cache@v4",
            "Run git checkout main",
            "Checkouts report",
        ] {
            assert!(!is_checkout(name), "{name}");
        }
    }

    #[test]
    fn dind_is_counted_by_its_value_never_by_its_presence() {
        let held = plan(
            &format!(
                "{}{}",
                docker_profile("linux", "dind = false"),
                docker_profile("linux-dind", "dind = true")
            ),
            true,
        );
        // Every run has the field; only the old one was granted a daemon.
        let runs = [
            run(1, "denied", None),
            run(2, "denied", None),
            run(21, "granted", None),
            run(40, "granted", None),
        ];
        let h = &held.hints(&runs, NOW)[0];
        assert_eq!(h.cause, Cause::DindGranted);
        assert_eq!(h.profiles, ["linux-dind"]);
        assert_eq!((h.docker_runs, h.dind_runs), (2, 0));
        assert_eq!(h.last_dind_ms, Some(NOW - 21 * DAY_MS));

        let used = [run(1, "granted", None), run(2, "denied", None)];
        let h = &held.hints(&used, NOW)[0];
        assert_eq!((h.docker_runs, h.dind_runs), (2, 1));
        assert_eq!(h.last_dind_ms, Some(NOW - DAY_MS));
        assert_eq!(serde_json::to_value(h).unwrap()["cause"], "dind_granted");
    }

    #[test]
    fn the_median_of_an_even_count_is_the_lower_middle() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [7]), Some(7));
        assert_eq!(median(&mut [9, 1, 5, 3]), Some(3));
    }

    // --- the cache -----------------------------------------------------------

    #[test]
    fn a_refresh_is_due_once_per_interval_and_never_with_nothing_to_gain() {
        assert!(!Hints::none().due(NOW));

        let hints = Hints::new(off());
        assert!(hints.view().is_empty(), "empty until the first refresh");
        assert!(hints.due(NOW));
        assert!(!hints.due(NOW + 1), "one caller hears it");
        hints.set(&[run(1, "denied", None)], NOW);
        assert_eq!(hints.view().len(), 1);
        assert!(!hints.due(NOW + REFRESH_MS - 1));
        assert!(hints.due(NOW + REFRESH_MS));
    }
}
