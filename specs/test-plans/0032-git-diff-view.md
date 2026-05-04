# Test plan 0032: Git diff view

- **Date**: 2026-05-04
- **Phase**: Phase 5 (Git)

## What shipped

- New Tauri command `fs_git_head_content` shells out to `git show HEAD:<path>` and returns the committed text (or `null` when the path isn't in `HEAD` / not in a repo / binary).
- New `DiffView.svelte` component wraps `@pierre/diffs`' `FileDiff` in split layout, themed with `pierre-dark` / `pierre-light` and flipping on the IDE's effective theme.
- Per-buffer toggle (`workspace.diffModes` map + `toggleDiffMode` / `setDiffMode` helpers) replaces the editor with the diff view for the active tab; markdown preview is suppressed while diff mode is on.
- Deleted rows become tabs: opening a `deleted` row loads `HEAD` content into the buffer (`loadDeletedFile` / `isDeleted` flag) and forces diff mode unconditionally. Session restore picks up the same routing so a deleted tab survives a restart.
- Context menu entry **"View diff"** on modified and deleted file rows (`FileTree.svelte`), plus palette command **Git: View Diff / Exit Diff View** on the active tab.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a folder bound that is a git repo with something committed (moon-ide itself works).

### Happy path — modified file

1. In a tracked file, change a few lines. The file tree row lights up with the modified marker.
2. Right-click the row. Menu shows **"View diff"** above **"Discard changes"** and **"Copy path"**. Select **View diff**.
3. The editor pane swaps to a split diff view. Left side = `HEAD`, right side = working tree. Changed lines are highlighted with bar indicators; unchanged regions collapse behind hunk separators. Syntax highlighting is consistent with the rest of the IDE (light/dark follows the active theme).
4. Tab click returns focus to the tab strip. The tab title is unchanged; only the pane contents are different.
5. Command palette (`Ctrl+P` then `>` or the dedicated palette bind): search **"Git: Exit Diff View"**. The editor comes back with caret at the first line.
6. Toggle again from the palette: **"Git: View Diff"**. The diff view reappears.

### Happy path — deleted file

1. `rm path/to/a/tracked/file` in an external terminal, or use the file tree's **Discard changes** on a tracked row and pick any _other_ tracked row first to force the "delete" shape via an untracked fallback path (or just `git rm --cached path`).
2. The row stays in the tree with a `deleted` marker. Click it. Expected: a new tab opens, the pane renders the diff view automatically — left side shows the `HEAD` content, right side shows nothing. Every line appears as a deletion.
3. The **Git: View Diff / Exit Diff View** palette entry is visible but a no-op on a deleted tab — there's no editor state to flip to. Palette label reads **"Git: Exit Diff View"** for a deleted tab.
4. Close the tab and reopen the deleted row. Same diff view appears. Close + restart the IDE with the tab left open: after session restore, the deleted tab pops back up in diff view (the `isDeleted` flag is persisted via `text`-replay + fallback detection in `loadTextFile`).
5. Discard the deletion from the file tree's context menu (**"Discard changes"** → un-delete). The next git-status refresh removes the row from the tree and the open tab's pane auto-reloads the restored working-tree text (normal editor view).

### Context-menu surfacing

1. Open the tree. Right-click:
   - A **clean** tracked file → no **View diff** entry (nothing to compare).
   - A **modified** file → **View diff** shows.
   - A **deleted** file → **View diff** shows.
   - An **untracked** file → no **View diff** entry (no `HEAD` side to compare against).
   - An **added** file → no **View diff** entry (same reason).
   - An **ignored** file → no **View diff** entry.
   - A **folder** row → no **View diff** entry. We don't fan folder-scoped diffs out yet; that's a later slice.

### Palette visibility

1. Focus the editor on a clean tab. Open palette, search **"Git"**. **Git: View Diff** is not listed.
2. Edit the file (flip it modified). Palette now lists **Git: View Diff**. Run it. Label flips to **Git: Exit Diff View**.
3. Focus a deleted tab. Palette lists **Git: Exit Diff View** as an informational entry; invoking it is a no-op (stays in diff mode).

### Theme + render correctness

1. Switch the IDE theme between dark and light (status bar theme picker). The diff view repaints with the matching `pierre-dark` / `pierre-light` Shiki theme without a remount. Syntax colours and background swap cleanly.
2. Open a large modified file (500+ lines; `specs/roadmap.md` with a handful of edits is a good stress target). The initial diff paints under ~500 ms on a warm repo. Scroll through: no obvious frame drops.
3. Expand unchanged hunks via the hunk separator controls. The expanded context renders with correct line numbers on both sides.

### Degradation

1. Bind a folder that is **not** a git repo. No rows carry a git status anyway; right-click surfaces no **View diff**. Palette entry hidden.
2. Rename `git` temporarily out of `PATH` (or bind a fresh unstaged repo with no commits). For any modified-looking file, the palette entry stays hidden if git-status couldn't classify it, and running the toggle programmatically shows an empty-vs-empty diff rather than crashing.
3. Open a very large binary that happens to be tracked. `git_head_content` detects the binary via the same `looks_binary` heuristic used by `read_file`, returns `null`; the diff view renders `HEAD` as empty — a known minor wart, but it doesn't crash or stall.

## What must keep working

- All prior git tree markers (`0020`), refresh-on-fs-event (`0021`), discard-changes menu (`0022`), and inline blame (`0029`).
- LSP diagnostics / hover / goto-def (`0024` / `0027` / `0030` / `0031`) on normal editor tabs.
- Markdown preview toggle (`0004`) on tabs that are _not_ in diff mode.
- Tab / split behaviour: closing a diff tab, switching tabs, dragging tabs between panes all behave the same as an editor tab.
- Session restore: markdown preview state, open tabs, active tab, cursor position (for editor tabs) all still survive a restart.

## Known limitations

- Diff view is read-only. Editing inside the diff (for merge-conflict resolution or partial apply) will come later; the underlying `@pierre/diffs` library supports it but we haven't wired up the accept/reject UI yet.
- The "before" side is always `HEAD`. No working-tree-vs-index, no `--staged`, no compare-against-branch. Covers 90 % of "what did I change?" but the SCM panel will grow these.
- Added / untracked files don't get a diff view — the UX would be an empty-vs-content splash that isn't useful beyond the normal editor. We revisit if someone asks.
- Folder-level diff (right-click a folder → show every diff under it) is not offered. Matches `@pierre/diffs` usage here (one file per component); a multi-file diff view belongs in the SCM panel.
- The diff view's header bar is suppressed (we already have the tab strip). No "jump to next hunk" keybinding; Pierre's hunk separator buttons are the current in-pane nav.
- Binary files in `HEAD` collapse to an empty before-side. A better UX ("binary file changed; no diff available") arrives once we wire a real binary detection path into the view layer — for now, `git_head_content` returns `null` and the working-tree side still paints.
- No persistence for the editor/diff toggle. Diff view is a transient "show me what changed" gesture; session restore lands every modified tab back in editor mode. Deleted tabs are the exception — those are held in diff by the `isDeleted` flag which _is_ reconstructed on open.

## Related

- Specs: `specs/roadmap.md` — Phase 5 section calls out `@pierre/diffs` as the diff-view engine.
- Prior test plans: `0020-*.md` / `0021-*.md` / `0022-*.md` / `0029-*.md` for the git status + discard + blame machinery this diff view reads on top of.
- Dependency: [`@pierre/diffs` docs](https://diffs.com/docs).
