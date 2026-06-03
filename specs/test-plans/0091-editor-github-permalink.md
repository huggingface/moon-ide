# Test plan 0091: editor "Copy GitHub link" / "Copy GitHub markdown link"

- **Date**: 2026-05-29
- **Phase**: post-Phase 5 SCM polish

## What shipped

- Right-clicking inside the main editor now offers **Copy GitHub link** and **Copy GitHub markdown link** for the lines under the current selection (or the caret line when nothing is selected).
- New `GitPermalink` DTO + `WorkspaceHost::git_permalink(path, start_line, end_line)` (`LocalHost` impl) that builds `https://github.com/<owner>/<repo>/blob/<sha>/<path>#L<start>(-L<end>)`, pinned to the current `HEAD` commit SHA so the link survives later commits.
- The Markdown form is `[<path>#L<start>(-L<end>)](<url>)`; single-line ranges drop the `-L<end>` suffix to match GitHub's own "Copy permalink".
- Wired through `fs_git_permalink` (Tauri) + `ipc.fs.gitPermalink`; the editor menu reuses `ContextMenu.svelte` portaled onto `document.body` (same approach as the tab-strip menu) and copies via the Tauri clipboard plugin (`@tauri-apps/plugin-clipboard-manager`), falling back to `navigator.clipboard` only if the plugin throws. The plugin is required because the actions fire from a portaled menu that doesn't take focus, and `navigator.clipboard.writeText` rejects on WebKitGTK in that case — the cause of the original "Could not copy GitHub link" failure.
- Resolution reuses the existing `remote_web_url` / `encode_branch_segment` helpers (currently `github.com` only).

## How to test

Prerequisites: `bun install`, a GitHub-remote checkout (this repo works), `git` on PATH.

1. Open this repo in moon-ide. Open `src/lib/components/Editor.svelte`.
2. Select lines 5-9, right-click → **Copy GitHub link**. Expected: a `Copied GitHub link` flash; clipboard holds `https://github.com/<owner>/<repo>/blob/<full-sha>/src/lib/components/Editor.svelte#L5-L9`. Paste it into a browser and confirm GitHub highlights lines 5-9 at that commit.
3. Right-click again → **Copy GitHub markdown link**. Expected: clipboard holds `[src/lib/components/Editor.svelte#L5-L9](https://github.com/.../#L5-L9)`. Paste into a GitHub PR/issue comment box and confirm it renders as a clickable line-range link.
4. Click once (no selection) on line 12 → right-click → **Copy GitHub link**. Expected: URL ends `#L12` (no `-L<end>`).
5. Make a new commit, then repeat step 2 on the same lines. Expected: the SHA in the URL is the new HEAD — the link is pinned to whatever HEAD is at copy time.
6. Open a folder with no GitHub remote (or a GitLab remote). Right-click in the editor → **Copy GitHub link**. Expected: `No GitHub link (not a GitHub repo or no commits)` flash; nothing written to the clipboard.
7. Right-click in an **untitled** buffer (`Ctrl+N`), an **external** buffer (`Ctrl+O` on a file outside every bound folder), or a `review://` tab. Expected: the browser's native context menu appears — no moon-ide menu (a permalink makes no sense there).
8. `cargo test -p moon-core --lib host::tests::git_permalink` — three green tests (pinned link, no-GitHub-remote → None, path-escape rejected).

## What must keep working

- The tab-strip context menu (`EditorTabs.svelte`) and the file-tree row menu — both share `ContextMenu.svelte`, which was not modified.
- Git blame / remote-URL resolution: `git_permalink` reuses `remote_web_url` and `encode_branch_segment`; `git_blame_resolves_github_remote_to_web_url` and `normalize_remote_url_handles_all_shapes` must still pass.
- The editor's normal selection behaviour and the `Ctrl+L` "Add selection to Coder" path — the new menu reads the selection but doesn't mutate `activeSelection`.

## Known limitations

- `github.com` only. Other hosts (GitLab, Bitbucket, self-hosted) return `None` and grey the action out — same scope as the existing blame/PR-URL link resolution. Add host mapping when there's a concrete need.
- The link always pins `HEAD`, never a branch ref. That's deliberate (permalinks should be stable), but it means a link to an unpushed commit 404s until the commit is pushed — same caveat as GitHub's own "Copy permalink" on a local checkout.
- No keybinding; the gesture is right-click only. Add one if it becomes a real ask.

## Related

- Specs: [`specs/frontend.md`](../frontend.md) (editor context menu), [`specs/protocol.md`](../protocol.md) (`git.*`).
- ADRs: [0002 — workspace host](../decisions/0002-workspace-host.md) (`git_permalink` is one more `WorkspaceHost` method).
- Prior test plans: [0060-tab-context-menu.md](0060-tab-context-menu.md) (the `ContextMenu.svelte` + clipboard pattern this reuses), [0033-git-change-gutter.md](0033-git-change-gutter.md) / blame (the `remote_web_url` resolution this reuses).
