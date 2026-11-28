---
title: A reservation at p90 has its own margin
type: adr
status: accepted
date: 2026-11-27
owner: rejifald
---

# ADR-0066: A reservation at p90 has its own margin

## Status

accepted (2026-11-28). The owner settled these questions in a design session on
2026-11-28:

- the ADR promises a smaller debit to a host whose ledger is its binding gate, not
  one more job on the host it was measured on (*Context*, *Rollout*);
- `"p90"` does not wait for the ledger's debit to move at job start, and that move
  is deferred until a measurement calls for it (§4);
- a replay is safe enough at 0.1% of packs over when peaks are independent, and 1%
  when they rise together (§3);
- a host sets `"p90"` once the ledger has held at least half of its parked time for
  a week, and no host is named first (*Rollout*);
- the replay ships as a `faber` subcommand (§1).

Two more choices follow rules the owner had set before: the smaller reservation is
an enum member with a fixed margin, not a number an operator types (ADR-0033 §2),
and it ships off (AGENTS.md, *Shipping a fix reaches more than this machine*).

It was proposed on 2026-11-27 under another title, "The ledger debits the job a
runner took, and a reservation at p90 has its own margin". That draft also moved the
ledger's debit at job start, which §4 defers. Its text is this file's first version,
`docs/adrs/0066-the-ledger-debits-the-job-a-runner-took.md` at commit `ef7c96a`.

Its statements about the code and about other documents were checked against
`bb23281` by agents that did not write them, and its figures were recomputed from
the host's logs by another.

It amends:

- [docs/autolimiter.md](../autolimiter.md):
  - *The shadow p75 reservation*, which gains the p90 figures on every host (§1).
  - *`reserve = "p75"`*, which says the enum has two values, and the tunables table.
- [ADR-0033](0033-a-reservation-is-the-typical-peak-the-cap-stays-the-worst.md):
  - §2 gains a third member.
  - Where §3 and §4 say "under `"p75"`", read "under a reservation below the cap",
    armed days included.
  - Its rollout order, the peer before the builder, is not kept for `"p90"`
    (*Rollout*).

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

- The three classes had been held at the bound. Their caps rose by about half, to
  what they wanted.
- Their median peaks moved by less than 10%, and their highest peaks rose by 10 to 20%.
- The host still ran two of them at a time.
- Under the earlier caps the three had run about 290 times, and none was OOM-killed.

`reserve = "p75"` (ADR-0033) barely helps these classes. Its reservation is
`min(cap, p75 × margin)`, and for them p75 × 1.4 comes to about 9 to 10.5 GiB, so the
third job still does not fit. ADR-0033 said as much in its *Consequences*: a class
whose typical peak is close to its worst gains nothing.

### The ledger held about a third of the waiting

Every outcome line of a job that parked says how long it waited at each gate
(`wait_gates`; autolimiter.md, *Which gate bound it*). Take the hours of the same
week in which at least one job was waiting:

- **The machine's own gates held the queue for about two thirds of them.** These are
  `machine-pressure` and `swap` (autolimiter.md, *Why a claim is parked*): the Mac
  reporting memory pressure, or its swap growing. For nearly all of that time the
  host was running one Docker job, or none.
- **The ledger held it for about a third.** For most of that time two jobs were
  running and Gate 1 refused the third, which is the case this ADR is about. It comes
  to about a quarter of the waiting.
- **Nothing else held it.** CPU admission parked claims for minutes in the week, and
  Gate 2 for none.
- **The split moved from day to day.** Over a full day the ledger's share of the
  parked time ran from about three quarters to almost none. In the two full days
  after the VM was made larger, the machine's gates held the queue more than ten
  times as long as the ledger did. Two days are a small sample, and on the morning
  after them it was the ledger again.

The machine's gates are checked before the ledger (same table), so their time could
hide refusals the ledger would also have made. The count of running jobs says it did
not: with one job running, the ledger had room for a second heavy one.

The week was measured before ADR-0021's amendment of 2026-11-27 loosened the swap
gate, which will move some of that time.

