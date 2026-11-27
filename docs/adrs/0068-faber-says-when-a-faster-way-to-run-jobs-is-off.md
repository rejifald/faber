---
title: faber says when a faster way to run jobs is off
type: adr
status: proposed
date: 2026-11-27
owner: rejifald
---

# ADR-0068: faber says when a faster way to run jobs is off

## Status

proposed (2026-11-27). The owner decided the shape on 2026-11-27, with the first
hint drawn on the dashboard before any daemon code: a neutral row, on the
dashboard only, with no button, beside the notices that already exist.

It adds a row to the attention list that
[ADR-0064](0064-a-warning-that-does-not-stop-jobs-can-be-dismissed.md) lets an
operator dismiss, and decides how
[ADR-0048](0048-attention-rows-can-be-delivered-outward.md)'s outward delivery
treats it. It reads the step timings of
[ADR-0056](0056-a-run-records-where-its-time-and-resources-went.md) and changes
none of them.

## Context

faber has several ways to make a job faster that a host has to turn on: a git
mirror ([ADR-0035](0035-jobs-borrow-git-objects-from-a-mirror-only-faber-writes.md)),
a package mirror
([ADR-0047](0047-jobs-fetch-through-a-mirror-only-faber-fills.md)), a build cache
volume ([ADR-0005](0005-persistent-per-repo-build-cache-volume.md)). Each ships
off, as a release that reaches a host unattended must (AGENTS.md). So a host can
run for months with one of them off, and nothing tells its operator what that
costs.

The case that started this: on a builder host the git mirror was off because the
engine had once granted dind (docs/git-mirror.md, "Not beside dind"). faber did
say so, in a notice that explains why a mirror the config asks for is off. The
notice says nothing about what checkout costs on that host, whether the dind
grant holding the mirror off is still used, or what the way back takes. An
operator reading it learns that a rule applied, not that there is time to win.

Three facts shape what faber can say:

- **faber knows what a step costs.** Every `outcome` line carries GitHub's steps
  with their start and end (ADR-0056 §3), so the time a host's runs spend in
  checkout is on record.
- **faber does not know what a capability saves.** No `outcome` line says that a
  run borrowed from the git mirror, fetched through the package mirror or mounted
  a cache volume. A before-and-after figure cannot come from a host's own
  records, and a figure from another host's measurement is not this host's.
- **Most of these switches widen trust.** Turning on `git_mirror`, `mirror` or
  `cache_volume` is a trust widening
  ([ADR-0028](0028-the-control-api-knows-its-caller.md)), and each carries a named
  risk. A row that encourages a switch must not hide what the switch accepts.

## Decision

### 1. A hint is a neutral row that changes nothing

A **speed-up hint** is a row faber shows when a capability that would make jobs
faster exists on this host and is off or held off. It has the attention list's
neutral severity (`info`): it sorts after every red and amber row and never
changes the dashboard's own colour. It is not blocking, so under ADR-0064 it
carries Dismiss, and a dismissed hint stays dismissed while its figures move.

A hint changes no behaviour, stops no job and is never a reason for faber to
refuse anything.

### 2. A host with nothing to gain shows nothing

No hint shows when the capability is already in use, when the host has no profile
that could use it, or when no run in the window would have used it. A healthy
host whose config already asks for everything stays as quiet as it is today.

### 3. The figure is this host's own, and it is a cost, never a saving

A hint states what the step it is about costs on this host today, read from the
step timings of the last 7 days: how many runs, the median per run, and the sum.
It never states what the capability would save, because faber has no record to
say it from (§ Context). With no timed step on record it says so and states no
figure.

The daemon computes the figures and sends them as numbers; the dashboard words
the row, so durations follow the UI's own formats.

### 4. A hint names what the fix accepts, and has no button

Every fix a hint points at is a config change, and most are trust widenings that
need sudo. So a hint carries no action: it names the key, says in the row what
turning it on accepts, and names the doc that has the steps. An operator is never
one press away from a widening.

### 5. Hints are on the dashboard, not in the popover, and never delivered outward

The popover is the glance surface and a healthy popover is calm
([popover.md](../design/popover.md)); a hint is not something that needs the
operator now. It shows in the dashboard's attention list only. It is never a
candidate for ADR-0048's outward delivery, whatever severities a channel is
configured for: a notification or an issue for "this could be faster" would teach
an operator to ignore the channel.

