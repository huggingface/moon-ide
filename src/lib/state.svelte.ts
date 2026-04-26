import { convertFileSrc } from '@tauri-apps/api/core';
import { confirm } from '@tauri-apps/plugin-dialog';
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
import { fingerprint, fingerprintEquals, type ContentFingerprint } from './util/hash';
import { fileKindFor, type FileKind } from './util/fileKind';
import { isMarkdownPath } from './util/markdown';

export type MarkdownView = 'source' | 'preview';

export type { SplitSide } from './protocol';

export type OpenFile = {
	path: string;
	name: string;
	kind: FileKind;
	text: string;
	// Tauri asset:// URL for image files; empty string for text. Text files
	// stream their contents through `text`, image files render via this URL.
	previewUrl: string;
	// Fingerprint of the bytes last known to be on disk. Comparing the
	// current text's fingerprint against this lets us derive `isDirty`
	// without keeping a second full copy of the file in memory.
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

	// Persistence guards. `persistScheduled` coalesces bursts of mutations
	// (e.g. closeFile mutates openFiles + leftActive in the same tick) into
	// a single IPC roundtrip. `suppressPersist` is set during startup
	// restore so we don't round-trip the freshly-loaded state right back
	// to disk on every `openFile` call.
	private persistScheduled = false;
	private suppressPersist = false;

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
			const session: WorkspaceSession | null = ws
				? {
						workspace_path: ws.root,
						open_files_left: [...this.leftTabs],
						open_files_right: this.hasSplit ? [...this.rightTabs] : [],
						active_left: this.leftActive,
						active_right: this.hasSplit ? this.rightActive : null,
						has_split: this.hasSplit,
						focused_side: this.focusedSide,
					}
				: null;
			const payload: AppState = {
				last_session: session,
				theme: this.theme,
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

	async openFile(path: string, side: SplitSide = this.focusedSide) {
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
		this.setActive(path, side);
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
			text: '',
			previewUrl: convertFileSrc(absolute),
			loadedFingerprint: fingerprint(''),
			loadedMtimeMs: null,
			isDirty: false,
		};
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
	 * scoped to the pane — the other pane's tab order is unaffected
	 * (VSCode/Zed convention). Drag-between-panes is intentionally not
	 * supported yet; surface it when someone actually asks for it.
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

	setActive(path: string, side: SplitSide = this.focusedSide) {
		if (!this.tabsFor(side).includes(path)) {
			return;
		}
		if (side === 'left') {
			this.leftActive = path;
		} else {
			this.rightActive = path;
		}
		this.focusedSide = side;
		this.requestEditorFocus();
		this.persistAppState();
	}

	requestEditorFocus() {
		this.focusTick += 1;
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
		if (!file || !file.isDirty) {
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

export const workspace = new WorkspaceState();
