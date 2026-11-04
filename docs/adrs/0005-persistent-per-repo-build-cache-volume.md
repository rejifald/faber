# ADR-0005 — A persistent per-repo build cache volume

**Status:** accepted; amended 2026-10-29 by [ADR-0055](0055-a-cache-volume-is-bounded-by-size-and-age.md) (faber bounds each cache volume by size and age)
**Supersedes:** nothing
**Related:** ADR-0002 (in-flight claims), `src/docker.rs`, `src/sweep.rs`, `src/gate.rs`

## Context

faber's Docker backend gives every job a fresh container filesystem: a private
network, its own dind, its own runner container, all force-removed at teardown.
That isolation is the point, and nothing about it should change.

But it also means a workflow has nowhere on local disk to keep anything between
jobs. A CI job that needs a large dependency tree — `node_modules`, a Cargo
registry, a Gradle cache — has to fetch it from a remote cache service on every
single run. For a watched repo that came to hundreds of megabytes restored per
job, paid by nearly every job in the graph, on a runner pool that is already the
bottleneck.

A watched repo had in fact written a local-disk fast path for exactly this,
pointed at a directory under `/runner/_work`, on the assumption that the runner
replicas shared one persistent `_work` volume. That was true of the always-on
runner pool it used before faber. Under faber it is not: no volume is
mounted into the runner container at all. The failure was silent — `/runner/_work`
exists *inside* the container because the runner image creates it, so the "is
the volume here?" probe passed, found no cache, and fell through to the remote
path on every job for months without a single line of log saying so.

That silence is as much the problem as the missing cache. Any fix has to make
the absence of a cache loud.

## Decision

Add an **opt-in, per-repo, persistent Docker named volume**, mounted into the
runner container only:

- **Off by default.** `cache_volume = true` on a `[[profile]]` turns it on. This
  is the only state faber lets cross the job boundary, so an operator opts into
  it deliberately rather than inheriting it from an upgrade.
- **Mounted at `/runner/_cache`** — a dedicated path, explicitly *not* `_work`.
  The job workspace (checkout, `_temp`, `_tool`, `_actions`) stays exactly as
  ephemeral as it is today. A `docker.rs` unit test asserts the mount never
  lands on `_work`.
- **Named `faber-cache-<owner>-<repo>`.** faber can service many repos from one
  host; one shared volume would let any of them read and overwrite another's
  cache. Per repo, the isolation matches what GitHub's own cache service gives
  across repos.
- **`FABER_CACHE_DIR=/runner/_cache` is exported into the container**, and only
  when a volume was actually mounted. The variable is the contract, not the
  path: a workflow keys off `-n "$FABER_CACHE_DIR"`, so the "no cache here" case
  is a distinguishable state it can report instead of a probe that passes for
  the wrong reason. faber pushes it after the profile's own `env`, so its value
  always describes the real mount.
- **Best-effort.** If `docker volume create` fails, faber logs a warning and runs
  the job without the mount. A cache is an optimisation; failing to provide one
  must make a job slower, never make it fail.
- **Never reaped.** Teardown removes only the per-job dind volume. The janitor's
  reap list is anchored to the per-job resource kinds (`runner-`/`dind-`/`net-`/
  `vol-`); `cache-` is not one, so the existing orphan filter shields it. A
  `sweep.rs` test pins that — including the contrived case where a repo slug
  reintroduces the `vol-` token — because the volume is dangling between jobs
  and would otherwise be reaped on the very next pass, with no symptom beyond a
  cache that is mysteriously never warm.

## Consequences

**Within a repo the cache is shared across refs, with no branch isolation.** A
job on any branch can read and overwrite what a job on any other branch wrote.
This is not incidental — it is what makes the cache useful (a PR job must be able
to read what a main job populated) — but it is a genuine reduction in isolation
and worth naming plainly.

It is defensible here because `src/gate.rs` already refuses, in code and
fail-closed, the two ways untrusted content could reach a faber runner: **public
repos** are never serviced at all, and **fork-PR provenance** is refused (a PR
whose head repo differs from its base, or whose head repo cannot be resolved).
So everything that can write to this volume is code from a non-fork ref of a
private repo — the same trust boundary that already lets faber execute that code
on the host in the first place. If either of those gates is ever relaxed, this
decision has to be revisited before it is: a shared writable cache would become
a cross-branch code-execution path.

**Disk grows with the cache.** faber creates the volume and never prunes its
contents — it has no idea what is inside. Bounding it is the consuming workflow's
job (keep the newest N entries, drop anything idle past a deadline). An operator
can reclaim it wholesale at any time with
`docker volume rm faber-cache-<owner>-<repo>`; the next job recreates it empty.

**Concurrent writers are the workflow's problem too.** Several jobs from the same
repo can hold the mount at once. faber provides a shared directory and nothing
more; a consumer that writes to it needs its own atomicity (write to a temp file
on the same filesystem, then `mv`).

**The first job has to take ownership.** Docker creates a mount point absent from
the image as `root:root 0755`, and a runner image that executes jobs as an
unprivileged user therefore cannot write to a brand-new cache volume. Verified
against a real runner image: uid 1001 gets `Permission denied` on the first job.
faber cannot fix this centrally — it does not know which uid a given image runs
jobs as — so the consumer chowns it once, either in the image's entrypoint while
it is still root or from the job itself where the image grants sudo. The new
ownership is stored in the volume, so every subsequent job finds it correct.

## Alternatives considered

**Mount `_work` itself.** It is what that earlier pool did and it would have
required no consumer change at all. Rejected: it makes the checkout and the
runner's internal state shared mutable memory across every job on the host,
which is precisely the contamination the per-job sandbox exists to prevent.

**Leave it out; delete the consumer's dead fast path.** Honest, zero risk, and it
was the other serious option. Rejected because the cost is real and recurring —
a full remote-cache round-trip on every job of a saturated pool — and the fix is
a bounded, opt-in mount rather than a structural change.

**Bake the cache into the runner image.** Works only for content that changes at
image-build cadence. A dependency tree keyed to a lockfile changes far more
often than the image, so most jobs would miss.
