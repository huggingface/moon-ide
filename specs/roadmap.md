# Roadmap

The full phased plan. Update the **Status** column as phases land.

## How to work this roadmap

- Phases are numbered. Land them in order.
- **At the end of every phase, stop.** Hand control back to the human reviewer with: a summary of what was built, what was skipped, the new files/specs, and how to test locally. Do **not** start the next phase, even if every checklist item is green. The reviewer kicks off the next phase explicitly.
- A phase is "implemented" only after the human says so — passing tests are necessary, not sufficient.
- **Scope discipline.** This is an IDE for one team, not a product. Don't pad phases with "nice-to-haves nobody asked for". Items get added to a phase when there's a concrete request behind them — from a test, a team member, or a real bug. Speculative `// later we might want…` lists in specs are fine; checklist items in a phase are not. The bootstrap exception applies: anything moon-ide's own source tree relies on counts as in-scope by default (see [AGENTS.md](../AGENTS.md#the-bootstrap-exception)).
- **Test plans.** Each phase ships with at least one entry in `specs/test-plans/` written before the phase is handed back for review. Subsequent commits inside the same phase get their own plans when they cross IPC / `WorkspaceHost` / new-UI boundaries. See [specs/test-plans/README.md](test-plans/README.md).
- If you discover a phase needs to be split or reordered, update the roadmap in the same change and call it out in the handoff.

| Phase | Title                           | Status      |
| ----- | ------------------------------- | ----------- |
| 0     | Skeleton (open / edit / save)   | implemented |
| 1     | Editor + navigation             | implemented |
| 1.5   | Editor polish                   | scaffolded  |
| 2     | Devcontainer / remote split     | scaffolded  |
| 3     | Terminal                        | scaffolded  |
| 4     | LSP                             | scaffolded  |
| 5     | Git layer                       | scaffolded  |
| 6     | ACP integration                 | scaffolded  |
| 7     | Multi-repo + cross-repo queries | scaffolded  |
| 8     | Linting / formatting            | scaffolded  |
| 9     | Custom tool plugins             | scaffolded  |
| 10+   | Innovation track                | open-ended  |

"Scaffolded" means: the module/spec slot exists but the feature is not real code yet. Each phase replaces "scaffolded" with "implemented" when its acceptance criteria are met.

## Phase 0 — Skeleton

**Acceptance**: launch the app, open a folder, see files in a Pierre tree, click a file to open it in CodeMirror, edit, save with Ctrl+S.

- Tauri 2 + Svelte 5 + Vite project boots
- `crates/moon-protocol/` defines fs/workspace methods
- `crates/moon-core/` implements local fs ops
- `src-tauri/` exposes those as Tauri commands
- `src/lib/ipc.ts` wraps the commands typedly
- `FileTree.svelte` shows the workspace via Pierre Trees vanilla
- `Editor.svelte` opens files in CodeMirror 6
- Save persists; dirty state shown in tab

## Phase 1 — Editor + navigation

**Acceptance**: tabs + horizontal/vertical splits, command palette, ripgrep-backed file search, persisted UI session (workspace + tabs + active + theme), hardcoded keybindings for the team's must-haves.

**Outstanding from Phase 1** (closed in the post-Phase 1 polish):

- Defaults pinned to tabs at width 2 (was a Phase-1.5 deferral; flipped in moon-protocol then absorbed into `Editor.svelte` constants alongside ADR 0006).
- Per-workspace `settings.json` removed in favour of `.editorconfig` (Phase 1.5) for project style and `AppState` for per-machine state (theme, last session). See [ADR 0006](decisions/0006-no-settings-file.md).

## Phase 1.5 — Editor polish

A small, scoped phase that closes Phase 1's loose ends and adds the bare minimum needed for moon-ide to feel right when opening _itself_. Surfaced after Phase 1 closed because of the bootstrap concern in [ADR 0005](decisions/0005-bootstrap.md): without this, contributors editing moon-ide-in-moon-ide diverge from house style on every keystroke until the pre-commit hook fires.

**Acceptance**:

- `.editorconfig` honored end-to-end. See [editorconfig.md](editorconfig.md). Specifically:
  - `indent_style`, `indent_size` / `tab_width` drive CodeMirror's tab size and the Tab keymap, replacing the hardcoded constants currently in `Editor.svelte`.
  - `end_of_line`, `insert_final_newline`, `trim_trailing_whitespace` are applied as pre-save hooks.
  - `charset` is utf-8 only for v1; anything else logs a warning.
  - Reload happens on `.editorconfig` save (the host clears its resolution cache when moon-ide writes a `.editorconfig`; external edits — git pull, another editor — wait for restart until Phase 5 ships the fs watcher).
