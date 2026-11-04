//! The audit log — an append-only JSONL record of every decision faber makes and
//! every runner outcome. One line per event at `~/Library/Logs/faber/audit.jsonl`.
//! It is faber's source of truth for observability (the dashboard reads it) and a
//! durable, replayable trail ("prove all of repo X's CI ran on my box").

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::docker::CpuUsage;

/// Milliseconds since the Unix epoch (wall clock).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One audit line. `kind` is `"decision"` (a gate verdict), `"outcome"` (a
/// finished runner), `"drift"` (a runner-image drift check), `"retry"` (an
/// auto-re-run of a faber-caused failure), `"fault"` (faber's own pipeline
/// failing — see [`Record::fault`]), `"host_oom"` (a kill by the VM-wide OOM
/// killer rather than by a container's own cap — see [`Record::host_oom`]),
/// `"freeze"` (the critical disk floor or the memory freeze
/// freezing or thawing a running job — see [`Record::freeze`]), `"reserve"` (the
/// ADR-0033 p75 fallback tripping or re-arming — see [`Record::reserve`]), `"mint"` (the
/// name of a runner faber is about to register — see [`Record::mint`]), `"control"`
/// (a trust-widening settings change through the control API, allowed or refused —
/// see [`Record::control`]), `"grant"` (a change to the VM wall's grants, made
/// as root — see [`Record::grant`]), or `"waste"` (a claim found likely wasted —
/// see [`Record::waste`]). Fields not relevant to a kind are omitted.
///
/// Every field is optional but `ts_ms` / `kind` / `repo` / `job_id`, so build one
/// with [`Record::new`] and struct-update only what the kind actually carries —
/// that way a new field costs one line here, not one line in every construction
/// site.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub ts_ms: u64,
    pub kind: &'static str,
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    pub job_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// The job GitHub actually dispatched to this runner, when it differs from
    /// `job_id` (`outcome` records only). JIT runners are not job-pinned, so the job
    /// a runner was minted for and the one it executed routinely differ; `class`
    /// below describes *this* job, not `job_id`. Absent when they agree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_job_id: Option<u64>,
    /// Autolimiter per-class key (`outcome` records only) — the learning substrate,
    /// keyed to the job that actually *ran* (see `executed_job_id`) so a restart
    /// replays each sample onto the class it truly belongs to.
    ///
    /// Present **only when the sample was fed to the live learner**. Its absence is
    /// what keeps replay consistent with live learning: a lifetime-cap kill, an idle
    /// reclaim, an infra failure, or a run whose executing job couldn't be identified
    /// ran nothing representative, and must not be re-learned on the next startup
    /// either. `label` is still recorded in those cases, for the dashboard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// Human job label (`repo · workflow · job`) — on an `outcome`, the class the
    /// sample belongs to (so the dashboard can name it after a restart); on a
    /// `decision`, the job the verdict was about. Same shape either way, so one
    /// grep finds a job's whole story.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Observed runner-container memory peak (MiB).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_peak_runner_mib: Option<u64>,
    /// Observed dind-container memory peak (MiB).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_peak_dind_mib: Option<u64>,
    /// Whether *either* container was OOM-killed (a censored peak → force-bump).
    ///
    /// Kept as-is now that `oom_runner`/`oom_dind` record the two separately: it is
    /// the field `replay_outcomes` reads and the one every historical line carries,
    /// so narrowing it would silently change what a restart re-learns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom: Option<bool>,
    /// Whether the **runner** container specifically was OOM-killed.
    ///
    /// Split out from `oom` because pairing one collapsed boolean with two separate
    /// peak fields made the log unreadable — and, while dind's OOM signal was
    /// hierarchical, actively self-contradictory: lines that reported `oom: true`
    /// alongside a dind peak of, say, 150 MiB under a 600 MiB cap were
    /// recording a *nested* container's kill with no way to tell. Which container
    /// hit its ceiling is the whole question when reading a cap back to its cause.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom_runner: Option<bool>,
    /// Whether the **dind** container specifically was OOM-killed — its own cgroup's
    /// non-hierarchical counter, not the flag its nested containers can raise. See
    /// `docker::local_oom_tally`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oom_dind: Option<bool>,
    /// The runner container's highest CPU use over one sampling interval, in
    /// millicores (`outcome` records only; ADR-0019). Present whenever the CPU was
    /// read, zero included — a job that used nothing is a sample, not a gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_peak_runner_mcpu: Option<u64>,
    /// The dind container's CPU peak, in millicores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_peak_dind_mcpu: Option<u64>,
    /// The runner container's average CPU use across the sampled run, in
    /// millicores. Observability only; the learner reads peaks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_avg_runner_mcpu: Option<u64>,
    /// The dind container's average CPU use, in millicores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_avg_dind_mcpu: Option<u64>,
    /// The share of CFS periods in which the runner hit its `--cpus` quota. Omitted
    /// when zero — never throttled, or no quota to hit — which is what replay reads
    /// an absent value as.
    ///
    /// Recorded next to the peak because the peak alone lies about a throttled run:
    /// it reads the quota back as the job's want. With both, replay censors the
    /// sample exactly as the live learner did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_throttled_runner: Option<f64>,
    /// The share of CFS periods in which dind was throttled (omitted when zero).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_throttled_dind: Option<f64>,
    /// The `--cpus` quota the runner ran under, in millicores — the value a
    /// throttled sample is censored at. Omitted when there was none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_quota_runner_mcpu: Option<u64>,
    /// The version baked into the runner image (`drift` records only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baked: Option<String>,
    /// The version the repo's lockfile pins (`drift` records only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
    /// The resolved drift mode (`off` / `surface` / `gate`) (`drift` records only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// What faber did about the drift check (`drift` records only) — e.g.
    /// `surfaced`, `in_sync`, `skipped`, `indeterminate`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The routing profile that served this job (`decision` records with a
    /// `provision` verdict, and every `outcome`). Names the backend that ran it,
    /// which is the first thing a failure reading needs — a macOS VM fault and a
    /// Docker fault have nothing in common but the word.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Whether this job was handed a Docker daemon of its own — `granted` or
    /// `denied` (`outcome` records only; absent for a macOS VM job, which has
    /// no containers, and for a claim reclaimed before any runner existed).
    ///
    /// The widest privilege faber grants, recorded per job rather than per config,
    /// because the config is mutable and the log is not: "was this job able to
    /// reach a privileged daemon?" has to stay answerable for a job that already
    /// ran. It records the *fact*, so there is no `unstated` here — whether an
    /// operator chose the grant or merely inherited it is a property of the config,
    /// which the dashboard shows and startup warns about. See
    /// [`crate::docker::DindGrant`] and issue #43.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dind: Option<String>,
    /// The runner name faber minted (`faber-<job>-<ts>`). The join key between
    /// this log, GitHub's runner list, and the host's `faber-runner-*` /
    /// `faber-dind-*` containers — without it, correlating a line here with
    /// anything outside faber is guesswork.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// The image **tag** faber started this job's runner container from
    /// (`outcome` records only; `None` for a macOS VM job, which has no
    /// runner image).
    ///
    /// Once ADR-0008 made the image a function of the job, "which image ran this"
    /// stopped being answerable from the config: two jobs of one repo can be
    /// served by the profile's base and by a per-version variant in the same
    /// minute. Recorded from what faber actually passed to `docker run`, after
    /// drift healing and the ADR-0009 pool decision have both had their say — so
    /// it is the tag that ran, not the tag the profile names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The image **content id** (`sha256:…`) the runner container actually ran, read
    /// back off the container itself (`outcome` records only).
    ///
    /// Not redundant with `image`, and the difference is the entire point: a tag is
    /// a mutable pointer. Rebuild or retag a base and every line that recorded only
    /// the tag becomes a claim about content nobody can reconstruct — the log says
    /// `my-runner:2.335.1` and the reader has no way to know *which*
    /// `2.335.1`. This is read from `{{.Image}}` on the exited container, so it
    /// says what ran even when the tag has since moved.
    ///
    /// It is what makes the ADR-0001 class of failure — a job dying on a toolchain
    /// version its image doesn't carry — diagnosable after the fact instead of by
    /// inspecting a host hours later and hoping nothing moved. `None` when the
    /// container never existed (an infra error before launch) or the inspect
    /// failed; the tag above is still recorded in that case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_id: Option<String>,
    /// How long the claim sat parked before the autolimiter admitted it
    /// (`outcome` records only). `duration_ms` is claim → finish, so a job that
    /// looks slow is either a slow job or a starved one, and only this
    /// distinguishes them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    /// The pull request a `waste` record is about (ADR-0063).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// The gate that held the claim longest before it was admitted: `memory`,
    /// `vm-memory`, `cpu`, `ceiling`, `disk`, `machine-pressure`, `swap`, … (the
    /// keys of [`crate::limiter::WaitView`]; `outcome` records only, and only for a
    /// claim that parked at all). Of everything that limits admission, this is the
    /// one that bound this job (issue #85).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_gate: Option<String>,
    /// How long, in milliseconds, the claim waited at each gate it parked at —
    /// `wait_gate` is the largest of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_gates: Option<std::collections::BTreeMap<String, u64>>,
    /// The provision attempt this run was (1-based; `outcome` records only). A
    /// job that keeps coming back is a different problem from one that failed
    /// once, and the throttle that produces the repeats is otherwise invisible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// **Why**, in one line, for anything that is not a plain success — the
    /// backend's error chain for an `infra_error`, the seam a reclaim landed at,
    /// the cap a timeout blew, the exit status of a failed job, the transport
    /// error behind a `fault`.
    ///
    /// This is the field that makes the log answer "what went wrong" on its own.
    /// Before it, `result` said `infra_error` and the actual cause existed only
    /// as a `tracing` line on stderr — a different file, different format, no
    /// shared key, and gone entirely once the log rotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// How the runner's life ended, as one stable word (`outcome` records only;
    /// [`crate::retry::EndKind::word`]): `oom`, `lifetime_cap`, `reclaimed`,
    /// `lost` (the container vanished without an exit), `exited` (status 0), or
    /// `failed`. The auto-retry sweep (ADR-0003, amendment 2026-10-18) judges a
    /// failed job by it. Absent on lines older than the field, which read as no
    /// record: never a reason to retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// Why, as one stable word: on a `freeze` line, what the freeze was for —
    /// `disk` (ADR-0032) or `memory` (ADR-0033 §3), which is what lets each loop
    /// thaw only its own across a restart; on a `reserve` line, what tripped the
    /// fallback (`freeze`, `host_oom`, `probe`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The cool-off a fallback trip set, in ms (`reserve` lines only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooloff_ms: Option<u64>,
    /// When that cool-off ends and the host re-arms (`reserve` lines only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    /// Armed time — time with p75 in force — since the last fallback trip, in ms
    /// (`reserve` lines only: a trip says how much preceded it, an `armed`
    /// checkpoint how much has accrued since).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub armed_ms: Option<u64>,
    /// How the caller reached the control API: `socket` or `session` (`control`
    /// lines only; ADR-0028).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// The caller's class: `refused`, `operator` or `present` (`control` lines
    /// only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_class: Option<String>,
    /// The socket peer's uid, as the kernel reported it (`control` lines over the
    /// socket only; a dashboard session has none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// The API route the change came through (`control` lines only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Every trust-widening change in the request, key and sentence (`control`
    /// lines only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub widenings: Vec<crate::control::Widening>,
    /// A grant's destination network, `ADDRESS/PREFIX`; on a `dns` change, the
    /// new resolver list (`grant` lines only; ADR-0026).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// A grant's port and protocol, `5432/tcp` (`grant` lines only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    /// The services the runner had, `[postgres-16, redis]` (`outcome` records
    /// only; ADR-0046 §9). Absent when it had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<String>>,
    /// Where the run's time and resources went (`outcome` records only;
    /// ADR-0056). Flattened, so each of its fields is a field of the line.
    #[serde(flatten)]
    pub footprint: crate::footprint::Footprint,
}

