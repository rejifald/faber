---
title: A restart adopts its in-flight work instead of draining
type: adr
status: accepted
date: 2026-08-28
owner: rejifald
---

# ADR-0011: A restart adopts its in-flight work instead of draining

## Status

accepted (2026-08-28) — decided this session, not yet implemented.

Supersedes one section of [ADR-0006](0006-fleet-autoupdate.md), *"When: wait for
natural idle, escalate to a drain after 6h"*. That mechanism is not wrong; it is
the best answer available while a restart destroys in-flight state. This ADR
removes that constraint, which makes waiting for idle unnecessary rather than
merely slow.

Prompted by an operator request for **blue/green deployment** — two faber
versions running side by side on one host, retiring the old one once the new one
is healthy. That request is declined, and the reasoning is recorded under
*Alternatives considered*, because the request identifies a real cost and only
the proposed remedy is wrong.

## Context

### What a restart costs today

faber's in-flight state is a single in-memory map — `inflight: job.id → Claim`
(`src/reconcile.rs`). It has no persistence and no reconstruction path. Nothing
at startup looks for work that is already running; `live_runner_containers`
(`src/reconcile.rs`) reads exactly the containers that would be needed for it
and is used only to decide whether an *already-known* claim is still live.

Because the map cannot survive the process, a restart has only two honest
options, and `decide` (`src/update.rs`) implements both:

- **Wait for idle.** `staged && inflight == 0 → Commit`. On a host that is
  actually being used, this window may never arrive.
- **Escalate to a drain.** After `escalate_after_secs` — **6 hours** by default
  (`src/config.rs`) — `do_escalate` calls `begin_drain()` then `await_drain()`,
  which refuses every new claim until the last admitted runner finishes. A
  runner may live up to the 3h `runner_timeout` cap.

So the worst case from "a release exists" to "this host runs it" is roughly
**nine hours**, of which up to three are spent provisioning nothing at all. The
same cost is paid by the manual path: `deploy/install.sh` and
`deploy/uninstall.sh` both go through `launchctl bootout`, whose own comment
states the consequence — *"bootout sends SIGTERM, so faber drains in-flight
runners"*.

### What the drain is actually protecting

Less than it appears — but *only* after one thing is fixed first.

The intuition is that faber is not in the job's data path: the JIT runner
registers with GitHub directly, so killing faber should leave the container
running to finish its job. **That was false**, and the first live restart under
this ADR proved it — a few adopted runners died with exit 143 within seconds of
an otherwise perfect handoff, and their jobs went back to `queued`.

The cause is that faber launched the runner as an *attached* `docker run`, and
`docker run` defaults to `--sig-proxy=true`. launchd signals the whole process
group when the job exits, the client received SIGTERM, and it dutifully forwarded
it into the container. faber *was* in its own containers' lifetime, through a
flag nobody had had reason to think about while every restart drained first.

The fix is one flag, and it makes the rest of this ADR true rather than merely
plausible: **the runner container is started detached**, and the only way faber
learns its exit code is `docker wait`. Detached, the container belongs to the
Docker daemon alone, nothing about faber's lifetime reaches it, and the watcher
becomes something any process can be — which is what lets an adopted container
and a freshly-started one be the *same* object rather than two similar ones. The
container still runs without `--rm`, so its corpse survives for the OOM inspect.

With that in place the original claim holds: **exiting without draining does not
fail a CI job.**

What is genuinely lost is three things:

1. **Teardown.** The container, network, volume and dind leak. The janitor
   already reclaims exactly this class, including the running dind that never
   exits on its own (`running_dind_orphans`, `src/sweep.rs`).
2. **The outcome record and the memory sample.** Observability and learning, not
   correctness.
3. **The lifetime cap.** This is the one that matters. The 3h `runner_timeout`
   and the idle-reclaim are enforced *in this process*, not by Docker. An
   unwatched runner has no ceiling at all — which is the idle-runner-leak class
   faber has already had to fix once.

The drain, in other words, protects **lifecycle enforcement and observability**.
It is not standing between CI and a broken build. That is what makes replacing
it a strict improvement rather than a trade.

