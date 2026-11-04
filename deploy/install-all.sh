#!/usr/bin/env bash
#
# One-shot installer: the faber daemon (a system service, ADR-0025) + the
# macOS menu-bar app. macOS / Apple Silicon.
#
#   ./deploy/install-all.sh            # build + install daemon and menu-bar app
#   ./deploy/install-all.sh --build    # force a release rebuild of the daemon
#
# This is the "everything" convenience wrapper. The pieces are also installable
# on their own: deploy/install.sh (daemon only) is what a headless host uses —
# it has no GUI session for a tray app. Building needs no privileges; the
# daemon's own install ends with `sudo faber setup`, which shows its plan and
# asks before changing anything.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

say()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33m!!  %s\033[0m\n' "$*" >&2; }

[[ "$(uname)" == "Darwin" ]] || { echo "faber's service + app install is macOS-only." >&2; exit 1; }

# ---------------------------------------------------------------------------
# 1. Preflight. Build toolchains are hard requirements (collected and reported
#    together, so you install them in one pass); runtime deps only warn — the
#    daemon and app install fine and simply idle until these are set up.
# ---------------------------------------------------------------------------
say "Preflight"
missing=()
command -v cargo >/dev/null || missing+=(
  "Rust (cargo): curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh  &&  source \$HOME/.cargo/env")
command -v npm >/dev/null || missing+=(
  "Node.js 20+ (npm), to build the menu-bar app: e.g.  brew install node")
xcode-select -p >/dev/null 2>&1 || missing+=(
  "Xcode Command Line Tools (compiler + codesign):  xcode-select --install")

if (( ${#missing[@]} )); then
  warn "Missing build prerequisites — install these, then re-run:"
  for m in "${missing[@]}"; do printf '      - %s\n' "$m" >&2; done
  exit 1
fi

docker info >/dev/null 2>&1 \
  || warn "Docker isn't running — Docker-backend jobs need Docker Desktop up (and the runner image built)."

# ---------------------------------------------------------------------------
# 2. Daemon — reuse the focused, tested installer (builds release, then hands
#    off to `sudo faber setup`, ADR-0025). Any args (e.g. --build) pass
#    straight through.
# ---------------------------------------------------------------------------
say "Installing the faber daemon"
"$SCRIPT_DIR/install.sh" "$@"

# ---------------------------------------------------------------------------
# 3. Menu-bar app — same focused installer the curl-pipe bootstrap calls, so
#    the Tauri build and bundle placement live in exactly one place.
# ---------------------------------------------------------------------------
say "Installing the menu-bar app"
"$SCRIPT_DIR/install-app.sh"

# ---------------------------------------------------------------------------
# 4. What's left (the inherently-manual bits).
# ---------------------------------------------------------------------------
say "Done — daemon + menu-bar app installed."
cat <<EOF
  dashboard   : faber dashboard (prints and opens a one-time link)
  menu bar    : look for the faber glyph (⊘ = daemon offline until it's serving)
  daemon logs : /Library/Logs/faber/stderr.log (readable through group
                _faber_operator — open a new terminal first; your membership
                takes effect at login)

  If you ran "sudo faber setup" above (the default), faber now runs as its own
  _faber account, kept awake and started at boot automatically — no auto-login,
  no manual pmset. Its config lives at
      /Library/Application Support/faber/config.toml
  (also _faber-owned; edit it from the dashboard's Settings page, or read it
  through group _faber_operator). Until it is handed a GitHub token it sits in
  standby, and the dashboard says so.

  Remaining setup, if not already done:
    - faber auth link                    (prints the GitHub page that pre-fills
                                           this host's fine-grained token; pick
                                           its repositories there)
    - sudo faber auth set                (hands the token to _faber, ADR-0027)
    - the runner image, built once with Docker and handed to setup (the
      installer's closing text has the commands)

  Updating later: fleet autoupdate (opt-in, ADR-0006) installs the daemon and,
  beside it, the VM launcher and the guest agent; the menu-bar app follows the
  daemon on its own (ADR-0034). By hand, from source: docs/manual-update.md.
EOF
