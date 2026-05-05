//! Active-folder filesystem watcher. Emits a single `fs:changed`
//! Tauri event whenever something inside the active folder
//! changes, coalescing rapid-fire events (editors rewriting temp
//! files, `cargo build` touching `target/`) into one notification
//! per window.
//!
//! Scope stays deliberately narrow for Phase 5's first fs-watch
//! slice: one recursive watcher at a time, `.git/` filtered out to
//! skip the spray of internal refs-writes a single `git commit`
//! produces. Gitignore-aware filtering, per-folder watches, and an
//! event payload carrying the changed paths are later-phase work —
//! the frontend today only needs a "re-fetch everything" nudge.
//!
//! Actor model: one tokio task owns the `notify::RecommendedWatcher`
//! and drains its callback. Another side of the same task receives
//! `SetRoot` commands from the Tauri command handlers. That keeps
//! the notify watcher entirely off the shared-state path — only the
//! `mpsc::Sender` escapes the actor, so swapping backends (Linux
//! inotify, macOS FSEvents, Windows ReadDirectoryChangesW) doesn't
//! leak past this file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant, Sleep};

/// Name of the Tauri event the frontend listens on. The payload
/// (`FsChangedPayload`) carries the workspace-relative paths that
/// changed inside the debounce window so the frontend can narrow
/// per-buffer refresh (HEAD content + working-tree reload) to the
/// files that actually moved instead of looping every open tab.
/// See `src/lib/state.svelte.ts#bindFolderChangeRefresh`.
pub const FS_CHANGED_EVENT: &str = "fs:changed";

/// Wire payload for `fs:changed`. `paths` are workspace-relative,
/// always forward-slash-separated to match `collect_paths` output.
/// Empty `paths` means "the watcher saw activity but none of it
/// resolved to a path inside the workspace root" (rare; an unwatch
/// race or a symlinked path that escapes the root) — the frontend
/// falls back to a conservative full-tab refresh in that case.
///
/// `topology_changed` is `true` when at least one event in the
/// batch was a Create / Remove / Rename — those change which
/// entries the tree should render and the frontend must re-walk
/// `collect_paths`. `false` means every event was an in-place
/// content / metadata edit on existing entries; the frontend can
/// skip the recursive walk and run only the cheap per-buffer
/// refresh. We classify conservatively: anything we can't
/// recognise (`EventKind::Any`, `EventKind::Other`) flips it true.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FsChangedPayload {
	paths: Vec<String>,
	topology_changed: bool,
}

/// Minimum gap between two consecutive `fs:changed` emits. We
/// run a leading-and-trailing debounce: the first event after an
/// idle period fires the frontend instantly (so a user-visible
/// save feels live), and any further events arriving inside the
/// window collapse into one trailing emit. The debounce also
/// caps how often we can fire during a `cargo build` / formatter
/// storm — once per `DEBOUNCE` rather than once per event.
///
/// 250ms is the sweet spot now that the frontend skips the
/// recursive `collect_paths` walk for modify-only batches: the
/// per-fire cost dropped from "tens to hundreds of ms" to a
/// single `git status --porcelain` invocation, so doubling up on
/// a `cargo build` storm doesn't melt anything. Was 500ms when
/// every fire walked the whole tree.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Public handle held by `AppState`. Cloneable; sends are
/// lock-free and non-blocking.
#[derive(Clone)]
pub struct FsWatcherHandle {
	commands: mpsc::UnboundedSender<Command>,
}

impl FsWatcherHandle {
	/// Point the watcher at `root` (replacing any previous root)
	/// or detach it when `None`. Called from `workspace_open_local`
	/// / `workspace_set_active_folder` / `workspace_remove_folder`
	/// to keep the watched tree in sync with the active folder.
	pub fn set_root(&self, root: Option<PathBuf>) {
		let _ = self.commands.send(Command::SetRoot(root));
	}
}

enum Command {
	SetRoot(Option<PathBuf>),
}

/// Spawn the actor and return its handle. Idempotent-safe in the
/// sense that callers only ever call this once at startup; the
/// actor lives for the app's lifetime.
pub fn spawn(app: AppHandle) -> FsWatcherHandle {
	let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
	tauri::async_runtime::spawn(async move {
		run(app, cmd_rx).await;
	});
	FsWatcherHandle { commands: cmd_tx }
}

