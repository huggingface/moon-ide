# Test plan 0023: editor QoL — auto-close brackets + completion scaffold

- **Date**: 2026-05-03
- **Phase**: Phase 4 polish (editor)

## What shipped

- Auto-close brackets and quotes in the main editor: typing `(`,
  `[`, `{`, `"`, `'`, or a backtick inserts the matching closer
  and parks the caret between them. Backspace on an empty pair
  deletes both sides.
- `@codemirror/autocomplete` is wired but stays silent until a
  real source is registered (activates only on Ctrl-Space). Sets
  up the surface for the future LSP client without leaking a
  buffer-identifier popover today.
- `specs/frontend.md` gets an editor-stack section that pins the
  layering rule: CodeMirror for every editable buffer, Pierre
  Diffs for read-only review, `@codemirror/merge` for editable
  merge-conflict resolution (deferred until SCM lands).

## How to test

Prerequisites: `bun install`, `bun run tauri dev`.

1. Open any editable text file (`.ts`, `.rs`, `.md`).
2. Type `function foo(`. Expected: editor now reads
   `function foo()` with the caret between the parens.
3. Continue typing `a, b, c`. Expected: `foo(a, b, c)`. Hit `)`.
   Expected: the caret steps over the existing closer — no
   duplicated `))`.
4. Inside a JS/TS file, type `const arr = [`. Expected: `[]`,
   caret inside. Same for `{`.
5. Type a double-quote. Expected: `""`, caret inside. Type
   another double-quote. Expected: skip over the closer.
6. Position the caret between an empty `()`; hit Backspace.
   Expected: both parens vanish in one keypress. Repeat with
   `[]`, `{}`, `""`.
7. Non-empty guard: type `[1, 2, 3]`, place caret between `[`
   and `1`, hit Backspace. Expected: only `[` is deleted (no
   matched-closer auto-kill).
8. Press Ctrl-Space on a word boundary. Expected: a completion
   popover opens (empty or showing buffer-local words depending
   on file type) with the moon palette; Escape dismisses it.
   Typing normally does **not** open the popover on its own.
9. With a selection, type `(`. Expected: the selection is
   wrapped in `()` (default close-brackets behaviour).
10. Regression sweep: tab insertion, indent-on-input, search
    panel (Ctrl-F), bracket-matching highlight, selection-match
    highlight all still behave as before.

## What must keep working

- Editorconfig-driven indent + tab size.
- Theme flipping (light/dark) still repaints the editor chrome
  and syntax highlight.
- `indentWithTab` still wins over any default autocomplete Tab
  binding — verify by pressing Tab on an empty line (inserts
  indent, does not intercept a non-existent completion).

## Known limitations

- No language-specific autocomplete source yet. The popover is
  plumbing for the upcoming LSP client and a future snippet
  source; stand-alone it's intentionally quiet.
- No rainbow brackets. A good implementation needs a Lezer-aware
  scan that skips strings and comments; will land as a focused
  standalone extension when someone actually wants it.
- No folding gutter, no rectangular selection. Deferred until
  requested.
- Conflict editor (`@codemirror/merge`) not wired. Shows up
  alongside the SCM "resolve conflicts" flow.

## Related

- Specs: `specs/frontend.md` (editor section, new diff/conflict
  subsection).
- ADRs: `specs/decisions/0001-stack.md` (CM6 + Pierre Diffs
  baseline).
- Prior test plans: `0022-discard-file-changes.md`.
