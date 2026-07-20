//! Tauri shell for moon-ide. Wires Tauri commands to `moon-core`.
//!
//! Process model: one workspace per OS process. The CLI accepts
//! `moon-ide --workspace <slug>` to mount a specific workspace;
//! a bare `moon-ide` either re-execs into the most-recently-used
//! workspace (catalog non-empty) or boots into preboot mode
//! (catalog empty) where the only thing the UI does is collect
//! a workspace name from the user before relaunching itself with
//! `--workspace <new-slug>`.
//!
//! Critically, the launcher / focus-relay paths run **before**
//! `tauri::Builder` is touched, so a bare `moon-ide` invocation
//! that ends up handing off to a child process never creates a
//! window of its own. See
//! [specs/roadmaps/phase-07-multi-workspace.md].

mod bridge_rpc;
mod commands;
mod focus_socket;
mod fs_watcher;
mod remote_bridge;
mod shell_resolver;
mod shutdown;
mod slack_poller;
mod state;
mod system_theme_watcher;
mod window_icon;

use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;
use moon_coder::CoderHandle;
use moon_core::{app_state as core_app_state, WorkspaceRegistry};
use moon_protocol::workspace::validate_workspace_id;
use moon_slack::{SlackClient, TokenStore};
use state::{AppMode, AppState, PREBOOT_WORKSPACE_ID};
use tauri::{Manager, RunEvent};
use tokio::net::UnixListener;

/// Bundle identifier from `tauri.conf.json`. Mirrored here
/// because we need to resolve `<XDG_CONFIG_HOME>/<bundle_id>`
/// **before** Tauri's app handle exists — the launcher path
/// has to decide what to do without ever creating a window.
///
/// Single-segment on purpose. Tauri's only validation rule is
/// alphanumeric / hyphen / dot; reverse-DNS notation is a
/// recommendation, not a hard rule. Keeping it bare gives us
/// friendly `~/.config/moon-ide/` and `~/.local/share/moon-ide/`
/// paths that line up with the workspaces data dir. If we ever
/// ship to macOS / Android we'll switch to a real reverse-DNS
/// bundle ID then — see AGENTS.md "no premature migrations".
const BUNDLE_IDENTIFIER: &str = "moon-ide";

