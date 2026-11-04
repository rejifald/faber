//! Where a run's time and resources went (ADR-0056).
//!
//! A run is cut into contiguous phases, each the gap between two timestamps the
//! `outcome` audit line records ([`Marks`]). GitHub's own steps split the `job`
//! phase into setup, job steps and teardown by a fixed table ([`bucket_of`]),
//! never by guessing from free text. Everything here is measurement: nothing reads
//! it to decide what a host does.

use serde::{Deserialize, Serialize};

use crate::github::{Job, Step};
use crate::retry::parse_rfc3339_to_ms;

/// Which part of a job a GitHub step belongs to (ADR-0056 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    /// GitHub's own job setup, a checkout / setup / cache action, or a dependency
    /// install GitHub named from its command.
    Setup,
    /// Everything not on either list. Called "job steps", not "work": an install
    /// hidden in a step named `test` lands here, and the view says so.
    Steps,
    /// `Post …` steps and GitHub's own cleanup.
    Teardown,
}

/// How one entry of [`TABLE`] matches a step name, as GitHub reports it.
#[derive(Debug, Clone, Copy)]
enum Rule {
    /// The whole name.
    Exact(&'static str),
    /// The start of the name.
    Prefix(&'static str),
    /// A run step GitHub named from its command: `Run <command>`, alone or with
    /// arguments after a space.
    Command(&'static str),
}

impl Rule {
    fn matches(self, name: &str) -> bool {
        match self {
            Rule::Exact(s) => name == s,
            Rule::Prefix(s) => name.starts_with(s),
            Rule::Command(cmd) => name
                .strip_prefix("Run ")
                .and_then(|rest| rest.strip_prefix(cmd))
                .is_some_and(|tail| tail.is_empty() || tail.starts_with(' ')),
        }
    }
}

/// The one classification table. First match wins, so teardown's `Post ` comes
/// before the setup actions a post step is named after. A step on no row is a
/// job step. Each row has its own test below; the table grows with what the Jobs
/// view shows.
const TABLE: &[(Rule, Bucket)] = &[
    (Rule::Prefix("Post "), Bucket::Teardown),
    (Rule::Exact("Stop containers"), Bucket::Teardown),
    (Rule::Exact("Complete job"), Bucket::Teardown),
    (Rule::Exact("Set up job"), Bucket::Setup),
    (Rule::Exact("Initialize containers"), Bucket::Setup),
    (Rule::Prefix("Run actions/checkout@"), Bucket::Setup),
    (Rule::Prefix("Run actions/setup-"), Bucket::Setup),
    (Rule::Prefix("Run actions/cache@"), Bucket::Setup),
    (Rule::Prefix("Run actions/cache/restore@"), Bucket::Setup),
    (Rule::Command("npm ci"), Bucket::Setup),
    (Rule::Command("pnpm install"), Bucket::Setup),
    (Rule::Command("yarn install"), Bucket::Setup),
    (Rule::Command("pip install"), Bucket::Setup),
    (Rule::Command("uv sync"), Bucket::Setup),
    (Rule::Command("cargo fetch"), Bucket::Setup),
    (Rule::Command("go mod download"), Bucket::Setup),
    (Rule::Command("bundle install"), Bucket::Setup),
];

/// The bucket a step named `name` lands in.
pub fn bucket_of(name: &str) -> Bucket {
    TABLE
        .iter()
        .find(|(rule, _)| rule.matches(name))
        .map_or(Bucket::Steps, |(_, bucket)| *bucket)
}

/// One GitHub step as the `outcome` line records it. The name is GitHub's,
/// verbatim: workflow text that stays on the host (ADR-0056 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    pub name: String,
    pub bucket: Bucket,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

impl StepRecord {
    fn of(step: &Step) -> Self {
        Self {
            name: step.name.clone(),
            bucket: bucket_of(&step.name),
            started_ms: step.started_at.as_deref().and_then(parse_rfc3339_to_ms),
            completed_ms: step.completed_at.as_deref().and_then(parse_rfc3339_to_ms),
            conclusion: step.conclusion.clone(),
        }
    }

    /// How long the step ran, when GitHub gave both ends in order.
    pub fn duration_ms(&self) -> Option<u64> {
        self.completed_ms?.checked_sub(self.started_ms?)
    }
}

/// GitHub's side of a run: when it queued, dispatched and finished the job, and
/// its steps once finished. Read from one job fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobTimes {
    /// The job these times describe, which may not be the one a runner was minted
    /// for (a JIT runner is not job-pinned).
    pub job_id: u64,
    pub queued_ms: Option<u64>,
    pub started_ms: Option<u64>,
    pub completed_ms: Option<u64>,
    /// `None` until GitHub reports the job `completed`: "no steps recorded",
    /// never an empty list.
    pub steps: Option<Vec<StepRecord>>,
}

impl JobTimes {
    pub fn of(job: &Job) -> Self {
        let done = job.status == "completed";
        Self {
            job_id: job.id,
            queued_ms: job.created_at.as_deref().and_then(parse_rfc3339_to_ms),
            started_ms: job.started_at.as_deref().and_then(parse_rfc3339_to_ms),
            completed_ms: done
                .then(|| job.completed_at.as_deref().and_then(parse_rfc3339_to_ms))
                .flatten(),
            steps: done
                .then(|| {
                    job.steps
                        .as_ref()
                        .map(|s| s.iter().map(StepRecord::of).collect())
                })
                .flatten(),
        }
    }

    /// Whether these times are final: the job completed and its steps were read.
    pub fn is_complete(&self) -> bool {
        self.completed_ms.is_some() && self.steps.is_some()
    }
}

/// faber's own marks for one claim, filled in as it moves through provisioning.
/// Shared between the claim's provision task and the reconciler's job poll.
#[derive(Debug, Clone, Default)]
pub struct Timeline {
    /// When faber's poll took the job.
    pub seen_ms: Option<u64>,
    /// When the claim began to wait for admission: its image resolved, and any
    /// variant built.
    pub prepared_ms: Option<u64>,
    /// When admission let the claim through.
    pub admitted_ms: Option<u64>,
    /// The CPU share the ledger debited for the run, in millicores.
    pub reserved_mcpu: Option<u64>,
    /// The last read of the claim's own job, kept once GitHub reports it
    /// completed.
    pub job: Option<JobTimes>,
}

/// Every boundary of a run, epoch milliseconds. Each is optional: a mark faber
/// did not see is "not measured", never zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Marks {
    pub queued: Option<u64>,
    pub seen: Option<u64>,
    pub prepared: Option<u64>,
    pub admitted: Option<u64>,
    pub container: Option<u64>,
    pub started: Option<u64>,
    pub completed: Option<u64>,
    pub finished: Option<u64>,
}

/// One phase of ADR-0056 §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pickup,
    Prepare,
    Parked,
    Sandbox,
    Register,
    Job,
    Teardown,
}

