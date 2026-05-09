# ADR 0008 — Host-shared Docker daemon, no nested Docker

Date: 2026-04-28
Status: accepted

## Context

Phase 2's container model gives every workspace a `dev`
container — a sandbox the IDE shells into for terminals,
LSPs, builds, and lints. Real projects also need
side-services: postgres, redis, mongo, an internal HTTP
service, a queue. moon-landing's `docker-compose.yml`
declares ten of them today.

We have to decide where those service containers run, which
in practice means deciding what privileges the workspace
container needs. There are three candidate models:

1. **Nested Docker (DinD) inside the `dev` container.** The
   workspace container ships its own `dockerd`, the user's
   project compose runs nested. We landed this in
   `8b5a2f7` / `026d410` and walked it back in this ADR.
2. **Sibling services on the host's daemon, via compose
   `include:`.** moon-ide generates a workspace-level
   `compose.yaml` (in moon-ide's per-workspace state directory),
   which `include:`s the project's own `docker-compose.yml`
   and adds a `dev` service alongside. Everything runs on the
   user's host daemon as one compose project.
3. **Forwarded host socket.** Bind-mount `/var/run/docker.sock`
   into the workspace container; the user's `docker` from
   inside actually creates _sibling_ containers on the host's
   daemon (not nested).

The decision turns on threat model. The workspace container
is constantly running freshly-fetched supply-chain code:
`bun install`, `cargo build`, `pip install`, untrusted
submodules, language-server extensions. That's the realistic
attack surface in a dev environment, and the question is how
much of the host a single compromised dependency can reach.

## Decision

**Adopt model (2): sibling services on the host's daemon via
compose `include:`.** The workspace container runs
**unprivileged** with Docker's default capability set; the
project's services run as siblings on the host's daemon,
declared by the project's own `docker-compose.yml` and pulled
into moon-ide's compose project via the top-level `include:`
directive.

Concretely:

- `moon-base` does not embed dockerd, fuse-overlayfs,
  iptables, or a docker CLI.
- The generated workspace `compose.yaml` does **not**
  set `privileged: true` on the `dev` service.
- When moon-ide spots a sibling `docker-compose.yml` (at the
  workspace root or in any first-level repo) it adds an
  `include:` entry pointing at it; the user is free to remove
  or reorder them.
- `docker compose up -d` brings up everything (project
  services + `dev`) on the host's daemon, sharing the
  default compose network. From inside `dev`, services are
  reachable by service name.

The user retains the option of also running their app
on the host (which is what moon-landing does today) or inside
`dev` — both work without configuration change.

## Consequences

### Security

Dropping `--privileged` is the point. With DinD + privileged,
a malicious `bun install` inside the workspace can drop into
host root via `/dev` access, mount the host disk, write to
`/proc/sysrq-trigger`, etc. — supply-chain compromise turns
into machine compromise. With model (2) the workspace gets
the default Docker capability set: no `CAP_SYS_ADMIN`,
seccomp on, AppArmor on, no raw devices. Escape requires a
kernel-level Docker bug, not just user-level shenanigans —
materially harder.

This is not _absolute_ safety — `cargo build` still runs in
the user's session and can still wreck the user's home
directory. But the host kernel and the rest of the user's
machine state are no longer one privileged container away
from a typo'd dependency name.

### Functionality

We don't lose anything moon-landing-shaped projects need.
The walk-through in [containers.md](../containers.md#walk-through-moon-landing)
shows that moon-landing's existing compose works untouched
under `include:`. The user logged into
`registry.internal.huggingface.tech` on their host's daemon
picks up private images transparently; service-to-service
networking happens via the compose network exactly as today;
host-published ports keep being reachable from host tools.

What we deferred / dropped:

- **Nested-tenant isolation.** A workspace can't host a
  user's "runs `docker run` against random images, mustn't
  see the host's other containers" scenario. Nobody on the
  team runs that workflow.
- **Workspaces that genuinely _author_ Dockerfiles inside the
  container** (e.g. `docker buildx bake`). Will be addressed
  via a forwarded-socket opt-in (model (3)) when somebody
  hits it. The plumbing is small.

### Operational

- `moon-base` shrinks (~500 MB lighter without docker-ce +
  containerd + buildx + compose plugins + fuse-overlayfs +
  iptables).
- The `moon-base` image is `--privileged`-free, so the
  workspace can run on hardened hosts (corp-managed laptops,
  shared dev machines) where privileged containers are
  policy-blocked.
- Pause/unpause now applies to the whole compose project
  (workspace + services), which is _better_: the user's
  in-flight mongo state and populated redis caches survive
  workspace close exactly as the editor's open buffers do.

## Alternatives

### Model (1) — nested Docker (rejected)

Walked through the canonical recipe (fuse-overlayfs +
iptables-legacy + the Docker 29+ snapshotter flag) and shipped
it in `8b5a2f7` / `026d410`. It worked. It's "industry
standard" in the sense that everyone who's done this accepts
the same trade-off. The trade-off is that a single compromised
dependency can root the host. For the Phase 2 use case
(developing trusted code that pulls untrusted dependencies)
that trade is on the wrong side.

There's also a partial mitigation we considered: only require
privileged on Linux, where escape is direct, and let macOS
contributors run with the protection of Docker Desktop's
Linux VM. We rejected this because (a) the asymmetry confuses
the threat model rather than clarifying it, (b) Mac users can
still be socially engineered into running on Linux machines
(CI, remote pairing), and (c) "we're privileged on half our
contributors' boxes" is not a story we want to defend in
review.

### Model (3) — forwarded host socket (deferred, not rejected)

Bind-mount `/var/run/docker.sock` into the workspace and let
the user run `docker` from inside; sibling containers, no
nesting. This doesn't actually improve security over DinD —
anyone with the socket has effective host root via
`docker run --privileged ...` — but it's simpler and we don't
have to ship a daemon.

We don't take it _now_ because Phase 2.0 doesn't need it —
the compose layer covers the realistic project-services use
case without the workspace ever calling `docker` itself. When
a concrete need shows up (someone wants `docker buildx bake`
inside the workspace, someone wants `act` for GitHub
Actions), we re-open the question with that workflow in
hand.

### Sysbox (rejected, complexity)

Nestybox's Sysbox runtime supports unprivileged DinD by
running the inner daemon under user namespaces. Real
isolation, no `--privileged`. Two reasons we passed:

- Adds a host runtime dependency (the contributor's Docker
  install needs Sysbox configured as a registered runtime).
  On Docker Desktop for macOS — the team's primary target —
  installing Sysbox inside the Linux VM is awkward and not
  officially supported.
- It solves a problem we now don't have. The compose
  `include:` model doesn't need nested Docker at all.

### Switching to Podman (rejected, no benefit here)

Podman's headline is rootless containers — even if a process
escapes "root in container", it lands as a normal user on
the host. Real improvement, but only for the escape case,
and only if we _also_ drop privileged. With model (2) the
workspace is already unprivileged; Podman's gain over Docker
collapses to "Linux contributors have one less daemon
running", which we don't value enough to pay for an extra
host dependency on Mac contributors who already have Docker
Desktop. Podman compose is also less battle-tested than
Docker Compose for the `include:` directive specifically.

## Notes

- The first nested-Docker spike (`8b5a2f7` + `026d410`)
  remains in the history as record-of-experiment. The follow-up
  reversal commit references this ADR. We're not editing
  history to pretend it didn't happen.
- ADR [0007](0007-compose-and-moon-base.md) committed to
  compose as the native format and to publishing a custom
  base image; this ADR refines that commitment by fixing the
  daemon topology. There is no contradiction with 0007 — it
  was always silent on whether the daemon was shared with the
  host. We make that explicit here.
- This decision is reversible. Adopting model (3) for an
  opt-in workspace is a small additive change; adopting (1)
  later requires un-shipping ergonomic guarantees, but
  nothing on the user-facing surface forecloses it.
