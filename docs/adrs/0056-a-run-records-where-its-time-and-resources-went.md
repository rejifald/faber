---
title: A run records where its time and resources went
type: adr
status: proposed
date: 2026-10-30
owner: rejifald
---

# ADR-0056: A run records where its time and resources went

## Status

proposed (2026-10-30). Amends decision 1 of
[ADR-0054](0054-the-dashboard-is-views-in-a-sidebar.md): the dashboard gains a seventh
view, Jobs. Extends the `outcome` audit line of [ADR-0019](0019-cpu-is-a-deadline-resource.md)
and [ADR-0033](0033-a-reservation-is-the-typical-peak-the-cap-stays-the-worst.md).

## Context

faber already removes most of the network from a job: services without dind (ADR-0046),
the git object store (ADR-0035), the npm and LFS proxy (ADR-0047), the per-repo cache
volume (ADR-0005), and runner-image variants built ahead of admission (ADR-0008). The
next candidates all cost real work and pull in different directions:

- shorter pickup (adaptive polling, a started container waiting for its JIT config);
- caches that need no workflow change (Actions' tool and action downloads, more proxy
  ecosystems, a BuildKit layer cache);
- faster services (a Postgres template with migrations applied);
- admission ordering, if jobs mostly wait for capacity rather than run slowly.

Which one pays depends on facts nobody has. For a slow job we cannot say how much of its
time was setup and how much was the job's own work, whether it waited for capacity, or
whether its own CPU quota slowed it. A started container waiting for its config holds a
memory reservation while idle (ADR-0021), so it is the wrong fix if capacity is the
bottleneck, and the right one if pickup is. Picking without data means a guess.

Part of the data exists. The `outcome` line already carries:

- `duration_ms` (claim to finish), `wait_ms`, `wait_gate` and `wait_gates`: how long the
  claim was parked and at which gate;
- memory peaks per container, OOM flags and `memory.events`;
- CPU peak, average and throttled ratio per container, and the runner's quota, from the
  run's sampler (`src/docker.rs`, `CpuUsage`).

What's missing is the time before the claim, the stretch between admission and the job's
first step, the job's own steps, I/O, and CPU time lost to throttling. Nothing puts it in
front of anyone, either.

## Decision

### 1. One record per run, on the line that already exists

Everything below goes on the run's `outcome` line in `audit.jsonl`, keyed by the
limiter's job class (`class_key`: repo, workflow id, job name). The audit log stays the
source of truth, and a restart replays it as it does for the learner. There is no new
store, no new file and no new identity to join on.

Every new field is optional and omitted when unknown. "Not measured" and "zero" are never
the same value, here or in the view.

### 2. The phases faber can see

A run is cut into contiguous phases. Each boundary is a timestamp, and each phase is the
gap between two of them:

| Phase | From → to | Clock |
|---|---|---|
| `pickup` | GitHub queued the job → faber's poll first saw it | GitHub → faber |
| `prepare` | seen → claim parked (image resolved, variant built if needed) | faber |
| `parked` | parked → admitted (already `wait_ms`, with its gates) | faber |
| `sandbox` | admitted → runner container started (network, services, dind, mirror attach) | faber |
| `register` | container started → GitHub dispatched the job (`started_at`) | faber → GitHub |
| `job` | `started_at` → `completed_at`, split by step (§3) | GitHub |
| `teardown` | GitHub completed → faber finished cleanup (`outcome` `ts_ms`) | GitHub → faber |

This needs `created_at` on `github::Job` (GitHub returns it; faber does not read it yet)
and three faber timestamps it does not record yet: first seen, container started and
image resolved. On a macOS VM, `sandbox` is the clone and boot.

Two of the boundaries cross clocks, the host's and GitHub's. A phase that crosses clocks
and comes out negative is recorded as zero and flagged `skewed`, and the view says so. A
host whose clock is that wrong has a problem bigger than this view.

### 3. Steps come from GitHub, and they are classified without guessing

`github::Step` gains `started_at` and `completed_at`. faber already polls the single-job
endpoint for each job in flight, to show the live step. When that poll sees the job
`completed`, its step list is the one recorded. Only a job whose completion no poll saw
costs one more `GET .../jobs/{id}`. That call is deferred, and it is the first thing
dropped when the rate-limit budget is low. Then the line records no steps, not an empty
list.

Each step lands in one bucket:

- **setup:** GitHub's own `Set up job` and `Initialize containers`; an action from a
  fixed list (`actions/checkout`, `actions/setup-*`, `actions/cache`); and a run step
  GitHub named from its command, when that command is on a fixed list of installs
  (`npm ci`, `pnpm install`, `yarn install`, `pip install`, `uv sync`, `cargo fetch`,
  `go mod download`, `bundle install`);
