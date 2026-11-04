---
title: CLI output leads with a summary, and never relies on colour alone
type: adr
status: proposed
date: 2026-10-21
owner: rejifald
---

# ADR-0050: CLI output leads with a summary, and never relies on colour alone

## Status

proposed (2026-10-21). Nothing here is implemented. It decides how faber's
operator-facing commands present what they are about to do or have found:
`faber setup` (with and without `sudo`), `faber uninstall`, `faber golden
prepare` / `build`, `faber check-config` and `faber status`. It changes what is
printed, never what is planned or executed.

## Context

The owner ran `faber setup` without `sudo` on a Mac before phase A of the cut
(ADR-0025, ADR-0026), to review what it would do. Their verdict on the plan:
"Output unstructured, verbose, all blunt. Without color it's hard to review this
plan."

The plan for a fresh Mac, as `plan::render` prints it today for the test
fixture `fresh_mac()`, is 133 lines (the full text is under *Before* below).
Measured on that render:

- **No summary until the end.** The counts come last, after 13 sections, and
  nothing says what the run does to the machine as a whole: two hidden role
  accounts, two system services, the operator's own faber stopped and its
  folders renamed. The reader assembles that from 50 items.
- **Every item is two lines, and the second is often an implementation
  detail.** 33 of the 50 items print a full command line (`dseditgroup`,
  `sysadminctl`, `dscl`, `pmset`, `launchctl`) or an owner and mode
  (`_faber:_faber 750`). That is what a reviewer auditing root's actions wants,
  and not what an operator deciding "yes" needs first.
- **Nothing tells kinds apart at a glance.** `will create`, `will change`,
  `ok`, `skipped` and `refused` are words in the same column, the same weight,
  no colour. A `refused` among 40 `will create`s is easy to miss.
- **The footer under-reports.** It prints "33 to create, 8 to change, 0
  refused". The three `skipped` items, which are warnings (no VM launcher, no
  guest agent, so no VM supervisor), are not counted anywhere. An operator who
  reads only the last line does not learn that VM jobs will not run.
- **Lines are as long as their content.** 20 lines are over 100 columns, the
  longest 294. The terminal wraps them at column 0, under the status column.
