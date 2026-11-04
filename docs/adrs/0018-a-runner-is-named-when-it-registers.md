---
title: A runner is named when it registers
type: adr
status: accepted
date: 2026-09-17
owner: rejifald
---

# ADR-0018: A runner is named when it registers

## Status

accepted (2026-09-17). Amends [ADR-0004](0004-demand-based-provisioning.md): the age
bound on connecting supply now counts from registration. Amends
[ADR-0017](0017-a-reclaim-asks-the-runner.md): a claim's runner name is published
as the claim is admitted, not when its provision task starts.
**Amended 2026-10-11** — the name ends in the minting instance's id; see
*"Amendment (2026-10-11)"* at the foot of this file.

## Context

faber names every JIT runner `faber-{job_id}-{ts}`. GitHub's runner list carries no
registration time, so that timestamp is the only age any instance has for a
runner, its own or a peer's. Three windows read it:

- **Connecting supply** (`CONNECTING_GRACE`, 120 s). Demand provisioning counts an
  `offline` runner as supply for its still-queued job only while it is younger than
  the grace. The bound exists because a dead registration once masked its own job
  forever.
- **The stale sweep** (`STALE_RUNNER_TTL`, 600 s). Every cycle, every instance
  deregisters `offline`, non-busy `faber-*` runners older than the TTL, a peer's
  included.
- **The busy deferral** (`BUSY_RUNNER_GRACE`, 6 h). A stale runner GitHub still
  reports busy is left alone until then.

All three assume the timestamp is close to registration. It was not. The provision
task read it on its first line, and a claim can then wait three times before its
runner registers: image resolution (ADR-0008), the pool hold (ADR-0009, up to
15 min), and memory admission, which has no bound under pressure. Admission already
sat between the name and the registration when the windows were added. The other two
waits came later.

On one host, over the first half of September 2026:

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| claim parked before admission (about 5,000 outcomes) | about a second | about a minute | about 20 min | about an hour |
| admission to the runner container launching (about 5,000 provisions) | a few seconds | about 10 s | about 20 s | about a minute |

Hundreds of claims parked longer than the connecting grace and over a hundred
longer than the stale TTL. The ramp after admission is short. The park before it
is what aged runners.

**Past the stale TTL, a sweep deleted the registration.** Such a runner was "stale"
from the moment it registered, and it stays `offline` until its listener connects.
A sweep in that window deleted it. The container then started, found no
registration, and exited without running anything. In one case that month a claim
parked for about 13 minutes was admitted, and a sweep reaped a stale runner about
a second later. Its container launched seconds after that and exited within ten
seconds with no job recorded against it. Across the period:

| claim parked | claims | ended < 90 s after admission, no job | with a reaping sweep during the run |
|---|---|---|---|
| ≤ 2 min | about 5,000 | about 700 (about 15%) | a handful |
| > 10 min | about 160 | about 80 (about half) | about three quarters of those |

Those provisions had already waited more than a day between them before admission.

**Past the connecting grace, demand over-minted.** A runner whose claim had parked
more than two minutes read as dead, not connecting, the moment it registered.
Demand provisioning, on this host or a peer, stopped counting it as supply and
minted one runner more than the queue needed. This effect was not measured
separately.

## Decision

**A runner is named when its claim is admitted, not when its provision task
starts.** `admit_claim` (`src/reconcile.rs`) reads the timestamp, publishes the name
into the claim, and flips `admitted`, in that order. Both admission points (Docker
and native) call it. Nothing waits between it and the registration request. The
Docker path reads the sizing cohort and then calls `generate-jitconfig` as the first
step of its ramp.

The timestamp is therefore the registration instant, give or take one request. All
three windows now measure what they say, on this host and on every peer reading the
same list. No window constant changes.

Everything that read the name before registration was checked:

- **The claim's `runner_name` cell** (ADR-0017). The reconciler reads it only for an
  admitted claim. It is still published before the flag flips, so every admitted
  claim has one. A parked claim now has none, and nothing reads it there.
- **Container, network and volume names** (`Names::for_runner`) and the
  `org.faber.id` label ADR-0011 adopts from. They derive from the name, but nothing
  is created before the JIT mint, so they only carry the later timestamp.
- **Attribution and the outcome record** (`executed_class`, `record_outcome`). Both
  read the name after the run. `wait_ms`, `duration_ms` and the OOM ledger's window
  still count from the task's start, so that window still contains the job's.
- **Uniqueness.** The timestamp keeps names unique per attempt, since a reused name
  gets a `409`. On one host, a later attempt for the same job is admitted only
  after this claim is released, so its timestamp is still later.
- **The janitor sweep and the retry join** match on the job id or the exact name,
  never on the timestamp.

The native backend does some work between the mint and registration: a check for
a cached runner binary and a copy-on-write clone of the runner directory. The first
job on a new runner version downloads the binary there, so that one runner
registers older by the download. That is a one-off per version, not a park.

### Why the name has to carry it

GitHub's runner list has no registration time, and every instance runs the sweep
from that list alone. A registration time kept in this host's memory, or on its
container, would fix how this host ages its own runners and nothing else. A peer
would still read the old timestamp and delete the runner. The name is the one field
every instance sees.

It also makes rollout safe in both directions. Old and new peers age a runner named
by an upgraded host correctly. A runner named by a host still on an older release
is aged no worse than before.

