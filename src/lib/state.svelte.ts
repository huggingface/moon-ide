import { convertFileSrc } from '@tauri-apps/api/core';
import { confirm, save as saveDialog } from '@tauri-apps/plugin-dialog';
import { ipc } from './ipc';
import {
	defaultEditorConfig,
	formatError,
	type AppState,
	type EditorConfig,
	type SplitSide,
	type ThemeMode,
	type Workspace,
	type WorkspaceSession,
} from './protocol';
import { slack } from './slack.svelte';
import { fingerprint, fingerprintEquals, type ContentFingerprint } from './util/hash';
import { fileKindFor, type FileKind } from './util/fileKind';
import { isMarkdownPath } from './util/markdown';

export type MarkdownView = 'source' | 'preview';

export type { SplitSide } from './protocol';

export type OpenFile = {
	// Stable identifier used as the key in `openFiles`, the per-pane tab
	// arrays, the active path fields, and the editorconfig / preview-mode
	// caches. For real files it's the workspace-relative path on disk.
	// For untitled buffers it's a synthetic `untitled:N` string — no
	// collision with real paths is possible because workspace paths
	// never start with `untitled:`. The first save flips `isUntitled`
	// to `false` and rewrites `path` to the chosen real path everywhere
	// the synthetic ID was used.
	path: string;
	name: string;
	kind: FileKind;
	// True only for buffers created via `Ctrl+N` / "New File" that have
	// never been saved. Drives the save-as flow on `Ctrl+S` and excludes
	// the buffer from session persistence — untitled state never survives
	// a restart by design.
	isUntitled: boolean;
	text: string;
	// Tauri asset:// URL for image files; empty string for text and
	// untitled buffers. Text and untitled buffers stream their contents
	// through `text`, image files render via this URL.
	previewUrl: string;
	// Fingerprint of the bytes last known to be on disk. Comparing the
	// current text's fingerprint against this lets us derive `isDirty`
	// without keeping a second full copy of the file in memory. Untitled
	// buffers seed this with the fingerprint of the empty string so the
	// first keystroke flips `isDirty`.
	loadedFingerprint: ContentFingerprint;
	loadedMtimeMs: number | null;
	isDirty: boolean;
};

class WorkspaceState {
	workspace = $state<Workspace | null>(null);
	paths = $state<string[]>([]);

	// Loaded text/image buffers, keyed by workspace-relative path. A
	// buffer is shared across panes — typing in pane A updates the same
	// `OpenFile` that pane B is rendering, so the dirty marker and text
	// stay in lockstep. A buffer is dropped from this list when it falls
	// out of every pane's tab list (`closeFile` does the GC).
	openFiles = $state<OpenFile[]>([]);

	// Per-pane tab order. The two lists are independent (VSCode/Zed
	// convention): a path can live in one pane, both, or neither, and
	// reordering on one strip never touches the other.
	leftTabs = $state<string[]>([]);
	rightTabs = $state<string[]>([]);

	// Primary and (optional) secondary editor each track their own active path.
	// Phase 1 is two-pane only; Phase 2+ can grow to N panes.
	leftActive = $state<string | null>(null);
	rightActive = $state<string | null>(null);
	hasSplit = $state(false);
	focusedSide = $state<SplitSide>('left');

	loadingPaths = $state(false);
	toast = $state<string | null>(null);

	// Per-machine UI theme. Persisted alongside the session in AppState.
	// There is no project-level theme override; if a workspace ever needs
	// one, that'd live in `.editorconfig` extensions, not here.
	theme = $state<ThemeMode>('dark');

	// Resolved `.editorconfig` per open file. Populated lazily when a
	// file is opened and refreshed when the user saves a `.editorconfig`
	// (which invalidates server-side, then we refetch every entry). Map
	// is treated as immutable for reactivity — replace the whole thing
	// on update, never mutate in place.
	editorConfigs = $state<Map<string, EditorConfig>>(new Map());

	// Source vs. rendered Preview, scoped to the buffer (not the pane:
	// each path gets one mode shared across panes — same as the buffer
	// itself). Markdown files default to Preview, every other path to
	// Source. Not persisted: `previewMode` is a UI affordance, not part
	// of the file or session.
	private previewModes = $state<Map<string, MarkdownView>>(new Map());

	// Monotonic counter the active editor view watches to refocus itself.
	// Bumped whenever the user "navigates" to a file (tab click, tree click,
	// post-close fallback). The Editor component reads it as a reactive
	// dependency and calls `view.focus()` on every change. Keeping focus
	// lives here — not in Editor.svelte — so non-editor surfaces (file tree,
	// command palette, future shortcuts) can request it uniformly.
	focusTick = $state(0);
	// Sibling tickers for the sidebar (file tree) and status bar. F6 /
	// Ctrl+0 / Esc-from-tree all just bump these and the relevant
	// component pulls focus in. Same pattern as `focusTick`; keeping
	// the tickers in WorkspaceState (rather than passing component
	// refs around) lets every region focus-shift call site stay
	// declarative.
	sidebarFocusTick = $state(0);
	statusFocusTick = $state(0);

	// Persistence guards. `persistScheduled` coalesces bursts of mutations
	// (e.g. closeFile mutates openFiles + leftActive in the same tick) into
	// a single IPC roundtrip. `suppressPersist` is set during startup
	// restore so we don't round-trip the freshly-loaded state right back
	// to disk on every `openFile` call.
	private persistScheduled = false;
	private suppressPersist = false;

	// Monotonic, per-process counter for untitled buffer IDs. Resets on
	// app start because untitled buffers don't persist; numbering is just
	// a UI affordance ("Untitled-1", "Untitled-2"…) and starting from 1
	// every launch is the expected behaviour.
	private untitledCounter = 0;

