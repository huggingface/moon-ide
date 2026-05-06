# Test plan 0056: JSONL syntax highlighting + Alt+Z soft-wrap toggle

- **Date**: 2026-05-07
- **Phase**: 1.5 / 6.x — small editor polish, mostly motivated by "open trace" (test plan 0055): a coder JSONL with single 30-kchar lines is unreadable without one or both.

## What shipped

- `.jsonl` / `.ndjson` files now highlight with the legacy stream-mode JSON tokenizer (`@codemirror/legacy-modes/mode/javascript` exports a per-line `json` parser). The `lang-json` Lezer grammar can't be used for newline-delimited JSON because it expects a single top-level value; the legacy mode is per-line which is exactly what JSONL needs — each line is independently tokenized as a JSON value, the next line resets, no cross-line state to break.
- New `Alt+Z` shortcut toggles soft-wrap on every editor pane (regular Editor _and_ both sides of the diff view's MergeView). Mirrors the VS Code / Cursor convention so muscle memory carries over.
- Off by default — source code on a tab-aligned grid is still the priority. The toggle is window-global rather than per-buffer (the team flips it occasionally, not constantly; per-buffer state would be extra surface for ~zero benefit). Not persisted across restarts: one keystroke to re-toggle is cheap, and it keeps `AppState` lean.
- Reachable from the command palette as `Toggle Line Wrap` (id `editor.toggleLineWrap`). A quick toast — `Line wrap on` / `Line wrap off` — confirms the flip so a hit on an empty editor pane isn't a silent no-op.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a folder with at least one source file and one long-lined file you can open.

### JSONL highlighting

1. Open a coder session in moon-ide and click the new `</>` "open trace" button (test plan 0055) — or any `.jsonl` file you have lying around. (Easy way to generate one: send a single short message to the coder, then open the trace.)
2. Expected: keys (string literals before `:`) render in the string colour; values render with proper JSON colours (numbers, booleans, `null` distinct from strings); braces / brackets / commas pick up the punctuation colour. Compare against a `.json` file in the same buffer for visual parity.
3. Open a `.ndjson` file (rename one of the JSONL files for a quick check). Same highlighting.
4. Edit a line so it's invalid JSON (drop a closing brace). Expected: the broken line tokenizes best-effort and **doesn't poison the next line** — line N+1 still highlights cleanly. This is the whole point of using the per-line stream mode.

### Soft-wrap on the regular editor

5. Open a source file with at least one line longer than the editor pane (a long string literal or a wrapped JSON config). Confirm the line scrolls horizontally — no wrap, default behaviour.
6. Press `Alt+Z`. Expected: a small toast `Line wrap on`, the long line wraps to fit the pane width, the gutter line numbers stay aligned with logical lines (a wrapped row shows no number on the continuation line).
7. Press `Alt+Z` again. Toast `Line wrap off`, the wrap reverts, horizontal scroll returns.
8. Open a second file in a split (`Ctrl+\`). Toggle wrap in either pane (the focused one is fine). Expected: both panes flip together — the toggle is window-global.

### Soft-wrap on the diff view

9. Edit a file with long lines, save. Open its diff view (`Ctrl+Shift+D`).
10. Press `Alt+Z`. Expected: both sides of the MergeView wrap together, hunks stay aligned, the change-bar gutter and right-edge overview ruler still point at the right rows.
11. Toggle off again. Both sides un-wrap together.

### Inputs / textareas don't swallow Alt+Z literal characters

12. Focus the command palette search input (`Ctrl+P`), the Slack composer, or the coder composer. Press `Alt+Z`.
13. Expected: depending on the OS / keyboard layout, the literal `z` (or whatever Alt+Z produces) types into the input. The wrap toggle does **not** fire — `isTextInputTarget` exempts inputs and textareas so dead-key compositions and similar still work in those surfaces.

### Palette discoverability

14. `Ctrl+Shift+P` → search "wrap" → `Toggle Line Wrap` shows up with the `Alt+Z` shortcut on the right. Click it. Same toast + flip as the keystroke path.

## What must keep working

- Every other shortcut wired in `App.svelte`: `Ctrl+O`, `Ctrl+N`, `Ctrl+S`, `Ctrl+W`, `Ctrl+P`, `Ctrl+Shift+F`, `Ctrl+Shift+D`, `Ctrl+0`, `Ctrl+L`, `Ctrl+J`, `Ctrl+\`, `Alt+Left`, `Alt+Right`, `F6`.
- Existing JSON highlighting (`.json`, `.jsonc`) — the Lezer-based `json()` extension is unchanged; only `.jsonl` / `.ndjson` route to the new path.
- LSP / git / blame / editorconfig wiring on regular files — wrap is a pure CM extension toggle and doesn't touch any of those.
- Diff view: hunk-jump (`F7` / `Shift+F7`), revert controls, auto-scroll-to-first-chunk on open, change-bar gutter + overview ruler.

## Known limitations

- Soft-wrap is window-global, not per-buffer. Toggling it for one specific JSONL trace also wraps every other tab in the window. The team's small + the use case is intermittent, so we held off on per-buffer state. Add later if the cost surfaces (probably under a second `ctrl-K Z` "wrap this file only" gesture, matching VS Code).
- Not persisted across restarts. Reopen → wrap is back off. One keystroke, low-friction; we'd persist if the team complained, but it'd also pull `lineWrap` into `AppState` which we'd rather avoid until needed.
- JSONL highlighting uses the legacy stream tokenizer, not the Lezer grammar. That's deliberate (per-line independence is a feature for JSONL) but it does mean **structural** features like fold-by-object or LSP-level JSON-schema validation don't apply. We don't ship those today regardless; flag this if anyone wants them.

## Related

- Specs: [coder.md](../coder.md) — the JSONL trace format that motivated highlighting.
- Prior test plans: [0055-open-session-trace.md](0055-open-session-trace.md) (the `</>` "open trace" affordance — this plan closes its "no syntax highlighting for JSONL" caveat), [0051-open-host-file.md](0051-open-host-file.md) (the host-direct file mechanism the trace tabs ride).
