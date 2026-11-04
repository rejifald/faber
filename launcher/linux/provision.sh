#!/bin/bash
# faber's Linux VM image, provisioned once in the throwaway build VM that
# `sudo faber setup` boots (ADR-0026, "The Linux VM runs Docker Engine under
# the same launcher"). cloud-init's runcmd runs it; it powers the VM off.
#
# Its last word is one line on the console, which setup reads from the
# launcher's serial log: "ok" with Docker's version, or "failed" with the step
# that failed. The tag is assembled at run time, so this file's own text never
# reads as a result line.
set -eu
set -o pipefail
export DEBIAN_FRONTEND=noninteractive
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

tag="FABER-LINUX-IMAGE"
share=/usr/local/share/faber
step=0
done_ok=0

say() {
    # The launcher's serial port is a virtio console: hvc0 in the guest.
    # The newline first: getty's login prompt shares hvc0 and ends with no
    # newline, and root reads a result only at the start of a line.
    if [ -c /dev/hvc0 ]; then
        printf '\n%s\n' "$1" >/dev/hvc0 || true
    else
        printf '\n%s\n' "$1" >/dev/console || true
    fi
}

finish() {
    if [ "$done_ok" != 1 ] && [ "$reported" != 1 ]; then
        say "$tag failed step=$step"
    fi
    sync
    systemctl poweroff --no-block || poweroff -f
}
reported=0
trap finish EXIT

# 0. The wall is up before anything is fetched (#236). The anchor's rules for
#    this network bind only once its bridge exists, after the VM started. Their
#    first rule for this guest drops everything to the host, the gateway
#    included, and the anchor loads as a unit. So DNS to the gateway going
#    unanswered, several times in a row, shows the whole rule set is bound.
#    An answer, or a refusal (the host's port is reachable), means not yet.
#    No wall by the deadline: report it and power off, having fetched nothing.
wall_gate() {
    python3 -I - "$1" <<'PY'
import socket
import sys
import time

gateway = sys.argv[1]
# A query for the root zone's NS records, id 0xfabe, recursion desired.
query = bytes.fromhex("fabe010000010000000000000000020001")
deadline = time.monotonic() + 120
misses = 0
while time.monotonic() < deadline:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(2)
    try:
        s.connect((gateway, 53))
        s.send(query)
        s.recv(512)
        misses = 0
    except socket.timeout:
        misses += 1
        if misses >= 3:
            sys.exit(0)
        continue
    except OSError:
        # Refused, or no address or route yet.
        misses = 0
    finally:
        s.close()
    time.sleep(1)
sys.exit(1)
PY
}
step=0
if ! wall_gate 192.168.239.1; then
    say "$tag wall-not-up"
    reported=1
    exit 1
fi

# 1. Docker's apt source, signed by the key faber embeds.
step=1
install -d -m 0755 /etc/apt/keyrings
install -m 0644 "$share/docker.asc" /etc/apt/keyrings/docker.asc
arch="$(dpkg --print-architecture)"
cat >/etc/apt/sources.list.d/docker.sources <<SOURCES
Types: deb
URIs: https://download.docker.com/linux/debian
Suites: trixie
Components: stable
Architectures: $arch
Signed-By: /etc/apt/keyrings/docker.asc
SOURCES
chmod 0644 /etc/apt/sources.list.d/docker.sources

# 2. Docker Engine, and the trust store faber's TLS uses (#170). Its
#    daemon.json goes first, so containers use Cloudflare's resolvers from
#    the first start too (ADR-0026, "DNS may leak names").
step=2
install -d -m 0755 /etc/docker
install -m 0644 "$share/daemon.json" /etc/docker/daemon.json
apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates docker-ce docker-ce-cli containerd.io docker-buildx-plugin

# 3. Docker on its Unix socket, and faber's relay and wall probe on vsock.
step=3
systemctl enable docker.socket faber-docker-vsock.socket faber-wall-probe.socket
systemctl start docker.socket docker.service

# 4. Resolvers that survive without cloud-init: Cloudflare's, never the
#    gateway's (ADR-0026, "DNS may leak names").
step=4
install -d -m 0755 /etc/systemd/network
install -m 0644 "$share/10-faber.network" /etc/systemd/network/10-faber.network
rm -f /etc/netplan/*.yaml /etc/systemd/network/50-cloud-init.network \
    /etc/network/interfaces.d/50-cloud-init /etc/network/interfaces.d/50-cloud-init.cfg
install -d -m 0755 /etc/cloud/cloud.cfg.d
printf 'network: {config: disabled}\n' >/etc/cloud/cloud.cfg.d/99-faber-no-network.cfg
systemctl enable systemd-networkd
if [ -e /usr/lib/systemd/system/systemd-resolved.service ] \
    || [ -e /lib/systemd/system/systemd-resolved.service ]; then
    install -d -m 0755 /etc/systemd/resolved.conf.d
    install -m 0644 "$share/faber-resolved.conf" /etc/systemd/resolved.conf.d/faber.conf
    systemctl enable systemd-resolved
else
    rm -f /etc/resolv.conf
    printf 'nameserver 1.1.1.1\nnameserver 1.0.0.1\n' >/etc/resolv.conf
    chmod 0644 /etc/resolv.conf
fi

# 5. No way in.
step=5
if dpkg-query -W -f='${Status}' openssh-server 2>/dev/null | grep -q 'install ok installed'; then
    apt-get purge -y openssh-server
fi
passwd -l root

# 6. Docker listens on no network port.
step=6
if ss -Hltunp | grep -E '"(dockerd|containerd)"'; then
    exit 1
fi

# 7. Docker answers on its socket.
step=7
docker_version="$(docker -H unix:///run/docker.sock version --format '{{.Server.Version}}')"
case "$docker_version" in
    '' | *[!0-9A-Za-z.+~-]*) exit 1 ;;
esac

# 8. Clean, and boot the long-lived VM without cloud-init or a seed.
step=8
apt-get clean
rm -rf /var/lib/apt/lists/*
cloud-init clean --logs --machine-id || truncate -s 0 /etc/machine-id
touch /etc/cloud/cloud-init.disabled
rm -rf "$share" /usr/local/libexec/faber-provision

# 9. Report, and power off (the EXIT trap).
step=9
done_ok=1
say "$tag ok docker=$docker_version"