pub fn run() {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,moon=debug")),
		)
		.init();

	// Resolve mode + (optional) listener before touching Tauri.
	// The launcher / focus-relay paths return `None` here and the
	// process exits with no window ever appearing.
	let cli_workspace = parse_workspace_arg();
	let config_dir = resolve_config_dir().expect("could not resolve app config dir");
	let workspaces_dir = resolve_workspaces_dir().expect("could not resolve workspaces dir");
	let Some(boot) = bootstrap(&cli_workspace, &config_dir, &workspaces_dir) else {
		return;
	};

	// Tauri's `setup` closure isn't `FnOnce`, so we can't just
	// move the listener and `Utf8PathBuf`s in — wrap once in a
	// `Mutex<Option<…>>` and `take()` them on the first call.
	let setup_state: Arc<Mutex<Option<SetupInputs>>> = Arc::new(Mutex::new(Some(SetupInputs {
		mode: boot.mode,
		listener: boot.listener,
		config_dir,
		workspaces_dir,
	})));

	tauri::Builder::default()
		.plugin(tauri_plugin_clipboard_manager::init())
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
			commands::workspace::workspace_sync_active_worktree_branch,
			commands::workspace::workspace_active,
			commands::workspace::workspace_list,
			commands::fs::fs_read_dir,
			commands::fs::fs_collect_paths,
			commands::fs::fs_collect_paths_under,
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
			commands::fs::fs_git_exclude_path,
			commands::fs::fs_git_change_summary,
			commands::fs::fs_git_restore_paths,
			commands::fs::fs_git_add_paths,
			commands::fs::fs_git_blame,
			commands::fs::fs_git_permalink,
			commands::fs::fs_git_blob_sha,
			commands::fs::fs_publish_pr_review,
			commands::fs::fs_git_head_content,
			commands::fs::fs_git_ref_content,
			commands::fs::fs_git_default_branch_diff,
			commands::fs::fs_git_branch,
			commands::fs::fs_git_commit,
			commands::fs::fs_git_commit_on_new_branch,
			commands::fs::fs_git_worktree_add,
			commands::fs::fs_git_worktree_list,
			commands::fs::fs_git_worktree_remove,
			commands::fs::fs_git_push,
			commands::fs::fs_git_publish_branch,
			commands::fs::fs_git_pull,
			commands::fs::fs_git_merge_default_branch,
			commands::fs::fs_git_merge_state,
			commands::fs::fs_git_merge_abort,
			commands::fs::fs_git_fetch,
			commands::fs::fs_git_head_commit_message,
			commands::fs::fs_git_log,
			commands::fs::fs_git_commit_diff,
			commands::fs::fs_branch_list,
			commands::fs::fs_git_existing_pr_url,
			commands::fs::fs_branch_switch,
			commands::search::search_files,
			commands::search::search_content,
			commands::search::search_replace_content,
			commands::app_info::app_info,
			commands::app_state::app_state_load,
			commands::app_state::app_state_save,
			commands::session::session_load,
			commands::session::session_save,
			commands::workspace::workspace_catalog,
			commands::workspace::workspace_create,
			commands::workspace::workspace_delete,
			commands::workspace::workspace_rename,
			commands::workspace::workspace_set_color,
			commands::window::window_open,
			commands::window::window_close,
			commands::window::window_set_title,
			commands::system::system_theme,
			commands::editorconfig::editorconfig_for_path,
			commands::editor_forward::editor_forward_finish,
			commands::editor_forward::editor_forward_cancel,
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
			commands::ports::ports_list,
			commands::ports::ports_set,
			commands::ports::ports_status,
			commands::ports::ports_reapply,
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
			commands::lsp::lsp_completion_resolve,
			commands::lsp::lsp_definition,
			commands::lsp::lsp_prepare_rename,
			commands::lsp::lsp_rename,
			commands::lsp::lsp_code_action,
			commands::lsp::lsp_restart,
			commands::lsp::lsp_refresh_open_diagnostics,
			commands::lsp::lsp_notify_files_changed,
			commands::logs::logs_snapshot,
			commands::logs::logs_sources,
			commands::logs::logs_clear,
			commands::logs::logs_emit,
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
			commands::companion::companion_status,
			commands::companion::companion_revoke_device,
			commands::companion::companion_revoke_ide,
			commands::companion::companion_enroll,
			commands::companion::companion_remote_status,
			commands::companion::companion_remote_disconnect,
			commands::companion::companion_remote_pair_code,
			commands::companion::companion_pair_code,
			commands::coder::coder_status,
			commands::coder::coder_folder_summary,
			commands::coder::coder_start_device_flow,
			commands::coder::coder_poll_device_code,
			commands::coder::coder_sign_out,
			commands::coder::coder_send,
			commands::coder::coder_suggest_branch_name,
			commands::coder::coder_suggest_commit_message,
			commands::coder::coder_suggest_terminal_command,
			commands::coder::coder_abort,
			commands::coder::coder_drain_steer_now,
			commands::coder::coder_unqueue_steer,
			commands::coder::coder_respond_to_prompt,
			commands::coder::coder_revert_to_message,
			commands::coder::coder_replay_from_message,
			commands::coder::coder_resume_from_assistant,
			commands::coder::coder_rerun_tool_call,
			commands::coder::coder_list_sessions,
			commands::coder::coder_search_sessions,
			commands::coder::coder_active_session,
			commands::coder::coder_last_opened_session,
			commands::coder::coder_new_session,
			commands::coder::coder_new_coordinator_session,
			commands::coder::coder_new_worktree_session,
			commands::coder::coder_discard_worktree,
			commands::coder::coder_merge_and_remove_worktree,
			commands::coder::coder_associate_branch,
			commands::coder::coder_move_session_to_worktree,
			commands::coder::coder_set_bash_target_override,
			commands::coder::coder_open_session,
			commands::coder::coder_delete_session,
			commands::coder::coder_session_jsonl_path,
			commands::coder::coder_get_model_settings,
			commands::coder::coder_set_model_settings,
			commands::coder::coder_list_models,
			commands::coder::coder_list_provider_models,
			commands::coder::coder_new_provider_id,
			commands::coder::coder_probe_provider,
			commands::coder::coder_save_provider,
			commands::coder::coder_delete_provider,
			commands::coder::coder_set_provider_api_key,
			commands::coder::coder_clear_provider_api_key,
			commands::coder::coder_web_search_configured,
			commands::coder::coder_set_web_search_key,
			commands::coder::coder_clear_web_search_key,
			commands::coder::coder_mcp_servers,
			commands::coder::coder_mcp_set_enabled,
			commands::coder::coder_mcp_add_custom,
			commands::coder::coder_mcp_remove_custom,
			commands::coder::coder_hub_list_namespaces,
			commands::coder::coder_hub_get_binding,
			commands::coder::coder_hub_create_bucket,
			commands::coder::coder_hub_set_autosync,
			commands::coder::coder_hub_disconnect,
			commands::coder::coder_hub_upload_session,
			commands::coder::coder_hub_upload_all_sessions,
			commands::coder::coder_hub_session_url,
			commands::ui::ui_set_right_panel,
		])
		.setup(move |app| {
			let SetupInputs {
				mode,
				listener,
				config_dir,
				workspaces_dir,
			} = setup_state
				.lock()
				.expect("setup state mutex poisoned")
				.take()
				.expect("setup callback fired twice");

			// Coder sessions / folder summaries live alongside compose
			// state under the shared `moon-ide/` data dir.
			let moon_ide_data = workspaces_dir.parent().expect("workspaces_dir has a parent").to_owned();
			let coder_sessions_dir = moon_ide_data.join("coder-sessions");
			let folder_summaries_dir = moon_ide_data.join("folder-summaries");

			// Wire the focus listener now that we have an
			// `AppHandle`. Bound pre-Tauri to guarantee single
			// instance even if app construction takes a beat.
			// The editor registry sits behind an `Arc` so the
			// listener task and the Tauri commands that resolve
			// pending edits share one map (see
			// `crate::focus_socket::EditorRegistry`).
			let editor_registry = std::sync::Arc::new(focus_socket::EditorRegistry::new());
			// The focus listener spawn is deferred until after the
			// coder handle + workspace registry exist below, so the
			// `R` (bridge RPC) request kind can dispatch against them
			// (Phase 13). `F` (focus) / `E` (editor-forward) don't
			// need them, but there's one listener for all three.
			let deferred_focus_listener = listener;
			app.manage(std::sync::Arc::clone(&editor_registry));

			let workspace_id = match &mode {
				AppMode::Workspace { id } => id.clone(),
				AppMode::Preboot => PREBOOT_WORKSPACE_ID.to_string(),
			};

			// Bump `last_active_at` for this process's
			// workspace (skip in preboot — there's nothing in
			// the catalog yet to bump). The bump-and-load is
			// a single locked round-trip: we hand the
			// post-write snapshot back so downstream
			// `restore_session` reads exactly what just hit
			// disk, with no chance of a sibling process
			// sneaking a write in between.
			let loaded_state = if matches!(mode, AppMode::Workspace { .. }) {
				let now = std::time::SystemTime::now()
					.duration_since(std::time::UNIX_EPOCH)
					.map(|d| d.as_secs() as i64)
					.unwrap_or(0);
				let id_for_bump = workspace_id.clone();
				match tauri::async_runtime::block_on(core_app_state::mutate(&config_dir, move |s| {
					if let Some(meta) = s.workspaces.iter_mut().find(|m| m.id == id_for_bump) {
						meta.last_active_at = now;
					}
					s.clone()
				})) {
					Ok(s) => s,
					Err(e) => {
						tracing::warn!(error = %e, "failed to persist workspace catalog at boot");
						moon_protocol::app_state::AppState::default()
					}
				}
			} else {
				match tauri::async_runtime::block_on(core_app_state::load(&config_dir)) {
					Ok(s) => s,
					Err(e) => {
						tracing::warn!(error = %e, "failed to load app state at boot");
						moon_protocol::app_state::AppState::default()
					}
				}
			};

			// Per-window icon derived from the workspace id and
			// optional user override colour pulled from the
			// catalog, so an alt-tab stack of multiple `moon-ide`s
			// shows a distinct coloured badge per workspace. X11
			// honours the per-window `_NET_WM_ICON` Tauri sets
			// here; on Wayland most compositors look icons up by
			// `app_id` and ignore per-window pixmaps, so the call
			// is best-effort there. Preboot mode also gets an
			// icon (keyed on the sentinel id, no override) — same
			// code path, no branching needed. Failures are logged
			// and dropped inside `apply_workspace_icon`.
			if let Some(window) = app.get_webview_window("main") {
				let override_color = loaded_state
					.workspaces
					.iter()
					.find(|m| m.id == workspace_id)
					.and_then(|m| m.color.clone());
				window_icon::apply_workspace_icon(&window, &workspace_id, override_color.as_deref());
			}

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
			let fs_watcher = fs_watcher::spawn(app.handle().clone());

			let workspace_registry = Arc::new(WorkspaceRegistry::new(workspace_id.clone()));

			// Plug a [`ShellResolver`] into the registry so
			// every folder's `LocalHost` can route format-on-save
			// through the workspace shell container when it's
			// running. Held as a `Weak` so dropping the registry
			// doesn't keep the resolver — and therefore the
			// registry — alive.
			let shell_resolver = std::sync::Arc::new(shell_resolver::WorkspaceShellResolver::new(
				Arc::downgrade(&workspace_registry),
				workspaces_dir.clone(),
			));
			workspace_registry.set_shell_resolver(moon_core::ShellResolverHandle::new(shell_resolver));

			// Resolve the effective active provider for this
			// workspace: a per-workspace lock (in `session.json`)
			// always wins over the global default
			// (`state.json`'s `coder.active_provider`). Loading
			// the session here is cheap (one small JSON file) and
			// done before `CoderHandle::new` so the runner sees
			// the locked provider on the very first turn instead
			// of resolving against the global and then flipping
			// later.
			//
			// `restore_session` further down reads the same file
			// again for tabs / folders / SCM filters; we don't
			// pass the session through because the two callers
			// want different slices and a missing-file fallback
			// is harmless either way.
			let initial_provider_lock = if matches!(mode, AppMode::Workspace { .. }) {
				match tauri::async_runtime::block_on(moon_core::session::load(&workspaces_dir, &workspace_id)) {
					Ok(session) => session.coder_provider_lock,
					Err(err) => {
						tracing::warn!(error = %err, "could not load session for provider-lock resolution");
						None
					}
				}
			} else {
				None
			};
			let effective_active_provider = match &initial_provider_lock {
				Some(moon_protocol::coder_models::CoderProviderLock::Hf) => None,
				Some(moon_protocol::coder_models::CoderProviderLock::User { id }) => Some(id.clone()),
				None => loaded_state.coder.active_provider.clone(),
			};

			// Seed the coder with the user's persisted model picks
			// + `bill_to` + user-added providers. Empty slugs on
			// the protocol side resolve to the hardcoded defaults
			// inside `CoderModels::standard()` / `cheap()` /
			// `bill_to()`; frontend stores wire-format slugs (with
			// optional `:provider` suffix) so the runner doesn't
			// have to concatenate suffixes at request time. The
			// `has_api_key` flag on each provider is overwritten
			// inside `CoderHandle::new` from the keyring — we don't
			// trust whatever was on disk.
			let initial_coder_models = moon_coder::CoderModels {
				standard: loaded_state.coder.standard_model.clone(),
				cheap: loaded_state.coder.cheap_model.clone(),
				bill_to: if loaded_state.coder.bill_to.is_empty() {
					None
				} else {
					Some(loaded_state.coder.bill_to.clone())
				},
				providers: loaded_state.coder.providers.clone(),
				active_provider: effective_active_provider,
				context_window_overrides: std::sync::Arc::new(loaded_state.coder.context_window_overrides.clone()),
				..moon_coder::CoderModels::default()
			};
			let coder = CoderHandle::new(
				workspace_registry.clone(),
				workspaces_dir.clone(),
				coder_sessions_dir,
				folder_summaries_dir,
				initial_coder_models,
			)
			.map_err(|err| format!("could not init moon-coder: {err}"))?;
			commands::coder::spawn_event_pump(app.handle().clone(), coder.clone());
			// Best-effort prime of the per-model context-window cache for
			// the active route, so the first turn after relaunch sizes
			// the usage ring + auto-compaction trigger off authoritative
			// numbers instead of the static 128k fallback. Background
			// task on Tauri's runtime — `tokio::spawn` doesn't work here
			// because the setup hook isn't on a Tokio reactor yet; the
			// runner's own `spawn_prime_context_windows` is reserved for
			// callers that already are. Failures are logged and swallowed
			// inside `prime_context_windows`.
			let coder_for_prime = coder.clone();
			tauri::async_runtime::spawn(async move {
				coder_for_prime.prime_context_windows().await;
			});

			// Now that the coder + registry exist, spawn the focus
			// listener with a bridge-RPC handler bound to them. See
			// `crate::bridge_rpc` (Phase 13, mobile companion).
			let bridge_rpc: std::sync::Arc<dyn focus_socket::BridgeRpcHandler> =
				std::sync::Arc::new(bridge_rpc::BridgeRpc::new(coder.clone(), workspace_registry.clone()));
			// Manage the bridge_rpc in Tauri state so the remote-bridge
			// client (Phase 14.3) can reach it via `companion_enroll` —
			// forwarded calls dispatch against the same handler the focus
			// listener uses, reused unchanged.
			app.manage(std::sync::Arc::clone(&bridge_rpc));
			let focus_listener_abort = deferred_focus_listener.map(|listener| {
				focus_socket::spawn_focus_listener(
					listener,
					app.handle().clone(),
					std::sync::Arc::clone(&editor_registry),
					bridge_rpc,
				)
			});

			let logs = moon_core::LogSink::new();
			commands::logs::spawn_event_pump(app.handle().clone(), logs.clone());
			// Share the sink with every folder's `LocalHost` so
			// format-on-save (and any future host-side pipeline
			// we wire in) lands in the bottom-panel logs view
			// under source `"format-on-save"`. Same shape as the
			// shell-resolver wiring above; both have to land
			// before the first folder gets added.
			workspace_registry.set_log_sink(logs.clone());

			let state = AppState::new(
				config_dir.clone(),
				workspaces_dir.clone(),
				slack_state,
				fs_watcher,
				workspace_registry,
				coder,
				mode.clone(),
				logs,
			);
			if let Some(abort) = focus_listener_abort {
				*state.focus_listener.blocking_lock() = Some(abort);
			}

			// Live OS colour-scheme tracking (Linux only — on macOS
			// and Windows the webview's own `onThemeChanged` fires).
			system_theme_watcher::spawn(app.handle().clone());

			// Restore folder set from the per-workspace
			// session.json. Only meaningful in workspace mode;
			// preboot has nothing to restore.
			if matches!(state.mode, AppMode::Workspace { .. }) {
				tauri::async_runtime::block_on(restore_session(&state, &workspace_id, &poller, &loaded_state));
			}

			app.manage(state);

			// Ensure the mobile-companion bridge is running for this
			// machine (ADR 0024). Best-effort, detached, release-only:
			// every workspace launch fires a `moon-bridge serve` child;
			// the bridge's own owner-election means at most one survives,
			// and it self-exits when the last workspace closes. Dev
			// builds skip this — a forked child can't reach the vite dev
			// server, and the developer runs `moon-bridge serve` by hand.
			if !cfg!(debug_assertions) && matches!(mode, AppMode::Workspace { .. }) {
				// In a bundled build the bridge + PWA live under the
				// tauri resource dir (`<resource>/bridge/`); in a
				// `--no-bundle` build they sit next to the exe. Pass
				// the resolved resource dir so the helper can try it
				// first and fall back to exe-adjacent.
				let resource_bridge_dir = app
					.path()
					.resolve("bridge", tauri::path::BaseDirectory::Resource)
					.ok();
				ensure_bridge_running(resource_bridge_dir);
			}

			// Auto-resume any compose project this workspace
			// had running last time. Workspace-mode only —
			// preboot has no compose project to resume.
			//
			// Order matters: the workspace shell first (so the
			// dev container exists and is on the daemon by
			// the time `auto_resume_project_composes` issues
			// per-folder `compose up`s, which try to attach
			// it to each project network).
			if matches!(mode, AppMode::Workspace { .. }) {
				let app_handle = app.handle().clone();
				tauri::async_runtime::spawn(async move {
					let state = app_handle.state::<AppState>();
					// If we relaunched while a previous instance was
					// still tearing this workspace down, wait for that
					// teardown to finish first. Otherwise auto-resume
					// would query container state mid-`stop_all`, see
					// the containers still `Running`, and paint the pip
					// green right before the old process kills them
					// (`exited (137)`). Common case (no previous
					// teardown in flight) returns immediately.
					if let Some(id) = state.workspace_id() {
						focus_socket::await_previous_shutdown(&state.workspaces_dir, id).await;
					}
					shutdown::auto_resume_shell(&app_handle, &state).await;
					shutdown::auto_resume_project_composes(&app_handle, &state).await;
				});
			}

			tracing::info!(
				protocol_version = moon_protocol::PROTOCOL_VERSION,
				workspace_id = %workspace_id,
				"moon-ide started"
			);
			Ok(())
		})
		.build(tauri::generate_context!())
		.expect("error while building moon-ide")
		.run(|app, event| {
			// moon-ide treats itself as the command centre for
			// the workspace it owns. On quit, hide the window
			// first (so the UI doesn't look frozen while
			// `compose stop` runs) then stop the workspace shell
			// and every bound-folder compose project before
			// exiting.
			if let RunEvent::ExitRequested { api, code, .. } = event {
				if code.is_some() {
					return;
				}
				api.prevent_exit();
				for (label, window) in app.webview_windows() {
					if let Err(err) = window.hide() {
						tracing::warn!(error = %err, label = %label, "failed to hide window during shutdown");
					}
				}
				let app_handle = app.clone();
				tauri::async_runtime::spawn(async move {
					let state = app_handle.state::<AppState>();
					// Release the single-instance lock *first*. Aborting
					// the listener task drops its `UnixListener` (closing
					// the listening fd) and unlinking the socket file
					// removes the path; together they make a concurrent
					// relaunch's `probe_alive` connect fail immediately.
					// If this ran after `stop_all` instead, the listener
					// would keep accepting connections for the whole
					// (multi-second) compose/LSP teardown window and a
					// relaunch would wrongly report the workspace as still
					// in use.
					if let Some(abort) = state.focus_listener.lock().await.take() {
						abort.abort();
					}
					// Mark the workspace as tearing down *before* we drop
					// the lock and run the (multi-second) `stop_all`. A
					// relaunch that fires during this window binds the
					// freed lock successfully but waits on this sentinel
					// before auto-resuming, so it never acts on the
					// still-`Running` containers we're about to stop. See
					// `await_previous_shutdown`.
					let workspace_id = state.workspace_id().map(str::to_owned);
					if let Some(id) = workspace_id.as_deref() {
						focus_socket::write_shutdown_sentinel(&state.workspaces_dir, id).await;
						focus_socket::cleanup(&state.workspaces_dir, id).await;
					}
					shutdown::stop_all(&state).await;
					if let Some(id) = workspace_id.as_deref() {
						focus_socket::clear_shutdown_sentinel(&state.workspaces_dir, id).await;
					}
					app_handle.exit(0);
				});
			}
		});
}

