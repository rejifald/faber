#!/usr/bin/env bash
#
# faber's JIT contract, and the whole of it: given RUNNER_JITCONFIG, run the
# GitHub Actions runner once with that config and exit when the job is done.
#
# faber registers the runner with GitHub itself and hands the resulting JIT blob
# in this variable, so there is no token here, no `config.sh`, and no
# registration step that could outlive the container. The runner is
# single-use by construction: one job, then it returns.
#
# Runs as root only long enough to fix up mount ownership, then drops to the
# unprivileged `runner` user with exec, so the runner keeps PID 1 and receives
# the SIGTERM faber sends it on teardown.
set -euo pipefail

RUNNER_UID=1001
RUNNER_GID=1001
CACHE_DIR=/runner/_cache

if [[ -z "${RUNNER_JITCONFIG:-}" ]]; then
  cat >&2 <<'EOF'
RUNNER_JITCONFIG is not set.

This image is started by faber, which registers a just-in-time runner with
GitHub and passes its config in that variable. It has no interactive mode: run
it by pointing a faber profile at this image, not by hand.

To sanity-check the image itself instead:
  docker run --rm --entrypoint bash faber-runner:local -lc 'node -v; git --version; ls /runner/run.sh'
EOF
  exit 64
fi

# The opt-in per-repo build cache (ADR-0005) arrives as a named volume. Docker
# creates a mount point the image does not already contain as root:root 0755, so
# on a repo's very first job it is not writable by uid 1001 and every cache write
# fails with EACCES. Ownership lives in the volume, so doing this once here is
# enough — every later job finds it already correct. Only touch the top level:
# chown -R over a warm cache would add seconds to every single job.
if [[ -d "$CACHE_DIR" ]]; then
  chown "$RUNNER_UID:$RUNNER_GID" "$CACHE_DIR" || true
fi

# Handed to run.sh through the environment, under the name the runner's own
# CLI parser reads as its fallback — never a command-line argument. A security
# review found this entrypoint passing the credential on argv, and
# /proc/<pid>/cmdline is world-readable on a native Linux dockerd host (a
# container's process is an ordinary host process there): any local account
# could have read a runner's one-time registration secret and taken its job.
# ACTIONS_RUNNER_INPUT_JITCONFIG is upstream's own mechanism, not faber's
# invention — its CommandSettings constructor reads any
# ACTIONS_RUNNER_INPUT_<ARG> variable as that argument's fallback, then scrubs
# it from its own process environment before a job step ever runs (verified
# against actions/runner's source; see src/runner.rs, which uses the same
# variable for the native backend). RUNNER_JITCONFIG is unset first so the old
# name never rides into the runner's process tree — nothing there would scrub
# it, and every job step would otherwise inherit it from its own environment.
export ACTIONS_RUNNER_INPUT_JITCONFIG="$RUNNER_JITCONFIG"
unset RUNNER_JITCONFIG

# `setpriv` passes the environment through by default (no `--reset-env` here,
# which would drop the variable just exported above), and execs rather than
# forking, unlike `su`, so signals keep reaching the runner instead of dying in
# an intermediate shell.
exec setpriv --reuid="$RUNNER_UID" --regid="$RUNNER_GID" --init-groups -- \
  /runner/run.sh
