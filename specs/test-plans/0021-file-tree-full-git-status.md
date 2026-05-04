# Test plan 0021: file tree full git status

- **Date**: 2026-05-03
- **Phase**: Phase 5 (Git layer)

## What shipped

- File tree rows now surface the full add / modify / delete /
  untracked / ignored vocabulary via Pierre's `gitStatus` API, not
  just ignored.
- Backend classifier reads `git status --porcelain=v1 -z --ignored
--untracked-files=all --no-renames` inside a git repo; the
  walker fallback still covers non-repo folders with ignored-only.
- Deleted files are merged back into the tree's path list so a
  worktree deletion persists as a strikethrough ghost row until
  the commit lands, per the roadmap's Phase 5 contract.
- Ignored entries flow through Pierre's `setGitStatus` so its
  built-in `[data-item-git-status='ignored']` fade applies to the
  whole row (icon, filename, git lane) without us having to
  recreate it. A small shadow-DOM overlay stylesheet hides the
  descendant-change dot on ancestor folders whose only descendants
  are ignored (otherwise `front/` would light up just because
  `front/node_modules/` exists), and tints each real-change folder
  dot by the worst tracked descendant status (`deleted > modified
  > added > untracked`).
- Live refresh via a `notify` watcher rooted at the active folder:
  500ms-debounced `fs:changed` events re-trigger the full
  enumerate + classify pass. Window-focus events are a second-
  class fallback (NFS / SSHFS / inotify exhaustion); a "Refresh
  File Tree" palette command is the manual escape hatch.

## How to test

Prerequisites: `bun install`, `git` on PATH, a git-tracked folder
with at least one commit.

1. `bun run tauri dev`, open a git-tracked folder.
2. Edit a tracked file. Expected: its row gets the modified marker.
3. Create a new file, don't `git add` it. Expected: untracked marker.
4. `git add` a new file. Expected: added marker (distinct from
   untracked).
5. `rm` a tracked file without `git rm`. Expected: the row stays
   visible with a deleted marker / strikethrough. Revert with
   `git checkout HEAD -- <path>`; the ghost row disappears on next
   refresh.
6. Confirm `.gitignore`-matching files (e.g. `target/`,
   `node_modules/`) still fade. Confirm that their ancestor
   folders do _not_ show a change dot just for containing ignored
   content — only folders with real edits beneath them do.
7. In a folder with mixed modifications and ignored descendants
   (e.g. a modified `src/foo.rs` alongside `target/`), confirm the
   ancestor dot is tinted by the worst _non-ignored_ status
   (modified-teal here). Delete a tracked file and the same
   ancestor dot flips to the deleted-red tint on next refresh.
8. `git add -f` a file matching a `.gitignore` rule. Expected: no
   fade — the tracked file is clean in git's view.
9. Open a folder that isn't a git repo. Expected: only ignored
   entries fade (walker fallback); no markers for add / modify etc.
10. Delete a tracked file (ghost row appears), then run
    `git checkout HEAD -- <path>` in the integrated terminal.
    Expected: within ~500ms the ghost row disappears and the
    restored file reappears — the fs watcher picks up the
    write without any focus change.
11. `touch new.txt` in an external terminal, alt-tab back.
    Expected: untracked row appears via either the fs watcher
    (likely) or the focus listener (fallback, e.g. on NFS).
12. "Refresh File Tree" palette command: same scenario as (10),
    but on a folder where the watcher failed to attach (the
    `tracing` log shows a warn line at startup). The command
    should still refresh the tree.

## What must keep working

- Pierre row selection, keyboard navigation, search, delete flow
  (Delete / Shift+Delete), double-click-to-open.
- `paths` reload after Save As still lands on the new row.
- Ignored-only fallback on non-git folders.

## Known limitations

- Refresh is a full re-walk, not incremental. Adequate for our
  folder sizes; a diff-based update is a later optimisation if a
  user repo makes it visible.
- Watching very large trees may exhaust inotify (default 8192
  watches). When that happens the watcher logs a warn and the
  window-focus / palette paths become the only refresh triggers.
- No watcher on remote hosts (NFS / SSHFS / Docker bind mounts):
  their kernels don't report changes to inotify. Same fallback.
- Renames render as `deleted(old) + added(new)` — intentional per
  roadmap, but means a pure `git mv` touches two rows instead of
  one.
- No conflict marker. Unmerged paths silently fall through the
  porcelain mapper until we have an SCM panel to surface them.
- Parsing drops `C` (copy) records and any porcelain byte
  combination we didn't explicitly map. That's the safe default
  (no fake marker); the alternative is rendering noise.

## Related

- Specs: `specs/roadmap.md` (Phase 5), `specs/frontend.md`.
- Prior test plans: `0020-file-tree-gitignore-fade.md`.
