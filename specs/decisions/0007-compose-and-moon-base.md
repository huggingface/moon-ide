# ADR 0007 — Compose-native config + moon-published base image

Date: 2026-04-28
Status: accepted

## Context

Phase 2 needs to commit to two coupled choices before any code
ships: the workspace config format (what file the user — and
moon-ide — reads to know what container to run) and the image
strategy (what the container is built from).

The dev-container ecosystem has a strong default answer: each
project ships a `.devcontainer/devcontainer.json`, optionally
referencing a Dockerfile, optionally pulling in "features" like
`docker-in-docker:2`. VSCode and Codespaces both build on that.

Three things made the default unattractive for moon-ide:

1. **Workspace, not project, is the unit.** moon-ide is a
   command centre — one workspace usually holds several repos
   (a target repo, the tooling repo that builds it, sibling
   projects in the same org, an active feature branch with
   pinned dependents). The devcontainer.json convention puts
   the container inside one repo; opening four repos in a
   workspace would mean four containers fighting for ports
   and four DinD daemons to keep in sync.
2. **DinD is a recurring tax.** Every team that's wanted
   nested Docker (so the user's project can run its own
   `docker compose up`) has fought the same configuration
   battle: `fuse-overlayfs` vs. `overlay2`, `iptables-legacy`
   vs. `iptables-nft`, `containerd-snapshotter: false` for
   Docker 29+. The recipe is well-known but hard-won, and
   relitigating it inside every repo's Dockerfile is waste.
3. **Translation has a worst case.** Devcontainer.json is
   already a thin wrapper over docker run / docker compose
   under the hood. Exotic users (multi-service compose,
   custom networks, GPU pinning) eventually exit the
   devcontainer.json subset and write compose anyway — at
   which point the json file becomes vestigial.

## Decision

Two parts, intentionally bundled.

### 1. `<workspace>/.moon/compose.yaml` is the native format

Standard Docker Compose syntax. moon-ide reads it on every
workspace open, parses just enough to find the service it
attaches to (`x-moon.shell-service` extension key), and
otherwise lets compose own its semantics.

Generated once on first opt-in with sane defaults
(see [`containers.md`](../containers.md#workspace-config-composeyaml));
user-owned thereafter — moon-ide never writes back.

Reading existing `.devcontainer/devcontainer.json` files (for
interop with repos shared with non-moon-ide users) is a
**separate** concern, deferred to Phase 2.3 where it can be
debated against concrete examples. The native format isn't
that.

### 2. moon-ide publishes its own base image

`ghcr.io/huggingface/moon-base:<tag>`, built from Debian stable
in the moon-ide repo's CI, shipping a polyglot toolchain plus a
pre-baked DinD setup (the canonical recipe). Teams `FROM
moon-base` to add their own tooling on top.

Versioning: `:<commit-sha>` for reproducibility, `:<major>.<minor>`
for human convenience, `:latest` for casual use. Generated
`compose.yaml` pins to a specific tag, never `:latest`.

Why our own image, not Microsoft's:

- Long-lived workspace containers warrant base-image control;
  we'd rather own the upgrade cadence than inherit MS's.
- The DinD recipe lives in the image, not in features layered
  on top — every workspace inherits the working setup with
  zero per-team configuration.
- The bootstrap concern (ADR 0005 — moon-ide must build
  inside its own container) gets a clean home: it's just a
  layer in moon-base's Dockerfile.

## Why these two are bundled

They reinforce each other.

- If we adopted compose without owning a base image, every
  team would need to solve DinD themselves in their
  `Dockerfile`. Solved by `moon-base`.
- If we owned `moon-base` but kept devcontainer.json native,
  the per-project-container assumption would still leak into
  every workflow — `moon-base` is fundamentally a workspace
  image.
- If we kept devcontainer.json + features, we'd be relitigating
  DinD per repo and inheriting MS's image cadence.

Either decision alone is a half-measure; together they describe
one model.

## Consequences

- The old `specs/devcontainers.md` is replaced by
  [`specs/containers.md`](../containers.md). Its
  process-injection design (`docker cp` a static-musl
  `moon-agent` into an arbitrary base image) is preserved as
  the future `RemoteHost` story (SSH / Codespaces) but is not
  what local containers do.
- The `WorkspaceHost::ContainerHost` impl uses bind-mount
  (filesystem ops are direct host I/O) plus `docker exec`
  (process / PTY ops). Simpler and faster than running an
  in-container agent for a case where the host and container
  share a filesystem.
- moon-ide grows a publishing concern: a CI workflow that
  builds + tags + pushes `moon-base` on every change to its
  sources (not on every moon-ide commit). The image is a
  **multi-arch manifest** — `linux/amd64` + `linux/arm64`,
  built natively on free GitHub Actions runners. arm64 is
  the team's primary target (Mac-majority, Apple Silicon);
  amd64 covers Linux contributors and CI. Detail in
  [`containers.md`](../containers.md#distribution).
- The DinD recipe is now version-controlled in moon-ide
  itself — when Docker N+1 changes the storage-driver story
  again, there's one place to fix it.
- `compose.yaml` is the format the team already knows; we
  inherit ecosystem conventions (override files, profiles,
  env interpolation) for free.
- Devcontainer.json interop becomes a translator that runs
  on workspace open (Phase 2.3). Translation is a one-way
  read; we never write devcontainer.json files.

## Reversibility

Reversible, with cost.

- Switching the format from compose back to devcontainer.json
  native is a parser swap and a lifecycle redesign — moderate.
- Switching the base image from a moon-published one to a
  team-managed one (or to MS's) is just a Dockerfile change in
  every `compose.yaml` we generate — cheap.
- The decision worth being most careful about is the
  one-container-per-workspace shape. If experience says
  per-project is right after all, the lifecycle layer
  inherits the change but the trait layer (`ContainerHost`)
  doesn't.

## Open follow-ups

- Image registry choice (GHCR vs. HF Hub) — defaulting to GHCR
  because moon-ide is on GitHub and CI ergonomics are simplest
  there. Reversible by changing the publish workflow + the
  generated compose's image reference. Pin a final answer when
  Phase 2.0 ships.
- Whether `moon-base` becomes its own repo or stays in-tree.
  Lean: stay in-tree until "publish moon-base" outweighs
  "moon-ide and moon-base versions drift". The smoke test
  (`bun install && cargo check` inside the freshly built
  image) is a release gate either way.
