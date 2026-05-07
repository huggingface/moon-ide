//! Tauri shell for moon-ide. Wires Tauri commands to `moon-core`.

mod commands;
mod fs_watcher;
mod shell_resolver;
mod shutdown;
mod slack_poller;
mod state;
mod system_theme_watcher;

use std::sync::Arc;

use camino::Utf8PathBuf;
use moon_coder::CoderHandle;
use moon_core::{app_state as core_app_state, WorkspaceRegistry};
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
			commands::fs::fs_collect_paths,
			commands::fs::fs_read_file,
			commands::fs::fs_write_file,
			commands::fs::fs_read_file_host,
			commands::fs::fs_write_file_host,
			commands::fs::fs_create_file,
			commands::fs::fs_create_dir,
			commands::fs::fs_rename,
			commands::fs::fs_stat,
			commands::fs::fs_absolute_path,
			commands::fs::fs_trash,
			commands::fs::fs_delete,
			commands::fs::fs_git_status_entries,
			commands::fs::fs_git_change_summary,
			commands::fs::fs_git_restore_paths,
			commands::fs::fs_git_blame,
			commands::fs::fs_git_head_content,
			commands::fs::fs_git_branch,
			commands::fs::fs_git_commit,
			commands::fs::fs_git_commit_on_new_branch,
			commands::fs::fs_git_push,
			commands::fs::fs_git_publish_branch,
			commands::fs::fs_git_pull,
			commands::fs::fs_git_merge_default_branch,
			commands::fs::fs_git_fetch,
			commands::fs::fs_git_head_commit_message,
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
			commands::lsp::lsp_open,
			commands::lsp::lsp_update,
			commands::lsp::lsp_close,
			commands::lsp::lsp_hover,
			commands::lsp::lsp_completion,
			commands::lsp::lsp_definition,
			commands::next_edit::next_edit_probe,
			commands::next_edit::next_edit_complete,
			commands::next_edit::next_edit_server_start,
			commands::next_edit::next_edit_server_stop,
			commands::next_edit::next_edit_server_status,
			commands::slack::slack_set_token,
			commands::slack::slack_status,
			commands::slack::slack_clear_token,
			commands::slack::slack_list_dm_bots,
			commands::slack::slack_select_bot,
			commands::slack::slack_clear_bot,
			commands::slack::slack_get_active_bot,
			commands::slack::slack_set_window_focused,
			commands::slack::slack_list_sessions,
			commands::slack::slack_get_thread,
			commands::slack::slack_set_active_thread,
			commands::slack::slack_get_user,
			commands::slack::slack_mark_read,
			commands::slack::slack_post_message,
			commands::coder::coder_status,
			commands::coder::coder_folder_summary,
			commands::coder::coder_start_device_flow,
			commands::coder::coder_poll_device_code,
			commands::coder::coder_sign_out,
			commands::coder::coder_send,
			commands::coder::coder_suggest_branch_name,
			commands::coder::coder_suggest_commit_message,
			commands::coder::coder_abort,
			commands::coder::coder_list_sessions,
			commands::coder::coder_active_session,
			commands::coder::coder_new_session,
			commands::coder::coder_open_session,
			commands::coder::coder_delete_session,
			commands::coder::coder_session_jsonl_path,
			commands::ui::ui_set_right_panel,
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
			let moon_ide_data = Utf8PathBuf::from_path_buf(local_data_dir)
				.map_err(|p| format!("non-utf8 local data dir: {}", p.display()))?
				.join("moon-ide");
			let workspaces_dir = moon_ide_data.join("workspaces");
			// Coder sessions live alongside compose state under the
			// shared `moon-ide/` data dir, organised by a slug
			// derived from each workspace folder's absolute path
			// (see `moon_coder::sessions::project_slug`). Sessions
			// are personal scratch / history rather than project
			// artefacts, so this is the right home — putting them
			// inside the project tree would put them under VCS by
			// default and tie them to the on-disk path rather than
			// the user's account.
			let coder_sessions_dir = moon_ide_data.join("coder-sessions");
			// Sibling cache for per-folder one-line descriptions
			// fed into the parent's "Bound folders" system-prompt
			// section. Same XDG root as sessions; same per-machine
			// scope; managed entirely by `moon-coder`.
			let folder_summaries_dir = moon_ide_data.join("folder-summaries");

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
			// Spawn the fs watcher before restoring workspace state
			// so the `set_root` below can point it at the active
			// folder as part of the same synchronous startup path —
			// avoids a "welcome screen with a watcher attached to
			// nothing" in-between state.
			let fs_watcher = fs_watcher::spawn(app.handle().clone());

			// The coder loop and every other command share the same
			// `WorkspaceRegistry` instance — without that, the agent
			// would dispatch tools against an empty registry and
			// every `read_file` would fail with `NoActiveFolder`.
			let workspaces = Arc::new(WorkspaceRegistry::new());

			// Plug a [`ShellResolver`] into the registry so every
			// folder's `LocalHost` can route format-on-save (and
			// any future host-issued subprocess) through the
			// workspace shell container when it's running. Same
			// routing decision the LSP and the agent's `bash` tool
			// make — deduplicating the three resolvers is a
			// follow-up, but the wire shape is identical. Held as
			// a `Weak` so dropping the registry doesn't keep the
			// resolver — and therefore the registry — alive.
			let shell_resolver = std::sync::Arc::new(shell_resolver::WorkspaceShellResolver::new(
				Arc::downgrade(&workspaces),
				workspaces_dir.clone(),
			));
			workspaces.set_shell_resolver(moon_core::ShellResolverHandle::new(shell_resolver));

			let coder = CoderHandle::new(
				workspaces.clone(),
				workspaces_dir.clone(),
				coder_sessions_dir,
				folder_summaries_dir,
			)
			.map_err(|err| format!("could not init moon-coder: {err}"))?;
			commands::coder::spawn_event_pump(app.handle().clone(), coder.clone());

			let state = AppState::new(
				config_dir.clone(),
				workspaces_dir,
				slack_state,
				fs_watcher,
				workspaces,
				coder,
			);

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
							// Point the fs watcher at the restored
							// active folder so the tree picks up
							// external edits from the first paint
							// — not just after the first folder
							// switch.
							let active_root = snap.active_folder.as_ref().map(std::path::PathBuf::from);
							state.fs_watcher.set_root(active_root);
						}

						// Seed the poller from persisted UI state so a
						// relaunch with the chat panel previously
						// active resumes polling without waiting for
						// the frontend to re-issue every setter. The
						// poller's `panel_visible` input is the
						// boolean "is *chat* the surface mounted in
						// the right slot", not "is *anything*
						// mounted" — opening the coder panel must
						// not keep slack polling.
						poller.set_panel_visible(matches!(
							s.right_panel,
							Some(moon_protocol::app_state::RightPanelKind::Chat)
						));
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
