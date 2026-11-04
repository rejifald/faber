#!/bin/sh
# Arm the golden image's run-once provisioner (ADR-0049). It runs inside the offline window VM,
# once Setup Assistant is done, from the read-only setup disk:
#
#   sudo sh /Volumes/FABERSETUP/arm.sh
#
# It installs, root-owned, a LaunchDaemon and its wrapper. At the NEXT boot, which is the
# walled build's, the wrapper finds this volume again and runs provision.sh from it as root.
# Then arm.sh shuts the guest down. It downloads nothing and the window VM has no network.
#
# It refuses, touching nothing, if it is not root, a `runner` user already exists, or FileVault
# is on (automatic login needs it off).
set -eu
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH

src=$(cd "$(dirname "$0")" && pwd)
dir="/Library/Application Support/faber-guest"
plist=/Library/LaunchDaemons/io.github.rejifald.faber.golden-provision.plist

refuse() { printf 'arm.sh: %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || refuse "run it as root: sudo sh $0"
if id runner >/dev/null 2>&1; then
  refuse "a user named runner exists. The build makes that user itself; name the admin account something else, or start from a fresh install."
fi
if [ "$(fdesetup isactive 2>/dev/null || true)" = true ]; then
  refuse "FileVault is on. Automatic login needs it off: turn it off in System Settings > Privacy & Security, wait for it to finish, then run this again."
fi
for f in provision.sh provision-wrapper.sh io.github.rejifald.faber.golden-provision.plist bootstrap.sh \
  io.github.rejifald.faber.guest-agent.plist; do
  [ -f "$src/$f" ] || refuse "$src/$f is missing; run this from the setup disk"
done

install -d -o root -g wheel -m 0755 "$dir"
install -o root -g wheel -m 0755 "$src/provision-wrapper.sh" "$dir/provision-wrapper.sh"
install -o root -g wheel -m 0644 "$src/io.github.rejifald.faber.golden-provision.plist" "$plist"
plutil -lint "$plist"
ls -le "$plist" "$dir/provision-wrapper.sh"

# Updates off now, offline, so they are off before the first networked boot. provision.sh
# sets them again.
for k in AutomaticDownload CriticalUpdateInstall ConfigDataInstall AutomaticCheckEnabled; do
  defaults write /Library/Preferences/com.apple.SoftwareUpdate "$k" -bool false
done
defaults write /Library/Preferences/com.apple.commerce AutoUpdate -bool false

echo "armed: the next boot runs provision.sh from the FABERSETUP volume. Shutting down now."
sync
shutdown -h now
