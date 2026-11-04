#!/bin/sh
# The golden macOS image, provisioned once in the throwaway build VM that
# `sudo faber golden build` boots behind the wall (ADR-0049, ADR-0026 *The golden image*).
# provision-wrapper.sh, which arm.sh installed, finds the read-only FABERSETUP volume and runs
# this from it as root. This is the macOS twin of launcher/linux/provision.sh. POSIX sh and
# Apple's tools only. It reads recipe.env from the same volume, and it powers the VM off.
#
# Its words are lines on the virtio console, /dev/cu.virtio, each starting FABER-GOLDEN-BUILD
# (launcher/golden/README.md, *Console protocol*): `wall-not-up`, `no-network`, `step=<name>`,
# `fact <key>=<value>`, then `ok` or `failed step=<name>`. The tag is assembled at run time so
# this file's own text never reads as a result line.
#
# recipe.env, one KEY=value per line, no quoting, `#` starts a comment:
#   XCODE_KIND       xip or app
#   XCODE_FILE       the Xcode file's name on the volume: an Xcode*.xip, or an Xcode*.app folder
#   RUNTIMES         space-separated simulator platforms: iOS, watchOS, tvOS or visionOS, each
#                    optionally pinned as <platform>:<version>, such as "iOS iOS:18.5 watchOS"
#   BREW_EXTRA       space-separated Homebrew formulas to add to the fixed ones (jq gh zstd git-lfs)
#   HOMEBREW_TAG     the Homebrew/brew release tag, such as 4.6.0
#   HOMEBREW_SHA256  the SHA-256 of that tag's tarball, 64 lowercase hex digits
# Every value is validated before it is used.
set -eu
PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH
umask 022

tag="FABER-GOLDEN-BUILD"
src=$(cd "$(dirname "$0")" && pwd)
gateway=192.168.239.1      # the build network's gateway (slot 15), as the Linux build's
resolvers="1.1.1.1 1.0.0.1"  # the wall's default resolvers (src/wall/grants.rs), never the gateway
brew_prefix=/opt/homebrew
guest_dir="/Library/Application Support/faber-guest"
daemon=/Library/LaunchDaemons/io.github.rejifald.faber.golden-provision.plist
step=start
reported=0
done_ok=0

say() {
  line="$tag $1"
  printf '%s\n' "$line"
  if [ -c /dev/cu.virtio ]; then
    # Printable ASCII only, at most 199 bytes plus the newline.
    # The newline first, so a line is never glued to other console output.
    printf '\n%s\n' "$line" | LC_ALL=C tr -cd '\n -~' | LC_ALL=C cut -c1-199 >/dev/cu.virtio || true
  fi
}

begin() { step=$1; say "step=$1"; }

finish() {
  # The run-once daemon and its wrapper go on every exit path, so a clone never re-runs
  # provisioning. (The build boots a clone; the operator's bundle stays armed for a retry.)
  rm -f "$daemon" "$guest_dir/provision-wrapper.sh"
  if [ "$done_ok" != 1 ] && [ "$reported" != 1 ]; then
    say "failed step=$step"
  fi
  sync
  shutdown -h now || /sbin/halt -q
}
trap finish EXIT

[ "$(id -u)" -eq 0 ] || exit 1

# --- a. Software Update off, before any network use ---------------------------------------
# `softwareupdate --schedule off` does not take effect on macOS 27; these keys do.
# A clone's boot can drop one, so the guest agent writes this same list again at every boot
# and before every job (launcher/agent/Sources/Updates.swift). launcher/Tests/UpdatesTests.swift
# reads this step and holds the two equal: change one, change both.
begin sw-update
for k in AutomaticDownload CriticalUpdateInstall ConfigDataInstall AutomaticCheckEnabled; do
  defaults write /Library/Preferences/com.apple.SoftwareUpdate "$k" -bool false
done
defaults write /Library/Preferences/com.apple.commerce AutoUpdate -bool false

# --- b. the guest's NIC has a network service ----------------------------------------------
# `faber golden prepare` installs macOS and runs Setup Assistant with no NIC attached, so no
# network service exists for the one this VM has: en0 stays down and sends nothing, not even
# DHCP. Create the default (DHCP) service for it. This script fetches nothing before the wall
# gate; what macOS itself sends once en0 is up (DHCP, then IPv6 neighbour discovery, mDNS,
# NTP, a captive-portal probe) meets the host's wall, which drops all IPv6.
begin network
networksetup -detectnewhardware

