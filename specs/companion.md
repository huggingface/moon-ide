# Companion app (mobile)

STATUS: shipped (Phase 13, v1) — local bridge + pairing + PWA. Remote
/ relay mode is Phase 14; decision record:
[ADR 0031 — remote / relay bridge topology](decisions/0031-remote-bridge-relay.md).
Original v1 decision:
[ADR 0023 — mobile companion via `moon-bridge`](decisions/0023-mobile-companion-bridge.md).
Sub-phase work breakdown:
[roadmaps/phase-13-mobile-companion.md](roadmaps/phase-13-mobile-companion.md).

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
- **Workspace discovery** (implemented — `moon-bridge list`).
  Enumerate `<data_local_dir>/moon-ide/workspaces/*/`. A socket
  that accepts a connection has a live owner; one that fails with
  `ECONNREFUSED` (or is missing) is stale / not running — exactly
  the liveness probe [ADR 0014](decisions/0014-process-per-workspace.md)
  already uses for single-instance enforcement. Names and
  last-active come from the `state.json` catalog. This list is the
  phone's workspace switcher.
- **Relay.** Forward JSON-RPC requests and event-stream
  subscriptions between the phone and the selected workspace
  process over that process's `instance.sock`.
- **Launch.** Spawn `moon-ide --workspace <slug>` for a discovered-
  but-not-running workspace (the same action `window_open` performs),
  so the phone isn't limited to whatever the desktop is focused on.

### Lifecycle — the IDE owns it (ADR 0024)

The user never runs the bridge by hand: **running the IDE makes the
companion reachable.** Each release IDE launch fires a detached
`moon-bridge serve` child; binding the LAN port is a machine-wide
owner election, so at most one bridge survives no matter how many
windows are open. The bridge self-exits when discovery finds zero
live workspaces (the last IDE closed), so it's running iff an IDE is.
Dev builds skip auto-start — run `moon-bridge serve --web-root
companion/dist` by hand. Full design: [ADR 0024](decisions/0024-bridge-lifecycle.md).

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
   home is the status bar or a small settings modal). The QR encodes a
   **link to the PWA itself** with the code in the fragment
   (`https://<bridge-host>/#pair=<code>`), so a camera scan opens the
   PWA and it pairs itself — the phone derives the WS URL from the
   page origin (the PWA is served by the same listener), and the
   fragment never reaches server logs. Type-in fallback: the URL +
   code shown alongside the QR.

   Codes are minted **on demand** (a "Show pairing QR" button — the
   local panel asks over the control socket, a remote-enrolled IDE
   over its WS; roadmap 14.5). There is no startup pairing window:
   one live single-use session at a time, a fresh mint replaces it.

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

The desktop surfaces all this in a **Companion** modal (command
palette → "Companion: Pair a phone…"): a QR of the pairing payload,
the address + code, the fingerprint, and a paired-devices list with
revoke. The bridge advertises `moon-bridge.local` over **mDNS**
(`mdns-sd`) so the phone reaches it by name regardless of the host's
IP; the raw IP rides in the payload as the fallback for networks
that block multicast.

Because the bridge is a separate process, the IDE talks to it over a
local **control socket** (`<bridge_dir>/control.sock`, newline-framed
JSON): `status` returns the pairing payload + device list, `revoke`
drops a paired device, `shutdown` asks it to exit. The
`companion_status` / `companion_revoke_device` commands are the IDE's
client. Liveness is intrinsic — a refused connect means the bridge
isn't running, so the status-bar pip can't be lit by a stale file
(an earlier file-based channel had exactly that bug). The bridge
stays the sole keyring writer; the IDE only _asks_ it to revoke.

Pairing is the **whole** security boundary: a paired device can
drive the coder, which can run anything via its `bash` tool, so
there's no point fencing the relay's method surface (same threat
model as the desktop — `coder.md` § Permissions). What the relay
exposes is a scope decision, not a safety one.

## App form

An **installable Svelte 5 + Vite PWA**, served by the bridge over
HTTPS, added to the home screen. Chosen because it reuses the IDE's
exact frontend stack and existing coder / SCM components, needs no
App Store review or distribution signing for an internal-LAN tool,
and keeps everything in the one framework the team maintains.

Installability: the manifest ships launcher + maskable icons
(generated PNGs, `scripts/gen-companion-icons.mjs` — no native
rasterizer dependency), iOS gets `apple-touch-icon` + its meta tags,
and a small hand-rolled service worker (`companion/public/sw.js`)
caches the app shell — network-first for navigations so deploys show
on next load, cache-first for hashed `/assets/*`. The WS to the
bridge is untouched by the worker.

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

