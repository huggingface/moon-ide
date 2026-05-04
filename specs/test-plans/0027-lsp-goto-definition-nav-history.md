# Test plan 0027: Goto-definition (Ctrl/Cmd click) + Alt-based nav history

- **Date**: 2026-05-04
- **Phase**: Phase 4 (LSP) — stage 2 slice

## What shipped

- Ctrl/Cmd-hover over an identifier underlines it as a link when the LSP has a definition target. Ctrl/Cmd-click jumps to that definition — same convention every other modern editor uses.
- New `textDocument/definition` wiring end-to-end: `LspLocation` in `moon-protocol`, `LspServer::definition` + `LspBroker::definition`, `lsp_definition` Tauri command, `ipc.lsp.definition`, and a new `src/lib/editor/lspGotoDefinition.ts` CM extension.
- Browser-style file-level navigation history on `WorkspaceState`: `navStack` / `navIndex` / `pendingJumps`, and `Alt+Left` / `Alt+Right` keybindings that fall through to CM's default word-motion when there's nowhere to go.
- External-target fallback: definitions that resolve outside the workspace root (e.g. `node_modules/@types/…`) surface a toast rather than opening a nonexistent tab.

## How to test

Prerequisites: `bun install`, `bun run tauri dev` against moon-ide itself (so the project-local `tsgo` LSP is live).

### Ctrl/Cmd-hover link preview

1. Open any TypeScript file with imports, e.g. `src/lib/state.svelte.ts`.
2. Move the mouse over an imported symbol **without** holding any modifier. Expected: nothing changes — no underline, no cursor change.
3. Press and hold `Ctrl` (Linux/Windows) or `Cmd` (macOS). Move the mouse over the same imported symbol. Expected:
   - The symbol's span is underlined in accent colour.
   - The cursor changes to `pointer`.
   - No flicker on sibling identifiers — move slowly from one identifier to another; each picks up and drops the underline independently.
4. Release the modifier. Expected: underline clears instantly. Re-hovering without the modifier: still nothing.
5. Hold the modifier and hover over a **local** variable — the same identifier as the declaration you're on. Expected: **no** underline (self-jump suppression: there's nowhere to go from the declaration of `x` back to `x` itself).
6. Hold the modifier and hover a keyword (`const`, `function`, `return`). Expected: no underline — LSP returns no target.
7. Hold the modifier and hover over whitespace / a comment. Expected: no underline, no IPC traffic (verify via network panel if desired — the ViewPlugin short-circuits when `wordAt` returns null).

### Ctrl/Cmd-click jump — in-workspace

1. In `src/lib/state.svelte.ts`, Ctrl/Cmd-click on an imported function (e.g. `workspace`, `ipc`, `fingerprint`).
2. Expected: the editor switches to the declaration file, and the caret lands on the identifier name (not the start of the line and not inside a function body).
3. The status-bar active-path reads the new file. The file tree highlights it. Tab strip shows it as active. (All of this is standard `openFile` behaviour — the jump goes through it.)
4. The jump is recorded in nav history: press `Alt+Left`. Expected: back at the original `state.svelte.ts` location.
5. Press `Alt+Right`. Expected: forward to the definition again.

### Ctrl/Cmd-click — external target

1. In any TS file, Ctrl/Cmd-click on a built-in like `Promise`, `Array`, or a type from `@types/node`.
2. Expected: a toast appears with text `Definition outside workspace: file:///…/node_modules/…`. No tab opens. Editor state is unchanged.
3. Ctrl/Cmd-click on an identifier that resolves to a moon-ide file. Expected: normal in-workspace jump — the external fallback only fires when the resolved URI strips outside the workspace root.

### Navigation history — basics

1. Start fresh: Ctrl+Shift+P → close all tabs (or just restart). Open `src/main.ts`, then `src/lib/state.svelte.ts`, then `src/lib/components/Editor.svelte` — three files via the file tree.
2. `Alt+Left` three times. Expected: `state.svelte.ts` → `main.ts` → (fourth press does nothing — already at oldest).
3. Track the UI: each back step opens the previous tab (or the tab stays open already), focuses it, and the file-tree highlight follows.
4. `Alt+Right` twice. Expected: walks forward to `Editor.svelte`.
5. From a middle-of-history entry (navigate partway back), open a **different** file (`src/styles.css`). Expected: the forward stack is truncated — pressing `Alt+Right` afterwards does nothing (no path left to go forward to).