- **The gaps mid-sentence are a source bug, not wrapping.** "the
  &nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;supervisor"
  comes from three string literals in `src/setup/vm_runtime.rs` (lines 631, 645
  and 913 on `main` at the time of writing) whose `\` line continuations were
  lost, so the next line's indentation became part of the text. A sweep of
  `src/` found no other literal of that shape.

The other commands share some of this. `faber uninstall` prints the same kind
of plan. `faber golden prepare` prints a recipe before a two-hour build.
`faber check-config` prints one line, or one line per error. `faber status`
already has a deliberate layout and `--json`. None of them uses colour, and none
has a `--verbose`.

What the repo already has:

- `src/term.rs` makes text faber did not write safe to print (escape sequences,
  bidi controls). Any styling has to sit on top of it, never replace it.
- `clap` with its default `color` feature already builds `anstyle`,
  `anstream`, `anstyle-query` and `colorchoice`, so they are in `Cargo.lock`
  today.

## Decision

### 1. Summary first, then grouped detail

Every command that prints a plan prints it in this order:

1. **A header**: the command, the host, and "nothing has changed yet" for a
   plan. Then the facts the plan rests on (operator, binary and version,
   layout), as today.
2. **The counts, on one line**: create, change, remove (uninstall), warnings,
   refused, each as its symbol, its number and its word ("+ 29 to create").
   Section headers carry their own counts the same way ("+ 4 create   ~ 1
   change   ! 3 warnings   ok 2"), never a bare symbol and number. A header's
   `ok` count counts every `ok` item in the section, listed or folded. Every kind that the plan
   contains is counted. A kind with zero
   items is still printed for `refused`, so "0 refused" is a statement.
3. **Refused items, if any, in full**, each with its reason and fix. A plan
   with a refusal stops here as far as the reader is concerned, so it is the
   first thing after the counts.
4. **At a glance**: at most about eight lines, each led by its kind's symbol
   and word like an item, one per thing that changes on the machine in a way a
   person would describe: "two hidden role accounts",
   "your per-user faber is drained and stopped". Warnings that leave a
   capability off ("VM jobs cannot run yet") are listed here too.
5. **The detail**, grouped in today's sections, in today's order. Each section
   header carries its own counts. One line per item.
6. **The counts again, as the second-last line**, with a hint that
   `--verbose` shows commands and modes. A long plan scrolls the top away, and
   the line the operator reads before deciding must be the whole verdict.
7. **One trailer, last**, saying what happens next. The first that applies
   wins:
   - without `sudo`: today's "This preview used read-only probes; nothing
     changed" and the command to apply it, plus, if anything is refused, that
     a real run would stop there;
   - `--dry-run`: "nothing changed", and how many refused steps a real run
     would stop at;
   - a refusal under `sudo`: "nothing was changed", no prompt, and a non-zero
     exit, as today;
   - nothing to create or change: "nothing to change: this host is set up" in
     place of the counts, then today's closing text, and no prompt;
   - otherwise the `Proceed? [y/N]` prompt.

   Exit codes are unchanged in every case.

The at-a-glance sentences are written by the plan code, not by the renderer:
each `Section` gains a function from its steps to zero or more summary lines.
The renderer never invents meaning; it lays out what the plan says. Tests assert
the summary for the fixtures, as they assert the steps today.

**One line per item, not per step.** Some items are several steps today: a role
account is "create user", "no password" and "hidden". A step gains an item key,
and steps that share one print as one line ("user _faber, uid 450, hidden, no
password"). The counts count items, the lines a person sees. `--verbose` lists
every step under its item.

**Within a section, refused items come first, then warnings, then the rest
in plan order.**

**An item is named by what it is on disk.** A file or directory setup writes,
moves aside, or would have written but skips is shown by its full path; a
service by its plist or unit path, with its label in the detail; an account
by its name and id. A short name ("the faber service") may go in the detail,
never in place of the path.

**Nothing that changes the machine is folded.** Every path, account and service
setup creates or changes is its own line, even when several are alike: a person
approving root's actions must be able to see each one, and a property that
matters for security ("only root can change the directories above it") stays
on its line in the default view.

**An `ok` item that describes something setup will still do** (the config
check runs as `_faber` before the service starts, and its failure undoes the
run) is listed like a change, because it bears on the decision. Every other
`ok` item is folded.

**A section whose items are all folded `ok` folds to its header**
("preflight   ok 2"). A section with a mix shows its `ok` count in the header
and lists only the rest. `--verbose` lists every `ok` item.

### 2. What is shown by default, and what `--verbose` adds

| Shown by default | Behind `--verbose` |
| --- | --- |
| What happens to each item, in words: create, change, remove | The exact commands (`dscl`, `sysadminctl`, `dseditgroup`, `launchctl`, `pmset`, `pfctl`) |
| The item: account, path, service label | Owner, group and mode of each path |
| Ids that identify an account (uid, gid) | Byte counts and the hash note on migrated files |
| Every refusal and warning, with its reason and fix, in full | `ok` items inside a mixed section |
| One clause of why, where the why changes the decision ("waits for admitted jobs") | The longer rationale clauses ("as for `_www`", "written last before the service starts") |

A refusal or a warning is never shortened by the default view. Those are the
lines a person must act on.

`--verbose` (`-v`) is added to `setup`, `uninstall`, `golden prepare`,
`golden build` and `check-config`. Verbose output is the full information of
today's output, laid out by the same rules.

Commands in verbose output are printed one argument list per line, prefixed
`$`, and broken only between arguments, with a trailing ` \` so a copied
command still runs. A path or argument is never split.

The content of the output depends only on `--verbose`, never on whether stdout
is a terminal. A plan piped to a file says the same thing as the one on the
screen, so a pasted log is what the operator saw.

### 3. Every line carries a word; colour only reinforces it

Each item line starts with a symbol and a word, in fixed-width columns. Colour
is applied to the symbol and the word, never to the item's text, and never
carries meaning on its own.

| Kind | Symbol | Word | Colour |
| --- | --- | --- | --- |
| create | `+` | `create` | green |
| change | `~` | `change` | cyan |
| remove (uninstall) | `-` | `remove` | red |
| refused | `x` | `refused` | bold red |
| warning (today's `skipped`) | `!` | `warning` | yellow |
| ok | (none) | `ok` | default foreground |

- **Only the eight basic ANSI foreground colours, plus bold.** No 256-colour
  or truecolor codes, no backgrounds. The operator's terminal theme decides
  the actual shades, so contrast follows the theme they already chose, light or
  dark. The hues match ADR-0037's status roles (`--green`, `--amber`, `--red`)
  by name only; the terminal does not read the token file, because its values
  are the terminal's to choose.
- **`ok` stays plain.** The plan's job is to make changes stand out; a page of
  green ticks would hide them.
- **Section headers are bold. Dim is used only for hints** that repeat
  something said elsewhere (the `--verbose` hint). Dim is unreadable on some
  themes, so nothing that is said only once is dim.
- **Symbols are ASCII.** They survive `LANG=C` under `sudo`, any font, a log
  file and a screen reader. A screen reader reads the word that follows, so the
  symbol is never the only cue.

### 4. How it degrades

Colour is decided per stream (stdout and stderr each on their own), by
`anstream`'s rules, which are the de facto conventions:

| Situation | Colour | Wrapping |
| --- | --- | --- |
| Interactive terminal | on | at the terminal's width, capped (see 5) |
| `NO_COLOR` set to anything non-empty; it wins over every variable below | off | unchanged |
| `TERM=dumb`, or `TERM` unset | off | unchanged |
| `CLICOLOR=0` | off | unchanged |
| Not a terminal: a pipe, `> file`, `tee`, launchd's log, CI | off | off: one item per line |
| `CLICOLOR=1`, on a terminal, even with `TERM=dumb` or unset | on | unchanged |
| `CI` set, on a terminal | on | unchanged |
| `CLICOLOR_FORCE` non-empty and `NO_COLOR` not (for `\| less -R`) | on | off |
| `--color always\|never\|auto` | as given; it overrides the environment | unchanged |

`--color` is a global flag on every command. It is needed because `sudo`
resets the environment by default (`env_reset`), so a `NO_COLOR` in the
operator's shell need not reach `sudo faber setup`. When setup re-executes the
CLI copy it just promoted (ADR-0034), it passes the same arguments, so
`--color` and `--verbose` carry over. Setup also runs `check-config` twice through
`tools::run_as`: on the source config as the operator, and on the migrated
config as `_faber`. Neither child has a terminal, `run_as` clears their
environment and pipes their output, and `tools::sanitize_child_output` would
show colour codes as literal `\x1b` text. `auto` already resolves to no colour
there, and setup passes `--color never` to both anyway, so nothing depends on
that.

No command draws a spinner, a progress bar, or moves the cursor. Long phases
(`golden build`, the drain) print appended lines. That keeps scrollback, `tee`,
a screen reader and a log identical, and keeps faber's own escape output to
one kind: fixed SGR colour codes.

### 5. Width

- **On a terminal**, the width is the terminal's columns (`ioctl(TIOCGWINSZ)`
  on the stream being written; `COLUMNS` if that fails, 80 if both do),
  clamped to between 60 and 100. A detail that does not
  fit wraps at a word boundary with a hanging indent, under the detail column.
  If the subject column leaves fewer than 30 columns, the detail moves to its
  own line. Paths, commands and arguments are never split; one that does not
  fit overflows.
- **Columns are fixed, not fitted.** An item line has three columns: kind
  (symbol and word, 9 wide), subject (24 wide plus a gap of at least two
  spaces), detail. The detail column is the same for every item in the output,
  whatever its section. A subject that does not leave a two-space gap puts its
  detail on the next line, at the detail column, never one space after the
  subject. Continuation lines of a wrapped detail start at the detail column
  too. Section-header counts start at one column for the whole output. A test
  renders every fixture and asserts that each detail starts at the detail
  column, whether on the item's line or the next. Item lines sit at these
  columns in every block, the refusal block included; only the at-a-glance
  sentences, which have no subject column, are indented under their heading.
- **Commands have their own two places.** In `--verbose`, an item's commands
  start under the subject column, each prefixed `$`. A command quoted inside a
  detail (a refusal's fix) starts at the detail column. Either way, a command
  broken between arguments continues four columns further in, so it stays
  visibly one command.
- **Not on a terminal**, nothing is wrapped. One item is one line, so `grep`
  finds whole items and the viewer wraps.
- **Source text carries no layout.** The renderer is the only place that
  inserts line breaks or alignment. A message string is one sentence with
  single spaces.

### 6. One small in-house module, on crates the build already has

A module `src/out.rs` owns the layout: the kinds above, sections, items, the
summary and counts, wrapping, and the colour and verbosity choice. It renders
to a `String` given an explicit style (colour on or off, a width or none,
verbose or not), so every command's output is unit-tested in plain mode, and a
test asserts that every coloured line still contains its word.

It uses:

- **`anstyle`** for the SGR codes, and **`anstream`** for the per-stream
  colour decision (`AutoStream::choice`), which implements `NO_COLOR`,
  `CLICOLOR`, `CLICOLOR_FORCE` and the `TERM` check. Both are already in
  `Cargo.lock` at the versions it already records, so declaring them adds no
  crate.
  Both are MIT OR Apache-2.0, from the `rust-cli` working group's
  `rust-cli/anstyle` repository; crates.io lists the working group's
  maintainers team and one maintainer whose GitHub profile is in the US.
  Neither is of Russian origin. Checked on crates.io and GitHub on 2026-10-21;
  the Ukrainian and Russian company registers do not apply, as neither crate
  has a company behind it.
- **No `libc`.** The one `ioctl(TIOCGWINSZ)` call is declared in-house in an
  `extern "C"` block, as `src/lock.rs` and the rest of the tree's FFI do: one
  call, one `struct winsize`, and the request constant per OS (`0x40087468` on
  macOS, `0x5413` on Linux), each behind its `cfg(target_os)`. `ioctl` is
  variadic in C and must be declared variadic (`...`), as `openat` is in
  `src/setup/fsx.rs`: Apple's arm64 ABI passes variadic arguments on the stack,
  so a fixed-argument declaration would read garbage there. Linux's half is
  verified in CI, as AGENTS.md requires.

Wrapping is in-house: the text is mostly ASCII, a few dozen lines cover the
rules in section 5, and widths are counted in `char`s. A path with wide
characters may then misalign by a column, which is accepted.

**`out.rs` sanitizes, then colours.** Today setup and uninstall pass the whole
rendered plan through `term::sanitize` at the point they print it
(`src/setup/mod.rs`). Colour codes cannot pass that sink, so the sink moves
into `out.rs`: every string it is given (subject, detail, summary, header
value) goes through `term::sanitize` there, whoever wrote it, and the fixed SGR
codes `anstyle` renders are added after. Callers that already sanitize lose
nothing, as `sanitize` leaves sanitized text unchanged. So root's terminal keeps
its one sanitizing layer over everything, and the only escape sequences in
faber's output are faber's own. Setup's filter for child-process output
(`tools::sanitize_child_output`) is unchanged.

### 7. Per command

- **`faber setup` / `sudo faber setup`**: all of the above. The footer counts
  warnings, so "3 warnings" is never silent again. Today's `skipped` is shown as
  `warning`, which is what its detail text already calls it.
- **`faber uninstall`**: the same shape, with the `remove` kind, and its summary
  names what is kept (`audit.jsonl` and `config.toml`).
- **`faber golden prepare` / `golden build`**: the recipe's summary comes first
  (Xcode version, download size, disk at peak, the time it takes), then its
  detail. Progress is appended lines, each a kind (`ok` for a finished phase,
  `!` for a warning, `x` for a stop).
- **`faber check-config`**: one counts line ("ok, 3 profiles" or "2 refused,
  1 warning"), then each problem as an item, with its reason. Exit codes are
  unchanged.
- **`faber status`**: its layout and `--json` stay as they are. It adopts only
  the colour of its state words (up, down, draining, standby) and of failed
  runs, through the same module, and the same degradation rules.

### 8. A lint for the class behind the gaps

The stray spaces came from a lost `\` continuation, which the compiler accepts.
A test scans `src/` for string literals containing a run of three or more
spaces between non-space characters, outside an allowlist of the deliberate
runs that exist today: column labels in `setup/plan.rs` (the header),
`status.rs`, `uninstall.rs`, `golden_prepare.rs` and `auth.rs`, the plist
indentation in `platform/service.rs`, the cloud-init YAML indentation in
`setup/linux_image.rs`, and test fixtures. Once `out.rs` does the alignment for
the commands in this ADR, their entries go. `auth.rs` stays until `faber auth`
adopts `out.rs`, and the plist and YAML indentation and the fixtures stay for
good: they are file formats, not messages.

## Before and after

The fixture is `fresh_mac()` in `src/setup/plan.rs`: a Mac with no faber
accounts, a per-user faber running, and no VM launcher beside the binary. The
account and home names are the fixture's. One change from the verbatim
text: a role account's directory-service node, which faber prints as `/Users/`
followed by the account's name, is written `/Users/<_faber>` here, because the
repo's privacy gate accepts `/Users/` only before a short list of generic
names.

### Before

<details>
<summary>Today's output, 133 lines, verbatim</summary>

```text
faber setup: faber runs as its own accounts (ADR-0025)

  operator  owner (uid 501), home /Users/owner
  binary    /usr/local/bin/faber (0.4.0+gabc1234)
  layout    SYSTEM: state in /Library/Application Support/faber, logs in /Library/Logs/faber

