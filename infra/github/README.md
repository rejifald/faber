# infra/github

ADR-0031's repository rules as Terraform (GitHub provider `~> 6.13`). Read
"Builds come only from protected `main`" in
`docs/adrs/0031-releases-are-signed-by-github-from-a-fresh-public-repository.md`
first; this directory is the code form of that list.

| Resource | Rule |
|---|---|
| `github_repository_ruleset.main` | Changes only by pull request with the CI checks green; no force-push, no deletion; no bypass. |
| `github_repository_ruleset.release_tags` | `v*` creation, update and deletion restricted; only the repository admin bypasses. |
| `github_workflow_repository_permissions` | Default workflow token read-only; workflows cannot approve PRs. |
| `github_actions_repository_permissions` | GitHub-owned actions plus an explicit allow-list, every one pinned to a full commit SHA. |
| `github_actions_secret.privacy_key` | The privacy gate's key, `FABER_PRIVACY_KEY` ([ADR-0058](../../docs/adrs/0058-the-privacy-denylist-is-keyed.md)). |
| `github_repository_vulnerability_alerts`, `github_repository_dependabot_security_updates` | Dependabot alerts, and the pull requests that fix them. |
| `github_repository.this`, `security_and_analysis` | Secret scanning and push protection, from the apply after the one that makes the repository public. |

`ci_check_contexts` in `variables.tf` must match the job names in
`.github/workflows/ci.yml`. Rename a job and the ruleset blocks every PR until
the variable follows.

`tests/workflow_pins.rs` checks the pins and the allow-list before anything is
pushed: it fails if a `uses:` is not pinned to a commit SHA, or names a
third-party action that `patterns_allowed` does not list. Dependabot
(`.github/dependabot.yml`) only moves the pins of actions already in use, so its
PRs keep matching.

## Applying

The owner runs this by hand, from their own machine. CI only validates; it never
holds credentials and never plans or applies.

1. Export a token with repository administration rights as `GITHUB_TOKEN`
   (fine-grained: Administration read/write and Secrets read/write on the
   target repository).
2. Export the privacy key from the keychain (ADR-0058). Every plan needs it,
   because this directory owns the `FABER_PRIVACY_KEY` secret:
   `export TF_VAR_privacy_key="$(security find-generic-password -s faber-privacy-key -w)"`.
3. `terraform -chdir=infra/github init`
4. Before the first apply, adopt the repository, which exists already and could
   not be created again:
   `terraform -chdir=infra/github import -var repository=<name> github_repository.this <name>`.
   Nothing else needs importing: the first apply creates the rest, and creating
   a permission resource or the secret sets it, whatever was there before.
5. `terraform -chdir=infra/github plan -var repository=<name>`, read it, then `apply`.

**The first apply makes the repository public, so it is the cut's go-public
step and never comes earlier.** It cannot come earlier anyway: GitHub refuses
rulesets on a private repository on the Free plan, so the rulesets are created
in the same apply that changes the visibility. That apply also takes
`-var secret_scanning=false`, because GitHub refuses secret scanning on a
repository that is still private when the request arrives. Plan and apply once
more without it, and secret scanning and push protection turn on.

**State stays on the owner's machine.** It is gitignored (`*.tfstate`,
`.terraform/`, `*.tfvars`) and must never be committed: state can contain
resource attributes the owner would not publish in a public repository, and it
holds the privacy key in the clear. No remote backend is configured on purpose.

To rehearse, point `repository` at a throwaway repository first.

**No bypass at the cut.** `owner_bypass_main_for_cut` defaults to `false`.
ADR-0031 planned it for the single first commit of a fresh repository. The code
arrived by pull request before any ruleset existed instead (ADR-0031's
2026-11-03 amendment), so the cut needs no bypass. Leave it false: a bypass left
on is a standing hole in `main`.

## Settings Terraform cannot express

Set these by hand at the cut and re-check them after any repository change.

- **Immutable releases.** Provider 6.13 has no attribute for it. Turn it on in
  Settings → General → Releases, or `PUT /repos/{owner}/{repo}/immutable-releases`.
  Why: it is what makes a published release's assets and tag unchangeable, even
  by the owner. The tag ruleset's admin bypass would otherwise let the owner move
  a tag; only immutability protects a published release against that.
- **No self-hosted runners for a user-owned repository.** A user-owned
  repository has no runner-group policy that Terraform can set, so nothing stops
  a self-hosted runner being registered to it. Why: ADR-0031 requires every
  workflow to run on GitHub-hosted runners, and faber must never be opted in to
  serve this repository. Leave Settings → Actions → Runners empty and check it
  after each repository change.
- **Private vulnerability reporting.** `SECURITY.md` sends reporters to the
  Security tab, which takes reports only once this is on: Settings → Code security
  → Private vulnerability reporting, or
  `PUT /repos/{owner}/{repo}/private-vulnerability-reporting`.
- **Code scanning.** CodeQL's default setup, which picks the languages itself:
  Settings → Code security → CodeQL analysis, or
  `PATCH /repos/{owner}/{repo}/code-scanning/default-setup` with `state=configured`.
- **Approval for outside contributors' workflow runs.** "Require approval for all
  external contributors" (Settings → Actions → General), or
  `PUT /repos/{owner}/{repo}/actions/permissions/fork-pr-contributor-approval`
  with `approval_policy=all_external_contributors`. Why: a fork's pull request
  gets no secrets, but its run still spends minutes and writes caches. GitHub
  offers the setting on a public repository only.

## Orphan-tag alarm

`.github/workflows/orphan-tags.yml` runs daily, on `release` events and on demand.
It fails, naming the tag, when a `v*` tag has no published release. Why: a
workflow token with `contents: write` can publish a draft whose tag already
exists, so an orphan tag is the one state that turns into an unclicked release.
The detection logic is `scripts/orphan-tags.sh`, tested by
`scripts/test-orphan-tags.sh` against `tests/fixtures/orphan-tags/`.
