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
| 12    | Innovation track                | open-ended  |
| 13    | Mobile companion                | planned     |

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

**Acceptance**: opening a folder adds it to the workspace as a new folder bar in the sidebar instead of replacing the active workspace; clicking a bar makes it active and swaps the file tree + tabs to that folder's persisted state; an inline `+ Add folder` row picks a new folder; an `×` per bar removes it (with confirm) including its session entry; per-folder tab/active state survives restart. One workspace (`"default"`) with N folders — multi-workspace UI stays a Phase 7 concern (see [`roadmaps/phase-07-multi-workspace.md`](roadmaps/phase-07-multi-workspace.md)).

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

`gix`-based status / blame / diff plus a focused SCM panel. Tree decorations ride Pierre's built-in `gitStatus` indicators (gitignored entries faded; modified / added / deleted / untracked all surface, deleted rows stay visible until committed). Inline blame on the caret line (CM6 widget). Diff view via `@codemirror/merge` with the working-tree side editable, single-tab toggle (`Source` / `Preview` / `Diff`). The SCM panel handles commit / amend / sync / publish / revert with split-button toggles, AI-suggested commit messages and branch names, and periodic background `git fetch`.

Sub-phase work breakdown, tree-marker contract, what's landed, and outstanding work: [`roadmaps/phase-05-git.md`](roadmaps/phase-05-git.md).

**Acceptance** (per sub-phase): tree markers via Pierre's `gitStatus` + porcelain status backing + auto-refresh + per-row Discard (5.0); inline blame at the caret with author + relative date + tooltip (5.1); `@codemirror/merge` diff view + single-tab toggle + git-change gutter + scrollbar overview ruler (5.2); SCM panel — branch label, change pill, revert-all, periodic auto-fetch, split commit button with branch + amend toggles, amend prefill, AI commit-message + branch-name sparkles, sync / publish spinners (5.3); `Ctrl+Shift+F` skips `.git/` explicitly while still respecting user `.gitignore` (5.4); merge-conflict resolution — `Conflicted` row state, `Merging <ref>` panel reshape, in-buffer accept widgets, auto-stage on save, abort merge, soft-warn on residual marker text (5.6). Deferred (per-hunk stage / discard, unstage of staged-new, guided pull / push failure recovery) — see [`roadmaps/phase-05-git.md` § "Still outstanding"](roadmaps/phase-05-git.md#still-outstanding).

## Phase 6 — Coder (in-process AI agent)

A right-side coder panel that owns its loop end-to-end: streams from Hugging Face Inference Providers, dispatches its own tool calls, routes every tool through the active `WorkspaceHost` so a containerised workspace gets a containerised agent for free. Sessions persist as append-only JSONL and sync to a per-user private HF bucket (`<user>/moon-ide-sessions`) via `hf-xet`.

Architectural spec: [`coder.md`](coder.md). Sub-phase work breakdown: [`roadmaps/phase-06-coder.md`](roadmaps/phase-06-coder.md). Decisions: [ADR 0010 — coder rewrite, not ACP](decisions/0010-coder-rewrite-not-acp.md), [ADR 0011 — rename `moon-agent` → `moon-remote`](decisions/0011-rename-moon-agent-to-moon-remote.md).

