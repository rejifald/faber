//! The trust gate — the security-critical core of faber.
//!
//! Given the provenance of a single queued job (distilled from the GitHub API),
//! [`evaluate`] decides whether faber should provision a runner for it. It is a
//! pure function with no I/O, so it is exhaustively unit-testable.
//!
//! The *primary* safety control is `runs-on` trust-routing at the workflow level:
//! untrusted jobs (fork PRs, Dependabot) are routed to GitHub-hosted runners and
//! never target the self-hosted labels at all. This gate is independent
//! defense-in-depth — and it fails **closed**:
//!
//! # The invariant this gate does not enforce
//!
//! **Every job that can target faber's labels must be one you'd let run here.**
//! The gate decides what faber *mints for*, not what executes. A JIT runner is
//! not pinned to the job it was minted for: GitHub hands it to any queued job its
//! labels cover, refused ones included, and nothing faber controls can stop that
//! (ADR-0030). So a refusal here is a provisioning decision, and the routing in
//! the workflow is what keeps an untrusted job off the host. What faber adds is
//! detection: [`trust_verdict`] re-judges every job that actually ran on a runner
//! this host minted, and a refusal there is a recorded *gate bypass*
//! ([`BypassClass`]), enforced only under `[gate] on_bypass = "enforce"`
//! ([`OnBypass`]).
//!
//!
//! * Only an explicit allow-list of trigger events is ever provisioned; anything
//!   else (notably `pull_request_target`, whose run carries base-repo secrets but
//!   reports `head_repo == base_repo`, sailing past a naive fork check) is refused.
//! * Trust rests on *provenance* (an allowed event on a non-fork ref), not on the
//!   `actor` login — which for a fork PR is the attacker, and on a re-run is the
//!   original triggerer, not who re-ran. A missing head repository on a PR event
//!   is treated as suspicious (refused), not as "not a fork".
//! * **Third-party code is refused** (ADR-0059 §3), ahead of every other check.
//!   A job runs a person's code when it runs their pull request, or a merge
//!   group carrying one. faber reads whose code it is with its own token
//!   ([`Code`], [`crate::authorship`]) and refuses anyone who is not the repo's
//!   owner, a member of its organization or a collaborator. A dispatch made with
//!   a job's own token is refused too, unless `[gate] allow_job_token_dispatches`
//!   turns such dispatches back on: it runs whatever code a job pushed.
//! * The actor lists judge every login a job runs on behalf of: the actor, the
//!   triggering actor, and the author of each pull request whose code it runs.
//!   A maintainer touching a denied bot's PR no longer admits it.
//!
//! # What an operator can and cannot change
//!
//! Two of the boundary's edges are configurable, and the shape of that
//! configurability is itself a security property (issue #42):
//!
//! * **Events narrow only.** [`MAX_EVENTS`] is a compiled-in *ceiling*, and
//!   `[gate] allowed_events` is intersected with it. A config can say "push
//!   only"; nothing can add `pull_request_target`. The ceiling is wider than
//!   the default ([`DEFAULT_EVENTS`]): `repository_dispatch` and `release` sit
//!   under the ceiling but are opt-in, so a config that does not list them never gets them.
//! * **Public repos are opt-in.** `[gate] allow_public_repos` lifts the outright
//!   refusal, and lifting it weakens nothing else: fork PRs stay refused, the
//!   ceiling stays the ceiling.
//! * **Fork PRs have no key at all.** Not an oversight — job code reaches a
//!   privileged Docker daemon (issue #43), so accepting a stranger's PR would be
//!   handing out host root behind a toggle. There is nothing to set.
//!
//! All of it comes from the *operator's own* `config.toml`. A watched repo's
//! in-repo `.faber/config.toml` is reachable from a pull request, so it can never
//! name a gate key — [`crate::config::RepoConfig`]'s strict schema rejects them,
//! and there is a test that pins exactly that.

use std::collections::BTreeSet;

use crate::credential::Permission;

/// The login GitHub shows as a run's actor when a job's own `GITHUB_TOKEN`
/// caused it (ADR-0059 §3).
pub const JOB_TOKEN_ACTOR: &str = "github-actions[bot]";

/// The events a job's own token can cause that run another workflow's code
/// under that workflow's labels and secrets (ADR-0059 §3).
const DISPATCH_EVENTS: &[&str] = &["workflow_dispatch", "repository_dispatch"];

/// The branch prefix of a merge queue's temporary branches, which carry a merge
/// group's code: `gh-readonly-queue/<base>/pr-<n>-<sha>` (ADR-0059 §3).
pub const QUEUE_BRANCH_PREFIX: &str = "gh-readonly-queue/";

/// The **ceiling** on trigger events faber will ever provision for — not a
/// default a config replaces, but a maximum a config can only narrow.
///
/// Everything outside it is refused unconditionally, with no key to turn that
/// off: critically `pull_request_target`, `issue_comment`, and `workflow_run`,
/// whose provenance fields cannot be trusted to reflect the code that will run.
///
/// Every one of them runs code that already lives on a branch of the repo (push,
/// schedule, merge_group, the two dispatches) or is guarded by the fork check
/// (pull_request), so provenance — not the actor — carries the trust.
///
/// **Why `repository_dispatch` is safe under the ceiling.** It always runs the
/// workflow file from the repo's default branch, and the run's head is the repo
/// itself — a non-PR event, so the fork check is unaffected. Sending one needs a
/// token with `contents: write`, which can already push arbitrary code; a hostile
/// `client_payload` interpolated into a `run:` step therefore grants the sender
/// nothing they did not already hold. That is the same trust basis as
/// `workflow_dispatch` inputs. `pull_request_target`, `workflow_run` and
/// `issue_comment` stay outside, unchanged.
///
/// **Except when the sender is a job.** A job's own `GITHUB_TOKEN` can
/// hold `contents: write` and `actions: write`, and job code is the adversary
/// (ADR-0025): it can push code and dispatch a workflow that runs it, under that
/// workflow's labels and secrets. So either dispatch whose actor or triggering
/// actor is [`JOB_TOKEN_ACTOR`] is refused, whatever the actor lists say, unless
/// `[gate] allow_job_token_dispatches` turns such dispatches back on.
///
/// **Why `release` is safe under the ceiling.** Publishing a release needs
/// `contents: write`, and the run is at the release tag's commit — code in the
/// repo, and a non-PR event, so the fork check is unaffected. The tag may point
/// at any commit, but whoever creates it already has write access and can push
/// any branch, which faber already provisions for `push`.
///
/// The ceiling is *not* the default — see [`DEFAULT_EVENTS`].
///
/// Making this a ceiling rather than a default is what keeps the config from
/// being a way to widen the boundary. An operator can say "push only"; nobody can
/// say "and `pull_request_target` too". [`crate::config::GatePolicy`] rejects an
/// event outside this set at load, which also catches the typo that would
/// otherwise silently stop provisioning a whole trigger.
pub const MAX_EVENTS: &[&str] = &[
    "push",
    "pull_request",
    "workflow_dispatch",
    "schedule",
    "merge_group",
    "repository_dispatch",
    "release",
];

