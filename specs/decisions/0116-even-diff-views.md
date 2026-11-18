# ADR 0116: Even diff views, block-aware expansion, unified read-only diffs

## Context

The review surfaces (Review changes, commit and pull-request tabs,
the working-tree diff) read as uneven: a comment or thread card in
one pane pushed every following line out of step with its counterpart
until the next change; gutters differed in width between panes and
files; added / removed / changed colours came from three different
places; and the "N unchanged lines" bars were the library's defaults,
expanding all or nothing. Reviewing an agent's PR usually needs the
function around a change, not the 200 lines above it.

## Decision

Keep `@codemirror/merge` and fix the causes.

- **Level panes.** In the split layout, corresponding unchanged lines
  sit at the same height at all times: a card under an unchanged line
  gets a filler of its height at the matching line of the other pane.
- **One card shape.** Draft comments, the composer and pull-request
  cards share one width rule (the visible pane, capped), inset and
  surface.
- **Even gutters and colours.** Line-number gutters are the same
  width in both panes and across stacked files; added, removed and
  changed-text colours and the filler rows come from the theme tokens.
- **Our own collapsing** replaces the library's. A bar reads
  `N unchanged lines` plus the enclosing definition of the code after
  it, and offers:
  - **Block below**: reveal from the start of the function / method /
    class containing the change after the bar down to that change
    (also what clicking the definition does);
  - **Block above**: reveal from the change before the bar to the end
    of its block;
  - **+20 above / below** where no block is known;
  - **Expand all**.
    Whatever stays hidden remains a bar. Blocks come from the syntax
    tree, then from indentation for languages without one. Both panes
    collapse and expand together. Editing next to a collapsed run opens
    it.
- **Unified layout for read-only diffs.** Commit and pull-request tabs
  honour the split / unified preference (and go unified when narrow),
  with pull-request cards in place; a thread on a removed line sits
  under its removed chunk, labelled with the old line number. Review
  changes stays split.

## Rejected alternatives

- **`@pierre/diffs`.** Polished static rendering, but expansion is by
  line count only and there is no editor inside: go-to-definition,
  editing the right side and the comment composer would all be
  rebuilt, on APIs it still marks experimental. The repository already
  moved off it once for these reasons.
- **Fillers for cards inside a change.** Those lines have no
  counterpart to stay level with; the library's spacer already evens
  the panes where the change ends.
- **Container-query units for card width.** They need containment on
  every editor's scroller; two measured pixel values behave the same
  in every webview.

## Not covered

Hover and go-to-definition in the read-only commit and pull-request
tabs, and a unified layout for Review changes.