- **Coordinator sessions (ADR 0030).** The phone can create
  coordinator sessions (the `✦` button in the workspace view, via
  `coder_new_coordinator_session`), and the session view renders a
  `coord` badge + a coordinator-specific empty-state hint describing
  the delegation model. Workers are ordinary sessions in the per-
  project list — opening one and sending a message takes it over
  from the coordinator (ADR 0036), same as the desktop.
- **Run / steer coder sessions.** Subscribe to `coder:event`,
  render the transcript, `coder_send` (send / steer), `coder_abort`.
  Session list / open / new reuse the existing `coder_*` commands.
  Send / abort carry the phone's open `session_id` so they can't
  land in whatever session the desktop happens to have visible.
  Opening a session from the phone is an **observe-open**: the
  runtime mounts (so send/abort by id work) and the transcript
  replay returns in the RPC response, but the folder's
  visible-session pointer stays untouched and nothing is broadcast
  — the phone never switches the desktop's panel or lights its
  background-attention badges.
- **Review & commit.** Read-mostly diff review plus commit / amend /
  sync over the existing [git layer](roadmaps/phase-05-git.md).
  Diffs render on a phone; full editing does not, and isn't
  attempted.
- **Workspace switcher.** The list of running and launchable
  workspace processes, from the `instance.sock` enumeration.
  Stopped workspaces show a **Start** button: the phone calls
  `workspace_launch` on the bridge, which spawns `moon-ide --workspace
<slug>` directly for local-carrier workspaces (the bridge is on the
  host and owns the workspaces dir), or forwards to the owning
  enrolled IDE for remote-carrier workspaces (the IDE runs its own
  `window_open` "focus or spawn" path). Either way the phone
  re-polls the list after ~1.5 s and the workspace appears live.
- **Project chip indicators.** The workspace view's project switcher
  badges each folder: a live pip while any of its sessions has a
  running turn, and a "finished" dot when a live turn completed
  while the phone was looking at another folder (cleared on opening
  the folder). Tracked phone-side from the event stream's envelope
  `folder`; replayed historical events never flag "finished".
