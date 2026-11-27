---
title: The share follows the machine's own use
type: adr
status: proposed
date: 2026-11-24
owner: rejifald
---

# ADR-0065: The share follows the machine's own use

## Status

proposed (2026-11-24). The owner decided in a session on 2026-11-24, before any code was
written:

- CI should use what the machine leaves free and step back when the machine needs it,
  for memory and CPU both.
- Memory moves by restarting the Linux VM at a new size, because the balloon was measured
  not to return memory to the Mac (*Context*).

Revised the same day after an independent check of its statements against `43f3545`. The
check found:

- that ADR-0061 already restarts the Linux VM at idle and by draining, which §3 now
  builds on instead of duplicating;
- that macOS VMs run under a second supervisor;
- that the supervisor protocol has no fallback between versions;
- that ADR-0022's hold is the operator's, so faber's own pauses park claims with a reason
  instead;
- that "elastic" and "resize" already mean other things here;
- three documents that contradict this one.

It amends [ADR-0021](0021-the-machine-share-is-declared-not-left-over.md) for hosts that
turn it on. The declared share stays, as the least the machine always keeps. That ADR's
§4 ("faber never resizes, restarts or reprioritises the VM to enforce a share") no longer
holds for the Linux VM, which faber runs itself
([ADR-0026](0026-job-code-runs-only-inside-vms-faber-owns.md)). A memory restart is one
more of faber's own restarts of that VM, on the paths
[ADR-0061](0061-the-linux-vm-disk-grows-within-a-ceiling.md) §3 laid down for the disk. A
host that does not turn this on is unchanged.

## Context

faber runs on Macs that also do other work: an operator's own containers, a development
session with agents, a build of their own. That load comes and goes. faber's share of the
machine does not:

- `[host] keep_memory_gib` and `keep_cpus` (ADR-0021 §2) say what CI leaves the machine,
  and the Linux VM runs at one size, `[linux_vm] memory_mib`, for as long as it runs.
- So the operator chooses between two bad settings. A share sized for the busy hours leaves
  most of the machine idle in the quiet ones. A share sized for the quiet hours pages the
  machine when its own work comes back, which is the failure ADR-0021 measured.

What we want is a share that moves: CI takes what the machine leaves free and gives it back
when the machine needs it. CPU can move, because a vCPU costs the Mac only while the guest
runs something on it (ADR-0021, *Context*). Memory is the hard part. Once a guest touches
a page, the Mac keeps paying for it.

### The balloon does not return memory

ADR-0021 rejected moving memory with a balloon only because Docker Desktop's VM was not
faber's to drive, and named it as the direction to take if a static share proved too
coarse. Since ADR-0026 the VM is faber's, and the launcher already attaches a traditional
virtio balloon to every VM (`launcher/Sources/Machine.swift`, `baseConfiguration`). Nothing
sets its target yet.

So we measured it before designing around it. The test was a throwaway harness with the
same balloon device as the launcher's, booting the Debian 13 cloud image faber pins
(`src/setup/linux_image.rs`) as a 4 GiB, 2-vCPU VM on Apple's Virtualization framework,
macOS 27. The VM's cost to the Mac was read as the physical footprint of its
`com.apple.Virtualization.VirtualMachine` process.

| step | inside the guest | the VM's footprint on the Mac |
|---|---|---|
| booted | almost all memory free | about half a GiB |
| fill the page cache by reading the disk | about 3 GiB of cache | up by about 3 GiB |
| drop the page cache (`drop_caches`) | the 3 GiB free again | unchanged |
| set the balloon's target to 1 GiB | the balloon holds 3 GiB (`balloon_inflate` in `/proc/vmstat`) | **up** by about half a GiB, then flat for a minute and more; all of it dirty, none reclaimable |
| stop the VM | | all of it returned at once |

- The guest side works. Debian's cloud kernel loads `virtio_balloon`. The device offers
  `DEFLATE_ON_OOM` but not free-page reporting, so `MemTotal` does not shrink as the
  balloon fills, and under memory pressure the guest takes pages back from the balloon
  instead of killing a process.
