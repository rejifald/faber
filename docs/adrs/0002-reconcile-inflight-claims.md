---
title: Reconcile in-flight claims against GitHub
type: adr
status: accepted
date: 2026-07-24
owner: rejifald
---

# ADR-0002: Reconcile in-flight claims against GitHub

## Status

accepted (2026-07-24) — implemented this session (see the diff on
`src/reconcile.rs`, `src/github.rs`, `src/docker.rs`, `src/backend.rs`); live
validation recorded below. Prompted by issue #1 (jobs shown in progress locally
that had already finished on GitHub).

**Amended by [ADR-0017](0017-a-reclaim-asks-the-runner.md) (2026-09-17).** The
consequence below that reclaim is "provably safe under reassignment" did not hold.
GitHub's `busy` flag read a runner as idle for minutes while it ran a sibling's
job, and the flag was always stale by the time the kill landed. Reclaim killed
runners mid-job. A reclaim is now a request that the runner's owner grants only
after the container's process table and GitHub's own refusal to deregister a busy
runner both confirm it is idle. That also closes the parked-to-admitted race this
ADR accepted.

## Context

faber's reconcile loop keys every job it acts on into an in-memory **claim set**
(`inflight: job.id → Claim`). The claim does two jobs: it dedups a job across
poll cycles (so one job mints one runner, not one per tick while the runner
connects), and it is what the dashboard renders as "in flight".

Historically a claim was released in exactly **one** place: the owning provision
task removed it when its runner container exited. Nothing ever reconciled the
claim set against GitHub. So any job that reached a terminal state *without* its
claiming task's runner cleanly ending kept its claim indefinitely:

- The job **finished** (or was **cancelled**, e.g. a new push cancels superseded
  runs by concurrency group) but was served by a **different** runner.
- The job was served by a **sibling** runner. faber's JIT runners are **not
  job-pinned**: a runner minted for job A picks up whatever label-matching job
  GitHub hands it, which may be sibling B. So the claim's `job_id` is only the
  *provisioning label* — not a statement of what any runner is doing.

Observed live: a single burst left about twenty claims, about half of them already
`completed` on GitHub yet still claimed half an hour later. Two harms follow:

1. **Dishonest accounting.** The dashboard and the daemon's own self-model list
   work that is done. An operator can't tell real load from ghosts.
2. **Wasted capacity that compounds.** Docker admission is gated by the
   autolimiter's **memory budget**, not a fixed count. A stranded *parked* claim
   (task blocked in `limiter.admit`, no container yet) eventually admits, spawns a
   runner for a job that no longer exists, finds nothing to do, and **holds its
   memory reservation until the 3 h lifetime cap** — starving real jobs out of the
   budget. Each of the parked phantoms does this in turn.

Four forces shape the fix:

- **Releasing a claim wrongly is worse than the bug.** Release a still-live claim
  and faber double-provisions, or — for a running runner — aborts real work. The
  run scan is **single-page** (`per_page=100`, no pagination) and
  eventually-consistent, so "absent from this cycle's scan" is too weak a signal
  to release on.
- **faber has no view of job-pickup.** The runner is launched attached via
  `.status()`; faber learns only that it exited, never that it started a job.
- **But faber knows its runner's name.** It registers each JIT runner as
  `faber-{job_id}-{ts}` (a name it chooses), so it can look *itself* up in
  `GET /actions/runners` and read the authoritative `busy` flag.
- **Cleanup is explicit, not RAII.** Claim removal and container/network/volume
  teardown are statements a task runs on its way out — not `Drop` guards. So a task
  cannot simply be `abort()`ed from outside without leaking them.

## Decision

Reconcile the claim set against authoritative GitHub state **every cycle**,
**phase-split** by whether a claim has admitted (spawned a runner) yet, driven by
**cooperative cancellation**.

1. **Parked claim (no runner yet).** If it is still in the cycle's active scan it
   is live — leave it. Otherwise fetch the job directly (`GET /actions/jobs/{id}`):
   `completed` or `404` ⇒ the job is gone ⇒ reclaim. Still `queued`/`in_progress`
   (a later-page job the single-page scan missed) or a transient error ⇒ **keep** —
   a claim is never released unconfirmed. The authoritative fetch, not scan-absence,
   is the signal.

