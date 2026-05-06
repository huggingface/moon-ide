# Test plan 0055: Open session trace in editor

- **Date**: 2026-05-06
- **Phase**: 6.x (coder polish) — small UX add, but it crosses the IPC boundary and re-uses the host-direct file mechanism, so it gets its own plan.

## What shipped

- New "Open trace" affordance (the `</>` icon) in three places in the coder panel: each row in the sessions list, the active-session header (next to "+ new"), and the sub-agent pop-out header. Clicking it opens the raw JSONL transcript in the editor as an `isExternal` buffer — same machinery as `Ctrl+O` for files outside the active folder.
- New Tauri command `coder_session_jsonl_path(id)` resolves a session id (parent or sub-agent) to the absolute on-disk JSONL under the active folder's slug. Errors `not found` for empty / never-persisted sessions; the panel surfaces that as a flash.
- Sessions list rows: the trace icon and the trash icon are both opacity-0 by default and fade in on hover / focus-within. Renamed the CSS class from `.session-delete` to `.session-row-action` so both share the visibility rule without duplication.
- Container parity: the trace lives on the host's `XDG_DATA_HOME` (under `<XDG_DATA_HOME>/moon-ide/coder-sessions/<folder-slug>/<id>.jsonl`), so even when the active folder is running in a container the file opens through `fs_read_file_host` — no docker exec round-trip.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a folder open as the active workspace, and at least one persisted coder session in that folder.

### Sessions list — open the trace of a non-active session

1. Open the coder panel. Hit the "back to sessions" arrow (`☰`) so the list view is showing.
2. Hover any session row. Two icon buttons fade in on the right side: the new `</>` (open trace) and the existing trash.
3. Click `</>`.
4. Expected: a new editor tab opens with the raw JSONL of that session. Tab label is `<session-id>.jsonl`. The buffer contains one JSON object per line (header, then `user` / `assistant` records). The clicked session is **not** opened in the panel — only its trace.
5. Edit the file (add a stray character), `Ctrl+S`. Expected: bytes land in the JSONL on disk; the dirty marker clears. (The session is decoupled from the live in-memory state, so editing the file doesn't corrupt anything that's currently mounted — at worst a future `coder_open_session` will fail to parse the bad line and surface a `tracing::warn!`. This is intentional: the trace is for inspection, not editing, but we don't lock it.)
6. Close the trace tab. The session's row in the panel is unaffected.

### Active-session header — open the trace of the session you're in

7. Click any session in the list to mount it. The session header bar shows: `←` (back), title, `</>` (new), `+` (new session).
8. Click `</>`. Expected: the JSONL of the **active** session opens in a new editor tab.
9. Send a fresh prompt in the panel ("hello"). After the turn completes, switch focus back to the trace tab and reload it (close + re-open via the `</>` button, since the buffer doesn't auto-refresh on disk changes by design).
10. Expected: the new lines (additional `user` and `assistant` records, plus any `token_usage` / tool-call records) are visible at the bottom.

### Sub-agent pop-out — open a sub-agent's trace

11. Trigger a sub-agent (e.g. ask the parent agent to "spawn a sub-agent to summarise…"). When the inline sub-agent card appears in the parent's transcript, click it to enter the pop-out view (`coder.view = 'subagent'`).
12. The sub-agent header shows: `← Back`, label `Sub-agent · <folder>`, mode pill, `</>`.
13. Click `</>`. Expected: the sub-agent's own JSONL opens. Note this lives under the **parent folder's slug**, but the file is the sub-agent id (e.g. `sess-…-sub-…jsonl`).

### Empty / blank session guard

14. Hit `+ new session` in the panel header to create a blank session. Don't send anything. Click `</>` in the header.
15. Expected: a flash toast like `Could not open trace: session jsonl not found: …`. No new editor tab opens. (Empty sessions never persist, by design — see `runner.rs::send` "first send allocates the file".)

### Container sanity (when running the project in a container, Phase 2)

16. Active folder is running inside a container. From either the sessions list or the active-session header, click `</>`.
17. Expected: the trace opens normally. The read routes through `fs_read_file_host` (`tokio::fs` directly on the host), bypassing the container's `WorkspaceHost`. Save also lands on the host's `XDG_DATA_HOME`.

### No-folder guard

18. Close every folder so the welcome screen shows. (Side-effect: the coder panel is empty; there's no way to click `</>` because the panel doesn't render trace buttons without a session.)
19. Sanity-only: backend `coder_session_jsonl_path` would error with `NoActiveFolder`; no UI path exists to call it without a session, so this is just a backend invariant.

## What must keep working

- All other coder-panel buttons: `+ new`, `← back to sessions`, sign out, the trash icon on each row, attachments, etc.
- `Ctrl+O` itself — the new command piggy-backs on `workspace.openHostFile` and didn't change that codepath.
- `coder_list_sessions`, `coder_open_session`, `coder_delete_session` — all unchanged. The new IPC is additive.
- LSP / git / persistence skip rules for `isExternal` buffers — JSONL traces inherit them, so opening a trace shouldn't trigger an LSP `didOpen` for a JSONL file or a git status reload.

## Known limitations

- The trace tab is a normal `isExternal` buffer; it does not auto-reload when new turns append to the JSONL. To see fresh bytes, close + re-open via the `</>` button. (Live tail would be a separate feature.)
- The trace is editable. We don't make it read-only because the cost of the edit guard isn't worth it for a power-user inspection tool — see step 5 for the rationale. Revisit if it bites someone.

## Related

- Specs: [coder.md](../coder.md) — session JSONL format and persistence.
- Prior test plans: [0051-open-host-file.md](0051-open-host-file.md) (the host-direct file mechanism this re-uses), [0054-token-usage-and-auto-compaction.md](0054-token-usage-and-auto-compaction.md) (the most recent coder-panel test plan).
