import { convertFileSrc } from '@tauri-apps/api/core';
import { confirm } from '@tauri-apps/plugin-dialog';
import { ipc } from './ipc';
import {
	formatError,
	type AppState,
	type SplitSide,
	type ThemeMode,
	type Workspace,
	type WorkspaceSession,
} from './protocol';
import { fingerprint, fingerprintEquals, type ContentFingerprint } from './util/hash';
import { fileKindFor, type FileKind } from './util/fileKind';

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
	openFiles = $state<OpenFile[]>([]);

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
			const loaded: OpenFile[] = [];
			for (const path of session.open_files) {
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
			this.openFiles = loaded;

			const isOpen = (p: string | null) => p !== null && loaded.some((f) => f.path === p);
			this.leftActive = isOpen(session.active_left) ? session.active_left : (loaded[0]?.path ?? null);
			this.rightActive = session.has_split && isOpen(session.active_right) ? session.active_right : null;
			this.hasSplit = this.rightActive !== null;
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
						open_files: this.openFiles.map((f) => f.path),
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
		this.setActive(path, side);
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

	async closeFile(path: string) {
		const file = this.openFiles.find((f) => f.path === path);
		if (!file) {
			return;
		}
		if (file.isDirty) {
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
		const idx = this.openFiles.findIndex((f) => f.path === path);
		if (idx < 0) {
			return;
		}
		this.openFiles = this.openFiles.filter((f) => f.path !== path);

		const fallback = this.openFiles[Math.max(0, idx - 1)]?.path ?? null;
		if (this.leftActive === path) {
			this.leftActive = fallback;
		}
		if (this.rightActive === path) {
			this.rightActive = fallback;
		}
		if (fallback !== null) {
			this.requestEditorFocus();
		}
		this.persistAppState();
	}

	/**
	 * Move an open tab so it sits immediately before `beforePath`, or to
	 * the end of the strip if `beforePath` is null. Tab order is shared
	 * across both panes for now (left and right show the same `openFiles`
	 * list), so reordering on either strip reorders the other. Active
	 * selections stay pointed at the same files.
	 *
	 * Phase 1.5 splits this into per-pane lists (see specs/roadmap.md);
	 * once that lands, this method takes a `side: SplitSide` and only
	 * touches the matching pane's list.
	 */
	moveFile(fromPath: string, beforePath: string | null) {
		const file = this.openFiles.find((f) => f.path === fromPath);
		if (!file) {
			return;
		}
		if (beforePath === fromPath) {
			return;
		}
		const without = this.openFiles.filter((f) => f.path !== fromPath);
		if (beforePath === null) {
			this.openFiles = [...without, file];
			this.persistAppState();
			return;
		}
		const beforeIdx = without.findIndex((f) => f.path === beforePath);
		if (beforeIdx < 0) {
			this.openFiles = [...without, file];
			this.persistAppState();
			return;
		}
		this.openFiles = [...without.slice(0, beforeIdx), file, ...without.slice(beforeIdx)];
		this.persistAppState();
	}

	setActive(path: string, side: SplitSide = this.focusedSide) {
		if (!this.openFiles.some((f) => f.path === path)) {
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
		this.hasSplit = true;
		this.rightActive = this.leftActive;
		this.focusedSide = 'right';
		this.persistAppState();
	}

	closeSplit() {
		if (!this.hasSplit) {
			return;
		}
		this.hasSplit = false;
		this.rightActive = null;
		this.focusedSide = 'left';
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
			this.openFiles = this.openFiles.map((f) =>
				f.path === file.path
					? { ...f, isDirty: false, loadedFingerprint: fingerprint(f.text), loadedMtimeMs: result.mtime_ms }
					: f,
			);
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