/// The events provisioned for when `[gate] allowed_events` is absent — faber's
/// historical five, i.e. [`MAX_EVENTS`] without the opt-in `repository_dispatch` and
/// `release`.
///
/// The ceiling and the default are different constants because
/// `repository_dispatch` and `release` are **opt-in**: a follower host updates itself unattended
/// (ADR-0006), and it must not start provisioning a trigger it never asked for.
/// An absent key keeps the old behaviour (ADR-0014); an operator who wants the
/// new trigger lists it explicitly in `allowed_events`.
pub const DEFAULT_EVENTS: &[&str] = &[
    "push",
    "pull_request",
    "workflow_dispatch",
    "schedule",
    "merge_group",
];

/// What faber does when a job it refused runs on a runner it minted anyway — a
/// gate bypass (ADR-0030). `[gate] on_bypass`; absent means [`OnBypass::Record`].
///
/// Detection is the same in both modes. `Enforce` adds, for a [`BypassClass::Trust`]
/// bypass only: kill the runner's container, delete the repo's cache volume, hold
/// the repo, and pause minting while a trust-refused job is queued in it. One
/// switch, so there is no half-enforced state to reason about.
///
/// The default is `Record` because hosts rely on today's behaviour: a whole class
/// of a repo's CI may run only by taking other jobs' runners, and enforcing on an
/// unattended update would break it with a failing job as the only symptom
/// (AGENTS.md). While `Record` sees trust bypasses, faber says so at every boot
/// and on the dashboard; flipping the default is a future, release-noted change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnBypass {
    /// Record the bypass (audit, faults feed, dashboard, tray). Nothing else.
    #[default]
    Record,
    /// Record it, and respond to a trust bypass with all four measures.
    Enforce,
}

impl OnBypass {
    pub fn as_str(self) -> &'static str {
        match self {
            OnBypass::Record => "record",
            OnBypass::Enforce => "enforce",
        }
    }
}

/// Which kind of rule a gate bypass broke (ADR-0030), because the response differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BypassClass {
    /// A fork, an untrusted actor, a public repo, or an event outside
    /// [`MAX_EVENTS`]: the code that ran may be hostile.
    Trust,
    /// An event under the ceiling that this host narrowed away or never opted
    /// into: the repo's own trusted code, just not wanted on this host.
    Policy,
    /// Third-party code (ADR-0059 §3): a stranger's pull request, a merge group
    /// carrying one or that left its queue unmerged, code faber could not
    /// attribute, or a dispatch made with a job's own token. Its own class
    /// because ADR-0059 §5 refuses it at job start in **both** modes, where
    /// `record` lets a trust refusal run; the job-start check enforces that.
    ThirdParty,
}

impl BypassClass {
    pub fn as_str(self) -> &'static str {
        match self {
            BypassClass::Trust => "trust",
            BypassClass::Policy => "policy",
            BypassClass::ThirdParty => "third_party",
        }
    }

    /// Whether the code that ran may be hostile: every class but policy. These
    /// are the bypasses `enforce` answers, the ones a queued job pauses minting
    /// for, and the ones `record` warns about.
    pub fn may_be_hostile(self) -> bool {
        self != BypassClass::Policy
    }
}

/// Classify a refusal as a bypass would be classified (ADR-0030).
///
/// Only an event inside the ceiling is policy: it is the one refusal an operator
/// could lift by config, so the code behind it is code this host would run if
/// asked. Third-party code is its own class (ADR-0059 §5). Everything else is
/// trust, including an event *outside* the ceiling — those are outside precisely
/// because their provenance cannot be believed.
pub fn classify(reason: &RefuseReason) -> BypassClass {
    match reason {
        RefuseReason::DisallowedEvent(e) if MAX_EVENTS.contains(&e.as_str()) => BypassClass::Policy,
        RefuseReason::DisallowedEvent(_)
        | RefuseReason::PublicRepo
        | RefuseReason::Fork
        | RefuseReason::UntrustedActor(_) => BypassClass::Trust,
        RefuseReason::ThirdParty(_)
        | RefuseReason::DestroyedGroup
        | RefuseReason::Unread
        | RefuseReason::JobTokenDispatch => BypassClass::ThirdParty,
    }
}

/// What faber should do with a queued job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Trusted, ours, and waiting for a runner — mint a JIT runner.
    Provision,
    /// Not faber's concern. Routine, not a security event.
    Skip(SkipReason),
    /// Targets our runner but fails a trust check — never provision. Security event.
    Refuse(RefuseReason),
}

/// Why a job was skipped (routine, expected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The job does not target our self-hosted labels.
    NotTargeted,
    /// The job is not actually waiting for a runner (e.g. `waiting` on an
    /// environment protection rule, or already `in_progress`).
    NotQueued,
}

/// Why a job was refused (a trust check failed — worth logging loudly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseReason {
    /// The repository is public and this host does not service public repos.
    /// faber mints runners that execute the repo's code on the host, so a public
    /// repo — whose queue can carry fork-PR / drive-by contributions — is refused
    /// by default. Checked ahead of event/fork/actor: a public repo targeting our
    /// runner is disqualified outright. Liftable via `[gate] allow_public_repos`,
    /// which changes nothing about the fork check or the event ceiling.
    PublicRepo,
    /// The trigger event is not provisioned for: outside [`MAX_EVENTS`] entirely
    /// (e.g. `pull_request_target`), or inside it but narrowed away by this
    /// host's `[gate] allowed_events`.
    DisallowedEvent(String),
    /// The head ref comes from a fork, or a PR run is missing head provenance
    /// (fail-closed). The authoritative fork signal is head repo != base repo.
    Fork,
    /// A login the job runs on behalf of — its actor, its triggering actor, or
    /// the author of a pull request whose code it runs — is explicitly denied
    /// (e.g. `dependabot[bot]`), or a non-empty allow-list is configured and the
    /// login is not in it.
    UntrustedActor(String),
    /// The code is this person's, and they are not the repo's owner, a member
    /// of its organization or a collaborator (ADR-0059 §3).
    ThirdParty(String),
    /// A merge group that left its queue without merging: GitHub keeps running
    /// it, and nothing says whose code it now carries (ADR-0059 §3).
    DestroyedGroup,
    /// faber could not read whose code the job runs (ADR-0059 §3).
    Unread,
    /// A `workflow_dispatch` or `repository_dispatch` made with a job's own
    /// token, and this host has not turned such dispatches back on.
    JobTokenDispatch,
}

/// Whose code a job runs, as faber read it from GitHub with its own token
/// (ADR-0059 §3). [`crate::authorship`] reads it; [`trust_verdict`] judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Code {
    /// The repository's own: code pushed, scheduled, dispatched or released by
    /// someone with write access, or a merge group that has merged.
    Own,
    /// These people's: the authors of the pull requests the job runs, or the
    /// actors of an event outside the ceiling.
    People(Vec<Person>),
    /// A merge group that left its queue without merging.
    DestroyedGroup,
    /// What faber could not read, and why. Refused.
    Unread(String),
    /// A private repository whose token lacks this read. faber keeps the
    /// verdict it gave before ADR-0059 for what it cannot read: only people
    /// with access can open a pull request there (ADR-0027, as ADR-0059
    /// amends it).
    Ungranted(Permission),
}

