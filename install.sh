#!/usr/bin/env bash
#
# faber bootstrap installer — the curl-pipe entry point.
#
#   curl -fsSL https://raw.githubusercontent.com/rejifald/faber/main/install.sh | bash
#
# It keeps a git checkout under ~/.faber/src, offers to install whatever build
# toolchains are missing (asking first, always), and then hands off to the
# focused installers that already live in deploy/: install.sh for the daemon,
# install-app.sh for the menu-bar app. Re-running it updates the checkout and
# rebuilds the daemon (and the app, if asked). It does not build the VM
# launcher or the guest agent, which a full update needs from the same commit
# (docs/manual-update.md).
#
# faber is built from source: there are no prebuilt release binaries, so a Rust
# toolchain is a hard requirement rather than a convenience.
#
# Cloning and building need no privileges and land under $HOME (plus
# /Applications for the app, if that happens to be writable). The one
# privileged step is deploy/install.sh's own hand-off to `sudo faber setup`
# (ADR-0025), which creates faber's accounts and installs its system service —
# it shows its plan and asks before changing anything.
#
# Flags — all optional; without them the script asks:
#   --with-app | --daemon-only   what to install (skips the "what?" prompt)
#   --ref <git-ref>              branch or tag to install     (default: main)
#   --dir <path>                 where to keep the checkout   (default: ~/.faber/src)
#   -y, --yes                    take the default for every prompt, ask nothing
#   -h, --help
#
# Non-interactive use (CI, a provisioning script) needs no tty — every prompt
# resolves to its documented default, and --yes accepts toolchain installs:
#
#   curl -fsSL .../install.sh | bash -s -- --with-app --yes
#
# Environment: FABER_REPO, FABER_REF, FABER_SRC mirror the flags.
#
# Everything lives inside functions and main() runs last, so a connection that
# dies mid-download can never execute a half-written script.
set -euo pipefail

FABER_REPO_DEFAULT="https://github.com/rejifald/faber.git"
# Resolved from the nvm release API at run time; this is only the offline fallback.
NVM_PINNED_FALLBACK="v0.40.6"
NODE_MIN="22.12.0"   # ui/package.json's engines
RUST_MIN="1.96.0"

say()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '\033[33m!!  %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[31mxx  %s\033[0m\n' "$*" >&2; exit 1; }

# Spelled out rather than scraped from the header comment: piped into bash there
# is no $0 to read the header back out of.
usage() {
  cat <<'EOF'
faber installer — builds from source and installs the daemon (and, optionally,
the macOS menu-bar app). Cloning and building need no sudo; the install ends by
handing off to `sudo faber setup`, which shows its plan and asks first.

  curl -fsSL https://raw.githubusercontent.com/rejifald/faber/main/install.sh | bash

Options:
  --with-app        install the daemon and the menu-bar app
  --daemon-only     install just the daemon (default when there is no terminal)
  --ref <git-ref>   branch or tag to install            (default: main)
  --dir <path>      where to keep the checkout          (default: ~/.faber/src)
  -y, --yes         take the default for every prompt, ask nothing
  -h, --help        this text

Environment: FABER_REPO, FABER_REF, FABER_SRC mirror the flags.

Passing options through a pipe needs bash's -s:
  curl -fsSL .../install.sh | bash -s -- --with-app --yes

Re-running pulls the ref again and rebuilds the daemon (and the app, if asked).
A full update also needs the VM launcher and guest agent: docs/manual-update.md.
EOF
  exit 0
}

# --------------------------------------------------------------------------
# Prompting. Under `curl | bash` stdin *is* the script, so a bare `read` would
# eat the script's own remaining bytes. Read from the controlling terminal
# instead, and treat "no terminal" as "take the default" rather than as an
# error — that is what makes the same script usable unattended.
# --------------------------------------------------------------------------
TTY=""
ASSUME_YES=0

open_tty() {
  if [[ -t 1 ]] && : >/dev/tty 2>/dev/null; then TTY=/dev/tty; fi
  return 0
}

