# Test plan 0043: coder sessions on disk + auto-rename

- **Date**: 2026-05-05
- **Phase**: 6.3 — Sessions on disk

> Superseded by [test plan 0044](0044-coder-polish.md), which
> moved sessions out of the project tree into
> `<XDG_DATA_HOME>/moon-ide/coder-sessions/<project-slug>/`.
> The paths in this plan describe the location that shipped at
> 6.3 time and was replaced soon after; rerunning this plan
> verbatim against current builds will not find files there.

## What shipped

- Per-workspace sessions persisted as JSONL under
  the project's source tree (`agent-sessions/<id>.jsonl`).
  Header line carries `schema, id, title, created_at_ms,
updated_at_ms, model`; body lines are tagged `SessionRecord`s
  (`user`, `assistant`, `tool`, `title_update`).
- Lazy persistence — empty sessions never write a file, the
  header lands on the first record append.
- Two-view panel: a sticky `← Sessions | title | +` strip in
  session view, a sessions list view with hover-revealed
  delete + confirm dialog. View choice lives in
  `coder.view` and is hydrated at startup from
  `AppState.coder.last_session_id`.
- New Tauri commands: `coder_list_sessions`,
  `coder_active_session`, `coder_new_session`,
  `coder_open_session`, `coder_delete_session`.
- Event vocabulary additions: `session_loaded`,
  `session_title_updated`, `session_list_changed`.
- Auto-rename: after the _first_ turn of a fresh session the
  runner spawns a fast-model call asking for a 4-6 word title.
  Result replaces the truncated-prompt fallback in memory and
  on disk (as a `title_update` record), and an event nudges the
  UI to update the sticky header + sessions list.

## How to test

Prerequisites: `bun install`, `bun run dev`, signed in to
Hugging Face (per test plan 0039), an active workspace folder
that the IDE can persist sessions for.

### Lazy persistence + first turn

1. Open the coder panel, hit the `+` icon to start a fresh
   session. Expected: empty transcript with the placeholder
   "Send a prompt to start." line. No JSONL file for the
   session in the sessions directory yet (see superseded note
   above for the path this plan originally targeted).

2. Send `summarise the structure of crates/moon-coder in 3
sentences`.

   Expected:
   - The transcript fills in via streaming as before.
   - A new file appears in the sessions directory named
     `sess-<...>.jsonl`. First line is the header with `title`
     set to the truncated prompt; each event in the turn lands
     as one body line.
   - In the panel's sticky session-bar, the title shows the
     truncated prompt.

3. Wait a few seconds for the auto-rename pass.

   Expected:
   - The session-bar title updates to a 4-6 word summary
     (something like "Summarise moon-coder crate structure").
   - On disk: `tail -1 <session-file>` shows a
     `{"kind":"title_update","title":"…"}` record.

### Multi-session list + back navigation

4. Click `← Sessions` in the session-bar.

   Expected: sessions list view, one row showing the
   auto-rename title, "just now" / "1m" relative timestamp.

5. Click the row.

   Expected: returns to the session view, transcript replays
   the previously-saved messages (user + assistant + any
   tool-result pairs). The composer is enabled.

6. Click `+` to start a fresh session, send a different prompt
   (e.g. `list every Tauri command we register`).

   Expected: a second `.jsonl` file appears; a second row
   lands in the sessions list.

7. Switch between the two via `← Sessions` → click → `←
Sessions` → click. Expected: each opens in <500 ms; the
   transcript replays correctly each time.

### Persistence across launches

8. With session B active, quit the app (Ctrl+Q) and relaunch.

   Expected: panel mounts directly into session B (the
   relaunch path reads `AppState.coder.last_session_id`).
   Transcript is intact.

9. Send a follow-up prompt in session B. Quit + relaunch.

   Expected: still in session B, follow-up visible.

### Switching workspace folders