/// Parse `--workspace <slug>` from the process's CLI args.
/// Anything else (positional args, unknown flags, the dev-mode
/// `--debug-config-dir` we'll likely add later) is left for
/// other parsers — we only care about the workspace target
/// here.
fn parse_workspace_arg() -> Option<String> {
	let mut args = std::env::args().skip(1);
	while let Some(arg) = args.next() {
		if let Some(value) = arg.strip_prefix("--workspace=") {
			return Some(value.to_string());
		}
		if arg == "--workspace" {
			return args.next();
		}
	}
	None
}

/// `<XDG_CONFIG_HOME>/<bundle_id>` — same path Tauri's
/// `app_config_dir()` would resolve to, computed without an
/// `AppHandle` so the launcher can read the catalog before
/// deciding whether to spawn a window.
fn resolve_config_dir() -> Result<Utf8PathBuf, String> {
	let raw = dirs::config_dir().ok_or_else(|| "could not resolve config dir for the current platform".to_owned())?;
	Utf8PathBuf::from_path_buf(raw.join(BUNDLE_IDENTIFIER))
		.map_err(|p| format!("non-utf8 app config dir: {}", p.display()))
}

/// `<XDG_DATA_HOME>/<bundle_id>/workspaces/`. ADR 0007 puts
/// per-workspace state (compose.yaml, bound-folders.json,
/// session.json, run/instance.sock) under this root; commands
/// compose the per-workspace dir from the workspace id at
/// call time. Same `<bundle_id>` segment as the config dir
/// so a wipe is `~/.config/<bundle_id>` plus
/// `~/.local/share/<bundle_id>`, no surprise third
/// directory.
/// Fire a detached `moon-bridge serve` child for the mobile
/// companion (ADR 0024). Best-effort: any failure (binary not found,
/// no companion assets, spawn error) is logged and swallowed — the
/// bridge is an optional affordance and must never block the editor
/// from launching. The bridge's own port-bind owner-election means a
/// duplicate child exits immediately, so we don't check first.
fn ensure_bridge_running(resource_bridge_dir: Option<std::path::PathBuf>) {
	let bin_name = if cfg!(windows) {
		"moon-bridge.exe"
	} else {
		"moon-bridge"
	};

	// Two source layouts hold the bridge + companion PWA:
	//   - bundled build:   `<resource>/bridge/{moon-bridge, companion/}`
	//   - --no-bundle:     `<exe-dir>/{moon-bridge, companion/}`
	// Try the resource dir first (it's the shipped artifact), then
	// fall back to exe-adjacent for the dev/team `build:bin` path.
	let mut candidates: Vec<std::path::PathBuf> = Vec::new();
	if let Some(dir) = resource_bridge_dir {
		candidates.push(dir);
	}
	if let Ok(exe) = std::env::current_exe() {
		if let Some(dir) = exe.parent() {
			candidates.push(dir.to_path_buf());
		}
	}

	let Some((src_bin, src_web)) = candidates.into_iter().find_map(|dir| {
		let bin = dir.join(bin_name);
		bin.exists().then(|| (bin, dir.join("companion")))
	}) else {
		tracing::debug!("moon-bridge binary not found in resource dir or next to exe; skipping auto-start");
		return;
	};

	// Critical for self-hosting (ADR 0005): the team rebuilds moon-ide
	// from a terminal *inside* the running IDE. Both source layouts
	// above live in the build tree (`target/release/...` or the
	// bundled `resources/...`), and `tauri-build` / the staging script
	// overwrite them on the next build. If we ran the bridge straight
	// from there, the build would hit `ETXTBSY` ("Text file busy") on
	// the running binary. So we copy the bridge + PWA to a stable
	// runtime dir *outside* the build tree and run from there — the
	// build can then freely replace the source copies.
	let Some((bridge_bin, web_root)) = stage_bridge_runtime(&src_bin, &src_web, bin_name) else {
		return;
	};
	ensure_executable(&bridge_bin);

	// Decide whether to spawn, leave the running bridge be, or evict
	// it. The contended resource is the LAN port (the owner election
	// is "first to bind"), so port-occupancy is the source of truth;
	// the control socket only tells us *which build* holds it.
	//
	// Multi-workspace-safe rule: a second window must NOT evict the
	// current bridge the first window relies on. So:
	// - port free                 -> nothing running; spawn.
	// - same build_id             -> a current bridge (maybe another
	//                                window's); leave it.
	// - different / unreachable   -> a stale build holds the port;
	//                                evict it, then spawn.
	if !bridge_port_occupied() {
		// nothing holding the port; fall through to spawn
	} else {
		match running_bridge_build_id() {
			Some(running_id) if running_id == bridge_build_id(&bridge_bin) => {
				tracing::debug!("a current-build bridge is already running; not respawning");
				return;
			}
			// Reachable but different build: graceful shutdown.
			Some(_) => evict_running_bridge(),
			// Port occupied but the control socket doesn't answer: an
			// un-talkable bridge (e.g. one built before the control
			// socket existed). It can't be current — a current bridge
			// would answer — so force it off the port.
			None => force_evict_port_holder(),
		}
	}

	let mut cmd = std::process::Command::new(&bridge_bin);
	cmd.arg("serve");
	// Point the bridge at the companion PWA if present. Without it the
	// bridge still runs (WS-only), so a missing dist is not fatal.
	if web_root.is_dir() {
		cmd.arg("--web-root").arg(&web_root);
	}

	// The bridge is meant to outlive the window that spawned it
	// (ADR 0024), so detach it into its own session: `setsid` makes it
	// a session leader with no controlling terminal, so closing the
	// IDE doesn't SIGHUP it and it isn't in the IDE's process group.
	detach_session(&mut cmd);

	match cmd.spawn() {
		Ok(child) => {
			tracing::info!(path = %bridge_bin.display(), "spawned moon-bridge serve (companion)");
			// Reap the child when it exits so a bridge that loses the
			// port-bind owner election (and exits immediately) doesn't
			// linger as a `<defunct>` zombie. A short-lived thread that
			// `wait()`s collects it; for the winning bridge the thread
			// simply parks until the bridge exits (or the IDE does, at
			// which point init adopts and reaps it).
			reap_in_background(child);
		}
		Err(err) => tracing::warn!(error = %err, "failed to spawn moon-bridge"),
	}
}

