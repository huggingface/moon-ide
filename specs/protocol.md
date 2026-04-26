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

### `git.*` (Phase 5), `lsp.*` (Phase 4), `acp.*` (Phase 6), `term.*` (Phase 3), `lint.*` (Phase 8)

Defined per-phase. Each follows the same pattern: requests with structured params, streaming events for long-lived subscriptions.

## Events

Pushed by the core to the UI. In Tauri this maps to events; in remote mode to JSON-RPC notifications.

- `fs.event` — `{ subscriptionId, kind: 'create'|'modify'|'remove'|'rename', path }`
- `lsp.diagnostics` (Phase 4)
- `term.output` (Phase 3)
- `acp.message` (Phase 6)

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
