//! A thin GitHub REST client for the handful of endpoints faber needs.
//!
//! Auth is ADR-0027's fine-grained token, read from the credential store
//! (`sudo faber auth set` writes it; [`crate::credential`] reads it) through
//! [`credential`], the one seam a GitHub App can later replace. faber reads no
//! token from `gh` or from the environment. A `401` re-reads the store once;
//! a token GitHub still rejects makes the credential `invalid`, and nothing new
//! is claimed until a token it accepts is stored. The updater's client is
//! [`Github::anonymous`]: it never borrows the CI token.
//!
//! Every write goes through one funnel that records it in `audit.jsonl`
//! ([`crate::audit::Record::github_write`]), and every call that needs a
//! particular permission is tagged with it, so a refusal is learned per
//! repository and the feature that needs it turns off visibly.
//!
//! GETs are ETag-aware: a stored `ETag` is replayed as `If-None-Match`, and a
//! `304 Not Modified` (which does not count against the primary rate limit when
//! authenticated) transparently returns the cached body.
//!
//! One read has no REST endpoint: a merge queue's entries, which the gate reads
//! over GraphQL (ADR-0059 §3). It is sent as a POST but is a read, so it is
//! never audited as a write.

use std::collections::HashMap;
use std::time::Duration;

pub use faber_release::{Release, ReleaseAsset};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::audit::{Audit, Record};
use crate::credential::{
    is_permission_refusal, CredentialView, Health, LoadError, Need, Permission, Problem, Secret,
    Stamp, Store, StoredToken, UNCONFIRMED,
};

/// An `actions/runner` release as [`Github::runner_release`] reads it: only what
/// the tools disk checks the tarball against. Everything here is GitHub's text and
/// is parsed, never echoed, by [`crate::vm::tools_disk::published_sha256`].
#[derive(Debug, Clone, Deserialize)]
pub struct RunnerRelease {
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<RunnerReleaseAsset>,
}

/// One asset of a [`RunnerRelease`]: its name and, when GitHub reports one, its
/// `sha256:<hex>` digest.
#[derive(Debug, Clone, Deserialize)]
pub struct RunnerReleaseAsset {
    pub name: String,
    #[serde(default)]
    pub digest: Option<String>,
}

/// Errors from talking to GitHub, or from finding the token to talk with.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// No usable token in the credential store. The text says why and what to
    /// run, and never holds the token.
    #[error("no usable GitHub token: {0}")]
    Token(String),
    /// A request that never produced a response. Rendered as a `transport_kind`
    /// tag plus the full `with_causes` chain, because `reqwest::Error`'s own
    /// `Display` names neither.
    #[error("http transport error [{}]: {}", transport_kind(.0), with_causes(.0))]
    Http(#[from] reqwest::Error),
    /// Rendered plainly, and deliberately: `serde_json::Error`'s own `Display`
    /// already names the fault *and* the offending line and column, and its
    /// `source()` is `None` for everything `from_str` can produce. Unlike
    /// [`Self::Http`] there is no hidden layer here to recover.
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("unexpected status {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },
    #[error("got 304 Not Modified for {0} but had no cached body")]
    StaleCache(String),
    /// A successful response whose content faber cannot act on, such as a tag
    /// that does not point at a commit.
    #[error("unexpected response: {0}")]
    Unexpected(String),
    /// A request faber will not send: its target is not in the form faber's own
    /// records write it in (#105). Raised before any URL is built.
    #[error("refusing to send a request about {0}")]
    Refused(String),
}

/// Refuse a runner request whose repo is not `owner/repo`, or whose runner name
/// (when there is one) is not faber's own `faber-{job_id}-{ts}`.
///
/// Both are built into the URL as they are, and on the path from an adopted
/// container they may have come off a label (#105): `..` in a repo would steer the
/// request to another endpoint, and `&` or `#` in a name would rewrite its query.
/// The rendered refusal is escaped and cut, since it is someone else's text.
fn check_runner_target(owner_repo: &str, name: Option<&str>) -> Result<(), GithubError> {
    if !crate::untrusted::repo_slug(owner_repo) {
        return Err(GithubError::Refused(format!(
            "repo {:?}, which is not owner/repo",
            crate::untrusted::for_log(owner_repo, 120)
        )));
    }
    if let Some(name) = name {
        if crate::untrusted::runner_name(name).is_none() {
            return Err(GithubError::Refused(format!(
                "runner {:?}, which is not a name faber mints",
                crate::untrusted::for_log(name, 120)
            )));
        }
    }
    Ok(())
}

/// Refuse a gate read (ADR-0059 §3) whose repo is not `owner/repo`, or whose
/// login (when there is one) is not a GitHub login. Both come off a run's
/// payload or a pull request and are built into the URL as they are.
fn check_gate_target(owner_repo: &str, login: Option<&str>) -> Result<(), GithubError> {
    check_runner_target(owner_repo, None)?;
    match login {
        Some(login) if !crate::credential::valid_login(login) => {
            Err(GithubError::Refused(format!(
                "login {:?}, which is not a GitHub login",
                crate::untrusted::for_log(login, 120)
            )))
        }
        _ => Ok(()),
    }
}

/// How deep [`with_causes`] walks a `source()` chain.
///
/// A real transport chain is four layers at most (reqwest → hyper → connector →
/// `io::Error`), so this only truncates a pathological one — including a
/// hypothetical `source()` that cycles, which would otherwise spin forever
/// building an unbounded string *inside the logging path*. It doubles as the
/// bound on how long one of these lines can grow.
const MAX_CAUSE_DEPTH: usize = 8;

/// Render `err` followed by every layer of its `source()` chain, `: `-separated.
///
/// `reqwest::Error`'s `Display` prints exactly one layer — "error sending request
/// for url (…)" — and stops there. A request-deadline expiry, a DNS failure, a
/// refused connection and a TLS fault therefore all log byte-identically, so a
/// run of transport errors cannot be attributed from the log at all. The layer
/// that actually names the failure ("operation timed out", "dns error: …",
/// "Connection refused (os error 61)") is reachable only through `source()`,
/// which is why faber renders the chain rather than the top of it.
pub(crate) fn with_causes(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cause = err.source();
    for _ in 0..MAX_CAUSE_DEPTH {
        let Some(e) = cause else { return out };
        out.push_str(": ");
        out.push_str(&e.to_string());
        cause = e.source();
    }
    // Out of budget with layers still to go. Say so, rather than let a truncated
    // chain read as a complete one.
    out.push_str(": …");
    out
}

/// A one-word name for *why* a request failed, so a run of these can be counted
/// by cause (`grep -o 'http transport error \[[a-z-]*\]' | sort | uniq -c`)
/// rather than only read one line at a time. Bracketed because the chain itself
/// is full of parentheses.
///
/// `is_connect` and `is_timeout` are read together because they overlap, and the
/// overlap is precisely the distinction worth having: `connect-timeout` means the
/// host was never reached ([`CONNECT_TIMEOUT`]), while a bare `timeout` means
/// GitHub accepted the connection and then did not answer within
/// [`REQUEST_TIMEOUT`] — the two point at entirely different culprits. Below
/// those, fall back to the kind reqwest assigned, with `request` checked last
/// because it is true of most send-path failures and so is the least specific
/// answer available.
pub(crate) fn transport_kind(err: &reqwest::Error) -> &'static str {
    match (err.is_connect(), err.is_timeout()) {
        (true, true) => "connect-timeout",
        (true, false) => "connect",
        (false, true) => "timeout",
        (false, false) => {
            if err.is_body() {
                "body"
            } else if err.is_decode() {
                "decode"
            } else if err.is_redirect() {
                "redirect"
            } else if err.is_builder() {
                "builder"
            } else if err.is_request() {
                "request"
            } else {
                "other"
            }
        }
    }
}

/// A minimal repository reference, as embedded in workflow-run and search results.
#[derive(Debug, Clone, Deserialize)]
pub struct MinimalRepo {
    pub full_name: String,
    #[serde(default)]
    pub fork: bool,
    /// Repo visibility. faber services **private repos only** — it mints runners
    /// that execute the repo's code on the host, so a public repo's queue (fork
    /// PRs, drive-by contributions) must never reach a runner. Defaults to `false`
    /// (public) when the field is absent, so a payload that omits it fails
    /// **closed** — refused, never silently serviced.
    #[serde(default)]
    pub private: bool,
}

/// The actor that triggered a run.
#[derive(Debug, Clone, Deserialize)]
pub struct Actor {
    pub login: String,
}

/// A pull request a run belongs to, as embedded in the workflow-run payload.
/// GitHub populates this for same-repo PRs; fork PRs carry an empty array (fall
/// back to `head_branch`). Only the number is needed — for the dashboard's
/// "which PR is this job for" readout.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestRef {
    pub number: u64,
}

/// A workflow run — carries the provenance faber gates on.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub event: String,
    /// The workflow's stable numeric id — the immutable component of the
    /// autolimiter's per-class key (survives workflow renames, unlike name/path).
    #[serde(default)]
    pub workflow_id: u64,
    /// The workflow's display name, for human-readable class labels.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// Who *first* caused the run to exist. On a re-run this stays the original
    /// triggerer — so for trust decisions prefer [`Self::triggering_actor`].
    #[serde(default)]
    pub actor: Option<Actor>,
    /// Who caused the *most recent* trigger (e.g. clicked "Re-run jobs"). This is
    /// the field the gate should trust; fall back to `actor` if absent.
    #[serde(default)]
    pub triggering_actor: Option<Actor>,
    /// The base repo where the workflow runs.
    pub repository: MinimalRepo,
    /// Where the head ref originated; absent on push/dispatch/schedule.
    #[serde(default)]
    pub head_repository: Option<MinimalRepo>,
    /// The head branch the run is for (e.g. `feat/x`). Present on push/PR runs;
    /// the dashboard's fallback when there's no PR number (fork PRs, or non-PR
    /// events).
    #[serde(default)]
    pub head_branch: Option<String>,
    /// PRs this run belongs to. Populated for same-repo PRs; empty for fork PRs
    /// and non-PR events. The dashboard shows the first one's number.
    #[serde(default)]
    pub pull_requests: Vec<PullRequestRef>,
    /// The commit SHA the run is for — what drift detection **posts its commit
    /// status on**. Present in the API payload; defaulted so a payload that ever
    /// omits it degrades to "" (drift check then skips) rather than failing the
    /// whole run deserialize.
    ///
    /// Do **not** use this to read repo files (the lockfile, `.faber/config.toml`)
    /// for a `pull_request` run — see [`Self::content_ref`], which is the tree
    /// `actions/checkout` actually checks out and can differ from this SHA.
    #[serde(default)]
    pub head_sha: String,
    /// Which attempt of this run produced these results. GitHub increments it on
    /// every re-run, so it is the auto-retry cap's natural, restart-proof counter
    /// (ADR-0003). Defaulted to `1` (the first-attempt value) for robustness — the
    /// runs payload always carries it.
    #[serde(default = "default_run_attempt")]
    pub run_attempt: u64,
    /// When the run last changed, RFC3339 UTC (`2026-01-15T12:00:00Z`). The retry
    /// sweep's lookback bound reads this so a fresh daemon retries only *recent*
    /// faber-caused failures, not a pile of historical ones.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// When the run was created, RFC3339 UTC. The waste check compares it with
    /// the PR's `closed_at`: a run created after the PR closed was triggered by
    /// the close, and is not wasted (ADR-0063 §2).
    #[serde(default)]
    pub created_at: Option<String>,
}

impl WorkflowRun {
    /// The ref to read this run's repo files at — **not** always [`Self::head_sha`].
    ///
    /// `actions/checkout`'s default (no `ref:` override, which is what the target
    /// repo uses) checks out `github.ref`, and for a `pull_request` event that is
    /// `refs/pull/<N>/merge` — the PR head *test-merged into the base branch* —
    /// not the PR branch head commit. When the branch is stale (its base moved
    /// since the branch's last push), the merge tree and the head-SHA tree can
    /// pin different dependency versions. A case from a watched repo: a pull
    /// request's head SHA pinned `@playwright/test` at one version, but `main`
    /// had moved to a newer one by the time the job ran, so the checked-out merge
    /// tree actually pinned the newer one — faber read the head SHA, saw the
    /// older version, called it in sync, and started the wrong runner image.
    ///
    /// So repo-file reads follow the merge ref only for a same-repo `pull_request`
    /// run (a known PR number in [`Self::pull_requests`]); every other case —
    /// `push`, `workflow_dispatch`, `schedule`, a fork PR (empty
    /// `pull_requests`) — has no separate merge tree, so `head_sha` already *is*
    /// the tree that runs.
    ///
    /// Pure and network-free: this names the ref to *try*, it does not resolve
    /// it. The merge ref can still 404 (merge conflict, closed PR, or GitHub
    /// simply hasn't computed it yet) — callers fall back to `head_sha` on that,
    /// not here (a conflicted PR can't run meaningful CI regardless).
    ///
    /// Never use this for the commit status — that must stay `head_sha`, see its
    /// own doc comment.
    pub fn content_ref(&self) -> String {
        match self.pull_number() {
            Some(n) => format!("refs/pull/{n}/merge"),
            None => self.head_sha.clone(),
        }
    }

    /// The PR a `pull_request` run with a known PR number belongs to: the runs
    /// [`Self::content_ref`] gives a merge ref to, and the only runs the waste
    /// check reads (ADR-0063 §1). `None` for every other event and a fork PR.
    pub fn pull_number(&self) -> Option<u64> {
        if self.event != "pull_request" {
            return None;
        }
        self.pull_requests.first().map(|pr| pr.number)
    }
}

/// A run's `run_attempt` defaults to its first-attempt value when (never, in
/// practice) absent, so an omitted field can't read as `0 < max_attempts` and
/// trigger a spurious retry.
fn default_run_attempt() -> u64 {
    1
}

#[derive(Debug, Deserialize)]
struct RunsPage {
    #[serde(default)]
    workflow_runs: Vec<WorkflowRun>,
}

/// One step within a job, as returned by the jobs API. Steps advance
/// `queued` → `in_progress` → `completed` as the runner executes them, so the
/// one with `status == "in_progress"` is what the job is running right now.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub name: String,
    /// `queued` | `in_progress` | `completed`.
    pub status: String,
    /// The step's 1-based ordinal within the job.
    #[serde(default)]
    pub number: i64,
    /// `success` | `failure` | `skipped` | `cancelled` … once completed.
    #[serde(default)]
    pub conclusion: Option<String>,
    /// When the step started and finished, RFC3339 (ADR-0056 §3). Absent until
    /// it has.
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

/// A single job within a run — carries the `runs-on` labels and status. `steps`
/// is read from the single-job endpoint (`GET .../jobs/{id}`), which faber polls
/// per in-flight job to surface the live step on the dashboard, and from the
/// per-run listing by the retry sweep.
#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub status: String,
    /// Terminal result once `status == "completed"`: `success` | `failure` |
    /// `cancelled` | `skipped` | … . Absent while queued/in-progress. The retry
    /// sweep (ADR-0003) only considers `failure`/`cancelled` jobs.
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub runner_name: Option<String>,
    /// The workflow's display name and the run's head branch, as GitHub reports
    /// them on the job. Read for a finished job whose identity faber did not
    /// capture at claim time (ADR-0044).
    #[serde(default)]
    pub workflow_name: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    /// When GitHub dispatched this job to a runner and when it finished, RFC3339
    /// (`2026-01-15T12:00:00Z`). The retry sweep intersects this window with the
    /// window of a locally-observed OOM on the same runner, so a kill can only ever
    /// excuse the job it actually happened during (ADR-0003). Absent while queued,
    /// and — like every field here — optional rather than assumed.
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
    /// When GitHub queued the job, RFC3339: where a run's `pickup` phase starts
    /// (ADR-0056 §2).
    #[serde(default)]
    pub created_at: Option<String>,
    /// The job's check-run URL; its trailing segment is the check-run id that keys
    /// the annotations endpoint the runner-retirement sampler reads (#64).
    #[serde(default)]
    pub check_run_url: Option<String>,
    /// `None` when the payload had no `steps` field — "unknown", never "zero steps".
    #[serde(default)]
    pub steps: Option<Vec<Step>>,
}

#[derive(Debug, Deserialize)]
struct JobsPage {
    #[serde(default)]
    jobs: Vec<Job>,
}

/// One check-run annotation. Only the message text is needed, which the
/// runner-retirement sampler scans for GitHub's deprecation notice.
#[derive(Debug, Deserialize)]
struct Annotation {
    #[serde(default)]
    message: Option<String>,
}

/// A self-hosted runner registered on a repo, as reported by
/// `GET /actions/runners`. faber registers each JIT runner under a name it
/// chooses (`faber-{job_id}-{ts}`), so it can find *its own* runner here and read
/// the authoritative `busy` flag — the only signal that survives GitHub's
/// not-job-pinned assignment (a runner minted for job A may be running sibling B).
#[derive(Debug, Clone, Deserialize)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    /// `online` | `offline`.
    pub status: String,
    /// True while the runner is executing *some* job. The reclaim path never
    /// touches a `busy` runner, so a reassigned sibling's work is never aborted.
    #[serde(default)]
    pub busy: bool,
    /// The runner's advertised labels (the `name` of each label object). Demand-
    /// based provisioning (ADR-0004) matches a registered runner to a routing
    /// profile by these to count it as already-available supply — the shared signal
    /// that lets several faber instances split one repo pool without extra infra.
    #[serde(default, deserialize_with = "runner_labels")]
    pub labels: Vec<String>,
}

/// One entry of a runner's `labels` array. faber only needs the label `name`; the
/// `id`/`type` fields the API also returns are ignored.
#[derive(Debug, Deserialize)]
struct RunnerLabel {
    name: String,
}

/// Deserialize a runner's `labels` (an array of `{name, …}` objects) into bare
/// label names. Paired with `#[serde(default)]` so an absent array yields `[]`.
fn runner_labels<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<RunnerLabel>::deserialize(de)?;
    Ok(raw.into_iter().map(|l| l.name).collect())
}

#[derive(Debug, Deserialize)]
struct RunnersPage {
    #[serde(default)]
    runners: Vec<Runner>,
}

/// One page of `search/repositories`. `total_count` and `incomplete_results`
/// are required, not defaulted: GitHub's search is intermittently short (in
/// October 2026 it answered the same query with a list of repos and then with
/// none a minute later), and a body that does not say how many there are cannot
/// be told from repos leaving the topic. See [`Github::repos_with_topic`].
#[derive(Debug, Deserialize)]
struct SearchPage {
    total_count: u64,
    incomplete_results: bool,
    #[serde(default)]
    items: Vec<MinimalRepo>,
}

/// Repos per search page, GitHub's maximum.
const SEARCH_PAGE_SIZE: usize = 100;

/// Search pages [`Github::repos_with_topic`] reads at most. GitHub's search
/// returns no result past the 1000th, so ten pages are all there ever is.
const SEARCH_PAGES: usize = 10;

/// Check one topic-search page against the ones before it, holding `have`
/// repos already. `Ok(true)` when it is the last page and the set adds up to
/// `total_count`; `Ok(false)` when a full page leaves more to read; an error for
/// a page that is marked incomplete, disagrees with an earlier page's count, or
/// ends the set short of (or past) the count it states.
fn search_page_fits(
    page: &SearchPage,
    total: &mut Option<u64>,
    have: usize,
) -> Result<bool, GithubError> {
    if page.incomplete_results {
        return Err(GithubError::Unexpected(
            "GitHub marked the topic search incomplete_results".into(),
        ));
    }
    let count = *total.get_or_insert(page.total_count);
    if count != page.total_count {
        return Err(GithubError::Unexpected(format!(
            "the topic search's total_count changed between pages ({count}, then {})",
            page.total_count
        )));
    }
    let seen = (have + page.items.len()) as u64;
    if seen > count {
        return Err(GithubError::Unexpected(format!(
            "the topic search returned {seen} repos against a total_count of {count}"
        )));
    }
    if page.items.len() == SEARCH_PAGE_SIZE && seen < count {
        return Ok(false);
    }
    if seen != count {
        return Err(GithubError::Unexpected(format!(
            "the topic search returned {seen} repos against a total_count of {count}"
        )));
    }
    Ok(true)
}

/// One open pull request, as the git mirror's refresh reads it (ADR-0035 §1):
/// its number, where its head lives and which repo it targets.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenPull {
    pub number: u64,
    pub head: PullHead,
    pub base: PullHead,
}

/// One end of a pull request. `repo` is `null` on a head whose fork has been
/// deleted, which is a head the mirror never fetches.
#[derive(Debug, Clone, Deserialize)]
pub struct PullHead {
    #[serde(default)]
    pub repo: Option<PullHeadRepo>,
}

/// The repository one end of a pull request lives in. `id` is GitHub's
/// numeric id, which survives renames and is what tells a fork apart.
#[derive(Debug, Clone, Deserialize)]
pub struct PullHeadRepo {
    pub id: u64,
    pub full_name: String,
}

/// `GET /repos/{owner}/{repo}`, for the one field the mirror's first clone
/// weighs against the disk floor (ADR-0035 §5).
#[derive(Debug, Deserialize)]
struct RepoSize {
    /// The repository's size in KiB, as GitHub reports it.
    #[serde(default)]
    size: u64,
}

/// Pages of open pull requests the mirror reads at most, 100 per page. A repo
/// with more open PRs than this loses only the speed-up for the rest: a head the
/// mirror lacks is fetched by the job, as before.
const OPEN_PULL_PAGES: u32 = 10;

/// Entries per page on every listing faber pages through: GitHub's maximum.
const PAGE_SIZE: usize = 100;

/// Pages of a run's jobs, a repo's runs at one status, or a repo's runners that
/// faber reads at most (#66). Each listing used to stop after its first page, so
/// a job past the 100th in its run was never found, and the job-start check
/// (ADR-0059 §2) must find whichever job a runner took. Bounded so one runaway
/// listing cannot turn a poll into hundreds of calls; GitHub itself stops a run's
/// matrix at 256 jobs.
const LIST_PAGES: u32 = 10;

/// The path of page `page` of a listing whose first page is `first`. Page 1 is
/// `first` itself, unchanged, so a listing that fits on one page costs the same
/// request it always did.
fn page_path(first: &str, page: u32) -> String {
    if page <= 1 {
        first.to_string()
    } else {
        format!("{first}&page={page}")
    }
}

