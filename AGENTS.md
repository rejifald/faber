# Working on faber

Instructions for coding agents. Read this before changing anything here.

## Everything you write here is public

Treat this repository as public. Whatever you push here is published, and git history
keeps it after a later commit removes it. A leak can't be fixed forward: once it's out,
it's a disclosure. So the rule is to leave it out from the start, not to clean up later.

The rule covers **every word you publish**, not only files:

- files and their history: code, tests, fixtures, docs, ADRs, comments
- commit messages and branch names
- pull request titles, descriptions and review comments
- issue titles, bodies and comments
- release notes, tags and discussion posts

### Never publish

- **Credentials of any kind:** tokens, keys, passwords, `.env` values, signing material,
  even expired ones. A credential that lands here is compromised. It must be rotated,
  and deleting it is not enough.
- **Specifics of an operator's environment:** hostnames, machine names, IP addresses,
  usernames, account names, LAN or VPN layout, hardware inventory, disk sizes, and
  memory or CPU figures read off a real machine.
- **Local file paths:** anything under a home directory (`/Users/<name>/…`,
  `/home/<name>/…`, `~/…` from a real machine), worktree paths, and scratch or temp
  paths. Derive a path at run time (`$HOME`), or write the role it plays instead.
  `/Library/Application Support/faber` is faber's own home and is fine.
- **Names of the projects faber runs for:** the repos, organizations, images,
  services and products whose CI runs on faber. Not in a fixture, not in a test, not
  in an incident write-up, and not in a comment that reconstructs a real failure.
- **Real configuration:** an operator's `config.toml`, grants, tokens' scopes, runner
  labels or profile names copied from a live host, and log or `audit.jsonl` excerpts
  that carry any of the above.
- **Anything about the operator's other work:** internal service names, customer
  names, private URLs, and ticket IDs from a private tracker.

The owner handle `rejifald` is fine: it is in the repository URL already.

### What to write instead

| Instead of | Write |
|---|---|
| a real repo or org | `owner/demo` |
| a real runner image | `ghcr.io/owner/my-runner` or `faber-runner:local` |
| a real host | "the builder host", "a second Mac" |
| a real path | `$HOME/…`, or the role, such as "the operator's checkout" |
| a real incident | "a watched repo", plus run, job and PR numbers, which identify nothing once the repo is gone |
| real numbers from a machine | round illustrative values, labelled as examples |

Replace the identifying detail **as you write**, not after a check complains. Do this
when you turn a real failure into a test fixture, an ADR or a bug report especially.

### Before you publish

- Re-read the diff, the commit message and the PR or issue text with this list in mind.
- If you need real details to debug, keep them in local, untracked files or in the
  conversation, never in the tree or on GitHub.
- If something private is already published, stop and tell the owner. Don't try to
  rewrite history or delete it quietly yourself; the owner decides on rotation and
  clean-up.

An automated scanner for private names comes with the source code. Until it lands
here, this file and your own review are the only guard. Even once it's here, the scanner
only knows the names it has been told about, so it is a backstop, not a licence to stop
checking.

## No artifact is published

The source is public; artifacts are not. Don't push images, packages or builds to any
public registry, and don't propose publishing one as a fix. The only exception is faber's
own release assets, built by this repository's release workflow.

## Ask before acting outside the repo

Never run `sudo` steps (`sudo faber setup`, `sudo faber uninstall`, installers) and
never create, change or delete accounts. Prepare the exact commands and hand them to the
operator.