- Precedence: `.editorconfig` over moon-ide defaults. There is no project-level overlay file; per [ADR 0006](decisions/0006-no-settings-file.md) `settings.json` is gone.
- No per-language `tab_size` default — let `.editorconfig` and the file's language decide.
- Pre-save hook pipeline is generic (a list of `BeforeSaveTransform`s) so Phase 8 can drop format-on-save into the same pipeline without re-architecting it.
- **Markdown rendered preview.** Opening a `.md` / `.markdown` file shows a per-tab toggle between source ("Code") and rendered ("Preview"). Default mode is "Preview" (the README is what we want to see when clicking it; opening for editing is the deliberate gesture). The Cursor-style two-state toggle lives on the tab strip, scoped to the active tab. Renderer runs in-process (no IPC roundtrip per render); pick a small library — `marked` or `markdown-it` — at implementation time, not before. No syntax-highlighting inside code fences yet, no Mermaid, no math; surface those when asked.
- **Per-pane open file lists.** Phase 1 ships splits with one shared `openFiles` array and two independent active selections — both panes show the identical tab strip, only the active tab differs. Move to one open list per pane (VSCode/Zed convention): each split has its own tab strip, reordering is per-pane, closing a tab on one pane leaves it open on the other, a file can live in one pane, both, or neither. `WorkspaceSession` grows from one `open_files` to a per-pane pair. Drag-between-panes comes later when someone actually asks for it.
- **New untitled tab.** `Ctrl+T` opens a fresh "untitled" buffer in the focused pane with no path on disk. The first `Ctrl+S` against an untitled buffer opens the native save dialog (Tauri); the chosen path becomes the tab's path and the buffer joins `openFiles` as a normal entry. The chosen extension drives the language extension (typing in an untitled buffer then saving as `foo.svelte` switches highlighting to Svelte; saving as `foo.ts` switches to TypeScript). Untitled buffers do **not** survive a restart — text is not persisted in `WorkspaceSession`; closing a dirty untitled buffer fires the same discard prompt as any other dirty file.
- **Save-as / language re-detection on rename.** When a file is saved to a new path with a different extension (untitled → `.ts`, or an existing `.ts` saved as `.svelte` once Save As exists), the language extension swaps in place — same compartment dance the editor already does on tab switch.

Keybindings remain hardcoded for now. We add user-rebindable keymaps when there's a concrete team request for it, not before.

## Phase 2 — Devcontainer / remote split

**Acceptance**: open a folder with `.devcontainer/devcontainer.json`, get prompted to "open in container", everything (terminal, fs, lint, LSP-once-it-exists) routes through the container. Forward a port via the palette and reach the in-container service from the host. See [devcontainers.md](devcontainers.md).

**Bootstrap concern** (per [ADR 0005](decisions/0005-bootstrap.md)): the devcontainer image used by Moon IDE itself ships with `rustup`, `bun`, and the WebKitGTK dev libraries so a fresh checkout of moon-ide is buildable with no host-side tooling.

## Phase 3 — Terminal

xterm.js + portable-pty terminals, multiple sessions, splits. Spawned via active host so they run inside the container when remote.

## Phase 4 — LSP

LSP multiplexer in `moon-core`. TS, Svelte, CSS, HTML, JSON, MD servers. Diagnostics, completion, hover, goto-def, find-refs, rename, code actions. Navigation history (alt+left/right).

## Phase 5 — Git

`gix`-based status/blame/diff. Tree decorations via Pierre Trees' built-in git status indicators (gitignored entries appear faded; modified/added/deleted/untracked all surface). Inline blame on hover (CM6 line decoration). Diff view via `@pierre/diffs`. Minimal SCM panel.

Tree behavior: gitignored directories are **collapsed by default** (and faded), so noise like `node_modules/`, `target/`, `dist/` doesn't render thousands of entries on first paint. Expanding one is still cheap and remembered for the session.

Until this phase lands, the file tree shows everything except the `.git/` directory itself. Dotfiles like `.editorconfig` and `.husky/` are real working files and stay visible by design.

## Phase 6 — ACP

ACP host using the `agent-client-protocol` crate. Agent panel in the UI: chat, tool stream, edit preview. Pluggable agent binary (settings select opencode / claude code / etc.). Tool calls route through the active host so containerized agents only touch container resources.

## Phase 7 — Multi-repo

Workspace = ordered list of repo roots. Multi-root tree (multiple Pierre Trees instances). Cross-repo search via per-repo `tantivy` index, parallel query. ACP gets a `workspace.repos` tool; agents can target `@repo-name`.

App state grows with this phase: today's single `last_session` (one workspace + its tabs) becomes a list of recently-opened workspaces and the most recent multi-repo set, with each workspace keeping its own session. The `AppState` struct in `moon-core` is the natural place for this.

## Phase 8 — Lint / format

oxlint, oxfmt, prettier, eslint as sidecar processes. Debounced. Diagnostics merged with LSP diagnostics in a single problems panel. Format on save with per-language chooser.

## Phase 9 — Custom tool plugins

Plugin manifest declares webview URL or sidecar binary, capabilities, display target. Tiny JSON-RPC API plugins call into the core. Mongoku-as-plugin is the first integration.

## Phase 10+ — Innovation track

Open-ended. Examples:

- Inline AI ghost text + accept/reject UI
- Agent-driven multi-file diffs with batch acceptance
- Cross-repo refactor planner
- Custom WebGL git lane renderer