/// One person whose code a job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub login: String,
    /// Not the repo's owner, a member of its organization or a collaborator, by
    /// GitHub's record. Never true of an app (`[bot]`): the actor lists judge
    /// apps.
    pub third_party: bool,
}

/// Provenance of a single queued job, distilled from the GitHub REST API.
///
/// `event`, the actors, `base_repo` and `head_repo` come from the parent
/// `workflow_run`; `labels` and `status` come from the `job`; `code` is what
/// faber read of whose code it is.
#[derive(Debug, Clone)]
pub struct JobContext {
    /// Triggering event: `push`, `pull_request`, `workflow_dispatch`, `schedule`, …
    pub event: String,
    /// `actor.login`: who first caused the run. Empty when GitHub left it out.
    pub actor: String,
    /// `triggering_actor.login`: who caused this attempt, such as whoever
    /// clicked re-run. `None` when GitHub left it out.
    pub triggering_actor: Option<String>,
    /// `repository.full_name` — the base repo where the workflow runs.
    pub base_repo: String,
    /// `repository.private` — faber services **private repos only**. A public
    /// repo that targets our runner is refused (see [`RefuseReason::PublicRepo`]),
    /// since a runner executes the repo's code on the host. Fail-closed: an absent
    /// field deserializes to `false`, so unknown visibility refuses, never runs.
    pub private: bool,
    /// `head_repository.full_name` — where the head ref originated. `None` on
    /// events without a PR head (push / workflow_dispatch / schedule), and also
    /// when GitHub cannot resolve it (deleted fork/ref) — which on a PR event is
    /// treated as a fork (fail-closed).
    pub head_repo: Option<String>,
    /// Whose code the job runs (ADR-0059 §3).
    pub code: Code,
    /// The job's `runs-on` labels.
    pub labels: Vec<String>,
    /// The job's status: `queued`, `in_progress`, `waiting`, …
    pub status: String,
}

impl JobContext {
    /// Every login the actor lists judge: the actor and the triggering actor,
    /// and the author of each pull request whose code the job runs. An
    /// unnamed actor is judged as the empty login, so an allow-list refuses it.
    fn logins(&self) -> Vec<&str> {
        let mut out: Vec<&str> = [Some(self.actor.as_str()), self.triggering_actor.as_deref()]
            .into_iter()
            .flatten()
            .filter(|l| !l.is_empty())
            .collect();
        if out.is_empty() {
            out.push("");
        }
        if let Code::People(people) = &self.code {
            out.extend(people.iter().map(|p| p.login.as_str()));
        }
        out
    }

    /// Whether a job's own token caused this run: its actor or triggering
    /// actor is [`JOB_TOKEN_ACTOR`].
    fn by_job_token(&self) -> bool {
        [Some(self.actor.as_str()), self.triggering_actor.as_deref()]
            .into_iter()
            .flatten()
            .any(|l| l.eq_ignore_ascii_case(JOB_TOKEN_ACTOR))
    }
}

/// The policy a [`JobContext`] is evaluated against.
///
/// `trusted_actors` is an *optional* allow-list: empty means "trust by provenance
/// alone" (the event allow-list + fork check already established this is our own
/// code on a non-fork ref) — the right default for a single-owner account, and it
/// avoids false-refusing legitimate non-human actors (`web-flow` from the merge
/// button, your own bots/apps, the merge-queue bot). A non-empty allow-list
/// additionally requires the actor to be a member. `denied_actors` always wins.
///
/// Actor logins are matched case-insensitively. Construct via [`TrustPolicy::new`],
/// which enforces faber's historical boundary, then narrow it to the operator's
/// with [`TrustPolicy::with_boundary`].
#[derive(Debug, Clone)]
pub struct TrustPolicy {
    trusted_actors: BTreeSet<String>,
    denied_actors: BTreeSet<String>,
    /// Labels a job must carry to be considered ours (e.g. `self-hosted`,
    /// `macOS`, `ARM64`).
    pub required_labels: Vec<String>,
    /// The trigger events this host provisions for — always a subset of
    /// [`MAX_EVENTS`], never a superset. See [`Boundary`].
    allowed_events: BTreeSet<String>,
    /// Whether a public repo may be serviced at all. `false` — the default, and
    /// faber's historical behaviour — refuses one outright, ahead of every other
    /// security check.
    allow_public_repos: bool,
    /// Whether a dispatch made with a job's own token may run here.
    /// `false` — the default — refuses one whatever the actor lists say.
    allow_job_token_dispatches: bool,
    /// What a gate bypass triggers (ADR-0030). Not part of what [`evaluate`]
    /// decides; carried here because it is host-wide and reported with the
    /// boundary.
    on_bypass: OnBypass,
}

/// The trust boundary in force, in the shape the dashboard and the audit trail
/// report it.
///
/// It exists because the boundary used to be readable only from the source: a
/// refusal named a rule nobody could locate without a checkout. Whatever
/// [`evaluate`] is actually enforcing has to be answerable from outside the
/// process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Boundary {
    /// Events provisioned for, in [`MAX_EVENTS`] order so the readout is stable.
    pub allowed_events: Vec<String>,
    /// Events in [`DEFAULT_EVENTS`] that this host has narrowed *away* — the
    /// operator's own tightening, shown so "why was my PR job refused?" has an
    /// answer that isn't the source.
    pub excluded_events: Vec<String>,
    /// Events under the ceiling but outside [`DEFAULT_EVENTS`] that are not
    /// allowed here (`repository_dispatch` and `release` until listed). Kept apart from
    /// `excluded_events`: nobody narrowed these, they were never on.
    pub opt_in_events: Vec<String>,
    pub allow_public_repos: bool,
    /// Always false, and present so the readout says so rather than being silent
    /// about the question everyone asks. There is no key for it: fork PRs reach a
    /// privileged Docker daemon (issue #43), and a container is not the boundary
    /// that would make accepting them safe.
    pub allow_fork_prs: bool,
    /// Whether this host runs dispatches made with a job's own token
    /// (`[gate] allow_job_token_dispatches`). Off unless the operator says so.
    pub allow_job_token_dispatches: bool,
    /// What a gate bypass triggers on this host (ADR-0030): `record` or `enforce`.
    pub on_bypass: OnBypass,
}

impl TrustPolicy {
    /// Build a policy with faber's default events ([`DEFAULT_EVENTS`]) and public
    /// repos refused — the boundary faber has always enforced.
    ///
    /// Actor logins are lower-cased so matching is case-insensitive.
    pub fn new(
        trusted_actors: impl IntoIterator<Item = String>,
        denied_actors: impl IntoIterator<Item = String>,
        required_labels: Vec<String>,
    ) -> Self {
        Self {
            trusted_actors: trusted_actors
                .into_iter()
                .map(|a| a.to_lowercase())
                .collect(),
            denied_actors: denied_actors
                .into_iter()
                .map(|a| a.to_lowercase())
                .collect(),
            required_labels,
            allowed_events: DEFAULT_EVENTS.iter().map(|e| e.to_string()).collect(),
            allow_public_repos: false,
            allow_job_token_dispatches: false,
            on_bypass: OnBypass::Record,
        }
    }