impl Record {
    /// An empty record of `kind` for `repo` / `job_id`, stamped now. Every other
    /// field is `None` (and so unserialized), for the caller to fill by
    /// struct-update:
    ///
    /// ```ignore
    /// Record { verdict: Some(v), ..Record::new("decision", repo, job.id) }
    /// ```
    ///
    /// `job_id` is `0` for the records that describe faber itself rather than one
    /// job — see [`Record::fault`].
    pub fn new(kind: &'static str, repo: &str, job_id: u64) -> Self {
        Self {
            ts_ms: now_ms(),
            kind,
            repo: repo.to_string(),
            run_id: None,
            job_id,
            event: None,
            actor: None,
            verdict: None,
            result: None,
            duration_ms: None,
            executed_job_id: None,
            end: None,
            class: None,
            label: None,
            mem_peak_runner_mib: None,
            mem_peak_dind_mib: None,
            oom: None,
            oom_runner: None,
            oom_dind: None,
            cpu_peak_runner_mcpu: None,
            cpu_peak_dind_mcpu: None,
            cpu_avg_runner_mcpu: None,
            cpu_avg_dind_mcpu: None,
            cpu_throttled_runner: None,
            cpu_throttled_dind: None,
            cpu_quota_runner_mcpu: None,
            baked: None,
            pinned: None,
            mode: None,
            action: None,
            profile: None,
            dind: None,
            runner: None,
            image: None,
            image_id: None,
            wait_ms: None,
            pr: None,
            wait_gate: None,
            wait_gates: None,
            attempt: None,
            cause: None,
            reason: None,
            cooloff_ms: None,
            until_ms: None,
            armed_ms: None,
            caller: None,
            caller_class: None,
            uid: None,
            route: None,
            widenings: Vec::new(),
            to: None,
            port: None,
            services: None,
            footprint: Default::default(),
        }
    }

    /// Build a `drift` record (ADR-0001). `baked`/`pinned` are present only when a
    /// version compare actually happened; `mode` only when a policy was resolved.
    /// `action` is the always-present summary of what the check did.
    pub fn drift(
        repo: &str,
        run_id: u64,
        job_id: u64,
        baked: Option<String>,
        pinned: Option<String>,
        mode: Option<String>,
        action: String,
    ) -> Self {
        Self {
            run_id: Some(run_id),
            baked,
            pinned,
            mode,
            action: Some(action),
            ..Self::new("drift", repo, job_id)
        }
    }

    /// Build a `waste` record (ADR-0063 §3): a check of a claim about to be
    /// admitted found it likely wasted. `cause` is every signal that fired, the
    /// strongest first; `action` is what faber did, `record`, `demote` or `skip`;
    /// `wait_ms` is how long the claim had been parked.
    #[allow(clippy::too_many_arguments)]
    pub fn waste(
        repo: &str,
        run_id: u64,
        job_id: u64,
        pr: u64,
        signals: String,
        action: &str,
        wait_ms: u64,
        label: String,
    ) -> Self {
        Self {
            run_id: Some(run_id),
            pr: Some(pr),
            cause: Some(signals),
            action: Some(action.to_string()),
            wait_ms: Some(wait_ms),
            label: Some(label),
            ..Self::new("waste", repo, job_id)
        }
    }

