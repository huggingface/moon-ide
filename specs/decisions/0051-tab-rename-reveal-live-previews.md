# ADR 0051: Tab-strip file rename, reveal-in-file-manager, live binary previews

## Context

Three small editor-chrome asks landed together:

1. Rename a file from its open editor tab (the file tree already had
   inline rename via Pierre; the tab strip did not).
2. "Open containing folder" — jump to the host file manager from the
   file tree and the tab strip.
3. Image previews went stale: open → edit externally → reopen showed
   the old render until the tab was closed.

## Decision

**Tab rename** is tab-strip-local chrome: `EditorTabs` swaps the label
for an inline input (Enter/blur commits, Escape cancels) and calls the
existing `WorkspaceState.renamePath`. Renaming is leaf-name-only
(separators refused), matching the tree's input. `renamePath` now also
refuses when the buffer (or any buffer under a renamed directory) is
dirty — otherwise the unsaved text rebinds to the new path while disk
holds stale bytes, and the next save silently publishes under the new
name.

**Reveal** is a host-side `fs_reveal_in_folder` Tauri command over
`tauri-plugin-opener`'s `reveal_item_in_dir` (already a dependency).
It is deliberately **not** a `WorkspaceHost` method: the file manager
runs on the user's desktop, and the container bind mount makes the
host path valid there. The Linux backend does a blocking D-Bus
roundtrip, so the command runs on the blocking pool.

**Live previews**: the `fs:changed` per-buffer reload loop previously
skipped binary buffers entirely (`kind !== 'text'` guard). It now
calls `refreshPreviewFile` when the watcher named an open image/PDF's
exact path. The webview caches `asset:` responses per URL, so the
refresh appends a dummy `?t=<previewToken>` query and bumps the token;
`ImageView` re-keys its `<img>` on the token, `PdfView` re-runs its
render effect on it. Only a concrete changed-path subset triggers a
reload — the `null`-subset full sweep (window focus, palette refresh)
skips binaries, since nothing indicates their bytes moved.

## Rejected alternatives

- **Route reveal through `WorkspaceHost`** — the trait is about fs
  parity between local/container/remote; a desktop file manager is
  none of those hosts' business.
- **Refetch previews on every refresh, or on `null` subset** —
  needlessly re-streams every open image on each window focus.
- **Bump `previewUrl` without a token** — Svelte patching `src` on the
  same `<img>` can keep a cached/partially-decoded render; the keyed
  block forces a fresh element.
