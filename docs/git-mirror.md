# Git mirror: jobs borrow git objects from a store only faber writes

[ADR-0035](adrs/0035-jobs-borrow-git-objects-from-a-mirror-only-faber-writes.md)
decides this; this page is how to use it.

Every job's `actions/checkout` downloads objects the host fetched minutes earlier
for another job. With the git mirror on, faber keeps one bare repository per
watched repo, fetches it from GitHub itself, and mounts it **read-only** into
that repo's runner containers. A short workflow step points the workspace at it
before checkout, and checkout then fetches only what the mirror lacks: in the
ADR's measurement, a depth-1 fetch went from over 10 MiB and a few seconds
to nothing and under a second, and `fetch-depth: 0` became about as cheap.

## Turn it on

Per docker profile, in `config.toml`:

```toml
[[profile]]
name = "linux-docker"
labels = ["faber", "docker"]
backend = "docker"
image = "ghcr.io/owner/my-runner:latest"
git_mirror = true
```

- **Absent or `false`** is off: no mirror, no mount, no variable. A config that
  never sets the key is unchanged by the release that added it, and the settings
  page does not write `false` back.
- **Docker profiles only.** `faber check-config` and the settings save refuse it
  on any other profile; a daemon that boots on one anyway warns and ignores it.
- **Restart-applied**, like `cache_volume`.
- **Not beside dind.** `faber check-config` and the settings save refuse it while
  any docker profile grants dind, and a daemon that boots on that anyway leaves the
  mirror off. See [Not beside dind](#not-beside-dind).
- **The token needs Contents: read** on the watched repos while the key is on
  ([ADR-0027](adrs/0027-github-access-is-an-explicit-fine-grained-grant.md)'s
  table), and **Pull requests: read**, which listing the open PRs for the refspec
  needs. `faber auth link` asks for both when a profile turns the mirror on. faber
  fetches with the token it already holds.

With the key on, faber mirrors every watched repo (the repos carrying the
discovery topic), not only the repos whose jobs reach a `git_mirror` profile.
That follows from how faber routes: a profile is chosen by a job's `runs-on`
labels, not by repo, so every profile can serve every watched repo. The cost is
one first clone per watched repo, counted against the disk floor as below. The first refresh runs at boot, then every 15 minutes, and
whenever a poll sees a new `push` or `pull_request` run. Until a repo's first
refresh has finished, its jobs check out as before.

## Add the step to the workflow

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

**`clean: false` is required.** Without it, `actions/checkout` runs
`git clean -ffdx` and `git reset --hard HEAD` on the directory the step made, the
reset fails on its unborn `HEAD`, and checkout deletes the directory, alternates
included. It then fetches everything, and nothing reports an error: the job is
simply as slow as before. On a faber runner the workspace is new for every job, so
there is nothing for `clean` to clean.

The rest of the rules:

- **The remote URL must be exactly what checkout computes**,
  `https://github.com/<owner>/<repo>` with no `.git`. The step above builds it
  from the same variables checkout uses. A mismatch also makes checkout delete
  the directory.
- **With checkout's `path:` input**, write to that path instead of
  `$GITHUB_WORKSPACE`.
- **`fetch-depth` stays your choice.** With the mirror, `fetch-depth: 0` costs
  about what depth 1 does.
- **Before copying `.git` somewhere the mirror is not mounted** (a Docker build
  context that includes `.git`, a tarball of the workspace), make the repository
  standalone: `git repack -a -d && rm .git/objects/info/alternates`. In the
  ADR's measurement it took a fraction of a second.
- **Jobs with a `container:` get no benefit yet**: the runner does not pass the
  mount into the job container, and the step's first line makes it a no-op there.
- **On a GitHub-hosted runner, or a host with the key off**, `FABER_GIT_MIRROR` is
  unset, the step exits at its first line, and `clean: false` on a fresh
  workspace changes nothing.

## What a job sees

| | |
|---|---|
| Mount | the repo's mirror at `/runner/_git`, read-only, beside `/runner/_cache` and outside `_work` |
| Variable | `FABER_GIT_MIRROR=/runner/_git`, set only when the mirror is mounted, and after the profile's `env` |
| Not mounted when | the first refresh has not finished, faber is pruning or deleting the mirror, or the volume under the mirror's name does not carry faber's `git-mirror` labels (see below). The job then checks out as before |
| Blocked when | a volume named `faber-git-<owner>-<repo>` exists without faber's `git-mirror` labels, for instance one Docker created empty at a job's mount after the mirror was deleted by hand. faber did not label it, so it never uses, writes or deletes it: the mirror stays blocked, and the log and dashboard (`blocked` in `/api/state.git_mirrors`) name the volume. Remove that volume by hand or rename it; the next refresh then builds the mirror |
| If the launch fails | a runner launch the daemon refuses while the mirror is mounted is retried once without the mount and without the variable, after removing anything the refused launch created. The dashboard shows why, and the mirror is not mounted again until a refresh has looked at it. A launch that timed out is not retried, because its runner may already be up |

## Trust model

faber is the only writer: a short-lived container of the profile's runner image
fetches into the volume with faber's token, which reaches it as a read-only
`0600` file in a volume of its own, written from a short-lived container's
stdin and removed after the refresh, never an argument, a variable or a host
file; jobs get the volume `:ro`, so no
uid, `sudo` or `chmod` inside a job can change what the next job checks out. Nor
can anything planted in the volume steer a fetch that holds the token: before git
runs, every refresh, repack and prune rewrites the repository's `config` from a
template and deletes its `hooks/`, `commondir` and alternates file, and git reads
no system or global config, runs no hook and speaks only https. The credential
helper answers only for `https://github.com`, so a URL rewritten to any other host
gets nothing. A
refresh fetches the repo's branches and tags and the heads of open PRs whose
head repo is the repo itself, read from the REST API, so fork PRs' code is never
stored on the host, not even as inert objects; merge refs are never fetched.
`fetch.fsckObjects` refuses malformed objects at the door. Git looks up a
borrowed object by the hash of its content, so an object in the mirror can only
ever satisfy a request for exactly that content: a malicious same-repo branch
(whose author already has write access to the repo) can make the mirror larger,
never change what another ref checks out. Mirrors are per repo and mounted only
into that repo's runners, which are minted for that repo alone. faber deletes a
repo's cache volume after an enforced gate bypass, but not its mirror: the job
could not write it.

## Not beside dind

The git mirror and `[mirror.lfs]` ([mirrors.md](mirrors.md#git-lfs)) both put
faber's GitHub token on the Docker engine that runs jobs: a refresh mounts it into
a short-lived container there, and the LFS mirror keeps it in a volume for as long
as it runs. It is faber's one fine-grained token, so it carries Administration:
write on every watched repo, which also covers deleting a repository
([ADR-0027](adrs/0027-github-access-is-an-explicit-fine-grained-grant.md)). A job
given dind is root on that engine
([ADR-0026](adrs/0026-job-code-runs-only-inside-vms-faber-owns.md)): it can read
any volume and any container there, and plant a `config` or a hook in a mirror for
the next refresh to obey. The owner decided that no GitHub credential may enter a
VM. A broker on the host, in an ADR of its own, will fetch for the mirrors; until
it does, faber keeps both away from dind:

- **Refused where you are looking.** `faber check-config` and the settings save
  refuse `git_mirror = true` or `[mirror.lfs]` while any docker profile grants
  dind: `dind = true`, or no `dind` key at all, which still grants
  ([ADR-0013](adrs/0013-docker-in-docker-is-a-grant-not-a-default.md)).
- **Off, and said so, at boot.** A daemon that boots on such a file starts anyway
  (a release never refuses to boot on a config it did not write). It leaves both
  mirrors off, and says so in a warning at every boot and in a notice on the
  dashboard. `[mirror.npm]` carries no token and keeps running.
- **The engine is remembered.** The first boot that sees a profile grant dind
  writes a marker, `engine-dind.json`, in faber's state directory, which no job can
  reach. While it is there, both mirrors stay off, even once every profile says
  `dind = false`: a job that was root on the engine may have left something there
  that would read the token. A host that has only ever denied dind is never marked.
  A grant from before faber kept the marker left none, so you create it yourself:
  see [Grants from before the marker](#grants-from-before-the-marker).
- **What the token touched is deleted.** While the mirrors are off, faber deletes
  every git mirror (`org.faber.kind=git-mirror`), every leftover refresh
  credential (`git-auth`) and `faber-mirror-auth` (`mirror-auth`), found by those
  labels and never by name, each once no container mounts it. A mirror is a cache:
  turned back on, it clones again.

### Grants from before the marker

The marker came with the refusal: the first release whose `faber check-config`
refuses a mirror beside dind is the first that writes it. An engine that granted
dind under an earlier release has none, and the update to that release can hide
it. `faber check-config`, which `sudo faber setup` and
[manual-update.md](manual-update.md) § 3 both run, refuses a mirror beside a
grant, and setting `dind = false` gets the config past it. The new daemon's first
boot then sees no grant and writes no marker. The mirror starts on an engine
where dind jobs were root until the update.

So if any docker profile on this host has ever granted dind (one with no `dind`
key did), create the marker before the new daemon first starts. If you cannot
tell, create it. On a Mac set up with `sudo faber setup`:

```sh
sudo -u _faber touch "/Library/Application Support/faber/engine-dind.json"
```

On any other host it goes in faber's state directory. Only the file's presence
counts, so an empty one marks the engine as surely as one faber wrote.

### If a mirror ran beside dind

This is for a host where either mirror has run on an engine that granted dind,
whether the grant came before the mirror or beside it. The token sat where a
root job could read it, so treat it as read.

1. **Let this release delete what the token touched.** Once this release runs,
   both mirrors are off and faber deletes the volumes listed above. *Check:* on a
   Mac set up with `sudo faber setup`, a few minutes after the daemon starts
   (the engine must be in service first), this prints nothing. A volume that
   stays listed means the mirrors are still on: the engine has no marker.

   ```sh
   sudo -H -u _faber sh -c 'for kind in git-mirror git-auth mirror-auth; do
     DOCKER_HOST=unix:///var/db/faber-vm/run/docker.sock /usr/local/libexec/faber/docker \
       volume ls -q --filter label=org.faber.owned=true --filter "label=org.faber.kind=$kind"
   done'
   ```

2. **Then regenerate the token.** Use GitHub's "Regenerate token", which keeps the
   repositories and permissions and makes the old value worthless, then run `sudo
   faber auth set`. Not before step 1: a release from before this one would put
   the new token on the same engine at its next refresh.
3. **Then review what the old token could have changed.** A new value undoes
   nothing done with the old one. Administration: write lets its holder register
   self-hosted runners, add deploy keys and collaborators, and rewrite the rules
   that protect a repository
   ([ADR-0027](adrs/0027-github-access-is-an-explicit-fine-grained-grant.md)).
   On each repository the old token reached, review its:
   - self-hosted runners;
   - deploy keys;
   - collaborators and pending invitations;
   - rulesets and branch protection;
   - environments;
   - Actions settings;
   - visibility.

   Where auto-retry gave the token Actions: write, also look for workflow runs
   nobody started. Where the repositories belong to an organization, read its
   audit log for the time the mirror ran.

### Turning them back on

It takes an engine that has never granted dind. On a Mac that is a fresh Linux
VM disk. Moving to one is an operator step, because the old disk holds the only
copy of the local-only runner image (AGENTS.md). Then set `dind = false` on every
docker profile and delete the marker as root. The boot warning names its full
path; on a Mac set up with `sudo faber setup`:

```sh
sudo rm "/Library/Application Support/faber/engine-dind.json"
```

Deleting the marker without a fresh engine turns the mirrors back on in one that
root jobs have had. That is the operator's call, not faber's.

## Disk and maintenance

- **Counted by the disk gate as it is.** The mirror lives on the Docker volume
  ADR-0024's gate measures, so its bytes are used space there. Jobs never write
  it, so ADR-0032's per-job charge has nothing to add.
- **The first clone respects both disk floors, one at a time.** It waits while
  free space, less the repo's size as GitHub reports it, would fall below the
  higher of `[host] disk_admission_floor_gib` and `[host] disk_floor_gib`, and
  tries again on the next timer. Below the reclaim floor the clone would only
  start a reclamation that deletes it again (ADR-0035's 2026-11-01 amendment).
  First clones run one at a time, so each weighs the disk the last one left.
  With both floors unset or `0` it builds.
- **Routine maintenance never deletes an object.** After a refresh that left
  enough loose objects or packs behind, faber runs
  `git repack -a -d --keep-unreachable`. `git gc --prune=now` runs at most daily,
  from the janitor, and only while no job holds the mirror and no container
  mounts its volume.
- **Reclaimable.** Under disk pressure (`[host] disk_floor_gib`) faber deletes
  every mirror nothing mounts, after expired cache entries and before runner-image
  variants and anything the operator declared recreatable. The next jobs check
  out cold until a refresh rebuilds it. A mirror whose repo no `git_mirror`
  profile serves any more (the key turned off, or the repo left the topic) is
  deleted at the next janitor pass, once unmounted.
- **Labelled, never named.** Each mirror is a volume labelled
  `org.faber.kind=git-mirror` and `org.faber.repo=<owner/repo>`. The janitor's
  per-job sweep never reaps it.

## Watching it

The dashboard shows a line per mirror: its size, its last successful refresh,
whether a refresh is running, and the last refresh error. `/api/state` carries
the same under `git_mirrors` (an additive field):

```json
"git_mirrors": [
  {"repo": "owner/demo", "ready": true, "refreshing": false, "size_kib": 87040,
   "last_refresh_ms": 1792223000000, "last_attempt_ms": 1792223000000,
   "last_error": null}
]
```

A failed refresh leaves the previous mirror in place. A stale mirror is still
correct, only less useful, and nothing about it fails a job.
