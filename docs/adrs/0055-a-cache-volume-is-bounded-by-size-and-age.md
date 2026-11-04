---
title: A cache volume is bounded by size and age
type: adr
status: accepted
date: 2026-10-29
owner: rejifald
---

# ADR-0055: A cache volume is bounded by size and age

## Status

accepted (2026-10-29). Amends [ADR-0005](0005-persistent-per-repo-build-cache-volume.md)
and §4 of [ADR-0024](0024-a-full-disk-stops-admission-not-docker.md): bounding a
cache volume is no longer only the consumer's job, and an absent key no longer
means "faber never deletes inside a cache volume". Found under
[#423](https://github.com/rejifald/faber-legacy/issues/423).

## Context

ADR-0005 gave each repo a persistent cache volume and left bounding it to the
consumer. ADR-0024 §4 added one bound faber may apply, `cache_max_age_days`, off
unless the operator sets it.

On the builder in October 2026, with `cache_max_age_days` set, one watched
repo's cache volume held nearly half of the Linux VM's 64 GiB disk. The repo
reads that cache on every run, so nothing in it ever ages out. Once #411 made the
disk gate read the VM's disk, the janitor fell below its 25 GiB floor on every
pass. Each pass deleted the git mirrors (under a GiB, and useful) and paused
admission every 120 s. It never touched the volume that held the space. An age
bound cannot bound a cache that is in use, and nothing else bounded it.

## Decision

### 1. Two limits, either one deletes

`[host]` gains `cache_max_gib`, beside `cache_max_age_days`. Both limits apply
per volume, and both are optional.

- **`cache_max_age_days`:** an entry goes when nothing in it has been read,
  written or changed for that many days, as in ADR-0024 §4.
- **`cache_max_gib`:** while a volume holds more than this, its least recently
  used entries go, oldest first, until it holds no more than the limit.

An entry goes if it breaks **either** limit that is set. `0` sets a limit to
"none" on its own axis. The minimum is still 2 for the age and 1 for the size.

### 2. Set by the operator, or left to faber

- **Either key is set:** the operator has chosen the policy. faber applies the
  limits that are set, on every sweep and inside every disk-pressure pass, and
  nothing more. A key that is absent while the other is set imposes no limit.
- **Neither key is set:** faber decides. It deletes nothing inside a cache volume
  while the disk is healthy. When the janitor's pressure pass falls below
  `disk_floor_gib` and its other cheap reclaims (dangling images, build cache)
  have run, it deletes the least recently used entries **across all cache
  volumes**. It stops once the scan's sizes say free space would reach the floor
  plus 5 GiB. This comes before the git mirrors and the runner-image variants: a
  cache entry costs a slower run, while a variant costs a build. Disk pressure
  sizes the deletion, so the cache keeps everything the disk can afford.

### 3. What keeps a size-driven delete safe

Everything in ADR-0024 §4 still holds: only faber's cache volumes, whole
top-level entries, a read-only scan, re-checked, renamed aside, then deleted.
Timestamps cannot show that an entry is not being read. The VM's disk is mounted
`relatime`, so a read refreshes atime at most once a day. An entry a running job
is restoring from can look hours idle. So size and pressure deletes add two
rules, and an age-driven delete is unchanged:

- **Never in a mounted volume.** Right before deleting, faber asks Docker whether
  any running container mounts the volume. If one does, or the answer fails,
  that volume loses only its expired entries this pass, and nothing for size or
  pressure.
- **Under the admission pause.** Size and pressure deletes run while admission is
  paused, so no job can mount the volume between that check and the delete. The
  pressure pass already holds the pause. The sweep scans with admission open and
  takes the pause only when its plan holds a delete for size.

A busy repo's volume is therefore trimmed between its jobs. If it stays over its
limit because a job always mounts it, the disk gate holds admission meanwhile.

**As a second safeguard, an entry with any timestamp in the last 2 hours is never
deleted for size or pressure.** One cutoff covers the whole batch: the newest
timestamp among the chosen entries. The delete script keeps anything newer than
the cutoff, so an entry written after the scan survives.

The pressure amount is computed from a free-space reading taken after the free
prunes, so cache is deleted only for what they could not free. If that reading
fails, nothing is deleted.

## Consequences

- A host that set `cache_max_age_days` and nothing else now has a policy that
  bounds nothing in use. Its operator should set `cache_max_gib` as well, or
  remove both keys and leave the policy to faber.
- A host that set neither now gets deletions under disk pressure, where before
  it got none. That is the intended change: faber owns the CI disk (ADR-0007),
  and the alternative was a full disk.
- A cache laid out as one huge top-level entry is deleted whole. faber does not
  know the consumer's layout, and half an entry is worse than none (ADR-0024 §4).
  A consumer that wants finer eviction keeps finer top-level entries.

## Alternatives considered

- **Size-only, applied by pressure even when the operator set limits.** Rejected:
  the operator's limits would no longer mean what they say.
- **A larger VM disk** (#423's open question). It postpones the problem: an
  unbounded cache fills any size. Size the disk after the cache is bounded.
- **Shrinking the cache from the consumer's workflow.** That fixes one repo, and
  ADR-0005 already showed consumers don't keep it up.
