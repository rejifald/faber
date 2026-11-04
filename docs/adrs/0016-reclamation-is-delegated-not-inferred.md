---
title: Labels decide provenance; the operator decides recreatability
type: adr
status: accepted
date: 2026-09-03
owner: rejifald
---

# ADR-0016: Labels decide provenance, the operator decides recreatability

## Status

accepted (2026-09-03). Amends [ADR-0007](0007-faber-owns-the-docker-host.md)'s
boundaries — see *Relationship to ADR-0007* for the sentence that changes and the
one that does not.

## Context

ADR-0007 gave faber custody of the host and, with it, the duty to keep disk
pressure from wedging the daemon. The implementation reclaims two things:
dangling images and build cache. Both are regenerable, neither can belong to
anyone else, and the narrowness was the point.

On a host a month into CI, that reclamation was a no-op against the actual
garbage:

| | count | size | reclaimable |
|---|---|---|---|
| Images | several hundred | tens of GB | about three quarters |
| Volumes | about a hundred | more than the images | about two thirds |

Most of the images were per-run stack images from a couple of compose projects;
most of the volumes were per-checkout cache triplets, the bulk of them belonging
to checkouts that no longer existed. All **tagged** or **named**, so the prunes
could not see them; none faber's, so ADR-0007 forbade touching them.

The janitor therefore hit the floor **dozens of times in under a week and freed
nothing on nearly all of them**, walking free space down to a few GiB while many
times that in dead weight sat in plain sight, logging one line per pass, of this
shape:

```
janitor: nothing left to reclaim from CI's own images or build cache
         — <n> MiB free. The pressure is not CI's to relieve.
```

That sentence was false. The pressure *was* CI's; it merely was not
*faber-created*, and faber had conflated the two.

Two measurements shaped the fix:

* **Images were never the space.** Removing the hundreds of stack images freed
  only **a few GB** — images derived from one base share nearly every layer.
  Removing the orphaned cache volumes freed **tens of GB**, many times more. A fix
  covering images alone would have answered the complaint and missed the problem.
* **The host's other cleaner was dead.** A cleanup job the operator had installed
  for exactly these resources had been failing on every run for weeks
  (`exit 127`) because it pointed into a deleted git worktree. That is the
  argument for this living in a supervised daemon.

### The first draft of this ADR was wrong, and how

It proposed matching **resource names**: an operator list of prefixes checked
against image tags and volume names. That contradicted ADR-0007's own finding —
*"A convention is not a fact"* — and its stated direction, that labels are
authoritative and *"retiring the prefix matching is what they unlock."* It would
have added a new prefix-matching subsystem to a codebase that had decided to
remove one.

Worse, it was unnecessary. Everything faber needs to know about *provenance* is
already stamped on the resources by whatever created them:

```
volume  myproj-visual-<checkout>_npm-cache
  com.docker.compose.project = myproj-visual-<checkout>
  com.docker.compose.volume  = npm-cache          ← the role, project-independent

image   ghcr.io/owner/my-runner:2.1
  org.opencontainers.image.version                ← no compose labels at all
```

## Decision

**Docker's labels answer "what is this and who made it". The operator answers
the one question no label records: "can it be recreated".**

### Provenance is read, never inferred

* **Images.** Candidates come from `--filter label=com.docker.compose.project` —
  built by compose, therefore built by something on this host that knows how.
  An image that was *pulled*, or built by hand, carries no compose label and is
  structurally absent from the listing.
* **Volumes.** Candidates come from `--filter dangling=true --filter
  label=com.docker.compose.volume=<role>`. `dangling` means *no container,
  running or stopped, refers to this volume*, so a live stack's data volume
  cannot appear whatever is declared. The role match is **exact**.

Both filters are applied by the daemon, so faber never holds a list of things it
must remember not to touch.

### Recreatability cannot be inferred, and here is the proof

Three findings on one host, each killing an otherwise-attractive automatic rule:

1. **"Compose built it, so it can be rebuilt" is false.** A *runner base image*
   on that host was itself built by a compose project, from build files that no
   longer exist, and exists on no registry. Compose provenance alone would have
   offered up the one image AGENTS.md says must never be removed.
