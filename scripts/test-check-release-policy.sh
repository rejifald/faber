#!/usr/bin/env bash
# Policy text in, verdict out: one accepted policy, and a refused one for each way
# a build can disagree with the run that builds it.
set -euo pipefail
cd "$(dirname "$0")/.."

repository=424242
workflow=owner/demo/.github/workflows/release.yml@refs/heads/main

# policy <name> <repository_id> <signer_workflow>: what `provenance-policy` prints.
policy() {
  printf 'policy: %s\nissuer: https://token.actions.githubusercontent.com\nrepository_id: %s\nsigner_workflow: %s\nsource_ref: refs/heads/main\nrunner_environment: github-hosted\npredicate_type: https://slsa.dev/provenance/v1\n' "$1" "$2" "$3"
}

check() {
  REPOSITORY_ID="$repository" WORKFLOW_REF="$workflow" scripts/check-release-policy.sh
}

# accepts <label>: the policy on stdin passes.
accepts() {
  local out
  out="$(check)" || {
    echo "$1: expected the policy to be accepted, got: $out"
    exit 1
  }
}

# refuses <label> <text>: the policy on stdin fails, and the error line, not the
# policy the script echoes first, says <text>.
refuses() {
  local out error
  if out="$(check)"; then
    echo "$1: expected a refusal"
    exit 1
  fi
  error="$(grep '^::error::' <<<"$out" || true)"
  [[ "$error" == *"$2"* ]] || {
    echo "$1: refused for the wrong reason (wanted '$2'): $out"
    exit 1
  }
}

signer=https://github.com/$workflow

policy release "$repository" "$signer" | accepts "this repository and workflow"

policy sandbox 1414155780 "$signer" | refuses "the sandbox policy" "not the release policy"
policy release "0 (placeholder: refuses every release)" "$signer" | refuses "the placeholder ID" "placeholder repository ID"
policy release 0 "$signer" | refuses "a bare zero ID" "placeholder repository ID"
policy release 424243 "$signer" | refuses "another repository's ID" "repository 424243"
policy release 0424242 "$signer" | refuses "an ID with a leading zero" "repository 0424242"
policy release many "$signer" | refuses "an ID that is not a number" "not a number"
policy release "$repository" "https://github.com/owner/other/.github/workflows/release.yml@refs/heads/main" \
  | refuses "another repository's signer" "signer is"
policy release "$repository" "https://github.com/owner/demo/.github/workflows/other.yml@refs/heads/main" \
  | refuses "another workflow's signer" "signer is"
policy release "$repository" "https://github.com/owner/demo/.github/workflows/release.yml@refs/heads/dev" \
  | refuses "another ref's signer" "signer is"
policy release "$repository" "" | refuses "no signer" "signer is"
printf '' | refuses "no output at all" "no verification policy"

# A run that cannot say which repository or workflow it is must not pass.
if policy release "$repository" "$signer" | REPOSITORY_ID="" WORKFLOW_REF="$workflow" scripts/check-release-policy.sh >/dev/null; then
  echo "an unknown repository: expected a refusal"
  exit 1
fi
if policy release "$repository" "$signer" | REPOSITORY_ID="$repository" WORKFLOW_REF="" scripts/check-release-policy.sh >/dev/null; then
  echo "an unknown workflow: expected a refusal"
  exit 1
fi

echo "check-release-policy: all cases pass"
