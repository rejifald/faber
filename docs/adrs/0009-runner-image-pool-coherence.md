---
title: One runner-image generation at a time, per repo
type: adr
status: accepted
date: 2026-08-28
owner: rejifald
---

# ADR-0009: One runner-image generation at a time, per repo

## Status

accepted (2026-08-28). Amends
[ADR-0008](0008-on-demand-runner-image-variants.md), whose Consequences already
named this as the open problem — "closing that properly means pinning runners to
jobs, which is a different decision than this one". This is that decision, and
the answer it reaches is *not* the one that sentence anticipated: runners cannot
be pinned to jobs at all, so the pool is made uniform instead.

Invalidates one sentence of [ADR-0004](0004-demand-based-provisioning.md) —
"naive parallel operation is **safe but wasteful**". It was true when it was
written and is not true any more; see below for why.

## Context

### The conflict

ADR-0008 made the runner image **a function of the job**: `resolve_runner_image`
reads the repo's pinned `@playwright/test` at that job's content ref, resolves a
per-version variant, and binds it to the container it starts.

ADR-0004 built demand provisioning on a property that was true at the time:

> The residual double-mint … is bounded and idle-reclaimed as before;
> **correctness never depends on it** (GitHub stays the single job broker).

Both statements are individually correct. Together they are not, because faber
registers its JIT runners through `generate_jitconfig` carrying **only the
profile's labels**, and *GitHub matches queued jobs to runners by label — never
by name, never by job id*. The `faber-{job_id}-{ts}` name is faber's
bookkeeping; it is not an affinity GitHub honours or has ever offered.

While every runner was interchangeable, a runner taking a sibling's job was
merely untidy accounting — ADR-0002 and the `ExecutedJob` attribution map exist
precisely because it happens routinely. Once the image varies per job, the same
swap stops being untidy and becomes wrong: **a job can land on a container built
for a different job's pin**, and with it every other per-job decision — the
resolved image, the memory reservation, the outcome accounting — is applied to
whichever job happened to arrive.

### The evidence

A watched repo, August 2026, on a re-run of one workflow run. Two jobs of that run
swapped runners outright:

| Job | Ran on the runner minted for |
|---|---|
| job A (a visual-test job) | `faber-<job B>-…` → **job B** |
| job B (a Playwright end-to-end job) | `faber-<job A>-…` → **job A** |

Read by comparing GitHub's `runner_name` (`gh api repos/{repo}/actions/jobs/{id}`)
against faber's own `outcome.runner` in the audit log. faber never saw the job it
had provisioned for: both outcome records showed a `mem_peak_runner_mib` of a few
dozen MiB, an idle runner's footprint, and faber's own `cause: "reclaimed mid-run:
the runner sat idle for a job that no longer needs one"` — the runner was doing
work faber had already written off. All runners carried identical labels,
`faber,docker`.

Four hypotheses were eliminated before this one: a stale base image (the label,
`.playwright-version` and all three browser directories read the newly pinned
version), a volume shadowing `/opt/ms-playwright` (`CACHE_MOUNT` is
`/runner/_cache`), faber pulling over the local retag (it does not pull), and a
second faber instance (only one process held the log handles).

### What is actually possible

The obvious fix — pin the runner to the job — **does not exist**.
`POST /actions/runners/generate-jitconfig` accepts a name, a runner group, labels
and a work folder; none of them scope a registration to a job id. Extra labels
cannot partition the pool either, since matching is a subset test on what the
*job* asks for and the job's `runs-on` belongs to the repo, not to faber.

So faber cannot decide *which* job reaches a given runner. What it can decide is
**which images it puts into the pool**, and that is the whole lever this ADR
uses.

## Decision

**A repo's live faber runners run one image generation at a time, and faber only
ever adds the maximum.**

Concretely, the images live for one repo are kept totally ordered by the set of
toolchain versions each one *bakes*, and a new runner may only join at the top of
that order. A pool satisfying this has the property the swap needs: every runner
can serve every job any *other* live runner could have taken, so GitHub handing a
job to the "wrong" runner stops being a failure mode.

### 1. An image is judged by what it serves, not by its tag

