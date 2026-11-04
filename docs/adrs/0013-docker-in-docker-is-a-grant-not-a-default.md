---
title: Docker-in-Docker is a grant, not a default
type: adr
status: accepted
date: 2026-09-03
owner: rejifald
---

# ADR-0013: Docker-in-Docker is a grant, not a default

## Status

accepted (2026-09-03). Implements mitigation 1 of
[issue #43](https://github.com/rejifald/faber-legacy/issues/43). The deeper mitigations
in that issue — declared service containers, rootless buildkit, a VM per job —
are untouched and remain open.

## Context

Every Docker-backed job faber ran got the same sandbox: a private network, a
`--privileged` docker-in-docker daemon, and an unprivileged runner container
pointed at that daemon with `DOCKER_HOST=tcp://dind:2375`.

The unprivileged runner reads like a boundary and is not one. dind runs with
`DOCKER_TLS_CERTDIR=` (empty), so its API is plain TCP on 2375 with no TLS and no
authentication, on the job's own network, and the runner is handed its address
deliberately — that is what lets a job run `docker`. Any job that reaches it can
start a container on it that mounts the host disk. That is host root: on macOS,
root inside Docker Desktop's VM; on Linux, root on the machine.

Two things make this worth changing rather than merely documenting:

- **Most jobs never invoke `docker` at all.** A `cargo test`, an `npm test`, a
  linter — for these the daemon is pure exposure, bought with a privileged
  container and several gigabytes of reserved memory, in exchange for nothing.
- **The cost of a mistake was maximal by default.** faber's threat model is
  "trusted code only", and that stays true. But one misrouted workflow was not a
  degraded sandbox, it was host root, and a `runs-on` typo is not an exotic
  failure. ADR-0012 sharpened this further: the same flag lands inside a VM on
  macOS and on the real machine on Linux.

## Decision

### The daemon is a per-profile grant

`dind` on a `[[profile]]`. `false` starts no privileged container, creates no
`/var/lib/docker` volume, and puts nothing on the job's network to reach — the
exposure is removed rather than reduced. `true` is today's behaviour, stated.

The grant is per **profile**, not per repo or per job, because a profile is the
one thing an operator owns outright. A watched repo's `.faber/config.toml` is
reachable from a pull request (`src/config.rs`), so anything it could set is
something a stranger could propose; `RepoConfig`'s strict schema already rejects
every key but `[playwright]`, and `dind` is now in the test that pins that.

### Withholding `DOCKER_HOST` is the grant, expressed

A denied profile's runner gets no `DOCKER_HOST` at all. This is not cosmetic:
pointing a job at a daemon that was never started would replace "no Docker" with
"a Docker that never answers", and a job's first `docker` call would hang out its
connect timeout instead of failing at once with the ordinary "is the docker daemon
running?".

### An omitted key grants, and says so

Three states, not two. `Unstated` — the key absent — behaves exactly as
`Granted`.

This is the one place the security-optimal answer was not taken, so the reasoning
belongs here rather than in a comment. faber reaches installed hosts through an
unattended update (ADR-0006). A default of `false` would mean a release silently
withdrawing a capability whose absence surfaces as a failing job, on hosts whose
operator did nothing and was not asked. That is precisely the failure mode
ADR-0006 exists to prevent, and the same rule the on-demand-image work was already
held to: *a release carrying a feature must change nothing on a host that didn't
ask*. Refusing to boot was considered and is worse — a dead daemon after an
auto-update, with ADR-0006's rollback flapping against it.

So `Unstated` is a grant, and everything else about it is designed to make it
short-lived and impossible to miss:

- `into_profiles` warns at **every** startup, naming the profile and what it
  grants.
- The dashboard renders it as `+docker?`, distinct from a stated `+docker`.
- The settings form shows a three-way control; saving from it writes an explicit
  value, so opening Settings once is the migration.
- A profile added through the UI states `dind = false`.

It is deliberately not a permanent third state. Flipping the unstated default to
`denied` is a stated future change, to be made in a release whose notes say so —
not something to arrive quietly.

### The audit line records the fact, not the policy

`outcome` records carry `dind: granted | denied`. There is no `unstated` there:
the log answers "could this job reach a privileged daemon?", which has two
answers, and it is read long after the config that produced it may have changed.
Whether an operator chose the grant or merely inherited it is a property of the
config, which the dashboard shows and startup warns about.

It is absent, not `denied`, for a claim reclaimed before any runner existed — no
job ran, so the question has no answer rather than a negative one.

### A denied class stops paying for a container it never runs

The autolimiter reserved and capped two containers per job. Left alone, a class
served by a denied profile would hold the whole profiling ceiling — gigabytes —
against a container faber never starts, and would hold it **forever**: no dind
sample is ever taken for such a class, and the learner only lowers a cap on
evidence that never arrives.

So `limits_for` / `reservation_for` / `sizing_for` take the grant and zero the
dind half, and `record_run` stores a zero dind cap for a run that reported no
daemon. The cohort needs no special handling: a sizing cohort is one profile's
queue, so every class in it shares the grant, and there is no peer whose dind
could raise the ceiling.

The learner keys this on the run report's own `dind` fact rather than on a zero
applied ceiling. A claim reclaimed mid-ramp also reports zero ceilings, and
reading that as "no daemon" would wipe a real learned cap.

### Adoption asks the container, not the config

ADR-0011 restarts adopt in-flight containers instead of draining, and its rule is
that the container is the claim. A container started under a config that has since
changed must still be torn down for what it actually is, so `adopt_one` probes
whether a dind exists rather than consulting the current profile. A dind is not
`--rm`, so even one that died mid-run is still there to be found; only a run that
never had one reads as absent. An unreadable answer falls back to "there is one",
which costs at worst a spurious warning rather than a skipped teardown.

## Consequences

- The exposure is **removed**, not narrowed, for any profile that declines the
  grant — there is no privileged container and no daemon on the job's network.
- A denied profile's jobs also get faster provisioning (no volume create, no dind
  start, no readiness poll — up to `DIND_READY_TIMEOUT` of the ramp) and roughly
  half the memory reservation, so more of them fit under the same budget.
