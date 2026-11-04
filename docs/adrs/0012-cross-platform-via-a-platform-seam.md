---
title: Cross-platform via a platform seam, and what it costs elsewhere
type: adr
status: accepted
date: 2026-09-03
owner: rejifald
---

# ADR-0012: Cross-platform via a platform seam, and what it costs elsewhere

## Status

accepted (2026-09-03). Linux is implemented; Windows is expressible behind the
seam and deliberately not implemented (see *Windows*, below).

## Context

faber has been macOS-only since it existed: a launchd LaunchAgent, `~/Library`
paths, Docker Desktop, a menu-bar app. The README of the time had long
promised Linux and Windows "coming" without anything behind it.

The port looked larger than it is, and the first useful thing this work produced
was a measurement rather than a patch. Of ~29,000 lines of Rust, the
platform-coupled surface is roughly 3%, and it is concentrated at the edges:
`setup.rs` (launchd + the `ci` user), `host.rs` (reviving Docker Desktop),
`update.rs` (install paths and asset names). The load-bearing middle is already
portable — `reconcile.rs` is 7,800 lines with 7 platform references,
`github.rs` 1,900 lines with one.

Two facts settled the approach:

- **There are zero `cfg(target_os)` gates in the codebase.** Nothing was ever
  conditionally compiled, so nothing is silently skipped anywhere.
- **faber already builds and passes its full test suite on Linux, unmodified.**
  CI's `ubuntu-latest` leg compiles the binary and runs all 424 tests green.

So this is not a compile port. Every macOS dependency is a *runtime* shell-out
or a hardcoded path, and the work is to introduce a seam, not to chase compiler
errors. That distinction is why this ADR exists: a port that only had to satisfy
the compiler would need no decision record. This one changes what two existing
ADRs promise, and those changes should be written down rather than discovered.

## Decision

### One seam, `src/platform/`, selected at compile time

`mod.rs` (paths), `host.rs` (Docker daemon control), `service.rs` (the service
manager), each `#[cfg(target_os)]`-selecting a `macos` / `linux` / `windows`
implementation.

Plain functions, not a trait object. faber only ever runs under one OS per
process, so there is nothing to dispatch over dynamically; a trait would add a
vtable and an impl block for no behavioural gain. The parts worth testing are
pure (path construction, `docker info` output parsing) and are tested directly.

### macOS output is frozen, byte for byte

Every macOS path reproduces exactly what the ~25 inline
`std::env::var_os("HOME")` call sites produced before, and
`platform::macos::tests::macos_paths_match_todays_literals` asserts each one
against its literal string. Installed hosts depend on those exact paths; the
regression test is what makes it safe to refactor around them. Changing macOS's
output now requires a migration plan, not just a passing build.

### Linux follows XDG; the service is a **user** unit

`$XDG_CONFIG_HOME/faber` for config, `$XDG_STATE_HOME/faber` for logs, audit
trail and update state, `$XDG_CACHE_HOME/faber` for downloaded runner binaries,
each with the spec's own fallback. There is no installed Linux base to stay
compatible with, so these are the conventional locations rather than a
reproduction of anything.

The systemd unit is `--user`, for the same reason the macOS one is a LaunchAgent
and not a LaunchDaemon: faber needs the operator's own `gh` credentials and
Docker group membership, which exist in a user session. Three of its keys are
load-bearing and easy to get wrong:

- `Restart=always`, **not** `on-failure`. The self-updater (ADR-0006) swaps its
  binary and then exits cleanly, relying on the manager to start the new one.
  `on-failure` would not catch `exit(0)` and would strand every host on the
  version it was installed with.
- `StartLimitIntervalSec=0`. systemd's default circuit breaker gives up after
  five restarts in ten seconds and leaves the unit `failed`. That directly
  defeats ADR-0006's rollback, which detects a crash-looping version *by being
  restarted* and counting boot attempts. A host that systemd has given up on
  never gets to roll itself back. `RestartSec=2` still paces a real crash loop.
