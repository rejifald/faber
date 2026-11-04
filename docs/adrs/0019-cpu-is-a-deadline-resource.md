---
title: CPU is a deadline resource
type: adr
status: accepted
date: 2026-09-17
owner: rejifald
---

# ADR-0019: CPU is a deadline resource

## Status

accepted (2026-09-17). The configuration surface is `CpuMode`, `CpuConfig` and
`HardCeiling` (`src/config.rs`); the measurement is `CpuUsage` and `RunReport`'s
CPU fields (`src/docker.rs`); admission and learning live in `src/limiter.rs`.

Supersedes the "CPU is compressible" premise in
[docs/autolimiter.md](../autolimiter.md), amended alongside this ADR.

## Context

The autolimiter admits Docker jobs by memory alone.
[docs/autolimiter.md](../autolimiter.md) states the reason directly: CPU is
compressible — "oversubscribe and the kernel time-slices, jobs run slower but
nothing dies" — so admission is "memory-only; CPU stays over-committed and
uncapped."

That held while the builder's Docker VM was smaller, because memory admission
alone already bounded concurrency to a few heavy jobs, which kept CPU
oversubscription to a few-fold however many cores each container thought it
had.

The VM has since been resized: its memory roughly doubled while its core count
barely moved. Memory alone now admits about three times as many heavy jobs,
but the core count a container sees did not move with it, and nothing in
admission accounts for it either: every container, runner and dind alike, still
sees all of the VM's cores, and any tool that sizes a worker pool from that
number sizes to the whole VM regardless of how many peers are also running. A
watched repo's test job derives its worker count from that full core count on
every run. Nine such jobs is not nine-way sharing of the VM's cores; it is nine
attempts to use most of them each.

Measured in September 2026: a VM load average of five to seven times its vCPU
count, with about nine Docker jobs executing on average over an hour.
`/proc/pressure/cpu`'s `some avg60` sat near 60%.

The cost is not diffuse slowness; it is jobs missing deadlines they would
otherwise have met. Over a couple of days that month, nearly a third of the
failed jobs on the builder host were timeouts traceable to CPU starvation, at
every level CI has one: unit-test timeouts, a hook timeout, browser sessions and
an end-to-end web server that didn't start in time, a screenshot timeout, and
whole jobs killed at their job timeout. A watched repo had already raised its own
job timeouts more than once to absorb a slowdown of several times under
saturation — a patch over the symptom, applied from the consuming side because
nothing on faber's side gave it a reason not to.

Contention also feeds back into the one resource admission does gate. A job
running 2–4× slower holds its memory reservation 3–5× longer, so the queue
behind it pays twice: claims that would otherwise be quick have measured parking
for most of an hour behind a saturated cohort.

"Nothing dies" was true and beside the point. CI work carries a deadline at
every level — test, hook, step, job — and blowing one of those is a failure
exactly as real as an OOM kill, just slower to arrive and, until now, invisible
to admission.

## Decision

### 1. CPU is admitted like memory

Every job class now carries a CPU share in millicores for each of its two
containers, the same runner/dind split memory already uses. Admission requires
`Σ(admitted shares) + next ≤ VM cores × cpu_overcommit − reserve_cpus`, checked
alongside Gate 1's memory sum rather than in place of it — a claim is admitted
only when both fit. Cores come from `docker info`'s `NCPU` field (`probe_ncpu`,
`src/docker.rs`), re-probed on the same cadence as `MemTotal` so a Docker
Desktop resize is picked up without a restart. On a 16-core VM, for example, that
is 16 × 1.25 − 1.0 = 19 admittable cores of share against 16 real ones —
overcommit still assumes *some* compressibility, just a bounded amount instead
of an unlimited one (§6 explains why 1.25 and not more).

The same monotonic-ticket FIFO and head set-aside
[docs/autolimiter.md](../autolimiter.md) already uses for memory governs CPU
too: one queue, one ordering, both dimensions checked against it, rather than a
second queue that could reorder the two resources against each other. A class
whose CPU share alone exceeds the whole budget is the one place CPU is allowed
to diverge from memory's rule: instead of refusing to protect it — what an
unsatisfiable memory head gets — its share is clamped down to fit the budget so
it can still be admitted and run alone rather than wedging the queue forever.
That asymmetry is deliberate, and it is the one place "CPU is compressible"
survives this ADR: going over a CPU quota degrades a job, where going over a
memory cap kills it, so clamping a CPU want is safe in exactly the way clamping
a memory want would not be.

### 2. Enforcement is a quota, sized like memory's cohort ceiling

