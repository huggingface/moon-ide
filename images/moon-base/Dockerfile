# syntax=docker/dockerfile:1.7

# moon-base — the workspace dev image for moon-ide.
#
# What lives here today: WebKitGTK dev libraries (so a fresh
# moon-ide checkout is buildable per ADR 0005), rustup with
# the stable toolchain, bun, git, the canonical
# Docker-in-Docker recipe (fuse-overlayfs + iptables-legacy +
# the Docker 29+ snapshotter flag), and a non-root `dev` user
# with passwordless sudo and `docker` group membership. The
# polyglot extras (uv / hf / gh CLI) and the multi-arch CI
# workflow arrive in subsequent commits.
#
# Spec: ../../specs/containers.md
# ADR:  ../../specs/decisions/0007-compose-and-moon-base.md

FROM debian:bookworm-slim

ARG USERNAME=dev
ARG UID=1000
ARG GID=1000

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# Two install passes so the image stays human-auditable: tauri/webkit
# build deps first, then general dev plumbing. Keep the layers small
# by always cleaning up apt lists in the same RUN.

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        libwebkit2gtk-4.1-dev \
        libsoup-3.0-dev \
        libgtk-3-dev \
        libayatana-appindicator3-dev \
        librsvg2-dev \
        libssl-dev \
        pkg-config \
 && rm -rf /var/lib/apt/lists/*

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        wget \
        git \
        less \
        sudo \
        unzip \
        xz-utils \
 && rm -rf /var/lib/apt/lists/*

# Docker-in-Docker. Pulls Docker's official apt repo so we get a
# version that's actually maintained, then layers in fuse-overlayfs
# (the storage driver that nests cleanly under another overlay) and
# the iptables packages (we'll pin to legacy below — Docker's bridge
# driver doesn't speak nftables).
RUN install -m 0755 -d /etc/apt/keyrings \
 && curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc \
 && chmod a+r /etc/apt/keyrings/docker.asc \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
      > /etc/apt/sources.list.d/docker.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
        docker-ce \
        docker-ce-cli \
        containerd.io \
        docker-buildx-plugin \
        docker-compose-plugin \
        fuse-overlayfs \
        iptables \
 && rm -rf /var/lib/apt/lists/*

# Pin iptables alternatives to legacy at build time. Idempotent and
# matches the canonical DinD recipe; Debian 12 defaults to nftables.
RUN update-alternatives --set iptables /usr/sbin/iptables-legacy \
 && update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy

# dockerd configuration:
# - fuse-overlayfs handles nested-overlay correctly inside a
#   privileged container.
# - features.containerd-snapshotter=false avoids the new image store
#   that Docker 29+ enables by default; it doesn't play well with
#   nested DinD.
COPY daemon.json /etc/docker/daemon.json

# Non-root user. UID 1000 lines up with the conventional "first user"
# on Linux contributors' hosts; on Docker Desktop for macOS the
# host->container user mapping fronts whatever the Mac contributor's
# uid actually is.
#
# `docker` group membership lets `dev` use the in-container daemon
# without sudo (the daemon-side socket at /var/run/docker.sock is
# mode 660, root:docker).
RUN groupadd --gid "${GID}" "${USERNAME}" \
 && useradd --uid "${UID}" --gid "${GID}" --create-home --shell /bin/bash "${USERNAME}" \
 && echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/${USERNAME}" \
 && chmod 0440 "/etc/sudoers.d/${USERNAME}" \
 && usermod -aG docker "${USERNAME}"

USER ${USERNAME}
WORKDIR /home/${USERNAME}

# rustup (stable, minimal profile). PATH is set in the env so it works
# both from `docker exec` and from interactive shells.
ENV RUSTUP_HOME=/home/${USERNAME}/.rustup \
    CARGO_HOME=/home/${USERNAME}/.cargo \
    PATH=/home/${USERNAME}/.cargo/bin:${PATH}

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path \
 && rustup component add clippy rustfmt

# bun
ENV BUN_INSTALL=/home/${USERNAME}/.bun \
    PATH=/home/${USERNAME}/.bun/bin:${PATH}

RUN curl -fsSL https://bun.sh/install | bash

WORKDIR /workspace

# Entrypoint backgrounds dockerd (so the user's `docker compose up`
# inside their project works), waits for the socket, then runs CMD
# under the dev user. The host needs `privileged: true` on the
# compose service for dockerd to start; without it the container
# still works for non-Docker workloads.
COPY --chown=root:root --chmod=0755 entrypoint.sh /usr/local/bin/moon-base-entrypoint
ENTRYPOINT ["/usr/local/bin/moon-base-entrypoint"]

# moon-ide attaches with `docker exec` for everything (terminals, LSPs,
# builds). The container itself just sits and waits — no daemonised
# language servers, no init scripts kicking off background work.
CMD ["sleep", "infinity"]