confirm() { # confirm <question> <default: y|n>
  local q="$1" def="${2:-n}" reply hint
  if (( ASSUME_YES )); then info "$q — yes (--yes)"; return 0; fi
  if [[ -z "$TTY" ]]; then
    # Unattended: take the documented default rather than failing. An installer
    # whose whole job is "install faber and what it builds with" saying no to
    # its own toolchains would just be a slower error message.
    if [[ $def == y ]]; then
      info "$q — yes (no terminal, taking the default)"
      return 0
    fi
    info "$q — no (no terminal, taking the default)"
    return 1
  fi
  hint="[y/N]"; [[ $def == y ]] && hint="[Y/n]"
  printf '    %s %s ' "$q" "$hint" >"$TTY"
  read -r reply <"$TTY" || reply=""
  reply="${reply:-$def}"
  [[ $reply =~ ^[Yy] ]]
}

# --------------------------------------------------------------------------
# Version comparison. macOS ships BSD sort, whose -V support has moved around
# across releases, so compare the dotted components directly instead.
# Always call this from an `if`/`&&` context — `set -e` is suspended there.
# --------------------------------------------------------------------------
ver_ge() { # ver_ge HAVE WANT -> 0 when HAVE >= WANT
  local h="${1%%[-+]*}" w="${2%%[-+]*}" h1 h2 h3 w1 w2 w3
  IFS=. read -r h1 h2 h3 <<<"$h"
  IFS=. read -r w1 w2 w3 <<<"$w"
  h1=${h1:-0}; h2=${h2:-0}; h3=${h3:-0}
  w1=${w1:-0}; w2=${w2:-0}; w3=${w3:-0}
  (( h1 != w1 )) && { (( h1 > w1 )); return; }
  (( h2 != w2 )) && { (( h2 > w2 )); return; }
  (( h3 >= w3 ))
}

# --------------------------------------------------------------------------
# Toolchains. Each ensure_* is idempotent: it reports and returns when the
# tool is already good, and otherwise explains what is missing and why faber
# needs it *before* asking to install it.
# --------------------------------------------------------------------------
have_clt() { xcode-select -p >/dev/null 2>&1; }

ensure_clt() {
  if have_clt; then info "Xcode Command Line Tools: present"; return; fi
  warn "Xcode Command Line Tools are missing — git, a C linker, and codesign all come from them."
  confirm "Open Apple's installer for them now?" y \
    || die "Command Line Tools are required. Run 'xcode-select --install', then re-run this script."
  xcode-select --install >/dev/null 2>&1 || true

  # The dialog is a GUI affair, so there is nobody to complete it on a headless
  # box. Say so immediately instead of burning the 30m timeout in silence.
  [[ -n "$TTY" ]] \
    || die "requested the Command Line Tools install; complete it on the machine's desktop, then re-run this script."

  info "Finish the install dialog macOS just opened — waiting for it to complete…"
  local waited=0
  until have_clt; do
    (( waited >= 1800 )) && die "still not installed after 30m; finish the dialog and re-run this script"
    sleep 5
    waited=$(( waited + 5 ))
    (( waited % 60 == 0 )) && info "…still waiting ($(( waited / 60 ))m)"
  done
  info "Xcode Command Line Tools: installed"
}