	// Bumped by `saveActiveAs` whenever it rebinds a buffer to a new path.
	// `lastRename` captures the (from, to) pair for that rebind. The editor
	// view watches the tick as a reactive dependency and consults
	// `isRename(...)` to decide whether a path change is a tab switch
	// (rebuild state) or a rename (keep state, swap language). Content
	// equality alone isn't reliable here — the pre-save pipeline can
	// touch line endings / trailing whitespace / final newline before the
	// re-read populates the buffer with bytes that differ from the live
	// view.
	renameTick = $state(0);
	private lastRename: { from: string; to: string } | null = null;

	activePath: string | null = $derived(this.focusedSide === 'left' ? this.leftActive : this.rightActive);

	activeFile: OpenFile | null = $derived.by(() => {
		if (this.activePath === null) {
			return null;
		}
		return this.openFiles.find((f) => f.path === this.activePath) ?? null;
	});

	async openLocal(path: string) {
		try {
			const ws = await ipc.workspace.openLocal(path);
			this.workspace = ws;
			this.paths = [];
			this.openFiles = [];
			this.leftTabs = [];
			this.rightTabs = [];
			this.leftActive = null;
			this.rightActive = null;
			this.hasSplit = false;
			this.focusedSide = 'left';
			await this.loadPaths();
			// Drop any tabs persisted for the previous workspace; the new
			// folder gets a clean session blob (theme is preserved).
			this.persistAppState();
		} catch (err) {
			this.flash(`Failed to open: ${formatError(err)}`);
		}
	}

	tabsFor(side: SplitSide): string[] {
		return side === 'left' ? this.leftTabs : this.rightTabs;
	}

	private setTabsFor(side: SplitSide, tabs: string[]) {
		if (side === 'left') {
			this.leftTabs = tabs;
		} else {
			this.rightTabs = tabs;
		}
	}

	/**
	 * Read the persisted AppState (theme + last session) and apply both.
	 * Theme is applied unconditionally. The session is only applied if it
	 * matches the currently-open workspace; tabs pointing at files that
	 * no longer exist are silently dropped and the cleaned-up state gets
	 * re-saved. Called once on startup from `App.svelte`.
	 */
	async restoreAppState() {
		let state: AppState;
		try {
			state = await ipc.appState.load();
		} catch (err) {
			this.flash(`Could not restore state: ${formatError(err)}`);
			return;
		}

		this.theme = state.theme;
		applyTheme(state.theme);
		// Restore the chat panel state up-front: the panel may have been
		// open at the last shutdown, in which case it mounts (and probes
		// `slack_status`) on this same paint without the user lifting a
		// finger.
		slack.hydrate(state.slack);

		const ws = this.workspace;
		const session = state.last_session;
		if (!ws || !session || session.workspace_path !== ws.root) {
			// Session is for a different workspace (or there's none). Leave
			// it on disk untouched — the next mutation will overwrite it.
			return;
		}

		this.suppressPersist = true;
		try {
			// Load each unique path exactly once; both panes can share the
			// resulting buffer.
			const unique = new Set<string>([...session.open_files_left, ...session.open_files_right]);
			const loaded: OpenFile[] = [];
			for (const path of unique) {
				try {
					const kind = fileKindFor(path);
					const file = kind === 'image' ? await this.loadImageFile(path) : await this.loadTextFile(path);
					if (file) {
						loaded.push(file);
					}
				} catch {
					// File was moved/deleted since the last session. Silently
					// drop it; the post-restore `persistAppState` writes the
					// cleaned-up list so it stops haunting future launches.
				}
			}
			const isLoaded = (p: string) => loaded.some((f) => f.path === p);
			this.openFiles = loaded;
			this.leftTabs = session.open_files_left.filter(isLoaded);
			this.rightTabs = session.open_files_right.filter(isLoaded);

			const isOpenIn = (side: SplitSide, p: string | null) =>
				p !== null && (side === 'left' ? this.leftTabs.includes(p) : this.rightTabs.includes(p));
			this.leftActive = isOpenIn('left', session.active_left) ? session.active_left : (this.leftTabs[0] ?? null);
			this.hasSplit = session.has_split && this.rightTabs.length > 0;
			this.rightActive =
				this.hasSplit && isOpenIn('right', session.active_right)
					? session.active_right
					: this.hasSplit
						? (this.rightTabs[0] ?? null)
						: null;
			this.focusedSide = session.focused_side === 'right' && this.hasSplit ? 'right' : 'left';
		} finally {
			this.suppressPersist = false;
		}
		// Re-save so dropped files don't haunt the next launch, and
		// request editor focus once the active tab is in place.
		this.persistAppState();
		if (this.activePath !== null) {
			this.requestEditorFocus();
		}
		// Warm the editorconfig cache for every restored tab so the
		// initial paint already shows the right indent settings (without
		// this, the active editor pops between defaults and the resolved
		// values on the first frame).
		await Promise.all(this.openFiles.map((f) => this.ensureEditorConfig(f.path)));
	}

