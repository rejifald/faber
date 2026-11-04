---
title: repository_dispatch is opt-in, and a refused job is visible while it waits
type: adr
status: accepted
date: 2026-10-09
owner: rejifald
---

# ADR-0029: `repository_dispatch` is opt-in, and a refused job is visible while it waits

## Status

accepted (2026-10-09). Closes [issue #95](https://github.com/rejifald/faber-legacy/issues/95).
Amends [ADR-0014](0014-the-trust-boundary-is-operator-configurable.md): the
ceiling and the default are no longer the same list.

Shipped on the builder from `main`; it reaches follower hosts with the first
signed release. Until then a host on v0.3.11 or earlier refuses
`repository_dispatch` and `release` whatever its config says. `release` was added
by the same decision (see below).

## Context

A watched repo ran a deployment check on faber's labels, triggered by a hosting
provider's `repository_dispatch` once a preview deployment was ready. The gate
refused the event (it was outside `MAX_EVENTS`), and yet the check usually passed.
It ran by **taking a JIT runner faber had minted for some other queued job** with
the same labels. JIT runners are not pinned to a job, so any queued job matching
the labels can claim one. The job it took from then waited for a second
provisioning.

That only works while the repo is busy. Once the repo went quiet, nothing else was
queued, nothing was minted, and the check waited indefinitely. Production
promotion depended on that check, so it waited too. On the host, the audit log
recorded each attempt as `refuse:event:repository_dispatch`. The dashboard showed
a cumulative "refused" count, which no one connects to a job sitting in a queue
right now. Everyone read it as "the deploy is still running".

So two things were wrong:

1. **Policy.** `repository_dispatch` was refused although its trust basis is the
   one `workflow_dispatch` already has.
2. **Legibility.** A refusal of a job that targets this host's labels is
   invisible while the job waits. That holds for any refused event, not just this
   one.

## Decision

### `repository_dispatch` joins the ceiling, not the default

`gate::MAX_EVENTS` (the ceiling) gains `repository_dispatch`. A new
`gate::DEFAULT_EVENTS` keeps the historical five, and it is what an absent `[gate]`
section, or an absent `allowed_events`, resolves to. An operator opts in by
listing the event:

```toml
[gate]
allowed_events = ["push", "pull_request", "workflow_dispatch", "schedule", "merge_group", "repository_dispatch"]
```

Until ADR-0014 the ceiling and the default could be one constant, because every
event under the ceiling was one every host already serviced. Adding an event
breaks that. If the default stayed "the whole ceiling", this release would start
provisioning a new trigger on every follower host as it updates itself (ADR-0006),
which widens a trust boundary on hosts whose operators were never asked. That is
the shape AGENTS.md rules out. The absent key keeps the old behaviour, and the new
state is legible: the boundary readout reports it as `opt_in_events` ("opt-in,
off") on every host that hasn't listed it. It is kept apart from
`excluded_events`, which stays what ADR-0014 made it: the events the operator
narrowed away. A host whose operator never touched `[gate]` is not described as
having narrowed anything.

The opt-in is permanent, not a step toward defaulting it on. The one case where
enabling it widens exposure (below) cannot be detected from faber's side, so only
the operator can decide it is safe for their repos.

It is not treated as a relaxation the way `allow_public_repos` is: no boot
warning and no amber readout. On the common repo it widens nothing, and a warning
that fires on correct configs teaches people to ignore the ones that matter. The
caveat lives where the decision is made, in the settings hint for
`allowed_events`.

### Why it is safe under the ceiling

`repository_dispatch` has the same provenance properties as `workflow_dispatch`,
which was always allowed:

- **It always runs the workflow file from the default branch.** That is code
  already on a branch of the repo, which is the property every event under the
  ceiling shares.
- **It is not a pull-request event.** It carries no fork head, so the fork check
  behaves exactly as it does for `push`.
- **Only a credential with `contents: write` can send one.**

The issue raised `client_payload`, which the caller controls: a workflow that
interpolates it into `run:` is injectable. It is, but on an ordinary repo the
injection grants nothing the sender does not already have. A credential with
`contents: write` can push **any branch**, and faber already provisions `push`
events on any non-fork branch. That branch's workflow can target faber's labels
and run anything. The equivalence is therefore through `push`, not through the
default branch, which branch protection may well close to that credential. This
is the same reasoning that makes `workflow_dispatch` inputs acceptable. Writing
such a workflow is a bug in that workflow, not a hole in faber's boundary.

**The one case where opting in widens exposure** is a repo whose rules stop a
write credential from creating branches *and* protect the default branch. There,
`push` no longer gives that credential code execution, while
`repository_dispatch` plus an injectable workflow still does. This is the case the
operator weighs when listing the event. (On the host this ADR was written for, no
watched repo could have such rules: they are private repos on a plan without
rulesets.)

For that reason the opt-in is **host-wide**, not per repo or per workflow.
ADR-0014 already rejected per-profile boundaries, because "what will this machine
run?" should have one answer per host. A per-workflow allow-list would be a new
axis, protecting only the locked-branch case above, which the operator can
already exclude by not opting in.

`pull_request_target`, `workflow_run` and `issue_comment` stay outside the ceiling,
for the reason ADR-0014 gives: their provenance fields do not describe the code
that runs.

### `release` joins on the same terms

The refused-jobs view found a second case within minutes of shipping. A watched
repo's mobile-release workflow runs on `release` and was refused as
`refuse:event:release`. Unlike `repository_dispatch`, `release` was outside the
ceiling, so no config could have allowed it.

`release` is added to `MAX_EVENTS` as a second opt-in event, also absent from
`DEFAULT_EVENTS`, and it is reported under `opt_in_events` until listed. The
argument is the one above:

- Publishing a release needs `contents: write`.
- The run is at the release tag's commit, which is code in the repo.
- It is not a pull-request event, so the fork check is unaffected.

The tag may point at any commit, not only a branch tip. But its author holds a
credential that can push any branch, which faber already provisions, so that
widens nothing. The locked-branch-creation caveat applies unchanged.

The generalisation stops there. Other events (`deployment`, `registry_package`,
…) join only when a real workflow needs one, each argued on its own provenance.

### A refused job that is waiting is shown as waiting

The orchestrator keeps a **live** set of jobs that are queued on GitHub, target
this host's labels, and are refused by its gate. It is exposed as
`refused_queued` in `GET /api/state`, oldest first, with the repo, run, job,
event, verdict and the time faber first saw it refused.

Every refuse reason belongs in it, not only a disallowed event. A fork or
untrusted-actor job that targets faber's labels is a misrouted workflow, since
`runs-on` routing is the primary control, and that is just as actionable. Because
outsiders can cause those, the set is **capped at 50 entries**, oldest kept, with
the overflow counted. The popover, which is always visible, shows only the repo
for a fork or actor refusal, never the workflow name: a fork PR controls that
string.

- **The dashboard** lists them under their own heading, with a link to the run.
  For an event the operator could allow (narrowed away, or opt-in and off) it
  names the `[gate] allowed_events` key.
- **The tray** shows them as an attention state: "N jobs refused here — no runner
  of its own; it waits, or takes one minted for another job". It does not say
  "forever", because the second half is what actually happened in #95.
- A job leaves the set when it stops being queued and refused, because it
  started, finished, was cancelled, or the boundary changed. A repo whose listing
  failed in a cycle keeps its entries, so a GitHub blip does not make a waiting
  job disappear.

The set is live state, not history. `counts.refused` and the decision feed remain
the history. This covers every refusal, not only `repository_dispatch`: a job
refused for any reason that targets this host's labels waits the same way.

## Consequences

- A host that lists `repository_dispatch` provisions for it like any other
  allowed event: the job gets its own runner and no longer takes another job's.
- A host that does not list it is unchanged, except that its refused jobs are now
  visible while they wait.
- **Runner theft is not fixed in general.** A refused job can still claim a runner
  minted for another job with the same labels, because JIT runners are not pinned
  to a job. That is what faber's earlier runner-mismatch fixes worked around
  rather than removed. This ADR makes the refusal visible; it does not prevent the
  theft. A repo that wants such a job to never run here should give it labels that
  don't match faber's.
- `repository_dispatch` stays off by default permanently (see above). Flipping it
  is not a deferred change.
- Runner theft is tracked in [issue #97](https://github.com/rejifald/faber-legacy/issues/97).

## Alternatives considered

**Keep refusing, and only surface it.** The issue's second option. It leaves a
trigger refused whose trust basis is identical to one that is allowed, and pushes
every consuming repo to move such workflows off faber for no security gain.
Surfacing is kept, because it is right for every other refused event.

**Default `repository_dispatch` on.** It would fix the issue's repo with no config
change, and it widens the boundary on every follower host unattended. Rejected on
ADR-0014's unattended-update argument.

**Allow it per repo or per workflow.** It would address `client_payload`
injection, which widens exposure only on a locked-branch repo (see above), at the
cost of the one-answer-per-host property ADR-0014 chose deliberately.

**Warn at boot when it is enabled**, as `allow_public_repos` does. Rejected:
`allow_public_repos` always widens, and this almost never does.
