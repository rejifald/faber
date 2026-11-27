---
title: The ledger debits the job a runner took, and a reservation at p90 has its own margin
type: adr
status: proposed
date: 2026-11-27
owner: rejifald
---

# ADR-0066: The ledger debits the job a runner took, and a reservation at p90 has its own margin

## Status

proposed (2026-11-27). The owner asked for this draft after a measurement on one host
(*Context*) and has decided nothing in it yet. It takes a position on four choices
that are the owner's:

- at job start the debit moves to the job the runner took, and the containers' limits
  stay where they were launched (§1);
- a debit that no longer fits the budget is taken anyway, and the job is never held
  for it (§1);
- the smaller reservation is one more enum member with a fixed margin, not a number
  an operator types (§4);
- both keys ship off, and neither becomes a default here (*Rollout*).

Revised the same day after an independent check of its statements against `728ab93`,
a second check of the revision, and an independent recomputation of its figures from
the host's logs. They found:

- that the job-start judgement keeps a workflow's name and drops its id, which a class
  key needs (§1);
- that an admission's guard gives back a fixed amount, and is out of the job-start
  loop's reach (§1);
- that each ADR-0033 behaviour is tested against `"p75"` by name (§4);
- that `[autolimit]` is a strict table, so an older binary refuses the new key, which
  ADR-0006's rollback already allows for (*Rollout*);
- that "charge" and "ceiling", the draft's first words for the debit and for a
  container's limit, already mean other things here;
- that the three jobs took 85% of the runner time and not 95%, and that the swap
  moves the ledger by 1% and not 2%;
- passages in other documents that this ADR contradicts, now listed below.

It amends, for a host that turns the keys on:

- [docs/autolimiter.md](../autolimiter.md):
  - *The two gates*, Gate 1: a debit can move after admission, and for a time the sum
    can stand above the budget.
  - *The cap is sized for the cohort, the reservation for the class*: the reservation
    is the minted-for class's until the job starts, and the class's of the job taken
    from then on.
  - *Which class a sample belongs to*: the learner takes the class found at job start,
    and keeps the lookup at teardown for a run that has none. Its claim that any run
    longer than one poll interval is captured does not hold on the measured host (§3).
  - *`reserve = "p75"`*, which says the enum has two values and that the label is what
    an adoption re-charges, and the tunables table.
  - *CPU admission*: the share the ledger debits moves with the memory debit.
- [ADR-0033](0033-a-reservation-is-the-typical-peak-the-cap-stays-the-worst.md):
  - §1: the gap it describes, a light-minted runner debited as light while it runs a
    heavy job, closes at job start, and the label is no longer all that an adoption
    re-debits.
  - §2 gains a third member.
  - Where §3 and §4 say "under `"p75"`", read "under a reservation below the cap",
    armed days included.
  - Its rollout order, the peer before the builder, is not kept for `"p90"`
    (*Rollout*).
- [ADR-0019](0019-cpu-is-a-deadline-resource.md): in §2, the CPU share the ledger
  debits moves with the memory debit, and the runner's quota does not. In §8, an
  adopted runner's share comes from the same audit line as its memory.
- [ADR-0011](0011-adopt-in-flight-work-across-restarts.md): an adopted runner is
  re-debited from its last `debit` audit line when it has one, and from its labels
  otherwise.

## Context

### Two heavy jobs at a time, with room for three

On one host, in the seven days to 2026-11-27, three job classes of one watched
repository's pull-request workflow took about 85% of the runner time. Sizes here and
below are rounded examples, not readings. Counts and shares are rounded.

| Job | Peak p50 | Peak p90 | Highest of its last 20 | Debited today |
|---|---|---|---|---|
| build and tests | 7 GiB | 8 GiB | 9 GiB | 12.5 GiB |
| screenshots, shard 1 | 5.5 GiB | 8.5 GiB | 9 GiB | 12.5 GiB |
| screenshots, shard 2 | 4.5 GiB | 8 GiB | 9.5 GiB | 13 GiB |

With a memory budget of, say, 28 GiB, two of these are admitted and the third waits.

- While it had work, the host ran one runner for about 55% of the time and two for
  about 40%.
- The median wait for admission was about three times the median run.
- In the thirteen workflow runs whose three jobs all ran on this host, the three
  peaks summed to 22 GiB at most. To run the three together the ledger asks for
  38 GiB.