	private persistAppState() {
		if (this.suppressPersist) {
			return;
		}
		if (this.persistScheduled) {
			return;
		}
		this.persistScheduled = true;
		queueMicrotask(() => {
			this.persistScheduled = false;
			if (this.suppressPersist) {
				return;
			}
			const ws = this.workspace;
			// Untitled buffers never persist: their text is in-memory only,
			// and there's no path to point session JSON at. Strip them
			// from both tab lists and from the active fields before we
			// serialise. The user-facing trade-off — `Ctrl+N`, type, quit
			// without saving = work is gone — matches every other editor.
			const realPaths = (paths: string[]) => paths.filter((p) => !isUntitledPath(p));
			const realActive = (path: string | null) => (path !== null && !isUntitledPath(path) ? path : null);
			const session: WorkspaceSession | null = ws
				? {
						workspace_path: ws.root,
						open_files_left: realPaths(this.leftTabs),
						open_files_right: this.hasSplit ? realPaths(this.rightTabs) : [],
						active_left: realActive(this.leftActive),
						active_right: this.hasSplit ? realActive(this.rightActive) : null,
						has_split: this.hasSplit,
						focused_side: this.focusedSide,
					}
				: null;
			// `slack` is owned by `slack.svelte.ts` + the Slack tauri
			// commands; the backend's `app_state_save` ignores whatever
			// we send here and preserves the on-disk value. The
			// placeholder satisfies the shared type only.
			const payload: AppState = {
				last_session: session,
				theme: this.theme,
				slack: { active_bot: null, panel_visible: false },
			};
			// AppState writes are best-effort. A toast on every failure
			// would be too noisy (this fires on every navigation); a
			// global frontend logger doesn't exist yet (and isn't worth
			// adding for one callsite). If saves systematically fail the
			// next launch's restore will simply have no data — that's
			// loud enough.
			void ipc.appState.save(payload).catch(() => {});
		});
	}

	async loadPaths() {
		if (!this.workspace) {
			return;
		}
		this.loadingPaths = true;
		try {
			const collected: string[] = [];
			await collectPaths('', collected, 0);
			this.paths = collected;
		} catch (err) {
			this.flash(`Failed to read folder: ${formatError(err)}`);
		} finally {
			this.loadingPaths = false;
		}
	}

	/**
	 * Open `path` in the given pane. By default, opening a file pulls
	 * editor focus — that's what tab clicks, the quick-open palette,
	 * and session restore want. Pass `{ focus: false }` for surfaces
	 * where the user is still navigating *around* files (most notably
	 * the file tree, where arrow-key selection should preview-open
	 * without yanking focus out of the tree); the tree separately
	 * raises focus on Enter / double-click. `focusedSide` updates
	 * either way so subsequent operations target the same pane.
	 */
	/**
	 * Open a fresh untitled buffer in `side`. Generates a synthetic
	 * `untitled:N` path so it can flow through every code path that
	 * keys on `OpenFile.path` (tab arrays, active fields, drag/drop)
	 * without special-casing. The buffer becomes active and pulls
	 * editor focus immediately — Ctrl+N is always a deliberate
	 * gesture, so no `{ focus: false }` opt-out here.
	 */
	newUntitledTab(side: SplitSide = this.focusedSide) {
		this.untitledCounter += 1;
		const n = this.untitledCounter;
		const path = `untitled:${n}`;
		const file: OpenFile = {
			path,
			name: `Untitled-${n}`,
			kind: 'text',
			isUntitled: true,
			text: '',
			previewUrl: '',
			loadedFingerprint: fingerprint(''),
			loadedMtimeMs: null,
			isDirty: false,
		};
		this.openFiles = [...this.openFiles, file];
		const tabs = this.tabsFor(side);
		this.setTabsFor(side, [...tabs, path]);
		this.setActive(path, side);
	}

	async openFile(path: string, side: SplitSide = this.focusedSide, options: { focus?: boolean } = {}) {
		const existing = this.openFiles.find((f) => f.path === path);
		if (!existing) {
			const kind = fileKindFor(path);
			try {
				const next = kind === 'image' ? await this.loadImageFile(path) : await this.loadTextFile(path);
				if (!next) {
					return;
				}
				this.openFiles = [...this.openFiles, next];
			} catch (err) {
				this.flash(`Failed to open ${path}: ${formatError(err)}`);
				return;
			}
		}
		const tabs = this.tabsFor(side);
		if (!tabs.includes(path)) {
			this.setTabsFor(side, [...tabs, path]);
		}
		this.setActive(path, side, options);
		void this.ensureEditorConfig(path);
	}

	editorConfigFor(path: string): EditorConfig {
		return this.editorConfigs.get(path) ?? defaultEditorConfig;
	}

	previewModeFor(path: string): MarkdownView {
		const stored = this.previewModes.get(path);
		if (stored) {
			return stored;
		}
		// Default markdown buffers to Preview (Cursor convention); every
		// other path renders Source. Non-markdown paths never see the
		// toggle, so the default for them is academic — pick Source so
		// flipping the toggle on by mistake does the safer thing.
		return isMarkdownPath(path) ? 'preview' : 'source';
	}

	setPreviewMode(path: string, mode: MarkdownView) {
		const current = this.previewModeFor(path);
		if (current === mode) {
			return;
		}
		const next = new Map(this.previewModes);
		next.set(path, mode);
		this.previewModes = next;
	}

	togglePreviewMode(path: string) {
		this.setPreviewMode(path, this.previewModeFor(path) === 'preview' ? 'source' : 'preview');
	}

	/**
	 * Fetch the resolved `.editorconfig` for `path` and stash it. Idempotent
	 * after the first successful call (the server caches per directory, but
	 * we still want to avoid an IPC roundtrip on every focus change).
	 * Failures are silent — the editor falls back to `defaultEditorConfig`,
	 * which is the same shape `EditorConfig::default()` produces server-side.
	 */
	async ensureEditorConfig(path: string) {
		// Untitled buffers have no on-disk path to resolve against — the
		// `.editorconfig` cascade only makes sense once a save has bound
		// the buffer to a real location. Until then the editor falls back
		// to `defaultEditorConfig`, same as it does during the first paint
		// of any tab before its IPC roundtrip resolves.
		if (isUntitledPath(path)) {
			return;
		}
		if (this.editorConfigs.has(path)) {
			return;
		}
		try {
			const ec = await ipc.editorconfig.forPath(path);
			const next = new Map(this.editorConfigs);
			next.set(path, ec);
			this.editorConfigs = next;
		} catch {
			// Leave the map untouched; subsequent calls will retry.
		}
	}

