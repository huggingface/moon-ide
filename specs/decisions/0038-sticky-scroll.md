# ADR 0038 — Sticky scroll from the indent heuristic, not the LSP

## Context

The editor and diff panes gain a VS Code-style "sticky scroll"
header: the chain of enclosing definitions of the first visible
line, pinned to the top of the pane. The obvious data source is LSP
`documentSymbol`; we already have a broker.

## Decision

Reuse the text heuristic in `editor/diffCollapseContext.ts`
(`enclosingStack`, generalised from the review tab's collapse-label
`enclosingSymbol`): walk upward from the anchor line for
definition-looking lines at strictly decreasing indents.

- Synchronous and cheap — the header recomputes on every scroll
  frame; a request/response round-trip per frame (or a cached
  symbol tree invalidated on edit) is machinery we don't need.
- Works on every surface identically, including buffers with no
  LSP (left diff pane holds a HEAD blob, plain-text-ish files).
- Same failure mode as the review labels: no header rather than a
  wrong one on oddly-indented code.

DOM-wise the header is a zero-height `position: sticky` wrapper
prepended to `.cm-editor`, so one extension serves both the regular
editor (scroller inside the editor) and `@codemirror/merge` panes
(doc-tall editors scrolled by the outer `.cm-mergeView` — the same
layout the synthetic h-scrollbar already navigates).

## Rejected

- **LSP `documentSymbol`**: async churn, per-language coverage
  holes, dead on non-LSP buffers; accuracy gain doesn't pay for it.
- **Review-tab sections**: their editors never scroll internally
  (sections stack in one outer scroller) and each section already
  has a sticky file header plus enclosing-symbol fold labels.
- **CodeMirror top panel (`showPanel`)**: panel height changing
  with stack depth resizes the scroller mid-scroll — feedback
  jitter. The overlay covers lines instead, like VS Code.
