---
title: A reservation is the typical peak; the cap stays the worst
type: adr
status: accepted
date: 2026-10-11
owner: rejifald
---

# ADR-0033: A reservation is the typical peak; the cap stays the worst

## Status

accepted (2026-10-17). The owner settled four questions in a design session on
2026-10-11:

- reserve at the p75 peak rather than at p90, at the median, or by splitting a
  class on a declared weight;
- when memory runs critically short, freeze the newest job and fall back to max;
- the switch is an enum, not a free percentile;
- a fallback re-arms itself, and its cool-off doubles each time it trips again.

It amends two sections of [docs/autolimiter.md](../autolimiter.md):

- **"Reservations are hard caps".** The container cap stays a hard cap. The amount
  the ledger debits is allowed to be smaller.
- **"Elasticity".** That section rejects overcommit outright. This ADR allows a
  bounded, measured amount of it, and backs it with a mechanism for when the bet
  loses.

## Context

### Admission is limited by worst cases, not by use

faber reserves each job class's learned cap: the rolling max of the class's last 20
peaks × `margin` 1.4, clamped to the profiling ceiling (`cap_from`,
`src/limiter.rs`). The same number is the container's `--memory` limit and the
ledger's debit.

On the peer host, in the seven days to 2026-10-11, one watched repo's PR workflow
ran seven heavy jobs. Their peaks are wide and skewed:

| Job | Runs | Peak p50 | Peak p90 | Reserved today |
|---|---|---|---|---|
| test shard 1 of 2 | ~290 | 2.1 GiB | 6.5 GiB | 10.2 GiB |
| test shard 2 of 2 | ~290 | 1.9 GiB | 8.1 GiB | 10.2 GiB |
| screenshot job | ~250 | 0.8 GiB | 4.9 GiB | 7.4 GiB |
| build and tests | ~250 | 5.7 GiB | 7.9 GiB | 10.2 GiB |
| end-to-end tests | ~210 | 3.0 GiB | 5.7 GiB | 10.0 GiB |
| static checks | ~200 | 3.1 GiB | 5.5 GiB | 8.3 GiB |
| type check | ~190 | 6.6 GiB | 7.1 GiB | 10.2 GiB |

With the peer's memory budget, only a few of these fit at once, and the host ran
at that ceiling for about half of its busy time.

Meanwhile a live `/api/state` read showed most of the memory still available
while the ledger held a large share of the budget for just two jobs. Jobs were waiting more
than they ran: the two test shards' median wait to be admitted was more than
twice their median run time.

The skew has a cause. The test shards and the screenshot job do most of their
work only when the change touches what they test. A run with little affected
peaks around 2 GiB; a run with a lot affected peaks around 10 GiB. One rare heavy
run sets the reservation for all the light ones.

### What a smaller reservation buys, and what it risks

A Monte Carlo replay packed the peer's budget with jobs drawn from the seven
classes' real mix, 20,000 times per rule. Each job's actual peak was sampled from
its class's history. Every rule is "that percentile of the window × 1.4, clamped
to the ceiling".

| Rule | Jobs admitted at once | Packs whose summed peaks exceed budget − headroom |
|---|---|---|
| max (today) | 4.0 | 0% |
| p90 | 4.2 | 0% |
| **p75** | **4.9** | **0%** |
| p50 | 9.3 | 10.9% |

Two biases pull the overrun column in opposite directions:

- **It understates risk.** It samples peaks independently, and they are not
  independent. Within one run, the two test shards correlate at about 0.5
  (Spearman, over a hundred runs), and the end-to-end tests with the static checks
  at about 0.4. A big change is big for every job it touches. This is the lockstep
  effect "Elasticity" names.
- **It overstates risk.** It adds whole-job peaks, and the peaks of concurrent
  jobs rarely land at the same second.

p90 buys nothing: three of the seven classes still clamp at the ceiling. p50 buys
2.3× and loses about one pack in ten, more once correlation is counted. p75 is the
largest step the data shows as safe.

The gain is modest: about one more heavy job at a time, roughly 22%. This ADR
takes that step and builds the means to measure the next one. It does not claim
the next one is safe.

## Decision

### 1. The reservation and the cap are two numbers

