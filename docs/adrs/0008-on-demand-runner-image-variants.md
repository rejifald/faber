---
title: On-demand runner-image variants, cached while hot
type: adr
status: accepted
date: 2026-08-26
owner: rejifald
---

# ADR-0008: On-demand runner-image variants, cached while hot

## Status

**Amended 2026-10-30** ([#423](https://github.com/rejifald/faber-legacy/issues/423)). A
variant build is gated on its own cost, not on the reclamation floor, and the
disk-pressure tier evicts warm variants only below the margin a build must leave.
See the amendment at the end.

accepted (2026-08-26). Promotes the "auto-heal" possibility recorded as
deferred in [ADR-0001](0001-runner-image-dependency-drift-detection.md) —
"promote only if bump frequency justifies it". It did.

## Context

A runner image bakes a version-locked toolchain: Playwright's browser binaries
at `/opt/ms-playwright`. One image can bake exactly one version. Real repos do
not have one version at a time — a PR that bumps `@playwright/test` needs the
new browsers while `main` and every other open PR still need the old ones, and
that overlap lasts as long as the PR is open, which is days.

ADR-0001 detects this and posts a red commit status. That was the right first
step and it is not a fix: the bump PR still cannot go green until someone
rebuilds the image by hand, and the moment they do, every *other* open PR goes
red instead. The transition window has no configuration of the single image
that is correct for all of it.

This has already been worked around by hand, twice — once by rebuilding, once
by hand-baking a dual-version "bridge" image carrying two browser sets. The
bridge worked and is exactly the sort of thing AGENTS.md calls a bug report
against faber: an operator doing routine work by hand to keep CI running.

The deeper reason it is faber's problem: **the image is currently a property of
the profile** (`DockerConfig.image`, one static string per `[[profile]]`), when
it wants to be **a function of the job**. Everything below is mechanism.

## Decision

faber resolves the runner image per job, and builds the one the job needs if it
does not exist yet.

### 1. Resolution chain, at provision time, before admission

1. The profile supplies the **base** image — operator-owned, built off-repo,
   never modified and never reaped.
2. Read the pinned `@playwright/test` from `package-lock.json` at the job's head
   SHA. This is ADR-0001's existing read, unchanged.
3. Pinned == the base's baked `org.faber.playwright.version` ⇒ **run on the
   base**. Zero cost, and the overwhelming common case.
4. Otherwise resolve the variant tag. Present ⇒ run on it. Absent ⇒ build it.

Resolution runs **inline, before admission**, with the claim still parked
(`admitted = false`): a build must not hold a memory reservation while it
downloads browsers, and a reclaim issued during one must be honoured without a
container ever having existed.

### 2. The variant is a thin derivation faber owns

```dockerfile
ARG BASE
FROM ${BASE}
ARG PW_VERSION
RUN PLAYWRIGHT_SKIP_BROWSER_GC=1 \
    npx --yes playwright@${PW_VERSION} install --with-deps chromium firefox webkit
```

Three properties are load-bearing:

- **`--with-deps` runs in the layer.** The alternative design — mount a
  per-version browser directory over the baked path, no build at all — is
  cheaper and was rejected for exactly this: browser builds occasionally need
  new system libraries, and a volume carries none. It would trade a legible
  version mismatch for a missing-`.so` error, which is the *precise* class of
  confusing failure ADR-0001 exists to eliminate.
- **The browser GC is off.** Left alone, `playwright install` deletes the browser
  revisions the new version doesn't need. That is right on a normal filesystem and
  wrong in a layered image, where the deleted files remain in the parent layer
  regardless: the variant pays for the base's browsers either way, and pruning them
  only means it cannot *use* what it is already carrying. Measured on a real runner
  image, the variant is the same size with the GC on or off — but serves one
  version with it on and two with it off. Free capability, in other words, and
  accumulation is bounded at two sets because a variant is always derived `FROM` the
  base and never from another variant. It is also what makes the output equal to the
  dual-version bridge image previously built by hand: the browser directories are
  identical, produced without a human.
- **The recipe is faber's, never the repo's.** The base comes from the
  operator's profile; the derivation is faber's own template (overridable by an
  operator path in faber's config, not by anything a PR can reach). A repo can
  ask to be healed; it can never say what image runs. This is ADR-0001's
  `RepoConfig` property — structurally incapable of expressing trust, gate,
  backend or image — held one level further out, and it matters more here: a
  `docker build` executes on the **host daemon**, outside the per-job dind
  sandbox that contains everything else faber runs.

faber, not the template, applies the labels — `org.faber.playwright.version`
included. What the image *is* is a fact faber asserts about something it built,
not a line the recipe has to remember to get right.

### 3. Tag on the base's image ID, not its tag

`faber-runner:<base-id12>-pw-<version>`.

Rebuild the base — a new runner release, new system libraries — and every
variant tag changes, so the old ones fall out of use and get evicted normally.
Keying on the base's *tag* instead would let a variant built against a
superseded base serve jobs silently for months, which is the same class of bug
as the drift this ADR exists to close.

### 4. Cached while hot; evicted because it is reproducible

AGENTS.md forbids faber ever reaping a runner image: the operator's base exists
on this builder and in no registry, so one `docker rmi` takes CI down with
nothing to re-pull. That rule is untouched, because **it is a rule about
irreproducible images**. A variant faber built from a recipe faber owns can be
rebuilt from nothing but its tag. Reproducibility is what licenses eviction, and
it is the only thing that does.

The filter is `org.faber.owned=true AND org.faber.kind=image`, which the base
can never match — it carries no faber labels at all.

- Keep the `keep` most-recently-used variants; evict beyond that.
- Evict any variant idle longer than `idle_ttl`, whatever its rank. A version
  nobody has needed in a week is not hot, even if it is the only one.
- Never evict an image a running container uses (checked, and `docker rmi`
  refuses it anyway), nor one handed to a job within the last few minutes. That
  second shield is not redundant: a job resolves its image *before* admission and
  may then park in the memory-budget queue, so between being handed a tag and
  having a container there is a window in which it is neither idle nor visibly in
  use. Evicting inside it would fail a job faber had just built an image for,
  which is the single outcome this whole mechanism exists to prevent. The shields
  hold under disk pressure too — a variant is worth a rebuild, a failed job is
  not.
- Last-used lives in faber's own ledger — Docker records no last-use time — and
  is reconciled against the daemon's actual image list at startup, so a variant
  removed by hand does not linger and one built by hand is not evicted blind.

Eviction runs in the janitor, and takes ADR-0007's admission pause **only when
it has something to delete**: candidates are computed from metadata first, so a
host with a stable version set pays nothing. Under the disk floor it becomes an
extra reclamation tier, ahead of giving up.

### 5. Failure posture: best-effort, exactly like the cache volume

A build that fails or times out is not an error the job inherits. faber runs the
job on the base image and falls through to ADR-0001's `mode` — normally a red
commit status explaining the mismatch, which is the status quo this ADR
improves on, not a regression from it. An optimisation must never turn a job
that would have run into one that cannot.

### 6. Off by default

`[runner_image] build = "off"` unless the operator turns it on, and with it off
the provisioning path is byte-for-byte what it is today, including drift
staying detached. A repo may opt *out* with `.faber/config.toml [playwright]
heal = false`; it cannot opt in on its own, because the cost lands on the
operator's host.

## Consequences

- The first job on a new version pays the build (~2–4 min, one browser layer).
  Every job after it pays nothing. Compared against a red job and a manual
  rebuild, this is not a cost.
- Two open PRs on different Playwright versions both go green, unattended. That
  is the whole point.
- **The superset property is load-bearing, not a nicety — and this was learned in
  production.** JIT runners are not pinned to the job they were minted for (see
  [ADR-0002](0002-reconcile-inflight-claims.md)); on a busy host, `runner … ran job
  X, not the job it was minted for` is the routine case, not the exception. So a
  runner minted with the 1.62.1 variant is regularly handed a job from a PR that
  pins 1.61.1. With the browser GC on, that job would fail on missing browsers —
  faber would have *created* the failure class it exists to remove, for a job that
  was previously fine. Keeping the base's browsers is what makes the swap
  survivable, which is a stronger reason for `PLAYWRIGHT_SKIP_BROWSER_GC=1` than
  the disk argument it was first adopted for.

  It is **mitigation, not a guarantee**, and the bound is worth stating plainly: a
  variant carries exactly two versions, its base's and its own. A job pinning a
  third — an older PR still on 1.60 — handed a 1.62.1 variant runner still fails.
  Closing that properly means pinning runners to jobs, which is a different
  decision than this one; until then the exposure is bounded by how many distinct
  versions are in flight at once, which is the same number `keep` is sized for.

  **Taken up in [ADR-0009](0009-runner-image-pool-coherence.md)**, which found
  that runners *cannot* be pinned to jobs — no GitHub API scopes a JIT
  registration to a job id — and holds the pool to one image generation per repo
  instead. It also records the live incident this consequence predicted: two jobs
  of one run swapping runners outright.
- Disk grows by about **2.3 GiB per live version** — measured, not estimated, as
  the gap between a real base and its variant. Base layers are shared between
  variants, so N versions cost the base plus N deltas, and `keep` bounds N. That
  is the same order as the hand-built bridge image, which carried its versions at
  once with no bound and no eviction.
- **A build is minutes; a resolution is milliseconds.** A rebuild against a real
  base with a warm BuildKit cache took about a minute and a half; a genuinely cold
  build (no cached browser archives) is longer and has not been cleanly timed here.
  Every job after the first resolves in a hundred milliseconds or so — a
  `docker inspect` and a ledger write. That ratio
  is the design: pay once per version, never per job.
- **Resolution moves two GitHub reads onto the provisioning path** — the repo's
  `.faber/config.toml` and its lockfile, which for a large repo is a megabyte or
  two. They were always made per job; ADR-0001 simply made them detached, and
  healing cannot be, because the answer decides which container to start. On a
  labelled image this is a few hundred milliseconds against a provisioning ramp
  already measured in seconds. Sibling jobs of one run repeat identical reads; a
  memo keyed on `(repo, head SHA)` would remove that, and is deliberately not
  built here — it is a second mechanism with its own staleness, for a cost that is
  not currently the one that hurts.
- Builds serialize behind one lock. Two jobs from the same PR would otherwise
  each build the same browser layer, and the second would win the tag race by
  discarding its own work.
- **The build is not memory-capped.** BuildKit does not honour `--memory`, so a
  build runs outside the autolimiter's budget. It is bounded in time and
  serialized to one at a time, which keeps the exposure to a single concurrent
  build; a build competing with admitted jobs for the VM's memory remains a real
  (small) risk, and is the first thing to revisit if the host starts OOMing
  during builds.
- This ships to a follower host under [ADR-0006](0006-fleet-autoupdate.md).
  Defaulting to `off` is what makes that safe: a release carrying this changes
  nothing on a host that has not asked for it.
- Nothing is published. Variants are built locally and stay locally — the only
  design that satisfies the private-only constraint at all, since a per-PR image
  cannot be a thing an operator pushes by hand.

## Alternatives considered

- **Per-version browser volume, no build.** Cheaper and faster (no build at
  all), and rejected: `--with-deps` has nowhere to run, so a browser build
  needing a new system library fails with a missing-library error instead of a
  version mismatch. Also makes the image's baked label lie about what is
  mounted, which breaks ADR-0001's detection rather than composing with it.
- **Build the repo's own runner Dockerfile at head SHA.** Single source of
  truth, and the bump PR carries any Dockerfile change with it. Rejected: a full
  build is ten-plus minutes rather than two, it needs the whole build context
  fetched, and it runs repo-authored build steps on the host daemon — outside
  the sandbox that contains every other thing a repo makes faber run.
- **One image carrying every version** (the hand-made bridge, automated).
  Rejected: unbounded growth with no eviction story, every job carries every
  version, and a rebuild is needed for each addition anyway — so it is this
  design with the cache replaced by a leak.
- **Keep surfacing only, rebuild by hand.** The status quo. Rejected by two
  hand-rebuilds and a hand-built bridge image.

---

## Amendment (2026-08-27): clear a stale surfaced failure on heal

**Supersedes** the Decision section's framing of healing as posting nothing —
"nothing is surfaced when that works, because at run time there is no mismatch
left to surface." True about the job; false about the commit.

A commit status is permanent per `(SHA, context)` — it never expires, and
nothing clears it except a later post on that exact pair. ADR-0001 posts a
`failure` on `faber/playwright-drift` the first time a SHA surfaces a drift.
Healing, as shipped, never posted anything at all, so once a SHA had a
`failure` on it — surfaced before this ADR existed, or before an operator
turned `[runner_image] build = "on-demand"` on, or simply on the one job that
lost the resolution race before the variant existed — no later job could ever
touch it, no matter how many of that SHA's jobs healed cleanly afterward.

Live incident: a pull request in a watched repo. `failure` surfaced on its head
SHA because the repo pinned a newer Playwright than the base image baked. Every
job on that SHA from about four hours later onward healed onto the new version's
variant and passed. The check stayed red anyway, because healing had nothing to
say to GitHub — the PR showed a failing gate for a mismatch that had not been true
for hours. A status that can only ever go red and never back to green is worse
than no status at all: an operator who has learned that `faber/playwright-drift`
lies stops reading it, which is the exact failure mode ADR-0001 exists to
prevent for the underlying Playwright mismatch.

So a heal now posts a `success` status on the same `faber/playwright-drift`
context — same SHA, same context string, so it supersedes any `failure` that
context already carries. It is gated on the same resolved `mode` a fresh drift
uses (`drift::act_healed`, mirroring `act`): only `surface` posts, because only
`surface` can have left a `failure` behind to clear; `off` stays exactly as
silent on a heal as it is on a fresh drift, and `gate` posts nothing in either
direction until Phase 2 gives it something to post. A failure to post the
clearing status degrades the same way a failure to post the original `failure`
does — audited, never a hard error, and never something that unwinds the heal
itself, which has already succeeded by the time the status POST is attempted.

This does not touch §5's failure posture: a build that fails or times out still
falls through to ADR-0001's `mode` unchanged, and no `success` is ever posted
for a heal that did not, in fact, produce a running variant.

---

## Amendment (2026-10-30): a build is gated on its cost, and eviction stops below where it could be rebuilt

**Supersedes** two lines of §4 and the pre-build gate `ensure` has carried since
ADR-0007's floor reached it: "under the disk floor it becomes an extra reclamation
tier" (every variant not running a job goes), and "refuse to start a build while
free space is below `disk_floor_gib`". Each rule was sound alone. Together they
were a loop.

### Context

In October 2026 a watched repo pinned a Playwright version its base image did not
bake, so its jobs needed a variant. The Linux VM's disk (a fixed 64 GiB image,
[#423](https://github.com/rejifald/faber-legacy/issues/423)) sat a few GiB under the
default 25 GiB reclamation floor all day. Several disk-pressure passes over the day
each evicted that variant, and each left free space still under the floor. Every
rebuild was then refused for being below the same 25 GiB floor, so
`needs::evaluate` saw the base's version and every job declaring
`faber.playwright-<v>` was refused a runner: dozens of refusals through the
afternoon, ending only when free space happened to clear 25 GiB. Evicting freed
about 2.3 GiB; rebuilding needed 25 GiB free. Nothing faber did could close that
gap.

The reclamation floor is a point to start reclaiming at, not a measure of what is
safe to write: ADR-0024 rejected gating admission on it because a host spent hours
under it while well over a hundred jobs succeeded, and the build gate made the same
mistake for builds.

### Decision

Two thresholds, both derived from the host's floors (`VariantDisk` in
`src/image.rs`), with one build's charge between them:

- **The margin** a build must leave free: the stated admission floor (ADR-0024 §1)
  or 10 GiB, whichever is larger. Ten is ADR-0032's charge for a dind job, so after
  a build there is still room for the heaviest job admission lets in, and the disk
  stays well clear of ADR-0032's 3 GiB critical floor.
- **The build threshold**: the margin plus a **4 GiB build charge**. The charge
  sits above the 2.3 GiB measured in this ADR's Consequences, as ADR-0024's job
  charges sit above what was measured: BuildKit keeps the downloaded browser
  archives until a prune, and newer browsers are not smaller. A constant until
  faber measures its own builds.
- **Both are capped at `disk_floor_gib`**, so neither is stricter than the single
  floor it replaces, and `0` turns both off as before. An unreadable reading still
  never refuses or squeezes (ADR-0007).

The rules:

1. **A build may start while free space is at or above the build threshold.** On
   the incident's numbers (the default 25 GiB floor, no admission floor stated) that
   is 14 GiB, so every reading of the day would have built.
2. **A disk-pressure pass evicts every variant no job holds only below the margin.**
   Between the margin and the reclamation floor it evicts by the ordinary rule —
   beyond `keep`, or idle past the TTL — and nothing warmer. Above the margin a
   rebuild would be allowed, so evicting a warm variant trades its space now for
   the same space on the next job that needs it, and under a floor the eviction
   cannot lift that trade repeats every pass.
3. **The images live claims hold are shielded in every tier**, beside the in-use
   set and the resolve grace. A claim parked in admission after resolving its
   variant has no container, and once the five-minute grace has passed nothing else
   said it was wanted. This is §4's own reason for the grace, held for as long as
   the claim lives rather than a fixed window.

The gap between the thresholds is one build's charge, which is what keeps the
rules from feeding each other: a pass that evicts just under the margin leaves
less than a rebuild needs, and a build that starts at the threshold ends no lower
than the margin, which does not squeeze.

### Consequences

- A host under its reclamation floor but with room for a build keeps its warm
  variants and builds new ones. Jobs needing a variant are no longer refused for a
  floor no eviction could reach.
- The disk-pressure pass frees less on such a host: warm variants stay. ADR-0024
  §3's backoff already stops a pass that freed nothing from repeating every tick,
  and the disk fault stays raised, which is accurate: the floor is not met.
- Below the margin the old behaviour holds in full: every variant no job holds
  goes, and no build starts until free space returns above the threshold by other
  means. Jobs needing a variant are then refused as before; that is the disk
  coming first, which §4 already chose.
- A refused build still surfaces only as a log line and a refused-needs warning;
  surfacing it on the dashboard is [#353](https://github.com/rejifald/faber-legacy/issues/353)'s,
  unchanged here.
- No new key. The thresholds follow from keys the host already states, and on a
  host whose floors leave them equal to the old floor (`disk_floor_gib` of 10 or
  less) nothing changes.

### Alternatives considered

- **Lower `disk_floor_gib` on the host.** Hides the loop at today's numbers and
  returns it whenever the disk sits just under the new floor. The loop is in the
  rules, not the value.
- **Never evict variants under pressure.** Gives up §4's protection for a disk
  that really is filling; below the margin a variant must still be able to go.
- **Gate the build on the variant's real size.** A variant that does not exist yet
  has no size to read, and the one it replaces was built from a different base or
  version. The measured constant is the honest figure until builds are measured.
- **Grow the VM disk.** Deferred in #423, and not this ADR's to decide. This
  amendment removes the deadlock at any disk size.
