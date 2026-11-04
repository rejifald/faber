#!/usr/bin/env bash
#
# Start a faber release candidate: dispatch the Release workflow on `main`,
# watch it, and check the draft it leaves has every asset. It never tags and
# never publishes.
#
# ADR-0031 moved both acts out of this script. `.github/workflows/release.yml`
# computes the version from the last published release, stamps it into its own
# checkout (nothing is committed), builds every target, attests SHA256SUMS and
# leaves one DRAFT. Publishing that draft, in GitHub's Releases page, is what
# creates the `v*` tag, and only the owner does it. This script is a
# convenience for step 1 of that flow and nothing more; Actions -> Release ->
# Run workflow does the same thing from a browser.
#
#   deploy/release.sh --patch   # bump the last published version
#   deploy/release.sh --minor
#   deploy/release.sh --major
#
# Flags:
#   --skip-checks   skip cargo fmt/clippy/test (they run by default)
#   --dry-run       print the plan and perform every read-only check for real
#                   (preflight, the version the workflow will compute), but
#                   dispatch nothing.
#   -h, --help
#
# Run it on a clean `main` that is not behind origin: the workflow builds
# origin's `main`, so the local checks are only meaningful against the same tree.
#
# Environment: FABER_RELEASE_REPO overrides the GitHub repo the workflow runs in
# (default: rejifald/faber).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_DIR" || exit 1

REPO_SLUG="${FABER_RELEASE_REPO:-rejifald/faber}"

CARGO_TOML="$REPO_DIR/Cargo.toml"

# Exactly what a draft carries. Linux is withheld (it has no wall yet, see
# release.yml), so every binary is for macOS: the daemon, the VM launcher, its
# guest agent, the golden-image tool and the menu-bar app, then the checksums and
# their attestation. A draft with one missing strands the hosts that need it, and
# one with an extra, a Linux binary say, ships something nobody decided to ship.
# tests/workflow_pins.rs fails when these differ from what release.yml stages.
EXPECTED_ASSETS=(
  "faber-aarch64-apple-darwin"
  "faber-launcher-aarch64-apple-darwin"
  "faber-golden-prep-aarch64-apple-darwin"
  "faber-agent-aarch64-apple-darwin"
  "faber-app-aarch64-apple-darwin.tar.gz"
  "SHA256SUMS"
  "SHA256SUMS.sigstore.json"
)

say()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '\033[33m!!  %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[31mxx  %s\033[0m\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
deploy/release.sh — dispatch the Release workflow and check the draft it makes.

  deploy/release.sh --patch   bump the last published version
  deploy/release.sh --minor
  deploy/release.sh --major

Options:
  --skip-checks   skip cargo fmt/clippy/test (they run by default)
  --dry-run       print the plan; dispatch nothing
  -h, --help      this text

Environment: FABER_RELEASE_REPO overrides the target repo (default rejifald/faber).

Steps, in order: preflight -> preview the version -> checks -> dispatch the
Release workflow on main -> watch it -> verify the DRAFT's assets. Then review
the draft under Releases and Publish it yourself: publishing creates the tag.
This script never tags and never publishes (ADR-0031).
EOF
}

DRY_RUN=0
SKIP_CHECKS=0
BUMP_KIND=""

while (( $# )); do
  case "$1" in
    --patch|--minor|--major)
      kind="${1#--}"
      [[ -z "$BUMP_KIND" || "$BUMP_KIND" == "$kind" ]] || die "pass only one of --patch/--minor/--major"
      BUMP_KIND="$kind"
      ;;
    --skip-checks) SKIP_CHECKS=1 ;;
    --dry-run)     DRY_RUN=1 ;;
    -h|--help)     usage; exit 0 ;;
    --no-install)  die "--no-install is gone: this script no longer installs anything (ADR-0031)" ;;
    -*)            die "unknown option: $1 (try --help)" ;;
    *)             die "explicit versions are gone: the workflow computes the version from the last published release. Pass --patch, --minor or --major." ;;
  esac
  shift
done

[[ -n "$BUMP_KIND" ]] || die "need one of --patch/--minor/--major — see --help"