    /// Narrow the boundary to the operator's configured one.
    ///
    /// **Intersected with [`MAX_EVENTS`], never unioned.** An event outside the
    /// ceiling cannot be admitted here even if a caller passes one, which makes
    /// this method incapable of widening the boundary regardless of what reaches
    /// it — the property that lets config be parsed leniently and still be safe.
    /// [`crate::config::GatePolicy`] additionally rejects such an event at load,
    /// so it is a loud error rather than a silent drop.
    pub fn with_boundary(
        mut self,
        allowed_events: impl IntoIterator<Item = String>,
        allow_public_repos: bool,
    ) -> Self {
        self.allowed_events = allowed_events
            .into_iter()
            .filter(|e| MAX_EVENTS.contains(&e.as_str()))
            .collect();
        self.allow_public_repos = allow_public_repos;
        self
    }

    /// Set what a gate bypass triggers (ADR-0030). Changes nothing [`evaluate`]
    /// decides.
    pub fn with_on_bypass(mut self, on_bypass: OnBypass) -> Self {
        self.on_bypass = on_bypass;
        self
    }

    pub fn on_bypass(&self) -> OnBypass {
        self.on_bypass
    }

    /// Turn dispatches made with a job's own token back on (`[gate]
    /// allow_job_token_dispatches`). The actor lists still judge them.
    pub fn with_job_token_dispatches(mut self, allowed: bool) -> Self {
        self.allow_job_token_dispatches = allowed;
        self
    }

    /// What this policy enforces, for the dashboard and the audit trail.
    pub fn boundary(&self) -> Boundary {
        Boundary {
            allowed_events: MAX_EVENTS
                .iter()
                .filter(|e| self.allowed_events.contains(**e))
                .map(|e| e.to_string())
                .collect(),
            excluded_events: MAX_EVENTS
                .iter()
                .filter(|e| DEFAULT_EVENTS.contains(*e) && !self.allowed_events.contains(**e))
                .map(|e| e.to_string())
                .collect(),
            opt_in_events: MAX_EVENTS
                .iter()
                .filter(|e| !DEFAULT_EVENTS.contains(*e) && !self.allowed_events.contains(**e))
                .map(|e| e.to_string())
                .collect(),
            allow_public_repos: self.allow_public_repos,
            allow_fork_prs: false,
            allow_job_token_dispatches: self.allow_job_token_dispatches,
            on_bypass: self.on_bypass,
        }
    }
}

/// Decide what faber should do with a queued job.
///
/// Order matters: routine skips (not ours / not waiting) precede security
/// refusals, so the audit log distinguishes "ignored a job that wasn't mine"
/// from "blocked an untrusted job that targeted my runner".
pub fn evaluate(ctx: &JobContext, policy: &TrustPolicy) -> Verdict {
    // 1. Is this even our kind of job? Every required label must be present —
    //    and an empty policy targets *nothing*, never everything.
    let targets_us = !policy.required_labels.is_empty()
        && policy
            .required_labels
            .iter()
            .all(|req| ctx.labels.iter().any(|have| have == req));
    if !targets_us {
        return Verdict::Skip(SkipReason::NotTargeted);
    }

    // 2. Is it actually waiting for a runner? `waiting` jobs are gated on an
    //    environment, not on runner availability; don't provision for them.
    if ctx.status != "queued" {
        return Verdict::Skip(SkipReason::NotQueued);
    }

    trust_verdict(ctx, policy)
}

/// The trust checks alone — [`evaluate`]'s steps 3–6, without "is it ours" and
/// "is it queued".
///
/// [`evaluate`] asks whether to mint for a queued job. Bypass detection (ADR-0030)
/// asks a different question of a job that already *ran* on a runner this host
/// minted: would the gate have refused it? Such a job is ours by construction
/// (GitHub handed it our runner, whatever subset of the labels it asked for) and
/// is no longer queued, so the two routine skips would only hide the answer.
/// Never returns [`Verdict::Skip`].
pub fn trust_verdict(ctx: &JobContext, policy: &TrustPolicy) -> Verdict {
    // Third-party code first (ADR-0059 §3). At job start it is refused in both
    // modes, where `record` lets a trust refusal run (§5), so it must be the
    // reason a job reports whatever else the job also fails.
    if let Some(reason) = third_party_code(ctx, policy) {
        return Verdict::Refuse(reason);
    }

    // 3. Private repos by default. A runner executes the repo's code on the host,
    //    so a public repo — whose queue can carry fork-PR / drive-by
    //    contributions — is refused outright, before any event/fork/actor
    //    reasoning. Fail-closed: a payload missing `private` deserializes to
    //    `false`, so "unknown visibility" refuses rather than runs.
    //
    //    An operator can lift this (`[gate] allow_public_repos`), and what makes
    //    that a defensible choice rather than a hole is that the checks below do
    //    not weaken with it: a fork PR is still refused by step 5, and every
    //    provenance-spoofable trigger is still outside `MAX_EVENTS` and
    //    unreachable by any config. What a public repo adds is that *anyone* may
    //    open a PR — and every such PR comes from a fork, which is exactly what
    //    step 5 refuses.
    if !ctx.private && !policy.allow_public_repos {
        return Verdict::Refuse(RefuseReason::PublicRepo);
    }

    // 4. Only provision for allow-listed events. This is what stops
    //    `pull_request_target` (base-repo secrets, head_repo == base_repo) and
    //    other provenance-spoofable triggers — the fork check below cannot.
    //
    //    The set is the operator's, but only ever a subset of `MAX_EVENTS`
    //    ([`TrustPolicy::with_boundary`] intersects rather than replaces), so
    //    configuring this can narrow the boundary and can never widen it.
    if !policy.allowed_events.contains(&ctx.event) {
        return Verdict::Refuse(RefuseReason::DisallowedEvent(ctx.event.clone()));
    }

    // 5. Fork check, fail-closed. A fork's head repo differs from the base repo.
    //    For a `pull_request` we *demand* positive same-repo provenance: a missing
    //    head repository (deleted fork/ref, revoked access) is suspicious, not
    //    safe. Non-PR events legitimately carry no head repo (code already on a
    //    repo branch). NB: full_name is mutable (rename/transfer) — a Phase-3
    //    hardening is to compare immutable repository ids instead.
    match &ctx.head_repo {
        Some(head) if !head.eq_ignore_ascii_case(&ctx.base_repo) => {
            return Verdict::Refuse(RefuseReason::Fork);
        }
        None if ctx.event == "pull_request" => {
            return Verdict::Refuse(RefuseReason::Fork);
        }
        _ => {}
    }

    // 6. Actor trust, over every login the job runs on behalf of: the actor, the
    //    triggering actor, and each pull request author whose code it runs,
    //    so a maintainer's click no longer admits a denied bot's PR. The
    //    deny-list always wins (e.g. dependabot[bot], whose PRs come from
    //    same-repo branches and so pass the fork check). A non-empty allow-list
    //    additionally restricts to its members; an empty allow-list trusts any
    //    non-denied login, since steps 3–4 already established this is our own
    //    code on a non-fork ref.
    for login in ctx.logins() {
        let lower = login.to_lowercase();
        if policy.denied_actors.contains(&lower)
            || (!policy.trusted_actors.is_empty() && !policy.trusted_actors.contains(&lower))
        {
            return Verdict::Refuse(RefuseReason::UntrustedActor(login.to_string()));
        }
    }

    Verdict::Provision
}

