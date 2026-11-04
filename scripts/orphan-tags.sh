#!/usr/bin/env bash
# Verdict on orphan release tags.
#
#   orphan-tags.sh <tags.json> <releases.json>
#
# tags.json      JSON array of tag names, e.g. ["v0.1.0","nightly"]
# releases.json  JSON array of releases as `gh release list --json tagName,isDraft`
#
# A v* tag is an orphan when no PUBLISHED (non-draft) release carries it. Prints
# one "ORPHAN <tag>" line per orphan and exits 1; prints "ok" and exits 0 otherwise.
set -euo pipefail

[ "$#" -eq 2 ] || { echo "usage: $0 <tags.json> <releases.json>" >&2; exit 2; }

orphans=$(jq -r --slurpfile rel "$2" '
  ($rel[0] | map(select(.isDraft | not) | .tagName)) as $published
  | .[] | select(startswith("v")) | select(. as $t | $published | index($t) | not)
' "$1")

if [ -n "$orphans" ]; then
  while IFS= read -r t; do echo "ORPHAN $t"; done <<<"$orphans"
  exit 1
fi
echo ok
