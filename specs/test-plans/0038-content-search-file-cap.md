# Test plan 0038: Drop the silent file cap from `Ctrl+Shift+F`

- **Date**: 2026-05-04
- **Phase**: cross-cutting bug

## What shipped

Bug fix: `Ctrl+Shift+F` (`palette.searchInFiles` → `ipc.search.content`) was silently missing matches in any non-tiny workspace.

`ContentSearchOptions` had a `max_files` field defaulting to **1000** (with a 50,000 hard ceiling) — a "keep responsive" knob that turned out to silently bail the walker out before it ever reached past-1000th-position files. On moon-landing-sized repos (>1000 source files after `.gitignore` filtering), this routinely meant searching for a string that exists returned `hits: []` and `truncated: true`. The CommandPalette renders the empty-state message in parallel with the truncation banner, so the dominant visual is "no results" with a subtle banner above — easy to miss, and wrong.

The cap was redundant: `max_matches` already bounds the size of the response and stops the walk early once enough matches are in. The walker also respects `.gitignore` / `.git/info/exclude`, so `node_modules`, `target/`, generated dirs, etc. are skipped automatically. Walking the rest of a sane workspace is bounded by the filesystem and `grep-searcher` is fast (it's the same engine `ripgrep` uses).

Changes:

- Removed `max_files` from `ContentSearchOptions` (Rust + TS bindings + manual `protocol.ts` interface). Per AGENTS.md, no migration shim — pre-1.0 schemas are free to restructure.
- Removed the `files_visited` counter and early-out from `search::search_content`. `truncated` now means exactly one thing: "you hit `max_matches`, narrow your query".
- Fixed the field's docstring on `max_matches` (the prior comment for `max_files` lied: it said "Cap on number of lines of context per match" but was a file cap).
- Added a regression test: 1500 noise files plus one target file with a unique string, search for the unique string, must surface the target hit (and `truncated == false`). Catches the same bug shape if anyone re-introduces a file cap.

Frontend call site (`commands.svelte.ts`) is unchanged — it was already passing only `query` and `max_matches`.

## How to test

Prerequisites: `bun install`, `bun run tauri dev`, a workspace folder with at least a few thousand source files (moon-landing or any real repo).

### Repro

1. Open a workspace folder with > 1000 tracked source files.
2. `Ctrl+Shift+F`. Search for a string you _know_ lives in a file that's likely past the 1000th file in the walker's order — e.g. `ReadRepoContent` in `server/lib/Permissions.ts` (alphabetical-ish order puts `server/` past many other dirs).
3. Hits appear, including the target file. The truncation banner only shows if you legitimately have > 500 (server-default) matches.

### Performance sanity

1. In a moon-landing-sized workspace, search for a common short string (e.g. `function`). The palette population still feels instant — `grep-searcher` is fast and `max_matches` (server default 500, frontend caps at 200 for the palette) bails the walk early.
2. Search for a string that doesn't exist anywhere. The palette does walk the whole tree (no file cap to short-circuit), but for a typical workspace this is sub-second; for a multi-tens-of-thousands-of-files workspace expect a few seconds while the walker scans. No spinner improvements here — that's a separate UX concern.

### Other walks unaffected

1. `Ctrl+P` (file fuzzy open) was not affected — it has its own `limit` (50) that's a result cap, not a file-visit cap. Verify it still returns the expected files in a big repo.
2. Git status walk is unchanged.

### Regression test

`cargo test --package moon-core search::tests::content_search_visits_files_past_old_default_cap` passes (it generates 1500 noise files plus a target file and asserts the search finds the target).

## What must keep working

- The `truncated` banner still fires when there are genuinely more than `max_matches` hits.
- `case_sensitive` and `regex` flags still work.
- `git_ignore` / `ignore` filtering still applies — searching a gitignored file (e.g. `node_modules/foo.js`) returns no hits for content inside it.
- `bun run check`, `bun run lint`, `cargo check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --package moon-core`, `cargo test --package moon-protocol --lib` (regenerates ts-rs bindings) all clean.

## Known limitations

- No cancellation: a slow query (millions of files, no gitignore) blocks until the walker finishes or `max_matches` saturates. If a workspace ever shows up where the unbounded walk feels slow in practice, the right move is request-cancellation on next-keystroke (drop in-flight searches when a new one starts), not putting the file cap back. Tracked informally; will revisit when someone hits it.
- Walker is single-threaded. `WalkBuilder::threads(...)` would parallelise; not done here because the user's reported issue is correctness, not speed, and threading the walk would change order-of-results determinism (currently filesystem order). If we need it, we add it under a feature gate plus a deterministic-sort step in the consumer.

## Related

- `crates/moon-core/src/search.rs` — the function this fixes.
- `specs/test-plans/0010-multi-folder-workspace.md` — the multi-folder framing under which `Ctrl+Shift+F` was scoped per active folder.
- `crates/moon-protocol/src/search.rs` — the schema this trims.
