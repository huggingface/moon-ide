# Test plan 0045: Ctrl+L "add selection to coder" gesture

- **Date**: 2026-05-05
- **Phase**: 6.3 follow-up

## What shipped

- Editor selections feed a workspace-level `activeSelection`
  snapshot (path + 1-based inclusive line range + captured
  text). Snapshot lives in `WorkspaceState`, updated by
  `Editor.svelte`'s existing selectionSet listener, cleared on
  tab switch / unmount.
- A floating `Ctrl+L Add selection to Coder` pill appears in
  the editor pane's top-right corner while a non-empty
  selection exists for the pane's file. Pointer-events
  disabled — the gesture is keyboard-only.
- `Ctrl+L` rebound:
  - With a selection: insert an inline `@path:start-end`
    token at the textarea's caret, add a chip (deduped by
    `(path, range)`), open the coder panel, focus the
    composer.
  - Without a selection: toggle the coder panel.
- Composer renders attached selections as chips above the
  textarea (`[doc-icon] basename:start-end [×]`). Clicking
  the body navigates to the captured range via
  `workspace.jumpTo`; `×` drops the chip _and_ scrubs every
  matching `@token` (with at most one trailing whitespace)
  from the draft so the chip and the inline references
  stay in sync.
- Wire shape mirrors Cursor: the user's prose ships intact
  with the `@`-tokens inline; resolved snippet contents land
  in a trailing `<context>\n<code_selection path lines>...
</code_selection>\n</context>` block. The wrapper element
  is the delimiter, no fencing — triple-backticks inside a
  snippet ride through verbatim.
- Slack's `Ctrl+L` binding dropped. Chat panel still reachable
  via the status-bar pip, the speech-bubble swap icon on the
  coder header, and the `Chat: Show Panel` palette entry.

## How to test

Prerequisites: `bun install`, `bun run dev`, signed in to
Hugging Face (per test plan 0039), an active workspace folder
with a tab-indented source file open.

### Selection hint

1. Open any text file in the editor. Click into it without
   selecting anything.

   Expected: no floating pill in the top-right corner.

2. Drag-select a few lines.

   Expected: the pill `Ctrl+L Add selection to Coder` appears
   in the top-right of the editor body, with the `Ctrl+L`
   keys rendered as a `<kbd>` chip. It does not block
   editor scrolling — try scrolling, the pill stays put;
   clicking on it does nothing (pointer-events disabled).

3. Click somewhere to collapse the selection.

   Expected: pill disappears within one CodeMirror update
   tick.

4. Drag-select again, then switch to a different tab.

   Expected: pill disappears (the snapshot is cleared on tab
   swap).

### Ctrl+L attach

5. Drag-select lines 89-101 of `crates/moon-coder/src/runner.rs`
   (or any file). Press Ctrl+L.

   Expected:
   - Coder panel opens (right slot mounts coder).
   - One chip appears above the textarea:
     `runner.rs:89-101` with a small document glyph and an `×`.
   - The composer textarea contains the inline token
     `@crates/moon-coder/src/runner.rs:89-101 ` (with
     trailing space). Focus is in the composer; the caret
     sits _after_ the trailing space so the next keystroke
     starts a new word.
   - Placeholder reads `Ask about the attached selection…`.

6. Without sending, press Ctrl+L again on the same selection.

   Expected: no second chip (dedupe by `(path, range)`), but
   a second `@…:89-101 ` token appears in the textarea. The
   user can reference the same selection at multiple spots
   in their prose, just like Cursor.

7. Drag-select a different range (lines 50-55) and press
   Ctrl+L.

   Expected: a second chip appears alongside the first; a
   `@…:50-55 ` token is inserted at the current caret
   position.

8. Click between the two `@`-tokens in the textarea and type
   `compare ` then move the caret to the end and type ` and
what's the trade-off?`. Press Enter.

   Expected:
   - The user-message bubble in the transcript shows the
     prose verbatim with the `@`-tokens inline, then a
     blank line, then `<context>` wrapping two
     `<code_selection path="…/runner.rs" lines="…">…
</code_selection>` elements. The model receives the
     same string.
   - Both chips clear from the composer; the textarea
     empties.
   - The agent's reply references the attached code by
     line number (it should — the wire shape matches the
     one Cursor's composer emits, which is what reasoning
     models are tuned on for this gesture).

