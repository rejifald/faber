---
title: faber builds the golden image, and runs jobs only on a verified one
type: adr
status: proposed
date: 2026-10-21
owner: rejifald
---

# ADR-0049: faber builds the golden image, and runs jobs only on a verified one

## Status

proposed (2026-10-21). The owner settled each decision below in one design
session. It amends ADR-0026's *The golden image* and its Lima section, and
narrows ADR-0025's rule on which account runs VMs, as stated below. The two facts
it depended on were measured on 2026-10-21; see *Measurements*.

## Context

ADR-0026 runs every macOS job in a throwaway clone of a golden image: macOS,
Xcode, the iOS runtime and a non-admin auto-login `runner` user. It says the
operator "prepares it once", and `sudo faber setup --golden-image` imports a
finished bundle. How the bundle comes to exist is left to the operator.

In the #76 spike it came from three hand-run scripts inside a Lima VM:

- **It took about 2 hours and 25 GB of downloads,** and the spike's image was
  later deleted to free disk for CI. Every host needs one before the cut,
  and again after every Xcode or macOS update.
- **The preparing VM ran outside the wall.** Homebrew's installer came from
  GitHub's `HEAD` and ran as an admin, in a VM that had the host's position on
  the network. ADR-0026 itself rejected that exception for the Linux build VM
  (#236).
- **Nothing checked the result.** A forgotten Apple ID sign-in, FileVault left
  on, or a weak admin password would be copied into every clone.
- **The image cannot be shared.** Apple's licences forbid redistributing macOS
  and Xcode, and nothing faber needs is ever published. Every host builds its
  own.

Two parts of the build cannot be automated well:

- **Setup Assistant.** A fresh macOS install waits for a person. Scripted
  clicks, or writing an account onto the disk before first boot, both break
  without warning on a new macOS release.
- **Xcode.** Apple serves it only to a signed-in Apple ID. That sign-in must
  never be in an image, because every clone would carry it.

People choose macOS VMs for Apple's own tooling: Xcode and simulator runtimes.
Those are large and slow to install, so they belong in the image. Tools that
projects pin themselves (Ruby, CocoaPods, Node) do not.

## Decision

### 1. The CLI builds the image; the dashboard only guides

`faber golden prepare` and `sudo faber golden build` are the build. They ship in
the signed release (ADR-0031), so a host needs no checkout. The guest-side
script is a plain `.sh` file in the repo, which the CLI writes onto a setup disk.
Its version therefore always matches the bootstrap and agent it installs.

The dashboard's assistant (§10) runs the same commands. It adds guidance, not
behaviour, and nothing depends on it.

### 2. Two phases: a person in an offline window, then a walled build

**`faber golden prepare`, as the operator, without `sudo`.**

- `faber-golden-prep` (shipped in releases, never `faber-vm`) installs macOS
  from Apple's restore image with no screen, then boots the VM in a window on
  the operator's desktop. The VM has **no network device**, so there is
  nothing for the wall to hold.
- The operator completes Setup Assistant offline, following a checklist: any
  admin name except `runner`, a throwaway password (§5), no Apple ID, FileVault
  off.
- The operator runs one command from the setup disk,
  `sudo sh /Volumes/FABERSETUP/arm.sh`. `arm.sh` installs the run-once
  provisioning LaunchDaemon for the next boot and powers the VM off. The guest
  reports to the host over a virtio console port, which appears in the guest
  as `/dev/cu.virtio` (measured, see *Measurements*).
- A desktop session is required for this phase, locally or over Screen
  Sharing. Without one, `prepare` stops before downloading anything and says so.
  An SSH-only host is out of scope.

**`sudo faber golden build`, headless, as `_faber_vm`.**

- Root clones the prepared bundle into a build bundle owned by `_faber_vm`,
  under `_faber_vm`'s home as the Linux build's is, because `_faber_vm` cannot
  enter the operator's home. The operator's copy is never booted again.
- It boots that clone on the build network, slot 15, under the rules
  ADR-0026 gives the Linux build VM (#236). It refuses unless the stored rules
  hold that network with no grants and the checker's verdict is green. The guest
  fetches nothing until DNS to its gateway goes unanswered, and powers off if
  that has not happened within 120 s.
- The provisioner installs everything (§3–§5), removes itself, and powers off.
  Root reads only the guest's console, bounded, by exact line prefix, as for the
  Linux build.
- The same command then imports the bundle (the existing import, with the
  manifest of §6) and runs the verification of §7.

`prepare` runs as the operator only because a window needs a desktop. This
narrows ADR-0025's rule that only `_faber_vm` runs VMs: the one exception is a
VM with no network device, and ADR-0025 gets that note when this ADR is
accepted.

**The build needs the wall first.** On a new host the order is `sudo faber
setup` without `--golden-image` (the flag is already optional), then `prepare`,
then `build`. The cut runbook (kept with the legacy repository) changes with the
CLI's PR, not before, so it never names a command that does not exist yet.

**Where the bundle lives.** `prepare` writes to
`~/Library/Application Support/faber/golden-build/<name>/`. It refuses if that
is not on the same volume as `/var/db/faber/images`, because the import's clone
is free only on one volume. Before each large step it checks free disk against
that step's real need: about 85 GB at peak (restore image ~25 GB, an Xcode disk
~15–20 GB, the VM disk ~40 GB), plus ADR-0024's floor. The macOS 27.0.1
restore image measured 25 GB, and a fresh install used 26 GB of its disk. It stops cleanly rather
than filling the disk. Both phases resume after an interruption. CI is not
paused. The restore image is deleted at the end unless `--keep-ipsw` is given,
and the operator's staging copy is deleted only when they agree.

### 3. One Xcode per image, from a source Apple signed

- **One Xcode per image.** A host that needs two has two named images, and each
  profile picks one with `image = "<name>"`, as today. The default name is the
  Xcode version, `xcode-27.0`. Several Xcodes in one image would need
  `sudo xcode-select`, which `runner` cannot run.
- **Where the Xcode comes from**, in this order:
  1. `--xcode <Xcode.app or .xip>`, if given.
  2. The host's `Xcode.app`, after showing its version and asking.
  3. Otherwise, Apple's download page opens in the operator's browser at the
     right version, and the build waits for a signed `Xcode_*.xip` in
     `~/Downloads`. A non-interactive run prints the URL and stops.
- **It is checked every time.** `codesign --verify --deep --strict` with Apple's
  team identity for an app, `pkgutil --check-signature` for a `.xip`. This
  catches a corrupt, partial or locally modified Xcode. It is not a defence
  against someone who already controls the host.
- **It reaches the guest on a read-only disk image,** attached to the build VM
  only. No host folder is ever shared.

### 4. Simulator runtimes: the host's by default, shown before anything starts

- **By default the build takes the host's installed runtimes** that are
  compatible with the chosen Xcode. If the Xcode is a different version from the
  host's, it falls back to the matching iOS runtime and says so.
- **The list is shown with sizes and a total before the build starts,** and the
  operator confirms or edits it. `--runtime <platform>[:<version>]` replaces the
  inferred list. `--no-host-runtimes` keeps only the matching iOS runtime. A
  non-interactive run prints the list it used.
- Runtimes are downloaded from Apple with `xcodebuild -downloadPlatform`, which
  needs no Apple ID, during the walled phase. They are never copied from the
  host.

### 5. What the base holds, and what it locks

- **Homebrew, owned by `runner`.** Jobs can `brew install` without `sudo`, which
  many iOS workflows assume. The installer comes from a tagged Homebrew release,
  never `HEAD`, and only after the wall is proven.
- **A fixed tool list:** `jq`, `gh`, `zstd`, `git-lfs`. **The operator may add
  formulas** with `--brew <formula>`. There are no arbitrary shell steps, so the
  recipe stays readable. Ruby, CocoaPods, fastlane and Node are not in the fixed
  list, because projects pin their own versions.
- **Software Update is off in the guest.** Clones live for one job, and an
  update inside one would make images differ from the recipe. An image changes
  only by being rebuilt.
  - The provisioner sets it, as its first step and before anything is fetched:
    automatic checks, downloads, macOS updates, and security and config-data
    installs all off, in `/Library/Preferences/com.apple.SoftwareUpdate`.
  - Setup Assistant cannot do it. On macOS 27 its "download only" choice leaves
    downloads and the security and data-file installs on, so it is not on the
    operator's checklist.
  - Measured on 2026-10-21 in a macOS 27.0.1 guest: `defaults write` of
    `AutomaticDownload`, `CriticalUpdateInstall`, `ConfigDataInstall` and
    `AutomaticCheckEnabled` to false, as root, turns all three System Settings
    switches off. `softwareupdate --schedule off` does not: it still reports
    automatic checking as on.
  - *Amended 2026-10-26: the guest agent enforces it at every boot.* The rule
    stands as written, but the image alone does not keep it. On a real host, an
    image whose disk held all four keys false (read read-only from the image)
    booted a verify clone in which, about a minute after boot,
    `AutomaticCheckEnabled` was absent, so it defaults to on. The other three
    stayed false. Something in a clone's boot removes it; the likely cause is
    `softwareupdated` reacting to the fresh machine identifier. So the guest
    agent, as root, writes the provisioner's step-a keys again: the four keys
    false in `/Library/Preferences/com.apple.SoftwareUpdate`, and `AutoUpdate`
    false in `com.apple.commerce`. It writes them through `defaults write`, as
    the provisioner does, never into the file under `cfprefsd`. It does this
    twice. The first time is when `faber-agent serve` starts, before it listens,
    so no clone reports `ready` without it. The second is right before a job's
    hand-off on `begin`, so a job starts with the keys off even if a daemon
    rewrote them in between. The two key lists are kept equal by a test that
    reads `provision.sh` (launcher/Tests/UpdatesTests.swift). A write that fails
    never stops the agent. It writes a `FABER-AGENT updates_off` line on the
    guest's console instead. §7 still judges the plist exactly as before, so a
    key that boot removes again after the agent's write still fails
    verification.
- **The Setup Assistant admin is locked.** The provisioner gives it a random
  32-byte password that is never stored, sets its shell to `/usr/bin/false` and
  hides it. It stays, because macOS can misbehave with no admin at all. A usable
  admin would let job code become root in its guest, which undoes ADR-0026's "a
  job gives up `sudo`".
- Everything else ADR-0026 lists stays: the `runner` user with auto-login
  through `/etc/kcpassword`, the bootstrap from `launcher/golden/`, the listed
  resolvers, Remote Login off, and no runner or agent in the image.

### 6. The recipe lives in the bundle

The build writes `manifest.json` into the bundle. It holds the recipe (Xcode
source and version, runtimes, extra formulas, restore image) and what was
actually installed: macOS build, Xcode build, runtime versions and
`brew list --versions`.

- **The installed half is guest-reported.** Root writes it from the build VM's
  console lines that carry a fixed prefix, bounded like every other console
  read, and never parses anything else the guest produced. It is a record, not
  a proof: §7 checks what matters on a fresh clone.
- **`manifest.json` is an optional bundle member.** The import accepts the four
  `GOLDEN_FILES` with or without it, and hashes it when it is present, so it
  cannot drift from the image it describes. It does not join the required set.
- **A rebuild starts from it:** `faber golden prepare --from <name>`, with flags
  overriding parts.
- `config.toml` still only names the image a profile uses. A recipe there would
  look editable while changing nothing until someone rebuilds.
- A bundle without a manifest, such as one prepared by hand, still imports and
  can pass §7. It shows as "unknown recipe" and cannot be rebuilt with `--from`.

### 7. Only a verified image runs jobs

`faber golden verify <name>` boots one clone exactly as a job does: under
`_faber_vm`, through the supervisor, behind the wall, with a fresh machine
identifier. It checks:

- the console user is `runner`, `runner` is not an admin, and every admin
  account cannot log in;
- Remote Login, FileVault and any Apple ID sign-in are off, and every
  automatic Software Update setting is off;
- the guest cannot reach the host, the LAN or the tailnet (the existing wall
  proof);
- `xcodebuild -version` matches the manifest, and every runtime in it is ready;
- `runner` owns the Homebrew prefix.

**It is a gate.** Admission sends no macOS job to an image without a passing
verification. A pass is recorded against the image's file hashes and the host's
macOS build. **When either changes, the image is unverified again,** and the
daemon re-runs verification itself. ADR-0026 already requires a re-test of the
fresh-identifier trick after every macOS update; this makes that re-test
automatic and makes admission depend on it.

A failure names the failed check and what to fix. There is no separate
acknowledgement: the passing result is the acknowledgement.

*Amended 2026-10-26: a failure is explained, too.* A code names which check
failed, not why, and on a real host two checks failed that the image's disk
said should pass. So the guest agent also writes one line per check to the
guest's console (which account, which key and value, which exit status), and
the macOS VM supervisor keeps that console as `verify-<image>-latest.log` in its
private state directory. The verdict is still the codes alone: `_faber` never
reads the console, and the refusal text only names the file and the `sudo cat -v`
that shows it (docs/vm-agent-protocol.md, *verify*; docs/vm-supervisor-protocol.md,
*A verification's console log*).

*Amended 2026-10-26, again: the Software Update keys.* The software_update
check still reads `/Library/Preferences/com.apple.SoftwareUpdate.plist`, and its
strictness is unchanged. After its line, the agent writes one evidence line with
the first line of `softwareupdate --schedule`. That line never changes the
verdict. A clone that had lost `AutomaticCheckEnabled` at boot is why the agent
now writes the keys again at boot (§5, *Software Update is off in the guest*).

*Amended 2026-10-26, a third time: the developer tools as `runner` sees them.*
An image passed every check, `xcode` included, and then every macOS job it
served stopped at its first `git` or link step with "No developer tools were
found, requesting install". The check ran `xcodebuild -version` as root, and
root reads any file whatever its mode, so it proved only that root could use
Xcode. A job is `runner`. The same blind spot hid the tools-disk modes the day
before. So the `xcode` check now runs as `runner` (through `sudo` from root,
with `runner`'s home), and passes only when all of these hold:

- `xcodebuild -version` matches the manifest, as before;
- `xcode-select -p` resolves, to a directory `runner` can enter and read;
- `/usr/bin/cc` compiles, links and runs a C program, and `/usr/bin/swiftc` a
  Swift one, through the same shims a job runs.

It keeps its code, `xcode`, so the wire does not change. Its console line names
which of these failed. A pass recorded before this change is not trusted:
the record's schema goes to 2, so every image is verified again under the
stricter check on the first boot of a daemon that has it. The file hashes are
kept, so no image is hashed again. Any later check that gets stricter raises the
schema the same way (src/vm/verify.rs, `SCHEMA`).

The likely cause on that host is in the build. An image built from an
`Xcode.app` (§3, source 1 or 2) stages it by extracting the operator's archive
into a fresh `0700` folder with `--no-same-permissions`. The top folder keeps
`0700` and the rest follows root's umask, and the guest's `ditto` copies those
modes into `/Applications/Xcode.app`. So the build now gives every staged folder
`0755` and every file `0644`, or `0755` when its owner may run it, before the
signature check (`shared_mode` in src/setup/golden_build.rs). A `.xip` is
expanded in the guest with the archive's own modes and was not affected.

The rule this adds: **a check proves what a job will see, as the account the
job runs as.** A check that root passes and `runner` would fail is a check of
the wrong account.

### 8. The build holds one of the two macOS places

Apple allows two macOS VMs per host, and admission caps macOS jobs at two
(ADR-0026). The build takes one of those places through the daemon. It waits
until one is free, holds it while its VM runs, and releases it afterwards.
Admission counts it, so a job never meets `VZErrorDomain 6`. The VM rows show
the build. If the daemon is not running, as on a host's first build, the build
tries anyway and says plainly if both places are taken.

### 9. Images age; faber nudges and never blocks

- **A newer macOS point release raises an attention row** as soon as Apple's
  catalog lists it. This is the existing warn row "golden image out of date"
  (docs/design/popover.md). The catalog does not say which releases carry
  security fixes, and almost all do, so every one is treated as if it did: "macOS
  27.0.2 is out; this image runs 27.0.1. Point releases usually carry security
  fixes."
- **Snooze for 7 days,** like the update rows' Skip. A newer release brings the
  row back at once.
- **A newer Xcode on the host** raises a separate row, at severity info.
- **Rebuild** on either row opens the assistant with `--from` that image.
- Nothing blocks jobs and nothing rebuilds on its own. The wall contains an old
  guest; blocking would turn a reminder into an outage; and a rebuild needs a
  person for Setup Assistant anyway. A host macOS update that breaks cloning is
  caught by §7.

### 10. The dashboard assistant guides; it never runs anything as root

In the dashboard (ADR-0041), built in React (ADR-0042):

1. **Plan:** the inferred Xcode and runtimes with sizes, the disk check and any
   extra formulas. Confirming it yields the recipe.
2. **Prepare:** the desktop app starts `faber golden prepare` in the operator's
   session. The VM window opens beside the dashboard with the Setup Assistant
   checklist, and the assistant moves on when the VM powers off.
3. **Build:** the exact `sudo faber golden build …` command with a copy button.
   The operator runs it in Terminal.
4. **Progress:** the build's current step, from a small status file the build
   writes for the daemon to read. No guest text is shown.
5. **Result:** the verification checks, each passed or failed. On a pass it
   offers to point a profile at the image through the settings page's existing
   path.

## Delivery

1. This ADR.
2. **The CLI path:** the guest scripts (`arm.sh`, which installs the run-once
   provisioner, and `provision.sh`), `prepare` (`faber-golden-prep`, shipped in
   releases and never `faber-vm`: windowed, no network, `VZMacOSInstaller`), `build`, and the optional manifest in the
   import.
   The cut runbook (kept with the legacy repository) changes here.
3. **The gate:** `verify`, admission refusing unverified images, re-verification
   after a host macOS update, and the build's macOS place.
4. **Reminders:** reading Apple's catalog, the rows, snooze and Rebuild.
5. **The dashboard assistant.**

Steps 2 and 3 come before cut phase A. That host's image is the first one built
with them.

## Measurements

Both were measured on 2026-10-21 under macOS 27.0, with a
throwaway test tool on Apple's Virtualization framework rather than faber's
launcher. Both are properties of the framework and the guest, not of the
launcher.

- **A macOS guest finishes Setup Assistant with no network device.** A fresh
  macOS 27.0.1 (`26A434`) install, booted in a window with no network device,
  reached the desktop through Setup Assistant's "My computer does not connect
  to the internet" path. The owner drove it. The install itself, with
  `VZMacOSInstaller`, also needs no guest network. So §2 stands as written:
  `prepare` has no network and needs no prior `sudo faber setup`. Because a new
  macOS release can change Setup Assistant, `prepare` treats a stall at the
  network screen as a clear failure, not a hang.
- **A virtio console port reaches a macOS guest, and root can write to it.**
  A `VZVirtioConsoleDeviceSerialPortConfiguration` appears in the guest as
  `/dev/cu.virtio`, and root writes lines to it. That is the build's report
  channel (§2). In the same guest, `softwareupdate --schedule off` has no
  effect, but `defaults write /Library/Preferences/com.apple.SoftwareUpdate`
  for `AutomaticDownload`, `CriticalUpdateInstall`, `ConfigDataInstall` and
  `AutomaticCheckEnabled`, as root, does turn Software Update off.
- **Linux VMs do not count against Apple's two-VM limit.** Two macOS VMs (clones
  with fresh machine identifiers) ran beside two Linux VMs: Docker Desktop's,
  which uses the same framework, and an EFI Debian VM that booted to its login
  prompt. A third macOS VM was refused with `VZErrorDomain 6`. With one macOS VM
  stopped, the third started at once. The limit is two macOS VMs, so §8 holds
  the build's place out of two whatever Linux VMs run. This also settles
  ADR-0026's runtime test 16.

## Consequences

- **Every host can produce its own image** from a release and an Xcode, without
  Lima, a checkout or SSH into a guest.
- **The only unwalled VM has no network.** Everything that downloads runs on the
  path ADR-0026 already reviewed for the Linux build.
- **Mistakes in the manual steps stop admission** instead of shipping into
  clones.
- **The build adds three commands** and a separate tool with a window,
  `faber-golden-prep`, which is not the launcher.
- **The import accepts an optional fifth file,** `manifest.json`. Images
  without one import and verify as before, show "unknown recipe", and cannot be
  rebuilt with `--from`.
- **A host with two Xcodes stores two ~40 GB images.** Images built separately
  share no blocks.
- **Workflows lose `sudo xcode-select`.** A job selects its Xcode by picking an
  image, through a profile label.
- **ADR-0026's image loses Homebrew's `ruby` and `cocoapods`,** which it listed
  as release-job tools. A host whose jobs need them pre-installed adds them with
  `--brew ruby --brew cocoapods`; otherwise jobs install their own pinned
  versions.
- **ADR-0025 is narrowed:** a VM with no network device may run under the
  operator's account, for `prepare` only.
- **ADR-0026 is amended:** *The golden image* is built as described here, and
  *Lima prepares; it does not run jobs* no longer applies to the golden image,
  since faber prepares it itself. A bundle Lima prepared still imports and
  verifies.

## Alternatives considered

**Automate Setup Assistant.** Either with scripted clicks over the VM's screen,
as Packer's tart plugin does, or by writing an account and the setup-done marker
onto the guest's disk before first boot. Both break silently on a new macOS
release, and the second is undocumented. Rejected by the owner (2026-10-21): the
operator spends about 5 minutes in the window instead.

**One walled VM for both phases, run by the operator.** This would put a VM with
a network under the operator's account, against ADR-0025, and the build slot's
lock lives where only `_faber_vm` can reach it.

**Show a `_faber_vm` VM's screen to the operator** through the Virtualization
framework's private VNC server. That is private API that any macOS update may
change.

**Several Xcodes in one image,** as GitHub's hosted images do. It saves disk on
hosts that need two, and it breaks the common `setup-xcode` action, which runs
`sudo xcode-select`.

**No Homebrew; pinned binaries only.** This makes images reproducible and breaks
every workflow that runs `brew install`. The manifest makes images comparable
instead.

**The recipe in `config.toml`.** See §6.

**Read Apple's security-releases page** to warn only about security releases.
That page is meant for people, its layout can change, and a parse failure would
hide the warning that matters.

**Automatic rebuilds.** A rebuild takes hours and a person, and would run
unprompted on a busy host.