impl Phase {
    pub const ALL: [Phase; 7] = [
        Phase::Pickup,
        Phase::Prepare,
        Phase::Parked,
        Phase::Sandbox,
        Phase::Register,
        Phase::Job,
        Phase::Teardown,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Phase::Pickup => "pickup",
            Phase::Prepare => "prepare",
            Phase::Parked => "parked",
            Phase::Sandbox => "sandbox",
            Phase::Register => "register",
            Phase::Job => "job",
            Phase::Teardown => "teardown",
        }
    }

    /// The phase's two boundaries, and whether they are read off different
    /// clocks (the host's and GitHub's).
    fn span(self, m: &Marks) -> (Option<u64>, Option<u64>, bool) {
        match self {
            Phase::Pickup => (m.queued, m.seen, true),
            Phase::Prepare => (m.seen, m.prepared, false),
            Phase::Parked => (m.prepared, m.admitted, false),
            Phase::Sandbox => (m.admitted, m.container, false),
            Phase::Register => (m.container, m.started, true),
            Phase::Job => (m.started, m.completed, false),
            Phase::Teardown => (m.completed, m.finished, true),
        }
    }
}

/// One phase's length. `ms` is `None` when either boundary was not measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PhaseSpan {
    pub phase: Phase,
    pub ms: Option<u64>,
    /// The phase crosses clocks and came out negative, so it reads as zero.
    pub skewed: bool,
}

