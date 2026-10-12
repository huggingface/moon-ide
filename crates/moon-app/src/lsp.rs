//! LSP commands.
//!
//! Thin surface: open / update / close (fire-and-forget-ish
//! notifications) + hover / completion (requests). Diagnostics stream
//! out on the `lsp:diagnostics` event; server status transitions
//! (starting, running, unavailable, crashed, stopped) come through
//! `lsp:status`. The broker lives behind [`AppCore::lsp`] and is
//! spawned lazily on the first open call — we don't pay the TS server
//! startup cost until the user actually opens a TS/JS file.
//!
//! This command module is intentionally dumb: every error path goes
//! through `MoonError` and every parameter is forwarded to the broker
//! unchanged. "No policy" is the policy. Path validation, language-id
//! mapping, and graceful `NotAvailable` fallback all live in
//! `moon_core::lsp`.

use camino::{Utf8Path, Utf8PathBuf};
use moon_container::{Workspace as ContainerWorkspace, WorkspaceConfig};
use moon_core::lsp::server::PathTranslator;
use moon_core::lsp::{LspBroker, LspServerEvent, LspSpawner};
use moon_protocol::container::ContainerState;
use moon_protocol::lsp::{
	LspCodeAction, LspCompletionItem, LspCompletionList, LspDiagnostic, LspFileChange, LspFileChangeKind, LspHover,
	LspLocation, LspPosition, LspPrepareRename, LspRange, LspWorkspaceEdit,
};
use moon_protocol::MoonError;
use moon_terminal::{container_name_for_workspace, TerminalTarget};
use tokio::task::JoinHandle;

use crate::events::emit;
use crate::AppCore;

/// `textDocument/publishDiagnostics` fan-out. Payload is
/// `LspDiagnosticsEvent` (path + full diagnostic list). Full replacement
/// semantics: the server gives us the new truth for the whole file on
/// every publish, so the UI overwrites rather than merges.
pub const LSP_DIAGNOSTICS_EVENT: &str = "lsp:diagnostics";

/// Per-language server status transition. Payload is `LspStatusEvent`.
/// The UI keeps the latest per language and paints a status-bar pill
/// when anything but `Running` is active.
pub const LSP_STATUS_EVENT: &str = "lsp:status";

pub async fn lsp_open(app: &AppCore, path: String, language_id: String, text: String) -> Result<(), MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.open(&path, text, &language_id)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

pub async fn lsp_update(app: &AppCore, path: String, language_id: String, text: String) -> Result<(), MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.update(&path, text, &language_id)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

