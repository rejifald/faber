---
title: Fleet autoupdate via designated-builder releases
type: adr
status: accepted
date: 2026-08-25
owner: rejifald
---

# ADR-0006: Fleet autoupdate via designated-builder releases

## Status

**Amended 2026-10-09 by
[ADR-0031](0031-releases-are-signed-by-github-from-a-fresh-public-repository.md).**
The deliberate human act is now the owner's Publish click, on a draft built and
attested from `main`, and not a tag pushed from the builder. A follower installs
a release only after it has matured
([#99](https://github.com/rejifald/faber-legacy/issues/99)). Nothing fires on a merge,
as before. The builder no longer cuts releases. The reason given below for keeping
a host that runs work-in-progress builds off autoupdate still holds.

**Amended 2026-09-03 (see also ADR-0012).** Releases are no longer built on one
designated Mac. Cross-platform support means a release carries an asset per
platform, and no single machine can build them all — the builder Mac has no
Linux toolchain. `.github/workflows/release.yml` now builds every target and
publishes, triggered by the tag push that `deploy/release.sh` still performs.

What this amendment deliberately does **not** change is the part this ADR argued
for: publishing stays a deliberate human act. Nothing fires on a merge to
`main`; a release happens because someone ran `deploy/release.sh` and pushed a
tag. An auto-updating fleet must not be able to acquire a new version by
accident, and moving the build to CI would have made exactly that easy.

Two properties the workflow has to preserve, both of which the old single
`gh release create` got for free:

- **Atomicity.** A release assembled from several jobs has a window in which
  some assets exist and others do not. A follower polling then would see a
  release whose `SHA256SUMS` or binary is missing and record a failed stage
  against a version that was fine. The workflow therefore creates the release as
  a **draft**, attaches everything, and only then publishes — and a follower
  tracks `/releases/latest`, which GitHub excludes drafts from, so the release
  does not exist for the fleet until it is complete.
- **No partial matrix.** The publish job requires the whole build matrix, so one
  failed target blocks the release rather than shipping a subset. A missing
  asset is not a cosmetic gap: a host on that platform is stranded, silently,
  which is the failure this ADR exists to prevent.

Each target is built on a runner native to it, so every binary is executed and
asked for its version before it can be published. That check is the only thing
between a broken build and a fleet that installs it automatically; a
cross-compiled target could not perform it, and adding one should be argued for
rather than slipped in.

accepted (2026-08-25) — implemented this session across the daemon
(`src/update.rs`, `src/config.rs`, `src/github.rs`, `src/web.rs`,
`src/reconcile.rs`, `src/settings.rs`) and the release/deploy tooling
(`deploy/release.sh`, `install.sh`, `deploy/install.sh`). Opt-in and off by
default (`[update] enabled = false`), so an existing single-host deployment —
including every deployment that predates this ADR — is unaffected until it
explicitly turns this on. Realises the multi-host fleet [ADR-0004](0004-demand-based-provisioning.md)
assumed but left someone to keep in sync by hand.

## Context

ADR-0004 made "more than one Mac" a supported shape: two or more faber
instances sharing a pool of repos, coordinating through GitHub's own runner
list. What it didn't address is that each of those hosts is upgraded today by
sitting down at its keyboard (or SSH-ing in), `git pull`, `cargo build
--release`, reinstall the LaunchAgent. Fine for one machine watched by the
person who just wrote the patch; it does not scale past a couple of hosts, and
it means a bug fix landed on `main` sits unfixed on every follower until
someone remembers to walk over and redeploy.

The fleet needs a follower to pick up a new build on its own — without an
operator physically present, and, because faber's entire job is running a CI
job to completion, without ever killing one mid-flight. Three questions shape
the design: **who builds** the bytes a follower installs, **when** a follower
is allowed to swap them in, and **what happens** if the new binary is bad.

## Decision

### Who builds: a designated builder Mac publishes release assets

One Mac — whichever one an operator chooses — runs `deploy/release.sh` to cut
a release: bump the version, tag, build `cargo build --release` and the Tauri
app bundle, and publish both plus a `SHA256SUMS` to a GitHub Release on the
private repo. Every other host in the fleet only ever *consumes* what that
release publishes; none of them compile faber themselves.

Two alternatives were rejected — see below — because they both reintroduce
exactly the problem faber's whole existence is a reaction to: paying to run
CI (a build is CI) on hardware or minutes you don't need to.

### When: explicit release cuts, not auto-publishing every green `main`

`deploy/release.sh` is a command an operator runs, not a job that fires on
every push to `main`. A build that ships to every Mac faber runs on is a
different kind of event than a commit landing on a branch — it deserves a
human deciding "yes, this is the one," with a changelog, not a bot publishing
whatever happened to be green when the poller last looked. Nothing about the
follower side assumes releases arrive on any particular cadence; `poll_interval_secs`
(default 1800s) is how often a follower *checks*, not how often the builder
*publishes*.

### When: wait for natural idle, escalate to a drain after 6h

A follower that has verified and staged a new binary does not swap it in
immediately — that would kill whatever CI job is running. It waits in
`WaitingIdle` for `Orchestrator::view().inflight` to go empty on its own,
which on a host with any reasonable job cadence happens within minutes. Only
if `escalate_after_secs` (default 21600s = 6h) passes without a natural idle
window does it call the *same* `begin_drain()` / `await_drain()` pair SIGTERM
handling already uses: stop claiming new jobs, cancel parked claims (they hold
no container and may never win admission — see the escalation-guard note on
`begin_drain` in `src/reconcile.rs`), and wait only on claims that still back
a running container. Reusing this path rather than inventing a second one
means autoupdate inherits the fix for issue #16 (a drain that hung on
admitted-but-actually-gone claims) for free, instead of being a second place
that bug could recur.

### When: swap-then-`exit(0)` into `KeepAlive`, not a self-directed `launchctl` call

Once idle (or drained), the update commits: back up the current binary and
config, atomically replace `bin/faber` (write-temp, `chmod`, `rename` — never
a partial file mid-swap), log loudly, and call `std::process::exit(0)`. It
never calls `launchctl` on itself. The LaunchAgent plist already has
`RunAtLoad` + `KeepAlive` (`deploy/io.github.rejifald.faber.plist`) —
*"if faber ever exits — a crash, or its own graceful SIGTERM drain — launchd
brings it straight back"* — so exiting cleanly after the swap is all a running
process needs to do to end up executing the new bytes.

The alternative — the daemon issuing its own `launchctl bootout` +
`bootstrap` — is a real trap this repo has already been burned by once,
documented in `deploy/install.sh`'s own installer: `bootout` returns as soon
as it has *asked* launchd to tear the job down, not once the label is
actually free, so a `bootstrap` on the next line races it. That script's fix
is a polling loop bounded at 660s (just past the plist's 600s `ExitTimeOut`)
with a retry-on-`bootstrap`-failure loop on top, because losing the race gives
the opaque `Bootstrap failed: 5: Input/output error`. That machinery exists
because an *external* installer has no other way to reload a label it doesn't
own the lifecycle of. A running daemon updating *itself* doesn't have that
problem — it already has a clean-exit path (the same one SIGTERM already
drives), and `KeepAlive` is specifically the primitive for "bring this back
after it stops of its own accord." Reimplementing the installer's bootout/wait
dance from inside the process it would be tearing down buys nothing but the
same race, self-inflicted.

### What happens if it's bad: boot-marker auto-rollback

Every commit writes `update/marker.json` (`{from, to, attempts, written_ms}`)
*before* the swap, and `rollback_if_needed` — called first thing in the `run`
command, ahead of config loading — inspects it every boot:

- No marker: normal boot, nothing to do.
- Marker's `to` doesn't match the running version (ignoring build metadata):
  something else replaced the binary since (a manual install); delete the
  marker and say so.
- `attempts >= 2`: this is the *third* boot in a row that hasn't survived
  probation. Restore `bin/faber.prev` → `bin/faber` and
  `update/config.prev.toml` → the live config path, quarantine the version,
  delete the marker, `exit(0)` so `KeepAlive` starts the restored binary.
- Otherwise: increment `attempts`, rewrite the marker, and a spawned task
  deletes it after `probation_secs` (default 120s) if the process is still
  alive then.

Two failed boots are given the benefit of the doubt — a flaky network blip
during startup is not the same failure mode as "this binary is bad" — and
only the third consecutive non-survival rolls back. Combined with `KeepAlive`,
a binary that dies on boot would otherwise crash-loop forever with no way out
except someone walking over to the machine; this closes that loop
automatically.

### Why the config is snapshotted alongside the binary — the subtle part

`update/config.prev.toml` is not a nicety; without it, rollback does not
actually recover. `FileConfig` (`src/config.rs`) carries
`#[serde(deny_unknown_fields)]` on the struct itself, and `main()` loads it
with `FileConfig::load(...)?` (`src/main.rs`) — a parse failure propagates
straight out of `main`'s `anyhow::Result<()>` and the process exits non-zero.

Now trace what rolling back *only* the binary would do. Say a later version
introduces a new top-level config key — plausible, since that is exactly what
this ADR's own `[update]` section is: a key that did not exist before this
release. Any host that has run the newer binary long enough for its Settings
page to save (`src/settings.rs`'s save rewrites the whole file from data) now
has that key sitting in its live `config.toml`. If that newer binary then
needs to roll back to `bin/faber.prev` — the one *without* that field in its
`FileConfig` — and rollback restored the binary alone, the "recovered" old
binary would immediately hit `deny_unknown_fields` on a key it has never
heard of, exit 1, and `KeepAlive` would restart it into the *identical*
failure. The rollback would not just fail to help — it would manufacture a
second, unbreakable crash loop, on the binary that was supposed to be the
known-good fallback.

Snapshotting the live config *at commit time* — before the new binary has
ever touched it — captures a copy guaranteed compatible with the binary it's
paired with, because it's literally the config that binary was running on a
moment before. Restoring binary and config together is what makes "roll
back" actually mean "back to a state that boots," not just "back to older
bytes paired with a config that may have moved on without them."

### Integrity, not authenticity

Every downloaded asset is verified against `SHA256SUMS` — published in the
*same* release, over `reqwest` with the follower's own `gh` auth — before its
`chmod 0755` or its `check-config` dry-run ever runs. This catches a truncated
download, a bit-flipped transfer, a corrupted asset. It does **not** prove the
release came from the real builder rather than anyone who could write to the
repo's releases: the digest travels in the same channel as the thing it
checksums, so a party able to forge one release asset can just as easily
publish a matching `SHA256SUMS` for it. That's an integrity check, not an
authenticity one, and this ADR does not claim otherwise.

That gap is acceptable *here* specifically because the repo is private and
every release consumer already authenticates as an account with read access
to it — the same trust boundary faber's whole design already leans on (see
the README's "Safety" section). It stops being acceptable the
moment faber ever ships prebuilt binaries to the general public. The repo
already carries an MIT `LICENSE` and a README written with a public audience
in mind, so "public source, no prebuilt binaries" is the state to design
against today — the moment that ever changes to "and here are binaries,"
code-signing the release assets is the prerequisite, not a future nice to
have. Recorded here so it isn't quietly forgotten when that day comes.

### Opt-in, and the builder Mac's own exemption

`[update].enabled` defaults to `false`. Installing faber must not, on its
own, hand it permission to replace its own binary — that is a big enough
grant of trust that it has to be a deliberate choice, made by whoever is
going to be asleep when it happens. `install.sh` and `deploy/install.sh` both
offer to turn it on right after a successful install, default the prompt to
**No**, and (`install.sh` specifically) do not let `--yes` sweep it in either
— `--yes` is documented as "take the default for every prompt" for the
toolchain installs (Xcode CLT, Rust, Node) that script already gates, and
autoupdate is a different *kind* of decision than "may I install a compiler."
A provisioning script running fully unattended gets faber with autoupdate
off, same as a human who just pressed Enter.

The designated builder Mac is expected to stay off this even if an operator
turns it on fleet-wide: it routinely runs work-in-progress builds with an
unbumped or ahead-of-`Cargo.toml` version (`build.rs`'s `+g<sha>` suffix
exists exactly to make those distinguishable), and having the one machine
that *cuts* releases also *auto-consume* whatever the fleet happens to be
running is the wrong direction for that data to flow.

## Consequences

- **A fleet upgrades on one command.** `deploy/release.sh` on the builder,
  and every opted-in follower converges within one `poll_interval_secs`
  window (worst case) plus however long it takes to go idle.
- **The builder is never behind its own release.** `release.sh`'s last step
  is installing the release it just cut, locally — the operator who just
  published a fix is running it before anyone else's follower even polls.
- **A bad release self-heals per host, not fleet-wide.** Rollback is local
  (each host restores its own `bin/faber.prev`); there is no fleet-wide
  "everyone is now stuck" failure mode, but there is also no fleet-wide
  circuit breaker — a bad release that *passes* every host's boot probation
  (a bug that only manifests under specific runtime conditions, not at boot)
  is not caught by this mechanism at all. Boot-time crash-looping is the
  failure mode this design catches; a silent behavioural regression is not,
  and isn't in scope here.
- **New on-disk state.** `$ROOT/update/{staged,marker.json,quarantine.json,config.prev.toml}`
  and `bin/faber.prev`, all under the existing `UpdatePaths` root — nothing
  new outside `Application Support/faber`.
- **New trust surface, scoped deliberately.** A follower now executes bytes
  it did not compile. That surface is exactly the private-repo + `gh` auth
  boundary faber already depends on for everything else it does (cloning,
  polling, provisioning) — this doesn't introduce a *new* trust boundary, it
  extends the existing one to cover one more artifact type.
- **`FileConfig` gains a permanent forwards-compatibility obligation.** Any
  future config field is now something an *older* binary might have to
  tolerate seeing during a rollback, not just something a newer one has to
  parse. This was already implicitly true (an operator could always downgrade
  by hand) — autoupdate just makes it a path that's exercised automatically,
  and therefore one that needs to actually work.

## Alternatives considered

- **Every host builds from source.** Zero new trust surface — a host already
  trusts `main`'s source, and "build what you already trust" sounds strictly
  safer than "download bytes." Rejected on two independent grounds: an LTO
  release build (`[profile.release] lto = true, codegen-units = 1` in
  `Cargo.toml`) is real, sustained CPU and memory load, and on a host also
  running the autolimiter's memory-budget admission (`src/limiter.rs`), a
  fleet-wide simultaneous rebuild contends with the very CI capacity faber
  exists to protect. And "every host builds independently" only guarantees *same
  source*, not *same bytes* — subtly different toolchain versions, or a
  dependency that re-resolves differently between two builds run hours apart,
  can produce a fleet where nominally-identical versions behave differently
  in ways nobody can diff. A single designated build is one artifact,
  checksummed once, identical everywhere.
- **GitHub-hosted macOS runners build the release.** Offloads the build
  entirely off the fleet's own hardware, and would be the obvious answer for
  a project that already used GitHub-hosted CI. Rejected for the same reason
  faber exists at all: GitHub-hosted minutes are billable, and a private repo
  is precisely the case where they cost real money instead of being
  included-tier free. Paying GitHub to build the tool whose entire purpose is
  not paying GitHub to run your CI is the wrong shape, independent of any
  technical merit.
- **Auto-publish every green `main`.** No separate release-cutting step,
  fastest possible propagation of a fix. Rejected: a fleet-wide binary swap
  is a bigger blast radius than a branch commit, and deserves a human
  decision point and a changelog entry, not "whatever was green when the
  poller last looked." `deploy/release.sh` still makes cutting a release a
  single command — the friction removed is *building and publishing*, not
  *deciding to*.
- **The daemon reloads itself via `launchctl bootout`/`bootstrap`.** Considered
  and rejected in detail above (see "swap-then-`exit(0)`") — this repo has
  already paid for the bootout-race lesson once, in the installer; there's no
  reason to pay for it again from inside the process being torn down, when
  `KeepAlive` already does the "bring it back" job for free.
- **Rely on an operator to notice and redeploy.** The status quo. Correct,
  and fine at one or two hosts watched closely. Rejected as the fleet grows
  past what one person reliably babysits — exactly ADR-0004's framing applied
  to the update problem instead of the provisioning one.
- **Sign release assets now, ahead of any public-distribution need.** Would
  close the integrity-vs-authenticity gap immediately. Rejected for *now*
  specifically because every consumer today already authenticates into the
  same private repo the unsigned asset lives in — signing adds real
  operational cost (a key to generate, store, and rotate) for a threat model
  (a compromised repo credential forging a release) that a compromised repo
  credential could already defeat in other ways (pushing directly to `main`).
  Recorded above as the prerequisite the moment that stops being true.

## Validation against real circumstances

To validate once a second host opts in:

1. **A real release reaches a follower without intervention.** Cut a release
   from the builder; confirm a follower with `[update] enabled = true` moves
   `Idle → Checking → Downloading → WaitingIdle → Idle` (new version) within
   one poll interval of going idle, with no operator action on the follower.
2. **A job in flight is never interrupted.** Cut a release while a follower
   has an admitted, running job; confirm the update sits in `WaitingIdle`
   until that job completes (or, forced via a long test, until
   `escalate_after_secs` triggers `Draining` and the job still finishes
   before the swap).
3. **A boot-incompatible release actually rolls back.** Ship a follower a
   binary that immediately errors against its own config (simulating the
   `deny_unknown_fields` scenario above); confirm two crash-restarts, then a
   restore to the previous binary *and* config, and that the bad version
   lands in `quarantine.json` so it isn't retried.
4. **Checksum mismatch fails closed.** Corrupt one byte of a published asset;
   confirm the follower deletes the staged file, reports `Failed`, and does
   not retry that exact version again this tick.

_Live results: pending — no second host has opted in yet as of this ADR._

## Amendment (#92, under ADR-0031): probation also proves the updater

Surviving `probation_secs` no longer clears the marker on its own. First the new
binary re-verifies the release it was installed from, with the update path's own
code and the Updater's own GitHub client (`src/self_check.rs`): the release for
its tag, `SHA256SUMS` and `SHA256SUMS.sigstore.json`, the real provenance
verifier, and the digest of its own executable against the sums. While the
repository is private that client is authenticated; once ADR-0031's cut makes the
updater anonymous, the self-check follows.

- **Refused** is narrow, and only about the bytes, on a release that was fetched
  successfully: the verifier refused on the bundle (including a release with no
  bundle asset; a refusal that rests on the tag's state in GitHub does not
  count: tag missing, a tag name or object sha faber will not use, or a source
  commit that differs from what the tag resolves to now, all Unavailable here),
  this binary's digest differs from the sums, or the sums have no entry for it.
  Strict. The marker is raised to the crash-loop threshold and the process exits,
  so the next boot restores the previous binary and quarantines this version.
- **Unavailable** is everything else, including **every HTTP status error (401,
  403, 404, 429, 5xx)**, network and TUF. A deleted or yanked release must never
  roll a host back (a workflow token can delete a published release, per
  ADR-0031's spike). Retried with backoff (2 min doubling to 20 min) for two
  hours: attempts at 0, 2, 6, 14, 30, 50 minutes, about 30 API requests in the
  first hour.
- **Unproven** (still Unavailable after two hours): the release is kept and the
  marker cleared, because a marker that outlives the window would roll the host
  back on two unrelated restarts days later. It is reported, not silent:
  `/api/state` `update.self_check.status` is `unproven` with the last reason,
  persisted across restarts until an update verifies, and the web dashboard and
  tray popover show a warning.

`/api/state` `update.self_check` is `pending`, `verified`, `unproven` or
`refused`; it is the check ADR-0031's upgrade sitting confirms before updates are
re-enabled. A hand-installed binary has no marker and skips all of this.