2. **Admitted claim (a runner exists).** **Ignore the job entirely** — under
   not-job-pinned assignment the runner may be executing a sibling. Key on the
   runner's own `busy` flag from `GET /actions/runners`, matched by the
   `faber-{job_id}-` name prefix: idle (`busy == false`, `status == "online"`)
   sustained past a **2-minute grace** ⇒ reclaim; `busy` ⇒ **keep**, so real
   (possibly reassigned) work is never aborted. A healthy JIT runner takes its job
   within seconds, so sustained idle means the job it was minted for is gone.

3. **Cooperative cancellation, task-owned cleanup.** Each `Claim` carries a
   `watch::Sender<bool>` cancel signal and an `admitted` flag. Reconcile only ever
   **signals** (`send_replace(true)`); it never touches the map internals or the
   container. The owning task, seeing the flag, exits through **its own** cleanup:
   parked ⇒ bail out of the admission wait before spawning anything; admitted ⇒ the
   **existing lifetime-cap kill + teardown path**, now also triggered by cancel.
   The claim is thus released in exactly one place (`release_claim`), by exactly one
   owner, and teardown always runs.

4. **Reclaim is local.** "Reclaim" means `docker kill` the idle container and free
   its claim + reservation. faber does **not** call `cancel_run`: the job is already
   terminal on GitHub (nothing to cancel), and `cancel_run` would abort the run's
   still-**live sibling** jobs.

The reclaim outcome is recorded as `reclaimed` (a clean reclaim), distinct from a
`failure` or a `timeout`, and is kept out of the autolimiter's learning (an idle
runner is not a representative memory sample).

## Consequences

- **Honest accounting restored.** Parked phantoms drop within a cycle of the job
  going terminal; the dashboard reflects real work.
- **Capacity reclaimed fast.** An admitted-but-idle runner is killed within the
  grace window (~2 min) instead of riding the 3 h cap, so doomed runners stop
  starving the memory budget.
- **Provably safe under reassignment.** Because an admitted runner is judged only
  by its own `busy` flag, faber never kills a runner doing work — including the
  common case of a runner whose *namesake* job is terminal but which is busy with a
  reassigned sibling.
- **New surface:** `Github::job()` (authoritative single-job fetch, `404 → None`)
  and `Github::list_runners()` (`name` + `busy` + `status`); `RunReport.reclaimed`;
  `IDLE_RECLAIM_GRACE`; a per-job idle-since timer. `admit()` is cancel-safe — it
  debits the ledger only on its synchronous success path, so dropping the future in
  a `select!` leaks no reservation.
- **A bounded race, accepted.** Reconcile may classify a claim *parked*, then it
  admits before the cancel lands; the run-phase cancel then kills a just-started
  runner without a `busy` re-check. The window is sub-second (a job fetch is
  ~100 ms; admitting + launching + registering + being assigned a job is seconds),
  the runner is not yet doing work, and this is strictly rarer than the pre-existing
  idle-runner leak (well under 1 % of runners). Left as-is rather than adding
  coupling.
- **API-call cost is proportional to churn.** A claim still in the active scan
  triggers no extra call; only claims around a terminal transition do (one job
  fetch each), and admitted claims share **one** `list_runners` call per repo per
  cycle. In steady state the reconcile pass makes no extra calls.
- **Deferred (recorded, not built) — stop the over-claim at the source.** This ADR
  *mops up* idle runners after the fact; it does not prevent minting one runner per
  queued job under not-job-pinned assignment. **Demand-based provisioning** —
  provision against net demand (`queued` minus already-idle registered runners) —
  would prevent the surplus, but it races GitHub's opaque assignment and is a
  rearchitecture of provisioning. Deferred; the idle-reclaim here makes the surplus
  cheap enough to live with meanwhile.

## Alternatives considered

