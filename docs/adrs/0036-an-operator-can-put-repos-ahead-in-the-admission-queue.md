---
title: An operator can put repos ahead in the admission queue
type: adr
status: accepted
date: 2026-10-17
owner: rejifald
---

# ADR-0036: An operator can put repos ahead in the admission queue

## Status

accepted (2026-10-17). Applies on restart.

## Context

Docker admission is first-come-first-served with a head set-aside
(`try_admit_charged`, `src/limiter.rs`): the oldest parked claim's reservation is
off-limits to every younger claim, so freed memory accumulates toward it instead
of going to whoever happens to fit.

That is fair between claims and blind to who they belong to. The operator's own
tooling repo, whose CI is small and low-volume, parks behind a long backlog from a
large workload repo, even for jobs of 512 MiB. Its refusal reads like this
(figures illustrative):

> needs 512 MiB, 4096 MiB free of 16384 MiB, 6144 MiB set aside for an older claim

The 512 MiB job would run at once if the older, larger claim were not first in
line. The operator knows which repos matter more; faber has no way to be told.

## Decision

`[autolimit] priority_repos`, a list of `owner/repo` strings, matched
case-insensitively.

- **A parked claim from a listed repo queues ahead of every other repo's.** Within
  each group the order is the existing ticket order, first-come-first-served. The
  head the set-aside protects is the oldest claim of the highest group present: the
  oldest priority claim if any is parked, otherwise the oldest claim.
- **Every gate still applies to it.** Memory Gate 1 and Gate 2, CPU, disk, hold
  and the hard ceiling are unchanged; priority only changes who is the head. A
  priority claim that cannot fit still waits, and a priority head that cannot fit
  the whole budget is still not set aside (it would wedge the pool).
- **No preemption.** Admitted and running jobs are never stopped or shrunk for a
  priority claim. Native-backend jobs do not queue here and are unaffected.
- **Absent or empty is exactly today's behaviour.** With no priority claim parked
  the head is the first ticket, as before; the key is skipped when the file is
  written, and an old config parses unchanged.
- **The refusal names the stall.** A claim set aside around a priority head reads
  `set aside for a priority claim`; the same-group case keeps `set aside for an
  older claim`.
- **It is host-only.** The key lives in the host's config file and is refused by
  `.faber/config.toml` (`RepoConfig` has `deny_unknown_fields`). A repo's in-tree
  config is reachable from a pull request, so a repo able to list itself could
  jump the queue at will, and a fork PR could starve every other repo. The
  operator, who owns the host, is the only party that may rank the repos on it.
  The same reasoning keeps `[gate]` and `dind` host-only (ADR-0013, ADR-0014).

The admission call carries a flag: `Limiter::admit_as(reservation, priority, ..)`,
with `admit` unchanged for callers that have no repo. The orchestrator asks
`Limiter::is_priority_repo` for the claim's repo.

## Consequences

- **Starvation is possible and accepted.** A continuous stream of claims from a
  priority repo can delay normal claims indefinitely, because a younger priority
  claim outranks an older normal one every time. This is acceptable because the
  list is chosen by the operator and is meant for small, low-volume repos. Listing
  a busy repo is the operator's mistake to see: the stalled claims say
  `set aside for a priority claim` on the dashboard. faber adds no aging or
  quota, which would reintroduce the head-of-line delay the key exists to remove.
- A priority head's set-aside can idle memory while it waits for room, exactly as
  an old head's does today.
- The key applies on restart, like `cpu` and `reserve`, and is editable from the
  settings page as a list of lines.

## Alternatives considered

- **A per-repo priority in `.faber/config.toml`.** Rejected: PR-reachable, so a
  repo could raise its own.
- **Numeric weights or several tiers.** More to explain and tune for one real
  need (small tooling repo versus large workload repo); two groups suffice.
- **Aging, so a long-waiting normal claim eventually outranks a priority one.**
  Rejected for now: it reintroduces the delay for the priority repo, and the
  list is small by intent. Revisit if a listed repo ever starves the rest.
- **Reserving a slice of the budget for priority repos.** Idles memory when the
  listed repos are quiet, which is most of the time.