Two tags are not interchangeable because they are equal; they are interchangeable
because one bakes everything the other does. `DriftOutcome::serves` states that
for each resolution:

- a **healed** job's variant serves two versions, its base's and its own — the
  browser GC is off in faber's recipe, which is what makes this true and is now
  load-bearing for a second reason;
- an **in-sync** job's base serves the one version it bakes;
- an **unhealed drift** or a **failed heal** runs on the base, so it serves the
  baked version and pointedly *not* the pinned one;
- everything else — no opt-in label, unreadable lockfile, drift off — serves the
  **empty set, which means unknown, never "nothing"**.

### 2. Three cases, decided under one lock

`pool_decision` weighs a freshly-resolved image against the images this repo's
other live claims are already cleared to run:

- **The candidate covers everything live ⇒ mint it.** Widening is always safe.
  This is the empty-pool case and the bump-PR case at once: a variant bakes the
  base's version too, so it dominates the base it derives from.
- **Something live covers the candidate, and everything else live ⇒ mint on
  *that* image instead.** This is the case a naive "images differ ⇒ wait" rule
  gets catastrophically wrong. With a variant hot, every ordinary `main` job
  resolves back to the base; making each of them wait for a drain, then making
  the next PR job wait for *those* to drain, serialises the repo's entire CI
  behind alternating drains. Substituting costs nothing — the live image already
  bakes what this job pins — and it makes the pool *converge* on the dominating
  image rather than oscillate.
- **Neither dominates ⇒ hold.** Genuinely incomparable images (a third Playwright
  version in flight) cannot both be live without one of them being wrong for
  somebody, so faber runs one generation at a time and lets the other drain.

The decision and the publish of the chosen image happen **under one held
`inflight` lock**. That is not incidental: two siblings resolving concurrently
would otherwise both read an empty pool and both mint, recreating the very split
the rule exists to prevent. Serialised through the map lock, the second sees the
first's image.

### 3. Holding is parking, and it is bounded

A hold happens with the claim **still parked** — no container, no memory
reservation — exactly where ADR-0008 already parks a claim across a variant
build, and a reclaim issued during one is honoured the same way.

Waiting there rather than releasing the claim and retrying next cycle is
deliberate: the job stays visible on the dashboard, no GitHub reads are spent
re-resolving a pin that cannot have changed, and the provision throttle keeps
measuring what it is for — a runner that never consumed its job — instead of
counting faber's own deliberate waits as failures and backing the job off to the
five-minute self-heal cadence.

Past `POOL_HOLD_MAX` (15 min) **faber mints anyway**, and says so. The hold
exists to keep the pool homogeneous; it must never become a way to starve a
queued job. Minting into a split pool risks a job landing on the wrong image,
which is where this repo already was before this ADR; a job held forever never
runs at all, which is worse than anything the rule prevents.

### 4. Every unknown fails open

A hold requires faber to **positively know** the candidate's versions *and* at
least one live runner's, and to have found them incomparable. An unlabelled
image, an unreadable lockfile, a native claim, a peer instance's runner, or
ADR-0008 switched off all constrain nothing. This is the same asymmetry
`compute_supply` is built on — under-minting starves CI, a surplus container does
not — and it is why "unknown" is spelled as an empty served set that callers must
read as *no constraint*.

With `[runner_image] build = "off"` the image is a static property of the profile
again, the pool is homogeneous by construction, and none of this code runs.

## Consequences

- **The common case is untouched.** Every queued job sharing a pin resolves to
  one image, every decision is "mint as resolved", and no claim ever holds.
- **The flagship ADR-0008 case gets *better*, not slower.** `main` on the base
  version with one open bump PR converges on the variant: the PR's first job
  mints it (widening), and every subsequent `main` job joins it rather than
  reintroducing the narrower base. Both PRs stay green and the pool stays
  uniform, with no drain in between.
- **A third version in flight costs latency, and buys determinism.** (Removed for
  adopting repos by [ADR-0010](0010-jobs-declare-their-runner-needs.md): disjoint
  label pools make it "both green" rather than "one waits".) With base
  1.60, one PR on 1.61 and another on 1.62, no single image serves everything, so
  one generation waits. That converts "both PRs fail intermittently, depending on
  which runner GitHub picked" into "one PR is green, the other waits" — which is
  legible, and which the operator can act on.
