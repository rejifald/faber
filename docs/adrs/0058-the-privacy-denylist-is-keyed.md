---
title: The privacy denylist is keyed
type: adr
status: accepted
date: 2026-10-31
owner: rejifald
---

# ADR-0058: The privacy denylist is keyed

## Status

accepted (2026-10-31, owner: keyed digests in the tree, over keeping the list
out of the tree).

Corrected on 2026-11-03, when the gate came into this repository ahead of the
code: names are checked only in text from an author the repository trusts
(decision 3), the steps that hold the key run no cargo command (decision 4),
nobody keeps the list by hand (decision 7), and the key's custody follows this
repository (§ Custody).

## Context

The privacy gate (AGENTS.md, *Everything you write here is public*) keeps private names out of
the tree and out of public text. It cannot store the names, so `DENIED` in
`src/privacy.rs` held SHA-256 digests of them, salted with a public constant.

The 2026-10-31 security review found that this protects nothing once
published. The salt is in the same file, so the digests can be searched
exhaustively. There are about 2.8·10¹² names of eight lower-case letters and
digits, and one GPU tries them all in minutes. No dictionary and no prior
suspicion are needed. The cut (ADR-0031) publishes the tree, so this had to be
fixed before cut step 7.

The same review found that the scan missed three spellings. It joined only
adjacent pairs of words and never split inside an alphanumeric run, so a name
of three or more words (`widget-data-platform`), a name embedded in camel case
(`WidgetcoClient`) and a name run together with digits (`widgetco2`) all passed
every gate.

## Decision

1. **The digests are keyed.** `DENIED` holds HMAC-SHA256 digests under a key
   of at least 32 bytes, given as hex in `FABER_PRIVACY_KEY`. Without the key,
   a published digest cannot confirm even one guess. HMAC is written over the
   `sha2` crate faber already depends on, and checked against RFC 4231.
2. **The key is held only by the repositories and the owner:** each
   repository's `FABER_PRIVACY_KEY` Actions secret, and the owner's own machine:
   the login keychain, which the environment reads it from, and the local
   Terraform state once `infra/github` owns the secret (§ Custody). It is never
   in the tree, a log, a message or a commit.
3. **Without the key, the gates say so.** They still check credentials and home
   paths, but print `PRIVACY GATE SKIPPED` instead of passing names in silence.
   The notice goes straight to stderr, which the test harness does not capture,
   and on GitHub Actions it is also a warning annotation. CI sets
   `FABER_PRIVACY_KEY_REQUIRED=true` on every run GitHub gives secrets to and
   whose author it trusts, so a missing secret fails the run there. A key with
   an empty `DENIED` is an error too, because nothing would be checked.

   Names are checked only in text from a trusted author: a pull request from a
   branch of this repository (not Dependabot's), or an issue, comment or review
   by the repository's owner, a member or a collaborator (a review only on such
   a pull request: one on a fork's gets no secrets). A keyed scan reports
   which lines of its input name something listed, and anyone can post an issue
   or a comment. Scanning that text with the key would confirm guesses without
   it: post a list of candidates, then read the public log. So
   `privacy-text.yml` decides trust in its first step, from fields GitHub sets,
   and an untrusted author's text gets the scan without the key.
4. **Only the steps that check names see the key, and they run no cargo
   command.** `ci.yml` builds every test binary in one step, and its
   `Privacy gate (names)` step runs the tree gate's binary straight from that
   build. The general test step skips both tree tests, because a path they
   print is masked only where the key is (decision 6). `privacy-text.yml` and
   `release.yml` build the scanner in one step and run its binary with the key
   in the next. A cargo command where the key is could re-run a build script:
   the root one re-runs whenever `ui/dist` is missing, as it is in those jobs.
   So no build script, other test or npm package runs where the key is.
5. **The scan splits and joins more.** A run splits into words at camel case
   (`widgetClient`, `HTTPServer`) and wherever letters and digits meet. Up to
   four adjacent words join into one token, across separators. Each run is also
   digested whole.
6. **Printed paths are masked.** Every path a gate prints passes through the
   denylist first, and any listed name in it prints as `***`.
7. **Nobody keeps the list.** `examples/privacy_denylist.rs` derives the names
   from what the owner's machine can see (the private repositories, packages
   and organisations of the account `gh` is signed in to, and the machine's
   host names), adjusted by two local files that are never committed, and
   writes their digests into `DENIED`. It prints counts, never names. A name
   stays listed after its source is gone.

