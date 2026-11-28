---
title: A check runs before the push, and faber records the tree it passed
type: adr
status: proposed
date: 2026-11-27
owner: rejifald
---

# ADR-0067: A check runs before the push, and faber records the tree it passed

## Status

proposed (2026-11-27). Recorded ahead of implementation, following the precedent of
ADR-0007, ADR-0010 and ADR-0046.

The owner set the direction and then settled the points below in a session on
2026-11-27:

- The value of running a pre-push check through faber is that faber gives it a container
  sized for the task, under admission. The same work must not then run again in CI, and
  what says so is tied to the tree that was checked, not to a label the pusher sets.
- The grain of a check is the repository's choice. faber records that a named check
  passed and knows nothing of CI jobs or steps (§2).
- A check's definition supplies defaults, a caller may override them per invocation, and
  a run with no definition at all is allowed. Neither is ever posted (§2, §8).
- Every check learns a limiter class of its own (§6).
- A local job ranks with its repo's other claims, two at a time unless set (§6, §11).
- It gets a cache of its own, not the repo's shared one (§5).
- The base a caller names is part of what is recorded and posted (§8).
- A pass is posted only from an engine that has not granted dind, while it is fresh, and
  while its image is unchanged (§8).
- The caller tells faber when the commit is pushed, and discovery is the fallback (§9).
- The first caller runs its slow gates through faber and keeps its cheap ones in the
  pre-push hook. A whole gate, as a sweep runs it, is one more named check (§2, §13).
- v1 has no token, no secrets, no LFS content, no dind and no macOS VM checks (§5).
- On 2026-11-28: which tree a workflow compares, the pull request head's or the merge
  result's, is the repository's choice (§10).
- On 2026-11-28: a check shows wherever jobs show, marked as local, with no view of its
  own. The dashboard says how it ended and what became of a pass. The popover draws it
  as it draws a job (§12).

Revised the same day after two independent checks of its statements against `31c952c`.
The first found:

- that reconciliation asks GitHub about every claim it minted for and reclaims one
  GitHub does not know, so a local claim needs more than an id of its own (§6);
- that ADR-0005 defends the shared cache volume only for code from a non-fork ref, which
  a local job's code is not (§5);
- that a dind job can plant a `config` or a hook in any volume, so the source store is
  cleaned before git runs in it, as the mirror is (§4);
- that the control API has no body limit of its own and feeds stdin from memory, so a
  bundle needs new streaming code (§3);
- that a runner is not pinned to the job it was minted for, so a status posted at
  discovery can arrive after the job that reads it has started (§9);
- five statements that were imprecise about ADR-0024, ADR-0028, ADR-0063, the mirror's
  report parser and where the "no credential in a VM" rule stands.

The second, after the owner's decisions were written in, found:

- that adoption today lists only containers of the runner kind, by a name prefix and a
  run-id label, none of which a local job has (§5);
- that two views show the last segment of a class key as the job's name, so the
  override class is named to keep the check's name last (§6);
- that ADR-0055 already bounds a cache volume by size, which the local cache now takes
  too (§5);
- three statements about the gate that claimed more than it does (§3), and one that
  cited ADR-0046 for something it does not say.

A third check, of §12 on 2026-11-28 against `31c952c`, found:

- that the Jobs view names a class from the outcome record's label and not from its
  key, so §7's record needs that label;
- that the first slice was said to record nothing while its Jobs class needs the
  outcome record;
- that the row for a host-wide cause did not fit the proposed hints ADR as it then
  read, in its figure and in sharing a cause with that ADR's first hint (that ADR has
  since taken up both, and §12 follows it);
- three statements that were imprecise about ADR-0056's phases, the Memory view and
  what specifies the dashboard.

It amends:

- [ADR-0028](0028-the-control-api-knows-its-caller.md): the control socket gains four
  routes (§3), and turning the feature on is a trust widening (§11).
- [ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md)'s permission
  table gains a row (§9).
- [ADR-0044](0044-a-finished-run-shows-ci-s-verdict-only-as-github-reported-it.md): a
  local job has no GitHub conclusion, so its verdict is the one faber saw (§7).
