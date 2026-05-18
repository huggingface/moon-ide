# Test plan 0083: HF bucket sync

- **Date**: 2026-05-18
- **Phase**: post-Phase 7 polish

## What shipped

- One bucket per workspace, owned by either the signed-in user or
  one of their orgs, surfaced through a dedicated cloud-sync icon
  in the coder panel header (separate from the model-settings
  cog) that opens a trace-sync settings modal.
- `coder_hub_*` Tauri commands cover the lifecycle: list
  namespaces, create + bind bucket, set autosync, disconnect,
  manually upload a single session.
- Per-session cloud icon on the sessions list reflects four
  states (idle / syncing / synced / failed) off the streamed
  `HubSyncStarted` / `HubSyncFinished` events.
- Autosync (off by default after connect) enqueues a debounced
  push on every successful `TurnComplete`; the runner never
  blocks on the upload.
- Backend pushes the JSONL through `hf-xet`'s `XetUploadCommit`
  (CAS upload + Merkle hash), then `POST /api/buckets/...batch`
  binds the hash at `<folder-slug>/<id>.jsonl` so each folder
  bound to the workspace gets its own directory at the bucket
  root. A README is written once at bucket-create time.

## How to test

Prerequisites: `bun install` + `cargo check` at the workspace
root; signed in to Hugging Face via the existing device flow
(the connect modal won't open without a valid session); the user
account has `contribute-repos` granted at the OAuth consent screen
(the current scope set requests it).

### 1. Connect — user namespace

1. Open a workspace, expand the coder panel. Expected: the
   header now carries a cloud-sync icon (between the chat-bubble
   and the model-settings cog).
2. Click the cloud-sync icon. Expected: the trace-sync modal
   opens with a single "Connect to Hugging Face" primary button.
3. Click it. Expected: the connect modal opens on top with the
   user's login pre-selected in the Owner dropdown, the name
   input defaulting to `<workspace-basename>-traces`, and
   Private pre-checked.
4. Click Create bucket. Expected: a flash banner reads "Bucket
   created at `<owner>/<name>`."; both modals close; the
   cloud-sync header icon switches to the accent colour and its
   tooltip now reads `HF trace sync: <owner>/<name>`.
5. Reopen the trace-sync modal. Expected: the connected state
   shows `<owner>/<name>` as a deep link, an unchecked Autosync
   checkbox, and Close / Disconnect buttons.
6. Open the Hub URL shown in the connected state. Expected: the
   bucket exists, is private, and at its root contains a single
   file `README.md` whose first heading is "moon-ide traces —
   `<workspace-basename>`" and whose body mentions the
   `<folder-slug>/` directory layout. No `sessions/` directory
   at this point — folder directories appear on first upload.
7. (Slug check) The default bucket name suggested in step 2
   matches `slugify(<workspaceName>) + '-traces'` —
   `Hugging Face` → `hugging-face-traces`, not
   `Hugging Face-traces`.

### 2. Connect — org namespace

1. Repeat the connect flow, picking an org from the dropdown.
   Expected: the bucket lands under `<org>/<name>` and the URL on
   the connected state points there.
2. Disconnect, then reopen the connect modal, pick the org
   namespace, and re-use the same name. Expected: the create
   call succeeds (409 from the Hub maps to success — moon-ide
   adopts the existing bucket without a name collision error).

### 3. Manual upload

1. With the workspace bound (autosync off), open the sessions
   list. Expected: every session row carries a cloud-up icon.
2. Hover the icon on an unsynced row. Expected: tooltip reads
   "Upload to `<owner>/<name>`".
3. Click. Expected: the icon flips to the syncing state (subtle
   pulse, accent colour). After the push lands, the icon flips
   to a muted state and the tooltip reads
   "Synced to `<owner>/<name>`".
4. Refresh the Hub bucket page. Expected: a top-level directory
   named after the source folder's slug (e.g. `moon-ide-7e985eb6`)
   contains `<session-id>.jsonl`; clicking it opens the pi-mono
   trace viewer with the messages from the local session. The
   slug should match what's under
   `~/.local/share/moon-ide/coder-sessions/` for the same folder.

### 4. Autosync end-to-end

1. Flip the Autosync checkbox on. Expected: no flash, no upload —
   we don't auto-push on toggle, only on the next TurnComplete.
2. In the active session, send a short prompt and let the turn
   finish. Expected: about 2 s after the final `TurnComplete`
   event the row's icon pulses, then flips to synced.
3. Send a quick follow-up while the previous debounce is still
   in flight (within 2 s of `TurnComplete`). Expected: only one
   eventual upload — the debounce coalesces.
4. Hub bucket re-fetch shows the latest JSONL has grown.

### 4b. Multi-folder grouping

1. In a workspace bound to ≥ 2 folders (e.g. the `hugging-face`
   workspace with `moon-ide`, `moon-landing`, `hub-docs`), open
   the coder panel on **two different folders** in turn and run
   one short session in each.
2. Manually upload both. Expected: the Hub bucket root shows
   **two distinct directories**, one per folder slug, each
   carrying its own session JSONL. Sessions never collide at
   the bucket root regardless of the autosync toggle.
3. The slug for each folder must match the corresponding
   directory under
   `~/.local/share/moon-ide/coder-sessions/<slug>/` on the host.

### 5. Re-sync skip

1. With a row marked synced, click its cloud icon again.
   Expected: the upload short-circuits at the bytes check —
   `HubSyncFinished { ok: true }` arrives near-instantly and the
   icon stays in the synced state. No new Xet token is fetched
   (eyeball the network panel or `tracing::debug!` log line
   `"hub sync skipped (already at length)"`).

### 6. Failure handling

1. Disconnect, reconnect with a deliberately-invalid name (e.g.
   starting with `.`). Expected: the modal surfaces a validation
   error inline; the Create button stays disabled.
2. Disconnect (clear the workspace binding), then sign out of HF
   from the panel header, then sign back in. Re-bind. Expected:
   create proceeds normally — the token rotation didn't poison
   the keyring.
3. With autosync on, kill the network mid-turn (`sudo ip link
set <iface> down` works). Expected: the row icon flips to
   the failed state with the error in the tooltip; the panel
   doesn't crash. Restore the network and click the cloud icon
   to retry — the upload succeeds.

### 7. Disconnect

1. From the settings section, click Disconnect. Expected: the
   section flips back to the "Connect to Hugging Face" button;
   the row icons disappear from the sessions list; the bucket
   itself stays on the Hub (verify by re-fetching the URL).

### 8. Server-side fence

1. While signed in, capture the OAuth bearer token (devtools →
   network, any `/api/buckets/...` request). Use it to attempt a
   batch addFile against a different user's bucket (or your own
   non-app bucket). Expected: the Hub responds with 403 — the
   `contribute-repos` scope only authorises buckets we created.