/// True if something is listening on the bridge's LAN port. Probed by
/// attempting to bind it: a refused bind means it's taken. This is the
/// same resource the bridge's owner election contends, so it's the
/// authoritative "is a bridge running" signal — more reliable than the
/// control socket, which an old bridge may lack.
fn bridge_port_occupied() -> bool {
	// Bind 0.0.0.0:53180 to mirror what the bridge binds; if it's free
	// we get the listener (and immediately drop it), if taken we error.
	std::net::TcpListener::bind(("0.0.0.0", 53180)).is_err()
}

/// Force a bridge that holds the port but doesn't answer the control
/// socket off the machine. Only reached when the port is occupied AND
/// the control socket is silent, i.e. a pre-control-socket bridge that
/// can't be asked to shut down. Sends SIGTERM to every `moon-bridge`
/// process (there should be exactly the one stale holder; a current
/// bridge would have answered the control socket and we'd never get
/// here), then waits for the port to free.
#[cfg(unix)]
fn force_evict_port_holder() {
	// SIGTERM every moon-bridge (there should be exactly the one stale
	// holder; a current bridge would have answered the control socket
	// and we'd never reach this branch). `pkill` keeps it dependency-
	// free — no `libc` crate for one signal.
	let status = std::process::Command::new("pkill")
		.arg("-x")
		.arg("moon-bridge")
		.status();
	if status.is_err() {
		tracing::warn!("could not run pkill to evict stale bridge");
		return;
	}
	for _ in 0..20 {
		if !bridge_port_occupied() {
			return;
		}
		std::thread::sleep(std::time::Duration::from_millis(100));
	}
	tracing::warn!("stale bridge didn't release the port after SIGTERM");
}