/// Whether a page of `len` entries is the last one: GitHub fills every page but
/// the last.
fn last_page(len: usize) -> bool {
    len < PAGE_SIZE
}

/// A pull request as the gate reads it, to learn whose code a run carries
/// (ADR-0059 §3): who opened it, and the commit its head is at.
#[derive(Debug, Clone, Deserialize)]
pub struct Pull {
    pub number: u64,
    /// Who opened it. GitHub shows a deleted account as its `ghost` user; a
    /// payload with none still decodes, for the caller to refuse.
    #[serde(default)]
    pub user: Option<Actor>,
    pub head: PullTip,
    /// `open` or `closed`, read by the waste check (ADR-0063 §2). Defaulted, so a
    /// payload without it reads as no answer rather than failing to decode.
    #[serde(default)]
    pub state: Option<String>,
    /// Whether GitHub can merge it into its base: `null` until GitHub has
    /// computed it, which the waste check reads as no answer (ADR-0063 §2).
    #[serde(default)]
    pub mergeable: Option<bool>,
    /// When it closed, RFC3339 UTC; `null` while open. The waste check fires
    /// `closed` only for a run created before this (ADR-0063 §2).
    #[serde(default)]
    pub closed_at: Option<String>,
}

/// The head of a [`Pull`].
#[derive(Debug, Clone, Deserialize)]
pub struct PullTip {
    /// The commit the head is at now.
    pub sha: String,
}

/// Pages of pull requests from one head that [`Github::pulls_from`] reads, 100
/// per page. A listing that does not end within them is an error, never a
/// partial answer: the gate refuses a run it cannot read whole.
const PULLS_FROM_PAGES: u32 = 10;

/// One entry of a merge queue, as GitHub's GraphQL API lists it (ADR-0059 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    /// Its place in the queue. An entry's merge group is built on the entries
    /// ahead of it, so it carries their code too.
    pub position: u64,
    /// The commit the entry's merge group is built at, when GitHub shows one.
    pub head_sha: Option<String>,
    /// The pull request the entry adds, when GitHub shows one.
    pub pull: Option<u64>,
    /// The login of that pull request's author, as REST and the actor lists
    /// spell it: an app is `name[bot]` there, though GraphQL calls it `name`.
    /// `None` for a deleted account, or a pull request GitHub does not show.
    pub author: Option<String>,
}

/// Pages of a merge queue that [`Github::merge_queue`] reads, 100 entries per
/// page. A queue that does not end within them is an error, as for
/// [`PULLS_FROM_PAGES`].
const MERGE_QUEUE_PAGES: usize = 10;

/// The merge queue read: each entry's place, head commit and pull request
/// author, a page of 100 at a time. `__typename` tells an app from a person,
/// since GraphQL drops an app's `[bot]` suffix.
const MERGE_QUEUE_QUERY: &str = "query($owner: String!, $name: String!, $branch: String!, \
     $after: String) { repository(owner: $owner, name: $name) { mergeQueue(branch: $branch) { \
     entries(first: 100, after: $after) { pageInfo { hasNextPage endCursor } nodes { position \
     headCommit { oid } pullRequest { number author { __typename login } } } } } } }";

/// GraphQL's answer envelope: `data`, plus `errors` when a field was refused.
/// GitHub answers `200` either way, so the status says nothing about either.
#[derive(Deserialize)]
struct GraphqlAnswer<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    /// `FORBIDDEN` for a field the token may not read, `NOT_FOUND`,
    /// `RATE_LIMITED`; absent on a malformed query.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct QueueData {
    repository: Option<QueueRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueRepository {
    /// `null` when the branch has no merge queue.
    merge_queue: Option<QueueOf>,
}

#[derive(Deserialize)]
struct QueueOf {
    entries: QueuePage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueuePage {
    page_info: PageInfo,
    /// GraphQL lets a connection hold `null` for a node it will not show.
    #[serde(default)]
    nodes: Vec<Option<QueueNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueNode {
    position: u64,
    head_commit: Option<CommitOid>,
    pull_request: Option<QueuePull>,
}

#[derive(Deserialize)]
struct CommitOid {
    oid: String,
}

#[derive(Deserialize)]
struct QueuePull {
    number: u64,
    author: Option<QueueAuthor>,
}

#[derive(Deserialize)]
struct QueueAuthor {
    #[serde(rename = "__typename")]
    typename: String,
    login: String,
}

impl QueueAuthor {
    /// The login as REST spells it: GraphQL's `Bot` login lacks the `[bot]`
    /// suffix every REST payload, and every actor list, gives an app.
    fn rest_login(self) -> String {
        if self.typename == "Bot" && !self.login.ends_with("[bot]") {
            format!("{}[bot]", self.login)
        } else {
            self.login
        }
    }
}

impl From<QueueNode> for QueueEntry {
    fn from(node: QueueNode) -> Self {
        let (pull, author) = match node.pull_request {
            Some(p) => (Some(p.number), p.author.map(QueueAuthor::rest_login)),
            None => (None, None),
        };
        Self {
            position: node.position,
            head_sha: node.head_commit.map(|c| c.oid),
            pull,
            author,
        }
    }
}

/// `GET /repos/{owner}/{repo}/compare/{base}...{head}`, for the one field the
/// gate's ancestry read weighs (ADR-0059 §3).
#[derive(Deserialize)]
struct Comparison {
    /// `identical`, `behind` (base contains head), `ahead` or `diverged`.
    status: String,
}

/// How long a collaborator check's answer is believed (ADR-0059 §3). A person
/// added or removed reaches the gate within it, and a run's many jobs, and the
/// next poll's, cost one check rather than one each.
const COLLABORATOR_TTL_MS: u64 = 10 * 60 * 1000;

/// Upper bound on cached collaborator answers. Full, the stale ones go first,
/// and then the whole cache: a cleared one costs a check per person, once.
const MAX_COLLABORATOR_ENTRIES: usize = 512;

/// `(repo, login)`, lower-cased, to whether GitHub said the login is a
/// collaborator and when, epoch ms.
type CollaboratorCache = HashMap<(String, String), (bool, u64)>;

/// `s` percent-encoded for a URL's path or query, keeping RFC 3986's
/// unreserved characters and `/`, which a branch name may hold and GitHub's
/// compare route reads as part of it.
fn escape_ref(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether `sha` is a full commit id: 40 hex digits (SHA-1) or 64 (SHA-256).
fn is_commit_id(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A repository the token reaches, as `GET /user/repos` lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct GrantedRepo {
    pub full_name: String,
    /// Absent reads as public, so a payload without it fails closed, as
    /// [`MinimalRepo::private`] does.
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub topics: Vec<String>,
}

/// Pages of `GET /user/repos` read at most, 100 per page.
const GRANTED_PAGES: u32 = 5;

#[derive(Deserialize)]
struct JitConfigResponse {
    encoded_jit_config: String,
    /// The registration GitHub created for the runner. Optional so a response
    /// that ever leaves it out still mints: the id only saves a reclaim a lookup.
    #[serde(default)]
    runner: Option<JitRunner>,
}

#[derive(Deserialize)]
struct JitRunner {
    id: u64,
}

/// What withdrawing a runner's registration established
/// ([`Github::withdraw_runner`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deregistration {
    /// The registration is gone — deleted now, or already absent — so GitHub can
    /// assign the runner nothing more.
    Removed,
    /// GitHub refused: a job is assigned to the runner.
    JobAssigned,
}

/// A minted just-in-time runner: the config its process starts from, and the id
/// of the registration GitHub created for it.
///
/// Deliberately no `Debug`: `encoded` is a credential for that one runner, and
/// the compiler then refuses any `{:?}` that would put it in a log line.
pub struct JitConfig {
    /// The value handed to `run.sh --jitconfig`.
    pub encoded: String,
    /// The registration's id, when GitHub returned one. A reclaim withdraws the
    /// registration by it before it may kill the runner (ADR-0017); `None` falls
    /// back to finding the registration by name.
    pub runner_id: Option<u64>,
}

/// Upper bound on release-list pages fetched (100 releases each).
const MAX_RELEASE_PAGES: u32 = 10;
/// Releases requested per page; a shorter page is the last one.
pub(crate) const RELEASES_PER_PAGE: usize = 100;

#[derive(Debug, Deserialize)]
struct Viewer {
    login: String,
}

/// Upper bound on distinct ETag cache entries. The per-run jobs endpoint keys on
/// the ever-unique `run_id`, so without a cap the map grows one permanent entry
/// per workflow run for the life of the daemon. 512 comfortably exceeds the number
/// of simultaneously-active runs, so hot (active-run) keys never get evicted.
const MAX_ETAG_ENTRIES: usize = 512;

#[derive(Clone)]
struct CacheEntry {
    etag: String,
    body: String,
}

/// A capacity-bounded ETag cache with LRU eviction, holding the leak (bug-005) at
/// bay without dropping the `304` win for active runs. Active runs are looked up
/// every poll cycle (they return `304`), so they stay most-recently-used and hot;
/// a completed run stops being looked up and falls off the tail. The victim scan is
/// O(n), but `n <= capacity` and evictions only happen once the cache is full, so
/// the cost is negligible.
struct EtagCache {
    capacity: usize,
    /// Monotonic logical clock; each `store`/`lookup` stamps the touched entry so
    /// the oldest stamp identifies the least-recently-used victim.
    tick: u64,
    /// `url -> (entry, last-used tick)`.
    entries: HashMap<String, (CacheEntry, u64)>,
}

impl EtagCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            tick: 0,
            entries: HashMap::new(),
        }
    }

    /// Return a clone of the cached entry for `url`, marking it most-recently-used.
    /// Cloning (rather than borrowing) keeps [`Github::get`] self-contained per
    /// call: the caller holds the whole entry, so a later eviction can never strand
    /// it mid-request.
    fn lookup(&mut self, url: &str) -> Option<CacheEntry> {
        self.tick += 1;
        let tick = self.tick;
        let (entry, used) = self.entries.get_mut(url)?;
        *used = tick;
        Some(entry.clone())
    }

    /// Insert or refresh `url -> {etag, body}`. When inserting a *new* key into a
    /// full cache, first evict the least-recently-used entry so `len() <= capacity`
    /// always holds.
    fn store(&mut self, url: String, etag: String, body: String) {
        self.tick += 1;
        let tick = self.tick;
        if !self.entries.contains_key(&url) && self.entries.len() >= self.capacity {
            self.evict_lru();
        }
        self.entries.insert(url, (CacheEntry { etag, body }, tick));
    }

    /// Drop the single least-recently-used entry (the smallest `used` tick).
    fn evict_lru(&mut self) {
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, (_, used))| *used)
            .map(|(url, _)| url.clone());
        if let Some(url) = victim {
            self.entries.remove(&url);
        }
    }

    /// Current entry count. Only the tests read this (production code never needs
    /// the size), so it is gated to test builds to stay dead-code-warning-free.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Total deadline on one GitHub HTTP call — connect, request, response body.
///
/// `reqwest` ships *no* default request timeout, so without this a call that
/// GitHub never answers (a black-holed connection, a hung TLS session, a laptop
/// whose network went away mid-request) waits forever rather than failing.
///
/// The damage is concentrated in provisioning. `spawn_provision` flips a claim to
/// `admitted` and debits its memory reservation *before* `Docker::run_one` mints
/// the JIT config, and the run only reaches the `select!` that watches its cancel
/// token afterwards. A `generate_jitconfig` that stalls therefore parks a task the
/// claim reconciler cannot reclaim — nothing is polling the cancel token yet, so
/// flipping it is a no-op — pinning that reservation against the memory budget
/// until the daemon restarts.
/// Every `docker` CLI call is already bounded ([`crate::docker::CLI_TIMEOUT`],
/// [`crate::docker::TEARDOWN_TIMEOUT`]); this is the same bound for the HTTP side.
///
/// Sized well above a slow-but-healthy GitHub: its p99 is a few seconds, and the
/// largest body faber ever reads is a raw lockfile of a couple of MB. Deliberately
/// wider than the 20s docker-CLI bound because a REST round-trip crosses the
/// internet rather than a local socket. The cap only ever fires on a call that was
/// not going to return, converting an indefinite hang into a logged error the next
/// reconcile cycle retries.
///
/// Applies *per request*, so the one-shot 401 retry in `send_with_auth` gets its
/// own budget rather than sharing this one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Inner bound on just the TCP + TLS handshake.
///
/// Subsumed by [`REQUEST_TIMEOUT`], but named separately because an *unreachable*
/// API host is the common failure and the one worth failing fast on: the OS will
/// happily retransmit SYNs for minutes before surfacing `ETIMEDOUT`, so without
/// this a dropped route or a stale interface burns the entire request budget on a
/// host that never answered. A handshake to `api.github.com` is sub-second when
/// the network is up, so 10s is generous for the healthy case.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an unused connection may sit in the pool before faber retires it.
///
/// `reqwest` defaults this to 90 seconds, which is *longer* than the window in
/// which api.github.com's edge actually hangs up on an idle keep-alive
/// connection — and that mismatch generates errors rather than saving handshakes.
/// When the server closes first, the socket stays in faber's pool looking usable;
/// the next request is written onto it and a FIN comes back instead of a response,
/// which hyper reports as [`RETRYABLE_TRANSPORT`]. faber logged hundreds of
/// these, one per discovery cycle: [`crate::reconcile`]'s cycle opens with
/// [`Github::repos_with_topic`], so the first request of every cycle is precisely
/// the one that inherits the connection left idle since the end of the previous
/// one. The rate stepped up by more than an order of magnitude with no faber
/// commit anywhere in that window, which is what a server-side or on-path
/// keep-alive policy change looks like from in here.
///
/// 5s is picked against two bounds rather than as a round number. Above: reuse
/// after one `poll_interval_secs` (20s by default) failed essentially every time,
/// so the real close window is somewhere *below* 20s and any value near it is
/// already too long to be safe. Below: pooling only pays across the bursts faber
/// issues *within* one cycle — a repo scan fires two run listings, a job listing
/// per run and a runner listing back to back, milliseconds apart — and 5s covers
/// those with room to spare while still sitting at or under the shortest
/// keep-alive idle window in common deployment (`Keep-Alive: timeout=5`). The net
/// effect is that faber pays for one extra handshake per cycle, deliberately and
/// on its own schedule, instead of discovering a dead connection by writing to it.
///
/// This narrows the race but cannot close it: no client-side idle bound rules out
/// a server that hangs up in the instant between a connection being taken from the
/// pool and the request being written onto it. That residue is what [`Idempotency`]
/// and the one-shot retry in [`Github::send_with_token`] cover.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the HTTP client every [`Github`] uses, bounded by [`REQUEST_TIMEOUT`],
/// [`CONNECT_TIMEOUT`] and [`POOL_IDLE_TIMEOUT`]. Split out from [`Github::new`]
/// so the transport policy has exactly one definition.
///
/// `pool_max_idle_per_host` is deliberately left at reqwest's unbounded default:
/// [`POOL_IDLE_TIMEOUT`] already drains the pool on a 5s timer, so a cap would
/// only ever bite during a genuinely concurrent burst — exactly the traffic
/// pooling exists to serve — and capping it there would trade the stale-connection
/// problem for a handshake-per-request one.
///
/// What faber calls itself to GitHub.
///
/// **The API rejects a request with no `User-Agent` outright** — a bare 403
/// with an explanatory body, not a 401 — so every client faber builds has to
/// carry one, not just the shared pooled one. Build-stamped rather than
/// hardcoded so a fleet's requests say which version made them.
const USER_AGENT: &str = concat!("faber/", env!("FABER_VERSION"));

/// `tcp_keepalive` was considered and left off. It defends against a path that
/// drops a flow *silently*, which surfaces as a `timeout`; every failure observed
/// here arrived as a clean FIN, which keepalive probes neither prevent nor detect
/// any sooner. It would also be inert configuration: with a 5s idle cap no pooled
/// connection survives long enough for a keepalive probe to fire.
fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build()
}

/// How long a whole release-asset download (not just the connect) may take,
/// used only by [`Github::download_release_asset`]'s two one-off clients.
///
/// Far wider than [`REQUEST_TIMEOUT`], which is sized for small JSON
/// responses: an asset download transfers actual payload — the daemon binary
/// and the app bundle tarball, tens of MB — over whatever the host's link
/// happens to be. The bound exists only to turn a truly stuck transfer into a
/// retried update-loop tick rather than a wedged updater, not to police how
/// long a normal download may reasonably take.
const ASSET_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// hyper's wording for a connection that died after the request was written and
/// before any part of a response arrived — the idle keep-alive race
/// [`POOL_IDLE_TIMEOUT`] describes.
///
/// Matched as a string because faber depends on `reqwest`, not on `hyper`: the
/// typed predicate (`hyper::Error::is_incomplete_message`) sits two `source()`
/// layers below `reqwest::Error`, behind a type faber cannot name without taking
/// a direct dependency on hyper's major version and then having to track it. The
/// wording has been stable across hyper 0.14 and 1.x, and a false positive would
/// have to be some *other* transport failure whose own cause chain quotes this
/// sentence.
const RETRYABLE_TRANSPORT: &str = "connection closed before message completed";

/// Whether `err` is the one transport failure faber may safely re-send an
/// idempotent request after.
///
/// Deliberately narrow — this is not "retry transport errors", it is "retry the
/// stale-pooled-connection race". Every other kind [`transport_kind`] names is
/// either not worth re-sending or actively wasteful to re-send: a `timeout` may
/// well have been processed by GitHub and re-sending it spends a second
/// [`REQUEST_TIMEOUT`] on a stall the reconcile loop would have retried next cycle
/// anyway, and a `connect` failure means the host is unreachable *right now*, so
/// an immediate second attempt fails identically. Only this one is both certain to
/// have gone unanswered and near-certain to succeed on the connection the pool
/// opens next.
///
/// The `is_request` guard keeps it to the send phase. hyper raises the same error
/// for a response body truncated mid-read, which reqwest classifies as `body` and
/// which reaches faber from `text()`/`bytes()` rather than `send()` — re-sending
/// there would mean re-sending a request that demonstrably *was* processed.
///
/// Classified off the very rendering [`GithubError::Http`] logs, so a log line and
/// the retry decision behind it can never disagree about what happened.
fn is_retryable_transport(err: &reqwest::Error) -> bool {
    err.is_request() && with_causes(err).contains(RETRYABLE_TRANSPORT)
}

/// The Contents API URL for `path` in `owner_repo`, everything but the `ref`.
///
/// The ref is added by the caller as a `?ref=` query parameter via
/// [`reqwest::RequestBuilder::query`] rather than interpolated into this string.
/// [`WorkflowRun::content_ref`] can hand `get_file_at_ref` a PR merge ref
/// (`refs/pull/123/merge`), which contains `/`. A query *value* doesn't strictly
/// need `/` escaped to be valid per RFC 3986, but building the query by hand
/// invites exactly the class of bug this avoids — a future caller passing a ref
/// with `&`, `#`, or a space would silently corrupt the request instead of
/// erroring. `RequestBuilder::query` percent-encodes properly and GitHub decodes
/// it back, so the ref survives the round trip unmangled regardless of content.
fn contents_url(base: &str, owner_repo: &str, path: &str) -> String {
    format!("{base}/repos/{owner_repo}/contents/{path}")
}

/// Whether a request may be transparently re-sent when its connection died before
/// any response came back.
///
/// Stated per call site rather than inferred, because the failure itself does not
/// say whether GitHub had already *processed* the request — only that faber never
/// saw the answer. For an idempotent request that ambiguity is harmless. For
/// `generate-jitconfig` it is the difference between one runner and two minted for
/// a single job, which would surface exactly as the symptoms faber already logs: a
/// runner executing a job it was not minted for, and an orphan left behind holding
/// a memory reservation. So the unsafe direction is the one that has to be spelled
/// out and the safe one is never assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Idempotency {
    /// Re-sending has no effect beyond the first send. True of GET and of DELETE
    /// (RFC 9110 §9.2.2) — and, for [`Github::delete_runner`] specifically, proven
    /// by that method already treating a `404` as success, which is exactly what a
    /// re-send of a DELETE that landed would come back as.
    Idempotent,
    /// Re-sending may perform the side effect a second time. Every POST faber
    /// issues, without exception — see [`Github::post`].
    NotIdempotent,
}

/// A GitHub REST client scoped to the few endpoints faber uses.
/// How many annotated-tag objects [`Github::tag_commit`] peels before it gives
/// up. A tag of a tag is legal git but never what `release.yml` makes.
const MAX_TAG_PEELS: usize = 4;

/// `GET /repos/{o}/{r}/git/ref/tags/{tag}`, the fields [`Github::tag_commit`] reads.
#[derive(Debug, Deserialize)]
struct GitRef {
    object: GitObject,
}

/// `GET /repos/{o}/{r}/git/tags/{sha}`, an annotated tag object.
#[derive(Debug, Deserialize)]
struct GitTag {
    object: GitObject,
}

#[derive(Debug, Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

/// How a [`Github`] authenticates.
enum Auth {
    /// No `Authorization` header at all: the updater, reading public releases.
    /// It never borrows the CI token (ADR-0027).
    Anonymous,
    /// A token, held behind a lock so a new one replaces it in place. `store`
    /// is where it came from, re-read on a `401` and whenever it changes; a
    /// token handed in directly has none.
    Token {
        token: RwLock<Secret>,
        store: Option<Store>,
    },
}

pub struct Github {
    http: reqwest::Client,
    auth: Auth,
    base: String,
    etags: Mutex<EtagCache>,
    /// Spacing between mints while `generate-jitconfig` answers 5xx (#89).
    mint_backoff: std::sync::Mutex<MintBackoff>,
    /// What faber knows about its token: usable or not, and which permissions
    /// which repositories lack.
    health: std::sync::Mutex<Health>,
    /// Where every write is recorded (ADR-0027). Set once, at daemon boot; a
    /// client without one (a test, `auth set`'s checks) writes nothing anyway
    /// that is not the daemon's.
    audit: std::sync::OnceLock<Audit>,
    /// The owner the daemon serves. Discovery is scoped to it at boot, so a
    /// stored token for any other owner is not taken without a restart.
    serving: Option<String>,
    /// The last `x-ratelimit-remaining` GitHub sent, or `u64::MAX` before any
    /// has. Read by work that can be dropped when the budget runs low.
    rate_remaining: std::sync::atomic::AtomicU64,
    /// Collaborator checks' answers, for [`COLLABORATOR_TTL_MS`]: a `204` or a
    /// `404` has no body for an ETag to replay.
    collaborators: std::sync::Mutex<CollaboratorCache>,
}

