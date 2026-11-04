# Running faber as a system service (ADR-0025)

faber runs as its own hidden accounts, started at boot with nobody logged
in — never as you, and never as a shared `ci` user. `sudo faber setup`
creates them, installs the service, and migrates any per-user install it
finds. It shows its plan first and only changes anything after you confirm.

- **`_faber`** runs the daemon and holds its credentials (ADR-0027). It runs
  nothing else — job code never executes as this account.
- **`_faber_vm`** runs the host side of faber's VMs (ADR-0026). It never holds
  a credential.

Both are hidden, passwordless role accounts (`sysadminctl -roleAccount` on
macOS, `useradd --system` on Linux), with no shell and no login session. There
is no auto-login to configure, because the service does not need anyone
logged in: it is a **LaunchDaemon** in launchd's `system` domain on macOS, a
**systemd system unit** on Linux.

```
deploy/
  io.github.rejifald.faber.plist  the LaunchDaemon template — setup renders it
  faber.service                   the systemd system unit template (Linux)
  config.starter.toml             the starter config setup seeds when there
                                   is nothing to migrate
  install.sh                      build/locate the binary, then hand off to
                                   `sudo faber setup`
  install-app.sh                  build + install the menu-bar app (no sudo)
  install-all.sh                  install.sh + install-app.sh
  uninstall.sh                    wraps `sudo faber uninstall`
  keep-awake.sh                   pmset -a sleep 0 — setup does this too
  release.sh                      cut a release (the designated builder Mac only)
```

These assume you already have a clone. To go from nothing to a running daemon
in one command, use the bootstrap installer at the repo root — it clones to
`~/.faber/src`, offers to install the missing toolchains, and then calls
exactly these scripts:

```sh
curl -fsSL https://raw.githubusercontent.com/rejifald/faber/main/install.sh | bash
```

## Install

```sh
./deploy/install.sh            # build target/release/faber, then hand off
                                # to `sudo faber setup`
./deploy/install.sh --build    # force a release rebuild first
FABER_BIN=/usr/local/bin/faber ./deploy/install.sh   # use a prebuilt binary
```

