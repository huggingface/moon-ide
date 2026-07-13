# ADR 0031 — Remote / relay bridge topology and IDE enrollment

Date: 2026-06-22
Status: accepted; refines [ADR 0023](0023-mobile-companion-bridge.md)
(the companion bridge) and [ADR 0024](0024-bridge-lifecycle.md) (its
lifecycle). Fills in the "Cloud / always-on future" that `companion.md`
left as prose and ADR 0023 deliberately deferred — now that a concrete
requester wants it.

## Context

ADR 0023 ships a **host-local** `moon-bridge`: the IDE auto-spawns one
daemon per machine (ADR 0024), the phone pairs to it over the LAN, and
the bridge reaches workspace processes by enumerating their
`instance.sock` files on the shared filesystem. That whole design rests
on one assumption: **the bridge and the IDE processes share a host**, so
the bridge can _find_ IDEs by reading a directory and _reach_ them over a
Unix socket.

A team member now wants to run the bridge somewhere else — a relay box
on the VPN, a small always-on machine in the closet — and have **both**
the IDE(s) and the phone(s) connect to it as clients. The motivating
properties:

- **The bridge is not bound to one IDE.** Multiple IDEs (different
  laptops, or several workspaces on one laptop) can enroll with the same
  bridge, and the phone sees all their workspaces in one switcher.
- **The bridge can be spawned locally _or_ remotely** — an operator
  choice, not a build-time property. Local mode is exactly today's
  ADR 0024 behaviour; remote mode is the new path.
- **Secure by construction.** Tokens, not open sockets. The existing
  pairing vocabulary (TOFU cert pin + short-lived single-use code →
  long-lived revocable bearer token) already works for phones; the
  remote path needs the _symmetric_ relationship (IDE ↔ bridge) to work
  the same way.

Two things from the existing specs shaped this decision:

1. ADR 0023 already bet that the bridge ↔ process hop would use
   JSON-RPC framing, **not** a bespoke socket relay, precisely so "phone
   → my laptop" and "phone → cloud box" become the same code path behind
   different channels. The framing decision is locked in; the topology
   was left as prose in `companion.md` § "Cloud / always-on future".
   This ADR makes the topology real.
2. `companion.md` lists two prerequisites the cloud future needs, and
   forbids deepening the coupling that would block it: the loop must
   stay owned by `moon-core` (not a UI lifetime), and sessions must stay
   on the machine that runs the core. **This ADR keeps both.** The
   remote bridge is a _relay_, not a _core_ — the coder loop, the
   session JSONL, the git layer all stay on the IDE host. The bridge
   forwards bytes; it does not adopt the loop. This is the decisive
   difference from the "headless `moon-core`" shape ADR 0023 speculated
   about, and the reason this ADR exists as its own decision rather than
   a footnote to 0023.

## Decision

### Relay hub, not headless core

The remote bridge is a **relay hub**: a process that accepts two kinds
of inbound WebSocket connections and forwards JSON-RPC between them. It
holds **no coder state, no sessions, no git layer** — those stay on the
IDE host, exactly where they are today. The bridge's only state is the
pairing/enrollment registries and a live-connection table.

```
remote / relay mode:

 IDE-A (laptop) ──(outbound WSS, enrolled)──► bridge
 IDE-B (laptop) ──(outbound WSS, enrolled)──► bridge
 phone ──(WSS, paired)────────────────────────► bridge

 the bridge routes call/subscribe from a phone to the IDE that owns
 the target workspace, and streams events back. The coder loop never
 moves off the IDE host.
```

This is chosen over the "headless `moon-core` serving JSON-RPC" shape
ADR 0023 deferred, for one reason: **the requester asked for a bridge
that can run remotely and serve multiple IDEs, not for the coder to move
off the laptop.** Headless core is a much larger change (sessions migrate
to the bridge machine; the detached-loop constraint from `companion.md`
becomes a build, not a constraint) and answers a different question
("work with the laptop closed, the loop running elsewhere") that nobody
has asked yet. The relay hub answers the actual ask with the minimum
moving part: a forwarding daemon. If the headless-core future is later
requested, it supersedes this ADR with a new one — but the framing
decisions both rely on (JSON-RPC over a channel) carry forward
unchanged, which is exactly what ADR 0023 spent the early decision on.

### Discovery inverts: IDEs register, the bridge no longer enumerates

Local mode (ADR 0024, unchanged) discovers IDEs by enumerating
`instance.sock` files on the shared filesystem — possible only because
bridge and IDE share a host. Remote mode **cannot** enumerate a remote
machine's filesystem, so discovery inverts: **the IDE dials out to the
bridge and registers its workspaces.** The bridge holds a
`WorkspaceRegistry` fed by two sources:

- **Local carrier** — the `instance.sock` enumeration (today's path,
  unchanged). A workspace whose socket the bridge can probe is
  local-carrier.
