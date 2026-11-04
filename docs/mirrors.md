# Mirrors: jobs fetch through a cache only faber fills

[ADR-0047](adrs/0047-jobs-fetch-through-a-mirror-only-faber-fills.md) decides
this; this page is how to run it.

Jobs download the same things again and again: LFS objects, npm packages, and
later more. A **pull-through mirror** answers those requests from a cache on the
host and fetches from upstream only what it lacks. faber runs one mirror per
host, as a container it owns on the Docker engine the runners use, and points
opted-in jobs at it.

Its store-shaped sibling is the [git mirror](git-mirror.md): a bare repository
faber fetches itself and mounts read-only. This one is a service jobs talk to
over their own network instead.

**Status: Git LFS and npm.** The service runs, is health-checked, joins jobs'
networks and is advertised to them as `$FABER_MIRROR`. Two ecosystems ship: Git
LFS, filled on a miss ([LFS](#git-lfs) below; ADR-0047 §4 without warming), and
the public npm registry ([npm](#npm) below; ADR-0047 §5). Warming LFS on push is
a later slice.

## Turn it on

Two keys, both in `config.toml`, both applied on restart:

```toml
[mirror]
max_size_gib = 20      # the cache volume's limit; default 20, must be above 0

[[profile]]
name = "linux-docker"
labels = ["faber", "docker"]
backend = "docker"
image = "ghcr.io/owner/my-runner:latest"
mirror = true
```

- **`[mirror]` is the switch.** Absent, the feature is off completely: faber
  checks no image, starts no container and makes no Docker call for it. A config
  that never had the section is unchanged by the release that added it.
- **`mirror = true` opts a profile's jobs in.** Absent or `false` is off, and the
  settings page does not write `false` back. Docker profiles only:
  `faber check-config` and the settings save refuse it on a native or macOS VM
  profile, and a daemon that boots on one anyway warns and ignores it.
- **A profile opted in with no `[mirror]` section** only warns at boot. Removing
  the section alone is how to turn the mirror off without editing every profile.
- **`[mirror]` is edited by hand**, not on the settings page: a form field for it
  would create the section, and so turn the feature on, the first time any
  settings form was saved. Saving the settings page keeps the section as it is.
- **One table per ecosystem: `[mirror.lfs]`** ([below](#git-lfs)) **and
  `[mirror.npm]`** ([below](#npm)). Each turns its ecosystem on; neither does
  anything without `[mirror]`.
- **An invalid `[mirror]`** (`max_size_gib = 0`) is refused by `check-config` and
  the settings save. A daemon that boots on one warns and keeps the mirror off.

## Build the image

faber runs the image `faber-mirror:<faber version>`, the version in the
checkout's `Cargo.toml`, and never pulls it: `--pull=never`, and nothing faber
builds is published anywhere. Build it from the same checkout as the faber you
run:

```sh
deploy/build-mirror-image.sh               # into the engine `docker` talks to
deploy/build-mirror-image.sh --print-tag   # the tag this checkout produces
```

The build is two stages, `rust:1-trixie` and then `debian:trixie-slim`, with the
service running as uid 65532 (`crates/faber-mirror/Dockerfile`).

On a set-up macOS host the runners' engine is faber's own Linux VM, which only
`_faber` may reach. Build anywhere you have Docker and load the result through
the VM's socket:

```sh
deploy/build-mirror-image.sh --save /tmp/faber-mirror.tar
sudo -u _faber sh -c 'DOCKER_HOST=unix:///var/db/faber-vm/run/docker.sock \
  docker load -i /tmp/faber-mirror.tar'
```

Rebuild it after every faber update: a new faber looks for its own version's tag,
reports **Mirror image missing** until it is there, and replaces the old
version's container once it is. faber compares image IDs, not tags, so a rebuild
under the same tag (two builds of one version between releases) also replaces
the running container at the next janitor pass.

## What faber runs

| | |
|---|---|
| Container | `faber-mirror`, labelled `org.faber.owned=true` and `org.faber.kind=mirror`, `--restart unless-stopped`, `--memory 256m`, on the default bridge for its own upstream traffic |
| Cache | the named volume `faber-mirror-cache`, same labels, mounted at `/cache` |
| Port | 7470, inside the container only. Nothing is published to the host |
| Health | `GET /healthz` answers `{"version", "status": "ok" \| "passing_through", "reason"?}`. faber reads it every 15 s with `docker exec faber-mirror faber-mirror health`, so it needs no published port |

- **Adopted across restarts.** A faber restart keeps the running container
  (ADR-0011's pattern). faber reads its labels, never only its name: a container
  called `faber-mirror` without `org.faber.kind=mirror` is not faber's, so faber
  leaves it alone and reports the mirror down.
- **Revived by its janitor.** A container that is missing, stopped or running
  another image than the tag now names is replaced, and reconnected to the job
  networks it was on, except those whose jobs have ended meanwhile. No job is
  connected or handed the variables while a replace runs. The cache volume is
  kept.
- **Never swept.** `mirror` is not one of the janitor's per-job kinds, so neither
  the container nor its volume is reaped.
- **Retired when `[mirror]` is removed.** At boot, a daemon without the section
  lists containers carrying `org.faber.owned=true` and `org.faber.kind=mirror`.
  It disconnects them from job networks as those jobs end, the jobs adopted
  across the restart included, and removes each once no job network holds it.
  An unlabelled container is never listed, so never touched. The cache volume is
  kept: it is only a cache, and a mirror turned back on starts warm.

## What a job sees

A runner on an opted-in profile is pointed at the mirror only when the mirror's
latest health reading succeeded and is under 60 seconds old. A failed reading
stops new jobs from being pointed at it at once, though it is reported down only
after 60 seconds. Then faber:

1. connects the mirror to the job's private network with
   `docker network connect --alias faber-mirror`, and
2. adds `FABER_MIRROR=http://faber-mirror:7470` to the runner, after the
   profile's own `env`, so faber's value wins.

Otherwise it injects nothing and the job runs as it would without the mirror: a
mirror that is missing, down or still starting, or a connect that fails, costs
the job nothing. Teardown disconnects the mirror before the network is removed,
and the janitor does the same for a network a crash left behind.

Each ecosystem's redirect is built per container, from the runner's repo. LFS's
also carries a token minted for that container (ADR-0047 §4). A variable that
carries the token is secret: like the JIT config, it is on the `docker run` argv
by name only, and its value travels in that one CLI child's environment, never
in an argument, a label or a log. With `[mirror.lfs]` on, that is the LFS
redirect ([below](#what-a-job-gets)); with `[mirror.npm]` on, the registry
variables ([below](#what-a-job-gets-1)), which carry no token. With neither, the
runner gets `FABER_MIRROR` alone.

The mirror is reached only on each job's own network, by alias. There is no
network shared between runners.

## Git LFS

### Turn it on

```toml
[mirror]
max_size_gib = 20

[mirror.lfs]
fill = "relaxed"       # the default; or "off"
```

- **`[mirror.lfs]` turns LFS on** for every profile with `mirror = true`. Absent,
  LFS is not redirected and the mirror behaves as before. Restart-applied.
- **`fill = "relaxed"`**: any opted-in job's miss makes the mirror fetch the
  object from GitHub, verify it and keep it. **`fill = "off"`**: objects already
  kept are served, and a miss is fetched and passed through without being kept.
- **Refused, by `check-config`, the settings save and the parser:**
  `fill = "strict"` (reserved, ADR-0047 §3), `warm_on_push` (a later slice), any
  other key, and `[mirror.lfs]` with no `[mirror]` written out. TOML would create
  `[mirror]` implicitly, which would turn the mirror on by accident; write the
  `[mirror]` line. A daemon that boots on that last one warns and keeps the
  mirror off.
- **faber's token needs Contents: read** on each watched repo whose LFS it
  fetches (ADR-0027's table). The mirror only reads, and pushes never touch it,
  but the token it reads with is not read-only: it is faber's one token, with
  Administration: write on every watched repo.
- **Not beside dind.** That token sits on the Docker engine for as long as the
  mirror runs, and a dind job is root there. So `check-config` and the settings
  save refuse `[mirror.lfs]` while any docker profile grants dind, and a daemon
  that boots on that anyway runs the mirror without LFS. npm is unaffected. See
  [git-mirror.md](git-mirror.md#not-beside-dind), which also says what to do on
  a host where it ran beside dind.

### What a job gets

On a runner pointed at the mirror, with `[mirror.lfs]` on, faber mints a random
token for that one container and adds, as **secret** variables (the name on the
`docker run` argv, the value only in that one CLI child's environment):

| Variable | Value |
|---|---|
| `GIT_CONFIG_COUNT` | `n + 2` |
| `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n` | `lfs.url` = `http://faber-mirror:7470/lfs/<token>/<owner>/<repo>/info/lfs` |
| `GIT_CONFIG_KEY_n+1` / `GIT_CONFIG_VALUE_n+1` | `lfs.pushurl` = `https://github.com/<owner>/<repo>.git/info/lfs` |

`n` is 0 unless the profile's own `env` sets `GIT_CONFIG_COUNT`. Then faber
**appends**: the profile's entries `0..n` stay where they are, faber's take `n`
and `n + 1`, and its count, which follows the profile's on the argv, wins. A
`GIT_CONFIG_COUNT` that is not a whole number up to 1000 is refused for that job
with a warning naming it, and the job runs without the LFS redirect. faber reads
the profile's `env`, not the image's own `ENV`: a count baked into the image is
overwritten, so set it in the profile instead.

- **Downloads go through the mirror; pushes go to GitHub** with the job's own
  credentials (the ones `actions/checkout` persisted). An upload sent to the
  mirror is refused with a message naming `lfs.pushurl`.
- **The token speaks for the runner's repo only.** An unknown token, or one used
  on another repo's path, is `403`. It is revoked at teardown.
- **Opting a job out**: unset the `GIT_CONFIG_*` variables in its `env:`, or set
  `GIT_CONFIG_COUNT: 0`. git's environment config also overrides a repo's
  `.lfsconfig`, so a repo that pins its own LFS server opts out the same way.
- Steps in a `container:` or under dind do not see the variables (ADR-0047,
  Consequences). They fetch from GitHub as before.

### How a request is served

1. **Hit**: the object is stored and the repo's index names it. It streams from
   the cache volume.
2. **Miss**: faber's read-only token asks GitHub's LFS batch API for it **as the
   job's repo**. If the bytes are already stored for another repo, GitHub's yes
   adds them to this repo's index and they are served with no download. If not,
   they are downloaded to a temp file and kept only when their sha256 is the oid;
   otherwise the job gets `502` and nothing is kept. A job cannot read another
   repo's objects by guessing a hash: the index is per repo, and only GitHub adds
   to it.
3. **Passing through**: with `fill = "off"`, an unwritable store, or the host's
   free disk below `[host] disk_admission_floor_gib` + 5 GiB (5 GiB with no
   floor set), a miss is fetched and streamed without being kept.

The cache holds at most `max_size_gib` and evicts least recently used. A fill's
temp file counts toward the cap from the moment it starts: room is made for it
first, or the request passes through and writes nothing. While the host is short
of disk the mirror evicts toward 80% of the cap, counting fills in flight, and a
fill already running stops and passes through. An object being streamed stays
readable until its response ends, eviction or not.

**Sizes come from GitHub, never the job.** GitHub answers a download only for
the object's true size, and that answer's size is what the mirror holds every
download to: a fill, or a pass-through, is cut off past it. A download offered
with no size is refused. No object larger than 5 GiB is stored, whatever GitHub
says. A `GET` with no recent batch answer behind it is `404`, so git-lfs asks
the batch API again; the links the mirror hands out expire after 5 minutes, as
the batch answers it remembers do.

**What the mirror follows.** The mirror sits on every opted-in job's network, so
it follows only GitHub's LFS download links: https, on port 443, to
`github-cloud.githubusercontent.com` or `github-cloud.s3.amazonaws.com` (the
hosts GitHub documents for LFS), with at most 3 redirects, each checked again.
An address that is loopback, private, link-local or otherwise not public is
refused, even when an allowed name resolves to it. A refused link is `502` to the
job, and nothing is fetched.

### How the token reaches the mirror

No port is published and no token is ever on an argv. The daemon runs `docker
exec -i faber-mirror faber-mirror sync` with the whole table (one entry per
container: its job network, its repo, its token) on stdin. That process hands it
to the service's control port, bound to `127.0.0.1` inside the container, which
no job network can reach. The janitor syncs every 15 s, so a mirror that
restarted gets its table back within one pass; a launch syncs before the runner
starts, and teardown syncs to revoke. Tokens live in the service's memory only.

faber's own GitHub token reaches the mirror the way it reaches the git mirror's
refresh container: a `0600` git-credential file in the volume
`faber-mirror-auth` (labelled `org.faber.kind=mirror-auth`), filled from a short
container's stdin and mounted read-only. The container carries a fingerprint of
the token as a label, so a rotated token replaces it at the next janitor pass.
Mounted read-only is not the same as out of reach: the volume is on the engine
that runs jobs, which is why LFS is off beside dind. When it goes off, the next
pass replaces the container with one that has no credential, and faber deletes
`faber-mirror-auth` by its label.

### What to expect: the counters

CI logs carry no byte counts, so the mirror counts for you, per repo, since the
service last started:

| Counter | Means |
|---|---|
| `hits` | objects served from the cache |
| `misses` | objects fetched from GitHub, kept or passed through |
| `bytes_from_cache` | bytes served from the cache |
| `bytes_from_upstream` | bytes fetched from GitHub |
| `passed_through` | misses served without being kept |

They are in `/api/state` → `mirror.lfs.repos`, with LFS's share of the store
(`objects`, `bytes`) and the store's one cap (`max_bytes`). The dashboard shows them in its quiet **Mirror** section
(`ui/src/blocks/MirrorSection.tsx`, in the Now group, ADR-0041), never among the
notices: a per-repo table and the store against its cap. The React dashboard is still
its placeholder (ADR-0042), so the section is built and storied, and drawn once the
dashboard's screens move there; until then read `/api/state`. The counters
reset when the service restarts, so compare a baseline and a trial window from
the same run of it. Do not run `faber-mirror sync` by hand to read them: a sync
replaces the token table, and an empty one revokes every running job's token.

### Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| `git lfs` gets `403 … not valid` | the token was revoked (the job outlived its teardown) or the mirror restarted and the 15 s resync has not run yet | re-run the job; a restart also marks running jobs as affected |
| `403 … not for this repository` | a job used another repo's `lfs.url` | it may not; unset the variables for that step |
| `404` for an object | GitHub says the repo does not have it (never pushed, or faber's token lacks Contents: read) | check the object exists on GitHub and the token's grant |
| `502 … hash to` | GitHub sent bytes that are not the object; nothing was kept | re-run; report it if it repeats |
| a warning `GIT_CONFIG_COUNT=… is not a count` | the profile's env sets a count faber cannot append to | fix the profile's value |
| **Mirror passing through** row, reason "host disk is below…" | the host is short of disk | free disk; the mirror resumes storing on its own |
| counters stay at 0 | the job's checkout does not fetch LFS, or it unsets the variables | `git lfs env` in the job shows the endpoint it uses |

### Failures are recorded

A request the mirror itself fails (a store error, its credential unreadable) is
counted against that container. Each such incident writes one audit `fault`
line, scope `mirror`, action `request_failed`, naming the runner. A job whose
outcome is a failure after such an incident, or that was attached while the
mirror went down or restarted, gets **`cause: "mirror"`** on its `outcome`
record. Failures from GitHub (`404`, its `5xx`) are not the mirror's and are
never counted.

## npm

### Turn it on

```toml
[mirror]
max_size_gib = 20

[mirror.npm]
fill = "relaxed"       # the default; or "off"
```

- **`[mirror.npm]` turns npm on** for every profile with `mirror = true`. Absent,
  the default registry is not redirected and the mirror behaves as before.
  Restart-applied.
- **`fill = "relaxed"`**: any opted-in job's miss makes the mirror fetch the
  tarball from the registry, verify it and keep it, and packuments are cached
  for 5 minutes. **`fill = "off"`**: tarballs already kept are served; a miss is
  fetched and passed through without being kept, and packuments are fetched
  every time.
- **Refused, by `check-config`, the settings save and the parser:**
  `fill = "strict"` (reserved, ADR-0047 §3), any other key (`upload`, a
  `registry`: the upstream is `https://registry.npmjs.org` and nothing else), and
  `[mirror.npm]` with no `[mirror]` written out, as for LFS. A daemon that boots
  on that last one warns and keeps the mirror off.
- **No GitHub permission.** The npm adapter never uses faber's token.
- **The settings page leaves it alone**, as it does all of `[mirror]`: a save
  neither creates the table nor removes it, nor changes its `fill`.

### What a job gets

On a runner pointed at the mirror, with `[mirror.npm]` on, faber adds, as plain
variables after the profile's own `env` (so faber's values win):

| Variable | Value | Read by |
|---|---|---|
| `NPM_CONFIG_REGISTRY` | `http://faber-mirror:7470/npm/` | npm, pnpm, bun, yarn classic |
| `YARN_NPM_REGISTRY_SERVER` | `http://faber-mirror:7470/npm/` | yarn berry |
| `YARN_UNSAFE_HTTP_WHITELIST` | `faber-mirror` | yarn berry, which refuses a plain-http registry whose host it does not allow |

**No token.** Everything the npm adapter serves is public, and these values are
the same for every job, so no secret can end up in a lockfile or a job log. The
service tells jobs apart by the address a request arrives on instead: it has one
address on each job network it joins, and the daemon's sync names it per
container (`docker container inspect` of the mirror, at every sync). That
attributes the counters and the mirror's own failures to a job; it never decides
what a request may read. A request on an address no sync has named yet, in the
seconds after a replace, is counted as `(unattributed)`.

- **Only the default registry moves.** A scope the project points at its own
  registry (`@scope:registry=…` in its `.npmrc`) is untouched, and so are its
  requests.
- **The variables override a project's own default registry**, as npm reads its
  environment before a project's `.npmrc`. A repo that pins its own default
  registry and wants to keep it opts out.
- **A token for the public registry is not sent to the mirror.** npm sends a
  token only to the registry host it was written for, so a
  `//registry.npmjs.org/:_authToken=…` (what `actions/setup-node` with
  `registry-url` writes) never reaches `faber-mirror`. A job that installs a
  **private package hosted on npmjs** gets a `404`, and one that runs **`npm
  publish`** gets a `401`. Opt such a job out. A token written for the mirror's
  own URL is sent, and the mirror forwards that request uncached.
- **Opting a job out**: set `NPM_CONFIG_REGISTRY` (and, for yarn berry,
  `YARN_NPM_REGISTRY_SERVER`) back to `https://registry.npmjs.org/` in its
  `env:`.
- Steps in a `container:` or under dind do not see the variables (ADR-0047,
  Consequences). They fetch from the registry as before.

### How a request is served

1. **Tarball hit**: the tarball is kept, by the hash its packument published.
   It streams from the cache volume. The path-to-hash is recorded when a tarball
   is kept, and npm never republishes a version, so a hit needs no packument:
   an `npm ci` that asks only for tarballs is served from the cache alone.
2. **Tarball miss**: the mirror reads the tarball's hash from the package's
   abbreviated packument (sha512 from `dist.integrity`; for the oldest packages,
   the sha1 `shasum`), downloads the tarball to a temp file, and keeps it only
   when the hash matches. Otherwise the job gets `502` and nothing is served or
   kept. A tarball with neither hash is passed through, unkept.
3. **Packuments** are name-addressed, so only faber writes one, from the
   registry. The full and the abbreviated (`Accept:
   application/vnd.npm.install-v1+json`) forms are cached apart, in the
   service's memory, for 5 minutes, and served stale when the registry fails
   (a `5xx` or no answer; a `404` is passed on). Each version's `dist.tarball`
   on the registry is rewritten to point at the mirror. A packument over 16 MiB
   (a few full packuments of very prolific packages) is passed through as the
   registry sent it.
4. **Passing through**: with `fill = "off"`, an unwritable store, or the host's
   disk low (as for LFS), a tarball miss is streamed without being kept. When the
   mirror knows its hash it still checks it on the way, holding the last chunk
   back until the whole download matched; on a mismatch the stream ends short of
   its length, so the job never receives a whole wrong file.

**Never cached, passed through as it came:** any request carrying
`Authorization` (a token written for the mirror's URL), any method but `GET`
(`npm publish` is a `PUT`), and every other endpoint: `/-/whoami`, search, the
audit endpoints, a single version's manifest. The registry's answer goes back as
it came, redirects included; the mirror follows none of them for a forwarded
request, so the job's credentials never follow a redirect.

Tarballs share the store, the cap and the eviction with LFS objects (ADR-0047
§8): one `max_size_gib`, least recently used first, one tarball at most 1 GiB.

**What the mirror follows.** Only `https://registry.npmjs.org`: https, port 443,
public addresses, at most 3 redirects, each checked again, and every one of them
on `registry.npmjs.org`. A refused link is `502` to the job.

### Lockfiles

Measured with a fresh cache, each install's other routes out closed. npm 11.17
is covered by the end-to-end test (`crates/faber-mirror/tests/npm_e2e.rs`); the
others were run by hand against the same mirror and fake registry.

| Tool | A lockfile it writes through the mirror | An existing lockfile naming the public registry |
|---|---|---|
| npm 11 | records the mirror: `resolved` is `http://faber-mirror:7470/npm/…` | through the mirror: npm's default `replace-registry-host=npmjs` rewrites `registry.npmjs.org` to the configured registry |
| pnpm 10 | records no URL: `resolution: {integrity: …}` | through the mirror: there is no URL to follow, so the configured registry is used |
| bun 1.3 | records the mirror's URL | through the mirror when the entry's URL is empty, which is how bun writes a default-registry package; an entry naming `https://registry.npmjs.org/…` explicitly is fetched from there, bypassing the mirror |
| yarn classic 1.22 | records the mirror's URL in `resolved` | fetched from the URL as written: a `yarn.lock` naming `registry.yarnpkg.com` bypasses the mirror for tarballs |
| yarn berry 4 | not run here. Per its documentation, `yarn.lock` records `resolution: "<name>@npm:<version>"` and a checksum, with no registry | per the same, through `YARN_NPM_REGISTRY_SERVER`; not verified |

**A job that writes a lockfile and keeps it** (a dependency-update job that
commits `package-lock.json`, say) records the mirror's URL with npm, bun and yarn
classic, and that URL works only inside a faber job. Opt such a job out.

### What to expect: the counters

As for LFS, per repo since the service last started, in `/api/state` →
`mirror.npm.repos`, with npm's share of the store (`objects`, `bytes`) and the
one cap (`max_bytes`). Here a request is a packument or a tarball, and anything
passed through:

| Counter | Means |
|---|---|
| `hits` | packuments and tarballs served from the cache |
| `misses` | requests that went to the registry, kept or passed through |
| `bytes_from_cache` | bytes served from the cache |
| `bytes_from_upstream` | bytes fetched from the registry, including the packuments a tarball miss read its hash from |
| `passed_through` | requests served without being kept, the forwarded ones included |

The dashboard's **Mirror** section shows them beside LFS's.

### Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| `502 … not the packument's` | the registry sent bytes that are not the tarball its packument names; nothing was kept | re-run; report it if it repeats |
| yarn berry: `Unsafe http requests must be explicitly whitelisted` | the job's own `.yarnrc.yml` or env replaced `unsafeHttpWhitelist` | add `faber-mirror` to it, or opt the job out |
| a private package on npmjs `404`s, or `npm publish` `401`s | the job's token is written for `//registry.npmjs.org/`, which npm does not send to the mirror | opt the job out |
| a private package on another registry `404`s | it lives on a registry the project configures for its scope | nothing to do here: scoped registries are untouched; check the scope's `.npmrc` |
| a lockfile in a PR names `faber-mirror` | a job wrote the lockfile through the mirror and committed it | opt that job out |
| counters stay at 0 | the job's tool does not read the variables, or it sets its own registry | `npm config get registry` in the job shows the registry it uses |

### Failures are recorded

As for LFS: a request the mirror itself fails (its store, say) is counted
against the container whose network it came on, and writes one audit `fault`
line, scope `mirror`, action `request_failed`. A job that then fails, or was
attached while the mirror went down or restarted, gets **`cause: "mirror"`** on
its `outcome` record. Failures from the registry are not the mirror's and are
never counted.

## Attention rows

The dashboard and the tray show the mirror's trouble as attention rows
(`/api/state` → `mirror.attention`; `mirror` is `null` with the feature off).
Each change also writes one audit `fault` line, scope `mirror`: `failed` on going
down or losing the image, `passing_through` (with the reason) on starting to pass
through, `recovered` on getting back to healthy. A poll that finds the same state
writes nothing.

| Row | Severity | Means | What to do |
|---|---|---|---|
| Mirror image missing | bad | `faber-mirror:<version>` is not on the runners' engine | Build it (above). Jobs run without the mirror meanwhile |
| Mirror down, since … | bad | No healthy answer for over 60 s: the container would not start, crashes, or something else holds its name. The popover counts the jobs that were using it when it went down; the dashboard and the audit line name them. The row carries the last error | Read the error and the daemon log. New jobs run without the mirror meanwhile |
| Mirror passing through | warn | The service answers but cannot cache, for the reason shown (for instance an unwritable cache volume) | Jobs still fetch through it, uncached. Fix the reason; nothing else is affected |

A mirror that is starting, for its first 60 seconds, shows no row.