    /// Build a `retry` record (ADR-0003): faber auto-re-ran `job_id` (in `run_id`)
    /// because its own runner killed/lost it. `mode` is `all` (whole run) or
    /// `surgical` (this job only); `result` carries the always-`runner_lost` cause,
    /// and `cause` the evidence: the runner and how faber's record says it ended.
    pub fn retry(repo: &str, run_id: u64, job_id: u64, mode: &str, evidence: String) -> Self {
        Self {
            run_id: Some(run_id),
            result: Some("runner_lost".to_string()),
            mode: Some(mode.to_string()),
            action: Some("retried".to_string()),
            cause: Some(evidence),
            ..Self::new("retry", repo, job_id)
        }
    }

    /// Build a `fault` record: **faber's own pipeline** failed, as distinct from a
    /// job that failed. `scope` says which part broke (`discovery`, a repo scan,
    /// …), `cause` is the error, and `action` is `failed` or `recovered`.
    ///
    /// These are the failures that used to exist only as a `tracing` line: the
    /// daemon stops being able to see GitHub overnight, provisions nothing for
    /// some minutes, and recovers — and the audit log, read the next morning,
    /// showed an unexplained gap. A fault line at each edge of the gap is what
    /// makes the gap itself readable. `repo` is the repo whose scan failed, or
    /// `-` for a fault that is not about one repo; `job_id` is `0` throughout,
    /// since no single job is at fault.
    pub fn fault(repo: &str, scope: &str, action: &str, cause: Option<String>) -> Self {
        Self {
            result: Some(scope.to_string()),
            action: Some(action.to_string()),
            cause,
            ..Self::new("fault", repo, 0)
        }
    }

    /// Build a `freeze` record (ADR-0032 §2): with the Docker volume below the
    /// critical floor, faber froze `job_id`'s containers with `docker pause`
    /// (`action` is `frozen`), or, with room again, thawed them (`thawed`).
    /// `runner` is the runner it froze, and `cause` the free space that decided
    /// it.
    ///
    /// Its own kind rather than an `outcome`: the job has not finished, and may
    /// well finish once thawed. If it does not, its `outcome` follows, and this
    /// line is what explains it.
    ///
    /// `reason` is what the freeze is for: `disk` or `memory` (ADR-0033 §3).
    pub fn freeze(
        repo: &str,
        job_id: u64,
        runner: &str,
        action: &str,
        reason: &str,
        cause: String,
    ) -> Self {
        Self {
            action: Some(action.to_string()),
            runner: Some(runner.to_string()),
            reason: Some(reason.to_string()),
            cause: Some(cause),
            ..Self::new("freeze", repo, job_id)
        }
    }

    /// Build a `reserve` record (ADR-0033 §4): the p75 fallback tripped
    /// (`action` is `trip`, `reason` what tripped it, with the cool-off it set and
    /// when it ends), re-armed (`rearm`), or checkpointed its armed time
    /// (`armed`). The startup replay rebuilds the fallback from these lines.
    pub fn reserve(
        action: &str,
        reason: &str,
        cooloff_ms: u64,
        until_ms: u64,
        armed_ms: Option<u64>,
        cause: String,
    ) -> Self {
        Self {
            action: Some(action.to_string()),
            reason: Some(reason.to_string()),
            cooloff_ms: Some(cooloff_ms),
            until_ms: Some(until_ms),
            armed_ms,
            cause: Some(cause),
            ..Self::new(crate::reserve::AUDIT_KIND, "", 0)
        }
    }

    /// Build a `host_oom` record (ADR-0033 §5): a container's cgroup counters showed
    /// a process killed (`oom_kill` rose) with no memory limit in its subtree
    /// breached (`oom` did not), so the kill came from the VM-wide OOM killer, not
    /// from the container's own cap. `runner` is the runner whose container it was
    /// and `cause` names the counters read.
    ///
    /// Observability only in rollout step 1: the learner still treats the run as a
    /// censored OOM, exactly as before, and `oom_runner` / `oom_dind` here say
    /// whether it did. Its own kind rather than an `outcome`, which the same run
    /// also writes.
    pub fn host_oom(
        repo: &str,
        job_id: u64,
        runner: &str,
        label: Option<String>,
        oom_runner: bool,
        oom_dind: bool,
        cause: String,
    ) -> Self {
        Self {
            runner: Some(runner.to_string()),
            label,
            oom_runner: Some(oom_runner),
            oom_dind: Some(oom_dind),
            cause: Some(cause),
            ..Self::new(HOST_OOM, repo, job_id)
        }
    }

    /// Build a `mint` record: faber chose `runner` as the name of the runner it is
    /// about to register on `repo` for `job_id`, under `profile`.
    ///
    /// Written before the runner registers, by the process that mints it, so a
    /// later process can tell this host's own runners from any other (#105). A
    /// restart rebuilds its claims from the runner containers on the host
    /// (ADR-0011), and those report whatever the Docker daemon says. This line is
    /// what faber wrote, so it is the record a GitHub write about an adopted runner
    /// must match: see [`crate::docker::MintedRunner`].
    ///
    /// faber's own. The gap this once had belonged to the Docker Desktop backend,
    /// which shared `/Users` into its VM by default, so root there could append a
    /// forged line to this log (moot only because the same share exposed faber's
    /// binary and config). ADR-0026's VMs close it on a set-up host: none shares a
    /// host folder (`checkWall`, in `launcher/Sources/Wall.swift`, refuses any
    /// directory-sharing device), so no guest can write to this log through one.
    pub fn mint(repo: &str, job_id: u64, runner: &str, profile: &str) -> Self {
        Self {
            runner: Some(runner.to_string()),
            profile: Some(profile.to_string()),
            ..Self::new(MINT, repo, job_id)
        }
    }
}

impl Record {
    /// Build a `github_write` record (ADR-0027): a write faber made to GitHub
    /// with the operator's token. On GitHub the token acts as the operator, so
    /// this line is where faber's own writes are told apart from theirs.
    ///
    /// `action` names the write (`mint_runner`, `delete_runner`,
    /// `withdraw_runner`, `rerun_job`, `rerun_failed_jobs`, `commit_status`),
    /// `reason` is why the caller made it, and `result` is `ok`, the HTTP status
    /// GitHub answered, or the kind of transport failure. `runner` names the
    /// runner a runner write was about. Never the token, and never the JIT
    /// config a mint returns.
    pub fn github_write(
        repo: &str,
        action: &str,
        reason: &str,
        result: &str,
        runner: Option<&str>,
    ) -> Self {
        Self {
            action: Some(action.to_string()),
            reason: Some(reason.to_string()),
            result: Some(result.to_string()),
            runner: runner.map(str::to_string),
            ..Self::new(GITHUB_WRITE, repo, 0)
        }
    }

