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

Phone `call` frames carry a `call_id` the bridge echoes on the
matching `result`/`error`: forwarded calls run concurrently on the
IDE and reply in completion order, so the phone matches replies by
id (id-less frames — pair, workspaces, pre-id bridges — fall back to
the FIFO queue). Without this, any two overlapping calls could swap
payloads.

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

Two deliberate limits of the current shape, both tracked in README
§ "Before wider release" and not built until someone shares a relay:

- The scope is the **relay**, not the IDE — phone and IDE tokens are
  relay-wide, so any paired phone can drive every enrolled IDE. A
  shared relay needs phone tokens scoped to an allowed-IDE set
  (default: the IDE that minted the pairing QR), enforced on every
  routed frame.
- The relay is **trusted**: it terminates TLS, sees plaintext
  JSON-RPC, and nothing stops it originating commands to an enrolled
  IDE. The release shape makes it a blind pipe: pairing mints a
  device keypair, the phone signs every frame (nonce/counter against
  replay), the IDE verifies against the pinned device key before
  dispatching, and ideally the payloads are end-to-end encrypted
  phone↔IDE — the relay's own tokens then only gate routing and
  denial-of-service, not command authority.

## App form

An **installable Svelte 5 + Vite PWA**, served by the bridge over
HTTPS, added to the home screen. Chosen because it reuses the IDE's
exact frontend stack and existing coder / SCM components, needs no
App Store review or distribution signing for an internal-LAN tool,
and keeps everything in the one framework the team maintains.

Installability: the manifest ships launcher + maskable icons
(`companion/artwork/icon.svg` — a crescent moon with an orbit ring
and spark — rasterized to PNGs by `scripts/gen-companion-icons.mjs`,
which re-implements that exact geometry with no native rasterizer
dependency), iOS gets `apple-touch-icon` + its meta tags,
and a small hand-rolled service worker (`companion/public/sw.js`)
caches the app shell — network-first for navigations so deploys show
on next load, cache-first for hashed `/assets/*`. The WS to the
bridge is untouched by the worker.

The composer follows mobile-messenger convention (Telegram): one
pill bubble holding an auto-growing textarea and an inset circular
send/stop button. On touch-primary devices (`pointer: coarse`) Enter
inserts a newline and the button sends; on hardware keyboards Enter
sends and Shift+Enter newlines, same as the desktop composer.

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
  `coder_new_coordinator_session`), and both the session list row and
  the session view render a `coord` badge — left of the title, so the
  truncating title can't ellipsise it away — plus a
  coordinator-specific empty-state hint describing the delegation
  model. Workers are ordinary sessions in the per-
  project list — opening one and sending a message parks a notice
  quoting what you said in the coordinator's steer queue (delivered
  with its next turn, [ADR 0043](decisions/0043-user-message-notifies-coordinator.md)
  / [0062](decisions/0062-parked-coordinator-notices.md)) and leaves
  the worker hooked up, same as the desktop.
- **Review a worktree session vs the default branch.** A session
  driving a git worktree (ADR 0028) shows a `⇄` button in its
  header; it opens a full-screen review of that checkout against
  the default branch — `workspace_scm_review(folder)` composes
  `git_default_branch_diff` (merge-base + file list, committed +
  uncommitted, untracked excluded) with `git_diff_against`
  (unified patch, 64 kB cap), and the phone renders per-file
  collapsible diff sections. `base_ref: null` means "nothing to
  review against" (on the default branch / detached / no remote).
- **Turn retry.** A trailing turn error shows a bar above the
  composer with the message and a Retry button
  (`coder_retry_last_turn`, session-targeted via
  `Coder::retry_last_turn_in`) — same semantics as the desktop's
  trailing-error retry: nothing truncated, output appends below.
- **Remote freshness.** A phone `workspace_scm_status` triggers a
  throttled background `git fetch` (once per 5 min per folder) so
  ahead/behind counts track the remote — a project switch is the
  natural "am I current?" moment. The Sync button fetches inline
  first, so its pull/push decision never runs on stale counts.
- **Provider management.** The provider card's "+ Add provider"
  form adds a user provider (OpenRouter / Anthropic / custom
  OpenAI-compat presets) with its API key from the phone:
  `coder_probe_provider` validates the endpoint+key first (upstream
  error verbatim), `coder_add_provider` persists (key in the IDE
  host's keyring, config in `state.json` via the shared
  `settings::add_provider`) and returns the refreshed settings; the
  new provider is auto-activated.
- **Working-tree changes.** The SCM card gets a manual refresh
  button, tappable file rows, and a "View changes" overlay: per-file
  collapsible unified diffs from `workspace_scm_diff`
  (`git_diff_patch` — vs HEAD, untracked synthesised in, 64 kB cap),
  auto-expanding the tapped file.
