# The faber-mirror service image (ADR-0047).
#
# Built from this checkout into the Docker engine the runners use, and never
# pushed anywhere: nothing faber builds is published (AGENTS.md). Use the script,
# which tags it with the faber version the daemon looks for:
#
#   deploy/build-mirror-image.sh
#
# The build context is the repository root, because the crate is a workspace
# member and builds against the workspace's Cargo.lock. Its own ignore file,
# Dockerfile.dockerignore beside this one, keeps target/ and the UI's node
# modules out of the context.
#
# The two stages share one Debian release, so the binary's glibc is the one the
# runtime image has.
ARG DEBIAN_RELEASE=trixie

FROM rust:1-${DEBIAN_RELEASE} AS build
ARG FABER_VERSION
WORKDIR /src
COPY . .
RUN FABER_VERSION="${FABER_VERSION}" \
    cargo build --locked --release -p faber-mirror --bin faber-mirror \
 && install -m 0755 target/release/faber-mirror /faber-mirror

FROM debian:${DEBIAN_RELEASE}-slim
ARG FABER_VERSION
LABEL org.opencontainers.image.title="faber-mirror" \
      org.opencontainers.image.version="${FABER_VERSION}" \
      org.opencontainers.image.source="https://github.com/rejifald/faber"
COPY --from=build /faber-mirror /usr/local/bin/faber-mirror
# The LFS adapter fetches from GitHub over TLS, verified against the system's
# trust store, which the slim image does not ship.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
# The cache is a named volume mounted at /cache. A new named volume takes its
# ownership from the image's directory, so the service's unprivileged user owns
# it from the first start.
RUN mkdir /cache && chown 65532:65532 /cache
USER 65532:65532
EXPOSE 7470
ENTRYPOINT ["/usr/local/bin/faber-mirror"]
CMD ["serve"]