preflight
  ok           operator owner (uid 501)
               reads config, logs and the audit trail through group _faber
  ok           this binary, /usr/local/bin/faber
               nothing faber's own accounts can write

accounts
  will create  group _faber (gid 450)
               /usr/sbin/dseditgroup -o create -i 450 -r "faber daemon" _faber
  will create  group _faber_vm (gid 451)
               /usr/sbin/dseditgroup -o create -i 451 -r "faber VM host" _faber_vm
  will create  user _faber (uid 450, gid 450)
               /usr/sbin/sysadminctl -addUser _faber -UID 450 -GID 450 -shell /usr/bin/false -home "/Library/Application Support/faber" -roleAccount
  will create  _faber with no password
               /usr/bin/dscl . -create /Users/<_faber> Password "*"; /usr/bin/dscl . -delete /Users/<_faber> AuthenticationAuthority if it has one; then checked, as for _www
  will create  _faber hidden
               /usr/bin/dscl . -create /Users/<_faber> IsHidden 1
  will create  user _faber_vm (uid 451, gid 451)
               /usr/sbin/sysadminctl -addUser _faber_vm -UID 451 -GID 451 -shell /usr/bin/false -home /var/db/faber-vm -roleAccount
  will create  _faber_vm with no password
               /usr/bin/dscl . -create /Users/<_faber_vm> Password "*"; /usr/bin/dscl . -delete /Users/<_faber_vm> AuthenticationAuthority if it has one; then checked, as for _www
  will create  _faber_vm hidden
               /usr/bin/dscl . -create /Users/<_faber_vm> IsHidden 1
  will change  owner in group _faber
               /usr/sbin/dseditgroup -o edit -a owner -t user _faber

