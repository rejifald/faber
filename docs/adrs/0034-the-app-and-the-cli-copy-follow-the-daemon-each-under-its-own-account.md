---
title: The menu-bar app and the CLI copy follow the daemon, each under its own account
type: adr
status: accepted
date: 2026-10-17
owner: rejifald
---

# ADR-0034: The menu-bar app and the CLI copy follow the daemon, each under its own account

## Status

accepted (2026-10-17). The owner settled it in a design session on 2026-10-17.
It decides [#80](https://github.com/rejifald/faber-legacy/issues/80) and
[#81](https://github.com/rejifald/faber-legacy/issues/81).

Amends:

- [ADR-0025](0025-faber-runs-as-its-own-accounts.md): "the tray app stays the
  operator's" now includes how it updates, and root gains one more command that
  reads `_faber`'s files.
- [ADR-0031](0031-releases-are-signed-by-github-from-a-fresh-public-repository.md):
  the app and the CLI copy verify releases with the same policy, each with its
  own copy of the verifier.
- [ADR-0006](0006-fleet-autoupdate.md): the daemon no longer refreshes the app
  bundle.

It ships in v0.4.0, the cut release, so v0.4.1 is the first release that reaches
the app and the CLI copy without a hand install.

**Amended 2026-10-28.** `sudo faber setup` does the app's first install, from the
release's assets: `faber-app-aarch64-apple-darwin.tar.gz`, found where the guest
agent is (the launcher's directory). It is unpacked as the operator, never by
root, its `CFBundleShortVersionString` must equal setup's own release version,
and it lands at `/Applications/faber.app`, the operator's all the way down, so
its own updater can swap it. Setup never downgrades an app that updated itself,
and a re-run plans nothing. Updates stay the app's own, as below. Uninstall
leaves the app, and says how to remove it.

**Amended 2026-10-31.** Every setup run moves the app along with the daemon, not
only the first. Until the cut, hosts are updated from source builds
(`docs/manual-update.md`), and a source build keeps the release version it started
from: a v0.3.12 binary built a week later still reads `0.3.12`. So did the app
installed the week before, so setup counted it current. The daemon moved and the
menu bar stayed a week behind. The bundle now carries a `FaberBuild` key in its
`Info.plist`: the build's whole version, `X.Y.Z+g<sha>`, in the same format the
daemon's `build.rs` derives. `desktop/scripts/assemble.mjs` writes it before every
app build. Setup keeps an app of its own version only when the stamp is its own
binary's. Any other stamp, or none, is replaced from the archive beside the
launcher, and at apply the unpacked bundle must carry the binary's stamp. The
source-build recipe builds that archive into the directory setup runs from. The
rest is unchanged: unpacked as the operator, never downgraded below a newer
version, and updates between setup runs stay the app's own.

## Context

Until the cut, the daemon runs as the operator, and its updater refreshes the
menu-bar app after its own swap: it quits it, removes it and copies it in again
(`update_app_bundle` in `src/update.rs`). The menu bar and its popover are one
asset: the popover's frontend is bundled inside `faber.app`.

ADR-0025 moves the daemon to the hidden `_faber` account, and it keeps two
copies of the binary, so that root never executes a file `_faber` can write:

| Copy | Owner | Updated by |
|---|---|---|
| `/Library/Application Support/faber/bin/faber` (the daemon) | `_faber` | the daemon's updater (ADR-0006, ADR-0031) |
| `/usr/local/bin/faber` (the CLI that `sudo` runs) | root | nobody |
| `faber.app` (the menu bar and popover) | the operator | nobody, once `_faber` must not write it |

So after the cut, two of the three copies stop following releases. Both issues
proposed verifying against `ACCEPTED_SIGNERS`, which ADR-0031 removed.

Under ADR-0031 the app also matters to the trust model. The popover's countdown
is the alert the GitHub account cannot silence. An app that silently stops
working removes that alert on every host at once.

Facts that shape the decision:

- `config.toml` lives in `_faber`'s home, and the daemon writes it through the
  settings save. Anything the app reads there, `_faber` can change.
- The app is a login item (`tauri-plugin-autostart`). No LaunchAgent watches it,
  so if a new bundle crashes at launch, nothing starts the app until the next
  login.
- `release.yml` checksums every `faber-*` asset, the app tarball included, into
  the attested `SHA256SUMS`. It also checks that the bundle's
  `CFBundleShortVersionString` equals the release version.

## Decision

### Each copy is updated by the account that owns it

The operator's app updates itself. Root's CLI copy is promoted only inside a
`sudo` command the operator runs. Updating adds no standing root process.
ADR-0025 keeps root to commands a person starts, and a root daemon that parses
network data would break that.

ADR-0026's pf anchor daemon is the one standing root process. It runs this same
root copy (`faber wall-daemon`, amended 2026-10-17), reads only local state, and
`upgrade-cli` restarts it on the new code.

Both use the ADR-0031 verifier and policy, compiled into their own binary. The
verifier moved out of `src/provenance.rs` into `crates/faber-release`, a crate that the daemon and the
app both build. Neither copy ever executes a file another account can write, or
trusts one: what `_faber` writes is only ever data that they check.

### The app follows the daemon, with guards of its own

- **The trigger.** The app reads the daemon's version from `/api/state`. Once the
  daemon has installed a release and passed boot probation, the app installs the
  app asset of exactly that version. The daemon owns the decision: its maturity
  wait, its pin, its `enabled`. So the app never runs ahead of the daemon it
  talks to.
- **The daemon's report is only a hint.** The app downloads the asset itself and
  runs ADR-0031's check in full:
  - the attestation of `SHA256SUMS`, against the compiled-in policy
  - the asset's digest against `SHA256SUMS`
  - the bundle's version equal to the tag, with no pre-release suffix

  It installs only a version strictly above its own.
- **Its own maturity floor.** A compromised `_faber` could report a release it
  never installed, to pull the app forward onto a genuine release that has not
  matured. So the app also requires GitHub's `published_at` to be at least its own
  floor old. The floor is the app's own setting, stored in the operator's
  preferences, which `_faber` cannot write:
  - The default is 24 hours, and anything under one hour (`MIN_MATURITY_SECS`) is
    refused.
  - A canary host sets it shorter by hand.
  - When it disagrees with the daemon's `maturity_secs`, the popover says so. A
    daemon value lowered behind the operator's back shows up there.
- **When.** The swap runs automatically, when the popover is closed and the app
  has been idle for about 30 seconds. The popover then shows one line: "Updated to
  X". A prompt could be ignored indefinitely, and the app would run old code against
  a newer daemon for as long as it was.
- **Failure is visible and rate-limited** like the daemon's: a refused release is
  held for one poll interval, and the popover says why.

### The app swaps atomically and rolls itself back

1. The verified bundle is staged beside the installed one, as
   `.faber.app.new` in the same directory, so the rename is atomic.
2. A small detached watchdog is started from the **old** bundle's code.
3. The old bundle is renamed `faber.app.previous` and the new one `faber.app`,
   and the new app is launched (`open -n`).
4. If the new app is not alive after N seconds, the watchdog swaps
   `faber.app.previous` back and launches it, and the refusal is recorded so the
   same version is not retried until the next release.
5. Once the new app has run healthy for a minute, it deletes
   `faber.app.previous`.

The app never installs itself fresh. A host without the app stays without it
until `sudo faber setup` installs it (amended 2026-10-28, above).

Tauri's own updater plugin is not used: it requires a minisign key pair, and
ADR-0031 holds no key anywhere.

### The daemon stops touching the app

`update_app_bundle` is deleted, on every layout. After the cut every host runs
ADR-0025's layout, two updaters for one bundle would be a race nobody tests, and
the daemon removing a bundle the operator owns was never ADR-0025-clean. This
withdraws no capability: v0.4.0, the release that carries the app's own updater,
is installed by hand with the new app at the cut sitting.

### The CLI copy upgrades when the operator says so

`sudo faber upgrade-cli`, which `sudo faber setup` also runs as its first step:

1. Copy the daemon binary into a root-owned temporary file **first**, so `_faber`
   cannot change it between check and install.
2. Hash the copy.
3. Fetch that release's `SHA256SUMS` and `SHA256SUMS.sigstore.json` anonymously,
   and verify them with the **running root copy's own** policy.
4. Check that the copy's digest is in `SHA256SUMS`, and run the version binding:
   `--version` equals the tag, and its `+g<sha>` prefixes the tag's commit.
5. Require a version strictly above its own.
6. Print the version and its `published_at`, ask y/N, then install over
   `/usr/local/bin/faber` atomically.

It takes no maturity wait, because the operator asked for it. If the daemon copy
is missing or fails a check, it fails with the reason. The fallback is
`sudo faber setup` from a hand-downloaded, hand-verified release.

**The gap is made loud, not closed.** Every CLI invocation, rootless ones
included, prints one line when the daemon is newer: "CLI 0.4.0 < daemon 0.4.1:
run `sudo faber upgrade-cli`". The popover shows the same line. Until the
operator runs it, the CLI↔daemon API stays additive (ADR-0025, ADR-0028), as it
has to anyway.

## Consequences

- After v0.4.0, a published release reaches the daemon (after its maturity wait),
  then the app (after the daemon, and after the app's own floor). The CLI copy
  follows at the operator's next `upgrade-cli` or `setup`.
- The app grows by the verifier's size, about 2–3 MB.
- A host has two maturity values: the daemon's and the app's. The popover shows
  both whenever they differ.
- The cut runbook (kept with the legacy repository) gains, in its host
  sitting: set the app's floor, and check that the app and the CLI copy both
  report 0.4.0. Its step 13 checks that the app follows v0.4.1 on its own and the
  CLI warns until `upgrade-cli`.
- A trust-root change (ADR-0031 § Changing the policy) now reaches three
  compiled-in policies. Each copy carries its own, and each must be shipped in a
  release verified under the old policy.

## Implementation notes

- `upgrade-cli` (#81, `src/upgrade_cli.rs`) never runs the daemon copy, not
  even for `--version`: the version binding reads the `FABER-VERSION` marker
  from the copy's digest-verified bytes, which is the same `FABER_VERSION`
  that `--version` prints.
- A daemon copy whose marker is not newer is settled before anything is
  fetched: up to date, or refused as older. Either way nothing changes, and an
  up-to-date host needs no network.
- When `sudo faber setup` promotes the CLI copy in its first step, it
  re-executes that copy with the same arguments and `--skip-cli-upgrade`, so
  the rest of setup cannot install the older binary over it. A host with no
  daemon copy yet is skipped silently, and any other failure is a warning, not
  a stop.

## Alternatives considered

**One root helper for both.** A root LaunchDaemon running the root copy as
`faber promote`, installing the CLI and the app. It is one mechanism, but it adds
a root process that runs all the time and parses network data.

**The app polls GitHub on its own**, with its own first-seen clock. That doubles
the polling and adds a second clock that can disagree with the daemon's. It also
lets the app run ahead of the daemon.

**The app reads the daemon's `maturity_secs`.** `_faber` writes that file, so the
floor would protect nothing. **A compiled-in one-hour floor** is out of reach of
`_faber`, but it is the shortest wait, not the one the operator chose.

**Swap without a rollback.** The app would be acceptable as a convenience. As the
alert ADR-0031 relies on, a release that breaks its launch would silently remove
that alert on every host.

**A prompted swap, or a swap at next login.** A prompt can be ignored for ever,
and a Mac that rarely logs out keeps the old app for days.

**Promote the CLI inside any `sudo faber` command.** It would swap code in
under a command the operator typed. `sudo faber uninstall` would first upgrade
itself, and a failed promotion would block the command the operator needed.

**The CLI downloads the binary fresh.** It works when the daemon copy is broken,
but it costs a 10 MB download. It also gives up the property that the CLI is
byte-identical to the running daemon. The verification is the same either way.