# --- c. the wall is up before anything is fetched (#236) ------------------------------------
# As the Linux build: the anchor's first rule for this guest drops everything to the gateway,
# and the anchor loads as a unit, so DNS to the gateway going unanswered three times in a row
# shows the whole rule set is bound. An answer, or a refusal, means not yet. No wall by the
# deadline: report it and power off, having fetched nothing.
# A timeout only counts once the guest has an address on the build network and its route to
# the gateway uses that interface: before that, silence means no route yet, not a wall.
net_ready() {
  rt=$(route -n get "$gateway" 2>/dev/null) || return 1
  ifc=$(printf '%s\n' "$rt" | awk '$1 == "interface:" { print $2; exit }')
  case "$ifc" in ""|*[!a-z0-9]*) return 1 ;; esac
  ifconfig "$ifc" 2>/dev/null | /usr/bin/grep -q 'inet 192\.168\.239\.'
}
# Returns 0 for a wall, 1 for none by the deadline, 2 for never an address on the build
# network by the deadline (so nothing could be judged).
wall_gate() {
  misses=0
  had_net=0
  deadline=$(( $(date +%s) + 120 ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if ! net_ready; then
      misses=0
      sleep 1
      continue
    fi
    had_net=1
    # Exit 0 only for "no reply within 2 s" on a connected UDP socket. A refusal (ECONNREFUSED,
    # which dig would also call a timeout), any send or receive error, or any reply exits 1.
    if perl -MIO::Socket::INET -e '
        my $s = IO::Socket::INET->new(PeerAddr => "$ARGV[0]:53", Proto => "udp") or exit 1;
        $s->send(pack("H*", "fabe010000010000000000000000020001")) or exit 1;
        my $r = ""; vec($r, fileno($s), 1) = 1;
        my $n = select($r, undef, undef, 2);
        exit 1 if !defined $n || $n < 0;
        exit 0 if $n == 0;
        exit 1;' "$gateway"; then
      misses=$((misses + 1))
      [ "$misses" -ge 3 ] && return 0
      continue
    else
      misses=0
    fi
    sleep 1
  done
  [ "$had_net" = 1 ] || return 2
  return 1
}
begin wall
gate=0
wall_gate || gate=$?
if [ "$gate" = 2 ]; then
  say "no-network"
  reported=1
  exit 1
elif [ "$gate" != 0 ]; then
  say "wall-not-up"
  reported=1
  exit 1
fi

# --- the recipe ---------------------------------------------------------------------------
begin recipe
XCODE_KIND="" XCODE_FILE="" RUNTIMES="" BREW_EXTRA="" HOMEBREW_TAG="" HOMEBREW_SHA256=""
[ -f "$src/recipe.env" ] && [ ! -L "$src/recipe.env" ] || exit 1
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    ""|"#"*) continue ;;
    XCODE_KIND=*) XCODE_KIND=${line#*=} ;;
    XCODE_FILE=*) XCODE_FILE=${line#*=} ;;
    RUNTIMES=*) RUNTIMES=${line#*=} ;;
    BREW_EXTRA=*) BREW_EXTRA=${line#*=} ;;
    HOMEBREW_TAG=*) HOMEBREW_TAG=${line#*=} ;;
    HOMEBREW_SHA256=*) HOMEBREW_SHA256=${line#*=} ;;
    *) exit 1 ;;
  esac
done < "$src/recipe.env"

case "$XCODE_KIND" in
  xip) case "$XCODE_FILE" in Xcode*.xip) ;; *) exit 1 ;; esac ;;
  app) case "$XCODE_FILE" in Xcode*.app) ;; *) exit 1 ;; esac ;;
  *) exit 1 ;;
esac
case "$XCODE_FILE" in .*|*[!A-Za-z0-9._-]*) exit 1 ;; esac
[ -e "$src/$XCODE_FILE" ] && [ ! -L "$src/$XCODE_FILE" ] || exit 1

for r in $RUNTIMES; do
  p=${r%%:*}
  case "$p" in iOS|watchOS|tvOS|visionOS) ;; *) exit 1 ;; esac
  case "$r" in
    *:*) v=${r#*:}; case "$v" in ""|.*|*.|*[!0-9.]*) exit 1 ;; esac ;;
  esac
done
for f in $BREW_EXTRA; do
  case "$f" in -*|*[!A-Za-z0-9@._+-]*) exit 1 ;; esac
done
case "$HOMEBREW_TAG" in
  [0-9]*.[0-9]*.[0-9]*) case "$HOMEBREW_TAG" in *[!0-9.]*|*..*|*.) exit 1 ;; esac ;;
  *) exit 1 ;;
