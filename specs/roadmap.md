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
- **New untitled tab + Save As / language re-detection on rename.** `Ctrl+N` opens a fresh "untitled" buffer in the focused pane with no path on disk. The first `Ctrl+S` against an untitled buffer opens the native save dialog (Tauri); the chosen path becomes the tab's path and the buffer joins `openFiles` as a normal entry. `Save File As…` does the same rebind for an already-saved file. In both cases the chosen extension drives the language extension (typing in an untitled buffer then saving as `foo.svelte` switches highlighting to Svelte; saving as `foo.ts` switches to TypeScript). Untitled buffers do **not** survive a restart — text is not persisted in `WorkspaceSession`; closing a dirty untitled buffer fires the same discard prompt as any other dirty file. Test plan: [0006-untitled-tabs.md](test-plans/0006-untitled-tabs.md).
- **Keyboard focus between regions.** `F6` / `Shift+F6` cycle through the major UI regions (file tree → editor pane(s) → status bar) in the layout-current order; `Ctrl+0` jumps directly to the file tree; `Esc` from the file tree returns focus to the active editor (the search input keeps its native Esc). All three actions are surfaced in the command palette so the keys are discoverable. The tree's single-click / arrow-key selection now preview-opens files **without** stealing focus from the tree; Enter or double-click is the explicit "take me to the editor" gesture. Test plan: `specs/test-plans/0005-focus-regions.md`.
- **File deletion from the tree.** `Delete` / `Backspace` moves the targeted paths to the OS trash (XDG / Finder / Recycle Bin) via the cross-platform `trash` crate; `Shift+Delete` / `Shift+Backspace` permanently removes them (the team's recovery story is git for tracked files). Acts on the full multi-selection when the keyboard cursor sits on a selected row, otherwise on just the focused row (so arrow keys after a click hit the row the user is on). Selecting a directory and one of its descendants collapses to a single IPC call. Both actions show a native confirm with mode-specific wording (single-target = filename, multi-target = "N items"), run IPC in parallel via `Promise.allSettled` with a single failure toast, drop every tab the operation invalidates without firing the per-tab dirty-discard prompt, and refresh the tree. Pierre's search/rename inputs are protected so typing Backspace inside them never triggers a delete. Test plan: [0007-file-deletion.md](test-plans/0007-file-deletion.md).

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

**Tree markers via Pierre's `gitStatus`.** Hand Pierre an array of `{ path, status: 'added' | 'modified' | 'deleted' }` via `tree.setGitStatus(entries)`; folder bubble-up (`data-item-contains-git-change="true"`) and per-row attributes (`data-item-git-status="…"`) come for free. The only behaviours we layer on top:

- **Deleted rows stay visible.** Pierre only renders paths we keep in the tree's `paths` array, so the array we hand it is `union(workdir, status_only_deletions)` — deleted-but-not-committed entries persist with their `deleted` marker until the deletion is committed, breaking VSCode's convention of dropping them. Restoring is `git checkout HEAD -- <path>` (palette command); after the working tree matches HEAD the next refresh strips the ghost row.
- **Renames** map naturally to a `deleted` row at the old path and an `added` row at the new path; we don't try to be cleverer than git here.
- **Conflicts** can't ride Pierre's three-state model; surface them in the SCM panel and the editor gutter, and leave the tree row in whatever working-tree state it actually has.

Refresh on fs-watch events plus an explicit `setGitStatus` call after any moon-ide-issued git op. Once the change reaches a commit, the markers and ghost rows disappear in the same refresh tick — no stale state surviving across commits.

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

A right-side panel that DMs a Slack bot (defaults to Hugging Face's [Moonbot](https://github.com/huggingface/moon-bot), pluggable to any DM-able bot — Cursor, GitHub, etc.). One Slack thread = one bot session; each top-level DM message starts a new session, replies stay inside the thread. We don't pretend to host the agent — we're a chat client over the Slack Web API. The bot has zero visibility into local IDE context; this is pure pass-through. Detailed design: [slack-chat.md](slack-chat.md).

User-facing setup is a one-time `xoxp-` user OAuth token paste with an in-IDE walk-through (the Slack app the user installs is theirs, not ours — we don't ship a moon-ide Slack app yet). The token lives in the OS keyring (libsecret / Keychain / Credential Manager). Real-time updates run on `conversations.history` polling (~5 s, gated on panel-visible + active-thread) since Slack's push paths (Events API, Socket Mode, RTM) aren't workable for a desktop user-token client.

Sub-phases:

- **11.0 — Foundation.** `moon-slack` crate (Web API client: `auth.test`, `conversations.list?types=im`, `users.info`). Token storage in the OS keyring (`keyring` crate, `apple-native + windows-native + sync-secret-service + crypto-rust`). Tauri commands `slack_set_token` / `slack_status` / `slack_clear_token` / `slack_list_dm_bots` / `slack_select_bot` / `slack_clear_bot` / `slack_get_active_bot` / `slack_set_panel_visible`. Right-side panel scaffolding with a "Connect Slack" walkthrough listing all upfront-granted scopes, validation via `auth.test`, and a "DM-first" bot picker that scans the user's 50 most recent DMs (`DM_SCAN_LIMIT`). End state: the panel says "Connected as Eli — Moon Bot" and persists token + bot pick + panel visibility across restarts. Test plan: `0008-slack-foundation.md`.
- **11.1 — Read-only chat.** Render the DM session list (top-level messages, newest first) and the active thread (read-only message bubbles). Bot tile uses the avatar resolved during 11.0's DM scan; user/bot bubbles distinguished by `bot_id`. New tauri commands `slack_list_sessions` / `slack_get_thread` / `slack_set_active_thread` and a new `SlackAppState.active_thread_ts` so the open thread + panel visibility both round-trip across restarts. No polling, no edits, no sending — those land in 11.2 / 11.3. Test plan: `0009-slack-read-only-chat.md`.
- **11.1.1 — Slack mrkdwn rendering.** Hand-rolled tokenizer + Svelte renderer for Slack's mrkdwn dialect (links `<URL|label>`, mentions `<@U…>`, channel refs `<#C…|name>`, broadcasts `<!here>`, dates `<!date^…>`, bold/italic/strike, inline + fenced code, block quotes). Mention names resolve through a per-process `users.info` cache (new `slack_get_user` command). Session-list previews flatten the same tree to plain text. Brought forward from 11.4 because raw `<@U…>` tokens were unreadable.
- **11.2 — Polling + read receipts.** Background tokio loop in `src-tauri/src/slack_poller.rs` driven by panel-visible + active-thread + OS focus, with a per-thread cadence ladder (3 s hot → 5 s warm → 15 s → 60 s → paused cold — see [`slack-chat.md`](slack-chat.md#cadence-ladder)). Detects new messages and `edited.ts` edits; pushes the full thread snapshot to the frontend via the `slack:thread-update` Tauri event, plus `slack:disconnected` on auth failure. `conversations.mark` fires on view, on session switch, and on poll-tick-while-focused so unread badges clear in the user's actual Slack client. Test plan: `0011-slack-polling.md`.
- **11.3 — Send messages.** `chat.postMessage` wired to a textarea-based composer at the bottom of the panel, sent on Ctrl+Enter (plain Enter is a newline, matching Slack's own composer). "+ New session" toggles a fresh-conversation mode: posting creates a top-level message in the bot's DM and pivots the panel into the resulting thread; otherwise the post becomes a reply with the open thread's `ts`. Optimistic append for replies (the next poll tick re-syncs from Slack's view), full state reset + sessions reload for the new-session pivot. Test plan: `0012-slack-send.md`.

- **11.3.1 — Reaction display.** Render the `reactions` array on each message as a row of small `<emoji> <count>` chips below the message body and above any action buttons. Slack shortcodes go through the existing `slackEmoji.resolveReactionName` helper (which strips `::skin-tone-N` modifiers and falls back to `:name:` for custom workspace emoji). Read-only — tapping a chip doesn't toggle the user's own reaction yet; that needs an emoji picker and lives behind the next concrete request.
- **11.4 — Multi-bot + polish.** Configurable bot profiles (Moonbot is the default; user adds Cursor / GitHub / any DM-able bot by handle). Tab strip inside the panel when there's more than one.

Deliberately deferred until somebody asks (see [`slack-chat.md`'s deferred-features section](slack-chat.md#what-this-phase-deliberately-doesnt-do) for the per-feature design notes):

- **File / image attachments.** Both inbound rendering (bots posting screenshots / log dumps) and outbound upload (drag-and-drop into the composer). Scopes already granted; the missing piece is a `files[]` renderer + a paste/drop handler.
- **Auto-scroll to the latest message.** Threads currently open scrolled to the oldest reply. Tabled until the team picks bottom-anchor vs. last-read-marker as the default.
- **AI-generated session titles.** Replace the raw-first-line preview with a 3–6 word LLM summary; bring back a sticky thread header above the message list once the summary is good enough to anchor the view.
- **OAuth flow that ships a moon-ide Slack app.** Today the user installs their own personal app; replacing that with a one-click "Sign in with Slack" needs a hosted callback we don't have.
- **Local IDE context for the bot.** Phase 11 is pure pass-through. ACP (Phase 6) is where context-aware agents live.

## Phase 12+ — Innovation track

Open-ended. Examples:

- Inline AI ghost text + accept/reject UI
- Agent-driven multi-file diffs with batch acceptance
- Cross-repo refactor planner
- Custom WebGL git lane renderer