async fn run(app: AppHandle, mut cmd_rx: mpsc::UnboundedReceiver<Command>) {
	let (event_tx, mut event_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

	// `notify` fires the callback on a dedicated thread. The send
	// into an unbounded tokio mpsc is non-blocking and gives the
	// actor loop a tokio-friendly receive end. Errors in the
	// callback are delivered in-band via `notify::Result` so we can
	// log and keep going without tearing the watcher down.
	let mut watcher = match RecommendedWatcher::new(
		move |res: notify::Result<Event>| {
			let _ = event_tx.send(res);
		},
		notify::Config::default(),
	) {
		Ok(w) => w,
		Err(e) => {
			// inotify exhaustion, FSEvents init failures, macOS
			// permission prompts — logged but non-fatal. The
			// frontend falls back to the window-focus refresh
			// and the palette command.
			tracing::warn!(error = %e, "failed to create fs watcher; refresh will be focus/manual-only");
			// Drain commands forever so the channel doesn't fill
			// up and leak memory; we just won't act on them.
			while cmd_rx.recv().await.is_some() {}
			return;
		}
	};

	let mut current_root: Option<PathBuf> = None;
	// Workspace-relative paths accumulated since the last emit. We
	// dedup with a `HashSet` because notify routinely fires several
	// events for the same path inside one debounce window (open,
	// write, attribute touch, close); the frontend doesn't need to
	// see each. Cleared after every successful emit. When the actor
	// resets `current_root`, any stale paths are dropped — they no
	// longer mean anything outside the workspace they came from.
	let mut pending_paths: HashSet<PathBuf> = HashSet::new();
	// Sticky-true flag: any topology event in the batch flips the
	// payload's `topologyChanged` so the frontend re-walks the
	// tree. Modify-only batches (the common Ctrl+S case) keep it
	// false and let the frontend skip `collect_paths` entirely.
	let mut pending_topology = false;
	// Wall-clock time of the most recent emit, used to gate the
	// leading edge: an event arriving more than `DEBOUNCE` after
	// the last emit is treated as a fresh burst and fires the
	// frontend immediately.
	let mut last_emit: Option<Instant> = None;
	// Trailing-edge timer: armed on the first in-window event after
	// a leading emit, fires once at `last_emit + DEBOUNCE` to flush
	// any paths accumulated during the cooldown. `None` while idle.
	let mut trailing: Option<Pin<Box<Sleep>>> = None;

	loop {
		tokio::select! {
			// Command handling takes priority over fs events so a
			// `SetRoot` that lands mid-storm swaps the watched
			// tree before the next batch of events goes through.
			biased;

			cmd = cmd_rx.recv() => {
				match cmd {
					Some(Command::SetRoot(new)) => {
						apply_set_root(&mut watcher, &mut current_root, new);
						// Stale paths from the previous root mean
						// nothing to the new one's frontend.
						pending_paths.clear();
						pending_topology = false;
						trailing = None;
						last_emit = None;
					}
					None => {
						// Sender dropped — AppState went away,
						// app is shutting down. Exit the actor.
						return;
					}
				}
			}

			Some(res) = event_rx.recv() => {
				let prev_count = pending_paths.len();
				collect_event_paths(
					&res,
					current_root.as_deref(),
					&mut pending_paths,
					&mut pending_topology,
				);
				if pending_paths.len() == prev_count {
					// Event was filtered (`.git/`, access bump,
					// out-of-root) — nothing to emit, nothing to
					// schedule.
					continue;
				}
				let now = Instant::now();
				let cooled_down = last_emit.is_none_or(|t| now.duration_since(t) >= DEBOUNCE);
				if cooled_down {
					// Leading edge: emit now, then start the
					// trailing window so any follow-up events in
					// this same save burst collapse into one
					// flush at the end.
					emit_pending(&app, &mut pending_paths, &mut pending_topology);
					last_emit = Some(now);
					trailing = None;
				} else if trailing.is_none() {
					// Inside the cooldown — accumulate. Schedule
					// the trailing flush relative to the leading
					// emit so two saves 50ms apart still fire at
					// most twice (lead + trail).
					let until = last_emit.unwrap_or(now) + DEBOUNCE;
					trailing = Some(Box::pin(sleep_until(until)));
				}
			}

			_ = poll_optional_sleep(&mut trailing) => {
				trailing = None;
				if pending_paths.is_empty() {
					continue;
				}
				emit_pending(&app, &mut pending_paths, &mut pending_topology);
				last_emit = Some(Instant::now());
			}
		}
	}
}

/// Drain `pending_paths` + `pending_topology` into a single
/// `fs:changed` emit. Pulled into a helper so the leading and
/// trailing call sites match exactly. Logs at debug on emit
/// failure (webview not attached / torn down) — the next event
/// will re-arm the watcher and we'll retry.
fn emit_pending(app: &AppHandle, paths: &mut HashSet<PathBuf>, topology: &mut bool) {
	if paths.is_empty() {
		return;
	}
	let payload = FsChangedPayload {
		paths: paths.drain().map(path_to_forward_slash).collect(),
		topology_changed: *topology,
	};
	*topology = false;
	if let Err(e) = app.emit(FS_CHANGED_EVENT, &payload) {
		tracing::debug!(error = %e, "failed to emit fs:changed");
	}
}

/// Await an optional `Sleep` inside `tokio::select!`. When the
/// option is `None` we want the branch to never resolve — the
/// classic `pending` sentinel — so the select loop blocks on the
/// other arms instead. Hand-rolled rather than using
/// `tokio::time::sleep(Duration::MAX)` because that allocates a
/// timer every iteration even when no trailing flush is armed.
async fn poll_optional_sleep(slot: &mut Option<Pin<Box<Sleep>>>) {
	match slot.as_mut() {
		Some(s) => s.as_mut().await,
		None => std::future::pending::<()>().await,
	}
}

fn apply_set_root(watcher: &mut RecommendedWatcher, current: &mut Option<PathBuf>, new: Option<PathBuf>) {
	if current.as_ref() == new.as_ref() {
		return;
	}
	if let Some(old) = current.take() {
		if let Err(e) = watcher.unwatch(&old) {
			// Unwatch failing is common when the folder was
			// unmounted or deleted out from under us. Log at
			// debug because the `watch` below is what the user
			// cares about.
			tracing::debug!(error = %e, path = %old.display(), "unwatch failed");
		}
	}
	if let Some(path) = new.clone() {
		match watcher.watch(&path, RecursiveMode::Recursive) {
			Ok(()) => {
				tracing::debug!(path = %path.display(), "fs watcher attached");
			}
			Err(e) => {
				// inotify's per-user watch limit is a realistic
				// failure on large monorepos (default 8192 on
				// many distros). The frontend still has focus
				// and palette refresh paths; the user can raise
				// `fs.inotify.max_user_watches` via sysctl if
				// this bites them.
				tracing::warn!(
					error = %e,
					path = %path.display(),
					"failed to attach fs watcher; live refresh will be unavailable for this folder"
				);
			}
		}
	}
	*current = new;
}

/// Sift one notify event into `pending`. Drops `.git/` churn
/// (a single commit writes dozens of ref / index / log files) and
/// `Access` events (read-only stat bumps from rust-analyzer,
/// tsgo, anything walking `.gitignore`) — neither moves what the
/// tree should render. Surviving paths are made workspace-relative
/// before storage; anything outside the current root is dropped
/// so we don't accidentally publish paths from a previous root
/// after a swap. Sticky-flips `topology` to `true` for any
/// Create / Remove / Rename — the frontend uses that to decide
/// whether the recursive `collect_paths` walk is needed.
fn collect_event_paths(
	res: &notify::Result<Event>,
	root: Option<&Path>,
	pending: &mut HashSet<PathBuf>,
	topology: &mut bool,
) {
	let event = match res {
		Ok(e) => e,
		Err(err) => {
			tracing::debug!(error = %err, "fs watcher error event");
			return;
		}
	};
	if matches!(event.kind, EventKind::Access(_)) {
		return;
	}
	let Some(root) = root else {
		return;
	};
	let mut took_a_path = false;
	for raw in &event.paths {
		if is_in_dotgit(raw) {
			continue;
		}
		let Ok(rel) = raw.strip_prefix(root) else {
			continue;
		};
		// `notify` sometimes reports the watched root itself for
		// directory-attribute events; the empty relative path
		// isn't useful to the frontend (it can't intersect any
		// open buffer) so we drop it.
		if rel.as_os_str().is_empty() {
			continue;
		}
		pending.insert(rel.to_path_buf());
		took_a_path = true;
	}
	// Only classify topology when at least one in-root path
	// survived filtering. Otherwise every `.git/`-only event
	// would flip the flag for nothing.
	if took_a_path && is_topology_event(&event.kind) {
		*topology = true;
	}
}

/// `true` for events that change which entries the tree should
/// render: file or directory creation, removal, rename. Plain
/// content / metadata edits stay `false`. The catch-alls
/// (`EventKind::Any`, `EventKind::Other`, and the corresponding
/// "we don't know" sub-kinds) classify as topology to err on the
/// side of correctness — a missed re-walk is a worse failure mode
/// than an unnecessary one.
fn is_topology_event(kind: &EventKind) -> bool {
	match kind {
		EventKind::Create(_) | EventKind::Remove(_) => true,
		EventKind::Modify(ModifyKind::Name(_)) => true,
		EventKind::Modify(ModifyKind::Any) => true,
		EventKind::Any | EventKind::Other => true,
		EventKind::Modify(_) => false,
		EventKind::Access(_) => false,
	}
}

/// Convert a workspace-relative path to the forward-slash string
/// shape every other moon-ide IPC uses (matches `collect_paths`'s
/// output and Pierre's path convention). On Windows this normalises
/// `\` to `/`; on Unix it's effectively a `to_string_lossy` clone.
fn path_to_forward_slash(p: PathBuf) -> String {
	let s = p.to_string_lossy();
	if std::path::MAIN_SEPARATOR != '/' {
		s.replace(std::path::MAIN_SEPARATOR, "/")
	} else {
		s.into_owned()
	}
}

fn is_in_dotgit(path: &Path) -> bool {
	path.components().any(|c| c.as_os_str() == ".git")
}