#[cfg(not(unix))]
fn force_evict_port_holder() {}

/// Query the running bridge's control socket for its `build_id`.
/// `None` means no bridge is reachable (or it's too old to report
/// one — which makes it stale by definition, so callers treat
/// `Some("")` differently from `None`). A reachable old bridge with
/// no `build_id` returns `Some(String::new())`, which won't match any
/// real staged hash, so it gets evicted — correct.
fn running_bridge_build_id() -> Option<String> {
	use std::io::{Read, Write};

	let raw = dirs::data_local_dir()?;
	let sock = raw.join(BUNDLE_IDENTIFIER).join("bridge").join("control.sock");
	let mut stream = std::os::unix::net::UnixStream::connect(&sock).ok()?;
	let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
	let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(500)));
	stream.write_all(b"{\"op\":\"status\"}\n").ok()?;
	stream.flush().ok()?;

	let mut buf = Vec::new();
	let mut tmp = [0u8; 4096];
	loop {
		let n = stream.read(&mut tmp).ok()?;
		if n == 0 {
			break;
		}
		buf.extend_from_slice(&tmp[..n]);
		if buf.contains(&b'\n') || buf.len() > 64 * 1024 {
			break;
		}
	}
	let end = buf.iter().position(|&b| b == b'\n').unwrap_or(buf.len());
	let v: serde_json::Value = serde_json::from_slice(&buf[..end]).ok()?;
	// An old bridge that doesn't report build_id yields "" — still
	// `Some`, so it's seen as a (stale) running bridge and evicted.
	Some(
		v.get("build_id")
			.and_then(|b| b.as_str())
			.unwrap_or_default()
			.to_owned(),
	)
}