directories
  will create  /etc/faber
               root:wheel 755
  will create  /Library/Application Support/faber
               _faber:_faber 750
  will create  /Library/Application Support/faber/bin
               _faber:_faber 755
  will create  /Library/Application Support/faber/registry-auth
               _faber:_faber 700
  will create  /Library/Logs/faber
               root:_faber 750
  will create  /Library/Caches/faber
               _faber:_faber 750
  will create  /var/db/faber-vm
               _faber_vm:_faber 710

binaries
  ok           /usr/local/bin/faber
               this binary (0.4.0+gabc1234)
  will create  /Library/Application Support/faber/bin/faber
               this binary (0.4.0+gabc1234), owned by _faber: the copy the daemon runs and updates

per-user instances
  will change  owner's per-user faber
               running: drain its control socket or 127.0.0.1:8787 (waits for admitted jobs), stop gui/501/io.github.rejifald.faber, move /Users/owner/Library/LaunchAgents/io.github.rejifald.faber.plist aside to /Users/owner/Library/LaunchAgents/io.github.rejifald.faber.plist.pre-system-<time>

config and state
  will create  /Library/Application Support/faber/config.toml
               copied from /Users/owner/Library/Application Support/faber/config.toml (120 bytes; the original stays, its sha256 goes in the record)
  will create  /Library/Logs/faber/audit.jsonl
               copied from /Users/owner/Library/Logs/faber/audit.jsonl (4096 bytes; the original stays, its sha256 goes in the record)
  ok           source config check
               the config to migrate passes this binary's check-config, run as owner before anything is drained or stopped
  ok           config check
               /usr/local/bin/faber check-config runs on it as _faber before the service starts; a failure stops setup and restores what it stopped

