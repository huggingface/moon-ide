# Test plan 0087: Goto-definition in the Review changes tab

- **Date**: 2026-05-20
- **Phase**: post-Phase 5 SCM polish (extends test plans 0074 and 0086)

## What shipped

- **Ctrl/Cmd-click goto-definition is wired in every Review section's right (working-tree) pane.** Hovering an identifier with the platform modifier held underlines it iff the LSP has a target; a click jumps to that target — which always opens as a regular editor tab in the same pane the review lived in, so the gesture doubles as "leave review mode and land on the function".
- **Lazy LSP attach.** `ReviewSection` does **not** issue `didOpen` at mount. The first time the user's mouse moves over a section's right pane with the modifier held, the section calls `workspace.ensureBackingBuffer(path)` (one-shot, shared with the existing edit-on-first-keystroke path), which loads the file as a normal `OpenFile` and fires `didOpen` exactly once. Sections the user only scrolls past pay nothing.
- **The extension is reused as-is.** `ReviewSection` calls `lspGotoDefinitionExtension` with the same callbacks `Editor` and `DiffView` use (`workspace.jumpTo`, `resolveExternalUri`, `pushClickNavigation`, `flash`); the only review-specific glue is the lazy-attach `mousemove` listener on `merge.b.scrollDOM` and a `side` prop plumbed through `ReviewView` so cross-file jumps replace the review tab in the **pane** the user is reading (not the pane that happens to have focus).
- **Same-file jumps are not supported** by design. The review tab is the active surface and the goto-def jump always opens a regular editor tab — there's no "stay in the review tab and move the caret to a different section" branch. Inside one section, hover on an identifier whose definition is in the same file: the jump still opens the file as a regular editor tab with the caret on the declaration; pre-existing nav history (Alt+Left) returns the user to the review tab.

## How to test

Prerequisites: a TypeScript or Rust repo on a feature branch with several changed files vs `main`. The moon-ide repo itself works once you're on a branch.

