---
title: Auto-retry faber-caused job failures
type: adr
status: accepted
date: 2026-07-25
owner: rejifald
---

# ADR-0003: Auto-retry faber-caused job failures

## Status

accepted (2026-07-25), **amended 2026-08-26** — a second retry trigger, see
*"Amendment (2026-08-26)"* at the foot of this file. The amendment reverses this
ADR's rejection of `oom` as a trigger; the decision, the opt-in, the attempt cap
and the surgical/batch split are unchanged. **Amended again 2026-10-11** — the
annotation gate also accepts a zero-step death, see *"Amendment (2026-10-11)"*.
**Amended again 2026-10-18** — the trigger is faber's own audit record, and the
annotation is no longer read at all, see *"Amendment (2026-10-18)"*; that
amendment supersedes the *"signal that does work"* section above, both earlier
amendments' triggers, and the *Opt-in* bullet's mention of a classic `repo` token.

accepted (2026-07-25) — implemented this session (see the diff on `src/retry.rs`
(new), `src/github.rs`, `src/reconcile.rs`, `src/config.rs`, `src/docker.rs`,
`src/web.rs`). Prompted by an operator observation: "some jobs failed, I suspect
faber or an unexpected shutdown — retry them automatically." Supersedes the
early backlog's "Auto-retry on OOM (deferred by design)" item, and refines its trigger.

## Context

A self-hosted job on a watched repo can fail for two very different reasons:

1. **Genuine.** The runner ran the job and a step exited non-zero (a test failed,
   lint failed, a build broke). This is the developer's problem; re-running it is
   noise at best and masks a real failure at worst.
2. **faber-caused.** faber's runner never finished the job: the container was
   OOM-killed by faber's memory cap, killed at its lifetime cap, reclaimed while
   idle, or — most commonly — the **daemon was SIGKILLed mid-job** (a redeploy;
   real jobs run up to 120 min, far past any drain window in `main.rs`). The
   job on GitHub then fails through no fault of the code.

An investigation over the retained audit log (over a hundred thousand records,
a few thousand outcomes) and GitHub ground truth on a watched repo found that
**nearly two thirds of recently-failed jobs were faber-caused** (sample: the last
few dozen failed jobs, across a couple of dozen failed runs). Yet faber's own
outcome record classified **~none** of them as self-inflicted. Three structural
reasons:

- **`job_ok` is the runner *container's* exit code, not the GitHub job
  conclusion.** A JIT runner exits `0` after cleanly running a job *whose steps
  failed*, so a genuine failure is recorded as `result: "success"`. faber never
  learns the job's pass/fail.
- **Infra failure and genuine failure collapse to one record.** When the backend
  errors before the job runs (`run_backend` returns `RunReport::default()`), the
  outcome is byte-identical to a runner that ran and exited non-zero — both
  `result: "failure", oom: false`.
- **Unexpected shutdown leaves no record at all.** A SIGKILLed daemon never runs
  `record_outcome`; the job dies on GitHub and faber has no trace.

So faber's *own* signals (`oom` / `timed_out` / `reclaimed`) cannot drive a safe
retry: `oom` mostly fires on **successful** jobs (dind memory pressure, non-fatal
— every one of the several dozen recorded OOMs was on `result: "success"`), and
the shutdown case has no local record to key on.

### The signal that does work

Every faber-caused death — OOM, lifetime-cap, reclaim, **and** shutdown-stranding
— surfaces the *same* authoritative annotation on GitHub, on a provably-faber
runner:

```
runner_name: faber-<id>-<ts>
annotation:  "The self-hosted runner lost communication with the server. …"
```

Genuine failures never carry it — they annotate `"Process completed with exit
code N"`. So the classifier is crisp and false-positive-free:

> **retry ⇔ the job's runner name starts with `faber-` AND its failure annotation
> is the "lost communication" runner-death marker.**

This lives entirely on GitHub's side, so it sidesteps faber's internal
classification gap and covers the shutdown case (which has no local record) for
free.

## Decision

Add a **retry sweep** to the reconcile cycle. Once per cycle, per watched repo,
faber fetches `runs?status=failure`, and for each failed run within a recent
**lookback window** whose `run_attempt` is below the cap, it inspects the run's
jobs. A job is a retry candidate iff its runner is `faber-*` and its check-run
annotations contain the runner-lost marker. faber then re-runs via the GitHub
Actions API and records the action.

