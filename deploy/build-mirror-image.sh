#!/usr/bin/env bash
#
# Build the faber-mirror service image from this checkout (ADR-0047;
# docs/mirrors.md).
#
# The image is tagged `faber-mirror:<version>`, where <version> is the
# `[package] version` in this checkout's Cargo.toml: the tag a faber built from
# the same checkout looks for. It is built into the Docker engine `docker`
# talks to (set DOCKER_HOST to choose one), and it is never pushed anywhere:
# nothing faber builds is published (AGENTS.md).
#
#   deploy/build-mirror-image.sh              # build into the current engine
#   deploy/build-mirror-image.sh --save FILE  # also write the image to FILE
#                                              # (a `docker save` tar), to load
#                                              # into an engine you cannot build in
#   deploy/build-mirror-image.sh --print-tag  # print the tag and do nothing else
#
# On a set-up macOS host the runners' engine is faber's own Linux VM, which only
# `_faber` may reach. Build anywhere you have Docker with --save, then load the
# file as `_faber` (docs/mirrors.md has the exact command).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  sed -n '3,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

save=""
print_tag=0
while [ $# -gt 0 ]; do
  case "$1" in
    --save)
      [ $# -ge 2 ] || { echo "--save needs a file" >&2; exit 2; }
      save="$2"
      shift 2
      ;;
    --print-tag)
      print_tag=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

# The first `version = "…"` after `[package]` in the root manifest.
version="$(awk '
  /^\[package\]/ { in_pkg = 1; next }
  /^\[/ { in_pkg = 0 }
  in_pkg && /^version[[:space:]]*=/ {
    gsub(/^[^"]*"/, ""); gsub(/".*$/, ""); print; exit
  }
' "$root/Cargo.toml")"
if [ -z "$version" ]; then
  echo "could not read the version from $root/Cargo.toml" >&2
  exit 1
fi
tag="faber-mirror:${version}"

if [ "$print_tag" = 1 ]; then
  echo "$tag"
  exit 0
fi

echo "building $tag from $root"
docker build \
  --file "$root/crates/faber-mirror/Dockerfile" \
  --build-arg "FABER_VERSION=${version}" \
  --tag "$tag" \
  "$root"

if [ -n "$save" ]; then
  docker save --output "$save" "$tag"
  echo "saved $tag to $save"
fi
echo "built $tag"
