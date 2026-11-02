<p align="center">
  <img src=".github/assets/banner.svg" alt="faber: your own CI, on your own Mac. Just-in-time GitHub Actions runners, one job each, then gone." width="100%">
</p>

faber runs your GitHub Actions jobs on your own hardware. It watches the queued jobs of
the repos you opt in, and for each one that is genuinely yours it starts a fresh,
single-use runner. The runner takes exactly that one job and is then torn down, so no
state carries over from one job to the next.

> **Status:** faber is moving into this repository from its previous one, piece by
> piece. Commands below that name files not here yet will work once those files
> arrive.

## Why faber

**What it's for:** speed, warm caches, and control over the machine your CI runs on.
You get the throughput of hardware you already own, with images and toolchains you
choose.

**What it's not for:**

- **Untrusted code.** faber is not a sandbox. Fork pull requests and outside
  contributions stay on GitHub-hosted runners. See [Trusted code only](#trusted-code-only).
- **Saving money.** GitHub-hosted minutes are often free or cheap. faber trades them
  for speed and control, not for cost.
- **Hosting CI for others.** It serves one owner, on hardware that owner controls.
- **Replacing GitHub Actions.** faber provisions runners *for* Actions. Workflows,
  steps and logs all stay on GitHub.

## How it works

```
poll GitHub ─→ gate ─→ start a runner ─→ one job ─→ tear it down
```

1. **Discover.** faber watches private repos that carry a GitHub topic (`faber` by
   default). Add the topic to opt a repo in; remove it to opt out.
2. **Poll.** It asks GitHub for queued jobs with conditional requests. Nothing
   connects *in* to your machine.
3. **Gate.** Every job passes an independent trust check before anything starts.
   See [The gate](#the-gate).
4. **Admit.** faber starts a job only when the machine has room for it. It budgets
   memory and CPU, learns what each kind of job really uses, and parks the rest
   until there is space.
5. **Run.** It mints a just-in-time runner for that one job, inside a throwaway
   environment:
   - **macOS jobs** run in a per-job macOS VM.
   - **Linux jobs** run in a Docker container, on a Linux VM that faber owns. Each
     job gets its own network and is removed when it finishes.
6. **Clean up.** The runner, its container or VM, and its network go away. faber also
   reclaims disk from images it built itself, and never deletes anything it did not
   build.

A control panel at `http://127.0.0.1:8787` (localhost only) shows live state, recent
gate decisions, finished runs and faults, and has pause, resume and settings
controls. Open it with `faber dashboard` or from the menu-bar app.

## Trusted code only

> [!WARNING]
> **faber runs your own trusted code on your own machine.** It is not built to run
> untrusted code safely. Never point a workflow that can run a fork pull request or
> an outside contribution at faber's labels.

Two layers keep untrusted work off your machine.

### 1. Routing in your workflows (the primary control)

Each workflow decides, per job, where it runs. Fork pull requests, pull requests by
anyone you do not trust, and Dependabot go to GitHub-hosted runners. Everything else
targets faber:

```yaml
runs-on: ${{ ((github.event_name == 'pull_request' && (github.event.pull_request.head.repo.fork || !contains(fromJSON('["owner"]'), github.event.pull_request.user.login))) || github.actor == 'dependabot[bot]') && 'ubuntu-latest' || fromJSON('["faber","docker"]') }}
```

Replace `["owner"]` with the logins whose pull requests you would run on this
machine. The expression checks the pull request's **author**, not `github.actor`. A
maintainer who re-runs someone else's pull request becomes the actor, but the code is
still the author's.

### 2. The gate (defense in depth)

faber re-checks every job on its own and fails closed. It refuses:

- **Public repos**, by default, even when they carry the topic.
- **Trigger events outside its allow-list.** `pull_request_target`, `issue_comment`
  and `workflow_run` can never be enabled.
- **Code by strangers.** faber reads who wrote the code a job runs: a pull request's
  author, or the authors inside a merge group. Code by anyone who is not the owner,
  an organization member or a collaborator is refused, and so is code faber cannot
  read.
- **Dispatches made with a job's own token**, unless you turn them back on.

Every gate setting can only **narrow** what faber accepts:

```toml
[gate]
allowed_events = ["push", "pull_request", "workflow_dispatch", "schedule", "merge_group"]
allow_public_repos = false
```

There is deliberately no setting that allows fork pull requests. These settings live
only in your own `config.toml`; a watched repo cannot change them.

### A container is not a boundary

A Docker job that may use `docker` gets a privileged Docker-in-Docker daemon, and
anything that can reach that daemon can become root on the Linux VM. So Docker
access is a grant, not a default:

- Set `dind = false` on profiles whose jobs never run `docker`. Most tests and
  linters don't need it.
- With `[host] dind_requires_label = true`, a job gets a daemon only when it asks for
  one with `dind` in its `runs-on`.
- If a job only needs a database, use a [declared service](#services-without-docker)
  instead.

## Services without Docker

A job that needs Postgres, Redis or Mailpit can ask for it with a label, and faber
runs the service next to the runner. No Docker daemon is involved.

```yaml
jobs:
  test:
    runs-on: [faber, docker, faber.postgres-16, faber.redis]
    steps:
      - uses: actions/checkout@v4
      - run: npm test   # reads DATABASE_URL and REDIS_URL
```

You decide in faber's config which services and versions a job may ask for:

```toml
[services.postgres]
versions = "16, 17"
image = "postgres:{version}-alpine"
extensions = ["pg_trgm"]

[services.redis]
image = "redis:7-alpine"
```

The job reaches each service by host name and receives its address in environment
variables:

| Service | Host | Variables |
|---|---|---|
| Postgres | `postgres` | `DATABASE_URL`, `PGHOST`, `PGPORT`, `PGUSER`, `PGPASSWORD`, `PGDATABASE` |
| Redis | `redis` | `REDIS_URL` |
| Mailpit | `mailpit` | `MAIL_SMTP_URL`, `MAIL_API_URL` |

Each job gets its own Postgres database and role. Redis and Mailpit run one container
per job. A job that asks for a service this host does not offer gets no runner, and
the dashboard says why.

## Platforms

| OS | Status |
|---|---|
| macOS | Supported. Runs as a system service under its own hidden accounts. |
| Linux | In progress. Docker jobs already work; the system service is being finished. |
| Windows | Not supported. |

## Install

faber is built from source. On macOS:

```sh
gh repo clone rejifald/faber ~/.faber/src
cd ~/.faber/src
./deploy/install.sh        # the daemon
./deploy/install-app.sh    # optional: the menu-bar app
```

The installer builds faber, then hands off to `sudo faber setup`. Setup creates
faber's own hidden accounts and installs the system service. It shows its plan and
asks before changing anything. Run `faber setup` without `sudo` to see the plan
without applying it.

## First run

A fresh install is running but watching nothing. Four steps turn it into CI:

1. **Give faber a token.** `faber auth link` opens a GitHub page with a fine-grained
   token already filled in, with only the permissions your config needs. Pick the
   repos this host should serve, create the token, then hand it over:

   ```sh
   faber auth link
   sudo faber auth set
   ```

2. **Opt a repo in:**

   ```sh
   gh repo edit owner/demo --add-topic faber
   ```

3. **Route the work.** Add the [`runs-on` expression](#1-routing-in-your-workflows-the-primary-control)
   to that repo's workflows.

4. **Push something,** then watch it with `faber dashboard` or `faber status`.

### The runner image

Linux jobs need a runner image that follows faber's just-in-time contract. One ships
in `docker/runner`:

```sh
docker build -t faber-runner:local docker/runner
```

A local tag is all you need; nothing has to go to a registry. Add whatever your jobs
need (a JDK, browsers, Python) to its Dockerfile and rebuild.

GitHub retires old runner versions on announced dates. faber reads GitHub's warnings
from finished jobs and shows a countdown on the dashboard and in the menu bar. When
it appears, rebuild the image with a newer `RUNNER_VERSION`, point the profile at the
new tag, and restart faber.

## Configure

Everything faber reads is in one `config.toml`:

- macOS: `/Library/Application Support/faber/config.toml`
- Linux: `/var/lib/faber/config.toml`

The installer writes a commented starting point once and never touches it again. You
can also edit it in the **Settings** tab of the control panel or the menu-bar app.
That form checks your input and tells you which changes need a restart.

On a machine someone also works on, declare what CI must leave free:

```toml
[host]
keep_memory_gib = 16
keep_cpus = 4
```

faber then plans CI within the rest, and holds back new jobs while the machine is
under memory pressure.

## Updating

A host can follow releases automatically. It's off by default:

```toml
[update]
enabled = true
```

A following host verifies each release before it downloads a binary, waits a
maturity period (24 hours by default) before installing, swaps the binary only when
it's idle, and rolls back a version that crashes on start.

## When something goes wrong

```sh
faber status    # the last 24 hours: tallies, faults, failed runs and their causes
```

faber keeps an append-only log, `audit.jsonl`, in `/Library/Logs/faber/`. It has one
line per gate decision, finished run and fault, and each failure carries its `cause`.
`faber status` reads it even when the daemon is stopped. Narrow it with
`--since 90m`, `--repo owner/demo` or `--json`.

## Develop

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -- status
```

Requires Rust 1.96 or later.

## License

MIT
