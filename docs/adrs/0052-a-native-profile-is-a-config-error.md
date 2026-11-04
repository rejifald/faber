---
title: A native profile is a config error, not a warning
type: adr
status: accepted
date: 2026-10-25
owner: rejifald
---

# ADR-0052: A native profile is a config error, not a warning

## Status

accepted (2026-10-25, owner). Supersedes one paragraph of ADR-0026, *macOS jobs
run in per-job macOS VMs, from the cut on*: "A profile with `backend = "native"`
then becomes a warning at every boot and is otherwise ignored". Everything else
in ADR-0026 stands.

## Context

ADR-0026 removed the native backend, which ran a job's runner as a process on the
host itself. To keep an unattended update from refusing to boot (AGENTS.md), a
profile that still said `backend = "native"` kept parsing, was built, and was then
dropped at boot. Four surfaces reported that: a warning at every boot, a
`native_ignored` notice in `/api/state`, a row in the tray popover, and a dimmed
"Ignored" row in Settings. `faber check-config` reported it as a warning only.

That kept the whole native backend alive in code for nothing to run it: the
`Backend::Native` variant and its match arms, a native admission semaphore, the
limiter's native slot, `runner::run_one`, the `[autolimit] native_concurrency`
knob, and a CLI flags path that synthesised a native profile.

Measured 2026-10-25, the warning was not idle. Both hosts run `main`, and the
peer still carried a native profile with GitHub's default labels for a macOS
runner, `[self-hosted, macOS, ARM64]`. About a dozen jobs across a few watched
repositories, faber's own `ci.yml` among them, target those labels. All of them
were queued with no runner: this repo's CI had waited since earlier that day.
Taking the warning away without moving those jobs would have hidden the stall.

## Decision

1. **`backend = "native"` is a config error.** It still parses, only so the
   error can name the profile and say what to write instead: `backend =
   "macos-vm"`, with the same labels, so no workflow changes. `faber
   check-config` and the settings save refuse it, and so does building the
   profiles at boot. Nothing builds a native profile any more.
2. **The warning goes, everywhere.** The boot warning, the `native_ignored`
   notice, the popover row, the Settings row and the check-config warning are
   removed.
3. **The native backend's code goes with it.** That covers `Backend::Native`,
   the native admission path, the limiter's native slot and `runner::run_one`.
   The no-config CLI path builds no profile.
4. **`[autolimit] native_concurrency` stays, and limits nothing.** The file, the
   overrides, the settings form and the popover's *Native jobs* knob keep it, as
   the owner decided on 2026-10-22 (`docs/design/settings/anatomy.md`), so no
   saved file or override stops loading.
5. **A config with no file, and the starter, hold no native profile.** The
   no-config CLI path builds no profile, and claims nothing until a config is
   saved; `--labels` and `--concurrency` still parse, and are ignored. The
   starter config seeds its Docker profile instead.
6. **Jobs that targeted a native profile move to a `macos-vm` profile with the
   same labels** (owner, 2026-10-25), not to GitHub-hosted runners. That is
   ADR-0026's sub-step 4 of the upgrade sitting, unchanged.

## Why an error is acceptable here

AGENTS.md says a release must not refuse to boot on a config it did not write.
That rule protects a host that updates itself unattended. No host does yet: no
release can install before the ADR-0031 cut, and the cut upgrades every host by
hand. `faber check-config` runs before each install
([manual-update.md](../manual-update.md)), and it now fails on a native profile.
The operator fixes the profile where a human is looking, before the binary that
refuses it is installed.

Like ADR-0027's removal of `gh`, this is a one-off exception, not a precedent.
After the cut, followers install releases on their own, and narrowing what a
config may say goes back to the warn-first shape AGENTS.md describes.

## Consequences

- A host still carrying a native profile fails `check-config`. If it is
  installed anyway, it fails to load its config at boot. Before installing,
  switch the profile to `backend = "macos-vm"` (ADR-0026's sub-step 4 of the
  upgrade sitting).
  A daemon that boots on such a file anyway exits, as it does on a docker
  profile with no `image`.
  Until the macOS path is ready, delete the block instead. Jobs that target its
  labels stay queued either way, as they already do today.
- Jobs whose `runs-on` matched only a native profile get a runner again only
  once a host serves those labels from a `macos-vm` profile.
- `native_concurrency` stays with no effect. Dropping the key and its knob is a
  later, release-noted change.

## Alternatives considered

- **Keep the warning** (ADR-0026 as written). Correct, and loud, but it keeps
  the native backend's code alive with nothing to run.
- **Keep parsing, drop the profile without a word.** The smallest change, but a
  host that missed the switch would strand its jobs with no signal. That is the
  failure the warning existed to prevent.
- **Move the jobs to GitHub-hosted macOS runners.** No host setup needed, but
  ADR-0026 already rejected a hosted interim on cost.
