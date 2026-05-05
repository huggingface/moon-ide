# Test plan 0040: Coder write tools (Phase 6.2)

- **Date**: 2026-05-05
- **Phase**: Phase 6.2 — Coder

## What shipped

- `write_file` tool: overwrites a file (or creates it) through
  `WorkspaceHost::write_file`. Returns `{ path, bytes_written,
mtime_ms }`. Path-traversal outside the workspace folder is
  rejected by `LocalHost::resolve` upstream — the tool inherits
  that for free.
- `edit_file` tool: exact-substring replace through
  `WorkspaceHost::read_file` + `write_file`. Fails when `find` is
  empty, missing, or matches multiple times without an
  `occurrence` arg; surfaces a useful retry hint in the error
  message ("matched N times … include more surrounding context").
- System prompt updated to advertise the new tools and the
  exact-string-match retry pattern. The "edits are not yet
  supported in this phase" line is gone.
- 6.2 sub-phase split in [`phase-06-coder.md`](../roadmaps/phase-06-coder.md):
  mutating tools and container-aware bash both land here.
- `bash` tool now routes through `docker exec -w <container_cwd>
<name> sh -lc <cmd>` when the active folder is a devcontainer,
  reusing `moon-terminal`'s helpers so the framing matches
  terminals and LSP. Result carries a `target` field
  (`"host"` / `"container"`).
- Panel header has a host-vs-container indicator pip next to the
  username; surfaces from `CoderStatus.bash_target` and re-probes
  when the active folder switches.
- Three unit tests for `byte_offsets_of` covering the corner
  cases `edit_file` relies on (multi-match, empty needle, no
  overlap loop).

## How to test

Prerequisites:

- Phase 6.0 working: signed in via HF device flow, the coder
  panel can ask the model a question and get a non-streaming
  reply. (See [test plan 0039](0039-coder-skeleton.md).)
- A scratch workspace folder with at least one source file you
  don't mind getting modified. A clean git worktree is ideal so
  rollback is `git restore`.

Steps:

1. `bun run dev`. Open the workspace, open the coder panel.

2. **Create a new file via `write_file`.**
   Prompt: `Create a file scratch/hello.txt containing exactly
the text "moon coder hello\n" — nothing else.`
   Expected:
   - The agent calls `write_file` with `path="scratch/hello.txt"`
     and the literal content. The collapsible tool block shows
     the args (full content) and the result `{ path: …,
bytes_written: 17, mtime_ms: … }`.
   - The file appears in the file tree on its next refresh; its
     contents match exactly (no extra whitespace).
   - Subsequent prompt "what's in scratch/hello.txt?" gets the
     same content back, confirming the write landed.

3. **Surgical edit via `edit_file`.**
   Pre-create `scratch/edit-target.md` containing:

   ```
   # Title

   Body line one.
   Body line two.
   ```

   Prompt: `In scratch/edit-target.md, change "Body line one." to
"Body line ONE."` Expected:
   - The agent calls `edit_file` with `find="Body line one."`
     and `replace="Body line ONE."`. Result includes `occurrence:
1, total_matches: 1`.
   - The file on disk now has `Body line ONE.` and the rest is
     unchanged byte-for-byte.

4. **Multi-match disambiguation.**
   Pre-create `scratch/dup.txt`:

   ```
   foo
   foo
   foo
   ```

   Prompt: `In scratch/dup.txt, change the second "foo" to "bar".`
   Expected — one of:
   - The agent calls `edit_file` with `occurrence: 2`. The result
     reports `occurrence: 2, total_matches: 3`. Only the second
     `foo` becomes `bar`.
   - The agent first calls `edit_file` without `occurrence`, gets
     a "matched 3 times" error in the tool result, then retries
     with `find` widened (e.g. `find="foo\nfoo\nfoo"` and
     `replace="foo\nbar\nfoo"`). Either retry strategy is
     acceptable — both prove the error is recoverable.