### The enabling observation

Everything a claim needs in order to be rebuilt is already on the host, written
durably, by faber itself.

[ADR-0007](0007-faber-owns-the-docker-host.md) established that every resource
faber creates is marked: `org.faber.owned=true`, `org.faber.kind`,
`org.faber.repo`, `org.faber.id=<job_id>-<mint_ts>` (`marks`, in
`src/docker.rs`). Those labels exist so a stray resource found at 2am is
self-explaining. They are equally an answer to *"what was this process doing
when it died?"*

The watcher is not privileged, either. Any process can `docker wait` a runner
container for its exit code, `docker inspect` its corpse for `.State.OOMKilled`,
and re-attach the memory sampler — `stats_mem_mib` (`src/docker.rs`) is a plain
`docker stats --no-stream <name>` addressed by name.

This is the same move [ADR-0004](0004-demand-based-provisioning.md) made when it
declined to add a coordination service: *GitHub's runner registry is itself the
shared state.* The analogue here is one step closer to home.

> **faber's in-flight truth is held in memory when it is already durably written
> on the Docker objects themselves.**

## Decision

**A restarting faber adopts its in-flight work. It does not drain, and it does
not wait for idle.**

### 1. The container is the claim

`marks` is extended so a runner container carries everything static about the
claim that minted it:

| Label | Purpose |
|---|---|
| `org.faber.owned=true` | unchanged — ownership |
| `org.faber.kind` | unchanged — `runner` / `dind` / `net` / `vol` / `cache` / `image` |
| `org.faber.repo` | unchanged — `owner/repo` |
| `org.faber.id` | unchanged — `<job_id>-<mint_ts>` |
| `org.faber.run-id` | the workflow run, for the retry sweep and the dashboard |
| `org.faber.provision-attempt` | faber's provision-attempt counter, so an adopted outcome record reads like any other |
| `org.faber.profile` | which profile serves this claim — selects the backend on rebuild |
| `org.faber.class` | the autolimiter class key this run belongs to |
| `org.faber.reserved-mib` | what the limiter charged for this claim |
| `org.faber.serves` | the toolchain versions this container's image serves (ADR-0009) |

`org.faber.reserved-mib` is **load-bearing and not redundant**. It cannot be
recovered by inspecting the container, because the reservation and the applied
ceiling are deliberately different numbers: a claim is charged `own_runner +
own_dind` (`src/limiter.rs`) while the container is *started* with the cohort
peer-max limits (`src/limiter.rs`). Reading `--memory` back off the container
would over-charge the rebuilt ledger and collapse concurrency.

`org.faber.serves` is the other one that had to be there. Without it an adopted
claim rejoins the ADR-0009 pool as an *unknown* image, and `pool_decision`
filters unknowns straight back out — the runner would be live and invisible,
which is the state ADR-0009 exists to forbid. Its value is legitimately empty for
an image faber knows nothing about, so empty and absent must stay
distinguishable: Docker renders a missing label as `<no value>` and an empty one
as nothing, and the parser leans on exactly that.

Labels are immutable at `docker run`, which bounds what may live here: only
static identity. The three mutable facts are all recoverable elsewhere — the
resolved image is readable off the container itself, `executed_job_id` comes
from GitHub, and `step` is re-polled live by the reconciler.

Everything else in `Claim` is runtime-only (`cancel`, `admitted`) or derivable.
`started_ms` comes from the container's `StartedAt`; `job_name` and `workflow`
fall out of the class key (`repo::workflow_id::job_name`) and the label the
limiter has already learned for that class. Only `pr`, `branch` and `event` are
genuinely lost, and they are purely presentational — worth strictly less than the
property their absence buys, which is that **adoption asks GitHub nothing at
all**, and therefore runs before the network is known to work.

### 2. Preflight, then fast-exit

`decide`'s `inflight == 0` requirement is replaced by an **adoptability
preflight**:

```
adoptable ⟺ docker_reachable
          ∧ every inflight claim is containerized
          ∧ every live faber-runner-* container carries the ADR-0011 labels
```

