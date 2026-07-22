//! Active-folder filesystem watcher. Emits a single `fs:changed`
//! Tauri event whenever something inside the active folder
//! changes, coalescing rapid-fire events (editors rewriting temp
//! files, `cargo build` touching `target/`) into one notification
//! per window.
//!
//! ## Why we walk the tree ourselves
//!
//! notify's `RecursiveMode::Recursive` would be the obvious
//! one-liner, but its internal walker follows symlinks. In a pnpm
//! monorepo that explodes: `packages/foo/node_modules/@scope/bar`
//! is a directory symlink back into `packages/bar/`, so notify
//! registers an inotify watch on the symlinked path *first*
//! (alphabetical order of the tree walk), and from then on every
//! event for files under `packages/bar/` is reported with the
//! `node_modules/@scope/bar/...` prefix. The frontend's
//! per-buffer reload keys on the real workspace-relative path
//! (`packages/bar/src/foo.ts`) and the predicate misses, so an
//! external `git checkout` modifying `packages/bar/src/foo.ts`
//! never reloaded the open buffer.
//!
//! Manual walk with `ignore::WalkBuilder` and `follow_links(false)`
//! pins the watches to canonical paths — and gitignore-aware so we
//! don't waste inotify watches (and the user's
//! `fs.inotify.max_user_watches` budget) on `node_modules` /
//! `target/` / build output that the user's `.gitignore` already
//! says to ignore. `.git/` is excluded by `ignore` unconditionally
//! and re-added by hand: one non-recursive watch on `.git/`
//! itself (HEAD, index, MERGE_HEAD / MERGE_MSG, ORIG_HEAD,
//! FETCH_HEAD, packed-refs) plus one recursive watch on
//! `.git/refs/` so external ref moves — an agent or terminal
//! `git commit`, a `git push` updating the remote-tracking ref,
//! a `git fetch` — surface immediately instead of waiting for
//! the 3-minute auto-fetch tick. Event filtering keeps the rest
//! of the `.git/` churn (objects, logs, transient `*.lock`
//! files) away from the frontend; see `is_dotgit_observed`.
//! Linked worktrees keep their git metadata *outside* the
//! workspace root (`.git` is a file pointing at the main repo);
//! `attach` resolves the worktree's private gitdir and the
//! shared commondir, watches those too, and maps their events
//! back into a synthetic `.git/` namespace so the frontend sees
//! the same shape either way. The auto-fetch loop's HEAD-SHA
//! snapshot remains the safety net when inotify is unavailable
//! — see `runGitAutoFetch` in `state.svelte.ts`.
//!
//! Cost of the manual approach: ~one inotify watch per source
//! directory + a few hundred ms of walk on workspace open. Both
//! are well within budget for the repos this team uses, and
//! orders of magnitude smaller than the unfiltered cost. New
//! directories created at runtime get a fresh watch via the
//! `Create(Folder)` event path so `mkdir foo/ && touch foo/bar.ts`
//! starts surfacing events on `foo/bar.ts` immediately.
//!
//! ## Actor model
//!
//! One tokio task owns the `notify::RecommendedWatcher` and drains
//! its callback. Another side of the same task receives `SetRoot`
//! commands from the Tauri command handlers. That keeps the
//! notify watcher entirely off the shared-state path — only the
//! `mpsc::Sender` escapes the actor, so swapping backends (Linux
//! inotify, macOS FSEvents, Windows ReadDirectoryChangesW) doesn't
//! leak past this file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use ignore::WalkBuilder;
use notify::event::{CreateKind, ModifyKind};
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

	let mut current: Option<WatchedRoot> = None;
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
						if let Some(old) = current.take() {
							old.detach(&mut watcher);
						}
						if let Some(path) = new {
							current = Some(WatchedRoot::attach(&mut watcher, path));
						}
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
				// `Create(Folder)` for a path inside our root means
				// the user (or a tool) just made a new directory.
				// Walk it and add non-recursive watches so files
				// dropped inside immediately surface. Without this
				// step the manual-walk-non-recursive approach would
				// be blind to anything created after startup.
				if let Some(root) = current.as_mut() {
					if let Ok(event) = &res {
						if matches!(event.kind, EventKind::Create(CreateKind::Folder)) {
							for path in &event.paths {
								root.add_subtree(&mut watcher, path);
							}
						}
					}
				}
				let prev_count = pending_paths.len();
				collect_event_paths(&res, current.as_ref(), &mut pending_paths, &mut pending_topology);
				if pending_paths.len() == prev_count {
					// Event was filtered (unobserved `.git/`
					// churn, access bump, `node_modules/`,
					// out-of-root) — nothing to emit, nothing
					// to schedule.
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

/// Tracks the set of directories we've registered non-recursive
/// watches against. Owned by the actor; never shared. We keep the
/// path set so a subsequent `SetRoot` can cleanly unwatch every
/// previous registration — notify's `RecommendedWatcher::unwatch`
/// takes a path, not a watch descriptor, so we have to remember
/// every path we passed to `watch`.
struct WatchedRoot {
	root: PathBuf,
	watched_dirs: HashSet<PathBuf>,
	/// Absolute directory prefixes whose events belong to the
	/// repo's `.git/` namespace even though they live outside
	/// `root`. Empty for a regular checkout (`.git/` is under the
	/// root and strips naturally); for a linked worktree it holds
	/// the private gitdir (`<main>/.git/worktrees/<name>`) and
	/// the shared commondir (`<main>/.git`), most-specific first.
	/// `relativize` maps an event under either prefix to a
	/// synthetic `.git/<suffix>` workspace-relative path so the
	/// downstream filter and the frontend see one shape.
	dotgit_aliases: Vec<PathBuf>,
}

impl WatchedRoot {
	/// Map an absolute event path to the workspace-relative path
	/// the frontend keys on, or `None` when the event is outside
	/// both the root and every `.git` alias prefix (stale watch
	/// after a root swap, symlink escape).
	fn relativize(&self, raw: &Path) -> Option<PathBuf> {
		for alias in &self.dotgit_aliases {
			if let Ok(suffix) = raw.strip_prefix(alias) {
				return Some(Path::new(".git").join(suffix));
			}
		}
		let rel = raw.strip_prefix(&self.root).ok()?;
		if rel.as_os_str().is_empty() {
			// `notify` sometimes reports the watched root itself
			// for directory-attribute events; the empty relative
			// path isn't useful to the frontend (it can't
			// intersect any open buffer).
			return None;
		}
		Some(rel.to_path_buf())
	}

	/// Walk the workspace tree (gitignore-aware, `follow_links(false)`,
	/// excluding `node_modules` and `.git`) and register a
	/// non-recursive inotify watch per directory. Git metadata is
	/// re-watched by hand (`.git/` top level plus `.git/refs/`;
	/// gitdir + commondir for a linked worktree) so external git
	/// activity — branch switches, commits, pushes, fetches —
	/// reaches the SCM panel. See the module docs.
	fn attach(watcher: &mut RecommendedWatcher, root: PathBuf) -> Self {
		let mut watched_dirs: HashSet<PathBuf> = HashSet::new();
		// Watch the workspace root itself first so events for
		// top-level entries (Create / Remove / Rename at the
		// workspace root) aren't lost while the recursive walk is
		// still in progress.
		match watcher.watch(&root, RecursiveMode::NonRecursive) {
			Ok(()) => {
				watched_dirs.insert(root.clone());
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
					path = %root.display(),
					"failed to attach fs watcher; live refresh will be unavailable for this folder"
				);
				return WatchedRoot {
					root,
					watched_dirs,
					dotgit_aliases: Vec::new(),
				};
			}
		}
		// `.git/` would otherwise be invisible to the walker —
		// `ignore` skips it unconditionally — but the SCM panel
		// needs to hear about git metadata writes: `.git/HEAD`
		// (branch switch), `.git/index` (stage / reset), and ref
		// moves under `.git/refs/` (commit, push, fetch, branch
		// create / delete). Two watches cover it: `.git/` itself
		// non-recursively for the top-level files, and
		// `.git/refs/` recursively for the (small, symlink-free)
		// ref tree — notify's recursive walker following symlinks
		// is a non-issue there, unlike the working tree. The
		// event filter (`is_dotgit_observed`) drops the rest of
		// the churn (`objects/`, `logs/`, `*.lock`) before it can
		// reach the frontend.
		let mut dotgit_aliases: Vec<PathBuf> = Vec::new();
		let dotgit = root.join(".git");
		if dotgit.is_dir() {
			let refs = dotgit.join("refs");
			for (path, mode) in [(dotgit, RecursiveMode::NonRecursive), (refs, RecursiveMode::Recursive)] {
				match watcher.watch(&path, mode) {
					Ok(()) => {
						watched_dirs.insert(path);
					}
					Err(e) => {
						tracing::debug!(error = %e, path = %path.display(), "failed to watch git metadata dir");
					}
				}
			}
		} else if let Some((gitdir, commondir)) = resolve_worktree_git_dirs(&dotgit, &root) {
			// Linked worktree: the metadata lives in the main
			// repo. Watch the private gitdir (HEAD, index,
			// MERGE_HEAD — everything per-worktree) plus the
			// shared commondir (packed-refs) and its `refs/`
			// tree, and remember the prefixes so `relativize`
			// can fold their events back into `.git/<name>`.
			// Order matters: the gitdir lives *under* the
			// commondir, so the more specific prefix must strip
			// first — `<gitdir>/HEAD` is the worktree's own HEAD
			// and must map to `.git/HEAD`, not
			// `.git/worktrees/<name>/HEAD`.
			let refs = commondir.join("refs");
			let watches = [
				(gitdir.clone(), RecursiveMode::NonRecursive),
				(commondir.clone(), RecursiveMode::NonRecursive),
				(refs, RecursiveMode::Recursive),
			];
			for (path, mode) in watches {
				if watched_dirs.contains(&path) {
					continue;
				}
				match watcher.watch(&path, mode) {
					Ok(()) => {
						watched_dirs.insert(path);
					}
					Err(e) => {
						tracing::debug!(error = %e, path = %path.display(), "failed to watch worktree git metadata dir");
					}
				}
			}
			dotgit_aliases.push(gitdir);
			dotgit_aliases.push(commondir);
			dotgit_aliases.dedup();
		}
		let walker = WalkBuilder::new(&root)
			.follow_links(false)
			.hidden(false)
			.git_ignore(true)
			.git_global(true)
			.git_exclude(true)
			.ignore(true)
			.require_git(false)
			.filter_entry(|entry| !is_excluded_dir_entry(entry))
			.build();
		let mut walked_dirs: usize = 0;
		let mut watch_failures: usize = 0;
		for entry in walker {
			let Ok(entry) = entry else {
				continue;
			};
			let Some(ft) = entry.file_type() else {
				continue;
			};
			if !ft.is_dir() {
				continue;
			}
			let path = entry.path();
			if path == root {
				continue;
			}
			walked_dirs += 1;
			match watcher.watch(path, RecursiveMode::NonRecursive) {
				Ok(()) => {
					watched_dirs.insert(path.to_path_buf());
				}
				Err(e) => {
					watch_failures += 1;
					tracing::debug!(error = %e, path = %path.display(), "failed to watch dir");
				}
			}
		}
		tracing::debug!(
			path = %root.display(),
			dirs_walked = walked_dirs,
			watches_held = watched_dirs.len(),
			watch_failures,
			"fs watcher attached",
		);
		WatchedRoot {
			root,
			watched_dirs,
			dotgit_aliases,
		}
	}

	fn detach(self, watcher: &mut RecommendedWatcher) {
		for path in &self.watched_dirs {
			if let Err(e) = watcher.unwatch(path) {
				// Unwatch failing is common when the folder was
				// unmounted or deleted out from under us. Log at
				// debug because the next `attach` is what the
				// user cares about.
				tracing::debug!(error = %e, path = %path.display(), "unwatch failed");
			}
		}
	}

	/// Add watches for a directory subtree that didn't exist when
	/// we last walked the workspace. Called on `Create(Folder)`
	/// events so `mkdir foo/` followed by `touch foo/bar.ts`
	/// surfaces the file write without waiting for a re-attach.
	fn add_subtree(&mut self, watcher: &mut RecommendedWatcher, path: &Path) {
		if !path.starts_with(&self.root) {
			return;
		}
		if self.watched_dirs.contains(path) {
			return;
		}
		if path_has_excluded_component(path) {
			return;
		}
		let walker = WalkBuilder::new(path)
			.follow_links(false)
			.hidden(false)
			.git_ignore(true)
			.git_global(true)
			.git_exclude(true)
			.ignore(true)
			.require_git(false)
			.filter_entry(|entry| !is_excluded_dir_entry(entry))
			.build();
		for entry in walker {
			let Ok(entry) = entry else {
				continue;
			};
			let Some(ft) = entry.file_type() else {
				continue;
			};
			if !ft.is_dir() {
				continue;
			}
			let p = entry.path();
			if self.watched_dirs.contains(p) {
				continue;
			}
			match watcher.watch(p, RecursiveMode::NonRecursive) {
				Ok(()) => {
					self.watched_dirs.insert(p.to_path_buf());
				}
				Err(e) => {
					tracing::debug!(error = %e, path = %p.display(), "failed to add late watch");
				}
			}
		}
	}
}