Forces and how they're resolved:

- **Never retry a genuine failure.** The annotation marker is the gate; a job
  with a failed *step* (exit code N) is never touched.
- **Never touch another runner's job.** The `faber-` name prefix is required.
- **Bound the blast radius.** Retry only if `run_attempt < max_attempts`
  (default 2 → one automatic retry). The bound is GitHub-native and needs no
  local persistence: a re-run increments `run_attempt`, so the cap survives a
  daemon restart. A short-lived in-memory `(run_id, run_attempt)` dedup set
  prevents a double-POST in the seconds between the re-run request and GitHub
  flipping the run to `in_progress`.
- **Don't retry ancient history on first boot.** `runs?status=failure` returns
  the last ~100 failed runs including old ones. A **lookback window** (default 30
  min, on the run's `updated_at`) means a fresh deploy retries only *recent*
  faber-deaths, not a pile of historical ones.
- **Surgical vs. batch re-run.** If **every** failed job in the run is a
  faber-death (the common redeploy case — a whole run's runners die at once),
  faber calls `rerun-failed-jobs` once. If the run is **mixed** (some genuine
  failures), faber re-runs only the faber-death jobs individually, so a genuine
  failure is never re-run. (Known limit: a *mixed* run with multiple faber-death
  jobs may retry only some per attempt; the remainder fall back to the operator's
  manual re-run — the status quo. Rare, and never unsafe.)
- **Opt-in.** The prior design kept faber "a pure provisioner … no `actions:write`,
  no re-run API". Auto-retry gives faber write access to Actions, so it ships
  **disabled by default** (`[retry] enabled = false`) and is enabled per
  deployment. The `gh` token carries the classic `repo` scope, which under
  GitHub's model covers the workflow re-run endpoints (no `workflow` scope is
  needed — that governs editing workflow *files*, not re-running them). faber has
  a `cancel_run` helper at the same write tier but it is currently unwired, so the
  re-run POST is exercised for the first time by this feature; its permission is
  confirmed on the first live retry.
- **Synergy with the autolimiter.** An OOM-death force-bumps the offending
  class's cap *before* the retry runs, so the re-run provisions at a larger cap
  and passes — the "remember-only" self-heal the autolimiter doc describes, now
  automated.

### Reporting fix (bundled)

To stop the dashboard/audit mislabeling faber-deaths:

- `RunReport` gains an **`infra_error`** flag, set when the backend errors before
  the job runs (dind/JIT/network setup, or a transport error). It records as
  `result: "infra_error"` — distinct from a clean non-zero job exit — and, like
  `timed_out`/`reclaimed`, is kept out of the autolimiter's learning (it is not a
  representative memory sample).
- Retries are first-class in the audit (`kind: "retry"`) and on the dashboard (a
  `retried` counter + a recent-retries feed), so faber-caused failures become
  *visible* rather than hidden inside `success`/`failure`.

**Deliberate boundary.** faber does **not** try to record the true GitHub job
conclusion at `record_outcome` time. Because JIT runners are not job-pinned (a
runner minted for job A may run sibling B — see ADR-0002), mapping faber's claimed
`job_id` to the job the runner actually ran is exactly the ambiguity that reconcile
already fights. The retry sweep reads GitHub's *own* per-job truth instead, so the
honest signal is sourced where it's unambiguous.

## Consequences

- faber now performs an outward-facing, state-changing action (re-running a
  workflow). It is gated behind an explicit opt-in, a genuine-failure-proof
  classifier, and a hard attempt cap.
- One extra `runs?status=failure` GET per repo per cycle (ETag-cached → cheap
  `304` when nothing changed), plus one annotations GET per *candidate* job
  (rare: only faber-`*` failed jobs under the cap).
- A genuinely broken host (every job dies) retries each run once and then stops
  at the cap — it cannot loop.
- Live validation to be recorded below after enabling on the running daemon.

---

## Amendment (2026-08-26): a second trigger, on a locally-observed OOM

**Supersedes** the *Context* section's rejection of `oom` as a trigger and the
*"signal that does work"* claim that the annotation gate is sufficient. Adds one
disjunct; changes nothing else.

### Why the annotation gate cannot see an OOM