## Custody

The key is made once, straight into the login keychain, and never displayed:

```sh
security add-generic-password -a "$USER" -s faber-privacy-key -w "$(openssl rand -hex 32)"
```

Export it into a shell when it is needed:

```sh
export FABER_PRIVACY_KEY="$(security find-generic-password -s faber-privacy-key -w)"
```

While this repository is private it gets the key by hand, because
`infra/github` cannot be applied to a private repository on GitHub Free (it has
no rulesets). This is the only secret set outside Terraform:

```sh
security find-generic-password -s faber-privacy-key -w | gh secret set FABER_PRIVACY_KEY --repo rejifald/faber
```

Once the repository is public, `infra/github` owns the secret
(`github_actions_secret.privacy_key`) and sets it from `TF_VAR_privacy_key`. It
is then in the Terraform state, which stays on the owner's machine.

## Consequences

- Names are checked only where the key is and the author is trusted: CI on
  this repository's own branches, its members' issues and comments, and
  wherever the owner exports the key. A fork's or Dependabot's pull request
  skips names, and the push to `main` checks them after merge. An outsider's
  issue or comment skips them for good: review is the only check there. A
  `pull_request_target` scan that reads a fork's text and files as data, with
  the scanner from the default branch, would close that gap. It is not built
  here.
- After the cut, a branch pushed to the public repository is public at once,
  before CI runs. An agent session checks names before pushing only if its
  environment has the key. A shell profile that exports it gives the key to
  every process that profile starts. That is the owner's choice for each
  machine. The cut exports the key for its sitting only.
- A leaked key makes every digest ever published guessable again, because
  history keeps them. Rotating means a new key, an emptied `DENIED` refilled
  under it by `cargo run --example privacy_denylist -- --write`, and the secret
  updated in each repository. The generator keeps every digest it cannot
  recompute, so without emptying `DENIED` first the old key's digests would
  stay as dead entries. A name no source shows any more goes into its
  `extra-names` file first, or the rotation drops it. That protects the names
  from then on, not the digests already published. Losing the key costs the
  same rotation, and nothing else.
- `DENIED` still shows how many names there are.
- Some spellings are still missed:
  - a name inside an all-lower-case run with no boundary (`widgetcoclient`);
  - a name of more than four words written with separators, when it is added
    by hand (`print_digest` warns about this). The generator also lists every
    four-word window of such a name, so a derived one is caught;
  - a name shorter than four characters.
- The scan digests about four times as many tokens, at two compressions each
  instead of one. In a debug build on the builder Mac (2026-10-31), the tree
  gate took about 7 s of CPU, against 3.4 s before. Two choices hold it there.
  The digests go through `sha2`'s raw compression function, which the
  `compress` feature exports, instead of its generic API. And `sha2` builds
  optimized in the dev profile. Without both, the same scan took 26 s.

## Considered options

- **The digest list only in a secret.** It publishes nothing, not even the
  count, and today's digests could have moved into the secret unchanged. It
  lost because every new name would become an unreviewed edit to the secret in
  each repository, plus the owner's own copy. A copy that was missed would
  silently stop checking that name.
- **A slow hash with a public salt** (scrypt, Argon2). It makes each guess
  cost more, but it makes each scanned token cost exactly as much. The tree
  gate digests millions of tokens, so any cost that slows an attacker down
  makes the gate unusable.