    /// Build a `control` record (ADR-0028): a trust-widening change reached the
    /// control API through `route`, from `caller` (`None` when the request
    /// carried no caller at all), and was `allowed` or refused (`action`).
    pub fn control(
        caller: Option<crate::control::Caller>,
        route: &str,
        widenings: Vec<crate::control::Widening>,
        allowed: bool,
    ) -> Self {
        Self {
            action: Some(if allowed { "allowed" } else { "refused" }.to_string()),
            caller: Some(caller.map_or("unknown", |c| c.via()).to_string()),
            caller_class: Some(
                caller
                    .map_or(crate::control::Class::Refused, |c| c.class())
                    .as_str()
                    .to_string(),
            ),
            uid: caller.and_then(|c| c.uid()),
            route: Some(route.to_string()),
            widenings,
            ..Self::new(CONTROL, "", 0)
        }
    }
}

impl Record {
    /// Build a `grant` record (ADR-0026): root changed the grants file through
    /// `sudo faber grant`, or re-rendered from it (`action` is `add`, `remove`,
    /// `dns` or `apply`). `actor` is the operator `sudo` names, validated.
    ///
    /// Written by root into `_faber`'s log, so the dashboard's history shows
    /// who opened what, beside the settings changes `control` lines record.
    pub fn grant(
        action: &str,
        profile: Option<&str>,
        to: Option<String>,
        port: Option<String>,
        actor: &str,
    ) -> Self {
        Self {
            action: Some(action.to_string()),
            profile: profile.map(str::to_string),
            to,
            port,
            actor: Some(actor.to_string()),
            ..Self::new(GRANT, "", 0)
        }
    }
}

/// The `kind` of a [`Record::grant`] line.
pub const GRANT: &str = "grant";

/// The `kind` of a [`Record::github_write`] line.
pub const GITHUB_WRITE: &str = "github_write";
/// The `kind` of a [`Record::control`] line.
pub const CONTROL: &str = "control";

/// The `kind` of a [`Record::host_oom`] line.
pub const HOST_OOM: &str = "host_oom";

/// The `kind` of a [`Record::mint`] line.
pub const MINT: &str = "mint";

/// One replayed `outcome` record carrying autolimiter learning data. Rebuilt into
/// the per-class rolling windows on startup so caps survive a restart.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OutcomeReplay {
    pub class: String,
    /// The repo the sample came from, so a restart can rebuild not just each
    /// class's cap but *where* it can be dispatched. Absent on lines old enough to
    /// predate the field.
    #[serde(default)]
    pub repo: String,
    /// The routing profile that served the run — the key a sizing cohort is
    /// bucketed by. `None` on lines written before it was recorded.
    #[serde(default)]
    pub profile: Option<String>,
    /// When the run finished, so a replayed sighting ages out on the same clock a
    /// live one does.
    #[serde(default)]
    pub ts_ms: u64,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub mem_peak_runner_mib: Option<u64>,
    #[serde(default)]
    pub mem_peak_dind_mib: Option<u64>,
    #[serde(default)]
    pub oom: Option<bool>,
    /// Which container was OOM-killed, where the line says (ADR-0033 §1: the
    /// replayed window needs each sample's censored flag). Absent on lines older
    /// than the split; [`OutcomeReplay::censored`] falls back to `oom` for those.
    #[serde(default)]
    pub oom_runner: Option<bool>,
    #[serde(default)]
    pub oom_dind: Option<bool>,
    /// The CPU learning inputs (ADR-0019). Absent on lines older than the fields.
    #[serde(default)]
    pub cpu_peak_runner_mcpu: Option<u64>,
    #[serde(default)]
    pub cpu_peak_dind_mcpu: Option<u64>,
    #[serde(default)]
    pub cpu_throttled_runner: Option<f64>,
    #[serde(default)]
    pub cpu_throttled_dind: Option<f64>,
    #[serde(default)]
    pub cpu_quota_runner_mcpu: Option<u64>,
}

impl OutcomeReplay {
    /// Whether the (runner, dind) sample on this line was censored by an OOM. A
    /// line from before the split carries only the collapsed `oom`, and counts as
    /// censored for both — the safe reading, since a censored window only keeps a
    /// reservation at its cap.
    pub fn censored(&self) -> (bool, bool) {
        let both = self.oom == Some(true);
        match (self.oom_runner, self.oom_dind) {
            (None, None) => (both, both),
            (r, d) => (r.unwrap_or(false), d.unwrap_or(false)),
        }
    }

    /// The runner's CPU as the live learner saw it, or `None` when it was never
    /// read. An absent throttle ratio is "not throttled", which is why the writer
    /// omits a zero.
    pub fn runner_cpu(&self) -> Option<CpuUsage> {
        replayed_cpu(self.cpu_peak_runner_mcpu, self.cpu_throttled_runner)
    }

    /// The dind container's CPU as the live learner saw it.
    pub fn dind_cpu(&self) -> Option<CpuUsage> {
        replayed_cpu(self.cpu_peak_dind_mcpu, self.cpu_throttled_dind)
    }
}

/// Rebuild the part of a [`CpuUsage`] the learner reads — the peak and the throttle
/// ratio; the average is observability only.
fn replayed_cpu(peak_mcpu: Option<u64>, throttled: Option<f64>) -> Option<CpuUsage> {
    Some(CpuUsage {
        peak_mcpu: peak_mcpu?,
        avg_mcpu: 0,
        throttled_ratio: throttled.unwrap_or(0.0),
    })
}

/// A tolerant, read-side view of one audit line — every field optional, unknown
/// fields ignored.
///
/// Deliberately *not* [`Record`]: the writer's shape is what faber emits today,
/// while a reader has to cope with every line the log already holds, written by
/// every earlier version of faber. Keeping them separate is what lets a field be
/// added to `Record` without the reader rejecting the years of lines that predate
/// it — and what stops a reader's needs leaking back into the append path.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Entry {
    pub ts_ms: u64,
    pub kind: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub job_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_job_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_peak_runner_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_peak_dind_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_runner: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_dind: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooloff_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<Vec<String>>,
}

/// A tolerant view of any audit line, for replay filtering.
#[derive(Debug, Deserialize)]
struct RawLine {
    kind: String,
    #[serde(default)]
    ts_ms: u64,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    mem_peak_runner_mib: Option<u64>,
    #[serde(default)]
    mem_peak_dind_mib: Option<u64>,
    #[serde(default)]
    oom: Option<bool>,
    #[serde(default)]
    oom_runner: Option<bool>,
    #[serde(default)]
    oom_dind: Option<bool>,
    #[serde(default)]
    cpu_peak_runner_mcpu: Option<u64>,
    #[serde(default)]
    cpu_peak_dind_mcpu: Option<u64>,
    #[serde(default)]
    cpu_throttled_runner: Option<f64>,
    #[serde(default)]
    cpu_throttled_dind: Option<f64>,
    #[serde(default)]
    cpu_quota_runner_mcpu: Option<u64>,
}

/// Append-only writer for the audit log.
#[derive(Clone)]
pub struct Audit {
    path: PathBuf,
}