impl Marks {
    /// Every phase in order. A phase that crosses clocks and comes out negative is
    /// zero and `skewed`; one on a single clock that does is a mark faber got
    /// wrong, and reads as not measured rather than as a number.
    pub fn phases(&self) -> [PhaseSpan; 7] {
        Phase::ALL.map(|phase| {
            let (from, to, crosses) = phase.span(self);
            let (ms, skewed) = match (from, to) {
                (Some(a), Some(b)) if b >= a => (Some(b - a), false),
                (Some(_), Some(_)) if crosses => (Some(0), true),
                _ => (None, false),
            };
            PhaseSpan { phase, ms, skewed }
        })
    }

    /// The words of every skewed phase, for the `outcome` line. `None` when none
    /// is.
    pub fn skewed(&self) -> Option<Vec<String>> {
        let words: Vec<String> = self
            .phases()
            .iter()
            .filter(|p| p.skewed)
            .map(|p| p.phase.word().to_string())
            .collect();
        (!words.is_empty()).then_some(words)
    }
}

/// The fields ADR-0056 adds to a run's `outcome` audit line, flattened into it.
/// Every one is omitted when unknown: "not measured" and zero are never the same
/// value. A macOS run carries the marks and steps only; it has no cgroups.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    /// When GitHub queued the job that ran (its `created_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_ms: Option<u64>,
    /// When faber's poll took the job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen_ms: Option<u64>,
    /// When the claim began waiting for admission, its image resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_ms: Option<u64>,
    /// When admission let it through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_ms: Option<u64>,
    /// When the runner container started (on a macOS VM: when the runner began
    /// inside the booted clone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_ms: Option<u64>,
    /// When GitHub dispatched the job to the runner, and when it finished it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_started_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_completed_ms: Option<u64>,
    /// The phases that crossed clocks and came out negative ([`Marks::skewed`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skewed: Option<Vec<String>>,
    /// GitHub's steps, once a read saw the job completed. Absent, never empty,
    /// when no read did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<StepRecord>>,
    /// The CPU share the ledger debited for the run, in millicores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_reserved_mcpu: Option<u64>,
    /// CPU time each container lost to its own quota (`cpu.stat`
    /// `throttled_usec`), as of the last sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_throttled_runner_usec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_throttled_dind_usec: Option<u64>,
    /// Block and network bytes per container, as of the last sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_runner: Option<IoBytes>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_dind: Option<IoBytes>,
}

impl Footprint {
    /// The run's boundaries, with `finished_ms` (the line's own `ts_ms`) as the
    /// last.
    pub fn marks(&self, finished_ms: u64) -> Marks {
        Marks {
            queued: self.queued_ms,
            seen: self.seen_ms,
            prepared: self.prepared_ms,
            admitted: self.admitted_ms,
            container: self.container_ms,
            started: self.job_started_ms,
            completed: self.job_completed_ms,
            finished: Some(finished_ms),
        }
    }
}

/// Block and network bytes one container moved, from `docker stats`. The
/// counters only grow, so the last reading is the value; a reading taken before
/// the container exited makes each a lower bound (ADR-0056 §4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoBytes {
    pub block_read: u64,
    pub block_written: u64,
    pub net_received: u64,
    pub net_sent: u64,
}

impl IoBytes {
    /// Fold a later reading in, field by field, so a reading that went backwards
    /// (a counter reset) never lowers what was already seen.
    pub fn latch(self, later: IoBytes) -> IoBytes {
        IoBytes {
            block_read: self.block_read.max(later.block_read),
            block_written: self.block_written.max(later.block_written),
            net_received: self.net_received.max(later.net_received),
            net_sent: self.net_sent.max(later.net_sent),
        }
    }

    /// Everything the container moved, both directions and both kinds.
    pub fn total(&self) -> u64 {
        self.block_read
            .saturating_add(self.block_written)
            .saturating_add(self.net_received)
            .saturating_add(self.net_sent)
    }
}

/// Parse one `docker stats` I/O pair (`1.2MB / 3.4kB`) into bytes. Docker writes
/// these in decimal units. `None` for `--` or anything unparsable.
pub fn parse_io_pair(field: &str) -> Option<(u64, u64)> {
    let (a, b) = field.split_once('/')?;
    Some((parse_io_bytes(a)?, parse_io_bytes(b)?))
}

