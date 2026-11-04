#!/bin/sh
# Build and run faber-vm's unit tests. They boot no VM and create no vmnet network unless
# FABER_VM_TEST_VMNET=1, so they run on CI's macOS runners, which cannot run VMs; a test
# that needs what a host lacks is skipped with its reason, never failed. Exits non-zero on
# any failure.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${FABER_VM_OUT:-${TMPDIR:-/tmp}/faber-vm}
FABER_VM_OUT=$out "$here/build.sh" --tests
# The protocol fixtures are shared with the Rust tests; the path comes from this script's own
# location, and the tests fail (never skip) when it is missing or empty.
exec "$out/faber-vm-tests" "$here/faber-vm.entitlements" "$here/../tests/fixtures/vm-proto"
