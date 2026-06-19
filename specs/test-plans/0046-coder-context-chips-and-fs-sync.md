# Test plan 0046: Hide `<context>` block in chat, sync open buffers on external mutation

- **Date**: 2026-05-05
- **Phase**: Phase 6 (Coder)

## What shipped

Two cross-cutting QoL items, both surfaced by everyday use of the AI coder:

1. **`<context>` block stays out of the chat UI.** The composer's `Ctrl+L` attachments produce a trailing `<context>\n<code_selection path="P" lines="A-B">…</code_selection>\n</context>` block on the outgoing prompt (see `renderPromptWithAttachments` in `coder.svelte.ts`). The model needs that XML; the user does not. Previously the user-message bubble rendered the full prompt verbatim, so attached selections appeared as a wall of XML below the prose. Now `CoderPanel.svelte` parses the trailing context block out at render time and replaces it with a strip of clickable file-reference chips below the bubble. Click → opens the file at the captured starting line via `workspace.jumpTo` (same nav-history-aware path as Ctrl/Cmd-click goto-def). Chips intentionally **don't** show the snapshot text — the agent likely just edited the file, so the chip is "navigate to the spot I referenced" not "show me what I sent".
   - Empty-prose + attachments-only sends ("explain this") render only the chip strip with no prose bubble.
   - Parse failure (model echoing `<context>` in an answer, partial buffer mid-stream, malformed selection) falls through to rendering the raw text — the parser anchors at `$` and only fires when the closing `</context>` is the last thing in the prompt.
   - Inline `@path:start-end` tokens in the prose are still rendered as plain text. Linkifying those is a follow-up if anyone asks.

