---
title: Jobs declare services, and faber runs them without dind
type: adr
status: proposed
date: 2026-10-19
owner: rejifald
---

# ADR-0046: Jobs declare services, and faber runs them without dind

## Status

proposed (2026-10-19). Decision recorded ahead of implementation, following the
precedent of ADR-0007 and ADR-0010. Takes up mitigation 2 of
[issue #43](https://github.com/rejifald/faber-legacy/issues/43), "declared service
containers instead of a general-purpose daemon", which ADR-0013 left open.
[ADR-0062](0062-an-s3-store-is-a-per-job-versity-gateway-with-a-secret-made-per-job.md)
amends A2: a per-job table's credentials may be generated per job (`{secret}`).

## Context

A job that needs a Postgres, a Redis or a Mailpit gets it today through GitHub's
`services:` block. The runner implements `services:` by driving a Docker daemon,
so every such job needs `dind` (ADR-0013, and with the 2026-10-17 amendment, a
`dind` label as well). That costs the job four things it does not otherwise need:

- **An empty image store.** Each dind has its own `/var/lib/docker` on a fresh
  volume, so every job pulls `postgres` again. Nothing is cached across jobs.
- **A daemon start.** faber waits for dind to answer, up to
  `DIND_READY_TIMEOUT` (60 s), before the runner even exists.
- **A second container's reservation.** The autolimiter reserves and caps the
  dind half of `Limits` for the job. That memory is held for a daemon that only
  ever runs a database.
- **Host-root exposure.** A granted dind is root in the shared Linux VM
  (ADR-0013, ADR-0026). A job that only wanted a database is given the widest
  privilege faber has.

Meanwhile every runner container already runs on one long-lived Docker Engine,
in faber's Linux VM (ADR-0026). Images pulled there stay there. faber can start
a container next to a runner as easily as it starts the runner.

CI throughput is the bottleneck on the hosts faber runs. Service jobs are a
large share of the slowest ones.

## Decision

### 1. A job declares a service with a need label

In ADR-0010's namespace, with the major version where the service has one:

```yaml
jobs:
  test:
    runs-on: [faber, docker, faber.postgres-16, faber.redis]
    steps:
      - uses: actions/checkout@v4
      - run: npm test   # reads DATABASE_URL and REDIS_URL
```

No `services:` block and no `dind`. A job that declares services and never runs
`docker` itself needs no daemon at all.

A label is the right carrier for the same reason ADR-0010 gives: *which runner
may serve this job* is only expressible where GitHub reads it. A label carries no
values, which is fine, because the job chooses nothing but the service (§4).

### 2. The operator decides what may be asked for

In faber's own config, which no repo can reach:

```toml
[services.postgres]
versions = "16, 17"
image = "postgres:{version}-alpine"     # or an operator-built image with PostGIS
extensions = ["postgis", "pg_trgm"]
memory_mib = 2048                       # reserved while the shared server runs
idle_stop_secs = 900                    # stop it after 15 min unused

[services.redis]
image = "redis:7-alpine"
memory_mib = 256                        # per job

[services.mailpit]
image = "axllent/mailpit:v1.21"
memory_mib = 128                        # per job
```

- **No table, no service.** A job asking for `faber.postgres-16` on a host
  without `[services.postgres]`, or for a version outside `versions`, is refused
  and gets no runner, as in ADR-0010 when a declared need is refused. The refusal
  is listed in `/api/state` as `service_refused`, with the profile and the label,
  and shown on the dashboard.
- **A runner carries a service label only if faber provides that service.** As
  in ADR-0010 §3: faber never registers a runner whose labels promise something
  nothing verified.
- **The image is the operator's choice, never the job's.** A label names a
  service and a version, and the image comes from this table.

This is not a widening ADR-0025 forbids `config.toml` to make. A service
container runs inside the Linux VM, behind the same pf wall as the runner, and
reaches nothing a runner could not already reach.

### 3. Postgres is shared; everything else is per job

**Postgres: one server per major version, one database per runner.** faber keeps
`faber-svc-postgres-16` running in the Linux VM, started on first demand. It is
tuned for throwaway data: `fsync=off`, `synchronous_commit=off`,
`full_page_writes=off`. Postgres is the service that has real tenant isolation
(roles, database ownership, `CONNECT` privileges) and also the one whose start
costs the most, so it is the one worth sharing.

**Redis and Mailpit: one container per job.** Neither isolates tenants in a way
tests respect. `FLUSHALL`, `KEYS *`, and "read the latest email" reach across
every tenant on the server, and a test that does them is not wrong. faber starts
the container on the job's network from the VM's cached image, which takes well
under a second, so sharing would buy little and cost flaky cross-job failures.

The per-job containers are torn down with the runner, like everything else in a
job (`docker.rs`'s header). They are faber's, labelled `org.faber.kind=service`
and `org.faber.id=<runner>`, so `sweep` reaps a leaked one by label.

MySQL fits the shared shape and is left for when a job needs it.

### 4. faber names the database and the credentials

Per **runner**, not per job: no GitHub API ties a JIT registration to a job id
(ADR-0009). A runner serves one job, so the two come out the same.

When it provisions a runner that carries `faber.postgres-*`, faber runs, as the
server's superuser:

```sql
CREATE ROLE faber_r_<id> LOGIN CREATEDB PASSWORD '<random>';
CREATE DATABASE faber_r_<id> TEMPLATE faber_tpl OWNER faber_r_<id>;
REVOKE CONNECT ON DATABASE faber_r_<id> FROM PUBLIC;
```

- **`CREATEDB`** is there because test runners create their own databases.
  Django creates `test_<name>`, and Rails may run `db:create`. A database a job
  creates is owned by its role, which is what teardown finds it by.
- **`REVOKE CONNECT … FROM PUBLIC`** is not optional. Postgres grants `CONNECT`
  on every database to every role by default, so without it one job could open
  another's database. It covers only the database faber creates. A database
  the job creates for itself gets Postgres's default `CONNECT` for `PUBLIC`
  again, because no default privilege or event trigger reaches `CREATE
  DATABASE`. Another job could connect to it with its own role if it guessed the
  name. That is a residual inside the trusted-code model (Consequences), and it
  is accepted rather than patched with a periodic revoke that would only narrow
  it. The owner chose this over dropping `CREATEDB`, which would break Django's
  test runner and per-worker test databases (2026-10-19).
- **`faber_tpl`** is created once per server, with the operator's `extensions`
  already installed. Untrusted extensions such as PostGIS need a superuser to
  install, so a job cannot add them. The template is how it gets them anyway.
  Creating from a template is also faster than creating an empty database and
  migrating extensions into it.

The runner container's environment carries the result:

```text
DATABASE_URL=postgres://faber_r_<id>:<password>@postgres:5432/faber_r_<id>
PGHOST=postgres  PGPORT=5432  PGUSER=…  PGPASSWORD=…  PGDATABASE=…
REDIS_URL=redis://redis:6379/0
MAIL_SMTP_URL=smtp://mailpit:1025   MAIL_API_URL=http://mailpit:8025
```

A profile's own `env` wins over these, as it already does for `DOCKER_HOST`
(ADR-0013 amendment).

**The job does not declare names or credentials.** Two concurrent jobs of one
repo would declare the same name and collide on the shared server. faber would
then either rename the database, which makes the declared name a lie, or
serialize the repo's jobs, which costs the throughput this ADR is for. Nothing is
added to `RepoConfig`, whose strict, PR-reachable schema stays as it is.

### 5. Services reach the job by name, on the job's own network

faber attaches the shared Postgres to each job's existing private network with
`docker network connect --alias postgres`, and detaches it at teardown. A per-job
Redis or Mailpit is started on that network with `--network-alias redis` /
`mailpit`.

- **No shared network.** Putting every runner on one services network would put
  every runner in reach of every other. Connecting the server to each job's
  network instead gives it one interface per job. A container does not route
  between its interfaces, so jobs stay apart.
- **No new network per job, and no new addresses from the pool.** The job's
  network already exists (ADR-0020), and the server takes one address on it.
  The server itself needs one network to start on, because a container started
  with `--network none` cannot be attached to another later. That is
  `faber-svc`, one host-wide `--internal` network that no runner joins.
- **A leaked attachment blocks `network rm`.** `sweep` detaches the server
  (`network disconnect -f`) from a network it is reaping before removing it.
- **Services do not answer on `localhost`.** A job reaches them by name or
  through the injected URLs. Workflows that hard-code `localhost:5432` change
  those lines when they move over. Making `localhost` work would take a
  forwarder container per job in the runner's network namespace, and would take
  the service ports inside the job, which collides with a job that also runs its
  own Postgres through dind. The owner chose names and env only (2026-10-19).

### 6. Teardown is generated inside the server, and faber reads only exit status

The shared server is reachable by every job using it, so what it prints is
input a job could shape. ADR-0025 keeps `_faber` from parsing that. Teardown
therefore runs as a script fed to `psql` through `docker exec`, which generates
its own statements and executes them with `\gexec`:

```sql
SELECT format('DROP DATABASE %I WITH (FORCE)', datname)
  FROM pg_database
 WHERE datdba = (SELECT oid FROM pg_roles WHERE rolname = 'faber_r_<id>') \gexec
DROP ROLE IF EXISTS faber_r_<id>;
```

faber supplies `<id>`, which is its own, and reads only the exit status. At startup,
adoption does the same for roles named `faber_r_*` whose runner no longer
exists, with the live set passed in. A role outlives its runner only when
faber did not get to run its teardown, which means faber stopped, so the next
start is where it is found. A database name a job chose is never
interpolated by faber.

### 7. The shared server is faber's own resource

- **Labelled `org.faber.kind=service-server`**, not `service`. The per-job
  containers carry `service` and an `org.faber.id`, and `sweep` reaps them like
  any other leaked job resource. The server, its volume and `faber-svc` have no
  job id, and `sweep` never touches them. After a restart, `adopt` finds it by
  label (ADR-0011) and keeps it, and keeps the databases of adopted runners.
- **Its data lives on a volume faber created**, labelled the same way. faber may
  delete it, because it can rebuild it from nothing but config: the data is
  throwaway by design. This is the same licence as ADR-0008's variants and
  ADR-0035's mirrors.
- **The image is not faber's.** It is pulled, not built, and the never-reap rule
  of ADR-0007 holds for it like any other image faber did not build.
- **Its memory is reserved only while it runs.** `memory_mib` is taken out of the
  Docker budget while the server is up, not charged to any job. It is also the
  server's `--memory` cap. A job's database load does not show in its own
  cgroup, so the learner cannot size it per class, and should not try.
- **A server that dies takes its in-flight jobs with it.** Those failures are
  faber's, and ADR-0003's retry covers them. faber restarts the server on the
  next demand, empty.
- **It stops when idle.** After `idle_stop_secs` (900 unless set) with no runner
  using it, faber stops the server and releases its reservation. The first job
  after a quiet spell pays a cold start of a few seconds. A busy host never
  reaches the timeout, so throughput is unaffected when it matters. The owner
  chose this over keeping the server always on (2026-10-19): an idle 2 GiB held
  around the clock is one or two jobs fewer at once, for no one's benefit.
- **A version removed from `versions`** stops its server once its last runner is
  torn down, without waiting for the timeout.

### 8. Per-job services are sized by the operator, not learned

A per-job Redis or Mailpit runs with its table's `memory_mib` as its cap, and
that amount is added to the job's reservation. These are small and steady, and
learning them would cost a fourth sampler per job for a few hundred MiB.

### 9. It ships off and changes nothing on a host that did not ask

A host without `[services.*]` provisions exactly as before. A job that keeps its
`services:` block and its `dind` label keeps working. Moving a workflow over is
the repo's change: drop `services:` and `dind`, add the labels, and read
`$DATABASE_URL` instead of a hard-coded `localhost` URL.

The outcome audit line gains `services: [postgres-16, redis]`, absent when the
runner had none.

### 10. Details the code settled

These follow from how faber already works, and are recorded so the
implementation has one source:

- **Label forms.** A table with `versions` accepts only the versioned label
  (`faber.postgres-16`). A table without it accepts only the bare one
  (`faber.redis`). Any other form is refused, with the form it expected.
- **`[needs]` does not judge service labels.** `needs::evaluate` checks a
  version against the runner image's `serves`, so it would refuse every service
  label. It skips the service tools, and `postgres`, `redis` and `mailpit` are
  reserved names in `[needs]`. Services are decided next to `dind`, whether or
  not `[runner_image]` is on.
- **Only Docker profiles provide services.** A service label on a native or
  macOS-VM profile is refused, not ignored.
- **Postgres `versions` are exact majors, 13 or later.** `DROP DATABASE … WITH
  (FORCE)` needs 13.
- **The role name comes from the claim id.** It is `faber_r_<job>_<ts>`, within
  Postgres's 63-character limit, and never carries the instance suffix.
- **No secret goes on an argv.** The runner gets the service variables as
  name-only `-e NAME`, with the values in the Docker CLI's own environment, the
  way `RUNNER_JITCONFIG` already travels. The role's password reaches `psql` on
  its stdin. A profile's `env` still wins, because it is written after them.
- **Images are pulled under the existing pull rule.** faber pulls a service
  image explicitly, under `pull_unqualified_images` (#156), and runs it with
  `--pull=never`, as it does runner images.
- **A running orphan is reaped.** `sweep`'s container listing covers exited
  containers only, so it gains a pass for running per-job service containers
  whose runner is gone, as it has for dind.
- **Changing `[services]` from the control socket is a trust widening**
  (ADR-0028). Adding a table, changing an `image` or adding `extensions` is
  flagged the way `[needs]` changes are, because that image runs on every
  service job's network.
- **The services an outcome names survive a restart** through an optional claim
  label, `org.faber.services`. It is not one of the labels adoption requires,
  so containers started before this release stay adoptable.

## Consequences

- **A service job stops needing dind**, so it stops paying for a daemon start, a
  cold image store and a second container's reservation. It also stops holding
  host root in the Linux VM, which completes the job ADR-0013 started for the
  jobs that wanted a database and nothing else.
- **Images are pulled once per VM**, not once per job.
- **Postgres per job drops to a `CREATE DATABASE … TEMPLATE`**, which is
  milliseconds, instead of a server start and a readiness wait.
- **Jobs share a Postgres process.** A job that exhausts connections, fills the
  disk or crashes the server hurts every job on it. `max_connections`, the
  volume and the restart path have to be sized for concurrency × pool size, not
  for one job.
- **Isolation between jobs on the server is Postgres's, not a container's.** It
  holds against an honest test suite. It does not hold against a job that
  exploits Postgres to become superuser. That is inside faber's trust model,
  trusted code only, and this ADR does not change it.
- **Workflows change.** faber cannot read `services:` and honour it, because the
  runner acts on that block before any step runs. Each repo moves over by hand.

## Alternatives considered

**Share everything.** Redis split by DB index or by ACL with a key prefix,
Mailpit by a tag. This saves a few hundred MiB per job and makes tests that do
ordinary things (`FLUSHALL`, "the latest email") fail depending on what else is
running. Rejected by the owner (2026-10-19).

**Per-job containers for everything, Postgres included.** This is the simplest
design, and it already removes dind and the repeated pulls, which is most of the
win. It keeps a Postgres start and readiness wait, two to four seconds, on every
job, and a Postgres-sized reservation per job instead of one per host. The
per-job path in §3 is what this design would have been, so falling back to it
for Postgres stays cheap if sharing proves troublesome.

**The job declares the database name and credentials** in
`.faber/config.toml`. This keeps hard-coded URLs working, at the cost of
collisions between concurrent jobs and a wider `RepoConfig` (§4). Rejected by
the owner (2026-10-19).

**A pull-through registry cache for dind.** This would fix the cold image store
and nothing else. The daemon start, the reservation and the host-root exposure
remain.

## Amendment (2026-10-29): a per-job service is a table the operator writes

§3 made Postgres the one shared service and every other one a container per
job, but the code kept the per-job set closed: `redis` and `mailpit` were enum
arms, each with its URLs hard-coded in faber. The next service a watched repo
needed, an S3-compatible object store, would have been a third arm naming one
vendor's product inside faber. That is the wrong layer. What a per-job service
*is* (an image, some arguments, the variables the job reads, and a way to tell
it is up) is operator knowledge, so it moves into the operator's table. The
capability is the table's name; the product is just its image.

### A1. Any name the operator declares is a per-job service

```toml
[services.s3]                                   # a job asks with faber.s3
image      = "versity/versitygw:v1.8.0"
args       = ["--port", ":7070", "--health", "/health", "posix", "/data"]
env        = { ROOT_ACCESS_KEY = "ci", ROOT_SECRET_KEY = "ci-secret" }
job_env    = { S3_ENDPOINT = "http://s3:7070", AWS_ACCESS_KEY_ID = "ci",
               AWS_SECRET_ACCESS_KEY = "ci-secret", AWS_REGION = "us-east-1" }
health_cmd = "wget -qO- http://127.0.0.1:7070/health"
memory_mib = 256
```

- **`image`, `memory_mib`**: as before.
- **`args`**: appended after the image, so they are the container's command.
  They travel on an argv, so they never carry a secret. That is what `env` is
  for.
- **`env`**: the service container's own environment. It travels as name-only
  `-e NAME` with the values in the Docker CLI's environment, as §10 already does
  for the runner.
- **`job_env`**: what the runner container gets, the counterpart of
  `REDIS_URL` in §4. A profile's `env` still wins over it.
- **`health_cmd`**: see A3.

`postgres` stays the built-in shared server of §3, with its own keys, and is
the one name a per-job table may not take. `redis` and `mailpit` become
ordinary per-job tables with built-in defaults: a table that leaves out
`job_env` gets the variables §4 lists, and a table without `health_cmd` is not
waited for (A3). A host that wrote either table under the original ADR keeps
the same behaviour.

**The name is a host name on the job's network**, because it becomes the
container's alias (§5). So it must be a DNS label: lowercase ASCII letters and
digits, starting with a letter, at most 32 characters. It must not be the name
of anything faber already puts on a job's network (`dind`) or of a `[needs]`
tool. A per-job service is only ever asked for by its bare label
(`faber.s3`), so a versioned label (`faber.playwright-1.61.1`) is never read as
one, even when a table shares its tool's name. It stays a `[needs]` label, which
is what keeps a runner-image tool from being captured by a service table. faber
cannot check those names when it reads the config, because a runner image's
`serves` comes from each repo's lockfile at run time.

### A2. faber does not judge what the variables mean

faber checks only that `env` and `job_env` names are environment names
(`[A-Z_][A-Z0-9_]*`) and that no value carries a NUL or a newline. A `job_env`
name may not be one faber sets itself: `DATABASE_URL`, the `PG*` set, or
anything under `RUNNER_`, `GITHUB_`, `ACTIONS_` or `FABER_`.

**Neither map may name a variable the Docker CLI reads.** Both reach the
container through the environment of the `docker` process faber runs (§10), so
`DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_CONFIG` or any other name faber passes
the CLI (`PATH`, `HOME`, `TMPDIR`, `LANG`, anything under `DOCKER_` or
`BUILDKIT_`) would steer faber's own call rather than the service. That is a
choice of daemon or credentials, which ADR-0026 keeps out of config. Such names
are refused in both maps.
Two tables may not export the same name, because a job declaring both would get
whichever was written last.

**Credentials in a per-job table are fixed values, not generated ones.** The
container sits on one job's private network and nothing else can reach it, so a
fixed key shared by every job's copy grants nothing a job did not already have.
Generating a secret per job would add code to protect a boundary the network
already draws. If a shared server other than Postgres ever exists, it needs
generated credentials, and that is a new decision.

**Initialization is the job's.** Buckets, queues and topics are created by the
job, with the credentials in `job_env`. faber starts the container and stops
there, as it does for Redis.

### A3. A job starts only after its services are healthy

A service that is still starting looks, to the job, exactly like one that is
broken. faber therefore waits for each per-job service before it starts the
runner:

- With `health_cmd`, faber passes it as `--health-cmd`, along with a start
  interval of one second, so the first check runs at once and not after Docker's
  thirty-second default. The start interval needs Docker Engine 25 or later, so
  a table with `health_cmd` needs that engine as well.
- Without it, faber passes no health flag and does not wait. That includes an
  image's own `HEALTHCHECK`, which runs at the image's own interval and could
  hold a job for as long as that interval. A table written before this amendment
  therefore starts its container with exactly the arguments it had before, on
  any engine.

faber starts every per-job service first and then waits for those with a
`health_cmd`, up to sixty seconds. It reads only Docker's health status (`starting`, `healthy`,
`unhealthy`), an enum the daemon writes, and never the check's output, which
the service, and so a job, could shape (ADR-0025). A service that turns
`unhealthy` or misses the deadline fails the provision, like a container that
fails to start, and the outcome names it.

### A4. What changes in trust and in names

- **A change to `image`, `args`, `env` or `health_cmd` is a trust widening**
  (§10, ADR-0028). Each one changes the code that runs on the network of every
  job that declares the service. A change to `job_env` or `memory_mib` is
  routine, since a job can set its own environment anyway.
- **Per-job containers are named `faber-svc-<name>-<runner>`**, not
  `faber-<name>-<runner>`. With names now chosen by the operator, the old form
  could collide with names faber gives its own containers. A container started
  under the old name by the previous release is reaped by its labels, by
  `sweep`'s pass for running per-job services whose runner is gone (§10), so the
  rename needs no migration.
- **Adoption accepts any service name in faber's form** in the
  `org.faber.services` label, not only the three it used to know. A restart
  must not orphan a service only because its table was edited in between.
