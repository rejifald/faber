#!/usr/bin/env bash
# Holds the verification policy compiled into a release binary to the repository
# and the workflow that are building it (ADR-0031). A follower trusts exactly what
# that policy names, so a build whose policy names anything else is one that
# followers could never update from, or one that makes them trust another
# repository's releases.
#
#   faber provenance-policy           | scripts/check-release-policy.sh
#   faber-menubar --provenance-policy | scripts/check-release-policy.sh
#
# Reads the policy on stdin, one `key: value` per line, and prints it back.
# Needs, from the workflow run:
#   REPOSITORY_ID  the numeric ID of the repository the run is in
#                  (github.repository_id)
#   WORKFLOW_REF   the workflow being run, as
#                  owner/repo/.github/workflows/release.yml@refs/heads/main
#                  (github.workflow_ref)
#
# Exits non-zero, after an `::error::` line, unless the policy is the release
# one, carries a real repository ID that is this run's, and names this workflow
# at this ref as the signer. Runs on the hosted macOS image's bash as well as
# Linux's, so it keeps to what bash 3.2 has.
set -euo pipefail

fail() {
  echo "::error::$*"
  exit 1
}

[[ "${REPOSITORY_ID:-}" =~ ^[1-9][0-9]*$ ]] \
  || fail "REPOSITORY_ID is '${REPOSITORY_ID:-}', not a repository ID"
[[ "${WORKFLOW_REF:-}" == */.github/workflows/*@refs/* ]] \
  || fail "WORKFLOW_REF is '${WORKFLOW_REF:-}', not a workflow ref"

policy="$(cat)"
[[ -n "$policy" ]] || fail "the binary printed no verification policy"
echo "$policy"

# The value of the first `name: value` line, or nothing when there is none.
field() {
  local line
  while IFS= read -r line; do
    case "$line" in
      "$1: "*)
        printf '%s' "${line#"$1: "}"
        return 0
        ;;
    esac
  done <<<"$policy"
}

# The `sandbox-policy` feature swaps in the throwaway sandbox repository's
# policy, for tests only; a binary built with it would make every follower trust
# that repository. The binary is asked, rather than the build command read, so a
# feature turned on any other way (a default, a dependency) is caught too.
first_line="${policy%%$'\n'*}"
[[ "$first_line" == "policy: release" ]] \
  || fail "the binary enforces '$first_line', not the release policy (is the sandbox-policy feature on?)"

# Until the cut sets the release repository's numeric ID, the policy carries the
# placeholder 0, which refuses every release. A follower that installed such a
# build could never verify another update and would be stranded on it.
repository_id="$(field repository_id)"
case "$repository_id" in
  0 | "0 "* | *placeholder*)
    fail "the release policy still carries the placeholder repository ID; a host that installed this build could never update again. Set the real ID first."
    ;;
esac
[[ "$repository_id" =~ ^[0-9]+$ ]] \
  || fail "the policy's repository_id is '$repository_id', not a number"
[[ "$repository_id" == "$REPOSITORY_ID" ]] \
  || fail "the policy trusts repository $repository_id, but this run is in repository $REPOSITORY_ID: a host that installed this build would verify another repository's releases"

signer="$(field signer_workflow)"
want="https://github.com/$WORKFLOW_REF"
[[ "$signer" == "$want" ]] \
  || fail "the policy's signer is '$signer', but this run is '$want': a host that installed this build would refuse every release this workflow signs"

echo "release policy trusts repository $repository_id and signer $signer"
