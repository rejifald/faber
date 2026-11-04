---
title: Runner-image dependency-drift detection
type: adr
status: accepted
date: 2026-07-22
owner: rejifald
---

# ADR-0001: Runner-image dependency-drift detection

## Status

accepted (2026-07-22), **amended twice** — see *"Amendment (2026-07-23)"* and
*"Amendment (2026-08-28)"* at the foot of this file. Phase 1 (detection +
`surface`) is implemented and live. `auto_pr` is **withdrawn** by the 2026-08-28
amendment; `mode = "gate"` remains deferred and is a detect-only no-op today.
This is faber's first ADR; it adopts the same template as the repos faber
serves.

## Context

faber provisions ephemeral JIT runners from a prebuilt image. When that image
bakes a **version-locked toolchain dependency**, the baked version must match the
version the target repo pins in its lockfile. The motivating case is Playwright:
its browser binaries are installed at image-build time into a fixed path
(`/opt/ms-playwright`, via a `PLAYWRIGHT_VERSION` build-arg), and the running job
resolves the browser executable path from the workspace's installed
`@playwright/test`. Bump the pin without rebuilding the image and the job fails at
runtime with an "executable missing" error that **conflates two unrelated causes**
— a genuine version mismatch (rebuild needed) and a transient one (a partial
dependency-cache restore) — so a rebuild gets chased when nothing was wrong.

Three forces shape the solution. (1) faber is a **generic, multi-repo**
provisioner, so anything that requires each watched repo to add CI wiring won't be
adopted uniformly — the intelligence must live in the shared component. (2) faber
provisions **before the repo is checked out**, so it can only read repo files via
the API at the job's ref, and only the **image** can honestly report what it baked
(a repo can't self-report the image's contents, or the check is meaningless). (3)
faber runs **only same-repo trusted jobs** (fork PRs are routed away by the gate),
so in-repo config is your own code — but it is still PR-reachable, so it must not
be able to weaken faber's own trust decisions.

## Decision

faber detects drift at provision time and, by default, only **surfaces** it.

1. **Image label = the baked truth.** A runner image opts in by baking
   `LABEL org.faber.playwright.version=<version>` plus a sibling file
   `/opt/ms-playwright/.playwright-version` (label for faber's `docker inspect`;
   file for any optional in-job consumer). **No label ⇒ faber does nothing** —
   label presence *is* the per-repo opt-in, and its absence gracefully excludes
   repos faber shouldn't judge (e.g. those using a shared prebuilt image they
   don't build).

2. **Lockfile = the pinned truth.** faber fetches the target repo's *resolved*
   root `@playwright/test` from `package-lock.json` at the job's head SHA via the
   contents API and compares it to the baked label. Resolved (not declared) is
   used because it is exactly what the job installs and the runtime resolves
   against.

3. **Policy = global default + optional in-repo override.** faber's global config
   carries a `[playwright_drift]` default. A repo may override it with
   `.faber/config.toml`, parsed into a **strict `RepoConfig` allowlist**
   (`deny_unknown_fields`) that structurally *cannot* express trust / gate /
   backend / image — only the drift knobs. It is read at the **head ref** and
   **repo-wins** over the global default. Safety is the schema, not the ref: the
   job's own code is about to run on the host regardless, so reading its
   drift-policy from the same ref is strictly less privileged.

4. **Two orthogonal axes.**
   - `mode = off | surface | gate` (default `surface`): *surface* posts a
     `failure` commit status on the head SHA plus an audit/dashboard record;
     *gate* cancels the run; *off* is an escape hatch for a known-transitional
     bump window.
   - `auto_pr` (default off) — **withdrawn, never built; see the 2026-08-28
     amendment.** As specified: on **default-branch** drift only, faber opens an
     idempotent catch-up PR (`faber/runner-playwright-<version>`, one per target
     version, updated not duplicated) that bumps the image's build-arg, its body
     linking the detecting run. On **PR-introduced** drift faber is surface-only —
     never opening a competing PR — which avoids the ordering hazard of an
     image-ahead-of-pin merge.

Detection + surfacing are the cheap, always-safe half; *gate* and *auto_pr* are
opt-in. faber is a passive surface by default, not an adjudicator of CI.

## Consequences

- Any watched repo gains drift protection by baking **one label** — no `.github/`
  changes. Repos that bake no such dependency pay nothing.
- The confusing runtime error is replaced by an honest, external diagnosis that
  needs no cooperation from the repo's CI.
- A pin-bump PR still runs the baked-dependency job red until the image is rebuilt
  post-merge — structural to a baked image *without* the deferred heal, now made
  legible instead of mysterious.
- faber must gain: an API contents-fetch + `head_sha` on the run model; a
  `RepoConfig` type + merge; a provision-time detection hook; commit-status
  posting. Lockfile parsing starts with **npm**; other ecosystems are a reserved
  `ecosystem` hint on `RepoConfig`.
