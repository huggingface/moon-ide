# Phase 16 — Remote workspaces

STATUS: in progress. Decision record:
[ADR 0095](../decisions/0095-remote-workspaces.md).

Goal: a workspace runs on an always-on server; either desktop attaches
to it over SSH with the full IDE (coder, review, terminals, browser
tab, editor), picks up exactly where the other left off, and moves
files in and out of it.

## 16.1 — Frontend transport seam (landed)

- Every core call goes through one transport (`ipc.ts`), every push
  event through one subscription helper, and binary-preview URLs
  through one resolver — no component imports `@tauri-apps/api/core`
  `invoke` / `event` `listen` / `convertFileSrc` directly.
- Local mode only; behaviour unchanged.

## 16.2 — One dispatched command surface

- Move the workspace process's state and command bodies out of
  `src-tauri` into a Tauri-free crate with a name-based dispatcher and
  an event sink trait; Tauri serves it through a single command and
  forwards sink events as Tauri events.
- Split out the desktop-shell commands (windows, dialogs, opener,
  clipboard, focus) that stay local in both modes.
- Lands in slices (fs/search, git, terminal, LSP, containers, coder,
  the rest) so the desktop keeps working at every commit.
- Landed: `moon-app` crate + `rpc` command + event sink; fs / git,
  search, editorconfig, session, coder, browser, logs, terminal, LSP,
  container / compose / ports / lifecycle and workspace folders + fs
  watcher groups.

## 16.3 — Server and attach (landed)

- `moon-remote serve` hosts the full surface (not just the
  companion subset) next to the relay client.
- `moon-remote attach --workspace <slug>`: stdio JSON-RPC bridged to
  the workspace process's local socket, launching `serve` if needed.
- Desktop: "Workspace: Open remote workspace…" (host + workspace)
  adds a catalog entry with a `remote` target; its window runs no
  local workspace process and forwards `rpc` over an SSH
  `ControlMaster` connection; reconnect with backoff after network
  loss / sleep, page reload on reconnect so no store is stale.
- Requirements on the server: `moon-remote` on the login-shell PATH,
  the workspace created there (`moon-remote workspace-add`), key-based
  SSH (`BatchMode`), and a Secret Service for the keyring (ADR 0035
  recipe). Relay enrollment is optional.
- Known gaps, picked up by later sub-phases: image / PDF previews
  (asset protocol reads the desktop's disk), "Add folder" (opens a
  local folder dialog), `$GIT_EDITOR` forwarding from remote
  terminals.

## 16.4 — Streams

- Terminals, LSP, fs-watch and coder events over the attach channel;
  input / resize back. Usable latency on a normal home connection.

## 16.5 — Tunnels

- Browser tabs: loopback listener per preview target forwarded to the
  server's preview proxy; "open in system browser" uses the same
  local URL.
- Declared port forwards land on the desktop's loopback.

## 16.6 — File transfer

- Drag-and-drop from the OS onto the file tree and "Upload file…"
  upload; "Download to this computer" downloads. Streamed, with
  progress and cancel, temp-file + rename, confirm before overwrite.
  Works for local workspaces as a copy.

## 16.7 — Attach / takeover (landed with 16.3)

- One attached desktop per workspace; a second attach detaches the
  first, which shows a "detached — attach again" state. UI state
  persists server-side so the new desktop opens on the same tabs.

## Deliberately not done

- Session sync / handoff between machines (rejected, ADR 0095).
- Concurrent multi-desktop attach.
- Desktops through the relay.
- "Save to workspace" for downloads made inside the in-IDE browser
  tab (drag the file from the OS download folder instead).

## Completion checklist

- [ ] 16.1–16.7 landed, `bun run check` / `lint` / `test`,
      `cargo clippy` / `test` clean
- [ ] Start a turn on desktop A, close A, attach from B: same
      session still running, same tabs, terminals with scrollback,
      browser tab on the dev server
- [ ] Upload a multi-GB file and use it from the container terminal
- [ ] Human review