5. **Editing an open dirty buffer.**
   Open `scratch/edit-target.md` in the editor and type a few
   characters but **don't save** (the tab gets the dirty pip).
   Prompt: `Append a new line "Body line three." at the end of
scratch/edit-target.md.`
   Expected:
   - `edit_file` lands. The editor tab reloads its contents from
     disk on the next mtime tick (CodeMirror's existing
     external-change handling). The dirty pip clears.
   - The user's unsaved typing is **gone** — that's the documented
     trade-off in `coder.md` § Tools (the agent owns its writes;
     we don't merge with editor state).

6. **Refusing path traversal.**
   Prompt: `Write the string "x" to ../escape.txt.`
   Expected: the tool result shows an error from
   `LocalHost::resolve` ("path … escapes workspace root"). The
   agent surfaces it ("I can't write outside the workspace
   folder"). No file is created at the resolved location.

7. **`edit_file` against a non-existent file.**
   Prompt: `In scratch/does-not-exist.txt, replace "a" with "b".`
   Expected: tool throws (file not found). Agent reports the
   failure and asks whether to create the file with `write_file`
   instead.

8. **Container-aware bash (host folder).**
   With a host-mounted workspace folder open, the panel header
   shows a subdued `host` pip next to the username. Prompt:
   `Run "uname -a" via bash and tell me the kernel.`
   Expected: tool result's `target` is `"host"`; `stdout` is the
   user's host kernel (matches `uname -a` from a host shell).

9. **Container-aware bash (devcontainer folder).**
   Open a workspace folder backed by `moon-container` (devcontainer
   running). The panel-header pip flips to an accent-tinted
   `container` pip without manual refresh. Prompt:
   `Run "cat /etc/os-release | head -1" via bash and report the
distro.`
   Expected: tool result's `target` is `"container"`. `stdout`
   shows the container's distro line (e.g. `PRETTY_NAME="Debian
GNU/Linux 12 (bookworm)"` for `moon-base`), **not** the host
   distro. Confirms `docker exec` routing with the
   `/workspace/<basename>` cwd.

10. **Switching folders flips the pip.**
    With both a host and a devcontainer folder bound in the
    workspace, click the folder bar to switch active folder. The
    panel-header pip should toggle between `host` and `container`
    immediately (within one tick). The next bash call follows the
    pip — no stale routing.

## What must keep working

- Read-only tools from 6.0 (`read_file`, `list_dir`, `grep`,
  `bash`) still dispatch and behave identically. Run a quick
  prompt that uses each before mutating anything.
- `cargo test -p moon-coder` passes (the `byte_offsets_of` tests
  guard `edit_file`'s match-counting logic).
- Sign-out / sign-in via the panel header still drops and re-
  acquires the keyring entry.
- The transcript markdown rendering from earlier in 6.x still
  works — the assistant renders fenced code blocks with
  highlighting, no leading-blank-line gap above the bubble.

## Known limitations

- **No undo from the panel.** If the agent edits the wrong file,
  the recovery path is `git restore <path>` (or "Discard
  changes" in the file tree). A panel-level undo is a separate
  feature; it's not in 6.2.
- **No multi-file diff preview.** The agent's `edit_file`
  applies immediately. A proposed-edits / accept-reject UI is a
  later phase if the team asks for it.
- **No `WorkspaceHost::spawn`.** Container routing for `bash`
  goes through `moon-terminal`'s helpers in the tool itself,
  not through a `WorkspaceHost::spawn` trait method. We add
  one when there's a second host implementor
  (`RemoteHost`/`ContainerHost`) to make the abstraction earn
  its keep — until then the inline branch in `tools::bash` is
  the cheaper bookkeeping.
- **`docker exec` doesn't allocate a TTY.** Bash output is
  captured stdout/stderr; commands that depend on `isatty()`
  (curses TUIs, `less`, `vim`) won't render correctly from
  the agent. That matches Phase 6's "agent runs non-interactive
  commands" model and is deliberate — terminals (with their
  `-it`) are the place to run interactive things.
- **No streaming.** The model still replies in a single
  non-streaming chunk. Streaming lands in 6.1; ordering between
  6.1 and 6.2 was flipped because write tools were the higher-
  value missing capability.
- **Open-buffer collision is destructive.** The agent's edits
  clobber unsaved keystrokes in the matching tab. This is
  deliberate (the agent owns its writes) but worth knowing.

## Related

- Spec: [`specs/coder.md`](../coder.md) — § Tool surface, § Error model.
- Roadmap: [`specs/roadmaps/phase-06-coder.md`](../roadmaps/phase-06-coder.md) § 6.2.
- ADR: [`specs/decisions/0010-coder-rewrite-not-acp.md`](../decisions/0010-coder-rewrite-not-acp.md).
- Prior: [`specs/test-plans/0039-coder-skeleton.md`](0039-coder-skeleton.md) (Phase 6.0 skeleton).
