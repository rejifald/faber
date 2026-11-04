# faber autolimiter — design

Status: **agreed design (2026-07-21), implementation in progress.**

## Problem

faber caps in-flight runners with a single fixed `concurrency = 3` semaphore,
shared across *both* backends. A fixed count can't be optimal because jobs aren't
fungible — three Playwright builds and three lint jobs have wildly different
memory footprints. Three heavy jobs plus co-resident dev stacks can overcommit
the Docker Desktop VM and OOM/thrash; three trivial jobs leave the box idle.
The count is the wrong control variable.

## Model

Replace the fixed count with **memory-budget admission control**. The invariant we
hold is `Σ(per-job memory) ≤ budget`; **concurrency becomes emergent**, not a knob.

- **Memory is incompressible** — overcommit → OOM. It *must* gate admission.
- **CPU is compressible** — oversubscribe and the kernel time-slices; jobs run
  slower but nothing dies. So the autolimiter is **memory-only**; CPU stays
  over-committed and uncapped (the existing manual `cpus` knob remains available
  as an optional latency-smoother, but the autolimiter never sets or gates on it).

  **Superseded by [ADR-0019](adrs/0019-cpu-is-a-deadline-resource.md).** That
  held at the VM's first size, where memory alone already bounded concurrency
  to a few heavy jobs. Grown to about twice the memory but barely more cores,
  memory admits enough jobs that every one of them still sizing its worker pool
  to the full core count measurably saturates the VM and starts missing its own
  timeouts — a blown deadline is a failure "nothing dies" didn't count. CPU is
  now admitted, quota-enforced and learned the same way memory is; see *CPU
  admission* below.
- **Docker-only.** macOS has no cgroup memory limits for a plain process, so a
  native runner can't be `--memory`-capped or cheaply profiled. Native jobs keep a
  separate, simple `native_concurrency` count — the honest tool when you can
  neither cap nor measure. They eat *host* RAM (outside the VM), so they don't
  cross-account against the Docker budget.

## Pipeline

```
reconcile (20s GitHub poll) → gate (provenance) → CLAIM (inflight map, dedup)
     → ADMISSION CONTROLLER  [ Gate 1 ∧ Gate 2 ]  → launch runner + dind
     → read memory.peak at teardown → update class cap → release budget
```

Discovery/claim (reconcile) and launch (admission controller) are separate
concerns at separate cadences. Claimed-but-unadmitted jobs wait in the
controller's queue.

## The two gates

A job launches only if **both** pass. They guard distinct failure modes and are
kept separate rather than fused (fusing double-counts in-flight jobs, which sit in
the ledger *and* already depress live-available).

### Gate 1 — reservation ledger (feed-forward, faber-internal)

`Σ reservations(admitted) + next ≤ budget`

- Reservations are debited **at admission**, before a job ramps to its memory
  peak. This is what stops the thundering herd — a batch admitted together is
  counted immediately, not once they all hit their `pnpm build` peak.
- `budget = probed MemTotal − reserve`. `MemTotal` comes from
  `docker info --format '{{.MemTotal}}'`, re-probed periodically so a resize of
  the VM is tracked automatically.
- `reserve` (a configurable knob, **not** an absolute budget — a reserve stays
  meaningful across VM resizes) covers Docker Desktop's own overhead plus a
  **growth cushion** so a co-tenant (dev stack) can spin up without an instant
  OOM. It need only cover baseline + cushion, *not* the worst-case dev footprint —
  Gate 2 has the variable tail.
- Once the machine's share is declared, the reserve is **measured** as well:
  the larger of `reserve_mib` and the 24-hour peak of what the VM uses beyond
  faber's own containers (ADR-0021 §3, see *The machine's share* below).

### Gate 2 — live pressure (feedback, external-aware)

`VM MemAvailable ≥ next + headroom`

- Sees dev stacks and everything faber didn't launch — the ledger is blind to
  those.
- **Signal is `MemAvailable`, not `MemFree`.** The VM kernel uses most nominally
  "free" RAM as reclaimable page cache, so `MemFree` reads catastrophically low.
  `MemAvailable` is the kernel's own "can a new allocation have this without
  swapping" estimate — exactly the admission question.
- **Read via a throwaway probe container:** `/proc/meminfo` isn't namespaced, so
  any unlimited container sees the VM-wide numbers —
  `docker run --rm busybox grep MemAvailable /proc/meminfo`, once per admission
  decision. (Optimization if churn bites: one long-lived probe container +
  `docker exec`.)
- **Anti-flap band + freeze:** stop admitting below `headroom`; don't resume until
  above `headroom + band`. If available stays below the line for a sustained
  window, surface a **freeze** state on the panel (health → amber). This is the
  only home for "pause under sustained pressure" — hysteresis on Gate 2, not a
  separate mechanism.
- Gate 2 alone has ramp-lag (freshly-admitted jobs haven't allocated yet, so
  available is briefly overstated); it's only safe because Gate 1 backs it.

## Reservations are hard caps

A job's reservation is also its Docker `--memory` **cap**. This is the keystone:
without it, Gate 1 is fiction — one runaway job (a leaking test, a ballooning
`docker build`) blows the VM regardless of the ledger. Capping makes the ledger a
true upper bound, and inverts the failure mode in our favor: a job that needs more
than its cap **OOM-fails in isolation** (one job dies fast, visibly) instead of
diffusely dragging the VM and dev stacks into swap-thrash.

A job is **two** containers (runner + dind), capped independently (they carry
different loads — checkout/test vs `docker build`/services). A job's reservation is
`runner_cap + dind_cap`; the ledger counts the pair.

### The cap is sized for the cohort, the reservation for the class

