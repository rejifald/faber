#!/usr/bin/env bash
#
# Keep the Mac awake so the daemon never gets suspended — a sleeping Mac
# suspends faber (and any in-flight runner) until it wakes.
#
# `sudo faber setup` (ADR-0025) already does this, as its last step, unless
# run with --skip-keep-awake — the daemon is a LaunchDaemon now, so it starts
# at boot with nobody logged in and needs no auto-login either way. This
# script is for running the same two `pmset` settings by hand: on a host that
# skipped that step, or standalone, without the rest of setup.
#
# The narrower alternative needs no sudo: `[host] prevent_sleep = "while_busy"`
# makes faber hold off idle sleep only while a job is running (issue #90);
# `"always"` holds it for as long as the daemon runs.
#
# *** RUN THIS YOURSELF, WITH sudo. *** `pmset` changes system-wide power
# settings. It is fully reversible (teardown shown at the end). Claude will not
# run it for you.
#
#   sudo ./deploy/keep-awake.sh
#
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
  echo "run with sudo: sudo ./deploy/keep-awake.sh" >&2
  exit 1
fi

echo "current power settings:"
pmset -g custom | grep -Ei 'sleep|powernap' || true
echo

# sleep 0   : never put the *system* to sleep (the display may still sleep —
#             that does not suspend the daemon).
# powernap 0: don't drop into the low-power Power Nap state.
echo "==> pmset -a sleep 0 powernap 0"
pmset -a sleep 0
pmset -a powernap 0

echo
echo "new power settings:"
pmset -g custom | grep -Ei 'sleep|powernap' || true

cat <<'EOF'

note: this keeps the *system* awake. Display sleep is fine and does not suspend
faber. If you also want to survive lid-close on a laptop, add: sudo pmset -a disablesleep 1

to undo later (restore sensible defaults):
  sudo pmset -a sleep 1
  sudo pmset -a powernap 1
EOF
