# ADR 0104: https browser tabs go through the preview proxy

Amends [ADR 0088](0088-in-ide-browser-tab.md) ("`https` isn't
proxied").

## Context

https tabs loaded directly (host) or were refused (container), so
they had no page bridge: no `browser_page` actions, no screenshots,
and container https dev servers couldn't be opened at all.

## Decision

- `https` URLs use the same per-target proxy as `http`, with TLS on
  the upstream leg (rustls). The iframe still loads
  `http://127.0.0.1:<port>`, so the bridge is injected as usual.
  Container-side dials put TLS on top of the `ncat` tunnel.
- Certificates: loopback hosts and container-side dials accept any
  certificate (dev servers run self-signed / mkcert certs); public
  hosts dialed from the host are verified against the webpki roots.
- For TLS upstreams the proxy sends the upstream's own `Host` and
  `Origin` (real sites and host-checking dev servers reject the
  proxy's), strips `Secure` from `Set-Cookie` (the frame is plain
  http), and — for every upstream — rewrites `Location` redirects to
  the upstream's own origin into path-relative ones so they stay in
  the proxy.

## Rejected alternatives

- **Serve the frame over https too** (a local CA) — needs a trusted
  certificate in the webview's store; the frame being http is
  invisible to pages that don't hardcode their scheme.
- **Inject the bridge without a proxy** — a cross-origin https
  iframe can't be scripted from the IDE.

## Known limits

- Absolute links and fetches to the original origin still leave the
  proxy (unchanged from ADR 0088); for https they then load without
  the bridge.
- Pages that require a secure context for an API the webview doesn't
  grant `http://127.0.0.1` lose that API.
- HTTP/1.1 upstream only (no ALPN h2).
