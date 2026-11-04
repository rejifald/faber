---
title: faber reserves its own address pool rather than reclaiming foreign networks
type: adr
status: accepted
date: 2026-09-23
owner: rejifald
---

# ADR-0020: faber reserves its own address pool

## Status

accepted (2026-09-23). Applies [ADR-0016](0016-reclamation-is-delegated-not-inferred.md)'s
rule to a resource it did not anticipate — address space rather than disk — and
reaches the opposite implementation for it. Nothing in ADR-0007 or ADR-0016
changes.

## Context

Every per-job sandbox gets a private bridge network. By default the daemon picks
its subnet from the *global* default address pools, which on a stock daemon are

| base | carved at | slots |
|---|---|---|
| `172.17.0.0/16`, `172.18.0.0/16`, `172.19.0.0/16` | `/16` | 3 |
| `172.20.0.0/14`, `172.24.0.0/14`, `172.28.0.0/14` | `/16` | 12 |
| `192.168.0.0/16` | `/20` | 16 |

**31 subnets for the entire machine** — shared by faber, by every compose stack,
and by every other tool that bridges a network. There is no reservation and no
priority: allocation is first come, first served, and a network holds its slot
until something removes it.

On the host this was written for, an unrelated tool left a container-less compose
network behind on each run. Some two dozen accumulated. Together with faber's own
live runners they held **nearly all of the 31 slots**, and provisioning failed:

```text
ERROR runner for <repo> job <id> failed: creating network faber-net-<id>-<ts>:
  all predefined address pools have been fully subnetted
```

**Over a thousand times across most of a month**, on consecutive jobs — every
affected job never got a runner.

The instructive part is what the investigation ruled out. The error names a
faber network, so a leak in faber was the obvious reading, and it was wrong on
every count:

* Every `faber-net-*` on the host had two containers attached. All were live.
* `teardown` is called unconditionally by `run_one` — `let result = …; teardown(…).await; result`
  — so it runs on the error path too, including when the network create is what
  failed.
* [`sweep`](../../src/sweep.rs) already reaps orphaned faber networks whose job
  is no longer in flight, and its logs showed `0 network(s)` because there were
  none to reap.

faber was not leaking. It was **one claimant among several on a pool none of
them owned**, and it lost the race because it needed a fresh subnet on every job
while the squatters needed theirs only once.

## Decision

**faber allocates its per-job subnets from a block the operator sets aside for
it**, instead of drawing on the shared defaults:

```toml
[host]
network_pool = "10.63.0.0/16"   # empty (the default) = Docker's own pools
network_subnet_size = 24        # /16 + /24 = 256 concurrent slots
```

With a pool declared, faber reads the subnets already bridged on the host, picks
one inside its own block that overlaps none of them, and passes it to
`docker network create --subnet`. Capacity stops being a race and becomes a
number the operator chose.

Absent the key, faber allocates exactly as every release before this one did.
That is the same contract as ADR-0016's grants, and it matters for the same
reason: this reaches unattended follower hosts through ADR-0006, and a host that
never asked must be unchanged by the release that added it.

## Why not reclaim the squatting networks

That was the first proposal, and it is the one ADR-0016 exists to refuse.

A container-less network *looks* disposable, and mostly is — compose recreates
one on the next `up`. But "mostly" is an inference about somebody else's
resource, and ADR-0016 settled that faber does not make those: a network with no
containers attached right now may belong to a stack between runs, and nothing on
the host distinguishes that from an abandoned one. The label says who created it,
never whether they are done with it.

The grant mechanism (`recreatable_projects`) could have been widened to cover
networks, and an operator who declared one would then get their slots back. That
remains available and consistent, but it is a worse fix for this problem:

* It only helps a host that declares, so the failure persists by default.
* It makes faber's ability to run jobs depend on deleting other tools' state —
  a coupling that grows every time another tool joins the machine.
* It treats the symptom. Even with the squatters gone, faber and everything else
  still share 31 slots, and the next tool to arrive re-creates the incident.

Reserving inverts all three. It works by default once declared, it deletes
nothing, and it makes faber's capacity independent of what else the host does.
**The cheapest way to win a race is to stop being in it.**

## Why not simply widen the daemon's pools

`default-address-pools` in `daemon.json` does raise the ceiling, and an operator
may still want it. It is not sufficient here:

* It is daemon-global, so the extra slots are *also* shared — it raises the
  number the squatters can consume, it does not reserve anything for faber.
* It needs a daemon restart to take effect, which is CI downtime faber is
  supposed to prevent, not cause.
* faber cannot set it at run time, so it cannot be part of a fix that ships.

It is documented as an operator option and is orthogonal to this decision.

## Consequences

* **The operator picks a range, and picks it badly at their peril.** A block that
  collides with something the host reaches over its LAN or a VPN will bridge
  fine and then quietly shadow that route — the symptom is a job that cannot
  reach a service, not an error at create time. Both the config comment and the
  settings-page hint say so.
* **Pool exhaustion becomes a faber-worded error.** An exhausted *declared* pool
  names the block, its slot count, and the two ways out (widen the pool, lower
  the runner cap) rather than handing the operator the daemon's sentence a
  third time.
* **Allocation races are handled, not prevented.** Reading the daemon's
  allocations is not atomic with creating, so two simultaneous ramps could pick
  the same subnet. Each scan starts at an offset derived from the network's own
  name, which keeps concurrent ramps apart in the common case, and a bounded
  retry re-reads and re-picks for the genuinely unlucky one. A *persistent*
  overlap is a config error that retrying cannot fix, so it is reported instead.
* **The declared pool is a concurrency ceiling.** `/16` at `/24` is 256 slots,
  far past any single host's runner cap, but a narrow pool could bind before the
  cap does. It is logged at startup with its slot count for exactly that reason.
* **Nothing about reclamation changes.** faber still deletes only what it built
  and what the operator declared recreatable.