pub async fn lsp_close(app: &AppCore, path: String, language_id: String) -> Result<(), MoonError> {
	// Don't spawn a broker just to close a file — if we never had
	// one, there's nothing to inform.
	let broker = {
		let guard = app.lsp.lock().await;
		match guard.as_ref() {
			Some(b) => b.broker.clone(),
			None => return Ok(()),
		}
	};
	broker
		.close(&path, &language_id)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

pub async fn lsp_hover(
	app: &AppCore,
	path: String,
	language_id: String,
	position: LspPosition,
) -> Result<Option<LspHover>, MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.hover(&path, &language_id, position)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

pub async fn lsp_completion_resolve(
	app: &AppCore,
	language_id: String,
	resolve_token: String,
) -> Result<LspCompletionItem, MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.completion_resolve(&language_id, &resolve_token)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

pub async fn lsp_completion(
	app: &AppCore,
	path: String,
	language_id: String,
	position: LspPosition,
) -> Result<LspCompletionList, MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.completion(&path, &language_id, position)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// Forward an fs-watcher batch to every running language
/// server. Each server filters the entries through the globs it
/// registered via `client/registerCapability` and sends one
/// `workspace/didChangeWatchedFiles` notification with the
/// matching subset; servers that didn't register watchers
/// silently no-op. The window-focus path uses
/// [`lsp_refresh_open_diagnostics`] instead, since focus events
/// don't carry a path list.
///
/// `changes` carries a per-entry kind (created / changed /
/// deleted, from the fs-watcher's event classification) so a
/// branch-switch delete is reported as `Deleted` — servers then
/// drop their in-memory snapshot of the file instead of trying
/// to re-read it and silently keeping the stale bytes. The
/// rename / quick-fix appliers pass `changed` for every path they
/// rewrote in place.
///
/// Well-behaved servers respond to a watched-files batch by
/// invalidating per-file caches and (because we advertise
/// `workspace.diagnostics.refreshSupport`) firing a
/// `workspace/diagnostic/refresh` request back at us, which the
/// notification pump turns into a per-server
/// `refresh_open_diagnostics` call. End result: the panel's
/// diagnostics catch up to the on-disk truth without the user
/// having to retype.
///
/// No-op when no broker exists yet (the user hasn't opened any
/// LSP language file in this session): forwarding fs events to
/// zero servers is a fine idempotent.
pub async fn lsp_notify_files_changed(app: &AppCore, changes: Vec<LspFileChange>) -> Result<(), MoonError> {
	let broker = {
		let guard = app.lsp.lock().await;
		match guard.as_ref() {
			Some(b) => b.broker.clone(),
			None => return Ok(()),
		}
	};
	let changes: Vec<(String, Option<LspFileChangeKind>)> = changes.into_iter().map(|c| (c.path, Some(c.kind))).collect();
	broker.notify_files_changed(&changes).await;
	Ok(())
}

/// Trigger a diagnostic re-pull for every document currently
/// open on every running language server. Called by the
/// frontend on window-focus events (typical "I `git checkout`ed
/// in a terminal and alt-tabbed back" flow) — this covers the
/// cold-start case the fs-watcher structurally can't, since the
/// watcher only fires for events that landed *during* its
/// lifetime, and a checkout that happened before launch leaves
/// no trace.
///
/// `language_ids` scopes the refresh; empty array means "every
/// running server".
///
/// Off-disk changes that happen *while the IDE is running* are
/// handled via [`lsp_notify_files_changed`] + the server's own
/// `workspace/diagnostic/refresh` request (see the spec section
/// "Stale-diagnostics refresh on off-disk changes" in
/// `specs/lsp.md`). This command is the focus-driven safety net
/// for everything that path can't catch.
///
/// No-op when no broker exists yet.
pub async fn lsp_refresh_open_diagnostics(app: &AppCore, language_ids: Vec<String>) -> Result<(), MoonError> {
	let broker = {
		let guard = app.lsp.lock().await;
		match guard.as_ref() {
			Some(b) => b.broker.clone(),
			None => return Ok(()),
		}
	};
	let filter = if language_ids.is_empty() {
		None
	} else {
		Some(language_ids.as_slice())
	};
	broker.refresh_open_diagnostics(filter).await;
	Ok(())
}

/// Tear down the server slot for `language_id` so the next
/// `lsp_*` request lazily re-spawns it. Used by the "Restart"
/// button in the diag-logs panel and by anyone who's just
/// upgraded a global toolchain (`bun add typescript-go` etc.)
/// and wants the IDE to pick up the new binary without bouncing
/// the whole window. No-op when no broker exists yet — calling
/// restart before any LSP has spun up is a fine idempotent.
pub async fn lsp_restart(app: &AppCore, language_id: String) -> Result<(), MoonError> {
	let broker = {
		let guard = app.lsp.lock().await;
		match guard.as_ref() {
			Some(b) => b.broker.clone(),
			None => return Ok(()),
		}
	};
	broker.shutdown_language(&language_id).await;
	Ok(())
}

/// Resolve `textDocument/definition` for the symbol at `position`.
/// Returns `Ok(None)` when the server doesn't know (e.g. the cursor
/// is on whitespace, a keyword with no jump, or a literal). The UI
/// treats that as "no link underline" rather than an error.
pub async fn lsp_definition(
	app: &AppCore,
	path: String,
	language_id: String,
	position: LspPosition,
) -> Result<Option<LspLocation>, MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.definition(&path, &language_id, position)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// `textDocument/prepareRename` — server-side rename gate. `Ok(None)`
/// = "cursor not on a renameable symbol"; the F2 flow surfaces a
/// quiet flash instead of an error. `fallback_word` is the
/// frontend's `wordAt(cursor)` reading; used as the input's
/// placeholder when the server returned a bare range with no
/// placeholder (the common shape — see
/// `translate::prepare_rename_response`).
pub async fn lsp_prepare_rename(
	app: &AppCore,
	path: String,
	language_id: String,
	position: LspPosition,
	fallback_word: String,
) -> Result<Option<LspPrepareRename>, MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.prepare_rename(&path, &language_id, position, &fallback_word)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// `textDocument/rename` — server-side identifier rewrite plan.
/// Returns an empty [`LspWorkspaceEdit`] when the server has
/// nothing to change (cursor wasn't on a real symbol, server
/// doesn't implement rename); the F2 flow treats that the same
/// as a `None` from `lsp_prepare_rename`.
pub async fn lsp_rename(
	app: &AppCore,
	path: String,
	language_id: String,
	position: LspPosition,
	new_name: String,
) -> Result<LspWorkspaceEdit, MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.rename(&path, &language_id, position, &new_name)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// `textDocument/codeAction` for one diagnostic the user has
/// parked the cursor on. `producer` is the slot key that emitted
/// it (`"typescript"`, `"oxlint"`, …) — the same string the
/// frontend received with the original `lsp:diagnostics` event,
/// round-tripped so the broker can route the request to exactly
/// that server. Returns the empty list when nothing is wired up
/// for the producer or the server isn't running; the lint
/// tooltip falls back to the always-on "Fix in coder" entry.
pub async fn lsp_code_action(
	app: &AppCore,
	path: String,
	producer: String,
	range: LspRange,
	diagnostic: LspDiagnostic,
) -> Result<Vec<LspCodeAction>, MoonError> {
	let broker = ensure_broker(app).await?;
	broker
		.code_action(&path, &producer, range, diagnostic)
		.await
		.map_err(|e| MoonError::internal(e.to_string()))
}

/// Hold for the currently-active broker plus the event-pump
/// supervisor. Dropping the handle drops the broker's `Arc`; the
/// supervisor task exits when its `broadcast::Receiver` returns
/// `Closed` (all senders dropped), which happens on the broker's
/// final `Arc` drop.
pub struct LspHandle {
	pub broker: std::sync::Arc<LspBroker>,
	pub root: Utf8PathBuf,
	/// What the broker is pointing at — host, or a specific
	/// container. Cached so `ensure_broker` can notice when the
	/// container state has changed underneath us (came up, went
	/// down, or recreated with a different name) and rebuild
	/// instead of handing back a stale handle.
	pub target: BrokerTarget,
	_pump: JoinHandle<()>,
}

/// What a given broker was built against. `Container` carries the
/// compose-assigned container name so we can tell apart "same
/// project, fresh container" from "still the same container".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerTarget {
	Host,
	Container { container_name: String },
}

