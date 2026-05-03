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

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};

/// Name of the Tauri event the frontend listens on. No payload —
/// the frontend re-fetches `paths` and `gitStatusEntries` from
/// scratch. See `src/lib/state.svelte.ts#bindFolderChangeRefresh`.
pub const FS_CHANGED_EVENT: &str = "fs:changed";

/// How long we let events pile up before emitting one
/// `fs:changed`. Long enough to swallow a `cargo build`'s
/// per-millisecond churn without the frontend melting; short
/// enough that a deliberate edit in an external terminal feels
/// instantly reflected in the tree.
///
/// Tune: the frontend's `refreshActiveFolder` itself takes ~100ms
/// on a medium repo, so shorter windows double up refreshes.
/// Longer windows make interactive ops feel laggy.
const DEBOUNCE: Duration = Duration::from_millis(500);

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
	let mut pending = false;
	let mut tick = interval(DEBOUNCE);
	// Skip-first so the idle tick doesn't immediately emit a
	// phantom event on startup — the first meaningful tick comes
	// after a real fs change queues `pending = true`.
	tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

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
					}
					None => {
						// Sender dropped — AppState went away,
						// app is shutting down. Exit the actor.
						return;
					}
				}
			}

			Some(res) = event_rx.recv() => {
				if should_flag(&res) {
					pending = true;
				}
			}

			_ = tick.tick() => {
				if !pending {
					continue;
				}
				pending = false;
				if let Err(e) = app.emit(FS_CHANGED_EVENT, ()) {
					// Webview hasn't attached yet (early startup)
					// or has been torn down. Either way we just
					// drop this batch — the next fs change will
					// rearm `pending` and we'll retry.
					tracing::debug!(error = %e, "failed to emit fs:changed");
				}
			}
		}
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

/// Whether an event should count toward a pending refresh. We
/// swallow `.git/` churn outright because a single commit writes
/// dozens of refs/index/logs files in a burst, and every one
/// would otherwise trigger the frontend to re-scan the whole
/// tree. Other filtering (build-output directories, gitignored
/// subtrees) is left for later — the debounce keeps the frontend
/// sane enough in the meantime.
fn should_flag(res: &notify::Result<Event>) -> bool {
	let event = match res {
		Ok(e) => e,
		Err(err) => {
			tracing::debug!(error = %err, "fs watcher error event");
			return false;
		}
	};
	// `Access` events (read-only stat bumps) don't change what
	// the tree would render. Skip them so tools that `stat` every
	// file in the repo (rust-analyzer, tsgo, editors reading
	// `.gitignore`) don't keep the watcher armed.
	if matches!(event.kind, EventKind::Access(_)) {
		return false;
	}
	event.paths.iter().any(|p| !is_in_dotgit(p))
}

fn is_in_dotgit(path: &Path) -> bool {
	path.components().any(|c| c.as_os_str() == ".git")
}