esac
case "$HOMEBREW_SHA256" in
  *[!0-9a-f]*) exit 1 ;;
esac
[ ${#HOMEBREW_SHA256} -eq 64 ] || exit 1

# --- d. resolvers, and the runner user ----------------------------------------------------
# Resolvers first: from here on the guest fetches, through the wall's resolvers only.
begin resolvers
services=$(mktemp /private/tmp/faber-services.XXXXXX)
networksetup -listallnetworkservices | sed 1d | sed 's/^\*//' > "$services"
while IFS= read -r service; do
  # shellcheck disable=SC2086 # one argument per resolver
  networksetup -setdnsservers "$service" $resolvers
done < "$services"
rm -f "$services"

# runner's password is random and kept nowhere but /etc/kcpassword, which automatic login
# needs. It is on sysadminctl's argv for a moment, inside the guest only.
begin runner
! id runner >/dev/null 2>&1 || exit 1
pw=$(openssl rand -base64 24)
sysadminctl -addUser runner -fullName "faber runner" -password "$pw" -home /Users/runner
id runner >/dev/null 2>&1 || exit 1
createhomedir -c -u runner >/dev/null 2>&1 || true
sysadminctl -autologin set -userName runner -password "$pw" || true
auto=$(defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser 2>/dev/null || true)
if [ "$auto" != runner ] || [ ! -f /etc/kcpassword ]; then
  # Without a desktop session sysadminctl may not set it. Write what it writes: the password,
  # NUL-padded to a multiple of 12 bytes, XORed with the fixed key loginwindow reads it with.
  (umask 077; printf '%s' "$pw" | perl -e '
    @k = (0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F);
    local $/; $p = <STDIN>; $p .= "\0"; $p .= "\0" while (length($p) % 12);
    $out = ""; $i = 0;
    $out .= chr(ord($_) ^ $k[$i++ % @k]) for split //, $p;
    print $out;' > /etc/kcpassword)
  chown root:wheel /etc/kcpassword
  chmod 600 /etc/kcpassword
  defaults write /Library/Preferences/com.apple.loginwindow autoLoginUser runner
fi
pw=""
auto=$(defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser 2>/dev/null || true)
[ "$auto" = runner ] && [ -f /etc/kcpassword ] || exit 1
case "$(dsmemberutil checkmembership -U runner -G admin)" in
  *"is a member"*) exit 1 ;;
esac

# --- d. Xcode, from the volume ------------------------------------------------------------
begin xcode
rm -rf /Applications/Xcode.app
case "$XCODE_KIND" in
  xip)
    work=$(mktemp -d /private/var/tmp/faber-xcode.XXXXXX)
    # xip -x only expands the archive; the codesign check below, after this case, is what
    # requires Apple's identity on the result.
    (cd "$work" && xip -x "$src/$XCODE_FILE")
    set -- "$work"/Xcode*.app
    [ $# -eq 1 ] && [ -d "$1" ] || exit 1
    mv "$1" /Applications/Xcode.app
    rm -rf "$work"
    ;;
  app)
    ditto "$src/$XCODE_FILE" /Applications/Xcode.app
    ;;
esac
# Both paths: xip accepts any validly signed archive, so require Apple's identity on the result.
# The requirement mirrors Xcode's designated requirement (Apple's Mac App Store application
# signing, team 59GAB85EFG); it was verified on Xcode 27.1. The deep verify takes about 15 s.
xcode_req='anchor apple generic and identifier "com.apple.dt.Xcode" and (certificate leaf[field.1.2.840.113635.100.6.1.9] or certificate leaf[subject.OU] = "59GAB85EFG")'
codesign --verify --deep --strict -R="$xcode_req" /Applications/Xcode.app || exit 1
xcode-select -s /Applications/Xcode.app/Contents/Developer
xcodebuild -license accept
xcodebuild -runFirstLaunch

# --- e. simulator runtimes ----------------------------------------------------------------
begin runtimes
for r in $RUNTIMES; do
  p=${r%%:*}
  case "$r" in
    *:*) xcodebuild -downloadPlatform "$p" -buildVersion "${r#*:}" ;;
    *) xcodebuild -downloadPlatform "$p" ;;
  esac
done

# --- f. Homebrew, owned by runner ---------------------------------------------------------
# Homebrew's installer refuses root and wants an admin, and runner is not one. So root makes
# the prefix, owned by runner, unpacks the pinned release tarball, and runs brew as runner.
begin homebrew
tarwork=$(mktemp -d /private/var/tmp/faber-brew.XXXXXX)
tarball=$tarwork/brew.tar.gz
curl -fsSL --proto =https --proto-redir =https --retry 3 -o "$tarball" \
  "https://github.com/Homebrew/brew/archive/refs/tags/$HOMEBREW_TAG.tar.gz"