- Existing hosts are unchanged until their operator says otherwise. That is the
  deliberate cost: the security property is achieved for every new profile and
  every operator who acts on the warning, and not yet for one who ignores it.
- The README no longer implies the runner container is an isolation boundary. It
  says plainly that a granted daemon is host root.
- **This does not make faber safe for untrusted code, and nothing here should be
  read as a step toward it.** A denied profile still runs the repo's code on the
  host under the operator's own account. The trust model is unchanged; what
  changed is that the widest privilege in it is now something an operator grants
  rather than something they inherit.

## Alternatives considered

**Default `false` outright.** The clean design, and rejected only on the
unattended-update argument above. Worth revisiting the moment there is a release
whose notes can carry the change.

**Per-job opt-in via a `runs-on` label.** Attractive symmetry with ADR-0010's
"jobs declare their needs" — but the label is written in the repo, and the repo is
PR-reachable. A job asking for host root and getting it because it asked is the
wrong direction; the ceiling has to stay operator-owned. A label could at most
narrow a profile's existing grant, which is not where the exposure is.

**Keeping dind but authenticating it (TLS).** Would stop an unrelated container on
the network reaching the daemon, and stop nothing that matters: the runner is
*given* the credentials by design. It fixes a threat faber does not have.

**Rootless dind, or Sysbox.** Genuinely better answers for the jobs that do need a
daemon, and both remain open on #43. Neither removes the daemon from a job that
never wanted one, which is what this ADR is for; they compose rather than compete.

## Amendment (2026-10-17): a job asks for the daemon with `dind`

ADR-0026 accepts one exception to ADR-0025's parsing rule for the v0.4.0 cut: a
dind job is root in the shared Linux VM and can steer the Docker daemon `_faber`
reads ([#105](https://github.com/rejifald/faber-legacy/issues/105)). One of the two
conditions the owner set on it is that "only jobs that ask for `[faber, dind]`
get a daemon" (ADR-0026 § Consequences). With the rule on, the grant takes two
keys:

- **The profile grants it.** `dind = true`, or the key absent (`Unstated`),
  exactly as above. `dind = false` still grants nothing.
- **The job asks for it.** Its `runs-on` includes `dind`, compared without regard
  to case, as GitHub compares labels. `faber` is already established by profile
  routing, so only `dind` is checked.

A job that does not ask gets no daemon, on any profile. This is the narrowing the
"Per-job opt-in via a `runs-on` label" alternative above left open: a label "could
at most narrow a profile's existing grant", and that is all it does. A label alone
never starts a daemon.

### The rule is a scheduled host key

`[host] dind_requires_label` switches it, in the shape AGENTS.md asks of any
release that narrows a default:

- **Absent or `false`: off.** Every job on a granting profile gets a daemon, as
  before this amendment. Every boot warns that the rule is off. `/api/state`
  reports `dind_label_rule` as `unstated` or `off`, and the dashboard shows it.
  The settings form writes the key explicitly the first time it saves.
- **`true`: on.** The rule above applies, with the three outcomes below.

The rule is **off until the cut and on from it**. The cut's host sitting sets
`dind_requires_label = true` before any token is stored, so no host holds a credential while the exception's condition is unmet.
After the cut the switch has done its work: the rule becomes unconditional and
the key is removed, in [#213](https://github.com/rejifald/faber-legacy/issues/213). It
is a migration, not a permanent third state.

### With the rule on

**Withheld, the job is told why.** A job that did not ask gets
`DOCKER_HOST=unix:///faber-no-dind/add-dind-to-runs-on`, unless its profile's
`env` sets `DOCKER_HOST`. No such socket exists, so the job's first `docker` call
fails at once and names the fix in its own log. This does not reopen the reason
the Decision withholds `DOCKER_HOST`: that was about a TCP address, which hangs
out a connect timeout. A missing Unix socket fails immediately.

**Refused, the job is not provisioned for.** A job that asks on a profile with
`dind = false` gets no runner, as in ADR-0010 when a declared need is refused. The
refusal is listed in `/api/state` as `dind_refused` and on the dashboard. faber
does not mint a daemonless runner carrying `dind` for it, because GitHub could
hand that runner a sibling's dind job routed to a profile that does grant it.

**A runner with a daemon carries `dind`.** It is registered with the label, so a
job that asked lands only on a runner that has a daemon, and a daemonless runner
never carries the label. The reverse cannot be enforced with labels: GitHub hands
a job any runner whose labels are a superset of its own, so a job of the same
repo that did not ask can still be handed a runner minted for a sibling that did.
[#108](https://github.com/rejifald/faber-legacy/issues/108), a VM per dind job, closes
that after the cut.

**faber does not count a daemon runner as supply for a job that did not ask.**
Under demand provisioning (ADR-0004), a registered runner whose labels cover a
queued job absorbs it, and nothing is minted. A runner carrying `dind` now covers
only jobs that carry `dind`. GitHub may still hand it the others, but faber does
not plan on that.
