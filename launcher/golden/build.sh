#!/bin/sh
# Compile faber-golden-prep, the tool `faber golden prepare` drives (ADR-0049): the one place
# that installs macOS from Apple's restore image and opens the offline window. POSIX sh, Apple's
# tools only.
#
#   build.sh --compile-only    build and sign faber-golden-prep, nothing else (CI and release)
#
# This script is no longer an orchestrator. The operator's flow, the disk checks, the restore
# image's download and verification, the recipe and the boot all live in `faber golden prepare`
# (src/setup/golden_prepare.rs), so there is one place that decides them. A host gets the
# compiled tool from a release (`faber-golden-prep-<target>`).
#
# Environment:
#   FABER_VM_OUT       where the tool is built (default: $TMPDIR/faber-golden)
#   FABER_VERSION      the version to stamp into faber-golden-prep (a release build sets it);
#                      otherwise <Cargo.toml version>+g<short sha>, as launcher/build.sh derives it
set -eu

here=$(cd "$(dirname "$0")" && pwd)
launcher=$(cd "$here/.." && pwd)
out=${FABER_VM_OUT:-${TMPDIR:-/tmp}/faber-golden}
export FABER_VM_OUT="$out"
prep=$out/faber-golden-prep

die() { printf '%s\n' "build.sh: $*" >&2; exit 1; }
say() { printf '%s\n' "build.sh: $*"; }

[ "${1:-}" = --compile-only ] && [ $# -eq 1 ] || die "usage: build.sh --compile-only (the operator's flow is: faber golden prepare)"

# The version, exactly as launcher/build.sh derives it.
if [ -n "${FABER_VERSION:-}" ]; then
  version=$FABER_VERSION
else
  pkg=$(awk '
    /^\[package\][[:space:]]*$/ { inpkg = 1; next }
    /^\[/                       { inpkg = 0 }
    inpkg && /^version[[:space:]]*=/ {
      sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
    }' "$launcher/../Cargo.toml")
  [ -n "$pkg" ] || die "no [package] version in Cargo.toml"
  if sha=$(git -C "$launcher/.." rev-parse --short=7 HEAD 2>/dev/null) && [ -n "$sha" ]; then
    version=$pkg+g$sha
  else
    version=$pkg
  fi
fi
case "$version" in
  ""|*[!0-9A-Za-z.+-]*) die "refusing to stamp that version: only [0-9A-Za-z.+-] is allowed" ;;
esac

# Swift and Apple's frameworks, hardened runtime, and exactly the virtualization entitlement.
mkdir -p "$out/gen"
src=$out/gen/faber-golden-prep.swift
sed "s|^let prepVersion = \"unstamped\"\$|let prepVersion = \"$version\"|" "$here/prepare.swift" > "$src"
/usr/bin/grep -qF "let prepVersion = \"$version\"" "$src" || die "could not stamp the version into prepare.swift"
swiftc -O -swift-version 6 -warnings-as-errors -target arm64-apple-macos15.0 \
  -framework Foundation -framework Virtualization -framework AppKit \
  -module-name faber_golden_prep -o "$prep.new" "$src"
codesign --force -s - --options runtime --entitlements "$launcher/faber-vm.entitlements" "$prep.new" 2>/dev/null
got=$(codesign -d --entitlements - --xml "$prep.new" 2>/dev/null | plutil -convert json -o - -)
[ "$got" = '{"com.apple.security.virtualization":true}' ] || die "$prep carries entitlements $got"
# A rename, never a write in place: a boot still running the old binary keeps its file.
mv -f "$prep.new" "$prep"
say "built $prep"