/// `ignore`'s filter for the recursive walk. We never descend
/// into `node_modules` (pnpm symlinks aside, npm-style physical
/// `node_modules` would explode the inotify watch count on a
/// large repo) or `.git/` (which we re-watch non-recursively as
/// a single entry for `.git/HEAD` detection). Other build-output
/// dirs (`target/`, `dist/`, `.next/`) are handled by the
/// gitignore axis since the user's `.gitignore` almost always
/// covers them — staying out of speculative-hardcode territory.
fn is_excluded_dir_entry(entry: &ignore::DirEntry) -> bool {
	let Some(name) = entry.file_name().to_str() else {
		return false;
	};
	name == "node_modules" || name == ".git"
}

/// Same logic as [`is_excluded_dir_entry`] but operating on a
/// raw path. Used for the `Create(Folder)` follow-up so we don't
/// re-walk into a freshly-created `node_modules`.
fn path_has_excluded_component(path: &Path) -> bool {
	path.components().any(|c| {
		let s = c.as_os_str();
		s == "node_modules" || s == ".git"
	})
}

/// Resolve a linked worktree's git metadata directories from its
/// `.git` *file* (`gitdir: <path>` pointing at
/// `<main>/.git/worktrees/<name>`). Returns `(gitdir, commondir)`
/// canonicalised, or `None` when the root isn't a linked worktree
/// (or the pointer is stale). The commondir is where the shared
/// state lives — `refs/`, `packed-refs` — while the gitdir holds
/// the per-worktree files (`HEAD`, `index`, `MERGE_HEAD`,
/// `FETCH_HEAD`, `commondir`).
///
/// Known imprecision: the commondir's own top-level `HEAD` is the
/// *main* checkout's HEAD, and `relativize` folds it to
/// `.git/HEAD` just like the worktree's. That can trigger a
/// refresh for a branch switch that happened in the sibling
/// checkout — harmless, because every refresh re-probes actual
/// git state rather than trusting the path.
fn resolve_worktree_git_dirs(dotgit_file: &Path, root: &Path) -> Option<(PathBuf, PathBuf)> {
	if !dotgit_file.is_file() {
		return None;
	}
	let content = std::fs::read_to_string(dotgit_file).ok()?;
	let pointer = content.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
	let gitdir = if Path::new(pointer).is_absolute() {
		PathBuf::from(pointer)
	} else {
		root.join(pointer)
	};
	// Canonicalise so the registered watch paths and the event
	// paths notify reports agree byte-for-byte — the `gitdir:`
	// pointer and especially the `commondir` file (usually the
	// relative `../..`) would otherwise leave `..` components in
	// the alias prefixes and `strip_prefix` would never match.
	let gitdir = std::fs::canonicalize(&gitdir).ok()?;
	let commondir = match std::fs::read_to_string(gitdir.join("commondir")) {
		Ok(s) => {
			let s = s.trim();
			if Path::new(s).is_absolute() {
				PathBuf::from(s)
			} else {
				gitdir.join(s)
			}
		}
		Err(_) => gitdir.clone(),
	};
	let commondir = std::fs::canonicalize(&commondir).ok()?;
	Some((gitdir, commondir))
}