Each class keeps its cap exactly as today: rolling max × margin, the escalation
guard, the OOM force-bump, the cohort max for the container. Nothing about how
large a container may grow changes, so a job is no more likely to be killed for
outgrowing its own limit.

Next to the cap, each class stores a reservation:

```text
reservation = min(cap, p75(window) × margin)      per container
```

The reservation equals the cap whenever any of these holds:

- **The window isn't full.** With fewer than 20 samples, a p75 means little. This
  covers profiling and new classes.
- **The window holds a censored sample.** An OOM in the last 20 runs means the
  class's distribution is not known yet. Today the window is a bare list of
  numbers (`src/limiter.rs`), and the startup replay feeds raw peaks without
  reading the outcome's OOM fields (`seed_limiter`, `src/reconcile.rs`). So each
  sample has to carry a censored flag, and the replay has to set it from
  `oom_runner` / `oom_dind`. Without both, a restart would forget the OOM and
  drop straight to p75.
- **The class is pinned.** An operator's pin is a statement about the job, and
  faber does not second-guess it.

Everything that debits the ledger uses the reservation:

- Gate 1;
- the head set-aside, so the head waits for its p75, not its max;
- the `org.faber.reserved-mib` label, so adoption re-charges what admission
  debited.

The container's `--memory` limit uses the cap. CPU shares are out of scope and stay
max-based.

**This widens a gap that already exists.** A runner is not pinned to the job it
was minted for, so a light-minted runner can be handed a heavy job. Today it then
debits the light class's max; under `"p75"` it debits the light class's p75,
which is smaller. The container cap still fits the heavy job, because it is the
cohort max. It is the ledger's error that grows, and Gate 2, §3 and §4 are what
catch it.

### 2. The switch is `[autolimit] reserve`, an enum

```toml
[autolimit]
reserve = "p75"   # absent or "max": today's behaviour
```

- **Two values only, `"max"` and `"p75"`.** A further value, such as `"p50"`, is a
  new enum member with its own measurement and its own ADR, not a number an
  operator can type.
- **Validated where a human is looking.** `faber check-config` and the settings save
  reject any other value. Boot warns about an unknown value and uses `"max"`
  (AGENTS.md: never refuse to boot on a config the release did not write).
- **Visible.** The dashboard shows the mode and, for each class, its reservation
  next to its cap.
- **Ships off.** An absent key is `"max"`, so a host that did not ask is unchanged.

### 3. When memory runs critically short, freeze the newest job

Gate 2 already stops admission below `headroom_mib`. Under `"p75"`, a second line
sits below it:

- **Freeze.** When `MemAvailable` falls below half of `headroom_mib`
  (768 MiB at the default), faber freezes the most recently admitted running
  Docker job with `docker pause`, its dind daemon included. While the reading
  stays below that line, it freezes the next newest on each later tick.
- **At most two jobs are frozen for memory.** This is the same cap #150 set
  for disk. Without it, a squeeze from outside CI would freeze every job, and
  each would fail about ten minutes later.
- **Thaw.** Above `headroom_mib + band_mib`, faber thaws oldest first.
- **The tick is the memory tick.** It decides on each `MemAvailable` reading
  (every 8 s, `MEM_TICK`), not on the disk loop's 20 s.
- **An unreadable reading is itself a signal.** The probe runs a container, and
  under real pressure that probe is what fails. So for memory, unlike disk, a
  failed reading neither freezes nor thaws, but it does trip the fallback in §4
  when it follows a reading below `headroom_mib`.

**A freeze records why it was made.** Today a freeze is not remembered: the loop
reads the paused state back from Docker. And the disk loop thaws every frozen job
when no disk floor is set or when free space is above 6 GiB (`src/freeze.rs`).
Shared as it stands, the disk loop would thaw a job frozen for memory at once,
and the memory loop would thaw a job frozen for disk. So each freeze carries its
reason (a container label), each path thaws only its own, and the dashboard
shows the reason.

A freeze does not free memory; it stops one job's growth so the older jobs can
finish and release theirs. The line is set relative to `headroom_mib` so that it
moves when the operator moves Gate 2, and it is open to tuning at
implementation.