/// FNV-1a 64-bit hash of the staged bridge binary — must match the
/// bridge's own `status::self_build_id` so equal builds compare equal.
fn bridge_build_id(bin: &std::path::Path) -> String {
	let Ok(bytes) = std::fs::read(bin) else {
		return String::new();
	};
	let mut hash: u64 = 0xcbf29ce484222325;
	for b in bytes {
		hash ^= b as u64;
		hash = hash.wrapping_mul(0x100000001b3);
	}
	format!("{hash:016x}")
}

/// Ask any running bridge to exit, via its control socket's
/// `shutdown` op, then wait briefly for the LAN port to free so the
/// replacement can bind. Best-effort and synchronous (runs in setup):
/// no socket / refused connect means no bridge is running, nothing to
/// do. A bridge too old to speak the control protocol won't shut down
/// here — but those predate this code and won't be in a fresh build.
fn evict_running_bridge() {
	use std::io::Write;

	let Some(raw) = dirs::data_local_dir() else {
		return;
	};
	let sock = raw.join(BUNDLE_IDENTIFIER).join("bridge").join("control.sock");
	if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) {
		let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(500)));
		let _ = stream.write_all(b"{\"op\":\"shutdown\"}\n");
		let _ = stream.flush();
	} else {
		// No live bridge to evict.
		return;
	}

	// Wait (up to ~2 s) for the port to free so our spawn can bind it.
	for _ in 0..20 {
		if !bridge_port_occupied() {
			return;
		}
		std::thread::sleep(std::time::Duration::from_millis(100));
	}
	tracing::warn!("old bridge didn't release the port in time; the new bridge may lose the election");
}

/// Put the spawned process in its own process group so it survives
/// the IDE closing and isn't signalled with the IDE's group (e.g. a
/// terminal SIGINT/SIGHUP). `process_group(0)` makes the child its
/// own group leader — std-only, no `libc` dependency.
#[cfg(unix)]
fn detach_session(cmd: &mut std::process::Command) {
	use std::os::unix::process::CommandExt;
	cmd.process_group(0);
}

