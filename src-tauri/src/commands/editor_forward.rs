//! Tauri commands that resolve a parked forwarded-edit request.
//!
//! Companion to [`crate::focus_socket`]. When a container terminal
//! invokes `git commit --amend` (or any other tool that respects
//! `$GIT_EDITOR`), `moon-edit` connects to the workspace's
//! `instance.sock`, sends an `E\n<host-path>\n` request, and parks
//! waiting for `OK\n` / `CANCEL\n`. The listener registers the
//! request on the shared [`EditorRegistry`], emits an
//! `editor:request` Tauri event with `{ id, host_path }`, and waits.
//!
//! The frontend opens the path as an external buffer
//! (`Workspace.openHostFile`), tags it with `pendingEdit = id`,
//! and surfaces a per-tab "Finish editing" affordance. When the
//! user finishes or cancels, the frontend calls one of these two
//! commands; the registry resolves the parked oneshot; the
//! listener writes the reply to the socket; `moon-edit` exits 0
//! or 1; `git` proceeds or aborts.
//!
//! See [ADR 0021](../../../specs/decisions/0021-git-editor-forward.md)
//! and [`specs/containers.md`](../../../specs/containers.md)
//! § "Editor forwarding".

use std::sync::Arc;

use moon_protocol::MoonError;
use tauri::State;

use crate::focus_socket::{EditOutcome, EditorRegistry};

/// Finish a forwarded edit. Called by the frontend after it has
/// already saved the buffer to disk; the host-side path is the
/// one the shim sent in the `editor:request` event. The IDE has
/// already written the bytes via `fs_write_file_host`, so the
/// only thing we do here is unpark the listener.
///
/// Returns `Ok(true)` if the registry had a matching pending
/// edit and was resolved; `Ok(false)` if the id was unknown
/// (already resolved, or the connection dropped before we got
/// here). The frontend treats both as success.
#[tauri::command]
pub async fn editor_forward_finish(registry: State<'_, Arc<EditorRegistry>>, id: String) -> Result<bool, MoonError> {
	Ok(registry.resolve(&id, EditOutcome::Finished).await)
}

/// Cancel a forwarded edit. Called when the user closes the
/// pending-edit tab without finishing (or hits an explicit
/// Cancel affordance — Phase 1 has neither yet, but the verb is
/// reserved). Same semantics as `editor_forward_finish` but the
/// shim sees `CANCEL\n` and exits non-zero so `git` aborts.
#[tauri::command]
pub async fn editor_forward_cancel(registry: State<'_, Arc<EditorRegistry>>, id: String) -> Result<bool, MoonError> {
	Ok(registry.resolve(&id, EditOutcome::Cancelled).await)
}
