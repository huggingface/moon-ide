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

### 13.2b — Bridge lifecycle: IDE auto-starts it, self-exits when idle — LANDED

- Decision: [ADR 0024](../decisions/0024-bridge-lifecycle.md). The
  user never runs the bridge by hand; running the IDE makes the
  companion reachable.
- `serve` owner-election: binding the LAN port _is_ the election — a
  second bridge hits `AddrInUse` and exits 0. So every IDE can
  fire-and-forget a `serve` child without coordinating.
- `serve` idle watcher: a 30 s tick (after a 30 s startup grace)
  exits the process when discovery reports zero live workspaces —
  "last IDE closed" needs no IPC, it's the same signal the switcher
  reads.
- IDE side (`ensure_bridge_running`, release builds only): spawns a
  detached `moon-bridge serve --web-root <dist>` child after setup,
  resolving the binary + companion assets next to the exe. Best-
  effort — a missing binary / dist is logged and skipped, never
  blocks launch. Dev builds skip it (run `moon-bridge serve` by hand).
- Verified live: two `serve`s on one port → second logs "already owns
  … exiting"; a `serve` with no live workspace self-exits exactly
  60 s in.
- Build wiring, **both paths** (ADR 0024 § Consequences):
  - `--no-bundle` (`bun run build:bin`): builds companion + IDE +
    `moon-bridge`, then `stage-bridge.mjs exe-adjacent` drops the
    bridge + `companion/` into `target/release/` next to
    `moon-desktop`. Verified the three land side by side and the
    staged binary serves the staged PWA.
  - bundled (`bun run build`): `stage-bridge.mjs prepare` populates
    `src-tauri/resources/bridge/` _before_ `tauri build`, so tauri's
    `bundle.resources` ships `bridge/{moon-bridge, companion/}` into
    the app resource dir. `ensure_bridge_running` resolves it via
    `BaseDirectory::Resource`, falling back to exe-adjacent, and
    `chmod +x`es the binary (resources can lose the exec bit). We
    bundle the binary as a resource and spawn it detached ourselves —
    _not_ tauri's sidecar API, which would tie the bridge's lifetime
    to the app process (the coupling ADR 0024 avoids).
  - A tracked `src-tauri/resources/bridge/.gitkeep` keeps tauri-build's
    resource-path validation happy on a fresh checkout.
- **Self-host safe (bootstrap, [ADR 0005](../decisions/0005-bootstrap.md)).**
  The team builds moon-ide from a terminal _inside_ a running
  moon-ide, which has already auto-spawned the bridge — so the build
  overwrites a binary that's currently executing. `stage-bridge.mjs`
  stages the `moon-bridge` binary via write-temp + `renameSync` (not
  copy-onto-path), so `rename(2)` swaps the directory entry without
  touching the running process's inode — no `ETXTBSY`. Cargo/tauri
  already relink their own binaries atomically. Verified: a full
  rebuild with the bridge running from `target/release` exits clean.
  Contract: the build always succeeds while the IDE runs; the
  freshly-built bridge + PWA are picked up on the **next** IDE
  launch (the still-running bridge keeps serving the old binary +
  old PWA until then — restart to test changes).
- Not verified live here: an actual AppImage/.deb run — the container
  can't produce one. The bundled resolution path is verified by
  construction (compiles; resource-dir-then-exe lookup; `--no-bundle`
  spawn proven live). First real bundle build is the remaining check.

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

Wired by 13.2: the QR payload (`PairingPayload`), the listener's
`pair` handshake (`verify_and_consume` → `DeviceStore::add`), and
the per-connection token check (`device_for_token`).

Desktop Companion panel + mDNS LANDED (13.4b follow-on):

- `Companion: Pair a phone…` command palette entry opens
  `CompanionModal.svelte`: a scannable QR of the pairing payload
  (via `uqr`), the `moon-bridge.local` + IP addresses, the code, the
  cert fingerprint, and a paired-devices list with Revoke.
- mDNS (`mdns-sd`): the bridge advertises `moon-bridge.local` →
  its LAN IP, so the phone reaches it by name regardless of IP. The
  payload's `url` stays the raw IP (always works); `.local` is the
  offered alternative since multicast is blocked on some networks.