So a smaller debit acts on about a quarter of this host's waiting. A third job at a
time would also raise what the VM really uses, on a machine whose own memory is what
holds its jobs back.
[ADR-0065](0065-the-share-follows-the-machine-s-own-use.md), still proposed, and
[issue 109](https://github.com/rejifald/faber/issues/109) are about those gates.

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
ledger's error that grows". §4 measures how far.

### Announcing a runner is the admission

This ADR began with a question from the owner: could a runner be announced with
nothing reserved, and be given its resources when its job arrives?

It cannot. GitHub hands a runner any queued job its labels cover, and faber cannot
prevent that ([ADR-0059](0059-the-gate-runs-again-when-a-job-starts.md), *Context*),
so an announced runner is an admitted job. The only place left to hold a job is
ADR-0059's job-started hook. A job held there is already in progress on GitHub and
assigned to this host's runner, and it waits under the hook's deadline
(`job_start_deadline_secs`, 180 s by default).

So a debit is taken when the runner is minted, as today. What can change is how
large it is.

## Decision

### 1. Measure first, on every host

Nothing in this section changes what is admitted.

- **The outcome line gains what the ledger held for the run,** in memory, and the
  memory limits its containers ran under. Today it carries the CPU share debited at
  admission (`cpu_reserved_mcpu`,
  [ADR-0056](0056-a-run-records-where-its-time-and-resources-went.md)) and the
  runner's quota (`cpu_quota_runner_mcpu`, ADR-0019), and nothing of the kind for
  memory, which is why *Context* had to read the daemon's log.
- **It also gains what the job taken would have been debited.** The line's `class` is
  the class of the job the runner took, and it is present when the run was fed to the
  learner (`src/audit.rs`). Beside it goes what admission would have debited for that
  class when the run ended, in memory and in CPU, for the runner as it was launched:
  with its own dind grant, and with what its own services hold (`with_services`,
  `src/reconcile.rs`).
- **The shadow figures.** Each class's p90 reservation (§2) is computed and shown
  beside its cap and its p75 reservation, on every host and in every mode, with the
  count of runs that outgrew it, as `p75_runs` and `p75_trips` are today.
  `/api/state` gains them additively.
- **The replay is a `faber` subcommand.** It reads the host's own audit log, which
  setup makes readable to the operator, and prints:
  - §3's table for that host: the jobs admitted at once and the packs over, for each
    rule;
  - the same with the debit moved at job start (§4);
  - the share of the parked time each gate held over the window.

  It prints aggregates only: no repository, workflow, job or class name, because its
  output is what an operator pastes into an issue (AGENTS.md, *Everything you write
  here is public*). Its output follows
  [ADR-0050](0050-cli-output-leads-with-a-summary-and-never-relies-on-colour.md), and
  it uses the daemon's own percentile and window rules, so that its verdict is the
  daemon's.
- **The dashboard** gains the p90 column in a later change, reviewed against the
  design guidelines.

### 2. `reserve = "p90"`: the p90 peak, at a margin of its own

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
- **`"p75"` is unchanged.** A host that runs it keeps its formula. The two members
  multiply differently: `"p75"` by `margin`, which an operator can change, and
  `"p90"` by 1.1 whatever `margin` is.
- **Validated where a human is looking.** `faber check-config` and the settings save
  accept the three values. A binary from before this ADR reads `"p90"` as a value it
  does not know: it boots, warns and runs as `"max"` (`ReserveSetting::Unknown`,
  `src/config.rs`). Its own `faber check-config` and settings save refuse the file
  until the value is gone.
- **Ships off.** An absent key is `"max"`, so a host that did not ask is unchanged.
  Making `"p90"` a default would be a future, release-noted change.

**Why a margin of its own.** The cap's margin protects one job from being killed: 1.4
over the worst of 20 runs. A reservation protects a sum. One job's excess over its
reservation is met by the others' slack, by `headroom_mib` and by the VM's reserve,
and when all of that is spent the cost is a freeze, not a kill. So it needs less. The
1.1 covers how coarsely a peak is measured: a `docker stats` reading every 3 s
(ADR-0033 §5; [issue 57](https://github.com/rejifald/faber/issues/57)).

**Why p90 and not p75.** With a small margin, the percentile has to carry the tail
itself. ADR-0033 turned p90 down at × 1.4, where three of its seven classes still
reached the ceiling.

### 3. What a replay must show

A host may set `"p90"` when a replay of its own audit log, over at least seven days,
shows all of:

- at most 0.1% of packs over when peaks are drawn independently;
- at most 1% of packs over when peaks rise together;
- at least one more job in the median pack than its present mode admits.

The gain is read off the median pack because the mean sits on the line: here it rises
from 2.15 to 3.15, and another seed puts that on either side of one.

**The replay** follows ADR-0033's. It fills the budget with runners drawn from the
host's real mix until the next does not fit, 200,000 times a rule, and counts the
packs whose summed peaks exceed budget − `headroom_mib`.

- Every class that ran is in the mix, weighted by its runner time. A class whose
  window is not full reserves its cap under every rule.
- A runner is drawn as a pair, the class it was minted for and the class of the job
  it took, from the host's own runs. It is admitted on the first and peaks as the
  second.
- Peaks are drawn two ways: independently from each class's history, and together,
  where every job in a pack takes the same quantile of its own class. The second is
  the lockstep that autolimiter.md's *Elasticity* names, at its worst.
- At 20,000 packs, ADR-0033's count, a share near 0.1% is within its own sampling
  error. Hence 200,000.

**What a pack over costs.** By itself, nothing. It means that if every job in the
pack peaked in the same second, `MemAvailable` would fall below `headroom_mib`, where
Gate 2 already stops admission. The memory freeze starts at half of that. A freeze
costs up to two frozen jobs, which GitHub fails after about ten minutes unless memory
returns, and a fallback to `"max"` for at least 24 hours (ADR-0033 §3 and §4).

On the measured host:

| Rule | Jobs admitted at once, median (mean) | Packs over, independent | Packs over, together |
|---|---|---|---|
| max × 1.4 (`"max"`) | 2 (2.15) | 0% | 0% |
| p75 × 1.4 (`"p75"`) | 2 (2.41) | 0% | 0.1% |
| p90 × 1.25 | 2 (2.33) | 0% | 0.05% |
| **p90 × 1.1** | **3 (3.15)** | **0.04%** | **0.6%** |
| p90 × 1.0 | 3 (3.22) | 0.25% | 1.4% |
| p50 × 1.15 | 4 (3.84) | 7% | 18% |

A share shown as 0% is under 0.01%. The debit stays where the runner was minted, as
the ledger works today.

- **p90 × 1.1 is the largest step inside the bar.** p90 × 1.0 is outside it, and
  p90 × 1.25 admits no third heavy job.
- **The gain is a jump, not a slope.** The third heavy job fits once a reservation is
  under a third of the budget, and p90 × 1.25 is still above that.
- **Real packs lie between the two columns.** In the thirteen runs above, a small
  sample, the two largest classes' peaks did not rise together.
- **This is one host and one mix.** The peer's mix, which ADR-0033 measured, is
  skewed, and it has not been replayed under this rule. `"p75"` may still be the
  better member there.

### 4. Deferred (recorded, not built): the debit moves to the job a runner took

The first draft of this ADR also moved the ledger's debit. Once ADR-0059's watch has
found and judged the job a runner took, the ledger would give back the minted-for
class's debit and take the taken class's. Only the ledger would move. The containers'
limits would stay as launched.

**Why it is not built.** On the measured host it buys no capacity and little safety:

| Under `"p90"` | Debit stays where minted | Debit moves |
|---|---|---|
| Jobs admitted at once, median (mean) | 3 (3.15) | 3 (3.20) |
| Packs over, independent | 0.04% | 0% |
| Packs over, together | 0.6% | 0.5% |
| Runs that peaked above the debit held for them | 3% | 0.5% |

The last row is the replay's, where each class's debit is the one its latest window
gives. It is not on the same base as *Context*'s 4%, which is against what the ledger
held at the time.

- **The total barely moves.** The jobs that swap are siblings of about the same size
  (*Context*), so the ledger's error is about 1% of what it holds.
- **The left column is overstated, if anything.** The replay draws each runner's
  swap independently. Real swaps are mostly siblings trading places, which cancels
  in the sum.
- **It is the costly half.** It needs a debit that changes after admission, a record
  of the move that an adoption can read back after a restart, and a ledger that can
  stand above its budget. It also rests on the job-start watch, whose behaviour at a
  restart was still being corrected when this was written.
- **The learner would gain little.** A run whose job faber cannot identify is kept
  out of the learner (autolimiter.md, *Which class a sample belongs to*), and the
  move would have placed it. About 9% of the runs that ended in success or failure
  carry no `class`, for that reason or another. Every one was short and small: none
  peaked above half a GiB. No heavy class lost a sample.

**What brings it back.** Either of:

- some host's replay passes §3's bar with the debit moved, and fails it without;
- a host running a reservation below the cap trips the fallback (ADR-0033 §4), and
  its outcome lines show a runner in that pack debited more than a quarter below the
  job it took.

**Where the design is.** In the first draft named in *Status*.
[Issue 118](https://github.com/rejifald/faber/issues/118) tracks what brings it back.
The design is to be checked against the code again before anyone builds from it.

### Rollout

1. **Land the measurement** (§1), behind no switch.
2. **Land `reserve = "p90"`** (§2), off.
3. **A host sets it when all three hold:**
   - `memory` held at least half of its parked time over the last seven days, which
     the replay prints;
   - its replay passes §3's bar;
   - the memory freeze's end-to-end test
     ([issue 11](https://github.com/rejifald/faber/issues/11)) has passed on a Docker
     host.

**No host is named first.** ADR-0033's rollout put the peer first, as the host whose
data moved it. Here a host goes when its own log says so. The measured host does not
qualify today: the ledger held a little over a third of its parked time. The peer has
not been measured.

**Why the freeze test is a condition.** A host that debits less than the cap relies
on the memory freeze (ADR-0033 §3), and nothing tests that loop end to end yet.

## Consequences

- **`"p90"` serves the classes ADR-0033 could not:** those whose typical peak is close
  to their worst. On a host whose ledger binds, there is room for a third heavy job
  where there was room for two.
- **It does not serve a host whose own memory binds.** On the measured host the
  ledger held about a third of the waiting. The machine's gates held most of the
  rest, and this ADR does not change them.
- **The gain comes in jumps.** A host whose reservations sit just above a divisor of
  its budget gains nothing until one more job fits.
- **The freeze and the fallback become reachable on more hosts.** They are ADR-0033's,
  with its costs: up to two frozen jobs, and at least a day at max.
- **The ledger stays wrong for a runner that took another job.** The gap is ADR-0033
  §1's. §4 measured it as small on one host and says what would reopen it.
- **One more reservation per class to explain,** beside the cap and the p75.
- **A command to keep true.** The replay decides whether a host may debit less, so it
  has to follow the daemon's rules as they change.
- **Not changed:** macOS VM jobs, whose debit is a flat figure per VM; what the Linux
  VM costs the machine; the containers' limits; and claims parked by the machine's
  own gates.

## Alternatives considered

**Announce a runner with nothing debited, and admit the job in the hook.** The owner's
first framing. *Context* says why the debit at mint has to stay.

**Move the debit at job start now.** The first draft's order: the move first, then
`"p90"`. §4 says why it waits.

**A stricter bar: no pack over when peaks are independent.** It is what ADR-0033's
table showed for `"p75"`, at 20,000 packs. At 200,000 even `"max"` shows a pack or
two over on the measured host, because a light-minted runner can take a heavy job.
So a bar of none is a bar on the sample size. The freeze and the fallback exist to
make a rare over-full pack cheap.

**Lower the containers' limits to the taken class's cap, with `docker update`.** On
the measured host about 1% of runs peaked above their own class's cap and passed
because the limit was the cohort's. Lowering would have cost those runs, and raising
would have helped none. Neither changes what is admitted, because a limit holds
nothing.

**Size classes on runner labels**
([issue 60](https://github.com/rejifald/faber/issues/60)). A workflow opts in to a
small or a large class, and a runner carries only its class's label, so GitHub cannot
hand it another class. The ledger stays a true bound. It needs a change in every
workflow and a choice of classes per profile, and on the measured host the jobs that
swap are the same size. It remains the right tool where light-minted runners take
heavy jobs as a rule. The issue reads the "sized for queued peer" line as what the
ledger reserves. At `bb23281` that line describes the containers' limits, and the
ledger reserves the minted-for class's own (*Context*).

**A reservation margin the operator types.** ADR-0033 §2 made the reservation an enum,
"not a number an operator can type", so that each member comes with its own
measurement. This ADR adds a member the same way.

**Lower `margin`.** It is the cap's margin too. Lowering it kills jobs to buy
admission.

**Reserve at the median.** It gives 4 jobs at once here, with about one pack in
fourteen over when peaks are independent and nearly one in five when they rise
together.
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

**A developer tool for the replay.** An example binary, as the privacy scanner is,
would be cheaper to build. It needs a checkout and a toolchain, which a follower host
does not have, and *Rollout* makes the replay a condition every host must meet.

## Validation

- **Before a host sets `"p90"`:** *Rollout*'s three conditions, from its own log.
- **After:**
  - fallback trips (ADR-0033 §4), which should be none in the first two weeks;
  - each class's OOM kills, which should not change, since no limit moved;
  - the median wait for admission;
  - the time claims park at `vm-memory`. Gate 2 asks for room for the cap, so it can
    hold a job the ledger admitted. It should stay small.
- **What withdraws it.** A host that trips the fallback converges to `"max"` by itself
  (ADR-0033 §4).
- **Not measured:** the peer host's mix under `"p90"`.
