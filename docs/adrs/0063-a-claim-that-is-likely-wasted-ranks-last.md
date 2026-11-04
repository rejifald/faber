---
title: A claim that is likely wasted ranks last
type: adr
status: proposed
date: 2026-11-01
owner: rejifald
---

# ADR-0063: A claim that is likely wasted ranks last

## Status

proposed (2026-11-01). Builds on the tiers of
[ADR-0057](0057-an-operator-ranks-claims-by-rules-a-pull-request-cannot-meet.md), which
is itself proposed: this ADR cannot be accepted before it. ADR-0057's tiers are rules the
operator writes. This ADR adds one tier below all of them, filled by signals faber reads
for itself. Adds one check at admission, beside the gates of
[ADR-0019](0019-cpu-is-a-deadline-resource.md) and
[ADR-0024](0024-a-full-disk-stops-admission-not-docker.md). It is not a trust check and
does not touch `src/gate.rs`. Every signal is off unless the operator sets it
(ADR-0013/0014, AGENTS.md).

## Context

faber parks a claim until the host can afford it, and on a busy host that is minutes to
hours. While it waits, the run it belongs to can stop being worth running:

- **The PR conflicts.** A `pull_request` run checks out `refs/pull/<N>/merge`
  ([`WorkflowRun::content_ref`](../../src/github.rs)). Once the PR conflicts with its
  base, GitHub stops updating that ref and starts no new runs for the PR. A job queued
  before the conflict is admitted anyway and tests the last merge GitHub computed, which
  is neither the PR as it is nor anything that can be merged.
- **The PR moved on.** A newer push started a newer run. The old run tests a commit
  nobody will merge. A workflow's `concurrency` group cancels it only if the workflow
  has one.
- **The PR was closed** or its branch deleted.
- **Another job of the same run already failed.** The run's verdict is red whatever this
  job does. Its result still has some value (it can show a second failure), so this is a
  weaker signal than the others.

Each of these holds a runner, memory and disk that a useful job was waiting for. faber
spends throughput on them only when something else is waiting, so the cost is
contention, not idle capacity.

Two facts shape the answer.

**1. Ranking last costs almost nothing when faber is wrong; cancelling does not.** faber
admits in order only while claims compete. When nothing else waits, a claim ranked last
starts at once. A wrongly ranked claim only starts later. A wrongly cancelled one makes
someone re-run it. So the default answer to "likely wasted" is to rank last, and
cancelling is kept for signals that are close to certain.

**2. Within one repo, a rank is only advisory** (ADR-0057, Context fact 2). A JIT runner
is not pinned to the job it was minted for, and GitHub hands it whichever queued job of
the repo matches its labels, often the oldest. Ranking a stale claim last therefore works
exactly across repos: a conflicted PR in one repo stops holding up another repo. Inside
a busy repo, where most waste competes with that repo's own work, a runner minted for a
higher claim can still take the stale job. There the only thing that stops the job is
cancelling its run.

There is no recorded incident yet, and no number for how often any signal fires. The
`record` mode below exists to get those numbers first.

## Decision

### 1. Which runs

Only a `pull_request` run with a PR number (`WorkflowRun::pull_requests` not empty), the
runs `content_ref` gives a merge ref to. `push`, `merge_group`, `workflow_dispatch` and
`schedule` runs test their own commit and are never checked. A fork PR's run carries no
PR number, and the gate refuses it before this point anyway.

### 2. The signals

| Signal | Read from | Fires when |
|---|---|---|
| `closed` | `GET /pulls/{n}` | `state` is `closed` and the run was created before `closed_at` |
| `superseded` | `GET /pulls/{n}` | `head.sha` differs from the run's `head_sha` |
| `conflicted` | `GET /pulls/{n}` | `mergeable` is `false` |
| `run_failed` | `GET /actions/runs/{id}/jobs` | another job of the run has concluded `failure` |

A run created after `closed_at` was started by the close itself (`on: pull_request: types: [closed]`, a preview teardown, a post-merge job), so it is not wasted. A missing timestamp on either side is no answer, like every other signal.

The first three come from one read of the PR. `run_failed` needs a second read, of the
run's jobs, through the `Github::run_jobs` call faber already makes for discovery.

Each signal fires only on a definite answer. `mergeable: null` (GitHub has not computed
it yet), a failed read, a timeout, a 404 or a missing permission is "not fired". This
is the opposite of the trust gate, which fails closed, and on purpose: a signal that
fails open leaves a job running as it does today.

Answers are cached for 60 s, per PR for the PR read and per run for the jobs read, so a
run with twenty jobs reads each once.

A signal can only lower the claim it describes. A PR controls whether its own run
conflicts, moves on or fails, so the most it can do with these signals is rank its own
jobs last. It cannot lower another PR's jobs or raise its own. ADR-0057's rule, that no
PR can promote itself, holds.

### 3. Four modes per signal

```toml
[waste]
closed     = "skip"
superseded = "demote"
conflicted = "demote"
run_failed = "record"
```

- **`off`** (the default, and what an absent key means): no read for this signal.
- **`record`:** read and decide. Nothing changes for the job. faber writes one audit
  line (`waste`, with the signal, the job, the PR and how long the claim had waited) and
  counts it. This is the measurement.
- **`demote`:** the claim moves to the waste tier (§5).
- **`skip`:** the claim is not admitted and its run is cancelled (§6).

When several signals fire, the strongest action wins: `skip`, then `demote`, then
`record`. A section with no keys reads nothing.

### 4. When faber checks: only at the head

A claim is checked when the limiter would admit it next, between "the gates pass" and
`admit_claim`, and nowhere else. A parked claim further back costs no reads while it
waits.

