---
title: A full disk stops admission, not Docker
type: adr
status: accepted
date: 2026-09-26
owner: rejifald
---

# ADR-0024: A full disk stops admission, not Docker

## Status

**Amended 2026-10-29 by
[ADR-0055](0055-a-cache-volume-is-bounded-by-size-and-age.md).** §4's age limit
gains a size limit beside it, and an absent key no longer means faber never
deletes inside a cache volume: with neither key set, faber trims the least
recently used entries under disk pressure.

**Amended 2026-10-28** ([#410](https://github.com/rejifald/faber-legacy/issues/410)). On
a macOS SYSTEM host the Docker volume is two volumes, and the Docker-side gates act
on the tighter of them. See the amendment at the end.

**Amended 2026-10-10 by
[ADR-0032](0032-the-disk-gate-charges-the-job-it-admits.md).** The gate in §1
also charges the job it is about to admit, at 10 GiB for a dind job and 3 GiB
for any other. Below a critical floor of 3 GiB, faber freezes the newest
running job.

accepted (2026-09-26). Amends [ADR-0007](0007-faber-owns-the-docker-host.md):
free disk becomes an admission gate, revival waits for room, the revival cooldown
survives a daemon that answers briefly, and a reclamation pass that freed nothing
is no longer repeated every tick. Amends
[ADR-0005](0005-persistent-per-repo-build-cache-volume.md): faber can age out
cache-volume entries nobody has used. Bears on #69, whose fast path assumes the
revival loop guard holds.

## Context

In September 2026 the builder's Docker died of a full disk, and faber did most of
the pushing.

- For about seven hours the volume backing Docker sat mostly under the 25 GiB
  reclamation floor. The space was held by data that is not CI's, which ADR-0007
  puts outside faber's reach. The janitor ran about a hundred reclamation passes
  in that time. Every one freed nothing, and every one held admission for about
  half a minute: roughly an hour of CI paused to delete nothing.
- That evening faber admitted four Docker jobs at once with under 10 GiB free, and
  a fifth minutes later with only a few GiB left. A job of the watched repo writes
  about 2 GiB in its first minutes: a dependency tarball restored from its cache
  volume, then the workspace. Within ten minutes the volume was all but full, and
  containerd's own metadata writes were failing inside the VM.
- faber then restarted Docker four times in about an hour. The first three booted
  the VM onto a disk with almost no room. Two of those boots logged ext4 errors
  warning of potential data loss within two minutes, in a VM that also holds the
  operator's other containers and their data volumes. Each restart killed the
  jobs in flight (about ten `infra_error` outcomes) and stranded their
  containers: over 20 GB of writable layers until the sweep could reach them.
- ADR-0007's loop guard never engaged. `REVIVE_GIVE_UP_AFTER` counts consecutive
  failed revivals and `revive_cooldown_secs` (1800 s) spaces them, but the first
  probe that succeeds after a revival resets both. A VM on a full disk answers for
  a minute before it wedges again, so the restarts came about 20 minutes apart,
  with nothing to stop them.
- Between the third and fourth restarts Docker Desktop stopped its own engine:
  `no space left on device`, writing its own log. Space came back minutes later,
  freed by hand outside faber. The fourth restart was the first onto a disk with
  room, and it worked.
- Inside the VM, faber's largest footprint was the watched repo's cache volume
  (ADR-0005), tens of GB. Over a third of it was a few top-level files nobody had
  read or written for nearly three weeks, left behind when the repo's workflow
  moved to a different cache layout. One of them was a temp file of several GiB
  from an interrupted write. ADR-0005 makes bounding the volume the consumer's
  job, and the consumer bounded only the layout it still used.

ADR-0007 closed on this: "faber can keep its own footprint from being the cause
and can survive the squeeze; it cannot make room that was never its to take."
Neither half held. faber's admissions took the volume from 10 GiB to nothing, and
its revivals turned a full disk into killed jobs and a filesystem reporting data
loss.

## Decision

### 1. Free disk is an admission gate

- `[host] disk_admission_floor_gib`. While free space on the volume backing Docker
  (the path the janitor already measures) is under the floor, Docker admission
  starts nothing. Running jobs finish. Parked claims report
  `waiting: {gate: "disk", detail: "… GiB free on the Docker volume, below the …
  GiB admission floor"}`.
- One burst of admissions reads one free-space figure, while a job's disk use
  arrives over its first minutes. So each Docker job admitted in the last 10
  minutes counts 3 GiB against the measured free space. Both numbers are
  constants. The incident measured about 2 GiB per job within its first ten
  minutes, across that evening's five admissions, and the constants sit above
  that on purpose. A learned per-class figure, the way memory is learned, can
  replace them once faber measures per-job disk.
- The limiter decides without I/O, so free space is measured every few seconds,
  apart from any Docker call, and the gate reads the stored figure. A measurement
  that failed or is more than two minutes old is not evidence of pressure. The gate
  stays open, just as `below_disk_floor` already treats a failed `df`.
- Native admission is not gated. The gate protects the Docker VM, and a native
  job's workspace need not sit on the volume it measures: on Linux, Docker's
  data-root is often a volume of its own.
- The key copies [ADR-0013](0013-docker-in-docker-is-a-grant-not-a-default.md)'s
  three-state shape, because it tightens admission on hosts that update unattended
  ([ADR-0006](0006-fleet-autoupdate.md), AGENTS.md):
  - **absent**, i.e. *unstated*: no gate, which is today's behaviour. It is warned
    at every boot, shown as its own state on the dashboard, and the settings form
    writes an explicit value the first time it is saved;
  - **`0`**: off, stated;
  - **a positive number**: the gate.

  Flipping the unstated default is a future, release-noted change. This ADR does
  not make it.
- `faber check-config` and the settings save reject a floor above `disk_floor_gib`
  when that is non-zero: admission would stop before reclamation started. At boot
  the same configuration is warned about, not refused.

### 2. Revival waits for room, and its cooldown survives a flap

- While the floor is set and free space is under it, faber does not restart
  Docker. A restart cannot make room, and restarting onto a full disk is what
  produced the killed jobs and the filesystem errors above. The Docker fault says
  so instead: unresponsive for N s, not restarting while the Docker volume has
  X GiB free. The unresponsive clock keeps running, so revival fires on the first
  cycle after room returns.
- Unstated or `0` keeps today's revival, apart from the next point.
- The cooldown and the give-up count reset only once Docker has stayed reachable
  for `revive_cooldown_secs` after a restart, not on its first answer. This change
  is not behind the key. It is the guard ADR-0007 already documents, and it never
  held against a daemon that answers briefly. A daemon that keeps wedging is now
  restarted at most once per cooldown. After `REVIVE_GIVE_UP_AFTER` restarts
  without a stable period in between, faber stops and says so in the fault.

### 3. A reclamation pass that frees nothing is not repeated every tick

- After a pass frees nothing, the janitor does not run another, or take the
  admission pause, until free space has fallen at least 1 GiB below where that
  pass left it, or the sweep interval has passed. The disk fault stays raised
  meanwhile, and the first reading above the floor clears it as before.
- A pass after which free space is back above the floor says pressure lifted and
  clears the fault. Until now it raised "freed nothing" with the post-pass figure:
  in the incident it reported a reading well above the floor as "still below the
  25 GiB floor".

### 4. The cache volume ages out what nobody uses

- `[host] cache_max_age_days`, absent by default. Absent means ADR-0005 as
  written: faber never deletes inside a cache volume. This is a new capability
  rather than a tightening, so it ships off, with no unstated state.
- When it is set, faber deletes each **top-level** entry of each cache volume in
  which nothing has been read, written or changed for that many days. The entry
  is either a file or a directory with everything in it. Its age is the newest
  atime, mtime or ctime anywhere in the entry. This runs on every sweep and in
  every disk-pressure pass, where it comes after the free prunes and before
  runner-image variants, since a variant costs a rebuild and an expired entry by
  definition costs nothing.
- Only whole top-level entries go. faber does not know the consumer's layout, and
  a partly deleted entry is worse than a stale one, because a consumer can mistake
  it for a hit. Inside a directory that is still in use, bounding stays the
  consumer's job. The rule targets the failure seen here: a layout the consumer
  abandoned, which nothing will read again.
- The VM mounts its ext4 `relatime`, so a read refreshes atime at most once a day.
  The minimum is therefore 2 days. `faber check-config` and the settings save
  reject 1. At boot a 1 is warned about and treated as off.
- faber re-checks an entry's age immediately before deleting it, then renames it
  aside, then deletes it. A consumer that looks it up afterwards sees a clean
  miss, never half an entry. An entry left renamed by an interrupted pass is
  recognised and finished on the next one.
- The volumes are the ones labelled `org.faber.kind=cache`, plus, for volumes
  created before labels existed, the exact name faber mounts for each repo it
  serves. Never a prefix match: AGENTS.md reads labels, not names.
- The work runs in a throwaway container of the probe image
  (`autolimit.probe_image`, which reclamation already protects), with no network,
  bounded like every other Docker call.
- Disk pressure does not shorten the age. An entry used within the window is not
  deletable however full the disk is.

## Consequences

- With a floor set, a squeezed host parks its queue with a reason instead of
  admitting jobs into a disk that cannot hold them. faber still cannot make room
  that was never its to take. Parked jobs wait until space returns.
- On the incident's numbers, a floor of, say, 10 GiB would have parked that
  evening's batch, and faber would not have restarted Docker until the disk had
  room again.
- Every host stays unstated, and says so at every boot, until its operator states
  a value.
- A daemon that keeps wedging for reasons unrelated to disk is restarted at most
  once per cooldown. After three unstable restarts it stays down, with a fault,
  until a human acts. Before, it was restarted every 10–20 minutes with no limit,
  killing whatever ran in between.
- While pressure stays unrelieved, the janitor takes the admission pause at most
  once per sweep interval instead of every 2.5 minutes.
- With `cache_max_age_days` set, a layout a consumer abandons is gone that many
  days later, and a consumer that reads an entry at least once per window keeps
  it. A file that must survive a window of disuse does not belong in a cache
  volume, which ADR-0005 already lets the operator delete wholesale at any time.
- One race remains: an entry unused for the whole window, looked up in the instant
  between the re-check and the rename. The consumer sees a miss.

## Alternatives considered

- **Gate at the reclamation floor.** That would have held CI for the seven or so
  hours this host spent under that floor while about 150 jobs succeeded.
  `disk_floor_gib` means "reclaim below this", not "stop".
- **Learn per-job disk use, as memory does.** Right eventually, but faber measures
  no per-job disk today. The constants are measured, conservative, and replaceable.
- **Refuse to revive below the reclamation floor.** The same objection as the first
  alternative: a wedge at 20 GiB free is not a disk wedge.
- **Cap the cache volume's size and evict least-recently-used entries.** Evicts
  entries still in use when a volume grows legitimately. Age targets disuse, which
  is what went wrong here.
- **Age individual files inside the cache.** Can leave half an entry for a consumer
  to mistake for a hit.
- **Leave the cache to the consumer.** It already was the consumer's job. This
  incident is what that looks like after a consumer changes layout.

## Amendment (2026-10-28): the volume Docker fills is read where it is

[#410](https://github.com/rejifald/faber-legacy/issues/410). Since ADR-0026, Docker's
data root on a macOS SYSTEM host is inside faber's Linux VM, on that VM's own
64 GiB disk. Every gate here measured the Mac's volume holding the VM disks, which
the host's `df` can see and the VM's disk lives on. In October 2026 two jobs died
of `No space left on device` inside the VM while the gate read plenty of room: §1
kept admitting, §3's pass never ran, ADR-0032 §2 never froze, and no fault was
raised.

What changes, on that layout only:

- **Two readings.** The disk loop reads the Mac's volume as before and, while
  Docker answers, the Linux VM's disk too, from inside the VM over its Docker
  socket. A Linux host, and a Mac without the SYSTEM layout, have one reading, as
  before.
- **The tighter binds.** §1's gate, §3's reclaim pass, ADR-0032 §2's critical
  floor, ADR-0008's variant-build gate and ADR-0047's mirrors weigh the lower of
  the two readings that are still evidence. The refusal, the freeze cause and the
  janitor's fault name the volume that bound, and `budget.disk.volume` says which.
- **An unreadable VM disk is no evidence.** As a failed `df` always was: the gate
  falls back to the Mac's volume, which is today's behaviour. It is surfaced, in
  `budget.disk.vm_unreadable` and as a `vm-disk` fault, rather than acted on.
  One failure is evidence: a probe that fails with "no space left on device"
  (the disk too full to create the probe container) counts as a zero reading, so
  the gate stays shut, the janitor reclaims and the freeze holds.
- **§2 and macOS VM admission keep the Mac's volume alone.** A restart must not
  boot onto a full host volume, which the VM's disk file lives on. A full VM disk
  is room only Docker can make, by §3's prunes, so holding a restart for it would
  hold Docker down until a reading that needs Docker says otherwise. A macOS clone
  never writes into the Linux VM.

No new key, and nothing narrows a capability a host relies on: on a host whose VM
disk has room, the gates act exactly as before. Where it is full, they now do what
§1 always said they did.

## Amendment (2026-10-31): a pass is judged on the disk it can raise, by what it raised

In October 2026 the builder's janitor held admission for about half a minute in
every two and a half minutes, from morning to evening, and freed no room. Two
things let it:

- **It weighed a disk it cannot reach.** For most of the day the tighter reading
  was the Mac volume holding the VM disks, under the floor. Everything a pass
  deletes is inside the Linux VM, and the VM's disk file is sparse and never
  shrinks ([#423](https://github.com/rejifald/faber-legacy/issues/423)). Each pass
  still paused admission, evicted the runner-image variant parked jobs had been
  resolved onto, and deleted the git mirrors, which the next jobs cloned again.
- **Deleting counted as freeing.** §3's backoff started only after a pass that
  reclaimed nothing. A pass that deleted the git mirrors was "freed" while the
  VM's disk stayed flat, hovering at the 25 GiB floor, so the backoff never
  started.

What changes:

- **The pass is judged on the disk its deletions can raise.** When the Mac
  volume is the tighter reading and the Linux VM's disk was read this tick, the
  pass weighs the VM's disk: whether to run, how much cache to relieve (ADR-0055,
  #427), the variant tier, and its verdict. If only the Mac volume is under the
  floor, there is no pass and no pause. The disk fault says that faber cannot
  free that volume and the operator must. §1's gate and the macOS VM gate still
  read the Mac volume, so admission holds as before. Without a VM reading, the
  tighter reading binds, as before.
- **§3's "frees nothing" means "did not raise free space by 1 GiB".** A pass
  that leaves the disk under the floor and less than 1 GiB higher than where it
  started is fruitless, whatever it deleted, so the backoff starts. 1 GiB is the
  step the backoff already waits for. A pass that leaves the disk at or above
  the floor clears the fault, as before.

No new key. A host whose reclamation really frees room is unchanged.
