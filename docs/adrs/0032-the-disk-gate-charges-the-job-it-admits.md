---
title: The disk gate charges the job it admits
type: adr
status: accepted
date: 2026-10-10
owner: rejifald
---

# ADR-0032: The disk gate charges the job it admits

## Status

accepted (2026-10-10). The owner decided it on
[#106](https://github.com/rejifald/faber-legacy/issues/106).

**Amended 2026-10-11** ([#111](https://github.com/rejifald/faber-legacy/issues/111)). A
frozen job whose GitHub job is over is thawed so it can exit, and at most two
jobs are frozen at once. See the amendment at the end.

It amends two ADRs:

- **[ADR-0024](0024-a-full-disk-stops-admission-not-docker.md) §1.** The gate counts
  the job it is about to admit, a dind job is charged more, and a critical floor
  adds a last resort.
- **[ADR-0021](0021-the-machine-share-is-declared-not-left-over.md) §2c.** Growing
  swap pauses admission too.

## Context

In October 2026 a single Docker job filled the builder's disk and took Docker
down. #106 has the timeline.

- **At admission:** faber admitted a dind job with free space just above the
  admission floor. No Docker job had been admitted in the previous 10 minutes, so
  the gate held nothing back.
- **The next few minutes:** the job wrote more than 3 GB in its first few
  minutes, and free space fell well below the floor.
- **About ten minutes in:** Docker stopped answering. faber rightly refused to
  restart it onto a full disk (ADR-0024 §2).
- **About half an hour in:** with the disk all but full, macOS purged tens of GiB
  of purgeable data. That, not faber, recovered the host.
- **Minutes later:** Docker came back.

Three things let the job in, or did nothing once it was in.

- **The gate never charges the job it is deciding about.** `DiskLedger::refusal`
  (`src/limiter.rs`) subtracts 3 GiB for each Docker job admitted in the last
  10 minutes. The job under consideration is not one of them yet. A job admitted
  alone is therefore never charged at all.
- **The charge describes light jobs, not dind jobs.** ADR-0024 measured about
  2 GiB per job in its first minutes and set 3 GiB above that. A dind job's own
  daemon pulls and builds images into its volume. This one wrote more than the
  whole charge within minutes.
- **Swap shares the volume.** It grew by a couple of GiB during the job, and
  nothing counts it.
  - Later that afternoon macOS reported memory pressure `warn` while faber kept
    admitting.
  - ADR-0021 §2c pauses admission at `warn`, but its gates are decided and not
    built: [#109](https://github.com/rejifald/faber-legacy/issues/109).

Raising the admission floor to the reclamation floor would have stopped this job.
The owner chose that first, then reversed it. ADR-0024 had already rejected it:
in its September incident the host spent about seven hours between the two floors
while about 150 jobs succeeded. The same key also holds off revival, and ADR-0024
rejected tying revival to the reclamation floor too.

## Decision

### 1. The gate charges the job it admits

- **The rule.** A Docker job is admitted only if free space stays at or above the
  floor after two charges are taken off: those of the jobs still inside the
  10-minute window, and the job's own.
- **The charge is per class:**
  - **10 GiB** for a job whose profile grants dind (ADR-0013)
  - **3 GiB** for any other Docker job

  Each charge is held for 10 minutes after admission, as now (or until the job ends
  if sooner: see the amendment of 2026-10-17, #161). Both stay constants
  until faber measures disk per job. Learned per-class figures then replace them,
  as ADR-0024 already plans.
- **The floor and its key are unchanged.** `disk_admission_floor_gib` stays at or
  below `disk_floor_gib`, and revival still waits only for the admission floor.
- **On the incident's numbers:** the dind job needed 10 GiB free above the floor,
  so it would have parked. A light job still runs down to 3 GiB above the floor.
- **The waiting claim shows the charge it is waiting on,** beside the free space
  and the floor, so a parked dind job says why it parks while light jobs run.

### 2. Below a critical floor, the newest job is frozen

Admission cannot stop a job that outgrows its charge once it has started. So:

- **Freeze.** When free space falls below 3 GiB, faber freezes the most recently
  admitted running job with `docker pause`, its dind daemon included. While free
  space stays below 3 GiB, it freezes the next newest on each later tick, one at a
  time.
- **Unfreeze.** Once free space is back above 6 GiB, faber unfreezes them, oldest
  first.
- **Cost.** A job frozen for long enough loses its runner's connection, and GitHub
  fails it. Auto-retry (ADR-0003) may run it again. That costs one job, where a
  full disk costs Docker and every job on the host.
- **Scope.** It applies only where the disk floor is stated, and `0` turns it off
  with the floor. Every freeze and unfreeze is logged and audited, and the
  dashboard shows a frozen job as its own state.
- **Rules the code also follows.**
  - A free-space reading that is stale neither freezes nor thaws.
  - A job can be frozen only once its runner container exists.
  - Turning the floor off stops freezing, not thawing: faber still thaws any job
    it finds frozen.
  - A thaw unpauses the dind daemon before the runner, so the job never runs
    against a frozen daemon.
  - A frozen job counts as alive everywhere: in reconcile, drain, the janitor and
    adoption. The stale-runner sweep skips this host's frozen runners.
- **Known gap** ([#111](https://github.com/rejifald/faber-legacy/issues/111)). A frozen
  job that GitHub has already failed keeps its disk until it thaws, because the
  reclaim guard (ADR-0017) still sees its worker. If the disk stays below 3 GiB for
  reasons outside CI, every job can end up frozen this way. Closed by the
  amendment of 2026-10-11 below.

### 3. Growing swap is a pressure signal

- **The signal.** This amends ADR-0021 §2c. Besides `warn` and `critical`,
  admission also pauses while swap use has grown by at least 1 GiB in the last
  5 minutes, and it resumes once swap has stopped growing for 5 minutes. On macOS,
  swap files grow on the data volume, so a machine short of memory also eats the
  disk that faber's gate protects.
- **Not a disk reservation.** Growing swap means the machine is short of memory,
  and memory is ADR-0021's to manage.
- **Scope.** Like the rest of §2c, it applies once the machine's share is declared.
  It is built with ADR-0021's gates, in #109. The swap-growth gate applies on
  Linux as well as macOS, while the pressure *level* stays inert on Linux until
  thresholds are chosen from a Linux host.

### Rollout

- **The builder** gets this by a hand-deployed `main`, in a quiet window.
- **Until then** its config raises `disk_admission_floor_gib` as a stopgap, which
  comes back down once this ships there.
- **The peer host** gets it with the auth cut.
- **No new key.** On every host the change applies only where the floor is
  already stated. A host whose operator stated no floor freezes nothing, but it
  is not quite unchanged: the thawing loop runs there too, so a faber runner
  someone paused by hand is unpaused on the next tick (corrected 2026-10-11, #111).

## Consequences

- **On a squeezed host, dind jobs park first while light jobs keep running.** A dind
  job now needs 10 GiB free above the floor to start, and a light job 3 GiB.
  Before, a job admitted alone needed only the floor.
- **One job can be frozen, and then failed, to save Docker.** Before, the disk
  filled and Docker took every job with it.
- **The constants are still constants.** A dind job that writes more than 10 GiB in
  10 minutes can still outrun the charge. The critical floor is what catches it.
- **On macOS this needs #109 to finish the job.** Without ADR-0021's gates, a
  machine that is paging still admits work, and its swap still eats the disk.

## Alternatives considered

**Raise the admission floor to the reclamation floor.** It was decided first and
then reversed. ADR-0024 already rejected it, for the stall of about seven hours it
would have caused in that ADR's incident, and it would hold off revival too.

**Charge every Docker job 8 GiB.** That is simpler, but it over-reserves for light
jobs, which are most of CI.

**Kill the newest job instead of freezing it.** That frees its space at once, but
it always costs the job. A freeze costs the job only if space doesn't come back.

**Reserve disk for swap.** That treats the symptom. Growing swap means the machine
is short of memory, which ADR-0021 owns.

## Amendment (2026-10-11): a frozen job GitHub has finished is let go

The owner decided it on [#111](https://github.com/rejifald/faber-legacy/issues/111),
choosing options 2 and 3 of the issue and leaving ADR-0017's guard as written.

**The problem.** §2's known gap: a frozen runner sends no heartbeat, so GitHub
fails its job about 10 minutes in. The reclaim guard still sees the frozen
`Runner.Worker` in `docker top` and refuses the teardown, so the job keeps its
disk. If free space stays below 3 GiB for reasons outside CI, the loop freezes
every running job, one per tick, and none ever thaws. CI is then down until the
runner lifetime cap, or for good when that cap is `0`.

**The decision.**

- **Release on terminal.** Once the reconciler has seen GitHub finish the job a
  frozen runner holds, the critical floor thaws it, whatever the disk reads and
  whether or not the floor is on. Its runner then exits by itself, and the normal
  teardown frees its disk. A job released this way is never frozen again.
  - "Finished" is the claim's own job reading `completed` or gone, or the
    sibling's job when the runner took one, and the runner not seen working:
    the "job finished" rows of the reclaim verdict. Any unreadable signal keeps
    the job frozen.
  - The claim's own job is already fetched every cycle. A sibling's costs one
    more fetch per frozen claim and cycle, at most two, and none once released.
  - A wrong answer costs a freeze, not a job: a thaw kills nothing, and the
    runner's own clearance still decides the teardown.
- **At most two frozen at once.** Below 3 GiB the newest job is still frozen
  first, but not while two are frozen already. A job whose thaw keeps failing
  still counts. On a disk nothing in CI is filling, this costs two jobs at a time
  rather than every job on the host. It is a constant, like the floors.
- **A failed thaw is shown.** `/api/state` carries the last failed thaw on the
  frozen claim (`frozen.thaw_failure`), and the dashboard shows it, so a job that
  cannot be unpaused no longer reads as one waiting for room.

**Rollout.** No new key. The cap only narrows behaviour this ADR introduced, which
no published release has shipped to a follower, so no host relies on more than two
freezes. A release that had shipped it would still be safe to narrow this way: the
cap lowers how many jobs a squeezed disk can cost, and never turns a running job
into a failing one.

## Amendment (2026-10-17): a finished job's charge is released

[#161](https://github.com/rejifald/faber-legacy/issues/161). §1's "held for 10 minutes
after admission" is now "held for 10 minutes after admission, or until the job
ends if that is sooner". The charge was held for the whole window even after the
job's containers were gone, so what the job wrote was counted twice: once in the
free-space reading, once in the charge. On the builder the gate held tens of GiB
for jobs that had already finished and closed with several times the floor free;
median admission wait went from about a second to most of a minute.

The job's admission guard now gives its ramp entry back when it drops, which is
after the backend has torn the job's containers down, as a provision that never
started already did (#89). A job still running keeps its charge until the window
passes. An adoption was never charged and has nothing to give back.

## Amendment (2026-11-01): what a ramping job has written comes off its charge

The 2026-10-17 amendment fixed the double count for a job that has ended. It
stayed for a job still running: a charge stands for disk the job has *yet* to
write, but it was held whole for ten minutes while the free-space reading already
showed what the job had written. On the builder's Linux VM, with less than half
its 64 GiB disk free, the gate closed after three or four admissions, saying
something like "30 GiB free (16 GiB held for 3 jobs still starting)", and
reopened only as the oldest charge aged out, so jobs started in batches with free
space barely moving.

Each admission now records the fresh reading of the volume it writes to (the
Docker reading for a Docker job, the Mac's volume for a macOS VM). The gate takes
the drop in free space since the oldest admission measured on the volume it is
reading off the charges of that admission and every later one on that volume,
never below zero. Equivalently, it admits on free space as it was before the
window started writing, less every charge in the window — or on free space now,
if that is less. A charge measured on another volume, or with no fresh reading at
admission, is never credited. §1's window, charges and floor are unchanged.

**What it gives up.** Anything else that writes during the window (an older job,
an image pull, the mirror cache) is credited to the window's charges too. It was
never charged before either, and it is in the reading in full once the window
ends; until then the gate can admit up to that much more than a perfect ledger
would. The floor is the margin for it. Per-job disk measurement, which §1 already
names as what replaces the constants, would close it.
