# faber-vm: the VM launcher

`faber-vm` boots faber's VMs on Apple's Virtualization framework:

- per-job macOS VMs, cloned from a golden image
- the long-lived Linux VM that runs Docker Engine

Its design is [ADR-0026](../docs/adrs/0026-job-code-runs-only-inside-vms-faber-owns.md),
§ *faber runs its VMs with a launcher it owns*. It is Swift and Apple's frameworks only,
with no package manager and no third-party code (the ADR's check 19).

It holds slice 1, the launcher core; slice 3, the VM supervisor (`supervise`, below);
slice 5, the macOS guest agent and its vsock control channel (*The guest agent*, below); and
slice 7, the Linux VM's guest agent and Docker bridge (*The Docker engine*, below).
Several pieces are other slices and are not here yet:

- the `_faber_vm` account and the supervisor's LaunchDaemon (setup, slice 6)
- the pf anchor
- the release asset

## The wall is built in

`run` and `validate` build exactly these devices:

- the bundle's disk (a Linux bundle also gets its EFI store and an optional read-only seed)
- a macOS clone's read-only tools disk (`--tools-disk`), holding the runner and the guest
  agent. It lives outside the bundle, and the launcher refuses one its own account owns or
  could write.
- one virtio NIC
- one vsock device
- entropy and a memory balloon
- a macOS guest's display, keyboard and trackpad
- optionally, a serial port. It writes into a pipe that the launcher drains into its log
  file, capped at `--console-log-max-mib` (default 64), so a guest cannot fill the host
  disk.

`checkWall` (`Sources/Wall.swift`) then holds the configuration to that list, by count and
by exact class, before the framework sees it. The launcher has no option that adds a
shared folder, a port forward, audio, USB, a console device or a second NIC. Its
entitlements are `com.apple.security.virtualization` alone, and `build.sh` fails if the
signed binary carries anything else. `com.apple.vm.networking`, the bridging
entitlement, is never in them.

**The network.**

- **vmnet (the default, macOS 26 and later).** Each VM gets a NAT network of its own,
  which `faber-vm` creates. The subnet is given (`--subnet`). NAT66, router
  advertisements and the gateway's DNS proxy are all off. The slot's MAC gets a DHCP
  reservation, by default at the subnet's third address.
- **`--network shared-nat`.** This is the ADR's fallback, Apple's shared NAT. Guests on
  it can reach each other, so the anchor alone must keep them apart.

Neither network has a user-mode NIC, so the host's `127.0.0.0/8` is unreachable from a
guest by construction.

**A fixed MAC per slot.** `--slot N` (0–15) fixes the NIC's MAC at `02:fa:be:00:00:NN`.
While the VM lives, `run` holds `--state-dir`/`slot-N.lock`, so two VMs never share a MAC.
The state directory must be the launcher account's own, mode 0700.

**Files are the launcher account's own.** The launcher refuses a bundle unless the bundle
and its parent belong to the account it runs as, with no group or other write, and it
refuses symlinked bundle files. The framework opens the files by path at start. So once
the VM runs, `run` checks every attached file's identity again. If one changed, it prints
`event=file-identity-changed` and force-stops, with exit code 4.

## Build and test

```sh
launcher/build.sh           # signed binary in $FABER_VM_OUT (default $TMPDIR/faber-vm)
launcher/build.sh --agent   # the guest agent, faber-agent, signed with no entitlements
launcher/test.sh            # unit tests; FABER_VM_TEST_VMNET=1 adds the vmnet ones
launcher/ci.sh              # build both, test, and validate --wall-only over a fixture, as CI does
```

The sources: `Sources/` is the launcher, `agent/` the guest agent, and `Shared/` the strict
codec and socket plumbing both compile (Foundation and Darwin only, never Virtualization).

`run`'s first line is `faber_vm=<version> protocol=<n>`, and `faber-vm protocol` prints
the child protocol (below) that `run` speaks to its supervisor. `--version` prints the same stamp as faber: `<Cargo.toml version>+g<commit>`, or
`$FABER_VERSION` when that is set.

## The supervisor

`faber-vm supervise` is the VM supervisor of
[docs/vm-supervisor-protocol.md](../docs/vm-supervisor-protocol.md), which is the spec; this
section says how the launcher implements it.

```text
faber-vm supervise --socket PATH --peer-uid N --state-dir DIR --image-dir DIR \
  --tools-dir DIR --clone-dir DIR (--network-plan PATH | --network shared-nat)
```

- **The socket.** Its directory must be this account's, with no group or other write. A
  stale socket this account owns is removed; anything else at the path is refused. The
  socket is created 0660. macOS gives it the directory's group, which is how setup (slice 6)
  lets `_faber` connect: it makes the directory `_faber_vm`'s with `_faber`'s group, mode
  0750. After every accept, `getpeereid(3)` must return `--peer-uid`, or the connection is
  closed before a byte is read or written.
- **The loop.** One thread and one kqueue: no VZ object, no lock between requests. At most
  four connections at once, each lockstep. A connection that sends nothing for 30 s, or
  never finishes a frame, is closed. Reads stop at MAX_FRAME (16 384 bytes); a frame with no
  LF by then, or a second frame sent before the first's reply, closes the connection
  without a reply.
- **`start`.** It takes the slot lock, then writes the claim marker `slot-<N>.claim`
  (`O_CREAT|O_EXCL|O_NOFOLLOW`, 0600, pid 0). A macOS image is cloned (`clone`, which writes
  the `faber-clone` marker) into `--clone-dir`; the Linux bundle boots in place. The child
  is `faber-vm run <bundle> --slot N --state-dir DIR --claim C --serve-socket
  DIR/slot-N.sock --slot-lock-fd 3 --cpus … --memory-mib … [--tools-disk … --tools-owner <peer uid>]` and the plan's
  network for slot N (`--network vmnet --subnet … --reserve-ip …`), or `--network
  shared-nat`. It is spawned with `posix_spawn`: its own process group, every descriptor
  closed but stdin (`/dev/null`), stdout, stderr and the slot lock as descriptor 3, every
  signal at its default, and a fixed environment. The supervisor hands the lock down rather
  than letting the child take it, so the slot is never free between the marker and the
  child: a supervisor that crashes there leaves a held lock, not a VM that looks gone. Then
  the marker is replaced by rename with the child's pid and `pid_started_us`
  (`kinfo_proc`'s `p_starttime`). The JIT config is never on argv or in the environment.
- **The network plan** is `/etc/faber/wall/plan.json`'s format, read once at start and
  strictly. Protocol slot N uses the plan's one slot with `id` N: its network's `subnet`
  and its `addr`. vmnet's gateway is always the subnet's second address, so a plan that
  names another gateway, or lacks the slot, fails that `start` with `start_failed`.
- **A child's exit** (waitpid for its own children, kqueue `NOTE_EXIT` for adopted ones):
  `requested` if faber asked; else `wall_violation` on exit 4; `agent_error` on exit 5;
  `runner_exited_without_job` on exit 6; `guest_shutdown` on exit 0;
  `start_failed` if it never reached `booting`; otherwise, and for any adopted child, whose
  exit status is unknowable, `launcher_exited`. Once the slot lock is free, a clone is
  deleted (only inside `--clone-dir`, carrying `faber-clone`, and named by no other live
  marker), the child's socket and the marker are removed, and the slot reports `stopped`
  until it is started again.
- **The supervisor owns the state machine.** A child runs as the account a VM escape lands
  in, so its word is checked. `status` may only move starting → booting → ready,
  in_service → job_finished, or any state → stopping; repeating the current state is no
  move. `probe` must answer `probe_sent` and `begin` `in_service`. `stopped` comes only from
  the supervisor seeing the child exit. Anything else keeps the last known state: `probe`
  and `begin` answer faber `internal`, and `status` is logged and ignored. At adoption a
  child may report any state but `stopped`; one that reports `stopped` is `unmanaged`, so its
  slot and clone are kept. `status` waits 500 ms for a child, then serves the last known
  state, so a wedged child cannot stall `list` (probe, begin and stop wait 2 s).
- **`stop`** asks the child over its socket. A child not serving yet (still checking the
  wall) gets SIGTERM instead, which does the same, but only a process proven to be that
  VM's child: one this supervisor spawned and has not reaped, or an adopted one whose start
  time still matches. An `unmanaged` VM is never stopped.
- **Adoption at start.** Each `slot-<N>.claim` with faber's signature is read. A free slot
  lock means the VM is gone: teardown, `stopped` with `launcher_exited`, once every marker
  has been loaded, so a clone is judged against all of them. A held one is
  adopted in its real state only if the pid is alive, this account's, started at the
  recorded microsecond, and its child answers the handshake at the marker's
  `child_protocol` with the marker's claim and slot. Anything less (pid 0, a reused pid, an
  unsupported version, a failed handshake) is `unmanaged`: the slot is held, nothing is
  signalled, and teardown follows when the lock comes free. `adopt` reports an `unmanaged`
  VM as found, in state `unmanaged`, since its marker and lock say it is still there. Then
  the sweep deletes every clone carrying `faber-clone` that no live marker names. While any
  marker cannot be read, no clone is deleted at all, by the sweep or by a teardown, since
  that marker might name any clone; the sweep runs once it is gone.
- **`exit`, SIGTERM and SIGINT** close and remove the socket and exit 0. No VM is stopped
  and no child is signalled.

`faber-vm supervisor-call --socket PATH [--no-hello] < frame` is a debugging client: it reads
one frame from stdin (at most MAX_FRAME bytes; never argv, so a `jit_config` in it does not
show in `ps`), prints the peer's uid, handshakes (unless `--no-hello`), sends the frame and
prints each reply. It never echoes what it was given.

## The child protocol

Each `run --serve-socket` child serves the supervisor on `<state-dir>/slot-N.sock`: mode
0600 in the 0700 state directory, refusing any peer but its own account. The framing and
the strictness are the supervisor protocol's. The supervisor opens one connection per
exchange: hello, one request, close. **Version 1:**

| Request | Reply |
|---|---|
| `{"hello":{"child_protocol":1}}` | `{"hello":{"child_protocol":1,"slot":N,"claim":C}}` |
| `{"status":{}}` | `{"status":{"state":S}}`; `stop_reason` only with `stopped`, which a v1 child never reports because it exits |
| `{"probe":{}}` | `{"status":{"state":"probe_sent"}}`, or `wrong_state` unless `ready` |
| `{"begin":{"jit_config":J}}` (`{}` for Linux) | `{"status":{"state":"in_service"}}`, or `wrong_state` unless `probe_sent` |
| `{"stop":{}}` | `{"done":{}}`: what SIGTERM does (macOS force-stop; Linux request, then force after 60 s) |

Errors are `{"error":{"code":C}}` with C one of `handshake_required`, `bad_request`,
`wrong_state`, or `version_mismatch` with `child_protocol`. The states are the wire's:
`starting` until the framework reports the VM running, then `booting`. A macOS child's guest
agent moves it on to `ready` and beyond (below); the Linux VM's is the launcher's own
(`Sources/LinuxGuest.swift`, *The Docker engine*, below). A `jit_config` is held in memory
only, until the agent takes it, and never logged.

The version is recorded in the child's claim marker and never changes for its life. A
supervisor adopts only versions in `supportedChildProtocols` (`Sources/Proto.swift`); a
release that drops one says so, and children on it finish as `unmanaged`.

## The guest agent

[docs/vm-agent-protocol.md](../docs/vm-agent-protocol.md) is the spec; this says where each
piece lives.

- **`faber-agent`** (`agent/`) is one binary with two modes. `serve` runs as root, started
  by the golden image's bootstrap (`golden/`) from the read-only tools disk. It listens on
  vsock port 6100, serves the host's connection only (peer CID 2), and answers `hello`,
  `status`, `probe` and `begin`. `runner-exec` runs as `runner`, from the LaunchAgent that
  `serve` bootstraps into `gui/<uid>` on `begin`. It takes the JIT config over a one-shot
  Unix socket that only `runner`'s uid may use, unpacks the runner tarball the tools disk's
  `manifest.json` names into `~/faber-runner`, and starts `run.sh` with the config in
  `ACTIONS_RUNNER_INPUT_JITCONFIG`. The config is never on an argv, in a file or in a log.
  `build.sh --agent` signs it with the hardened runtime and no entitlements, and fails if it
  carries any.
- **The launcher's side** (`Sources/Agent.swift`, `Sources/AgentVsock.swift`): a macOS
  `run --serve-socket` child connects to the agent with `VZVirtioSocketDevice.connect(toPort:)`
  once the VM runs, every second until `--agent-ready-timeout` (600 s by default). It polls
  `status` every second on a thread of its own, one exchange at a time, and moves the child
  `booting` → `ready` at the first `ready` and `in_service` → `job_finished` at `exited`.
  `probe` goes to the slot's gateway (on Apple's shared NAT, its default `192.168.64.1`),
  UDP port 9, three datagrams. Anything the agent sends that the spec does not allow, and
  every missed deadline, logs `event=agent-error what=<code>`, force-stops the VM and exits
  5, which the supervisor reports as `agent_error`. The Linux VM gets the launcher's own
  agent instead (*The Docker engine*, below).
