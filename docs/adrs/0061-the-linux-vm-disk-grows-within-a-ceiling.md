---
title: The Linux VM's disk grows within a ceiling
type: adr
status: proposed
date: 2026-11-01
owner: rejifald
---

# ADR-0061: The Linux VM's disk grows within a ceiling

## Status

proposed (2026-11-01). The owner picked the three choices below in a session on
2026-11-01: grow when idle and drain only if no idle moment comes, resize the
guest with a privileged one-shot, and a default ceiling of 128 GiB.

Revised the same day after an independent review. The review found that the
change needs a protocol bump; that a failed resize could loop; that the idle
restart raced runner assignment; that the probe image is a mutable tag; and that
the host check undercounted the sparse file. It also left a list of smaller
gaps. §7 adds what a cleanup check found the same day: the caches' limits add up
past the disk.

It amends [ADR-0024](0024-a-full-disk-stops-admission-not-docker.md) (the Linux
VM's disk is no longer a fixed 64 GiB) and builds on the engine VM of
[ADR-0026](0026-job-code-runs-only-inside-vms-faber-owns.md). It touches the
draft `the-linux-vm-stops-when-no-docker-job-needs-it` (PR #383): an idle stop
is the cheapest moment to grow, see §3.

## Context

On a macOS SYSTEM host, Docker's data root is on the Linux VM's root filesystem.
That filesystem is on a sparse raw disk fixed at 64 GiB (`DISK_BYTES`,
`src/setup/linux_image.rs`). The size is set once, when the image is built:
setup extends the file with `set_len`, then cloud-init's `growpart` and
`resize_rootfs` grow root into it. After the build, `provision.sh` disables
cloud-init. The runtime disk, `<vm_home>/linux/disk.raw`, is an APFS clone of
the image that the supervisor makes on first start and never replaces, because it
holds the only copy of the runner image (AGENTS.md).

In November 2026 the builder's VM was down to a few GiB above the janitor's
floor, with the Mac's own volume far from full. The disk gate (ADR-0024 /
ADR-0032 §1) admitted a few Docker jobs and then held back. The janitor's 25 GiB
reclaim floor ran over and over. #423 records the same disk filling and failing
jobs with `No space left on device`. The ramp-charge double count is fixed
separately (ADR-0032's 2026-11-01 amendment). This ADR deals with the cause
underneath: a fixed 64 GiB is too small for the jobs this host runs, while the
Mac has room to spare.

Three facts constrain any fix:

1. **Virtualization.framework reads a disk's size only at boot.** No API resizes
   an attached `VZDiskImageStorageDeviceAttachment`, so growing means restarting
   the VM, and a restart kills every Docker job in it.
2. **Nothing in the guest grows root at boot.** cloud-init is disabled after the
   build. A unit added to the image would never reach a running host, because its
   runtime disk is never recreated.
3. **The file grows; it never shrinks.** It is sparse on APFS, and blocks the
   guest frees do not come back to the Mac (#423). Every gigabyte the VM may grow
   into is a gigabyte the Mac may eventually lose.

## Decision

### 1. A ceiling, in config

`[linux_vm] disk_max_gib`, default **128**. It must be at least the built size
(64); `64` turns growth off. The disk grows in steps of **32 GiB**, never past the
ceiling, and never shrinks. Lowering the key below the disk's current size
changes nothing on disk and stops further growth.

### 2. When the disk needs to grow

The daemon decides, from the disk loop's readings of both volumes
(`read_linux_vm_disk` and the host volume). A grow is **pending** when all of
these hold:

- **The disk is short, and cleaning did not fix it.** One of two triggers:
  - The Linux VM's disk has been below the janitor's floor (`disk_floor_gib`)
    for 30 minutes running, and the janitor's last pass in that time freed
    nothing or backed off (ADR-0024 §3).
  - The disk gate has refused a Docker job for 30 minutes running. This trigger
    has no janitor condition: above its floor the janitor runs no pass at all.
    It is there because the November 2026 incident sat *above* the janitor's
    floor (a few GiB over it, with the gate holding).
- **The guest has used what it was given.** The §5 container's last run, at
  this file length, reported that `growpart` and `resize2fs` found nothing left
  to grow. It reports the file length it ran at, so the daemon can tell. A `df`
  figure is not compared with the file's length: ext4's metadata, the ESP
  before root and the backup GPT header after it leave a gap that varies.
  Without that report no grow is pending, and the next start retries the
  resize. This keeps a failed resize from setting off a restart and another
  32 GiB, over and over, up to the ceiling.
- **No `vm-disk-grow` fault stands.**
- **The disk is below `disk_max_gib`.**
- **The Mac's volume can hold it.** The check must still pass if the VM fills
  everything it is promised: `host free − (file length − blocks allocated) −
  step ≥ disk_floor_gib`. The file is sparse, so what the guest has not written
  yet is a debt on the Mac's volume. That volume also holds the macOS VM
  clones, so its free space is read fresh, not assumed. The step shrinks to fit,
  down to 8 GiB; below that, no grow is pending.

Pending is a state the daemon recomputes, not one it stores. A daemon restart
works it out again from the next readings. The 30-minute clocks restart too,
which errs towards waiting.

The pending grow is shown on the dashboard's engine disk rail and in
`faber status`, with what it waits for.

### 3. When a pending grow is applied

Growing needs a VM restart (fact 1), and no restart for growth kills a job:

1. **At the next start that happens anyway.** An idle stop (PR #383, if
   accepted), a revival (ADR-0007), `faber engine stop`, or a daemon boot that
   finds the VM stopped all start the VM at the new size. The engine driver
   restarts a stopped VM by itself (`engine.rs` `run`), so `bring_up`'s `start`
   reads the target size from state the policy shares with it, not from config.
2. **Else at the next idle moment.** When no Docker runner is busy, the daemon
   stops claiming Docker jobs. It then withdraws and removes the idle runners,
   one `retire_unwatched` pass as `keep_off` does. JIT runners are not pinned to
   a job, and GitHub can hand an idle registered runner a job at any moment, so
   "nothing is busy" is not enough. It checks again that no runner is busy;
   if one is, it resumes claiming and waits for the next idle moment. Then it
   stops the VM, and the driver starts it at the new size.
3. **Else, after 2 hours pending, by draining.** The daemon stops claiming Docker
   jobs (claims park with the reason "growing the Linux VM's disk"). It waits
   for busy runners to finish, each at most until its lifetime cap, as
   `keep_off` does. Then it restarts as in step 2. macOS VM jobs are not
   affected.

A grow restart is faber's own, so while one runs:

- **Revival is held off**, and Gate 0's "Docker is down" fault is muted, until
  the driver reports the VM in service or the start fails.
- **The grow restart is exempt from ADR-0024's rule against restarting Docker
  onto a full disk.** That rule is about revival, which cannot make room. A grow
  restart is the one restart that does.

An operator can apply a pending grow at once with `faber engine grow` (the same
drain, with no wait), or hold it by setting `disk_max_gib` to the current size.

### 4. Who extends the file: supervisor protocol v4

The runtime disk belongs to `_faber_vm`, not to the daemon. The supervisor
protocol goes to **version 4** (`docs/vm-supervisor-protocol.md`,
`src/vm/proto.rs` `PROTOCOL_VERSION`, `launcher/Sources/Proto.swift`). v3 parses
`start` with a closed list of fields and answers an unknown field with
`bad_request`, so a new optional field is a protocol change, not an extension.
v4 adds:

- **`start.disk_bytes`** (optional, Linux only). When the VM is stopped and the
  file is shorter, the supervisor extends it with `ftruncate` before it builds
  the configuration. It refuses (`bad_request`) any of these:
  - `disk_bytes` on a macOS VM
  - a Linux boot in place without `--linux-dir`, where the disk is root's image
    at 0444
  - a size smaller than the file (it never shrinks)
  - a size that is not a multiple of 1 MiB
  - a size above 1 TiB, its own bound whatever the daemon asks
  - growth larger than the free space on the volume holding the file
- **`VmRecord.disk_bytes`** and **`disk_allocated_bytes`** for a Linux VM: the
  file's length and the blocks it holds, which §2 reads.

The wall's Linux check is unchanged. It checks the disk by device and inode
(`Wall.swift`), and extending the file in place keeps both.

### 5. The guest grows root at every start

Once Docker answers after a start, the daemon runs one container: privileged,
network-less, short-lived. It enters PID 1's mount namespace, finds root's disk
and partition with `findmnt` (never hard-coded), and runs `growpart` on them,
then `resize2fs` on root (an online ext4 resize). Both do nothing when there is
nothing to grow, so this runs at **every** start. That also covers a crash
between extending the file and resizing the filesystem.

- **Its image is an upstream image pinned by content digest, never
  `probe_image`.** The probe image is an operator-set, mutable tag (`busybox` by
  default). It runs unprivileged and read-only today; run privileged, it would
  give root in the guest to whatever that tag resolves to. faber publishes no
  images (the owner's no-public-artifacts rule), so it cannot ship its own.
  Instead the binary names one upstream reference by digest
  (`busybox@sha256:…`, chosen at implementation for an arm64 `nsenter`
  applet). faber pulls it once, checks its image ID against the digest before
  every run, and never re-tags it. `growpart` and `resize2fs` run from the
  guest itself, through the namespace.
- **The trust it adds is narrow, but real.** Root in the guest, for one fixed
  command, from one image pinned by digest. On a host with no dind grant
  (ADR-0013) nothing else runs privileged, so this is new there. Job code never
  reaches it: no job chooses its image, command or flags.
- **It fails safe.** A failure raises a `vm-disk-grow` fault and changes nothing
  else: the VM keeps the size it had, the disk gate keeps reading real free space,
  and §2 grows no further while the fault stands. A guest so full that Docker
  cannot create the container fails the same way. The next start tries again,
  after the janitor has had a pass.
- **The tools are checked.** The guest needs `growpart` (cloud-guest-utils) and
  `resize2fs` (e2fsprogs). Debian's genericcloud image ships both for cloud-init.
  Implementation confirms that on the live VM, and that root is the disk's last
  partition, which growpart needs. The first start checks for both tools and
  raises the fault if either is missing.

### 6. Size is read, not assumed

The VM's disk size comes from the supervisor (`VmRecord.disk_bytes`) and from
`df` in the guest, not from `DISK_BYTES`. The dashboard's "of 64 GiB" becomes the
actual size and the ceiling. `DISK_BYTES` stays the size a new image is built at.

### 7. Caches are bounded against the disk they live on

The cleanup check of 2026-11-01 found the caches' fixed limits adding up past a
64 GiB disk: the npm/LFS mirror's cache volume at its default `max_size_gib = 20`,
each job-cache volume at the `cache_max_gib` the operator had set, and the janitor
wanting its default 25 GiB free. Growing the disk does not fix a sum that was
never checked. At boot and after every grow, faber checks that the cache limits
plus `disk_floor_gib` leave at least half the VM's disk for jobs and images. If
they do not, it warns, as `check-config` does for other overcommits, and keeps
the operator's numbers. The sum counts:

- **The mirror's one cache volume:** `[mirror] max_size_gib` (default 20), when
  `[mirror]` is set.
- **Each existing cache volume (ADR-0005):** `cache_max_gib`, which is a limit
  per volume. Unset means "faber decides": nothing is deleted until the disk
  is under pressure. Then the volume counts at its current size, and the
  warning says no limit is set.
- **Git mirrors and runner-image variants** at their current size. They have no
  configured limit.

Deriving the limits from the disk size is left to a later ADR.

## Implementation notes

Added with the implementation (2026-11-01). None changes a decision above; each
pins down what the text left open, or a case the tests found.

- **The image (§5)** is `busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec`,
  busybox 1.37.0's index; its arm64 image ships the `nsenter` applet. faber pulls it
  with `--platform linux/arm64` and accepts three image IDs before every run: the
  index digest (the containerd image store's ID), its arm64 manifest
  (`sha256:d82c2ab9…`) and that manifest's config (`sha256:ad4f11f7…`, the classic
  store's ID). Anything else is refused and raises the fault.
- **"Nothing left to grow" (§2, §5)** is: `growpart` grew or found nothing to do,
  `resize2fs` succeeded, and a following `growpart --dry-run` finds nothing left. Root's
  partition comes from `findmnt`, its disk from `lsblk`'s `PKNAME`, its number from
  sysfs. The tools check covers `findmnt`, `lsblk`, `growpart` and `resize2fs`.
- **The clocks restart after a grow (§2).** A grown disk is a new situation, so the
  30-minute clocks start over when the supervisor reports a larger file. Without that,
  one long run of gate refusals set off a second step the moment the first restart
  finished, before anyone could see whether 32 GiB helped.
- **"No Docker runner is busy" (§3 step 2)** is read as no admitted Docker claim. That
  is stricter than busy (an idle registered runner counts), so the idle path only
  starts on a host with nothing up; the two retire passes then guard the race as
  written. A busy host reaches step 3 instead.
- **Only a VM in service is stopped for a grow.** A VM that is down, starting or
  failing is starting anyway, and its next start takes the target (step 1).
- **A refused grow.** If the supervisor answers a `start` with `disk_bytes` with
  `bad_request`, the driver starts the VM again without it, at its current size, and
  raises `vm-disk-grow`. That fault stands until `faber engine grow` or a daemon
  restart. A failed resize's fault clears at the next resize that succeeds.
- **Size of an adopted VM (§4).** The claim marker does not name a Linux VM's image,
  and changing its format would leave a rolled-back supervisor unable to read it. So a
  supervisor that adopts a running Linux VM reports the disk of the one bundle in
  `--linux-dir`, and no size when there are several. Until it reports one, no grow is
  pending.
- **§7 warns at runtime only**: at the grow loop's first reading of the disk's size,
  and after every grow, in the daemon's log. Current sizes come from
  `docker system df -v` (volumes, by `org.faber.kind`) and `docker image ls` (variants,
  by label). `check-config` does not run it: it cannot read the VM's Docker.
- **§6 on the dashboard.** The resources view's disk rail is specified
  (`docs/design/dashboard/resources.md`) but not built yet. This change ships what it
  reads, `budget.disk.vm_grow`, and the spec (D9); `faber status` prints the `disk`
  line now.

## Rollout

The ceiling key ships with growth live (default 128) on both hosts. A grow is
logged, audited (`engine_disk_grown`, from → to) and shown, so the first ones are
watched.

The protocol bump means faber and its supervisor must update **together**. Under
ADR-0026 they already do, since the launcher's version matches faber's. But a
mismatched pair, old supervisor with new daemon or the reverse, starts **no VM at
all**, macOS or Linux, until both sides update: the handshake refuses a version it
does not speak. A manual install updates both in one step (`docs/manual-update.md`).
A follower does the same through the release channel. The peer host goes second,
after the builder has grown once.

Validate on the builder first:

1. Confirm `growpart`, `resize2fs` and the partition layout in the live guest.
2. Set `disk_max_gib = 96`, and push the VM below the floor with a scratch volume.
3. Watch one idle-path grow.
4. Check the disk gate's reading after it, `df` in the guest, and the file's
   length and allocated size.

## Consequences

- Batches caused by a too-small VM disk end once it grows. A host whose janitor
  reclaims enough never grows at all.
- The Mac loses up to `disk_max_gib − 64` GiB over time, for good, until the VM
  is rebuilt. The ceiling and the host check in §2 bound it.
- A grow costs a VM restart:
  - On the start and idle paths, nothing.
  - On the drain path, Docker admission pauses for up to the 2 h wait plus the
    longest running job's remaining time, bounded by the 3 h runner lifetime cap.
  - It never kills a job.
- One privileged container per engine start, from faber's own pinned image.
- A protocol bump, so faber and the supervisor update in lockstep.

## Alternatives considered

- **A larger fixed `DISK_BYTES`.** It only reaches newly built images, and a
  running host never rebuilds its runtime disk. It also takes the Mac's space
  whether or not the host needs it.
- **A second data disk for Docker.** The wall refuses any other disk set today.
  Moving Docker's data root would migrate the runner image and every volume, and
  the data disk would still be fixed-size.
- **Leave the caches' limits alone (§7).** A grown disk would hide the
  overcommit until it grew to the ceiling.
- **A boot unit in the guest image.** The owner rejected it (fact 2): it never
  reaches existing hosts without replacing their runtime disk.
- **Drain as soon as a grow is pending.** It grows fastest, but pauses CI each
  time. The owner chose idle first.
- **Grow only when idle.** Under steady load a busy host may never be idle, and
  that host is exactly the one that needs to grow.
- **Lower the admission floor.** This repeats #106.
