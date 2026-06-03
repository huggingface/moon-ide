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

### 13.0 — `moon-bridge` crate + workspace discovery (no app yet) — LANDED

- New `crates/moon-bridge/` workspace member (binary crate;
  depends on `moon-core` + `moon-protocol`, mirrors `moon-remote`'s
  shape — a binary, so it's a `members` entry but not a
  `[workspace.dependencies]` lib).
- Workspace discovery (`src/discovery.rs`): enumerate
  `<data_local_dir>/moon-ide/workspaces/*/`, probe each
  `instance.sock` (connect within a 250 ms timeout = live owner;
  refused / missing = stale or stopped), decorate with `name` +
  `last_active_at` from the `state.json` catalog
  (`moon_core::app_state::load`) when present, falling back to the
  slug. Sorted live-first, then most-recently-active, then slug.
  Re-implements the few lines of the focus-socket liveness probe
  rather than depending on the `src-tauri` binary crate, so the
  bridge stays a leaf linking only `moon-core` + `moon-protocol`.
- `moon-bridge list` (`--json` for machine-readable) is the
  acceptance surface. Verified end-to-end: a real `UnixListener`
  bound at a workspace's `instance.sock` reports `running`; a
  stale empty socket file reports `stopped`; a missing workspaces
  dir is an empty list, not an error. Unit tests cover all four
  paths.
- No listener, no TLS, no app, no relay. `cargo test` /
  `cargo clippy --all-targets -D warnings` / `cargo fmt` clean.

### 13.1 — Bridge ↔ workspace-process relay (JSON-RPC framing) — LANDED

- `moon_protocol::focus_socket` grew an `R\n<json>\n` request kind
  carrying an `RpcRequest` (`{ method, params }`) with a single-line
  `RpcResponse` (`{ ok? , error? }`) reply, alongside the existing
  `F` / `E`. `PROTOCOL_VERSION` → 2.
- Workspace process: `src-tauri/src/bridge_rpc.rs` implements a
  `BridgeRpcHandler` (new trait on `focus_socket`) bound to the
  coder + registry, dispatched from the focus listener. The listener
  spawn moved to after the coder is built so the handler has them.
- Methods wired so far (all read-only): `coder_status`,
  `coder_list_sessions`, `coder_active_session`, `workspace_snapshot`,
  `bridge_methods`. Not a security fence — pairing is the boundary, a
  paired device can drive the coder, which runs anything via `bash`.
  Mutating methods (`coder_send`, commit) land with the PWA screens
  that call them.
- `moon-bridge` side: `relay::call` connects to a workspace's
  `instance.sock`, sends one `R` request, reads the response;
  `moon-bridge call <ws> <method> [--params JSON]` is the surface.
- Verified end-to-end: a fake workspace listener using the real
  protocol framing answers `moon-bridge call huggingface coder_status`
  / `bridge_methods`; unknown method exits non-zero. No network / TLS
  yet — Unix socket end to end, exercising the framing.

### 13.1b — Sharing the framing with `moon-remote`

- `moon-remote` doesn't exist as a server yet, so 13.1 put the JSON
  shape in `moon_protocol::focus_socket` (where `F`/`E` already
  live). When the remote-host story starts, lift the `Rpc*` types
  into whatever shared module `moon-remote` wants and have both
  consume it — the "build the framing once" payoff, deferred until
  there's a second consumer.

### 13.2 — LAN listener + TLS — LANDED

- `moon-bridge serve` binds one TLS + WebSocket listener (default
  `0.0.0.0:53180`). `tls.rs` generates-or-loads a self-signed cert +
  key under `<data_local_dir>/moon-ide/bridge/` (stable fingerprint
  across restarts so a pinned phone keeps trusting it) via `rcgen`;
  TLS accept is `tokio-rustls` (ring provider), WS upgrade is
  `tokio-tungstenite` over the TLS stream — no second protocol, the
  listener is a transport adapter in front of `relay::call`.
- `serve.rs` connection flow: TLS accept → WS upgrade → one JSON
  message per frame, tagged `pair` or `call`. `call` authenticates
  the device token against the `DeviceStore` (the whole boundary)
  then relays to the workspace process; `pair` verifies the startup
  code and mints a device (closing the 13.3 loop).
- Startup emits the `PairingPayload` (the QR contents): `wss://<lan-
ip>:<port>` + cert fingerprint + pairing code. LAN IP is detected
  via the UDP-connect trick (no interface-enum dep).
- Verified end-to-end against a fake phone (no-verify TLS client) +
  fake workspace: TLS handshake → WS upgrade → routing → code verify
  (correct accepted, wrong rejected with "did not match" _before_
  any token store touch) → token-auth gate → relay round-trip. The
  device-token persistence half (keyring `add` / `device_for_token`)
  only fails in the headless CI container, which has no secret-
  storage backend; it's the same keyring the coder / Slack tokens
  already use on real machines.

### 13.3 — Pairing (TOFU cert + device tokens)

Credential core LANDED early (`moon-bridge/src/pairing.rs`):

- `PairingSession` — short-lived (120 s TTL), single-use pairing
  code; `issue` / `verify_and_consume` with expiry + replay
  rejection. Pure in-memory, unit-tested. `moon-bridge pair-code`
  issues one.
- `DeviceStore` — keyring-backed (`service=moon-ide,
account=companion-devices`, one JSON blob) registry of revocable
  per-device bearer tokens; `add` / `list` / `revoke` /
  `device_for_token`. `moon-bridge pair <label>` / `devices` /
  `revoke <id>` are the surface; the token prints once.

Still to wire (needs the 13.2 listener): the QR payload encoder
(`wss://<lan-ip>:53180` + cert fingerprint + pairing code), the
listener's token check on connect (`device_for_token`), and the
one-time cert-trust artifact (iOS `.mobileconfig` / Android user
cert). The desktop-side Companion affordance (QR display, paired-
devices UI) lands with that.

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
