---
title: A warning that does not stop jobs can be dismissed
type: adr
status: proposed
date: 2026-11-01
owner: rejifald
---

# ADR-0064: A warning that does not stop jobs can be dismissed

## Status

proposed (2026-11-01). Owner decisions of 2026-11-01, recorded before any code. Amends
the trust-change notice of [ADR-0028](0028-the-control-api-knows-its-caller.md)
(addendum B, cited in `src/audit.rs` and `src/web.rs` but never written into that file):
the notice still holds every trust widening for 7 days, but the operator may now
acknowledge it, which folds it instead of hiding it.

## Context

The popover's attention list, the dashboard's alarms and the Memory view's cards show
every standing issue faber knows about. None of them can be dismissed. A row leaves only
when its cause does, and many causes stay for days on purpose:

- an autolimiter hold ("runner wants 4.91 cores, held at 4 — pin to allow") stays until
  the class is pinned, and comes back after nearly every run of the class;
- a boot notice (git mirrors off beside a dind profile) stays as long as the config does;
- a trust widening stays for 7 days by design, one row per change. Adding four declared
  services in one save is four amber rows for a week.

An operator who has read a row and decided to live with it has no way to say so. The
list then stays amber, and a new issue arrives in a list nobody reads any more. A
list that can never be cleared stops working as an alarm.

Some rows must still never go away by hand: the ones that say faber has stopped doing
its job.

## Decision

### 1. Blocking rows cannot be dismissed; every other row can

A row is **blocking** when it says that jobs which would otherwise run here cannot start
until something changes, or that faber's own record cannot be trusted. Blocking rows
carry no Dismiss. Every other row has a Dismiss, whatever its colour: a red row such as
an OOM-held class is dismissable, because faber still takes jobs.

Blocking today: the GitHub token missing or invalid; GitHub unreachable; paused;
draining; a Docker admission hold; a repo minting hold; the wall red or out of date; a
profile image missing; a runner image fault; macOS jobs refused on a gate while jobs
wait; a golden image not verified while jobs wait; a runner already retired; a git
mirror blocked; repositories opted in that the token cannot administer; a memory or
disk freeze and machine pressure (they clear on their own); and the trust record
shrinking. Each row declares this itself (`blocking: true` in `ui/src/popover/attention.ts`
and `ui/src/dashboard/alarms.ts`). A new row is dismissable unless it declares otherwise,
so a reviewer must ask "does this stop jobs?" of every new row.

### 2. A dismissal is the row's identity, not the row's key alone

A dismissal records the row's key and its **identity**: by default, the row's words with
every run of digits replaced. A dismissed row stays hidden while its key and identity
stay the same. Figures that move with every run ("wants 4.91 cores", "retiring in 5
days") keep it hidden; a different situation under the same key (a learned hold turning
into an OOM hold, a different repo, a new version) shows it again. A row whose occurrence
has a better identity than its words declares it (an update row uses its version).

Rejected: re-raising a row once its cause clears and returns. The autolimiter clears and
re-raises a hold on nearly every run of the class, so the row would come back constantly,
which is the problem this ADR exists to solve.

The cost is that a row with the same wording stays dismissed after its cause clears and
returns. So nothing disappears silently: dismissed rows still active fold into one quiet
line, "N dismissed", which opens to list them, each with Restore.

### 3. The daemon keeps the dismissals

The popover runs in the app and the dashboard is served by the daemon, so they share no
browser storage. Dismissals kept by each would let a row dismissed in the popover stay
amber on the dashboard. So the daemon keeps one list for every surface:

- `POST /api/attention/dismiss {key, ident}` adds or replaces the key's dismissal;
- `POST /api/attention/restore {key?}` removes the key's dismissal, or all of them;
- `POST /api/attention/ack-trust {until_ms}` acknowledges every trust change at or before
  `until_ms` (`null` withdraws the acknowledgement).

Each answers the state snapshot, which carries `attention: {dismissed, trust_acked_ms}`.
The daemon stores keys and identities as opaque strings and decides nothing about rows:
which rows exist and which are blocking is the UI's, from the state it already reads. The
list is bounded (256 entries, oldest dropped; a key of at most 256 bytes and an identity
of at most 4096) and persisted in the state directory as `attention.json`, so a restart
keeps it. A missing or unreadable file starts empty with a warning; it never stops the
daemon.

The routes need the operator class, like every other control route. None widens trust:
a dismissal hides nothing the audit log or `faber status` shows, and stops nothing.

### 4. Trust changes are acknowledged, not dismissed

A trust widening is still not dismissable, one by one or at all. Its row gets
**Acknowledge**, which records the newest change's time. Acknowledged changes fold into one
quiet line, "N trust changes this week, acknowledged", which opens to list them. A change
made after the acknowledgement shows as a full row again. The fold stays for the 7 days
each change is kept, so a change made by a process acting as the operator is still in front
of the operator for a week: what changes is its colour, not whether it is there. The row
that says the trust record shrank is blocking.

## Consequences

- An operator can clear the list down to what stops jobs, and a new issue arrives in a list
  that is empty again.
- A row whose cause returns with the same wording does not alert again. The "N dismissed"
  fold is where it is found.
- Classifying a row is now part of adding one.
- The app's own update rows use the same daemon list as every other row; the daemon does
  not know which are the app's.

## Alternatives considered

- **Dismissals per surface** (app preferences for the popover, browser storage for the
  dashboard). No daemon change, but the two disagree, and the dashboard's storage is lost
  with the browser's.
- **Dismiss by severity** (only amber and grey rows). Colour measures how bad a row is,
  not whether faber still works; an OOM-held class is red and harmless to dismiss.
- **Fully dismissable trust rows.** Simplest, and gives up the reason the notice exists.