- **Project management.** The projects row's edit toggle flips chips
  into remove targets; removal (`workspace_remove_folder`, refused
  for worktree folders) requires typing the project name — the
  workspace state is shared, so the desktop's folder bar loses the
  project too. `session.json` is updated so the unbind survives a
  restart, and `WorkspaceFoldersChanged` refreshes every attached UI.
- **Hash routing.** The current view (workspace + optional session)
  is mirrored into the URL hash (`#/w?ws=…&ide=…&f=…&s=…`, `f` the
  active project folder) via
  `replaceState` — a refresh restores the same page, and no history
  entries pile up for the back button. Restore is best-effort with a
  hard no-stuck rule: a workspace that fails to open falls back to
  the list, a session that fails to replay falls back to its
  workspace.
- **Issue-reference autolinks.** Assistant markdown linkifies
  standalone `#123` to `<repo web url>/issues/123` (GitHub
  redirects PRs), where the repo URL comes from the active folder's
  origin/upstream remote (`remote_url` on `workspace_scm_status`,
  via `WorkspaceHost::git_remote_web_url`). Token-stream rule, so
  references inside existing links or code spans are untouched.
  The repo URL is cached per folder in `localStorage` and seeded on
  folder open, so links render instantly (and offline); the live
  fetch retries with backoff on slow connections and refreshes the
  cache. Precondition visibility: the SCM card shows the resolved
  link base, and the session view shows a dashed "links off" row
  with a retry button when no repo URL is known.
- **Image attachments.** The composer takes images two ways: paste
  (desktop browsers) and a 📷 button (mobile: photo library /
  camera via a file input). Client-side downscale to ≤1600 px +
  webp (jpeg fallback on Safari) before the base64 rides
  `coder_send`'s `images` field; the runner re-encodes on ingest as
  usual. Thumbnails with remove buttons above the composer; sent
  user bubbles render the images inline (tap opens the lightbox),
  live and on replay. Attachments are not part of the
  pending-send persistence (localStorage quota) — a failed send
  keeps the text, drops the images.
