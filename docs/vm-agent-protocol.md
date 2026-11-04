# The guest agent protocol

The wire between a per-VM launcher child (`faber-vm run --serve-socket`, running
as `_faber_vm`) and the guest agent inside a macOS clone (ADR-0026, *faber
controls a guest over vsock, through its launcher*). The launcher asks; the agent
answers.

**Protocol version: 3.** Version 3 added `job` to `exited`: whether the runner
was ever handed a job ([The runner's exit](#the-runners-exit), #392). Version 2
added `verify`, the phases `verifying` and `verified`, and `failed` (ADR-0049 §7).

The launcher side lives in `launcher/Sources/Agent*.swift`, the agent in
`launcher/agent/`. Rust never speaks this protocol: what the agent says ends at
the launcher child, which turns it into the fixed states of
[the supervisor protocol](vm-supervisor-protocol.md) or stops the VM.

## Who trusts whom

Job code runs as `runner` inside the guest, and a guest can be fully
compromised. So **everything the agent sends is hostile input to the launcher.**

- **The launcher reads only the closed vocabulary below.** A reply that does not
  match exactly is a protocol violation: an unknown field, a missing one, a
  duplicate key, a value out of range, a frame over its cap, a reply to a
  request that was not asked, or a phase change this document does not allow.
- **A violation stops the VM.** The child logs a fixed code (`event=agent-error
  what=<code>`), never the guest's bytes, force-stops the VM and exits with 5.
  The supervisor reports that as `stop_reason: agent_error`. The codes, the
  launcher's own words (`AgentViolation` in `launcher/Sources/Agent.swift`):
  `connection_lost`, `version_mismatch`, `bad_reply` (a frame this document does
  not decode, or one over the cap), `wrong_reply` (a reply under another `id`, or
  to another request), `error_reply`, `unasked`, `bad_phase`, `start_failed`,
  `probe_failed` and `timeout`. A connection that closes while the VM is already
  stopping is no violation.
- **Nothing the agent sends is forwarded.** The child keeps the job's exit status
  in its own log, as integers it formatted itself. `_faber` sees only
  `ready` and `job_finished`, decided by the child, and, for a runner the agent
  says never took a job, the VM stopping with `runner_exited_without_job`.

The agent trusts the launcher: a connection on its vsock port can only come from
the host. It still checks the peer's CID is the host's (`VMADDR_CID_HOST`, 2) and
holds the launcher's frames to the same strictness, as defence in depth.

## Transport

- **virtio-vsock.** The VM's one socket device (the wall's device list). The
  agent listens on **port 6100** in the guest. The launcher connects with
  `VZVirtioSocketDevice.connect(toPort:)` once the VM is running, and retries
  every second until the ready deadline.
- **One connection for the VM's life.** The agent serves one connection at a
  time and refuses another while one is open. If the connection drops after the
  handshake, the launcher treats it as a violation (`connection_lost`). The
  agent closes a connection that sends nothing for 60 s; the launcher polls
  every second until the job is finished, then stops polling.
- **One frame is one JSON object on one line,** UTF-8, ending in one LF, as in
  the supervisor protocol.
- **Caps.** A launcher→agent frame is at most 16 384 bytes (it carries the JIT
  config). An **agent→launcher frame is at most 1 024 bytes**, which the
  launcher's reader enforces before it buffers more. No reply defined here is
  near that.
- **Lockstep.** The launcher sends one request and waits for its one reply. The
  agent never sends unasked; a byte that arrives with no request outstanding is a
  violation (`unasked`).
- **Deadlines the launcher holds the agent to:**
  - **ready:** the agent must report `ready` within 600 s of the VM running
    (`--agent-ready-timeout`). The guest's desktop session was measured at about
    57 s after boot in the 2026-10-10 spike (ADR-0026); faber's own macOS VMs
    start in a login session (ADR-0051).
  - **every reply:** 1.5 s. `probe` and `begin` are answered inside the
    supervisor's 2 s call to the child, so neither waits on slow work: `begin`
    replies once the config is held, and the runner starts after.
  - **started:** after `begin`, the agent must report `running` (or `exited`)
    within 300 s. That is longer than the guest's own deadlines add up to: 50 s
    for the Software Update writes before the hand-off (5 s each plus 5 s for its
    output reader) and 225 s for the hand-off itself (three launchctl runs of 10 s
    plus 5 s for their output, 60 s to connect, 10 s for the config, 110 s to
    start), 275 s in all. So a start the guest still counts as on time is never
    stopped as an agent error.
  - **verified:** after `verify`, the agent must report `verified` within 360 s.
  A missed deadline is a violation (`timeout`).
- **probe and begin never wait behind a poll for long.** They are serialized with
  the status poll. If a poll's exchange is still in flight after 300 ms, the child
  answers the supervisor `wrong_state` without sending anything, which is no
  violation, and faber may ask again. 300 ms plus the 1.5 s reply deadline fits the
  supervisor's 2 s call.

Objects are closed, fields are written in the order listed, and the frame shape
is the supervisor protocol's: `{"id":N,"request":{"<op>":{…}}}` and
`{"id":N,"reply":{"<op>":{…}}}`, `id` from 1 to 4 294 967 295 and echoed back.
An error is a reply too: `{"id":N,"reply":{"error":{"code":C}}}`. The codec is
`launcher/Shared/AgentProto.swift`, which both sides compile.

## Requests and replies

### `hello`

```json
{"id":1,"request":{"hello":{"protocol":3}}}
{"id":1,"reply":{"hello":{"protocol":3}}}
```

The agent answers `hello` only with its own version. On any other it answers
`{"error":{"code":"version_mismatch"}}` and closes. The launcher speaks exactly
one version; a mismatch is a violation (`version_mismatch`). The agent's own
release version is not on the wire: it is in the tools disk's manifest, which
faber wrote.

### `status`

```json
{"id":2,"request":{"status":{}}}
{"id":2,"reply":{"status":{"phase":"waiting_console"}}}
{"id":9,"reply":{"status":{"phase":"exited","exit_code":0,"job":"started"}}}
```

| `phase` | Meaning |
|---|---|
| `waiting_console` | The console user is not yet `runner`. |
| `ready` | The console user is `runner`, with the uid `runner` has in the guest's directory. |
| `starting` | `begin` was accepted; the LaunchAgent is being started. |
| `running` | The runner process is running in `runner`'s session. |
| `exited` | The runner exited. Carries at most one of `exit_code` (0–255) or `signal` (1–31); neither when the agent lost track of it. Then `job`, when the agent could tell: `started` when GitHub handed the runner a job, `none` when it never did ([The runner's exit](#the-runners-exit)). `job` is present with `exited` and with no other phase. |
| `failed` | The runner could not be started. |
| `verifying` | `verify` was accepted; the agent runs the image's checks. |
| `verified` | The checks are done. Carries `failed`, the codes of the checks that failed (below), each once and in the table's order; empty is a pass. `failed` is present with `verified` and with no other phase. |

The launcher polls `status` about once a second. The phases it accepts, in order:
`waiting_console` → `ready` → (`probe`) → (`begin`) → `starting` → `running` →
`exited`. A phase may repeat. After `begin` any forward move is accepted, since a
1 s poll can miss a short phase: `starting` may go straight to `exited`, and the
first phase seen after `begin` may already be `running` or `exited`. A verification
takes `verify` in `begin`'s place: `ready` → (`probe`) → (`verify`) → `verifying`
→ `verified`, and `verified` repeats with the same `failed`. Anything
else, such as `waiting_console` after `ready`, `ready` after `begin`, `running`
before `begin`, a job's phase after `verify`, `verifying` or `verified` without
`verify`, a verdict that changes, or `failed`, stops the VM (`bad_phase`, or
`start_failed` for `failed`).

The launcher maps:

- `ready`, the first time → the child's state `booting` → `ready`
- `verified` → `verifying` → `verified`, carrying the codes it decoded (and
  logging `event=verified failed=N`, a count it formatted itself)
- `exited` → `in_service` → `job_finished`, logging
  `event=runner-exited exit_code=N` or `signal=N` (or `status=unknown`) and
  `job=started`, `job=none` or `job=unknown`, written from the integer and the
  word it parsed
- `exited` with `job` `none` → not `job_finished`: the runner left without ever
  being handed a job, so no job finished. The child moves to `stopping`, waits 2 s
  for the runner's tail to reach the console, force-stops the VM and exits with 6,
  which the supervisor reports as `stop_reason: runner_exited_without_job` and
  keeps the console for. Without `job` the exit is taken as a finished job, as
  before version 3.

### `probe`

```json
{"id":3,"request":{"probe":{"addr":"192.0.2.1","port":9,"count":3}}}
{"id":3,"reply":{"probed":{"sent":3}}}
```

The agent sends `count` UDP datagrams, each of 8 zero bytes, from its NAT
address to `addr`:`port`, and reports how many `sendto(2)` accepted.

- **The launcher picks the target, never the guest.** `addr` is the slot's
  gateway, the host's own address on the VM's network (on vmnet the subnet's
  second address; on Apple's shared NAT its default, `192.168.64.1`, which the
  launcher cannot ask the framework for), so the anchor's `host`
  rule for that slot must count every datagram (`src/wall/counters.rs`,
  `ProbeTarget::Host`). `count` is 1–16, `port` 1–65 535, `addr` a dotted-quad
  IPv4 address.
- **Valid in phase `ready` only,** once.
- **`sent` must equal `count`.** Anything else is a violation (`probe_failed`):
  a probe not fully sent proves nothing, and the VM gets no work.
- The verdict is the anchor's counters, read by faber from the root wall
  daemon. The agent's reply proves only that the datagrams left.

### `begin`

```json
{"id":4,"request":{"begin":{"jit_config":"ZmFrZS1qaXQtY29uZmln"}}}
{"id":4,"reply":{"accepted":{}}}
```

- **Valid once, in phase `ready`, after `probe`.** `jit_config` has the
  supervisor protocol's grammar: 1–8 192 bytes of `[A-Za-z0-9+/=]`.
- The agent holds the config in memory only, replies `accepted`, and moves to
  `starting`. It never writes the config to disk and never puts it on any
  process's argv, in the guest or on the host.

### `verify`

```json
{"id":4,"request":{"verify":{"xcode":{"version":"27.0","build":"18A5123b"},"runtimes":[{"platform":"iOS","version":"27.0","build":"24A335"}]}}}
{"id":4,"reply":{"accepted":{}}}
{"id":9,"reply":{"status":{"phase":"verified","failed":["filevault","apple_id"]}}}
```

ADR-0049 §7: run the golden image's checks on this fresh clone instead of a job.

- **Valid once, in phase `ready`, after `probe`, in place of `begin`.** No JIT
  config exists for such a VM, and `begin` is refused after `verify`.
- **Expectations in, codes out.** The body is what faber read from the image's
  manifest: `xcode` (optional; absent for a bundle without a manifest) with
  `version` (Xcode's version line: first a digit, then `[A-Za-z0-9. ]`, at most 32
  bytes) and `build` (3–16 letters and digits), and `runtimes`, 0–64 of
  `{platform, version, build}` (1–16 letters; first a digit, then digits and dots,
  at most 16 bytes; 3–16 letters and digits).
- The agent replies `accepted` at once and runs the checks on a thread of its own:
  the phase is `verifying` until they are done, then `verified`.
- **`failed` is a closed set.** Each check is one of these codes, and a check
  that cannot be answered (a command that does not run or times out) fails:

| Code | Passes when |
|---|---|
| `console_user` | The console user is `runner`, with `runner`'s directory uid. |
| `runner_not_admin` | `runner` is not in the `admin` group, nested groups included (`dsmemberutil checkmembership`), its primary group is not `admin`'s, and `sudo -n -l -U runner` says it may run nothing. |
| `admins_locked` | `dscl . -read /Users/root` answers with no `AuthenticationAuthority`, or one holding `;DisabledUser;`; and every other admin has the shell `/usr/bin/false`. The admins are the directory's: the `admin` group's members by name (`GroupMembership`) and by GeneratedUID (`GroupMembers`, resolved through `dscl -plist . -readall /Users RecordName GeneratedUID`), those of every group nested in it by `NestedGroups` (transitively), and every account whose primary group is `admin`'s (`dscl -plist . -readall /Groups`). An admin it cannot judge fails: a listing that cannot be read, a nested group not in the directory, a member GUID no user carries, a member with no account. The build's `lock-admins` step locks the same set. `/sbin/nologin` does not count: a GUI login passes with it (ADR-0051). |
| `remote_login` | `launchctl print-disabled system` lists sshd as disabled, or, when it does not list it, `launchctl print system/com.openssh.sshd` exits 113 (not loaded). |
| `filevault` | `fdesetup status` says FileVault is off. |
| `apple_id` | No account under `/Users` has a non-empty `Accounts` list in `Library/Preferences/MobileMeAccounts.plist`. |
| `software_update` | `/Library/Preferences/com.apple.SoftwareUpdate.plist` holds `AutomaticDownload`, `CriticalUpdateInstall`, `ConfigDataInstall` and `AutomaticCheckEnabled`, each false, and `AutomaticallyInstallMacOSUpdates` is absent or false. |
| `xcode` | As `runner` (`sudo -n -H -u runner` from root, the account and home a job has): `xcodebuild -version` exits 0, and, when `xcode` is expected, prints `Xcode <version>` first and `Build version <build>`; `xcode-select -p` resolves to a directory `runner` can enter and read; and `/usr/bin/cc` and `/usr/bin/swiftc` each compile, link and run a trivial program in a fresh directory under `/private/tmp`. Root bypasses every permission bit, so root's answer is not a job's. |
| `runtimes` | Every expected runtime is in `xcrun simctl list runtimes -j` with its platform, version and build, and `isAvailable`. |
| `brew_owner` | `/opt/homebrew` is owned by `runner`'s uid and `/opt/homebrew/bin/brew` exists. |

Every check passes only on a plain, expected answer: a command that fails, times
out or says anything else counts as failed. The wall is not one of them: faber proves it from the anchor's counters after
`probe`, as for a job. Nothing a check's command prints crosses vsock; the
launcher sees the codes and nothing else, and decodes anything else (an unknown
code, free text, an object, a repeat, the wrong order) as `bad_reply`.

**Each check also explains itself, on the guest's console, for the operator.**
The agent locks `/dev/cu.virtio` to root (0600, as the golden build's wrapper
does) and writes one line per check, then a last one, each on a line of its own:

```
FABER-GOLDEN-VERIFY start checks=10
FABER-GOLDEN-VERIFY check=admins_locked ok=false why=admin _mbsetupuser shell /bin/bash
FABER-GOLDEN-VERIFY check=software_update ok=false why=AutomaticallyInstallMacOSUpdates=1
FABER-GOLDEN-VERIFY evidence check=software_update schedule=exit 0 'Automatic checking for updates is turned off.'
FABER-GOLDEN-VERIFY done failed=admins_locked,software_update
```

The `evidence` line follows the software_update line. It holds the first line of
`softwareupdate --schedule` (stdout and stderr, at most 100 bytes, 20 s at most),
or `did not run or timed out`. It is for the operator only. The check's verdict
is the plist's alone, and this line never changes it.

`why` is the one fact that decided the check: the account, the key and its value,
the command and its exit status, or why a plist could not be read. A line is
printable ASCII (every other byte becomes `?`), at most 199 bytes, and a
verification writes at most 32. It never carries a secret: root's record is never
shown (as root, `dscl` prints its `ShadowHashData`), and no command's output is
copied beyond a bounded first line of a tool that prints none (`fdesetup`,
`xcodebuild -version`). This is a second, one-way channel, and it changes nothing
on the wire: the codes are still the verdict. The supervisor keeps the console
where only root and its own account can read it, and never sends it to `_faber`
([the supervisor protocol](vm-supervisor-protocol.md#the-kept-console-logs)).

## Errors

`{"error":{"code":C}}`, with `C` one of `version_mismatch`, `handshake_required`,
`bad_request`, `wrong_phase`. The agent closes after `version_mismatch`,
`handshake_required` and `bad_request`. A request too malformed to carry a
readable `id` gets no reply: the agent closes. To the launcher every error reply is a
violation: it only sends what is valid.

## Inside the guest

**The bootstrap.** The golden image holds one root-owned LaunchDaemon and its
script (`launcher/golden/`). At boot the script finds the tools disk by its
volume label, `FABERTOOLS` (faber's `hdiutil` stamps it,
`src/vm/tools_disk.rs`), on a read-only device that is not the boot disk. It
mounts it read-only, `nosuid,nodev,nobrowse`, at `/private/var/faber/tools`
and runs the agent from it as root. The image holds neither the agent nor the
runner.

**Software Update, written again.** A clone's boot does not keep the image's
Software Update keys: on a real host a verify clone lost `AutomaticCheckEnabled`
about a minute after boot (ADR-0049 §5, amended 2026-10-26). So the agent, as
root, runs the golden build's step a again (`launcher/golden/provision.sh`):
`/usr/bin/defaults write /Library/Preferences/com.apple.SoftwareUpdate <key> -bool
false` for `AutomaticDownload`, `CriticalUpdateInstall`, `ConfigDataInstall` and
`AutomaticCheckEnabled`, then `AutoUpdate` false in
`/Library/Preferences/com.apple.commerce`. Each write gets 5 s, plus 5 s for its output reader. It runs them at
two moments:

1. when `serve` starts, before the agent opens its vsock port, so it never
   reports `ready` first;
2. on `begin`, after the `accepted` reply and before the hand-off below, so a
   job starts with the keys off whatever rewrote them in between.

`verify` writes nothing of its own; it judges the plist as the clone left it.
A write that fails or times out never stops the agent. It runs the rest, then
writes one line to the guest's console and to its log:

```
FABER-AGENT updates_off at=serve ok=true wrote=5/5
FABER-AGENT updates_off at=begin ok=false wrote=4/5 failed=AutomaticCheckEnabled exit 1
```

**Ready.** The agent reads the console user with
`SCDynamicStoreCopyConsoleUser` and reports `ready` only when it is `runner`
with `runner`'s directory uid (from `getpwnam`), and that uid is not 0. The
agent checks the peer of every vsock connection: its CID must be the host's.

**Starting the runner.** On `begin` the agent, after writing the Software Update
keys again:

1. writes a root-owned LaunchAgent plist, mode 0644, whose program is the agent
   itself in `runner-exec` mode, with `LANG` and `LC_ALL` set to `en_US.UTF-8`.
   The plist holds no secret. It is
   `/private/var/faber/handoff/io.github.rejifald.faber.runner.plist`, in a
   directory only root can write, and is limited to the `Aqua` session.
2. opens a one-shot hand-off socket that only `runner`'s uid may use, checked
   with `getpeereid(2)` on the connection: `/private/var/faber/handoff/runner.sock`,
   owned by `runner`, mode 0600. Another peer is closed unread.
3. runs `launchctl bootstrap gui/<uid> <plist>` with that fixed argv, at most 10 s
   (plus 5 s for its output reader).

The tools disk must be usable by `runner`, not only by root: launchd runs
`runner-exec` from it as `runner`, and `runner-exec` reads the manifest and the
tarball there. faber builds the disk with `hdiutil create -srcfolder` as
`_faber`, which stamps `_faber`'s uid and gid on every file and gives the
volume's root its source folder's mode, and the bootstrap mounts it with owners
honoured. So `runner` gets only the "other" bits, and faber writes the folder
0755, the agent 0555 and the tarball and manifest 0444 (`src/vm/tools_disk.rs`,
`guest_readable`), whatever its umask.

`runner-exec`, running as `runner` in its own session, connects to the hand-off
socket, checks its peer is root, and reads the config. It reads the tools disk's
`manifest.json` strictly (at most 4 KiB, its four fields only, `runner_version`
`N.N.N`), creates `~/faber-runner` (refusing one that exists: a clone runs one
job), runs `/usr/bin/tar -xzf <actions-runner-osx-arm64-<runner_version>.tar.gz>
-C ~/faber-runner`, and starts `run.sh` there with the config in
`ACTIONS_RUNNER_INPUT_JITCONFIG`, the runner's own fallback for `--jitconfig`,
which faber's Docker backend uses too (`src/docker.rs`), and a UTF-8 locale. It
never passes it as an argument. It sends `run.sh`'s stdout and stderr to
`~/faber-runner/_faber-runner-output.log` (mode 0600, a file the tarball never
holds; without it the runner still runs, its output unread). It waits for the
runner, reports the exit status on the hand-off connection, and exits. The agent accepts one hand-off connection,
then closes the listener, removes the socket and drops its copy of the config.

The hand-off frames are one JSON line each, in the same codec: the agent sends
`{"jit_config":J}` (at most 16 384 bytes), and `runner-exec` answers
`{"started":{}}` once `run.sh` is spawned, then `{"exited":{"exit_code":N}}`,
`{"exited":{"signal":N}}`, `{"exited":{}}` or `{"failed":{}}` (at most 256
bytes each). `exited` ends with `"job":"started"` or `"job":"none"` when
`runner-exec` could read the runner's `_diag` after it exited
([The runner's exit](#the-runners-exit)). Before `started` it also reports each of its steps:
`{"step":{"name":S,"result":"ok"|"failed"}}`, with at most one of `exit_code`
(0–255), `signal` (1–31) or `errno` (1–255), `S` one of `config_read`,
`manifest`, `mkdir`, `extract`, `spawn`. Nothing else: no free text crosses this
connection. The agent ignores a step after `started`, when job code runs as
`runner`. The agent's deadlines: 60 s for `runner-exec` to connect, 10 s for
the config, 110 s for `started`, and 6 days for the job; GitHub's own limit for a
self-hosted job is 5. A hand-off that ends before `started` makes the agent
report `failed`; one that ends after makes it report `exited` with neither code.

### The hand-off's console lines

A job VM whose hand-off fails stops with `start_failed`, and the launcher says
only that code. So the agent writes one line per hand-off step to the guest's
console, the verification's channel and shape
([verify](#verify)): printable ASCII, at most 199 bytes, at most 32 lines.

```
FABER-AGENT handoff step=session ok=true why=console user runner uid 502; launchctl print gui/502: exit 0
FABER-AGENT handoff step=tools_access ok=false why=/private/var/faber/tools mode 700 uid 299 gid 299: runner cannot search it
FABER-AGENT handoff step=plist ok=true why=/private/var/faber/handoff/io.github.rejifald.faber.runner.plist
FABER-AGENT handoff step=bootstrap ok=true why=launchctl bootstrap gui/502: exit 0
FABER-AGENT handoff step=connect ok=false why=runner-exec never connected within 60s; launchd: state=not running, runs=1, last exit code=78: EX_CONFIG
```

Root's own steps, in order: `session` (the console user and its uid, and
`launchctl print gui/<uid>`'s exit: whether `runner`'s GUI domain exists),
`tools_access` (whether `runner`, by the mode bits of every directory from `/`
down, may run `faber-agent` and read `manifest.json` and the tarball; observed
only, since bits cannot show an ACL), `plist`, `socket`, `bootstrap` (its exit
status and the first line of what it printed, at most 100 bytes), `connect`
(on a miss, launchd's `state`, `runs`, `last exit code` and `last terminating
signal` from `launchctl print gui/<uid>/<label>`), `config`, then `run`
(`run.sh` started, how it ended and after how long, or why it never started).

`runner-exec` runs as `runner` and cannot open the console, which root locks to
itself. It reports its steps to root on the hand-off connection it already
holds, as the closed `step` frames above, and root writes each as
`step=<name>`, the object (a path, the `tar` command) from paths root knows
itself, and the integer: `step=extract ok=false why=tar -xzf <tarball> -C
/Users/runner/faber-runner: exit 2`. No byte `runner-exec` chose reaches the
console.

No line carries the JIT config: it is on no argv, in no file and in no command
output the agent reads. As a second guard every line is redacted: a run of 24 or
more bytes of the config's alphabet (`[A-Za-z0-9+/=]`) that holds anything but
lowercase letters and `/` becomes `<redacted>`.

The supervisor keeps the console of a job VM that stopped without finishing as
`job-<image>-failed-latest.log` ([the supervisor
protocol](vm-supervisor-protocol.md#the-kept-console-logs)), and faber's error
for the job names its stop reason and the command to read it:
`sudo cat -v /var/db/faber-mac/state/job-<image>-failed-latest.log`.

### The runner's exit

`run.sh` exits 0 for nearly every way the runner ends: after a job, and also
when the listener gives up on a registration GitHub deleted, a version GitHub
retired, or a session it could not create. Its exit status says nothing about
whether a job ran. So the agent reports that apart, as a closed word, and writes
the runner's own last lines to the console for the operator.

**Whether the runner took a job** is the runner's own fact. Its listener starts
`Runner.Worker` only once GitHub has assigned it a job, and the worker's first act
is to open `_diag/Worker_<yyyyMMdd>-<HHmmss>-utc.log` under the runner's root. A
listener that never got a job leaves only `Runner_*.log` files there. After
`run.sh` exits, `runner-exec` reads the names in `~/faber-runner/_diag`, never
their bytes, and reports `job`: `started` with a worker log, `none` without one,
and `none` when there is no `_diag` at all (the listener never started). A
directory it cannot read leaves `job` out. When `runner-exec`'s report has none
(it was killed, or could not read the directory), root reads the same names itself.

The word is the guest's, and job code runs as `runner`, which can write that
directory. A job that forges or removes a worker log changes only how its own run
is classified: `none` stops its VM and keeps the console, `started` is a finished
job, as every exit was before version 3. Nothing else turns on it.

**The runner's tail.** After `run.sh` exits, root writes to the console the last
20 lines of `_faber-runner-output.log` and of the newest `_diag/Runner_*.log`,
each under a header line, before it reports `exited` (the times here are made up):

```
FABER-AGENT runner output: the last 3 lines run.sh wrote to stdout and stderr
FABER-AGENT runner out| ??? Connected to GitHub
FABER-AGENT runner out| 2026-01-01 12:00:30Z: Listening for Jobs
FABER-AGENT runner out| Exiting runner...
FABER-AGENT runner diag: the last 20 lines of _diag/Runner_20260101-120000-utc.log
FABER-AGENT runner diag| [2026-01-01 12:00:30Z INFO Runner] ...
```

(`√` is not ASCII, so it shows as `???`.)

- **Read as root, through no symlink.** Every component from `runner`'s home down
  is opened with `O_NOFOLLOW`; a directory must be `runner`'s, and a file a regular
  file `runner` owns with one link, read at most 64 KiB from its end without
  blocking. A planted link to a file only root may read is refused.
- **Bounded:** 20 lines per file, each a console line (printable ASCII, at most
  199 bytes), at most 8 KiB in all, apart from the hand-off's 32 lines.
- **Redacted before a byte is written.** A line holding `authorization`, `bearer`,
  `token`, `jitconfig`, `password`, `secret` or `credential`, in any case, is cut
  after the first of them; and every run of 20 or more bytes of
  `[A-Za-z0-9+/=_-]` that holds anything but lowercase letters, `/`, `_` and `-`
  becomes `<redacted>`, which covers a JIT config, a GitHub token and a JWT's
  segments, and also a path with a capital in it. A log's name is shown as it is
  only in the runner's own shape, `Runner_<yyyyMMdd>-<HHmmss>-utc.log`.
- **The console only.** Nothing of the tail crosses vsock or reaches `_faber`. The
  supervisor keeps the console of a VM stopped with `runner_exited_without_job`,
  as of any job VM that stopped without finishing, and faber's error for the run
  names the command to read it.

## SSH fallback

ADR-0026 allows the control session to run over SSH if a macOS guest cannot use
vsock. That was not needed: this protocol is the path. Check 15, "faber controls
a macOS guest over vsock alone, and no JIT config appears on a host command
line", is the owner's builder run (`launcher/README.md`).
