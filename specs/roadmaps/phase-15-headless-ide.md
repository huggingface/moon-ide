# Phase 15 — Headless enrolled IDE

STATUS: landed. Decision record:
[ADR 0059](../decisions/0059-headless-enrolled-ide.md).

Goal: run a coder-capable moon-ide on a remote machine with no
desktop session and drive it from the phone through the existing
relay (`wss://` bridge, Phase 14).

## 15.1 — Shared companion surface (landed)

- Move the companion RPC dispatcher (`bridge_rpc.rs`), the model/
  provider settings bodies, and the outbound relay client
  (`remote_bridge.rs`) from `src-tauri` into `crates/moon-remote`'s
  new lib. Tauri coupling factored into `SettingsContext` +
  `WorkspaceLauncher`.
- Desktop rewired to link the shared code; behaviour unchanged.

## 15.2 — Headless binary (landed)

- `moon-remote login` — HF device-flow sign-in into the shared keyring.
- `moon-remote enroll --bridge <wss://…> --code <code>` — relay
  enrollment, credential in the keyring, `ide_id` = hostname.
- `moon-remote workspace-add --name <n> --folder <abs path>` —
  catalog entry + folder binding, desktop-compatible.
- `moon-remote model [--standard <slug>] [--cheap <slug>]` — show/set
  the model picks in `state.json` (same store as the desktop picker);
  a running `serve` re-reads on restart.
- `moon-remote serve --workspace <slug>` — boot (state load, folder
  restore, models seed, `CoderHandle`), instance.sock single-instance
  lock, relay connect with the full catalog registered.
  `workspace_launch` from the phone spawns a sibling serve process.

## 15.3 — Multi-IDE event attribution (landed)

- Bridge stamps relayed event envelopes with `(ide, workspace)`;
  the phone drops events not matching its active carrier.

## Deliberately not done

- Local `moon-bridge` service from the headless instance.sock
  (accept-and-drop only); the relay is the supported path.
- Live `Register` refresh on workspace-set changes (Phase 14 gap,
  unchanged): new workspaces appear after the next reconnect.
- Containerised dev shells on the headless box: coder tools run
  host-side.

## Completion checklist

- [x] `cargo clippy --workspace` / tests / fmt clean
- [x] Desktop still builds against the shared crate
- [x] Headless binary boots, enrolls, serves (manual: needs a real box)
- [ ] Human review
