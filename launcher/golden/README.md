# The golden macOS image

faber's macOS jobs run in clones of one golden image per host (ADR-0026, *The golden image*).
ADR-0049 splits building it in two, so that nothing downloads outside the wall. The operator
runs two commands:

1. **`faber golden prepare <bundle>`**, as yourself, without `sudo`, at the Mac's desktop
   (Phase 1 below). macOS is installed from Apple's restore image, and the VM boots in a window
   with **no network device at all**. You complete Setup Assistant offline and run one command,
   `arm.sh`, from a read-only setup disk. That is all the window does.
2. **`sudo faber golden build <bundle> --name … --xcode …`** (Phase 2 below). The armed VM boots
   behind the wall, its network a build network with no grants, and runs `provision.sh`, which
   does every download: Xcode's runtimes, Homebrew and its formulas. Progress and facts come
   back over a virtio console. The same command then imports the result.

`prepare` prints the exact `build` command when it finishes.

The wall must exist first, so a host runs `sudo faber setup` (without `--golden-image`) before it
builds. An image built by hand still imports with `sudo faber setup --golden-image`.

This directory holds the guest's scripts and the one Swift tool; the orchestration is the CLI's
(`src/setup/golden_prepare.rs`, `src/setup/golden_build.rs`). `build.sh --compile-only` builds
the tool for CI and for releases, and nothing else.

## Phase 1: `faber golden prepare`

```sh
faber golden prepare "$HOME/faber-golden/macos-27"
```

Run it as yourself, never with `sudo`, from a desktop session (at the Mac or over Screen
Sharing; an SSH-only session is refused before anything is downloaded). It needs
`faber-golden-prep`, which ships in each release as `faber-golden-prep-<target>`. Install it
root-owned beside the CLI, and the same release as the CLI:

```sh
sudo install -m 0755 -o root -g wheel faber-golden-prep-aarch64-apple-darwin /usr/local/bin/faber-golden-prep
```

`prepare` refuses a tool that is not root's, or sits in a directory others can write, or reports
another version than the CLI. Options:

| Option | Meaning |
|---|---|
| `--ipsw FILE`, `--sha256 HEX` | install from this restore image, and refuse it unless its SHA-256 is HEX |
| `--keep-ipsw` | keep a downloaded restore image (default: delete it after the install) |
| `--disk-gib N` | the guest disk's size, 64 to 2048, default 100. It is sparse |
| `--xcode PATH` | the Xcode: an `Xcode.app` or an `Xcode_*.xip` |
| `--runtime PLATFORM[:VERSION]` | a simulator runtime; repeatable; replaces the inferred list |
| `--no-host-runtimes` | only the matching iOS runtime, not this host's |
| `--brew FORMULA` | a Homebrew formula on top of `jq gh zstd git-lfs`; repeatable |
| `--from NAME` | start from the recipe stored with `/var/db/faber/images/NAME`; flags override |
| `--yes` | do not ask for confirmation |

In order:

1. **Where.** The bundle's parent must be on the same APFS volume as faber's image store,
   `/var/db/faber/images`, so the import is a free clone. The volume needs the whole build's
   peak (about 85 GiB: restore image, an Xcode disk, the VM disk) plus ADR-0024's 10 GiB
   floor, 95 GiB, to start; each large step checks again against its own need (the download
   and install 65 GiB, the install alone 40 GiB, the boot 12 GiB). It stops cleanly rather than
   fill the disk.
2. **The recipe**, shown before anything starts, and confirmed unless `--yes`:
   - **Xcode**: `--xcode` if given; else this host's `/Applications/Xcode.app` (its version and
     build are shown and you are asked); else Apple's download page opens in your browser and
     `prepare` waits for a new `Xcode_*.xip` in `~/Downloads`. Without a terminal it prints the
     URL and stops. The Xcode is checked against Apple's signature right away (`codesign` with
     the requirement `golden build` holds an `.app` to, `pkgutil --check-signature` for an
     `.xip`), so a bad one is found before the hours are spent.
   - **Runtimes**: by default this host's (`xcrun simctl runtime list -j`) that the Xcode's SDK
     can run, with the matching iOS runtime added if none is one. When the Xcode is not this
     host's, or is older than Xcode 26, it falls back to the matching iOS runtime and says so.
     Each is shown with its size, and a total.
   - **Formulas**, and the image name, `xcode-<version>`.
3. **Install.** The newest restore image this host supports comes from Apple's catalog
   (`VZMacOSRestoreImage.fetchLatestSupported`), is downloaded over HTTPS from Apple's servers
   (`*.apple.com` or `*.cdn-apple.com`), and is verified: its SHA-256 is printed, and it must
   load, install on this host, and be the build the catalog named. The installer then checks
   Apple's signature on everything it restores. About 15 minutes. The restore image is deleted
   after, unless `--keep-ipsw`.