/// Which repository a request is about, and the permission it needs there: how
/// a refusal is learned per repository ([`Health::refused`]).
#[derive(Debug, Clone, Copy)]
struct Scope<'a> {
    repo: &'a str,
    permission: Permission,
}

impl<'a> Scope<'a> {
    fn of(repo: &'a str, permission: Permission) -> Option<Self> {
        Some(Self { repo, permission })
    }
}

/// One write faber makes with the token, as `audit.jsonl` records it.
#[derive(Debug, Clone, Copy)]
struct Write<'a> {
    repo: &'a str,
    /// `mint_runner`, `delete_runner`, `withdraw_runner`, `rerun_job`,
    /// `rerun_failed_jobs`, `commit_status`.
    action: &'static str,
    /// Why the caller made it.
    reason: &'a str,
    permission: Permission,
    /// The runner a runner write is about, when it has a name.
    runner: Option<&'a str>,
}

impl Write<'_> {
    fn scope(&self) -> Option<Scope<'_>> {
        Scope::of(self.repo, self.permission)
    }
}

/// The remaining rate-limit budget below which optional reads are dropped
/// ([`Github::rate_budget_low`]).
const RATE_BUDGET_FLOOR: u64 = 500;

/// Whether GitHub says this response's rate-limit window is spent, which makes
/// a `403` a rate limit rather than a permission refusal.
fn rate_limit_exhausted(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "0")
}

/// How a write went, in a word or a number, for its audit line.
fn write_result(err: &GithubError) -> String {
    match err {
        GithubError::Status { status, .. } => status.to_string(),
        GithubError::Http(e) => format!("transport:{}", transport_kind(e)),
        GithubError::Refused(_) => "refused".to_string(),
        GithubError::Token(_) => "no_token".to_string(),
        GithubError::Decode(_) => "decode".to_string(),
        GithubError::StaleCache(_) | GithubError::Unexpected(_) => "unexpected".to_string(),
    }
}

/// The first wait after a `generate-jitconfig` 5xx; each further one in a row
/// doubles it, up to [`MINT_BACKOFF_MAX_MS`] (#89).
const MINT_BACKOFF_BASE_MS: u64 = 10_000;
/// The longest wait between two mints while GitHub keeps failing them. A burst
/// once lasted hours, so this is what bounds faber's share of it: one mint per
/// host every five minutes, instead of the next claim's at once.
const MINT_BACKOFF_MAX_MS: u64 = 5 * 60 * 1000;

/// Backoff for `generate-jitconfig` while it answers 5xx (#89).
///
/// Before this, each failed mint ended its provision and the next claim minted at
/// once: dozens of failures in an hour, spending the API budget other
/// watchers share. Now, after `n` failures in a row, the next mint waits
/// `10 s × 2ⁿ⁻¹`, capped at five minutes, and a success clears it.
///
/// Once a wait is over, one claim takes the turn and the next one waits a whole
/// delay again: the claims that parked behind the backoff would otherwise all
/// mint at the instant it ends, which is the burst again. Only a 5xx counts. A
/// 4xx is an answer about this request, and a transport failure is already
/// retried or reported where it happens.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MintBackoff {
    /// 5xx answers in a row since the last success.
    failures: u32,
    /// When the next mint may go (epoch ms).
    until_ms: u64,
}

impl MintBackoff {
    /// The wait after `failures` 5xx answers in a row.
    fn delay_ms(failures: u32) -> u64 {
        let doublings = failures.saturating_sub(1).min(16);
        MINT_BACKOFF_BASE_MS
            .saturating_mul(1 << doublings)
            .min(MINT_BACKOFF_MAX_MS)
    }

    /// A mint got a 5xx at `now_ms`.
    pub fn failed(&mut self, now_ms: u64) {
        self.failures = self.failures.saturating_add(1);
        self.until_ms = now_ms.saturating_add(Self::delay_ms(self.failures));
    }

    /// A mint succeeded: GitHub is answering again.
    pub fn succeeded(&mut self) {
        *self = Self::default();
    }

    /// How many ms are left before a turn may be taken at `now_ms`, without
    /// taking one: `None` when a mint may go now.
    pub fn wait_ms(&self, now_ms: u64) -> Option<u64> {
        (self.failures > 0 && now_ms < self.until_ms).then(|| self.until_ms - now_ms)
    }

    /// Take a turn to mint at `now_ms`, or learn how many ms are left to wait.
    /// A turn taken while backing off pushes the next one a whole delay out.
    pub fn turn(&mut self, now_ms: u64) -> Result<(), u64> {
        if self.failures == 0 {
            return Ok(());
        }
        if let Some(wait) = self.wait_ms(now_ms) {
            return Err(wait);
        }
        self.until_ms = now_ms.saturating_add(Self::delay_ms(self.failures));
        Ok(())
    }