(( DRY_RUN )) && warn "--dry-run: showing the plan, writing nothing"

# run <command...> — executes for real, or under --dry-run prints what would
# have run instead of running it. Used for every step from "Checks" onward;
# preflight and version/tag validation above always run for real, dry-run or
# not — a dry run that lied about whether you're on `main`, clean, and up to
# date with origin would not be a "sane plan," it would be a fantasy.
run() {
  if (( DRY_RUN )); then
    info "[dry-run] would run: $*"
  else
    info "running: $*"
    "$@"
  fi
}

# Read a package's own top-level `version = "x.y.z"` line. Anchored to column
# 0 so this can only ever match the `[package]` table's own field, never an
# inline dependency spec like `tokio = { version = "1", ... }` — none of
# which start a line with `version` in this repo's Cargo.toml style.
toml_pkg_version() {
  grep -m1 -E '^version = "' "$1" | sed -E 's/^version = "([^"]*)".*/\1/'
}

# ---------------------------------------------------------------------------
# Preflight — every check here is read-only, so all of it runs for real even
# under --dry-run.
# ---------------------------------------------------------------------------
say "Preflight"

CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[[ "$CURRENT_BRANCH" == "main" ]] \
  || die "on '$CURRENT_BRANCH', not main — releases are cut from main only."

[[ -z "$(git status --porcelain)" ]] \
  || die "working tree is not clean (git status) — commit or stash first."

info "fetching origin…"
git fetch origin main --tags

BEHIND="$(git rev-list --count HEAD..origin/main)"
[[ "$BEHIND" -eq 0 ]] \
  || die "local main is $BEHIND commit(s) behind origin/main — git pull --ff-only first."

gh auth status >/dev/null 2>&1 || die "gh is not authenticated — run 'gh auth login' first."

command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH."
command -v npm   >/dev/null 2>&1 || die "npm not found on PATH — needed to build the menu-bar app."

info "on main, clean, up to date with origin/main, gh authenticated, cargo+npm present."

# ---------------------------------------------------------------------------
# Version — a preview of what the workflow will compute, by the same rule:
# bump the higher of the last published, non-pre-release version and
# Cargo.toml's committed floor. Read-only, so it runs for real under --dry-run.
# The workflow's own computation is the one that counts.
# ---------------------------------------------------------------------------
say "Version"

