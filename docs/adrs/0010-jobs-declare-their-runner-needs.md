---
title: Jobs declare their runner needs; faber provisions to match
type: adr
status: accepted
date: 2026-08-28
owner: rejifald
---

# ADR-0010: Jobs declare their runner needs; faber provisions to match

## Status

accepted (2026-08-28). Decision recorded ahead of implementation, following the
precedent of [ADR-0007](0007-faber-owns-the-docker-host.md): the shape of the
contract is what binds, and it should be settled before the first patch rather
than discovered by it.

Amends [ADR-0009](0009-runner-image-pool-coherence.md), whose three accepted
residuals this closes, and takes up the sentence it left open — "labels cannot
partition a pool whose jobs' `runs-on` faber does not author". They can, if the
job authors them.

## Context

[ADR-0008](0008-on-demand-runner-image-variants.md) made the runner image a
function of the job. [ADR-0009](0009-runner-image-pool-coherence.md) then found
that faber cannot bind a runner to a job — no GitHub API scopes a JIT
registration to a job id — and made the *pool* uniform instead: hold a repo's
live runners to one image generation, substitute onto the dominating image, wait
when two are incomparable, mint anyway past 15 minutes.

That works, and it accepted three residuals in writing:

1. faber cannot prevent a wrong assignment, only stop manufacturing one.
2. Enforcement is **per instance** — two hosts share a repo's pool and neither
   can see what image the other started a container from.
3. A third version in flight costs latency: one generation waits.

All three have the same root. **GitHub decides which runner serves a job, and it
decides by labels.** Every mechanism so far has approached this by trying to make
every runner acceptable to every job — a superset image, a dominating generation,
a hold. The inverse has been available the whole time and unused: let the *job*
state what it needs, and GitHub's own matcher does the binding faber cannot.

The reason it stayed unused is that `runs-on` is authored by the watched repo, and
[ADR-0001](0001-runner-image-dependency-drift-detection.md)'s first force says the
intelligence must live in the shared component, because anything requiring per-repo
CI wiring will not be adopted uniformly. That force has held for nine ADRs. This
one breaks it deliberately, because the thing being expressed — *which runner may
serve this job* — is structurally only expressible where GitHub reads it.

## Decision

### 1. The job declares its needs, computed from the repo's own lockfile

A setup job resolves the needs and emits a label set; the jobs that care consume
it:

```yaml
jobs:
  faber-setup:
    runs-on: [faber, docker]
    outputs:
      labels: ${{ steps.resolve.outputs.labels }}
    steps:
      - uses: actions/checkout@v4
      - id: resolve
        # Reads the resolved @playwright/test from package-lock.json and emits
        # ["faber","docker","faber.playwright-1.62.1"]
        run: echo "labels=$(.github/faber-needs.sh)" >> "$GITHUB_OUTPUT"

  e2e:
    needs: faber-setup
    runs-on: ${{ fromJSON(needs.faber-setup.outputs.labels) }}
```

**Dynamic, not literal, and that is the whole point.** A literal
`faber.playwright-1.62.1` written into `runs-on` would put the version in the
lockfile *and* in every workflow file that needs it. Bumping the pin then means
editing both, and missing one produces exactly the mismatch ADR-0001 exists to
detect — reintroduced in the one place faber cannot read as authoritative.
Computing the label from the lockfile keeps the lockfile the single source of
truth it already is.

### 2. The namespace is `faber.<tool>` and `faber.<tool>-<version>`

The `faber.` prefix keeps these from colliding with an operator's own labels.
Two forms, and the difference is deliberate:

- **`faber.playwright`** — capability. "This job needs Playwright, at whatever
  version my lockfile pins." Partitions the pool by *tool*, which is what a
  lighter-image-for-lighter-jobs split needs, and costs no setup job when written
  literally.
- **`faber.playwright-1.62.1`** — pinned capability. Partitions by *version*, which
  is what per-job image correctness needs, and is what the setup job emits.