- **Deferred (recorded, not built) — auto-heal.** faber could keep a host-cached,
  per-version browser directory (populated by copying the upstream framework
  image's browsers, no rebuild) and bind-mount it over the baked path at
  provision, making bump PRs green *pre-merge*. Deferred because drift is rare and
  this is standing complexity in a provisioner; promote only if bump frequency
  justifies it. **Promoted and built** — see
  [ADR-0008](0008-on-demand-runner-image-variants.md), which took the build-a-variant
  route rather than the bind-mount sketched here, and whose §2 records why.
- **Public-hygiene follow-up (owned here).** faber's source and fixtures must
  reference a placeholder image (`owner/my-runner`), never a real private image
  name, and design docs must stay name-free — this repo is public.

## Alternatives considered

- **Per-project CI guard (a hygiene-job step).** Rejected: repo-specific, forces
  every watched repo to re-implement it — the exact adoption friction faber exists
  to remove.
- **A drop-in reusable action.** Kept as an *optional* add-on (one line per
  workflow, ecosystem-agnostic in-job via the baked version file), but rejected as
  the *primary* surface: still per-repo wiring, only protects repos that add it.
- **Derive the baked version from the lockfile at build time** (nothing to
  drift). More robust against a hand-edited build-arg, but removes the reviewable
  version-bump PR and needs a gated heavy-rebuild trigger; rejected in favour of
  an asserted build-arg + catch-up PR.
- **Central per-repo config in faber's own config file.** Rejected in favour of
  in-repo `.faber/config.toml` — policy lives with, and is versioned by, the repo
  it governs.
- **Active gate by default.** Rejected: a provisioner that cancels CI on a version
  heuristic is a frightening default, and false positives are real. Gate is
  opt-in.

## Amendment (2026-07-23): surface the half-wired opt-in

The opt-in has two halves — the repo's `.faber/config.toml [playwright]` and the
image's `LABEL org.faber.playwright.version`. Phase 1 shipped the label reader but
nothing baked the label (the runner image builds off-repo), so every job hit the
silent `Skipped(NoLabel)` short-circuit: faber surfaced **nothing**, and a real
Playwright bump sailed past into the exact cryptic `executablePath` error the
detector exists to pre-empt. The silence *is* correct for a generic provisioner
(most images never opt in) — but it also hides the case where a repo *did* opt in
and only the image half is missing.

So faber now distinguishes the two. On the no-label path, **once per repo per
daemon lifetime**, it fetches `.faber/config.toml`; a repo that opted in yet ships
an unlabelled image yields `SkipCause::ConfiguredNoLabel` — a low-severity,
one-shot audit note + dashboard event (`status = "misconfigured"`), never a commit
status (there is no baked version to judge, only a misconfiguration to flag). A
repo with no config keeps the silent, zero-network-I/O short-circuit; the per-repo
probe is why the extra contents-fetch stays a per-repo diagnostic, not a per-job
cost. This does **not** turn detection on — that still requires baking the label —
it makes "I opted in but nothing happens" legible instead of invisible.

## Amendment (2026-08-28): `auto_pr` is withdrawn

Phase 2 was two independent things — `mode = "gate"` and `auto_pr`. This
amendment retires the second and leaves the first exactly where it was.

**`auto_pr` will not be built.** Two reasons, either of which is sufficient.

*It has nothing left to do.* The catch-up PR existed to make the **image** catch
up with a pin, so that a bump PR — red until someone rebuilt the image — would
eventually go green. [ADR-0008](0008-on-demand-runner-image-variants.md) reaches
the same end faster and without a human in it: a confirmed mismatch is *healed*
at provision time by building the per-version variant the job pins, so the bump
PR is green on its first run. A PR that asks someone to fix an image is a slower
path to an outcome faber now produces itself. This ADR's own Consequences
predicted the shape of this ("a pin-bump PR still runs the baked-dependency job
red until the image is rebuilt post-merge — structural to a baked image *without*
the deferred heal"); the heal is no longer deferred, so that sentence no longer
holds, and neither does the mitigation built on top of it.

*It has nothing left to bump.* `auto_pr` presumed the runner image's Dockerfile
lived **in the watched repo** — that is what the `dockerfile` and `arg`
coordinates on `RepoPlaywright` name. That presumption is false and has been for
some time: the base image is operator-owned and built off-repo
([ADR-0008](0008-on-demand-runner-image-variants.md) §1; it exists on the builder
and in no registry), and the repo that adopted this contract retired its own
in-repo runner-image build as superseded by faber. There is no file in any
watched repo for a catch-up PR to open against. `auto_pr` was not merely
redundant by 2026-08-26; it had become unbuildable as specified.

### What is lost, stated plainly

Nothing now bumps the base image. It stays on the version it bakes, and drift
against it grows monotonically as repos move forward — each new pinned version
costing one variant build and its delta on disk while hot. That steady state is
not new and not a regression: ADR-0008 chose it deliberately ("pay once per
version, never per job") and bounds it with `keep` and `idle_ttl`.

What genuinely disappears is a *signal* — "the base is far enough behind to be
worth rebuilding". It is not replaced by a new mechanism, because one is not
warranted for a judgement call made a few times a year: every heal is already an
audit record, so the evidence is on hand when an operator wants it, and the
rebuild itself is off-repo work only the operator can do anyway.

### What this does not change

`mode = "gate"` is untouched — still Phase 2, still detect-and-record with no
cancel and no status. It is a separate decision, and withdrawing `auto_pr` neither
argues for it nor against it.

### The keys stay parseable

`[playwright_drift] auto_pr`, and `auto_pr` / `dockerfile` / `arg` on a repo's
`.faber/config.toml`, are deleted from the settings UI and from `ResolvedPolicy`,
but **remain accepted and ignored** by the schemas. Both are
`deny_unknown_fields`, so deleting a key is a breaking parse change, and the two
blast radii are different sizes and both bad: faber's own config aborts startup on
a parse error, which under [ADR-0006](0006-fleet-autoupdate.md) reaches unattended
followers on the next release; a repo's `.faber/config.toml` degrades to the global
default instead, silently taking that repo's `mode` and `heal` overrides down with
it. Neither failure is worth inflicting to retire a key that costs one ignored
field, so the fields keep their `serde` defaults, are documented as withdrawn, and
are never written back — the same treatment `concurrency` already gets.
