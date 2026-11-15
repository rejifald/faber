# Operating faber

This guide is for the person who runs faber on a Mac: the operator. The
[README](../README.md) takes you from nothing to a first job. This page picks up from
there: where the trust boundary sits and which settings move it, how the runner image
reaches faber, how to configure and update a Mac, and what to read when something goes
wrong.

It assumes a Mac set up with `sudo faber setup`, so faber keeps its files in
`/Library/Application Support/faber` and its logs in `/Library/Logs/faber`.
[`deploy/README.md`](../deploy/README.md#file-layout) lists every path. Each section
links the decision records (ADRs) behind it, in [`adrs/`](adrs/).

`/api/state`, which comes up throughout, is the daemon's live state as JSON. The
dashboard reads it, and so can you, over the control socket, which answers only you and
root ([ADR-0028](adrs/0028-the-control-api-knows-its-caller.md)):

```sh
curl -s --unix-socket "/Library/Application Support/faber/control.sock" http://faber/api/state
```

## First run

The [quick start](../README.md#quick-start) leaves you with a daemon, a token and one
repo opted in. A few things it doesn't say:

- **Without a token, faber waits in standby.** It claims no jobs, the dashboard says
  what is missing, and it tries again every minute. A Mac that has not run
  `sudo faber setup` has nowhere to keep a token, so run setup first.
- **`sudo faber auth set` checks the token before it keeps it.** It reads the token from
  a hidden prompt, or from a pipe, never from an argument. It asks GitHub which
  repositories the token reaches, lists them, and waits for the daemon to pick it up.
- **A repo needs both the topic and the token.** faber runs jobs for a repo that carries
  the `faber` topic *and* that you picked on the token. The dashboard flags a repo that
  has one but not the other.
- **The token expires.** `faber auth link` asks for 90 days unless you pass `--days`
  (1 to 366; a token that never expires is refused). faber warns 14 days ahead. To renew
  it, use *Regenerate token* on its page on GitHub, which keeps its repositories and
  permissions, then run `sudo faber auth set` again.
- **Some features need more permissions.** Turn on auto-retry or the git mirror, for
  example, and the dashboard names the permission each repo lacks. You can tick it on
  the token's page on GitHub without making a new token
  ([ADR-0027](adrs/0027-github-access-is-an-explicit-fine-grained-grant.md)).

The config that setup writes already has one profile, `linux-docker`, for the image the
quick start builds. [The runner image](#the-runner-image) says what that image must do.

## The trusted-code boundary

faber runs your own code on your own Mac. It is not a sandbox for anyone else's.

Two layers keep other people's code off the machine, and they are not equal:

1. **Routing, in each workflow.** The `runs-on` expression in the
   [quick start](../README.md#3-send-your-jobs-to-faber) sends fork pull requests, pull
   requests from people you haven't listed, and Dependabot to GitHub's runners. This is
   the control you rely on. Never point a workflow that can run a stranger's code at
   faber's labels.
2. **faber's gate, before it starts a runner.** It refuses public repos, trigger events
   outside an allow-list, fork pull requests, and code written by anyone who is not the
   repo's owner, a member of its organization or a collaborator. When it can't tell, it
   refuses.

The gate is the second layer because of how GitHub hands out work. A runner faber starts
for one job can be given any other queued job of the same repo that its labels cover,
including one the gate refused. So the gate decides what faber starts a runner *for*,
not what runs on it. faber judges again every job that ran on one of its runners, and
records one it would have refused as a *gate bypass*
([ADR-0030](adrs/0030-the-gate-is-defense-in-depth-and-a-bypass-is-detected.md)).

### The settings that move it

The boundary is yours to set, in your `config.toml`. No setting can let in a fork pull
request or a trigger outside faber's ceiling, and the few that widen faber's default are
opt-in and take `sudo` to change. These are the defaults:

```toml
trusted_actors = []                  # empty: trust by provenance alone
denied_actors = ["dependabot[bot]"]  # always refused

[gate]
allowed_events = ["push", "pull_request", "workflow_dispatch", "schedule", "merge_group"]
allow_public_repos = false
allow_job_token_dispatches = false
on_bypass = "record"
```

- **Events only narrow.** faber has a fixed ceiling of trigger events, and
  `allowed_events` can only pick from it. `repository_dispatch` and `release` are under
  the ceiling but stay off until you add them. `pull_request_target`, `issue_comment`
  and `workflow_run` are outside it. `faber check-config` and the Settings page refuse
  an event outside the ceiling, and so catch a typo that would otherwise quietly stop a
  whole trigger. A daemon that starts on such a file ignores the event and warns.
- **Public repos are opt-in.** Turning `allow_public_repos` on weakens nothing else:
  fork pull requests are still refused, and the ceiling stays the ceiling. But anyone can
  then queue jobs against a repo faber watches.
- **There is no `allow_fork_prs`.** Writing it is an error, and faber won't start on the
  file. While job code can reach a privileged Docker daemon (next section), running a
  stranger's pull request would hand out root on faber's Linux VM, and that is not a
  switch.
- **Dispatches made by a job are refused.** A `workflow_dispatch` or
  `repository_dispatch` made with a job's own token runs whatever code that job pushed,
  with the dispatched workflow's secrets. `allow_job_token_dispatches = true` lets them
  through again, and faber warns at every boot while it is on.
- **The actor lists judge everyone a job runs for:** the actor, the triggering actor, and
  the author of each pull request whose code the job runs. `denied_actors` always wins.
  A non-empty `trusted_actors` also requires a match.
- **`on_bypass` decides what a gate bypass costs.** `"record"` records it, in the audit
  log and on the dashboard. `"enforce"` also acts on a bypass whose code may be hostile,
  which is any but an event this Mac merely chose not to run: it kills that runner,
  deletes the repo's build cache, and holds the repo, so faber starts no runner for it
  until you run `sudo faber repo-hold clear owner/demo`.

Only your own config can say any of this. A repo's own `.faber/config.toml` is reachable
from a pull request, so it can never name a gate key, and a test pins that. The boundary
in force is reported in `/api/state` and written to the audit log at every boot. For
how a widening change is made, see [Configuring](#configuring); for the reasons, see
[ADR-0014](adrs/0014-the-trust-boundary-is-operator-configurable.md) and
[ADR-0059](adrs/0059-the-gate-runs-again-when-a-job-starts.md).

## The runner container is not a boundary

It is tempting to read "the job runs in a container" as isolation. Once a job can reach
a Docker daemon, it isn't.

A Docker profile that grants Docker access (`dind`) starts a `--privileged`
Docker-in-Docker container next to each runner. Its API is plain, unauthenticated TCP on
the job's own network, and the job gets `DOCKER_HOST=tcp://dind:2375`: that is what lets
a job run `docker`. Any job that reaches that daemon can start a container that mounts
the disk under it, which makes it root in faber's Linux VM. The VM holds nothing of the
Mac's, but every Docker job on the Mac shares it, so a dind job can reach later jobs,
for example by rewriting the runner image
([ADR-0026](adrs/0026-job-code-runs-only-inside-vms-faber-owns.md)).

What limits that is the routing above, and not handing the daemon out:

```toml
# A job lands on the first profile whose labels are all in its runs-on,
# so the profile with more labels comes first.
[[profile]]
name = "linux-dind"
labels = ["faber", "docker", "dind"]
backend = "docker"
image = "faber-runner:local"
dind = true    # only for jobs that build images

[[profile]]
name = "linux-docker"
labels = ["faber", "docker"]
backend = "docker"
image = "faber-runner:local"
dind = false   # no privileged container, no daemon on the job's network
```

Most jobs (`cargo test`, `npm test`, a linter) never run `docker`, and for them the
grant is pure exposure. A job needs it if it runs `docker` itself, or if its workflow
uses `container:` or `services:`, which GitHub's runner implements by driving a Docker
daemon. For `services:`, [declared services](#declared-services-instead-of-dind) are the
way out.

Leaving `dind` out of a profile grants it, so configs written before the key existed
keep working, and faber warns at every boot. Say `true` or `false`. Each finished job's
audit line records whether it had a daemon
([ADR-0013](adrs/0013-docker-in-docker-is-a-grant-not-a-default.md)).

### Ask for the daemon by label

With `[host] dind_requires_label = true`, the grant alone is not enough: a job gets a
daemon only when its `runs-on` also includes `dind` (`runs-on: [faber, docker, dind]`).

- A job that doesn't ask gets no daemon on any profile. Its first `docker` call fails
  with `unix:///faber-no-dind/add-dind-to-runs-on` in the error, which names the fix.
- A job that asks on a profile with `dind = false` gets no runner at all. It is listed
  under `dind_refused` in `/api/state`.
- Left out, the rule is off: every job on a granting profile gets a daemon, as before,
  and the dashboard and every boot say so.

Labels can require, never exclude. GitHub may hand a runner that has a daemon to a
sibling job of the same repo that never asked for one, as long as the runner's labels
cover it. Treat every job of a repo with a dind job as one that can reach the daemon. See
[ADR-0013's amendment](adrs/0013-docker-in-docker-is-a-grant-not-a-default.md#amendment-2026-10-17-a-job-asks-for-the-daemon-with-dind).

### The mirrors stay off beside dind

The git mirror (`git_mirror`) and the LFS mirror (`[mirror.lfs]`) both put faber's
GitHub token on the Docker engine that runs jobs, and a dind job is root there. So
`faber check-config` and the Settings page refuse either mirror while any Docker profile
grants dind, and a daemon that boots on such a config leaves both off and says so on the
dashboard. Once the engine has granted dind, they stay off on it. If either mirror ever
ran beside dind on your Mac, regenerate the token and review what it could have changed:
[docs/git-mirror.md](git-mirror.md#not-beside-dind) has the steps.

## Declared services instead of dind

A job that only needs a database, a cache or a mail catcher doesn't need a Docker daemon.
It asks for the service with a label, and faber runs the service next to the runner, in
its Linux VM. The [README](../README.md#databases-and-other-services-per-job--opt-in)
shows the short version; this is the rest.

You decide what may be asked for, in your config. No table, no service: a job that asks
for a service this Mac doesn't offer, or for a Postgres major outside `versions`, gets
no runner. It is listed under `service_refused` in `/api/state` and on the dashboard.

```toml
[services.postgres]
versions = "16, 17"                  # exact majors, 13 or later
image = "postgres:{version}-alpine"  # {version} is the major the job asked for
extensions = ["pg_trgm"]             # installed into every job's database
memory_mib = 2048                    # reserved while the shared server runs
idle_stop_secs = 900                 # stop it after 15 minutes unused (the default)

[services.redis]
image = "redis:7-alpine"
memory_mib = 256                     # added to each job's reservation
```

- **Postgres has versions.** A job asks for `faber.postgres-<major>`, and `versions`
  must list that major.
- **Redis and Mailpit don't.** Their labels are bare (`faber.redis`, `faber.mailpit`),
  and their tables take no `versions`, `extensions` or `idle_stop_secs`.
  `[services.mailpit]` takes the same keys as Redis.
- **Any other name is a service of your own,** one container per job. A table
  `[services.<name>]` offers it, a job asks for it with `faber.<name>`, and `<name>`
  (lowercase letters and digits, starting with a letter) is its host name. Besides
  `image` and `memory_mib`, such a table takes `args`, `env`, `job_env` (what the runner
  gets) and `health_cmd`, and `{secret}` in `env` or `job_env` is a fresh random value
  per job
  ([ADR-0062](adrs/0062-an-s3-store-is-a-per-job-versity-gateway-with-a-secret-made-per-job.md)).
  The schema at the top of [`src/config.rs`](../src/config.rs) has a full example.
- **A service name can't also be a `[needs]` tool.**
- `faber check-config` refuses a table that breaks these rules, and warns when
  `[host] pull_unqualified_images = false` stops faber pulling a service image.
- **Changing what may run widens trust**, so it takes `sudo`: adding a table, changing
  its image, command, environment or health check, adding or changing a `job_env`
  entry, or adding an extension or a Postgres major.
- **`job_env` tells a job where a service is**, not how to run: names that change how
  the job's own programs load, start or connect (`LD_*`, `DYLD_*`, `BASH_ENV`, `ENV`,
  `NODE_OPTIONS`, `*_PROXY`) are refused.

Postgres is one shared server per major, with a database and a role of its own for each
job. The role may create databases, so test runners that make `test_<name>` keep
working. Redis, Mailpit and your own services are one container per job, removed with
the runner. A job reaches each by host name on its own network, never on `localhost`,
and gets the addresses as environment variables:

| Service | Host | Variables |
|---|---|---|
| Postgres | `postgres` | `DATABASE_URL`, `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE` |
| Redis | `redis` | `REDIS_URL` (`redis://redis:6379/0`) |
| Mailpit | `mailpit` | `MAIL_SMTP_URL` (`smtp://mailpit:1025`), `MAIL_API_URL` (`http://mailpit:8025`) |

A profile's own `env` wins over these.

To move a workflow in `owner/demo` off dind:

1. Delete its `services:` block.
2. Take `dind` out of `runs-on` and add the service labels:
   `runs-on: [faber, docker, faber.postgres-16]`. Keep `dind` only if a step still runs
   `docker` itself.
3. Read `$DATABASE_URL` (or the `PG*` variables), `$REDIS_URL` and the `MAIL_*` URLs
   instead of a hard-coded `localhost:5432`, `localhost:6379` or `localhost:1025`.

A Mac without `[services.*]` provisions exactly as before, and a job that keeps its
`services:` block and its `dind` label keeps working. See
[ADR-0046](adrs/0046-jobs-declare-services-and-faber-runs-them-without-dind.md).

## VM jobs and the LAN

Every job runs inside a VM faber owns, and the VM wall decides what it can reach: the
public internet, and nothing else. Not this Mac, not your LAN, not your VPN. DNS goes
only to the wall's resolvers, and all IPv6 is dropped
([ADR-0026](adrs/0026-job-code-runs-only-inside-vms-faber-owns.md)).

When a job really needs a machine on your network, root grants it, one IPv4 network and
port at a time. A grant names a profile, but reaches every VM of that profile's kind (see
below). Here `192.0.2.5` stands for that machine's address:

```sh
sudo faber grant add --profile linux-docker --to 192.0.2.5/32 --port 5432/tcp
sudo faber grant remove --profile linux-docker --to 192.0.2.5/32 --port 5432/tcp
faber grant list
```

- **A destination is always `ADDRESS/PREFIX`, never a hostname:** a name can resolve
  somewhere else tomorrow. Reach a VPN peer by its address.
- **A grant reaches every VM of its kind.** A grant to a Docker profile covers the whole
  Linux VM, which every Docker job on this Mac shares. A grant to a `macos-vm` profile
  covers every macOS VM.
- **A grant never opens this Mac, another VM, a broadcast or multicast,** however wide
  its network. A destination that holds one of the Mac's own addresses is refused unless
  you add `--overlaps-host`, and the Mac stays blocked even then.
- **The grants live in `/etc/faber/grants.toml`, root's file.** Each change re-renders
  the wall's rules, reloads them at once, and leaves a `grant` line in the audit log.

The resolvers are root's too. The default pair is 1.1.1.1 and 1.0.0.1:

```sh
sudo faber grant dns 192.0.2.53 198.51.100.53   # example addresses
sudo faber grant dns --default
```

`config.toml` can narrow a grant but never make one. List the grant under the profile's
`drop_grants`, then apply it as root:

```toml
[[profile]]
name = "linux-docker"
# ...
drop_grants = ["192.0.2.5/32 5432/tcp"]
```

```sh
sudo faber grant apply
```

Taking an entry out of `drop_grants` reopens the grant at the next render, so that
change takes `sudo`.

## The runner image

Job code never runs on the Mac itself. Linux jobs run in a container in faber's Linux
VM, started from a runner image you control. macOS jobs will run in a macOS VM each
([still in progress](../README.md#macos-jobs-in-their-own-vm--in-progress)). The old
native backend, which ran a runner as faber's own account, is gone: a profile with
`backend = "native"` is a config error. Switch it to `backend = "macos-vm"` and keep its
labels, so workflows don't change
([ADR-0052](adrs/0052-a-native-profile-is-a-config-error.md)).

### What the image must do

faber never installs or configures a runner itself, so the image decides what a job can
run. faber registers a single-use runner with GitHub and starts the image with that
registration in `RUNNER_JITCONFIG`. The entrypoint must hand it to the runner's `run.sh`
through the environment, as `ACTIONS_RUNNER_INPUT_JITCONFIG`, and never as a
`--jitconfig` argument: `ps` and `/proc` show a process's arguments to every account on
a Linux host. That keeps it off the argv, and no more: the container's first process
keeps it in its environment, where a job step can read it. By then it is spent, and
nothing in faber treats it as a secret from the job.

Every runner also waits, when a job starts, for faber's verdict on the job GitHub gave
it ([ADR-0059](adrs/0059-the-gate-runs-again-when-a-job-starts.md)). faber mounts a
job-started hook read-only and points `ACTIONS_RUNNER_HOOK_JOB_STARTED` at it, so the
image must also:

- run the GitHub Actions runner itself, which honours `ACTIONS_RUNNER_HOOK_JOB_STARTED`
  and runs a `.js` hook on the Node it bundles. The hook needs nothing else from the
  image, not even a shell.
- have no `.env` file in the runner's directory that names
  `ACTIONS_RUNNER_HOOK_JOB_STARTED`. The runner applies its `.env` over its
  environment, so such an entry would replace faber's hook.
- have `sh` and `wc`, which faber uses once per job, in a short-lived container of the
  same image, to write the hook into its volume and check it is all there.

A runner that ignores the hook runs jobs unguarded until faber notices: faber records a
*hook fault* for a job it never saw a hook waiting for, and stops it under `enforce`.
A profile's `env` cannot name `RUNNER_JITCONFIG` or `ACTIONS_RUNNER_HOOK_JOB_STARTED`:
`faber check-config` refuses it, and a daemon booting with one leaves it out.

The image in [`docker/runner`](../docker/runner) does exactly this. It is a starting
point: add what your jobs need (a JDK, a browser set, Python) to its Dockerfile and
rebuild.

### How the image reaches faber

The Linux VM's Docker is faber's own, so an image in your local Docker doesn't reach it
by itself. You hand it over as a `docker save` archive, as in the
[quick start](../README.md#4-give-it-a-runner-image):

```sh
docker build -t faber-runner:local docker/runner
docker save faber-runner:local -o "$HOME/faber-runner.tar"
sudo faber setup --runner-image-tar "$HOME/faber-runner.tar"
```

- **Setup keeps the archive** in root's image store, `/var/db/faber/images/runner/`.
  Each time the Linux VM comes into service, at every daemon start too, faber loads
  every archive whose tags the VM lacks. Jobs wait while it loads. A profile image that
  is in neither the VM nor an archive is named on the dashboard and in `faber status`.
- **The archive is the image's backup.** A local tag is the intended shape: nothing has
  to be pushed to a registry. faber never deletes the archive or the image, and
  `sudo faber uninstall` leaves the store alone.
- **An archive is never replaced.** Setup refuses a file name it already keeps with
  different contents, and faber skips an archive whose tags the VM already has. So a
  rebuilt image needs a new tag and a new file name, as in the next section.
- **faber reads the first 16 archives there, by name.** Once no profile names an old tag
  and the new one has run jobs, removing the old archive is up to you.

## When GitHub retires the runner version

The image bakes one `actions/runner` release (`RUNNER_VERSION` in its Dockerfile).
GitHub retires old releases on a date it announces. From that date a runner on one
registers but never connects, and every job on the profile stops.

Weeks ahead, GitHub adds a note to each job such a runner runs, naming the version and
the date it stops working. faber samples a finished job of each profile about once an
hour, reads that note, and turns it into a warning per profile: on the dashboard, in the
menu bar, under `runner_retirements` in `/api/state`, and in a daily log line naming the
profile, image, runner version, cutoff date and days left. From three days out the
wording turns urgent. It is only a warning: jobs keep running until GitHub's cutoff.

To act on it:

1. **Rebuild** the image with a current release, under a new tag, and check it:

   ```sh
   V=2.340.0   # an example: use the latest from github.com/actions/runner/releases
   docker build --build-arg RUNNER_VERSION=$V -t faber-runner:$V docker/runner
   docker run --rm --entrypoint /runner/bin/Runner.Listener faber-runner:$V --version
   ```

   The last line should print the new version.
2. **Hand it to faber,** under a new file name:

   ```sh
   docker save faber-runner:$V -o "$HOME/faber-runner-$V.tar"
   sudo faber setup --runner-image-tar "$HOME/faber-runner-$V.tar"
   ```

3. **Point the profile at the new tag,** on the dashboard's Settings page or with
   `sudo faber config set profile.linux-docker.image faber-runner:$V`. A profile's
   image is what its jobs run in, so changing it widens trust and takes `sudo`.
4. **Drain, then restart faber,** so the profile change applies and the new archive
   loads:

   ```sh
   faber drain
   sudo launchctl kickstart -k system/io.github.rejifald.faber
   ```

   `faber drain` stops claiming and waits until no claim is admitted. Use it rather than
   pausing: a paused Mac keeps its parked claims, so the queue never empties
   ([ADR-0023](adrs/0023-pause-stops-what-starts.md)).
5. **Check.** faber keeps the warning in memory, so the restart clears it, and its
   absence proves nothing yet. The first job to finish after the restart is sampled, and
   the warning comes back if that job's runner still carries the note.

Keep the old archive until the new tag has run a job. Every Mac that runs the profile
needs the new archive: a local tag lives on one machine only.

## Configuring

Everything faber reads lives in one `config.toml`: the topic it watches, the profiles
and their images, the trust boundary, the machine's share, services, mirrors and
updates. On a Mac set up with `sudo faber setup` it is
`/Library/Application Support/faber/config.toml`. Setup writes it from
[`deploy/config.starter.toml`](../deploy/config.starter.toml) when there is nothing to
migrate, and leaves it alone after that. The schema, every key with its default, is the
comment at the top of [`src/config.rs`](../src/config.rs).

The file belongs to faber's own account. You can read it, through the `_faber_operator`
group setup puts you in (from your next login), but you change it through faber:

- **The dashboard's Settings page** (`faber dashboard`) edits every key, as a form or as
  the `config.toml` text. A form save rewrites the file from data, so comments don't
  survive it; a save from the text view keeps them. Either way the previous file stays
  beside it as `config.toml.bak`.
- **`faber config`** changes one key, or the whole file, from a terminal. Lists are
  comma-separated, and `faber config unset <key>` clears an optional key:

  ```sh
  faber config set update.maturity_secs 86400
  faber config set profile.linux-docker.dind false
  faber config apply my-config.toml
  ```

- **`faber check-config`** checks a file without changing anything, and exits non-zero
  when faber would refuse it. Without `--config` it checks this Mac's own file.

The Settings page and `faber config` judge a change before they write it. A routine
change, or one that narrows something, works as you. One that widens trust takes
`sudo`: granting dind, widening `[gate]` or the actor lists, changing a Docker
profile's image or backend or adding one, adding a service or changing its image,
switching a mirror on (`git_mirror`, a profile's `mirror`, `[mirror]` and its tables),
changing `[autolimit] probe_image`, letting faber pull unqualified image names, turning
updates on, pinning them or shortening their wait, and taking an entry out of
`drop_grants`, among others. The dashboard can't make such a
change: it refuses with the exact `sudo faber config …` command to run instead. Every
widening, made or refused, leaves a `control` line in the audit log.

faber reads the file once, at startup, so most changes wait for a restart:

```sh
sudo launchctl kickstart -k system/io.github.rejifald.faber
```

Jobs already running carry on: the new process adopts them
([ADR-0011](adrs/0011-adopt-in-flight-work-across-restarts.md)). After a save, the
Settings page lists the keys the running daemon hasn't taken yet. A few apply at once:
the autolimiter's `reserve_mib`, `headroom_mib`, `margin` and `hard_ceiling`, and every
`[update]` key.

Some keys tighten what faber does, and a release must not tighten a Mac that didn't ask.
Those have a third state, *unstated*: left out, faber behaves as it did before the key
existed, warns at every boot, and shows the key as unstated. A profile's `dind`,
`[host] dind_requires_label`, `keep_memory_gib`, `keep_cpus` and
`disk_admission_floor_gib`, and `[autolimit] cpu` work this way. Say what you mean.

### Leave the Mac its share

On a Mac someone also works on, state what CI must leave it, as in the
[README](../README.md#a-job-only-starts-when-it-fits--on):

```toml
[host]
keep_memory_gib = 16   # example values
keep_cpus = 2
```

Once stated, faber plans CI within the rest of the machine and warns while the Linux VM
is larger than that. With `keep_memory_gib` above 0 it also starts nothing new while
macOS reports memory pressure or swap is growing, and it holds back the larger of
`reserve_mib` and the most the Linux VM used beyond faber's own containers over the last
day. `faber check-config` refuses a share as large as the machine. Until you state the
keys, faber warns at every boot
([ADR-0021](adrs/0021-the-machine-share-is-declared-not-left-over.md));
[docs/autolimiter.md](autolimiter.md#the-machines-share-adr-0021) has the arithmetic.

Every VM faber runs comes out of the rest: the Linux VM (`[linux_vm] memory_mib`, 16384
unless set) and each macOS VM. On macOS a VM never gives memory back, so a bigger VM is
not spare capacity. It is memory the Mac loses for as long as the VM runs.

## Updating

### Until releases install

No faber release can install yet. The release check compiled into faber does not yet
name the repository releases will come from, so it refuses every one
([ADR-0031](adrs/0031-releases-are-signed-by-github-from-a-fresh-public-repository.md)).
`faber provenance-policy` prints that policy and says when it refuses everything.

Until then you update a Mac by hand, from source.
[docs/manual-update.md](manual-update.md) is the procedure: build `main` in a worktree
of its own, check your config against the new binary, drain, and run `sudo faber setup`
from the new build. Setup drains the running daemon again before it swaps anything, and
puts the previous daemon back if the new one fails to start.

### Once releases install

A Mac that opts in follows releases on its own
([ADR-0006](adrs/0006-fleet-autoupdate.md)):

```sh
sudo faber config set update.enabled true
```

It is off by default: installing faber must not quietly let it replace its own binary.
`sudo faber setup` asks once, when it writes a starter config, and `--yes` never answers
that question for you. Turning it on widens trust, hence `sudo`.

A follower checks for releases every 30 minutes (`[update] poll_interval_secs`). With a
new one, it:

1. **Waits for it to mature:** `[update] maturity_secs`, 24 hours unless set and never
   under one, counted from the later of its publication and when this Mac first saw it.
   The wait is how a release the owner didn't make gets caught. Nothing in a release
   shortens it; an urgent fix is installed by hand.
2. **Checks where it came from, before it downloads any binary.** It fetches
   `SHA256SUMS` and checks its GitHub attestation, without keys, against a policy
   compiled into faber. No setting skips this. Then every asset must match
   `SHA256SUMS`, and the binary must report the release's version.
3. **Checks your config** with the new binary before it swaps anything.
4. **Waits for an idle moment.** After `[update] escalate_after_secs` (6 hours unless
   set) it stops claiming and drains instead.
5. **Swaps, and watches the next boot.** A version that crash-loops on boot is rolled
   back, and never tried again on this Mac.

A refused release is held for one poll interval before faber tries again, and one GitHub
couldn't answer about backs off from 60 seconds up to the poll interval. A new tag, or a
restart, ends the hold. Deleting a published release never rolls a Mac back: Macs that
installed it keep it, and the others stop seeing it. To hold one Mac at a version while
a release proves itself on another, set `[update] pin = "x.y.z"`.

The menu-bar app and the CLI copy follow on their own terms
([ADR-0034](adrs/0034-the-app-and-the-cli-copy-follow-the-daemon-each-under-its-own-account.md)):

- **The app updates itself,** as you. Once the daemon has installed a release and passed
  its boot check, the app fetches that same version, checks it the same way, waits its
  own maturity time (24 hours by default), and swaps itself once its popover has been
  closed for a moment. If the new app doesn't come up, it puts the old one back.
- **The CLI copy, `/usr/local/bin/faber`, is root's,** and root never runs a file
  faber's own accounts can write, so nothing updates it behind your back. While the
  daemon is newer, every `faber` command prints one line saying so.
  `sudo faber upgrade-cli` copies the daemon's binary, checks the copy against its
  signed release, and asks before installing it. `sudo faber setup` runs it first.

## When something goes wrong

Start with the dashboard (`faber dashboard`). It says why each waiting job is waiting,
and what faber thinks is wrong with the Mac, its VMs or GitHub.

In a terminal, `faber status` reads the audit trail back: tallies, faults, and failed
runs with their causes. It works on a stopped daemon, since the log outlives the
process, and adds live state when the daemon answers. On a Mac it also shows the VM
wall, which must read green, and whether the Linux VM's image is current and the runner
image is loaded.

```sh
faber status                                 # the last 24 hours
faber status --since 90m --repo owner/demo
faber status --limit 50                      # print up to 50 failed runs in full
faber status --json                          # the same digest, as JSON
faber status --log ./audit.jsonl             # a trail copied off another Mac
```

If `faber status` can't read the log, your membership of `_faber_operator` hasn't taken
effect yet: open a new login session, or run `newgrp _faber_operator`. To see whether
the daemon is running, use `sudo launchctl print system/io.github.rejifald.faber`. Its
own log is `/Library/Logs/faber/stderr.log`.

### The audit trail

faber's durable record is `/Library/Logs/faber/audit.jsonl`: append-only, one JSON line
per event. `kind` says what each line is: a gate `decision`, a finished run's `outcome`,
a `drift` check, an auto-`retry`, a `fault`, a `grant` change, a trust-widening
`control` change, and a few more. Every line that isn't a plain success carries a
`cause`: the error behind a failed start, where a reclaim landed, the cap a timeout hit,
a failed job's exit status, the error behind a fault.

These fields join a line to everything else:

- `runner`: the runner's name, the same as in GitHub's runner list and on the
  `faber-runner-*` container that ran the job.
- `profile`: the profile that served the job.
- `attempt`: which provision attempt this run was.
- `wait_ms` and `wait_gate`: how long the job waited for room before it started, and
  which limit held it longest.
- `dind`: whether the job had a Docker daemon, `granted` or `denied`.

`fault` lines are faber failing rather than a job: a cycle that can't reach GitHub, a
repo whose scan keeps failing. faber writes one when the trouble starts and one when it
clears, so a stretch with no jobs says why instead of just being empty.

When you report a problem in public, describe it. Audit lines and logs name your repos,
your logins and your network, so don't paste them.

## Developing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

faber needs Rust 1.96 or later. The dashboard is a web app in `ui/`, built into the
binary at compile time. Build it first, or the binary serves a "dashboard was not built"
page instead:

```sh
( cd ui && npm ci && npm run build )
FABER_REQUIRE_UI=1 cargo build --release --locked
```

`FABER_REQUIRE_UI=1` makes a build without the UI fail instead. `cargo run -- status`
and `cargo run -- check-config --config <file>` try a change against a real log or
config.

Read [AGENTS.md](../AGENTS.md) before you change anything. It is written for coding
agents, and its rules hold for every change: everything written here is public, root
never runs what faber's accounts can write, and a Mac can't compile the Linux half of
the code, so it says how to check that half in a container.
