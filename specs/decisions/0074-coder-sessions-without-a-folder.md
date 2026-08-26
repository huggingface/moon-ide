# ADR 0074 — Coder sessions without a bound folder

## Context

Every coder command resolved through the active workspace folder and
errored `NoActiveFolder` when the workspace was empty: you couldn't
ask the agent anything — not even a folder-free question — until
you'd picked a directory. That's a real friction point on a fresh
window: the natural first move is often "help me set up / clone /
scaffold X", which the coordinator toolkit (`clone_repo`, `init_repo`)
can literally do, except it never got to run.

## Decision

An empty workspace gets a **scratch root**: the user's home
directory, canonicalised. It is never registered in
`WorkspaceRegistry` — `CoderState::folder_entry_for` synthesises a
`WorkspaceFolderEntry` (plain `LocalHost`) on demand for exactly
that path — so it can't leak into the folder bar, the MCP roots set,
the fs watcher, or container mounts.

- **Sessions file under the scratch root's slug**
  (`coder-sessions/<home-slug>/`), so `list_sessions` / `new_session`
  / `send` / `open_session` / delete / rename / search / revert /
  open-trace all work unchanged through the existing per-folder
  plumbing. `last_session_by_folder` uses the empty-string key (the
  frontend's existing `NO_FOLDER_KEY`).
- **Tool posture**: relative paths and `bash` run from home; absolute
  host paths keep working through the out-of-workspace arm (ADR
  0025). `grep` validates its root exists instead of silently
  returning 0 matches on a bad root. A scratch session never routes
  to the workspace shell container (its root is never in the
  container's mount set) and never gets format-on-save or project
  rules — home has no project config. The system prompt gains a
  "No folders bound" section explaining the posture; the "Bound
  folders" section is absent when none are bound.
- **A scratch coordinator can bootstrap the workspace**: `init_repo`
  / `clone_repo` / `add_folder` resolve their sibling destination
  against the scratch root (a new project lands at `~/<name>`), and
  `spawn_worker` / `check_worker_base` / `workspace_scm_status`
  resolve the scratch entry the same way.
- Sessions started under the scratch root **stay** there: binding a
  folder later doesn't migrate them (their transcript, events, and
  tooling are already anchored). New sessions in the freshly bound
  folder are separate.

## Rejected alternatives

- **Keep refusing**: the failure mode the user hits is "type a
  question, get `NoActiveFolder`" — the composer already accepts
  drafts with no folder bound, so the refusal reads as a bug.
- **Register home as a real workspace folder**: it would appear in
  the folder bar, get file-tree/LSP/SCM treatment, and join the MCP
  roots set — a folder the user never asked for.
- **Sessions keyed "workspace-global" instead of home-slugged**:
  conflates scratch sessions across every empty workspace the app
  ever opens; the slug keeps them distinct per machine layout and
  reuses `sessions_dir` unchanged.
