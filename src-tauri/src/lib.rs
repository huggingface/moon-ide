//! Tauri shell for moon-ide. Wires Tauri commands to `moon-core`.

mod commands;
mod state;

use camino::Utf8PathBuf;
use moon_core::app_state as core_app_state;
use moon_slack::SlackClient;
use state::AppState;
use tauri::Manager;

pub fn run() {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,moon=debug")),
		)
		.init();

	tauri::Builder::default()
		.plugin(tauri_plugin_dialog::init())
		.plugin(tauri_plugin_opener::init())
		.invoke_handler(tauri::generate_handler![
			commands::workspace::workspace_open_local,
			commands::workspace::workspace_active,
			commands::workspace::workspace_list,
			commands::fs::fs_read_dir,
			commands::fs::fs_read_file,
			commands::fs::fs_write_file,
			commands::fs::fs_stat,
			commands::fs::fs_absolute_path,
			commands::fs::fs_trash,
			commands::fs::fs_delete,
			commands::search::search_files,
			commands::search::search_content,
			commands::app_state::app_state_load,
			commands::app_state::app_state_save,
			commands::editorconfig::editorconfig_for_path,
			commands::slack::slack_set_token,
			commands::slack::slack_status,
			commands::slack::slack_clear_token,
			commands::slack::slack_list_dm_bots,
			commands::slack::slack_select_bot,
			commands::slack::slack_clear_bot,
			commands::slack::slack_get_active_bot,
		])
		.setup(|app| {
			let config_dir = app
				.path()
				.app_config_dir()
				.map_err(|e| format!("could not resolve app config dir: {e}"))?;
			let config_dir =
				Utf8PathBuf::from_path_buf(config_dir).map_err(|p| format!("non-utf8 app config dir: {}", p.display()))?;

			let state = AppState::new(config_dir.clone());

			// Restore the last session's workspace synchronously so the
			// frontend's first call to `workspace_active` already sees it. The
			// session blob also carries open files and the active tab, but
			// those live entirely in the frontend — it pulls them via
			// `app_state_load` once Svelte is mounted. If the folder has been
			// moved or deleted, log it and keep the saved session: re-saving
			// would erase the user's tabs just because they unmounted a USB
			// drive once.
			tauri::async_runtime::block_on(async {
				match core_app_state::load(&config_dir).await {
					Ok(s) => {
						if let Some(session) = s.last_session.as_ref() {
							let path = Utf8PathBuf::from(&session.workspace_path);
							match state.workspaces.open_local(path.clone()).await {
								Ok(ws) => {
									tracing::info!(workspace = %ws.record.root, "restored last workspace");
								}
								Err(e) => {
									tracing::warn!(error = %e, path = %path, "failed to restore last workspace");
								}
							}
						}
					}
					Err(e) => {
						tracing::warn!(error = %e, "failed to load app state");
					}
				}

				// Rehydrate the Slack client from the keyring if the
				// user had previously connected. We don't validate the
				// token at startup — `slack_status` will do that on the
				// frontend's first poll, and clear the keyring entry if
				// the token has gone bad. Avoiding a blocking round-trip
				// to slack.com here keeps the splash time snappy.
				match state.slack.tokens.load() {
					Ok(Some(token)) => match SlackClient::new(token) {
						Ok(client) => state.slack.set_client(client).await,
						Err(e) => tracing::warn!(error = %e, "failed to build Slack client from stored token"),
					},
					Ok(None) => {}
					Err(e) => tracing::warn!(error = %e, "failed to read Slack token from keyring"),
				}
			});

			app.manage(state);

			tracing::info!(protocol_version = moon_protocol::PROTOCOL_VERSION, "moon-ide started");
			Ok(())
		})
		.run(tauri::generate_context!())
		.expect("error while running moon-ide");
}
