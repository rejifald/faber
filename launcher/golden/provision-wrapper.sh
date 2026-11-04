#!/bin/sh
# The golden image's run-once provisioner launcher (ADR-0049). arm.sh installs it as
#   /Library/Application Support/faber-guest/provision-wrapper.sh   root:wheel 0755
# and io.github.rejifald.faber.golden-provision.plist runs it as root at boot.
#
# It waits (bounded) for exactly one read-only volume named FABERSETUP on media that is not the
# boot disk, mounts it read-only at /private/var/faber/setup, and runs provision.sh from it. It
# touches no network. Its only report channel is the virtio console, /dev/cu.virtio, as
# launcher/golden/README.md (Console protocol) describes.
set -eu
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
umask 022

tag="FABER-GOLDEN-BUILD"
label=FABERSETUP
root_dir=/private/var/faber
mnt=$root_dir/setup
wait_s=300

say() {
  if [ -c /dev/cu.virtio ]; then
    printf '\n%s %s\n' "$tag" "$1" >/dev/cu.virtio || true
  fi
}

handoff=0
tmp=""
wstep=setup-volume
# Every exit before the hand-off, set -e included, reports and powers off.
onexit() {
  [ -z "$tmp" ] || rm -rf "$tmp"
  if [ "$handoff" != 1 ]; then
    say "failed step=$wstep"
    sync
    shutdown -h now || /sbin/halt -q
  fi
}
trap onexit EXIT
fail() { wstep=$1; exit 1; }

# /dev/cu.* is world-writable: lock the console to root before anything runs as runner, or
# runner-owned code could forge a result line.
if [ -c /dev/cu.virtio ]; then
  chmod 0600 /dev/cu.virtio || fail console
  [ "$(stat -f %Lp /dev/cu.virtio)" = 600 ] && [ "$(stat -f %u /dev/cu.virtio)" = 0 ] || fail console
fi

tmp=$(mktemp -d /private/tmp/faber-provision-wrapper.XXXXXX) || fail setup-volume

key() { plutil -extract "$2" raw -o - "$1" 2>/dev/null || true; }

diskutil info -plist / > "$tmp/boot.plist" || fail setup-volume
boot_whole=$(key "$tmp/boot.plist" ParentWholeDisk)
[ -n "$boot_whole" ] || fail setup-volume

# Sets setup_dev to the one device holding the setup volume; returns 1 when there is none yet.
# Two such volumes is a failure, never a guess.
find_setup() {
  setup_dev=""
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
    [ "$(key "$tmp/info.plist" WritableMedia)" = false ] || continue
    whole=$(key "$tmp/info.plist" ParentWholeDisk)
    [ -n "$whole" ] && [ "$whole" != "$boot_whole" ] || continue
    if [ -n "$setup_dev" ]; then
      say "step=setup-volume-ambiguous"
      fail setup-volume
    fi
    setup_dev=$dev
  done
  [ -n "$setup_dev" ]
}

waited=0
until find_setup; do
  [ "$waited" -lt "$wait_s" ] || fail setup-volume
  sleep 2
  waited=$((waited + 2))
done

# Unmount an automount, or a mount left by an earlier run.
diskutil info -plist "/dev/$setup_dev" > "$tmp/setup.plist"
mounted=$(key "$tmp/setup.plist" MountPoint)
if [ -n "$mounted" ]; then
  if [ "$mounted" = "$mnt" ]; then
    umount -f "$mnt" || fail setup-volume
  else
    diskutil unmount force "/dev/$setup_dev" >/dev/null || fail setup-volume
  fi
fi

for d in "$root_dir" "$mnt"; do
  [ ! -L "$d" ] || fail setup-volume
  [ -d "$d" ] || mkdir -m 0755 "$d" || fail setup-volume
  [ "$(stat -f %u "$d")" = 0 ] || fail setup-volume
  chmod go-w "$d" || fail setup-volume
done

fs=$(key "$tmp/setup.plist" FilesystemType)
mount -t "${fs:-hfs}" -o ro,nosuid,nodev,nobrowse "/dev/$setup_dev" "$mnt" || fail setup-volume
[ -f "$mnt/provision.sh" ] && [ ! -L "$mnt/provision.sh" ] || fail setup-volume

rm -rf "$tmp"
tmp=""
handoff=1
trap - EXIT
# provision.sh reports, removes this wrapper and its daemon, and powers off, on success and on
# failure alike. It is exec'd so that nothing here can speak after its final line.
exec /bin/sh "$mnt/provision.sh"