- **Adoptable ⇒ commit immediately.** `do_commit` runs its existing sequence
  (re-check config, snapshot for rollback, arm the boot marker, atomic swap) and
  `exit(0)`s. launchd's `KeepAlive` restarts on the new binary, which adopts.
- **Not adoptable ⇒ today's path, unchanged.** `begin_drain()` + `await_drain()`
  + commit. This is the escape hatch for a wedged Docker, a native claim, and
  the one-time label migration.

`escalate_after_secs` is **kept and re-purposed**: it is no longer the deadline
for "give up on idle", but the deadline for the *fallback drain* — how long a
non-adoptable host may keep waiting before it stands down the hard way.

### 3. SIGTERM means "I'm coming back"

`bootout` (decommission) and `kickstart -k` (restart) are the same bare SIGTERM;
the process cannot tell them apart. Rather than guess, the meaning is declared:
**SIGTERM is a restart.** The shutdown arm of `main.rs`'s `tokio::select!` runs
the same preflight-then-fast-exit path the updater uses.

Decommissioning becomes explicit, because it *is* explicit — a human running an
uninstall script:

- new `POST /api/drain` alongside the existing `/api/pause` and `/api/resume`
  (`src/web.rs`);
- new `faber drain` subcommand that calls it and waits;
- `deploy/uninstall.sh` calls `faber drain` **before** `launchctl bootout`.

The residual hazard is named rather than engineered away: a bare `bootout` by
someone who skips the drain step leaves running containers with no lifetime cap
and no one to tear them down. They still complete their jobs; the next faber to
start on this host adopts or sweeps them.

### 4. One faber at a time, enforced by a lock

There is no lock today. A second `faber run` fails to bind `127.0.0.1:8787`, but
that error is only logged (`src/main.rs`) and the reconcile loop runs on
regardless. Today the damage is double-minting — untidy and self-healing under
ADR-0004. Under adoption it becomes **double-watch**: two processes tearing down
one container, emitting two outcome records, and double-releasing one
reservation.

It is `flock(2)`, declared directly rather than pulled in with a `libc`
dependency — two constants and one call, in a binary that is self-contained by
design. macOS's `O_EXLOCK` open flag would do the same job in one syscall, but
those flag bits mean nothing on Linux: the open succeeds and silently locks
nothing. faber ships for macOS and runs its tests on Linux, so that version of
the lock had a test that could only ever pass by accident — and did not.

An advisory `flock` on `~/Library/Application Support/faber/faber.lock` is taken
**before adoption** and held for process lifetime. The kernel releases it on
exit or crash, so there is no stale-lock class to handle. Without the lock,
faber refuses to provision.

The lock is about **provisioning authority**, deliberately not about the
dashboard: the right to mint runners and the ability to serve a UI are
unrelated, and a `--port` override must not be able to defeat the former.

### 5. Nothing is admitted until adoption succeeds

Adoption runs before the first claim of the process's life, and until it
completes **no job is admitted**.

This is [ADR-0007](0007-faber-owns-the-docker-host.md)'s Gate 0 applied to the
ledger rather than to liveness, and for the same reason: that ADR exists because
faber admitted about ten jobs into a dead Docker daemon, stranding them for more
than three hours. A faber that cannot enumerate what is already running does not
know what memory is already reserved, and admitting on a ledger that reads zero
while real containers hold real memory is precisely the over-admission that ends
in an OOM.

On a healthy host this is one `docker ps` and costs milliseconds.

### 6. Every faber-owned running container is charged, adopted or not

The ledger's invariant is deliberately **wider** than the claim map's. A
container faber owns but cannot rebuild into a claim — minted before these
labels existed, running under a profile this config no longer defines, a second
container left behind by a re-provision — is still holding memory. A ledger that
counts only what it could adopt would admit new work on top of it, which is the
same over-admission this ADR exists to prevent, arriving by a different door.

So an un-rebuildable *running* container is charged its applied ceiling (the one
number it can answer with, and pessimistic on the safe side) and the charge is
held until the container goes. It gets no claim, no outcome record and no
teardown: there is nothing to report on, and the janitor already reaps by
ownership.