	/**
	 * Drop every cached editorconfig and refetch for currently open files.
	 * Called after a `.editorconfig` save: the server invalidated its
	 * cache during `write_file`, so the next `for_path` call returns the
	 * new rules. We refresh open files eagerly so the active editor's
	 * indent/tab handling updates without waiting for a tab switch.
	 */
	async refreshEditorConfigs() {
		this.editorConfigs = new Map();
		await Promise.all(this.openFiles.map((f) => this.ensureEditorConfig(f.path)));
	}

	private async loadTextFile(path: string): Promise<OpenFile | null> {
		const result = await ipc.fs.readFile(path);
		if (result.is_binary) {
			this.flash(`Cannot open binary file: ${path}`);
			return null;
		}
		return {
			path,
			name: basename(path),
			kind: 'text',
			isUntitled: false,
			text: result.text,
			previewUrl: '',
			loadedFingerprint: fingerprint(result.text),
			loadedMtimeMs: result.mtime_ms,
			isDirty: false,
		};
	}

	private async loadImageFile(path: string): Promise<OpenFile> {
		const absolute = await ipc.fs.absolutePath(path);
		return {
			path,
			name: basename(path),
			kind: 'image',
			isUntitled: false,
			text: '',
			previewUrl: convertFileSrc(absolute),
			loadedFingerprint: fingerprint(''),
			loadedMtimeMs: null,
			isDirty: false,
		};
	}

	/**
	 * Move `paths` (files and/or directories) to the OS trash after
	 * confirming. The default destructive action — what `Delete` in the
	 * file tree maps to. Reversible via the OS UI; the confirm exists
	 * so an accidental keypress on the wrong selection is recoverable
	 * in one dialog rather than digging through the trash bin.
	 */
	async trashPaths(paths: string[]) {
		await this.removePaths(paths, 'trash');
	}

	/**
	 * Permanently delete `paths` (files and/or directories). Reachable
	 * via `Shift+Delete` in the file tree. Bypasses the OS trash
	 * entirely; the team's recovery story for tracked files is git,
	 * untracked files are gone for good. The confirm dialog warns
	 * explicitly.
	 */
	async deletePaths(paths: string[]) {
		await this.removePaths(paths, 'delete');
	}

	/**
	 * Shared backbone for `trashPaths` and `deletePaths`. Drops
	 * descendants of selected ancestors (deleting `src/` + `src/foo.ts`
	 * is the same as deleting just `src/`), confirms with the user,
	 * dispatches to the matching IPC method per remaining path, then
	 * drops every open buffer the operation just invalidated (the
	 * paths themselves and — for directories — anything under them)
	 * and refreshes the tree. Dirty-discard prompts are intentionally
	 * skipped: the user just confirmed they want these gone, asking
	 * again per-tab would be noise.
	 *
	 * Untitled buffers in the input list are no-ops for IPC (they only
	 * live in memory) but their tabs are still closed so the keystroke
	 * isn't a silent miss when one slips into a multi-selection.
	 */
	private async removePaths(rawPaths: string[], mode: 'trash' | 'delete') {
		if (rawPaths.length === 0) {
			return;
		}
		// Pull untitled buffers aside: they need a tab close, not an
		// IPC call. Real paths get descendant-flattened so the user
		// can shift-click `src/` plus a few files inside without us
		// double-firing on already-doomed entries (and risking a
		// "no such file" error when the parent removal cleans up
		// the children first).
		const untitled = rawPaths.filter((p) => isUntitledPath(p));
		const real = dropDescendantPaths(rawPaths.filter((p) => !isUntitledPath(p)));
		if (real.length === 0 && untitled.length === 0) {
			return;
		}

		// Display strings strip the trailing slash dir convention so
		// the dialog and toast read the way the user expects.
		const displays = real.map((p) => (p.endsWith('/') ? p.replace(/\/$/, '') : p));
		const total = displays.length;
		const message = buildRemovalMessage(displays, real, mode);

		if (total > 0) {
			const ok = await confirm(message, {
				title: mode === 'trash' ? 'Move to trash' : 'Permanently delete',
				okLabel: mode === 'trash' ? 'Move to trash' : 'Delete',
				cancelLabel: 'Cancel',
			});
			if (!ok) {
				return;
			}
		}

		// Run IPC calls in parallel; collect failures so a single bad
		// path (locked file, permission denied) doesn't drop the rest.
		// We still tear down the in-memory state below for paths that
		// _did_ go through, so the editor stays consistent with disk.
		const ipcCall = mode === 'trash' ? ipc.fs.trash : ipc.fs.delete;
		const results = await Promise.allSettled(displays.map((p) => ipcCall(p)));
		const removed: string[] = [];
		const failures: { path: string; err: unknown }[] = [];
		results.forEach((r, i) => {
			const p = displays[i] ?? '';
			if (r.status === 'fulfilled') {
				removed.push(p);
			} else {
				failures.push({ path: p, err: r.reason });
			}
		});

		// Build a single "was this path removed" predicate covering
		// every successfully-removed real path *and* the untitled
		// buffers we're closing. Dir removals nuke everything
		// underneath; we match by path prefix with the trailing slash
		// so e.g. removing `src/` doesn't accidentally drop `src.ts`
		// from the editor.
		const removedSet = new Set<string>(removed);
		for (const u of untitled) {
			removedSet.add(u);
		}
		const dirPrefixes = real
			.map((p, i) => ({ p, display: displays[i] ?? '' }))
			.filter(({ p }) => p.endsWith('/'))
			.filter(({ display }) => removedSet.has(display))
			.map(({ display }) => display + '/');
		const wasRemoved = (p: string) => removedSet.has(p) || dirPrefixes.some((pre) => p.startsWith(pre));

		this.openFiles = this.openFiles.filter((f) => !wasRemoved(f.path));
		this.leftTabs = this.leftTabs.filter((p) => !wasRemoved(p));
		this.rightTabs = this.rightTabs.filter((p) => !wasRemoved(p));
		if (this.leftActive !== null && wasRemoved(this.leftActive)) {
			this.leftActive = this.leftTabs[this.leftTabs.length - 1] ?? null;
		}
		if (this.rightActive !== null && wasRemoved(this.rightActive)) {
			this.rightActive = this.rightTabs[this.rightTabs.length - 1] ?? null;
		}
		// Drop preview-mode + editorconfig entries for the removed paths
		// so a future file at the same path starts with default modes.
		let modesChanged = false;
		const modes = new Map(this.previewModes);
		for (const key of modes.keys()) {
			if (wasRemoved(key)) {
				modes.delete(key);
				modesChanged = true;
			}
		}
		if (modesChanged) {
			this.previewModes = modes;
		}
		let ecsChanged = false;
		const ecs = new Map(this.editorConfigs);
		for (const key of ecs.keys()) {
			if (wasRemoved(key)) {
				ecs.delete(key);
				ecsChanged = true;
			}
		}
		if (ecsChanged) {
			this.editorConfigs = ecs;
		}

		if (failures.length > 0) {
			// One toast covers the lot. We don't list every failure
			// because there's nowhere good to put a multi-line error
			// in the current UI; for now the count + first reason is
			// enough to debug. Promote to a problems-panel entry in
			// Phase 8 when one exists.
			const first = failures[0];
			const prefix = mode === 'trash' ? 'Move to trash' : 'Delete';
			const reason = first ? formatError(first.err) : 'unknown error';
			this.flash(`${prefix} failed for ${failures.length} of ${displays.length}: ${reason}`);
		}

		await this.loadPaths();
		this.persistAppState();
	}