/// Get or create the broker for the current active folder. Spawned
/// lazily so no LSP process starts until the user actually opens a
/// file. If the active folder *or* the broker target (host vs.
/// container) has changed since last time, the old broker is shut
/// down and a new one is built.
///
/// Routing decision: when the workspace container is `Running`,
/// build a `DockerExec` spawner + `HostMount` translator so the
/// LSP runs **inside** the container and sees `/workspace/<basename>`.
/// Otherwise — container not up, no container config, or container
/// failed — fall back to the host spawner. The routing table is
/// documented in [`specs/lsp.md#container-backed-lsp`].
async fn ensure_broker(app: &AppCore) -> Result<std::sync::Arc<LspBroker>, MoonError> {
	let snap = app.workspaces.snapshot().await;
	let active = snap
		.active_folder
		.ok_or_else(|| MoonError::invalid("lsp: no active folder; open a workspace before using LSP"))?;
	let root = Utf8PathBuf::from(active);

	let target = resolve_target(
		app,
		&snap.id,
		&snap
			.folders
			.iter()
			.map(|f| Utf8PathBuf::from(&f.path))
			.collect::<Vec<_>>(),
		&root,
	)
	.await;

	let mut guard = app.lsp.lock().await;
	if let Some(existing) = guard.as_ref() {
		if existing.root == root && existing.target == target {
			return Ok(existing.broker.clone());
		}
		// Active folder or container target changed. Detach the
		// teardown to a background task so the rebuild path
		// doesn't hold `app.lsp` across `shutdown_all().await`
		// — sequential per-server with 2s shutdown + 2s child-
		// wait budgets, so synchronously waiting would gate
		// every subsequent `lsp_*` IPC behind the full teardown
		// (matching what `detach_lsp_teardown_if_root_changed`
		// already does for the folder-switch entry point).
		let old = guard.take().expect("guard.take after is_some");
		tokio::spawn(async move {
			old.broker.shutdown_all().await;
		});
	}

	let (spawner, translator) = match &target {
		BrokerTarget::Host => {
			let translator = PathTranslator::Identity {
				host_root: root.clone(),
			};
			(LspSpawner::Local, translator)
		}
		BrokerTarget::Container { container_name } => {
			// Mirrors the terminal layer's basename-under-
			// `/workspace` mount convention; falls back to
			// `/workspace` for the pathological no-basename
			// case, matching `moon-terminal`'s own fallback.
			let server_root =
				TerminalTarget::container_cwd_for_folder(&root).unwrap_or_else(|| Utf8PathBuf::from("/workspace"));
			let translator = PathTranslator::HostMount {
				host_root: root.clone(),
				server_root,
			};
			let spawner = LspSpawner::DockerExec {
				container_name: container_name.clone(),
			};
			(spawner, translator)
		}
	};

	let broker = LspBroker::new_with_spawner(root.clone(), spawner, translator, app.logs.clone());
	let mut rx = broker.subscribe();
	let events = app.events.clone();
	let pump = tokio::spawn(async move {
		loop {
			match rx.recv().await {
				Ok(LspServerEvent::Diagnostics(ev)) => {
					emit(events.as_ref(), LSP_DIAGNOSTICS_EVENT, &ev);
				}
				Ok(LspServerEvent::StatusChanged(ev)) => {
					emit(events.as_ref(), LSP_STATUS_EVENT, &ev);
				}
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					// Should be rare (256-deep channel, one UI
					// consumer). Log and keep going rather than
					// exit — the next publish replaces whatever
					// was missed.
					tracing::warn!(skipped = n, "lsp event pump lagged");
				}
				Err(tokio::sync::broadcast::error::RecvError::Closed) => {
					break;
				}
			}
		}
	});

	*guard = Some(LspHandle {
		broker: broker.clone(),
		root,
		target,
		_pump: pump,
	});
	Ok(broker)
}