The gate's premise — that every faber-caused death surfaces "lost communication
with the server" — holds only when the **runner process** dies. The cgroup OOM
killer takes the largest consumer in the cgroup, which for a real job is a child
of a step, not the runner listener. The listener survives, reports its step's exit
137 to GitHub like any other failed step, and exits 0. GitHub never observes a
missing runner, so no runner-lost annotation is written. The failed job carries
either `"Process completed with exit code 137"` — which this ADR named as the
marker of a *genuine* failure — or, as observed live, **no annotation at all**, in
which case `job_runner_lost` answers `Ok(false)` on an empty list. Either way the
sweep files it as genuine and skips it.

Ground truth, one event, both sides (IDs replaced with placeholders, times
relative to the mint):

```
t+0 s     faber mints faber-<job A>-<ts> for job A
t+10 s    GitHub dispatches job B (a deploy job in another run) to it instead
t+35 s    job completes: conclusion "failure", annotations list []
t+40 s    faber records oom_runner: true, cap 512 MiB
```

The original rejection rested on every recorded OOM being on `result: "success"`
(dind memory pressure, non-fatal). That statistic could not support the
conclusion, for the reason this ADR states two paragraphs earlier: `job_ok` is the
*container's* exit code, so at the time every outcome was a `success` unless the
container itself died. The parenthetical was an assumption, not a measurement.

### The added trigger

> **retry ⇔ the job's runner is `faber-*` AND ( the runner-lost annotation is
> present OR faber's own record shows it OOM-killed that job on that runner ).**

The second disjunct is local ground truth the annotation structurally lacks: faber
minted the runner, chose the `--memory` cap, and read the kernel's own verdict on
that container (`.State.OOMKilled` for the runner; the cgroup's `memory.events.local`
counter, with a post-mortem fallback, for dind). It is held in a bounded in-memory
`OomLedger` (`src/retry.rs`), written at `record_outcome` and read before the
annotations GET, with no network call.