2. **"Its checkout is gone, so it is orphaned" is false.** One of the operator's
   other stacks — several healthy containers with real data — carried
   `com.docker.compose.project.working_dir` pointing at a **deleted worktree**.
   Path-liveness would have condemned a running database's volume the moment that
   stack was brought down.
3. **Nothing distinguishes a cache from the only copy.** `..._npm-cache` and
   `..._postgres-data` carry the same kind of compose labels, can both be
   unreferenced at the same moment, and both belonged to projects whose directory
   was already deleted. Every automatic signal reads them identically.

So the operator declares, in the one file no watched repo can reach:

```toml
[host]
recreatable_projects = ["myproj-e2e-"]   # com.docker.compose.project, prefix
recreatable_volumes  = ["npm-cache"]     # com.docker.compose.volume, exact
```

The two matchers differ because the namespaces do. A per-run project
(`myproj-e2e-<date>`) is an unbounded family, so a prefix is right — but it is a
prefix over *provenance*, not over whatever the image happened to be tagged. A
volume role is a fixed word chosen in the compose file, so an exact match is
right, and it is **project-independent**: one `npm-cache` entry covers every
checkout of every project, now and later, and cannot drift as project names do.

Empty — the default for both — is byte-for-byte the behaviour that shipped before
these keys existed. ADR-0006 means this reaches followers nobody is watching, and
a release must not widen what faber deletes on a host that never asked.

### What still outranks a declaration

* **The protected set** — every profile's runner base, its dind image, the probe
  image, the variant repo, matched on both `repo:tag` and `repo`. Finding (1)
  above is why this is load-bearing rather than defence in depth.
* **In use** — the running set for images; `dangling` for volumes.
* **Disk pressure** — the pass runs only below `disk_floor_gib`, after the
  regenerable half, behind the same admission pause and ramp settle every bulk
  deletion in ADR-0007 takes.

### A pass that frees nothing is a fault

The old warning fired dozens of times without being seen. It now goes through
`observe_fault`: collapsed across repeats, on the dashboard beside every other
fault, cleared as soon as a later pass frees something. When nothing is declared
it says so, because "faber cannot help here" and "faber is not configured to help
here" have different fixes.

### There is deliberately no age bound

ADR-0007 settled this for runner-image variants and the reasoning transfers:
under the floor, everything not in use goes, because a rebuild costs minutes and
a full disk costs the daemon. The cleanup job that inspired this used a TTL
because it ran unconditionally on a timer; faber's trigger *is* the pressure,
which also means faber ships no date parsing it would otherwise have to own.

## Relationship to ADR-0007

ADR-0007's boundary read: never an image or container belonging to a workload
faber did not provision, and never named volumes it did not create.

* **"faber did not create it" is no longer the test.** It was a proxy, and a bad
  one: it let faber watch a host die of garbage it could see but had no
  vocabulary to be told about.
* **"faber does not decide" is the test, and it is unchanged.** Nothing here is
  heuristic — no "looks like a cache", no size ranking, no age guessing. faber
  reads facts other tools stamped, and is *told* the one fact nobody stamps.

ADR-0007's blanket prohibitions stand as written: no `system prune -a --volumes`,
no `image prune -a`, no untargeted volume reap. And its own closing note —
*"labels are the authoritative signal going forward, and retiring the prefix
matching is what they unlock"* — is what this ADR finally acts on.

## Consequences

- An operator who declares nothing sees no change, here or on any follower.
- A declaration is a deletion capability. The blast radius is bounded, not zero:
  a declared cache volume removed under pressure costs the next run a cold cache.
  That is the trade, and it is the operator's to make.
- `recreatable_projects` is host-wide, so an unrelated compose project whose
  prefix matches is in scope. Deliberate — under the floor the alternative is a
  wedged daemon, which costs that project more — but it is why the pass is
  pressure-only rather than a cadence.
- Both keys are validated where a human is looking, hinted in the settings page,
  and every refusal is logged by name and reason.
- `tests/live_recreatable_reclaim.rs` pins the four Docker behaviours the safety
  rests on, because under this design the daemon's filters *are* most of the
  safety and an assumption about `--filter` that is wrong deletes the wrong thing.