/// Sift one notify event into `pending`. Drops `.git/` churn
/// (a single commit writes object, log and lock files alongside
/// the ref) and `Access` events (read-only stat bumps from
/// rust-analyzer, tsgo, anything walking `.gitignore`) — neither
/// moves what the tree should render. The `.git/` paths that
/// *do* survive are the ones the SCM panel observes — see
/// `is_dotgit_observed`.
///
/// Surviving paths are made workspace-relative before storage
/// (worktree git metadata folds into the synthetic `.git/`
/// namespace via `WatchedRoot::relativize`); anything outside
/// the current root and its git aliases is dropped so we don't
/// accidentally publish paths from a previous root after a swap.
/// Sticky-flips `topology` to `true` for any Create / Remove /
/// Rename — the frontend uses that to decide whether the
/// recursive `collect_paths` walk is needed. `.git/` paths never
/// count towards topology: git metadata isn't rendered in the
/// tree, and a commit's loose-ref create / remove would
/// otherwise force a full re-walk for nothing.
fn collect_event_paths(
	res: &notify::Result<Event>,
	watched: Option<&WatchedRoot>,
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
	let Some(watched) = watched else {
		return;
	};
	let mut took_a_tree_path = false;
	for raw in &event.paths {
		let Some(rel) = watched.relativize(raw) else {
			continue;
		};
		if is_in_dotgit(&rel) {
			if is_dotgit_observed(&rel) {
				pending.insert(rel);
			}
			continue;
		}
		// Belt-and-braces. The walker excludes `node_modules` so
		// inotify shouldn't fire events for paths under it in the
		// first place — but a manually-added watch from a future
		// code path, or a symlink we missed, would otherwise leak
		// `node_modules/...` paths to the frontend's per-buffer
		// reload loop and cause `subset.has(open_file.path)`
		// mismatches.
		if path_has_excluded_component(&rel) {
			continue;
		}
		pending.insert(rel);
		took_a_tree_path = true;
	}
	if took_a_tree_path && is_topology_event(&event.kind) {
		*topology = true;
	}
}

