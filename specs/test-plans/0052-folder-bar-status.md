# Test plan 0052: project-bar git change badges + container icon

- **Date**: 2026-05-06
- **Phase**: 1.5 (workspace polish). Single-file UI add plus one new IPC command; doesn't change persisted state.

## What shipped

- Each folder bar now shows compact git badges `+N ~N -N` for its working tree (added/untracked, modified, deleted). Badges hide when the count is zero. Colours mirror the file-tree palette (`--m-success` / `--m-warning` / `--m-danger`). Untracked files fold into `added` — the SCM panel inside the active folder still distinguishes the two; the project bar just signals "this folder has new files".
- The compose dot is gone; folders with a `docker-compose.yml` now show a small container glyph (`ContainerIcon.svelte`) tinted by the same state colours (`absent` muted / `creating` warning + pulse / `running` success / `paused` warning / `stopped` muted / `failed` danger). Click still toggles the per-folder compose popover.
- New Tauri command `fs_git_change_summary(folderPath)` returns `{ added, modified, deleted }` for any bound folder by going through `WorkspaceRegistry::folder_for_path` (i.e. it doesn't require the folder to be active). Returns zeros for non-repo folders / git unavailable.
- Per-folder summaries refresh on:
  - workspace adoption (startup + open/add folder + restore session),
  - every active-folder `refreshGitStatus` pass (so a watcher event in folder A also fans out and re-counts B/C/D),
  - any `coder:event` whose kind is `tool_result`, `turn_complete`, or `subagent_finished` (debounced 200ms) — covers the cross-folder "agent in A wrote to B" case where A's watcher sees nothing.
- Per-folder in-flight guard collapses bursts so a 30-`edit_file` turn doesn't stack 30 `git status` subprocesses per folder.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, two real git repos open as bound folders (moon-ide + any sibling repo). One additional non-repo folder bound for the "no-repo" case.

### Static counts

1. Active folder = `moon-ide`. From a terminal in another bound repo, edit one file and `git add` a second new file.
2. Switch focus back to moon-ide. Within ~1s the second folder's bar should show `+1 ~1` (the new file, the modified one).
3. `git rm --cached <file>` something in the second folder. Re-focus moon-ide. The folder bar should now show `... -1` for the staged delete.
4. Bind a folder that's not a git repo. Its bar must show **no** badges (zero counts cached) and never spam errors in the console.

### Cross-folder agent edits

1. Active folder = `moon-ide`. Sibling repo also bound, no working-tree changes.
2. Through the coder panel in moon-ide, run a turn whose tool calls `edit_file` against the sibling repo (e.g. via a sub-agent targeting the sibling, or a `bash` `echo … >> sibling/path`).
3. Within ~250ms after the tool result lands, the sibling folder's bar should show the corresponding badge (`+1` for a new file, `~1` for a modification). The user does **not** need to switch to the sibling folder for this to update — that's the load-bearing scenario.
4. Sub-agents: spawn a coder sub-agent that touches the parent's sibling folder. The bar updates after `subagent_finished`.

### Container icon

1. Folder with a `docker-compose.yml`, project not running. Bar shows the container glyph in muted (`--m-fg-subtle`) colour. Hover tooltip says "Services: not running".
2. Click the icon → popover opens. Bring the project up. The glyph turns green (`--m-success`).
3. While `compose up` is mid-flight, the glyph is amber and pulsing. After it settles to running (or fails), the pulse stops. After a failure, the glyph is red.
4. Folder without `docker-compose.yml`: a placeholder space remains where the icon would be (so the `×` button doesn't jitter on hover) but no icon is drawn and clicking doesn't open anything.

### Regression checks

- Removing a folder from the workspace also drops its cached summary (no stale badges if the same path is later re-bound under a clean tree).
- `fs:changed` watcher events still feed the active folder's tree; nothing is now skipped.
- `Ctrl+O` external buffers (test plan 0051) don't influence any folder's badge — they aren't part of any folder's working tree.

## Known limitations

- Counts include staged + working-tree in one number per category. The user can't tell `staged-add + clean-worktree` from `worktree-add` from the bar — that's deliberate; finer detail belongs in the SCM panel.
- Inactive folders' badges only refresh on the triggers listed above. An external `git checkout` in a terminal for a non-active folder won't reflect until the next workspace focus / coder event / fs watcher tick on the active folder. Acceptable for now; if it becomes load-bearing we'd extend the watcher to every bound folder rather than poll.
- The container icon is a single static glyph; we don't show service counts (`3/4 running`) on the bar. The popover already covers that.
