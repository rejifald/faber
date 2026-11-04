---
title: A hold is named, and a parked claim says why
type: adr
status: accepted
date: 2026-09-24
owner: rejifald
---

# ADR-0022: A hold is named, and a parked claim says why

## Status

accepted (2026-09-24). Amends [ADR-0019](0019-cpu-is-a-deadline-resource.md): the
live API refuses `hard_ceiling = 0`, which the config file already refused.
Resolves #66.
**Amended 2026-10-11** — a missing profile image is a parked reason of its own;
see *"Amendment (2026-10-11)"* at the foot of this file. **Amended 2026-10-12** —
runners start only from a local image (#156); see *"Amendment (2026-10-12)"*.

## Context

In September 2026 an operator held the builder's Docker admission until its Docker
VM could be resized, by posting `{"hard_ceiling": 0}` to `/api/config`. The hold
was the right call. faber hid it:

- The value was persisted to `overrides.json`, and every restart reloaded it. No
  Docker job ran on the builder for about three hours; dozens of claims sat parked.
- `/api/state` reported `healthy: true`, `paused: false`, `last_error: null`. Every
  parked claim read `admitted: false, step: null`. The dashboard showed only
  ` · ceiling 0` beside the budget, under a green "running" dot.
- An agent diagnosing the stall read `/api/state`, missed the zero, and blamed a
  missing runner image (#63). Nothing in the state named a hold or its reason.

Two gaps made that possible.

**There is no named, persistent hold.** `POST /api/pause` lives only in memory,
stops faber from claiming anything, and already has two other users. The install
drain pauses before it swaps the binary, and the janitor pauses around a
reclamation (ADR-0007). Persisting pause would bring every install back up paused.
So the operator reached for a zero ceiling. It persists, but it carries no reason
and no time. It is also a value `faber check-config` rejects, because in many tools
a limit of 0 means "unlimited", while the live API and the boot path took it
unvalidated.

**A parked claim does not say why it is parked.** `try_admit` weighs seven gates:
Docker liveness, the ceiling, adoption, the memory ledger, the CPU ledger, live VM
memory and CPU pressure. It collapses them into one `bool`. The waits before
admission, an image build (ADR-0008) and the pool hold (ADR-0009), say nothing
either, and neither does the native permit. From outside, a held host, a full
budget, a dead Docker daemon and a claim waiting on an image build all look the
same.

## Decision

### 1. A hold is a named, persisted state of Docker admission

- `POST /api/hold` with `{"reason": "..."}` sets it. The reason is required and at
  most 200 characters. Holding while already held replaces the reason and keeps
  the original start time. `POST /api/unhold` releases it and wakes every parked
  claim.
- The hold is kept in `overrides.json` with its reason and start time, restored at
  boot, and warned about at every boot while it lasts.
- It is a gate in the limiter, checked before every other gate, and it covers
  **Docker admission only**. Running containers, adoption (ADR-0011) and native
  jobs are untouched. faber keeps claiming jobs while held, so the held work stays
  visible in the in-flight list. `healthy` stays true, as it does for a pause: a
  hold is deliberate, not a fault.
- The dashboard shows `HELD` and the tray shows `Held`, each with the reason, the
  start time and a Release control.

### 2. A zero ceiling never reaches the limiter

- `POST /api/config` refuses `hard_ceiling = 0` with a 400 that points at
  `/api/hold`, matching `check-config`.
- An `overrides.json` with a zero ceiling, written by an older binary, becomes a
  hold at boot. Its reason names the migration, and its start time is the file's
  mtime. The ceiling falls back to the config file's value, and nothing is written
  at boot. A host that was holding keeps holding across the upgrade.
- While a hold is set, the legacy numeric `hard_ceiling` mirror is written as `0`.
  A rollback to a released binary, which reads only that key, therefore stays held
  rather than releasing a hold nobody lifted. The exception is an unreleased build
  between ADR-0019 and this one, which reads the newer `ceiling` key first. Rolling
  back to such a build means restoring the `overrides.json` it wrote, or posting
  the zero again.
- A zero typed into `config.toml` is still refused by `check-config` and the
  settings save. Boot does not validate the file, so a hand edit can still reach
  the limiter. Its claims then say `hard ceiling: 0 of 0` rather than nothing.

### 3. Every parked claim says why

`try_admit` returns the gate that refused it. Each claim carries
`waiting: {gate, detail}` while it is parked and `null` once it is admitted. The
gates are `held`, `docker`, `adoption`, `ceiling`, `memory`, `cpu`, `vm-memory`,
`cpu-pressure`, `image`, `pool` and `native`. `detail` is one short sentence with
the numbers that decided it. The field is separate from `step`, which is the
reclaim signal and must keep meaning "the job's live step".

## Consequences

- The September stall reads at a glance: every claim says
  `Docker admission held: <reason>`, and the header says since when.
- Stalls inside admission name their gate: a full ledger, a frozen VM, a dead
  daemon or an image build. Failures after admission are not covered. A container
  that cannot start, as in #63, is admitted first and fails afterwards, and it
  still needs its own detection.
- One capability is withdrawn: posting a zero ceiling. `/api/hold` replaces it,
  the refusal happens where a caller is looking, and a host already holding through
  a zero is migrated rather than released. This follows AGENTS.md's rule for
  narrowing a behaviour.
- An older menu-bar app on a newer daemon ignores both new fields and shows what it
  showed before. The tray has to be redeployed for the hold to be visible there.
- The peer host gets holds only through a release (ADR-0006).

## Alternatives considered

**Persist `pause`.** Rejected. The install drain and the janitor use pause as a
transient switch, so a persisted pause would bring every install up paused, and the
janitor's resume would have to tell its own pause from an operator's. Pause also
stops claiming, so a held host would stop showing what it is holding back.

**Keep the zero ceiling and show it loudly.** Rejected. A number carries no reason
and no start time, and 0 reads as "unlimited" in enough tools that `check-config`
already refuses it. A hold is a different thing from a limit, so it gets its own
name.

**A global hold covering native jobs too.** Rejected for now. Every hold so far has
been about the Docker VM, and native jobs run on the host outside it. Pause remains
the global, transient switch.

**Warn only.** Rejected. The limiter's warning list is transient, and the tray's
popover does not render it. A state that stops all Docker work needs to be a state,
not a log line.

## Validation

- Unit tests cover:
  - the gate `try_admit` reports, with `held` ahead of every other gate
  - a hold parking `admit`, and a release admitting it at once
  - the zero-ceiling migration
  - persisting the hold with its rollback mirror
  - the refusal of a zero ceiling and of a blank reason
- On the builder, 2026-09-24 (a 0.3.12 development build):
  - The first boot read the zero ceiling in `overrides.json` as a hold, dated to
    the moment the zero was set, and warned about it by reason and date.
  - `/api/state` showed `budget.hold`, and the ceiling setting fell back to the
    config file's `off`.
  - Every parked claim reported `waiting.gate = "held"` with the reason.
  - Re-holding with the real reason kept the original start. The persisted file
    carried the hold, `ceiling: "off"` and the rollback mirror
    `hard_ceiling: 0`, with every pin intact.

---

## Amendment (2026-10-11): a missing profile image parks, and says so

*Consequences* above left #63 open: a container that cannot start is admitted
first and fails afterwards. On the host behind #63 the profile's own image had
gone from the local Docker, and faber registered a JIT runner for every queued
job, every few minutes, each of which failed to start: about 1,400 provisions,
none completed and about 900 faults in a day and a half, with `healthy: true`
throughout and a peer silently taking the work.

The scope settled for it is **detect only**. The image is the operator's, often a
local-only tag no registry holds, and faber may not infer that it can recreate it
(AGENTS.md, [ADR-0016](0016-reclamation-is-delegated-not-inferred.md)). So:

- **Once a cycle, per Docker profile**, faber asks the daemon whether the
  profile's image is present (`docker image inspect`, read under #105's rules:
  only the daemon's own "No such image" counts as absent, and a success must print
  an image id). Skipped while Docker is not answering.
- **Absent is not yet missing.** A profile image may sit in a registry, never
  pulled here, which `docker run` has always handled by pulling it. Refusing on
  "not local" alone would withdraw that from every host relying on it, so faber
  pulls an absent image in the background (at most once per 10 minutes per
  profile) and calls it **missing** only when it is still absent after a pull that
  failed. A pull that times out decides nothing.
- **A missing image parks its profile's claims** under the new gate
  `image-missing`, before admission and again after it, exactly as a repo hold
  does (ADR-0030). No runner is registered, so none is left offline on the repo.
  A faber-built variant (ADR-0008) is a different tag, rebuildable, and is never
  held back by this.
- **One fault per outage**, `profile_image:<profile>` in the fault feed and the
  audit log, and its recovery when the image is back — not one per claim per
  cycle.
- **`healthy` reads false**, `last_error` names the profile and image, and
  `/api/state` carries `missing_images`. The dashboard shows `IMAGE MISSING` with
  a note per profile; the tray shows `Image missing` with its own banner rather
  than "Can't reach GitHub". An older tray still shows Unhealthy with the
  `last_error` text.
- **Recovery is automatic.** The first cycle that sees the image again clears the
  state, and parked claims start within one poll.

What this does not do is rebuild or protect the image (#63's questions 2 and 3).
Those remain open.

---

## Amendment (2026-10-12): a runner starts only from a local image

A review of #152 found the hole the amendment above walked past (#156). A profile
image given as a bare `owner/x` resolves to Docker Hub's `owner` namespace. When
such a tag is local-only and goes missing, `docker run` — and the background pull
above — fetched `docker.io/owner/x` instead of failing, and if somebody else held
that namespace, jobs ran in their image. That was true before #63 too; a missing
image only made it reachable.

- **The runner container is started with `--pull=never`.** An image is either
  local or was pulled on purpose by the path above. The other containers faber
  starts keep Docker's default, because none of them runs the profile image: the
  dind sidecar runs `dind_image` (`docker:dind` by default), and the VM probe and
  the cache-age scan run `autolimit.probe_image`. A fresh host has always pulled
  those on first use, and faber has no deliberate pull for them to fall back on.
  `docker build` of a variant (ADR-0008) already passed `--pull=false`.
- **A claim waits while its profile image is not known to be local**, under the
  gate `image-pull`: until the first per-cycle check of it has run (while Docker
  answers), and while it is absent and being pulled — where `docker run` used to
  pull it on the spot. A variant is another tag and never waits on this.
- **References are classified, not just pulled.** The first path component is a
  registry host when it contains `.` or `:`, is `localhost`, or has an uppercase
  letter (Docker's own rule). A registry-qualified reference, or one pinned by
  digest, is pulled as before. One with no host is governed by a new key,
  `[host] pull_unqualified_images`. `false` never pulls it: absent is missing at
  once, under `image-missing`, with the fault recorded once.
- **Absent keeps the pull** (AGENTS.md: a release does not withdraw what a host
  relies on). A host whose profile image is a bare Docker Hub reference not yet
  pulled here works today only through that pull. So unstated means pulled, and
  each profile whose image sits in a Docker Hub **user** namespace (`owner/x`) is
  warned about at every boot and listed in `/api/state.unqualified_images`, shown
  on the dashboard; the settings form writes `true` on its first save. Flipping
  the unstated default to `false` is a future, release-noted change.
- **Official-library references are not warned about.** `ubuntu:24.04` resolves to
  `docker.io/library/ubuntu`, a namespace only Docker publishes into. Nobody can
  register a name there, so a pull of it returns Docker's own image or nothing —
  the squatting risk is the user namespace. `false` still covers both, because its
  promise is the simpler one: faber pulls only from a registry the operator named.
- **An explicit `docker.io/` or `index.docker.io/` counts as a named registry.**
  It resolves to the same Docker Hub user namespace as a bare `owner/x`, so the
  exposure is the same, but writing the host reads as a deliberate choice. It is
  pulled and not warned about. Revisit if such a profile image is ever configured.