log files
  will create  /Library/Logs/faber/stdout.log
               empty, _faber:_faber 640
  will create  /Library/Logs/faber/stderr.log
               empty, _faber:_faber 640

setup record
  will create  /etc/faber/setup.json
               root-owned 0644, written last before the service starts: it is what makes this host SYSTEM

VMs (ADR-0026)
  will create  /var/db/faber/bin
               _faber:_faber 755, under directories only root can change: the              supervisor (_faber_vm) runs the launcher in it, and a release swaps it              there
  skipped      /var/db/faber/bin/faber-vm
               warning: no launcher to install: none beside this binary (faber-vm or faber-launcher-aarch64-apple-darwin) and no --launcher PATH. VM jobs cannot run until it is installed; re-run setup with it
  skipped      /Library/Application Support/faber/bin/faber-agent
               warning: no guest agent to install: none named faber-agent or faber-agent-aarch64-apple-darwin in the launcher's directory. macOS VM jobs cannot build their tools disk until it is installed; re-run setup with the release's assets in one directory
  will create  /var/db/faber-vm/run
               _faber_vm:_faber 710
  will create  /var/db/faber-vm/state
               _faber_vm:_faber_vm 700
  will create  /var/db/faber-vm/clones
               _faber_vm:_faber_vm 700
  will create  /var/db/faber-vm/linux
               _faber_vm:_faber_vm 700
  will create  /var/db/faber/tools-disks
               _faber:_faber 755: the supervisor (_faber_vm) reads the tools disks _faber builds
  will create  /var/db/faber/images
               root:wheel 755, and every directory above it root's
  will change  /etc/faber/wall/plan.json
               16 VM networks: slots 0-13 macOS, 14 the Linux engine, 15 the image build, rendered once this run's config.toml is in place, from the macos-vm and docker profiles it names
  will change  /etc/faber/wall/stored.json
               the rules for that plan and /etc/faber/grants.toml, rendered once this run's config.toml is in place, from the macos-vm and docker profiles it names
  will create  /var/db/faber/wall
               root:wheel 755: only root writes the checker's verdict, and _faber reads it
  will create  /var/log/faber-wall.log
               empty, root:wheel 640
  will create  /var/log/faber-vm-supervisor.log
               empty, _faber_vm:wheel 640
  will create  /Library/LaunchDaemons/io.github.rejifald.faber.wall.plist
               root:wheel 0644: root runs /usr/local/bin/faber wall-daemon, enforcing the stored rules; logs to /var/log/faber-wall.log; plutil -lint, then launchctl enable, bootstrap and kickstart
  skipped      /Library/LaunchDaemons/io.github.rejifald.faber.vm-supervisor.plist
               warning: not installed: it runs the launcher, which is not installed

