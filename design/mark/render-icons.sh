#!/usr/bin/env bash
# Renders the desktop app's bundle icons from the SVG masters in this folder.
# Needs macOS (AppKit renders the SVGs, iconutil packs the .icns) and node (packs the .ico).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
icons="$here/../../desktop/src-tauri/icons"
mac="$here/faber-app-icon-macos.svg"
full="$here/faber-mark-forge.svg"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

swiftc -O "$here/rasterize.swift" -o "$work/rasterize"
render() { "$work/rasterize" "$1" "$2" "$3"; }

# macOS: the tile sits inside Apple's canvas margin.
mkdir "$work/AppIcon.iconset"
for size in 16 32 128 256 512; do
  render "$mac" "$work/AppIcon.iconset/icon_${size}x${size}.png" "$size"
  render "$mac" "$work/AppIcon.iconset/icon_${size}x${size}@2x.png" "$((size * 2))"
done
iconutil -c icns "$work/AppIcon.iconset" -o "$icons/icon.icns"
render "$mac" "$icons/32x32.png" 32
render "$mac" "$icons/128x128.png" 128
render "$mac" "$icons/128x128@2x.png" 256
render "$mac" "$icons/icon.png" 512
cp "$mac" "$icons/icon.svg"

# Windows: full-bleed tile.
for size in 30 44 71 89 107 142 150 284 310; do
  render "$full" "$icons/Square${size}x${size}Logo.png" "$size"
done
render "$full" "$icons/StoreLogo.png" 50
for size in 16 32 48 256; do
  render "$full" "$work/$size.png" "$size"
done
node "$here/pack-ico.mjs" "$icons/icon.ico" "$work/16.png" "$work/32.png" "$work/48.png" "$work/256.png"

echo "rendered bundle icons into $icons"
