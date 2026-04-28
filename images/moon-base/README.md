# moon-base

The base image for moon-ide workspace containers. Every team
that opts into Phase 2's containerised dev shells builds
`FROM huggingface/moon-base:<tag>` to inherit a working dev
environment without relitigating Tauri build deps, the
Docker-in-Docker recipe, or toolchain installs.

Architectural spec:
[`specs/containers.md`](../../specs/containers.md). Format /
registry / image-strategy decision:
[ADR 0007](../../specs/decisions/0007-compose-and-moon-base.md).

## Status

- **In tree** — first scaffold, iterating on the recipe.
- **Not yet published** — no Docker Hub tags exist yet. Local
  `docker build` only.
- **DinD pending** — Docker-in-Docker isn't wired in yet. The
  current image is rust + bun + WebKitGTK dev libs and nothing
  more.

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
  Debian/Ubuntu hosts; Docker Desktop for macOS handles the
  uid translation transparently.

## What's coming (later commits in Phase 2.0)

1. **Docker-in-Docker.** The canonical recipe — `fuse-overlayfs`
   storage driver, `iptables-legacy` alternative, the Docker
   29+ `containerd-snapshotter: false` flag — so users' own
   `docker compose up` works inside the container. See
   `specs/containers.md#the-moon-base-image`.
2. **Polyglot CLIs.** `uv` (Python), `hf` (Hugging Face Hub),
   `gh` (GitHub), plus comfort tooling (`ripgrep`, `fzf`,
   `bat`, `jq`).
3. **GitHub Actions workflow.** Multi-arch native builds
   (`linux/amd64` + `linux/arm64`) with `docker buildx
imagetools create` to publish a combined manifest to
   `huggingface/moon-base` on Docker Hub. Matrix-runs on
   `ubuntu-24.04` + `ubuntu-24.04-arm` (both free for public
   repos, so no QEMU emulation tax).
4. **Smoke test.** `git clone moon-ide && bun install &&
cargo check` inside the freshly built image — green is the
   release gate per ADR 0005.

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

First-build size lands around 2.5–3 GB (WebKitGTK + GTK 3
dev libs are ~1.4 GB on their own; rustup with the stable
toolchain pulls another ~1 GB). DinD adds a small amount in
the next commit; we'll look at slimming if it gets unwieldy.

## Versioning

| Tag               | Meaning                                                                                       |
| ----------------- | --------------------------------------------------------------------------------------------- |
| `<commit-sha>`    | Reproducible reference — moon-ide pins generated compose to this in production.               |
| `<major>.<minor>` | Human-friendly (e.g. `0.1`, `0.2`); moves to the latest matching commit on each release.      |
| `latest`          | Casual use only — generated `compose.yaml` never points here.                                 |
| `dev`             | Pre-release / WIP builds — used by CI for smoke-tests before promoting to a real version tag. |

The first stable tag will be `0.1.0` (released once DinD + the
smoke test land). Pre-`0.1.0` everything is via `:dev` or local
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
