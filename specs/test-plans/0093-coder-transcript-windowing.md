# Test plan 0093: coder transcript windowing

- **Date**: 2026-05-21
- **Phase**: post-Phase 6 coder polish (performance)

## What shipped

- The coder session transcript renders only a **floating window** of
  `coder.rows` (starting at `INITIAL_WINDOW = 50` rows at the tail),
  not the full history. Opening a long session paints a screenful,
  so initial load / paint no longer scales with transcript length.
- Scrolling near the top (or clicking the **Load N older messages**
  pill) pulls in `WINDOW_GROW_STEP = 50` more rows, with an
  element-based scroll anchor so the row under the user's eye stays
  fixed (no lurch).
- The mounted row count is **hard-capped at `WINDOW_MAX = 300`**.
  Past the cap the window _slides_ instead of growing — it drops 50
  rows off the (off-screen) bottom for each chunk pulled in at the
  top, detaching its bottom edge from the live tail.
- A detached window reels back toward the tail either incrementally
  (**Load N newer** pill at the bottom, or scrolling down toward it)
  or in one jump (sticky **"Jump to latest ↓"** button that snaps to
  the tail and scrolls to the bottom). Both directions share one
  element-based scroll anchor.
- Streaming, sticky-bottom auto-follow, and send-while-scrolled-up
  all keep working: while anchored to the tail new rows land inside
  the window; while reading history, appended rows are clipped off
  the bottom so nothing the user is reading moves.
- The window resets to the tail on any session / folder /
  sub-agent-view change, and on an in-session shrink (revert).

## How to test

Prerequisites: `bun run tauri dev`, signed in to the coder, and a
coder session with a **long** transcript (ideally 400+ rows so the
300-row cap is exercised — a long agent run with many tool calls
produces this).

1. Open the long session from the sessions list. Expected: the
   transcript paints quickly and is scrolled to the bottom (latest
   turn visible). A **Load N older messages (M hidden)** pill sits at
   the top of the rendered rows. Open DevTools > Performance, record
   the click → paint: the mount cost should be a bounded ~50 rows,
   not the full history.
2. Scroll up slowly toward the top. Older rows mount in automatically
   and the pill's hidden-count drops. The row you were looking at
   stays put — no visible jump when the new rows prepend.
3. Click the **Load older** pill directly (without scrolling to the
   very top). Expected: the next chunk of older rows appears above
   and your scroll position is preserved.
4. **Cap + slide**: keep loading older (scroll-up or pill) past ~300
   rows' worth. Expected: the DOM row count stops climbing (inspect
   the transcript's child count in DevTools — it plateaus around 300).
   As you keep pulling in older rows, a **Jump to latest ↓ (N below)**
   button appears, sticky at the bottom of the transcript viewport.
   Loading older still keeps your viewport anchored.
5. **Load newer (incremental)**: with the window detached (jump
   button + a **Load N newer (M below)** pill visible at the bottom
   of the rendered rows), scroll down toward the pill — newer rows
   reel in a chunk at a time and your viewport stays anchored. The
   pill click does the same. Confirm it does **not** cascade-load the
   whole tail in one step (the re-entrancy guard stops the anchor's
   own scroll from re-triggering a grow).
6. **Jump to latest**: click the **Jump to latest** button. Expected:
   the transcript snaps to the bottom showing the most recent rows,
   the window is back to ~50 rows, both the jump button and (if you
   were at the very top) the load-older pill update accordingly.
7. Scroll back to the bottom manually (when not detached). Expected:
   sticky-bottom re-arms — send a new prompt and the reply streams in
   with the view auto-following to the bottom.
8. **Send-while-reading-history**: scroll up into history (not at
   bottom), then send a steer / new prompt. Expected: your viewport
   does **not** jump; the **Jump to latest** button appears (the new
   rows are clipped below); clicking it shows the streamed reply at
   the tail.
9. **Session swap resets the window**: with a long session scrolled
   open (window grown / detached), open a different session, then a
   third. Expected: each opens at the tail with a fresh ~50-row
   window, scrolled to the bottom, no jump button — no inherited
   scroll depth or clip.

## What must keep working

- Sticky-bottom auto-scroll: while parked at the bottom (window
  anchored to the tail), every new streamed row / delta keeps the
  view pinned to the latest content.
- Streaming in place: an assistant reply streaming token-by-token
  (row text mutates, count unchanged) does not jump the scroll.
- Revert / edit-and-resend on a user message: works on any row the
  user has loaded into the window (scroll up first if it's old); the
  window resets to the tail after the revert shrinks the list.
- Tool-row lazy bodies (test plan 0076 follow-up) still apply — a
  collapsed tool row in the window renders no body until expanded.
- Folder switch / sub-agent pop-out and back: window resets to the
  tail and the correct transcript renders.

## Known limitations

- `Ctrl+F` find-in-page only matches rows currently in the DOM.
  History outside the window is not searchable until loaded — the
  **Load older** pill / **Jump to latest** button + counts make that
  discoverable. Accepted tradeoff for not measuring/virtualizing
  every row.
- The sub-agent pop-out view (`coder.view === 'subagent'`) is **not**
  windowed. Sub-agent transcripts are in-memory and typically short;
  the windowing state machine is scoped to the main session
  transcript only. Revisit if a sub-agent transcript ever gets large
  enough to matter.
- No measured-height / offset-math virtualization. Coder rows are
  wildly variable in height and stream their height in over time; a
  sliding window avoids a height cache and spacer divs entirely at
  the cost of not being able to jump to an arbitrary offset without
  walking the window there (the **Jump to latest** button is the one
  direct jump, back to the tail).

## Related

- Specs: [specs/coder.md](../coder.md) (transcript rendering section).
- Prior test plans:
  [0076-folder-switch-perf.md](0076-folder-switch-perf.md) (markdown
  viewport-gating + lazy tool bodies — the same transcript surface),
  [0043-coder-sessions.md](0043-coder-sessions.md) (session
  hydration).