- AGENTS.md, *What faber may delete*: the source store (§4) and the local cache (§5)
  join it, on a licence of their own.

## Context

### A check before the push runs outside everything faber does for jobs

A repository's own tooling runs checks before a branch is pushed: a pre-push hook, or a
stage that reviews and fixes a change before its pull request opens. Those checks run on
the operator's desktop, as the operator.

- **Nothing admits them.** Several agent sessions can each start one at once. faber's
  limiter does not see them, so nothing holds the second one back until the first has
  finished, and a machine that is already busy is asked for more.
- **They are not the CI job.** They run on macOS against the operator's toolchain. The
  job they stand in for runs on Linux in the runner image.
- **They cannot use a declared service.** The shared Postgres and the per-job services
  of [ADR-0046](0046-jobs-declare-services-and-faber-runs-them-without-dind.md) sit on
  networks inside the Linux VM. [ADR-0026](0026-job-code-runs-only-inside-vms-faber-owns.md)
  gives that VM no port forwards, and the bridged Docker socket is `_faber`'s alone. A
  check that needs a database starts its own, in whatever Docker the operator keeps.

On a watched repo, roughly a third of pull requests opened with a first run that was red
on steps needing no secret and nothing from GitHub (one window of about ten days). Each
of those could have been caught before the pull request existed.

### Running the check in faber is half of it

faber can run that check as a job: in the runner image, on a private network, with the
services it declares, the npm proxy and a cache, admitted against the same budget as
every other job. That removes the three problems above.

It also spends the pool. A check that passes locally and then runs again as the pull
request's first CI run has cost the host twice. So the second half is a way for CI to
learn that the work is done.

### What can say "it already ran"

A `runs-on` label cannot: a label carries no value (ADR-0046 §1), and whoever writes the
workflow writes the label. A pull request label is the author's claim. The statement has
to name the exact content that was checked, so that it cannot be carried over to
different content.

A git tree id does that. Two commits with the same tree have the same files, whatever
their messages, parents or dates.

## Decision

### 1. One command gives two things: a sized run, and a vouched check

`faber check` runs a command on a commit, inside faber.

- **A sized run** is any such run. It gets the container, admission, a learned cap, the
  declared services and a cache. Its result goes to the caller. This is the part that
  works for every caller and every command.
- **A vouched check** is a sized run that was exactly what the tree defines. faber
  records it, and may post it to GitHub so that CI can skip the same work.

Everything that limits posting (§8) is a condition on the second. None of it restricts
the first.

### 2. The tree defines a check; the caller may override it, or run without one

A repository lists its checks in `.faber/checks.toml`:

```toml
[check.verify]
labels = ["faber", "docker", "faber.postgres-17", "faber.redis"]
run = ["ci/verify.sh"]
timeout_minutes = 20
```

- **`labels`** are what a workflow job puts in `runs-on`. faber matches them to a
  profile and judges them exactly as it judges a workflow job's: a need or a service the
  host does not provide is refused (ADR-0010, ADR-0046).
- **`run`** is an argv, executed in the checked-out tree.
- **`timeout_minutes`** may lower the host's limit (§11) and never raises it.

**The grain is the repository's.** A check may stand for a whole CI job, for some of its
steps, or for a whole gate that a sweep runs. faber does not know which. A repository
defines as many checks as it has things worth running alone.

**The definition is part of the tree it describes.** A change that weakens a check
changes the tree, and shows in the pull request's diff like a workflow edit does.

It is a file of its own and not a table in `.faber/config.toml`. `RepoConfig` rejects
unknown tables, and a file faber cannot parse falls back to the host's drift policy. A
new table there would silently change drift behaviour on every host that runs an older
release.

**A caller may override.** `faber check verify` takes the definition as its defaults,
and flags replace `run`, `labels`, environment variables or the timeout for that one
invocation. An agent fixing a failure can rerun one test in the same container the check
gets.

**A caller may run with no definition.** Given labels and a command, `faber check` runs
them in a repo that has no `.faber/checks.toml`. The CLI says that such a run is not
recorded as a check.

