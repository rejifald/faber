---
title: The machine's share is declared, not left over
type: adr
status: accepted
date: 2026-09-24
owner: rejifald
---

# ADR-0021: The machine's share is declared, not left over

## Status

**Amended 2026-10-10 by
[ADR-0032](0032-the-disk-gate-charges-the-job-it-admits.md).** §2c also pauses
admission while swap use is growing: by at least 1 GiB within 5 minutes. As of
2026-10-10 none of this ADR is built ([#109](https://github.com/rejifald/faber-legacy/issues/109)).
§1, §2 and the swap signal are built in #109; §3, the measured co-tenant reserve,
in [#85](https://github.com/rejifald/faber-legacy/issues/85).

accepted (2026-09-24). Recorded before implementation, as
[ADR-0007](0007-faber-owns-the-docker-host.md) was. It adds a term for the
machine faber runs on to two existing budgets: the autolimiter's Gate 1
([docs/autolimiter.md](../autolimiter.md)) and
[ADR-0019](0019-cpu-is-a-deadline-resource.md)'s CPU budget. Neither design
changes.

## Context

faber sizes admission against the Docker daemon's view of the machine. Gate 1's
memory budget is `MemTotal − reserve_mib`, and ADR-0019's CPU budget is
`NCPU × cpu_overcommit − reserve_cpus`. Both numbers come from `docker info`,
and both reserves hold back room *inside* the daemon's view. ADR-0019 describes
its reserve as held back "for Docker Desktop and co-tenant stacks".

On Linux with a native daemon, the daemon's view is the machine, so
`reserve_mib` is the machine's reserve. On macOS the daemon's view is a VM carved
out of the operator's Mac, and nothing in faber accounts for the Mac itself.
faber does not read the Mac's memory, cores or pressure, and no setting names
them. The Mac keeps whatever the VM's size happens to leave.

That was harmless while the VM was small. On the builder host it grew between the
autolimiter's design and ADR-0019: its memory roughly doubled, to about three
quarters of the machine's RAM, and its vCPUs to most of the machine's cores, on a
Mac that is also the operator's own everyday machine. faber's budget grew with
the VM, because re-probing `MemTotal` exists to do exactly that. Nothing checked
the new size against what the machine itself needs.

Measured in September 2026, with several jobs running:

| where | reading |
|---|---|
| Docker VM, seen from macOS | a footprint the size of the whole VM. Only a small fraction was resident; the rest was compressed or swapped |
| macOS memory | the compressor held pages worth about three quarters of the machine's RAM, squeezed into about half of it. Swap was nearly full, and had grown again an hour later. Terabytes had been swapped in over two days of uptime |
| macOS CPU | `kernel_task` at several cores' worth, compressing and paging. Load about three times the core count. Pressure level `warn` |
| macOS disk | data volume 99% full, and swap files growing into it |
| inside the VM | more Linux page cache than process memory. Load about seven times the vCPU count. CPU PSI `some avg60` above 80% |

The operator's report was that the machine was unusable while CI ran.

### Why admission alone cannot give memory back

Docker Desktop 4.89's VM has a virtio balloon, but never inflates it:
`balloon_inflate` was still 0 after two days. Free-page reporting is not
negotiated either (feature bit 5 is clear). So once the guest touches a page, it
counts against the VM process until the VM restarts.

The guest touches every page quickly, because Linux uses free memory as page
cache. From macOS's side, page cache is dirty anonymous memory: it can be
compressed or swapped, never dropped. When the guest later reuses a cache page
for a new allocation, macOS has to fault the stale contents back in first. The
paging is paid twice.

So a VM sized at, say, 24 GiB claims all 24 GiB of the Mac within hours of
booting, whatever admission does inside it. Admitting fewer jobs changes how the
guest divides its memory between processes and cache. It does not change how much
of the Mac the guest holds. The builder host showed this the same day: with every
CI job drained and admission held at zero, the VM still held its full size, and
macOS stayed at `warn`. **On macOS, the VM's size is the machine's memory
reservation, and there is no other.**

CPU is different. A vCPU costs the Mac CPU only while the guest runs something
on it. Admission that keeps CI's share small therefore does leave cores idle for
the machine, even when the VM is sized larger.

### The co-tenant reserve drifted the same way

`reserve_mib` defaults to 5 GiB. That figure was chosen when the co-tenant stacks
sharing the VM used a little under that at idle. At the measurement above they
held about three times as much and several cores, so Gate 1 was planning with
twice the reserve's worth of memory the VM did not have. The only protection was
Gate 2's last 1.5 GiB of headroom. Beyond that, the guest's OOM
killer would pick among the co-tenants' uncapped databases as readily as among CI
jobs. It is the same failure as the VM's: a reservation stated once as a number,
then never checked against the thing it describes.

## Decision

### 1. faber measures the machine as well as the daemon

A new probe goes in the platform seam, next to `docker_disk_path` in
[`src/platform/host.rs`](../../src/platform/host.rs). ADR-0007 solved the same
problem for disk: measure the thing that actually matters.

- **macOS:** `hw.memsize`; `hw.ncpu`, plus the performance/efficiency split for
  display; `kern.memorystatus_vm_pressure_level` (1 normal, 2 warn, 4
  critical); `vm.swapusage`; `vm.compressor_bytes_used`. All are unprivileged
  sysctls.
- **Linux:** `/proc/meminfo`, the CPU count from the process's affinity, and
  `/proc/pressure/memory`.

A **VM boundary** exists when the daemon's `MemTotal` or `NCPU` is smaller than
the machine's. That comparison decides it, not the platform. Docker Desktop for
Linux runs a VM too, and a native Linux daemon has no boundary. Without one, the
rest of this ADR reduces to what `reserve_mib` already does.

The dashboard, the tray and `/api/state` show the split:

- the machine's memory and cores;
- the VM's size, and what it leaves the machine;
- the machine's pressure level and swap use.

### 2. The machine's share is a declared grant

```toml
[host]
keep_memory_gib = 16   # RAM the Docker VM and CI must leave to the machine
keep_cpus = 2          # cores CI must leave idle for the machine
```

No value suits every machine. A workstation used alongside CI wants about half
its RAM kept, as the builder host does. A dedicated builder can keep
far less. The operator knows which kind of machine it is, and faber does not
guess.

Once the share is declared, faber does three things.

**a. It checks the VM against the share.** The VM encroaches when its `MemTotal`
exceeds `machine RAM − keep_memory_gib`, or its `NCPU` exceeds
`machine cores − keep_cpus`. faber then:

- warns at startup, and on every re-probe that finds it;
- shows an attention state on the dashboard and the tray;
- words the warning as the fix, for example on a 32 GiB machine keeping 16:
  "set Docker Desktop → Resources → Memory to 16 GiB or less".

The warning includes the reason, because nothing else will tell the operator the
VM never returns memory. faber does not resize the VM itself (see Alternatives).

**b. It plans within the share.** Gate 1's memory term becomes
`min(MemTotal, machine RAM − keep_memory_gib)`. ADR-0019's core count becomes
`min(NCPU, machine cores − keep_cpus)`.

- For CPU this is a real reservation, even before the VM is resized.
- For memory it limits CI's share of the VM, and so the paging CI causes. It
  cannot shrink the VM's footprint; only acting on the warning in (a) does that.

**c. It yields to the machine's memory pressure.** Admission pauses while macOS
reports `warn` or `critical`, and resumes at `normal`.

- The pause covers native jobs as well as Docker jobs, because native jobs run on
  the machine itself.
- The kernel's levels already carry their own hysteresis, so there is nothing to
  tune.
- It reclaims nothing. Like Gate 0, it is a floor: it stops CI adding work to a
  machine that is already paging.
- On Linux the signal is `/proc/pressure/memory`. The gate stays inert there until
  thresholds are chosen from a Linux host's measurements rather than guessed.

**Absent keys are unstated, and unstated keeps today's behaviour.** Both keys
tighten admission, and AGENTS.md forbids a release from narrowing a default on a
host whose operator did nothing and was not asked. So an absent key behaves as
before, and faber makes it loud instead. This is the shape
[ADR-0013](0013-docker-in-docker-is-a-grant-not-a-default.md) set for `dind` and
ADR-0019 copied for `cpu`:

- Every startup warns with the measured split, for example "the Docker VM holds
  24 of this machine's 32 GiB, leaving it 8".
- The dashboard shows the share as unstated.
- The settings form writes explicit values the first time it is saved.

Choosing a default for the unstated case is a future, release-noted change.
`faber check-config` and the settings save reject a share that is not smaller
than the machine. A running daemon only warns; it never refuses to boot.

### 3. The co-tenant reserve is measured, not assumed

With the share declared, Gate 1 holds back `max(reserve_mib, co-tenant peak)`
instead of `reserve_mib` alone. `reserve_mib` stays as the operator's floor.

The co-tenant figure comes from readings faber already takes: the VM's
unavailable memory (`MemTotal − MemAvailable`), minus the working set of faber's
own containers. That covers everything else, whoever started it, with no list of
names to maintain. It is a rolling peak over the last 24 hours, so a stack that
happens to be idle when sampled is not planned at its idle size.

CPU needs no counterpart. ADR-0019's pressure gate already exists to catch load
faber never admitted.

### 4. What does not change

- faber never resizes, restarts or reprioritises the VM to enforce a share.
- Nothing running is stopped for pressure. Every gate here, like every gate
  before it, acts only on admission.

## Consequences

**Positive.** The split between the machine and CI becomes a number the operator
chose and faber checks, instead of whatever a VM slider left behind. A state like
the one measured above shows up on the dashboard the moment it appears, with the
fix spelled out. Nobody has to find a machine that has been paging for two days.

**Cost: less CI per host.** On the builder host, keeping half the machine leaves
CI a small share once the co-tenants are counted: one or two heavy jobs, where
memory alone had admitted several. Demand provisioning
([ADR-0004](0004-demand-based-provisioning.md)) moves the rest to peers.
ADR-0019 already showed that oversubscription never added throughput.

**Memory is still enforced only by the VM's size**, which means by the operator
acting on the warning. faber can make the misconfiguration loud and plan inside
the share, but it cannot make Docker Desktop give memory back. If a later Docker
Desktop starts returning memory, (a) and (b) become conservative rather than
wrong.

**The pressure gate can hold admission for long stretches** on a machine whose
own workload keeps macOS at `warn`. That is the declared intent: the machine
comes first. The dashboard shows it as its own state, not as a queue that has
stopped for no visible reason.

**The usual deploy-order hazard applies.** A binary older than this one rejects
the new keys outright (`deny_unknown_fields`), so declare them only once the new
binary is running.

As this ADR is written, the builder host's VM is being resized by hand to fit its
share, which is this decision applied manually. Check (a) is what would have
asked for it.

## Alternatives considered

**faber resizes the VM itself.** It would make the memory share enforceable.
Rejected, for three reasons:

- Docker Desktop has no supported interface for it. 4.89's `docker desktop`
  command starts, stops and restarts the app and toggles features, but has no
  resources command.
- Its settings file lives in a group container that macOS protects from other
  applications.
- A resize restarts the engine, taking down co-tenant stacks faber does not own.
  ADR-0007 reserves restarts for a wedge, not a tuning change.

**Lower the VM's scheduling priority instead of sizing it.** Attractive for CPU,
since CI would soak up idle cores and yield at once. Rejected: the only
per-process lever macOS gives an unprivileged caller is darwin-background
(`taskpolicy -b`). It confines the VM to the efficiency cores (a minority of the
builder's cores) and throttles its disk, so CI would crawl and ADR-0019's
deadline failures would return. A `renice` cannot be undone without root. And
priority does nothing for memory, which is the actual cause.

**Admission alone, with no model of the machine.** `hard_ceiling` and a larger
`reserve_mib` can already do this. It limits CI's CPU but not the VM's memory
footprint. It also asks the operator to redo the arithmetic whenever the VM or
the co-tenants change, and that arithmetic is exactly what drifted here.

**Drop the guest's page cache on a timer.** This frees nothing on the Mac.
Without free-page reporting, a page the guest frees still counts against the VM,
and macOS still faults it in when the guest reuses it.

**Move memory at run time with a balloon faber drives.** This is the only way
memory could shift between CI and the machine as demand changes. Docker Desktop
has the device but never inflates it, so faber would have to run its own VM,
with an image, an engine, networking and a disk. Out of scope. It is recorded as
the direction to take if a static share proves too coarse.

## Amendment (2026-10-17): the pressure gate keeps one job moving

[#160](https://github.com/rejifald/faber-legacy/issues/160). §2c held every Docker claim
while macOS reported `warn` or `critical`. With the VM larger than the share
leaves CI, that pressure cannot fall however little CI runs, because the VM never
gives memory back (the finding recorded above). On the builder, with the VM larger
than its share, the gate parked several claims for a quarter of an hour with zero
jobs running, and said nothing about why.

- **A floor of one.** While no job is running, memory pressure no longer refuses
  one: Docker admission lets one claim through, and a native slot starts one
  native job (re-read once a second, since a job ending does not change the
  hold). The floor is one job in total, Docker and native together, counted
  under the lock Docker admission takes so two arriving at once cannot both
  find the host empty. CI adds load one job at a time but never stops. With a
  job running the gate holds as before. Growing swap has no floor, because it clears once
  nothing adds to it: the machine reports it ahead of pressure, so under both
  the hold is swap and nothing starts.
- **The reason names the VM.** When pressure holds and the VM is larger than
  `machine RAM − keep_memory_gib`, the claim's `waiting.detail` says, for example,
  "macOS memory pressure is warn and the Docker VM (24 GiB) is larger than the
  machine share leaves for CI (16 GiB): resize the VM — running one job at a
  time". The §2a
  encroachment warning, at boot and on the dashboard, says the same.
- **Not chosen:** yielding only once the VM fits the share. That drops the
  pressure gate for exactly the hosts that need it most, while one job at a time
  still protects the machine.

Absent or `0` `keep_*` keys build no gate, so they behave as before.
