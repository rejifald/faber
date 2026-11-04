---
title: Job code runs only inside VMs faber owns
type: adr
status: proposed
date: 2026-09-28
owner: rejifald
---

# ADR-0026: Job code runs only inside VMs faber owns

## Status

proposed (2026-09-28). It becomes accepted when the runtime test below passes on
the builder. **Checks 1–5, the macOS VMs, passed on 2026-10-10**
([#76](https://github.com/rejifald/faber-legacy/issues/76); with a spike tool, not faber's
supervisor, which needs a login session: see the 2026-10-25 amendment), and **check 16 passed on
2026-10-21** (ADR-0049 *Measurements*); the rest are pending.

**Revised 2026-10-10.** Two changes, both decided by the owner.

- **The runtime is a launcher faber owns, not Lima.** It runs on Apple's
  Virtualization framework, and its VMs get a NAT network and nothing else, plus a
  host pf anchor. The original decision needed a network mode in which a guest
  cannot reach the host's loopback. Lima has none, measured on a Linux guest
  (2026-09-28) and on a macOS guest (2026-10-09). See *Lima prepares; it does not
  run jobs*, below.
- **macOS jobs move into macOS VMs in the same release that removes the native
  backend.** They no longer leave faber for GitHub-hosted runners in between.
  #76 is therefore part of this release, not its later half.

A review of the revision, the same day, found three kinds of gap:

- The wall's pf half could fail open without anyone noticing.
- IPv6 and guest-to-guest traffic were uncovered.
- The control channel and the process model broke ADR-0025's account rules.

The decisions below close these gaps, and the runtime test gains the checks they
need.

**Amended 2026-10-17.** Before the build, the owner settled six questions the
revision had left open. Each answer is written into its section below.

1. **Which root binary holds the anchor:** the root-owned CLI copy, enforcing
   rules that setup stored.
2. **How the launcher updates without stopping VMs:** the supervisor restarts on
   the new binary at once, and running VMs are handed to it.
3. **Where the runner and the guest agent come from:** a read-only disk built
   per version and attached to each clone, not the golden image.
4. **Who builds the Linux VM's image:** `sudo faber setup`, from a pinned Debian
   cloud image.
5. **Which resolver guests use:** Cloudflare's, and nothing else unless the
   operator grants it.
6. **What a LAN or tailnet grant looks like:** a root-owned `grants.toml`,
   edited with `sudo faber grant`.

**Amended 2026-10-18** ([#236](https://github.com/rejifald/faber-legacy/issues/236)).
The throwaway VM that builds the Linux image runs under the wall, with the rules
of a job VM that has no grants. See *`sudo faber setup` builds its image*,
below.

It also settles where #107 and #108 fall. Both come after the cut, as
*Consequences* already says. For the cut, the Docker API exception holds on
its two conditions.

**Amended 2026-10-18** ([#259](https://github.com/rejifald/faber-legacy/issues/259)).
`_faber` reads the probe's counters from the verdict file. The root checker
already reads every anchor rule with its counters on each tick. On a green tick
it now writes them into the verdict file as well, with the networks bound at
that moment. So root writes and `_faber` only reads, on the verdict's own trust
terms, and nothing `_faber` sends reaches the root daemon. See *Each VM proves
the wall*, below.

**Amended 2026-10-21** by [ADR-0049](0049-faber-builds-the-golden-image-and-runs-only-a-verified-one.md).
faber builds the golden image itself: an offline windowed phase for Setup
Assistant, then a walled build as `_faber_vm`. Admission runs macOS jobs only on
an image that has passed verification. See *The golden image*, below.

**Amended 2026-10-21** (owner decision). The SYSTEM daemon on macOS runs a
pinned static Docker CLI that `sudo faber setup` installs root-owned at
`/usr/local/libexec/faber/docker`, and no other `docker`. For `docker build` it
also runs CLI plugins, but only from system plugin directories that are root's
alone, or from `_faber`'s own home. See *The daemon runs a Docker CLI faber
pins*, below.

**Amended 2026-10-25** by [ADR-0051](0051-macos-vms-start-inside-a-login-session.md).
macOS VMs do not start under the `_faber_vm` LaunchDaemon: on the builder every start fails
with "Failed to get current host key". They start under a supervisor that is a LaunchAgent
in the login session of a third account, `_faber_mac`; the Linux VM stays with
`_faber_vm`. Each host's VM layout is configurable, including turning the Linux VM off. The
"measured" boot under a role account in *The launcher* below came from the 2026-10-10 spike
tool and does not hold for faber's supervisor.

**Amended 2026-10-23** (owner decision, [#76](https://github.com/rejifald/faber-legacy/issues/76)).
The tools disks live in `/var/db/faber/tools-disks`, which setup makes `_faber`'s, because
`_faber` builds them. The launcher refused every disk there: it allowed a tools disk's
directories to be root's or its own only, so no macOS job could start. The supervisor now
passes its peer's uid (`--tools-owner`, `_faber`'s) to each child it starts with a tools
disk, and the launcher accepts the disk's own directory as that account's. Every other rule
stays: the disk is read-only, one link, never `_faber_vm`'s; no ancestor is group- or
other-writable; and every ancestor above the disk's directory is root's. Owning the directory,
`_faber` can race the check and the framework's open with a symlink, and so hand a guest a
file only `_faber_vm` can read, such as another VM's clone disk, read-only. It gains no reach
by that: it already writes the agent every guest runs. The alternative, a root-owned directory filled by a root helper at
job time, would add the runtime root process ADR-0025 avoids. See *The runner and the agent
arrive on a tools disk*, below.

This ADR builds the wall that ADR-0025's rule requires. It amends five ADRs, and
each gets its note when this one is accepted:

- ADR-0007: what faber revives
- ADR-0016: what faber may reclaim
- ADR-0021: what the machine share sizes
- ADR-0024: which volume the disk gate protects, and which jobs it gates
- ADR-0027: the steps of the upgrade sitting

## Context

ADR-0025 settles whose account each process runs as. What remains is where job
code runs. Today there are two answers, and both are on the wrong side of the
wall.

- **Native jobs run on the host itself.** Nothing but an account boundary would
  stand between a native job and faber. A macOS account with no login session
  also cannot give a job what native builds usually want: simulators, UI tests,
  keychain-based signing. The builder's native profile is in use: watched repos run their
  macOS builds on it.
- **Docker jobs run in Docker Desktop's VM.** Docker Desktop is a GUI
  application, so it needs a logged-in, auto-logged-in account, which ADR-0025
  rules out. Its VM serves bind mounts from the host's `/Users`, so a granted
  dind, which is root in that VM, reaches the files of whichever account runs
  Docker Desktop (#43, #72). Its host networking also made faber's own API
  reachable from an unprivileged container through `host.docker.internal` (#74,
  verified 2026-09-25).

Two earlier probes on the builder shaped this ADR.

- **2026-09-27, a Lima VM under a hidden role account.** A LaunchDaemon in the
  system domain started a Lima VM on Apple's Virtualization framework. The
  account had no login session, yet VZ's `VirtualMachine` XPC service ran as
  that account, macOS started the account's background agents on demand, and the
  VM reached `Running`.
- **2026-10-09 and 10, the #76 spike.** It answered the open questions with
  measurements. They appear below where they decide something.

## Decision

### faber runs its VMs with a launcher it owns

The launcher is a small helper, built in Swift against Apple's Virtualization
framework.

- **One process per VM, under `_faber_vm`** (ADR-0025). `_faber` cannot start a
  process as another account, so `sudo faber setup` installs a second
  LaunchDaemon. It runs the launcher as a supervisor under `_faber_vm`. faber
  asks it to start and stop VMs over a socket that only `_faber` may connect to.
  A role account with no login session booted a macOS guest this way in the
  2026-10-10 spike (about 57 s to its desktop), but faber's supervisor cannot:
  macOS VMs moved to a login session (ADR-0051).
- **VMs outlive a faber restart.** They belong to the supervisor, not to faber,
  so a restart or a self-update leaves running jobs alone.
  - At boot, faber adopts them by the claim each clone records, the way ADR-0011
    adopts containers.
  - The sweep removes only clones that carry faber's marker, never by path
    (AGENTS.md: read labels, never names).
  - **The supervisor updates without stopping a VM.** Each VM runs in its own
    launcher process. An atomic swap of the binary leaves running processes on
    the file they started from. So:
    1. The supervisor's LaunchDaemon sets `AbandonProcessGroup`, so its per-VM
       children outlive it.
    2. After a release swaps the launcher, faber checks the new binary first:
       its `--version` must match faber's, and `validate` must pass. A failure
       rolls back with the daemon, under ADR-0006's probation.
    3. faber asks the supervisor to exit, and launchd starts it again on the
       new binary.
    4. The new supervisor adopts the running per-VM processes by their claim
       markers, through a handshake that carries a protocol version. A child
       whose version it cannot speak is left to finish and is never stopped.

    New VMs start on the new launcher at once. Running ones finish on the old
    one.
- **Why Swift.** The framework's API is Swift and Objective-C first, and the
  helper must be a separate process under its own account in any case.
- **It ships as a release asset,** built, attested and verified like every
  other asset (ADR-0031).
  - It reports the same stamped `--version` as faber, so the follower's version
    check covers it too.
  - It is signed with the `com.apple.security.virtualization` entitlement:
    ad-hoc until faber is Developer ID signed.
- **`_faber_vm` cannot change what later VMs start from.**
  - The launcher binary belongs to `_faber`, like faber's own, so a release can
    update it. It lives in `/var/db/faber/bin`, not in `_faber`'s state
    directory, which `_faber_vm` cannot enter (ADR-0025, amended 2026-10-18).
  - The golden image belongs to root.
  - `_faber_vm` can read both and write neither, so a VM escape, which lands in
    `_faber_vm`, cannot alter them.

**The wall is built in, not configured.** The launcher builds exactly these
devices and refuses to start a VM with any other:

- **One disk, the VM's own image.** The Linux VM also has its EFI variable store.
  A macOS clone also gets its read-only tools disk (*The golden image*, below).
  The throwaway VM that builds the Linux image gets a read-only seed as well.
- **One network device,** attached to a NAT network (below).
- **One vsock device,** the control channel.
- **For a macOS guest, a graphics device, a keyboard and a trackpad,** which its
  desktop session needs.
- **An entropy device and a memory balloon.**
- **Optionally, a serial port** that writes to a log file the launcher owns.

There are no shared directories, no port forwards, and no audio, USB or
clipboard devices. With that configuration, the host's loopback-only services are
unreachable from a guest by construction: the host's IP stack drops traffic for
`127.0.0.0/8` that arrives on any other interface. That was measured: faber's own
API on `127.0.0.1` did not answer from inside a guest.

**Each VM gets a NAT network of its own,** where the launcher can create one.
On macOS 26 and later, vmnet's network API lets it set three things:

- **The VM's own IPv4 subnet.** The anchor reads it rather than assuming
  `192.168.64.0/24`, which is only what Apple's default happened to be.
- **No IPv6.** NAT66 and router advertisements, both on by default, are off.
- **A DHCP reservation for the VM's slot.**

Separate networks also stop guests from reaching each other directly on one
link. They do not, alone, stop one guest reaching another through the host's IP
forwarding (amended 2026-10-18, from #199's security review). The spike measured
no such traffic (check 12), but the guarantee is the anchor: every VM subnet is a
private range, and the anchor blocks private ranges from every VM interface. On Apple's
shared NAT, `vmnet.h` says guests can reach each other by default. Whether an
ad-hoc-signed launcher may create its own network is a runtime check: the
framework requires an entitlement, and its header does not say which. If the
launcher may not, VMs share Apple's NAT, and the anchor alone must block IPv6
and traffic between guests.

**NAT gives a guest the host's network position, so a host anchor finishes the
wall.** Measured from inside a guest on bare NAT, three things answered:

- services the host listens on for all interfaces, at the NAT gateway
- the LAN router
- the tailnet's resolver

So faber keeps a pf anchor loaded. It lives in the `com.apple/*` slot that
`/etc/pf.conf` already evaluates at boot. Its rules apply to the VMs' interfaces
only, and they deny by default:

- **Allowed:** DHCP on the VM's gateway, DNS to the listed public resolvers
  only (*DNS may leak names*, below), and public unicast IPv4.
- **Blocked:** everything else:
  - the host's own addresses
  - every private range: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`,
    `100.64.0.0/10` and `169.254.0.0/16`
  - multicast, and broadcast other than DHCP
  - all IPv6

The rules are rendered from the live interfaces and subnets. They are rendered
again when those change, or when the host's addresses do. They name addresses
explicitly: in the spike, the dynamic form `(self)` matched nothing.

The spike's rules covered IPv4 unicast only. Measured with them in place, the
host, the LAN and the tailnet were unreachable, while GitHub, App Store Connect,
the npm registry and the CocoaPods CDN all answered.

**DNS may leak names, so guests use a public resolver.** The gateway's DNS proxy
answers with the host's own view of names, including the tailnet's and the
LAN's. The spike measured this: port 53 on the gateway leaked tailnet names even
with vmnet's DNS proxy off.

- **The resolvers are Cloudflare's:** `1.1.1.1` and `1.0.0.1`. They are fast and
  filter nothing, so a CI guest never hits a legitimate host blocked by a threat
  feed.
- **There is no fallback by default.** The operator adds one, or replaces the
  pair, only as a grant (*LAN or tailnet access*, below), never in
  `config.toml`.
- **The anchor allows DNS only to the listed resolvers,** and stops DNS to the
  gateway and every other destination.

**The anchor is checked, not trusted.** pf can stop applying the anchor while VMs
keep booting:

- `pfctl -d` switches pf off for every holder of an enable reference.
- A main ruleset loaded without the `com.apple/*` anchor point stops evaluating
  it. `/etc/pf.conf` itself warns about this.

So the wall fails closed:

- **A root daemon holds the anchor.** `sudo faber setup` installs it, so root
  never runs a file `_faber` can write (ADR-0025).
  - **It runs the root-owned CLI copy,** as `/usr/local/bin/faber wall-daemon`.
    That is the one root binary, and ADR-0034's `sudo faber upgrade-cli` is its
    one verified way to change. `upgrade-cli` restarts the daemon on the new
    code. That is a sudo act by the operator, not a release.
  - **The rules are stored, not re-derived.** Setup, or `sudo faber grant`,
    renders the rules and stores them root-owned. The daemon enforces that
    stored set, never what its own code would render now. So new code alone
    never changes what a guest can reach.
  - Every few seconds it checks three things: pf is enabled, the main ruleset
    still has the anchor point, and the anchor holds exactly the rules setup
    installed.
  - If pf is off or the anchor is gone, it re-enables pf and reloads the anchor.
  - It never rewrites the main ruleset, which is the operator's.
  - `_faber` can read its verdict.
- **faber admits no job while that verdict is red.** Admission stops, and the
  dashboard shows a distinct state. Jobs already running finish.
- **Each VM proves the wall from inside before it gets work.** A macOS clone does
  so before it receives its JIT config, and the Linux VM before faber uses its
  Docker socket.
  - A probe sent from the guest must be stopped by the anchor itself, and the
    daemon confirms that from the rule's own counters.
  - A probe that merely goes unanswered proves nothing, because something else,
    such as the application firewall, could have dropped it.
  - `_faber` cannot read pf's counters, so the daemon publishes them. On each
    green tick it writes them into the verdict file, stamped with the load
    generation and with the moment it read them on the host's monotonic clock.
    faber takes one snapshot before it sends the probe and one read after, and
    judges "before" and "after" on that same clock, never the wall clock, so a
    clock that steps cannot pass off an earlier reading as a later one. Between
    the two, the slot's `host` rule must count at least as many packets as the
    probe sends (`PROBE_PACKETS`), under one load of one stored set, on a
    network that holds that slot alone. Anything less is unproven, and the VM
    gets no work. That includes a red or stale verdict, a reload, or a file
    with no counters.
  - What this proves is that the slot's block-to-host rule dropped at least
    that many packets from that address, not that they were the probe's own:
    any other traffic the guest sent to a host address in the window counts
    too. It is still evidence that the anchor drops traffic from that slot,
    which is the claim the proof makes.
  - A VM found already probed, with a probe that an earlier faber process
    sent, cannot be proven this way. It is stopped and started again.
- **No release can change the daemon or its rules.** Only a re-run of `sudo
  faber setup` can.
  - A release that needs new rules says so, and faber shows the anchor as out of
    date until setup runs.
  - Until then the old rules keep applying, which is AGENTS.md's shape for a
    tightening.

**LAN or tailnet access for a job is a grant,** made by the operator with `sudo`.
It is never made through `config.toml` or the API, which may narrow a grant but
never define one (ADR-0025).

- **Grants live in a root-owned `/etc/faber/grants.toml`** on both macOS and
  Linux, which `_faber` may read. `/etc/faber` is root's trusted directory and
  already holds `setup.json`. Nowhere under faber's state directory qualifies:
  `_faber` owns that directory, so it could rename any root-owned folder inside it
  and plant its own grants.
- **The operator edits it with `sudo faber grant add | list | remove`**, for
  example `sudo faber grant add --profile macos-vm --to 10.0.0.5/32 --port
  5432/tcp`.
  - Each change is validated before it reaches pf.
  - It re-renders and reloads the anchor at once, since it is a sudo act like
    setup.
  - It is written to the audit log.
- **A destination is a CIDR with a port and protocol, never a hostname.** pf
  matches addresses, and a name that resolves somewhere else tomorrow would
  quietly widen the wall. A tailnet host is granted by its `100.x` address.
- **A grant names a profile,** and only that profile's jobs get it.
  `config.toml` may drop a profile's grant, but never add one.
- **The same file holds `[dns] resolvers`,** the one place the default pair can
  be changed.

- **Absent, there is no access.** That withdraws what native and Docker Desktop
  jobs reach today, so it relies on the hard-cut exception that ADR-0027 records:
  every host is upgraded by hand. ADR-0013's shape, in which an absent key
  grants, does not apply here.
- **For macOS jobs, a grant can follow a profile,** since each macOS VM has its
  own address.
- **For Docker jobs, it covers the whole Linux VM,** because pf sees a single
  address for all of them.

**faber controls a guest over vsock, through its launcher.** Nothing about
control crosses the network.

- **A small agent listens on vsock.** It waits for `runner`'s desktop session,
  starts the JIT runner and reports its exit. It comes from the per-job tools
  disk (*The golden image*, below), not from the image itself.
- **The launcher talks to that agent and passes faber only a fixed set of
  states,** so `_faber` never parses guest-controlled input (ADR-0025).
- **The JIT config reaches the guest only over that channel.** It never appears
  on a host command line, where `ps` shows it to every account. It never crosses
  the network.
- **Remote Login is off in the golden image.**

The spike controlled guests over SSH on their NAT address, which caused three
problems:

- it put guest-controlled bytes in `_faber`'s parser
- it put the JIT config on a host command line
- every clone had the golden image's SSH host keys, so on a shared network one
  guest could pose as another

If a macOS guest cannot use vsock, the launcher runs the control session over
SSH instead, with the JIT config on the session's input. `_faber` still never
does.

**Size and recovery.**

- **The operator's declared machine share (ADR-0021) now covers every VM faber
  runs.** The Linux VM keeps the memory it takes, as Docker Desktop's VM does.
  The macOS VMs give theirs back when each job ends.
- **On a 32 GiB machine that keeps 16, say,** one macOS VM at 8 GiB leaves 8 GiB
  for the Linux VM.
- **Docker Desktop's VM is not in the share,** while the operator keeps it. It
  comes out of what the machine keeps, so it must shrink.
- **faber restarts a wedged VM** by asking the supervisor to stop and start it.
  That is ADR-0007's revival, retargeted at a VM faber created.

**Disk.** Every VM's disk is a file on the host volume, and a full host volume
fails the guests' writes. So ADR-0024's floor now protects that volume, and it
gates macOS VM jobs as well as Docker ones. The Linux VM's own disk is a second
volume Docker jobs fill, read from inside the VM; Docker's gates act on the
tighter of the two (ADR-0024's 2026-10-28 amendment, #410).

- **The Linux VM's disk is opened with full synchronization,** because it holds
  the only copy of the local-only runner image.
- **Per-job clones skip synchronization.** The framework says an unsynchronized
  image cannot safely be reused after a failure, and a clone is never reused.
- **Whether space that Docker frees inside the Linux VM returns to the host** is
  a runtime check. The framework's headers document no discard.

### The Linux VM runs Docker Engine under the same launcher

One long-lived Linux VM per host runs Docker Engine, as originally decided.

- **Boot and wall.** It boots by EFI under the same launcher, behind the same
  wall, with no shares.
- **The Docker socket.** It reaches `_faber` over vsock. The launcher bridges
  the guest's Docker socket to a host socket that only `_faber` may use, across
  the account boundary. `DOCKER_HOST` points at that socket.
- **The Docker backend's contract** does not otherwise change. Its JIT config
  moves off the `docker run` command line in the same release
  ([#104](https://github.com/rejifald/faber-legacy/issues/104)).

**`sudo faber setup` builds its image.** No hand step is needed.

- **The base is Debian 13's `genericcloud` arm64 image,** pinned by digest in
  faber's source and checked against Debian's published checksum.
- **Setup installs everything in a throwaway build VM** before any job boots:
  Docker Engine, `ca-certificates`, the guest's Docker relay and the resolver
  configuration.
  - `ca-certificates` is required: faber's TLS uses the OS trust store (#170).
  - The relay must keep half-closes. `systemd-socket-proxyd` drops them, which
    the spike measured.
  - Installing at build time means every boot finds Docker ready. Installing on
    first boot took minutes.
- **The build VM runs under the wall, as a job VM with no grants**
  ([#236](https://github.com/rejifald/faber-legacy/issues/236)). A signed package can
  still run maintainer scripts as root and reach whatever the VM can reach. An
  exception that only setup uses is one that nothing checks stays narrow.
  - **Plan.** The build network (`192.168.239.0/24`, gateway `.1`, guest `.2`,
    launcher slot 15) is one more network in the plan setup writes, and its slot
    lists no profile. So no grant, and no `drop_grants`, can ever apply to it.
    It renders as every other slot does: DHCP to the gateway, DNS to the listed
    resolvers only, the fence, non-global ranges dropped, public unicast
    passed. The host, the LAN and the tailnet are refused.
  - **Before boot.** Setup reads the stored rules root-only. It refuses to
    build, says why, and boots nothing unless they hold the build network in
    exactly that no-grant shape and the checker's verdict is green for that
    stored set. It asks before downloading and again just before boot.
  - **The boot race.** A network's rules bind only once its bridge exists,
    which is after the VM has started. So the guest's provisioning first waits
    until DNS to its gateway goes unanswered, three times in a row. The
    anchor's first per-slot rule drops everything to the host, the gateway
    included, and the anchor loads as a unit, so an unanswered gateway shows
    the whole rule set is bound. If that does not happen within 120 seconds,
    the guest reports it on the console and powers off, and setup says the
    wall was not up. Provisioning fetches nothing before the wall is up.
  - **Rejected:** keeping the build VM outside the anchor and recording why
    here, which leaves an unchecked exception in the wall; and interface-free,
    source-keyed rules loaded before boot, which also apply on other
    interfaces and break the checker's line-by-line comparison.
- **The result belongs to root,** beside the macOS golden image, so `_faber_vm`
  can read it and never write it.
- **A release that bumps the pinned digest** shows the image as out of date
  until setup runs again, the same as the anchor. A release cannot change the
  image itself.

A granted dind (ADR-0013) now makes a job root inside a VM that holds nothing of
the host's. The grant stays a grant: it still costs memory, and it is still the
widest privilege a job can have.

Its blast radius stops at the VM. That VM is long-lived and shared by every
Docker job, though, so a dind job can still reach later Docker jobs, for example
by rewriting the runner image. Docker Desktop's VM had the same exposure. The
per-job macOS VMs do not.

### macOS jobs run in per-job macOS VMs, from the cut on

The release that ships these ADRs removes the native backend. A profile with
`backend = "native"` then becomes a warning at every boot and is otherwise
ignored; it is not a boot failure (AGENTS.md). The same release ships
`backend = "macos-vm"`.

There is no GitHub-hosted interim. On the Free plan, private repositories get
2,000 included minutes a month. GitHub prices a macOS minute at about ten times a
Linux one: $0.062 against $0.006, checked 2026-10-10. A few iOS builds a day
would use up the allowance even if a macOS minute counted as one.

**The golden image.** ADR-0049 says how faber builds and verifies it. The
operator prepares it once:

- **macOS:** from Apple's restore image, new enough for the jobs' Xcode. The
  builder's Xcode 27 needs macOS 26.6 or later. Lima's stock template installs
  26.5.1, so Apple's catalog was used instead, giving 27.0.1.
- **Xcode:** copied from the host, which needs no Apple ID. The iOS simulator
  runtime is added.
- **Release-job tools:** Homebrew's `ruby` and `cocoapods`.
- **A small root-owned bootstrap.** At boot it mounts the clone's read-only tools
  disk and starts the guest agent from it. The image holds neither the GitHub
  runner nor the agent, so a runner or faber release never means a re-import.
- **A non-admin `runner` user with GUI auto-login.** On macOS 27,
  `sysadminctl -autologin` fails over SSH with error 22, so setup writes
  `/etc/kcpassword` itself.
- **DNS: the listed public resolvers, never the gateway.** The launcher keeps
  vmnet's DNS proxy off. Guests resolve through Cloudflare (see *DNS may leak
  names*), and the anchor allows DNS to those resolvers only.
- **No way in.** Remote Login is off. The preparing tool's SSH key and stored
  password are removed: Lima, for one, leaves both in the guest admin's home.

The operator imports the image with `sudo`, and from then on it belongs to root.
It is a local-only artifact: never published, and never deleted by faber. That is
AGENTS.md's rule for the local-only runner image, unchanged. A tool such as Lima
may prepare it, but no such tool is on the run path.

**The runner and the agent arrive on a tools disk.** faber builds one read-only
disk image per pair of runner version and agent version, caches it, and attaches
it to each clone as a second disk.

- **The agent** comes from the verified release, like every other asset
  (ADR-0031).
- **The runner** is the version the job pins, the same choice ADR-0008 makes for
  runner images. It is checked against the SHA-256 that GitHub publishes for that
  runner release.
- **`_faber_vm` can read the cached disks and never write them,** so a VM escape
  still cannot change what later VMs start from. The disks' directory is `_faber`'s, the
  account that builds them; the launcher accepts it only as the supervisor's `--tools-owner`
  (amended 2026-10-23).
- **The wall's device list gains this one disk,** read-only, on macOS guests.

**Per job:**

1. **Clone the golden image** with `clonefile(2)`. That takes under 1 ms, and
   the clone shares the golden image's blocks until the guest writes.
2. **Give the clone a fresh machine identifier.** This boots, measured, even
   though the auxiliary storage is personalized to the golden image's
   identifier, so two concurrent VMs never share an identifier.
   - It is outside the framework's documented contract. The header says a VM
     that is loaded again must keep its original identifier and auxiliary
     storage, and that VMs made from one configuration should each have their
     own.
   - It is therefore re-tested after every macOS update.
   - The fallback is one golden image per concurrency slot, each booting with its
     own original identifier.
3. **Use a fixed MAC per concurrency slot,** so jobs cannot exhaust the NAT's
   DHCP pool. The supervisor holds a slot for as long as its VM lives, so no two
   VMs ever share a MAC.
4. **Boot under `_faber_vm`.** In the spike, SSH answered after 13 s for one
   VM, and after 23–26 s for two booting at once.
5. **Wait until the console user is `runner`.** The guest agent reports it. A
   guest can still be at the login window when it first answers.
6. **Prove the wall from inside the guest** (see *The anchor is checked, not
   trusted*).
7. **Start the JIT runner** as a LaunchAgent in `runner`'s `gui/<uid>` domain,
   with its config delivered over vsock and a UTF-8 locale. Measured in a real
   job: it ran as `runner`, without admin rights or `sudo`, and an iOS simulator
   booted in the job's own session.
8. **Run one job,** then force-stop the VM and delete the clone. `requestStop()`
   meets the logged-in session's shutdown dialog, so a graceful stop would only
   wait out its grace period.

**Admission** gets a macOS class capped at two per host. That is Apple's limit:
the macOS licence allows two additional copies in virtual machines, and the
framework enforces it by refusing a third VM with `VZErrorDomain 6`. The
framework's error describes a limit on running VMs in general, but it counts
macOS VMs only: two ran beside two Linux VMs, and only a third macOS VM was
refused (measured 2026-10-21, ADR-0049 *Measurements*).

**Resources.** Each VM takes about 4 vCPUs and 6–8 GiB from the machine share.
The golden image uses about 40 GB of disk, and a clone grows by what its job
writes.

A job gives up `sudo`, because `runner` is not an admin. The native workflows
that run on the builder today are written to work without it.

**The upgrade sitting (ADR-0027) gains four steps.** They come after `sudo
faber setup` and before `sudo faber auth set`, because a host starts claiming
jobs as soon as it holds a token:

1. Import the golden image. Setup builds the Linux VM's image itself.
2. Load the runner image into faber's Linux VM, keeping the tarball (below).
3. Re-create any LAN or tailnet access the host's jobs need, with `sudo faber
   grant add`. Absent, there is none.
4. Switch the native profile to `backend = "macos-vm"`, keeping its labels, so
   workflows do not change.

### Lima prepares; it does not run jobs

**Lima cannot hold the wall.** Its vz instances always attach Lima's user-mode
NIC. Through that NIC's gateway (`192.168.5.2`, or `host.lima.internal`), a guest
reaches whatever listens on the host's `127.0.0.1`.

- That was measured on a Linux guest (2026-09-28) and on a macOS guest
  (2026-10-09). On the macOS guest, faber's own API answered.
- Lima's `networks:` setting only adds NICs beside that one.
- `ssh.overVsock` is off for macOS guests.

Colima is a layer over Lima and inherits the same NIC.

Lima therefore stays off the run path. It remains a convenient way to prepare
the golden macOS image, and its VZ bundle files boot unchanged under faber's
launcher: `disk`, `vz-aux`, `vz-hwmodel` and `vz-identifier`.

### Docker Desktop stops being faber's

faber stops using Docker Desktop; it does not remove it. The operator's own
containers stay where they are, and whether to keep Docker Desktop is the
operator's call. Three consequences follow:

- **ADR-0016's delegated reclamation leaves faber's scope.** Its grants name the
  operator's compose projects and volumes, which live in Docker Desktop, not in
  faber's VM. faber's disk duties become its own VMs plus the host floor of
  ADR-0024.
- **The runner image moves first.** The local-only runner image exists only
  inside Docker Desktop's VM. The operator exports it (`docker save`) and setup
  keeps the archive; the daemon loads it into faber's VM (`docker load`, through
  the VM's socket) whenever that VM comes into service and lacks it, before faber
  stops using Docker Desktop. While it loads, Docker jobs wait, and an image
  with no kept backup shows in `/api/state`, the dashboard and the tray
  (`src/vm/runner_image.rs`).
  - The tarball is kept as the image's backup, owned by root, since the image's
    only other copy is now one VM disk.
  - The never-reap rule in AGENTS.md applies to both, unchanged.
- **Up to four VMs may run** while the operator keeps Docker Desktop: its own,
  faber's Linux VM, and two macOS VMs. *Size and recovery*, above, says how the
  memory divides.

### The daemon runs a Docker CLI faber pins

*Added 2026-10-21.* Every Docker call the daemon makes is a `docker` child
pointed at the Linux VM's socket. On a set-up host the daemon's `PATH` is
`/usr/bin:/bin:/usr/sbin:/sbin`, deliberately: it holds nothing an operator can
write (ADR-0025). The only `docker` a Mac usually has is Docker Desktop's, a
symlink in `/usr/local/bin` into `/Applications/Docker.app`. That file is the
operator's to write, and Docker Desktop is not faber's (above). So, measured on
the builder after setup, every Docker call failed with "No such file or
directory".

- **Setup installs one.** `sudo faber setup` installs Docker's static macOS CLI
  at `/usr/local/libexec/faber/docker`, `root:wheel` 0755. Every directory above
  it must be root's and writable by nobody else, or the plan refuses, the same
  rule as for the CLI copy in `/usr/local/bin`. It is under `libexec`, so it is
  not on the operator's `PATH` and never shadows their own `docker`. `sudo faber
  uninstall` removes it, and its directory when nothing else is in it.
- **The daemon runs it by absolute path**, and no other `docker`, on the SYSTEM
  layout on macOS. The daemon's `PATH` does not change. A per-user (LEGACY)
  faber, and Linux, keep `docker` from `PATH`. If the file is missing or is not
  the pinned build, the daemon says so at boot and on the dashboard.
- **Plugins come only from root's directories, or from `_faber`'s home.** For
  `docker build`, the CLI runs a plugin (buildx). It looks in its config
  directory, `_faber`'s own `~/.docker/cli-plugins`, and in docker/cli's fixed
  system directories: `/usr/local/lib/docker/cli-plugins`,
  `/usr/local/libexec/docker/cli-plugins`, `/usr/lib/docker/cli-plugins` and
  `/usr/libexec/docker/cli-plugins` (`defaultSystemPluginDirs` in
  `cli-plugins/manager/manager_unix.go`, and the only plugin paths in the pinned
  binary). None exists on a stock Mac. All four are judged, present or not.
  Every directory on the way to each must be root's alone, as far as it exists,
  so that no other account can create the directory later. A present one may
  hold only regular files that root owns, that nobody else can write, and that
  have one link. A symlink is refused wherever it points: Docker Desktop's
  system install puts symlinks into `/Applications/Docker.app` there. Setup
  refuses and names the directory or entry: remove it, or make it root's. The
  daemon checks the same rule at boot and at most once a minute before the
  commands that load plugins: `docker build` (runner-image variants) and
  `docker builder prune` (docker/cli's `processBuilder`). While the rule
  fails, it refuses those two commands with a warning and a dashboard notice.
  Every other Docker call goes on. Unknown subcommands (`compose`, `buildx`)
  and help load plugins too, and faber runs neither. Hooks run plugins only
  when stdout is a terminal.
- **Apple silicon only.** The pin is the `aarch64` build. On another Mac the
  plan step refuses with that reason, rather than install a binary that cannot
  run.
- **The pin is a source change.** One constant
  (`src/setup/docker_cli.rs`, `PIN`) names the release, its URL on
  `download.docker.com`, the archive's SHA-256 and size, and the SHA-256 and
  size of the one member kept, `docker/docker`. It starts at 29.9.2, the version
  of the Linux VM's Docker Engine. It is bumped by a reviewed change, like the
  Debian base, and an ignored test downloads and checks the new pin end to end.
- **The SHA-256 is the trust anchor.** Docker signs this binary ad hoc only, with
  no team ID, so no signature says who built it. Setup downloads the archive
  over HTTPS only. It refuses anything longer than the pinned size, and checks
  the archive's SHA-256 before anything reads it. It then keeps exactly the one
  member, with no links and no absolute or `..` paths, and checks that
  binary's own SHA-256. Root never runs the binary. Its staged copy's
  `--version` runs as `_faber`, as the launcher's does, and only then is it
  renamed into place. A re-run with the pinned binary in place plans nothing.

### Linux hosts

The launcher is macOS-only. On a Linux host, a VM runtime would need its own
wall, and Lima's QEMU user-mode network has the same loopback mapping. That
runtime is a later decision. Until then ADR-0025's rule stands: a Linux host that
holds a credential refuses dind. Rootless Docker is a possible later wall for
such hosts; it is not decided here.

### The runtime test

This ADR is accepted when tests on the builder show all of the following.

**macOS VMs: passed 2026-10-10** (#76, experiments 1–5, with a spike tool; faber's own
macOS VMs start in a login session, per the 2026-10-25 amendment and ADR-0051):

1. A golden image with Xcode, the iOS runtime and a non-admin auto-login
   `runner` user is prepared.
2. A clone boots with a fresh machine identifier, and two run at once.
3. The wall holds from inside a guest over IPv4 unicast. No host folder is
   visible, and the host, LAN and tailnet are unreachable, while the internet
   works.
4. A real Actions job runs inside the guest as `runner`, without `sudo`,
   including an iOS simulator.
5. A clone boots under a hidden role account from a LaunchDaemon, and its guest
   reaches its own desktop session.

**The Linux VM: pending:**

6. `_faber` can use the Docker socket that the launcher forwards over vsock from
   `_faber_vm`'s VM, across the account boundary, and no other account can.
7. The same wall holds from inside the Linux VM.
8. Docker build and test throughput is comparable to Docker Desktop's on the same
   host.
9. Space that Docker frees inside the Linux VM returns to the host volume.

**The wall, for both kinds of VM: pending:**

10. It fails closed. faber admits no job, and a clone's probe is reported as not
    stopped by the anchor, in each of three cases: pf disabled, the anchor point
    removed, or the anchor's rules changed.
11. On whichever network the launcher uses, no guest reaches the host, a
    private range or another guest over IPv6, multicast or broadcast.
12. On whichever network the launcher uses, no guest reaches another guest at
    all.
13. DNS from a guest reaches only the listed resolvers: port 53 to the gateway,
    or to any other address, is blocked by the anchor.
14. The anchor survives a reboot and a change of the host's addresses.

**Operation: pending, except 16:**

15. faber controls a macOS guest over vsock alone, and no JIT config appears on
    a host command line.
16. Two macOS VMs and the Linux VM run at once. **Passed 2026-10-21** with a
    test tool on the same framework (ADR-0049 *Measurements*); the limit
    counts macOS VMs only.
17. Running VMs survive a faber restart and are adopted, and the sweep removes
    only clones that carry faber's marker.
18. Both iOS workflows that the builder runs natively today pass in a macOS VM.
19. The launcher has no third-party dependency: Swift and Apple frameworks only.

## Consequences

- No job code runs on the host, under any account.
- faber can run with nobody logged in, which it could not while Docker Desktop
  was its runtime.
- **The release carries a runtime migration for live CI,** alongside ADR-0025's
  accounts and ADR-0027's credentials. That is its largest risk, which is why
  this ADR is second in the build order: it gets the longest time running on the
  builder before the release.
- **macOS jobs stay on faber through the cut,** in VMs, at most two at a time per
  Mac.
- **faber ships a second binary,** the launcher. It is covered by the same
  release verification as faber itself.
- **faber owns host firewall state for the first time.** Setup installs two more
  LaunchDaemons: the anchor's, which runs as root, and the supervisor's, which
  runs as `_faber_vm`. Uninstall removes them, and only a re-run of setup changes
  the anchor.
- **One guest-controlled input still reaches `_faber`: the Docker API.** faber
  speaks it to the daemon in the Linux VM, and a granted dind is root there.
  ADR-0025's parsing rule holds for the VMs' own traffic. For the Docker API it
  holds only on hosts that grant no dind. The owner accepted that as the rule's
  one exception for the cut
  ([#105](https://github.com/rejifald/faber-legacy/issues/105)):
  - faber treats the daemon's output as untrusted (#112)
  - only jobs that ask for `[faber, dind]` get a daemon
  - after the cut, [#107](https://github.com/rejifald/faber-legacy/issues/107) closes
    the exception by moving Docker calls under `_faber_vm`, and
    [#108](https://github.com/rejifald/faber-legacy/issues/108) gives each dind job a VM
    of its own
- AGENTS.md's "faber owns the Docker host" section describes Docker Desktop; it
  changes with the implementation.

## Alternatives considered

**Lima at run time.** Its user-mode NIC maps the host's loopback into every
guest, and the NIC cannot be removed. See above.

**A firewall inside the guest.** This would keep Lima. But the wall would live
inside the VM, so any route to root in the guest removes it, and jobs that need
`sudo` could not run at all.

**Patch Lima upstream** to run vz instances without the user-mode NIC. That keeps
Lima and gets a real wall, but the timeline is the Lima maintainers'.

**Write the launcher in Rust, through Objective-C bindings.** That keeps one
language in the repository. But Virtualization's API is Swift and Objective-C
first, the bindings add an unsafe layer over it, and the launcher is a separate
process under its own account either way.

**SSH on the NAT address as the control channel,** as in the spike. It needs no
guest agent. But `_faber` would parse guest-controlled input, the JIT config
would sit on a host command line, and every clone would share the golden image's
host keys.

**GitHub-hosted macOS runners until macOS VMs land.** This was the previous plan.
On Free, a few iOS builds a day exhaust the 2,000 included minutes, and GitHub
prices macOS minutes at about ten times Linux ones.

**Keep Docker Desktop.** It needs a visible account with auto-login, shares that
account's home with the VM, and exposes the host's loopback: everything ADR-0025
set out to remove.

**Colima.** It inherits Lima's NIC; see above.

**Other desktop container runtimes.** The ones considered are GUI applications,
with the same login-session requirement.

**Apple's `container` tool**, which runs each container in its own VM. That is
the deepest answer to #43. It is not Docker-compatible, though, so adopting it
means rewriting the Docker backend and every job that uses Docker, not just
swapping the runtime.

**Keep the native backend under a hidden account.** Command-line builds would
keep working; anything needing a desktop session would not. It keeps job code on
the host behind nothing but an account boundary, while macOS VMs reach the same
builds without that.
