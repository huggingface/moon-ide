# Phase 14 — Remote / relay bridge

The bridge can run remotely (a relay box on the VPN, a small
always-on machine) and both IDEs and phones connect to it as clients.
Multiple IDEs enroll with the same bridge; the phone sees all their
workspaces in one switcher. Local mode is unchanged (Phase 13 / ADR
0024). Decision record: [ADR 0031 — remote / relay bridge topology +
IDE enrollment](../decisions/0031-remote-bridge-relay.md). Architectural
spec: [`companion.md`](../companion.md) § "Remote / relay mode".

## Why this phase number

Builds on Phase 13 (mobile companion), which shipped the host-local
bridge, pairing, and the PWA. 13.5 (phone-side review & commit) is the
remaining 13.x work and is **not blocked** by this phase — remote mode
is purely additive, and a phone that only knows today's protocol keeps
working.

## Scope discipline reminder

Per [AGENTS.md](../../AGENTS.md#scope-discipline): the requester asked
for a bridge that can run remotely and serve multiple IDEs, **not** for
the coder to move off the laptop. So this phase is a **relay hub**, not
headless `moon-core` — the bridge forwards bytes and holds no coder
state. The headless-core future ("work with the laptop closed, the loop
elsewhere") stays prose in `companion.md`; this phase does not build it.

## Acceptance

Per sub-phase. Land in order; stop at each gate. The team handoff gate
at the end of each sub-phase is the usual roadmap rule.

### 14.0 — Enrollment credential core + enroll-code CLI

The symmetric counterpart to Phase 13.3's pairing core. Pure
credential logic, unit-tested, no network.

- `crates/moon-bridge/src/enrollment.rs`:
  - `EnrollmentSession` — mirror of `PairingSession`: short-lived
    (120 s), single-use enrollment code; `issue` /
    `verify_and_consume` with `Expired` / `AlreadyUsed` /
    `CodeMismatch`. In-memory only, never persisted, never logged.
    Identical code shape to the pairing code (`A1B2-C3D4`).
  - `EnrolledIde` — mirror of `PairedDevice`: `{ id, label, token,
enrolled_at_ms }`. The `id` is the IDE's self-assigned `ide_id`
    (so reconnections rebind to the same registry entry), not a
    bridge-minted random — distinct from `PairedDevice` which mints
    its own id. Token = two concatenated v4 UUIDs (256 bits), as
    before.
  - `IdeStore` — mirror of `DeviceStore`: keyring-backed
    (`service=moon-ide, account=companion-ides`, one JSON blob),
    `open` / `list` / `add` / `revoke` / `ide_for_token`.
- CLI (`crates/moon-bridge/src/main.rs`):
  - `moon-bridge enroll-code` — issue + print an enrollment code (mirror
    of `pair-code`).
  - `moon-bridge ides` — list enrolled IDEs (id, label, enrolled-at).
    Mirror of `devices`.
  - `moon-bridge revoke-ide <id>` — revoke an enrolled IDE. Mirror of
    `revoke`.
- Unit tests cover issuance, single-use consumption, expiry, case-
  insensitivity, distinct tokens, and keyring round-trip (same
  keyring-backend caveat as 13.3 — only fails in the headless CI
  container).
- `cargo test` / `cargo clippy --all-targets -D warnings` / `cargo fmt`
  clean.

### 14.1 — Bridge accepts enrolled IDEs (WS server side)

The bridge gains a second inbound connection type: an enrolled IDE
dialing in over WSS. This sub-phase wires the enrollment handshake and
the IDE-side registry; it does **not** yet forward `call`/`subscribe` to
IDEs (that's 14.2).

- `serve.rs`:
  - `ClientMessage` gains `Enroll { code, label, ide_id }` and
    `Register { token, workspaces }`. `ServerMessage` gains `Enrolled
{ ide_id, token }`.
  - `ServeCtx` gains `ides: IdeStore`, an
    `enrollment: Mutex<Option<EnrollmentSession>>` (mirror of
    `pairing`), and a live-IDE table: `Arc<Mutex<HashMap<IdeId,
IdeConnection>>>` where `IdeConnection` holds the WS sink (an
    `mpsc::Sender<ServerMessage>`) and the IDE's last-reported
    workspace list.
  - `handle_enroll` — mirror of `handle_pair`: verify_and_consume the
    enrollment code, `EnrolledIde::mint` with the IDE-supplied `ide_id`
    - `label`, `IdeStore::add`, reply `Enrolled`.
  - `handle_register` — check the IDE token (`ide_for_token`, mirror
    of `check_token`), then update the live-IDE table with the
    reported workspaces. The table is the remote-carrier half of the
    `WorkspaceRegistry`.
  - Enrollment session issued at `serve` startup (alongside the phone
    pairing session) unless `--no-enrollment`. Printed to stdout like
    the pairing payload.
- `status.rs` control socket: `ControlRequest` gains `RevokeIde {
ide_id }`; `CompanionStatus` gains an `ides: Vec<IdeEntry>` field.
  The IDE's Companion panel renders the enrolled-IDEs list with
  revoke, alongside the paired-devices list.
- `src-tauri/src/commands/companion.rs`: mirror `RevokeIde` and the
  `ides` field in the local `CompanionStatus` copy; add a
  `companion_revoke_ide` Tauri command.
- `main.rs` CLI: `moon-bridge serve` gains `--no-enrollment`.
- Acceptance: against a fake IDE (a no-verify TLS WS client), enroll
  with a correct code → receive `Enrolled` with a token; re-enroll
  with the same code → rejected (single-use); `Register` with the
  token → live-IDE table updated; wrong token → rejected. The phone
  pairing flow is unaffected.

### 14.2 — Relay routes call/subscribe to enrolled IDEs

The forwarding path. A phone's `call`/`subscribe` for a
remote-carrier workspace goes over the held-open IDE WS instead of the
local Unix socket.

- `serve.rs`:
  - `handle_call` / `handle_subscribe` resolve the carrier from `(ide,
workspace)`. Empty/absent `ide` → local carrier (`relay::call`,
    unchanged). Present `ide` → look up the live-IDE table, forward
    the `call`/`subscribe` as a WS frame to that IDE's sink, and await
    the reply (with a timeout) to send back to the phone. For
    `subscribe`, the bridge subscribes a forwarding task that pipes
    the IDE's pushed events to the phone's sink until either side
    drops.
  - `handle_workspaces` returns the union of local-carrier
    (discovery) and remote-carrier (live-IDE table) workspaces, each
    tagged with its `ide` (empty for local).
  - Request ids: the bridge↔phone hop is FIFO today (sequential calls);
    the bridge↔IDE hop needs a request id so a forwarded `call` can
    match its reply when the IDE has multiple in-flight. Add an `id`
    field to the bridge↔IDE frame (the `R`/`S` JSON-RPC shape already
    supports it via `params`, but a top-level `id` on the WS frame is
    cleaner). The phone↔bridge hop stays FIFO.
- `relay.rs`: add `call_remote` / `subscribe_remote` that take an
  `IdeConnection` sink instead of a socket path. Same `RpcRequest` /
  `RpcResponse` framing, different transport.
- Acceptance: a fake IDE enrolled + registered with a workspace; a
  fake phone `call`s a method on that workspace and gets the IDE's
  reply back through the bridge; `subscribe` streams events from the
  fake IDE to the fake phone. Local-carrier `call` (to a real
  `instance.sock`) still works unchanged.

### 14.3 — IDE-side outbound WS client + enrollment UI

The IDE becomes a WS _client_ with a persistent outbound connection to a
remote bridge, plus the UI to enroll.

- `src-tauri/src/remote_bridge.rs` (new):
  - A persistent task that, given a bridge URL + an IDE token (from the
    IDE's own keyring), opens a WSS connection, TOFU-pins the bridge
    cert, sends `Register` with the IDE's live workspaces, and
    answers `call`/`subscribe` frames by dispatching to the same
    `BridgeRpc` the focus listener uses (reused unchanged). Reconnect
    with exponential backoff on drop; the stored token means a
    reconnect, not a re-enrollment.
  - On a workspace-set change (new window / workspace closed), re-send
    `Register` so the bridge's registry stays live without polling.
  - Keyring: store `{ bridge_url, ide_id, token }` under
    `service=moon-ide, account=remote-bridge` in the IDE's own
    keyring.
- Desktop UI:
  - Command palette entry "Companion: Connect to remote bridge…"
    opens a modal: bridge URL + enrollment code inputs, an "Enroll"
    button, the cert fingerprint (TOFU), and once enrolled, the
    connection status + a "Disconnect" button. Backed by new Tauri
    commands `companion_enroll` / `companion_remote_status` /
    `companion_remote_disconnect`.
  - The existing Companion modal gains a "Remote bridges" section
    listing enrolled bridges with disconnect, mirroring the
    paired-devices list.
- Acceptance: from a real IDE, enroll with a remote bridge (started by
  hand on another machine / port), see the IDE's workspaces appear in
  the phone's switcher (grouped under the IDE's id), `call` a method
  from the phone and get the reply, `subscribe` to stream coder events.
  Dropping the VPN → reconnects without re-enrollment. Local mode
  (auto-spawned bridge) still works alongside.

### 14.4 — PWA: grouped workspace switcher

The phone's `WorkspaceList` groups workspaces by IDE (the new `ide`
field), so a multi-IDE bridge is legible. Pure frontend; the bridge
already sends the `ide` tag from 14.2.

- `companion/src/lib/transport.ts` / `app.svelte.ts`: the `workspaces`
  reply carries `ide` per entry; `WorkspaceList.svelte` groups by it
  (local-carrier workspaces under a "This machine" header, remote IDEs
  under their label).
- Acceptance: with two IDEs enrolled to one bridge, the phone's
  switcher shows two groups; selecting a remote workspace drives it
  end to end.

## What this phase deliberately doesn't do

Prose, not milestones — mirrors
[`companion.md`](../companion.md) § "What remote mode deliberately
doesn't do":

- **Move the coder loop off the IDE.** The bridge forwards bytes; it
  does not adopt the loop. (Headless-core future, prose only.)
- **Auto-forward IDE listening ports through the bridge.** Violates
  the explicit-forward invariant.
- **mTLS for IDE ↔ bridge.** Bearer tokens match the existing posture;
  mTLS is a documented follow-up, not v1 of this phase.
- **Public-internet exposure.** VPN / trusted network only, same as
  Phase 13.

## Open questions

- **Reconnect backoff tuning.** A dropped VPN should heal without
  re-enrollment. Settle during 14.3 against what the team's network
  actually does — start with exponential backoff capped at 30 s.
- **Does the IDE enroll with one bridge or many?** v1 supports one
  enrolled bridge per IDE (the common case — one relay box). The
  keyring shape (`{ bridge_url, ide_id, token }`) doesn't preclude a
  list later; revisit if someone asks for multi-bridge.

## Test-plan links

Each new network / pairing boundary earns a plan when the sub-phase is
handed back for review (per [test-plans/README.md](../test-plans/README.md)):
the enrollment handshake (14.1), the relay forwarding (14.2), the
IDE-side client + enrollment UI (14.3). Filled in as they land.
