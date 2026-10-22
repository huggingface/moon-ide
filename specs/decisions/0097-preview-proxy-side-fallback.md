# ADR 0097: Browser-tab loopback URLs fall back across the container boundary

Amends the dial rule of [ADR 0088](0088-in-ide-browser-tab.md).

## Context

A tab's URL resolves on one side only — the opening session's `bash`
side. A `localhost` server on the other side (agent session routed to
the host while the dev server runs in `dev`, or the reverse) got a
`502 could not reach` page, and the user had to declare a port forward
by hand — exactly what ADR 0088 exists to avoid. Container dial failures
were also opaque: ncat's stderr was discarded and a refused dial only
showed up as "connection closed before message completed".

## Decision

- A preview proxy dials an ordered list of sides. For loopback hosts
  (`localhost`, `127.0.0.0/8`, `::1`; `0.0.0.0` already maps to
  `localhost`) the list is the tab's side, then the other one. Other
  hosts (service names, real hostnames) stay single-sided.
- The container dial runs `ncat -v` and waits for its `Connected to`
  line before handing the stream to HTTP, so a refused dial fails fast
  and the next side is tried. The 502 body lists each side's reason
  (ncat's message, "container not running", "ncat not installed").
- The tab's `in_container` flag, and so the proxy's origin, is
  unchanged: it only decides which side is tried first.
- The `open_browser` tool description says no forward or `0.0.0.0`
  bind is needed, so agents stop asking the user for one.
- An agent-opened tab records the session's folder (`BrowserTab.folder`)
  and lands in that folder's tab strip — its worktree after
  `enter_worktree` — rather than whichever folder is active. Focusing
  it later (e.g. `browser_page` needs the iframe mounted) still moves
  it to the active folder, per ADR 0088.

## Rejected alternatives

- **Probe once at resolve time and pick a side** — a server started
  after the tab opened (or restarted on the other side) would stay
  unreachable until the proxy is recreated.
- **Auto-declare a forward on failure** — same objections as in ADR
  0088, and it still can't reach container loopback.
