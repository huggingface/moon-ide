# ADR 0023 — Mobile companion app via a `moon-bridge` daemon

Date: 2026-06-20
Status: planned; introduces a new component (`moon-bridge`) and a
new area spec ([`specs/companion.md`](../companion.md)). Builds on
[ADR 0014 — process per workspace](0014-process-per-workspace.md)
(per-workspace `instance.sock`), the
[remote-mode JSON-RPC transport](../protocol.md#transport) the
protocol already commits to, and the
[focus-socket protocol](0021-git-editor-forward.md) the IDE already
speaks over those sockets.

## Context

A team member wants to drive moon-ide from a phone on the same LAN
(typically over the company VPN): run and steer coder sessions
against a workspace folder, and review + commit. Not a full IDE on
the phone — no editing, no terminal, no LSP. A remote control for
the agent and the SCM panel.

Three facts about the existing architecture make this far cheaper
than a from-scratch mobile IDE, and they shape every decision
below:

1. **Everything already routes through JSON-RPC method names on the
   core** (the non-negotiable invariant in
   [`architecture.md`](../architecture.md#the-non-negotiable-invariant)).
   The coder loop and the git layer are already `coder_*` / `git_*`
   commands; the coder already streams `coder:event` envelopes
   tagged `{ folder, session_id, event }`. Nothing about the coder
   loop or git status needs the editor webview to exist. A phone is
   just another renderer of a surface that already exists.
2. **One OS process per workspace** ([ADR 0014](0014-process-per-workspace.md)),
   each owning its own registry / coder / LSP broker / git layer,
   and each already binding a per-workspace Unix domain socket at
   `<XDG_DATA_HOME>/moon-ide/workspaces/<id>/instance.sock`. The set
   of live `instance.sock` files **is** the registry of "what's
   open right now" — which directly answers "how do I handle
   multiple moon-ide windows on different workspaces".
3. **The remote-mode transport is already a planned contract.**
   `protocol.md` commits to "JSON-RPC 2.0 framed over a Unix
   socket, same method names, notifications for streams" for the
   `moon-remote` story. A phone client reuses that schema rather
   than inventing a mobile-only API; `crates/moon-protocol/` stays
   the single source of truth (invariant 4).

Two questions had to be answered: **how does the phone reach N
workspace processes that come and go**, and **what form does the
app take**.

## Decision

### A single host-resident bridge daemon, not per-process listeners

Add a new component, **`moon-bridge`** (sibling in spirit to the
planned `moon-remote`). One per host machine. It:

- Binds **one** HTTPS + WebSocket listener on the LAN
  (default `0.0.0.0:53180`, IANA dynamic range, adjacent to the
  next-edit server's `53281`). Self-signed TLS, cert generated on
  first run; the cert fingerprint is the trust anchor.
- **Discovers workspaces by enumerating
  `<XDG_DATA_HOME>/moon-ide/workspaces/*/instance.sock`.** A live
  socket means a running owner (probe succeeds, exactly the
  liveness check ADR 0014 already does for single-instance
  enforcement); a stale socket means not running. This is the phone's
  workspace switcher: the same list the desktop `Ctrl+Shift+O`
  picker shows, derived from the same on-disk truth.
- Relays JSON-RPC requests and the `coder:event` / git event
  streams between the phone and the chosen workspace process over
  that process's `instance.sock`.
- Can **launch** a not-running workspace by spawning
  `moon-ide --workspace <slug>` — the same thing `window_open` does
  today — so the phone isn't limited to whatever the desktop
  happens to be focused on.

```
 Phone (installable Svelte PWA)
   │  WSS over LAN / VPN
   ▼
 moon-bridge (one per host)
   │ enumerates workspaces/*/instance.sock
   ├─► moon-ide --workspace huggingface   (process: its coder, git, registry)
   ├─► moon-ide --workspace gitaly
   └─► moon-ide --workspace moon-landing
```

Per-process TCP listeners were rejected: N ports / N certs / N
pairings, all appearing and vanishing as workspaces open and close
(ADR 0014's whole point is that processes are ephemeral), which a
phone can't track. It also collides with the
[explicit-port-forwarding invariant](../architecture.md#components)
(invariant 3) — a bridge is one deliberate, named surface, not
auto-exposed webviews. The bridge can also serve a workspace whose
process isn't currently running, which per-process listeners
structurally can't.

### The app is an installable Svelte 5 PWA, served by the bridge

The companion is a **Svelte 5 + Vite single-page app** the bridge
serves over HTTPS, installable to the home screen, talking to the
bridge over WSS. Reasons specific to this team and codebase:

- **Maximum reuse.** The coder transcript (`CoderMessage`,
  `CoderToolCall`, the markdown pipeline) and the SCM diff
  components are already written in Svelte 5. v1 is largely
  "render the existing components against a WSS transport adapter
  instead of Tauri `invoke`".
- **It's an internal tool on a trusted network**, not an App Store
  product — so the main reasons to suffer native mobile toolchains
  (store review, distribution signing, install funnel) don't apply.
  Distribution is "open the bridge URL on the VPN, Add to Home
  Screen". Updates are "push to the bridge, reload".
- **The team's entire frontend skillset is Svelte 5.** A non-Svelte
  app is a toolchain nobody maintains.

The one thing genuinely worse in a browser — the self-signed-cert
interstitial — has a one-time fix the team performs once per device
anyway (see Pairing). After trust is installed, the PWA loads with
no warning.

### Native (Tauri mobile) is a deliberate future option, not v1

Tauri 2 has iOS / Android targets and would reuse the exact stack
the IDE is built on (same Tauri, same Svelte), giving native
keychain, native camera, and clean cert-pinning of a self-signed
bridge cert. It's the right move **if** the team later needs
background agent-watching (PWA WebSockets drop when backgrounded)
or push notifications.

It is explicitly **out of v1**: real native build / sign / deploy
overhead, an Apple Developer account for any iOS distribution, a
second CI app target — a lot of machinery for a remote control.

The choice is reversible because **the bridge protocol is the
contract, not the app.** Wrapping the same Svelte SPA in Tauri
mobile later swaps only the transport adapter
(browser `WebSocket` → native HTTP/WS client with cert-pinning);
nothing on the bridge changes. This is the same lesson
[ADR 0003 (no adapter layer)](0003-no-adapter-layer.md) and the
JSON-RPC-single-source-of-truth invariant already teach: pin the
protocol, stay loose about the renderer.

### Why JSON-RPC framing, not a socket relay

The first draft left the bridge ↔ workspace-process hop open: a
bespoke verb set on the per-workspace `instance.sock` (cheap,
precedent in [ADR 0021](0021-git-editor-forward.md)) vs. the
`moon-remote` JSON-RPC channel. The "cloud dev machine you can work
on with the laptop closed" future settles it in favour of JSON-RPC
framing.

`moon-bridge` and `moon-remote` are converging on the same thing:
**a headless `moon-core` serving the JSON-RPC surface to clients
over a channel.** On an always-on cloud box, those aren't two
daemons — they're one process. The box runs `moon-core` headless;
the laptop UI attaches to it as a remote host; the phone attaches
to it as a companion; both speak the same `crates/moon-protocol`
schema (invariant 4). They may literally merge.

A bespoke `instance.sock` relay only works _because the bridge and
the workspace process share a host_ — same Unix socket, same
filesystem for the session JSONL. The moment the workspace can live
on a different machine, a Unix-domain-socket relay is structurally
dead and you need an authenticated network transport — which is
exactly what `moon-remote` is specified to be. Building the socket
relay now and the network transport later means **building the
network transport twice, with the second obsoleting the first.**
Building the bridge on the JSON-RPC-over-a-channel shape from day
one makes "phone → my laptop" (today) and "phone → cloud box" /
"laptop → cloud box" (later) the _same_ code path behind different
channels (local socket, WSS, SSH tunnel).

This does **not** pull the cloud machine into scope. v1 is still a
laptop-local bridge talking to local workspace processes; closing
the laptop closes the bridge, as asked. Only the _framing decision_
is made now, so it isn't paid for twice.

### The detached-loop prerequisite (a constraint to preserve, not build)

"Works even if the laptop is closed" is really two things: the
_workspace_ living on an always-on machine (the remote-host story)
**and** the _agent loop_ surviving without a UI attached. The
second already half-exists: per
[ADR 0016](0016-coder-concurrent-sessions.md) a turn is a spawned
task closing over an `Arc<SessionRuntime>`, so background turns in
any session already run whether or not their session is the visible
one. The boundary that remains is the **process** — a restart kills
every in-flight turn because the runtime map is in-memory only
(test plan 0085).

So `coder.md`'s old "the loop only runs while the panel is active"
out-of-scope line was stale on two counts: the loop runs without
the panel visible, and the real lifetime boundary is the process,
not the UI. This ADR records the resulting constraint: **keep the
loop owned by `moon-core`, observed by attaching/detaching clients,
never coupled to a UI lifetime.** We are not building detached or
cross-restart runs now; we are refusing to deepen the coupling that
would make them expensive to add — the bridge work is the natural
place that coupling would otherwise creep in.

### Pairing — TOFU cert + device tokens, mirroring the device-flow vocabulary

The flow reuses patterns the codebase already trusts (the coder's
HF [device authorization grant](../coder.md#flow), keyring secret
storage):

1. The bridge generates a stable TLS keypair + self-signed cert on
   first run.
2. The desktop surfaces a **pairing QR** (a "Companion" affordance,
   natural home is the status bar or a small settings modal)
   encoding `wss://<lan-ip>:53180`, the **cert fingerprint**, and a
   short-lived **pairing token** (~120 s TTL, device-flow-style).
3. The phone scans, connects, **pins the fingerprint (TOFU)**,
   installs the bridge cert once (iOS: a `.mobileconfig` profile the
   bridge serves; Android: a user cert), and presents the pairing
   token.
4. The bridge issues a long-lived **device token** bound to that
   device. It lives in the host keyring at
   `service=moon-ide, account=companion-device:<id>` — the same
   keyring backend HF / Slack / provider secrets already use.
5. A **Paired devices** list with per-device revoke is the
   management surface.

### What the phone does in v1 (scope discipline)

Per [AGENTS.md scope rules](../../AGENTS.md#scope-discipline), the
thinnest thing actually requested:

- **Run / steer coder sessions** — subscribe to `coder:event`,
  render the transcript, `coder_send`, `coder_abort`. Session
  list / open / new already exist as commands.
- **Review & commit** — read-mostly diff review + commit / amend /
  sync over the existing git surface. Diffs render on a phone; full
  editing does not, and isn't attempted.
- **Workspace switcher** — the list of running (and launchable)
  workspace processes, from the `instance.sock` enumeration.

Explicitly **out of v1** (prose, not checklist items): full file
editing, terminal, LSP, multi-account, background agent-watching,
push notifications.

## Consequences

- **One new component** (`moon-bridge`) and **one new area spec**
  ([`companion.md`](../companion.md)). No change to the existing
  IPC surface — the bridge consumes it.
- **The bridge ↔ workspace-process hop uses the `moon-remote`
  JSON-RPC framing, not a bespoke `instance.sock` relay verb set**
  (see "Why JSON-RPC framing, not a socket relay" below). The
  `instance.sock` enumeration stays purely as the _discovery_
  mechanism (which workspaces are live); the data plane is the
  remote-mode JSON-RPC channel.
- **One new LAN-exposed surface**, deliberately named and
  pairing-gated — consistent with invariant 3 (explicit forwards,
  never auto-expose).
- **New secret class** in the keyring: per-device companion tokens,
  revocable.
- **Reuse, not fork.** The phone renders existing Svelte coder /
  SCM components; the protocol stays in `crates/moon-protocol/`
  (invariant 4). No divergent mobile schema to hand-maintain.
- **Roadmap placement:** lands after Phase 7 (multi-workspace),
  since it depends on process-per-workspace + per-workspace
  `instance.sock` being real — which, post-ADR-0014, they are.

## Alternatives considered and rejected

- **Per-workspace TCP listeners (each Tauri process exposes its
  own port).** N ports / certs / pairings that churn as
  workspaces open and close; can't serve a not-running workspace;
  fights the explicit-forward invariant. The bridge multiplexes
  by slug over the sockets the IDE already maintains.
- **Native app (Swift / Kotlin) or React Native / Flutter.** A
  new language + toolchain nobody in the IDE codebase uses, for an
  internal remote control. Against the "small focused packages,
  match existing style" posture.
- **Tauri mobile for v1.** Right stack, wrong time — build / sign /
  distribute overhead unjustified before a concrete need (background
  runs, push). Kept as a drop-in future option precisely because the
  bridge protocol is the contract.
- **A cloud relay (phone ↔ HF-hosted service ↔ host).** Sends the
  team's source and agent traces through a third hop for a problem
  the LAN / VPN already solves. The team is already on a shared
  network; keep it host-local.
- **Expose the coder over the public internet with HF OAuth.**
  Wildly larger threat surface than a LAN remote control needs.
  Pairing + TOFU on the VPN is the right granularity.

## Follow-ups

- **Background runs / push notifications** → the trigger to build
  the Tauri-mobile wrapper. Until someone asks, the PWA reconnects
  its WS on resume and that's enough.
- **Cloud / always-on dev machine.** The likely shape of "work with
  the laptop closed": a cloud box runs headless `moon-core`; laptop
  and phone are both attaching clients over the same JSON-RPC
  surface. `moon-bridge` and `moon-remote` very likely merge into
  one "headless core serving JSON-RPC over a channel" daemon when
  this lands. Out of scope until someone asks; the framing decision
  above is what keeps it a transport swap rather than a rewrite.
- **Detached / overnight agent runs.** Need the loop owned by the
  always-on core and re-attachable across client connect/disconnect
  — see the detached-loop constraint above. Builds on the cloud
  machine, not on v1.
- **Windows.** The `instance.sock` enumeration inherits ADR 0014's
  Unix-domain-socket limitation; a Windows bridge needs the same
  named-pipe shim the focus socket defers. The team develops on
  Linux/macOS, so this defers with it.
