# Test plan 0064: Periodic git auto-fetch on the active folder

- **Date**: 2026-05-07
- **Phase**: 5.x (SCM polish) — adds an IPC + a background loop, so it earns a test plan even though there's no new visual surface beyond an existing button appearing more reliably.

## What shipped

- **New `WorkspaceHost::git_fetch` trait method.** `LocalHost` impl shells out to `git fetch --quiet --no-tags` with `stdin` nulled, `stdout` discarded, `stderr` captured. Prompts are disabled via `GIT_TERMINAL_PROMPT=0` plus blanked `GIT_ASKPASS` / `SSH_ASKPASS`, so a remote that needs credentials fails fast instead of hanging on a TTY prompt the desktop process can't render. `LC_ALL=C` matches the convention from `run_git_commit`. Capped at 30s via `tokio::time::timeout` to bound a hung fetch (DNS stall, dropped TCP) — we'd rather retry on the next tick than starve the work pool. Errors propagate git's stderr verbatim as `MoonError::IoError`.
- **New Tauri command `fs_git_fetch`** on the active folder + `ipc.fs.gitFetch` frontend wrapper. Same `require_active_folder` shape as the rest of the SCM commands.
- **Periodic auto-fetch loop in `WorkspaceState`.** `wireGitAutoFetch` runs once at app startup (idempotent, survives HMR), schedules the first fetch ~5s after wire (lets the IDE settle before touching the network), then every 3 minutes thereafter (matches VSCode / Cursor's `git.autofetchPeriod` default — hardcoded; flip to a setting when someone asks). Window-focus and folder-switch events also trigger a fetch, throttled to a 30s minimum so an alt-tab flurry doesn't spam IPC. The periodic 3-minute tick bypasses the throttle (it's the floor for "we definitely want a fresh fetch even if nothing else nudged us").
- **`gitFetch` followup is just `refreshGitBranch`, not a full status walk.** A fetch only moves remote-tracking refs (`refs/remotes/origin/<branch>`); the local working tree, index, and `HEAD` are unchanged. Refreshing branch ahead/behind is the only thing that makes the SCM panel's "Sync Changes" button surface — anything else (file tree, blame, gutters) would be wasted work.
- **Best-effort: failures stay quiet.** Auto-fetch errors (offline, no upstream, auth refused, 30s timeout) are silently swallowed by the frontend — the user never asked us to fetch, so a flash toast would be noise. The backend's `run_git_fetch_quiet` emits a `tracing::debug!("git_fetch failed", root, detail)` on every failure path, so `RUST_LOG=moon_core=debug` users can still triage "why isn't Sync Changes appearing?" without leaving the dev terminal. The loop also short-circuits when `document.visibilityState === 'hidden'`, when no folder is active, and when a fetch is already in flight (single in-flight guard, no concurrent fetches).
- **Backend tests** (`crates/moon-core/src/host.rs`):
  - `git_fetch_advances_remote_tracking_ref` — three-repo end-to-end: bare `remote.git`, `pusher` lands a commit upstream, `local` runs `git_fetch`, asserts `refs/remotes/origin/main` advanced and `git_branch().behind == 1`.
  - `git_fetch_fails_fast_on_unreachable_remote` — bogus `file:///` URL, asserts the call returns `IoError` in well under the 30s timeout (validates the no-prompt env path without flaky network).

## How to test

Prerequisites: `bun install`, `cargo build`, `bun run tauri dev`. Open a folder bound to a git repo with an `origin` upstream you can push to from a second clone (e.g. another working tree on disk).

### Periodic fetch surfaces "Sync Changes"

1. Open moon-ide on a repo. The SCM panel header shows the current branch.
2. From a separate terminal, in a sibling clone of the same repo, commit and push something new:
   ```
   echo demo >> README.md && git -C ~/work/sibling-clone commit -am 'demo' && git -C ~/work/sibling-clone push
   ```
3. **Don't** click anything in moon-ide. Wait up to 3 minutes (or alt-tab away and back to trigger the focus nudge).
4. Expected: the SCM panel grows the "Sync Changes" button with `↓1` (or whatever `behind` count matches what you pushed). The branch label / ahead count unchanged.
5. Click "Sync Changes" — it pulls (and pushes if you also have local commits), counts go to 0, the button disappears. Same code path as before; auto-fetch only governs _when the button shows up_.

### Initial fetch ~5s after startup

6. Quit moon-ide. From the sibling clone, push another commit. Restart moon-ide.
7. Within ~5 seconds the "Sync Changes" button should appear on its own. Confirms the initial-fetch path runs.

### Focus nudge with throttle

8. With moon-ide focused, alt-tab to another app, push another commit upstream from the sibling clone, alt-tab back.
9. The Sync Changes button updates within a couple seconds of regaining focus.
10. Alt-tab away and back rapidly (5+ times in 30 seconds). The fetch should _not_ fire on every focus event — verify by tailing the dev terminal: at most one `git fetch` subprocess should have spawned during the burst (open `htop`, watch for transient `git fetch` processes, or `strace -ff -p $tauri-pid`).

### Folder switch nudge

11. Add a second folder to the workspace (a second repo with an upstream where you have remote write access). Push a new commit upstream from a sibling clone of _that_ repo.
12. Switch the active folder to the second one. Within ~1 second the SCM panel updates: branch label flips, and after the (throttled) fetch the Sync Changes button surfaces if the fetch was outside the 30s window.

### No-upstream / non-repo folders

13. Open a non-repo folder (a plain directory). The SCM panel reports "no branch". Auto-fetch fires (in-process timer doesn't know better) but `git fetch` exits non-zero ("not a git repository") within ms. The frontend dev tools console stays clean (no flash toast, no console log). With `RUST_LOG=moon_core=debug` set on the dev terminal, you'll see one `git_fetch failed root=… detail=fatal: not a git repository` line per attempt; without it the failure is invisible by design.
14. Same with a repo folder that has no `origin` remote — same story: backend logs `detail=fatal: 'origin' does not appear to be a git repository` at debug level, frontend stays quiet, no UI change.

### Hidden tab — pauses

15. With moon-ide running, hide its window (cmd+H on macOS, minimise on Linux/Windows). Wait several minutes. From the sibling clone, push commits.
16. Bring moon-ide back. Within ~1s of regaining focus the Sync Changes button should appear (the focus nudge fires on visibility transition). The periodic 3-minute timer alone wouldn't have fired during the hidden interval (we short-circuit on `document.visibilityState === 'hidden'`).

### Auth-prompt safety

17. Configure a folder whose `origin` URL points at a HTTPS remote that needs credentials and for which you have **no** credential helper / cached token (e.g. a private repo on a fresh machine). Open it as the active folder.
18. Wait for the initial fetch (~5s). With `RUST_LOG=moon_core=debug` the dev terminal logs `git_fetch failed root=… detail=fatal: could not read Username for …` (or similar) — but the moon-ide process **does not hang**. No prompt window appears. The 30s timeout is the hard ceiling; in practice git's HTTPS layer fails much faster when stdin is nulled.

### Backend unit tests

19. `cargo test -p moon-core --lib host::tests::git_fetch_advances_remote_tracking_ref` — green (creates three temp repos, lands a commit upstream, runs `git_fetch`, asserts `refs/remotes/origin/main` moved and `git_branch().behind == 1`).
20. `cargo test -p moon-core --lib host::tests::git_fetch_fails_fast_on_unreachable_remote` — green (bogus `file:///` URL, asserts `IoError` returned in well under 10s — validates the no-prompt path without exercising the 30s timeout).

## What must keep working

- **Manual sync / push / pull / publish-branch** — all unchanged code paths. The new `git_fetch` lives alongside them on `WorkspaceHost`; the frontend doesn't reuse `pullChanges` or `pushChanges` from the auto-fetch loop.
- **`gitBranch` snapshot** — same `GitBranchInfo` shape; `refreshGitBranch` is the same private method `refreshGitStatus` already calls. Auto-fetch just calls it more often.
- **`refreshGitStatus` / `refreshActiveFolder` / fs-watch** — auto-fetch deliberately does **not** call these. Local working tree and index don't move during fetch; running them would be wasted work and would defeat the surgical-refresh story from test plan 0052.
- **Cross-folder badge refreshes from coder edits (test plan 0052)** — unaffected. Auto-fetch only refreshes the active folder's branch state; per-folder change badges still ride on the existing `refreshAllGitChangeSummaries` fanout.
- **The 30s focus throttle** — applies to focus + folder-switch + initial; the periodic 3-minute timer always runs (subject to the in-flight guard + visibility / active-folder gates).
- **`bun run check`, `bun run lint`, `cargo check --workspace --exclude moon-desktop`, `cargo clippy --workspace --exclude moon-desktop --all-targets -- -D warnings`** all clean.

## Known limitations

- **No fs watcher for `.git/refs/remotes/`.** A `git fetch` from an external terminal still has to wait for moon-ide's next tick (or window focus) to surface in the SCM panel. Phase 5's full fs watcher will close that gap.
- **No setting to disable / change interval.** Hardcoded to 3 minutes. AGENTS.md "hardcode first, configure later" — flip to a knob when someone asks.
- **Single remote per fetch.** `git fetch` with no args fetches only the current branch's configured remote (defaults to `origin`). Multi-remote auto-fetch (`--all`) is not wired; if the team ends up with non-origin upstreams in regular use we'll add it.
- **Container parity not exercised.** `WorkspaceHost::git_fetch` is on the trait, so a future `RemoteHost` (Phase 2) implementation runs the fetch inside the container — exactly what we want for in-container repos. Smoke when the remote host lands.
- **No retry / backoff on transient failure.** The next 3-minute tick is the retry. A spike of failures during a flaky network shows up only as repeated `console.debug` lines.
- **No "auto-pull" option.** We deliberately fetch-only; merging upstream into a working buffer is the user's decision via the Sync Changes button. Auto-pull would surprise people mid-edit.
- **Fetch races with manual sync.** If the user clicks Sync Changes while an auto-fetch is in flight, the in-flight guard makes auto-fetch the no-op for that tick; the manual `git pull` runs unchanged. We don't cancel an auto-fetch in flight to make room for the manual op — git's own concurrency-safe lock handles overlap, and 30s is the worst-case wait.

## Related

- ADRs: [0002 — workspace host](../decisions/0002-workspace-host.md) (`git_fetch` is one more `WorkspaceHost` method; same shape as `git_pull` / `git_push`).
- Specs: [roadmap.md § Phase 5](../roadmap.md#phase-5--git).
- Prior plans: [0021 — file tree full git status](0021-file-tree-full-git-status.md), [0033 — git change gutter](0033-git-change-gutter.md), [0052 — folder bar status](0052-folder-bar-status.md), [0062 — commit to a new branch](0062-commit-to-new-branch.md).
