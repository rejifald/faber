//! The Jobs view's data (ADR-0056 §5): per job class, where its runs' time went,
//! whether they waited or ran, and whether they were starved or idle — and one
//! run's own phases for Activity's row detail.
//!
//! Read from the `outcome` lines of the audit log, the source of truth, over the
//! window the view asks for. There is no store of its own.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::audit::Audit;
use crate::footprint::{Bucket, Footprint, Phase, PhaseSpan, StepRecord};

/// The window the view reads when it names none, in days.
pub const DEFAULT_WINDOW_DAYS: u64 = 7;
/// The widest window it may ask for, in days.
pub const MAX_WINDOW_DAYS: u64 = 90;
/// Below this many runs in the window a class is `profiling` and left out of the
/// sort: the limiter's own threshold. Two runs must not rank the levers.
pub const PROFILING_RUNS: usize = 20;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The fields of an `outcome` line the view reads. Tolerant like every audit
/// reader: unknown fields are ignored, missing ones are `None`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RunLine {
    pub ts_ms: u64,
    kind: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub job_id: u64,
    #[serde(default)]
    pub executed_job_id: Option<u64>,
    #[serde(default)]
    pub runner: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub wait_gate: Option<String>,
    #[serde(default)]
    pub wait_gates: Option<std::collections::BTreeMap<String, u64>>,
    #[serde(default)]
    pub cpu_avg_runner_mcpu: Option<u64>,
    #[serde(default)]
    pub cpu_avg_dind_mcpu: Option<u64>,
    #[serde(default)]
    pub cpu_peak_runner_mcpu: Option<u64>,
    #[serde(flatten)]
    pub footprint: Footprint,
}

impl RunLine {
    fn phases(&self) -> [PhaseSpan; 7] {
        self.footprint.marks(self.ts_ms).phases()
    }

    /// GitHub's steps summed per bucket, or `None` when no read recorded them.
    fn split(&self) -> Option<Split<u64>> {
        let steps = self.footprint.steps.as_ref()?;
        let mut split = Split::default();
        for s in steps {
            let ms = s.duration_ms().unwrap_or(0);
            *split.of_mut(s.bucket) += ms;
        }
        Some(split)
    }

    /// The run's wall time, from GitHub queueing it (or, failing that, faber
    /// seeing it) to faber finishing cleanup.
    fn wall_ms(&self) -> Option<u64> {
        let from = self.footprint.queued_ms.or(self.footprint.seen_ms)?;
        self.ts_ms.checked_sub(from)
    }

    /// CPU used on average across the sampled run, runner and dind together.
    fn cpu_used_mcpu(&self) -> Option<u64> {
        match (self.cpu_avg_runner_mcpu, self.cpu_avg_dind_mcpu) {
            (None, None) => None,
            (r, d) => Some(r.unwrap_or(0) + d.unwrap_or(0)),
        }
    }

    fn throttled_ms(&self) -> Option<u64> {
        let f = &self.footprint;
        match (f.cpu_throttled_runner_usec, f.cpu_throttled_dind_usec) {
            (None, None) => None,
            (r, d) => Some((r.unwrap_or(0) + d.unwrap_or(0)) / 1000),
        }
    }

    fn io_bytes(&self) -> Option<u64> {
        let f = &self.footprint;
        match (f.io_runner, f.io_dind) {
            (None, None) => None,
            (r, d) => Some(r.map_or(0, |io| io.total()) + d.map_or(0, |io| io.total())),
        }
    }
}

/// Read every `outcome` line stamped at or after `from_ms`, oldest first.
pub fn read_runs(audit: &Audit, from_ms: u64) -> Vec<RunLine> {
    let Ok(text) = std::fs::read_to_string(audit.path()) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| line.contains("\"kind\":\"outcome\""))
        .filter_map(|line| serde_json::from_str::<RunLine>(line).ok())
        .filter(|r| r.kind == "outcome" && r.ts_ms >= from_ms)
        .collect()
}

/// The window, in days, a request for `days` gets.
pub fn window_days(days: Option<u64>) -> u64 {
    days.unwrap_or(DEFAULT_WINDOW_DAYS)
        .clamp(1, MAX_WINDOW_DAYS)
}

/// A value per bucket of [`Bucket`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Split<T> {
    pub setup: T,
    pub steps: T,
    pub teardown: T,
}

