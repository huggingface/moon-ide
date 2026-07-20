# ADR 0035 — Public nginx-fronted relay bridge deployment

Date: 2026-07-20
Status: accepted; deploys [ADR 0031](0031-remote-bridge-relay.md)'s
relay hub on a public VPS, deviating from its "VPN / trusted network
only" posture.

## Context

ADR 0031 designed the remote relay bridge for a box on the team VPN and
explicitly rejected public-internet exposure. The operator now wants the
relay on a personal VPS (`bridge.coyo.dev`) that already runs nginx with
Let's Encrypt certs and wildcard DNS — there is no shared VPN between
the IDE hosts and the phones in this setup, so "public with tokens as
the boundary" is the actual requirement, accepted knowingly.

Two properties of the shipped code make this workable without weakening
anything:

- **The auth model never depended on the network.** Pairing/enrollment
  windows are 120 s single-use codes only open right after `serve`
  starts; everything after is a 256-bit revocable bearer token. Exposure
  widens who can _reach_ the listener, not who can pass it.
- **The IDE's outbound WS client verifies TLS against system roots**
  (`rustls-tls-native-roots`), so a real cert in front is the path of
  least resistance — the client would actually reject the bridge's
  self-signed cert if dialed directly.

## Decision

Run `moon-bridge serve` on the VPS bound to loopback, with nginx
terminating public TLS (Let's Encrypt) and proxying WebSocket upgrades
to the bridge's own TLS listener (`proxy_pass https://127.0.0.1:53180`,
double TLS on loopback — harmless, avoids a plaintext-listener mode).
Phones and IDEs connect to `wss://bridge.coyo.dev` (port 443) and see a
publicly-valid cert: no TOFU interstitial, standard verification on the
IDE hop.

Two `serve` flags added for this shape:

- `--no-idle-exit` — the ADR 0024 idle watcher exits the bridge when no
  _local_ workspace is live; a standing relay has none, ever, and would
  exit seconds after start. Local auto-spawned bridges must not set it.
- `--advertise-url <wss://…>` — the pairing payload otherwise advertises
  `wss://<bind-host>:<bind-port>`, which is wrong behind a proxy.

Deployment details (systemd unit under a dedicated `moon-bridge` user,
`gnome-keyring-daemon` unlocked in a private D-Bus session because the
keyring backend needs a Secret Service even headless) live on the box
and in the unit file, not here.

## Alternatives considered and rejected

- **Expose the bridge's own TLS listener on a public port.** Works for
  phones (TOFU) but the IDE client verifies against system roots and
  would refuse the self-signed cert; teaching it a pin-on-first-use
  verifier is more code than fronting with the cert infrastructure the
  box already has.
- **A plaintext `--behind-proxy` listener mode.** Saves one loopback TLS
  wrap; costs a mode in which a misconfigured bridge serves cleartext.
  Not worth it.
- **WireGuard/Tailscale to keep the VPN-only posture.** Adds a VPN
  client to every IDE host and phone for a single-operator deployment;
  the token boundary was already the load-bearing control in ADR 0031's
  threat model ("enrollment is the boundary; what the relay exposes is a
  scope decision, not a safety one").

## Consequences

- The pairing/enrollment codes print to the journal at service start;
  pairing a new phone or enrolling a new IDE means restarting the
  service and reading `journalctl` within 120 s. Acceptable for one
  operator; revisit (control-socket verb to reopen a window) if it
  chafes.
- The relay sees plaintext JSON-RPC after TLS termination — coder
  transcripts, file contents. The VPS is the trust boundary, same as
  ADR 0031's relay box, just with a hostile network around it.
- mTLS remains the documented follow-up if this ever serves more than
  the one operator's devices.