/// `true` for the workspace-relative `.git/` paths the frontend
/// cares about:
///
///   - `.git/HEAD` — external `git switch` / `git checkout
///     <branch>` writes no working-tree files when the new
///     branch's content matches the old one's, but always
///     rewrites `.git/HEAD`.
///   - `.git/index` — commit / `git add` / `git reset --mixed`
///     rewrite the index even when no working-tree file changes.
///   - `.git/MERGE_HEAD` + `.git/MERGE_MSG` — appear when
///     `git merge` starts and disappear when it commits /
///     aborts; `refreshGitMergeState` keys off them.
///   - `.git/ORIG_HEAD` — written by merge / rebase / reset,
///     often alongside a ref move we'd want anyway.
///   - `.git/FETCH_HEAD` — an external `git fetch` may move only
///     remote-tracking refs, but it always rewrites this.
///   - `.git/packed-refs` — ref updates after a `git pack-refs`
///     land here instead of a loose ref file.
///   - anything under `.git/refs/` except git's transient
///     `*.lock` files — loose ref moves: commit, push (updates
///     the remote-tracking ref), fetch, branch create / delete.
///
/// Everything else under `.git/` (objects, logs, hooks, temp
/// lock files) is churn the frontend must never see — a single
/// commit would otherwise fan out into dozens of emits.
fn is_dotgit_observed(rel: &Path) -> bool {
	let mut components = rel.components();
	if components.next().map(|c| c.as_os_str()) != Some(std::ffi::OsStr::new(".git")) {
		return false;
	}
	let Some(second) = components.next() else {
		return false;
	};
	let second = second.as_os_str();
	if second == "refs" {
		let is_lock = rel.extension().is_some_and(|e| e == "lock");
		return !is_lock;
	}
	if components.next().is_some() {
		return false;
	}
	second == "HEAD"
		|| second == "ORIG_HEAD"
		|| second == "FETCH_HEAD"
		|| second == "MERGE_HEAD"
		|| second == "MERGE_MSG"
		|| second == "index"
		|| second == "packed-refs"
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

#[cfg(test)]
mod tests {
	use notify::event::DataChange;

	use super::*;

	fn plain_root() -> WatchedRoot {
		WatchedRoot {
			root: PathBuf::from("/ws"),
			watched_dirs: HashSet::new(),
			dotgit_aliases: Vec::new(),
		}
	}

	fn event(kind: EventKind, paths: &[&str]) -> Event {
		let mut e = Event::new(kind);
		for p in paths {
			e = e.add_path(PathBuf::from(p));
		}
		e
	}

	fn modify(paths: &[&str]) -> Event {
		event(EventKind::Modify(ModifyKind::Data(DataChange::Any)), paths)
	}

	fn collect(e: Event, watched: &WatchedRoot) -> (Vec<String>, bool) {
		let mut pending = HashSet::new();
		let mut topology = false;
		collect_event_paths(&Ok(e), Some(watched), &mut pending, &mut topology);
		let mut paths: Vec<String> = pending.into_iter().map(path_to_forward_slash).collect();
		paths.sort();
		(paths, topology)
	}

	#[test]
	fn observed_dotgit_files_survive_filtering() {
		let root = plain_root();
		for name in [
			"HEAD",
			"index",
			"MERGE_HEAD",
			"MERGE_MSG",
			"ORIG_HEAD",
			"FETCH_HEAD",
			"packed-refs",
		] {
			let raw = format!("/ws/.git/{name}");
			let (paths, topology) = collect(modify(&[&raw]), &root);
			assert_eq!(paths, vec![format!(".git/{name}")], "{name} should survive");
			assert!(!topology);
		}
	}

	#[test]
	fn ref_moves_survive_but_lock_files_do_not() {
		let (paths, _) = collect(
			modify(&[
				"/ws/.git/refs/heads/feature/x",
				"/ws/.git/refs/heads/feature/x.lock",
				"/ws/.git/refs/remotes/origin/main",
			]),
			&plain_root(),
		);
		assert_eq!(
			paths,
			vec![
				".git/refs/heads/feature/x".to_string(),
				".git/refs/remotes/origin/main".to_string()
			]
		);
	}

	#[test]
	fn dotgit_churn_is_dropped() {
		let (paths, topology) = collect(
			modify(&[
				"/ws/.git/objects/ab/cdef0123",
				"/ws/.git/logs/HEAD",
				"/ws/.git/logs/refs/heads/main",
				"/ws/.git/COMMIT_EDITMSG",
				"/ws/.git/index.lock",
				"/ws/.git/HEAD.lock",
			]),
			&plain_root(),
		);
		assert!(paths.is_empty(), "got {paths:?}");
		assert!(!topology);
	}

	#[test]
	fn dotgit_events_never_flip_topology() {
		// A commit creates / removes loose ref files; that must
		// not force the frontend's recursive tree re-walk.
		let (paths, topology) = collect(
			event(EventKind::Create(CreateKind::File), &["/ws/.git/refs/heads/main"]),
			&plain_root(),
		);
		assert_eq!(paths, vec![".git/refs/heads/main".to_string()]);
		assert!(!topology);
	}

	#[test]
	fn tree_create_flips_topology() {
		let (paths, topology) = collect(
			event(EventKind::Create(CreateKind::File), &["/ws/src/new.rs"]),
			&plain_root(),
		);
		assert_eq!(paths, vec!["src/new.rs".to_string()]);
		assert!(topology);
	}

	#[test]
	fn node_modules_and_out_of_root_paths_are_dropped() {
		let (paths, topology) = collect(
			event(
				EventKind::Create(CreateKind::File),
				&["/ws/node_modules/foo/index.js", "/elsewhere/file.txt", "/ws"],
			),
			&plain_root(),
		);
		assert!(paths.is_empty(), "got {paths:?}");
		assert!(!topology);
	}

	#[test]
	fn worktree_aliases_fold_into_synthetic_dotgit() {
		let watched = WatchedRoot {
			root: PathBuf::from("/ws/wt"),
			watched_dirs: HashSet::new(),
			// Most-specific first: the gitdir lives under the
			// commondir, mirroring `attach`'s push order.
			dotgit_aliases: vec![PathBuf::from("/main/.git/worktrees/wt"), PathBuf::from("/main/.git")],
		};
		let (paths, topology) = collect(
			modify(&[
				"/main/.git/worktrees/wt/HEAD",
				"/main/.git/worktrees/wt/index",
				"/main/.git/refs/heads/feature",
				"/main/.git/packed-refs",
				"/main/.git/objects/ab/cdef0123",
				"/main/.git/worktrees/other/HEAD",
			]),
			&watched,
		);
		assert_eq!(
			paths,
			vec![
				".git/HEAD".to_string(),
				".git/index".to_string(),
				".git/packed-refs".to_string(),
				".git/refs/heads/feature".to_string(),
			]
		);
		assert!(!topology);
	}

	#[test]
	fn nested_dotgit_outside_root_level_is_dropped() {
		let (paths, _) = collect(modify(&["/ws/vendor/dep/.git/HEAD"]), &plain_root());
		assert!(paths.is_empty(), "got {paths:?}");
	}

	#[test]
	fn resolves_gitdir_and_commondir_of_a_real_linked_worktree() {
		let tmp = tempfile::tempdir().expect("tempdir");
		let main = tmp.path().join("main");
		std::fs::create_dir(&main).expect("mkdir main");
		let git = |args: &[&str], cwd: &Path| {
			let out = std::process::Command::new("git")
				.args(args)
				.current_dir(cwd)
				.env("GIT_AUTHOR_NAME", "t")
				.env("GIT_AUTHOR_EMAIL", "t@t")
				.env("GIT_COMMITTER_NAME", "t")
				.env("GIT_COMMITTER_EMAIL", "t@t")
				.output()
				.expect("spawn git");
			assert!(
				out.status.success(),
				"git {args:?}: {}",
				String::from_utf8_lossy(&out.stderr)
			);
		};
		git(&["init", "-q", "-b", "main"], &main);
		git(&["commit", "-q", "--allow-empty", "-m", "init"], &main);
		let wt = tmp.path().join("wt");
		git(
			&[
				"worktree",
				"add",
				"-q",
				"-b",
				"feature",
				wt.to_str().expect("utf8"),
				"main",
			],
			&main,
		);

		let dotgit = wt.join(".git");
		assert!(dotgit.is_file(), "worktree .git should be a pointer file");
		let (gitdir, commondir) = resolve_worktree_git_dirs(&dotgit, &wt).expect("resolve");
		let canonical_main_git = std::fs::canonicalize(main.join(".git")).expect("canonicalize");
		assert_eq!(commondir, canonical_main_git);
		assert!(
			gitdir.starts_with(canonical_main_git.join("worktrees")),
			"gitdir {gitdir:?}"
		);
		assert!(gitdir.join("HEAD").is_file());
		assert!(commondir.join("refs").is_dir());

		// A plain checkout's `.git` directory is not a pointer.
		assert!(resolve_worktree_git_dirs(&main.join(".git"), &main).is_none());
	}
}
