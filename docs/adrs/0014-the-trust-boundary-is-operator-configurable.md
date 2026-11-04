---
title: The trust boundary is operator-configurable, and narrows only
type: adr
status: accepted
date: 2026-09-03
owner: rejifald
---

# ADR-0014: The trust boundary is operator-configurable, and narrows only

## Status

accepted (2026-09-03). Closes [issue #42](https://github.com/rejifald/faber-legacy/issues/42).
Amended by [ADR-0029](0029-repository-dispatch-is-opt-in-and-refusals-are-visible.md):
the ceiling now holds `repository_dispatch`, which the default does not, so "no
`[gate]` section" means `gate::DEFAULT_EVENTS`, not the whole ceiling.
Amended by [ADR-0030](0030-the-gate-is-defense-in-depth-and-a-bypass-is-detected.md):
the gate decides what faber mints for, not what executes; a bypass is detected,
and enforced under `[gate] on_bypass = "enforce"`.

## Context

`src/gate.rs` decides whether faber provisions a runner for a queued job. Until
now its boundary was compiled in: a fixed allow-list of trigger events, an
unconditional fork refusal, and an outright refusal of any public repo. Only the
actor allow/deny lists were configurable.

That was defensible as a backstop and had become the wrong shape:

- **Too strict for some.** A public repo servicing only its own `push` events is
  a legitimate configuration faber refused outright. Notably, it meant faber
  structurally could not run its own CI once this repo goes public.
- **Too coarse for others.** An operator who wants `push` only — not
  `pull_request` at all, not even same-repo — had no way to say so.
- **Invisible.** Nothing reported the boundary in force. A refusal named a rule
  you could only locate by reading the source.

## Decision

### The event list is a *ceiling*, not a default

`gate::MAX_EVENTS` stays compiled in, and `[gate] allowed_events` is
**intersected** with it — never unioned, never substituted.

This is the load-bearing choice, and it is what makes "the boundary is
configurable" a safe sentence. An operator can say "push only". Nobody — not a
config file, not a settings form, not a caller inside the process — can add
`pull_request_target`, whose run carries base-repo secrets while reporting
`head_repo == base_repo`, and which would sail past the fork check. The
intersection happens in `TrustPolicy::with_boundary`, so it holds regardless of
what reaches it; `GatePolicy` additionally rejects an out-of-ceiling event at
load, which makes that a *loud* error rather than a silent drop.

The same check catches the other, likelier mistake: a typo. `puhs` would
otherwise be accepted, silently narrow the boundary, and stop provisioning every
push job on the host with nothing to explain it.

### Public repos are opt-in, and opting in weakens nothing else

`allow_public_repos` defaults `false` — faber's historical behaviour, refusing a
public repo ahead of every other check.

Lifting it is defensible precisely because the checks behind it do not soften. A
public repo's distinguishing property is that *anyone* may open a pull request —
and every such PR comes from a fork, which the fork check refuses on its own, with
a missing head repo still failing closed. Every provenance-spoofable trigger stays
outside the ceiling and unreachable. So what `allow_public_repos = true` actually
buys an attacker is the ability to queue a job that gets refused, and what it buys
the operator is `push` / `schedule` / `merge_group` CI on a public repo.

It is still a relaxation, so it is warned about at every boot and rendered in
amber on the dashboard rather than listed as ordinary metadata.

### `allow_fork_prs` does not exist, and writing it is an error

Not deferred, not defaulted to false — absent. `GatePolicy` carries
`deny_unknown_fields`, so the key an operator reaches for first produces a startup
error naming the fields that do exist.

While job code reaches a privileged Docker daemon ([#43](https://github.com/rejifald/faber-legacy/issues/43),
ADR-0013), accepting a stranger's pull request is handing out host root behind a
toggle. A key defaulting to `false` would still be a key: something to flip in a
hurry, or to be talked into. The `Boundary` readout reports `allow_fork_prs:
false` explicitly rather than staying silent, so the answer is visible without the
setting existing.

ADR-0013 removes the daemon for profiles that decline it, which is real progress,
but it is not the boundary that would make fork PRs safe — a denied profile still
runs the repo's code on the host under the operator's account. That needs a VM per
job, not a container.

### No `[gate]` section means faber's historical boundary

The issue proposed defaulting `allowed_events` to `["push"]` — the narrowest
useful set. Rejected for the same reason ADR-0013 kept an omitted `dind` granting:
faber reaches installed hosts through an unattended update (ADR-0006), and a
release that silently stops provisioning `pull_request` jobs breaks CI on hosts
whose operator did nothing and was not asked.

"Zero trust by default" is still satisfied, and by a stronger argument than a
narrow default would have given: **every edge that has a safe direction defaults
to the safe one, and nothing a config omits widens the boundary.** Public repos
default to refused. Fork PRs are unconditional. Provenance-spoofable triggers are
not merely off, they are unreachable. The event list is the one place where
narrower is not safer — `push` is your own code on your own branch, exactly like
`schedule` — so defaulting it narrow would have cost working CI and bought
nothing.

### The boundary is reported, not inferred

A `Boundary` value, from `TrustPolicy::boundary()`:

- **The dashboard** renders it under the header — events allowed, events the
  operator narrowed away, public-repo state, and fork PRs refused.
- **The audit log** gets one `gate` record at boot. Not a field on every decision:
  the boundary cannot change without a restart, so per-job it would be the same
  string repeated forever. One line per boot is what lets "these jobs were refused
  last Tuesday" be read against what the machine was enforcing then.

`excluded_events` is reported alongside `allowed_events` on purpose. "Why was my
PR job refused?" is answered by the events that are *missing*, and computing that
by diffing against a constant in the source is what this ADR exists to end.

### The boundary is host-wide, read off the profiles

`into_profiles` gives every profile the same boundary. Letting one profile accept
`pull_request` while another refuses it would make "what will this machine run?"
depend on label routing — a question with one answer per host, not per profile.

### It comes from the operator's config, and cannot come from a repo

A watched repo's in-repo `.faber/config.toml` (`RepoConfig`) is reachable from a
pull request: a fork PR can propose changes to it. If gate policy were readable
from there, a PR could widen the boundary that is meant to be judging it — the
single most dangerous thing an in-repo file could be allowed to say.

`RepoConfig`'s strict schema admits only `[playwright]`, and
`repo_config_cannot_widen_trust` now pins both gate keys, at top level and nested,
alongside the daemon grant from ADR-0013. The same reasoning covers the write
path: the settings page can rewrite `config.toml`, so relaxing the gate from a
browser form is a real action, and the form says what it does rather than
presenting a bare checkbox.

## Consequences

- faber can service a public repo — including, once this repo is public, its own
  CI — without any change to how fork PRs or spoofable triggers are treated.
- An operator can tighten to `push`-only, which was previously impossible.
- Existing hosts are unchanged: no `[gate]` section is byte-for-byte the old
  boundary, so the unattended update channel carries no behaviour change.
- A misconfigured event list now fails loudly at `faber config check` and on
  settings save, and warns at boot, instead of silently narrowing the boundary.
- **The trust model is unchanged.** faber still runs trusted code on your own
  machine and is still not a sandbox. What changed is that its boundary is
  legible and adjustable in the safe direction, not that it became safe for
  untrusted work.

## Alternatives considered

**Defaulting `allowed_events` to `["push"]`.** The issue's own proposal, rejected
on the unattended-update argument above. Worth revisiting in a release whose notes
can carry the change, together with ADR-0013's `dind` default.

**Making the boundary per-profile.** More expressive, and wrong: it turns "what
will this machine run?" into a question you answer by tracing label routing.
Per-profile *capability* (ADR-0013's daemon grant) is a different axis and stays
per-profile, because it describes the sandbox rather than the admission rule.

**Refusing to boot on a bad `allowed_events`.** Fail-closed, and worse in
practice: a daemon that will not start after an unattended update is a dead host,
with ADR-0006's rollback flapping against a config file the release did not write.
Warn at boot, reject on the paths a human is actually looking at.

**Letting a repo narrow its own gate.** Superficially safe — narrowing is the
harmless direction — and rejected because it puts gate keys in a PR-reachable
file. The moment that schema exists, the next key added to it has to re-derive
"but can a fork PR set this?", and one day the answer will be wrong. The file
stays incapable of naming a gate key at all.
