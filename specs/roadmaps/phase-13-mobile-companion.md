# Phase 13 — Mobile companion

A phone companion that drives a running moon-ide over the LAN
(typically the company VPN): run and steer coder sessions, and
review + commit. Not a mobile IDE — no editing, no terminal, no
LSP. A remote control for the agent and the SCM panel.

Architectural spec: [`companion.md`](../companion.md). Decision
record: [ADR 0023 — mobile companion via `moon-bridge`](../decisions/0023-mobile-companion-bridge.md).

## Why this phase number

Depends on [Phase 7](phase-07-multi-workspace.md) being real:
process-per-workspace ([ADR 0014](../decisions/0014-process-per-workspace.md))
and the per-workspace `instance.sock` are both the multi-workspace
answer the phone needs and the bridge's workspace-discovery
mechanism. It also reuses the coder + git surfaces (Phases 5 / 6),
which are in progress. So it lands after those, not in the
open-ended Phase 12 innovation track — it has a concrete
deliverable and a concrete requester.

## Scope discipline reminder

Per [AGENTS.md](../../AGENTS.md#scope-discipline): v1 is a
laptop-local bridge + an installable PWA doing coder + review only.
The cloud / always-on dev machine and detached overnight runs are
**futures written as prose** in `companion.md`, not milestones
here. The one early decision the cloud future forced — JSON-RPC
framing on the bridge↔process hop, not a socket relay — is already
recorded in ADR 0023 and is honoured by 13.1 below; we do not build
the cloud machine in this phase.

## Acceptance

Per sub-phase. Land in order; stop at each gate. The team handoff
gate at the end of each sub-phase is the usual roadmap rule.

### 13.0 — `moon-bridge` crate + workspace discovery (no app yet)

- New `crates/moon-bridge/` workspace member (add to
  `Cargo.toml` `members` + `[workspace.dependencies]`). A binary
  crate; depends on `moon-core` + `moon-protocol`.
- Workspace discovery: enumerate
  `<XDG_DATA_HOME>/moon-ide/workspaces/*/instance.sock`, probe
  each (connect succeeds = live owner; `ECONNREFUSED` = stale),
  return the live set with `{ id, name, last_active_at }` from the
  catalog. Reuses the same liveness probe ADR 0014 already does for
  single-instance enforcement.
- No listener, no TLS, no app. A `moon-bridge --list` debug
  subcommand that prints the discovered live workspaces is the
  acceptance surface — proves discovery works against real running
  IDE processes before any network code exists.
- `STATUS` line in `companion.md` flips the discovery section to
  partial.

### 13.1 — Bridge ↔ workspace-process relay (JSON-RPC framing)

- The bridge connects to a chosen workspace's `instance.sock` and
  relays the JSON-RPC surface + event-stream notifications over the
  **`moon-remote` framing**, not a bespoke `instance.sock` verb set
  (ADR 0023 § "Why JSON-RPC framing, not a socket relay"). If
  `moon-remote`'s framing module doesn't exist yet, this sub-phase
  creates it as the shared crate both `moon-bridge` and the future
  `moon-remote` consume — that is the "build the network framing
  once" payoff.
- The workspace process gains a relay endpoint on its
  `instance.sock` (or accepts the `moon-remote` channel) exposing
  the read-only-plus-coder-plus-git method subset the phone needs.
  Scope the exposed methods to the v1 surface; don't relay the full
  IPC surface.
- Acceptance: a host-local CLI client of the bridge can
  `coder_list_sessions` / `coder_open_session` and receive
  `coder:event` notifications for a real running workspace. No
  network, no TLS yet — Unix socket end to end, exercising the
  framing.
- `PROTOCOL_VERSION` bump if the relayed shape diverges from the
  in-process surface; per AGENTS.md no-premature-migrations, no
  compat shim.

### 13.2 — LAN listener + TLS

- Bridge binds one HTTPS + WebSocket listener (default
  `0.0.0.0:53180`). Self-signed keypair + cert generated on first
  run, persisted under `<XDG_DATA_HOME>/moon-ide/bridge/`.
- WS frames carry the same JSON-RPC the 13.1 relay speaks; the
  listener is a transport adapter in front of the relay, not a
  second protocol.
- Acceptance: a `wscat`-style client on another LAN machine (cert
  trust bypassed for the test) can drive the coder surface over
  WSS. This is the "explicit, named LAN surface" the invariant 3
  requires — one listener, deliberately bound.

### 13.3 — Pairing (TOFU cert + device tokens)

- Bridge issues short-lived pairing tokens (~120 s TTL,
  device-flow-style) and long-lived per-device tokens bound on
  successful pair. Device tokens live in the host keyring at
  `service=moon-ide, account=companion-device:<id>`.
- Desktop surfaces a **pairing QR** (Companion affordance —
  status-bar entry or small settings modal) encoding
  `wss://<lan-ip>:53180` + cert fingerprint + pairing token.
- Bridge serves the cert-trust artifact for one-time install
  (iOS `.mobileconfig`; Android user cert) so the PWA loads without
  the self-signed interstitial after pairing.
- **Paired devices** management list with per-device revoke.
- Acceptance: pair a phone browser end to end; the token persists;
  revoking it from the desktop drops the connection.

### 13.4 — Companion PWA: coder

- Svelte 5 + Vite SPA the bridge serves over HTTPS, installable to
  home screen. Reuses the IDE's existing coder Svelte components
  (`CoderMessage`, `CoderToolCall`, the markdown pipeline) against
  a WSS transport adapter instead of Tauri `invoke`.
- Workspace switcher: the list of live (and launchable) workspace
  processes from 13.0's discovery. Launch a not-running workspace
  via `moon-ide --workspace <slug>` (the bridge spawns it, same as
  `window_open`).
- Coder surface: session list / open / new, transcript render off
  `coder:event`, send / steer (`coder_send`), abort
  (`coder_abort`). Concurrent / background sessions already work
  backend-side (ADR 0016); the phone renders the per-`(folder,
session)` buckets the desktop already routes.
- Acceptance: from a paired phone, pick a workspace, open a coder
  session, kick off a turn, watch it stream, steer it, abort it.

### 13.5 — Companion PWA: review & commit

- Read-mostly diff review + commit / amend / sync over the existing
  [git layer](phase-05-git.md). Diffs render on a phone; no
  full-file editing.
- Acceptance: from a paired phone, review the working-tree diff of
  a folder and land a commit + sync.

## What this phase deliberately doesn't do

Prose, not milestones — revisit when someone asks. Mirrors
[`companion.md` § "What this deliberately doesn't do (v1)"](../companion.md#what-this-deliberately-doesnt-do-v1):

- Full file editing / terminal / LSP on the phone.
- Background agent-watching with the screen off (PWA WS drops on
  background; v1 reconnects on resume) — the trigger for a future
  Tauri-mobile wrapper.
- Detached / overnight runs surviving the laptop closing — needs an
  always-on headless core (the cloud / always-on future in
  `companion.md`), not a phone-side feature.
- Multi-account, public-internet exposure, Windows host bridge
  (inherits ADR 0014's Unix-domain-socket limitation).

## Open questions

- **Does `moon-remote`'s framing module land here or in its own
  phase?** ADR 0023 says the bridge and `moon-remote` converge on
  one "headless core serving JSON-RPC over a channel" shape. If the
  remote-host story hasn't started by 13.1, this phase creates the
  shared framing crate; if it has, 13.1 consumes it. Decide at 13.1
  based on what exists then.
- **Relay method allowlist.** Exactly which methods the phone-facing
  relay exposes (coder + git + workspace-list, read-mostly) wants a
  concrete list in `companion.md` before 13.1, so the surface is a
  reviewed allowlist rather than "whatever the IDE has".
- **QR-scan vs. manual entry on the phone.** Camera QR scan via
  `getUserMedia` is the happy path; a typed `wss://…` + code
  fallback may be enough for v1 and avoids a camera-permission
  prompt. Settle during 13.3 against what the team actually finds
  comfortable.

## Test-plan links

Each network / pairing / new-UI boundary earns a plan when the
sub-phase is handed back for review (per
[test-plans/README.md](../test-plans/README.md)): the bridge relay
(13.1), TLS + pairing (13.2 / 13.3), and the PWA coder + review
surfaces (13.4 / 13.5). Filled in as they land.
