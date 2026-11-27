# syntax=docker/dockerfile:1.7

# The two headless binaries as images, built from the repo root:
#
#   docker build -f images/headless/Dockerfile --target moon-bridge -t moon-bridge .
#   docker build -f images/headless/Dockerfile --target moon-remote -t moon-remote .
#
# `moon-bridge` is the relay plus the companion PWA on a slim base.
# `moon-remote` is the headless IDE on top of moon-base: agents run
# their commands in the same process tree, so it needs the dev
# toolchain around it.
#
# Specs: ../../specs/companion.md § "Headless images"
# ADR:   ../../specs/decisions/0129-published-images.md

# What `FROM` the moon-remote image builds on. An ARG so a local
# moon-base build can stand in for the published one.
ARG MOON_BASE=cr.hf.co/huggingface/moon-base:dev

FROM oven/bun:1.3.10 AS companion
WORKDIR /src
COPY package.json bun.lock ./
# `prepare` installs git hooks; there is no repository in here.
RUN bun install --frozen-lockfile --ignore-scripts
COPY . .
# The PWA logs this on startup; without a repository to ask, the
# commit comes from the build.
ARG MOON_BUILD_SHA=unknown
RUN MOON_BUILD_SHA="${MOON_BUILD_SHA}" bun run build:companion

# Same Debian release as both runtime images below, so the binaries
# link against a glibc that is there.
FROM rust:1.98.0-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        libdbus-1-dev \
        pkg-config \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p moon-bridge -p moon-remote \
 && mkdir /out \
 && cp target/release/moon-bridge target/release/moon-remote /out/ \
 && /out/moon-bridge --version \
 && /out/moon-remote --version

FROM debian:bookworm-slim AS moon-bridge
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        libdbus-1-3 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 1000 --create-home moon \
 && install -d -o moon -g moon /home/moon/.local/share/moon-ide
COPY --from=build /out/moon-bridge /usr/local/bin/moon-bridge
COPY --from=companion /src/companion/dist /usr/share/moon-bridge/companion
USER moon
# Everything the relay must keep across restarts — its TLS identity,
# enrollments and pairings — is under this one directory.
ENV MOON_SECRETS_DIR=/home/moon/.local/share/moon-ide/secrets
VOLUME /home/moon/.local/share/moon-ide
EXPOSE 53180
# The whole `serve` command is the entrypoint so a deployment only
# adds what is its own (`--advertise-url wss://…`) as arguments.
ENTRYPOINT ["moon-bridge", "serve", "--bind", "0.0.0.0:53180", "--no-idle-exit", "--web-root", "/usr/share/moon-bridge/companion"]

# moon-remote drives the host's container engine for project
# containers (a docker or podman socket in `DOCKER_HOST`), which
# moon-base doesn't ship a client for: the static docker CLI and the
# compose plugin. Pinned with checksums; bumps are intentional.
FROM debian:bookworm-slim AS docker-cli
ARG DOCKER_VERSION=29.10.1
ARG DOCKER_SHA256=061642059fa7f2ed9b2f04a327a951cff4249585cf04b2661c2a6539197c9962
ARG COMPOSE_VERSION=5.8.0
ARG COMPOSE_SHA256=b8840f5008c447f41fce0a57ae3adeab42c89b1eb6e6012524bcabbfb4ffbaf6
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://download.docker.com/linux/static/stable/x86_64/docker-${DOCKER_VERSION}.tgz" -o /tmp/docker.tgz \
 && echo "${DOCKER_SHA256}  /tmp/docker.tgz" | sha256sum -c - \
 && tar -xzf /tmp/docker.tgz -C /tmp docker/docker \
 && install -D -m 0755 /tmp/docker/docker /out/bin/docker \
 && curl -fsSL "https://github.com/docker/compose/releases/download/v${COMPOSE_VERSION}/docker-compose-linux-x86_64" -o /tmp/compose \
 && echo "${COMPOSE_SHA256}  /tmp/compose" | sha256sum -c - \
 && install -D -m 0755 /tmp/compose /out/cli-plugins/docker-compose

FROM ${MOON_BASE} AS moon-remote
COPY --from=docker-cli /out/bin/docker /usr/local/bin/docker
COPY --from=docker-cli /out/cli-plugins/docker-compose /usr/local/lib/docker/cli-plugins/docker-compose
COPY --from=build /out/moon-remote /usr/local/bin/moon-remote
ENV MOON_SECRETS_DIR=/home/dev/.local/share/moon-ide/secrets
ENTRYPOINT ["moon-remote"]
CMD ["--help"]
