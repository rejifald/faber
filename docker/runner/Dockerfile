# A runner image that satisfies faber's JIT contract.
#
# faber never installs or configures a runner itself. It starts this image with
# `RUNNER_JITCONFIG` in the environment and expects the entrypoint to do the
# rest — so the image, not faber, owns what a job can actually run. That is the
# seam: swap the toolchain here and faber does not change.
#
# This is a starting point, not a fixed recipe. It carries git, Node, and a
# Docker CLI pointed at the paired dind, which is enough for most JavaScript CI.
# Add what your jobs need (a JDK, Python, a browser set) and rebuild.
#
#   docker build -t faber-runner:local docker/runner
#
# Then point a profile at it — a local tag is fine, and is the recommended shape
# (nothing here needs to be pushed to a registry):
#
#   [[profile]]
#   name = "linux-docker"
#   labels = ["faber", "docker"]
#   backend = "docker"
#   image = "faber-runner:local"
#   dind = false   # true only for jobs that actually run `docker` — see ADR-0013
#
# Bump RUNNER_VERSION when GitHub deprecates the runner — it announces a minimum
# and then refuses older ones. Releases: https://github.com/actions/runner/releases
# faber warns per profile ahead of the cutoff; docs/operating.md, "When GitHub retires
# the runner version", has the steps.
ARG UBUNTU_VERSION=24.04
FROM ubuntu:${UBUNTU_VERSION}

ARG RUNNER_VERSION=2.335.1
ARG NODE_VERSION=22.20.0
ARG DOCKER_CLI_VERSION=27.5.1
# Set by buildx; defaulted so a plain `docker build` on arm64 still works.
ARG TARGETARCH=arm64

ENV DEBIAN_FRONTEND=noninteractive

# The things nearly every job shells out to. `setpriv` (util-linux) is what the
# entrypoint drops privileges with; `build-essential` is here because native
# addons and Rust builds need a C linker and the runner image has none by
# default. The runner's *own* prerequisites are deliberately not listed: they
# are libicu and friends, whose package names move with the Ubuntu release
# (libicu74 on 24.04, libicu70 on 22.04), so hardcoding them silently breaks the
# moment UBUNTU_VERSION changes. `installdependencies.sh` below ships with the
# runner and resolves them for whatever distro this is.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git gnupg jq unzip zip tar gzip xz-utils \
      build-essential \
      sudo util-linux \
 && rm -rf /var/lib/apt/lists/*

# The GitHub Actions runner, at /runner — the conventional install dir, and where
# faber's cache mount expects to find a sibling (`/runner/_cache`, ADR-0005).
RUN case "${TARGETARCH}" in \
      amd64) RUNNER_ARCH=x64 ;; \
      arm64) RUNNER_ARCH=arm64 ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && mkdir -p /runner \
 && curl -fsSL -o /tmp/runner.tar.gz \
      "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-${RUNNER_ARCH}-${RUNNER_VERSION}.tar.gz" \
 && tar -xzf /tmp/runner.tar.gz -C /runner \
 && rm /tmp/runner.tar.gz \
 && apt-get update \
 && /runner/bin/installdependencies.sh \
 && rm -rf /var/lib/apt/lists/*

# Node, from the official tarball rather than a distro package, so the version is
# exactly the one pinned above. faber's on-demand image variants (ADR-0008) shell
# out to `npx` in this image, so a base without Node cannot be varied.
RUN case "${TARGETARCH}" in \
      amd64) NODE_ARCH=x64 ;; \
      arm64) NODE_ARCH=arm64 ;; \
    esac \
 && curl -fsSL -o /tmp/node.tar.xz \
      "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
 && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
      --exclude CHANGELOG.md --exclude LICENSE --exclude README.md \
 && rm /tmp/node.tar.xz \
 && node --version && npm --version

# Just the Docker *client*. There is no daemon in here: for a profile with
# `dind = true`, faber pairs the runner with its own dind and sets
# DOCKER_HOST=tcp://dind:2375, so a job that runs `docker` talks to that sibling
# and nothing on the host is reachable. Under `dind = false` there is no daemon
# at all and no DOCKER_HOST, so this client fails immediately against a socket
# that isn't there — which is the point (ADR-0013). It stays in the image so one
# image serves both kinds of profile.
RUN case "${TARGETARCH}" in \
      amd64) DOCKER_ARCH=x86_64 ;; \
      arm64) DOCKER_ARCH=aarch64 ;; \
    esac \
 && curl -fsSL -o /tmp/docker.tgz \
      "https://download.docker.com/linux/static/stable/${DOCKER_ARCH}/docker-${DOCKER_CLI_VERSION}.tgz" \
 && tar -xzf /tmp/docker.tgz -C /tmp \
 && mv /tmp/docker/docker /usr/local/bin/docker \
 && rm -rf /tmp/docker /tmp/docker.tgz \
 && docker --version

# Jobs run unprivileged. uid 1001 is the actions-runner convention, and it is the
# uid the cache-volume ownership note in src/docker.rs is written against.
RUN useradd -m -u 1001 -s /bin/bash runner \
 && chown -R runner:runner /runner \
 && echo 'runner ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/runner \
 && chmod 0440 /etc/sudoers.d/runner

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh

WORKDIR /runner

# Root at start so the entrypoint can take ownership of a freshly-created cache
# mount; it drops to `runner` before the runner itself ever executes.
USER root
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
