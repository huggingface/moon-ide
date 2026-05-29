# 0090 — Coder per-session host-mode override

Date: 2026-06-09

## What shipped

- A per-session escape hatch that pins the coder's `bash` / shell
  tool to the **host** machine even while the workspace runs in a
  container — for diagnosing host-side Docker / networking from an
  agent. Two states only: Auto (default) and Force host.
- The override lives on `SessionHeader.bash_target_override`
  (persisted as the wire string `"host"`, omitted when auto), is
  snapshotted per turn, and flows into `resolve_bash_target`, the
  system prompt's bound-folder path advertising, and
  `CoderStatus`.
- The coder panel header's bash-target pip is now a button opening
  an Auto / Force-host popover, with an off-default badge when a
  session is forced.
- File tools and format-on-save are intentionally unaffected (see
  Known limitations). Fresh sessions and sub-agents always start
  Auto.

## How to test

Prerequisites: a workspace with a `compose.yaml` whose shell
container is **Running** (so Auto resolves to container), the
coder signed in, and an active folder.

1. **Baseline (Auto = container).** Open the coder panel. The
   bash-target pip in the header shows the container glyph
   (green). Hover: tooltip says "bash and shell tools run inside
   the workspace container. Click to change." Send a turn with
   `run a bash command: hostname`. The `bash` tool result footer
   reads `· container`.

2. **Open the popover.** Click the pip. A two-row popover appears:
   **Auto** (selected, radio filled) with sub-label "Currently:
   container.", and **Force host**. Press Escape — it closes.
   Click the pip, then click outside — it closes.

3. **Force host.** Click the pip → click **Force host**. The
   popover closes. The pip flips to the host glyph, picks up the
   warning tint, and shows a small dot badge. Hover: "Forced to
   host mode — bash runs on the host, not the container. Click to
   change."

4. **Verify bash relocated.** Send `run bash: cat /etc/hostname &&
docker ps --format '{{.Names}}' | head`. Expected: the command
   runs on the **host** (the hostname differs from step 1's
   container hostname, and `docker ps` lists the host's containers
   — including `moon-ws-…-dev-1` — which is exactly what you can't
   see from inside the dev container). The tool result footer
   reads `· host`.

5. **Verify the system prompt followed.** Ask the agent "what
   absolute path should you use to address the <folder-name>
   folder?" In force-host mode it should answer with the real
   **host** path (e.g. `/home/you/code/<folder>`), not
   `/workspace/<folder>`. (Optionally confirm via the network
   panel that the system prompt's "Bound folders" section lists
   host paths.)

6. **Persistence across reload.** With the session forced to host,
   send at least one message (so the session persists). Then
   reload the webview (or quit + reopen the IDE) and reopen the
   same session from the sessions list. The pip should come back
   **forced to host** (badge present), and a new `bash` call still
   runs on the host. Inspect the JSONL header on disk
   (`coder_session_jsonl_path` → open it): line 1 contains
   `"bash_target_override":"host"`.

7. **Back to Auto.** Click the pip → **Auto**. Pip returns to the
   container glyph, badge gone. A `bash` call routes to the
   container again (`· container`). The on-disk header no longer
   contains `bash_target_override`.

8. **Fresh session starts Auto.** With the current session forced
   to host, click the **+** (new session). The new session's pip
   shows Auto (container) — the override did **not** inherit.

9. **Concurrent sessions are independent.** Force session A to
   host. Start a turn in A (`sleep 10; hostname`). While it runs,
   open/start session B in the same folder and confirm B's pip is
   Auto (container) and a `bash` in B runs in the container while
   A's still runs on the host.

10. **Sub-agents stay Auto.** From a force-host session, have the
    agent spawn a `task` sub-agent that runs `bash: hostname`. The
    sub-agent's bash runs in the **container** (Auto) — the forced
    parent does not leak. Confirm via the sub-agent's tool result
    footer / its JSONL header (no `bash_target_override`).

## What must keep working

- With no override set, `bash` routing is byte-identical to before
  (container when Running, host otherwise), and the pip is
  read-through-correct. Existing on-disk session headers (which
  predate the field) load fine and re-serialise without spuriously
  gaining the key.
- File tools (`read_file` / `write_file` / `edit_file`) work the
  same in both modes — they're host-direct via the bind mount.
- `cargo test -p moon-coder` green, including the new
  `bash_target_override_omitted_when_none_and_round_trips_force_host`,
  `rewrite_header_updates_first_line_and_preserves_body`, and
  `rewrite_header_is_a_noop_for_unpersisted_session`.

## Known limitations

- **Format-on-save is not relocated by the override.** It follows
  the global shell resolver and operates on the same bind-mounted
  bytes regardless of where it runs. Re-plumbing
  `WorkspaceHost::format_file` for a per-session flag is a wider
  change than this escape hatch warrants; revisit only if a
  formatter binary present in one place but not the other actually
  bites. (ADR 0022.)
- **No `ForceContainer`.** Auto already prefers the container when
  it's up; forcing a down container only errors. Add it if a
  concrete need appears.
- **Mid-turn toggles apply next turn.** The override is
  snapshotted at turn start (same as model picks), so flipping it
  while a turn is running doesn't relocate in-flight commands.

## Related

- [ADR 0022 — Per-session host-mode override for the coder's
  `bash`](../decisions/0022-coder-host-mode-override.md)
- [ADR 0016 — coder concurrent
  sessions](../decisions/0016-coder-concurrent-sessions.md)
- [`specs/coder.md`](../coder.md) — `bash` tool routing
- Test plan
  [0079 — coder host paths](0079-coder-host-paths-and-task-rename.md)
  (the system-prompt path-advertising plumbing this reuses)