	async closeFile(path: string, side: SplitSide = this.focusedSide) {
		const file = this.openFiles.find((f) => f.path === path);
		if (!file) {
			return;
		}
		const otherSide: SplitSide = side === 'left' ? 'right' : 'left';
		const otherHasIt = this.tabsFor(otherSide).includes(path);

		// Dirty prompt only fires when this is the last copy: if pane B
		// still has the buffer open, closing pane A's tab discards
		// nothing (the buffer stays alive and reachable from B).
		if (file.isDirty && !otherHasIt) {
			// 2-button native dialog. We don't (yet) offer "Save" here:
			// Ctrl+S already saves, and a 3-way dialog would need a custom
			// in-app modal (`@tauri-apps/plugin-dialog` is OK/Cancel only).
			// If anyone wants the 3rd button we can build the modal then.
			const ok = await confirm(`${file.name} has unsaved changes. Discard them?`, {
				title: 'Unsaved changes',
				okLabel: 'Discard',
				cancelLabel: 'Cancel',
			});
			if (!ok) {
				return;
			}
		}

		const tabs = this.tabsFor(side);
		const idx = tabs.indexOf(path);
		if (idx < 0) {
			return;
		}
		const remaining = tabs.filter((p) => p !== path);
		this.setTabsFor(side, remaining);

		const fallback = remaining[Math.max(0, idx - 1)] ?? null;
		if (side === 'left' && this.leftActive === path) {
			this.leftActive = fallback;
		}
		if (side === 'right' && this.rightActive === path) {
			this.rightActive = fallback;
		}

		// GC the buffer when no pane references it anymore. Keeping
		// stale entries in `openFiles` would leak the loaded text and
		// break the "re-click an already-open file" flow (the existing
		// entry would short-circuit the load).
		if (!this.leftTabs.includes(path) && !this.rightTabs.includes(path)) {
			this.openFiles = this.openFiles.filter((f) => f.path !== path);
			if (this.previewModes.has(path)) {
				const next = new Map(this.previewModes);
				next.delete(path);
				this.previewModes = next;
			}
		}

		if (fallback !== null) {
			this.requestEditorFocus();
		}
		this.persistAppState();
	}

	/**
	 * Move a tab on `side` so it sits immediately before `beforePath`,
	 * or to the end of the strip if `beforePath` is null. Reordering is
	 * scoped to the pane. For cross-pane moves use `moveTab`.
	 */
	moveFile(fromPath: string, beforePath: string | null, side: SplitSide = this.focusedSide) {
		if (beforePath === fromPath) {
			return;
		}
		const tabs = this.tabsFor(side);
		if (!tabs.includes(fromPath)) {
			return;
		}
		const without = tabs.filter((p) => p !== fromPath);
		if (beforePath === null) {
			this.setTabsFor(side, [...without, fromPath]);
			this.persistAppState();
			return;
		}
		const beforeIdx = without.indexOf(beforePath);
		if (beforeIdx < 0) {
			this.setTabsFor(side, [...without, fromPath]);
			this.persistAppState();
			return;
		}
		this.setTabsFor(side, [...without.slice(0, beforeIdx), fromPath, ...without.slice(beforeIdx)]);
		this.persistAppState();
	}