- `KillMode=process`. faber keeps one `docker wait` child per in-flight runner
  and manages their lifetime itself across a drain; the default
  `KillMode=control-group` would signal those watchers out from under the drain
  still using them. Note precisely what this is *not*: the runner containers are
  unaffected either way, because they are started detached and live in dockerd's
  cgroup. The signal-proxying bug ADR-0011 fixed came from *attached*
  `docker run`, which no longer happens. This is about the watchers.

`loginctl enable-linger` is required for a Linux host to survive logout, and is
deliberately **not** run by the installer — it is the same conscious,
security-relevant opt-in as macOS auto-login, and is stated rather than assumed.

### Release assets are derived from the build target

`build.rs` stamps `FABER_TARGET`, and the updater asks for
`faber-<target-triple>`. A Linux binary therefore *cannot* request the macOS
asset — there is no runtime choice to get wrong, and no way for a mixed fleet to
cross-install. On aarch64 macOS this still renders `faber-aarch64-apple-darwin`,
which is what hosts already running v0.3.x look for, so the channel is unbroken
in both directions.

The app-bundle asset became `Option<&str>`, `None` off macOS. Requiring it
everywhere would have failed every Linux update with "release has no asset" and
pinned the host to its install version — the exact stranding ADR-0006 exists to
prevent, and invisible in practice, because a host that never updates never
reports anything.

## What this costs elsewhere

The point of writing this down is that two existing ADRs mean something weaker
on Linux, and pretending otherwise would be the real failure.

### ADR-0007's promise is partial on Linux

"faber owns the Docker host" was written against launchd plus a GUI
application. On macOS, reviving Docker Desktop is a quit/kill/relaunch sequence
a user-session process can perform unaided.

On Linux, dockerd is a **system** service. A user-session faber usually has no
standing privilege to restart it, so `systemctl restart docker` will typically be
denied. Revival is therefore best-effort by design: the denial returns as an
ordinary error, the caller records a fault, and Gate 0's existing fail-closed
path keeps admission shut — a degradation path that already exists and is
tested. An operator who wants ADR-0007's full promise grants exactly one command
via polkit or sudoers. Installing faber must not *require* that.

So on Linux, ADR-0007 holds for disk hygiene and only conditionally for
liveness. That is a real difference in what the software guarantees, not a
packaging detail.

### The disk floor had to become platform-specific, because sharing it was a bug

`docker_data_path()` resolved `$HOME/Library/Containers/com.docker.docker` and
fell back to `$HOME` — sound on macOS, where Docker's VM disk image sits on that
same volume. On Linux, dockerd writes to its `data-root` (default
`/var/lib/docker`, operator-configurable), frequently a different filesystem
entirely. Reusing the macOS logic would have measured the wrong volume: the
floor would either never fire while Docker's disk filled — the ADR-0007 incident,
reproduced — or fire constantly and prune for nothing.

Linux therefore asks `docker info --format '{{.DockerRootDir}}'`, and returns
`None` rather than a guess when that cannot be read. macOS keeps measuring the
container directory, because `DockerRootDir` there names a path *inside* the VM
that the host's `df` cannot stat. An unknown path is fed to the pre-existing
"unreadable free space is not evidence of pressure" rule, so the failure
direction is *not pruning* rather than pruning the wrong disk — the right way to
fail for a system whose standing rule is never to delete an image it cannot
re-pull.

### The autolimiter's numbers survive; its meaning does not

The memory probes go through `docker info` and a throwaway container reading
`/proc/meminfo`, never an OS API, so they port with no code change at all. But
what they *mean* changes: on macOS `MemTotal` is the Docker VM's walled
allocation, where essentially only Docker competes. On Linux it is the whole
machine, shared with every process faber cannot see.