impl Audit {
    /// Open (creating the directory) `~/Library/Logs/faber/audit.jsonl`.
    pub fn open() -> std::io::Result<Self> {
        let dir = crate::platform::log_dir()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME not set"))?;
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            path: dir.join("audit.jsonl"),
        })
    }

    /// The on-disk log path (shown by `status` / the dashboard).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Target an explicit file path, bypassing the default `~/Library/Logs/faber`
    /// location.
    ///
    /// Two callers: a unit test pointing the log at a throwaway dir instead of the
    /// user's real audit trail, and `faber status --log`, which reads a log that
    /// isn't this machine's — a trail copied off the box it was written on is
    /// exactly what you have when the box it was written on is the problem.
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append one record as a JSON line. Best-effort: a logging failure must
    /// never take down a reconcile cycle, so errors are returned for the caller
    /// to log, not propagated into provisioning.
    pub async fn append(&self, record: &Record) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        // tokio::fs::File defers the real write(2) to its blocking pool:
        // `write_all` schedules the write and resolves `Ready` before that
        // pool thread has run it, so a fresh handle dropped right here could
        // leave the just-written line unwritten if the process exits before
        // the blocking write completes, and a synchronous reader could miss
        // it entirely. `flush` drives the pending write to completion (into
        // the OS page cache) before `append` returns, closing both gaps. No
        // `sync_all` (fsync) — this is the hot per-decision append path and
        // power-loss durability isn't a requirement here.
        file.flush().await
    }

    /// Read every line stamped at or after `from_ms`, oldest first.
    ///
    /// Tolerant in exactly the way a post-mortem read has to be: a missing log is
    /// an empty result, not an error (faber may simply never have run here), and a
    /// line that won't parse is skipped rather than aborting the read — a
    /// half-written final line from a `SIGKILL`ed daemon must not cost you the
    /// thousands of good lines above it, which are the ones explaining the kill.
    ///
    /// The whole file is read and filtered rather than seeked into: lines are
    /// append-ordered, so a from-the-end scan would be strictly better, but the log
    /// is one line per decision on a single-owner machine, and a reader that is
    /// simple enough to be obviously correct beats one that is fast on a file this
    /// size.
    pub fn read_since(&self, from_ms: u64) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
            .filter(|e| e.ts_ms >= from_ms)
            .collect()
    }

    /// Read every line of `kind`, oldest first, as tolerantly as
    /// [`Self::read_since`]. A startup replay of a rare kind (`freeze`, `reserve`)
    /// skips the parse of every other line by its `kind` field as faber writes it.
    pub fn read_kind(&self, kind: &str) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let marker = format!("\"kind\":\"{kind}\"");
        text.lines()
            .filter(|line| line.contains(&marker))
            .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
            .filter(|e| e.kind == kind)
            .collect()
    }

    /// Replay the `outcome` records that carry autolimiter learning data, oldest
    /// first. Best-effort: a missing log yields an empty list, and unparsable or
    /// non-outcome lines are silently skipped. Startup-only (rebuilds caps).
    pub fn replay_outcomes(&self) -> Vec<OutcomeReplay> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str::<RawLine>(line).ok())
            .filter(|r| r.kind == "outcome")
            .filter_map(|r| {
                r.class.map(|class| OutcomeReplay {
                    class,
                    repo: r.repo,
                    profile: r.profile,
                    ts_ms: r.ts_ms,
                    label: r.label,
                    mem_peak_runner_mib: r.mem_peak_runner_mib,
                    mem_peak_dind_mib: r.mem_peak_dind_mib,
                    oom: r.oom,
                    oom_runner: r.oom_runner,
                    oom_dind: r.oom_dind,
                    cpu_peak_runner_mcpu: r.cpu_peak_runner_mcpu,
                    cpu_peak_dind_mcpu: r.cpu_peak_dind_mcpu,
                    cpu_throttled_runner: r.cpu_throttled_runner,
                    cpu_throttled_dind: r.cpu_throttled_dind,
                    cpu_quota_runner_mcpu: r.cpu_quota_runner_mcpu,
                })
            })
            .collect()
    }
}

// --- the trust-change notice (ADR-0028 addendum B) ---------------------------

/// A tolerant, read-side view of one `control` line: what [`Record::control`]
/// writes, every field optional. Its own type rather than more fields on
/// [`Entry`], which other readers build field by field.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ControlLine {
    #[serde(default)]
    pub ts_ms: u64,
    #[serde(default)]
    pub kind: String,
    /// `allowed` or `refused`.
    #[serde(default)]
    pub action: Option<String>,
    /// `socket` or `session`.
    #[serde(default)]
    pub caller: Option<String>,
    #[serde(default)]
    pub caller_class: Option<String>,
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub widenings: Vec<WideningLine>,
}

/// One widening as a `control` line records it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WideningLine {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub sentence: String,
}

impl Audit {
    /// Every `control` line stamped at or after `from_ms`, oldest first, as
    /// tolerantly as [`Self::read_since`].
    pub fn read_control_since(&self, from_ms: u64) -> Vec<ControlLine> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let marker = format!("\"kind\":\"{CONTROL}\"");
        text.lines()
            .filter(|line| line.contains(&marker))
            .filter_map(|line| serde_json::from_str::<ControlLine>(line).ok())
            .filter(|l| l.kind == CONTROL && l.ts_ms >= from_ms)
            .collect()
    }
}

/// How long an allowed trust-widening change stays on the notice: 7 days.
pub const TRUST_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// One allowed trust-widening change, as `/api/state`'s `trust_changes` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustChange {
    pub ts_ms: u64,
    pub key: String,
    pub sentence: String,
    /// `socket` or `session`.
    pub via: String,
    /// The socket peer's uid; `null` for a dashboard session.
    pub uid: Option<u32>,
    pub route: String,
}

/// Every trust-widening change allowed in the last [`TRUST_WINDOW_MS`], in
/// memory: seeded at boot from the audit log, appended to as each allowed one is
/// audited. Nothing clears it but time: there is no route to dismiss an entry,
/// so a change made by a process acting as the operator stays in front of the
/// operator for a week (ADR-0028 addendum B).
pub struct TrustChanges {
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    entries: Mutex<Vec<TrustChange>>,
}

