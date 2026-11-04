---
title: An S3 store is a per-job Versity Gateway, with a secret made per job
type: adr
status: proposed
date: 2026-11-01
owner: rejifald
---

# ADR-0062: An S3 store is a per-job Versity Gateway, with a secret made per job

## Status

proposed (2026-11-01). Builds on
[ADR-0046](0046-jobs-declare-services-and-faber-runs-them-without-dind.md) and
amends its A2 in one respect: a per-job table's credentials no longer have to be
fixed values in config. Everything else in A1–A4 stands.

## Context

A watched repo's pull-request end-to-end gate, and two of its nightly e2e jobs,
start an S3-compatible store in Docker from their tests (MinIO, through
Testcontainers). Everything else those jobs need is already a faber service
(Postgres, Redis, Mailpit). The S3 store is the one thing that still needs
`dind`.

The host they run on now has `dind = false`: a privileged per-job daemon is the
most expensive and the widest grant faber makes (ADR-0013, ADR-0046's Context),
and the owner is cutting CI cost. Until an S3 store is a faber service, those
jobs get no runner on that host, which the owner accepted.

On 2026-11-01 the owner chose a faber service over the two alternatives (a
server binary baked into that repo's runner image, or an in-process fake in its
tests).

ADR-0046's amendment already made any `[services.<name>]` table a per-job
service, and used an S3 store as its example. That leaves three things open:

1. **Which server.** The amendment's example named one, but nothing chose it.
2. **Credentials.** A2 made a per-job table's credentials fixed values in
   config. The owner wants them generated per job and never written into
   config. There is a concrete reason as well as a preference: `config.toml` is
   the file the settings page rewrites and the dashboard serves (see the
   `[registry_auth]` comment in `src/config.rs`). A fixed secret there is
   shown to anyone who can read the dashboard, and is copied into every
   example made from that file.
3. **Buckets**, including ones a test serves unauthenticated: the job puts a
   reverse proxy in front of the store and expects anonymous reads through it.

## Decision

### 1. The server is Versity Gateway

`versity/versitygw`, pinned to a release tag (`v1.10.0` at the time of writing;
its multi-platform index is
`sha256:89cad13cc981986ee284cbafea4613f9533ce6fd5f37a2c96c4c4fb8adb3a316`),
run with its POSIX backend on the container's own filesystem:

```toml
[services.s3]
image      = "versity/versitygw:v1.10.0"
args       = ["--port", ":7070", "--health", "/health", "posix", "/tmp/vgw"]
env        = { ROOT_ACCESS_KEY = "faber", ROOT_SECRET_KEY = "{secret}" }
health_cmd = "wget -qO- http://127.0.0.1:7070/health"
memory_mib = 256

[services.s3.job_env]
S3_ENDPOINT           = "http://s3:7070"
AWS_ACCESS_KEY_ID     = "faber"
AWS_SECRET_ACCESS_KEY = "{secret}"
AWS_REGION            = "us-east-1"
```

- **`posix /tmp/vgw`** is the shape the vendor's own compose file uses, and
  `/tmp/vgw` is a directory the image creates. The data lives in the
  container's writable layer and goes with `docker rm -v`, so nothing outlives
  the job.
- **`--health /health`** turns on an unauthenticated `GET /health`. The image is
  Alpine-based, so busybox `wget` is there for `health_cmd`, and A3's wait
  applies.
- **The table is the operator's.** This is the recipe faber's docs give, not
  code: nothing in faber names Versity, as A1 intended. Another S3 server is
  another table.

Why Versity Gateway, and not MinIO:

| | Versity Gateway | MinIO (community) |
|---|---|---|
| Licence | Apache-2.0 | AGPL-3.0 |
| Maintained | releases every few weeks (v1.7.0 2026-07-15 … v1.10.0 2026-10-22) | repository archived read-only (April 2026); maintenance mode since December 2025 |
| Images | published by the vendor for every release, amd64 and arm64 | community images stopped in October 2025; the last ones receive no fixes |
| Anonymous reads | bucket policy with `"Principal": "*"`, documented and implemented (§3) | `mc anonymous set`, through a client the job would have to install |
| Fit for tests | single static binary, starts in well under a second | larger, more features than a test needs |

MinIO is what the watched repo uses today, so it would have been the smaller
change for that repo's tests. But faber would be pinning an unmaintained image
on every host that declares the table, and a service image runs on the network
of every job that asks for it (A4). A community fork republishes images, under
a different maintainer whose release cadence is not yet established. It is not
taken. The S3 API the tests use is the same on both servers, so the repo's
change is its configuration, not its test logic.

Not evaluated in depth: SeaweedFS, Garage, Zenko CloudServer, RustFS. Any of
them is a table away if Versity Gateway proves wrong.

### 2. Credentials are generated per job: `{secret}`

A value in a per-job table's `env` or `job_env` may contain `{secret}`. When
faber starts that table's container for a job, it draws one fresh value (32 hex
characters from `/dev/urandom`, as for Postgres roles) and puts it in place of
every `{secret}` in both maps. The container's server and the job's client
therefore hold the same credential, and no two jobs hold the same one.

- **It is never stored.** The value exists in the `docker` CLI child's
  environment, the service container's environment and the runner's, and
  nowhere in config, logs, the dashboard or the audit line. A restart does not
  need it back: an adopted runner keeps the container it already has.