got=$(shasum -a 256 "$tarball" | cut -d' ' -f1)
[ "$got" = "$HOMEBREW_SHA256" ] || exit 1
rm -rf "$brew_prefix"
mkdir -p "$brew_prefix"
tar -xzf "$tarball" -C "$brew_prefix" --strip-components=1
rm -rf "$tarwork"
chown -R runner:staff "$brew_prefix"
printf '%s\n' "$brew_prefix/bin" > /etc/paths.d/10-faber-homebrew
sudo -u runner -H env PATH="$brew_prefix/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
  HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1 \
  "$brew_prefix/bin/brew" install jq gh zstd git-lfs $BREW_EXTRA \
  >>/private/var/log/faber-golden-brew.log 2>&1

# --- g. the bootstrap, as it was installed by hand before ------------------------------------
begin bootstrap
install -d -o root -g wheel -m 0755 "$guest_dir"
install -o root -g wheel -m 0755 "$src/bootstrap.sh" "$guest_dir/bootstrap.sh"
install -o root -g wheel -m 0644 "$src/io.github.rejifald.faber.guest-agent.plist" \
  /Library/LaunchDaemons/io.github.rejifald.faber.guest-agent.plist
plutil -lint /Library/LaunchDaemons/io.github.rejifald.faber.guest-agent.plist

# --- h. no way in --------------------------------------------------------------------------
begin lock-admins
# Every admin the image's verification counts (faber-agent's admins_locked, ADR-0049 §7): the
# admin group's members by name (GroupMembership) and by GeneratedUID (GroupMembers), those of
# every group nested in it (NestedGroups), transitively, and every account whose primary group
# is admin's. A member or nested group the directory cannot resolve fails the build.

# attr_values RECORD ATTR: the attribute's values, one per line; nothing when it is absent.
attr_values() {
  dscl . -read "$1" "$2" 2>/dev/null | sed -n -e "s/^$2://p" -e '/^ /p' | tr ' ' '\n' | sed '/^$/d'
}
# name_of Users|Groups GUID: the record name whose GeneratedUID is GUID; nothing when none is.
name_of() {
  dscl . -list "/$1" GeneratedUID | awk -v g="$2" 'toupper($2) == toupper(g) { print $1; exit }'
}
# The directory must answer: a read that fails is no answer, never an empty admin set.
dscl . -read /Groups/admin RecordName >/dev/null || exit 1
dscl . -list /Users GeneratedUID >/dev/null || exit 1
set -f  # the names below are split unquoted; no pattern in them may expand
admins=""
add_admin() {
  case "$1" in *[!A-Za-z0-9._-]*|"") exit 1 ;; esac
  case " $admins " in *" $1 "*) ;; *) admins="$admins $1" ;; esac
}
todo=admin
seen_groups=""
while [ -n "$todo" ]; do
  set -- $todo
  g=$1
  shift
  todo=$*
  case "$g" in *[!A-Za-z0-9._-]*|"") exit 1 ;; esac
  case " $seen_groups " in *" $g "*) continue ;; esac
  seen_groups="$seen_groups $g"
  dscl . -read "/Groups/$g" RecordName >/dev/null || exit 1
  for u in $(attr_values "/Groups/$g" GroupMembership); do add_admin "$u"; done
  for guid in $(attr_values "/Groups/$g" GroupMembers); do
    u=$(name_of Users "$guid")
    [ -n "$u" ] || exit 1
    add_admin "$u"
  done
  for guid in $(attr_values "/Groups/$g" NestedGroups); do
    n=$(name_of Groups "$guid")
    [ -n "$n" ] || exit 1
    todo="$todo $n"
  done
done
for u in $(dscl . -list /Users PrimaryGroupID | awk '$2 == 80 { print $1 }'); do add_admin "$u"; done
set +f
# root is always in admin: a set without it means the directory was not read.
case " $admins " in *" root "*) ;; *) exit 1 ;; esac

for u in $admins; do
  [ "$u" != root ] || continue
  case "$u" in *[!A-Za-z0-9._-]*|"") exit 1 ;; esac
  # A SecureToken admin may refuse dscl -passwd; disable the account instead. Fail only if both do.
  if ! dscl . -passwd "/Users/$u" "$(openssl rand -base64 24)"; then
    pwpolicy -u "$u" -disableuser || exit 1
  fi
  dscl . -create "/Users/$u" UserShell /usr/bin/false
  dscl . -create "/Users/$u" IsHidden 1