- **The tools disk** is `<name>.img` in the supervisor's `--tools-dir`, faber's cache
  (`src/vm/tools_disk.rs`): an HFS+ volume `FABERTOOLS` holding `faber-agent`,
  `manifest.json` and `actions-runner-osx-arm64-<version>.tar.gz`. `_faber` builds the disks,
  so setup makes the directory `_faber`'s (755). The supervisor passes its `--peer-uid` to the
  child as `--tools-owner`, and the child accepts the disk's own directory as that account's;
  every directory above it must still be root's (ADR-0026, amended 2026-10-23).

## The builder-side boot check (the owner's)

CI's macOS runners cannot run VMs, so booting is checked by hand on the builder. Run
these as the operator, not as root. `_faber_vm` and the supervisor come with later
slices.

1. Build and run the tests, including the vmnet ones:

   ```sh
   launcher/build.sh && FABER_VM_TEST_VMNET=1 launcher/test.sh
   ```

2. Set a scratch directory for the clone and the slot locks. It must be on the same
   APFS volume as the golden image, because `clone` never falls back to a full copy.
   Set `GOLDEN` to the golden macOS bundle (`disk`, `vz-aux`, `vz-hwmodel`,
   `vz-identifier`; [golden/README.md](golden/README.md) builds one):

   ```sh
   export VMDIR="$HOME/faber-vm-check" GOLDEN="$VMDIR/golden" && mkdir -p -m 700 "$VMDIR" "$VMDIR/state"
   ```

