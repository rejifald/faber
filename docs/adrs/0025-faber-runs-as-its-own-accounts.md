---
title: faber runs as its own accounts, never as the operator
type: adr
status: accepted
date: 2026-09-28
owner: rejifald
---

# ADR-0025: faber runs as its own accounts, never as the operator

## Status

**Amended 2026-10-09 by
[ADR-0031](0031-releases-are-signed-by-github-from-a-fresh-public-repository.md).**
The cut no longer needs `ACCEPTED_SIGNERS`. GitHub signs releases, and the
follower checks a compiled-in attestation policy instead. The Consequences below
predate this.

**Amended 2026-10-25 by
[ADR-0051](0051-macos-vms-start-inside-a-login-session.md).** On macOS a third account,
`_faber_mac` (uid 600–699, not a role account, with a password and shell `/sbin/nologin`),
holds a login session that runs the macOS VM supervisor. It never holds a credential. macOS
auto-login of that account is an explicit opt-in, only with FileVault off. "No visible
account and no auto-login" below holds for `_faber` and `_faber_vm` only.

**Amended 2026-10-18:** the VM launcher lives in `/var/db/faber/bin`, outside the
state directory, so `_faber_vm` can run it (see "faber runs as hidden role
accounts").

**Amended 2026-11-01 by
[ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md)'s amendment of
that date.** The operator is no longer in `_faber`'s group, which reads the token.
The state directory is `_faber:_faber_operator` 0750: the operator and `_faber`
are in `_faber_operator`, and `_faber_vm` is in neither group, so it still cannot
enter it.

accepted (2026-09-28). Resolves [#72](https://github.com/rejifald/faber-legacy/issues/72).

First of four ADRs decided together. The others are ADR-0026 (job code runs only
inside VMs faber owns), ADR-0027 (GitHub access is an explicit, fine-grained
grant) and ADR-0028 (the control API knows its caller). This one carries the
threat model the other three cite. All four ship in one release, built in the
order 0025 → 0026 → 0027 → 0028.

## Context

faber acts with the operator's identity. Nothing in faber asks for that; it
inherits it.

- **The daemon runs as the operator.** On the builder it is a LaunchAgent in the
  operator's GUI domain (`src/platform/service.rs`), and its GitHub credential is
  whatever `gh auth token` returns for that user (`src/github.rs:732`). There that
  was an OAuth token with the broad scopes of a personal CLI login, far beyond
  what faber needs: every private repository the operator can reach, not the
  handful faber serves. ADR-0027 deals with the credential; this ADR deals with
  whose account it lives in.
- **Native jobs run as the operator too.** `src/runner.rs:289` spawns `run.sh` as
  the daemon's own user, with the daemon's whole environment. A job step can run
  `gh auth token` for the operator's full login, or sign through the operator's
  ssh-agent, which on the builder holds the ADR-0015 release key (#72). One
  compromised dependency in a job routed there could publish a signed release
  that every follower installs.
- **Docker jobs reach the operator's files.** Docker Desktop runs as the
  operator, and a granted dind is root inside its VM, which serves bind mounts
  from the host's `/Users` (#43, #72).
- **The isolation that exists was never adopted, and would not be enough.**
  `deploy/setup-ci-user.sh` creates a separate `ci` account so that jobs cannot
  read the operator's keychain, `~/.ssh` or `gh` token. The builder does not use
  it, and where it is used, faber and its jobs share that one account.

### Who faber defends against

Ranked, because each principle below turns into different requirements
depending on the adversary:

1. **Code running in a job**, including every dependency it installs. faber
   exists to run it. ADR-0013's "trusted code only" trusts a repository's
   authors, not their dependency trees, and one malicious install script is
   enough.
2. **Other processes on the host**, including ones running as the operator: a
   compromised tool, anything that can open a loopback socket or read a file.
3. **Later:** a malicious faber release, which ADR-0006 carries to every host,
   and credential leakage through logs, backups or a copied config.

Out of scope: root on the host, GitHub itself, and the operator.

The invariant that follows: **faber never leaks or exposes a credential it
holds.** One credential reaches job code by design: the JIT config, which lets a
runner register once. It is the job's credential, not faber's.

### The principles

The operator's four are least privilege, granularity (by repository, by feature,
by host and in time), explicit consent, and revocability. The four ADRs also
apply these:

- **No borrowed identity.** faber holds only what was handed to it for its own
  use.
- **GitHub enforces the ceiling, not faber.** faber's own config is writable by
  whatever can reach its API (ADR-0028), so it may narrow a grant but never
  define one.
- **Attributable.** Every write faber makes with a grant is recorded in
  `audit.jsonl`.
- **Checkable.** faber can say what it holds, and flag what it holds but does not
  use.
- **Fails loud and narrow.** A missing or expired permission turns off the
  feature that needs it, visibly. There is never a silent fallback, and never a
  crash loop against ADR-0006's rollback.
- **Separate credentials for separate jobs.** Running CI, reading releases and
  signing releases never share a credential.
- **Secrets are not config.**
- **Configurable within a ceiling**, ADR-0014's shape: an operator may narrow,
  and invariants do not move.

## Decision

### Job code never runs as the operator or as faber

That is the whole rule, and it does not depend on the container runtime. It has
one corollary: a VM counts as the wall between job code and an account only if
it shares none of that account's folders. ADR-0026 builds that wall and moves
every job behind it.

### faber runs as hidden role accounts

faber uses two macOS role accounts. Each is hidden, has no password, no shell
and no login session, and is created with `sysadminctl -addUser … -roleAccount`.
The name must start with `_` and the UID must fall in 450–499; both were checked
on the builder.

- **`_faber`** runs the daemon and holds its credentials (ADR-0027). It runs
  nothing else.
- **`_faber_vm`** runs the host-side processes of faber's VMs (ADR-0026). It never
  holds a credential. Those processes handle traffic from the guest, where job
  code has root, and the account that parses guest-controlled input must not be
  the account that can read a token.

faber becomes a LaunchDaemon in the system domain, started at boot with nobody
logged in. There is no visible account and no auto-login anywhere in the design.
On Linux the same split uses ordinary system users.

**Why not one account.** An account shared with job-facing code exposes more
than files. Whatever runs as faber's account can edit faber's config, reload its
service definition, and run the faber binary with arguments of its choosing.
`faber provision` would mint it a runner on any repository the credential
covers, and that runner would take that repository's next job, secrets included.
A separate account turns "can read" and "can steer" into "can do neither".

**Amended 2026-10-18: where the launcher lives.** `_faber`'s state directory,
`/Library/Application Support/faber`, is `_faber:_faber` 0750, and `_faber_vm`
is never in `_faber`'s group, so `_faber_vm` cannot enter it. It must still
execute ADR-0026's launcher, so the launcher lives in `/var/db/faber/bin`, not
in the state directory's `bin/`. Every directory above `bin/` is root's and
writable by nobody else; `bin/` and the launcher are `_faber`'s 0755, and setup
refuses any other chain. Being `_faber`'s lets a release swap the launcher, with
its staged copy and `.prev` in the same directory. It also lets `_faber` replace
the code `_faber_vm` runs, which the updater already required, so trust is
unchanged. The supervisor's own files and the VM state stay `_faber_vm`'s, in
its home.

### The operator creates the accounts, once

`sudo faber setup` creates both accounts and the LaunchDaemon, showing its plan
first as `faber setup` already does. It runs from a terminal the operator
already trusts with administrative work, such as Terminal.app, or from an
installer package once faber is signed. It is never run by faber itself, and
never by an agent.

This was learned rather than assumed. On 2026-09-27 a throwaway role account,
created with `sudo` from a terminal hosted inside another application, was
created without any further prompt. Deleting it was refused
(`eDSPermissionError`). macOS checked the deletion against "administer your
computer" and Full Disk Access (`kTCCServiceSystemPolicySysAdminFiles`,
`kTCCServiceSystemPolicyAllFiles`) for the application hosting the terminal, and
denied it. Granting those rights to a general-purpose application in order to
remove one account is the wrong trade, so faber's uninstall path runs from the
same place its setup does.

### Runners get an allowlisted environment

A runner process gets only the variables it is given explicitly, inside a VM
after ADR-0026, and during the migration wherever it runs. Nothing is inherited:
not `SSH_AUTH_SOCK`, not `GH_TOKEN`, not whatever the service manager happened
to set. This is a speed bump rather than a boundary, since the boundary is the
account and the VM. It removes the cheapest route a secret has taken into a job.

### On Linux, dind waits for a wall

A granted dind is a privileged container on the host's own Docker Engine, which
is root on the machine (ADR-0013). No account split protects `_faber`'s files
from it. So a Linux host that holds a credential refuses dind until it runs its
jobs behind a wall (ADR-0026).

A host that grants dind today keeps it, per AGENTS.md:

- a warning at every boot and in the dashboard
- rejection where a human is looking: `faber check-config` and the settings form
- a hard refusal in a later release whose notes say so

## Consequences

- **#72 is resolved.** No job runs as the operator, so the operator's ssh-agent,
  and the release key in it, are out of every job's reach. That unblocks setting
  `ACCEPTED_SIGNERS` (ADR-0015), which the release shipping these ADRs needs.
- **The deploy story changes.** `sudo faber setup` replaces both documented
  options: the LaunchAgent under the operator's account, and the `ci` account
  from `deploy/setup-ci-user.sh`. The deploy docs, the README and AGENTS.md
  change with the implementation.
- **The tray app stays the operator's.** It runs in the operator's session and
  reaches faber only through the API, which ADR-0028 authenticates.
- **faber no longer needs anyone logged in.** Today's LaunchAgent relies on an
  auto-login session; a LaunchDaemon does not.
- **Uninstalling needs the same trusted terminal as installing.** The asymmetry
  above is a macOS rule faber works with, not around.

## Alternatives considered

**The existing `ci` account for faber and its jobs together.** It protects the
operator's credentials from jobs, which is most of #72. It leaves faber's own
credential readable by native jobs, and by any dind job through Docker Desktop's
file sharing. It also leaves faber steerable by its own jobs.

**A keychain entry readable only by the faber binary**, so that faber and its
jobs could share an account. It protects the token's bytes but not the process
that uses it: faber becomes the deputy of whatever runs beside it. It also needs
a stable code signature, because without Developer ID signing a keychain access
list breaks on every self-update. ADR-0027 adopts the keychain as storage once
faber is signed; it does not replace the account split.

**A visible job account with auto-login.** It would be needed for as long as
Docker Desktop is the runtime, because Docker Desktop is a GUI application. It
was rejected along with Docker Desktop itself (ADR-0026).