done
# Root stays unusable: its AuthenticationAuthority is absent or ;DisabledUser;. (dsenableroot -d
# prompts without a tty, so the attribute is set directly.)
rootacct=root
root_path="/Users/$rootacct"
root_auth=$(dscl . -read "$root_path" AuthenticationAuthority 2>/dev/null || true)
case "$root_auth" in
  ""|*";DisabledUser;"*) ;;
  *)
    dscl . -create "$root_path" AuthenticationAuthority ";DisabledUser;" || true
    root_auth=$(dscl . -read "$root_path" AuthenticationAuthority 2>/dev/null || true)
    case "$root_auth" in *";DisabledUser;"*) ;; *) exit 1 ;; esac
    ;;
esac

begin checks
[ "$(fdesetup isactive 2>/dev/null || true)" != true ] || exit 1
if launchctl print system/com.openssh.sshd >/dev/null 2>&1; then
  launchctl disable system/com.openssh.sshd || true
  launchctl bootout system/com.openssh.sshd 2>/dev/null || true
  ! launchctl print system/com.openssh.sshd >/dev/null 2>&1 || exit 1
fi
for home in /var/root /Users/*; do
  for f in "$home/.ssh/authorized_keys" "$home/.ssh/authorized_keys2"; do
    [ ! -e "$f" ] || exit 1
  done
  accounts="$home/Library/Preferences/MobileMeAccounts.plist"
  if [ -f "$accounts" ] && plutil -extract Accounts.0 xml1 -o /dev/null "$accounts" 2>/dev/null; then
    exit 1
  fi
done
[ -z "$(ls -A /etc/sudoers.d 2>/dev/null)" ] || exit 1

# --- i. facts, then remove this provisioner and power off -------------------------------------
begin facts
say "fact macos_build=$(sw_vers -buildVersion)"
say "fact xcode_version=$(xcodebuild -version | sed -n '1s/^Xcode //p')"
say "fact xcode_build=$(xcodebuild -version | sed -n 's/^Build version //p')"

# The runtimes: each entry of `simctl runtime list -j`, read key by key with plutil.
rtdir=$(mktemp -d /private/var/tmp/faber-rt.XXXXXX)
xcrun simctl runtime list -j > "$rtdir/rt.json"
plutil -convert xml1 -o "$rtdir/rt.plist" "$rtdir/rt.json"
plutil -p "$rtdir/rt.plist" | sed -n 's/^  "\([0-9A-Fa-f-]\{36\}\)" => {$/\1/p' > "$rtdir/ids"
nfacts=0
while IFS= read -r id; do
  rb=$(plutil -extract "$id.build" raw -o - "$rtdir/rt.plist")
  rv=$(plutil -extract "$id.version" raw -o - "$rtdir/rt.plist")
  rp=$(plutil -extract "$id.platformIdentifier" raw -o - "$rtdir/rt.plist")
  case "${rp##*.}" in
    iphonesimulator) n=iOS ;; watchsimulator) n=watchOS ;; appletvsimulator) n=tvOS ;;
    xrsimulator) n=visionOS ;; *) n=${rp##*.} ;;
  esac
  say "fact runtime=$n $rv $rb"
  printf '%s %s %s\n' "$n" "$rv" "$rb" >> "$rtdir/seen"
done < "$rtdir/ids"
# Every requested runtime must be installed: its platform by name, and its version when given.
for r in $RUNTIMES; do
  p=${r%%:*}
  case "$r" in
    *:*) /usr/bin/grep -qxE "$p ${r#*:}(\\.[0-9]+)* .*" "$rtdir/seen" || exit 1 ;;
    *) /usr/bin/grep -q "^$p " "$rtdir/seen" || exit 1 ;;
  esac
done
rm -rf "$rtdir"

brewlist=$(mktemp /private/var/tmp/faber-brewlist.XXXXXX)
sudo -u runner -H env PATH="$brew_prefix/bin:/usr/bin:/bin:/usr/sbin:/sbin" HOMEBREW_NO_AUTO_UPDATE=1 \
  "$brew_prefix/bin/brew" list --formula --versions >"$brewlist" 2>>/private/var/log/faber-golden-brew.log \
  || exit 1
while IFS= read -r f; do
  set -- $f
  say "fact brew=$1 $2"
done < "$brewlist"
rm -f "$brewlist"

# The run-once daemon and wrapper are removed by finish(), on every exit path.
rm -f /private/var/log/faber-golden-provision.log /private/var/log/faber-golden-brew.log
done_ok=1
say "ok"