### Navigation history — word-motion fall-through on macOS

On macOS, `Option+Left` / `Option+Right` is the default CodeMirror word-motion binding. We only shadow it when nav history has somewhere to go.

1. In a fresh session with no prior file opens, put the caret in the middle of a multi-word line (e.g. `const myVariableName = 1;`). Press `Option+Left`. Expected: caret jumps one word left — CM's default word-motion works because the nav stack has only one entry (`canNavigateBack === false`).
2. After opening two different files (so `canNavigateBack` is true), the same key now navigates back instead. Word-motion via `Option+Left` is shadowed while nav is available.
3. This is documented behaviour, not a bug — explicitly requested trade-off. `Option+Right` behaves symmetrically.

On Linux/Windows, `Alt+Left` is unbound by default, so the shadowing question doesn't arise.

### Lifecycle edge cases

1. Ctrl/Cmd-click an identifier whose server hasn't started yet (e.g. the first TS file of the session, while the "typescript: starting…" pill is still showing). Expected: the click enqueues the request; the jump happens as soon as the server is up. No crash, no toast.
2. Kill `tsgo` with `pkill tsgo` mid-session. Ctrl/Cmd-hover an identifier. Expected: the probe's LSP call fails silently, no underline, no toast (status pill flips to `crashed` on its own). Next file open respawns the server.
3. Open the preferences file (`Ctrl+,` — if wired) or any non-TS file. Ctrl/Cmd-hover an identifier. Expected: no underline, no probes (the LSP language-id map returns `null` for unknown extensions).
4. Split view: open two files side by side, jump to definition from the left pane. Expected: target lands in the left pane (the side that originated the jump), not the right.

### Regression: hover popover still works

1. With the `Cmd`/`Ctrl` key **not** held, let the cursor rest on an identifier for ~400 ms. Expected: the Markdown hover popover (test plan 0025) still opens with the symbol's type signature.
2. While the hover popover is visible, tap `Ctrl`/`Cmd`. Expected: the popover dismisses (CM's `hideOnChange: true` behaviour is unchanged). A second hover pops it again.

## What must keep working

- LSP stage 1 behaviour from test plans 0024 and 0025: diagnostics markers + status pill, Markdown-rendered hover, Ctrl-Space completions, missing-binary handling.
- Existing navigation: tab clicks, file-tree clicks, split view, focus ring — all route through `setActive`, which now also pushes nav history. No visible change other than the history being populated.
- Word-motion via `Ctrl+Left` / `Ctrl+Right` on Linux/Windows. The nav binding is `Alt+...` only, not `Ctrl+...`.
- `Ctrl+Click` on non-editor surfaces (file-tree entries, tab strip) is unaffected — this extension only attaches its listeners inside the editor's DOM.
- DOMPurify / Markdown pipelines are untouched.

## Known limitations

- Back/forward preserves **file identity** only, not caret position. Re-visiting a file via `Alt+Left` opens it at wherever CM rebuilds to (usually doc start). The `pendingJumps` plumbing is already there; extending nav entries to `{ path, line, character }` is a follow-up.
- External definitions (node_modules types, toolchain sources) show a toast rather than opening a read-only view. A real external-file viewer is a later stage.
- The "peek definition" inline UI (VSCode-style preview without leaving the current file) isn't here. Jumps are hard — they replace the active tab's contents.
- No go-to-references / go-to-implementation / rename / code-actions yet. Stage 3+.
- First Ctrl/Cmd-hover probe on a cold tsgo has the same ~1–3 s latency as the first hover request; the link shows up once the server replies. No spinner — the underline just appears when ready.
- On macOS, holding Cmd while clicking a **link inside a Markdown hover popover** still follows the link via the app's normal link handler (opens externally); the goto-def hook only triggers on editor-DOM events, not popover children.
- A Ctrl/Cmd-click that lands during a rapid-fire debounce window (e.g. mid-type) might see the LSP answer based on slightly-stale text — tsgo will re-resolve after its internal doc sync catches up. In practice tsserver/tsgo's didChange is fast enough for this to never matter.

## Related

- Specs: `specs/lsp.md` — new "Go-to-definition" + "Navigation history" + "Pending jumps" subsections.
- Prior test plans: `0024-lsp-typescript-stage-1.md` (diagnostics/hover/completion), `0025-markdown-syntax-highlighting.md` (rich hover).
