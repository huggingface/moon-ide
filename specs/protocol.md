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
- `fs.watch({ path }) -> SubscriptionId` (event stream `fs.event`)
- `fs.unwatch({ subscriptionId })`

### `editor.*` (later phases)

- `editor.openFile({ path }) -> EditorId`
- `editor.save({ id })`
- `editor.close({ id })`

### `git.*` (Phase 5), `lsp.*` (Phase 4), `coder.*` (Phase 6), `term.*` (Phase 3), `lint.*` (Phase 8)

Defined per-phase. Each follows the same pattern: requests with structured params, streaming events for long-lived subscriptions.

### `next_edit.*` (local llama.cpp)

- `next_edit_probe({ baseUrl }) -> NextEditProbeResult` — `GET {baseUrl}/health` (llama.cpp server). Surfaces `ready` (200), `model_loading` (503), `unreachable` (connection/timeout), or `error`.
- `next_edit_complete({ params }) -> NextEditCompleteResult` — builds a Sweep-style prompt (original / current / updated 21-line windows; see [Sweep next-edit post](https://blog.sweep.dev/posts/oss-next-edit)) and `POST {baseUrl}/completion`.
- `next_edit_server_start({ params }) -> NextEditServerSnapshot` — spawns `llama-server` with `--host`, `--port`, `--hf-repo` (HF weights download on first run). Managed child lives in [`AppState`](src-tauri/src/state.rs) (`next_edit_server`); IDE exit runs `stop_all` → `SIGKILL`/`wait` the child.
- `next_edit_server_stop() -> NextEditServerSnapshot` — kills the managed child if any.
- `next_edit_server_status() -> NextEditServerSnapshot` — running flag, optional pid / last exit code, start error, tail of stdout/stderr lines.

Spawn settings persist in [`AppState.next_edit`](crates/moon-protocol/src/app_state.rs) (`llama_binary`, `hf_repo`, `server_host`, `server_port`, optional `external_base_url`, **`server_autostart`**: managed mode only — true after **Start**, false after **Stop**; next IDE launch calls `next_edit_server_start` when autostart is on and `external_base_url` is empty). When `external_base_url` is empty, probes and `next_edit_complete` use `http://{server_host}:{server_port}` (default port **53281**, IANA dynamic range). When non-empty, that URL overrides the listen address for HTTP. In the editor, **Ctrl+T** (or the palette) calls `next_edit_complete` and **replaces the model’s line range in the buffer** (not CodeMirror completion). **Ctrl+Space** is LSP-only. The status bar **Autocomplete** control can show while `next_edit_complete` is in flight. `hf_repo` defaults to **sweepai/sweep-next-edit-1.5B** unless overridden.

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