3. Clone it, and check the clone's identifier and marker:

   ```sh
   "$TMPDIR/faber-vm/faber-vm" clone "$GOLDEN" "$VMDIR/clone-0" && "$TMPDIR/faber-vm/faber-vm" inspect "$VMDIR/clone-0"
   ```

4. Validate against the framework, on a per-VM vmnet network. The subnet is an example;
   use any private /24 the host does not route:

   ```sh
   "$TMPDIR/faber-vm/faber-vm" validate "$VMDIR/clone-0" --slot 0 --subnet 192.168.100.0/24
   ```

5. Boot it. Expect `wall=ok`, a `network mode=vmnet …` line showing `guest=192.168.100.2`,
   `ipv6=link-local-only` and `dns_proxy=off`, then `state=running`:

   ```sh
   "$TMPDIR/faber-vm/faber-vm" run "$VMDIR/clone-0" --slot 0 --state-dir "$VMDIR/state" --subnet 192.168.100.0/24 --console-log "$VMDIR/clone-0.log"
   ```

6. While it runs, check from a second terminal:
   - A second `run` with `--slot 0` is refused, because the slot lock is held.
   - A third macOS VM is refused with `VZErrorDomain 6`, if two are already running.
   - `ping -c1 192.168.100.2` answers once the guest is up, unless the guest's own
     firewall drops ICMP. That address is the slot's reservation.
7. Stop it with Ctrl-C. A macOS guest is force-stopped, which exits with 3. Then delete
   the clone:

   ```sh
   rm -rf "$VMDIR/clone-0" "$VMDIR/clone-0.log"
   ```

Repeat steps 4 and 5 for the Linux bundle (`disk.raw`) at `--slot 1` with a different
subnet. Its first `run` creates `efi-vars` and `machine-id`. Ctrl-C asks the guest to stop
and force-stops it after 60 s.

Report back:

- each step's last lines
- whether the guest got `192.168.100.2`
- whether `ifconfig` in the guest shows only a link-local IPv6 address

## The Linux VM image

