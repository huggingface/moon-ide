# Test plan 0070: Goto-definition pending jumps reach the diff view

- **Date**: 2026-05-07
- **Phase**: Phase 5 (Git) — bugfix follow-up to 0036

## What shipped

- The diff view's right-hand pane now consumes `workspace.pendingJumps` so a goto-definition (Ctrl/Cmd-click, palette "Go to Definition", coder navigation, Alt+Left/Right history, anything routed through `workspace.jumpTo`) targeting a buffer that happens to be open in diff mode lands the caret on the LSP-returned `(line, character)` instead of leaving it at the clicked symbol.
- The conversion helper `offsetForLspPosition` was hoisted from a private function in `Editor.svelte` to an exported helper in `src/lib/editor/lsp.ts`. Both `Editor.svelte` and `DiffView.svelte` now share one implementation; the editor's local copy is gone.

### Why

Before this change only `Editor.svelte` watched `workspace.pendingJumps`. A Ctrl-click in the right pane of `DiffView` called `workspace.jumpTo(path, position, …)`, which queued the pending jump and called `openFile(path)`. For the **same-file** case (definition in the same buffer the user clicked in — typical for a goto-def on a local variable, helper function, or symbol declared higher up in the same file), `openFile` was a no-op (the buffer was already active in diff mode) and nothing else moved the right-pane caret. The user saw "Ctrl-click did nothing" / "jumped to the wrong line" because the caret stayed wherever the mousedown had placed it.

Cross-file goto-def worked because the destination file landed in editor mode (diff mode is per-path, transient, default off for newly-opened files), so `Editor.svelte`'s consumer fired.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a bound TypeScript repo. The `capfi-international` workspace is a good fit — it's where the regression was reported.

### Same-file goto-def while in diff mode

1. Open a TypeScript file with at least one modified hunk so the diff toggle is available, e.g. edit any `.ts` file in the bound folder so its tree row flips to `modified`.
2. Add a usage of a symbol declared elsewhere **in the same file** (e.g. add a call to a helper function near the top of the file). Save (`Ctrl+S`) so the LSP server is in sync.
3. Toggle into diff mode (`Ctrl+Shift+D`).
4. In the right pane, hover the helper's name with `Ctrl` held — the identifier should underline (link decoration).
5. Ctrl-click the name. The right pane's caret + viewport jumps to the helper's declaration line in the same diff view. The pane stays in diff mode (we don't auto-flip back to editor mode — diff is what the user chose).
6. Nav history: press `Alt+Left`. Caret returns to the click site (recorded via `setActive`'s file-switch entry that `jumpTo` overrides with the post-arrival position). `Alt+Right` goes forward to the declaration again. Both bounces land in the right pane while in diff mode.

### Cross-file goto-def from diff mode

1. From the same diff-mode buffer, Ctrl-click an imported symbol whose definition is in a sibling file.
2. Sibling file opens in **editor mode** (it wasn't in diff mode before), caret on the definition line. Nav history still records both endpoints; `Alt+Left` returns to the diff-mode buffer at the click site.
3. Toggle the sibling back into diff mode (`Ctrl+Shift+D`), then Ctrl-click an identifier inside _it_. Same-file jump within the sibling works in diff mode too.

### Regression check: regular editor mode

1. Flip out of diff mode (`Ctrl+Shift+D`). Ctrl-click on the same symbols. Editor-mode goto-def still works exactly as before — pending jump applies on the next frame, caret lands on the target line, nav history updates.
2. Palette → `Go to Definition` (if your platform exposes it) from a clean editor buffer. Same behavior.

### Regression check: deleted-file diff

1. Open a deleted-file tab (e.g. `git rm --cached <path>` externally and click the row in the tree). The right pane is empty + read-only. The new pending-jump consumer is gated on `!file.isDeleted` and short-circuits cleanly. No console errors, no caret dispatch into an empty doc.

### Coder navigation

1. Have the coder propose changes that include `read_file` / `edit_file` tool blocks for a file currently in diff mode. Clicking the file in a tool result block routes through `workspace.jumpTo`. The right pane scrolls to the relevant line. Pre-fix this also went to the wrong line in diff mode.

## What must keep working

- All gestures listed in plan 0036 (single-tab diff with mode toggle): tri-state toolbar, `Ctrl+Shift+D`, palette entry, file-tree context menu, gutter-marker click, edit-on-right, theme + editorconfig, HEAD refresh, deleted file flow, session restore, LSP / blame / completion / diagnostics on the right pane.
- Editor-mode goto-def + nav history (plans 0027 / 0028) — unchanged path; we only added a second consumer for `pendingJumps`.
- `bun run check`, `bun run lint`, `cargo check --workspace --exclude moon-desktop`, `cargo clippy --workspace --exclude moon-desktop --all-targets -- -D warnings` all clean.

## Known limitations

- The pending-jump consumer queues the dispatch via `queueMicrotask` to defer past any in-flight CM state-rebuild (same timing rationale as `Editor.svelte`'s consumer). If the user closes the diff tab or flips out of diff mode in the microtask window, the dispatch is a no-op on the now-orphaned `merge.b` view — harmless, but the jump itself is lost. A subsequent Ctrl-click reproduces it. Not worth a `buildToken` recheck for a window this small.
- Flipping out of diff mode after a same-file jump preserves the new caret position (single shared `OpenFile.text` buffer), but `Editor.svelte`'s pending-jump consumer also fires once on mount with an _empty_ `pendingJumps` entry (we consumed it) — net effect: caret lands where the diff view left it. Confirmed in the manual test above (`Alt+Right` after flipping modes).

## Related

- `specs/test-plans/0036-diff-view-single-tab-toggle.md` — establishes that the diff view's right pane carries the full LSP stack (hover, goto-def, completion, diagnostics) but didn't wire the pending-jump consumer.
- `specs/test-plans/0027-lsp-goto-definition-nav-history.md` — original goto-def + nav-history wiring this fix completes coverage for.
- `specs/lsp.md` — LSP architecture; the broker / position-conversion behaviour is unchanged.