FLOOR="$(toml_pkg_version "$CARGO_TOML")"
[[ "$FLOOR" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "Cargo.toml's version '$FLOOR' isn't three dotted integers — fix it by hand first."

LAST="$(gh api "repos/$REPO_SLUG/releases" --paginate \
          --jq '.[] | select(.draft == false and .prerelease == false) | .tag_name' \
        | sed -n 's/^v\([0-9]*\.[0-9]*\.[0-9]*\)$/\1/p' | sort -V | tail -n1)"
BASE="$(printf '%s\n%s\n' "${LAST:-0.0.0}" "$FLOOR" | sort -V | tail -n1)"
IFS=. read -r MAJ MIN PAT <<<"$BASE"
case "$BUMP_KIND" in
  patch) PAT=$((10#$PAT + 1)) ;;
  minor) MIN=$((10#$MIN + 1)); PAT=0 ;;
  major) MAJ=$((10#$MAJ + 1)); MIN=0; PAT=0 ;;
esac
VERSION="$((10#$MAJ)).$((10#$MIN)).$((10#$PAT))"
TAG="v$VERSION"

info "last published: ${LAST:-none}, floor (Cargo.toml): $FLOOR  ->  next: $VERSION"
if [[ -n "$LAST" ]]; then
  info "release notes will cover v$LAST..main"
else
  info "no published release found — this will read as the first tracked release"
fi

# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------
if (( SKIP_CHECKS )); then
  warn "skipping fmt/clippy/test (--skip-checks)"
else
  say "Checks"
  run cargo fmt --all -- --check
  run cargo clippy --workspace --all-targets --locked -- -D warnings
  run cargo test --workspace --locked
fi

# ---------------------------------------------------------------------------
# Dispatch the Release workflow on main and watch it. It builds, attests and
# leaves a DRAFT; a release that failed halfway is exactly the thing nobody
# notices until a follower is stranded, so this waits for the verdict.
# ---------------------------------------------------------------------------
say "Build + draft (GitHub Actions)"

if (( DRY_RUN )); then
  info "[dry-run] would run: gh workflow run release.yml --repo $REPO_SLUG --ref main -f bump=$BUMP_KIND"
  info "[dry-run] would wait for that run, then verify the draft $TAG has exactly"
  for asset in "${EXPECTED_ASSETS[@]}"; do
    info "[dry-run]   $asset"
  done
else
  BEFORE="$(gh run list --repo "$REPO_SLUG" --workflow release.yml --event workflow_dispatch \
              --limit 1 --json databaseId --jq '.[0].databaseId // ""')"
  run gh workflow run release.yml --repo "$REPO_SLUG" --ref main -f "bump=$BUMP_KIND"

  info "waiting for the Release run to appear…"
  RUN_ID=""
  for _ in $(seq 1 30); do
    RUN_ID="$(gh run list --repo "$REPO_SLUG" --workflow release.yml --event workflow_dispatch \
                --branch main --limit 1 --json databaseId --jq '.[0].databaseId // ""' 2>/dev/null || true)"
    [[ -n "$RUN_ID" && "$RUN_ID" != "$BEFORE" ]] && break
    RUN_ID=""
    sleep 5
  done
  [[ -n "$RUN_ID" ]] \
    || die "no new Release run appeared — check Actions; nothing was tagged or published."

  info "watching run $RUN_ID"
  gh run watch "$RUN_ID" --repo "$REPO_SLUG" --exit-status \
    || die "the Release run failed. Nothing is tagged or published; fix it and dispatch again (the next run deletes any older draft)."

  # Trust the draft, not the workflow's exit code: the assets are what the
  # fleet consumes, and a follower that finds one missing records a failed
  # stage against a version that was otherwise fine.
  say "Verify the draft's assets"
  DRAFT="$(gh release view "$TAG" --repo "$REPO_SLUG" --json isDraft --jq .isDraft)" \
    || die "no release $TAG after a green run — the workflow computed a different version than this preview; check Releases."
  [[ "$DRAFT" == "true" ]] || die "$TAG is not a draft — something other than this flow published it; investigate before anything else."
  # Both lists sorted bytewise, and compared bytewise, so `comm` can tell what is
  # missing from what is extra in any locale.
  PUBLISHED="$(gh release view "$TAG" --repo "$REPO_SLUG" --json assets --jq '.assets[].name' | LC_ALL=C sort)"
  info "$(echo "$PUBLISHED" | tr '\n' ' ')"
  EXPECTED="$(printf '%s\n' "${EXPECTED_ASSETS[@]}" | LC_ALL=C sort)"
  MISSING="$(LC_ALL=C comm -13 <(echo "$PUBLISHED") <(echo "$EXPECTED") | tr '\n' ' ')"
  EXTRA="$(LC_ALL=C comm -23 <(echo "$PUBLISHED") <(echo "$EXPECTED") | tr '\n' ' ')"
  # Braced: macOS's bash 3.2 reads the dash's first byte as part of the name in a
  # UTF-8 locale, and `set -u` then dies on exactly the mismatch being reported.
  [[ -z "$MISSING" ]] || die "draft $TAG is missing asset(s) ${MISSING}— do not publish it; the hosts that need them would be stranded."
  [[ -z "$EXTRA" ]] || die "draft $TAG carries unexpected asset(s) ${EXTRA}— do not publish it; Linux binaries are withheld until a Linux wall is decided, and nothing else was agreed."
  info "exactly the expected assets present"
fi

# ---------------------------------------------------------------------------
# Publishing is not this script's job. It is the owner's click, with the draft
# and its notes in view, and it is what creates the tag (ADR-0031).
# ---------------------------------------------------------------------------
say "Draft ready: $TAG"
if (( DRY_RUN )); then
  info "(--dry-run: nothing was dispatched)"
else
  info "review it, then Publish: https://github.com/$REPO_SLUG/releases"
  info "this script did not tag or publish anything."
fi