- **Remote carrier** — the set of currently-enrolled IDE connections,
  each reporting its live workspaces. A workspace belonging to an
  enrolled IDE is remote-carrier.

`call`/`subscribe` route to whichever carrier owns the target
workspace: a local-carrier workspace goes over the Unix socket
(`relay::call`, unchanged); a remote-carrier workspace goes over the
held-open IDE WebSocket (a new forwarding path in `serve.rs`). The
JSON-RPC framing on both hops is identical — that is the payoff of
ADR 0023's framing decision. The phone's `workspaces` reply is the
union, with each entry namespaced by IDE so the switcher can group
them.

### Enrollment mirrors pairing — same vocabulary, symmetric relationship

Today only phones authenticate. Remote mode adds the symmetric
relationship (IDE ↔ bridge) using the **exact same** vocabulary, so
there is one security model, not two:

1. The bridge generates its TLS keypair + self-signed cert on first run
   (today's `tls.rs`, unchanged).
2. The operator runs `moon-bridge enroll-code` (mirror of `pair-code`)
   to issue a short-lived (120 s), single-use **enrollment code**.
3. In the IDE, a "Connect to remote bridge" affordance (command palette
   entry, not a keybinding — Ctrl+T is already `next_edit_complete`)
   takes the bridge URL + the enrollment code. The IDE **TOFU-pins the
   bridge cert** (same as a phone), presents the code, and the bridge
   mints a long-lived **IDE token** stored in the bridge's keyring at
   `service=moon-ide, account=companion-ides` (a 1:1 mirror of
   `companion-devices`).
4. The IDE stores its token in **its own** keyring and reconnects with
   it on restart — no re-enrollment per launch.
5. A **Paired IDEs** list with per-IDE revoke is the management
   surface, alongside the existing paired-devices list.

`EnrolledIde` / `IdeStore` are deliberate mirrors of `PairedDevice` /
`DeviceStore` in `pairing.rs`. The enrollment handshake (`enroll` client
message → `enrolled` server message) mirrors `pair` → `paired`. The
token check on the IDE side's connection mirrors `check_token` for
phones. **No per-method ACL** behind enrollment — same threat model as
pairing: an enrolled IDE can drive the coder, which runs anything via
`bash`. Enrollment is the boundary; what the relay exposes is a scope
decision, not a safety one.

### mTLS is a documented future, not v1

Mutual TLS (client certs for IDEs) is the strict stronger option and is
left as a follow-up, not built now. Reasons: bearer tokens match the
existing posture (phones use them; the coder's HF/Slack tokens use
them), they're simpler to rotate and revoke, and the threat model
(enrollment is the boundary) doesn't require the stronger binding. If a
later need appears (e.g. compliance, or a bridge exposed beyond the
trusted VPN), mTLS layers onto the same enrollment handshake without
changing the registry model.

### Local mode is unchanged; the topology is an operator choice

Nothing about ADR 0024's local-mode lifecycle changes: a release IDE
still auto-spawns a detached `moon-bridge serve` child, the bridge still
self-exits when no workspace is live, the phone still pairs to the
LAN-bridge. The new work is purely **additive** — a connection mode the
IDE _can_ use, not one it must. An IDE that never enrolls with a remote
bridge behaves exactly as it does today. The bridge process does not
gain a "mode" flag; it serves both carriers in one process (a local
bridge can also accept enrolled IDEs; a remote bridge can also accept
local-socket workspaces if it happens to share a host with one).

## Wire additions

All additions are **new message tags** alongside the existing `pair` /
`workspaces` / `call` / `subscribe`; none change existing shapes, so a
phone that only knows today's protocol keeps working. `crates/moon-protocol/`
remains the single source of truth (invariant 4); the WS message enums
in `serve.rs` are the bridge's own transport adapter, not a divergent
schema.

- `ClientMessage::Enroll { code, label, ide_id }` — IDE presents an
  enrollment code + a stable self-assigned `ide_id` (so reconnections
  re-bind to the same registry entry). `ServerMessage::Enrolled { ide_id,
token }` is the success reply (mirror of `Paired`).
- `ClientMessage::Register { token, workspaces }` — an enrolled IDE
  reports its live workspaces (slug + name + last-active). Sent on
  connect and whenever the IDE's workspace set changes. The bridge
  updates its `WorkspaceRegistry` for that IDE.
- `ClientMessage::Call { token, workspace, ide, method, params }` —
  gains an optional `ide` field (the IDE that owns the workspace).
  Omitted / empty means "local carrier" (today's behaviour). The bridge
  resolves the carrier from `(ide, workspace)`.
- `ClientMessage::Subscribe { token, workspace, ide }` — same `ide`
  addition.
- `ServerMessage::Workspaces { workspaces }` — each entry gains an `ide`
  field (the owning IDE's id, or empty for local-carrier workspaces).
  The phone's switcher groups by it.

The bridge ↔ IDE hop reuses the same WS framing; the IDE is a WS
**client** (a new persistent outbound-connection module in the IDE),
not a listener. It sends `Register` on connect + on workspace-set
changes, and it answers `call`/`subscribe` frames the bridge forwards to
it by running them against the local `BridgeRpcHandler` (the same
`BridgeRpc` the focus listener dispatches today) and sending the reply
back up the socket. The IDE-side `BridgeRpcHandler` is reused unchanged;
the only new IDE code is the persistent WS client + the enrollment UI.

## Consequences

- **No change to local mode.** An IDE that doesn't enroll behaves
  exactly as in ADR 0024. The phone pairing flow, the PWA, the control
  socket, the idle watcher — all unchanged.
- **The bridge gains a second carrier** (`relay::call_remote` /
  `subscribe_remote`) and a `WorkspaceRegistry` that merges local +
  remote sources. `relay::call` (local Unix socket) is untouched.
- **New secret class** in the bridge's keyring: per-IDE enrollment
  tokens (`companion-ides`), revocable, mirroring `companion-devices`.
- **New secret in the IDE's keyring**: this IDE's enrollment token for
  a given bridge URL. The IDE is a keyring _reader_ here (it stores its
  own credential), unlike the revoke flow where the bridge stays the
  sole keyring writer.
- **The coder loop does not move.** This is the load-bearing invariant
  that distinguishes relay-hub from headless-core. Sessions, the JSONL,
  the git layer, the `Arc<SessionRuntime>` all stay on the IDE host. The
  bridge never sees coder state, only JSON-RPC bytes. The
  detached-loop constraint from `companion.md` is preserved, not built
  — this ADR refuses to deepen the coupling, exactly as 0023 asked.
- **One process serves both carriers.** A bridge has no "mode" flag. It
  accepts enrolled IDEs and paired phones on the same listener; it
  also still enumerates `instance.sock` if it shares a host with an
  IDE. This keeps the "local or remote" choice an operator
  configuration, not a build.
- **mTLS deferred.** The enrollment handshake is designed so mTLS can
  layer on later without restructuring.
- **Roadmap placement:** a new phase (14), not a 13.x sub-phase. 13.5
  (phone-side review & commit) remains the outstanding 13.x work and is
  not blocked by this.

## Alternatives considered and rejected

- **Headless `moon-core` on the bridge machine (the ADR 0023
  speculation).** Moves the coder loop, the session JSONL, and the git
  layer off the laptop onto the bridge box. A far larger change that
  answers "work with the laptop closed, the loop elsewhere" — a
  question nobody has asked. The relay hub answers the actual ask (a
  remote bridge serving multiple IDEs) with the minimum moving part.
  If headless-core is later requested, it supersedes this ADR, but the
  framing decision it and this both rely on carries forward unchanged.
- **Per-IDE TCP listeners, each IDE its own bridge port.** Rejected
  for the same reasons ADR 0023 rejected per-workspace TCP listeners: N
  ports / certs / enrollments that churn as IDEs come and go, and a
  phone that can't track them. One bridge, one port, multiplexed by IDE
  id.
- **A cloud relay (phone ↔ HF-hosted service ↔ IDEs).** Sends the
  team's source and agent traces through a third hop for a problem the
  VPN + a small relay box solves. Same rejection as ADR 0023's cloud
  relay alternative.
- **Public-internet exposure with HF OAuth instead of enrollment.**
  Wildly larger threat surface than a VPN relay needs. Enrollment +
  TOFU on the VPN is the right granularity, and it's symmetric with the
  already-shipped phone pairing.
- **mTLS for v1.** Stronger binding than bearer tokens, but unjustified
  by the threat model (enrollment is the boundary) and heavier to
  rotate/revoke. Kept as a layer-on follow-up.
- **Auto-forward the IDE's listening ports through the bridge.** Would
  violate the explicit-forward invariant (cross-cutting invariant 3).
  The bridge is one deliberate, named, enrollment-gated surface; IDEs
  do not expose their own ports to the relay.

## Follow-ups

- **mTLS for IDE ↔ bridge.** Layer onto the enrollment handshake if a
  compliance or beyond-VPN need appears.
- **Headless `moon-core` (the "laptop closed, loop elsewhere"
  future).** Supersedes this ADR if requested; the JSON-RPC framing
  both depend on is unchanged, so it remains a transport swap.
- **Reconnect / backoff policy for the IDE's outbound WS.** A dropped
  VPN should heal without re-enrollment; the stored IDE token makes
  this a reconnect, not a re-pair. Tuned in the implementation.
- **Per-IDE workspace set change notifications.** The `Register`
  message is sent on connect; sending it again when the IDE's open
  workspaces change (a new window) keeps the bridge's registry live
  without polling. Lands with the IDE-side client.
