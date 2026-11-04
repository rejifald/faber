<p align="center">
  <img src=".github/assets/banner.svg" alt="faber: your own CI runners, on your own hardware. Faster jobs, service containers, isolated, trust-gated." width="100%">
</p>

**faber runs your CI jobs on hardware you own.** A *runner* is the computer that
actually carries out a CI job. When a job is queued, faber starts a fresh runner for
that one job, lets it run, and then throws the runner away. Today it works with
**GitHub Actions**, on **Macs with Apple silicon**. Other platforms may follow.

You keep writing workflows exactly as you do today. Logs, checks, re-runs and secrets
are managed on GitHub, where they always were. faber only changes *which computer* does the
work.

> [!WARNING]
> **Only your own code.** faber runs jobs on your machine, so only send it code you
> would run yourself. Pull requests from forks and from people you don't know must keep
> running on GitHub's own runners. The [routing snippet](#3-send-your-jobs-to-faber)
> below does this for you, and faber double-checks every job before it starts.

> [!IMPORTANT]
> **Your secrets come to your Mac.** To run a job, GitHub sends that job's secrets
> (deploy keys, API tokens, anything in `secrets.*`) to the runner, which now sits on
> your machine. Every self-hosted runner works this way, but it means your Mac holds
> those secrets while the job runs. Give faber only the jobs whose secrets you're
> comfortable having there.

> **Status:** faber is moving into this repository from its previous home, piece by
> piece. Commands that use files not here yet will work once those files arrive.

## Why you'd want it

- **It's fast.** A recent Mac usually outruns a standard GitHub-hosted runner, and
  nothing has to download a fresh machine image first.
- **Caches stay warm.** Git history, `node_modules` downloads and Git LFS files can be
  served from your own disk instead of fetched from the internet on every run.
- **You pick the tools.** Need a particular browser build, JDK or Postgres version?
  Put it in your runner image once. No more waiting for hosted images to catch up.
- **Every job starts clean.** Each job gets its own throwaway runner, so nothing leaks
  from one run into the next.
- **It saves money.** GitHub bills macOS minutes at a premium, and Git LFS bandwidth
  and storage on top. With many pipelines running many times a day, that adds up fast.
  On faber those minutes run on a machine you already own, and LFS files come from a
  local cache.
- **It's polite to your Mac.** You tell faber how much memory and CPU to leave for you,
  and it only starts as many jobs as fit in the rest.

What it's **not** for: running CI for strangers, or replacing your CI service. faber
gives your CI a computer to run on; the pipelines themselves are still defined and run
by your CI service (GitHub Actions, today).

## What you need

- A Mac with **Apple silicon**. faber runs your Linux jobs inside a small Linux
  *virtual machine* (VM), a separate computer simulated in software, which it manages
  for you.
