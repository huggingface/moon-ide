# syntax=docker/dockerfile:1.7

# moon-base — the workspace dev image for moon-ide.
#
# What lives here today: WebKitGTK dev libraries (so a fresh
# moon-ide checkout is buildable per ADR 0005), rustup with
# the stable toolchain, bun, git, and a non-root `dev` user
# with passwordless sudo. The container runs unprivileged with
# the default Docker capability set; project services come up
# as siblings on the host's daemon via compose `include:`
# rather than nested inside this container (ADR 0008). Polyglot
# extras (uv / hf / gh CLI) and the multi-arch CI workflow
# arrive in subsequent commits.
#
# Specs: ../../specs/containers.md
# ADRs:  ../../specs/decisions/0007-compose-and-moon-base.md
#        ../../specs/decisions/0008-host-shared-daemon.md

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

# Non-root user. UID 1000 lines up with the conventional "first user"
# on Linux contributors' hosts; on Docker Desktop for macOS the
# host->container user mapping fronts whatever the Mac contributor's
# uid actually is.
RUN groupadd --gid "${GID}" "${USERNAME}" \
 && useradd --uid "${UID}" --gid "${GID}" --create-home --shell /bin/bash "${USERNAME}" \
 && echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/${USERNAME}" \
 && chmod 0440 "/etc/sudoers.d/${USERNAME}"

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

# moon-ide attaches with `docker exec` for everything (terminals, LSPs,
# builds). The container itself just sits and waits — no daemonised
# language servers, no init scripts kicking off background work, no
# in-container Docker daemon (project services run as siblings on the
# host's daemon via compose `include:` — see ADR 0008).
CMD ["sleep", "infinity"]