- **It never reaches an argv.** `env` and `job_env` already travel name-only
  (`-e NAME`, ADR-0046 §10), so the substitution happens in the values faber
  hands the CLI. `args` and `health_cmd` are an argv, so `faber check-config`
  and a settings save refuse `{secret}` in either, and boot leaves such a table
  out of the policy with a warning, as for any other table error.
- **One secret per job and service.** Two tables a job declares get two
  different secrets. A table that needs two secret values (a key and a
  separate signing key) is not served by this. None does today, and adding
  `{secret:<name>}` later is backwards compatible.
- **The access key ID is not a secret.** It is the account name the job signs
  as, so the recipe leaves it fixed (`faber`), visible in config.
- **Fixed values still work.** A table written under A2 with fixed credentials
  is unchanged. Generated is the recommended form; fixed is no longer the only
  one.

This is the one part of A2 that changes. A2's argument still holds as far as it
goes: the container is on one job's private network, so a fixed key shared by
every copy grants no access across jobs. The change is about where the secret
is written down (§ Context, point 2), not about the network boundary.

### 3. Buckets are the job's, public ones included

faber creates no bucket. The job does, in its own setup, signed with the
credentials in `job_env`, as A2 already says for initialization in general. This
keeps faber out of the S3 API, which it would otherwise have to speak, or reach
through a client the image does not ship, and keeps bucket names a test's
concern.

A bucket a test serves unauthenticated is made public by the job, through the
standard S3 call:

```json
{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": "*",
    "Action": ["s3:GetObject"],
    "Resource": ["arn:aws:s3:::assets/*"]
  }]
}
```

applied with `PutBucketPolicy` to `assets`. Versity Gateway matches a request
with no `Authorization` header and no presigned query against the bucket's
policy (`s3api/middlewares/public-bucket.go` at v1.10.0), so a plain
`GET http://s3:7070/assets/<key>`, which is what the job's reverse proxy sends,
is served. It refuses anonymous bucket listing across the server, bucket
creation and ACL changes whatever the policy says.

**Path style only.** The bucket is in the path (`http://s3:7070/<bucket>/<key>`).
Virtual-hosted style needs `<bucket>.s3` to resolve on the job's network, and
nothing puts those names there. A job sets its client's path-style option
(`forcePathStyle` in the JavaScript SDK, `addressing_style = "path"` in boto3).

### 4. One store per job

As for Redis and Mailpit (ADR-0046 §3). Bucket names are global to a server, so
two jobs sharing one would collide on `CreateBucket` and see each other's
objects, and a test that empties or deletes "its" bucket would break the other
job. Isolating them would need an account per job and bucket names prefixed per
job, which the job's tests do not know to use. A per-job container starts from
the VM's cached image in well under a second, so sharing would save a few
hundred MiB and cost flaky cross-job failures.

`memory_mib = 256` is the container's cap and is added to the job's reservation
(ADR-0046 §8). A test that stores large objects may need more. The operator sizes it.

### 5. It ships off

A host without a `[services.s3]` table refuses `faber.s3` and gives the job no
runner, with `service_refused` in `/api/state`, as for any undeclared service
(ADR-0046 §2). No host's behaviour changes until its operator adds the table.

## Origin of the image (the no-Russian-origin rule, ADR-0042)

Checked:

- **The image and its source.** The Docker Hub repository `versity/versitygw`.
  The v1.10.0 image's OCI labels name its source as
  `github.com/versity/versitygw` at revision `72f75bd`, licence Apache-2.0. Its
  build history is an Alpine 3.24.3 minirootfs, the Go binary built from that
  repository, and the vendor's `docker-entrypoint.sh` (read in full: it only
  assembles `versitygw` arguments).
- **The vendor.** The GitHub organisation `versity` is Versity Software, Inc.,
  which business directories place in the US; they also name its co-founder
  and CEO.
- **Sanctions.** OpenSanctions found nothing for "Versity" that is Versity
  Software (the only hits are an unrelated French company), and nothing for
  its named founder.

Not checked, and so not a clean verdict:

- the Ukrainian register (its search refused automated access),
- the Russian company register, for the brand and for the named person (not
  reachable from the session that wrote this),
- the US state company register (behind a CAPTCHA),
- the Go modules compiled into the binary, one by one.

Nothing found points to a Russian origin. Before this ADR is accepted, the owner
completes the register checks above, or records why they are not needed.

## Consequences

- **The watched repo's S3 jobs can run without `dind`** once its host declares
  `[services.s3]`. The repo's workflows move over as ADR-0046 §9 describes: add
  `faber.s3`, drop the Testcontainers MinIO, read `S3_ENDPOINT` and the AWS
  variables, use path style, and create buckets (and the public policy) in
  setup.
- **No S3 secret is written into faber's config**, and the recipe faber
  documents has none to copy.
- **Any per-job service can use `{secret}`.** It is not specific to S3.
- **The generated secret is visible to `docker inspect`** on faber's own Docker
  socket, which only `_faber` can reach (ADR-0026), and to the job, which is
  meant to have it.
- **Versity Gateway is not MinIO.** A test that drives MinIO's own admin API
  (`mc admin`, MinIO-specific headers) has no counterpart. The watched repo's
  tests use the plain S3 API.
- **Not yet verified on a running host:** the recipe in §1 end to end inside
  faber's Linux VM. In particular, the POSIX backend keeps object metadata and
  bucket policies in extended attributes, and these must work on the
  container's overlay filesystem. If they do not, `posix --sidecar <dir>` keeps
  them in files instead, and the recipe changes to that.