/// Figure out whether the workspace container is running and
/// should host the LSP. Purely a query — doesn't start or stop
/// anything. Any failure (missing container config, docker
/// daemon unreachable, state other than `Running`) resolves to
/// `Host` rather than bubbling an error up to the `lsp_open`
/// caller: a container problem shouldn't prevent the user from
/// getting diagnostics on host-installed Rust.
///
/// `root` is the active folder the broker will serve. A running
/// container that doesn't mount it (the folder was bound after the
/// container came up — coordinator `init_repo` / `clone_repo`)
/// resolves to `Host` too: a container-spawned LSP would see no
/// files at the translated path.
async fn resolve_target(
	app: &AppCore,
	workspace_id: &str,
	bound_folders: &[Utf8PathBuf],
	root: &Utf8Path,
) -> BrokerTarget {
	let state_dir = app.workspaces_dir.join(workspace_id);
	let ws = match ContainerWorkspace::new(WorkspaceConfig {
		workspace_id: workspace_id.to_owned(),
		state_dir,
		bound_folders: bound_folders.to_vec(),
	}) {
		Ok(ws) => ws,
		Err(err) => {
			tracing::debug!(%err, "lsp: container config unavailable, using host spawner");
			return BrokerTarget::Host;
		}
	};
	match ws.status().await {
		Ok(status) if matches!(status.state, ContainerState::Running) => {
			let mount_root = app
				.workspaces
				.folder_for_path(root.as_str())
				.await
				.map(|entry| moon_core::worktree::effective_mount_root(&entry.folder).to_owned())
				.unwrap_or_else(|| root.to_string());
			let applied = ws.applied_bound_folders().await;
			if !applied.iter().any(|p| p.as_str() == mount_root) {
				tracing::debug!(%root, "lsp: folder not mounted in running container, using host spawner");
				return BrokerTarget::Host;
			}
			BrokerTarget::Container {
				container_name: container_name_for_workspace(workspace_id),
			}
		}
		Ok(_) => BrokerTarget::Host,
		Err(err) => {
			tracing::debug!(%err, "lsp: container status query failed, using host spawner");
			BrokerTarget::Host
		}
	}
}

crate::commands! {
	lsp_open([app] path: String, language_id: String, text: String),
	lsp_update([app] path: String, language_id: String, text: String),
	lsp_close([app] path: String, language_id: String),
	lsp_hover([app] path: String, language_id: String, position: LspPosition),
	lsp_completion_resolve([app] language_id: String, resolve_token: String),
	lsp_completion([app] path: String, language_id: String, position: LspPosition),
	lsp_notify_files_changed([app] changes: Vec<LspFileChange>),
	lsp_refresh_open_diagnostics([app] language_ids: Vec<String>),
	lsp_restart([app] language_id: String),
	lsp_definition([app] path: String, language_id: String, position: LspPosition),
	lsp_prepare_rename([app] path: String, language_id: String, position: LspPosition, fallback_word: String),
	lsp_rename([app] path: String, language_id: String, position: LspPosition, new_name: String),
	lsp_code_action([app] path: String, producer: String, range: LspRange, diagnostic: LspDiagnostic),
}