This is what makes the migration window safe rather than merely short — during it
*every* in-flight container is un-adoptable, and a ledger reading zero against a
full host is exactly the state that ends in an OOM. It is not migration-specific:
the same three cases recur long after the labels are universal.

### 7. An adopted job may widen a cap, never narrow one

Memory peaks are high-water marks, so a sample that starts at adoption
**under-reports** — and teaching the learner a smaller cap surfaces later as an
OOM in an unrelated run, with nothing pointing back at the restart that caused
it.

The two directions are separable, because `.State.OOMKilled` survives on the
corpse while the peak does not:

- an adopted run **may** force-bump its class cap upward on an OOM kill;
- an adopted run **never** contributes a peak sample.

The existing convention already encodes the discipline — an `outcome` record
carries `class` only when the sample was actually fed to the live learner, which
is what keeps startup replay consistent with live learning. Adopted runs simply
join lifetime-cap kills and idle reclaims in that category.

Adoption also **re-seeds the `OomLedger`** (`src/retry.rs`) from the same
inspection — state that is silently lost on every restart today.

### 8. Native claims are out of scope, and that is free

`Backend::Native` (`src/backend.rs`) runs an `osx-arm64` runner as a bare host
process. It creates no Docker object and carries no labels, so it has no
substrate to be reconstructed from. `Claim.containerized` already makes the
distinction first-class.

A native claim in flight therefore fails preflight and falls back to the drain.
The measured cost of that on this host is zero: across tens of thousands of audit
lines, **every one of the few thousand profile records is `faber-docker`**. The
`macos-native` profile is configured and has never served a job.

Building a second adoption substrate for a backend with no traffic is not
justified. If native ever carries load, this is the ADR to amend.

## Implementation plan

Five phases. Each is independently shippable and leaves the tree working; the
behaviour change lands only in phase 3.

### Phase 1 — label the containers (no behaviour change)

- `docker.rs`: add `LABEL_RUN_ID`, `LABEL_ATTEMPT`, `LABEL_PROFILE`,
  `LABEL_CLASS`, `LABEL_RESERVED`, `LABEL_SERVES` next to the existing `LABEL_*`
  consts, plus a `ClaimMarks` struct carrying them. `runner_marks` composes them
  onto `marks`, so only the runner container widens — dind, the network and the
  volume are reaped by ownership and never adopted.
- `reconcile.rs`: `spawn_provision` already holds every value at mint time —
  `run_id` and the provision `attempt` from `ProvisionIntent`, `profile_idx`, and the
  admission's `reservation_mib` and class label from the limiter. Thread them to
  the backend call.
- Tests: extend the existing `marks`/argv unit tests, which already assert the
  exact `--label` pairs.

Ships alone. From here the host accumulates fully-described containers, which is
what phase 3's preflight will require.

### Phase 2 — adoption, behind the gate

- New `reconcile::adopt()`:
  1. `docker ps -a --filter label=org.faber.owned=true --filter
     label=org.faber.kind=runner` — running and exited alike.
  2. For each, `docker inspect` → labels, `StartedAt`, `State.ExitCode`,
     `State.OOMKilled`.
  3. **Running** ⇒ rebuild a `Claim` with `admitted = true`, re-charge
     `org.faber.reserved-mib` to the limiter ledger, publish the resolved image
     into the ADR-0009 pool snapshot, spawn a watcher that `docker wait`s
     instead of owning a `docker run`, and re-attach the sampler.
  4. **Exited** ⇒ *completed unwatched*: emit the outcome record from the corpse
     (exit code, `OOMKilled`), feed the OOM signal only, then tear down. This is
     strictly better than today, where such a container is swept silently.
  5. The lifetime cap re-derives its deadline from `StartedAt`, **not** from
     adoption time, or a restart would grant every in-flight job a fresh 3h.