## What must keep working

Regression checks. If any of these break, the commit needs a
follow-up.

- The pre-existing model-settings popover continues to load and
  save unchanged — it no longer carries any HF-sync chrome.
- Coder sessions persist locally to JSONL exactly as before —
  no record in this feature touches `sessions.rs` write paths.
- `coder_hub_*` commands behave gracefully without a workspace
  bound (preboot mode): `get_binding` returns `null`,
  `list_namespaces` errors out cleanly through the panel.
- A session with no records (e.g. fresh `+` then `TurnComplete`
  on an empty prompt) never triggers a sync — autosync skips on
  `persisted_records == 0`.

## Known limitations

Things we deliberately did not do, with one-line justification.

- No "browse buckets I've already created" affordance — the
  per-workspace pointer is the only discovery path. A future
  moon-landing PR could add `?createdByApp=<client_id>` to
  `/api/buckets` if a second use case appears.
- No partial / append upload — every push round-trips the full
  JSONL. Xet dedup makes the unchanged prefix nearly free.
- No per-session opt-out toggle. NDA workspaces leave autosync
  off; the per-row Upload button is a deliberate click.
- Deleting the bucket from inside moon-ide isn't supported.
  Disconnect drops the binding; bucket deletion is a web-UI
  action.

## Related

- Specs: [coder.md § Bucket sync (HF buckets)](../coder.md#bucket-sync-hf-buckets)
- ADRs: [0005 — bootstrap](../decisions/0005-bootstrap.md)
- Prior test plans:
  [0071-coder-model-picker.md](0071-coder-model-picker.md),
  [0073-anthropic-prompt-caching.md](0073-anthropic-prompt-caching.md)
