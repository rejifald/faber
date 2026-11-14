#!/bin/sh
# What CI runs for the launcher (.github/workflows/ci.yml, "launcher · macos"): build and
# sign it and the guest agent, run the unit tests, then drive the real binary's validate over a fixture Linux
# bundle. CI's macOS runners cannot run VMs, so validate runs with --wall-only on Apple's
# shared NAT: the configuration is built and held to the wall, and the framework is never
# asked. Booting is the owner's builder-side check (launcher/README.md).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${FABER_VM_OUT:-${TMPDIR:-/tmp}/faber-vm}
export FABER_VM_OUT=$out

"$here/build.sh"
"$here/build.sh" --agent
"$here/test.sh"

vm=$out/faber-vm
"$vm" --version
# The guest agent runs only inside a guest; here it is built, signed with no entitlements, and
# asked its version. The bootstrap script must at least parse.
"$out/faber-agent" --version
sh -n "$here/golden/bootstrap.sh"
sh -n "$here/golden/arm.sh"
sh -n "$here/golden/provision.sh"
sh -n "$here/golden/provision-wrapper.sh"
# The golden image's build tool: it must compile, sign with exactly the virtualization
# entitlement, and parse. Installing and booting macOS is the owner's (golden/README.md).
"$here/golden/build.sh" --compile-only
"$out/faber-golden-prep" --version | /usr/bin/grep -Eq '^faber-golden-prep [0-9]+\.[0-9]+\.[0-9]+\+g[0-9a-f]{7,40}$'
plutil -lint "$here/golden/io.github.rejifald.faber.guest-agent.plist"
plutil -lint "$here/golden/io.github.rejifald.faber.golden-provision.plist"

work=$(mktemp -d "${TMPDIR:-/tmp}/faber-vm-ci.XXXXXX")
trap 'rm -rf "$work"' EXIT
bundle=$work/linux
mkdir "$bundle"
mkfile -n 64m "$bundle/disk.raw"
before=$(ls -A "$bundle")

# The guest size every validate below asks for. The framework caps a guest's memory
# by the host's: about 7 GiB on GitHub's macOS runners, where validate refuses the
# default 8 GiB. The supervisor always passes --memory-mib, so this is how production
# calls it. The refusals get it too, so each differs from the accepted call only in
# what it breaks: today each is refused before the memory check, and none can start
# passing on the memory cap if that order changes.
fit="--memory-mib 4096"

echo "--- validate accepts the launcher's own configuration"
"$vm" validate "$bundle" --slot 0 --network shared-nat $fit --wall-only | tee "$work/ok.txt"
/usr/bin/grep -q '^wall=ok' "$work/ok.txt"
[ "$(ls -A "$bundle")" = "$before" ] || { echo "validate wrote into the bundle" >&2; exit 1; }

# Each must be refused, with a non-zero exit and no wall=ok.
refuse() {
  label=$1; shift
  if "$vm" "$@" > "$work/refused.txt" 2>&1; then
    echo "FAIL: validate accepted: $label" >&2
    cat "$work/refused.txt" >&2
    exit 1
  fi
  if /usr/bin/grep -q '^wall=ok' "$work/refused.txt"; then
    echo "FAIL: refused but printed wall=ok: $label" >&2
    exit 1
  fi
  echo "refused: $label"
}

echo "--- validate refuses what breaks the wall"
refuse "no slot" validate "$bundle" --network shared-nat $fit --wall-only
refuse "slot out of range" validate "$bundle" --slot 16 --network shared-nat $fit --wall-only
refuse "vmnet options on shared NAT" validate "$bundle" --slot 0 --network shared-nat --subnet 192.168.100.0/24 $fit --wall-only
refuse "vmnet without a network to check" validate "$bundle" --slot 0 --network vmnet --subnet 192.168.100.0/24 $fit --wall-only
refuse "unknown network mode" validate "$bundle" --slot 0 --network bridged $fit --wall-only
refuse "unknown option" validate "$bundle" --slot 0 --network shared-nat --share-dir /tmp $fit --wall-only

mkdir "$work/both"
mkfile -n 1m "$work/both/disk.raw"
mkfile -n 1m "$work/both/vz-hwmodel"
refuse "a bundle of both kinds" validate "$work/both" --slot 0 --network shared-nat $fit --wall-only

mkdir "$work/symlinked"
ln -s "$bundle/disk.raw" "$work/symlinked/disk.raw"
refuse "a symlinked disk" validate "$work/symlinked" --slot 0 --network shared-nat $fit --wall-only

ln -s "$work/elsewhere.log" "$work/console.log"
refuse "a symlinked console log" validate "$bundle" --slot 0 --network shared-nat --console-log "$work/console.log" $fit --wall-only

# The real binary's supervisor: it listens, answers a handshaken list, refuses a request
# before hello, and exits on exit, removing its socket. No VM is started.
echo "--- supervise answers on its socket and exits on exit"
# A Unix socket path must fit in 103 bytes and a runner's $TMPDIR can be deeper than that,
# so the supervisor's directories (its socket, and the state dir holding the children's
# slot-N.sock) live under a short /tmp directory.
sup=$(mktemp -d /tmp/fvm.XXXXXX)
chmod 700 "$sup"
mkdir -m 700 "$sup/state" "$sup/images" "$sup/tools" "$sup/clones"
"$vm" supervise --socket "$sup/s" --peer-uid "$(id -u)" --state-dir "$sup/state" --image-dir "$sup/images" \
  --tools-dir "$sup/tools" --clone-dir "$sup/clones" --network shared-nat 2> "$work/supervise.log" &
sup_pid=$!
trap 'kill "$sup_pid" 2>/dev/null || true; rm -rf "$work" "$sup"' EXIT
i=0
until [ -S "$sup/s" ]; do
  i=$((i + 1))
  if [ "$i" -gt 100 ]; then echo "FAIL: supervise never listened" >&2; cat "$work/supervise.log" >&2; exit 1; fi
  sleep 0.1
done
call() {
  want=$1 frame=$2; shift 2
  printf '%s\n' "$frame" | "$vm" supervisor-call --socket "$sup/s" "$@" | tee "$work/call.txt"
  /usr/bin/grep -qF "$want" "$work/call.txt" || { echo "FAIL: expected $want" >&2; exit 1; }
}
call 'reply={"id":2,"reply":{"vms":{"vms":[]}}}' '{"id":2,"request":{"list":{}}}'
call 'reply={"id":2,"reply":{"error":{"code":"handshake_required"}}}' '{"id":2,"request":{"list":{}}}' --no-hello
call 'reply={"id":3,"reply":{"done":{}}}' '{"id":3,"request":{"exit":{}}}'
wait "$sup_pid" || { echo "FAIL: supervise exited non-zero" >&2; cat "$work/supervise.log" >&2; exit 1; }
[ ! -e "$sup/s" ] || { echo "FAIL: supervise left its socket behind" >&2; exit 1; }

echo "launcher ci: ok"
