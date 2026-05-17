# 0081 — Coder per-workspace provider lock

Date: 2026-05-17

## What shipped

- New per-workspace pin on the coder's active provider, persisted at `WorkspaceSession::coder_provider_lock` in each workspace's `session.json`. Two-variant tagged enum: `{kind:'hf'}` or `{kind:'user', id}`.
- Effective active provider = lock if pinned, else global `state.json` `coder.active_provider`. Boot, `coder_get_model_settings`, and the runner's `set_providers` calls all route through the resolver so the workspace runs against the locked provider from the very first turn.
- Picker modal grows a "Lock provider to this workspace (\<name\>)" checkbox with a hint that names the locked provider when active. Save translates the checkbox + the modal's currently-active provider into the wire-shape `CoderProviderLock | null`.
- Locked saves write only to `session.json`; the global `coder.active_provider` in `state.json` is left untouched. Unlocked saves keep the previous behaviour (write the global).
- Stale-pin handling: deleting a provider that this workspace was pinned to clears the lock as part of `coder_delete_provider`. Sibling workspaces (other OS processes) that pinned the same id get the existing fall-back-to-HF + `tracing::warn!` path on next read.

## How to test

Setup: at least two workspaces in the catalog, e.g. `personal` and `work`, plus an OpenRouter or Anthropic provider configured in either. The lock is a per-workspace knob; you need the second workspace to confirm it doesn't bleed.

### Lock to a user provider

1. Open `personal` (`moon-ide --workspace personal`). Open the coder panel, click the model-settings cog.
2. Switch the active provider to OpenRouter.
3. Tick "Lock provider to this workspace (personal)". The hint underneath should change to "Locked to OpenRouter. Other workspaces can switch freely; this one stays put."
4. Click **Save**. The modal closes.
5. Open `~/.local/share/moon-ide/workspaces/personal/session.json`. Confirm `"coder_provider_lock": { "kind": "user", "id": "prov-…" }` matches the OpenRouter id.
6. Open `~/.config/moon-ide/state.json` (or the platform equivalent). Confirm `coder.active_provider` is **not** that OpenRouter id — it's whatever it was before step 2 (the global default for unlocked workspaces). _If it was already OpenRouter from a prior session, you can verify by switching providers in step 2 to something different — same flow, the global stays put._
7. Send a turn from `personal`'s coder panel. The model-settings cog still says OpenRouter is active; the runner's `coder_status` event reflects the same model.

### Verify another workspace isn't dragged along

1. From `personal`, switch to `work` (`Ctrl+Shift+O` → `work`, or spawn `moon-ide --workspace work`).
2. Open the coder model settings on `work`. The active provider tab should be whatever the **global** default was — _not_ OpenRouter (unless that was already the global before any of this).
3. In `work`, switch active provider to Anthropic. Don't tick the lock. **Save**.
4. Confirm `state.json`'s `coder.active_provider` is now the Anthropic id.
5. Switch back to `personal`. Open the model settings. The active tab should still be **OpenRouter** (the lock survived `work`'s flip). Confirm in `session.json` that the lock is unchanged.
6. Send a turn from `personal`. Runner uses OpenRouter; the global `state.json` still says Anthropic.

### Unlock and re-follow the global

1. In `personal`, open model settings. Untick the "Lock provider…" checkbox. The hint should change back to "Off — this workspace follows the global active provider."
2. Save.
3. `session.json` no longer has a `coder_provider_lock` key (or has it as `null`).
4. The active provider tab in the modal now matches the global (Anthropic from the previous step). The runner switches to Anthropic for the next turn.

### Lock to HF specifically

1. Switch active provider to **Hugging Face** in the modal.
2. Tick the lock. Save. `session.json` should show `"coder_provider_lock": { "kind": "hf" }`.
3. From another workspace, flip the global provider to OpenRouter. Save.
4. Reopen `personal`. Active provider is still HF; runner uses HF on the next turn.

### Stale-pin recovery

1. With `personal` locked to OpenRouter, open the model settings and click **Edit** on the OpenRouter tab → **Delete provider**. (Or use any other route to call `coder_delete_provider`.)
2. The lock is cleared as part of the delete (`session.json`'s `coder_provider_lock` reverts to absent / `null`).
3. The runner falls back to the global `state.json` `coder.active_provider`. No `tracing::warn!` for this workspace because the lock was cleaned up; if a sibling process was also pinned to the same id, _that_ sibling gets a `provider lock points at deleted provider; falling back to HF` warn on its next boot.

## What must keep working

- Single-workspace setups (the common case): no `session.json` lock means everything behaves exactly as before. The global `coder.active_provider` is the one source of truth.
- Adding a new user provider while locked: the new provider is added to the global `providers` list as before, and the runner's effective active provider stays on the lock — `coder_save_provider` re-resolves the effective active before calling `set_providers`.
- Provider switching inside the modal: every tab click still updates the modal's local state without IPC. Save is the only thing that writes; the lock decides where (session.json vs state.json).
- The "+ Add provider" / Custom / OpenRouter / Anthropic preset flow in the modal is unchanged.
- `bill_to`, per-slug `context_window_overrides`, and the picker's catalog UI are all orthogonal to the lock; they round-trip the same way regardless.

## Known limitations

- The lock is a single boolean from the user's POV: "lock this workspace to whatever provider is active in the modal right now". There's no separate "lock to HF specifically" vs "lock to whatever I happen to be on" — they collapse to the same thing because the active provider _is_ what gets locked.
- Sibling workspace cleanup on provider delete: if workspace A and workspace B both pinned to the same provider id and A deletes that provider, B's `session.json` still carries the orphaned id. B falls back to HF + `tracing::warn!` on its next boot — that's the existing stale-id graceful path, not a new failure mode. We don't reach across processes to scrub other workspaces' session files.
- The lock UI lives only in the model-settings modal. There's no folder-bar / status-bar indicator that a workspace is currently locked beyond what the modal shows. If the team asks for a persistent indicator (e.g. a tiny chip in the coder panel header), wire it up later off `coder.modelSettings.provider_lock`.

## Related

- [`crates/moon-protocol/src/coder_models.rs`](../../crates/moon-protocol/src/coder_models.rs) — `CoderProviderLock` + `CoderModelSettings.provider_lock`.
- [`crates/moon-protocol/src/session.rs`](../../crates/moon-protocol/src/session.rs) — `WorkspaceSession.coder_provider_lock`.
- [`src-tauri/src/commands/coder.rs`](../../src-tauri/src/commands/coder.rs) — `coder_get_model_settings`, `coder_set_model_settings`, `coder_save_provider`, `coder_delete_provider`.
- [`src-tauri/src/lib.rs`](../../src-tauri/src/lib.rs) — boot-time effective-provider resolution feeding `CoderModels`.
- [`src/lib/components/CoderModelSettingsModal.svelte`](../../src/lib/components/CoderModelSettingsModal.svelte) — picker UI.
- [`specs/coder.md`](../coder.md) § "Per-workspace provider lock".
- [Test plan 0072](0072-coder-multi-provider.md) — original multi-provider design that this layers onto.
