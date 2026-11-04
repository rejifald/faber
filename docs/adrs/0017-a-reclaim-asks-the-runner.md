---
title: A reclaim asks the runner before it kills it
type: adr
status: accepted
date: 2026-09-17
owner: rejifald
---

# ADR-0017: A reclaim asks the runner before it kills it

## Status

accepted (2026-09-17). Amends [ADR-0002](0002-reconcile-inflight-claims.md): its
claim that reclaim is "provably safe under reassignment" did not hold, and the
parked-to-admitted race it accepted is closed here.

**Amended by [ADR-0018](0018-a-runner-is-named-when-it-registers.md) (2026-09-17).**
A claim's runner name is minted, and published, as the claim is admitted rather
than when its provision task starts, because the timestamp in it is the runner's
age. It is still published before `admitted` flips, so every admitted claim has
one. A parked claim has none.

## Context

The claim reconciler reclaims admitted runners that do no work, because an idle
runner holds a memory reservation other jobs are parked behind (issue #16). It
decides "does no work" from GitHub's view of the runner: the job the runner was
minted for, and the `busy` flag on `GET /actions/runners`. ADR-0002 made that flag
the veto, and the #16 fix kept it as one. Both assumed the flag is true at the
moment the runner is killed.

It is not, in two different ways.

**The flag was wrong for minutes.** In September 2026, on one host running one
binary, a few reclaims killed runners that GitHub records as running *other*
jobs. A JIT runner is not job-pinned, so each had been handed a sibling's job
while the job it was minted for ran and finished elsewhere:

| runner's job | in it for | peak memory | GitHub failed it |
|---|---|---|---|
| a build job | a few minutes | several GiB | about ten minutes after the kill |
| a test shard | a few minutes | several GiB | about ten minutes after |
| a test suite | under a minute | a few hundred MiB | about ten minutes after |

Each ended with its current step frozen `in_progress` and "The self-hosted runner
lost communication with the server". Every verdict read *job finished, runner
executing nothing*. In the first, several consecutive reconcile cycles over more
than two minutes read the runner as not busy while it was minutes into its job. A
listener waiting for work peaks at a few dozen MiB. Across that host's audit log,
about one recorded reclaim in a hundred had first peaked far above that, from a
few hundred MiB to several GiB.

Why the list said "not busy" for so long was not established. A later observation
window found the flag accurate to within a few seconds and ETags that tracked
every change, so it is an intermittent failure. The design below does not depend
on explaining it.

**The flag is stale by the time the kill lands.** The list is read once per
cycle and reused for every claim in it. The kill then waited for the memory
sampler to stop and three post-mortem `docker` reads. Over a few thousand
reclaims, the gap from decision to kill was a few seconds at the median, over ten
at p99 and more than half a minute at worst. An idle runner is exactly the one
GitHub hands the next queued job, and bursts are when reclaims happen. Seen live
the same month: a reclaim was decided, the container was killed several seconds
later, and seconds after that GitHub's list showed that runner **busy**. A job
queued seconds earlier had been handed to a dead runner. That time the job waited
a few minutes and ran elsewhere. Handed over a few seconds sooner, inside the
seconds between decision and kill, it would have started and then been killed.

A third defect sat beside these. A claim found its runner by the
`faber-{job_id}-` prefix. Instances sharing a pool (ADR-0004) mint for the same
queued job, one pair observed milliseconds apart, so the prefix matched a peer's
runner as well. A peer's busy runner kept an idle one alive, and a peer's idle
runner could stand in for one missing from the list.

### What this does not explain

This was investigated after "lost communication" failures in mid-September 2026,
in a few runs of a watched repo where two CI runs were queued at once. Those
failed jobs all ran on a **peer instance's** runners. The runners this host
reclaimed in those windows peaked at an idle listener's few dozen MiB, and no job
records them as its runner. The failed jobs' step logs also show a graceful
cancel, one of them still executing steps minutes after GitHub failed the job. A
`docker kill` leaves a step frozen `in_progress` instead. So those incidents need
the peer's own logs. The bug below is real and is fixed. It is not proven to be
what caused them.

## Decision

### The reconciler requests, the runner's owner decides

A reclaim verdict is a request. The provision task that owns the runner kills it
only after `reclaim_clearance` (`src/docker.rs`) confirms it, from sources read at
that moment:

1. **The container's process table.** `docker top <runner> -eo pid,comm`. A
   `Runner.Worker` process exists exactly while the runner executes a job, post
   steps included. If one is there, the request is refused and GitHub is not asked.
   Only `comm` is read. The full command line carries the runner's JIT credential.
2. **Withdraw the registration.** `DELETE /actions/runners/{id}`, with the id from
   the JIT mint or, for an adopted container, found by exact name. GitHub refuses a
   runner with a job assigned (`422 … is currently running a job and cannot be
   deleted`). GitHub checks that when the request arrives, not from a listing
   faber fetched earlier. Once it agrees, GitHub can assign the runner nothing
   more, so the answer holds until the kill lands.
3. **Look again.** A job the listener accepted just before the withdrawal shows up
   as a worker now.
4. **Kill.** Immediately, before the post-mortem reads.

If anything is unreadable, the request is refused. A container that has already
stopped needs no kill. Its run gets a few seconds to report its own exit before the
end is recorded as a reclaim, so a wedged `docker wait` still cannot hold a claim or
a drain open.

A refused request changes nothing. The job runs on, and the watcher goes back to
waiting for the run to end, the lifetime cap, or the next request. This is the one
place a reclaim kills, and both the provisioning path and ADR-0011 adoption pass
through it, so the parked-to-admitted race ADR-0002 accepted is closed too.

### Better requests

- **Exact identity.** A claim records the name its runner registers under the
  moment the name is minted, before admission. The verdict reads that runner only.
- **A second witness.** A runner is busy if the list says so **or** this cycle's
  scan found a queued or in-progress job naming it as `runner_name`, the same
  harvest the learner already attributes runs by. The swap in the table above is
  that case exactly.
- **A request restarts the idle window.** A refused claim is asked again only after
  another full grace of confirmed idleness, not every cycle.
- **Honest logs.** `requesting reclaim of job N's claim (job finished, runner looks
  idle)`, then either `reclaimed while idle (no job running in it) — killing` or
  `not reclaiming — a job is running in it; it keeps running`.

## Consequences

- Genuine idle reclaim keeps working. An idle runner is withdrawn, then killed. It
  now costs one `docker top` and one GitHub `DELETE`, plus a lookup for an adopted
  container.
- Withdrawing first also means GitHub stops offering jobs to a runner being
  reclaimed. Before, a killed runner stayed `online` until its session timed out,
  and could be handed a job while dead.
- A host whose process table cannot be read never reclaims. The 3 h lifetime cap
  stays the backstop. That is the chosen direction, because a kept idle runner
  costs a reservation and a wrong kill costs a job.
- Two external contracts are now load-bearing. The worker's process name comes
  from `actions/runner`: an image built on a different agent would always read as
  idle and fall back to the withdrawal alone. And GitHub refuses to delete a runner
  with a job assigned, which the stale-runner sweep has relied on since #21.
- This reaches other hosts only through a release (ADR-0006). A follower on an
  older release keeps the old behaviour until then.

## Alternatives considered

- **Re-read `busy` from `GET /actions/runners/{id}` just before killing.** This is
  the flag that was wrong for minutes, and a read followed by a kill still leaves a
  window between them. Rejected as the authority. The withdrawal is the atomic
  version of this check.
- **Key the decision only on the job each runner actually took.** Adopted as a
  witness, not as the authority. The scan is single-page and a cycle old, and it
  cannot see a job handed over after it ran.
- **Trust only the local process check.** A listener can accept a job between the
  check and the kill. Withdrawing first is what closes that.
- **Stop the runner gracefully (SIGTERM).** The runner cancels a running job on
  SIGTERM, so it aborts the same work, more politely.
- **Stop reclaiming.** Brings back #16: idle runners holding the budget while real
  jobs park.

## Validation

Unit-tested (`src/docker.rs`, `src/reconcile.rs`, `src/github.rs`):

- the clearance table: a worker refuses without asking GitHub; a `422` refuses; a
  job accepted during the withdrawal refuses; anything unreadable refuses; an idle,
  withdrawn runner is cleared;
- the watcher: a refused request never ends a run, a repeated request is answered
  again, a standing request is honoured, the cap still ends a run, a dropped sender
  is never a request;
- the swap end to end, `a_runner_serving_a_swapped_job_is_not_killed_until_that_job_ends`.
  Two jobs, each on the other's runner. Job 1 finishes. The runner list goes stale
  and the scan later loses job 2 too. Every request is delivered to runner A's real
  watcher, and A runs until job 2 ends. The test fails if the kill site kills on
  request, if the scan witness is removed, or if the process table is consulted
  after GitHub instead of before.

Live criteria, after deployment:

1. No `reclaimed` outcome peaks above ~100 MiB.
2. Refusals appear as `not reclaiming — …` lines, and none is followed by that job
   failing with "lost communication".
3. Idle reclaims continue at their previous rate.