- The host side does nothing. The guest's kernel zeroes pages as it allocates them
  (`mem auto-init: heap alloc:on`), so inflating briefly *raises* the footprint, and the
  framework then keeps every page. Apple documents only that the VM "may" release the
  pages a guest returns
  ([VZVirtioTraditionalMemoryBalloonDevice](https://developer.apple.com/documentation/virtualization/vzvirtiotraditionalmemoryballoondevice)).
- Lima's maintainers saw the same, and closed their balloon command in February 2026
  because it "does not return RAM" on macOS
  ([lima-vm/lima#4226](https://github.com/lima-vm/lima/pull/4226)).
- The guest uses 4 KiB pages on a host with 16 KiB pages. Whether a 16 KiB-page guest
  kernel changes the outcome was not tested (see *Alternatives*).

The one thing that returned memory was stopping the VM. faber already restarts its Linux
VM through the supervisor, whose `start` takes the VM's `cpus` and `memory_mib`
([docs/vm-supervisor-protocol.md](../vm-supervisor-protocol.md), `start`):

- to revive it (`src/vm/revive.rs`, `linux_revival`);
- to grow its disk (ADR-0061, `src/vm/grow.rs`), which reads the size for the next `start`
  from state the policy shares with the engine driver, and applies it at a start that
  happens anyway, else at an idle moment, else by draining.

A memory restart is the second of these, with a memory size beside the disk size.

## Decision

### 1. The share is declared, or it follows the machine

```toml
# example values throughout
[host]
keep_memory_gib = 16      # the least the machine always keeps
keep_cpus = 2
share = "observe"         # "declared" (absent), "observe" or "follow"

[linux_vm]
memory_mib = 16384        # the size the Linux VM never goes below
memory_max_mib = 40960    # the size it may grow to
cpus = 10                 # vCPUs; when following, not a reservation (§4)
```

- **`[host] share`** takes one of three values. It governs memory and CPU together,
  because both are the same idea: the machine's measured use moves the share.
  - `"declared"`, or an absent key, is ADR-0021 as it stands.
  - `"follow"` turns this ADR on.
  - `"observe"` keeps the declared share and computes every decision below, showing it
    without acting, for example "would restart the Linux VM at 32 GiB: the machine has
    had room for 30 minutes". That is how a host is first turned on, and how the starting
    values in §2 to §4 (the headroom, the steps and the waits) are tuned.
- **`[linux_vm] memory_max_mib`** is the ceiling, named like ADR-0061's `disk_max_gib`.
  `memory_mib` keeps its meaning as the size the VM starts at and never goes below.
- **Validation.** `faber check-config` and the settings save reject `"observe"` or
  `"follow"` in four cases:
  - without a stated `keep_memory_gib` above 0, since there is no share to plan the room
    within;
  - without `memory_max_mib`;
  - with a ceiling below `memory_mib`;
  - with a ceiling above the machine's memory less `keep_memory_gib`.

  A running daemon that boots with any of these warns, shows an attention row, and keeps
  the declared share.
- **This turns a feature on**, so AGENTS.md's rule against narrowing a default does not
  apply. An absent key is `"declared"`, and `"declared"` is today. `LinuxVmFile` denies
  unknown fields, so the new keys go into a config only once the binary that knows them
  runs, as ADR-0021's did.

### 2. The room is what the machine leaves, measured

Each reading of the machine (ADR-0021 §1) yields the memory the machine uses beyond faber,
its **own use**, and from it the room CI may have:

```text
own    = (machine RAM − available) − faber's VMs
needs  = max(keep_memory_gib, peak(own, last hour) + headroom)
room   = machine RAM − needs
target = clamp(room − macOS VMs, memory_mib, memory_max_mib)
```

- **available** is the kernel's own estimate, `kern.memorystatus_level`: an unprivileged
  sysctl giving the percentage of memory memorystatus counts as available. Observe mode
  checks it against the pressure level before any host acts on it.
- **faber's VMs** is what the supervisors report their VMs hold (§5).
- **The peak over the last hour**, not the current reading, decides growth. A machine
  whose own load spiked twenty minutes ago is not planned at its quiet size.
- **headroom** starts at the larger of 4 GiB and a tenth of the machine's memory: a named
  constant, tuned in observe mode before any default is chosen.
- **macOS VMs** is `[macos_vm] count` VMs at `[macos_vm] memory_mib` each, what
  `decide_macos` charges them at, while a `macos-vm` profile is configured. The Linux VM
  grows only into memory the macOS VMs could not use. Otherwise a grown Linux VM would park every macOS job until it shrank.
  ADR-0026's arithmetic is unchanged; only the fixed share becomes the room.
- **Own use is not the co-tenant reserve.** ADR-0021 §3 measures what runs inside the
  Linux VM beside faber's containers. Own use is what runs on the Mac outside every VM
  faber has. Each keeps its own figure.

### 3. Memory moves by restarting the Linux VM

A **memory restart** stops the Linux VM and starts it at `target`, through the supervisor.
The disk, and the local-only runner image on it, are kept (ADR-0007). It is a restart of
faber's own in ADR-0061 §3's sense:

- **The next `start` reads the size from shared state.** It reads the memory size from
  the state the policy shares with the engine driver, as it reads a pending disk size.
  One restart applies every pending change.
- **Revival is held off**, and Gate 0's "Docker is down" fault is muted, while the
  restart runs.
- **The macOS VM charge follows.** The engine driver already reports the memory its VM
  holds with every status (`EngineSink::engine`, `held_mib`), so it reports the new size
  once the VM comes back.
- **No job is killed**, on any path.

Two directions, on ADR-0061 §3's three paths (a start that happens anyway, an idle moment,
a drain):

- **Step back** when the VM has been larger than `target` for 2 minutes, or as soon as the
  machine reports pressure `warn` or `critical`, or swap growth (ADR-0021 §2c,
  ADR-0032 §3). The machine needs the memory now, so a step back drains at once:
  1. The daemon stops claiming Docker jobs. Claims park with the reason "returning memory
     to the machine (ADR-0065)" (ADR-0022 §3).
  2. Busy runners finish, and idle ones are withdrawn, as ADR-0061's drain does.
  3. Then it restarts the VM.

  If the cause clears for 5 minutes before the VM goes idle, the step back is called off
  and claiming resumes at the current size.
- **Step up** when `target` has been at least 4 GiB above the current size for 15
  minutes, with pressure `normal` and swap flat throughout, and the last memory restart
  was at least 30 minutes ago. A pending step up is applied:
  1. at a start that happens anyway;
  2. else at the next idle moment;
  3. else, if claims have been parked on memory all along, by draining after the 2 hours
     ADR-0061 waits.

  It is checked against the room again right before the restart, and dropped if the room
  no longer allows it.
- **A larger VM costs the Mac little until it is used.** The Mac pays for the pages a
  guest touches: the 4 GiB test VM's footprint was about half a GiB at boot. Jobs fill
  it, and a step back returns it.
- **The current size is read, not remembered.** faber takes it from the guest's
  `MemTotal`. A restarted daemon adopts the running VM (ADR-0026) and needs no record of
  the last memory restart. An engine faber has to start from nothing starts at
  `memory_mib`.

### 4. The CPU budget follows the machine's busy cores

With CPU admission on (`[autolimit] cpu`, ADR-0019), its core count becomes:

```text
own_busy = machine busy cores − faber's VMs' busy cores
cores    = min(NCPU, machine cores − max(keep_cpus, own_busy + 1))
```

- **The machine's busy cores** come from the kernel's per-CPU tick counters
  (`host_processor_info`, unprivileged). **faber's VMs' busy cores** come from the same
  supervisor report as their memory (§5).
- Stepping up uses the peak of one-minute averages over the last 10 minutes. Stepping
  back uses the last minute alone.
- **CPU moves admission only.** No vCPU count changes. A running job keeps its quota;
  ADR-0019 made CPU a deadline resource, and squeezing a job mid-run fails it later.
- When following, the vCPU count is no longer a reservation, so `[linux_vm] cpus` can be
  raised towards the machine's cores less `keep_cpus` (the example in §1). With CPU
  admission off, the CPU half of this ADR does nothing, and the dashboard says so.

### 5. The supervisors report what faber's VMs hold

The next supervisor protocol version adds a `usage` request. Its reply holds the summed
physical footprint and the cumulative CPU time of every Virtualization-framework process
the supervisor's own account runs.

- **There are up to two supervisors.** `_faber_vm`'s runs the Linux VM, and, where the
  setup record names the session account, `_faber_mac`'s runs the macOS VMs (ADR-0051).
  faber asks each one and adds the answers. A per-account total needs no list of VMs, and
  cannot be wrong about which VM a process belongs to.
- **It is read with `proc_pid_rusage`.** That needs no privilege for a process of the same
  account and fails with `EPERM` for another account's, so a supervisor can read its VMs
  and faber's own account cannot. Both were checked on macOS 27, and so was the framework
  running a VM's process under the account that created the VM.
- **The protocol does not negotiate** (*The handshake* in the protocol document): the
  request ships in a new
  version, faber and the supervisors update in lockstep as they did for ADR-0061's v4,
  and while their versions differ faber starts no new VMs, as it does today.

### 6. Guard rails

A memory restart stops the VM that serves every Docker job, on hosts nobody is watching
(AGENTS.md), so:

- **Not onto a full disk.** A memory restart alone never restarts Docker onto a full disk
  (ADR-0024). One that also applies a pending disk grow is ADR-0061's exempt restart.
- **Not while the pf wall's verdict is anything but green.** A VM that cannot pass the
  wall at boot would not come back.
- **A failed restart falls back.** If the VM does not come back at `target`, faber starts
  it at the size it had and raises a `vm-memory-step` fault. While the fault stands, the
  host keeps the declared share, as ADR-0061's `vm-disk-grow` fault stops further growth.
  It stands until a daemon restart.
- **Every decision is visible.** The dashboard shows the Linux VM's floor, ceiling,
  current size and `target`, and the machine's own use and busy cores. Each memory
  restart, and each step called off, is logged with its reason and the numbers behind it.

### 7. Scope

- **macOS hosts only.** On a Linux host the room arithmetic would be cgroups, and nothing
  there has been measured. `"observe"` and `"follow"` are refused there, with that reason.
- **Docker admission and the macOS VMs keep their own gates.** Gate 1 sees the new
  `MemTotal` on its next probe after a memory restart, and `decide_macos` sees the Linux
  VM's new size through what the engine driver reports.

## Consequences

**Positive.** A host that follows gives CI the machine's spare memory and cores in its
quiet hours, and gives them back when its own work returns, without the operator re-doing
the arithmetic. The worst state ADR-0021 measured, a VM holding memory the machine is
paging for, now ends when the running jobs finish instead of lasting until someone notices.

**Cost: a restart per step.** The Linux VM is down for its boot, and comes back with a cold
page cache. Shared services start again when next used (ADR-0046). Hysteresis, the 4 GiB
step and the 30-minute spacing keep this to a few times a day on a host whose own load
changes a few times a day.

**Stepping back is as slow as the longest running Docker job.** Until then Docker claiming
stops, which is close to what memory pressure already does today (ADR-0021 §2c, with its
floor of one job), and the machine keeps paging until the restart. Jobs that run for hours
make this slow, bounded by the runners' lifetime cap as ADR-0061's drain is. Observe mode
shows how slow before a host acts.

**The room is an estimate.** `available` is the kernel's own figure, but own use lags by
up to the hour's peak, and the headroom is a guess until observe mode has tuned it. The
pressure and swap signals of ADR-0021 stay as the backstop, and they now also start a step
back.

**A protocol bump**, so faber and both supervisors update in lockstep.

**More moving parts on an unattended host.** It ships declared. Making `"follow"` the
default is a future, release-noted change, made after observe mode on the builder host and
a second Mac has shown the numbers.

**What changes elsewhere, once accepted, in the change that implements it:**

- AGENTS.md, *faber owns the Docker host*. "A bigger VM is not spare capacity" still holds
  for a VM at a fixed size; when following, faber gives the memory back by a restart, and
  the paragraph says so. Restarting through the supervisor gains a third reason besides
  revival and disk growth, by the same stop and start, never by killing the launcher.
- `docs/autolimiter.md`: "faber never resizes the VM" (*The machine's share*) holds only
  for the declared share, and "a VM resize is pending" stops being an example of an
  operator hold for a host that follows.
- `docs/operating.md`: "a VM never gives memory back" gains the restart that does.
- ADR-0021 gains a status line pointing here.

## Alternatives considered

**Drive the balloon.** This would move memory live, with no restart. Rejected because it
was measured not to return memory to the Mac (*Context*). If Apple changes that, §2 still
computes the right target, and only §3's mechanism would change.

**A 16 KiB-page guest kernel, to test the balloon again.** Not tried. Lima's experience
points the same way as ours. Even if it worked, every job image would then run on a kernel
whose page size some software built for 4 KiB pages does not support: a risk to every job,
for a mechanism the restart already provides.

**Stop the Linux VM whenever no Docker job needs it.** This is the draft idle stop
`the-linux-vm-stops-when-no-docker-job-needs-it` (rejifald/faber-legacy#383), which
ADR-0061 already counts as its cheapest moment to grow. It would return all of the VM's
memory between bursts, and every start would apply the room for free. It complements this
ADR rather than replacing it: with steady load the VM never stops, and that host still
needs §3. If the idle stop is accepted, its starts are §3's "start that happens anyway".

**One Linux VM per job, as macOS jobs have.** Memory would return when each job ends, with
no restart policy, which is also how Apple's own container tooling isolates Linux
containers. It is the structural answer, but a large change: the local-only runner image,
the image cache, the git mirrors and the shared services all assume one long-lived engine
(ADR-0007, ADR-0035, ADR-0046). It is recorded as the direction to take if restarting at a
new size proves too coarse.

**Drain at once to step up.** It reaches the larger size fastest, but stops CI each time the
machine has room. Rejected for the reason ADR-0061 drains only after waiting for an idle
moment.

**Never drain to step up.** Under steady load a busy host may never be idle, and that host
is exactly the one that needs the room: ADR-0061's reason, unchanged.

**Lower running jobs' CPU quotas when the machine gets busy.** This would step back on CPU
within seconds instead of at the next admission. Rejected: CPU is a deadline resource
(ADR-0019), and a job slowed mid-run fails later instead.

**Size the VM from the machine's memory and let admission do the rest.** This is ADR-0021's
measured failure: the VM fills to its size and holds it, whatever admission does inside.