The debit is not in the audit log. These figures join the daemon's own log, which
prints each launch's reservation and limit, to the outcome lines by job id.

### The debit is the worst peak, and the worst peak follows the budget

- A class's cap is the highest of its last 20 peaks × `margin` 1.4, both defaults
  (`cap_from`, `src/limiter.rs`). Under `reserve = "max"` the ledger debits that cap,
  plus what the runner's own services hold.
- A learned cap is held at the profiling ceiling, `profiling_cap_mib` per container. A
  class that has outgrown it, by an OOM or by a peak above it, may grow up to
  `escalation_frac` of the budget, runner and dind together
  ([ADR-0053](0053-an-oom-at-the-profiling-ceiling-grows-the-cap.md); `apply_want`,
  `src/limiter.rs`).

So a class that wants more than that bound is held at it, and its cap rises when the
budget does. During the week the host's Linux VM was made larger, and its budget went
from, say, 17 GiB to 28 GiB.

- The three classes had been held at the bound. Their caps rose by more than half, to
  what they wanted.
- Their median peaks moved by less than 10%, and their highest peaks rose by 10 to 20%.
- The host still ran two of them at a time.
- Under the earlier caps the three had run about 290 times, and none was OOM-killed.

`reserve = "p75"` (ADR-0033) barely helps these classes. Its reservation is
`min(cap, p75 × margin)`, and for them p75 × 1.4 comes to about 9 to 10.5 GiB, so the
third job still does not fit. ADR-0033 said as much in its *Consequences*: a class
whose typical peak is close to its worst gains nothing.

### A runner takes another job most of the time, and the total barely moves

A JIT runner is not pinned to the job it was minted for (autolimiter.md, *Which class
a sample belongs to*). Below, a class's *cap* is the number it has learned, and a
container's *limit* is the `--memory` it was launched under. So today:

- **The containers' limits are sized for the cohort:** the per-container maximum over
  every class seen queued in the repository within 7 days that the runner's profile
  can serve (`Limiter::sizing_for`, `src/limiter.rs`). On this host 98% of launches
  were logged as "sized for queued peer".
- **The debit is the minted-for class's own** (`reservation_for`, called from the
  provisioning path in `src/reconcile.rs`), on the argument that dispatch is a
  permutation over the cohort.

The week bears that argument out for the total, and not for a single run:

- About 70% of the runs faber could attribute to a job had run another job than their
  runner was minted for.
- Debiting each of those runners for the job it took would have held 99% of the
  GiB-hours the ledger did hold for them. The jobs that swap are siblings in one
  workflow, about the same size.
- Runners that never took a job held about 2% of all the GiB-hours.
- A limit holds nothing. Only the debit limits admission.
- But 6% of runs were debited over a quarter less than the class of the job they took,
  15% over a quarter more, and 4% peaked above their own debit.

The last line is the gap ADR-0033 §1 named: under a smaller reservation "it is the
ledger's error that grows". Gate 2, `headroom_mib` and `reserve_mib` absorb it today,
unseen.

### faber now knows the job before its first step

Before [ADR-0059](0059-the-gate-runs-again-when-a-job-starts.md), faber learned which
job a runner took from a later discovery scan, or from one more read at teardown. The
ledger never used the answer. It fed bypass detection (ADR-0030), the busy check
before a reclaim and, at teardown, the learner (`executed_class`,
`src/reconcile.rs`).

ADR-0059 is still marked proposed, and its Docker half is merged. With it:

- every Docker runner carries a job-started hook, which waits for faber's verdict
  before the job's first step;
- each runner has a watch (`Watch`, `src/job_start.rs`), run in a loop that sleeps 2 s
  between passes. Once a watch sees the runner's worker process, it searches GitHub
  for the job whose `runner_name` is the runner, every 4 s, until it finds it or
  `job_start_search_secs` (120 s by default) has passed (`judge_started_job`,
  `src/reconcile.rs`);
- what it finds is kept as the job's id and name, its run and attempt, and the
  workflow's display name (`Judged`).

On this host the hook's step took 3 s at the median and 12 s at p90, over about 750
runs. So faber knows which job a runner has seconds after GitHub assigned it, and
before any of its steps.

### Announcing a runner is the admission

This ADR began with a question from the owner: could a runner be announced with
nothing reserved, and be given its resources when its job arrives?