service
  will create  /Library/LaunchDaemons/io.github.rejifald.faber.plist
               plutil -lint, then launchctl enable, bootstrap and kickstart system/io.github.rejifald.faber; ready once the control socket answers

your old per-user files
  will change  /Users/owner/Library/Application Support/faber
               renamed aside to <name>.pre-system-<time> once the new daemon answers: the backup stays, and nothing faber reads stays in this home (--keep-legacy leaves it)
  will change  /Users/owner/Library/Logs/faber
               renamed aside to <name>.pre-system-<time> once the new daemon answers: the backup stays, and nothing faber reads stays in this home (--keep-legacy leaves it)
  will change  /Users/owner/Library/Caches/faber
               renamed aside to <name>.pre-system-<time> once the new daemon answers: the backup stays, and nothing faber reads stays in this home (--keep-legacy leaves it)

keep awake
  will change  system sleep and Power Nap
               pmset -a sleep 0; pmset -a powernap 0 (a sleeping Mac suspends every job)

Linux VM image
  ok           /var/db/faber/images/linux/disk.raw
               Debian 20261015-2632, provisioned by this release

33 to create, 8 to change, 0 refused
```

</details>

### After, default view

Exactly what a pipe or `NO_COLOR` prints; on a terminal the symbols and words
are coloured as in section 3 and section headers are bold. 110 lines, none over
100 columns, against 133 lines today. The counts are items: the four account
items stand for the eight account steps above. A run without `sudo` or with
`--dry-run` ends with its own trailer instead of the prompt (section 1).

```text
faber setup · plan for this Mac · nothing has changed yet

  operator  owner (uid 501)
  binary    /usr/local/bin/faber 0.4.0+gabc1234
  layout    SYSTEM: state in /Library/Application Support/faber, logs in /Library/Logs/faber

  + 29 to create   ~ 8 to change   ! 3 warnings   x 0 refused

  At a glance
    + create   two hidden role accounts, _faber and _faber_vm
    + create   faber and its VM wall as system services
    ~ change   you join group _faber, to read config, logs and the audit trail
    ~ change   your per-user faber is drained and stopped; its config and audit log are copied
    ~ change   your old faber folders are renamed aside once the new daemon answers
    ~ change   system sleep and Power Nap are turned off
    ! warning  VM jobs cannot run yet: no launcher or guest agent beside this binary

