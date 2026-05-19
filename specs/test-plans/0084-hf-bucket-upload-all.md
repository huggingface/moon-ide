# Test plan 0084: HF bucket bulk upload + session-save merge

- **Date**: 2026-05-19
- **Phase**: post-Phase 7 polish (follows 0083)

## What shipped

- **`Upload all sessions now`** button in the bucket settings
  modal. One click pushes every top-level session JSONL from
  every folder bound to the workspace into the connected HF Hub
  bucket; sessions already up to date are skipped at the byte-
  length marker. Result strip in the modal reports
  `<uploaded> uploaded, <skipped> already up to date,
<failed> failed`, with per-session failure detail when
  anything errored.
- Backend folds the round-trips: one `xet-write-token` fetch
  - parallel Xet CAS uploads (capped at 4) + **one** `/batch`
    NDJSON POST that binds every newly-uploaded hash. A workspace
    with N stale sessions costs roughly **two** Hub-API round-
    trips instead of `3·N` if the user clicked the per-row Upload
    button N times.
- `coder_hub_upload_all_sessions` Tauri command + matching
  `HubUploadAllSummary` / `HubUploadFailure` protocol types.
  Per-session `HubSyncStarted` / `HubSyncFinished` events are
  emitted from the runner so the existing session-row cloud
  icon animation works across the bulk run — no separate
  progress channel.
- Autosync toggle now flashes `"Autosync on — uploads after
every turn."` / `"Autosync off."` so the user sees the
  persistence took effect.
- **Session-state clobber fix.** `session_save` now reads the
  on-disk `WorkspaceSession`, keeps its backend-managed fields
  (`coder_hub_bucket`, `coder_provider_lock`, `forwarded_ports`),
  and overlays only the frontend-owned ones (`folders` +
  `active_folder_path`) on top. Before this change every
  `persistAppState` tick — including the one that fires on
  every folder switch — overwrote the bucket binding, the
  provider lock, and the port-forward set to their `Default`
  values; the cloud icon went inactive after a folder switch
  and the user had to re-connect the bucket every time.

## How to test

Prerequisites: `bun install` + `cargo check` at the workspace
root; signed in to Hugging Face via the existing device flow;
a workspace with ≥ 1 folder and ≥ 1 coder session on disk.

### 1. Folder-switch persistence (the clobber regression)

1. Open a workspace bound to ≥ 2 folders. Connect a Hub
   bucket via the cloud-sync header icon. Expected: the icon
   flips to the accent colour.
2. Switch active folder via the folder bar.
3. Re-open the trace-sync modal (cloud-sync icon). Expected:
   the modal still shows the connected bucket; the cloud-sync
   icon in the panel header is still accent-coloured. No
   re-connect prompt.
4. Add a port forward (Ports tab), switch folders, come back.
   Expected: the forward is still present.
5. With the workspace open, run
   `cat ~/.local/share/moon-ide/workspaces/<id>/session.json |
jq 'keys'`. Expected: the file contains `coder_hub_bucket`
   (and `forwarded_ports` if you added one in step 4).

### 2. Autosync flash

1. With a bucket connected, open the trace-sync modal and
   toggle Autosync on. Expected: a flash reads
   `"Autosync on — uploads after every turn."`
2. Toggle it off. Expected: `"Autosync off."`
3. Close + reopen the modal. Expected: the checkbox reflects
   whatever you set; the persistence survived.

### 3. Upload all — happy path

1. With the bucket connected and ≥ 2 local sessions across
   ≥ 2 folders, open the modal and click **Upload all
   sessions now**. Expected: the button label becomes
   `Uploading sessions…` and is disabled; the per-row cloud
   icon on the sessions list (for any folder you're currently
   looking at) flips through `idle → syncing → synced`.
2. The button returns to its normal label; the result strip
   reads `Last run: <uploaded> uploaded, <skipped> already up
to date.` Bottom-of-screen flash mirrors the summary.
3. In the Hub bucket page, every folder's `<slug>/` directory
   exists and contains the expected JSONLs. Open one — the
   pi-mono trace viewer renders it inline.
4. Click **Upload all sessions now** again immediately.
   Expected: the result strip flips to
   `0 uploaded, N already up to date.` — the byte-length
   skip shortcut kicked in for every session.

### 4. Upload all — no sessions yet

1. From a clean workspace with zero session JSONLs on disk,
   click **Upload all sessions now**. Expected: the result
   strip reads `No sessions on disk yet.` and the bottom
   flash is `No sessions to upload yet.`

### 5. Upload all — partial failure

1. Kill network mid-batch (`sudo ip link set <iface> down`
   while the upload is in flight) on a workspace with several
   stale sessions. Expected: the run completes; the result
   strip reads `Uploaded X, N failed`; the failure list shows
   each failed `session_id` with the Hub error string. The
   modal stays open and the panel doesn't crash.
2. Restore the network and click **Upload all sessions now**
   again. Expected: only the previously-failed sessions are
   actually pushed (the successful ones are now `skipped`).

### 6. Cross-folder batching

1. With ≥ 2 folders bound, run **Upload all sessions now**
   from a workspace where folder A has 2 sessions and folder
   B has 3 sessions, all unsynced. Expected: the result
   shows `5 uploaded`; the Hub bucket has two top-level
   directories, one for each folder's slug, containing the
   right session ids.

## What must keep working

Regression checks. If any of these break, the commit needs a
follow-up.

- The per-row **Upload** button on the session list still
  pushes a single session synchronously and updates that
  row's marker. The bulk-upload path doesn't share the
  debounce slot used by autosync, so a row mid-autosync isn't
  blocked by a parallel bulk run.
- Autosync after `TurnComplete` still fires on the parent
  session id and still skips when `bucket.autosync` is
  `false`. The bulk-upload path doesn't touch the autosync
  debounce map.
- `coder_hub_disconnect` clears the binding cleanly; the
  cloud-sync icon goes inactive, the per-row markers
  disappear, and the bucket itself stays on the Hub.
- The two pre-existing session-row states (`syncing`,
  `synced`) still drive their styling — the bulk run emits
  the same envelopes the per-row path does.
- `WorkspaceSession`'s `coder_provider_lock` survives folder
  switches (`coder_set_workspace_provider_lock` writes
  through the same `session.json` the merge fix now
  protects).
- `forwarded_ports` survives folder switches for the same
  reason.

## Known limitations

Things we deliberately did not do.

- Sub-agent sessions are not uploaded by this path. They
  live under per-parent subdirectories on disk; the panel's
  per-row button only ever targets the top-level row's id,
  so matching that behaviour keeps "Upload all" predictable.
  Folding sub-agents in is a separate sub-phase.
- The `xet-write-token` is fetched once for the whole batch.
  A run that spans more than the token's `exp` window will
  fail on the CAS push and surface that session in
  `failed[]`. In practice tokens are long enough that we
  don't bother re-fetching; if it bites we add a retry that
  refreshes the token and re-uploads the failures.
- CAS upload concurrency is hard-capped at 4. Raise it
  later if a real workload shows the bottleneck.
- The bulk run reads each session file fully into memory
  before pushing. A workspace with very large traces (tens
  of MB each) and dozens of stale sessions could spike RAM
  briefly. We bound concurrency at 4 specifically to keep
  this tractable; streaming the file into the Xet client is
  a follow-up if anyone hits it.

## Related

- Specs: [coder.md § Bucket sync (HF buckets)](../coder.md#bucket-sync-hf-buckets)
- Prior test plans: [0083-hf-bucket-sync.md](0083-hf-bucket-sync.md)
