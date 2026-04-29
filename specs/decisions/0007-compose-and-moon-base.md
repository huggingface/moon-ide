# ADR 0007 — Compose-native config + moon-published base image

Date: 2026-04-28
Status: accepted (with amendments — the
[DinD-in-`moon-base`](#2-moon-ide-publishes-its-own-base-image)
aspect of this decision was reverted by
[ADR 0008](0008-host-shared-daemon.md), and the
[state-directory + multi-folder shape](#amendment-2026-04-29--state-dir-and-multi-folder-mounts)
was added as part of Phase 2.5).

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

`huggingface/moon-base:<tag>` on Docker Hub, built from Debian
stable in the moon-ide repo's CI, shipping a polyglot toolchain
plus a pre-baked DinD setup (the canonical recipe). Teams
`FROM moon-base` to add their own tooling on top. Multi-arch
(`linux/amd64` + `linux/arm64`); see [Distribution](../containers.md#distribution).

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

### Registry: Docker Hub

Published to **Docker Hub** at `huggingface/moon-base`, not
GHCR or HF Hub. Trade-offs walked through:

- **GHCR (rejected)**: ties the workspace's runtime base image
  to a GitHub/Microsoft account. moon-ide already lives on
  GitHub — having both source hosting _and_ the runtime image
  go through the same vendor concentrates the dependency.
  We'd rather not bake that into every team member's
  `docker pull` path for a foundational artefact.
- **Private Docker registry (rejected)**: image needs to be
  public so contributors and downstream users can pull
  without credential setup. Rules out anything self-hosted-and-
  gated.
- **HF Spaces "run locally" (rejected)**: HF Spaces only ship
  `linux/amd64` artefacts; using them as the distribution
  channel would lock arm64 (the primary target) out.
- **Docker Hub (chosen)**: vendor-neutral relative to our
  GitHub dependency, public, multi-arch-native, idiomatic
  (`docker pull huggingface/moon-base` works without a
  registry prefix, the syntax most teams already know).

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
- ~~The DinD recipe is now version-controlled in moon-ide
  itself — when Docker N+1 changes the storage-driver story
  again, there's one place to fix it.~~ Superseded by
  [ADR 0008](0008-host-shared-daemon.md): we no longer
  embed dockerd in `moon-base`. Project services run as
  siblings on the host's daemon via compose `include:`. The
  rest of this ADR — compose as the native format, moon-ide
  publishing its own base image, Docker Hub as the registry,
  the polyglot toolchain in `moon-base`, devcontainer.json
  interop deferred to 2.3 — stands.
- `compose.yaml` is the format the team already knows; we
  inherit ecosystem conventions (override files, profiles,
  env interpolation, **`include:`**) for free.
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

- Whether `moon-base` becomes its own repo or stays in-tree.
  Lean: stay in-tree until "publish moon-base" outweighs
  "moon-ide and moon-base versions drift". The smoke test
  (`bun install && cargo check` inside the freshly built
  image) is a release gate either way.

## Amendment (2026-04-29) — state dir and multi-folder mounts

The original wording put `compose.yaml` inside the workspace
itself (`<workspace>/.moon/compose.yaml`), and described the
"workspace" as if it were one folder. Phase 2.5's multi-folder
UX makes that conflation incoherent: a workspace is now a list
of folders, the active one swaps without compose-project
churn, and the file we generate has to mount _every_ bound
folder. The amendment below lands that shift; the rest of the
ADR (compose as the native format, `huggingface/moon-base` on
Docker Hub, polyglot toolchain in the base image,
devcontainer.json interop deferred to 2.3) is unchanged.

What moves:

- **State dir**. `compose.yaml` lives at
  `<dirs::data_local_dir>/moon-ide/workspaces/<id>/compose.yaml`
  — outside any specific repo, decoupled from any specific
  folder. `<id>` is the constant `"default"` until multi-
  workspace UI ships (Phase 7); the layout is forward-compatible
  with named workspaces under sibling subdirectories. A
  sibling `bound-folders.json` records the list of folder paths
  the file was generated from, so the generator stays
  deterministic and the workspace's bound set survives an IDE
  crash without consulting `app_state.json`.
- **Project name**. `moon-ws-<id>` (so `moon-ws-default`),
  derived from the workspace id, not a hash of any path. The
  compose project survives folder switches and folder add /
  remove; what changes is the contents of `compose.yaml`, not
  its identity.
- **Mount layout**. Each bound folder is bind-mounted at
  `/workspace/<basename>` inside the dev container, with
  `working_dir: /workspace`. Single-folder workspaces (the
  common case today) get one mount; multi-folder is the same
  shape with more entries.
- **Path resolution**. Every path inside the generated
  `compose.yaml` is **absolute** — host-side absolute for
  bind mounts and `include:` entries. The earlier `../`-based
  layout was meaningful only when the file lived next to a
  repo; from a state-dir under `dirs::data_local_dir`, no
  base path makes the relative form clearer than the absolute
  one.
- **User-owned thereafter**. The original ADR called the file
  "user-owned after first generation". That contract no longer
  holds: the IDE rewrites `compose.yaml` whenever the bound-
  folder set changes, because the bound set _is_ what the file
  encodes. Per-project customisations belong in each project's
  own `docker-compose.yml`. _Update under the
  [2026-04-29 second amendment](#amendment-2026-04-29--workspace-shell-vs-project-services):
  those project compose files are no longer pulled in via
  `include:` — they run as separate compose projects, managed
  per folder. moon-ide still never rewrites them._
  Bound-folder edits go through `bound-folders.json` (or, in
  practice, the IDE's add / remove folder gestures); the
  compose file is a derived artefact. The file header is
  reworded accordingly.

What stays:

- Compose itself, `huggingface/moon-base` on Docker Hub, the
  multi-arch publishing pipeline, the bootstrap-via-`moon-base`
  story, and the Phase 2.3 devcontainer.json interop deferral.
- `WorkspaceHost::ContainerHost` (Phase 2.1) still uses bind-
  mount + `docker exec`; the only difference is that the bind
  source is now per-folder rather than the workspace root.

Consequence beyond Phase 2: this is the layout multi-workspace
(Phase 7) inherits. `workspaces/<id>/` is the shared root; a
new workspace is a new id with its own state dir; `compose.yaml`
generation, project naming, and lifecycle commands are already
keyed on id and don't need to change shape when more than one
exists at a time.

## Amendment (2026-04-29) — workspace shell vs project services

The first amendment landed multi-folder shape but kept a single
compose project per workspace, with each bound folder's
`docker-compose.yml` pulled in via `include:`. That model
conflated two concerns the user thinks about separately:

- The **workspace shell** — the moon-ide `dev` container that
  hosts terminals, LSP servers, agents. One per workspace,
  IDE-managed, expected to be up almost always.
- **Project services** — a folder's own `docker-compose.yml`
  (gitaly, mongo, redis, …). Per folder, started/stopped on
  demand by the user from the folder bar.

In production this conflation made a stalled gitaly container
in moon-landing block the IDE's own ability to give the user a
terminal — `compose up -d --wait` waited on every included
service, not just `dev`. Once one project broke, the workspace
was unusable, and the status pip reported "setting up…" for as
long as the daemon refused to settle.

What changes:

- The workspace's generated `compose.yaml` is **dev-only**. No
  more `include:`. The file just defines the `dev` service and
  bind-mounts each bound folder under `/workspace/<basename>`.
  Setup is fast (one image pull, no project-service health
  waits) and doesn't depend on any user project's correctness.
- Each bound folder's own root-level compose file
  (`<folder>/docker-compose.yml` or `compose.yaml`) is run as
  a **separate** compose project. moon-ide doesn't generate or
  modify that file — it just shells out with
  `docker compose -f <user's file> -p moon-ws-<id>-<slug> ...`.
- Project name namespacing: the workspace shell stays at
  `moon-ws-<id>`, per-folder projects sit at
  `moon-ws-<id>-<folder-slug>` (slug is the lower-cased
  basename with non-alnum collapsed to `-`). A single
  `docker compose ls --filter name=moon-ws-default-`
  enumerates everything the workspace owns.
- Folder-bar UX: each bound folder grows a small status
  indicator (visible only when the folder has a compose
  file at its root). Click opens a per-folder popover with
  Start / Pause / Resume / Rebuild / Stop and a service
  list, mirroring the workspace shell's status pip but
  scoped to that one project.
- Networking: the dev shell and per-folder services run on
  separate compose networks by default. Cross-talk via
  `host.docker.internal:<port>` if the user's compose
  exposes host ports; an explicit external network is the
  escape hatch. Phase 2.2 will formalise routing.
- Discovery scope: per-folder lifecycle uses **only** the
  folder's root compose file. Sub-directory composes that
  the previous `include:`-based discovery would have picked
  up are out of scope for the per-folder UX — the user can
  bind the sub-directory as its own workspace folder if
  they want to manage it from moon-ide.

What stays:

- The workspace state dir (`<dirs::data_local_dir>/moon-ide/
workspaces/<id>/`), the `bound-folders.json` sidecar, and
  the workspace project name (`moon-ws-<id>`). The
  `compose.yaml` we write there is just narrower now.
- `huggingface/moon-base` on Docker Hub, the multi-arch
  publishing pipeline, the bootstrap-via-`moon-base` story,
  and the devcontainer.json-interop deferral.
- The host-shared Docker daemon (no DinD), per
  [ADR 0008](0008-host-shared-daemon.md).

Migration: the previous unified project (`moon-ws-default`
with the `dev` service plus every included project's
services) doesn't auto-clean up. After upgrading, the user's
old containers from moon-landing's `include:` are orphaned
under `moon-ws-default`; the rewritten `compose.yaml` only
declares `dev`. A one-time
`docker compose -p moon-ws-default down --remove-orphans`
clears them, and the new per-folder UI takes over for
moon-landing's services. Test plan
[0012-workspace-shell-vs-project-services](../test-plans/0012-workspace-shell-vs-project-services.md)
documents the upgrade path.
