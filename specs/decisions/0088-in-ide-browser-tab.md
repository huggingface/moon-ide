# ADR 0088: In-IDE browser tabs the agent can drive

## Context

An agent that starts a dev server ends with "open http://localhost:5173
to check it". The user has to switch to a browser, and when the server
runs in the workspace container it isn't reachable at all unless a port
forward ([containers.md § Network and port forwarding](../containers.md#network-and-port-forwarding))
was declared first — and even then only if the server listens on
`0.0.0.0`. We want the agent to show the preview inside the IDE, with
nothing to configure — and then let the agent check its UI work in
that same tab (read it, click through it, see console errors), the
first step towards replacing the Playwright MCP.

## Decision

- **Backend-owned tab registry** — `BrowserTabRegistry` (in
  `moon-coder`, one per process) holds `{ id, url, in_container }`
  per tab: the URL last asked for and which side of the container
  boundary it means. Every change broadcasts the full set as
  `browser:tabs` (`BrowserTabsChanged { tabs, focus, reload }`); the
  frontend only mirrors it, and the user's actions (URL bar, reload,
  closing the tab) go through `browser_tab_*` commands into the same
  registry. Agents and user therefore see one tab set. Not persisted.
- **Browser tab UI** — a synthetic editor tab `browser://<id>` (same
  pattern as `commit://`, skipped by persistence / LSP / blame via
  `isSyntheticBufferPath`) rendering an `<iframe>`, with a URL bar,
  reload, and "open in system browser". New tabs land in the active
  folder; focusing a tab parked in another folder moves it over.
- **Coder tools** — `open_browser(url)` (focuses + reloads an
  existing tab on the same URL instead of duplicating, returns
  `tab_id`), `list_browser_tabs()`, and
  `browser_tab(tab_id, action: navigate|reload|focus|close, url?)`.
  URLs resolve on the session's `bash` side (`host` / `container`,
  honouring the host-mode override). Tools mutate the registry
  directly, so session replay can't re-trigger them.
- **Every `http` tab loads through a preview proxy** —
  `browser_resolve_url(url, in_container)` rewrites the URL onto
  `http://127.0.0.1:<ephemeral>`, an HTTP/1 proxy on host loopback.
  Upstream it dials directly (host) or via
  `docker exec -i <dev> ncat <host> <port>` (container) — the dial
  happens _inside_ the container, so container-loopback servers work
  (no `--host` needed), compose service names resolve, and no port
  forward is involved. One proxy per target per IDE process, so a
  target keeps a stable origin (cookies) until restart; one upstream
  connection per downstream keep-alive connection; `Upgrade`
  (WebSocket/HMR) is spliced through. `0.0.0.0` dials `localhost`.
  `https` isn't proxied: host URLs load directly (no bridge),
  container ones are refused.
- **Page bridge injected by the proxy** — HTML responses (requested
  identity-encoded) get an inline `<script>` right after `<head>`
  (`preview_bridge.js`), with `X-Frame-Options` and CSP dropped so the
  page frames and the script runs. The bridge talks to the IDE over
  `postMessage` (it only runs as a direct child of the IDE window, and
  only obeys messages from it): it reports `hello`/location changes
  (so the tab's URL tracks in-page navigation) and serves requests —
  `snapshot` (visible-content outline with `[eN]` element refs),
  `click` / `type` / `select` / `press` (by ref or CSS selector),
  `eval`, `console` (captured log + uncaught errors), `wait_for`.
- **`browser_page(tab_id, action, …)` coder tool** — the registry
  turns it into a `browser:page_request` event with a request id and
  awaits `browser_page_respond` (timeout ~20 s). The frontend posts it
  into the tab's iframe, focusing the tab first if it isn't mounted and
  holding the request until the reloaded page's bridge says hello.
  Acting actions return a fresh snapshot. `click`/`type`/`select`/
  `press`/`eval` count as writes (refused in read-only modes);
  `snapshot`/`console`/`wait_for` don't.

This is compatible with the explicit-forwarding invariant: nothing is
published or auto-detected; a proxy exists only for a URL a tab asked
for, bound to host loopback, gone with the process.

## Rejected alternatives

- **Auto-declare a port forward** — mutates the persisted forward set
  and restarts the socat sidecar (dropping other forwards'
  connections), needs a free host port, and still can't reach
  container loopback.
- **Dev container bridge IP** — Linux-only (Docker Desktop doesn't
  route it), still needs `0.0.0.0` binds.
- **Custom URI scheme proxy** (`moonport://…`) — HTTP-only (no
  WebSocket, so no HMR), and a non-http origin breaks apps.
- **Frontend-owned tabs, UI reacting to tool results** (the first
  cut) — the agent couldn't see tabs it didn't open or the user's
  navigation, and every new action needed replay-vs-live filtering.
- **Raw TCP tunnel** (the first cut) — enough to display a page, but
  cross-origin leaves no way to reach into it.
- **Tauri child webview** instead of an iframe — native `eval` and
  real screenshots, but needs the `unstable` multi-webview feature,
  manual positioning over the layout, and it paints above all HTML
  (menus, popovers). Revisit if screenshots or trusted input events
  become a real need.
- **Headless browser + CDP** (the Playwright MCP shape) — a different
  page from the one the user is looking at.

## Known limits

- One `docker exec` per TCP connection (~100 ms setup); fine with
  keep-alive, noticeable on connection-heavy pages.
- The dev image needs `ncat` (`moon-base` ships it).
- Synthetic events (`isTrusted` false); no screenshots.
- HTML responses are buffered whole to inject the bridge (no streamed
  SSR progressive rendering); CSP is dropped.
- Absolute URLs to the original origin (`http://localhost:5173/…`)
  leave the proxy.
- The iframe reloads when its tab is re-shown (the pane rebuilds the
  view on tab switch), which also resets page state and the console
  buffer.
