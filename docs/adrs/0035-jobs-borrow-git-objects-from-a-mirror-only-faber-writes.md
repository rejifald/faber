---
title: Jobs borrow git objects from a mirror only faber writes
type: adr
status: accepted
date: 2026-10-17
owner: rejifald
---

# ADR-0035: Jobs borrow git objects from a mirror only faber writes

## Status

accepted (2026-10-17), including `clean: false`. It decides
[#164](https://github.com/rejifald/faber-legacy/issues/164). The owner settled the design
on 2026-10-17:

- **faber alone writes.** faber keeps one bare object store per watched repo and
  fetches it from GitHub with its own credentials. A job never fetches into it.
- **Jobs read it through a read-only mount, and it never holds fork-PR refs.** A
  refresh fetches the repo's own branches and tags and the heads of same-repo PRs
  only. It runs when faber sees a push or pull_request run, and on a ~15 minute
  timer.
- **Opt-in per profile.** `[[profile]] git_mirror = true`. Absent is off, which is
  today's behaviour.
- **Delivery is an env var and a workflow step.** faber exports
  `FABER_GIT_MIRROR=<path>`. A short workflow step writes
  `.git/objects/info/alternates` before `actions/checkout` runs, because
  `actions/checkout` has no `reference` input.

The mechanics were measured before this was written (*The mechanics, measured*,
below). They answer the issue's second question and leave no open question. One
detail the owner did not settle is decided here from the measurement:
`actions/checkout` has to run with `clean: false`.

Amends:

- [ADR-0005](0005-persistent-per-repo-build-cache-volume.md): the cache volume
  is no longer the only per-repo state that outlives a job. The git mirror is the
  second, and unlike the cache, jobs cannot write it.
- AGENTS.md, *What faber may delete*: item 1, "an image faber built", becomes "an
  image or a git mirror faber built". The licence is the same: faber can rebuild
  it from nothing it does not own ([ADR-0008](0008-on-demand-runner-image-variants.md)).
- [ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md)'s permission
  table gains a row: the git mirror adds Contents: read.

## Context

### Every job fetches the same objects again

A step-level profile of nearly 300 jobs on one host (one day in mid-October 2026,
two watched repos, about 18 runner-hours; #164) found:

- **Checkout:** about an hour and a half in total, or about 9% of runner time.
  The median was about 25 s, the p90 about 45 s, the max about two and a half
  minutes.
- **A `git fetch --depth=2` of a PR merge ref** took about a minute in one job and
  half that in another.
- **One job type** has a median checkout of over a minute.

The host runs only a handful of Docker jobs at once, and CPU pressure is already
above the ADR-0019 hysteresis at that level. A second saved per job is therefore
worth more here than another admission slot. Queue wait on the host had a median
of about six minutes, and part of the capacity behind it goes to fetching objects
the host fetched minutes earlier.

Every one of those objects is already on the host, in some earlier job's
workspace, and is deleted with that job. The workspace has to stay ephemeral
(ADR-0005 keeps `_work` per job on purpose). The objects do not: git names an
object by the hash of its content, so a copy fetched for one job is exactly the
object any later job would fetch.

### Why a writable shared cache is not the answer

ADR-0005's cache volume is writable by every job of the repo. That is defensible
for build outputs, which a job writes and a later job reads, because
the gate (`src/gate.rs`) refuses fork PRs and public repos. It is not acceptable for the
checkout. Whatever a job writes into a shared object store, the next job checks out
as source code. One misrouted job ([ADR-0030](0030-the-gate-is-defense-in-depth-and-a-bypass-is-detected.md)
exists because such jobs have happened) could then plant code in every later
build. So the store is read-only to jobs, and its only writer is faber, fetching
from GitHub.

### `actions/checkout` has no reference input

`actions/checkout` (v7.0.1, the latest release) initialises the workspace itself
and offers no way to borrow objects. There are three ways to feed it some:

- an `objects/info/alternates` file in the workspace, written before checkout runs;
- `GIT_ALTERNATE_OBJECT_DIRECTORIES` in the environment;
- replacing `actions/checkout` with a wrapper that runs `git clone --reference`.

Which of them survives `actions/checkout`'s own preparation, and whether a shallow
fetch uses borrowed objects at all, had to be measured rather than assumed.

## The mechanics, measured

Measured on 2026-10-17 with git 2.55.0 on macOS 27. The steps reproduce
`actions/checkout` v7.0.1's commands from its source: `git init`,
`git remote add origin https://github.com/<owner>/<repo>`, `gc.auto 0`, then
`git -c protocol.version=2 fetch --no-tags --prune --no-recurse-submodules
[--depth=N] origin +<sha>:refs/remotes/origin/main`, then
`git checkout --force -B main`. The mirror is a bare repo, fetched with
`+refs/heads/*` plus one same-repo PR head, then made read-only (`chmod -R a-w`).
Workspaces borrow from it through `objects/info/alternates`.

There were two repos:

- **faber itself:** 188 files, 2.8 MiB at depth 1, a 6.3 MiB mirror;
- **a public repo of 77 MB (`vitejs/vite`):** 12.3 MiB at depth 1, 68 MiB of full
  history, an 85 MiB mirror built in 16 s.

A stale mirror is the same store held back on `main` by 1, 5 or 20 first-parent
commits. "Received" is the HTTP payload, summed from `GIT_TRACE_CURL`. Fetch times
are wall clock over a home connection. They are relative figures, not CI ones.

| Case (`vitejs/vite`) | Received | Objects sent | Fetch |
|---|---|---|---|
| depth 1, no mirror (today) | 12,588 KiB | 3,384 | 4.4–4.7 s |
| depth 1, fresh mirror | 0 KiB | 0 | 0.85–0.88 s |
| depth 1, mirror 1 commit behind | 6 KiB | 12 | 0.87–1.00 s |
| depth 1, mirror 20 commits behind | 60 KiB | 169 | 0.89–1.03 s |
| full history, no mirror | 69,700 KiB | 111,781 | 23.5–24.9 s |
| full history, fresh mirror | 0 KiB | none: no server contact | 0.06–0.07 s |
| full history, mirror 20 commits behind | 491 KiB | 306 | 0.92 s |
| checkout's `fetch-depth: 0` refspec (all branches and tags), no mirror | 76,145 KiB | — | 25.8 s |
| the same, mirror with branches only | 1,725 KiB | — | 2.8 s |
| the same, mirror with branches and tags | 88 KiB | — | 1.0 s |
| PR merge ref, depth 2, no mirror | 12,590 KiB | 3,392 | 4.4–5.7 s |
| PR merge ref, depth 2, fresh mirror | 1 KiB | 1 | 1.0–1.1 s |
| PR merge ref, depth 2, mirror 1 behind, PR head absent | 8 KiB | 23 | 0.94–0.99 s |

On faber, the same shape holds:

- depth 1 dropped from 2,867 KiB to 0 with a fresh mirror;
- it dropped to 12 KiB with a mirror 3 commits behind;
- it dropped to 993 KiB with a mirror 20 commits behind, a stretch in which large
  source files changed many times;
- a PR merge ref at depth 2 dropped from 2,870 KiB to 1 KiB.

What this settles:

1. **A shallow fetch does use the borrowed objects.** The packet trace shows the
   client sending 16 `have` lines, which are the mirror's ref tips: fetch
   advertises an alternate's refs as its own haves. The server then sends only
   what the mirror lacks. With a fresh mirror that is nothing, and only the
   negotiation round trip is left.
2. **`fetch-depth: 0` becomes nearly free.** Fetching one commit's full history,
   with that commit already in the mirror, git finds everything local and never
   contacts the server: 24 s and 68 MiB today, 0.06 s with the mirror.
   `actions/checkout`'s own `fetch-depth: 0` fetches every branch and tag by name,
   so it always lists the refs. With a mirror that holds tags as well, that came to
   88 KiB and 1.0 s, against 76 MiB and 25.8 s today. A mirror without tags still
   sent 1.7 MiB, which is why §1 fetches tags. Tools that diff against a base
   commit no longer need a shallow checkout to stay fast.
3. **Staleness costs little.** A mirror one merge behind transfers kilobytes. The
   merge commit of a PR is never in the mirror, and does not need to be: its
   parents are, and the fetch came to 1 KiB.
4. **`actions/checkout` wipes the alternates unless `clean: false` is set.** On a
   workspace that already holds a `.git`, `prepareExistingDirectory` runs
   `git clean -ffdx` and then `git reset --hard HEAD`. After the pre-step, `HEAD`
   is unborn, so the reset exits 128 and checkout deletes the directory,
   alternates included. It then fetches everything, and no error appears
   anywhere. With `clean: false` it skips both commands, reuses the
   repository, and fetches through the alternate. Its other checks pass: the
   fetch URL matches, `isDetached` reads an unborn `HEAD` as detached, and
   `git submodule status` exits 0.
5. **A read-only mirror is never written.** Fetch, checkout, `git gc` and
   `fsck --connectivity-only` in a borrowing workspace all succeed against a
   read-only mirror.
6. **A missing mirror degrades to today.** An alternates line naming a path that
   does not exist prints `error: unable to normalize alternate object path`, five
   times. The fetch still exits 0 and downloads everything. The step below checks
   for the directory so that this never prints.
7. **Readers are safe while faber writes.** 20 borrowing checkouts ran while the
   mirror was repacked (`repack -a -d --keep-unreachable`) 45 times and fetched into
   between repacks. All 20 passed `fsck --connectivity-only` with a clean tree.
8. **A borrowing repo can be made standalone in a fraction of a second.**
   `git repack -a -d` followed by removing the alternates file copied the 12.4 MiB
   it borrowed in 0.11 s, and `fsck` passed. This is for a step that copies `.git`
   somewhere the mirror is not mounted.
9. **`GIT_ALTERNATE_OBJECT_DIRECTORIES` works for the fetch, but the repository
   depends on it afterwards.** Once the variable is gone, `fsck` fails (exit 10).

## Decision

### 1. One bare store per watched repo, written by faber alone

For each repo that at least one `git_mirror = true` profile serves, faber keeps a
bare git repository in a named Docker volume:

- labelled `org.faber.kind=git-mirror` and `org.faber.repo=<owner/repo>`;
- read by its labels, never by its name (AGENTS.md: read labels, never names);
- not one of the janitor's per-job kinds, so the sweep never reaps it, pinned by a
  `sweep.rs` test in the way the cache volume's is.

**The refspec is explicit, never `--mirror`.** `git clone --mirror` sets
`+refs/*:refs/*`, and GitHub advertises `refs/pull/*`, fork PRs' heads and every
merge ref included. A refresh therefore fetches:

- `+refs/heads/*:refs/heads/*` and `+refs/tags/*:refs/tags/*`, with `--prune`;
- `+refs/pull/<n>/head:refs/pull/<n>/head` for each open PR whose
  `head.repo.full_name` equals the base repo, read from the REST API. A ref name
  cannot tell a fork's head from a same-repo head, so the list has to come from
  the API. PR refs no longer on the list are deleted (`update-ref -d`);
- never `refs/pull/*/merge`. GitHub recomputes merge refs, and the measurement
  shows that a merge ref's parents are enough.

**When it refreshes.** faber has no webhook. It already polls each watched repo's
workflow runs, so an "event" is a new `push` or `pull_request` run in that poll.
Each one triggers a refresh of its repo, and a 15-minute timer refreshes every
mirror regardless. Refreshes are serialised per repo and coalesced: one runs at a
time, and requests that arrive meanwhile collapse into one follow-up.

**How it refreshes.** A refresh is a short-lived, faber-owned container, never a
runner. It runs the profile's runner image, which has git because
`actions/checkout` needs git. It mounts the volume read-write, on no job network
and with no dind. faber's credential reaches it as a read-only mounted
git-credential file, never as an argument or an env var, in the same way the
registry credential reaches a runner (`REGISTRY_CONFIG_MOUNT`, `src/docker.rs`).
The fetch runs with `fetch.fsckObjects=true`, so a malformed object is refused at
the door. A refresh that fails or times out leaves the previous mirror in place. A
stale mirror is still correct, only less useful. The failure is logged and shown on
the dashboard, and nothing fails a job.

**The token** needs Contents: read on the repo. Under ADR-0027 that is one more row
in the permission table. It is asked for only while a profile has `git_mirror` on.

### 2. Jobs get it read-only, and only their own repo's

- **The mount.** A `git_mirror = true` runner container mounts its own repo's
  mirror at `/runner/_git` with `:ro`. That is a sibling of `/runner/_cache`,
  outside `_work`. JIT runners are minted per repo
  (`/repos/{owner}/{repo}/actions/runners/generate-jitconfig`), so a job only
  ever sees the mirror of the repo whose token it already holds. The mirror shows
  it nothing that `actions/checkout` could not fetch.
- **The variable is the contract.** `FABER_GIT_MIRROR=/runner/_git` is exported
  only when the volume was mounted, and after the profile's `env`, so it always
  describes the real mount. That mirrors `FABER_CACHE_DIR` (ADR-0005).
- **Not before the first refresh.** A volume whose first refresh has not finished
  is not mounted. A job admitted while the first clone runs gets a plain checkout.
- **Best-effort.** If anything about the mount fails, the job runs without it.
  It is slower, and never fails because of the mirror.
- **The mirror must be readable by the job's uid.** faber writes it world-readable
  (the refresh runs with `umask 022` and `core.sharedRepository=world`), because it
  cannot know the uid the image runs jobs as. The cache volume has the same problem
  in the write direction. Git's `safe.directory` ownership check applies to the
  repository being used, not to its alternates. The implementation's runtime test
  confirms both on a real read-only volume with the runner image's uid 1001.

### 3. The workflow opts in with one step and one input

```yaml
- name: Borrow faber's git objects
  shell: bash
  run: |
    [ -d "${FABER_GIT_MIRROR:-}/objects" ] || exit 0
    git init -q "$GITHUB_WORKSPACE"
    git -C "$GITHUB_WORKSPACE" remote add origin "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY"
    echo "$FABER_GIT_MIRROR/objects" > "$GITHUB_WORKSPACE/.git/objects/info/alternates"
- uses: actions/checkout@v4
  with:
    clean: false
```

- **`clean: false` is required.** Without it, checkout deletes what the step wrote,
  and the step silently does nothing (finding 4). On a faber runner the workspace
  is new for every job, so there is nothing to clean. On a GitHub-hosted runner the
  step exits at its first line, and `clean: false` on a fresh VM changes nothing.
- **The remote URL must be exactly what checkout computes,**
  `https://github.com/<owner>/<repo>` with no `.git`. A mismatch makes checkout
  delete the directory too. With checkout's `path:` input, the step writes to the
  same path.
- **`fetch-depth` stays the workflow's choice.** With the mirror, `fetch-depth: 0`
  costs about what depth 1 does (finding 2).
- **A step that copies `.git` elsewhere dissociates first.** A Docker build context
  that includes `.git`, or a tarball of the workspace, would otherwise carry a
  repository whose objects are on a path that does not exist there. The fix is
  `git repack -a -d && rm .git/objects/info/alternates`, measured at 0.11 s
  for 12 MiB (finding 8).
- **Jobs with a `container:` get no benefit yet.** The runner does not pass
  `/runner/_git` into a job container. The guard line makes the step a no-op there.

### 4. Concurrency: faber writes in place, jobs read through it

Git already keeps a repository readable while it is written:

- **A fetch** writes its pack under a temporary name and renames it into place,
  `.idx` last. Readers ignore a pack that has no index, and refs move only after
  their objects exist, each ref through its own lockfile rename. So a reader that
  sees a ref sees all of its objects.
- **A repack** writes the new pack before deleting the old ones. A reader that
  misses an object rescans the pack directory and retries. Finding 7 exercised
  exactly this: 45 repacks under 20 readers, with no failure.

One danger is specific to alternates and is not a race. A borrowing workspace
holds objects it never copied. If the mirror deletes an object a running job
borrowed, that job's repository is corrupt for the rest of the run. This can happen
when the commit's branch was force-pushed away and gc then pruned the commit. The
`git clone --shared` documentation warns about exactly this. So:

- **Routine maintenance never deletes an object.** After a refresh faber runs
  `git repack -a -d --keep-unreachable`, only when enough loose objects or packs
  have built up. Unreachable objects stay packed.
- **Pruning waits until nothing mounts the mirror.** `git gc --prune=now` runs only
  while no runner container mounts the volume. That is at most daily, in the
  janitor's cadence. A mirror that is never idle is still bounded. Its growth from
  unreachable objects is the force-pushed history of one repo, and §5 can delete it
  outright.

### 5. Disk: counted by the gate, reclaimable, cheaper per job

- **It counts against the floor without a new rule.** The mirror lives on the
  Docker volume that ADR-0024's gate measures. Its bytes are used space in that
  measurement, the same as the cache volume's. Jobs never write it, so the
  per-job charge of ADR-0032 has nothing to add for it.
- **Its growth is a refresh's, not a job's.** After the first clone, a refresh
  adds what the repo's pushes added. The first clone is large: for the public repo
  above, it is 85 MiB. faber skips it while free space, less the repo's size
  from the REST API, would sit below `disk_admission_floor_gib`, and tries again on
  the next timer. With the floor unset, it builds. *Amended 2026-11-01: the higher
  of this floor and `disk_floor_gib`, one first clone at a time (see the
  amendment below).*
- **It makes each opted-in job smaller.** A depth-1 workspace's `.git/objects` went
  from 13 MiB to 60 KiB for the public repo above. ADR-0032's charges stay the
  constants they are; this ADR does not lower them.
- **It is reclaimable.** A mirror is rebuilt from GitHub with nothing else. That is
  ADR-0008's reproducibility licence, so it joins "what faber built" in AGENTS.md,
  not the operator-declared set of [ADR-0016](0016-reclamation-is-delegated-not-inferred.md).
  - **Under disk pressure** (`disk_floor_gib` reclamation), faber may delete a
    mirror that no running container mounts, before anything the operator
    declared. Docker refuses to remove a mounted volume in any case. The cost
    is that the next jobs check out cold until a refresh rebuilds it.
  - **No longer needed:** a mirror whose repo no `git_mirror` profile serves any
    more is deleted at the next janitor pass, once unmounted.
- **Visible.** The dashboard shows each mirror's size, its last refresh and its
  last refresh error.

### 6. Poisoning: what a job, a PR and GitHub can and cannot do

- **A job cannot write it.** The mount is read-only, so no uid, sudo or
  `chmod` inside the container changes that. A job can point its own workspace's
  alternates anywhere it likes. That affects only its own workspace, which
  dies with it.
- **Content addressing defends a lookup.** Git looks up a borrowed object by its
  hash. The mirror's objects arrive through `index-pack`, which computes every
  object's name from its content, using git's collision-detecting SHA-1. So an
  object in the mirror can satisfy only a request for exactly its own content. It
  cannot stand in for a different blob, tree or commit. Git does not re-hash
  borrowed objects on read, which is why the write path is what has to be
  trusted, and why it is faber's alone.
- **A malicious same-repo PR head.** Pushing it already takes write access to the
  repo, which is inside the trust boundary of ADR-0014. Its objects in the mirror:
  - **cannot** change what any other ref checks out, since a checkout of `main`
    asks for `main`'s hashes and gets exactly those;
  - **cannot** reach a job of another repo, because mirrors are per repo and
    mounted only into that repo's runners;
  - **cannot** plant malformed objects aimed at git itself, because
    `fetch.fsckObjects` refuses them at refresh;
  - **can** make the mirror larger, for instance with a large blob on a branch.
    That is a disk cost, visible on the dashboard, bounded by the floor (§5), and
    reclaimable. The same person can already do it to every job's checkout today.
- **Fork PRs never enter.** Their heads are not on the refspec (§1), so code from
  outside the trust boundary is not stored on a faber host even as inert objects,
  and strangers cannot grow the disk.
- **A gate bypass (ADR-0030).** Under `on_bypass = "enforce"` the repo's cache
  volume is deleted, because a bypassing job could have written it. The mirror is
  not deleted: the job could not write it, and that is the point of §2. The
  bypassing job could read it, but it could also read the repo itself, so the
  mirror discloses nothing new.
- **GitHub serves bad objects.** This is out of scope. Every checkout already
  trusts GitHub, and the mirror trusts it no more than they do.

### 7. Per host

Each host keeps its own mirrors, refreshed from GitHub on its own (#87). Nothing is
shared between hosts. faber has no shared storage between hosts, and nothing it
builds leaves the host it was built on (AGENTS.md: no image or package is published
publicly). The cost per host is one first clone per repo, then incremental fetches
that are a ref listing when nothing changed. A host where `git_mirror` is off is
unchanged by this ADR.

When job code moves into faber-owned VMs ([ADR-0026](0026-job-code-runs-only-inside-vms-faber-owns.md)),
the mount becomes a read-only share or disk into the job's VM. The contract is
unchanged: a store only faber writes, a variable that is set only when it is there,
and a step in the workflow.

### 8. The key ships off

`[[profile]] git_mirror = true`. Absent or `false` means no mirror, no mount and no
variable, which is today's behaviour. It turns a feature on rather than tightening
anything, so AGENTS.md's three-state shape is not needed. The settings schema
(`src/settings.rs`) shows it with the profile's other grants. It is
restart-applied, like `cache_volume`.

### Rollout

1. Land the mirror, the refresh and the mount behind the key. Put the workflow step
   in docs, with the `clean: false` requirement in bold.
2. Turn it on for one profile of one watched repo on the builder, and add the step
   to that repo's workflows.
3. Re-run #164's step profile after a week. The checkout step's median and total
   time are the measure.

## Consequences

- **Checkout time comes down to a round trip and the worktree write.** In the
  measurement, transfer fell to between zero and tens of KiB, and what was left was
  one negotiation round trip of about 0.9 s plus writing the files. If that holds
  for the watched repos, most of #164's hour and a half a day (about 9% of runner
  time) is recovered. That is a projection from a public repo smaller than the
  watched ones.
  Rollout step 3 measures it.
- **History becomes affordable.** A workflow that needs a base commit can use
  `fetch-depth: 0` without paying for it.
- **Workflows carry a step and `clean: false`.** Forgetting either is silent: the
  job is as slow as today. The step's first line also keeps it harmless on
  GitHub-hosted runners and on hosts with the key off.
- **One more faber-owned volume per repo.** It counts toward disk and appears on the
  dashboard, and it can be deleted under pressure at the price of a few cold
  checkouts.
- **One more token permission while it is on:** Contents: read (ADR-0027).
- **`container:` jobs gain nothing** until the runner passes the mount into job
  containers.

## Alternatives considered

**faber exports `GIT_ALTERNATE_OBJECT_DIRECTORIES` itself.** This needs no workflow
change and survives `clean: true`. It was rejected for three reasons. It applies to
every git command in the job, including other repositories the job clones. A
repository built under it is broken once the variable is gone (finding 9), which is
what a later container, a Docker build or a cached workspace would see. And it is
invisible in the workflow, so nobody reading it can tell why a checkout is fast or
why a copied repository is corrupt.

**A wrapper action around `git clone --reference`.** `--reference` is the same
alternates file, written by clone. The wrapper would have to reproduce
`actions/checkout`'s auth, ref resolution, `persist-credentials` and submodules, all
to gain nothing the alternates step does not already deliver (findings 1–3).

**A cache volume jobs write (ADR-0005's model).** Rejected in *Context*: the checkout
is code, and a shared writable store lets one job choose the next job's source.

**`git clone --mirror`.** Its refspec takes every advertised ref, fork PR heads and
merge refs included (§1).

**A caching proxy for github.com.** It would have to terminate TLS for GitHub inside
the job's trust path, and it caches packs, which differ per request, rather than
objects.

**Bake the repo into the runner image.** The objects go stale with every push, each
refresh is an image rebuild, and the image is the protected, never-reaped artifact
of AGENTS.md. The mirror is cheap to throw away; the image is not.

**One mirror shared across hosts (#87).** faber has no shared storage between hosts,
and building it is a larger decision than this one. Per-host mirrors cost one first
clone each.

**Also cache service images (#86) under the same rule.** The rule, faber-owned and
read-only to jobs, would carry over. The mechanism would not: images reach a fresh
dind daemon through a registry or `docker load`, not through alternates. #86 stays
its own decision.

## Amendment (2026-11-01): a first clone must not start the next reclamation

§5 weighed the first clone against `disk_admission_floor_gib` and reclaimed mirrors
below `disk_floor_gib`. On the builder the Linux VM disk hovered around the reclaim
floor, well above the admission floor. A clone that left the disk above the
admission floor was admitted, the next janitor pass found the disk under the
reclaim floor and deleted every mirror nothing mounted, and the next timer cloned
them all again. Over about ten days the janitor reclaimed the mirrors nearly 800
times, so jobs almost never found one warm. Every refresh tick also ran a dozen or
so clones at once, each weighing the same free space and each starting a
credential fill on a busy disk. Those fills were the timeouts of
[#348](https://github.com/rejifald/faber-legacy/issues/348).

**A1. The first clone is weighed against the higher of the two floors.** It builds
only if free space, less the repo's size, stays at or above both
`disk_admission_floor_gib` and `disk_floor_gib`. A clone that would put the disk
under the reclaim floor waits for the next timer instead of starting the
reclamation that deletes it. With both floors unset or `0`, it builds, as before.

**A2. First clones run one at a time.** Each holds a host-wide lock from before it
reads the disk to the end of its clone, so the next one weighs the disk the last
one left. Routine refreshes of existing mirrors are not serialized: they add only
what the pushes added.

**Consequence.** On a host whose disk lives under its reclaim floor, mirrors
stay deleted until the disk has room for them, rather than churning. That is the
honest state: jobs check out cold, the dashboard shows no mirror, and the fix is
the disk (#423), not the mirror.

**Alternative rejected: a reclamation that deletes only enough mirrors.**
It would keep some mirrors warm under pressure, but it changes ADR-0024's
reclamation, which deletes each kind whole, and it does not stop a clone from
starting the very reclamation that removes it. A1 is needed either way.