impl<T> Split<T> {
    fn of_mut(&mut self, bucket: Bucket) -> &mut T {
        match bucket {
            Bucket::Setup => &mut self.setup,
            Bucket::Steps => &mut self.steps,
            Bucket::Teardown => &mut self.teardown,
        }
    }
}

/// The median and p90 of a quantity, over the runs that measured it. `runs`
/// says how many did: a quantity no run measured has no figures, never zeros.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Spread {
    pub median: Option<u64>,
    pub p90: Option<u64>,
    pub runs: usize,
}

impl Spread {
    fn of(mut values: Vec<u64>) -> Self {
        values.sort_unstable();
        Self {
            median: nearest_rank(&values, 50),
            p90: nearest_rank(&values, 90),
            runs: values.len(),
        }
    }
}

/// The nearest-rank percentile of sorted `values`.
fn nearest_rank(values: &[u64], pct: usize) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let rank = (pct * values.len()).div_ceil(100).max(1);
    values.get(rank - 1).copied()
}

/// One phase's spread across a class's runs.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseSpread {
    pub phase: Phase,
    #[serde(flatten)]
    pub spread: Spread,
    /// Runs whose figure for this phase crossed clocks and came out negative.
    pub skewed_runs: usize,
}

/// The gate that bound a class's parked runs most often.
#[derive(Debug, Clone, Serialize)]
pub struct TopGate {
    pub gate: String,
    pub runs: usize,
}

/// One job class in the window.
#[derive(Debug, Clone, Serialize)]
pub struct ClassView {
    /// The limiter's class key, or the run's label for a class it never learned
    /// (a macOS job, a run that fed no sample).
    pub key: String,
    pub label: String,
    pub repo: String,
    /// The workflow and job the label names, split from its `repo · workflow · job`
    /// form, so the view can lead with the job. `workflow` is `None` for a label
    /// of any other shape, whose whole text is then `job`.
    pub workflow: Option<String>,
    pub job: String,
    pub runs: usize,
    /// Fewer than [`PROFILING_RUNS`] runs: shown, and left out of the sort.
    pub profiling: bool,
    /// Runs on a backend without cgroups, which measure phases and steps only.
    pub unmeasured_resource_runs: usize,
    pub wall: Spread,
    /// The sum of every measured run's wall time: what the sort ranks by.
    pub total_ms: u64,
    pub phases: Vec<PhaseSpread>,
    /// The `job` phase split by GitHub's steps.
    pub split: Split<Spread>,
    /// Parked time over wall time, summed over the runs that measured both.
    pub parked_share: Option<f64>,
    pub top_gate: Option<TopGate>,
    pub cpu_used_mcpu: Spread,
    pub cpu_reserved_mcpu: Spread,
    pub throttled_ms: Spread,
    pub io_bytes: Spread,
}

/// The Jobs view: every class with a run in the window.
#[derive(Debug, Clone, Serialize)]
pub struct JobsView {
    pub window_days: u64,
    pub from_ms: u64,
    pub to_ms: u64,
    pub profiling_runs: usize,
    pub classes: Vec<ClassView>,
}

/// Build the view over `runs` for a window of `days` ending at `now_ms`.
pub fn jobs_view(runs: &[RunLine], days: u64, now_ms: u64) -> JobsView {
    let from_ms = now_ms.saturating_sub(days * DAY_MS);
    // A class's learned runs carry its key and its others only its label, so the
    // label is mapped back to the key wherever any line names both.
    let key_of_label: HashMap<&str, &str> = runs
        .iter()
        .filter_map(|r| Some((r.label.as_deref()?, r.class.as_deref()?)))
        .collect();
    let mut groups: HashMap<String, Vec<&RunLine>> = HashMap::new();
    for r in runs.iter().filter(|r| r.ts_ms >= from_ms) {
        let key = r
            .class
            .clone()
            .or_else(|| {
                let label = r.label.as_deref()?;
                Some(key_of_label.get(label).map_or(label, |k| k).to_string())
            })
            .unwrap_or_else(|| format!("{} · job {}", r.repo, r.job_id));
        groups.entry(key).or_default().push(r);
    }
    let mut classes: Vec<ClassView> = groups
        .into_iter()
        .map(|(key, runs)| class_view(key, &runs))
        .collect();
    classes.sort_by(|a, b| {
        a.profiling
            .cmp(&b.profiling)
            .then(b.total_ms.cmp(&a.total_ms))
            .then(a.label.cmp(&b.label))
    });
    JobsView {
        window_days: days,
        from_ms,
        to_ms: now_ms,
        profiling_runs: PROFILING_RUNS,
        classes,
    }
}