- **The residual is real and is stated here rather than implied.** faber cannot
  prevent a wrong assignment; it can only stop *manufacturing* the conditions for
  one. A job pinning a version no live image bakes can still be dispatched onto a
  live runner and fail — deferring faber's own mint does not un-queue it. What
  this ADR removes is the case faber creates itself: two of its own images live
  for one repo at one time. The remaining exposure is bounded by how many
  distinct versions are queued at once, which is the same bound ADR-0008's `keep`
  is sized for.
- **Enforcement is per instance.** **Closed by
  [ADR-0010](0010-jobs-declare-their-runner-needs.md)**, which lets GitHub's own
  label matcher do the partitioning, so which host minted a runner stops
  mattering — and the peer-advertisement mechanism this bullet ends by
  contemplating is not needed for it. Under demand provisioning two hosts share one
  repo pool (ADR-0004), and this host cannot see what image another host started
  a container from. The rule holds within an instance and fails open across
  instances. That is a real narrowing of ADR-0004's "safe but wasteful": a peer's
  surplus runner is no longer merely wasteful once the image varies per job. A
  cross-instance fix needs the peers to advertise their resolved images — the
  same mechanism ADR-0004 already contemplates for parked claims — and is not
  built here.
- **The pool is keyed on the repo, not the profile.** Profiles can carry
  overlapping label sets, so treating one repo's faber runners as one pool is the
  conservative direction. It costs nothing in practice: a claim only participates
  once it has published a *known* image, and a native claim never resolves one at
  all.
- `PLAYWRIGHT_SKIP_BROWSER_GC=1` now carries a third justification. ADR-0008
  adopted it for disk, then for surviving a not-job-pinned swap; it is also the
  only reason a variant *dominates* its base, and therefore the only reason the
  common two-version case can be served by one image instead of by a drain.

## Alternatives considered

- **Resolve per repo/ref instead of per job.** Simplest: pick one image for the
  whole repo and stop varying it. Rejected because it gives up the
  two-PRs-on-different-pins case ADR-0008 exists for — and, once the superset
  property is taken seriously, it turns out not to be necessary: the dominating
  image already covers both PRs, so uniformity and healing are not actually in
  tension.
- **Verify at container start.** Have the runner check its baked version against
  the job it was handed and fail loudly. Cheapest, and it prevents nothing — it
  converts a confusing failure into an explicit one. It also cannot be built
  where it belongs: the base image is operator-owned and built off-repo, so faber
  would be asking for a change it does not control. Worth revisiting as a
  *complement* once the base grows a faber-owned entrypoint; a post-hoc version
  of it is already within reach, since faber harvests `job.runner_name` and knows
  which job actually ran on each of its runners.
- **Refuse to mint whenever the resolved image differs at all.** The literal
  reading of "drain the pool on image change". Rejected on inspection: it
  serialises a busy repo behind alternating drains for the *most common*
  heterogeneous case (a `main` job while a variant is hot), which is under-minting
  — the one direction faber must not fail in. Substituting onto the dominating
  image handles that case at zero cost and leaves the hold for the genuinely
  incomparable one.
- **One image carrying every version in flight.** Automates the hand-built bridge
  image and keeps the pool trivially uniform. Rejected for the reasons ADR-0008
  already gave — every job carries every version, growth is unbounded, and a
  rebuild is needed per addition — with one added: the union changes whenever a PR
  opens, so the rebuild lands on the provisioning path rather than once per
  version.
- **Pin the runner to its job.** The fix that would end the problem outright.
  Not available: no GitHub API scopes a JIT registration to a job id, and labels
  cannot partition a pool whose jobs' `runs-on` faber does not author.

  **Taken up in [ADR-0010](0010-jobs-declare-their-runner-needs.md)**, on the half
  of that sentence this one treated as closed: faber does not author `runs-on`, but
  the *job* does, and a job that states its needs lets GitHub's matcher perform the
  binding faber cannot. The cost is the per-repo CI wiring ADR-0001's first force
  exists to avoid, which is why it is a separate, opt-in decision rather than a
  correction to this one.