2. **Open buffers re-sync from disk on external mutation.** When the chat agent's `write_file` / `edit_file` tools (or any external process: integrated terminal, formatter, `git checkout`) modify a file that's currently open and **clean**, the file tree was correctly flipping the row to `(M)` (because `git status` looks at disk), but the editor's git-change gutter, blame, and LSP all kept showing the pre-edit content — the user had to close + reopen the tab to see the new state. Root cause: `refreshGitStatus` re-fetched the HEAD content for open buffers but never re-read the working-tree content. Now the same loop also calls `reloadOpenFileFromDisk(path)` for any clean buffer, which re-reads the disk content and replaces the in-memory `OpenFile` if it differs. Dirty buffers are skipped — silently clobbering unsaved edits would be far worse than a stale gutter. `reloadOpenFileFromDisk` short-circuits when the on-disk text already matches the buffer (we just wrote it ourselves; the watcher is firing about our own save), so the reactive graph doesn't churn on every `fs:changed` debounced burst. The same reload branch also kicks a debounced `scheduleBlameRefresh(path)` so the inline blame re-attributes to the new on-disk content; without it the gutter and buffer updated but the blame widget kept the pre-mutation authorship. A `.git/HEAD` move with no working-tree change (a branch switch where the new branch's bytes match) re-blames every open buffer that wasn't itself in the changed subset — handled separately so a content-changing switch (which fires both the `.git/HEAD` event and the file's own working-tree event) never blames the same buffer twice in one pass.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, signed in to the Coder, a workspace folder bound that's a git repo with at least one commit.

### Chat: context chips render

1. Open any text file. Select a few lines, press `Ctrl+L`. The Coder panel opens with a composer chip for the selection.
2. Type a question like `Refactor this for clarity.` and send.
3. The user-message row shows:
   - The prose bubble: just `Refactor this for clarity.` (no `<context>`, no `<code_selection>`, no XML).
   - Below the bubble: a single chip with the file basename and the `:start-end` range. Hovering shows the full path and range as a tooltip.
4. Click the chip. The file opens (or activates, if already open) and the caret jumps to the captured starting line. `Alt+Left` returns to wherever the user was — the click went through `workspace.jumpTo` which records nav history.
5. Attach **two** selections via `Ctrl+L` from the same file at different line ranges, send. Two chips render. Each opens at its own start line.
6. Send with an attachment but **no** typed prose. The user-message row renders only the chip(s), no empty bubble — the strip lives directly under the `you` label.
7. Reload the session (back to the sessions list, click in again). The same parsing applies on session-restore — historic user messages with context blocks render as chips, not as XML walls.
8. Send a message that legitimately contains the literal string `<context>` somewhere in the prose (e.g. `What does <context> mean in this prompt?`). The parser only matches a closing `</context>` at the very end of the prompt with the wrapper shape, so a literal mid-prose `<context>` survives in the bubble.

### Editor sync on external mutation

1. Open a clean tracked file in the editor (no unsaved edits, tree row not `(M)`).
2. From an external terminal, `echo '// hello world' >> <path>`. Within ~500ms (fs-watcher debounce):
   - Tree row flips to `(M)`.
   - Editor's git-change gutter shows the new line as `added`.
   - Inline blame on the modified line updates (shows "Not Committed Yet" or whatever `git blame` reports for working-tree edits).
   - The CodeMirror buffer reflects the new content (you can scroll to it / search for the new line).
3. Repeat with the **chat agent** as the editor. In the Coder panel, send `Append "// hello world" to <path>`. Watch the same three signals update without closing the tab.
4. Make a manual edit in the editor (without saving) — the buffer is now **dirty**. From an external terminal, modify the same file. The dirty buffer is **not** reloaded; the user's unsaved edits survive. The tree still reads `(M)` for the on-disk state — that's expected (git doesn't know about the in-memory buffer). No flash, no warning yet (follow-up if anyone hits the dirty-conflict case in practice).
5. Save your edits. Now the buffer is clean again. Trigger an external modification. The clean buffer reloads.
6. Open multiple files. Modify one externally. Only that one's buffer reloads; the others' OpenFile references are unchanged (no reactive churn for unaffected tabs).
7. Modify a file via `saveActive` from inside moon-ide. The fs-watcher fires, `refreshGitStatus` runs, the buffer's "reload from disk" path no-ops because on-disk and in-memory are already equal. The editor's caret position is preserved (no spurious dispatch).
8. Open a tracked file, note the inline blame on a few lines. From a terminal, `git switch` to a branch where that file differs. Within ~500ms the buffer reloads to the new content _and_ the inline blame re-attributes to the new branch's commits (one `git blame` per touched buffer, not two — the `.git/HEAD` event and the file's working-tree event coalesce). Switch to a branch where the file is byte-identical but has different line history: the buffer doesn't reload (no change) but the blame still refreshes off the `.git/HEAD` signal.

### Diff view stays coherent

1. Open a file in diff mode (`Ctrl+Shift+D` or click a gutter marker). Externally modify the file via the chat agent. The diff view's right side updates in place (it consumes the same `OpenFile.text`); HEAD on the left side is unchanged.

## What must keep working

- All Phase-6 tests: streaming (0042), sessions (0043), polish (0044), `Ctrl+L` add-to-chat (0045). The composer chip strip itself is unchanged — only the user-message row's rendering of the _sent_ prompt changed.
- Phase-5 git surfaces: tree markers (0020/0021), discard (0022/0037), inline blame (0029), git-change gutter (0033), single-tab diff toggle (0036). All driven by the same `refreshGitStatus` that now also pulls fresh disk content.
- `fs:changed` window-focus path still works; the reload runs from `refreshGitStatus`, which both triggers go through.
- `bun run check`, `bun run lint`, `cargo check`, `cargo clippy --all-targets -- -D warnings` clean.

## Known limitations

- The chip strip ignores the inline `@path:start-end` tokens inside the prose. They render as plain text. We could linkify them (regex-match in the prose, render as inline anchors) — punted because nobody's asked yet, and the chip strip already covers the same files.
- A clean-but-stale buffer reload preserves the caret only if CodeMirror considers the new doc similar enough; for a wholesale rewrite the caret may snap to position 0. Acceptable: this is the chat agent rewriting the file under us, the user's prior caret is unlikely to still be meaningful.
- Dirty-vs-external-edit conflict has no UI. The user keeps their edits; the disk content stays as the agent wrote it. Saving the buffer would clobber the agent's edits. A "your buffer differs from disk — reload? overwrite?" prompt is the obvious next step but adding a modal flow for an edge case nobody's actively hit feels premature.

## Related

- `specs/test-plans/0045-coder-add-to-chat.md` — the `Ctrl+L` flow that produces context blocks.
- `specs/test-plans/0033-git-change-gutter.md` — the gutter that needed fresh disk content to render correctly.
- `specs/test-plans/0021-file-tree-full-git-status.md` — the tree flip that was correctly firing while the editor stayed stale.
- `specs/coder.md` — the prompt-shape spec for `<context>` / `<code_selection>`.