	/**
	 * Move a tab between panes (or reorder within one pane when
	 * `fromSide === toSide`). Buffer is shared so we never reload —
	 * just shuffle the path between the per-pane tab lists. After the
	 * move, the dragged tab becomes active on the destination side and
	 * focus follows (VSCode convention).
	 *
	 * If the destination pane already had `fromPath` open, we treat the
	 * drop as "consolidate here": the tab is removed from the source
	 * and the destination keeps its existing copy at the drop position
	 * (or untouched if dropping on itself).
	 */
	moveTab(fromPath: string, fromSide: SplitSide, toSide: SplitSide, beforePath: string | null) {
		if (fromSide === toSide) {
			this.moveFile(fromPath, beforePath, toSide);
			return;
		}
		const fromTabs = this.tabsFor(fromSide);
		if (!fromTabs.includes(fromPath)) {
			return;
		}
		const toTabs = this.tabsFor(toSide);
		const alreadyInTarget = toTabs.includes(fromPath);

		const fromIdx = fromTabs.indexOf(fromPath);
		const newFromTabs = fromTabs.filter((p) => p !== fromPath);

		let newToTabs: string[];
		if (alreadyInTarget) {
			// Drop landed on the dragged tab itself or past the end:
			// don't shuffle the destination, just remove from source.
			if (beforePath === null || beforePath === fromPath) {
				newToTabs = toTabs;
			} else {
				const without = toTabs.filter((p) => p !== fromPath);
				const beforeIdx = without.indexOf(beforePath);
				newToTabs =
					beforeIdx < 0
						? [...without, fromPath]
						: [...without.slice(0, beforeIdx), fromPath, ...without.slice(beforeIdx)];
			}
		} else if (beforePath === null) {
			newToTabs = [...toTabs, fromPath];
		} else {
			const beforeIdx = toTabs.indexOf(beforePath);
			newToTabs =
				beforeIdx < 0 ? [...toTabs, fromPath] : [...toTabs.slice(0, beforeIdx), fromPath, ...toTabs.slice(beforeIdx)];
		}

		this.setTabsFor(fromSide, newFromTabs);
		this.setTabsFor(toSide, newToTabs);

		// Source loses the active tab — pick the same fallback `closeFile`
		// would (sibling immediately to the left, or null if pane went empty).
		const wasActiveOnSource = (fromSide === 'left' ? this.leftActive : this.rightActive) === fromPath;
		if (wasActiveOnSource) {
			const fallback = newFromTabs[Math.max(0, fromIdx - 1)] ?? null;
			if (fromSide === 'left') {
				this.leftActive = fallback;
			} else {
				this.rightActive = fallback;
			}
		}

		// Destination becomes active + focused on the dropped tab.
		if (toSide === 'left') {
			this.leftActive = fromPath;
		} else {
			this.rightActive = fromPath;
		}
		this.focusedSide = toSide;
		this.requestEditorFocus();
		this.persistAppState();
	}

	setActive(path: string, side: SplitSide = this.focusedSide, options: { focus?: boolean } = {}) {
		if (!this.tabsFor(side).includes(path)) {
			return;
		}
		if (side === 'left') {
			this.leftActive = path;
		} else {
			this.rightActive = path;
		}
		this.focusedSide = side;
		// `focus` defaults to true; the tree opts out via `{ focus: false }`
		// so arrow-key navigation can preview-browse without stealing focus.
		if (options.focus !== false) {
			this.requestEditorFocus();
		}
		this.persistAppState();
	}

	/**
	 * True when the editor's last-known `currentPath` was just rebound
	 * to `newPath` by a save-as. Read by `Editor.svelte`'s reactive
	 * effect to decide whether a `file.path` change is a rename (keep
	 * view state) or a tab switch (rebuild). The window is only open
	 * for as long as `lastRename` survives — which is until the next
	 * save-as, since we don't proactively clear it. That's good enough:
	 * any later path mismatch on the same editor is, by definition, a
	 * different change.
	 */
	isRename(fromPath: string | null, toPath: string): boolean {
		if (fromPath === null || !this.lastRename) {
			return false;
		}
		return this.lastRename.from === fromPath && this.lastRename.to === toPath;
	}

	requestEditorFocus() {
		this.focusTick += 1;
	}

	requestSidebarFocus() {
		this.sidebarFocusTick += 1;
	}

	requestStatusFocus() {
		this.statusFocusTick += 1;
	}

	focusSide(side: SplitSide) {
		if (this.focusedSide === side) {
			return;
		}
		this.focusedSide = side;
		this.persistAppState();
	}

	splitActive(direction: 'right') {
		if (this.hasSplit) {
			this.focusedSide = direction === 'right' ? 'right' : 'left';
			this.persistAppState();
			return;
		}
		// Open the split with the active tab mirrored on the right. Per-
		// pane lists are independent from this point on: the user can
		// open / close / reorder tabs on each strip without affecting the
		// other. Mirroring just one tab (rather than copying the full
		// left list) keeps the split visually clean — the second pane is
		// for "look at this file alongside that one", not "duplicate my
		// whole tab strip".
		this.hasSplit = true;
		const seed = this.leftActive;
		if (seed !== null && !this.rightTabs.includes(seed)) {
			this.rightTabs = [...this.rightTabs, seed];
		}
		this.rightActive = seed;
		this.focusedSide = 'right';
		this.persistAppState();
	}

	closeSplit() {
		if (!this.hasSplit) {
			return;
		}
		// Drop the right pane's tabs entirely. Buffers that are still in
		// the left pane stay loaded; ones that lived only on the right
		// fall out of `openFiles` via the same GC pass `closeFile` uses.
		const dropped = this.rightTabs.filter((p) => !this.leftTabs.includes(p));
		this.rightTabs = [];
		this.hasSplit = false;
		this.rightActive = null;
		this.focusedSide = 'left';
		if (dropped.length > 0) {
			this.openFiles = this.openFiles.filter((f) => !dropped.includes(f.path));
			let modes: Map<string, MarkdownView> | null = null;
			for (const path of dropped) {
				if (this.previewModes.has(path)) {
					if (!modes) {
						modes = new Map(this.previewModes);
					}
					modes.delete(path);
				}
			}
			if (modes) {
				this.previewModes = modes;
			}
		}
		this.persistAppState();
	}

