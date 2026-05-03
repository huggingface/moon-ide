//! Tauri shell for moon-ide. Wires Tauri commands to `moon-core`.

mod commands;
mod shutdown;
mod slack_poller;
mod state;
mod system_theme_watcher;

use camino::Utf8PathBuf;
use moon_core::app_state as core_app_state;
use moon_slack::{SlackClient, TokenStore};
use state::AppState;
use tauri::{Manager, RunEvent};

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
		// Persists window size / position / maximized / fullscreen
		// state to a plugin-owned JSON next to `state.json` and
		// restores it on next launch. No code on our side — the
		// plugin hooks into window creation and close events.
		.plugin(tauri_plugin_window_state::Builder::new().build())
		.invoke_handler(tauri::generate_handler![
			commands::workspace::workspace_open_local,
			commands::workspace::workspace_remove_folder,
			commands::workspace::workspace_set_active_folder,
			commands::workspace::workspace_active,
			commands::workspace::workspace_list,
			commands::fs::fs_read_dir,
			commands::fs::fs_read_file,
			commands::fs::fs_write_file,
			commands::fs::fs_stat,
			commands::fs::fs_absolute_path,
			commands::fs::fs_trash,
			commands::fs::fs_delete,
			commands::fs::fs_git_ignored_paths,
			commands::search::search_files,
			commands::search::search_content,
			commands::app_state::app_state_load,
			commands::app_state::app_state_save,
			commands::system::system_theme,
			commands::editorconfig::editorconfig_for_path,
			commands::container::container_status,
			commands::container::container_setup,
			commands::container::container_pause,
			commands::container::container_resume,
			commands::container::container_rebuild,
			commands::container::container_stop,
			commands::container::container_teardown,
			commands::container::container_apply_bound_folders,
			commands::container::container_render_compose,
			commands::project_compose::project_compose_status,
			commands::project_compose::project_compose_up,
			commands::project_compose::project_compose_pause,
			commands::project_compose::project_compose_resume,
			commands::project_compose::project_compose_rebuild,
			commands::project_compose::project_compose_stop,
			commands::project_compose::project_compose_down,
			commands::project_compose::project_compose_service_start,
			commands::project_compose::project_compose_service_stop,
			commands::project_compose::project_compose_service_restart,
			commands::compose_logs::compose_logs_open,
			commands::compose_logs::compose_logs_close,
			commands::terminal::terminal_open,
			commands::terminal::terminal_write,
			commands::terminal::terminal_resize,
			commands::terminal::terminal_close,
			commands::slack::slack_set_token,
			commands::slack::slack_status,
			commands::slack::slack_clear_token,
			commands::slack::slack_list_dm_bots,
			commands::slack::slack_select_bot,
			commands::slack::slack_clear_bot,
			commands::slack::slack_get_active_bot,
			commands::slack::slack_set_panel_visible,
			commands::slack::slack_set_window_focused,
			commands::slack::slack_list_sessions,
			commands::slack::slack_get_thread,
			commands::slack::slack_set_active_thread,
			commands::slack::slack_get_user,
			commands::slack::slack_mark_read,
			commands::slack::slack_post_message,
		])
		.setup(|app| {
			let config_dir = app
				.path()
				.app_config_dir()
				.map_err(|e| format!("could not resolve app config dir: {e}"))?;
			let config_dir =
				Utf8PathBuf::from_path_buf(config_dir).map_err(|p| format!("non-utf8 app config dir: {}", p.display()))?;

			// Per the ADR 0007 amendment, workspace state (compose.yaml +
			// bound-folders.json) lives outside any specific repo, in
			// `<XDG_DATA_HOME>/moon-ide/workspaces/<id>/`. Resolved once
			// at startup; commands compose the per-workspace directory
			// from the workspace id at call time.
			let local_data_dir =
				dirs::data_local_dir().ok_or_else(|| "could not resolve local data dir for the current platform".to_owned())?;
			let workspaces_dir = Utf8PathBuf::from_path_buf(local_data_dir)
				.map_err(|p| format!("non-utf8 local data dir: {}", p.display()))?
				.join("moon-ide")
				.join("workspaces");

			// Build the shared client cell first, spawn the Slack
			// poller against it, then hand the same Arc to AppState
			// so commands and the poller always see the same live
			// `Option<SlackClient>`.
			let client_cell = std::sync::Arc::new(tokio::sync::RwLock::new(None::<SlackClient>));
			let poller = slack_poller::spawn(
				app.handle().clone(),
				client_cell.clone(),
				TokenStore,
				config_dir.clone(),
			);
			let slack_state = state::SlackState::new(client_cell, poller.clone());
			let state = AppState::new(config_dir.clone(), workspaces_dir, slack_state);

			// Live OS colour-scheme tracking (Linux only — on macOS
			// and Windows the webview's own `onThemeChanged` fires).
			// Compiles to a no-op shim on non-Linux targets.
			system_theme_watcher::spawn(app.handle().clone());

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
						// Restore each bound folder from the persisted
						// session, in insertion order. The active folder
						// is set last so it wins over the natural
						// "newest add becomes active" behaviour. Any
						// folder whose path no longer exists is logged
						// and skipped; the post-restore frontend re-save
						// strips stale entries from disk.
						if let Some(session) = s.last_session.as_ref() {
							for folder in &session.folders {
								let path = Utf8PathBuf::from(&folder.folder_path);
								if let Err(e) = state.workspaces.add_folder(path.clone()).await {
									tracing::warn!(error = %e, path = %path, "failed to restore folder");
								}
							}
							if let Some(active) = session.active_folder_path.as_ref() {
								if let Err(e) = state.workspaces.set_active_folder(active).await {
									tracing::warn!(error = %e, path = %active, "failed to restore active folder");
								}
							}
							let snap = state.workspaces.snapshot().await;
							tracing::info!(
								folders = snap.folders.len(),
								active = ?snap.active_folder,
								"restored workspace folders"
							);
						}

						// Seed the poller from persisted Slack inputs
						// so a relaunch with the panel previously open
						// resumes polling without waiting for the
						// frontend to re-issue every setter.
						poller.set_panel_visible(s.slack.panel_visible);
						if let Some(bot) = s.slack.active_bot.as_ref() {
							poller.set_active_channel(Some(bot.dm_channel_id.clone()));
							poller.set_active_thread_ts(s.slack.active_thread_ts.clone());
						}
					}
					Err(e) => {
						tracing::warn!(error = %e, "failed to load app state");
					}
				}

				// OS focus starts true: the user just launched the
				// app, the window is in front. Frontend will correct
				// us on the next blur via `slack_set_window_focused`.
				poller.set_os_focused(true);

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

			// If the previous session ran the graceful shutdown
			// hook (the common case once the user has opted into
			// the workspace shell), bring it back up in the
			// background. The pip will track the transition from
			// `stopped` → `creating` → `running` automatically via
			// the normal status poll.
			let app_handle = app.handle().clone();
			tauri::async_runtime::spawn(async move {
				let state = app_handle.state::<AppState>();
				shutdown::auto_resume_shell(&state).await;
			});

			tracing::info!(protocol_version = moon_protocol::PROTOCOL_VERSION, "moon-ide started");
			Ok(())
		})
		.build(tauri::generate_context!())
		.expect("error while building moon-ide")
		.run(|app, event| {
			// moon-ide treats itself as the command centre for
			// every Docker project it spawned. On quit, hide the
			// window first (so the UI doesn't look frozen while
			// `compose stop` runs) then stop the workspace shell
			// and every bound-folder compose project before
			// exiting. Best-effort: any per-step failure is logged
			// but doesn't block the exit.
			if let RunEvent::ExitRequested { api, code, .. } = event {
				if code.is_some() {
					// Programmatic exit (already in our shutdown
					// path). Don't recurse.
					return;
				}
				api.prevent_exit();
				if let Some(window) = app.get_webview_window("main") {
					if let Err(err) = window.hide() {
						tracing::warn!(error = %err, "failed to hide main window during shutdown");
					}
				}
				let app_handle = app.clone();
				tauri::async_runtime::spawn(async move {
					let state = app_handle.state::<AppState>();
					shutdown::stop_all(&state).await;
					app_handle.exit(0);
				});
			}
		});
}