- **Cheap scan-diff release** ("job absent from this cycle's active scan ⇒
  release"). Zero extra API calls, but the scan is single-page and
  eventually-consistent: a still-`queued` job on a later page, or a transient
  hiccup, reads as absent and would release a live claim, **double-provisioning**
  the job. Rejected — its failure mode is worse than the bug.
- **Release on namesake-job-terminal, ignoring `busy`.** Simpler, but
  reassignment (observed live) means a terminal-namesake runner may be doing real
  sibling work; this would abort it. Rejected — the `busy` flag is the only signal
  that survives not-job-pinned assignment.
- **External `abort()` + reconcile removes the claim.** Rejected: cleanup (claim
  removal, teardown) is explicit, not `Drop`-based, so aborting a task mid-await
  leaks its claim and its container/network/volume. Making it safe would mean
  converting all cleanup to `Drop` guards — a larger, riskier refactor of working
  code. Cooperative cancellation keeps the task the sole owner.
- **`cancel_run` to clear the job on GitHub as part of reclaim.** Rejected: the job
  is already terminal, and `cancel_run` cancels the *whole run*, taking out live
  sibling jobs.
- **Demand-based provisioning now** (the root-cause fix). Deferred, not rejected —
  see Consequences. Bigger change; races assignment; disproportionate to this fix.
- **Lower the global lifetime cap** so idle runners die sooner. Rejected: the 3 h
  cap exists to cover a legitimate maximum job duration; lowering it globally would
  kill long real jobs.

## Validation against real circumstances

Issue #1 was diagnosed against the live daemon, and the design was validated there
before and after implementation. Pass/fail criteria:

1. **Parked phantoms clear.** A burst that strands terminal-namesake parked claims
   drops them within 1–2 cycles, not after the 3 h cap. (A redeploy also clears the
   current backlog immediately, since the claim set is in-memory; `reconcile_claims`
   prevents recurrence.)
2. **Safe under reassignment.** An admitted runner whose namesake job is terminal
   but whose runner reports `busy == true` — the exact live pattern observed (a
   runner minted for a `completed` job, busy with a sibling) — is **never** killed.
3. **Idle reclaim.** An admitted runner reporting `busy == false` + `online` past
   the 2-minute grace is `docker kill`ed, its claim + reservation freed, recorded as
   `reclaimed` (not `failure`).
4. **No double-provision.** A still-`queued` job the single-page scan missed is
   never released; the audit shows no re-provision of a live job.

_Live results (2026-07-24, daemon rebuilt from this change and restarted):_

- **(1) Phantoms clear — PASS.** The reported pileup stood at about a dozen
  claims at redeploy time (about twenty at first observation, about half already
  `completed` on GitHub). The restart cleared the in-memory claim set to **0**;
  across subsequent cycles the daemon re-claimed **only** genuinely-active jobs —
  no stale claim reappeared.
- **(2) Safe under reassignment — PASS (live), with a concrete case.** A claim whose
  namesake job was `completed (failure)` but whose runner reported `busy == true`
  (up a quarter of an hour on a reassigned sibling) was **kept** across a minute or
  two of observation and never reclaimed — killing it would have aborted real
  work. Multiple other
  `busy == true` runners were likewise untouched, and an `offline` orphan left by
  the forced restart was correctly left alone (reclaim requires `online`).
- **Residual, by design — a *cosmetic* label staleness remains.** Because a claim is
  keyed on its provisioning `job_id`, an admitted runner busy on a *reassigned*
  sibling keeps showing its now-`completed` namesake id on the dashboard. This is
  **not** a phantom (the runner is doing real work and the capacity is not wasted) —
  only the stale *label* survives, and only the deferred runner-keyed-claims work
  removes it. The harm the issue reported (dead claims wasting budget) is gone.
- **(4) No double-provision — PASS (live).** Parked claims for still-`queued` jobs
  were kept (authoritative fetch, not scan-absence); no live job was re-provisioned.
- **(3) Idle reclaim past grace — decision logic unit-tested; live trigger not
  reproduced in-window.** The reconcile decision is split into pure functions
  (`parked_verdict`, `runner_is_idle`, `admitted_verdict`) so the phase-split,
  busy-gate, and grace window are covered deterministically — including the
  reassignment guard (`runner_is_idle` returns false for a busy terminal-namesake
  runner) and the exact grace boundary. The cooperative-cancel plumbing is covered
  by `reclaim_is_visible_to_a_late_subscriber` / `wait_cancelled_resolves_only_on_a_true_flag`.
  Live, the admitted branch's "keep busy" half was exercised; the *idle-reclaim*
  half needs an admitted runner idle past the grace — precisely the over-provisioning
  this fix *suppresses* (a runner minted for a now-done job usually gets a
  reassigned sibling and stays busy, so idle is a burst-tail case), so it did not
  recur in the observation window. It will surface as an audit `result: "reclaimed"`
  on the next natural over-provision.
