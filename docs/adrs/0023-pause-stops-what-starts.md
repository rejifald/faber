---
title: Pause stops what starts
type: adr
status: accepted
date: 2026-09-25
owner: rejifald
---

# ADR-0023: Pause stops what starts

## Status

accepted (2026-09-25). Amends [ADR-0007](0007-faber-owns-the-docker-host.md): the
janitor's reclamation pause now holds parked claims too, as that ADR always said
it did. Amends [ADR-0022](0022-a-hold-is-named-and-a-parked-claim-says-why.md):
pause joins the hold as a gate in admission. Resolves #67.

## Context

Two things set faber's pause flag: `POST /api/pause`, which is the Pause button in
the dashboard and the tray, and the janitor's guard around a disk reclamation
(ADR-0007). The flag stopped only one thing, *claiming*. A claim already parked,
in Docker admission or waiting for a native slot, still started its runner the
moment it fit. So:

- ADR-0007 says reclamation "shuts admission before it deletes anything". It did
  not. A parked claim could create its network and start its daemon in the middle
  of a reclamation. That has been harmless only because reclamation deletes
  nothing a starting job uses.
- An operator who pressed Pause could watch parked work start anyway.

This came to light while sweeping for other ways to stop admission after
ADR-0022.

## Decision

- **Paused means nothing new starts.** Running jobs finish. Parked claims stay
  parked, each reporting `waiting: {gate: "paused", ...}`, and resume releases
  them at once.
- Pause is a gate in the limiter, checked right after the hold. A native claim
  waits out a pause before it takes a slot.
- The limiter owns the only pause flag, and it records who paused: the operator
  or the janitor. The orchestrator reads that same flag, so claiming, admission
  and the native slots cannot disagree.
- An operator's pause replaces the janitor's, and the janitor lifts only its own.
  Before this, an operator who paused during a reclamation was resumed silently
  when the reclamation ended. The operator's Resume still lifts either pause.
- Pause and the hold remain different tools. Pause is transient and global: stop
  now, briefly. The hold is persistent and covers Docker admission only: stop
  until someone releases it, and say why.

## Consequences

- The janitor's reclamation window now holds admission, as ADR-0007 described.
- **Pausing and then waiting for `inflight: 0` no longer finishes while jobs are
  queued**, because parked claims stay in the list. The install procedure in
  AGENTS.md now uses `POST /api/drain`, which cancels parked claims and waits only
  for admitted ones. The autoupdater already drains that way.
- A claim taken by a cycle that raced the start of a drain is released at once.
  Otherwise it could sit behind a pause, waiting for a resume a draining host may
  never get.
- The autoupdater counts a host as idle only when its in-flight list is empty. A
  paused host with queued jobs therefore installs only after the updater escalates
  to a drain, which cancels parked claims. With escalation turned off (`0`), it
  waits for a resume.
- Native claims that a pause sent back re-queue in the order they wake, not the
  order they arrived.
- Image resolution and variant builds (ADR-0008) are not gated. They build an
  image but start no runner.
- The tray and dashboard now say that nothing new starts, rather than that
  nothing new is claimed.
- The change reaches other hosts through a release (ADR-0006). A peer on an older
  release keeps the old meaning until then.

## Alternatives considered

**Keep pause as "claim nothing new" and give the janitor a gate of its own.**
Rejected. It would make ADR-0007 true, but the Pause button would still mean
something narrower than it looks, and parked claims would still start under an
operator's pause.

**Make pause cancel parked claims, as drain does.** Rejected, because that is
drain. A pause is meant to be undone. Cancelled claims would claim again on
resume and pay a round of registration churn for nothing.

## Validation

- Unit tests cover:
  - `try_admit` reporting `paused`, with `held` still ahead of it
  - a parked admission held by a pause and admitted at once on resume
  - the janitor's pause parking admission and lifting it on drop, but never
    lifting an operator's pause, including one set during the reclamation
  - the operator's Pause and Resume buttons
  - a native claim waiting a pause out before taking a slot
  - a claim that raced a drain being released
