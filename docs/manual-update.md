---
title: Updating a host by hand, from source
type: runbook
date: 2026-10-11
---

# Updating a host by hand, from source

Until the ADR-0031 cut, a host is updated by building `main` and installing it
yourself. No release can install before then: the release policy's repository ID
is a placeholder that refuses every release, and a private user-owned repository
can't hold GitHub attestations. The dashboard's update button only installs from
the release channel, so it has nothing to offer.

This covers one macOS host, in either layout:
- per-user, with faber running as a LaunchAgent;
- set up, with faber running as `_faber` with its VMs after `sudo faber setup`.

Step 5 has a part for each. It takes about 10 minutes, plus however long the
running jobs need.

The `curl` commands below speak to a pre-cut daemon, whose API is plain TCP on
`127.0.0.1:8787`. From v0.4.0 that port answers only a dashboard session
([ADR-0028](adrs/0028-the-control-api-knows-its-caller.md)): send the same
requests to the control socket instead, as the operator or with `sudo`, for
example `curl -s --unix-socket "/Library/Application Support/faber/control.sock"
http://faber/api/state`. `faber drain` and `faber status` pick the right one
themselves.

## 1. Pick a quiet window

Drain when few jobs are running: a drain lets admitted jobs finish but claims
nothing new, so queued jobs wait for the whole drain.

```sh
curl -s http://127.0.0.1:8787/api/state | jq '[.inflight[] | select(.admitted)] | length'
```

## 2. Build `main` in a worktree of its own

Never build from a checkout another session is working in, and never from the
primary checkout while CI runs out of it.

```sh
git -C <repo> fetch origin
git -C <repo> worktree add /tmp/faber-build origin/main
cd /tmp/faber-build
( cd ui && npm ci && npm run build )   # the web UI the daemon embeds
FABER_REQUIRE_UI=1 cargo build --release --locked
./target/release/faber --version      # faber <version>+g<sha of origin/main>
```

## 3. Check the new binary accepts this host's config

```sh
./target/release/faber check-config
```

Fix anything it reports before going on. A daemon that rejects its config at
boot is a dead daemon.

## 4. Drain

```sh
curl -s -X POST http://127.0.0.1:8787/api/drain    # "draining N"
```

Wait until no claim is admitted. Don't pause and wait for `inflight` to reach 0:
since ADR-0023 a paused host keeps its parked claims, so that never empties while
jobs are queued.

```sh
while :; do
  n=$(curl -sf --max-time 5 http://127.0.0.1:8787/api/state | jq '[.inflight[] | select(.admitted)] | length')
  [ -z "$n" ] && { echo "faber stopped answering: investigate before installing"; break; }
  [ "$n" = 0 ] && { echo "drained"; break; }
  sleep 30
done
```

Install only after it prints `drained`. If faber stops answering mid-drain,
read its log (below) first.

`faber drain` does the same from the CLI. It waits up to 3 h 10 min by default,
just above the 3 h runner lifetime cap, and it treats a daemon that stops
answering as drained, so check the daemon is still up before you install.

## 5. Install

