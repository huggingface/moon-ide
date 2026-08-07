# ADR 0059 — Headless enrolled IDE (`moon-remote serve`)

Date: 2026-08-08
Status: accepted; builds on
[ADR 0031](0031-remote-bridge-relay.md) (relay topology) and
[ADR 0035](0035-public-relay-deployment.md) (public relay).

## Context

The operator wants a coder-capable moon-ide on a remote machine (a
dev box with the repos on it) driven from the phone, with no desktop
session anywhere. Phase 14 already ships everything except the
process: the relay hub is deployed, the phone speaks to enrolled IDEs
by carrier id, and the coder core (`moon-coder`, `moon-core`) has no
Tauri dependency. What was missing is an enrolled IDE that doesn't
need a webview.

This is **not** the "headless core on the bridge" that ADR 0023/0031
rejected: the coder loop stays co-located with the filesystem it
edits, the relay still only relays, and enrollment/tokens are
unchanged. It's the desktop IDE minus the window.

## Decision

Re-charter `crates/moon-remote` (previously a stub reserved for the
SSH/Codespaces `RemoteHost` server, ADR 0011) as **lib + bin**:

- **Lib** — the companion RPC surface and the outbound relay client,
  moved out of `src-tauri` so there is exactly one implementation:
  - `rpc`: `BridgeRpcHandler` trait + the full `coder_*` /
    `workspace_*` dispatcher (formerly `src-tauri/bridge_rpc.rs`).
    Tauri coupling was factored into two seams: a `SettingsContext`
    (dirs + workspace id) and an optional `WorkspaceLauncher` trait.
  - `settings`: get/set model settings + per-workspace provider-lock
    persistence (formerly `commands/coder.rs` `_impl`s).
  - `relay`: the enroll/register/forward WS client (formerly
    `src-tauri/remote_bridge.rs`), `tokio::spawn` instead of Tauri's
    runtime.
    The desktop now links this crate; its `bridge_rpc.rs` /
    `remote_bridge.rs` are thin adapters (Tauri launcher, re-exports).
- **Bin** — `moon-remote` subcommands for the headless box:
  - `login` — HF sign-in via the existing device flow (prints URL +
    code); stores the same keyring bundle the desktop uses.
  - `enroll --bridge wss://… --code …` — one-shot relay enrollment;
    credential in the keyring, `ide_id` = hostname.
  - `workspace-add --name … --folder …` — catalog entry in
    `state.json` + folder binding in `session.json`, mirroring the
    desktop's create-workspace + open-folder.
  - `serve --workspace <slug>` — desktop-boot subset (state load,
    registry + folder restore, models seed, `CoderHandle`), then the
    relay client with the full catalog registered and this workspace
    live. `workspace_launch` from the phone spawns a sibling
    `moon-remote serve` process (ADR 0014 process-per-workspace,
    same instance.sock single-instance lock).

The headless binary shares the desktop's dir identity (`moon-ide`
config/data dirs) and keyring entries, so a machine can flip between
desktop and headless serving of the same workspaces.

Additionally the bridge now stamps relayed event envelopes with their
`(ide, workspace)` carrier and the phone drops events that don't
match its active carrier — with N enrolled IDEs, folder paths and
workspace slugs collide across hosts and previously could cross-light
pips/attention dots.

## Alternatives considered and rejected

- **A new crate name.** `moon-remote`'s documented charter (RemoteHost
  server) is a different axis, but architecture.md already predicted
  the convergence ("the same headless moon-core serving JSON-RPC over
  a channel shape … expected to converge"), and a third
  networked-binary crate would be sprawl. The RemoteHost story, when
  it lands, belongs in this crate too.
- **Running the Tauri binary headless (xvfb / no-window mode).**
  Drags webkit/GTK onto servers and fights Tauri's lifecycle for zero
  benefit — everything the phone needs is Tauri-free already.
- **Serving local `moon-bridge` R/S frames from the headless
  instance.sock.** Deferred: the headless listener only answers
  liveness probes (accept-and-drop). LAN-local phone service without
  the relay is a follow-up if anyone asks; the relay path covers the
  actual request.

## Consequences

- A headless box needs a Secret Service for the keyring (HF token,
  provider keys, relay credential): same `dbus-run-session` +
  `gnome-keyring-daemon` recipe ADR 0035 documents for the relay.
- Coder tool calls run host-side on the headless box (no
  auto-resumed dev containers); `bash` falls back to host exactly as
  it does on the desktop when the container isn't running.
- `Register` still isn't refreshed on workspace-set changes (known
  Phase 14 gap) — a `workspace-add` while `serve` is running shows up
  on the phone after the next reconnect.
