# syntax=docker/dockerfile:1.7

# moon-base — the workspace dev image for moon-ide.
#
# What lives here today: WebKitGTK dev libraries (so a fresh
# moon-ide checkout is buildable per ADR 0005), rustup with the
# stable toolchain, bun, the polyglot CLIs we use across most
# repos in the org (gh, uv, hf) plus comfort tooling (ripgrep,
# fzf, bat, jq), and a non-root `dev` user with passwordless
# sudo. The container runs unprivileged with the default Docker
# capability set; project services come up as siblings on the
# host's daemon via compose `include:` rather than nested inside
# this container (ADR 0008). The multi-arch CI workflow lands
# in a subsequent commit.
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
        openssh-client \
        sudo \
        unzip \
        xz-utils \
 && rm -rf /var/lib/apt/lists/*

# Pre-seed system-wide known_hosts with the SSH host keys for the
# git providers we actually use, so the first `git fetch` /
# `git clone` in a container terminal doesn't prompt
# `Are you sure you want to continue connecting?` (or worse, fail
# under non-interactive `docker exec`). The keys are fetched
# fresh from each provider at image build time; if they rotate
# between rebuilds the user falls back to the same prompt-based
# accept flow they'd see on a clean host.
#
# moon-ide forwards the host's SSH agent into the dev container
# (mount + `SSH_AUTH_SOCK` injected by the generated compose
# file) so private keys never leave the host. See
# specs/containers.md § "SSH agent forwarding".
RUN install -m 0755 -d /etc/ssh \
 && ssh-keyscan -t rsa,ecdsa,ed25519 github.com gitlab.com \
      >> /etc/ssh/ssh_known_hosts \
 && chmod 0644 /etc/ssh/ssh_known_hosts

# Comfort tooling that's stable enough to live in the base image:
# ripgrep / fzf / bat / jq. `bat` ships as `batcat` on Debian to
# avoid a name clash with `bacula-tools`; symlink it back so
# muscle memory works.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ripgrep \
        fzf \
        bat \
        jq \
 && rm -rf /var/lib/apt/lists/* \
 && ln -s /usr/bin/batcat /usr/local/bin/bat

# GitHub CLI from its official apt repo (Debian's `gh` is usually
# behind upstream by a release or two, and `gh` cuts releases
# frequently enough that the lag matters for newer commands).
RUN install -m 0755 -d /etc/apt/keyrings \
 && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
      -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && chmod a+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
 && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
      > /etc/apt/sources.list.d/github-cli.list \
 && apt-get update \
 && apt-get install -y --no-install-recommends gh \
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

# `rust-analyzer` ships with the LSP broker's in-container fallback
# (moon-core::lsp::spawn::LspSpawner::DockerExec). We install it here
# rather than lazily so a user's first `.rs` file open doesn't pay a
# download cost — the image build already ate that bandwidth and we
# can spawn the server as soon as the container is up.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path \
 && rustup component add clippy rustfmt rust-analyzer

# bun
ENV BUN_INSTALL=/home/${USERNAME}/.bun \
    PATH=/home/${USERNAME}/.bun/bin:${PATH}

RUN curl -fsSL https://bun.sh/install | bash

# fnm + Node — fnm reads `.nvmrc` and `.node-version`, so projects
# that pin a specific Node (moon-landing pins 24.14.1, for
# instance) pick up automatically when the user `cd`s into them.
# Single Rust binary, shell-agnostic, faster than the bash-based
# nvm and saner inside non-interactive `docker exec`. The image
# ships the current LTS as the default Node.
#
# Corepack (bundled with Node 16+) is enabled here, with `pnpm`
# pre-staged via `corepack prepare`. Each project's actual
# pnpm/yarn version resolves on demand from its `packageManager`
# field in package.json, so we don't bake a pnpm version that'll
# drift.
ENV FNM_DIR=/home/${USERNAME}/.fnm \
    PATH=/home/${USERNAME}/.fnm/aliases/default/bin:/home/${USERNAME}/.fnm:${PATH}

RUN curl -fsSL https://fnm.vercel.app/install \
      | bash -s -- --install-dir "${FNM_DIR}" --skip-shell \
 && fnm install --lts \
 && fnm default lts-latest

# After `fnm default`, the alias symlink chain
# `aliases/default -> aliases/lts-latest -> .../v<x.y.z>/installation`
# is in place, and the ENV PATH set above keeps Node + npm + corepack
# resolved for every subsequent `docker exec`. Enable corepack so the
# `pnpm` / `yarn` shims are on PATH; the actual pnpm/yarn version
# downloads on first use from each project's `packageManager` field.
RUN corepack enable \
 && node --version \
 && npm --version \
 && corepack --version

# Auto-switch + auto-install Node when the user `cd`s into a
# directory with a .nvmrc / .node-version. fnm's bundled hook
# calls `fnm use --silent-if-unchanged` (no auto-install); we
# override the hook function to also `--install-if-missing` so a
# `cd ~/code/moon-landing` after the team bumps `.nvmrc` Just
# Works rather than printing a "version not installed" error.
#
# Sourced for interactive shells only; `docker exec` non-interactive
# calls get the default Node from $PATH.
RUN { \
        echo 'eval "$(fnm env --use-on-cd --version-file-strategy=recursive --shell bash)"'; \
        echo '__fnm_use_if_file_found() { fnm use --install-if-missing --silent-if-unchanged; }'; \
    } >> /home/${USERNAME}/.bashrc

# uv (Astral's Python toolchain manager). Standalone installer
# detects the right arch automatically and lands at
# ~/.local/bin/uv. We pin to a known version so the image is
# reproducible; bumps are intentional.
ENV UV_VERSION=0.5.18 \
    PATH=/home/${USERNAME}/.local/bin:${PATH}

RUN curl -LsSf "https://astral.sh/uv/${UV_VERSION}/install.sh" | sh \
 && uv --version

# hf (Hugging Face Hub CLI). Installed as a uv-managed tool so
# its dependency tree is isolated from any project venvs the
# user later creates. uv fetches the pinned Python interpreter
# itself; we don't want to depend on whatever python the base
# image happens to have.
RUN uv python install 3.12 \
 && uv tool install --python 3.12 huggingface_hub \
 && hf --version

WORKDIR /workspace

# moon-ide attaches with `docker exec` for everything (terminals, LSPs,
# builds). The container itself just sits and waits — no daemonised
# language servers, no init scripts kicking off background work, no
# in-container Docker daemon (project services run as siblings on the
# host's daemon via compose `include:` — see ADR 0008).
CMD ["sleep", "infinity"]