- Cross-process channel: the bridge publishes `companion-status.json`
  (payload + devices) to the bridge dir and watches
  `companion-revoke.json`; the IDE reads/writes these via
  `companion_status` / `companion_revoke_device` Tauri commands. No
  shared keyring writer (bridge stays sole owner), no second socket.

Cert SANs cover the host's detected LAN IP (+ `moon-bridge.local`,
`localhost`, loopback), so a browser hitting `https://<ip>:port`
doesn't reject on a name mismatch after the user trusts the cert.
The cert is stable for a fixed IP (the phone's pinned fingerprint
keeps working); a network change regenerates it once — a deliberate,
logged re-pair — tracked by a `bridge-cert-sans.txt` marker.

Still to wire: the one-time cert-trust artifact (iOS `.mobileconfig`
/ Android user cert) so the browser stops warning at all — for now
the user accepts the self-signed cert once per device.

### 13.4 — Companion PWA: shell + workspace switcher + coder read — LANDED (first slice)

- New `companion/` Vite + Svelte 5 app (own `vite.config.ts` /
  `tsconfig.json` / `svelte.config.js`, builds to `companion/dist`).
  Root `package.json` gains `build:companion` / `dev:companion` /
  `check:companion`; the latter joins `bun run check`.
- `transport.ts` is the WSS equivalent of `invoke`: `BridgeSocket`
  opens the socket, `pair` / `workspaces` / `call` map to the
  bridge's message shapes, the device token persists in
  `localStorage` (FIFO reply matching since calls are sequential).
- `app.svelte.ts` (runes, single store) drives three screens:
  `PairScreen` (paste the QR payload or type url+code), `WorkspaceList`
  (the switcher — live pip per workspace, from a new bridge-level
  `workspaces` message backed by 13.0 discovery), and `WorkspaceView`
  (coder status + session list for the picked workspace).
- The bridge serves the built PWA: `http.rs` reads the request head
  and branches static-GET vs WS-upgrade on the one TLS port; `serve
--web-root companion/dist` turns it on. Path-traversal blocked,
  SPA fallback to `index.html`.
- Verified end-to-end over HTTPS (curl -k): index / manifest / JS
  asset with correct content-types, SPA fallback, traversal blocked.
- **Read-only first slice.** Sending prompts / steering / abort
  (`coder_send` / `coder_abort`) need the relay's mutating methods,
  which land with the screen that calls them (next). The switcher +
  status + session list exercise the whole stack today.

### 13.4b — Coder send / abort + streaming transcript — LANDED

- Relay gained the mutating methods `coder_send` / `coder_abort` /
  `coder_open_session` (drive the active folder's visible session —
  the one the desktop has open; per-folder/session targeting from the
  phone is a later refinement).
- Streaming: a new `S` (Subscribe) request kind on
  `moon_protocol::focus_socket` (alongside `R`) lets the workspace
  push many `RpcResponse` lines on one held-open connection.
  `BridgeRpcHandler::subscribe` bridges the coder's
  `broadcast::Receiver<CoderEventEnvelope>` to an mpsc of JSON the
  focus listener forwards. `relay::subscribe` drains it; the bridge's
  WS layer splits the socket (writer task + mpsc) so pushed
  `ServerMessage::Event` frames and request/reply replies share one
  sink. The phone sends `{type:"subscribe"}`; events arrive as
  `{type:"event"}` frames.
- PWA: `transport.ts` routes pushed `event` frames to an `onEvent`
  handler (bypassing the request/reply FIFO). `app.svelte.ts` reduces
  the coder event grammar into three transcript row kinds (user /
  assistant-with-delta-accumulation / tool-with-status).
  `SessionView.svelte` renders the transcript + a composer (Enter
  sends, Stop aborts while busy).
- Verified: the streaming hop end-to-end against a fake workspace
  (3 pushed delta events drained through `relay::subscribe`);
  protocol round-trip test for `S`; full gauntlet clean. Not verified
  live: a real agent turn over the phone — needs the keyring (pairing)
  - a real IDE, same constraint as 13.2/13.4.
- Deferred: image attachments in the phone composer; per-session
  targeting; rich tool-body rendering (the phone shows tool name +
  status, not the desktop's expandable input/output).

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