**Do not run `deploy/install.sh` on a host that has not been through the cut.**
Since ADR-0025 (#79) it hands off to `sudo faber setup`, which moves faber onto
its hidden accounts. That is the cut's step: under `_faber`, faber cannot use
Docker Desktop, and until ADR-0026's VMs exist the host would run no jobs.
Since #223, `deploy/install.sh` and `sudo faber setup` refuse on macOS unless
the binary leaves a runnable VM runtime (the launcher, and the Linux VM's image
it can build), and `install.sh` then prints the recipe below instead.
`faber setup --check-vm-runtime` (no sudo) asks the same question.

A host still on the per-user layout, with no `/etc/faber/setup.json`, swaps the
binary in place and restarts its LaunchAgent:

```sh
D="$HOME/Library/Application Support/faber"
cp -p "$D/bin/faber" "$D/bin/faber.bak-$(date +%Y%m%d)"
install -m 0755 ./target/release/faber "$D/bin/faber.new"
mv -f "$D/bin/faber.new" "$D/bin/faber"
launchctl kickstart -k "gui/$(id -u)/io.github.rejifald.faber"
```

A host that has been through `sudo faber setup` (it has `/etc/faber/setup.json`
and runs as `_faber` with its VMs) is updated by running setup again from the
new build. Never copy binaries into `/Library/Application Support/faber/bin` or
`/var/db/faber/bin` by hand. Setup owns their ownership, modes and the order of
the swap. A re-run plans only what changed. If the daemon binary or the service
definition changes, setup drains the running daemon itself, swaps it, starts the
new one, and waits for its API. A failure, or Ctrl-C, puts the previous daemon
binary, service definition and setup record back, so step 4's drain is optional
here. Two things are not rolled back. The launcher and the agent are installed
after the daemon swap, just before the new daemon starts, and stay at the new
version if that start fails. Setup also refreshes the CLI copy at `/usr/local/bin/faber`, and that has
no rollback either. After a failed run, re-run setup from the build you meant
to install, or from the previous one.

Setup takes the launcher and the guest agent from beside the binary it runs
from, so build all three into one directory the operator owns:

```sh
OUT="$HOME/faber-update" && rm -rf "$OUT" && mkdir -m 700 "$OUT"
( cd ui && npm ci && npm run build )
touch build.rs && FABER_REQUIRE_UI=1 cargo build --release --locked
FABER_VM_OUT="$OUT" launcher/build.sh && FABER_VM_OUT="$OUT" launcher/build.sh --agent
install -m 0755 ./target/release/faber "$OUT/faber"
( cd desktop && npm ci && npm run tauri -- build -- --locked )   # the menu-bar app
tar -czf "$OUT/faber-app-aarch64-apple-darwin.tar.gz" \
  -C desktop/src-tauri/target/release/bundle/macos faber.app
"$OUT/faber" --version && "$OUT/faber-vm" --version && "$OUT/faber-agent" --version
plutil -extract FaberBuild raw desktop/src-tauri/target/release/bundle/macos/faber.app/Contents/Info.plist
```

The three versions and the app's `FaberBuild` stamp must match (`<version>+g<sha>`):
setup refuses an agent, a launcher or an app that disagrees with the binary. The `ui/` build comes first because
`build.rs` embeds `ui/dist/dashboard`. Without it, `/settings` serves the
"dashboard was not built" page, and `FABER_REQUIRE_UI=1` makes that build fail
rather than install, as in `release.yml`. The `touch build.rs` is there because
`build.rs` re-derives faber's stamp only when `.git/HEAD` changes, so a new
commit on the same branch could otherwise leave `faber` with an old one. Then,
from your own admin terminal
(Terminal.app, never an agent; AGENTS.md):

```sh
sudo "$HOME/faber-update/faber" setup
```

Read the plan before you answer `y`. A normal update changes:
- the daemon binary;
- the service step ("the running daemon is drained … its binary swapped");
- the launcher, the agent and the CLI copy;
- the menu-bar app.

It can also show **the Linux VM image** as out of date, and plan a rebuild that
takes up to 30 minutes. That happens when this build changed the image's guest
files or its Debian pin. It is expected, and needs no flag.

The first run of a build with the operator group (ADR-0027, amended
2026-11-01) also changes accounts and directories, on a host set up before it:
it creates group `_faber_operator`, adds you and `_faber` to it, takes you out
of group `_faber`, gives the state and log trees and the files you read the new
group, rewrites the record and restarts the daemon. Afterwards, log out of
every session, then replace the GitHub token (`faber auth link`, `sudo faber
auth set`) and revoke the old one: until now every process you ran could read
it.

Anything else means the host drifted: accounts, directories, the wall, the
golden images, the runner-image tarball. Stop and find out why first. Setup
re-imports a golden image or a tarball only when you pass `--golden-image` or
`--runner-image-tar` again. Delete `$OUT` afterwards.

If a config change goes with the build, write it before the restart. Check it
first with the new binary: `./target/release/faber check-config --config <file>`.
On a set-up host the config is `_faber`'s: make the change with
`sudo faber config set|unset|apply` (ADR-0028, #78), not by editing the file.
The menu-bar app moves with the daemon (ADR-0034, amended 2026-10-31): setup
replaces `/Applications/faber.app` from the `faber-app-aarch64-apple-darwin.tar.gz`
in `$OUT` whenever the installed app's `FaberBuild` stamp is not this binary's
version. The plan shows it under "menu-bar app" as replaced, and the app quits and
reopens at the end of the run. If the plan warns there that it has no app to
install, the archive is missing from `$OUT`: build it as above and re-run, rather
than leave the menu bar on an older build.

The new process starts fresh: the drain does not carry over, so claiming resumes
on its own.

## 6. Verify

```sh
curl -s http://127.0.0.1:8787/api/state | jq '{healthy, paused, draining, version: .update.current}'
```

Expect `healthy: true`, `paused: false`, `draining: false`, and the version from
step 2. On a set-up host, `faber status` must also show `wall green`: a healthy
daemon still admits nothing while the wall's checker or the VM supervisor is
down. If either is stuck in `launchctl print` (`state = spawn scheduled`,
`last exit code = 78: EX_CONFIG`), re-run setup from the same build: it reloads
a job launchd cannot spawn (#479). Then watch that the host claims its next
job: a new `inflight` entry, or a runner of this host's in the watched repo's
job list.

## 7. Clean up

```sh
git -C <repo> worktree remove /tmp/faber-build
```

## If it goes wrong

- **The new daemon won't start:** read `~/Library/Logs/faber/stderr.log`. The
  usual cause is a config the new binary rejects, which step 3 should have caught.
- **Going back:** there is no automatic rollback for a hand install. Build the
  previous commit the same way and install it.
- **A forced install** (skipping the drain) kills running jobs. Don't, unless a
  wedged daemon leaves no other way.
