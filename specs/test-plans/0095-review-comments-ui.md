# Test plan 0095: review comments + reviewed-file marks (Phase 5.7.1)

Date: 2026-06-13

## What shipped

- Inline **review comments** on the Review changes tab: select line(s) in any section's diff (either side), press `Ctrl+Alt+C`, and a composer card opens below the selection. Comments render as anchored cards (author, relative time, body, Edit / Delete) and persist as local drafts in the per-folder session.
- **Content-fingerprint re-anchoring**: a comment is anchored by a hash of its line text, not a fixed line number. Editing the file (or a new commit) re-locates each comment by content; if the line moved, the card follows it; if the line is gone, the card renders **stale** ("line changed") rather than disappearing.
- Per-section **"Viewed" checkbox** (GitHub's checkbox): ticking marks the file reviewed (pinned to its working-tree blob SHA) and collapses the section. A later commit / edit that changes the file auto-un-ticks just that file on the next git-status refresh and re-expands it.
- Banner gains review state: `N / M reviewed` progress, an unpublished-comment count, and a **disabled** "Publish review →" CTA (the publish path itself is 5.7.2).
- New CM extension `src/lib/editor/reviewComments.ts` (facet-driven block widgets, modelled on `conflictMarkers.ts`); state CRUD on `WorkspaceState`; both ride the existing `FolderSession` persistence from 5.7.0.

## How to test

Prerequisites: `bun install`, a git repo bound as the active folder with a branch that has changes against its default branch (so the Review tab has sections). Run `bun dev`.

### Comments

1. Open the Review changes tab (SCM panel → review icon, or the change-count badge flow). Confirm stacked diff sections render as before.
2. In a `modified` file's section, click into the **right** (working) pane, select one line, press `Ctrl+Alt+C`. Expected: a composer card opens below that line with a focused textarea.
3. Type a comment, press `Ctrl+Enter` (or click **Comment**). Expected: the composer closes; an anchored card appears below the line showing `You`, a relative time, and your text. The banner's comment count reads `1 comment`.
4. Select 3 lines, `Ctrl+Alt+C`, submit. Expected: a card anchored below the **last** selected line. Multi-line anchors are supported.
5. Click **Edit** on a card, change the text, **Save**. Expected: the body updates in place. Click **Delete** on a card. Expected: it's removed and the banner count drops.
6. Add a comment on the **left** (base) pane of a modified file (select a deleted/old line, `Ctrl+Alt+C`). Expected: the card anchors on the base side. (This is GitHub's `LEFT`-side comment.)
7. **Gutter "+".** Hover the pointer over a line in either pane (no selection). Expected: a small accent-colored **"+"** appears in a gutter column on that row only; it disappears when you move to another row. Click it. Expected: the composer opens anchored to that single line.
8. **Markdown.** In a comment, type ``check `foo()` and **this**`` plus a fenced code block, submit. Expected: the card renders the inline code, bold, and a highlighted code block (same pipeline as the Markdown preview), not raw asterisks/backticks.
9. Reload the IDE (`Ctrl+R` in dev, or relaunch). Re-open the Review tab. Expected: every comment is still there, anchored to the same lines — drafts persist in the session.

### Re-anchoring & stale

8. With a comment anchored on, say, line 20 of a file, open that file in a normal editor tab and **insert 5 blank lines above line 20**, save. Return to the Review tab and re-open/scroll its section. Expected: the comment card now sits below the _moved_ line (line 25), not line 20 — it followed the content.
9. Now **delete the exact line** the comment was anchored to, save. Back in the Review tab. Expected: the card renders **muted with a "line changed" flag** (stale), still showing Edit / Delete. It is **not** silently dropped.
10. Undo the deletion (restore the line text). Expected: on the next section rebuild the card un-stales and re-anchors cleanly.

### Reviewed-file marks

11. In a section header, tick the **Viewed** checkbox. Expected: the checkbox checks, the section collapses (you signed off), and the banner progress increments (e.g. `1 / 8 reviewed`).
12. Click the caret to expand the reviewed section manually. Expected: it expands and stays expanded (manual override), checkbox still ticked.
13. Edit that file (in a normal editor tab) and save, **or** make a commit that touches it. Trigger a git-status refresh (save, focus the window, or `Refresh File Tree`). Expected: the file's **Viewed** tick clears automatically and the section re-expands — its blob SHA no longer matches what you reviewed. Other ticked files stay ticked.
14. Un-tick a Viewed file manually. Expected: tick clears, banner progress decrements.
15. Reload the IDE. Expected: Viewed marks persist across the relaunch (until a drift clears them).

## What must keep working

- The existing Review tab behaviour from test plan 0074: stacked diffs, lazy section mount, `n`/`p` + `Alt+Arrow` navigation, SCM-tree-click scroll, sticky banner with current file, scroll restore across tab/folder switches.
- Inline pane edits (0086) and review-mode goto-definition (0087): typing in a section's right pane still saves with `Ctrl+S`; `Ctrl/Cmd`-click still jumps to definitions. The comment composer's `Ctrl+Alt+C` and the cards don't intercept those.
- `Ctrl+Enter` / `Escape` inside the composer submit / cancel without leaking the keystroke to the editor underneath; clicks inside a card or composer don't move the editor selection.
- Folder switches keep each folder's own comments and Viewed marks (per-`FolderState`).
- 5.7.0's persistence: `review_comments` / `reviewed_files` round-trip through `session.json`; old sessions without the fields still load.

## Known limitations

- **No threading / replies / resolve**, and other people's GitHub comments are not shown. This is a write-my-own-review tool (per ADR 0027 non-goals).
- Re-anchoring is exact trimmed-text match within ±40 lines, then "stale". No fuzzy/semantic relocation.
- Publish to GitHub is a separate sub-phase — see test plan 0096.

> Note: this plan's original draft shipped without a gutter "+" and with plain-text comment bodies. Both landed in the same sub-phase shortly after: a hover-only **"+"** gutter button (hover any line → click + to comment there, no selection needed) and **markdown** rendering of comment bodies via the existing `renderMarkdown` pipeline. The keyboard path (`Ctrl+Alt+C` on a selection) still works for multi-line anchors.

> Post-testing fixes (same sub-phase): (1) the "+" gutter now reserves a **fixed-width column**, so hovering no longer shifts the code horizontally; (2) comments are also available in the **regular editor** and the **diff view's working pane**, gated on `workspace.isReviewableBranch` (open PR, or any non-default branch) — on `main` with no PR the affordances don't render at all. Verify: open a file on a feature branch in plain Source mode, hover a line → "+" appears with no horizontal jump; switch to the default branch → no "+" anywhere outside the Review tab. All widget/composer styling moved into the extension's `baseTheme` so the three host surfaces render identically.

## Related

- Spec: [`specs/review-comments.md`](../review-comments.md)
- Decision: [ADR 0027 — local-first review comments](../decisions/0027-review-comments.md)
- Roadmap: [`specs/roadmaps/phase-05.7-review-comments.md`](../roadmaps/phase-05.7-review-comments.md)
- Builds on: test plans 0074 (Review changes tab), 0086 (review pane edits), 0087 (review-mode goto-def)
