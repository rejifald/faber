---
title: Releases are signed by GitHub, from a fresh public repository
type: adr
status: accepted
date: 2026-10-09
owner: rejifald
---

# ADR-0031: Releases are signed by GitHub, from a fresh public repository

## Status

accepted (2026-10-09), amended 2026-10-11 with what the pre-implementation
spikes found, and 2026-11-03 with how the repositories moved (the last
section). Supersedes
[ADR-0015](0015-releases-are-signed-on-the-builder.md)'s signing key. ADR-0015's
other rules stand:

- verification fails closed
- it runs before any binary is fetched
- the trust root is compiled into the follower

Amends:

- [ADR-0006](0006-fleet-autoupdate.md): the release decision is a Publish click,
  a follower waits before it installs, and the builder no longer cuts releases.
- [ADR-0025](0025-faber-runs-as-its-own-accounts.md): the cut needs this ADR's
  policy, not `ACCEPTED_SIGNERS`.
- [ADR-0027](0027-github-access-is-an-explicit-fine-grained-grant.md): the updater
  needs no token.
- [ADR-0028](0028-the-control-api-knows-its-caller.md): which update-channel
  changes count as trust-widening.

It takes effect with the release that ships ADR-0025..0028, which becomes the
first release of a new public repository.

- It settles [#91](https://github.com/rejifald/faber-legacy/issues/91) for this path.
- It makes [#99](https://github.com/rejifald/faber-legacy/issues/99) part of the trust
  model, and settles #99's open points.
- [#100](https://github.com/rejifald/faber-legacy/issues/100) lands before the cut.

## Context

ADR-0015 made an operator-held ssh key the fleet's trust root, on the reasoning
that GitHub access alone must never yield a release a host will install. Running
it surfaced costs the owner does not want to carry.

- **The key needs a home no CI job can reach.** On the builder, jobs ran as the
  operator, and the key sat in the agent they could use
  ([#72](https://github.com/rejifald/faber-legacy/issues/72)). ADR-0025 moves jobs away
  from the operator. Even so, the key would still have to be:
  - generated
  - kept from CI
  - backed up off the machine
  - rotated through a release it signs itself
  - recovered if the Mac dies

  None of that was ever finished. `ACCEPTED_SIGNERS` is still empty, and nothing
  has shipped since v0.3.11 (2026-08-28).
- **It never proved the build.** The builder cannot rebuild the Linux assets. The
  signature therefore meant "the operator meant this", never "these bytes came
  from that commit", as ADR-0015 says itself.

ADR-0015 rejected GitHub's own keyless attestation, and the only thing ever
blocking it was the repository being private and user-owned
([#50](https://github.com/rejifald/faber-legacy/issues/50)). faber is going public
anyway.

**The owner's position.** faber has one maintainer. That GitHub account already
decides every merge, and the only person who would ever use a release key is the
same person. So the account is accepted as the release authority, and no key is
held anywhere.

Three facts shape how that works.

1. **On GitHub Free, a private repository has none of the controls this needs:**
   no rulesets, no branch protection, no environments and no attestations. The API
   answers "Upgrade to GitHub Pro or make this repository public to enable this
   feature." GitHub Pro would add the first three, but attestations on a private
   repository need GitHub Enterprise Cloud. A public repository has all four, free.
2. **This repository's history cannot be published cleanly.** On 2026-10-09 the
   digests from `tests/privacy.rs` were run over a mirror clone (#100):
   - 37 of 235 commits introduce a denylisted name.
   - Those commits are reachable from 62 of 63 `refs/pull/*` and from all 15 tags.
   - 20 issue and PR texts, and one release's notes, carry one too.
   - No API deletes a `refs/pull/*` ref.

   The tip of `main` is clean, and so is the whole history for credentials.
3. **The release that ships ADR-0025..0028 is already a hard cut,** installed by
   hand on every host (ADR-0027). It is the one moment when changing where hosts
   look for releases costs nothing extra.

## Decision

### faber restarts as a fresh public repository at the auth cut

At the cut:

- This repository is renamed `rejifald/faber-archive` and stays private. It keeps
  its history, issues, PRs and releases.
- A new public repository takes the name `rejifald/faber`.
- The new repository's first commit is the tree of the cut release. The privacy
  gate is green on it, and #100's scan finds nothing in it.

Followers need no change of channel. `[update] repo` defaults to
`rejifald/faber`, and after the rename that name belongs to the new repository.

**At that moment, every existing clone is a hazard.** Its `origin` keeps the URL,
which then reaches the new public repository. One `git push` of an old branch,
from any worktree or agent session, would publish private history, and that
cannot be undone. So two things happen before the name is reused:

- Every clone is re-pointed at the archive.
- The new repository is created private, and made public only after its refs are
  audited (see the cut, below).

The cut release is **v0.4.0**. A hard cut is worth a minor bump. Starting above
every version the archive ever tagged (its highest is `v0.3.12`, never released)
means no host ever compares against a version that went backwards.

Docs link to this repository's issues and PRs by URL. After the rename, those
URLs would land on the new repository's unrelated numbers. So before the push,
every such link is rewritten to the archive. The rewritten links resolve for the
owner and 404 for everyone else, which is honest about where that discussion
lives. Open issues that are still wanted are re-filed in the new repository, each
one first passing #100's scan.

### GitHub signs every release, and nobody holds a key

`release.yml` attests its assets with `actions/attest-build-provenance`:

- The signing certificate lasts minutes and is issued for the workflow run's own
  OIDC identity.
- Every signing is recorded in Sigstore's public transparency log.
- There is no key to generate, store, back up, rotate or lose.
- The release path holds no secret at all, so a fork's pull request has nothing to
  reach.

The bundle for `SHA256SUMS` ships as a release asset, `SHA256SUMS.sigstore.json`.
Verifying a release then needs only the release itself, never GitHub's
attestations API.

The attest step stays in a job defined in `release.yml` itself. Moving it into a
reusable workflow would change the signer the policy pins, and that is a policy
change (below).

### What a follower verifies

The trust root stays compiled into the follower, which is ADR-0015's property.
What used to be `ACCEPTED_SIGNERS` becomes a policy:

| Field | Required value |
|---|---|
| Issuer | `https://token.actions.githubusercontent.com` |
| Source repository | the new repository's **numeric ID**, not only its name |
| Signer workflow | `https://github.com/rejifald/faber/.github/workflows/release.yml@refs/heads/main` |
| Source ref | `refs/heads/main` |
| Source commit | the commit the release's tag points to |
| Runner | GitHub-hosted |
| Subject | the digest of the downloaded `SHA256SUMS` |

- **The numeric ID is pinned because names move.** The archive is the proof, and
  a renamed or re-created repository must not inherit the trust.
- **The source commit is resolved from the tag when the release is verified.**
  Immutable releases mean a published tag never moves.
- **The runner pin covers the job that signs.** The build jobs' runners are fixed
  by `release.yml` on protected `main`.

A follower then checks, in order:

1. **The bundle**, before any binary is downloaded:
   - the certificate chain to Sigstore's root
   - transparency-log inclusion
   - every field of the policy above
   - that the digest of `SHA256SUMS` is a subject of the attestation
2. **Every asset's digest** against `SHA256SUMS`, as today.
3. **The staged binary's `--version`.** Its version must equal the tag's exactly,
   with no pre-release suffix. Its `+g<sha>` build suffix must also be a prefix of
   the tag's commit.

Steps 1 and 3, with the source-commit pin, are #91's binding. An older attested
build, republished under a newer tag or swapped into a draft, carries the wrong
version or the wrong commit and is refused.

Then `check-config` and the swap run as they do today.

**The Sigstore trust root.** It is refreshed through Sigstore's TUF repository,
anchored on a TUF root compiled into the binary, which is how every Sigstore
client works. So a Sigstore key rotation between two releases cannot strand a
follower. The trusted root covers everything Sigstore publishes for verification:
certificate authorities, transparency logs and timestamp authorities. A move to
timestamp-based logging therefore changes nothing here. If TUF or Sigstore is
unreachable, updates pause, visibly. Verification is never skipped.

**The verifier runs inside faber.** ADR-0027 removes `gh`, so `gh attestation
verify` is not available to faber.

- **The verifier is `sigstore-verify`** (from `sigstore/sigstore-rust`), pinned
  to an exact version, subject to the owner's origin check for new dependencies,
  as Lima was (ADR-0026). The older `sigstore` crate does not verify attestations.
  A spike on 2026-10-11 verified a real GitHub provenance bundle with it, end to
  end and offline, and refused every mutated policy field and every tampered
  bundle (see *What the spikes found*).
- faber writes no cryptographic check itself. It compares the certificate's CI
  claims, which the crate has already chain-verified, against the policy.
- **If no verifier running inside faber can verify GitHub's bundles end to end,
  the cut ships with updates unavailable** and hosts stay on hand installs. It
  never ships a weaker check.

**Changing the policy is a trust-root change.** Renaming `release.yml`, moving the
attest step, or releasing from another branch ships the way a key rotation would
have. It goes out in a release that is verified under the old policy and carries
the new one.

### Builds come only from protected `main`

The new repository's rules:

- **`main`** changes only through a pull request with CI green. It cannot be
  force-pushed or deleted.
- **`v*` tags** are created only by the owner, and are never moved or deleted.
- **Immutable releases** are on, so a published release's assets and tag never
  change.
- **The default workflow token** stays read-only. Only `release.yml` asks for
  more: `contents: write` for the draft, and `id-token` plus `attestations` to
  sign.
- **Release builds restore no cache.** Every workflow on a branch shares that
  branch's caches, and code running in any of them can write an entry under a key
  the release would later restore. A release built from a planted entry would
  still be attested as `release.yml` on `main`.
- **Runners.** Every workflow in the repository runs on GitHub-hosted runners, and
  this repository never opts in to being served by faber.

These rules are now part of the trust root.

- **Declared as code.** Rulesets and branch rules live in `infra/github/`, using
  the GitHub Terraform provider. The owner applies them, and a change to them is a
  reviewed pull request like any other.
- **Immutable releases are set by hand.** The provider cannot express that setting
  yet, so it is turned on by hand at the cut. `infra/github/README.md` records it
  and says why.
- **No secret to declare.** Keyless signing leaves none. The state stays on the
  owner's machine and is never committed.

### A release is the owner's Publish click, and it matures before it installs

1. **Actions → Release → Run workflow on `main`**, choosing patch, minor or major.
   The workflow refuses to run from any other ref, so a branch run cannot delete
   `main`'s draft. Its build and attest jobs hold no `contents: write`. One last
   job, which runs only the `gh` CLI and no third-party action, takes their
   output and writes the draft. The workflow:
   - computes the version from the last published release
   - deletes any older draft, so only one candidate exists
   - stamps the version into its own checkout's manifests, without committing
     anything
   - builds every target on a native runner, and checks that each binary reports
     exactly that version. Today's check is a substring match that runs only on
     tag pushes; it becomes exact, and runs on every dispatch.
   - checksums and attests the assets
   - creates a **draft** whose tag targets the built commit
2. **Releases → Publish.** Publishing creates the `v*` tag, which only the owner
   may do. Nothing fires on a merge, as ADR-0006 requires. The deliberate human
   act is this click, made with the draft and its notes in view.
3. **Followers wait** for the release to mature (#99), then verify it and install
   at their next idle.

Consequences of this flow:

- `Cargo.toml` no longer carries the release version, because the workflow stamps
  it. Local builds keep reporting `<version>+g<sha>`.
- Release notes are generated as now, from the last *published* release. They are
  public, so #100's gate covers what they are made from.
- `deploy/release.sh` no longer publishes, and the builder no longer cuts
  releases. Whether it follows them remains its operator's choice. ADR-0006's
  reason to keep a host that runs work-in-progress builds off autoupdate still
  holds.

### Opting in means trusting the account, and a release waits

- `[update] enabled` stays off by default (ADR-0006). The setup flow spells out
  what opting in means: *this host installs what the `rejifald/faber` GitHub
  account publishes, after the waiting period.*
- **The waiting period (#99) is part of this model, not an option.** With the
  account as the authority, the wait is how a release the owner did not make gets
  caught before any host installs it.
  - **The clock is this host's.** The wait starts at the later of two moments:
    the release's `published_at`, and the moment this host first saw it as its
    candidate. That second moment is written to disk, so a restart does not reset
    it.

    The reason: a release published as a pre-release, or with "latest" unticked,
    is invisible to `/releases/latest`, and those flags stay editable on an
    immutable release. If the clock started at `published_at` alone, a release
    could be published quietly and flipped visible only once it had already
    "matured".
  - **The alert is on the owner's machine.** The tray shows the pending release
    and its countdown, and that is the alert the account cannot silence. GitHub's
    release notifications are a second channel.
  - **#99's open points are settled:**
    - An absent `maturity_secs` means the default wait, 24 hours, and not zero.
    - A value under one hour is refused, the way `poll_interval_secs` refuses
      anything under a minute.
    - A pinned version waits too.

    This tightening arrives with the hand-installed cut, so ADR-0027's exception
    to AGENTS.md's absent-key rule covers it.
  - The cut sets the wait explicitly on every host. A host the owner watches may
    run a shorter wait as the canary.
- **An urgent fix is installed by hand.** Nothing in a release can shorten the
  wait.
- **Lowering the wait, or setting a pin, is a trust-widening change** under
  ADR-0028. Both apply live through `/api/settings`.

### What is accepted, explicitly

- **Whoever controls the owner's GitHub account can release to every opted-in
  host**, once the waiting period has passed. ADR-0015's claim that a repository
  takeover cannot yield an installable release no longer holds, and that is
  deliberate. The defences are on the account and in time:
  - a hardware security key as the second factor
  - no long-lived token with more scope than its job
  - the waiting period
  - the tray's countdown
- **The trust root includes GitHub's OIDC issuer and Sigstore's public
  infrastructure.** An outage pauses updates. It never weakens a check.
- **Release binaries are public.** AGENTS.md's rule that every artifact stays
  private gets exactly one exception: faber's own release assets, built from this
  public repository by its public workflow. Runner images, operator images and
  anything built from private code stay private, as before.
- **History, issues and PRs from before the cut stay private**, in the archive.

### What the spikes found (2026-10-11)

Two spikes ran before implementation. One verified a public project's attested
release with `sigstore-verify`. The other used a throwaway public repository with
this ADR's rules applied through Terraform, and probed what a workflow token can
do to releases.

**The verifier works.** Every check in the policy table passed on a genuine
bundle and refused its mutation: repository ID, workflow, ref, commit, runner,
issuer. A changed artifact and flipped DSSE signature and payload bytes were
refused too. With the network denied, verification passed from a cached TUF
root. With TUF unreachable, the fetch failed with a clean error and no fallback.
It adds about 2.3 MB stripped and 81 crates, most of them in the same families
faber already uses.

**What a workflow token can do, against a ruleset that restricts `v*` tags:**

| Action by `GITHUB_TOKEN` with `contents: write` | Result |
|---|---|
| Publish a draft whose tag does not exist | refused (creation restricted) |
| Create a `v*` ref directly | refused |
| **Publish a draft whose tag already exists** | **allowed**, and it became latest |
| Delete a published immutable release | **allowed**; its tag stays |
| Re-release on that orphaned tag | refused (used by an immutable release) |
| Mark an older release as latest | **allowed** |

**Decisions they forced:**

- **Only Publish creates a `v*` tag.** The owner never pushes one by hand. A
  scheduled job in faber's own CI fails loudly on any `v*` tag without a release,
  which is the one state the token can turn into a release. A refused edit of an
  immutable release's `tag_name` was seen to create such a tag, so the check is
  not hypothetical.
- **A follower never reads "latest".** It lists published, non-pre-release
  releases and takes the highest version strictly above its own. Deleting a
  release or moving "latest" then cannot downgrade a host or hide a newer one.
  The first-seen clock (above) covers a release that only becomes visible later.
- **Write access lives in one minimal job** (above), and the workflow refuses to
  run off `main`.
- **TUF unreachable pauses the update.** A cached TUF root is not used to ride out
  an outage; the maturity wait makes the delay cost nothing.
- **Accepted:** draft attestations are public before Publish, and the owner's
  ruleset bypass covers `v*` tags with no release. Only immutability protects a
  published release's tag, against the owner too.
- **Immutable releases cannot be set from Terraform** (provider 6.13), nor can a
  ban on self-hosted runners for a user-owned repository. Both are set by hand at
  the cut and recorded in `infra/github/README.md`.
- **GitHub's own release attestation** (`release/v0.2`) records the repository,
  tag and release ID, but not who published. No follower check can tell the
  owner's click from a token's.

## The cut, in order

This extends ADR-0027's upgrade sitting.

1. **In this repository, before the cut:** #100's text gate lands, the verifier
   passes the proofs below, and the open issues to keep are listed.
2. **Re-point every clone.** Stop every agent session working in this repository.
   On every machine, point each clone's and worktree's `origin` at
   `rejifald/faber-archive`.
3. **Pause every follower** (`[update] enabled = false`), as ADR-0027 already
   requires.
4. **Rename this repository** to `rejifald/faber-archive`. It stays private.
5. **Create `rejifald/faber`, private and empty,** and read its numeric ID.
6. **Fill the new repository.**
   - Put the ID into the cut tree's policy.
   - Push that tree as the single first commit.
   - Apply `infra/github/`, and turn on immutable releases.
   - Re-file the kept issues.
7. **Audit, then go public.** `git ls-remote` must show `main` and nothing else.
   Only then make the repository public.
8. **Release.**
   - Run the Release workflow.
   - Check the draft on the owner's machine with `gh attestation verify`, passing
     `--signer-workflow`, `--source-ref refs/heads/main` and
     `--deny-self-hosted-runners`.
   - Publish v0.4.0.
9. **Upgrade each host by hand.**
   - **Verify before installing.** The hand-installed binary becomes the host's
     trust root, and a binary cannot vouch for itself. So first verify its assets
     the same way, on the owner's machine.
   - **Run ADR-0027's sitting**, minus `faber auth set --updater`, since releases
     are read anonymously.
   - Set `[update] maturity_secs`.
   - Confirm that `/api/state` shows the host's own updater verifying v0.4.0
     ([#92](https://github.com/rejifald/faber-legacy/issues/92)).
   - Re-enable updates.

## Proofs before the cut

These are the release's acceptance test, in the shape of ADR-0026's runtime test.
A follower built for a throwaway public repository runs against attested releases
made there. It must show all of the following:

1. A release built from `main` verifies and installs.
2. A bundle is refused when it was made:
   - on another branch
   - by another workflow
   - on a self-hosted runner
   - by a repository with a different ID
   - from a commit other than the tag's
3. A bundle whose subject is not the downloaded `SHA256SUMS` is refused.
4. An older attested build is refused (#91), whether republished under a newer
   tag or swapped into a draft.
5. The wait holds:
   - A release published as a pre-release or not as latest, then flipped, waits
     the full period from when the host first sees it.
   - A restart does not reset the clock.
6. With TUF unreachable, the update pauses and says why.
7. The `v*` tag rule stops a workflow's `GITHUB_TOKEN` from publishing a draft
   whose tag does not exist yet. The token **can** publish a draft whose tag
   already exists (see *What the spikes found*), so the proof is instead that
   faber's orphan-tag check fails on a `v*` tag without a release.
8. A follower ignores "latest": with a newer release deleted or "latest" moved to
   an older one, it neither downgrades nor loses sight of the highest release.

## Consequences

- **No key exists anywhere.** These go with the implementation:
  - `deploy/signing-setup.sh`
  - the key list in `ACCEPTED_SIGNERS`
  - the signing step in `deploy/release.sh`

  So does ADR-0015's rule that a lost Mac is a fleet-level event.
- **The archive keeps the past.** Clones are re-pointed at it before the cut, and
  new work clones the public repository.
- **Open issues change role:**
  - **#91** is settled by the source-commit pin and the exact version check, not
    by changing what an ssh signature covers.
  - **#92** becomes a step of the upgrade sitting.
  - **#99** becomes a precondition of this model, with its open points settled
    above.
  - **[#71](https://github.com/rejifald/faber-legacy/issues/71)** becomes load-bearing,
    and gains the no-cache rule: the build jobs are now the whole chain of trust.
- **The 0.3.13 release plan is superseded.** It is kept with the legacy
  repository. 0.3.13 will not be cut, and no
  release ships from this repository again.
- **Docs change with the implementation.** AGENTS.md, the README and the deploy
  docs gain the artifact exception and the new release procedure, and lose the
  signing scripts.
- **Native macOS jobs.** If the native backend survives the cut as an exception
  ([#76](https://github.com/rejifald/faber-legacy/issues/76)), it must not run jobs as
  the owner. A job running as the owner can reach the owner's own GitHub login,
  and that login can publish.

## Alternatives considered

**Keep ADR-0015's operator key.** It is the strongest defence against an account
takeover. The owner rejected it: it leaves a key to generate, keep from CI, back
up, rotate and recover, on a project whose one maintainer's account already
decides every merge.

**GitHub Pro, a private repository, and the key in an environment secret limited
to `main`.** This works today, but it still leaves a key, one that can be neither
read back nor moved. A repository move or a deleted secret strands every host. It
also costs a subscription, for a repository that is going public anyway.

**Flip this repository to public in place.** That would publish private names in
the history, in 62 PR refs, in 15 tags and in 21 issue, PR and release texts
(#100). No API removes PR refs.

**A fresh repository now, not at the cut.** This would strand open pull requests
and every clone mid-flight. It would also move the hosts' release channel in a
step separate from the hard cut that already visits every host.

**Attest from a tag push, not from `main`.** The tag would then sit in the
certificate itself. But whoever creates a tag chooses its commit, and with it the
workflow file that runs. The GitHub UI also cannot create a tag without publishing
a release, which would go out empty. Building from `main` makes "built from
protected `main`" the property that gets verified, and the Publish click the
decision. The source-commit pin then ties the tag to that build.

**Start the wait at `published_at`.** A release can be published as a pre-release
or not as latest, which hides it from followers, and then be flipped once its
clock has run out. The host's own first sighting closes that.

**Immutable releases' own attestations (`gh release verify`).** They prove a
release has not changed since it was published. They do not prove which workflow
built it, or from which branch.

**Shell out to `cosign` or `gh` to verify.** That puts "install this tool" between
a host and its next update, which is ADR-0015's reason against minisign and
cosign. ADR-0027 removes `gh` anyway.

## Amendment (2026-11-03): the repositories moved before the cut

On 2026-11-02 the repositories moved ahead of the rest of the cut, and not quite
as planned above. The rest of this ADR stands.

- **The archive is `rejifald/faber-legacy`, not `faber-archive`.** It was renamed
  first, and the clones were re-pointed at that name. A second rename would break
  them again, so the name stays. Wherever this ADR says `faber-archive`, read
  `faber-legacy`. The issue and PR links in this repository's docs point there,
  and they work only for the owner.
- **The new repository came first, and it is not one commit.** `rejifald/faber`
  was created private before the cut. A README, a license and the agent
  instructions came first, then the privacy gate
  ([ADR-0058](0058-the-privacy-denylist-is-keyed.md)), each through a pull
  request. The code arrives the same way: one pull request with the archive's
  tree at the frozen commit, never its history, and with whatever must stay
  private reworded or left out. Step 5's "private and empty" and step 6's
  "single first commit" no longer hold. Every pull request leaves a
  `refs/pull/<n>/head` ref, and GitHub keeps it for good.
- **So the audit covers every ref.** Before the repository goes public (step 7),
  `git ls-remote` must show `main` and only the expected `refs/pull/*`. Every
  commit reachable from any ref, and every pull request and issue text, is
  scanned with the key and read. Nothing can be taken back afterwards: a bad ref
  goes only with the repository, and a new repository has a new ID, which is the
  release trust root. So nothing reaches the repository without that scan and
  read first.
- **The order changed.** Steps 2 and 3 (stop every agent session, re-point every
  clone, pause every follower) were to come before the rename, and the rename came
  first. The clones on the owner's own machine were re-pointed at once. Every other
  clone is re-pointed, and every follower paused, before the code arrives and before
  any release.
- **The gate is keyed.** Step 1's text gate (#100) landed in the archive, but its
  digests used a public salt and become guessable once published. The keyed gate
  of ADR-0058 replaced it before any code arrived here.
- **Bare issue numbers belong to the archive.** A `#123` written in a doc, an ADR
  or a code comment before the import is the archive's. GitHub does not link bare
  numbers inside files, and this repository's own numbers start again from 1, so
  a new reference names the repository or links in full.
