use camino::Utf8PathBuf;
use moon_protocol::editorconfig::EditorConfig;
use moon_protocol::MoonError;

use crate::AppCore;

pub async fn editorconfig_for_path(app: &AppCore, path: String) -> Result<EditorConfig, MoonError> {
	let entry = app.workspaces.require_active_folder().await?;
	let path = Utf8PathBuf::from(path);
	entry.host.editorconfig_for(&path).await
}

crate::commands! {
	editorconfig_for_path([app] path: String),
}