fn class_view(key: String, runs: &[&RunLine]) -> ClassView {
    let latest = runs.iter().max_by_key(|r| r.ts_ms).copied();
    let label = latest
        .and_then(|r| r.label.clone())
        .unwrap_or_else(|| key.clone());
    let repo = latest.map(|r| r.repo.clone()).unwrap_or_default();
    let (workflow, job) = split_label(&label);

    let spans: Vec<[PhaseSpan; 7]> = runs.iter().map(|r| r.phases()).collect();
    let phases = Phase::ALL
        .iter()
        .enumerate()
        .map(|(i, phase)| PhaseSpread {
            phase: *phase,
            spread: Spread::of(spans.iter().filter_map(|s| s[i].ms).collect()),
            skewed_runs: spans.iter().filter(|s| s[i].skewed).count(),
        })
        .collect();

    let splits: Vec<Split<u64>> = runs.iter().filter_map(|r| r.split()).collect();
    let split = Split {
        setup: Spread::of(splits.iter().map(|s| s.setup).collect()),
        steps: Spread::of(splits.iter().map(|s| s.steps).collect()),
        teardown: Spread::of(splits.iter().map(|s| s.teardown).collect()),
    };

    let walls: Vec<u64> = runs.iter().filter_map(|r| r.wall_ms()).collect();
    let total_ms = walls.iter().sum();

    let parked_index = Phase::ALL
        .iter()
        .position(|p| *p == Phase::Parked)
        .expect("parked is a phase");
    let (parked, wall) = runs
        .iter()
        .zip(&spans)
        .filter_map(|(r, s)| Some((s[parked_index].ms?, r.wall_ms()?)))
        .fold((0u64, 0u64), |(p, w), (rp, rw)| (p + rp, w + rw));
    let parked_share = (wall > 0).then(|| parked as f64 / wall as f64);

    let mut gates: HashMap<&str, usize> = HashMap::new();
    for gate in runs.iter().filter_map(|r| r.wait_gate.as_deref()) {
        *gates.entry(gate).or_default() += 1;
    }
    let top_gate = gates
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
        .map(|(gate, runs)| TopGate {
            gate: gate.to_string(),
            runs,
        });

    let measured =
        |f: fn(&RunLine) -> Option<u64>| Spread::of(runs.iter().filter_map(|r| f(r)).collect());
    ClassView {
        key,
        label,
        repo,
        workflow,
        job,
        runs: runs.len(),
        profiling: runs.len() < PROFILING_RUNS,
        unmeasured_resource_runs: runs
            .iter()
            .filter(|r| r.cpu_peak_runner_mcpu.is_none() && r.footprint.io_runner.is_none())
            .count(),
        wall: Spread::of(walls),
        total_ms,
        phases,
        split,
        parked_share,
        top_gate,
        cpu_used_mcpu: measured(RunLine::cpu_used_mcpu),
        cpu_reserved_mcpu: measured(|r| r.footprint.cpu_reserved_mcpu),
        throttled_ms: measured(RunLine::throttled_ms),
        io_bytes: measured(RunLine::io_bytes),
    }
}

/// The workflow and job of a label in `crate::reconcile`'s `repo · workflow ·
/// job` form. Any other shape is all job.
fn split_label(label: &str) -> (Option<String>, String) {
    let mut parts = label.splitn(3, " · ");
    match (parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(workflow), Some(job)) => (Some(workflow.to_string()), job.to_string()),
        _ => (None, label.to_string()),
    }
}

/// One run's own footprint, for Activity's row detail.
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub runner: String,
    pub job_id: u64,
    pub label: Option<String>,
    pub result: Option<String>,
    pub finished_ms: u64,
    pub phases: Vec<PhaseSpan>,
    /// The `job` phase split by GitHub's steps; `None` when no read recorded them.
    pub split: Option<Split<u64>>,
    pub steps: Option<Vec<StepRecord>>,
    pub wait_gate: Option<String>,
    pub cpu_used_mcpu: Option<u64>,
    pub cpu_reserved_mcpu: Option<u64>,
    pub throttled_ms: Option<u64>,
    pub io_bytes: Option<u64>,
}

