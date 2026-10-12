use moon_protocol::MoonError;

/// Reveal `path` in the host's file manager (Explorer / Finder /
/// org.freedesktop.FileManager1). Deliberately host-side, not a
/// `WorkspaceHost` method: the file manager runs on the user's
/// desktop regardless of whether the workspace is containerised —
/// the bind mount makes the path valid there. The opener plugin's
/// Linux backend does a blocking D-Bus roundtrip, so run it on the
/// blocking pool.
#[tauri::command]
pub async fn fs_reveal_in_folder(path: String) -> Result<(), MoonError> {
	tauri::async_runtime::spawn_blocking(move || {
		tauri_plugin_opener::reveal_item_in_dir(&path).map_err(|e| MoonError::io(e.to_string()))
	})
	.await
	.map_err(|e| MoonError::internal(e.to_string()))?
}
