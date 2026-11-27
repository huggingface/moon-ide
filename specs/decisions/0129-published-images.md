# ADR 0129: Images are published to the Hugging Face container registry

Supersedes the registry choice of
[ADR 0007](0007-compose-and-moon-base.md) (Docker Hub). Part of
roadmap Phase 17.

## Context

ADR 0007 picked Docker Hub for `moon-base`. The workflow was written
but its credentials were never provisioned, so nothing was ever
published: every machine built the 5 GB image locally, and the
headless binaries were copied to servers by hand. The Hub now has a
container registry (`cr.hf.co`) that serves multi-arch images and
lets anyone pull a public one without logging in — the two things
ADR 0007 wanted from Docker Hub.

## Decision

- **Three public images under `cr.hf.co/huggingface/`**, built by
  GitHub Actions, tagged `:dev` (moves with `main`) and
  `:sha-<commit>`:
  - `moon-base` — unchanged in content and cadence: multi-arch,
    rebuilt only when its own sources change.
  - `moon-bridge` — the relay and the companion PWA on a slim base.
  - `moon-remote` — the headless IDE **on top of `moon-base`**.
    Agents' commands run in its process tree, so the toolchain has
    to be around it.
- The headless images follow moon-ide commits and are `amd64` only:
  every machine they run on is.
- One credential, `HF_REGISTRY_TOKEN`, pushes all three. The
  container repos are created on the Hub beforehand; a push does not
  create them.
- Both headless images default `MOON_SECRETS_DIR`
  ([ADR 0128](0128-file-backed-secrets-for-headless.md)) to a
  directory under their data dir, so one volume holds everything
  they must keep.

## Rejected

- **Docker Hub.** Needs an org account nobody set up, and rate-limits
  anonymous pulls.
- **GHCR, a private registry.** As in ADR 0007: a login on the pull
  path.
- **A slim `moon-remote` image.** It could serve the phone but an
  agent's first `cargo` or `bun` would not be there.
- **Building the binaries inside `moon-base`.** Proven to compile
  the workspace, but makes every relay build pull 5 GB; the plain
  Rust image on the same Debian release links against the same
  glibc.