    /// 5xx answers in a row since the last success.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

/// The daemon's GitHub credential, through the one seam ADR-0027 names: a
/// token for this host's repositories and operations. Today that is the
/// fine-grained token in the credential store; a GitHub App can later replace
/// this function without touching a caller. The daemon asks for its credential
/// here and nowhere else: at boot, from [`crate::standby`], and through the
/// store this returns, on a `401` and when the store changes.
///
/// Blocking: it reads a file. A LEGACY host has no store and so no token.
///
/// The error says what is wrong ([`LoadError::defect`]); what to run about it
/// is the caller's to add, since standby and a terminal say it differently.
pub fn credential() -> Result<(StoredToken, Store), GithubError> {
    let store = Store::system().ok_or_else(|| GithubError::Token(LoadError::NoStore.defect()))?;
    let token = store.load().map_err(|e| GithubError::Token(e.defect()))?;
    Ok((token, store))
}

impl Github {
    fn build(auth: Auth, health: Health, serving: Option<String>) -> Result<Self, GithubError> {
        Ok(Self {
            http: http_client()?,
            auth,
            base: "https://api.github.com".to_string(),
            etags: Mutex::new(EtagCache::with_capacity(MAX_ETAG_ENTRIES)),
            mint_backoff: std::sync::Mutex::new(MintBackoff::default()),
            rate_remaining: std::sync::atomic::AtomicU64::new(u64::MAX),
            health: std::sync::Mutex::new(health),
            audit: std::sync::OnceLock::new(),
            serving,
            collaborators: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// A client with a token handed in directly, and no store behind it.
    pub fn new(token: String) -> Result<Self, GithubError> {
        Self::with_token(Secret::new(token))
    }

    /// [`Self::new`] for a token already held as a [`Secret`]: `sudo faber
    /// auth set` checking a token before it is stored.
    pub fn with_token(token: Secret) -> Result<Self, GithubError> {
        Self::build(
            Auth::Token {
                token: RwLock::new(token),
                store: None,
            },
            Health::default(),
            None,
        )
    }

    /// The daemon's client: the token [`credential`] read, re-read from
    /// `store` on a `401` and whenever the store changes.
    pub fn from_store(token: StoredToken, store: Store) -> Result<Self, GithubError> {
        let health = Health::of(&token, store.stamp());
        let serving = Some(token.owner.clone());
        Self::build(
            Auth::Token {
                token: RwLock::new(token.token.clone()),
                store: Some(store),
            },
            health,
            serving,
        )
    }

    /// A client that sends no credential at all: the updater's (ADR-0027).
    /// GitHub allows 60 anonymous requests an hour per address, which the
    /// updater's half-hourly poll stays far inside.
    pub fn anonymous() -> Result<Self, GithubError> {
        Self::build(Auth::Anonymous, Health::default(), None)
    }

    /// A client aimed at a local stand-in for the API, so a test outside this module
    /// can drive code that calls GitHub without reaching api.github.com.
    #[cfg(test)]
    pub(crate) fn at(base: String) -> Self {
        Self {
            base,
            ..Self::new("test-token".to_string()).expect("client builds")
        }
    }

    /// Whether GitHub's rate-limit budget is low enough that optional reads are
    /// skipped: under a tenth of the hourly 5000 an installation token gets. A
    /// budget never reported is not low.
    pub fn rate_budget_low(&self) -> bool {
        self.rate_remaining
            .load(std::sync::atomic::Ordering::Relaxed)
            < RATE_BUDGET_FLOOR
    }

    fn note_rate(&self, resp: &reqwest::Response) {
        if let Some(left) = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            self.rate_remaining
                .store(left, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Mark the token in use as taken but not yet confirmed by GitHub.
    #[cfg(test)]
    pub(crate) fn mark_unconfirmed(&self) {
        self.health.lock().unwrap().set_unconfirmed();
    }

    /// Record every write in `audit` from now on. Set once, at daemon boot.
    pub fn set_audit(&self, audit: Audit) {
        let _ = self.audit.set(audit);
    }

    /// What the enabled features need of the token, so a refusal names the
    /// feature it turns off ([`crate::credential::required`]).
    pub fn set_needs(&self, needs: Vec<Need>) {
        self.health.lock().unwrap().set_needs(needs);
    }

    /// What the enabled features need of the token.
    pub fn needs(&self) -> Vec<Need> {
        self.health.lock().unwrap().needs().to_vec()
    }

    /// `credential` for `/api/state`. Never the token.
    pub fn credential_view(&self) -> CredentialView {
        self.health.lock().unwrap().view(crate::audit::now_ms())
    }

    /// Why nothing new may be claimed on this credential, if that is so: GitHub
    /// rejected it, the store changed into something unusable or into another
    /// owner's token, a token taken from the store waits for GitHub to say whom
    /// it acts as, or it expired. A token handed in directly carries no expiry
    /// to judge.
    pub fn claim_block(&self) -> Option<String> {
        let health = self.health.lock().unwrap();
        if let Some(problem) = health.problem() {
            return Some(problem.reason());
        }
        // Claiming on it before `GET /user` confirms the served owner could
        // mint runners for another account's jobs. The next cycle asks.
        if health.unconfirmed() {
            return Some(UNCONFIRMED.to_string());
        }
        let meta = health.meta()?;
        let now = crate::audit::now_ms();
        matches!(
            crate::credential::expiry(meta.expires_at_ms, now),
            crate::credential::Expiry::Expired
        )
        .then(|| {
            CredentialView::of(meta, None, now)
                .reason
                .unwrap_or_default()
        })
    }

    /// Whether to skip work on `repo` that needs `permission`, because GitHub
    /// refused it there within the last hour ([`Health::blocked`]).
    pub fn blocked(&self, repo: &str, permission: Permission) -> bool {
        self.health
            .lock()
            .unwrap()
            .blocked(repo, permission, crate::audit::now_ms())
    }

    /// Record which repositories the token reaches and which opted in disagree
    /// about ([`crate::credential::Mismatches`]).
    pub fn set_mismatches(&self, mismatches: crate::credential::Mismatches) {
        self.health.lock().unwrap().set_mismatches(mismatches);
    }

    /// GitHub says the token in use acts as `acts_as`, not `serving`, the
    /// owner this daemon serves: nothing new is claimed until a token for that
    /// owner is stored ([`Problem::ActsAsOther`]). `true` when that is news.
    pub fn acts_as_other(&self, acts_as: &str, serving: &str) -> bool {
        let mut health = self.health.lock().unwrap();
        let stamp = health.stamp().cloned();
        health.set_problem(
            Problem::ActsAsOther {
                acts_as: acts_as.to_string(),
                serving: serving.to_string(),
            },
            stamp,
        )
    }

    /// Whether a token taken from the store still waits for GitHub to say
    /// whom it acts as ([`Self::current_login`]): set whenever a different
    /// token is taken, by a store change or a `401`'s re-read alike.
    pub fn unconfirmed(&self) -> bool {
        self.health.lock().unwrap().unconfirmed()
    }

    /// GitHub says the token in use acts as the owner this daemon serves:
    /// that ends an [`Problem::ActsAsOther`] block, and nothing else does.
    /// `true` when it ended one.
    pub fn owner_confirmed(&self) -> bool {
        self.health.lock().unwrap().confirmed()
    }

    /// Whether GitHub's last answer to the token in use was a `401`: no request
    /// has been accepted since, and no different token taken. True under a
    /// standing [`Problem::ActsAsOther`] too, so a rejected token is throttled
    /// whatever else blocks claims.
    pub fn is_rejected(&self) -> bool {
        self.health.lock().unwrap().token_rejected()
    }

    /// Probe whether the token holds `permission` on `owner_repo`, with the
    /// cheapest read that needs it, ETag-cached so a repeat costs nothing
    /// against the rate limit. A refusal is recorded like any other
    /// ([`Self::observe`]). Writes cannot be probed without side effects, and
    /// Checks: read without a check run to read, so those return `Ok` unasked.
    pub async fn probe(&self, owner_repo: &str, permission: Permission) -> Result<(), GithubError> {
        let path = match permission {
            Permission::ActionsRead => format!("repos/{owner_repo}/actions/runs?per_page=1"),
            Permission::Administration => format!("repos/{owner_repo}/actions/runners?per_page=1"),
            Permission::ContentsRead => format!("repos/{owner_repo}/commits?per_page=1"),
            Permission::PullRequestsRead => format!("repos/{owner_repo}/pulls?per_page=1"),
            // A GraphQL read, with no REST endpoint to probe: learned when used.
            Permission::ActionsWrite
            | Permission::ChecksRead
            | Permission::MergeQueuesRead
            | Permission::StatusesWrite => return Ok(()),
        };
        let _: serde_json::Value = self.get(&path, Scope::of(owner_repo, permission)).await?;
        Ok(())
    }

    /// The repositories the token reaches (`GET /user/repos`), at most
    /// [`GRANTED_PAGES`] pages of 100. A fine-grained token lists the ones the
    /// operator picked for it, which is the ceiling GitHub enforces (ADR-0027).
    pub async fn granted_repos(&self) -> Result<Vec<GrantedRepo>, GithubError> {
        let mut out = Vec::new();
        for page in 1..=GRANTED_PAGES {
            let batch: Vec<GrantedRepo> = self
                .get(&format!("user/repos?per_page=100&page={page}"), None)
                .await?;
            let last = batch.len() < 100;
            out.extend(batch);
            if last {
                break;
            }
        }
        Ok(out)
    }

    /// The imminent-expiry warning, at most once a day.
    pub fn daily_expiry_warning(&self) -> Option<String> {
        self.health
            .lock()
            .unwrap()
            .daily_expiry_warning(crate::audit::now_ms())
    }

    /// Re-read the store if it has changed since the token in use was read.
    /// `true` when a different token was taken, which the caller confirms with
    /// GitHub ([`Self::current_login`], then [`Self::owner_confirmed`] or
    /// [`Self::acts_as_other`]). GitHub's word that the token acts as another
    /// account stands until then, whatever the store does meanwhile.
    pub async fn reload_if_changed(&self) -> bool {
        let Auth::Token {
            token,
            store: Some(store),
        } = &self.auth
        else {
            return false;
        };
        let stamp = store.stamp();
        if self.health.lock().unwrap().stamp() == stamp.as_ref() {
            return false;
        }
        self.take_from_store(store, token, stamp).await
    }

    /// Read `store` and take its token if it may be used. `true` when the token
    /// in use changed.
    async fn take_from_store(
        &self,
        store: &Store,
        token: &RwLock<Secret>,
        stamp: Option<Stamp>,
    ) -> bool {
        let reader = store.clone();
        let loaded = tokio::task::spawn_blocking(move || reader.load())
            .await
            .unwrap_or_else(|e| {
                Err(LoadError::Refused(format!(
                    "the credential store could not be read ({e})"
                )))
            });
        let fresh = match loaded {
            Ok(fresh) => fresh,
            Err(e) => {
                let problem = Problem::Unusable(e.reason());
                if self
                    .health
                    .lock()
                    .unwrap()
                    .set_problem(problem.clone(), stamp)
                {
                    warn!(
                        "GitHub token: {}. Nothing new is claimed until it is fixed",
                        problem.reason()
                    );
                }
                return false;
            }
        };
        if let Some(serving) = self.serving.as_ref().filter(|s| **s != fresh.owner) {
            let problem = Problem::OwnerChanged {
                stored: fresh.owner.clone(),
                serving: serving.clone(),
            };
            if self
                .health
                .lock()
                .unwrap()
                .set_problem(problem.clone(), stamp)
            {
                warn!("GitHub token: {}", problem.reason());
            }
            return false;
        }
        let same = token.read().await.same_as(&fresh.token);
        *token.write().await = fresh.token.clone();
        {
            let mut health = self.health.lock().unwrap();
            let rejected = health.token_rejected();
            let acts_as_other = health.acts_as_other().cloned();
            let was_unconfirmed = health.unconfirmed();
            let warned_form = health.file_warning().map(str::to_string);
            health.replace(&fresh, stamp);
            // Said once per change into the earlier, `_faber`-owned form, not
            // on every re-read of a file that stays in it.
            if let Some(warning) = fresh.file_warning.as_ref() {
                if warned_form.as_ref() != Some(warning) {
                    warn!("GitHub token: {warning}");
                }
            }
            if !same {
                health.set_unconfirmed();
            }
            // GitHub said the token acts as another account. Neither
            // re-reading it (a touch of the file changes the stamp) nor
            // taking a different one lifts that: only GitHub confirming the
            // token now in use acts as the served owner does.
            if let Some(problem) = acts_as_other {
                let stamp = health.stamp().cloned();
                health.set_problem(problem, stamp);
                if !same || was_unconfirmed {
                    health.set_unconfirmed();
                }
            }
            // Re-reading the token GitHub just rejected does not un-reject it,
            // under a standing ActsAsOther block as much as without one.
            if same && rejected {
                health.rejected();
            }
        }
        if !same {
            info!(
                "GitHub token re-read from the credential store: owner {}, expires {}",
                fresh.owner,
                crate::credential::date_of(fresh.expires_at_ms)
            );
        }
        !same
    }

    /// GitHub answered `401` to the token, and re-reading the store did not
    /// change that: the credential is `invalid` until it accepts one.
    fn note_rejected(&self) {
        let mut health = self.health.lock().unwrap();
        if health.rejected() {
            warn!("{}", crate::credential::REJECTED);
        }
        if let Some(meta) = health.meta() {
            crate::credential::note_rejected(meta.stored_at_ms);
        }
    }

    /// GitHub accepted the token.
    fn note_accepted(&self) {
        if self.health.lock().unwrap().accepted() {
            info!("GitHub accepted the stored token again; claiming resumes");
        }
    }

    /// What one answer says about the permission `scope` needed.
    fn observe(&self, scope: Option<Scope<'_>>, status: u16, exhausted: bool, body: &str) {
        let Some(scope) = scope else { return };
        let mut health = self.health.lock().unwrap();
        if (200..300).contains(&status) || status == 304 {
            health.permitted(scope.repo, scope.permission);
        } else if is_permission_refusal(status, exhausted, body) {
            if let Some(warning) =
                health.refused(scope.repo, scope.permission, crate::audit::now_ms())
            {
                warn!("{warning}");
            }
        }
    }

    /// Append a write's audit line. Best effort, like every audit append: a
    /// logging failure must not fail the write it describes.
    async fn audit_write(&self, w: &Write<'_>, result: &str) {
        let Some(audit) = self.audit.get() else {
            return;
        };
        let record = Record::github_write(w.repo, w.action, w.reason, result, w.runner);
        if let Err(e) = audit.append(&record).await {
            warn!("audit: could not record a GitHub write: {e}");
        }
    }

    /// Send a built request with the current token, if any. A `401` re-reads the
    /// store once, and the request is sent again only if that produced a
    /// different token; a `401` that outlasts it makes the credential
    /// `invalid` and is returned for the caller to report. `build` is a closure
    /// so the request (headers and body included) can be rebuilt with the
    /// fresh token.
    ///
    /// A write is not re-sent on a different token: that token has not yet been
    /// confirmed to act as the served owner (`GET /user`), and a write under
    /// another account's token could land in that account's repository. The
    /// `401` is returned instead, and the next cycle confirms the token before
    /// anything is claimed on it ([`Self::claim_block`]).
    ///
    /// `idempotency` is forwarded to [`Self::send_with_token`], which owns the
    /// separate one-shot retry for a connection that died unanswered. The two
    /// compose multiplicatively but stay bounded and loop-free: at most two token
    /// attempts, each at most two sends.
    ///
    /// The worst case costs four [`REQUEST_TIMEOUT`] budgets, but only in theory:
    /// the retried failure is immediate (a FIN, not a stall), so a retry that
    /// happens costs a handshake and a timeout that happens is never retried.
    async fn send_with_auth<F>(
        &self,
        idempotency: Idempotency,
        build: F,
    ) -> Result<reqwest::Response, GithubError>
    where
        F: Fn(Option<&str>) -> reqwest::RequestBuilder,
    {
        let Auth::Token { token, store } = &self.auth else {
            return self.send_with_token(&build, None, idempotency).await;
        };
        let current = token.read().await.clone();
        let resp = self
            .send_with_token(&build, Some(current.expose()), idempotency)
            .await?;
        self.note_rate(&resp);
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_MODIFIED {
                self.note_accepted();
            }
            return Ok(resp);
        }
        let retaken = match store {
            Some(store) => self.take_from_store(store, token, store.stamp()).await,
            None => false,
        };
        if !retaken {
            self.note_rejected();
            return Ok(resp);
        }
        let read_only = build(None).build().is_ok_and(|r| r.method().is_safe());
        if !read_only && self.unconfirmed() {
            return Ok(resp);
        }
        let fresh = token.read().await.clone();
        let resp = self
            .send_with_token(&build, Some(fresh.expose()), idempotency)
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.note_rejected();
        } else if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            self.note_accepted();
        }
        Ok(resp)
    }

    /// One send under a fixed `token` (none for an anonymous client), re-sent at
    /// most once when the transport failed in the single way that proves GitHub
    /// never answered *and* that a fresh connection fixes (see
    /// [`is_retryable_transport`]).
    ///
    /// Exactly one extra attempt, never a loop. The failure this covers is a stale
    /// pooled connection, and the failed attempt is itself what evicts it, so the
    /// second send goes out on a connection the pool has just opened. If that one
    /// fails too then the pool was not the problem, and a third send would only
    /// spend the cycle's budget confirming it.
    ///
    /// [`REQUEST_TIMEOUT`] is a deadline reqwest applies inside each `send`, so the
    /// retry gets its own full budget rather than sharing the first attempt's —
    /// which is what keeps that constant's "applies *per request*" note true.
    ///
    /// Logged at `debug` rather than `warn`: the outcome here is a *recovery*, and
    /// the daemon's default level is `info`, so a `warn` would reproduce at the
    /// same cadence the very log flood this retry exists to end. At `debug` the
    /// signal is still there for anyone checking whether the race persists.
    async fn send_with_token<F>(
        &self,
        build: &F,
        token: Option<&str>,
        idempotency: Idempotency,
    ) -> Result<reqwest::Response, GithubError>
    where
        F: Fn(Option<&str>) -> reqwest::RequestBuilder,
    {
        let err = match build(token).send().await {
            Ok(resp) => return Ok(resp),
            Err(e) => e,
        };
        if idempotency == Idempotency::NotIdempotent || !is_retryable_transport(&err) {
            return Err(err.into());
        }
        debug!(
            "re-sending an idempotent request on a fresh connection: {}",
            with_causes(&err)
        );
        Ok(build(token).send().await?)
    }

    /// Workflow runs for `owner/repo` at a given run-level `status`
    /// (`queued`, `in_progress`, …).
    ///
    /// Run-level status diverges from job-level status: a run flips to
    /// `in_progress` as soon as *any* job starts, so a self-hosted job still
    /// `queued` under an otherwise-running multi-job run only surfaces if you
    /// also fetch `in_progress` runs and then inspect each run's *jobs*. Reconcile
    /// fetches both statuses and gates on the job's own status.
    pub async fn runs_with_status(
        &self,
        owner_repo: &str,
        status: &str,
    ) -> Result<Vec<WorkflowRun>, GithubError> {
        let first = format!("repos/{owner_repo}/actions/runs?status={status}&per_page={PAGE_SIZE}");
        let mut out = Vec::new();
        for n in 1..=LIST_PAGES {
            let page: RunsPage = self
                .get(
                    &page_path(&first, n),
                    Scope::of(owner_repo, Permission::ActionsRead),
                )
                .await?;
            let last = last_page(page.workflow_runs.len());
            out.extend(page.workflow_runs);
            if last {
                break;
            }
        }
        Ok(out)
    }

    /// The login of the authenticated user (faber's account owner) — used to
    /// scope topic discovery to repos this user owns.
    pub async fn current_login(&self) -> Result<String, GithubError> {
        let viewer: Viewer = self.get("user", None).await?;
        Ok(viewer.login)
    }

    /// The login the token acts as, and GitHub's own word on when it expires
    /// (the `github-authentication-token-expiration` header, when GitHub sends
    /// it), for `sudo faber auth set` to check a token before storing it.
    pub async fn token_owner(&self) -> Result<(String, Option<String>), GithubError> {
        let url = format!("{}/user", self.base);
        let resp = self
            .send_with_auth(Idempotency::Idempotent, |tok| {
                self.request(reqwest::Method::GET, &url, tok)
            })
            .await?;
        let status = resp.status();
        let expiry = resp
            .headers()
            .get("github-authentication-token-expiration")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(GithubError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let viewer: Viewer = serde_json::from_str(&body)?;
        Ok((viewer.login, expiry))
    }

    /// Jobs for a run, every page of them (#66). There is no repo-wide
    /// queued-jobs endpoint, so callers filter by `status`/`labels` client-side.
    pub async fn run_jobs(&self, owner_repo: &str, run_id: u64) -> Result<Vec<Job>, GithubError> {
        let first = format!("repos/{owner_repo}/actions/runs/{run_id}/jobs?per_page={PAGE_SIZE}");
        let mut out = Vec::new();
        for n in 1..=LIST_PAGES {
            let page: JobsPage = self
                .get(
                    &page_path(&first, n),
                    Scope::of(owner_repo, Permission::ActionsRead),
                )
                .await?;
            let last = last_page(page.jobs.len());
            out.extend(page.jobs);
            if last {
                break;
            }
        }
        Ok(out)
    }

    /// One workflow run by its id: the facts the job-start check judges a job
    /// by when the run is not in this cycle's scan (ADR-0059 §2).
    pub async fn run(&self, owner_repo: &str, run_id: u64) -> Result<WorkflowRun, GithubError> {
        self.get(
            &format!("repos/{owner_repo}/actions/runs/{run_id}"),
            Scope::of(owner_repo, Permission::ActionsRead),
        )
        .await
    }

    /// One job by its stable id, or `Ok(None)` when GitHub no longer has it (404 —
    /// e.g. the run was deleted or its logs expired). The authoritative
    /// terminal-state check for a *parked* claim (a job faber claimed but has not
    /// yet spawned a runner for): the reconcile loop's run scan is
    /// eventually-consistent, so "absent from the scan" is too weak to release a
    /// claim on — a still-`queued` job on a later page would be dropped and
    /// double-provisioned. A direct fetch is the trustworthy signal.
    pub async fn job(&self, owner_repo: &str, job_id: u64) -> Result<Option<Job>, GithubError> {
        match self
            .get::<Job>(
                &format!("repos/{owner_repo}/actions/jobs/{job_id}"),
                Scope::of(owner_repo, Permission::ActionsRead),
            )
            .await
        {
            Ok(job) => Ok(Some(job)),
            Err(GithubError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The self-hosted runners registered on `owner/repo`, with their live `busy`
    /// flags. Used to reclaim an *admitted* claim: faber matches its own runner by
    /// the `faber-{job_id}-` name prefix and kills it only when it is idle
    /// (`busy == false`). Every page, like the run and job scans (#66).
    pub async fn list_runners(&self, owner_repo: &str) -> Result<Vec<Runner>, GithubError> {
        let first = format!("repos/{owner_repo}/actions/runners?per_page={PAGE_SIZE}");
        let mut out = Vec::new();
        for n in 1..=LIST_PAGES {
            let page: RunnersPage = self
                .get(
                    &page_path(&first, n),
                    Scope::of(owner_repo, Permission::Administration),
                )
                .await?;
            let last = last_page(page.runners.len());
            out.extend(page.runners);
            if last {
                break;
            }
        }
        Ok(out)
    }

    /// The registration named exactly `name`, or `Ok(None)` when none exists.
    ///
    /// Filtered by GitHub (`?name=`) rather than picked out of [`Self::list_runners`],
    /// so the answer does not depend on the runner fitting on that listing's single
    /// page. Only ever asked for faber's own `faber-{job_id}-{ts}` names, which need
    /// no escaping — and refused, before a URL is built, for anything else (#105).
    pub async fn runner_named(
        &self,
        owner_repo: &str,
        name: &str,
    ) -> Result<Option<Runner>, GithubError> {
        check_runner_target(owner_repo, Some(name))?;
        let page: RunnersPage = self
            .get(
                &format!("repos/{owner_repo}/actions/runners?name={name}"),
                Scope::of(owner_repo, Permission::Administration),
            )
            .await?;
        Ok(page.runners.into_iter().find(|r| r.name == name))
    }

    /// Withdraw a runner's registration so GitHub can assign it nothing more — or
    /// learn, from GitHub refusing, that a job is already assigned to it.
    ///
    /// The refusal is the point (ADR-0017). A `DELETE` on a runner with a job
    /// assigned answers `422` ("… is currently running a job and cannot be
    /// deleted"), checked by GitHub when the request arrives rather than read from
    /// a listing fetched earlier. `id` is used when the caller minted the runner;
    /// otherwise the registration is found by exact name, and finding none is
    /// `Removed` — there is nothing left to assign a job to.
    ///
    /// Which runner to withdraw is the caller's to know, from its own record of the
    /// mint ([`crate::docker::MintedRunner`]). What is checked here is the form: a
    /// repo that is not `owner/repo`, or a name that is not faber's own, is refused
    /// before any request is built, whatever the caller was told (#105).
    pub async fn withdraw_runner(
        &self,
        owner_repo: &str,
        name: &str,
        id: Option<u64>,
        reason: &str,
    ) -> Result<Deregistration, GithubError> {
        check_runner_target(owner_repo, Some(name))?;
        let id = match id {
            Some(id) => id,
            None => match self.runner_named(owner_repo, name).await? {
                Some(runner) => runner.id,
                None => return Ok(Deregistration::Removed),
            },
        };
        let write = Write {
            repo: owner_repo,
            action: "withdraw_runner",
            reason,
            permission: Permission::Administration,
            runner: Some(name),
        };
        match self.delete_runner_as(&write, id).await {
            // `delete_runner` already reads a 404 as success.
            Ok(()) => Ok(Deregistration::Removed),
            Err(GithubError::Status { status: 422, .. }) => Ok(Deregistration::JobAssigned),
            Err(e) => Err(e),
        }
    }

    /// Deregister a self-hosted runner by id (`DELETE /actions/runners/{id}`). Used
    /// by the stale-runner sweep to reap faber's own JIT registrations that died
    /// `offline` without ever consuming their job — dead supply that would otherwise
    /// mask queued demand (see `runner_is_supply`) and clutter the runner list.
    /// Idempotent: a runner GitHub has already removed (404) is treated as success,
    /// so a concurrent peer's reap or GitHub's own GC never surfaces as an error.
    /// That tolerance is also what makes this safe to auto-re-send after a
    /// connection dies unanswered ([`Idempotency::Idempotent`]) — a re-sent DELETE
    /// whose predecessor actually landed comes back as exactly that 404.
    ///
    /// `reason` is why the caller deletes it, for the write's audit line.
    pub async fn delete_runner(
        &self,
        owner_repo: &str,
        runner_id: u64,
        reason: &str,
    ) -> Result<(), GithubError> {
        let write = Write {
            repo: owner_repo,
            action: "delete_runner",
            reason,
            permission: Permission::Administration,
            runner: None,
        };
        self.delete_runner_as(&write, runner_id).await
    }

    /// `DELETE` a runner registration as `write` says, audited.
    async fn delete_runner_as(&self, write: &Write<'_>, runner_id: u64) -> Result<(), GithubError> {
        if let Err(e) = check_runner_target(write.repo, None) {
            self.audit_write(write, &write_result(&e)).await;
            return Err(e);
        }
        let url = format!(
            "{}/repos/{}/actions/runners/{runner_id}",
            self.base, write.repo
        );
        let resp = match self
            .send_with_auth(Idempotency::Idempotent, |tok| {
                self.request(reqwest::Method::DELETE, &url, tok)
            })
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                self.audit_write(write, &write_result(&e)).await;
                return Err(e);
            }
        };
        let status = resp.status();
        let exhausted = rate_limit_exhausted(&resp);
        if !status.is_success() && status.as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            self.observe(write.scope(), status.as_u16(), exhausted, &body);
            self.audit_write(write, status.as_str()).await;
            return Err(GithubError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        self.observe(write.scope(), 200, false, "");
        // A 404 is the registration already gone: the outcome asked for.
        self.audit_write(write, "ok").await;
        Ok(())
    }

    /// Repos owned by `owner` carrying `topic` — faber's opt-in discovery signal.
    ///
    /// Complete or an error, never a short list. Callers read a repo missing
    /// from the answer as a repo that left the topic, and the janitor deletes its
    /// git mirror on that reading, so a partial answer passed off as `Ok` is a
    /// deletion: in October 2026 an intermittently short search took most of the
    /// mirrors with it. A page GitHub marks `incomplete_results`, or a set that
    /// does not add up to `total_count`, is refused, and every caller already
    /// treats a refused listing as "decide nothing from it this pass".
    pub async fn repos_with_topic(
        &self,
        owner: &str,
        topic: &str,
    ) -> Result<Vec<String>, GithubError> {
        let mut out: Vec<String> = Vec::new();
        let mut total = None;
        for n in 1..=SEARCH_PAGES {
            let page: SearchPage = self
                .get(
                    &format!(
                        "search/repositories?q=user:{owner}+topic:{topic}\
                         &per_page={SEARCH_PAGE_SIZE}&page={n}"
                    ),
                    None,
                )
                .await?;
            let last = search_page_fits(&page, &mut total, out.len())?;
            out.extend(page.items.into_iter().map(|r| r.full_name));
            if last {
                return Ok(out);
            }
        }
        Err(GithubError::Unexpected(format!(
            "the topic search for {owner} did not end within {SEARCH_PAGES} pages"
        )))
    }

    /// The open pull requests of `owner_repo`, for the git mirror's refspec
    /// (ADR-0035 §1). Which of them are same-repo is decided by the caller from
    /// `head.repo`, because a ref name cannot tell a fork's head from a branch's.
    pub async fn open_pulls(&self, owner_repo: &str) -> Result<Vec<OpenPull>, GithubError> {
        let mut out = Vec::new();
        for page in 1..=OPEN_PULL_PAGES {
            let batch: Vec<OpenPull> = self
                .get(
                    &format!("repos/{owner_repo}/pulls?state=open&per_page=100&page={page}"),
                    Scope::of(owner_repo, Permission::PullRequestsRead),
                )
                .await?;
            let last = batch.len() < 100;
            out.extend(batch);
            if last {
                break;
            }
        }
        Ok(out)
    }

    /// Pull request `number` of `owner_repo`: its author and head, for the gate
    /// (ADR-0059 §3). ETag-cached, so a run judged every poll costs a `304`
    /// while the pull request is unchanged.
    pub async fn pull(&self, owner_repo: &str, number: u64) -> Result<Pull, GithubError> {
        check_gate_target(owner_repo, None)?;
        self.get(
            &format!("repos/{owner_repo}/pulls/{number}"),
            Scope::of(owner_repo, Permission::PullRequestsRead),
        )
        .await
    }

    /// Every pull request of `owner_repo`, open or closed, whose head is
    /// `branch` in `head_owner`'s repository. A fork's `pull_request` run names
    /// none (its `pull_requests` is empty), so the gate finds it by its head
    /// (ADR-0059 §3). Paginated, and an error rather than part of the list past
    /// [`PULLS_FROM_PAGES`].
    pub async fn pulls_from(
        &self,
        owner_repo: &str,
        head_owner: &str,
        branch: &str,
    ) -> Result<Vec<Pull>, GithubError> {
        check_gate_target(owner_repo, Some(head_owner))?;
        let head = escape_ref(&format!("{head_owner}:{branch}"));
        let mut out = Vec::new();
        for page in 1..=PULLS_FROM_PAGES {
            let batch: Vec<Pull> = self
                .get(
                    &format!(
                        "repos/{owner_repo}/pulls?state=all&head={head}&per_page=100&page={page}"
                    ),
                    Scope::of(owner_repo, Permission::PullRequestsRead),
                )
                .await?;
            let last = batch.len() < 100;
            out.extend(batch);
            if last {
                return Ok(out);
            }
        }
        Err(GithubError::Unexpected(format!(
            "the pull requests from {head_owner}:{branch} in {owner_repo} did not end within \
             {PULLS_FROM_PAGES} pages"
        )))
    }

    /// Whether `login` is a collaborator on `owner_repo`: GitHub's own record
    /// (`204` or `404`), which covers an organization's owners, its members who
    /// reach the repository through a team or the base permission, and outside
    /// collaborators (ADR-0059 §3). It needs only Metadata: read, which every
    /// fine-grained token holds.
    ///
    /// The answer is kept for [`COLLABORATOR_TTL_MS`]. Anything but `204` or
    /// `404` is an error, never a guess either way.
    pub async fn is_collaborator(
        &self,
        owner_repo: &str,
        login: &str,
    ) -> Result<bool, GithubError> {
        check_gate_target(owner_repo, Some(login))?;
        let key = (owner_repo.to_lowercase(), login.to_lowercase());
        let now = crate::audit::now_ms();
        if let Some(&(answer, at)) = self.collaborators.lock().unwrap().get(&key) {
            if now.saturating_sub(at) < COLLABORATOR_TTL_MS {
                return Ok(answer);
            }
        }
        let url = format!("{}/repos/{owner_repo}/collaborators/{login}", self.base);
        let resp = self
            .send_with_auth(Idempotency::Idempotent, |tok| {
                self.request(reqwest::Method::GET, &url, tok)
            })
            .await?;
        let answer = match resp.status().as_u16() {
            204 => true,
            404 => false,
            status => {
                let body = resp.text().await.unwrap_or_default();
                return Err(GithubError::Status { status, url, body });
            }
        };
        let mut cache = self.collaborators.lock().unwrap();
        if cache.len() >= MAX_COLLABORATOR_ENTRIES {
            cache.retain(|_, (_, at)| now.saturating_sub(*at) < COLLABORATOR_TTL_MS);
            if cache.len() >= MAX_COLLABORATOR_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(key, (answer, now));
        Ok(answer)
    }

    /// The entries of `owner_repo`'s merge queue for `branch`, in the order
    /// GitHub lists them, over GraphQL (ADR-0059 §3); empty when the branch has
    /// no queue. Paginated, and an error past [`MERGE_QUEUE_PAGES`] or on an
    /// entry GitHub will not show, since the gate binds a run to an entry by
    /// commit and judges every entry ahead of it.
    pub async fn merge_queue(
        &self,
        owner_repo: &str,
        branch: &str,
    ) -> Result<Vec<QueueEntry>, GithubError> {
        check_gate_target(owner_repo, None)?;
        let (owner, name) = owner_repo
            .split_once('/')
            .ok_or_else(|| GithubError::Refused(owner_repo.to_string()))?;
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MERGE_QUEUE_PAGES {
            let variables = serde_json::json!({
                "owner": owner, "name": name, "branch": branch, "after": after,
            });
            let data: QueueData = self
                .graphql(
                    MERGE_QUEUE_QUERY,
                    variables,
                    Scope::of(owner_repo, Permission::MergeQueuesRead),
                )
                .await?;
            let repository = data.repository.ok_or_else(|| {
                GithubError::Unexpected(format!("GitHub's GraphQL API has no {owner_repo}"))
            })?;
            let Some(queue) = repository.merge_queue else {
                return Ok(out);
            };
            let page = queue.entries;
            for node in page.nodes {
                let node = node.ok_or_else(|| {
                    GithubError::Unexpected(format!(
                        "the merge queue of {owner_repo} holds an entry GitHub will not show"
                    ))
                })?;
                out.push(QueueEntry::from(node));
            }
            if !page.page_info.has_next_page {
                return Ok(out);
            }
            after = Some(page.page_info.end_cursor.ok_or_else(|| {
                GithubError::Unexpected(format!(
                    "the merge queue of {owner_repo} has a next page but no cursor to it"
                ))
            })?);
        }
        Err(GithubError::Unexpected(format!(
            "the merge queue of {owner_repo} did not end within {MERGE_QUEUE_PAGES} pages"
        )))
    }

    /// Whether `branch` of `owner_repo` contains commit `sha`: GitHub's compare
    /// says `identical` or `behind` (ADR-0059 §3, a merge group that left its
    /// queue having merged). Needs Contents: read. ETag-cached.
    pub async fn branch_contains(
        &self,
        owner_repo: &str,
        branch: &str,
        sha: &str,
    ) -> Result<bool, GithubError> {
        check_gate_target(owner_repo, None)?;
        if !is_commit_id(sha) {
            return Err(GithubError::Refused(format!(
                "commit {:?}, which is not a commit id",
                crate::untrusted::for_log(sha, 120)
            )));
        }
        let compared: Comparison = self
            .get(
                &format!(
                    "repos/{owner_repo}/compare/{}...{sha}?per_page=1",
                    escape_ref(branch)
                ),
                Scope::of(owner_repo, Permission::ContentsRead),
            )
            .await?;
        match compared.status.as_str() {
            "identical" | "behind" => Ok(true),
            "ahead" | "diverged" => Ok(false),
            other => Err(GithubError::Unexpected(format!(
                "comparing {sha} with {branch} in {owner_repo} answered {:?}",
                crate::untrusted::for_log(other, 40)
            ))),
        }
    }

    /// The size of `owner_repo` in KiB as GitHub reports it, which the git
    /// mirror's first clone is weighed by (ADR-0035 §5).
    pub async fn repo_size_kib(&self, owner_repo: &str) -> Result<u64, GithubError> {
        let repo: RepoSize = self.get(&format!("repos/{owner_repo}"), None).await?;
        Ok(repo.size)
    }

    /// The token faber authenticates with, for the git mirror's refresh, which
    /// fetches over HTTPS rather than through this client (ADR-0035 §1). It
    /// leaves this process only on a fill container's stdin, into a `0600` file
    /// in a named volume mounted read-only into the refresh container, never as
    /// an argument, a variable or a host file.
    pub(crate) async fn token_for_git(&self) -> Secret {
        match &self.auth {
            Auth::Token { token, .. } => token.read().await.clone(),
            Auth::Anonymous => Secret::new(String::new()),
        }
    }

    /// Mint a just-in-time, single-use runner config for `owner/repo`. The token
    /// expires ~60 minutes after generation, so call this immediately before
    /// spawning the runner. `runner_group_id` is `1` (the default group) for a
    /// repo-level runner on a personal account.
    ///
    /// A 5xx answer starts or lengthens the [`MintBackoff`] that
    /// [`Self::mint_turn`] reads, and a success clears it (#89).
    ///
    /// `reason` is why the caller mints it, for the write's audit line, which
    /// never holds the config itself.
    pub async fn generate_jitconfig(
        &self,
        owner_repo: &str,
        name: &str,
        labels: &[String],
        work_folder: &str,
        reason: &str,
    ) -> Result<JitConfig, GithubError> {
        let body = serde_json::json!({
            "name": name,
            "runner_group_id": 1,
            "labels": labels,
            "work_folder": work_folder,
        });
        let write = Write {
            repo: owner_repo,
            action: "mint_runner",
            reason,
            permission: Permission::Administration,
            runner: Some(name),
        };
        let resp: Result<JitConfigResponse, _> = self
            .post(
                &format!("repos/{owner_repo}/actions/runners/generate-jitconfig"),
                &body,
                &write,
            )
            .await;
        {
            let mut backoff = self.mint_backoff.lock().unwrap();
            match &resp {
                Ok(_) => backoff.succeeded(),
                Err(GithubError::Status { status, .. }) if *status >= 500 => {
                    backoff.failed(crate::audit::now_ms())
                }
                Err(_) => {}
            }
        }
        let resp = resp?;
        Ok(JitConfig {
            encoded: resp.encoded_jit_config,
            runner_id: resp.runner.map(|r| r.id),
        })
    }

    /// Take a turn to mint a runner at `now_ms`, or learn how many ms are left
    /// before one may (see [`MintBackoff`]). A provision takes it once it has won
    /// admission, immediately before the mint: a turn taken any earlier would let
    /// every claim parked in admission mint the moment it is admitted, however
    /// many mints have failed since.
    pub fn mint_turn(&self, now_ms: u64) -> Result<(), u64> {
        self.mint_backoff.lock().unwrap().turn(now_ms)
    }

    /// How many ms are left before a turn may be taken at `now_ms`, without
    /// taking one: `None` when a mint may go now. A provision waits on this
    /// before admission, so a claim waiting out the backoff holds no memory or
    /// disk.
    pub fn mint_wait(&self, now_ms: u64) -> Option<u64> {
        self.mint_backoff.lock().unwrap().wait_ms(now_ms)
    }

    /// `generate-jitconfig` 5xx answers in a row since the last success.
    pub fn mint_failures(&self) -> u32 {
        self.mint_backoff.lock().unwrap().failures()
    }

    /// The annotation messages on a completed job's check run, for the
    /// runner-retirement sampler (#64). It is no longer read by the retry sweep,
    /// which judges by faber's own record (ADR-0003, amendment 2026-10-18). The
    /// parsing is pure and lives in `runner_retirement`; this is only the fetch.
    ///
    /// `check_run_url` is the value from the [`Job`]; its trailing segment is the
    /// check-run id. An absent/empty url (a job that never got a check run) yields
    /// `Ok(None)` — no evidence either way, which the sweep treats as nothing to
    /// retry. An annotation without a message is kept as an empty string, so it
    /// still counts as "the job was annotated".
    pub async fn job_annotations(
        &self,
        owner_repo: &str,
        check_run_url: &str,
    ) -> Result<Option<Vec<String>>, GithubError> {
        let Some(id) = check_run_url
            .rsplit('/')
            .find(|s| !s.is_empty())
            .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
        else {
            return Ok(None);
        };
        let anns: Vec<Annotation> = self
            .get(
                &format!("repos/{owner_repo}/check-runs/{id}/annotations?per_page=100"),
                Scope::of(owner_repo, Permission::ChecksRead),
            )
            .await?;
        Ok(Some(
            anns.into_iter()
                .map(|a| a.message.unwrap_or_default())
                .collect(),
        ))
    }

    /// Re-run a single completed job (and its dependents). Used by the retry sweep
    /// for a run with a *mix* of faber-deaths and genuine failures, so only the
    /// faber-caused job is re-run and the genuine one is left untouched. Requires
    /// Actions: write.
    pub async fn rerun_job(
        &self,
        owner_repo: &str,
        job_id: u64,
        reason: &str,
    ) -> Result<(), GithubError> {
        let write = Write {
            repo: owner_repo,
            action: "rerun_job",
            reason,
            permission: Permission::ActionsWrite,
            runner: None,
        };
        self.post_empty(
            &format!("repos/{owner_repo}/actions/jobs/{job_id}/rerun"),
            &write,
        )
        .await
    }

    /// Re-run every failed job of a completed run in one call. Used when *all* of a
    /// run's failed jobs are faber-deaths (the redeploy case), so a whole run's
    /// worth of runner-deaths recovers in a single attempt without a per-job storm.
    pub async fn rerun_failed_jobs(
        &self,
        owner_repo: &str,
        run_id: u64,
        reason: &str,
    ) -> Result<(), GithubError> {
        let write = Write {
            repo: owner_repo,
            action: "rerun_failed_jobs",
            reason,
            permission: Permission::ActionsWrite,
            runner: None,
        };
        self.post_empty(
            &format!("repos/{owner_repo}/actions/runs/{run_id}/rerun-failed-jobs"),
            &write,
        )
        .await
    }

    /// Cancel workflow run `run_id`: a `skip` of a likely-wasted claim (ADR-0063
    /// §6). GitHub has no per-job cancel. Requires Actions: write.
    pub async fn cancel_run(
        &self,
        owner_repo: &str,
        run_id: u64,
        reason: &str,
    ) -> Result<(), GithubError> {
        let write = Write {
            repo: owner_repo,
            action: "cancel_run",
            reason,
            permission: Permission::ActionsWrite,
            runner: None,
        };
        self.post_empty(
            &format!("repos/{owner_repo}/actions/runs/{run_id}/cancel"),
            &write,
        )
        .await
    }

    /// Latest published `actions/runner` version (without the leading `v`). A JIT
    /// runner that is too stale self-updates then exits *without* running the job,
    /// so faber always provisions the current release.
    pub async fn latest_runner_version(&self) -> Result<String, GithubError> {
        let release: Release = self
            .get("repos/actions/runner/releases/latest", None)
            .await?;
        Ok(release.tag_name.trim_start_matches('v').to_string())
    }

    /// The `actions/runner` release tagged exactly `v{version}`, for the tools
    /// disk (ADR-0026): its body, which carries the per-platform SHA-256 GitHub
    /// publishes, and its assets' names and API digests. By tag, never
    /// `/releases/latest`, because the version is the one a job pins. `Ok(None)`
    /// when there is no such tag (404). The caller validates `version` first; it
    /// is built into the URL as it is.
    pub async fn runner_release(
        &self,
        version: &str,
    ) -> Result<Option<RunnerRelease>, GithubError> {
        match self
            .get::<RunnerRelease>(
                &format!("repos/actions/runner/releases/tags/v{version}"),
                None,
            )
            .await
        {
            Ok(release) => Ok(Some(release)),
            Err(GithubError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every release of `owner_repo` GitHub will list, newest-created first,
    /// paginated (100 per page, at most [`MAX_RELEASE_PAGES`] pages). Callers
    /// choose among them by semver themselves (ADR-0031): `/releases/latest`
    /// is deliberately not used, because it is a mutable pointer a token able
    /// to delete a release can move to an older build. Drafts are invisible
    /// to an anonymous reader and are filtered by the caller regardless.
    /// An empty list when the repo has no releases (404).
    pub async fn list_releases(&self, owner_repo: &str) -> Result<Vec<Release>, GithubError> {
        let mut all = Vec::new();
        for page in 1..=MAX_RELEASE_PAGES {
            let batch = match self
                .get::<Vec<Release>>(
                    &format!("repos/{owner_repo}/releases?per_page=100&page={page}"),
                    None,
                )
                .await
            {
                Ok(b) => b,
                Err(GithubError::Status { status: 404, .. }) if page == 1 => return Ok(all),
                Err(e) => return Err(e),
            };
            let full = crate::update::more_release_pages(batch.len());
            all.extend(batch);
            if !full {
                break;
            }
        }
        Ok(all)
    }

    /// A release by its exact tag, e.g. `"v0.2.0"` (ADR-0006: `update.pin`
    /// holds a host at a specific version). Unlike [`Self::list_releases`] callers,
    /// the tag endpoint does *not* filter drafts/pre-releases on its own, so
    /// a pinned draft would be returned here — accepted as the operator's
    /// explicit choice, since a pin is opt-in and named exactly, unlike the
    /// "track latest" default. `Ok(None)` when the tag doesn't exist (404).
    pub async fn release_by_tag(
        &self,
        owner_repo: &str,
        tag: &str,
    ) -> Result<Option<Release>, GithubError> {
        match self
            .get::<Release>(&format!("repos/{owner_repo}/releases/tags/{tag}"), None)
            .await
        {
            Ok(release) => Ok(Some(release)),
            Err(GithubError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The commit a tag points to, or `Ok(None)` when the tag does not exist
    /// (404). Release verification pins the attestation's source commit to this
    /// (ADR-0031): the release object's own `target_commitish` can be a branch
    /// name, so the tag ref itself is read.
    ///
    /// `git/ref/tags/{tag}` is an exact match (unlike `git/refs/`, which matches
    /// by prefix). A lightweight tag's ref points at the commit directly; an
    /// annotated tag's points at a tag object, which is peeled through
    /// `git/tags/{sha}` until a commit appears. A tag of a tree or blob, or a
    /// chain deeper than [`MAX_TAG_PEELS`], is an error rather than a guess.
    pub async fn tag_commit(
        &self,
        owner_repo: &str,
        tag: &str,
    ) -> Result<Option<String>, GithubError> {
        // Both go into the URL path as they are, and the tag is the release's
        // own text: refuse anything that could steer the request elsewhere.
        let tag_ok = !tag.is_empty()
            && !tag.contains("..")
            && tag
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
        if !crate::untrusted::repo_slug(owner_repo) || !tag_ok {
            return Err(GithubError::Refused(format!(
                "tag {:?} of {:?}",
                tag.chars().take(64).collect::<String>(),
                owner_repo.chars().take(64).collect::<String>()
            )));
        }
        let mut object = match self
            .get::<GitRef>(&format!("repos/{owner_repo}/git/ref/tags/{tag}"), None)
            .await
        {
            Ok(r) => r.object,
            Err(GithubError::Status { status: 404, .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        for _ in 0..MAX_TAG_PEELS {
            // Every sha here is the API's text: it is compared against the
            // attestation, or put into the next request's path. Only a full
            // lowercase SHA-1 is either.
            let sha_ok = object.sha.len() == 40
                && object
                    .sha
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !sha_ok {
                return Err(GithubError::Refused(format!(
                    "tag {tag} of {owner_repo}: object sha {:?} is not 40 lowercase hex",
                    object.sha.chars().take(64).collect::<String>()
                )));
            }
            match object.kind.as_str() {
                "commit" => return Ok(Some(object.sha)),
                "tag" => {
                    object = self
                        .get::<GitTag>(&format!("repos/{owner_repo}/git/tags/{}", object.sha), None)
                        .await?
                        .object;
                }
                other => {
                    return Err(GithubError::Unexpected(format!(
                        "tag {tag} of {owner_repo} points at a {other}, not a commit"
                    )))
                }
            }
        }
        Err(GithubError::Unexpected(format!(
            "tag {tag} of {owner_repo} is nested more than {MAX_TAG_PEELS} tag objects deep"
        )))
    }

    /// Raw bytes of one release asset (`GET /releases/assets/{id}` with
    /// `Accept: application/octet-stream`). Not ETag-cached — a large binary
    /// download is not something to hold in the ETag body cache, and it is
    /// fetched exactly once per staged version anyway.
    ///
    /// GitHub answers this endpoint with a redirect to short-lived storage
    /// (routinely a pre-signed S3 URL) rather than the bytes directly, and
    /// that matters more than it sounds like it should: `reqwest`'s default
    /// client follows redirects and would carry the `Authorization: Bearer
    /// <token>` header onto that URL, which S3 treats as an unrecognized,
    /// hostile header and rejects — and even if it didn't, forwarding a live
    /// GitHub token to a host faber does not control is a real credential
    /// leak regardless of how the target treats it. So this method uses a
    /// one-off client with redirects disabled, sends exactly one authed
    /// request, and — on a 3xx — follows `Location` itself with a *plain*,
    /// unauthenticated GET.
    ///
    /// Both of those bespoke clients must set [`USER_AGENT`] explicitly. A
    /// client built here inherits nothing from [`http_client`], and GitHub
    /// answers a `User-Agent`-less API call with a 403 whose body is about
    /// administrative rules — which reads like a permissions problem and is
    /// not one. This cost a published release that could not update itself.
    pub async fn download_release_asset(
        &self,
        owner_repo: &str,
        asset_id: u64,
    ) -> Result<Vec<u8>, GithubError> {
        let url = format!(
            "{}/repos/{owner_repo}/releases/assets/{asset_id}",
            self.base
        );
        let no_redirect = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(ASSET_DOWNLOAD_TIMEOUT)
            .build()?;
        let resp = self
            .send_with_auth(Idempotency::Idempotent, |tok| {
                with_auth(no_redirect.request(reqwest::Method::GET, &url), tok)
                    .header("Accept", "application/octet-stream")
                    .header("X-GitHub-Api-Version", "2022-11-28")
            })
            .await?;

        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| GithubError::Status {
                    status: status.as_u16(),
                    url: url.clone(),
                    body: "redirected with no Location header".to_string(),
                })?;
            let plain = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(ASSET_DOWNLOAD_TIMEOUT)
                .build()?;
            let asset_resp = plain.get(&location).send().await?;
            let asset_status = asset_resp.status();
            if !asset_status.is_success() {
                return Err(GithubError::Status {
                    status: asset_status.as_u16(),
                    url: location,
                    body: asset_resp.text().await.unwrap_or_default(),
                });
            }
            return Ok(asset_resp.bytes().await?.to_vec());
        }

        if !status.is_success() {
            return Err(GithubError::Status {
                status: status.as_u16(),
                url,
                body: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// Fetch one repo file's raw bytes at a specific ref (branch, tag, or SHA), or
    /// `Ok(None)` when the file does not exist there (404).
    ///
    /// Used by drift detection to read `.faber/config.toml` and `package-lock.json`
    /// at a job's [`WorkflowRun::content_ref`] — which, for a `pull_request` run,
    /// is `refs/pull/<N>/merge` rather than a bare SHA, so `at` can contain `/`.
    /// [`contents_url`] appends it as a query parameter rather than
    /// string-interpolating it, so those slashes are encoded (and decoded back by
    /// GitHub) rather than relying on `/` happening to be a legal raw byte in a
    /// URL query. Requests the **`raw` media type** rather than the default JSON
    /// envelope: the contents API only inlines (base64) files up to 1 MB and
    /// returns `encoding: "none"` with an empty body above that, so a large
    /// monorepo lockfile would read as unreadable. The raw type streams the file
    /// directly for any size up to 100 MB. Not ETag-cached (drift checks are
    /// infrequent and the raw body isn't a JSON envelope), so it always reflects
    /// the ref's current bytes.
    pub async fn get_file_at_ref(
        &self,
        owner_repo: &str,
        path: &str,
        at: &str,
    ) -> Result<Option<Vec<u8>>, GithubError> {
        let base_url = contents_url(&self.base, owner_repo, path);
        // Build the request directly (not via `request`, which pins the JSON
        // `Accept`) so the single `Accept` header is the raw media type. `at`
        // goes through `.query(...)`, not string interpolation — see
        // [`contents_url`] for why.
        let resp = self
            .send_with_auth(Idempotency::Idempotent, |tok| {
                with_auth(
                    self.http
                        .request(reqwest::Method::GET, &base_url)
                        .query(&[("ref", at)]),
                    tok,
                )
                .header("Accept", "application/vnd.github.raw")
                .header("X-GitHub-Api-Version", "2022-11-28")
            })
            .await?;
        let status = resp.status();
        let scope = Scope::of(owner_repo, Permission::ContentsRead);
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let exhausted = rate_limit_exhausted(&resp);
            let body = resp.text().await.unwrap_or_default();
            self.observe(scope, status.as_u16(), exhausted, &body);
            return Err(GithubError::Status {
                status: status.as_u16(),
                url: format!("{base_url}?ref={at}"),
                body,
            });
        }
        self.observe(scope, status.as_u16(), false, "");
        Ok(Some(resp.bytes().await?.to_vec()))
    }

    /// Post a commit status on `sha`. Drift detection uses this to surface a
    /// mismatch as a `failure` on the head commit (`context = faber/playwright-drift`).
    /// `description` (≤ 140 chars, GitHub truncates) is optional; `reason` is
    /// why, for the write's audit line.
    pub async fn post_commit_status(
        &self,
        owner_repo: &str,
        sha: &str,
        state: &str,
        context: &str,
        description: Option<&str>,
        reason: &str,
    ) -> Result<(), GithubError> {
        let write = Write {
            repo: owner_repo,
            action: "commit_status",
            reason,
            permission: Permission::StatusesWrite,
            runner: None,
        };
        let mut body = serde_json::json!({ "state": state, "context": context });
        if let Some(d) = description {
            body["description"] = serde_json::Value::String(d.to_string());
        }
        // The created-status object is returned but of no use to faber.
        let _: serde_json::Value = self
            .post(&format!("repos/{owner_repo}/statuses/{sha}"), &body, &write)
            .await?;
        Ok(())
    }

    /// ETag-aware GET. Returns the cached body on `304 Not Modified`. `scope`
    /// is the repository and permission the call needs, when it needs one, so
    /// a refusal is learned ([`Self::observe`]).
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        scope: Option<Scope<'_>>,
    ) -> Result<T, GithubError> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        // Capture the whole prior entry (a clone) up front, not just its etag, so
        // this call is self-contained: on a 304 we replay the body we already hold
        // rather than re-reading the cache — which, now that it evicts, could have
        // dropped the entry in between and turned a valid 304 into a spurious
        // `StaleCache` error.
        let prior = self.etags.lock().await.lookup(&url);

        let resp = self
            .send_with_auth(Idempotency::Idempotent, |tok| {
                let mut req = self.request(reqwest::Method::GET, &url, tok);
                if let Some(entry) = &prior {
                    req = req.header("If-None-Match", entry.etag.clone());
                }
                req
            })
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            self.observe(scope, 304, false, "");
            // GitHub only 304s a conditional request, so `prior` is present in
            // practice; the `None` arm stays as a defensive fallback.
            return match prior {
                Some(entry) => Ok(serde_json::from_str(&entry.body)?),
                None => Err(GithubError::StaleCache(url)),
            };
        }

        let status = resp.status();
        let exhausted = rate_limit_exhausted(&resp);
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = resp.text().await?;
        self.observe(scope, status.as_u16(), exhausted, &body);
        if !status.is_success() {
            return Err(GithubError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        if let Some(etag) = etag {
            self.etags.lock().await.store(url, etag, body.clone());
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// POST a JSON body, decoding a JSON response. Every POST faber makes is a
    /// write, so it takes the [`Write`] its audit line is made from, and
    /// records it however the request ends.
    ///
    /// Pinned to [`Idempotency::NotIdempotent`], and not parameterised, so that no
    /// future caller can hand faber's POST path a retry it should not have. A POST
    /// whose connection dies unanswered is genuinely ambiguous — GitHub may have
    /// processed it — and the worst case here is not a duplicate record but a
    /// second *runner*: [`Self::generate_jitconfig`] mints a JIT registration on
    /// every call, so a blind re-send is how one job ends up with two runners, one
    /// of which then runs somebody else's job and one of which is orphaned.
    /// [`Self::post_commit_status`], the other caller, is effectively harmless to
    /// repeat (GitHub collapses statuses per `context`), but it is not worth an
    /// exception that would make this path's rule "usually no".
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &serde_json::Value,
        write: &Write<'_>,
    ) -> Result<T, GithubError> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let resp = match self
            .send_with_auth(Idempotency::NotIdempotent, |tok| {
                self.request(reqwest::Method::POST, &url, tok).json(body)
            })
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                self.audit_write(write, &write_result(&e)).await;
                return Err(e);
            }
        };
        let status = resp.status();
        let exhausted = rate_limit_exhausted(&resp);
        let text = match resp.text().await {
            Ok(text) => text,
            Err(e) => {
                let e = GithubError::from(e);
                self.audit_write(write, &write_result(&e)).await;
                return Err(e);
            }
        };
        self.observe(write.scope(), status.as_u16(), exhausted, &text);
        if !status.is_success() {
            self.audit_write(write, status.as_str()).await;
            return Err(GithubError::Status {
                status: status.as_u16(),
                url,
                body: text,
            });
        }
        self.audit_write(write, "ok").await;
        Ok(serde_json::from_str(&text)?)
    }

    /// POST with no request or response body (a re-run), audited as [`Self::post`] is.
    ///
    /// [`Idempotency::NotIdempotent`] for the same reason as [`Self::post`], and
    /// on its own merits: [`Self::rerun_job`] and [`Self::rerun_failed_jobs`] each
    /// create a *new run attempt* per call, so a silent re-send would burn a slot
    /// against the auto-retry attempt cap (ADR-0003) for work already under way.
    /// An empty body makes a request cheap to repeat, not safe to repeat.
    async fn post_empty(&self, path: &str, write: &Write<'_>) -> Result<(), GithubError> {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let resp = match self
            .send_with_auth(Idempotency::NotIdempotent, |tok| {
                self.request(reqwest::Method::POST, &url, tok)
            })
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                self.audit_write(write, &write_result(&e)).await;
                return Err(e);
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let exhausted = rate_limit_exhausted(&resp);
            let body = resp.text().await.unwrap_or_default();
            self.observe(write.scope(), status.as_u16(), exhausted, &body);
            self.audit_write(write, status.as_str()).await;
            return Err(GithubError::Status {
                status: status.as_u16(),
                url,
                body,
            });
        }
        self.observe(write.scope(), status.as_u16(), false, "");
        self.audit_write(write, "ok").await;
        Ok(())
    }

    /// One GraphQL query, decoding its `data`. A read sent as a POST: idempotent,
    /// so a connection that died unanswered is re-sent once, and never audited
    /// as a write.
    ///
    /// GitHub answers a field the token may not read with `200` and a
    /// `FORBIDDEN` error. That is returned as the `403` it stands for, and
    /// learned as one ([`Self::observe`]), so the caller and the token's health
    /// see any other refusal of `scope`'s permission.
    async fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
        scope: Option<Scope<'_>>,
    ) -> Result<T, GithubError> {
        let url = format!("{}/graphql", self.base);
        let body = serde_json::json!({ "query": query, "variables": variables });
        let resp = self
            .send_with_auth(Idempotency::Idempotent, |tok| {
                self.request(reqwest::Method::POST, &url, tok).json(&body)
            })
            .await?;
        let status = resp.status();
        let exhausted = rate_limit_exhausted(&resp);
        let text = resp.text().await?;
        if !status.is_success() {
            self.observe(scope, status.as_u16(), exhausted, &text);
            return Err(GithubError::Status {
                status: status.as_u16(),
                url,
                body: text,
            });
        }
        let answer: GraphqlAnswer<T> = serde_json::from_str(&text)?;
        let first = |kind: &str| {
            answer
                .errors
                .iter()
                .find(|e| e.kind.as_deref() == Some(kind))
        };
        // In the words `is_permission_refusal` and a rate-limit check read.
        let refusal = if let Some(e) = first("FORBIDDEN") {
            Some((403, format!("Resource not accessible: {}", e.message)))
        } else if let Some(e) = first("RATE_LIMITED") {
            Some((403, format!("API rate limit exceeded: {}", e.message)))
        } else {
            first("NOT_FOUND").map(|e| (404, e.message.clone()))
        };
        if let Some((status, body)) = refusal {
            self.observe(scope, status, false, &body);
            return Err(GithubError::Status { status, url, body });
        }
        if let Some(e) = answer.errors.first() {
            return Err(GithubError::Unexpected(format!(
                "GitHub's GraphQL API answered: {}",
                crate::untrusted::for_log(&e.message, 200)
            )));
        }
        self.observe(scope, 200, false, "");
        answer.data.ok_or_else(|| {
            GithubError::Unexpected("GitHub's GraphQL API answered with no data".to_string())
        })
    }

    /// A request builder with faber's standard auth and API-version headers.
    fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        token: Option<&str>,
    ) -> reqwest::RequestBuilder {
        with_auth(self.http.request(method, url), token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }
}

/// `req` with the token as its bearer credential, or as it is when there is
/// none (the anonymous updater).
fn with_auth(req: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(token) => req.header("Authorization", format!("Bearer {token}")),
        None => req,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    /// The two stamps the retry sweep windows an OOM against are `#[serde(default)]`,
    /// so a misspelt field name would deserialize to `None` forever rather than
    /// failing — silently switching the window check off with nothing to notice.
    /// Pin them against a payload of the shape GitHub sends for a finished job
    /// (the window an OOM is matched against, ADR-0003), field names and all.
    #[test]
    fn a_jobs_dispatch_and_finish_stamps_deserialize() {
        let job: Job = serde_json::from_str(
            r#"{
                "id": 1001,
                "name": "build",
                "status": "completed",
                "conclusion": "failure",
                "runner_name": "faber-1001-1768478390000",
                "started_at": "2026-01-15T12:00:00Z",
                "completed_at": "2026-01-15T12:00:30Z",
                "check_run_url": "https://api.github.com/repos/owner/demo/check-runs/1001"
            }"#,
        )
        .expect("the shape GitHub actually sends");
        assert_eq!(job.started_at.as_deref(), Some("2026-01-15T12:00:00Z"));
        assert_eq!(job.completed_at.as_deref(), Some("2026-01-15T12:00:30Z"));
        // Still absent-tolerant: a queued job carries neither, and `labels`/`steps`
        // are omitted by the endpoints that don't populate them.
        let queued: Job = serde_json::from_str(r#"{"id": 1, "name": "build", "status": "queued"}"#)
            .expect("a queued job omits both stamps");
        assert_eq!(queued.started_at, None);
        assert_eq!(queued.completed_at, None);
    }

    /// A minimal `WorkflowRun` fixture, `event` and `pull_requests` overridable by
    /// the caller — the only two fields [`WorkflowRun::content_ref`] reads.
    fn run(event: &str, pull_requests: Vec<PullRequestRef>) -> WorkflowRun {
        WorkflowRun {
            id: 1,
            event: event.to_string(),
            workflow_id: 1,
            name: None,
            status: None,
            actor: None,
            triggering_actor: None,
            repository: MinimalRepo {
                full_name: "owner/demo".to_string(),
                fork: false,
                private: true,
            },
            head_repository: None,
            head_branch: None,
            pull_requests,
            head_sha: "deadbeef".to_string(),
            run_attempt: 1,
            updated_at: None,
            created_at: None,
        }
    }

    /// The bug this whole change fixes: a same-repo `pull_request` run's checked-
    /// out tree is the PR head merged into the base (`refs/pull/<N>/merge`), not
    /// the head SHA — so repo-file reads must follow the PR number, not the SHA.
    #[test]
    fn content_ref_is_the_merge_ref_for_a_pull_request_run_with_a_known_pr() {
        let run = run("pull_request", vec![PullRequestRef { number: 42 }]);
        assert_eq!(run.content_ref(), "refs/pull/42/merge");
    }

    /// `push`, `workflow_dispatch`, `schedule`, … have no separate merge tree —
    /// the head SHA already is what ran, so it stays the content ref too.
    #[test]
    fn content_ref_is_head_sha_for_a_non_pull_request_run() {
        let run = run("push", vec![]);
        assert_eq!(run.content_ref(), "deadbeef");
    }

    /// A fork PR carries an empty `pull_requests` array (GitHub doesn't resolve
    /// cross-repo PR refs into this payload), so there is no PR number to build a
    /// merge ref from — fall back to the head SHA rather than guessing.
    #[test]
    fn content_ref_is_head_sha_for_a_pull_request_run_with_no_pr_number() {
        let run = run("pull_request", vec![]);
        assert_eq!(run.content_ref(), "deadbeef");
    }

    /// The waste check reads only a `pull_request` run with a PR number
    /// (ADR-0063 §1): the same runs that get a merge ref.
    #[test]
    fn only_a_pull_request_run_with_a_pr_number_has_a_pull_number() {
        let pr = vec![PullRequestRef { number: 7 }];
        assert_eq!(run("pull_request", pr.clone()).pull_number(), Some(7));
        assert_eq!(run("push", pr.clone()).pull_number(), None);
        assert_eq!(run("merge_group", pr).pull_number(), None);
        assert_eq!(run("pull_request", vec![]).pull_number(), None);
    }

    #[test]
    fn a_pull_reads_its_state_and_mergeable() {
        let pull: Pull = serde_json::from_str(
            r#"{"number":7,"head":{"sha":"aaaa"},"state":"open","mergeable":null}"#,
        )
        .unwrap();
        assert_eq!(pull.state.as_deref(), Some("open"));
        assert_eq!(pull.mergeable, None);
        let pull: Pull =
            serde_json::from_str(r#"{"number":7,"head":{"sha":"aaaa"},"mergeable":false}"#)
                .unwrap();
        assert_eq!(pull.mergeable, Some(false));
    }

    /// [`get_file_at_ref`] carries `at` through `RequestBuilder::query` rather
    /// than string-interpolating it into the URL, specifically so a PR merge ref
    /// (`refs/pull/123/merge`, which contains `/`) survives intact. Build the
    /// exact same request the real call makes and decode its query back out —
    /// proving the slashes round-trip rather than getting mangled or double
    /// encoded.
    #[test]
    fn contents_request_query_preserves_merge_ref_slashes() {
        let client = reqwest::Client::new();
        let base_url = contents_url("https://api.github.com", "owner/repo", "package-lock.json");
        let req = client
            .request(reqwest::Method::GET, &base_url)
            .query(&[("ref", "refs/pull/123/merge")])
            .build()
            .expect("a well-formed request");
        let pairs: HashMap<String, String> = req.url().query_pairs().into_owned().collect();
        assert_eq!(
            pairs.get("ref").map(String::as_str),
            Some("refs/pull/123/merge"),
            "the ref must decode back to exactly what was passed in"
        );
    }

    /// The per-run jobs endpoint keys on the ever-unique `run_id`, so an unbounded
    /// map would grow one permanent entry per workflow run. The cap must hold no
    /// matter how many distinct keys are stored.
    #[test]
    fn etag_cache_stays_bounded() {
        const CAP: usize = 3;
        let mut cache = EtagCache::with_capacity(CAP);
        for i in 0..(CAP + 5) {
            cache.store(
                format!("repos/o/r/actions/runs/{i}/jobs"),
                format!("etag{i}"),
                format!("body{i}"),
            );
        }
        assert!(
            cache.len() <= CAP,
            "cache must stay bounded (was {})",
            cache.len()
        );
    }

    /// The eviction victim must be the least-recently-used key, so a still-active
    /// run (looked up every cycle) survives while a completed run falls off the
    /// tail. Touch the oldest key, then overflow by one and check the untouched
    /// LRU key — not the freshly-touched one — is the entry that got dropped.
    #[test]
    fn etag_cache_evicts_least_recently_used() {
        const CAP: usize = 3;
        let mut cache = EtagCache::with_capacity(CAP);
        for i in 0..CAP {
            cache.store(
                format!("runs/{i}/jobs"),
                format!("etag{i}"),
                format!("body{i}"),
            );
        }
        // Touch the first-stored (oldest) key so it becomes most-recently-used.
        assert!(cache.lookup("runs/0/jobs").is_some());
        // A new distinct key at capacity forces exactly one eviction.
        cache.store(
            "runs/3/jobs".to_string(),
            "etag3".to_string(),
            "body3".to_string(),
        );

        assert!(
            cache.lookup("runs/0/jobs").is_some(),
            "the recently-used key must survive eviction"
        );
        assert!(
            cache.lookup("runs/1/jobs").is_none(),
            "the least-recently-used key must be the one evicted"
        );
        assert!(cache.len() <= CAP);
    }

    /// A stored entry round-trips through `lookup` with its etag and body intact
    /// (this is what `get` replays on a 304), and an absent key returns `None`.
    #[test]
    fn etag_cache_lookup_round_trips() {
        let mut cache = EtagCache::with_capacity(4);
        cache.store(
            "runs/42/jobs".to_string(),
            "W/\"abc123\"".to_string(),
            "{\"jobs\":[]}".to_string(),
        );
        let entry = cache
            .lookup("runs/42/jobs")
            .expect("stored entry is present");
        assert_eq!(entry.etag, "W/\"abc123\"");
        assert_eq!(entry.body, "{\"jobs\":[]}");
        assert!(cache.lookup("runs/99/jobs").is_none());
    }

    /// A runner's `labels` array of `{id,name,type}` objects deserializes to the
    /// bare names (what demand-based provisioning matches on), and an absent array
    /// yields `[]` rather than failing.
    #[test]
    fn runner_labels_deserialize_to_names() {
        let with_labels = r#"{
            "id": 7,
            "name": "faber-42-1700000000000",
            "status": "offline",
            "busy": false,
            "labels": [
                {"id": 1, "name": "self-hosted", "type": "read-only"},
                {"id": 5, "name": "docker", "type": "custom"}
            ]
        }"#;
        let r: Runner = serde_json::from_str(with_labels).unwrap();
        assert_eq!(
            r.labels,
            vec!["self-hosted".to_string(), "docker".to_string()]
        );
        assert_eq!(r.status, "offline");

        // No `labels` key at all → empty, not an error.
        let bare = r#"{"id": 8, "name": "x", "status": "online", "busy": true}"#;
        let r: Runner = serde_json::from_str(bare).unwrap();
        assert!(r.labels.is_empty());
        assert!(r.busy);
    }

    /// A call that never gets a response must end in a *timeout*, not a hang —
    /// the hang is what strands an admitted claim's memory reservation (see
    /// [`REQUEST_TIMEOUT`]). Point a real [`Github`] at a local listener that
    /// accepts the connection and then stays silent, so the client's own deadline
    /// is the only way out.
    ///
    /// The clock is paused, which buys two things. Proving the deadline fires
    /// costs no wall-clock time; and the outer guard below is the *only* timer
    /// left if the bounds are ever dropped from [`http_client`], so that
    /// regression surfaces as an immediate, legible failure rather than a test
    /// run that hangs until CI kills it. The guard is deliberately far wider than
    /// [`REQUEST_TIMEOUT`] so it can never pre-empt the deadline under test.
    ///
    /// Which of the two bounds wins is deliberately not asserted: paused time can
    /// advance past the loopback connect, so [`CONNECT_TIMEOUT`] may trip first.
    /// Either one is the property under test — the call terminates on its own.
    #[tokio::test(start_paused = true)]
    async fn a_request_that_is_never_answered_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept, then hold every connection open without ever writing a
        // response. Holding the sockets matters: dropping one would close the
        // connection and hand the client a transport error instead of a stall.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });

        // Built through the real constructor, so this exercises the shipping
        // client configuration rather than a hand-rolled copy of it.
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");

        let guard = REQUEST_TIMEOUT * 4;
        match tokio::time::timeout(guard, gh.current_login()).await {
            Ok(Err(GithubError::Http(e))) => {
                assert!(e.is_timeout(), "expected a timeout, got {e}")
            }
            Ok(other) => panic!("expected a timeout error, got {other:?}"),
            Err(_) => panic!(
                "the call was still running after {guard:?} — the GitHub client has no \
                 request timeout, so a stalled call would hang the daemon forever"
            ),
        }
    }

    /// A transport failure must log *what actually went wrong*, not just that a
    /// request was sent. `reqwest::Error`'s `Display` renders one layer and stops,
    /// so a timeout, a DNS failure and a refused connection all print the same
    /// bytes — which is how faber accumulated a pile of unattributable transport
    /// errors before [`with_causes`] existed. Assert the wrapped error reaches the
    /// bottom of its own cause chain.
    ///
    /// A refused connection is the vehicle because it is immediate and needs no
    /// clock control: learn a free port from a socket that never listens, so the
    /// connect is *refused* rather than merely unanswered. That yields a real,
    /// multi-layer `reqwest::Error` — not a hand-built stand-in that could drift
    /// from what reqwest actually produces.
    ///
    /// Never a listener that is then dropped. While another test in this binary
    /// spawns a process, the child holds a copy of every descriptor until it
    /// execs (issue #47's mechanism), so a dropped listener could still accept
    /// the connect and then reset it: "connection reset by peer", not refused,
    /// in about 1% of full parallel runs. A socket that never listened cannot
    /// accept, whoever holds a copy, so the connect is refused every time.
    #[tokio::test]
    async fn a_transport_error_renders_its_underlying_cause() {
        let addr = {
            let probe = tokio::net::TcpSocket::new_v4().unwrap();
            probe.bind("127.0.0.1:0".parse().unwrap()).unwrap();
            probe.local_addr().unwrap()
        };

        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");

        let err = match gh.current_login().await {
            Err(GithubError::Http(e)) => e,
            other => panic!("expected a transport error, got {other:?}"),
        };

        // The deepest layer — the one naming the actual fault, and the one
        // reqwest's own Display never reaches.
        let root = {
            let mut e: &dyn std::error::Error = &err;
            while let Some(next) = e.source() {
                e = next;
            }
            e.to_string()
        };

        // Guard against a vacuous assertion: if reqwest ever starts rendering its
        // own root cause, the test below would pass without proving anything.
        assert!(
            !err.to_string().contains(&root),
            "reqwest now renders its root cause ({root}) — this test no longer \
             proves that faber does"
        );

        let rendered = GithubError::Http(err).to_string();
        assert!(
            rendered.contains(&root),
            "the log line must name the underlying cause ({root}), got: {rendered}"
        );
        assert!(
            rendered.contains("[connect]"),
            "a refused connection must be classified as `connect`, got: {rendered}"
        );
    }

    /// Read one HTTP/1.1 request off `sock`: the head, then exactly the
    /// `Content-Length` bytes of body. Draining the body matters for the POST
    /// case — hanging up while the client still has bytes to write would surface
    /// as a broken pipe rather than the unanswered-request failure under test.
    async fn read_request(sock: &mut tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                let body_len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() >= end + 4 + body_len {
                    return;
                }
            }
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    }

    /// A local stand-in for api.github.com that reproduces the idle keep-alive
    /// race on demand: it reads the first `hang_up` requests *in full* and then
    /// drops the socket without answering, and serves `body` as a normal `200` on
    /// every connection after that.
    ///
    /// Reading before hanging up is what makes this the failure under test rather
    /// than some other one: hyper only reports "connection closed before message
    /// completed" when EOF lands on a connection it considers busy, which is
    /// exactly the state a pooled connection is in the instant after a request has
    /// been written onto it.
    ///
    /// Returns the address to point a [`Github`] at plus the accepted-connection
    /// counter. That counter is the load-bearing part: whether a request was
    /// re-sent is not observable from the client's return value, only from how
    /// many times the server heard about it.
    async fn spawn_flaky_api(
        hang_up: usize,
        body: &'static str,
    ) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let nth = counter.fetch_add(1, Ordering::SeqCst);
                read_request(&mut sock).await;
                if nth < hang_up {
                    // Dropped at the end of the iteration, mid-exchange: the client
                    // has written a request and now sees EOF instead of a response.
                    continue;
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (addr, attempts)
    }

    /// The pool race must not reach the caller. A GET whose connection dies before
    /// any response arrives is re-sent once, on a connection the pool opens fresh,
    /// and succeeds — so a keep-alive connection GitHub had already closed stops
    /// costing a whole `discovery failed this cycle`.
    ///
    /// The connection counter is the real assertion. A bare `Ok` would be equally
    /// consistent with the first attempt having simply worked, so proving the
    /// *retry* means proving the server was reached twice for one call.
    #[tokio::test]
    async fn an_idempotent_request_is_retried_after_a_dead_connection() {
        let (addr, attempts) = spawn_flaky_api(1, r#"{"login":"octocat"}"#).await;

        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");

        let login = gh
            .current_login()
            .await
            .expect("the retry must absorb a connection that died unanswered");

        assert_eq!(login, "octocat");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "the request must have been re-sent exactly once, on a second connection"
        );
    }

    /// `generate-jitconfig` mints a runner, and a connection that dies before the
    /// response leaves it genuinely unknown whether GitHub already minted one.
    /// Re-sending would risk two runners for a single job — one that goes on to
    /// pick up somebody else's job, one orphaned holding a memory reservation — so
    /// the failure has to reach the caller instead.
    ///
    /// Deliberately the *same* failure, from the same server, that the GET above
    /// recovers from: the only thing standing between this request and a retry is
    /// [`Idempotency`]. Asserted below, so this can never pass merely because the
    /// error happened to be classified unretryable.
    #[tokio::test]
    async fn a_non_idempotent_request_is_never_silently_retried() {
        let (addr, attempts) = spawn_flaky_api(1, r#"{"encoded_jit_config":"eyJhIjoxfQ=="}"#).await;

        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");

        let err = match gh
            .generate_jitconfig(
                "o/r",
                "faber-1-2",
                &["self-hosted".to_string()],
                "_work",
                "test",
            )
            .await
        {
            Err(GithubError::Http(e)) => e,
            Err(other) => panic!("the transport failure must reach the caller, got {other:?}"),
            Ok(_) => panic!("the transport failure must reach the caller, got a minted config"),
        };

        assert!(
            is_retryable_transport(&err),
            "expected the retryable pool race — otherwise this proves nothing about \
             the idempotency gate — got: {}",
            with_causes(&err)
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a POST that mints a runner must never be re-sent: a second attempt \
             could mint a second runner for one job"
        );
    }

    /// A mint hands back the id of the registration it created, which is what a
    /// reclaim withdraws before it may kill the runner (ADR-0017) — and a response
    /// without one still mints, because the id only ever saves a lookup.
    #[tokio::test]
    async fn a_jit_mint_returns_the_registration_id_when_github_sends_one() {
        let (addr, _) = spawn_flaky_api(
            0,
            r#"{"runner":{"id":30542,"name":"faber-1-2","os":"unknown","status":"offline","busy":false,"labels":[]},"encoded_jit_config":"eyJhIjoxfQ=="}"#,
        )
        .await;
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");
        let jit = gh
            .generate_jitconfig("o/r", "faber-1-2", &["docker".to_string()], "_work", "test")
            .await
            .expect("mints");
        assert_eq!(jit.encoded, "eyJhIjoxfQ==");
        assert_eq!(jit.runner_id, Some(30542));

        let (addr, _) = spawn_flaky_api(0, r#"{"encoded_jit_config":"eyJhIjoxfQ=="}"#).await;
        gh.base = format!("http://{addr}");
        let jit = gh
            .generate_jitconfig("o/r", "faber-1-2", &["docker".to_string()], "_work", "test")
            .await
            .expect("a response without the runner object still mints");
        assert_eq!(jit.runner_id, None);
    }

    #[test]
    fn mint_backoff_doubles_to_a_cap_and_a_success_clears_it() {
        let mut b = MintBackoff::default();
        assert_eq!(b.turn(0), Ok(()), "no failures, no wait");
        b.failed(0);
        assert_eq!(b.turn(1_000), Err(9_000));
        b.failed(0);
        assert_eq!(b.turn(0), Err(20_000));
        b.failed(0);
        assert_eq!(b.turn(0), Err(40_000));
        for _ in 0..20 {
            b.failed(0);
        }
        assert_eq!(
            b.turn(0),
            Err(MINT_BACKOFF_MAX_MS),
            "bounded however long it lasts"
        );
        b.succeeded();
        assert_eq!(b.turn(0), Ok(()));
        assert_eq!(b.failures(), 0);
    }

    /// The claims parked behind a backoff must not all mint the moment it ends:
    /// that is the burst #89 is about, deferred by one delay.
    #[test]
    fn once_a_backoff_ends_one_mint_goes_and_the_next_waits_a_whole_delay() {
        let mut b = MintBackoff::default();
        b.failed(0);
        assert_eq!(b.wait_ms(4_000), Some(6_000));
        assert_eq!(b.wait_ms(4_000), Some(6_000), "asking takes no turn");
        assert_eq!(b.wait_ms(10_000), None);
        assert_eq!(b.turn(10_000), Ok(()), "the wait is over");
        assert_eq!(b.wait_ms(10_000), Some(10_000));
        assert_eq!(b.turn(10_000), Err(10_000), "the next claim waits again");
        assert_eq!(b.turn(20_000), Ok(()));
    }

    /// Only a 5xx starts the backoff: a 4xx is about this one request, and a
    /// success ends it.
    #[tokio::test]
    async fn a_5xx_from_generate_jitconfig_backs_the_next_mint_off() {
        let minted = r#"{"encoded_jit_config":"eyJhIjoxfQ=="}"#;
        let (addr, _) = spawn_scripted_api(vec![
            (422, r#"{"message":"no"}"#),
            (500, r#"{"message":"boom"}"#),
            (502, r#"{"message":"boom"}"#),
            (201, minted),
        ])
        .await;
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");
        let labels = ["docker".to_string()];
        let now = crate::audit::now_ms();
        let mint = || gh.generate_jitconfig("o/r", "faber-1-2", &labels, "_work", "test");

        assert!(mint().await.is_err());
        assert_eq!(gh.mint_failures(), 0, "a 4xx is not GitHub failing");
        assert_eq!(gh.mint_turn(now), Ok(()));

        assert!(mint().await.is_err());
        assert!(mint().await.is_err());
        assert_eq!(gh.mint_failures(), 2);
        let wait = gh
            .mint_turn(crate::audit::now_ms())
            .expect_err("backing off");
        assert!(wait > 10_000 && wait <= 20_000, "{wait}");

        mint().await.expect("mints");
        assert_eq!(gh.mint_failures(), 0);
        assert_eq!(gh.mint_turn(now), Ok(()));
    }

    /// The lookup a reclaim falls back to must match the name exactly: GitHub's
    /// filter is the one faber asks for, but a peer's runner minted for the same
    /// job differs only in its trailing timestamp and must never be mistaken for
    /// this one.
    #[tokio::test]
    async fn runner_named_matches_the_name_exactly() {
        let (addr, _) = spawn_flaky_api(
            0,
            r#"{"total_count":2,"runners":[{"id":1,"name":"faber-7-100","status":"online","busy":false,"labels":[]},{"id":2,"name":"faber-7-1000","status":"online","busy":true,"labels":[]}]}"#,
        )
        .await;
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");
        let found = gh
            .runner_named("o/r", "faber-7-1000")
            .await
            .expect("lists")
            .expect("present");
        assert_eq!((found.id, found.busy), (2, true));
        assert!(gh
            .runner_named("o/r", "faber-7-10")
            .await
            .expect("lists")
            .is_none());
    }

    /// A local stand-in for api.github.com that answers each request with the next
    /// scripted `(status, body)` and records each request line (`METHOD /path`),
    /// so a test can see both what faber asked and in what order.
    async fn spawn_scripted_api(
        script: Vec<(u16, &'static str)>,
    ) -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut script = script.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let line = head.lines().next().unwrap_or_default();
                log.lock()
                    .unwrap()
                    .push(line.rsplit_once(' ').map_or(line, |(l, _)| l).to_string());
                let Some((status, body)) = script.next() else {
                    return;
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (addr, seen)
    }

    /// A topic-search body with `total` and these repos.
    fn search_body(total: usize, incomplete: bool, repos: std::ops::Range<usize>) -> &'static str {
        let items: Vec<String> = repos
            .map(|i| format!(r#"{{"full_name":"owner/demo{i}"}}"#))
            .collect();
        let body = format!(
            r#"{{"total_count":{total},"incomplete_results":{incomplete},"items":[{}]}}"#,
            items.join(",")
        );
        Box::leak(body.into_boxed_str())
    }

    /// A jobs page of `n` jobs with ids from `from`.
    fn jobs_body(from: u64, n: u64) -> &'static str {
        let jobs: Vec<String> = (from..from + n)
            .map(|i| format!(r#"{{"id":{i},"name":"j{i}","status":"queued"}}"#))
            .collect();
        Box::leak(format!(r#"{{"total_count":0,"jobs":[{}]}}"#, jobs.join(",")).into_boxed_str())
    }

    /// #66: a job past the 100th in its run is found. The listing reads on while
    /// pages come back full, and page 1 is the request it always was.
    #[tokio::test]
    async fn a_runs_jobs_are_read_past_the_first_page() {
        let (addr, seen) =
            spawn_scripted_api(vec![(200, jobs_body(1, 100)), (200, jobs_body(101, 3))]).await;
        let gh = Github::at(format!("http://{addr}"));
        let jobs = gh.run_jobs("owner/demo", 7).await.expect("jobs");
        assert_eq!(jobs.len(), 103);
        assert_eq!(jobs.last().map(|j| j.id), Some(103));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "GET /repos/owner/demo/actions/runs/7/jobs?per_page=100",
                "GET /repos/owner/demo/actions/runs/7/jobs?per_page=100&page=2",
            ]
        );

        // A page that is not full is the last: one request.
        let (addr, seen) = spawn_scripted_api(vec![(200, jobs_body(1, 4))]).await;
        let gh = Github::at(format!("http://{addr}"));
        assert_eq!(gh.run_jobs("owner/demo", 7).await.expect("jobs").len(), 4);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// A whole answer comes back whole, one page or several.
    #[tokio::test]
    async fn the_topic_search_returns_a_complete_answer() {
        let (addr, seen) = spawn_scripted_api(vec![(200, search_body(2, false, 0..2))]).await;
        let gh = Github::at(format!("http://{addr}"));
        assert_eq!(
            gh.repos_with_topic("owner", "faber").await.expect("whole"),
            vec!["owner/demo0", "owner/demo1"]
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["GET /search/repositories?q=user:owner+topic:faber&per_page=100&page=1"]
        );

        let (addr, seen) = spawn_scripted_api(vec![
            (200, search_body(101, false, 0..100)),
            (200, search_body(101, false, 100..101)),
        ])
        .await;
        let gh = Github::at(format!("http://{addr}"));
        let repos = gh.repos_with_topic("owner", "faber").await.expect("whole");
        assert_eq!(repos.len(), 101);
        assert_eq!(seen.lock().unwrap().len(), 2, "the second page is read");

        // No repo carries the topic: a consistent empty answer is an answer.
        let (addr, _) = spawn_scripted_api(vec![(200, search_body(0, false, 0..0))]).await;
        let gh = Github::at(format!("http://{addr}"));
        assert!(gh
            .repos_with_topic("owner", "faber")
            .await
            .unwrap()
            .is_empty());
    }

    /// A short search is an error, never an `Ok` short list: every caller reads
    /// a missing repo as one that left the topic, and the git-mirror janitor
    /// deletes on that reading (the October 2026 incident).
    #[tokio::test]
    async fn the_topic_search_refuses_a_partial_answer() {
        for (why, script) in [
            (
                "marked incomplete",
                vec![(200, search_body(12, true, 0..1))],
            ),
            (
                "fewer items than total_count",
                vec![(200, search_body(12, false, 0..1))],
            ),
            (
                "more items than total_count",
                vec![(200, search_body(1, false, 0..2))],
            ),
            ("an empty object", vec![(200, "{}")]),
            (
                "items without a count",
                vec![(200, r#"{"items":[{"full_name":"owner/demo"}]}"#)],
            ),
            ("an empty body", vec![(200, "")]),
            (
                "a second page that is short",
                vec![
                    (200, search_body(150, false, 0..100)),
                    (200, search_body(150, false, 100..120)),
                ],
            ),
            (
                "a count that moves between pages",
                vec![
                    (200, search_body(150, false, 0..100)),
                    (200, search_body(120, false, 100..120)),
                ],
            ),
        ] {
            let (addr, _) = spawn_scripted_api(script).await;
            let gh = Github::at(format!("http://{addr}"));
            assert!(
                gh.repos_with_topic("owner", "faber").await.is_err(),
                "{why} must be refused"
            );
        }
    }

    /// ADR-0017's atomic half. GitHub refusing to delete a runner is the one
    /// answer about "is a job assigned to it" that is not read from a listing
    /// fetched earlier, so a `422` must come back as `JobAssigned` — not as an
    /// error a caller might treat as "try the kill anyway".
    #[tokio::test]
    async fn withdrawing_a_runner_reads_githubs_refusal_as_a_job_assigned() {
        let busy = r#"{"message":"Bad request - Runner faber-7-1000 is currently running a job and cannot be deleted.","status":"422"}"#;
        let (addr, seen) = spawn_scripted_api(vec![(422, busy)]).await;
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");
        assert_eq!(
            gh.withdraw_runner("o/r", "faber-7-1000", Some(30542), "test")
                .await
                .expect("a refusal is an answer, not an error"),
            Deregistration::JobAssigned
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["DELETE /repos/o/r/actions/runners/30542"],
            "with the id from the mint, no lookup is needed"
        );

        // Without an id (an adopted runner) it is found by exact name first; a
        // `204` and a `404` both leave nothing GitHub could assign a job to.
        let listed = r#"{"total_count":1,"runners":[{"id":30543,"name":"faber-7-1000","status":"online","busy":false,"labels":[]}]}"#;
        for gone in [204, 404] {
            let (addr, seen) = spawn_scripted_api(vec![(200, listed), (gone, "")]).await;
            gh.base = format!("http://{addr}");
            assert_eq!(
                gh.withdraw_runner("o/r", "faber-7-1000", None, "test")
                    .await
                    .expect("withdraws"),
                Deregistration::Removed,
                "{gone} must read as removed"
            );
            assert_eq!(
                *seen.lock().unwrap(),
                vec![
                    "GET /repos/o/r/actions/runners?name=faber-7-1000",
                    "DELETE /repos/o/r/actions/runners/30543",
                ]
            );
        }

        // Nothing registered under the name: nothing to delete, nothing to assign.
        let (addr, seen) =
            spawn_scripted_api(vec![(200, r#"{"total_count":0,"runners":[]}"#)]).await;
        gh.base = format!("http://{addr}");
        assert_eq!(
            gh.withdraw_runner("o/r", "faber-7-1000", None, "test")
                .await
                .expect("withdraws"),
            Deregistration::Removed
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "no DELETE without a registration"
        );

        // Any other failure is not an answer about the runner at all.
        let (addr, _) = spawn_scripted_api(vec![(500, r#"{"message":"boom"}"#)]).await;
        gh.base = format!("http://{addr}");
        assert!(gh
            .withdraw_runner("o/r", "faber-7-1000", Some(30542), "test")
            .await
            .is_err());
    }

    /// #105: on the path from an adopted container, a repo and a runner name may
    /// have come off a label. One that would steer the URL, or name a runner faber
    /// does not mint, is refused before any request is sent.
    #[tokio::test]
    async fn a_runner_target_not_in_fabers_own_form_is_never_sent() {
        let (addr, seen) = spawn_scripted_api(vec![(204, ""), (204, "")]).await;
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = format!("http://{addr}");
        for (repo, name) in [
            ("x/../../orgs/victim", "faber-7-1000"),
            ("o/r/actions/runners/1", "faber-7-1000"),
            ("o/r", "faber-7-1000&name=other"),
            ("o/r", "faber-7-1000#"),
            ("o/r", "a-peers-runner"),
            ("o/r", "faber-runner-7-1000"),
        ] {
            for id in [None, Some(30542)] {
                assert!(
                    matches!(
                        gh.withdraw_runner(repo, name, id, "test").await,
                        Err(GithubError::Refused(_))
                    ),
                    "{repo} {name} {id:?}"
                );
            }
        }
        assert!(matches!(
            gh.delete_runner("x/../../orgs/victim", 1, "test").await,
            Err(GithubError::Refused(_))
        ));
        assert!(seen.lock().unwrap().is_empty(), "nothing may reach GitHub");
    }

    /// `latest_runner_version` has only ever read `tag_name`; broadening
    /// `Release` to carry `draft`/`prerelease`/`assets` for the autoupdate
    /// checks (ADR-0006) must not break it. A minimal payload (the shape that
    /// endpoint has always returned faber's own requests, since it never
    /// asked for anything else) must still parse, with the new fields
    /// defaulting rather than erroring; a full payload must populate all of
    /// them, including nested assets.
    #[test]
    fn release_deserializes_draft_and_prerelease_defaults_and_assets() {
        let minimal = r#"{"tag_name": "v1.2.3"}"#;
        let r: Release = serde_json::from_str(minimal).unwrap();
        assert_eq!(r.tag_name, "v1.2.3");
        assert!(!r.draft);
        assert!(!r.prerelease);
        assert!(r.assets.is_empty());

        let full = r#"{
            "tag_name": "v0.2.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"id": 1, "name": "faber-aarch64-apple-darwin", "size": 123},
                {"id": 2, "name": "SHA256SUMS", "size": 45}
            ]
        }"#;
        let r: Release = serde_json::from_str(full).unwrap();
        assert_eq!(r.assets.len(), 2);
        assert_eq!(r.assets[0].id, 1);
        assert_eq!(r.assets[0].name, "faber-aarch64-apple-darwin");
        assert_eq!(r.assets[1].name, "SHA256SUMS");
    }

    /// `download_release_asset` must not carry faber's GitHub bearer token
    /// onto the S3-style URL a release-asset download redirects to — S3
    /// treats an unrecognized `Authorization` header as a hard failure, and
    /// leaking a live token to a third-party host would be a real credential
    /// leak regardless. Two local servers stand in for GitHub and the
    /// redirect target; the assertion that matters is on the *second* one,
    /// which records whether it ever saw an `Authorization` header at all.
    #[tokio::test]
    async fn download_release_asset_does_not_forward_the_token_past_a_redirect() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn accept_and_capture_head(
            listener: tokio::net::TcpListener,
        ) -> (String, tokio::net::TcpStream) {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                match sock.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            (String::from_utf8_lossy(&buf).to_ascii_lowercase(), sock)
        }

        let asset_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let asset_addr = asset_listener.local_addr().unwrap();
        let asset_task = tokio::spawn(async move {
            let (head, mut sock) = accept_and_capture_head(asset_listener).await;
            let body = b"asset-bytes";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(body).await.unwrap();
            let _ = sock.flush().await;
            head
        });

        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect_addr = redirect_listener.local_addr().unwrap();
        let redirect_task = tokio::spawn(async move {
            let (head, mut sock) = accept_and_capture_head(redirect_listener).await;
            let location = format!("http://{asset_addr}/presigned");
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.flush().await;
            head
        });

        let mut gh = Github::new("super-secret-token".to_string()).expect("client builds");
        gh.base = format!("http://{redirect_addr}");

        let bytes = gh
            .download_release_asset("o/r", 42)
            .await
            .expect("follows the redirect and returns the asset bytes");
        assert_eq!(bytes, b"asset-bytes");

        let redirect_head = redirect_task.await.unwrap();
        assert!(
            redirect_head.contains("authorization: bearer super-secret-token"),
            "the first hop (the real GitHub API call) must still be authenticated: {redirect_head}"
        );
        let asset_head = asset_task.await.unwrap();
        assert!(
            !asset_head.contains("authorization"),
            "the redirect target must never see faber's token: {asset_head}"
        );

        // Both hops must identify themselves. These clients are built by hand
        // rather than by `http_client`, so they inherit no default User-Agent —
        // and GitHub 403s an API request that lacks one, which is exactly how
        // v0.2.0 shipped an updater that could not download its own successor.
        assert!(
            redirect_head.contains("user-agent: faber/"),
            "the API hop must send a User-Agent or GitHub 403s it: {redirect_head}"
        );
        assert!(
            asset_head.contains("user-agent: faber/"),
            "the asset hop must send a User-Agent too: {asset_head}"
        );
    }

    // --- tag -> commit (ADR-0031 release verification) ------------------------

    /// Answers each request with the next scripted `(status, body)` and records
    /// the request lines, so a test sees exactly which endpoints were asked.
    async fn scripted_api(
        script: Vec<(u16, &'static str)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut script = script.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                let head = read_request_head(&mut sock).await;
                let line = head.lines().next().unwrap_or_default().to_string();
                log.lock().unwrap().push(line);
                let Some((status, body)) = script.next() else {
                    continue;
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, seen)
    }

    async fn read_request_head(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// The updater's client never sends a credential (ADR-0027): not a token
    /// of its own, and never the CI token.
    #[tokio::test]
    async fn an_anonymous_client_sends_no_credential() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let head = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let head = read_request_head(&mut sock).await;
            let body = "[]";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
            head.to_ascii_lowercase()
        });
        let mut gh = Github::anonymous().expect("client builds");
        gh.base = base;
        assert!(gh.list_releases("o/r").await.unwrap().is_empty());
        let head = head.await.unwrap();
        assert!(head.starts_with("get /repos/o/r/releases"), "{head}");
        assert!(!head.contains("authorization"), "{head}");
        assert!(head.contains("user-agent: faber/"), "{head}");
    }

    /// Every write lands in `audit.jsonl` with its reason and how it went, a
    /// refusal included, and never with the token. A permission refusal also
    /// turns off what needs that permission, for that repository only.
    #[tokio::test]
    async fn every_write_is_audited_and_a_refusal_is_learned() {
        let denied = r#"{"message":"Resource not accessible by personal access token"}"#;
        let (base, _) = scripted_api(vec![(201, ""), (403, denied)]).await;
        let scratch = crate::setup::fsx::tests::Scratch::new("gh-audit");
        let path = scratch.path("audit.jsonl");
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = base;
        gh.set_audit(Audit::at(path.clone()));
        gh.rerun_job("o/r", 7, "runner lost")
            .await
            .expect("re-runs");
        assert!(gh.rerun_failed_jobs("o/r", 9, "runner lost").await.is_err());
        assert!(gh.blocked("o/r", Permission::ActionsWrite));
        assert!(!gh.blocked("o/other", Permission::ActionsWrite));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("test-token"), "{text}");
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "{text}");
        for (line, action, result) in [
            (&lines[0], "rerun_job", "ok"),
            (&lines[1], "rerun_failed_jobs", "403"),
        ] {
            assert_eq!(line["kind"], "github_write");
            assert_eq!(line["repo"], "o/r");
            assert_eq!(line["action"], action);
            assert_eq!(line["reason"], "runner lost");
            assert_eq!(line["result"], result);
        }
    }

    /// A refused write is audited before anything is sent.
    #[tokio::test]
    async fn a_refused_runner_target_is_audited_as_refused() {
        let scratch = crate::setup::fsx::tests::Scratch::new("gh-audit-refused");
        let path = scratch.path("audit.jsonl");
        let gh = Github::at("http://127.0.0.1:9".to_string());
        gh.set_audit(Audit::at(path.clone()));
        assert!(gh.delete_runner("x/../y", 1, "sweep").await.is_err());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""result":"refused""#), "{text}");
    }

    /// A token GitHub says acts as another account than the served owner
    /// stops claims, and the block names both logins and the way out.
    #[test]
    fn a_token_acting_as_another_account_blocks_claims() {
        let gh = Github::new("test-token".to_string()).expect("client builds");
        assert_eq!(gh.claim_block(), None);
        assert!(gh.acts_as_other("other", "owner"));
        assert!(!gh.acts_as_other("other", "owner"), "news only once");
        let why = gh.claim_block().expect("claims are blocked");
        assert!(
            why.contains("acts as other") && why.contains("serves owner"),
            "{why}"
        );
        assert!(why.contains("sudo faber auth set"), "{why}");
    }

    /// A `401` that re-reading the store cannot fix makes the credential
    /// `invalid`, which stops claims, and the next request GitHub accepts ends
    /// it.
    #[tokio::test]
    async fn a_rejected_token_blocks_claims_until_github_accepts_it() {
        let (base, _) = scripted_api(vec![
            (401, r#"{"message":"Bad credentials"}"#),
            (200, r#"{"login":"owner"}"#),
        ])
        .await;
        let mut gh = Github::new("test-token".to_string()).expect("client builds");
        gh.base = base;
        assert_eq!(gh.claim_block(), None);
        assert!(matches!(
            gh.current_login().await,
            Err(GithubError::Status { status: 401, .. })
        ));
        assert_eq!(
            gh.claim_block().as_deref(),
            Some(crate::credential::REJECTED)
        );
        assert_eq!(gh.current_login().await.unwrap(), "owner");
        assert_eq!(gh.claim_block(), None);
    }

    /// `rate_budget_low` flips once a response reports a remaining count under the
    /// floor, and flips back when a later one reports more.
    #[tokio::test]
    async fn the_rate_budget_is_low_once_a_response_says_so() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for remaining in [RATE_BUDGET_FLOOR + 1, RATE_BUDGET_FLOOR - 1] {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"id":1,"name":"b","status":"queued"}"#;
                let resp = format!(
                    "HTTP/1.1 200 X\r\nContent-Type: application/json\r\n\
                     x-ratelimit-remaining: {remaining}\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let gh = Github::at(base);
        assert!(!gh.rate_budget_low(), "never reported is not low");
        gh.job("owner/demo", 1).await.unwrap();
        assert!(!gh.rate_budget_low(), "at the floor is not under it");
        gh.job("owner/demo", 1).await.unwrap();
        assert!(gh.rate_budget_low());
    }

    const COMMIT: &str = "8ef14b57c19a80f6f1adfc0cc37943d94e36c20f";

    #[tokio::test]
    async fn a_lightweight_tag_resolves_to_its_commit_in_one_request() {
        let (base, seen) = scripted_api(vec![(
            200,
            r#"{"ref":"refs/tags/v1.2.3","object":{"sha":"8ef14b57c19a80f6f1adfc0cc37943d94e36c20f","type":"commit"}}"#,
        )])
        .await;
        let gh = Github::at(base);
        assert_eq!(
            gh.tag_commit("owner/demo", "v1.2.3")
                .await
                .unwrap()
                .as_deref(),
            Some(COMMIT)
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["GET /repos/owner/demo/git/ref/tags/v1.2.3 HTTP/1.1".to_string()]
        );
    }

    /// An annotated tag's ref names a tag object, not a commit: it must be
    /// peeled, or the attestation would be compared against the tag object's sha.
    #[tokio::test]
    async fn an_annotated_tag_is_peeled_to_its_commit() {
        let (base, seen) = scripted_api(vec![
            (
                200,
                r#"{"ref":"refs/tags/v1.2.3","object":{"sha":"1111111111111111111111111111111111111111","type":"tag"}}"#,
            ),
            (
                200,
                r#"{"sha":"1111111111111111111111111111111111111111","tag":"v1.2.3","object":{"sha":"8ef14b57c19a80f6f1adfc0cc37943d94e36c20f","type":"commit"}}"#,
            ),
        ])
        .await;
        let gh = Github::at(base);
        assert_eq!(
            gh.tag_commit("owner/demo", "v1.2.3")
                .await
                .unwrap()
                .as_deref(),
            Some(COMMIT)
        );
        assert_eq!(
            seen.lock().unwrap()[1],
            "GET /repos/owner/demo/git/tags/1111111111111111111111111111111111111111 HTTP/1.1"
        );
    }

    /// A sha that is not a full lowercase SHA-1 is refused, whether it would be
    /// returned (compared against the attestation) or peeled (put in a URL).
    #[tokio::test]
    async fn a_malformed_object_sha_is_refused() {
        for body in [
            r#"{"object":{"sha":"8EF14B57C19A80F6F1ADFC0CC37943D94E36C20F","type":"commit"}}"#,
            r#"{"object":{"sha":"8ef14b5","type":"commit"}}"#,
            r#"{"object":{"sha":"../../../user/xxxxxxxxxxxxxxxxxxxxxxxxxxxxx","type":"tag"}}"#,
        ] {
            let (base, seen) = scripted_api(vec![(200, body)]).await;
            let gh = Github::at(base);
            let err = gh.tag_commit("owner/demo", "v1.2.3").await.unwrap_err();
            assert!(matches!(err, GithubError::Refused(_)), "{body}: {err}");
            assert_eq!(
                seen.lock().unwrap().len(),
                1,
                "no follow-up request: {body}"
            );
        }
    }

    /// The anonymous client (ADR-0034's `upgrade-cli`) sends no credential, and
    /// a `401` is an answer, never a reason to go looking for a token: one
    /// request, no `Authorization` header, no second attempt.
    #[tokio::test]
    async fn an_anonymous_client_sends_no_token_and_never_refreshes_one() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = heads.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let head = read_request_head(&mut sock).await;
                log.lock().unwrap().push(head.to_ascii_lowercase());
                let body = r#"{"message":"Bad credentials"}"#;
                let resp = format!(
                    "HTTP/1.1 401 X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        let gh = Github {
            base,
            ..Github::anonymous().expect("client builds")
        };
        let err = gh.release_by_tag("owner/demo", "v1.2.3").await.unwrap_err();
        assert!(
            matches!(err, GithubError::Status { status: 401, .. }),
            "{err}"
        );
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 1, "no retry with a refreshed token: {heads:?}");
        assert!(!heads[0].contains("authorization"), "{}", heads[0]);
        assert!(heads[0].contains("user-agent: faber/"), "{}", heads[0]);
    }

    #[tokio::test]
    async fn a_missing_tag_is_none() {
        let (base, _) = scripted_api(vec![(404, r#"{"message":"Not Found"}"#)]).await;
        let gh = Github::at(base);
        assert_eq!(gh.tag_commit("owner/demo", "v9.9.9").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_tag_of_something_other_than_a_commit_is_an_error() {
        let (base, _) = scripted_api(vec![(
            200,
            r#"{"ref":"refs/tags/v1.2.3","object":{"sha":"2222222222222222222222222222222222222222","type":"tree"}}"#,
        )])
        .await;
        let gh = Github::at(base);
        let err = gh.tag_commit("owner/demo", "v1.2.3").await.unwrap_err();
        assert!(err.to_string().contains("tree"), "{err}");
    }

    // --- the gate's reads (ADR-0059 §3) -----------------------------------------

    mod gate_reads {
        use super::*;

        type Seen = Arc<std::sync::Mutex<Vec<(String, String)>>>;

        /// A stand-in for api.github.com that answers `script` in order and
        /// records each request's line and body, the GraphQL variables included.
        async fn recorded_api(script: Vec<(u16, String)>) -> (String, Seen) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let seen: Seen = Arc::default();
            let log = seen.clone();
            tokio::spawn(async move {
                let mut script = script.into_iter();
                while let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let request = loop {
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                            let len = head
                                .to_ascii_lowercase()
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.len() >= end + 4 + len {
                                let body = &buf[end + 4..end + 4 + len];
                                break (
                                    head.lines().next().unwrap_or_default().to_string(),
                                    String::from_utf8_lossy(body).into_owned(),
                                );
                            }
                        }
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => break Default::default(),
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    log.lock().unwrap().push(request);
                    let Some((status, body)) = script.next() else {
                        continue;
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                }
            });
            (base, seen)
        }

        fn sha(n: u64) -> String {
            format!("{n:040x}")
        }

        /// A page of `count` pull requests numbered from `from`.
        fn pulls_page(count: u64, from: u64) -> String {
            let items: Vec<String> = (from..from + count)
                .map(|n| {
                    format!(
                        r#"{{"number":{n},"user":{{"login":"author-{n}"}},"head":{{"sha":"{}"}}}}"#,
                        sha(n)
                    )
                })
                .collect();
            format!("[{}]", items.join(","))
        }

        /// One page of a merge queue's GraphQL answer.
        fn queue_page(nodes: &str, next: Option<&str>) -> String {
            let info = match next {
                Some(c) => format!(r#"{{"hasNextPage":true,"endCursor":"{c}"}}"#),
                None => r#"{"hasNextPage":false,"endCursor":null}"#.to_string(),
            };
            format!(
                r#"{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"pageInfo":{info},"nodes":[{nodes}]}}}}}}}}}}"#
            )
        }

        fn queue_node(position: u64, head: &str, number: u64, kind: &str, login: &str) -> String {
            format!(
                r#"{{"position":{position},"headCommit":{{"oid":"{head}"}},"pullRequest":{{"number":{number},"author":{{"__typename":"{kind}","login":"{login}"}}}}}}"#
            )
        }

        #[tokio::test]
        async fn a_pull_request_reads_its_author_and_head() {
            let body = format!(
                r#"{{"number":12,"user":{{"login":"dependabot[bot]"}},"head":{{"sha":"{}"}}}}"#,
                sha(7)
            );
            let (base, seen) = recorded_api(vec![(200, body)]).await;
            let pull = Github::at(base).pull("owner/demo", 12).await.unwrap();
            assert_eq!(pull.number, 12);
            assert_eq!(pull.user.unwrap().login, "dependabot[bot]");
            assert_eq!(pull.head.sha, sha(7));
            assert_eq!(
                seen.lock().unwrap()[0].0,
                "GET /repos/owner/demo/pulls/12 HTTP/1.1"
            );
        }

        #[tokio::test]
        async fn the_pull_requests_from_a_head_are_read_to_the_last_page() {
            let (base, seen) =
                recorded_api(vec![(200, pulls_page(100, 1)), (200, pulls_page(3, 101))]).await;
            let pulls = Github::at(base)
                .pulls_from("owner/demo", "a-stranger", "feat/x#1")
                .await
                .unwrap();
            assert_eq!(pulls.len(), 103);
            assert_eq!(pulls[102].user.as_ref().unwrap().login, "author-103");
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            for (n, (line, _)) in seen.iter().enumerate() {
                assert!(
                    line.contains("state=all&head=a-stranger%3Afeat/x%231&per_page=100"),
                    "{line}"
                );
                assert!(line.contains(&format!("&page={} ", n + 1)), "{line}");
            }
        }

        #[tokio::test]
        async fn a_head_listing_that_does_not_end_is_an_error_not_a_partial_answer() {
            let script = (0..PULLS_FROM_PAGES as u64)
                .map(|p| (200, pulls_page(100, p * 100 + 1)))
                .collect();
            let (base, seen) = recorded_api(script).await;
            let err = Github::at(base)
                .pulls_from("owner/demo", "a-stranger", "main")
                .await
                .unwrap_err();
            assert!(matches!(err, GithubError::Unexpected(_)), "{err}");
            assert_eq!(seen.lock().unwrap().len(), PULLS_FROM_PAGES as usize);
        }

        #[tokio::test]
        async fn a_collaborator_check_reads_204_and_404_and_keeps_each_answer() {
            let (base, seen) = recorded_api(vec![
                (204, String::new()),
                (404, r#"{"message":"Not Found"}"#.into()),
            ])
            .await;
            let gh = Github::at(base);
            assert!(gh.is_collaborator("owner/demo", "alice").await.unwrap());
            assert!(!gh
                .is_collaborator("owner/demo", "a-stranger")
                .await
                .unwrap());
            // Kept: asked again, in any case, GitHub hears nothing more.
            assert!(gh.is_collaborator("Owner/Demo", "ALICE").await.unwrap());
            assert!(!gh
                .is_collaborator("owner/demo", "a-stranger")
                .await
                .unwrap());
            let seen = seen.lock().unwrap();
            assert_eq!(
                seen.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
                [
                    "GET /repos/owner/demo/collaborators/alice HTTP/1.1",
                    "GET /repos/owner/demo/collaborators/a-stranger HTTP/1.1",
                ]
            );
        }

        #[tokio::test]
        async fn a_collaborator_check_github_does_not_answer_is_an_error_and_not_kept() {
            let (base, seen) = recorded_api(vec![
                (502, r#"{"message":"Bad Gateway"}"#.into()),
                (204, String::new()),
            ])
            .await;
            let gh = Github::at(base);
            let err = gh.is_collaborator("owner/demo", "alice").await.unwrap_err();
            assert!(
                matches!(err, GithubError::Status { status: 502, .. }),
                "{err}"
            );
            assert!(gh.is_collaborator("owner/demo", "alice").await.unwrap());
            assert_eq!(seen.lock().unwrap().len(), 2);
        }

        #[tokio::test]
        async fn a_gate_read_refuses_a_target_github_could_not_have_named() {
            let (base, seen) = recorded_api(vec![]).await;
            let gh = Github::at(base);
            for login in ["dependabot[bot]", "a/../b", "-dash", ""] {
                let err = gh.is_collaborator("owner/demo", login).await.unwrap_err();
                assert!(matches!(err, GithubError::Refused(_)), "{login}: {err}");
            }
            let err = gh
                .pulls_from("owner/demo", "x&y", "main")
                .await
                .unwrap_err();
            assert!(matches!(err, GithubError::Refused(_)), "{err}");
            let err = gh.pull("owner/../demo", 1).await.unwrap_err();
            assert!(matches!(err, GithubError::Refused(_)), "{err}");
            let err = gh
                .branch_contains("owner/demo", "main", "HEAD")
                .await
                .unwrap_err();
            assert!(matches!(err, GithubError::Refused(_)), "{err}");
            assert!(seen.lock().unwrap().is_empty(), "nothing was sent");
        }

        #[tokio::test]
        async fn a_merge_queue_is_read_to_the_last_page_with_apps_in_rest_form() {
            let first = queue_page(&queue_node(1, &sha(1), 5, "User", "alice"), Some("c1"));
            let second = queue_page(&queue_node(2, &sha(2), 6, "Bot", "dependabot"), None);
            let (base, seen) = recorded_api(vec![(200, first), (200, second)]).await;
            let entries = Github::at(base)
                .merge_queue("owner/demo", "release/1.x")
                .await
                .unwrap();
            assert_eq!(
                entries,
                vec![
                    QueueEntry {
                        position: 1,
                        head_sha: Some(sha(1)),
                        pull: Some(5),
                        author: Some("alice".into()),
                    },
                    QueueEntry {
                        position: 2,
                        head_sha: Some(sha(2)),
                        pull: Some(6),
                        author: Some("dependabot[bot]".into()),
                    },
                ]
            );
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            let vars = |n: usize| -> serde_json::Value {
                serde_json::from_str::<serde_json::Value>(&seen[n].1).unwrap()["variables"].clone()
            };
            assert_eq!(seen[0].0, "POST /graphql HTTP/1.1");
            assert_eq!(vars(0)["owner"], "owner");
            assert_eq!(vars(0)["name"], "demo");
            assert_eq!(vars(0)["branch"], "release/1.x");
            assert_eq!(vars(0)["after"], serde_json::Value::Null);
            assert_eq!(vars(1)["after"], "c1");
        }

        #[tokio::test]
        async fn a_branch_with_no_merge_queue_has_no_entries() {
            let body = r#"{"data":{"repository":{"mergeQueue":null}}}"#.to_string();
            let (base, _) = recorded_api(vec![(200, body)]).await;
            let entries = Github::at(base).merge_queue("owner/demo", "main").await;
            assert_eq!(entries.unwrap(), vec![]);
        }

        #[tokio::test]
        async fn a_merge_queue_the_token_may_not_read_is_a_permission_refusal() {
            let body = r#"{"data":{"repository":{"mergeQueue":null}},"errors":[{"type":"FORBIDDEN","path":["repository","mergeQueue"],"message":"Resource not accessible by personal access token"}]}"#;
            let (base, _) = recorded_api(vec![(200, body.to_string())]).await;
            let gh = Github::at(base);
            let err = gh.merge_queue("owner/demo", "main").await.unwrap_err();
            let GithubError::Status { status, body, .. } = &err else {
                panic!("{err}");
            };
            assert!(is_permission_refusal(*status, false, body), "{err}");
            // Learned like any other refusal, so the token's health names it.
            assert!(gh.blocked("owner/demo", Permission::MergeQueuesRead));
        }

        #[tokio::test]
        async fn a_merge_queue_entry_github_will_not_show_is_an_error() {
            let nodes = format!("{},null", queue_node(1, &sha(1), 5, "User", "alice"));
            let (base, _) = recorded_api(vec![(200, queue_page(&nodes, None))]).await;
            let err = Github::at(base)
                .merge_queue("owner/demo", "main")
                .await
                .unwrap_err();
            assert!(matches!(err, GithubError::Unexpected(_)), "{err}");
        }

        #[tokio::test]
        async fn a_branch_contains_a_commit_it_is_behind_or_identical_to() {
            let answers = ["behind", "identical", "ahead", "diverged"]
                .map(|s| (200, format!(r#"{{"status":"{s}","ahead_by":0}}"#)));
            let (base, seen) = recorded_api(answers.to_vec()).await;
            let gh = Github::at(base);
            let mut got = Vec::new();
            for _ in 0..4 {
                got.push(
                    gh.branch_contains("owner/demo", "release/1.x", &sha(9))
                        .await
                        .unwrap(),
                );
            }
            assert_eq!(got, [true, true, false, false]);
            assert_eq!(
                seen.lock().unwrap()[0].0,
                format!(
                    "GET /repos/owner/demo/compare/release/1.x...{}?per_page=1 HTTP/1.1",
                    sha(9)
                )
            );
        }

        #[test]
        fn a_ref_is_escaped_but_keeps_its_slashes() {
            assert_eq!(escape_ref("feat/a b#c&d"), "feat/a%20b%23c%26d");
            assert_eq!(escape_ref("owner:main"), "owner%3Amain");
            assert_eq!(escape_ref("v1.2_x-y~z"), "v1.2_x-y~z");
        }
    }

    // --- the store-backed client (ADR-0027) -----------------------------------

    mod store_backed {
        use super::*;
        use crate::credential::FINE_GRAINED_PREFIX;
        use crate::setup::fsx::tests::{me, Scratch};
        use crate::setup::fsx::Account;

        fn tok(fill: char) -> Secret {
            Secret::new(format!(
                "{FINE_GRAINED_PREFIX}{}",
                fill.to_string().repeat(40)
            ))
        }

        /// A store in a scratch directory owned by this process, holding a
        /// token for `owner`, and the daemon's client reading it.
        fn client(s: &Scratch, owner: &str) -> (Github, Store, Account) {
            use std::os::unix::fs::PermissionsExt;
            let me = me(s);
            std::fs::create_dir(s.path("etc")).unwrap();
            std::fs::set_permissions(s.path("etc"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            let store = Store::owned_by(s.path("etc/credentials"), me, me);
            save(&store, owner, 'A', me);
            let gh = Github::from_store(store.load().unwrap(), store.clone()).unwrap();
            (gh, store, me)
        }

        /// Write a token file for `owner`, by rename as `sudo faber auth set`
        /// does, so even rewriting the same token changes the store's stamp.
        fn save(store: &Store, owner: &str, fill: char, _me: Account) {
            let far = crate::audit::now_ms() + 30 * 24 * 60 * 60 * 1000;
            store
                .save(owner, &tok(fill), far, crate::audit::now_ms())
                .unwrap();
        }

        fn block(gh: &Github) -> String {
            gh.claim_block().expect("claims are blocked")
        }

        /// GitHub's word that the token acts as another account is lifted
        /// only by GitHub confirming a token for the served owner: not by
        /// rewriting the same token in place, not by a store that briefly held
        /// another owner's file, and not by a different token on its own.
        #[tokio::test]
        async fn acts_as_other_stands_until_github_confirms_the_owner() {
            let s = Scratch::new("gh-acts-as-other");
            let (gh, store, me) = client(&s, "owner");
            assert!(gh.acts_as_other("intruder", "owner"));
            assert!(block(&gh).contains("acts as intruder"));

            save(&store, "owner", 'A', me);
            assert!(!gh.reload_if_changed().await, "the same token");
            assert!(block(&gh).contains("acts as intruder"), "{}", block(&gh));

            save(&store, "other", 'B', me);
            assert!(!gh.reload_if_changed().await);
            assert!(block(&gh).contains("different owner"), "{}", block(&gh));
            save(&store, "owner", 'A', me);
            assert!(!gh.reload_if_changed().await, "the same token, back");
            assert!(block(&gh).contains("acts as intruder"), "{}", block(&gh));

            save(&store, "owner", 'C', me);
            assert!(gh.reload_if_changed().await, "a different token is news");
            assert!(gh.unconfirmed(), "and waits for GitHub's word");
            assert!(block(&gh).contains("acts as intruder"), "{}", block(&gh));

            save(&store, "owner", 'C', me);
            assert!(!gh.reload_if_changed().await, "the same token, rewritten");
            assert!(gh.unconfirmed(), "still waits for GitHub's word");

            assert!(gh.owner_confirmed());
            assert!(!gh.unconfirmed());
            assert_eq!(gh.claim_block(), None);
            assert!(!gh.owner_confirmed(), "news only once");
        }

        /// A token for another owner stored under the daemon blocks claims:
        /// discovery is scoped to the owner fixed at boot.
        #[tokio::test]
        async fn another_owners_token_sets_owner_changed() {
            let s = Scratch::new("gh-owner-changed");
            let (gh, store, me) = client(&s, "owner");
            assert_eq!(gh.claim_block(), None);
            save(&store, "other", 'B', me);
            assert!(!gh.reload_if_changed().await);
            let why = block(&gh);
            assert!(
                why.contains("other") && why.contains("restart faber"),
                "{why}"
            );
        }

        /// A `401` stays a rejection when re-reading the store finds the same
        /// token, even though its file was rewritten; a new token for the
        /// served owner ends it.
        #[tokio::test]
        async fn a_rejection_outlasts_the_same_token_and_ends_with_a_new_one() {
            let s = Scratch::new("gh-rejected");
            let (mut gh, store, me) = client(&s, "owner");
            let (base, seen) = scripted_api(vec![(401, r#"{"message":"Bad credentials"}"#)]).await;
            gh.base = base;
            gh.current_login().await.unwrap_err();
            assert!(gh.is_rejected());
            assert_eq!(
                seen.lock().unwrap().len(),
                1,
                "the same token is not re-sent"
            );

            save(&store, "owner", 'A', me);
            assert!(!gh.reload_if_changed().await);
            assert!(
                gh.is_rejected(),
                "re-reading the same token does not un-reject it"
            );

            save(&store, "owner", 'D', me);
            assert!(gh.reload_if_changed().await);
            assert!(!gh.is_rejected());
            assert!(gh.unconfirmed());
        }

        /// A different token taken by a `401`'s re-read is confirmed with
        /// GitHub too, like one a store change brings.
        #[tokio::test]
        async fn a_token_taken_on_a_401_awaits_confirmation() {
            let s = Scratch::new("gh-401-retake");
            let (mut gh, store, me) = client(&s, "owner");
            let (base, seen) = scripted_api(vec![
                (401, r#"{"message":"Bad credentials"}"#),
                (200, r#"{"login":"owner"}"#),
            ])
            .await;
            gh.base = base;
            save(&store, "owner", 'E', me);
            assert_eq!(gh.current_login().await.unwrap(), "owner");
            assert_eq!(seen.lock().unwrap().len(), 2, "re-sent with the new token");
            assert!(gh.unconfirmed());
        }

        /// A write is not re-sent on a token a `401`'s re-read took: GitHub has
        /// not yet said whom it acts as, and the write could land in another
        /// account's repository. The `401` is returned instead.
        #[tokio::test]
        async fn a_write_is_not_resent_on_an_unconfirmed_token() {
            let s = Scratch::new("gh-401-write");
            let (mut gh, store, me) = client(&s, "owner");
            let (base, seen) = scripted_api(vec![
                (401, r#"{"message":"Bad credentials"}"#),
                (201, r#"{}"#),
            ])
            .await;
            gh.base = base;
            save(&store, "owner", 'G', me);
            let err = gh
                .post_commit_status("owner/demo", COMMIT, "success", "faber/test", None, "test")
                .await
                .unwrap_err();
            assert!(
                matches!(err, GithubError::Status { status: 401, .. }),
                "{err}"
            );
            assert_eq!(seen.lock().unwrap().len(), 1, "not re-sent");
            assert!(gh.unconfirmed());
            assert!(!gh.is_rejected(), "the new token was never sent");
            assert_eq!(block(&gh), crate::credential::UNCONFIRMED);
        }

        /// A token taken from the store claims nothing until GitHub confirms
        /// it acts as the served owner; re-reading it confirms nothing.
        #[tokio::test]
        async fn a_new_token_blocks_claims_until_github_confirms_the_owner() {
            let s = Scratch::new("gh-unconfirmed-blocks");
            let (gh, store, me) = client(&s, "owner");
            assert_eq!(gh.claim_block(), None);
            save(&store, "owner", 'F', me);
            assert!(gh.reload_if_changed().await);
            assert_eq!(block(&gh), crate::credential::UNCONFIRMED);
            save(&store, "owner", 'F', me);
            assert!(!gh.reload_if_changed().await);
            assert_eq!(
                block(&gh),
                crate::credential::UNCONFIRMED,
                "still unchecked"
            );
            assert!(!gh.owner_confirmed(), "no ActsAsOther block to end");
            assert_eq!(gh.claim_block(), None);
        }

        /// A new token GitHub rejects under a standing ActsAsOther block is
        /// recorded as rejected, so it is throttled, and the block keeps its
        /// reason. Re-reading the same token changes neither.
        #[tokio::test]
        async fn a_rejected_token_under_acts_as_other_stays_rejected_and_blocked() {
            let s = Scratch::new("gh-acts-as-other-401");
            let (mut gh, store, me) = client(&s, "owner");
            let (base, seen) = scripted_api(vec![(401, r#"{"message":"Bad credentials"}"#)]).await;
            gh.base = base;
            assert!(gh.acts_as_other("intruder", "owner"));
            save(&store, "owner", 'C', me);
            assert!(gh.reload_if_changed().await);
            assert!(gh.unconfirmed());
            gh.current_login().await.unwrap_err();
            assert_eq!(seen.lock().unwrap().len(), 1);
            assert!(gh.is_rejected(), "rejected, whatever else stands");
            assert!(block(&gh).contains("acts as intruder"), "{}", block(&gh));

            save(&store, "owner", 'C', me);
            assert!(!gh.reload_if_changed().await, "the same token, rewritten");
            assert!(gh.is_rejected(), "re-reading it does not un-reject it");
            assert!(gh.unconfirmed());
            assert!(block(&gh).contains("acts as intruder"), "{}", block(&gh));
        }
    }
}