It cannot. GitHub hands a runner any queued job its labels cover, and faber cannot
prevent that (ADR-0059, *Context*), so an announced runner is an admitted job. The
only place left to hold a job is the hook, and a job held there:

- is in progress on GitHub, because the hook is one of its steps (ADR-0059, *Context*
  and §2). Its `timeout-minutes` should be running, by GitHub's description of that
  key as the time a job may run. faber has not measured it;
- is assigned to this host's runner. While it was queued, any host with room could
  mint for it;
- waits under the gate's deadlines. faber gives up its search after
  `job_start_search_secs`, and the hook gives up after `job_start_deadline_secs` (180 s
  by default). A job with no verdict by then is stopped, or runs unverified.

So a debit is taken when the runner is minted, as today. What can change is what
becomes of it once the job is known.

## Decision

### 1. At job start, the debit moves to the job the runner took

```toml
[autolimit]
debit = "taken"   # absent or "minted": today's behaviour. "observe": measure only
```

**At mint, nothing changes.** A claim is admitted on its minted-for class's
reservation, and its containers start under the cohort's limits. That debit decides
how many runners faber announces.

**At job start, the debit moves.** When a runner's watch has found and judged the job
it took, the decision is to let it run, and the job's class is not the minted-for
class:

- the class is `class_key` (`src/limiter.rs`) of the repository, the workflow id and
  the job's name. `judge_started_job` reads the run and the job to judge them and
  keeps the workflow's name only, so the judgement gains the workflow id;
- the ledger gives back what the claim was admitted on and takes what admission would
  debit for the taken class at that moment. In memory that is its cap, or its
  reservation while one below the cap is in force (`debit_mib`, `src/limiter.rs`). In
  CPU it is the class's share (`Reservation.mcpu`), which no reserve mode changes;
- the taken class's debit is worked out for the runner as it was launched: with its
  own dind grant, and with what its own services hold (`with_services`,
  `src/reconcile.rs`), because they run whichever job arrived;
- the disk charge was settled with that dind grant, and does not move.

The move is the ledger's alone. It makes no Docker call, and the verdict is delivered
as ADR-0059 §2 says whether or not the move has happened yet.

**What the code needs for it.** Three things stand in the way today:

- An admission's guard (`AdmissionGuard`, `src/limiter.rs`) gives back exactly the
  reservation it was created with, and it is a local of the provisioning task. The
  job-start loop holds only watches. The move rewrites the guard's reservation and
  the ledger's totals together, by the runner's name.
- A watch carries no class, dind grant or services. The move takes them from the
  claim.
- A watch is dropped once its allow is delivered, and its judgement with it. The
  class it found is kept with the claim, for the outcome line (§3).

**A move never waits and never refuses.** If the taken class's debit does not fit, the
ledger stands above its budget. Gate 1's sums then refuse every claim until it is back
under, and the move records by how much it went over. Whatever subtracts the debits
from the budget, such as the state view's room for more jobs, has to stop at zero.
Today the same job runs with the ledger not knowing, and the hook is not the place to
hold it (*Context*).

**The containers' limits do not move.** They stay as the cohort sized them, and the
runner keeps its `--cpus` quota. *Alternatives* gives the measurement behind this.

**No judgement, no move.** A runner whose job is not found, or is found and cannot be
judged, keeps its mint-time debit, as today: faber stops the job, or on a private
repository under `record` it runs unverified. A refused job is stopped, and its debit
does not move either.

**`"observe"` moves nothing.** It writes the same audit line with what would have
moved, so a host can see how often, how far, and how far above its budget, before it
turns the move on.

