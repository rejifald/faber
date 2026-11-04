---
title: Jobs fetch packages and LFS objects through a mirror only faber fills
type: adr
status: proposed
date: 2026-10-21
owner: rejifald
---

# ADR-0047: Jobs fetch packages and LFS objects through a mirror only faber fills

## Status

proposed (2026-10-21). The owner settled the design on 2026-10-21; this records it
ahead of implementation, following the precedent of ADR-0007, ADR-0010 and ADR-0046.

It stays `proposed` until every stage in *Validation* is live on the first watched
repo and measured against its baseline. Each stage that lands fills its rows; the
ADR is accepted when the last one does.

Corrected 2026-10-31: this ADR called faber's token read-only. It is not. It is
faber's one fine-grained token, which carries Administration: write on every
watched repo ([ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md)),
and with `[mirror.lfs]` on it sits on the Docker engine that runs jobs, where a dind
job is root. Until a broker on the host keeps it out of every VM, `[mirror.lfs]` is
refused beside dind ([docs/git-mirror.md](../git-mirror.md#not-beside-dind)). §4 and
*Consequences* say so below.

Amends:

- [ADR-0035](0035-jobs-borrow-git-objects-from-a-mirror-only-faber-writes.md): the
  git mirror becomes one of two shapes of "faber mirrors" (§1). Its mechanics, its
  workflow step and its `git_mirror` key are unchanged. It gains one consumer:
  `warm_on_push` reads its refreshes (§4).
- [ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md)'s permission
  table gains a row: the LFS mirror adds Contents: read. The npm mirror needs no
  permission.
- AGENTS.md, *What faber may delete*: the mirror's cache volume joins "what faber
  built", on the same licence as ADR-0035's store: faber can rebuild it from
  upstream with nothing it does not own.

## Context

### Jobs download the same bytes again, from the internet

ADR-0035 stopped jobs re-fetching git objects. Two larger downloads are untouched:

- **Git LFS objects.** A watched repo keeps its visual-test baselines in LFS. Every
  shard of its visual suite downloads the whole set again from GitHub's LFS
  storage, which is both slow and billed against the account's LFS bandwidth.
- **npm packages.** Every job that installs dependencies without a warm
  `node_modules` fetches tarballs from the public registry. The per-repo cache
  volume (ADR-0005) helps only the jobs that wire it in, and only for the repo
  that wrote it.

Both are content the host already downloaded minutes earlier, for another job. Both
are addressed by content: an LFS object by its sha256 `oid`, an npm tarball by the
`dist.integrity` hash its packument publishes. A verified copy fetched for one job
is exactly what any later job would fetch.

### A store does not fit these tools; a proxy does

ADR-0035's store works because git can borrow objects from a path. LFS can too
(alternates, see *Alternatives considered*), but only with a workflow step, and npm
cannot at all. Both tools, however, fetch over HTTP from a URL they read from
configuration or the environment. A pull-through cache at that URL needs no
workflow change: faber sets the URL, the tool fetches through it, and a miss is
fetched upstream once.

### What a job may and may not write

ADR-0035's rule carries over: whatever a job could write into a shared cache, the
next job would consume as its build input. So a job never writes the mirror. Only
faber fills it, from upstream, after verifying what it fetched. A job's request can
*cause* a fill; it cannot choose the content.

That rule is enough for content-addressed objects, where a request names the hash
it will accept. It is not enough for an upload, and not enough for name-addressed
content (an npm packument names versions, it is not named by a hash). Deciding
*who* may fill or upload those needs an identity faber's attribution cannot give:
faber's JIT runners are pinned to a repo but not to a job, so faber cannot tell
which job is behind a request. That is why strict mode is deferred (§3).

## Decision

### 1. "faber mirrors" has two shapes: a store and a proxy

- **Store** — the git mirror of ADR-0035. A bare repository faber writes, mounted
  read-only, borrowed through alternates by a workflow step. Unchanged; its key
  stays `git_mirror`.
- **Proxy** — new: a pull-through cache that tools reach over HTTP. v1 covers Git
  LFS and npm. Other ecosystems (a Docker Hub pull-through, other package
  registries) are later decisions, behind the same front door.

A proxy is used without any workflow change. faber injects environment variables
into the runner container that point each tool at the mirror (§4, §5). A job that
does not want it unsets them.

### 2. One faber-written service, one container per host

- **faber's own code.** The mirror is a small Rust service (axum and reqwest) in
  the faber workspace. It is not an off-the-shelf registry (*Alternatives
  considered*).
- **One container per host, on the runners' Docker engine.** It runs in faber's
  Linux VM (ADR-0026), next to the runners, as a faber-owned container under a
  restart policy. It is never a runner and runs no job code.
- **Its cache is a named volume** labelled `org.faber.kind=mirror`, read by its
  labels, never by its name, and not one of the janitor's per-job kinds, so the
  sweep never reaps it. A `sweep.rs` test pins that, as it does for ADR-0035's
  store.
- **It reaches each job by name, on the job's own network.** faber attaches it to
  each opted-in job's private network with `docker network connect --alias
  faber-mirror`, and detaches it at teardown, in the way ADR-0046 attaches the
  shared Postgres. Jobs share no network with each other through it.
- **Its image is built locally, from the faber checkout.** Nothing is published
  (AGENTS.md). A host without the image shows an attention row (§9) and simply
  does not use the mirror: no env is injected, and jobs run as they do today.

### 3. Policy: fill and upload are separate, and v1 is relaxed only

Each ecosystem has two policy settings:

- **fill** — whether a job's miss makes faber fetch the object upstream, verify it
  and store it;
- **upload** — whether a job may put content into the mirror itself.

v1 has one mode, **relaxed**: any opted-in job's miss may trigger a fill, and no
ecosystem accepts an upload (LFS pushes go to GitHub, §4; npm has no upload, §5).

**Strict mode is deferred.** In strict mode only an operator-triggered, privileged
workflow could fill or upload. Its identity would come from the job's GitHub OIDC
token, its claims verified against GitHub's JWKS. faber's own attribution cannot
grant that privilege, because a JIT runner is pinned to a repo, not to a job: any
job of the repo can land on it. The design keeps the "who is asking" check behind
one interface in the service, so strict mode adds an implementation of it rather
than reshaping the request path.

### 4. Git LFS: GitHub stays the source of truth

- **Downloads go through the mirror.** faber injects `lfs.url` through git's
  environment config (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`,
  `GIT_CONFIG_VALUE_<n>`), pointing at `http://faber-mirror/…` for the container's
  repo.
- **Pushes go straight to GitHub.** The same entries set `lfs.pushurl` to the
  repo's GitHub LFS endpoint. git-lfs then pushes with the job's own credentials,
  the ones `actions/checkout` persisted. faber never pushes LFS with its own
  token. That token is not read-only, though: it is faber's one fine-grained token,
  with Administration: write on every watched repo (ADR-0027). The mirror only
  reads with it.
- **A per-container token names the repo.** Each runner container gets a mirror
  token that the service maps to that container's repo. JIT runners are minted per
  repo (`repos/{owner}/{repo}/actions/runners/generate-jitconfig`), so the token
  can only ever speak for the repo the runner already serves. It travels in the
  injected `lfs.url` itself, so git-lfs presents it on every request without a
  credential helper. It is not a GitHub credential and is useless outside the
  job's network.
- **Objects are stored once, by sha256 `oid`, and verified on fill.** A fill
  downloads through GitHub's LFS batch API with faber's token, reading only, hashes the
  bytes, and stores them only if the hash is the `oid` that was asked for.
- **A per-repo index decides who may read what.** Repo R is granted `oid` X only
  after GitHub's batch API confirmed that R has X. An `oid` not in R's index is a
  miss, checked upstream *as R* even when the bytes are already stored for another
  repo. So a job cannot read another repo's objects by guessing hashes, and the
  bytes are still stored once.
- **Warming on push.** `[mirror.lfs] warm_on_push = true` makes faber fetch new
  objects before any job asks for them. After each git-mirror refresh (ADR-0035
  §1), faber diffs the LFS pointer files between the old and new ref tips (for a
  new ref, the whole tree) and fills the new `oid`s. It needs `git_mirror` on the
  same repo's profile; `faber check-config` and the settings save refuse it when
  `git_mirror` is off. v1 has no include filter: every new pointer is warmed.

### 5. npm: a pull-through of the public registry

- **Upstream is `registry.npmjs.org` only.** faber injects
  `NPM_CONFIG_REGISTRY` and `YARN_NPM_REGISTRY_SERVER` pointing at the mirror.
  pnpm and bun read the npm variable. npm's default `replace-registry-host=npmjs`
  rewrites the lockfile's `resolved` URLs to the configured registry, so existing
  lockfiles go through the mirror too.
- **Anything authenticated passes through uncached.** A request carrying
  `Authorization` is forwarded as is and never stored. Scoped registries a project
  configures itself are untouched; only the default registry is redirected.
- **Tarballs are verified and kept.** A tarball is stored only if it matches its
  packument's `dist.integrity`, and is kept until evicted (§8).
- **Packuments are cached briefly.** The full and the abbreviated (`Accept`)
  variants are cached separately, for 5 minutes, and served stale when upstream
  errors. The service rewrites each version's `dist.tarball` to point at itself.
- **Everything else is forwarded.** A publish (`PUT`) goes upstream, uncached.
  Other endpoints (audit, search, whoami) pass through.
- **No npm upload setting in v1.** Packuments are name-addressed: a stored one
  decides which tarball a later install resolves. Letting a job put one in is the
  poisoning case, and it waits for strict mode (§3).

### 6. Config

```toml
[mirror]
max_size_gib = 20        # default; one cap across ecosystems (§8)

[mirror.lfs]
fill = "relaxed"         # or "off"
warm_on_push = false     # needs git_mirror (§4)

[mirror.npm]
fill = "relaxed"         # or "off"

[[profile]]
mirror = true            # docker profiles only
```

- **Per profile, off by default.** `mirror = true` turns it on for every repo the
  profile serves. Absent or `false` means no attachment and no env, which is
  today's behaviour. It turns a feature on rather than tightening anything, so
  AGENTS.md's three-state shape is not needed.
- **Docker profiles only.** A native profile with `mirror = true` is refused by
  `faber check-config` and the settings save, as `git_mirror` is.
- **Restart-applied**, like `git_mirror` and `cache_volume`.
- **A job opts out** by unsetting the variables in its own `env:`.
- `strict` is a reserved future value of `fill`, and `upload` a future key; neither
  exists in v1.

### 7. Failure: a mirror that is not healthy is not used

- **A health gate.** faber injects the env and attaches the alias only if the
  mirror passed a health check recently. A job admitted while it is down runs as it
  does today, straight to upstream.
- **Local faults degrade, they do not fail.** A full cache, a store error or a
  failed verification makes the service pass the request through without storing.
- **Revival.** The container runs under a restart policy, and the janitor revives
  it if it is gone, in the way ADR-0007 treats faber's own resources.
- **A crash mid-request may fail that job.** That is accepted in v1. Such an
  outcome is recorded with `cause: mirror` in the audit log.
- **Not auto-retried in v1.** ADR-0003's trigger is an abnormal runner ending in
  faber's own record. A mirror fault fails a step while the runner ends normally,
  so it does not qualify. Extending ADR-0003 to `cause: mirror` is a possible
  future amendment, not part of this one.

### 8. Disk

- **One cap across ecosystems,** `[mirror] max_size_gib`, evicted least recently
  used.
- **It yields to the host's disk floor.** While the host's free space is below
  ADR-0024's `disk_admission_floor_gib` plus 5 GiB, the mirror stops storing and
  evicts toward 80% of its cap.
- **Eviction never breaks a read.** An object being served stays readable until
  its response ends.
- **No per-repo quotas.** The dashboard shows usage per ecosystem and per repo.
- The volume is counted by ADR-0024's gate like every other volume, and is
  reclaimable: deleting it costs only cold misses.

### 9. Incidents are attention rows and audit records

- **Attention rows:**
  - bad, **"Mirror down"**, with the jobs it affected;
  - warn, **"Mirror passing through"**, with the reason (cap reached, disk floor,
    store error);
  - the missing image (§2).
- **Audit:** one `fault` record per incident, naming the affected jobs.
- **Nothing is posted to GitHub runs.** A run keeps showing CI's verdict as GitHub
  reported it (ADR-0044); faber's own faults stay in faber's surfaces.
- Delivering these rows outward, as a notification or an issue, is not mirror
  specific. It is decided separately, in `attention-rows-can-be-delivered-outward`.

### 10. Rollout and validation

The first watched repo, which has LFS-backed visual tests and npm installs, is the
first real test. The slices are:

1. **Skeleton:** the service, its image build, the container, the health gate, the
   network attachment and the attention rows, with no ecosystem behind them.
2. **LFS fill** (§4, without warming).
3. **Warming** (`warm_on_push`).
4. **npm** (§5).

Each is enabled on that repo before the next, starting with `git_mirror`, and
measured against a baseline taken before it.

## Validation

Filled in as each stage goes live on the first watched repo. The ADR stays
`proposed` until every row has a measurement.

Measured with [`scripts/mirror-impact.py`](../../scripts/mirror-impact.py)
(read-only `gh` GETs; per-host step seconds from the jobs API).

| Stage | Metric | Baseline | With the mirror | Date |
|---|---|---|---|---|
| `git_mirror` on (ADR-0035) | checkout bytes per job | not measurable: CI logs print no transfer sizes (git shows no progress without a tty) | not measurable, as baseline | 2026-10-22 |
| `git_mirror` on (ADR-0035) | checkout seconds per job | mirror host: median about 20 s, p90 about 50 s (about 15 jobs); peer host: about 30 s / 50 s (about 50 jobs); week of 2026-10-14..21 | mirror host: median about 4 s, p90 about 13 s (about 60 jobs); peer host, no mirror, the control: about 30 s / 45 s (about 200 jobs); 24 h from 2026-10-21, when the first watched repo began borrowing the mirror, about 120 of 150 runs sampled; the mirror host's slow tail is a handful of jobs whose checkout wiped the borrowed workspace and fetched in full (about 20 to 100 s) | 2026-10-22 |
| LFS fill | LFS bytes from GitHub per visual shard | | | |
| LFS fill | LFS seconds per visual shard | | | |
| LFS fill | mirror counters per repo (`/api/state` → `mirror.lfs.repos`): hits, misses, bytes from cache, bytes from GitHub | | | |
| Warming | LFS bytes from GitHub per visual shard, first run after a push | | | |
| Warming | LFS seconds per visual shard, first run after a push | | | |
| npm | install seconds on a `node_modules` miss | | | |
| npm | bytes from the public registry on a `node_modules` miss | | | |

## Consequences

- **No workflow change.** An opted-in profile's jobs use the mirror through env
  alone. That is the point of the proxy shape, and it is also its cost: the effect
  is invisible in the workflow, so the dashboard and the audit log have to say when
  a job went through the mirror.
- **LFS bandwidth leaves GitHub once per object per host,** not once per shard.
  With warming, most visual shards should see no LFS download from GitHub at all.
- **faber's token gains Contents: read for LFS** (ADR-0027's table). It is not
  read-only: it already carries Administration: write, and with `[mirror.lfs]` on
  it sits in the `faber-mirror-auth` volume on the Docker engine for as long as the
  mirror runs. LFS writes keep the job's own token. A dind job is root on that
  engine (ADR-0026), so `[mirror.lfs]` is refused beside dind until a host-side
  broker keeps the token out of every VM
  ([docs/git-mirror.md](../git-mirror.md#not-beside-dind)).
- **One more faber-owned container and volume per host.** The container is a new
  long-lived process in the VM, with its own memory, which the machine share
  (ADR-0021) has to account for.
- **The env overrides what a repo configures.** `GIT_CONFIG_*` entries take
  precedence over a repo's own `.lfsconfig`, and the npm variable over a project's
  default `registry`. A repo that pins its own LFS server or default registry and
  wants to keep it must opt its jobs out.
- **An npm token written for the public registry does not follow the
  redirect.** npm sends a token only to the host it names, so a job that
  installs private packages from npmjs, or publishes, must opt out of npm.
- **Jobs that run inside `container:` or under dind do not get it in v1.** The env
  reaches the runner process, and the alias resolves on the job's network; steps in
  another container or another daemon see neither. They run as they do today.
- **A mirror crash can fail a job.** Rare, recorded with `cause: mirror`, and not
  retried in v1.
- **Strict mode is still owed** before any upload or name-addressed write is
  allowed, and it needs OIDC verification in the service.

## Alternatives considered

**Store-shaped LFS through alternates.** git-lfs also looks for objects in the LFS
store of a repository's git alternates. Tested offline with git-lfs 3.7.1, a
workspace was served every object from a read-only shared store. It works, but like
ADR-0035 it needs a workflow step, and it does nothing for npm. The proxy shape
covers both with no workflow change.

**Write-through upload.** Let pushes go to the mirror, which forwards them to
GitHub. Rejected: faber would need a write credential for every repo's LFS, and
the mirror would hold content a job chose.

**An authoritative LFS server.** Make the mirror the LFS server and GitHub a
backup. Rejected: it moves the source of truth onto one host, and a host lost is
then data lost.

**Off-the-shelf caches (Verdaccio for npm, `registry:2` for Docker images).**
Rejected for v1. Each is another runtime with its own config, auth and storage
model, none of them handles LFS, and none enforces the per-repo index of §4. They
may sit behind the same front door later, for ecosystems where they fit.

**Hosted by the daemon itself.** Rejected. The daemon runs on the macOS side, as
the hidden `_faber` account (ADR-0025), and jobs run inside faber's VMs
(ADR-0026). Serving jobs from the daemon would open a path through the VM wall for
bulk traffic, and put a network service in the account that holds faber's
credentials. A container on the runners' own engine needs neither.