- Admin rights on that Mac, once, for setup.
- The [GitHub CLI](https://cli.github.com) (`gh`), signed in.
- A few minutes while faber builds. It is built from source; the installer offers to
  install anything missing (Xcode Command Line Tools, Rust, and Node for the menu-bar
  app).
- Any local Docker, only to build the runner image the first time.

## Quick start

### 1. Install

```bash
gh repo clone rejifald/faber ~/.faber/src
```

```bash
cd ~/.faber/src && ./install.sh
```

The installer asks whether you want the menu-bar app too, builds faber, and finishes
with `sudo faber setup`. Setup shows you exactly what it is about to do and asks before
it changes anything. After that, faber runs in the background under its own hidden
user accounts (not yours), starts when the Mac starts, and doesn't need you logged in.

### 2. Connect it to GitHub

```bash
faber auth link
```

This prints a link to GitHub's page for creating an access token, with the right
permissions already filled in. Open it, pick the repositories this Mac should run jobs
for, create the token, then hand it to faber:

```bash
sudo faber auth set
```

### 3. Send your jobs to faber

Opt a repository in by giving it the `faber` topic:

```bash
gh repo edit owner/demo --add-topic faber
```

Then change `runs-on` in that repo's workflow. The long expression below sends your
own jobs to faber and keeps fork pull requests, other people's pull requests and
Dependabot on GitHub's runners:

```yaml
jobs:
  test:
    runs-on: ${{ ((github.event_name == 'pull_request' && (github.event.pull_request.head.repo.fork || !contains(fromJSON('["your-login"]'), github.event.pull_request.user.login))) || github.actor == 'dependabot[bot]') && 'ubuntu-latest' || fromJSON('["faber","docker"]') }}
    steps:
      - uses: actions/checkout@v4
      - run: npm ci && npm test
```

Replace `your-login` with the GitHub usernames whose pull requests you trust, for
example `["alice","bob"]`.

### 4. Give it a runner image

Linux jobs run inside a *container*: an isolated, disposable environment started from
an *image* (a snapshot of an operating system plus the tools your jobs need). You
control that image. One that works out of the box ships with faber; build it and hand
it to faber's Linux VM:

```bash
docker build -t faber-runner:local docker/runner
```

```bash
docker save faber-runner:local -o ~/faber-runner.tar
```

```bash
sudo faber setup --runner-image-tar ~/faber-runner.tar
```

Until the image is there, jobs wait (they don't fail), and the dashboard tells you why.
Later, add whatever your jobs need (a browser, a JDK, Python) to the image's Dockerfile
and rebuild.

### 5. Push and watch

Push a commit, then open the dashboard:

```bash
faber dashboard
```

Your job shows up within about 20 seconds.

## How it works

```text
          GitHub                                 your Mac
 ┌──────────────────────┐      polls       ┌─────────────────────────────────────┐
 │ queued job           │ ◄─────────────── │ faber daemon (its own hidden user)  │
 │                      │                  │  1. check: is this job trusted?     │
 │ one-time runner      │ ───────────────► │  2. check: is there room for it?    │
 │ registration         │                  │  3. start a runner for this one job │
 └──────────────────────┘                  │        │                            │
            ▲                              │        ▼                            │
            │ job, logs, secrets           │  ┌──────── Linux VM ─────────────┐  │
            └──────────────────────────────┼──┤ fresh container + runner      │  │
                                           │  │ (deleted when the job ends)   │  │
                                           │  └───────────────────────────────┘  │
                                           └─────────────────────────────────────┘
```

1. **faber asks GitHub** every 20 seconds or so whether any opted-in repo has a queued
   job. It only ever calls out: nothing on the internet connects in to your Mac.
2. **It checks the job** is yours and safe to run (see [Safety](#safety)), and refuses
   it if not.
3. **It waits for room**, so jobs never crowd out you or each other.
4. **It asks GitHub for a one-time runner registration**, a pass that lets a runner
   take exactly that one job, and starts a runner with it inside a fresh container in
   faber's Linux VM.
5. **GitHub hands the job to that runner.** Logs and results go straight back to GitHub,
   as with any runner.
6. **When the job ends, everything is deleted**: the runner, its container, its network.

## Features

Each feature is marked:

- **On**: works out of the box.
- **Opt-in**: finished, switched off until you turn it on in `config.toml`.
- **In progress**: not ready yet.

### Linux jobs in a fresh container · On

Every Linux job gets a brand-new container, its own private network, and a runner that
is registered for that one job only. When the job ends, all of it is deleted. The
containers live inside faber's Linux VM, so jobs never run directly on your Mac. By
default they can reach the internet but not your home network or the Mac itself. If a
job genuinely needs a machine on your network, you can open exactly one address and
port with `sudo faber grant add`. Every Linux job on this Mac can then reach it, not
only the job that needed it ([docs/operating.md](docs/operating.md#vm-jobs-and-the-lan)).

### macOS jobs in their own VM · In progress

Jobs that need macOS (an iOS build, say) will each run in a fresh macOS virtual machine,
copied from an image you prepare once and thrown away afterwards. Much of this is
already in place, but it doesn't run real jobs yet.

### A job only starts when it fits · On

faber watches how much memory each kind of job really uses and starts a job only when
there is room for it. You never set a "max 3 jobs" number; you get as many at once as
fit. A job that has to wait shows the reason on the dashboard.

On a Mac you also work on, tell faber what to leave for you:

```toml
[host]
keep_memory_gib = 16   # memory CI must leave free for you
keep_cpus = 4          # cores CI must leave free for you
```

faber also holds back new jobs while macOS reports it is running low on memory. If you
want it to count CPU as well as memory, add `cpu = "auto"` under `[autolimit]`
(opt-in).

### Databases and other services, per job · Opt-in

Need Postgres, Redis or Mailpit in your tests? Ask for them in `runs-on` instead of
writing a `services:` block:

```yaml
runs-on: [faber, docker, faber.postgres-16, faber.redis]
```

The job gets a ready-to-use database of its own, plus the usual environment variables
(`DATABASE_URL`, `PGHOST` and friends, `REDIS_URL`, `MAIL_SMTP_URL`). Turn on the
services you want to offer in `config.toml`:

```toml
[services.postgres]
versions = "16, 17"
image = "postgres:{version}-alpine"

[services.redis]
image = "redis:7-alpine"
```

You can add other services the same way, such as an S3-compatible store.

### Faster checkouts and downloads · Opt-in

- **Git mirror.** faber keeps a copy of each repo's history on your Mac and lends it to
  jobs, so `actions/checkout` fetches only what's new. Even a full-history checkout
  becomes cheap. Turn it on with `git_mirror = true` on a *profile* (a group of jobs in
  `config.toml` that share labels and an image); your workflow needs one extra step
  before checkout.
- **Git LFS and npm mirror.** A local cache answers LFS and npm downloads, so a file is
  fetched from the internet once instead of on every run. Turn it on with a `[mirror]`
  section.
- **Build cache that survives between runs.** Give a repo a folder that persists from
  one job to the next (`cache_volume = true`), available to the job as
  `$FABER_CACHE_DIR`.

The git mirror and the LFS mirror are switched off while any of your jobs is allowed to
run its own Docker (`dind = true`, see [Safety](#safety)), because such a job could
reach faber's GitHub token.

### The right Playwright version for every branch · On, and Opt-in

If your runner image includes Playwright's browsers, it can only include one version.
faber notices when a job's lockfile (the file that pins your exact dependency versions)
asks for a different version and warns you on the dashboard (on by default). Turn on
`[runner_image] build = "on-demand"` and faber goes further: it builds a small variant
of your image with the version the branch asks for, once, and keeps it while it's in
use. You decide which versions it may build.

### Automatic retry when faber was at fault · Opt-in

If a job dies because of faber, not your code (it ran out of memory, hit a time limit,
or the Mac shut down), faber can re-run it once for you. A real test failure is never
retried. Turn it on with `[retry] enabled = true`.

### Several Macs, one shared queue · Opt-in

Run faber on two or more Macs and let them share the same repositories. Each one starts
only the runners that are still missing, so a job is picked up by exactly one machine.
Set `provisioning = "demand"` on every Mac.

### Pick who goes first · Opt-in

List your most important repos under `priority_repos` and their waiting jobs jump ahead
of everyone else's.

### Menu bar and dashboard · On

- The **menu-bar app** shows at a glance how many jobs are running and waiting, and
  whether anything needs your attention. Click it for details.
- The **dashboard** runs on your Mac only, at `http://127.0.0.1:8787`, and is never
  reachable from the network. It shows live jobs, recent runs and why each one ended,
  lets you pause and resume, and has a Settings page that edits the whole config for
  you, so you rarely touch the file by hand.
- **`faber status`** prints the last 24 hours in your terminal: what ran, what failed
  and why. It works even when faber is stopped.
- faber warns you weeks ahead when GitHub is about to retire the runner version in your
  image, so jobs don't suddenly stop.

### Automatic updates · In progress

A Mac will be able to follow new faber releases on its own. It checks that each
release really came from this project before downloading it, waits a day before
installing, swaps only when no job is running, and rolls back a version that won't
start. The switch exists (`[update] enabled = true`), but no signed release has been
published yet, so for now you update by hand, from source. The installer, run again,
pulls the latest source and rebuilds the daemon.
[docs/manual-update.md](docs/manual-update.md) is the full procedure: it also builds
the VM launcher and guest agent from the same commit, and `sudo faber setup` lets
running jobs finish before it swaps the daemon.

## Safety

faber is built for **your own trusted code**. It is not a safe place to run strangers'
code, and no self-hosted runner is a fortress. What faber does is put several walls
between a job and the rest of your Mac.

### Who gets to run

Two layers keep untrusted work off your machine:

1. **Your workflow decides.** The `runs-on` line in the
   [quick start](#3-send-your-jobs-to-faber) sends fork pull requests, pull requests
   from people you haven't listed, and Dependabot to GitHub's runners. This is the rule
   that matters most.
2. **faber checks again.** Before every job, faber refuses public repositories (unless
   you allow them), events that can carry outside code (such as `pull_request_target`),
   and code written by anyone who isn't the owner, an organization member or a
   collaborator. When it can't tell, it says no. You can make these rules stricter in
   `config.toml`, and there is deliberately no switch that allows fork pull requests.

### What a job can reach

- **Jobs never run on your Mac directly.** They run in a virtual machine faber owns,
  inside a container of their own, as a user that isn't you.
- **Jobs can't see your network.** A job can reach the internet, but not your Mac, other
  devices at home or the office, or your VPN, unless you open one specific address for
  it.
- **Nothing carries over.** Each runner is registered for a single job and deleted
  afterwards, so one job can't leave something behind for the next. (The optional
  per-repo build cache is the one thing you can choose to keep.)
- **faber itself runs as its own hidden user accounts**, not as you. It never has your login,
  your keychain or your browser sessions.
- **Nothing listens for the internet.** faber only makes outgoing requests. Its
  dashboard is reachable from your Mac only.
- **Its GitHub access is narrow.** faber's GitHub token reaches only the repos you
  picked, with only the permissions your features need.
- **No credential of faber's reaches a job**, except the runner's own one-time
  registration. Registry logins for the images a job pulls (`[registry_auth]`) are on
  hold: a credential mounted into a job can be read by the job's code, so faber mounts
  none and those pulls stay anonymous. The section returns when faber pulls images for
  the job. Until then `faber check-config` refuses `enabled = true`, and a Mac that
  still has it on warns at start and ignores it.

### The limits

A job runs with whatever access its workflow and secrets give it. And a job that may
run Docker itself (the `dind = true` setting) can take over the whole Linux VM, because
whatever controls that Docker controls the VM. Only turn it on for jobs that really
build container images. If you only need a database, use
[services](#databases-and-other-services-per-job--opt-in) instead.

## FAQ

**Do I have to change my workflows?**
Only the `runs-on` line. Steps, actions, secrets and logs work as before.

**Will it slow my Mac down while I work?**
Not if you set `keep_memory_gib` and `keep_cpus`. faber starts new jobs only within the
share you give it, and pauses while macOS is short on memory.

**What happens if my Mac is asleep or off?**
Jobs wait in GitHub's queue until faber is back, or until GitHub gives up on them. faber
can keep the Mac awake while jobs run (`prevent_sleep = "while_busy"`).

**Can I use it for open-source or public repos?**
Not by default. faber refuses public repositories, because anyone can open a pull
request against them. Keep public CI on GitHub's runners.

**Does faber see my secrets?**
Yes, in the sense that they leave GitHub: a job's secrets go to its runner on your Mac,
as with any self-hosted runner. They live in that job's throwaway runner and are
deleted with it, but treat the Mac as a place that holds them. faber's own GitHub token
reaches only the repos you picked, with only the permissions the features you turned
on need.

**Does it work on Linux or Windows?**
macOS with Apple silicon today. Running faber on a Linux machine is in progress;
Windows is not planned.

**How do I remove it?**
Run `sudo faber uninstall`. It shows what it will remove and asks first. It keeps the
images you built, because they may be your only copy.

## Going deeper

- [docs/operating.md](docs/operating.md): the operator's guide. Which jobs faber will
  run and the settings that change that, the runner image and what to do when GitHub
  retires its runner, network access for jobs, configuring and updating a Mac, and
  reading the logs when something goes wrong.
- [docs/mirrors.md](docs/mirrors.md): the download mirror, a local cache that answers
  Git LFS and npm downloads, and how to turn it on.
- [docs/git-mirror.md](docs/git-mirror.md): the git mirror, a copy of each repo's
  history on your Mac, and the workflow step that lets checkout use it.
- [docs/autolimiter.md](docs/autolimiter.md): how faber decides how many jobs fit at
  once, from memory, CPU and disk, and why a job waits.
- [docs/adrs/](docs/adrs/): the design decisions, one record each, with the reasons and
  the options turned down.

## License

MIT. See [LICENSE](LICENSE).
