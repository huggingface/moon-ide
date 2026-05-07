# Test plan 0044: coder polish — sessions location, linkify, tab-size, panel chrome

- **Date**: 2026-05-05
- **Phase**: 6.3 follow-up

## What shipped

- Sessions moved out of `<workspace>/.moon/agent-sessions/`
  (project tree) into
  `<XDG_DATA_HOME>/moon-ide/coder-sessions/<slug>/` (global data
  dir). Slug is `<basename>-<FNV-1a hex>` so two folders sharing
  a basename get distinct directories. **Per AGENTS.md "no
  premature migrations": old in-project sessions are not
  migrated; they stay where they are and the panel doesn't see
  them.**
- Auto-spawn a terminal in the bottom panel when `Ctrl+J` opens
  it from an empty state. Container terminal when the workspace
  shell is up, host terminal otherwise — same picker the
  startup-time auto-spawn uses.
- Empty assistant bubble after a thinking-only / tool-only turn
  no longer renders (the visibility check now trims whitespace
  before counting).
- Raw URLs in coder messages are clickable. Implemented as a
  second `markdown-it` instance with `linkify: true`; file
  content rendering keeps `linkify: false` so the markdown
  preview hasn't grown new auto-link surprises.
- Code blocks in rendered markdown + Slack `<pre>` snippets use
  `tab-size: 2` instead of the browser default of 8. Matches
  the team's display-width preference (ADR 0004) and reads
  better for the dense code the model tends to paste back.
- The in-session header's `← Sessions` text link became a
  muted three-line "list" icon button. The session title is
  now the visual focus on that strip; navigation chrome is
  small icons on the sides.

## How to test

Prerequisites: `bun install`, `bun run dev`, signed in to
Hugging Face (per test plan 0039), an active workspace folder.

### Sessions location

1. Send a prompt in a fresh session.

   Expected: a file at
   `~/.local/share/moon-ide/coder-sessions/<slug>/<session-id>.jsonl`
   exists. Confirm with `ls
~/.local/share/moon-ide/coder-sessions/`. The slug looks like
   `moon-ide-9e3779b1` (basename + 8 hex chars).

   `<workspace>/.moon/agent-sessions/` should **not** exist (or
   only contains pre-migration sessions you can ignore).

2. Open a different workspace folder whose basename matches —
   e.g. clone the same repo to a second path. Send a prompt.

   Expected: a _different_ slug directory appears under
   `coder-sessions/`. The two sessions don't bleed into each
   other's lists.

3. Check `tracing` logs for any "failed to persist user
   message" warnings during normal operation. Expected: none.

### Bottom panel auto-spawn

4. Close the bottom panel (Ctrl+J, then again to confirm it's
   hidden). Confirm there are no terminal tabs visible.

5. Press Ctrl+J.

   Expected:
   - Panel opens.
   - A terminal tab appears within ~200 ms.
   - If the workspace container is running, it's a container
     terminal (target chip says `container`); host otherwise.

6. Press Ctrl+J again.

   Expected: panel closes; the existing terminal stays alive
   in the background.

7. Press Ctrl+J once more.

   Expected: panel reopens, **does not** spawn a second
   terminal — the existing tab is still there.

### Empty bubble fix

8. With a reasoning model selected, send a prompt that the
   model will resolve via tool calls only (e.g. `read AGENTS.md
and just stop`). The model emits thinking + tool calls and
   may return without any natural-language content.

   Expected: in the transcript, the assistant row shows the
   `THINKING` disclosure (collapsed after the message ends) and
   no empty grey rectangle below it. The next row is the
   `TOOL · …` block as usual.

### Linkify

9. Send `give me the moon-ide repo URL: https://example.com/repo`.

   Expected: the URL renders as a clickable link in the
   assistant bubble. Clicking it opens the system browser (not
   the Tauri webview) — same routing as `[text](url)` markdown
   links.

10. Open a markdown file that contains a raw URL (no
    `[]()`-wrapping) in plain text. Switch to Preview.

    Expected: the URL is **not** auto-linked. File-content
    rendering keeps the old behaviour ("the author would have
    used `[]()` if they meant a link").

### Tab-size

11. Ask the agent for a code snippet that uses literal tabs:
    `paste me the first 20 lines of moon-coder/src/runner.rs as a markdown code block`
    (the file is tab-indented per ADR 0004).

    Expected: the rendered fenced code block shows 2-column
    tabs, not 8. Compare to the same content in Cursor / VS
    Code's Markdown preview at default settings — the
    moon-ide rendering is tighter.

12. Same check in the Slack panel: paste a tab-indented snippet
    inside triple-backticks via Slack proper, then look at it
    inside the IDE.

    Expected: the `.codeblock` `<pre>` renders at 2-column tabs.

### Panel chrome

13. Open a session.

    Expected:
    - In the sticky strip: a small three-line "list" icon
      button on the left, the session title centred and
      slightly bolder than before, the `+` button on the
      right.
    - Hover the list icon: tooltip says "Back to sessions";
      the icon takes on the same hover treatment as `+` and
      the sign-out icon.
    - Click it: returns to the sessions list view.

14. From the sessions list, click any session to return to
    session view. The title is the focal element of the
    strip — the back/new buttons fade visually next to it.

## What must keep working

- `app_state_save`'s merge rule still preserves the `coder`
  slice from disk; a relaunch with `last_session_id` set still
  re-opens the right session.
- All 25 `cargo test -p moon-coder` tests pass, including
  `project_slug_is_deterministic_and_disambiguates`.
- `bun run check` and `bun run lint` are clean.
- `MarkdownView.svelte` (the file-content preview) still
  renders without auto-linking raw URLs.

## Known limitations

- Old in-project sessions under `<workspace>/.moon/agent-sessions/`
  aren't migrated — by design (no premature migrations).
  Manually move the files into `~/.local/share/moon-ide/coder-sessions/<slug>/`
  if you actually want to keep them.
- Auto-spawn on Ctrl+J fires only when the panel transitions
  from hidden → visible. Re-focusing an already-open empty
  panel doesn't auto-spawn (the panel was never closed; we
  assume the user actively chose the empty state).
- The `← Sessions` icon is a generic three-line glyph. If we
  add a "tabs" view (per-folder vs all), pick a different icon
  for one of them so they don't collide.
- Context-usage circle around the new-session button (mentioned
  during this round of feedback) is a separate follow-up. The
  next phase that surfaces context-window pressure (compaction,
  6.6) is the right place to slot it in.

## Related

- Specs: [`specs/coder.md`](../coder.md#sessions),
  [`specs/coder.md`](../coder.md#auto-rename).
- Prior test plans:
  [0043-coder-sessions.md](./0043-coder-sessions.md),
  [0042-coder-streaming.md](./0042-coder-streaming.md).
