#!/bin/sh
# faber's guest bootstrap (launcher/golden/README.md; docs/vm-agent-protocol.md, Inside the
# guest). Installed in the golden image as
#   /Library/Application Support/faber-guest/bootstrap.sh   root:wheel 0755
# and run as root at boot by io.github.rejifald.faber.guest-agent.plist.
#
# It waits (up to 180 s) for the clone's tools disk: the HFS+ volume FABERTOOLS that faber's
# hdiutil stamps (src/vm/tools_disk.rs), on read-only media that is not the boot disk. It
# unmounts any automount, mounts the volume read-only, nosuid, nodev and nobrowse at
# /private/var/faber/tools, and runs the guest agent from it as root. The image itself holds
# neither the agent nor the runner. A failure exits non-zero, and launchd starts it again.
set -eu
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
umask 022

label=FABERTOOLS
root_dir=/private/var/faber
mnt=$root_dir/tools
wait_s=180

log() { printf 'faber-bootstrap: %s\n' "$*" >&2; }

tmp=$(mktemp -d /private/tmp/faber-bootstrap.XXXXXX)
trap 'rm -rf "$tmp"' EXIT

# One key of a plist, or nothing.
key() { plutil -extract "$2" raw -o - "$1" 2>/dev/null || true; }

diskutil info -plist / > "$tmp/boot.plist"
boot_whole=$(key "$tmp/boot.plist" ParentWholeDisk)
[ -n "$boot_whole" ] || { log "cannot read the boot disk"; exit 1; }

# Sets tools_dev to the one device holding the tools disk; returns 1 when there is none yet.
# Two such volumes is a refusal, never a guess.
find_tools() {
  tools_dev=""
  diskutil list -plist > "$tmp/list.plist" || return 1
  n=$(key "$tmp/list.plist" AllDisks)
  case "$n" in ""|*[!0-9]*) return 1 ;; esac
  i=0
  while [ "$i" -lt "$n" ]; do
    dev=$(key "$tmp/list.plist" "AllDisks.$i")
    i=$((i + 1))
    case "$dev" in disk[0-9]*) ;; *) continue ;; esac
    case "$dev" in *[!a-z0-9]*) continue ;; esac
    diskutil info -plist "/dev/$dev" > "$tmp/info.plist" 2>/dev/null || continue
    [ "$(key "$tmp/info.plist" VolumeName)" = "$label" ] || continue
    [ "$(key "$tmp/info.plist" FilesystemType)" = hfs ] || continue
    [ "$(key "$tmp/info.plist" WritableMedia)" = false ] || continue
    whole=$(key "$tmp/info.plist" ParentWholeDisk)
    [ -n "$whole" ] && [ "$whole" != "$boot_whole" ] || continue
    if [ -n "$tools_dev" ]; then
      log "more than one $label volume; refusing to choose"
      exit 1
    fi
    tools_dev=$dev
  done
  [ -n "$tools_dev" ]
}

waited=0
until find_tools; do
  if [ "$waited" -ge "$wait_s" ]; then
    log "no read-only $label volume after ${wait_s}s"
    exit 1
  fi
  sleep 2
  waited=$((waited + 2))
done

# Unmount it wherever it is: an automount under /Volumes, or a mount left by an earlier run.
diskutil info -plist "/dev/$tools_dev" > "$tmp/tools.plist"
mounted=$(key "$tmp/tools.plist" MountPoint)
if [ -n "$mounted" ]; then
  if [ "$mounted" = "$mnt" ]; then
    umount -f "$mnt"
  else
    diskutil unmount force "/dev/$tools_dev" > /dev/null
  fi
fi

# The mount point: real directories, root's, writable by nobody else.
for d in "$root_dir" "$mnt"; do
  if [ -L "$d" ]; then log "$d is a symlink; refusing"; exit 1; fi
  [ -d "$d" ] || mkdir -m 0755 "$d"
  if [ "$(stat -f %u "$d")" != 0 ]; then log "$d is not root's; refusing"; exit 1; fi
  chmod go-w "$d"
done

mount -t hfs -o ro,nosuid,nodev,nobrowse "/dev/$tools_dev" "$mnt"
agent=$mnt/faber-agent
if [ ! -f "$agent" ] || [ -L "$agent" ]; then
  log "no faber-agent on the tools disk"
  exit 1
fi
log "tools disk /dev/$tools_dev mounted read-only at $mnt"
rm -rf "$tmp"
trap - EXIT
exec "$agent" serve