4. **Boot.** The bundle boots in a window with no network device. A read-only disk, the volume
   `FABERSETUP` (`/Volumes/FABERSETUP` in the guest), carries `arm.sh` and the files it
   installs. The Xcode and the recipe are not on it: they join the build's own setup disk. The
   phase ends when the guest shuts itself down, which `arm.sh` does. Closing the window, or
   Ctrl-C, asks the guest to shut down; a second one forces it off, and `prepare` then boots it
   again when you re-run it.
5. **Done.** The recipe is written to `<bundle>/recipe.json` (yours, and not a file the build
   takes), and the next command is printed:

   ```sh
   sudo faber golden build "$HOME/faber-golden/macos-27" --name xcode-27.1 \
     --xcode /Applications/Xcode.app --runtime iOS:26.3.1 --runtime iOS:27.1
   ```

**Resuming.** A sidecar beside the bundle, `.<name>.prepare`, records the last phase that
finished. Run the same command again after an interruption: a bundle whose install finished
skips to the boot; one whose boot finished goes straight to printing the command. A bundle with
no sidecar (a half install, or one made another way) is refused rather than adopted, because
booting an armed bundle would run the provisioner in the offline window. Remove it and run again.
The bundle, its files and the sidecar must be yours and not symlinks, and its parent must not be writable by group or others, so another account cannot plant a bundle that `prepare` would boot. A failed install removes its bundle and keeps the restore image.

**No credential goes in.** `prepare` puts no SSH key and no password into the image, and the
read-only disk holds only the files above. Remote Login is never turned on. The one credential
the image needs, the auto-login password, is random, is set by `provision.sh`, and nobody sees
it. Your Apple ID is used only in your browser, to download Xcode.

The window and the installer are `prepare.swift` (`faber-golden-prep`), compiled by
`build.sh --compile-only` with Swift and Apple's frameworks only. It is **not the launcher**:
`faber-vm` has neither a window nor an installer, and this tool is never on a job's run path. Its
`--version` prints `faber-golden-prep <x.y.z>+g<sha>`.

### The window checklist (the operator's)

`prepare` prints this too. In the guest's window:

1. **Setup Assistant, offline.** At the network screen choose "My computer does not connect
   to the internet". Create the admin account with **any name except `runner`** (the build
   makes that user) and a **throwaway password**: the build locks every admin account, so
   nothing keeps it. Do **not** sign in with an Apple Account. Leave **FileVault off**,
   because automatic login needs it off. Skip Location Services, Siri, Screen Time and
   analytics.
2. **Arm.** In Terminal:

   ```sh
   sudo sh /Volumes/FABERSETUP/arm.sh
   ```

   It refuses, changing nothing, if it is not root, a `runner` user exists, or FileVault is
   on. Otherwise it installs a root-owned run-once LaunchDaemon,
   `io.github.rejifald.faber.golden-provision`, and its wrapper under
   `/Library/Application Support/faber-guest/`, then shuts the guest down.

The bundle is then ready for the walled build.


## Phase 2: the walled build

After `faber golden prepare` has finished (it prints this command), run it from your own account with `sudo` (not
from a root shell), on a host that has run `sudo faber setup`:

```sh
sudo faber golden build "$HOME/faber-golden/macos-27" --name xcode-27.0 \
  --xcode "$HOME/Downloads/Xcode_27.0.xip" \
  [--runtime iOS:18.5]... [--brew ruby]... [--replace]
```

It takes hours and holds one of the host's two macOS VM places. If two macOS VMs already run,
the VM cannot start; the command says so, and you run it again later. In order, refusing
before the first large step wherever it can:

1. The wall must cover the build network (slot 15, the Linux image build's) and be green, or
   nothing happens. It is read again every 2 s while the VM runs, and the VM is killed if the
   wall stops covering the network.
2. Your bundle is checked: exactly `disk`, `vz-aux`, `vz-hwmodel` and `vz-identifier` (and the
   `recipe.json` that `prepare` leaves, which is never read), each a
   regular file with one link, owned by you, none a symlink. The name must be free in the
   store, or `--replace` given. An `.app` gets the `codesign` check `provision.sh` makes, and
   the store's volume must have room for the Xcode twice plus 50 GiB.
3. Root builds the `FABERSETUP` disk: `provision.sh`, `recipe.env`, the bootstrap and its
   plist (compiled into `faber`, never read from a checkout) and the Xcode. The disk is
   root:_faber_vm 0440, in a directory only root and `_faber_vm` enter, so the launcher
   reads it and cannot write it. An `.xip` must be your own file with one link; an `.app` is
   archived by your own account and only extracted by root, then walked and checked on root's copy.
4. Root clones your bundle into a build bundle owned by `_faber_vm`. **Your bundle is never
   booted and never changed**, so a failed build is retried with the same command.
5. The VM boots as `_faber_vm` with 4 CPUs and 8 GiB, the setup disk attached read-only and a
   console log. It is given four hours.
6. The console is judged: `wall-not-up` outranks everything, then `no-network`, then `failed step=`. Success
   needs the launcher to exit 0, `ok` as the last protocol line, and the facts that name the
   build. Only the exact lines of the protocol below are read, by strict patterns.
7. `manifest.json` is written beside the four files: the recipe, which `faber` wrote, and the
   installed half, which the guest reported (a record, not a proof). The existing golden
   import lands the bundle as `/var/db/faber/images/<name>`, `root:wheel` 0444. `--replace`
   replaces a different image of that name, as `setup --replace-golden` does.
8. The build bundle and root's staging are deleted, on failure too.

`--runtime` is `iOS`, `watchOS`, `tvOS` or `visionOS`, optionally `:<version>`, and repeats;
with none the build installs the matching iOS runtime. `--brew` adds Homebrew formulas to
`jq gh zstd git-lfs`. The Homebrew release is pinned in the binary (`HOMEBREW_TAG` and
`HOMEBREW_SHA256`, `src/setup/golden_build.rs`), never `HEAD`.

### What the guest does

At the next boot, which is the walled build's, the wrapper (`provision-wrapper.sh`) waits for
up to 300 s for exactly one read-only volume named `FABERSETUP` on media that is not the boot
disk. It mounts it read-only at `/private/var/faber/setup` and runs `provision.sh` from it as
root. The build puts three more things on that volume: `recipe.env`, the Xcode file, and the
same scripts. `provision.sh` is POSIX sh and Apple's tools only, and runs in this order:

1. **Software Update off**, before any network use: `defaults write
   /Library/Preferences/com.apple.SoftwareUpdate` `AutomaticDownload`,
   `CriticalUpdateInstall`, `ConfigDataInstall` and `AutomaticCheckEnabled` to false, and
   `com.apple.commerce AutoUpdate` to false. (`softwareupdate --schedule off` does not work on
   macOS 27.)
2. **The network service**: `networksetup -detectnewhardware`. `prepare` ran Setup Assistant
   with no NIC, so the build VM's NIC has no service and stays down until this creates the
   default (DHCP) one.
3. **The wall gate**, as the Linux build's `launcher/linux/provision.sh`: wait until DNS to
   the gateway goes unanswered three times in a row. If it has not within 120 s, report
   `wall-not-up` (or `no-network`, when the guest never had an address on the build network)
   and power off. Nothing is fetched before this.
4. **The recipe**, validated.
5. **Resolvers** to the wall's, never the gateway.
6. **The `runner` user**, non-admin, random password, automatic login (`/etc/kcpassword`).
7. **Xcode**, from the volume: `xip -x` and move into `/Applications/Xcode.app`, or copy an
   `.app`. `xcode-select`, licence accepted, `-runFirstLaunch`.
8. **Runtimes**: `xcodebuild -downloadPlatform <platform> [-buildVersion <version>]`.
9. **Homebrew**: Homebrew's installer refuses root and wants an admin, and `runner` is not
   one. So root creates `/opt/homebrew` owned by `runner`, unpacks the release tarball for
   `HOMEBREW_TAG` (fetched only after the wall gate, and refused unless its SHA-256 is
   `HOMEBREW_SHA256`), and runs `brew` as `runner`. Fixed formulas: `jq gh zstd git-lfs`,
   plus `BREW_EXTRA`.
10. **The bootstrap** (below).
11. **No way in**: every admin account gets a random password nobody stores, the shell
    `/usr/bin/false` and is hidden. Remote Login must be off, FileVault off, and no
    Apple Account, `authorized_keys` or `sudoers.d` entry may remain.
12. **Facts, clean-up, power off.** It reports `ok` and powers off. On any failure it reports
    `failed step=<name>` and powers off. On every exit path, `wall-not-up` included, it first
    removes the run-once daemon and wrapper from the build's clone, so no clone re-runs
    provisioning; the operator's own bundle stays armed for a retry. The wall gate counts
    unanswered DNS only once the guest has an address in `192.168.239.0/24` and its route to
    the gateway uses that interface.

### recipe.env

One `KEY=value` per line, no quoting, `#` comments. Every value is validated.

| Key | Meaning |
|---|---|
| `XCODE_KIND` | `xip` or `app` |
| `XCODE_FILE` | the Xcode file's name on the volume, `Xcode*.xip` or `Xcode*.app`; `[A-Za-z0-9._-]` only |
| `RUNTIMES` | space-separated `iOS`, `watchOS`, `tvOS`, `visionOS`, each optionally `<platform>:<version>`, such as `iOS iOS:18.5 watchOS` |
| `BREW_EXTRA` | space-separated extra formulas |
| `HOMEBREW_TAG` | the `Homebrew/brew` release tag, such as `4.6.0` |
| `HOMEBREW_SHA256` | the SHA-256 of that tag's tarball, 64 lowercase hex digits |

### Console protocol

`provision.sh` and its wrapper write to the virtio console port, which the guest sees as
`/dev/cu.virtio` (`VZVirtioConsoleDeviceSerialPortConfiguration`), as root. Root on the host
reads it, bounded, by exact line prefix. Every line starts `FABER-GOLDEN-BUILD `, is printable
ASCII, and is at most 200 bytes. These are all of them:

| Line | Meaning |
|---|---|
| `FABER-GOLDEN-BUILD wall-not-up` | the wall did not come up in 120 s; nothing was fetched. Outranks every other line |
| `FABER-GOLDEN-BUILD no-network` | the guest had no address on the build network in 120 s, so the wall could not be judged; nothing was fetched. Outranks every line but `wall-not-up` |
| `FABER-GOLDEN-BUILD step=<name>` | a step began: `sw-update`, `network`, `wall`, `recipe`, `resolvers`, `runner`, `xcode`, `runtimes`, `homebrew`, `bootstrap`, `lock-admins`, `checks`, `facts` |
| `FABER-GOLDEN-BUILD fact <key>=<value>` | a measured fact (below) |
| `FABER-GOLDEN-BUILD ok` | the build finished |
| `FABER-GOLDEN-BUILD failed step=<name>` | the named step failed (the wrapper also uses `setup-volume`) |

Fact keys: `macos_build`; `xcode_version`; `xcode_build`; `runtime` (value
`<platform> <version> <build>`, one line each); `brew` (value `<formula> <version>`, one line
each).

## The bootstrap

Two files `provision.sh` installs into the macOS golden image
(ADR-0026, *The golden image*). At every boot of a clone they mount the clone's read-only
tools disk and start the guest agent from it. The image holds neither the agent nor the
GitHub runner: both come with the tools disk, so a faber or runner release never means
importing the image again.

| File | Installed as | Owner, mode |
|---|---|---|
| `io.github.rejifald.faber.guest-agent.plist` | `/Library/LaunchDaemons/io.github.rejifald.faber.guest-agent.plist` | `root:wheel` 0644 |
| `bootstrap.sh` | `/Library/Application Support/faber-guest/bootstrap.sh` | `root:wheel` 0755, in a `root:wheel` 0755 directory |

What happens at boot:

1. launchd runs `bootstrap.sh` as root.
2. The script waits, for up to 180 s, for the tools disk. That is the HFS+ volume
   `FABERTOOLS` that faber's `hdiutil` stamps (`src/vm/tools_disk.rs`). It must be on
   read-only media that is not the boot disk, and exactly one such volume must exist.
3. It unmounts any automount and mounts the volume read-only, `nosuid,nodev,nobrowse`, at
   `/private/var/faber/tools`.
4. It runs `faber-agent serve` from there, as root. The agent listens on vsock port 6100
   ([docs/vm-agent-protocol.md](../../docs/vm-agent-protocol.md)).

If any step fails, or the agent exits with an error, launchd starts the script again after
10 s (`KeepAlive` with `SuccessfulExit` false). Its log is
`/private/var/log/faber-guest-agent.log`. The agent writes no JIT config there, or
anywhere else. The daemon is not loaded in the golden image itself: it has no tools disk, and
launchd loads it at the next boot of every clone anyway.

### What else the image needs

- **A non-admin `runner` user with GUI auto-login** (ADR-0026). The agent reports `ready`
  only when the console user is `runner`, with the uid `runner` has in the directory.
  `provision.sh` makes it.
- **Remote Login stays off.** Control runs over vsock alone. `provision.sh` fails the build
  if it is on, and the image holds no SSH key or stored password.
- **DNS goes to the wall's resolvers,** never the gateway: the launcher keeps vmnet's DNS
  proxy off. `provision.sh` sets the default pair, `1.1.1.1` and `1.0.0.1`.
- **No copy of the agent or the runner.** Both arrive on the tools disk.