- **No signal fires:** the claim is admitted as today.
- **A `demote` signal fires:** its reservation goes back to the ledger, it moves to the
  waste tier, and the limiter picks the next head. The claim holds the slot for one read,
  bounded by a 5 s timeout. A timeout means "not fired".
- **A `skip` signal fires:** §6.

The number of reads therefore tracks admissions, not the length of the queue. Checking
every parked claim on every tick would cost up to fifty reads a minute on a full queue,
on a token shared with every other part of faber and with CI.

A demoted claim stays demoted. It is checked again when it reaches the head of the waste
tier. If no signal fires then, it is admitted, still from the waste tier. In practice
nothing undoes a signal for the same run: resolving a conflict, or any change to the
PR, is a new push, and GitHub starts a new run for it.

### 5. The waste tier

The waste tier sits below ADR-0057's lowest tier, the tier of claims that match no rule.

- **Within it, claims keep their ticket order**, so the oldest demoted claim goes first.
  A demoted claim keeps its ticket and changes only its tier.
- **Every gate still applies, and nothing is pre-empted** (ADR-0057 §3). The waste tier
  only decides who is the head.
- **It applies to both queues**, Docker admission and the macOS queue of ADR-0057 §4,
  each on its own budget.
- **The refusal names the signal**: `set aside: likely wasted (conflicted, PR #N)`, or
  `queued behind` on the macOS queue. The dashboard and popover show it as a reason to
  wait, not as a failure.

### 6. What `skip` does

1. **The claim is released**, as for any claim that will not be admitted. No runner is
   minted.
2. **The workflow run is cancelled** (`POST .../actions/runs/{id}/cancel`). Leaving the
   job queued does not stop it: a runner minted for another job of the repo can take it,
   the bypass ADR-0030 records. GitHub has no per-job cancel, so the whole run goes. That
   is right for `closed`, `superseded` and `conflicted`, where every job in the run
   tests the same stale tree. It is not obviously right for `run_failed`, whose other
   jobs may still report more failures, so `skip` is refused for `run_failed` when the
   config is validated.
3. **For `conflicted`, the PR is told why.** faber posts a commit status on the run's
   `head_sha`, context `faber/waste`, state `error`, description "Skipped: this PR
   conflicts with its base. Resolve the conflict to run CI." It sits on the head commit,
   so it drops out of view at the next push. `closed` and `superseded` post nothing:
   nobody is looking at that commit.
4. **The outcome records the skip** with its signal. The job's verdict is shown as GitHub
   reports it, `cancelled`
   ([ADR-0044](0044-a-finished-run-shows-ci-s-verdict-only-as-github-reported-it.md)). A
   cancelled run is not a faber-caused failure, so auto-retry (ADR-0003) never retries
   it.

### 7. Permissions

The section adds its needs to `credential::needs` under a feature of its own, so the
token link asks for them and the daemon probes them at boot:

- **Any of `closed`, `superseded`, `conflicted` not `off`:** Pull requests: read.
- **`run_failed` not `off`:** Actions: read, which faber always needs.
- **Any signal set to `skip`:** Actions: write (the cancel).
- **`conflicted = "skip"`:** Commit statuses: write (the status).

A token without them fails each read, which §2 turns into "not fired", and the boot
probe says so. A missing permission never blocks a job.

### 8. Peers

Each host checks on its own. Two hosts that read the same PR reach the same answer, and
cancelling a run twice is harmless.

## Consequences

- With `demote`, likely waste stops holding up other repos. Inside one busy repo it does
  much less (Context, fact 2). An operator who wants waste gone there has to use `skip`
  for the signals that allow it.
- **The waste tier can starve, and that is the point.** Under steady load a demoted claim
  may never reach the head. GitHub cancels a job that waits more than 24 hours for a
  self-hosted runner, which clears what is left. The refusal names the signal, so a
  starved claim is visible.
- **A PR can become mergeable again without a push**: the base commit that caused the
  conflict is reverted. With `conflicted = "skip"` its run stays cancelled, and someone
  re-runs it by hand. With `demote` nothing is lost. This is one reason `demote` is the
  suggested setting for `conflicted`.
- At most two extra reads per admitted PR job, one for the PR and one for the run's
  jobs, each cached for a minute. A parked claim costs nothing until it reaches the head.
- `record` changes nothing a job sees, so it can run on every host while the numbers
  come in. The owner chooses `demote` or `skip` per signal from those numbers.

## Alternatives considered

- **Only skipping conflicted PRs** (this ADR's first draft). Rejected as the only tool:
  a false skip costs a re-run, while a false demote costs a delay that happens only under
  contention. Skip stays available for the near-certain signals.
- **Putting the signals in the trust gate.** Rejected: the gate answers "is this code
  safe to run here" and fails closed. Waste says a run is useless, not unsafe, and must
  fail open.
- **Signals as ADR-0057 rule fields** (`conflicted = true` in a rule). Rejected: a rule
  is static and set by the operator, and its tier is fixed when the claim is taken. A
  signal changes while the claim waits, and is read only at the head. One tier with its
  own key keeps that difference visible.
- **Re-checking every parked claim on every tick.** Rejected for its cost in reads (§4).
- **Checking `push` runs on a PR branch too.** Rejected: a push run tests the branch head,
  a real commit whatever the PR's state. Ranking it last would hide a broken branch.
- **A `draft` signal.** Left out of this version. A draft PR's run is often exactly what
  its author is waiting for, so ranking it last is a policy the owner should ask for, not
  a waste signal. It can be added as a signal later without changing anything here.
- **Aging the waste tier.** Rejected for ADR-0057's reason: aging brings back the delay
  the tier exists to remove.
