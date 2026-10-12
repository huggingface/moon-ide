use camino::Utf8Path;
use moon_core::app_state as core_app_state;
use moon_protocol::workspace::WorkspaceMeta;

/// Bump `last_active_at` for `workspace_id` in the on-disk
/// `state.json`. Best-effort: a write failure is logged but
/// otherwise swallowed — the picker just shows a slightly stale
/// "recent" sort until the next successful save.
pub async fn bump_last_active(config_dir: &Utf8Path, workspace_id: &str) {
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);
	let id = workspace_id.to_string();
	let id_for_log = id.clone();
	let result = core_app_state::mutate(config_dir, move |s| {
		for meta in s.workspaces.iter_mut() {
			if meta.id == id {
				meta.last_active_at = now;
				return;
			}
		}
		// The workspace was bumped before the catalog knew
		// about it. Repair rather than silently drop.
		s.workspaces.push(WorkspaceMeta {
			id: id.clone(),
			name: id,
			last_active_at: now,
			color: None,
		});
	})
	.await;
	if let Err(err) = result {
		tracing::warn!(error = %err, workspace_id = %id_for_log, "failed to persist last_active_at bump");
	}
}