**A caller picks which checks to run, never part of one under its name.** That is what
the two rules above protect: an overridden run and a run with no definition are sized
runs, and are never vouched (§8).

A check's name is a lowercase label of letters, digits and hyphens. It appears in a
status context and in a limiter class, and `adhoc` is reserved.

It checks a commit, `HEAD` unless one is named, never a working tree.

### 3. The request comes over the control socket, from the operator

Four routes, mounted on the socket only, as `POST /api/session` is. A page in the
dashboard cannot reach them.

- `POST /api/check` takes the repo, the commit, an optional base, the check's name or
  the overriding fields, and a git bundle as its body. It answers once the tree is
  prepared (§4), with the job's id and the tree id. The body is new ground: no route
  sets a body limit today, so the framework's default of about 2 MB applies, and the
  daemon feeds a container's stdin from a buffer in memory. This route sets its own
  limit (`max_bundle_mib`, §11) and streams the body through.
- `GET /api/check/{id}` returns the job's state, its exit status once it has one, and
  the log from a byte offset. The CLI polls it. The control API has no streaming
  response today, and polling also means a daemon restart is only a pause for the
  caller.
- `POST /api/check/{id}/cancel`.
- `POST /api/check/{id}/publish` (§9).

The caller is the operator's account, by the socket's peer uid. No presence is asked for
a check: an unattended agent session has to be able to run one, which is the point.

**The repo is the caller's word.** It must be one a profile serves, and a public repo
is refused wherever the gate would refuse it. faber does not verify that the commit
belongs to that repo; it uses the name to pick the class, the source store and the local
cache. No gate judges the code: the gate judges a workflow run, by its event, its
actors, and its pull request's author and provenance, and a local job has no run. The
caller's account is all faber has to go on, which is why turning the feature on asks for
the operator's presence (§11), as widening the gate does.

**Overrides add no reach.** A caller can already commit any command into
`.faber/checks.toml` in a local commit and run it, because the definition comes from the
tree and no gate judges it.

### 4. The source arrives as a bundle, and a step that runs only git prepares it