fn parse_io_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = text.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let scale = match unit.to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(((value * scale).round() as u64).min(crate::untrusted::MAX_IO_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str) -> Bucket {
        bucket_of(name)
    }

    #[test]
    fn post_steps_are_teardown_even_after_a_setup_action() {
        assert_eq!(step("Post Run actions/checkout@v4"), Bucket::Teardown);
        assert_eq!(step("Post Checkout"), Bucket::Teardown);
    }

    #[test]
    fn stop_containers_is_teardown() {
        assert_eq!(step("Stop containers"), Bucket::Teardown);
    }

    #[test]
    fn complete_job_is_teardown() {
        assert_eq!(step("Complete job"), Bucket::Teardown);
    }

    #[test]
    fn set_up_job_is_setup() {
        assert_eq!(step("Set up job"), Bucket::Setup);
    }

    #[test]
    fn initialize_containers_is_setup() {
        assert_eq!(step("Initialize containers"), Bucket::Setup);
    }

    #[test]
    fn checkout_action_is_setup() {
        assert_eq!(step("Run actions/checkout@v4"), Bucket::Setup);
    }

    #[test]
    fn setup_actions_are_setup() {
        assert_eq!(step("Run actions/setup-node@v4"), Bucket::Setup);
        assert_eq!(step("Run actions/setup-python@v5"), Bucket::Setup);
    }

    #[test]
    fn cache_action_is_setup() {
        assert_eq!(step("Run actions/cache@v4"), Bucket::Setup);
    }

    #[test]
    fn cache_restore_action_is_setup() {
        assert_eq!(step("Run actions/cache/restore@v4"), Bucket::Setup);
    }

    #[test]
    fn npm_ci_is_setup() {
        assert_eq!(step("Run npm ci"), Bucket::Setup);
        assert_eq!(step("Run npm ci --prefer-offline"), Bucket::Setup);
    }

    #[test]
    fn pnpm_install_is_setup() {
        assert_eq!(step("Run pnpm install --frozen-lockfile"), Bucket::Setup);
    }

    #[test]
    fn yarn_install_is_setup() {
        assert_eq!(step("Run yarn install"), Bucket::Setup);
    }

    #[test]
    fn pip_install_is_setup() {
        assert_eq!(step("Run pip install -r requirements.txt"), Bucket::Setup);
    }

    #[test]
    fn uv_sync_is_setup() {
        assert_eq!(step("Run uv sync"), Bucket::Setup);
    }

    #[test]
    fn cargo_fetch_is_setup() {
        assert_eq!(step("Run cargo fetch"), Bucket::Setup);
    }

    #[test]
    fn go_mod_download_is_setup() {
        assert_eq!(step("Run go mod download"), Bucket::Setup);
    }

    #[test]
    fn bundle_install_is_setup() {
        assert_eq!(step("Run bundle install"), Bucket::Setup);
    }

    #[test]
    fn anything_else_is_a_job_step() {
        // An install hidden in an author's name is not guessed at.
        assert_eq!(step("Install dependencies"), Bucket::Steps);
        assert_eq!(step("Run npm test"), Bucket::Steps);
        // A command that merely starts with a listed one is not it.
        assert_eq!(step("Run npm cinema"), Bucket::Steps);
        assert_eq!(step("Run actions/checkout-extra@v1"), Bucket::Steps);
    }

    #[test]
    fn every_table_row_is_reachable() {
        // A row shadowed by an earlier one would be dead: each must win for a
        // name built from its own pattern.
        for (rule, bucket) in TABLE {
            let name = match rule {
                Rule::Exact(s) => s.to_string(),
                Rule::Prefix(s) => format!("{s}x"),
                Rule::Command(c) => format!("Run {c}"),
            };
            assert_eq!(bucket_of(&name), *bucket, "{name}");
        }
    }

    fn marks() -> Marks {
        Marks {
            queued: Some(1_000),
            seen: Some(21_000),
            prepared: Some(22_000),
            admitted: Some(32_000),
            container: Some(40_000),
            started: Some(45_000),
            completed: Some(105_000),
            finished: Some(110_000),
        }
    }

    #[test]
    fn phases_are_the_gaps_between_marks() {
        let ms: Vec<Option<u64>> = marks().phases().iter().map(|p| p.ms).collect();
        assert_eq!(
            ms,
            [20_000, 1_000, 10_000, 8_000, 5_000, 60_000, 5_000].map(Some)
        );
    }

    #[test]
    fn a_missing_mark_is_not_measured_never_zero() {
        let m = Marks {
            container: None,
            ..marks()
        };
        let p = m.phases();
        assert_eq!(p[3].ms, None, "sandbox");
        assert_eq!(p[4].ms, None, "register");
        assert_eq!(p[2].ms, Some(10_000), "parked is untouched");
    }

    #[test]
    fn a_negative_phase_across_clocks_is_zero_and_skewed() {
        // GitHub's clock says it queued after faber saw it.
        let m = Marks {
            queued: Some(22_000),
            ..marks()
        };
        let pickup = m.phases()[0];
        assert_eq!((pickup.ms, pickup.skewed), (Some(0), true));
        assert_eq!(m.skewed(), Some(vec!["pickup".to_string()]));
        assert_eq!(marks().skewed(), None);
    }

    #[test]
    fn a_negative_phase_on_one_clock_is_not_measured() {
        let m = Marks {
            admitted: Some(21_500),
            ..marks()
        };
        let parked = m.phases()[2];
        assert_eq!((parked.ms, parked.skewed), (None, false));
    }

    #[test]
    fn docker_io_pairs_parse_to_bytes() {
        assert_eq!(parse_io_pair("1.2MB / 3.4kB"), Some((1_200_000, 3_400)));
        assert_eq!(parse_io_pair("0B / 0B"), Some((0, 0)));
        assert_eq!(parse_io_pair("2GB / 512B"), Some((2_000_000_000, 512)));
        assert_eq!(parse_io_pair("--"), None);
        assert_eq!(parse_io_pair("NaNMB / 1B"), None);
        // A figure past the bound is held to it.
        assert_eq!(
            parse_io_pair("999999999TB / 1B"),
            Some((crate::untrusted::MAX_IO_BYTES, 1))
        );
    }

    #[test]
    fn io_latch_keeps_the_highest_of_each_counter() {
        let a = IoBytes {
            block_read: 10,
            block_written: 5,
            net_received: 7,
            net_sent: 1,
        };
        let b = IoBytes {
            block_read: 8,
            block_written: 9,
            net_received: 7,
            net_sent: 3,
        };
        assert_eq!(
            a.latch(b),
            IoBytes {
                block_read: 10,
                block_written: 9,
                net_received: 7,
                net_sent: 3
            }
        );
        assert_eq!(a.latch(b).total(), 29);
    }

    fn gh_step(name: &str, started: &str, completed: &str) -> Step {
        serde_json::from_value(serde_json::json!({
            "name": name, "status": "completed", "number": 1, "conclusion": "success",
            "started_at": started, "completed_at": completed,
        }))
        .unwrap()
    }

    #[test]
    fn job_times_read_steps_only_once_completed() {
        let mut job: Job = serde_json::from_value(serde_json::json!({
            "id": 7, "name": "build", "status": "in_progress",
            "created_at": "2026-08-25T18:06:00Z",
            "started_at": "2026-08-25T18:06:14Z",
            "steps": [],
        }))
        .unwrap();
        let live = JobTimes::of(&job);
        assert_eq!(live.steps, None);
        assert_eq!(live.completed_ms, None);
        assert!(live.queued_ms.is_some() && live.started_ms.is_some());
        assert!(!live.is_complete());

        job.status = "completed".into();
        job.completed_at = Some("2026-08-25T18:07:00Z".into());
        job.steps = Some(vec![gh_step(
            "Run npm ci",
            "2026-08-25T18:06:15Z",
            "2026-08-25T18:06:45Z",
        )]);
        let done = JobTimes::of(&job);
        assert!(done.is_complete());
        let steps = done.steps.unwrap();
        assert_eq!(steps[0].bucket, Bucket::Setup);
        assert_eq!(steps[0].duration_ms(), Some(30_000));
    }
}