**Acceptance** (per sub-phase): HF OAuth device-flow sign-in + read-only tool surface (6.0); SSE streaming + abort (6.1); mutating tools + container-aware bash (6.2); on-disk JSONL sessions + sidebar (6.3); model picker (6.4); steering + follow-up queues (6.5); `AGENTS.md` / `SYSTEM.md` / `SKILL.md` system-prompt assembly + compaction (6.6); per-user private HF bucket sync via `hf-xet` (6.7). Deliberately deferred (sub-agents, OpenRouter / custom providers, Anthropic OAuth, bucket browser, MCP, plan mode, permission popups) — see [`coder.md` § "Out of scope"](coder.md#out-of-scope-explicitly).

## Phase 7 — Multi-repo coordination

Two sibling concerns ride this phase number; they ship independently.

**Multi-workspace UI**: today's singleton workspace becomes one of many, **one OS process per workspace** (see [ADR 0014](decisions/0014-process-per-workspace.md)), with `Open Workspace…` / `Switch Workspace…` affordances. Workspace ids are user-set slugs (`huggingface` / `gitaly` / …) so `docker ps`, the per-workspace state dir, and the `--workspace <slug>` CLI line in process listings all stay readable. Sub-phase work breakdown: [`roadmaps/phase-07-multi-workspace.md`](roadmaps/phase-07-multi-workspace.md). The plan there is staged so each step is reviewable on its own — registry id refactor, catalog plumbing, per-workspace session.json + create/delete IPC, process-per-workspace + focus-socket pivot, picker UX, restore-most-recent.

**Cross-folder search**: per-folder `tantivy` indices with a parallel query layer, a `workspace_list` / `workspace_grep` tool surface for the coder so it can target `@folder-name`. Independent of the multi-workspace work and gets its own roadmap doc when it grows past one paragraph.

App state grows with both: per-workspace state moves into `<XDG_DATA_HOME>/moon-ide/workspaces/<id>/session.json`; the `AppState` struct in `moon-core` keeps only per-machine concerns (theme, AI creds, Slack token pointer, the list of known workspaces).

## Phase 8 — Lint / format

oxlint, oxfmt, prettier, eslint as sidecar processes. Debounced. Diagnostics merged with LSP diagnostics in a single problems panel. Format on save with per-language chooser.

**What has landed early** (see `specs/test-plans/0047-*.md`, `specs/test-plans/0063-*.md`, [ADR 0012](decisions/0012-format-on-save.md), and [ADR 0013](decisions/0013-format-on-save-file-based.md)): format-on-save itself, pulled forward as a bootstrap concern. Driven by the project's `.lintstagedrc.json` / `package.json#lint-staged`, runs through `WorkspaceHost::save_file` as the second stage of the editorconfig pre-save pipeline. Each command in the chain runs against the on-disk file (file path appended as the last positional arg, command mutates in place) — same shape `bun run lint-staged` uses on commit, so any tool the team's lint-staged map names works. There's no per-language chooser UI — lint-staged's map is the picker. The remaining Phase 8 surface (debounced lint diagnostics, problems panel, format-on-save toggle if anyone asks) stays scaffolded.

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

## Phase 12 — Innovation track

Open-ended. Examples:

- Inline AI ghost text + accept/reject UI
- Agent-driven multi-file diffs with batch acceptance
- Cross-repo refactor planner
- Custom WebGL git lane renderer

## Phase 13 — Mobile companion

A phone companion that drives a running moon-ide over the LAN / VPN: run and steer coder sessions, and review + commit. Not a mobile IDE. A single host-resident `moon-bridge` daemon discovers running workspace processes by enumerating their per-workspace `instance.sock` files (the multi-workspace answer falls straight out of [ADR 0014](decisions/0014-process-per-workspace.md)) and relays the coder + git surface to the phone over the same JSON-RPC framing `moon-remote` uses — so the cloud / always-on future is a transport swap, not a second network transport. The app is an installable Svelte 5 PWA the bridge serves; native (Tauri mobile) is a deliberate future, not v1.

Architectural spec: [companion.md](companion.md). Sub-phase work breakdown: [roadmaps/phase-13-mobile-companion.md](roadmaps/phase-13-mobile-companion.md). Decision: [ADR 0023 — mobile companion via `moon-bridge`](decisions/0023-mobile-companion-bridge.md).

**Acceptance** (per sub-phase): `moon-bridge` crate + `instance.sock` workspace discovery (13.0); bridge ↔ process JSON-RPC relay over `moon-remote` framing (13.1); LAN HTTPS + WebSocket listener with self-signed TLS (13.2); TOFU-cert + device-token pairing with QR + revoke (13.3); PWA coder surface — workspace switcher, session run / steer / abort (13.4); PWA review & commit over the existing git layer (13.5). Deferred (full editing / terminal / LSP on phone, background-screen-off watching, detached overnight runs, multi-account, public-internet exposure, Windows host bridge) — see [`companion.md` § "What this deliberately doesn't do (v1)"](companion.md#what-this-deliberately-doesnt-do-v1).
