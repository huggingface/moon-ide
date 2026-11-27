# ADR 0128: File-backed secrets for headless processes

Extends [ADR 0059](0059-headless-enrolled-ide.md) and
[ADR 0035](0035-public-relay-deployment.md), replacing their
`dbus-run-session` + `gnome-keyring-daemon` recipe.

## Context

Every secret goes through the OS keyring. A server has none, so the
headless `moon-remote` and the public `moon-bridge` ran inside a
private D-Bus session with a gnome-keyring unlocked by an empty
password: a wrapper script per box, a store that can't be provisioned
or backed up, and nothing a container can use. Reinstalling a box
meant signing in, enrolling and re-entering every key by hand.

## Decision

- **`MOON_SECRETS_DIR` switches `moon-remote` and `moon-bridge` to a
  directory of files**, one per keyring account, owner-only, written
  by temp file and rename. Unset, they use the OS keyring as before,
  so a desktop machine still shares one store between the app and the
  headless binaries. The desktop app never reads the variable.
- **`MOON_MANAGED_SECRETS_DIR` is an optional read-only layer** for
  what the operator provisions (provider keys, the web-search key)
  from a secret manager. A managed entry shadows the writable one and
  refuses set and delete: a key changed from the UI must not silently
  diverge from the provisioned one. One trailing newline is dropped
  on read.
- **What the process mints or rotates stays in the writable
  directory**: the HF sign-in, ChatGPT tokens, the relay token,
  pairing and e2e keys. It needs a persistent volume, not a secret
  manager.
- **File name = account**, with `:` written as `.`
  (`coder-provider:<id>` → `coder-provider.<id>`). Names stay inside
  `[-._a-zA-Z0-9]`, the key charset of a Kubernetes Secret, so a
  mounted Secret provides any account without renaming.
- Files are plaintext. The boundary is the directory's permissions
  and the volume's encryption, which is what the empty-password
  keyring amounted to.

## Rejected

- **Keeping the gnome-keyring recipe.** Needs D-Bus and a daemon in
  every image, and its store is an opaque blob.
- **Secrets in environment variables.** Can't hold what the process
  rotates, and leak through `/proc` and child processes — the coder
  runs arbitrary commands.
- **Calling the secret manager's API from moon-ide.** Ties the
  binaries to one vendor; a mounted directory works with any of them
  and with none.
- **Encrypting the files with a passphrase.** The passphrase would
  sit next to them on an unattended box.
- **Secrets in the HF bucket that syncs sessions.** Not a secret
  store.

## Open

Providers are declared in `state.json` under generated ids, so a
fresh install can't yet be handed its provider keys without first
creating the providers. Declaring them up front belongs to the
headless-deployment phase (roadmap Phase 17).