Three conditions gate a match, each closing a distinct false positive: **exact
runner-name identity** (the name embeds the mint instant, so it names one
ephemeral runner); **the executed job must agree** when faber knows it (a JIT
runner is not job-pinned, so a kill on a different job is not this job's story);
and **the windows must overlap** when both are known. An unknown job window is not
a non-match — refusing on a missing stamp would silently disarm the signal.

What remains, accepted rather than solved: a job that genuinely fails *and* OOMs
cannot be told from one the OOM caused, because faber cannot read the job log. It
is bounded by `run_attempt` (one automatic re-run), and — when the run's job was
attributed — the class cap is force-bumped first, so the re-run is a real second
chance. An unattributed OOM never reaches the ledger in the first place, for
exactly this reason: without the bump, arming it would only spend the retry on a
rerun into the same cap.

**Not a new knob.** It sits inside the existing `[retry] enabled` opt-in.

### The ledger keys on the OOM flags, not on `result`

An OOM reaches the audit log under two labels, depending on where the kill landed:
`failure` when the kernel took the runner listener, and `success` when it took a
child of a step and the listener exited 0 anyway. The `success` case is deliberate
and unchanged by this amendment — the container did what it was asked, and faber
still does not know the job's conclusion (see *Deliberate boundary* above) — and
it is also the common shape. So the trigger reads `oom_runner`/`oom_dind` on the
outcome, which are true on both; keying on `result` would miss the archetypal one.

### The teardown gap

The sweep no longer files a run as terminally genuine while this host is still
running the runner that failed it. GitHub marks a job complete the moment the
runner reports; faber writes the outcome — and the ledger entry — only after
teardown, about five seconds later in the trace above. A sweep landing in that gap
would put the run in the permanent dedup memory on evidence that had not finished
arriving. Deferring one cycle costs nothing.

### Fleet note

The ledger only ever holds runners *this host* minted. Hosts share a runner pool
([ADR-0004](0004-demand-based-provisioning.md)), so a `faber-*` runner may belong
to a peer daemon — visible in GitHub's shared runner list, with an audit log this
host does not have. A miss therefore means "not mine, or not an OOM", never "no
OOM happened", so the sweep falls through to the annotation gate rather than
concluding anything from it. The peer reaches the same verdict for its own runners.

### Why this ships while the OOM rate is zero

v0.3.1 fixed the cause these kills came from — a runner sized only for the classes
queued at its mint, then handed a heavier job (`fix(limiter): size a runner for
every job it can be handed`, plus the cohort rebuild on startup). The kill traced
above is the **last one this host recorded**: counting from it, no OOMs in about
70 outcomes over most of the following day, against about two dozen in about 160
outcomes over a similar span before it (audit log, measured 2026-08-26).

So this trigger is idle by design, and is built to be correct *when* idle rather
than exercised constantly. It is the standing defence for the cases sizing cannot
cover — a brand-new job class, one dormant past `COHORT_TTL`, a lowered ceiling, a
regression — where the retry gate would otherwise silently skip the failures
again, exactly as it did before. Its cost while idle is a scan of an empty deque
per candidate job and no network I/O at all.

---

## Amendment (2026-10-11): a runner that dies before its first step

**Widens** the annotation gate by one shape; the opt-in, the attempt cap, the
lookback, the surgical/batch split and the OOM trigger are unchanged. Issue #88.

### What the marker misses

GitHub attaches "lost communication with the server" only once the runner has
reported at least one step. A runner that goes silent earlier — and a host-dropout
analysis in September 2026 found most losses go silent within a minute or two of
job start — leaves a job GitHub fails at its runner-lock expiry with **zero steps,
zero annotations and no log**. Over one week in October 2026 a watched repo had a
few dozen such jobs on `faber-*` runners, completing ten to twenty minutes after
they started; most sat in runs where no other job carried the marker, so none of
those runs was retried. They cluster in the same windows as marker-carrying losses
on the same host: the same death, only earlier.

### The added shape

> **runner-lost ⇔ the runner is `faber-*` AND ( the runner-lost annotation is
> present OR ( conclusion is `failure` AND the job reported an empty `steps` list
> AND it carries no annotations at all ) ).**

It keeps the gate false-positive-free for the reason the marker did: a runner that
ran anything reports "Set up job" as step 1, and a failed step annotates "Process
completed with exit code N". A job with neither was never run. Three narrowings
keep it that way:

- **`failure` only.** A `cancelled` job with no steps is one somebody cancelled
  before it was picked up; that is not faber's to re-run.
- **An absent `steps` field is unknown, not zero.** `Job.steps` is an `Option`,
  so a payload that omits the field can never read as the zero-step shape.
- **Any annotation disqualifies.** The live OOM shape — steps, no annotations — is
  untouched and stays the OOM ledger's to judge.

The owner's call (2026-10-11) was that the shape alone is enough: faber does
**not** also require the runner to be offline or gone in `GET /actions/runners`.
By the time the job is failed at lock expiry an ephemeral JIT runner has
usually been deregistered, so that lookup would mostly answer "not found" and add
a request without adding evidence.

The classifier is the pure `retry::runner_loss`, which takes the job's runner
name, conclusion, step count and annotation messages; `Github::job_annotations`
only fetches. The marker constant moved with it, from `src/github.rs` to
`src/retry.rs`.

---

## Amendment (2026-10-18): the trigger is faber's own record, not GitHub's annotation

**Supersedes** the annotation trigger (*"The signal that does work"*), the
2026-10-11 zero-step shape and the 2026-08-26 OOM ledger as separate disjuncts.
**Keeps** the opt-in, the attempt cap, the lookback, the surgical/batch split, the
dedup memory and the gate-bypass exclusion (ADR-0030). Issue #192; decided by the
owner on 2026-10-18.

### Why

Reading a check run's annotations needs Checks: read, and a fine-grained token
cannot hold it ([ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md)).
From the v0.4.0 cut no token can read them, so #187 switched auto-retry off for
any repository the annotations were unreadable on, which is every private one.
Auto-retry is a capability the host relies on daily, so the trigger moves to
evidence faber already holds and no permission gates.

### The rule

> **A failed job is faber's fault only when faber's audit record shows that the
> `faber-*` runner GitHub names on the job ended abnormally while the job was
> running.**

The jobs API (Actions: read) names the runner. The record is the `outcome` line the
runner's run wrote (its new `end` word), or the `freeze` line for a freeze. These
endings count:

- **`oom`** — faber's memory cap OOM-killed the runner or its dind container
  (`oom_runner` / `oom_dind`, as before: on a `success` outcome too, because the
  kernel may take a child of a step and leave the listener exiting 0).
- **`lifetime_cap`** — faber killed the runner at its lifetime cap.
- **`reclaimed`** — faber reclaimed it.
- **`lost`** — the container vanished without an exit status: `docker wait` failed
  on a runner faber launched (a daemon or VM restart, the host sleeping), so faber
  never saw it end and tore down what was left. This is the one case that used to
  have no local record; it now has one.
- **`frozen`** — faber froze the runner (ADR-0032, ADR-0033 §3) and had not thawed
  it when GitHub completed the job. A job that failed after a thaw failed on its
  own.

These do **not** count, and are never retried:

- a runner that **exited cleanly** (`exited`), or failed in some way that was not
  faber's act (`failed`: a non-zero exit of its own, a setup failure before any job
  ran);
