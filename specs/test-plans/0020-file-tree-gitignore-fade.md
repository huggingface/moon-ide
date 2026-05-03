# Test plan 0020: file tree gitignore fade

- **Date**: 2026-05-03
- **Phase**: post-Phase 1 polish

## What shipped

- File tree rows that git would ignore render faded via Pierre's
  built-in `gitStatus: 'ignored'` style.
- Classification happens in the Rust backend and prefers
  `git ls-files --others --ignored --exclude-standard --directory`
  (index-aware), falling back to the `ignore` crate's walker when
  the folder isn't a git repo or `git` isn't on PATH.
- Tree no longer auto-expands top-level folders — everything starts
  collapsed.

## How to test

Prerequisites: `bun install`, `git` on PATH, a Rust project like this
repo available for "Open folder".

1. `bun run tauri dev`, open this repo as a workspace.
2. In the file tree, confirm `target/`, `node_modules/`, and
   `.env*` (if present) render faded; `src/`, `Cargo.toml`,
   `README.md`, etc. render at full opacity.
3. Confirm no top-level folder is auto-expanded — the tree opens
   fully collapsed.
4. Open a folder without a `.git/` (e.g. a fresh `mkdir` with a
   single `.gitignore`). Expected: gitignore patterns still fade
   matching entries via the walker fallback.
5. Inside a git repo, `git add -f` a file that matches a
   `.gitignore` rule (the repo's own `.env.example` under `.env*` is
   a good real example). Expected: the tracked file renders at full
   opacity even though it matches the rule.
6. Create a new file that _does_ match a gitignore rule and isn't
   tracked (`touch target/hello.txt`). Refresh the folder; the new
   entry should fade.

## What must keep working

- Pierre's row selection, keyboard navigation, search, Delete /
  Shift+Delete, and double-click-to-open.
- File tree rendering on folders without `.gitignore` (no fading,
  no backend errors in the console).
- Hidden files (`.gitignore`, `.editorconfig`) still show up in the
  tree — the classifier treats `hidden(false)`-style visibility,
  so the only effect of `.gitignore` membership is opacity.

## Known limitations

- No live refresh: edits to `.gitignore` during a session don't
  re-classify until the folder is re-opened or paths reload.
- Only the `ignored` status is surfaced. Modified / untracked /
  staged states aren't shown yet (not in scope for this change).
- The walker fallback is pattern-only and doesn't know about the
  index, so on a folder with a `.gitignore` but no `.git/` a
  matching-but-"tracked" file would still fade. Acceptable because
  "tracked" has no meaning without a git repo.

## Related

- Specs: `specs/frontend.md` (file tree notes).
- Prior test plans: `0001-initial-bootstrap.md`.