- **teardown:** `Post …`, `Stop containers` and `Complete job`;
- **job steps:** everything else.

The third bucket is called "job steps", not "work". An install hidden inside a step named
`test` lands there, and the view does not pretend otherwise. The lists live in one table
in one module, with a test per entry, and grow with what the view shows.

Step names are recorded as GitHub reports them. They are workflow text from watched
repos, so they stay on the host, like the job names the audit log already holds, and
leave it only through §6's exporter.

### 4. Resources: what the sampler already reads, and no new sampling

The run's sampler already reads each container on a cadence. This ADR adds nothing to
that cadence. It reads more from each reading it already takes:

- **CPU time lost to throttling:** `throttled_usec` from `cpu.stat`, beside the ratio the
  line already has. The ratio counts periods, while this counts the time a job lost to
  its own quota.
- **CPU reserved:** the share the ledger debited for the run (ADR-0019, ADR-0033), so
  used against reserved can be read from one line.
- **Block and network bytes** per container, from the readings `docker stats` already
  returns: BlockIO from the cgroup and NetIO from the container's interface. The counters
  only grow, so the last reading is the value.

A container's cgroup dies with it, so the last reading is the last sample, not the
container's final value. Bytes and averages therefore cover the sampled part of the run,
like the CPU average today, and the view labels them as lower bounds.

macOS VMs have no cgroups. A macOS run records phases and steps only, and the view shows
its resources as "not measured".

### 5. A Jobs view

The dashboard gains **Jobs** (`/jobs`), between Activity and Memory: one row per job
class, over the runs in a window the view chooses (7 days by default, at most 90). Each
row answers three questions:

- **Where the time went:** median and p90 of each phase in §2, with `job` split into
  setup, job steps and teardown.
- **Waiting or running:** the share of wall time spent parked, and the gate that bound
  most often.
- **Starved or idle:** CPU used against reserved, time lost to throttling, and the bytes
  each run moved.

Below 20 runs in the window, the limiter's own threshold, a class shows its numbers
labelled "profiling" and is left out of the sort. Two runs must not rank the levers.

A run's own phase bar goes in Activity's row detail, not in Jobs. Per ADR-0054 decision
4, each section lives in one view. Overview gets no new tile until a number on it would
need a decision.

The view is UI, so it gets a design review like any other.

### 6. Export is opt-in, and adds no trust

`[telemetry] otlp_endpoint` turns on an OpenTelemetry exporter. Absent means off. The
key is config-file only: the settings form never offers it, so pointing run data at a new
host takes the same access as editing faber's config, never a dashboard session.

Each finished run becomes one trace: a root span named `job`, a child per phase, and a
child per step named `step`, with the step's name as an attribute so span names stay
low-cardinality. The attributes are the same fields as the `outcome` line.

The exporter reads the audit line after it is written and never sits on the run's path. A
collector that is down or slow drops spans, never a job. Its crates come in with this
section, behind a cargo feature, and go through dependency review like any other.

## Consequences

- Each `outcome` line grows by the phase timestamps, I/O counters and a step array. A run
  with 40 steps adds about 4 KB. The audit log is unbounded today, and this makes it grow
  faster. Bounding it is a separate decision, and the Jobs view reads only its window.
- `register` exposes the cost of the JIT runner starting. That is the number that decides
  whether a started container waiting for its config is worth its idle reservation.
- The `pickup` median measures the 20 s poll directly, so adaptive polling can be judged
  by data instead of argued.
- The sidebar has seven views. ADR-0054 decision 1 is amended, and its other decisions
  stand.
- A step list that names private build steps leaves the host only to a collector the
  operator configured.

## Considered and not adopted

- **A separate metrics store (SQLite, a time-series file).** Rejected: a second source
  of truth that the replay would have to reconcile with the audit log, for data that fits
  on a line that already exists.
- **Sampling more often to get final byte counts and time series.** Rejected: the owner
  asked for measurement that costs nothing visible, and lower bounds answer the ranking
  question just as well.
- **Asking workflows to emit timing marks.** Precise, but every repo has to change, and
  the data covers only what authors bother to mark. faber's phases and GitHub's steps
  need nothing from a repo.
- **A guest agent reporting resources from macOS VMs.** It widens what a guest sends the
  host. That's a trust-boundary change worth its own ADR, if macOS resource data turns
  out to matter.
- **Folding the breakdown into Memory and renaming it Capacity, or into Repositories.**
  Memory would get too wide, and its name would stop describing the budget rail. The
  Repositories view has no endpoint or design yet, so this would wait on it.
- **Classifying run steps by guessing from free text.** Rejected: a wrong guess hides the
  setup the view exists to expose. A fixed list, plus an honest "job steps" bucket, can be
  checked.