/// The run whose runner is named `runner`, from its `outcome` line. The latest
/// line wins, should a runner name ever recur.
pub fn run_view(runs: &[RunLine], runner: &str) -> Option<RunView> {
    let r = runs
        .iter()
        .rev()
        .find(|r| r.runner.as_deref() == Some(runner))?;
    Some(RunView {
        runner: runner.to_string(),
        job_id: r.executed_job_id.unwrap_or(r.job_id),
        label: r.label.clone(),
        result: r.result.clone(),
        finished_ms: r.ts_ms,
        phases: r.phases().to_vec(),
        split: r.split(),
        steps: r.footprint.steps.clone(),
        wait_gate: r.wait_gate.clone(),
        cpu_used_mcpu: r.cpu_used_mcpu(),
        cpu_reserved_mcpu: r.footprint.cpu_reserved_mcpu,
        throttled_ms: r.throttled_ms(),
        io_bytes: r.io_bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 100 * DAY_MS;

    /// One outcome line of class `key`, finished `ago` ms before [`NOW`], with a
    /// pickup of `pickup` ms and a parked phase of `parked` ms around 60 s of job.
    fn line(key: Option<&str>, label: &str, ago: u64, pickup: u64, parked: u64) -> RunLine {
        let end = NOW - ago;
        let started = end - 70_000;
        let admitted = started - 10_000;
        let prepared = admitted - parked;
        let seen = prepared - 1_000;
        let mut v = serde_json::json!({
            "ts_ms": end, "kind": "outcome", "repo": "owner/demo", "job_id": 7,
            "runner": format!("faber-7-{end}"), "label": label,
            "queued_ms": seen - pickup, "seen_ms": seen, "prepared_ms": prepared,
            "admitted_ms": admitted, "container_ms": admitted + 5_000,
            "job_started_ms": started, "job_completed_ms": started + 60_000,
            "cpu_avg_runner_mcpu": 1_500, "cpu_peak_runner_mcpu": 3_000,
            "cpu_reserved_mcpu": 2_000, "cpu_throttled_runner_usec": 2_000_000,
            "io_runner": {"block_read": 10, "block_written": 20, "net_received": 30, "net_sent": 40},
            "steps": [
                {"name": "Set up job", "bucket": "setup", "started_ms": started, "completed_ms": started + 5_000},
                {"name": "Run tests", "bucket": "steps", "started_ms": started + 5_000, "completed_ms": started + 55_000},
                {"name": "Complete job", "bucket": "teardown", "started_ms": started + 55_000, "completed_ms": started + 60_000},
            ],
        });
        if parked > 0 {
            v["wait_gate"] = "memory".into();
        }
        if let Some(k) = key {
            v["class"] = k.into();
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_label_splits_into_workflow_and_job() {
        assert_eq!(
            split_label("owner/demo · CI · unit · fast"),
            (Some("CI".to_string()), "unit · fast".to_string())
        );
        assert_eq!(
            split_label("owner/demo job 7"),
            (None, "owner/demo job 7".to_string())
        );
    }

    #[test]
    fn nearest_rank_percentiles() {
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(nearest_rank(&v, 50), Some(5));
        assert_eq!(nearest_rank(&v, 90), Some(9));
        assert_eq!(nearest_rank(&[7], 90), Some(7));
        assert_eq!(nearest_rank(&[], 50), None);
    }

    #[test]
    fn the_window_defaults_to_a_week_and_stops_at_ninety_days() {
        assert_eq!(window_days(None), 7);
        assert_eq!(window_days(Some(0)), 1);
        assert_eq!(window_days(Some(365)), 90);
    }

    #[test]
    fn a_class_reports_each_phase_and_the_step_split() {
        let runs: Vec<RunLine> = (0..20)
            .map(|i| {
                line(
                    Some("owner/demo::1::build"),
                    "owner/demo · ci · build",
                    i * 1_000,
                    20_000,
                    0,
                )
            })
            .collect();
        let view = jobs_view(&runs, 7, NOW);
        let class = &view.classes[0];
        assert_eq!(class.runs, 20);
        assert!(!class.profiling);
        let ms = |p: Phase| {
            class
                .phases
                .iter()
                .find(|s| s.phase == p)
                .unwrap()
                .spread
                .median
        };
        assert_eq!(ms(Phase::Pickup), Some(20_000));
        assert_eq!(ms(Phase::Parked), Some(0));
        assert_eq!(ms(Phase::Sandbox), Some(5_000));
        assert_eq!(ms(Phase::Job), Some(60_000));
        assert_eq!(class.split.setup.median, Some(5_000));
        assert_eq!(class.split.steps.median, Some(50_000));
        assert_eq!(class.cpu_used_mcpu.median, Some(1_500));
        assert_eq!(class.cpu_reserved_mcpu.median, Some(2_000));
        assert_eq!(class.throttled_ms.median, Some(2_000));
        assert_eq!(class.io_bytes.median, Some(100));
        assert_eq!(class.top_gate.as_ref().map(|g| g.gate.as_str()), None);
    }

    #[test]
    fn a_class_under_twenty_runs_is_profiling_and_sorts_last() {
        let mut runs: Vec<RunLine> = (0..20)
            .map(|i| {
                line(
                    Some("owner/demo::1::fast"),
                    "owner/demo · ci · fast",
                    i,
                    0,
                    0,
                )
            })
            .collect();
        // Two very slow runs of another class must not outrank twenty.
        runs.extend((0..2).map(|i| {
            line(
                Some("owner/demo::1::slow"),
                "owner/demo · ci · slow",
                i,
                0,
                10_000_000,
            )
        }));
        let view = jobs_view(&runs, 7, NOW);
        assert_eq!(view.classes[0].key, "owner/demo::1::fast");
        assert!(view.classes[1].profiling);
        assert_eq!(view.classes[1].top_gate.as_ref().unwrap().gate, "memory");
        assert!(view.classes[1].parked_share.unwrap() > 0.9);
    }

    #[test]
    fn a_run_that_fed_no_sample_joins_its_class_by_label() {
        let runs = vec![
            line(
                Some("owner/demo::1::build"),
                "owner/demo · ci · build",
                0,
                0,
                0,
            ),
            line(None, "owner/demo · ci · build", 1, 0, 0),
        ];
        let view = jobs_view(&runs, 7, NOW);
        assert_eq!(view.classes.len(), 1);
        assert_eq!(view.classes[0].runs, 2);
    }

    #[test]
    fn runs_outside_the_window_are_left_out() {
        let runs = vec![
            line(Some("k"), "l", 0, 0, 0),
            line(Some("k"), "l", 8 * DAY_MS, 0, 0),
        ];
        assert_eq!(jobs_view(&runs, 7, NOW).classes[0].runs, 1);
    }

    #[test]
    fn an_unmeasured_quantity_has_no_figures() {
        // A macOS run: phases only, no cgroups.
        let mut r = line(None, "owner/demo · mac · build", 0, 0, 0);
        r.cpu_avg_runner_mcpu = None;
        r.cpu_peak_runner_mcpu = None;
        r.footprint.io_runner = None;
        r.footprint.cpu_throttled_runner_usec = None;
        r.footprint.steps = None;
        let class = &jobs_view(&[r], 7, NOW).classes[0];
        assert_eq!(class.cpu_used_mcpu, Spread::default());
        assert_eq!(class.io_bytes.median, None);
        assert_eq!(class.split.setup.runs, 0);
        assert_eq!(class.unmeasured_resource_runs, 1);
    }

    #[test]
    fn one_run_is_found_by_its_runner() {
        let runs = vec![line(Some("k"), "l", 0, 20_000, 0)];
        let runner = runs[0].runner.clone().unwrap();
        let view = run_view(&runs, &runner).unwrap();
        assert_eq!(view.phases.len(), 7);
        assert_eq!(view.phases[0].ms, Some(20_000));
        assert_eq!(view.split.unwrap().steps, 50_000);
        assert!(run_view(&runs, "faber-0-0").is_none());
    }

    #[test]
    fn the_reader_takes_outcome_lines_only() {
        let dir = crate::reconcile::tests::TempDir::new();
        let path = dir.path.join("audit.jsonl");
        std::fs::write(
            &path,
            "{\"ts_ms\":5,\"kind\":\"decision\",\"repo\":\"owner/demo\",\"job_id\":1}\n\
             {\"ts_ms\":6,\"kind\":\"outcome\",\"repo\":\"owner/demo\",\"job_id\":1,\"seen_ms\":2}\n\
             not json\n",
        )
        .unwrap();
        let runs = read_runs(&Audit::at(path), 0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].footprint.seen_ms, Some(2));
    }
}
