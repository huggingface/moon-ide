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
| 2     | Containerised dev shells        | scaffolded  |
| 2.5   | Multi-folder workspace UX       | scaffolded  |
| 3     | Terminal                        | scaffolded  |
| 4     | LSP                             | in progress |
| 5     | Git layer                       | in progress |
| 6     | Coder (in-process AI agent)     | scaffolded  |
| 7     | Multi-repo + cross-repo queries | scaffolded  |
| 8     | Linting / formatting            | scaffolded  |
| 9     | Custom tool plugins             | scaffolded  |
| 10    | Theming                         | scaffolded  |
| 11    | Slack chat panel                | scaffolded  |
| 12+   | Innovation track                | open-ended  |

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

Closes Phase 1's loose ends and adds the bare minimum needed for moon-ide to feel right when opening _itself_. Driven by the bootstrap concern in [ADR 0005](decisions/0005-bootstrap.md). Architectural spec: [editorconfig.md](editorconfig.md). Sub-phase work breakdown: [roadmaps/phase-01.5-editor-polish.md](roadmaps/phase-01.5-editor-polish.md).

**Acceptance**: `.editorconfig` honored end-to-end (replacing the per-workspace `settings.json` killed by [ADR 0006](decisions/0006-no-settings-file.md)); a generic pre-save hook pipeline that Phase 8 can drop format-on-save into; rendered Markdown preview; per-pane open-file lists; untitled tabs (`Ctrl+N`) with save-as language re-detection; `F6` / `Shift+F6` / `Ctrl+0` region focus; tree-driven file deletion (trash by default, hard-delete on Shift). Keybindings stay hardcoded.

## Phase 2 — Containerised dev shells

**Acceptance**: opening a workspace provisions a single unprivileged Docker container (one per workspace, not per project) from a moon-published `moon-base` image (Debian + polyglot toolchain). The workspace shell (`moon-ws-<id>`, dev-only) handles terminals, LSP, lint/format, and builds; the Tauri shell, Slack, and agent runtimes stay on the host. Each bound folder's own `docker-compose.yml` runs as a **separate** compose project (`moon-ws-<id>-<folder-slug>`), launched per-folder from the folder bar — keeps a stalled project service from blocking the workspace shell. Closing the workspace pauses the shell; per-folder projects are user-driven. Declared port forwards are reachable from the host and surfaced in the IDE.

System architecture: [containers.md](containers.md). Sub-phase work breakdown: [roadmaps/phase-02-containers.md](roadmaps/phase-02-containers.md). Decisions: [ADR 0007 — compose + moon-base](decisions/0007-compose-and-moon-base.md), [ADR 0008 — host-shared daemon](decisions/0008-host-shared-daemon.md).

**Bootstrap concern** (per [ADR 0005](decisions/0005-bootstrap.md)): `moon-base` ships with `rustup`, `bun`, and the WebKitGTK dev libraries so a fresh moon-ide checkout is buildable inside its own container.

## Phase 2.5 — Multi-folder workspace UX