- `docker.rs`: start the runner **detached** (`-d`), and factor the
  watch/teardown tail of `run_in_sandbox` so both callers reach it by name. With
  the runner detached there is no second path to support — `run_in_sandbox` and
  `adopt_one` issue the same `docker wait`. This is the only non-trivial refactor
  in the plan, and the detach is the part that makes adoption work at all.
- `main.rs`: take the flock, then run `adopt()`, **before** spawning the
  janitor. The ordering is load-bearing: the startup sweep would
  otherwise reap the very corpses adoption turns into outcome records. Hold
  admission shut until `adopt()` returns (§5).

### Phase 3 — preflight and fast-exit (the behaviour change)

- `update.rs`: `decide` takes an `adoptable: bool` beside `inflight`; a staged
  update with `adoptable` returns `Commit` regardless of `inflight`.
  `do_escalate` is now reached only when `!adoptable`.
- `main.rs`: the shutdown arm of the `tokio::select!` runs the same preflight —
  fast-exit when adoptable, `begin_drain()`/`await_drain()` when not.
- Keep the second-signal force-exit as-is.

### Phase 4 — the probation fix

`rollback_if_needed` rolls back at `attempts >= 2` (`src/update.rs`),
incrementing once per boot, and the marker is only cleared after
`probation_secs` — **120 seconds** by default (`src/config.rs`). Three boots
inside two minutes therefore trigger a rollback.

Today that is a fair proxy for a crash-loop. Once restarts are deliberate and
cheap, it is not: an operator iterating with `bootout`/`install`/`bootstrap`
right after an autoupdate would roll the update back and quarantine a perfectly
good version.

Fix, the standard crash-only distinction: every deliberate exit path (fast-exit,
`do_commit`, `faber drain`) writes a one-shot `update/clean-exit` breadcrumb on
the way out. `rollback_if_needed` consumes it at boot and, when present and
fresh, **does not increment `attempts`**. A crash writes no breadcrumb, so
genuine crash-loops count exactly as they do now.

### Phase 5 — the decommission path

- `POST /api/drain` in `web.rs`; `faber drain` subcommand in `main.rs`.
- `deploy/uninstall.sh` calls `faber drain` before `launchctl bootout`.
- `deploy/install.sh` needs no change — its `bootout` is a restart, which is now
  the fast path.

### Migration

The first deploy of the adopting binary meets in-flight containers minted by the
old one, which lack the phase-1 labels. Preflight sees them, reports
non-adoptable, and drains **once**. The window is self-clearing and needs no
migration code.

## Consequences

- **Restart latency goes from up to ~9h to roughly one second.** The bounded
  cost is launchd's restart gap, during which nothing is claimed and the sampler
  misses a poll or two — a documented-acceptable miss, since peaks are
  high-water marks.
- **The crash-orphan class is retired, not merely reduced.** Because the
  substrate is Docker rather than a handoff file, adoption covers SIGKILL,
  panic, power loss and a Docker restart — not just planned updates. There is no
  second, rarely-run recovery path to rot.
- **ADR-0009 pool coherence survives a restart.** Adoption republishes each
  claim's resolved image into the pool snapshot. Today that is lost, so the
  first post-restart mint weighs itself against an empty pool.
- **The recovery path is exercised on every restart**, which is the strongest
  argument for it. A path that only runs during incidents is a path that is
  broken during incidents.
- **`concurrency` and the ledger are correct across a restart** for the first
  time. Today a restart forgets every reservation while the containers holding
  that memory keep running.
- **A new invariant to hold: exactly one faber may provision per host.** The
  lock makes it explicit and enforced, where before it was an accident of port
  binding that did not actually stop anything.
- **Six new labels are a permanent interface.** Anything a future claim needs at
  adoption time must either be static enough to be a label, or recoverable from
  GitHub. That is a real constraint on future `Claim` fields, accepted
  knowingly.
- **A bare `bootout` now leaks.** Skipping `faber drain` leaves capped-less
  runners behind. Jobs still complete; the next faber adopts or sweeps.

## Alternatives considered

### Blue/green — two faber versions side by side

The request that prompted this ADR. Rejected, for a reason that is structural
rather than about effort:

