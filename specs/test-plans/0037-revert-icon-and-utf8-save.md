# Test plan 0037: Revert-from-toolbar, deleted-file label, UTF-8 save fix

- **Date**: 2026-05-04
- **Phase**: Phase 5 (Git) + cross-cutting bug

## What shipped

Three small QoL items grouped because they all hit the save / git path together:

1. **Bug fix — UTF-8 corruption on save.** `pre_save::trim_trailing_whitespace` was iterating `text.as_bytes()` and pushing each non-separator byte with `b as char`, which only round-trips ASCII. Every multi-byte UTF-8 sequence (`é`, CJK, emoji) was being widened from a single codepoint into N bogus Latin-1 codepoints — classic mojibake (`é` → `Ã©`). Rewrote the scan to slice the original `&str` between separators rather than push byte-by-byte. Newline detection (`\n`, `\r`, `\r\n`) was already correct (those are ASCII so byte-level scanning lands on UTF-8 boundaries by construction); only the accumulation was broken. Added a regression test covering Latin-1 accents, Japanese, and emoji inside trim-trailing-whitespace lines.
2. **Deleted-file context-menu label.** `Discard changes` for a `deleted` row reads as "throw away the deletion", but operationally it `git restore`s the file — i.e. brings it back. Renamed to **`Restore file`** in `FileTree.svelte`'s `discardLabel`. Behavior is unchanged (still routes through `workspace.discardPaths`); only the copy moves.
3. **Revert icon next to Source / Diff.** The tab toolbar gets a small icon button (lucide `rotate-ccw`) on its right edge whenever the active file is `modified` or `deleted`. Click → same `discardPaths` flow as the file-tree menu, including the confirm dialog (skipped for pure-undelete, which is non-destructive). Toolbar visibility logic now ORs `showViewToggle` and `canRevert`, so a deleted buffer (where the Source/Preview/Diff toggle hides because there's no editor view) still gets the revert affordance.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a bound git-repo folder with at least one commit.

### UTF-8 save round-trip

1. Open a tracked file in the workspace. Type a line like `const greeting = "café résumé 日本語 🚀";` and add some trailing spaces.
2. Save. Reload the buffer (close + reopen, or `git checkout HEAD -- <path>` is **not** what we want here — just ensure the on-disk content is what's now in the buffer).
3. Open the file in another editor (or `cat <path>` from a terminal). The content matches exactly what was typed; **no `Ã©`**, no replacement characters.
4. Repeat with a file that has both trailing whitespace and CRLF endings (e.g. `foo\r\n` lines). Save. The file remains valid UTF-8 and the line endings are preserved as configured by the editorconfig.
5. `cargo test --package moon-core pre_save::tests::trim_preserves_utf8_multibyte` passes.

### "Restore file" label

1. With a tracked file open, delete it externally (`rm <path>` in a terminal or via the file-tree's Delete). It appears in the tree with the deleted git status (red strikethrough).
2. Right-click the deleted row in the file tree. The destructive entry now reads **`Restore file`** (not `Discard changes`).
3. Click it. Pure-undelete skips the confirm dialog (intentional — `git restore` on a deleted file is non-destructive). The file reappears on disk and the row's git status clears.
4. Compare to an `untracked` row — the entry still reads `Discard (move untracked file to trash)`.
5. Compare to a `modified` row — the entry still reads `Discard changes`.
6. Folder-level entries (e.g. right-clicking `src/` while it contains a deleted file plus a modified file) still read `Discard N changes in this folder` — the per-status copy only applies to single-file rows.

### Revert icon in tab toolbar

1. Open a clean tracked file. The toolbar shows nothing on the right edge (no diff, no preview, no revert).
2. Edit it; tree row flips to `modified`. The toolbar grows a `Source` / `Diff` group **and** the revert icon to its right.
3. Hover the icon: tooltip reads `Revert file to HEAD`. Click. The standard confirm dialog appears (`title: 'Discard changes'`, ok label `Discard`). Confirm → buffer reloads to HEAD, dirty dot clears, `modified` status clears.
4. Edit the file again. Click the revert icon. Cancel the confirm. Buffer is unchanged; status stays `modified`.
5. Open a deleted file (the row still shows in the tree). The Source/Preview/Diff group is hidden (deleted is forced into diff mode), but the revert icon is **still visible**. Tooltip reads `Restore file from HEAD`. Click → confirm is skipped (pure-undelete), file reappears, the buffer reloads as a regular editor view.
6. Open an `untracked` / `added` / clean / `untitled` buffer. The icon does **not** appear. Use the file-tree menu for those (which still does the right thing per status).
7. Open a markdown file with no edits. Toolbar shows nothing (no preview yet — the toggle only appears on edit, same as `Diff`). Edit it to dirty it; toolbar appears with `Source` / `Preview` / `Diff` plus the revert icon.

### Style + interaction polish

1. The revert icon's hover/active states match the text buttons next to it (same `--m-bg-overlay` / `--m-bg-3`).
2. Tab strip width is unaffected when the icon shows up; the strip's flex layout keeps the toggle pinned to the right.
3. Click drag-and-drop in the tab strip is unaffected by the new button.
4. Keyboard: `Tab` from the last tab focuses the Source button → Diff → revert icon, in order. Pressing Enter on the icon triggers revert (matches `<button>` defaults).

## What must keep working

- All prior phase-5 gestures: tree markers (0020), refresh on fs events (0021), discard (0022), inline blame + PR linking (0029), git-change gutter + overview ruler (0033), single-tab diff toggle (0036).
- Save round-trip on plain ASCII files is byte-identical (no spurious whitespace, no LF/CRLF flips beyond what editorconfig dictates).
- `bun run check`, `bun run lint`, `cargo check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --package moon-core` all clean.

## Known limitations

- The revert icon hides for `untracked` files. Reverting an untracked file means trashing it — the file-tree menu's `Discard (move untracked file to trash)` already covers that, and we'd rather not put a destructive trash action behind a small icon next to harmless view-toggle buttons.
- The revert icon's confirm dialog reuses the existing copy. For deleted-only it skips the dialog (consistent with the file-tree menu). Custom toolbar-specific copy could be argued for, but the dialog text already adapts to the file count and status mix.
- The icon is inline SVG (lucide `rotate-ccw`) rather than from a shared icon set because we don't have one yet. When we adopt a project-wide icon library, this is one of the first switches.

## Related

- `specs/test-plans/0022-discard-file-changes.md` — the file-tree discard flow this hooks into.
- `specs/test-plans/0036-diff-view-single-tab-toggle.md` — the toolbar this icon attaches to.
- `specs/editorconfig.md` — the pre-save pipeline `trim_trailing_whitespace` lives in.
