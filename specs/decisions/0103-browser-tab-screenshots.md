# ADR 0103: Browser-tab screenshots via the platform webview snapshot

Amends [ADR 0088](0088-in-ide-browser-tab.md) ("no screenshots").

## Context

The agent can read and drive an IDE browser tab, but only through a
text outline, so it can't check layout, styling or anything visual.
Tauri 2.12 has no webview capture API (wry issue #1358 is open), and
child webviews are still behind the `unstable` flag, with the overlay
problems ADR 0088 rejected them for.

## Decision

- `browser_page(action: "screenshot")` returns the tab's visible area
  as an image attachment (the `images` tool-result convention), so the
  model sees it and the panel shows it on the tool row. Read-only;
  refused up front when the active model has no image input.
- The tab stays an `<iframe>`. The frontend measures the iframe's
  viewport rect after two frames and calls the desktop-local
  `browser_capture(x, y, width, height, viewport_width)`, which takes
  the platform webview's own snapshot of the window's visible area and
  crops it. Pixel scale comes from snapshot width / CSS viewport width,
  not from platform DPI reporting.
- Linux only for now (WebKitGTK `webkit_web_view_get_snapshot` via
  `Webview::with_webview`, cropped with cairo). macOS
  (`WKWebView takeSnapshotWithConfiguration`) and Windows (WebView2
  `CapturePreview`) slot into the same command when someone on those
  platforms needs it; until then the command returns an error saying
  so.

## Rejected alternatives

- **DOM-to-canvas in the page** (html2canvas-style, via the bridge) —
  portable, but an approximation: cross-origin images, nested frames,
  video and some CSS render wrong.
- **Child webview** — see ADR 0088; capture alone doesn't justify the
  `unstable` flag and the overlay problems.
- **Trusted input events** — no cross-platform API (only WebView2's
  DevTools protocol). Synthetic events stay until a real page needs
  more.

## Known limits

- It captures what is on screen: an IDE popover over the tab is in
  the shot, and a mounted-but-hidden tab errors as not visible.
- Needs the page bridge's hello like every other action (https tabs
  have it since ADR 0104).
