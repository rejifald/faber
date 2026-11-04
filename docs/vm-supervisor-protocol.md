# The VM supervisor protocol

The wire between faber, running as `_faber`, and the VM supervisor, the launcher
running as `_faber_vm` (ADR-0025, ADR-0026). faber asks; the supervisor starts,
watches and stops VMs, one launcher process per VM.

**Protocol version: 4.** Version 4 added `start`'s optional `disk_bytes` and
the Linux record's `disk_bytes` and `disk_allocated_bytes` (ADR-0061 §4).
Version 3 added the stop reason `runner_exited_without_job` (#392). Version 2 added `verify`, the states
`verifying` and `verified`, and the record's `failed` (ADR-0049 §7).

The Rust side lives in `src/vm/proto.rs`. The golden fixtures in
`tests/fixtures/vm-proto/` are shared by both sides: the Rust tests and the
launcher's Swift tests (ADR-0026 slice 3) read the same files.

## What `_faber` may read

`_faber` holds the GitHub credential, and the supervisor handles traffic from
guests where job code is root. So ADR-0025's rule applies to everything on this
wire: **`_faber` parses only the fixed vocabulary in this document, never a
guest's bytes.**

- **Every value faber receives is from a closed set, a bounded number or a
  grammar-bounded name.** States, stop reasons and error codes are enumerations.
  Slots, timestamps and protocol versions are integers in fixed ranges. The only
  strings are claim ids and the supervisor's version, each at most 64 bytes of a
  small ASCII alphabet with no spaces, quotes, slashes or control characters.
  A claim id is usually one faber sent, but `list` may show one it does not
  hold; faber compares claim ids and never interprets them.
- **A verification's result is codes, never words.** `failed` lists check codes
  from a closed set (below); the launcher decodes the guest agent's report into
  them or stops the VM.
- **No free text crosses the wire toward faber.** An error is a code, never a
  message. A guest's console output, its agent's words and a job's exit status
  stay on the supervisor's side. The supervisor turns what the guest agent says
  into one of the states below, or stops the VM.
- **A frame that does not match exactly is refused,** not partly read: an unknown
  field, a missing field, a duplicate key, a value out of range, a string outside
  its grammar or an oversized frame. faber then closes the connection, and the
  supervisor is treated as unreachable until a new handshake succeeds.

The supervisor applies the same strictness to faber's requests. That is defence
in depth for the supervisor, not a trust boundary for faber.

## Transport

- **A Unix stream socket** that `sudo faber setup` creates for the supervisor's
  LaunchDaemon. Only `_faber` may connect:
  - the socket's directory is `_faber_vm`'s, with no access for other accounts
    beyond what `_faber` needs to reach the socket;
  - on every accepted connection, the supervisor reads the peer's uid
    (`getpeereid(3)`) and closes the connection unless it is `_faber`'s;
  - on every connection it opens, faber reads the peer's uid and closes the
    connection unless it is `_faber_vm`'s. A socket some other account managed to
    put in that path gets nothing, not even a hello.
- **A second supervisor, for macOS VMs (ADR-0051).** Where the setup record
  names the session account `_faber_mac`, its LaunchAgent runs the same
  `supervise` in that account's login session, at
  `/var/db/faber-mac/run/supervisor.sock` (`_faber_mac:_faber` 0710, like
  `_faber_vm`'s `run/`), with its own state and clone directories. faber sends
  every macOS VM job there, and checks the peer is `_faber_mac`; the Linux VM
  stays with `_faber_vm`'s supervisor. While that socket does not answer a
  handshake, faber starts no macOS VM job ("no VM session"). A record without
  the account keeps everything on `_faber_vm`'s supervisor. A new launcher
  reaches it as it reaches `_faber_vm`'s: the updater (as `_faber`) sends it
  `exit` while its session answers, and the agent's `KeepAlive` relaunches it;
  `sudo faber setup` boots the agent out of the account's GUI domain and
  bootstraps it again when one is up (never `kickstart -k`, which leaves
  launchd pinned to the replaced launcher, #479). A session that is down starts on the new launcher at its next
  login.
- **One frame is one JSON object on one line,** UTF-8, ending in a single LF
  (`\n`). JSON escapes every control character inside a string, so a frame
  never contains a raw LF before its end.
- **A frame is at most 16 384 bytes, its LF included** (`MAX_FRAME`). A reader
  stops reading at that many bytes; if no LF has come by then, the frame is
  refused and the connection closed. The cap is the reader's to enforce, before
  it buffers more: checking a line's length after reading it whole is too late.
  The I/O code on each side tests it. No frame this protocol defines comes close:
  the largest, a 16-VM list, is about 3 KiB.
- **Lockstep.** The client sends one request and waits for its one response
  before it sends the next. The supervisor never sends a frame unasked. State
  changes are learned by asking (`list`, `adopt`), the way faber's reconcile loop
  already polls.
- **The first exchange on every connection is the handshake.** Any other request
  before a successful handshake is answered with `handshake_required` and the
  connection closed.
- **Either side closes on a protocol error.** The supervisor answers a request
  it refuses with `bad_request` when it can still read the frame's `id`, and
  closes without a reply when it cannot (an oversized frame, or one that is not
  JSON). A client that wants to recover opens a new connection and handshakes
  again.

## Frames

A request:

```json
{"id":7,"request":{"stop":{"claim":"123456789-1760700000"}}}
```

A response:

```json
{"id":7,"reply":{"done":{}}}
```

- **`id`** is chosen by the client, an integer from 1 to 4 294 967 295. The
  response carries the same `id`. A client that receives any other `id` closes
  the connection.
- **`request` and `reply` each hold exactly one key,** the operation's name,
  whose value is an object. An operation without arguments still takes `{}`.
- **Objects are closed.** No field may be added, left out (unless this document
  marks it optional) or repeated. An optional field that is absent is left out,
  never `null`.
- **An object is always a JSON object.** The frame, `request`, `reply`, every
  operation's value and every VM record is refused in any other form, such as
  an array of its values in field order. The only arrays are `claims`, `vms`,
  `found` and `unknown`.
- Encoders write fields in the order this document lists them, with no
  whitespace. Decoders accept any order. The fixtures are written that way; the
  Rust tests check that decoding then encoding a fixture reproduces its bytes.

## Field grammar and bounds

| Field | Type | Bounds |
|---|---|---|
| `id` | integer | 1 … 4 294 967 295 |
| `protocol` | integer | 1 … 65 535 |
| `supervisor_version` | string | 1–64 bytes of `[0-9A-Za-z.+-]` |
| `slot` | integer | 0 … 15 (the launcher's slot: it fixes the VM's MAC) |
| `kind` | string | `macos` or `linux` |
| `claim` | string | 1–64 bytes; first `[A-Za-z0-9]`, then `[A-Za-z0-9._-]` |
| `image` | string | 1–64 bytes; first `[a-z0-9]`, then `[a-z0-9._-]` |
| `tools` | string | same grammar as `image` |
| `cpus` | integer | 1 … 64 |
| `memory_mib` | integer | 1 024 … 262 144 |
| `jit_config` | string | 1–8 192 bytes of the base64 alphabet `[A-Za-z0-9+/=]` |
| `started_unix` | integer | 0 … 4 102 444 800 (seconds; the year 2100) |
| `disk_bytes` (in `start`) | integer | 1 048 576 … 1 099 511 627 776 (1 MiB … 1 TiB), a whole number of MiB |
| `disk_bytes`, `disk_allocated_bytes` (in a record) | integer | 0 … 2 199 023 255 552 (2 TiB) |
| `expect` (in `verify`) | object | `xcode` (optional: `version`, 1–32 bytes, first a digit, then `[A-Za-z0-9. ]`; `build`, 3–16 bytes of `[A-Za-z0-9]`) and `runtimes`, 0–64 of `{platform: 1–16 letters, version: first a digit then digits and dots, ≤16 bytes, build: 3–16 letters and digits}` |
| `failed` (in a record) | array of check codes | 0–10 entries, each once, in the order of the table under *Check codes* |
| `claims` (in `adopt`) | array of `claim` | 1–16 entries, no duplicates |
| `vms` (in `vms`) | array of VM records | 0–16 entries, no slot or claim twice |
| `found`, `unknown` (in `adopted`) | arrays | each 0–16 entries, together exactly the claims asked |

- **`image` and `tools` are names, never paths.** The supervisor resolves them
  inside the root-owned image store and faber's tools-disk cache, which
  `_faber_vm` can read and never write. The grammar has no `/`, and no leading
  `.`, so a name cannot leave its directory.
- **`claim` is faber's own claim id,** the `<job_id>-<mint_ts>` it already
  writes as `org.faber.id` (ADR-0011), or a fixed id for the Linux VM. The
  supervisor treats it as opaque.
- **`jit_config` is a credential.** It is single-use and short-lived, but neither
  side ever logs it, prints it or writes it to disk. The supervisor holds it only
  until it has handed it to the guest agent over vsock, and the Rust type redacts
  it from `Debug`. It never appears on a command line (ADR-0026).
- Integers are JSON integers: no fraction, no exponent, no sign on a positive
  value.

## The handshake

The client's first request:

```json
{"id":1,"request":{"hello":{"protocol":4}}}
```

The supervisor answers with its own version only if it is exactly the one asked:

```json
{"id":1,"reply":{"hello":{"protocol":4,"supervisor_version":"0.4.0"}}}
```

On any other version it answers:

```json
{"id":1,"reply":{"error":{"code":"version_mismatch","protocol":4}}}
```

where `protocol` is the one version the supervisor speaks.

- **There is no negotiation.** Each side speaks exactly one version, and the
  client offers exactly one. No list of versions is offered, and no side ever
  picks a lower one, so neither a peer nor anyone between them can steer a
  connection to an older protocol. Mismatched peers never exchange anything but
  the frozen core (below).
- **The client checks the reply, too.** A `hello` reply whose `protocol` is not
  the client's own is refused, as is a `hello` reply to anything but `hello`.
- **faber checks `supervisor_version`.** After a release it must equal faber's
  own stamped version (ADR-0026: the launcher's `--version` matches faber's).
  A difference is not a protocol error, since the two are paired by release and
  one may briefly lead the other. faber shows it and does not start new VMs
  until they agree.
- **A second `hello` on a connection** is answered with `bad_request`, and the
  connection closed.

### The frozen core

Three frames keep their exact shape in every version of this protocol, so any
faber can retire any supervisor, older or newer:

- the `hello` request, and the `version_mismatch` error reply;
- the `exit` request (below) and its `done` reply.

After a `version_mismatch`, the supervisor accepts exactly one more request on
that connection, and only `exit`. Anything else closes the connection. That is
how a new faber retires a supervisor that speaks an older protocol: ADR-0026's
update sequence asks the old supervisor to exit, and launchd starts the new
binary. `exit` grants nothing a version could change: it stops no VM.

## Requests

### `start`: start a VM in a slot

```json
{"id":2,"request":{"start":{"slot":0,"kind":"macos","claim":"123456789-1760700000","image":"macos-27","tools":"runner-2.330.0-agent-0.4.0","cpus":4,"memory_mib":8192}}}
```

- `tools` is **required for `macos` and forbidden for `linux`.** It names the
  read-only tools disk, holding the runner and the guest agent, that the clone
  gets as its second disk (ADR-0026, amended 2026-10-17). The supervisor
  resolves it to `<tools>.img` in the cache, the file faber's cache stores
  (`src/vm/tools_disk.rs`).
- For `macos`, the supervisor clones `image` (`faber-vm clone`, which writes the
  `faber-clone` marker) and boots the clone. For `linux`, `image` names the one
  long-lived Linux VM bundle, which is booted in place and never cloned.
- The supervisor writes the claim marker (below) before it spawns the launcher
  child, so a crash between the two leaves nothing it cannot find again.
- `disk_bytes` is **optional, and only for `linux`** (ADR-0061 §4). The VM is
  stopped, so before it builds the configuration the supervisor extends the
  bundle's `disk.raw` to this size with `ftruncate`, keeping its device and
  inode (the wall's Linux check reads both). Equal to the file's length, it does
  nothing. It answers `bad_request`, and starts nothing, for: `disk_bytes` on a
  `macos` start; a Linux boot in place (no `--linux-dir`), where the disk is
  root's image at 0444; a size smaller than the file (a disk never shrinks); a
  size that is not a whole number of MiB; a size above 1 TiB, its own bound
  whatever faber asks; growth larger than the free space on the volume holding
  the file. faber's answer to a refused grow is the same `start` without
  `disk_bytes`.

  ```json
  {"id":2,"request":{"start":{"slot":14,"kind":"linux","claim":"linux-engine","image":"linux","cpus":6,"memory_mib":16384,"disk_bytes":103079215104}}}
  ```
- **Reply:** `vm`, the new VM's record, normally in state `starting`.
- **Errors:** `slot_busy` (a VM not yet `stopped` holds the slot, including an
  `unmanaged` one), `claim_exists` (a live VM already carries the claim),
  `image_not_found`, `tools_not_found`, `capacity` (the framework refuses
  another VM: Apple's limit of two macOS guests), `start_failed`.

### `probe`: ask the guest to prove the wall

```json
{"id":3,"request":{"probe":{"claim":"123456789-1760700000"}}}
```

The supervisor asks the guest agent to send the wall probe (ADR-0026, *The
anchor is checked, not trusted*). It reports only that the probe was sent. The
verdict comes from the anchor's own rule counters, which faber reads from the
root wall daemon, never from the guest or the supervisor.

- **Valid in state `ready` only.**
- **Reply:** `vm`, in state `probe_sent`.
- **Errors:** `unknown_claim`, `wrong_state`.

### `begin`: put a proven VM to work

```json
{"id":4,"request":{"begin":{"claim":"123456789-1760700000","jit_config":"ZmFrZS1qaXQtY29uZmln"}}}
```

faber sends `begin` only after it has confirmed the probe from the rule counters.

- **macOS:** `jit_config` is **required**. The supervisor passes it to the guest
  agent over vsock, which starts the JIT runner in `runner`'s session.
- **Linux:** `jit_config` is **forbidden**. The supervisor opens the bridge from
  the guest's Docker socket to the host socket only `_faber` may use. Until
  `begin`, that host socket does not exist.
- **Valid in state `probe_sent` only.**
- **Reply:** `vm`, in state `in_service`.
- **Errors:** `unknown_claim`, `wrong_state`.

### `verify`: run the golden image's checks instead of a job

```json
{"id":5,"request":{"verify":{"claim":"0-1760700000000-a1b2c3d4","expect":{"xcode":{"version":"27.0","build":"18A5123b"},"runtimes":[{"platform":"iOS","version":"27.0","build":"24A335"}]}}}}
```

ADR-0049 §7. faber sends `verify` in `begin`'s place, after it has confirmed the
probe from the rule counters, to a VM it started for `faber golden verify` (or its
own re-verification). The claim is `0-<mint_ms>-<instance>`: job id 0, which no
GitHub job has. No JIT config is ever sent for it.

- **macOS only, valid in state `probe_sent` only,** and only to a child started
  with child protocol 2 or later; a version-1 child answers `wrong_state`.
- `expect` is what faber read from the image's `manifest.json`; it reaches the
  guest agent unchanged.
- **Reply:** `vm`, in state `verifying`. `list` later shows `verified`, with
  `failed`.
- **Errors:** `unknown_claim`, `wrong_state`, `bad_request` (a Linux VM).

### `stop`: stop a VM

```json
{"id":5,"request":{"stop":{"claim":"123456789-1760700000"}}}
```

- A macOS VM is force-stopped and its clone deleted. The Linux VM is asked to stop
  and force-stopped after its grace period, as the launcher core already does.
- **Idempotent.** A VM already `stopping` or `stopped` answers `done` again.
- **Reply:** `done`. Stopping is asynchronous; `list` shows `stopping`, then
  `stopped`.
- **Errors:** `unknown_claim`, `unmanaged` (see the versioning rule: such a VM is
  never stopped).

### `list`: every VM the supervisor knows

```json
{"id":6,"request":{"list":{}}}
```

- **Reply:** `vms`, one record per occupied slot, ordered by slot.

### `adopt`: take back VMs by their claims

```json
{"id":7,"request":{"adopt":{"claims":["123456789-1760700000","linux-engine"]}}}
```

faber sends `adopt` at boot, with the claims it is taking back (ADR-0011's
adoption, applied to VMs). The supervisor answers from the claim markers it has
verified, never from a slot alone.

- **Reply:** `adopted`, splitting the claims asked into `found`, the VM records
  that carry them, and `unknown`, the claims no live marker carries.
- An `unknown` claim is gone: faber settles it the way ADR-0011 settles a
  container that vanished. A VM in `list` whose claim faber does not hold is one
  faber's sweep may `stop`.

### `exit`: the supervisor exits

```json
{"id":8,"request":{"exit":{}}}
```

- The supervisor replies `done`, closes its socket and exits 0. launchd starts
  it again, on whatever binary is now installed.
- **It stops no VM.** Its LaunchDaemon sets `AbandonProcessGroup`, so every
  per-VM launcher child keeps running, and the next supervisor adopts it
  (below).
- Part of the frozen core: accepted after a `version_mismatch` too.

## Replies

| Reply | Shape | Answers |
|---|---|---|
| `hello` | `{"protocol":P,"supervisor_version":V}` | `hello` |
| `vm` | a VM record | `start`, `probe`, `begin`, `verify` |
| `vms` | `{"vms":[record, …]}` | `list` |
| `adopted` | `{"found":[record, …],"unknown":[claim, …]}` | `adopt` |
| `done` | `{}` | `stop`, `exit` |
| `error` | `{"code":C}`, or `{"code":"version_mismatch","protocol":P}` | any |

A reply that does not fit its request, a `vms` for a `stop`, say, is a protocol
error. `error`'s `protocol` field is present for `version_mismatch` and for no
other code.

### The VM record

```json
{"slot":0,"kind":"macos","claim":"123456789-1760700000","state":"stopped","stop_reason":"requested","started_unix":1760700000}
{"slot":0,"kind":"macos","claim":"0-1760700000000-a1b2c3d4","state":"verified","failed":["filevault"],"started_unix":1760700000}
```

- `slot`, `kind` and `claim` are the ones `start` gave.
- `state` is one of the states below.
- `stop_reason` is **present when `state` is `stopped` and absent otherwise.**
- `failed` is **present when `state` is `verified` and absent otherwise,** and
  only on a macOS VM. Empty is a pass. It is written after `stop_reason`'s place
  and before `started_unix`.
- `started_unix` is when the supervisor spawned the VM's launcher, so an adopted
  claim keeps its age (ADR-0011's `started_ms`, to the second).
- `disk_bytes` and `disk_allocated_bytes` are a Linux VM's disk file: its length,
  and the bytes its blocks hold (`st_blocks` × 512). **Both or neither, and never
  on a macOS VM.** They follow `started_unix`. They are absent when the supervisor
  cannot tell which file the VM boots from: a VM adopted after a supervisor
  restart is matched to the one bundle in `--linux-dir`, and to none when there
  are several.

  ```json
  {"slot":14,"kind":"linux","claim":"linux-engine","state":"in_service","started_unix":1760700000,"disk_bytes":68719476736,"disk_allocated_bytes":41943040000}
  ```

### The VM states

The whole set `_faber` may receive. It is closed: a new state is a new protocol
version.

| State | Meaning |
|---|---|
| `starting` | The launcher child is building the VM, checking the wall and booting it. The framework has not yet reported it running. |
| `booting` | The VM is running. Its guest agent has not yet reported ready. |
| `ready` | The guest agent reports ready: on macOS, the console user is `runner`; on Linux, the Docker relay answers. Waiting for `probe`. |
| `probe_sent` | The guest sent the wall probe. Waiting for faber's verdict, then `begin` or `stop`. |
| `in_service` | macOS: the JIT runner was started. Linux: the Docker socket is bridged. |
| `job_finished` | macOS only: the guest agent reports the JIT runner exited. Waiting for `stop`. |
| `verifying` | macOS only: `verify` was sent; the guest agent runs the image's checks. |
| `verified` | macOS only: the checks are done; the record carries `failed`. Waiting for `stop`. |
| `stopping` | A stop was asked for, or the supervisor is stopping the VM itself. |
| `stopped` | The VM and its launcher child have exited, and a clone is deleted. Carries `stop_reason`. The slot is free. |
| `unmanaged` | A launcher child this supervisor cannot speak to, left from an older supervisor. It holds its slot until its process exits, and is never stopped (see the versioning rule). |

The order is the order above, except that `stopping` and `stopped` may follow any
state, and `unmanaged` is entered only at adoption and left only for `stopped`.
`job_finished` carries no exit status: the job's outcome is GitHub's to report,
and an exit status from the guest is a guest's byte.

### Stop reasons

| Reason | Meaning |
|---|---|
| `requested` | faber asked, with `stop`. |
| `guest_shutdown` | The guest shut itself down. |
| `start_failed` | The VM never reached `booting`: the wall check, the framework or the clone failed. |
| `wall_violation` | The launcher found the running VM's devices or files changed after the wall check, and force-stopped it (exit 4 in the launcher core). |
| `agent_error` | The guest agent broke its own protocol ([vm-agent-protocol.md](vm-agent-protocol.md)), or missed a deadline the launcher holds it to, and the launcher force-stopped the VM (exit 5 in the launcher core). |
| `launcher_exited` | The launcher child exited without reporting why. |
| `runner_exited_without_job` | The JIT runner exited without ever being handed a job, by the guest agent's closed word ([vm-agent-protocol.md](vm-agent-protocol.md#the-runners-exit)), and the launcher stopped the VM (exit 6 in the launcher core). Not a finished job: the VM never reported `job_finished`. |

### Check codes

The closed set `failed` carries, in this order (what each checks is in
[the agent protocol](vm-agent-protocol.md#verify)): `console_user`,
`runner_not_admin`, `admins_locked`, `remote_login`, `filevault`, `apple_id`,
`software_update`, `xcode`, `runtimes`, `brew_owner`. A new code is a new
protocol version.

### Error codes

| Code | Meaning |
|---|---|
| `version_mismatch` | The handshake asked for a version the supervisor does not speak. Carries the one it does. |
| `handshake_required` | A request came before a successful `hello`. |
| `bad_request` | The request broke this document: an unknown field, a bound, a missing `tools`, a second `hello`. The supervisor closes after sending it. |
| `slot_busy` | `start` named a slot a VM still holds. |
| `claim_exists` | `start` named a claim a live VM already carries. |
| `unknown_claim` | No live VM carries the claim. |
| `wrong_state` | The request is not valid in the VM's current state. |
| `unmanaged` | The VM is `unmanaged`, and the supervisor will not act on it. |
| `image_not_found` | `image` names nothing in the image store. |
| `tools_not_found` | `tools` names nothing in the tools-disk cache. |
| `capacity` | The framework refused another VM. |
| `start_failed` | The VM could not be started for another reason. The detail is in the supervisor's own log, not on the wire. |
| `internal` | The supervisor failed in a way it cannot name. |

faber decodes an unknown code as a protocol error, like any other value outside
this document.

## The kept console logs

A guest's console is the operator's, never faber's. ADR-0049 §7's agent writes
one `FABER-GOLDEN-VERIFY` line per check to it, naming what decided the check
([the agent protocol](vm-agent-protocol.md#verify)), and the supervisor keeps it:

- **Every macOS child logs its console** (`--console-log`, capped at 1 MiB) to
  `console-slot<N>-<image>.log` in the state directory. The supervisor empties the
  slot's log before it starts a child, and at its own start deletes the log of every
  slot no live VM holds (a job's, left by a supervisor that died mid-job).
- **At teardown, a job that finished has its log deleted.** A VM that went through
  `verify` keeps its log as `verify-<image>-latest.log`; the one before is renamed
  `verify-<image>-<mtime>.log`, and only the latest five of an image are kept.
  Only regular files this account owns, with one link, are kept, renamed or
  deleted; a symlink is never followed.
- **A job VM that stopped without finishing keeps its log too**, as
  `job-<image>-failed-latest.log`, rotated the same way
  (`job-<image>-failed-<mtime>.log`, five in all) and apart from the
  verification's. That is a VM whose child never reported `job_finished` and that
  faber did not ask to stop: its `stop_reason` is `agent_error`, `wall_violation`,
  `guest_shutdown`, `start_failed`, `launcher_exited` or
  `runner_exited_without_job`. The log holds the agent's `FABER-AGENT handoff`
  line per step of starting the runner
  ([the agent protocol](vm-agent-protocol.md#the-hand-offs-console-lines)), and,
  once the runner has exited, its last lines
  ([the runner's exit](vm-agent-protocol.md#the-runners-exit)). A job
  that finished, or a VM faber stopped (`requested`: a reclaim, a timeout, any
  teardown), has its log deleted as before. faber's error for such a job names
  the stop reason and the command to read the log,
  `sudo cat -v <state>/job-<image>-failed-latest.log`.
- **A reclaim in service can be kept as a failed job.** faber withdraws the
  runner's registration before it sends `stop` (#402), and the withdrawal can
  make the runner exit without a job first. The VM then stops with
  `runner_exited_without_job` before faber's `stop` arrives, so its console
  becomes `job-<image>-failed-latest.log`, and the console of an earlier, real
  no-job failure moves to `job-<image>-failed-<mtime>.log`. That one is
  deleted only once five newer ones of the image are kept, and faber's own
  report of the run is unaffected: it says reclaimed. So when the latest log
  does not match the failure you are chasing, read the archived ones too. The
  supervisor does not tell the two apart, since nothing reaches it before the
  VM has stopped.
- **The state directory is this account's alone** (0700, each log 0600): on a
  host with the session account, `/var/db/faber-mac/state`, otherwise
  `_faber_vm`'s. Root and this account can read the log; `_faber` cannot, and
  **nothing of it crosses this wire**. faber's refusal text names the file and
  the operator's command to read it, `sudo cat -v <state>/verify-<image>-latest.log`
  (or `job-<image>-failed-latest.log`)
  (`cat -v`, so no guest byte reaches the terminal as a control sequence). No
  code path, root's or faber's, parses it.

## Claim markers

A claim marker is the supervisor's record of which claim runs in which launcher
child. Adoption, by faber and by a new supervisor alike, goes by markers, never
by a slot number, a path or a process name (AGENTS.md: read labels, never names).

- **It is a file in the supervisor's state directory**, `slot-<N>.claim` beside
  the launcher's `slot-<N>.lock`, owned by `_faber_vm` with mode 0600. The state
  directory is `_faber_vm`'s own, mode 0700, as the launcher core requires.
- **It is written before the child is spawned,** with `O_CREAT|O_EXCL|O_NOFOLLOW`,
  and removed only after the child has exited and its slot lock is free. A marker
  whose slot lock is free belongs to a VM that is gone: the supervisor finishes
  its teardown (deleting the clone, which carries the `faber-clone` marker) and
  reports `stopped` with `launcher_exited`.
- **Its contents** are one JSON object, which only the supervisor reads:

  ```json
  {"marker":"org.faber.kind=vm-claim","child_protocol":1,"slot":0,"kind":"macos","claim":"123456789-1760700000","pid":4242,"pid_started_us":1760700000123456,"started_unix":1760700000,"bundle":"clone-0"}
  ```

  - `marker` is the literal string shown. A file without it is not a marker, and
    the supervisor leaves it alone.
  - `child_protocol` is the version of the supervisor-to-child protocol the child
    was started with (below).
  - `pid` and `pid_started_us` identify the child together. `pid_started_us` is
    the process's start time in microseconds, as the kernel reports it
    (`kinfo_proc`'s `p_starttime`), read right after the spawn. The supervisor
    compares it with the live process's, so a reused pid is
    never adopted.
  - `bundle` is the clone's directory name inside the clone directory, for
    `macos` only. The sweep deletes a clone that carries the `faber-clone`
    marker and that no live claim marker names.
- **faber never reads a marker.** It sees markers only through `list` and
  `adopt`, in the vocabulary above.

## The versioning rule

Two protocols cross this boundary, and they are versioned apart:

- **faber ↔ supervisor, this document.** A faber and a supervisor from the same
  release speak the same version, and each speaks exactly one. Mismatched peers
  share only the frozen core.
- **supervisor ↔ launcher child.** Each per-VM launcher child serves its own
  socket in the state directory, and the supervisor talks to it over the same
  framing, starting with a `hello` that carries `child_protocol`. That version is
  recorded in the child's claim marker when it starts, and it never changes for
  that child's life, because the child keeps running the binary it started from.
  Its operations are the launcher's business (slice 3), and it is the one place a
  supervisor speaks more than one version. Child protocol 2 added `verify` and
  the `verifying` and `verified` states (with `failed`) and nothing else, so a
  supervisor speaks version 1 to an older child by never sending it `verify`.

When a supervisor starts, after an `exit` or a crash, it reads every claim marker
in its state directory and, for each live child:

1. **If it supports the marker's `child_protocol`,** it connects, handshakes at
   that version, confirms the child serves the marker's claim and slot, and
   **adopts** it. The VM appears in `list` in its real state.
2. **If it does not,** it leaves the child alone. The VM appears as
   `unmanaged`: it keeps its slot, it is never stopped, and its requests are
   refused with `unmanaged`. When the child's process exits, the supervisor
   tears down after it and reports `stopped` with `launcher_exited`.
3. **If the handshake fails, or the child names another claim or slot,** the
   supervisor treats the child as `unmanaged` too. It never stops a process it
   could not identify.

So **a new supervisor must speak every `child_protocol` that a supported older
launcher could have started.** A release that drops one says so in its notes;
the VMs on it then finish as `unmanaged`, and their jobs are not lost. New VMs
start on the new launcher at once (ADR-0026, amended 2026-10-17).

Changing this document follows the same rule. A change to a closed set or a
frame's shape is a new `protocol` version, and faber and the supervisor of one
release move to it together. The frozen core never changes.