**macOS VM jobs are out of scope.** A VM's size is fixed at boot, VM admission debits
a flat figure per VM (`[macos_vm] memory_mib`; `src/vm/admit.rs`), and the macOS half
of ADR-0059 is not built ([issue 67](https://github.com/rejifald/faber/issues/67)).

### 2. A moved debit survives a restart

Adoption re-debits the ledger from a container's `org.faber.reserved-mib` label
(ADR-0011) and its `org.faber.reserved-mcpu` label (ADR-0019), both read in
`src/docker.rs`. Docker cannot relabel a running container.

- **The move is an audit line** (`kind: "debit"`), written before the ledger changes.
  It names the runner, the job it was minted for, the job it took, that job's class,
  the debit before and after in memory and in CPU, and how far above its budget the
  ledger went.
- **Adoption reads the runner's last `debit` line that records a move** and re-debits
  that. A runner with none is re-debited from its labels, as today. A line written
  under `"observe"` records no move and is not read. The reason a job was frozen is
  already kept this way, for the same cause (`src/freeze.rs`).
- **A runner adopted before its job started** is watched again, as every adopted
  runner with a hook is (ADR-0059 §2), and its debit moves when the job starts.

### 3. The learner takes the same answer

Under `"taken"`, a finished run is attributed to the class its debit moved to, or
stayed on. The lookup at teardown (`executed_class`) stays for a run with no
judgement, and for every host that has not turned this on.

- A run the teardown lookup cannot place is still learned. That lookup reads the
  discovery scans' record and then asks GitHub once more. On the measured host about
  8% of the runs that ended were kept out of the learner because it found nothing.
- The ledger and the learner cannot disagree about which job ran.

`executed_job_id` on the outcome line keeps its meaning.

### 4. `reserve = "p90"`: the p90 peak, at a margin of its own

```toml
[autolimit]
reserve = "p90"   # absent or "max": today's behaviour. "p75": ADR-0033, unchanged
```

```text
reservation = min(cap, p90(window) × 1.1)      per container
```

`p90` is the nearest-rank 90th percentile of the class's rolling window, computed as
`p75` is (`src/limiter.rs`).

- **The rest of the reservation is ADR-0033 §1's.** It equals the cap while the window
  is not full, while it holds a censored sample, and while the class is pinned. Gate 1,
  the head set-aside and the `org.faber.reserved-mib` label use it.
- **What does not use it stays as it is.** The containers' limits are the cohort's.
  Gate 2 asks for room for the cap, as autolimiter.md has it under `"p75"`.
- **§3 to §5 of ADR-0033 apply as they do under `"p75"`:** the memory freeze, the
  fallback to max with its doubling cool-off, and a host OOM kept out of the learner.
  Each is tested against `"p75"` by name today (`ReserveMode::P75`, in
  `src/reserve.rs`, `src/limiter.rs` and `src/reconcile.rs`), and the shadow figures
  are p75's alone (`p75_reservation`, `src/limiter.rs`), so the member is added at
  each.
- **`"p75"` is unchanged.** A host that runs it keeps its formula.
- **Validated where a human is looking.** `faber check-config` and the settings save
  accept the three values. A binary from before this ADR reads `"p90"` as a value it
  does not know: it boots, warns and runs as `"max"` (`ReserveSetting::Unknown`,
  `src/config.rs`). Its own `faber check-config` and settings save refuse the file
  until the value is gone.

**Why a margin of its own.** The cap's margin protects one job from being killed: 1.4
over the worst of 20 runs. A reservation protects a sum. One job's excess over its
reservation is met by the others' slack, by `headroom_mib` and by the VM's reserve,
and when all of that is spent the cost is a freeze, not a kill. So it needs less. The
1.1 covers how coarsely a peak is measured: `docker stats` every 3 s, net of inactive
file pages (ADR-0033 §5; [issue 57](https://github.com/rejifald/faber/issues/57)).

**Why p90 and not p75.** With a small margin, the percentile has to carry the tail
itself. ADR-0033 turned p90 down at × 1.4, where three of its seven classes still
reached the ceiling.

**What the replay shows.** It follows ADR-0033's replay: fill the budget with jobs
drawn from the host's real mix until the next does not fit, 20,000 times a rule, and
count the packs whose summed peaks exceed budget − `headroom_mib`. Every class that
ran is in the mix, weighted by its runner time, and a class whose window is not full
reserves its cap under every rule. Peaks are drawn two ways: independently from each
class's history, and together, where every job in a pack takes the same quantile of
its own class. The second is the lockstep that *Elasticity* names, at its worst.

| Rule | Jobs admitted at once | Packs over, independent | Packs over, together |
|---|---|---|---|
| max × 1.4 (`"max"`) | 2.2 | 0% | 0% |
| p75 × 1.4 (`"p75"`) | 2.4 | 0% | 0.07% |
| p90 × 1.25 | 2.4 | 0% | 0% |
| **p90 × 1.1** | **3.2** | **0%** | **0.5%** |
| p90 × 1.0 | 3.3 | 0.01% | 0.7% |
| p50 × 1.15 | 3.8 | 3.6% | 13% |

- The gain is a jump, not a slope. The third heavy job fits once a reservation is
  under a third of the budget, and p90 × 1.25 is still above that.
- Real packs lie between the two columns. In the thirteen runs above, a small sample,
  the two largest classes' peaks did not rise together.
- This is one host and one mix. The peer's mix, which ADR-0033 measured, is skewed,
  and it has not been replayed under this rule. `"p75"` may still be the better member
  there. Each host replays its own log before it chooses (*Validation*).

### 5. What is recorded and shown

- **The `debit` audit line** of §2.
- **The outcome line** gains what the ledger held for the run when it ended, in memory
  and in CPU, and the memory limits its containers ran under. Today it carries the CPU
  share debited at admission and the runner's quota (`cpu_reserved_mcpu`,
  `cpu_quota_runner_mcpu`;
  [ADR-0056](0056-a-run-records-where-its-time-and-resources-went.md)) and nothing of
  the kind for memory, which is why *Context* had to read the daemon's log.
  `cpu_reserved_mcpu` keeps its meaning: the new fields are beside it.
- **The shadow figures.** Each class's p90 reservation is computed and shown beside
  its cap and its p75 reservation, on every host and in every mode, with the count of
  runs that outgrew it, as `p75_runs` and `p75_trips` are today.
- **`/api/state`**, additively: for each in-flight claim, the job its runner took and
  the debit it holds now; for the budget, the `debit` mode, the moves since the daemon
  started, and how far above its budget the ledger stands.
- **The dashboard and the settings form.** The dashboard shows the job taken beside
  the job minted for, says when the ledger is above its budget because of a move, and
  gains the p90 column. The settings form gains `debit` and the third `reserve` value.
  Each is a UI change, reviewed against the design guidelines before it merges.

### Rollout

1. **Land the measurement, behind no switch:** the outcome fields, the shadow p90
   figures, and a replay that prints §4's table from a host's own audit log.
2. **Land `debit`,** with an absent key meaning `"minted"`. The builder host runs
   `"observe"` for a week, then `"taken"`.
3. **Land `reserve = "p90"`.** The builder host sets it once `"taken"` has run there
   for a week and its replay still shows the third job fitting.
4. **The peer host** follows, with whichever member its own replay favours.

Both keys ship off, so a host that did not ask is unchanged. `"taken"` can lower what
a host admits as well as raise it: a host whose heavy jobs often land on light-minted
runners admits more today than it would if its ledger knew. That is why it is a key
and not a fix, and why `"observe"` comes first. Making either a default would be a
future, release-noted change.

**An older binary does not start on the new key.** `[autolimit]` is a strict table
(`deny_unknown_fields`, `AutoLimitFile` in `src/config.rs`), and the daemon exits on a
file it cannot parse. An automatic rollback is safe all the same: it restores the
config it snapshotted before the update together with the binary it ran on
([ADR-0006](0006-fleet-autoupdate.md), *Why the config is snapshotted alongside the
binary*). An operator who installs an older release by hand takes `debit` out first.
`faber check-config` on that release names the key.

**The builder goes first, which is not ADR-0033's order.** ADR-0033's rollout puts
the peer first, as the host whose data moved it. The data behind `"p90"` is the
builder's. Either way, the first host to run a reservation below the cap is the first
to rely on the memory freeze, which has no end-to-end test yet
([issue 11](https://github.com/rejifald/faber/issues/11)). That test comes before
`"p90"` is set anywhere.

## Consequences

- **The ledger is right from a job's first step.** What Gate 2, `headroom_mib` and
  `reserve_mib` absorb today becomes a number on the dashboard.
- **Alone, `"taken"` buys little capacity:** about 1% on the measured host. It is the
  condition for debiting less, because under a smaller reservation the ledger's error
  would otherwise grow (ADR-0033 §1).
- **`"p90"` serves the classes ADR-0033 could not:** those whose typical peak is close
  to their worst. On the measured host there is room for a third heavy job where
  there was room for two. The same host could instead keep two at a time in a smaller
  Linux VM, which is room
  [ADR-0065](0065-the-share-follows-the-machine-s-own-use.md), still proposed, could
  give back to the machine.
- **The gain comes in jumps.** A host whose reservations sit just above a divisor of
  its budget gains nothing until one more job fits.
- **The freeze and the fallback become reachable on more hosts.** They are ADR-0033's,
  with its costs: up to two frozen jobs, and at least a day at max.
- **Adoption depends on the audit log for one more thing.** A lost or truncated log
  re-debits an adopted runner from its labels, which is today's debit.
- **One more reservation per class to explain,** beside the cap and the p75.
- **Not changed:** macOS VM jobs, what the Linux VM costs the machine, the containers'
  limits, and claims parked by the machine's own pressure. On the measured host those
  gates parked claims for longer than the ledger did. ADR-0065 is about them.

## Alternatives considered

**Announce a runner with nothing debited, and admit the job in the hook.** The owner's
first framing. *Context* says why the debit at mint has to stay.

**Hold a job in the hook until its moved debit fits.** It would keep Gate 1 a bound.
The job would be in progress while it waited, stay assigned to this host, and wait
under deadlines that exist to stop unverified code. A heavy job's room frees in tens
of minutes, not inside those deadlines.

**Move the containers' limits too, with `docker update`.** Lowering each container to
the taken class's own cap would restore "a job is bounded by its own class".

- On the measured host about 1% of runs peaked above their own class's cap and passed
  because the limit was the cohort's. Most were classes still held at the profiling
  ceiling, and a peak above it is how such a class is learned without a kill
  (ADR-0053).
- Raising a limit that is below the taken class's cap would have helped no run. Among
  the runs faber could attribute, the warning for that case, a job whose class wants
  more than the limits its runner was launched under, was not logged once in five
  weeks.

Lowering costs jobs and raising buys none, and neither changes what is admitted. faber
has also never called `docker update`. This can be revisited if that warning starts to
appear.

**Size classes on runner labels**
([issue 60](https://github.com/rejifald/faber/issues/60)). A workflow opts in to a
small or a large class, and a runner carries only its class's label, so GitHub cannot
hand it another class. The ledger stays a true bound and no debit ever moves. It needs
a change in every workflow and a choice of classes per profile, and on the measured
host the jobs that swap are the same size. It remains the right tool where
light-minted runners take heavy jobs as a rule. The issue reads the "sized for queued
peer" line as what the ledger reserves. At this commit that line describes the
containers' limits, and the ledger reserves the minted-for class's own (*Context*).

**A reservation margin the operator types.** ADR-0033 §2 made the reservation an enum,
"not a number an operator can type", so that each member comes with its own
measurement. This ADR adds a member the same way.

**Lower `margin`.** It is the cap's margin too. Lowering it kills jobs to buy
admission.

**Reserve at the median.** It gives 3.8 jobs at once here, with about one pack in
thirty over when peaks are independent and one in eight when they rise together.
ADR-0033 turned it down for the same reason on the peer's mix.

**Debit what a job is using now.** The ledger would follow each container's live
reading and admit against the sum. It is the largest saving on offer, since a job is at
its peak for a small part of its run. It is also Gate 2 under another name: jobs just
admitted have not allocated yet, which is why Gate 1 exists. It needs a model of how a
job ramps, and is not this ADR.

**Stop a cap following the budget.** The bound ADR-0053 lets a class grow to is a
share of the budget, so a larger budget buys larger caps before it buys concurrency
(*Context*). Bounding that growth is a change to the cap, with its own risk of killing
jobs. Under `"p90"` the debit no longer follows the cap, so the budget is spent on
jobs either way.

**Let the re-created watch move the debit again after a restart.** Every adopted
runner with a hook is watched again, and its job is found again from GitHub. So the
ledger could start from the labels and correct itself, with nothing read from the
audit log at adoption. It would leave the ledger wrong from adoption until GitHub
answers, with admission already open. The audit line is written either way, for
`"observe"`.

## Validation

To measure before each stage of the rollout:

- **Before `"taken"`:** a week of `"observe"` on the host. The share of runs whose job
  the watch found before the job's first step, expected above 99%: on the measured
  host four of about 750 were not found. How often the ledger would have stood above
  its budget, and by how much.
- **Before `"p90"`:** the end-to-end test of the memory freeze (issue 11), the host's
  replay, and a week of its shadow figures.
- **After `"p90"`:** fallback trips (ADR-0033 §4), which should be none in the first
  two weeks; each class's OOM kills, which should not change, since no limit moved;
  the median wait for admission.
- **What withdraws it.** A host that trips the fallback converges to `"max"` by itself
  (ADR-0033 §4). A `"taken"` host whose ledger stands above its budget for a large
  share of its busy time has light-minted runners taking heavy jobs as a rule, and
  wants the routing of issue 60, not this.