- **Switch to default branch.** When the folder is on a feature
  branch, a "⇄ Switch to main" chip switches the working tree back
  to the default branch (`workspace_scm_switch_branch`, wrapping the
  same `branch_switch` host method as the desktop's switcher).
  Disabled while the tree is dirty — commit or discard first.
- **SCM (git) status + commit.** The workspace view shows the
  active folder's current branch, ahead/behind upstream, changed
  file counts (added / modified / deleted) and a collapsible file
  list. A commit composer with a sparkle button (auto-suggest via
  the fast model, same prompt as the desktop's SCM panel) lets the
  phone commit changes. All folder-targeted, reusing the same
  `WorkspaceHost` git methods the desktop uses.
- **Edit & resend / replay.** Tapping a user bubble (idle sessions
  only) reveals two chips: _Edit & resend_ truncates the session to
  just before that message and drops the text back into the
  composer; _Replay_ truncates and re-sends the same prompt
  verbatim. Backed by a session-targeted `coder_revert_to_message`
  (the desktop's visible session and panel are untouched; the phone
  repaints via observe-open).
- **Provider switch.** The workspace view surfaces the active LLM
  provider (HF or a configured user provider) with the per-workspace
  lock toggle, via `coder_get_model_settings` /
  `coder_set_model_settings` — the same read/write payload and
  semantics as the desktop picker (a locked save pins the workspace;
  an unlocked save writes the global default). Provider CRUD and
  API keys stay desktop-only.
- **Project switcher.** Inside a workspace, the phone lists the
  bound folders (from `workspace_snapshot`, worktree folders hidden
  — they share their parent's session list per ADR 0028) and scopes
  the session commands with an explicit `folder` param. This is
  phone-side targeting only: it never moves the desktop's
  active-folder selection, which stays owned by the desktop UI (no
  workspace-changed event exists for a remote mutation to ride).

## Remote / relay mode (Phase 14)

The v1 bridge is host-local: the IDE spawns it, it enumerates
`instance.sock` files on the shared filesystem, it dies with the last
IDE. That rests on one assumption — **bridge and IDE share a host**, so
the bridge can _find_ IDEs by reading a directory and _reach_ them over
a Unix socket.

Remote mode drops that assumption. The bridge runs somewhere else (a
relay box on the VPN, a small always-on machine), and **both the IDE(s)
and the phone(s) connect to it as clients.** The motivating
properties: the bridge is not bound to one IDE (multiple IDEs enroll
with the same bridge; the phone sees all their workspaces in one
switcher), and local-vs-remote is an operator choice, not a build
property. Local mode is exactly ADR 0024's behaviour and is unchanged.
Decision record: [ADR 0031](decisions/0031-remote-bridge-relay.md).

### Relay hub, not headless core

The remote bridge is a **relay**: it forwards JSON-RPC between phones
and IDEs over WebSocket connections. It holds **no coder state, no
sessions, no git layer** — those stay on the IDE host, exactly where
they are today. This is the load-bearing distinction from the "headless
`moon-core`" shape the old "Cloud / always-on future" prose speculated
about (see below). The requester asked for a bridge that can run
remotely and serve multiple IDEs, **not** for the coder to move off the
laptop; relay hub answers the actual ask with the minimum moving part.

```
remote / relay mode:

 IDE-A (laptop) ──(outbound WSS, enrolled)──► bridge
 IDE-B (laptop) ──(outbound WSS, enrolled)──► bridge
 phone ──(WSS, paired)────────────────────────► bridge

 the bridge routes call/subscribe from a phone to the IDE that owns
 the target workspace; events stream back. The coder loop never moves
 off the IDE host.
```

### Discovery inverts

Local mode discovers IDEs by enumerating `instance.sock` files
(possible only because bridge and IDE share a host). Remote mode
**cannot** enumerate a remote filesystem, so discovery inverts: **the
IDE dials out to the bridge and registers its workspaces.** The bridge
holds a `WorkspaceRegistry` fed by two carriers:

- **Local carrier** — the `instance.sock` enumeration (today's path,
  unchanged).
- **Remote carrier** — the set of currently-enrolled IDE connections,
  each reporting its live workspaces.

`call`/`subscribe` route to whichever carrier owns the target
workspace: local-carrier over the Unix socket (`relay::call`,
unchanged); remote-carrier over the held-open IDE WebSocket (a new
forwarding path). The JSON-RPC framing on both hops is identical —
the payoff of ADR 0023's framing decision. The phone's `workspaces`
reply is the union, each entry namespaced by IDE so the switcher can
group them.

### Enrollment mirrors pairing

Today only phones authenticate (TOFU cert pin + short single-use code →
long-lived revocable bearer token). Remote mode adds the **symmetric**
relationship (IDE ↔ bridge) using the same vocabulary, so there is one
security model, not two:

1. Bridge generates its TLS keypair + self-signed cert (unchanged).
2. A short-lived (120 s), single-use enrollment code prints at `serve`
   startup (the operator reads it from the terminal / service journal).
   Startup-only by design: enrollment bootstraps the trust that any
   on-demand path would itself need.
3. IDE's "Connect to remote bridge" affordance (command palette entry,
   not a keybinding — Ctrl+T is `next_edit_complete`) takes the bridge
   URL + code. The IDE **TOFU-pins the bridge cert** (same as a phone),
   presents the code, the bridge mints a long-lived **IDE token** in the
   bridge keyring at `service=moon-ide, account=companion-ides` (a 1:1
   mirror of `companion-devices`).
4. The IDE stores its token in **its own** keyring and reconnects with
   it on restart — no re-enrollment per launch.
5. A **Paired IDEs** list with per-IDE revoke is the management surface,
   alongside the existing paired-devices list.

`EnrolledIde` / `IdeStore` mirror `PairedDevice` / `DeviceStore`. The
enrollment handshake (`enroll` → `enrolled`) mirrors `pair` → `paired`.
**No per-method ACL** behind enrollment — same threat model as pairing:
an enrolled IDE can drive the coder, which runs anything via `bash`.
Enrollment is the boundary; what the relay exposes is a scope decision,
not a safety one. mTLS (client certs for IDEs) is a documented future,
not v1 — bearer tokens match the existing posture and are simpler to
rotate/revoke.

### Wire additions

All additions are **new message tags** alongside the existing `pair` /
`workspaces` / `call` / `subscribe`; none change existing shapes, so a
phone that only knows today's protocol keeps working. `crates/moon-protocol/`
stays the single source of truth (invariant 4); the WS message enums in
`serve.rs` are the bridge's own transport adapter, not a divergent
schema.

- `Enroll { code, label, ide_id }` → `Enrolled { ide_id, token }`
  (IDE presents an enrollment code + a stable self-assigned `ide_id` so
  reconnections rebind to the same registry entry).
- `Register { token, workspaces }` — an enrolled IDE reports its live
  workspaces (slug + catalog name + last-active — the same identity
  the desktop shows, so the phone's switcher reads "Hugging Face",
  not a process label). Sent on connect and whenever the IDE's
  workspace set changes. Because moon-ide is process-per-workspace
  (ADR 0014), every open workspace holds its **own** enrolled
  connection under the shared `ide_id`; the bridge keys its live
  table by connection (not by `ide_id`, which would clobber) and
  routes `call`/`subscribe` by `(ide, workspace)`. The phone's
  switcher sees the union.
- `Call` / `Subscribe` gain an optional `ide` field (the owning IDE's
  id, or empty for local-carrier). The bridge resolves the carrier from
  `(ide, workspace)`.
- `Workspaces` reply — each entry gains an `ide` field; the phone's
  switcher groups by it.
- `PairCode { token }` → `PairPayload { payload, url, code,
fingerprint }` — an enrolled IDE asks the bridge to mint a fresh
  phone-pairing code and renders the payload as a QR in its Companion
  panel. An enrolled IDE is already fully trusted (it is what a paired
  phone would drive), so this adds no capability — it moves _when_ a
  pairing window opens from "bridge startup only" to "on demand from
  the IDE". Codes keep the usual TTL + single-use semantics; one live
  pairing session at a time (a new request replaces the old code).

Liveness: every WS connection (phone and IDE, both directions)
carries a 30 s ping / 95 s read-idle deadline. Without it a
half-open TCP (suspended laptop, dropped NAT entry) left a ghost
workspace registration in the bridge's live table indefinitely; the
pings double as traffic through proxy idle timeouts (nginx
`proxy_read_timeout`, ADR 0035). The `workspaces` reply additionally
dedupes by `(ide, workspace)` keeping the newest connection, so a
restarted IDE doesn't list twice while its ghost awaits the reaper.

The bridge ↔ IDE hop reuses the same WS framing; the IDE is a WS
**client** (a new persistent outbound-connection module in the IDE), not
a listener. It sends `Register` on connect + on workspace-set changes,
and answers `call`/`subscribe` frames the bridge forwards to it by
running them against the local `BridgeRpcHandler` (the same `BridgeRpc`
the focus listener dispatches today) and sending the reply back up the
socket. The IDE-side `BridgeRpcHandler` is reused unchanged; the only
new IDE code is the persistent WS client + the enrollment UI.

### What remote mode deliberately doesn't do

- **Move the coder loop off the IDE.** Sessions, the JSONL, the git
  layer all stay on the IDE host. The bridge forwards bytes; it does
  not adopt the loop. This preserves the detached-loop constraint below
  rather than building it.
- **Auto-forward IDE listening ports through the bridge.** Violates
  the explicit-forward invariant (invariant 3). The bridge is one
  deliberate, enrollment-gated surface; IDEs do not expose their own
  ports to the relay.
- **Public-internet exposure.** Same v1 exclusion as local mode:
  VPN / trusted network only. Superseded for one deliberate deployment
  by [ADR 0035](decisions/0035-public-relay-deployment.md): a standing
  relay on a public VPS behind an nginx TLS front, accepted because the
  token boundary (not the network) was always the load-bearing control.

## Cloud / always-on future

The _next_ shape after relay mode — "I want to kick off work, close the
laptop, and keep going from the phone" — would move the coder loop
itself off the laptop: a cloud dev machine runs **headless `moon-core`**,
and both the laptop UI and the phone are _attaching clients_ over the
same JSON-RPC surface. Same schema, same channel framing; very likely
the same daemon. `moon-bridge` and `moon-remote` would converge on one
"headless core serving JSON-RPC over a channel" shape and may merge.

This is **not** the relay-hub mode above. Relay hub (Phase 14) keeps the
loop on the IDE and only forwards bytes; headless core moves the loop
to the bridge machine. They share the JSON-RPC framing decision (that's
why it was locked in early), but headless core is a much larger change
that answers "work with the laptop closed, the loop elsewhere" — a
question nobody has asked yet. If it's later requested, it supersedes
ADR 0031 with a new one; the framing both rely on carries forward
unchanged.

This is **not** v1 or v1.5 (Phase 14). Only the framing decision is
locked in early, so neither the relay hub nor the headless core pays
for a second network transport.

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
  where that would otherwise creep in. Remote / relay mode (Phase 14)
  honours this: the bridge forwards bytes and never adopts the loop.
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
- [ADR 0024](decisions/0024-bridge-lifecycle.md) — the IDE-owns-it
  lifecycle remote mode preserves unchanged for local operation.
- [ADR 0031](decisions/0031-remote-bridge-relay.md) — the relay-hub
  topology + IDE-enrollment auth for remote mode.
- [ADR 0035](decisions/0035-public-relay-deployment.md) — the public
  nginx-fronted standing-relay deployment (`serve --no-idle-exit
--advertise-url`).
- [`coder.md`](coder.md) — the coder surface the phone renders;
  device-flow + keyring patterns the pairing flow mirrors.