preflight                                     ok 2

accounts                                      + 4 create   ~ 1 change
  + create   group _faber              gid 450
  + create   group _faber_vm           gid 451
  + create   user _faber               uid 450, hidden, no password
  + create   user _faber_vm            uid 451, hidden, no password
  ~ change   group _faber              adds you (owner)

directories                                   + 7 create
  + create   /etc/faber
  + create   /Library/Application Support/faber
  + create   /Library/Application Support/faber/bin
  + create   /Library/Application Support/faber/registry-auth
  + create   /Library/Logs/faber
  + create   /Library/Caches/faber
  + create   /var/db/faber-vm

binaries                                      + 1 create   ok 1
  + create   /Library/Application Support/faber/bin/faber
                                       the daemon's own copy of this binary

per-user instances                            ~ 1 change
  ~ change   /Users/owner/Library/LaunchAgents/io.github.rejifald.faber.plist
                                       your per-user faber: drained (waits for admitted jobs),
                                       stopped, and this file moved aside

config and state                              + 2 create   ok 2
  + create   /Library/Application Support/faber/config.toml
                                       copied from your per-user install; the original stays
  + create   /Library/Logs/faber/audit.jsonl
                                       copied from your per-user install; the original stays
  ok         config check              run as _faber before the service starts; a failure
                                       stops setup and restores what it stopped

log files                                     + 2 create
  + create   /Library/Logs/faber/stdout.log
  + create   /Library/Logs/faber/stderr.log

setup record                                  + 1 create
  + create   /etc/faber/setup.json     written last; it makes this host SYSTEM

VMs (ADR-0026)                                + 11 create   ~ 2 change   ! 3 warnings
  ! warning  /var/db/faber/bin/faber-vm
                                       the VM launcher, not installed: none beside this binary
                                       (faber-vm or faber-launcher-aarch64-apple-darwin) and no
                                       --launcher. VM jobs cannot run until it is installed;
                                       re-run setup with it
  ! warning  /Library/Application Support/faber/bin/faber-agent
                                       the guest agent, not installed: none named faber-agent or
                                       faber-agent-aarch64-apple-darwin in the launcher's
                                       directory. macOS VM jobs cannot build their tools disk;
                                       re-run setup with the release's assets in one directory
  ! warning  /Library/LaunchDaemons/io.github.rejifald.faber.vm-supervisor.plist
                                       the VM supervisor service, not installed: it runs the
                                       launcher
  + create   /var/db/faber/bin         only root can change the directories above it
  + create   /var/db/faber-vm/run
  + create   /var/db/faber-vm/state
  + create   /var/db/faber-vm/clones
  + create   /var/db/faber-vm/linux
  + create   /var/db/faber/tools-disks
  + create   /var/db/faber/images      root's, and every directory above it root's
  + create   /var/db/faber/wall        only root writes the checker's verdict
  + create   /var/log/faber-wall.log
  + create   /var/log/faber-vm-supervisor.log
  + create   /Library/LaunchDaemons/io.github.rejifald.faber.wall.plist
                                       the VM wall service, run by root
  ~ change   /etc/faber/wall/plan.json
                                       16 VM networks, from config.toml's profiles
  ~ change   /etc/faber/wall/stored.json
                                       the wall's rules for that plan and /etc/faber/grants.toml

service                                       + 1 create
  + create   /Library/LaunchDaemons/io.github.rejifald.faber.plist
                                       the faber service; ready once its control socket answers

your old per-user files                       ~ 3 change
  ~ change   /Users/owner/Library/Application Support/faber
                                       renamed aside once the new daemon answers (--keep-legacy
                                       leaves it)
  ~ change   /Users/owner/Library/Logs/faber
                                       renamed aside
  ~ change   /Users/owner/Library/Caches/faber
                                       renamed aside

keep awake                                    ~ 1 change
  ~ change   sleep and Power Nap       off: a sleeping Mac suspends every job

Linux VM image                                ok 1