The daemon runs as its own account (ADR-0025) and takes its input over the socket, and
the VM has no shared folders (ADR-0026). The owner has decided that no GitHub credential
should enter a VM ([git-mirror.md](../git-mirror.md#not-beside-dind)); the mirrors do
not meet that yet, and a local job meets it from the start. So the CLI sends the
commits:

- **A git bundle on the request body.** The daemon pipes it to a container's stdin, as
  it pipes a hook script or a credential today, and never writes it to its own disk.
- **Thin against a per-repo source store.** faber keeps one volume per repo, labelled
  `org.faber.kind=local-source`, holding the objects of earlier bundles. The daemon
  remembers, in its own state directory, which commits it stored. The CLI excludes those
  it also has, so a second run in a repo sends only new commits. The first sends its
  whole history, once.
- **It does not use the git mirror.** ADR-0035's mirror would serve, but it is filled
  with faber's token, and it is off on any engine that has granted dind
  ([git-mirror.md](../git-mirror.md#not-beside-dind)). That rule protects the token. The
  source store holds none, so it may exist beside dind.

The prepare step is a short-lived container from the profile's runner image, with
faber's own entrypoint, no network, and git's system and user configuration ignored.
Before git runs in the store, the step rewrites the store's `config` and deletes its
`hooks/`, as a mirror refresh does, so nothing left in the volume steers it. It then
fetches the bundle into the store and checks the commit out into a per-job workspace
volume. When the caller named a base, the workspace gets one remote-tracking ref for it,
so a check that asks git what changed gets an answer.

**faber, not the caller, establishes what is checked.** The daemon reads two things from
the prepare step, each under a size limit, as a mirror refresh's output is capped:

- the commit's tree id, accepted only as forty hex digits;
- the bytes of `.faber/checks.toml` at that commit, parsed with a schema that rejects
  unknown keys, as `RepoConfig` is.

No code from the tree has run at that point. For a named check with no override, the
command that runs is the one the tree defines, and the tree id is one git computed from
verified objects.

The store is read by its labels, never its name, and is not one of the janitor's per-job
kinds. faber may delete it: it is a transfer cache, every object in it came from a clone
or from GitHub, and losing it costs one complete bundle. No existing limit covers a
volume of this kind, so faber deletes a store that no run has used for
`store_max_age_days` (§11).

### 5. The job is a container like a runner's, without a runner

- **The image** is the profile's runner image, with the entrypoint overridden. The
  command is the container's main process, running as the image's job user, so its exit
  status is the container's and its output is the container's log.
- **Around it, what a runner gets:** a private network (ADR-0020), the declared services
  and their variables (ADR-0046), and the proxy mirror's variables when the profile has
  `mirror` on and the mirror is healthy (ADR-0047).
- **A cache of its own, not the repo's cache volume.** ADR-0005 defends the shared
  volume because everything that writes it is code from a non-fork ref of a private
  repo, and says the decision has to be revisited if that stops being true. ADR-0028
  names the volume as the one state that crosses the job boundary: what one job seeds, a
  later job that holds secrets reads. A local job's code is on no ref at all. So where
  the profile has `cache_volume` on, a local job mounts a second per-repo volume,
  labelled `org.faber.kind=local-cache`, that only local jobs mount. It is writable and
  is given under the same `FABER_CACHE_DIR`, so a repository's cache scripts work
  unchanged. It is bounded as ADR-0055 bounds a cache volume, by the host's
  `cache_max_gib` and `cache_max_age_days`.
- **The variables it gets** are `CI=true`, `FABER_CHECK` when the run is a named check,
  and `FABER_BASE_SHA` when the caller named a base. With no base, a check is expected
  to do all of its work.

**v1 limits.** Each is refused with its reason, not ignored:

- **No GitHub token, no repository secret, no `GITHUB_*` variable, no JIT config.** A
  step that needs one stays in CI.
- **No Git LFS content.** The workspace holds pointer files.
- **No dind.** A definition or an override whose labels ask for it is refused.
- **Docker profiles only.** Labels that match a macOS VM profile are refused.

Its containers and volumes are faber's, labelled with a job id, and `sweep` reaps a
leaked one by label like any other per-job resource.

**A daemon restart adopts a running local job, which is new code.** Adoption (ADR-0011)
lists only containers of the runner kind today, and expects a runner's name prefix and a
run-id label. A local job has a kind of its own and no run id. So adoption gains a
second listing for that kind, with its own required labels, which carry what §8 needs to
record the run. The outcome is then not lost to a restart.

### 6. Admission and sizing are the limiter's, with a class of its own

A local job is a claim. It parks and is admitted against the same memory, CPU and disk
budgets, shows why it is parked (ADR-0022), is cancelled by a drain while parked and
waited for once admitted.

- **Every check learns its own class.** The limiter keys a class by repo, GitHub
  workflow id and job name, and a local job has no workflow id. So:
  - a check as defined is `{repo}::local::{check}`;
  - an overridden run of it is `{repo}::local-override::{check}`, so an odd command
    never distorts what the check has learned;
  - a run with no definition is `{repo}::local::adhoc`.

  The check's name stays the last segment in each, because two views show a key's last
  segment as the job's name.

  A new class starts where any new class starts, at the profiling ceiling, and is sized
  from its real peak after that. A check does not borrow a CI job's cap, whatever its
  grain.
- **It takes the tier its repo's claims take:** priority where `priority_repos`
  (ADR-0036) names the repo, normal otherwise. It is the work a pull request would have
  queued minutes later, so it gets the same place. Waste ranking (ADR-0063) does not
  apply: it judges only `pull_request` runs that have a pull request number.
- **Two host limits** keep a loop of requests from starving CI: how many local jobs run
  at once (two unless set), and how many may wait. A request past the second is refused,
  not queued.

**A local claim is not in the map reconciliation walks.** That map is keyed by GitHub's
job id, and each cycle asks GitHub about every claim in it that faber minted for, and
reclaims one whose job GitHub does not know. A local claim with a made-up id would be
reclaimed on the next cycle. The existing way to mark a claim faber did not mint for
also changes when a claim counts as started, so it is not the exemption to borrow. Local
claims are held apart, under ids faber mints, and reach the limiter through the same
admission call.

### 7. The result is an exit status, a log and an audit record

- The CLI exits with the command's status. A fault that was faber's (the container was
  lost, a service failed to start, the daemon cancelled it) has its own exit code, so a
  caller can tell "the check failed" from "the check did not run".
- The log is job output. The daemon relays bytes and reads none of them (ADR-0025). The
  CLI passes it through `term.rs`'s `sanitize` before printing, because job output on
  the operator's terminal must not carry control sequences.
- The audit trail gets an outcome record with the check's name, the tree id, the base
  and what was overridden.
- **ADR-0044 says a verdict is shown only as GitHub reported it.** A local job has no
  GitHub job. Its verdict is the container's exit status, shown as faber's own and drawn
  apart from a CI verdict.

### 8. What makes a run a vouched check

faber writes a pass to a file in its state directory, which no job can reach, when all
of these hold:

- the run named a check that the tree defines;
- nothing overrode `run`, `labels` or the environment (a different timeout alone does
  not count, since the same command ran);
- it exited 0.

The record is `(repo, check, tree id, base, image id, time)`.

- **The base is part of it.** A check that asks git what changed does different work for
  a different base. A caller who named the commit itself as the base would have run
  nothing and passed. So the record keeps the base commit, or notes that there was none.
  A run with no base is a full run, and satisfies a reader whatever base it has.
- **The base is the only caller input that may change what a vouched check does.** Any
  other input that did would have to join the record first.

A failed run writes no record and posts nothing.

A record is **postable** only while all of these hold:

- **The engine has not granted dind.** That is the test the mirrors already use: no
  docker profile grants dind, and the engine carries no `engine-dind.json` marker. A job
  given dind is root in the Linux VM (ADR-0013, ADR-0026), where the workspace, the
  source store and a running local job all live, and could make a check exit 0. On such
  an engine the check still runs and the caller still gets its result. The record is
  kept and shown as not posted.
- **It is younger than `attest_ttl_hours`** (24 unless set), because a service version
  or the calendar can change what the same tree does.
- **Its image is unchanged.** faber posts only if the image id that ran the check is
  still the id of the image it would start that check in now.
- **`attest` is on** (§11).

### 9. A pass is posted when the caller says the commit is pushed

GitHub does not know the commit when the check passes, so the status cannot be posted
then.

- **The caller announces the push.** After pushing, it runs `faber check publish <id>`.
  faber posts a commit status on the commit it checked: state `success`, context
  `faber/check/<name>`, and a description naming the tree id and the base, or `full`.
  A status needs only the commit, not a pull request or a run, so a caller that pushes,
  publishes and then opens its pull request has the status in place before any run can
  start, on any host.
- **Discovery is the fallback.** When discovery lists a run whose head commit faber has
  not looked up, and the repo has a postable record, faber reads the commit's tree id
  from GitHub's git data API and posts on a match. This also covers a later commit with
  the same tree, such as one that only reworded a message. It is best effort: a runner
  is not pinned to the job it was minted for, and a second host knows nothing of the
  record, so a job can start before this post exists. That job finds no status and the
  work runs.

Posting needs Contents: read and Commit statuses: write. ADR-0027's table gains the row,
and the token's required set gains both while `attest` is on. Statuses write cannot be
probed, so a refusal is learned from the first post, as it is for drift and for the
waste status today.

A status already on a commit stays there. A workflow that cares how old it is reads its
date.

### 10. Skipping is the workflow's, and it compares the tree and the base

faber cannot finish a GitHub job without a runner running its steps, and does not know
what a check stands for. So the repository skips, in whichever shape fits the check:

- **Some steps of a job.** The job's first steps check out, read the head commit's
  `faber/check/<name>` status and compare; the covered steps carry an `if:`. No extra
  job and no extra runner.
- **A whole job.** A small first job does the same and sets an output that the heavy
  job's `if:` reads.

The comparison is the same in both:

1. the tree id in the status equals the tree the repository chose to compare;
2. the status says `full`, or its base equals the base this run would use.

**Which tree is the repository's choice** (the owner, 2026-11-28):

- **The pull request head's tree.** The status then answers "were this change's own
  files checked". The skip survives the base branch moving after the check. It says
  nothing about the merge result, which fits a repository that verifies integration
  somewhere else, such as a scheduled run on the integration branch.
- **The tree the run checked out.** A `pull_request` run tests the merge of the branch
  into its base, and that tree equals the checked tree only when the branch already
  contains the base's tip. Otherwise the work runs, because the merge result was never
  checked. This is the strict reading, and on a repository whose base moves often the
  skip is rare.

The status carries the tree id and the base, which is what either comparison needs.

faber ships no action for this. The steps are a few lines of shell in the repo's own
workflow, and [docs/operating.md](../operating.md) will carry an example of each shape.

**The check and the CI steps must be the same work.** The record says that one argv
exited 0 on one tree, against one base, in one image, with the declared services. It
means "CI would pass" only if the workflow runs the same script under the same labels.
faber cannot enforce that. Keeping both to one script that the workflow calls is the
repo's side of the bargain.

### 11. It ships off, and turning it on is a widening

```toml
[local]
enabled = true            # absent or false: no local jobs, as today
max_admitted = 2          # running at once
max_parked = 8            # a request past this is refused
max_bundle_mib = 2048
store_max_age_days = 14   # a source store unused this long is deleted
attest = true             # false keeps the record and posts nothing
attest_ttl_hours = 24
```

- The values shown are the defaults, except `enabled`, which is off when absent.
- A host without `[local]` is unchanged, and `faber check` there says the host does not
  offer it. This turns a feature on and tightens nothing, so AGENTS.md's three-state
  shape is not needed.
- **The time limit is the one runners have.** `runner_timeout_secs`, which caps a
  runner's whole lifetime, applies to a local job in the same way. A whole-gate check
  can need most of it. A definition or a caller may lower it. On a host that set it to
  0, which means no limit, a check's own timeout is the only one.
- **Enabling it is a trust widening** (ADR-0028). That ADR's test is whether a change
  widens what runs next to a privileged container, a Docker socket or a secret. This
  one does: code that no gate judged joins the engine, and on a host with
  `[services.postgres]` it connects to the server that jobs holding secrets also use.
  It is also exactly what phase 2 exists for, a process running as the operator that
  wants code run. Changing the limits is routine.
- It applies to the host it is set on. A follower that updates gains nothing until its
  operator writes the table.

### 12. A check shows wherever jobs show, marked as local

The four routes of §3 are on the socket only. Without more, only the terminal that
started a check would know it exists, while it holds budget a CI job is parked for. So
(the owner, 2026-11-28):

- **No view of its own.** A local job joins the surfaces that already show jobs.
  [ADR-0054](0054-the-dashboard-is-views-in-a-sidebar.md) has a new section join a view
  and a new view change that ADR. This adds neither.
- **The state snapshot carries local jobs,** in flight and recently finished, each
  marked as local, with its repo, its check's name, its tree id, its phase and why it is
  parked. An in-flight entry is identified today by its GitHub job id, which a local job
  does not have, so it carries the check's id (§3) instead. Whether local jobs sit in
  the lists the snapshot has or in lists of their own is the build's choice.
- **In the popover it is a job group.** A group is one pull request, or a repo and a
  workflow when there is none ([popover.md](../design/popover.md)). A local job has
  neither, so its group is its repo and its check: `demo | check verify`, and
  `demo | check adhoc` for a run with no definition. Its pips are a job's: waiting,
  running, frozen. A check that ran to its end draws the done pip whether it passed or
  failed, and the red ring stays what it is for a job, a fault of faber's own (§7). The
  popover shows no pass or fail, as it shows none for CI: the caller has the verdict in
  the terminal.
- **In the dashboard it is a row in Activity and a class in Jobs.** Activity lists a
  finished check as an event of a kind of its own, so the Kind facet filters on it. The
  row says how it ended: passed, failed, or did not run, which is faber's fault. Its
  detail names the check, the tree id, the base or `full`, and what was overridden. The
  Jobs view ranks classes from the audit trail's outcome records
  ([ADR-0056](0056-a-run-records-where-its-time-and-resources-went.md) § 5), so §7's
  record must be one it reads. The view names a class from the record's label, not from
  its key, so the record carries a label in the shape the view reads, with the check's
  name where a job's name stands. The class is marked as local. ADR-0056 clocks a run's
  `job` phase from GitHub's timestamps and splits it by GitHub's steps. A check has
  neither, so faber clocks its command's run itself and the phase is not split. Memory
  shows the class's learned cap as any class's, and a parked check counts in its tally
  of what parked jobs.
- **A passed row says what became of the pass,** in one line: posted, with the commit;
  recorded and not published yet; not recorded, because the run was overridden or had no
  definition; or cannot be posted, with the condition of §8 that failed. For the first
  of these faber keeps, beside the record, the commit it posted to. This line is how
  "why did CI repeat the work?" is answered after the terminal is gone.
- **A cause that holds for the whole host is said once.** On an engine that has granted
  dind no pass is ever posted. Repeating that on every row is noise, and an alarm would
  be wrong, since nothing is broken. It is a hint under the proposed ADR
  `faber-says-when-a-faster-way-to-run-jobs-is-off` (rejifald/faber#115, not merged):
  neutral, on the dashboard only, with no button, never delivered outward, and one an
  operator can dismiss. It shows only while the host has passes it could not post. Its
  figure is a count, the passes of the last 7 days that could not be posted, which that
  ADR's §3 allows. The same grant already has a row there, the git mirror held off
  beside dind, which names the way back. That ADR's §8 has hints that share a cause and
  a way back share a row (the owner, 2026-11-28), so this is a line on the grant's row,
  and a row of its own only where nothing else is held off. It is built only once both
  ADRs are accepted.

The popover's content is frozen by `popover.md`: that document changes before the
popover does. The dashboard's views are specified by the anatomy under
[docs/design/dashboard](../design/dashboard/screens.md), which the change that builds
the rows updates. Both are reviewed against the design guidelines, like any change to
the UI.

### 13. It is built in three slices

1. **The sized run.** The routes, the bundle and the prepare step, the container, the
   claim path and its classes, the CLI. No pass is recorded or posted (§8). This alone
   gives a caller the container and the admission. With no definition yet, every run is
   ad hoc and learns the `adhoc` class. It includes §7's outcome record, which the Jobs
   view reads, the local jobs in the state snapshot, the popover group, and the Activity
   row and Jobs class with how the run ended (§12): a check is never running where the
   operator cannot see it.
2. **The vouched check.** The definition, the record, the conditions of §8, `publish`
   and the fallback, the permission row, and the line that says what became of a pass
   (§12).
3. **The first repository adopts it.** Its workflow gains the comparison of §10, its
   pre-push tooling calls `faber check`, and the rows of *Validation* are measured.

## Consequences

- **A pre-push check gets what a CI job gets:** the runner image, admission, a learned
  cap, declared services and a cache. The operator's desktop stops being where it runs.
- **A check is as visible as a job.** It shows in the popover and the dashboard from
  the first slice, which also means the popover opens to its two-column width for a
  check as it does for a job.
- **The pool pays for it.** A local job takes budget a CI job could have had. With the
  skip of §10 working, the work is done once, earlier. It is done twice without it,
  on an engine that has granted dind, and, where a repository compares the merge
  result, whenever the base moved after the check.
- **The status proves less than it seems to.** It is posted with faber's token, which is
  the operator's own identity on GitHub (ADR-0027). Anything running as the operator
  with a token of its own can post the same context. So the status says "the operator's
  account vouches for this tree", which is also who may merge. The evidence that faber
  ran it is the outcome record in the audit trail.
- **A host that has granted dind gets the sized run and no skip.** Clearing that is the
  operator's step, the same one that brings the git mirror back
  ([git-mirror.md](../git-mirror.md#not-beside-dind)).
- **An override is free, and worth nothing to CI.** A caller who overrides to go faster
  has to run the check as defined once before pushing, or CI does the work.
- **v1 covers part of a typical job.** Steps that need a token, LFS content, a Docker
  daemon or a macOS VM stay in CI.
- **Two more per-repo volumes**, the source store and the local cache. ADR-0024's gate
  does not count volumes: it measures free space on the Docker volume, where these show
  up as less of it. The cache has ADR-0055's bounds. The store has only the age rule of
  §11.

## Validation

Filled in on the first watched repo. The ADR stays `proposed` until each row has a
measurement.

| Stage | Metric | Baseline | With local jobs | Date |
|---|---|---|---|---|
| Sized run | seconds from request to verdict, against the CI steps' duration | | | |
| Sized run | bundle size, first run and later runs | | | |
| Sized run | learned cap per check class, against the first run's reservation | | | |
| Vouched check | share of pull requests whose first CI run is red on the checked steps | | | |
| Vouched check | share of pull request runs that found a matching status | | | |
| Vouched check | share of passes posted by `publish`, by the fallback, and not at all | | | |
| Adoption | validating CI runs per merged pull request | | | |
| Adoption | runner-seconds per merged pull request, local jobs included | | | |

## Alternatives considered

**Lease a database to the host.** Let a check on the desktop reach the shared Postgres
through a new bridged socket. It reuses one service and nothing else, it puts bytes from
a server that jobs can reach in front of processes running as the operator, and Redis
and the per-job services would each need a bridge of their own. The check would still
run on macOS, outside admission.

**Run the real workflow on the branch before the pull request opens.** A dispatch or a
called workflow on the pushed branch gets faber's sizing and exact equivalence with no
new job source. Rejected as the first step because the code is pushed before it passes,
each fix round pays a push, a runner registration and a GitHub round trip, and a job
that needs no token still gets one. The comparison of §10 would serve it unchanged, so
it stays available.

**A label.** On the pull request or in `runs-on`. It is set by whoever pushes and names
no content.

**Post an overridden run.** Under the check's name it would let `--run true` vouch for
the check. Under a name of its own it would be a status no workflow reads.

**Checks never take a base.** Every check a full run. Unforgeable without recording
anything more, and it rules out the scoped checks that are fast enough to run before
every pull request.

**Share the CI job's limiter class.** A check that covers a whole job could use that
job's learned cap from its first run. It saves one profiling run per check per host, and
feeds two kinds of run into one class: the CI job includes the runner, the checkout and
the steps a check cannot run.

**Rank local jobs behind CI.** Open pull requests' runs would always go first. Under
load a check waits, its caller gives up, and CI runs the work anyway.

**Post from an engine that has granted dind.** A forged pass needs a trusted actor's
dind job to turn hostile, which is inside the trusted-code model. Rejected because a
pass lets CI do less, and one rule about such engines is easier to hold than two.

**Mount the repo's cache volume read-only.** Warm from the first run. After a lockfile
change a check has nowhere to save its install, and every script that expects a writable
cache has to tolerate one that is not.

**Post only at discovery, or poll for the commit.** Discovery alone loses the race on a
host that did not run the check. Polling spends API calls on every pass and still races
a pull request opened quickly.

**Tell jobs through the runner's environment.** faber could give each runner the list of
its repo's passed trees. No GitHub call and no race, but only on the host that ran the
check, and a job on another host or a hosted runner would learn nothing.

**A Checks view of its own.** Everything about checks in one place, the record of what
was vouched included. It changes ADR-0054, and it splits "what is using the pool now"
across two views: the operator asking why a job is parked would have to look in both.

**Show checks in the dashboard only.** The popover would stay about CI. A check that
parks a CI job would then be missing from the surface the operator glances at.

**Pass and fail in the popover.** It would answer "did my check pass?" at a glance. The
popover draws no verdict for a CI job, and a red failed check would sit beside the red
ring that means faber broke.

**Keep what became of a pass in the CLI only.** Less to build. "Why did CI repeat the
work?" could then be answered only by whoever still has the terminal's output.

**Use the git mirror for the base objects.** A thin bundle against ADR-0035's mirror
needs no second store. The mirror is off on every engine that has granted dind until a
host-side broker exists, which would leave such a host without local jobs.