The runner container is started under `--cpus` set to the largest runner share
across the queued cohort — the same reason memory sizes the *container cap* to
the cohort rather than to the minted-for class
([docs/autolimiter.md](../autolimiter.md): "the cap is sized for the cohort,
the reservation for the class"). A JIT runner is not job-pinned, so whichever
job GitHub actually hands it must already fit the container it lands in. The
*reservation* charged to the ledger stays the minted-for class's own share,
mirroring the memory split exactly.

Verified: Node's `os.availableParallelism()` honours the cgroup quota — it
reports 3 under `--cpus=3`, not the VM's full count — so a tool that sizes its
pool from it, Vitest among them, shrinks automatically with no change needed in
the watched repo. `nproc` and `os.cpus()` still report every core of the VM; a
tool that sizes from those
instead over-spawns, but only inside its own quota — CFS throttles the excess
rather than letting it take real cores from a neighbour.

dind's share is reserved and counted in admission but not itself
quota-enforced in this version: it hosts the service containers a job
declares, not a worker pool sized from a core count, so there is no
over-sizing hazard for a quota to correct yet.

### 3. Shares are learned from cgroup `cpu.stat`

Sampled during the run, on the same poll that samples each container's memory
peak:
`peak_mcpu` (the highest average usage over one sampling interval), `avg_mcpu`,
and `throttled_ratio` — `nr_throttled / nr_periods` from `cpu.stat`
(`CpuUsage`, `src/docker.rs`). A run throttled across ≥10% of its periods is a
censored sample, exactly as an OOM censors a memory sample: the container's
true need is at least its quota, so the learner records `quota × 1.5` rather
than the necessarily-depressed measured usage — the same ×1.5 force-bump step
memory already uses on an OOM.

Share = rolling max × margin, floored at 1 core for a runner and 0.1 for dind,
and clamped at `profiling_cpus` — 4 cores by default. `profiling_cpus` does
double duty where memory uses two separate knobs: it is both the starting
share for a class with no history yet *and* the most a learned share may grow
to without an operator pin. A class wanting more than that is held at the
ceiling and warned rather than auto-applied — the same auto-apply/escalate
split memory already makes, expressed here as a flat core count instead of a
fraction of budget.

One caveat the mechanism doesn't fully correct for: usage measured without a
quota in force is only a lower bound. A class profiled while a co-tenant
saturated the VM records what it got, not what it needed, and reads smaller
than its real requirement until a later, less-contended run corrects it
upward. Raise-fast already covers that correction; it just means a
freshly-learned share should be read as "at least this much," not as settled.

### 4. A live pressure gate catches what the ledger can't see

Admission pauses while `/proc/pressure/cpu`'s `some avg60` is at or above
`cpu_pause_pressure` (40%), and resumes only once it falls to
`cpu_resume_pressure` (25%) — the same anti-flap shape as Gate 2's memory
headroom/band, read by the same throwaway probe container that already reads
`MemAvailable` (`probe_vm_pressure`, `src/docker.rs`). It fails open when the
kernel exposes no PSI, the same direction every other unreadable CPU signal in
this design defaults to. This is what catches load faber never admitted — a
co-tenant dev stack building its own project — and a share that is simply
wrong: the ledger only knows what it was told a class needs, not what the VM
is actually doing right now.

### 5. CPU is measured, learned and logged in every mode

The sampling, the learner and the audit record run whether `cpu` is `auto` or
`off`. Turning admission on is then a config flip against a warm history
instead of a cold profiling pass through every class in the fleet — the same
reason memory's own cap-learning replays the audit log at startup rather than
starting blank.

### 6. The switch is a grant, and it copies ADR-0013's shape

`[autolimit] cpu = "auto" | "off"`, resolved into `CpuConfig`/`CpuMode`
(`src/config.rs`). An absent key is `CpuMode::Unstated`, which *behaves* as
`off` but is not the same word: it is warned at every boot, shown as its own
state on the dashboard, and the settings form writes an explicit value the
first time it is saved from — the same three-state shape
[ADR-0013](0013-docker-in-docker-is-a-grant-not-a-default.md) built for
`dind`, copied rather than re-derived because the reasoning is identical: this
reaches installed hosts through an unattended update
([ADR-0006](0006-fleet-autoupdate.md)), it tightens admission, and a release
must not narrow a default on a host whose operator did nothing and was not
asked (AGENTS.md). Flipping the unstated default to `auto` is a future,
release-noted change, not something this ADR does.

The knobs, restart-applied rather than live-tunable the way memory's are:

| Knob | Default | Notes |
|---|---|---|
| `cpu_overcommit` | 1.25 | CPU budget = VM cores × this − `reserve_cpus` |
| `reserve_cpus` | 1.0 | held back for Docker Desktop and co-tenant stacks |
| `profiling_cpus` | 4.0 | cold-start share and unattended escalation ceiling (cores) |
| `cpu_pause_pressure` | 40 | PSI `some avg60` (%) that pauses admission |
| `cpu_resume_pressure` | 25 | PSI `some avg60` (%) that resumes it |

`cpu_overcommit` starts at 1.25, not higher, because the failure point had
already been observed: test timeouts appearing at roughly 1.5×
oversubscription inside a single job. 1.25 leaves margin under a threshold
that is already measured, not guessed.

### 7. `hard_ceiling` becomes a three-state setting too

`"auto" | "off" | N` (`HardCeiling`, `src/config.rs`). `auto` resolves to
`AUTO_HARD_CEILING`, a constant 4 for now — given a setting of its own rather
than folded into a bare number so a later release can derive it (from the
VM's core count, plausibly) without another config migration. Absent or `off`
still means no ceiling, as it always has, and the value stays live-tunable:
unlike the rest of this ADR, the ceiling is a blunt backstop rather than part
of the learned model, so nothing about changing it mid-flight is unsafe.

`overrides.json` keeps a numeric mirror of the *effective* ceiling
(`Overrides::hard_ceiling`) alongside the new setting (`Overrides::ceiling`),
because an older binary reading the file after a rollback understands only
the numeric key — and, per that file's own rule, an overrides file it cannot
parse at all is discarded whole, pins included, which a string-valued `"auto"`
in a field an old binary expects to be numeric would otherwise risk.

### 8. Adoption re-charges the CPU ledger the same way it re-charges memory's

[ADR-0011](0011-adopt-in-flight-work-across-restarts.md) already stamps
`org.faber.reserved-mib` on the runner container so a restart can recover a
claim's memory charge without asking the container what it is using —
load-bearing because the reservation and the applied ceiling are deliberately
different numbers, so reading `--memory` back off the container would recover
the wrong one. The same is true of CPU: §1 and §2 give the reservation and
the applied `--cpus` quota their own reasons to differ. So it gets the same
fix, `org.faber.reserved-mcpu`, stamped alongside the existing label and read
back on adoption, so a restart re-charges the CPU budget instead of starting
it blank against containers that are still holding real cores.

## Consequences

Positive: jobs finish in the time their own timeouts assume, instead of
intermittently in 2–4× that. Concurrency drops, but throughput does not — the
VM cannot perform more than its core count's worth of CPU work per hour no
matter how many jobs it is divided among; oversubscription only spreads the
same work thinner and, per Context, holds memory longer while doing it.
Watched repos should be able to stop raising job timeouts to keep pace with
saturation.

Negative and risk: a CFS quota introduces throttling stalls, typically under
100 ms, inside an otherwise-healthy job — a cost `auto` accepts that `off`
does not. A fresh deployment of `auto` profiles every unlearned class at the
4-core / 1-core ceiling until each has run once, the same one-time dip
memory's own cold-start already has; running with `cpu = "off"` first (§5) to
collect a day of history before flipping to `auto` avoids paying it twice. A
follower host gets any of this only through a release (ADR-0006). And a
`config.toml` carrying the new keys — `cpu`, the CPU knobs, or
`hard_ceiling = "auto"` — is rejected outright by an older binary
(`[autolimit]` denies unknown fields), the same deploy-order hazard every new
key has had; rolling back means removing them. `overrides.json` does not share
the hazard, because of the numeric mirror in §7.

Rollout: deploy with `cpu = "off"` to build a day of CPU history across the
fleet's classes, then set `cpu = "auto"`. `hard_ceiling = "auto"` needs no
history and is available as an immediate stopgap in the meantime.

## Alternatives considered

**A count cap alone.** Already available as `hard_ceiling`, and kept as the
stopgap for exactly that reason. It treats a 1-core job the same as a 10-core
one, and does nothing about the actual mechanism: every container still sizes
itself to all of the VM's cores regardless of how many peers are running.

**A pressure gate alone, with no admission accounting.** Reactive rather than
predictive: it lags a job's ramp-up, so a batch admitted together in one tick
can still saturate the VM before PSI climbs enough to stop the next one. Jobs
would still size their pools to every core of the VM each.

**A quota alone, with no admission accounting.** Shrinks each job's own
worker pool correctly, but nothing stops the sum of quotas handed out from
exceeding the VM's real core count — it fixes what a job does to itself, not
what the fleet does to the VM.

**cpuset pinning instead of a CFS quota.** Would fix `nproc` and `os.cpus()`
too, not just `os.availableParallelism()`, since pinning changes which cores
a container can see rather than how much of them it may use. Rejected because
it partitions the VM into fixed sets with no overcommit at all — idle cores
in one job's set sit unusable while another job starves next to them, which
is worse than the oversubscription this ADR bounds, not better.

**Raising timeouts in the watched repos further.** Already the de facto
response, done twice, and it is the problem restated rather than solved — it
does not return the throughput or the predictability the saturation cost, it
only widens the window before the cost is visible again.