Building needs no privileges. On macOS, `install.sh` first asks the binary
whether setup from it leaves the host a runnable VM runtime (`faber setup
--check-vm-runtime`, no sudo): the VM launcher, installed or beside the binary,
and the Linux VM's image, current or buildable. Without one the host would run
no jobs under `_faber`, so it stops there and prints the binary-swap recipe from
[docs/manual-update.md](../docs/manual-update.md) instead (#223). `sudo faber
setup` refuses the same way; `--before-vm-runtime` overrides it, for testing.

The hand-off at the end runs `sudo faber setup`, which:

1. **shows its plan** — every step, from read-only probes — and changes
   nothing until you confirm (or pass `--yes`; running without `sudo` at all
   prints the same plan and exits)
2. **creates `_faber` and `_faber_vm`** (the lowest free id in 450-499, both
   as uid and gid; `sysadminctl -addUser … -roleAccount` / `useradd
   --system`), each with no password at all, and the group
   `_faber_operator`, which *you* and `_faber` join — never `_faber_vm`. If an
   earlier setup put you in group `_faber`, it takes you out: that group
   reads the GitHub token, and setup refuses to go on while anyone but
   `_faber` is in it
3. **creates the SYSTEM directories** (the layout table below)
4. **installs the binaries**: the root-owned CLI copy at
   `/usr/local/bin/faber`, and `_faber`'s own daemon copy — both from the
   binary you ran `setup` from. Every directory above the CLI copy must be
   root-owned and writable only by root; an operator-owned `/usr/local/bin`,
   as some older Homebrew setups have, is refused, since anything running as
   you could then replace what `sudo faber` runs. Setup never runs the daemon copy it finds: it
   reads the version marker in its bytes, keeps a copy that claims to be newer,
   and replaces anything else. `--replace-daemon` replaces it regardless (the
   repair path). The copy it replaces is kept aside until the new daemon
   answers, and put back if it does not
5. **seeds a starter config** (`deploy/config.starter.toml`) when there is
   nothing to migrate, and asks whether to turn on fleet autoupdate
   (ADR-0006) — the *only* place that question is asked; `--yes` accepts the
   rest of the plan but never this
6. **finds any per-user faber** — a LaunchAgent in `gui/<uid>`, or a `systemd
   --user` unit — drains it over its API, stops it, and moves its
   plist/unit aside to `<name>.pre-system-<timestamp>` so it does not come
   back at the next login
7. **copies (never moves)** the config, overrides, image cache and audit
   trail of *your* per-user install — the account that ran `sudo` — recording
   each file's SHA-256; the original stays where it was, as the backup.
   Another account's install (an old `ci` user's, say) is stopped but never
   copied from unless you name it with `--migrate-from <user>`; if it has a
   config and you have none, the plan refuses until you choose
   `--migrate-from <user>` or `--fresh` (the starter config, nothing
   migrated). The config to copy is checked with the new binary's
   `check-config`, run as you, before anything is drained or stopped, and
   again as `_faber` once copied. An existing destination is never
   overwritten; `--remigrate` moves it aside first instead, and on a set-up
   host drains and restarts the daemon around the copy
8. **writes `/etc/faber/setup.json`**, the setup record
9. **installs and starts the service**, waiting for `GET /api/state` to answer
10. **moves your per-user directories aside** once the daemon answers —
    `~/Library/Application Support/faber`, `~/Library/Logs/faber` and
    `~/Library/Caches/faber` on macOS, the XDG `faber` directories on Linux —
    renaming each to `<name>.pre-system-<timestamp>`, so nothing faber reads
    stays in your home and the originals remain as the backup (the closing
    message lists them). `--keep-legacy` leaves them where they are
11. **keeps the Mac awake**, macOS only (`pmset -a sleep 0`, `-a powernap 0`),
    unless `--skip-keep-awake`

Flags: `--dry-run`, `--yes`, `--skip-keep-awake`, `--drain-timeout <secs,
default 3600>`, `--replace-daemon` (`--allow-downgrade` is an old name for
it), `--remigrate`, `--migrate-from <user>`, `--fresh`, `--keep-legacy`.

A re-run on an already set-up host plans nothing new — it is idempotent — and
restarts nothing unless the service definition, the binary or (with
`--remigrate`) the state changes, in which case it drains the running daemon
over its API (up to `--drain-timeout`) before swapping in the change and
restarting the service.

Setup's keep-awake step (or `keep-awake.sh`) stops the Mac sleeping at all. If
you would rather it still sleep when idle, skip it (`--skip-keep-awake`) and set
`prevent_sleep = "while_busy"` under `[host]` in the config (or *Prevent sleep*
in the dashboard's settings): faber then holds off idle sleep only while a job's
runner is up, and lets go three minutes after the last one ends. `"always"`
holds it for as long as the daemon runs, jobs or not; `"off"` (the default)
never does. The old `prevent_sleep_while_busy = true` still works as
`"while_busy"`, with a boot warning. Either way a sleeping Mac loses the jobs it
is running (issue #90). Neither keeps a closed laptop lid from sleeping.

To get the **menu-bar app** alongside the daemon in one pass, use
`./deploy/install-all.sh` instead of `install.sh` — it runs the same daemon
install and then builds + installs the tray app (`desktop/`). The app needs a
GUI login session, so a headless host runs `install.sh` alone.

## File layout

macOS, where `D` = `/Library/Application Support/faber`:

| Path | Owner | Contents |
|---|---|---|
| `/usr/local/bin/faber` | root:wheel | the CLI copy — what `sudo faber …` runs |
| `/etc/faber/setup.json` | root:wheel | the setup record |
| `/Library/LaunchDaemons/io.github.rejifald.faber.plist` | root:wheel | the service |
| `/etc/faber/credentials/` | root:wheel 0755 | the GitHub token, `root:_faber` 0640: only `_faber` reads it, and nobody else is in group `_faber` (ADR-0027) |
| `D/` | `_faber:_faber_operator` 0750 | `config.toml`, `overrides.json`, `faber.lock` (0600: a lock anyone could open, anyone could hold), `control.sock` (`_faber_operator` 0660), `image-cache.json`, `runner-variant/`, `update/` — also `_faber`'s `HOME` |
| `D/bin/` | `_faber:_faber_operator` | `faber` and `faber.prev` — the daemon's own copy, swapped by same-directory rename on self-update |
| `D/registry-auth/` | `_faber` 0700 | per-job registry credential directories |
| `/Library/Logs/faber/` | root:`_faber_operator` 0750 | `stdout.log`, `stderr.log`, `audit.jsonl`, each `_faber:_faber_operator` 0640 and created by setup. launchd may open the first two as root, so `_faber` writes all three but can create, rename or replace nothing here |
| `/Library/Caches/faber/` | `_faber:_faber_operator` | faber's cache |
| `/var/db/faber-vm/` | `_faber_vm` | its `HOME`, reserved for ADR-0026 |
| `/var/db/faber/bin/` | `_faber` 0755 | `faber-vm`, the VM launcher (ADR-0026), with its `.staged` and `.prev`: outside `D`, which `_faber_vm` cannot enter, and under directories only root can change |

Linux: the CLI copy and `/etc/faber/` sit at the same paths; the state
directory is `/var/lib/faber`, logs are in `/var/log/faber` (plus
`journalctl -u faber.service`), the cache is `/var/cache/faber`, and
`_faber_vm`'s home is `/var/lib/faber-vm`. The service is
`/etc/systemd/system/faber.service`; it runs the daemon with
`Group=_faber_operator`, so systemd gives those three directories that group,
and `SupplementaryGroups=_faber`, through which it reads the token.

**Reading faber's files as yourself:** you are added to group
`_faber_operator`, which can read — never write — `D`, the logs and the audit
trail (umask `027`), and reach the control socket. It cannot read the GitHub
token: that file's group is `_faber`, whose only member is `_faber` itself.
That membership takes effect at your *next login*, so open a new terminal (or
`newgrp _faber_operator`) before `faber status` or a plain `cat` of the logs
works without `sudo`.

**If an earlier setup put you in group `_faber`**, the next `sudo faber setup`
moves you to `_faber_operator` and restarts the daemon. A process keeps the
groups it started with, so every process you started before then can still
read the token. Log out of every session (or restart the machine), then
replace the token: `faber auth link`, create the new one on GitHub, `sudo faber
auth set`, and revoke the old one on GitHub.

### Why no native profile here

A native profile runs job code directly as the account the daemon runs
under. On this layout that account is `_faber`, which holds faber's GitHub
credential (ADR-0027). Running arbitrary job code as the account that holds the
credential is exactly what ADR-0025 exists to rule out, so ADR-0026 removed the
backend, and `backend = "native"` is a config error
([ADR-0052](../docs/adrs/0052-a-native-profile-is-a-config-error.md)). Route
macOS work through a `macos-vm` profile and Linux work through a Docker
profile. `deploy/config.starter.toml` seeds a Docker profile, and [`src/config.rs`](../src/config.rs) has the annotated
schema.

### Standby

A freshly set-up daemon has no GitHub credential yet: you hand it one with
`faber auth link` and `sudo faber auth set` (ADR-0027). Until then it comes up
in **standby**: the dashboard answers with `healthy: false` and says why, it
retries every 60 seconds, and it never crash-loops or exits. The closing message
of `sudo faber setup` says the same thing.

## Uninstall

```sh
./deploy/uninstall.sh              # plan, confirm, then remove everything
./deploy/uninstall.sh --keep-logs  # copy audit.jsonl + config.toml to your home first
./deploy/uninstall.sh --yes        # skip the confirmation
./deploy/uninstall.sh --dry-run
./deploy/uninstall.sh --no-drain   # stop without draining: orphans running jobs
```

This wraps `sudo faber uninstall`, which drains the daemon, stops the
service, removes faber's files, then deletes `_faber` and `_faber_vm` and the
groups `_faber_operator`, `_faber_vm` and `_faber` last.

Flags: `--dry-run`, `--yes`, `--keep-logs`, `--drain-timeout <secs, default
3600>`, `--no-drain`. `--no-drain` is for a daemon that cannot drain (it does
not answer, or will not finish): every job it is running is cut off, its
runner orphaned on GitHub and its container left behind. It always asks for
confirmation, even with `--yes`.

**Run it from Terminal.app.** macOS refuses to delete an account — the last
step — from a terminal hosted inside another application
(`eDSPermissionError`: it checks that application for "administer this Mac"
and Full Disk Access, and refusing that trade is correct). Everything before
the accounts is idempotent, so re-running from Terminal.app after such a
refusal picks up exactly where it stopped.

## Operating it

```sh
# is it running?
sudo launchctl print system/io.github.rejifald.faber | grep -E 'state|pid'

# tail logs (through group _faber_operator — no sudo needed once your
# membership is active; see "Reading faber's files as yourself" above) and the
# audit trail
tail -f /Library/Logs/faber/stderr.log
tail -f /Library/Logs/faber/audit.jsonl

# restart now (also the way to test KeepAlive)
sudo launchctl kickstart -k system/io.github.rejifald.faber

# stand it down before an uninstall or a manual service stop
faber drain
```

An uninstall removes the logs and the append-only audit trail under
`/Library/Logs/faber/` with everything else. Pass `--keep-logs` to copy
`audit.jsonl` and `config.toml` to your home first.

A "Runner retiring" warning on the dashboard or tray, or a log line naming a
profile's actions/runner version and a cutoff date, means GitHub is about to stop
running jobs on the runner that profile's image bakes. Rebuild the image, point
the profile at it, then drain and restart: see
[When GitHub retires the runner version](../docs/operating.md#when-github-retires-the-runner-version).

## Updating the menu-bar app

Fleet autoupdate (ADR-0006) installs the daemon and, beside it on macOS, the VM
launcher and the guest agent. It never touches the menu-bar app, which lives in
your own session, not `_faber`'s. The app follows the daemon on its own instead
(ADR-0034): once the daemon has installed a release, the app fetches that
release's app asset, checks it the same way, waits its own maturity time, and
swaps itself in while its popover is closed, putting the old bundle back if the
new one does not come up. `sudo faber setup` moves the app along with the daemon
too, whenever the app archive (`faber-app-aarch64-apple-darwin.tar.gz`, a
release's or a source build's) is beside the launcher; see
[docs/manual-update.md](../docs/manual-update.md). `./deploy/install-app.sh`
builds and installs one from a local checkout.

## Graceful shutdown (drain)

faber traps `SIGTERM` (a service stop / `bootout` / `kickstart -k`) and
`SIGINT` (Ctrl-C). On the first signal it stops claiming new jobs and waits
for in-flight runners to finish, so a stop or restart never orphans a running
job. A second signal during the drain forces an immediate exit. The dashboard
shows `DRAINING` while this happens.

> Drain time is bounded by the plist's `ExitTimeOut` (600s) or the unit's
> `TimeoutStopSec`. If a job runs longer than that, the service manager
> force-kills faber mid-drain and the runner orphans on GitHub until it ages
> out. `sudo faber setup` and `sudo faber uninstall` avoid this by draining
> over the API first (`--drain-timeout`, default 3600s) and only stopping the
> service once nothing is admitted.