### Click-through navigation

9. Attach a selection from `runner.rs:89-101`. Without
   sending, switch to a different tab.

10. Click the chip body in the composer.

    Expected: focus jumps back to the editor pane, the
    `runner.rs` tab activates, and the caret lands at line 89. Alt+Left returns to wherever you were.

### × drop

11. Attach 2-3 selections. Click `×` on the middle one.

    Expected: only that chip drops; the others stay. The
    matching `@token` is also stripped from the textarea (and
    every other instance of it, in case the user inserted the
    same token at multiple spots). Surrounding prose stays
    intact apart from the deleted token + at most one trailing
    whitespace char.

### No-selection Ctrl+L

12. Click into the composer (or anywhere) so the editor has
    no selection. Press Ctrl+L.

    Expected: panel toggles open / closed. No chip is added.

13. Press Ctrl+L while the panel is open + no selection.

    Expected: panel closes. No spurious side-effects in the
    composer.

### Snippets containing `<` or `"`

14. Open a file with `<` or `"` in the bytes you select
    (e.g. an HTML / TSX file or a Rust string literal
    containing `"`). Press Ctrl+L, type `summarise this`,
    send.

    Expected: the `<code_selection>` element's `path` /
    `lines` attribute values are XML-attribute-escaped (the
    formatter calls `escapeXmlAttr`); the snippet body is
    NOT escaped (it's element text content, not an
    attribute, and round-tripping through escape would mangle
    the agent's view of the code). The model receives a
    well-formed wrapper with the snippet body verbatim.

15. Snippet content containing triple-backticks rides through
    untouched — the wrapper element is the delimiter, no
    fence-width logic in play. Verify by attaching a markdown
    fragment with its own ` ``` ` blocks and confirming
    the user-message bubble still renders one outer code
    selection.

### Slack still reachable

16. Press Ctrl+L from anywhere — verify it goes to coder.

17. Open the Chat panel via the status-bar `chat` pip.

    Expected: chat panel mounts; Ctrl+L still goes to coder
    (would swap to coder if pressed while chat is mounted).

18. Open command palette (Ctrl+Shift+P), type `chat`.

    Expected: `Chat: Show/Hide Panel` is listed without a
    `Ctrl+L` shortcut chip.

## What must keep working

- Existing selection-driven behaviours (LSP go-to-definition,
  nav history `Alt+Left` / `Alt+Right`, click-to-select). The
  selection listener now does an extra
  `workspace.setActiveSelection(...)` call but doesn't mutate
  CodeMirror state, so the LSP / nav paths see no diff.
- Panel hydration on startup (test plan 0043) still re-opens
  the previous session.
- All 25 `cargo test -p moon-coder` tests still pass.
- The slack `setPanelVisible` palette wrapper (used by the
  `Chat: Connect Slack…` entry) keeps working — it routes
  through `rightPanel.set('chat')` like before.

## Known limitations

- The pill is a static top-right corner indicator, not a
  floating tooltip near the cursor. Cursor's "ghost button
  above the selection" is nicer; ours is good enough as a
  reminder and far simpler. If anybody asks, swap to a
  CodeMirror-anchored tooltip later.
- Attachment rendering is a flat chip strip above the
  textarea. No grouping by file, no max-count cap. Send
  overflow today is the model's context window — when 6.6
  surfaces a token-usage ring around `+`, the chip strip
  becomes a more useful tell.
- Re-attach after edit: if the user edits the file after
  attaching, the chip still carries the _old_ text. Refresh
  workflow is "drop the chip, re-select, Ctrl+L". This is
  intentional (matches Cursor): we don't want a follow-up
  formatter pass to silently change what the agent saw.
- No multi-file attach via the file tree yet (drag-and-drop
  a `.rs` onto the composer to attach the whole file). Add
  when somebody asks; the `ComposerAttachment` shape already
  generalises (just set `startLine: 1` / `endLine:
total_lines`).

## Related

- Specs: [`specs/coder.md`](../coder.md#composer-attachments).
- Prior test plans:
  [0044-coder-polish.md](./0044-coder-polish.md),
  [0043-coder-sessions.md](./0043-coder-sessions.md),
  [0042-coder-streaming.md](./0042-coder-streaming.md).