`reserve_mib`'s default of 5120 is documented as "Docker Desktop overhead +
growth cushion for co-tenant dev stacks" and is simply wrong for Linux, where
the reserve must cover the entire host OS. Gate 1's budget arithmetic also
becomes a rougher approximation, shifting more weight onto Gate 2's live probe.
This is a retune with a rationale, and is deliberately left open rather than
guessed at here.

### `--privileged` dind is riskier on Linux

Same flag, higher stakes. On Docker Desktop, a nested-container escape lands
inside Apple's VM; on native Linux it is host root. This does not change the
threat model faber already documents — it runs trusted code only, and the gate
refuses public repos — but it sharpens it. It is a further reason fork-PR
support would need a VM boundary per job rather than a container, on any
platform, and more obviously so on Linux.

### There is no menu-bar app on Linux

The Tauri tray app stays macOS-only. `installed_app_dirs()` returns empty there,
the updater skips the app asset, and the app refresh is a no-op. A Linux
operator gets the daemon, the localhost control panel and `faber status`.

## Windows

Implemented far enough to compile and no further, and this is deliberate rather
than an omission of effort. Windows forks hardest exactly where faber is least
abstract: Docker there either runs Linux containers in a VM — in which case the
macOS assumptions mostly carry — or Windows containers, which are a separate
image ecosystem the shipped runner Dockerfile does not address at all. Choosing
between those is a product decision, not a porting one.

Every `platform::windows` function therefore either returns a correct value
where one exists (paths) or a clear "not implemented on Windows yet" error where
it does not. An honest unimplemented behind a working seam is worth more than a
half-port that appears to work, and the seam makes finishing it cheap when the
container question is answered.

## Alternatives considered

**Rootless Docker on Linux.** Genuinely attractive: the operator can restart
their own daemon, keeping ADR-0007 whole without any privilege grant, and
`--privileged` is far less dangerous when "root" is a mapped unprivileged user.
Rejected because dind under rootless is operationally fiddly, and making the
default install depend on it trades a documented limitation for an
under-supported happy path. Worth revisiting if the privilege grant proves to be
a real adoption obstacle.

**A system unit instead of a user unit.** Would make `systemctl restart docker`
work without a grant. Rejected for the same reason faber is a LaunchAgent and
not a LaunchDaemon: it needs the operator's `gh` credentials and Docker group
membership. Running CI as root to simplify daemon restarts is the wrong trade.

**A runtime `Platform` trait.** Rejected as noted above — one OS per process
means nothing to dispatch over.

**Deriving the target triple from `cfg!` at the use site.** Rejected: a triple
assembled from `target_os`/`target_arch` guesses at the vendor and libc fields
and would drift from what `deploy/release.sh` actually names the asset. Cargo
gives build scripts the real `TARGET`, so it is stamped there.

## Consequences, and what is still unfinished

- Linux hosts **cannot autoupdate yet.** The Rust side asks for its own
  platform's asset correctly, but `deploy/release.sh` still publishes macOS
  assets only, and the builder Mac cannot cross-build Linux binaries. Closing
  this means deciding whether releases move into CI — which rewrites ADR-0006's
  premise that one designated Mac cuts them — or whether CI appends assets to a
  Mac-cut release, which races the `SHA256SUMS` generated at cut time. Until
  then, Linux installs from source and the channel is macOS-only.
- The install scripts (`install.sh`, `deploy/install.sh`, `deploy/uninstall.sh`)
  still gate on `uname == Darwin`.
- The dedicated-`ci`-user isolation (`dseditgroup`, `sysadminctl`) is macOS-only
  and now returns a clear "not supported on this platform yet" rather than
  shelling out to tools that do not exist. Linux's equivalent (`useradd`, and a
  different isolation model) is unbuilt.
- `--topic` and `--labels` in `main.rs` still default to `selfhosted-ci` and
  `self-hosted,macOS,ARM64`. Both are stale, and the label default is now
  actively wrong: a flags-only Linux host would advertise macOS/ARM64 runners.
  The installer's seeded config masks it; the defaults should still be fixed.
