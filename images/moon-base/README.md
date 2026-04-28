# moon-base

The base image for moon-ide workspace containers. Every team
that opts into Phase 2's containerised dev shells builds
`FROM huggingface/moon-base:<tag>` to inherit a working dev
environment without relitigating Tauri build deps or
toolchain installs.

The container runs **unprivileged** with the default Docker
capability set. Project services (postgres, redis, etc.)
come up as siblings on the host's daemon via compose
`include:` rather than nested inside the workspace — see
ADR 0008 for the threat-model reasoning.

Architectural spec:
[`specs/containers.md`](../../specs/containers.md).
Decisions:
[ADR 0007 — compose + moon-base](../../specs/decisions/0007-compose-and-moon-base.md),
[ADR 0008 — host-shared daemon](../../specs/decisions/0008-host-shared-daemon.md).

## Status

- **In tree** — iterating on the recipe.
- **Not yet published** — no Docker Hub tags exist yet. Local
  `docker build` only.
- **Unprivileged.** `moon-base` does not embed Docker-in-Docker.
  An earlier commit landed the canonical DinD recipe; we
  reverted it after walking the supply-chain threat model
  through (ADR 0008). The image is smaller and the workspace
  container runs with normal Docker capabilities.

## What's in the image (today)

- **Debian bookworm-slim** as the base.
- **Tauri build deps**: WebKitGTK 4.1 + libsoup 3 + GTK 3 +
  ayatana-appindicator + librsvg + OpenSSL + pkg-config.
  Required because the moon-ide Linux build chain links against
  these; see ADR 0005.
- **`rustup` (stable, minimal profile)** with `clippy` and
  `rustfmt`.
- **`bun`**.
- **Standard plumbing**: `git`, `curl`, `wget`, `ca-certificates`,
  `build-essential`, `less`, `sudo`, `unzip`, `xz-utils`.
- **Non-root `dev` user** (uid 1000, gid 1000) with passwordless
  sudo. The uid lines up with the conventional first user on
  Debian/Ubuntu hosts; Docker Desktop for macOS handles the uid
  translation transparently.

What is **not** here, deliberately:

- No Docker daemon, no Docker CLI. Generated `compose.yaml`
  uses the host's daemon, so the workspace container never
  needs to talk to one. If a project genuinely wants a Docker
  CLI inside the workspace later, it's a forwarded socket +
  `apt-get install docker-ce-cli` line in that project's own
  Dockerfile-on-top, not a base-image concern.

## What's coming (later commits in Phase 2.0)

1. **Polyglot CLIs.** `uv` (Python), `hf` (Hugging Face Hub),
   `gh` (GitHub), plus comfort tooling (`ripgrep`, `fzf`,
   `bat`, `jq`).
2. **GitHub Actions workflow.** Multi-arch native builds
   (`linux/amd64` + `linux/arm64`) with
   `docker buildx imagetools create` to publish a combined
   manifest to `huggingface/moon-base` on Docker Hub.
   Matrix-runs on `ubuntu-24.04` + `ubuntu-24.04-arm` (both
   free for public repos, so no QEMU emulation tax).
3. **Smoke test.** `git clone moon-ide && bun install && cargo check`
   inside the freshly built image — green is the release gate
   per ADR 0005.

Tracker: [`specs/roadmaps/phase-02-containers.md`](../../specs/roadmaps/phase-02-containers.md).

## Build locally

```bash
docker build -t moon-base:dev images/moon-base/
```

(Run from the repo root.)

Verify the toolchain landed:

```bash
docker run --rm moon-base:dev bash -c '
  rustc --version
  cargo --version
  bun --version
  pkg-config --modversion webkit2gtk-4.1
'
```

First-build size lands around 2.5–3 GB (WebKitGTK + GTK 3 dev
libs are ~1.4 GB on their own; rustup with the stable toolchain
pulls another ~1 GB). We'll look at slimming if it gets
unwieldy, but a multi-GB workspace base image is normal for the
"polyglot toolchain" tradeoff we picked in ADR 0007.

## Versioning

| Tag               | Meaning                                                                                       |
| ----------------- | --------------------------------------------------------------------------------------------- |
| `<commit-sha>`    | Reproducible reference — moon-ide pins generated compose to this in production.               |
| `<major>.<minor>` | Human-friendly (e.g. `0.1`, `0.2`); moves to the latest matching commit on each release.      |
| `latest`          | Casual use only — generated `compose.yaml` never points here.                                 |
| `dev`             | Pre-release / WIP builds — used by CI for smoke-tests before promoting to a real version tag. |

The first stable tag will be `0.1.0`, released once the
multi-arch CI workflow and the `bun install && cargo check`
smoke test land. Pre-`0.1.0` everything is via `:dev` or local
`:dev` builds.

## Extending it

Teams add their own tooling on top with a thin `Dockerfile`
that lives next to their workspace's `compose.yaml`:

```dockerfile
FROM huggingface/moon-base:0.1

RUN sudo apt-get update \
 && sudo apt-get install -y --no-install-recommends awscli redis-tools \
 && sudo rm -rf /var/lib/apt/lists/*
```

The base image leaves the user as `dev`, so commands run
without explicit `USER` switches.
