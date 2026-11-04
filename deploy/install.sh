#!/usr/bin/env bash
#
# Build (or locate) the faber binary, then hand off to `sudo faber setup`
# (ADR-0025) — the privileged step that creates the hidden `_faber` and
# `_faber_vm` accounts, installs faber as a system service, and migrates any
# per-user install it finds. `setup` shows its plan and asks before it
# changes anything; this script itself needs no privileges up to that point.
#
#   ./deploy/install.sh            # use target/release/faber (build if missing)
#   ./deploy/install.sh --build    # force `cargo build --release` first
#   FABER_BIN=/usr/local/bin/faber ./deploy/install.sh   # use a prebuilt binary
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

force_build=0
[[ "${1:-}" == "--build" ]] && force_build=1

# 1. Resolve the binary to run `setup` from: an explicit FABER_BIN wins;
#    otherwise build from / use the repo's release artifact. `sudo faber
#    setup` copies *this* binary to /usr/local/bin/faber (root-owned) and to
#    _faber's own state directory — whatever is resolved here is what faber
#    runs from for good (see the root invariant in AGENTS.md: root refuses to
#    run a binary either of faber's accounts could have written).
if [[ -n "${FABER_BIN:-}" ]]; then
  SRC_BIN="$FABER_BIN"
  [[ -x "$SRC_BIN" ]] || { echo "FABER_BIN=$SRC_BIN is not an executable" >&2; exit 1; }
else
  SRC_BIN="$REPO_DIR/target/release/faber"
  if [[ "$force_build" == 1 || ! -x "$SRC_BIN" ]]; then
    command -v cargo >/dev/null || { echo "cargo not found; build elsewhere and pass FABER_BIN" >&2; exit 1; }
    command -v npm >/dev/null || { echo "npm not found: the daemon embeds the web UI built in ui/" >&2; exit 1; }
    echo "==> npm ci && npm run build (ui/)"
    ( cd "$REPO_DIR/ui" && npm ci && npm run build )
    echo "==> cargo build --release"
    ( cd "$REPO_DIR" && FABER_REQUIRE_UI=1 cargo build --release )
  fi
fi

# 2. macOS: hand off only to a binary whose setup leaves the host a runnable
#    VM runtime (ADR-0026; #223). Under _faber the daemon cannot use the
#    operator's Docker Desktop and there is no native backend, so without its VMs
#    the host would run no jobs at all. The binary judges it itself, without
#    sudo (`setup --check-vm-runtime`); one from before that flag existed fails
#    it, which is the right answer. Otherwise: the binary swap.
swap_recipe() {
  cat <<EOF

Not handing off to "sudo faber setup": it would leave this Mac running no jobs.
Update it by swapping the binary instead (docs/manual-update.md, "Install").
Drain first (step 4 there), then:

  D="\$HOME/Library/Application Support/faber"
  cp -p "\$D/bin/faber" "\$D/bin/faber.bak-\$(date +%Y%m%d)"
  install -m 0755 "$SRC_BIN" "\$D/bin/faber.new"
  mv -f "\$D/bin/faber.new" "\$D/bin/faber"
  launchctl kickstart -k "gui/\$(id -u)/io.github.rejifald.faber"

That recipe is for a host still on the per-user layout. If /etc/faber/setup.json
exists, this host already runs as _faber: see docs/manual-update.md.
To set up anyway, for testing only: sudo "$SRC_BIN" setup --before-vm-runtime
EOF
}
if [[ "$(uname -s)" == Darwin ]]; then
  echo "==> checking that setup from $SRC_BIN leaves a runnable VM runtime"
  if ! "$SRC_BIN" setup --check-vm-runtime; then
    swap_recipe >&2
    exit 1
  fi
fi

echo
echo "==> handing off to: sudo $SRC_BIN setup"
echo "    creates _faber + _faber_vm, installs the system service, and migrates"
echo "    any per-user install it finds. It prints its plan and asks before"
echo "    changing anything — see deploy/README.md."

# 3. Run it, or say how to. sudo itself prompts on the controlling terminal
#    either way, but `setup`'s own confirmation reads *stdin* — and under
#    `curl | bash` stdin is the piped script, not a terminal. Redirect it from
#    /dev/tty explicitly: the same trick this repo's other installers use to
#    prompt past a piped stdin (see install.sh's own confirm()).
if [[ -t 1 ]] && : >/dev/tty 2>/dev/null; then
  # shellcheck disable=SC2024  # a read redirect, not sudo gaining write access
  sudo "$SRC_BIN" setup </dev/tty
else
  cat <<EOF

no terminal to run "setup" interactively from. Run it yourself:
  sudo $SRC_BIN setup
EOF
fi
