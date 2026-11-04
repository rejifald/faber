---
title: faber owns the Docker host's liveness and disk hygiene
type: adr
status: accepted
date: 2026-08-26
owner: rejifald
---

# ADR-0007: faber owns the Docker host's liveness and disk hygiene

## Status

accepted (2026-08-26). Decision recorded ahead of implementation, because the
principle is what matters and it should bind every agent and every future
change, not just the patch that first implements it.

## Context

faber is the Docker manager. It is the only thing on the host that provisions,
sizes, runs and reaps Docker workloads, and it is the component whose whole
purpose is that CI jobs get a working sandbox. Everything else on the machine
is a consumer of Docker; faber is its operator.

The incident that prompted this ADR made the gap concrete. The host's data volume
filled to 100% (most of it the operator's own data, not CI's doing). The Docker
daemon wedged so completely that its own `/_ping` on the unix socket timed out.
faber detected this correctly and did the right
defensive thing: [`Limiter`]'s Gate 0 flipped `docker_reachable: false` and
refused to admit, so no job was fed into a dead daemon — the failure mode of a
previous incident, where about ten jobs were admitted into a dead Docker and
stranded for more than three hours.

But detecting it was the end of faber's involvement. It then sat and logged
`mem-available probe failed: docker run --rm exceeded 180s — daemon
unresponsive` every three minutes for about half a day while CI was completely
blocked, waiting for a human to notice and restart Docker Desktop by hand. The
backend process had been wedged for days.

That is the wrong division of labour. A manager that can diagnose the thing it
manages, and then does nothing but wait for a person, is a monitor. "Refuse to
make it worse" is a floor, not the job.

The same argument applies one step earlier. The wedge was caused by disk
exhaustion, and disk exhaustion is largely *earned by CI*: images, per-job dind
volumes, per-repo caches and build cache accumulate with every run. `sweep.rs`
already reaps faber's own leaked containers, networks and volumes, but nothing
watches total disk pressure or reclaims the space that CI churn creates. The
janitor keeps faber tidy; it does not keep the host alive.

## Decision

**faber is responsible for keeping Docker alive and healthy.** Concretely:

1. **Revive, don't just refuse.** When the daemon is unresponsive for long
   enough to be a wedge rather than a hiccup, faber restarts it and confirms it
   came back, rather than waiting for a human. Gate 0 stays exactly as it is —
   refusing admission remains correct *while* Docker is down; revival is what
   happens next, not instead.
2. **Keep disk pressure off the cliff.** faber watches free space on the Docker
   host and reclaims what CI created — dangling images, build cache, its own
   orphaned volumes — before exhaustion wedges the daemon, rather than after.
3. **Maintenance is faber's, not the operator's.** Anything a human would have
   to do by hand on a routine basis to keep CI running is a missing faber
   feature. An agent that finds itself running `docker prune`, killing the
   backend, or restarting Docker Desktop to unblock CI should treat that as a
   bug report against faber and implement it there.

## Boundaries

Owning the host is not licence to delete the operator's data. faber reclaims
only what CI itself produced:

- **Never** a blanket `docker system prune -a --volumes`.
- **Never** named volumes it did not create. A host can run the operator's other
  containers, with real data volumes, beside CI; they are not faber's to reap.
- **Never** an image or container belonging to a workload faber did not
  provision.
- Revival restarts the Docker *daemon*, which is disruptive to any other
  container on the host — so it is reserved for a daemon that is already
  unusable, where there is nothing left to disrupt.

## Every resource faber creates is marked

Owning the host only works if "is this mine?" has an answer. It used to be a
*naming convention*: faber created `faber-<kind>-<id>` and everything that
reaped it matched that prefix. A convention is not a fact — it cannot separate
faber's own `faber-dind-…` from an unrelated compose project that happens to
produce `myproj-faber-dind-1` (the sweep carries explicit shielding against
exactly that), and it says nothing about *who is responsible for removing it*,
which is the question this ADR turns on.

So every container, network and volume faber creates carries Docker labels:

| label | meaning |
|---|---|
| `org.faber.owned=true` | the ownership fact — the filter reclamation keys on |
| `org.faber.kind` | `runner`, `dind`, `net`, `vol`, `cache` |
| `org.faber.repo` | the repo whose CI it was created for |
| `org.faber.id` | `<job_id>-<mint_ts>`, tying it to one claim |

`docker ps -a --filter label=org.faber.owned=true` now returns faber's
resources and only faber's, whatever anything is named, and a stray resource
found by hand explains itself — which repo, which claim — instead of having to
be decoded from a name.

Per-job volumes are created explicitly for this reason. They used to appear as
a side effect of `docker run -v name:/path`, and an implicitly-created volume
carries no labels, which would have made them the one thing faber owns that
cannot say so.

The janitor now finds its candidates by these labels and reads the job id from
`org.faber.id` rather than carving it back out of a name. What the labels
unlocked in practice: the persistent build cache is shielded by *being*
`org.faber.kind=cache` instead of by `cache-` happening not to be a prefix the
sweep knew; a dind is paired with its runner on the claim they share rather than
on a matching name suffix; and `myproj-faber-dind-1` is simply not in the
listing, so there is nothing to shield against.

The name listings survive beside them as a fallback, contributing only resources
the label listings did not already return. Two things keep them: resources
created before this shipped carry no labels at all, and the per-job volume is
still created unlabelled whenever its explicit `docker volume create` fails and
`docker run -v` conjures it instead. They retire when that second case does.

## Reclamation pauses admission first

Bulk deletion against a live pool is not safe, and this was learned by doing it:
a manual cleanup sweep run while jobs were provisioning made a few of them fail
with `network faber-net-… not found` — the per-job network disappearing in the
seconds between faber creating it and starting dind. They self-healed on retry,
but a reclamation that costs CI runs is not maintenance, it is an outage with a
tidy motive.

So the janitor shuts admission before it deletes anything, waits out any ramp
already under way, reclaims, and lifts the pause again — through a guard that
releases on drop, so an early return or a panic cannot leave CI paused. It never
lifts a pause the *operator* set: that is someone's deliberate decision, and
resuming it would silently overrule them.

The wait is a fixed window rather than a precise one, because a claim's
`admitted` flag stays true for its whole run — faber cannot yet tell "half-way
through building its sandbox" from "running a twenty-minute job", and waiting
for the latter would mean never reclaiming on a busy host.

## Consequences

- faber gains a privileged, host-affecting responsibility, so each such action
  must be conservative, logged, and visible in the audit trail as a fault plus
  the recovery that ended it — the shape `observe_fault` already uses.
- A restart loop is the obvious hazard: revival must be rate-limited and must
  give up loudly rather than restart Docker forever.
- The operator keeps the escape hatch. Host-level actions are configurable and
  can be turned off, for a host where something else owns Docker.
- **These actions ship to hosts nobody is watching.** Under
  [ADR-0006](0006-fleet-autoupdate.md) a follower already tracks the release
  channel and updates itself when a version is published. A host-affecting
  behaviour is therefore not a change to one Mac someone is sitting at — it
  reaches the fleet on the next release. That is the argument for keeping each
  action conservative and rate-limited, and for defaulting the disruptive half
  (daemon revival) to something an unattended host can survive.
- Disk that is *not* CI's (the operator's own data that filled the volume in
  this incident) is still the operator's problem. faber can keep its own
  footprint from being the cause and can survive the squeeze; it cannot make room
  that was never its to take.
