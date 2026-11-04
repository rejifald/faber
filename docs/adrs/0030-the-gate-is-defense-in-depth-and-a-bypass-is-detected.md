---
title: The gate is defense in depth, and a bypass is detected
type: adr
status: accepted
date: 2026-10-09
owner: rejifald
---

# ADR-0030: The gate is defense in depth, and a bypass is detected

## Status

accepted (2026-10-09). Resolves [issue #97](https://github.com/rejifald/faber-legacy/issues/97).
Amends [ADR-0014](0014-the-trust-boundary-is-operator-configurable.md) by stating
what the gate does and does not guarantee.

## Context

faber mints a JIT runner for a job it admits. The runner registers with that job's
`runs-on` labels, and GitHub hands it to **any** queued job those labels cover.
That includes a job faber's gate refused. The refused job then runs on the host
after all, and the job the runner was minted for waits for another one.

[ADR-0029](0029-repository-dispatch-is-opt-in-and-refusals-are-visible.md) found
this through `repository_dispatch`, which was harmless. An audit of the builder
showed it is not rare. Over about three weeks, a watched repo's jobs were refused
over two hundred times as `dependabot[bot]` and dozens of times as `workflow_run`.
Every one sampled ran on a `faber-*` runner minted for a different job. The actor
deny-list had been doing nothing at all, and nothing said so.

**faber cannot prevent this.**

- A runner is not pinned to a job.
- `runs-on` belongs to the workflow, so faber cannot add a label only the intended
  job asks for. Extra labels on a runner restrict nothing, since a runner with a
  superset of labels still matches.
- Runner groups restricted to selected workflows exist only for organizations.

The one lever faber holds is not minting. "Stop minting while any refused job is
queued" would make the gate an execution boundary. It would also give anyone who
can queue such a job a switch that stops the host, which is worse than the theft
it prevents.

## Decision

### The gate is defense in depth, and says so

`runs-on` routing is the primary control, as `src/gate.rs` already said. The
invariant is now written down where the gate lives: **every job that can target
faber's labels must be one you would let run here.** The gate decides what faber
*mints for*. It does not decide what executes, and no part of faber claims it
does.

### A bypass is detected, always

faber already reads, each cycle, which job each of its runners actually executed
(`harvest_executed`, the source the autolimiter attributes by). Each such job on a
runner **this host** minted is now evaluated against the gate. A `Refuse` verdict
there is a **gate bypass**, recorded as a `gate_bypass` fault naming:

- the job that ran;
- the runner it ran on;
- the job that runner was minted for.

A bypass belongs to one of two classes, because the response differs:

- **Trust:** fork, untrusted actor, public repo, or an event outside the ceiling.
  The code that ran may be hostile.
- **Policy:** an event the operator narrowed away, or an opt-in event left off. It
  is the repo's own trusted code; the operator just did not want it on this host.

Detection is visibility only, so it ships on. It reaches the audit log, the faults
feed, the dashboard and the tray.

### Enforcement is a grant: `[gate] on_bypass`

`on_bypass = "record" | "enforce"`. **Absent means `record`.**

A release must not withdraw a capability a host relies on (AGENTS.md), and hosts
rely on this one today. In the audit above, a whole class of a repo's CI ran only
by theft. Enforcing by default would break it unattended, on every follower host,
with a failing job as the only symptom.

`enforce` turns on everything below at once. One switch, so there is no
half-enforced state to reason about.

1. **A trust bypass kills the runner's container.** The job has already started,
   so this caps the damage rather than preventing it: no long-lived process, less
   time to plant anything. A policy bypass is never killed. Killing the operator's
   own deployment check mid-run helps nobody. This kill does not ask the runner
   first (ADR-0017): that rule protects legitimate work, and this is not
   legitimate work.
2. **The repo's cache volume is deleted.** A per-repo cache volume (ADR-0005) is
   the one thing faber carries across jobs, so it is where a hostile job's
   foothold would outlive it. faber cannot tell a clean cache from a poisoned one.
   It deletes the volume once no job mounts it, and records that in the same fault.
   This is within what faber may delete: it created the volume, and any job
   rebuilds it.
3. **The repo is held.** A trust bypass means a workflow in that repo routes
   untrusted work to faber's labels, and that does not fix itself. A named
   per-repo hold, in ADR-0022's shape, stops minting for that repo until the
   operator clears it from the dashboard or the tray. Other repos are untouched.
4. **Minting is paused while a trust-refused job is queued.** Theft needs a
   trust-refused job queued with labels L, and a runner whose labels cover L. faber
   sees the first in `refused_queued` (ADR-0029). While it is there, faber does not
   mint runners covering L **in that repo**. Parked claims say why, in ADR-0022's
   waiting slot. The pause lifts by itself when the job leaves the queue. This makes
   items 1–3 a backstop that should not fire. It is the per-repo, trust-only form of
   the rejected global pause, so it stalls only the repo whose workflow is
   misrouted.

### `record` is loud, and temporary

While `record` is in force and trust bypasses have been seen in the last 7 days:

- faber warns at every boot;
- the dashboard shows a distinct line: "N trust bypasses in 7 days — enforcement off".

`enforce` becomes the default in a future, release-noted release. That is a
scheduled migration, not an indefinite third state.

## Consequences

- The deny-list and the event boundary stop being silently ineffective. Either
  they hold, under `enforce`, or the host says each time they did not.
- Under `enforce`, a misrouted workflow costs its repo its CI until the routing is
  fixed. That is the intended forcing function.
- An operator may trust Dependabot, with `denied_actors = []` for example. A
  Dependabot `cooldown` of a few days in the repo then gives what the deny was
  meant to: time for a malicious release to be noticed and yanked. faber's
  compiled default still denies `dependabot[bot]`.
- Detection sees only runners this host minted. A peer's runner is the peer's to
  judge.

## Alternatives considered

**An execution boundary: no minting while any refused job is queued.** Rejected
above. One misrouted job, or one stranger's PR, stops the host.

**Kill every bypass, policy included.** It turns a visible notice into a failed run
of trusted code, which the operator then retries by hand.

**Enforce by default.** Breaks CI that hosts rely on today, unattended. See
`on_bypass`.

**Flag the cache volume for the operator instead of deleting it.** It leaves the
foothold in place for every trusted job that runs before someone reads the fault.
