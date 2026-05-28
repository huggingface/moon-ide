# Test plan 0089: terminal Ctrl+K command suggestion

- **Date**: 2026-05-29
- **Phase**: Phase 3 (Terminal) / Phase 6 (Coder) cross-cut

## What shipped

- `Ctrl+K` in any terminal pane opens an inline overlay where the user types a natural-language request ("cherry pick last commit from feat-x"); on Enter the coder's standard model returns one shell command, which is prefilled into the PTY line for the user to review and run.
- New `coder_suggest_terminal_command` Tauri command: gathers terminal context (host/container shell kind + cwd from the frontend, active-folder git branch + local branch names from the host) and asks the standard model for a single command.
- New `CoderRunner::suggest_terminal_command` mirrors the `suggest_branch_name` / `suggest_commit_message` one-shot pattern, with a `sanitise_terminal_command` pass that keeps the output to one clean line (strips fences, `$`/`>` prompt markers, a single backtick wrap; preserves shell quotes).
- New cheap, local-only `WorkspaceHost::git_local_branches` (no `gh`/network) feeds the model real branch names for fuzzy matching.

## How to test

Prerequisites: `bun install`, a signed-in coder (HF device flow or a configured provider), a bound folder that is a git repo with at least two local branches.

1. Open a folder, open a terminal (host or container) from the bottom panel.
2. With the terminal focused, press `Ctrl+K`. Expected: an inline overlay appears centred near the top with a "Describe a command" label and a focused text input; the shell does **not** receive `^K`.
3. Type `list files sorted by size` and press Enter. Expected: "Generating…" shows briefly, the overlay closes, and a command like `ls -laS` (or similar) appears at the shell prompt **without** executing. Press Enter to run it yourself.
4. Press `Ctrl+K` again, type `cherry pick the last commit from <other-branch>` (use a real local branch name), press Enter. Expected: a `git cherry-pick <ref>` command is prefilled referencing the named branch.
5. Press `Ctrl+K`, then `Esc`. Expected: overlay closes, terminal regains focus, nothing written to the PTY.
6. Press `Ctrl+K`, click outside the overlay box (on the dimmed backdrop). Expected: overlay dismisses, no PTY write.
7. Sign the coder out (or disconnect the network) and retry step 3. Expected: the overlay stays open and shows a one-line red error under the input; the typed request is preserved so the user can retry.

## What must keep working

- `Ctrl+C` (copy with selection / SIGINT without), `Ctrl+V` paste, `Ctrl+Shift+C/V`, and the `Ctrl+L`-to-coder selection forward — none of these regress from the new `KeyK` branch in `attachCustomKeyEventHandler`.
- xterm file-link Ctrl/Cmd-click navigation and theme re-paint on dark/light flip.
- `coder_suggest_branch_name` / `coder_suggest_commit_message` and the SCM panel sparkles (same runner pattern, unchanged).
- `branch_list` (the full PR-aware listing) is untouched; `git_local_branches` is the only new host method.

## Known limitations

- Single command only; multi-step requests are chained with `&&` on one line rather than emitting a script.
- The command is prefilled but never auto-run — by design, the user always presses Enter.
- Git context is the active folder's, not the terminal's cwd folder, when they differ (terminals don't carry a folder binding). Good enough for the single-folder common case.
- No history of past prompts; each `Ctrl+K` starts blank.

## Related

- Specs: [specs/architecture.md](../architecture.md) (UI-never-touches-LLM invariant), [specs/coder.md](../coder.md)
- Prior test plans: 0062 (commit-to-new-branch, same suggester pattern), the Phase 3 terminal plans
