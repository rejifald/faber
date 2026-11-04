# Working on faber

Instructions for coding agents. Read this before changing anything here.

## Everything you write here is public

Treat this repository as public. Whatever you push here is published, and git history
keeps it after a later commit removes it. A leak can't be fixed forward: once it's out,
it's a disclosure. So the rule is to leave it out from the start, not to clean up later.

The rule covers **every word you publish**, not only files:

- files and their history: code, tests, fixtures, docs, ADRs, comments
- commit messages and branch names
- pull request titles, descriptions and review comments
- issue titles, bodies and comments
- release notes, tags and discussion posts

### Never publish

- **Credentials of any kind:** tokens, keys, passwords, `.env` values, signing material,
  even expired ones. A credential that lands here is compromised. It must be rotated,
  and deleting it is not enough.
- **Specifics of an operator's environment:** hostnames, machine names, IP addresses,
  usernames, account names, LAN or VPN layout, hardware inventory, disk sizes, and
  memory or CPU figures read off a real machine.
- **Local file paths:** anything under a home directory (`/Users/<name>/…`,
  `/home/<name>/…`, `~/…` from a real machine), worktree paths, and scratch or temp
  paths. Derive a path at run time (`$HOME`), or write the role it plays instead.
  `/Library/Application Support/faber` is faber's own home and is fine.
- **Names of the projects faber runs for:** the repos, organizations, images,
  services and products whose CI runs on faber. Not in a fixture, not in a test, not
  in an incident write-up, and not in a comment that reconstructs a real failure.
- **Real configuration:** an operator's `config.toml`, grants, tokens' scopes, runner
  labels or profile names copied from a live host, and log or `audit.jsonl` excerpts
  that carry any of the above.
- **Anything about the operator's other work:** internal service names, customer
  names, private URLs, and ticket IDs from a private tracker.

The owner handle `rejifald` is fine: it is in the repository URL and the
`io.github.rejifald.faber` launchd label already.

### What to write instead

| Instead of | Write |
|---|---|
| a real repo or org | `owner/demo` |
| a real runner image | `ghcr.io/owner/my-runner` or `faber-runner:local` |
| a real host | "the builder host", "a second Mac" |
| a real path | `$HOME/…`, or the role, such as "the operator's checkout" |
| a real incident | "a watched repo", plus run, job and PR numbers, which identify nothing once the repo is gone |
| real numbers from a machine | round illustrative values, labelled as examples |

Replace the identifying detail **as you write**, not after a check complains. Do this
when you turn a real failure into a test fixture, an ADR or a bug report especially.

### Before you publish

- Re-read the diff, the commit message and the PR or issue text with this list in mind.
- Run the scanner on any text you are about to publish that is not a tracked file:
  `cargo run --example privacy_scan -- <file>...`.
- If you need real details to debug, keep them in local, untracked files or in the
  conversation, never in the tree or on GitHub.
- If something private is already published, stop and tell the owner. Don't try to
  rewrite history or delete it quietly yourself; the owner decides on rotation and
  clean-up.

### The gate

`tests/privacy.rs` checks every tracked file on every `cargo test`, so CI fails on a
leak before review does. `.github/workflows/privacy-text.yml` checks the text that is
not a file: a pull request's title, body, branch name and commit messages, and issue,
comment and review text. `release.yml` scans the release notes the same way before any
draft exists. All three run the one scanner in `src/privacy.rs`.

It matches names against keyed digests, HMAC-SHA256 under `FABER_PRIVACY_KEY`
([ADR-0058](docs/adrs/0058-the-privacy-denylist-is-keyed.md)), because a plain list
would publish the names it protects. Without the key it still checks credentials and
home paths, and says loudly that it skipped names. A hit prints the file, line and
kind, never the token: CI logs are public. Nobody keeps the list by hand:
`examples/privacy_denylist.rs` derives it on the owner's machine.

The scanner only knows the names it has been told about. It is a backstop, not a
licence to stop checking.

## No artifact is published

The source is public; artifacts are not. **Every image, package and artifact this
project touches stays private.** Don't push images, packages or builds to any public
registry. If something needs to be reachable from another host, it goes to a
**private** registry or is built there. Don't propose publishing an image as the fix
for a problem, and don't treat "it would be easier if this were pullable" as a reason
to make it public.