- **Run / steer coder sessions.** Subscribe to `coder:event`,
  render the transcript, `coder_send` (send / steer), `coder_abort`.
  Opening a session is **windowed** (`coder_open_session` with
  `max_events`): only the newest slice of a long transcript is
  replayed over the WS, so a very long or image-heavy session
  renders immediately instead of shipping its whole JSONL up front.
  The backend mounts the runtime from the _full_ record list (the
  next turn's `messages` stay complete) and inserts a
  `history_window_start` boundary event carrying the full-sequence
  ordinal where the window begins; the phone's upward scroll /
  "Load older" pill then pages earlier history in via
  `coder_session_history_older(id, before_event_ordinal,
max_events)`, which replays the slice ending just before that
  ordinal and prepends it. Session list / new reuse the existing
  `coder_*` commands. The composer keeps **send available while a
  turn runs** (the message queues as a steer, same as the desktop);
  Stop is a separate smaller button. Sends are **optimistic**: a
  pending row renders immediately, confirmed away by the echoed
  `user_message`. A failed RPC (typically the bridge's forward
  timeout while the IDE host sleeps) flips the row to "delivery
  unconfirmed" with copy / resend / dismiss — deliberately not a
  global error, because a timed-out forward frequently _does_
  arrive when the IDE wakes and drains its socket, at which point
  the echo reconciles the row away. A `not connected` rejection
  (thrown before anything hits the wire) is the distinct **unsent**
  state instead: provably never left the phone, so it auto-resends
  on reconnect — no double-send possible, no question asked.
  Unresolved entries persist in `localStorage` (restored on launch;
  mid-flight `sending` restores as `unconfirmed` since the RPC's
  fate is unknown), each carrying its own workspace/ide carrier so
  a post-restart resend targets the right IDE. The workspace switcher's Start
  button shows a disabled "Starting…" state and polls the listing
  until the launched workspace reports live. A queued steer renders as a
  muted "queued" bubble; tapping it reveals two chips — **un-queue**
  (`coder_unqueue_steer`, pops it back into the composer to edit)
  and **go now** (`coder_drain_steer_now`, cancels the running turn
  so the steer drains immediately). Both are session-targeted by id
  (the session the phone has open, not the desktop's visible one)
  via the runner's `unqueue_steer_in` / `drain_steer_now_in`. The session title is editable
  inline from the header (`coder_rename_session`): the backend
  persists a `TitleUpdate` and broadcasts `session_title_updated`,
  so the desktop panel and every subscribed phone pick the new
  title up off the event channel without a refresh. The session
  list's **"running" pip is seeded from the backend**
  (`coder_running_sessions`) on workspace open and each folder's
  session-list refresh — it's otherwise event-driven and would
  miss sessions already in flight at subscribe time or a queued
  steer (which emits no live `user_message`) — then kept current
  by live events; a replayed `user_message` never flips it (a
  windowed replay pairs it with a trailing `turn_complete`, but a
  queued steer has no terminator).
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
- **Switch back to previous branch.** The mirror gesture: when the
  folder is _on_ its default branch and git remembers a previous
  branch (`@{-1}`), a "⇄ Switch to <branch>" chip swaps the working
  tree back to it. The previous branch name is forwarded in the
  `workspace_scm_status` response's `branch.previous_branch` field
  (resolved server-side from `git rev-parse --abbrev-ref @{-1}`), so
  the chip hides itself when there's no recorded previous branch
  (fresh repo, or the prior state was detached HEAD).
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
  an unlocked save writes the global default). On the HF route the
  card also edits the **standard model slug** inline (tap the Model
  row; empty resets to the built-in default) — free-text, no catalog
  browser on the phone. Provider CRUD and
  API keys stay desktop-only. The same settings payload round-trips
  the **context-window cap** (`context_window_overrides`): tapping
  the session's token-usage widget expands an editor for the
  resolved standard model's cap — entered in **thousands of
  tokens** (`500` = a 500k cap), empty = the model's full catalog
  window, with the current `used / window` shown for reference.
  Since the runner clamps `min(catalog, cap)` at every
  `CoderModels::context_window` call site, the phone's usage ring
  and auto-compaction respect it identically to the desktop (whose
  picker also edits in k).
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

## Headless enrolled IDE (`moon-remote serve`)

Shipped ([ADR 0059](decisions/0059-headless-enrolled-ide.md)): a
remote dev box can serve its workspaces to the phone with no desktop
session. `moon-remote` is an enrolled IDE without a webview — same
relay protocol, same keyring/enrollment semantics, same companion RPC
dispatcher (the desktop and the headless binary now share one
implementation in `crates/moon-remote`'s lib). Setup on the box:
`moon-remote login` (HF device flow) → `enroll --bridge --code` →
`workspace-add --name --folder` → `serve --workspace <slug>`. Needs a
Secret Service for the keyring (ADR 0035's `dbus-run-session` +
`gnome-keyring-daemon` recipe). The phone sees the box as another IDE
group in its switcher; relayed event envelopes carry `(ide,
workspace)` carrier tags so multi-IDE fan-in can't cross-light pips.
Folder binds made at runtime (a coordinator's `clone_repo` /
`init_repo`, worker worktrees, the phone's remove-project) persist to
`session.json` off `WorkspaceFoldersChanged` — the desktop's frontend
does this saving continuously, headless has no frontend, so `serve`
runs its own reconcile task; without it those binds evaporated on
restart.

## Cloud / always-on future

The _next_ shape — "the loop survives with **no** IDE process at all,
attachable from anywhere" — would make the headless core a standing
daemon both the laptop UI and the phone attach to over the same
JSON-RPC surface. `moon-remote serve` covers most of the practical
ask (an always-on box runs the loop; phones drive it), but it is
still one process per workspace whose in-flight turns die with it.

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

## Reliability + polish (round N)

- **Bubble timestamps**: user and assistant bubbles carry a `title`
  tooltip with the message's wall-clock time (time-only today, date +
  time otherwise) — parity with the desktop transcript's hover.
- **Running-pip consistency**: replayed terminator events no longer
  flip a session's busy pip off; only live terminators do, and replay
  asserts running state via the batch's `in_flight` flag. Previously
  opening a running session and backing out greyed its pip until the
  next list refresh.
- **Send resilience**: RPC calls carry a 30s client-side deadline (a
  zombie-OPEN socket otherwise parks the waiter forever), and a send
  that fails at the transport level (not connected / connection
  closed / call timed out) tears the socket down, reconnects, and
  retries once before surfacing an error.
- **Failed-session badges**: `SessionSummary.last_error` is true when
  the newest conversational record is a persisted turn error (folded
  during the summary scan, so it survives restarts and applies to
  worker sessions a coordinator spawned). The session list shows a
  red `!` in place of the idle pip — which sessions need a retry is
  visible at a glance. Kept live between refreshes by the error /
  user_message event stream. The desktop session list renders the
  same flag (red `!` dot + "last turn failed" meta label, below
  awaiting-input/running in precedence).
- **Ctrl/Cmd-click + middle-click open in a new tab**: session rows
  and → open-worker links build a per-session hash route
  (`sessionRouteHash`) and `window.open` it, desktop-browser style;
  plain click keeps in-tab SPA navigation.
- **Relaunch interrupted sessions**: `SessionSummary.interrupted` is
  true when the newest conversational record leaves the turn
  unfinished (user message with no answer, assistant with pending
  tool calls, tool result with no follow-up) — the on-disk shape of
  a restart-kill or stop. The runner clears it for currently-running
  turns. Both lists badge it (amber `!`); opening one on the phone
  surfaces the retry bar ("turn never finished — retry to relaunch"),
  which drives `coder_retry_last_turn`.
- **Rotation editor**: the provider card's Fallbacks row expands to
  a per-slug list with up/down reorder and remove buttons plus an
  append field; every action persists immediately through
  `coder_set_model_settings`.