### 6. One key turns them off

`[host] speed_hints` (default `true`) turns every hint off when `false`. On is
the default because a hint changes nothing faber does (§1): the rule that a
feature ships off protects a host from a behaviour it did not ask for, and a
neutral, dismissable row is not one. The key applies on restart.

### 7. The first hint: the git mirror

`hints` in `/api/state` carries one kind today, `git_mirror`:

- **When.** The host has a docker profile, a docker run finished in the window,
  and either no docker profile sets `git_mirror` (`off`), or the mirror is held
  off beside dind: a profile grants dind now (`dind_granted`), or none does but
  the engine is marked (`engine_marked`).
- **The cost.** The time runs spent in checkout steps: a step GitHub named from
  the action (`Run actions/checkout@…`), or one a workflow named beginning with
  the word "Checkout". A checkout step named anything else is not counted, so the
  figure can only be low.
- **Whether the grant is used.** Held off beside dind, the row says how many
  docker runs in the window were handed a Docker daemon, and when the last one
  was. A grant nothing has used for weeks is then visible as what holds the
  mirror off. The count compares the `outcome` line's `dind` field with
  `granted`: the field is `granted` or `denied`, and absent only for a run with
  no container.
- **The way back.** `off`: `git_mirror = true`, a trust widening that puts
  faber's token on the engine for each refresh. Held: `dind = false` on every
  docker profile and a fresh Linux VM disk, an operator step with risks of its
  own (docs/git-mirror.md, "Turning them back on"). The row says which.

What is off and why is decided at boot, from the config as written: every key it
reads applies on restart. The figures are read from the audit log in the
background, at most every 10 minutes, and served from memory, so `/api/state`
never waits on the file. A host with nothing to gain reads nothing.

The existing `mirrors_beside_dind` notice stays as it is. It says why a mirror
the config asks for is off; the hint says what that costs and what the way back
takes, and shows whether or not the config asks.

## Consequences

- An operator sees, on the dashboard, time a host is leaving unused, with a
  figure from that host's own runs.
- A hint can understate. It counts only checkout steps it can recognise by name,
  and it cannot see whether a workflow carries the step that borrows from a
  mirror (docs/git-mirror.md). It never overstates a saving, because it states
  none.
- Every new hint is a review question: does faber detect it reliably, is the
  figure this host's, and does the row name what the fix accepts?
- The audit log is read in full for the figures, like the Jobs view (ADR-0056).
  The read is in the background and no more often than every 10 minutes.

## Candidates not shipped

- **The package mirror off** (`[mirror]`, a profile's `mirror`). Detectable, and
  the install steps are timed. Held until the risks of turning it on are named
  where it is turned on: withdrawn versions are still served, and the redirect
  outranks a project's own registry. A hint must not get ahead of that text.
- **`cache_volume` off.** Not shipped: faber cannot see whether a workflow would
  use the cache, holds no figure for what it would restore, and it is the
  riskiest of these widenings (a pull request job can plant what a later push job
  with secrets restores).
- **Jobs that take dind while the host declares services**
  ([ADR-0046](0046-jobs-declare-services-and-faber-runs-them-without-dind.md)).
  faber cannot tell whether a job used its Docker daemon or what it started in
  it, so it cannot say the job could run without one. What it can count, runs
  handed a daemon, is part of the git mirror hint (§7).
- **`autolimit.reserve = "p75"`, auto-retry, the poll interval.** faber knows the
  time jobs waited, not how much of it the switch would cut; and a bigger VM is
  never a suggestion (AGENTS.md).

## Alternatives considered

- **Add the figure to the existing notice.** One row instead of two. Rejected:
  the notice is decided once at boot and is amber by a security decision, the
  figure is live and neutral, and the hint also has to show on a host whose config
  does not ask for the mirror, where there is no notice.
- **An amber row.** Rejected: it would keep a healthy host amber for as long as
  its config stands, and an attention list that is always amber stops working as
  an alarm (ADR-0064's own reason).
- **State the saving from faber's published measurements.** Rejected: the
  measurements are another host's, on other repos. A figure that is not this
  host's is a guess shown as a fact.
- **Record on each run whether it used a mirror**, so faber could state a measured
  before-and-after once a capability is on. Not decided here; it would be an
  additive field on the `outcome` line and its own change.
- **A button that turns the capability on.** Rejected by §4.