# shellcheck disable=SC1091  # written by rustup at run time, not in this repo
load_cargo_env() { [[ -s "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"; return 0; }

ensure_rust() {
  load_cargo_env
  local have=""
  command -v cargo >/dev/null 2>&1 && have="$(cargo --version 2>/dev/null | awk '{print $2}')"

  if [[ -n "$have" ]] && ver_ge "$have" "$RUST_MIN"; then
    info "Rust: $have"
    return
  fi

  if [[ -n "$have" ]]; then
    warn "Rust $have is older than the $RUST_MIN faber needs."
    if command -v rustup >/dev/null 2>&1 && confirm "Run 'rustup update stable' now?" y; then
      rustup update stable
      load_cargo_env
      have="$(cargo --version 2>/dev/null | awk '{print $2}')"
      ver_ge "$have" "$RUST_MIN" || die "still on Rust $have after the update; upgrade manually and re-run"
      info "Rust: $have"
      return
    fi
    die "Upgrade Rust to $RUST_MIN or newer, then re-run this script."
  fi

  warn "Rust (cargo) is missing — faber has no prebuilt binaries, so it is compiled here."
  confirm "Install the Rust toolchain via rustup now?" y \
    || die "Rust is required. Install it from https://rustup.rs, then re-run this script."
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y
  load_cargo_env
  command -v cargo >/dev/null 2>&1 || die "rustup finished but cargo is still not on PATH"
  info "Rust: $(cargo --version | awk '{print $2}')"
}

# nvm is a shell function, not a binary, so a non-interactive shell like this
# one never has it until nvm.sh is sourced. Do that before concluding Node is
# missing, or we would re-install Node for someone who already has it. nvm.sh
# reads unset variables, so `set -u` has to stand down while it loads.
load_nvm() {
  export NVM_DIR="${NVM_DIR:-$HOME/.nvm}"
  [[ -s "$NVM_DIR/nvm.sh" ]] || return 0
  set +u
  # shellcheck disable=SC1091
  . "$NVM_DIR/nvm.sh"
  set -u
  return 0
}

node_ok() {
  command -v node >/dev/null 2>&1 || return 1
  command -v npm  >/dev/null 2>&1 || return 1
  local v; v="$(node -v 2>/dev/null)"; v="${v#v}"
  ver_ge "$v" "$NODE_MIN"
}

install_nvm() {
  local tag=""
  tag="$(curl -fsSL https://api.github.com/repos/nvm-sh/nvm/releases/latest 2>/dev/null \
         | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
  if [[ -z "$tag" ]]; then
    tag="$NVM_PINNED_FALLBACK"
    info "could not reach the nvm release API — falling back to pinned $tag"
  fi
  info "installing nvm $tag (also adds it to your shell profile)"
  curl -fsSL "https://raw.githubusercontent.com/nvm-sh/nvm/$tag/install.sh" | bash
  load_nvm
  [[ -s "$NVM_DIR/nvm.sh" ]] || die "nvm install did not land at $NVM_DIR"
}

ensure_node() {
  load_nvm
  if node_ok; then info "Node: $(node -v) (npm $(npm -v))"; return; fi

  local why="Node $NODE_MIN+ is missing"
  command -v node >/dev/null 2>&1 && why="Node $(node -v) is older than the v$NODE_MIN needed"
  warn "$why — the menu-bar app is a Tauri bundle and npm builds its frontend."

  if [[ -s "${NVM_DIR:-$HOME/.nvm}/nvm.sh" ]]; then
    confirm "Install the current Node LTS with the nvm you already have?" y \
      || die "Node $NODE_MIN+ is required for the app. Install it, or re-run with --daemon-only."
  else
    confirm "Install nvm (Node Version Manager) and the current Node LTS now?" y \
      || die "Node $NODE_MIN+ is required for the app. Install it, or re-run with --daemon-only."
    install_nvm
  fi

  set +u
  nvm install --lts
  nvm use --lts
  set -u
  node_ok || die "Node still is not usable after the nvm install"
  info "Node: $(node -v) (npm $(npm -v))"
}

# --------------------------------------------------------------------------
# Source checkout. Shallow, and treated as ours to manage — but never blown
# away if someone has edited it, since --dir can point anywhere.
# --------------------------------------------------------------------------
clone_repo() {
  # Without this, git on a private repo blocks forever on a username prompt
  # that a piped stdin can never answer. Fail fast instead, then try gh.
  if GIT_TERMINAL_PROMPT=0 git clone --depth 1 --branch "$REF" "$REPO_URL" "$SRC_DIR" 2>/dev/null; then
    return
  fi

  # The gh retry only makes sense for an https github.com remote. For anything
  # else (ssh, a local path, a mirror) re-run the clone unmuted so the caller
  # sees git's own diagnosis instead of a misleading "not logged in".
  if [[ "$REPO_URL" != https://github.com/* ]]; then
    warn "could not clone $REPO_URL ($REF) — git's error follows:"
    GIT_TERMINAL_PROMPT=0 git clone --depth 1 --branch "$REF" "$REPO_URL" "$SRC_DIR"
    return
  fi

  # faber's own repo is public, so an anonymous clone normally just works. This
  # path is for the cases that remain: $REF does not exist, the network is
  # behind something, or FABER_REPO points at a private fork or mirror.
  warn "could not clone $REPO_URL anonymously — $REF may not exist, or the repo is private."
  command -v gh >/dev/null 2>&1 \
    || die "Check the ref, or authenticate: install GitHub CLI and run 'gh auth login', or set FABER_REPO to an SSH URL you can read."
  gh auth status >/dev/null 2>&1 \
    || die "gh is installed but not logged in — run 'gh auth login' as an account that can read the repo, then re-run."
  info "retrying with your gh credentials…"
  local slug="${REPO_URL#https://github.com/}"
  gh repo clone "${slug%.git}" "$SRC_DIR" -- --depth 1 --branch "$REF"
}

sync_source() {
  if [[ -e "$SRC_DIR" && ! -d "$SRC_DIR/.git" ]]; then
    die "$SRC_DIR exists but is not a git checkout — move it aside, or pass --dir <path>."
  fi

  if [[ -d "$SRC_DIR/.git" ]]; then
    # An update would discard local work, so refuse rather than surprise anyone.
    [[ -z "$(git -C "$SRC_DIR" status --porcelain)" ]] \
      || die "$SRC_DIR has uncommitted changes — commit or stash them, or pass --dir <path>."
    info "updating existing checkout at $SRC_DIR"
    git -C "$SRC_DIR" fetch --depth 1 origin "$REF"
    git -C "$SRC_DIR" reset --hard FETCH_HEAD
  else
    info "cloning $REPO_URL ($REF) -> $SRC_DIR"
    mkdir -p "$(dirname "$SRC_DIR")"
    clone_repo
  fi

  info "source @ $(git -C "$SRC_DIR" rev-parse --short HEAD) ($REF)"
}

# --------------------------------------------------------------------------
# What to install. The app needs a GUI login session and a Node toolchain, so
# it stays opt-in: the unattended default is the daemon alone, which is also
# the right answer on a headless runner host.
# --------------------------------------------------------------------------
choose_components() {
  [[ -n "$COMPONENTS" ]] && return 0

  if [[ -z "$TTY" ]]; then
    COMPONENTS="daemon"
    info "no terminal to ask — installing the daemon only (pass --with-app to add the app)"
    return 0
  fi

  cat >"$TTY" <<'EOF'

    What should be installed?

      1) daemon only             the always-on system service + dashboard on :8787
      2) daemon + menu-bar app   also builds the macOS tray app (needs Node 20+)

EOF
  local reply=""
  printf '    Choice [1]: ' >"$TTY"
  read -r reply <"$TTY" || reply=""
  case "${reply:-1}" in
    1) COMPONENTS="daemon" ;;
    2) COMPONENTS="daemon+app" ;;
    *) die "unrecognised choice '$reply' — pick 1 or 2." ;;
  esac
}