Both are needed. GitHub's matching is subset-based — a runner carrying extra
labels still matches a job requesting fewer — so a versionless label cannot
partition by version, and a version label is overkill for a job that only needs
the tool to exist.

### 3. A runner may only carry a need label its image can honour

> **Corrected during implementation (2026-08-28).** This section originally read
> *"the label is intent; the lockfile is authority"*, justified by a threat that
> does not exist. It claimed a PR could name `faber.playwright-99.0.0` and cause
> `npx playwright@99.0.0 install` to run on the host daemon. It cannot: image
> resolution reads the **lockfile**, never the label, so a label has no path to
> commissioning a build. The rule below replaces it, the original text is kept
> after it for the record, and §4 is unaffected — it was always the part that
> bounds what may be built.

faber validates a declared need against **what the resolved image actually
serves** — `PoolImage::serves`, after the pool decision has had its say, so it
describes the image the container will really start from. A need the image cannot
honour is refused and no runner is minted for it.

What a PR-authored label *can* do is get a runner registered under a claim that is
false about its own image: a pool where the label lies, which is precisely the
incoherence ADR-0009 exists to prevent — and worse than it, because GitHub would
now be routing on the false claim. Checking against the served set closes that,
costs no extra API read, and reuses the set-domination vocabulary ADR-0009 already
built.

Unknown fails **closed** here (an empty served set refuses), the opposite of
ADR-0009 §4's fail-open pool rule. The asymmetry is deliberate: an
under-constrained pool starves a job, while a runner labelled with a promise
nothing verified gets handed work it cannot do.

<details><summary>Original §3, superseded</summary>

### The label is intent; the lockfile is authority

faber does not build what the label asks for. It reads the label to learn which
pool the job belongs to, then **validates it against the pin it reads from the
lockfile** at the job's content ref — ADR-0001's existing read, unchanged. A label
that disagrees with the lockfile is refused with a legible reason: not healed, not
built, not silently honoured.

This is not ceremony. `runs-on` is PR-reachable. Without this rule a PR could name
`faber.playwright-99.0` and cause `npx playwright@99.0 install` to execute **on the
host daemon**, outside the per-job dind sandbox that contains everything else faber
runs (ADR-0008 §2). Validating against the lockfile makes the label strictly
non-additive: a PR can only ask for what its own lockfile already pins, which is
the boundary faber already trusts when it agrees to run that code at all. It is
the same property ADR-0001 gave `RepoConfig` — structurally incapable of widening
what runs on the host — held one level further out.

</details>

### 4. An operator-owned allowlist bounds what may be asked for

In faber's own config, which no repo can reach:

```toml
[needs.playwright]
versions = ">=1.55, <1.70"        # a range…
# versions = "1.61.1, 1.62.1"     # …or an explicit list
```

- A tool with no `[needs.<tool>]` entry is **not provisionable**. Fail closed.
- A version outside the range is refused, with the reason surfaced the way a
  drift mismatch is — never silently ignored, which would look like a match.

This is not redundant with §3, and the pair is the point: §3 stops a PR from
asking for something its lockfile does not pin, while §4 stops a *lockfile bump*
from causing an unbounded build. A PR may legitimately edit its own lockfile, so
§3 alone leaves the operator's host taking build orders from anyone who can open
one. §4 is the operator's ceiling on what CI may cause to be built — bounding disk,
build time, and blast radius in the one file only the operator writes.

### 5. Pools partition by label, and unlabelled jobs change in no way

A runner is registered with the labels its job's needs resolved to, so the pools
for two different versions are disjoint **by construction** rather than by faber's
bookkeeping. A repo that adopts none of this keeps ADR-0008 + ADR-0009 behaviour
byte-for-byte — the same opt-in posture as `[runner_image] build = "off"`.

## Consequences

- **ADR-0009's cross-instance hole closes for free, and this is the strongest
  single argument for this ADR.** Which host minted a runner stops mattering: a job
  matches its own label wherever the runner came from. The peer-advertisement
  protocol ADR-0009 sketched and declined to build is not needed for this — no new
  protocol, no shared state between instances, no consensus.