impl TrustChanges {
    /// An empty store on `clock` (wall-clock ms).
    pub fn new(clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            clock,
            entries: Mutex::new(Vec::new()),
        }
    }

    /// The store on the wall clock, seeded from `audit`'s allowed `control`
    /// lines of the last 7 days.
    pub fn from_audit(audit: &Audit) -> Self {
        let store = Self::new(Arc::new(now_ms));
        let from = now_ms().saturating_sub(TRUST_WINDOW_MS);
        store.seed(audit.read_control_since(from));
        store
    }

    /// Add the allowed widenings of `lines`, one entry per widening.
    pub fn seed(&self, lines: Vec<ControlLine>) {
        let mut entries = self.entries.lock().unwrap();
        for line in lines {
            if line.action.as_deref() != Some("allowed") {
                continue;
            }
            for w in line.widenings {
                entries.push(TrustChange {
                    ts_ms: line.ts_ms,
                    key: w.key,
                    sentence: w.sentence,
                    via: line.caller.clone().unwrap_or_default(),
                    uid: line.uid,
                    route: line.route.clone().unwrap_or_default(),
                });
            }
        }
    }

    /// Note an allowed change, as [`Record::control`] audits it: `record` must
    /// be an allowed `control` record. Anything else is ignored.
    pub fn record(&self, record: &Record) {
        if record.kind != CONTROL || record.action.as_deref() != Some("allowed") {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        for w in &record.widenings {
            entries.push(TrustChange {
                ts_ms: record.ts_ms,
                key: w.key.clone(),
                sentence: w.sentence.clone(),
                via: record.caller.clone().unwrap_or_default(),
                uid: record.uid,
                route: record.route.clone().unwrap_or_default(),
            });
        }
    }

    /// The changes of the last 7 days, newest first. Older ones are dropped
    /// here, for good.
    pub fn list(&self) -> Vec<TrustChange> {
        let from = (self.clock)().saturating_sub(TRUST_WINDOW_MS);
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|e| e.ts_ms >= from);
        let mut out = entries.clone();
        // Stable, so widenings of one request keep their order.
        out.sort_by_key(|e| std::cmp::Reverse(e.ts_ms));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A throwaway directory removed (with its contents) on drop — the repo's
    /// hand-rolled test-scratch pattern (mirrors `TempDir` in `reconcile.rs` /
    /// `runner.rs`), so this test needs no `tempfile` dep and leaves nothing
    /// behind, even on an unwinding (failed-assertion) path.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "faber-audit-test-{}-{}",
                std::process::id(),
                TMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// A minimal decision record distinguished only by `job_id`. The autolimiter
    /// learning fields (class/label/mem_peak_*/oom) are `None` — irrelevant to
    /// the durability property under test.
    fn rec(job_id: u64) -> Record {
        Record {
            ts_ms: 1,
            run_id: Some(7),
            event: Some("push".to_string()),
            actor: Some("me".to_string()),
            verdict: Some("provision".to_string()),
            ..Record::new("decision", "o/r", job_id)
        }
    }

    /// ADR-0028: a widening through the socket names the uid, class, route and
    /// every change; one through a dashboard session says `session` and has no
    /// uid; a routine line carries none of these fields.
    #[test]
    fn a_grant_line_names_the_change_and_who_made_it() {
        let r = Record::grant(
            "add",
            Some("macos-vm"),
            Some("10.0.0.5/32".into()),
            Some("5432/tcp".into()),
            "operator",
        );
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["kind"], GRANT);
        assert_eq!(json["action"], "add");
        assert_eq!(json["profile"], "macos-vm");
        assert_eq!(json["to"], "10.0.0.5/32");
        assert_eq!(json["port"], "5432/tcp");
        assert_eq!(json["actor"], "operator");
        assert_eq!(json["job_id"], 0);
        // Every other kind leaves the two new fields out.
        let other = serde_json::to_value(Record::new("decision", "o/r", 1)).unwrap();
        assert!(other.get("to").is_none() && other.get("port").is_none());
    }

    #[test]
    fn a_control_line_names_the_caller_the_route_and_every_widening() {
        use crate::control::{Caller, Class, Widening};
        let widening = Widening {
            key: "update.pin".into(),
            sentence: "update.pin none -> 0.4.0".into(),
        };
        let socket = Record::control(
            Some(Caller::Socket {
                uid: 501,
                class: Class::Operator,
            }),
            "POST /api/settings",
            vec![widening.clone()],
            true,
        );
        let json = serde_json::to_value(&socket).unwrap();
        assert_eq!(json["kind"], "control");
        assert_eq!(json["action"], "allowed");
        assert_eq!(json["caller"], "socket");
        assert_eq!(json["caller_class"], "operator");
        assert_eq!(json["uid"], 501);
        assert_eq!(json["route"], "POST /api/settings");
        assert_eq!(json["widenings"][0]["key"], "update.pin");
        assert_eq!(json["widenings"][0]["sentence"], "update.pin none -> 0.4.0");

        let session = Record::control(
            Some(Caller::Session),
            "POST /api/settings",
            vec![widening],
            false,
        );
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["action"], "refused");
        assert_eq!(json["caller"], "session");
        assert!(json.get("uid").is_none());

        let plain = serde_json::to_value(rec(1)).unwrap();
        for absent in ["caller", "caller_class", "uid", "route", "widenings"] {
            assert!(plain.get(absent).is_none(), "{absent}");
        }
    }

    #[tokio::test]
    async fn a_freeze_line_says_which_runner_what_was_done_and_why() {
        // ADR-0032 §2: every freeze and thaw is audited, in a kind of its own —
        // the job has not finished, so it is not an outcome, and replay must never
        // learn from it.
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));
        let rec = Record::freeze(
            "owner/demo",
            77,
            "faber-77-1756000000000",
            "frozen",
            "disk",
            "2 GiB free on the Docker volume, below the 3 GiB critical floor".to_string(),
        );
        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["kind"], "freeze");
        assert_eq!(json["repo"], "owner/demo");
        assert_eq!(json["job_id"], 77);
        assert_eq!(json["action"], "frozen");
        assert_eq!(json["runner"], "faber-77-1756000000000");
        assert_eq!(json["reason"], "disk");
        assert_eq!(
            json["cause"],
            "2 GiB free on the Docker volume, below the 3 GiB critical floor"
        );
        assert!(
            json.get("result").is_none() && json.get("class").is_none(),
            "{json}"
        );

        audit.append(&rec).await.expect("append");
        assert!(audit.replay_outcomes().is_empty(), "not a learning sample");
        let read = audit.read_since(0);
        assert_eq!(read.len(), 1);
        assert_eq!(
            (read[0].kind.as_str(), read[0].action.as_deref()),
            ("freeze", Some("frozen"))
        );
        assert_eq!(read[0].runner.as_deref(), Some("faber-77-1756000000000"));
    }

    /// A `github_write` line names the repo, the write, why and how it went,
    /// and nothing else: no job, no token, no JIT config.
    #[test]
    fn a_github_write_line_has_its_shape() {
        let rec = Record::github_write(
            "owner/demo",
            "mint_runner",
            "provision job 7",
            "ok",
            Some("faber-7-1000"),
        );
        let json = serde_json::to_value(&rec).unwrap();
        let obj = json.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["action", "job_id", "kind", "reason", "repo", "result", "runner", "ts_ms"]
        );
        assert_eq!(obj["kind"], "github_write");
        assert_eq!(obj["repo"], "owner/demo");
        assert_eq!(obj["action"], "mint_runner");
        assert_eq!(obj["reason"], "provision job 7");
        assert_eq!(obj["result"], "ok");
        assert_eq!(obj["runner"], "faber-7-1000");
        let bare = serde_json::to_value(Record::github_write("o/r", "rerun_job", "r", "403", None))
            .unwrap();
        assert!(bare.get("runner").is_none(), "{bare}");
    }

    #[tokio::test]
    async fn a_reserve_line_reads_back_for_the_fallback_replay() {
        // ADR-0033 §4: the audit log is the fallback's persistence substrate.
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));
        let rec = Record::reserve(
            "trip",
            "freeze",
            86_400_000,
            86_400_123,
            Some(5),
            "why".into(),
        );
        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["kind"], "reserve");
        assert_eq!(json["action"], "trip");
        assert_eq!(json["reason"], "freeze");
        assert_eq!(json["cooloff_ms"], 86_400_000u64);
        audit.append(&rec).await.expect("append");
        assert!(audit.replay_outcomes().is_empty());
        let read = audit.read_since(0);
        assert_eq!(read[0].reason.as_deref(), Some("freeze"));
        assert_eq!(read[0].cooloff_ms, Some(86_400_000));
        assert_eq!(read[0].until_ms, Some(86_400_123));
        assert_eq!(read[0].armed_ms, Some(5));
    }

    #[tokio::test]
    async fn a_host_oom_line_is_its_own_kind_and_never_a_learning_sample() {
        // ADR-0033 §5, rollout step 1: observability only. The learner still bumps
        // on the outcome line, and replay must not read this one as a sample.
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));
        let rec = Record::host_oom(
            "owner/demo",
            78,
            "faber-78-1756000000000",
            Some("owner/demo - ci - build".to_string()),
            true,
            false,
            "runner cgroup memory.events: oom_kill=1 oom=0".to_string(),
        );
        let json = serde_json::to_value(&rec).unwrap();
        assert_eq!(json["kind"], "host_oom");
        assert_eq!(json["oom_runner"], true);
        assert_eq!(json["oom_dind"], false);
        assert!(json.get("class").is_none(), "{json}");
        audit.append(&rec).await.expect("append");
        assert!(audit.replay_outcomes().is_empty());
        assert_eq!(audit.read_since(0)[0].kind, "host_oom");
    }

    #[test]
    fn replay_reads_which_container_was_censored() {
        let both = OutcomeReplay {
            oom: Some(true),
            ..Default::default()
        };
        // A line from before the split: the collapsed flag censors both.
        assert_eq!(both.censored(), (true, true));
        let split = OutcomeReplay {
            oom: Some(true),
            oom_runner: Some(true),
            oom_dind: Some(false),
            ..Default::default()
        };
        assert_eq!(split.censored(), (true, false));
        assert_eq!(OutcomeReplay::default().censored(), (false, false));
    }

    #[tokio::test]
    async fn replay_takes_only_samples_that_were_learned() {
        // `class` present == "this sample was fed to the live learner". Replay keys on
        // exactly that, which is what keeps a restart from re-learning what the live
        // learner refused (a reclaim, a lifetime-cap kill, an infra failure, or a run
        // whose executing job couldn't be identified) — each of those records a
        // `label` for the dashboard but no `class`.
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));

        let learned = Record {
            kind: "outcome",
            class: Some("o/r::1::build".to_string()),
            label: Some("o/r · ci · build".to_string()),
            mem_peak_runner_mib: Some(900),
            mem_peak_dind_mib: Some(40),
            oom: Some(false),
            ..rec(1)
        };
        // Ran on a runner we couldn't tie to any job: labelled, but unlearned.
        let unattributed = Record {
            kind: "outcome",
            class: None,
            label: Some("o/r · ci · build".to_string()),
            mem_peak_runner_mib: Some(11_000),
            ..rec(2)
        };
        // A different kind entirely must never reach the learner.
        let decision = Record {
            class: Some("o/r::1::build".to_string()),
            ..rec(3)
        };
        for r in [&learned, &unattributed, &decision] {
            audit.append(r).await.expect("append");
        }

        let replayed = audit.replay_outcomes();
        assert_eq!(replayed.len(), 1, "only the learned outcome is replayed");
        assert_eq!(replayed[0].class, "o/r::1::build");
        assert_eq!(replayed[0].mem_peak_runner_mib, Some(900));
    }

    #[tokio::test]
    async fn read_since_windows_the_log_and_survives_a_torn_line() {
        // The reader has to hold up on the worst log there is: the one a SIGKILLed
        // daemon left behind, whose final line is half-written — which is exactly
        // the log you reach for when a daemon was SIGKILLed. One bad line must cost
        // that line, not the thousands above it explaining the kill.
        let dir = TempDir::new();
        let path = dir.path.join("audit.jsonl");
        let audit = Audit::at(path.clone());
        for (ts, job) in [(100u64, 1u64), (200, 2), (300, 3)] {
            audit
                .append(&Record {
                    ts_ms: ts,
                    ..rec(job)
                })
                .await
                .expect("append");
        }
        // A process killed mid-append.
        std::fs::write(
            &path,
            format!(
                "{}{{\"ts_ms\":400,\"kind\":\"out",
                std::fs::read_to_string(&path).unwrap()
            ),
        )
        .unwrap();

        let all = audit.read_since(0);
        assert_eq!(
            all.len(),
            3,
            "the torn line is skipped, the good ones are not"
        );
        assert_eq!(all[0].job_id, 1, "oldest first");

        let recent = audit.read_since(200);
        assert_eq!(
            recent.len(),
            2,
            "the window is inclusive of its lower bound"
        );
        assert_eq!(recent[0].ts_ms, 200);
    }

    #[test]
    fn read_since_treats_a_missing_log_as_empty() {
        // faber may simply never have run on this machine, and `faber status`
        // saying so beats it reporting an I/O error about a file nobody expected.
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("nope.jsonl"));
        assert!(audit.read_since(0).is_empty());
    }

    #[tokio::test]
    async fn a_reader_tolerates_a_line_written_before_a_field_existed() {
        // Every field the reader added is optional, so lines written by an older
        // faber still parse. Without this the first read after an upgrade would
        // discard the entire history that predates the upgrade.
        let dir = TempDir::new();
        let path = dir.path.join("audit.jsonl");
        std::fs::write(
            &path,
            "{\"ts_ms\":1,\"kind\":\"outcome\",\"repo\":\"o/r\",\"job_id\":9,\"result\":\"failure\"}\n",
        )
        .unwrap();
        let read = Audit::at(path).read_since(0);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].result.as_deref(), Some("failure"));
        assert!(read[0].cause.is_none(), "absent, not a parse failure");
        assert!(read[0].profile.is_none());
    }

    /// ADR-0046 §9: an outcome names its services, and a line without them
    /// carries no `services` key at all.
    #[tokio::test]
    async fn services_round_trip_and_are_omitted_when_absent() {
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));
        audit
            .append(&Record {
                services: Some(vec!["postgres-16".to_string(), "redis".to_string()]),
                ..Record::new("outcome", "o/r", 1)
            })
            .await
            .expect("append");
        audit
            .append(&Record::new("outcome", "o/r", 2))
            .await
            .expect("append");
        let text = std::fs::read_to_string(dir.path.join("audit.jsonl")).expect("read");
        let mut lines = text.lines();
        assert!(lines
            .next()
            .expect("first")
            .contains(r#""services":["postgres-16","redis"]"#));
        assert!(!lines.next().expect("second").contains("services"));
        let read = audit.read_since(0);
        assert_eq!(
            read[0].services.as_deref(),
            Some(&["postgres-16".to_string(), "redis".to_string()][..])
        );
        assert!(read[1].services.is_none());
    }

    #[tokio::test]
    async fn executed_job_id_round_trips_and_is_omitted_when_absent() {
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));
        audit
            .append(&Record {
                kind: "outcome",
                executed_job_id: Some(4242),
                ..rec(1)
            })
            .await
            .expect("append");
        audit.append(&rec(2)).await.expect("append");
        let text = std::fs::read_to_string(dir.path.join("audit.jsonl")).expect("read");
        let mut lines = text.lines();
        assert!(lines
            .next()
            .expect("first")
            .contains("\"executed_job_id\":4242"));
        assert!(
            !lines.next().expect("second").contains("executed_job_id"),
            "an absent executed job must not be serialized"
        );
    }

    /// Without the `flush().await`, `tokio::fs::File::write_all` merely
    /// *schedules* the write on tokio's blocking pool and resolves `Ready`
    /// before that pool thread has actually run it, so `append` can return
    /// while the line is still not on disk. A `std::fs::metadata` `stat()`
    /// right after each `append` call is cheap enough (no `open`/`read`
    /// overhead) to reliably observe that gap: pre-fix this reports the old
    /// (pre-append) length every time; a `read_to_string`-based content
    /// check is comparatively slow and only catches the race about half the
    /// time, which is why the length check carries the real assertion here
    /// and the content check at the end is just a sanity check run once
    /// durability is no longer in question.
    #[tokio::test]
    async fn appends_are_immediately_durable() {
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));

        let mut expected_len = 0u64;
        for job_id in 0..20u64 {
            let record = rec(job_id);
            let mut line = serde_json::to_string(&record).unwrap();
            line.push('\n');
            expected_len += line.len() as u64;

            audit.append(&record).await.unwrap();

            // Bare `stat()` right after each `append` returns — no `open`/
            // `read` overhead to give the blocking pool extra time to catch
            // up on the write this `append` call just scheduled.
            let on_disk_len = std::fs::metadata(audit.path()).unwrap().len();
            assert_eq!(
                on_disk_len, expected_len,
                "job_id {job_id}'s line was not on disk immediately after append returned"
            );
        }

        // Timing is no longer at risk once every append above has already
        // been confirmed durable; check the actual content now.
        let body = std::fs::read_to_string(audit.path()).unwrap();
        for job_id in 0..20u64 {
            assert!(
                body.contains(&format!("\"job_id\":{job_id}")),
                "job_id {job_id} missing from audit log: {body:?}"
            );
        }
    }

    #[tokio::test]
    async fn cpu_fields_round_trip_through_the_log_and_replay() {
        // ADR-0019: a restart re-learns CPU shares from this log, and a throttled
        // sample only replays correctly if its throttle ratio and the quota it ran
        // under come back with its peak.
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));
        let throttled = Record {
            kind: "outcome",
            class: Some("o/r::1::e2e".to_string()),
            cpu_peak_runner_mcpu: Some(3990),
            cpu_peak_dind_mcpu: Some(0),
            cpu_avg_runner_mcpu: Some(2100),
            cpu_avg_dind_mcpu: Some(0),
            cpu_throttled_runner: Some(0.42),
            cpu_quota_runner_mcpu: Some(4000),
            ..rec(1)
        };
        // A line written before the fields existed replays with no CPU at all —
        // never as a zero-CPU sample.
        let older = Record {
            kind: "outcome",
            class: Some("o/r::1::lint".to_string()),
            mem_peak_runner_mib: Some(300),
            ..rec(2)
        };
        for r in [&throttled, &older] {
            audit.append(r).await.expect("append");
        }

        let text = std::fs::read_to_string(audit.path()).unwrap();
        let first = text.lines().next().unwrap();
        assert!(
            !first.contains("cpu_throttled_dind"),
            "an absent ratio is omitted, and replay reads absence as unthrottled"
        );
        assert!(!text.lines().nth(1).unwrap().contains("cpu_"));

        let replayed = audit.replay_outcomes();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].cpu_quota_runner_mcpu, Some(4000));
        assert_eq!(
            replayed[0].runner_cpu(),
            Some(CpuUsage {
                peak_mcpu: 3990,
                avg_mcpu: 0,
                throttled_ratio: 0.42,
            })
        );
        // A zero peak is a sample: an idle dind is evidence, not a gap.
        assert_eq!(replayed[0].dind_cpu().map(|u| u.peak_mcpu), Some(0));
        assert_eq!(replayed[0].dind_cpu().unwrap().throttled_ratio, 0.0);
        assert_eq!(replayed[1].runner_cpu(), None);
        assert_eq!(replayed[1].dind_cpu(), None);

        // The averages are observability, not learning input: on the line, not in
        // the replay.
        assert!(first.contains("\"cpu_avg_runner_mcpu\":2100"));
    }

    // --- the trust-change notice (ADR-0028 addendum B) -------------------------

    fn allowed_record(ts_ms: u64, caller: crate::control::Caller, keys: &[&str]) -> Record {
        let widenings = keys
            .iter()
            .map(|k| crate::control::Widening {
                key: (*k).to_string(),
                sentence: format!("{k} widened"),
            })
            .collect();
        Record {
            ts_ms,
            ..Record::control(Some(caller), "POST /api/settings", widenings, true)
        }
    }

    const DAY: u64 = 24 * 60 * 60 * 1000;

    #[tokio::test]
    async fn boot_seeds_the_allowed_widenings_of_the_last_seven_days() {
        use crate::control::{Caller, Class};
        let dir = TempDir::new();
        let audit = Audit::at(dir.path.join("audit.jsonl"));
        let now = 30 * DAY;
        let operator = Caller::Socket {
            uid: 501,
            class: Class::Operator,
        };
        // Eight days old: out of the window.
        audit
            .append(&allowed_record(now - 8 * DAY, operator, &["update.repo"]))
            .await
            .unwrap();
        // Two widenings in one request: one entry each.
        audit
            .append(&allowed_record(
                now - 2 * DAY,
                operator,
                &["update.pin", "gate.on_bypass"],
            ))
            .await
            .unwrap();
        // Refused: never on the notice.
        audit
            .append(&Record {
                ts_ms: now - DAY,
                ..Record::control(
                    None,
                    "POST /api/settings",
                    vec![crate::control::Widening {
                        key: "update.enabled".into(),
                        sentence: "refused".into(),
                    }],
                    false,
                )
            })
            .await
            .unwrap();
        audit
            .append(&allowed_record(
                now - DAY,
                Caller::Session,
                &["registry_auth.enabled"],
            ))
            .await
            .unwrap();
        // Another kind entirely is ignored.
        audit.append(&rec(1)).await.unwrap();

        let clock = Arc::new(AtomicU64::new(now));
        let at = clock.clone();
        let store = TrustChanges::new(Arc::new(move || at.load(Ordering::SeqCst)));
        store.seed(audit.read_control_since(now - TRUST_WINDOW_MS));
        let list = store.list();
        let keys: Vec<&str> = list.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(
            keys,
            ["registry_auth.enabled", "update.pin", "gate.on_bypass"]
        );
        assert_eq!(list[0].via, "session");
        assert_eq!(list[0].uid, None);
        assert_eq!(list[1].via, "socket");
        assert_eq!(list[1].uid, Some(501));
        assert_eq!(list[1].route, "POST /api/settings");
        assert_eq!(list[1].sentence, "update.pin widened");

        // Time alone takes them off: six days on, only the newest is left.
        clock.store(now + 6 * DAY - 1, Ordering::SeqCst);
        let keys: Vec<String> = store.list().into_iter().map(|t| t.key).collect();
        assert_eq!(keys, ["registry_auth.enabled"]);
        clock.store(now + 7 * DAY, Ordering::SeqCst);
        assert!(store.list().is_empty());
    }

    #[test]
    fn an_allowed_change_is_listed_as_it_is_audited_and_a_refused_one_is_not() {
        use crate::control::{Caller, Class};
        let now = 10 * DAY;
        let store = TrustChanges::new(Arc::new(move || now));
        let root = Caller::Socket {
            uid: 0,
            class: Class::Present,
        };
        store.record(&allowed_record(now - 5, root, &["repo_hold.owner/demo"]));
        store.record(&Record {
            ts_ms: now,
            ..Record::control(Some(root), "POST /api/settings", Vec::new(), false)
        });
        store.record(&rec(3));
        let list = store.list();
        assert_eq!(list.len(), 1);
        let json = serde_json::to_value(&list[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "ts_ms": now - 5,
                "key": "repo_hold.owner/demo",
                "sentence": "repo_hold.owner/demo widened",
                "via": "socket",
                "uid": 0,
                "route": "POST /api/settings",
            })
        );
        // A session has no uid: `null`, present.
        store.record(&allowed_record(
            now - 1,
            Caller::Session,
            &["update.enabled"],
        ));
        let json = serde_json::to_value(store.list()).unwrap();
        assert_eq!(json[0]["key"], "update.enabled");
        assert!(json[0]["uid"].is_null());
        assert_eq!(json[0]["via"], "session");
    }
}