1. Run `bun run check`, `bun run lint`, `bun run fmt`. Expected: clean.
2. Launch `bun dev` with `RUST_LOG=moon_core::lsp=debug` so you can watch `didOpen` traffic in stderr.
3. Open a feature-branch folder. Click the SCM review icon. The **Review changes** tab opens with stacked sections. Watch the log: **no** `didOpen` for any changed file — only the synthetic `review://default-branch` buffer is mounted (and that one is skipped by `isSyntheticBufferPath`, so it never reaches the broker either).
4. Scroll through 4-5 sections without holding any modifier. Expected: lazy MergeView mounts proceed as in plan 0074, but **still no `didOpen`** in the log. The broker stays silent.
5. Hover an identifier in the **right pane** of one section with `Ctrl` held (Linux/Windows) or `Cmd` held (macOS). Expected:
   - The log shows `didOpen` for that file's path — exactly once.
   - Within a frame or two, the identifier underlines (`.cm-lsp-link` decoration) iff the LSP has a target for it. (If it's the first probe after `didOpen`, the underline can be a beat late; release and re-hover the modifier and it appears.)
6. Click that underlined identifier with the modifier held. Expected:
   - The target file opens as a **regular editor tab** in the same pane the review tab was in. The review tab is no longer the active tab in that pane (it's still in the tab strip — `Alt+Left` returns to it).
   - The caret + viewport lands on the LSP-returned `(line, character)` — the pending-jump consumer in `Editor.svelte` fires once on mount and drops the entry.
   - Press `Alt+Left`. The review tab becomes active again in that pane, scrolled to where it was. Press `Alt+Right` — back to the definition.
7. Hover a second identifier in a **different** section (without leaving review mode again — press `Alt+Left` first). Expected: the log shows a second `didOpen` for that section's file. Each section attaches its own backing buffer the first time it's probed.
8. Hover the **same** identifier in section A a second time. Expected: **no new** `didOpen` (the buffer is already attached); the probe routes straight to the live server.
9. Hover an identifier whose definition is in a **sibling** bound folder (cross-folder goto-def). Expected: clicking jumps into the sibling folder — same routing as a regular Ctrl-click does from `Editor.svelte`. The original review tab stays in its pane.
10. Hover an identifier whose definition is in `node_modules` / Rust toolchain / a `ts://` pseudo-URI. Expected: a toast `Definition outside workspace: <uri>`; no tab change.
11. **`deleted` rows.** Find a section whose status badge is `D`. Hover identifiers with the modifier held. Expected: no underline ever appears; no `didOpen` fires for that path. (The right pane is empty for deleted files; goto-def is gated off by `status !== 'deleted'`.)
12. **Files without an LSP language.** Find a section for `package.json`, `.editorconfig`, `Cargo.lock`, or similar. Hover with the modifier held. Expected: no underline, no `didOpen` (`lspLanguageFor(path) === null` gates the extension off entirely for these).
13. **Split panes.** Drag the review tab into a split (right pane), keep a regular editor tab in the left pane. Hold focus on the left editor and click into the review tab's right pane while holding Ctrl/Cmd. Goto-def from the review opens the target file **in the right pane** (where the review lives), not the left — because `side` is plumbed through to `workspace.jumpTo`.
14. **Edit + goto-def share the buffer.** Type a few characters in a section's right pane (existing edit flow), then Ctrl-click an identifier elsewhere in the section. Expected: only one `didOpen` for the file (shared `ensureBackingBuffer`); the edit and the jump both target the same `OpenFile`.

## What must keep working

Regression checks.

- All gestures from test plans 0074 and 0086: lazy IO mount, section collapse, `n` / `p` / Alt-Arrow scrolling, SCM-tree click to scroll, baseline toggle remounts, `Ctrl+L` selection capture, right-pane edits, `Ctrl+S` per-section save, dirty pip.
- The synthetic `review://default-branch` buffer is still filtered out of session persistence and out of every LSP request — `isSyntheticBufferPath` is unchanged.
- Sections for `deleted` files stay read-only; the right pane never gets a goto-def extension.
- Ctrl-click in a regular editor tab still works exactly as before (plans 0027, 0070). Hover / completion / diagnostics in regular editor tabs are untouched.
- Closing the review tab teardown removes the goto-def attach listener (`detachGotoAttach`) along with the horizontal-scroll mirror; no leaked `mousemove` handlers (verify with the browser devtools event listener inspector on `.cm-scroller` after closing a section).

## Known limitations

- **No hover / completion / diagnostics in review sections.** Adding any of those would force eager `didOpen` (you can't show diagnostics for a file you haven't opened), which is the broker-traffic explosion the original design was protecting against. Goto-def gets a pass because it's modifier-gated — the user has to express intent before the broker hears a peep.
- **No "same-file scroll" branch.** Goto-def always exits review mode, even when the definition is inside the section the user is in. Same-file jumps in review mode would mean adding a `pendingJumps` consumer to `ReviewView` like 0070 did for `DiffView`, which is straightforward but not worth the extra moving part — leaving review mode to read a function is a clearer intent signal than mutating the caret inside the stacked-diff view.
- **First probe after attach can flash an empty result.** If the user holds the modifier and immediately hovers an identifier in a never-probed section, the very first `definition` request races `didOpen` and returns `null` (the server doesn't know the doc yet). The underline simply doesn't appear; nudging the mouse fires another probe and the underline shows up. No toast, no error — the extension already treats `null` as "no target".
- **Lazily-attached `OpenFile`s aren't garbage-collected** — same caveat as 0086. A user who Ctrl-hovers in 5 sections keeps those 5 files in `openFiles` until they close them explicitly.

## Related

- Specs: [`specs/frontend.md`](../frontend.md) § "Diff and conflict surfaces" (Review changes bullet updated to describe the lazy goto-def attach).
- Prior test plans: [0074](0074-review-changes-tab.md) (Review tab — this plan supersedes its "No LSP / hover / completion" known limitation for goto-def specifically), [0086](0086-review-pane-edits.md) (editable right pane — introduced `ensureBackingBuffer`, which this plan reuses), [0070](0070-diff-mode-goto-def.md) (same wiring pattern in `DiffView`), [0027](0027-lsp-goto-definition-nav-history.md) (original goto-def + nav history).
- ADRs: none new. The decision to add goto-def specifically (and not the rest of the LSP stack) lives in the "Known limitations" section above — `didOpen` is the cost driver and goto-def is the only modifier-gated affordance that earns it.
