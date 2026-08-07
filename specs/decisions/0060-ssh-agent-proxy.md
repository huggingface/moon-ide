# ADR 0060 — ssh-agent proxy socket + public-key pass-through

Date: 2026-08-08
Status: accepted; extends the ADR 0026 stale-socket lesson to the
agent forward.

## Context

Two container-SSH failures hit in practice:

1. The workspace shell container bind-mounted the host's
   `$SSH_AUTH_SOCK` **file**. A file bind-mount pins the inode, so a
   host agent restart (re-login, keyring restart) left every running
   container with a dead socket (`Connection refused`) until it was
   recreated — the exact failure ADR 0026 fixed for `instance.sock`
   by mounting a directory.
2. The host's `~/.ssh/config` (mounted read-only) uses
   `IdentityFile` + `IdentitiesOnly yes` to stop the agent offering
   too many keys (servers reject after 6). Those `IdentityFile`
   entries point at private keys that exist only on the host, so
   in-container ssh had no way to select the right agent key.

## Decision

1. **Agent proxy** (`moon_container::agent_proxy`): the IDE's host
   process listens on a stable socket at
   `<data>/moon-ide/ssh-agent/ssh-auth.sock` and pipes each
   connection to the _live_ host agent, re-resolved per connection
   (`$SSH_AUTH_SOCK`, then gnome-keyring / gcr / systemd user-unit
   paths; own socket excluded to prevent loops). Compose mounts the
   proxy **directory** at `/run/host-services`; the in-container
   `SSH_AUTH_SOCK` path is unchanged. Agent restarts, re-logins and
   IDE restarts all heal without recreating containers.
   Linux only; macOS keeps Docker Desktop's magic socket. If the
   proxy isn't running (tests, non-IDE callers), compose falls back
   to the old direct `$SSH_AUTH_SOCK` file mount.
2. **Public-key pass-through**: `~/.ssh/*.pub` files are bind-mounted
   read-only into the container's `~/.ssh/`. With host config
   entries written as `IdentityFile ~/.ssh/<key>.pub` +
   `IdentitiesOnly yes`, both host and container ssh offer exactly
   that key and obtain signatures from the agent. No private
   material ever enters the container.

## Alternatives considered and rejected

- **Mount the live agent socket's parent directory.** For
  gnome-keyring that directory also holds the Secret Service control
  socket — mounting it would hand every container the host keyring.
  The proxy directory contains exactly one socket, ours.
- **Mount the whole `~/.ssh` read-only.** Puts private keys in the
  container; the threat model (ADR 0008) keeps them host-side.
- **Copy `.pub` files into the image / a volume at setup time.** Goes
  stale when keys rotate; bind mounts track the host. (New `.pub`
  files still need a container recreate to appear — acceptable.)

## Consequences

- Users should point `IdentityFile` at the `.pub` (works identically
  on the host: ssh loads the public key and asks the agent for the
  signature).
- One more long-lived task in the desktop process; failure degrades
  to the previous behaviour with a warning.
- Existing containers must be recreated once to pick up the new
  mounts.