+ 29 to create   ~ 8 to change   ! 3 warnings   x 0 refused   (--verbose: commands and modes)
Proceed? [y/N]
```


### After, `--verbose`, one section

```text
accounts                                      + 4 create   ~ 1 change
  + create   group _faber              gid 450
             $ /usr/sbin/dseditgroup -o create -i 450 -r "faber daemon" _faber
  + create   user _faber               uid 450, hidden, no password
             $ /usr/sbin/sysadminctl -addUser _faber -UID 450 -GID 450 \
                 -shell /usr/bin/false -home "/Library/Application Support/faber" \
                 -roleAccount
             $ /usr/bin/dscl . -create /Users/<_faber> Password "*"
             $ /usr/bin/dscl . -delete /Users/<_faber> AuthenticationAuthority
               (only if it has one; then checked, as for _www)
             $ /usr/bin/dscl . -create /Users/<_faber> IsHidden 1
  ...
directories                                   + 7 create
  + create   /etc/faber                root:wheel 755
  + create   /Library/Application Support/faber
                                       _faber:_faber 750
  ...
```

### After, a refused plan

A refusal moves to the top, in full, and no prompt is offered. The fixture
here is `fresh_mac()` with a `_faber` user and group that already exist, the
user able to authenticate. The reason is the text `accounts.rs` and `plan.rs`
print today, laid out by section 5; only its line breaks and command layout
are new:

```text
faber setup · plan for this Mac · nothing has changed yet
  ...
  + 27 to create   ~ 8 to change   ! 3 warnings   x 1 refused

  Refused: setup will not run until this is fixed
  x refused  user _faber               _faber can authenticate: it has an
                                       AuthenticationAuthority (a ShadowHash password).
                                       Lock it with
                                       $ sudo /usr/bin/dscl . -delete /Users/<_faber> \
                                           AuthenticationAuthority
                                       $ sudo /usr/bin/dscl . -create /Users/<_faber> Password "*"
                                       Setup does not adopt an account of another shape.
  ...
x 1 refused: nothing was changed
```

## Delivery

In this order, each a PR of its own:

1. `src/out.rs` with its tests, `--color` on every command, and the fix for the
   three literals in `src/setup/vm_runtime.rs` with the lint of section 8.
2. `faber setup`: item keys, section summaries, the folded `ok` sections, the
   repeated footer, warnings counted, `--verbose`. The plan's tests keep
   asserting steps; new tests assert the rendering for `fresh_mac()` and
   `set_up_mac()` in plain mode.
3. `faber uninstall`, then `check-config`, then `golden prepare` / `build`,
   then `status`'s colours.

## Consequences

- An operator reviewing `sudo faber setup` reads the counts and the at-a-glance
  block first, and finds a refusal or warning without scanning 50 items.
- Auditing root's commands takes `--verbose`. The information is the same as
  today's; nothing that was printed is dropped from the verbose view.
- Plan text is now two things: a step's detail, and its section's summary. Both
  live in the plan code and both are tested, so the summary cannot drift from
  what the plan does without a test failing.
- Output that scripts parse is not a contract. `faber status --json` is, and
  stays so. A `--json` for setup's plan is not decided here.
- `out.rs` is one more place every command passes through, and the lint of
  section 8 adds an allowlist to maintain until the alignments move into it.
- No new crate is added to the build, and no new FFI dependency.

## Considered and not adopted

- **Fix the wrapping only, add no colour or summary.** It removes the
  mid-sentence gaps but not the owner's complaint: the plan stays 133 lines
  with its verdict at the bottom, and a warning stays uncounted.
- **Colour by kind with no symbol or word change.** Fails anyone who cannot
  tell red from green, every log file, and every `NO_COLOR` user.
- **Unicode symbols (`✓`, `✗`, `→`).** Nicer on a modern terminal, but they
  depend on the locale (`sudo` often runs with `LANG=C`) and the font, and can
  take two columns. ASCII plus a word carries the same meaning everywhere.
- **An interactive review (a TUI, a pager, collapsible sections).** It cannot
  be logged, pasted or read by a screen reader the same way, and setup runs in
  root's terminal, where faber should do as little to the terminal as possible.
- **`owo-colors`, `termcolor`, `console`, `colored`, `terminal_size`,
  `textwrap`.** Each would add a crate for something the build already has
  (`anstyle`, `anstream`) or that takes a few dozen lines here. `libc` is
  in `Cargo.lock` through `tokio`, but the tree declares its FFI in-house
  (`src/lock.rs`), and one `ioctl` is no reason to change that.
- **Wrapping when piped, at 80 columns.** It would make `grep` miss items split
  across lines, and a log viewer wraps anyway.
- **Different content on a terminal and in a pipe.** A pasted log would not be
  what the operator saw when they said yes.
