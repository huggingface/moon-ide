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

## Build locally

```bash
docker build -t moon-base:dev images/moon-base/
```

(Run from the repo root.)

First-build size lands around 3.3 GB (WebKitGTK + GTK 3 dev
libs are ~1.4 GB on their own; rustup with the stable
toolchain pulls another ~1 GB; uv-managed Python 3.12 + the
hf CLI's dep tree adds ~250 MB; Node LTS + corepack adds ~250 MB;
gh + comfort tooling adds ~50 MB). We'll look at slimming if it
gets unwieldy, but a multi-GB workspace base image is normal for
the "polyglot toolchain" tradeoff we picked in ADR 0007.

## Status

- **In tree** — iterating on the recipe.
- **CI wired but unpublished.** Multi-arch builds run on PRs
  (no push) and on push to `main`
  (push to `huggingface/moon-base:dev` + `:sha-<long>`). No
  versioned tag exists yet; `0.1.0` will be cut once Phase 2.0
  ships end-to-end.
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
- **`fnm` + Node LTS + Corepack**. fnm reads `.nvmrc` /
  `.node-version`, so projects that pin a specific Node
  (moon-landing pins `24.14.1`, for example) auto-switch on
  `cd` — and auto-install the missing version too, so the
  first `cd` after a team-wide bump Just Works rather than
  printing "version not installed". Corepack is enabled so
  the `pnpm` / `yarn` shims are on PATH; the actual version
  resolves from each project's `packageManager` field on
  first use, so nothing in this image drifts vs. what teams
  pin in their repos.
- **`uv`** (pinned to a specific version for reproducibility),
  managing Python toolchains and tool installs.
- **`hf`** (Hugging Face Hub CLI), installed via `uv tool` so
  its dependency tree stays isolated from any project venv.
- **`gh`** from GitHub's official apt repo (Debian's `gh`
  trails upstream by a release or two).
- **`mongosh`** (the MongoDB shell) and the **MongoDB
  database tools** (`mongodump`, `mongorestore`,
  `mongoexport`, `mongoimport`, `bsondump`, `mongostat`,
  `mongotop`, `mongofiles`) from MongoDB's official apt
  repo, pinned to the 8.0 channel. Server is **not**
  bundled — projects that need MongoDB run it as a host-
  daemon sibling via compose `include:` (ADR 0008); the
  shell and the dump/restore tools are the pieces the dev
  needs at the interactive prompt (ad-hoc queries,
  snapshotting a dev mongo, restoring a fixture into it).
  mongosh 2.x speaks every server protocol from 4.4 onwards.
- **`helm`** (pinned, user-mode) for the Helm-chart-heavy infra
  and workloads repos. No `kubectl` or Kubernetes daemon baked
  in — extend with `FROM moon-base` if a team needs them.
- **Comfort tooling**: `ripgrep` (`rg`), `fzf`, `bat` (the
  Debian `batcat` symlinked back to `bat`), `jq`.
- **Standard plumbing**: `git`, `curl`, `wget`, `ca-certificates`,
  `build-essential`, `less`, `sudo`, `unzip`, `xz-utils`,
  `openssh-client` (so `git` over SSH works inside the container
  when moon-ide forwards the host's agent — see
  [`specs/containers.md` § SSH agent forwarding](../../specs/containers.md#ssh-agent-forwarding)).
- **Pre-seeded `/etc/ssh/ssh_known_hosts`** for `github.com` and
  `gitlab.com`, populated via `ssh-keyscan` at image build time.
  Lets the first `git fetch` / `git clone` over SSH succeed
  without an interactive prompt — important because non-interactive
  `docker exec` invocations would otherwise fail the host-key
  check. If a provider rotates keys between rebuilds, the
  prompt-based flow takes over until the next image rebuild.
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

## CI / publishing

The image rebuilds on changes to `images/moon-base/**` (or to
the workflow itself), triggered by pushes to `main`, pull
requests touching the same paths, or manual dispatch. PR builds
run the smoke test but do not push — they're a safety net for
recipe changes. Pushes to `main` go further: they push each
arch's image by digest to Docker Hub and assemble a multi-arch
manifest under `:dev` (rolling) and `:sha-<commit>` (immutable).

The workflow ([.github/workflows/moon-base.yml](../../.github/workflows/moon-base.yml))
runs natively on `ubuntu-24.04` (amd64) and `ubuntu-24.04-arm`
(arm64) — both are free for public repos, so we skip QEMU
emulation entirely. The release gate from ADR 0005 lives inside
each per-arch job: `bun install --frozen-lockfile` and
`cargo check --workspace --locked` against a moon-ide checkout
mounted into the freshly built image. A green merge to `main`
means, by construction, the published image builds moon-ide.

### Required secrets

| Secret               | What                                                                     |
| -------------------- | ------------------------------------------------------------------------ |
| `DOCKERHUB_USERNAME` | Org/bot Docker Hub account with write access to `huggingface/moon-base`. |
| `DOCKERHUB_PASSWORD` | Access token / password for that account.                                |

Names match the existing `huggingface/Mongoku` publish
workflow, so the org-level secret pair (if configured) covers
both repos without setup. Neither secret is referenced in
PR-mode runs, so PRs from forks build and smoke-test cleanly
without secret access.

### Versioned releases (later)

Tag-based releases (`:0.1.0`, `:0.1`, `:latest`) come once the
recipe stabilises. The mechanic will be a git tag like
`moon-base-0.1.0` driving an extra `metadata-action` config —
out of scope until Phase 2.0 ships end-to-end.

Tracker: [`specs/roadmaps/phase-02-containers.md`](../../specs/roadmaps/phase-02-containers.md).

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