**faber's own release assets are the one public artifact**
([ADR-0031](docs/adrs/0031-releases-are-signed-by-github-from-a-fresh-public-repository.md)):
the binaries, the macOS app bundle, `SHA256SUMS` and its attestation, built from this
repository by its release workflow, `.github/workflows/release.yml`. Nothing else
qualifies. Runner images, operator images, and anything built from private code or on
a private machine stay private.

The concrete case this came from: an operator's runner image is a **local-only tag**.
It exists on the builder and in no registry, so faber must never auto-delete unused
runner images. A single `docker rmi` would take CI down with nothing to re-pull. The fix
is *not* to publish it. Keep the never-reap rule (see
[ADR-0007](docs/adrs/0007-faber-owns-the-docker-host.md)) and leave the image where it
is.

**The one deletion exception, and why it is not one.** faber evicts the per-version
runner-image *variants* it builds itself
([ADR-0008](docs/adrs/0008-on-demand-runner-image-variants.md)). That is not a
softening of the rule above: the rule is about **irreproducible** images, and a variant
faber built from a recipe faber owns can be rebuilt from nothing but its tag.
Reproducibility is the licence. A base image carries no faber labels at all, so it
structurally cannot be returned by the listing eviction works from. If you find
yourself widening that filter, you are removing the property that makes any of this
safe.

**What faber may delete, in full.** One thing: **an image or a git mirror faber
built**, `org.faber.kind=image` or `org.faber.kind=git-mirror`, on faber's own
judgement, because it can rebuild it from nothing it does not own (ADR-0008,
ADR-0035).

There used to be a second route, the operator's declared-recreatable compose projects
and volumes ([ADR-0016](docs/adrs/0016-reclamation-is-delegated-not-inferred.md)).
Those lived in Docker Desktop, which ADR-0026 took out of faber's hands, so
`[host] recreatable_projects` / `recreatable_volumes` are still parsed but do nothing,
and faber warns when they are set. Do not bring the route back inside faber's own VM:
nothing an operator owns runs there.

Two rules from that path still hold everywhere faber deletes:

**Read labels, never names.** Provenance is a fact something stamped on the resource
(`org.faber.kind`, and for the operator's stacks `com.docker.compose.project`), and
faber asks the daemon to filter on it. Matching image tags or volume names instead is
the "convention is not a fact" mistake ADR-0007 already diagnosed. If you are writing a
string comparison against a resource *name* here, stop.

**Never infer recreatability.** It is the one thing no label records, and three
plausible-looking inferences are all false, each disproven on a real host: a
compose-built image may be an irreplaceable runner base; a project whose `working_dir`
is deleted may still be a live stack with real data; a cache volume and the only copy
of a database look identical to every automatic signal. Do not add a heuristic that
deletes something faber did not build, and do not relax the protected set: it is the
code-level enforcement of the local-only-tag rule above, and it is load-bearing, not
belt-and-braces.

## Ask before acting outside the repo

Never run `sudo` steps yourself: `sudo faber setup`, `sudo faber uninstall`,
`deploy/install.sh` (it hands off to `sudo faber setup`) or any other installer. Never
create, change or delete accounts. Setup runs from a terminal the operator already
trusts with administrative work, never from faber and never from an agent
([ADR-0025](docs/adrs/0025-faber-runs-as-its-own-accounts.md)). Prepare the exact
commands and hand them to the operator.

## Root never executes what faber's accounts can write

`_faber` and `_faber_vm` (ADR-0025) hold no more than a daemon and a VM host process
need. Job code itself never runs as either; that is the whole point. But both can
still write files, and a file either can write is a file root must never trust. So
`sudo faber setup` and `sudo faber uninstall` refuse to run from a binary that account
could have replaced, checking every directory above it for the same thing, not only
the binary itself. The daemon's own `PATH` holds nothing an operator can write either
(no `/usr/local/bin`, no Homebrew prefix), and setup's migration opens every source and
destination through `O_NOFOLLOW`, never trusting a symlink into a file it does not
expect.

## macOS cannot compile the Linux branches: verify before you claim

`cargo test` and `cargo clippy` on a Mac never compile anything behind
`#[cfg(target_os = "linux")]`. A green local run therefore says nothing about the Linux
half of `src/platform/`, and this has already produced four rounds of CI failures that
looked impossible locally: dead code that `-D warnings` makes fatal off macOS, a
unit-file comment containing a literal placeholder token, and an assertion rejecting
`unknown`, which is the vendor field of an ordinary `x86_64-unknown-linux-gnu` triple.

Do not report a cross-platform change as verified on a macOS run alone. Either push and
read CI's Linux job, or run the suite locally in a container:

```sh
docker run --rm \
  -v "$PWD":/src \
  -v "$(git rev-parse --git-common-dir)":"$(git rev-parse --git-common-dir)":ro \
  -v "$PWD/.git":"$PWD/.git":ro \
  -w /src -e CARGO_TARGET_DIR=/tmp/lt rust:1 sh -c '
    git config --global --add safe.directory "*"
    rustup component add clippy
    cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'
```

The two `.git` mounts are what make it faithful rather than merely green: without them
`git ls-files` fails inside the container, `tests/privacy.rs` falls back to walking the
filesystem, and it reports ignored tooling files that would never be published.
`rust:1` ships without clippy, hence the `rustup component add`.

## faber owns the Docker host

faber is the Docker manager on the machine it runs on. On macOS that Docker is faber's
own: one long-lived Linux VM running Docker Engine, booted by faber's launcher
(`faber-vm`, under `_faber_vm`) through the VM supervisor, behind the pf wall
([ADR-0026](docs/adrs/0026-job-code-runs-only-inside-vms-faber-owns.md)). faber reaches
it at `DOCKER_HOST=unix://<vm_home>/run/docker.sock`, a socket the launcher bridges
over vsock to the guest's Docker and that only `_faber` may connect to. Keeping that VM
alive and its disk clear is faber's job, not the operator's and not yours.

- **Docker Desktop is not faber's.** faber never starts, quits, kills or resizes it,
  and nothing in faber points at its socket. If the operator keeps it, its containers
  are theirs. Do not "fix" CI by pointing faber back at it.
- **Reviving Docker means restarting the VM through the supervisor** (ADR-0007,
  retargeted by ADR-0026): `stop` the `linux-engine` claim, then start it again. Never
  by killing the launcher process, and never on a full disk (ADR-0024).
- **The engine's disk is never recreated.** Its bundle, `<vm_home>/linux/linux`, holds
  the only copy of the local-only runner image, so faber never deletes or replaces it.
  A new base image from setup applies to a new bundle only; moving to it is an operator
  step.

If you find yourself running `docker prune`, killing a `faber-vm` process, restarting
the VM by hand, or editing the VM's disk to unblock CI, **that is a bug report against
faber**, not a task you have completed. Implement it in faber instead. Manual
intervention is acceptable only to break an immediate deadlock, and should be reported
as such.

See [ADR-0007](docs/adrs/0007-faber-owns-the-docker-host.md) for the boundaries, in
particular what faber may never delete, and why bulk reclamation pauses admission
first. Its disk duties are now faber's own VMs plus ADR-0024's floor on the host volume
that holds their disks.

**A bigger VM is not spare capacity.** On macOS a Linux VM never gives memory back.
Every GiB it holds is taken from the machine the operator is using, and a builder host
whose VMs held most of its memory has paged itself unusable. Do not propose growing
`[linux_vm] memory_mib` to buy CI throughput. The operator declares the machine's
share, and it covers every VM faber runs, the Linux VM and the macOS VMs together: see
[ADR-0021](docs/adrs/0021-the-machine-share-is-declared-not-left-over.md).

## Shipping a fix reaches more than this machine

A follower host tracks the release channel (ADR-0006) and updates itself when a version
is published. Consequences:

- `deploy/install.sh` (which hands off to `sudo faber setup`, an operator step) fixes
  **this** Mac only. A fix reaches the fleet only through a release (ADR-0031): Actions
  → Release, run on `main`; review the draft it leaves; Publish. `deploy/release.sh`
  can start the run, but only the owner's Publish click releases. A follower then waits
  out `[update] maturity_secs` (24 hours unless set) before installing. Until the
  ADR-0031 cut no release can install, so hosts are updated by hand: see
  [docs/manual-update.md](docs/manual-update.md).
- A follower does not hammer a release it cannot install. A refused release is held for
  one poll interval, and one GitHub could not answer about backs off from 60 s up to the
  poll interval. A new tag ends the hold, and so does a restart; *Check now* does not.
- Deleting a published release never rolls a host back. It only stops hosts that have
  not installed it from picking it up. An urgent fix is installed by hand.
- A bad release propagates automatically, to a machine nobody is watching. Anything
  host-affecting must be conservative, rate-limited, and configurable off.
- **A release must not withdraw a capability a host already relies on.** The rule for
  turning a feature *on* is well understood: it ships off, and a host that didn't ask
  is unchanged. The mirror is easy to miss and has bitten twice: narrowing a default is
  also a behaviour change, and it arrives the same unattended way, except its symptom
  is a failing job rather than an unused feature.

  So when a new key tightens something, an **absent** key keeps the old behaviour, and
  the new state is made loud instead: warn at every boot, show it distinctly in the
  dashboard, and have the settings form write the value explicitly the first time it
  is saved. Say in the ADR that flipping the default is a future, release-noted change:
  an indefinite third state is a fudge, a scheduled one is a migration. `dind`
  (ADR-0013) and `[gate]` (ADR-0014) are both built this way; copy the shape rather
  than re-deriving it.

  Refusing to boot on a config the release did not write is *not* the safe option: it
  is a dead daemon with ADR-0006's rollback flapping against it. Reject bad input where
  a human is looking (`faber check-config`, the settings save); warn where they are
  not.

  [ADR-0027](docs/adrs/0027-github-access-is-an-explicit-fine-grained-grant.md)'s
  removal of `gh` is one deliberate exception. It is run by hand on every host, with
  followers' updates paused first, and the ADR says why.
  [ADR-0052](docs/adrs/0052-a-native-profile-is-a-config-error.md) is another:
  `backend = "native"` became an error while every host was still updated by hand,
  behind `faber check-config`.
  [ADR-0059](docs/adrs/0059-the-gate-runs-again-when-a-job-starts.md) adds a third: a
  `workflow_dispatch` or `repository_dispatch` made with a job's own token is refused on
  every host, whatever the actor lists say, unless `[gate] allow_job_token_dispatches`
  turns such dispatches back on. Such a run executes whatever code a job pushed with
  that token, under the dispatched workflow's labels and secrets, and job code is
  ADR-0025's adversary; the owner chose refusal over a warning on existing hosts. The
  actor lists judging a pull request's author and a run's original actor ship with it.
  Both ship while hosts are updated by hand, and each refusal is recorded with its
  reason.
  [ADR-0025](docs/adrs/0025-faber-runs-as-its-own-accounts.md#addendum-registry_auth-is-held)'s
  addendum is a fourth: `[registry_auth] enabled = true` is refused by `validate()`, and a
  daemon booting with it on warns, shows the `registry_auth_held` notice and pulls
  anonymously, because a credential mounted into a job is readable by its code. The
  section returns once faber pulls images on the job's behalf. None is a precedent.

Drain before installing rather than letting the swap force-kill runners: a forced
install has cost real jobs. Use `faber drain` (`POST /api/drain` on the control socket,
ADR-0028): it cancels parked claims and stops claiming, and you wait until no claim in
`/api/state` is `admitted`. Do not pause and wait for `inflight: 0`. Since
[ADR-0023](docs/adrs/0023-pause-stops-what-starts.md) a paused host keeps its parked
claims parked, so while jobs are queued the list never empties.

## Design

The design rules live in [`docs/design/guidelines.md`](docs/design/guidelines.md);
never copy them elsewhere. Every pull request that changes UI (a screen, block, kit
component, token, copy, icon or navigation) is reviewed against them before it merges,
and one with open design-review changes does not merge.

## Issue and pull request numbers

A bare `#123` in text that came with the code from the earlier repository refers to
that repository, `rejifald/faber-legacy`, which is private (see `docs/README.md`).
When you write a new reference, link it in full or name the repository.
