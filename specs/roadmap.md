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
| 4     | LSP                             | scaffolded  |
| 5     | Git layer                       | scaffolded  |
| 6     | ACP integration                 | scaffolded  |
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

## Phase 5 — Git

`gix`-based status/blame/diff. Tree decorations via Pierre Trees' built-in git status indicators (gitignored entries appear faded; modified/added/deleted/untracked all surface). Inline blame on hover (CM6 line decoration). Diff view via `@pierre/diffs`. Minimal SCM panel.

Tree behavior: gitignored directories are **collapsed by default** (and faded), so noise like `node_modules/`, `target/`, `dist/` doesn't render thousands of entries on first paint. Expanding one is still cheap and remembered for the session.

**Tree markers via Pierre's `gitStatus`.** Hand Pierre an array of `{ path, status: 'added' | 'modified' | 'deleted' }` via `tree.setGitStatus(entries)`; folder bubble-up (`data-item-contains-git-change="true"`) and per-row attributes (`data-item-git-status="…"`) come for free. The only behaviours we layer on top:

- **Deleted rows stay visible.** Pierre only renders paths we keep in the tree's `paths` array, so the array we hand it is `union(workdir, status_only_deletions)` — deleted-but-not-committed entries persist with their `deleted` marker until the deletion is committed, breaking VSCode's convention of dropping them. Restoring is `git checkout HEAD -- <path>` (palette command); after the working tree matches HEAD the next refresh strips the ghost row.
- **Renames** map naturally to a `deleted` row at the old path and an `added` row at the new path; we don't try to be cleverer than git here.
- **Conflicts** can't ride Pierre's three-state model; surface them in the SCM panel and the editor gutter, and leave the tree row in whatever working-tree state it actually has.

Refresh on fs-watch events plus an explicit `setGitStatus` call after any moon-ide-issued git op. Once the change reaches a commit, the markers and ghost rows disappear in the same refresh tick — no stale state surviving across commits.

Until this phase lands, the file tree shows everything except the `.git/` directory itself. Dotfiles like `.editorconfig` and `.husky/` are real working files and stay visible by design.

## Phase 6 — ACP

ACP host using the `agent-client-protocol` crate. Agent panel in the UI: chat, tool stream, edit preview. Pluggable agent binary (settings select opencode / claude code / etc.). Tool calls route through the active host so containerized agents only touch container resources.

## Phase 7 — Multi-repo coordination

Phase 2.5 already shipped the multi-folder workspace shape. What Phase 7 adds on top: cross-folder search via per-folder `tantivy` indices with a parallel query layer, ACP's `workspace.repos` tool so agents can target `@repo-name`, and named multi-workspace UI (today's singleton `"default"` becomes one of many, with `Open Workspace…` / `Switch Workspace…` affordances).

App state grows with this phase: today's single workspace (folders + active) becomes a list of named workspaces with the most recent set of folders per workspace. The `AppState` struct in `moon-core` is the natural place for this.

## Phase 8 — Lint / format

oxlint, oxfmt, prettier, eslint as sidecar processes. Debounced. Diagnostics merged with LSP diagnostics in a single problems panel. Format on save with per-language chooser.

## Phase 9 — Custom tool plugins

Plugin manifest declares webview URL or sidecar binary, capabilities, display target. Tiny JSON-RPC API plugins call into the core. Mongoku-as-plugin is the first integration.

## Phase 10 — Theming

A single theme definition drives every coloured surface in the IDE — file tree, diff view, editor, terminal, SCM gutters, status bar — instead of three separate styling regimes. Pierre publishes [`@pierre/theme`](https://github.com/pierrecomputer/theme) (VS Code + Shiki + Zed + Cursor compatible, light/dark/vibrant variants); we adopt its theme file shape as the canonical format so anything Shiki understands works out of the box.

Surfaces this phase wires up:

- **File tree** (`@pierre/trees`) — already CSS-variable driven; map the active theme's role colours onto its tokens.
- **Diff view** (`@pierre/diffs`) — consumes Pierre/Shiki themes natively; pass the same theme JSON.
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