`sudo faber setup` builds the Linux VM's disk itself (ADR-0026, § *The Linux VM runs Docker
Engine under the same launcher*). The code is `src/setup/linux_image.rs`; the guest files are
in `launcher/linux/` and are compiled into faber, never read from a checkout at run time.

- **The base** is Debian 13's `genericcloud` arm64 tarball, pinned in `linux_image.rs` by build
  (`BASE_BUILD`), URL and SHA-512. Setup refuses the download unless it matches the pin, and
  unless Debian's `SHA512SUMS` in the same directory lists the same digest for that file.
- **Provisioning** runs once, in a throwaway build VM: the launcher boots the base as
  `_faber_vm` on slot 15 and its own vmnet subnet, `192.168.239.0/24` (no job VM's), with a
  cloud-init seed (`seed.iso`). That network is in the wall's plan with no grant (#236):
  setup boots nothing unless the stored rules hold it and the checker's verdict is green, and
  `provision.sh` first waits, up to 120 s, until DNS to the gateway `192.168.239.1` goes
  unanswered (else it prints `FABER-LINUX-IMAGE wall-not-up` and powers off having fetched
  nothing). The guest cannot tell an anchor that drops its probe from a host that never
  answers one, so setup also proves the binding from the host (#243): while the build VM
  runs it reads the checker's verdict every 2 s, and kills the launcher if the verdict turns
  red, if network 16 is still in `pending_networks` 60 s after the launcher started, or if it
  goes back there; a build it never saw bound fails. Then `provision.sh` installs Docker Engine from
  `download.docker.com` (signed by the embedded key, fingerprint
  `9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88`), `ca-certificates`, the vsock relay
  (`faber-docker-relay`, which keeps half-closes), Cloudflare's resolvers, and removes every
  way in (no users, no SSH server, root locked). It prints `FABER-LINUX-IMAGE ok
  docker=<version>` or `FABER-LINUX-IMAGE failed step=<n>` on the console, and powers off.
- **The result** lands in `/var/db/faber/images/linux/`: `disk.raw` (64 GiB, sparse) and then
  its marker `image.json`, both `root:wheel` 0444, in directories `root:wheel` 0755. The
  marker records the base build and SHA-512, the digest of the guest files, the build time and
  Docker's version. Setup refuses a build VM disk longer than 64 GiB before copying it. A
  failed build leaves the previous image as it was, a failed landing included: the old
  marker and disk move aside first and come back if any step after fails (#230).
- **Out of date.** A release that bumps the pin, or changes a guest file, shows "Linux image
  out of date" (`/api/state` `budget.vm.images.linux` = `out_of_date`) until setup runs again.
  Setup skips the build with a warning while the launcher is not installed at
  `/var/db/faber/bin/faber-vm`, `_faber`'s like faber's own binaries, in a `_faber` 0755
  directory under directories only root can change. It is outside faber's state directory
  because `_faber_vm` runs it and cannot enter that directory (ADR-0025, amended 2026-10-18).

**Bumping the pin.** Pick a newer build directory under
`https://cloud.debian.org/images/cloud/trixie/`, and set `BASE_BUILD`, `BASE_URL`,
`BASE_SUMS_URL` and `BASE_SHA512` from that directory's `SHA512SUMS` line for
`debian-13-genericcloud-arm64-<build>.tar.xz`. The unit tests check the four agree.

### The builder check for the image (the owner's)

Run these yourself, from your own admin terminal, never through an agent: setup is a `sudo`
step (AGENTS.md).

1. See the plan. The `Linux VM image` section shows `will create` (or `skipped` while the
   launcher is not installed):

   ```sh
   sudo faber setup --dry-run
   ```

2. Build it (up to 30 minutes, after the daemon answers):

   ```sh
   sudo faber setup
   ```

3. Check the result: both files `root wheel`, `-r--r--r--`, and the marker's fields:

   ```sh
   ls -l /var/db/faber/images/linux/ && cat /var/db/faber/images/linux/image.json
   ```

4. The build VM's console log is removed with its bundle; setup prints the result line. Expect
   `Linux VM image built: Debian <build>, Docker <version>`.
5. `faber dashboard` shows no "Linux image out of date" note, and `/api/state` has
   `budget.vm.images.linux` = `current`.

Once slice 7 boots the long-lived Linux VM from this image, also check, through faber's Docker
socket:

- `docker version` answers, server included.
- `docker run --rm debian:13 echo ok` prints `ok`: the attached-output half-close test that
  `systemd-socket-proxyd` fails.
- `docker exec` into a running container prints its output.
- DNS: `getent hosts github.com` resolves in the guest, while a query to the vmnet gateway
  (`dig @<gateway> github.com`) is refused.

Report back each step's last lines.

## The Docker engine

faber's Docker jobs run in one long-lived Linux VM, the engine (ADR-0026, § *The Linux VM
runs Docker Engine under the same launcher*). faber starts it through the supervisor with
claim `linux-engine`, image `linux` and slot 14, and keeps it in service (`src/vm/engine.rs`).

- **Its bundle is `_faber_vm`'s own and long-lived.** With `--linux-dir`, a Linux `start`
  boots `<linux-dir>/<image>`. The first start creates it by `clonefile(2)` of
  `<image-dir>/<image>/disk.raw`, the root-owned image setup built, and never falls back to a
  copy. Once it exists it is never recreated, replaced or deleted: it holds the only copy of
  the local-only runner image. A newer base image from setup reaches it only when the operator
  moves to a new bundle. Without `--linux-dir`, a Linux bundle boots in place from
  `--image-dir`, as check 17 below does. Setup passes its image store,
  `/var/db/faber/images`, as `--image-dir`: the golden macOS bundles and `linux` sit side by
  side there, each at `<image-dir>/<name>`, and the request's kind decides how a name is read
  (a macOS clone refuses a source that is not a macOS bundle). The supervisor never lists
  `--image-dir`, so the store's `runner/` (`root:_faber` 0750, which `_faber_vm` cannot read)
  costs it nothing. `sudo faber uninstall` keeps `<vm_home>/linux` and everything in it.
- **Ready** is the guest's Docker answering. The child connects to vsock port 2375, the
  guest's relay, and sends `GET /_ping`. Only `HTTP/1.x 200` with the body `OK` counts, read
  through a 512-byte window.
- **The probe.** On `probe`, the child connects to vsock port 6101, where `faber-wall-probe`
  is socket-activated in the guest. It asks for 3 UDP datagrams to the slot's gateway, port 9.
  The launcher picks the target, never the guest. The reply must be exactly `sent 3`. faber
  then confirms from the anchor's own counters, as for a macOS clone.
- **The bridge exists only after `begin`.** The child then listens on `--docker-socket`. The
  supervisor passes its own `--docker-socket`, or `docker.sock` beside its own socket by
  default: `<vm_home>/run/docker.sock`. That socket has mode 0660 and the directory's group,
  `_faber`. Each connection must come from `--docker-peer-uid`, which the supervisor sets to its
  `--peer-uid`, or it is closed before a byte moves. Then the child opens a vsock connection
  to port 2375 and copies bytes both ways. An EOF on one side becomes `shutdown(SHUT_WR)` on the
  other, so an attached `docker run` or `docker exec` still gets its output after closing
  stdin. At most 64 connections at once. The socket is removed when the VM stops.
- **faber's side.** On macOS under the system layout, every `docker` faber runs gets
  `DOCKER_HOST=unix://<vm_home>/run/docker.sock` and no `DOCKER_CONTEXT`. Nothing points it at
  Docker Desktop.
- **Revival** (ADR-0007, retargeted) is `stop` then `start` of `linux-engine` through the
  supervisor, under the same cooldown, give-up and disk-room rules as before. faber never
  quits, kills or relaunches Docker Desktop.
- **Size.** `[linux_vm] cpus` (default 6) and `memory_mib` (default 16384). The VM keeps its
  memory while it runs, and it counts in the operator's machine share (ADR-0021) beside the
  macOS VMs.
- **DNS.** The guest and its containers use only Cloudflare's resolvers: `10-faber.network`,
  `faber-resolved.conf` and `/etc/docker/daemon.json` (`"dns"`).

### The builder checks for the engine (the owner's)

These are ADR-0026's checks 6, 8 and 9. They need setup's `_faber`, `_faber_vm`, the
supervisor's LaunchDaemon and the Linux image, so run them after `sudo faber setup`, from your
own admin terminal. `sudo` steps are yours, never an agent's (AGENTS.md). Set these first:

```sh
export RUN=/var/db/faber-vm/run DISK=/var/db/faber-vm/linux/linux/disk.raw
```

**Check 6: only `_faber` can use the Docker socket.**

1. Wait until faber reports the engine in service (the dashboard's VM section, or
   `/api/state`), then look at the directory and the socket. Expect `run` to be
   `_faber_vm _faber` with no access for others, and `docker.sock` to be `srw-rw----` with
   group `_faber`:

   ```sh
   sudo ls -ld "$RUN" "$RUN/docker.sock"
   ```

2. As `_faber`, Docker answers, server version included:

   ```sh
   sudo -u _faber curl -sS --unix-socket "$RUN/docker.sock" http://docker/version
   ```

3. As yourself, the path is unreachable (`Permission denied`):

   ```sh
   curl -sS --unix-socket "$RUN/docker.sock" http://docker/_ping
   ```

4. As `_faber_vm`, which can reach the path, the bridge closes the connection unanswered
   (curl reports an empty reply or a reset):

   ```sh
   sudo -u _faber_vm curl -sS --unix-socket "$RUN/docker.sock" http://docker/_ping
   ```

5. Also run the image section's socket checks above, through this socket as `_faber`:
   `docker version`, `docker run --rm debian:13 echo ok`, `docker exec`, and the DNS check.

**Check 8: throughput comparable to Docker Desktop's.** This needs a quiet CI window, because
it compares wall-clock times on a host with nothing else running. Drain faber first
(`POST /api/drain`, then wait until no claim in `/api/state` is `admitted`). Give Docker Desktop
the same CPUs and memory as `[linux_vm]` for the run. Then run the same work three times on
each daemon and keep the median:

1. A build with no cache, from a checkout of one of the watched repos' Dockerfiles. Run it
   once against Docker Desktop (your own `docker`) and once as `_faber` against the engine
   (`sudo -u _faber env DOCKER_HOST=unix://$RUN/docker.sock docker build --no-cache .`).
2. That repo's test command in the built image, timed the same way.
3. One real CI job of each kind the builder runs (a Docker build, a test job, a dind job if
   granted). Compare its duration on GitHub against the same job's last runs on Docker
   Desktop.

"Comparable" is within about 15 % of Docker Desktop's median. Report all the times, not only
the verdict.

**Check 9: space Docker frees in the VM returns to the host.** The framework documents no
discard, so this is measured, not assumed.

1. Note the host's free space and the disk file's allocated size:

   ```sh
   df -g /System/Volumes/Data && sudo du -h "$DISK"
   ```

2. As `_faber`, write 10 GiB inside a container that is removed afterwards. Then repeat step 1:
   `du` should have grown by about 10 GiB:

   ```sh
   sudo -u _faber env DOCKER_HOST=unix://$RUN/docker.sock docker run --rm debian:13 dd if=/dev/urandom of=/big bs=1M count=10240
   ```

3. Wait 5 minutes and repeat step 1. Then ask the guest to trim, and repeat step 1 once more:

   ```sh
   sudo -u _faber env DOCKER_HOST=unix://$RUN/docker.sock docker run --rm --privileged --pid=host debian:13 nsenter -t 1 -m -- fstrim -av
   ```

Report the three readings. If `du` shrinks after the trim but not before, faber needs a
periodic trim, so open an issue. If it never shrinks, the ADR's disk floor must count the
file's full size: say so on #76.

## The builder-side check 17 (the owner's)

ADR-0026's check 17: a VM outlives its supervisor, and a new supervisor takes it back by its
claim marker. Run these as the operator, never with sudo. `_faber_vm` and the supervisor's
LaunchDaemon come with slice 6, so here the supervisor runs as you, serving only you
(`--peer-uid "$(id -u)"`), on scratch directories. The VM is the Linux bundle, booted in
place: a macOS VM through the supervisor needs a tools disk your account does not own,
which setup provides in slice 6. The sweep is checked with macOS clones.

1. Build:

   ```sh
   launcher/build.sh
   ```

2. Set the paths, in both of the terminals the next steps use. A Unix socket path must fit
   in 103 bytes, and both the supervisor's socket and the children's (`slot-N.sock` in the
   state directory) live under `$SUP`, so keep `$VMDIR` short: `$HOME/faber-vm-check` is. `GOLDEN` is the golden
   macOS bundle and `LINUX` the Linux bundle from the boot check above; move them to these
   paths first if they live elsewhere. Everything must be on one APFS volume:

   ```sh
   export VMDIR="$HOME/faber-vm-check" FVM="$TMPDIR/faber-vm/faber-vm" && export GOLDEN="$VMDIR/golden" LINUX="$VMDIR/linux" SUP="$VMDIR/sup"
   ```

3. Make the supervisor's directories, put a clone of the Linux bundle in its image store,
   and write a network plan for slot 1:

   ```sh
   mkdir -p -m 700 "$SUP" "$SUP/state" "$SUP/images" "$SUP/tools" "$SUP/clones" && cp -c -R "$LINUX" "$SUP/images/linux-engine" && printf '%s\n' '{"networks":[{"id":1,"gateway":"192.168.100.1","subnet":"192.168.100.0/24","slots":[{"id":1,"addr":"192.168.100.2"}]}]}' > "$VMDIR/plan.json"
   ```

4. In terminal A, start the supervisor. Its log, and the VM's own output, land in this
   terminal; keep it open until step 11:

   ```sh
   "$FVM" supervise --socket "$SUP/s" --peer-uid "$(id -u)" --state-dir "$SUP/state" --image-dir "$SUP/images" --tools-dir "$SUP/tools" --clone-dir "$SUP/clones" --network-plan "$VMDIR/plan.json"
   ```

5. In terminal B, start the VM, then list until it shows `booting`, and note the child's pid
   and process group:

   ```sh
   echo '{"id":2,"request":{"start":{"slot":1,"kind":"linux","claim":"check-17","image":"linux-engine","cpus":2,"memory_mib":4096}}}' | "$FVM" supervisor-call --socket "$SUP/s"
   ```

   ```sh
   echo '{"id":2,"request":{"list":{}}}' | "$FVM" supervisor-call --socket "$SUP/s" && cat "$SUP/state/slot-1.claim" && pgrep -lf "faber-vm run" && ps -o pid,pgid,command -p "$(pgrep -f 'faber-vm run')"
   ```

6. Ask the supervisor to exit. Terminal A returns, with `exiting; 1 VM(s) left running`.
   The VM must still run, under the same pid:

   ```sh
   echo '{"id":3,"request":{"exit":{}}}' | "$FVM" supervisor-call --socket "$SUP/s" ; sleep 1 ; pgrep -lf "faber-vm run" && ping -c1 192.168.100.2
   ```

7. Start the supervisor again in terminal A (step 4). Its log must say `adopt slot 1:
   booting`, not `unmanaged`. In terminal B, list and adopt:

   ```sh
   echo '{"id":2,"request":{"list":{}}}' | "$FVM" supervisor-call --socket "$SUP/s" && echo '{"id":2,"request":{"adopt":{"claims":["check-17","not-a-vm"]}}}' | "$FVM" supervisor-call --socket "$SUP/s"
   ```

8. Kill the supervisor outright, check the VM again, then start the supervisor once more
   (step 4) and repeat step 7's list:

   ```sh
   pkill -KILL -f "faber-vm supervise" ; sleep 1 ; pgrep -lf "faber-vm run"
   ```

9. Stop the VM through the supervisor. The Linux guest is asked to stop and forced after
   60 s; list until it shows `stopped` with `stop_reason` `requested`:

   ```sh
   echo '{"id":2,"request":{"stop":{"claim":"check-17"}}}' | "$FVM" supervisor-call --socket "$SUP/s" && sleep 5 && echo '{"id":2,"request":{"list":{}}}' | "$FVM" supervisor-call --socket "$SUP/s"
   ```

10. The sweep. Ask the supervisor to exit (as in step 6), make one clone carrying faber's
    marker and one directory without it, start the supervisor again (step 4), and list the
    clone directory. Only `not-faber` may be left, and terminal A must say `deleted clone
    vm-orphan`:

    ```sh
    echo '{"id":3,"request":{"exit":{}}}' | "$FVM" supervisor-call --socket "$SUP/s" ; "$FVM" clone "$GOLDEN" "$SUP/clones/vm-orphan" && mkdir "$SUP/clones/not-faber"
    ```

    ```sh
    ls -la "$SUP/clones"
    ```

11. Clean up: ask the supervisor to exit (as in step 6), then:

    ```sh
    rm -rf "$SUP" "$VMDIR/plan.json"
    ```

Report back:

- terminal A's log from every supervisor run
- the reply lines of steps 5 to 9, and the `pgrep` and `ps` lines
- whether the `faber-vm run` pid was the same before and after each supervisor exit and kill
- whether `ping` answered in step 6, and the `ls` of step 10

## The faber-side check 17 (the owner's)

The rest of check 17: running VMs survive a restart of **faber**, which adopts them by their
claims, and faber's sweep stops only VMs of its own while the supervisor deletes only clones
that carry faber's marker. This needs a host set up by `sudo faber setup` (slice 6), with a
`macos-vm` profile, the wall green, and a watched repo (`owner/demo` here) with a job routed
to that profile. faber never deletes a VM file itself: it only asks the supervisor to stop
VMs. The `sudo` steps are yours; no agent runs them.

1. Set, in your shell, `PORT` to the control panel's port (8787 unless you changed it),
   `CLAIM` once step 2 shows it, and, from the supervisor's LaunchDaemon, `FVM` to its
   first `ProgramArguments` entry (the installed `faber-vm`) and `CLONES` to its
   `--clone-dir`:

   ```sh
   plutil -p /Library/LaunchDaemons/io.github.rejifald.faber.vm.plist
   ```

2. Start a job on `owner/demo` that the `macos-vm` profile serves. Wait until `/api/state`
   shows it at step `running`, and note its claim and slot, and the launcher child's pid:

   ```sh
   curl -s "localhost:$PORT/api/state" | jq '.vm_jobs' && pgrep -lf "faber-vm run"
   ```

3. Restart faber without draining. The VM must keep running under the same pid, and the
   job must not be cancelled on GitHub:

   ```sh
   sudo launchctl kickstart -k system/io.github.rejifald.faber ; sleep 10 ; pgrep -lf "faber-vm run"
   ```

   The daemon log (`/Library/Logs/faber/stderr.log`) must show, once, `vm adopt: <claim> in
   slot <n> is running job <id> — watching it` and `adopt: faber-<claim> — VM in slot <n>
   running job <id>`, and no `vm sweep: stopping` line for it. `/api/state` shows the same
   claim and slot at `running`, and the job in `.inflight`:

   ```sh
   curl -s "localhost:$PORT/api/state" | jq --argjson job "${CLAIM%%-*}" '.vm_jobs, (.inflight | map(select(.job_id == $job)))'
   ```

4. Let the job finish. It is green on GitHub, its outcome line is in the audit log, and
   `.vm_jobs` is empty again: faber stopped the VM and the supervisor deleted its clone.

5. Start another job and restart faber (step 3) while `.vm_jobs` shows it at `booting`. The
   log says `vm adopt: stopping <claim> … no job code ran yet`, the VM stops, and GitHub's
   job stays queued and is claimed again under a new claim.

6. The clone sweep. Plant, as `_faber_vm`, a directory without faber's marker, a
   Linux-shaped bundle that carries the marker, and an orphaned marked clone of the golden
   image (`GOLDEN` is its path in the image store). Then restart the supervisor, which sweeps
   at start, and list the clone directory:

   ```sh
   sudo -u _faber_vm sh -c 'mkdir "$0/not-faber" && touch "$0/not-faber/vz-hwmodel" && mkdir "$0/linux-shaped" && touch "$0/linux-shaped/disk.raw" && printf "org.faber.kind=vm-clone\n" > "$0/linux-shaped/faber-clone"' "$CLONES"
   ```

   ```sh
   sudo -u _faber_vm "$FVM" clone "$GOLDEN" "$CLONES/vm-orphan" && sudo launchctl kickstart -k system/io.github.rejifald.faber.vm ; sleep 5 ; sudo ls -la "$CLONES"
   ```

   Only `vm-orphan` is gone; `not-faber` and `linux-shaped` survive. The supervisor's log
   says `deleted clone vm-orphan` and nothing about the other two.

7. Remove what step 6 planted:

   ```sh
   sudo -u _faber_vm rm -rf "$CLONES/not-faber" "$CLONES/linux-shaped"
   ```

Report back:

- the `pgrep` lines before and after each restart, and the step 3 and step 5 log lines
- the `/api/state` output of steps 2 and 3
- the job's conclusion on GitHub, and the `ls` of step 6

## The builder-side check 15 (the owner's)

ADR-0026's check 15: faber controls a macOS guest over vsock alone, and no JIT config appears
on a host command line. Run these as the operator. The two `sudo` steps put the tools disk
where the launcher accepts it, owned by root and under directories your account does not
own; nothing else needs root. The supervisor runs as you, serving only you, as in check 17.

1. Build the launcher and the agent:

   ```sh
   launcher/build.sh && launcher/build.sh --agent
   ```

2. In both terminals, first export two values of your own: `RUNNER_VERSION`, the GitHub
   runner release to put on the tools disk (such as `2.330.0`), and `REPO`, the
   `owner/name` of a repository you can register a runner on, for the JIT config. Then set
   the paths. `GOLDEN` is the golden macOS bundle, with the bootstrap installed
   ([golden/README.md](golden/README.md)) and a `runner` user that logs in automatically:

   ```sh
   export VMDIR="$HOME/faber-vm-check" FVM="$TMPDIR/faber-vm/faber-vm" AGENT="$TMPDIR/faber-vm/faber-agent" && export GOLDEN="$VMDIR/golden" SUP="$VMDIR/sup" TOOLS="/Library/Application Support/faber-check15/tools"
   ```

3. Make a tools disk by hand, as faber's cache would: the agent, the runner tarball and the
   manifest, on an HFS+ volume named `FABERTOOLS`:

   ```sh
   mkdir -p "$VMDIR/tools-src" && cp "$AGENT" "$VMDIR/tools-src/faber-agent" && curl -fL -o "$VMDIR/tools-src/actions-runner-osx-arm64-$RUNNER_VERSION.tar.gz" "https://github.com/actions/runner/releases/download/v$RUNNER_VERSION/actions-runner-osx-arm64-$RUNNER_VERSION.tar.gz"
   ```

   ```sh
   printf '{"runner_version":"%s","runner_sha256":"%s","agent_version":"%s","agent_sha256":"%s"}\n' "$RUNNER_VERSION" "$(shasum -a 256 "$VMDIR/tools-src/actions-runner-osx-arm64-$RUNNER_VERSION.tar.gz" | cut -d' ' -f1)" "$("$AGENT" --version | cut -d' ' -f2)" "$(shasum -a 256 "$AGENT" | cut -d' ' -f1)" > "$VMDIR/tools-src/manifest.json" && hdiutil create -quiet -srcfolder "$VMDIR/tools-src" -volname FABERTOOLS -fs HFS+ -format UDTO -ov "$VMDIR/check15.cdr"
   ```

4. Give it to root, read-only, in a root-owned cache directory:

   ```sh
   sudo install -d -o root -g wheel -m 0755 "/Library/Application Support/faber-check15" "$TOOLS" && sudo install -o root -g wheel -m 0444 "$VMDIR/check15.cdr" "$TOOLS/check15.img"
   ```

5. Make the supervisor's directories, put a clone of the golden image in its image store,
   and write a network plan for slot 0:

   ```sh
   mkdir -p -m 700 "$SUP" "$SUP/state" "$SUP/images" "$SUP/clones" && cp -c -R "$GOLDEN" "$SUP/images/macos-27" && printf '%s\n' '{"networks":[{"id":0,"gateway":"192.168.100.1","subnet":"192.168.100.0/24","slots":[{"id":0,"addr":"192.168.100.2"}]}]}' > "$VMDIR/plan.json"
   ```

6. In terminal A, start the supervisor. Its log and the child's `event=` lines land here:

   ```sh
   "$FVM" supervise --socket "$SUP/s" --peer-uid "$(id -u)" --state-dir "$SUP/state" --image-dir "$SUP/images" --tools-dir "$TOOLS" --clone-dir "$SUP/clones" --network-plan "$VMDIR/plan.json"
   ```

7. In terminal B, start the VM, then list until it is `ready`. The agent reports `ready` once
   `runner`'s desktop session is up, about a minute after boot:

   ```sh
   echo '{"id":2,"request":{"start":{"slot":0,"kind":"macos","claim":"check-15","image":"macos-27","tools":"check15","cpus":4,"memory_mib":8192}}}' | "$FVM" supervisor-call --socket "$SUP/s"
   ```

   ```sh
   echo '{"id":2,"request":{"list":{}}}' | "$FVM" supervisor-call --socket "$SUP/s"
   ```

8. Probe. The reply is the VM in `probe_sent`:

   ```sh
   echo '{"id":2,"request":{"probe":{"claim":"check-15"}}}' | "$FVM" supervisor-call --socket "$SUP/s"
   ```

9. Get a JIT config into a file only you can read, never onto a command line, and begin.
   `printf` is the shell's builtin, so the frame is on no process's argv. Queue a job for
   the runner first: a `workflow_dispatch` workflow in `$REPO` with `runs-on: [self-hosted,
   faber-check15]` whose one step runs `id; echo "LANG=$LANG LC_ALL=$LC_ALL"; locale;
   launchctl managername`:

   ```sh
   (umask 077 && gh api -X POST "repos/$REPO/actions/runners/generate-jitconfig" -f name=faber-check15 -F runner_group_id=1 -f 'labels[]=self-hosted' -f 'labels[]=faber-check15' -f work_folder=_work --jq .encoded_jit_config > "$VMDIR/jit")
   ```

   ```sh
   printf '{"id":2,"request":{"begin":{"claim":"check-15","jit_config":"%s"}}}\n' "$(cat "$VMDIR/jit")" | "$FVM" supervisor-call --socket "$SUP/s"
   ```

10. While the job runs, look for the config on every host command line. `grep -F -f` reads
    the pattern from the file, so it is on no argv either. Expect `0`:

    ```sh
    ps -axww -o args | grep -cF -f "$VMDIR/jit"
    ```

11. List until the VM is `job_finished`. Terminal A shows `event=runner-exited exit_code=0`
    and no `event=agent-error`. The job's log shows the guest side: `uid=` of `runner`,
    `LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8`, and `Aqua` from `launchctl managername`, which
    is runner's GUI session.

12. Stop the VM and clean up. The tools disk is root's, so removing it takes `sudo`:

    ```sh
    echo '{"id":2,"request":{"stop":{"claim":"check-15"}}}' | "$FVM" supervisor-call --socket "$SUP/s" ; echo '{"id":3,"request":{"exit":{}}}' | "$FVM" supervisor-call --socket "$SUP/s" ; rm -rf "$SUP" "$VMDIR/plan.json" "$VMDIR/jit" "$VMDIR/tools-src" "$VMDIR/check15.cdr"
    ```

    ```sh
    sudo rm -rf "/Library/Application Support/faber-check15"
    ```

Report back:

- terminal A's log, the reply lines of steps 7 to 9 and 12, and step 10's count
- the job's log for its one step
- how long `start` took to reach `ready`