Cap and reservation are the same number only if a runner is guaranteed to run the
job it was minted for. It isn't: a JIT runner is not job-pinned (see *Which class a
sample belongs to*), so GitHub may hand it any queued job whose `runs-on` labels it
satisfies. Sizing the containers for the minted-for class alone therefore runs job B
under the limit computed for job A, and when B is the heavier the kernel kills it
mid-step — measured at ~7% of mints across one live log, and at essentially *every*
mint during one short burst, with per-job reservations spanning about 4–10 GiB
(over a 2× spread). Downstream it reads as CI flakiness: the same commit passes or
fails run to run, and the kill surfaces variously as `exit 137` in a setup step,
`docker: unexpected EOF` on an image pull (dind dying during decompression), or
`lookup dind ... no such host` (a dead dind's name no longer resolving).

So the two numbers are split:

- **The container cap is the per-container max over the cohort** — every class seen
  queued in that repo within `COHORT_TTL` (7 days) whose labels the runner can serve.
  The set *accumulates* across discovery scans rather than being replaced by each
  one, and is rebuilt from the audit log on startup, because a runner is
  dispatchable for its whole idle life and not just for the instant it was minted:
  sizing it against the queue as it stood at `docker run` leaves every job queued
  afterwards uncovered, and across one live log some 200 runs executed a class
  absent from their launch cohort, about two-thirds of them OOM-killed. Whichever
  job GitHub picks, the container already fits it.
- **The reservation stays the minted-for class's own.** The ledger bounds the
  *aggregate* footprint, and dispatch is a permutation: every cohort job runs exactly
  once, so the total the cohort consumes doesn't depend on which runner takes which
  job. Reserving the ceiling per runner would instead count the single heaviest job
  once per cohort member — one 10 GiB class among six queued jobs would demand 60 GiB
  of a 15 GiB budget, say, and collapse concurrency to one runner — while protecting
  nothing extra, since only one container will actually hold that job.

What the split leaves exposed is an *ordering* effect: light-minted runners admitted
first, then handed the heavy job. That is what Gate 2's live `MemAvailable` reading,
`headroom_mib` and `reserve_mib` exist to absorb.

Two invariants keep this from re-opening older bugs. The cohort maxes over each
class's **stored** cap — already clamped by the escalation guard — never over a raw
`want`, so a flagged class cannot smuggle its refused want into its neighbours. And
an OOM is censored at the cap the container **actually ran under**, not at the
class's own: censoring at the class cap would "learn" a value the job has just been
shown to exceed, and OOM again next run.

The residual is a class this repo has *never* had queued within `COHORT_TTL` — a
brand-new job name, or one dormant longer than a week. Sizing can't cover that one,
so the mismatch log line names the shortfall explicitly instead of leaving an
unexplained OOM in the job log, and the retry sweep re-runs the job on faber's own
record of the kill ([ADR-0003](adrs/0003-auto-retry-faber-caused-failures.md)) — the
annotation GitHub publishes for it does not say "memory", and may not exist at all,
which is why faber no longer reads it.

### Elasticity — how a job "takes more when needed"

Not by bursting past its neighbors in the moment (that's memory overcommit, whose
one safety assumption — *uncorrelated* bursts — CI violates, since jobs march
through install→build→test in lockstep and peak together). Instead the **cap rises
to meet the job** across runs:

- **Margin** — cap = observed peak × `margin` (~1.4), so "a bit heavier than usual"
  is already inside.
- **Learning** — the cap tracks the job's own recent history (below).
- **Force-bump** — an actual OOM immediately raises that class's cap.

The trade accepted: a job that spikes above its learned cap on a single anomalous
run dies that run (then the cap adjusts) — a contained, predictable failure bought
in exchange for never thrashing the whole VM.

## Per-class caps & learning

Caps are **per-class**, keyed by `repo + workflow + job-name` (matrix cells are
distinct classes — exactly the granularity we want, since each cell has its own
footprint). A single global cap would re-create the original problem one level up:
one Playwright OOM would force every lint job to reserve 6 GiB, collapsing
concurrency for all.

### Which class a sample belongs to

Not the class of the job the runner was *minted* for. A JIT runner is **not
job-pinned**: faber mints it for a specific job, but GitHub dispatches whichever
queued job it likes to the freshly-registered runner, and the two come apart
routinely (a few consecutive live runs each executed some job other than the one
their runner was named for). Charging a run's measurements to the minted-for class
poisoned the learner across the board — a lint class absorbed an E2E job's
footprint, and a light job learned a runner peak of about 11 GiB it could not
plausibly use. Inflated caps then trip the escalation guard and exhaust the budget,
which is what parks every other claim.

Attribution therefore keys on GitHub's own `job.runner_name`, harvested from the
discovery scan each cycle (a job carries its runner from the moment it is
dispatched, so any run longer than one poll interval is captured before teardown).
A run whose executing job cannot be identified is **kept out of the learner**
entirely rather than charged to a guess — the same treatment as a lifetime-cap
kill, an idle reclaim, or an infra failure.

### Cap update rule — rolling window, raise-fast/lower-slow

Read `memory.peak` (cgroup v2) at teardown of **every** job, not just profiling
runs. `cap = rolling-max(last ~20 peaks for this class) × margin`.

- **Raise fast:** a bigger peak or an OOM bumps the cap immediately (protect
  first).
- **Lower slow:** downward moves are damped — only after a window of lower peaks
  sustains — so a cap is never shaved right before a job needs the room. The margin
  absorbs run-to-run variance.

This unifies the special cases: the **profiling run is the first sample**; an
**OOM is a censored sample** ("true peak ≥ cap", so bump rather than wait for the
window).

### Cold-start — profiling at a bounded ceiling

A brand-new class has no history. Rather than guess a default or eat an OOM,
profile it: run at a **generous bounded ceiling** and read the real peak.

- The ceiling is **per-container** (~4–5 GiB each). Not truly uncapped — a truly
  uncapped first run is the exact hazard the whole design avoids (incompressible,
  non-sheddable; a leaking first run eats the VM while you "profile" it, and Gate 2
  can't shed it). Per-container keeps a profiling job's reservation (~8–10 GiB)
  inside the budget with room for learned small jobs alongside.
- A class that genuinely needs more than the ceiling OOMs → force-bump. So
  profiling and force-bump are **one mechanism from two directions**: profiling
  learns the peak *under* the ceiling, force-bump learns it *at* the ceiling.
- First-ever rollout has a one-time dip (everything profiles ~2-at-a-time until
  each class has run once), then self-heals. No bootstrap is possible — the old
  audit recorded duration, never memory. Best-fit admission softens it: once a
  class is learned, its small cap lets it flow *alongside* a profiling job.

### On OOM — remember-only (v1)

The class cap is bumped and the OOM recorded, but the failed job is **not**
auto-rerun. faber stays a pure provisioner (it provisions runners; it does not
drive run lifecycle — no `actions:write`, no re-run API, no retry loop-guards in
v1). The developer sees an honest, self-healing failure: a re-run (routine muscle
memory) provisions at the bigger cap and passes. Auto-retry is a clean,
well-scoped later addition. Remember-only also avoids *masking* a genuinely leaking
job by silently feeding it bigger caps.

### The shadow p75 reservation (ADR-0033, rollout step 1)

[ADR-0033](adrs/0033-a-reservation-is-the-typical-peak-the-cap-stays-the-worst.md)
proposes debiting a smaller number than the cap. This step **measures** it, on
every host and in every mode; [rollout step 2](#reserve--p75-adr-0033-rollout-step-2)
below is the `[autolimit] reserve` switch that debits it.

Per class and container faber computes the reservation `"p75"` would debit:

```text
reserve_p75 = min(cap, p75(window) × margin)
```

with `p75` the nearest-rank 75th percentile of the rolling window. It equals the
cap while the window is not full, while the window holds a censored (OOM) sample,
and while the class is pinned. Each window sample carries a censored flag, and the
startup replay sets it from the audit line's `oom_runner` / `oom_dind` (a line from
before the split falls back to `oom` for both), so a restart does not forget an OOM.
The figures are rebuilt from the audit log with the caps; there is no second store.

The state view adds, all additively (an older client ignores them):

| Where | Field | Meaning |
|---|---|---|
| `budget` | `reserved_p75_mib` | what the running jobs would hold under p75, beside `reserved_mib` |
| `budget.classes[]` | `reserve_p75_runner_mib`, `reserve_p75_dind_mib`, `reserve_p75_mib` | the would-be reservation, beside `runner_mib` / `dind_mib` |
| `budget.classes[]` | `p75_runs`, `p75_trips` | finished runs judged against a p75 reservation below the cap, and how many peaked above it or were OOM-killed |

`p75_trips / p75_runs` is how often `"p75"` would have been outgrown. A run is
judged only where a p75 reservation was below the cap, since otherwise the two
modes debit alike. The web dashboard shows both columns and the budget-level total;
the menu-bar app does not yet.

**Host OOM versus cap breach (ADR-0033 §5).** faber latches each container's
hierarchical `memory.events` (`docker exec ... cat /sys/fs/cgroup/memory.events`,
read on the same cadence as the other cgroup counters, and a last time for dind
before teardown). `oom_kill` raised with `oom` not raised means a process was
killed with no limit in the subtree breached: the VM-wide OOM killer, not the
container's cap. That writes an audit line of its own `kind`, `host_oom`, naming the
counters. Under `reserve = "max"` the learner is **unchanged**: such a run is still
censored and bumped exactly as before, and the `host_oom` line records whether it
was. Under `"p75"` it is kept out of the learner (step 2, below). Gap: the runner's
cgroup is gone once the runner exits, so a kill in the last sampling interval
before exit is not seen, and a runner with no reading at all is never classified
(it stays a class OOM, as today). dind gets a final read just before teardown.

### `priority_repos` (ADR-0036)

```toml
[autolimit]
priority_repos = ["acme/tooling"]   # owner/repo, case-insensitive; absent or empty: strict FIFO
```

A parked Docker claim from a listed repo queues ahead of every other repo's.
Within each group admission stays first-come-first-served, so the "head" the
set-aside protects is the oldest claim of the highest group present. Every gate
(memory, CPU, disk, hold, hard ceiling) still applies to it, nothing already
running is preempted, and native jobs are unaffected. A claim set aside around a
priority head says so (`set aside for a priority claim`). **Absent or empty is
exactly today's behaviour.** The key is host-only: `.faber/config.toml` refuses
it, because a repo must not be able to raise its own priority. It applies on
restart. A steady stream from a listed repo can delay everyone else, so keep the
list to small, low-volume repos.

### `reserve = "p75"` (ADR-0033, rollout step 2)

```toml
[autolimit]
reserve = "p75"   # absent or "max": today's behaviour
```

An enum with two values. **Absent or `"max"` is exactly today's behaviour**:
admission debits each class's cap, nothing is frozen for memory, and the learner
is unchanged. Any other value is rejected by `faber check-config` and the settings
save; a daemon that boots on one warns and runs as `"max"`. It applies on restart,
like `cpu`. The settings page shows it as an optional select: blank reads
"unstated", and a save writes `"max"` explicitly.

Under `"p75"`:

- **The ledger debits the p75 reservation** above (`min(cap, p75 × margin)`, equal
  to the cap while the window is not full, holds an OOM, or the class is pinned):
  Gate 1, the head set-aside, and the `org.faber.reserved-mib` label, so an
  adoption re-charges what admission debited. The container's `--memory` limit is
  still the cap — the cohort max, with the escalation guard and the OOM force-bump
  unchanged. Gate 2 still asks for room for the cap (`available ≥ cap + headroom`):
  the conservative reading of an ADR that names only the debits.
- **The memory freeze.** When `MemAvailable` falls below `headroom_mib / 2`
  (768 MiB at the default), faber freezes the newest running Docker job with
  `docker pause`, dind included, deciding on every memory-tick reading (8 s); at
  most two are frozen for memory. Above `headroom_mib + band_mib` they thaw, oldest
  first. A failed reading neither freezes nor thaws. A frozen job GitHub has
  finished is released, as for disk. It is the disk floor's machinery
  (`src/freeze.rs`), not a second pause path. **Only the freeze is gated**: the
  memory thaw runs on every Docker host in every mode, so a job frozen before the
  fallback tripped, or before an operator went back to `"max"`, still comes back.
- **Each freeze records why** (`disk` or `memory`), and each loop thaws only its
  own; the caps of two are per reason. Docker cannot label a running container, so
  the reason lives in the process and on the `freeze` audit line (`reason`), which
  a restart reads back. A paused container with no recorded reason is the disk
  loop's, as before. `/api/state` shows it as `inflight[].frozen.reason`.
- **The fallback to max.** A memory freeze, a host OOM, or a failed memory probe
  right after a reading below `headroom_mib` trips it: the host debits caps until
  the cool-off ends, then re-arms on its own. The first trip cools off 24 h; each
  further trip within 7 armed days of the last doubles it (48 h, 96 h, up to 7
  days); 7 armed days without a trip reset it to 24 h. **Armed time is time with
  p75 in force**, counted on the memory tick (at most 60 s per gap), so a
  cool-off, time under `"max"` and the daemon being down never count. Every trip
  begins a squeeze, which lasts until a reading above `headroom_mib + band_mib`:
  through it the memory freeze stays possible even while cooling off (a host-OOM
  trip included), and nothing in it extends the cool-off. A squeeze that outlives
  its cool-off trips again. Every trip and re-arm is an audit line (`kind:
  "reserve"`, `action: "trip" | "rearm" | "armed"`, `reason`, `cooloff_ms`,
  `until_ms`, `armed_ms`); armed time is checkpointed every 6 armed hours, and a
  restart rebuilds the fallback from these lines. A restart that finds a job still
  frozen for memory resumes its squeeze.
- **A host OOM is not a class sample** (§5). A container whose latched
  hierarchical `memory.events` shows `oom_kill` raised with `oom` not raised has its
  OOM flag and truncated peak dropped; a run whose only OOM signal was a host OOM
  is kept out of the learner entirely (its outcome line carries no `class`, so a
  restart does not re-learn it). A raised `oom` — the runner's own or dind's local
  one — stays a class sample. A runner with no reading is a class OOM, as today.
  Under `"max"` none of this applies.

`/api/state` adds `budget.reserve`: `mode` (configured), `unknown` (an unknown
configured value), `in_force` (`"max"` while cooling off), `cooling`, `squeeze`,
`freeze_armed`, `trips_7d`, `last_trip`, `next_rearm_ms`, `armed_ms`, `next_cooloff_ms` and the
recent `trips`. The web dashboard shows it under the budget line; the menu-bar app
does not yet.

### Recommendations & escalation

- **Auto-apply below threshold:** a learned cap is applied immediately (class
  protected + packs well), surfaced as "faber learned class X → N GiB, tune if you
  disagree."
- **Escalate above threshold:** a recommendation over ~50% of budget does **not**
  auto-apply — it holds the class at a safe interim and **warns**. A job wanting
  half the box is either a real monster that serializes CI or a leak you'd rather
  fix than feed. This is where "warn if requested resources are too high" lands:
  the warning is the gate between *auto-apply* (no friction) and *human-confirm*.
- **The interim is a clamp, never a floor:** each container is held at
  `min(want, ceiling)`, so escalating a class can only ever *lower* its
  reservation. The first implementation assigned the ceiling to *both* containers,
  which inflated exactly the classes it flagged — a dind that had never exceeded
  a few hundred MiB across about a thousand runs was handed the full profiling
  ceiling, so a flagged class reserved twice that ceiling, most of the budget: for
  several classes *more* than the want that tripped the guard, enough that one
  such job held the whole pool and every other claim parked behind it. Startup
  replay (`seed`) goes through the same guard, so a restart cannot seed a cap no
  live run would have been granted.
- **An OOM or a peak above the ceiling lifts the ceiling to the threshold**
  ([ADR-0053](adrs/0053-an-oom-at-the-profiling-ceiling-grows-the-cap.md)). The
  profiling ceiling clamps a want built from peaks below it (peak × margin). A
  container whose window holds an OOM, or a measured peak above the ceiling, has
  proven that ceiling too low. Held there, it is killed on every run. Such a peak
  is reachable only under a cap that has already grown, so counting it keeps a
  grown cap from dropping back to the ceiling once its OOMs leave the window. Its
  ceiling is the escalation threshold instead, less the other container's cap, so
  runner + dind stays within the threshold. When both containers have that proof,
  the threshold is shared in proportion to their wants. Growth
  never drops a cap below the profiling ceiling. A want past the threshold is held
  there, `warned`, and raised to the owner in `budget.oom_held`, which the popover
  and the dashboard show as a red attention row; the entry carries the caps a pin
  needs, and the popover row's Pin sets them. That happens only while an OOM is
  among the class's last 5 runs; an older one still holds the cap up but asks
  nothing. A pin overrides all of it. Replay runs before the budget is measured,
  so it can only clamp; the first measured budget re-evaluates every class.

## Admission controller

A **single serialized arbiter** owns the ledger and a queue of
claimed-but-unadmitted jobs — the sole thing that evaluates gates and debits, so no
races (per-task self-gating would let two jobs both see room and both admit). It
replaces the `orch.permits` semaphore.

- Wakes on three signals: **job-complete** (frees ledger), **memory-tick**
  (~5–10 s Gate 2 re-probe, so a dev stack shutting down admits a waiter without
  needing a job to finish), and **new-claim**.
- **FIFO with a head set-aside.** The original design admitted any queued job that
  fit both gates (best-fit, maximizing throughput), noting the risk that "a very
  large job could wait under a continuous small-job stream" and deferring
  anti-starvation aging "until it bites". It bit: 9 GiB claims sat parked for
  over an hour while 1 GiB claims were served continuously, because every waiter
  re-raced the gates each second and the large one never found a big enough gap.
  Admission is now ordered by a monotonic ticket, and Gate 1 refuses anyone whose
  reservation would encroach on what the **longest-waiting** caller needs. Freed
  memory therefore accumulates toward the head instead of being taken by whoever
  happens to fit, while juniors still fill any room beyond the head's set-aside, so
  utilization stays high. A head that cannot fit the whole budget is *not* set
  aside — protecting an unsatisfiable request would wedge the pool for everyone.
- **The operator's hold comes first.** Ahead of every gate below sits the hold (see
  *Holding admission*): while it is set, nothing new is admitted, whatever the
  ledger says.
- **Then pause.** Right behind the hold sits pause (see *Pausing*): while faber is
  paused nothing is admitted either, and a native claim does not take a slot.
- **A refusal is never silent.** Each one names the first gate that said no, with
  the numbers it said no on, and the parked claim publishes it (see *Why a claim
  is parked*).

## Holding admission

To stop Docker admission — a VM resize is pending, Docker Desktop is about to be
upgraded — an operator **holds** it, with a reason:

- `POST /api/hold` with `{"reason": "…"}` (trimmed; blank or over 200 characters
  is a `400`), or **Hold Docker…** on the dashboard or in the tray. Holding again
  replaces the reason and keeps the original *since*.
- `POST /api/unhold`, or **Release**, lifts it. Parked claims re-check at once.

A hold stops **new Docker admissions** and nothing else. Running and adopted
containers finish, native jobs are untouched, and the reconcile loop still takes
claims — they park and say they are held. It is deliberate, like a pause, so
`/api/state` stays `healthy`; unlike a pause (see *Pausing*) it has a reason, a
date, and it **persists**: `overrides.json` carries `hold: {reason, since_ms}`, and
every boot while held logs a warning naming both and how to release it.
`budget.hold` in `/api/state` is the hold, or `null`.

It replaces the zero ceiling. Before holds, `hard_ceiling = 0` was the only way to
stop admission; it said nothing about why, read as a healthy host, and survived
restarts — on one host for several hours, while every claim sat parked with no
reason given and the stall was diagnosed as something else. So:

- `POST /api/config` refuses `hard_ceiling: 0` (the whole update, before any field
  applies) and points at `POST /api/hold`; `faber check-config` and the settings
  page already refuse a zero in `config.toml`.
- An `overrides.json` that already carries a zero is read at boot as the hold it
  stood for, dated by the file's mtime, and the config file's own ceiling applies.
  Nothing is written back until the next save.
- While held, the legacy numeric `hard_ceiling` in `overrides.json` is written as
  `0` — the one key a binary from before holds reads — so a rollback keeps
  admission held. The true setting stays in `ceiling`.

## Pausing

Pause is the other stop, wider and shorter than a hold
([ADR-0023](adrs/0023-pause-stops-what-starts.md)). Two things set it: the operator
(`POST /api/pause`, or **Pause** on the dashboard or in the tray) and the janitor,
for the length of a reclamation (ADR-0007). While faber is paused **nothing new
starts**, Docker or native: the loop claims no jobs, a parked Docker claim stays
parked behind the `paused` gate, and a native claim waits the pause out before it
takes a slot. Running jobs finish. `POST /api/resume`, or **Resume**, lets every
parked claim go at once. A pause lives only in memory, so a restart comes back
unpaused.

The janitor pauses only a faber nobody has paused, and lifts only its own pause. A
pause the operator sets during a reclamation replaces the janitor's and outlives
it; the operator's resume lifts either. If a reclamation frees nothing, the janitor
doesn't run another, and so doesn't pause again, until free space has dropped a
further GiB or the sweep interval has passed (ADR-0024).

A paused host keeps its parked claims, so the in-flight list never empties while
jobs are queued. To empty a host, drain it (`POST /api/drain`): that cancels the
parked claims and waits only for the admitted ones.

## Why a claim is parked

Every in-flight claim in `/api/state` carries `waiting`: `null` once admitted,
otherwise `{"gate": …, "detail": …}` — the gate holding it and a line saying why,
refreshed at every retry. Admission's gates, in the order they are checked:

| `gate` | Meaning |
|---|---|
| `held` | The operator's hold, with its reason. |
| `paused` | faber is paused — by the operator, or by the janitor for the length of a reclamation; the detail says which. |
| `machine-pressure` | The machine itself reports memory pressure `warn` or `critical` (ADR-0021 §2c; macOS, once `[host] keep_memory_gib` is above 0). With no Docker job admitted, one is let through anyway (#160), and the detail names a Docker VM larger than the share. |
| `swap` | The machine's swap use grew by 1 GiB within 5 minutes, and has not yet been flat for 5 (ADR-0032 §3; same condition). |
| `docker` | Gate 0: the availability probe has missed enough times that Docker counts as down. |
| `disk` | The disk gate: free space on the Docker volume, less what the Docker jobs still held (admitted in the last ten minutes and not yet ended) were charged and less this job's own charge (10 GiB with dind, 3 GiB without), is under `[host] disk_admission_floor_gib` (ADR-0024, ADR-0032; only when that floor is set). The detail names the job's charge. |
| `ceiling` | The hard ceiling: *N of N Docker jobs running*. |
| `adoption` | Gate 0b: containers from before the restart are still being enumerated (ADR-0011). |
| `memory` | Gate 1: the reservation does not fit the budget — naming the set-aside when it is the queue head's room that blocks, and a budget not measured yet as such. |
| `cpu` | CPU Gate 1, the same in cores (`cpu = "auto"` only). |
| `vm-memory` | Gate 2: live `MemAvailable` too low for this job, or frozen under pressure until it recovers. |
| `cpu-pressure` | CPU Gate 2: VM CPU pressure at or above the pause threshold (`cpu = "auto"` only). |

A claim can also wait before it reaches admission: `github` while
`generate-jitconfig` keeps answering 5xx — after `n` in a row the next mint waits
10 s × 2ⁿ⁻¹, at most 5 minutes, one claim per wait, and a success clears it
([#89](https://github.com/rejifald/faber-legacy/issues/89)) — `image` while ADR-0008 resolves
its runner image (or builds a variant), `pool` while ADR-0009 holds it for the
repo's runner-image pool to drain, and `native` while every native runner slot is
busy. A native claim reports `paused` too, while it waits out a pause, and
`machine-pressure` or `swap` while it waits out the machine.

### Which gate bound it (issue #85)

The reason above is what a claim is waiting on *now*. A claim also keeps time at
each gate it parks at, so once it is admitted faber knows which one held it
longest — its **binding gate**:

- **The audit log.** Every `outcome` line of a job that parked carries
  `wait_gate`, the binding gate, and `wait_gates`, the milliseconds spent at each
  (`{"image": 800, "memory": 412000}`). A job that never parked carries neither.
  The dashboard's *Recent runs* names the gate beside the time parked.
- **Per-gate counters.** `budget.gates` in `/api/state` keeps, for every gate a
  claim has parked at since faber started: `parked_now`, the claims parked there
  right now; `parks`, how many times a claim arrived there; `parked_ms`, the time
  spent there by claims that have since moved on; and `binding`, how many admitted
  jobs it bound. The dashboard shows them as *What parked jobs*, the gate holding
  jobs longest first. A gate nothing has parked at is absent. The counters live in
  memory and start again at a restart.

Together they answer the question the parked reason cannot: which limit is
actually binding on this host — the memory ledger, live `MemAvailable`, CPU, the
hard ceiling, disk, the machine's pressure or swap — and so which knob would buy
throughput.

## CPU admission (ADR-0019)

CPU is admitted, enforced and learned the same way memory is, on a design
[ADR-0019](adrs/0019-cpu-is-a-deadline-resource.md) lays out in full. This
section is the terse version, kept in sync with it.

- **Shares.** Every class carries a CPU share in millicores for each
  container, the same runner/dind split as memory. Admission adds a second sum
  alongside Gate 1's: `Σ(admitted shares) + next ≤ VM cores × cpu_overcommit −
  reserve_cpus`, checked against the same monotonic-ticket FIFO and head
  set-aside above — one queue, both dimensions. A share above the whole budget
  is clamped rather than refused, so it can still run alone instead of
  wedging the queue; unlike a memory want, a CPU want can be shrunk safely,
  because going over is a slowdown rather than a kill.
- **Quota.** The runner container runs under `--cpus` set to the cohort's
  largest runner share — the same cohort-vs-class split memory uses (*the cap
  is sized for the cohort, the reservation for the class*, above). Node's
  `os.availableParallelism()` honours the cgroup quota, so pool-sizing tools
  (Vitest among them) shrink automatically; `nproc` / `os.cpus()` still report
  the host's full core count, so a tool reading those over-spawns but stays
  inside the quota, throttled rather than let loose on a neighbour's cores.
  dind's share is reserved but not quota-enforced — it hosts service
  containers, not a sized worker pool.
- **Learning.** Sampled from cgroup `cpu.stat` during the run: peak and
  average usage, plus the throttled share of CFS periods. ≥10% throttled
  periods is a censored sample (true need ≥ quota), recorded as `quota × 1.5`
  — the same ×1.5 force-bump memory uses on an OOM. Share = rolling max ×
  margin, floored (1 core runner / 0.1 dind), clamped at `profiling_cpus`,
  which is both the cold-start share and the unattended escalation ceiling —
  one knob where memory uses two. CPU is sampled, learned and audited in
  every mode, `off` included, so flipping to `auto` starts from history
  rather than a cold profiling pass.
- **Pressure gate.** Admission pauses while `/proc/pressure/cpu`'s `some
  avg60` is ≥ `cpu_pause_pressure`, resumes at ≤ `cpu_resume_pressure` — Gate
  2's headroom/band shape, read by the same probe container that already
  reads `MemAvailable`. Fails open with no PSI. Catches load faber never
  admitted (a co-tenant dev stack) and a share that's simply wrong.
- **Config.** `[autolimit] cpu = "auto" | "off"`; absent is `unstated`, which
  behaves as `off` but is warned at boot and shown as its own state — the
  same grant shape as `dind`
  ([ADR-0013](adrs/0013-docker-in-docker-is-a-grant-not-a-default.md)),
  because turning this on tightens admission and reaches follower hosts
  unattended. Knobs: `cpu_overcommit` (1.25), `reserve_cpus` (1.0),
  `profiling_cpus` (4.0 cores), `cpu_pause_pressure` (40%),
  `cpu_resume_pressure` (25%) — restart-applied, not live-tunable.
  `hard_ceiling` gains the same three-state shape, `"auto" | "off" | N`, where
  `auto` is faber's own pick (a constant 4 for now) and stays live-tunable.
- **Modes.** `off` (today's behaviour, and what an absent key behaves as until
  a release flips the default): CPU is measured and learned but never gates or
  caps admission. `auto`:
  shares are reserved, `--cpus` is enforced, and the pressure gate is live.

## Disk admission (ADR-0024)

Memory and CPU say nothing about disk. In September 2026 they admitted a handful of
jobs into a Docker volume with little room left, and the jobs filled it until
containerd's own writes failed.
[ADR-0024](adrs/0024-a-full-disk-stops-admission-not-docker.md) lays out the
design. Here is the short version.

- **The floor.** `[host] disk_admission_floor_gib`: while the volume backing Docker
  has less than this free, no Docker job starts, and parked claims wait at the
  `disk` gate. Native admission is not gated. The floor may not sit above
  `disk_floor_gib`, where reclamation starts. `faber check-config` and the
  settings save refuse that combination, and at boot it only draws a warning.
- **The ramp.** A job writes most of its disk in its first minutes, and a burst of
  admissions all see the same free-space figure. So a Docker job is charged
  against the reading from its own admission until 10 minutes after it or until
  the job ends, whichever is sooner ([#161](https://github.com/rejifald/faber-legacy/issues/161)), and is
  admitted only if free space stays at or above the floor once its own charge and
  the window's are taken off
  ([ADR-0032](adrs/0032-the-disk-gate-charges-the-job-it-admits.md) §1). The
  charge is 10 GiB for a job whose profile grants dind (`unstated` included) and
  3 GiB for any other: over a 10 GiB floor a dind job needs 20 GiB free and a
  light one 13. A parked claim says which charge it waits on, and so does
  `budget.disk.charge_mib`. The charge goes back with the job's memory when its
  containers are torn down, and a provision that ends before any container starts
  gives it back the same way
  ([#89](https://github.com/rejifald/faber-legacy/issues/89),
  [#161](https://github.com/rejifald/faber-legacy/issues/161)).
- **The critical floor.** Below 3 GiB free, faber freezes the most recently
  admitted running Docker job with `docker pause` (the runner, then its dind), and
  one more per 20 s tick while free space stays below 3 GiB. Above 6 GiB it thaws
  them, oldest first, one per tick, unpausing dind before the runner. A job whose
  pause or unpause fails is passed over for three ticks, so the next one is acted
  on. At most 2 jobs are frozen at once. A frozen job whose GitHub job is over —
  GitHub fails it about 10 minutes into the freeze — is thawed whatever the disk
  reads, so its runner exits and its disk is freed, and it is never frozen again.
  A frozen job is still running — admitted, reserved, shown with `frozen` in
  `/api/state`, with `frozen.thaw_failure` once a thaw of it has failed — and each
  freeze and thaw is audited as `kind: "freeze"`. It freezes only where the floor
  is stated and non-zero; with the floor off it still thaws any job it finds
  frozen, which on every Docker host includes a faber runner paused by hand
  (ADR-0032 §2, #111).
- **The reading.** A loop of its own runs `df` on the Docker volume every 10 s,
  independent of any Docker call. A reading that failed or is over 2 minutes old
  is no evidence, and the gate stays open. `/api/state` shows it as `budget.disk`,
  measured in every mode.
- **Two volumes on a macOS SYSTEM host** ([#410](https://github.com/rejifald/faber-legacy/issues/410)).
  There `df` sees the Mac's volume holding the VM disks, while Docker's data root
  is inside the Linux VM, on its own fixed-size disk. So the same loop, while
  Docker answers, also reads that disk: `df -kP /` in a throwaway container from
  `probe_image`, never pulled. The Docker gate, the critical floor, the janitor's
  reclaim pass, a variant build and a first git-mirror clone act on the tighter of
  the two. `budget.disk.volume` names it (`docker`, `mac_vms` or `linux_vm`), as
  does a parked claim's detail; `host_free_mib` and `vm_free_mib` show each. A VM
  disk that cannot be read is not acted on: the gate falls back to the Mac's
  volume, `budget.disk.vm_unreadable` says why, and the janitor raises a
  `vm-disk` fault. macOS VM jobs and revival weigh the Mac's volume alone.
- **Revival waits for room.** With the floor set, faber does not restart a wedged
  Docker onto a disk below it; the Docker fault says why. In every mode, a restart
  counts as having worked only once Docker has then stayed up for
  `revive_cooldown_secs`. `revive_unable_to_start_secs`, once a fast path for
  Docker Desktop's own "unable to start" error
  ([#69](https://github.com/rejifald/faber-legacy/issues/69)), has been ignored
  since ADR-0026: faber no longer restarts Docker Desktop. A file that sets it still
  boots, with a warning.
- **Three states.** An absent key is *unstated*: no gate, which is the behaviour
  before ADR-0024. It is warned about at every boot, the dashboard shows it as
  `disk gate unstated`, and the first settings save writes `0`. `0` means off,
  stated, and any other number is the floor.

## The machine's share (ADR-0021)

Every budget above is sized from `docker info`. On macOS that is a VM carved out
of the operator's Mac, which never gives memory back once the guest has touched
it, so the VM's size is the Mac's only memory reservation.
[ADR-0021](adrs/0021-the-machine-share-is-declared-not-left-over.md) adds the
machine itself. Here is the short version.

- **The reading.** A loop of its own reads the machine every 10 s, on every host:
  `hw.memsize`, `hw.ncpu` (with the performance/efficiency split),
  `kern.memorystatus_vm_pressure_level`, `vm.swapusage` and
  `vm.compressor_bytes_used` on macOS; `/proc/meminfo`, the process's CPU affinity
  and `/proc/pressure/memory` on Linux. `/api/state` shows it as `budget.machine`,
  with the VM's size and what it leaves the machine, in every mode. A VM
  boundary is the daemon having fewer bytes or cores than the machine, whatever
  the platform.
- **The share.** `[host] keep_memory_gib` and `keep_cpus` are what the Docker VM
  and CI must leave the machine. Gate 1 plans against
  `min(MemTotal, machine RAM − keep_memory_gib)` and the CPU budget against
  `min(NCPU, machine cores − keep_cpus)`. For CPU that is a real reservation; for
  memory it limits CI's share of the VM, not the VM's footprint.
- **The VM check.** A VM larger than the share leaves CI is warned about at boot
  and on every re-probe, shown on the dashboard and in the tray, and worded as
  its fix, for example "set [linux_vm] memory_mib to 16384 or less". faber never
  resizes the VM.
- **The pressure gate.** With `keep_memory_gib` above 0, nothing new starts —
  Docker or native — while macOS reports `warn` or `critical`, or while swap use
  has grown by 1 GiB within 5 minutes (ADR-0032 §3). It resumes at `normal`, and
  once swap has been flat (risen less than 256 MiB) for 5 minutes. On Linux the
  level is not gated on until thresholds are chosen from a Linux host; growing
  swap is. A reading over 2 minutes old holds nothing shut.
- **Three states.** Absent is *unstated*: no share and no pressure gate, the
  behaviour before ADR-0021, warned about at every boot with the measured split
  (e.g. "the Docker VM holds 20 of this machine's 32 GiB, leaving it 12"), shown as
  `unstated`, and written as `0` by the first settings save. `0` is stated and
  changes nothing. A share not smaller than the machine is refused by
  `faber check-config` and the settings save. At boot it is too large to plan
  within, so planning ignores it with a warning, while the pressure and swap
  gates still apply.
- **The measured reserve** (§3). With `keep_memory_gib` above 0, Gate 1 holds
  back the larger of `reserve_mib` and the co-tenants' peak over the last 24
  hours, rather than `reserve_mib` alone. The co-tenant figure is the VM's
  `MemTotal − MemAvailable` less the working set of faber's own containers, found
  by their `org.faber.owned` label and never by name. So it counts everything
  else, whoever started it: other stacks, Docker Desktop itself, the kernel.
  - **Sampling.** Once a minute, on a loop of its own. faber's containers are
    listed before and after, and their memory is read with `docker stats` before
    and after `MemAvailable`, each at the larger of its two readings. A sample is
    dropped if the listing changed in between, if one of faber's containers has
    no reading, or if faber was building a runner-image variant (ADR-0008) at any
    point during it, since a build's memory is in no container faber can list. A job that exits or frees memory mid-sample would otherwise be
    charged to the co-tenants, and the peak would hold that error for a day.
  - **The window.** Hour buckets, each keeping its own peak: at most 25 numbers.
    It is written to `cotenant.json` in faber's state directory whenever it
    changes, and read back at startup, so a restart keeps the day's peak.
  - **Only ever more.** `reserve_mib` stays the floor and the operator's override.
    An empty window — a first boot, a lost or unreadable file — holds back
    `reserve_mib`, which is the behaviour from before §3, never an optimistic
    zero. A daemon that overstates faber's working set can bring the figure down
    to zero at most, which again leaves `reserve_mib`.
  - **Measured in every mode.** `budget.cotenant` in `/api/state` shows the latest
    sample, the peak, how many of the 24 hours it covers, and the reserve in force
    (`mode` is `measured` or `floor`). The dashboard shows the figure even while
    the share is unstated, marked as not held back. A reserve that moves by
    512 MiB or more is logged, and one restored above the floor at startup is
    warned about.
  - **A plan the reserve eats.** When the reserve takes the whole planned memory,
    the budget is 1 MiB, not 0, so a parked claim says the budget is too small
    rather than that it was never measured.

## Persistence — the audit log is the substrate

Add `mem_peak_mib` + `oom` + the class-key to the `outcome` audit record. On
startup, replay the recent tail to rebuild each class's rolling window. No second
store to keep coherent, and caps become **auditable** — "why is this class at
5 GiB?" is answerable from its last 20 peaks. Derived caps may be cached in
`overrides.json` for fast cold-start, but the audit stays authoritative.

## Control surface

- **Always-on**, no fixed-N fallback mode (that just preserves the knob we're
  removing and doubles the code paths).
- **Concurrency slider → readout:** budget utilization (e.g. `12.3 / 15 GiB reserved ·
  4 Docker jobs`) + the existing sparkline. You stop *setting* concurrency and
  start *watching* it.
- **Per-class `pin`** = the manual override: pin a class to an operator-set cap the
  learner won't touch. Un-pinned = auto. Finer than the old global knob, not
  coarser.
- **Two real knobs:** the `native_concurrency` count, and global budget settings
  (reserve / headroom / margin / profiling ceiling).
- **Limits panel → per-class table:** class → learned cap (runner + dind) → status
  (`profiling` / `learned` / `warned` / `pinned`), with pin + acknowledge-warning
  actions.
- **Optional** hard Docker-concurrency ceiling ("never more than N even if budget
  allows"), **default off** (= pure budget). At least 1: stopping admission is a
  hold, not a ceiling of zero (see *Holding admission*).
- **Hold / Release** — stop and restart Docker admission, with a reason that is
  shown everywhere the hold is.
- **Pause / Resume** — stop starting anything, Docker or native, for now. Not
  persisted, and it carries no reason.

## Tunables (defaults)

| Knob | Default | Notes |
|---|---|---|
| `reserve_mib` | 5120 | → a ~15 GiB budget on a 20 GiB VM, for example; the floor under the measured co-tenant peak once `keep_memory_gib` > 0 |
| `gate2_headroom_mib` | 1536 | slack before Gate 2 blocks |
| `gate2_band_mib` | 1024 | resume-admitting threshold above headroom |
| `profiling_cap_mib` | 4096–5120 / container | bounded profiling ceiling |
| `margin` | 1.4 | rolling-max → cap |
| `escalation_frac` | 0.5 | of budget; auto-apply → warn+hold above |
| `window` | 20 | runs per class |
| force-bump step | ×1.5 (→ ceiling → warn) | OOM is censored at the cap |
| `native_concurrency` | 2 | non-budgeted |
| `reserve` | `"max"` | `"p75"` debits the typical peak (ADR-0033); restart-applied |
| Gate 2 re-probe | 5–10 s | admission memory-tick |
| MemTotal re-probe | few min | tracks VM resize |

This table is memory-only; CPU's knobs (`cpu_overcommit`, `reserve_cpus`,
`profiling_cpus`, `cpu_pause_pressure`, `cpu_resume_pressure`) are listed with
*CPU admission* above, not repeated here.

## Verify at implementation (facts, not decisions)

- cgroup v2 `memory.peak` present on the Docker VM kernel (≥ 5.19); else poll
  `memory.current` / `docker stats`. OOM via `docker inspect .State.OOMKilled`.
- `docker info --format '{{.MemTotal}}'` parses cleanly.
- Exact stable class-key fields in the GitHub run/job payload (workflow
  name/path + `job.name`).

## VM admission (ADR-0026, slice 8b)

macOS VM jobs and, once Docker runs in the Linux VM, Docker jobs wait on gates
that are pure functions in `src/vm/admit.rs`:

- **The wall.** No job starts unless the pf wall's verdict is green and current.
  Red, stale, unread and out-of-date all refuse (`/api/state` `vm.wall`:
  `open`, `red`, `out_of_date`, `unread`), and jobs already running finish.
  Docker admission waits on it only once `set_wall_required` is on, so a host
  that never installed the wall keeps today's behaviour.
- **The macOS class** is capped at two per host (Apple's limit).
- **The machine share.** Each macOS VM plans at 8 GiB inside the share ADR-0021
  declares, after the Linux VM's reservation. With no share declared, no macOS
  VM starts.
- **The disk floor** (ADR-0024) gates a macOS VM job, charged 10 GiB for its
  first minutes (ADR-0032 §1). Its reading and staleness rule are Docker's.

The operator's hold, a pause and the machine's pressure stop VM jobs as they
stop native ones, and a VM job counts toward the pressure floor of one job.

A refused macOS VM claim **parks**, like a Docker claim, before its runner is
named, and names its gate (`wall`, `macos_cap`, `vm_memory`, `share`, `disk`,
…) in `/api/state`. It takes no native slot. The admission it wins is handed to
the VM backend, which holds it until the VM is stopped and releases it on every
exit; a VM teardown could not confirm gone is quarantined first and counts
against the cap from then on. Without an admission the backend starts nothing
(#263). An adopted VM is counted past every gate, for as long as it runs.
