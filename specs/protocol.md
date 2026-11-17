# Protocol

STATUS: partial — Phase 0 ships fs operations via Tauri commands. Streams (`fs.watch`) and the JSON-RPC framing for remote mode land in Phase 2.

## Source of truth

[`crates/moon-protocol/`](../crates/moon-protocol/) is the single source of truth. Every method, every type, every event lives there. The Svelte UI's TS types in `src/lib/protocol.ts` mirror it.

## Transport

- **In-process (Phase 0)**: Tauri commands. The Svelte UI calls `invoke('fs_read_dir', { path })` etc.
- **Cross-process (Phase 2+)**: JSON-RPC 2.0 framed over a Unix socket. Same method names (snake_case in Rust, camelCase exposed as Tauri command). Notifications used for streams.

## Categories

### `workspace.*`

- `workspace.openFolder({ path }) -> WorkspaceId` — register a folder as the active workspace.
- `workspace.list() -> Workspace[]` — for multi-repo (Phase 7).
- `workspace.activeId() -> WorkspaceId | null`

### `fs.*`

- `fs.readDir({ path }) -> DirEntry[]`
- `fs.readFile({ path }) -> { bytes, encoding, mtimeMs }`
- `fs.writeFile({ path, bytes }) -> { mtimeMs }`
- `fs.stat({ path }) -> Stat`
- `fs.revealInFolder({ path })` — host-side: opens the desktop file manager with the (absolute) path selected via `tauri-plugin-opener`. Not a `WorkspaceHost` method; the file manager always runs on the host. See [ADR 0051](decisions/0051-tab-rename-reveal-live-previews.md).
- `fs.watch({ path }) -> SubscriptionId` (event stream `fs.event`)
- `fs.unwatch({ subscriptionId })`

### `editor.*` (later phases)

- `editor.openFile({ path }) -> EditorId`
- `editor.save({ id })`
- `editor.close({ id })`

### `pr.*` (ADR 0113)

- `pr_current() -> PullRequestDetail | null` — the open GitHub PR for the active folder's branch: description (minus moonbot's notes block), state, review decision, checks, the parsed notes and the review threads. `null` when there is no PR, the remote isn't GitHub, or `gh` can't answer.

- `pr_open({ number }) -> PrOpened` — pull request `number` of the active folder's repository, without a checkout ([ADR 0115](decisions/0115-pr-tab-without-a-checkout.md)): fetches its head into `refs/moon/pr/<number>` and returns the detail, the merge base with the base branch, and the files changed between the two. Both revisions can then be read with `fs_git_ref_content`. Errors when the fetch fails or `gh` cannot describe the PR.
- `fs_publish_pr_review`'s request takes an optional `prNumber`: the review goes to that PR instead of the current branch's.

Shapes: [`crates/moon-protocol/src/review.rs`](../crates/moon-protocol/src/review.rs).

### `inbox.*` (ADR 0112)

- `inbox_fetch({ watchedAuthors }) -> InboxSnapshot` — one poll of GitHub through the host's `gh`, limited to the bound folders' GitHub repositories (echoed in `repos`; empty means nothing was asked). Never errors for GitHub's sake: `status` reports a missing or logged-out `gh`, and `failures` lists the sources whose rows are absent from this snapshot.
- `inbox_mark_read({ threadId })` — marks a notification thread read.
- `inbox_open_pr({ folderPath, branch }) -> { workspace, folderPath }` — makes `branch` available for review without moving the project's checkout: returns the bound worktree already on it, the project itself when it is on it, or a new worktree under `<project>/.worktrees/`. `folderPath` must be a project folder, not a worktree.

Shapes: [`crates/moon-protocol/src/inbox.rs`](../crates/moon-protocol/src/inbox.rs).

### `git.*` (Phase 5), `lsp.*` (Phase 4), `coder.*` (Phase 6), `term.*` (Phase 3), `lint.*` (Phase 8)

Defined per-phase. Each follows the same pattern: requests with structured params, streaming events for long-lived subscriptions.

## Events

Pushed by the core to the UI. In Tauri this maps to events; in remote mode to JSON-RPC notifications.

- `fs.event` — `{ subscriptionId, kind: 'create'|'modify'|'remove'|'rename', path }`
- `lsp.diagnostics` (Phase 4)
- `term.output` (Phase 3)
- `coder.event` (Phase 6 — every loop event: `agent_start`, `turn_start`, `message_*`, `tool_execution_*`, `turn_end`, `agent_end`, `error`)
- `coder.sync_state` (Phase 6 — bucket-sync status pip)

## Error model

All requests return `Result<T, MoonError>`:

```ts
type MoonError =
	| { code: 'NotFound'; message: string }
	| { code: 'IoError'; message: string }
	| { code: 'PermissionDenied'; message: string }
	| { code: 'HostUnavailable'; message: string } // remote host disconnected
	| { code: 'Internal'; message: string };
```

UI components must handle `HostUnavailable` gracefully (Phase 2+).

## Versioning

Single integer `protocol_version` advertised by the core. The agent must match. Bump on breaking changes; describe in CHANGELOG.
