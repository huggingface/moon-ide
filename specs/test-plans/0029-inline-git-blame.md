# Test plan 0029: Inline git blame

- **Date**: 2026-05-04
- **Phase**: Phase 5 (Git)

## What shipped

- GitLens-style inline annotation at end-of-line for the caret's current row, showing `{author}, {relative date} • {commit summary}` in a dim italic face.
- Hover on the annotation pops a tooltip with the full commit subject, author + email, absolute + relative date, and short commit hash.
- Backend shells out to `git blame --porcelain -w` per file, parses the stable porcelain format, and caches the result per open buffer. Debounced refresh on save picks up new commits without a manual reload.
- Files outside a git repo, untracked files, and uncommitted lines degrade gracefully — the widget either disappears (non-repo) or shows `Uncommitted changes` (local edits).
- `#NNN` and `owner/repo#NNN` PR references inside the commit subject/body turn into clickable links in the hover tooltip. Backend resolves the repo's `origin` (then `upstream`) remote URL and normalises it to the canonical `https://github.com/<owner>/<repo>` base; only GitHub is supported for now, other hosts render as plain text.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a folder bound that is a git repo (moon-ide itself works).

### Happy path — current-line annotation

1. Open any TypeScript or Markdown file in the moon-ide folder. A dim italic badge appears at end-of-line for the active line: e.g. `Your Name, last week • some commit subject…`.
2. Move the caret with the arrow keys. The badge follows the caret row by row, one annotation visible at a time.
3. Click to a different line. Same behaviour — always the caret's line.
4. Open a file with fewer than ~5 commits on it (a recently-added file). The authors shown cycle between the two or three people who've touched it; the relative dates match what `git log` shows.

### Hover tooltip

1. With the annotation visible on some line, move the mouse over the badge (not the code). After ~400 ms a tooltip opens above (or below if no room above) the badge.
2. Tooltip contents:
   - First line: full commit subject, bold.
   - Then: `Author Name <author@email>`.
   - Then: absolute date + `·` + relative date (e.g. `May 4, 2026, 2:15 PM · an hour ago`).
   - Last line: `commit <8-char-sha>`.
3. Move the mouse onto the tooltip itself. It stays open. Move off it: ~120 ms later it dismisses.
4. Rapidly move the mouse over and off the badge. No flicker; no leaked tooltip divs in the DOM.
5. Switch tabs while a tooltip is open. The tooltip dismisses automatically (the Editor instance destroy path hides it).

### Uncommitted lines

1. Open a tracked file and edit an existing line. Save the file. Before any new commit, the badge for that line shows `Uncommitted changes`.
2. Hover: the tooltip reads `Uncommitted changes` / `Local edits not yet committed.` with no author or sha.
3. Commit the edit (`git commit -am "test"` in an external terminal, then trigger the IDE's "Refresh File Tree" command or save the file again). The badge updates to the new author + relative date.

### Stale blame while editing

1. Open a tracked file. Place caret on a line with a real commit badge.
2. Type a few characters in the middle of that line (don't add newlines). The annotation text may temporarily drift (the on-disk blame still reports the old row contents). Save the file (`Ctrl+S`). The badge refreshes within ~250 ms to reflect the new truth.
3. Insert a newline mid-file (so line numbers shift below). Without saving, badges on lines below the insertion show the commit metadata of the previous row at that offset — a known limitation. Saving restores correctness.

### Non-repo and outside-repo degradation

1. Bind a folder that is _not_ a git repo (e.g. a fresh `mkdir /tmp/plain && cd /tmp/plain && touch a.txt`). Open `a.txt`. No badge appears. No toast, no error in the console.
2. Bind a git repo but open a binary file (image). No badge; Markdown and image previews don't have the editor pane at all, so the blame extension never loads for them.
3. Open an untracked file inside a real repo (`echo hi > /path/to/repo/new.txt`, then open it from the tree). No badge — `git blame` exits non-zero for untracked paths, which the backend maps to `Ok(None)`.

### PR linking in hover

1. In the moon-ide repo (or any GitHub-hosted repo), find a commit whose subject ends in `(#NNN)` — squash-merge style. Open a file where that commit touched a line, hover the annotation for that line. `#NNN` renders in accent colour and underlines on mouse-over.
2. Click the link. The system's default browser opens `https://github.com/<owner>/<repo>/pull/NNN`. The IDE window is not navigated.
3. Cmd/Ctrl-click and middle-click on the link: same behaviour (the click handler preempts the default).
4. Open a repo whose `origin` is on GitLab or Bitbucket (or has no remote at all). `#NNN` text appears in the hover but is _not_ a link — rendered as plain text.
5. Find / craft a commit with a cross-repo reference like `fixes foo/bar#42` in the subject. The whole `foo/bar#42` token becomes a single link pointing at `https://github.com/foo/bar/pull/42`, regardless of what the local repo's origin is.
6. Sanity check parser rejections: a commit subject with `abc#123` (alphanum immediately before the `#`) or `#12345abc` (alnum trailing digits) should render as plain text.

### Performance sanity

1. Open a large tracked file (1000+ lines, e.g. `src/lib/state.svelte.ts`). First appearance of the annotation should happen within ~400 ms of the file opening on a warm repo. No visible editor hitch.
2. Hold down `Down` to scroll through the file. The badge updates per row; scroll speed is unchanged from a file without blame.
3. Fast save cycles (`Ctrl+S` rapidly) should coalesce to a single blame refresh at the end of the burst (there's a 250 ms debounce); you shouldn't see a dozen git processes in `ps aux | grep blame`.

## What must keep working

- All prior git tree markers (`0020`), discard-changes menu (`0022`), refresh-on-fs-event (`0021`).
- LSP hover (`0025`), navigation history (`0027` / `0028`), and the existing CM extensions (brackets, completion, goto-def).
- Markdown preview (`0004`), editor tab / split behaviour.
- The editor remains usable when `git` isn't on `PATH` at all — the blame extension treats that as "no blame".

## Known limitations

- Line-shift staleness: while the buffer is dirty and contains insertions / deletions, the per-line blame reflects the file's state at last save, so rows below an inserted line may show the wrong commit. Saving recomputes. No attempt is made to rewrite blame entries through pending edits.
- No "You" substitution for the current user — the annotation always prints the commit author's name as recorded, even when that's the local user. We'd need to read `git config user.email` and compare per row; fine follow-up once someone asks.
- The full commit message body isn't shown in the hover: `git blame --porcelain` hands us only the subject. Pulling the full body means a second `git show --no-patch --format=%B <sha>` per unique sha; worth it, but scoped out of this slice.
- No "open diff for this commit" or "copy sha" action from the hover tooltip. Those arrive with the SCM panel (Phase 5 later slice).
- PR link resolution is GitHub-only. GitLab's `!NNN` merge-request syntax, Bitbucket's `pull-requests/NNN` path, and self-hosted Gitea / Forgejo all render the raw text. Add mapping when a user on one of those hosts asks.
- Non-editor surfaces (Markdown preview, image view) don't show blame — the widget is an editor-only concern.
- The backend's `git blame` subprocess is unbounded per call. A pathological 100k-line file could stall the blocking pool for a few seconds. Good enough for source code; if anyone opens a huge generated file, we'll know.

## Related

- Specs: `specs/roadmap.md` — Phase 5 section lists inline blame as landed.
- Prior test plans: `0020-*.md` / `0021-*.md` / `0022-*.md` for the git status + discard machinery this blame layer rides on top of.
