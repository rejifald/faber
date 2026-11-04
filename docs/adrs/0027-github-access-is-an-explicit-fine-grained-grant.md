---
title: GitHub access is an explicit, fine-grained grant
type: adr
status: accepted
date: 2026-09-28
owner: rejifald
---

# ADR-0027: GitHub access is an explicit, fine-grained grant

## Status

**Amended 2026-10-09 by
[ADR-0031](0031-releases-are-signed-by-github-from-a-fresh-public-repository.md).**
The cut release is the first release of a public repository, so the updater reads
releases anonymously from the start. The upgrade sitting drops `faber auth set
--updater`.

**Amended 2026-10-17 when it was built (#143, #187):**

- **The permissions table gains two reads.** The git mirror lists open pull
  requests, so it also needs Pull requests: read. Runner-image variants read a
  repository's lockfiles to pick a runner version, so they need Contents: read.
- **Checks: read cannot be granted to a fine-grained token.** Where GitHub
  refuses the annotations read, the runner-retirement warning is off for that
  repository, visibly. Auto-retry was too until 2026-10-18, when its trigger moved
  to faber's own runner-outcome evidence (ADR-0003, amendment 2026-10-18, #192); it
  no longer reads annotations.
- **A token GitHub reports no expiry for is refused.** GitHub sends the expiry
  header only for tokens that expire, so a missing header most likely means one
  that never does. `sudo faber auth set --expires YYYY-MM-DD` overrides that,
  with a warning that GitHub did not confirm the date. The cut sitting confirms
  the header on the first real token; if GitHub sends it, the override goes.
- **Linux uses the same file as macOS.** systemd's encrypted credentials
  wait until a Linux host runs faber (#214).
- **A token that acts as another account is refused**, at boot and on reload.
  The token file is owned by `_faber`, so `_faber` can rewrite it. With that
  check, a rewrite can only swap in another token for the same account, which
  gives an attacker nothing it does not already hold as `_faber`. So the file
  stays `_faber`'s. (Reversed 2026-10-18, below.)

**Amended 2026-10-18 (#189):**

- **The token file is root's, readable by `_faber`.** It is `root:_faber`, mode
  `0640`, in the root-owned `/etc/faber/credentials/`: the rule
  `/etc/faber/grants.toml` already follows, root writes and faber reads. Only
  `sudo faber auth set` writes it, atomically. `_faber` can no longer rewrite its
  own token, so the account check above is no longer the only thing between a
  compromised `_faber` and the file.
- **The reader** opens the file `O_NOFOLLOW` and requires owner root, group
  `_faber`, and no group-write or world bits. Anything else is refused with a
  reason.
- **A file in the old form is still read.** One owned by `_faber` at `0600`, as
  `auth set` wrote it before this amendment, is used, with a warning at every
  boot and a `file_warning` on `/api/state`'s credential, which the dashboard
  shows. `sudo faber setup` re-owns it in place, and `sudo faber auth set`
  rewrites it in the new form. Refusing it would be a release withdrawing a
  token a host relies on (AGENTS.md). Dropping the old form is a future,
  release-noted change.
- **Linux uses the same file.** systemd's encrypted credentials wait for
  [#214](https://github.com/rejifald/faber-legacy/issues/214).
- **A LEGACY host** (no setup record) has no store at all, as before.

**Amended 2026-11-01: the operator leaves `_faber`'s group.** Setup used to add
the operator to group `_faber` so they could read faber's config and logs. That
group is also the token file's, so every process the operator ran could read
the token, which carries Administration: write on every served repository.
"Where only `_faber` can read it" was not true.

- **The operator reads through a group of its own,** `_faber_operator`. It owns
  the state, cache and log trees and the files the operator reads in them
  (config, overrides, the logs and the audit trail), and on Linux it is the
  daemon's primary group (`Group=_faber_operator`, with
  `SupplementaryGroups=_faber`), so what the daemon writes takes it. `_faber`
  is a member too: on macOS the log directory is root's, and the daemon reaches
  its own logs only through that group. `_faber_vm` and `_faber_mac` are never
  in it.
- **Nobody but `_faber` is in group `_faber`.** `sudo faber setup` takes the
  operator out and refuses to go on while anyone else is in it, naming the
  command that removes them. The token stays `root:_faber` 0640.
- **Moving a host set up before this rotates the token.** A process keeps the
  groups it started with, so after setup moves the operator, every session must
  end (log out, or restart) before the old token is replaced: `faber auth
  link`, `sudo faber auth set`, then revoke the old token on GitHub. Setup's
  closing message says so.
- **The setup record names the group** (`accounts.operator_group`). A record
  from before it names none, which is how the daemon tells a host that has
  moved from one that has not.
- **The reader refuses a token others can read.** At boot and at every re-read
  of the store, the daemon reads the member list of the token file's group. On
  a host whose record names the operator group, a member besides `_faber`
  refuses the token, naming the member and the fix. With a record from before
  it, the operator is in that group by setup's own doing, so the token is used,
  with a warning at every boot and a `file_warning` on `/api/state`, as for the
  earlier file form: refusing it would stop a host that updated unattended
  (AGENTS.md). Turning that into a refusal is a future, release-noted change.
  The control socket is the operator group's, given it at every bind.

accepted (2026-09-28). Closes [#73](https://github.com/rejifald/faber-legacy/issues/73).
Depends on ADR-0025 for the `_faber` account that holds the token. ADR-0028
governs who may replace the token.

## Context

`token_from_gh` (`src/github.rs:732`) takes `$GH_TOKEN` or `$GITHUB_TOKEN`, else
runs `gh auth token`. On a `401` it runs `gh auth token` again (`refresh_token`,
`src/github.rs:784`). `faber setup` runs `gh auth login` for whoever installs it
(`src/setup.rs:239`). The same token serves the updater's reads of this
repository's releases (`src/update.rs`).

On the builder that token is the operator's own CLI login, an OAuth token with
scopes far broader than faber needs. Among other things, a login like it can:

- push to every private repository the operator can reach
- write workflow files, and a workflow can print a repository's secrets
- add SSH keys to the account, which keep working after the token is revoked

What faber actually uses (#73): Actions read and write, Administration read and
write, Checks read, Contents read, Commit statuses write, Metadata read.
`cancel_run` has no caller.

**The floor is high.** Every repository-level runner endpoint needs
Administration. Administration: write also covers deleting the repository,
changing its visibility, and managing deploy keys (checked against GitHub's
permission tables on 2026-09-25). No narrower repository permission exists for
runners. Only organization-level runners avoid it, under the organization
permission "Self-hosted runners".

## Decision

### A fine-grained personal access token, one per host and owner

Each host holds its own fine-grained PAT. By GitHub's rule, a fine-grained token
has exactly one resource owner. faber serves one owner today, and the token
store is keyed by owner, so adding a second one is an addition, not a redesign.

Callers ask one interface for "a token for this repository and this operation".
That seam is where a GitHub App can later replace the PAT without touching a
call site.

### Permissions follow the features that are on

| Feature | Permissions |
|---|---|
| Always | Actions: read, Administration: write, plus the Metadata: read every fine-grained token carries |
| Auto-retry (ADR-0003) | adds Actions: write |
| Drift detection (ADR-0001) | adds Contents: read, Commit statuses: write |
| Git mirror (ADR-0035) | adds Contents: read, Pull requests: read |
| Runner-image variants (ADR-0008, ADR-0010) | adds Contents: read |

A feature that is off asks for nothing. Turning one on later does not need a new
token: a fine-grained token's permissions can be edited on GitHub, and faber
says which box to tick.

### faber stores the token where only `_faber` can read it

The token lives in a file owned by root with group `_faber`, mode `0640`
(amended 2026-10-18; it was `_faber`'s own, `0600`), outside `/Users` and
excluded from Time Machine. Nobody but `_faber` is in that group: the operator
reads faber's config and logs through `_faber_operator` instead (amended
2026-11-01). Linux uses the same file for now. systemd's encrypted credentials
wait for a real Linux host (#214).

Once faber is signed with a Developer ID, the token moves to a keychain entry
that only the faber binary can read. That waits for signing, because without a
stable signature a keychain access list breaks on every self-update.

The token is never stored or passed anywhere else:

- **Never in `config.toml`**, which the settings API serves and rewrites.
- **Never in an environment variable**, which every child process inherits. faber
  stops reading `$GH_TOKEN` and `$GITHUB_TOKEN`.
- **Never on a command line**, which `ps` shows to every account.
- **Never in a log, the audit log, or a runner's environment.**

### The operator hands it over, explicitly

1. `faber auth link` prints a GitHub link that pre-fills a new fine-grained
   token. It sets the name `faber-<host>`, a description, the owner, the
   lifetime, and one parameter per permission the enabled features need. GitHub
   shipped template links for fine-grained tokens on 2025-08-26. The operator
   picks the repositories on GitHub's own page. The link cannot pre-select them,
   and that choice is the grant.
2. `sudo faber auth set` reads the token from a hidden prompt or from stdin,
   never from an argument. It then:
   - accepts only fine-grained tokens (`github_pat_`), refusing classic and
     OAuth ones (`ghp_`, `gho_`)
   - lists the repositories the token reaches and checks it can read them
     (writes cannot be checked without side effects)
   - stores the token
   - reloads it without a restart
   - clears the clipboard it was pasted from

Replacing the token widens trust. A substituted token can point faber at someone
else's repositories, whose jobs would then run on this host. So replacing it
needs proof that the operator is present. Today that proof is `sudo`. When
ADR-0028's second phase lands, the tray app can do the same with Touch ID.

### The updater has its own token, or none

While this repository is private, the updater reads releases with its own
fine-grained token: Contents: read on this repository only, set with `sudo faber
auth set --updater`. Once the repository is public, it reads anonymously.
Without a token, it pauses updates visibly.

It never borrows the CI token. Fine-grained permissions apply to every
repository a token selects, so a CI token that could also read releases would
carry Administration: write on faber's own repository.

### Tokens expire, and faber says so first

The link pre-fills a 90-day lifetime. The lifetime is configurable from 1 to 366
days, GitHub's range. A token that never expires is refused: an unnoticed leak
of one has no upper bound, and expiry is the backstop for exactly that.

faber learns the expiry from GitHub's response, if GitHub reports it; that is to
be confirmed on the first real token. Otherwise `auth set` asks for the date
GitHub showed. faber warns 14 days ahead in the dashboard, the tray, `faber
status` and the boot log, and daily from 3 days out. To rotate, use GitHub's
"Regenerate token", which keeps the repositories and permissions, then run `sudo
faber auth set`.

At expiry or revocation, faber:

- stops claiming new jobs
- lets running jobs finish
- shows a distinct "credential invalid" state

It never falls back to `gh`, and never crash-loops into ADR-0006's rollback.

### A repository is served only if the token reaches it and it opts in

The token's repository list is the ceiling, and GitHub enforces it. Inside it,
faber serves a repository only if it also carries the `faber` topic. That is
today's opt-in. It belongs to the repository's own admins, who can see it and
remove it without touching the operator's token.

The topic check is not configurable (amended 2026-10-18, #188). An earlier
draft proposed a `require_topic` key. The owner dropped it: the token's
repository list is already the explicit grant, and the topic is a second opt-in
that belongs to the repository's own admins. Turning it off would remove a
defence and save only clicks. A real need comes back as an ADR of its own.

faber flags two mismatches:

- a repository granted but not opted in: "remove it from the token"
- a repository opted in but not granted

The first is the only way an over-broad token becomes visible, because faber
cannot read a fine-grained token's permissions.

Every write faber makes with the token is recorded in `audit.jsonl` with its
reason: minting or deleting a runner, re-running a job, posting a status. On
GitHub the token acts as the operator, so faber's own log is where its actions
are told apart from the operator's.

### The switch is a hard cut, run by hand

The release that ships this removes `gh` entirely. A host without a stored token
boots normally, claims nothing, and says why.

That is a deliberate exception to AGENTS.md's rule that a release must not
withdraw a capability a host relies on. It is acceptable here because the fleet
is two hosts under one operator. ADR-0025's accounts need `sudo` on every host
anyway, so each host gets a hands-on visit regardless.

Two steps keep it safe:

- **Followers are paused first.** A follower on v0.3.11 checks only `SHA256SUMS`
  before installing, so it would install the cut release unattended. It would
  then have no token to claim jobs with, and an updater that cannot read the
  private release repository, so it could not even receive a fix. Before the
  release is published, every follower gets `[update] enabled = false`.
- **Each host is upgraded in one sitting:**
  1. Install the release.
  2. Run `sudo faber setup`.
  3. Run `faber auth link`, then `sudo faber auth set` and `sudo faber auth set
     --updater`.
  4. Confirm that `/api/state` reports the stored credential.
  5. Re-enable updates.

## Consequences

- faber holds a token that reaches only the repositories the operator selected,
  with only the permissions its enabled features use, for at most the lifetime
  the operator chose. It can no longer read or write anything else the operator
  owns.
- The Administration floor remains. The token can still delete a served
  repository, make it public, or add a deploy key to it. Only moving the served
  repositories into an organization removes that, and this ADR does not.
- The PAT gives up four things, recorded here as the reasons to revisit it:
  - **No per-call narrowing:** every call carries Administration: write.
  - **No introspection:** faber cannot list a fine-grained token's permissions,
    only probe reads.
  - **No identity of its own on GitHub:** reruns and statuses appear as the
    operator.
  - **Manual rotation**, once per lifetime.
- #73 is closed by this ADR.
- `faber setup` stops running `gh auth login`, and `gh` is no longer a
  dependency.

## Alternatives considered

**A GitHub App per operator**, created from a manifest faber fills in. It is
better on every principle that matters here:

- tokens that last an hour and can be narrowed per call
- an identity that is not the operator's
- installations faber can inspect
- a separate key per host

The cost is code (JWT signing, token exchange and caching) and a setup flow. It
is deferred, not rejected: the seam above exists so that it can replace the PAT.

**One public GitHub App that every faber user installs.** An App has one set of
private keys for all of its installations, and whoever holds one can mint a
token for any of them. Shipping the key to hosts would let any compromised host
reach every user's repositories. Holding it centrally would make faber a hosted
service with write access to all of them.

**User access tokens from a public App's device flow.** No key would sit on the
host, but the approach has four problems:

- Every installation grants the union of permissions any faber feature needs.
- The token acts as the user.
- Revoking from GitHub is all-or-nothing per account.
- A public client ID invites device-code phishing.

**A `gh` fallback with a scheduled cut-off.** This is AGENTS.md's usual shape: a
warning at every boot until a later release removes `gh`. The owner rejected it
in favour of a hard cut with the safeguards above.

**A one-release bridge.** Release N would add `auth set` alongside `gh`, and
release N+1 would remove `gh`. It is safer against a forgotten host, but was
rejected in favour of one release upgraded by hand.