10. Open a second workspace folder via the sidebar / command
    palette.

    Expected:
    - The sessions list shows that folder's sessions only
      (probably empty for a fresh checkout).
    - If the previously-active session id doesn't exist in
      the new folder, the panel falls back to the sessions
      list view (or empty session view if the list is also
      empty). No error toast.

11. Switch back to the first workspace. Expected: sessions A
    and B still visible.

### Delete

12. Hover a session row. Expected: a trash icon fades in on
    the right of the row.

13. Click the trash icon. Expected: confirm dialog
    `Delete session "<title>"? This cannot be undone.`

14. Confirm. Expected:
    - Row disappears from the list within ~200 ms.
    - The on-disk JSONL file is gone.
    - If the deleted session was the active one,
      `AppState.coder.last_session_id` is cleared and the
      session view goes back to "send your first message".
    - `session_list_changed` events ping any other windows
      pointed at the same workspace (open a second window for
      this check).

### Auto-rename failure paths

15. Disable network (drop wifi / `sudo pfctl -e`-style block).

16. Send a prompt and wait for the streaming to finish. The
    auto-rename call will fail.

    Expected:
    - The panel keeps the truncated-prompt title — no error
      toast, no spurious empty title.
    - `tracing` logs an `info!` line with the rename failure
      reason.
    - On disk, no `title_update` record is appended.

17. Re-enable network and send a fresh prompt in a _new_
    session. Auto-rename should run again on the new session.
    The previous session keeps its truncated-prompt title;
    auto-rename only fires on the first turn per session.

### Schema-change resilience

18. Edit the on-disk JSONL header to bump `schema` to a
    nonsense value (`5`). Reopen the session. Expected: the
    panel falls back to the sessions list (parse-failure path
    in `load_summary` logs a warn, the unreadable file is
    skipped).

19. Restore the schema. Expected: the session reappears in
    the list.

## What must keep working

- `coder_send` against a workspace with **no** active folder
  surfaces `NoActiveFolder` instead of writing to a phantom
  path.
- Existing streaming behaviour (test plan 0042) is unchanged.
  Deltas, abort, thinking blocks, tool dispatch, all still
  work.
- HMR reloads of the frontend re-hydrate the active session
  via `coder_active_session` rather than dropping into the
  list view (the runner's session is in-process, not in the
  webview).
- `app_state_save` from the frontend session-persist path
  preserves `coder` from disk verbatim — same merge rule that
  protects `slack` and `right_panel`.

## Known limitations

- `last_session_id` is flat, not per-folder. If you bounce
  between two folders frequently the relaunch picks one
  arbitrarily; the panel falls back to the list view when
  the id doesn't resolve. Cheap to upgrade later.
- Session ids are timestamp + 32-bit pseudo-random suffix,
  not UUIDv7 / ULID. Within one millisecond two `+` clicks
  could in principle collide; the suffix makes that
  vanishingly unlikely. If we ever observe a collision in
  practice, swap in `uuid` / `ulid`.
- `title_update` records are an append, not a header
  rewrite. A session file's header still shows the original
  truncated title; the on-load code applies the latest
  `title_update` record over it. Visually identical, but
  worth knowing if you're `head -1`-ing the file.
- No "rename session" UI yet (manual override). Add when
  needed.
- The sessions list shows just the title + relative time. No
  preview of the last assistant line, unlike the Slack
  panel. Add if it matters; keeps the row small for now.

## Related

- Specs: [`specs/coder.md`](../coder.md#sessions),
  [`specs/coder.md`](../coder.md#auto-rename).
- Roadmap:
  [`specs/roadmaps/phase-06-coder.md` § 6.3](../roadmaps/phase-06-coder.md#63--sessions-on-disk--auto-rename--done-todo_write-deferred).
- Prior test plans:
  [0042-coder-streaming.md](./0042-coder-streaming.md),
  [0041-right-panel-single-slot.md](./0041-right-panel-single-slot.md),
  [0040-coder-write-tools.md](./0040-coder-write-tools.md),
  [0039-coder-skeleton.md](./0039-coder-skeleton.md).