# --------------------------------------------------------------------------
# Runtime dependencies. Unlike the toolchains these only warn: faber installs
# and starts fine without them, and simply idles until they are set up.
#
# GitHub is not checked here: faber holds a token of its own (ADR-0027), handed
# over after the install with `faber auth link` and `sudo faber auth set`, and
# reads neither gh's login nor $GH_TOKEN.
# --------------------------------------------------------------------------
check_runtime_deps() {
  if docker info >/dev/null 2>&1; then
    info "Docker: running (only needed to build the runner image)"
  else
    warn "Docker is not running — you need one only to build the runner image the first time; faber's jobs run in its own Linux VM."
  fi
}

main() {
  local REPO_URL="${FABER_REPO:-$FABER_REPO_DEFAULT}"
  local REF="${FABER_REF:-main}"
  local SRC_DIR="${FABER_SRC:-$HOME/.faber/src}"
  local COMPONENTS=""

  while (( $# )); do
    case "$1" in
      --with-app)    COMPONENTS="daemon+app" ;;
      --daemon-only) COMPONENTS="daemon" ;;
      --ref)         REF="${2:?--ref needs a value}"; shift ;;
      --dir)         SRC_DIR="${2:?--dir needs a value}"; shift ;;
      -y|--yes)      ASSUME_YES=1 ;;
      -h|--help)     usage ;;
      *)             die "unknown option: $1 (try --help)" ;;
    esac
    shift
  done

  [[ "$(uname)" == "Darwin" ]] \
    || die "faber's service install is macOS-only today (launchd). Linux and Windows are on the roadmap."
  (( EUID != 0 )) \
    || die "do not run this as root — clone and build as yourself. The one privileged step, 'sudo faber setup', is invoked on its own from deploy/install.sh."

  open_tty

  say "faber installer"
  info "source   : $REPO_URL ($REF)"
  info "checkout : $SRC_DIR"
  info "host     : $(uname -s) $(uname -m)"

  choose_components

  say "Toolchains"
  ensure_clt
  ensure_rust
  if [[ "$COMPONENTS" == "daemon+app" ]]; then ensure_node; fi

  say "Source"
  sync_source

  # Hand off to the focused installers. They are the tested path the repo
  # already uses, and they stay the supported way to re-run a single piece.
  # deploy/install.sh ends by handing off to `sudo faber setup` (ADR-0025),
  # which prints its own plan and asks before changing anything — including
  # whether to turn on fleet autoupdate, which it is now the only place that
  # asks (never implied by --yes here, which only covers the toolchain
  # prompts above).
  say "Daemon"
  bash "$SRC_DIR/deploy/install.sh" --build

  if [[ "$COMPONENTS" == "daemon+app" ]]; then
    say "Menu-bar app"
    bash "$SRC_DIR/deploy/install-app.sh"
  fi

  say "Runtime dependencies"
  check_runtime_deps

  say "Done"
  cat <<EOF
  dashboard   : faber dashboard (prints and opens a one-time link)
  checkout    : $SRC_DIR
  upgrade     : re-run this installer (it pulls $REF and rebuilds the daemon);
                a full update also builds the VM launcher and guest agent:
                $SRC_DIR/docs/manual-update.md
  remove      : $SRC_DIR/deploy/uninstall.sh

  If "sudo faber setup" ran above (the default), faber now runs as its own
  _faber account — a system service, started at boot, kept awake and never
  needing auto-login. Its config is
      /Library/Application Support/faber/config.toml
  and its logs are in /Library/Logs/faber/ (both readable through group
  _faber_operator; open a new terminal first — that membership takes effect
  at your next login). A new host holds no GitHub token yet, so it sits in
  standby until step 1, and the dashboard says so.

  Four steps turn faber into CI:

  1. Hand faber its own GitHub token (ADR-0027). The first command prints a
     link to the GitHub page that pre-fills a fine-grained token for this host;
     pick the repos faber should serve there. The second stores it for _faber:
       faber auth link
       sudo faber auth set

  2. Opt a repo in — faber services PRIVATE repos carrying the topic:
       gh repo edit <owner>/<repo> --add-topic faber

  3. Route the work. In that repo's workflow, send trusted jobs to faber and
     everything else to GitHub — fork PRs must never reach your Mac. The full
     one-line expression is in the README under "Send your jobs to faber"; the shape is:
       runs-on: \${{ <is-this-a-fork-PR?> && 'ubuntu-latest'
                    || fromJSON('["faber","docker"]') }}

  4. Watch it happen: 'faber dashboard' (or 'faber status')

  The starter config (written when there was nothing to migrate) holds one
  Docker profile, linux-docker, with the labels faber and docker. Jobs routed
  to it wait, and the dashboard says why, until the runner image is in faber's
  Linux VM. Build it, save it, and hand it to setup:
    cd $SRC_DIR && docker build -t faber-runner:local docker/runner
    docker save faber-runner:local -o ~/faber-runner.tar
    sudo faber setup --runner-image-tar ~/faber-runner.tar
EOF
}

main "$@"
