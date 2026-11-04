---
title: A finished run shows CI's verdict only as GitHub reported it
type: adr
status: accepted
date: 2026-10-19
owner: rejifald
---

# ADR-0044: A finished run shows CI's verdict only as GitHub reported it

## Status

accepted (2026-10-19). The owner decided it on 2026-10-17, in the popover's anatomy
([`docs/design/popover/anatomy.md`](../design/popover/anatomy.md) § 2, "Daemon: finished jobs
carry their CI verdict", and § 5).

*Amended 2026-10-19 (owner, #287):* each finished run also gets one early fetch about 5 s after
it ends, and the outcome records when its verdict arrived (`conclusion_ts_ms`). The popover counts
a CI pip's linger from that time, not from the run's end. § 3 and Consequences say how.

*Amended 2026-11-01 (owner):* **only failures faber caused are faber's to report.** A job whose
tests caught a bug, or whose own code is broken, ran on a runner faber delivered, so faber has
done its part. The popover no longer draws CI's `failure` or `timed_out` as a failure: it draws
the same muted **done** pip as `success`, for the same 30 s, with no red mark, no red line and no
"failed" word. Red is reserved for faber's own faults (the ring, from `result`). The daemon side
of this ADR is unchanged: `conclusion` is still fetched and sent verbatim, and the popover still
shows it in a finished job's hover title. § 1 to § 6 stand; the anatomy's pip table
([`popover/anatomy.md`](../design/popover/anatomy.md) § 2) carries the new pip.

## Context

The popover gives each job in a group one pip. The owner wanted a job that has just finished
to keep a pip for a short time: green for 30 s when CI passed, and red for 10 min when it
failed. The daemon's outcome feed (`OutcomeEvent`, `/api/state` `outcomes[]`) could not supply
that. Its `result` measures the runner **container**, and an ephemeral Actions runner exits 0
whether the job it ran passed or failed. So `result: success` does not mean CI passed.

The code already said this, in `tally_of` in `src/reconcile.rs`: "faber has no visibility into
the job's conclusion and must not imply it does." That remark was about the container exit. The
container cannot tell faber the verdict, and nothing should pretend it can. GitHub can tell
faber, and faber already reads finished jobs for other purposes (claim reconcile, the
runner-retirement sampler, auto-retry).

## Decision

1. **`OutcomeEvent` gains `conclusion`**: GitHub's conclusion for the executed job, spelled as
   GitHub spells it (`success`, `failure`, `timed_out`, `cancelled`, `skipped`, …). faber does
   not interpret it, map it or default it.
2. **A verdict is shown only as GitHub reported it.** `conclusion` is set only from a job that
   GitHub reports as `completed` **and** whose `runner_name` is this run's runner. In every other
   case it stays absent: the job is gone, another runner ran it, the fetch failed, the token is
   refused, or GitHub has not finished the job yet. An absent `conclusion` is not a guess. The
   popover then draws no CI pip, and the container's `result` still drives the fault ring.
3. **A small fetch budget, off the loop.** After a runner exits, its job is queued, and each
   reconcile cycle starts one batch on its own task. It reads the queue's 8 oldest jobs once
   each (`GET /repos/{repo}/actions/jobs/{id}`, the ADR-0027 `Actions: read` grant that is
   already held, so no new scope). The loop never waits on a batch, and a batch still running
   is not joined by a second. The batch starts before the pause, drain and topic-search early
   returns, so a paused or draining host still reads what its last jobs concluded. Only a
   blocked credential, which claims nothing and asks GitHub as little as it can, skips it.
   - A job that is not completed yet, a refused fetch, or a repo where `Actions: read` is
     currently refused each spends one try. After **3 tries** faber stops asking, and
     `conclusion` stays absent.
   - A request that got no response at all (a timeout, a refused connection) ends the batch and
     spends no try. GitHub was not reached, so nothing was learned.
   - A reclaimed runner ran no job, so faber does not ask about it.

   - *Amended 2026-10-19:* each run also gets one **early fetch**, on its own task, 5 s after
     it ends. It takes the run out of the queue before asking, so the early fetch and a batch never
     ask about the same run, and its try counts against the same 3. A job not yet completed goes
     back into the queue for the batches.

   The queue is bounded by the outcome feed's own cap (50). That is at most 8 GETs a cycle, plus
   one early GET per finished run, and three per run in total.
4. **The identity of the job that ran.** `OutcomeEvent` also gains `pr`, `branch`, `workflow`,
   `job_name` and `event`, in `InflightView`'s shapes, so that a finished job joins its live
   group. When the executed job is the minted job, they are copied from the claim. Otherwise
   they come from the executed job's own fields, from the same fetch (`name`, `workflow_name`,
   `head_branch`), with **no PR**. The minted job's PR is never attached to a different job.
5. **Display data only.** `result`, `tally_of`, `Counts`, the audit record, admission and the
   menu-bar count do not read `conclusion`. Nothing is counted, admitted or retried because of
   it.
6. **Back-compatibility.** The new fields are always present in the JSON, as `null` when
   unknown. A reader handles a missing key and `null` the same way. An older daemon sends
   neither, so the popover shows no passed or CI-failed pips, and the fault ring still works
   from `result`.

## Consequences

- `tally_of`'s comment no longer says faber has no visibility into the conclusion. It now says
  that the container's exit is not a verdict and that the tally never reads GitHub's.
- The outcome feed is in memory. After a daemon restart, finished runs and their pending fetches
  are gone, and so are their lingering pips. The anatomy accepted this.
- *Amended 2026-10-19 (owner, #287).* As first built, the verdict arrived on the first cycle
  after the runner exited (20 s by default), and the popover counted the passed pip's 30 s from
  the run's end. A slow first answer shortened the green pip, and three misses could hide it
  entirely. Now the early fetch usually has the verdict about 5 s after the run ends, and
  `OutcomeEvent::conclusion_ts_ms` records when it arrived. The popover counts the passed pip's
  30 s and the CI-failed pip's 10 min from `conclusion_ts_ms`, so a late verdict still shows for
  its full time. With an older daemon, which sends no `conclusion_ts_ms`, it falls back to the
  run's end (`ts_ms`).
- One more GET per finished job is a small rise in API use. ETag caching does not help here,
  because each job is read once while it changes.