#[cfg(not(unix))]
fn detach_session(_cmd: &mut std::process::Command) {}

/// Collect a child's exit status off-thread so it never zombies
/// against the IDE while the IDE is alive.
fn reap_in_background(mut child: std::process::Child) {
	std::thread::spawn(move || {
		let _ = child.wait();
	});
}

/// Copy the bridge binary + companion PWA from a build-tree source
/// into a stable runtime dir (`<data>/moon-ide/bridge/runtime/`) and
/// return the runtime `(binary, web_root)` to spawn from. Returns
/// `None` (logged) on any failure — the bridge is optional.
///
/// The binary is staged via write-temp + atomic rename so that even
/// *this* copy can't fail when a prior runtime bridge is still running
/// from the destination (e.g. relaunching the IDE before the old
/// bridge's idle-exit fired). See `ensure_bridge_running` for why the
/// runtime dir must be outside the build tree.
fn stage_bridge_runtime(
	src_bin: &std::path::Path,
	src_web: &std::path::Path,
	bin_name: &str,
) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
	let runtime = dirs::data_local_dir()?
		.join(BUNDLE_IDENTIFIER)
		.join("bridge")
		.join("runtime");
	if let Err(err) = std::fs::create_dir_all(&runtime) {
		tracing::warn!(error = %err, "could not create bridge runtime dir; skipping auto-start");
		return None;
	}

	let dest_bin = runtime.join(bin_name);
	let tmp_bin = runtime.join(format!("{bin_name}.new"));
	let _ = std::fs::remove_file(&tmp_bin);
	if let Err(err) = std::fs::copy(src_bin, &tmp_bin) {
		tracing::warn!(error = %err, "could not stage bridge binary; skipping auto-start");
		return None;
	}
	if let Err(err) = std::fs::rename(&tmp_bin, &dest_bin) {
		tracing::warn!(error = %err, "could not install bridge binary; skipping auto-start");
		return None;
	}

	// Refresh the PWA copy. A stale web dir is non-fatal (the bridge
	// runs WS-only without it), so failures here only warn.
	let dest_web = runtime.join("companion");
	if src_web.is_dir() {
		let _ = std::fs::remove_dir_all(&dest_web);
		if let Err(err) = copy_dir_recursive(src_web, &dest_web) {
			tracing::warn!(error = %err, "could not stage companion PWA; bridge will run WS-only");
		}
	}

	Some((dest_bin, dest_web))
}

/// Minimal recursive directory copy for the companion PWA (a handful
/// of small files). Avoids a dep for what `std::fs` covers.
fn copy_dir_recursive(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
	std::fs::create_dir_all(dest)?;
	for entry in std::fs::read_dir(src)? {
		let entry = entry?;
		let from = entry.path();
		let to = dest.join(entry.file_name());
		if entry.file_type()?.is_dir() {
			copy_dir_recursive(&from, &to)?;
		} else {
			std::fs::copy(&from, &to)?;
		}
	}
	Ok(())
}

#[cfg(unix)]
fn ensure_executable(path: &std::path::Path) {
	use std::os::unix::fs::PermissionsExt;
	let Ok(meta) = std::fs::metadata(path) else {
		return;
	};
	let mode = meta.permissions().mode();
	// Add the execute bit for owner/group/other if missing.
	if mode & 0o111 != 0o111 {
		let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o111));
	}
}

#[cfg(not(unix))]
fn ensure_executable(_path: &std::path::Path) {}

fn resolve_workspaces_dir() -> Result<Utf8PathBuf, String> {
	let raw =
		dirs::data_local_dir().ok_or_else(|| "could not resolve local data dir for the current platform".to_owned())?;
	let utf8 = Utf8PathBuf::from_path_buf(raw).map_err(|p| format!("non-utf8 local data dir: {}", p.display()))?;
	Ok(utf8.join(BUNDLE_IDENTIFIER).join("workspaces"))
}

/// Inputs the pre-Tauri bootstrap hands into the setup closure.
/// Wrapped in a `Mutex<Option<…>>` because Tauri's setup callback
/// isn't `FnOnce`.
struct SetupInputs {
	mode: AppMode,
	/// Bound pre-Tauri so `try_bind` failures can short-circuit
	/// the launch before any window appears. Wired into the
	/// async runtime once the app handle exists.
	listener: Option<UnixListener>,
	config_dir: Utf8PathBuf,
	workspaces_dir: Utf8PathBuf,
}

/// Output of the pre-Tauri bootstrap: the resolved mode and an
/// optional bound listener. `None` from [`bootstrap`] means
/// "this process is done — exit without spawning a window".
struct Bootstrap {
	mode: AppMode,
	listener: Option<UnixListener>,
}

