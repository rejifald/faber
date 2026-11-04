---
title: The gate runs again when a job starts, and refuses third-party code
type: adr
status: proposed
date: 2026-11-01
owner: rejifald
---

# ADR-0059: The gate runs again when a job starts, and refuses third-party code

## Status

proposed (2026-11-01). The owner decided both rules on 2026-10-31, as decisions 1
and 2 of the public-release design review of 2026-10-31 (not published;
tracked in [#452](https://github.com/rejifald/faber-legacy/issues/452)). This records them
ahead of implementation, following the precedent of ADR-0047.

The code review's work plan implements it, as revised here. That plan first
described a design this ADR rejects (*Alternatives considered*): a policy shipped
into each runner and a hook that reads the event payload. This ADR is its
specification instead.

The owner settled the choices this ADR adds on 2026-11-01:

- the hook learns its verdict from faber, which reads GitHub's record of the job,
  while the hook only waits (§2);
- under `record`, only third-party code, a dispatch made with a job's own token,
  and, on a public repository, a job faber cannot verify by the deadline fail at
  start (§5);
- bots are judged by the actor lists, and organization members through GitHub's
  collaborator records (§3);
- a dispatch made with a job's own token is refused on every host, unless its
  operator turns such dispatches back on (§3);
- the minting pause ends for refusals the hook enforces, once *Validation* passes
  (§6).

Amends:

- [ADR-0030](0030-the-gate-is-defense-in-depth-and-a-bypass-is-detected.md): on a
  runner faber started, the gate now decides what executes from the job's first
  step, under `enforce` for every trust refusal and in both modes for third-party
  code (§3, §5), and not only what faber mints for. A refusal at job start is not a
  bypass. Detection stays, as the check that the hook did its job (§6).
- [ADR-0014](0014-the-trust-boundary-is-operator-configurable.md), *Public repos
  are opt-in, and opting in weakens nothing else*: that rested on a refused job
  never running, which ADR-0030 disproved. A public repo is supported where GitHub
  stops third parties from opening pull requests, and §3 is the backstop on
  faber's labels (§7).
- [ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md): the
  *Always* row gains Pull requests: read, plus whatever reading a merge queue needs
  (*Validation* measures it). On a token without them, faber keeps today's verdict
  on private repositories, warns at every boot and on the dashboard, and
  `faber auth` names the missing box.
- AGENTS.md, *A release must not withdraw a capability a host already relies on*:
  three narrowings hold in both modes, and join that section's list when they ship
  (*Why three narrowings hold under `record`*).

## Context

### A runner takes any job its labels cover, and ADR-0030 called that unpreventable

faber mints a JIT runner for a job its gate admitted, and GitHub hands the runner
any queued job its labels cover, refused jobs included. ADR-0030 found this running
on the builder and concluded that **faber cannot prevent this**: a runner is not
pinned to a job, `runs-on` belongs to the workflow, and runner groups restricted to
selected workflows exist only for organizations. Nor does `generate-jitconfig` take
a job. So the gate became defense in depth. faber detects a bypass after the fact,
and under `on_bypass = "enforce"` it kills the runner's container once the job has
started. On a macOS VM it cannot even do that.

### The job-started hook runs between assignment and the first step

ADR-0030 never weighed the runner's job-started hook. A self-hosted runner reads
`ACTIONS_RUNNER_HOOK_JOB_STARTED` from its own environment: the absolute path of a
script that the runner's administrator set. The runner runs that script once a job
is assigned to it and before the job runs, as the step *Set up runner*: after
*Set up job*, and before container setup, action pre-steps or any workflow step.

- It runs on the runner's host as the runner's user, never in a job container. Its
  parent is the runner's worker process. It has no timeout.
- A workflow cannot overwrite the `GITHUB_*` and `RUNNER_*` variables the script
  sees. By the runner's source, nothing a workflow sets reaches the decision to run
  it. The runner does apply its own directory's `.env` over its environment, though.
- A non-zero exit fails the step, and with it the job.

So faber can stop a refused job before any of its code runs. Three details of the
runner's behaviour shape how:

- **The exit code is not a stop.** By the runner's source, later steps whose
  condition is `always()` or `failure()` still run after a failed hook, and an
  action's pre-step defaults to `always()`. GitHub's docs imply that nothing runs;
  faber has not tested it. A stranger's workflow can mark every step `always()`.
- **The hook does not know its job's id.** It sees the run id and attempt, the
  repository and the runner's name, but no numeric job id. GitHub's job record
  carries the runner's name (`runner_name`) once the job leaves the queue. How soon
  is not documented, so *Validation* measures it.
- **A hook that cannot start fails open.** A `.js` hook runs on the runner's
  bundled Node, and a `.sh` hook under bash or sh from the runner's `PATH`. If the
  file or the interpreter is missing, the step fails with an exception, and the
  runner goes on as it does after any failed step.

Sources: GitHub's *Running scripts before or after a job*, and the `actions/runner`
source for `Program`, `JobExtension`, `JobHookProvider`, `ScriptHandler`,
`StepsRunner`, `ActionManifestManager` and `HostContext`, read on 2026-10-31.

### The gate judges who clicked, not who wrote the code

`trust_verdict` judges the event, the repository's visibility, the head repository,
and one login: the triggering actor, or else the actor. It never reads a pull
request's author.

- A maintainer who updates, re-opens or queues a denied Dependabot PR becomes that
  login, and the run is admitted.
- A merge-queue run's head repository is the base repository, so the fork check
  admits a group that carries a stranger's PR. So does a `push` run on the queue's
  temporary `gh-readonly-queue/…` branch.
- GitHub's `author_association` is no substitute. A member whose organization
  membership is private is reported to show as `CONTRIBUTOR` (apache/airflow
  PR 14718), and a `merge_group` event lists no PRs at all.

### What the owner decided

1. The job-started hook ships in phase 1. Each runner re-runs the gate on the job it
   received, and fails a refused job before its first step.
2. `allow_public_repos` stays opt-in, warned about, and "at your own risk". It is
   supported only where third parties cannot raise PRs, and faber refuses
   third-party PRs and third-party code. A third party is anyone who is not the
   repo's owner, a member or a collaborator. Their PRs, fork or not, and merge-queue
   runs carrying them are refused. Merged code counts as the repo's own.

## Decision

### 1. Every runner faber starts waits for faber's verdict, and stops itself otherwise

faber installs a job-started hook in every runner it starts, Docker and macOS alike.

- **The script sits where the job cannot write it before it runs:** a read-only
  volume that faber fills over stdin on Docker, and the read-only tools disk on
  macOS.
- **The variable is faber's.** On Docker it is set after the profile's `env`, so
  faber's value is the one the job sees. On macOS it joins the agent's fixed runner
  environment. A profile `env` that names it is refused where a human is looking
  (`faber check-config`, the settings save) and warned about at boot. The
  image contract forbids a runner `.env` that names it.
- **The hook waits for faber's verdict on this job.** It acts on an allow only when
  the allow's run id and attempt equal `GITHUB_RUN_ID` and `GITHUB_RUN_ATTEMPT`.
- **Anything else stops the runner, from inside.** On a refusal, a mismatch, a
  verdict it cannot read, a signal, an error of its own, or a deadline outcome of
  refuse, the hook prints one fixed reason line and kills its parent, the runner's
  worker, which runs as the same user. It does so on every exit path but an accepted
  allow. No later step can start, pre-steps and `always()` steps included, whether
  or not faber is reachable. faber's own stop (§4) is the cleanup.
- **The hook is JavaScript, run by the runner's own Node.** The runner carries Node
  for JavaScript actions and runs a `.js` hook with it, so the hook needs no shell
  from the image. A hook that never starts would fail its step and let later
  `always()` steps run, so faber checks that the hook file is in place before it
  starts a runner, and §2 catches a hook that is never seen waiting.
- **One fallback is decided at mint.** faber gives each runner a deadline outcome
  with the hook: allow on a private repository under `record`, since a JIT runner
  takes jobs of its own repository only, and refuse otherwise. At its deadline with
  no verdict, the hook applies it: an allow lets the job run and prints
  "unverified", and a refuse stops it with "no verdict". Beyond that one value, no
  gate policy is shipped into a runner, and the hook parses no event payload.

**On a host that grants dind, this holds only as far as the engine does.** An
earlier dind job is root in the Linux VM. It can replace the hook, forge a verdict
or fake faber's kill, and the engine keeps what it changed. That is ADR-0026's
Docker-API exception, which #107 and #108 close.

### 2. faber judges the job GitHub assigned, from GitHub's record

- **It notices the assignment.** A local signal starts the search: the runner's
  worker appearing in `docker top` on Docker, or in the agent's status on macOS. So
  does the runner's busy flag in GitHub's runner list, read conditionally so that an
  unchanged answer costs nothing.
- **It finds the job by the runner's name.** faber's runner names are unique
  (`faber-<job>-<time>-<instance>`). faber finds the job whose `runner_name` is that
  name: first among the jobs of the runs it minted for, then among the repository's
  queued and in-progress runs. Every listing it reads is paginated. A runner often takes a different job than the one it was minted
  for, and this is how faber finds it.
- **It judges with its own reads and its one gate.** faber reads that job's facts
  with its own token: the event, the head branch and head SHA, the repository and
  its visibility, the head repository, the actor and triggering actor, the pull
  request and its author, and a merge queue's entries. It judges them with the same
  `trust_verdict` that decides at mint time, with §3 added. Nothing the guest says
  is an input.
- **It delivers the verdict outward, and only to a hook it sees waiting.** On Docker,
  faber sees the hook's process in `docker top`, then writes the verdict into the
  container through the Docker API, atomically. On macOS it sends a new `verdict`
  request down the supervisor wire, the launcher's child link and the agent wire;
  the agent writes the verdict where the hook reads it, and confirms in its reply
  that a hook was waiting. The host still asks, the guest volunteers nothing, and
  `_faber` still parses no guest bytes (ADR-0025). A job in progress with no waiting
  hook is a hook fault: faber stops it under `enforce`, and under `record` whenever
  §5 refuses it at start.
- **Two deadlines.** faber gives up its search before the hook's deadline. Both come
  from config, and both outlast one poll interval plus a daemon restart. Runners
  adopted after a restart get their verdicts.

The mint-time verdict stays as the first filter: faber still mints only for jobs it
would run. The job-start verdict is the one that decides what executes.

### 3. Third-party code is refused

- **A third party is a person** who is not the repository's owner, a member of the
  organization that owns it, or a collaborator on it. faber reads this from GitHub's
  collaborator records with its own token, never from an event payload's
  `author_association`. GitHub lists the organization's owners, members who reach
  the repository through a team or the organization's base permission, and outside
  collaborators. A member with no access to the repository at all reads as a third
  party, which errs on the side of refusing.
- **Whose code a job runs:**

  | Event | Whose code it is |
  |---|---|
  | `pull_request` | the PR's author, whether the PR comes from a fork or not |
  | `merge_group` | the authors of the queue entry whose head commit is the run's head SHA, and of every entry ahead of it |
  | `push` to a branch under `gh-readonly-queue/` | the same as `merge_group`: it is a merge queue's temporary branch |
  | any other `push`, `workflow_dispatch`, `repository_dispatch`, `release` | the repository's own, since each takes write access, except a dispatch made with a job's own token, which is refused (below) |
  | `schedule` | the repository's own: it runs the default branch |
  | any other event | third-party code when its actor or triggering actor is a third party; these events are outside the ceiling, so such a job runs only by theft |

- **A merge queue is read by commit.** faber binds a merge-queue run to the live
  queue entry whose head commit equals the run's head SHA, and judges that entry and
  every entry ahead of it. If no live entry carries the SHA, faber checks whether the
  base branch now contains that commit. If it does, the group merged, and its code
  is the repository's own. If not, the group was destroyed (GitHub does not cancel
  such a group's runs), and the job is refused, at mint and at start. faber never
  resolves a group from its branch name or from the queue's current order.
- **Merged code is the repository's own.** A PR's code stops being its author's once
  someone with write access merges it.
- **A bot is not a person.** A GitHub App account, whose login ends in `[bot]`, is
  never a third party. The actor lists decide bots. They judge one login today; they
  now judge the actor, the triggering actor, and the author of every PR whose code
  the job runs, which for a merge group or its `gh-readonly-queue/` push is every
  entry the commit binding selects. faber's compiled default still denies
  `dependabot[bot]`, and a host that trusts it (`denied_actors = []`) keeps its
  Dependabot CI.
- **A dispatch made with a job's own token is refused**. Such a dispatch shows
  `github-actions[bot]` as its actor. It runs whatever code a job pushed with that
  token, under the dispatched workflow's labels and secrets, and job code is
  ADR-0025's adversary #1. So a `workflow_dispatch` or `repository_dispatch` whose
  actor or triggering actor is `github-actions[bot]` is refused on every host, at
  mint and at start, in both modes, whatever the actor lists say. An operator whose
  CI needs such dispatches turns them back on with an explicit `[gate]` key,
  `allow_job_token_dispatches`, which warns at every boot and on the dashboard while it
  is on.
- **What faber cannot read, it refuses,** with one exception. A pull request, a
  collaborator record or a merge queue's entries that faber cannot read refuse the
  job. On a private repository whose token lacks Pull requests: read, or the read
  that a merge queue or a commit's ancestry needs, faber keeps today's verdict for
  what it cannot read (ADR-0027, above): only people with access can open a PR
  there. A public repository is read without those grants.
- **It applies twice:** at mint time, so faber does not mint for third-party code,
  and at job start (§2).

### 4. A refusal stops the runner twice

The exit code is not trusted to stop a job (*Context*). On a refusal:

- the hook prints its reason line and kills the runner's worker (§1);
- faber stops the runner without asking it first. It kills the container on Docker,
  and stops the VM through the supervisor's `stop` on macOS, retrying until the
  runner is gone. ADR-0017 protects legitimate work, and this is not legitimate work
  (as in ADR-0030).
- faber records this stop, and a bypass stop, as a refusal ending, which ADR-0003's
  auto-retry never re-runs. Re-judging retries is part of the same work.

The reason line is one of a fixed set (event, fork, actor, author, public
repository, no verdict, unverified), because anyone who can read the repository
can read the job log. It names no host, path or rule text. GitHub may keep no log of
the killed step, since the runner uploads a step's log when the step ends, and it
may fail the job as a lost runner. So faber's own record carries the reason: the
refused-at-start entry, and the dashboard and boot warning, which count refusals per
repository. *Validation* records what GitHub shows.

faber does not cancel the run. GitHub cancels only whole runs, which would stop the
run's other jobs too, and cancelling needs Actions: write (ADR-0002, ADR-0027).

### 5. `record` and `enforce` at job start

| Refusal | `enforce` | `record` |
|---|---|---|
| Trust: fork, untrusted actor, public repo, event outside the ceiling | refused at start | allowed, and recorded as a bypass (ADR-0030) |
| Policy: an event the operator narrowed away, or an opt-in left off | allowed, and recorded | allowed, and recorded |
| Third-party code (§3) | refused at start | refused at start |
| A dispatch made with a job's own token, unless the operator turned such dispatches back on (§3) | refused at start | refused at start |
| No verdict by the deadline: faber or GitHub unreachable, or the job not found | refused at start | allowed on a private repository, and recorded as unverified; refused at start on a public one |

`on_bypass` keeps ADR-0030's meaning and its absent value, `record`. New installs
get `enforce` from the starter config, as the review settled.

### 6. What remains of ADR-0030

- **A job refused at start is not a bypass,** if faber saw its hook waiting, and
  either delivered the refusal to it or saw it still waiting at the deadline, and
  GitHub's record shows no step started after the hook's. faber records it as
  refused at start, naming the job, the runner and the job the runner was minted
  for. The repository is not held, its cache volume is kept, and the learner learns
  nothing from it. **Otherwise it is a bypass,** whatever the job's steps are called,
  since a workflow can name a step *Set up runner*: ADR-0030's kill, cache deletion
  and hold, plus a hook fault.
- **Detection stays, with a sharper meaning.** Under `enforce`, a trust bypass now
  means that a runner ran a trust-refused job's steps without faber's verdict: the
  hook is broken, missing or ignored. That is a fault of its own, loud on every
  surface. A policy refusal faber allowed at start is recorded as it is today.
- **The minting pause stays for now.** Under `enforce`, faber mints no runner that a
  queued trust-refused job could take (ADR-0030). That lets a stranger's PR stall an
  opted-in public repo's faber CI until it leaves the queue. Once
  *Validation* shows that the stop at job start holds on both backends, the pause
  ends for the refusals the hook enforces: a stolen job is then refused before its
  first step and leaves the queue.
- **A true bypass is still handled as ADR-0030 says:** the kill, the cache deletion
  and the hold.

ADR-0030's "The gate decides what faber *mints for*. It does not decide what
executes" no longer holds for runners faber starts: under `enforce` for every trust
refusal, and in both modes for third-party code. "faber cannot prevent this"
becomes: **faber cannot pin a runner to a job, but it stops before its first step
every job that §5 refuses at start.**

### 7. Public repositories

- `allow_public_repos` stays opt-in, and absent means `false`. Turning it on warns at
  every boot, on the dashboard and in the popover, and README calls it "at your own
  risk".
- **It is supported only on a public repository whose pull requests are off, or
  limited to collaborators.** GitHub has offered that setting for every repository
  since 2026-02-13. README and the boot warning say so. §3 is the backstop on
  faber's labels for when the setting is off or changes.
- `faber auth set` no longer says that faber never runs a public repository's jobs
 . It says what this host is set to.
- **What faber still cannot see:** a workflow that fetches an outsider's code under
  an event faber admits, such as a `push` workflow that checks out a fork; any job
  that downloads code while it runs; and an outsider's code moved onto a branch with
  a job token elsewhere, then dispatched on a host whose operator turned such
  dispatches back on.
  ADR-0030's invariant still holds: every job that can target faber's labels must be
  one you would let run here.

## Why three narrowings hold under `record`

AGENTS.md forbids a release to withdraw a capability a host relies on. Its two
exceptions shipped while hosts were updated by hand, and it calls neither a
precedent. These ship the same way, since this work blocks the cut, and they join
that list for these reasons.

**Third-party code is refused whatever `on_bypass` says (§3), and so is a public
repository's job that faber cannot verify by the deadline (§5).**

- A third party's PR always comes from a fork, because a third party cannot push to
  the repository. The fork check has refused every fork PR at mint time since
  ADR-0014, and ADR-0014 rested the public-repo opt-in on it. Such a job ran only by
  theft, which ADR-0030 shows faber never meant to allow.
- The cases the fork check misses, a merge-queue run and a merge queue's `push` run
  carrying a third party's PR, run a stranger's code that the owner has decided
  never runs on faber. The events outside the ceiling run only by theft.
- On a public repository, a job faber cannot verify might be a stranger's, so the
  deadline outcome there is refuse. It fires only when faber or GitHub cannot answer
  in time.

**The actor lists also judge a PR's author and the original actor.** On a host
that keeps the default deny list, faber no longer mints for a Dependabot PR that a
maintainer touched. That run was the deny list's own target, admitted by mistake.

**A dispatch made with a job's own token is refused on every host (§3).** It runs
code that a job, possibly a hostile one, pushed with its token, under the dispatched
workflow's labels and secrets. Workflows that dispatch faber-labelled
workflows with `GITHUB_TOKEN` stop running on faber until the operator turns such
dispatches back on with the `[gate]` key. The owner chose this over a warning on
existing hosts (2026-11-01).

All three are loud on faber: each refusal is recorded with its reason and counted on
the dashboard, the boot warning names the rules, and the release notes say so.

## Consequences

- **Every job start waits for one verdict:** a GitHub lookup or two, then the
  delivery. *Validation* measures how long.
- **A job start costs a few API calls,** conditional and paginated.
- **A GitHub outage or a rate limit on faber's own token fails jobs at start:** every
  job under `enforce`, and public repositories' jobs under `record`. A rate limit
  does not stop the runner, which uses its own credential, so the API budget comes
  first.
- **The runner image contract gains two requirements:** a runner that honours
  `ACTIONS_RUNNER_HOOK_JOB_STARTED` and runs a `.js` hook on its bundled Node, and no
  `.env` entry naming the variable. A runner that ignores the variable runs jobs
  unguarded until faber's stop: faber treats a job it never saw a hook waiting for
  as a hook fault, and as a bypass when it refused that job.
- **macOS: three protocol versions change:** supervisor protocol 4, child protocol 3
  and agent protocol 4, for the `verdict` request and the agent's worker status. A
  VM adopted from an older version has no hook and is left to detection.
- **Outsiders' PRs on an opted-in public repo are refused at mint and wait in the
  queue.** Until the pause ends (§6), each one pauses minting for its repo's
  matching labels under `enforce`. README's routing snippet routes on the PR's
  author with an allow-list, so such jobs go to other runners and never reach
  faber.
- **Workflows that dispatch faber-labelled workflows with a job's own token stop
  running on faber** until the operator turns such dispatches back on (§3).
- **A third party's PR in a merge queue whose checks run on faber cannot merge**
  until someone with write access takes its code onto a branch of their own.

## Alternatives considered

**The hook judges, from a policy faber ships in.** This was the code review's plan.
Rejected:

- It puts a second copy of the gate in the hook's language, which can drift from
  `trust_verdict`.
- The event payload is reported to show private members as `CONTRIBUTOR`, and lists
  no PRs for a merge group, so §3 would need lists computed ahead of time and
  shipped in.
- faber would learn of a refusal only afterwards, and Docker has no path back for
  one.

**The hook asks the daemon.** That needs a path on which the guest sends requests to
the host. Every VM link today is dialed by the host, and the wall drops guest
traffic to the host. The first such path is the one a later ADR
plans for a credential broker, whose relays accept only the host's connections and connect to nothing. The job-start verdict
does not need one.

**Exit non-zero and stop there.** By the runner's source, later steps whose condition
is `always()` still run, and an action's pre-step defaults to it.

**Cancel the run.** GitHub cancels whole runs only, and cancelling needs Actions:
write.

**Trust `author_association` from the payload.** It is reported to show private
organization members as `CONTRIBUTOR`.

**Require `enforce` for public repositories at load.** Refusing to boot on a config
the release did not write leaves a dead daemon (AGENTS.md). The rule in §3 gives the
safety without that.

**Pin a runner to its job.** GitHub offers no way to (ADR-0030).

## Validation

It stays `proposed` until these are measured on the builder, on Docker and on a
macOS VM:

| Check | Pass |
|---|---|
| A refused hook, then steps marked `if: always()`, and an action with a `pre:` and no `pre-if` | no step of the job runs |
| A refusal with the daemon stopped | the hook stops the runner at its deadline, and no step runs |
| The hook file missing; the hook failing with an error of its own | faber does not start the runner; the hook kills its parent, and no step runs |
| What GitHub shows after a refusal | the job's conclusion and log recorded |
| A refusal at start, and a runner that ignores the hook | faber records refused at start for the first, and a hook fault for the second |
| A job taken by a runner minted for another job | judged at start as the job it is, found by `runner_name` |
| A job past the 100th in its run | found |
| A stranger's fork PR, and a merge group carrying it, on a test repository | both refused before their first step |
| A merge group of collaborators' PRs | allowed at start, bound by its head SHA |
| Merge-group and `gh-readonly-queue/` push jobs that start after their group merged | allowed |
| A merge group behind a stranger's PR that left the queue | refused |
| An unfiltered `push` workflow while a stranger's PR is queued | its `gh-readonly-queue/` run is refused |
| A public repository's pull request, read with a token that lacks Pull requests: read | read |
| The permission that reading a merge queue needs | named, and added to ADR-0027's table |
| Time from *Set up runner* to the hook's exit | recorded: p50 and p95 over a day of jobs |
| Time until `runner_name` appears in GitHub's job record | recorded |
| API calls per job start | recorded |