	updateText(path: string, text: string) {
		this.openFiles = this.openFiles.map((f) => {
			if (f.path !== path) {
				return f;
			}
			// Length mismatch is a fast path: different sizes can never compare
			// equal, so we skip hashing entirely while the user is typing.
			const dirty =
				text.length !== f.loadedFingerprint.length || !fingerprintEquals(fingerprint(text), f.loadedFingerprint);
			return { ...f, text, isDirty: dirty };
		});
	}

	closeActive() {
		const path = this.activePath;
		if (path === null) {
			return;
		}
		void this.closeFile(path);
	}

	async saveActive() {
		const file = this.activeFile;
		if (!file) {
			return;
		}
		// Untitled buffers route through the save-as flow on every save
		// until they get a real path. `Ctrl+S` and the "Save File"
		// command both hit this path — there is no separate "Save"
		// surface for untitled.
		if (file.isUntitled) {
			await this.saveActiveAs();
			return;
		}
		if (!file.isDirty) {
			return;
		}
		try {
			const result = await ipc.fs.writeFile(file.path, file.text);
			// The pre-save pipeline (line endings, trim trailing ws, final
			// newline) runs server-side, so the bytes on disk may not equal
			// `file.text`. Re-read so the in-memory buffer matches disk and
			// the dirty fingerprint reflects the canonical form. Otherwise
			// "save then save again" would still mark dirty if the pipeline
			// changed anything.
			const fresh = await ipc.fs.readFile(file.path);
			this.openFiles = this.openFiles.map((f) =>
				f.path === file.path
					? {
							...f,
							text: fresh.is_binary ? f.text : fresh.text,
							isDirty: false,
							loadedFingerprint: fingerprint(fresh.is_binary ? f.text : fresh.text),
							loadedMtimeMs: result.mtime_ms,
						}
					: f,
			);
			// Saving a `.editorconfig` invalidated the host-side cache;
			// refresh frontend copies so the active editor immediately
			// honours the new indent/tab rules.
			if (file.name === '.editorconfig') {
				await this.refreshEditorConfigs();
			}
		} catch (err) {
			this.flash(`Save failed: ${formatError(err)}`);
		}
	}

	/**
	 * Save the active buffer to a path the user picks via the native
	 * dialog. Used both to commit untitled buffers and to do a true
	 * "Save As" for an existing file (rebinding the buffer to the new
	 * path; the original on-disk file is left untouched).
	 *
	 * The path swap that follows the write is the same dance Ctrl+N
	 * relies on: every place that keys on `OpenFile.path` (tab arrays,
	 * active fields, preview-mode map) is rewritten in lockstep so the
	 * buffer remains the single source of truth. The editor view detects
	 * the rename via content equality and swaps language extensions
	 * without rebuilding state, preserving selection / scroll / undo.
	 */
	async saveActiveAs() {
		const file = this.activeFile;
		if (!file) {
			return;
		}
		const ws = this.workspace;
		if (!ws) {
			this.flash('Open a folder before saving.');
			return;
		}
		if (file.kind === 'image') {
			// Image buffers are read-only previews; "Save As" would need
			// us to copy bytes through the host, which nobody has asked
			// for yet. Refuse loudly rather than half-implement.
			this.flash('Save As is not supported for image files.');
			return;
		}

		const defaultPath = file.isUntitled
			? `${ws.root}/${file.name}.txt`
			: await ipc.fs.absolutePath(file.path).catch(() => `${ws.root}/${file.path}`);
		const target = await saveDialog({
			title: file.isUntitled ? 'Save Untitled File' : 'Save As',
			defaultPath,
		});
		if (!target) {
			return;
		}

		// Workspace-bound: the host-side fs commands all take workspace-
		// relative paths, and our session model assumes every open file
		// lives inside the current root. Refuse out-of-tree saves with a
		// toast — supporting them would mean a multi-root model, which
		// is Phase 7's problem, not ours.
		const root = ws.root.replace(/\/+$/, '');
		const newPath = relativeToRoot(target, root);
		if (newPath === null) {
			this.flash('Save target must be inside the current workspace.');
			return;
		}

		// Refuse to merge with another open buffer at the same path. The
		// alternative is to close the existing tab silently and replace
		// its buffer with ours, which is surprising; better to ask the
		// user to close the conflicting tab first.
		const oldPath = file.path;
		if (newPath !== oldPath && this.openFiles.some((f) => f.path === newPath)) {
			this.flash(`A buffer for ${newPath} is already open. Close it before saving here.`);
			return;
		}

		try {
			const result = await ipc.fs.writeFile(newPath, file.text);
			const fresh = await ipc.fs.readFile(newPath);
			const newName = basename(newPath);
			const newKind = fileKindFor(newPath);
			const freshText = fresh.is_binary ? file.text : fresh.text;
			this.openFiles = this.openFiles.map((f) =>
				f.path === oldPath
					? {
							...f,
							path: newPath,
							name: newName,
							kind: newKind,
							isUntitled: false,
							text: freshText,
							isDirty: false,
							loadedFingerprint: fingerprint(freshText),
							loadedMtimeMs: result.mtime_ms,
						}
					: f,
			);
			if (newPath !== oldPath) {
				this.leftTabs = this.leftTabs.map((p) => (p === oldPath ? newPath : p));
				this.rightTabs = this.rightTabs.map((p) => (p === oldPath ? newPath : p));
				if (this.leftActive === oldPath) {
					this.leftActive = newPath;
				}
				if (this.rightActive === oldPath) {
					this.rightActive = newPath;
				}
				if (this.previewModes.has(oldPath)) {
					const next = new Map(this.previewModes);
					const mode = next.get(oldPath);
					next.delete(oldPath);
					if (mode) {
						next.set(newPath, mode);
					}
					this.previewModes = next;
				}
				// Signal the rename to live editor views so they can
				// preserve selection / scroll / undo across the path
				// swap. See the comment on `renameTick`.
				this.lastRename = { from: oldPath, to: newPath };
				this.renameTick += 1;
			}

			await this.ensureEditorConfig(newPath);
			if (newName === '.editorconfig') {
				await this.refreshEditorConfigs();
			}
			// Refresh the file tree so the new file appears (or moves) in
			// the listing without requiring a manual refresh.
			await this.loadPaths();
			this.persistAppState();
		} catch (err) {
			this.flash(`Save failed: ${formatError(err)}`);
		}
	}