## Consequences

- A runner whose claim parked counts as connecting supply for the whole grace
  after it registers, and is not swept until the TTL has passed since then. The
  deleted registrations and over-mints above stop.
- Every bound still closes. A registration that never connects stops counting as
  supply 120 s after it registered and is reaped 600 s after. A dead runner still
  cannot mask its job, which is ADR-0004's fail-open direction. A timestamp is
  never later than its registration request, so it cannot extend either window
  past registration.
- 120 s from registration covers the ramp that follows it, p99 about 20 s and at
  most about a minute to the container launching on the host measured.
- The busy deferral can still end early, but only by the ramp and GitHub's dispatch
  delay, not by the whole park (up to about an hour observed). Ending early is
  harmless: GitHub refuses (`422`) to delete a runner with a job assigned.
- This reaches other hosts through a release (ADR-0006).

## Alternatives considered

- **Keep the name and track registration time locally.** It fixes only this host's
  view of its own runners, as above. Rejected.
- **Carry the registration time in a runner label.** Peers would see it, but labels
  are what GitHub matches jobs on, and a per-runner value would sit in every label
  set faber and GitHub compare. The name already carries a timestamp. The fix is to
  take it at the right moment. Rejected.
- **Widen the windows past the longest park.** Parks have no bound under memory
  pressure (about an hour observed). A wider grace is also exactly what the bound
  was added to prevent: a dead registration masking its job for longer. Rejected.
- **Mint inside each backend, next to `generate-jitconfig`.** Tightest by
  construction, but both backends would have to hand the name back for attribution,
  the outcome, and the claim cell ADR-0017 needs before `admitted` flips. Admission
  is the last place either path parks, so minting there gives the same timestamp
  without that plumbing.

## Validation

Unit-tested (`src/reconcile.rs`):

- `a_parked_claims_runner_is_aged_from_registration_not_from_its_claim` drives the
  real provision task. A claim parks in memory admission, is admitted, and registers
  against a local stand-in for the API. The test uses the name it registered to pin
  each window at its edge, counted from admission: still supply at the connecting
  grace, unswept at the stale TTL, deferred at the busy grace. It also checks that
  the timestamp falls between admission and the request, and that the claim names
  that runner by the time it is admitted. With the name minted on the task's first
  line again, each of the three window assertions fails on its own.
- `a_795_second_park_no_longer_ages_its_runner_past_both_windows` replays the
  incident's numbers under both origins. It also shows both bounds closing on
  schedule for a runner that never connects.

Live criteria, after deployment:

1. Claims that park over 10 minutes stop ending as a quick exit with no job: from
   about half toward the roughly 15% of short parks.
2. Among those claims, a reaping sweep during the run no longer goes with a quick
   exit (about three quarters of them before).
3. Dead registrations are still reaped: `swept N/M stale offline runner(s)` lines
   continue at their previous rate.

---

## Amendment (2026-10-11): the name says which instance minted it

Issue #94. Two faber instances can serve one repository with the same labels.
When one of them lost every busy runner at once, the other saw the failures and
its sweep deferring runners it had never minted, and nothing in a name said whose
they were.

A runner is now named `faber-{job_id}-{ts}-{inst}` and registered with a
`faber-host-{inst}` label. `{inst}` is the operator's `[host] instance` when set,
otherwise `h` and seven hex digits of a SHA-256 of the hostname: stable per
machine, and never the hostname itself, which every runner listing would publish.
It is a lowercase letter then lowercase letters or digits, so it can never be
mistaken for a timestamp and sits inside Docker's container-name alphabet.

**The id goes at the end, not after `faber-`.** Every parser — the job id in
`faber_job_id`, the registration time in `faber_mint_ts` that this ADR made
meaningful, the janitor's name fallback, the freeze ordering — reads the first two
fields. A suffix keeps them where they were, so a peer still on an older release
reads a new name exactly as it reads its own, and this release reads an old one.
The claim id stamped as `org.faber.id`, and every container name built from it,
carry the suffix too, because adoption (ADR-0011) rebuilds the registration name
from them.

**Rolling back is safe, but not free.** That holds for runner names read from
GitHub, not for this host's own containers. A binary from before this release
cannot parse a suffixed claim id, and it reads its container listings all or
nothing (#105). So while any new-style container is still alive on a host rolled
back to it, that binary rejects its whole running-container listing and its whole
critical-floor job listing: it does no janitor pass over running runners, and the
critical floor neither freezes nor thaws any job, until those containers exit.
That is the safe direction — it admits and deletes less — but those passes stay
off for the containers' remaining lifetime.

**Peers.** Each runner listing faber already reads for demand and the sweep is
also folded into a peer view: a faber-shaped name with a different instance id is
a peer, warned about once per peer and shown on the dashboard, and a peer runner
that is `offline` with a job still assigned is written to the audit log once as
`peer_runner_lost`, with the job id. A name from before this release carries no
instance and is not attributed to anyone — it may be this host's own — so a peer
becomes visible once it runs this release.

**Declined (owner, 2026-10-12): a switch to refuse sharing a pool** (#94's item 4).
Each host works as if no other runners exist. Instance ids and the peer view only
label and report: they never change what a host claims, admits or runs. A host
that refused while a peer served the same labels would couple the two, and two
hosts that both refused would stall each other.
