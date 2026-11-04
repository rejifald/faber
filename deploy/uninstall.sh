#!/usr/bin/env bash
#
# Remove what `sudo faber setup` installed (ADR-0025): wraps `sudo faber
# uninstall`, which drains the daemon, stops the service, removes faber's
# files, and deletes the `_faber` / `_faber_vm` accounts last. It shows its
# plan and asks before changing anything.
#
#   ./deploy/uninstall.sh              # plan, confirm, then remove everything
#   ./deploy/uninstall.sh --keep-logs  # copy audit.jsonl + config.toml to your home first
#   ./deploy/uninstall.sh --yes        # skip the confirmation prompt
#   ./deploy/uninstall.sh --dry-run    # print the plan, change nothing
#
# *** RUN THIS FROM Terminal.app. *** macOS refuses to delete an account —
# the last step — from a terminal hosted inside another application
# (`eDSPermissionError`: it would mean granting that application "administer
# this Mac" and Full Disk Access just to remove one account, and denying that
# trade is correct). Everything before the accounts is idempotent, so
# re-running this from Terminal.app after such a refusal picks up exactly
# where it stopped.
#
set -euo pipefail

FABER_BIN="${FABER_BIN:-/usr/local/bin/faber}"
[[ -x "$FABER_BIN" ]] || {
  echo "no faber binary at $FABER_BIN — nothing installed to remove, or it was built" >&2
  echo "elsewhere: pass FABER_BIN=<path>." >&2
  exit 1
}

exec sudo "$FABER_BIN" uninstall "$@"
