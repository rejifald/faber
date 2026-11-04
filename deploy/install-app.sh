#!/usr/bin/env bash
#
# Build and install the faber macOS menu-bar app for the *current* user.
#
#   ./deploy/install-app.sh
#
# The app is a Tauri bundle: npm builds the frontend, cargo builds its Rust
# backend. It needs a GUI login session, so this is the desktop half of the
# install — a headless runner host wants deploy/install.sh alone.
#
# On first launch the real bundle registers itself as a login item, so there is
# no LaunchAgent for the app to install or unload.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DESKTOP_DIR="$REPO_DIR/desktop"

[[ "$(uname)" == "Darwin" ]] || { echo "the faber menu-bar app is macOS-only." >&2; exit 1; }
command -v npm >/dev/null 2>&1 || {
  echo "npm not found — the app's frontend needs Node 22.12+ (e.g. 'brew install node', or nvm)." >&2
  exit 1
}

echo "==> installing the UI's packages (the popover is built from ui/, ADR-0042)"
( cd "$REPO_DIR/ui" && npm ci )

echo "==> building the menu-bar app (Tauri) — the first build compiles a Rust backend, give it a few minutes"
( cd "$DESKTOP_DIR" && npm install && npm run tauri build )

APP_SRC="$DESKTOP_DIR/src-tauri/target/release/bundle/macos/faber.app"
[[ -d "$APP_SRC" ]] || { echo "expected app bundle not found at $APP_SRC" >&2; exit 1; }

# Prefer /Applications; fall back to ~/Applications if it isn't writable (no sudo).
if [[ -w /Applications ]]; then APP_DIR="/Applications"; else APP_DIR="$HOME/Applications"; mkdir -p "$APP_DIR"; fi
APP_DEST="$APP_DIR/faber.app"

echo "==> installing app -> $APP_DEST"
osascript -e 'quit app "faber"' >/dev/null 2>&1 || true   # release the bundle if a prior copy is running
sleep 1
rm -rf "$APP_DEST"
cp -R "$APP_SRC" "$APP_DIR/"
open "$APP_DEST"

echo
echo "menu-bar app installed -> $APP_DEST"
echo "  look for the faber glyph in the menu bar (⊘ = daemon offline until it's serving)"
