# Companion app (mobile)

STATUS: planned — design only, no code yet. Decision record:
[ADR 0023 — mobile companion via `moon-bridge`](decisions/0023-mobile-companion-bridge.md).

A phone companion that drives a running moon-ide over the LAN
(typically the company VPN): run and steer coder sessions against a
workspace folder, and review + commit. It is **not** a mobile IDE —
no file editing, no terminal, no LSP. A remote control for the agent
and the SCM panel.

## Shape

```
 Phone (installable Svelte 5 PWA)
   │  WSS over LAN / VPN  (paired, TLS-pinned)
   ▼
 moon-bridge  (one daemon per host machine)
   │  enumerates <XDG_DATA_HOME>/moon-ide/workspaces/*/instance.sock
   │  relays JSON-RPC + event streams over each workspace's socket
   ├─► moon-ide --workspace huggingface    (process: coder, git, registry)
   ├─► moon-ide --workspace gitaly
   └─► moon-ide --workspace moon-landing
```

The companion is a renderer of a surface that already exists. The
coder loop and git layer are already JSON-RPC methods on the core
(architecture invariant); the coder already streams `coder:event`
envelopes tagged `{ folder, session_id, event }`; sessions are
append-only JSONL in pi-mono shape. None of that needs the editor
webview. The phone reuses the IDE's own Svelte coder / SCM
components against a network transport instead of Tauri `invoke`.

## `moon-bridge` — the host daemon

One per host machine. Responsibilities:

- **Listener.** One HTTPS + WebSocket listener on the LAN, default
  `0.0.0.0:53180`. Self-signed TLS; keypair + cert generated on
  first run, persisted under `<XDG_DATA_HOME>/moon-ide/bridge/`.
  The cert fingerprint is the trust anchor (see [Pairing](#pairing)).
- **Workspace discovery.** Enumerate
  `<XDG_DATA_HOME>/moon-ide/workspaces/*/instance.sock`. A socket
  that accepts a connection has a live owner; one that fails with
  `ECONNREFUSED` is stale (not running) — exactly the liveness
  probe [ADR 0014](decisions/0014-process-per-workspace.md) already
  uses for single-instance enforcement. This list is the phone's
  workspace switcher.
- **Relay.** Forward JSON-RPC requests and event-stream
  subscriptions between the phone and the selected workspace
  process over that process's `instance.sock`.
- **Launch.** Spawn `moon-ide --workspace <slug>` for a discovered-
  but-not-running workspace (the same action `window_open` performs),
  so the phone isn't limited to whatever the desktop is focused on.

Why a single daemon and not one listener per Tauri process: per
[ADR 0014](decisions/0014-process-per-workspace.md) workspace
processes are ephemeral (one per workspace, spawned on demand,
exit on close). Per-process ports would churn constantly, demand N
certs / N pairings, and couldn't serve a not-running workspace. The
bridge multiplexes by slug over the sockets the IDE already
maintains, and is one deliberate LAN surface — consistent with the
[explicit-forward invariant](architecture.md#components) (never
auto-expose listening ports).

## Transport

The phone speaks **JSON-RPC 2.0 over WSS**, the same method names
and event grammar the in-process Tauri surface and the planned
[remote-mode transport](protocol.md#transport) use.
`crates/moon-protocol/` stays the single source of truth
(invariant 4); the companion does not get a hand-maintained mobile
schema.

Bridge ↔ workspace-process hop: the **`moon-remote` JSON-RPC
framing**, not a bespoke `instance.sock` relay verb set. The
`instance.sock` enumeration is the _discovery_ mechanism only
(which workspaces are live); the data plane is the remote-mode
JSON-RPC channel. This is deliberate so the cloud / always-on
future (below) is a transport swap, not a rewrite — see
[ADR 0023 § Why JSON-RPC framing, not a socket relay](decisions/0023-mobile-companion-bridge.md).

Event streaming: `coder:event` and git events become JSON-RPC
notifications over the WS, routed to the phone by the same
`{ folder, session_id }` envelope the desktop already uses.

## Pairing

TOFU cert pin + revocable device tokens, mirroring the vocabulary
of the coder's [HF device flow](coder.md#flow) and the keyring
secret storage already in use.

1. Bridge generates its TLS keypair + self-signed cert on first run.
2. Desktop surfaces a **pairing QR** (a "Companion" affordance,
   home is the status bar or a small settings modal) encoding:
   - `wss://<lan-ip>:53180`
   - the bridge cert **fingerprint**
   - a short-lived **pairing token** (~120 s TTL).
3. Phone scans → connects → **pins the fingerprint (TOFU)** →
   installs the bridge cert once (iOS: a `.mobileconfig` the bridge
   serves; Android: a user cert) → presents the pairing token.
4. Bridge issues a long-lived **device token** bound to that
   device, stored in the host keyring at
   `service=moon-ide, account=companion-device:<id>`.
5. **Paired devices** list with per-device revoke is the management
   surface.

The one-time cert-trust install is what removes the browser's
self-signed interstitial; after it the PWA loads cleanly. It's a
per-device ritual the team performs once, alongside pairing.

## App form

An **installable Svelte 5 + Vite PWA**, served by the bridge over
HTTPS, added to the home screen. Chosen because it reuses the IDE's
exact frontend stack and existing coder / SCM components, needs no
App Store review or distribution signing for an internal-LAN tool,
and keeps everything in the one framework the team maintains.

**Native (Tauri 2 mobile) is a deliberate future option**, not v1.
It would reuse the same Svelte SPA wrapped in the same Tauri the
IDE already uses, with native keychain / camera / cert-pinning and
proper background behaviour — the right move once a concrete need
appears (background agent-watching, push notifications). Because the
bridge protocol is the contract, switching to native swaps only the
transport adapter (browser `WebSocket` → native HTTP/WS client);
nothing on the bridge changes.

## What the phone does (v1 scope)

Per [scope discipline](../AGENTS.md#scope-discipline), the thinnest
requested surface:

- **Run / steer coder sessions.** Subscribe to `coder:event`,
  render the transcript, `coder_send` (send / steer), `coder_abort`.
  Session list / open / new reuse the existing `coder_*` commands.
- **Review & commit.** Read-mostly diff review plus commit / amend /
  sync over the existing [git layer](roadmaps/phase-05-git.md).
  Diffs render on a phone; full editing does not, and isn't
  attempted.
- **Workspace switcher.** The list of running and launchable
  workspace processes, from the `instance.sock` enumeration.

## Cloud / always-on future

The likely shape of "I want to kick off work, close the laptop, and
keep going from the phone": a cloud dev machine runs **headless
`moon-core`**, and both the laptop UI and the phone are _attaching
clients_ over the same JSON-RPC surface — the laptop as a remote
host (the `moon-remote` story already in
[`architecture.md`](architecture.md#components)), the phone as a
companion. Same schema, same channel framing; very likely the same
daemon. `moon-bridge` and `moon-remote` are converging on one
"headless core serving JSON-RPC over a channel" shape and may
merge. This is **why the bridge ↔ process hop uses JSON-RPC framing
now** — so the cloud box is a transport swap (local socket → WSS →
SSH tunnel), not a second network transport that obsoletes the
first.

This is **not** v1. v1 is a laptop-local bridge; closing the laptop
closes it, as asked. Only the framing decision is locked in early.

Two prerequisites the cloud future needs, written down so v1
doesn't accidentally design them out:

- **The loop must stay owned by `moon-core`, not by a UI lifetime.**
  Already half-true: a coder turn is a spawned task closing over an
  `Arc<SessionRuntime>`, so background turns run whether or not
  their session is the visible one, and concurrent turns per folder
  already work (see [ADR 0016](decisions/0016-coder-concurrent-sessions.md)).
  The remaining boundary is the **process** — a restart kills
  in-flight turns because the runtime map is in-memory only (test
  plan [0085](test-plans/0085-coder-concurrent-sessions.md)).
  Detached / overnight runs need the loop re-attachable across
  client connect/disconnect; the constraint for now is simply
  **don't deepen the loop ↔ process coupling** — the bridge work is
  where that would otherwise creep in.
- **Sessions stay on the machine that runs the core.** The JSONL
  lives next to whichever `moon-core` owns the loop; clients render
  it, they don't own it. Already true today.

## What this deliberately doesn't do (v1)

Prose, not commitments — revisit when someone asks:

- **Full file editing / terminal / LSP** on the phone. The phone is
  a coder + review remote control.
- **Background agent-watching** with the screen off — a backgrounded
  PWA's WebSocket drops; v1 reconnects on resume. This is the
  trigger for the Tauri-mobile wrapper.
- **Detached / overnight runs that survive the laptop closing** —
  needs an always-on headless core (see [Cloud / always-on
  future](#cloud--always-on-future)), not a phone-side feature.
- **Push notifications** ("your agent finished / needs input") —
  same trigger.
- **Multi-account.** One HF account per moon-ide install
  (matches the coder's posture).
- **Public-internet exposure.** LAN / VPN only; pairing + TOFU is
  scoped to a trusted network.
- **Windows host bridge.** Inherits ADR 0014's Unix-domain-socket
  limitation; needs the same named-pipe shim the focus socket
  defers.

## Cross-spec touch-points

- [`architecture.md`](architecture.md) — the bridge consumes the
  existing JSON-RPC surface; the UI-never-touches-IO invariant is
  upheld because the phone goes through the core like every other
  client.
- [`protocol.md`](protocol.md) — reuses the remote-mode JSON-RPC
  transport and method names; no divergent mobile schema.
- [ADR 0014](decisions/0014-process-per-workspace.md) — the
  per-workspace `instance.sock` is both the multi-workspace answer
  and the bridge's discovery mechanism.
- [ADR 0021](decisions/0021-git-editor-forward.md) — precedent for
  extending the per-workspace socket's verb set.
- [`coder.md`](coder.md) — the coder surface the phone renders;
  device-flow + keyring patterns the pairing flow mirrors.