- **Cost.** A frozen runner sends no heartbeat, and GitHub fails its job about ten
  minutes in (#111). So the freeze saves that job only if memory comes back within
  about ten minutes. Otherwise it fails. Auto-retry (ADR-0003) picks that up
  from faber's own record of the freeze (amendment 2026-10-18): a runner still
  frozen when GitHub completed its job is faber's doing, the same signal a disk
  freeze relies on.
- **Why it is still worth it.** The alternative is the kernel's OOM killer, which
  ends a job at once, and not necessarily one that overran its reservation.

**Only the freeze is gated; the thaw runs everywhere.** faber freezes for memory
only under `"p75"`. A §4 fallback does not switch the freeze off at once: falling
back to max reservations changes what is admitted next, not memory already in
use, so a second freeze may still be needed. The freeze path therefore stays
active until the squeeze that tripped the fallback ends, which is a reading above
`headroom_mib + band_mib`. Only after that does the cool-off refuse new freezes.
The memory thaw runs on every Docker host in every mode, the way the disk loop
already thaws with its floor off ("turning the floor off stops freezing, not
thawing", `src/freeze.rs`). So a job frozen when the fallback trips, or when an
operator switches back to `"max"`, still comes back. With
`runner_timeout_secs = 0` it would otherwise stay paused forever. A container
paused by an older binary carries no reason label, and the disk path treats an
unlabelled freeze as its own, as it does today.

### 4. A sign of overrun falls back to max, with a doubling cool-off

Any of these trips the fallback:

- **a memory freeze** (§3);
- **a host OOM**, as defined in §5;
- **an unreadable memory probe after a low reading** (§3).

When it trips, the host reserves as under `"max"` until the cool-off ends, then
re-arms on its own:

- **The first trip** cools off for 24 hours.
- **Each further trip within 7 armed days of the last** doubles it: 48 hours,
  then 96, up to a maximum of 7 days. Armed days are days spent under `"p75"`.
  Time spent in a cool-off doesn't count, or a long cool-off would reset itself.
- **Seven armed days without a trip** reset the next cool-off to 24 hours.
- **One squeeze is one trip.** A further freeze during the same squeeze, before
  memory recovers above `headroom_mib + band_mib`, extends nothing.

The trip, its cause and the re-arm time are audited (`kind: "reserve"`) and shown
on the dashboard as their own state. Because the audit log is faber's persistence
substrate, a restart rebuilds the fallback state from it, the way caps are
replayed.

A host whose workload genuinely doesn't fit p75 converges toward max on its own,
with no operator watching. A host that overruns once in a long while keeps the
gain.

### 5. A host OOM must not teach the class

This is a correction that the overcommit makes necessary.

Today, an OOM flag on a container is a censored sample: the cap the container ran
under × `OOM_BUMP` 1.5, which the learner then multiplies by `margin` 1.4. So one
OOM grows the class's cap about 2.1×. Under overcommit, a VM-wide OOM kill could
land that bump on an innocent class, which would then cost concurrency for 20
runs.

**The two containers are read differently, and only one of them sees host OOMs.**

- **The runner** is flagged from `docker inspect .State.OOMKilled`
  (`src/docker.rs`). That reflects the cgroup's hierarchical `oom_kill` counter,
  which counts a kill by any OOM killer, the VM-wide one included. So a host OOM
  in the runner *is* learned as a class OOM today.
- **dind** is flagged from `oom` in its own `memory.events.local`. That counter
  rises only when dind's own limit is breached, a deliberate fix for service
  containers' kills inflating dind's cap. So a host OOM that kills something
  inside dind leaves the flag false. dind's local `oom_kill` is always zero,
  because dockerd lives in a child cgroup. The one exception is dind itself
  dying with exit 137, which the postmortem catches.

**Tell them apart by counters, not by peaks.** Peaks come from `docker stats`
polled every 3 s, net of inactive file pages, so a container killed at its own
cap almost always records a peak below it. A rule of "killed below the cap" would
call nearly every real cap OOM a host OOM. The cgroup counters answer the
question directly:

- **The container's own limit was breached:** the runner's `oom`, or dind's
  *local* `oom`. That is a class OOM, and it is learned as today.
- **Hierarchical `oom_kill` rose and hierarchical `oom` did not:** something in
  the subtree was killed with no limit anywhere in it breached. A nested service
  container hitting its own small limit raises the hierarchical `oom`, so it
  is not counted here. That is a host OOM. It is kept out of the
  learner, like an idle reclaim or an infra failure, and recorded as its own
  audit kind.

For the runner, that means sampling and latching its `memory.events` during the
run, the way dind's counters already are. The inspect runs after the container
has exited, and by then its cgroup is gone. When no reading exists, an
`OOMKilled` runner is classified as a class OOM, which is today's behaviour and
errs safe. For dind, it means a new latched read of the hierarchical
`memory.events` next to the local one it reads now. The hierarchical read is
used only for this distinction, never as a class sample, so the old inflation
bug stays fixed.

### Rollout

1. **Land the measurement first, behind no switch.**
   - Store and show each class's p75 reservation next to its cap.
   - Read the counters §5 needs, and audit a host OOM as its own kind.
   - The learner is unchanged in this step: a host OOM is still censored and
     bumped, exactly as today. Changing how every host learns from OOMs before
     the classification has been seen on real hosts would withdraw a capability
     followers rely on.
   - No admission change either, so every host can show what `"p75"` would do,
     and how often it would have tripped, before any host runs it.
2. **Then land the switch, the freeze, the fallback and §5's learner change,
   all under `"p75"`.** `"max"` stays the default, and a `"max"` host keeps
   today's learner. Applying §5 under `"max"` too is a later, release-noted
   change, once step 1's audit shows the classification is right.
3. **Turn it on on the peer host first.** It is the host whose data motivated this.
   Leave the builder on `"max"` until the peer has a week of trips (or none) on the
   dashboard.

Making `"p75"` the default would be a future, release-noted change, judged on the
fleet's fallback history. It is not part of this ADR.

## Consequences

- **About one more heavy job at a time on a busy host.** The waits shrink most for
  the skewed classes. Classes whose typical peak is close to their worst, like the
  type check above, gain nothing. Only the workflow lowering its own peak
  helps them.
- **Gate 1 is no longer a strict upper bound under `"p75"`.** autolimiter.md called
  that property the keystone. What replaces it: the container cap still bounds each
  job, Gate 2 still bounds admission, and §3 and §4 bound the cost of a bad pack and
  stop it from repeating.
- **A bad pack can cost frozen jobs.** It costs up to two frozen jobs and a
  fallback of at least 24 hours. An unclassified host OOM would instead cost a
  job the kernel picked, plus a ratcheted cap on an innocent class, which is
  what §5 prevents.
- **Two numbers per class to explain.** The dashboard has to show both, or "why is
  my job parked" stops being answerable from the panel (ADR-0022).

## Alternatives considered

**Reserve at p90.** It was proposed first, and the replay showed 4.2 jobs against
4.0, because the skewed classes' p90 still clamps at the ceiling.

**Reserve at the median.** It gives 2.3× the concurrency, but about one pack in ten
overruns before correlation is counted. It would make overruns routine and lean on
§3 and §4 as everyday machinery rather than as a backstop.

**Split a class on a declared weight.** The workflow would decide from a cheap "what
changed" job whether a run is light, and route it to its own `runs-on` label, so
faber learns a light class and a heavy class. The ledger stays a true bound. It
needs a change in every workflow that wants it, an extra job in front of every run,
and it helps only the skewed classes. It stays open as a complement to this ADR,
not a replacement.

**Lower the peaks in the workflow.** Capping the Node heap or build parallelism in
the heavy jobs lowers their caps within 20 runs, with no faber change. It is the
right lever for classes whose typical peak is close to their worst, and it is the
workflow owner's to pull. It is independent of this ADR.

**Grow the Docker VM.** Ruled out by ADR-0021: on macOS the VM never gives memory
back, and the machine's share is declared, not taken.

**Let small jobs go ahead of a waiting large one.** The head set-aside exists
because pure best-fit once starved large claims for over an hour. Duration-aware
backfill (admit a junior only if it should finish before the head's room frees) is
a separate idea, with its own risks. This ADR keeps the set-aside as it is.