- an ending **more than 60 s after the job's `completed_at`** (`END_GRACE_MS`). An
  ending *before* `completed_at` is the common case: a runner killed at 10:00 is
  failed by GitHub's runner-lock expiry at 10:10. Exactly 60 s still counts; 61 s
  does not;
- an ending **before the job's `started_at`**;
- **no record**: a runner minted by a peer host, one from before the `end` field,
  an audit line that failed to append, a job with a missing or nonsensical
  `started_at` / `completed_at`;
- a record that **names a different job** than the one that failed (a JIT runner is
  not job-pinned, ADR-0002), and an OOM faber could not attribute to a job: the
  class cap was never bumped, so the re-run would meet the same cap (unchanged from
  2026-08-26).

Ambiguity is never retried. Every unknown above reads as "not faber's fault", so
the failure mode of the new trigger is a missed retry, which is what the status
quo is without it, and never a re-run of a genuine failure.

### Shape

- `retry::classify` is a pure function from the runner name, job id, the job's two
  timestamps and the endings recorded for that runner to `FaberCaused { kind,
  at_ms }` or `NotFaber(reason)`. A table test covers every ending kind, the
  timing (before, during, +59 s, +60 s, +61 s), a missing record, a foreign
  runner and the freeze cases.
- `retry::EndLedger` indexes the audit records by runner name, bounded, and is
  **rebuilt from the audit log at startup** (`retry::ledger_from_audit`), so a
  restart, the commonest way a runner is lost, does not forget the evidence. The
  live write path and the replay share `retry::outcome_ending`.
- The `outcome` audit line gains `end`. A line from before the field counts only
  through its OOM flags.
- The sweep no longer fetches annotations, so it needs Actions: write and nothing
  from Checks. #187's per-repository "off without Checks: read" state, and the
  `Checks: read` line in the auto-retry row of the permissions table, are gone.
- **Each retry is audited with its evidence.** The `retry` line's `cause` names
  the runner, the ending's word and time, and the job's window.

### Bounds, unchanged

One automatic retry per job (`run_attempt` below `max_attempts`, default 2, which
survives a restart because it lives on GitHub), a 30-minute lookback on the run's
`updated_at`, and the in-memory `(run_id, run_attempt)` dedup. A sweep that lands
before faber has finished with a runner (the teardown gap) defers a cycle.

### The runner-retirement warning (#154)

It shared the sweep's annotation fetch: the sweep handed it the annotations of
failed jobs for free. That is the only link, and it is gone: the warning keeps its
own sampler (one finished job per profile per interval, two API calls), its own
Checks: read gate and its own handling, unchanged. With the sweep silent, the
sampler is its only source, so a profile whose jobs only fail now takes a sample
like any other.

### Consequences

- Auto-retry works on private repositories again, with the cut's token.
- It no longer sees a death faber has no record of. `lost` is written only by a
  daemon that was waiting on the container when it vanished (a `docker wait` that
  fails), or that adopts it (ADR-0011) and then waits. A daemon that is SIGKILLed
  together with the containers under it, and meets nothing at restart, leaves no
  line, and its jobs are not retried. Nor is a runner that dies on its own, such as
  on a host that loses power. The annotation retried both; not retrying them is the
  owner's decision (*err toward not retrying*), and the cost is the missed retries
  a manual re-run still covers.
- A runner that an old daemon started and a new one adopts is judged by the new
  daemon's record of how it ended.