/// The refusal of third-party code (ADR-0059 §3), if `ctx` runs any: a
/// dispatch made with a job's own token that this host has not turned back on,
/// a destroyed merge group, code faber could not attribute, or a stranger's.
///
/// A private repo whose token lacks a read keeps the verdict it had before
/// ADR-0059 for what it cannot read, so [`Code::Ungranted`] is no refusal here.
fn third_party_code(ctx: &JobContext, policy: &TrustPolicy) -> Option<RefuseReason> {
    if !policy.allow_job_token_dispatches
        && DISPATCH_EVENTS.contains(&ctx.event.as_str())
        && ctx.by_job_token()
    {
        return Some(RefuseReason::JobTokenDispatch);
    }
    match &ctx.code {
        Code::DestroyedGroup => Some(RefuseReason::DestroyedGroup),
        Code::Unread(_) => Some(RefuseReason::Unread),
        Code::People(people) => people
            .iter()
            .find(|p| p.third_party)
            .map(|p| RefuseReason::ThirdParty(p.login.clone())),
        Code::Own | Code::Ungranted(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABELS: [&str; 3] = ["self-hosted", "macOS", "ARM64"];

    fn labels() -> Vec<String> {
        LABELS.iter().map(|s| s.to_string()).collect()
    }

    /// Default daemon policy: empty allow-list (provenance trust), deny Dependabot.
    fn policy() -> TrustPolicy {
        TrustPolicy::new([], ["dependabot[bot]".to_string()], labels())
    }

    /// Policy with a human-only allow-list (the optional tightening).
    fn allowlist_policy() -> TrustPolicy {
        TrustPolicy::new(
            ["owner".to_string()],
            ["dependabot[bot]".to_string()],
            labels(),
        )
    }

    /// A trusted, ours, queued job — the happy path (owner push).
    fn trusted_job() -> JobContext {
        JobContext {
            event: "push".to_string(),
            actor: "owner".to_string(),
            triggering_actor: None,
            base_repo: "owner/demo".to_string(),
            private: true,
            head_repo: None,
            code: Code::Own,
            labels: labels(),
            status: "queued".to_string(),
        }
    }

    fn person(login: &str, third_party: bool) -> Person {
        Person {
            login: login.to_string(),
            third_party,
        }
    }

    /// A same-repo pull request run, its code `people`'s.
    fn pr_job(people: Vec<Person>) -> JobContext {
        JobContext {
            event: "pull_request".to_string(),
            head_repo: Some("owner/demo".to_string()),
            code: Code::People(people),
            ..trusted_job()
        }
    }

    // --- happy paths --------------------------------------------------------

    #[test]
    fn push_by_owner_provisions() {
        assert_eq!(evaluate(&trusted_job(), &policy()), Verdict::Provision);
    }

    #[test]
    fn same_repo_pr_provisions() {
        let mut ctx = trusted_job();
        ctx.event = "pull_request".to_string();
        ctx.head_repo = Some("owner/demo".to_string());
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
    }

    #[test]
    fn workflow_dispatch_provisions() {
        let mut ctx = trusted_job();
        ctx.event = "workflow_dispatch".to_string();
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
    }

    #[test]
    fn merge_group_by_bot_provisions_under_provenance_trust() {
        // Merge-queue runs are synthesized by GitHub with a bot actor and no head
        // repo; provenance trust (empty allow-list) must accept them.
        let mut ctx = trusted_job();
        ctx.event = "merge_group".to_string();
        ctx.actor = "github-actions[bot]".to_string();
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
    }

    #[test]
    fn web_flow_merge_button_provisions_under_provenance_trust() {
        // Merging your own PR via the web UI yields a push run with actor web-flow.
        let mut ctx = trusted_job();
        ctx.actor = "web-flow".to_string();
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
    }

    // --- the bypasses the adversarial review found --------------------------

    #[test]
    fn pull_request_target_is_refused_as_disallowed_event() {
        // The headline bypass: base-repo secrets, head_repo == base_repo, would
        // pass a naive fork check. The event allow-list stops it.
        let mut ctx = trusted_job();
        ctx.event = "pull_request_target".to_string();
        ctx.head_repo = Some("owner/demo".to_string());
        ctx.actor = "owner".to_string();
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::DisallowedEvent(
                "pull_request_target".to_string()
            ))
        );
    }

    #[test]
    fn missing_head_on_pr_fails_closed() {
        // Deleted fork/ref → head_repo None on a pull_request. Must refuse, not skip.
        let mut ctx = trusted_job();
        ctx.event = "pull_request".to_string();
        ctx.head_repo = None;
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::Fork)
        );
    }

    #[test]
    fn fork_pr_is_refused() {
        let mut ctx = trusted_job();
        ctx.event = "pull_request".to_string();
        ctx.head_repo = Some("someone-else/demo".to_string());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::Fork)
        );
    }

    #[test]
    fn issue_comment_event_is_refused() {
        let mut ctx = trusted_job();
        ctx.event = "issue_comment".to_string();
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::DisallowedEvent("issue_comment".to_string()))
        );
    }

    // --- the configurable boundary (issue #42) ------------------------------

    /// `push`-only, public repos still refused — an operator tightening.
    fn push_only() -> TrustPolicy {
        policy().with_boundary(["push".to_string()], false)
    }

    #[test]
    fn the_boundary_cannot_be_widened_past_the_ceiling() {
        // The property the whole design rests on: `with_boundary` intersects with
        // `MAX_EVENTS`, so even a caller that hands it a provenance-spoofable
        // trigger cannot make the gate accept one. config's validator rejects it
        // too, but this is the layer that makes that a convenience rather than the
        // only thing standing in the way.
        let wide = policy().with_boundary(
            [
                "push".to_string(),
                "pull_request_target".to_string(),
                "workflow_run".to_string(),
                "issue_comment".to_string(),
            ],
            false,
        );
        assert_eq!(wide.boundary().allowed_events, vec!["push".to_string()]);
        for smuggled in ["pull_request_target", "workflow_run", "issue_comment"] {
            let mut ctx = trusted_job();
            ctx.event = smuggled.to_string();
            ctx.head_repo = Some("owner/demo".to_string());
            assert_eq!(
                evaluate(&ctx, &wide),
                Verdict::Refuse(RefuseReason::DisallowedEvent(smuggled.to_string())),
                "{smuggled} must stay refused however it got into the config"
            );
        }
    }

    #[test]
    fn narrowing_the_events_refuses_what_was_dropped() {
        let mut ctx = trusted_job();
        ctx.event = "pull_request".to_string();
        ctx.head_repo = Some("owner/demo".to_string());
        // Provisioned under the default boundary...
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
        // ...and refused once the operator says push-only.
        assert_eq!(
            evaluate(&ctx, &push_only()),
            Verdict::Refuse(RefuseReason::DisallowedEvent("pull_request".to_string()))
        );
        assert_eq!(evaluate(&trusted_job(), &push_only()), Verdict::Provision);
    }

    #[test]
    fn an_empty_event_set_refuses_everything_it_is_shown() {
        // The maximally restrictive setting, and a valid one. It must still be a
        // refusal rather than a panic or a fall-through to provision.
        let closed = policy().with_boundary([], false);
        assert!(closed.boundary().allowed_events.is_empty());
        assert_eq!(
            evaluate(&trusted_job(), &closed),
            Verdict::Refuse(RefuseReason::DisallowedEvent("push".to_string()))
        );
        // Still a routine skip for a job that was never ours.
        let mut other = trusted_job();
        other.labels = vec!["ubuntu-latest".to_string()];
        assert_eq!(
            evaluate(&other, &closed),
            Verdict::Skip(SkipReason::NotTargeted)
        );
    }

    #[test]
    fn allowing_public_repos_provisions_an_owner_push() {
        let mut ctx = trusted_job();
        ctx.private = false;
        assert_eq!(
            evaluate(
                &ctx,
                &policy().with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), true)
            ),
            Verdict::Provision
        );
    }

    #[test]
    fn allowing_public_repos_does_not_admit_a_fork_pr() {
        // The composition claim, and the reason lifting the visibility refusal is
        // defensible rather than a hole. On a public repo *anyone* can open a PR —
        // and every such PR comes from a fork, which step 5 refuses on its own.
        let open = policy().with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), true);
        let mut ctx = trusted_job();
        ctx.private = false;
        ctx.event = "pull_request".to_string();
        ctx.head_repo = Some("a-stranger/demo".to_string());
        assert_eq!(evaluate(&ctx, &open), Verdict::Refuse(RefuseReason::Fork));
        // And the deleted-fork case still fails closed.
        ctx.head_repo = None;
        assert_eq!(evaluate(&ctx, &open), Verdict::Refuse(RefuseReason::Fork));
    }

    #[test]
    fn allowing_public_repos_does_not_admit_a_spoofable_trigger() {
        let open = policy().with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), true);
        let mut ctx = trusted_job();
        ctx.private = false;
        ctx.event = "pull_request_target".to_string();
        ctx.head_repo = Some("owner/demo".to_string());
        assert_eq!(
            evaluate(&ctx, &open),
            Verdict::Refuse(RefuseReason::DisallowedEvent(
                "pull_request_target".to_string()
            ))
        );
    }

    #[test]
    fn the_default_boundary_is_fabers_historical_one() {
        // A `TrustPolicy` built without a boundary must enforce exactly what faber
        // shipped before this was configurable — that is what lets a config with
        // no `[gate]` section be a no-op for every installed host.
        let b = policy().boundary();
        assert_eq!(b.allowed_events, DEFAULT_EVENTS.to_vec());
        // `repository_dispatch` is under the ceiling but opt-in, so by default it
        // is reported as opt-in-and-off (not as narrowed: nobody narrowed it),
        // which is what makes its refusal legible.
        assert!(b.excluded_events.is_empty());
        assert_eq!(
            b.opt_in_events,
            vec!["repository_dispatch".to_string(), "release".to_string()]
        );
        assert!(!b.allow_public_repos);
        assert!(!b.allow_fork_prs);
    }

    #[test]
    fn the_boundary_reports_what_the_operator_narrowed_away() {
        // The readout exists so a refusal names a rule someone can locate. Knowing
        // which events were dropped is the half that answers "why was my PR job
        // refused?" without a checkout.
        let b = push_only().boundary();
        assert_eq!(b.allowed_events, vec!["push".to_string()]);
        assert_eq!(
            b.excluded_events,
            vec![
                "pull_request".to_string(),
                "workflow_dispatch".to_string(),
                "schedule".to_string(),
                "merge_group".to_string(),
            ]
        );
        assert_eq!(
            b.opt_in_events,
            vec!["repository_dispatch".to_string(), "release".to_string()]
        );
    }

    #[test]
    fn release_is_opt_in() {
        let mut ctx = trusted_job();
        ctx.event = "release".to_string();
        assert!(MAX_EVENTS.contains(&"release"));
        assert!(!DEFAULT_EVENTS.contains(&"release"));
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::DisallowedEvent("release".to_string()))
        );
        let opted_in = policy().with_boundary(
            DEFAULT_EVENTS
                .iter()
                .map(|e| e.to_string())
                .chain(["release".to_string()]),
            false,
        );
        assert_eq!(evaluate(&ctx, &opted_in), Verdict::Provision);
        assert_eq!(
            opted_in.boundary().opt_in_events,
            vec!["repository_dispatch".to_string()]
        );
    }

    #[test]
    fn repository_dispatch_is_opt_in() {
        let mut ctx = trusted_job();
        ctx.event = "repository_dispatch".to_string();
        // Under the ceiling, but not in the default set: refused unless listed.
        assert!(MAX_EVENTS.contains(&"repository_dispatch"));
        assert!(!DEFAULT_EVENTS.contains(&"repository_dispatch"));
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::DisallowedEvent(
                "repository_dispatch".to_string()
            ))
        );
        // Listed: a private non-fork repository_dispatch job is provisioned.
        let opted_in = policy().with_boundary(
            DEFAULT_EVENTS
                .iter()
                .map(|e| e.to_string())
                .chain(["repository_dispatch".to_string()]),
            false,
        );
        assert_eq!(evaluate(&ctx, &opted_in), Verdict::Provision);
        assert!(opted_in.boundary().excluded_events.is_empty());
        assert_eq!(
            opted_in.boundary().opt_in_events,
            vec!["release".to_string()]
        );
        // Opting in does not open the spoofable triggers.
        let mut spoof = trusted_job();
        spoof.event = "pull_request_target".to_string();
        assert_eq!(
            evaluate(&spoof, &opted_in),
            Verdict::Refuse(RefuseReason::DisallowedEvent(
                "pull_request_target".to_string()
            ))
        );
    }

    #[test]
    fn fork_prs_are_refused_with_no_way_to_say_otherwise() {
        // There is no key, no builder argument and no code path. While job code
        // reaches a privileged Docker daemon (issue #43), this is not a toggle —
        // and the readout says so rather than staying silent about it.
        let mut ctx = trusted_job();
        ctx.event = "pull_request".to_string();
        ctx.head_repo = Some("a-stranger/demo".to_string());
        for p in [
            policy(),
            push_only(),
            policy().with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), true),
        ] {
            assert!(!p.boundary().allow_fork_prs);
            // Refused under every boundary. `push_only` refuses it a step earlier,
            // as a disallowed event — narrower still, and the point is that no
            // setting reaches `Provision`.
            assert!(
                matches!(evaluate(&ctx, &p), Verdict::Refuse(_)),
                "a fork PR must never be provisioned, boundary {:?}",
                p.boundary()
            );
        }
        // Where `pull_request` *is* allowed, the reason is the fork check itself.
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::Fork)
        );
    }

    // --- repo visibility ----------------------------------------------------

    #[test]
    fn public_repo_is_refused() {
        // Same trusted owner-push, but the repo is public — a runner would execute
        // its code on the host, so refuse.
        let mut ctx = trusted_job();
        ctx.private = false;
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::PublicRepo)
        );
    }

    #[test]
    fn public_repo_refused_ahead_of_event_and_fork_checks() {
        // A public repo carrying an otherwise-refusable trigger (disallowed event
        // *and* a fork head) is still refused as PublicRepo — visibility is the
        // first security gate, so the audit reason is unambiguous.
        let mut ctx = trusted_job();
        ctx.private = false;
        ctx.event = "pull_request_target".to_string();
        ctx.head_repo = Some("attacker/demo".to_string());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::PublicRepo)
        );
    }

    #[test]
    fn public_repo_not_targeting_us_is_skipped_not_refused() {
        // Visibility is a *security* refusal, so it must sit behind the "is this
        // even ours?" label check — a public repo whose job doesn't target our
        // labels is routine noise, not a security event.
        let mut ctx = trusted_job();
        ctx.private = false;
        ctx.labels = vec!["ubuntu-latest".to_string()];
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Skip(SkipReason::NotTargeted)
        );
    }

    // --- actor model --------------------------------------------------------

    #[test]
    fn dependabot_is_refused_even_though_same_repo() {
        // Dependabot PRs come from a same-repo branch, so they PASS the fork
        // check — the deny-list is what stops them.
        let mut ctx = trusted_job();
        ctx.event = "pull_request".to_string();
        ctx.head_repo = Some("owner/demo".to_string());
        ctx.actor = "dependabot[bot]".to_string();
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::UntrustedActor("dependabot[bot]".to_string()))
        );
    }

    #[test]
    fn empty_allowlist_trusts_unknown_actor_on_non_fork() {
        // Provenance trust: an unknown (non-denied) actor on a non-fork allowed
        // event is trusted — the (c) single-owner default.
        let mut ctx = trusted_job();
        ctx.actor = "some-app[bot]".to_string();
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
    }

    #[test]
    fn non_empty_allowlist_refuses_outsiders() {
        let mut ctx = trusted_job();
        ctx.actor = "random-collaborator".to_string();
        assert_eq!(
            evaluate(&ctx, &allowlist_policy()),
            Verdict::Refuse(RefuseReason::UntrustedActor(
                "random-collaborator".to_string()
            ))
        );
    }

    #[test]
    fn non_empty_allowlist_admits_members() {
        assert_eq!(
            evaluate(&trusted_job(), &allowlist_policy()),
            Verdict::Provision
        );
    }

    // --- skips and precedence ----------------------------------------------

    #[test]
    fn wrong_labels_are_skipped_not_refused() {
        let mut ctx = trusted_job();
        ctx.labels = vec!["ubuntu-latest".to_string()];
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Skip(SkipReason::NotTargeted)
        );
    }

    #[test]
    fn partial_label_match_is_skipped() {
        let mut ctx = trusted_job();
        ctx.labels = vec!["self-hosted".to_string(), "macOS".to_string()];
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Skip(SkipReason::NotTargeted)
        );
    }

    #[test]
    fn non_queued_status_is_skipped() {
        let mut ctx = trusted_job();
        ctx.status = "waiting".to_string();
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Skip(SkipReason::NotQueued)
        );
    }

    #[test]
    fn empty_required_labels_match_nothing() {
        let empty = TrustPolicy::new(["owner".to_string()], [], vec![]);
        assert_eq!(
            evaluate(&trusted_job(), &empty),
            Verdict::Skip(SkipReason::NotTargeted)
        );
    }

    #[test]
    fn label_skip_takes_precedence_over_security_checks() {
        // A disallowed-event fork PR that doesn't even target us is a routine Skip.
        let mut ctx = trusted_job();
        ctx.event = "pull_request_target".to_string();
        ctx.labels = vec!["ubuntu-latest".to_string()];
        ctx.head_repo = Some("someone-else/demo".to_string());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Skip(SkipReason::NotTargeted)
        );
    }

    #[test]
    fn disallowed_event_takes_precedence_over_fork_check() {
        // pull_request_target from a real fork is refused as a disallowed event
        // (checked before the fork check).
        let mut ctx = trusted_job();
        ctx.event = "pull_request_target".to_string();
        ctx.head_repo = Some("attacker/demo".to_string());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::DisallowedEvent(
                "pull_request_target".to_string()
            ))
        );
    }

    #[test]
    fn actor_and_repo_matching_is_case_insensitive() {
        let mut ctx = trusted_job();
        ctx.actor = "OwNeR".to_string();
        ctx.event = "pull_request".to_string();
        ctx.head_repo = Some("OWNER/demo".to_string());
        assert_eq!(evaluate(&ctx, &allowlist_policy()), Verdict::Provision);
    }

    // --- bypass detection (ADR-0030) ---------------------------------------

    #[test]
    fn a_bypass_is_trust_unless_it_is_an_event_the_operator_could_allow() {
        assert_eq!(classify(&RefuseReason::PublicRepo), BypassClass::Trust);
        assert_eq!(classify(&RefuseReason::Fork), BypassClass::Trust);
        assert_eq!(
            classify(&RefuseReason::UntrustedActor("dependabot[bot]".into())),
            BypassClass::Trust
        );
        // Outside the ceiling: its provenance cannot be believed.
        for e in ["workflow_run", "pull_request_target", "issue_comment"] {
            assert_eq!(
                classify(&RefuseReason::DisallowedEvent(e.into())),
                BypassClass::Trust,
                "{e}"
            );
        }
        // Inside the ceiling: narrowed away, or opt-in and off.
        for e in MAX_EVENTS {
            assert_eq!(
                classify(&RefuseReason::DisallowedEvent(e.to_string())),
                BypassClass::Policy,
                "{e}"
            );
        }
    }

    #[test]
    fn trust_verdict_judges_a_job_that_already_ran() {
        // A job seen on our runner is in progress and may carry only a subset of
        // the profile's labels; `evaluate` would skip it twice over.
        let mut ctx = trusted_job();
        ctx.status = "in_progress".to_string();
        ctx.labels = vec!["self-hosted".to_string()];
        ctx.actor = "dependabot[bot]".to_string();
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Skip(SkipReason::NotTargeted)
        );
        assert_eq!(
            trust_verdict(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::UntrustedActor("dependabot[bot]".into()))
        );
        ctx.actor = "owner".to_string();
        assert_eq!(trust_verdict(&ctx, &policy()), Verdict::Provision);
        // And `evaluate` still refuses to judge a job that is not queued.
        ctx.labels = labels();
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Skip(SkipReason::NotQueued)
        );
    }

    #[test]
    fn on_bypass_defaults_to_record_and_is_reported_with_the_boundary() {
        assert_eq!(policy().on_bypass(), OnBypass::Record);
        assert_eq!(policy().boundary().on_bypass, OnBypass::Record);
        let enforcing = policy().with_on_bypass(OnBypass::Enforce);
        assert_eq!(enforcing.boundary().on_bypass, OnBypass::Enforce);
        // It changes nothing `evaluate` decides.
        let mut ctx = trusted_job();
        ctx.event = "workflow_run".into();
        assert_eq!(evaluate(&ctx, &enforcing), evaluate(&ctx, &policy()));
    }

    // --- whose code a job runs (ADR-0059 §3) --------------------------------

    #[test]
    fn a_dependabot_pr_a_maintainer_updated_is_refused_under_the_default_deny_list() {
        // The maintainer's push makes them the actor and the triggering
        // actor, but the code is still the denied bot's.
        let mut ctx = pr_job(vec![person("dependabot[bot]", false)]);
        ctx.actor = "maintainer".to_string();
        ctx.triggering_actor = Some("maintainer".to_string());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::UntrustedActor("dependabot[bot]".into()))
        );
        // A host that trusts Dependabot keeps its Dependabot CI.
        let trusting = TrustPolicy::new([], [], labels());
        assert_eq!(evaluate(&ctx, &trusting), Verdict::Provision);
    }

    #[test]
    fn the_actor_lists_judge_the_original_actor_of_a_rerun_too() {
        let mut ctx = trusted_job();
        ctx.actor = "dependabot[bot]".to_string();
        ctx.triggering_actor = Some("owner".to_string());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::UntrustedActor("dependabot[bot]".into()))
        );
        // An allow-list must hold every login the job runs on behalf of.
        let mut ctx = pr_job(vec![person("alice", false)]);
        ctx.triggering_actor = Some("owner".to_string());
        assert_eq!(
            evaluate(&ctx, &allowlist_policy()),
            Verdict::Refuse(RefuseReason::UntrustedActor("alice".into()))
        );
    }

    #[test]
    fn a_third_partys_fork_pr_is_refused_as_third_party_code() {
        // The fork check refuses it too; third-party code is the reason, so a
        // job-start refusal holds under `record` as well (ADR-0059 §5).
        let mut ctx = pr_job(vec![person("a-stranger", true)]);
        ctx.head_repo = Some("a-stranger/demo".to_string());
        ctx.private = false;
        let open = policy().with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), true);
        let verdict = evaluate(&ctx, &open);
        assert_eq!(
            verdict,
            Verdict::Refuse(RefuseReason::ThirdParty("a-stranger".into()))
        );
        let Verdict::Refuse(reason) = &verdict else {
            unreachable!()
        };
        assert_eq!(classify(reason), BypassClass::ThirdParty);
    }

    #[test]
    fn third_party_code_is_the_reason_whatever_else_a_job_fails() {
        // A public repo this host does not serve, with an event it narrowed
        // away: still third-party code first.
        let mut ctx = pr_job(vec![person("owner", false), person("a-stranger", true)]);
        ctx.private = false;
        assert_eq!(
            evaluate(&ctx, &push_only()),
            Verdict::Refuse(RefuseReason::ThirdParty("a-stranger".into()))
        );
    }

    #[test]
    fn a_merge_group_carrying_a_strangers_pr_is_refused() {
        let mut ctx = trusted_job();
        ctx.event = "merge_group".to_string();
        ctx.actor = "github-merge-queue[bot]".to_string();
        ctx.code = Code::People(vec![person("alice", false), person("a-stranger", true)]);
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::ThirdParty("a-stranger".into()))
        );
        // A group of collaborators' PRs runs.
        ctx.code = Code::People(vec![person("alice", false), person("owner", false)]);
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
    }

    #[test]
    fn a_destroyed_group_and_code_faber_cannot_read_are_refused() {
        let mut ctx = trusted_job();
        ctx.event = "merge_group".to_string();
        ctx.code = Code::DestroyedGroup;
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::DestroyedGroup)
        );
        ctx.code = Code::Unread("the pull request answered 404".into());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::Unread)
        );
        for reason in [RefuseReason::DestroyedGroup, RefuseReason::Unread] {
            assert_eq!(classify(&reason), BypassClass::ThirdParty);
        }
    }

    #[test]
    fn a_private_repo_whose_token_lacks_the_read_keeps_todays_verdict() {
        let mut ctx = pr_job(vec![]);
        ctx.code = Code::Ungranted(Permission::PullRequestsRead);
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
        // Today's verdict, so the actor lists still judge the actors.
        ctx.triggering_actor = Some("dependabot[bot]".to_string());
        assert_eq!(
            evaluate(&ctx, &policy()),
            Verdict::Refuse(RefuseReason::UntrustedActor("dependabot[bot]".into()))
        );
    }

    #[test]
    fn a_job_token_dispatch_is_refused_whatever_the_actor_lists_say() {
        let with_dispatches =
            policy().with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), false);
        let listed = TrustPolicy::new([JOB_TOKEN_ACTOR.to_string()], [], labels())
            .with_boundary(MAX_EVENTS.iter().map(|e| e.to_string()), false);
        for event in DISPATCH_EVENTS {
            for (actor, triggering) in [
                (JOB_TOKEN_ACTOR, None),
                ("owner", Some("GitHub-Actions[bot]")),
            ] {
                let mut ctx = trusted_job();
                ctx.event = event.to_string();
                ctx.actor = actor.to_string();
                ctx.triggering_actor = triggering.map(str::to_string);
                for p in [&with_dispatches, &listed] {
                    assert_eq!(
                        evaluate(&ctx, p),
                        Verdict::Refuse(RefuseReason::JobTokenDispatch),
                        "{event} by {actor}/{triggering:?}"
                    );
                }
                // Turned back on, the actor lists judge it as before.
                assert_eq!(
                    evaluate(&ctx, &listed.clone().with_job_token_dispatches(true)),
                    if triggering.is_some() {
                        Verdict::Refuse(RefuseReason::UntrustedActor("owner".into()))
                    } else {
                        Verdict::Provision
                    }
                );
            }
        }
        assert_eq!(
            classify(&RefuseReason::JobTokenDispatch),
            BypassClass::ThirdParty
        );
        // Only the dispatches: a merge group's run by the bot is not one.
        let mut ctx = trusted_job();
        ctx.event = "merge_group".to_string();
        ctx.actor = JOB_TOKEN_ACTOR.to_string();
        assert_eq!(evaluate(&ctx, &policy()), Verdict::Provision);
    }

    #[test]
    fn the_boundary_reports_job_token_dispatches_and_they_are_off_by_default() {
        assert!(!policy().boundary().allow_job_token_dispatches);
        let on = policy().with_job_token_dispatches(true);
        assert!(on.boundary().allow_job_token_dispatches);
    }

    #[test]
    fn every_class_but_policy_may_be_hostile() {
        assert!(BypassClass::Trust.may_be_hostile());
        assert!(BypassClass::ThirdParty.may_be_hostile());
        assert!(!BypassClass::Policy.may_be_hostile());
        assert_eq!(
            serde_json::to_value(BypassClass::ThirdParty).unwrap(),
            BypassClass::ThirdParty.as_str()
        );
    }
}
