---
title: macOS VMs start inside a login session
type: adr
status: accepted
date: 2026-10-23
owner: rejifald
---

# ADR-0051: macOS VMs start inside a login session

## Status

accepted (2026-10-24). The owner chose Option 2 with the design under *Decision* below.
The reboot path (d) and the operator's own logout passed their spike on 2026-10-25
(below), so the design stands as written. Cite it by its slug until it
merges. It amends ADR-0026 (*faber runs its VMs with a launcher it owns*, *macOS jobs run
in per-job macOS VMs*) and ADR-0025 (*faber runs as hidden role accounts*: two accounts,
"no visible account and no auto-login anywhere in the design"). Both carry an
*Amended* note pointing here.

## Context

ADR-0026 runs every VM under one supervisor: a system LaunchDaemon, `_faber_vm`, started at
boot with nobody logged in. The Linux VM has run that way since late October 2026. No macOS
job VM had ever started that way until a day later, when the first one was tried on the
builder (image `xcode-27.1`, faber `main` plus #350).

**Every macOS VM start fails there,** after the launcher's own checks pass:

```
faber-vm: start failed: Unable to access security information. The virtual machine
encountered a security error. [VZErrorDomain -9]; underlying: … Failed to get current
host key. [NSPOSIXErrorDomain 22]
```

**What was ruled out.**
- Two earlier faults hid this one. The tools disk's directory owner was fixed in #349, and a
  clone's read-only aux storage is fixed in #350.
- The launchd `SessionCreate` key, which gives the daemon a security session of its own,
  changes nothing. It was tried and reverted.
- The guest image is not the cause: the same image boots in a golden build.

**ADR-0026 says the opposite, and that measurement does not carry over.** ADR-0026 records
that "a role account with no login session boots a macOS guest" from a LaunchDaemon, about
a minute to the guest's desktop. That was the #76 runtime test of 2026-10-10: a throwaway
spike tool on a bundle it had installed itself, not faber's supervisor on an imported golden
image under the host's current macOS. On the builder the supervisor fails as above
every time. The difference was not isolated; this ADR does not depend on it, because the
login session works either way. ADR-0026's line is amended to say so.

**What works.** The golden build (`sudo faber golden build`, ADR-0049) starts the same kind
of VM as the same account, `_faber_vm`, and it boots, leases its address and runs for hours.
The one difference is where the process starts. The golden build is launched with `sudo`
from the operator's Terminal, so the VM process lives in the operator's **login session**
(its audit session and bootstrap namespace). The supervisor's VMs live in the system
domain, outside any login session.

**What Apple says.** A DTS engineer on the Apple Developer Forums
([thread 841688](https://developer.apple.com/forums/thread/841688)) advises against running
macOS VMs from a launchd daemon. Virtualization links AppKit, and the guest's Secure Enclave
key request is refused outside a user context. The suggested shape is a launchd agent in a
user's session. Another report
([thread 822670](https://developer.apple.com/forums/thread/822670)) has the same error chain
for macOS 26+ guests, while 15.x guests start.

**Constraints on any fix.**
- **FileVault may be on,** and macOS turns automatic login off when FileVault is on.
  faber does not require FileVault either way, because each installation picks a
  reboot mode (see *Decision*). On a host with FileVault on, a person unlocks the disk
  after every reboot, which logs that person in. So "a session exists" holds after a
  reboot exactly as long as someone unlocks the disk, which faber already needs.
- **Apple allows at most two macOS VMs per Mac.** Nothing here changes that.
- **ADR-0025's account split stays.** Job VMs must not run as the operator. `_faber_vm`
  must still never hold the token, and `_faber` must never run a VM.

## Options

### 1. Borrow the console session, keep `_faber_vm` (ruled out: see the spike below)

A root helper starts a second supervisor, for macOS VMs only, **as `_faber_vm` but inside
the console user's login session**: `launchctl asuser <console uid>`, then dropping to
`_faber_vm`, which is what the golden build does by hand today. The Linux VM stays on the
system supervisor. faber talks to both sockets.

- **For:** the account model is unchanged; the VMs run as `_faber_vm`, exactly as the golden
  build's do. No new account and no new manual step: the session already exists
  whenever someone has unlocked FileVault.
- **Against:**
  - The macOS VMs live in someone's session. A logout kills them, and a Mac with nobody
    logged in runs no macOS jobs. Admission needs a new gate, "no console session", and
    a logout has to be treated like a revival: drain, then refuse.
  - It needs a root step at runtime to start the session supervisor. The wall daemon
    (`io.github.rejifald.faber.wall`) already runs as root and could own it, but that
    widens what it does.
- **Unverified:**
  - (a) `launchctl asuser` from a root daemon, dropping to `_faber_vm`, starts a macOS
    guest;
  - (b) it survives screen lock and fast user switching;
  - (c) what a logout does to running VMs.

### 2. A dedicated session account

A new account, such as `_faber_mac`, that can log in to a GUI session. It runs a
per-user LaunchAgent (`LimitLoadToSessionType = Aqua`), the macOS VM supervisor, as itself.

- **For:** this is the shape Apple's engineers describe. It does not depend on the
  operator's session.
- **Against:**
  - With FileVault on there is no automatic login. After every reboot a person must log in
    to this account, or unlock FileVault as it, then switch users.
  - It is a third faber account, with a password stored for login. VMs would run as it,
    not `_faber_vm`, which reopens ADR-0025's split, unless its agent drops to
    `_faber_vm`, and dropping needs root again. *Decision* amends ADR-0025 instead: the
    third account runs only macOS VMs and never holds the token.

### 3. The operator's own LaunchAgent

The VM supervisor runs as the operator, in their session.

- **For:** the simplest change.
- **Against:** a VM escape lands in the operator's account: their keychain, files and
  `sudo`. This is what ADR-0025 exists to prevent. **Not recommended.**

### 4. Keep macOS jobs off faber's VMs

Send `[self-hosted, macOS, ARM64]` jobs to GitHub-hosted macOS runners, or keep one host on
the native backend that ADR-0026 removed.

- **For:** no new mechanism.
- **Against:**
  - Hosted runners cost money and change where signing secrets are used.
  - The native backend runs job code as the operator, which ADR-0025 and ADR-0026 removed.
  - Either way, #76's goal is dropped.

### 5. An older guest

One report has macOS 15.x guests starting where 26+ guests fail.

- **Against:** it is unverified, and Xcode 27 needs a newer macOS than 15. **Not viable.**

### Spike result, 2026-10-23: `launchctl asuser` is not enough

Both runs started from a one-shot system LaunchDaemon, cloned `xcode-27.1` as `_faber_vm`,
and booted it on slot 15:
- **Control,** plain from the daemon: `Failed to get current host key`, as expected.
- **Option 1,** via `launchctl asuser <console uid>`, then `sudo -u _faber_vm`: **the same
  failure.**

`asuser` moves a process into the user's bootstrap namespace but not into the login
session's **audit session**. The golden build's VM, which boots, descends from the
operator's Terminal and so inherits that audit session. What macOS checks appears to be the
audit session. A VM must start from a process inside the login session.

**Option 1b:** a per-user LaunchAgent in the console session
(`LimitLoadToSessionType = Aqua`) whose only job is
`sudo -n -u _faber_vm /var/db/faber/bin/faber-vm …`. That is allowed by one sudoers rule:
the console user may run that one binary as `_faber_vm` and nothing else. The VMs still run
as `_faber_vm`; the session is inherited as in the golden build. The cost is a passwordless
sudoers rule naming the operator.

### Spike result, 2026-10-23: option 1b boots, and is rejected

A LaunchAgent loaded into the console user's GUI domain (`gui/<console uid>`,
`LimitLoadToSessionType = Aqua`) ran `sudo -n -u _faber_vm <wrapper> start` through a
temporary one-line sudoers rule. The VM started as `_faber_vm` (a uid in ADR-0025's 450–499
block, launchd manager `Aqua`) and reached `state=running`. **A macOS VM boots once its
process is inside a login session;** the account does not matter.

**The owner rejected 1b on 2026-10-23: nothing in faber gets sudo,** not even one narrow
rule. The spike's rule and wrapper were removed. faber's accounts have no sudo today:
`_faber` and `_faber_vm` are hidden, their shell is `/usr/bin/false`, they are not in
`admin`, and no sudoers entry names them.

So the process that starts macOS VMs must already be `_faber_vm` (or the account the VMs
run as) **and** inside a login session. With no sudo and no root step, the only way to get
both is for that account to log in itself.

### Option 2, revised: the VM account has its own login session (no sudo, no root)

The account that runs macOS VMs logs in to a GUI session of its own. That is `_faber_vm`,
or a dedicated account if `_faber_vm` must stay unable to log in. A per-user LaunchAgent in
that session (`LimitLoadToSessionType = Aqua`) runs the macOS VM supervisor as that
account. The session can sit in the background under fast user switching while the
operator uses the console. The Linux VM stays on the system supervisor.

- **For:**
  - No sudo, no runtime root step, and no privilege change of any kind: the VMs run as
    the account whose session it is.
  - It is the shape Apple's engineers describe.
- **Against:**
  - The account needs a password and a shell other than `/usr/bin/false`, which PAM
    refuses at login (spike below).
  - Its uid must be 501 or more. Background Task Management ignores accounts below 500,
    so their LaunchAgents never run (spike below). ADR-0025 puts faber's accounts at
    450–499.
  - With FileVault on, after every reboot someone logs this account in: by unlocking the
    disk as it (it needs a SecureToken), or by switching users after unlocking as the
    operator.
  - A logout or a reboot without that login means no macOS jobs, so admission needs a "no
    VM session" gate.
- **What the spikes check:**
  - (a) a hidden, non-admin account can log in to a background GUI session and run a
    LaunchAgent there (**yes, with a uid of 501 or more**: spikes below);
  - (b) a macOS VM it starts boots (**yes**, spike below);
  - (c) the session survives the operator's lock, fast user switching and logout (**yes**,
    spikes below);
  - (d) the FileVault and reboot path (**yes**, spike below).

### Spike result, 2026-10-23: the account logs in; its LaunchAgent does not start

A throwaway account shaped like `_faber_vm` logged in through fast user switching, with the
operator's session still open, and reached its desktop. It was hidden, not in `admin`, with
a uid below 500 like faber's own accounts, its home under `/var/db` and no SecureToken, on a
Mac with FileVault on.

- **Shell `/usr/bin/false` cannot log in.** The password verified (`opendirectoryd`:
  `Authentication succeeded`), then `pam_opendirectory`'s account check refused the
  account: `od_record_check_shell(): failed: 7`, which is `PAM_PERM_DENIED`. The login
  window calls that "deactivated". The check matches exactly `/usr/bin/false`, and the
  same module refuses the homes `/dev/null` and `99`. It also runs for `sshd`, `login` and
  the screen saver's unlock (`su` skips it). `/sbin/nologin` passes and still gives no
  interactive shell.
- **The recipe that works,** run as root by the operator:

  ```sh
  dscl . -create /Users/<name>
  dscl . -create /Users/<name> UniqueID <uid>            # 501 or more: next spike
  dscl . -create /Users/<name> PrimaryGroupID 20
  dscl . -create /Users/<name> UserShell /sbin/nologin    # not /usr/bin/false
  dscl . -create /Users/<name> RealName '<real name>'
  dscl . -create /Users/<name> NFSHomeDirectory <home>    # not /dev/null or 99
  dscl . -create /Users/<name> IsHidden 1                 # Auto-login only: Decision
  dscl . -passwd /Users/<name>                            # prompts for the password
  ```

  `-passwd` alone stores the shadow hash and sets `AuthenticationAuthority` to
  `;ShadowHash;HASHLIST:<…>` plus a `Kerberosv5` LKDC entry. Logging in once the disk is
  unlocked needs no SecureToken. A hidden account is reached through "Other…", which the
  login window shows only with `SHOWOTHERUSERS_MANAGED = true` in
  `/Library/Preferences/com.apple.loginwindow`.
- **Traps on the way.** A non-root `dscl -read` of another user's
  `AuthenticationAuthority` says "No such key" even when it is set: read it as root, or
  test the password with `dscl . -authonly`. The log line `pam_sm_setcred: ntlm user …
  doesn't have a password` is noise. `sysadminctl -resetPasswordFor` without an admin's
  credentials refuses on a FileVault Mac ("not permitted without secure token unlock").
- **Its LaunchAgent did not start.** launchd scanned the account's
  `~/Library/LaunchAgents`, and Background Task Management failed to register the agent
  (`registerLaunchItem: failed to construct an identifier`, then
  `effectiveItemDisposition: record not found`), so launchd never ran it. The uid was the
  cause (next spike).

faber's setup makes `_faber_vm` today with
`sysadminctl -addUser … -roleAccount`, no password and shell `/usr/bin/false`, and its
shape check accepts no other shell on macOS. That `_faber_vm` fails at the login window
before the shell check: with no password it has no way to authenticate
(`ODErrorCredentialsMethodNotSupported`). Whether a `-roleAccount` account can log in once
it has a password is untested.

### Spike result, 2026-10-23: the agent runs once the account's uid is 501 or more

Background Task Management keeps records only for uid 0, uids 500 and up, and two system
accounts (201 and 248). For every other uid below 500, `-[BTMStore setForUID:]` in
`backgroundtaskmanagementd` returns nil (macOS 27.0, read from the binary). With no record
set, each registration is dropped, every lookup is `record not found`, and launchd refuses
the account's third-party agents as untrusted (`Untrusted service was denied launch by
BTM`). For the account below 500 that also stopped signed third-party agents in
`/Library/LaunchAgents`. BTM's usage tracking (`LaunchMonitor`) starts only at uid 501, so
501 is the floor.

The same account was then recreated at a uid above that floor: still hidden, home under
`/var/db`, shell `/sbin/nologin`, no SecureToken, on its first login. BTM made its record
set and allowed the agent (`[enabled, allowed, not notified]`), and launchd started it in
the session (`manager=Aqua`). It cloned `xcode-27.1` and started the VM to
`state=running`. The operator logged the session out half a minute later, so the guest's
boot was not watched. The `failed to construct an identifier` errors appear at the higher
uid too, where the agent runs: they are noise.

So (a) holds for an account whose uid is 501 or more. With ADR-0025 placing `_faber` and
`_faber_vm` at 450–499, the session account is either a separate account above 500 or
`_faber_vm` moved there. That is the owner's call.

### Spike result, 2026-10-23: the VM boots, and survives switching away and the lock

At the higher uid, the session's agent started the VM about ten seconds after login. The
guest was on the network about 20 seconds later and answered ping. The operator stayed on
the session's desktop for several minutes, switched back to their own session by fast user
switching (no logout), and later locked their own screen for a few minutes. A watcher in
the operator's session sampled every 10 seconds: the same `faber-vm` process ran in every
sample, over a hundred of them, including those taken while the screen was locked, and the
guest still answered ping more than 20 minutes after the VM started.

So (b) holds, and (c) holds for switching away and for the lock. The operator logging out
of their own session is untested: that logout ends the watcher too, so the next witness
has to run outside the operator's session.

### Spike result, 2026-10-25: the session survives the operator's logout and comes back from a FileVault unlock

The same throwaway account (uid above 501, `/sbin/nologin`, not admin), this time visible
and with a SecureToken, as the *Unlock* mode makes it; `fdesetup list` then listed it next
to the operator. The witness was a root LaunchDaemon sampling every 10 seconds from the
system domain, so it outlived the operator's logout and came back after the reboot.

- **The operator's logout.** With the account's VM running and answering ping, the
  operator logged out of their own session and back in. The account's GUI domain stayed
  up and the same `faber-vm` process kept running, with the guest answering ping in every
  sample, including the one taken at the login window with only the account logged in.
  The logout lasted only seconds; it shows the session is not tied to the operator's,
  not that it survives a long one.
- **A cold reboot with FileVault on** (`shutdown -r now`, not `fdesetup authrestart`).
  At the FileVault screen the operator unlocked as the account and landed on its desktop.
  Its agent started about 20 seconds after the witness's first sample after boot; the VM ran
  and the guest answered ping about 10 seconds after that. The operator then switched to
  their own account; the VM kept running. faber's daemon came back by itself.
- **A reboot is a hard stop for a running VM.** At shutdown the VM got SIGTERM and stopped
  with `VZErrorDomain 1` ("The virtual machine stopped unexpectedly"), not a clean guest
  shutdown. Job VMs are throwaway clones, so this costs only the job; a planned reboot
  still drains first.
- **Found on the way: every clone of an imported golden image fails to start.**
  `clonefile(2)` keeps the image's `0444`, and Virtualization.framework opens `disk` and
  `vz-aux` read-write ("Failed to open auxiliary storage … Permission denied"). With
  `chmod 0600` on those two after the clone, the VM booted. That is the fix in #350,
  which the macOS path needs before any job can boot.

So (c) and (d) hold, and the design under *Decision* stands unchanged.

## Recommendation

**Option 2, revised (above).** Its spikes passed (a)–(d). Options 1 and 1b are ruled out:
1 does not boot, and 1b needs sudo, which the owner rejected. Option 4, GitHub-hosted macOS
runners, stays the fallback if a later macOS release breaks the login session.

## Decision (2026-10-24)

- **Option 2.** macOS VMs run in the login session of a dedicated account, never as the
  operator. ADR-0025's split holds.
- **A third account, separate from `_faber_vm`.** Example name `_faber_mac`. Its uid comes
  from a block at 600–699: Background Task Management ignores uids below 500 and its
  LaunchMonitor needs 501 or more (spike above), and the block keeps clear of people's
  accounts at 501 and up. Not admin. Shell `/sbin/nologin`. A password. setup makes it
  with the dscl recipe above, because `sysadminctl -roleAccount` only makes uids below
  500. It never holds the token. `_faber_vm` is unchanged and keeps the Linux VM, so a
  macOS-guest escape cannot reach the Linux engine or its Docker socket bridge.
- **Its own LaunchAgent** (`LimitLoadToSessionType = Aqua`) in that session is the macOS
  VM supervisor.
- **Reboot modes,** chosen per installation at setup and changed by running setup again:
  - *Unlock*: the account gets a SecureToken. After a reboot the operator unlocks the disk
    as it, which starts its session. Pre-selected when FileVault is on. Its password
    becomes a disk-unlock credential.
  - *Auto-login*: macOS logs it in at boot behind the lock screen (`autoLoginUser`,
    `autoLoginUserScreenLocked`, `/etc/kcpassword`). Only with FileVault off, since macOS
    ignores auto-login while FileVault is on. Never pre-selected: it is an explicit,
    security-relevant opt-in, stated rather than assumed (ADR-0012 says the same of
    Linux linger and macOS auto-login).
  - *Manual*: a password only; the operator logs it in. Pre-selected when FileVault is
    off.
- **Visibility follows the mode:** visible for Unlock and Manual, hidden for Auto-login.
  setup lets the operator override it. The login window's "Other…" field is turned on only
  for a hidden account that is logged in by hand.
- **Password:** the operator types one at setup or asks faber to generate one. A generated
  password is shown once, in every mode. Generate is pre-selected for Auto-login. faber
  stores the password only where Auto-login needs it (`/etc/kcpassword`).
- **A "no VM session" admission gate:** while the session is down, faber refuses macOS VM
  jobs, and the dashboard and `faber status` say which mode applies and what to do.
- **Per-host VM layout:** the Linux VM on or off and its size, macOS VMs 0–2 and their
  size, and the machine share (ADR-0021), as config keys first and in the Settings UI
  later (#343). Today the Linux VM has no off switch; this adds one. Turning macOS VMs on
  needs the session account, which only setup can create.
- **Target layout for the current hosts:** the builder ends running macOS VMs only
  and the peer host the Linux VM only. Until the peer host's cut (#339), the rehearsal
  host runs both, its Linux VM shrunk to leave room for the macOS VMs.
- **uninstall** removes the account, its agent, any auto-login settings and
  `/etc/kcpassword`.

## Consequences (of Option 2, revised)

- **The peer host's cut (#339) waits on this.** Until macOS VMs start, the native backend
  there is the only thing running macOS jobs.
- **The builder's native profile stays on the ignored backend,** so it claims no
  macOS job it cannot run. The test-only `faber-vm` smoke profile proves the change.
- **New admission gate:** "no VM session" refuses macOS VM jobs. The dashboard and
  `faber status` say so in words.
- **setup** creates the session account (uid 600–699, password, `/sbin/nologin`), sets its
  reboot mode and visibility, installs its LaunchAgent, and tells the operator the login
  step for that mode. `uninstall` removes them.
- **ADR-0026 is amended** (its *Amended 2026-10-25* note): macOS VMs start in the session
  account's own login session, Linux VMs do not, and each host's VM layout is configurable,
  including turning the Linux VM off. **ADR-0025 is amended** too: a third account that can
  log in, and macOS auto-login as an explicit opt-in.