**faber has no traffic switch.** GitHub is the sole job broker, and both
instances' runners would carry identical labels — matching is a subset test on
the job's `runs-on`, which belongs to the repo, not to faber (established at
length in [ADR-0009](0009-runner-image-pool-coherence.md)). So "cut over once
blue is healthy" cannot be implemented as a cutover. It degrades to "blue starts
claiming, green stops claiming and exits when its last job ends" — an
overlapping drain. It does not make green leave sooner; it only stops the drain
window from costing throughput.

For that, the price is cross-process arbitration on six host-level singletons:

1. **The memory budget.** Fatal on its own. Each instance computes `MemTotal −
   reserve` off the same Docker VM and keeps its own reservation ledger
   (`src/limiter.rs`), so two instances admit ~2× the budget. Admission control
   is the entire reason this host does not OOM.
2. **The janitor**, which reaps `faber-*` orphans against *its own* in-flight
   set.
3. **Runner-image eviction**, where `org.faber.kind=image` does not say which
   faber built a variant.
4. **Disk-pressure reclamation**, which pauses only its own admission before
   pruning — the comment in `src/sweep.rs` records that a bulk cleanup against a
   live pool already cost three jobs once.
5. **ADR-0009 pool coherence**, which that ADR states plainly *"across instances
   it still relies on peers advertising what they resolved, which is not
   built."*
6. **Dashboard port, config, audit log and the update directory.**

And it buys nothing the operator asked for. The stated pain was restart latency;
the only goal that genuinely *requires* two live processes is behavioural
canarying, which was explicitly not wanted — and which faber partly covers
already through ADR-0006's boot probation, `faber.prev` restore and quarantine.

Adoption is not an alternative to blue/green so much as its prerequisite: a
Docker-derived ledger is exactly what would make two coexisting faberes safe. If
canarying ever becomes a goal, this ADR is the groundwork, and the singleton
lock of §4 is the piece to revisit.

### An on-disk claim journal

Write `inflight` to disk on every transition; validate against Docker at
startup. Richer reconstruction — `wait_ms`, the exact learned class, the
reservation with no new labels.

Rejected because it is a **second source of truth that can disagree with
Docker**, and when it does, Docker wins — so the journal earns nothing it costs.
It also adds a write on every claim transition, and its recovery path only runs
on planned restarts unless stale-journal handling is built too. The label
approach has one source of truth and is exercised on every boot.

### Docker labels plus a GitHub re-fetch

Keep today's two labels and re-fetch the rest per adopted job at startup. No new
labels, no migration window.

Rejected because it makes **adoption depend on GitHub being reachable at exactly
the moment faber is restarting**, and it spends N job GETs on every boot. The
whole point is that the answer is already on the host.

### Make the drain cheap instead

Shorter lifetime caps, redeploy-aware scheduling windows. Cheapest by far, but
the 3h `runner_timeout` ceiling means the worst case stays in hours, and
shrinking it to help deployment would be tuning CI's ceiling for an operator's
convenience.

## Validation against real circumstances

- **A 6h escalation that never had to happen.** With ADR-0006 enabled and jobs
  in flight, an update today waits 6h and then drains. The check is direct:
  stage a release with work in flight and confirm the log goes staged →
  preflight → commit → exit, with no drain line and no admission pause beyond
  the restart.
- **The long job.** The busiest watched repo's longest jobs run near 120
  minutes. A restart mid-job must show the job completing normally on GitHub,
  one outcome record (not two), no `class` on that record, and teardown
  happening at the *adopted* deadline derived from `StartedAt`.
- **A wedged Docker.** The failure of ADR-0007 replayed: with Docker
  unresponsive, preflight must report non-adoptable and take the drain path, and
  a booting faber must admit nothing until `adopt()` succeeds.
- **Double-watch.** Run `faber run --port 8788` alongside the LaunchAgent. It
  must fail on the lock and provision nothing — where today it would run a full
  second reconcile loop.
- **Probation.** Autoupdate, then `kickstart -k` three times inside
  `probation_secs`. The version must survive: `attempts` unchanged, no rollback,
  no quarantine entry.
