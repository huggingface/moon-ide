# Phase 2.5 — Multi-folder workspace UX

The "command centre" foundation. A workspace stops _being_ a
folder and becomes a list of folders the user has bound into a
single moon-ide session. Pulled forward from Phase 7 because
[Phase 2's container redesign](phase-02-containers.md#pending-redesign-workspace--folder)
— workspace state lives outside any repo, the compose project
survives folder switches — only makes sense once "workspace"
and "folder" are different things.

Architectural reference: [`containers.md` § Multi-folder workspace](../containers.md#multi-folder-workspace-the-command-centre-ux)
for the container shape that lights up on this. Single-folder
[ADR 0006](../decisions/0006-no-settings-file.md)'s position on
per-machine vs project state still holds — there is no
per-folder settings file.

## Acceptance

- Opening a folder adds it to the workspace as a new folder bar
  in the sidebar instead of replacing the active workspace.
- Folder bars are stacked vertically, one per row. Active bar
  shows a `▾` and renders that folder's file tree directly
  underneath; inactive bars show `▸` (header-only).
- Clicking a bar makes that folder active. The file tree + both
  pane tab strips swap to the clicked folder's persisted state.
- Each bar has an `×` on hover. Clicking it confirms ("Remove
  `<name>` from workspace?") then drops the folder, its session
  entry, and any open files unique to it.
- An inline `+ Add folder` row at the bottom of the bar list
  opens the folder picker. Picking a folder already in the
  workspace flashes a duplicate-rejection toast.
- Per-folder tab state (open files, active file, split layout,
  focused side) persists across folder switches and across
  application restarts.
- Empty-workspace state (no folders bound) falls back to the
  Welcome screen.

## Data model

### `moon-protocol`

`Workspace` is the single workspace's whole shape:

```rust
pub struct Workspace {
    pub id: WorkspaceId,                  // "default" until 7.2 introduces slug ids
    pub folders: Vec<WorkspaceFolder>,    // insertion order, displayed in bar order
    pub active_folder: Option<String>,    // absolute path, must match a folder.path
}

pub struct WorkspaceFolder {
    pub path: String,    // absolute, canonical
    pub name: String,    // basename, used as the bar label
    pub host: HostKind,  // local for now
}
```

`WorkspaceSession` carries one entry per folder:

```rust
pub struct WorkspaceSession {
    pub folders: Vec<FolderSession>,
    pub active_folder_path: Option<String>,
}

pub struct FolderSession {
    pub folder_path: String,                    // absolute
    pub open_files_left: Vec<String>,
    pub open_files_right: Vec<String>,
    pub active_left: Option<String>,
    pub active_right: Option<String>,
    pub has_split: bool,
    pub focused_side: SplitSide,
}
```

Per AGENTS' "no premature migrations": straight shape change,
no compat aliases. Existing on-disk `app_state.json` from
Phase 0–2 sessions is silently dropped on first launch.

### `moon-core`

`WorkspaceRegistry` carries a single `Workspace` (the singleton)
with multiple folders. Methods:

- `add_folder(path)` — adds; rejects duplicate; sets active
  if first folder.
- `remove_folder(path)` — drops folder from list; if it was
  active, picks the previous folder in insertion order, or
  `None` if it was the last.
- `set_active_folder(path)` — sets active; rejects unknown path.
- `active()` / `require_active()` return the whole workspace
  shape; `require_active_folder()` returns the currently active
  folder's `WorkspaceHost` for fs/search ops.

Each folder gets its own `LocalHost` instance, keyed by its
canonical path.

## Tauri commands

| Command                             | Behaviour                                                                                             |
| ----------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `workspace_open_local(path)`        | Adds `path` as a folder; makes it active. Idempotent on duplicate path (returns the existing folder). |
| `workspace_remove_folder(path)`     | Drops folder + its session entry; reassigns active if needed.                                         |
| `workspace_set_active_folder(path)` | Sets active.                                                                                          |
| `workspace_active()`                | Returns the new `Workspace` shape.                                                                    |
| `workspace_list()`                  | Stays — returns one entry, the singleton workspace. Phase 7 lights it up.                             |

`fs_*` and `search_*` commands stay path-keyed; they don't gain
a workspace argument because the active folder's host is
implicit on the call site.

## Frontend

`WorkspaceState` grows a per-folder layer:

- `folders: WorkspaceFolder[]` — workspace's folder list.
- `activeFolderPath: string | null` — pointer.
- `folderStates: SvelteMap<path, FolderState>` — per-folder
  buffers (`paths`, `openFiles`, `leftTabs`, `rightTabs`,
  `leftActive`, `rightActive`, `hasSplit`, `focusedSide`,
  `previewModes`).

Existing accessors (`workspace.openFiles`, `workspace.leftTabs`,
…) become getters that route through the active folder's
state, so call sites in `Editor.svelte`, `EditorPane.svelte`,
`EditorTabs.svelte`, etc. don't change. Mutators (assignments
to those accessors) write through to the active folder's
state.

`workspace.workspace` stays as the new shape's plain object.
Components that previously read `workspace.workspace.root` /
`.name` reach for a new `workspace.activeFolder` accessor
(`WorkspaceFolder | null`).

### Components

- **`FolderBars.svelte`** (new) — renders the stacked bars.
  Per-bar: chevron, label, indicator slot (empty in 2.5),
  `×` on hover. `+ Add folder` row at the bottom.
- **`Sidebar.svelte`** — drops the single-folder header in
  favour of `FolderBars`. The active folder's tree mounts
  underneath the active bar (the existing `FileTree.svelte`
  is reused — it already keys off `workspace.paths`, which
  becomes per-folder).
- **`Welcome.svelte`** — unchanged user-facing copy; the
  "Open folder" button now goes through the same
  add-folder code path.
- **`StatusBar.svelte`** — the `host` / `root` slots read from
  `workspace.activeFolder` instead of `workspace.workspace`.

### Tree state across switches

Tree is **re-mounted** when the active folder changes
(simplest implementation; expanded-folder state and scroll
position are not preserved across switches). Tab state (the
user-visible thing) is preserved by the `folderStates` map.
Preserving tree state across switches is a follow-up if it
turns out to bite — purely an internal change.

## Persistence

`app_state.json` carries the new `WorkspaceSession` shape.
Restore order on launch:

1. Load `AppState` from disk.
2. Reconstruct workspace: each `FolderSession.folder_path` is
   re-opened (call `workspace_open_local` per folder, in order).
3. Set the active folder from `active_folder_path`.
4. Restore each folder's tabs from its `FolderSession`.
5. Drop folders whose path no longer exists; the cleaned-up
   state is re-saved.

## What deliberately doesn't ship

- Multi-tree view (showing more than one folder's tree at
  once). Single active folder only; clicking a bar swaps.
  Phase 7 if it lands.
- Cross-folder search. `Ctrl+P` and `Ctrl+Shift+F` scope to
  the active folder.
- Drag-to-reorder folder bars. Insertion order only for now.
- Folder rename / display name override. Path basename is the
  label.
- Compose indicators on folder bars. Reserved slot only;
  populated by the Phase 2 container redesign.
- Multiple workspaces. One workspace (`"default"`) with N
  folders. User-named multi-workspace lands in Phase 7.5
  (per-workspace `session.json`) and Phase 7.6
  (`workspace_create` / `_delete` IPC); windowed UX in Phase
  7.7+.

## Suggested commit chain

1. **Roadmap docs** (this file + `roadmap.md` + Phase 7 trim +
   cross-link in phase-02-containers.md).
2. **Backend + IPC + protocol mirror end-to-end** — every
   layer changes for the same reason; one cohesive commit that
   leaves the system green even though the UI is still
   single-folder visually.
3. **Frontend `WorkspaceState` per-folder refactor** —
   internal map + active pointer; existing accessors
   preserved; no UI change yet.
4. **`FolderBars` UI + Sidebar wiring** — the user-visible
   multi-folder UX (add, switch, remove with confirm).
5. **Polish + edge cases + test plan** — empty-workspace
   fallback, duplicate add, removing the active folder,
   stale sessions on launch.

## Test plan

`specs/test-plans/0010-multi-folder-workspace.md` (TBD at
implementation start). The numbering picks up where the
Slack run left off (0008–0009).
