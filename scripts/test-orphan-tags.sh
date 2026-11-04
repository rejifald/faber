#!/usr/bin/env bash
# Fixture JSON in, verdict out.
set -euo pipefail
cd "$(dirname "$0")/.."
f=tests/fixtures/orphan-tags

out=$(scripts/orphan-tags.sh "$f/clean.tags.json" "$f/clean.releases.json")
[ "$out" = "ok" ] || { echo "clean case: expected ok, got: $out"; exit 1; }

if out=$(scripts/orphan-tags.sh "$f/orphan.tags.json" "$f/orphan.releases.json"); then
  echo "orphan case: expected failure"; exit 1
fi
[ "$out" = "ORPHAN v0.3.0" ] || { echo "orphan case: wrong verdict: $out"; exit 1; }

# No tags at all passes.
empty=$(mktemp)
echo '[]' > "$empty"
scripts/orphan-tags.sh "$empty" "$f/clean.releases.json" >/dev/null
rm -f "$empty"
echo "orphan-tags: all cases pass"