The "command centre" foundation: a workspace becomes a list of folders rather than _being_ a folder. Pulled forward from Phase 7 because the Phase 2 container redesign — workspace state lives outside any repo, compose project survives across folder switches — is incoherent without it. See [`roadmaps/phase-02.5-multi-folder.md`](roadmaps/phase-02.5-multi-folder.md) for the work breakdown and [`containers.md` § Multi-folder workspace](containers.md#multi-folder-workspace-the-command-centre-ux) for the container redesign that lights up on top.

**Acceptance**: opening a folder adds it to the workspace as a new folder bar in the sidebar instead of replacing the active workspace; clicking a bar makes it active and swaps the file tree + tabs to that folder's persisted state; an inline `+ Add folder` row picks a new folder; an `×` per bar removes it (with confirm) including its session entry; per-folder tab/active state survives restart. One workspace (`"default"`) with N folders — multi-workspace UI stays a Phase 7 concern.

What deliberately doesn't ship in 2.5: showing more than one folder's tree at once, cross-folder search, drag-to-reorder bars, folder rename. (Compose indicators on the folder bars shipped right after, with Phase 2.0.6 — see the [`phase-02-containers.md` § 2.0.6 — workspace shell vs project services](roadmaps/phase-02-containers.md#206--workspace-shell-vs-project-services-shipped) section.)

## Phase 3 — Terminal

xterm.js + portable-pty terminals, multiple sessions, splits. Spawned via active host so they run inside the container when remote.

## Phase 4 — LSP

LSP multiplexer in `moon-core`. TS, Svelte, CSS, HTML, JSON, MD servers. Diagnostics, completion, hover, goto-def, find-refs, rename, code actions. Navigation history (alt+left/right).

Architectural spec: [lsp.md](lsp.md). The [`tower-lsp` vs thin-client open question](architecture.md#resolved) is resolved — we roll ~300 LOC on top of `lsp-types`.

**What has landed so far** (see `specs/test-plans/0024-*.md`):

- Stage 1 slice for **TypeScript only**: diagnostics (red squigglies + gutter markers + status-bar error/warn counts), hover tooltip, explicit-invocation completion source registered on the existing `autocompletion` extension.
- Server is `tsgo` (Microsoft's native TS 7 port, shipped as `@typescript/native-preview` — already in moon-ide's devDependencies). Project-local discovery walks up from the active folder looking for `node_modules/.bin/tsgo` before falling back to `$PATH`, so a fresh `bun install` is all a contributor needs.
- `moon-core::lsp` module: Content-Length framing, thin JSON-RPC client with actor-pattern reader/writer, per-language `LspServer` actor, multi-language `LspBroker` with lazy spawn and graceful `NotAvailable` fallback when no copy of the binary can be found anywhere.
- `moon-protocol::lsp` carries moon-shaped subsets of upstream LSP types so the UI never sees raw `lsp-types`.
- Per-language availability pill in the status bar (`starting…`, `not available`, `crashed`, `stopped`) — `running` stays invisible. Tooltip reveals the resolved binary path (project-local vs global) on hover.
- Stage 2 slice: **goto-definition** via Ctrl/Cmd-hover link preview + Ctrl/Cmd-click jump, routed through a **position-aware, cross-folder** navigation history (`Alt+Left` / `Alt+Right`). Nav entries carry `{ folder, path, line, character }`; clicks push, keyboard motion updates the tip, and folder swaps happen transparently on back/forward. Goto-definition into a sibling bound folder opens in that folder; only targets outside every bound folder still surface a toast.

**Still outstanding for this phase**: Rust (rust-analyzer), Svelte (svelte-language-server), CSS / HTML / JSON / MD servers; go-to-definition with Ctrl-click underline; find-references panel; rename; code actions; navigation history (Alt-Left / Alt-Right); incremental document sync; signature help.

## Phase 5 — Git

`gix`-based status/blame/diff. Tree decorations via Pierre Trees' built-in git status indicators (gitignored entries appear faded; modified/added/deleted/untracked all surface). Inline blame on hover (CM6 line decoration). Diff view via `@codemirror/merge` (CodeMirror's native side-by-side merge view, editable working-tree side). Minimal SCM panel.

Tree behavior: gitignored directories are **collapsed by default** (and faded), so noise like `node_modules/`, `target/`, `dist/` doesn't render thousands of entries on first paint. Expanding one is still cheap and remembered for the session.

**Tree markers via Pierre's `gitStatus`.** Hand Pierre an array of `{ path, status: 'added' | 'modified' | 'deleted' }` via `tree.setGitStatus(entries)`; folder bubble-up (`data-item-contains-git-change="true"`) and per-row attributes (`data-item-git-status="…"`) come for free. The only behaviours we layer on top:

- **Deleted rows stay visible.** Pierre only renders paths we keep in the tree's `paths` array, so the array we hand it is `union(workdir, status_only_deletions)` — deleted-but-not-committed entries persist with their `deleted` marker until the deletion is committed, breaking VSCode's convention of dropping them. Restoring is `git checkout HEAD -- <path>` (palette command); after the working tree matches HEAD the next refresh strips the ghost row.
- **Renames** map naturally to a `deleted` row at the old path and an `added` row at the new path; we don't try to be cleverer than git here.
- **Conflicts** can't ride Pierre's three-state model; surface them in the SCM panel and the editor gutter, and leave the tree row in whatever working-tree state it actually has.

Refresh on fs-watch events plus an explicit `setGitStatus` call after any moon-ide-issued git op. Once the change reaches a commit, the markers and ghost rows disappear in the same refresh tick — no stale state surviving across commits.

Until this phase lands, the file tree shows everything except the `.git/` directory itself. Dotfiles like `.editorconfig` and `.husky/` are real working files and stay visible by design.

**What has landed so far** (see `specs/test-plans/0020-*.md` through `0022-*.md`):

- Tree markers via Pierre's `gitStatus` for added / modified / deleted / untracked / ignored, backed by `git status --porcelain=v1` with a `WalkBuilder` fallback for non-repo folders.
- Deleted rows stay visible by union-ing git's `deleted` set into the tree's `paths` array, matching the contract above.
- Auto-refresh: a `notify::RecommendedWatcher` rooted at the active folder emits debounced `fs:changed` Tauri events; window-focus events are a second-class fallback for when inotify is exhausted or the folder lives on NFS / SSHFS. Palette has "Refresh File Tree" as a manual escape hatch for the integrated terminal.
- Per-row "Discard changes" via a hover / right-click context menu on changed rows: routes modified + deleted through `git restore --source=HEAD --staged --worktree` and untracked rows to the OS trash, confirming every time. First consumer of Pierre's `composition.contextMenu` API, via a reusable `ContextMenu.svelte` popover.
- **Inline blame** for the active line (GitLens-style): a dim `author, relative-date • summary` badge sits at end-of-line for the caret's current row, and hovering the badge opens a tooltip with the full author, commit date, short hash, and commit subject. Backed by `WorkspaceHost::git_blame` / `fs_git_blame` shelling out to `git blame --porcelain -w`. Uncommitted edits render as `Uncommitted changes`; blame refreshes on save. Stale across live edits by design — the widget is a glance, not a ground truth.
- **Diff view** via `@codemirror/merge` (see `specs/test-plans/0036-*.md`). `HEAD` content is pulled via a new `fs_git_head_content` command (`git show HEAD:<path>`); `DiffView.svelte` builds a `MergeView` with the HEAD blob (read-only) on the left and the working-tree buffer (editable) on the right. Both editors share the rest of the editor's chrome — language extension, theme, editorconfig, highlight-tabs — so the diff feels like the regular editor side-by-side, not a separate component to learn. **Single-tab + mode toggle**: each open buffer can flip between the regular editor and the diff view via `workspace.diffModes` (per-folder `Set<string>`), with toggle surfaces at (1) a `Source` / `Preview` / `Diff` tri-state in the right-edge tab toolbar, (2) `Ctrl/Cmd+Shift+D`, (3) the file-tree context menu's `View diff` entry, (4) clicks on per-line markers in the editor's git-change gutter, and (5) the palette command **Git: Toggle Diff View** (title flips with mode). Deleted rows are always in diff view (no editor counterpart). Edits on the right side go through the same `updateText` / `saveActive` path the editor uses — flip into diff, fix the line, flip back — because the diff and editor share one OpenFile buffer. LSP / blame / goto-def stay on the editor view (one `didOpen` per path); the diff view is intentionally a viewer + light-edit surface. The HEAD side picks up external `git commit` / `checkout` via the existing `headByPath` cache. Scope is deliberately minimal — `HEAD` vs working tree only, no staging / no branch compare / no per-hunk accept — matching what the team actively needs right now.
- **Git-change gutter** in the regular editor (see `specs/test-plans/0033-*.md`). A dedicated CodeMirror gutter diffs the live buffer against the cached `HEAD` blob (`jsdiff::diffLines`) and paints a thin green bar for added lines, a thin blue bar for modified lines, and a red wedge at the top / bottom of the line bordering a pure deletion. Recomputes on every transaction so the markers stay in sync as the user types; the `HEAD` cache itself re-fetches whenever `refreshGitStatus` runs (covering external commits / checkouts). A matching overview ruler overlays the right-edge scrollbar with scaled-down, clickable change markers so the user can jump to any diff region in the file at a glance. Deleted buffers keep rendering in diff view and suppress the inline gutter.

**Still outstanding for this phase**: the SCM panel, conflict markers, per-hunk stage / discard, and the "unstage" half of discarding staged-new files.

## Phase 6 — Coder (in-process AI agent)

A right-side coder panel that owns its loop end-to-end: streams from Hugging Face Inference Providers, dispatches its own tool calls, routes every tool through the active `WorkspaceHost` so a containerised workspace gets a containerised agent for free. Sessions persist as append-only JSONL and sync to a per-user private HF bucket (`<user>/moon-ide-sessions`) via `hf-xet`.

Architectural spec: [`coder.md`](coder.md). Sub-phase work breakdown: [`roadmaps/phase-06-coder.md`](roadmaps/phase-06-coder.md). Decisions: [ADR 0010 — coder rewrite, not ACP](decisions/0010-coder-rewrite-not-acp.md), [ADR 0011 — rename `moon-agent` → `moon-remote`](decisions/0011-rename-moon-agent-to-moon-remote.md).

**Acceptance** (per sub-phase): HF OAuth device-flow sign-in + read-only tool surface (6.0); SSE streaming + abort (6.1); mutating tools + container-aware bash (6.2); on-disk JSONL sessions + sidebar (6.3); model picker (6.4); steering + follow-up queues (6.5); `AGENTS.md` / `SYSTEM.md` / `SKILL.md` system-prompt assembly + compaction (6.6); per-user private HF bucket sync via `hf-xet` (6.7). Deliberately deferred (sub-agents, OpenRouter / custom providers, Anthropic OAuth, bucket browser, MCP, plan mode, permission popups) — see [`coder.md` § "Out of scope"](coder.md#out-of-scope-explicitly).

## Phase 7 — Multi-repo coordination

Phase 2.5 already shipped the multi-folder workspace shape. What Phase 7 adds on top: cross-folder search via per-folder `tantivy` indices with a parallel query layer, a `workspace_list` / `workspace_grep` tool surface for the coder so it can target `@folder-name`, and named multi-workspace UI (today's singleton `"default"` becomes one of many, with `Open Workspace…` / `Switch Workspace…` affordances).

App state grows with this phase: today's single workspace (folders + active) becomes a list of named workspaces with the most recent set of folders per workspace. The `AppState` struct in `moon-core` is the natural place for this.

## Phase 8 — Lint / format

oxlint, oxfmt, prettier, eslint as sidecar processes. Debounced. Diagnostics merged with LSP diagnostics in a single problems panel. Format on save with per-language chooser.

## Phase 9 — Custom tool plugins

Plugin manifest declares webview URL or sidecar binary, capabilities, display target. Tiny JSON-RPC API plugins call into the core. Mongoku-as-plugin is the first integration.

## Phase 10 — Theming

A single theme definition drives every coloured surface in the IDE — file tree, diff view, editor, terminal, SCM gutters, status bar — instead of three separate styling regimes. Pierre publishes [`@pierre/theme`](https://github.com/pierrecomputer/theme) (VS Code + Shiki + Zed + Cursor compatible, light/dark/vibrant variants); we adopt its theme file shape as the canonical format so anything Shiki understands works out of the box.

Surfaces this phase wires up:

- **File tree** (`@pierre/trees`) — already CSS-variable driven; map the active theme's role colours onto its tokens.
- **Diff view** (`@codemirror/merge`) — wraps two CodeMirror editors that already consume our `moonEditorTheme` + Lezer highlight styles, so the same Shiki-derived bridge applies on both sides.
- **Editor** — CodeMirror 6 today, with our own dark/light theme. Either:
  - Bridge: a tiny adapter that converts a Shiki theme's `tokenColors` into a CodeMirror `EditorView.theme` + Lezer highlight style. Keeps the Lezer pipeline.
  - Or rip-and-replace: drop CodeMirror in favour of a Shiki-backed custom editor (open-ended; lives in Phase 11+ if it ever happens). Until then, the bridge keeps everything visually consistent.
- **Terminal** (`xterm.js`) — Pierre themes already include ANSI palettes; feed them straight in.
- **Editor chrome** — status bar, command palette, sidebar background, scrollbar corner — already CSS-variable driven from `app.css`; rename variables once so a single map covers everything.

User-facing model: themes are **machine-local**, picked from the status-bar theme switcher (which today only flips dark/light) and persisted in `AppState`. No per-workspace overrides — the team agrees on personal preference here, like font size. Custom themes are JSON files dropped into a discoverable user dir; bundled themes ship in the binary. Future-fancy bits (live reload of edited theme files, per-tab theme overrides for screenshots, etc.) wait until somebody asks.

## Phase 11 — Slack chat panel

A right-side panel that DMs a Slack bot (defaults to Hugging Face's [Moonbot](https://github.com/huggingface/moon-bot), pluggable to any DM-able bot — Cursor, GitHub, etc.). One Slack thread = one bot session; we don't pretend to host the agent — we're a chat client over the Slack Web API. The bot has zero visibility into local IDE context; this is pure pass-through. Architectural spec: [slack-chat.md](slack-chat.md). Sub-phase work breakdown: [roadmaps/phase-11-slack-chat.md](roadmaps/phase-11-slack-chat.md).

**Acceptance** (per sub-phase): Slack token + bot pick persist (11.0); read-only chat with Slack mrkdwn rendering (11.1 / 11.1.1); polling-driven thread updates + read receipts (11.2); send messages with reaction display (11.3 / 11.3.1); multi-bot panel (11.4). Deferred features (file attachments, AI session titles, hosted OAuth, etc.) live in [`slack-chat.md` § "What this phase deliberately doesn't do"](slack-chat.md#what-this-phase-deliberately-doesnt-do).

## Phase 12+ — Innovation track

Open-ended. Examples:

- Inline AI ghost text + accept/reject UI
- Agent-driven multi-file diffs with batch acceptance
- Cross-repo refactor planner
- Custom WebGL git lane renderer