/// Decide what this process should do, **before** any Tauri
/// machinery starts. Returns `None` for the launcher /
/// focus-relay paths so `run()` can `return` early and the
/// process exits without ever creating a webview window.
fn bootstrap(
	cli_workspace: &Option<String>,
	config_dir: &Utf8PathBuf,
	workspaces_dir: &Utf8PathBuf,
) -> Option<Bootstrap> {
	if let Some(slug) = cli_workspace.as_deref() {
		if let Err(err) = validate_workspace_id(slug) {
			eprintln!("moon-ide: invalid --workspace `{slug}`: {err}");
			return None;
		}
		// Try to bind the per-workspace lock socket. If a
		// sibling owns it, send a focus message and exit; if
		// the file is stale, recover and keep going; otherwise
		// take ownership.
		match tauri::async_runtime::block_on(focus_socket::try_bind(workspaces_dir, slug)) {
			Ok(listener) => Some(Bootstrap {
				mode: AppMode::Workspace { id: slug.to_string() },
				listener: Some(listener),
			}),
			Err(err) => {
				tracing::info!(slug = %slug, error = %err, "workspace already owned; sending focus");
				if let Err(send_err) = tauri::async_runtime::block_on(focus_socket::send_focus(workspaces_dir, slug)) {
					tracing::warn!(slug = %slug, error = %send_err, "failed to focus existing window");
				}
				None
			}
		}
	} else {
		// No `--workspace` arg. Look at the catalog: empty →
		// preboot; non-empty → restore the most-recently-used
		// slug. The exact path differs between dev and prod
		// (see below) but the visible end-state is the same:
		// one window, one workspace.
		let catalog = match tauri::async_runtime::block_on(core_app_state::load(config_dir)) {
			Ok(s) => s.workspaces,
			Err(err) => {
				tracing::warn!(error = %err, "failed to load catalog; treating as empty");
				Vec::new()
			}
		};
		if catalog.is_empty() {
			return Some(Bootstrap {
				mode: AppMode::Preboot,
				listener: None,
			});
		}
		let restore_slug = catalog
			.iter()
			.max_by_key(|m| m.last_active_at)
			.map(|m| m.id.clone())
			.expect("non-empty catalog");

		// Dev mode (debug build, typically running under
		// `tauri dev`): forking a child won't work because
		// vite is supervised by the parent `tauri dev`
		// process and gets torn down when this binary exits.
		// Run inline instead — the dev experience is
		// naturally one-workspace-per-`bun run dev`-session.
		if cfg!(debug_assertions) {
			tracing::info!(slug = %restore_slug, "dev mode: running most-recent workspace inline");
			return inline_workspace(workspaces_dir, &restore_slug);
		}

		// Production: spawn a child for the most-recently
		// used slug and exit before any window appears.
		let exe = match std::env::current_exe() {
			Ok(p) => p,
			Err(err) => {
				eprintln!("moon-ide: could not resolve current exe for re-exec: {err}");
				return None;
			}
		};
		match std::process::Command::new(&exe)
			.arg("--workspace")
			.arg(&restore_slug)
			.spawn()
		{
			Ok(_) => {
				tracing::info!(slug = %restore_slug, "no --workspace arg; spawned child for most recent workspace");
			}
			Err(err) => {
				eprintln!("moon-ide: failed to spawn workspace child for `{restore_slug}`: {err}");
			}
		}
		None
	}
}

/// Dev-mode inline launch: bind the slug's lock and proceed
/// in workspace mode without forking. If the lock is held
/// (somebody else's stale dev session, or a real prod sibling
/// process) we drop into preboot rather than collide — the
/// dev-mode picker can then create a fresh slug.
fn inline_workspace(workspaces_dir: &Utf8PathBuf, slug: &str) -> Option<Bootstrap> {
	match tauri::async_runtime::block_on(focus_socket::try_bind(workspaces_dir, slug)) {
		Ok(listener) => Some(Bootstrap {
			mode: AppMode::Workspace { id: slug.to_string() },
			listener: Some(listener),
		}),
		Err(err) => {
			tracing::warn!(slug = %slug, error = %err, "dev mode: workspace already locked; falling back to preboot");
			Some(Bootstrap {
				mode: AppMode::Preboot,
				listener: None,
			})
		}
	}
}

/// Restore folders + UI state for the workspace this process
/// owns. Best-effort: a missing or malformed `session.json`
/// falls back to an empty workspace; the frontend's first
/// persist tick re-saves a healthy copy.
async fn restore_session(
	state: &AppState,
	workspace_id: &str,
	poller: &slack_poller::PollerHandle,
	loaded_state: &moon_protocol::app_state::AppState,
) {
	let session = match moon_core::session::load(&state.workspaces_dir, workspace_id).await {
		Ok(s) => s,
		Err(e) => {
			tracing::warn!(error = %e, workspace_id = %workspace_id, "failed to load workspace session");
			moon_protocol::session::WorkspaceSession::default()
		}
	};
	if !session.folders.is_empty() || session.active_folder_path.is_some() {
		for folder in &session.folders {
			let path = Utf8PathBuf::from(&folder.folder_path);
			// Worktree-backed session folders (ADR 0028) re-bind as
			// nested worktree folders so their session keeps routing
			// to the checkout. The git worktree itself persisted on
			// disk, so this is just re-registering the binding. Folders
			// are stored in insertion order, so a worktree's parent is
			// always re-added before it.
			let result = match &folder.origin {
				moon_protocol::workspace::FolderOrigin::Worktree { parent_path, branch } => {
					state
						.workspaces
						.add_worktree_folder(path.clone(), parent_path.clone(), branch.clone())
						.await
				}
				moon_protocol::workspace::FolderOrigin::UserPicked => state.workspaces.add_folder(path.clone()).await,
			};
			if let Err(e) = result {
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
			workspace = %workspace_id,
			folders = snap.folders.len(),
			active = ?snap.active_folder,
			"restored workspace folders"
		);
		let active_root = snap.active_folder.as_ref().map(std::path::PathBuf::from);
		state.fs_watcher.set_root(active_root);
	}

	// Seed the poller from persisted UI state so a relaunch
	// with the chat panel previously active resumes polling
	// without waiting for the frontend to re-issue every
	// setter.
	poller.set_panel_visible(matches!(
		loaded_state.right_panel,
		Some(moon_protocol::app_state::RightPanelKind::Chat)
	));
	if let Some(bot) = loaded_state.slack.active_bot.as_ref() {
		poller.set_active_channel(Some(bot.dm_channel_id.clone()));
		poller.set_active_thread_ts(loaded_state.slack.active_thread_ts.clone());
	}

	poller.set_os_focused(true);

	// Rehydrate the Slack client from the keyring if the user
	// had previously connected. We don't validate the token at
	// startup — `slack_status` will do that on the frontend's
	// first poll, and clear the keyring entry if the token has
	// gone bad.
	match state.slack.tokens.load() {
		Ok(Some(token)) => match SlackClient::new(token) {
			Ok(client) => state.slack.set_client(client).await,
			Err(e) => tracing::warn!(error = %e, "failed to build Slack client from stored token"),
		},
		Ok(None) => {}
		Err(e) => tracing::warn!(error = %e, "failed to read Slack token from keyring"),
	}
}