- **A third version in flight stops costing latency.** ADR-0009 converts that case
  into "one PR green, the other waits"; disjoint pools make it "both green".
- **The hold narrows to unlabelled jobs.** `pool_decision`'s `Hold` and
  `POOL_HOLD_MAX` become unreachable within a labelled pool, which is homogeneous
  by construction. Both stay for unlabelled ones. The `MintOn` substitution is
  untouched and still earns its keep there.
- **`compute_supply` must key on the job's label set, not the profile — this is a
  starvation bug if missed.** Today a runner counts as supply when it carries all
  of a *profile's* labels, and extra labels do not exclude it. So a runner carrying
  `faber.playwright-1.60.0` would be counted as supply for a job needing
  `faber.playwright-1.62.1`, and the shortfall would be under-minted — the one
  direction [ADR-0004](0004-demand-based-provisioning.md) says faber must never
  fail in. Supply has to be counted per label set for labelled jobs.
- **Adoption cost is per repo, charged openly.** An adopting repo adds a setup job
  to every workflow wanting pinned runners, and pays one extra job's latency per
  run (~5–15 s). Against a 2–4 min variant build that is cheap; against a 30 s lint
  job it is not, which is exactly why §2 keeps the versionless capability form for
  jobs that need a tool but not a particular version.
- **More pools means more idle runners.** Partitioning is not free: a narrow pool's
  surplus runner is less likely to be reused by the next job, and ADR-0002's
  idle-reclaim carries more of the load. Demand provisioning's fail-open asymmetry
  still applies — a surplus container is cheaper than a starved job.
- **Lighter images for lighter jobs is a disk win, and a smaller speed win than it
  looks.** Two mechanisms people expect are absent here. Images are **local-only and
  run with `--pull=false`**, so a smaller image does not start faster — there is no
  registry pull to shorten, only a thin writable layer over layers already on disk.
  And memory reservations are learned per `class_key(repo, workflow_id, job_name)`
  from *observed usage*, so a job's learned cap does not shrink because its image
  did: a lint job never pages in browser binaries it does not execute, whichever
  image carries them.

  What can genuinely improve is **Gate 2's availability reading**: fewer multi-GiB
  layers resident in the VM's page cache leaves more real memory, and Gate 2 admits
  against measured availability rather than against the learned caps alone. So
  "more jobs fit on one machine" is plausible through availability, not through
  reservations. It is recorded here as **expected and unmeasured** — the audit's
  per-job `image` / `image_id` fields and the limiter's availability probe are
  already enough to quantify it, and this bullet should be replaced by that number
  rather than defended.
- **The `faber.` namespace becomes a compatibility surface.** Once a repo's
  `runs-on` names it, renaming it breaks that repo's CI. It is the first faber
  identifier a watched repo depends on by name.

## Alternatives considered

- **A literal version in `runs-on`.** Simplest to implement and needs no setup job.
  Rejected: it duplicates the lockfile's version into every workflow file, which is
  the drift ADR-0001 exists to detect, in a location faber cannot treat as
  authoritative.
- **The version in the repo's `.faber/config.toml`.** Same duplication, plus a
  second source of truth that is invisible from the workflow reading it, and one
  more PR-reachable file in the trust argument.
- **Versionless capability labels only.** Kept as a supported form (§2), rejected as
  the only form: subset-based matching means a runner with extra labels still
  matches, so versionless labels cannot partition by version.
- **Peer advertisement of resolved images** (ADR-0009's sketch for the
  cross-instance gap). Not wrong, but superseded for this purpose: labels reach the
  same coherence with no new protocol between hosts and no shared state to keep
  consistent.
- **Keep ADR-0009's hold as the only mechanism.** It is correct and it stays for
  unlabelled jobs, but it converts a real case into latency and is structurally
  unable to help across instances.
- **Have faber rewrite `runs-on` itself.** Not available: `runs-on` is evaluated by
  GitHub from the repo's workflow file before faber sees the job at all.