	toggleTheme() {
		const next: ThemeMode = this.theme === 'dark' ? 'light' : 'dark';
		this.theme = next;
		applyTheme(next);
		this.persistAppState();
	}

	flash(msg: string) {
		this.toast = msg;
		setTimeout(() => {
			if (this.toast === msg) {
				this.toast = null;
			}
		}, 4000);
	}
}

function applyTheme(mode: ThemeMode) {
	const root = document.documentElement;
	if (mode === 'light') {
		root.classList.add('light');
	} else {
		root.classList.remove('light');
	}
	// The editor scrollbar corner (the moon easter egg) does NOT follow
	// theme toggles on WebKitGTK — see the note in `lib/editor/theme.ts`
	// for the full list of invalidation paths we tried. We accept the
	// stale-corner-after-toggle behaviour rather than ship more hacks.
}

async function collectPaths(rel: string, out: string[], depth: number): Promise<void> {
	// We cap depth so very deep trees can't lock the UI on first load. Dirs beyond the
	// cap are still listed at their level but not recursed into. Phase 1 will add lazy
	// expansion to remove this cap.
	const MAX_DEPTH = 6;
	const path = rel === '' ? '.' : rel;
	const entries = await ipc.fs.readDir(path);
	for (const entry of entries) {
		if (entry.kind === 'dir') {
			const dirPath = rel === '' ? entry.name : `${rel}/${entry.name}`;
			out.push(dirPath + '/');
			if (depth < MAX_DEPTH) {
				await collectPaths(dirPath, out, depth + 1);
			}
		} else if (entry.kind === 'file' || entry.kind === 'symlink') {
			const filePath = rel === '' ? entry.name : `${rel}/${entry.name}`;
			out.push(filePath);
		}
	}
}

function basename(path: string): string {
	const i = path.lastIndexOf('/');
	return i >= 0 ? path.slice(i + 1) : path;
}

/**
 * Convert an absolute path returned by the native save dialog into a
 * workspace-relative path, or `null` if the target is outside `root`.
 * Trailing slashes on `root` are normalised by the caller. We deliberately
 * don't resolve symlinks here — the host will canonicalise on write
 * if it needs to.
 */
function relativeToRoot(absolute: string, root: string): string | null {
	if (absolute === root) {
		// Saving to the workspace root itself isn't meaningful (it's a
		// directory); the dialog shouldn't return this, but handle it
		// defensively.
		return null;
	}
	const prefix = root + '/';
	if (!absolute.startsWith(prefix)) {
		return null;
	}
	return absolute.slice(prefix.length);
}

export function isUntitledPath(path: string): boolean {
	return path.startsWith('untitled:');
}

/**
 * Drop any path whose ancestor (a directory path with trailing slash)
 * is also in the input. Used when the user multi-selects e.g. `src/`
 * and `src/foo.ts` — we only need to issue one IPC call for `src/`,
 * the directory delete subsumes the file. The shorter ancestor path
 * sorts first lexicographically, which is why a single ascending
 * sort + linear scan is enough.
 */
function dropDescendantPaths(paths: string[]): string[] {
	const sorted = paths.toSorted();
	const kept: string[] = [];
	for (const p of sorted) {
		const ancestor = kept.find((k) => k.endsWith('/') && p.startsWith(k));
		if (!ancestor) {
			kept.push(p);
		}
	}
	return kept;
}

/**
 * Build the body text for the trash / delete confirm dialog. Single
 * selections get the precise filename or "the folder X" wording so
 * the user can sanity-check the row. Multi-selections fall back to a
 * count — listing every path doesn't fit a native dialog cleanly and
 * the visible tree highlight already shows what's selected.
 */
function buildRemovalMessage(displays: string[], realPaths: string[], mode: 'trash' | 'delete'): string {
	if (displays.length === 0) {
		return '';
	}
	if (displays.length === 1) {
		const display = displays[0] ?? '';
		const isDir = realPaths[0]?.endsWith('/') ?? false;
		if (mode === 'trash') {
			return isDir
				? `Move the folder ${display} (and everything inside it) to the trash?`
				: `Move ${display} to the trash?`;
		}
		return isDir
			? `Permanently delete the folder ${display} and everything inside it? This cannot be undone (recover via git if it was tracked).`
			: `Permanently delete ${display}? This cannot be undone (recover via git if it was tracked).`;
	}
	const noun = `${displays.length} items`;
	return mode === 'trash'
		? `Move ${noun} to the trash?`
		: `Permanently delete ${noun}? This cannot be undone (recover via git if any of them were tracked).`;
}

export const workspace = new WorkspaceState();
